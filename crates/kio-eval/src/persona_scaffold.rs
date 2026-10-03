//! Create-only workspace topology derived solely from a canonical persona plan.

use crate::{
    boundary::sync_retained_directory,
    persona_artifact,
    persona_plan::{MAX_CANONICAL_BYTES, PersonaPlan},
    scale_fixture::rename_noreplace,
};
use cap_primitives::{ambient_authority, fs as cap_fs};
use kio_core::cas::{canonical_json_bytes, hash_bytes};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::sync::{Mutex, OnceLock};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};
use thiserror::Error;

const LAYOUT_ID: &str = "kio.persona.workspace-layout/v1";
const STAGE_PREFIX: &str = ".kio-persona-scaffold-stage-";
const MAX_COMPONENTS: usize = 64;
const MAX_COMPONENT_BYTES: usize = 255;
const MAX_OWNER_BYTES: usize = 16 * 1024;

#[derive(Debug, Error)]
pub enum PersonaScaffoldError {
    #[error("unsafe persona scaffold: {0}")]
    Unsafe(String),
    #[error("persona scaffold I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("persona scaffold plan: {0}")]
    Plan(#[from] crate::persona_plan::PersonaPlanError),
    #[error("persona scaffold artifact: {0}")]
    Artifact(#[from] crate::persona_artifact::PersonaArtifactError),
    #[error("persona scaffold root already exists: {0}")]
    AlreadyExists(PathBuf),
    #[error("persona scaffold publication is indeterminate: {0}")]
    Indeterminate(String),
    #[error("atomic directory no-replace publication is unsupported")]
    Unsupported,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Scaffold {
    pub root: PathBuf,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Owner {
    schema: String,
    layout_id: String,
    workspace_root: String,
    fixture_id: String,
    profile: crate::persona_plan::PersonaProfile,
    plan_digest: String,
    plan_sha256: String,
    plan_bytes: u64,
}
struct Parent {
    handle: fs::File,
    metadata: fs::Metadata,
    public: PathBuf,
    leaf: String,
}

/// Descriptor-bound, verified authority for an already-published workspace.
/// This deliberately stays crate-private: consumers must not be able to
/// manufacture an owner digest or bypass the scaffold topology verifier.
pub(crate) struct WorkspaceAuthority {
    pub root: fs::File,
    pub root_meta: cap_fs::Metadata,
    pub owner: fs::File,
    pub owner_meta: cap_fs::Metadata,
    pub plan_file: fs::File,
    pub plan_meta: cap_fs::Metadata,
    pub control: fs::File,
    pub control_meta: cap_fs::Metadata,
    pub plan: PersonaPlan,
    pub root_path: PathBuf,
    owner_bytes: Vec<u8>,
    plan_bytes: Vec<u8>,
}

impl WorkspaceAuthority {
    pub(crate) fn recheck(&mut self) -> Result<(), PersonaScaffoldError> {
        let rebound = bind_workspace(&self.root_path)?;
        if !same_cap(&self.root_meta, &cap_fs::Metadata::from_file(&self.root)?)
            || !same_cap(&self.owner_meta, &cap_fs::Metadata::from_file(&self.owner)?)
            || !same_cap(
                &self.plan_meta,
                &cap_fs::Metadata::from_file(&self.plan_file)?,
            )
            || !same_cap(
                &self.control_meta,
                &cap_fs::Metadata::from_file(&self.control)?,
            )
            || !same_cap(&self.root_meta, &rebound.root_meta)
            || !same_cap(&self.owner_meta, &rebound.owner_meta)
            || !same_cap(&self.plan_meta, &rebound.plan_meta)
            || !same_cap(&self.control_meta, &rebound.control_meta)
            || self.owner_bytes != rebound.owner_bytes
            || self.plan_bytes != rebound.plan_bytes
        {
            return bad("workspace authority changed during lease operation");
        }
        Ok(())
    }
    pub(crate) fn persona(&self, id: &str) -> bool {
        self.plan.personas.iter().any(|p| p.id.as_str() == id)
    }
    pub(crate) fn scope(&self, persona: &str, scope: &str) -> bool {
        self.plan
            .personas
            .iter()
            .find(|p| p.id.as_str() == persona)
            .is_some_and(|p| p.scopes.iter().any(|s| s.id == scope))
    }
}

pub(crate) fn bind_workspace(root: &Path) -> Result<WorkspaceAuthority, PersonaScaffoldError> {
    preflight()?;
    // bind_parent performs the absolute, normalized and Darwin alias checks;
    // opening the root through its retained parent prevents path re-resolution.
    let parent = bind_parent(root)?;
    let root_path = parent.public.join(&parent.leaf);
    let root = cap_fs::open_dir_nofollow(&parent.handle, Path::new(&parent.leaf))?;
    let root_meta = cap_fs::Metadata::from_file(&root)?;
    if !root_meta.is_dir() || root_meta.file_type().is_symlink() {
        return bad("workspace root is not a directory");
    }
    #[cfg(unix)]
    {
        use cap_fs::MetadataExt;
        if root_meta.mode() & 0o777 != 0o700 || root_meta.uid() != unsafe { libc::geteuid() } {
            return bad("workspace root mode or owner invalid");
        }
    }
    let plan_bytes = read_bounded_file(&root, "persona-plan.json", MAX_CANONICAL_BYTES)?;
    let plan = PersonaPlan::parse_canonical(&plan_bytes)?;
    let owner_bytes = read_bounded_file(&root, "persona-workspace-owner.json", MAX_OWNER_BYTES)?;
    let owner = parse_owner(&owner_bytes)?;
    if owner.plan_digest != plan.digest()?
        || owner.plan_sha256 != hash_bytes(&plan_bytes)
        || owner.plan_bytes != plan_bytes.len() as u64
        || owner.fixture_id != plan.fixture_id
        || owner.profile != plan.profile
        || Path::new(&owner.workspace_root) != root_path
    {
        return bad("workspace owner does not bind exact canonical plan");
    }
    let plan_file = open_regular(&root, "persona-plan.json", plan_bytes.len())?;
    let owner_file = open_regular(&root, "persona-workspace-owner.json", owner_bytes.len())?;
    let control = cap_fs::open_dir_nofollow(&root, Path::new("_control"))?;
    let control_meta = cap_fs::Metadata::from_file(&control)?;
    #[cfg(unix)]
    {
        use cap_fs::MetadataExt;
        if control_meta.mode() & 0o777 != 0o500 || control_meta.uid() != unsafe { libc::geteuid() }
        {
            return bad("workspace control mode invalid");
        }
    }
    verify_runtime_tree(&root, &plan, &plan_bytes, &owner_bytes)?;
    Ok(WorkspaceAuthority {
        root,
        root_meta,
        owner: owner_file.0,
        owner_meta: owner_file.1,
        plan_file: plan_file.0,
        plan_meta: plan_file.1,
        control,
        control_meta,
        plan,
        root_path,
        owner_bytes,
        plan_bytes,
    })
}

// Publication is exact-pristine. Runtime binding has a different boundary:
// scope leaves under `people` are production payload roots. We bind the
// plan-derived route *to* each leaf, but deliberately never enumerate its
// contents. Lease state remains the only mutable allowlist below `_control`.
fn verify_runtime_tree(
    root: &fs::File,
    plan: &PersonaPlan,
    plan_bytes: &[u8],
    owner_bytes: &[u8],
) -> Result<(), PersonaScaffoldError> {
    let expected = expected_tree(plan)?;
    let routes = runtime_routes(&expected)?;
    let payload_leaves = payload_leaves(plan)?;
    verify_runtime_directory(root, Path::new(""), &routes, &payload_leaves, plan)?;
    verify_file(root, "persona-plan.json", plan_bytes)?;
    let owner = read_exact(root, "persona-workspace-owner.json", owner_bytes)?;
    if parse_owner(&owner)?.plan_digest != plan.digest()? {
        return bad("runtime owner no longer binds plan");
    }
    Ok(())
}

/// Routing entries are plan-sized, whereas a payload leaf can contain an
/// unbounded corpus. Keep the former in memory and never add the latter to a
/// topology set.
fn runtime_routes(
    expected: &BTreeSet<PathBuf>,
) -> Result<BTreeMap<PathBuf, BTreeSet<String>>, PersonaScaffoldError> {
    let mut routes = BTreeMap::<PathBuf, BTreeSet<String>>::new();
    for path in expected {
        let parent = path
            .parent()
            .ok_or_else(|| PersonaScaffoldError::Unsafe("runtime path lacks parent".into()))?;
        let name = path
            .file_name()
            .and_then(|x| x.to_str())
            .filter(|x| safe(x))
            .ok_or_else(|| PersonaScaffoldError::Unsafe("runtime name invalid".into()))?;
        routes
            .entry(parent.to_owned())
            .or_default()
            .insert(name.into());
        routes.entry(path.clone()).or_default();
    }
    Ok(routes)
}

fn payload_leaves(plan: &PersonaPlan) -> Result<BTreeSet<PathBuf>, PersonaScaffoldError> {
    let mut leaves = BTreeSet::new();
    for person in &plan.personas {
        for scope in &person.scopes {
            let leaf = PathBuf::from(format!(
                "people/{}-{}/home",
                person.id.as_str(),
                person.role
            ))
            .join(&scope.path);
            if !leaves.insert(leaf.clone()) {
                return bad("duplicate payload leaf");
            }
        }
    }
    // A scope cannot simultaneously be an opaque payload root and a routing
    // parent for another scope. Frozen plans have distinct leaves; fail closed
    // instead of accidentally skipping a planned route.
    if leaves.iter().any(|leaf| {
        leaves
            .iter()
            .any(|other| leaf != other && other.starts_with(leaf))
    }) {
        return bad("payload leaf is also a planned routing parent");
    }
    Ok(leaves)
}

fn verify_runtime_directory(
    dir: &fs::File,
    prefix: &Path,
    routes: &BTreeMap<PathBuf, BTreeSet<String>>,
    payload_leaves: &BTreeSet<PathBuf>,
    plan: &PersonaPlan,
) -> Result<(), PersonaScaffoldError> {
    let expected = routes
        .get(prefix)
        .ok_or_else(|| PersonaScaffoldError::Unsafe("runtime route missing".into()))?;
    let mut seen = BTreeSet::new();
    for entry in cap_fs::read_dir(dir, Path::new("."))? {
        let name = entry?
            .file_name()
            .to_str()
            .filter(|x| safe(x))
            .ok_or_else(|| PersonaScaffoldError::Unsafe("unsafe runtime entry".into()))?
            .to_owned();
        if expected.contains(&name) {
            seen.insert(name);
            continue;
        }
        if is_lease_file(prefix, &name, plan) {
            verify_runtime_lease_file(dir, &name)?;
            continue;
        }
        return bad("workspace runtime topology contains unknown entry");
    }
    if seen.len() != expected.len() {
        return bad("workspace runtime topology is incomplete");
    }
    for name in expected {
        let path = prefix.join(name);
        let meta = cap_fs::stat(dir, Path::new(name), cap_fs::FollowSymlinks::No)?;
        if meta.file_type().is_symlink() {
            return bad("workspace runtime routing has symlink");
        }
        if !runtime_authority_file(&path) {
            let child = cap_fs::open_dir_nofollow(dir, Path::new(name))?;
            let opened = cap_fs::Metadata::from_file(&child)?;
            if !opened.is_dir() || opened.file_type().is_symlink() || !same_cap(&meta, &opened) {
                return bad("workspace runtime directory changed while opening");
            }
            #[cfg(unix)]
            {
                use cap_fs::MetadataExt;
                if opened.mode() & 0o777 != expected_directory_mode(&path)
                    || opened.uid() != unsafe { libc::geteuid() }
                {
                    return bad("workspace runtime directory mode is not private");
                }
            }
            if !payload_leaves.contains(&path) {
                verify_runtime_directory(&child, &path, routes, payload_leaves, plan)?;
            }
        } else if !meta.file_type().is_file() {
            return bad("workspace runtime has special entry");
        }
    }
    Ok(())
}

fn runtime_authority_file(path: &Path) -> bool {
    matches!(
        path.to_str(),
        Some("persona-plan.json" | "persona-workspace-owner.json")
    )
}

fn is_lease_file(prefix: &Path, name: &str, plan: &PersonaPlan) -> bool {
    if !matches!(name, ".lease.lock" | "lease.json" | "lease-recovery.jsonl") {
        return false;
    }
    let parts: Vec<_> = prefix
        .components()
        .map(|c| c.as_os_str().to_str())
        .collect();
    match parts.as_slice() {
        [Some("_control"), Some("personas"), Some(persona)] => {
            plan.personas.iter().any(|p| p.id.as_str() == *persona)
        }
        [Some("_control"), Some("scopes"), Some(persona), Some(scope)] => plan
            .personas
            .iter()
            .find(|p| p.id.as_str() == *persona)
            .is_some_and(|p| p.scopes.iter().any(|s| s.id == *scope)),
        _ => false,
    }
}

fn verify_runtime_lease_file(dir: &fs::File, name: &str) -> Result<(), PersonaScaffoldError> {
    let _meta = cap_fs::stat(dir, Path::new(name), cap_fs::FollowSymlinks::No)?;
    #[cfg(unix)]
    {
        use cap_fs::MetadataExt;
        let max = match name {
            ".lease.lock" => 0,
            "lease.json" => 16 * 1024,
            "lease-recovery.jsonl" => 64 * 1024,
            _ => unreachable!("closed by is_lease_file"),
        };
        if !_meta.file_type().is_file()
            || _meta.file_type().is_symlink()
            || _meta.nlink() != 1
            || _meta.mode() & 0o777 != 0o600
            || _meta.uid() != unsafe { libc::geteuid() }
            || _meta.len() as usize > max
        {
            return bad("runtime file is not private single-link regular");
        }
    }
    Ok(())
}

fn read_bounded_file(
    parent: &fs::File,
    name: &str,
    maximum: usize,
) -> Result<Vec<u8>, PersonaScaffoldError> {
    let before = cap_fs::stat(parent, Path::new(name), cap_fs::FollowSymlinks::No)?;
    if !before.file_type().is_file()
        || before.file_type().is_symlink()
        || links(&before) != 1
        || before.len() as usize > maximum
    {
        return bad("workspace authority file invalid");
    }
    let mut o = cap_fs::OpenOptions::new();
    o.read(true)._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let mut f = cap_fs::open(parent, Path::new(name), &o)?;
    let opened = cap_fs::Metadata::from_file(&f)?;
    if !same_cap(&before, &opened) {
        return bad("workspace authority file changed while opening");
    }
    let mut bytes = Vec::with_capacity(before.len() as usize);
    (&mut f).take(maximum as u64 + 1).read_to_end(&mut bytes)?;
    let after = cap_fs::stat(parent, Path::new(name), cap_fs::FollowSymlinks::No)?;
    if bytes.len() > maximum || !same_cap(&opened, &after) {
        return bad("workspace authority file changed while reading");
    }
    Ok(bytes)
}
fn open_regular(
    parent: &fs::File,
    name: &str,
    len: usize,
) -> Result<(fs::File, cap_fs::Metadata), PersonaScaffoldError> {
    let before = cap_fs::stat(parent, Path::new(name), cap_fs::FollowSymlinks::No)?;
    if !regular(&before, len) {
        return bad("workspace authority file invalid");
    }
    let mut o = cap_fs::OpenOptions::new();
    o.read(true)._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let f = cap_fs::open(parent, Path::new(name), &o)?;
    let opened = cap_fs::Metadata::from_file(&f)?;
    if !regular(&opened, len) || !same_cap(&before, &opened) {
        return bad("workspace authority file changed while opening");
    }
    Ok((f, opened))
}

pub fn scaffold(plan_path: &Path, root: &Path) -> Result<Scaffold, PersonaScaffoldError> {
    preflight()?;
    let plan_source = persona_artifact::bind_strict(plan_path, MAX_CANONICAL_BYTES)?;
    let plan = PersonaPlan::parse_canonical(plan_source.bytes())?;
    let parent = bind_parent(root)?;
    let canonical_root = parent.public.join(&parent.leaf);
    absent(&parent, &parent.leaf)?;
    let (stage_name, stage) = create_stage(&parent, &canonical_root)?;
    let stage_cap_meta = cap_fs::Metadata::from_file(&stage)?;
    let stage_meta = stage.metadata()?;
    if !stage_cap_meta.is_dir() || stage_cap_meta.file_type().is_symlink() {
        return bad("stage is not a real directory");
    }
    #[cfg(unix)]
    {
        use cap_fs::MetadataExt;
        if stage_cap_meta.mode() & 0o777 != 0o700 {
            return bad("stage mode is not private");
        }
    }
    (|| {
        let digest = plan.digest()?;
        let owner = Owner {
            schema: "kio.persona.workspace-owner/v1".into(),
            layout_id: LAYOUT_ID.into(),
            workspace_root: canonical_root
                .to_str()
                .ok_or_else(|| PersonaScaffoldError::Unsafe("workspace root is not UTF-8".into()))?
                .to_owned(),
            fixture_id: plan.fixture_id.clone(),
            profile: plan.profile,
            plan_digest: digest,
            plan_sha256: hash_bytes(plan_source.bytes()),
            plan_bytes: plan_source.bytes().len() as u64,
        };
        let mut owner_bytes = canonical_json_bytes(
            &serde_json::to_value(&owner)
                .map_err(|e| PersonaScaffoldError::Unsafe(e.to_string()))?,
        )
        .map_err(|e| PersonaScaffoldError::Unsafe(e.to_string()))?;
        owner_bytes.push(b'\n');
        write(&stage, "persona-plan.json", plan_source.bytes())?;
        write(&stage, "persona-workspace-owner.json", &owner_bytes)?;
        verify_file(&stage, "persona-plan.json", plan_source.bytes())?;
        let owner_readback = read_exact(&stage, "persona-workspace-owner.json", &owner_bytes)?;
        let parsed_owner = parse_owner(&owner_readback)?;
        if parsed_owner != owner {
            return bad("owner readback differs");
        }
        mkdir(&stage, "_control")?;
        let control = cap_fs::open_dir_nofollow(&stage, Path::new("_control"))?;
        mkdir(&control, "personas")?;
        mkdir(&control, "scopes")?;
        let people = mkdir(&stage, "people")?;
        for person in &plan.personas {
            let id = person.id.as_str().to_owned();
            let control_people = cap_fs::open_dir_nofollow(&control, Path::new("personas"))?;
            mkdir(&control_people, &id)?;
            let control_scopes = cap_fs::open_dir_nofollow(&control, Path::new("scopes"))?;
            let per_scopes = mkdir(&control_scopes, &id)?;
            let home_person = mkdir(&people, &format!("{id}-{}", person.role))?;
            let home = mkdir(&home_person, "home")?;
            for scope in &person.scopes {
                mkdir(&per_scopes, &scope.id)?;
                let mut current = home.try_clone()?;
                for component in scope.path.split('/') {
                    if !safe(component) {
                        return bad("plan scope path is unsafe");
                    }
                    current = ensure_dir(&current, component)?;
                }
            }
        }
        // These are topology-owned routing parents, not lease-state leaves.
        // Keep the direct persona/scope leaves private and writable for the
        // opaque lease protocol, but make all routing levels immutable.
        set_mode(&control, 0o500)?;
        let control_people = cap_fs::open_dir_nofollow(&control, Path::new("personas"))?;
        set_mode(&control_people, 0o500)?;
        let control_scopes = cap_fs::open_dir_nofollow(&control, Path::new("scopes"))?;
        set_mode(&control_scopes, 0o500)?;
        for person in &plan.personas {
            let scopes = cap_fs::open_dir_nofollow(&control_scopes, Path::new(person.id.as_str()))?;
            set_mode(&scopes, 0o500)?;
        }
        verify_tree(&stage, &plan, plan_source.bytes(), &owner_bytes)?;
        sync_tree(&stage, &parent.public.join(&stage_name))?;
        sync_retained_directory(&stage, &stage_meta, &parent.public.join(&stage_name))
            .map_err(|e| PersonaScaffoldError::Unsafe(e.to_string()))?;
        plan_source.recheck()?;
        run_before_rename_hook(&canonical_root);
        plan_source.recheck()?;
        verify_tree(&stage, &plan, plan_source.bytes(), &owner_bytes)?;
        recheck_parent(&parent)?;
        recheck_named_directory(&parent, &stage_name, &stage_cap_meta)?;
        rename_noreplace(&parent.handle, &stage_name, &parent.handle, &parent.leaf)
            .map_err(|e| PersonaScaffoldError::Indeterminate(e.to_string()))?;
        sync_retained_directory(&parent.handle, &parent.metadata, &parent.public)
            .map_err(|e| PersonaScaffoldError::Indeterminate(e.to_string()))?;
        verify_published(
            &parent,
            &stage_cap_meta,
            &plan,
            plan_source.bytes(),
            &owner_bytes,
        )
        .map_err(|e| PersonaScaffoldError::Indeterminate(e.to_string()))?;
        Ok(Scaffold {
            root: canonical_root,
        })
    })()
}
fn create_stage(parent: &Parent, root: &Path) -> Result<(String, fs::File), PersonaScaffoldError> {
    for _ in 0..32 {
        let stage_name = format!("{STAGE_PREFIX}{}", stage_token(root)?);
        let options = cap_fs::DirOptions::new();
        #[cfg(unix)]
        let options = {
            use cap_fs::DirBuilderExt;
            let mut options = options;
            options.mode(0o700);
            options
        };
        match cap_fs::create_dir(&parent.handle, Path::new(&stage_name), &options) {
            Ok(()) => {
                return Ok((
                    stage_name.clone(),
                    cap_fs::open_dir_nofollow(&parent.handle, Path::new(&stage_name))?,
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    bad("exhausted private scaffold staging candidates")
}
#[cfg(test)]
static STAGE_TOKENS: OnceLock<Mutex<BTreeMap<PathBuf, Vec<String>>>> = OnceLock::new();
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
fn install_stage_tokens(root: PathBuf, tokens: Vec<String>) {
    let old = STAGE_TOKENS
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .unwrap()
        .insert(root, tokens);
    assert!(old.is_none());
}
fn stage_token(root: &Path) -> Result<String, PersonaScaffoldError> {
    #[cfg(test)]
    if let Some(token) = STAGE_TOKENS
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .unwrap()
        .get_mut(root)
        .and_then(|tokens| {
            if tokens.is_empty() {
                None
            } else {
                Some(tokens.remove(0))
            }
        })
    {
        if safe(&token) {
            return Ok(token);
        }
        return bad("test staging token is invalid");
    }
    let _ = root;
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|error| {
        PersonaScaffoldError::Unsafe(format!("secure stage randomness unavailable: {error}"))
    })?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}
fn write(parent: &fs::File, name: &str, bytes: &[u8]) -> Result<(), PersonaScaffoldError> {
    let mut o = cap_fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use cap_fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let mut f = cap_fs::open(parent, Path::new(name), &o)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    let metadata = cap_fs::Metadata::from_file(&f)?;
    if !regular(&metadata, bytes.len()) {
        return bad("staged file is not a single-link regular file");
    }
    Ok(())
}
fn read_exact(
    parent: &fs::File,
    name: &str,
    expected: &[u8],
) -> Result<Vec<u8>, PersonaScaffoldError> {
    use std::io::Read;
    let before = cap_fs::stat(parent, Path::new(name), cap_fs::FollowSymlinks::No)?;
    if !regular(&before, expected.len()) {
        return bad("owner is not regular exact file");
    }
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let mut file = cap_fs::open(parent, Path::new(name), &options)?;
    let opened = cap_fs::Metadata::from_file(&file)?;
    if !regular(&opened, expected.len()) || !same_cap(&before, &opened) {
        return bad("owner is not regular exact file");
    }
    let mut bytes = Vec::with_capacity(expected.len());
    (&mut file)
        .take(expected.len() as u64 + 1)
        .read_to_end(&mut bytes)?;
    let after = cap_fs::stat(parent, Path::new(name), cap_fs::FollowSymlinks::No)?;
    if bytes != expected
        || !regular(&after, expected.len())
        || !same_cap(&before, &opened)
        || !same_cap(&opened, &after)
    {
        return bad("owner changed while readback");
    }
    Ok(bytes)
}
#[cfg(unix)]
fn same_cap(a: &cap_fs::Metadata, b: &cap_fs::Metadata) -> bool {
    use cap_fs::MetadataExt;
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.len() == b.len()
        && a.nlink() == b.nlink()
        && a.mode() == b.mode()
        && a.uid() == b.uid()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}
#[cfg(not(unix))]
fn same_cap(_: &cap_fs::Metadata, _: &cap_fs::Metadata) -> bool {
    false
}
fn mkdir(parent: &fs::File, name: &str) -> Result<fs::File, PersonaScaffoldError> {
    if !safe(name) {
        return bad("unsafe plan-derived directory name");
    }
    let options = cap_fs::DirOptions::new();
    #[cfg(unix)]
    let options = {
        use cap_fs::DirBuilderExt;
        let mut options = options;
        options.mode(0o700);
        options
    };
    cap_fs::create_dir(parent, Path::new(name), &options)?;
    Ok(cap_fs::open_dir_nofollow(parent, Path::new(name))?)
}
fn set_mode(directory: &fs::File, mode: u32) -> Result<(), PersonaScaffoldError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let expected = cap_fs::Metadata::from_file(directory)?;
        let mut options = cap_fs::OpenOptions::new();
        options
            .read(true)
            ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
        let writable = cap_fs::open(directory, Path::new("."), &options)?;
        writable.set_permissions(fs::Permissions::from_mode(mode))?;
        let metadata = cap_fs::Metadata::from_file(&writable)?;
        use cap_fs::MetadataExt;
        // chmod intentionally changes the full metadata snapshot; retain the
        // object identity check here rather than treating that expected change
        // as a substitution.
        if expected.dev() != metadata.dev()
            || expected.ino() != metadata.ino()
            || metadata.mode() & 0o777 != mode
        {
            return bad("directory mode changed unexpectedly");
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (directory, mode);
        Err(PersonaScaffoldError::Unsupported)
    }
}
fn ensure_dir(parent: &fs::File, name: &str) -> Result<fs::File, PersonaScaffoldError> {
    if !safe(name) {
        return bad("unsafe plan-derived directory name");
    }
    match cap_fs::open_dir_nofollow(parent, Path::new(name)) {
        Ok(dir) => Ok(dir),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => mkdir(parent, name),
        Err(error) => Err(error.into()),
    }
}
fn parse_owner(bytes: &[u8]) -> Result<Owner, PersonaScaffoldError> {
    if bytes.len() > MAX_OWNER_BYTES || !bytes.ends_with(b"\n") {
        return bad("owner size or terminator invalid");
    }
    let owner: Owner =
        serde_json::from_slice(bytes).map_err(|e| PersonaScaffoldError::Unsafe(e.to_string()))?;
    if owner.schema != "kio.persona.workspace-owner/v1"
        || owner.layout_id != LAYOUT_ID
        || !Path::new(&owner.workspace_root).is_absolute()
        || persona_artifact::normalize_persona_path(Path::new(&owner.workspace_root))?.as_path()
            != Path::new(&owner.workspace_root)
        || owner.fixture_id != crate::persona_plan::FIXTURE_ID
        || !valid_hash(&owner.plan_digest)
        || owner.plan_digest != owner.plan_sha256
        || owner.plan_bytes == 0
        || owner.plan_bytes > MAX_CANONICAL_BYTES as u64
    {
        return bad("owner closed semantics invalid");
    }
    let mut canonical = canonical_json_bytes(
        &serde_json::to_value(&owner).map_err(|e| PersonaScaffoldError::Unsafe(e.to_string()))?,
    )
    .map_err(|e| PersonaScaffoldError::Unsafe(e.to_string()))?;
    canonical.push(b'\n');
    if canonical != bytes {
        return bad("owner is not canonical JCS+LF");
    }
    Ok(owner)
}
fn verify_file(parent: &fs::File, name: &str, expected: &[u8]) -> Result<(), PersonaScaffoldError> {
    let before = cap_fs::stat(parent, Path::new(name), cap_fs::FollowSymlinks::No)?;
    if !regular(&before, expected.len()) {
        return bad("tree file is not bounded single-link regular");
    }
    #[cfg(unix)]
    {
        use cap_fs::MetadataExt;
        if before.mode() & 0o777 != 0o600 || before.uid() != unsafe { libc::geteuid() } {
            return bad("tree file mode is not private");
        }
    }
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let mut file = cap_fs::open(parent, Path::new(name), &options)?;
    let opened = cap_fs::Metadata::from_file(&file)?;
    if !regular(&opened, expected.len()) || !same_cap(&before, &opened) {
        return bad("tree file changed while opening");
    }
    let mut actual = Vec::with_capacity(expected.len());
    (&mut file)
        .take(expected.len() as u64 + 1)
        .read_to_end(&mut actual)?;
    let after_open = cap_fs::Metadata::from_file(&file)?;
    let after = cap_fs::stat(parent, Path::new(name), cap_fs::FollowSymlinks::No)?;
    if !regular(&after_open, expected.len())
        || !regular(&after, expected.len())
        || !same_cap(&before, &after_open)
        || !same_cap(&after_open, &after)
        || actual != expected
        || hash_bytes(&actual) != hash_bytes(expected)
    {
        return bad("tree file changed while reading");
    }
    Ok(())
}
fn expected_tree(plan: &PersonaPlan) -> Result<BTreeSet<PathBuf>, PersonaScaffoldError> {
    let mut expected = BTreeSet::from([
        PathBuf::from("persona-plan.json"),
        PathBuf::from("persona-workspace-owner.json"),
        PathBuf::from("_control"),
        PathBuf::from("_control/personas"),
        PathBuf::from("_control/scopes"),
        PathBuf::from("people"),
    ]);
    for person in &plan.personas {
        let id = person.id.as_str();
        expected.insert(PathBuf::from(format!("_control/personas/{id}")));
        expected.insert(PathBuf::from(format!("_control/scopes/{id}")));
        expected.insert(PathBuf::from(format!("people/{id}-{}", person.role)));
        expected.insert(PathBuf::from(format!("people/{id}-{}/home", person.role)));
        for scope in &person.scopes {
            expected.insert(PathBuf::from(format!("_control/scopes/{id}/{}", scope.id)));
            let mut p = PathBuf::from(format!("people/{id}-{}/home", person.role));
            for component in scope.path.split('/') {
                if !safe(component) {
                    return bad("plan topology component invalid");
                }
                p.push(component);
                expected.insert(p.clone());
            }
        }
    }
    if expected.len() > 8192 {
        return bad("plan topology exceeds entry bound");
    }
    Ok(expected)
}
fn collect_tree(
    dir: &fs::File,
    prefix: &Path,
    output: &mut BTreeSet<PathBuf>,
) -> Result<(), PersonaScaffoldError> {
    for entry in cap_fs::read_dir(dir, Path::new("."))? {
        let entry = entry?;
        let name = entry
            .file_name()
            .to_str()
            .ok_or_else(|| PersonaScaffoldError::Unsafe("directory entry not UTF-8".into()))?
            .to_owned();
        if !safe(&name) {
            return bad("unsafe tree entry");
        }
        let path = prefix.join(&name);
        if !output.insert(path.clone()) || output.len() > 8192 {
            return bad("tree entry set exceeds bounds");
        }
        let metadata = cap_fs::stat(dir, Path::new(&name), cap_fs::FollowSymlinks::No)?;
        if metadata.file_type().is_symlink() {
            return bad("tree has symlink");
        }
        if metadata.is_dir() {
            let child = cap_fs::open_dir_nofollow(dir, Path::new(&name))?;
            let opened = cap_fs::Metadata::from_file(&child)?;
            if !opened.is_dir() || opened.file_type().is_symlink() || !same_cap(&metadata, &opened)
            {
                return bad("tree directory changed while opening");
            }
            #[cfg(unix)]
            {
                use cap_fs::MetadataExt;
                if opened.mode() & 0o777 != expected_directory_mode(&path)
                    || opened.uid() != unsafe { libc::geteuid() }
                {
                    return bad("tree directory mode is not private");
                }
            }
            collect_tree(&child, &path, output)?;
        } else if !metadata.file_type().is_file() {
            return bad("tree has special entry");
        }
    }
    Ok(())
}
#[cfg(unix)]
fn expected_directory_mode(path: &Path) -> u32 {
    let components: Vec<_> = path.components().collect();
    if path == Path::new("_control")
        || path == Path::new("_control/personas")
        || path == Path::new("_control/scopes")
        || (components.len() == 3
            && components[0].as_os_str() == "_control"
            && components[1].as_os_str() == "scopes")
    {
        0o500
    } else {
        0o700
    }
}
fn sync_tree(dir: &fs::File, public: &Path) -> Result<(), PersonaScaffoldError> {
    let expected = dir.metadata()?;
    for entry in cap_fs::read_dir(dir, Path::new("."))? {
        let name = entry?
            .file_name()
            .to_str()
            .ok_or_else(|| PersonaScaffoldError::Unsafe("directory entry not UTF-8".into()))?
            .to_owned();
        let named = cap_fs::stat(dir, Path::new(&name), cap_fs::FollowSymlinks::No)?;
        if named.is_dir() {
            let child = cap_fs::open_dir_nofollow(dir, Path::new(&name))?;
            let opened = cap_fs::Metadata::from_file(&child)?;
            if !opened.is_dir() || opened.file_type().is_symlink() || !same_cap(&named, &opened) {
                return bad("directory changed while syncing");
            }
            sync_tree(&child, &public.join(&name))?;
            let after = cap_fs::stat(dir, Path::new(&name), cap_fs::FollowSymlinks::No)?;
            if !same_cap(&named, &after) {
                return bad("directory changed after syncing");
            }
        }
    }
    sync_retained_directory(dir, &expected, public)
        .map_err(|e| PersonaScaffoldError::Unsafe(e.to_string()))
}
fn verify_tree(
    stage: &fs::File,
    plan: &PersonaPlan,
    plan_bytes: &[u8],
    owner_bytes: &[u8],
) -> Result<(), PersonaScaffoldError> {
    let expected = expected_tree(plan)?;
    let mut actual = BTreeSet::new();
    collect_tree(stage, Path::new(""), &mut actual)?;
    if actual != expected {
        return bad("scaffold topology allowlist differs");
    }
    verify_file(stage, "persona-plan.json", plan_bytes)?;
    let owner = read_exact(stage, "persona-workspace-owner.json", owner_bytes)?;
    let parsed = parse_owner(&owner)?;
    if parsed.plan_digest != plan.digest()?
        || parsed.plan_sha256 != hash_bytes(plan_bytes)
        || parsed.plan_bytes != plan_bytes.len() as u64
        || parsed.profile != plan.profile
        || parsed.fixture_id != plan.fixture_id
    {
        return bad("owner does not bind exact plan");
    }
    Ok(())
}
fn verify_published(
    parent: &Parent,
    staged: &cap_fs::Metadata,
    plan: &PersonaPlan,
    plan_bytes: &[u8],
    owner_bytes: &[u8],
) -> Result<(), PersonaScaffoldError> {
    recheck_parent(parent)?;
    let root = cap_fs::open_dir_nofollow(&parent.handle, Path::new(&parent.leaf))?;
    let metadata = cap_fs::Metadata::from_file(&root)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || !same_directory_cap(staged, &metadata)
    {
        return bad("published root identity invalid");
    }
    recheck_named_directory(parent, &parent.leaf, staged)?;
    verify_tree(&root, plan, plan_bytes, owner_bytes)?;
    recheck_named_directory(parent, &parent.leaf, staged)?;
    verify_tree(&root, plan, plan_bytes, owner_bytes)?;
    recheck_named_directory(parent, &parent.leaf, staged)?;
    recheck_parent(parent)
}
fn absent(parent: &Parent, name: &str) -> Result<(), PersonaScaffoldError> {
    match cap_fs::stat(&parent.handle, Path::new(name), cap_fs::FollowSymlinks::No) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(PersonaScaffoldError::AlreadyExists(
            parent.public.join(name),
        )),
        Err(e) => Err(e.into()),
    }
}
fn bind_parent(root: &Path) -> Result<Parent, PersonaScaffoldError> {
    if !root.is_absolute()
        || root
            .components()
            .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
        || root.components().count() > MAX_COMPONENTS
    {
        return bad("root must be absolute UTF-8 and normalized");
    }
    let root = persona_artifact::normalize_persona_path(root)?;
    if root
        .components()
        .any(|c| matches!(c, Component::Normal(p) if p.len() > MAX_COMPONENT_BYTES))
    {
        return bad("root component exceeds bound");
    }
    let leaf = root
        .file_name()
        .and_then(|x| x.to_str())
        .filter(|x| safe(x))
        .ok_or_else(|| PersonaScaffoldError::Unsafe("unsafe root leaf".into()))?
        .to_owned();
    let public = root
        .parent()
        .ok_or_else(|| PersonaScaffoldError::Unsafe("root lacks parent".into()))?
        .to_owned();
    let mut handle = cap_fs::open_ambient_dir(Path::new("/"), ambient_authority())?;
    for c in public.components().skip(1) {
        let Component::Normal(p) = c else {
            return bad("root not normalized");
        };
        if p.len() > MAX_COMPONENT_BYTES {
            return bad("root component exceeds bound");
        }
        handle = cap_fs::open_dir_nofollow(&handle, Path::new(p))?;
    }
    let metadata = handle.metadata()?;
    if !metadata.is_dir() {
        return bad("root parent is not directory");
    }
    validate_parent_permissions(&metadata)?;
    Ok(Parent {
        handle,
        metadata,
        public,
        leaf,
    })
}
fn recheck_parent(parent: &Parent) -> Result<(), PersonaScaffoldError> {
    let rebound = bind_parent(&parent.public.join(&parent.leaf))?;
    let retained = cap_fs::Metadata::from_file(&parent.handle)?;
    if !retained.is_dir()
        || retained.file_type().is_symlink()
        || !same(&parent.metadata, &parent.handle.metadata()?)
        || !same(&parent.metadata, &rebound.handle.metadata()?)
    {
        return bad("root parent changed");
    }
    Ok(())
}
fn recheck_named_directory(
    parent: &Parent,
    name: &str,
    expected: &cap_fs::Metadata,
) -> Result<(), PersonaScaffoldError> {
    let named = cap_fs::stat(&parent.handle, Path::new(name), cap_fs::FollowSymlinks::No)?;
    if !named.is_dir() || named.file_type().is_symlink() || !same_directory_cap(expected, &named) {
        return bad("named directory identity changed");
    }
    Ok(())
}
#[cfg(unix)]
fn same_directory_cap(a: &cap_fs::Metadata, b: &cap_fs::Metadata) -> bool {
    use cap_fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}
#[cfg(windows)]
fn same_directory_cap(a: &cap_fs::Metadata, b: &cap_fs::Metadata) -> bool {
    use cap_fs::_WindowsByHandle;
    matches!(
        (
            a.volume_serial_number(),
            a.file_index(),
            b.volume_serial_number(),
            b.file_index(),
        ),
        (Some(a_volume), Some(a_index), Some(b_volume), Some(b_index))
            if a_volume == b_volume && a_index == b_index
    )
}
#[cfg(not(any(unix, windows)))]
fn same_directory_cap(_: &cap_fs::Metadata, _: &cap_fs::Metadata) -> bool {
    false
}
#[cfg(unix)]
fn validate_parent_permissions(metadata: &fs::Metadata) -> Result<(), PersonaScaffoldError> {
    use std::os::unix::fs::MetadataExt;
    let mode = metadata.mode();
    let euid = unsafe { libc::geteuid() };
    let owner_trusted = metadata.uid() == euid || metadata.uid() == 0;
    if (metadata.uid() == euid && mode & 0o022 == 0) || (owner_trusted && mode & 0o1000 != 0) {
        Ok(())
    } else {
        bad("root parent permits unprotected entry replacement")
    }
}
#[cfg(not(unix))]
fn validate_parent_permissions(_: &fs::Metadata) -> Result<(), PersonaScaffoldError> {
    Err(PersonaScaffoldError::Unsupported)
}
#[cfg(unix)]
fn same(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}
#[cfg(unix)]
fn links(metadata: &cap_fs::Metadata) -> u64 {
    use cap_fs::MetadataExt;
    metadata.nlink()
}
#[cfg(not(unix))]
fn links(_: &cap_fs::Metadata) -> u64 {
    0
}
fn regular(metadata: &cap_fs::Metadata, bytes: usize) -> bool {
    metadata.file_type().is_file()
        && !metadata.file_type().is_symlink()
        && metadata.len() == bytes as u64
        && links(metadata) == 1
        && {
            #[cfg(unix)]
            {
                use cap_fs::MetadataExt;
                metadata.mode() & 0o777 == 0o600 && metadata.uid() == unsafe { libc::geteuid() }
            }
            #[cfg(not(unix))]
            {
                false
            }
        }
}
fn valid_hash(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}
#[cfg(not(unix))]
fn same(_: &fs::Metadata, _: &fs::Metadata) -> bool {
    false
}
fn safe(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_COMPONENT_BYTES
        && matches!(Path::new(s).components().next(), Some(Component::Normal(_)))
        && Path::new(s).components().nth(1).is_none()
        && !s.contains('\0')
}
fn preflight() -> Result<(), PersonaScaffoldError> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(PersonaScaffoldError::Unsupported)
    }
}
#[cfg(test)]
type BeforeRenameHook = Box<dyn FnOnce() + Send>;
#[cfg(test)]
static BEFORE_RENAME_HOOK: OnceLock<Mutex<BTreeMap<PathBuf, BeforeRenameHook>>> = OnceLock::new();
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
fn install_before_rename_hook(root: PathBuf, hook: BeforeRenameHook) {
    let old = BEFORE_RENAME_HOOK
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .unwrap()
        .insert(root, hook);
    assert!(old.is_none());
}
#[cfg(test)]
fn run_before_rename_hook(root: &Path) {
    if let Some(hook) = BEFORE_RENAME_HOOK
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .unwrap()
        .remove(root)
    {
        hook();
    }
}
#[cfg(not(test))]
fn run_before_rename_hook(_: &Path) {}
fn bad<T>(s: impl Into<String>) -> Result<T, PersonaScaffoldError> {
    Err(PersonaScaffoldError::Unsafe(s.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use crate::persona_plan::{PersonaProfile, frozen_plan};
    use tempfile::tempdir;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn plan_file(root: &Path, profile: PersonaProfile) -> PathBuf {
        let path = root.join("plan.json");
        fs::write(&path, frozen_plan(profile).canonical_bytes().unwrap()).unwrap();
        path
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn payload_leaf(root: &Path, plan: &PersonaPlan) -> PathBuf {
        let person = plan
            .personas
            .iter()
            .find(|p| p.id.as_str() == "p01")
            .unwrap();
        let scope = person.scopes.first().unwrap();
        root.join(format!(
            "people/{}-{}/home",
            person.id.as_str(),
            person.role
        ))
        .join(&scope.path)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn creates_exact_topology_and_owner_for_tiny_and_pilot() {
        for profile in [PersonaProfile::Tiny, PersonaProfile::Pilot] {
            let temp = tempdir().unwrap();
            let parent = fs::canonicalize(temp.path()).unwrap();
            let plan_path = plan_file(&parent, profile);
            let destination = parent.join(format!("workspace-{profile:?}"));
            scaffold(&plan_path, &destination).unwrap();
            let plan = PersonaPlan::parse_canonical(&fs::read(&plan_path).unwrap()).unwrap();
            let expected = expected_tree(&plan).unwrap();
            let mut actual = BTreeSet::new();
            let root = cap_fs::open_ambient_dir(&destination, ambient_authority()).unwrap();
            collect_tree(&root, Path::new(""), &mut actual).unwrap();
            assert_eq!(actual, expected);
            let owner = fs::read(destination.join("persona-workspace-owner.json")).unwrap();
            assert_eq!(
                parse_owner(&owner).unwrap().plan_digest,
                plan.digest().unwrap()
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_aliases_return_publish_and_bind_one_canonical_workspace_identity() {
        use tempfile::tempdir_in;

        let input_root = tempdir().unwrap();
        let input_root = fs::canonicalize(input_root.path()).unwrap();
        let plan = plan_file(&input_root, PersonaProfile::Tiny);

        for alias_base in [Path::new("/tmp"), Path::new("/var/tmp")] {
            let output_root = tempdir_in(alias_base).unwrap();
            let alias_root = alias_base.join(output_root.path().file_name().unwrap());
            let requested = alias_root.join("workspace");
            let canonical = PathBuf::from("/private").join(
                requested
                    .strip_prefix("/")
                    .expect("Darwin alias test path is absolute"),
            );

            let created = scaffold(&plan, &requested).unwrap();
            assert_eq!(created.root, canonical);
            assert!(requested.is_dir());

            let owner_bytes = fs::read(canonical.join("persona-workspace-owner.json")).unwrap();
            let owner = parse_owner(&owner_bytes).unwrap();
            assert_eq!(owner.workspace_root, canonical.to_str().unwrap());

            let rebound = bind_workspace(&canonical).unwrap();
            assert_eq!(rebound.root_path, canonical);
            let output = serde_json::to_value(&created).unwrap();
            assert_eq!(output["root"], canonical.to_str().unwrap());

            assert!(matches!(
                scaffold(&plan, &canonical),
                Err(PersonaScaffoldError::AlreadyExists(path)) if path == canonical
            ));
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn existing_destination_is_preserved_and_attacker_stage_does_not_block() {
        let temp = tempdir().unwrap();
        let parent = fs::canonicalize(temp.path()).unwrap();
        let plan = plan_file(&parent, PersonaProfile::Tiny);
        let destination = parent.join("workspace");
        fs::write(&destination, b"do not modify").unwrap();
        assert!(matches!(
            scaffold(&plan, &destination),
            Err(PersonaScaffoldError::AlreadyExists(_))
        ));
        assert_eq!(fs::read(&destination).unwrap(), b"do not modify");
        fs::remove_file(&destination).unwrap();
        let stage = parent.join(format!("{STAGE_PREFIX}predictable"));
        fs::create_dir(&stage).unwrap();
        fs::write(stage.join("attacker-marker"), b"untouched").unwrap();
        install_stage_tokens(
            destination.clone(),
            vec!["predictable".into(), "fresh".into()],
        );
        scaffold(&plan, &destination).unwrap();
        assert!(stage.is_dir());
        assert_eq!(
            fs::read(stage.join("attacker-marker")).unwrap(),
            b"untouched"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn source_replacement_before_publish_fails_closed() {
        let temp = tempdir().unwrap();
        let parent = fs::canonicalize(temp.path()).unwrap();
        let plan = plan_file(&parent, PersonaProfile::Tiny);
        let destination = parent.join("workspace");
        let replacement = parent.join("replacement.json");
        fs::copy(&plan, &replacement).unwrap();
        let plan_for_hook = plan.clone();
        let replacement_for_hook = replacement.clone();
        install_before_rename_hook(
            destination.clone(),
            Box::new(move || {
                fs::remove_file(&plan_for_hook).unwrap();
                fs::rename(&replacement_for_hook, &plan_for_hook).unwrap();
            }),
        );
        assert!(matches!(
            scaffold(&plan, &destination),
            Err(PersonaScaffoldError::Artifact(_))
        ));
        assert!(!destination.exists());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn named_stage_replacement_before_publish_fails_closed() {
        let temp = tempdir().unwrap();
        let parent = fs::canonicalize(temp.path()).unwrap();
        let plan = plan_file(&parent, PersonaProfile::Tiny);
        let destination = parent.join("workspace");
        let stage = parent.join(format!("{STAGE_PREFIX}swap"));
        let moved = parent.join("moved-stage");
        let stage_for_hook = stage.clone();
        let moved_for_hook = moved.clone();
        install_stage_tokens(destination.clone(), vec!["swap".into()]);
        install_before_rename_hook(
            destination.clone(),
            Box::new(move || {
                fs::rename(&stage_for_hook, &moved_for_hook).unwrap();
                fs::create_dir(&stage_for_hook).unwrap();
            }),
        );
        assert!(matches!(
            scaffold(&plan, &destination),
            Err(PersonaScaffoldError::Unsafe(_))
        ));
        assert!(moved.is_dir());
        assert!(!destination.exists());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn rejects_partial_and_linked_plan_inputs_before_root_mutation() {
        let temp = tempdir().unwrap();
        let parent = fs::canonicalize(temp.path()).unwrap();
        let destination = parent.join("workspace");
        let partial = parent.join("partial.json");
        fs::write(&partial, b"{}\n").unwrap();
        assert!(matches!(
            scaffold(&partial, &destination),
            Err(PersonaScaffoldError::Artifact(_)) | Err(PersonaScaffoldError::Plan(_))
        ));
        assert!(!destination.exists());

        let plan = plan_file(&parent, PersonaProfile::Tiny);
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let linked = parent.join("linked.json");
            symlink(&plan, &linked).unwrap();
            assert!(matches!(
                scaffold(&linked, &destination),
                Err(PersonaScaffoldError::Artifact(_))
            ));
            assert!(!destination.exists());
            fs::remove_file(&linked).unwrap();
            fs::hard_link(&plan, &linked).unwrap();
            assert!(matches!(
                scaffold(&linked, &destination),
                Err(PersonaScaffoldError::Artifact(_))
            ));
            assert!(!destination.exists());
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn destination_created_at_publish_barrier_is_never_overwritten() {
        let temp = tempdir().unwrap();
        let parent = fs::canonicalize(temp.path()).unwrap();
        let plan = plan_file(&parent, PersonaProfile::Tiny);
        let destination = parent.join("workspace");
        let destination_for_hook = destination.clone();
        install_before_rename_hook(
            destination.clone(),
            Box::new(move || {
                fs::write(&destination_for_hook, b"concurrent destination").unwrap();
            }),
        );
        assert!(matches!(
            scaffold(&plan, &destination),
            Err(PersonaScaffoldError::Indeterminate(_))
        ));
        assert_eq!(fs::read(&destination).unwrap(), b"concurrent destination");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn static_control_parents_reject_injected_ids_while_leaves_remain_writable() {
        use std::os::unix::fs::MetadataExt;
        let temp = tempdir().unwrap();
        let parent = fs::canonicalize(temp.path()).unwrap();
        let plan = plan_file(&parent, PersonaProfile::Tiny);
        let destination = parent.join("workspace");
        scaffold(&plan, &destination).unwrap();
        let control = destination.join("_control");
        let personas = control.join("personas");
        let scopes = control.join("scopes");
        assert_eq!(fs::metadata(&control).unwrap().mode() & 0o777, 0o500);
        assert_eq!(fs::metadata(&personas).unwrap().mode() & 0o777, 0o500);
        assert_eq!(fs::metadata(&scopes).unwrap().mode() & 0o777, 0o500);
        assert_eq!(
            fs::create_dir(personas.join("injected-persona"))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            fs::create_dir(scopes.join("injected-persona"))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        let p01_scopes = scopes.join("p01");
        assert_eq!(fs::metadata(&p01_scopes).unwrap().mode() & 0o777, 0o500);
        assert_eq!(
            fs::create_dir(p01_scopes.join("injected-scope"))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        fs::write(personas.join("p01").join("lease-state"), b"opaque").unwrap();
        fs::write(
            p01_scopes.join("p01-primary-01").join("lease-state"),
            b"opaque",
        )
        .unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn runtime_binding_treats_scope_payloads_as_opaque_but_keeps_routes_closed() {
        let temp = tempdir().unwrap();
        let parent = fs::canonicalize(temp.path()).unwrap();
        let plan_path = plan_file(&parent, PersonaProfile::Tiny);
        let plan = PersonaPlan::parse_canonical(&fs::read(&plan_path).unwrap()).unwrap();
        let destination = parent.join("workspace");
        scaffold(&plan_path, &destination).unwrap();

        // This exceeds the old global topology-entry ceiling. Binding must
        // still be plan-sized because the payload leaf is not enumerated.
        let leaf = payload_leaf(&destination, &plan);
        for n in 0..8193 {
            fs::write(leaf.join(format!("payload-{n:05}")), b"opaque").unwrap();
        }
        bind_workspace(&destination).unwrap();

        for injected in [
            destination.join("unexpected-root"),
            destination.join("people").join("unexpected-person"),
            destination
                .join("people")
                .join(format!("p01-{}", plan.personas[0].role))
                .join("home")
                .join("unexpected-intermediate"),
            destination
                .join("_control/personas/p01")
                .join("unexpected-control"),
        ] {
            fs::write(&injected, b"reject").unwrap();
            assert!(
                bind_workspace(&destination).is_err(),
                "accepted {injected:?}"
            );
            fs::remove_file(injected).unwrap();
        }
        bind_workspace(&destination).unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn runtime_binding_ignores_payload_internal_links_but_rejects_a_linked_payload_route() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let parent = fs::canonicalize(temp.path()).unwrap();
        let plan_path = plan_file(&parent, PersonaProfile::Tiny);
        let plan = PersonaPlan::parse_canonical(&fs::read(&plan_path).unwrap()).unwrap();
        let destination = parent.join("workspace");
        scaffold(&plan_path, &destination).unwrap();

        // Payload contents do not grant lease authority. They are deliberately
        // opaque even when ordinary workspace data contains links.
        let leaf = payload_leaf(&destination, &plan);
        let external = parent.join("ordinary-payload");
        fs::write(&external, b"payload").unwrap();
        symlink(&external, leaf.join("opaque-symlink")).unwrap();
        fs::hard_link(&external, leaf.join("opaque-hardlink")).unwrap();
        bind_workspace(&destination).unwrap();

        // The plan-derived route to that opaque leaf remains authority. A
        // replacement link must therefore be rejected before lease mutation.
        let parked = parent.join("parked-payload-leaf");
        fs::rename(&leaf, &parked).unwrap();
        symlink(&parked, &leaf).unwrap();
        assert!(bind_workspace(&destination).is_err());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn scaffold_never_adopts_a_payload_filled_existing_root() {
        let temp = tempdir().unwrap();
        let parent = fs::canonicalize(temp.path()).unwrap();
        let plan = plan_file(&parent, PersonaProfile::Tiny);
        let destination = parent.join("workspace");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("payload"), b"must remain untouched").unwrap();

        assert!(matches!(
            scaffold(&plan, &destination),
            Err(PersonaScaffoldError::AlreadyExists(path)) if path == destination
        ));
        assert_eq!(
            fs::read(destination.join("payload")).unwrap(),
            b"must remain untouched"
        );
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    #[test]
    fn unsupported_platform_fails_before_input_or_root_mutation() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("never-created");
        assert!(matches!(
            scaffold(Path::new("relative-plan"), &root),
            Err(PersonaScaffoldError::Unsupported)
        ));
        assert!(!root.exists());
    }
}
