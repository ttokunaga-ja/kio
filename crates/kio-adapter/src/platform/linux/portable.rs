//! Membership and content evidence for an extracted Office bundle.
use super::{
    dpkg::{open_package_file, open_regular},
    office::RuntimeTrust,
};
use crate::{
    office_convert::hex_digest,
    office_runtime_identity::{
        MAX_PACKAGE_ENTRIES, RecordSnapshot, RuntimeSnapshot, check_verification_deadline,
    },
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, btree_map::Entry},
    fs::{self, Metadata},
    io::{self, Read},
    path::{Path, PathBuf},
    time::Instant,
};

pub(super) const MANIFEST: &str = "SHA256SUMS";
const MAX_RELATIVE_PATH_BYTES: usize = 1024;
const MAX_RELATIVE_PATH_DEPTH: usize = 32;

#[derive(PartialEq, Eq)]
enum Member {
    Directory,
    File(String),
    Link(String),
}

pub(super) struct PortableCatalog {
    members: BTreeMap<PathBuf, Member>,
    pub(super) records: RecordSnapshot,
}

impl PortableCatalog {
    pub(super) fn read(bundle: &Path, trust: RuntimeTrust, deadline: Instant) -> io::Result<Self> {
        const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
        let path = bundle.join(MANIFEST);
        let mut records = RecordSnapshot::default();
        records.observe(bundle)?;
        records.observe(&path)?;
        let file = open_regular(&path)?;
        trust.verify_entry(&path, &file.metadata()?)?;
        let mut manifest = String::new();
        file.take(MAX_MANIFEST_BYTES + 1)
            .read_to_string(&mut manifest)?;
        if manifest.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(invalid(&path, "oversized portable Office runtime manifest"));
        }
        let mut members = BTreeMap::new();
        for line in manifest.lines() {
            check_verification_deadline(deadline)?;
            let (kind, relative) = line
                .split_once("  ")
                .ok_or_else(|| invalid(&path, "invalid portable Office runtime manifest row"))?;
            if relative.len() > MAX_RELATIVE_PATH_BYTES {
                return Err(invalid(
                    &path,
                    "portable Office runtime manifest path length exceeds limit",
                ));
            }
            if relative.split('/').count() > MAX_RELATIVE_PATH_DEPTH {
                return Err(invalid(
                    &path,
                    "portable Office runtime manifest path depth exceeds limit",
                ));
            }
            if members.len() >= MAX_PACKAGE_ENTRIES {
                return Err(invalid(
                    &path,
                    "too many portable Office runtime manifest entries",
                ));
            }
            let mut parts = relative.split('/');
            if !matches!(parts.next(), Some("program" | "share" | "presets"))
                || parts.any(|part| matches!(part, "" | "." | ".."))
                || relative.chars().any(char::is_control)
            {
                return Err(invalid(
                    &path,
                    "invalid portable Office runtime manifest path",
                ));
            }
            let member = if kind == "dir" {
                Member::Directory
            } else {
                let (link, hash) = kind
                    .strip_prefix("link:")
                    .map_or((false, kind), |hash| (true, hash));
                if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(invalid(&path, "invalid portable Office runtime SHA-256"));
                }
                if link {
                    Member::Link(hash.to_ascii_lowercase())
                } else {
                    Member::File(hash.to_ascii_lowercase())
                }
            };
            let member_path = bundle.join(relative);
            if members.insert(member_path, member).is_some() {
                return Err(invalid(
                    &path,
                    "duplicate portable Office runtime manifest path",
                ));
            }
        }
        for member in members.keys().cloned().collect::<Vec<_>>() {
            for parent in member
                .ancestors()
                .skip(1)
                .take_while(|parent| *parent != bundle)
            {
                check_verification_deadline(deadline)?;
                let at_limit = members.len() >= MAX_PACKAGE_ENTRIES;
                match members.entry(parent.into()) {
                    Entry::Vacant(_) if at_limit => {
                        return Err(invalid(
                            &path,
                            "too many portable Office runtime manifest entries",
                        ));
                    }
                    Entry::Vacant(entry) => {
                        entry.insert(Member::Directory);
                    }
                    Entry::Occupied(entry) if *entry.get() != Member::Directory => {
                        return Err(invalid(
                            parent,
                            "portable runtime manifest parent is not a directory",
                        ));
                    }
                    Entry::Occupied(_) => {}
                }
            }
        }
        records.verify(deadline)?;
        Ok(Self { members, records })
    }

    pub(super) fn verify(
        &self,
        path: &Path,
        metadata: &Metadata,
        deadline: Instant,
    ) -> io::Result<()> {
        let (actual, expected) = match self.members.get(path) {
            Some(Member::Directory) if metadata.is_dir() => return Ok(()),
            Some(Member::File(hash)) if metadata.is_file() => {
                let mut file = open_package_file(path, metadata)?;
                let mut digest = Sha256::new();
                let mut bytes = [0; 64 * 1024];
                loop {
                    check_verification_deadline(deadline)?;
                    let size = file.read(&mut bytes)?;
                    if size == 0 {
                        break;
                    }
                    digest.update(&bytes[..size]);
                }
                (digest.finalize(), hash)
            }
            Some(Member::Link(hash)) if metadata.is_symlink() => {
                let target = fs::read_link(path)?;
                (Sha256::digest(target.as_os_str().as_encoded_bytes()), hash)
            }
            Some(_) => {
                return Err(invalid(
                    path,
                    "unexpected portable Office runtime file type",
                ));
            }
            None => {
                return Err(invalid(
                    path,
                    "unlisted in portable Office runtime manifest",
                ));
            }
        };
        if hex_digest(&actual) != *expected {
            return Err(invalid(path, "portable Office runtime checksum mismatch"));
        }
        Ok(())
    }

    pub(super) fn verify_complete(
        &self,
        snapshot: &RuntimeSnapshot,
        deadline: Instant,
    ) -> io::Result<()> {
        for path in self.members.keys() {
            check_verification_deadline(deadline)?;
            if !snapshot.contains_entry(path) {
                return Err(invalid(path, "missing portable Office runtime member"));
            }
        }
        Ok(())
    }
}

