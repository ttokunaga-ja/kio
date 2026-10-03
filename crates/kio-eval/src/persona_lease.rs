//! Descriptor-bound leases over a Rust-owned persona workspace.
//!
//! There is intentionally no owner-digest argument: [`bind_workspace`] reads
//! and validates the sealed owner and plan before any mutable control state is
//! opened.
use crate::{
    boundary::sync_retained_directory,
    persona_scaffold::{PersonaScaffoldError, WorkspaceAuthority, bind_workspace},
};
use cap_primitives::fs as cap_fs;
use kio_core::cas::{canonical_json_bytes, hash_bytes};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
    sync::Mutex,
};
use thiserror::Error;

const LEASE: &str = "lease.json";
const LOCK: &str = ".lease.lock";
const RECOVERY: &str = "lease-recovery.jsonl";
const MAX_LEASE: usize = 16 * 1024;
const MAX_LOG: usize = 64 * 1024;
const MAX_LABEL: usize = 256;
const MAX_REASON: usize = 2048;

#[derive(Debug, Error)]
pub enum PersonaLeaseError {
    #[error("unsafe persona lease: {0}")]
    Unsafe(String),
    #[error("persona lease I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("persona lease scaffold: {0}")]
    Scaffold(#[from] PersonaScaffoldError),
    #[error("persona lease already exists")]
    AlreadyExists,
    #[error("persona lease mutation is indeterminate: {0}")]
    Indeterminate(String),
    #[error("persona lease unsupported on this platform")]
    Unsupported,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParentLease {
    pub schema: String,
    pub persona_id: String,
    pub session: String,
    pub owner_label: Option<String>,
    pub claimed_at: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeLease {
    pub schema: String,
    pub persona_id: String,
    pub scope_id: String,
    pub parent_session: String,
    pub worker_session: String,
    pub owner_label: Option<String>,
    pub claimed_at: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Claimed<T> {
    #[serde(flatten)]
    pub lease: T,
    pub release_token: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recovery<T> {
    pub schema: String,
    pub action: String,
    pub reason: String,
    pub lease: T,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredParent {
    schema: String,
    persona_id: String,
    session: String,
    owner_label: Option<String>,
    claimed_at: u64,
    release_token_sha256: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredScope {
    schema: String,
    persona_id: String,
    scope_id: String,
    parent_session: String,
    worker_session: String,
    owner_label: Option<String>,
    claimed_at: u64,
    release_token_sha256: String,
}

pub fn claim(
    root: &Path,
    persona: &str,
    session: &str,
    label: Option<&str>,
) -> Result<Claimed<ParentLease>, PersonaLeaseError> {
    valid_member(persona, "persona")?;
    valid_session(session)?;
    valid_label(label)?;
    let mut a = bind_workspace(root)?;
    ensure_persona(&a, persona)?;
    let dir = persona_dir(&a, persona)?;
    dir.recheck()?;
    let _lock = guard(dir.file())?;
    dir.after_mutation()?;
    a.recheck()?;
    // scope_claim takes this same parent guard first.  While holding it, a
    // validated scope lease is a stable veto: do not create a replacement
    // parent session over an orphaned child lease.
    if active_scopes(&a, persona)?.next().is_some() {
        return bad("active scope lease blocks parent claim");
    }
    let token = token()?;
    let stored = StoredParent {
        schema: "kio.persona.lease/v1".into(),
        persona_id: persona.into(),
        session: session.into(),
        owner_label: label.map(str::to_owned),
        claimed_at: now()?,
        release_token_sha256: hash_bytes(token.as_bytes()),
    };
    create(dir.file(), &stored)?;
    post_state("create", || {
        dir.after_mutation()?;
        if read_parent(dir.file(), persona)? != stored {
            return bad("parent lease readback differs");
        }
        fault("create")?;
        Ok(a.recheck()?)
    })?;
    Ok(Claimed {
        lease: parent_public(stored),
        release_token: token,
    })
}
pub fn show(root: &Path, persona: &str) -> Result<ParentLease, PersonaLeaseError> {
    valid_member(persona, "persona")?;
    let mut a = bind_workspace(root)?;
    ensure_persona(&a, persona)?;
    let d = persona_dir(&a, persona)?;
    d.recheck()?;
    let _l = guard(d.file())?;
    d.after_mutation()?;
    a.recheck()?;
    let out = parent_public(read_parent(d.file(), persona)?);
    d.recheck()?;
    a.recheck()?;
    Ok(out)
}
pub fn release(root: &Path, persona: &str, token: &str) -> Result<ParentLease, PersonaLeaseError> {
    valid_member(persona, "persona")?;
    valid_token(token)?;
    let mut a = bind_workspace(root)?;
    ensure_persona(&a, persona)?;
    let d = persona_dir(&a, persona)?;
    d.recheck()?;
    let _l = guard(d.file())?;
    d.after_mutation()?;
    let v = read_parent(d.file(), persona)?;
    if !ct_eq(&v.release_token_sha256, &hash_bytes(token.as_bytes())) {
        return bad("release token mismatch");
    }
    if active_scopes(&a, persona)?.next().is_some() {
        return bad("active scope lease blocks parent release");
    }
    a.recheck()?;
    remove(d.file(), LEASE)?;
    post_state("remove", || {
        d.after_mutation()?;
        fault("remove")?;
        Ok(a.recheck()?)
    })?;
    Ok(parent_public(v))
}
pub fn recover(
    root: &Path,
    persona: &str,
    session: &str,
    reason: &str,
) -> Result<Recovery<ParentLease>, PersonaLeaseError> {
    valid_member(persona, "persona")?;
    valid_session(session)?;
    let reason = valid_reason(reason)?;
    let mut a = bind_workspace(root)?;
    ensure_persona(&a, persona)?;
    let d = persona_dir(&a, persona)?;
    d.recheck()?;
    let _l = guard(d.file())?;
    d.after_mutation()?;
    let v = read_parent(d.file(), persona)?;
    if v.session != session {
        return bad("parent session changed");
    }
    if active_scopes(&a, persona)?.next().is_some() {
        return bad("active scope lease blocks parent recovery");
    }
    a.recheck()?;
    let p = parent_public(v);
    let recovered_reason = reason.clone();
    append(
        d.file(),
        &Recovery {
            schema: "kio.persona.lease-recovery/v1".into(),
            action: "forced-recovery".into(),
            reason,
            lease: p.clone(),
        },
    )?;
    post_state("recovery", || {
        d.after_mutation()?;
        fault("append")?;
        a.recheck()?;
        remove(d.file(), LEASE)?;
        d.after_mutation()?;
        fault("remove")?;
        Ok(a.recheck()?)
    })?;
    Ok(Recovery {
        schema: "kio.persona.lease-recovery/v1".into(),
        action: "forced-recovery".into(),
        reason: recovered_reason,
        lease: p,
    })
}
pub fn scope_claim(
    root: &Path,
    persona: &str,
    scope: &str,
    parent_session: &str,
    worker_session: &str,
    label: Option<&str>,
) -> Result<Claimed<ScopeLease>, PersonaLeaseError> {
    valid_member(persona, "persona")?;
    valid_member(scope, "scope")?;
    valid_session(parent_session)?;
    valid_session(worker_session)?;
    valid_label(label)?;
    let mut a = bind_workspace(root)?;
    ensure_scope(&a, persona, scope)?;
    let p = persona_dir(&a, persona)?;
    p.recheck()?;
    let _pl = guard(p.file())?;
    p.recheck()?;
    if read_parent(p.file(), persona)?.session != parent_session {
        return bad("parent session changed");
    }
    let d = scope_dir(&a, persona, scope)?;
    d.recheck()?;
    let _l = guard(d.file())?;
    d.after_mutation()?;
    a.recheck()?;
    let token = token()?;
    let v = StoredScope {
        schema: "kio.persona.scope-lease/v1".into(),
        persona_id: persona.into(),
        scope_id: scope.into(),
        parent_session: parent_session.into(),
        worker_session: worker_session.into(),
        owner_label: label.map(str::to_owned),
        claimed_at: now()?,
        release_token_sha256: hash_bytes(token.as_bytes()),
    };
    create(d.file(), &v)?;
    post_state("create", || {
        d.after_mutation()?;
        if read_scope(d.file(), persona, scope)? != v {
            return bad("scope lease readback differs");
        }
        fault("create")?;
        Ok(a.recheck()?)
    })?;
    Ok(Claimed {
        lease: scope_public(v),
        release_token: token,
    })
}
pub fn scope_show(
    root: &Path,
    persona: &str,
    scope: &str,
) -> Result<ScopeLease, PersonaLeaseError> {
    valid_member(persona, "persona")?;
    valid_member(scope, "scope")?;
    let mut a = bind_workspace(root)?;
    ensure_scope(&a, persona, scope)?;
    let d = scope_dir(&a, persona, scope)?;
    d.recheck()?;
    let _l = guard(d.file())?;
    d.after_mutation()?;
    a.recheck()?;
    let out = scope_public(read_scope(d.file(), persona, scope)?);
    d.recheck()?;
    a.recheck()?;
    Ok(out)
}
pub fn scope_release(
    root: &Path,
    persona: &str,
    scope: &str,
    parent_session: &str,
    token: &str,
) -> Result<ScopeLease, PersonaLeaseError> {
    valid_member(persona, "persona")?;
    valid_member(scope, "scope")?;
    valid_session(parent_session)?;
    valid_token(token)?;
    let mut a = bind_workspace(root)?;
    ensure_scope(&a, persona, scope)?;
    let p = persona_dir(&a, persona)?;
    p.recheck()?;
    let _pl = guard(p.file())?;
    p.after_mutation()?;
    if read_parent(p.file(), persona)?.session != parent_session {
        return bad("parent session changed");
    }
    let d = scope_dir(&a, persona, scope)?;
    d.recheck()?;
    let _l = guard(d.file())?;
    d.after_mutation()?;
    let v = read_scope(d.file(), persona, scope)?;
    if v.parent_session != parent_session
        || !ct_eq(&v.release_token_sha256, &hash_bytes(token.as_bytes()))
    {
        return bad("scope token mismatch");
    }
    a.recheck()?;
    remove(d.file(), LEASE)?;
    post_state("remove", || {
        d.after_mutation()?;
        fault("remove")?;
        Ok(a.recheck()?)
    })?;
    Ok(scope_public(v))
}
pub fn scope_recover(
    root: &Path,
    persona: &str,
    scope: &str,
    parent_session: &str,
    worker_session: &str,
    reason: &str,
) -> Result<Recovery<ScopeLease>, PersonaLeaseError> {
    valid_member(persona, "persona")?;
    valid_member(scope, "scope")?;
    valid_session(parent_session)?;
    valid_session(worker_session)?;
    let reason = valid_reason(reason)?;
    let mut a = bind_workspace(root)?;
    ensure_scope(&a, persona, scope)?;
    let p = persona_dir(&a, persona)?;
    p.recheck()?;
    let _pl = guard(p.file())?;
    p.after_mutation()?;
    match read_parent(p.file(), persona) {
        Ok(parent) if parent.session == parent_session => {}
        Ok(_) => return bad("parent session changed"),
        Err(PersonaLeaseError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let d = scope_dir(&a, persona, scope)?;
    d.recheck()?;
    let _l = guard(d.file())?;
    d.after_mutation()?;
    let v = read_scope(d.file(), persona, scope)?;
    if v.parent_session != parent_session || v.worker_session != worker_session {
        return bad("scope session changed");
    }
    a.recheck()?;
    let pubv = scope_public(v);
    let recovered_reason = reason.clone();
    append(
        d.file(),
        &Recovery {
            schema: "kio.persona.lease-recovery/v1".into(),
            action: "forced-recovery".into(),
            reason,
            lease: pubv.clone(),
        },
    )?;
    post_state("recovery", || {
        d.after_mutation()?;
        fault("append")?;
        a.recheck()?;
        remove(d.file(), LEASE)?;
        d.after_mutation()?;
        fault("remove")?;
        Ok(a.recheck()?)
    })?;
    Ok(Recovery {
        schema: "kio.persona.lease-recovery/v1".into(),
        action: "forced-recovery".into(),
        reason: recovered_reason,
        lease: pubv,
    })
}

/// A writable leaf bound both by its retained descriptor and by the exact
/// name in its retained parent.  A pathname cannot be swapped underneath a
/// lease operation without one of these checks failing.
struct BoundLeaseDir {
    chain: Vec<BoundLeaseComponent>,
}
struct BoundLeaseComponent {
    parent: fs::File,
    parent_meta: cap_fs::Metadata,
    name: String,
    child: fs::File,
    meta: Mutex<cap_fs::Metadata>,
}
impl BoundLeaseDir {
    fn open(control: fs::File, names: &[&str]) -> Result<Self, PersonaLeaseError> {
        let mut parent = control;
        let mut chain = Vec::with_capacity(names.len());
        for (index, name) in names.iter().enumerate() {
            let parent_meta = cap_fs::Metadata::from_file(&parent)?;
            let named = cap_fs::stat(&parent, Path::new(name), cap_fs::FollowSymlinks::No)?;
            let child = cap_fs::open_dir_nofollow(&parent, Path::new(name))?;
            let opened = cap_fs::Metadata::from_file(&child)?;
            if !same_cap(&named, &opened) || !lease_dir(&opened, index + 1 == names.len()) {
                return bad("lease control directory changed while opening");
            }
            chain.push(BoundLeaseComponent {
                parent,
                parent_meta,
                name: (*name).into(),
                child: child.try_clone()?,
                meta: Mutex::new(opened),
            });
            parent = child;
        }
        Ok(Self { chain })
    }
    fn file(&self) -> &fs::File {
        &self.chain.last().expect("non-empty lease chain").child
    }
    fn recheck(&self) -> Result<(), PersonaLeaseError> {
        for component in &self.chain {
            let parent = cap_fs::Metadata::from_file(&component.parent)?;
            let named = cap_fs::stat(
                &component.parent,
                Path::new(&component.name),
                cap_fs::FollowSymlinks::No,
            )?;
            let opened = cap_fs::Metadata::from_file(&component.child)?;
            let saved = component
                .meta
                .lock()
                .map_err(|_| PersonaLeaseError::Unsafe("lease directory lock poisoned".into()))?;
            let final_leaf = self
                .chain
                .last()
                .is_some_and(|x| std::ptr::eq(x, component));
            if !same_cap(&component.parent_meta, &parent)
                || !same_cap(&saved, &named)
                || !same_cap(&saved, &opened)
                || !lease_dir(&opened, final_leaf)
            {
                return bad("lease control directory changed during operation");
            }
        }
        Ok(())
    }
    /// Use only immediately after a successful mutation of this leaf.  It
    /// preserves identity while advancing the expected metadata snapshot.
    fn after_mutation(&self) -> Result<(), PersonaLeaseError> {
        for (index, component) in self.chain.iter().enumerate() {
            let parent = cap_fs::Metadata::from_file(&component.parent)?;
            let named = cap_fs::stat(
                &component.parent,
                Path::new(&component.name),
                cap_fs::FollowSymlinks::No,
            )?;
            let opened = cap_fs::Metadata::from_file(&component.child)?;
            let mut saved = component
                .meta
                .lock()
                .map_err(|_| PersonaLeaseError::Unsafe("lease directory lock poisoned".into()))?;
            let final_leaf = index + 1 == self.chain.len();
            let valid = if final_leaf {
                same_identity(&saved, &named)
            } else {
                same_cap(&saved, &named)
            };
            if !same_cap(&component.parent_meta, &parent)
                || !valid
                || !same_cap(&named, &opened)
                || !lease_dir(&opened, final_leaf)
            {
                return bad("lease control directory changed after mutation");
            }
            if final_leaf {
                *saved = opened;
            }
        }
        Ok(())
    }
}
fn lease_dir(m: &cap_fs::Metadata, writable: bool) -> bool {
    #[cfg(unix)]
    {
        use cap_primitives::fs::MetadataExt;
        m.is_dir()
            && !m.file_type().is_symlink()
            && m.mode() & 0o777 == if writable { 0o700 } else { 0o500 }
            && m.uid() == unsafe { libc::geteuid() }
    }
    #[cfg(not(unix))]
    {
        let _ = (m, writable);
        false
    }
}
fn persona_dir(a: &WorkspaceAuthority, p: &str) -> Result<BoundLeaseDir, PersonaLeaseError> {
    BoundLeaseDir::open(a.control.try_clone()?, &["personas", p])
}
fn scope_dir(a: &WorkspaceAuthority, p: &str, s: &str) -> Result<BoundLeaseDir, PersonaLeaseError> {
    BoundLeaseDir::open(a.control.try_clone()?, &["scopes", p, s])
}
fn ensure_persona(a: &WorkspaceAuthority, p: &str) -> Result<(), PersonaLeaseError> {
    if a.persona(p) {
        Ok(())
    } else {
        bad("unknown persona")
    }
}
fn ensure_scope(a: &WorkspaceAuthority, p: &str, s: &str) -> Result<(), PersonaLeaseError> {
    if a.scope(p, s) {
        Ok(())
    } else {
        bad("unknown persona scope")
    }
}
fn create<T: Serialize>(d: &fs::File, v: &T) -> Result<(), PersonaLeaseError> {
    let mut b = canonical_json_bytes(
        &serde_json::to_value(v).map_err(|e| PersonaLeaseError::Unsafe(e.to_string()))?,
    )
    .map_err(|e| PersonaLeaseError::Unsafe(e.to_string()))?;
    b.push(b'\n');
    let mut o = cap_fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use cap_primitives::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let mut f = cap_fs::open(d, Path::new(LEASE), &o).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            PersonaLeaseError::AlreadyExists
        } else {
            e.into()
        }
    })?;
    // From this point onward the create-new inode is visible by name.  Any
    // error must therefore prevent a caller from assuming that retry is safe.
    post_state("lease file creation", || {
        f.write_all(&b)?;
        f.sync_all()?;
        regular_private(&cap_fs::Metadata::from_file(&f)?, MAX_LEASE, LEASE)?;
        sync(d)?;
        if read_private(d, LEASE, MAX_LEASE)? != b {
            return bad("lease file readback differs");
        }
        Ok(())
    })
}
fn read<T: for<'a> Deserialize<'a> + Serialize>(
    d: &fs::File,
    name: &str,
) -> Result<T, PersonaLeaseError> {
    let named = cap_fs::stat(d, Path::new(name), cap_fs::FollowSymlinks::No)?;
    regular_private(&named, MAX_LEASE, name)?;
    let mut o = cap_fs::OpenOptions::new();
    o.read(true)._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let f = cap_fs::open(d, Path::new(name), &o)?;
    let opened = cap_fs::Metadata::from_file(&f)?;
    if !same_cap(&named, &opened) {
        return bad("lease changed while opening");
    }
    let mut b = Vec::new();
    f.take(MAX_LEASE as u64 + 1).read_to_end(&mut b)?;
    if b.len() > MAX_LEASE || !b.ends_with(b"\n") {
        return bad("lease bytes invalid");
    }
    let after = cap_fs::stat(d, Path::new(name), cap_fs::FollowSymlinks::No)?;
    if !same_cap(&opened, &after) {
        return bad("lease changed while reading");
    }
    let v: T = serde_json::from_slice(&b)
        .map_err(|_| PersonaLeaseError::Unsafe("lease JSON invalid".into()))?;
    let mut c = canonical_json_bytes(
        &serde_json::to_value(&v).map_err(|e| PersonaLeaseError::Unsafe(e.to_string()))?,
    )
    .map_err(|e| PersonaLeaseError::Unsafe(e.to_string()))?;
    c.push(b'\n');
    if c != b {
        return bad("lease not canonical");
    }
    Ok(v)
}
fn read_parent(d: &fs::File, p: &str) -> Result<StoredParent, PersonaLeaseError> {
    let v: StoredParent = read(d, LEASE)?;
    if v.schema != "kio.persona.lease/v1" || v.persona_id != p {
        return bad("parent lease schema invalid");
    }
    valid_session(&v.session)?;
    valid_label(v.owner_label.as_deref())?;
    valid_digest(&v.release_token_sha256)?;
    valid_claimed_at(v.claimed_at)?;
    Ok(v)
}
fn read_scope(d: &fs::File, p: &str, s: &str) -> Result<StoredScope, PersonaLeaseError> {
    let v: StoredScope = read(d, LEASE)?;
    if v.schema != "kio.persona.scope-lease/v1" || v.persona_id != p || v.scope_id != s {
        return bad("scope lease schema invalid");
    }
    valid_session(&v.parent_session)?;
    valid_session(&v.worker_session)?;
    valid_label(v.owner_label.as_deref())?;
    valid_digest(&v.release_token_sha256)?;
    valid_claimed_at(v.claimed_at)?;
    Ok(v)
}
fn parent_public(v: StoredParent) -> ParentLease {
    ParentLease {
        schema: v.schema,
        persona_id: v.persona_id,
        session: v.session,
        owner_label: v.owner_label,
        claimed_at: v.claimed_at,
    }
}
fn scope_public(v: StoredScope) -> ScopeLease {
    ScopeLease {
        schema: v.schema,
        persona_id: v.persona_id,
        scope_id: v.scope_id,
        parent_session: v.parent_session,
        worker_session: v.worker_session,
        owner_label: v.owner_label,
        claimed_at: v.claimed_at,
    }
}
fn remove(d: &fs::File, n: &str) -> Result<(), PersonaLeaseError> {
    cap_fs::remove_file(d, Path::new(n))?;
    post_state("lease file removal", || sync(d))
}
fn append<T: Serialize>(d: &fs::File, v: &T) -> Result<(), PersonaLeaseError> {
    let mut b = canonical_json_bytes(
        &serde_json::to_value(v).map_err(|e| PersonaLeaseError::Unsafe(e.to_string()))?,
    )
    .map_err(|e| PersonaLeaseError::Unsafe(e.to_string()))?;
    b.push(b'\n');
    let mut o = cap_fs::OpenOptions::new();
    o.write(true)
        .create(true)
        .append(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_primitives::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let before = stat_optional(d, RECOVERY)?;
    if let Some(ref existing) = before {
        regular_private(existing, MAX_LOG, RECOVERY)?;
        validate_log(&read_private(d, RECOVERY, MAX_LOG)?)?;
    }
    let mut f = cap_fs::open(d, Path::new(RECOVERY), &o)?;
    // `create(true)` may already have made a new log visible.  Conservatively
    // treat every subsequent failure as indeterminate, including validation
    // of an existing file, because an append-capable descriptor is now held.
    post_state("recovery log append", || {
        let opened = cap_fs::Metadata::from_file(&f)?;
        regular_private(&opened, MAX_LOG, RECOVERY)?;
        if let Some(before) = before
            && !same_cap(&before, &opened)
        {
            return bad("recovery log changed while opening");
        }
        if f.metadata()?.len() as usize + b.len() > MAX_LOG {
            return bad("recovery log exceeds bound");
        }
        f.write_all(&b)?;
        f.sync_all()?;
        let current = cap_fs::Metadata::from_file(&f)?;
        let after = cap_fs::stat(d, Path::new(RECOVERY), cap_fs::FollowSymlinks::No)?;
        if !same_cap(&current, &after) {
            return bad("recovery log changed while appending");
        }
        let readback = read_private(d, RECOVERY, MAX_LOG)?;
        validate_log(&readback)?;
        if !readback.ends_with(&b) {
            return bad("recovery log append readback differs");
        }
        sync(d)
    })
}

fn stat_optional(
    directory: &fs::File,
    name: &str,
) -> Result<Option<cap_fs::Metadata>, PersonaLeaseError> {
    match cap_fs::stat(directory, Path::new(name), cap_fs::FollowSymlinks::No) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}
fn read_private(d: &fs::File, name: &str, maximum: usize) -> Result<Vec<u8>, PersonaLeaseError> {
    let named = cap_fs::stat(d, Path::new(name), cap_fs::FollowSymlinks::No)?;
    regular_private(&named, maximum, name)?;
    let mut o = cap_fs::OpenOptions::new();
    o.read(true)._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let f = cap_fs::open(d, Path::new(name), &o)?;
    let opened = cap_fs::Metadata::from_file(&f)?;
    if !same_cap(&named, &opened) {
        return bad("private file changed while opening");
    }
    let mut b = Vec::new();
    f.take(maximum as u64 + 1).read_to_end(&mut b)?;
    let after = cap_fs::stat(d, Path::new(name), cap_fs::FollowSymlinks::No)?;
    if b.len() > maximum || !same_cap(&opened, &after) {
        return bad("private file changed while reading");
    }
    Ok(b)
}
fn validate_log(bytes: &[u8]) -> Result<(), PersonaLeaseError> {
    if bytes.len() > MAX_LOG || (!bytes.is_empty() && !bytes.ends_with(b"\n")) {
        return bad("recovery log terminator invalid");
    }
    for line in bytes.split(|b| *b == b'\n').filter(|x| !x.is_empty()) {
        let value: serde_json::Value = serde_json::from_slice(line)
            .map_err(|_| PersonaLeaseError::Unsafe("recovery log JSON invalid".into()))?;
        let canonical =
            canonical_json_bytes(&value).map_err(|e| PersonaLeaseError::Unsafe(e.to_string()))?;
        if canonical != line {
            return bad("recovery log row not canonical");
        }
        if let Ok(v) = serde_json::from_value::<Recovery<ParentLease>>(value.clone()) {
            if v.schema != "kio.persona.lease-recovery/v1"
                || v.action != "forced-recovery"
                || v.lease.schema != "kio.persona.lease/v1"
            {
                return bad("recovery parent schema invalid");
            }
            valid_reason(&v.reason)?;
            valid_member(&v.lease.persona_id, "persona")?;
            valid_session(&v.lease.session)?;
            valid_label(v.lease.owner_label.as_deref())?;
            valid_claimed_at(v.lease.claimed_at)?;
        } else if let Ok(v) = serde_json::from_value::<Recovery<ScopeLease>>(value) {
            if v.schema != "kio.persona.lease-recovery/v1"
                || v.action != "forced-recovery"
                || v.lease.schema != "kio.persona.scope-lease/v1"
            {
                return bad("recovery scope schema invalid");
            }
            valid_reason(&v.reason)?;
            valid_member(&v.lease.persona_id, "persona")?;
            valid_member(&v.lease.scope_id, "scope")?;
            valid_session(&v.lease.parent_session)?;
            valid_session(&v.lease.worker_session)?;
            valid_label(v.lease.owner_label.as_deref())?;
            valid_claimed_at(v.lease.claimed_at)?;
        } else {
            return bad("recovery log schema invalid");
        }
    }
    Ok(())
}
fn active_scopes<'a>(
    a: &'a WorkspaceAuthority,
    p: &'a str,
) -> Result<impl Iterator<Item = String> + 'a, PersonaLeaseError> {
    let (scopes, scopes_meta) = open_static_child(&a.control, "scopes")?;
    let (d, persona_meta) = open_static_child(&scopes, p)?;
    let mut out = Vec::new();
    for e in cap_fs::read_dir(&d, Path::new("."))? {
        let n = e?
            .file_name()
            .into_string()
            .map_err(|_| PersonaLeaseError::Unsafe("nonutf8 scope".into()))?;
        if !a.scope(p, &n) {
            return bad("unknown scope control entry");
        }
        let bound = scope_dir(a, p, &n)?;
        bound.recheck()?;
        if stat_optional(bound.file(), LEASE)?.is_some() {
            read_scope(bound.file(), p, &n)?;
            bound.recheck()?;
            out.push(n);
        }
    }
    recheck_static_child(&scopes, p, &persona_meta, &d)?;
    recheck_static_child(&a.control, "scopes", &scopes_meta, &scopes)?;
    Ok(out.into_iter())
}

fn open_static_child(
    parent: &fs::File,
    name: &str,
) -> Result<(fs::File, cap_fs::Metadata), PersonaLeaseError> {
    let named = cap_fs::stat(parent, Path::new(name), cap_fs::FollowSymlinks::No)?;
    let child = cap_fs::open_dir_nofollow(parent, Path::new(name))?;
    let opened = cap_fs::Metadata::from_file(&child)?;
    if !same_cap(&named, &opened) || !lease_dir(&opened, false) {
        return bad("static lease routing directory changed while opening");
    }
    Ok((child, opened))
}

fn recheck_static_child(
    parent: &fs::File,
    name: &str,
    expected: &cap_fs::Metadata,
    child: &fs::File,
) -> Result<(), PersonaLeaseError> {
    let named = cap_fs::stat(parent, Path::new(name), cap_fs::FollowSymlinks::No)?;
    let opened = cap_fs::Metadata::from_file(child)?;
    if !same_cap(expected, &named) || !same_cap(expected, &opened) || !lease_dir(&opened, false) {
        return bad("static lease routing directory changed during enumeration");
    }
    Ok(())
}
struct Guard {
    // Retain the lock handle on every platform for the full guard lifetime.
    _file: fs::File,
    _metadata: cap_fs::Metadata,
}
impl Drop for Guard {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::flock(std::os::fd::AsRawFd::as_raw_fd(&self._file), libc::LOCK_UN);
        }
    }
}
fn guard(d: &fs::File) -> Result<Guard, PersonaLeaseError> {
    let mut o = cap_fs::OpenOptions::new();
    o.read(true)
        .write(true)
        .create(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_primitives::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let named = stat_optional(d, LOCK)?;
    let created = named.is_none();
    let f = cap_fs::open(d, Path::new(LOCK), &o)?;
    let opened = cap_fs::Metadata::from_file(&f)?;
    regular_private(&opened, 0, LOCK)?;
    if let Some(named) = named.as_ref()
        && !same_cap(named, &opened)
    {
        return bad("lock changed while opening");
    }
    if created {
        f.sync_all()?;
        sync(d)?;
    }
    #[cfg(unix)]
    if unsafe {
        libc::flock(
            std::os::fd::AsRawFd::as_raw_fd(&f),
            libc::LOCK_EX | libc::LOCK_NB,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let post = cap_fs::stat(d, Path::new(LOCK), cap_fs::FollowSymlinks::No)?;
    if !same_cap(&opened, &post) {
        return bad("lock changed while locking");
    }
    Ok(Guard {
        _file: f,
        _metadata: opened,
    })
}
fn regular_private(
    m: &cap_fs::Metadata,
    maximum: usize,
    label: &str,
) -> Result<(), PersonaLeaseError> {
    #[cfg(unix)]
    {
        use cap_primitives::fs::MetadataExt;
        if !m.file_type().is_file()
            || m.file_type().is_symlink()
            || m.nlink() != 1
            || m.len() as usize > maximum
            || m.mode() & 0o777 != 0o600
            || m.uid() != unsafe { libc::geteuid() }
        {
            return bad(format!(
                "{label} must be a bounded single-link private regular file"
            ));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (m, maximum, label);
        Err(PersonaLeaseError::Unsupported)
    }
}
#[cfg(unix)]
fn same_cap(a: &cap_fs::Metadata, b: &cap_fs::Metadata) -> bool {
    use cap_primitives::fs::MetadataExt;
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
#[cfg(unix)]
fn same_identity(a: &cap_fs::Metadata, b: &cap_fs::Metadata) -> bool {
    use cap_primitives::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}
#[cfg(not(unix))]
fn same_identity(_: &cap_fs::Metadata, _: &cap_fs::Metadata) -> bool {
    false
}
#[cfg(not(unix))]
fn same_cap(_: &cap_fs::Metadata, _: &cap_fs::Metadata) -> bool {
    false
}
fn sync(d: &fs::File) -> Result<(), PersonaLeaseError> {
    let m = d.metadata()?;
    sync_retained_directory(d, &m, Path::new("."))
        .map_err(|e| PersonaLeaseError::Unsafe(e.to_string()))
}
fn valid_member(s: &str, what: &str) -> Result<(), PersonaLeaseError> {
    if s.is_empty()
        || s.len() > 128
        || matches!(s, "." | "..")
        || !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')
    {
        return bad(format!("{what} must be a safe direct component"));
    }
    Ok(())
}
fn valid_session(s: &str) -> Result<(), PersonaLeaseError> {
    if s.is_empty()
        || s.len() > 128
        || !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
    {
        return bad("session invalid");
    }
    Ok(())
}
fn valid_label(s: Option<&str>) -> Result<(), PersonaLeaseError> {
    if s.is_some_and(|x| {
        x.is_empty() || x.len() > MAX_LABEL || x.trim() != x || x.chars().any(char::is_control)
    }) {
        return bad("owner label exceeds bound");
    }
    Ok(())
}
fn valid_reason(s: &str) -> Result<String, PersonaLeaseError> {
    if s.is_empty() || s.len() > MAX_REASON || s.trim() != s || s.chars().any(char::is_control) {
        return bad("recovery reason invalid");
    }
    Ok(s.into())
}
fn valid_token(s: &str) -> Result<(), PersonaLeaseError> {
    if s.len() != 64
        || !s
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
    {
        return bad("release token invalid");
    }
    Ok(())
}
fn valid_digest(s: &str) -> Result<(), PersonaLeaseError> {
    if s.len() != 71
        || !s.starts_with("sha256:")
        || !s[7..]
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
    {
        return bad("release token digest invalid");
    }
    Ok(())
}
fn valid_claimed_at(value: u64) -> Result<(), PersonaLeaseError> {
    if value == 0 {
        return bad("claimed_at invalid");
    }
    Ok(())
}
fn token() -> Result<String, PersonaLeaseError> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| PersonaLeaseError::Unsafe(e.to_string()))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}
fn now() -> Result<u64, PersonaLeaseError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|x| x.as_secs())
        .map_err(|e| PersonaLeaseError::Unsafe(e.to_string()))
}
fn ct_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut d = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        d |= x ^ y;
    }
    d == 0
}
fn bad<T>(s: impl Into<String>) -> Result<T, PersonaLeaseError> {
    Err(PersonaLeaseError::Unsafe(s.into()))
}
fn post_state(
    action: &str,
    check: impl FnOnce() -> Result<(), PersonaLeaseError>,
) -> Result<(), PersonaLeaseError> {
    match check() {
        Ok(()) => Ok(()),
        Err(PersonaLeaseError::Indeterminate(reason)) => {
            Err(PersonaLeaseError::Indeterminate(reason))
        }
        Err(error) => Err(PersonaLeaseError::Indeterminate(format!(
            "{action} completed but post-state verification failed: {error}"
        ))),
    }
}

// A deliberately tiny deterministic test seam.  It is checked only after a
// durable state transition, so callers learn that retrying is unsafe rather
// than receiving a misleading ordinary I/O error.
#[cfg(test)]
thread_local! {
    static FAULT_AFTER: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}
fn fault(point: &str) -> Result<(), PersonaLeaseError> {
    #[cfg(test)]
    if FAULT_AFTER.with(|slot| {
        let hit = slot.get().is_some_and(|wanted| wanted == point);
        if hit {
            slot.set(None);
        }
        hit
    }) {
        return Err(PersonaLeaseError::Indeterminate(format!(
            "fault injected after {point}"
        )));
    }
    let _ = point;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use crate::{
        persona_plan::{PersonaProfile, frozen_plan},
        persona_scaffold::scaffold,
    };
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use std::path::PathBuf;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use std::sync::OnceLock;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use tempfile::tempdir;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    static TINY_PLAN: OnceLock<Vec<u8>> = OnceLock::new();

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn workspace() -> (tempfile::TempDir, PathBuf, String) {
        let temp = tempdir().unwrap();
        let parent = std::fs::canonicalize(temp.path()).unwrap();
        let plan = parent.join("plan.json");
        let bytes =
            TINY_PLAN.get_or_init(|| frozen_plan(PersonaProfile::Tiny).canonical_bytes().unwrap());
        let parsed = crate::persona_plan::PersonaPlan::parse_canonical(bytes).unwrap();
        std::fs::write(&plan, bytes).unwrap();
        let root = parent.join("workspace");
        scaffold(&plan, &root).unwrap();
        (temp, root, parsed.personas[0].scopes[0].id.clone())
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn payload_leaf(root: &Path, scope_id: &str) -> PathBuf {
        let plan = crate::persona_plan::PersonaPlan::parse_canonical(
            &std::fs::read(root.join("persona-plan.json")).unwrap(),
        )
        .unwrap();
        let person = plan
            .personas
            .iter()
            .find(|p| p.id.as_str() == "p01")
            .unwrap();
        let scope = person.scopes.iter().find(|s| s.id == scope_id).unwrap();
        root.join(format!(
            "people/{}-{}/home",
            person.id.as_str(),
            person.role
        ))
        .join(&scope.path)
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn parent_and_scope_lifecycle_is_descriptor_bound() {
        let (_t, root, scope) = workspace();
        let parent = claim(&root, "p01", "parent", Some("owner")).unwrap();
        assert_eq!(show(&root, "p01").unwrap().session, "parent");
        let child = scope_claim(&root, "p01", &scope, "parent", "worker", None).unwrap();
        assert!(release(&root, "p01", &parent.release_token).is_err());
        assert_eq!(
            scope_show(&root, "p01", &scope).unwrap().worker_session,
            "worker"
        );
        scope_release(&root, "p01", &scope, "parent", &child.release_token).unwrap();
        release(&root, "p01", &parent.release_token).unwrap();
        assert!(claim(&root, "p99", "x", None).is_err());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn populated_scope_payload_does_not_block_release_or_next_lifecycle() {
        let (_t, root, scope) = workspace();
        let parent = claim(&root, "p01", "parent-a", None).unwrap();
        let child = scope_claim(&root, "p01", &scope, "parent-a", "worker-a", None).unwrap();

        let payload = payload_leaf(&root, &scope);
        std::fs::write(payload.join("production-artifact.txt"), b"opaque payload").unwrap();
        std::fs::create_dir(payload.join("nested-output")).unwrap();
        std::fs::write(
            payload.join("nested-output").join("result.bin"),
            b"still opaque",
        )
        .unwrap();

        scope_release(&root, "p01", &scope, "parent-a", &child.release_token).unwrap();
        release(&root, "p01", &parent.release_token).unwrap();
        let next = claim(&root, "p01", "parent-b", None).unwrap();
        release(&root, "p01", &next.release_token).unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn orphan_scope_blocks_reclaim_and_can_be_explicitly_recovered() {
        let (_t, root, scope) = workspace();
        let parent = claim(&root, "p01", "parent-a", None).unwrap();
        let child = scope_claim(&root, "p01", &scope, "parent-a", "worker-a", None).unwrap();

        // Simulate the interrupted/manual parent deletion which used to allow
        // a new parent session to strand this child indefinitely.
        std::fs::remove_file(root.join("_control/personas/p01/lease.json")).unwrap();
        assert!(claim(&root, "p01", "parent-b", None).is_err());
        assert!(scope_release(&root, "p01", &scope, "parent-a", &child.release_token).is_err());
        assert!(!root.join("_control/personas/p01/lease.json").exists());

        let receipt = scope_recover(
            &root,
            "p01",
            &scope,
            "parent-a",
            "worker-a",
            "recover orphan",
        )
        .unwrap();
        assert_eq!(receipt.lease.parent_session, "parent-a");
        assert!(scope_show(&root, "p01", &scope).is_err());
        let next = claim(&root, "p01", "parent-b", None).unwrap();
        release(&root, "p01", &next.release_token).unwrap();
        assert_eq!(parent.lease.session, "parent-a");
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn token_is_returned_once_and_recovery_is_durable() {
        let (_t, root, _scope) = workspace();
        let parent = claim(&root, "p01", "parent", None).unwrap();
        assert!(
            !serde_json::to_string(&show(&root, "p01").unwrap())
                .unwrap()
                .contains("release_token")
        );
        assert!(release(&root, "p01", "wrong").is_err());
        let receipt = recover(&root, "p01", "parent", "stopped").unwrap();
        assert_eq!(receipt.reason, "stopped");
        assert!(
            root.join("_control/personas/p01/lease-recovery.jsonl")
                .is_file()
        );
        assert!(parent.release_token.len() >= 32);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn opaque_or_forged_workspace_authority_is_rejected_before_mutation() {
        let (_t, root, _scope) = workspace();
        let owner = root.join("persona-workspace-owner.json");
        let original_owner = std::fs::read(&owner).unwrap();
        let mut unknown: serde_json::Value = serde_json::from_slice(&original_owner).unwrap();
        unknown
            .as_object_mut()
            .unwrap()
            .insert("unknown".into(), serde_json::Value::Bool(true));
        let mut unknown = canonical_json_bytes(&unknown).unwrap();
        unknown.push(b'\n');
        let mut wrong_schema: serde_json::Value = serde_json::from_slice(&original_owner).unwrap();
        wrong_schema["schema"] = serde_json::Value::String("kio.persona.workspace-owner/v0".into());
        let mut wrong_schema = canonical_json_bytes(&wrong_schema).unwrap();
        wrong_schema.push(b'\n');
        let reordered = serde_json::to_vec_pretty(
            &serde_json::from_slice::<serde_json::Value>(&original_owner).unwrap(),
        )
        .unwrap();
        for invalid in [
            b"{\"opaque\":true}\n".as_slice(),
            unknown.as_slice(),
            wrong_schema.as_slice(),
            reordered.as_slice(),
        ] {
            std::fs::write(&owner, invalid).unwrap();
            assert!(claim(&root, "p01", "parent", None).is_err());
            assert!(!root.join("_control/personas/p01/lease.json").exists());
        }
        std::fs::write(&owner, &original_owner).unwrap();

        let mut authority = bind_workspace(&root).unwrap();
        let saved = root.parent().unwrap().join("persona-workspace-owner.saved");
        std::fs::rename(&owner, &saved).unwrap();
        std::fs::write(&owner, &original_owner).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&owner, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(authority.recheck().is_err());
        std::fs::remove_file(&owner).unwrap();
        std::fs::rename(&saved, &owner).unwrap();

        let mut authority = bind_workspace(&root).unwrap();
        let parked = root.parent().unwrap().join("workspace-parked");
        std::fs::rename(&root, &parked).unwrap();
        std::os::unix::fs::symlink(&parked, &root).unwrap();
        assert!(authority.recheck().is_err());

        let (_t, root, _scope) = workspace();
        let plan = root.join("persona-plan.json");
        let mut bytes = std::fs::read(&plan).unwrap();
        bytes[0] = b'['; // canonical JSON no longer parses as a plan.
        std::fs::write(&plan, bytes).unwrap();
        assert!(claim(&root, "p01", "parent", None).is_err());
        assert!(!root.join("_control/personas/p01/lease.json").exists());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn rejects_linked_private_files_and_unknown_or_active_scope_state() {
        use std::os::unix::fs::symlink;
        let (_t, root, scope) = workspace();
        let p = root.join("_control/personas/p01");
        symlink("/tmp", p.join("lease.json")).unwrap();
        assert!(show(&root, "p01").is_err());
        std::fs::remove_file(p.join("lease.json")).unwrap();

        let lease = claim(&root, "p01", "parent", None).unwrap();
        std::fs::hard_link(p.join("lease.json"), p.join("lease-copy")).unwrap();
        assert!(release(&root, "p01", &lease.release_token).is_err());
        std::fs::remove_file(p.join("lease-copy")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p.join("lease.json"), std::fs::Permissions::from_mode(0o640))
            .unwrap();
        assert!(show(&root, "p01").is_err());
        std::fs::set_permissions(p.join("lease.json"), std::fs::Permissions::from_mode(0o600))
            .unwrap();
        let child = scope_claim(&root, "p01", &scope, "parent", "worker", None).unwrap();
        assert!(recover(&root, "p01", "parent", "manual").is_err());
        scope_release(&root, "p01", &scope, "parent", &child.release_token).unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn mutation_faults_are_indeterminate_and_ancestor_swap_is_detected() {
        let (_t, root, _scope) = workspace();
        FAULT_AFTER.with(|slot| slot.set(Some("create")));
        assert!(matches!(
            claim(&root, "p01", "parent", None),
            Err(PersonaLeaseError::Indeterminate(_))
        ));
        assert_eq!(show(&root, "p01").unwrap().session, "parent");
        FAULT_AFTER.with(|slot| slot.set(Some("append")));
        assert!(matches!(
            recover(&root, "p01", "parent", "first fault"),
            Err(PersonaLeaseError::Indeterminate(_))
        ));
        assert_eq!(show(&root, "p01").unwrap().session, "parent");
        FAULT_AFTER.with(|slot| slot.set(Some("remove")));
        assert!(matches!(
            recover(&root, "p01", "parent", "second fault"),
            Err(PersonaLeaseError::Indeterminate(_))
        ));
        assert!(show(&root, "p01").is_err());

        let authority = bind_workspace(&root).unwrap();
        let bound = persona_dir(&authority, "p01").unwrap();
        let control = root.join("_control");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&control, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::rename(control.join("personas"), control.join("personas-old")).unwrap();
        std::fs::create_dir(control.join("personas")).unwrap();
        assert!(bound.recheck().is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn lifecycle_does_not_retain_a_descriptor_into_its_workspace() {
        let (_t, root, _scope) = workspace();
        let lease = claim(&root, "p01", "one-lifecycle", None).unwrap();
        release(&root, "p01", &lease.release_token).unwrap();

        let workspace = std::fs::canonicalize(&root).unwrap();
        for entry in std::fs::read_dir("/proc/self/fd").unwrap() {
            let target = std::fs::read_link(entry.unwrap().path());
            if let Ok(target) = target {
                assert!(
                    !target.starts_with(&workspace),
                    "descriptor retained into test workspace: {}",
                    target.display()
                );
            }
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    #[test]
    fn unsupported_is_reported_before_mutation() {
        let root = Path::new("/definitely-not-a-workspace");
        assert!(matches!(
            claim(root, "p01", "s", None),
            Err(PersonaLeaseError::Scaffold(_)) | Err(PersonaLeaseError::Unsupported)
        ));
    }
}
