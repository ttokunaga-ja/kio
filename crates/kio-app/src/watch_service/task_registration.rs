use super::watch_task_definition::task_lookup;
use crate::watch_service::service_error;
use base64::Engine;
use kio_core::Result;
use serde::{Deserialize, Serialize};

pub(crate) const TASK_SCRIPT: &str = concat!(
    include_str!("../platform/windows/watch_task_compare.ps1"),
    include_str!("../platform/windows/watch_task.ps1"),
);

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
    let script = TASK_SCRIPT;
    let command = kio_process::windows_powershell(program, Path::new("Windows"), script);
    assert_eq!(command.get_program(), program);
    let args = command
        .get_args()
        .map(|arg| arg.to_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        &args[..4],
        &["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"]
    );
    assert_eq!(args.len(), 5);
    assert!(args[4].starts_with("$PSModuleAutoLoadingPreference = 'None'"));
    assert!(args[4].contains(r"$env:SystemRoot + '\System32\WindowsPowerShell\v1.0\Modules\Microsoft.PowerShell.Utility\Microsoft.PowerShell.Utility.psd1'"));
    assert!(args[4].ends_with(script));
    assert_eq!(
        command.get_envs().collect::<Vec<_>>(),
        [(
            std::ffi::OsStr::new("SystemRoot"),
            Some(std::ffi::OsStr::new("Windows"))
        )]
    );
    // Windows PowerShell receives fixed script text as a single command argument.
    assert!(!args[4].contains('"'));
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
    phase: Option<String>,
    #[serde(default)]
    exceptions: Vec<SchedulerException>,
    lookup_hresult: u32,
    pub(crate) present: bool,
    enabled_mismatch: Option<Vec<String>>,
    disabled_mismatch: Option<Vec<String>>,
    differing_fields: Option<Vec<String>>,
    registered_xml: Option<String>,
    pub(crate) state: u32,
    pub(crate) arguments: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SchedulerException {
    hresult: u32,
    message: String,
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

pub(crate) fn scheduler_operation_error(
    error: kio_core::KioError,
    operation: &str,
    id: &str,
) -> kio_core::KioError {
    kio_core::KioError::new(
        error.error_code(),
        format!("Task Scheduler {operation} for {id}: {}", error.message()),
        error.context().clone(),
        error.exit_code(),
    )
}

pub(crate) fn validate_query(result: TaskQuery) -> Result<TaskQuery> {
    // A missing COM component or a failed connection is never task absence.
    // Only the HRESULT returned by GetTask itself may establish absence.
    if result.hresult != 0 {
        let bounded = |text: &str, limit| {
            text.chars()
                .take(limit)
                .map(|ch| if ch.is_control() { ' ' } else { ch })
                .collect::<String>()
        };
        let mut detail = format!("Task Scheduler API HRESULT 0x{:08x}", result.hresult);
        if let Some(phase) = &result.phase {
            detail.push_str(&format!(" during {}", bounded(phase, 64)));
        }
        for exception in result.exceptions.iter().take(6) {
            detail.push_str(&format!(
                "; 0x{:08x}: {}",
                exception.hresult,
                bounded(&exception.message, 384)
            ));
        }
        return Err(result.diagnostic(
            detail,
            result.differing_fields.as_deref().unwrap_or_default(),
        ));
    }
    if task_lookup(result.lookup_hresult)? != result.present
        || result.enabled_mismatch.is_some() != result.disabled_mismatch.is_some()
        || (!result.present && result.enabled_mismatch.is_some())
        || [
            &result.enabled_mismatch,
            &result.disabled_mismatch,
            &result.differing_fields,
        ]
        .into_iter()
        .flatten()
        .any(|fields| {
            fields.len() > 10
                || fields
                    .iter()
                    .any(|field| field.len() > 1024 || field.chars().any(char::is_control))
        })
    {
        return Err(service_error("inconsistent Task Scheduler response"));
    }
    Ok(result)
}

impl TaskQuery {
    fn diagnostic(&self, mut message: String, fields: &[String]) -> kio_core::KioError {
        let fields: Vec<_> = fields
            .iter()
            .take(10)
            .map(|field| {
                field
                    .chars()
                    .filter(|ch| !ch.is_control())
                    .take(256)
                    .collect::<String>()
            })
            .collect();
        if !fields.is_empty() {
            message.push_str(" at ");
            message.push_str(&fields.join(", "));
        }
        let mut error = service_error(message)
            .with_context_field("differing_fields", serde_json::json!(fields));
        if let Some(xml) = &self.registered_xml {
            error = error.with_context_field(
                "registered_xml",
                serde_json::json!(&xml[..xml.floor_char_boundary(8192)]),
            );
        }
        error
    }

    pub(crate) fn mismatch_error(&self, fields: &[String]) -> kio_core::KioError {
        let message = if fields.is_empty() {
            "missing definition comparison"
        } else {
            "registered task definition differs"
        };
        self.diagnostic(message.to_owned(), fields)
    }

    pub(crate) fn definition_mismatch(&self, enabled: bool) -> Option<&[String]> {
        let mismatch = if enabled {
            &self.enabled_mismatch
        } else {
            &self.disabled_mismatch
        };
        match mismatch.as_deref() {
            Some([]) if self.present => None,
            Some(fields) => Some(fields),
            None => Some(&[]),
        }
    }
}

fn matches_enabled(query: &TaskQuery, enabled: bool) -> bool {
    query.definition_mismatch(enabled).is_none()
}
pub(crate) fn record_mismatch<'a>(
    query: &'a TaskQuery,
    record: &TaskRecord,
) -> Option<&'a [String]> {
    if record
        .pending_enabled
        .is_some_and(|enabled| matches_enabled(query, enabled))
    {
        None
    } else {
        query.definition_mismatch(record.enabled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply() -> TaskQuery {
        TaskQuery {
            hresult: 0,
            phase: None,
            exceptions: Vec::new(),
            lookup_hresult: 0,
            present: true,
            enabled_mismatch: Some(Vec::new()),
            disabled_mismatch: Some(vec!["Task/Settings/Enabled".into()]),
            differing_fields: None,
            registered_xml: None,
            state: 3,
            arguments: None,
        }
    }
    #[test]
    fn windows_scheduler_semantic_comparison_fixtures() {
        if std::env::consts::OS != "windows" {
            return;
        }
        let windows = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap());
        let program = windows.join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let fixtures =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/platform/windows");
        let script =
            "& ([ScriptBlock]::Create([IO.File]::ReadAllText('watch_task_compare_test.ps1'))) '.'";
        let mut command = kio_process::windows_powershell(&program, &windows, script);
        command.current_dir(fixtures);
        let output = kio_process::run_bounded_command(
            &mut command,
            kio_process::BoundedProcessOptions {
                // A cold PowerShell startup on a loaded Windows runner can take
                // longer than the comparison itself.
                timeout: std::time::Duration::from_secs(120),
                max_stdout_bytes: 8192,
                max_stderr_bytes: 8192,
            },
            None,
        )
        .unwrap();
        assert!(output.status.success(), "{}", output.stderr);
    }

    #[test]
    fn reports_bounded_scheduler_exception_chain_with_the_operation_phase() {
        let result = serde_json::from_value(serde_json::json!({
            "hresult": 0x80070005_u32, "phase": "RegisterTaskDefinition",
            "exceptions": [
                {"hresult": 0x80131501_u32, "message": "wrapper"},
                {"hresult": 0x80070005_u32, "message": format!("access denied\n{}", "x".repeat(512))}
            ],
            "lookup_hresult": 0, "present": false,
            "enabled_mismatch": null, "disabled_mismatch": null, "state": 0, "arguments": null
        })).unwrap();
        let error = validate_query(result).err().unwrap().to_string();
        assert!(error.contains("during RegisterTaskDefinition"));
        assert!(error.contains("0x80131501: wrapper"));
        assert!(error.contains("0x80070005: access denied "));
        assert!(!error.contains('\n'));
        assert!(!error.contains(&"x".repeat(385)));
    }

    #[test]
    fn scheduler_mismatch_errors_carry_multiple_fields_and_bounded_xml() {
        let mut result = reply();
        result.registered_xml = Some(format!("{}界suffix", "a".repeat(8191)));
        let fields = vec![
            "Task/Settings/Hidden".to_owned(),
            "Task/Settings/Priority".to_owned(),
        ];
        let error = result.mismatch_error(&fields);
        assert!(
            error
                .message()
                .ends_with("Task/Settings/Hidden, Task/Settings/Priority")
        );
        assert_eq!(
            error.context()["differing_fields"],
            serde_json::json!(fields)
        );
        assert_eq!(
            error.context()["registered_xml"].as_str().unwrap(),
            "a".repeat(8191)
        );
        result.hresult = 0x80131501;
        result.differing_fields = Some(fields.clone());
        let error =
            scheduler_operation_error(validate_query(result).err().unwrap(), "create", "fixture");
        assert!(
            error
                .message()
                .starts_with("Task Scheduler create for fixture")
        );
        assert_eq!(
            error.context()["differing_fields"],
            serde_json::json!(fields)
        );
        assert!(error.context()["registered_xml"].is_string());
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
        result.enabled_mismatch = None;
        result.disabled_mismatch = None;
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
        assert!(record_mismatch(&reply(), &record).is_none());
        let mut foreign = reply();
        foreign.enabled_mismatch = Some(vec!["Task/Actions/Exec/WorkingDirectory".into()]);
        assert!(record_mismatch(&foreign, &record).is_some());
        let mut disabled = reply();
        std::mem::swap(
            &mut disabled.enabled_mismatch,
            &mut disabled.disabled_mismatch,
        );
        assert!(record_mismatch(&disabled, &record).is_some());
        let pending = TaskRecord {
            pending_enabled: Some(false),
            ..record
        };
        assert_eq!(pending.allowed(), ["true", "false"]);
        assert!(record_mismatch(&disabled, &pending).is_none());
        assert!(!matches_enabled(&reply(), false));
        assert!(matches_enabled(&disabled, false));
    }
}
