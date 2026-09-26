//! Journaled materialization of a reachable historical tree into the current
//! folder.  This deliberately creates a new, linear `restored` child rather
//! than moving HEAD backwards.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{CommitObject, CommitType, Repository, TreeEntry, TreeObject, commit_stats};
use crate::ExitCode;
use crate::cas::{ObjectKind, canonical_json_bytes, hash_bytes};
use crate::dag::{build_tree_with_chunking_config, is_materializable_direct_child, tree_hash};
use crate::error::{KioError, Result};
use crate::store_dir::{Publication, StoreDirectory};

const MANAGED_RESTORE_JOURNAL_LEAF: &str = "managed-restore-v1.json";
const MAX_MANAGED_RESTORE_JOURNAL_BYTES: u64 = 8 * 1024 * 1024;
const MAX_MANAGED_RESTORE_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_MANAGED_RESTORE_TOTAL_BYTES: u64 = 512 * 1024 * 1024;

/// Immutable request inputs for [`Repository::plan_restore`].  The request is
/// intentionally constructed through [`Self::new`] so callers cannot omit the
/// provenance-bearing source commit or the reproducible commit timestamp.
#[derive(Debug, Clone)]
pub struct ManagedRestoreRequest {
    source_commit: String,
    paths: Option<Vec<String>>,
    delete_missing: Vec<String>,
    message: String,
    created_at: String,
}

impl ManagedRestoreRequest {
    #[must_use]
    pub fn new(
        source_commit: impl Into<String>,
        paths: Option<Vec<String>>,
        delete_missing: Vec<String>,
        message: impl Into<String>,
        created_at: impl Into<String>,
    ) -> Self {
        Self {
            source_commit: source_commit.into(),
            paths,
            delete_missing,
            message: message.into(),
            created_at: created_at.into(),
        }
    }
}

/// One direct-child mutation planned by a managed restore.  `None` denotes an
/// absent leaf, never an unverified path lookup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedRestoreChange {
    pub path: String,
    pub old_raw_hash: Option<String>,
    pub new_raw_hash: Option<String>,
}

/// A read-only, authority-bound restore plan.  Its internals stay private so a
/// caller cannot forge a child commit or substitute an unreviewed raw hash.
#[derive(Debug, Clone, Serialize)]
pub struct ManagedRestorePlan {
    expected_head: String,
    source_commit: String,
    new_tree: TreeObject,
    new_tree_hash: String,
    new_commit: CommitObject,
    new_commit_hash: String,
    /// Expected current raw state for every selected path, including a path
    /// whose historical raw is already equal to HEAD.  This prevents a no-op
    /// restore preview from silently accepting dirty selected working bytes.
    selected_working_hashes: Vec<(String, Option<String>)>,
    changes: Vec<ManagedRestoreChange>,
}

impl ManagedRestorePlan {
    #[must_use]
    pub fn expected_head(&self) -> &str {
        &self.expected_head
    }

    #[must_use]
    pub fn source_commit(&self) -> &str {
        &self.source_commit
    }

    #[must_use]
    pub fn new_tree_hash(&self) -> &str {
        &self.new_tree_hash
    }

    #[must_use]
    pub fn new_commit_hash(&self) -> &str {
        &self.new_commit_hash
    }

