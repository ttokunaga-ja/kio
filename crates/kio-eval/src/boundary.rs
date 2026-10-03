//! Capability-bound filesystem roots for evaluator subprocesses.
//!
//! Public paths are retained only for diagnostics.  Filesystem authority comes
//! from the held directory handles, so replacing a visible corpus path after
//! binding cannot redirect a later relative lookup.

use std::{
    collections::HashSet,
    env,
    ffi::OsString,
    fs, io,
    path::{Component, Path, PathBuf},
};

use cap_primitives::fs as cap_fs;
use thiserror::Error;

#[cfg(unix)]
use crate::process_boundary::configure_retained_cwd;

const DEVICE_DIR: &str = ".kio-eval-device";
const DEVICE_SUBDIRS: [&str; 6] = ["home", "config", "cache", "data", "state", "runtime"];

pub type BoundaryResult<T> = Result<T, BoundaryError>;

/// An in-process identity for a real directory.
///
/// The Windows representation deliberately remains opaque: it is only valid
/// for comparing a public path with the handle retained for this operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirectoryIdentity {
    #[cfg(unix)]
    Unix { dev: u64, ino: u64 },
    #[cfg(windows)]
    Windows(kio_core::cas::WindowsDirectoryIdentity),
}

#[derive(Debug, Error)]
#[error("unsafe evaluator corpus boundary at {path}: {message}")]
pub struct BoundaryError {
    path: PathBuf,
    message: String,
}

impl BoundaryError {
    fn new(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }

    fn io(path: impl Into<PathBuf>, error: io::Error) -> Self {
        Self::new(path, error.to_string())
    }
}

/// A fixed evaluator corpus and only the explicitly declared scope roots.
#[derive(Debug)]
pub struct BoundCorpus {
    public_path: PathBuf,
    root: fs::File,
    scopes: Vec<BoundScope>,
    device: BoundDevice,
}

impl BoundCorpus {
    /// Canonicalize and bind `corpus_dir`, then bind exactly `scope_names`.
    ///
    /// Scope names must be single normal path components.  They are not
    /// discovered from the filesystem.
    pub fn bind(corpus_dir: &Path, scope_names: &[String]) -> BoundaryResult<Self> {
        let public_path =
            fs::canonicalize(corpus_dir).map_err(|error| BoundaryError::io(corpus_dir, error))?;
        let root = open_canonical_directory(&public_path)?;
        let mut seen = HashSet::new();
        let mut scopes = Vec::with_capacity(scope_names.len());
        for scope_name in scope_names {
            validate_component(scope_name, "corpus scope name")?;
            if !seen.insert(scope_name) {
                return Err(BoundaryError::new(
                    &public_path,
                    "duplicate corpus scope name",
                ));
            }
            let public_scope_path = public_path.join(scope_name);
            let scope = open_child_dir(&root, scope_name, &public_scope_path)?;
            let public_kio_path = public_scope_path.join(".kio");
            let kio = open_child_dir(&scope, ".kio", &public_kio_path)?;
            scopes.push(BoundScope::new(
                scope_name.clone(),
                public_scope_path,
                scope,
                public_kio_path,
                kio,
            )?);
        }
        let device = BoundDevice::create(&root, &public_path)?;
        Ok(Self {
            public_path,
            root,
            scopes,
            device,
        })
    }

    #[must_use]
    pub fn public_path(&self) -> &Path {
        &self.public_path
    }

    /// A cloned capability handle for cap-relative corpus operations.
    pub fn try_clone_handle(&self) -> BoundaryResult<fs::File> {
        self.root
            .try_clone()
            .map_err(|error| BoundaryError::io(&self.public_path, error))
    }

    #[must_use]
    pub fn scopes(&self) -> &[BoundScope] {
        &self.scopes
    }

    #[must_use]
    pub fn scope(&self, name: &str) -> Option<&BoundScope> {
        self.scopes.iter().find(|scope| scope.name == name)
    }

    #[must_use]
    pub fn device(&self) -> &BoundDevice {
        &self.device
    }
}

