use std::path::Path;

pub(crate) fn protect_owner_private_scratch(
    path: &Path,
) -> Result<(), kio_process::confinement::ConfinementError> {
    kio_process::platform::protect_owner_private_scratch(path)
}
