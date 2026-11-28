use clap::Args as ClapArgs;

#[derive(Debug, ClapArgs)]
pub struct Args {}

pub fn run(args: Args) -> Result<(), String> {
    #[cfg(unix)]
    {
        unix::run(args).map_err(|message| {
            use std::io::Write;
            let (code, response) = if lease_holder(&message).is_some() {
                ("LEASE_HELD", message)
            } else {
                let code = error_code(&message);
                (code, error_response(code))
            };
            let _ = std::io::stdout().lock().write_all(response.as_bytes());
            code.into()
        })
    }
    #[cfg(not(unix))]
    {
        let _ = args;
        Err("GPU dispatcher is supported only on Linux".into())
    }
}

const ERROR_CODES: &[(&str, &str)] = &[
    ("deployment path is missing", "DEPLOYMENT_MISSING"),
    ("deployment is not JSON", "DEPLOYMENT_INVALID"),
    ("deployment contract differs", "DEPLOYMENT_INVALID"),
    (
        "deployment source list differs",
        "DEPLOYMENT_INPUT_MISMATCH",
    ),
    ("installed source hash differs", "DEPLOYMENT_INPUT_MISMATCH"),
    (
        "installed shared GPU query hash differs",
        "DEPLOYMENT_INPUT_MISMATCH",
    ),
    (
        "tools executable differs from deployment",
        "DEPLOYMENT_INPUT_MISMATCH",
    ),
    ("GPU dispatcher is busy", "DISPATCHER_BUSY"),
    (
        "cannot inspect shared Docker GPU containers",
        "SHARED_GPU_QUERY_FAILED",
    ),
    ("GPU is busy", "GPU_BUSY"),
    (
        "cannot inspect dedicated Docker containers",
        "DEDICATED_ENGINE_QUERY_FAILED",
    ),
    ("GPU VRAM is insufficient", "GPU_VRAM_INSUFFICIENT"),
    ("OCR compose failed", "OCR_COMPOSE_FAILED"),
    ("GPU host setup is in progress", "MAINTENANCE_BUSY"),
    (
        "private dispatcher root is unavailable",
        "PRIVATE_ROOT_UNAVAILABLE",
    ),
    ("GPU lease is already held", "LEASE_HELD"),
    ("controller init failed", "CONTROLLER_INIT_FAILED"),
    ("controller start-ocr failed", "CONTROLLER_START_OCR_FAILED"),
    ("controller stop-ocr failed", "CONTROLLER_STOP_OCR_FAILED"),
    ("controller cleanup failed", "CONTROLLER_CLEANUP_FAILED"),
    ("GPU sample unavailable", "GPU_SAMPLE_UNAVAILABLE"),
    (
        "candidate is not installed deployment",
        "CANDIDATE_MISMATCH",
    ),
];
const REJECTED: &str = "DISPATCHER_REJECTED";

#[cfg(unix)]
fn error_code(message: &str) -> &'static str {
    ERROR_CODES
        .iter()
        .find_map(|&(detail, code)| (detail == message).then_some(code))
        .unwrap_or(REJECTED)
}

#[cfg(unix)]
fn error_response(code: &'static str) -> String {
    format!("{{\"code\":\"{code}\"}}\n")
}

pub(super) fn response_code(stdout: &str) -> Option<&'static str> {
    if lease_holder(stdout).is_some() {
        return Some("LEASE_HELD");
    }
    let received = stdout.strip_prefix("{\"code\":\"")?.strip_suffix("\"}\n")?;
    ERROR_CODES
        .iter()
        .map(|&(_, code)| code)
        .chain([REJECTED])
        .find(|&code| code == received)
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LeaseHolder<'a> {
    pub run_id: &'a str,
    pub attempt: &'a str,
    pub os: &'a str,
    code: &'a str,
}

pub(super) fn lease_holder(stdout: &str) -> Option<LeaseHolder<'_>> {
    if !stdout.ends_with('\n') || stdout.len() > 128 {
        return None;
    }
    let holder: LeaseHolder<'_> = serde_json::from_str(stdout).ok()?;
    (holder.code == "LEASE_HELD"
        && positive_decimal(holder.run_id, 19)
        && positive_decimal(holder.attempt, 10)
        && matches!(holder.os, "linux" | "macos" | "windows"))
    .then_some(holder)
}

fn positive_decimal(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.as_bytes()[0] != b'0'
        && value.bytes().all(|byte| byte.is_ascii_digit())
}

#[cfg(all(unix, test))]
mod protocol_tests {
    use super::*;

    #[test]
    fn only_fixed_errors_round_trip() {
        for &(detail, code) in ERROR_CODES {
            assert_eq!(error_code(detail), code);
            let reply = error_response(code);
            assert!(reply.len() < 64);
            assert_eq!(response_code(&reply), Some(code));
            assert_eq!(response_code(reply.trim_end()), None);
        }
        assert_eq!(error_code("unexpected private detail"), REJECTED);
        assert_eq!(response_code(&error_response(REJECTED)), Some(REJECTED));
    }
}

#[cfg(unix)]
mod unix {
    use super::super::route_preflight::{
        FIXED_SOURCES, MAX_BINARY_BYTES, MAX_FIXED_BYTES, SCHEMA, checked_digest, checked_open,
        valid_sha,
    };
    use super::{Args, positive_decimal};
    use kio_core::{
        private_fs::{
            read_private_file_at, verify_private_directory, verify_private_directory_handle,
        },
        store_dir::{Publication, StoreDirectory},
    };
    use serde::{Deserialize, Serialize};
    use std::{
        env, fs,
        io::Read,
        os::unix::{
            fs::{MetadataExt, OpenOptionsExt},
            io::AsRawFd,
        },
        path::{Path, PathBuf},
        process::{Command, Stdio},
        time::Duration,
    };
    use subtle::ConstantTimeEq;

