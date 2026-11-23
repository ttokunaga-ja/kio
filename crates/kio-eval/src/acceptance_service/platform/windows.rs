use super::*;

pub(in crate::acceptance_service) fn assert_pid_absent(pid: u32) -> Result<(), AcceptanceError> {
    let output = scheduler_query("tasklist.exe", &["/FI", &format!("PID eq {pid}"), "/NH"])?;
    if output.status.success() && output.stdout.contains(&pid.to_string()) {
        return Err(invalid("watcher process remains after stop"));
    }
    Ok(())
}

pub(in crate::acceptance_service) fn registered_definition(
    private: &Path,
    id: &str,
) -> Result<PathBuf, AcceptanceError> {
    Ok(private
        .join("xdg-data/kio/watch-services-native")
        .join(format!("{id}.xml")))
}

pub(in crate::acceptance_service) fn assert_native_unregistered(
    id: &str,
) -> Result<(), AcceptanceError> {
    if scheduler_query("schtasks.exe", &["/query", "/tn", id])?
        .status
        .success()
    {
        return Err(invalid("Task Scheduler still has service after uninstall"));
    }
    Ok(())
}
