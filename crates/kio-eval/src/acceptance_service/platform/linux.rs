use super::*;

pub(in crate::acceptance_service) fn registered_definition(
    deadline: Instant,
    _private: &Path,
    id: &str,
) -> Result<PathBuf, AcceptanceError> {
    let output = systemctl(
        deadline,
        &["show", "--property=FragmentPath", "--value", id],
    )?;
    let path = PathBuf::from(output.stdout.trim_end_matches('\n'));
    let expected_name = format!("{id}.service");
    if !output.status.success()
        || !path.is_absolute()
        || path
            .file_name()
            .is_none_or(|name| name != expected_name.as_str())
        || !path.is_file()
    {
        return Err(invalid(
            "systemd did not report the installed unit source file",
        ));
    }
    Ok(path)
}

pub(in crate::acceptance_service) fn assert_disabled(
    deadline: Instant,
    id: &str,
) -> Result<(), AcceptanceError> {
    let output = systemctl(
        deadline,
        &["show", "--property=UnitFileState", "--value", id],
    )?;
    if !output.status.success() || output.stdout.trim() != "disabled" {
        return Err(invalid(
            "systemd unit is not installed and disabled after stop",
        ));
    }
    Ok(())
}

pub(in crate::acceptance_service) fn assert_native_unregistered(
    deadline: Instant,
    id: &str,
) -> Result<(), AcceptanceError> {
    let output = systemctl(deadline, &["show", "--property=LoadState", "--value", id])?;
    if output.stdout.trim() != "not-found"
        || systemctl(deadline, &["is-enabled", id])?.status.success()
    {
        return Err(invalid(
            "systemd did not confirm the service is absent after uninstall",
        ));
    }
    Ok(())
}

fn systemctl(
    deadline: Instant,
    args: &[&str],
) -> Result<crate::runner::BoundedProcessOutput, AcceptanceError> {
    scheduler_query(
        deadline,
        "/usr/bin/systemctl",
        &[&["--user"], args].concat(),
    )
}
