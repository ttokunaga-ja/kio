use super::*;

impl ObjectStore {
    #[must_use]
    pub fn control(&self) -> &crate::control::ControlHandle {
        &self.control
    }

    #[must_use]
    pub fn rebind_with_control(&self, control: crate::control::ControlHandle) -> Self {
        let mut store = self.clone();
        store.control = control;
        store
    }

    /// Capture an exact physical content file through already-retained CAS
    /// parents. Quarantine residue is only a candidate; hash verification and
    /// the caller's fresh orphan proof are required on every new plan.
    pub fn plan_content_removal(
        &self,
        kind: ContentObjectKind,
        hash: &str,
        budget: &mut crate::store_dir::RemovalBudget,
    ) -> Result<Option<PlannedContentRemoval>> {
        self.read_access().charge(0)?;
        if !matches!(kind, ContentObjectKind::Prepared | ContentObjectKind::Image) || !is_hash(hash)
        {
            return Err(KioError::invalid_usage(
                "planned orphan removal requires a prepared/image content hash",
            ));
        }
        let bound = self.bound.as_ref().ok_or_else(|| {
            KioError::invalid_usage("planned removal requires a retained ObjectStore")
        })?;
        let (parent, leaf) = match bound.content_fanout(kind, hash, false) {
            Ok(slot) => slot,
            Err(error) if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => return Ok(None),
            Err(error) => return Err(error),
        };
        crate::store_dir::require_same_filesystem(
            &bound.kio,
            &parent,
            Path::new("planned CAS parent"),
        )?;
        let directory = crate::store_dir::StoreDirectory::from_retained_with_control(
            parent,
            PathBuf::from("planned CAS parent"),
            self.control.clone(),
        )?;
        crate::private_fs::verify_private_directory_handle(&directory)?;
        if directory.contains_entry(Path::new(&leaf))? {
            directory.ensure_owner_private(Path::new(&leaf))?;
        }
        let Some(target) = crate::store_dir::PlannedFileRemoval::capture_content(
            &directory,
            Path::new(&leaf),
            kind.max_bytes(),
            budget,
            self.read_access(),
        )?
        else {
            return Ok(None);
        };
        if target.content_hash() != hash {
            return Err(corrupt_object_error(
                Path::new(&leaf),
                "planned content hash does not match filename",
                hash,
                Some(target.content_hash()),
            ));
        }
        Ok(Some(PlannedContentRemoval(target)))
    }

    #[must_use]
    pub fn new(kio_dir: impl Into<PathBuf>) -> Self {
        Self::new_with_control(kio_dir, crate::control::ControlHandle::default())
    }

    #[must_use]
    pub fn new_with_control(
        kio_dir: impl Into<PathBuf>,
        control: crate::control::ControlHandle,
    ) -> Self {
        Self {
            control,
            reads: CasReadAccess::Unmetered,
            kio_dir: Some(kio_dir.into()),
            bound: None,
        }
    }

    // A retained store deliberately has no fallback pathname. Incomplete API
    // migrations must fail before touching the process working directory.
    pub(super) fn ambient_kio_dir(&self) -> Result<&Path> {
        self.kio_dir.as_deref().ok_or_else(|| {
            KioError::invalid_usage("a retained ObjectStore has no ambient pathname")
        })
    }

    /// Construct a CAS capability rooted in an already retained `.kio`
    /// directory.  This is intentionally narrow: it binds precisely the
    /// namespaces used by the scheduled snapshot writer, and refuses a
    /// missing/replaced/symlinked object namespace rather than recovering via
    /// the public pathname.
    pub fn from_bound_kio(kio: &File) -> Result<Self> {
        Self::from_bound_kio_with_control(kio, crate::control::ControlHandle::default())
    }

    pub fn from_bound_kio_with_control(
        kio: &File,
        control: crate::control::ControlHandle,
    ) -> Result<Self> {
        let objects = bound_open_dir(kio, "objects")?;
        let raw = bound_open_dir(&objects, ObjectKind::Raw.directory())?;
        let trees = bound_open_dir(&objects, ObjectKind::Tree.directory())?;
        let commits = bound_open_dir(&objects, ObjectKind::Commit.directory())?;
        // Content namespaces are historically lazy. Bind an existing one but
        // leave an absent kind absent: constructing a read-only Repository
        // must not repair its on-disk layout. A writer creates a missing base
        // through `objects` and retains it before its first operation.
        let optional_dir = |name: &str| match bound_open_dir(&objects, name) {
            Ok(dir) => Ok(Some(Arc::new(dir))),
            Err(error) if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => Ok(None),
            Err(error) => Err(KioError::new(
                "KIO-E-STORE-NAMESPACE-001",
                "cannot bind object namespace without following links",
                serde_json::json!({
                    "namespace": name,
                    "cause": error.error_code(),
                }),
                crate::ExitCode::PermanentFailure,
            )),
        };
        let chunks = optional_dir("chunks")?;
        let prepared = optional_dir(ContentObjectKind::Prepared.directory())?;
        let image = optional_dir(ContentObjectKind::Image.directory())?;
        let embeddings = optional_dir(ContentObjectKind::Embedding.directory())?;
        let manifests = optional_dir(ContentObjectKind::Manifest.directory())?;
        let toollocks = optional_dir(ContentObjectKind::Toollock.directory())?;
        let normalized_units = optional_dir(ContentObjectKind::NormalizedUnit.directory())?;
        Ok(Self {
            control,
            reads: CasReadAccess::Unmetered,
            kio_dir: None,
            bound: Some(BoundObjectDirs {
                kio: {
                    let retained = kio
                        .try_clone()
                        .map_err(|e| KioError::io(e.to_string(), ".kio"))?;
                    // The borrowed caller handle remains caller-owned. Our
                    // persistent clone must open by ID on Windows so it does
                    // not retain a named link after the caller closes theirs.
                    cas_semantics::retain_bound_kio(retained)?
                },
                objects: Arc::new(objects),
                raw: Arc::new(raw),
                trees: Arc::new(trees),
                commits: Arc::new(commits),
                chunks: Arc::new(Mutex::new(chunks)),
                prepared: Arc::new(Mutex::new(prepared)),
                image: Arc::new(Mutex::new(image)),
                embeddings: Arc::new(Mutex::new(embeddings)),
                manifests: Arc::new(Mutex::new(manifests)),
                toollocks: Arc::new(Mutex::new(toollocks)),
                normalized_units: Arc::new(Mutex::new(normalized_units)),
            }),
        })
    }

