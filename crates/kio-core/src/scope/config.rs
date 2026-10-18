use super::*;

impl Repository {
    /// `kio_format_version` is a `.kio/scope.json`-only concept. A config file
    /// carrying that retired key is rejected by the strict config schema.
    pub(super) fn validate_config(&self) -> Result<()> {
        self.validated_config_value()?;
        Ok(())
    }

    fn validated_config_value(&self) -> Result<Value> {
        let text = self.metadata_text("config.toml", MAX_BOUND_CONFIG_BYTES)?;
        let toml: toml::Value =
            toml::from_str(&text).map_err(|error| KioError::schema(error.to_string()))?;
        let value =
            serde_json::to_value(&toml).map_err(|error| KioError::schema(error.to_string()))?;
        validate_json_schema(SchemaKind::Config, &value)?;
        // R12-2 / R12-1: reject documented-but-unwired values the schema can only
        // type-check (e.g. `allowed_scope != "."`) LOUDLY, so a scope config never
        // silently ignores a policy the user set.
        enforce_scope_config_semantics(&value)?;
        Ok(value)
    }

    /// Read, validate, and freeze the chunking configuration used to construct
    /// one tree. The returned hash is derived from the exact parsed
    /// descriptor-bound `config.toml` bytes, never a later mutable lookup.
    pub(super) fn effective_chunking_config_hash(&self) -> Result<String> {
        let value = self.validated_config_value()?;

        let chunking = value.get("chunking").and_then(Value::as_object);
        let strategy = chunking
            .and_then(|chunking| chunking.get("strategy"))
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_CHUNKING_STRATEGY);
        let max_chars = match chunking.and_then(|chunking| chunking.get("max_chars")) {
            None => DEFAULT_CHUNKING_MAX_CHARS,
            Some(number) => number
                .as_u64()
                .filter(|value| *value > 0)
                .and_then(|value| usize::try_from(value).ok().map(|_| value))
                .ok_or_else(|| {
                    KioError::schema(
                        "chunking.max_chars must be a positive integer representable on this platform",
                    )
                })?,
        };
        chunking_config_hash(strategy, max_chars)
    }

    pub(super) fn validate_scope(&self) -> Result<()> {
        self.validated_scope_id().map(|_| ())
    }

    /// The exact `kio_format_version` judgment runs before current JSON Schema
    /// validation. Missing, malformed, older, and newer versions all fail with
    /// `KIO-E-STORE-VERSION-001` / exit 8; this ordering supplies a stable
    /// rejection code and does not permit read-only degradation.
    pub(super) fn validated_scope_id(&self) -> Result<String> {
        let text = self.metadata_text("scope.json", MAX_BOUND_CONFIG_BYTES)?;
        let value: Value =
            serde_json::from_str(&text).map_err(|err| KioError::schema(err.to_string()))?;
        validate_scope_json_value(&value)?;
        validate_stable_store_gate(&self.store_directory()?)?;
        let Some(scope_id) = value.get("scope_id").and_then(Value::as_str) else {
            return Err(KioError::schema("scope.json missing scope_id"));
        };
        if scope_id.is_empty() {
            return Err(KioError::schema("scope_id is empty"));
        }
        if !is_ulid(scope_id) {
            return Err(KioError::schema("scope_id must be a ULID"));
        }
        Ok(scope_id.to_owned())
    }

    pub(super) fn validate_manifest(&self) -> Result<()> {
        let text = self.metadata_text("manifest.json", MAX_BOUND_CONFIG_BYTES)?;
        let value: Value =
            serde_json::from_str(&text).map_err(|err| KioError::schema(err.to_string()))?;
        validate_json_schema(SchemaKind::Manifest, &value)?;
        if !value.is_object() {
            return Err(KioError::schema("manifest.json must be an object"));
        }
        let Some(files) = value.get("files") else {
            return Err(KioError::schema("manifest.json missing files"));
        };
        let files = files
            .as_array()
            .ok_or_else(|| KioError::schema("manifest.files must be an array"))?;
        for file in files {
            let object = file
                .as_object()
                .ok_or_else(|| KioError::schema("manifest file entry must be an object"))?;
            let path = object
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| KioError::schema("manifest file entry missing path"))?;
            if path.is_empty() || path.contains('/') {
                return Err(KioError::path(
                    "manifest file path must be a direct child file name",
                    path.to_owned(),
                ));
            }
            let raw_hash = object
                .get("raw_hash")
                .and_then(Value::as_str)
                .ok_or_else(|| KioError::schema("manifest file entry missing raw_hash"))?;
            if !is_hash(raw_hash) {
                return Err(KioError::schema("manifest raw_hash must be a hash"));
            }
            let status = object
                .get("status")
                .and_then(Value::as_str)
                .ok_or_else(|| KioError::schema("manifest file entry missing status"))?;
            if !matches!(status, "new" | "modified" | "deleted" | "unchanged") {
                return Err(KioError::schema("manifest status has invalid value"));
            }
        }
        Ok(())
    }

    /// Merge the current working tree into `manifest.json`, preserving rows for
    /// paths that vanished (`03 §8`: never DELETE a files row; set
    /// `status="deleted"` and keep the last observed `raw_hash`). A path that
    /// reappears recovers from `deleted` to `modified`/`unchanged`; this is
    /// exercised by `s2_manifest_retains_deleted_rows_and_recovers`.
    ///
    /// The previous state is sourced from the prior HEAD tree (the durable
    /// truth, `03 §2`) merged with the prior manifest's `deleted` rows (older
    /// deletions that no tree carries). The manifest's live rows are never
    /// trusted: a stale or hand-edited manifest cannot lose a deletion this way.
    pub(super) fn manifest_value(
        &self,
        tree: &TreeObject,
        prior_tree: Option<&TreeObject>,
    ) -> Result<Value> {
        let mut previous: BTreeMap<String, String> = prior_tree
            .map(|prior| {
                prior
                    .entries
                    .iter()
                    .map(|entry| (entry.path.clone(), entry.raw_hash.clone()))
                    .collect()
            })
            .unwrap_or_default();
        for (path, raw_hash) in self.read_manifest_deleted_hashes()? {
            previous.entry(path).or_insert(raw_hash);
        }
        let current: BTreeMap<&str, &str> = tree
            .entries
            .iter()
            .map(|entry| (entry.path.as_str(), entry.raw_hash.as_str()))
            .collect();

        // BTreeMap keyed by path gives a deterministic, path-sorted file list.
        let mut rows: BTreeMap<String, Value> = BTreeMap::new();

        for entry in &tree.entries {
            let status = match previous.get(&entry.path) {
                None => "new",
                Some(prev) if *prev != entry.raw_hash => "modified",
                Some(_) => "unchanged",
            };
            rows.insert(
                entry.path.clone(),
                json!({ "path": entry.path, "raw_hash": entry.raw_hash, "status": status }),
            );
        }

        // Retain vanished paths as deleted rows carrying their last raw_hash.
        for (path, raw_hash) in &previous {
            if !current.contains_key(path.as_str()) {
                rows.insert(
                    path.clone(),
                    json!({ "path": path, "raw_hash": raw_hash, "status": "deleted" }),
                );
            }
        }

        let files = rows.into_values().collect::<Vec<_>>();
        Ok(json!({
            "schema_version": 1,
            "files": files,
            "updated_at": self.now_utc_seconds(),
        }))
    }

    pub(super) fn write_manifest_value(&self, value: &Value) -> Result<()> {
        validate_json_schema(SchemaKind::Manifest, value)?;
        let bytes =
            serde_json::to_vec_pretty(value).map_err(|err| KioError::schema(err.to_string()))?;
        if bytes.len() as u64 > MAX_PUBLICATION_JOURNAL_BYTES {
            return Err(KioError::schema(
                "manifest exceeds the publication size limit",
            ));
        }
        self.replace_metadata("manifest.json", &bytes)
    }

    /// Read the current `manifest.json` `deleted` rows as a
    /// `path -> last raw_hash` map. Live rows are intentionally excluded — the
    /// prior HEAD tree is the authoritative source for those (see
    /// `write_manifest`).
    /// Returns an empty map when the manifest is absent. The manifest is schema
    /// validated before `snapshot` runs, so entries are well formed here.
    fn read_manifest_deleted_hashes(&self) -> Result<BTreeMap<String, String>> {
        let mut map = BTreeMap::new();
        let Some(bytes) = self
            .store_directory()?
            .read_optional(Path::new("manifest.json"), MAX_BOUND_CONFIG_BYTES)?
        else {
            return Ok(map);
        };
        let text = String::from_utf8(bytes)
            .map_err(|_| KioError::schema("manifest.json must be UTF-8"))?;
        let value: Value =
            serde_json::from_str(&text).map_err(|err| KioError::schema(err.to_string()))?;
        if let Some(files) = value.get("files").and_then(Value::as_array) {
            for file in files {
                if file.get("status").and_then(Value::as_str) != Some("deleted") {
                    continue;
                }
                if let (Some(entry_path), Some(raw_hash)) = (
                    file.get("path").and_then(Value::as_str),
                    file.get("raw_hash").and_then(Value::as_str),
                ) {
                    map.insert(entry_path.to_owned(), raw_hash.to_owned());
                }
            }
        }
        Ok(map)
    }

    pub(super) fn publish_tool_lock(&self) -> Result<String> {
        let (expected_hash, bytes) = self.tool_lock_identity()?;
        let published_hash = self
            .store
            .write_content_object(ContentObjectKind::Toollock, &bytes)?;
        if published_hash != expected_hash {
            return Err(KioError::schema(
                "published immutable tool-lock hash differs from mutable authority",
            ));
        }
        Ok(published_hash)
    }

    pub(super) fn tool_lock_identity(&self) -> Result<(String, Vec<u8>)> {
        let text = self.metadata_text("tool-lock.json", MAX_BOUND_CONFIG_BYTES)?;
        let value: Value =
            serde_json::from_str(&text).map_err(|err| KioError::schema(err.to_string()))?;
        let canonical = canonical_tool_lock_value(&value)?;
        let bytes = canonical_json_bytes(&canonical)?;
        Ok((crate::cas::hash_bytes(&bytes), bytes))
    }
}