fn invalid(path: &Path, message: &str) -> io::Error {
    io::Error::other(format!("{message}: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn check(manifest: &str) -> io::Result<PortableCatalog> {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(MANIFEST), manifest)?;
        PortableCatalog::read(
            root.path(),
            RuntimeTrust::fixture(root.path()),
            Instant::now() + Duration::from_secs(10),
        )
    }

    #[test]
    fn portable_rejects_a_long_path_before_expanding_parents() {
        let manifest = format!("dir  share/{}\n", "a".repeat(1024));
        let error = check(&manifest).err().expect("long paths must be rejected");
        assert!(error.to_string().contains("path length"), "{error}");
    }

    #[test]
    fn portable_rejects_a_deep_path_before_expanding_parents() {
        let manifest = format!("dir  share/{}leaf\n", "a/".repeat(32));
        let error = check(&manifest).err().expect("deep paths must be rejected");
        assert!(error.to_string().contains("path depth"), "{error}");
    }

    #[test]
    fn portable_bounds_explicit_entries_and_counts_implicit_parents() {
        let manifest: String = (0..32768).map(|i| format!("dir  share/{i}\n")).collect();
        assert!(
            check(&format!(
                "dir  share\n{}",
                manifest.lines().take(32767).collect::<Vec<_>>().join("\n")
            ))
            .is_ok()
        );
        let explicit = check(&format!("dir  share\n{manifest}"));
        let implicit = check(&manifest);
        for result in [explicit, implicit] {
            let error = result
                .err()
                .expect("entry limits must count every directory");
            assert!(error.to_string().contains("too many"), "{error}");
        }
    }
}
