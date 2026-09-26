//! Separate device-private batch recovery HMAC key. Never reuse cursor keys.

use kio_adapter::batch_recovery::BatchRecoveryContext;
use kio_core::store_dir::{Publication, StoreDirectory};
use kio_core::{ExitCode, KioError, Result};
use std::path::Path;

const KEY_LEAF: &str = "batch-recovery-key";
const KEY_BYTES: u64 = 32;

pub(crate) fn load_or_create(
    parent_path: &Path,
    pending_bound_authority: bool,
) -> Result<BatchRecoveryContext> {
    let parent = crate::private_state::ensure_private_directory(parent_path)?;
    load_or_create_at(&parent, pending_bound_authority)
}

fn load_or_create_at(
    parent: &StoreDirectory,
    pending_bound_authority: bool,
) -> Result<BatchRecoveryContext> {
    let leaf = Path::new(KEY_LEAF);
    if parent.contains_entry(leaf)? {
        return read_key(parent);
    }
    if pending_bound_authority {
        return Err(invalid_key(
            "batch recovery key is missing while bound requests remain; restore the original private key or explicitly abandon those requests before creating a replacement",
        ));
    }
    let mut key = [0; 32];
    getrandom::fill(&mut key)
        .map_err(|_| invalid_key("operating system random source failed for batch recovery key"))?;
    match parent.write_atomic(leaf, &key, Publication::CreateOnly) {
        Ok(()) => read_key(parent),
        Err(error) => {
            if parent.contains_entry(leaf)? {
                read_key(parent)
            } else {
                Err(error)
            }
        }
    }
}

fn read_key(parent: &StoreDirectory) -> Result<BatchRecoveryContext> {
    let bytes = kio_core::private_fs::read_private_file_at(parent, KEY_LEAF, KEY_BYTES)?;
    let key: [u8; 32] = bytes
        .try_into()
        .map_err(|_| invalid_key("batch recovery key must contain exactly 32 bytes"))?;
    Ok(BatchRecoveryContext::from_key(key))
}

fn invalid_key(message: &str) -> KioError {
    KioError::new(
        "KIO-E-BATCH-RECOVERY-KEY-001",
        message,
        serde_json::json!({}),
        ExitCode::PermanentFailure,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn private_tempdir() -> tempfile::TempDir {
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let directory = StoreDirectory::open(root.path()).unwrap();
        kio_core::store_dir::restrict_new_private_directory(&directory.root_handle()).unwrap();
        root
    }

    #[test]
    fn private_key_is_reused_and_malformed_key_is_never_replaced() {
        let root = private_tempdir();
        let parent = crate::private_state::ensure_private_directory(root.path()).unwrap();
        load_or_create_at(&parent, false).unwrap();
        let original = fs::read(root.path().join(KEY_LEAF)).unwrap();
        assert_eq!(original.len(), 32);
        load_or_create_at(&parent, true).unwrap();
        assert_eq!(fs::read(root.path().join(KEY_LEAF)).unwrap(), original);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(root.path().join(KEY_LEAF))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        parent
            .write_atomic(Path::new(KEY_LEAF), b"short", Publication::Replace)
            .unwrap();
        assert!(load_or_create_at(&parent, false).is_err());
        assert_eq!(fs::read(root.path().join(KEY_LEAF)).unwrap(), b"short");
    }

    #[test]
    fn missing_key_with_pending_authority_is_not_recreated() {
        let root = private_tempdir();
        let parent = crate::private_state::ensure_private_directory(root.path()).unwrap();
        assert!(load_or_create_at(&parent, true).is_err());
        assert!(!parent.contains_entry(Path::new(KEY_LEAF)).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn linked_key_is_rejected_without_touching_its_target() {
        let root = private_tempdir();
        let target = root.path().join("victim");
        fs::write(&target, [8u8; 32]).unwrap();
        std::os::unix::fs::symlink(&target, root.path().join(KEY_LEAF)).unwrap();
        let parent = crate::private_state::ensure_private_directory(root.path()).unwrap();
        assert!(load_or_create_at(&parent, false).is_err());
        assert_eq!(fs::read(target).unwrap(), [8u8; 32]);
    }
}
