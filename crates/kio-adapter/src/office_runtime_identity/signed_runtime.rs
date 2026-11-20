use crate::office_runtime_identity::{ExecutableFingerprint, RuntimeSnapshot};
use kio_process::confinement::RenderSources;
use std::{io, path::PathBuf, time::Instant};

#[cfg(debug_assertions)]
pub(crate) fn office_isolation_fault(
    _: &str,
) -> Option<kio_process::confinement::ConfinementError> {
    None
}

impl crate::platform::OfficeRuntime {
    pub(crate) fn bind_runtime(&self, sources: RenderSources) -> crate::Result<RenderSources> {
        Ok(sources)
    }

    pub(crate) fn snapshot(
        &self,
        roots: Vec<PathBuf>,
        sources: &RenderSources,
        deadline: Instant,
    ) -> io::Result<RuntimeSnapshot> {
        RuntimeSnapshot::new(roots, sources, deadline)
    }

    pub(crate) fn verify_roots(&self, _: Instant) -> io::Result<()> {
        Ok(())
    }

    pub(crate) fn verify_initial_runtime(
        &self,
        sources: &RenderSources,
        snapshot: &RuntimeSnapshot,
        fingerprint: &ExecutableFingerprint,
        deadline: Instant,
    ) -> crate::Result<()> {
        snapshot
            .verify_signatures(sources, deadline, || {
                let program = sources.program();
                self.verify_captured_signature(program, snapshot, deadline)?;
                fingerprint.verify(program, "during signature verification", deadline)
            })
            .map_err(|error| crate::AdapterError::ContractViolation(error.to_string()))
    }
}
