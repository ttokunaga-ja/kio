//! Capability boundary for the fresh, Rust-owned history replay.
//!
//! This module deliberately does not run commands or interpret a history plan.
//! It only retains the filesystem authority that a replay executor may use after
//! it has validated its immutable input.  Public paths are diagnostics, never
//! authority: every corpus and scope operation goes through a retained handle.

use std::{
    collections::HashSet,
    fs,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

use cap_primitives::{ambient_authority, fs as cap_fs};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::boundary::{
    DirectoryIdentity, directory_identity_from_file, directory_identity_from_path,
};
#[cfg(target_os = "linux")]
use crate::process_boundary::configure_descriptor_environment;
#[cfg(unix)]
use crate::process_boundary::configure_retained_cwd;
use crate::process_boundary::{DescriptorExecutable, ProcessBoundaryError};

const DEVICE_DIR: &str = ".kio-eval-device";
const HISTORY_MANIFEST: &str = "history-manifest.json";
const MAX_SOURCE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 128 * 1024 * 1024;
const MAX_DIRECT_ENTRY_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
static ROLLBACK_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub type ReplayBoundaryResult<T> = Result<T, ReplayBoundaryError>;

#[derive(Debug, Error)]
pub enum ReplayBoundaryError {
    #[error("unsafe history replay boundary at {path}: {message}")]
    Unsafe { path: PathBuf, message: String },
    #[error("history replay boundary is unsupported on this platform: {0}")]
    Unsupported(&'static str),
    #[error("history manifest publication is indeterminate at {path}: {message}")]
    Indeterminate { path: PathBuf, message: String },
}

impl ReplayBoundaryError {
    fn unsafe_(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self::Unsafe {
            path: path.into(),
            message: message.into(),
        }
    }
    fn io(path: impl Into<PathBuf>, error: io::Error) -> Self {
        Self::unsafe_(path, error.to_string())
    }
}

fn process_boundary_error(error: ProcessBoundaryError) -> ReplayBoundaryError {
    match error {
        ProcessBoundaryError::Unsafe { path, message } => {
            ReplayBoundaryError::Unsafe { path, message }
        }
        ProcessBoundaryError::Unsupported(message) => ReplayBoundaryError::Unsupported(message),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileBinding {
    pub sha256: String,
    pub bytes: u64,
}

/// An observed direct child. The executor compares these exact, sorted facts
/// to the frozen corpus manifest before it creates any replay state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectEntry {
    pub name: String,
    pub kind: DirectEntryKind,
    pub binding: Option<FileBinding>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectEntryKind {
    RegularFile,
    Directory,
}

/// A fresh corpus and exactly the supplied direct child scope directories.
#[derive(Debug)]
pub struct ReplayBoundary {
    public_root: PathBuf,
    root: fs::File,
    root_identity: DirectoryIdentity,
    scopes: Vec<ReplayScope>,
}

impl ReplayBoundary {
    /// Confirm that this platform can execute the copied binary and expose
    /// HOME/XDG roots through retained descriptors without pathname fallback.
    /// Call this before binding or preparing a corpus device.
    #[cfg(target_os = "linux")]
    pub fn preflight_platform() -> ReplayBoundaryResult<()> {
        DescriptorExecutable::preflight_platform().map_err(process_boundary_error)
    }
    #[cfg(not(target_os = "linux"))]
    pub fn preflight_platform() -> ReplayBoundaryResult<()> {
        Err(ReplayBoundaryError::Unsupported(
            "descriptor-bound replay execution and environment",
        ))
    }

    /// Bind a *fresh* generated corpus. This is read-only: the private device
    /// is created only later, after the executor has validated its plan.
    pub fn bind(corpus: &Path, scope_names: &[String]) -> ReplayBoundaryResult<Self> {
        if !corpus.is_absolute() {
            return Err(ReplayBoundaryError::unsafe_(
                corpus,
                "corpus path must be absolute",
            ));
        }
        // Do not canonicalize the final user-controlled leaf after checking
        // it: bind that exact component nofollow from a normalized parent.
        let public_root = corpus.to_path_buf();
        let (root, root_identity) = open_directory(&public_root, "corpus root")?;
        let mut seen = HashSet::new();
        let mut scopes = Vec::with_capacity(scope_names.len());
        for name in scope_names {
            normal_component(name, "scope name")?;
            if !seen.insert(name) {
                return Err(ReplayBoundaryError::unsafe_(
                    &public_root,
                    "duplicate scope name",
                ));
            }
            let public_path = public_root.join(name);
            let (handle, identity) = open_child_directory(&root, name, &public_path, "scope")?;
            scopes.push(ReplayScope {
                name: name.clone(),
                public_path,
                handle,
                identity,
            });
        }
        let boundary = Self {
            public_root,
            root,
            root_identity,
            scopes,
        };
        boundary.verify_fresh()?;
        Ok(boundary)
    }

    #[must_use]
    pub fn public_root(&self) -> &Path {
        &self.public_root
    }
    #[must_use]
    pub fn scopes(&self) -> &[ReplayScope] {
        &self.scopes
    }
    #[must_use]
    pub fn scope(&self, name: &str) -> Option<&ReplayScope> {
        self.scopes.iter().find(|s| s.name == name)
    }

    /// Capability-read a root-level corpus input after rechecking identities.
    pub fn read_root_file(
        &self,
        name: &str,
        expected_sha256: &str,
    ) -> ReplayBoundaryResult<Vec<u8>> {
        normal_component(name, "root input filename")?;
        self.recheck_identities()?;
        let bytes = regular_at(&self.root, name, MAX_SOURCE_BYTES, &self.public_root)?;
        if sha256(&bytes) != expected_sha256 {
            return Err(ReplayBoundaryError::unsafe_(
                self.public_root.join(name),
                "root source hash precondition failed",
            ));
        }
        self.recheck_identities()?;
        Ok(bytes)
    }

    /// Bounded, sorted observations of corpus-root direct children.
    pub fn root_entries(&self) -> ReplayBoundaryResult<Vec<DirectEntry>> {
        self.recheck_identities()?;
        direct_entries(&self.root, &self.public_root, 16)
    }

    /// Call before and after every Kio subprocess. This detects public path
    /// replacement even though the child itself uses a retained cwd handle.
    pub fn recheck_after_command(&self) -> ReplayBoundaryResult<()> {
        self.recheck_identities()
    }

    /// Recheck public identities and that replay has not been started already.
    pub fn verify_fresh(&self) -> ReplayBoundaryResult<()> {
        self.recheck_identities()?;
        for name in [HISTORY_MANIFEST, DEVICE_DIR] {
            match cap_fs::stat(&self.root, Path::new(name), cap_fs::FollowSymlinks::No) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Ok(_) => {
                    return Err(ReplayBoundaryError::unsafe_(
                        self.public_root.join(name),
                        "fresh corpus already contains replay state",
                    ));
                }
                Err(error) => {
                    return Err(ReplayBoundaryError::io(self.public_root.join(name), error));
                }
            }
        }
        for scope in &self.scopes {
            match cap_fs::stat(&scope.handle, Path::new(".kio"), cap_fs::FollowSymlinks::No) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Ok(_) => {
                    return Err(ReplayBoundaryError::unsafe_(
                        scope.public_path.join(".kio"),
                        "scope is not fresh",
                    ));
                }
                Err(error) => {
                    return Err(ReplayBoundaryError::io(
                        scope.public_path.join(".kio"),
                        error,
                    ));
                }
            }
        }
        Ok(())
    }

    /// Create a private device only after strict plan/freshness validation.
    pub fn prepare_device(&self) -> ReplayBoundaryResult<ReplayDevice> {
        self.verify_fresh()?;
        let device_path = self.public_root.join(DEVICE_DIR);
        create_private_dir(&self.root, DEVICE_DIR, &device_path)?;
        let (device, identity) =
            open_child_directory(&self.root, DEVICE_DIR, &device_path, "private device")?;
        let mut dirs = Vec::new();
        for name in [
            "home", "config", "cache", "data", "state", "runtime", "bin", "trash",
        ] {
            let path = device_path.join(name);
            create_private_dir(&device, name, &path)?;
            let (handle, identity) =
                open_child_directory(&device, name, &path, "private device directory")?;
            dirs.push(DeviceDir {
                name,
                path,
                handle,
                identity,
            });
        }
        self.recheck_active()?;
        Ok(ReplayDevice {
            public_path: device_path,
            handle: device,
            identity,
            dirs,
        })
    }

    /// Recheck public identities during the active phase (where a device is
    /// expected to exist, so `verify_fresh` is intentionally inapplicable).
    pub fn recheck_active(&self) -> ReplayBoundaryResult<()> {
        self.recheck_identities()
    }

    fn recheck_identities(&self) -> ReplayBoundaryResult<()> {
        let (_, root_identity) = open_directory(&self.public_root, "corpus root")?;
        if root_identity != self.root_identity {
            return Err(ReplayBoundaryError::unsafe_(
                &self.public_root,
                "corpus root identity changed",
            ));
        }
        for scope in &self.scopes {
            scope.recheck_public_identity()?;
        }
        Ok(())
    }
}

/// A durable staged history manifest. Dropping an unpublished token removes
/// its private temp name; publishing is create-only and never overwrites a
/// previous success record.
#[derive(Debug)]
pub struct StagedHistoryManifest<'a> {
    boundary: &'a ReplayBoundary,
    staging: fs::File,
    temp: String,
    binding: FileBinding,
    identity: FileIdentity,
    published: bool,
    #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
    fail_after_rename: bool,
    #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
    post_root_sync: Option<PostRootSyncAction>,
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
#[derive(Debug)]
enum PostRootSyncAction {
    Replace(Vec<u8>),
    Remove,
}

impl StagedHistoryManifest<'_> {
    pub fn publish(mut self) -> ReplayBoundaryResult<()> {
        rename_no_replace_between(
            &self.staging,
            &self.temp,
            &self.boundary.root,
            HISTORY_MANIFEST,
        )?;
        self.published = true;
        #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
        if self.fail_after_rename {
            return self.rollback_after_publish(ReplayBoundaryError::unsafe_(
                &self.boundary.public_root,
                "injected post-rename publication failure",
            ));
        }
        if let Err(error) = self
            .boundary
            .root
            .sync_all()
            .map_err(|e| ReplayBoundaryError::io(&self.boundary.public_root, e))
        {
            return self.rollback_after_publish(error);
        }
        #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
        if let Some(action) = self.post_root_sync.take() {
            self.inject_post_root_sync_action(action)?;
        }
        self.verify_publication()?;
        // A public-path replacement after publication makes the result
        // indeterminate.  Do not attempt a cleanup through the retained old
        // root: it cannot establish anything about the replacement namespace.
        self.boundary
            .recheck_identities()
            .map_err(|error| ReplayBoundaryError::Indeterminate {
                path: self.boundary.public_root.join(HISTORY_MANIFEST),
                message: format!("post-publication boundary identity changed: {error}"),
            })?;
        // Close the interval opened by the identity recheck. A success result
        // is therefore backed by the exact staged inode and bytes at the final
        // observation barrier.
        self.verify_publication()?;
        Ok(())
    }

    fn verify_publication(&self) -> ReplayBoundaryResult<()> {
        let observed = regular_observed_at(
            &self.boundary.root,
            HISTORY_MANIFEST,
            MAX_SOURCE_BYTES,
            &self.boundary.public_root,
        )
        .map_err(|error| ReplayBoundaryError::Indeterminate {
            path: self.boundary.public_root.join(HISTORY_MANIFEST),
            message: format!("published manifest cannot be verified: {error}"),
        })?;
        let binding = FileBinding {
            sha256: sha256(&observed.bytes),
            bytes: observed.bytes.len() as u64,
        };
        if observed.identity != self.identity || binding != self.binding {
            return Err(ReplayBoundaryError::Indeterminate {
                path: self.boundary.public_root.join(HISTORY_MANIFEST),
                message: "published manifest differs from staged authority".into(),
            });
        }
        Ok(())
    }

    fn rollback_after_publish(&mut self, cause: ReplayBoundaryError) -> ReplayBoundaryResult<()> {
        // Never observe then unlink a public name: another same-UID process
        // could replace it in between. Instead atomically move whatever is at
        // the public leaf into retained private staging, then prove it was our
        // staged object. A mismatched capture is retained as evidence.
        let capture = format!(
            ".{HISTORY_MANIFEST}.{}.{}.rollback",
            std::process::id(),
            ROLLBACK_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        if rename_no_replace_between(
            &self.boundary.root,
            HISTORY_MANIFEST,
            &self.staging,
            &capture,
        )
        .is_err()
        {
            return Err(ReplayBoundaryError::Indeterminate {
                path: self.boundary.public_root.join(HISTORY_MANIFEST),
                message: "could not atomically capture post-rename manifest for rollback".into(),
            });
        }
        if self.boundary.root.sync_all().is_err() || self.staging.sync_all().is_err() {
            return Err(ReplayBoundaryError::Indeterminate {
                path: self.boundary.public_root.join(HISTORY_MANIFEST),
                message: "could not durably capture post-rename manifest for rollback".into(),
            });
        }
        let observed = regular_observed_at(
            &self.staging,
            &capture,
            MAX_SOURCE_BYTES,
            &self.boundary.public_root.join(DEVICE_DIR).join("state"),
        )
        .map_err(|_| ReplayBoundaryError::Indeterminate {
            path: self.boundary.public_root.join(HISTORY_MANIFEST),
            message: "captured post-rename manifest cannot be verified".into(),
        })?;
        let binding = FileBinding {
            sha256: sha256(&observed.bytes),
            bytes: observed.bytes.len() as u64,
        };
        if observed.identity != self.identity || binding != self.binding {
            return Err(ReplayBoundaryError::Indeterminate {
                path: self.boundary.public_root.join(HISTORY_MANIFEST),
                message: "captured post-rename manifest differs from staged authority".into(),
            });
        }
        ensure_absent(
            &self.boundary.root,
            HISTORY_MANIFEST,
            &self.boundary.public_root,
        )
        .map_err(|_| ReplayBoundaryError::Indeterminate {
            path: self.boundary.public_root.join(HISTORY_MANIFEST),
            message: "manifest reappeared while completing rollback".into(),
        })?;
        Err(cause)
    }
    #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
    fn inject_post_rename_failure(mut self) -> Self {
        self.fail_after_rename = true;
        self
    }
    #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
    fn inject_post_root_sync_replacement(mut self, bytes: Vec<u8>) -> Self {
        self.post_root_sync = Some(PostRootSyncAction::Replace(bytes));
        self
    }
    #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
    fn inject_post_root_sync_removal(mut self) -> Self {
        self.post_root_sync = Some(PostRootSyncAction::Remove);
        self
    }
    #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
    fn inject_post_root_sync_action(&self, action: PostRootSyncAction) -> ReplayBoundaryResult<()> {
        cap_fs::remove_file(&self.boundary.root, Path::new(HISTORY_MANIFEST)).map_err(|e| {
            ReplayBoundaryError::io(self.boundary.public_root.join(HISTORY_MANIFEST), e)
        })?;
        if let PostRootSyncAction::Replace(bytes) = action {
            let mut options = cap_fs::OpenOptions::new();
            options.write(true).create_new(true);
            let mut replacement =
                cap_fs::open(&self.boundary.root, Path::new(HISTORY_MANIFEST), &options).map_err(
                    |e| {
                        ReplayBoundaryError::io(self.boundary.public_root.join(HISTORY_MANIFEST), e)
                    },
                )?;
            replacement
                .write_all(&bytes)
                .and_then(|_| replacement.sync_all())
                .map_err(|e| {
                    ReplayBoundaryError::io(self.boundary.public_root.join(HISTORY_MANIFEST), e)
                })?;
        }
        self.boundary
            .root
            .sync_all()
            .map_err(|e| ReplayBoundaryError::io(&self.boundary.public_root, e))
    }
}
impl Drop for StagedHistoryManifest<'_> {
    fn drop(&mut self) {
        if !self.published {
            let _ = cap_fs::remove_file(&self.staging, Path::new(&self.temp));
        }
    }
}

#[derive(Debug)]
pub struct ReplayScope {
    name: String,
    public_path: PathBuf,
    handle: fs::File,
    identity: DirectoryIdentity,
}

impl ReplayScope {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn direct_entries(&self) -> ReplayBoundaryResult<Vec<DirectEntry>> {
        self.recheck_public_identity()?;
        direct_entries(&self.handle, &self.public_path, 256)
    }
    pub fn recheck_after_command(&self) -> ReplayBoundaryResult<()> {
        self.recheck_public_identity()
    }

    /// Bind the immutable post-`init` authority required before replay can
    /// execute index/snapshot commands. The canonical initial config is an
    /// existing, empty, single-link regular file.
    pub fn bind_initialized_authority(&self) -> ReplayBoundaryResult<InitializedScopeAuthority> {
        self.recheck_public_identity()?;
        let kio_path = self.public_path.join(".kio");
        let (kio, kio_identity) =
            open_child_directory(&self.handle, ".kio", &kio_path, "initialized .kio")?;
        let config = regular_observed_at(&kio, "config.toml", MAX_SOURCE_BYTES, &kio_path)?;
        if !config.bytes.is_empty() {
            return Err(ReplayBoundaryError::unsafe_(
                kio_path.join("config.toml"),
                "initialized config.toml must be exactly empty",
            ));
        }
        Ok(InitializedScopeAuthority {
            kio_path,
            kio,
            kio_identity,
            config_identity: config.identity,
        })
    }

    /// Arrange a command to `fchdir` to this retained scope immediately before
    /// exec. The parent cwd is never modified.
    #[cfg(unix)]
    pub fn configure_command_cwd(&self, command: &mut Command) -> ReplayBoundaryResult<()> {
        configure_retained_cwd(command, &self.handle).map_err(process_boundary_error)
    }
    #[cfg(not(unix))]
    pub fn configure_command_cwd(&self, _command: &mut Command) -> ReplayBoundaryResult<()> {
        Err(ReplayBoundaryError::Unsupported(
            "retained-handle subprocess cwd",
        ))
    }

    pub fn read_file(
        &self,
        relative: &str,
        expected_sha256: &str,
    ) -> ReplayBoundaryResult<Vec<u8>> {
        normal_component(relative, "source filename")?;
        self.recheck_public_identity()?;
        let bytes = regular_at(&self.handle, relative, MAX_SOURCE_BYTES, &self.public_path)?;
        if sha256(&bytes) != expected_sha256 {
            return Err(ReplayBoundaryError::unsafe_(
                self.public_path.join(relative),
                "source hash precondition failed",
            ));
        }
        self.recheck_public_identity()?;
        Ok(bytes)
    }

    pub fn edit_text(
        &self,
        relative: &str,
        old: &str,
        new: &str,
        before: &str,
        after: &str,
    ) -> ReplayBoundaryResult<()> {
        let bytes = self.read_file(relative, before)?;
        let text = std::str::from_utf8(&bytes).map_err(|_| {
            ReplayBoundaryError::unsafe_(
                self.public_path.join(relative),
                "edit source is not UTF-8",
            )
        })?;
        if text.matches(old).count() != 1 {
            return Err(ReplayBoundaryError::unsafe_(
                self.public_path.join(relative),
                "edit old value does not occur exactly once",
            ));
        }
        let replaced = text.replacen(old, new, 1).into_bytes();
        if sha256(&replaced) != after {
            return Err(ReplayBoundaryError::unsafe_(
                self.public_path.join(relative),
                "edit post-hash precondition failed",
            ));
        }
        replace_regular(&self.handle, relative, &replaced, before, &self.public_path)?;
        let verified = regular_at(&self.handle, relative, MAX_SOURCE_BYTES, &self.public_path)?;
        if sha256(&verified) != after {
            return Err(ReplayBoundaryError::unsafe_(
                self.public_path.join(relative),
                "edit changed during verification",
            ));
        }
        self.recheck_public_identity()
    }

    pub fn rename(
        &self,
        source: &str,
        destination: &str,
        expected_sha256: &str,
    ) -> ReplayBoundaryResult<()> {
        normal_component(source, "rename source filename")?;
        normal_component(destination, "rename destination filename")?;
        let source_bytes = self.read_file(source, expected_sha256)?;
        ensure_absent(&self.handle, destination, &self.public_path)?;
        rename_no_replace(&self.handle, source, destination)?;
        let verified = regular_at(
            &self.handle,
            destination,
            MAX_SOURCE_BYTES,
            &self.public_path,
        )?;
        if verified != source_bytes || sha256(&verified) != expected_sha256 {
            return Err(ReplayBoundaryError::unsafe_(
                self.public_path.join(destination),
                "rename postcondition failed",
            ));
        }
        self.handle
            .sync_all()
            .map_err(|e| ReplayBoundaryError::io(&self.public_path, e))?;
        self.recheck_public_identity()
    }

    fn recheck_public_identity(&self) -> ReplayBoundaryResult<()> {
        let (_, identity) = open_directory(&self.public_path, "scope")?;
        if identity != self.identity {
            return Err(ReplayBoundaryError::unsafe_(
                &self.public_path,
                "scope identity changed",
            ));
        }
        Ok(())
    }
}

/// Retained post-init `.kio` and canonical empty config authority.
#[derive(Debug)]
pub struct InitializedScopeAuthority {
    kio_path: PathBuf,
    kio: fs::File,
    kio_identity: DirectoryIdentity,
    config_identity: FileIdentity,
}
impl InitializedScopeAuthority {
    pub fn recheck(&self) -> ReplayBoundaryResult<()> {
        let (_, kio_identity) = open_directory(&self.kio_path, "initialized .kio")?;
        if kio_identity != self.kio_identity {
            return Err(ReplayBoundaryError::unsafe_(
                &self.kio_path,
                "initialized .kio identity changed",
            ));
        }
        let config =
            regular_observed_at(&self.kio, "config.toml", MAX_SOURCE_BYTES, &self.kio_path)?;
        if config.identity != self.config_identity || !config.bytes.is_empty() {
            return Err(ReplayBoundaryError::unsafe_(
                self.kio_path.join("config.toml"),
                "initialized config authority changed",
            ));
        }
        Ok(())
    }
}

/// Private HOME/XDG root plus a copied executable, all below the corpus.
#[derive(Debug)]
pub struct ReplayDevice {
    public_path: PathBuf,
    handle: fs::File,
    identity: DirectoryIdentity,
    dirs: Vec<DeviceDir>,
}
#[derive(Debug)]
struct DeviceDir {
    name: &'static str,
    path: PathBuf,
    handle: fs::File,
    identity: DirectoryIdentity,
}
impl ReplayDevice {
    /// Durable private-device staging. The corpus root remains unchanged until
    /// the returned token is published after final replay authority checks.
    pub fn stage_history_manifest<'a>(
        &self,
        boundary: &'a ReplayBoundary,
        bytes: &[u8],
    ) -> ReplayBoundaryResult<StagedHistoryManifest<'a>> {
        self.recheck()?;
        boundary.recheck_active()?;
        if bytes.len() as u64 > MAX_SOURCE_BYTES {
            return Err(ReplayBoundaryError::unsafe_(
                &self.public_path,
                "history manifest exceeds bound",
            ));
        }
        #[cfg(not(unix))]
        {
            let _ = bytes;
            Err(ReplayBoundaryError::Unsupported(
                "create-only durable manifest publication",
            ))
        }
        #[cfg(unix)]
        {
            let state = self
                .dirs
                .iter()
                .find(|item| item.name == "state")
                .expect("fixed device dirs");
            let staging = state
                .handle
                .try_clone()
                .map_err(|e| ReplayBoundaryError::io(&state.path, e))?;
            let temp = format!(".{HISTORY_MANIFEST}.{}.tmp", std::process::id());
            let mut options = cap_fs::OpenOptions::new();
            options.write(true).create_new(true);
            let mut file = cap_fs::open(&staging, Path::new(&temp), &options)
                .map_err(|e| ReplayBoundaryError::io(state.path.join(&temp), e))?;
            let written = file.write_all(bytes).and_then(|_| file.sync_all());
            if let Err(error) = written {
                drop(file);
                let _ = cap_fs::remove_file(&staging, Path::new(&temp));
                return Err(ReplayBoundaryError::io(state.path.join(&temp), error));
            }
            let identity = cap_fs::Metadata::from_file(&file)
                .map_err(|e| ReplayBoundaryError::io(state.path.join(&temp), e))
                .and_then(|metadata| {
                    file_identity(&metadata).ok_or_else(|| {
                        ReplayBoundaryError::unsafe_(
                            state.path.join(&temp),
                            "temporary manifest has no stable file identity",
                        )
                    })
                })?;
            drop(file);
            Ok(StagedHistoryManifest {
                boundary,
                staging,
                temp,
                binding: FileBinding {
                    sha256: sha256(bytes),
                    bytes: bytes.len() as u64,
                },
                identity,
                published: false,
                #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
                fail_after_rename: false,
                #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
                post_root_sync: None,
            })
        }
    }

    #[cfg(target_os = "linux")]
    pub fn configure_hermetic_environment(
        &self,
        command: &mut std::process::Command,
    ) -> ReplayBoundaryResult<()> {
        self.recheck()?;
        let mut directories = Vec::new();
        for (directory, variable) in [
            ("home", "HOME"),
            ("config", "XDG_CONFIG_HOME"),
            ("cache", "XDG_CACHE_HOME"),
            ("data", "XDG_DATA_HOME"),
            ("state", "XDG_STATE_HOME"),
            ("runtime", "XDG_RUNTIME_DIR"),
        ] {
            let handle = self
                .dirs
                .iter()
                .find(|item| item.name == directory)
                .expect("fixed device dirs")
                .handle
                .try_clone()
                .map_err(|e| ReplayBoundaryError::io(&self.public_path, e))?;
            directories.push((variable, handle));
        }
        let borrowed: Vec<_> = directories
            .iter()
            .map(|(variable, handle)| (*variable, handle))
            .collect();
        configure_descriptor_environment(command, &borrowed).map_err(process_boundary_error)
    }
    #[cfg(not(target_os = "linux"))]
    pub fn configure_hermetic_environment(
        &self,
        _command: &mut std::process::Command,
    ) -> ReplayBoundaryResult<()> {
        Err(ReplayBoundaryError::Unsupported(
            "descriptor-backed hermetic environment",
        ))
    }
    pub fn snapshot_executable(&self, source: &Path) -> ReplayBoundaryResult<BoundExecutable> {
        self.recheck()?;
        if !source.is_absolute() {
            return Err(ReplayBoundaryError::unsafe_(
                source,
                "binary path must be absolute",
            ));
        }
        let (parent, name) = source.parent().zip(source.file_name()).ok_or_else(|| {
            ReplayBoundaryError::unsafe_(source, "binary has no parent or filename")
        })?;
        let (parent_handle, _) = open_directory(parent, "binary parent")?;
        let source_name = name.to_string_lossy();
        normal_component(&source_name, "binary filename")?;
        let observed = regular_observed_at(&parent_handle, &source_name, MAX_BINARY_BYTES, parent)?;
        #[cfg(unix)]
        if !observed.executable {
            return Err(ReplayBoundaryError::unsafe_(
                source,
                "binary is not executable",
            ));
        }
        let bytes = observed.bytes;
        let source_identity = observed.identity;
        let bin_dir = self
            .dirs
            .iter()
            .find(|item| item.name == "bin")
            .expect("fixed device dirs");
        let bin_path = bin_dir.path.join("kio");
        let bin = bin_dir
            .handle
            .try_clone()
            .map_err(|e| ReplayBoundaryError::io(&bin_dir.path, e))?;
        let mut options = cap_fs::OpenOptions::new();
        options.write(true).create_new(true);
        let mut out = cap_fs::open(&bin, Path::new("kio"), &options)
            .map_err(|e| ReplayBoundaryError::io(&bin_path, e))?;
        out.write_all(&bytes)
            .and_then(|_| out.sync_all())
            .map_err(|e| ReplayBoundaryError::io(&bin_path, e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            out.set_permissions(fs::Permissions::from_mode(0o700))
                .map_err(|e| ReplayBoundaryError::io(&bin_path, e))?;
        }
        let private_identity = cap_fs::Metadata::from_file(&out)
            .map_err(|e| ReplayBoundaryError::io(&bin_path, e))
            .and_then(|metadata| {
                file_identity(&metadata).ok_or_else(|| {
                    ReplayBoundaryError::unsafe_(
                        &bin_path,
                        "private binary has no stable file identity",
                    )
                })
            })?;
        let mut read_options = cap_fs::OpenOptions::new();
        read_options
            .read(true)
            ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
        let private_executable = cap_fs::open(&bin, Path::new("kio"), &read_options)
            .map_err(|e| ReplayBoundaryError::io(&bin_path, e))?;
        let reopened_identity = cap_fs::Metadata::from_file(&private_executable)
            .map_err(|e| ReplayBoundaryError::io(&bin_path, e))
            .and_then(|metadata| {
                file_identity(&metadata).ok_or_else(|| {
                    ReplayBoundaryError::unsafe_(
                        &bin_path,
                        "reopened private binary has no stable file identity",
                    )
                })
            })?;
        if reopened_identity != private_identity {
            return Err(ReplayBoundaryError::unsafe_(
                &bin_path,
                "private binary changed while reopening executable descriptor",
            ));
        }
        let after = regular_observed_at(&parent_handle, &source_name, MAX_BINARY_BYTES, parent)?;
        if after.bytes != bytes || after.identity != source_identity {
            return Err(ReplayBoundaryError::unsafe_(
                source,
                "binary changed while snapshotting",
            ));
        }
        let descriptor = DescriptorExecutable::bind(source).map_err(process_boundary_error)?;
        Ok(BoundExecutable {
            path: bin_path,
            original: FileBinding {
                sha256: sha256(&bytes),
                bytes: bytes.len() as u64,
            },
            original_parent: parent_handle,
            original_name: source_name.into_owned(),
            original_identity: source_identity,
            private_parent: bin,
            private_identity,
            private_executable,
            descriptor,
        })
    }

    /// Delete means atomically moving the prevalidated source into the private
    /// device trash. It never blindly unlinks a public name.
    pub fn delete_verified(
        &self,
        scope: &ReplayScope,
        relative: &str,
        expected_sha256: &str,
    ) -> ReplayBoundaryResult<()> {
        self.recheck()?;
        normal_component(relative, "delete filename")?;
        scope.read_file(relative, expected_sha256)?;
        let trash_dir = self
            .dirs
            .iter()
            .find(|item| item.name == "trash")
            .expect("fixed device dirs");
        let trash_path = trash_dir.path.clone();
        let trash = trash_dir
            .handle
            .try_clone()
            .map_err(|e| ReplayBoundaryError::io(&trash_path, e))?;
        let captured = format!("{}--{}", scope.name, relative);
        normal_component(&captured, "trash filename")?;
        ensure_absent(&trash, &captured, &trash_path)?;
        rename_no_replace_between(&scope.handle, relative, &trash, &captured)?;
        let bytes = regular_at(&trash, &captured, MAX_SOURCE_BYTES, &trash_path)?;
        if sha256(&bytes) != expected_sha256 {
            return Err(ReplayBoundaryError::unsafe_(
                trash_path.join(&captured),
                "delete capture postcondition failed",
            ));
        }
        scope
            .handle
            .sync_all()
            .map_err(|e| ReplayBoundaryError::io(&scope.public_path, e))?;
        trash
            .sync_all()
            .map_err(|e| ReplayBoundaryError::io(&trash_path, e))?;
        ensure_absent(&scope.handle, relative, &scope.public_path)?;
        scope.recheck_public_identity()
    }

    /// Recheck retained private-device children before and after every command
    /// or mutation that exposes their public names.
    pub fn recheck(&self) -> ReplayBoundaryResult<()> {
        let (_, identity) = open_directory(&self.public_path, "private device")?;
        if identity != self.identity {
            return Err(ReplayBoundaryError::unsafe_(
                &self.public_path,
                "private device identity changed",
            ));
        }
        for item in &self.dirs {
            let (_, identity) = open_child_directory(
                &self.handle,
                item.name,
                &item.path,
                "private device directory",
            )?;
            if identity != item.identity {
                return Err(ReplayBoundaryError::unsafe_(
                    &item.path,
                    "private device directory identity changed",
                ));
            }
        }
        Ok(())
    }
}
#[derive(Debug)]
pub struct BoundExecutable {
    pub path: PathBuf,
    pub original: FileBinding,
    original_parent: fs::File,
    original_name: String,
    original_identity: FileIdentity,
    private_parent: fs::File,
    private_identity: FileIdentity,
    // Keep this nofollow descriptor alive for the whole binding lifetime.  An
    // unlinked private path cannot then recycle this exact inode into a
    // same-bytes replacement that would otherwise evade an identity check.
    private_executable: fs::File,
    descriptor: DescriptorExecutable,
}
impl BoundExecutable {
    #[cfg(all(test, target_os = "linux"))]
    fn sealed_fd(&self) -> std::os::fd::RawFd {
        self.descriptor.sealed_fd()
    }
    /// Recheck both the original supplied binary and the private immutable
    /// snapshot before/after every subprocess invocation.
    pub fn recheck_original(&self) -> ReplayBoundaryResult<()> {
        self.descriptor
            .recheck_original()
            .map_err(process_boundary_error)?;
        let observed = regular_observed_at(
            &self.original_parent,
            &self.original_name,
            MAX_BINARY_BYTES,
            Path::new("<original-kio>"),
        )?;
        if (FileBinding {
            sha256: sha256(&observed.bytes),
            bytes: observed.bytes.len() as u64,
        }) != self.original
            || observed.identity != self.original_identity
        {
            return Err(ReplayBoundaryError::unsafe_(
                &self.path,
                "original binary identity changed",
            ));
        }
        let retained_private = cap_fs::Metadata::from_file(&self.private_executable)
            .map_err(|e| ReplayBoundaryError::io(&self.path, e))?;
        if !retained_private.file_type().is_file()
            || link_count(&retained_private) != Some(1)
            || file_identity(&retained_private) != Some(self.private_identity)
        {
            return Err(ReplayBoundaryError::unsafe_(
                &self.path,
                "retained private binary snapshot changed",
            ));
        }
        let private =
            regular_observed_at(&self.private_parent, "kio", MAX_BINARY_BYTES, &self.path)?;
        if sha256(&private.bytes) != self.original.sha256
            || private.bytes.len() as u64 != self.original.bytes
            || private.identity != self.private_identity
        {
            return Err(ReplayBoundaryError::unsafe_(
                &self.path,
                "private binary snapshot changed",
            ));
        }
        Ok(())
    }

