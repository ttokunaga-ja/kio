use crate::platform::unix::scratch::sampled_scratch_bytes;
use crate::*;
use std::fs::File;

pub(crate) struct UnixRendererMonitor {
    scratch: File,
    pub(super) scratch_path: std::path::PathBuf,
    max_scratch_bytes: u64,
    pub(super) next_scratch_observation: Instant,
}

impl UnixRendererMonitor {
    pub(crate) fn new(
        scratch: File,
        scratch_path: std::path::PathBuf,
        max_scratch_bytes: u64,
        physical_memory_limit: u64,
    ) -> Self {
        let _ = physical_memory_limit;
        Self {
            scratch,
            scratch_path,
            max_scratch_bytes,
            next_scratch_observation: Instant::now() + Duration::from_millis(50),
        }
    }
}

impl BoundedProcessMonitor for UnixRendererMonitor {
    fn observe(&mut self, child: &Child) -> Result<(), BoundedProcessError> {
        let _ = child;
        let now = Instant::now();
        if now < self.next_scratch_observation {
            return Ok(());
        }
        self.next_scratch_observation = now + Duration::from_millis(50);
        let observed = sampled_scratch_bytes(&self.scratch, &self.scratch_path)
            .map_err(BoundedProcessError::ScratchInspection)?;
        if observed > self.max_scratch_bytes {
            return Err(BoundedProcessError::ScratchLimit {
                limit: self.max_scratch_bytes,
                observed,
            });
        }
        Ok(())
    }
}
