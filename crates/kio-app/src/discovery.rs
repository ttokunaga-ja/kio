//! Durable, owner-private cursor storage for bounded child-scope discovery.
//!
//! The cursor is operational state rather than portable knowledge.  It stays
//! below the scope's retained `.kio` authority and is serialized separately
//! from the ordinary index writer lock: a child page releases that writer lock
//! before enrolling descendants, but must retain exclusive ownership of its
//! frontier until the entire page has been processed.

use std::fs::File;
use std::path::Path;

use kio_core::scope::Repository;
use kio_core::store_dir::{ATOMIC_WORKSPACE_DIR, Publication, StoreDirectory};
use kio_core::{ExitCode, KioError, Result};
use kio_pipeline::scan::{
    ChildScopeDiscoveryContinuation, parse_child_scope_discovery_continuation,
};

const DIRECTORY: &str = "internal/child-discovery";
const FRONTIER: &str = "frontier.json";
const LOCK: &str = ".lock";
const LOCK_BYTES: &[u8] = b"kio-child-discovery-frontier-lock-v1\n";
const MAX_FRONTIER_BYTES: u64 = 1024 * 1024;

/// An exclusive, retained frontier session.  The lock is held from cursor
/// loading until the page's successor cursor is committed, so a crash leaves
/// either the old page (safe to replay) or its fully processed successor.
pub(crate) struct ChildDiscoveryFrontier {
    directory: StoreDirectory,
    _lock: File,
}

impl ChildDiscoveryFrontier {
    pub(crate) fn acquire(repo: &Repository) -> Result<Self> {
        // The retained descriptor below prevents path redirection, while this
        // fresh named-identity proof prevents a first frontier write after the
        // managed root or its `.kio` entry was replaced.
        crate::management::binding_for_repo(repo)?.revalidate()?;
        let kio = repo.bound_kio_handle().ok_or_else(|| {
            KioError::invalid_usage("child discovery requires a retained .kio handle")
        })?;
        let kio = StoreDirectory::from_retained(
            kio.try_clone().map_err(|error| {
                KioError::io(error.to_string(), repo.kio_dir().display().to_string())
            })?,
            repo.kio_dir().to_path_buf(),
        )?;
        let child_directory = kio.create_directory_all(Path::new(DIRECTORY))?;
        let directory = StoreDirectory::from_retained(
            child_directory.try_clone().map_err(|error| {
                KioError::io(
                    error.to_string(),
                    repo.kio_dir().join(DIRECTORY).display().to_string(),
                )
            })?,
            repo.kio_dir().join(DIRECTORY),
        )?;
        validate_directory(&directory)?;
        let lock = acquire_lock(&directory)?;
        directory.recover_atomic(&[&directory])?;
        validate_directory(&directory)?;
        Ok(Self {
            directory,
            _lock: lock,
        })
    }

    /// Read and fully validate an untrusted persisted continuation before it
    /// reaches the pipeline resume API.
    pub(crate) fn load(&self) -> Result<Option<ChildScopeDiscoveryContinuation>> {
        let leaf = Path::new(FRONTIER);
        let Some(bytes) = self.directory.read_optional(leaf, MAX_FRONTIER_BYTES)? else {
            return Ok(None);
        };
        self.directory.ensure_owner_private(leaf)?;
        verify_private_transport(&self.directory, leaf, &bytes)?;
        parse_child_scope_discovery_continuation(&bytes)
            .map(Some)
            .map_err(|error| {
                KioError::new(
                    "KIO-E-CHILD-DISCOVERY-FRONTIER-CORRUPT-001",
                    "child discovery frontier is invalid",
                    serde_json::json!({ "cause": error.to_string() }),
                    ExitCode::PermanentFailure,
                )
            })
    }

    /// Atomically publish the next cursor only after every row of the current
    /// page has completed. `None` clears a completed traversal.
    pub(crate) fn commit(
        &self,
        continuation: Option<&ChildScopeDiscoveryContinuation>,
    ) -> Result<()> {
        let leaf = Path::new(FRONTIER);
        match continuation {
            Some(continuation) => {
                let bytes = serde_json::to_vec(continuation)
                    .map_err(|error| KioError::schema(error.to_string()))?;
                if bytes.len() as u64 > MAX_FRONTIER_BYTES {
                    return Err(KioError::new(
                        "KIO-E-CHILD-DISCOVERY-FRONTIER-OVERSIZED-001",
                        "child discovery frontier exceeds its byte limit",
                        serde_json::json!({ "max_bytes": MAX_FRONTIER_BYTES }),
                        ExitCode::PermanentFailure,
                    ));
                }
                // Round-trip through the pipeline's untrusted-input parser so
                // this module never persists a token it would reject later.
                parse_child_scope_discovery_continuation(&bytes).map_err(|error| {
                    KioError::new(
                        "KIO-E-CHILD-DISCOVERY-FRONTIER-CORRUPT-001",
                        "child discovery frontier cannot be validated",
                        serde_json::json!({ "cause": error.to_string() }),
                        ExitCode::PermanentFailure,
                    )
                })?;
                self.directory
                    .write_atomic(leaf, &bytes, Publication::Upsert)?;
                self.directory.ensure_owner_private(leaf)?;
                verify_private_transport(&self.directory, leaf, &bytes)?;
                self.directory.sync()
            }
            None => {
                if self
                    .directory
                    .read_optional(leaf, MAX_FRONTIER_BYTES)?
                    .is_some()
                {
                    self.directory.ensure_owner_private(leaf)?;
                    self.directory.remove_file(leaf)?;
                    self.directory.sync()?;
                }
                Ok(())
            }
        }
    }
}