    /// Construct a command that executes only the retained private executable
    /// descriptor. The public snapshot path is never used as exec authority.
    #[cfg(target_os = "linux")]
    pub fn command(&self) -> ReplayBoundaryResult<std::process::Command> {
        self.recheck_original()?;
        self.descriptor.command().map_err(process_boundary_error)
    }
    #[cfg(not(target_os = "linux"))]
    pub fn command(&self) -> ReplayBoundaryResult<std::process::Command> {
        Err(ReplayBoundaryError::Unsupported(
            "descriptor-bound executable command",
        ))
    }
}

fn open_directory(path: &Path, label: &str) -> ReplayBoundaryResult<(fs::File, DirectoryIdentity)> {
    let listed = fs::symlink_metadata(path).map_err(|e| ReplayBoundaryError::io(path, e))?;
    if listed.file_type().is_symlink() || !listed.is_dir() {
        return Err(ReplayBoundaryError::unsafe_(
            path,
            format!("{label} must be a real directory"),
        ));
    }
    let listed_identity = directory_identity_from_path(path, &listed)
        .map_err(|e| ReplayBoundaryError::io(path, e))?
        .ok_or_else(|| ReplayBoundaryError::unsafe_(path, "directory has no stable identity"))?;
    let parent = path
        .parent()
        .ok_or_else(|| ReplayBoundaryError::unsafe_(path, "directory has no parent"))?;
    let leaf = path
        .file_name()
        .ok_or_else(|| ReplayBoundaryError::unsafe_(path, "directory has no final component"))?;
    let parent_handle = open_lexical_directory(parent)?;
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let handle = cap_fs::open(&parent_handle, Path::new(leaf), &options).map_err(|_| {
        ReplayBoundaryError::unsafe_(
            path,
            format!("{label} must be a readable real non-reparse directory"),
        )
    })?;
    let opened = handle
        .metadata()
        .map_err(|e| ReplayBoundaryError::io(path, e))?;
    if !opened.is_dir() {
        return Err(ReplayBoundaryError::unsafe_(
            path,
            format!("{label} must remain a directory while binding"),
        ));
    }
    let opened_identity = directory_identity_from_file(&handle)
        .map_err(|e| ReplayBoundaryError::io(path, e))?
        .ok_or_else(|| ReplayBoundaryError::unsafe_(path, "directory has no stable identity"))?;
    if listed_identity != opened_identity {
        return Err(ReplayBoundaryError::unsafe_(
            path,
            "directory changed while binding",
        ));
    }
    Ok((handle, opened_identity))
}