/// One declared scope together with its already-bound `.kio` directory.
#[derive(Debug)]
pub struct BoundScope {
    name: String,
    public_path: PathBuf,
    handle: fs::File,
    public_kio_path: PathBuf,
    kio: fs::File,
    #[cfg(unix)]
    runner_cwd: fs::File,
}

impl BoundScope {
    fn new(
        name: String,
        public_path: PathBuf,
        handle: fs::File,
        public_kio_path: PathBuf,
        kio: fs::File,
    ) -> BoundaryResult<Self> {
        #[cfg(unix)]
        let runner_cwd = open_runner_cwd(&handle, &public_path)?;
        Ok(Self {
            name,
            public_path,
            handle,
            public_kio_path,
            kio,
            #[cfg(unix)]
            runner_cwd,
        })
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
    #[must_use]
    pub fn public_path(&self) -> &Path {
        &self.public_path
    }
    #[must_use]
    pub fn public_kio_path(&self) -> &Path {
        &self.public_kio_path
    }

    pub fn try_clone_handle(&self) -> BoundaryResult<fs::File> {
        self.handle
            .try_clone()
            .map_err(|error| BoundaryError::io(&self.public_path, error))
    }

    pub fn try_clone_kio_handle(&self) -> BoundaryResult<fs::File> {
        self.kio
            .try_clone()
            .map_err(|error| BoundaryError::io(&self.public_kio_path, error))
    }

    /// Configure a subprocess to use the retained scope directory as its cwd.
    ///
    /// On Unix this performs `fchdir` in the child immediately before exec;
    /// the parent process cwd is never changed. On Windows the retained
    /// directory handle denies delete/rename sharing, making `public_path`
    /// stable while this bound corpus remains alive.
    #[cfg(unix)]
    pub fn configure_command_cwd(&self, command: &mut std::process::Command) -> BoundaryResult<()> {
        configure_retained_cwd(command, &self.runner_cwd)
            .map_err(|error| BoundaryError::new(&self.public_path, error.to_string()))
    }

    #[cfg(windows)]
    pub fn configure_command_cwd(&self, command: &mut std::process::Command) -> BoundaryResult<()> {
        command.current_dir(&self.public_path);
        Ok(())
    }
}

/// Private evaluator device directories, all bound below the corpus handle.
#[derive(Debug)]
pub struct BoundDevice {
    public_path: PathBuf,
    handle: fs::File,
    home: PathBuf,
    home_handle: fs::File,
    config: PathBuf,
    config_handle: fs::File,
    cache: PathBuf,
    cache_handle: fs::File,
    data: PathBuf,
    data_handle: fs::File,
    state: PathBuf,
    state_handle: fs::File,
    runtime: PathBuf,
    runtime_handle: fs::File,
}

impl BoundDevice {
    fn create(corpus: &fs::File, corpus_path: &Path) -> BoundaryResult<Self> {
        let public_path = corpus_path.join(DEVICE_DIR);
        let handle = create_or_open_child_dir(corpus, DEVICE_DIR, &public_path)?;
        let mut directories = Vec::with_capacity(DEVICE_SUBDIRS.len());
        for name in DEVICE_SUBDIRS {
            let path = public_path.join(name);
            let child = create_or_open_child_dir(&handle, name, &path)?;
            directories.push((path, child));
        }
        let mut directories = directories.into_iter();
        let (home, home_handle) = directories.next().expect("fixed device subdirectory list");
        let (config, config_handle) = directories.next().expect("fixed device subdirectory list");
        let (cache, cache_handle) = directories.next().expect("fixed device subdirectory list");
        let (data, data_handle) = directories.next().expect("fixed device subdirectory list");
        let (state, state_handle) = directories.next().expect("fixed device subdirectory list");
        let (runtime, runtime_handle) = directories.next().expect("fixed device subdirectory list");
        Ok(Self {
            public_path,
            handle,
            home,
            home_handle,
            config,
            config_handle,
            cache,
            cache_handle,
            data,
            data_handle,
            state,
            state_handle,
            runtime,
            runtime_handle,
        })
    }