    #[must_use]
    pub fn changes(&self) -> &[ManagedRestoreChange] {
        &self.changes
    }

    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.changes.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManagedRestoreOutcome {
    pub noop: bool,
    pub commit_hash: Option<String>,
    pub tree_hash: String,
    pub restored_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagedRestoreJournal {
    version: u8,
    expected_head: String,
    source_commit: String,
    new_tree: String,
    new_commit: String,
    changes: Vec<ManagedRestoreChange>,
    progress: Vec<RestoreProgress>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RestoreProgress {
    Pending,
    RemovingOld,
    OldRemoved,
    PublishingNew,
    Applied,
    RollbackRemovingNew,
    RollbackNewRemoved,
    RestoringOld,
}

impl ManagedRestoreJournal {
    fn from_plan(plan: &ManagedRestorePlan) -> Self {
        Self {
            version: 1,
            expected_head: plan.expected_head.clone(),
            source_commit: plan.source_commit.clone(),
            new_tree: plan.new_tree_hash.clone(),
            new_commit: plan.new_commit_hash.clone(),
            changes: plan.changes.clone(),
            progress: vec![RestoreProgress::Pending; plan.changes.len()],
        }
    }

    fn validate(&self) -> Result<()> {
        if self.version != 1
            || !crate::cas::is_hash(&self.expected_head)
            || !crate::cas::is_hash(&self.source_commit)
            || !crate::cas::is_hash(&self.new_tree)
            || !crate::cas::is_hash(&self.new_commit)
            || self.changes.is_empty()
            || self.progress.len() != self.changes.len()
        {
            return Err(restore_incomplete("managed restore journal is malformed"));
        }
        let mut previous: Option<&str> = None;
        for change in &self.changes {
            validate_restore_path(&change.path)?;
            if previous.is_some_and(|value| value >= change.path.as_str()) {
                return Err(restore_incomplete(
                    "managed restore journal changes must be strictly sorted",
                ));
            }
            if change.old_raw_hash == change.new_raw_hash {
                return Err(restore_incomplete(
                    "managed restore journal contains a no-op change",
                ));
            }
            for hash in [&change.old_raw_hash, &change.new_raw_hash]
                .into_iter()
                .flatten()
            {
                if !crate::cas::is_hash(hash) {
                    return Err(restore_incomplete(
                        "managed restore journal contains an invalid raw hash",
                    ));
                }
            }
            previous = Some(&change.path);
        }
        Ok(())
    }

    fn bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let value =
            serde_json::to_value(self).map_err(|error| KioError::schema(error.to_string()))?;
        let bytes = canonical_json_bytes(&value)?;
        if bytes.len() as u64 > MAX_MANAGED_RESTORE_JOURNAL_BYTES {
            return Err(restore_incomplete(
                "managed restore journal exceeds its size limit",
            ));
        }
        Ok(bytes)
    }
}

impl Repository {
    /// Construct a read-only restore plan.  It accepts only an existing,
    /// non-shallow ancestor of the current HEAD and never writes CAS, working
    /// files, refs, or journals.
    pub fn plan_restore(&self, request: ManagedRestoreRequest) -> Result<ManagedRestorePlan> {
        self.ensure_no_pending_publication()?;
        self.ensure_no_pending_managed_restore()?;
        let expected_head = self
            .head_commit_hash()?
            .ok_or_else(|| KioError::invalid_usage("managed restore requires an existing HEAD"))?;
        let head_commit = self.read_commit(&expected_head).map_err(|error| {
            if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" {
                KioError::commit_shallow(
                    "HEAD commit is shallow and cannot be restored",
                    expected_head.clone(),
                )
            } else {
                error
            }
        })?;
        let head_tree = self.read_tree(&head_commit.tree).map_err(|error| {
            if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" {
                KioError::commit_shallow(
                    "HEAD tree is shallow and cannot be restored",
                    expected_head.clone(),
                )
            } else {
                error
            }
        })?;
        ensure_reachable_ancestor(self, &expected_head, &request.source_commit)?;
        let source = self.read_commit(&request.source_commit).map_err(|error| {
            if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" {
                KioError::commit_shallow(
                    "restore source commit is shallow",
                    request.source_commit.clone(),
                )
            } else {
                error
            }
        })?;
        let source_tree = self.read_tree(&source.tree).map_err(|error| {
            if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" {
                KioError::commit_shallow(
                    "restore source tree is shallow",
                    request.source_commit.clone(),
                )
            } else {
                error
            }
        })?;

        let head_entries = entries_by_path(&head_tree)?;
        let source_entries = entries_by_path(&source_tree)?;
        let delete_missing = requested_delete_paths(&request, &source_entries)?;
        let requested_paths =
            requested_restore_paths(&request, &head_entries, &source_entries, &delete_missing)?;
        let mut target_entries = head_entries.clone();
        let mut changes = Vec::new();
        let selected_working_hashes = requested_paths
            .union(&delete_missing)
            .map(|path| {
                (
                    path.clone(),
                    head_entries.get(path).map(|entry| entry.raw_hash.clone()),
                )
            })
            .collect::<Vec<_>>();
        let mut restored_bytes = 0_u64;
        for path in requested_paths.union(&delete_missing) {
            let old = head_entries.get(path).cloned();
            let new = if delete_missing.contains(path) {
                None
            } else {
                source_entries
                    .get(path)
                    .cloned()
                    // An absent source path never implies deletion.  The
                    // caller must name it in `delete_missing` explicitly.
                    .or_else(|| old.clone())
            };
            // Managed restore materializes user raw bytes only.  A historical
            // normalization reference must never manufacture a journaled
            // working-file mutation when the selected raw bytes are unchanged.
            if old.as_ref().map(|entry| &entry.raw_hash)
                == new.as_ref().map(|entry| &entry.raw_hash)
            {
                continue;
            }
            match &new {
                Some(entry) => {
                    let bytes = ensure_raw_present(self, &entry.raw_hash)?;
                    restored_bytes = restored_bytes.checked_add(bytes).ok_or_else(|| {
                        restore_incomplete("managed restore source bytes overflow")
                    })?;
                    if restored_bytes > MAX_MANAGED_RESTORE_TOTAL_BYTES {
                        return Err(restore_incomplete(
                            "managed restore source bytes exceed the aggregate limit",
                        ));
                    }
                    target_entries.insert(path.clone(), entry.clone());
                }
                None => {
                    target_entries.remove(path);
                }
            }
            changes.push(ManagedRestoreChange {
                path: path.clone(),
                old_raw_hash: old.map(|entry| entry.raw_hash),
                new_raw_hash: new.map(|entry| entry.raw_hash),
            });
        }
        changes.sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
        if changes.is_empty() {
            return Ok(ManagedRestorePlan {
                expected_head,
                source_commit: request.source_commit,
                new_tree: head_tree.clone(),
                new_tree_hash: head_commit.tree.clone(),
                new_commit: head_commit,
                new_commit_hash: String::new(),
                selected_working_hashes,
                changes,
            });
        }
        let new_tree = build_tree_with_chunking_config(
            target_entries.into_values().collect(),
            head_tree.chunking_config_hash.clone(),
        )?;
        let new_tree_hash = tree_hash(&new_tree)?;
        let (tool_lock_hash, _) = self.tool_lock_identity()?;
        let stats = commit_stats(Some(&head_tree), &new_tree);
        let paths = changes.iter().map(|change| change.path.clone()).collect();
        let new_commit = CommitObject::new_restored(
            new_tree_hash.clone(),
            expected_head.clone(),
            request.created_at,
            request.message,
            tool_lock_hash,
            stats,
            crate::dag::RestoreProvenance {
                source_commit: request.source_commit.clone(),
                paths,
            },
        )?;
        let value = serde_json::to_value(&new_commit)
            .map_err(|error| KioError::schema(error.to_string()))?;
        let new_commit_hash = crate::cas::hash_json(&value)?;
        Ok(ManagedRestorePlan {
            expected_head,
            source_commit: request.source_commit,
            new_tree,
            new_tree_hash,
            new_commit,
            new_commit_hash,
            selected_working_hashes,
            changes,
        })
    }

    /// Apply a plan after repeatedly checking the caller's current policy and
    /// authority.  The callback must be side-effect free; a rejection leaves a
    /// durable restore journal for explicit recovery once working bytes moved.
    pub fn apply_restore<F>(
        &self,
        plan: &ManagedRestorePlan,
        mut validate_current_authority: F,
    ) -> Result<ManagedRestoreOutcome>
    where
        F: FnMut() -> Result<()>,
    {
        let _lock = self.lock_store()?;
        self.ensure_no_pending_publication()?;
        self.ensure_no_pending_managed_restore()?;
        validate_current_authority()?;
        self.revalidate_restore_working_hashes(plan)?;
        if plan.is_noop() {
            if self.head_commit_hash()?.as_deref() != Some(plan.expected_head.as_str()) {
                return Err(restore_conflict("HEAD changed after restore planning"));
            }
            return Ok(ManagedRestoreOutcome {
                noop: true,
                commit_hash: None,
                tree_hash: plan.new_tree_hash.clone(),
                restored_paths: Vec::new(),
            });
        }
        validate_plan(self, plan)?;

        let tree_value = serde_json::to_value(&plan.new_tree)
            .map_err(|error| KioError::schema(error.to_string()))?;
        let (stored_tree_hash, _) = self.store.write_json(ObjectKind::Tree, &tree_value)?;
        if stored_tree_hash != plan.new_tree_hash {
            return Err(restore_incomplete(
                "managed restore tree hash changed before publication",
            ));
        }
        let commit_value = serde_json::to_value(&plan.new_commit)
            .map_err(|error| KioError::schema(error.to_string()))?;
        let (stored_commit_hash, _) = self.store.write_json(ObjectKind::Commit, &commit_value)?;
        if stored_commit_hash != plan.new_commit_hash {
            return Err(restore_incomplete(
                "managed restore commit hash changed before publication",
            ));
        }

        validate_current_authority()?;
        let mut journal = ManagedRestoreJournal::from_plan(plan);
        let mut journal_bytes = journal.bytes()?;
        self.store_directory()?.write_atomic(
            Path::new(MANAGED_RESTORE_JOURNAL_LEAF),
            &journal_bytes,
            Publication::CreateOnly,
        )?;
        self.store_directory()?.sync()?;
        crate::durability::checkpoint(crate::durability::DurabilityPoint::RestoreJournal)?;

        for index in 0..plan.changes.len() {
            validate_current_authority()?;
            self.apply_restore_change_with_progress(
                &plan.changes[index],
                index,
                &mut journal,
                &mut journal_bytes,
            )?;
        }
        crate::durability::checkpoint(crate::durability::DurabilityPoint::RestoreWorkingBytes)?;
        self.revalidate_final_restore_working_hashes(plan)?;
        validate_current_authority()?;
        let head = self.read_commit(&plan.expected_head)?;
        let manifest = self.manifest_value(&plan.new_tree, Some(&self.read_tree(&head.tree)?))?;
        self.publish_with_journal(
            Some(&plan.expected_head),
            &plan.new_commit_hash,
            &plan.new_commit,
            manifest,
        )?;
        crate::durability::checkpoint(crate::durability::DurabilityPoint::RestoreHead)?;
        self.remove_managed_restore_journal(&journal_bytes)?;
        Ok(ManagedRestoreOutcome {
            noop: false,
            commit_hash: Some(plan.new_commit_hash.clone()),
            tree_hash: plan.new_tree_hash.clone(),
            restored_paths: plan
                .changes
                .iter()
                .map(|change| change.path.clone())
                .collect(),
        })
    }

    /// Explicitly resolve a pending managed restore.  It never advances HEAD
    /// from the publication journal while rolling working bytes back.
    pub fn recover_managed_restore(&self) -> Result<bool> {
        let _lock = self.lock_store()?;
        let atomic_recovered = self.recover_atomic_storage()?;
        let Some((mut journal, mut journal_bytes)) = self.managed_restore_journal()? else {
            return Ok(atomic_recovered);
        };
        let current_head = self.head_commit_hash()?;
        self.validate_restore_journal_authority(&journal)?;
        if current_head.as_deref() == Some(journal.new_commit.as_str()) {
            self.validate_published_restore_child(&journal)?;
            if self.publication_journal()?.is_some() {
                self.finish_exact_restore_publication(&journal)?;
            }
            self.remove_managed_restore_journal(&journal_bytes)?;
            return Ok(true);
        }
        if current_head.as_deref() != Some(journal.expected_head.as_str()) {
            return Err(restore_incomplete(
                "managed restore journal does not match current HEAD",
            ));
        }
        if self.publication_journal()?.is_some() {
            self.remove_exact_restore_publication_journal(&journal)?;
        }
        for index in (0..journal.changes.len()).rev() {
            let change = journal.changes[index].clone();
            self.rollback_restore_change_with_progress(
                &change,
                index,
                &mut journal,
                &mut journal_bytes,
            )?;
        }
        self.remove_managed_restore_journal(&journal_bytes)?;
        Ok(true)
    }

    /// Refuse ordinary mutation while a managed restore journal exists.  This
    /// intentionally does not recover it: callers that need restore recovery
    /// must invoke [`Self::recover_managed_restore`] explicitly.
    pub fn ensure_no_pending_managed_restore(&self) -> Result<()> {
        reject_pending_managed_restore(&self.store_directory()?)
    }

    fn managed_restore_journal(&self) -> Result<Option<(ManagedRestoreJournal, Vec<u8>)>> {
        let Some(bytes) = self.store_directory()?.read_optional(
            Path::new(MANAGED_RESTORE_JOURNAL_LEAF),
            MAX_MANAGED_RESTORE_JOURNAL_BYTES,
        )?
        else {
            return Ok(None);
        };
        let journal: ManagedRestoreJournal = serde_json::from_slice(&bytes)
            .map_err(|_| restore_incomplete("managed restore journal is not valid JSON"))?;
        journal.validate()?;
        Ok(Some((journal, bytes)))
    }

    fn remove_managed_restore_journal(&self, expected: &[u8]) -> Result<()> {
        self.store_directory()?.quarantine_then_remove(
            Path::new(MANAGED_RESTORE_JOURNAL_LEAF),
            expected,
            MAX_MANAGED_RESTORE_JOURNAL_BYTES,
        )
    }

    fn write_working_atomic(
        &self,
        relative: &Path,
        bytes: &[u8],
        publication: Publication,
    ) -> Result<()> {
        self.root_directory()?.write_atomic_with_owner(
            &self.store_directory()?,
            relative,
            bytes,
            publication,
        )
    }

    fn remove_working_atomic(
        &self,
        relative: &Path,
        expected: &[u8],
        max_bytes: u64,
    ) -> Result<()> {
        self.root_directory()?.quarantine_then_remove_with_owner(
            &self.store_directory()?,
            relative,
            expected,
            max_bytes,
        )
    }

    /// Recover private one-file operations before replaying their owning
    /// restore/publication journal. Both permitted retained roots are explicit.
    pub fn recover_atomic_storage(&self) -> Result<bool> {
        let _lock = self.lock_store()?;
        let owner = self.store_directory()?;
        let root = self.root_directory()?;
        owner.recover_atomic(&[&owner, &root])
    }

    fn root_directory(&self) -> Result<StoreDirectory> {
        let root = self.bound_root.as_deref().ok_or_else(|| {
            KioError::invalid_usage("managed restore requires retained scope authority")
        })?;
        StoreDirectory::from_retained(
            root.try_clone()
                .map_err(|error| KioError::io(error.to_string(), "."))?,
            self.canonical_root.clone(),
        )
    }

    fn revalidate_restore_working_hashes(&self, plan: &ManagedRestorePlan) -> Result<()> {
        if self.head_commit_hash()?.as_deref() != Some(plan.expected_head.as_str()) {
            return Err(restore_conflict("HEAD changed after restore planning"));
        }
        for (path, expected) in &plan.selected_working_hashes {
            if self.working_raw_hash(path)? != *expected {
                return Err(restore_conflict(
                    "working file changed after restore planning",
                ));
            }
        }
        for change in &plan.changes {
            if self.working_raw_hash(&change.path)? != change.old_raw_hash {
                return Err(restore_conflict(
                    "working file changed after restore planning",
                ));
            }
        }
        Ok(())
    }

    fn revalidate_final_restore_working_hashes(&self, plan: &ManagedRestorePlan) -> Result<()> {
        if self.head_commit_hash()?.as_deref() != Some(plan.expected_head.as_str()) {
            return Err(restore_conflict("HEAD changed before restore publication"));
        }
        for change in &plan.changes {
            if self.working_raw_hash(&change.path)? != change.new_raw_hash {
                return Err(restore_incomplete(
                    "working file changed during managed restore before publication",
                ));
            }
        }
        Ok(())
    }

    fn working_raw_hash(&self, path: &str) -> Result<Option<String>> {
        validate_restore_path(path)?;
        let directory = self.root_directory()?;
        let Some(bytes) =
            directory.read_optional(Path::new(path), MAX_MANAGED_RESTORE_FILE_BYTES)?
        else {
            return Ok(None);
        };
        Ok(Some(hash_bytes(&bytes)))
    }

    fn raw_bytes(&self, raw_hash: &str) -> Result<Vec<u8>> {
        let metadata = self.store.inspect_object(ObjectKind::Raw, raw_hash)?;
        if metadata.size_bytes > MAX_MANAGED_RESTORE_FILE_BYTES {
            return Err(restore_incomplete(
                "restore file exceeds the managed restore byte limit",
            ));
        }
        let object = self.store.read_by_hash(raw_hash)?;
        if object.kind != ObjectKind::Raw {
            return Err(restore_incomplete(
                "restore raw hash does not identify a raw object",
            ));
        }
        if object.bytes.len() as u64 != metadata.size_bytes {
            return Err(restore_incomplete(
                "restore raw object changed while it was materialized",
            ));
        }
        Ok(object.bytes)
    }

    fn persist_restore_progress(
        &self,
        journal: &ManagedRestoreJournal,
        bytes: &mut Vec<u8>,
    ) -> Result<()> {
        let next = journal.bytes()?;
        self.store_directory()?.write_atomic(
            Path::new(MANAGED_RESTORE_JOURNAL_LEAF),
            &next,
            Publication::Replace,
        )?;
        self.store_directory()?.sync()?;
        *bytes = next;
        Ok(())
    }

    fn apply_restore_change_with_progress(
        &self,
        change: &ManagedRestoreChange,
        index: usize,
        journal: &mut ManagedRestoreJournal,
        bytes: &mut Vec<u8>,
    ) -> Result<()> {
        if self.working_raw_hash(&change.path)? != change.old_raw_hash {
            return Err(restore_conflict(
                "working file changed before restore mutation",
            ));
        }
        if let Some(old) = &change.old_raw_hash {
            journal.progress[index] = RestoreProgress::RemovingOld;
            self.persist_restore_progress(journal, bytes)?;
            let old_bytes = self.raw_bytes(old)?;
            self.remove_working_atomic(
                Path::new(&change.path),
                &old_bytes,
                MAX_MANAGED_RESTORE_FILE_BYTES,
            )?;
            journal.progress[index] = RestoreProgress::OldRemoved;
            self.persist_restore_progress(journal, bytes)?;
        }
        if let Some(new) = &change.new_raw_hash {
            journal.progress[index] = RestoreProgress::PublishingNew;
            self.persist_restore_progress(journal, bytes)?;
            let new_bytes = self.raw_bytes(new)?;
            self.write_working_atomic(
                Path::new(&change.path),
                &new_bytes,
                Publication::CreateOnly,
            )?;
        }
        journal.progress[index] = RestoreProgress::Applied;
        self.persist_restore_progress(journal, bytes)?;
        Ok(())
    }

    #[cfg(test)]
    fn apply_restore_change(&self, change: &ManagedRestoreChange) -> Result<()> {
        if self.working_raw_hash(&change.path)? != change.old_raw_hash {
            return Err(restore_conflict(
                "working file changed before restore mutation",
            ));
        }
        if let Some(old) = &change.old_raw_hash {
            self.remove_working_atomic(
                Path::new(&change.path),
                &self.raw_bytes(old)?,
                MAX_MANAGED_RESTORE_FILE_BYTES,
            )?;
        }
        if let Some(new) = &change.new_raw_hash {
            self.write_working_atomic(
                Path::new(&change.path),
                &self.raw_bytes(new)?,
                Publication::CreateOnly,
            )?;
        }
        Ok(())
    }

    fn rollback_restore_change_with_progress(
        &self,
        change: &ManagedRestoreChange,
        index: usize,
        journal: &mut ManagedRestoreJournal,
        bytes: &mut Vec<u8>,
    ) -> Result<()> {
        let current = self.working_raw_hash(&change.path)?;
        if current == change.old_raw_hash {
            if journal.progress[index] != RestoreProgress::Pending {
                journal.progress[index] = RestoreProgress::Pending;
                self.persist_restore_progress(journal, bytes)?;
            }
            return Ok(());
        }
        let phase = journal.progress[index];
        if matches!(
            phase,
            RestoreProgress::OldRemoved | RestoreProgress::RollbackNewRemoved
        ) && current.is_none()
        {
            if phase == RestoreProgress::RollbackNewRemoved && change.old_raw_hash.is_none() {
                journal.progress[index] = RestoreProgress::Pending;
                self.persist_restore_progress(journal, bytes)?;
                return Ok(());
            }
            let old = change.old_raw_hash.as_ref().ok_or_else(|| {
                if phase == RestoreProgress::RollbackNewRemoved {
                    restore_incomplete("rollback-new-removed phase has no old raw")
                } else {
                    restore_incomplete("old-removed phase has no old raw")
                }
            })?;
            // This intent must be stable before creation.  A later crash with
            // `restoring_old` and an absent path is deliberately ambiguous:
            // it could be a failed create or a post-crash user deletion.
            journal.progress[index] = RestoreProgress::RestoringOld;
            self.persist_restore_progress(journal, bytes)?;
            let old_bytes = self.raw_bytes(old)?;
            self.write_working_atomic(
                Path::new(&change.path),
                &old_bytes,
                Publication::CreateOnly,
            )?;
            journal.progress[index] = RestoreProgress::Pending;
            self.persist_restore_progress(journal, bytes)?;
            return Ok(());
        }
        // An applied deletion has no new bytes.  Its durable applied phase is
        // the evidence that the absence is operation-owned, so it may begin
        // the same explicitly journaled old-byte restoration.
        if change.new_raw_hash.is_none() && phase == RestoreProgress::Applied && current.is_none() {
            let old = change
                .old_raw_hash
                .as_ref()
                .ok_or_else(|| restore_incomplete("applied deletion phase has no old raw"))?;
            journal.progress[index] = RestoreProgress::RestoringOld;
            self.persist_restore_progress(journal, bytes)?;
            self.write_working_atomic(
                Path::new(&change.path),
                &self.raw_bytes(old)?,
                Publication::CreateOnly,
            )?;
            journal.progress[index] = RestoreProgress::Pending;
            self.persist_restore_progress(journal, bytes)?;
            return Ok(());
        }
        if current != change.new_raw_hash
            || !matches!(
                phase,
                RestoreProgress::PublishingNew
                    | RestoreProgress::Applied
                    | RestoreProgress::RollbackRemovingNew
            )
        {
            return Err(restore_incomplete(
                "working file changed by another writer; manual managed restore recovery is required",
            ));
        }
        debug_assert!(change.new_raw_hash.is_some());
        if current == change.new_raw_hash {
            let new = change
                .new_raw_hash
                .as_ref()
                .expect("current can equal new only when new is present");
            let new_bytes = self.raw_bytes(new)?;
            journal.progress[index] = RestoreProgress::RollbackRemovingNew;
            self.persist_restore_progress(journal, bytes)?;
            self.remove_working_atomic(
                Path::new(&change.path),
                &new_bytes,
                MAX_MANAGED_RESTORE_FILE_BYTES,
            )?;
            journal.progress[index] = RestoreProgress::RollbackNewRemoved;
            self.persist_restore_progress(journal, bytes)?;
        }
        if let Some(old) = &change.old_raw_hash {
            journal.progress[index] = RestoreProgress::RestoringOld;
            self.persist_restore_progress(journal, bytes)?;
            let old_bytes = self.raw_bytes(old)?;
            self.write_working_atomic(
                Path::new(&change.path),
                &old_bytes,
                Publication::CreateOnly,
            )?;
        }
        journal.progress[index] = RestoreProgress::Pending;
        self.persist_restore_progress(journal, bytes)?;
        Ok(())
    }

    fn validate_published_restore_child(&self, journal: &ManagedRestoreJournal) -> Result<()> {
        self.validate_restore_journal_authority(journal)
    }

    /// Verify every immutable and mutable-history binding needed before
    /// recovery changes a working file.  The journal is merely an intent; it
    /// never authorizes rollback by itself.
    fn validate_restore_journal_authority(&self, journal: &ManagedRestoreJournal) -> Result<()> {
        journal.validate()?;
        ensure_reachable_ancestor(self, &journal.expected_head, &journal.source_commit)?;
        let expected_commit = self.read_commit(&journal.expected_head)?;
        let expected_tree = self.read_tree(&expected_commit.tree)?;
        let source_commit = self.read_commit(&journal.source_commit)?;
        let source_tree = self.read_tree(&source_commit.tree)?;
        let commit = self.read_commit(&journal.new_commit)?;
        if commit.commit_type != CommitType::Restored
            || commit.parent.as_deref() != Some(journal.expected_head.as_str())
            || commit.tree != journal.new_tree
            || commit.restore_provenance.as_ref().is_none_or(|value| {
                value.source_commit != journal.source_commit
                    || value.paths
                        != journal
                            .changes
                            .iter()
                            .map(|change| change.path.clone())
                            .collect::<Vec<_>>()
            })
        {
            return Err(restore_incomplete(
                "managed restore child does not match journal authority",
            ));
        }
        let restored_tree = self.read_tree(&journal.new_tree)?;
        let mut expected_entries = entries_by_path(&expected_tree)?;
        let source_entries = entries_by_path(&source_tree)?;
        for change in &journal.changes {
            let old = expected_entries
                .get(&change.path)
                .map(|entry| entry.raw_hash.clone());
            let source = source_entries.get(&change.path);
            let new = restored_tree
                .entries
                .iter()
                .find(|entry| entry.path == change.path)
                .map(|entry| entry.raw_hash.clone());
            if old != change.old_raw_hash
                || new != change.new_raw_hash
                || change.old_raw_hash == change.new_raw_hash
                || match (&change.new_raw_hash, source) {
                    (Some(raw_hash), Some(entry)) => entry.raw_hash != *raw_hash,
                    (None, None) => false,
                    _ => true,
                }
            {
                return Err(restore_incomplete(
                    "managed restore journal changes do not match source and tree diff",
                ));
            }
            match source {
                Some(entry) => {
                    expected_entries.insert(change.path.clone(), entry.clone());
                }
                None => {
                    expected_entries.remove(&change.path);
                }
            }
        }
        let expected_restored = build_tree_with_chunking_config(
            expected_entries.into_values().collect(),
            expected_tree.chunking_config_hash,
        )?;
        if expected_restored != restored_tree || tree_hash(&restored_tree)? != journal.new_tree {
            return Err(restore_incomplete(
                "managed restore journal tree is not the exact expected child tree",
            ));
        }
        Ok(())
    }

    fn finish_exact_restore_publication(&self, journal: &ManagedRestoreJournal) -> Result<()> {
        let publication = self
            .publication_journal()?
            .ok_or_else(|| restore_incomplete("managed restore publication journal disappeared"))?;
        if publication.expected_head.as_deref() != Some(journal.expected_head.as_str())
            || publication.new_head != journal.new_commit
        {
            return Err(restore_incomplete(
                "publication journal is not this managed restore",
            ));
        }
        let tree = self.read_tree(&journal.new_tree)?;
        super::validate_manifest_matches_tree(&publication.manifest, &tree)?;
        self.write_manifest_value(&publication.manifest)?;
        self.remove_publication_journal()?;
        Ok(())
    }

    fn remove_exact_restore_publication_journal(
        &self,
        journal: &ManagedRestoreJournal,
    ) -> Result<()> {
        let publication = self
            .publication_journal()?
            .ok_or_else(|| restore_incomplete("managed restore publication journal disappeared"))?;
        if publication.expected_head.as_deref() != Some(journal.expected_head.as_str())
            || publication.new_head != journal.new_commit
        {
            return Err(restore_incomplete(
                "publication journal is not this managed restore",
            ));
        }
        let value = serde_json::to_value(&publication)
            .map_err(|error| KioError::schema(error.to_string()))?;
        let bytes = canonical_json_bytes(&value)?;
        self.store_directory()?.quarantine_then_remove(
            Path::new(super::PUBLICATION_JOURNAL_LEAF),
            &bytes,
            super::MAX_PUBLICATION_JOURNAL_BYTES,
        )
    }
}