/// Open every lexical ancestor nofollow. macOS's two OS-owned aliases are
/// rewritten before this walk; no arbitrary user-controlled ancestor symlink
/// is ever resolved.
#[cfg(unix)]
fn open_lexical_directory(path: &Path) -> ReplayBoundaryResult<fs::File> {
    let normalized = normalize_os_alias(path)?;
    if !normalized.is_absolute() {
        return Err(ReplayBoundaryError::unsafe_(
            path,
            "directory path must be absolute",
        ));
    }
    let mut handle = cap_fs::open_ambient_dir(Path::new("/"), ambient_authority())
        .map_err(|e| ReplayBoundaryError::io("/", e))?;
    for component in normalized.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                handle = cap_fs::open_dir_nofollow(&handle, Path::new(name)).map_err(|_| {
                    ReplayBoundaryError::unsafe_(
                        path,
                        "directory ancestor must be a real non-reparse directory",
                    )
                })?;
            }
            _ => {
                return Err(ReplayBoundaryError::unsafe_(
                    path,
                    "directory path is not lexical absolute components",
                ));
            }
        }
    }
    Ok(handle)
}
#[cfg(not(unix))]
fn open_lexical_directory(path: &Path) -> ReplayBoundaryResult<fs::File> {
    cap_fs::open_ambient_dir(path, ambient_authority())
        .map_err(|e| ReplayBoundaryError::io(path, e))
}

