use std::path::{Path, PathBuf};
use std::sync::Arc;

use kio_core::Result;
use kio_core::scope::Repository;

pub trait Interaction: Send + Sync {
    fn confirm(&self, summary: &str) -> Result<bool>;
    fn read_input(&self, max_bytes: usize) -> Result<Vec<u8>>;
    fn is_interactive(&self) -> bool;
    fn diagnostic(&self, message: &str);
}

#[derive(Clone)]
pub struct AppContext {
    pub working_directory: PathBuf,
    pub interaction: Arc<dyn Interaction + Send + Sync>,
}

impl AppContext {
    pub(crate) fn cwd(&self) -> &Path {
        &self.working_directory
    }
    pub(crate) fn open_read_only_repository(&self) -> Result<Repository> {
        Repository::open_read_only(self.cwd())
    }
    pub(crate) fn open_recovery_repository(&self) -> Result<Repository> {
        Repository::open_for_recovery(self.cwd())
    }
    pub(crate) fn open_search_repository(&self) -> Result<Repository> {
        Repository::open_for_search_read_only(self.cwd())
    }
}

#[derive(Default)]
pub(crate) struct EvidenceRegistry {
    #[cfg(test)]
    fixture_path: Option<PathBuf>,
}

impl EvidenceRegistry {
    #[cfg(test)]
    pub(crate) fn fixture(path: PathBuf) -> Self {
        Self {
            fixture_path: Some(path),
        }
    }

    pub(crate) fn path(&self) -> Result<PathBuf> {
        #[cfg(test)]
        if let Some(path) = &self.fixture_path {
            return Ok(path.clone());
        }
        kio_index::registry::default_registry_path().map_err(|error| {
            crate::registry_snapshot_transient_error(
                Path::new("<unresolved>"),
                "path_resolution",
                error.to_string(),
            )
        })
    }
}

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
mod tests;