/// Shared destructive-operation barrier.  Presence alone is sufficient: an
/// unreadable, malformed, or oversized journal must block GC/purge just as a
/// valid interrupted restore does.
pub(crate) fn reject_pending_managed_restore(directory: &StoreDirectory) -> Result<()> {
    if directory
        .read_optional(
            Path::new(MANAGED_RESTORE_JOURNAL_LEAF),
            MAX_MANAGED_RESTORE_JOURNAL_BYTES,
        )?
        .is_some()
    {
        return Err(restore_incomplete(
            "an interrupted managed restore requires explicit recovery",
        ));
    }
    Ok(())
}

fn validate_plan(repo: &Repository, plan: &ManagedRestorePlan) -> Result<()> {
    if plan.changes.is_empty()
        || repo.head_commit_hash()?.as_deref() != Some(plan.expected_head.as_str())
        || plan.new_commit_hash.is_empty()
        || plan.new_commit.commit_type != CommitType::Restored
        || plan.new_commit.parent.as_deref() != Some(plan.expected_head.as_str())
        || plan.new_commit.tree != plan.new_tree_hash
        || plan
            .new_commit
            .restore_provenance
            .as_ref()
            .is_none_or(|value| {
                value.source_commit != plan.source_commit
                    || value.paths
                        != plan
                            .changes
                            .iter()
                            .map(|change| change.path.clone())
                            .collect::<Vec<_>>()
            })
        || tree_hash(&plan.new_tree)? != plan.new_tree_hash
    {
        return Err(restore_conflict(
            "managed restore plan no longer has valid authority",
        ));
    }
    for change in &plan.changes {
        validate_restore_path(&change.path)?;
        if let Some(raw_hash) = &change.new_raw_hash {
            ensure_raw_present(repo, raw_hash)?;
        }
    }
    Ok(())
}

