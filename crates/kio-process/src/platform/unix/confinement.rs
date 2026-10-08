//! Native Unix renderer resource limits.
use crate::confinement::{ConfinementError, RenderResourceLimits};
use std::process::Command;

pub(crate) fn apply_unix_limits(
    command: &mut Command,
    limits: RenderResourceLimits,
    address_space_optional: bool,
) -> Result<(), ConfinementError> {
    use std::os::unix::process::CommandExt;
    let cpu = libc::rlim_t::try_from(limits.cpu_seconds).map_err(|_| {
        ConfinementError::Resource(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "CPU limit out of range",
        ))
    })?;
    let address_space = libc::rlim_t::try_from(limits.max_address_space_bytes).map_err(|_| {
        ConfinementError::Resource(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "address-space limit out of range",
        ))
    })?;
    let file_size = libc::rlim_t::try_from(limits.max_file_bytes).map_err(|_| {
        ConfinementError::Resource(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "file-size limit out of range",
        ))
    })?;
    unsafe {
        command.pre_exec(move || {
            for (resource, value, optional) in [
                (libc::RLIMIT_CPU, cpu, false),
                // Darwin exposes RLIMIT_AS but does not consistently support
                // applying it to every process class. CPU and output-file
                // limits remain mandatory; unsupported address-space limits
                // are handled by the platform-specific Windows Job path.
                (libc::RLIMIT_AS, address_space, address_space_optional),
                (libc::RLIMIT_FSIZE, file_size, false),
            ] {
                if optional && value == 0 {
                    continue;
                }
                let limit = libc::rlimit {
                    rlim_cur: value,
                    rlim_max: value,
                };
                if libc::setrlimit(resource, &limit) != 0 {
                    if optional
                        && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL)
                    {
                        continue;
                    }
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    Ok(())
}
