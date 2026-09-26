//! Read-only installation proposal for the forced local-GPU route.
use clap::Args as ClapArgs;
use kio_process::{BoundedProcessOptions, BoundedStdin, run_bounded_command};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    io::Read,
    path::{Component, Path, PathBuf},
    process::Command,
    time::Duration,
};

pub const SCHEMA: &str = "kio.v1.local_gpu.deployment/v2";
pub const MAX_FIXED_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_BINARY_BYTES: u64 = 256 * 1024 * 1024;
pub(crate) const FIXED_SOURCES: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "crates/kio-eval/Cargo.toml",
    "crates/kio-eval/src/acceptance_tools_main.rs",
    "crates/kio-eval/src/acceptance_tools/mod.rs",
    "crates/kio-eval/src/acceptance_tools/dispatch.rs",
    "crates/kio-eval/src/acceptance_tools/route_preflight.rs",
    "crates/kio-eval/src/acceptance_tools/provider_ledger.rs",
    "crates/kio-eval/src/acceptance_tools/local_client.rs",
    "crates/kio-eval/src/acceptance_tools/prepare_embedding.rs",
    "crates/kio-eval/src/acceptance_tools/ocr_proxy.rs",
    "crates/kio-eval/src/acceptance_tools/gpu_identity.rs",
    "crates/kio-eval/src/acceptance_tools/gpu_memory.rs",
    "crates/kio-eval/src/acceptance_tools/package_extract.rs",
    "scripts/v1-local-gpu/gpu-phase.sh",
    "scripts/v1-local-gpu/ocr_api_entrypoint.sh",
    "scripts/v1-local-gpu/model-files.json",
    "tasks/artifacts/v1-gpu-4060/ocr/backend-config.yaml",
    "tasks/artifacts/v1-gpu-4060/ocr/compose.yaml",
    "tasks/artifacts/v1-gpu-4060/embedding/compose.yaml",
];
#[derive(Debug, ClapArgs)]
pub struct Args {
    #[arg(long)]
    pub repository: PathBuf,
    #[arg(long)]
    pub candidate: String,
    #[arg(long)]
    pub tools_binary: PathBuf,
}
pub fn valid_sha(v: &str, n: usize) -> bool {
    v.len() == n
        && v.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub fn lower_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
fn checked_open(path: &Path, max: u64) -> Result<fs::File, String> {
    if !path.is_absolute() {
        return Err("fixed path must be absolute".into());
    };
    let parent = path.parent().ok_or("fixed source has no parent")?;
    let leaf = path.file_name().ok_or("fixed source has no leaf")?;
    let directory = kio_core::private_fs::verify_private_creation_parent(parent)
        .map_err(|_| "fixed source ancestry is unsafe")?;
    let f = directory
        .open_regular_read(Path::new(leaf), max)
        .map_err(|_| "fixed source is missing or unsafe")?;
    let m = f.metadata().map_err(|_| "cannot inspect fixed source")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if m.mode() & 0o022 != 0 || (m.uid() != unsafe { libc::geteuid() } && m.uid() != 0) {
            return Err("unsafe fixed source".into());
        }
    }
    #[cfg(windows)]
    {
        let _ = m;
        kio_core::private_fs::verify_trusted_executable(path)
            .map_err(|_| "fixed source ACL is unsafe")?;
    }
    Ok(f)
}
pub fn checked_digest(path: &Path, max: u64) -> Result<String, String> {
    let mut f = checked_open(path, max)?;
    let mut h = Sha256::new();
    let mut left = max;
    let mut b = [0; 65536];
    loop {
        let n = f
            .read(&mut b)
            .map_err(|_| "fixed source unreadable".to_string())?;
        if n == 0 {
            break;
        }
        left = left.checked_sub(n as u64).ok_or("fixed source too large")?;
        h.update(&b[..n]);
    }
    Ok(lower_hex(&h.finalize()))
}
fn trusted_git() -> Result<PathBuf, String> {
    let path = env::var_os("PATH").ok_or("trusted git executable unavailable")?;
    for parent in env::split_paths(&path) {
        if !parent.is_absolute() {
            continue;
        }
        for name in git_names() {
            let candidate = parent.join(name);
            let Ok(candidate) = candidate.canonicalize() else {
                continue;
            };
            if !candidate.is_absolute() {
                continue;
            }
            if kio_core::private_fs::verify_trusted_executable(&candidate).is_ok() {
                return Ok(candidate);
            }
        }
    }
    Err("trusted git executable unavailable".into())
}

#[cfg(windows)]
fn git_names() -> &'static [&'static str] {
    &["git.exe"]
}

#[cfg(not(windows))]
fn git_names() -> &'static [&'static str] {
    &["git"]
}

fn controlled_repository(repo: &Path) -> Result<PathBuf, String> {
    let repo = repo
        .canonicalize()
        .map_err(|_| "repository unavailable".to_string())?;
    kio_core::private_fs::verify_private_creation_parent(&repo)
        .map_err(|_| "repository ancestry is unsafe")?;
    // Git reads this directory for every proof. Retain the same no-follow,
    // trusted-ancestor policy instead of treating a worktree pathname as proof.
    kio_core::private_fs::verify_private_creation_parent(&repo.join(".git"))
        .map_err(|_| "repository metadata is unsafe")?;
    Ok(repo)
}

