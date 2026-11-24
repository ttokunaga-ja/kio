use super::watch_task_definition::{task_definition_matches, task_lookup};
use crate::watch_service::service_error;
use base64::Engine;
use kio_core::Result;
use serde::{Deserialize, Serialize};

pub(crate) fn automation_command(
    program: &std::path::Path,
    windows: &std::path::Path,
    script: &str,
) -> Result<std::process::Command> {
    let mut command = std::process::Command::new(program);
    command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            script,
        ])
        .current_dir(
            program
                .parent()
                .ok_or_else(|| service_error("OS tool has no installation directory"))?,
        )
        .env_clear()
        .env("SystemRoot", windows);
    Ok(command)
}

pub(crate) fn automation_request(
    operation: &str,
    id: &str,
    template: &str,
    allowed: &[&str],
) -> Result<Vec<u8>> {
    let request = serde_json::to_vec(&serde_json::json!({
        "operation": operation, "id": id, "template": template, "allowed": allowed,
    }))
    .map_err(service_error)?;
    let mut frame = base64::engine::general_purpose::STANDARD
        .encode(request)
        .into_bytes();
    // ReadLine can complete on the delimiter even if stdin remains open.
    frame.push(b'\n');
    Ok(frame)
}

#[cfg(test)]
#[test]
fn automation_uses_fixed_noninteractive_arguments_and_a_framed_data_request() {
    use std::path::Path;

    let program = Path::new("Windows/System32/WindowsPowerShell/v1.0/powershell.exe");
    let script = include_str!("../platform/windows/watch_task.ps1");
    let command = automation_command(program, Path::new("Windows"), script).unwrap();
    assert_eq!(command.get_program(), program);
    let args = command
        .get_args()
        .map(|arg| arg.to_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        args,
        [
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            script
        ]
    );
    // Windows PowerShell receives fixed script text as a single command argument.
    assert!(!script.contains('"'));
    assert!(script.contains("[Console]::In.ReadLine()"));
    assert!(script.contains("$folder.GetTask($request.id)"));
    assert!(!script.contains("GetTasks("));
    assert!(
        script
            .contains("RegisterTaskDefinition($request.id, $definition, 2, $sid, $null, 3, $null)")
    );

    let id = "kio-test";
    let template = "<xml>日本語</xml>";
    for operation in ["query", "create", "enable"] {
        let frame = automation_request(operation, id, template, &["true"]).unwrap();
        let line = frame.strip_suffix(b"\n").unwrap();
        assert!(!line.contains(&b'\n'));
        let json = base64::engine::general_purpose::STANDARD
            .decode(line)
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&json).unwrap(),
            serde_json::json!({"operation":operation,"id":id,"template":template,"allowed":["true"]})
        );
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskQuery {
    hresult: u32,
    lookup_hresult: u32,
    pub(crate) present: bool,
    actual: Option<String>,
    enabled_xml: Option<String>,
    disabled_xml: Option<String>,
    pub(crate) state: u32,
    pub(crate) arguments: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskRecord {
    pub(crate) template_hash: String,
    pub(crate) enabled: bool,
    pub(crate) pending_enabled: Option<bool>,
}
impl TaskRecord {
    pub(crate) fn allowed(&self) -> Vec<&'static str> {
        let mut states = vec![if self.enabled { "true" } else { "false" }];
        if let Some(pending) = self.pending_enabled {
            let state = if pending { "true" } else { "false" };
            if !states.contains(&state) {
                states.push(state);
            }
        }
        states
    }
}

pub(crate) fn validate_query(result: TaskQuery) -> Result<TaskQuery> {
    // A missing COM component or a failed connection is never task absence.
    // Only the HRESULT returned by GetTask itself may establish absence.
    if result.hresult != 0 {
        return Err(service_error(format!(
            "Task Scheduler API HRESULT 0x{:08x}",
            result.hresult
        )));
    }
    if task_lookup(result.lookup_hresult)? != result.present
        || (result.present && result.actual.is_none())
    {
        return Err(service_error("inconsistent Task Scheduler response"));
    }
    Ok(result)
}

pub(crate) fn matches_enabled(query: &TaskQuery, enabled: bool) -> bool {
    let Some(actual) = query.actual.as_deref() else {
        return false;
    };
    let expected = if enabled {
        query.enabled_xml.as_deref()
    } else {
        query.disabled_xml.as_deref()
    };
    expected.is_some_and(|xml| task_definition_matches(actual, xml))
}
pub(crate) fn matches_record(query: &TaskQuery, record: &TaskRecord) -> bool {
    matches_enabled(query, record.enabled)
        || record
            .pending_enabled
            .is_some_and(|enabled| matches_enabled(query, enabled))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply() -> TaskQuery {
        TaskQuery { hresult: 0, lookup_hresult: 0, present: true,
            actual: Some("<Task><Settings><Enabled>true</Enabled></Settings><Actions><Exec><Command>kio</Command></Exec></Actions></Task>".into()),
            enabled_xml: Some("<Task><Settings><Enabled>true</Enabled></Settings><Actions><Exec><Command>kio</Command></Exec></Actions></Task>".into()),
            disabled_xml: Some("<Task><Settings><Enabled>false</Enabled></Settings><Actions><Exec><Command>kio</Command></Exec></Actions></Task>".into()), state: 3, arguments: None }
    }
    #[test]
    fn scheduler_connection_failure_cannot_masquerade_as_absence() {
        let mut result = reply();
        assert_eq!(result.state, 3);
        assert!(result.arguments.is_none());
        result.hresult = 0x80070002;
        result.present = false;
        assert!(validate_query(result).is_err());
        let mut result = reply();
        result.lookup_hresult = 0x80070002;
        result.present = false;
        result.actual = None;
        assert!(!validate_query(result).unwrap().present);
        let mut result = reply();
        result.lookup_hresult = 0x80070005;
        result.present = false;
        assert!(validate_query(result).is_err());
    }
    #[test]
    fn os_only_task_fields_and_unapproved_enable_changes_are_foreign() {
        let record = TaskRecord {
            template_hash: "fixture".into(),
            enabled: true,
            pending_enabled: None,
        };
        assert!(matches_record(&reply(), &record));
        let mut foreign = reply();
        foreign.actual = foreign.actual.map(|xml| {
            xml.replace(
                "</Exec>",
                "<WorkingDirectory>foreign</WorkingDirectory></Exec>",
            )
        });
        assert!(!matches_record(&foreign, &record));
        let mut disabled = reply();
        disabled.actual = disabled.disabled_xml.clone();
        assert!(!matches_record(&disabled, &record));
        let pending = TaskRecord {
            pending_enabled: Some(false),
            ..record
        };
        assert_eq!(pending.allowed(), ["true", "false"]);
        assert!(matches_record(&disabled, &pending));
        assert!(!matches_enabled(&reply(), false));
        assert!(matches_enabled(&disabled, false));
    }
}
