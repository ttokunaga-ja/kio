//! Streaming typed tree I/O. Parsed metadata is retained for existing tree
//! consumers; serialized JSON bytes are never accumulated alongside it.
use super::*;
use crate::dag::TreeObject;

#[cfg(test)]
thread_local! { pub(super) static FAIL_TREE_WRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
#[cfg(test)]
pub(crate) fn fail_next_tree_write() {
    FAIL_TREE_WRITE.with(|fault| fault.set(true));
}

#[derive(Clone, Copy)]
enum StreamKind {
    Tree,
    Content(ContentObjectKind),
}

impl StreamKind {
    fn size_error(self, limit: u64, observed: u64) -> KioError {
        match self {
            Self::Tree => object_size_error(ObjectKind::Tree, limit, observed),
            Self::Content(kind) => content_object_size_error(kind, limit, observed),
        }
    }
    fn label(self) -> &'static Path {
        Path::new(match self {
            Self::Tree => "tree",
            Self::Content(kind) => kind.object_type(),
        })
    }
}

struct VerifiedRead<'a> {
    accounted: &'a mut u64,
    max_bytes: u64,
    kind: StreamKind,
    file: File,
    reads: &'a CasReadAccess,
    hasher: Sha256,
    consumed: u64,
    error: Option<KioError>,
}
impl Read for VerifiedRead<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self
            .max_bytes
            .saturating_sub(self.consumed)
            .saturating_add(1);
        let cap = bytes
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        let count = match self.file.read(&mut bytes[..cap]) {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => return Err(error),
            Err(error) => {
                let kind = error.kind();
                self.error = Some(stream_error(error, self.kind.label()));
                return Err(std::io::Error::new(kind, "cannot read CAS object"));
            }
        };
        if count != 0 {
            self.consumed = self.consumed.saturating_add(count as u64);
            *self.accounted = self.consumed;
            let result = self.reads.charge(count as u64).and_then(|()| {
                if self.consumed > self.max_bytes {
                    Err(self.kind.size_error(self.max_bytes, self.consumed))
                } else {
                    Ok(())
                }
            });
            if let Err(error) = result {
                self.error = Some(error);
                return Err(std::io::Error::other("CAS read budget exceeded"));
            }
            self.hasher.update(&bytes[..count]);
        }
        Ok(count)
    }
}

impl ObjectStore {
    /// Decode from a bounded I/O buffer while verifying the same retained file.
    /// Exact physical reads are charged, including reads that fail verification.
    pub fn read_tree(&self, hash: &str) -> Result<(TreeObject, u64)> {
        self.read_tree_accounted(hash).map_err(|e| e.error)
    }
    pub fn read_tree_accounted(
        &self,
        hash: &str,
    ) -> std::result::Result<(TreeObject, u64), AccountedReadError> {
        self.read_tree_accounted_with_limit(hash, u64::MAX)
    }
    pub fn read_tree_accounted_with_limit(
        &self,
        hash: &str,
        max_bytes: u64,
    ) -> std::result::Result<(TreeObject, u64), AccountedReadError> {
        let mut consumed_bytes = 0;
        self.read_tree_stream(hash, max_bytes, &mut consumed_bytes)
            .map_err(|error| AccountedReadError {
                error,
                consumed_bytes,
            })
    }
    fn read_tree_stream(
        &self,
        hash: &str,
        max_bytes: u64,
        accounted: &mut u64,
    ) -> Result<(TreeObject, u64)> {
        self.reads.charge(0)?;
        if !is_hash(hash) {
            return Err(KioError::invalid_usage("invalid tree hash"));
        }
        let slot = self
            .bound
            .as_ref()
            .map(|bound| bound.fanout(ObjectKind::Tree, hash, false))
            .transpose()?;
        let file = if let Some((parent, leaf)) = &slot {
            bound_open_regular_handle(parent, leaf)?
        } else {
            if !self.validate_object_parent(ObjectKind::Tree, hash)? {
                return Err(KioError::not_found(hash));
            }
            let path = self
                .existing_object_path(ObjectKind::Tree, hash)?
                .ok_or_else(|| KioError::not_found(hash))?;
            open_private_cas_file(&path)?
        };
        let (tree, consumed) = self.decode_verified_stream(
            StreamKind::Tree,
            slot.as_ref(),
            file,
            hash,
            max_bytes,
            accounted,
            |input| {
                crate::disk_records::decode_bounded_json::<TreeObject>(
                    &self.record_stages()?,
                    input,
                )
            },
        )?;
        tree.validate()?;
        if crate::dag::tree_hash(&tree)? != hash {
            return Err(bound_stage_corrupt("tree JSON is not canonical"));
        }
        Ok((tree, consumed))
    }

