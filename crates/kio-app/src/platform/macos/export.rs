use std::path::Path;
pub(crate) fn is_platform_root_alias(path: &Path) -> bool {
    matches!(path.to_str(), Some("/var" | "/tmp" | "/etc"))
        && path
            .canonicalize()
            .is_ok_and(|canonical| canonical.starts_with("/private"))
}
