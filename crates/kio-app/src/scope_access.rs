use crate::ExecScope;
use crate::ParsedSearch;
use crate::ScopeTarget;
use crate::SearchedScopeInfo;
use crate::check_index_generation_current;
use crate::context;
use crate::context::EvidenceRegistry;
use crate::diagnostic_safe_text;
use crate::index_to_kio;
use crate::invocation::Invocation;
use crate::is_ancestor_or_equal;
use crate::read_tombstone;
use kio_core::ExitCode;
use kio_core::KioError;
use kio_core::Result;
use kio_core::control::ControlHandle;
use kio_core::dag::CommitType;
use kio_core::purge::EventKind;
use kio_core::purge::PurgeState;
use kio_core::purge::canonical_final_event;
use kio_core::scope::Repository;
use kio_index::registry::RegistryDb;
use kio_index::registry::RegistryEntry;
use kio_index::registry::RegistrySnapshotError;
use kio_pipeline::ledger::TaskKey as LedgerTaskKey;
use kio_search::evidence::EvidencePointer;
use kio_search::query::ScopeSelectionMode;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;

/// Mutation-side purge gate. U19/LC22: a public tombstone (active or
/// retired) no longer permanently rejects identical-byte re-ingest — that
/// block is reversed into the resurrection flow (re-publication is allowed;
/// the same locked mutation that republishes the raw retires the marker,
/// `Repository::snapshot_with_type`'s resurrection scan). Only an active
/// post-`prepared` journal barrier for this exact raw_hash (an in-progress,
/// not-yet-complete purge) still gates ingest here — orthogonal to, and kept
/// unchanged by, the resurrection reversal. Fsck-only erase receipts likewise
/// never block explicit ingest.
pub(crate) fn ensure_raw_ingest_allowed(repo: &Repository, raw_hash: &str) -> Result<()> {
    let purge = PurgeState::open_with_control(repo.kio_dir(), repo.control().clone())?;
    if purge.barrier_blocks(raw_hash)? {
        return Err(KioError::new(
            "KIO-E-PURGE-INCOMPLETE-001",
            "raw ingest is blocked while purge remains incomplete",
            json!({ "component": "raw_ingest" }),
            ExitCode::PartialFailure,
        ));
    }
    Ok(())
}

pub(crate) fn raw_ingest_is_purge_blocked(repo: &Repository, raw_hash: &str) -> bool {
    ensure_raw_ingest_allowed(repo, raw_hash).is_err()
}

/// Rebuilders hold the scope store lock, so an already-visible purge journal
/// cannot make forward progress until they return. Refuse the rebuild before any
/// derived write instead of temporarily resurrecting its blocked rows.
pub(crate) fn ensure_no_visible_purge_journal(
    kio_dir: &Path,
    control: &ControlHandle,
) -> Result<()> {
    if PurgeState::open_with_control(kio_dir, control.clone())?
        .read_journal()?
        .is_some_and(|journal| journal.phase.is_barrier_visible())
    {
        return Err(KioError::new(
            "KIO-E-PURGE-INCOMPLETE-001",
            "index rebuild is blocked while purge remains incomplete",
            json!({ "component": "index_rebuild" }),
            ExitCode::PartialFailure,
        ));
    }
    Ok(())
}

/// The active visibility barrier and canonical marker state are both
/// authoritative over append-only history/chunk ledgers. Derived rebuilds
/// must omit a raw_hash whose canonical final event (LC8-14/PB64-68's
/// cross-marker aggregation — `kio_core::purge::canonical_final_event`) is
/// `purged`/`erased`.
///
/// Item 2 fix (surfaced by PB46/PB64's evidence-verify ancestry checks,
/// which are the first consumer of `chunk_publications`/
/// `chunk_config_generations` sensitive to this): this used to short-circuit
/// on `read_tombstone(...).is_some()` — true whenever a tombstone RECORD
/// exists at all, regardless of its own tail or a co-existing erase
/// receipt's higher-`lifecycle_epoch` `retired` tail. A resurrected raw_hash
/// (canonical = `retired`) therefore stayed excluded from every rebuild
/// forever after its first purge, even though `08 §3.1`'s resolvers treat it
/// as alive — `kio repair rebuild-db` after a resurrection silently
/// dropped its chunk from `chunk_publications`/`tree_entries`/FTS, which
/// then made a freshly-alive pointer's own v2/v3 ancestry check (this
/// session's new consumer) resolve `not_found` against a rebuilt index, even
/// though the un-rebuilt index (and the raw object itself) were fine.
pub(crate) fn purge_blocks_rebuild_raw(
    kio_dir: &Path,
    raw_hash: &str,
    control: &ControlHandle,
) -> Result<bool> {
    let state = PurgeState::open_with_control(kio_dir, control.clone())?;
    if state.barrier_blocks(raw_hash)? {
        return Ok(true);
    }
    let tombstone_tail = state
        .read_tombstone(raw_hash)?
        .map(|record| record.tail().clone());
    let receipt_tail = state
        .read_erase_receipt(raw_hash)?
        .map(|receipt| receipt.tail().clone());
    let canonical = canonical_final_event(tombstone_tail.as_ref(), receipt_tail.as_ref())?;
    Ok(canonical.is_some_and(|canonical| canonical.event.kind != EventKind::Retired))
}

/// R23-10: the narrower sibling of [`purge_blocks_rebuild_raw`], for
/// `kio reindex --at <commit>` (`historical_reindex::run`) specifically.
/// Unlike the primary full-rebuild/embedding-generation path
/// (`retained_history_instances`/`retained_history_chunks`, which
/// intentionally exclude BOTH canonical `purged` and `erased` from the
/// always-searched/always-embedded index), an explicit historical `--at`
/// enrichment is not one of the erase receipt's five whitelisted uses
/// (08-evidence-pointer-spec.md §4.2: "public の tombstone 判定・re-ingest
/// barrier には使わない") — it must gate ONLY on the PUBLIC tombstone's
/// canonical state, never on a non-public erase receipt
/// (`ct4_purge_erase_receipt_is_ignored_then_retired_by_explicit_ingest`
/// re-indexes a pre-erase historical commit and expects zero
/// `blocked_raw_hashes`). Still resolves the cross-marker
/// `canonical_final_event` (not single-marker existence), so a resurrected
/// tombstone, or a higher-lifecycle-epoch `retired` receipt superseding an
/// older active tombstone, is not wrongly excluded either (AUD-08's other
/// worked example — the same "existence vs canonical" bug this whole fix
/// targets).
pub(crate) fn purge_blocks_historical_reindex_raw(
    kio_dir: &Path,
    raw_hash: &str,
    control: &ControlHandle,
) -> Result<bool> {
    let state = PurgeState::open_with_control(kio_dir, control.clone())?;
    if state.barrier_blocks(raw_hash)? {
        return Ok(true);
    }
    let tombstone_tail = state
        .read_tombstone(raw_hash)?
        .map(|record| record.tail().clone());
    let receipt_tail = state
        .read_erase_receipt(raw_hash)?
        .map(|receipt| receipt.tail().clone());
    let canonical = canonical_final_event(tombstone_tail.as_ref(), receipt_tail.as_ref())?;
    Ok(canonical.is_some_and(|canonical| canonical.event.kind == EventKind::Purged))
}

