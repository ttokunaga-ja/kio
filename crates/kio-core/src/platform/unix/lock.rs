use crate::ExitCode;
use crate::cas::lower_hex;
use crate::error::{IoResultExt, KioError, Result};
use crate::platform::exchange_bound_lock;
use crate::scope::{
    LockFile, RELEASED_LOCK_PID, canonical_lock_bytes, canonical_lock_bytes_for, new_lock_token,
    unix_nanos, unsafe_store_error,
};
use crate::store_dir::StoreDirectory;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
fn unlock_flock(file: &File) {
    use std::os::fd::AsRawFd;

    // Closing the local descriptor is normally sufficient, but a concurrent
    // fork may retain the same open-file description until its exec boundary.
    // Explicit unlock releases that shared kernel lock at the logical guard
    // boundary instead of extending it into the child process transiently.
    let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
}

pub(crate) struct BoundLockGate {
    pub(crate) file: File,
}

impl Drop for BoundLockGate {
    fn drop(&mut self) {
        unlock_flock(&self.file);
    }
}

fn reclaim_stale_lock(path: &Path, replacement: &[u8]) -> Result<bool> {
    {
        let Some(old) = read_lock_snapshot(path)? else {
            return Ok(false);
        };
        if crate::platform::process_is_alive(old.lock.pid) {
            return Ok(false);
        }
        let Some(again) = read_lock_snapshot(path)? else {
            return Ok(false);
        };
        if old != again || crate::platform::process_is_alive(again.lock.pid) {
            return Ok(false);
        }
        let parent = path
            .parent()
            .ok_or_else(|| KioError::io("lock path has no parent", path.display().to_string()))?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| KioError::locked(path.display().to_string()))?;
        let temp_name = format!(".{name}.reclaimed-{}-{}", std::process::id(), unix_nanos());
        let temp = parent.join(&temp_name);
        let mut temp_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .kio_io(&temp)?;
        temp_file.write_all(replacement).kio_io(&temp)?;
        temp_file.sync_all().kio_io(&temp)?;
        drop(temp_file);
        let Some(current) = read_lock_snapshot(path)? else {
            return Ok(false);
        };
        if current != old {
            return Ok(false);
        }
        let directory = File::open(parent).kio_io(parent)?;
        exchange_bound_lock(&directory, name, &directory, &temp_name)?;
        let expected_replacement = parse_canonical_lock(replacement)?;
        let active_ok = read_lock_snapshot(path).is_ok_and(|snapshot| {
            snapshot.is_some_and(|snapshot| {
                snapshot.bytes == replacement && snapshot.lock == expected_replacement
            })
        });
        let stale_ok =
            read_lock_snapshot(&temp).is_ok_and(|snapshot| snapshot == Some(old.clone()));
        if !active_ok || !stale_ok {
            // Both postconditions must hold before a compensating exchange could
            // be safe. Retain the names instead: no concurrent replacement is
            // removed or overwritten by a cleanup attempt.
            return Err(bound_lock_corrupt(
                "ordinary stale lock exchange validation failed",
            ));
        }
        fs::remove_file(&temp).kio_io(&temp)?;
        directory.sync_all().kio_io(parent)?;
        Ok(true)
    }
}

