use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, Metadata},
    io::{self, Result},
    path::{Path, PathBuf},
    time::Instant,
};

use kio_core::platform::{RegularFileHandleIdentity, real_regular_file_identity};
use kio_process::confinement::RenderSources;
use sha2::{Digest, Sha256};

use crate::office_convert::hex_digest;

pub(crate) fn check_verification_deadline(deadline: Instant) -> Result<()> {
    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "office converter exceeded its time limit",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExecutableFingerprint {
    pub(crate) digest: String,
    identity: RegularFileHandleIdentity,
}

impl ExecutableFingerprint {
    pub(crate) fn new(program: &Path, deadline: Instant) -> Result<Self> {
        const MAX_RENDERER_PROGRAM_BYTES: u64 = 32 * 1024 * 1024;
        let identity = || {
            real_regular_file_identity(program)
                .map_err(|error| executable_error(&error))?
                .ok_or_else(|| executable_error(&"expected a real, single-link file"))
        };
        check_verification_deadline(deadline)?;
        let before = identity()?;
        let bytes = kio_core::cas::read_bounded_regular_file_until(
            program,
            MAX_RENDERER_PROGRAM_BYTES,
            deadline,
        )
        .map_err(|error| {
            if error.kind() == io::ErrorKind::TimedOut {
                error
            } else {
                executable_error(&error)
            }
        })?;
        if before != identity()? {
            return Err(executable_error(&"executable changed while being read"));
        }
        let mut digest = Sha256::new();
        for chunk in bytes.chunks(64 * 1024) {
            check_verification_deadline(deadline)?;
            digest.update(chunk);
        }
        check_verification_deadline(deadline)?;
        Ok(Self {
            digest: hex_digest(&digest.finalize()),
            identity: before,
        })
    }

    pub(crate) fn verify(&self, program: &Path, stage: &str, deadline: Instant) -> Result<()> {
        if Self::new(program, deadline)? != *self {
            return Err(io::Error::other(format!(
                "office converter executable changed {stage}; resolve a fresh converter before retrying"
            )));
        }
        Ok(())
    }
}

fn executable_error(error: &dyn std::fmt::Display) -> io::Error {
    io::Error::other(format!(
        "cannot safely fingerprint Office renderer executable: {error}"
    ))
}

pub(crate) const MAX_PACKAGE_ENTRIES: usize = 32_768;

/// Identity of the package and the parts of the mounted runtime it links to.
#[derive(Debug, Clone)]
pub(crate) struct RuntimeSnapshot {
    roots: Vec<PathBuf>,
    entries: BTreeMap<PathBuf, EntryIdentity>,
    mounts: Vec<EntryIdentity>,
    excluded: Option<PathBuf>,
    #[cfg(target_os = "linux")]
    records: RecordSnapshot,
}

/// Metadata for the package evidence and its database directories, including
/// optional records whose appearance would change the meaning of the evidence.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Default)]
pub(crate) struct RecordSnapshot(BTreeMap<PathBuf, Option<EntryIdentity>>);

#[cfg(target_os = "linux")]
impl RecordSnapshot {
    pub(crate) fn observe(&mut self, path: &Path) -> Result<()> {
        if !self.0.contains_key(path) {
            self.0.insert(path.to_path_buf(), Self::identity(path)?);
        }
        Ok(())
    }

    pub(crate) fn verify(&self, deadline: Instant) -> Result<()> {
        for (path, before) in &self.0 {
            check_verification_deadline(deadline)?;
            if Self::identity(path)? != *before {
                return Err(path_error(
                    path,
                    io::Error::other("Office package records changed"),
                ));
            }
        }
        Ok(())
    }

    fn identity(path: &Path) -> Result<Option<EntryIdentity>> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => entry_identity(path, &metadata).map(Some),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
}

type EntryCheck<'a> = dyn Fn(&Path, &Metadata) -> Result<()> + 'a;

impl RuntimeSnapshot {
    #[cfg(any(test, not(target_os = "linux")))]
    pub(crate) fn new(
        roots: Vec<PathBuf>,
        sources: &RenderSources,
        deadline: Instant,
    ) -> Result<Self> {
        Self::new_checked(roots, sources, None, &|_, _| Ok(()), deadline)
    }

    pub(crate) fn new_checked(
        roots: Vec<PathBuf>,
        sources: &RenderSources,
        excluded: Option<PathBuf>,
        check_entry: &EntryCheck<'_>,
        deadline: Instant,
    ) -> Result<Self> {
        let mounts = mount_identities(sources, deadline)?;
        let entries = Self::read(&roots, sources, excluded.as_deref(), check_entry, deadline)?;
        Ok(Self {
            roots,
            entries,
            mounts,
            excluded,
            #[cfg(target_os = "linux")]
            records: RecordSnapshot::default(),
        })
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn with_records(
        mut self,
        records: RecordSnapshot,
        deadline: Instant,
    ) -> Result<Self> {
        records.verify(deadline)?;
        self.records = records;
        Ok(self)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn contains_entry(&self, path: &Path) -> bool {
        self.entries.contains_key(path)
    }

    fn read(
        roots: &[PathBuf],
        sources: &RenderSources,
        excluded: Option<&Path>,
        check_entry: &EntryCheck<'_>,
        deadline: Instant,
    ) -> Result<BTreeMap<PathBuf, EntryIdentity>> {
        if roots.is_empty() {
            return Err(io::Error::other(format!(
                "unsupported Office runtime placement for {}: cannot identify its package",
                sources.program().display()
            )));
        }
        let mut entries = BTreeMap::new();
        let mut scanned = HashSet::new();
        let mut pending = roots.to_vec();
        while let Some(path) = pending.pop() {
            check_verification_deadline(deadline)?;
            if !scanned.insert(path.clone()) {
                continue;
            }
            let error = |cause: io::Error| path_error(&path, cause);
            let metadata = fs::symlink_metadata(&path).map_err(error)?;
            if roots.contains(&path) && (!metadata.is_dir() || !in_runtime_sources(&path, sources))
            {
                return Err(error(io::Error::other(
                    "expected a real, admitted Office runtime directory",
                )));
            }
            let identity = entry_identity(&path, &metadata).map_err(error)?;
            check_entry(&path, &metadata)?;
            entries.insert(path.clone(), identity);
            if metadata.is_symlink() {
                let target =
                    runtime_link_target(&path, sources, check_entry, deadline, &mut entries)?;
                if excluded.is_some_and(|excluded| target.starts_with(excluded)) {
                    return Err(error(io::Error::other(
                        "link points outside the verified Office runtime",
                    )));
                }
                pending.push(target);
            } else if metadata.is_dir() {
                for child in fs::read_dir(&path).map_err(error)? {
                    check_verification_deadline(deadline)?;
                    if entries.len() + pending.len() >= MAX_PACKAGE_ENTRIES {
                        return Err(error(io::Error::other("too many Office runtime entries")));
                    }
                    let child = child.map_err(error)?;
                    if excluded == Some(child.path().as_path())
                        && child.file_type().map_err(error)?.is_dir()
                    {
                        continue;
                    }
                    pending.push(child.path());
                }
            }
            if entries.len() + pending.len() > MAX_PACKAGE_ENTRIES {
                return Err(error(io::Error::other("too many Office runtime entries")));
            }
        }
        Ok(entries)
    }

    pub(crate) fn verify(
        &self,
        sources: &RenderSources,
        stage: &str,
        deadline: Instant,
    ) -> Result<()> {
        let changed = || {
            io::Error::other(format!(
                "Office runtime changed {stage}; resolve a fresh converter before retrying"
            ))
        };
        let check_entry = |path: &Path, metadata: &Metadata| {
            if self.entries.get(path) != Some(&entry_identity(path, metadata)?) {
                return Err(changed());
            }
            Ok(())
        };
        #[cfg(target_os = "linux")]
        if sources.masked_runtime_directory() != self.excluded.as_deref() {
            return Err(changed());
        }
        if mount_identities(sources, deadline)? != self.mounts
            || Self::read(
                &self.roots,
                sources,
                self.excluded.as_deref(),
                &check_entry,
                deadline,
            )? != self.entries
        {
            return Err(changed());
        }
        #[cfg(target_os = "linux")]
        self.records.verify(deadline)?;
        check_verification_deadline(deadline)
    }
}

fn path_error(path: &Path, cause: io::Error) -> io::Error {
    io::Error::other(format!(
        "cannot verify Office runtime path {}: {cause}",
        path.display()
    ))
}

fn mount_identities(sources: &RenderSources, deadline: Instant) -> Result<Vec<EntryIdentity>> {
    sources
        .roots()
        .iter()
        .map(|(destination, source)| {
            check_verification_deadline(deadline)?;
            // Inspect the resolved path the native backend uses. Refuse an alias
            // that now points to a different source, even if both paths exist.
            let current =
                fs::canonicalize(destination).map_err(|error| path_error(destination, error))?;
            if current != *source {
                return Err(path_error(
                    destination,
                    io::Error::other("runtime mount source changed"),
                ));
            }
            let metadata =
                fs::symlink_metadata(source).map_err(|error| path_error(source, error))?;
            entry_identity(source, &metadata).map_err(|error| path_error(source, error))
        })
        .collect()
}

fn in_runtime_sources(path: &Path, sources: &RenderSources) -> bool {
    path == sources.program()
        || sources
            .roots()
            .iter()
            .any(|(_, source)| path.starts_with(source))
}

#[cfg(target_os = "linux")]
fn runtime_link_target(
    path: &Path,
    sources: &RenderSources,
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
    let mounted = |named: &Path| {
        if named == sources.program() {
            return Some(named.to_path_buf());
        }
        sources
            .roots()
            .iter()
            .filter_map(|(destination, source)| {
                named
                    .strip_prefix(destination)
                    .ok()
                    .map(|suffix| (destination, source, suffix))
            })
            .max_by_key(|(destination, _, _)| destination.as_os_str().len())
            .map(|(_, source, suffix)| {
                if suffix.as_os_str().is_empty() {
                    source.clone()
                } else {
                    source.join(suffix)
                }
            })
    };

    let mut named = path
        .parent()
        .expect("runtime link has a parent")
        .to_path_buf();
    let mut pending = components(&fs::read_link(path).map_err(|error| path_error(path, error))?);
    let mut hops = 0;
    while let Some(part) = pending.pop_front() {
        check_verification_deadline(deadline)?;
        match Path::new(&part).components().next() {
            Some(Component::RootDir) => named = PathBuf::from("/"),
            Some(Component::ParentDir) => {
                named.pop();
            }
            Some(Component::Normal(_)) => {
                named.push(&part);
                let Some(physical) = mounted(&named) else {
                    // Bubblewrap creates these ancestors for file and directory mounts.
                    if sources.program().starts_with(&named)
                        || sources
                            .roots()
                            .iter()
                            .any(|(destination, _)| destination.starts_with(&named))
                    {
                        continue;
                    }
                    return Err(outside(&named));
                };
                let metadata = fs::symlink_metadata(&physical)
                    .map_err(|error| path_error(&physical, error))?;
                if metadata.is_symlink() {
                    hops += 1;
                    if hops >= 40 {
                        return Err(path_error(
                            path,
                            io::Error::other("too many Office runtime link hops"),
                        ));
                    }
                    check_entry(&physical, &metadata)?;
                    entries.insert(physical.clone(), entry_identity(&physical, &metadata)?);
                    let mut target = components(
                        &fs::read_link(&physical).map_err(|error| path_error(&physical, error))?,
                    );
                    target.append(&mut pending);
                    pending = target;
                    named.pop();
                } else if !metadata.is_dir() && !pending.is_empty() {
                    return Err(path_error(
                        &physical,
                        io::Error::other("expected an Office runtime directory"),
                    ));
                }
            }
            _ => {}
        }
    }
    verify_link_target(path, mounted(&named).ok_or_else(|| outside(&named))?)
}

#[cfg(not(target_os = "linux"))]
fn runtime_link_target(
    path: &Path,
    sources: &RenderSources,
    check_entry: &EntryCheck<'_>,
    deadline: Instant,
    entries: &mut BTreeMap<PathBuf, EntryIdentity>,
) -> Result<PathBuf> {
    let read_link = |link: &Path| fs::read_link(link).map_err(|error| path_error(link, error));
    let parent = |link: &Path| link.parent().unwrap_or(Path::new("/")).to_path_buf();
    let mut named = normalize_link_path(&parent(path).join(read_link(path)?));
    // Follow each link in the namespace the renderer sees and record
    // intermediate symlinks, even when they live in another mounted package.
    'resolve: for _ in 0..40 {
        check_verification_deadline(deadline)?;
        if named == sources.program() {
            return verify_link_target(path, named);
        }
        let mount = sources
            .roots()
            .iter()
            .filter_map(|(_, source)| named.starts_with(source).then_some((source, source)))
            .max_by_key(|(visible, _)| visible.as_os_str().len());
        let Some((visible, source)) = mount else {
            return Err(path_error(
                path,
                io::Error::other(format!(
                    "runtime link target is outside the renderer sandbox: {}",
                    named.display()
                )),
            ));
        };
        let suffix = named.strip_prefix(visible).expect("matched mount prefix");
        let mut components = suffix.components();
        let mut physical = source.clone();
        let mut logical = visible.clone();
        while let Some(component) = components.next() {
            check_verification_deadline(deadline)?;
            physical.push(component.as_os_str());
            logical.push(component.as_os_str());
            let metadata =
                fs::symlink_metadata(&physical).map_err(|error| path_error(&physical, error))?;
            if metadata.is_symlink() {
                check_entry(&physical, &metadata)?;
                let identity = entry_identity(&physical, &metadata)
                    .map_err(|error| path_error(&physical, error))?;
                entries.insert(physical.clone(), identity);
                named = normalize_link_path(
                    &parent(&logical)
                        .join(read_link(&physical)?)
                        .join(components.as_path()),
                );
                continue 'resolve;
            }
        }
        return verify_link_target(path, physical);
    }
    Err(path_error(
        path,
        io::Error::other("too many Office runtime link hops"),
    ))
}

fn verify_link_target(path: &Path, physical: PathBuf) -> Result<PathBuf> {
    // Reject paths that have different targets on the host and in the
    // sandbox, for example when traversing a separate mount alias.
    let actual = fs::canonicalize(path).map_err(|error| path_error(path, error))?;
    if actual != physical {
        return Err(path_error(
            path,
            io::Error::other("runtime link resolves differently in the renderer sandbox"),
        ));
    }
    Ok(physical)
}

#[cfg(not(target_os = "linux"))]
fn normalize_link_path(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut normalized = PathBuf::new();
    for component in path.components() {
        if component == Component::ParentDir {
            normalized.pop();
        } else {
            normalized.push(component);
        }
    }
    normalized
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct EntryIdentity {
    device: u64,
    inode: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    len: u64,
    modification: (i64, i64),
    change: (i64, i64),
}

#[cfg(unix)]
fn entry_identity(_path: &Path, metadata: &Metadata) -> io::Result<EntryIdentity> {
    use std::os::unix::fs::MetadataExt;
    let kind = metadata.file_type();
    if !(kind.is_file() || kind.is_dir() || kind.is_symlink()) {
        return Err(io::Error::other("unexpected Office runtime file type"));
    }
    Ok(EntryIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        mode: metadata.mode(),
        uid: metadata.uid(),
        gid: metadata.gid(),
        len: metadata.len(),
        modification: (metadata.mtime(), metadata.mtime_nsec()),
        change: (metadata.ctime(), metadata.ctime_nsec()),
    })
}

#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct EntryIdentity {
    information: kio_core::platform::FileInformation,
    changed: i64,
}

#[cfg(windows)]
fn entry_identity(path: &Path, metadata: &Metadata) -> io::Result<EntryIdentity> {
    use kio_core::platform::{file_change_time, file_information, open_metadata_nofollow};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    };

    let file = open_metadata_nofollow(path)?;
    let information = file_information(&file)?;
    if information.file_attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (information.file_attributes & FILE_ATTRIBUTE_DIRECTORY != 0) != metadata.is_dir()
    {
        return Err(io::Error::other(
            "Office runtime path is an alias or changed type",
        ));
    }
    Ok(EntryIdentity {
        information,
        changed: file_change_time(&file)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(10)
    }

    #[test]
    fn runtime_snapshot_rejects_unknown_package() {
        let exe = std::env::current_exe().unwrap();
        let sources = RenderSources::resolve(&exe, []).unwrap();
        assert!(RuntimeSnapshot::new(vec![], &sources, deadline()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn runtime_snapshot_rejects_replacement_of_a_link_target() {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path().canonicalize().unwrap();
        let package = root.join("package");
        fs::create_dir(&package).unwrap();
        let resource = root.join("resource");
        fs::write(&resource, b"original").unwrap();
        std::os::unix::fs::symlink(&resource, package.join("link")).unwrap();
        let exe = std::env::current_exe().unwrap();
        let sources = RenderSources::resolve(&exe, [package.clone(), resource.clone()]).unwrap();
        let snapshot = RuntimeSnapshot::new(vec![package], &sources, deadline()).unwrap();

        fs::write(&resource, b"updated resource").unwrap();
        assert!(
            snapshot
                .verify(&sources, "before conversion", deadline())
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn runtime_snapshot_rejects_link_outside_existing_mounts() {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path().canonicalize().unwrap();
        let package = root.join("package");
        fs::create_dir(&package).unwrap();
        let resource = root.join("outside");
        fs::write(&resource, b"original").unwrap();
        std::os::unix::fs::symlink(&resource, package.join("link")).unwrap();
        let exe = std::env::current_exe().unwrap();
        let sources = RenderSources::resolve(&exe, [package.clone()]).unwrap();
        let error = RuntimeSnapshot::new(vec![package], &sources, deadline()).unwrap_err();
        assert!(
            error.to_string().contains("outside the renderer sandbox"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn runtime_snapshot_rejects_alias_chain_that_leaves_and_reenters_the_sandbox() {
        use std::os::unix::fs::symlink;
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path().canonicalize().unwrap();
        let package = root.join("package");
        let shared = root.join("shared");
        let outside = root.join("outside");
        for directory in [&package, &shared, &outside] {
            fs::create_dir(directory).unwrap();
        }
        fs::write(shared.join("resource"), b"runtime").unwrap();
        symlink(shared.join("resource"), outside.join("back")).unwrap();
        symlink(outside.join("back"), shared.join("bridge")).unwrap();
        symlink(shared.join("bridge"), package.join("link")).unwrap();
        let exe = std::env::current_exe().unwrap();
        let sources = RenderSources::resolve(&exe, [package.clone(), shared]).unwrap();

        let error = RuntimeSnapshot::new(vec![package], &sources, deadline()).unwrap_err();
        assert!(
            error.to_string().contains("outside the renderer sandbox"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn runtime_snapshot_rejects_mismatched_mount_source_after_alias_is_restored() {
        use std::os::unix::fs::symlink;
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path().canonicalize().unwrap();
        let package = root.join("package");
        let other = root.join("other");
        let alias = root.join("alias");
        fs::create_dir(&package).unwrap();
        fs::create_dir(&other).unwrap();
        symlink(&package, &alias).unwrap();
        let exe = std::env::current_exe().unwrap();
        let original = RenderSources::resolve(&exe, [alias.clone()]).unwrap();
        let snapshot = RuntimeSnapshot::new(vec![package.clone()], &original, deadline()).unwrap();
        fs::remove_file(&alias).unwrap();
        symlink(&other, &alias).unwrap();
        let substituted = RenderSources::resolve(&exe, [alias.clone()]).unwrap();
        fs::remove_file(&alias).unwrap();
        symlink(&package, &alias).unwrap();

        snapshot.verify(&original, "control", deadline()).unwrap();
        let error = snapshot
            .verify(&substituted, "before launch", deadline())
            .unwrap_err();
        assert!(
            error.to_string().contains("mount source changed"),
            "{error}"
        );
    }

    #[test]
    fn runtime_snapshot_rejects_replacement_of_a_runtime_file() {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path().canonicalize().unwrap();
        let runtime = root.join("package");
        fs::create_dir(&runtime).unwrap();
        let component = runtime.join("component");
        let replacement = root.join("replacement");
        fs::write(&component, b"runtime component").unwrap();
        fs::write(&replacement, b"runtime component").unwrap();
        let exe = std::env::current_exe().unwrap();
        let sources = RenderSources::resolve(&exe, [runtime.clone()]).unwrap();
        let snapshot = RuntimeSnapshot::new(vec![runtime], &sources, deadline()).unwrap();
        snapshot
            .verify(&sources, "before conversion", deadline())
            .unwrap();

        fs::remove_file(&component).unwrap();
        fs::rename(&replacement, &component).unwrap();
        let error = snapshot
            .verify(&sources, "before conversion", deadline())
            .unwrap_err();
        assert!(error.to_string().contains("runtime changed"), "{error}");
    }
}