    /// Decode an immutable content object from the same bounded, accounted
    /// stream that verifies its digest and retained file identity.
    pub fn decode_content_accounted<T>(
        &self,
        kind: ContentObjectKind,
        hash: &str,
        max_bytes: u64,
        decode: impl FnOnce(&mut dyn std::io::BufRead) -> Result<T>,
    ) -> std::result::Result<(T, u64), AccountedReadError> {
        let mut consumed_bytes = 0;
        let result = (|| {
            self.reads.charge(0)?;
            if !is_hash(hash) {
                return Err(KioError::invalid_usage("invalid content object hash"));
            }
            let slot = self
                .bound
                .as_ref()
                .map(|bound| bound.content_fanout(kind, hash, false))
                .transpose()?;
            let file = if let Some((parent, leaf)) = &slot {
                bound_open_regular_handle(parent, leaf)?
            } else {
                if !self.validate_content_parent(kind, hash)? {
                    return Err(KioError::not_found(hash));
                }
                let path = self
                    .existing_content_path(kind, hash)?
                    .ok_or_else(|| KioError::not_found(hash))?;
                open_private_cas_file(&path)?
            };
            self.decode_verified_stream(
                StreamKind::Content(kind),
                slot.as_ref(),
                file,
                hash,
                max_bytes.min(kind.max_bytes()),
                &mut consumed_bytes,
                decode,
            )
        })();
        result.map_err(|error| AccountedReadError {
            error,
            consumed_bytes,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn decode_verified_stream<T>(
        &self,
        kind: StreamKind,
        slot: Option<&(File, String)>,
        file: File,
        hash: &str,
        max_bytes: u64,
        accounted: &mut u64,
        decode: impl FnOnce(&mut dyn std::io::BufRead) -> Result<T>,
    ) -> Result<(T, u64)> {
        let label = kind.label();
        let opened = cap_primitives::fs::Metadata::from_file(&file).kio_io(label)?;
        let before = observe_regular_file_stability(&file.metadata().kio_io(label)?, label)?;
        let size = file.metadata().kio_io(label)?.len();
        if size > max_bytes {
            return Err(kind.size_error(max_bytes, size));
        }
        let mut source = VerifiedRead {
            file,
            accounted,
            max_bytes,
            kind,
            reads: &self.reads,
            hasher: Sha256::new(),
            consumed: 0,
            error: None,
        };
        let decoded = decode(&mut std::io::BufReader::with_capacity(
            CAS_STREAM_BUFFER_BYTES,
            &mut source,
        ));
        if decoded
            .as_ref()
            .err()
            .is_some_and(|e| e.resource_limit().is_some())
        {
            return decoded.map(|tree| (tree, source.consumed));
        }
        // Verification precedes schema errors, just as for other CAS objects.
        // A parser may stop early on invalid JSON; drain through the same
        // accounted hash reader without retaining the remaining body.
        let drained = if source.error.is_none() {
            std::io::copy(&mut source, &mut std::io::sink())
        } else {
            Ok(0)
        };
        if let Some(error) = source.error.take() {
            return Err(error);
        }
        drained.kio_io(label)?;
        crate::private_fs::verify_private_regular_file_handle(&source.file)?;
        if source.consumed != size
            || observe_regular_file_stability(&source.file.metadata().kio_io(label)?, label)?
                != before
        {
            return Err(bound_stage_corrupt("CAS object changed while reading"));
        }
        if let Some((parent, leaf)) = slot {
            let named = cap_primitives::fs::stat(
                parent,
                Path::new(leaf),
                cap_primitives::fs::FollowSymlinks::No,
            )
            .map_err(|e| bound_io_or_not_found(e, leaf))?;
            if !named.is_file()
                || !bound_metadata_single_link(&named)
                || !same_bound_regular_entry(
                    parent,
                    Path::new(leaf),
                    &source.file,
                    &opened,
                    &named,
                )?
            {
                return Err(bound_stage_corrupt("CAS namespace changed while reading"));
            }
        }
        let actual = format!("sha256:{}", lower_hex(&source.hasher.finalize()));
        if actual != hash {
            return Err(corrupt_object_error(
                label,
                "CAS object hash mismatch",
                hash,
                Some(&actual),
            ));
        }
        Ok((decoded?, source.consumed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::{NormalizeRef, TreeEntry, TreeJsonReader, build_tree, tree_hash};

    #[test]
    fn st05_tree_larger_than_sixteen_mib_roundtrips() {
        let temp = tempfile::tempdir().unwrap();
        let repo = crate::scope::Repository::init(temp.path()).unwrap();
        let config = build_tree(
            &crate::disk_records::RecordStageRoot::temporary().unwrap(),
            Vec::new(),
        )
        .unwrap()
        .chunking_config_hash;
        let mut builder = crate::dag::TreeBuilder::new(
            &crate::disk_records::RecordStageRoot::temporary().unwrap(),
            config,
        );
        for i in 0..100_000 {
            builder
                .push(
                    TreeEntry::raw_file(format!("{i:06}-{}", "a".repeat(100)), hash_bytes(b"raw"))
                        .unwrap(),
                )
                .unwrap();
        }
        assert!(builder.peak_buffered_entries() <= crate::disk_records::SORT_BUFFER_RECORDS);
        let tree = builder.finish().unwrap();
        let hash = repo.object_store().write_tree(&tree).unwrap();
        assert!(
            repo.object_store()
                .inspect_object(ObjectKind::Tree, &hash)
                .unwrap()
                .size_bytes
                > 16 * 1024 * 1024
        );
        assert_eq!(repo.read_tree(&hash).unwrap().entries.len(), 100_000);
    }

    #[test]
    fn tree_read_verifies_corruption_and_charges_the_stream_before_schema_errors() {
        let temp = tempfile::tempdir().unwrap();
        let repo = crate::scope::Repository::init(temp.path()).unwrap();
        let tree = build_tree(
            &crate::disk_records::RecordStageRoot::temporary().unwrap(),
            vec![TreeEntry::raw_file("a", hash_bytes(b"raw")).unwrap()],
        )
        .unwrap();
        let hash = repo.object_store().write_tree(&tree).unwrap();
        let size = repo
            .object_store()
            .inspect_object(ObjectKind::Tree, &hash)
            .unwrap()
            .size_bytes;
        let budget = VerifiedCasReadBudget::new();
        let (_, consumed) = budget
            .run(size, |reads| {
                repo.object_store()
                    .with_verified_reads(reads)?
                    .read_tree(&hash)
                    .map(|_| ())
            })
            .unwrap();
        assert_eq!(consumed, size);
        let error = budget
            .run(size - 1, |reads| {
                repo.object_store()
                    .with_verified_reads(reads)?
                    .read_tree(&hash)
                    .map(|_| ())
            })
            .unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-STORE-VERIFIED-BYTES-LIMIT-001");
        let path = ObjectStore::new(repo.kio_dir())
            .object_path(ObjectKind::Tree, &hash)
            .unwrap();
        fs::write(path, b"not JSON").unwrap();
        assert_eq!(
            repo.read_tree(&hash).unwrap_err().error_code(),
            "KIO-E-STORE-CORRUPT-001"
        );
    }

    #[test]
    fn st05_streamed_tree_bytes_hash_reader_and_writer_agree() {
        let temp = tempfile::tempdir().unwrap();
        let repo = crate::scope::Repository::init(temp.path()).unwrap();
        let mut entries = Vec::new();
        for index in (0..10_001).rev() {
            let mut entry =
                TreeEntry::raw_file(format!("{index:05}-日本語-😀.txt"), hash_bytes(b"raw"))
                    .unwrap();
            if index % 2 == 0 {
                entry.normalize = Some(NormalizeRef {
                    tool_profile_hash: hash_bytes(b"profile"),
                    r#gen: 12,
                    manifest_hash: hash_bytes(b"manifest"),
                });
            }
            entries.push(entry);
        }
        let tree = build_tree(
            &crate::disk_records::RecordStageRoot::temporary().unwrap(),
            entries,
        )
        .unwrap();
        let expected = serde_jcs::to_vec(&tree).unwrap();
        let mut streamed = Vec::new();
        let mut source = TreeJsonReader::new(&tree).unwrap();
        // Small caller buffers exercise boundaries inside UTF-8 and escapes.
        let mut buffer = [0; 7];
        loop {
            let n = source.read(&mut buffer).unwrap();
            if n == 0 {
                break;
            }
            streamed.extend_from_slice(&buffer[..n]);
        }
        assert_eq!(streamed, expected);
        assert_eq!(tree_hash(&tree).unwrap(), hash_bytes(&expected));
        let hash = repo.object_store().write_tree(&tree).unwrap();
        assert_eq!(hash, hash_bytes(&expected));
        assert_eq!(repo.read_tree(&hash).unwrap(), tree);
        assert_eq!(
            repo.object_store().read_tree(&hash).unwrap().1,
            expected.len() as u64
        );
        assert_eq!(repo.object_store().write_tree(&tree).unwrap(), hash);
        assert!(repo.object_store().inspect_raw_stages().unwrap().is_empty());
        let path = ObjectStore::new(repo.kio_dir())
            .object_path(ObjectKind::Tree, &hash)
            .unwrap();
        let mut corrupted = expected;
        let at = corrupted.iter().position(|b| *b == b'2').unwrap();
        corrupted[at] = b'3';
        fs::write(path, corrupted).unwrap();
        assert_eq!(
            repo.read_tree(&hash).unwrap_err().error_code(),
            "KIO-E-STORE-CORRUPT-001"
        );
    }
}