fn release_ordinary_lock(path: &Path, pid: u32, token: &str) -> Result<()> {
    {
        let Some(owned) = read_lock_snapshot(path)? else {
            return Ok(());
        };
        if owned.lock.pid != pid || owned.lock.token != token {
            return Ok(());
        }
        let parent = path
            .parent()
            .ok_or_else(|| KioError::io("lock path has no parent", path.display().to_string()))?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| KioError::locked(path.display().to_string()))?;
        let temp_name = format!(".{name}.released-{}-{}", std::process::id(), unix_nanos());
        let temp = parent.join(&temp_name);
        let sentinel = canonical_lock_bytes(RELEASED_LOCK_PID, &new_lock_token(RELEASED_LOCK_PID))?;
        let mut temp_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .kio_io(&temp)?;
        temp_file.write_all(&sentinel).kio_io(&temp)?;
        temp_file.sync_all().kio_io(&temp)?;
        drop(temp_file);
        if read_lock_snapshot(path)? != Some(owned.clone()) {
            return Ok(());
        }
        let directory = File::open(parent).kio_io(parent)?;
        exchange_bound_lock(&directory, name, &directory, &temp_name)?;
        let sentinel_lock = parse_canonical_lock(&sentinel)?;
        let active_ok = read_lock_snapshot(path).is_ok_and(|snapshot| {
            snapshot.is_some_and(|snapshot| {
                snapshot.bytes == sentinel && snapshot.lock == sentinel_lock
            })
        });
        let archived_ok = read_lock_snapshot(&temp).is_ok_and(|snapshot| snapshot == Some(owned));
        if !active_ok || !archived_ok {
            return Err(bound_lock_corrupt(
                "ordinary lock release exchange validation failed",
            ));
        }
        fs::remove_file(&temp).kio_io(&temp)?;
        directory.sync_all().kio_io(parent)
    }
}

pub(crate) fn process_is_alive(pid: u32) -> bool {
    if pid == RELEASED_LOCK_PID {
        return false;
    }
    if pid == std::process::id() {
        return true;
    }

    // PID zero names a process group rather than a process, and values above
    // pid_t's positive range cannot name a Unix process.
    if pid == 0 || pid > libc::pid_t::MAX as u32 {
        return false;
    }
    // SAFETY: signal zero performs no delivery. ESRCH is the sole definite
    // absence result; EPERM and every other error are conservatively live.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        true
    } else {
        std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
}

pub(crate) fn bound_lock_corrupt(message: impl Into<String>) -> KioError {
    KioError::new(
        "KIO-E-STORE-CORRUPT-001",
        message,
        json!({}),
        ExitCode::PermanentFailure,
    )
}

fn bound_lock_io(error: std::io::Error, path: impl Into<String>) -> KioError {
    let kind = match error.kind() {
        std::io::ErrorKind::AlreadyExists => "already_exists",
        std::io::ErrorKind::NotFound => "not_found",
        _ => "other",
    };
    KioError::new(
        "KIO-E-STORE-IO-001",
        error.to_string(),
        json!({"path": path.into(), "io_error_kind": kind}),
        ExitCode::Failure,
    )
}

