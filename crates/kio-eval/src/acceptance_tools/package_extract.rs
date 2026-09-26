//! Closed six-leaf Actions package envelope, validated before materialization.
use std::{
    collections::BTreeSet,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use clap::Args as ClapArgs;
use kio_core::{
    private_fs,
    store_dir::{Publication, StoreDirectory},
};
use zip::{
    ZipArchive,
    read::{ArchiveOffset, Config},
};

const MAX_BYTES: u64 = 768 * 1024 * 1024;
const MAX_DIRECTORY: u64 = 16 * 1024;

#[derive(Debug, ClapArgs)]
pub struct Args {
    #[arg(long)]
    pub archive: PathBuf,
    /// New directory under a controlled parent. Existing destinations are refused.
    #[arg(long)]
    pub output: PathBuf,
}

fn parent(path: &Path) -> Result<(StoreDirectory, PathBuf), String> {
    let leaf = path.file_name().ok_or("path requires a file name")?;
    let parent = path.parent().ok_or("path requires a parent")?;
    let absolute = std::path::absolute(parent).map_err(|_| "cannot resolve parent")?;
    let held = private_fs::verify_private_creation_parent(&absolute)
        .map_err(|_| "package parent is not controlled")?;
    Ok((held, PathBuf::from(leaf)))
}

// Bound the central-directory allocation before the ZIP library parses it.
// These small Actions envelopes do not need ZIP64, split archives, or prefixes.
fn envelope(file: &mut File) -> Result<(), String> {
    let size = file.metadata().map_err(|_| "cannot inspect archive")?.len();
    if !(22..=MAX_BYTES).contains(&size) {
        return Err("package archive size exceeds contract".into());
    }
    let count = size.min(22 + u16::MAX as u64) as usize;
    file.seek(SeekFrom::End(-(count as i64)))
        .map_err(|_| "cannot seek archive")?;
    let mut tail = vec![0; count];
    file.read_exact(&mut tail)
        .map_err(|_| "cannot read archive envelope")?;
    let end = (0..=count - 22)
        .rev()
        .find(|&at| {
            tail[at..at + 4] == *b"PK\x05\x06"
                && at + 22 + u16::from_le_bytes([tail[at + 20], tail[at + 21]]) as usize == count
        })
        .ok_or("missing canonical ZIP end record")?;
    let u16_at = |at| u16::from_le_bytes([tail[end + at], tail[end + at + 1]]);
    let u32_at = |at| u32::from_le_bytes(tail[end + at..end + at + 4].try_into().unwrap()) as u64;
    let directory_size = u32_at(12);
    let directory_start = u32_at(16);
    let end_position = size - count as u64 + end as u64;
    if u16_at(4) != 0
        || u16_at(6) != 0
        || u16_at(8) != 6
        || u16_at(10) != 6
        || directory_size > MAX_DIRECTORY
        || directory_start.checked_add(directory_size) != Some(end_position)
    {
        return Err("ZIP directory is outside the closed package contract".into());
    }
    // Count each central record independently; do not trust the EOCD count.
    file.seek(SeekFrom::Start(directory_start))
        .map_err(|_| "cannot seek directory")?;
    let mut directory = vec![0; directory_size as usize];
    file.read_exact(&mut directory)
        .map_err(|_| "cannot read directory")?;
    let mut at = 0;
    for _ in 0..6 {
        if directory.len().saturating_sub(at) < 46 || directory[at..at + 4] != *b"PK\x01\x02" {
            return Err("ZIP central record count differs".into());
        }
        let word = |offset| {
            u16::from_le_bytes([directory[at + offset], directory[at + offset + 1]]) as usize
        };
        let next = at + 46 + word(28) + word(30) + word(32);
        if next > directory.len() || word(34) != 0 {
            return Err("ZIP central record is invalid".into());
        }
        at = next;
    }
    if at != directory.len() {
        return Err("ZIP central directory has extra records".into());
    }
    file.rewind().map_err(|_| "cannot rewind archive")?;
    Ok(())
}

fn inspect(mut file: File) -> Result<ZipArchive<File>, String> {
    envelope(&mut file)?;
    let mut archive = ZipArchive::with_config(
        Config {
            archive_offset: ArchiveOffset::Known(0),
        },
        file,
    )
    .map_err(|_| "invalid package ZIP")?;
    if archive.len() != 6 {
        return Err("package requires exactly six leaves".into());
    }
    let mut names = BTreeSet::new();
    let mut kinds = [0_u8; 6];
    let mut total = 0_u64;
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|_| "cannot inspect package leaf")?;
        let name = std::str::from_utf8(entry.name_raw()).map_err(|_| "non-UTF-8 package leaf")?;
        if !names.insert(name.to_owned())
            || entry.is_dir()
            || entry.encrypted()
            || entry.unix_mode() != Some(0o100644)
            || name.contains('\\')
            || name.starts_with('/')
            || name
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
            || name.bytes().any(|b| b < 32 || b == 127 || b == b':')
        {
            return Err("unsafe or duplicate package leaf".into());
        }
        let kind = if name == "target-binding.json" {
            0
        } else if name == "kio-v1-lock-sha256" {
            1
        } else if !name.contains('/') && name.ends_with(".tar.gz") {
            2
        } else if !name.contains('/') && name.ends_with(".checksums.json") {
            3
        } else if name.starts_with("reproducible/")
            && name.matches('/').count() == 1
            && name.ends_with(".tar.gz")
        {
            4
        } else if name.starts_with("reproducible/")
            && name.matches('/').count() == 1
            && name.ends_with(".checksums.json")
        {
            5
        } else {
            return Err("unexpected package leaf".into());
        };
        kinds[kind] += 1;
        total = total
            .checked_add(entry.size())
            .ok_or("package size overflow")?;
        if total > MAX_BYTES {
            return Err("package decoded bytes exceed bound".into());
        }
    }
    if kinds != [1; 6] {
        return Err("package leaf set differs".into());
    }
    Ok(archive)
}

