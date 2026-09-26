//! Device-private HMAC key material for search cursors.
//!
//! The key is reachable only through a retained owner-private directory.  In
//! particular, an ambient filename is never re-opened after that directory has
//! been accepted: an existing unsafe leaf is rejected, and a concurrent creator
//! is read back through the same retained parent.

use std::path::Path;

use kio_core::store_dir::{Publication, StoreDirectory};
use kio_core::{ExitCode, KioError, Result};

const CURSOR_KEY_LEAF: &str = "cursor-key";
const CURSOR_KEY_BYTES: u64 = 32;

pub(crate) fn load_or_create(parent_path: &Path) -> Result<Vec<u8>> {
    let parent = crate::private_state::ensure_private_directory(parent_path)?;
    load_or_create_at(&parent)
}

fn load_or_create_at(parent: &StoreDirectory) -> Result<Vec<u8>> {
    let leaf = Path::new(CURSOR_KEY_LEAF);
    if parent.contains_entry(leaf)? {
        return read_key(parent);
    }

    let key = random_key_32()?;
    match parent.write_atomic(leaf, &key, Publication::CreateOnly) {
        Ok(()) => read_key(parent),
        Err(write_error) => {
            // A create-only collision is expected when another process won.
            // Do not replace an occupied name; strictly read the winner through
            // the same retained parent.  If it was not a collision, preserve
            // the publication error.
            if parent.contains_entry(leaf)? {
                read_key(parent)
            } else {
                Err(write_error)
            }
        }
    }
}

fn read_key(parent: &StoreDirectory) -> Result<Vec<u8>> {
    let key =
        kio_core::private_fs::read_private_file_at(parent, CURSOR_KEY_LEAF, CURSOR_KEY_BYTES)?;
    if key.len() != CURSOR_KEY_BYTES as usize {
        return Err(invalid_key(
            "cursor signing key must contain exactly 32 bytes",
        ));
    }
    Ok(key)
}

fn random_key_32() -> Result<Vec<u8>> {
    let mut key = vec![0u8; CURSOR_KEY_BYTES as usize];
    getrandom::fill(&mut key).map_err(|error| {
        KioError::io(
            format!("operating system random source failed: {error}"),
            "operating system random source",
        )
    })?;
    Ok(key)
}

fn invalid_key(message: &str) -> KioError {
    KioError::new(
        "KIO-E-CURSOR-KEY-001",
        message,
        serde_json::json!({}),
        ExitCode::PermanentFailure,
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::fs::File;
    #[cfg(unix)]
    use std::os::fd::AsRawFd;
    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::Path;

    use kio_core::store_dir::Publication;

    use super::{CURSOR_KEY_LEAF, load_or_create_at};

    fn private_tempdir() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    #[test]
    fn reuses_a_private_exact_length_key() {
        let root = private_tempdir();
        let parent = crate::private_state::ensure_private_directory(root.path()).unwrap();
        let first = load_or_create_at(&parent).unwrap();
        let second = load_or_create_at(&parent).unwrap();

        assert_eq!(first.len(), 32);
        assert_eq!(first, second);
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(root.path().join(CURSOR_KEY_LEAF))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn malformed_or_symlink_key_is_rejected_without_replacement() {
        let malformed_root = private_tempdir();
        let malformed_parent =
            crate::private_state::ensure_private_directory(malformed_root.path()).unwrap();
        malformed_parent
            .write_atomic(
                Path::new(CURSOR_KEY_LEAF),
                b"too-short",
                Publication::CreateOnly,
            )
            .unwrap();
        let error = load_or_create_at(&malformed_parent).unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-CURSOR-KEY-001");
        assert_eq!(
            fs::read(malformed_root.path().join(CURSOR_KEY_LEAF)).unwrap(),
            b"too-short"
        );

        #[cfg(unix)]
        {
            let symlink_root = private_tempdir();
            let victim = private_tempdir();
            let victim_key = victim.path().join("key");
            fs::write(&victim_key, b"victim-key-is-not-replaced").unwrap();
            symlink(&victim_key, symlink_root.path().join(CURSOR_KEY_LEAF)).unwrap();
            let symlink_parent =
                crate::private_state::ensure_private_directory(symlink_root.path()).unwrap();

            assert!(load_or_create_at(&symlink_parent).is_err());
            assert_eq!(fs::read(victim_key).unwrap(), b"victim-key-is-not-replaced");
        }
    }

    #[cfg(unix)]
    #[test]
    fn retained_inherited_parent_cannot_be_retargeted_after_fd_swap() {
        let original = private_tempdir();
        let replacement = private_tempdir();
        let moved = original.path().with_extension("retained");
        let original_handle = File::open(original.path()).unwrap();
        let inherited =
            std::path::PathBuf::from(format!("/dev/fd/{}", original_handle.as_raw_fd()));
        let (parent, suffix) = kio_core::private_fs::resolve_inherited_private_root(&inherited)
            .unwrap()
            .expect("canonical inherited root");
        assert!(suffix.is_empty());

        fs::rename(original.path(), &moved).unwrap();
        fs::create_dir(original.path()).unwrap();
        fs::set_permissions(original.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let replacement_handle = File::open(replacement.path()).unwrap();
        assert!(
            unsafe { libc::dup2(replacement_handle.as_raw_fd(), original_handle.as_raw_fd()) } >= 0
        );

        let key = load_or_create_at(&parent).unwrap();
        assert_eq!(fs::read(moved.join(CURSOR_KEY_LEAF)).unwrap(), key);
        assert!(!original.path().join(CURSOR_KEY_LEAF).exists());
        assert!(!replacement.path().join(CURSOR_KEY_LEAF).exists());
        fs::remove_dir_all(&moved).unwrap();
    }
}
