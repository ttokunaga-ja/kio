use super::*;

impl Repository {
    pub fn log(&self) -> Result<LogReport> {
        self.log_from(None)
    }

    /// QB50 (step4b-contract-tests-p3b.md §D, 06 §1 L61 + 裁定5's adopted
    /// analogy to `--at`'s established "resolve the operand, then treat it
    /// as the walk's basis" semantics elsewhere — search 06 §3 L226 /
    /// 05-runtime.md §1.6 L214): `kio log --at <commit>` walks from an
    /// explicit, already-`resolve_commit`-resolved starting commit instead
    /// of HEAD. `start = None` (plain `kio log`) is exactly the prior `log`
    /// behavior (QB53's non-breaking regression requirement) — first-parent
    /// walk from HEAD, truncating (not failing) on a missing ancestor.
    pub fn log_from(&self, start: Option<String>) -> Result<LogReport> {
        self.log_from_with_limit(start, crate::history::DEFAULT_MAX_HISTORY_COMMITS)
    }

    /// [`Self::log_from`]'s implementation, parameterized on the
    /// first-parent walk's aggregate commit-count cap (R23-29,
    /// 05-runtime.md §1.6 L304-313) so tests can exercise the exact
    /// boundary without materializing `DEFAULT_MAX_HISTORY_COMMITS`
    /// (100,000) real commits. Shares `history::DEFAULT_MAX_HISTORY_COMMITS`
    /// with the `HistoryReader`-based all-parent/first-parent walks
    /// (`crate::history`) so the bound is exact and identically sized
    /// everywhere it is named, not a locally re-guessed constant. Unlike
    /// those walks, this one never reads tree objects at all -- only commit
    /// objects -- so the aggregate bound's other two components (tree
    /// entries / verified bytes) do not independently apply: a commit
    /// object holds only metadata (tree hash, parents, timestamp, message,
    /// stats), so reaching the 4 GiB byte bound from `commits.len()` commit
    /// objects alone would require an implausible ~40 KiB average commit
    /// size -- the commit-count cap below always fires first.
    pub(super) fn log_from_with_limit(
        &self,
        start: Option<String>,
        max_commits: u64,
    ) -> Result<LogReport> {
        self.validate()?;
        let mut entries = Vec::new();
        let mut next = match start {
            Some(hash) => Some(hash),
            None => self.head_commit_hash()?,
        };
        let mut truncated = false;
        let mut commit_count: u64 = 0;
        while let Some(hash) = next {
            commit_count += 1;
            if commit_count > max_commits {
                return Err(KioError::new(
                    "KIO-E-COMMIT-HISTORY-LIMIT-001",
                    "history walk aggregate limit exceeded",
                    json!({
                        "exceeded": "commits",
                        "attempted": commit_count,
                        "max_commits": max_commits,
                    }),
                    ExitCode::PermanentFailure,
                ));
            }
            // R16-1: a missing ancestor commit object (shallow / external corruption)
            // truncates the history at that point instead of bricking the whole `log`
            // on a raw KIO-E-STORE-NOT-FOUND-001. The healthy prefix from HEAD is
            // returned with `truncated = true` so the loss is explicit — Sonnet-B's
            // repro (a missing root commit swallowing the healthy recent commits too)
            // is exactly what this prevents.
            let commit = match self.read_commit(&hash) {
                Ok(commit) => commit,
                Err(error) if is_store_not_found(&error) => {
                    truncated = true;
                    break;
                }
                Err(error) => return Err(error),
            };
            next = commit.parent.clone();
            entries.push(LogEntry {
                commit_hash: hash,
                commit,
            });
        }
        Ok(LogReport { entries, truncated })
    }