pub fn run(args: Args) -> Result<(), String> {
    let (source_parent, source_leaf) = parent(&args.archive)?;
    let file = source_parent
        .open_regular_read(&source_leaf, MAX_BYTES)
        .map_err(|_| "unsafe package archive")?;
    let mut archive = inspect(file)?;
    let (destination_parent, destination_leaf) = parent(&args.output)?;
    let directory = destination_parent
        .create_directory(&destination_leaf)
        .map_err(|_| "package output must be a new private directory")?;
    let output =
        StoreDirectory::from_retained(directory, destination_parent.path().join(&destination_leaf))
            .map_err(|_| "cannot retain package output")?;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|_| "cannot read package leaf")?;
        // Reproducibility leaves are verified in the envelope but are not consumed here.
        if entry.name().contains('/') {
            continue;
        }
        let name = PathBuf::from(entry.name());
        let expected = entry.size();
        let mut bytes = Vec::new();
        entry
            .by_ref()
            .take(expected + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "package checksum or read failure")?;
        if bytes.len() as u64 != expected {
            return Err("package decoded length differs".into());
        }
        output
            .write_atomic(&name, &bytes, Publication::CreateOnly)
            .map_err(|_| "cannot publish package leaf")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};
    fn zip(names: &[&str]) -> File {
        let mut file = tempfile::tempfile().unwrap();
        {
            let mut writer = ZipWriter::new(&mut file);
            for name in names {
                writer
                    .start_file(
                        *name,
                        SimpleFileOptions::default()
                            .compression_method(CompressionMethod::Stored)
                            .unix_permissions(0o644),
                    )
                    .unwrap();
                writer.write_all(b"fixture").unwrap();
            }
            writer.finish().unwrap();
        }
        file.rewind().unwrap();
        file
    }
    const NAMES: &[&str] = &[
        "target-binding.json",
        "kio-v1-lock-sha256",
        "kio.tar.gz",
        "kio.checksums.json",
        "reproducible/kio.tar.gz",
        "reproducible/kio.checksums.json",
    ];
    #[test]
    fn accepts_only_exact_closed_leaf_set() {
        assert!(inspect(zip(NAMES)).is_ok());
        let mut names = NAMES.to_vec();
        names[2] = "../kio.tar.gz";
        assert!(inspect(zip(&names)).is_err());
        names[2] = "kio:stream.tar.gz";
        assert!(inspect(zip(&names)).is_err());
        names[2] = "unexpected.json";
        assert!(inspect(zip(&names)).is_err());
        assert!(inspect(zip(&NAMES[..5])).is_err());
    }
    #[test]
    fn rejects_forged_directory_count_before_library_indexing() {
        let mut file = zip(NAMES);
        file.seek(SeekFrom::End(-12)).unwrap();
        file.write_all(&7_u16.to_le_bytes()).unwrap();
        assert!(inspect(file).is_err());
    }
}
