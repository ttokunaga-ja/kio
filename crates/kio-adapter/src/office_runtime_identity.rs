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
use crate::platform::{EntryIdentity, entry_identity, runtime_link_target};

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
    pub(crate) entries: BTreeMap<PathBuf, EntryIdentity>,
    mounts: Vec<EntryIdentity>,
    excluded: Option<PathBuf>,
    pub(crate) records: RecordSnapshot,
}

/// Metadata for the package evidence and its database directories, including
/// optional records whose appearance would change the meaning of the evidence.
#[derive(Debug, Clone, Default)]
pub(crate) struct RecordSnapshot(pub(crate) BTreeMap<PathBuf, Option<EntryIdentity>>);

impl RecordSnapshot {
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

    pub(crate) fn identity(path: &Path) -> Result<Option<EntryIdentity>> {
        optional_metadata(path)?
            .map(|metadata| entry_identity(path, &metadata))
            .transpose()
    }
}

pub(crate) fn optional_metadata(path: &Path) -> io::Result<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) type EntryCheck<'a> = dyn Fn(&Path, &Metadata) -> Result<()> + 'a;

impl RuntimeSnapshot {
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
            records: RecordSnapshot::default(),
        })
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
                let target = runtime_link_target(
                    &path,
                    sources,
                    roots,
                    check_entry,
                    deadline,
                    &mut entries,
                )?;
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
        self.records.verify(deadline)?;
        check_verification_deadline(deadline)
    }
}

pub(crate) fn path_error(path: &Path, cause: io::Error) -> io::Error {
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

    #[test]
    fn runtime_snapshot_includes_uno_service_catalogs_and_bootstrap_configuration() {
        let scratch = tempfile::tempdir().unwrap();
        let package = scratch.path().canonicalize().unwrap().join("package");
        fs::create_dir_all(package.join("program/services")).unwrap();
        fs::create_dir_all(package.join("share/uno")).unwrap();
        let program = package.join("program/renderer");
        fs::write(&program, b"renderer").unwrap();
        let sources = RenderSources::resolve(&program, [package.clone()]).unwrap();
        for relative in [
            "program/fundamental.ini",
            "program/services/services.rdb",
            "share/uno/services.rdb",
        ] {
            let configuration = package.join(relative);
            fs::write(&configuration, b"original configuration").unwrap();
            let snapshot =
                RuntimeSnapshot::new(vec![package.clone()], &sources, deadline()).unwrap();
            fs::write(
                &configuration,
                b"modified configuration with a different size",
            )
            .unwrap();
            assert!(
                snapshot
                    .verify(&sources, "before launch", deadline())
                    .is_err(),
                "{relative}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_runtime_rejects_a_junction_to_an_external_service_directory() {
        let scratch = tempfile::tempdir().unwrap();
        let package = scratch.path().join("package");
        let outside = scratch.path().join("outside");
        fs::create_dir_all(package.join("share")).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("services.rdb"), b"external settings").unwrap();
        // mklink treats a forward slash in this argument as a switch.
        let junction = package.join("share").join("services");
        let result = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&outside)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let package = package.canonicalize().unwrap();
        let program = package.join("renderer.exe");
        fs::write(&program, b"renderer").unwrap();
        let sources = RenderSources::resolve(&program, [package.clone()]).unwrap();
        let error = RuntimeSnapshot::new(vec![package], &sources, deadline()).unwrap_err();
        fs::remove_dir(junction).unwrap();
        assert!(
            error.to_string().contains("alias") || error.to_string().contains("reparse"),
            "{error}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_runtime_rejects_files_with_an_alias_outside_the_package() {
        let scratch = tempfile::tempdir().unwrap();
        let base = scratch.path().canonicalize().unwrap();
        let package = base.join("package");
        fs::create_dir(&package).unwrap();
        let program = package.join("renderer.exe");
        let library = package.join("library.dll");
        fs::write(&program, b"renderer").unwrap();
        fs::write(&library, b"library").unwrap();
        fs::hard_link(library, base.join("outside.dll")).unwrap();
        let sources = RenderSources::resolve(&program, [package.clone()]).unwrap();
        assert!(RuntimeSnapshot::new(vec![package], &sources, deadline()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn runtime_snapshot_rejects_replacement_of_a_link_target() {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path().canonicalize().unwrap();
        let package = root.join("package");
        fs::create_dir(&package).unwrap();
        #[cfg(target_os = "linux")]
        let resource = root.join("resource");
        #[cfg(target_os = "macos")]
        let resource = package.join("resource");
        fs::write(&resource, b"original").unwrap();
        std::os::unix::fs::symlink(&resource, package.join("link")).unwrap();
        let exe = std::env::current_exe().unwrap();
        #[cfg(target_os = "linux")]
        let sources = RenderSources::resolve(&exe, [package.clone(), resource.clone()]).unwrap();
        #[cfg(target_os = "macos")]
        let sources = RenderSources::resolve(&exe, [package.clone()]).unwrap();
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
    fn macos_bundle_links_stay_inside_the_package_at_every_hop() {
        use std::os::unix::fs::symlink;

        let scratch = tempfile::tempdir().unwrap();
        let base = scratch.path().canonicalize().unwrap();
        let bundle = base.join("Office.app");
        let os = base.join("os");
        fs::create_dir(&os).unwrap();
        fs::create_dir_all(bundle.join("Versions/A")).unwrap();
        let library = bundle.join("Versions/A/library");
        fs::write(&library, b"library").unwrap();
        let link = bundle.join("library");
        symlink("A", bundle.join("Versions/Current")).unwrap();
        symlink("Versions/Current/library", &link).unwrap();
        let resolve = || {
            crate::platform::macos_runtime_link_target(
                &link,
                std::slice::from_ref(&bundle),
                &|_, _| Ok(()),
                deadline(),
                &mut BTreeMap::new(),
            )
        };
        assert_eq!(resolve().unwrap(), library);

        symlink(&library, os.join("library")).unwrap();
        fs::remove_file(&link).unwrap();
        symlink("../os/library", &link).unwrap();
        assert_eq!(link.canonicalize().unwrap(), library);
        assert!(resolve().unwrap_err().to_string().contains("outside"));
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

    #[test]
    fn signature_verification_is_bound_to_the_captured_runtime() {
        let scratch = tempfile::tempdir().unwrap();
        let package = scratch.path().canonicalize().unwrap().join("package");
        fs::create_dir(&package).unwrap();
        let program = package.join("renderer");
        let library = package.join("library");
        fs::write(&program, b"renderer").unwrap();
        fs::write(&library, b"library").unwrap();
        let sources = RenderSources::resolve(&program, [package.clone()]).unwrap();
        let snapshot = RuntimeSnapshot::new(vec![package], &sources, deadline()).unwrap();
        snapshot
            .verify_signatures(&sources, deadline(), || Ok(()))
            .unwrap();
        assert!(
            snapshot
                .verify_signatures(&sources, deadline(), || Err(io::Error::other("unsigned")))
                .unwrap_err()
                .to_string()
                .contains("unsigned")
        );
        let error = snapshot
            .verify_signatures(&sources, deadline(), || {
                fs::write(&library, b"replacement after signature check")
            })
            .unwrap_err();
        assert!(error.to_string().contains("runtime changed"), "{error}");
    }
}