fn acquire_lock(directory: &StoreDirectory) -> Result<File> {
    let leaf = Path::new(LOCK);
    if directory
        .read_optional(leaf, LOCK_BYTES.len() as u64)?
        .is_none()
    {
        let _ = directory.write_atomic(leaf, LOCK_BYTES, Publication::CreateOnly);
    }
    directory.ensure_owner_private(leaf)?;
    let bytes = directory
        .read_optional(leaf, LOCK_BYTES.len() as u64)?
        .ok_or_else(|| KioError::schema("child discovery frontier lock disappeared"))?;
    if bytes != LOCK_BYTES {
        return Err(KioError::schema(
            "child discovery frontier lock has unexpected contents",
        ));
    }
    verify_private_transport(directory, leaf, &bytes)?;
    let file = directory.open_regular_read(leaf, LOCK_BYTES.len() as u64)?;
    file.try_lock().map_err(|_| {
        KioError::new(
            "KIO-E-CHILD-DISCOVERY-FRONTIER-LOCKED-001",
            "child discovery frontier is already being processed",
            serde_json::json!({}),
            ExitCode::PartialFailure,
        )
    })?;
    Ok(file)
}

/// The retained read is the authority for normal operations. This independent
/// owner-private read rechecks the retained parent without reimporting its
/// diagnostic path; byte equality catches
/// any replacement between the two reads.
fn verify_private_transport(
    directory: &StoreDirectory,
    leaf: &Path,
    expected: &[u8],
) -> Result<()> {
    let path = directory.path().join(leaf);
    let name = leaf
        .to_str()
        .ok_or_else(|| KioError::invalid_usage("private state leaf must be UTF-8"))?;
    let private =
        kio_core::private_fs::read_private_file_at(directory, name, expected.len() as u64)?;
    if private != expected {
        return Err(KioError::new(
            "KIO-E-CHILD-DISCOVERY-FRONTIER-RACED-001",
            "child discovery frontier changed while it was being bound",
            serde_json::json!({ "path": path }),
            ExitCode::PermanentFailure,
        ));
    }
    Ok(())
}

fn validate_directory(directory: &StoreDirectory) -> Result<()> {
    // Inspection is observational before the frontier lease is acquired.
    // Only the validated reserved workspace can be recovered under that lease.
    directory.inspect_atomic()?;
    for entry in directory.entries(Path::new(""))? {
        let name = entry.name.to_string_lossy();
        if name == ATOMIC_WORKSPACE_DIR && entry.is_directory {
            continue;
        }
        if (name != FRONTIER && name != LOCK) || entry.is_directory || !entry.is_regular_file {
            return Err(KioError::schema(
                "child discovery frontier directory contains an unexpected entry",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::ChildDiscoveryFrontier;
    use crate::management::{ExplicitRoot, binding_for_repo, initialize_explicit_root};
    use kio_pipeline::scan::discover_managed_child_scopes_page;
    use std::fs;

    #[test]
    fn retained_frontier_round_trips_and_clears() {
        let temp = tempfile::tempdir().unwrap();
        let repo = match initialize_explicit_root(temp.path()).unwrap() {
            ExplicitRoot::Created(repo) => repo,
            ExplicitRoot::Existing(_) => panic!("new root must be created"),
        };
        for index in 0..130 {
            fs::create_dir(temp.path().join(format!("child-{index:03}"))).unwrap();
        }
        let binding = binding_for_repo(&repo).unwrap();
        let slice = discover_managed_child_scopes_page(&binding, None).unwrap();
        let continuation = slice.continuation.as_ref().unwrap();
        let frontier = ChildDiscoveryFrontier::acquire(&repo).unwrap();
        frontier.commit(Some(continuation)).unwrap();
        assert!(frontier.load().unwrap().is_some());
        drop(frontier);
        let frontier = ChildDiscoveryFrontier::acquire(&repo).unwrap();
        assert!(frontier.load().unwrap().is_some());
        frontier.commit(None).unwrap();
        assert!(frontier.load().unwrap().is_none());
    }
}
