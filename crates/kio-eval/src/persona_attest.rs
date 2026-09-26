//! Descriptor-bound attestation for one Rust materialized persona bundle.
//!
//! This boundary deliberately does not accept an expected digest.  The
//! materialization record and all referenced artifacts are discovered beneath
//! one retained root directory, parsed by their Rust schema authorities, and
//! observed twice before a create-only report is published.

use crate::{
    persona_artifact::{self, PersonaArtifactError, StrictArtifact},
    persona_materialize::{PersonaMaterializationRecord, PersonaMaterializeError},
    persona_plan::{self, PersonaPlan, PersonaPlanError, PersonaProfile},
    persona_render_artifact::{self, RenderArtifact, RenderArtifactError},
    persona_schedule::{self, PersonaScheduleError, SuiteSchedule},
};
use cap_primitives::{ambient_authority, fs as cap_fs};
use kio_core::cas::{canonical_json_bytes, hash_bytes};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::collections::BTreeMap;
#[cfg(test)]
use std::collections::btree_map::Entry;
#[cfg(test)]
use std::sync::{Mutex, OnceLock};
use std::{
    collections::BTreeSet,
    fs,
    path::{Component, Path, PathBuf},
};
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;

const REPORT_SCHEMA: &str = "kio.persona.filesystem-attestation/v1";
const RECORD_NAME: &str = "persona-materialization.json";
const PLAN_NAME: &str = "persona-plan.json";
const SCHEDULE_NAME: &str = "persona-schedule.json";
const RENDER_NAME: &str = "persona-render.json";
const MAX_ENTRIES: usize = 4;
const MAX_DEPTH: usize = 1;
const MAX_COMPONENTS: usize = 64;
const MAX_COMPONENT_BYTES: usize = 255;
const MAX_REPORT_BYTES: usize = 16 * 1024;

