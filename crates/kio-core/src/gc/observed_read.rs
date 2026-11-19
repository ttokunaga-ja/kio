//! Descriptor-bound observed reads and their existing GC accounting.

use std::{collections::BTreeMap, io::Read, path::Path};

use cap_primitives::fs as cap_fs;

use crate::{
    cas::{CasReadAccess, hash_bytes},
    error::Result,
};

use super::{
    FileObservation, GcPlanLimits, GcPlanStats, account, corrupt, file_state, id_meta,
    insert_observation, ioerr, limit, same_file_state, valid_file,
};

pub(crate) fn read_regular_observed(
    d: &std::fs::File,
    n: &str,
    max: u64,
) -> Result<(Vec<u8>, FileObservation)> {
    read_cas_regular_observed(&CasReadAccess::Unmetered, d, n, max)
}

pub(crate) fn read_cas_regular_observed(
    reads: &CasReadAccess,
    d: &std::fs::File,
    n: &str,
    max: u64,
) -> Result<(Vec<u8>, FileObservation)> {
    read_observed(reads, d, n, max, true)
}

/// Working-tree ignore policy is user-owned; retain no-follow identity and
/// byte observations without imposing Kio storage permissions on that file.
pub(super) fn read_user_regular_observed(
    d: &std::fs::File,
    n: &str,
    max: u64,
) -> Result<(Vec<u8>, FileObservation)> {
    read_observed(&CasReadAccess::Unmetered, d, n, max, false)
}

fn read_observed(
    reads: &CasReadAccess,
    d: &std::fs::File,
    n: &str,
    max: u64,
    require_private: bool,
) -> Result<(Vec<u8>, FileObservation)> {
    reads.charge(0)?;
    let p = Path::new(n);
    let before = cap_fs::stat(d, p, cap_fs::FollowSymlinks::No).map_err(|e| ioerr(e, n))?;
    valid_file(&before, max)?;
    let mut o = cap_fs::OpenOptions::new();
    o.read(true);
    o._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let mut f = cap_fs::open(d, p, &o).map_err(|e| ioerr(e, n))?;
    if require_private {
        crate::private_fs::verify_private_regular_file_handle(&f)?;
    }
    let opened = cap_fs::Metadata::from_file(&f).map_err(|e| ioerr(e, n))?;
    valid_file(&opened, max)?;
    if !same_file_state(&before, &opened)? {
        return Err(corrupt("store file changed while opening"));
    }
    let mut b = Vec::with_capacity(usize::try_from(opened.len()).map_err(|_| limit("file bytes"))?);
    let mut bounded = (&mut f).take(max.saturating_add(1));
    let mut buffer = [0_u8; 8192];
    loop {
        reads.charge(0)?;
        let count = match bounded.read(&mut buffer) {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(ioerr(error, n)),
        };
        reads.charge(count as u64)?;
        if count == 0 {
            break;
        }
        b.extend_from_slice(&buffer[..count]);
    }
    if b.len() as u64 > max {
        return Err(limit("file bytes"));
    }
    let after = cap_fs::stat(d, p, cap_fs::FollowSymlinks::No).map_err(|e| ioerr(e, n))?;
    valid_file(&after, max)?;
    if b.len() as u64 != opened.len() || !same_file_state(&after, &opened)? {
        return Err(corrupt("store file changed while read"));
    }
    let observation = FileObservation {
        identity: id_meta(&opened)?,
        state: file_state(&opened),
        digest: hash_bytes(&b),
    };
    Ok((b, observation))
}

pub(super) fn read_accounted_observed(
    directory: &std::fs::File,
    name: &str,
    max: u64,
    stats: &mut GcPlanStats,
    limits: &GcPlanLimits,
    observations: &mut BTreeMap<String, FileObservation>,
    observation_name: &str,
) -> Result<Vec<u8>> {
    read_cas_accounted_observed(
        &CasReadAccess::Unmetered,
        (directory, name, max),
        stats,
        limits,
        observations,
        observation_name,
    )
}

pub(super) fn read_cas_accounted_observed(
    reads: &CasReadAccess,
    source: (&std::fs::File, &str, u64),
    stats: &mut GcPlanStats,
    limits: &GcPlanLimits,
    observations: &mut BTreeMap<String, FileObservation>,
    observation_name: &str,
) -> Result<Vec<u8>> {
    let (bytes, observation) = read_cas_regular_observed(reads, source.0, source.1, source.2)?;
    account(stats, bytes.len() as u64, limits)?;
    insert_observation(observations, observation_name, observation)?;
    Ok(bytes)
}

/// Hash an arbitrarily large object without retaining its serialized body.
pub(crate) fn observe_cas_regular(
    reads: &CasReadAccess,
    directory: &std::fs::File,
    name: &str,
    max: u64,
) -> Result<FileObservation> {
    stream_named(reads, directory, name, max, false).map(|(_, observation)| observation)
}

