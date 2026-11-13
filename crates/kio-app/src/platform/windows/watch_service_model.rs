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

pub(crate) fn task_action_matches(xml: &str, binary: &str, arguments: &str) -> Result<bool> {
    Ok(xml.matches("<Exec>").count() == 1
        && xml.matches("<Command>").count() == 1
        && xml.matches("<Arguments>").count() == 1
        && !["<ComHandler", "<SendEmail", "<ShowMessage"]
            .iter()
            .any(|tag| xml.contains(tag))
        && xml.contains(&format!("<Command>{}</Command>", xml_escape(binary)?))
        && xml.contains(&format!(
            "<Arguments>{}</Arguments>",
            xml_escape(arguments)?
        )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_action_rejects_prefix_matches_and_additional_actions() {
        let action = "<Exec><Command>C:\\kio.exe</Command><Arguments>watch service runner</Arguments></Exec>";
        assert!(task_action_matches(action, "C:\\kio.exe", "watch service runner").unwrap());
        for foreign in [
            action.replace("kio.exe", "kio.exe-foreign"),
            action.replace("service runner", "service runner foreign"),
            format!("{action}{action}"),
            format!("{action}<ComHandler><ClassId>foreign</ClassId></ComHandler>"),
        ] {
            assert!(!task_action_matches(&foreign, "C:\\kio.exe", "watch service runner").unwrap());
        }
    }
}