fn ensure_reachable_ancestor(repo: &Repository, head: &str, source: &str) -> Result<()> {
    let mut next = Some(head.to_owned());
    for _ in 0..=crate::history::DEFAULT_MAX_HISTORY_COMMITS {
        let Some(hash) = next else {
            return Err(KioError::invalid_usage(
                "restore source commit is not an ancestor of current HEAD",
            ));
        };
        if hash == source {
            return Ok(());
        }
        next = repo.read_commit(&hash)?.parent;
    }
    Err(restore_incomplete(
        "restore ancestry exceeds the history bound",
    ))
}

fn entries_by_path(tree: &TreeObject) -> Result<BTreeMap<String, TreeEntry>> {
    let mut entries = BTreeMap::new();
    for entry in &tree.entries {
        validate_restore_path(&entry.path)?;
        if entries.insert(entry.path.clone(), entry.clone()).is_some() {
            return Err(restore_incomplete("restore tree has duplicate path"));
        }
    }
    Ok(entries)
}

fn requested_restore_paths(
    request: &ManagedRestoreRequest,
    head: &BTreeMap<String, TreeEntry>,
    source: &BTreeMap<String, TreeEntry>,
    delete_missing: &BTreeSet<String>,
) -> Result<BTreeSet<String>> {
    let mut paths = BTreeSet::new();
    match &request.paths {
        Some(requested) => {
            for path in requested {
                validate_restore_path(path)?;
                if !source.contains_key(path) && !delete_missing.contains(path) {
                    return Err(KioError::invalid_usage(
                        "explicit restore path is absent from the source commit and is not named by delete_missing",
                    ));
                }
                paths.insert(path.clone());
            }
        }
        None => {
            paths.extend(source.keys().cloned());
            paths.extend(head.keys().cloned());
        }
    }
    Ok(paths)
}