    pub fn write_raw(&self, bytes: &[u8]) -> Result<String> {
        let hash = hash_bytes(bytes);
        self.write_object_bytes(ObjectKind::Raw, &hash, bytes)?;
        Ok(hash)
    }

    pub fn stage_raw_from_reader<R: Read>(
        &self,
        reader: &mut R,
        max_bytes: u64,
    ) -> Result<BoundRawStage> {
        self.bound
            .as_ref()
            .ok_or_else(|| {
                KioError::invalid_usage("bound raw staging requires a retained ObjectStore")
            })?
            .stage_raw_from_reader(reader, max_bytes)
    }

    pub fn cleanup_bound_raw_stages(&self) -> Result<()> {
        self.bound
            .as_ref()
            .ok_or_else(|| {
                KioError::invalid_usage("bound raw cleanup requires a retained ObjectStore")
            })?
            .cleanup_raw_ingest_orphans()
    }

    pub fn validate_bound_layout(&self) -> Result<()> {
        self.bound
            .as_ref()
            .ok_or_else(|| {
                KioError::invalid_usage("bound layout validation requires a retained ObjectStore")
            })?
            .validate_layout()
    }

    pub fn publish_bound_raw_stage(&self, stage: BoundRawStage) -> Result<(String, u64)> {
        self.bound
            .as_ref()
            .ok_or_else(|| {
                KioError::invalid_usage(
                    "bound raw stage publication requires a retained ObjectStore",
                )
            })?
            .publish_raw_stage(stage)
    }

    /// Repair one corrupt, single-representation raw CAS slot with verified
    /// working bytes. The destination is derived only from `expected_hash`; a
    /// noncanonical or conflicting representation remains fail-closed. The caller must
    /// hold the scope store lock for the entire operation.
    pub fn repair_raw(&self, expected_hash: &str, bytes: &[u8]) -> Result<bool> {
        if let Some(bound) = &self.bound {
            return bound.repair_raw(expected_hash, bytes);
        }
        if !is_hash(expected_hash) || hash_bytes(bytes) != expected_hash {
            return Err(KioError::invalid_usage(
                "raw repair bytes do not match the expected hash",
            ));
        }
        if bytes.len() as u64 > ObjectKind::Raw.max_bytes() {
            return Err(object_size_error(
                ObjectKind::Raw,
                ObjectKind::Raw.max_bytes(),
                bytes.len() as u64,
            ));
        }
        self.ensure_object_parent(ObjectKind::Raw, expected_hash)?;
        let Some(path) = self.existing_object_path(ObjectKind::Raw, expected_hash)? else {
            self.write_object_bytes(ObjectKind::Raw, expected_hash, bytes)?;
            return Ok(true);
        };
        let path = path.as_path();
        if read_verified_object(
            &CasReadAccess::Unmetered,
            path,
            ObjectKind::Raw,
            expected_hash,
            false,
        )
        .is_ok()
        {
            return Ok(false);
        }

        // Re-open through the hardened no-follow/single-link boundary and consume
        // the complete corrupt body before authorizing replacement. Unsafe links
        // and path swaps fail before any namespace mutation.
        let mut corrupt = open_private_cas_file(path)?;
        let metadata = corrupt.metadata().kio_io(path)?;
        if metadata.len() > ObjectKind::Raw.max_bytes() {
            return Err(object_size_error(
                ObjectKind::Raw,
                ObjectKind::Raw.max_bytes(),
                metadata.len(),
            ));
        }
        let mut actual_hasher = Sha256::new();
        let mut buffer = [0_u8; CAS_STREAM_BUFFER_BYTES];
        loop {
            let count = corrupt.read(&mut buffer).kio_io(path)?;
            if count == 0 {
                break;
            }
            actual_hasher.update(&buffer[..count]);
        }
        let actual = format!("sha256:{}", lower_hex(&actual_hasher.finalize()));
        if actual == expected_hash {
            return Err(corrupt_object_error(
                path,
                "raw repair observed an unstable CAS slot",
                expected_hash,
                Some(&actual),
            ));
        }

        let parent = path
            .parent()
            .ok_or_else(|| KioError::io("CAS path has no parent", path.display().to_string()))?;
        let (temp_path, mut temp, temp_parent) = create_private_temp(parent)?;
        let mut corrupt = Some(corrupt);
        let source = cas_semantics::retain_repair_source(&mut corrupt);
        let result = (|| -> Result<()> {
            temp.write_all(bytes).kio_io(&temp_path)?;
            temp.sync_all().kio_io(&temp_path)?;
            drop(temp);

            verify_named_cas_directory(&temp_parent, parent)?;
            let quarantine = create_repair_quarantine(path, source.file())?;
            verify_named_cas_directory(&temp_parent, parent)?;
            // MoveFileEx requires both path operands to be closed on Windows.
            // The verified quarantine hard link now pins the exact corrupt
            // object for rollback, so the original read handle can be released
            // without reopening or trusting its pathname.
            source.prepare_replace();
            if let Err(error) = replace_file(&temp_path, path) {
                let _ = fs::remove_file(&quarantine);
                return Err(error);
            }
            if let Err(error) = read_verified_object(
                &CasReadAccess::Unmetered,
                path,
                ObjectKind::Raw,
                expected_hash,
                false,
            ) {
                let _ = replace_file(&quarantine, path);
                return Err(error);
            }
            fs::remove_file(&quarantine).kio_io(&quarantine)?;
            // R23-07: propagate. The rename that published the repaired bytes
            // and the quarantine unlink above are both only as durable as this
            // entry, and the unlink already fails the repair loudly — swallowing
            // the sync that backs it let `repair_raw` answer `true` for a repair
            // that a crash could still undo. Retry is idempotent: a slot that
            // already verifies returns `false` before any mutation.
            sync_directory(parent).kio_io(parent)?;
            verify_named_cas_directory(&temp_parent, parent)
        })();
        if result.is_err() {
            let _ = remove_private_temp(&temp_parent, &temp_path);
        }
        result.map(|()| true)
    }

    pub fn write_json(&self, kind: ObjectKind, value: &Value) -> Result<(String, Vec<u8>)> {
        let bytes = canonical_json_bytes(value)?;
        let hash = hash_bytes(&bytes);
        self.write_object_bytes(kind, &hash, &bytes)?;
        Ok((hash, bytes))
    }

