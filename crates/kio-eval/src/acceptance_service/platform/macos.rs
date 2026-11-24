use super::*;

pub(in crate::acceptance_service) fn registered_definition(
    deadline: Instant,
    _private: &Path,
    id: &str,
) -> Result<PathBuf, AcceptanceError> {
    let output = print_agent(deadline, id)?;
    if !output.status.success() {
        return Err(invalid("launchd could not inspect the running LaunchAgent"));
    }
    let path = agent_source(&output.stdout, id)?;
    if !path.is_file() {
        return Err(invalid("running LaunchAgent source file is missing"));
    }
    Ok(path)
}

fn agent_source(printed: &str, id: &str) -> Result<PathBuf, AcceptanceError> {
    let mut paths = printed
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("path = "))
        .map(Path::new);
    let expected_name = format!("{id}.plist");
    match (paths.next(), paths.next()) {
        (Some(path), None)
            if path.is_absolute()
                && path
                    .file_name()
                    .is_some_and(|name| name == expected_name.as_str()) =>
        {
            Ok(path.to_path_buf())
        }
        _ => Err(invalid(
            "launchd did not report the running LaunchAgent source path",
        )),
    }
}

pub(in crate::acceptance_service) fn assert_native_unregistered(
    deadline: Instant,
    id: &str,
) -> Result<(), AcceptanceError> {
    let output = print_agent(deadline, id)?;
    if output.status.code() != Some(113) || !output.stderr.contains("Could not find service") {
        return Err(invalid(
            "launchd did not confirm the service is absent after uninstall",
        ));
    }
    Ok(())
}

fn print_agent(
    deadline: Instant,
    id: &str,
) -> Result<crate::runner::BoundedProcessOutput, AcceptanceError> {
    let target = format!("gui/{}/{}", unsafe { libc::geteuid() }, id);
    scheduler_query(deadline, "/bin/launchctl", &["print", &target])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_loaded_agent_source_in_the_login_account() {
        let id = "io.kio.watch.example";
        let path = "/Users/Login Account/Library/LaunchAgents/io.kio.watch.example.plist";
        let print = format!("gui/501/{id} = {{\n\tpath = {path}\n\tstate = running\n}}");
        assert_eq!(agent_source(&print, id).unwrap(), Path::new(path));
        assert!(agent_source(&print, "io.kio.watch.other").is_err());
        assert!(agent_source(&format!("{print}\n\tpath = {path}"), id).is_err());
        assert!(agent_source("path = relative.plist", "relative").is_err());
    }
}