fn requested_delete_paths(
    request: &ManagedRestoreRequest,
    source: &BTreeMap<String, TreeEntry>,
) -> Result<BTreeSet<String>> {
    let mut paths = BTreeSet::new();
    for path in &request.delete_missing {
        validate_restore_path(path)?;
        if source.contains_key(path) {
            return Err(KioError::invalid_usage(
                "delete_missing path exists in the source commit",
            ));
        }
        paths.insert(path.clone());
    }
    Ok(paths)
}

fn validate_restore_path(path: &str) -> Result<()> {
    if !is_materializable_direct_child(path) || path == ".kio" || path.starts_with(".kio") {
        return Err(KioError::path(
            "managed restore path must be a user direct-child path outside .kio",
            path.to_owned(),
        ));
    }
    Ok(())
}

fn ensure_raw_present(repo: &Repository, raw_hash: &str) -> Result<u64> {
    let purge = repo.purge_state()?;
    if purge
        .read_tombstone(raw_hash)?
        .is_some_and(|record| record.is_active())
        || purge
            .read_erase_receipt(raw_hash)?
            .is_some_and(|record| record.is_active())
        || purge.barrier_blocks(raw_hash)?
    {
        return Err(restore_incomplete(
            "restore source raw object is retired, erased, or under an active purge barrier",
        ));
    }
    let object = repo
        .store
        .inspect_object(ObjectKind::Raw, raw_hash)
        .map_err(|error| {
            if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" {
                restore_incomplete("restore source raw object is missing or purged")
            } else {
                error
            }
        })?;
    if object.size_bytes > MAX_MANAGED_RESTORE_FILE_BYTES {
        return Err(restore_incomplete(
            "restore source file exceeds the managed restore byte limit",
        ));
    }
    Ok(object.size_bytes)
}