    const ROOT: &str = "/var/lib/kio-v1-local/state";
    const SOURCE: &str = "/var/lib/kio-v1-local/source";
    const MAINTENANCE_LOCK: &str = "/var/lib/kio-v1-local/maintenance.lock";
    const SHARED_GPU_QUERY: &str = "/var/lib/kio-v1-local/shared-gpu-count";
    const SHARED_GPU_QUERY_SOURCE: &str = "scripts/v1-local-gpu/host/shared-gpu-count";
    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct Deployment {
        schema: String,
        candidate: String,
        repo_rel: String,
        tools_binary_sha256: String,
        #[serde(deserialize_with = "unique_sources")]
        sources: std::collections::BTreeMap<String, String>,
    }
    fn unique_sources<'de, D>(
        deserializer: D,
    ) -> Result<std::collections::BTreeMap<String, String>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{Error, MapAccess, Visitor};
        struct Sources;
        impl<'de> Visitor<'de> for Sources {
            type Value = std::collections::BTreeMap<String, String>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("deployment source digests with unique paths")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut sources = Self::Value::new();
                while let Some((path, digest)) = map.next_entry()? {
                    if sources.insert(path, digest).is_some() {
                        return Err(A::Error::custom("duplicate deployment source path"));
                    }
                }
                Ok(sources)
            }
        }
        deserializer.deserialize_map(Sources)
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct LeaseTuple {
        candidate: String,
        run_id: String,
        attempt: String,
        os: String,
    }
    #[derive(Clone, Debug)]
    struct Request {
        verb: String,
        tuple: LeaseTuple,
    }
    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct Lease {
        schema: String,
        tuple: LeaseTuple,
        capability_sha256: String,
    }
    const LEASE_SCHEMA: &str = "kio.v1.local_gpu.lease/v1";
    const STATE_SCHEMA: &str = "kio.v1.local_gpu.dispatch-state/v1";
    fn valid(raw: &str) -> bool {
        if raw.len() > 128 {
            return false;
        }
        let p: Vec<_> = raw.split(' ').collect();
        p.len() == 6
            && p[0] == "v1"
            && matches!(
                p[1],
                "init" | "identity" | "start-ocr" | "stop-ocr" | "finish"
            )
            && valid_sha(p[2], 40)
            && positive_decimal(p[3], 19)
            && positive_decimal(p[4], 10)
            && matches!(p[5], "linux" | "macos" | "windows")
    }
    fn same_capability(expected: &str, supplied: &str) -> bool {
        valid_sha(expected, 64)
            && bool::from(expected.as_bytes().ct_eq(token_digest(supplied).as_bytes()))
    }
    fn relative_present(base: &StoreDirectory, path: &Path) -> Result<bool, String> {
        let parts = path.components().collect::<Vec<_>>();
        let mut parent = base.clone();
        for (index, component) in parts.iter().enumerate() {
            let std::path::Component::Normal(name) = component else {
                return Err("invalid run path".into());
            };
            let leaf = Path::new(name);
            if !parent
                .contains_entry(leaf)
                .map_err(|_| "run root unavailable")?
            {
                return Ok(false);
            }
            if index + 1 == parts.len() {
                return Ok(true);
            }
            parent = retained_private_dir(&parent, leaf)?;
        }
        Err("empty run path".into())
    }
    fn request(raw: &str) -> Result<Request, String> {
        if !valid(raw) {
            return Err("invalid forced-command grammar".into());
        }
        let p: Vec<_> = raw.split(' ').collect();
        Ok(Request {
            verb: p[1].into(),
            tuple: LeaseTuple {
                candidate: p[2].into(),
                run_id: p[3].into(),
                attempt: p[4].into(),
                os: p[5].into(),
            },
        })
    }
    fn canonical<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
        let mut bytes =
            serde_jcs::to_vec(value).map_err(|_| "canonical encoding failed".to_string())?;
        bytes.push(b'\n');
        Ok(bytes)
    }
    fn private_dir(base: &StoreDirectory, relative: &Path) -> Result<StoreDirectory, String> {
        let mut directory = base.clone();
        for component in relative.components() {
            let std::path::Component::Normal(name) = component else {
                return Err("private directory path is invalid".into());
            };
            let leaf = Path::new(name);
            let handle = if directory
                .contains_entry(leaf)
                .map_err(|_| "private directory unavailable")?
            {
                directory.open_directory(leaf)
            } else {
                directory.create_directory(leaf)
            }
            .map_err(|_| "private directory unavailable")?;
            directory = StoreDirectory::from_retained(handle, directory.path().join(leaf))
                .map_err(|_| "private directory unavailable")?;
            verify_private_directory_handle(&directory)
                .map_err(|_| "private directory check failed")?;
        }
        Ok(directory)
    }
    fn retained_private_dir(
        base: &StoreDirectory,
        relative: &Path,
    ) -> Result<StoreDirectory, String> {
        let handle = base
            .open_directory(relative)
            .map_err(|_| "private directory unavailable".to_string())?;
        let directory = StoreDirectory::from_retained(handle, base.path().join(relative))
            .map_err(|_| "private directory unavailable".to_string())?;
        verify_private_directory_handle(&directory)
            .map_err(|_| "private directory check failed".to_string())?;
        Ok(directory)
    }
    fn retained_private_child(
        parent: &StoreDirectory,
        leaf: &str,
    ) -> Result<StoreDirectory, String> {
        retained_private_dir(parent, Path::new(leaf))
    }
    fn bootstrap_gate(base: &StoreDirectory) -> Result<(), String> {
        if base
            .contains_entry(Path::new(".dispatcher.lock"))
            .map_err(|_| "private lock unavailable")?
        {
            return Ok(());
        }
        match base.write_atomic(Path::new(".dispatcher.lock"), b"", Publication::CreateOnly) {
            Ok(()) => Ok(()),
            Err(_)
                if base
                    .contains_entry(Path::new(".dispatcher.lock"))
                    .map_err(|_| "private lock unavailable".to_string())? =>
            {
                Ok(())
            }
            Err(_) => Err("private lock unavailable".into()),
        }
    }
    fn strict_canonical_without_lf(bytes: &[u8], label: &str) -> Result<serde_json::Value, String> {
        let value = serde_json::from_slice(bytes).map_err(|_| format!("{label} invalid"))?;
        if serde_jcs::to_vec(&value).map_err(|_| format!("{label} invalid"))? != bytes {
            return Err(format!("{label} is not canonical JSON"));
        }
        Ok(value)
    }
    fn create_private(parent: &StoreDirectory, leaf: &str, bytes: &[u8]) -> Result<(), String> {
        verify_private_directory_handle(parent)
            .map_err(|_| "private directory check failed".to_string())?;
        parent
            .write_atomic(Path::new(leaf), bytes, Publication::CreateOnly)
            .map_err(|_| "create-only record already exists".to_string())
    }
    fn write_new<T: Serialize>(
        parent: &StoreDirectory,
        leaf: &str,
        value: &T,
    ) -> Result<(), String> {
        create_private(parent, leaf, &canonical(value)?)
    }
    fn replace_state(root: &StoreDirectory, request: &Request, phase: &str) -> Result<(), String> {
        verify_private_directory_handle(root)
            .map_err(|_| "private directory check failed".to_string())?;
        root.write_atomic(
            Path::new("dispatcher-state.json"),
            &canonical(
                &serde_json::json!({"schema":STATE_SCHEMA,"tuple":request.tuple,"phase":phase}),
            )?,
            Publication::Replace,
        )
        .map_err(|_| "state replacement failed".to_string())
    }
    fn private_json<T: for<'a> Deserialize<'a> + Serialize>(
        parent: &StoreDirectory,
        leaf: &str,
    ) -> Result<(T, Vec<u8>), String> {
        let bytes = read_private_file_at(parent, leaf, 65536)
            .map_err(|_| "required private record is absent".to_string())?;
        let v: T = serde_json::from_slice(&bytes).map_err(|_| "record is not JSON".to_string())?;
        if canonical(&v)? != bytes {
            return Err("record is not canonical JSON".into());
        }
        Ok((v, bytes))
    }
    fn current_state(root: &StoreDirectory, request: &Request) -> Result<String, String> {
        let (v, _) = private_json::<serde_json::Value>(root, "dispatcher-state.json")?;
        if v.get("schema").and_then(|x| x.as_str()) != Some(STATE_SCHEMA)
            || v.get("tuple") != Some(&serde_json::to_value(&request.tuple).unwrap())
        {
            return Err("state does not belong to this lease".into());
        }
        v.get("phase")
            .and_then(|x| x.as_str())
            .map(str::to_owned)
            .ok_or("state phase is invalid".into())
    }
    fn token_digest(token: &str) -> String {
        use sha2::Digest;
        let mut h = sha2::Sha256::new();
        h.update(token.as_bytes());
        h.finalize().iter().map(|x| format!("{x:02x}")).collect()
    }
    fn capability() -> Result<String, String> {
        let fd = std::io::stdin().as_raw_fd();
        let mut out = [0u8; 66];
        let mut n = 0;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let remain = deadline.saturating_duration_since(std::time::Instant::now());
            if remain.is_zero() {
                return Err("lease capability stdin timed out".into());
            }
            let mut set = unsafe { std::mem::zeroed::<libc::fd_set>() };
            unsafe {
                libc::FD_ZERO(&mut set);
                libc::FD_SET(fd, &mut set)
            };
            let mut tv = libc::timeval {
                tv_sec: remain.as_secs() as _,
                tv_usec: remain.subsec_micros() as _,
            };
            if unsafe {
                libc::select(
                    fd + 1,
                    &mut set,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut tv,
                )
            } <= 0
            {
                return Err("lease capability stdin timed out".into());
            };
            let got = unsafe { libc::read(fd, out[n..].as_mut_ptr().cast(), 66 - n) };
            if got < 0 {
                return Err("lease capability unreadable".into());
            }
            if got == 0 {
                break;
            }
            n += got as usize;
            if n > 65 {
                return Err("lease capability stdin has extra payload".into());
            }
        }
        if n != 64 && (n != 65 || out[64] != b'\n') {
            return Err("lease capability stdin has extra payload".into());
        }
        let s = std::str::from_utf8(&out[..64])
            .map_err(|_| "lease capability malformed".to_string())?;
        if !valid_sha(s, 64) {
            return Err("lease capability malformed".into());
        }
        Ok(s.into())
    }
    fn require_root_owned(path: &Path) -> Result<(), String> {
        for path in path.ancestors() {
            let metadata = fs::symlink_metadata(path).map_err(|_| "deployment path is missing")?;
            if metadata.is_symlink() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
                return Err("deployment path is not root controlled".into());
            }
        }
        Ok(())
    }

    fn deployment() -> Result<Deployment, String> {
        let p = Path::new(SOURCE).join("scripts/v1-local-gpu/deployment.json");
        require_root_owned(&p)?;
        let mut b = Vec::new();
        checked_open(&p, 65536)?
            .take(65537)
            .read_to_end(&mut b)
            .map_err(|_| "cannot read installed deployment".to_string())?;
        if b.len() > 65536 {
            return Err("installed deployment exceeds its size limit".into());
        }
        let d: Deployment =
            serde_json::from_slice(&b).map_err(|_| "deployment is not JSON".to_string())?;
        if d.schema != SCHEMA
            || d.repo_rel != "../.."
            || !valid_sha(&d.candidate, 40)
            || !valid_sha(&d.tools_binary_sha256, 64)
            || d.sources.len() != FIXED_SOURCES.len()
        {
            return Err("deployment contract differs".into());
        }
        for s in FIXED_SOURCES {
            let h = d.sources.get(*s).ok_or("deployment source list differs")?;
            let path = Path::new(SOURCE).join(s);
            require_root_owned(&path)?;
            if !valid_sha(h, 64) || checked_digest(&path, MAX_FIXED_BYTES)? != *h {
                return Err("installed source hash differs".into());
            }
        }
        let query = Path::new(SHARED_GPU_QUERY);
        require_root_owned(query)?;
        let query_hash = d
            .sources
            .get(SHARED_GPU_QUERY_SOURCE)
            .ok_or("shared GPU query is not in the deployment")?;
        if checked_digest(query, MAX_FIXED_BYTES)? != *query_hash {
            return Err("installed shared GPU query hash differs".into());
        }
        let exe = env::current_exe().map_err(|_| "cannot locate tools executable".to_string())?;
        let expected = Path::new(SOURCE).join("scripts/v1-local-gpu/kio-acceptance-tools");
        require_root_owned(expected.as_path())?;
        if exe != expected || checked_digest(&exe, MAX_BINARY_BYTES)? != d.tools_binary_sha256 {
            return Err("tools executable differs from deployment".into());
        }
        Ok(d)
    }
    fn controller_error(action: &str, status: Option<i32>) -> &'static str {
        match (action, status) {
            (_, Some(42)) => "GPU sample unavailable",
            ("init" | "start-ocr", Some(43)) => "cannot inspect shared Docker GPU containers",
            ("init" | "start-ocr", Some(44)) => "GPU is busy",
            ("init" | "start-ocr", Some(45)) => "cannot inspect dedicated Docker containers",
            ("start-ocr", Some(46)) => "GPU VRAM is insufficient",
            ("start-ocr", Some(47)) => "OCR compose failed",
            ("init", _) => "controller init failed",
            ("start-ocr", _) => "controller start-ocr failed",
            ("stop-ocr", _) => "controller stop-ocr failed",
            ("cleanup", _) => "controller cleanup failed",
            _ => "controller phase is invalid",
        }
    }

    fn invoke(root: &Path, action: &str) -> Result<(), String> {
        let mut c = Command::new("/bin/bash");
        c.arg(Path::new(SOURCE).join("scripts/v1-local-gpu/gpu-phase.sh"))
            .arg(action)
            .current_dir(Path::new(SOURCE).join("scripts/v1-local-gpu"))
            .env_clear()
            .env(
                "PATH",
                "/opt/kio-v1-local/docker:/usr/sbin:/usr/bin:/sbin:/bin",
            )
            .env("KIO_GPU_ROOT", root)
            .env("HOME", "/home/kio-test")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let o = kio_process::run_bounded_command(
            &mut c,
            kio_process::BoundedProcessOptions {
                timeout: Duration::from_secs(420),
                max_stdout_bytes: 16 * 1024,
                max_stderr_bytes: 16 * 1024,
            },
            None,
        )
        .map_err(|_| controller_error(action, None))?;
        if o.status.success() {
            Ok(())
        } else {
            Err(controller_error(action, o.status.code()).into())
        }
    }
    pub(super) fn run(_: Args) -> Result<(), String> {
        if !cfg!(target_os = "linux") {
            return Err("GPU dispatcher is supported only on Linux".into());
        }
        let command =
            env::var("SSH_ORIGINAL_COMMAND").map_err(|_| "missing forced command".to_string())?;
        let request = request(&command)?;
        // A client must finish its bounded capability input before holding
        // the shared GPU lease gate.
        let supplied = capability()?;
        let maintenance = Path::new(MAINTENANCE_LOCK);
        require_root_owned(maintenance)?;
        let _maintenance = maintenance_gate(maintenance, 0, unsafe { libc::getegid() })?;
        let d = deployment()?;
        if request.tuple.candidate != d.candidate {
            return Err("candidate is not installed deployment".into());
        }
        let base = verify_private_directory(Path::new(ROOT))
            .map_err(|_| "private dispatcher root is unavailable".to_string())?;
        let response = handle(&base, &request, Some(&supplied), &mut invoke)?;
        use std::io::Write;
        std::io::stdout()
            .write_all(&response)
            .map_err(|_| "cannot write dispatcher response".into())
    }

    fn maintenance_gate(path: &Path, owner: u32, group: u32) -> Result<DispatcherGate, String> {
        let lock = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(|_| "maintenance lock is unavailable")?;
        let metadata = lock
            .metadata()
            .map_err(|_| "cannot inspect maintenance lock")?;
        if !metadata.is_file()
            || metadata.uid() != owner
            || metadata.gid() != group
            || metadata.mode() & 0o7777 != 0o640
        {
            return Err(
                "maintenance lock must be owned by root and readable only by the dedicated group"
                    .into(),
            );
        }
        lock.try_lock_shared().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => "GPU host setup is in progress",
            std::fs::TryLockError::Error(_) => "cannot acquire maintenance lock",
        })?;
        Ok(DispatcherGate(lock))
    }

    struct DispatcherGate(std::fs::File);
    impl Drop for DispatcherGate {
        fn drop(&mut self) {
            // A forked child can retain the same open file description after
            // this handler closes its descriptor. Explicit unlock releases
            // this handler's gate even while that child has not exec'd/exited.
            let _ = self.0.unlock();
        }
    }

    fn handle(
        base: &StoreDirectory,
        request: &Request,
        supplied: Option<&str>,
        controller: &mut impl FnMut(&Path, &str) -> Result<(), String>,
    ) -> Result<Vec<u8>, String> {
        // The caller durably owns this secret before any remote effect. Losing
        // the init response therefore cannot lose the authority to clean up.
        let capability = supplied
            .filter(|value| valid_sha(value, 64))
            .ok_or("lease capability is absent or invalid")?;
        bootstrap_gate(base)?;
        let lock = base
            .open_regular_read(Path::new(".dispatcher.lock"), 0)
            .map_err(|_| "private lock unavailable".to_string())?;
        base.ensure_owner_private(Path::new(".dispatcher.lock"))
            .map_err(|_| "private lock unavailable".to_string())?;
        if lock
            .metadata()
            .map_err(|_| "private lock unavailable".to_string())?
            .len()
            != 0
        {
            return Err("private lock unavailable".into());
        }
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            return Err(if error.kind() == std::io::ErrorKind::WouldBlock {
                "GPU dispatcher is busy".into()
            } else {
                format!("cannot acquire GPU dispatcher lock: {error}")
            });
        }
        let _gate = DispatcherGate(lock);
        let root_relative = PathBuf::from("runs")
            .join(&request.tuple.candidate)
            .join(&request.tuple.run_id)
            .join(&request.tuple.attempt)
            .join(&request.tuple.os);
        if request.verb == "init" {
            if base
                .contains_entry(Path::new("lease.json"))
                .map_err(|_| "lease unavailable")?
            {
                if let Ok((lease, _)) = private_json::<Lease>(base, "lease.json")
                    && lease.schema == LEASE_SCHEMA
                    && positive_decimal(&lease.tuple.run_id, 19)
                    && positive_decimal(&lease.tuple.attempt, 10)
                    && matches!(lease.tuple.os.as_str(), "linux" | "macos" | "windows")
                {
                    let reply = serde_json::json!({
                        "attempt": lease.tuple.attempt, "code": "LEASE_HELD",
                        "os": lease.tuple.os, "run_id": lease.tuple.run_id,
                    });
                    return Err(format!("{reply}\n"));
                }
                return Err("GPU lease is already held".into());
            }
            if relative_present(base, &root_relative)? {
                return Err("run root is create-only".into());
            }
            // No controller can run until both the run state and lease exist.
            // A pre-lease filesystem failure leaves only an unused run root.
            let root = private_dir(base, &root_relative)?;
            write_new(
                &root,
                "dispatcher-state.json",
                &serde_json::json!({"schema":STATE_SCHEMA,"tuple":request.tuple,"phase":"initializing"}),
            )?;
            write_new(
                base,
                "lease.json",
                &serde_json::json!({"schema":LEASE_SCHEMA,"tuple":request.tuple,"capability_sha256":token_digest(capability)}),
            )?;
            if let Err(e) = (|| {
                controller(root.path(), "init")?;
                replace_state(&root, request, "ready")
            })() {
                if root
                    .contains_entry(Path::new("dispatcher-state.json"))
                    .unwrap_or(false)
                {
                    let _ = replace_state(&root, request, "unknown");
                }
                return Err(e);
            }
            return canonical(&serde_json::json!({"status":"ok"}));
        }
        let (lease, lease_bytes) = private_json::<Lease>(base, "lease.json")?;
        if lease.schema != LEASE_SCHEMA
            || lease.tuple != request.tuple
            || !same_capability(&lease.capability_sha256, capability)
        {
            return Err("GPU lease belongs to a different job".into());
        }
        let root = retained_private_dir(base, &root_relative)?;
        let phase = current_state(&root, request)?;
        if request.verb == "identity" {
            if !matches!(phase.as_str(), "ready" | "ocr-running" | "ocr-stopped") {
                return Err("identity is forbidden in this phase".into());
            };
            let state = retained_private_child(&root, "state")?;
            let tls = retained_private_child(&root, "tls-server")?;
            let identity = read_private_file_at(&state, "identity.json", 65536)
                .map_err(|_| "public identity unavailable".to_string())?;
            let identity: serde_json::Value =
                strict_canonical_without_lf(&identity, "public identity")?;
            let ca = read_private_file_at(&tls, "ca-cert.pem", 65536)
                .map_err(|_| "public CA unavailable".to_string())?;
            let ca = String::from_utf8(ca).map_err(|_| "public CA unavailable".to_string())?;
            let mut observed = Vec::new();
            let observed_file = "ocr-observed.json";
            if state
                .contains_entry(Path::new(observed_file))
                .unwrap_or(false)
            {
                observed.push(private_json::<serde_json::Value>(&state, observed_file)?.0);
            }
            let response = serde_jcs::to_string(
                &serde_json::json!({"identity":identity,"observed":observed,"public_ca_pem":ca}),
            )
            .map_err(|_| "response encoding failed".to_string())?;
            if response.len() >= 65536 {
                return Err("public identity response exceeds limit".into());
            }
            return Ok(format!("{response}\n").into_bytes());
        }
        if request.verb == "finish" {
            if !matches!(
                phase.as_str(),
                "initializing"
                    | "ready"
                    | "unknown"
                    | "ocr-starting"
                    | "ocr-running"
                    | "ocr-stopping"
                    | "ocr-stopped"
                    | "finishing"
                    | "finished"
            ) {
                return Err("finish is forbidden in this phase".into());
            };
            replace_state(&root, request, "finishing")?;
            if let Err(e) = controller(root.path(), "cleanup") {
                let _ = replace_state(&root, request, "unknown");
                return Err(e);
            };
            replace_state(&root, request, "finished")?;
            let (lease_check, lease_check_bytes) = private_json::<Lease>(base, "lease.json")?;
            if lease_check.schema != LEASE_SCHEMA
                || lease_check.tuple != request.tuple
                || !same_capability(&lease_check.capability_sha256, capability)
                || lease_check_bytes != lease_bytes
            {
                return Err("GPU lease changed before removal".into());
            }
            base.quarantine_then_remove(Path::new("lease.json"), &lease_bytes, 65536)
                .map_err(|_| "lease removal failed".to_string())?;
            return Ok(b"ok\n".to_vec());
        }
        let (before, action, after) = match (request.verb.as_str(), phase.as_str()) {
            ("start-ocr", "ready") => ("ocr-starting", "start-ocr", "ocr-running"),
            ("stop-ocr", "ocr-running") => ("ocr-stopping", "stop-ocr", "ocr-stopped"),
            _ => return Err("controller phase is invalid".into()),
        };
        replace_state(&root, request, before)?;
        if let Err(e) = controller(root.path(), action) {
            let _ = replace_state(&root, request, "unknown");
            return Err(e);
        };
        replace_state(&root, request, after)?;
        Ok(b"ok\n".to_vec())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::acceptance_tools::dispatch::response_code;

        fn fixture() -> (tempfile::TempDir, StoreDirectory) {
            let temp = tempfile::tempdir().unwrap();
            use std::os::unix::fs::DirBuilderExt;
            let path = temp.path().canonicalize().unwrap().join("private");
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .unwrap();
            let directory = verify_private_directory(&path).unwrap();
            (temp, directory)
        }
        fn req(verb: &str) -> Request {
            request(&format!("v1 {verb} {} 12 1 linux", "a".repeat(40))).unwrap()
        }
        fn init(base: &StoreDirectory, calls: &mut Vec<String>) -> String {
            let cap = "b".repeat(64);
            let raw = handle(base, &req("init"), Some(&cap), &mut |_, action| {
                calls.push(action.to_owned());
                Ok(())
            })
            .unwrap();
            let value: serde_json::Value = serde_json::from_slice(&raw).unwrap();
            assert_eq!(canonical(&value).unwrap(), raw);
            assert_eq!(value, serde_json::json!({"status":"ok"}));
            cap
        }
        // Only the parent executes Rust after fork. The child retains the
        // dispatcher's open file description while blocked in an async-safe
        // read; it never execs, allocates, unwinds, or runs Rust destructors.
        struct InheritedGateChild {
            pid: libc::pid_t,
            _read: std::os::unix::net::UnixStream,
            _write: std::os::unix::net::UnixStream,
        }
        impl InheritedGateChild {
            fn spawn() -> Self {
                let (read, write) = std::os::unix::net::UnixStream::pair().unwrap();
                let read_fd = read.as_raw_fd();
                for fd in [read_fd, write.as_raw_fd()] {
                    // SAFETY: each stream owns this live descriptor; F_GETFD
                    // reads flags without changing the process or descriptor.
                    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
                    assert!(flags >= 0, "cannot inspect fixture descriptor");
                    assert_ne!(flags & libc::FD_CLOEXEC, 0);
                }
                // SAFETY: child uses only read and _exit before termination.
                // All test assertions and resource cleanup remain in parent.
                let pid = unsafe { libc::fork() };
                if pid == 0 {
                    let mut byte = 0u8;
                    loop {
                        // Retain the inherited gate even if a signal interrupts
                        // read. The parent guard always terminates this child.
                        unsafe {
                            if libc::read(read_fd, (&mut byte as *mut u8).cast(), 1) == 1 {
                                libc::_exit(0);
                            }
                        }
                    }
                }
                assert!(pid > 0, "fork failed: {}", std::io::Error::last_os_error());
                Self {
                    pid,
                    _read: read,
                    _write: write,
                }
            }
        }
        impl Drop for InheritedGateChild {
            fn drop(&mut self) {
                // Always terminate/reap even if the regression assertion
                // panics. No socket EOF dependency on other concurrent forks.
                unsafe {
                    libc::kill(self.pid, libc::SIGKILL);
                }
                loop {
                    let result = unsafe { libc::waitpid(self.pid, std::ptr::null_mut(), 0) };
                    if result >= 0
                        || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                    {
                        break;
                    }
                }
            }
        }
        #[test]
        fn fork_inherited_gate_does_not_block_finish() {
            let (_temp, base) = fixture();
            let cap = "b".repeat(64);
            let mut child = None;
            handle(&base, &req("init"), Some(&cap), &mut |_, action| {
                assert_eq!(action, "init");
                child = Some(InheritedGateChild::spawn());
                Ok(())
            })
            .unwrap();
            let child = child.expect("controller must fork while gate is held");
            assert_eq!(
                handle(&base, &req("finish"), Some(&cap), &mut |_, action| {
                    assert_eq!(action, "cleanup");
                    Ok(())
                })
                .unwrap(),
                b"ok\n"
            );
            assert!(!base.contains_entry(Path::new("lease.json")).unwrap());
            drop(child);
        }
        #[test]
        fn setup_and_dispatch_exclude_each_other() {
            let (temp, _) = fixture();
            let path = temp.path().join("maintenance.lock");
            use std::os::unix::fs::PermissionsExt;
            let setup = fs::File::create(&path).unwrap();
            let meta = setup.metadata().unwrap();
            let (owner, group) = (meta.uid(), meta.gid());
            setup
                .set_permissions(fs::Permissions::from_mode(0o644))
                .unwrap();
            assert!(
                maintenance_gate(&path, owner, group).is_err(),
                "dispatcher accepted a public lock"
            );
            setup
                .set_permissions(fs::Permissions::from_mode(0o640))
                .unwrap();
            assert!(maintenance_gate(&path, owner, group.wrapping_add(1)).is_err());
            let alias = temp.path().join("alias");
            std::os::unix::fs::symlink(&path, &alias).unwrap();
            assert!(maintenance_gate(&alias, owner, group).is_err());
            setup.try_lock().unwrap();
            assert!(maintenance_gate(&path, owner, group).is_err());
            setup.unlock().unwrap();
            let dispatch = maintenance_gate(&path, owner, group).unwrap();
            assert!(setup.try_lock().is_err());
            drop(dispatch);
            setup.try_lock().unwrap();
        }

        #[test]
        fn active_dispatcher_gate_rejects_concurrent_handle() {
            let (_temp, base) = fixture();
            let cap = "b".repeat(64);
            handle(&base, &req("init"), Some(&cap), &mut |_, _| {
                std::thread::scope(|scope| {
                    let result = scope
                        .spawn(|| {
                            handle(&base, &req("finish"), Some(&cap), &mut |_, _| {
                                panic!("busy controller invoked")
                            })
                        })
                        .join()
                        .unwrap();
                    assert_eq!(result.unwrap_err(), "GPU dispatcher is busy");
                });
                Ok(())
            })
            .unwrap();
        }
        #[test]
        fn forced_grammar_has_no_aliases_or_shell_fragments() {
            let good = format!("v1 init {} 12 1 linux", "a".repeat(40));
            assert!(valid(&good));
            for bad in [
                good.replace(" 12 ", " +12 "),
                good.replace(" 12 ", " 012 "),
                good.replace(" 1 linux", " 01 linux"),
                format!("{good}\n"),
                format!("{good};whoami"),
                good.replace("init", "cleanup"),
            ] {
                assert!(!valid(&bad));
            }
        }
        #[test]
        fn conflicting_init_reports_the_existing_holder() {
            let (_temp, base) = fixture();
            init(&base, &mut Vec::new());
            let other = request(&format!("v1 init {} 99 2 windows", "a".repeat(40))).unwrap();
            let error = handle(&base, &other, Some(&"c".repeat(64)), &mut |_, _| {
                panic!("conflicting init invoked the controller")
            })
            .unwrap_err();
            assert_eq!(
                error,
                "{\"attempt\":\"1\",\"code\":\"LEASE_HELD\",\"os\":\"linux\",\"run_id\":\"12\"}\n"
            );
            assert_eq!(response_code(&error), Some("LEASE_HELD"));
        }

        #[test]
        fn controller_errors_distinguish_action_and_sample_unavailability() {
            for (action, code) in [
                ("init", "CONTROLLER_INIT_FAILED"),
                ("start-ocr", "CONTROLLER_START_OCR_FAILED"),
                ("stop-ocr", "CONTROLLER_STOP_OCR_FAILED"),
                ("cleanup", "CONTROLLER_CLEANUP_FAILED"),
            ] {
                assert_eq!(
                    super::super::error_code(controller_error(action, None)),
                    code
                );
                assert_eq!(
                    super::super::error_code(controller_error(action, Some(42))),
                    "GPU_SAMPLE_UNAVAILABLE"
                );
            }
        }

        #[test]
        fn deployment_format_allows_whitespace_but_refuses_duplicate_source_paths() {
            let flexible = r#"{
                "sources": {"file": "digest"}, "candidate": "abc",
                "repo_rel": "../..", "schema": "schema", "tools_binary_sha256": "digest"
            }"#;
            assert!(serde_json::from_str::<Deployment>(flexible).is_ok());
            let duplicate =
                flexible.replace(r#""file": "digest""#, r#""file": "old", "file": "digest""#);
            assert!(serde_json::from_str::<Deployment>(&duplicate).is_err());
        }

        #[test]
        fn start_ocr_prechecks_report_fixed_codes() {
            for (status, code) in [
                (43, "SHARED_GPU_QUERY_FAILED"),
                (44, "GPU_BUSY"),
                (45, "DEDICATED_ENGINE_QUERY_FAILED"),
                (46, "GPU_VRAM_INSUFFICIENT"),
                (47, "OCR_COMPOSE_FAILED"),
            ] {
                assert_eq!(
                    super::super::error_code(controller_error("start-ocr", Some(status))),
                    code
                );
            }
        }

        #[test]
        fn wrong_capability_or_tuple_cannot_invoke_controller() {
            let (_temp, base) = fixture();
            let mut calls = Vec::new();
            let cap = init(&base, &mut calls);
            let before = read_private_file_at(&base, "lease.json", 65536).unwrap();
            for candidate in [
                None,
                Some("wrong"),
                Some("0000000000000000000000000000000000000000000000000000000000000000"),
            ] {
                assert!(
                    handle(&base, &req("start-ocr"), candidate, &mut |_, action| {
                        calls.push(action.to_owned());
                        Ok(())
                    })
                    .is_err()
                );
            }
            let mut wrong = req("start-ocr");
            wrong.tuple.run_id = "13".into();
            assert!(
                handle(&base, &wrong, Some(&cap), &mut |_, _| panic!(
                    "wrong tuple executed"
                ))
                .is_err()
            );
            assert_eq!(calls, ["init"]);
            assert_eq!(
                read_private_file_at(&base, "lease.json", 65536).unwrap(),
                before
            );
        }
        #[test]
        fn ordered_phases_and_finish_remove_only_the_owned_lease() {
            let (_temp, base) = fixture();
            let mut calls = Vec::new();
            let cap = init(&base, &mut calls);
            assert!(
                handle(&base, &req("init"), None, &mut |_, _| panic!(
                    "duplicate init"
                ))
                .is_err()
            );
            for verb in ["start-ocr", "stop-ocr", "finish"] {
                assert_eq!(
                    handle(&base, &req(verb), Some(&cap), &mut |_, action| {
                        calls.push(action.to_owned());
                        Ok(())
                    })
                    .unwrap(),
                    b"ok\n"
                );
            }
            assert!(!base.contains_entry(Path::new("lease.json")).unwrap());
            assert_eq!(calls, ["init", "start-ocr", "stop-ocr", "cleanup"]);
        }
        #[test]
        fn failed_controller_cannot_reexecute_and_cleanup_must_succeed() {
            let (_temp, base) = fixture();
            let cap = init(&base, &mut Vec::new());
            assert!(
                handle(&base, &req("start-ocr"), Some(&cap), &mut |_, _| Err(
                    "simulated failure".into()
                ))
                .is_err()
            );
            assert!(
                handle(&base, &req("start-ocr"), Some(&cap), &mut |_, _| panic!(
                    "replayed uncertain effect"
                ))
                .is_err()
            );
            assert!(
                handle(&base, &req("finish"), Some(&cap), &mut |_, action| {
                    assert_eq!(action, "cleanup");
                    Err("cleanup cannot prove absence".into())
                })
                .is_err()
            );
            assert!(base.contains_entry(Path::new("lease.json")).unwrap());
            handle(&base, &req("finish"), Some(&cap), &mut |_, action| {
                assert_eq!(action, "cleanup");
                Ok(())
            })
            .unwrap();
            assert!(!base.contains_entry(Path::new("lease.json")).unwrap());
        }
        #[test]
        fn lost_init_response_and_failed_init_keep_client_cleanup_authority() {
            for init_fails in [false, true] {
                let (_temp, base) = fixture();
                let cap = "c".repeat(64);
                let result = handle(&base, &req("init"), Some(&cap), &mut |_, action| {
                    assert_eq!(action, "init");
                    if init_fails {
                        Err("init result unknown".into())
                    } else {
                        Ok(())
                    }
                });
                assert_eq!(result.is_err(), init_fails);
                // Deliberately discard the response; the preexisting client
                // secret, not anything returned by init, authorizes cleanup.
                drop(result);
                handle(&base, &req("finish"), Some(&cap), &mut |_, action| {
                    assert_eq!(action, "cleanup");
                    Ok(())
                })
                .unwrap();
                assert!(!base.contains_entry(Path::new("lease.json")).unwrap());
            }
        }
        #[test]
        fn host_bootstrap_accepts_helpers_without_a_deployment() {
            use std::os::unix::fs::PermissionsExt;

            let (temp, _) = fixture();
            let gpu = temp.path().join("scripts/v1-local-gpu");
            fs::create_dir_all(gpu.join("host")).unwrap();
            let helper = gpu.join("kio-acceptance-tools");
            let query = gpu.join("host/shared-gpu-count");
            for path in [&helper, &query] {
                fs::write(path, b"fixture").unwrap();
                fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
            }

            let setup = include_str!("../../../../scripts/v1-local-gpu/host/setup-wsl.sh");
            let (_, setup) = setup
                .split_once("# Begin bootstrap payload validation.")
                .unwrap();
            let (preflight, _) = setup
                .split_once("# End bootstrap payload validation.")
                .unwrap();
            let shell = format!(
                "set -euo pipefail\nsource=$1\nquery_source=$2\n\
                 die() {{ echo \"$*\" >&2; exit 1; }}\n\
                 trusted_path() {{ [[ -f $1 && ! -L $1 ]]; }}\n{preflight}"
            );
            let run = || {
                Command::new("/bin/bash")
                    .arg("-c")
                    .arg(&shell)
                    .arg("bootstrap-fixture")
                    .arg(temp.path())
                    .arg(&query)
                    .output()
                    .unwrap()
            };
            let missing = gpu.join("deployment.json");
            assert_eq!(
                require_root_owned(&missing),
                Err("deployment path is missing".into())
            );
            let result = run();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );

            fs::remove_file(&query).unwrap();
            assert!(!run().status.success());
            fs::write(&query, b"fixture").unwrap();
            fs::set_permissions(&query, fs::Permissions::from_mode(0o755)).unwrap();
            fs::remove_file(&helper).unwrap();
            assert!(!run().status.success());
        }

        #[test]
        fn docker_checks_use_the_dedicated_engine_and_fail_closed() {
            use std::os::unix::fs::PermissionsExt;
            let (temp, base) = fixture();
            let stub = temp.path().join("docker");
            std::fs::write(
                &stub,
                br#"#!/bin/bash
[ -z "${DOCKER_HOST+x}${DOCKER_CONTEXT+x}" ] || exit 40
case "$1 $2" in
  '--host unix:///run/user/'*'/kio-v1-local-docker.sock') shift 2 ;;
  *) exit 41 ;;