fn preflight_bound_lock_entry(kio: &File) -> Result<()> {
    match read_bound_lock(kio, ".lock") {
        Ok((bytes, _)) => {
            let lock = parse_canonical_lock(&bytes).map_err(|_| KioError::locked(".kio/.lock"))?;
            if crate::platform::process_is_alive(lock.pid) {
                return Err(KioError::locked(".kio/.lock"));
            }
            // A dead, exact entry is re-read by the reclaim routine below
            // before it is exchanged.  Its presence authorizes preparing the
            // private archive capability; it does not authorize deletion.
            Ok(())
        }
        Err(error) if bound_is_not_found(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

fn acquire_bound_lock_gate(kio: &File) -> Result<BoundLockGate> {
    use std::os::fd::AsRawFd;

    // `try_clone` shares an open-file description with the caller on Unix;
    // that would keep flock held by the retained session FD after this guard
    // drops. Re-open `.` capability-relatively to obtain an independent
    // description while staying inside the already-bound `.kio` directory.
    let gate = open_bound_directory_for_io(kio, Path::new(".kio"))?;
    // SAFETY: this retained descriptor names the same `.kio` directory used
    // for every later capability-relative operation and outlives the lock.
    if unsafe { libc::flock(gate.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(libc::EWOULDBLOCK)) {
            return Err(KioError::locked(".kio/.lock"));
        }
        return Err(KioError::io(error.to_string(), ".kio"));
    }
    Ok(BoundLockGate { file: gate })
}

fn parse_canonical_lock(bytes: &[u8]) -> Result<LockFile> {
    let lock: LockFile =
        serde_json::from_slice(bytes).map_err(|_| KioError::locked(".kio/.lock"))?;
    if canonical_lock_bytes_for(&lock)? != bytes {
        return Err(KioError::locked(".kio/.lock"));
    }
    Ok(lock)
}

/// Strict ordinary-lock read used solely by stale recovery. The path based
/// API needs this one retained file descriptor check before its atomic
/// exchange; it never unlinks a name after a best-effort string comparison.
fn read_lock_snapshot(path: &Path) -> Result<Option<LockSnapshot>> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let before = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(KioError::io(error.to_string(), path.display().to_string())),
    };
    if !before.file_type().is_file() || before.nlink() != 1 || before.len() > MAX_BOUND_LOCK_BYTES {
        return Err(KioError::locked(path.display().to_string()));
    }
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(KioError::io(error.to_string(), path.display().to_string())),
    };
    let opened = file.metadata().kio_io(path)?;
    if opened.dev() != before.dev() || opened.ino() != before.ino() || opened.len() != before.len()
    {
        return Err(bound_lock_corrupt("ordinary lock changed while opening"));
    }
    let mut bytes = Vec::with_capacity(opened.len() as usize);
    (&mut file)
        .take(MAX_BOUND_LOCK_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .kio_io(path)?;
    let after = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(KioError::io(error.to_string(), path.display().to_string())),
    };
    if bytes.len() as u64 != opened.len()
        || after.dev() != opened.dev()
        || after.ino() != opened.ino()
        || after.len() != opened.len()
    {
        return Err(bound_lock_corrupt("ordinary lock changed while reading"));
    }
    let lock = parse_canonical_lock(&bytes)?;
    Ok(Some(LockSnapshot {
        lock,
        digest: lower_hex(&Sha256::digest(&bytes)),
        bytes,
        dev: opened.dev(),
        ino: opened.ino(),
        len: opened.len(),
    }))
}

fn open_bound_directory_for_io(directory: &File, label: impl AsRef<Path>) -> Result<File> {
    use cap_primitives::fs::{self as cap_fs, MetadataExt};

    let label = label.as_ref();
    let expected = cap_fs::Metadata::from_file(directory)
        .map_err(|error| KioError::io(error.to_string(), label.display().to_string()))?;
    if !expected.is_dir() {
        return Err(unsafe_store_error(label, "retained directory changed type"));
    }
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let syncable = cap_fs::open(directory, Path::new("."), &options)
        .map_err(|error| KioError::io(error.to_string(), label.display().to_string()))?;
    let observed = cap_fs::Metadata::from_file(&syncable)
        .map_err(|error| KioError::io(error.to_string(), label.display().to_string()))?;
    if !observed.is_dir() || observed.dev() != expected.dev() || observed.ino() != expected.ino() {
        return Err(unsafe_store_error(
            label,
            "retained directory changed while reopening",
        ));
    }
    Ok(syncable)
}

pub(crate) fn sync_bound_directory(directory: &File, label: impl AsRef<Path>) -> Result<()> {
    let label = label.as_ref();
    open_bound_directory_for_io(directory, label)?
        .sync_all()
        .map_err(|error| KioError::io(error.to_string(), label.display().to_string()))
}

fn bound_lock_archive_dir(kio: &File) -> Result<File> {
    use cap_primitives::fs as cap_fs;
    fn ensure(parent: &File, leaf: &str) -> Result<File> {
        use cap_primitives::fs as cap_fs;
        match cap_fs::open_dir_nofollow(parent, Path::new(leaf)) {
            Ok(dir) => Ok(dir),
            Err(_) => {
                cap_fs::create_dir(parent, Path::new(leaf), &cap_fs::DirOptions::new())
                    .map_err(|e| KioError::io(e.to_string(), leaf))?;
                let dir = cap_fs::open_dir_nofollow(parent, Path::new(leaf))
                    .map_err(|e| KioError::io(e.to_string(), leaf))?;
                sync_bound_directory(parent, leaf)?;
                Ok(dir)
            }
        }
    }
    // Stale writer-lock archival is generic store plumbing, never GC state:
    // normal writer acquisition must not create a `gc/` namespace.
    let internal = ensure(kio, "internal")?;
    let locks = ensure(&internal, "locks")?;
    // Re-open validation is descriptor-relative; `ensure` never follows a
    // public path and refuses a non-directory/reparse child.
    let _ = cap_fs::read_base_dir(&locks).map_err(|e| KioError::io(e.to_string(), "locks"))?;
    Ok(locks)
}