    /// Atomically publish a semantic chunk object under its identity hash.
    pub fn write_chunk(&self, chunk: &ChunkObject) -> Result<String> {
        let hash = chunk.identity_hash()?;
        let value =
            serde_json::to_value(chunk).map_err(|error| KioError::schema(error.to_string()))?;
        let bytes = canonical_json_bytes(&value)?;
        if bytes.len() as u64 > MAX_CHUNK_OBJECT_BYTES {
            return Err(chunk_size_error(bytes.len() as u64));
        }
        if let Some(bound) = &self.bound {
            let (parent, leaf) = bound.chunk_fanout(&hash, true)?;
            bound_write_semantic(&parent, &leaf, &bytes, MAX_CHUNK_OBJECT_BYTES)?;
            bound_read_chunk_accounted(&CasReadAccess::Unmetered, &parent, &leaf, &hash)
                .map_err(|e| e.error)?;
            return Ok(hash);
        }
        self.ensure_chunk_parent(&hash)?;
        if let Some(existing) = self.existing_chunk_path(&hash)? {
            verify_existing_bytes(&existing, &hash, &bytes)?;
            read_chunk_path(&CasReadAccess::Unmetered, &existing, &hash)?;
            return Ok(hash);
        }

        let path = self.chunk_path(&hash)?;
        let (temp_path, mut temp, temp_parent) = create_private_temp(
            path.parent()
                .ok_or_else(|| KioError::io("path has no parent", path.display().to_string()))?,
        )?;
        let result = (|| -> Result<()> {
            temp.write_all(&bytes).kio_io(&temp_path)?;
            temp.sync_all().kio_io(&temp_path)?;
            drop(temp);
            publish_temp_object(
                &temp_parent,
                &temp_path,
                &path,
                &hash,
                bytes.len() as u64,
                Some(&bytes),
            )?;
            let Some(published_path) = self.existing_chunk_path(&hash)? else {
                return Err(KioError::not_found(&hash));
            };
            let (object, _) = read_chunk_path(&CasReadAccess::Unmetered, &published_path, &hash)?;
            if &object != chunk {
                return Err(chunk_corrupt_error(
                    "published chunk object changed identity or text",
                    Some(&published_path),
                ));
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = remove_private_temp(&temp_parent, &temp_path);
        }
        result.map(|()| hash)
    }

    /// Read one required chunk namespace and verify semantic identity, exact
    /// schema, text hash, bounded bytes, and rejection of noncanonical or
    /// conflicting representations.
    pub fn read_chunk(&self, hash: &str) -> Result<ChunkObject> {
        self.read_chunk_with_size(hash).map(|(chunk, _)| chunk)
    }

    /// Read a semantic chunk and return the exact logical object byte count.
    pub fn read_chunk_with_size(&self, hash: &str) -> Result<(ChunkObject, u64)> {
        if self.bound.is_some() {
            return self.read_chunk_accounted(hash).map_err(|e| e.error);
        }
        if !is_hash(hash) {
            return Err(KioError::invalid_usage("invalid chunk hash"));
        }
        if !self.validate_chunk_parent(hash)? {
            return Err(KioError::not_found(hash));
        }
        let Some(path) = self.existing_chunk_path(hash)? else {
            return Err(KioError::not_found(hash));
        };
        let (object, bytes) = read_chunk_path(&self.reads, &path, hash)?;
        Ok((object, bytes.len() as u64))
    }

    pub fn read_chunk_accounted(
        &self,
        hash: &str,
    ) -> std::result::Result<(ChunkObject, u64), AccountedReadError> {
        if let Some(bound) = &self.bound {
            let (parent, leaf) =
                bound
                    .chunk_fanout(hash, false)
                    .map_err(|error| AccountedReadError {
                        error,
                        consumed_bytes: 0,
                    })?;
            return bound_read_chunk_accounted(&self.reads, &parent, &leaf, hash);
        }
        let path = (|| -> Result<PathBuf> {
            if !is_hash(hash) {
                return Err(KioError::invalid_usage("invalid chunk hash"));
            }
            if !self.validate_chunk_parent(hash)? {
                return Err(KioError::not_found(hash));
            }
            self.existing_chunk_path(hash)?
                .ok_or_else(|| KioError::not_found(hash))
        })()
        .map_err(|error| AccountedReadError {
            error,
            consumed_bytes: 0,
        })?;
        let (result, consumed_bytes) = read_chunk_path_accounted(&self.reads, &path, hash);
        let (object, _) = result.map_err(|error| AccountedReadError {
            error,
            consumed_bytes,
        })?;
        Ok((object, consumed_bytes))
    }

    /// Stream-verify a referenced prepared/image content object without
    /// materializing its body.
    pub fn inspect_content_object(
        &self,
        kind: ContentObjectKind,
        hash: &str,
    ) -> Result<StoredContentObjectMetadata> {
        if let Some(bound) = &self.bound {
            return bound.inspect_content_object(&self.reads, kind, hash);
        }
        if !is_hash(hash) {
            return Err(KioError::invalid_usage("invalid content object hash"));
        }
        if !self.validate_content_parent(kind, hash)? {
            return Err(KioError::not_found(hash));
        }
        let Some(path) = self.existing_content_path(kind, hash)? else {
            return Err(KioError::not_found(hash));
        };
        let size_bytes = verify_content_object_path(&self.reads, &path, kind, hash)?;
        Ok(StoredContentObjectMetadata {
            kind,
            hash: hash.to_owned(),
            size_bytes,
        })
    }

    pub fn inspect_content_accounted(
        &self,
        kind: ContentObjectKind,
        hash: &str,
    ) -> std::result::Result<StoredContentObjectMetadata, AccountedReadError> {
        if let Some(bound) = &self.bound {
            return bound.inspect_content_accounted(&self.reads, kind, hash);
        }
        let path = (|| -> Result<PathBuf> {
            if !is_hash(hash) {
                return Err(KioError::invalid_usage("invalid content object hash"));
            }
            if !self.validate_content_parent(kind, hash)? {
                return Err(KioError::not_found(hash));
            }
            self.existing_content_path(kind, hash)?
                .ok_or_else(|| KioError::not_found(hash))
        })()
        .map_err(|error| AccountedReadError {
            error,
            consumed_bytes: 0,
        })?;
        let (result, consumed_bytes) =
            verify_content_object_path_accounted(&self.reads, &path, kind, hash);
        result.map_err(|error| AccountedReadError {
            error,
            consumed_bytes,
        })?;
        Ok(StoredContentObjectMetadata {
            kind,
            hash: hash.to_owned(),
            size_bytes: consumed_bytes,
        })
    }

    pub fn embedding_path(&self, hash: &str) -> Result<PathBuf> {
        fanout_path(self.ambient_kio_dir()?.join("objects/embeddings"), hash)
    }

    /// Publish one embedding vector under its identity hash (03 §8.1).
    ///
    /// Idempotent by content: the same vector for the same identity re-verifies
    /// and returns. A DIFFERENT vector under the same identity is corruption,
    /// not an update — the identity names the profile, the target text and the
    /// context, so a deterministic adapter cannot legitimately produce two.
    pub fn write_embedding(&self, embedding: &EmbeddingObject) -> Result<String> {
        let hash = embedding.identity_hash()?;
        let bytes = embedding.to_bytes()?;
        if bytes.len() as u64 > MAX_EMBEDDING_OBJECT_BYTES {
            return Err(KioError::new(
                "KIO-E-STORE-OBJECT-OVERSIZED-001",
                "embedding object exceeds its byte limit",
                serde_json::json!({
                    "object_type": "embedding",
                    "max_bytes": MAX_EMBEDDING_OBJECT_BYTES,
                    "actual_bytes": bytes.len() as u64,
                }),
                crate::ExitCode::PermanentFailure,
            ));
        }
        if let Some(bound) = &self.bound {
            let (parent, leaf) = bound.content_fanout(ContentObjectKind::Embedding, &hash, true)?;
            bound_write_semantic(&parent, &leaf, &bytes, MAX_EMBEDDING_OBJECT_BYTES)?;
            bound_read_embedding(&CasReadAccess::Unmetered, &parent, &leaf, &hash)?;
            return Ok(hash);
        }
        self.ensure_embedding_parent(&hash)?;
        let path = self.embedding_path(&hash)?;
        if fs::symlink_metadata(&path).is_ok() {
            verify_existing_bytes(&path, &hash, &bytes)?;
            return Ok(hash);
        }
        let (temp_path, mut temp, temp_parent) = create_private_temp(
            path.parent()
                .ok_or_else(|| KioError::io("path has no parent", path.display().to_string()))?,
        )?;
        let result = (|| -> Result<()> {
            temp.write_all(&bytes).kio_io(&temp_path)?;
            temp.sync_all().kio_io(&temp_path)?;
            drop(temp);
            publish_temp_object(
                &temp_parent,
                &temp_path,
                &path,
                &hash,
                bytes.len() as u64,
                Some(&bytes),
            )?;
            // Read back through the same parser a rebuild will use, so a
            // publish that somehow lands unreadable fails HERE rather than in
            // the `repair rebuild-db` that was counting on it.
            let published = self.read_embedding(&hash)?;
            if &published != embedding {
                return Err(embedding_corrupt_error(
                    "published embedding object changed identity or vector",
                    Some(&path),
                ));
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = remove_private_temp(&temp_parent, &temp_path);
        }
        result.map(|()| hash)
    }

    /// Read and fully verify one embedding object, including that its identity
    /// hashes back to the name it is stored under.
    pub fn read_embedding(&self, hash: &str) -> Result<EmbeddingObject> {
        if let Some(bound) = &self.bound {
            let (parent, leaf) = bound.content_fanout(ContentObjectKind::Embedding, hash, false)?;
            return bound_read_embedding(&self.reads, &parent, &leaf, hash);
        }
        if !self.validate_content_parent(ContentObjectKind::Embedding, hash)? {
            return Err(KioError::not_found(hash));
        }
        read_embedding_path(&self.reads, &self.embedding_path(hash)?, hash)
    }

    /// Remove one embedding object after identity verification.
    ///
    /// The embeddings namespace is keyed by [`EmbeddingObject::identity_hash`]
    /// — what the vector is OF — and NOT by the hash of the stored bytes, which
    /// also carry the vector body. [`Self::remove_content`] re-hashes the file
    /// and compares that against the key, so it can never verify an embedding
    /// (every call reports `KIO-E-STORE-CORRUPT-001` on a perfectly healthy
    /// object); this is the correct primitive for the `Embedding` kind, exactly
    /// as [`Self::remove_chunk`] is for the identity-keyed chunk namespace.
    /// Missing is an idempotent `false`.
    pub fn remove_embedding(&self, hash: &str) -> Result<bool> {
        if let Some(bound) = &self.bound {
            return bound.remove_embedding(hash, &self.control);
        }
        if !is_hash(hash) {
            return Err(KioError::invalid_usage("invalid embedding hash"));
        }
        if !self.validate_content_parent(ContentObjectKind::Embedding, hash)? {
            return Ok(false);
        }
        remove_verified_cas_path(
            &self.control,
            self.ambient_kio_dir()?,
            &self.embedding_path(hash)?,
            CasRemovalKind::Embedding,
            hash,
        )
    }

    /// Visit every canonical chunk through retained directory handles. Unknown
    /// entries fail closed. The complete traversal permits at most 100,000
    /// directory entries and 512 MiB of chunk bodies, including irrelevant rows.
    /// This preparation inventory does not grant deletion authority.
    pub fn visit_chunks_bounded(
        &self,
        mut visit: impl FnMut(&str, &ChunkObject) -> Result<()>,
    ) -> Result<()> {
        self.reads.charge(0)?;
        if let Some(bound) = &self.bound {
            let mut retained = bound
                .chunks
                .lock()
                .map_err(|_| KioError::io("bound chunks directory lock poisoned", "chunks"))?;
            if retained.is_none() {
                match bound_open_dir(&bound.objects, "chunks") {
                    Ok(directory) => *retained = Some(Arc::new(directory)),
                    Err(error) if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => {
                        return Ok(());
                    }
                    Err(error) => return Err(error),
                }
            }
            let base = retained
                .as_ref()
                .expect("retained chunks namespace")
                .clone();
            drop(retained);
            return bound_visit_chunks(&self.reads, &base, &mut visit);
        }
        Err(KioError::invalid_usage(
            "chunk completeness inventory requires a retained object store",
        ))
    }

    /// Every embedding object this scope holds, by storage key.
    ///
    /// The rebuild source: `kio repair rebuild-db` replays these into
    /// `embeddings` and derives `chunk_vec` from that (04 §4.3). Returns an
    /// empty list when the namespace does not exist, which is the ordinary
    /// state of a scope nothing has embedded yet.
    pub fn embedding_hashes(&self) -> Result<Vec<String>> {
        self.reads.charge(0)?;
        if let Some(bound) = &self.bound {
            return bound.embedding_hashes(&self.reads);
        }
        ensure_real_directory(self.ambient_kio_dir()?, false)?;
        let kio = crate::store_dir::StoreDirectory::open(self.ambient_kio_dir()?)?;
        let objects = match bound_open_dir(&kio.root_handle(), "objects") {
            Ok(directory) => directory,
            Err(error) if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => {
                return Ok(Vec::new());
            }
            Err(error) => return Err(error),
        };
        let embeddings = match bound_open_dir(&objects, "embeddings") {
            Ok(directory) => directory,
            Err(error) if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => {
                return Ok(Vec::new());
            }
            Err(error) => return Err(error),
        };
        bound_embedding_hashes(&self.reads, &embeddings)
    }

    fn ensure_embedding_parent(&self, hash: &str) -> Result<()> {
        ensure_real_directory(self.ambient_kio_dir()?, false)?;
        ensure_real_directory(&self.ambient_kio_dir()?.join("objects"), true)?;
        let base = self.ambient_kio_dir()?.join("objects/embeddings");
        ensure_real_directory(&base, true)?;
        let digest = hash_path_component(hash)?;
        let first = base.join(&digest[0..2]);
        let second = first.join(&digest[2..4]);
        ensure_real_directory(&first, true)?;
        ensure_real_directory(&second, true)
    }

    pub fn chunk_path(&self, hash: &str) -> Result<PathBuf> {
        fanout_path(self.ambient_kio_dir()?.join("objects/chunks"), hash)
    }

    fn existing_chunk_path(&self, hash: &str) -> Result<Option<PathBuf>> {
        occupied_slot(self.chunk_path(hash)?)
    }

    /// Canonical fan-out path for a content object. `pub` (widened from the
    /// original prepared/image-only usage) so fsck-side verification (PB01/
    /// PB02) can locate `objects/embeddings|manifests|toollocks/<hash>` for a
    /// bounded semantic read, not just the byte-hash check
    /// [`Self::inspect_content_accounted`] already provides generically.
    pub fn content_path(&self, kind: ContentObjectKind, hash: &str) -> Result<PathBuf> {
        if self.bound.is_some() {
            let _ = (kind, hash);
            return Err(KioError::invalid_usage(
                "a bound ObjectStore has no ambient content pathname",
            ));
        }
        fanout_path(
            self.ambient_kio_dir()?
                .join("objects")
                .join(kind.directory()),
            hash,
        )
    }

    fn existing_content_path(
        &self,
        kind: ContentObjectKind,
        hash: &str,
    ) -> Result<Option<PathBuf>> {
        occupied_slot(self.content_path(kind, hash)?)
    }

    fn validate_content_parent(&self, kind: ContentObjectKind, hash: &str) -> Result<bool> {
        let digest = hash_path_component(hash)?;
        let objects = self.ambient_kio_dir()?.join("objects");
        let kind_base = objects.join(kind.directory());
        let first = kind_base.join(&digest[0..2]);
        let second = first.join(&digest[2..4]);
        for directory in [
            self.ambient_kio_dir()?,
            &objects,
            &kind_base,
            &first,
            &second,
        ] {
            match fs::symlink_metadata(directory) {
                Ok(metadata)
                    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() =>
                {
                    ensure_real_directory(directory, false)?;
                }
                Ok(_) => return Err(non_regular_object_error(directory)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(error) => {
                    return Err(KioError::io(
                        error.to_string(),
                        directory.display().to_string(),
                    ));
                }
            }
        }
        Ok(true)
    }

    fn ensure_chunk_parent(&self, hash: &str) -> Result<()> {
        ensure_real_directory(self.ambient_kio_dir()?, false)?;
        ensure_real_directory(&self.ambient_kio_dir()?.join("objects"), true)?;
        let base = self.ambient_kio_dir()?.join("objects/chunks");
        ensure_real_directory(&base, true)?;
        let digest = hash_path_component(hash)?;
        let first = base.join(&digest[0..2]);
        let second = first.join(&digest[2..4]);
        ensure_real_directory(&first, true)?;
        ensure_real_directory(&second, true)
    }

    fn validate_chunk_parent(&self, hash: &str) -> Result<bool> {
        let digest = hash_path_component(hash)?;
        let objects = self.ambient_kio_dir()?.join("objects");
        let base = objects.join("chunks");
        let first = base.join(&digest[0..2]);
        let second = first.join(&digest[2..4]);
        for directory in [self.ambient_kio_dir()?, &objects, &base, &first, &second] {
            match fs::symlink_metadata(directory) {
                Ok(metadata)
                    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() =>
                {
                    ensure_real_directory(directory, false)?;
                }
                Ok(_) => return Err(non_regular_object_error(directory)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(error) => {
                    return Err(KioError::io(
                        error.to_string(),
                        directory.display().to_string(),
                    ));
                }
            }
        }
        Ok(true)
    }

    pub fn write_object_bytes(&self, kind: ObjectKind, hash: &str, bytes: &[u8]) -> Result<()> {
        if let Some(bound) = &self.bound {
            return bound.write_object_bytes(kind, hash, bytes);
        }
        if bytes.len() as u64 > kind.max_bytes() {
            return Err(object_size_error(
                kind,
                kind.max_bytes(),
                bytes.len() as u64,
            ));
        }
        if hash_bytes(bytes) != hash {
            return Err(KioError::invalid_usage(
                "CAS object hash does not match the supplied bytes",
            ));
        }
        self.ensure_object_parent(kind, hash)?;
        if let Some(existing) = self.existing_object_path(kind, hash)? {
            return verify_existing_bytes(&existing, hash, bytes);
        }

        let path = self.object_path(kind, hash)?;
        let (temp_path, mut temp, temp_parent) = create_private_temp(
            path.parent()
                .ok_or_else(|| KioError::io("path has no parent", path.display().to_string()))?,
        )?;
        let result = (|| -> Result<()> {
            temp.write_all(bytes).kio_io(&temp_path)?;
            temp.sync_all().kio_io(&temp_path)?;
            drop(temp);
            publish_temp_object(
                &temp_parent,
                &temp_path,
                &path,
                hash,
                bytes.len() as u64,
                Some(bytes),
            )?;

            let Some(published) = self.existing_object_path(kind, hash)? else {
                return Err(KioError::not_found(hash));
            };
            verify_existing_bytes(&published, hash, bytes)
        })();
        if result.is_err() {
            let _ = remove_private_temp(&temp_parent, &temp_path);
        }
        result
    }

    /// Stream a raw object from an already-authorized handle. At most
    /// `max_bytes + 1` bytes are consumed, and the payload is never materialized
    /// as one attacker-sized allocation.
    pub fn write_raw_reader<R: Read>(
        &self,
        reader: &mut R,
        max_bytes: u64,
    ) -> Result<(String, u64)> {
        if let Some(bound) = &self.bound {
            return bound.write_raw_reader(reader, max_bytes);
        }
        let max_bytes = max_bytes.min(MAX_RAW_OBJECT_BYTES);
        let raw_base = self
            .ambient_kio_dir()?
            .join("objects")
            .join(ObjectKind::Raw.directory());
        self.ensure_kind_base(ObjectKind::Raw)?;
        let (temp_path, mut temp, temp_parent) = create_private_temp(&raw_base)?;
        let result = (|| -> Result<(String, u64)> {
            let mut hasher = Sha256::new();
            let mut buffer = [0_u8; CAS_STREAM_BUFFER_BYTES];
            let mut total = 0_u64;
            loop {
                let read_cap = max_bytes
                    .saturating_sub(total)
                    .saturating_add(1)
                    .min(buffer.len() as u64) as usize;
                let count = reader.read(&mut buffer[..read_cap]).kio_io(&temp_path)?;
                if count == 0 {
                    break;
                }
                total = total
                    .checked_add(count as u64)
                    .ok_or_else(|| object_size_error(ObjectKind::Raw, max_bytes, u64::MAX))?;
                if total > max_bytes {
                    return Err(object_size_error(ObjectKind::Raw, max_bytes, total));
                }
                hasher.update(&buffer[..count]);
                temp.write_all(&buffer[..count]).kio_io(&temp_path)?;
            }
            temp.sync_all().kio_io(&temp_path)?;
            drop(temp);

            let hash = format!("sha256:{}", lower_hex(&hasher.finalize()));
            let path = self.object_path(ObjectKind::Raw, &hash)?;
            self.ensure_object_parent(ObjectKind::Raw, &hash)?;
            if let Some(existing) = self.existing_object_path(ObjectKind::Raw, &hash)? {
                verify_existing_matches_file(&existing, &temp_path, &hash, total)?;
                remove_private_temp(&temp_parent, &temp_path)?;
                return Ok((hash, total));
            }

            publish_temp_object(&temp_parent, &temp_path, &path, &hash, total, None)?;
            let Some(published) = self.existing_object_path(ObjectKind::Raw, &hash)? else {
                return Err(KioError::not_found(&hash));
            };
            read_verified_object(
                &CasReadAccess::Unmetered,
                &published,
                ObjectKind::Raw,
                &hash,
                false,
            )?;
            Ok((hash, total))
        })();
        if result.is_err() {
            let _ = remove_private_temp(&temp_parent, &temp_path);
        }
        result
    }

    pub fn read_by_hash(&self, hash: &str) -> Result<StoredObject> {
        if let Some(bound) = &self.bound {
            return bound.read_by_hash(&self.reads, hash);
        }
        let (kind, path) = self.locate_object(hash)?;
        let (_, bytes) = read_verified_object(&self.reads, &path, kind, hash, true)?;
        Ok(StoredObject {
            kind,
            hash: hash.to_owned(),
            bytes,
        })
    }

    /// Verify the single canonical raw slot, then physically remove it. This
    /// destructive primitive is intentionally raw-only and is consumed
    /// by the purge transaction while its visibility barrier and store lock are
    /// held. Missing is an idempotent `false`; malformed links or bytes fail
    /// closed before unlink.
    pub fn remove_raw(&self, hash: &str) -> Result<bool> {
        if let Some(bound) = &self.bound {
            return bound.remove_raw(hash, &self.control);
        }
        if !is_hash(hash) {
            return Err(KioError::invalid_usage("invalid raw hash"));
        }
        if !self.validate_object_parent(ObjectKind::Raw, hash)? {
            return Ok(false);
        }
        remove_verified_cas_path(
            &self.control,
            self.ambient_kio_dir()?,
            &self.object_path(ObjectKind::Raw, hash)?,
            CasRemovalKind::Raw,
            hash,
        )
    }

    /// Remove one semantic chunk object after identity/text verification.
    pub fn remove_chunk(&self, hash: &str) -> Result<bool> {
        if let Some(bound) = &self.bound {
            return bound.remove_chunk(hash, &self.control);
        }
        if !is_hash(hash) {
            return Err(KioError::invalid_usage("invalid chunk hash"));
        }
        if !self.validate_chunk_parent(hash)? {
            return Ok(false);
        }
        remove_verified_cas_path(
            &self.control,
            self.ambient_kio_dir()?,
            &self.chunk_path(hash)?,
            CasRemovalKind::Chunk,
            hash,
        )
    }

    /// Verify the single canonical prepared or image slot, then physically
    /// remove it. Purge callers must first prove that no surviving
    /// normalized instance references the content hash. Missing is an idempotent
    /// `false`; malformed links, bytes, or duplicate representations fail closed
    /// before the first unlink.
    pub fn remove_content(&self, kind: ContentObjectKind, hash: &str) -> Result<bool> {
        if let Some(bound) = &self.bound {
            return bound.remove_content(kind, hash, &self.control);
        }
        if !is_hash(hash) {
            return Err(KioError::invalid_usage("invalid content object hash"));
        }
        if !self.validate_content_parent(kind, hash)? {
            return Ok(false);
        }
        remove_verified_cas_path(
            &self.control,
            self.ambient_kio_dir()?,
            &self.content_path(kind, hash)?,
            CasRemovalKind::Content(kind),
            hash,
        )
    }

    fn ensure_content_kind_base(&self, kind: ContentObjectKind) -> Result<()> {
        ensure_real_directory(self.ambient_kio_dir()?, false)?;
        ensure_real_directory(&self.ambient_kio_dir()?.join("objects"), true)?;
        ensure_real_directory(
            &self
                .ambient_kio_dir()?
                .join("objects")
                .join(kind.directory()),
            true,
        )
    }

    fn ensure_content_parent(&self, kind: ContentObjectKind, hash: &str) -> Result<()> {
        self.ensure_content_kind_base(kind)?;
        let digest = hash_path_component(hash)?;
        let kind_base = self
            .ambient_kio_dir()?
            .join("objects")
            .join(kind.directory());
        let first = kind_base.join(&digest[0..2]);
        let second = first.join(&digest[2..4]);
        ensure_real_directory(&first, true)?;
        ensure_real_directory(&second, true)
    }

    /// PB01/PB02: write a content-addressed embedding/manifest/toollock/
    /// normalized-unit
    /// object, keyed by `hash_bytes(bytes)`. Idempotent (matching
    /// [`Self::write_object_bytes`]'s "verify existing, don't overwrite"
    /// contract) — used by tests to construct fsck fixtures directly, and is
    /// the storage primitive a future write-path integration would call.
    pub fn write_content_object(&self, kind: ContentObjectKind, bytes: &[u8]) -> Result<String> {
        if let Some(bound) = &self.bound {
            return bound.write_content_object(kind, bytes);
        }
        if bytes.len() as u64 > kind.max_bytes() {
            return Err(content_object_size_error(
                kind,
                kind.max_bytes(),
                bytes.len() as u64,
            ));
        }
        let hash = hash_bytes(bytes);
        self.ensure_content_parent(kind, &hash)?;
        if let Some(existing) = self.existing_content_path(kind, &hash)? {
            verify_existing_bytes(&existing, &hash, bytes)?;
            return Ok(hash);
        }
        let path = self.content_path(kind, &hash)?;
        let (temp_path, mut temp, temp_parent) = create_private_temp(
            path.parent()
                .ok_or_else(|| KioError::io("path has no parent", path.display().to_string()))?,
        )?;
        let result = (|| -> Result<()> {
            temp.write_all(bytes).kio_io(&temp_path)?;
            temp.sync_all().kio_io(&temp_path)?;
            drop(temp);
            publish_temp_object(
                &temp_parent,
                &temp_path,
                &path,
                &hash,
                bytes.len() as u64,
                Some(bytes),
            )?;
            let Some(published) = self.existing_content_path(kind, &hash)? else {
                return Err(KioError::not_found(&hash));
            };
            verify_existing_bytes(&published, &hash, bytes)
        })();
        if result.is_err() {
            let _ = remove_private_temp(&temp_parent, &temp_path);
        }
        result.map(|()| hash)
    }

    /// PB01/PB02: bounded read of a content object's raw bytes for semantic
    /// (JSON) parsing — [`Self::inspect_content_accounted`] verifies the
    /// byte-hash but deliberately never retains the body (it streams
    /// arbitrarily large prepared/image blobs). Embedding/manifest/toollock
    /// objects are small structured JSON, so a full bounded read is
    /// appropriate here. Does not itself verify the digest; callers that need
    /// the digest check should also call `inspect_content_accounted` (PB01
    /// (f)/(g)) or compare the returned bytes' hash directly.
    pub fn read_content_object_bytes(
        &self,
        kind: ContentObjectKind,
        hash: &str,
        max_bytes: u64,
    ) -> Result<Vec<u8>> {
        if let Some(bound) = &self.bound {
            return bound.read_content_object_bytes(&self.reads, kind, hash, max_bytes);
        }
        if !is_hash(hash) {
            return Err(KioError::invalid_usage("invalid content object hash"));
        }
        if !self.validate_content_parent(kind, hash)? {
            return Err(KioError::not_found(hash));
        }
        let Some(path) = self.existing_content_path(kind, hash)? else {
            return Err(KioError::not_found(hash));
        };
        read_bounded_regular_file_inner(&path, max_bytes, &self.reads, true)
    }

    /// Read and verify a hash from one required CAS namespace. Unlike
    /// [`Self::read_by_hash`], this never accepts an object with the same digest
    /// from a different kind as a substitute for the requested object.
    pub fn read_object(&self, kind: ObjectKind, hash: &str) -> Result<StoredObject> {
        self.read_object_with_size(kind, hash)
            .map(|(object, _)| object)
    }

    /// Exact-kind read plus the total bytes verified for the canonical slot.
    pub fn read_object_with_size(
        &self,
        kind: ObjectKind,
        hash: &str,
    ) -> Result<(StoredObject, u64)> {
        if let Some(bound) = &self.bound {
            return bound.read_object_with_size(&self.reads, kind, hash);
        }
        if !is_hash(hash) {
            return Err(KioError::invalid_usage("invalid hash"));
        }
        if !self.validate_object_parent(kind, hash)? {
            return Err(KioError::not_found(hash));
        }
        let Some(path) = self.existing_object_path(kind, hash)? else {
            return Err(KioError::not_found(hash));
        };
        let (size_bytes, bytes) = read_verified_object(&self.reads, &path, kind, hash, true)?;
        Ok((
            StoredObject {
                kind,
                hash: hash.to_owned(),
                bytes,
            },
            size_bytes,
        ))
    }

    /// Verify and count an object through a fixed-size buffer. This is the
    /// metadata-only path used by raw `inspect`; it does not retain the body.
    pub fn inspect_by_hash(&self, hash: &str) -> Result<StoredObjectMetadata> {
        if self.bound.is_some() {
            for kind in [ObjectKind::Tree, ObjectKind::Commit, ObjectKind::Raw] {
                match self.inspect_object(kind, hash) {
                    Ok(metadata) => return Ok(metadata),
                    Err(error) if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => {}
                    Err(error) => return Err(error),
                }
            }
            return Err(KioError::not_found(hash));
        }
        let (kind, path) = self.locate_object(hash)?;
        let (size_bytes, _) = read_verified_object(&self.reads, &path, kind, hash, false)?;
        Ok(StoredObjectMetadata {
            kind,
            hash: hash.to_owned(),
            size_bytes,
        })
    }

    /// Stream-verify metadata from one required CAS namespace without accepting
    /// a same-digest object from another namespace as a substitute.
    pub fn inspect_object(&self, kind: ObjectKind, hash: &str) -> Result<StoredObjectMetadata> {
        if let Some(bound) = &self.bound {
            let (parent, leaf) = bound.fanout(kind, hash, false)?;
            let (size_bytes, _) =
                bound_read_verified(&self.reads, &parent, &leaf, kind, hash, false)?;
            return Ok(StoredObjectMetadata {
                kind,
                hash: hash.to_owned(),
                size_bytes,
            });
        }
        if !is_hash(hash) {
            return Err(KioError::invalid_usage("invalid hash"));
        }
        if !self.validate_object_parent(kind, hash)? {
            return Err(KioError::not_found(hash));
        }
        let Some(path) = self.existing_object_path(kind, hash)? else {
            return Err(KioError::not_found(hash));
        };
        let (size_bytes, _) = read_verified_object(&self.reads, &path, kind, hash, false)?;
        Ok(StoredObjectMetadata {
            kind,
            hash: hash.to_owned(),
            size_bytes,
        })
    }

    /// Fsck-only exact-kind verification that reports bytes consumed even when
    /// verification fails. This lets a single aggregate budget bound adversarial
    /// corrupt objects rather than charging successful reads only.
    pub fn inspect_object_accounted(
        &self,
        kind: ObjectKind,
        hash: &str,
    ) -> std::result::Result<StoredObjectMetadata, AccountedReadError> {
        if let Some(bound) = &self.bound {
            let (size_bytes, _) = bound.read_object_accounted(&self.reads, kind, hash, false)?;
            return Ok(StoredObjectMetadata {
                kind,
                hash: hash.to_owned(),
                size_bytes,
            });
        }
        let path = (|| -> Result<PathBuf> {
            if !is_hash(hash) {
                return Err(KioError::invalid_usage("invalid hash"));
            }
            if !self.validate_object_parent(kind, hash)? {
                return Err(KioError::not_found(hash));
            }
            self.existing_object_path(kind, hash)?
                .ok_or_else(|| KioError::not_found(hash))
        })()
        .map_err(|error| AccountedReadError {
            error,
            consumed_bytes: 0,
        })?;
        let (result, consumed_bytes) =
            read_verified_object_accounted(&self.reads, &path, kind, hash, false);
        result.map_err(|error| AccountedReadError {
            error,
            consumed_bytes,
        })?;
        Ok(StoredObjectMetadata {
            kind,
            hash: hash.to_owned(),
            size_bytes: consumed_bytes,
        })
    }

    /// Accounted materializing read used by fsck for tree/commit schema checks.
    pub fn read_object_accounted(
        &self,
        kind: ObjectKind,
        hash: &str,
    ) -> std::result::Result<(StoredObject, u64), AccountedReadError> {
        if let Some(bound) = &self.bound {
            let (size_bytes, bytes) = bound.read_object_accounted(&self.reads, kind, hash, true)?;
            return Ok((
                StoredObject {
                    kind,
                    hash: hash.to_owned(),
                    bytes,
                },
                size_bytes,
            ));
        }
        let path = (|| -> Result<PathBuf> {
            if !is_hash(hash) {
                return Err(KioError::invalid_usage("invalid hash"));
            }
            if !self.validate_object_parent(kind, hash)? {
                return Err(KioError::not_found(hash));
            }
            self.existing_object_path(kind, hash)?
                .ok_or_else(|| KioError::not_found(hash))
        })()
        .map_err(|error| AccountedReadError {
            error,
            consumed_bytes: 0,
        })?;
        let (result, consumed_bytes) =
            read_verified_object_accounted(&self.reads, &path, kind, hash, true);
        let (_, bytes) = result.map_err(|error| AccountedReadError {
            error,
            consumed_bytes,
        })?;
        Ok((
            StoredObject {
                kind,
                hash: hash.to_owned(),
                bytes,
            },
            consumed_bytes,
        ))
    }

    /// Stream one exact-kind CAS object into `writer` while verifying its
    /// bounded size and content identity. Bytes are never collected into an
    /// object-sized allocation. A caller must discard its private output if
    /// this method reports an error because hash verification completes only
    /// after the last byte has been written.
    pub fn copy_object_to<W: Write>(
        &self,
        kind: ObjectKind,
        hash: &str,
        writer: &mut W,
    ) -> Result<StoredObjectMetadata> {
        if let Some(bound) = &self.bound {
            let (parent, leaf) = bound.fanout(kind, hash, false)?;
            let mut hasher = Sha256::new();
            let (result, _) =
                bound_stream_regular(&self.reads, &parent, &leaf, kind.max_bytes(), |bytes| {
                    hasher.update(bytes);
                    writer.write_all(bytes).kio_io(Path::new(&leaf))
                });
            let size_bytes = result?;
            let actual = format!("sha256:{}", lower_hex(&hasher.finalize()));
            if actual != hash {
                return Err(corrupt_object_error(
                    Path::new(&leaf),
                    "CAS object hash does not match filename",
                    hash,
                    Some(&actual),
                ));
            }
            return Ok(StoredObjectMetadata {
                kind,
                hash: hash.to_owned(),
                size_bytes,
            });
        }
        if !is_hash(hash) {
            return Err(KioError::invalid_usage("invalid hash"));
        }
        if !self.validate_object_parent(kind, hash)? {
            return Err(KioError::not_found(hash));
        }
        let path = self
            .existing_object_path(kind, hash)?
            .ok_or_else(|| KioError::not_found(hash))?;
        let size_bytes = copy_verified_object(&self.reads, &path, kind, hash, writer)?;
        Ok(StoredObjectMetadata {
            kind,
            hash: hash.to_owned(),
            size_bytes,
        })
    }

    pub fn object_path(&self, kind: ObjectKind, hash: &str) -> Result<PathBuf> {
        let base = self
            .ambient_kio_dir()?
            .join("objects")
            .join(kind.directory());
        fanout_path(base, hash)
    }

    fn locate_object(&self, hash: &str) -> Result<(ObjectKind, PathBuf)> {
        if !is_hash(hash) {
            return Err(KioError::invalid_usage("invalid hash"));
        }
        for kind in [ObjectKind::Tree, ObjectKind::Commit, ObjectKind::Raw] {
            if !self.validate_object_parent(kind, hash)? {
                continue;
            }
            if let Some(path) = self.existing_object_path(kind, hash)? {
                return Ok((kind, path));
            }
        }
        Err(KioError::not_found(hash))
    }

    fn existing_object_path(&self, kind: ObjectKind, hash: &str) -> Result<Option<PathBuf>> {
        occupied_slot(self.object_path(kind, hash)?)
    }

    fn ensure_kind_base(&self, kind: ObjectKind) -> Result<()> {
        ensure_real_directory(self.ambient_kio_dir()?, false)?;
        ensure_real_directory(&self.ambient_kio_dir()?.join("objects"), true)?;
        ensure_real_directory(
            &self
                .ambient_kio_dir()?
                .join("objects")
                .join(kind.directory()),
            true,
        )
    }

    pub(super) fn ensure_object_parent(&self, kind: ObjectKind, hash: &str) -> Result<()> {
        self.ensure_kind_base(kind)?;
        let digest = hash_path_component(hash)?;
        let kind_base = self
            .ambient_kio_dir()?
            .join("objects")
            .join(kind.directory());
        let first = kind_base.join(&digest[0..2]);
        let second = first.join(&digest[2..4]);
        ensure_real_directory(&first, true)?;
        ensure_real_directory(&second, true)
    }

    fn validate_object_parent(&self, kind: ObjectKind, hash: &str) -> Result<bool> {
        let digest = hash_path_component(hash)?;
        let objects = self.ambient_kio_dir()?.join("objects");
        let kind_base = objects.join(kind.directory());
        let first = kind_base.join(&digest[0..2]);
        let second = first.join(&digest[2..4]);
        for directory in [
            self.ambient_kio_dir()?,
            &objects,
            &kind_base,
            &first,
            &second,
        ] {
            match fs::symlink_metadata(directory) {
                Ok(metadata)
                    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() =>
                {
                    ensure_real_directory(directory, false)?;
                }
                Ok(_) => return Err(non_regular_object_error(directory)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(error) => {
                    return Err(KioError::io(
                        error.to_string(),
                        directory.display().to_string(),
                    ));
                }
            }
        }
        Ok(true)
    }
}
