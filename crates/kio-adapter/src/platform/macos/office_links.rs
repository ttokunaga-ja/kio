use crate::office_runtime_identity::{EntryCheck, check_verification_deadline, path_error};
use crate::platform::EntryIdentity;
use crate::platform::unix::office_snapshot::{LinkWalk, verify_link_target};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Result},
    path::{Path, PathBuf},
    time::Instant,
};

pub(crate) fn macos_runtime_link_target(
    path: &Path,
    package_roots: &[PathBuf],
    check_entry: &EntryCheck<'_>,
    deadline: Instant,
    entries: &mut BTreeMap<PathBuf, EntryIdentity>,
) -> Result<PathBuf> {
    use std::{collections::VecDeque, ffi::OsString, path::Component};

    let outside = || {
        path_error(
            path,
            io::Error::other("runtime link target is outside the renderer sandbox package"),
        )
    };
    let read_target = |link: &Path| -> Result<(PathBuf, VecDeque<OsString>)> {
        let target = fs::read_link(link).map_err(|error| path_error(link, error))?;
        let (base, suffix) = if target.is_absolute() {
            let root = package_roots
                .iter()
                .find(|root| target.starts_with(root))
                .ok_or_else(outside)?;
            (
                root.clone(),
                target.strip_prefix(root).expect("matched package prefix"),
            )
        } else {
            (
                link.parent()
                    .expect("package link has a parent")
                    .to_path_buf(),
                target.as_path(),
            )
        };
        let pending = suffix
            .components()
            .map(|part| part.as_os_str().to_owned())
            .collect();
        Ok((base, pending))
    };

    // Walk each hop without collapsing parents before resolving symlinks.
    // The OS's separate sandbox roots do not extend the sealed bundle.
    let (mut physical, mut pending) = read_target(path)?;
    let mut walk = LinkWalk::new(path, check_entry, entries);
    while let Some(part) = pending.pop_front() {
        check_verification_deadline(deadline)?;
        if Path::new(&part).components().next() == Some(Component::ParentDir) {
            physical.pop();
            if !package_roots.iter().any(|root| physical.starts_with(root)) {
                return Err(outside());
            }
            continue;
        }
        physical.push(part);
        if walk.observe(&physical, !pending.is_empty())? {
            let (base, mut target) = read_target(&physical)?;
            physical = base;
            target.append(&mut pending);
            pending = target;
        }
    }
    verify_link_target(path, physical)
}