fn create_bound_lock(kio: &File, leaf: &str, bytes: &[u8]) -> Result<BoundLockObservation> {
    use cap_primitives::fs as cap_fs;
    let mut options = cap_fs::OpenOptions::new();
    options.write(true).create_new(true);
    options._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let mut file =
        cap_fs::open(kio, Path::new(leaf), &options).map_err(|e| bound_lock_io(e, leaf))?;
    file.write_all(bytes)
        .map_err(|e| KioError::io(e.to_string(), leaf))?;
    file.sync_all()
        .map_err(|e| KioError::io(e.to_string(), leaf))?;
    drop(file);
    let (actual, observed) = read_bound_lock(kio, leaf)?;
    if actual != bytes || parse_canonical_lock(&actual).is_err() {
        return Err(bound_lock_corrupt("GC lock changed after creation"));
    }
    sync_bound_directory(kio, leaf)?;
    Ok(observed)
}

fn read_bound_lock(kio: &File, leaf: &str) -> Result<(Vec<u8>, BoundLockObservation)> {
    use cap_primitives::fs::{self as cap_fs, MetadataExt};
    let path = Path::new(leaf);
    let before =
        cap_fs::stat(kio, path, cap_fs::FollowSymlinks::No).map_err(|e| bound_lock_io(e, leaf))?;
    validate_bound_lock_meta(&before)?;
    let mut options = cap_fs::OpenOptions::new();
    options.read(true);
    options._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let mut file = cap_fs::open(kio, path, &options).map_err(|e| bound_lock_io(e, leaf))?;
    let opened =
        cap_fs::Metadata::from_file(&file).map_err(|e| KioError::io(e.to_string(), leaf))?;
    validate_bound_lock_meta(&opened)?;
    if before.dev() != opened.dev() || before.ino() != opened.ino() || before.len() != opened.len()
    {
        return Err(bound_lock_corrupt("GC lock changed while opening"));
    }
    let mut bytes = Vec::with_capacity(opened.len() as usize);
    (&mut file)
        .take(MAX_BOUND_LOCK_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| KioError::io(e.to_string(), leaf))?;
    let after =
        cap_fs::stat(kio, path, cap_fs::FollowSymlinks::No).map_err(|e| bound_lock_io(e, leaf))?;
    validate_bound_lock_meta(&after)?;
    if bytes.len() as u64 != opened.len()
        || after.dev() != opened.dev()
        || after.ino() != opened.ino()
        || after.len() != opened.len()
    {
        return Err(bound_lock_corrupt("GC lock changed while reading"));
    }
    Ok((
        bytes.clone(),
        BoundLockObservation {
            dev: opened.dev(),
            ino: opened.ino(),
            len: opened.len(),
            digest: lower_hex(&Sha256::digest(&bytes)),
        },
    ))
}

fn validate_bound_lock_meta(metadata: &cap_primitives::fs::Metadata) -> Result<()> {
    use cap_primitives::fs::MetadataExt;
    if !metadata.is_file() || metadata.len() > MAX_BOUND_LOCK_BYTES || metadata.nlink() != 1 {
        return Err(bound_lock_corrupt("invalid GC lock entry"));
    }
    Ok(())
}

