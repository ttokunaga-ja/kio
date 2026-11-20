//! Complete systemd source comparison, shared with the scheduler fixtures.
use std::path::Path;

pub(crate) fn unit_definition_matches(observed: &str, path: &Path, definition: &str) -> bool {
    let expected = format!("# {}\n{}", path.display(), definition);
    observed.trim_end_matches('\n') == expected.trim_end_matches('\n')
}
