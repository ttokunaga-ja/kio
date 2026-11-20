use crate::watch_service::invalid;
use kio_core::Result;

pub(crate) fn xml_escape(value: &str) -> Result<String> {
    if value
        .chars()
        .any(|c| c == '\0' || c == '\r' || c == '\n' || (c.is_control() && c != '\t'))
    {
        return Err(invalid(
            "native scheduler value contains a control character",
        ));
    }
    Ok(value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\"', "&quot;")
        .replace('\'', "&apos;"))
}

/// Both strings come from the same Task Scheduler XML serializer. Compare
/// the complete definition, including principals, triggers, actions, settings
/// and extensions. There is no field whitelist or ignored execution option.
pub(crate) fn task_definition_matches(actual: &str, expected: &str) -> bool {
    actual == expected
}

pub(crate) fn task_lookup(hresult: u32) -> Result<bool> {
    match hresult {
        0 => Ok(true),
        0x80070002 | 0x80070003 => Ok(false),
        _ => Err(crate::watch_service::service_error(format!(
            "Task Scheduler HRESULT 0x{hresult:08x}"
        ))),
    }
}

pub(crate) fn manifest_anchor_from_cache(
    id: &str,
    cache: &std::path::Path,
) -> Result<std::path::PathBuf> {
    let parent = cache
        .parent()
        .ok_or_else(|| invalid("native task cache has no parent"))?;
    if parent.file_name() != Some(std::ffi::OsStr::new("watch-services-native"))
        || cache.file_name() != Some(std::ffi::OsStr::new(&format!("{id}.xml")))
    {
        return Err(invalid(
            "native task cache does not bind the service identity",
        ));
    }
    let root = parent
        .parent()
        .ok_or_else(|| invalid("native task cache has no private data directory"))?;
    let path = root.join(crate::watch_service::manifest_leaf(id)?);
    crate::watch_service::validate_argument_path(&path)?;
    Ok(path)
}

pub(crate) fn manifest_from_arguments(arguments: &str) -> Result<std::path::PathBuf> {
    let value = arguments
        .strip_prefix("\"watch\" \"service\" \"runner\" \"--manifest\" ")
        .and_then(|value| value.strip_prefix('\"'))
        .and_then(|value| value.strip_suffix('\"'))
        .ok_or_else(|| invalid("foreign runner arguments"))?;
    let mut decoded = String::new();
    let mut slashes = 0;
    for ch in value.chars() {
        if ch == '\\' {
            slashes += 1;
            continue;
        }
        if ch == '\"' {
            if slashes % 2 == 0 {
                return Err(invalid("ambiguous runner manifest quoting"));
            }
            decoded.push_str(&"\\".repeat(slashes / 2));
            decoded.push(ch);
        } else {
            decoded.push_str(&"\\".repeat(slashes));
            decoded.push(ch);
        }
        slashes = 0;
    }
    if slashes % 2 != 0 {
        return Err(invalid("ambiguous runner manifest suffix"));
    }
    decoded.push_str(&"\\".repeat(slashes / 2));
    let path = std::path::PathBuf::from(decoded);
    crate::watch_service::validate_argument_path(&path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_anchor_is_independent_of_the_watcher_environment() {
        let root = std::env::current_dir().unwrap().join("old-data/kio");
        let id = "io.kio.watch.abc";
        let cache = root.join("watch-services-native").join(format!("{id}.xml"));
        assert_eq!(
            manifest_anchor_from_cache(id, &cache).unwrap(),
            root.join("watch-services").join(format!("{id}.json"))
        );
        assert!(manifest_anchor_from_cache("io.kio.watch.foreign", &cache).is_err());
    }

    #[test]
    fn registered_manifest_arguments_round_trip_quoted_paths() {
        let root = std::env::current_dir().unwrap();
        let path = root.join("data with space").join("quote\"file.json");

        // Backslashes before ordinary characters remain single under the
        // native argv convention; only quote/trailing runs are doubled.
        let quoted = format!(
            "\"watch\" \"service\" \"runner\" \"--manifest\" \"{}\"",
            path.to_string_lossy().replace('\"', "\\\"")
        );
        assert_eq!(manifest_from_arguments(&quoted).unwrap(), path);
        assert!(manifest_from_arguments("foreign --manifest bad").is_err());
    }

    #[test]
    fn review3_os_only_task_changes_are_foreign_even_when_actions_match() {
        let expected = "<Task><RegistrationInfo><URI>\\io.kio.watch.fixture</URI></RegistrationInfo><Triggers><LogonTrigger/></Triggers><Principal><UserId>S-1-5-21-100-200-300-1001</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal><Actions><Exec><Command>kio</Command><Arguments>runner</Arguments></Exec></Actions></Task>";
        assert!(task_definition_matches(expected, expected));
        for foreign in [
            expected.replace(
                "</Exec>",
                "<WorkingDirectory>C:\\foreign</WorkingDirectory></Exec>",
            ),
            expected.replace("<LogonTrigger/>", "<TimeTrigger/>"),
            expected.replace("S-1-5-21-100-200-300-1001", "S-1-5-21-100-200-300-1002"),
        ] {
            assert!(!task_definition_matches(&foreign, expected));
        }
    }

    #[test]
    fn review5_task_absence_uses_hresult_in_every_language() {
        // The lookup protocol contains only the GetTask HRESULT; localized
        // diagnostic text has no input path into the absence decision.
        for hresult in [0x80070002, 0x80070003] {
            assert!(!task_lookup(hresult).unwrap());
        }
        assert!(task_lookup(0x80070005).is_err());
        assert!(task_lookup(0).unwrap());
    }
}