fn restore_conflict(message: impl Into<String>) -> KioError {
    KioError::new(
        "KIO-E-MANAGED-RESTORE-CONFLICT-001",
        message,
        json!({}),
        ExitCode::PartialFailure,
    )
}

fn restore_incomplete(message: impl Into<String>) -> KioError {
    KioError::new(
        "KIO-E-MANAGED-RESTORE-RECOVERY-REQUIRED-001",
        message,
        json!({ "journal": MANAGED_RESTORE_JOURNAL_LEAF }),
        ExitCode::PartialFailure,
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::{
        MANAGED_RESTORE_JOURNAL_LEAF, ManagedRestoreJournal, ManagedRestoreRequest, ObjectKind,
        Publication, Repository, RestoreProgress,
    };

    fn snapshot(repo: &Repository, at: &str) -> String {
        repo.snapshot(Some("test"), Some(at))
            .unwrap()
            .commit_hash
            .unwrap()
    }

    fn stage_restore_journal(
        repo: &Repository,
        plan: &super::ManagedRestorePlan,
        phase: RestoreProgress,
    ) {
        repo.store
            .write_json(
                ObjectKind::Tree,
                &serde_json::to_value(&plan.new_tree).unwrap(),
            )
            .unwrap();
        repo.store
            .write_json(
                ObjectKind::Commit,
                &serde_json::to_value(&plan.new_commit).unwrap(),
            )
            .unwrap();
        let mut journal = ManagedRestoreJournal::from_plan(plan);
        journal.progress[0] = phase;
        repo.store_directory()
            .unwrap()
            .write_atomic(
                Path::new(MANAGED_RESTORE_JOURNAL_LEAF),
                &journal.bytes().unwrap(),
                Publication::CreateOnly,
            )
            .unwrap();
    }

    #[test]
    fn selected_restore_is_a_linear_child_and_preserves_unselected_dirty_file() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), b"one").unwrap();
        fs::write(directory.path().join("b.txt"), b"base").unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        let source = snapshot(&repo, "2026-01-01T00:00:00Z");
        fs::write(directory.path().join("a.txt"), b"two").unwrap();
        fs::write(directory.path().join("b.txt"), b"head").unwrap();
        let head = snapshot(&repo, "2026-01-02T00:00:00Z");
        fs::write(directory.path().join("b.txt"), b"dirty").unwrap();

        let plan = repo
            .plan_restore(ManagedRestoreRequest::new(
                source.clone(),
                Some(vec!["a.txt".to_owned()]),
                Vec::new(),
                "restore a",
                "2026-01-03T00:00:00Z",
            ))
            .unwrap();
        assert_eq!(plan.expected_head(), head);
        assert_eq!(plan.changes().len(), 1);
        let outcome = repo.apply_restore(&plan, || Ok(())).unwrap();
        let restored = outcome.commit_hash.unwrap();
        let commit = repo.read_commit(&restored).unwrap();
        assert_eq!(commit.parent.as_deref(), Some(head.as_str()));
        assert_eq!(commit.restore_provenance.unwrap().source_commit, source);
        assert_eq!(fs::read(directory.path().join("a.txt")).unwrap(), b"one");
        assert_eq!(fs::read(directory.path().join("b.txt")).unwrap(), b"dirty");
    }

    #[test]
    fn source_absence_never_deletes_without_explicit_delete_missing() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), b"one").unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        let source = snapshot(&repo, "2026-01-01T00:00:00Z");
        fs::write(directory.path().join("b.txt"), b"head-only").unwrap();
        snapshot(&repo, "2026-01-02T00:00:00Z");

        let no_delete = repo
            .plan_restore(ManagedRestoreRequest::new(
                source.clone(),
                None,
                Vec::new(),
                "no delete",
                "2026-01-03T00:00:00Z",
            ))
            .unwrap();
        assert!(no_delete.is_noop());

        let delete = repo
            .plan_restore(ManagedRestoreRequest::new(
                source,
                None,
                vec!["b.txt".to_owned()],
                "delete b",
                "2026-01-03T00:00:00Z",
            ))
            .unwrap();
        assert_eq!(delete.changes().len(), 1);
        repo.apply_restore(&delete, || Ok(())).unwrap();
        assert!(!directory.path().join("b.txt").exists());
    }

    #[test]
    fn controls_and_dirty_selected_files_are_refused_before_writes() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), b"one").unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        let source = snapshot(&repo, "2026-01-01T00:00:00Z");
        fs::write(directory.path().join("a.txt"), b"two").unwrap();
        snapshot(&repo, "2026-01-02T00:00:00Z");
        fs::write(directory.path().join("a.txt"), b"dirty").unwrap();
        let dirty = repo
            .plan_restore(ManagedRestoreRequest::new(
                source.clone(),
                Some(vec!["a.txt".to_owned()]),
                Vec::new(),
                "restore a",
                "2026-01-03T00:00:00Z",
            ))
            .unwrap();
        assert!(repo.apply_restore(&dirty, || Ok(())).is_err());
        assert_eq!(fs::read(directory.path().join("a.txt")).unwrap(), b"dirty");

        let controls = repo.plan_restore(ManagedRestoreRequest::new(
            source,
            Some(vec![".kio-control".to_owned()]),
            Vec::new(),
            "bad",
            "2026-01-03T00:00:00Z",
        ));
        assert!(controls.is_err());
    }

    #[test]
    fn noop_restore_still_refuses_a_dirty_selected_path() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), b"one").unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        let head = snapshot(&repo, "2026-01-01T00:00:00Z");
        let plan = repo
            .plan_restore(ManagedRestoreRequest::new(
                head,
                Some(vec!["a.txt".to_owned()]),
                Vec::new(),
                "noop",
                "2026-01-02T00:00:00Z",
            ))
            .unwrap();
        assert!(plan.is_noop());
        fs::write(directory.path().join("a.txt"), b"dirty").unwrap();
        assert!(repo.apply_restore(&plan, || Ok(())).is_err());
        assert_eq!(fs::read(directory.path().join("a.txt")).unwrap(), b"dirty");
    }

    #[test]
    fn explicit_recovery_rolls_back_only_the_journaled_new_bytes_before_publication() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), b"one").unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        let source = snapshot(&repo, "2026-01-01T00:00:00Z");
        fs::write(directory.path().join("a.txt"), b"two").unwrap();
        let head = snapshot(&repo, "2026-01-02T00:00:00Z");
        let plan = repo
            .plan_restore(ManagedRestoreRequest::new(
                source,
                Some(vec!["a.txt".to_owned()]),
                Vec::new(),
                "restore a",
                "2026-01-03T00:00:00Z",
            ))
            .unwrap();

        let tree = serde_json::to_value(&plan.new_tree).unwrap();
        repo.store.write_json(ObjectKind::Tree, &tree).unwrap();
        let commit = serde_json::to_value(&plan.new_commit).unwrap();
        repo.store.write_json(ObjectKind::Commit, &commit).unwrap();
        let mut journal = ManagedRestoreJournal::from_plan(&plan);
        journal.progress[0] = RestoreProgress::Applied;
        let bytes = journal.bytes().unwrap();
        repo.store_directory()
            .unwrap()
            .write_atomic(
                Path::new(MANAGED_RESTORE_JOURNAL_LEAF),
                &bytes,
                Publication::CreateOnly,
            )
            .unwrap();
        repo.apply_restore_change(&plan.changes[0]).unwrap();
        assert_eq!(fs::read(directory.path().join("a.txt")).unwrap(), b"one");

        assert!(repo.recover_managed_restore().unwrap());
        assert_eq!(
            repo.head_commit_hash().unwrap().as_deref(),
            Some(head.as_str())
        );
        assert_eq!(fs::read(directory.path().join("a.txt")).unwrap(), b"two");
        assert!(!repo.kio_dir().join(MANAGED_RESTORE_JOURNAL_LEAF).exists());
    }

    #[test]
    fn explicit_recovery_refills_the_durably_recorded_old_removed_gap() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), b"one").unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        let source = snapshot(&repo, "2026-01-01T00:00:00Z");
        fs::write(directory.path().join("a.txt"), b"two").unwrap();
        snapshot(&repo, "2026-01-02T00:00:00Z");
        let plan = repo
            .plan_restore(ManagedRestoreRequest::new(
                source,
                Some(vec!["a.txt".to_owned()]),
                Vec::new(),
                "restore a",
                "2026-01-03T00:00:00Z",
            ))
            .unwrap();
        let tree = serde_json::to_value(&plan.new_tree).unwrap();
        repo.store.write_json(ObjectKind::Tree, &tree).unwrap();
        let commit = serde_json::to_value(&plan.new_commit).unwrap();
        repo.store.write_json(ObjectKind::Commit, &commit).unwrap();
        let mut journal = ManagedRestoreJournal::from_plan(&plan);
        journal.progress[0] = RestoreProgress::OldRemoved;
        let bytes = journal.bytes().unwrap();
        repo.store_directory()
            .unwrap()
            .write_atomic(
                Path::new(MANAGED_RESTORE_JOURNAL_LEAF),
                &bytes,
                Publication::CreateOnly,
            )
            .unwrap();
        let old = repo
            .raw_bytes(plan.changes[0].old_raw_hash.as_deref().unwrap())
            .unwrap();
        repo.remove_working_atomic(
            Path::new("a.txt"),
            &old,
            super::MAX_MANAGED_RESTORE_FILE_BYTES,
        )
        .unwrap();
        assert!(!directory.path().join("a.txt").exists());

        assert!(repo.recover_managed_restore().unwrap());
        assert_eq!(fs::read(directory.path().join("a.txt")).unwrap(), b"two");
        assert!(!repo.kio_dir().join(MANAGED_RESTORE_JOURNAL_LEAF).exists());
    }

    #[test]
    fn publishing_new_with_an_absent_path_requires_manual_recovery() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), b"one").unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        let source = snapshot(&repo, "2026-01-01T00:00:00Z");
        fs::write(directory.path().join("a.txt"), b"two").unwrap();
        snapshot(&repo, "2026-01-02T00:00:00Z");
        let plan = repo
            .plan_restore(ManagedRestoreRequest::new(
                source,
                Some(vec!["a.txt".to_owned()]),
                Vec::new(),
                "restore a",
                "2026-01-03T00:00:00Z",
            ))
            .unwrap();
        stage_restore_journal(&repo, &plan, RestoreProgress::PublishingNew);
        let old = repo
            .raw_bytes(plan.changes[0].old_raw_hash.as_deref().unwrap())
            .unwrap();
        repo.remove_working_atomic(
            Path::new("a.txt"),
            &old,
            super::MAX_MANAGED_RESTORE_FILE_BYTES,
        )
        .unwrap();

        assert_eq!(
            repo.recover_managed_restore().unwrap_err().error_code(),
            "KIO-E-MANAGED-RESTORE-RECOVERY-REQUIRED-001"
        );
        assert!(!directory.path().join("a.txt").exists());
        assert!(repo.kio_dir().join(MANAGED_RESTORE_JOURNAL_LEAF).exists());
    }

    #[test]
    fn recovery_refuses_to_resurrect_after_new_bytes_were_applied_then_deleted() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), b"one").unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        let source = snapshot(&repo, "2026-01-01T00:00:00Z");
        fs::write(directory.path().join("a.txt"), b"two").unwrap();
        snapshot(&repo, "2026-01-02T00:00:00Z");
        let plan = repo
            .plan_restore(ManagedRestoreRequest::new(
                source,
                Some(vec!["a.txt".to_owned()]),
                Vec::new(),
                "restore a",
                "2026-01-03T00:00:00Z",
            ))
            .unwrap();
        stage_restore_journal(&repo, &plan, RestoreProgress::Applied);
        let old = repo
            .raw_bytes(plan.changes[0].old_raw_hash.as_deref().unwrap())
            .unwrap();
        repo.remove_working_atomic(
            Path::new("a.txt"),
            &old,
            super::MAX_MANAGED_RESTORE_FILE_BYTES,
        )
        .unwrap();
        let new = repo
            .raw_bytes(plan.changes[0].new_raw_hash.as_deref().unwrap())
            .unwrap();
        repo.write_working_atomic(Path::new("a.txt"), &new, Publication::CreateOnly)
            .unwrap();
        repo.remove_working_atomic(
            Path::new("a.txt"),
            &new,
            super::MAX_MANAGED_RESTORE_FILE_BYTES,
        )
        .unwrap();

        assert_eq!(
            repo.recover_managed_restore().unwrap_err().error_code(),
            "KIO-E-MANAGED-RESTORE-RECOVERY-REQUIRED-001"
        );
        assert!(!directory.path().join("a.txt").exists());
        assert!(repo.kio_dir().join(MANAGED_RESTORE_JOURNAL_LEAF).exists());
    }

    #[test]
    fn rollback_new_removed_phase_retries_without_overwriting_foreign_bytes() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), b"one").unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        let source = snapshot(&repo, "2026-01-01T00:00:00Z");
        fs::write(directory.path().join("a.txt"), b"two").unwrap();
        snapshot(&repo, "2026-01-02T00:00:00Z");
        let plan = repo
            .plan_restore(ManagedRestoreRequest::new(
                source,
                Some(vec!["a.txt".to_owned()]),
                Vec::new(),
                "restore a",
                "2026-01-03T00:00:00Z",
            ))
            .unwrap();
        stage_restore_journal(&repo, &plan, RestoreProgress::RollbackNewRemoved);
        let old = repo
            .raw_bytes(plan.changes[0].old_raw_hash.as_deref().unwrap())
            .unwrap();
        repo.remove_working_atomic(
            Path::new("a.txt"),
            &old,
            super::MAX_MANAGED_RESTORE_FILE_BYTES,
        )
        .unwrap();
        let new = repo
            .raw_bytes(plan.changes[0].new_raw_hash.as_deref().unwrap())
            .unwrap();
        repo.write_working_atomic(Path::new("a.txt"), &new, Publication::CreateOnly)
            .unwrap();
        repo.remove_working_atomic(
            Path::new("a.txt"),
            &new,
            super::MAX_MANAGED_RESTORE_FILE_BYTES,
        )
        .unwrap();

        assert!(repo.recover_managed_restore().unwrap());
        assert_eq!(fs::read(directory.path().join("a.txt")).unwrap(), b"two");
        assert!(!repo.kio_dir().join(MANAGED_RESTORE_JOURNAL_LEAF).exists());
    }

    #[test]
    fn restoring_old_absence_is_ambiguous_and_never_resurrected() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), b"one").unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        let source = snapshot(&repo, "2026-01-01T00:00:00Z");
        fs::write(directory.path().join("a.txt"), b"two").unwrap();
        snapshot(&repo, "2026-01-02T00:00:00Z");
        let plan = repo
            .plan_restore(ManagedRestoreRequest::new(
                source,
                Some(vec!["a.txt".to_owned()]),
                Vec::new(),
                "restore a",
                "2026-01-03T00:00:00Z",
            ))
            .unwrap();
        stage_restore_journal(&repo, &plan, RestoreProgress::RestoringOld);
        let old = repo
            .raw_bytes(plan.changes[0].old_raw_hash.as_deref().unwrap())
            .unwrap();
        repo.remove_working_atomic(
            Path::new("a.txt"),
            &old,
            super::MAX_MANAGED_RESTORE_FILE_BYTES,
        )
        .unwrap();
        let new = repo
            .raw_bytes(plan.changes[0].new_raw_hash.as_deref().unwrap())
            .unwrap();
        repo.write_working_atomic(Path::new("a.txt"), &new, Publication::CreateOnly)
            .unwrap();
        repo.remove_working_atomic(
            Path::new("a.txt"),
            &new,
            super::MAX_MANAGED_RESTORE_FILE_BYTES,
        )
        .unwrap();

        assert_eq!(
            repo.recover_managed_restore().unwrap_err().error_code(),
            "KIO-E-MANAGED-RESTORE-RECOVERY-REQUIRED-001"
        );
        assert!(!directory.path().join("a.txt").exists());
        assert!(repo.kio_dir().join(MANAGED_RESTORE_JOURNAL_LEAF).exists());
    }

    #[test]
    fn generic_publication_recovery_refuses_a_dual_managed_restore_journal() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), b"one").unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        let source = snapshot(&repo, "2026-01-01T00:00:00Z");
        fs::write(directory.path().join("a.txt"), b"two").unwrap();
        let head = snapshot(&repo, "2026-01-02T00:00:00Z");
        let plan = repo
            .plan_restore(ManagedRestoreRequest::new(
                source,
                Some(vec!["a.txt".to_owned()]),
                Vec::new(),
                "restore a",
                "2026-01-03T00:00:00Z",
            ))
            .unwrap();
        let tree = serde_json::to_value(&plan.new_tree).unwrap();
        repo.store.write_json(ObjectKind::Tree, &tree).unwrap();
        let commit = serde_json::to_value(&plan.new_commit).unwrap();
        repo.store.write_json(ObjectKind::Commit, &commit).unwrap();
        let mut managed = ManagedRestoreJournal::from_plan(&plan);
        managed.progress[0] = RestoreProgress::Applied;
        let managed_bytes = managed.bytes().unwrap();
        repo.store_directory()
            .unwrap()
            .write_atomic(
                Path::new(MANAGED_RESTORE_JOURNAL_LEAF),
                &managed_bytes,
                Publication::CreateOnly,
            )
            .unwrap();
        repo.apply_restore_change(&plan.changes[0]).unwrap();
        let prior = repo.read_commit(&head).unwrap();
        let manifest = repo
            .manifest_value(&plan.new_tree, Some(&repo.read_tree(&prior.tree).unwrap()))
            .unwrap();
        let publication = super::super::PublicationJournal {
            version: 1,
            expected_head: Some(head.clone()),
            new_head: plan.new_commit_hash.clone(),
            manifest,
        };
        let publication_bytes =
            super::canonical_json_bytes(&serde_json::to_value(&publication).unwrap()).unwrap();
        repo.store_directory()
            .unwrap()
            .write_atomic(
                Path::new(super::super::PUBLICATION_JOURNAL_LEAF),
                &publication_bytes,
                Publication::CreateOnly,
            )
            .unwrap();
        let before_head = fs::read(repo.kio_dir().join("HEAD")).unwrap();
        let before_working = fs::read(directory.path().join("a.txt")).unwrap();
        assert_eq!(
            repo.recover_publication().unwrap_err().error_code(),
            "KIO-E-MANAGED-RESTORE-RECOVERY-REQUIRED-001"
        );
        assert_eq!(fs::read(repo.kio_dir().join("HEAD")).unwrap(), before_head);
        assert_eq!(
            fs::read(directory.path().join("a.txt")).unwrap(),
            before_working
        );
        assert_eq!(
            fs::read(repo.kio_dir().join(super::super::PUBLICATION_JOURNAL_LEAF)).unwrap(),
            publication_bytes
        );
        assert_eq!(
            fs::read(repo.kio_dir().join(MANAGED_RESTORE_JOURNAL_LEAF)).unwrap(),
            managed_bytes
        );

        assert!(repo.recover_managed_restore().unwrap());
        assert_eq!(
            repo.head_commit_hash().unwrap().as_deref(),
            Some(head.as_str())
        );
        assert_eq!(fs::read(directory.path().join("a.txt")).unwrap(), b"two");
        assert!(
            !repo
                .kio_dir()
                .join(super::super::PUBLICATION_JOURNAL_LEAF)
                .exists()
        );
        assert!(!repo.kio_dir().join(MANAGED_RESTORE_JOURNAL_LEAF).exists());
    }
}