esac
[ "$1 $2" = "--config /opt/kio-v1-local/docker-cli" ] || exit 42
shift 2
case "$*" in
  'ps -a --filter label=com.docker.compose.project=kio-v1-local-gpu-ocr -q')
    case "$KIO_FIXTURE_DOCKER" in
      empty|shared-cleanup|broken-cleanup) exit 0 ;;
      container) echo retained-container ;;
      *) exit 7 ;;
    esac ;;
  'ps -q') if [ "$KIO_FIXTURE_DOCKER" = foreign ]; then echo running-container; fi ;;
  *DeviceRequests*) echo null ;;
  *com.docker.compose.project*running-container) echo foreign-project ;;
  *) exit 43 ;;
esac
"#,
            )
            .unwrap();
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o700)).unwrap();
            let sudo = temp.path().join("sudo");
            std::fs::write(
                &sudo,
                br#"#!/bin/bash
[ "$*" = "-n -- /var/lib/kio-v1-local/shared-gpu-count" ] || exit 31
case "$KIO_FIXTURE_DOCKER" in
  shared*) echo 1 ;;
  malformed*) echo invalid ;;
  broken*) exit 1 ;;
  *) echo 0 ;;
esac
"#,
            )
            .unwrap();
            std::fs::set_permissions(&sudo, std::fs::Permissions::from_mode(0o700)).unwrap();
            let source = include_str!("../../../../scripts/v1-local-gpu/gpu-phase.sh");
            let definitions = source.split_once("case ${1:-} in").unwrap().0;
            let script = temp.path().join("docker-fixture.sh");
            for (mode, operation, expected_error) in [
                (
                    "empty",
                    r#"cleanup_project "$OCR_PROJECT" "$ROOT/missing-compose.yaml""#,
                    None,
                ),
                (
                    "container",
                    r#"cleanup_project "$OCR_PROJECT" "$ROOT/missing-compose.yaml""#,
                    Some("has containers but no verified compose file"),
                ),
                (
                    "unavailable",
                    r#"cleanup_project "$OCR_PROJECT" "$ROOT/missing-compose.yaml""#,
                    Some("cannot inspect Docker"),
                ),
                ("foreign", "gpu_guard", Some("unrelated container")),
                ("shared", "gpu_guard", Some("shared Docker GPU container")),
                ("shared-cleanup", "cleanup", None),
                ("broken-cleanup", "cleanup", None),
                (
                    "malformed",
                    "gpu_guard",
                    Some("invalid shared Docker GPU count"),
                ),
                (
                    "broken",
                    "gpu_guard",
                    Some("cannot inspect shared Docker GPU containers"),
                ),
            ] {
                std::fs::write(
                    &script,
                    format!("{definitions}\nlock() {{ :; }}\nprivate_dir() {{ mkdir -p \"$1\"; }}\ngpu_memory() {{ return 1; }}\n{operation}\n"),
                )
                .unwrap();
                let mut command = Command::new("/bin/bash");
                command
                    .arg(&script)
                    .env_clear()
                    .env("PATH", format!("{}:/usr/bin:/bin", temp.path().display()))
                    .env("KIO_GPU_ROOT", base.path())
                    .env("KIO_FIXTURE_DOCKER", mode)
                    .env("DOCKER_HOST", "unix:///var/run/docker.sock")
                    .env("DOCKER_CONTEXT", "shared");
                let result = kio_process::run_bounded_command(
                    &mut command,
                    kio_process::BoundedProcessOptions {
                        timeout: Duration::from_secs(5),
                        max_stdout_bytes: 4096,
                        max_stderr_bytes: 4096,
                    },
                    None,
                )
                .unwrap();
                assert_eq!(
                    result.status.success(),
                    expected_error.is_none(),
                    "{mode}: {}",
                    result.stderr
                );
                if let Some(error) = expected_error {
                    assert!(result.stderr.contains(error), "{mode}: {}", result.stderr);
                }
            }
        }
    }
}