pub(crate) fn read_cas_tree_observed(
    reads: &CasReadAccess,
    directory: &std::fs::File,
    name: &str,
    max: u64,
) -> Result<(crate::dag::TreeObject, FileObservation)> {
    let (tree, observation) = stream_named(reads, directory, name, max, true)?;
    Ok((
        tree.ok_or_else(|| corrupt("invalid tree object"))?,
        observation,
    ))
}

/// The retirement sentinel shares an archive slot with a tree during recovery.
/// Its fixed digest distinguishes it without materializing either body.
pub(crate) fn read_tree_archive_observed(
    reads: &CasReadAccess,
    directory: &std::fs::File,
    name: &str,
    max: u64,
) -> Result<(Option<crate::dag::TreeObject>, FileObservation)> {
    stream_named(reads, directory, name, max, true)
}

fn stream_named(
    reads: &CasReadAccess,
    directory: &std::fs::File,
    name: &str,
    max: u64,
    decode: bool,
) -> Result<(Option<crate::dag::TreeObject>, FileObservation)> {
    reads.charge(0)?;
    let path = Path::new(name);
    let before =
        cap_fs::stat(directory, path, cap_fs::FollowSymlinks::No).map_err(|e| ioerr(e, name))?;
    valid_file(&before, max)?;
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let file = cap_fs::open(directory, path, &options).map_err(|e| ioerr(e, name))?;
    let opened = cap_fs::Metadata::from_file(&file).map_err(|e| ioerr(e, name))?;
    if !same_file_state(&before, &opened)? {
        return Err(corrupt("store file changed while opening"));
    }
    let result = stream_handle(reads, &file, max, decode)?;
    let after =
        cap_fs::stat(directory, path, cap_fs::FollowSymlinks::No).map_err(|e| ioerr(e, name))?;
    valid_file(&after, max)?;
    if !same_file_state(&after, &opened)? {
        return Err(corrupt("store file changed while read"));
    }
    Ok(result)
}

#[cfg(windows)]
pub(crate) fn observe_tree_handle(
    file: &std::fs::File,
    max: u64,
) -> Result<(crate::dag::TreeObject, FileObservation)> {
    let (tree, observation) = stream_handle(&CasReadAccess::Unmetered, file, max, true)?;
    Ok((
        tree.ok_or_else(|| corrupt("invalid tree object"))?,
        observation,
    ))
}