fn git(git: &Path, repo: &Path, args: &[&str], input: Option<Vec<u8>>) -> Result<String, String> {
    let mut c = Command::new(git);
    c.env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", git_null_config())
        .env("GIT_CONFIG_COUNT", "0")
        .env("LC_ALL", "C")
        .arg("--no-optional-locks")
        .arg("-c")
        .arg("core.fsmonitor=false")
        .arg("-c")
        .arg(format!("core.hooksPath={}", git_null_config()))
        .args(args)
        .current_dir(repo);
    #[cfg(windows)]
    {
        let system_root = env::var_os("SystemRoot").ok_or("git preflight failed")?;
        c.env("SystemRoot", system_root);
    }
    let o = run_bounded_command(
        &mut c,
        BoundedProcessOptions {
            timeout: Duration::from_secs(10),
            max_stdout_bytes: 1024 * 1024,
            max_stderr_bytes: 4096,
        },
        input.map(|v| BoundedStdin::new(v, MAX_FIXED_BYTES as usize)),
    )
    .map_err(|_| "git preflight failed".to_string())?;
    if !o.status.success() {
        return Err("git preflight failed".into());
    }
    Ok(o.stdout)
}

#[cfg(windows)]
fn git_null_config() -> &'static str {
    "NUL"
}