fn reclaim_stale_bound_lock(
    kio: &File,
    locks: &File,
    own_bytes: &[u8],
) -> Result<Option<BoundLockObservation>> {
    let (old_bytes, old) = match read_bound_lock(kio, ".lock") {
        Ok(value) => value,
        Err(error) if error.error_code() == "KIO-E-STORE-IO-001" => return Ok(None),
        Err(error) => return Err(error),
    };
    let old_lock = match parse_canonical_lock(&old_bytes) {
        Ok(lock) => lock,
        Err(_) => return Ok(None),
    };
    if crate::platform::process_is_alive(old_lock.pid) {
        return Ok(None);
    }
    // Read it twice before publishing a replacement. A same-content rename is
    // observable through identity and never reclaimed as our stale lock.
    let (again_bytes, again) = read_bound_lock(kio, ".lock")?;
    if again_bytes != old_bytes || again != old || crate::platform::process_is_alive(old_lock.pid) {
        return Ok(None);
    }
    let archive = bound_lock_name("stale");
    let new = create_bound_lock(locks, &archive, own_bytes)?;
    let source_unchanged = read_bound_lock(kio, ".lock")
        .is_ok_and(|(bytes, observed)| bytes == old_bytes && observed == old);
    if !source_unchanged {
        // The unpublished temporary is retained rather than check-then-unlinking
        // a name which a hostile writer could replace in the final gap.
        return Ok(None);
    }
    exchange_bound_lock(kio, ".lock", locks, &archive)?;
    let old_ok = read_bound_lock(locks, &archive)
        .is_ok_and(|(bytes, observed)| bytes == old_bytes && observed == old);
    let new_ok = read_bound_lock(kio, ".lock")
        .is_ok_and(|(bytes, observed)| bytes == own_bytes && observed == new);
    if !old_ok || !new_ok {
        // Never exchange back after a failed postcondition: at least one name
        // no longer designates the expected object, so rollback could move a
        // concurrent owner's entry. The canonical lock remains fail-closed.
        return Err(bound_lock_corrupt(
            "GC stale lock exchange validation failed",
        ));
    }
    cap_primitives::fs::remove_file(locks, Path::new(&archive))
        .map_err(|e| KioError::io(e.to_string(), &archive))?;
    sync_bound_directory(kio, ".lock")?;
    sync_bound_directory(locks, &archive)?;
    Ok(Some(new))
}

fn bound_lock_name(prefix: &str) -> String {
    format!("{prefix}-{}-{}", std::process::id(), unix_nanos())
}

fn bound_is_exists(error: &KioError) -> bool {
    error.error_code() == "KIO-E-STORE-IO-001"
        && error
            .context()
            .get("io_error_kind")
            .and_then(serde_json::Value::as_str)
            == Some("already_exists")
}

