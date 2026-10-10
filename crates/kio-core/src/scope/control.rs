use super::*;

impl Repository {
    pub(super) fn authority_capture_ready(&self) -> Option<&Path> {
        #[cfg(debug_assertions)]
        {
            let control = &self.control().debug().core;
            control.snapshot_authority_capture_ready.as_deref()
        }
        #[cfg(not(debug_assertions))]
        {
            None
        }
    }

    pub(super) fn pre_checkpoint_ready(&self) -> Option<&Path> {
        #[cfg(debug_assertions)]
        {
            let control = &self.control().debug().core;
            control.snapshot_pre_checkpoint_ready.as_deref()
        }
        #[cfg(not(debug_assertions))]
        {
            None
        }
    }

    pub(super) fn after_state_write_ready(&self) -> Option<&Path> {
        #[cfg(debug_assertions)]
        {
            let control = &self.control().debug().core;
            control.snapshot_after_state_write_ready.as_deref()
        }
        #[cfg(not(debug_assertions))]
        {
            None
        }
    }

    pub(super) fn hold_lock_ready(&self) -> Option<&Path> {
        #[cfg(debug_assertions)]
        {
            let control = &self.control().debug().core;
            control.hold_lock_ready.as_deref()
        }
        #[cfg(not(debug_assertions))]
        {
            None
        }
    }

    #[must_use]
    pub fn control(&self) -> &crate::control::ControlHandle {
        self.store.control()
    }

    #[must_use]
    pub fn now_utc_seconds(&self) -> String {
        self.control().now_utc_seconds()
    }

    #[must_use]
    pub fn rebind_with_control(&self, control: crate::control::ControlHandle) -> Self {
        let mut repository = self.clone();
        repository.store = self.store.rebind_with_control(control);
        repository
    }
}
