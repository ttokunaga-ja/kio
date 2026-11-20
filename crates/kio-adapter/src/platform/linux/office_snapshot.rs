use crate::office_runtime_identity::{
    EntryCheck, RecordSnapshot, RuntimeSnapshot, check_verification_deadline, path_error,
};
use crate::platform::EntryIdentity;
use crate::platform::unix::office_snapshot::{LinkWalk, verify_link_target};
use kio_process::confinement::RenderSources;
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Result},
    path::{Path, PathBuf},
    time::Instant,
};

impl RecordSnapshot {
    pub(crate) fn observe(&mut self, path: &Path) -> Result<()> {
        if !self.0.contains_key(path) {
            self.0.insert(path.to_path_buf(), Self::identity(path)?);
        }
        Ok(())
    }
}

impl RuntimeSnapshot {
    pub(crate) fn with_records(
        mut self,
        records: RecordSnapshot,
        deadline: Instant,
    ) -> Result<Self> {
        records.verify(deadline)?;
        self.records = records;
        Ok(self)
    }
}

pub(crate) fn runtime_link_target(
    path: &Path,
    sources: &RenderSources,
    _roots: &[PathBuf],
    check_entry: &EntryCheck<'_>,
    deadline: Instant,
    entries: &mut BTreeMap<PathBuf, EntryIdentity>,
) -> Result<PathBuf> {
    use std::{collections::VecDeque, ffi::OsString, path::Component};

    let outside = |target: &Path| {
        path_error(
            path,
            io::Error::other(format!(
                "runtime link target is outside the renderer sandbox and outside the verified Office runtime: {}",
                target.display()
            )),
        )
    };
    let components = |target: &Path| -> VecDeque<OsString> {
        target
            .components()
            .map(|part| part.as_os_str().to_owned())
            .collect()
    };
    let mut named = path
        .parent()
        .expect("runtime link has a parent")
        .to_path_buf();
    let mut pending = components(&fs::read_link(path).map_err(|error| path_error(path, error))?);
    let mut walk = LinkWalk::new(path, check_entry, entries);
    while let Some(part) = pending.pop_front() {
        check_verification_deadline(deadline)?;
        match Path::new(&part).components().next() {
            Some(Component::RootDir) => named = PathBuf::from("/"),
            Some(Component::ParentDir) => {
                named.pop();
            }
            Some(Component::Normal(_)) => {
                named.push(&part);
                let Some(physical) = sources.sandbox_path_source(&named) else {
                    // Bubblewrap creates these ancestors for file and directory mounts.
                    if sources.contains_sandbox_destination(&named) {
                        continue;
                    }
                    return Err(outside(&named));
                };
                if walk.observe(&physical, !pending.is_empty())? {
                    let mut target = components(
                        &fs::read_link(&physical).map_err(|error| path_error(&physical, error))?,
                    );
                    target.append(&mut pending);
                    pending = target;
                    named.pop();
                }
            }
            _ => {}
        }
    }
    verify_link_target(
        path,
        sources
            .sandbox_path_source(&named)
            .ok_or_else(|| outside(&named))?,
    )
}