/// An erase purge intentionally removes the immutable manifest/unit closure
/// that an older snapshot pins.  Historical enrichment may therefore skip that
/// one missing closure while the erase receipt is still active, but only when
/// the receipt's own erased event is backed by a real purge commit at or after
/// the selected snapshot.  This is deliberately separate from
/// `verify_objects::purge_explains_missing_retained_instance`: normal rebuilds retain that
/// stricter retired/resurrection-only rule.
pub(crate) fn active_erase_purge_explains_historical_missing_manifest(
    repo: &Repository,
    raw_hash: &str,
    selected_commit: &str,
) -> Result<bool> {
    let state = PurgeState::open_with_control(repo.kio_dir(), repo.control().clone())?;
    if state.barrier_blocks(raw_hash)? {
        return Ok(false);
    }
    let Some(receipt) = state.read_erase_receipt(raw_hash)? else {
        return Ok(false);
    };
    let erased = receipt.tail();
    if erased.kind != EventKind::Erased {
        return Ok(false);
    }
    let purge_commit = match repo.read_commit(&erased.in_commit) {
        Ok(commit) => commit,
        Err(error) if is_store_not_found(&error) => return Ok(false),
        Err(error) => return Err(error),
    };
    if purge_commit.commit_type != CommitType::Purged
        || !purge_commit
            .purged_raws
            .iter()
            .any(|purged| purged == raw_hash)
    {
        return Ok(false);
    }
    is_ancestor_or_equal(repo, selected_commit, &erased.in_commit)
}

/// Enumerate the scopes a search targets (K3, 05 §1.8). Default and `--all-scopes`
/// enumerate the device-local registry (all indexed, `participates_in_global_search`);
/// `--scope <path>` targets exactly that scope (filesystem, even if unregistered);
/// `--descendants` filters the registry by root-path prefix.
pub(crate) fn enumerate_scope_targets(
    ctx: &Invocation<'_>,
    repo: &Repository,
    parsed: &ParsedSearch,
) -> Result<(ScopeSelectionMode, Vec<ScopeTarget>, bool)> {
    if let Some(scope) = &parsed.scope {
        let root = if scope.is_absolute() {
            scope.clone()
        } else {
            repo.root().join(scope)
        };
        if parsed.descendants {
            let root = root.canonicalize().unwrap_or(root);
            let targets = registry_targets_under(&root, ctx.control)?
                .unwrap_or_else(|| registry_unavailable_fallback(ctx, &root));
            return Ok((ScopeSelectionMode::Descendants, targets, false));
        }
        return Ok((
            ScopeSelectionMode::Scope,
            vec![scope_target(&root, ctx.control)?],
            false,
        ));
    }
    if parsed.descendants {
        let targets = registry_targets_under(repo.root(), ctx.control)?
            .unwrap_or_else(|| registry_unavailable_fallback(ctx, repo.root()));
        return Ok((ScopeSelectionMode::Descendants, targets, false));
    }
    // Default and `--all-scopes` share the same enumeration: every indexed,
    // participating scope in the registry (05 §1.8 / 06 §3, CT3-MULTI-008). The
    // difference between the two is spec-undefined (§C-8) and intentionally none.
    let _all_scopes = parsed.all_scopes;
    match registry_all_targets(ctx.control)? {
        Some(targets) => Ok((ScopeSelectionMode::All, targets, true)),
        // The fallback is deliberately just the current scope.  It is enough
        // to serve a degraded search, but not an authoritative device-wide
        // registry snapshot, so callers must not prune replica siblings from
        // it.
        None => Ok((
            ScopeSelectionMode::All,
            registry_unavailable_fallback(ctx, repo.root()),
            false,
        )),
    }
}

/// Shared by [`registry_all_targets`] and [`registry_duplicate_groups`]
/// (R23-27): the registry rows that pass `registry_entry_is_live`'s
/// stale-duplicate check, from ONE registry read. `Ok(None)` mirrors
/// `registry_all_targets`'s "registry could not be opened" signal.
fn live_registry_entries(control: &ControlHandle) -> Result<Option<Vec<RegistryEntry>>> {
    let Ok(db) = RegistryDb::open_default_with_control(control) else {
        return Ok(None);
    };
    let entries = db.search_targets().map_err(index_to_kio)?;
    Ok(Some(
        entries
            .into_iter()
            // R15-3 (defense in depth): a registry row is a valid search target only
            // when the on-disk `.kio` still carries the SAME scope_id. `register_scope`
            // retires stale same-path rows on the next init/index, but a search that
            // runs BEFORE that re-registration must not trust them — otherwise a
            // deleted-then-re-init'd folder double-returns its doc via a dead scope_id.
            // Mirrors the on-disk verification `resolve_scope_id_in_registry` already
            // does on the Evidence-resolution side (removing the search/resolve
            // asymmetry).
            .filter(|entry| registry_entry_is_live(entry, control))
            .collect(),
    ))
}

/// All indexed, participating scopes from the registry (deterministic order).
/// `Ok(None)` when the registry could not be *opened* (transient lock or real
/// error) — deliberately distinct from `Ok(Some(vec![]))` (registry opened, no
/// eligible scopes). The caller degrades an open failure to the current scope
/// instead of a misleading "no indexed scopes registered" that would erase the
/// healthy scope the user is standing in (P6, Opus F2); an empty registry keeps
/// the exit-4 guidance.
///
/// R23-27 (10 §3 L284-299): a scope_id with more than one LIVE registered
/// path is a clone pair — fail-closed, never search either one silently. Such
/// groups are dropped from this healthy list entirely (see
/// [`registry_duplicate_groups`] for the caller-facing `excluded_scopes`
/// reporting of what was dropped and why).
fn registry_all_targets(control: &ControlHandle) -> Result<Option<Vec<ScopeTarget>>> {
    let Some(live) = live_registry_entries(control)? else {
        return Ok(None);
    };
    let mut by_scope_id: BTreeMap<String, Vec<RegistryEntry>> = BTreeMap::new();
    for entry in live {
        by_scope_id
            .entry(entry.scope_id.clone())
            .or_default()
            .push(entry);
    }
    Ok(Some(
        by_scope_id
            .into_values()
            .filter(|group| group.len() == 1)
            .flatten()
            .map(registry_entry_target)
            .filter(|target| participates_in_global_search(&target.kio_dir))
            .collect(),
    ))
}

/// R23-27 (10 §3 L284-299 "同一 scope_id の複数 live path は clone 併存...
/// global search は当該 scope_id を skip して excluded_scopes に
/// KIO-E-REGISTRY-DUP-001 の理由付きで記録"): the live scope_id groups
/// `registry_all_targets` drops for having more than one live path, paired
/// with each candidate's `.kio` path — for the caller to record as
/// `excluded_scopes` entries. `Ok(None)` mirrors `registry_all_targets`'s
/// "registry could not be opened" signal.
type RegistryDuplicateGroups = Vec<(String, Vec<PathBuf>)>;

pub(crate) fn registry_duplicate_groups(
    control: &ControlHandle,
) -> Result<Option<RegistryDuplicateGroups>> {
    let Some(live) = live_registry_entries(control)? else {
        return Ok(None);
    };
    let mut by_scope_id: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for entry in live {
        by_scope_id
            .entry(entry.scope_id)
            .or_default()
            .push(PathBuf::from(entry.kio_path));
    }
    Ok(Some(
        by_scope_id
            .into_iter()
            .filter(|(_, paths)| paths.len() > 1)
            .map(|(scope_id, mut paths)| {
                paths.sort();
                (scope_id, paths)
            })
            .collect(),
    ))
}

/// R15-3: reject a registry row ONLY when the on-disk `.kio` at `entry.root_path` opens
/// and carries a DIFFERENT `scope_id` — i.e. a delete-and-re-`init` minted a fresh
/// scope_id at the same path (the stale-duplicate case). An open/read failure
/// (unreadable/locked/absent `.kio`) is deliberately KEPT as a target: it is not a
/// confirmed stale row, and silently dropping it would hide a genuinely unreachable
/// scope that the search must instead surface as an `excluded_scopes` partial failure
/// (05 §1.8). Only a positively-confirmed scope_id mismatch is filtered here.
fn registry_entry_is_live(
    entry: &RegistryEntry,
    control: &kio_core::control::ControlHandle,
) -> bool {
    match scope_target(Path::new(&entry.root_path), control) {
        Ok(target) => target.scope_id == entry.scope_id,
        Err(_) => true,
    }
}

