//! Explicit, journaled registration of an already-existing moved scope.

use kio_core::management::{
    ChildEnrollment, ManagementAuthority, ManagementBinding, ManagementRecord, begin_registration,
    detect_case_insensitive, finish_registration, peek_registration_marker,
    publish_registration_record, read_registration_recovery_record, recorded_child_root,
    validate_prospective_child,
};
use kio_core::scope::{Repository, new_ulid};
use kio_core::store_dir::{Publication, StoreDirectory};
use kio_core::{KioError, Result};
use kio_index::registry::{
    RegistryDb, RegistryEntry, RegistrySnapshotError, ScopeRegistrationRebind,
    default_registry_path,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use crate::commands::RootRegisterArgs;

/// This entrypoint is intentionally separate from init/index: only an explicit
/// root-register request may recover a management record whose path binding no
/// longer matches the live retained root.
const JOURNAL_LEAF: &str = "root-registration.json";
const MAX_JOURNAL_BYTES: u64 = 512 * 1024;
const MAX_NODES: usize = 256;

#[cfg(test)]
mod recovery_tests;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u8,
    operation_id: String,
    target_root: String,
    phase: Phase,
    nodes: Vec<JournalNode>,
}
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Phase {
    Prepared,
    Marked,
    Revoked,
    Published,
    RegistryUpdated,
    Finishing,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalNode {
    relative_path: String,
    before: ManagementRecord,
    after: ManagementRecord,
    old_kio_paths: Vec<String>,
    retired_children: BTreeMap<String, ChildEnrollment>,
}
struct Node {
    relative: PathBuf,
    binding: ManagementBinding,
    before: ManagementRecord,
    after: ManagementRecord,
    old_kio_paths: Vec<String>,
    retired_children: BTreeMap<String, ChildEnrollment>,
}

pub(crate) fn run(args: RootRegisterArgs) -> Result<Value> {
    if args.preview == args.yes || (args.resume.is_some() && !args.yes) {
        return Err(KioError::invalid_usage(
            "root register requires exactly one of --preview or --yes; --resume requires --yes",
        ));
    }
    let context = crate::context::working_directory()?;
    let path = match args.path {
        Some(path) if path.is_absolute() => path,
        Some(path) => context.join(path),
        None => context,
    };
    let canonical = path
        .canonicalize()
        .map_err(|e| KioError::io(e.to_string(), path.display().to_string()))?;
    if !canonical.is_dir() {
        return Err(KioError::invalid_usage(
            "root register path must be a directory",
        ));
    }
    let binding = ManagementBinding::bind(&canonical)?;
    let control = control_for(&binding)?;
    let pending = control.read_optional(Path::new(JOURNAL_LEAF), MAX_JOURNAL_BYTES)?;
    if args.preview {
        if let Some(bytes) = pending {
            let journal = decode(&bytes, &canonical)?;
            return Err(KioError::new(
                "KIO-E-REGISTRATION-PENDING-001",
                format!(
                    "root registration is pending; run root register --yes --resume {}",
                    journal.operation_id
                ),
                json!({"path":canonical,"operation_id":journal.operation_id}),
                kio_core::ExitCode::Failure,
            ));
        }
        return Ok(preview(&plan_new(&canonical, binding.clone())?, None));
    }
    if !args.yes {
        return Err(KioError::invalid_usage(
            "root register requires --yes or --preview",
        ));
    }
    let (mut journal, bytes, persisted) = match pending {
        Some(bytes) => {
            let j = decode(&bytes, &canonical)?;
            if args.resume.as_deref() != Some(j.operation_id.as_str()) {
                return Err(KioError::invalid_usage(
                    "resume operation ID does not match root registration journal",
                ));
            }
            (j, bytes, true)
        }
        None => {
            if args.resume.is_some() {
                return Err(KioError::invalid_usage(
                    "no root registration journal matches --resume",
                ));
            }
            let nodes = plan_new(&canonical, binding.clone())?;
            let j = Journal {
                version: 1,
                operation_id: new_ulid(&canonical),
                target_root: canonical.display().to_string(),
                phase: Phase::Prepared,
                nodes: nodes.iter().map(node_to_journal).collect(),
            };
            let bytes = encode(&j)?;
            (j, bytes, false)
        }
    };
    execute(&binding, &mut journal, bytes, persisted).map_err(|error| {
        let mut context = error.context().clone();
        if let Some(context) = context.as_object_mut() {
            context.insert("operation_id".into(), json!(journal.operation_id));
            context.insert("registration_path".into(), json!(canonical));
        }
        KioError::new(
            error.error_code(),
            error.message(),
            context,
            error.exit_code(),
        )
    })
}

fn plan_new(root: &Path, binding: ManagementBinding) -> Result<Vec<Node>> {
    let mut nodes = Vec::new();
    collect(root, PathBuf::new(), binding, &mut nodes)?;
    let mut scope_ids = std::collections::BTreeSet::new();
    let mut identities = std::collections::BTreeSet::new();
    for node in &nodes {
        if !scope_ids.insert(node.before.scope_id.clone())
            || !identities.insert(
                serde_json::to_string(node.binding.directory_identity())
                    .map_err(|_| KioError::invalid_usage("cannot encode directory identity"))?,
            )
        {
            return Err(KioError::invalid_usage(
                "root registration subtree repeats a scope or directory identity",
            ));
        }
    }
    let root_scope = nodes[0].before.scope_id.clone();
    let detach = !matches!(nodes[0].before.authority, ManagementAuthority::Root);
    for (i, n) in nodes.iter_mut().enumerate() {
        n.after.canonical_root = n.binding.canonical_root().to_path_buf();
        n.after.directory_identity = n.binding.directory_identity().clone();
        n.after.registration_generation = n
            .before
            .registration_generation
            .checked_add(1)
            .ok_or_else(|| KioError::invalid_usage("registration generation overflow"))?;
        if i == 0 && detach {
            n.after.authority = ManagementAuthority::Root;
        }
        if i > 0
            && let ManagementAuthority::Child { root_scope_id, .. } = &mut n.after.authority
        {
            *root_scope_id = root_scope.clone();
        }
    }
    let current_children = nodes
        .iter()
        .skip(1)
        .map(|node| {
            (
                node.relative.clone(),
                node.binding.directory_identity().clone(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    for node in &mut nodes {
        for (name, enrollment) in &mut node.after.children {
            enrollment.directory_identity = current_children
                .get(&node.relative.join(name))
                .ok_or_else(|| {
                    KioError::invalid_usage("root registration child identity is absent")
                })?
                .clone();
        }
    }
    // The registry is a derived cache. Planning must neither create it nor
    // require it to exist when importing a moved tree on a new device.
    let db = match RegistryDb::open_read_only(default_registry_path().map_err(crate::index_to_kio)?)
    {
        Ok(db) => Some(db),
        Err(RegistrySnapshotError::Missing) => None,
        Err(error) => return Err(KioError::invalid_usage(error.to_string())),
    };
    for n in &mut nodes {
        let entries = match &db {
            Some(db) => db
                .lookup_scope_id(&n.before.scope_id)
                .map_err(crate::index_to_kio)?,
            None => Vec::new(),
        };
        let destination = n
            .binding
            .canonical_root()
            .join(".kio")
            .display()
            .to_string();
        for e in &entries {
            if e.kio_path != destination {
                probe_live(e, &n.before.scope_id)?;
            }
        }
        n.old_kio_paths = entries.into_iter().map(|e| e.kio_path).collect();
    }
    recheck_old_paths(&nodes)?;
    Ok(nodes)
}
fn collect(
    root: &Path,
    relative: PathBuf,
    binding: ManagementBinding,
    out: &mut Vec<Node>,
) -> Result<()> {
    if out.len() >= MAX_NODES || relative.components().count() > 64 {
        return Err(KioError::invalid_usage(
            "root registration subtree exceeds bounds",
        ));
    }
    let before = read_registration_recovery_record(&binding)?;
    ensure_no_child_lifecycle(&binding)?;
    let children = before.children.clone();
    let parent_scope = before.scope_id.clone();
    let original_root_scope = match &before.authority {
        ManagementAuthority::Root => before.scope_id.clone(),
        ManagementAuthority::Child { root_scope_id, .. } => root_scope_id.clone(),
    };
    let parent_index = out.len();
    out.push(Node {
        relative: relative.clone(),
        binding,
        before: before.clone(),
        after: before.clone(),
        old_kio_paths: Vec::new(),
        retired_children: BTreeMap::new(),
    });
    for (name, grant) in children {
        if name.is_empty() || name.contains('/') || name.contains('\\') {
            return Err(KioError::invalid_usage("managed child name is invalid"));
        }
        let child_relative = relative.join(&name);
        if !root_directory(&out[parent_index].binding)?.contains_entry(Path::new(&name))? {
            out[parent_index].after.children.remove(&name);
            out[parent_index].retired_children.insert(name, grant);
            continue;
        }
        let child = ManagementBinding::bind(root.join(&child_relative))?;
        let record = read_registration_recovery_record(&child)?;
        validate_prospective_child(
            &out[parent_index].binding,
            child.root_handle(),
            child.canonical_root(),
        )?;
        match &record.authority {
            ManagementAuthority::Child {
                parent_scope_id,
                root_scope_id,
                enrollment_token,
                ..
            } if parent_scope_id == &parent_scope
                && record.scope_id == grant.scope_id
                && enrollment_token == &grant.enrollment_token
                && record.directory_identity == grant.directory_identity
                && root_scope_id == &original_root_scope
                && record.canonical_root == recorded_child_root(&before, &name)? => {}
            _ => {
                return Err(KioError::invalid_usage(
                    "managed subtree has a non-reciprocal child or independent scope",
                ));
            }
        }
        collect(root, child_relative, child, out)?;
    }
    Ok(())
}
fn probe_live(entry: &RegistryEntry, id: &str) -> Result<()> {
    let p = PathBuf::from(&entry.root_path);
    if !p.is_absolute() || Path::new(&entry.kio_path) != p.join(".kio") {
        return Err(KioError::invalid_usage(
            "root registration registry path is not canonical",
        ));
    }
    let p = match p.canonicalize() {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(KioError::io(e.to_string(), p.display().to_string())),
    };
    match p.join(".kio").canonicalize() {
        Ok(kio) if kio == p.join(".kio") => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Ok(_) => {
            return Err(KioError::invalid_usage(
                "old registered .kio path is not canonical",
            ));
        }
        Err(e) => {
            return Err(KioError::io(
                e.to_string(),
                p.join(".kio").display().to_string(),
            ));
        }
    }
    if read_registration_recovery_record(&ManagementBinding::bind(&p)?)?.scope_id == id {
        return Err(KioError::invalid_usage(
            "another live registered path has this scope ID",
        ));
    }
    Ok(())
}
fn execute(
    retained_root: &ManagementBinding,
    journal: &mut Journal,
    mut bytes: Vec<u8>,
    persisted: bool,
) -> Result<Value> {
    retained_root.revalidate()?;
    let root = retained_root.canonical_root();
    let control = control_for(retained_root)?;
    let mut nodes = journal
        .nodes
        .iter()
        .map(|j| node_from_journal(root, j))
        .collect::<Result<Vec<_>>>()?;
    // Keep the root used for the initial journal read, locks, and every journal
    // mutation identical even if an attacker replaces the named directory.
    nodes[0].binding = retained_root.clone();
    let repos = nodes.iter().map(repo).collect::<Result<Vec<_>>>()?;
    let _locks = repos
        .iter()
        .map(Repository::lock_store)
        .collect::<Result<Vec<_>>>()?;
    retained_root.revalidate()?;
    let actual_journal = control.read_optional(Path::new(JOURNAL_LEAF), MAX_JOURNAL_BYTES)?;
    if (persisted && actual_journal.as_deref() != Some(bytes.as_slice()))
        || (!persisted && actual_journal.is_some())
    {
        return Err(KioError::invalid_usage(
            "root registration journal changed before locking",
        ));
    }
    for n in &nodes {
        n.binding.revalidate()?;
        ensure_no_child_lifecycle(&n.binding)?;
        let current = read_registration_recovery_record(&n.binding)?;
        if current != n.before && current != n.after {
            return Err(KioError::invalid_usage(
                "root registration record changed outside its journal",
            ));
        }
    }
    if !persisted {
        for (index, node) in nodes.iter_mut().enumerate() {
            let observed = detect_case_insensitive(&node.binding)?;
            node.after.case_insensitive = observed;
            journal.nodes[index].after.case_insensitive = observed;
        }
        bytes = encode(journal)?;
    } else {
        for node in &nodes {
            if detect_case_insensitive(&node.binding)? != node.after.case_insensitive {
                return Err(KioError::invalid_usage(
                    "root registration filesystem case behavior changed since the journal was created",
                ));
            }
        }
    }
    validate_journal_plan(&nodes)?;
    recheck_retired_children(&nodes)?;
    recheck_old_paths(&nodes)?;
    if !persisted {
        nodes[0].binding.revalidate()?;
        control.write_atomic(Path::new(JOURNAL_LEAF), &bytes, Publication::CreateOnly)?;
    }
    if persisted && journal.phase != Phase::Prepared {
        verify_resume_markers(&nodes, &journal.operation_id, journal.phase)?;
    }
    if journal.phase == Phase::Prepared {
        for n in &nodes {
            begin_registration(&n.binding, &journal.operation_id)?;
        }
        bytes = advance(&control, &nodes[0].binding, journal, Phase::Marked)?;
    }
    if journal.phase == Phase::Marked {
        revoke_grants(&nodes)?;
        bytes = advance(&control, &nodes[0].binding, journal, Phase::Revoked)?;
    } else if journal.phase != Phase::Prepared {
        revoke_grants(&nodes)?;
    }
    if journal.phase == Phase::Revoked {
        for n in &nodes {
            recheck_retired_children(std::slice::from_ref(n))?;
            publish_registration_record(&n.binding, &journal.operation_id, &n.before, &n.after)?;
        }
        bytes = advance(&control, &nodes[0].binding, journal, Phase::Published)?;
    }
    if journal.phase == Phase::Published {
        recheck_old_paths(&nodes)?;
        let mut db = RegistryDb::open_default().map_err(crate::index_to_kio)?;
        let rs = nodes
            .iter()
            .map(|n| ScopeRegistrationRebind {
                scope_id: n.before.scope_id.clone(),
                old_kio_paths: n.old_kio_paths.clone(),
                new_registration: RegistryEntry {
                    scope_id: n.before.scope_id.clone(),
                    kio_path: n
                        .binding
                        .canonical_root()
                        .join(".kio")
                        .display()
                        .to_string(),
                    root_path: n.binding.canonical_root().display().to_string(),
                    participates_in_global_search: false,
                    indexed: false,
                    last_seen_at: crate::now_utc_seconds(),
                },
            })
            .collect::<Vec<_>>();
        db.rebind_scope_registrations(&rs)
            .map_err(crate::index_to_kio)?;
        for node in &nodes {
            // Foreign persisted paths cannot address this device's cache.
            if node.before.directory_identity.is_native_platform() {
                for (name, enrollment) in &node.retired_children {
                    db.remove(
                        &enrollment.scope_id,
                        &recorded_child_root(&node.before, name)?
                            .join(".kio")
                            .display()
                            .to_string(),
                    )
                    .map_err(crate::index_to_kio)?;
                }
            }
        }
        bytes = advance(&control, &nodes[0].binding, journal, Phase::RegistryUpdated)?;
    }
    if journal.phase == Phase::RegistryUpdated {
        bytes = advance(&control, &nodes[0].binding, journal, Phase::Finishing)?;
    }
    if journal.phase == Phase::Finishing {
        for n in nodes.iter().skip(1).rev() {
            finish(n, &journal.operation_id)?;
        }
        finish(&nodes[0], &journal.operation_id)?;
        nodes[0].binding.revalidate()?;
        control.quarantine_then_remove(Path::new(JOURNAL_LEAF), &bytes, MAX_JOURNAL_BYTES)?;
    }
    Ok(
        json!({"status":"registered","operation_id":journal.operation_id,"scope_id":nodes[0].before.scope_id,"path":root,"registration_generation":nodes[0].after.registration_generation,"scopes_registered":nodes.len(),"child_memberships_retired":nodes.iter().map(|node| node.retired_children.len()).sum::<usize>(),"grants_revoked":true}),
    )
}
fn validate_journal_plan(nodes: &[Node]) -> Result<()> {
    validate_topology(&nodes.iter().map(node_to_journal).collect::<Vec<_>>())?;
    let mut identities = std::collections::BTreeSet::new();
    let root_scope = &nodes[0].before.scope_id;
    let detaches = !matches!(nodes[0].before.authority, ManagementAuthority::Root);
    for (index, node) in nodes.iter().enumerate() {
        if !identities.insert(
            serde_json::to_string(node.binding.directory_identity())
                .map_err(|_| KioError::invalid_usage("cannot encode directory identity"))?,
        ) {
            return Err(KioError::invalid_usage(
                "root registration repeats a directory identity",
            ));
        }
        if index > 0 {
            let parent = nodes
                .iter()
                .find(|parent| Some(parent.relative.as_path()) == node.relative.parent())
                .ok_or_else(|| KioError::invalid_usage("root registration parent is absent"))?;
            validate_prospective_child(
                &parent.binding,
                node.binding.root_handle(),
                node.binding.canonical_root(),
            )?;
        }
        let mut expected = node.before.clone();
        expected.canonical_root = node.binding.canonical_root().to_path_buf();
        expected.directory_identity = node.binding.directory_identity().clone();
        // Case behavior is measured only after every retained lock is held and
        // is then pinned in the journal.  The caller has just compared this
        // value with a fresh retained probe.
        expected.case_insensitive = node.after.case_insensitive;
        expected.registration_generation = expected
            .registration_generation
            .checked_add(1)
            .ok_or_else(|| KioError::invalid_usage("registration generation overflow"))?;
        for name in node.retired_children.keys() {
            expected.children.remove(name);
        }
        for (name, enrollment) in &mut expected.children {
            let child = nodes
                .iter()
                .find(|child| child.relative == node.relative.join(name))
                .ok_or_else(|| KioError::invalid_usage("root registration child is absent"))?;
            enrollment.directory_identity = child.binding.directory_identity().clone();
        }
        if index == 0 && detaches {
            expected.authority = ManagementAuthority::Root;
        }
        if index > 0
            && let ManagementAuthority::Child { root_scope_id, .. } = &mut expected.authority
        {
            *root_scope_id = root_scope.clone();
        }
        if node.after != expected {
            return Err(KioError::invalid_usage(
                "root registration journal after record is not the allowed transition",
            ));
        }
    }
    Ok(())
}
fn revoke_grants(nodes: &[Node]) -> Result<()> {
    if crate::grants::PrivateGrantStore::open_readonly(&crate::approvals::store_path())?.is_some() {
        let mut grants =
            crate::grants::PrivateGrantStore::open_or_create(&crate::approvals::store_path())?;
        for node in nodes {
            grants.revoke(&node.before.scope_id, None, crate::now_utc_seconds())?;
            for (name, enrollment) in &node.retired_children {
                grants.revoke_scope_instance(
                    &enrollment.scope_id,
                    &recorded_child_root(&node.before, name)?,
                    &enrollment.directory_identity,
                    crate::now_utc_seconds(),
                )?;
            }
        }
    }
    Ok(())
}
fn recheck_retired_children(nodes: &[Node]) -> Result<()> {
    for node in nodes {
        node.binding.revalidate()?;
        let parent = root_directory(&node.binding)?;
        for name in node.retired_children.keys() {
            if parent.contains_entry(Path::new(name))? {
                return Err(KioError::invalid_usage(
                    "root registration retired child is no longer absent",
                ));
            }
        }
        node.binding.revalidate()?;
    }
    Ok(())
}
fn verify_resume_markers(nodes: &[Node], operation_id: &str, phase: Phase) -> Result<()> {
    for node in nodes {
        match peek_registration_marker(&node.binding)?.as_deref() {
            Some(id) if id == operation_id => {}
            None if phase == Phase::Finishing
                && read_registration_recovery_record(&node.binding)? == node.after => {}
            _ => {
                return Err(KioError::invalid_usage(
                    "root registration marker does not match resume journal",
                ));
            }
        }
    }
    Ok(())
}
fn recheck_old_paths(nodes: &[Node]) -> Result<()> {
    for node in nodes {
        // An enrolled child may never have had its own registry row. Its
        // authoritative former path must also be checked independently.
        if node.before.directory_identity.is_native_platform()
            && node.before.canonical_root != node.binding.canonical_root()
        {
            probe_live(
                &RegistryEntry {
                    scope_id: node.before.scope_id.clone(),
                    kio_path: node
                        .before
                        .canonical_root
                        .join(".kio")
                        .display()
                        .to_string(),
                    root_path: node.before.canonical_root.display().to_string(),
                    participates_in_global_search: false,
                    indexed: false,
                    last_seen_at: String::new(),
                },
                &node.before.scope_id,
            )?;
        }
        let destination = node
            .binding
            .canonical_root()
            .join(".kio")
            .display()
            .to_string();
        for kio_path in &node.old_kio_paths {
            if kio_path == &destination {
                continue;
            }
            let kio = PathBuf::from(kio_path);
            let root = kio
                .parent()
                .ok_or_else(|| {
                    KioError::invalid_usage("root registration registry path has no parent")
                })?
                .to_path_buf();
            probe_live(
                &RegistryEntry {
                    scope_id: node.before.scope_id.clone(),
                    kio_path: kio_path.clone(),
                    root_path: root.display().to_string(),
                    participates_in_global_search: false,
                    indexed: false,
                    last_seen_at: String::new(),
                },
                &node.before.scope_id,
            )?;
        }
    }
    Ok(())
}
fn finish(n: &Node, op: &str) -> Result<()> {
    match peek_registration_marker(&n.binding)?.as_deref() {
        Some(id) if id == op => finish_registration(&n.binding, op, &n.after),
        None if read_registration_recovery_record(&n.binding)? == n.after => Ok(()),
        _ => Err(KioError::invalid_usage(
            "root registration marker does not match journal",
        )),
    }
}
fn node_to_journal(n: &Node) -> JournalNode {
    JournalNode {
        relative_path: n.relative.display().to_string(),
        before: n.before.clone(),
        after: n.after.clone(),
        old_kio_paths: n.old_kio_paths.clone(),
        retired_children: n.retired_children.clone(),
    }
}
fn node_from_journal(root: &Path, j: &JournalNode) -> Result<Node> {
    let relative = if j.relative_path.is_empty() {
        PathBuf::new()
    } else {
        PathBuf::from(&j.relative_path)
    };
    if relative.is_absolute()
        || relative.components().count() > 64
        || relative
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err(KioError::invalid_usage(
            "root registration journal has invalid relative path",
        ));
    }
    Ok(Node {
        relative: relative.clone(),
        binding: ManagementBinding::bind(root.join(relative))?,
        before: j.before.clone(),
        after: j.after.clone(),
        old_kio_paths: j.old_kio_paths.clone(),
        retired_children: j.retired_children.clone(),
    })
}
fn repo(n: &Node) -> Result<Repository> {
    Repository::open_bound_for_recovery(
        n.binding.canonical_root().to_path_buf(),
        n.binding.root_handle().try_clone().map_err(|e| {
            KioError::io(
                e.to_string(),
                n.binding.canonical_root().display().to_string(),
            )
        })?,
        n.binding.kio_handle().try_clone().map_err(|e| {
            KioError::io(
                e.to_string(),
                n.binding.canonical_root().display().to_string(),
            )
        })?,
    )
}
fn control_for(b: &ManagementBinding) -> Result<StoreDirectory> {
    StoreDirectory::from_retained(
        b.kio_handle()
            .try_clone()
            .map_err(|e| KioError::io(e.to_string(), b.canonical_root().display().to_string()))?,
        b.canonical_root().join(".kio"),
    )
}
fn root_directory(binding: &ManagementBinding) -> Result<StoreDirectory> {
    StoreDirectory::from_retained(
        binding.root_handle().try_clone().map_err(|error| {
            KioError::io(
                error.to_string(),
                binding.canonical_root().display().to_string(),
            )
        })?,
        binding.canonical_root().to_path_buf(),
    )
}
fn ensure_no_child_lifecycle(binding: &ManagementBinding) -> Result<()> {
    let control = control_for(binding)?;
    if control.inspect_atomic()? == kio_core::store_dir::AtomicWorkspaceState::Pending {
        return Err(KioError::invalid_usage(
            "atomic storage recovery must finish before moving or registering this scope",
        ));
    }
    for journal in [
        ".root-init-journal.json",
        "child-initialization.json",
        "child-initialization-cancel.json",
        "child-retirement.json",
        kio_core::management::CHILD_INITIALIZATION_PENDING_LEAF,
    ] {
        if control.contains_entry(Path::new(journal))? {
            return Err(KioError::invalid_usage(
                "child lifecycle recovery must finish before moving or registering this scope",
            ));
        }
    }
    if let Some(entries) = control.entries_optional(Path::new("child-initializations"))? {
        let operations = StoreDirectory::from_retained(
            control.open_directory(Path::new("child-initializations"))?,
            control.path().join("child-initializations"),
        )?;
        if operations.inspect_atomic()? == kio_core::store_dir::AtomicWorkspaceState::Pending
            || entries
                .iter()
                .any(|entry| entry.name != kio_core::store_dir::ATOMIC_WORKSPACE_DIR)
        {
            return Err(KioError::invalid_usage(
                "child initializations must finish before moving or registering this scope",
            ));
        }
    }
    Ok(())
}
fn encode(j: &Journal) -> Result<Vec<u8>> {
    let b = serde_json::to_vec(j)
        .map_err(|_| KioError::invalid_usage("cannot serialize root registration journal"))?;
    if b.len() as u64 > MAX_JOURNAL_BYTES {
        return Err(KioError::invalid_usage(
            "root registration journal exceeds size limit",
        ));
    }
    Ok(b)
}
fn decode(b: &[u8], root: &Path) -> Result<Journal> {
    let j: Journal = serde_json::from_slice(b)
        .map_err(|_| KioError::invalid_usage("root registration journal is corrupt"))?;
    if j.version != 1
        || j.target_root != root.display().to_string()
        || j.nodes.is_empty()
        || j.nodes.len() > MAX_NODES
        || j.operation_id.is_empty()
    {
        return Err(KioError::invalid_usage(
            "root registration journal is invalid",
        ));
    }
    if encode(&j)? != b {
        return Err(KioError::invalid_usage(
            "root registration journal is not canonical bytes",
        ));
    }
    for n in &j.nodes {
        if n.old_kio_paths.len() > MAX_NODES {
            return Err(KioError::invalid_usage(
                "root registration journal has too many registry paths",
            ));
        }
        let mut unique_paths = std::collections::BTreeSet::new();
        for path in &n.old_kio_paths {
            let path = PathBuf::from(path);
            if !path.is_absolute()
                || path.file_name().is_none_or(|name| name != ".kio")
                || !unique_paths.insert(path)
            {
                return Err(KioError::invalid_usage(
                    "root registration journal has invalid registry paths",
                ));
            }
        }
        if n.after.registration_generation
            != n.before
                .registration_generation
                .checked_add(1)
                .ok_or_else(|| KioError::invalid_usage("registration generation overflow"))?
        {
            return Err(KioError::invalid_usage(
                "root registration journal has invalid generation",
            ));
        }
    }
    validate_topology(&j.nodes)?;
    Ok(j)
}

fn validate_topology(nodes: &[JournalNode]) -> Result<()> {
    if nodes.is_empty() || nodes.len() > MAX_NODES || !nodes[0].relative_path.is_empty() {
        return Err(KioError::invalid_usage(
            "root registration journal does not begin at its retained root",
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut scope_ids = std::collections::BTreeSet::new();
    for (index, node) in nodes.iter().enumerate() {
        let relative = if node.relative_path.is_empty() {
            PathBuf::new()
        } else {
            PathBuf::from(&node.relative_path)
        };
        if relative.is_absolute()
            || relative.components().count() > 64
            || relative
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
            || !seen.insert(relative.clone())
            || !scope_ids.insert(node.before.scope_id.clone())
            || relative.components().collect::<PathBuf>().as_os_str() != node.relative_path.as_str()
        {
            return Err(KioError::invalid_usage(
                "root registration journal has invalid relative path",
            ));
        }
        if index > 0 {
            let parent = relative.parent().ok_or_else(|| {
                KioError::invalid_usage("root registration journal child has no parent")
            })?;
            let parent_index = nodes
                .iter()
                .position(|candidate| Path::new(&candidate.relative_path) == parent)
                .ok_or_else(|| {
                    KioError::invalid_usage("root registration journal child parent is absent")
                })?;
            if parent_index >= index {
                return Err(KioError::invalid_usage(
                    "root registration parent must precede its child",
                ));
            }
            let name = relative
                .file_name()
                .and_then(|v| v.to_str())
                .ok_or_else(|| {
                    KioError::invalid_usage("root registration journal child name is invalid")
                })?;
            let enrollment = nodes[parent_index]
                .before
                .children
                .get(name)
                .ok_or_else(|| {
                    KioError::invalid_usage("root registration journal child is not enrolled")
                })?;
            let expected_root = match &nodes[parent_index].before.authority {
                ManagementAuthority::Root => &nodes[parent_index].before.scope_id,
                ManagementAuthority::Child { root_scope_id, .. } => root_scope_id,
            };
            match &node.before.authority {
                ManagementAuthority::Child {
                    parent_scope_id,
                    root_scope_id,
                    enrollment_token,
                    ..
                } if parent_scope_id == &nodes[parent_index].before.scope_id
                    && root_scope_id == expected_root
                    && enrollment.scope_id == node.before.scope_id
                    && enrollment.enrollment_token.as_str() == enrollment_token.as_str()
                    && enrollment.directory_identity == node.before.directory_identity
                    && node.before.canonical_root
                        == recorded_child_root(&nodes[parent_index].before, name)? => {}
                _ => {
                    return Err(KioError::invalid_usage(
                        "root registration journal child is non-reciprocal",
                    ));
                }
            }
        }
        // The records are checked against the live before/after record while
        // all scope locks are held. Require every enrolled child here so a
        // canonical but incomplete journal cannot omit its grant revocation.
        for (name, grant) in &node.retired_children {
            if node.before.children.get(name) != Some(grant)
                || node.after.children.contains_key(name)
                || nodes
                    .iter()
                    .any(|child| Path::new(&child.relative_path) == relative.join(name))
                || !scope_ids.insert(grant.scope_id.clone())
            {
                return Err(KioError::invalid_usage(
                    "root registration journal has an invalid child retirement",
                ));
            }
        }
        for name in node.before.children.keys() {
            if !node.retired_children.contains_key(name)
                && !nodes
                    .iter()
                    .any(|child| Path::new(&child.relative_path) == relative.join(name))
            {
                return Err(KioError::invalid_usage(
                    "root registration journal omits an enrolled child",
                ));
            }
        }
    }
    Ok(())
}
fn advance(
    c: &StoreDirectory,
    root: &ManagementBinding,
    j: &mut Journal,
    p: Phase,
) -> Result<Vec<u8>> {
    root.revalidate()?;
    j.phase = p;
    let b = encode(j)?;
    c.write_atomic(Path::new(JOURNAL_LEAF), &b, Publication::Replace)?;
    Ok(b)
}
fn preview(nodes: &[Node], op: Option<&str>) -> Value {
    let n = &nodes[0];
    let retired = nodes
        .iter()
        .flat_map(|node| {
            node.retired_children.iter().map(|(name, enrollment)| {
                json!({"path": node.relative.join(name), "scope_id": enrollment.scope_id,
                "reason": "directory_absent"})
            })
        })
        .collect::<Vec<_>>();
    json!({"operation":"root_register","operation_id":op,"path":n.binding.canonical_root(),"scope_id":n.before.scope_id,"preview":true,"requires_yes":true,"detaches_from_former_parent":!matches!(n.before.authority,ManagementAuthority::Root),"scopes_registered":nodes.len(),"child_memberships_retired":retired.len(),"retired_children":retired,"effects":["rebind management path and directory identity for enrolled subtree","revoke device grants","refresh registry registration as unindexed","retire memberships of absent children","detachment preserves descendant parent IDs and enrollment tokens but changes their root scope"]})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::management::{
        ExplicitRoot, binding_for_repo, initialize_explicit_root, reconcile_planned_child,
    };
    use kio_pipeline::scan::BoundPlannedChild;

    fn child(path: &Path) -> BoundPlannedChild {
        let canonical_root = path.canonicalize().unwrap();
        let root = StoreDirectory::open(&canonical_root)
            .unwrap()
            .root_handle()
            .as_ref()
            .try_clone()
            .unwrap();
        BoundPlannedChild {
            canonical_root,
            root,
            inherited_rules: Vec::new(),
        }
    }

    fn initialized_root(path: &Path) -> Repository {
        match initialize_explicit_root(path).unwrap() {
            ExplicitRoot::Created(repo) | ExplicitRoot::Existing(repo) => repo,
        }
    }

    #[test]
    fn collects_an_enrolled_subtree_without_reopening_unrelated_scopes() {
        let directory = tempfile::tempdir().unwrap();
        let child_path = directory.path().join("child");
        std::fs::create_dir(&child_path).unwrap();
        let root = initialized_root(directory.path());
        let child_scope = reconcile_planned_child(&root, child(&child_path)).unwrap();
        let crate::management::ChildScope::Managed { repo: child_repo } = child_scope else {
            panic!("child was not enrolled")
        };

        let mut nodes = Vec::new();
        collect(
            root.canonical_root(),
            PathBuf::new(),
            binding_for_repo(&root).unwrap(),
            &mut nodes,
        )
        .unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(
            nodes[0].before.scope_id,
            root.scope_identity().unwrap().scope_id
        );
        assert_eq!(
            nodes[1].before.scope_id,
            child_repo.scope_identity().unwrap().scope_id
        );
        assert_eq!(nodes[1].relative, PathBuf::from("child"));
    }

    #[test]
    fn journal_refuses_tampered_root_and_generation() {
        let directory = tempfile::tempdir().unwrap();
        let root = initialized_root(directory.path());
        let binding = binding_for_repo(&root).unwrap();
        let before = read_registration_recovery_record(&binding).unwrap();
        let mut after = before.clone();
        after.registration_generation += 1;
        let canonical = root.canonical_root().to_path_buf();
        let journal = Journal {
            version: 1,
            operation_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned(),
            target_root: canonical.display().to_string(),
            phase: Phase::Prepared,
            nodes: vec![JournalNode {
                relative_path: String::new(),
                before: before.clone(),
                after: after.clone(),
                old_kio_paths: Vec::new(),
                retired_children: BTreeMap::new(),
            }],
        };
        assert!(decode(&encode(&journal).unwrap(), &canonical).is_ok());

        let mut wrong_root = journal.clone();
        wrong_root.nodes[0].relative_path = "elsewhere".to_owned();
        assert!(decode(&encode(&wrong_root).unwrap(), &canonical).is_err());

        let mut wrong_generation = journal;
        wrong_generation.nodes[0].after.registration_generation += 1;
        assert!(decode(&encode(&wrong_generation).unwrap(), &canonical).is_err());
    }

    #[test]
    fn registration_refuses_an_unfinished_child_publication_without_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let root = initialized_root(directory.path());
        let binding = binding_for_repo(&root).unwrap();
        let control = control_for(&binding).unwrap();
        let marker = kio_core::management::CHILD_INITIALIZATION_PENDING_LEAF;
        control
            .write_atomic(Path::new(marker), b"pending", Publication::CreateOnly)
            .unwrap();
        let before = read_registration_recovery_record(&binding).unwrap();
        assert!(plan_new(root.canonical_root(), binding.clone()).is_err());
        assert_eq!(read_registration_recovery_record(&binding).unwrap(), before);
        assert_eq!(
            control.read_optional(Path::new(marker), 64).unwrap(),
            Some(b"pending".to_vec())
        );
        assert!(!control.contains_entry(Path::new(JOURNAL_LEAF)).unwrap());
    }
}