    pub fn diff(&self, a: &str, b: &str) -> Result<Vec<DiffEntry>> {
        self.validate()?;
        let a_hash = self.resolve_commit(a)?;
        let b_hash = self.resolve_commit(b)?;
        // R16-5: if either side's commit or tree object is gone (shallow), a
        // full-file diff is impossible — surface a clear KIO-E-COMMIT-SHALLOW-001
        // that names WHICH side (a/b) is shallow, not a raw opaque
        // KIO-E-STORE-NOT-FOUND-001 whose hash the user cannot map to an operand
        // (docs/05 §3.4.1: "片方が shallow なら全ファイル差分は不能と明示").
        let a_tree = self.diff_side_tree("a", &a_hash)?;
        let b_tree = self.diff_side_tree("b", &b_hash)?;
        let a_map = tree_map(&a_tree);
        let b_map = tree_map(&b_tree);

        let mut paths = BTreeSet::new();
        paths.extend(a_map.keys().cloned());
        paths.extend(b_map.keys().cloned());

        let mut changes = Vec::new();
        for path in paths {
            let change = match (a_map.get(&path), b_map.get(&path)) {
                (None, Some(_)) => "added",
                (Some(_), None) => "deleted",
                (Some(old), Some(new)) if old != new => "modified",
                _ => continue,
            };
            changes.push(DiffEntry {
                path: is_materializable_direct_child(&path).then(|| self.root.join(&path)),
                relative_path: path.clone(),
                change: change.to_owned(),
                old_raw_hash: a_map.get(&path).cloned(),
                new_raw_hash: b_map.get(&path).cloned(),
            });
        }
        Ok(changes)
    }

    /// R16-5: read one `diff` side's tree, folding a missing commit OR tree object
    /// (shallow: discarded / corrupt) into a KIO-E-COMMIT-SHALLOW-001 that names
    /// which side (`side` = "a"/"b") is shallow. The rationale for reusing
    /// COMMIT-SHALLOW rather than minting a diff-specific code: the recovery
    /// (restore the object or diff a non-shallow pair) and semantics are identical
    /// to every other shallow-commit site (R16-1). `resolve_commit` already ran, so
    /// a genuinely absent operand is a distinct not_found — only a resolved commit
    /// whose backing objects are gone reaches here.
    fn diff_side_tree(&self, side: &str, commit_hash: &str) -> Result<TreeObject> {
        let commit = match self.read_commit(commit_hash) {
            Ok(commit) => commit,
            Err(error) if is_store_not_found(&error) => {
                return Err(diff_side_shallow_error(side, commit_hash));
            }
            Err(error) => return Err(error),
        };
        match self.read_tree(&commit.tree) {
            Ok(tree) => Ok(tree),
            Err(error) if is_store_not_found(&error) => {
                crate::gc::GcSweepSession::bind_repository(self)?
                    .validate_final_shallow_tree(commit_hash, &commit.tree)?;
                Err(diff_side_shallow_error(side, commit_hash))
            }
            Err(error) => Err(error),
        }
    }

    pub fn inspect(&self, hash: &str) -> Result<InspectedObject> {
        self.validate()?;
        let object = self.store.inspect_by_hash(hash)?;
        match object.kind {
            ObjectKind::Tree => self.read_tree(hash).map(InspectedObject::Tree),
            ObjectKind::Commit => self.read_commit(hash).map(InspectedObject::Commit),
            ObjectKind::Raw => Ok(InspectedObject::Raw {
                raw_hash: object.hash,
                size_bytes: object.size_bytes,
            }),
        }
    }

