use std::path::Path;
use std::sync::Arc;

use super::{AppContext, Interaction};
use kio_core::Result;

struct Unattended;
impl Interaction for Unattended {
    fn confirm(&self, _: &str) -> Result<bool> {
        Ok(false)
    }
    fn read_input(&self, _: usize) -> Result<Vec<u8>> {
        Ok(Vec::new())
    }
    fn is_interactive(&self) -> bool {
        false
    }
    fn diagnostic(&self, _: &str) {}
    fn write_diagnostic(&self, _: &str) -> kio_core::Result<()> {
        Ok(())
    }
    fn flush_diagnostics(&self) -> kio_core::Result<()> {
        Ok(())
    }
}

pub(crate) fn context(cwd: &Path) -> AppContext {
    AppContext {
        working_directory: cwd.to_path_buf(),
        interaction: Arc::new(Unattended),
    }
}
