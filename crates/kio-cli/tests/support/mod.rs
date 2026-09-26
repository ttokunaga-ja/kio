//! Shared temporary roots for CLI fixtures with private device state.

pub fn canonical_tempdir() -> tempfile::TempDir {
    // Resolve the system root before creation: macOS /var is a symlink.
    // Preserve each fixture's child paths, including deliberate symlink targets.
    tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap()
}