#[cfg(target_os = "macos")]
fn normalize_os_alias(path: &Path) -> ReplayBoundaryResult<PathBuf> {
    let value = path
        .to_str()
        .ok_or_else(|| ReplayBoundaryError::unsafe_(path, "path is not UTF-8"))?;
    let rewritten = if value == "/tmp"
        || value.starts_with("/tmp/")
        || value == "/var"
        || value.starts_with("/var/")
    {
        format!("/private{value}")
    } else {
        value.to_owned()
    };
    Ok(PathBuf::from(rewritten))
}
#[cfg(all(unix, not(target_os = "macos")))]
fn normalize_os_alias(path: &Path) -> ReplayBoundaryResult<PathBuf> {
    Ok(path.to_path_buf())
}
fn open_child_directory(
    parent: &fs::File,
    name: &str,
    path: &Path,
    label: &str,
) -> ReplayBoundaryResult<(fs::File, DirectoryIdentity)> {
    let listed = fs::symlink_metadata(path).map_err(|e| ReplayBoundaryError::io(path, e))?;
    if listed.file_type().is_symlink() || !listed.is_dir() {
        return Err(ReplayBoundaryError::unsafe_(
            path,
            format!("{label} must be a real directory"),
        ));
    }
    let listed_identity = directory_identity_from_path(path, &listed)
        .map_err(|e| ReplayBoundaryError::io(path, e))?
        .ok_or_else(|| ReplayBoundaryError::unsafe_(path, "directory has no stable identity"))?;
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let handle = cap_fs::open(parent, Path::new(name), &options).map_err(|_| {
        ReplayBoundaryError::unsafe_(
            path,
            format!("{label} must be a readable real non-reparse directory"),
        )
    })?;
    let metadata = handle
        .metadata()
        .map_err(|e| ReplayBoundaryError::io(path, e))?;
    if !metadata.is_dir() {
        return Err(ReplayBoundaryError::unsafe_(
            path,
            format!("{label} must remain a directory while binding"),
        ));
    }
    let opened_identity = directory_identity_from_file(&handle)
        .map_err(|e| ReplayBoundaryError::io(path, e))?
        .ok_or_else(|| ReplayBoundaryError::unsafe_(path, "directory has no stable identity"))?;
    if listed_identity != opened_identity {
        return Err(ReplayBoundaryError::unsafe_(
            path,
            "directory changed while binding",
        ));
    }
    Ok((handle, opened_identity))
}
fn normal_component(value: &str, label: &str) -> ReplayBoundaryResult<()> {
    let mut parts = Path::new(value).components();
    if value.is_empty()
        || !matches!(parts.next(), Some(Component::Normal(_)))
        || parts.next().is_some()
    {
        Err(ReplayBoundaryError::unsafe_(
            value,
            format!("invalid {label}"),
        ))
    } else {
        Ok(())
    }
}
fn safe_relative(value: &str) -> ReplayBoundaryResult<()> {
    if value.is_empty()
        || Path::new(value).is_absolute()
        || Path::new(value)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        Err(ReplayBoundaryError::unsafe_(
            value,
            "path must be normalized relative components",
        ))
    } else {
        Ok(())
    }
}
fn create_private_dir(parent: &fs::File, name: &str, path: &Path) -> ReplayBoundaryResult<()> {
    normal_component(name, "private directory")?;
    let opts = cap_fs::DirOptions::new();
    #[cfg(unix)]
    let opts = {
        use cap_fs::DirBuilderExt;
        let mut opts = opts;
        opts.mode(0o700);
        opts
    };
    cap_fs::create_dir(parent, Path::new(name), &opts).map_err(|e| ReplayBoundaryError::io(path, e))
}
struct ObservedFile {
    bytes: Vec<u8>,
    identity: FileIdentity,
    #[cfg(unix)]
    executable: bool,
}
fn regular_at(
    root: &fs::File,
    relative: &str,
    maximum: u64,
    label: &Path,
) -> ReplayBoundaryResult<Vec<u8>> {
    Ok(regular_observed_at(root, relative, maximum, label)?.bytes)
}
fn regular_observed_at(
    root: &fs::File,
    relative: &str,
    maximum: u64,
    label: &Path,
) -> ReplayBoundaryResult<ObservedFile> {
    observed_file_at(root, relative, maximum, label, true)
}
fn observed_file_at(
    root: &fs::File,
    relative: &str,
    maximum: u64,
    label: &Path,
    require_single_link: bool,
) -> ReplayBoundaryResult<ObservedFile> {
    safe_relative(relative)?;
    let path = Path::new(relative);
    let before = cap_fs::stat(root, path, cap_fs::FollowSymlinks::No)
        .map_err(|e| ReplayBoundaryError::io(label.join(path), e))?;
    if !before.file_type().is_file()
        || before.len() > maximum
        || !matches!(link_count(&before), Some(1..))
        || (require_single_link && link_count(&before) != Some(1))
    {
        return Err(ReplayBoundaryError::unsafe_(
            label.join(path),
            "must be a bounded single-link regular file",
        ));
    }
    let mut opts = cap_fs::OpenOptions::new();
    opts.read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let file = cap_fs::open(root, path, &opts)
        .map_err(|e| ReplayBoundaryError::io(label.join(path), e))?;
    let opened = cap_fs::Metadata::from_file(&file)
        .map_err(|e| ReplayBoundaryError::io(label.join(path), e))?;
    let after = cap_fs::stat(root, path, cap_fs::FollowSymlinks::No)
        .map_err(|e| ReplayBoundaryError::io(label.join(path), e))?;
    if !opened.file_type().is_file()
        || opened.len() > maximum
        || !matches!(link_count(&opened), Some(1..))
        || (require_single_link && link_count(&opened) != Some(1))
        || file_identity(&before) != file_identity(&opened)
        || file_identity(&opened) != file_identity(&after)
        || file_identity(&opened).is_none()
        || link_count(&before) != link_count(&opened)
        || link_count(&opened) != link_count(&after)
    {
        return Err(ReplayBoundaryError::unsafe_(
            label.join(path),
            "file changed while opening",
        ));
    }
    let mut bytes = Vec::with_capacity(opened.len() as usize);
    file.take(maximum + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| ReplayBoundaryError::io(label.join(path), e))?;
    if bytes.len() as u64 != opened.len() {
        return Err(ReplayBoundaryError::unsafe_(
            label.join(path),
            "file changed while reading",
        ));
    }
    #[cfg(unix)]
    let executable = {
        use cap_fs::PermissionsExt;
        opened.permissions().mode() & 0o111 != 0
    };
    Ok(ObservedFile {
        bytes,
        identity: file_identity(&opened).ok_or_else(|| {
            ReplayBoundaryError::unsafe_(label.join(path), "file has no stable identity")
        })?,
        #[cfg(unix)]
        executable,
    })
}
fn replace_regular(
    root: &fs::File,
    relative: &str,
    bytes: &[u8],
    expected_before: &str,
    label: &Path,
) -> ReplayBoundaryResult<()> {
    safe_relative(relative)?;
    let temp = format!(".{relative}.{}.tmp", std::process::id());
    if temp.contains('/') {
        return Err(ReplayBoundaryError::unsafe_(
            relative,
            "nested writes are unsupported",
        ));
    }
    let mut opts = cap_fs::OpenOptions::new();
    opts.write(true).create_new(true);
    let mut file = cap_fs::open(root, Path::new(&temp), &opts)
        .map_err(|e| ReplayBoundaryError::io(label.join(&temp), e))?;
    let outcome = file.write_all(bytes).and_then(|_| file.sync_all());
    drop(file);
    if let Err(e) = outcome {
        let _ = cap_fs::remove_file(root, Path::new(&temp));
        return Err(ReplayBoundaryError::io(label.join(&temp), e));
    }
    rename_exchange(root, &temp, relative)?;
    // The former target must be exactly the object we prevalidated. The
    // exchange never blindly overwrites it; on mismatch retain both evidence
    // files and fail closed rather than deleting an unknown object.
    let previous = regular_at(root, &temp, MAX_SOURCE_BYTES, label)?;
    if sha256(&previous) != expected_before {
        return Err(ReplayBoundaryError::unsafe_(
            label.join(relative),
            "edit target changed before atomic exchange",
        ));
    }
    cap_fs::remove_file(root, Path::new(&temp))
        .map_err(|e| ReplayBoundaryError::io(label.join(&temp), e))?;
    root.sync_all()
        .map_err(|e| ReplayBoundaryError::io(label, e))
}
fn ensure_absent(root: &fs::File, relative: &str, label: &Path) -> ReplayBoundaryResult<()> {
    safe_relative(relative)?;
    match cap_fs::stat(root, Path::new(relative), cap_fs::FollowSymlinks::No) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(ReplayBoundaryError::unsafe_(
            label.join(relative),
            "destination already exists",
        )),
        Err(e) => Err(ReplayBoundaryError::io(label.join(relative), e)),
    }
}
/// Atomically move `source` to a previously absent destination.  This is the
/// only accepted publication primitive; ordinary rename can clobber evidence.
#[cfg(target_os = "macos")]
fn rename_no_replace(root: &fs::File, source: &str, destination: &str) -> ReplayBoundaryResult<()> {
    rename_no_replace_between(root, source, root, destination)
}
#[cfg(target_os = "macos")]
fn rename_no_replace_between(
    from_root: &fs::File,
    source: &str,
    to_root: &fs::File,
    destination: &str,
) -> ReplayBoundaryResult<()> {
    use std::{
        ffi::CString,
        os::{fd::AsRawFd, raw::c_char},
    };
    unsafe extern "C" {
        fn renameatx_np(
            fromfd: i32,
            from: *const c_char,
            tofd: i32,
            to: *const c_char,
            flags: u32,
        ) -> i32;
    }
    const RENAME_EXCL: u32 = 0x0000_0004;
    let source = CString::new(source)
        .map_err(|_| ReplayBoundaryError::unsafe_(source, "NUL in temporary name"))?;
    let destination = CString::new(destination)
        .map_err(|_| ReplayBoundaryError::unsafe_(destination, "NUL in manifest name"))?;
    if unsafe {
        renameatx_np(
            from_root.as_raw_fd(),
            source.as_ptr(),
            to_root.as_raw_fd(),
            destination.as_ptr(),
            RENAME_EXCL,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(ReplayBoundaryError::io(
            destination.to_string_lossy().as_ref(),
            io::Error::last_os_error(),
        ))
    }
}
#[cfg(target_os = "linux")]
fn rename_no_replace(root: &fs::File, source: &str, destination: &str) -> ReplayBoundaryResult<()> {
    rename_no_replace_between(root, source, root, destination)
}
#[cfg(target_os = "linux")]
fn rename_no_replace_between(
    from_root: &fs::File,
    source: &str,
    to_root: &fs::File,
    destination: &str,
) -> ReplayBoundaryResult<()> {
    use std::{ffi::CString, os::fd::AsRawFd};
    let source = CString::new(source)
        .map_err(|_| ReplayBoundaryError::unsafe_(source, "NUL in temporary name"))?;
    let destination = CString::new(destination)
        .map_err(|_| ReplayBoundaryError::unsafe_(destination, "NUL in manifest name"))?;
    // Linux renameat2's RENAME_NOREPLACE is atomic and rejects existing files.
    if unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            from_root.as_raw_fd(),
            source.as_ptr(),
            to_root.as_raw_fd(),
            destination.as_ptr(),
            1_u32,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(ReplayBoundaryError::io(
            destination.to_string_lossy().as_ref(),
            io::Error::last_os_error(),
        ))
    }
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn rename_no_replace(
    _root: &fs::File,
    _source: &str,
    _destination: &str,
) -> ReplayBoundaryResult<()> {
    Err(ReplayBoundaryError::Unsupported("atomic no-replace rename"))
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn rename_no_replace_between(
    _from: &fs::File,
    _source: &str,
    _to: &fs::File,
    _destination: &str,
) -> ReplayBoundaryResult<()> {
    Err(ReplayBoundaryError::Unsupported("atomic no-replace rename"))
}
#[cfg(target_os = "macos")]
fn rename_exchange(root: &fs::File, left: &str, right: &str) -> ReplayBoundaryResult<()> {
    use std::{
        ffi::CString,
        os::{fd::AsRawFd, raw::c_char},
    };
    unsafe extern "C" {
        fn renameatx_np(
            fromfd: i32,
            from: *const c_char,
            tofd: i32,
            to: *const c_char,
            flags: u32,
        ) -> i32;
    }
    const RENAME_SWAP: u32 = 0x0000_0002;
    let left_c = CString::new(left)
        .map_err(|_| ReplayBoundaryError::unsafe_(left, "NUL in temporary name"))?;
    let right_c = CString::new(right)
        .map_err(|_| ReplayBoundaryError::unsafe_(right, "NUL in source name"))?;
    if unsafe {
        renameatx_np(
            root.as_raw_fd(),
            left_c.as_ptr(),
            root.as_raw_fd(),
            right_c.as_ptr(),
            RENAME_SWAP,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(ReplayBoundaryError::io(right, io::Error::last_os_error()))
    }
}
#[cfg(target_os = "linux")]
fn rename_exchange(root: &fs::File, left: &str, right: &str) -> ReplayBoundaryResult<()> {
    use std::{ffi::CString, os::fd::AsRawFd};
    let left_c = CString::new(left)
        .map_err(|_| ReplayBoundaryError::unsafe_(left, "NUL in temporary name"))?;
    let right_c = CString::new(right)
        .map_err(|_| ReplayBoundaryError::unsafe_(right, "NUL in source name"))?;
    if unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            root.as_raw_fd(),
            left_c.as_ptr(),
            root.as_raw_fd(),
            right_c.as_ptr(),
            2_u32,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(ReplayBoundaryError::io(right, io::Error::last_os_error()))
    }
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn rename_exchange(_root: &fs::File, _left: &str, _right: &str) -> ReplayBoundaryResult<()> {
    Err(ReplayBoundaryError::Unsupported("atomic exchange rename"))
}
fn sha256(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(windows)]
    volume: u32,
    #[cfg(windows)]
    index: u64,
}
fn file_identity(m: &cap_fs::Metadata) -> Option<FileIdentity> {
    #[cfg(unix)]
    {
        use cap_fs::MetadataExt;
        Some(FileIdentity {
            dev: m.dev(),
            ino: m.ino(),
        })
    }
    #[cfg(windows)]
    {
        use cap_fs::_WindowsByHandle;
        Some(FileIdentity {
            volume: m.volume_serial_number()?,
            index: m.file_index()?,
        })
    }
}
fn link_count(m: &cap_fs::Metadata) -> Option<u64> {
    #[cfg(unix)]
    {
        use cap_fs::MetadataExt;
        Some(m.nlink())
    }
    #[cfg(windows)]
    {
        use cap_fs::_WindowsByHandle;
        m.number_of_links().map(u64::from)
    }
}
fn direct_entries(
    root: &fs::File,
    public: &Path,
    limit: usize,
) -> ReplayBoundaryResult<Vec<DirectEntry>> {
    let mut observed = Vec::new();
    let mut total_bytes = 0_u64;
    for entry in
        cap_fs::read_dir(root, Path::new(".")).map_err(|e| ReplayBoundaryError::io(public, e))?
    {
        let entry = entry.map_err(|e| ReplayBoundaryError::io(public, e))?;
        if observed.len() >= limit {
            return Err(ReplayBoundaryError::unsafe_(
                public,
                "direct entry count exceeds bound",
            ));
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        normal_component(&name, "directory entry name")?;
        let metadata = cap_fs::stat(root, Path::new(&name), cap_fs::FollowSymlinks::No)
            .map_err(|e| ReplayBoundaryError::io(public.join(&name), e))?;
        if metadata.file_type().is_file() && link_count(&metadata) == Some(1) {
            total_bytes = total_bytes.checked_add(metadata.len()).ok_or_else(|| {
                ReplayBoundaryError::unsafe_(public, "direct-entry byte accounting overflow")
            })?;
            if total_bytes > MAX_DIRECT_ENTRY_TOTAL_BYTES {
                return Err(ReplayBoundaryError::unsafe_(
                    public,
                    "direct-entry aggregate byte bound exceeded",
                ));
            }
            let bytes = regular_at(root, &name, MAX_SOURCE_BYTES, public)?;
            observed.push(DirectEntry {
                name,
                kind: DirectEntryKind::RegularFile,
                binding: Some(FileBinding {
                    sha256: sha256(&bytes),
                    bytes: bytes.len() as u64,
                }),
            });
        } else if metadata.file_type().is_dir() {
            observed.push(DirectEntry {
                name,
                kind: DirectEntryKind::Directory,
                binding: None,
            });
        } else {
            return Err(ReplayBoundaryError::unsafe_(
                public.join(&name),
                "direct entry must be a real directory or single-link regular file",
            ));
        }
    }
    observed.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(observed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    fn fixture() -> (tempfile::TempDir, Vec<String>) {
        let temp = tempfile::TempDir::new().unwrap();
        fs::create_dir(temp.path().join("scope")).unwrap();
        fs::write(temp.path().join("scope/a.md"), b"old\n").unwrap();
        (temp, vec!["scope".into()])
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn fresh_binding_and_mutation_preconditions() {
        let (temp, scopes) = fixture();
        let b = ReplayBoundary::bind(temp.path(), &scopes).unwrap();
        let s = b.scope("scope").unwrap();
        let before = sha256(b"old\n");
        let after = sha256(b"new\n");
        s.edit_text("a.md", "old", "new", &before, &after).unwrap();
        assert_eq!(fs::read(temp.path().join("scope/a.md")).unwrap(), b"new\n");
        let device = b.prepare_device().unwrap();
        device.delete_verified(s, "a.md", &after).unwrap();
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn initialized_authority_rejects_config_replacement_and_in_place_mutation() {
        let (temp, scopes) = fixture();
        let boundary = ReplayBoundary::bind(temp.path(), &scopes).unwrap();
        let kio = temp.path().join("scope/.kio");
        fs::create_dir(&kio).unwrap();
        let config = kio.join("config.toml");
        fs::write(&config, b"").unwrap();
        let authority = boundary
            .scope("scope")
            .unwrap()
            .bind_initialized_authority()
            .unwrap();
        authority.recheck().unwrap();
        fs::write(&config, b"changed = true\n").unwrap();
        assert!(authority.recheck().is_err());

        let (temp, scopes) = fixture();
        let boundary = ReplayBoundary::bind(temp.path(), &scopes).unwrap();
        let kio = temp.path().join("scope/.kio");
        fs::create_dir(&kio).unwrap();
        let config = kio.join("config.toml");
        fs::write(&config, b"").unwrap();
        let authority = boundary
            .scope("scope")
            .unwrap()
            .bind_initialized_authority()
            .unwrap();
        let replacement = kio.join("replacement.toml");
        fs::write(&replacement, b"").unwrap();
        fs::rename(&replacement, &config).unwrap();
        assert!(authority.recheck().is_err());
    }
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn unsupported_platform_preflight_does_not_mutate_fresh_corpus() {
        let (temp, _scopes) = fixture();
        let root_entries = fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        let source = fs::read(temp.path().join("scope/a.md")).unwrap();
        assert!(matches!(
            ReplayBoundary::preflight_platform(),
            Err(ReplayBoundaryError::Unsupported(_))
        ));
        assert_eq!(
            fs::read_dir(temp.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>(),
            root_entries
        );
        assert_eq!(fs::read(temp.path().join("scope/a.md")).unwrap(), source);
        assert!(!temp.path().join(DEVICE_DIR).exists());
        assert!(!temp.path().join(HISTORY_MANIFEST).exists());
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn publication_is_create_only() {
        let (temp, scopes) = fixture();
        let b = ReplayBoundary::bind(temp.path(), &scopes).unwrap();
        let device = b.prepare_device().unwrap();
        device.stage_history_manifest(&b, b"discard\n").unwrap();
        assert!(fs::read_dir(temp.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("history-manifest.json.")
        }));
        assert!(
            device
                .stage_history_manifest(&b, b"rollback\n")
                .unwrap()
                .inject_post_rename_failure()
                .publish()
                .is_err()
        );
        assert!(!temp.path().join(HISTORY_MANIFEST).exists());
        device
            .stage_history_manifest(&b, b"{}\n")
            .unwrap()
            .publish()
            .unwrap();
        assert!(
            device
                .stage_history_manifest(&b, b"bad")
                .unwrap()
                .publish()
                .is_err()
        );
        assert_eq!(
            fs::read(temp.path().join(HISTORY_MANIFEST)).unwrap(),
            b"{}\n"
        );
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn publication_rejects_post_sync_replacement_without_deleting_it() {
        let (temp, scopes) = fixture();
        let boundary = ReplayBoundary::bind(temp.path(), &scopes).unwrap();
        let device = boundary.prepare_device().unwrap();
        let replacement = b"forged evidence\n".to_vec();
        let result = device
            .stage_history_manifest(&boundary, b"expected evidence\n")
            .unwrap()
            .inject_post_root_sync_replacement(replacement.clone())
            .publish();
        assert!(matches!(
            result,
            Err(ReplayBoundaryError::Indeterminate { .. })
        ));
        assert_eq!(
            fs::read(temp.path().join(HISTORY_MANIFEST)).unwrap(),
            replacement
        );
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn publication_rejects_post_sync_removal_without_success() {
        let (temp, scopes) = fixture();
        let boundary = ReplayBoundary::bind(temp.path(), &scopes).unwrap();
        let device = boundary.prepare_device().unwrap();
        let result = device
            .stage_history_manifest(&boundary, b"expected evidence\n")
            .unwrap()
            .inject_post_root_sync_removal()
            .publish();
        assert!(matches!(
            result,
            Err(ReplayBoundaryError::Indeterminate { .. })
        ));
        assert!(!temp.path().join(HISTORY_MANIFEST).exists());
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn descriptor_backed_device_environment_survives_public_home_swap() {
        use std::os::unix::fs::symlink;
        let (temp, scopes) = fixture();
        let b = ReplayBoundary::bind(temp.path(), &scopes).unwrap();
        let d = b.prepare_device().unwrap();
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "test -z \"${GEMINI_API_KEY+x}\" && printf retained > \"$HOME/proof\"",
        ]);
        command.env("GEMINI_API_KEY", "must-not-be-inherited");
        d.configure_hermetic_environment(&mut command).unwrap();
        let home = temp.path().join(DEVICE_DIR).join("home");
        let retained = temp.path().join(DEVICE_DIR).join("old-home");
        let victim = tempfile::TempDir::new().unwrap();
        fs::rename(&home, &retained).unwrap();
        symlink(victim.path(), &home).unwrap();
        assert!(command.status().unwrap().success());
        assert_eq!(fs::read(retained.join("proof")).unwrap(), b"retained");
        assert!(fs::read_dir(victim.path()).unwrap().next().is_none());
    }
    #[cfg(unix)]
    #[test]
    fn rejects_symlink_and_hardlink_sources() {
        use std::os::unix::fs::symlink;
        let (temp, scopes) = fixture();
        let outside = tempfile::TempDir::new().unwrap();
        symlink(outside.path(), temp.path().join("scope/link.md")).unwrap();
        let b = ReplayBoundary::bind(temp.path(), &scopes).unwrap();
        let s = b.scope("scope").unwrap();
        assert!(s.read_file("link.md", "x").is_err());
        fs::hard_link(
            temp.path().join("scope/a.md"),
            temp.path().join("scope/hard.md"),
        )
        .unwrap();
        assert!(s.read_file("hard.md", &sha256(b"old\n")).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn rejects_final_leaf_symlink_corpus_without_touching_victim() {
        use std::os::unix::fs::symlink;
        let victim = tempfile::TempDir::new().unwrap();
        let holder = tempfile::TempDir::new().unwrap();
        let alias = holder.path().join("corpus");
        symlink(victim.path(), &alias).unwrap();
        assert!(ReplayBoundary::bind(&alias, &["scope".into()]).is_err());
        assert!(fs::read_dir(victim.path()).unwrap().next().is_none());
    }
    #[cfg(unix)]
    #[test]
    fn rejects_final_leaf_symlink_binary_without_touching_victim() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let (temp, scopes) = fixture();
        let boundary = ReplayBoundary::bind(temp.path(), &scopes).unwrap();
        let device = boundary.prepare_device().unwrap();
        let victim = tempfile::TempDir::new().unwrap();
        let victim_binary = victim.path().join("victim-kio");
        fs::write(&victim_binary, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&victim_binary, fs::Permissions::from_mode(0o700)).unwrap();
        let alias = temp.path().join("kio-alias");
        symlink(&victim_binary, &alias).unwrap();
        assert!(device.snapshot_executable(&alias).is_err());
        assert!(!temp.path().join(DEVICE_DIR).join("bin/kio").exists());
    }
    #[cfg(unix)]
    #[test]
    fn rejects_scope_path_replacement() {
        use std::os::unix::fs::symlink;
        let (temp, scopes) = fixture();
        let b = ReplayBoundary::bind(temp.path(), &scopes).unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        fs::rename(temp.path().join("scope"), temp.path().join("old")).unwrap();
        symlink(outside.path(), temp.path().join("scope")).unwrap();
        assert!(b.verify_fresh().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_replaced_private_device_children_before_writes() {
        use std::os::unix::fs::symlink;
        let (temp, scopes) = fixture();
        let b = ReplayBoundary::bind(temp.path(), &scopes).unwrap();
        let device = b.prepare_device().unwrap();
        let victim = tempfile::TempDir::new().unwrap();
        let bin = temp.path().join(DEVICE_DIR).join("bin");
        fs::remove_dir(&bin).unwrap();
        symlink(victim.path(), &bin).unwrap();
        assert!(device.recheck().is_err());
        let binary = std::env::current_exe().unwrap();
        assert!(device.snapshot_executable(&binary).is_err());
        assert!(!victim.path().join("kio").exists());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_replaced_trash_before_delete() {
        use std::os::unix::fs::symlink;
        let (temp, scopes) = fixture();
        let b = ReplayBoundary::bind(temp.path(), &scopes).unwrap();
        let device = b.prepare_device().unwrap();
        let victim = tempfile::TempDir::new().unwrap();
        let trash = temp.path().join(DEVICE_DIR).join("trash");
        fs::remove_dir(&trash).unwrap();
        symlink(victim.path(), &trash).unwrap();
        let scope = b.scope("scope").unwrap();
        assert!(
            device
                .delete_verified(scope, "a.md", &sha256(b"old\n"))
                .is_err()
        );
        assert!(temp.path().join("scope/a.md").exists());
        assert!(fs::read_dir(victim.path()).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn bound_executable_rejects_original_replacement() {
        use std::os::unix::fs::PermissionsExt;
        let (temp, scopes) = fixture();
        let binary = temp.path().join("kio-under-test");
        fs::write(&binary, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        let boundary = ReplayBoundary::bind(temp.path(), &scopes).unwrap();
        let device = boundary.prepare_device().unwrap();
        let bound = device.snapshot_executable(&binary).unwrap();
        let replacement = temp.path().join("replacement");
        fs::write(&replacement, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&replacement, fs::Permissions::from_mode(0o700)).unwrap();
        fs::rename(&replacement, &binary).unwrap();
        assert!(bound.recheck_original().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn executable_source_rejects_hardlinks() {
        use std::os::unix::fs::PermissionsExt;
        let (temp, scopes) = fixture();
        let binary = temp.path().join("kio-under-test");
        fs::write(&binary, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        let cargo_peer = temp.path().join("deps-kio-under-test");
        fs::hard_link(&binary, &cargo_peer).unwrap();
        let boundary = ReplayBoundary::bind(temp.path(), &scopes).unwrap();
        let device = boundary.prepare_device().unwrap();
        assert!(device.snapshot_executable(&binary).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn bound_executable_rejects_same_content_private_replacement() {
        use std::os::unix::fs::PermissionsExt;
        let (temp, scopes) = fixture();
        let binary = temp.path().join("kio-under-test");
        let script = b"#!/bin/sh\nexit 0\n";
        fs::write(&binary, script).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        let boundary = ReplayBoundary::bind(temp.path(), &scopes).unwrap();
        let device = boundary.prepare_device().unwrap();
        let bound = device.snapshot_executable(&binary).unwrap();
        let private = temp.path().join(DEVICE_DIR).join("bin/kio");
        fs::remove_file(&private).unwrap();
        fs::write(&private, script).unwrap();
        fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(bound.recheck_original().is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bound_executable_command_executes_retained_copy_and_rejects_replacement() {
        let (temp, scopes) = fixture();
        let binary_dir = tempfile::tempdir().unwrap();
        let binary = binary_dir.path().join("kio-eval-under-test");
        fs::write(&binary, b"#!/bin/sh\n[ \"$1\" = \"--list\" ]\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        let boundary = ReplayBoundary::bind(temp.path(), &scopes).unwrap();
        let device = boundary.prepare_device().unwrap();
        let bound = device.snapshot_executable(&binary).unwrap();
        assert_eq!(
            unsafe { libc::pwrite(bound.sealed_fd(), b"X".as_ptr().cast(), 1, 0,) },
            -1
        );
        assert!(matches!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        ));
        let output = bound.command().unwrap().arg("--list").output().unwrap();
        assert!(output.status.success());
        let private = temp.path().join(DEVICE_DIR).join("bin/kio");
        fs::remove_file(&private).unwrap();
        fs::write(&private, b"replacement").unwrap();
        fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(bound.command().is_err());
    }
}
