//! Evaluator-owned streaming JSON hashing. Only one tree entry is canonicalized
//! at a time; the outer array is never passed to the JCS serializer.
use kio_core::{
    cas::{canonical_json_bytes, lower_hex},
    disk_records::DiskRecords,
};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::io::{self, BufReader, Read};

pub(crate) struct Decoded<T> {
    pub decoded: kio_core::Result<T>,
    pub consumed: u64,
    pub hash: String,
    pub token_limit_exceeded: bool,
}

const MAX_TOKEN_BYTES: usize = 64 * 1024;
struct Hashed<R> {
    source: R,
    hash: Sha256,
    consumed: u64,
    quoted: bool,
    escaped: bool,
    token_bytes: usize,
    rejected: bool,
}
impl<R: Read> Read for Hashed<R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if self.rejected {
            return Err(io::Error::other("tree JSON token exceeds buffer bound"));
        }
        let n = self.source.read(bytes)?;
        self.hash.update(&bytes[..n]);
        self.consumed += n as u64;
        for &byte in &bytes[..n] {
            if self.quoted {
                if self.escaped {
                    self.escaped = false;
                } else if byte == b'\\' {
                    self.escaped = true;
                } else if byte == b'"' {
                    self.quoted = false;
                    self.token_bytes = 0;
                    continue;
                }
                self.token_bytes += 1;
            } else if byte == b'"' {
                self.quoted = true;
                self.token_bytes = 0;
            } else if byte.is_ascii_whitespace()
                || matches!(byte, b'{' | b'}' | b'[' | b']' | b':' | b',')
            {
                self.token_bytes = 0;
            } else {
                self.token_bytes += 1;
            }
            if self.token_bytes > MAX_TOKEN_BYTES {
                self.rejected = true;
                return Err(io::Error::other("tree JSON token exceeds buffer bound"));
            }
        }
        Ok(n)
    }
}

/// `max` is the caller's existing budget. Even invalid JSON is drained through
/// the same reader so bytes used by failed verification are accounted for.
pub(crate) fn decode<T: DeserializeOwned>(source: impl Read, max: u64) -> Decoded<T> {
    let mut reader = Hashed {
        source: source.take(max),
        hash: Sha256::new(),
        consumed: 0,
        quoted: false,
        escaped: false,
        token_bytes: 0,
        rejected: false,
    };
    let mut decoded =
        kio_core::disk_records::decode_json(BufReader::with_capacity(64 * 1024, &mut reader));
    if !reader.rejected
        && let Err(error) = io::copy(&mut reader, &mut io::sink())
    {
        decoded = Err(kio_core::KioError::io(error.to_string(), "evaluator tree"));
    }
    Decoded {
        decoded,
        consumed: reader.consumed,
        hash: format!("sha256:{}", lower_hex(&reader.hash.finalize())),
        token_limit_exceeded: reader.rejected,
    }
}

pub(crate) fn canonical_hash<T: Serialize + DeserializeOwned>(
    chunking: &str,
    entries: &DiskRecords<T>,
    object_type: &str,
) -> Result<String, String> {
    let mut hash = Sha256::new();
    hash.update(b"{\"chunking_config_hash\":");
    hash.update(
        canonical_json_bytes(&serde_json::Value::String(chunking.into()))
            .map_err(|e| e.to_string())?,
    );
    hash.update(b",\"entries\":[");
    for (i, entry) in entries.iter().enumerate() {
        if i != 0 {
            hash.update(b",");
        }
        let entry = entry.map_err(|e| e.to_string())?;
        let value = serde_json::to_value(entry).map_err(|e| e.to_string())?;
        hash.update(canonical_json_bytes(&value).map_err(|e| e.to_string())?);
    }
    hash.update(b"],\"object_type\":");
    hash.update(
        canonical_json_bytes(&serde_json::Value::String(object_type.into()))
            .map_err(|e| e.to_string())?,
    );
    hash.update(b"}");
    Ok(format!("sha256:{}", lower_hex(&hash.finalize())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::{cell::Cell, rc::Rc};

    #[derive(Deserialize)]
    struct Tree {
        entries: DiskRecords<serde_json::Value>,
    }
    struct Measured<R> {
        source: R,
        largest: Rc<Cell<usize>>,
    }
    impl<R: Read> Read for Measured<R> {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            self.largest.set(self.largest.get().max(bytes.len()));
            self.source.read(bytes)
        }
    }

    #[test]
    fn streamed_canonical_hash_matches_jcs_for_escaped_and_unicode_entries() {
        let entries = serde_json::json!([
            {"path": "é\"\\.txt", "raw_hash": "sha256:a", "type": "file"},
            {"path": "日本語.txt", "raw_hash": "sha256:b", "type": "file", "normalize": {"gen": 3, "manifest_hash": "sha256:c", "tool_profile_hash": "sha256:d"}}
        ]);
        let records: DiskRecords<serde_json::Value> =
            serde_json::from_value(entries.clone()).unwrap();
        let envelope = serde_json::json!({"chunking_config_hash": "sha256:config", "entries": entries, "object_type": "tree"});
        assert_eq!(
            canonical_hash("sha256:config", &records, "tree").unwrap(),
            kio_core::cas::hash_bytes(&canonical_json_bytes(&envelope).unwrap())
        );
    }

    #[test]
    fn tree_json_uses_bounded_reads_and_disk_backed_entries() {
        use std::io::{Seek, SeekFrom, Write};
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"{\"entries\":[").unwrap();
        for i in 0..60_000 {
            if i != 0 {
                file.write_all(b",").unwrap();
            }
            write!(file, "{{\"path\":\"{i:06}{}\"}}", "x".repeat(290)).unwrap();
        }
        file.write_all(b"]}").unwrap();
        let size = file.stream_position().unwrap();
        assert!(size > 16 * 1024 * 1024);
        file.seek(SeekFrom::Start(0)).unwrap();
        let largest = Rc::new(Cell::new(0));
        let outcome = decode::<Tree>(
            Measured {
                source: file,
                largest: largest.clone(),
            },
            size,
        );
        let tree = outcome.decoded.unwrap();
        assert_eq!(outcome.consumed, size);
        assert_eq!(tree.entries.len(), 60_000);
        assert_eq!(tree.entries.iter().count(), 60_000);
        assert!(largest.get() <= 64 * 1024);
        assert!(std::mem::size_of_val(&tree.entries) < 128);
    }

    #[test]
    fn tree_json_never_reads_past_the_callers_remaining_budget() {
        let input = vec![b' '; 1024];
        let outcome = decode::<Tree>(input.as_slice(), 512);
        assert!(outcome.decoded.is_err());
        assert_eq!(outcome.consumed, 512);
    }

    #[test]
    fn tree_json_does_not_allocate_an_unbounded_string_token() {
        // The raw input is lazy; rejection must happen near the token buffer
        // bound, before parsing or staging the whole field.
        let input = b"{\"entries\":[{\"path\":\""
            .iter()
            .copied()
            .chain(std::iter::repeat_n(b'x', 2 * 1024 * 1024))
            .chain(b"\"}]}".iter().copied());
        struct Bytes<I>(I);
        impl<I: Iterator<Item = u8>> Read for Bytes<I> {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                let mut n = 0;
                for byte in out {
                    if let Some(value) = self.0.next() {
                        *byte = value;
                        n += 1;
                    } else {
                        break;
                    }
                }
                Ok(n)
            }
        }
        let outcome = decode::<Tree>(Bytes(input), 4 * 1024 * 1024);
        assert!(outcome.decoded.is_err());
        assert!(outcome.consumed <= 128 * 1024);
    }
}
