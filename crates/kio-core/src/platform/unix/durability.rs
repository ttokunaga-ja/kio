use std::fs::File;
use std::path::Path;

/// Fsync a directory so a prior rename/unlink within it is durable (R23-07,
/// 05-runtime.md §3.5 L834-836 "temp 書込 → file fsync → atomic rename →
/// 親 directory fsync" and L843-845 "journal を除去 + directory fsync").
/// Must propagate failure to its caller: silently swallowing a failed
/// open/fsync here would let a legacy path-based object-store publication
/// report success before its directory entry was durable. Retained
/// `StoreDirectory` purge operations enforce the corresponding ordering by
/// descriptor-relative primitives.
///
/// `pub(crate)` because the object store needs the identical guarantee for its
/// own post-`remove_file` entries: [`crate::cas`] carried two hand-rolled
/// copies of the pre-R23-07 `if let Ok(dir) = File::open(parent)` shape, which
/// reproduced both defects one layer down — the discarded POSIX fsync, and the
/// permanent Windows no-op. They now call this, so the two arms have exactly
/// one definition to keep correct.
pub(crate) fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}
