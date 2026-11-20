use crate::office_runtime_identity::{RuntimeSnapshot, check_verification_deadline};
use kio_process::confinement::RenderSources;
use std::{io::Result, path::PathBuf, time::Instant};

impl RuntimeSnapshot {
    pub(crate) fn new(
        roots: Vec<PathBuf>,
        sources: &RenderSources,
        deadline: Instant,
    ) -> Result<Self> {
        Self::new_checked(roots, sources, None, &|_, _| Ok(()), deadline)
    }

    pub(crate) fn verify_signatures(
        &self,
        sources: &RenderSources,
        deadline: Instant,
        verify: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        check_verification_deadline(deadline)?;
        verify()?;
        self.verify(sources, "during signature verification", deadline)
    }
}