#[cfg(not(windows))]
fn git_null_config() -> &'static str {
    "/dev/null"
}
fn clean_path(p: &str) -> bool {
    !Path::new(p).is_absolute()
        && Path::new(p)
            .components()
            .all(|x| matches!(x, Component::Normal(_)))
}
pub fn preflight(repo: &Path, candidate: &str, binary: &Path) -> Result<serde_json::Value, String> {
    if !valid_sha(candidate, 40) {
        return Err("candidate must be lowercase 40-hex".into());
    };
    let repo = controlled_repository(repo)?;
    let git_binary = trusted_git()?;
    if git(
        &git_binary,
        &repo,
        &["rev-parse", "--verify", &format!("{candidate}^{{commit}}")],
        None,
    )?
    .trim()
        != candidate
    {
        return Err("candidate does not resolve to commit".into());
    }
    if git(&git_binary, &repo, &["rev-parse", "--verify", "HEAD"], None)?.trim() != candidate {
        return Err("candidate is not exact HEAD".into());
    }
    let binary = checked_digest(binary, MAX_BINARY_BYTES)?;
    let mut sources = serde_json::Map::new();
    for source in FIXED_SOURCES {
        if !clean_path(source) {
            return Err("unsafe fixed source list".into());
        }
        if !git(
            &git_binary,
            &repo,
            &["status", "--porcelain", "--", source],
            None,
        )?
        .is_empty()
        {
            return Err("a fixed deployment source is dirty or untracked".into());
        }
        let tree = git(
            &git_binary,
            &repo,
            &["ls-tree", candidate, "--", source],
            None,
        )?;
        let f: Vec<_> = tree.split_whitespace().collect();
        if f.len() != 4
            || !matches!(f[0], "100644" | "100755")
            || f[1] != "blob"
            || !valid_sha(f[2], 40)
            || f[3] != *source
        {
            return Err("candidate source is not a regular blob".into());
        }
        let p = repo.join(source);
        let mut bytes = Vec::new();
        checked_open(&p, MAX_FIXED_BYTES)?
            .take(MAX_FIXED_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "fixed source unreadable")?;
        if bytes.len() as u64 > MAX_FIXED_BYTES {
            return Err("fixed source too large".into());
        }
        let hash = lower_hex(&Sha256::digest(&bytes));
        if git(
            &git_binary,
            &repo,
            &["hash-object", "--no-filters", "--stdin"],
            Some(bytes),
        )?
        .trim()
            != f[2]
        {
            return Err("working source differs from candidate blob".into());
        }
        // Recheck the exact retained-source digest after Git consumed the
        // bounded bytes; a concurrent trusted edit is never an installable proposal.
        if checked_digest(&p, MAX_FIXED_BYTES)? != hash {
            return Err("fixed source changed during preflight".into());
        }
        sources.insert((*source).to_owned(), json!(hash));
    }
    Ok(
        json!({"mode":"read-only-preflight","deployment":{"schema":SCHEMA,"candidate":candidate,"repo_rel":"../..","tools_binary_sha256":binary,"sources":sources}}),
    )
}
pub fn run(args: Args) -> Result<(), String> {
    let v = preflight(&args.repository, &args.candidate, &args.tools_binary)?;
    println!("{}", serde_jcs::to_string(&v).map_err(|e| e.to_string())?);
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use kio_core::store_dir::{Publication, StoreDirectory};
    use std::os::unix::fs::PermissionsExt;

    fn private_fixture() -> (tempfile::TempDir, StoreDirectory, PathBuf, PathBuf, String) {
        let temp = super::super::canonical_tempdir();
        let root = StoreDirectory::open(temp.path()).expect("retained fixture root");
        for source in FIXED_SOURCES {
            let relative = Path::new(source);
            if let Some(parent) = relative
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                root.create_directory_all(parent)
                    .expect("private fixture parent");
            }
            root.write_atomic(relative, b"fixture\n", Publication::CreateOnly)
                .expect("private fixture source");
        }
        root.write_atomic(
            Path::new("tools-binary"),
            b"fixture tools binary\n",
            Publication::CreateOnly,
        )
        .expect("fixture tools binary");
        let git = trusted_git().expect("trusted git");
        fixture_git(temp.path(), &git, &["init"]);
        fixture_git(temp.path(), &git, &["add", "--all"]);
        fixture_git(
            temp.path(),
            &git,
            &[
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-m",
                "fixture",
            ],
        );
        let candidate = fixture_git(temp.path(), &git, &["rev-parse", "HEAD"])
            .trim()
            .to_owned();
        let binary = root.path().join("tools-binary");
        (temp, root, git, binary, candidate)
    }

    fn fixture_git(repo: &Path, git: &Path, args: &[&str]) -> String {
        let output = Command::new(git)
            .args(args)
            .current_dir(repo)
            .env_clear()
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .expect("fixture git execution");
        assert!(output.status.success(), "fixture git failed");
        String::from_utf8(output.stdout).expect("fixture git UTF-8")
    }

    #[test]
    fn preflight_subprocess_helper() {
        let Ok(repo) = env::var("KIO_ROUTE_PREFLIGHT_HELPER_REPO") else {
            return;
        };
        let candidate = env::var("KIO_ROUTE_PREFLIGHT_HELPER_CANDIDATE").expect("helper candidate");
        let binary = env::var("KIO_ROUTE_PREFLIGHT_HELPER_BINARY").expect("helper binary");
        preflight(Path::new(&repo), &candidate, Path::new(&binary))
            .expect("hardened preflight helper");
    }

    fn hardened_preflight_in_child(
        repo: &Path,
        candidate: &str,
        binary: &Path,
        key: &str,
        value: &Path,
    ) {
        let status = Command::new(env::current_exe().expect("current test executable"))
            .arg("--exact")
            .arg("acceptance_tools::route_preflight::tests::preflight_subprocess_helper")
            .arg("--nocapture")
            .env("KIO_ROUTE_PREFLIGHT_HELPER_REPO", repo)
            .env("KIO_ROUTE_PREFLIGHT_HELPER_CANDIDATE", candidate)
            .env("KIO_ROUTE_PREFLIGHT_HELPER_BINARY", binary)
            .env(key, value)
            .status()
            .expect("preflight helper execution");
        assert!(status.success(), "hardened preflight child failed");
    }

    #[test]
    fn preflight_requires_the_exact_clean_candidate() {
        let (_temp, root, _git, binary, candidate) = private_fixture();
        preflight(root.path(), &candidate, &binary).expect("clean exact candidate");
        root.write_atomic(
            Path::new(FIXED_SOURCES[0]),
            b"changed\n",
            Publication::Replace,
        )
        .expect("dirty source");
        assert!(preflight(root.path(), &candidate, &binary).is_err());
        assert!(
            preflight(
                root.path(),
                "0000000000000000000000000000000000000000",
                &binary
            )
            .is_err()
        );
    }

    #[test]
    fn preflight_ignores_inherited_git_dir() {
        let (_temp, root, _git, binary, candidate) = private_fixture();
        hardened_preflight_in_child(
            root.path(),
            &candidate,
            &binary,
            "GIT_DIR",
            Path::new("/dev/null"),
        );
    }

    #[test]
    fn preflight_disables_inherited_fsmonitor() {
        let (temp, root, git, binary, candidate) = private_fixture();
        let sentinel = temp.path().join("fsmonitor-ran");
        let script = temp.path().join("fsmonitor.sh");
        let config = temp.path().join("global.gitconfig");
        root.write_atomic(
            Path::new("fsmonitor.sh"),
            b"#!/bin/sh\n: > \"${0%/*}/fsmonitor-ran\"\nprintf '\\n'\n",
            Publication::CreateOnly,
        )
        .expect("fsmonitor script");
        root.write_atomic(
            Path::new("global.gitconfig"),
            format!("[core]\nfsmonitor = {}\n", script.display()).as_bytes(),
            Publication::CreateOnly,
        )
        .expect("fsmonitor config");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700))
            .expect("fsmonitor script permissions");
        fixture_git(
            root.path(),
            &git,
            &[
                "config",
                "core.fsmonitor",
                script.to_str().expect("script UTF-8"),
            ],
        );
        fixture_git(root.path(), &git, &["status", "--porcelain"]);
        assert!(
            sentinel.exists(),
            "unconstrained status did not run fsmonitor"
        );
        root.remove_file(Path::new("fsmonitor-ran"))
            .expect("remove owned sentinel");
        hardened_preflight_in_child(
            root.path(),
            &candidate,
            &binary,
            "GIT_CONFIG_GLOBAL",
            &config,
        );
        assert!(!sentinel.exists());
    }
}