#[derive(Debug, Error)]
pub enum PersonaAttestError {
    #[error("unsafe persona filesystem attestation: {0}")]
    Unsafe(String),
    #[error("persona filesystem attestation I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Artifact(#[from] PersonaArtifactError),
    #[error(transparent)]
    Materialization(#[from] PersonaMaterializeError),
    #[error(transparent)]
    Plan(#[from] PersonaPlanError),
    #[error(transparent)]
    Schedule(#[from] PersonaScheduleError),
    #[error(transparent)]
    Render(#[from] RenderArtifactError),
    #[error("persona filesystem attestation output already exists: {0}")]
    AlreadyExists(PathBuf),
    #[error("persona filesystem attestation publication is indeterminate: {0}")]
    Indeterminate(String),
    #[error("persona filesystem attestation is unsupported on this platform")]
    Unsupported,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PersonaFilesystemAttestation {
    pub schema: String,
    pub root: String,
    #[serde(with = "kio_core::identity_serde::u64_hex")]
    pub filesystem_device: u64,
    pub fixture_id: String,
    pub profile: PersonaProfile,
    pub plan_digest: String,
    pub materialization_sha256: String,
    pub materialization_bytes: u64,
    pub plan_sha256: String,
    pub plan_bytes: u64,
    pub schedule_sha256: String,
    pub schedule_bytes: u64,
    pub render_sha256: String,
    pub render_bytes: u64,
    pub directory_merkle_sha256: String,
    pub entries: u64,
    pub files: u64,
    pub directories: u64,
    pub bytes: u64,
    pub claims: AttestationClaims,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AttestationClaims {
    pub actual_kio_evidence: bool,
    pub history_ready: bool,
}

struct BoundRoot {
    handle: fs::File,
    identity: cap_fs::Metadata,
    public: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Snapshot {
    dev: u64,
    ino: u64,
    size: u64,
    mode: u32,
    nlink: u64,
    mtime: i64,
    mtime_nsec: i64,
    ctime: i64,
    ctime_nsec: i64,
}

/// Attest a fully materialized Rust persona root and create `out` once.
///
/// Both paths must be absolute, lexically normalized paths.  `out` must not
/// reside inside `root`; publication uses the existing descriptor-safe,
/// fsyncing create-only artifact primitive.
pub fn attest(root: &Path, out: &Path) -> Result<PersonaFilesystemAttestation, PersonaAttestError> {
    preflight_platform()?;
    let bound = bind_root(root)?;
    let output = persona_artifact::normalize_persona_path(out)?;
    validate_output(&bound, &output)?;
    recheck_root(&bound)?;

    let before_root = snapshot(&cap_fs::Metadata::from_file(&bound.handle)?)?;
    let names = exact_names(&bound)?;
    if names.len() > MAX_ENTRIES || MAX_DEPTH != 1 {
        return bad("materialized tree exceeds fixed bounds");
    }

    // All four leaves are opened below the one retained root descriptor. This
    // prevents a root replacement from mixing a record with artifacts from a
    // different directory between independent public path walks.
    let record_source = persona_artifact::bind_strict_at(
        &bound.handle,
        &bound.public,
        RECORD_NAME,
        MAX_REPORT_BYTES,
    )?;
    let plan_source = persona_artifact::bind_strict_at(
        &bound.handle,
        &bound.public,
        PLAN_NAME,
        persona_plan::MAX_CANONICAL_BYTES,
    )?;
    let schedule_source = persona_artifact::bind_strict_at(
        &bound.handle,
        &bound.public,
        SCHEDULE_NAME,
        persona_schedule::MAX_CANONICAL_BYTES,
    )?;
    let render_source = persona_artifact::bind_strict_at(
        &bound.handle,
        &bound.public,
        RENDER_NAME,
        persona_render_artifact::MAX_CANONICAL_BYTES,
    )?;
    let record = PersonaMaterializationRecord::parse_canonical(record_source.bytes())?;
    let plan = PersonaPlan::parse_canonical(plan_source.bytes())?;
    let _schedule = SuiteSchedule::parse_canonical(&plan, schedule_source.bytes())?;
    let _render = RenderArtifact::parse_canonical(&plan, render_source.bytes())?;
    verify_record(
        &bound,
        &record,
        &plan,
        &plan_source,
        &schedule_source,
        &render_source,
    )?;
    for name in [RECORD_NAME, PLAN_NAME, SCHEDULE_NAME, RENDER_NAME] {
        let metadata = cap_fs::stat(&bound.handle, Path::new(name), cap_fs::FollowSymlinks::No)?;
        if device(&metadata) != device(&bound.identity) {
            return bad("materialized artifact is on a different filesystem device");
        }
        #[cfg(unix)]
        {
            use cap_fs::MetadataExt;
            if metadata.mode() & 0o777 != 0o600 || metadata.uid() != unsafe { libc::geteuid() } {
                return bad("materialized artifact mode or owner is not canonical");
            }
        }
    }
    let initial = build_report(
        &bound,
        &record,
        &plan,
        &record_source,
        &plan_source,
        &schedule_source,
        &render_source,
    )?;

    // Double-read record/root/sources after the complete parse and tree hash.
    recheck_bundle(
        &bound,
        &record_source,
        &plan_source,
        &schedule_source,
        &render_source,
        &before_root,
    )?;
    let record_again = PersonaMaterializationRecord::parse_canonical(record_source.bytes())?;
    if record_again != record
        || build_report(
            &bound,
            &record,
            &plan,
            &record_source,
            &plan_source,
            &schedule_source,
            &render_source,
        )? != initial
    {
        return bad("materialization changed during attestation");
    }
    let bytes = canonical_report_bytes(&initial)?;
    if bytes.len() > MAX_REPORT_BYTES {
        return bad("attestation report exceeds bound");
    }
    let prepared = persona_artifact::prepare_create_only(&output, &bytes, MAX_REPORT_BYTES)
        .map_err(publication_error)?;
    run_before_publish_hook(&output);
    recheck_bundle(
        &bound,
        &record_source,
        &plan_source,
        &schedule_source,
        &render_source,
        &before_root,
    )?;
    let published = prepared.publish().map_err(publication_error)?;
    if published != output {
        return bad("published report path identity differs");
    }
    run_after_publish_hook(&output);
    recheck_bundle(
        &bound,
        &record_source,
        &plan_source,
        &schedule_source,
        &render_source,
        &before_root,
    )
    .map_err(|error| PersonaAttestError::Indeterminate(error.to_string()))?;
    Ok(initial)
}

fn verify_record(
    root: &BoundRoot,
    record: &PersonaMaterializationRecord,
    plan: &PersonaPlan,
    plan_source: &StrictArtifact,
    schedule_source: &StrictArtifact,
    render_source: &StrictArtifact,
) -> Result<(), PersonaAttestError> {
    let root_text = root
        .public
        .to_str()
        .ok_or_else(|| PersonaAttestError::Unsafe("root is not UTF-8".into()))?;
    if record.destination_root != root_text
        || record.filesystem_device != device(&root.identity)
        || record.fixture_id != plan.fixture_id
        || record.profile != plan.profile
        || record.plan.digest != plan.digest()?
        || record.plan.sha256 != hash_bytes(plan_source.bytes())
        || record.plan.bytes != plan_source.bytes().len() as u64
        || record.schedule.sha256 != hash_bytes(schedule_source.bytes())
        || record.schedule.bytes != schedule_source.bytes().len() as u64
        || record.render.sha256 != hash_bytes(render_source.bytes())
        || record.render.bytes != render_source.bytes().len() as u64
        || record.claims.actual_kio_evidence
        || record.claims.history_ready
        || record.claims.sources_materialized
    {
        return bad("materialization record does not bind retained root artifacts");
    }
    Ok(())
}

fn build_report(
    root: &BoundRoot,
    record_value: &PersonaMaterializationRecord,
    plan_value: &PersonaPlan,
    record: &StrictArtifact,
    plan: &StrictArtifact,
    schedule: &StrictArtifact,
    render: &StrictArtifact,
) -> Result<PersonaFilesystemAttestation, PersonaAttestError> {
    let merkle = directory_merkle(record, plan, schedule, render)?;
    let bytes = record.bytes().len() as u64
        + plan.bytes().len() as u64
        + schedule.bytes().len() as u64
        + render.bytes().len() as u64;
    Ok(PersonaFilesystemAttestation {
        schema: REPORT_SCHEMA.into(),
        root: root
            .public
            .to_str()
            .ok_or_else(|| PersonaAttestError::Unsafe("root is not UTF-8".into()))?
            .into(),
        filesystem_device: device(&root.identity),
        fixture_id: plan_value.fixture_id.clone(),
        profile: plan_value.profile,
        plan_digest: plan_value.digest()?,
        materialization_sha256: hash_bytes(record.bytes()),
        materialization_bytes: record_value.canonical_bytes()?.len() as u64,
        plan_sha256: hash_bytes(plan.bytes()),
        plan_bytes: plan.bytes().len() as u64,
        schedule_sha256: hash_bytes(schedule.bytes()),
        schedule_bytes: schedule.bytes().len() as u64,
        render_sha256: hash_bytes(render.bytes()),
        render_bytes: render.bytes().len() as u64,
        directory_merkle_sha256: merkle,
        entries: MAX_ENTRIES as u64,
        files: MAX_ENTRIES as u64,
        directories: 1,
        bytes,
        claims: AttestationClaims {
            actual_kio_evidence: false,
            history_ready: false,
        },
    })
}

fn directory_merkle(
    record: &StrictArtifact,
    plan: &StrictArtifact,
    schedule: &StrictArtifact,
    render: &StrictArtifact,
) -> Result<String, PersonaAttestError> {
    let mut entries = Vec::new();
    for source in [record, plan, schedule, render] {
        let name = source
            .path()
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| PersonaAttestError::Unsafe("artifact filename is not UTF-8".into()))?;
        entries.push(serde_json::json!({"name": name, "sha256": hash_bytes(source.bytes()), "bytes": source.bytes().len()}));
    }
    entries.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
    Ok(hash_bytes(
        &canonical_json_bytes(&serde_json::json!({"entries": entries}))
            .map_err(|error| PersonaAttestError::Unsafe(error.to_string()))?,
    ))
}

fn canonical_report_bytes(
    report: &PersonaFilesystemAttestation,
) -> Result<Vec<u8>, PersonaAttestError> {
    let mut bytes = canonical_json_bytes(
        &serde_json::to_value(report)
            .map_err(|error| PersonaAttestError::Unsafe(error.to_string()))?,
    )
    .map_err(|error| PersonaAttestError::Unsafe(error.to_string()))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn recheck_bundle(
    root: &BoundRoot,
    record: &StrictArtifact,
    plan: &StrictArtifact,
    schedule: &StrictArtifact,
    render: &StrictArtifact,
    root_before: &Snapshot,
) -> Result<(), PersonaAttestError> {
    recheck_root(root)?;
    if snapshot(&cap_fs::Metadata::from_file(&root.handle)?)? != *root_before {
        return bad("root metadata changed during attestation");
    }
    exact_names(root)?;
    record.recheck()?;
    plan.recheck()?;
    schedule.recheck()?;
    render.recheck()?;
    recheck_root(root)
}

fn exact_names(root: &BoundRoot) -> Result<BTreeSet<String>, PersonaAttestError> {
    let expected: BTreeSet<String> = [RECORD_NAME, PLAN_NAME, SCHEDULE_NAME, RENDER_NAME]
        .into_iter()
        .map(str::to_owned)
        .collect();
    let mut actual = BTreeSet::new();
    let mut folded = BTreeSet::new();
    for entry in cap_fs::read_dir(&root.handle, Path::new("."))? {
        let name = entry?
            .file_name()
            .to_str()
            .ok_or_else(|| PersonaAttestError::Unsafe("materialized entry is not UTF-8".into()))?
            .to_owned();
        if name.len() > MAX_COMPONENT_BYTES
            || Path::new(&name).components().count() != 1
            || name.nfc().collect::<String>() != name
        {
            return bad("materialized entry violates component or NFC bound");
        }
        let key = name.nfc().flat_map(char::to_lowercase).collect::<String>();
        if !actual.insert(name) || !folded.insert(key) || actual.len() > MAX_ENTRIES {
            return bad("materialized entries collide or exceed bounds");
        }
    }
    if actual != expected {
        return bad("materialized root must contain exactly four canonical files");
    }
    Ok(actual)
}

fn bind_root(root: &Path) -> Result<BoundRoot, PersonaAttestError> {
    if !root.is_absolute()
        || root.as_os_str().is_empty()
        || root
            .components()
            .any(|part| matches!(part, Component::CurDir | Component::ParentDir))
        || root.components().count() > MAX_COMPONENTS
    {
        return bad("root must be absolute and lexically normalized");
    }
    let public = persona_artifact::normalize_persona_path(root)?;
    if public
        .components()
        .any(|part| matches!(part, Component::Normal(value) if value.len() > MAX_COMPONENT_BYTES))
    {
        return bad("root component exceeds bound");
    }
    let mut handle = cap_fs::open_ambient_dir(Path::new("/"), ambient_authority())?;
    for component in public.components().skip(1) {
        let Component::Normal(part) = component else {
            return bad("root must be normalized");
        };
        handle = cap_fs::open_dir_nofollow(&handle, Path::new(part))?;
    }
    let identity = cap_fs::Metadata::from_file(&handle)?;
    if !identity.is_dir() || identity.file_type().is_symlink() {
        return bad("root is not a real directory");
    }
    #[cfg(unix)]
    {
        use cap_fs::MetadataExt;
        if identity.mode() & 0o777 != 0o700 || identity.uid() != unsafe { libc::geteuid() } {
            return bad("materialized root mode or owner is not canonical");
        }
    }
    Ok(BoundRoot {
        handle,
        identity,
        public,
    })
}

fn recheck_root(root: &BoundRoot) -> Result<(), PersonaAttestError> {
    let retained = cap_fs::Metadata::from_file(&root.handle)?;
    let rebound = bind_root(&root.public)?;
    if !retained.is_dir()
        || retained.file_type().is_symlink()
        || !same(&root.identity, &retained)
        || !same(&root.identity, &rebound.identity)
    {
        return bad("attested root identity changed");
    }
    Ok(())
}

fn validate_output(root: &BoundRoot, out: &Path) -> Result<(), PersonaAttestError> {
    if !out.is_absolute()
        || out
            .components()
            .any(|part| matches!(part, Component::CurDir | Component::ParentDir))
        || out.components().count() > MAX_COMPONENTS
        || out.components().any(
            |part| matches!(part, Component::Normal(value) if value.len() > MAX_COMPONENT_BYTES),
        )
    {
        return bad("output must be absolute and lexically normalized");
    }
    if out == root.public || out.starts_with(&root.public) {
        return bad("attestation output overlaps attested root");
    }
    // Lexical separation is insufficient when another absolute spelling is a
    // bind mount of the attested directory.  Walk the output parent no-follow
    // and reject whenever the retained root appears anywhere in that chain.
    let parent = out
        .parent()
        .ok_or_else(|| PersonaAttestError::Unsafe("output has no parent".into()))?;
    let mut handle = cap_fs::open_ambient_dir(Path::new("/"), ambient_authority())?;
    if same(&root.identity, &cap_fs::Metadata::from_file(&handle)?) {
        return bad("attestation output parent aliases attested root");
    }
    for component in parent.components().skip(1) {
        let Component::Normal(part) = component else {
            return bad("attestation output must be normalized");
        };
        handle = cap_fs::open_dir_nofollow(&handle, Path::new(part))?;
        if same(&root.identity, &cap_fs::Metadata::from_file(&handle)?) {
            return bad("attestation output parent aliases attested root");
        }
    }
    Ok(())
}

fn preflight_platform() -> Result<(), PersonaAttestError> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(PersonaAttestError::Unsupported)
    }
}

#[cfg(unix)]
fn snapshot(metadata: &cap_fs::Metadata) -> Result<Snapshot, PersonaAttestError> {
    use cap_fs::MetadataExt;
    Ok(Snapshot {
        dev: metadata.dev(),
        ino: metadata.ino(),
        size: metadata.len(),
        mode: metadata.mode(),
        nlink: metadata.nlink(),
        mtime: metadata.mtime(),
        mtime_nsec: metadata.mtime_nsec(),
        ctime: metadata.ctime(),
        ctime_nsec: metadata.ctime_nsec(),
    })
}
#[cfg(not(unix))]
fn snapshot(_: &cap_fs::Metadata) -> Result<Snapshot, PersonaAttestError> {
    Err(PersonaAttestError::Unsupported)
}
#[cfg(unix)]
fn device(metadata: &cap_fs::Metadata) -> u64 {
    use cap_fs::MetadataExt;
    metadata.dev()
}
#[cfg(not(unix))]
fn device(_: &cap_fs::Metadata) -> u64 {
    0
}
#[cfg(unix)]
fn same(left: &cap_fs::Metadata, right: &cap_fs::Metadata) -> bool {
    use cap_fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}
#[cfg(not(unix))]
fn same(_: &cap_fs::Metadata, _: &cap_fs::Metadata) -> bool {
    false
}
fn bad<T>(message: impl Into<String>) -> Result<T, PersonaAttestError> {
    Err(PersonaAttestError::Unsafe(message.into()))
}

fn publication_error(error: PersonaArtifactError) -> PersonaAttestError {
    match error {
        PersonaArtifactError::AlreadyExists(path) => PersonaAttestError::AlreadyExists(path),
        PersonaArtifactError::Indeterminate(message) => PersonaAttestError::Indeterminate(message),
        other => PersonaAttestError::Artifact(other),
    }
}

// This seam is deliberately test-only. It proves that once the no-replace
// rename has succeeded, any later input recheck error is reported as
// indeterminate and the visible report is not removed or overwritten.
#[cfg(test)]
type AfterPublishHook = Box<dyn FnOnce() + Send>;
#[cfg(test)]
static AFTER_PUBLISH_HOOK: OnceLock<Mutex<BTreeMap<PathBuf, AfterPublishHook>>> = OnceLock::new();
#[cfg(test)]
static BEFORE_PUBLISH_HOOK: OnceLock<Mutex<BTreeMap<PathBuf, AfterPublishHook>>> = OnceLock::new();
#[cfg(test)]
fn install_after_publish_hook(output: PathBuf, hook: AfterPublishHook) {
    let mut slot = AFTER_PUBLISH_HOOK
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    match slot.entry(output) {
        Entry::Vacant(entry) => {
            entry.insert(hook);
        }
        Entry::Occupied(_) => panic!("duplicate after-publish hook"),
    }
}
#[cfg(test)]
fn run_after_publish_hook(output: &Path) {
    if let Some(hook) = AFTER_PUBLISH_HOOK
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .remove(output)
    {
        hook();
    }
}
#[cfg(not(test))]
fn run_after_publish_hook(_: &Path) {}
#[cfg(test)]
fn install_before_publish_hook(output: PathBuf, hook: AfterPublishHook) {
    let mut slot = BEFORE_PUBLISH_HOOK
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    match slot.entry(output) {
        Entry::Vacant(entry) => {
            entry.insert(hook);
        }
        Entry::Occupied(_) => panic!("duplicate before-publish hook"),
    }
}
#[cfg(test)]
fn run_before_publish_hook(output: &Path) {
    if let Some(hook) = BEFORE_PUBLISH_HOOK
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .remove(output)
    {
        hook();
    }
}
#[cfg(not(test))]
fn run_before_publish_hook(_: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filesystem_device_jcs_preserves_all_bits() {
        for value in [(1_u64 << 53) + 1, 9_851_624_185_183_609, u64::MAX] {
            let mut report = PersonaFilesystemAttestation {
                schema: REPORT_SCHEMA.into(),
                root: "/fixture".into(),
                filesystem_device: value,
                fixture_id: "fixture".into(),
                profile: PersonaProfile::Tiny,
                plan_digest: "0".repeat(64),
                materialization_sha256: "0".repeat(64),
                materialization_bytes: 1,
                plan_sha256: "0".repeat(64),
                plan_bytes: 1,
                schedule_sha256: "0".repeat(64),
                schedule_bytes: 1,
                render_sha256: "0".repeat(64),
                render_bytes: 1,
                directory_merkle_sha256: "0".repeat(64),
                entries: 1,
                files: 1,
                directories: 1,
                bytes: 1,
                claims: AttestationClaims {
                    actual_kio_evidence: false,
                    history_ready: false,
                },
            };
            let bytes = canonical_report_bytes(&report).unwrap();
            assert_eq!(
                serde_json::from_slice::<PersonaFilesystemAttestation>(&bytes).unwrap(),
                report
            );
            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(json["filesystem_device"], format!("{value:016x}"));
            report.filesystem_device -= 1;
            assert_ne!(canonical_report_bytes(&report).unwrap(), bytes);
            for invalid in [
                serde_json::json!(value),
                serde_json::json!("FFFFFFFFFFFFFFFF"),
                serde_json::json!("1"),
            ] {
                let mut invalid_report = json.clone();
                invalid_report["filesystem_device"] = invalid;
                assert!(
                    serde_json::from_value::<PersonaFilesystemAttestation>(invalid_report).is_err()
                );
            }
        }
    }

    use crate::{
        persona_materialize::{MaterializeRequest, materialize},
        persona_plan::PersonaProfile,
        persona_render_artifact::RenderArtifact,
        persona_schedule::build_suite_schedule,
    };
    use std::{
        fs,
        sync::{
            Arc, OnceLock,
            atomic::{AtomicUsize, Ordering},
        },
    };
    use tempfile::tempdir;

    static TINY_BUNDLE: OnceLock<(Vec<u8>, Vec<u8>, Vec<u8>)> = OnceLock::new();

    fn tiny_bundle() -> &'static (Vec<u8>, Vec<u8>, Vec<u8>) {
        TINY_BUNDLE.get_or_init(|| {
            let plan = crate::persona_plan::frozen_plan(PersonaProfile::Tiny);
            let schedule = build_suite_schedule(&plan).unwrap();
            let render = RenderArtifact::build(&plan).unwrap();
            (
                plan.canonical_bytes().unwrap(),
                schedule.canonical_bytes().unwrap(),
                render.canonical_bytes().unwrap(),
            )
        })
    }

    fn bundle() -> (tempfile::TempDir, PathBuf) {
        let temp = tempdir().unwrap();
        let base = fs::canonicalize(temp.path()).unwrap();
        let (plan, schedule, render) = tiny_bundle();
        fs::write(base.join("plan.json"), plan).unwrap();
        fs::write(base.join("schedule.json"), schedule).unwrap();
        fs::write(base.join("render.json"), render).unwrap();
        let root = base.join("materialized");
        materialize(MaterializeRequest {
            plan: &base.join("plan.json"),
            schedule: &base.join("schedule.json"),
            render: &base.join("render.json"),
            destination: &root,
            replay_id: "replay-01",
        })
        .unwrap();
        (temp, root)
    }
    #[test]
    fn duplicate_hook_install_keeps_original_pending_hook() {
        let temp = tempdir().unwrap();
        let output = fs::canonicalize(temp.path())
            .unwrap()
            .join("hook-report.json");
        let calls = Arc::new(AtomicUsize::new(0));
        let original_calls = Arc::clone(&calls);
        install_before_publish_hook(
            output.clone(),
            Box::new(move || {
                original_calls.fetch_add(1, Ordering::SeqCst);
            }),
        );
        let replacement_calls = Arc::clone(&calls);
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                install_before_publish_hook(
                    output.clone(),
                    Box::new(move || {
                        replacement_calls.fetch_add(100, Ordering::SeqCst);
                    }),
                );
            }))
            .is_err()
        );
        run_before_publish_hook(&output);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn attests_stably_and_creates_only_once() {
        let (_temp, root) = bundle();
        let out = root.parent().unwrap().join("report.json");
        let first = attest(&root, &out).unwrap();
        assert_eq!(first.schema, REPORT_SCHEMA);
        assert_eq!(
            first.claims,
            AttestationClaims {
                actual_kio_evidence: false,
                history_ready: false
            }
        );
        assert!(attest(&root, &out).is_err());
        let second = attest(&root, &root.parent().unwrap().join("report-2.json")).unwrap();
        assert_eq!(first, second);
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn rejects_opaque_record_and_output_overlap() {
        let (_temp, root) = bundle();
        fs::write(root.join(RECORD_NAME), b"{\"opaque\":true}\n").unwrap();
        assert!(attest(&root, &root.parent().unwrap().join("report.json")).is_err());
        let (_temp, root) = bundle();
        assert!(attest(&root, &root.join("report.json")).is_err());
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn rejects_cross_bound_record() {
        let (_first_temp, first) = bundle();
        let (_second_temp, second) = bundle();
        fs::copy(second.join(RECORD_NAME), first.join(RECORD_NAME)).unwrap();
        assert!(attest(&first, &first.parent().unwrap().join("report.json")).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn post_publication_same_content_inode_replacement_is_indeterminate() {
        let (_temp, root) = bundle();
        let out = root.parent().unwrap().join("report.json");
        let plan = root.join(PLAN_NAME);
        let replacement = root.parent().unwrap().join("replacement-plan.json");
        fs::copy(&plan, &replacement).unwrap();
        install_after_publish_hook(
            out.clone(),
            Box::new(move || {
                fs::rename(&plan, plan.with_extension("old")).unwrap();
                fs::rename(&replacement, &plan).unwrap();
            }),
        );
        assert!(matches!(
            attest(&root, &out),
            Err(PersonaAttestError::Indeterminate(_))
        ));
        assert!(
            out.is_file(),
            "attestation may already be visible; never remove it"
        );
    }
    #[cfg(unix)]
    #[test]
    fn prepublication_replacement_rejects_without_output() {
        let (_temp, root) = bundle();
        let out = root.parent().unwrap().join("report.json");
        let plan = root.join(PLAN_NAME);
        let replacement = root.parent().unwrap().join("replacement-plan.json");
        fs::copy(&plan, &replacement).unwrap();
        install_before_publish_hook(
            out.clone(),
            Box::new(move || {
                fs::rename(&plan, plan.with_extension("old")).unwrap();
                fs::rename(&replacement, &plan).unwrap();
            }),
        );
        assert!(attest(&root, &out).is_err());
        assert!(!out.exists(), "barrier fails before no-replace publication");
    }
    #[cfg(unix)]
    #[test]
    fn prepublication_root_and_record_replacements_reject() {
        let (_temp, root) = bundle();
        let out = root.parent().unwrap().join("report-root.json");
        let public_root = root.clone();
        install_before_publish_hook(
            out.clone(),
            Box::new(move || {
                fs::rename(&public_root, public_root.with_extension("old")).unwrap();
                fs::create_dir(&public_root).unwrap();
            }),
        );
        assert!(attest(&root, &out).is_err());
        assert!(!out.exists());

        let (_temp, root) = bundle();
        let out = root.parent().unwrap().join("report-record.json");
        let record = root.join(RECORD_NAME);
        let replacement = root.parent().unwrap().join("replacement-record.json");
        fs::copy(&record, &replacement).unwrap();
        install_before_publish_hook(
            out.clone(),
            Box::new(move || {
                fs::rename(&record, record.with_extension("old")).unwrap();
                fs::rename(&replacement, &record).unwrap();
            }),
        );
        assert!(attest(&root, &out).is_err());
        assert!(!out.exists());
    }
    #[cfg(unix)]
    #[test]
    fn rejects_symlink_hardlink_and_extra_casefold_entries() {
        use std::os::unix::fs::symlink;
        let (_temp, root) = bundle();
        let outside = root.parent().unwrap().join("outside");
        fs::write(&outside, b"x").unwrap();
        fs::remove_file(root.join(PLAN_NAME)).unwrap();
        symlink(&outside, root.join(PLAN_NAME)).unwrap();
        assert!(attest(&root, &root.parent().unwrap().join("report.json")).is_err());
        let (_temp, root) = bundle();
        fs::write(root.join("PERSONA-PLAN.JSON"), b"x").unwrap();
        assert!(attest(&root, &root.parent().unwrap().join("report.json")).is_err());
        let (_temp, root) = bundle();
        let copy = root.parent().unwrap().join("copy-plan.json");
        fs::copy(root.join(PLAN_NAME), &copy).unwrap();
        fs::remove_file(root.join(PLAN_NAME)).unwrap();
        fs::hard_link(&copy, root.join(PLAN_NAME)).unwrap();
        assert!(attest(&root, &root.parent().unwrap().join("report.json")).is_err());
        let (_temp, root) = bundle();
        fs::remove_file(root.join(RENDER_NAME)).unwrap();
        let fifo =
            std::ffi::CString::new(root.join(RENDER_NAME).as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: the path is NUL-free, inside this test's private temporary
        // directory, and the FIFO is never opened for reading or writing.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(attest(&root, &root.parent().unwrap().join("report.json")).is_err());
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    #[test]
    fn unsupported_platform_fails_before_path_resolution_or_output_mutation() {
        let out = Path::new("relative-output-that-must-not-exist.json");
        assert!(matches!(
            attest(Path::new("relative-root-that-must-not-exist"), out),
            Err(PersonaAttestError::Unsupported)
        ));
        assert!(!out.exists());
    }
}