fn stream_handle(
    reads: &CasReadAccess,
    file: &std::fs::File,
    max: u64,
    decode: bool,
) -> Result<(Option<crate::dag::TreeObject>, FileObservation)> {
    use sha2::{Digest, Sha256};
    use std::io::{Seek, SeekFrom};
    struct Source<'a> {
        file: &'a std::fs::File,
        reads: &'a CasReadAccess,
        hash: Sha256,
        count: u64,
        max: u64,
        error: Option<crate::KioError>,
    }
    impl Read for Source<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if let Err(error) = self.reads.charge(0) {
                self.error = Some(error);
                return Err(std::io::Error::other("GC read cancelled"));
            }
            let count = self.file.read(buffer)?;
            self.count = self.count.saturating_add(count as u64);
            if let Err(error) = self.reads.charge(count as u64).and_then(|()| {
                if self.count > self.max {
                    Err(limit("file bytes"))
                } else {
                    Ok(())
                }
            }) {
                self.error = Some(error);
                return Err(std::io::Error::other("GC read budget exceeded"));
            }
            self.hash.update(&buffer[..count]);
            Ok(count)
        }
    }
    let before = cap_fs::Metadata::from_file(file).map_err(|e| ioerr(e, "GC object"))?;
    valid_file(&before, max)?;
    crate::private_fs::verify_private_regular_file_handle(file)?;
    let mut handle = file;
    handle
        .seek(SeekFrom::Start(0))
        .map_err(|e| ioerr(e, "GC object"))?;
    let mut source = Source {
        file,
        reads,
        hash: Sha256::new(),
        count: 0,
        max,
        error: None,
    };
    let decoded = if decode {
        Some(crate::disk_records::decode_bounded_json::<
            crate::dag::TreeObject,
        >(std::io::BufReader::with_capacity(
            64 * 1024,
            &mut source,
        )))
    } else {
        None
    };
    let drained = if source.error.is_none() {
        std::io::copy(&mut source, &mut std::io::sink())
    } else {
        Ok(0)
    };
    if let Some(error) = source.error.take() {
        return Err(error);
    }
    drained.map_err(|e| ioerr(e, "GC object"))?;
    let after = cap_fs::Metadata::from_file(file).map_err(|e| ioerr(e, "GC object"))?;
    valid_file(&after, max)?;
    if source.count != before.len() || !same_file_state(&before, &after)? {
        return Err(corrupt("store file changed while read"));
    }
    let observation = FileObservation {
        identity: id_meta(&after)?,
        state: file_state(&after),
        digest: format!("sha256:{}", crate::cas::lower_hex(&source.hash.finalize())),
    };
    let tree = if let Some(decoded) = decoded {
        if observation.digest == hash_bytes(b"kio gc retirement sentinel\n") {
            None
        } else {
            let tree = decoded.map_err(|error| {
                if error.error_code() == "KIO-E-CONFIG-SCHEMA-001" {
                    corrupt("invalid tree object")
                } else {
                    error
                }
            })?;
            tree.validate().map_err(|error| {
                if error.error_code() == "KIO-E-CONFIG-SCHEMA-001"
                    || error.error_code() == "KIO-E-STORE-DUP-001"
                    || error.error_code() == "KIO-E-STORE-PATH-001"
                {
                    corrupt("invalid tree object")
                } else {
                    error
                }
            })?;
            if crate::dag::tree_hash(&tree)? != observation.digest {
                return Err(corrupt("tree object is not canonical"));
            }
            Some(tree)
        }
    } else {
        None
    };
    Ok((tree, observation))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        dag::{TreeBuilder, TreeEntry, TreeJsonReader},
        store_dir::StoreDirectory,
    };

    #[test]
    fn large_tree_observation_streams_canonical_bytes_and_preserves_limits() {
        let temp = tempfile::tempdir().unwrap();
        let directory =
            StoreDirectory::open_or_create_directories(&temp.path().join("owned")).unwrap();
        let config = crate::dag::build_tree(Vec::new())
            .unwrap()
            .chunking_config_hash;
        let mut builder = TreeBuilder::new(config);
        let suffix = "a".repeat(30_000);
        let raw = format!("sha256:{}", "a".repeat(64));
        for index in 0..600 {
            builder
                .push(TreeEntry::raw_file(format!("{index:04}{suffix}"), raw.clone()).unwrap())
                .unwrap();
        }
        let tree = builder.finish().unwrap();
        let mut file = directory.create_private_file(Path::new("tree")).unwrap();
        let size = std::io::copy(&mut TreeJsonReader::new(&tree).unwrap(), &mut file).unwrap();
        assert!(size > 16 * 1024 * 1024);
        let expected = crate::dag::tree_hash(&tree).unwrap();
        let (decoded, observed) = read_cas_tree_observed(
            &CasReadAccess::Unmetered,
            &directory.root_handle(),
            "tree",
            size,
        )
        .unwrap();
        assert_eq!(decoded.entries.len(), 600);
        assert_eq!(observed.digest, expected);
        assert_eq!(observed.state.len, size);
        assert!(
            read_cas_tree_observed(
                &CasReadAccess::Unmetered,
                &directory.root_handle(),
                "tree",
                size - 1
            )
            .is_err()
        );
    }

    #[test]
    fn tree_archive_accepts_only_exact_sentinel_or_canonical_tree() {
        use std::io::Write;
        let temp = tempfile::tempdir().unwrap();
        let directory =
            StoreDirectory::open_or_create_directories(&temp.path().join("owned")).unwrap();
        let mut sentinel = directory
            .create_private_file(Path::new("sentinel"))
            .unwrap();
        sentinel.write_all(b"kio gc retirement sentinel\n").unwrap();
        assert!(
            read_tree_archive_observed(
                &CasReadAccess::Unmetered,
                &directory.root_handle(),
                "sentinel",
                64
            )
            .unwrap()
            .0
            .is_none()
        );
        assert!(
            read_cas_tree_observed(
                &CasReadAccess::Unmetered,
                &directory.root_handle(),
                "sentinel",
                64
            )
            .is_err()
        );
        let mut oversized = directory
            .create_private_file(Path::new("oversized-token"))
            .unwrap();
        write!(oversized, "{{\"object_type\":\"{}\"}}", "a".repeat(70_000)).unwrap();
        let error = read_cas_tree_observed(
            &CasReadAccess::Unmetered,
            &directory.root_handle(),
            "oversized-token",
            100_000,
        )
        .unwrap_err();
        assert_eq!(
            error.resource_limit().unwrap().resource,
            crate::error::Resource::Memory
        );
        let tree = crate::dag::build_tree(Vec::new()).unwrap();
        let mut file = directory
            .create_private_file(Path::new("noncanonical"))
            .unwrap();
        serde_json::to_writer_pretty(&mut file, &tree).unwrap();
        assert!(
            read_tree_archive_observed(
                &CasReadAccess::Unmetered,
                &directory.root_handle(),
                "noncanonical",
                4096
            )
            .is_err()
        );
    }
}