    pub fn tag(&self, name: &str, commit: Option<&str>) -> Result<String> {
        self.validate()?;
        validate_tag_name(name)?;
        let _lock = self.lock_store_in_session(&self.lock_session)?;
        let commit_hash = match commit {
            Some(value) => self.resolve_commit(value)?,
            None => self
                .head_commit_hash()?
                .ok_or_else(|| KioError::not_found("HEAD"))?,
        };
        // R17-5: with no explicit operand the implicit HEAD is resolved via
        // head_commit_hash() (which never reads the commit object), so this is the
        // first existence check. A shallow (missing / corrupt) HEAD commit folds into
        // KIO-E-COMMIT-SHALLOW-001 with tag-write context, matching every other
        // shallow-commit site (R16-1), not a raw, opaque KIO-E-STORE-NOT-FOUND-001.
        // (When `commit_hash` came from `resolve_commit`, existence was already
        // verified there, so this read only surfaces the implicit-HEAD case.)
        let tagged_commit = match self.read_commit(&commit_hash) {
            Ok(commit) => commit,
            Err(error) if is_store_not_found(&error) => {
                return Err(KioError::commit_shallow(
                    "cannot create a tag on a shallow commit: the HEAD commit object is \
                     missing (discarded / corrupt); restore the commit object or tag a \
                     non-shallow commit",
                    commit_hash.clone(),
                ));
            }
            Err(error) => return Err(error),
        };
        // A final shallow receipt is an explicit history boundary; publishing a
        // new ref to that commit would make it a live tip with no tree.  Route
        // *every* missing target tree through the shared strict classifier:
        // a canonical final boundary returns COMMIT-SHALLOW, while a missing
        // receipt, a mismatch, a manual/purged commit, an active sweep, or an
        // already-live ref tip is store corruption.  If the tree is present,
        // an extant receipt is likewise corruption (a receipt cannot be a
        // best-effort hint).
        match self.read_tree(&tagged_commit.tree) {
            Err(error) if is_store_not_found(&error) => {
                crate::gc::GcSweepSession::bind_repository(self)?
                    .validate_final_shallow_tree(&commit_hash, &tagged_commit.tree)?;
                return Err(KioError::commit_shallow(
                    "cannot create a tag on a final shallow commit; restore a non-shallow commit first",
                    commit_hash.clone(),
                ));
            }
            Ok(_)
                if crate::gc::read_shallow_receipts(&self.kio_dir)?.contains_key(&commit_hash) =>
            {
                return Err(KioError::new(
                    "KIO-E-STORE-CORRUPT-001",
                    "shallow receipt coexists with the tagged tree object",
                    json!({"commit_hash": commit_hash}),
                    ExitCode::PermanentFailure,
                ));
            }
            Ok(_) => {}
            Err(error) => return Err(error),
        }
        if !self
            .unpublished_history_roots(&BTreeSet::from([commit_hash.clone()]))?
            .is_empty()
        {
            return Err(KioError::invalid_usage(
                "a tag must name the current HEAD or one of its ancestors",
            ));
        }
        let directory = self.store_directory()?;
        let tags = Path::new("refs").join(PORTABLE_TAGS_DIRECTORY);
        directory.create_directory_all(&tags)?;
        let leaf = portable_tag_leaf(name);
        if directory
            .read_optional(&tags.join(&leaf), MAX_TAG_REF_BYTES)?
            .is_some()
        {
            return Err(KioError::new(
                "KIO-E-COMMIT-TAG-001",
                "tag already exists (tag names collide case-insensitively)",
                json!({ "tag": name }),
                ExitCode::InvalidUsage,
            ));
        }
        let row = canonical_json_bytes(&json!({
            "digest64": portable_tag_digest64(name),
            "logical_name": name.nfc().collect::<String>(),
            "recorded_at": self.now_utc_seconds(),
        }))?;
        let mut row = row;
        row.push(b'\n');
        let names = tags.join("names.jsonl");
        match directory.read_optional(&names, MAX_TAG_NAMES_BYTES)? {
            None => {
                directory.write_atomic(&names, &row, crate::store_dir::Publication::CreateOnly)?
            }
            Some(existing) => {
                if (existing.len() as u64).saturating_add(row.len() as u64) > MAX_TAG_NAMES_BYTES {
                    return Err(KioError::invalid_usage(
                        "tag name ledger exceeds its byte limit",
                    ));
                }
                directory.append(&names, &row)?;
            }
        }
        directory.write_atomic(
            &tags.join(leaf),
            commit_hash.as_bytes(),
            crate::store_dir::Publication::CreateOnly,
        )?;
        Ok(commit_hash)
    }

    /// Remove only the canonical tag ref. The append-only logical-name ledger
    /// remains as an audit record, and no commit/tree object needs to be read:
    /// a tag can be removed even after its target history became shallow.
    pub fn delete_tag(&self, name: &str) -> Result<String> {
        self.validate()?;
        validate_tag_name(name)?;
        let _lock = self.lock_store_in_session(&self.lock_session)?;
        let directory = self.store_directory()?;
        // An interrupted removal may already have unpublished the ref while
        // leaving a durable atomic intent/quarantine. Complete that exact
        // operation before deciding whether this invocation still has a tag.
        directory.recover_atomic(&[&directory])?;
        let relative = Path::new("refs")
            .join(PORTABLE_TAGS_DIRECTORY)
            .join(portable_tag_leaf(name));
        let bytes = directory
            .read_optional(&relative, MAX_TAG_REF_BYTES)?
            .ok_or_else(|| KioError::not_found(name))?;
        let target = std::str::from_utf8(&bytes)
            .map_err(|_| tag_ref_corrupt(&self.kio_dir.join(&relative), "tag ref is not UTF-8"))?;
        if !is_hash(target) {
            return Err(tag_ref_corrupt(
                &self.kio_dir.join(&relative),
                "tag ref target is invalid",
            ));
        }
        directory.quarantine_then_remove(&relative, &bytes, MAX_TAG_REF_BYTES)?;
        Ok(target.to_owned())
    }