    #[must_use]
    pub fn public_path(&self) -> &Path {
        &self.public_path
    }
    #[must_use]
    pub fn home(&self) -> &Path {
        &self.home
    }
    #[must_use]
    pub fn config(&self) -> &Path {
        &self.config
    }
    #[must_use]
    pub fn cache(&self) -> &Path {
        &self.cache
    }
    #[must_use]
    pub fn data(&self) -> &Path {
        &self.data
    }
    #[must_use]
    pub fn state(&self) -> &Path {
        &self.state
    }
    #[must_use]
    pub fn runtime(&self) -> &Path {
        &self.runtime
    }

    /// Hermetic fixture-only environment for evaluator subprocesses.
    ///
    /// Credentials, agent sockets, and ambient Kio seams are excluded. `PATH`
    /// is the sole ambient value because platform helpers may need it; locale
    /// and time zone remain deterministic.
    #[must_use]
    pub fn hermetic_environment(&self) -> Vec<(OsString, OsString)> {
        let path = env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin"));
        vec![
            (OsString::from("PATH"), path),
            (OsString::from("LANG"), OsString::from("C.UTF-8")),
            (OsString::from("LC_ALL"), OsString::from("C.UTF-8")),
            (OsString::from("TZ"), OsString::from("UTC")),
            (OsString::from("HOME"), self.home.as_os_str().to_owned()),
            (
                OsString::from("XDG_CONFIG_HOME"),
                self.config.as_os_str().to_owned(),
            ),
            (
                OsString::from("XDG_CACHE_HOME"),
                self.cache.as_os_str().to_owned(),
            ),
            (
                OsString::from("XDG_DATA_HOME"),
                self.data.as_os_str().to_owned(),
            ),
            (
                OsString::from("XDG_STATE_HOME"),
                self.state.as_os_str().to_owned(),
            ),
            (
                OsString::from("XDG_RUNTIME_DIR"),
                self.runtime.as_os_str().to_owned(),
            ),
        ]
    }
    pub fn try_clone_home_handle(&self) -> BoundaryResult<fs::File> {
        self.home_handle
            .try_clone()
            .map_err(|error| BoundaryError::io(&self.home, error))
    }
    pub fn try_clone_config_handle(&self) -> BoundaryResult<fs::File> {
        self.config_handle
            .try_clone()
            .map_err(|error| BoundaryError::io(&self.config, error))
    }
    pub fn try_clone_cache_handle(&self) -> BoundaryResult<fs::File> {
        self.cache_handle
            .try_clone()
            .map_err(|error| BoundaryError::io(&self.cache, error))
    }
    pub fn try_clone_data_handle(&self) -> BoundaryResult<fs::File> {
        self.data_handle
            .try_clone()
            .map_err(|error| BoundaryError::io(&self.data, error))
    }
    pub fn try_clone_state_handle(&self) -> BoundaryResult<fs::File> {
        self.state_handle
            .try_clone()
            .map_err(|error| BoundaryError::io(&self.state, error))
    }
    pub fn try_clone_runtime_handle(&self) -> BoundaryResult<fs::File> {
        self.runtime_handle
            .try_clone()
            .map_err(|error| BoundaryError::io(&self.runtime, error))
    }
    pub fn try_clone_handle(&self) -> BoundaryResult<fs::File> {
        self.handle
            .try_clone()
            .map_err(|error| BoundaryError::io(&self.public_path, error))
    }
}

fn validate_component(value: &str, label: &str) -> BoundaryResult<()> {
    let mut components = Path::new(value).components();
    if value.is_empty()
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(BoundaryError::new(value, format!("invalid {label}")));
    }
    Ok(())
}

fn open_canonical_directory(path: &Path) -> BoundaryResult<fs::File> {
    let listed = fs::symlink_metadata(path).map_err(|error| BoundaryError::io(path, error))?;
    if listed.file_type().is_symlink() || !listed.is_dir() {
        return Err(BoundaryError::new(
            path,
            "corpus root must be a real directory",
        ));
    }
    let listed_identity = directory_identity_from_path(path, &listed)
        .map_err(|error| BoundaryError::io(path, error))?
        .ok_or_else(|| BoundaryError::new(path, "corpus root must be a real directory"))?;
    let parent = path
        .parent()
        .ok_or_else(|| BoundaryError::new(path, "corpus root has no parent"))?;
    let leaf = path
        .file_name()
        .ok_or_else(|| BoundaryError::new(path, "corpus root has no final component"))?;
    let parent_handle = cap_fs::open_ambient_dir(parent, cap_primitives::ambient_authority())
        .map_err(|error| BoundaryError::io(parent, error))?;
    let handle = cap_fs::open_dir_nofollow(&parent_handle, Path::new(leaf))
        .map_err(|error| BoundaryError::io(path, error))?;
    let opened_identity = directory_identity_from_file(&handle)
        .map_err(|error| BoundaryError::io(path, error))?
        .ok_or_else(|| BoundaryError::new(path, "corpus root is not a real directory"))?;
    if listed_identity != opened_identity {
        return Err(BoundaryError::new(
            path,
            "corpus root changed while opening",
        ));
    }
    Ok(handle)
}

