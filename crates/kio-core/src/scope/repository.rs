use super::*;

impl Repository {
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }

    #[must_use]
    pub fn kio_dir(&self) -> &Path {
        &self.kio_dir
    }

    /// Retained scope-root descriptor for normal and internal repository I/O.
    #[must_use]
    pub fn bound_root_handle(&self) -> Option<&File> {
        self.bound_root.as_deref()
    }

    /// Retained `.kio` descriptor for an internal descriptor-bound child index
    /// process. Operational store paths are relative to this directory.
    #[must_use]
    pub fn bound_kio_handle(&self) -> Option<&File> {
        self.bound_kio.as_deref()
    }

    /// Enumerate targets named by the sole mutable HEAD and immutable tags.
    ///
    /// Tags must belong to the published HEAD ancestry. Detached CAS objects,
    /// unpublished descendants, and tags in an unborn store cannot introduce
    /// additional history roots. Trees are validated by the subsequent reader;
    /// this proof needs only immutable commit predecessor edges.
    pub fn current_ref_targets(&self) -> Result<BTreeSet<String>> {
        if self.bound_kio.is_some() {
            let directory = self.store_directory()?;
            let head = directory
                .read_optional(Path::new("HEAD"), MAX_TAG_REF_BYTES)?
                .ok_or_else(|| KioError::schema("HEAD is missing"))?;
            let head =
                std::str::from_utf8(&head).map_err(|_| KioError::schema("HEAD must be UTF-8"))?;
            let mut targets = BTreeSet::new();
            if let Some(hash) = parse_head_ref(head)? {
                targets.insert(hash);
            }
            let tags = Path::new("refs").join(PORTABLE_TAGS_DIRECTORY);
            for entry in directory.entries(&tags)? {
                let name = entry.name.to_string_lossy();
                if name == "names.jsonl" {
                    continue;
                }
                if !entry.is_regular_file
                    || name.len() != 68
                    || !name.starts_with("tag-")
                    || !name[4..]
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
                {
                    return Err(KioError::schema("canonical tag leaf is invalid"));
                }
                let bytes = directory
                    .read_optional(&tags.join(entry.name), MAX_TAG_REF_BYTES)?
                    .ok_or_else(|| KioError::schema("tag ref disappeared"))?;
                let hash = std::str::from_utf8(&bytes)
                    .map_err(|_| KioError::schema("tag ref must be UTF-8"))?
                    .trim();
                if !is_hash(hash) {
                    return Err(KioError::schema("tag ref target is invalid"));
                }
                targets.insert(hash.to_owned());
            }
            self.validate_published_history_roots(&targets)?;
            return Ok(targets);
        }
        let mut targets = BTreeSet::new();
        let head = read_commit_ref(&self.kio_dir.join("HEAD"), true)?;
        if let Some(hash) = head {
            targets.insert(hash);
        }

        let refs = self.kio_dir.join("refs");
        collect_tag_ref_targets(&refs.join(PORTABLE_TAGS_DIRECTORY), &mut targets)?;
        self.validate_published_history_roots(&targets)?;
        Ok(targets)
    }

    pub(super) fn publication_journal(&self) -> Result<Option<PublicationJournal>> {
        let Some(bytes) = self.store_directory()?.read_optional(
            Path::new(PUBLICATION_JOURNAL_LEAF),
            MAX_PUBLICATION_JOURNAL_BYTES,
        )?
        else {
            return Ok(None);
        };
        let value: PublicationJournal =
            serde_json::from_slice(&bytes).map_err(|error| KioError::schema(error.to_string()))?;
        if value.version != 1
            || !is_hash(&value.new_head)
            || value
                .expected_head
                .as_deref()
                .is_some_and(|head| !is_hash(head))
        {
            return Err(KioError::schema(
                "publication journal has invalid authority fields",
            ));
        }
        validate_json_schema(SchemaKind::Manifest, &value.manifest)?;
        Ok(Some(value))
    }

    pub(super) fn ensure_no_pending_publication(&self) -> Result<()> {
        if self.publication_journal()?.is_some() {
            return Err(KioError::new(
                "KIO-E-PUBLICATION-RECOVERY-REQUIRED-001",
                "an incomplete history publication requires explicit recovery",
                json!({ "journal": PUBLICATION_JOURNAL_LEAF }),
                ExitCode::PartialFailure,
            ));
        }
        Ok(())
    }

    /// Finish an interrupted journaled publication. The caller must hold the
    /// store lock. It refuses a changed HEAD, a missing commit, or a commit
    /// that is not the exact child named by the journal.
    pub fn recover_publication(&self) -> Result<bool> {
        let _lock = self.lock_store_in_session(&self.lock_session)?;
        let atomic_recovered = self.recover_atomic_storage()?;
        // A publication journal may be the last phase of a managed restore.
        // It must never advance HEAD while that restore's working-tree journal
        // still authorizes rollback rather than finalization.
        self.ensure_no_pending_managed_restore()?;
        let Some(journal) = self.publication_journal()? else {
            return Ok(atomic_recovered);
        };
        let current = self.head_commit_hash()?;
        if current.as_deref() != journal.expected_head.as_deref()
            && current.as_deref() != Some(journal.new_head.as_str())
        {
            return Err(KioError::new(
                "KIO-E-PUBLICATION-CONFLICT-001",
                "publication journal does not match the current HEAD",
                json!({ "expected_head": journal.expected_head, "current_head": current, "new_head": journal.new_head }),
                ExitCode::PartialFailure,
            ));
        }
        let commit = self.read_commit(&journal.new_head)?;
        if commit.parent != journal.expected_head {
            return Err(KioError::schema(
                "publication journal target is not the expected HEAD child",
            ));
        }
        let tree = self.read_tree(&commit.tree)?;
        validate_manifest_matches_tree(&journal.manifest, &tree)?;
        if current.as_deref() != Some(journal.new_head.as_str()) {
            let head_bytes = format!("{}\n", journal.new_head);
            self.replace_metadata("HEAD", head_bytes.as_bytes())?;
        }
        self.write_manifest_value(&journal.manifest)?;
        self.remove_publication_journal()?;
        Ok(true)
    }

    pub(super) fn publish_with_journal(
        &self,
        expected_head: Option<&str>,
        new_head: &str,
        commit: &CommitObject,
        manifest: Value,
    ) -> Result<()> {
        self.ensure_no_pending_publication()?;
        if commit.parent.as_deref() != expected_head {
            return Err(KioError::schema(
                "published commit is not an exact child of HEAD",
            ));
        }
        if self.head_commit_hash()?.as_deref() != expected_head {
            return Err(KioError::new(
                "KIO-E-PUBLICATION-CONFLICT-001",
                "HEAD changed before publication",
                json!({ "expected_head": expected_head, "new_head": new_head }),
                ExitCode::PartialFailure,
            ));
        }
        let stored = self.store.read_by_hash(new_head)?;
        if stored.kind != ObjectKind::Commit
            || serde_json::from_slice::<CommitObject>(&stored.bytes)
                .map_err(|error| KioError::schema(error.to_string()))?
                != *commit
        {
            return Err(KioError::schema(
                "publication hash does not name the supplied commit",
            ));
        }
        let tree = self.read_tree(&commit.tree)?;
        validate_manifest_matches_tree(&manifest, &tree)?;
        let journal = PublicationJournal {
            version: 1,
            expected_head: expected_head.map(str::to_owned),
            new_head: new_head.to_owned(),
            manifest,
        };
        let value =
            serde_json::to_value(&journal).map_err(|error| KioError::schema(error.to_string()))?;
        let bytes = canonical_json_bytes(&value)?;
        if bytes.len() as u64 > MAX_PUBLICATION_JOURNAL_BYTES {
            return Err(KioError::schema(
                "publication journal exceeds its durable size limit",
            ));
        }
        let directory = self.store_directory()?;
        directory.write_atomic(
            Path::new(PUBLICATION_JOURNAL_LEAF),
            &bytes,
            crate::store_dir::Publication::CreateOnly,
        )?;
        self.control()
            .checkpoint(crate::durability::DurabilityPoint::PublicationJournal)?;
        directory.write_atomic(
            Path::new("HEAD"),
            format!("{new_head}\n").as_bytes(),
            crate::store_dir::Publication::Replace,
        )?;
        self.control()
            .checkpoint(crate::durability::DurabilityPoint::PublicationHead)?;
        self.write_manifest_value(&journal.manifest)?;
        self.control()
            .checkpoint(crate::durability::DurabilityPoint::PublicationManifest)?;
        self.remove_publication_journal()?;
        Ok(())
    }

    pub(super) fn remove_publication_journal(&self) -> Result<()> {
        self.store_directory()?
            .remove_file(Path::new(PUBLICATION_JOURNAL_LEAF))
    }

    /// QA5 (step4b-contract-tests-p3a.md §B, 10 §1 L97-113): record the
    /// one-time scope-level scan approval into `.kio/scope.json`'s
    /// `scan_approval` key — distinct from the adapter-level network opt-in
    /// (`approvals.jsonl`/`consents.jsonl`, 07 §3). Idempotent: a scope is
    /// approved once, so an existing `scan_approval` key is left untouched
    /// (`Ok(false)`); the first call materializes it (`Ok(true)`). `fields`
    /// must be a JSON object shaped per 10 §1 L101-113 (scope_id / root_path /
    /// approved_at / actor / approval_method / kio_version /
    /// effective_ignore_hash / estimated_file_count / estimated_total_bytes /
    /// estimated_markdownize_usd / estimated_embedding_usd).
    pub fn record_scan_approval(&self, fields: Value) -> Result<bool> {
        let mut value: Value =
            serde_json::from_str(&self.metadata_text("scope.json", MAX_BOUND_CONFIG_BYTES)?)
                .map_err(|err| KioError::schema(err.to_string()))?;
        let Some(object) = value.as_object_mut() else {
            return Err(KioError::schema("scope.json must be an object"));
        };
        if object.contains_key("scan_approval") {
            return Ok(false);
        }
        object.insert("scan_approval".to_owned(), fields);
        validate_json_schema(SchemaKind::Scope, &value)?;
        // Scope approval is mutable metadata: replace through the retained
        // store capability while the caller holds its publication lock.
        self.replace_metadata(
            "scope.json",
            serde_json::to_string_pretty(&value)
                .map_err(|err| KioError::schema(err.to_string()))?
                .as_bytes(),
        )?;
        Ok(true)
    }

    /// Return the portable scope ID together with the canonical local root.
    /// Authorization callers must bind both values to protected device-local
    /// state; `scope.json` alone is portable audit data, not active consent.
    pub fn scope_identity(&self) -> Result<ScopeIdentity> {
        self.validate_store_authority()?;
        Ok(ScopeIdentity {
            scope_id: self.validated_scope_id()?,
            canonical_root: self.canonical_root.clone(),
        })
    }

    /// Acquire the exclusive `.kio/.lock` store lock (05 §6) and return an RAII
    /// guard held for the caller's lifetime. Used to serialize whole mutating
    /// commands (`kio index` / `repair` / `reindex`) end-to-end, not just their
    /// snapshot sub-step. Independent calls use fresh authority and contend
    /// through the OS; nested engine work uses `lock_store_in_session`.
    /// The loser of a concurrent acquisition gets `KIO-E-STORE-LOCKED-001`
    /// (exit 3), the same contract as `snapshot` / `tag`.
    pub fn lock_store(&self) -> Result<RetainedStoreLock> {
        self.lock_store_in_session(&LockSession::default())
    }

    pub fn lock_store_in_session(&self, session: &LockSession) -> Result<RetainedStoreLock> {
        let kio = self.bound_kio.as_deref().ok_or_else(|| {
            KioError::invalid_usage("repository store lock requires retained .kio authority")
        })?;
        let lock = acquire_retained_store_lock_with_control(session, kio, self.control())?;
        if let Err(error) = crate::gc::ensure_no_active_sweep_bound(kio, self.control()) {
            drop(lock);
            return Err(error);
        }
        Ok(lock)
    }

    /// Narrow retained publication barrier shared by purge and restore. It is
    /// a distinct leaf, so a purge holding the writer lease can acquire it
    /// without reopening a pathname or contending on the directory flock.
    pub fn lock_purge_publication(&self) -> Result<RetainedPublicationLock> {
        self.lock_purge_publication_in_session(&LockSession::default())
    }

    pub fn lock_purge_publication_in_session(
        &self,
        session: &LockSession,
    ) -> Result<RetainedPublicationLock> {
        let kio = self.bound_kio.as_deref().ok_or_else(|| {
            KioError::invalid_usage("publication lock requires retained .kio authority")
        })?;
        acquire_retained_publication_lock(session, kio, self.control())
    }

    pub(super) fn purge_state(&self) -> Result<PurgeState> {
        if let Some(kio) = self.bound_kio.as_deref() {
            return Ok(PurgeState::from_directory(
                StoreDirectory::from_retained_with_control(
                    kio.try_clone()
                        .map_err(|error| KioError::io(error.to_string(), ".kio"))?,
                    self.kio_dir.clone(),
                    self.control().clone(),
                )?,
            ));
        }
        PurgeState::open_with_control(&self.kio_dir, self.control().clone())
    }

    pub(crate) fn store_directory(&self) -> Result<StoreDirectory> {
        if let Some(kio) = self.bound_kio.as_deref() {
            return StoreDirectory::from_retained_with_control(
                kio.try_clone()
                    .map_err(|error| KioError::io(error.to_string(), ".kio"))?,
                self.kio_dir.clone(),
                self.control().clone(),
            );
        }
        StoreDirectory::open_with_control(&self.kio_dir, self.control().clone())
    }

    pub(super) fn metadata_text(&self, leaf: &str, max_bytes: u64) -> Result<String> {
        let bytes = self
            .store_directory()?
            .read_optional(Path::new(leaf), max_bytes)?
            .ok_or_else(|| KioError::schema(format!("{leaf} is missing")))?;
        String::from_utf8(bytes).map_err(|_| KioError::schema(format!("{leaf} must be UTF-8")))
    }

    pub(super) fn replace_metadata(&self, leaf: &str, bytes: &[u8]) -> Result<()> {
        self.store_directory()?.write_atomic(
            Path::new(leaf),
            bytes,
            crate::store_dir::Publication::Replace,
        )
    }

    pub fn validate(&self) -> Result<()> {
        self.ensure_no_pending_managed_restore()?;
        self.validate_store_authority()?;
        self.validate_config()?;
        self.validate_scope()?;
        self.head_commit_hash()?;
        self.validate_manifest()?;
        Ok(())
    }

    /// Revalidate the public names against the retained capabilities, then
    /// re-open each CAS namespace descriptor-relative.  This deliberately
    /// does not read or create `management.json`: enrollment is owned by the
    /// application layer, while every ordinary repository open still needs a
    /// current named-identity and store-layout safety check.
    fn validate_store_authority(&self) -> Result<()> {
        if let (Some(root), Some(kio)) = (self.bound_root.as_deref(), self.bound_kio.as_deref()) {
            let binding = crate::management::ManagementBinding::from_retained_with_control(
                root.try_clone()
                    .map_err(|error| KioError::io(error.to_string(), "."))?,
                kio.try_clone()
                    .map_err(|error| KioError::io(error.to_string(), ".kio"))?,
                self.canonical_root.clone(),
                self.control().clone(),
            )?;
            binding.revalidate()?;
            validate_retained_store_directory(kio, &self.kio_dir)?;
            self.store.validate_bound_layout()?;
            return Ok(());
        }
        validate_store_directory(&self.kio_dir)
    }

    /// Replace the scope configuration only after the same schema and semantic
    /// checks used by [`Self::validate`].  Internal child indexing uses this to
    /// persist the ancestor-derived ignore policy before any scan or task can
    /// observe the child scope.  Callers must provide the complete document so
    /// unrelated child-local configuration is retained deliberately.
    pub fn replace_config_value(&self, value: toml::Value) -> Result<()> {
        let json_value =
            serde_json::to_value(&value).map_err(|err| KioError::schema(err.to_string()))?;
        validate_json_schema(SchemaKind::Config, &json_value)?;
        enforce_scope_config_semantics(&json_value)?;
        let text = toml::to_string(&value).map_err(|err| KioError::schema(err.to_string()))?;
        self.replace_metadata("config.toml", text.as_bytes())
    }

    /// Read the current retained `config.toml` document after schema and
    /// scope-semantic validation. The returned text is suitable as the exact
    /// expected value for [`Self::compare_replace_config_document`].
    pub fn read_config_document(&self) -> Result<String> {
        self.validate_store_authority()?;
        let result = (|| {
            let text = self.metadata_text("config.toml", MAX_BOUND_CONFIG_BYTES)?;
            validate_scope_config_document(&text)?;
            Ok(text)
        })();
        let authority_after = self.validate_store_authority();
        match (result, authority_after) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Ok(text), Ok(())) => Ok(text),
        }
    }

    /// Replace retained `config.toml` only when its current bytes exactly
    /// equal `expected`. This preserves comments and ordering produced by an
    /// application-layer editor while refusing a stale overwrite.
    pub fn compare_replace_config_document(&self, expected: &str, replacement: &str) -> Result<()> {
        if replacement.len() as u64 > MAX_BOUND_CONFIG_BYTES {
            return Err(KioError::schema("config.toml exceeds the byte limit"));
        }
        validate_scope_config_document(replacement)?;
        self.validate_store_authority()?;
        let _lock = self.lock_store_in_session(&self.lock_session)?;
        self.validate_store_authority()?;
        let result = (|| {
            let directory = self.store_directory()?;
            let current = read_bound_config_document(&directory)?;
            if current != expected {
                return Err(KioError::new(
                    "KIO-E-CONFIG-CONFLICT-001",
                    "config.toml changed before the requested replacement; retry from a fresh document",
                    json!({}),
                    ExitCode::AuthError,
                ));
            }
            directory.write_atomic(
                Path::new("config.toml"),
                replacement.as_bytes(),
                crate::store_dir::Publication::Replace,
            )
        })();
        let authority_after = self.validate_store_authority();
        match (result, authority_after) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    /// Read approval state through this repository's retained `.kio`
    /// capability. The public `.kio` binding is checked both before and after
    /// the read so a replaced scope cannot be silently treated as this one.
    pub fn read_network_approvals(&self) -> Result<Vec<Value>> {
        self.with_retained_scope_approval_read(network_approvals_from_value)
    }

    /// Whether this retained scope has recorded an approval lifecycle.
    pub fn network_approvals_initialized(&self) -> Result<bool> {
        self.with_retained_scope_approval_read(network_approvals_initialized_from_value)
    }

    /// Return the one pending approval intent from the retained scope.
    pub fn read_network_approval_pending(&self) -> Result<Option<Value>> {
        self.with_retained_scope_approval_read(network_approval_pending_from_value)
    }

    /// Test a strict active approval row against the retained scope.
    pub fn network_approval_active(
        &self,
        tool_id: &str,
        execution_mode: &str,
        tool_profile_hash: &str,
    ) -> Result<bool> {
        self.with_retained_scope_approval_read(|value| {
            network_approval_active_from_value(value, tool_id, execution_mode, tool_profile_hash)
        })
    }

    /// Test whether this retained scope has any active row for `tool_id`.
    pub fn network_approval_row_present(&self, tool_id: &str) -> Result<bool> {
        self.with_retained_scope_approval_read(|value| {
            network_approval_row_present_from_value(value, tool_id)
        })
    }

    /// Durably record an approval intent through the retained store boundary.
    pub fn write_network_approval_pending(&self, pending: Value) -> Result<()> {
        self.with_retained_scope_approval_write(|directory| {
            write_network_approval_pending_in_directory(directory, pending)
        })
    }

    /// Publish a pending approval only when the retained current state still
    /// equals `expected_pending`.
    pub fn publish_network_approval(
        &self,
        row: Value,
        expected_pending: Option<&Value>,
    ) -> Result<()> {
        self.with_retained_scope_approval_write(|directory| {
            publish_network_approval_in_directory(directory, row, expected_pending)
        })
    }

    /// Revoke matching approval rows and any matching pending intent through
    /// the retained store boundary.
    pub fn revoke_network_approval(
        &self,
        tool_id: Option<&str>,
        revoked_at: &str,
    ) -> Result<NetworkRevokeOutcome> {
        self.with_retained_scope_approval_write(|directory| {
            revoke_network_approval_in_directory(directory, tool_id, revoked_at)
        })
    }

    fn with_retained_scope_approval_read<T>(
        &self,
        operation: impl FnOnce(&Value) -> Result<T>,
    ) -> Result<T> {
        self.validate_store_authority()?;
        let result = (|| {
            let directory = self.store_directory()?;
            let value = read_bound_scope_json_value(&directory)?;
            operation(&value)
        })();
        let authority_after = self.validate_store_authority();
        match (result, authority_after) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Ok(value), Ok(())) => Ok(value),
        }
    }

    fn with_retained_scope_approval_write<T>(
        &self,
        operation: impl FnOnce(&StoreDirectory) -> Result<T>,
    ) -> Result<T> {
        self.validate_store_authority()?;
        let _lock = self.lock_store_in_session(&self.lock_session)?;
        self.validate_store_authority()?;
        let result = (|| {
            let directory = self.store_directory()?;
            operation(&directory)
        })();
        let authority_after = self.validate_store_authority();
        match (result, authority_after) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Ok(value), Ok(())) => Ok(value),
        }
    }
}