fn bound_is_not_found(error: &KioError) -> bool {
    error.error_code() == "KIO-E-STORE-IO-001"
        && error
            .context()
            .get("io_error_kind")
            .and_then(serde_json::Value::as_str)
            == Some("not_found")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundLockObservation {
    dev: u64,
    ino: u64,
    len: u64,
    digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LockSnapshot {
    lock: LockFile,
    bytes: Vec<u8>,
    dev: u64,
    ino: u64,
    len: u64,
    digest: String,
}

pub(crate) struct BoundLock {
    kio: File,
    pub(crate) _gate: BoundLockGate,
    locks: File,
    pid: u32,
    token: String,
    owned: BoundLockObservation,
    bytes: Vec<u8>,
}

impl Drop for BoundLock {
    fn drop(&mut self) {
        let Ok((bytes, observed)) = read_bound_lock(&self.kio, ".lock") else {
            return;
        };
        if bytes != self.bytes
            || observed != self.owned
            || !parse_canonical_lock(&bytes)
                .map(|lock| lock.pid == self.pid && lock.token == self.token)
                .unwrap_or(false)
        {
            return;
        }
        // Never create an absent-lock window and never move an entry that may
        // have replaced ours after the check above. Exchange our lock with a
        // canonical, deliberately dead release sentinel, then verify both
        // sides. The next writer reclaims that sentinel through the ordinary
        // stale-lock protocol. This is conservative (the lock may outlive this
        // guard briefly) but cannot remove a concurrent owner's replacement.
        let archive = bound_lock_name("released");
        let released_pid = RELEASED_LOCK_PID;
        let released_token = new_lock_token(released_pid);
        let Ok(released_bytes) = canonical_lock_bytes(released_pid, &released_token) else {
            return;
        };
        let Ok(released) = create_bound_lock(&self.locks, &archive, &released_bytes) else {
            return;
        };
        if !read_bound_lock(&self.kio, ".lock")
            .is_ok_and(|(actual, state)| actual == self.bytes && state == self.owned)
        {
            return;
        }
        if exchange_bound_lock(&self.kio, ".lock", &self.locks, &archive).is_err() {
            return;
        }
        let old_ok = read_bound_lock(&self.locks, &archive)
            .is_ok_and(|(actual, state)| actual == self.bytes && state == self.owned);
        let released_ok = read_bound_lock(&self.kio, ".lock")
            .is_ok_and(|(actual, state)| actual == released_bytes && state == released);
        if !old_ok || !released_ok {
            // Do not attempt a compensating exchange unless both entries are
            // still exact; in the mismatch branch that is intentionally never
            // true. Leaving a lock entry is fail-closed.
            return;
        }
        if cap_primitives::fs::remove_file(&self.locks, Path::new(&archive)).is_err() {
            return;
        }
        let _ = sync_bound_directory(&self.kio, ".lock");
        let _ = sync_bound_directory(&self.locks, &archive);
    }
}

pub(crate) struct ReadGate {
    kio: File,
    _gate: BoundLockGate,
    initial_lock: Option<(Vec<u8>, BoundLockObservation)>,
}

impl ReadGate {
    /// Recheck the public lock leaf while the shared kernel gate remains held.
    /// A byte-identical dead release sentinel is harmless; a live owner or any
    /// replacement is an uncertain writer boundary and therefore retryable.
    pub(crate) fn recheck_idle(&self) -> Result<()> {
        let current = match read_bound_lock(&self.kio, ".lock") {
            Ok((bytes, observation)) => {
                let lock =
                    parse_canonical_lock(&bytes).map_err(|_| KioError::locked(".kio/.lock"))?;
                if lock.pid != RELEASED_LOCK_PID {
                    return Err(KioError::locked(".kio/.lock"));
                }
                Some((bytes, observation))
            }
            Err(error) if bound_is_not_found(&error) => None,
            Err(error) => return Err(error),
        };
        if current != self.initial_lock {
            return Err(KioError::locked(".kio/.lock"));
        }
        Ok(())
    }
}

pub(crate) fn acquire_bound_lock(kio: &File, _store_gate: bool) -> Result<BoundLock> {
    let gate = acquire_bound_lock_gate(kio)?;
    let pid = std::process::id();
    let token = new_lock_token(pid);
    let bytes = canonical_lock_bytes(pid, &token)?;
    // Every fallible resource needed by `BoundLock::drop` is prepared
    // before the authoritative lock is published.  In particular, a broken
    // Generic `.kio/internal/locks` setup must not turn a failed acquisition into a live lock
    // that no guard owns to release.  Inspect the current entry first so a
    // live ordinary writer sees no GC-internal directory creation.
    preflight_bound_lock_entry(kio)?;
    let retained_kio = kio
        .try_clone()
        .map_err(|e| KioError::io(e.to_string(), ".kio"))?;
    let locks = bound_lock_archive_dir(kio)?;

    match create_bound_lock(kio, ".lock", &bytes) {
        Ok(owned) => Ok(BoundLock {
            kio: retained_kio,
            _gate: gate,
            locks,
            pid,
            token,
            owned,
            bytes,
        }),
        Err(error) if bound_is_exists(&error) => {
            if let Some(owned) = reclaim_stale_bound_lock(kio, &locks, &bytes)? {
                Ok(BoundLock {
                    kio: retained_kio,
                    _gate: gate,
                    locks,
                    pid,
                    token,
                    owned,
                    bytes,
                })
            } else {
                Err(KioError::locked(".kio/.lock"))
            }
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn acquire_bound_read_gate(kio: &File) -> Result<ReadGate> {
    use std::os::fd::AsRawFd;

    let gate = open_bound_directory_for_io(kio, Path::new(".kio"))?;
    // SAFETY: `gate` is owned by the returned guard, names the retained `.kio`
    // directory, and outlives every inventory read protected by this lock.
    if unsafe { libc::flock(gate.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(libc::EWOULDBLOCK)) {
            return Err(KioError::locked(".kio/.lock"));
        }
        return Err(KioError::io(error.to_string(), ".kio"));
    }
    let gate = BoundLockGate { file: gate };
    let initial_lock = match read_bound_lock(kio, ".lock") {
        Ok((bytes, observation)) => {
            let lock = parse_canonical_lock(&bytes).map_err(|_| KioError::locked(".kio/.lock"))?;
            if lock.pid != RELEASED_LOCK_PID {
                return Err(KioError::locked(".kio/.lock"));
            }
            Some((bytes, observation))
        }
        Err(error) if bound_is_not_found(&error) => None,
        Err(error) => return Err(error),
    };
    Ok(ReadGate {
        kio: kio
            .try_clone()
            .map_err(|error| KioError::io(error.to_string(), ".kio"))?,
        _gate: gate,
        initial_lock,
    })
}

const MAX_BOUND_LOCK_BYTES: u64 = 4096;
pub(crate) struct PathLock {
    path: PathBuf,
    pid: u32,
    token: String,
    pub(crate) _gate: crate::scope::locks::LockGate,
}
pub(crate) fn acquire_path_lock(
    path: PathBuf,
    pid: u32,
    _store_gate: bool,
    _validate_authority: fn(&File) -> Result<()>,
    acquire_gate: crate::scope::locks::AcquireLockGate,
) -> Result<PathLock> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).kio_io(parent)?;
    }

    let gate = acquire_gate(
        &path,
        super::scope_semantics::acquire_path_gate,
        super::scope_semantics::unlock_path_gate,
    )?;

    let token = new_lock_token(pid);
    let canonical = canonical_lock_bytes(pid, &token)?;
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            file.write_all(&canonical).kio_io(&path)?;
            file.sync_all().kio_io(&path)?;
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            if !reclaim_stale_lock(&path, &canonical)? {
                return Err(KioError::locked(path.display().to_string()));
            }
        }
        Err(err) => return Err(KioError::io(err.to_string(), path.display().to_string())),
    }
    Ok(PathLock {
        path,
        pid,
        token,
        _gate: gate,
    })
}
impl PathLock {
    pub(crate) fn release_outer(&self) {
        let _ = release_ordinary_lock(&self.path, self.pid, &self.token);
    }
}
pub(crate) fn validate_store_gate(directory: &StoreDirectory) -> Result<()> {
    match directory.read_optional(Path::new(".store-gate"), 0) {
        Ok(Some(bytes)) if bytes.is_empty() => Ok(()),
        _ => Err(KioError::locked(
            ".kio/.store-gate is missing, unsafe, or not empty",
        )),
    }
}
pub(crate) fn directory_lease_identity(
    kio: &File,
) -> Result<crate::platform::DirectoryHandleIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = kio
        .metadata()
        .map_err(|error| KioError::io(error.to_string(), ".kio"))?;
    Ok(crate::platform::DirectoryHandleIdentity::from_components(
        metadata.dev(),
        metadata.ino(),
    ))
}

pub(crate) type PathLockLease = crate::platform::PathLock;
pub(crate) fn finish_path_lock(lease: &mut Option<PathLockLease>, release_outer: bool) {
    if release_outer && let Some(native) = lease {
        native.release_outer();
    }
}