fn open_child_dir(parent: &fs::File, name: &str, path: &Path) -> BoundaryResult<fs::File> {
    cap_fs::open_dir_nofollow(parent, Path::new(name))
        .map_err(|_| BoundaryError::new(path, "must be a real non-reparse directory"))
}

fn create_or_open_child_dir(
    parent: &fs::File,
    name: &str,
    path: &Path,
) -> BoundaryResult<fs::File> {
    match open_child_dir(parent, name, path) {
        Ok(handle) => Ok(handle),
        Err(_) => match cap_fs::stat(parent, Path::new(name), cap_fs::FollowSymlinks::No) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let options = cap_fs::DirOptions::new();
                #[cfg(unix)]
                let options = {
                    use cap_fs::DirBuilderExt;
                    let mut options = options;
                    options.mode(0o700);
                    options
                };
                match cap_fs::create_dir(parent, Path::new(name), &options) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(BoundaryError::io(path, error)),
                }
                open_child_dir(parent, name, path)
            }
            Ok(_) => Err(BoundaryError::new(
                path,
                "must be a real non-reparse directory",
            )),
            Err(error) => Err(BoundaryError::io(path, error)),
        },
    }
}

#[cfg(unix)]
fn open_runner_cwd(scope: &fs::File, path: &Path) -> BoundaryResult<fs::File> {
    let mut options = cap_fs::OpenOptions::new();
    options.read(true);
    cap_fs::open(scope, Path::new("."), &options).map_err(|error| BoundaryError::io(path, error))
}

pub(crate) fn directory_identity_from_path(
    path: &Path,
    listed: &fs::Metadata,
) -> io::Result<Option<DirectoryIdentity>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let _ = path;
        return Ok(
            (listed.is_dir() && !listed.file_type().is_symlink()).then_some(
                DirectoryIdentity::Unix {
                    dev: listed.dev(),
                    ino: listed.ino(),
                },
            ),
        );
    }
    #[cfg(windows)]
    {
        let _ = listed;
        return kio_core::cas::windows_real_directory_identity(path)
            .map(|identity| identity.map(DirectoryIdentity::Windows));
    }
    #[allow(unreachable_code)]
    Ok(None)
}

pub(crate) fn directory_identity_from_file(
    file: &fs::File,
) -> io::Result<Option<DirectoryIdentity>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        return Ok(
            (metadata.is_dir() && !metadata.file_type().is_symlink()).then_some(
                DirectoryIdentity::Unix {
                    dev: metadata.dev(),
                    ino: metadata.ino(),
                },
            ),
        );
    }
    #[cfg(windows)]
    {
        return Ok(
            kio_core::cas::windows_directory_handle_identity(file).map(DirectoryIdentity::Windows)
        );
    }
    #[allow(unreachable_code)]
    Ok(None)
}

