use super::watch_task_definition::{task_definition_matches, task_lookup};
use crate::watch_service::service_error;
use kio_core::Result;
use serde::{Deserialize, Serialize};

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