    /// Verify that retained refs and publication records cannot expand the
    /// single HEAD ancestry. This is also used before rebuilding projections.
    pub fn validate_published_history_roots(&self, roots: &BTreeSet<String>) -> Result<()> {
        let unpublished = self.unpublished_history_roots(roots)?;
        if !unpublished.is_empty() {
            return Err(KioError::schema(
                "history roots are outside the current HEAD linear ancestry chain",
            ));
        }
        Ok(())
    }

    /// A detached CAS object is not a published history root. Retired ancestor
    /// trees are irrelevant: only verified commit predecessor edges are read.
    fn unpublished_history_roots(&self, roots: &BTreeSet<String>) -> Result<BTreeSet<String>> {
        let mut pending = roots.clone();
        let mut current = self.head_commit_hash()?;
        let mut visited = BTreeSet::new();
        let mut verified_bytes = 0_u64;
        while !pending.is_empty() {
            let Some(hash) = current else { break };
            if visited.len() as u64 >= crate::history::DEFAULT_MAX_HISTORY_COMMITS {
                return Err(KioError::new(
                    "KIO-E-HISTORY-LIMIT-001",
                    "published ancestry proof exceeds the commit limit",
                    json!({ "limit": crate::history::DEFAULT_MAX_HISTORY_COMMITS }),
                    ExitCode::PermanentFailure,
                ));
            }
            if !visited.insert(hash.clone()) {
                return Err(KioError::schema("commit history contains a parent cycle"));
            }
            let (object, bytes) = self
                .store
                .read_object_accounted(ObjectKind::Commit, &hash)
                .map_err(|error| error.error)?;
            verified_bytes = verified_bytes.saturating_add(bytes);
            if verified_bytes > crate::history::DEFAULT_MAX_HISTORY_VERIFIED_BYTES {
                return Err(KioError::new(
                    "KIO-E-HISTORY-LIMIT-001",
                    "published ancestry proof exceeds the byte limit",
                    json!({ "limit": crate::history::DEFAULT_MAX_HISTORY_VERIFIED_BYTES }),
                    ExitCode::PermanentFailure,
                ));
            }
            let commit: CommitObject = serde_json::from_slice(&object.bytes)
                .map_err(|error| KioError::schema(error.to_string()))?;
            commit.validate()?;
            pending.remove(&hash);
            current = commit.parent;
        }
        Ok(pending)
    }