/// Registered scopes whose root path is at or below `root` (05 §1.8 prefix
/// filter). `Ok(None)` propagates a registry open failure (see
/// [`registry_all_targets`]).
fn registry_targets_under(
    root: &Path,
    control: &ControlHandle,
) -> Result<Option<Vec<ScopeTarget>>> {
    Ok(registry_all_targets(control)?.map(|targets| {
        targets
            .into_iter()
            .filter(|target| target.repo_root.starts_with(root))
            .collect()
    }))
}

/// P6: when the registry cannot be opened (distinct from an empty registry),
/// degrade a default / `--all-scopes` / `--descendants` search to `root`'s own
/// scope — already held open and known reachable — rather than returning a
/// misleading KIO-E-SEARCH-SCOPE-ALL-FAILED-001. Empty when `root` is not itself
/// a scope (then the search is a genuine all-failed, exit 4, as before).
fn registry_unavailable_fallback(ctx: &Invocation<'_>, root: &Path) -> Vec<ScopeTarget> {
    ctx.interaction.diagnostic(
        "warning: scope registry unavailable (search cache; recover with `kio index`); \
         searching the current scope only",
    );
    match scope_target(root, ctx.control) {
        Ok(target) => vec![target],
        Err(_) => Vec::new(),
    }
}

fn registry_entry_target(entry: RegistryEntry) -> ScopeTarget {
    ScopeTarget {
        repo_root: PathBuf::from(entry.root_path),
        kio_dir: PathBuf::from(entry.kio_path),
        scope_id: entry.scope_id,
    }
}

/// Upsert the scope into the device-local registry (03 §4 cache, K3). Best-effort:
/// a registry write never fails `init` / `index`.
pub(crate) fn register_scope(ctx: &Invocation<'_>, repo: &Repository, indexed: bool) {
    let Ok(scope_id) = scope_id(repo.kio_dir()) else {
        return;
    };
    let entry = RegistryEntry {
        scope_id,
        // Bound child scopes keep descriptor capabilities; device-global state
        // retains the discovery-time canonical identity for later lookup.
        kio_path: repo.canonical_root().join(".kio").display().to_string(),
        root_path: repo.canonical_root().display().to_string(),
        participates_in_global_search: participates_in_global_search(repo.kio_dir()),
        indexed,
        last_seen_at: ctx.control.now_utc_seconds(),
    };
    if let Ok(db) = RegistryDb::open_default_with_control(ctx.control) {
        // R15-3: retire any stale registration for THIS `.kio` path under a DIFFERENT
        // scope_id first. A deleted-then-re-`init`ed folder mints a fresh scope_id at
        // the same path; the composite PK `(scope_id, kio_path)` would otherwise leave
        // the old row forever, double-returning the doc in multi-scope search with a
        // dead-pointer (unresolvable) scope_id. Best-effort like the upsert itself.
        if let Err(error) = db.retire_stale_kio_path(&entry.kio_path, &entry.scope_id) {
            ctx.interaction.diagnostic(&format!(
                "warning: scope registry cleanup failed (search cache; recover with `kio index`): {}",
                diagnostic_safe_text(&error.to_string(), false)
            ));
        }
        if let Err(error) = db.upsert(&entry) {
            ctx.interaction.diagnostic(&format!(
                "warning: scope registry write failed (search cache; recover with `kio index`): {}",
                diagnostic_safe_text(&error.to_string(), false)
            ));
        }
    }
}

/// `participates_in_global_search` (defaults to true, 05 §1.8). The config schema
/// (config.schema.json) puts this under `[scope]`; `[search]` is also accepted for
/// robustness. Either way, absence means "participates".
pub(crate) fn participates_in_global_search(kio_dir: &Path) -> bool {
    let path = kio_dir.join("config.toml");
    let Ok(text) = fs::read_to_string(&path) else {
        return true;
    };
    let Ok(value) = toml::from_str::<toml::Value>(&text) else {
        return true;
    };
    for section in ["scope", "search"] {
        if let Some(flag) = value
            .get(section)
            .and_then(|table| table.get("participates_in_global_search"))
            .and_then(toml::Value::as_bool)
        {
            return flag;
        }
    }
    true
}

pub(crate) fn scope_target(root: &Path, control: &ControlHandle) -> Result<ScopeTarget> {
    let repo = Repository::open_for_search_with_control(root, control.clone())?;
    Ok(ScopeTarget {
        repo_root: repo.root().to_path_buf(),
        kio_dir: repo.kio_dir().to_path_buf(),
        scope_id: scope_id(repo.kio_dir())?,
    })
}

