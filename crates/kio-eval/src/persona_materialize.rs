//! Create-only publication of a verified persona artifact bundle.
//!
//! This deliberately has no "resume" path: a destination is either absent and
//! atomically published, or it is reported as occupied without touching it.

use crate::{
    boundary::sync_retained_directory,
    persona_artifact,
    persona_consumer::{CanonicalPersonaBundle, CanonicalPersonaBundleError},
    persona_plan::{FIXTURE_ID, PersonaProfile},
    scale_fixture::rename_noreplace,
};
use cap_primitives::{ambient_authority, fs as cap_fs};
use kio_core::cas::{canonical_json_bytes, hash_bytes};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::collections::BTreeMap;
#[cfg(test)]
use std::sync::{Mutex, OnceLock};
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};
use thiserror::Error;

const MAX_COMPONENTS: usize = 64;
const MAX_COMPONENT_BYTES: usize = 255;
const MAX_RECORD_BYTES: usize = 16 * 1024;

#[derive(Debug, Error)]
pub enum PersonaMaterializeError {
    #[error("invalid persona materialization: {0}")]
    Unsafe(String),
    #[error("persona materialization I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Bundle(#[from] CanonicalPersonaBundleError),
    #[error("persona destination already exists: {0}")]
    AlreadyExists(PathBuf),
    #[error("persona materialization is indeterminate: {0}")]
    Indeterminate(String),
    #[error("atomic directory no-replace publication is unsupported on this platform")]
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializeRequest<'a> {
    pub plan: &'a Path,
    pub schedule: &'a Path,
    pub render: &'a Path,
    pub destination: &'a Path,
    pub replay_id: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Materialization {
    pub destination: PathBuf,
    pub record: PersonaMaterializationRecord,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PersonaMaterializationRecord {
    pub schema: String,
    pub fixture_id: String,
    pub profile: PersonaProfile,
    pub replay_id: String,
    pub destination_root: String,
    #[serde(with = "kio_core::identity_serde::u64_hex")]
    pub filesystem_device: u64,
    pub plan: PlanRecord,
    pub schedule: ArtifactRecord,
    pub render: ArtifactRecord,
    pub claims: Claims,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlanRecord {
    pub digest: String,
    pub sha256: String,
    pub bytes: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRecord {
    pub sha256: String,
    pub bytes: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Claims {
    pub sources_materialized: bool,
    pub actual_kio_evidence: bool,
    pub history_ready: bool,
}

impl PersonaMaterializationRecord {
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, PersonaMaterializeError> {
        let mut bytes = canonical_json_bytes(
            &serde_json::to_value(self)
                .map_err(|e| PersonaMaterializeError::Unsafe(e.to_string()))?,
        )
        .map_err(|e| PersonaMaterializeError::Unsafe(e.to_string()))?;
        bytes.push(b'\n');
        Ok(bytes)
    }
    pub fn parse_canonical(bytes: &[u8]) -> Result<Self, PersonaMaterializeError> {
        if bytes.len() > MAX_RECORD_BYTES || !bytes.ends_with(b"\n") {
            return bad("invalid materialization record size or terminator");
        }
        let record: Self = serde_json::from_slice(bytes)
            .map_err(|e| PersonaMaterializeError::Unsafe(e.to_string()))?;
        if record.schema != "kio.persona.materialization/v1" {
            return bad("unsupported materialization record schema");
        }
        let record_destination = Path::new(&record.destination_root);
        if record.fixture_id != FIXTURE_ID
            || !valid_replay(&record.replay_id)
            || record.destination_root.is_empty()
            || record.destination_root.len() > 16 * 1024
            || !valid_record_destination(&record.destination_root)
            || persona_artifact::normalize_persona_path(record_destination)
                .map_err(|error| PersonaMaterializeError::Unsafe(error.to_string()))?
                != record_destination
            || record.plan.digest != record.plan.sha256
            || !valid_hash(&record.plan.digest)
            || !valid_hash(&record.plan.sha256)
            || !valid_hash(&record.schedule.sha256)
            || !valid_hash(&record.render.sha256)
            || record.plan.bytes == 0
            || record.schedule.bytes == 0
            || record.render.bytes == 0
            || record.plan.bytes > crate::persona_plan::MAX_CANONICAL_BYTES as u64
            || record.schedule.bytes > crate::persona_schedule::MAX_CANONICAL_BYTES as u64
            || record.render.bytes > crate::persona_render_artifact::MAX_CANONICAL_BYTES as u64
            || record.claims.sources_materialized
            || record.claims.actual_kio_evidence
            || record.claims.history_ready
        {
            return bad("materialization record has invalid closed semantics");
        }
        if record.canonical_bytes()? != bytes {
            return bad("materialization record is not canonical JCS+LF");
        }
        Ok(record)
    }
}

#[derive(Debug)]
struct Parent {
    handle: fs::File,
    metadata: fs::Metadata,
    public: PathBuf,
    leaf: String,
}

/// Materialize exactly one closed bundle.  No input is parsed except by the
/// Rust-owned bundle consumer.
pub fn materialize(
    request: MaterializeRequest<'_>,
) -> Result<Materialization, PersonaMaterializeError> {
    preflight_platform()?;
    if !valid_replay(request.replay_id) {
        return bad("replay id is outside the closed set");
    }
    let parent = bind_parent(request.destination)?;
    // `bind_parent` applies the Darwin /tmp and /var aliases before retaining
    // the parent descriptor. Every public path identity must use that same
    // bound spelling; otherwise the returned value and sealed record could
    // identify a different lexical destination than the one we published.
    let canonical_destination = parent.public.join(&parent.leaf);
    recheck_parent(&parent)?;
    match cap_fs::stat(
        &parent.handle,
        Path::new(&parent.leaf),
        cap_fs::FollowSymlinks::No,
    ) {
        Ok(metadata)
            if metadata.file_type().is_symlink()
                || (!metadata.is_dir() && !metadata.file_type().is_file())
                || (metadata.file_type().is_file() && links(&metadata) != 1) =>
        {
            return bad("destination existing object is unsafe");
        }
        Ok(_) => {
            return Err(PersonaMaterializeError::AlreadyExists(
                canonical_destination,
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let bundle = CanonicalPersonaBundle::load(request.plan, request.schedule, request.render)?;
    let stage_name = stage_name(&parent.leaf)?;
    // A stale stage is evidence of an interrupted prior attempt. Never adopt or erase it.
    match cap_fs::stat(
        &parent.handle,
        Path::new(&stage_name),
        cap_fs::FollowSymlinks::No,
    ) {
        Ok(_) => return bad("pre-existing materialization staging directory"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let directory_options = cap_fs::DirOptions::new();
    #[cfg(unix)]
    let directory_options = {
        use cap_fs::DirBuilderExt;
        let mut directory_options = directory_options;
        directory_options.mode(0o700);
        directory_options
    };
    cap_fs::create_dir(&parent.handle, Path::new(&stage_name), &directory_options)?;
    let stage = cap_fs::open_dir_nofollow(&parent.handle, Path::new(&stage_name))?;
    let stage_cap_metadata = cap_fs::Metadata::from_file(&stage)?;
    let stage_metadata = stage.metadata()?;
    if !stage_cap_metadata.is_dir() || stage_cap_metadata.file_type().is_symlink() {
        return bad("staging is not a real directory");
    }
    let result = (|| {
        let record = build_record(
            &bundle,
            request.replay_id,
            &canonical_destination,
            device(&stage_cap_metadata),
        )?;
        let record_bytes = record.canonical_bytes()?;
        write_file(&stage, "persona-plan.json", bundle.plan_source.bytes())?;
        write_file(
            &stage,
            "persona-schedule.json",
            bundle.schedule_source.bytes(),
        )?;
        write_file(&stage, "persona-render.json", bundle.render_source.bytes())?;
        write_file(&stage, "persona-materialization.json", &record_bytes)?;
        verify_stage(&stage, &bundle, &record_bytes)?;
        sync_retained_directory(&stage, &stage_metadata, &parent.public.join(&stage_name))
            .map_err(|e| PersonaMaterializeError::Unsafe(e.to_string()))?;
        bundle.recheck_sources()?;
        run_before_rename_hook(&canonical_destination);
        bundle.recheck_sources()?;
        verify_stage(&stage, &bundle, &record_bytes)?;
        recheck_parent(&parent)?;
        recheck_named_directory(&parent, &stage_name, &stage_cap_metadata)?;
        // Once rename is attempted, every error is ambiguous unless we can prove
        // the target was concurrently occupied before the call.
        if let Err(error) =
            rename_noreplace(&parent.handle, &stage_name, &parent.handle, &parent.leaf)
        {
            return Err(PersonaMaterializeError::Indeterminate(error.to_string()));
        }
        sync_retained_directory(&parent.handle, &parent.metadata, &parent.public)
            .map_err(|e| PersonaMaterializeError::Indeterminate(e.to_string()))?;
        verify_published(&parent, &stage_cap_metadata, &bundle, &record_bytes)
            .map_err(|e| PersonaMaterializeError::Indeterminate(e.to_string()))?;
        Ok(Materialization {
            destination: canonical_destination,
            record,
        })
    })();
    // Failed stages are forensic evidence. Never delete a name that could have
    // been replaced after our final descriptor-relative observation.
    result
}

fn build_record(
    bundle: &CanonicalPersonaBundle,
    replay: &str,
    destination: &Path,
    filesystem_device: u64,
) -> Result<PersonaMaterializationRecord, PersonaMaterializeError> {
    let destination_root = destination
        .to_str()
        .ok_or_else(|| PersonaMaterializeError::Unsafe("destination is not UTF-8".into()))?
        .to_owned();
    Ok(PersonaMaterializationRecord {
        schema: "kio.persona.materialization/v1".into(),
        fixture_id: bundle.identity.fixture_id.clone(),
        profile: bundle.identity.profile,
        replay_id: replay.into(),
        destination_root,
        filesystem_device,
        plan: PlanRecord {
            digest: bundle.identity.plan_digest.clone(),
            sha256: bundle.identity.plan_hash.clone(),
            bytes: bundle.identity.plan_len,
        },
        schedule: ArtifactRecord {
            sha256: bundle.identity.schedule_hash.clone(),
            bytes: bundle.identity.schedule_len,
        },
        render: ArtifactRecord {
            sha256: bundle.identity.render_hash.clone(),
            bytes: bundle.identity.render_len,
        },
        claims: Claims {
            sources_materialized: false,
            actual_kio_evidence: false,
            history_ready: false,
        },
    })
}

fn write_file(parent: &fs::File, name: &str, bytes: &[u8]) -> Result<(), PersonaMaterializeError> {
    let mut options = cap_fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use cap_fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = cap_fs::open(parent, Path::new(name), &options)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    let metadata = cap_fs::Metadata::from_file(&file)?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || links(&metadata) != 1
        || metadata.len() != bytes.len() as u64
    {
        return bad("staged output is not a single-link regular file");
    }
    Ok(())
}
fn verify_stage(
    stage: &fs::File,
    bundle: &CanonicalPersonaBundle,
    record: &[u8],
) -> Result<(), PersonaMaterializeError> {
    exact_names(stage)?;
    for (name, expected) in [
        ("persona-plan.json", bundle.plan_source.bytes()),
        ("persona-schedule.json", bundle.schedule_source.bytes()),
        ("persona-render.json", bundle.render_source.bytes()),
        ("persona-materialization.json", record),
    ] {
        verify_file(stage, name, expected)?;
    }
    exact_names(stage)?;
    Ok(())
}
fn verify_published(
    parent: &Parent,
    staged_identity: &cap_fs::Metadata,
    bundle: &CanonicalPersonaBundle,
    record: &[u8],
) -> Result<(), PersonaMaterializeError> {
    recheck_parent(parent)?;
    let root = cap_fs::open_dir_nofollow(&parent.handle, Path::new(&parent.leaf))?;
    let metadata = cap_fs::Metadata::from_file(&root)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || !same_cap(staged_identity, &metadata)
    {
        return bad("published destination is not a real directory");
    }
    recheck_named_directory(parent, &parent.leaf, staged_identity)?;
    verify_stage(&root, bundle, record)?;
    recheck_named_directory(parent, &parent.leaf, staged_identity)?;
    verify_stage(&root, bundle, record)?;
    recheck_named_directory(parent, &parent.leaf, staged_identity)?;
    recheck_parent(parent)
}
fn verify_file(
    parent: &fs::File,
    name: &str,
    expected: &[u8],
) -> Result<(), PersonaMaterializeError> {
    let named_before = cap_fs::stat(parent, Path::new(name), cap_fs::FollowSymlinks::No)?;
    if !regular(&named_before, expected.len()) {
        return bad("output is not a bounded single-link regular file");
    }
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let mut file = cap_fs::open(parent, Path::new(name), &options)?;
    let metadata = cap_fs::Metadata::from_file(&file)?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || links(&metadata) != 1
        || metadata.len() != expected.len() as u64
        || !same_cap(&named_before, &metadata)
    {
        return bad("published output is not a single-link regular file");
    }
    let mut actual = Vec::with_capacity(expected.len());
    (&mut file)
        .take(expected.len() as u64 + 1)
        .read_to_end(&mut actual)?;
    let descriptor_after = cap_fs::Metadata::from_file(&file)?;
    let named_after = cap_fs::stat(parent, Path::new(name), cap_fs::FollowSymlinks::No)?;
    if !regular(&descriptor_after, expected.len())
        || !regular(&named_after, expected.len())
        || !same_cap(&named_before, &descriptor_after)
        || !same_cap(&descriptor_after, &named_after)
    {
        return bad("output changed while reading");
    }
    if actual != expected || hash_bytes(&actual) != hash_bytes(expected) {
        return bad("published output bytes changed");
    }
    Ok(())
}
fn preflight_platform() -> Result<(), PersonaMaterializeError> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(PersonaMaterializeError::Unsupported)
    }
}
fn bind_parent(destination: &Path) -> Result<Parent, PersonaMaterializeError> {
    if !destination.is_absolute()
        || destination.as_os_str().is_empty()
        || destination
            .components()
            .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
        || destination.components().count() > MAX_COMPONENTS
    {
        return bad("destination must be absolute and lexically normalized");
    }
    let destination = persona_artifact::normalize_persona_path(destination)
        .map_err(|error| PersonaMaterializeError::Unsafe(error.to_string()))?;
    if destination
        .components()
        .any(|c| matches!(c, Component::Normal(p) if p.len() > MAX_COMPONENT_BYTES))
    {
        return bad("destination component exceeds bound");
    }
    let leaf = destination
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| safe_leaf(n))
        .ok_or_else(|| PersonaMaterializeError::Unsafe("unsafe destination leaf".into()))?
        .to_owned();
    let public = destination
        .parent()
        .ok_or_else(|| PersonaMaterializeError::Unsafe("destination has no parent".into()))?
        .to_owned();
    let mut handle = cap_fs::open_ambient_dir(Path::new("/"), ambient_authority())?;
    for c in public.components().skip(1) {
        let Component::Normal(p) = c else {
            return bad("destination must be normalized");
        };
        handle = cap_fs::open_dir_nofollow(&handle, Path::new(p))?;
    }
    let cap_metadata = cap_fs::Metadata::from_file(&handle)?;
    if !cap_metadata.is_dir() || cap_metadata.file_type().is_symlink() {
        return bad("destination parent is not a real directory");
    }
    let metadata = handle.metadata()?;
    validate_parent_permissions(&metadata)?;
    Ok(Parent {
        handle,
        metadata,
        public,
        leaf,
    })
}
#[cfg(unix)]
fn validate_parent_permissions(metadata: &fs::Metadata) -> Result<(), PersonaMaterializeError> {
    use std::os::unix::fs::MetadataExt;

    let mode = metadata.mode();
    // SAFETY: `geteuid` has no preconditions and only returns process metadata.
    let effective_uid = unsafe { libc::geteuid() };
    let owner_is_trusted = metadata.uid() == effective_uid || metadata.uid() == 0;
    let private_owner = metadata.uid() == effective_uid && mode & 0o022 == 0;
    let sticky_shared = owner_is_trusted && mode & 0o1000 != 0;
    if private_owner || sticky_shared {
        Ok(())
    } else {
        bad("destination parent permits unprotected entry replacement")
    }
}
#[cfg(not(unix))]
fn validate_parent_permissions(_: &fs::Metadata) -> Result<(), PersonaMaterializeError> {
    Err(PersonaMaterializeError::Unsupported)
}
fn recheck_parent(parent: &Parent) -> Result<(), PersonaMaterializeError> {
    let retained = cap_fs::Metadata::from_file(&parent.handle)?;
    let rebound = bind_parent(&parent.public.join(&parent.leaf))?;
    let public = rebound.handle.metadata()?;
    let retained_std = parent.handle.metadata()?;
    if !retained.is_dir()
        || retained.file_type().is_symlink()
        || !public.is_dir()
        || public.file_type().is_symlink()
        || !same_std(&parent.metadata, &retained_std)
        || !same_std(&parent.metadata, &public)
    {
        return bad("destination parent identity changed");
    }
    Ok(())
}
fn recheck_named_directory(
    parent: &Parent,
    name: &str,
    expected: &cap_fs::Metadata,
) -> Result<(), PersonaMaterializeError> {
    let named = cap_fs::stat(&parent.handle, Path::new(name), cap_fs::FollowSymlinks::No)?;
    if !named.is_dir() || named.file_type().is_symlink() || !same_cap(expected, &named) {
        return bad("named directory identity changed");
    }
    Ok(())
}
fn exact_names(dir: &fs::File) -> Result<(), PersonaMaterializeError> {
    let expected: BTreeSet<String> = [
        "persona-plan.json",
        "persona-schedule.json",
        "persona-render.json",
        "persona-materialization.json",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    let mut actual = BTreeSet::new();
    for entry in cap_fs::read_dir(dir, Path::new("."))? {
        let name = entry?
            .file_name()
            .to_str()
            .ok_or_else(|| PersonaMaterializeError::Unsafe("directory entry is not UTF-8".into()))?
            .to_owned();
        if !safe_leaf(&name) || !actual.insert(name) || actual.len() > expected.len() {
            return bad("directory entry set exceeds bounds");
        }
    }
    if actual != expected {
        return bad("directory entry allowlist differs");
    }
    Ok(())
}
fn regular(metadata: &cap_fs::Metadata, maximum: usize) -> bool {
    metadata.file_type().is_file()
        && !metadata.file_type().is_symlink()
        && metadata.len() <= maximum as u64
        && links(metadata) == 1
}
fn valid_replay(value: &str) -> bool {
    matches!(value, "replay-01" | "replay-02" | "replay-03")
}
fn valid_hash(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}
#[cfg(test)]
type BeforeRenameHook = Box<dyn FnOnce() + Send>;
#[cfg(test)]
static BEFORE_RENAME_HOOK: OnceLock<Mutex<BTreeMap<PathBuf, BeforeRenameHook>>> = OnceLock::new();
#[cfg(all(test, unix))]
fn install_before_rename_hook(destination: PathBuf, hook: BeforeRenameHook) {
    let destination = persona_artifact::normalize_persona_path(&destination)
        .expect("before-rename test hook destination must be valid UTF-8");
    let previous = BEFORE_RENAME_HOOK
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .unwrap()
        .insert(destination, hook);
    assert!(previous.is_none(), "duplicate before-rename test hook");
}
#[cfg(test)]
fn run_before_rename_hook(destination: &Path) {
    let hook = BEFORE_RENAME_HOOK
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .unwrap()
        .remove(destination);
    if let Some(hook) = hook {
        hook();
    }
}
#[cfg(not(test))]
fn run_before_rename_hook(_: &Path) {}
fn valid_record_destination(value: &str) -> bool {
    let path = Path::new(value);
    path.is_absolute()
        && !path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        && path.components().count() <= MAX_COMPONENTS
        && path.components().all(|component| match component {
            Component::Normal(part) => !part.is_empty() && part.len() <= MAX_COMPONENT_BYTES,
            Component::RootDir => true,
            _ => false,
        })
}
#[cfg(unix)]
fn same_std(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as S;
    a.dev() == b.dev() && a.ino() == b.ino()
}
#[cfg(not(unix))]
fn same_std(_: &fs::Metadata, _: &fs::Metadata) -> bool {
    false
}
#[cfg(unix)]
fn links(m: &cap_fs::Metadata) -> u64 {
    use cap_fs::MetadataExt;
    m.nlink()
}
#[cfg(not(unix))]
fn links(_: &cap_fs::Metadata) -> u64 {
    0
}
#[cfg(unix)]
fn device(m: &cap_fs::Metadata) -> u64 {
    use cap_fs::MetadataExt;
    m.dev()
}
#[cfg(not(unix))]
fn device(_: &cap_fs::Metadata) -> u64 {
    0
}
fn safe_leaf(name: &str) -> bool {
    !name.is_empty() && Path::new(name).components().count() == 1 && !name.contains('\0')
}
fn stage_name(leaf: &str) -> Result<String, PersonaMaterializeError> {
    let digest = hash_bytes(leaf.as_bytes());
    let name = format!(".kio-persona-materialize-stage-{}", &digest[7..]);
    if safe_leaf(&name) {
        Ok(name)
    } else {
        bad("unsafe staging name")
    }
}
#[cfg(unix)]
fn same_cap(a: &cap_fs::Metadata, b: &cap_fs::Metadata) -> bool {
    use cap_fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}
#[cfg(not(unix))]
fn same_cap(_: &cap_fs::Metadata, _: &cap_fs::Metadata) -> bool {
    false
}
fn bad<T>(message: impl Into<String>) -> Result<T, PersonaMaterializeError> {
    Err(PersonaMaterializeError::Unsafe(message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filesystem_device_jcs_preserves_all_bits() {
        for value in [(1_u64 << 53) + 1, 9_851_624_185_183_609, u64::MAX] {
            let mut record = PersonaMaterializationRecord {
                schema: "kio.persona.materialization/v1".into(),
                fixture_id: FIXTURE_ID.into(),
                profile: PersonaProfile::Tiny,
                replay_id: "replay-01".into(),
                destination_root: "/fixture".into(),
                filesystem_device: value,
                plan: PlanRecord {
                    digest: hash_bytes(b"fixture"),
                    sha256: hash_bytes(b"fixture"),
                    bytes: 1,
                },
                schedule: ArtifactRecord {
                    sha256: hash_bytes(b"fixture"),
                    bytes: 1,
                },
                render: ArtifactRecord {
                    sha256: hash_bytes(b"fixture"),
                    bytes: 1,
                },
                claims: Claims {
                    sources_materialized: false,
                    actual_kio_evidence: false,
                    history_ready: false,
                },
            };
            let bytes = record.canonical_bytes().unwrap();
            assert_eq!(
                serde_json::from_slice::<PersonaMaterializationRecord>(&bytes).unwrap(),
                record
            );
            #[cfg(unix)]
            assert_eq!(
                PersonaMaterializationRecord::parse_canonical(&bytes).unwrap(),
                record
            );
            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(json["filesystem_device"], format!("{value:016x}"));
            record.filesystem_device -= 1;
            assert_ne!(record.canonical_bytes().unwrap(), bytes);
            for invalid in [
                serde_json::json!(value),
                serde_json::json!("FFFFFFFFFFFFFFFF"),
                serde_json::json!("1"),
            ] {
                let mut invalid_record = json.clone();
                invalid_record["filesystem_device"] = invalid;
                let bytes = canonical_json_bytes(&invalid_record).unwrap();
                assert!(serde_json::from_slice::<PersonaMaterializationRecord>(&bytes).is_err());
            }
        }
    }

    use crate::{
        persona_plan::{PersonaProfile, frozen_plan},
        persona_render_artifact::RenderArtifact,
        persona_schedule::build_suite_schedule,
    };
    use std::{fs, sync::OnceLock};
    use tempfile::tempdir;
    #[cfg(target_os = "macos")]
    use tempfile::tempdir_in;

    static TINY_BUNDLE: OnceLock<(Vec<u8>, Vec<u8>, Vec<u8>)> = OnceLock::new();

    fn tiny_bundle() -> &'static (Vec<u8>, Vec<u8>, Vec<u8>) {
        TINY_BUNDLE.get_or_init(|| {
            let plan = frozen_plan(PersonaProfile::Tiny);
            let schedule = build_suite_schedule(&plan).unwrap();
            let render = RenderArtifact::build(&plan).unwrap();
            (
                plan.canonical_bytes().unwrap(),
                schedule.canonical_bytes().unwrap(),
                render.canonical_bytes().unwrap(),
            )
        })
    }

    fn inputs(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let (plan, schedule, render) = tiny_bundle();
        let plan_path = root.join("plan.json");
        let schedule_path = root.join("schedule.json");
        let render_path = root.join("render.json");
        fs::write(&plan_path, plan).unwrap();
        fs::write(&schedule_path, schedule).unwrap();
        fs::write(&render_path, render).unwrap();
        (plan_path, schedule_path, render_path)
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn materializes_tiny_bundle_with_one_canonical_record() {
        let root = tempdir().unwrap();
        let root_path = fs::canonicalize(root.path()).unwrap();
        let (plan, schedule, render) = inputs(&root_path);
        let destination = root_path.join("published");
        let created = materialize(MaterializeRequest {
            plan: &plan,
            schedule: &schedule,
            render: &render,
            destination: &destination,
            replay_id: "replay-01",
        })
        .unwrap();
        assert_eq!(created.destination, destination);
        assert_eq!(
            created.record.destination_root,
            destination.to_str().unwrap(),
        );
        let entries: Vec<_> = fs::read_dir(&destination)
            .unwrap()
            .map(|x| x.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 4);
        let record = fs::read(destination.join("persona-materialization.json")).unwrap();
        assert_eq!(
            PersonaMaterializationRecord::parse_canonical(&record).unwrap(),
            created.record
        );
        assert_eq!(
            fs::read(destination.join("persona-plan.json")).unwrap(),
            fs::read(plan).unwrap()
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_aliases_return_and_record_the_bound_canonical_destination() {
        let input_root = tempdir().unwrap();
        let input_root = fs::canonicalize(input_root.path()).unwrap();
        let (plan, schedule, render) = inputs(&input_root);

        // Both locations are deliberately created by this test. `/tmp` and
        // `/var/tmp` exercise the two aliases without selecting an existing
        // public output path.
        for alias_base in [Path::new("/tmp"), Path::new("/var/tmp")] {
            let output_root = tempdir_in(alias_base).unwrap();
            let alias_root = alias_base.join(output_root.path().file_name().unwrap());
            let destination = alias_root.join("published");
            let canonical_destination = PathBuf::from("/private").join(
                destination
                    .strip_prefix("/")
                    .expect("alias test destination is absolute"),
            );
            assert!(!canonical_destination.exists());

            let created = materialize(MaterializeRequest {
                plan: &plan,
                schedule: &schedule,
                render: &render,
                destination: &destination,
                replay_id: "replay-01",
            })
            .unwrap();
            let record_path = canonical_destination.join("persona-materialization.json");
            let record =
                PersonaMaterializationRecord::parse_canonical(&fs::read(record_path).unwrap())
                    .unwrap();

            assert_eq!(created.destination, canonical_destination);
            assert_eq!(
                created.record.destination_root,
                canonical_destination.to_str().unwrap()
            );
            assert_eq!(
                record.destination_root,
                canonical_destination.to_str().unwrap()
            );
            assert!(destination.is_dir());
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn occupied_destination_preserves_existing_bytes() {
        let root = tempdir().unwrap();
        let root_path = fs::canonicalize(root.path()).unwrap();
        let (plan, schedule, render) = inputs(&root_path);
        let destination = root_path.join("published");
        fs::create_dir(&destination).unwrap();
        let sentinel = destination.join("sentinel");
        fs::write(&sentinel, b"preserve").unwrap();
        let before = fs::metadata(&sentinel).unwrap();
        assert!(matches!(
            materialize(MaterializeRequest {
                plan: &plan,
                schedule: &schedule,
                render: &render,
                destination: &destination,
                replay_id: "replay-01"
            }),
            Err(PersonaMaterializeError::AlreadyExists(_))
        ));
        assert_eq!(fs::read(&sentinel).unwrap(), b"preserve");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(before.ino(), fs::metadata(&sentinel).unwrap().ino());
        }
    }

    #[test]
    fn rejects_relative_destination_and_invalid_replay_before_publication() {
        let root = tempdir().unwrap();
        let root_path = fs::canonicalize(root.path()).unwrap();
        let (plan, schedule, render) = inputs(&root_path);
        assert!(
            materialize(MaterializeRequest {
                plan: &plan,
                schedule: &schedule,
                render: &render,
                destination: Path::new("relative"),
                replay_id: "replay-01"
            })
            .is_err()
        );
        let destination = root_path.join("not-published");
        assert!(
            materialize(MaterializeRequest {
                plan: &plan,
                schedule: &schedule,
                render: &render,
                destination: &destination,
                replay_id: "replay-x"
            })
            .is_err()
        );
        assert!(!destination.exists());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn fixed_stale_stage_is_retained_and_rejected() {
        let root = tempdir().unwrap();
        let root_path = fs::canonicalize(root.path()).unwrap();
        let (plan, schedule, render) = inputs(&root_path);
        let destination = root_path.join("published");
        let stale = root_path.join(stage_name("published").unwrap());
        fs::create_dir(&stale).unwrap();
        fs::write(stale.join("evidence"), b"do not remove").unwrap();
        assert!(matches!(
            materialize(MaterializeRequest {
                plan: &plan,
                schedule: &schedule,
                render: &render,
                destination: &destination,
                replay_id: "replay-01"
            }),
            Err(PersonaMaterializeError::Unsafe(_))
        ));
        assert_eq!(fs::read(stale.join("evidence")).unwrap(), b"do not remove");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn record_rejects_non_hex_hash_and_true_claims() {
        let root = tempdir().unwrap();
        let root_path = fs::canonicalize(root.path()).unwrap();
        let (plan, schedule, render) = inputs(&root_path);
        let created = materialize(MaterializeRequest {
            plan: &plan,
            schedule: &schedule,
            render: &render,
            destination: &root_path.join("published"),
            replay_id: "replay-01",
        })
        .unwrap();
        let mut invalid_hash = created.record.clone();
        invalid_hash.plan.sha256 = format!("sha256:{}", "g".repeat(64));
        assert!(
            PersonaMaterializationRecord::parse_canonical(&invalid_hash.canonical_bytes().unwrap())
                .is_err()
        );
        let mut invalid_claim = created.record;
        invalid_claim.claims.history_ready = true;
        assert!(
            PersonaMaterializationRecord::parse_canonical(
                &invalid_claim.canonical_bytes().unwrap()
            )
            .is_err()
        );

        // This was accepted by the retired Python attestation boundary when
        // the caller supplied the matching digest.  Rust record identity is
        // established by the closed canonical schema, never by a digest of
        // arbitrary caller-selected bytes.
        assert!(PersonaMaterializationRecord::parse_canonical(b"{\"opaque\":true}\n").is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn record_rejects_darwin_alias_destination_spelling() {
        let root = tempdir().unwrap();
        let root_path = fs::canonicalize(root.path()).unwrap();
        let (plan, schedule, render) = inputs(&root_path);
        let created = materialize(MaterializeRequest {
            plan: &plan,
            schedule: &schedule,
            render: &render,
            destination: &root_path.join("published"),
            replay_id: "replay-01",
        })
        .unwrap();
        let mut aliased = created.record;
        aliased.destination_root = "/tmp/kio-persona-materialization-record".into();
        assert!(
            PersonaMaterializationRecord::parse_canonical(&aliased.canonical_bytes().unwrap())
                .is_err()
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn rejects_same_content_source_inode_replacement_before_publication() {
        let root = tempdir().unwrap();
        let root_path = fs::canonicalize(root.path()).unwrap();
        let (plan, schedule, render) = inputs(&root_path);
        let replacement = root_path.join("replacement.json");
        let original = fs::read(&schedule).unwrap();
        let schedule_for_hook = schedule.clone();
        let destination = root_path.join("published");
        install_before_rename_hook(
            destination.clone(),
            Box::new(move || {
                fs::write(&replacement, &original).unwrap();
                fs::rename(&replacement, &schedule_for_hook).unwrap();
            }),
        );
        assert!(matches!(
            materialize(MaterializeRequest {
                plan: &plan,
                schedule: &schedule,
                render: &render,
                destination: &destination,
                replay_id: "replay-01"
            }),
            Err(PersonaMaterializeError::Bundle(_))
        ));
        assert!(!destination.exists());
        assert!(root_path.join(stage_name("published").unwrap()).exists());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn rejects_staged_file_replacement_before_publication() {
        let root = tempdir().unwrap();
        let root_path = fs::canonicalize(root.path()).unwrap();
        let (plan, schedule, render) = inputs(&root_path);
        let destination = root_path.join("published");
        let staged_plan = root_path
            .join(stage_name("published").unwrap())
            .join("persona-plan.json");
        install_before_rename_hook(
            destination.clone(),
            Box::new(move || {
                let replacement = staged_plan.with_extension("replacement");
                fs::write(&replacement, b"tampered\n").unwrap();
                fs::rename(replacement, staged_plan).unwrap();
            }),
        );
        assert!(matches!(
            materialize(MaterializeRequest {
                plan: &plan,
                schedule: &schedule,
                render: &render,
                destination: &destination,
                replay_id: "replay-01"
            }),
            Err(PersonaMaterializeError::Unsafe(_))
        ));
        assert!(!destination.exists());
        assert!(root_path.join(stage_name("published").unwrap()).exists());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_named_stage_directory_replacement_before_publication() {
        let root = tempdir().unwrap();
        let root_path = fs::canonicalize(root.path()).unwrap();
        let (plan, schedule, render) = inputs(&root_path);
        let destination = root_path.join("published");
        let stage = root_path.join(stage_name("published").unwrap());
        let captured = root_path.join("captured-stage");
        let captured_for_hook = captured.clone();
        install_before_rename_hook(
            destination.clone(),
            Box::new(move || {
                fs::rename(&stage, &captured_for_hook).unwrap();
                fs::create_dir(&stage).unwrap();
            }),
        );
        assert!(matches!(
            materialize(MaterializeRequest {
                plan: &plan,
                schedule: &schedule,
                render: &render,
                destination: &destination,
                replay_id: "replay-01"
            }),
            Err(PersonaMaterializeError::Unsafe(_))
        ));
        assert!(!destination.exists());
        assert!(captured.join("persona-materialization.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn publishes_owner_only_stage_permissions_and_rejects_unprotected_parent() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempdir().unwrap();
        let root_path = fs::canonicalize(root.path()).unwrap();
        let (plan, schedule, render) = inputs(&root_path);
        let destination = root_path.join("published");
        materialize(MaterializeRequest {
            plan: &plan,
            schedule: &schedule,
            render: &render,
            destination: &destination,
            replay_id: "replay-01",
        })
        .unwrap();
        assert_eq!(
            fs::metadata(&destination).unwrap().permissions().mode() & 0o077,
            0
        );
        assert_eq!(
            fs::metadata(destination.join("persona-plan.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o077,
            0
        );

        let unsafe_parent = root_path.join("unsafe-parent");
        fs::create_dir(&unsafe_parent).unwrap();
        fs::set_permissions(&unsafe_parent, fs::Permissions::from_mode(0o777)).unwrap();
        let unsafe_destination = unsafe_parent.join("published");
        assert!(matches!(
            materialize(MaterializeRequest {
                plan: &plan,
                schedule: &schedule,
                render: &render,
                destination: &unsafe_destination,
                replay_id: "replay-01"
            }),
            Err(PersonaMaterializeError::Unsafe(_))
        ));
        assert!(!unsafe_destination.exists());
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    #[test]
    fn unsupported_platform_refuses_before_any_mutation() {
        let root = tempdir().unwrap();
        let destination = root.path().join("must-not-exist");
        let result = materialize(MaterializeRequest {
            plan: Path::new("missing-plan"),
            schedule: Path::new("missing-schedule"),
            render: Path::new("missing-render"),
            destination: &destination,
            replay_id: "replay-01",
        });
        assert!(matches!(result, Err(PersonaMaterializeError::Unsupported)));
        assert!(!destination.exists());
    }
}
