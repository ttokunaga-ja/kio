use std::fs;
use std::path::Path;

/// Windows counterpart. **Windows has no directory fsync**, so this arm cannot
/// make the same durability promise as the POSIX one above.
///
/// What survives is the *fail-closed* half of R23-07: a parent that is missing
/// or is not a directory still surfaces to the caller rather than
/// type-checking as success. That is the failure this arm can actually see.
pub(crate) fn sync_directory(path: &Path) -> std::io::Result<()> {
    if fs::metadata(path)?.is_dir() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            "sync_directory expects a directory",
        ))
    }
}