    pub fn resolve_commit(&self, value: &str) -> Result<String> {
        // N4 (03 §3 scope boundary): a commit-ref operand is only ever `HEAD`, a
        // hash, or a tag name — none legitimately carry a path separator or a
        // `.`/`..` component. Without this guard, constructing a tag-ref path from
        // an unchecked operand would treat
        // `../../..` as a filesystem escape, turning `kio diff`/`kio tag <commit>`
        // into an out-of-scope file-existence oracle. Validate before any join.
        validate_ref_operand(value)?;
        if value == "HEAD" {
            return self
                .head_commit_hash()?
                .ok_or_else(|| KioError::not_found("HEAD"));
        }
        if is_hash(value) {
            // R17-5: `resolve_commit` runs before `diff_side_tree`'s R16-5 shallow
            // absorption (and before `tag`'s own verification read), so a hash-literal
            // shallow commit (commit object discarded / corrupt) must fold into
            // KIO-E-COMMIT-SHALLOW-001 HERE — otherwise it escapes as a raw, opaque
            // KIO-E-STORE-NOT-FOUND-001 while the `HEAD` operand (which skips
            // read_commit) correctly reaches COMMIT-SHALLOW. Matches the other 8
            // read_commit sites hardened in R16-1.
            match self.read_commit(value) {
                Ok(_) => {}
                Err(error) if is_store_not_found(&error) => {
                    return Err(resolve_commit_shallow_error(value));
                }
                Err(error) => return Err(error),
            }
            return Ok(value.to_owned());
        }
        let normalized_operand = portable_tag_collision_key(value);
        if normalized_operand == "head" || is_hash(&normalized_operand) {
            return Err(KioError::invalid_usage(
                "commit reference collides with a reserved operand",
            ));
        }
        let tag = Path::new("refs")
            .join(PORTABLE_TAGS_DIRECTORY)
            .join(portable_tag_leaf(value));
        if let Some(bytes) = self
            .store_directory()?
            .read_optional(&tag, MAX_TAG_REF_BYTES)?
        {
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| tag_ref_corrupt(&self.kio_dir.join(&tag), "tag ref is not UTF-8"))?;
            let hash = text.trim().to_owned();
            if !is_hash(&hash) {
                return Err(tag_ref_corrupt(
                    &self.kio_dir.join(&tag),
                    "tag ref target is invalid",
                ));
            }
            // R17-5: a tag whose target commit object is shallow (discarded / corrupt)
            // folds into COMMIT-SHALLOW too, for the same reason as the hash-literal
            // branch above.
            match self.read_commit(&hash) {
                Ok(_) => {}
                Err(error) if is_store_not_found(&error) => {
                    return Err(resolve_commit_shallow_error(&hash));
                }
                Err(error) => return Err(error),
            }
            return Ok(hash);
        }
        Err(KioError::not_found(value))
    }

    pub fn read_commit(&self, hash: &str) -> Result<CommitObject> {
        let object = self.store.read_by_hash(hash)?;
        if object.kind != ObjectKind::Commit {
            return Err(KioError::schema("hash does not identify a commit"));
        }
        let commit: CommitObject = serde_json::from_slice(&object.bytes)
            .map_err(|err| KioError::schema(err.to_string()))?;
        commit.validate()?;
        Ok(commit)
    }

    pub fn read_tree(&self, hash: &str) -> Result<TreeObject> {
        let object = self.store.read_by_hash(hash)?;
        if object.kind != ObjectKind::Tree {
            return Err(KioError::schema("hash does not identify a tree"));
        }
        let tree: TreeObject = serde_json::from_slice(&object.bytes)
            .map_err(|err| KioError::schema(err.to_string()))?;
        if tree.entries.len() > MAX_TREE_ENTRIES {
            return Err(KioError::schema(format!(
                "tree entries exceed the limit of {MAX_TREE_ENTRIES}"
            )));
        }
        tree.validate()?;
        Ok(tree)
    }

    pub fn head_commit_hash(&self) -> Result<Option<String>> {
        if let Some(kio) = self.bound_kio.as_deref() {
            let directory = StoreDirectory::from_retained_with_control(
                kio.try_clone()
                    .map_err(|error| KioError::io(error.to_string(), ".kio"))?,
                self.kio_dir.clone(),
                self.control().clone(),
            )?;
            let bytes = directory
                .read_optional(Path::new("HEAD"), MAX_TAG_REF_BYTES)?
                .ok_or_else(|| KioError::schema("HEAD is missing"))?;
            return parse_head_ref(
                std::str::from_utf8(&bytes).map_err(|_| KioError::schema("HEAD must be UTF-8"))?,
            );
        }
        let path = self.kio_dir.join("HEAD");
        let value = String::from_utf8(read_bounded_regular_file(&path, MAX_TAG_REF_BYTES)?)
            .map_err(|_| KioError::schema("HEAD must be UTF-8"))?;
        parse_head_ref(&value)
    }

    /// R15-4: read the HEAD commit's tree object, distinguishing an unborn branch
    /// (no HEAD) from a shallow HEAD (tree object gone) from a present tree. A raw
    /// `KIO-E-STORE-NOT-FOUND-001` from the tree read is folded into the `Shallow`
    /// variant so callers decide the policy: pure reads degrade, writes fail loudly.
    pub(super) fn head_tree_state(&self) -> Result<HeadTreeState> {
        let Some(commit_hash) = self.head_commit_hash()? else {
            return Ok(HeadTreeState::Unborn);
        };
        // R16-1: fold a missing *commit* object into `Shallow` too (was an
        // unconditional `?` that bricked pure reads on a raw KIO-E-STORE-NOT-FOUND-001
        // when the commit — not just its tree — was gone). Same corruption class, same
        // degrade-vs-fail-loudly policy the tree case already had.
        let tree_hash = match self.read_commit(&commit_hash) {
            Ok(commit) => commit.tree,
            Err(error) if is_store_not_found(&error) => return Ok(HeadTreeState::Shallow),
            Err(error) => return Err(error),
        };
        match self.read_tree(&tree_hash) {
            Ok(tree) => Ok(HeadTreeState::Present(tree)),
            Err(error) if is_store_not_found(&error) => {
                crate::gc::GcSweepSession::bind_repository(self)?
                    .validate_final_shallow_tree(&commit_hash, &tree_hash)?;
                Ok(HeadTreeState::Shallow)
            }
            Err(error) => Err(error),
        }
    }
}
