//! Registration of an external, paid fixture without a Python control plane.
//!
//! The source tree is read-only input.  The output root is an absolute final
//! location: Kio persists absolute scope paths, so a completed fixture is not
//! relocatable.

#[cfg(unix)]
use cap_primitives::fs::MetadataExt as CapMetadataExt;
use cap_primitives::{ambient_authority, fs as cap_fs};
use std::{
    ffi::OsString,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

use crate::{
    artifact::CreateOnlyArtifact,
    runner::{BoundedProcessOptions, run_bounded_command},
};

const MAX_SCOPES: usize = 10_000;
const MAX_ENTRIES: usize = 100_000;
const MAX_REPORT_BYTES: usize = 4 * 1024 * 1024;
const MAX_SOURCE_FILE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixtureMode {
    Offline,
    Online,
    Realtime,
}

impl std::str::FromStr for FixtureMode {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "offline" => Ok(Self::Offline),
            "online" => Ok(Self::Online),
            "realtime" => Ok(Self::Realtime),
            _ => Err("fixture mode must be offline, online, or realtime".into()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FixtureRegisterOptions {
    pub corpus: PathBuf,
    pub out: PathBuf,
    pub bin: PathBuf,
    pub mode: FixtureMode,
    pub personas: Vec<String>,
    pub drain_rounds: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixtureRegisterSummary {
    pub scopes: usize,
    pub indexed: usize,
    pub pending: usize,
    pub report: PathBuf,
}

#[derive(Debug, Error)]
pub enum FixtureRegisterError {
    #[error("{0}")]
    Input(String),
    #[error("could not serialize registration report: {0}")]
    Serialize(#[from] serde_json::Error),
}

#[derive(Serialize)]
struct Report<'a> {
    schema: &'static str,
    corpus: &'a Path,
    fixture_root: &'a Path,
    mode: &'static str,
    results: Vec<PersonaReport>,
}
#[derive(Serialize)]
struct Owner<'a> {
    schema: &'static str,
    corpus: &'a Path,
    fixture_root: &'a Path,
    mode: &'static str,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct PersonaReport {
    persona: String,
    leaves: usize,
    indexed: usize,
    pending: usize,
    failures: Vec<Failure>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Failure {
    scope: PathBuf,
    step: String,
    exit: i32,
    detail: String,
}
#[derive(Deserialize)]
struct Status {
    tasks: Vec<StatusTask>,
}
#[derive(Deserialize)]
struct StatusTask {
    status: TaskStatus,
}
#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TaskStatus {
    Pending,
    Running,
    Done,
    Partial,
    Failed,
    Paused,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExistingReport {
    schema: String,
    corpus: PathBuf,
    fixture_root: PathBuf,
    mode: String,
    results: Vec<PersonaReport>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExistingOwner {
    schema: String,
    corpus: PathBuf,
    fixture_root: PathBuf,
    mode: String,
}

fn mode_name(mode: FixtureMode) -> &'static str {
    match mode {
        FixtureMode::Offline => "offline",
        FixtureMode::Online => "online",
        FixtureMode::Realtime => "realtime",
    }
}

fn absolute_existing_dir(path: &Path, label: &str) -> Result<PathBuf, FixtureRegisterError> {
    if !path.is_absolute() {
        return Err(FixtureRegisterError::Input(format!(
            "{label} must be absolute"
        )));
    }
    fs::canonicalize(path)
        .map_err(|e| FixtureRegisterError::Input(format!("cannot open {label}: {e}")))
        .and_then(|p| {
            if p.is_dir() {
                Ok(p)
            } else {
                Err(FixtureRegisterError::Input(format!(
                    "{label} is not a directory"
                )))
            }
        })
}

fn regular_file_nofollow(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_file() && !metadata.file_type().is_symlink())
        .unwrap_or(false)
}

fn safe_persona_home(corpus: &Path, persona: &str) -> Result<PathBuf, FixtureRegisterError> {
    if persona.is_empty() || Path::new(persona).components().count() != 1 {
        return Err(FixtureRegisterError::Input(
            "persona name is not a single path component".into(),
        ));
    }
    let persona_path = corpus.join(persona);
    let persona_meta = fs::symlink_metadata(&persona_path)
        .map_err(|_| FixtureRegisterError::Input(format!("persona {persona} is missing")))?;
    if persona_meta.file_type().is_symlink() || !persona_meta.is_dir() {
        return Err(FixtureRegisterError::Input(format!(
            "persona {persona} must be a real directory"
        )));
    }
    let home = persona_path.join("home");
    let home_meta = fs::symlink_metadata(&home)
        .map_err(|_| FixtureRegisterError::Input(format!("persona {persona} has no home")))?;
    if home_meta.file_type().is_symlink() || !home_meta.is_dir() {
        return Err(FixtureRegisterError::Input(format!(
            "persona {persona} home must be a real directory"
        )));
    }
    let home = fs::canonicalize(home)
        .map_err(|e| FixtureRegisterError::Input(format!("cannot bind persona home: {e}")))?;
    if !home.starts_with(corpus) {
        return Err(FixtureRegisterError::Input(
            "persona home escapes the bound corpus".into(),
        ));
    }
    Ok(home)
}

fn bind_persona_home(
    corpus: &Path,
    persona: &str,
) -> Result<(PathBuf, fs::File), FixtureRegisterError> {
    let home = safe_persona_home(corpus, persona)?;
    let corpus_fd = cap_fs::open_ambient_dir(corpus, ambient_authority())
        .map_err(|e| FixtureRegisterError::Input(format!("cannot retain corpus: {e}")))?;
    let persona_fd = cap_fs::open_dir_nofollow(&corpus_fd, Path::new(persona))
        .map_err(|_| FixtureRegisterError::Input("persona changed while binding".into()))?;
    let home_fd = cap_fs::open_dir_nofollow(&persona_fd, Path::new("home"))
        .map_err(|_| FixtureRegisterError::Input("persona home changed while binding".into()))?;
    Ok((home, home_fd))
}

fn leaf_scopes(root: &Path) -> Result<Vec<PathBuf>, FixtureRegisterError> {
    let mut todo = vec![root.to_owned()];
    let mut leaves = Vec::new();
    let mut entries_seen = 0usize;
    while let Some(dir) = todo.pop() {
        let mut entries = fs::read_dir(&dir)
            .map_err(|e| {
                FixtureRegisterError::Input(format!("cannot enumerate {}: {e}", dir.display()))
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| FixtureRegisterError::Input(e.to_string()))?;
        entries.sort_by_key(|e| e.file_name());
        let mut has_file = false;
        for entry in entries {
            entries_seen += 1;
            if entries_seen > MAX_ENTRIES {
                return Err(FixtureRegisterError::Input(
                    "fixture source exceeds entry bound".into(),
                ));
            }
            let ty = entry
                .file_type()
                .map_err(|e| FixtureRegisterError::Input(e.to_string()))?;
            if ty.is_symlink() {
                return Err(FixtureRegisterError::Input(format!(
                    "fixture source contains symlink: {}",
                    entry.path().display()
                )));
            }
            if ty.is_file() {
                has_file = true;
            } else if ty.is_dir() && entry.file_name() != ".kio" {
                todo.push(entry.path());
            }
        }
        if has_file {
            leaves.push(dir);
            if leaves.len() > MAX_SCOPES {
                return Err(FixtureRegisterError::Input(
                    "fixture has too many scopes".into(),
                ));
            }
        }
    }
    leaves.sort();
    Ok(leaves)
}

fn copy_clean_retained(source: &fs::File, dest: &Path) -> Result<(), FixtureRegisterError> {
    fs::create_dir_all(dest)
        .map_err(|e| FixtureRegisterError::Input(format!("cannot create fixture output: {e}")))?;
    for entry in cap_fs::read_dir(source, Path::new(".")).map_err(|e| {
        FixtureRegisterError::Input(format!("cannot enumerate retained source: {e}"))
    })? {
        let entry = entry.map_err(|e| FixtureRegisterError::Input(e.to_string()))?;
        let name = entry.file_name();
        let name_path = Path::new(&name);
        let ty = entry
            .file_type()
            .map_err(|e| FixtureRegisterError::Input(e.to_string()))?;
        if name == ".kio" {
            continue;
        }
        if ty.is_symlink() {
            return Err(FixtureRegisterError::Input(
                "fixture source contains symlink".into(),
            ));
        }
        let output = dest.join(&name);
        if ty.is_dir() {
            let _before =
                cap_fs::stat(source, name_path, cap_fs::FollowSymlinks::No).map_err(|_| {
                    FixtureRegisterError::Input(
                        "fixture source directory changed during copy".into(),
                    )
                })?;
            let child = cap_fs::open_dir_nofollow(source, name_path).map_err(|_| {
                FixtureRegisterError::Input("fixture source directory changed during copy".into())
            })?;
            #[cfg(unix)]
            {
                let opened = cap_fs::Metadata::from_file(&child).map_err(|_| {
                    FixtureRegisterError::Input(
                        "fixture source directory changed during copy".into(),
                    )
                })?;
                if _before.dev() != opened.dev() || _before.ino() != opened.ino() {
                    return Err(FixtureRegisterError::Input(
                        "fixture source directory changed during copy".into(),
                    ));
                }
            }
            copy_clean_retained(&child, &output)?;
            let _after =
                cap_fs::stat(source, name_path, cap_fs::FollowSymlinks::No).map_err(|_| {
                    FixtureRegisterError::Input(
                        "fixture source directory changed during copy".into(),
                    )
                })?;
            #[cfg(unix)]
            if _before.dev() != _after.dev() || _before.ino() != _after.ino() {
                return Err(FixtureRegisterError::Input(
                    "fixture source directory changed during copy".into(),
                ));
            }
        } else if ty.is_file() {
            let _before =
                cap_fs::stat(source, name_path, cap_fs::FollowSymlinks::No).map_err(|_| {
                    FixtureRegisterError::Input("fixture source file changed during copy".into())
                })?;
            #[cfg(unix)]
            {
                if _before.nlink() != 1 {
                    return Err(FixtureRegisterError::Input(
                        "fixture source contains hard-linked file".into(),
                    ));
                }
            }
            let mut options = cap_fs::OpenOptions::new();
            options
                .read(true)
                ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
            let mut file = cap_fs::open(source, name_path, &options).map_err(|_| {
                FixtureRegisterError::Input("fixture source file changed during copy".into())
            })?;
            let _opened = cap_fs::Metadata::from_file(&file).map_err(|_| {
                FixtureRegisterError::Input("fixture source file changed during copy".into())
            })?;
            #[cfg(unix)]
            if _before.dev() != _opened.dev()
                || _before.ino() != _opened.ino()
                || _before.nlink() != _opened.nlink()
            {
                return Err(FixtureRegisterError::Input(
                    "fixture source file changed during copy".into(),
                ));
            }
            let mut bytes = Vec::new();
            (&mut file)
                .take(MAX_SOURCE_FILE_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| {
                    FixtureRegisterError::Input(format!("cannot read retained fixture file: {e}"))
                })?;
            if bytes.len() as u64 > MAX_SOURCE_FILE_BYTES {
                return Err(FixtureRegisterError::Input(
                    "fixture source file exceeds byte bound".into(),
                ));
            }
            let _after =
                cap_fs::stat(source, name_path, cap_fs::FollowSymlinks::No).map_err(|_| {
                    FixtureRegisterError::Input("fixture source file changed during copy".into())
                })?;
            #[cfg(unix)]
            {
                if _before.dev() != _after.dev()
                    || _before.ino() != _after.ino()
                    || _before.nlink() != _after.nlink()
                    || _before.len() != _opened.len()
                {
                    return Err(FixtureRegisterError::Input(
                        "fixture source file changed during copy".into(),
                    ));
                }
            }
            fs::write(output, bytes).map_err(|e| {
                FixtureRegisterError::Input(format!("cannot write fixture copy: {e}"))
            })?;
        } else {
            return Err(FixtureRegisterError::Input(
                "fixture source contains special file".into(),
            ));
        }
    }
    Ok(())
}

fn environment(
    root: &Path,
    persona: &str,
    mode: FixtureMode,
) -> Result<Vec<(OsString, OsString)>, FixtureRegisterError> {
    let base = root.join("env").join(persona);
    for value in [
        base.join("home"),
        base.join("xdg-config"),
        base.join("xdg-data"),
        base.join("xdg-cache"),
    ] {
        fs::create_dir_all(value).map_err(|error| {
            FixtureRegisterError::Input(format!(
                "cannot create isolated fixture environment: {error}"
            ))
        })?;
    }
    let mut environment = [
        ("HOME", base.join("home")),
        ("XDG_CONFIG_HOME", base.join("xdg-config")),
        ("XDG_DATA_HOME", base.join("xdg-data")),
        ("XDG_CACHE_HOME", base.join("xdg-cache")),
    ]
    .into_iter()
    .map(|(key, value)| (OsString::from(key), value.into_os_string()))
    .collect::<Vec<_>>();
    if matches!(mode, FixtureMode::Online | FixtureMode::Realtime) {
        for key in ["GEMINI_API_KEY", "MISTRAL_API_KEY"] {
            if let Some(value) = std::env::var_os(key) {
                environment.push((OsString::from(key), value));
            }
        }
    }
    Ok(environment)
}

fn invoke(
    bin: &Path,
    scope: &Path,
    env: &[(OsString, OsString)],
    args: &[&str],
) -> Result<(i32, String), FixtureRegisterError> {
    let mut command = Command::new(bin);
    command
        .args(args)
        .arg("--json")
        .current_dir(scope)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("TZ", "UTC")
        .envs(env.iter().cloned());
    let out = run_bounded_command(&mut command, BoundedProcessOptions::default(), None)
        .map_err(|e| FixtureRegisterError::Input(format!("bounded kio subprocess failed: {e}")))?;
    let text = if out.stdout.is_empty() {
        out.stderr
    } else {
        out.stdout
    };
    Ok((out.status.code().unwrap_or(-1), text))
}

fn detail(text: String) -> String {
    text.chars().take(600).collect()
}

fn canonical_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, FixtureRegisterError> {
    let mut bytes = serde_jcs::to_vec(value).map_err(|error| {
        FixtureRegisterError::Input(format!("cannot canonicalize fixture record: {error}"))
    })?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn validate_identity(
    path: &Path,
    schema: &str,
    corpus: &Path,
    root: &Path,
    mode: FixtureMode,
) -> Result<(), FixtureRegisterError> {
    let bytes =
        kio_core::cas::read_bounded_regular_file(path, MAX_REPORT_BYTES as u64).map_err(|_| {
            FixtureRegisterError::Input("fixture owner marker is missing or unsafe".into())
        })?;
    let owner: ExistingOwner = serde_json::from_slice(&bytes)
        .map_err(|_| FixtureRegisterError::Input("fixture owner marker is invalid".into()))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|_| FixtureRegisterError::Input("fixture owner marker is invalid".into()))?;
    let mut canonical = serde_jcs::to_vec(&value).map_err(|_| {
        FixtureRegisterError::Input("fixture owner marker cannot be canonicalized".into())
    })?;
    canonical.push(b'\n');
    if bytes != canonical
        || owner.schema != schema
        || owner.corpus != corpus
        || owner.fixture_root != root
        || owner.mode != mode_name(mode)
    {
        return Err(FixtureRegisterError::Input(
            "fixture identity does not match this final path/corpus/mode".into(),
        ));
    }
    Ok(())
}

fn validate_report(
    path: &Path,
    corpus: &Path,
    root: &Path,
    mode: FixtureMode,
    expected: &[PersonaReport],
) -> Result<(), FixtureRegisterError> {
    let bytes =
        kio_core::cas::read_bounded_regular_file(path, MAX_REPORT_BYTES as u64).map_err(|_| {
            FixtureRegisterError::Input("fixture registration report is missing or unsafe".into())
        })?;
    let report: ExistingReport = serde_json::from_slice(&bytes).map_err(|_| {
        FixtureRegisterError::Input("fixture registration report is invalid".into())
    })?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| {
        FixtureRegisterError::Input("fixture registration report is invalid".into())
    })?;
    let mut canonical = serde_jcs::to_vec(&value).map_err(|_| {
        FixtureRegisterError::Input("fixture registration report cannot be canonicalized".into())
    })?;
    canonical.push(b'\n');
    if bytes != canonical
        || report.schema != "kio.fixture-registration/v1"
        || report.corpus != corpus
        || report.fixture_root != root
        || report.mode != mode_name(mode)
        || report.results != expected
        || report
            .results
            .iter()
            .any(|row| row.indexed != row.leaves || row.pending != 0 || !row.failures.is_empty())
    {
        return Err(FixtureRegisterError::Input(
            "fixture registration report does not exactly attest this completed registration"
                .into(),
        ));
    }
    Ok(())
}

fn publish_persona_work(
    root: &Path,
    persona: &str,
    source: &fs::File,
) -> Result<PathBuf, FixtureRegisterError> {
    let person_dir = root.join(persona);
    let work = person_dir.join("home");
    match fs::symlink_metadata(&person_dir) {
        Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_dir() => {
            retained_visible_matches(source, &work)?;
            return Ok(work);
        }
        Ok(_) => {
            return Err(FixtureRegisterError::Input(
                "fixture persona output is not a real directory".into(),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(FixtureRegisterError::Input(error.to_string())),
    }
    let staging_root = root.join(".staging");
    fs::create_dir_all(&staging_root).map_err(|e| {
        FixtureRegisterError::Input(format!("cannot create fixture staging root: {e}"))
    })?;
    let stage = staging_root.join(persona);
    fs::create_dir(&stage).map_err(|_| {
        FixtureRegisterError::Input(format!(
            "fixture persona {persona} has interrupted staging; refusing recovery"
        ))
    })?;
    copy_clean_retained(source, &stage.join("home"))?;
    let root_fd = cap_fs::open_ambient_dir(root, ambient_authority()).map_err(|e| {
        FixtureRegisterError::Input(format!("cannot retain fixture output root: {e}"))
    })?;
    let staging_fd = cap_fs::open_dir_nofollow(&root_fd, Path::new(".staging")).map_err(|_| {
        FixtureRegisterError::Input("fixture staging root changed during publication".into())
    })?;
    crate::scale_fixture::rename_noreplace(&staging_fd, persona, &root_fd, persona).map_err(
        |e| FixtureRegisterError::Input(format!("cannot atomically publish fixture persona: {e}")),
    )?;
    Ok(work)
}

fn retained_visible_matches(source: &fs::File, work: &Path) -> Result<(), FixtureRegisterError> {
    let metadata = fs::symlink_metadata(work).map_err(|_| {
        FixtureRegisterError::Input("published fixture persona has no safe home tree".into())
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(FixtureRegisterError::Input(
            "published fixture home is not a real directory".into(),
        ));
    }
    retained_visible_matches_inner(source, work)
}

fn retained_visible_matches_inner(
    source: &fs::File,
    work: &Path,
) -> Result<(), FixtureRegisterError> {
    let mut source_names = std::collections::BTreeSet::new();
    for entry in cap_fs::read_dir(source, Path::new(".")).map_err(|_| {
        FixtureRegisterError::Input("cannot enumerate retained fixture source".into())
    })? {
        let entry = entry.map_err(|e| FixtureRegisterError::Input(e.to_string()))?;
        let name = entry.file_name();
        if name == ".kio" {
            continue;
        }
        source_names.insert(name.clone());
        let ty = entry
            .file_type()
            .map_err(|e| FixtureRegisterError::Input(e.to_string()))?;
        let output = work.join(&name);
        if ty.is_dir() {
            let child = cap_fs::open_dir_nofollow(source, Path::new(&name)).map_err(|_| {
                FixtureRegisterError::Input(
                    "fixture source changed during resume validation".into(),
                )
            })?;
            retained_visible_matches(&child, &output)?;
        } else if ty.is_file() {
            let mut options = cap_fs::OpenOptions::new();
            options
                .read(true)
                ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
            let input = cap_fs::open(source, Path::new(&name), &options).map_err(|_| {
                FixtureRegisterError::Input(
                    "fixture source changed during resume validation".into(),
                )
            })?;
            let mut source_bytes = Vec::new();
            input
                .take(MAX_SOURCE_FILE_BYTES + 1)
                .read_to_end(&mut source_bytes)
                .map_err(|_| {
                    FixtureRegisterError::Input("cannot read retained fixture source".into())
                })?;
            if source_bytes.len() as u64 > MAX_SOURCE_FILE_BYTES
                || kio_core::cas::read_bounded_regular_file(&output, MAX_SOURCE_FILE_BYTES)
                    .map_err(|_| {
                        FixtureRegisterError::Input("published fixture file is unsafe".into())
                    })?
                    != source_bytes
            {
                return Err(FixtureRegisterError::Input(
                    "published fixture work does not exactly match retained source".into(),
                ));
            }
        } else {
            return Err(FixtureRegisterError::Input(
                "fixture source contains unsupported entry during resume validation".into(),
            ));
        }
    }
    for entry in fs::read_dir(work).map_err(|_| {
        FixtureRegisterError::Input("cannot enumerate published fixture work".into())
    })? {
        let entry = entry.map_err(|e| FixtureRegisterError::Input(e.to_string()))?;
        let name = entry.file_name();
        if name != ".kio" && !source_names.contains(&name) {
            return Err(FixtureRegisterError::Input(
                "published fixture work has unexpected non-runtime content".into(),
            ));
        }
    }
    Ok(())
}

fn scope_complete(
    bin: &Path,
    scope: &Path,
    env: &[(OsString, OsString)],
) -> Result<bool, FixtureRegisterError> {
    let head = scope.join(".kio/HEAD");
    if !fs::symlink_metadata(&head)
        .map(|m| m.file_type().is_file() && !m.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Ok(false);
    }
    Ok(pending(bin, scope, env)? == 0)
}

fn runtime_dir_exists_safely(scope: &Path) -> Result<bool, FixtureRegisterError> {
    match fs::symlink_metadata(scope.join(".kio")) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => Err(
            FixtureRegisterError::Input("fixture runtime directory is not a real directory".into()),
        ),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(FixtureRegisterError::Input(error.to_string())),
    }
}

fn pending(
    bin: &Path,
    scope: &Path,
    env: &[(OsString, OsString)],
) -> Result<usize, FixtureRegisterError> {
    let (code, body) = invoke(bin, scope, env, &["status"])?;
    if code != 0 {
        return Err(FixtureRegisterError::Input("kio status failed".into()));
    }
    let value: Status = serde_json::from_str(&body).map_err(|_| {
        FixtureRegisterError::Input("kio status was not a typed task response".into())
    })?;
    Ok(value
        .tasks
        .iter()
        .filter(|task| task.status != TaskStatus::Done)
        .count())
}

pub fn register(
    options: FixtureRegisterOptions,
) -> Result<FixtureRegisterSummary, FixtureRegisterError> {
    let corpus = absolute_existing_dir(&options.corpus, "corpus")?;
    if !options.out.is_absolute() {
        return Err(FixtureRegisterError::Input(
            "fixture output must be an absolute final path".into(),
        ));
    }
    let bin = fs::canonicalize(&options.bin)
        .map_err(|_| FixtureRegisterError::Input("kio binary is unavailable".into()))?;
    if !bin.is_file() {
        return Err(FixtureRegisterError::Input(
            "kio binary is unavailable".into(),
        ));
    }
    let personas = if options.personas.is_empty() {
        fs::read_dir(&corpus)
            .map_err(|e| FixtureRegisterError::Input(e.to_string()))?
            .filter_map(Result::ok)
            .filter_map(|entry| {
                (entry.path().join("home").exists())
                    .then(|| entry.file_name().to_string_lossy().into_owned())
            })
            .collect::<Vec<_>>()
    } else {
        options.personas.clone()
    };
    if personas.is_empty() {
        return Err(FixtureRegisterError::Input(
            "corpus has no persona home directories".into(),
        ));
    }
    let persona_homes = personas
        .iter()
        .map(|persona| {
            bind_persona_home(&corpus, persona).map(|(home, fd)| (persona.clone(), home, fd))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let output_exists = match fs::symlink_metadata(&options.out) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(FixtureRegisterError::Input(
                "fixture output must not be a symlink".into(),
            ));
        }
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(FixtureRegisterError::Input(error.to_string())),
    };
    let (out, existing_report) = if output_exists {
        let existing = absolute_existing_dir(&options.out, "fixture output")?;
        validate_identity(
            &existing.join("fixture-owner.json"),
            "kio.fixture-owner/v1",
            &corpus,
            &existing,
            options.mode,
        )?;
        let report_path = existing.join("registration-report.json");
        if fs::symlink_metadata(&report_path).is_ok() && !regular_file_nofollow(&report_path) {
            return Err(FixtureRegisterError::Input(
                "fixture registration report is not a safe regular file".into(),
            ));
        }
        (existing.clone(), regular_file_nofollow(&report_path))
    } else {
        let parent = options
            .out
            .parent()
            .ok_or_else(|| FixtureRegisterError::Input("fixture output has no parent".into()))?;
        let parent = absolute_existing_dir(parent, "fixture output parent")?;
        let out =
            parent.join(options.out.file_name().ok_or_else(|| {
                FixtureRegisterError::Input("fixture output has no filename".into())
            })?);
        fs::create_dir(&out)
            .map_err(|e| FixtureRegisterError::Input(format!("cannot create fixture root: {e}")))?;
        let owner = CreateOnlyArtifact::bind(
            &out.join("fixture-owner.json"),
            &corpus,
            "fixture owner marker",
        )
        .map_err(|e| FixtureRegisterError::Input(e.to_string()))?;
        owner
            .publish(
                &canonical_bytes(&Owner {
                    schema: "kio.fixture-owner/v1",
                    corpus: &corpus,
                    fixture_root: &out,
                    mode: mode_name(options.mode),
                })?,
                MAX_REPORT_BYTES,
            )
            .map_err(|e| FixtureRegisterError::Input(e.to_string()))?;
        (out, false)
    };
    let mut rows = Vec::new();
    let mut total = 0;
    let mut indexed = 0;
    let mut all_pending = 0;
    for (persona, source_path, source_fd) in persona_homes {
        // Re-bind the published pathname before consuming the retained
        // descriptor. The descriptor protects the copy; this catches a source
        // root swap before any fixture-side command runs.
        if safe_persona_home(&corpus, &persona)? != source_path {
            return Err(FixtureRegisterError::Input(
                "persona home changed after descriptor binding".into(),
            ));
        }
        let work = if existing_report {
            let work = out.join(&persona).join("home");
            // A terminal report is only a cache of a completed registration,
            // never permission to traverse a path that has since been
            // replaced. Validate the entire visible tree before leaf discovery
            // or any fixture-side subprocess is reached.
            retained_visible_matches(&source_fd, &work)?;
            work
        } else {
            publish_persona_work(&out, &persona, &source_fd)?
        };
        if !work.is_dir() {
            return Err(FixtureRegisterError::Input(
                "fixture report references a missing persona work tree".into(),
            ));
        }
        let scopes = leaf_scopes(&work)?;
        let env = environment(&out, &persona, options.mode)?;
        let mut failures = Vec::new();
        let mut done = 0;
        let mut completed = Vec::new();
        for scope in &scopes {
            if scope_complete(&bin, scope, &env)? {
                completed.push(scope);
                continue;
            }
            if !runtime_dir_exists_safely(scope)? {
                let (code, text) = invoke(&bin, scope, &env, &["init"])?;
                if code != 0 {
                    failures.push(Failure {
                        scope: scope.clone(),
                        step: "init".into(),
                        exit: code,
                        detail: detail(text),
                    });
                    continue;
                }
            }
            let mut args = vec!["index", "--yes"];
            match options.mode {
                FixtureMode::Offline => args.push("--offline"),
                FixtureMode::Online => {
                    args.extend(["--approve"]);
                }
                FixtureMode::Realtime => {
                    args.extend(["--approve", "--realtime"]);
                }
            }
            match invoke(&bin, scope, &env, &args) {
                Ok((0 | 3, _)) => {
                    completed.push(scope);
                }
                Ok((code, text)) => failures.push(Failure {
                    scope: scope.clone(),
                    step: "index".into(),
                    exit: code,
                    detail: detail(text),
                }),
                Err(error) => failures.push(Failure {
                    scope: scope.clone(),
                    step: "index".into(),
                    exit: -1,
                    detail: detail(error.to_string()),
                }),
            }
        }
        let mut pending_count = 0;
        for scope in completed {
            let mut before = pending(&bin, scope, &env)?;
            if options.mode == FixtureMode::Online {
                for _ in 0..options.drain_rounds {
                    if before == 0 {
                        break;
                    }
                    let (code, text) = invoke(&bin, scope, &env, &["batch", "resume", "--online"])?;
                    if code != 0 {
                        return Err(FixtureRegisterError::Input(format!(
                            "kio batch resume failed: {}",
                            detail(text)
                        )));
                    }
                    let now = pending(&bin, scope, &env)?;
                    if now >= before {
                        before = now;
                        break;
                    }
                    before = now;
                }
            }
            if before == 0 {
                done += 1;
            } else {
                pending_count += before;
            }
        }
        total += scopes.len();
        indexed += done;
        all_pending += pending_count;
        rows.push(PersonaReport {
            persona,
            leaves: scopes.len(),
            indexed: done,
            pending: pending_count,
            failures,
        });
    }
    let report_path = out.join("registration-report.json");
    if existing_report {
        validate_report(&report_path, &corpus, &out, options.mode, &rows)?;
    } else if indexed == total && all_pending == 0 && rows.iter().all(|row| row.failures.is_empty())
    {
        let artifact =
            CreateOnlyArtifact::bind(&report_path, &corpus, "fixture registration report")
                .map_err(|e| FixtureRegisterError::Input(e.to_string()))?;
        let bytes = canonical_bytes(&Report {
            schema: "kio.fixture-registration/v1",
            corpus: &corpus,
            fixture_root: &out,
            mode: mode_name(options.mode),
            results: rows,
        })?;
        artifact
            .publish(&bytes, MAX_REPORT_BYTES)
            .map_err(|e| FixtureRegisterError::Input(e.to_string()))?;
    }
    Ok(FixtureRegisterSummary {
        scopes: total,
        indexed,
        pending: all_pending,
        report: report_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[cfg(unix)]
    fn mock_kio(root: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = root.join("mock-kio.sh");
        fs::write(&path, r#"#!/bin/sh
set -eu
if env | grep -Eq '^(KIO_TEST_|UNNEEDED_SECRET=)'; then exit 91; fi
test -n "${HOME:-}"; test -n "${XDG_CONFIG_HOME:-}"; test -n "${XDG_DATA_HOME:-}"; test -n "${XDG_CACHE_HOME:-}"
case "$1" in
 init) mkdir -p .kio; echo init >> .kio/calls ;;
 index) echo "index:$*" >> .kio/calls; touch .kio/HEAD; exit 3 ;;
 status) echo status >> .kio/calls; if test -e .kio/batched; then printf '{"tasks":[{"status":"done"}]}' ; else printf '{"tasks":['; i=0; while test "$i" -lt 150; do printf '{"status":"done"},'; i=$((i+1)); done; printf '{"status":"pending"}]}' ; fi ;;
 batch) echo "batch:$*" >> .kio/calls; touch .kio/batched ;;
 *) exit 92 ;;
esac
"#).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
    #[test]
    fn discovers_direct_file_leaves_and_excludes_kio() {
        let d = tempdir().unwrap();
        let home = d.path().join("home");
        fs::create_dir_all(home.join("a/nested/.kio")).unwrap();
        fs::write(home.join("a/x"), b"x").unwrap();
        fs::write(home.join("a/nested/y"), b"y").unwrap();
        let leaves = leaf_scopes(&home).unwrap();
        assert_eq!(leaves, vec![home.join("a"), home.join("a/nested")]);
    }
    #[test]
    fn fixture_mode_is_closed() {
        assert!("fixture-b".parse::<FixtureMode>().is_err());
        assert_eq!(
            "online".parse::<FixtureMode>().unwrap(),
            FixtureMode::Online
        );
    }

    #[cfg(unix)]
    #[test]
    fn mock_registration_preserves_source_and_is_path_bound() {
        let root = tempdir().unwrap();
        let corpus = root.path().join("corpus");
        let home = corpus.join("p01/home");
        fs::create_dir_all(home.join("one/.kio")).unwrap();
        fs::create_dir_all(home.join("two/nested")).unwrap();
        fs::write(home.join("one/a.md"), b"source-a").unwrap();
        fs::write(home.join("two/nested/b.md"), b"source-b").unwrap();
        fs::write(home.join("one/.kio/source-state"), b"must-not-copy").unwrap();
        let bin = mock_kio(root.path());
        let out = root.path().join("final-fixture");
        let options = FixtureRegisterOptions {
            corpus: corpus.clone(),
            out: out.clone(),
            bin,
            mode: FixtureMode::Online,
            personas: vec![],
            drain_rounds: 2,
        };
        let summary = register(options.clone()).unwrap();
        assert_eq!(summary.scopes, 2);
        assert_eq!(summary.indexed, 2); // exit 3 is partial success.
        assert_eq!(summary.pending, 0);
        assert_eq!(fs::read(home.join("one/a.md")).unwrap(), b"source-a");
        assert!(!out.join("p01/home/one/.kio/source-state").exists());
        for scope in [out.join("p01/home/one"), out.join("p01/home/two/nested")] {
            let calls = fs::read_to_string(scope.join(".kio/calls")).unwrap();
            assert!(
                calls.contains("init\n") && calls.contains("index:index --yes --approve --json\n")
            );
            assert!(
                calls.contains("status\n")
                    && calls.contains("batch:batch resume --online --json\n")
            );
        }
        let report = fs::read(summary.report).unwrap();
        assert!(report.ends_with(b"\n"));
        let value: serde_json::Value = serde_json::from_slice(&report).unwrap();
        let mut canonical = serde_jcs::to_vec(&value).unwrap();
        canonical.push(b'\n');
        assert_eq!(report, canonical);
        register(options.clone()).unwrap();
        for scope in [out.join("p01/home/one"), out.join("p01/home/two/nested")] {
            assert_eq!(
                fs::read_to_string(scope.join(".kio/calls"))
                    .unwrap()
                    .matches("index:")
                    .count(),
                1
            );
        }
        let moved = root.path().join("moved-fixture");
        fs::rename(&out, &moved).unwrap();
        let mut moved_options = options;
        moved_options.out = moved;
        assert!(register(moved_options).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_persona_home_is_rejected_before_fixture_publication() {
        use std::os::unix::fs::symlink;
        let root = tempdir().unwrap();
        let corpus = root.path().join("corpus");
        let outside = root.path().join("outside-private");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret"), b"outside").unwrap();
        fs::create_dir_all(corpus.join("p01")).unwrap();
        symlink(&outside, corpus.join("p01/home")).unwrap();
        let out = root.path().join("final");
        let error = register(FixtureRegisterOptions {
            corpus,
            out: out.clone(),
            bin: mock_kio(root.path()),
            mode: FixtureMode::Online,
            personas: vec![],
            drain_rounds: 1,
        })
        .unwrap_err();
        assert!(error.to_string().contains("real directory"));
        assert!(!out.exists());
    }

    #[cfg(unix)]
    #[test]
    fn exact_published_persona_work_is_resumed() {
        let root = tempdir().unwrap();
        let corpus = root.path().join("corpus");
        fs::create_dir_all(corpus.join("p01/home/scope")).unwrap();
        fs::write(corpus.join("p01/home/scope/doc.md"), b"source").unwrap();
        let out = root.path().join("final");
        let options = FixtureRegisterOptions {
            corpus,
            out: out.clone(),
            bin: mock_kio(root.path()),
            mode: FixtureMode::Online,
            personas: vec!["p01".into()],
            drain_rounds: 1,
        };
        register(options.clone()).unwrap();
        fs::remove_file(out.join("registration-report.json")).unwrap();
        register(options).unwrap();
        assert!(out.join("registration-report.json").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn changed_published_work_is_not_resumed() {
        let root = tempdir().unwrap();
        let corpus = root.path().join("corpus");
        fs::create_dir_all(corpus.join("p01/home/scope")).unwrap();
        fs::write(corpus.join("p01/home/scope/doc.md"), b"source").unwrap();
        let out = root.path().join("final");
        let options = FixtureRegisterOptions {
            corpus,
            out: out.clone(),
            bin: mock_kio(root.path()),
            mode: FixtureMode::Online,
            personas: vec!["p01".into()],
            drain_rounds: 1,
        };
        register(options.clone()).unwrap();
        fs::remove_file(out.join("registration-report.json")).unwrap();
        fs::write(out.join("p01/home/scope/doc.md"), b"changed").unwrap();
        assert!(register(options).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn terminal_report_does_not_traverse_a_replaced_home_symlink() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let corpus = root.path().join("corpus");
        fs::create_dir_all(corpus.join("p01/home/scope")).unwrap();
        fs::write(corpus.join("p01/home/scope/doc.md"), b"source").unwrap();
        let out = root.path().join("final");
        let options = FixtureRegisterOptions {
            corpus,
            out: out.clone(),
            bin: mock_kio(root.path()),
            mode: FixtureMode::Online,
            personas: vec!["p01".into()],
            drain_rounds: 1,
        };
        register(options.clone()).unwrap();
        let external = root.path().join("external");
        fs::create_dir_all(external.join("scope")).unwrap();
        fs::write(external.join("scope/external.md"), b"outside").unwrap();
        let home = out.join("p01/home");
        fs::rename(&home, out.join("p01/old-home")).unwrap();
        symlink(&external, &home).unwrap();
        assert!(register(options).is_err());
        assert!(!external.join("scope/.kio").exists());
    }

    #[cfg(unix)]
    #[test]
    fn minimal_terminal_report_is_not_accepted() {
        let root = tempdir().unwrap();
        let corpus = root.path().join("corpus");
        fs::create_dir_all(corpus.join("p01/home/scope")).unwrap();
        fs::write(corpus.join("p01/home/scope/doc.md"), b"source").unwrap();
        let out = root.path().join("final");
        let options = FixtureRegisterOptions {
            corpus: corpus.clone(),
            out: out.clone(),
            bin: mock_kio(root.path()),
            mode: FixtureMode::Online,
            personas: vec!["p01".into()],
            drain_rounds: 1,
        };
        register(options.clone()).unwrap();
        let report = out.join("registration-report.json");
        fs::remove_file(&report).unwrap();
        let value = serde_json::json!({
            "schema": "kio.fixture-registration/v1",
            "corpus": corpus,
            "fixture_root": out,
            "mode": "online",
            "results": [],
        });
        let mut bytes = serde_jcs::to_vec(&value).unwrap();
        bytes.push(b'\n');
        fs::write(&report, bytes).unwrap();
        assert!(register(options).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn retained_descriptor_does_not_follow_a_replaced_source_home() {
        let root = tempdir().unwrap();
        let corpus = root.path().join("corpus");
        let home = corpus.join("p01/home");
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join("original.md"), b"original").unwrap();
        let corpus = fs::canonicalize(&corpus).unwrap();
        let home = corpus.join("p01/home");
        let (_, retained) = bind_persona_home(&corpus, "p01").unwrap();
        fs::rename(&home, corpus.join("p01/old-home")).unwrap();
        fs::create_dir(&home).unwrap();
        fs::write(home.join("replacement.md"), b"replacement").unwrap();
        let copied = root.path().join("copied");
        copy_clean_retained(&retained, &copied).unwrap();
        assert_eq!(fs::read(copied.join("original.md")).unwrap(), b"original");
        assert!(!copied.join("replacement.md").exists());
    }
}