/// Durably flush a retained directory without relying on its capability handle
/// being directly syncable.  On Linux, `open_dir_nofollow` may retain an
/// `O_PATH` descriptor: it is valid authority for relative operations but
/// `fsync(2)` rejects it with `EBADF`.  Reopen exactly that directory as `.`
/// without following links, verify the held identity, and sync the reopened
/// descriptor instead.
pub(crate) fn sync_retained_directory(
    directory: &fs::File,
    _expected: &fs::Metadata,
    path: &Path,
) -> BoundaryResult<()> {
    #[cfg(unix)]
    {
        let mut options = cap_fs::OpenOptions::new();
        options
            .read(true)
            ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
        let syncable = cap_fs::open(directory, Path::new("."), &options)
            .map_err(|error| BoundaryError::io(path, error))?;
        let expected = directory_identity_from_file(directory)
            .map_err(|error| BoundaryError::io(path, error))?;
        let observed = directory_identity_from_file(&syncable)
            .map_err(|error| BoundaryError::io(path, error))?;
        if !matches!((expected, observed), (Some(expected), Some(observed)) if expected == observed)
        {
            return Err(BoundaryError::new(
                path,
                "retained directory changed before sync",
            ));
        }
        syncable
            .sync_all()
            .map_err(|error| BoundaryError::io(path, error))?;
    }
    #[cfg(windows)]
    {
        if directory_identity_from_file(directory)
            .map_err(|error| BoundaryError::io(path, error))?
            .is_none()
        {
            return Err(BoundaryError::new(path, "retained directory is not real"));
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::BoundCorpus;
    use std::{fs, os::unix::fs::symlink, process::Command};

    fn corpus() -> (tempfile::TempDir, Vec<String>) {
        let temp = tempfile::TempDir::new().unwrap();
        fs::create_dir(temp.path().join("research")).unwrap();
        fs::create_dir(temp.path().join("research/.kio")).unwrap();
        (temp, vec!["research".to_owned()])
    }

    #[test]
    fn rejects_symlinked_scope_and_kio() {
        let (temp, scopes) = corpus();
        let outside = tempfile::TempDir::new().unwrap();
        fs::remove_dir(temp.path().join("research/.kio")).unwrap();
        symlink(outside.path(), temp.path().join("research/.kio")).unwrap();
        assert!(BoundCorpus::bind(temp.path(), &scopes).is_err());

        fs::remove_file(temp.path().join("research/.kio")).unwrap();
        fs::remove_dir(temp.path().join("research")).unwrap();
        symlink(outside.path(), temp.path().join("research")).unwrap();
        assert!(BoundCorpus::bind(temp.path(), &scopes).is_err());
    }

    #[test]
    fn retained_scope_handle_survives_public_path_swap() {
        let (temp, scopes) = corpus();
        let bound = BoundCorpus::bind(temp.path(), &scopes).unwrap();
        let replacement = tempfile::TempDir::new().unwrap();
        fs::rename(
            temp.path().join("research"),
            temp.path().join("old-research"),
        )
        .unwrap();
        symlink(replacement.path(), temp.path().join("research")).unwrap();
        let kio = bound.scopes()[0].try_clone_kio_handle().unwrap();
        assert!(kio.metadata().unwrap().is_dir());
        assert!(
            fs::symlink_metadata(temp.path().join("research"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn command_cwd_uses_retained_scope_after_public_path_swap() {
        let (temp, scopes) = corpus();
        fs::write(temp.path().join("research/.kio/original"), b"bound").unwrap();
        let bound = BoundCorpus::bind(temp.path(), &scopes).unwrap();
        let replacement = tempfile::TempDir::new().unwrap();
        fs::rename(
            temp.path().join("research"),
            temp.path().join("old-research"),
        )
        .unwrap();
        symlink(replacement.path(), temp.path().join("research")).unwrap();

        let mut command = Command::new("/bin/sh");
        command.args(["-c", "test -f .kio/original && cat .kio/original"]);
        bound
            .scope("research")
            .unwrap()
            .configure_command_cwd(&mut command)
            .unwrap();
        let output = command.output().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"bound");
    }

    #[test]
    fn rejects_symlinked_device_directory() {
        let (temp, scopes) = corpus();
        let outside = tempfile::TempDir::new().unwrap();
        symlink(outside.path(), temp.path().join(".kio-eval-device")).unwrap();
        assert!(BoundCorpus::bind(temp.path(), &scopes).is_err());
    }

    #[test]
    fn rejects_symlinked_device_child() {
        let (temp, scopes) = corpus();
        let outside = tempfile::TempDir::new().unwrap();
        fs::create_dir(temp.path().join(".kio-eval-device")).unwrap();
        symlink(outside.path(), temp.path().join(".kio-eval-device/home")).unwrap();
        assert!(BoundCorpus::bind(temp.path(), &scopes).is_err());
    }
}
