use crate::office_runtime_identity::{EntryCheck, RuntimeSnapshot};
use crate::platform::EntryIdentity;
use kio_process::confinement::RenderSources;
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    time::Instant,
};

pub(crate) fn runtime_link_target(
    path: &Path,
    _sources: &RenderSources,
    roots: &[PathBuf],
    check_entry: &EntryCheck<'_>,
    deadline: Instant,
    entries: &mut BTreeMap<PathBuf, EntryIdentity>,
) -> io::Result<PathBuf> {
    crate::platform::macos_runtime_link_target(path, roots, check_entry, deadline, entries)
}

impl RuntimeSnapshot {
    pub(crate) fn macos_code_resources(&self) -> impl Iterator<Item = &Path> {
        self.entries
            .keys()
            .map(PathBuf::as_path)
            .filter(|path| path.ends_with("_CodeSignature/CodeResources"))
    }
}