pub(crate) fn scope_id(kio_dir: &Path) -> Result<String> {
    let path = kio_dir.join("scope.json");
    let value: Value = serde_json::from_slice(
        &fs::read(&path)
            .map_err(|err| KioError::io(err.to_string(), path.display().to_string()))?,
    )
    .map_err(|err| KioError::schema(err.to_string()))?;
    value
        .get("scope_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| KioError::schema("scope.json missing scope_id"))
}

/// Two-stage scope resolution (08 §3.1 step 1). Root trust is `scope_id`; the
/// `scope_path` hint and the scope-registry are both non-authoritative caches
/// (05 §1.7 truth vs cache).
///
/// PB21 (step4b-contract-tests-p2b.md §H, 10 §3 L284-285 / 08 §3.1 step 1b
/// L152-155): fail-closed on ANY live scope_id duplicate, not only a
/// `last_seen_at` tie — the old tie-only check left a silent "newest wins"
/// auto-selection live whenever two clones' timestamps merely differed,
/// which can resolve to a purge-stale clone and misjudge scope-wide purge
/// state. `Ok(None)` when no registered `.kio` still resolves to this
/// scope_id. Shared by `resolve_scope_target` (Evidence) and
/// `resolve_cursor_exec_scopes` (search cursor) so a `.kio`-copy collision is
/// detected identically on both paths (O7).
pub(crate) fn resolve_scope_id_in_registry(
    ctx: &Invocation<'_>,
    scope_id: &str,
) -> Result<Option<ScopeTarget>> {
    resolve_scope_id_in_registry_with_hint(
        ctx.interaction.as_ref(),
        scope_id,
        None,
        ScopeResolutionMode::Normal,
        ctx.control,
    )
}

/// QA67 (step4b-contract-tests-p3a.md §T, PB24, 10 §3 L297-299): fail-closed
/// (`KIO-E-REGISTRY-DUP-001`) when `scope_id` resolves to more than one live
/// `.kio` clone, for a write path that is about to touch a device-global
/// ledger row keyed by this `scope_id`. A no-op for the reserved
/// `LedgerTaskKey::DEVICE_SCOPE_ID` pseudo-scope (never a real registered
/// scope) and for any `scope_id` the registry does not resolve at all
/// (`resolve_scope_id_in_registry_with_hint` already treats "no live
/// duplicate" and "not found" identically — `Ok(None)`).
pub(crate) fn registry_duplicate_guard(ctx: &Invocation<'_>, scope_id: &str) -> Result<()> {
    if scope_id == LedgerTaskKey::DEVICE_SCOPE_ID {
        return Ok(());
    }
    resolve_scope_id_in_registry(ctx, scope_id).map(|_| ())
}

/// PB23 (08 §3.1 step 1b L156-158): `extra_live` folds a scope_path-hinted
/// candidate into the SAME live-duplicate candidate pool as the registry rows
/// (deduplicated by canonical `.kio` path) — a registry-unregistered clone
/// named only via `scope_path` counts toward the duplicate check exactly like
/// a registered one, so the JSON-pointer (`scope_path` present) and URI
/// (`scope_path` dropped) representations of the same pointer never disagree
/// about whether a scope_id is live-duplicated.
fn resolve_scope_id_in_registry_with_hint(
    interaction: &dyn context::Interaction,
    scope_id: &str,
    extra_live: Option<ScopeTarget>,
    mode: ScopeResolutionMode,
    control: &kio_core::control::ControlHandle,
) -> Result<Option<ScopeTarget>> {
    let mut live = Vec::<ScopeTarget>::new();
    live.extend(extra_live);
    match open_registry_for_scope_resolution(mode, control) {
        Ok(registry) => {
            if let Ok(entries) = registry.lookup_scope_id(scope_id) {
                for entry in &entries {
                    if let Some(target) =
                        open_scope_from_hint_with_mode(&entry.root_path, mode, control)
                        && target.scope_id == scope_id
                    {
                        live.push(target);
                    }
                }
            }
        }
        // P6: a registry *open* failure is not "scope_id absent". Surface it (the
        // caller still falls back to the scope_path hint) instead of silently
        // conflating it with a genuine registry miss; WAL + busy_timeout makes the
        // transient case rare, and a real failure is now observable.
        Err(_) if matches!(mode, ScopeResolutionMode::Normal) => {
            interaction.diagnostic(
                "warning: scope registry unavailable (search cache); \
                 resolving evidence scope via the scope_path hint only",
            );
        }
        // A missing device registry is the normal first-run state for the
        // strict read-only Evidence path.  It remains a non-authoritative cache
        // miss and must not prefix the command's structured JSON stream.
        Err(_) => {}
    }
    if live.is_empty() {
        return Ok(None);
    }
    let mut unique_dirs = live
        .iter()
        .map(|target| target.kio_dir.clone())
        .collect::<Vec<_>>();
    unique_dirs.sort();
    unique_dirs.dedup();
    if unique_dirs.len() > 1 {
        return Err(registry_duplicate_error(scope_id, &unique_dirs));
    }
    Ok(live.into_iter().next())
}

pub(crate) fn resolve_scope_target(
    cwd: &Path,
    interaction: &dyn context::Interaction,
    scope_id: &str,
    scope_path_hint: Option<&str>,
    control: &kio_core::control::ControlHandle,
) -> Result<ScopeTarget> {
    resolve_scope_target_with_mode(
        cwd,
        interaction,
        scope_id,
        scope_path_hint,
        ScopeResolutionMode::Normal,
        control,
    )
}

/// Evidence must use the exact same candidate, duplicate, and incompatible-format
/// resolution rules as ordinary Evidence consumers, but it must not repair HEAD.
/// The mode-specific opener is the sole difference; registry entries and hints are
/// still non-authoritative candidates.
/// Evidence resolution takes exactly one owned registry snapshot per resolution.
/// The snapshot supplies both the live-candidate union and the final
/// incompatible-format peek; unsafe or unstable registry state is a command
/// failure, never a reason to bypass the registry using a hint or CWD.
pub(crate) fn resolve_evidence_scope_target(
    cwd: &Path,
    registry_input: &EvidenceRegistry,
    scope_id: &str,
    scope_path_hint: Option<&str>,
    control: &kio_core::control::ControlHandle,
) -> Result<ScopeTarget> {
    let registry_path = registry_input.path(control)?;
    let registry = match RegistryDb::open_read_only_with_control(&registry_path, control) {
        Ok(registry) => Some(registry),
        Err(RegistrySnapshotError::Missing) => None,
        Err(error) => return Err(registry_snapshot_open_error(&registry_path, error)),
    };
    // Keep the owned `RegistryDb` alive through candidate selection and the
    // format peek.  `entries` is consequently one coherent, private snapshot
    // generation rather than a second live-registry open.
    let entries = registry
        .as_ref()
        .map(|registry| {
            registry
                .lookup_scope_id_snapshot(scope_id)
                .map_err(|error| registry_snapshot_lookup_error(&registry_path, error))
        })
        .transpose()?
        .unwrap_or_default();

    // A valid scope_path is a candidate, not an authority: fold it into the
    // same deduplicated pool as the rows read from this snapshot.
    let hinted = scope_path_hint
        .and_then(|hint| {
            open_scope_from_hint_with_mode(hint, ScopeResolutionMode::EvidenceReadOnly, control)
        })
        .filter(|target| target.scope_id == scope_id);
    if let Some(target) = resolve_scope_id_from_registry_entries(
        scope_id,
        hinted,
        ScopeResolutionMode::EvidenceReadOnly,
        &entries,
        control,
    )? {
        return Ok(target);
    }

    // A cache miss alone retains the documented hint/CWD recovery path.  The
    // branches above deliberately returned before this point for every unsafe,
    // unstable, busy, copy/open/query, or TOCTOU snapshot outcome.
    if let Some(target) =
        current_scope_target_with_mode(cwd, ScopeResolutionMode::EvidenceReadOnly, control)
            .ok()
            .filter(|target| target.scope_id == scope_id)
    {
        return Ok(target);
    }

    // QB6: re-use the entries from the owned snapshot, rather than re-opening
    // the mutable registry for the incompatible-format diagnosis.
    let mut candidate_roots: Vec<PathBuf> = Vec::new();
    if let Some(hint) = scope_path_hint {
        candidate_roots.push(PathBuf::from(hint));
    }
    candidate_roots.extend(entries.iter().map(|entry| PathBuf::from(&entry.root_path)));
    candidate_roots.push(cwd.to_path_buf());
    for root in candidate_roots {
        if let Some(version) = peek_incompatible_format_version(&root, scope_id) {
            return Err(KioError::incompatible_format(version));
        }
    }
    Err(scope_unreachable_error(scope_id))
}

fn resolve_scope_id_from_registry_entries(
    scope_id: &str,
    extra_live: Option<ScopeTarget>,
    mode: ScopeResolutionMode,
    entries: &[RegistryEntry],
    control: &kio_core::control::ControlHandle,
) -> Result<Option<ScopeTarget>> {
    let mut live = Vec::<ScopeTarget>::new();
    live.extend(extra_live);
    for entry in entries {
        if let Some(target) = open_scope_from_hint_with_mode(&entry.root_path, mode, control)
            && target.scope_id == scope_id
        {
            live.push(target);
        }
    }
    if live.is_empty() {
        return Ok(None);
    }
    let mut unique_dirs = live
        .iter()
        .map(|target| target.kio_dir.clone())
        .collect::<Vec<_>>();
    unique_dirs.sort();
    unique_dirs.dedup();
    if unique_dirs.len() > 1 {
        return Err(registry_duplicate_error(scope_id, &unique_dirs));
    }
    Ok(live.into_iter().next())
}

pub(crate) fn registry_snapshot_unsafe_error(registry_path: &Path, detail: String) -> KioError {
    KioError::new(
        "KIO-E-REGISTRY-SNAPSHOT-UNSAFE-001",
        "scope registry could not be read without violating integrity safeguards",
        json!({
            "registry_path": registry_path,
            "reason": "unsafe_integrity",
            "detail": detail,
        }),
        ExitCode::PermanentFailure,
    )
}

fn registry_snapshot_open_error(registry_path: &Path, error: RegistrySnapshotError) -> KioError {
    match error {
        RegistrySnapshotError::Missing => registry_snapshot_unsafe_error(
            registry_path,
            "registry snapshot open reported Missing after a present leaf was required".to_owned(),
        ),
        RegistrySnapshotError::UnsafeIntegrity(detail) => {
            registry_snapshot_unsafe_error(registry_path, detail)
        }
        RegistrySnapshotError::UnstableBusy(detail) => {
            registry_snapshot_transient_error(registry_path, "unstable_or_busy", detail)
        }
    }
}

pub(crate) fn registry_snapshot_lookup_error(
    registry_path: &Path,
    error: RegistrySnapshotError,
) -> KioError {
    match error {
        // `RegistryDb` owns a successfully opened snapshot here.  A later
        // Missing therefore cannot be a cache miss and must never enable hint
        // or CWD fallback.
        RegistrySnapshotError::Missing => registry_snapshot_unsafe_error(
            registry_path,
            "owned registry snapshot disappeared before lookup".to_owned(),
        ),
        RegistrySnapshotError::UnsafeIntegrity(detail) => {
            registry_snapshot_unsafe_error(registry_path, detail)
        }
        RegistrySnapshotError::UnstableBusy(detail) => {
            registry_snapshot_transient_error(registry_path, "unstable_or_busy", detail)
        }
    }
}

pub(crate) fn registry_snapshot_transient_error(
    registry_path: &Path,
    reason: &str,
    detail: String,
) -> KioError {
    KioError::new(
        "KIO-E-REGISTRY-SNAPSHOT-001",
        "scope registry could not be read from a stable snapshot; retry the command",
        json!({
            "registry_path": registry_path,
            "reason": reason,
            "detail": detail,
        }),
        ExitCode::PartialFailure,
    )
}

#[derive(Clone, Copy)]
pub(crate) enum ScopeResolutionMode {
    /// Existing search/open behavior, including the established HEAD self-heal.
    Normal,
    /// Evidence verify/retarget behavior: no HEAD repair and no registry mutation.
    EvidenceReadOnly,
}

fn resolve_scope_target_with_mode(
    cwd: &Path,
    interaction: &dyn context::Interaction,
    scope_id: &str,
    scope_path_hint: Option<&str>,
    mode: ScopeResolutionMode,
    control: &kio_core::control::ControlHandle,
) -> Result<ScopeTarget> {
    // PB23: a scope_path hint that itself resolves live to this scope_id is
    // folded into the registry-duplicate candidate pool below rather than
    // short-circuiting before that check ever runs (the old 1a/1b ordering
    // let a valid hint bypass duplicate detection entirely).
    let hinted = scope_path_hint
        .and_then(|hint| open_scope_from_hint_with_mode(hint, mode, control))
        .filter(|target| target.scope_id == scope_id);
    if let Some(target) =
        resolve_scope_id_in_registry_with_hint(interaction, scope_id, hinted, mode, control)?
    {
        return Ok(target);
    }
    // Pragmatic fallback: the current working directory when it *is* the scope.
    // Object URIs carry no scope_path hint, and a freshly-created scope may not
    // yet be listed in the registry. Still gated on scope_id equality.
    if let Some(target) = current_scope_target_with_mode(cwd, mode, control)
        .ok()
        .filter(|target| target.scope_id == scope_id)
    {
        return Ok(target);
    }
    // QB6 (step4b-contract-tests-p3b.md §A, 10 §3 L300-305): every path above
    // resolves a candidate through `scope_target` / `Repository::open_current`,
    // which FULLY validates the store (`kio_format_version` included) — so a
    // scope that exists but is format-incompatible is indistinguishable from
    // one that does not exist at all, and silently drops out of every
    // candidate pool above. That conflates two materially different
    // diagnoses: "cannot find this scope" vs. "found it, but its format is
    // non-current" (0). Before reporting the generic scope_unreachable, take one
    // more direct, unvalidated peek at each candidate's `scope.json` (the
    // hint, every registry row for this scope_id, and the CWD) so a
    // genuinely-incompatible-version scope is reported as
    // `KIO-E-STORE-VERSION-001` — outranking scope_unreachable, matching (0)'s
    // priority over every other preflight condition.
    let mut candidate_roots: Vec<PathBuf> = Vec::new();
    if let Some(hint) = scope_path_hint {
        candidate_roots.push(PathBuf::from(hint));
    }
    if let Ok(registry) = open_registry_for_scope_resolution(mode, control)
        && let Ok(entries) = registry.lookup_scope_id(scope_id)
    {
        candidate_roots.extend(
            entries
                .into_iter()
                .map(|entry| PathBuf::from(entry.root_path)),
        );
    }
    candidate_roots.push(cwd.to_path_buf());
    for root in candidate_roots {
        if let Some(version) = peek_incompatible_format_version(&root, scope_id) {
            return Err(KioError::incompatible_format(version));
        }
    }
    Err(scope_unreachable_error(scope_id))
}

fn scope_target_with_mode(
    root: &Path,
    mode: ScopeResolutionMode,
    control: &ControlHandle,
) -> Result<ScopeTarget> {
    match mode {
        ScopeResolutionMode::Normal => scope_target(root, control),
        ScopeResolutionMode::EvidenceReadOnly => {
            let repo = Repository::open_for_search_read_only_with_control(root, control.clone())?;
            Ok(ScopeTarget {
                repo_root: repo.root().to_path_buf(),
                kio_dir: repo.kio_dir().to_path_buf(),
                scope_id: scope_id(repo.kio_dir())?,
            })
        }
    }
}

fn current_scope_target_with_mode(
    cwd: &Path,
    mode: ScopeResolutionMode,
    control: &ControlHandle,
) -> Result<ScopeTarget> {
    match mode {
        ScopeResolutionMode::Normal => {
            let repo = Repository::open_for_recovery_with_control(cwd, control.clone())?;
            scope_target(repo.root(), control)
        }
        ScopeResolutionMode::EvidenceReadOnly => {
            let cwd = cwd.to_path_buf();
            scope_target_with_mode(&cwd, mode, control)
        }
    }
}

fn open_registry_for_scope_resolution(
    mode: ScopeResolutionMode,
    control: &ControlHandle,
) -> kio_index::Result<RegistryDb> {
    match mode {
        ScopeResolutionMode::Normal => RegistryDb::open_default_with_control(control),
        ScopeResolutionMode::EvidenceReadOnly => {
            RegistryDb::open_default_read_only_with_control(control)
        }
    }
}

pub(crate) fn open_scope_from_hint_with_mode(
    hint: &str,
    mode: ScopeResolutionMode,
    control: &kio_core::control::ControlHandle,
) -> Option<ScopeTarget> {
    let path = Path::new(hint);
    scope_target_with_mode(path, mode, control)
        .ok()
        .or_else(|| {
            (path.file_name() == Some(std::ffi::OsStr::new(".kio")))
                .then(|| path.parent())
                .flatten()
                .and_then(|parent| scope_target_with_mode(parent, mode, control).ok())
        })
}

/// QB6: reads `scope.json` directly (no `Repository::open*` — deliberately
/// skips JSON Schema validation and every other store check) for one
/// resolution candidate, so `resolve_scope_target`'s final fallback can
/// diagnose "found, but its kio_format_version is incompatible" distinctly
/// from "does not exist" without paying for (or risking a false positive
/// from) full validation. `candidate_root` may be either a scope root or its
/// `.kio` directory (mirrors `open_scope_from_hint`). Returns the persisted
/// version—or an explicit missing/non-string marker—only when the scope_id
/// matches and the version is not this build's exact current format. Missing
/// file, unparsable JSON, scope_id mismatch, and the current version return
/// `None`, so this cannot invent a STORE-VERSION error for an absent scope.
fn peek_incompatible_format_version(candidate_root: &Path, scope_id: &str) -> Option<String> {
    let scope_json_path = if candidate_root.file_name() == Some(std::ffi::OsStr::new(".kio")) {
        candidate_root.join("scope.json")
    } else {
        candidate_root.join(".kio/scope.json")
    };
    let text = fs::read_to_string(&scope_json_path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    if value.get("scope_id").and_then(Value::as_str) != Some(scope_id) {
        return None;
    }
    let version = match value.get("kio_format_version") {
        Some(Value::String(version)) => version.clone(),
        Some(_) => "<non-string>".to_owned(),
        None => "<missing>".to_owned(),
    };
    // Mirrors `kio_core::scope`'s private current-format equality check. This
    // pre-stable reader understands only 0.1.0; an older, malformed, or newer
    // value all take the same version-specific fail-closed path before current
    // schema validation.
    (version != kio_core::scope::KIO_FORMAT_VERSION).then_some(version)
}

/// §I read barrier (LC52-56, 10-operations.md §3's "複合状態の優先順位" /
/// "返却直前の再検査"): the (active journal, `purge/epoch`, lifecycle-epoch)
/// triple observed once at a read command's start (checkpoint 1, LC53) and
/// reverified — in this fixed order — unchanged immediately before the
/// command returns body or existence information (checkpoint 2, LC54/LC55).
///
/// This is a SEPARATE mechanism from `enforce_purge_read_barrier`'s
/// per-raw_hash tombstone/journal-target check (LC11-14 and the mutation-side
/// `barrier_blocks`): that answers "is THIS raw_hash visible"; this answers
/// "did a purge transaction start or complete on ANY raw_hash in this scope
/// while this read was in flight" (05-runtime.md §3.5's explicit warning not
/// to conflate the two barriers, echoed at §G's LC45 vs LC54 boundary).
#[derive(Debug, Clone)]
pub(crate) struct ReadBarrierCheckpoint {
    pub(crate) kio_dir: PathBuf,
    pub(crate) control: ControlHandle,
    pub(crate) purge_epoch: u64,
    pub(crate) lifecycle_epoch: u64,
}

impl ReadBarrierCheckpoint {
    /// Checkpoint 1 (LC53): reject an already-active journal, then snapshot
    /// `purge/epoch` (fail-closed, LC39) and the raw lifecycle-epoch counter
    /// as this invocation's linearization point.
    pub(crate) fn open(kio_dir: &Path, control: &ControlHandle) -> Result<Self> {
        let purge = PurgeState::open_with_control(kio_dir, control.clone())?;
        if purge.read_barrier_active()? {
            return Err(purge_journal_active_error());
        }
        Ok(Self {
            kio_dir: kio_dir.to_path_buf(),
            control: control.clone(),
            purge_epoch: purge.read_purge_epoch()?,
            lifecycle_epoch: purge.read_lifecycle_epoch()?,
        })
    }

    /// Checkpoint 2 (LC54/LC55): fixed order — journal absence, then purge
    /// epoch == the checkpoint-1 value, then lifecycle counter == the
    /// checkpoint-1 value. Compares only against the values `open` captured
    /// on this same struct — never against a freshly-read `last_lifecycle_epoch`
    /// (LC54's explicit warning against reusing the §G/LC45 rollback
    /// comparison here, which would silently swap in the *current* SQLite
    /// value instead of the invocation's own frozen baseline).
    pub(crate) fn recheck(&self) -> Result<()> {
        let purge = PurgeState::open_with_control(&self.kio_dir, self.control.clone())?;
        if purge.read_barrier_active()? {
            return Err(purge_journal_active_error());
        }
        if purge.read_purge_epoch()? != self.purge_epoch {
            return Err(purge_journal_active_error());
        }
        if !purge.lifecycle_epoch_matches(self.lifecycle_epoch)? {
            return Err(purge_journal_active_error());
        }
        Ok(())
    }

    /// LC55: run checkpoint 2 and only on success hand back `value` — an Err
    /// here drops `value` by construction (Rust's normal `?`/return-value
    /// semantics), so "discard the already-obtained result" holds without
    /// relying on caller discipline at each of the many return sites.
    pub(crate) fn finish<T>(&self, value: T) -> Result<T> {
        self.recheck()?;
        Ok(value)
    }
}

/// Recheck only the candidate scopes that remain eligible to cross a search
/// response boundary.  A normal multi-scope search has already had one chance
/// to discard a newly blocked scope and retain healthy siblings; this final
/// pass is intentionally stricter because the response body (including signed
/// pointers and a cursor) is now complete.  Returning an error drops that
/// assembled body atomically from the caller's point of view.
pub(crate) fn recheck_search_response_barriers(
    late_barriers: &[(usize, usize, ReadBarrierCheckpoint)],
    exec_scopes: &[ExecScope],
) -> Result<()> {
    for (scope_index, _, checkpoint) in late_barriers {
        if let Err(error) = checkpoint.recheck() {
            if error.error_code() != "KIO-E-PURGE-JOURNAL-ACTIVE-001" {
                return Err(error);
            }
            let exec = &exec_scopes[*scope_index];
            if exec.from_cursor {
                return Err(KioError::new(
                    "KIO-E-SEARCH-CURSOR-001",
                    "search cursor active scope is no longer available; re-run without a cursor",
                    json!({
                        "reason": "active_scope_unavailable",
                        "cause": "purge_journal_active",
                        "scope_id": exec.target.scope_id,
                    }),
                    ExitCode::InvalidUsage,
                ));
            }
            return Err(error);
        }
    }
    Ok(())
}

/// Return whether any scope participating in this response has a validated
/// Phase-4 GC recovery marker.  This intentionally performs no recovery and
/// makes no assumptions from the replica: the marker is source-store truth.
/// A malformed/replaced marker is a core fail-closed error, not "inactive".
pub(crate) fn gc_marker_active_for_scopes(
    searched_scopes: &[SearchedScopeInfo],
    control: &ControlHandle,
) -> Result<bool> {
    for scope in searched_scopes {
        if kio_core::gc::read_active_marker_with_control(&scope.scope_path, control)?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Integration-test synchronization point for the narrow interval after the
/// ordinary candidate check and after all response materialization, but before
/// the final response-boundary check.  The value is a ready-file path; the
/// child writes it and waits briefly for the same path with a `.release`
/// extension.  It is deliberately inert unless the explicitly test-named
/// variable is set.
#[cfg(debug_assertions)]
pub(crate) fn maybe_wait_at_test_search_response_barrier(control: &ControlHandle) {
    let ready_path = control.debug().cli.search_response_barrier_ready.clone();
    let Some(ready_path) = ready_path else {
        return;
    };
    if fs::write(&ready_path, b"ready").is_err() {
        return;
    }
    let release_path = ready_path.with_extension("release");
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    while !release_path.exists() && Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[cfg(not(debug_assertions))]
pub(crate) fn maybe_wait_at_test_search_response_barrier(_control: &ControlHandle) {}

/// Test-only synchronization point immediately before search's final consent
/// re-read. It does not alter consent or a search outcome: the integration
/// test performs an ordinary public `kio adapter revoke` while the child waits
/// here, proving that a revoke which wins the final pre-attempt gate cannot
/// open or mutate the source ledger.
#[cfg(debug_assertions)]
pub(crate) fn maybe_wait_at_test_search_final_consent_barrier(control: &ControlHandle) {
    let ready_path = control
        .debug()
        .cli
        .search_final_consent_barrier_ready
        .clone();
    let Some(ready_path) = ready_path else {
        return;
    };
    if fs::write(&ready_path, b"ready").is_err() {
        return;
    }
    let release_path = ready_path.with_extension("release");
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    while !release_path.exists() && Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[cfg(not(debug_assertions))]
pub(crate) fn maybe_wait_at_test_search_final_consent_barrier(_control: &ControlHandle) {}

/// QB5/QB6/QB7 §Z1 / 裁定1 (step4b-contract-tests-p3b.md, 10-operations.md §3
/// L300-311): the shared (1)+(3) read-path preflight pair — §I checkpoint 1
/// (purge journal / epoch, LC53) immediately followed by index (sqlite.db)
/// availability (LC45) — as ONE implementation every read-path command calls,
/// instead of each hand-rolling the same two-line sequence (and risking
/// re-deriving it out of order, as QB6 found `open`/`view`/`kio evidence
/// verify` had). Callers MUST complete step (0) (`kio_format_version`
/// compatibility, via `Repository::open`/`open_for_search`) BEFORE calling
/// this — 10 §3's fixed cross-command order places (0) ahead of (1), and a
/// command that checked this pair first used to surface
/// `KIO-E-PURGE-JOURNAL-ACTIVE-001` instead of the higher-priority
/// `KIO-E-STORE-VERSION-001` when a scope violated both at once.
pub(crate) fn preflight_barrier_and_index(
    kio_dir: &Path,
    control: &ControlHandle,
) -> Result<ReadBarrierCheckpoint> {
    let checkpoint = ReadBarrierCheckpoint::open(kio_dir, control)?;
    check_index_generation_current(kio_dir, control)?;
    Ok(checkpoint)
}

/// 05-runtime.md §3.5 / 10-operations.md §11.1: retryable (exit 3) rejection
/// for either §I checkpoint. Carries no context beyond the error itself — the
/// violation is about scope-wide purge-transaction state, not about any one
/// object, so there is nothing object-specific to disclose (unlike
/// `tombstone_error`/`purge_not_found_error`, which are about one raw_hash).
fn purge_journal_active_error() -> KioError {
    KioError::new(
        "KIO-E-PURGE-JOURNAL-ACTIVE-001",
        "an incomplete purge transaction is active, or completed while this read was in flight; retry",
        json!({}),
        ExitCode::PartialFailure,
    )
}

/// LC8-LC14 (§C): cross-marker canonical-final-event dispatch for the
/// `open`/`view`/`restore` tombstone gate (item 3 of this session's task).
/// Replaces those three commands' prior use of `enforce_purge_read_barrier`'s
/// single-marker `TombstoneRecord::is_active()` check with
/// `kio_core::purge::canonical_final_event` fed by BOTH markers' tail events
/// (each already structurally validated — LC1-3/15/16/19 — by
/// `PurgeState::read_tombstone`/`read_erase_receipt`'s parse path).
///
/// `raw_present` is the caller's own already-resolved answer to "does the raw
/// CAS object exist" — LC13/LC14 both branch on it, and each call site
/// determines it differently (a working-tree-or-CAS scan for open/view,
/// `ObjectStore::inspect_object` for restore's evidence-source path), so this
/// takes it rather than re-deriving it a second way.
///
/// - (i) canonical = `purged` -> `Err` tombstone response (LC11,
///   `status:"tombstoned"`), regardless of `raw_present`.
/// - (ii) canonical = `erased` and `!raw_present` -> `Err`
///   `KIO-E-PURGE-NOT-FOUND-001` (LC12 — erase receipts are never disclosed).
/// - (iii) canonical = `retired` and `!raw_present` -> `Err`
///   `KIO-E-STORE-CORRUPT-001` (LC13 — a retired marker's raw MUST exist;
///   its absence is corruption, a different code from LC12's expected
///   erased-and-gone).
/// - (iv) no marker at all and `!raw_present` -> `Err`
///   `KIO-E-STORE-CORRUPT-001` (LC14(a) — an unmarked absence is a
///   corruption suspicion).
/// - every other combination (canonical = `erased`/`retired`/none with
///   `raw_present` true) -> `Ok(())`, the normal continue-resolving path
///   (LC14(b)/(c)).
///
/// Known residual scope gap (noted for the implementation report): the other
/// 5 barrier commands (`search`/`log`/`diff`/`inspect`/`evidence verify`)
/// still resolve tombstone visibility via the single-marker
/// `enforce_purge_read_barrier`/`verify_pointer_for_cli`'s own
/// `read_tombstone(...).is_some()` check, not this canonical dispatch — the
/// task instructions scoped LC12-14's replacement to open/view/restore only.
pub(crate) fn enforce_canonical_marker_barrier(
    target: &ScopeTarget,
    raw_hash: &str,
    raw_present: bool,
    control: &ControlHandle,
) -> Result<()> {
    let purge = PurgeState::open_with_control(&target.kio_dir, control.clone())?;
    let tombstone_tail = purge
        .read_tombstone(raw_hash)?
        .map(|record| record.tail().clone());
    let receipt_tail = purge
        .read_erase_receipt(raw_hash)?
        .map(|receipt| receipt.tail().clone());
    let canonical_event =
        canonical_final_event(tombstone_tail.as_ref(), receipt_tail.as_ref())?.map(|c| c.event);
    match canonical_event {
        Some(event) if event.kind == EventKind::Purged => Err(tombstone_error(json!({
            "raw_hash": raw_hash,
            "purged_at": event.at,
            "purged_reason": event.reason,
            "purged_in_commit": event.in_commit,
            "scope_path": target.kio_dir.display().to_string(),
        }))),
        Some(event) if event.kind == EventKind::Erased && !raw_present => {
            Err(purge_not_found_error(target, raw_hash))
        }
        Some(event) if event.kind == EventKind::Retired && !raw_present => {
            Err(retired_raw_missing_error(target, raw_hash))
        }
        None if !raw_present => Err(unmarked_missing_raw_error(target, raw_hash)),
        _ => Ok(()),
    }
}

/// LC8-LC11 only: does canonical dispatch say this raw_hash is currently
/// `purged`? For a caller that has ALREADY independently determined
/// "not found" for a reason unrelated to raw CAS/working-tree presence — a
/// tree-membership failure (this pointer's commit never referenced this
/// raw_hash at all; LC12/13/14's raw-presence branches do not apply, since
/// the raw itself may be perfectly present elsewhere, just not in THIS
/// commit's tree) — and only wants the more specific tombstone response when
/// that is the actual reason. Equivalent to calling
/// `enforce_canonical_marker_barrier` with `raw_present=true`: that
/// unconditionally suppresses branches (ii)/(iii)/(iv) (LC12/13/14 all
/// require `raw_present=false` to fire), leaving only branch (i) live.
pub(crate) fn enforce_canonical_tombstone_only(
    target: &ScopeTarget,
    raw_hash: &str,
    control: &ControlHandle,
) -> Result<()> {
    enforce_canonical_marker_barrier(target, raw_hash, true, control)
}

/// LC13: canonical final event = `retired` but the raw object is absent —
/// corruption (a retired marker's raw MUST have been re-published by the
/// same locked mutation that appended `retired`), distinct from LC12's
/// expected-absence `KIO-E-PURGE-NOT-FOUND-001`.
pub(crate) fn retired_raw_missing_error(target: &ScopeTarget, raw_hash: &str) -> KioError {
    KioError::new(
        "KIO-E-STORE-CORRUPT-001",
        "tombstone lifecycle records a resurrection but the raw object is missing",
        json!({
            "raw_hash": raw_hash,
            "scope_path": target.kio_dir.display().to_string(),
        }),
        ExitCode::PermanentFailure,
    )
}

/// LC14(a): no tombstone/erase-receipt marker at all, yet the raw object is
/// absent — an unmarked absence is a corruption suspicion (`kio repair
/// --verify-objects` is the recommended next step), not a normal purge.
pub(crate) fn unmarked_missing_raw_error(target: &ScopeTarget, raw_hash: &str) -> KioError {
    KioError::new(
        "KIO-E-STORE-CORRUPT-001",
        "raw object is missing with no purge marker to explain the absence; run kio repair verify-objects",
        json!({
            "raw_hash": raw_hash,
            "scope_path": target.kio_dir.display().to_string(),
        }),
        ExitCode::PermanentFailure,
    )
}

/// Enforce the purge visibility boundary on every raw-derived read surface.
/// Erase receipts are deliberately absent here: they are fsck-only and must not
/// prevent a later verified re-ingest of identical bytes.
pub(crate) fn enforce_purge_read_barrier(
    target: &ScopeTarget,
    raw_hash: &str,
    control: &ControlHandle,
) -> Result<()> {
    if let Some(tombstone) = read_tombstone(target, raw_hash, control)? {
        return Err(tombstone_error(tombstone));
    }
    if PurgeState::open_with_control(&target.kio_dir, control.clone())?.barrier_blocks(raw_hash)? {
        return Err(purge_not_found_error(target, raw_hash));
    }
    Ok(())
}

/// 08 §4.1 tombstone response as an exit-4 error (open/view surface it as a
/// dead pointer). `context` carries the full `status="tombstoned"` tombstone
/// body. LC11: renamed from `"purged"` so this path agrees with
/// `verify_objects.rs`'s evidence-verify response (both must say
/// `"tombstoned"` — the fact of the purge itself is carried separately by the
/// `purged_*` fields).
pub(crate) fn tombstone_error(mut tombstone: Value) -> KioError {
    if let Some(object) = tombstone.as_object_mut() {
        object.insert("status".to_owned(), json!("tombstoned"));
    }
    KioError::new(
        "KIO-E-PURGE-TOMBSTONED-001",
        "evidence target was purged (tombstone recorded)",
        tombstone,
        ExitCode::PermanentFailure,
    )
}

/// 08 §4.2 — raw object absent with no tombstone record.
pub(crate) fn purge_not_found_error(target: &ScopeTarget, raw_hash: &str) -> KioError {
    KioError::new(
        "KIO-E-PURGE-NOT-FOUND-001",
        "evidence target was purged without tombstone record",
        json!({
            "raw_hash": raw_hash,
            "scope_path": target.kio_dir.display().to_string(),
        }),
        ExitCode::PermanentFailure,
    )
}

/// M6 — the Evidence Pointer's (raw_hash, tool_profile_hash, chunk_hash) do not
/// mutually bind: the chunk row or tree entry it resolves to carries a different
/// identity than the pointer claims. This is a tampered / internally inconsistent
/// pointer; refuse it rather than serve mismatched evidence. Exit 4 (a dead
/// pointer that will never resolve consistently, like the purge family).
pub(crate) fn evidence_policy_denied() -> KioError {
    KioError::new(
        "KIO-E-EVIDENCE-POLICY-001",
        "evidence is not referenced by a currently eligible managed source",
        json!({}),
        ExitCode::PermanentFailure,
    )
}

pub(crate) fn invalid_pointer_identity_error(pointer: &EvidencePointer) -> KioError {
    KioError::new(
        "KIO-E-EVIDENCE-POINTER-INVALID-001",
        "evidence pointer identity mismatch: raw_hash / tool_profile_hash / chunk_hash do not bind to the same chunk",
        json!({
            "raw_hash": pointer.raw_hash,
            "tool_profile_hash": pointer.tool_profile_hash,
            "chunk_hash": pointer.chunk_hash,
        }),
        ExitCode::PermanentFailure,
    )
}

/// R17-1: the pointer's `commit` object cannot be resolved (missing / never existed
/// — e.g. a forged `commit` hash on a `view`/`open` operand). Distinct from a genuine
/// shallow commit (commit present, tree GC'd), whose resolution docs/05 §3.6
/// guarantees never fails; that guarantee is limited to commit *existence* (docs/08
/// §3.2:150) and does not extend to a missing commit. A pointer whose commit does not
/// exist cannot bind the raw_hash to a snapshot tree or the chunk to a normalization
/// generation (N5), so it is rejected as an invalid pointer rather than served
/// best-effort. Same code / exit as `invalid_pointer_identity_error`
/// (EVIDENCE-POINTER-INVALID, exit 4): both mean "this pointer does not name a
/// resolvable, self-consistent piece of evidence".
pub(crate) fn unresolvable_commit_pointer_error(pointer: &EvidencePointer) -> KioError {
    KioError::new(
        "KIO-E-EVIDENCE-POINTER-INVALID-001",
        "evidence pointer references a commit object that does not exist (missing or forged); \
         a resolvable commit is required to bind the raw_hash to its snapshot tree and the chunk \
         to its normalization generation",
        json!({
            "commit": pointer.commit,
            "raw_hash": pointer.raw_hash,
            "chunk_hash": pointer.chunk_hash,
        }),
        ExitCode::PermanentFailure,
    )
}

/// A markerless final shallow receipt proves tree deletion, not the tree's
/// contents. Evidence open/view therefore cannot authenticate a pointer once
/// the historical tree is gone: doing so would let a later-generation or an
/// unrelated chunk borrow the old commit identity through mutable state.
pub(crate) fn unresolvable_shallow_tree_pointer_error(pointer: &EvidencePointer) -> KioError {
    KioError::commit_shallow(
        "evidence pointer cannot be authenticated because this commit's tree was discarded; \
         restore the tree object from backup before opening or viewing this evidence",
        pointer.commit.clone(),
    )
}

/// 08 §3.2 — scope `.kio` unreachable (scope_path unreachable and scope_id not
/// registered). QB1 (step4b-contract-tests-p3b.md §A, 06 §7 L370 / 10 §11.2
/// L931): dead pointers (tombstoned / not_found) are exit 4, but
/// scope_unreachable alone is retryable (the scope may simply be unmounted) —
/// exit 3 (`PartialFailure`), not 4. This is the single shared helper behind
/// `open`/`view` (via `resolve_scope_target`) and `restore`
/// (`resolve_evidence_source`), so the fix here covers all three callers.
fn scope_unreachable_error(scope_id: &str) -> KioError {
    KioError::new(
        "KIO-E-EVIDENCE-SCOPE-UNREACHABLE-001",
        "evidence scope unreachable",
        json!({ "scope_id": scope_id }),
        ExitCode::PartialFailure,
    )
}

/// PB21/22 (step4b-contract-tests-p2b.md §H, 10 §3 L284-287): a scope_id
/// resolves to more than one LIVE registered `.kio` — fail-closed, dedupe
/// required. `KIO-E-REGISTRY-DUP-001` (namespace REGISTRY) replaces the old
/// `KIO-E-EVIDENCE-SCOPE-AMBIGUOUS-001` (PB22's "実装時に確定が必要" resolved:
/// one code, not two overlapping ones, now that PB21 widens detection from
/// "last_seen_at tie only" to "more than one live match at all" — the old
/// code's narrower condition is now a strict subset of this one's). Default
/// exit is `PermanentFailure` (4, matching every other pointer-resolution
/// ambiguity/corruption code in this module); `kio evidence verify` overrides
/// this to exit 3 for its own status-union response (PB54 — registry_duplicate
/// is retryable there regardless of `--strict`).
fn registry_duplicate_error(scope_id: &str, candidates: &[PathBuf]) -> KioError {
    KioError::new(
        "KIO-E-REGISTRY-DUP-001",
        "scope_id resolves to more than one live registered .kio; dedupe before retrying",
        json!({
            "scope_id": scope_id,
            "candidates": candidates
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>(),
        }),
        ExitCode::PermanentFailure,
    )
}

pub(crate) fn is_store_not_found(error: &KioError) -> bool {
    error.error_code() == "KIO-E-STORE-NOT-FOUND-001"
}

/// R16-2: the store-corruption error classes that a single scope may raise mid-search
/// and that must NOT abort a multi-scope search (05 §1.8 per-scope isolation). A
/// missing object (STORE-NOT-FOUND), a corrupt object (STORE-CORRUPT, e.g. a CAS
/// hash mismatch), a store I/O failure (STORE-IO), and a shallow commit
/// (COMMIT-SHALLOW) are all localized to one scope's store; the search loop
/// downgrades them to `Excluded("store_corrupt")` so healthy scopes still return.
/// Deliberately narrow: it does not catch schema / programming errors, which stay
/// fail-fast.
pub(crate) fn is_store_corrupt_class(error: &KioError) -> bool {
    matches!(
        error.error_code(),
        "KIO-E-STORE-NOT-FOUND-001"
            | "KIO-E-STORE-CORRUPT-001"
            | "KIO-E-STORE-IO-001"
            | "KIO-E-COMMIT-SHALLOW-001"
    )
}
