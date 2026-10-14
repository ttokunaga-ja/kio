use super::*;

impl BoundObjectDirs {
    pub(super) fn validate_layout(&self) -> Result<()> {
        let changed = || {
            KioError::new(
                "KIO-E-SNAPSHOT-AUTHORITY-CHANGED-001",
                "public object layout changed after scheduled snapshot binding",
                serde_json::json!({}),
                crate::ExitCode::PartialFailure,
            )
        };
        let objects = bound_open_dir(&self.kio, "objects").map_err(|_| changed())?;
        let raw = bound_open_dir(&objects, ObjectKind::Raw.directory()).map_err(|_| changed())?;
        let trees =
            bound_open_dir(&objects, ObjectKind::Tree.directory()).map_err(|_| changed())?;
        let commits =
            bound_open_dir(&objects, ObjectKind::Commit.directory()).map_err(|_| changed())?;
        for (current, retained) in [
            (&objects, &self.objects),
            (&raw, &self.raw),
            (&trees, &self.trees),
            (&commits, &self.commits),
        ] {
            if !same_bound_directory(current, retained)? {
                return Err(changed());
            }
        }
        for kind in [
            ContentObjectKind::Prepared,
            ContentObjectKind::Image,
            ContentObjectKind::Embedding,
            ContentObjectKind::Manifest,
            ContentObjectKind::Toollock,
            ContentObjectKind::NormalizedUnit,
        ] {
            let retained = self.content_slot(kind)?;
            let Some(retained) = retained.as_ref() else {
                continue;
            };
            let current = bound_open_dir(&objects, kind.directory()).map_err(|_| changed())?;
            if !same_bound_directory(&current, retained)? {
                return Err(changed());
            }
        }
        let chunks = self.chunks.lock().map_err(|_| changed())?;
        if let Some(retained) = chunks.as_ref() {
            let current = bound_open_dir(&objects, "chunks").map_err(|_| changed())?;
            if !same_bound_directory(&current, retained)? {
                return Err(changed());
            }
        }
        Ok(())
    }

    pub(super) fn stage_raw_from_reader<R: Read>(
        &self,
        reader: &mut R,
        max_bytes: u64,
    ) -> Result<BoundRawStage> {
        let max_bytes = max_bytes.min(MAX_RAW_OBJECT_BYTES);
        let (name, mut file) = bound_create_ingest_temp(&self.raw)?;
        let result = (|| {
            let mut hasher = Sha256::new();
            let mut total = 0_u64;
            let mut buffer = [0_u8; CAS_STREAM_BUFFER_BYTES];
            loop {
                let cap = max_bytes
                    .saturating_sub(total)
                    .saturating_add(1)
                    .min(buffer.len() as u64) as usize;
                let count = reader.read(&mut buffer[..cap]).kio_io(Path::new(&name))?;
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
                file.write_all(&buffer[..count]).kio_io(Path::new(&name))?;
            }
            file.sync_all().kio_io(Path::new(&name))?;
            file.seek(std::io::SeekFrom::Start(0))
                .kio_io(Path::new(&name))?;
            Ok((format!("sha256:{}", lower_hex(&hasher.finalize())), total))
        })();
        match result {
            Ok((raw_hash, size_bytes)) => Ok(BoundRawStage {
                parent: Arc::clone(&self.raw),
                name,
                file: Some(file),
                raw_hash,
                size_bytes,
            }),
            Err(error) => {
                drop(file);
                let _ = bound_remove(&self.raw, &name);
                Err(error)
            }
        }
    }

    pub(super) fn publish_raw_stage(&self, mut stage: BoundRawStage) -> Result<(String, u64)> {
        if !Arc::ptr_eq(&stage.parent, &self.raw) {
            return Err(KioError::invalid_usage(
                "bound raw stage belongs to another ObjectStore",
            ));
        }
        let opened = stage
            .file
            .as_ref()
            .ok_or_else(|| bound_stage_corrupt("bound raw stage handle is unavailable"))?
            .metadata()
            .kio_io(Path::new(&stage.name))?;
        if !opened.is_file() || opened.len() != stage.size_bytes {
            return Err(corrupt_object_error(
                Path::new(&stage.name),
                "bound raw stage changed before publication",
                &stage.raw_hash,
                None,
            ));
        }
        let hash = stage.raw_hash.clone();
        let size = stage.size_bytes;
        // Windows may reject linking or unlinking a name while its writer
        // handle remains open. The exact file was checked above; publication
        // re-opens it through the retained raw directory and verifies hash,
        // type and single-link identity before accepting it.
        cas_semantics::prepare_bound_raw_stage(&mut stage.file);
        let (parent, leaf) = self.fanout(ObjectKind::Raw, &hash, true)?;
        match bound_read_verified(
            &CasReadAccess::Unmetered,
            &parent,
            &leaf,
            ObjectKind::Raw,
            &hash,
            true,
        ) {
            Ok((existing_size, existing)) => {
                let (_, staged) = bound_read_regular(
                    &CasReadAccess::Unmetered,
                    &self.raw,
                    &stage.name,
                    MAX_RAW_OBJECT_BYTES,
                )?;
                if existing_size != size || existing != staged {
                    return Err(corrupt_object_error(
                        Path::new(&leaf),
                        "CAS object bytes do not match existing object",
                        &hash,
                        Some(&hash_bytes(&existing)),
                    ));
                }
                bound_remove(&self.raw, &stage.name)?;
            }
            Err(error) if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => {
                bound_publish_between(
                    &self.raw,
                    &stage.name,
                    &parent,
                    &leaf,
                    ObjectKind::Raw,
                    &hash,
                    size,
                    None,
                )?
            }
            Err(error) => return Err(error),
        }
        stage.name.clear();
        Ok((hash, size))
    }

    pub(super) fn cleanup_raw_ingest_orphans(&self) -> Result<()> {
        const MAX_ENTRIES: usize = 100_000;
        let mut total = 0_u64;
        let entries = cap_primitives::fs::read_base_dir(&self.raw)
            .map_err(|e| KioError::io(e.to_string(), "bound raw stage directory"))?;
        for (n, entry) in entries.enumerate() {
            if n >= MAX_ENTRIES {
                return Err(bound_stage_corrupt(
                    "bound raw stage directory exceeds entry limit",
                ));
            }
            let entry =
                entry.map_err(|e| KioError::io(e.to_string(), "bound raw stage directory"))?;
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                return Err(bound_stage_corrupt("bound raw stage name is not UTF-8"));
            };
            if !name.starts_with(".ingest-") {
                continue;
            }
            if !bound_ingest_name(name) {
                return Err(bound_stage_corrupt("bound raw stage name is malformed"));
            }
            let (size, _) = bound_read_regular(
                &CasReadAccess::Unmetered,
                &self.raw,
                name,
                MAX_RAW_OBJECT_BYTES,
            )?;
            total = total
                .checked_add(size)
                .ok_or_else(|| bound_stage_corrupt("bound raw stage byte count overflow"))?;
            if total > MAX_RAW_OBJECT_BYTES {
                return Err(bound_stage_corrupt(
                    "bound raw stage cleanup exceeds byte limit",
                ));
            }
            bound_remove(&self.raw, name)?;
        }
        sync_bound_directory(&self.raw, Path::new("bound raw stage directory"))
    }

    fn dir(&self, kind: ObjectKind) -> &File {
        match kind {
            ObjectKind::Raw => &self.raw,
            ObjectKind::Tree => &self.trees,
            ObjectKind::Commit => &self.commits,
        }
    }

    pub(super) fn read_object_accounted(
        &self,
        reads: &CasReadAccess,
        kind: ObjectKind,
        hash: &str,
        materialize: bool,
    ) -> std::result::Result<(u64, Vec<u8>), AccountedReadError> {
        let (parent, leaf) =
            self.fanout(kind, hash, false)
                .map_err(|error| AccountedReadError {
                    error,
                    consumed_bytes: 0,
                })?;
        let (result, consumed_bytes) =
            bound_read_verified_accounted(reads, &parent, &leaf, kind, hash, materialize);
        result.map_err(|error| AccountedReadError {
            error,
            consumed_bytes,
        })
    }

    fn content_slot(
        &self,
        kind: ContentObjectKind,
    ) -> Result<std::sync::MutexGuard<'_, Option<Arc<File>>>> {
        match kind {
            ContentObjectKind::Prepared => &self.prepared,
            ContentObjectKind::Image => &self.image,
            ContentObjectKind::Embedding => &self.embeddings,
            ContentObjectKind::Manifest => &self.manifests,
            ContentObjectKind::Toollock => &self.toollocks,
            ContentObjectKind::NormalizedUnit => &self.normalized_units,
        }
        .lock()
        .map_err(|_| KioError::io("bound content directory lock poisoned", kind.directory()))
    }

    pub(super) fn fanout(
        &self,
        kind: ObjectKind,
        hash: &str,
        create: bool,
    ) -> Result<(File, String)> {
        let digest = hash_path_component(hash)?;
        let first = &digest[..2];
        let second = &digest[2..4];
        let base = self.dir(kind);
        let first_dir = bound_open_or_create_dir(base, first, create)?;
        let second_dir = bound_open_or_create_dir(&first_dir, second, create)?;
        Ok((second_dir, digest.to_owned()))
    }

    pub(super) fn chunk_fanout(&self, hash: &str, create: bool) -> Result<(File, String)> {
        let digest = hash_path_component(hash)?;
        let mut retained = self
            .chunks
            .lock()
            .map_err(|_| KioError::io("bound chunks directory lock poisoned", "chunks"))?;
        if retained.is_none() {
            if !create {
                *retained = Some(Arc::new(bound_open_dir(&self.objects, "chunks")?));
            } else {
                *retained = Some(Arc::new(bound_open_or_create_dir(
                    &self.objects,
                    "chunks",
                    true,
                )?));
            }
        }
        let base = retained
            .as_ref()
            .expect("created or retained chunks namespace");
        let first = bound_open_or_create_dir(base, &digest[..2], create)?;
        let second = bound_open_or_create_dir(&first, &digest[2..4], create)?;
        Ok((second, digest.to_owned()))
    }

    pub(super) fn content_fanout(
        &self,
        kind: ContentObjectKind,
        hash: &str,
        create: bool,
    ) -> Result<(File, String)> {
        let digest = hash_path_component(hash)?;
        let first = &digest[..2];
        let second = &digest[2..4];
        let base = {
            let mut retained = self.content_slot(kind)?;
            if let Some(directory) = retained.as_ref() {
                directory
                    .try_clone()
                    .map_err(|error| KioError::io(error.to_string(), kind.directory()))?
            } else if create {
                let directory = Arc::new(bound_open_or_create_dir(
                    &self.objects,
                    kind.directory(),
                    true,
                )?);
                let clone = directory
                    .try_clone()
                    .map_err(|error| KioError::io(error.to_string(), kind.directory()))?;
                *retained = Some(directory);
                clone
            } else {
                let directory = Arc::new(bound_open_dir(&self.objects, kind.directory())?);
                let clone = directory
                    .try_clone()
                    .map_err(|error| KioError::io(error.to_string(), kind.directory()))?;
                *retained = Some(directory);
                clone
            }
        };
        let first_dir = bound_open_or_create_dir(&base, first, create)?;
        let second_dir = bound_open_or_create_dir(&first_dir, second, create)?;
        Ok((second_dir, digest.to_owned()))
    }

    pub(super) fn repair_raw(&self, expected_hash: &str, bytes: &[u8]) -> Result<bool> {
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
        let (parent, leaf) = self.fanout(ObjectKind::Raw, expected_hash, true)?;
        match bound_read_regular(
            &CasReadAccess::Unmetered,
            &parent,
            &leaf,
            ObjectKind::Raw.max_bytes(),
        ) {
            Ok((_, existing)) if hash_bytes(&existing) == expected_hash => return Ok(false),
            Ok(_) => {}
            Err(error) if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => {
                self.write_object_bytes(ObjectKind::Raw, expected_hash, bytes)?;
                return Ok(true);
            }
            // `bound_read_regular` rejects symlinks, hardlinks, directories,
            // unstable entries and over-limit bodies before exposing bytes.
            // Those unsafe representations are never repairable in place.
            Err(error) => return Err(error),
        }

        // Capture an exact safe source handle before linking. The later
        // quarantine must prove it names this handle, never merely whatever
        // pathname happened to occupy the leaf after the corrupt read.
        let mut source = bound_open_regular_handle(&parent, &leaf)?;
        if bound_hash_open_regular(
            &CasReadAccess::Unmetered,
            &mut source,
            ObjectKind::Raw.max_bytes(),
        )? == expected_hash
        {
            return Err(corrupt_object_error(
                Path::new(&leaf),
                "raw repair observed an unstable CAS slot",
                expected_hash,
                Some(expected_hash),
            ));
        }
        let (quarantine, file) = bound_create_temp(&parent)?;
        drop(file);
        bound_remove(&parent, &quarantine)?;
        cap_primitives::fs::hard_link(&parent, Path::new(&leaf), &parent, Path::new(&quarantine))
            .map_err(|error| KioError::io(error.to_string(), &leaf))?;
        let result = (|| {
            if !bound_quarantine_matches(&parent, &quarantine, &source)? {
                let _ = bound_remove(&parent, &quarantine);
                return Err(corrupt_object_error(
                    Path::new(&leaf),
                    "raw repair leaf changed before quarantine",
                    expected_hash,
                    None,
                ));
            }
            bound_remove(&parent, &leaf)?;
            let (temp, mut replacement) = bound_create_temp(&parent)?;
            let publish = (|| {
                replacement.write_all(bytes).kio_io(Path::new(&temp))?;
                replacement.sync_all().kio_io(Path::new(&temp))?;
                drop(replacement);
                bound_publish(
                    &parent,
                    &temp,
                    &leaf,
                    ObjectKind::Raw,
                    expected_hash,
                    bytes.len() as u64,
                    Some(bytes),
                )
            })();
            if publish.is_err() {
                let _ = bound_remove(&parent, &temp);
                return publish;
            }
            if let Err(error) = bound_read_verified(
                &CasReadAccess::Unmetered,
                &parent,
                &leaf,
                ObjectKind::Raw,
                expected_hash,
                false,
            ) {
                let _ = bound_remove(&parent, &leaf);
                let _ = cap_primitives::fs::hard_link(
                    &parent,
                    Path::new(&quarantine),
                    &parent,
                    Path::new(&leaf),
                );
                return Err(error);
            }
            bound_remove(&parent, &quarantine)?;
            sync_bound_directory(&parent, Path::new(&leaf))?;
            Ok(())
        })();
        if result.is_err() {
            // If removal/republication itself failed, leave the quarantined
            // corrupt evidence intact for a later explicit recovery attempt.
            return result.map(|()| true);
        }
        Ok(true)
    }

    pub(super) fn remove_raw(
        &self,
        hash: &str,
        control: &crate::control::ControlHandle,
    ) -> Result<bool> {
        if !is_hash(hash) {
            return Err(KioError::invalid_usage("invalid raw hash"));
        }
        self.remove_verified(control, ObjectKind::Raw, hash, |parent, leaf| {
            bound_read_verified(
                &CasReadAccess::Unmetered,
                parent,
                leaf,
                ObjectKind::Raw,
                hash,
                false,
            )
            .map(|_| ())
        })
    }

    pub(super) fn remove_chunk(
        &self,
        hash: &str,
        control: &crate::control::ControlHandle,
    ) -> Result<bool> {
        if !is_hash(hash) {
            return Err(KioError::invalid_usage("invalid chunk hash"));
        }
        let (parent, leaf) = match self.chunk_fanout(hash, false) {
            Ok(slot) => slot,
            Err(error) if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => return Ok(false),
            Err(error) => return Err(error),
        };
        bound_remove_verified(control, &parent, &leaf, |parent, leaf| {
            bound_read_chunk_accounted(&CasReadAccess::Unmetered, parent, leaf, hash)
                .map(|_| ())
                .map_err(|error| error.error)
        })
    }

    pub(super) fn remove_embedding(
        &self,
        hash: &str,
        control: &crate::control::ControlHandle,
    ) -> Result<bool> {
        if !is_hash(hash) {
            return Err(KioError::invalid_usage("invalid embedding hash"));
        }
        let (parent, leaf) = match self.content_fanout(ContentObjectKind::Embedding, hash, false) {
            Ok(slot) => slot,
            Err(error) if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => return Ok(false),
            Err(error) => return Err(error),
        };
        bound_remove_verified(control, &parent, &leaf, |parent, leaf| {
            bound_read_embedding(&CasReadAccess::Unmetered, parent, leaf, hash).map(|_| ())
        })
    }

    fn remove_verified<F>(
        &self,
        control: &crate::control::ControlHandle,
        kind: ObjectKind,
        hash: &str,
        verify: F,
    ) -> Result<bool>
    where
        F: Fn(&File, &str) -> Result<()>,
    {
        let (parent, leaf) = match self.fanout(kind, hash, false) {
            Ok(slot) => slot,
            Err(error) if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => return Ok(false),
            Err(error) => return Err(error),
        };
        bound_remove_verified(control, &parent, &leaf, verify)
    }

    pub(super) fn embedding_hashes(&self, reads: &CasReadAccess) -> Result<Vec<String>> {
        let mut retained = self.content_slot(ContentObjectKind::Embedding)?;
        if retained.is_none() {
            match bound_open_dir(&self.objects, ContentObjectKind::Embedding.directory()) {
                Ok(directory) => *retained = Some(Arc::new(directory)),
                Err(error) if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => {
                    return Ok(Vec::new());
                }
                Err(error) => return Err(error),
            }
        }
        let Some(base) = retained.as_ref() else {
            return Ok(Vec::new());
        };
        bound_embedding_hashes(reads, base)
    }

    pub(super) fn inspect_content_object(
        &self,
        reads: &CasReadAccess,
        kind: ContentObjectKind,
        hash: &str,
    ) -> Result<StoredContentObjectMetadata> {
        if !is_hash(hash) {
            return Err(KioError::invalid_usage("invalid content object hash"));
        }
        let (parent, leaf) = self.content_fanout(kind, hash, false)?;
        let size_bytes = bound_verify_content(reads, &parent, &leaf, kind, hash)?;
        Ok(StoredContentObjectMetadata {
            kind,
            hash: hash.to_owned(),
            size_bytes,
        })
    }

    pub(super) fn inspect_content_accounted(
        &self,
        reads: &CasReadAccess,
        kind: ContentObjectKind,
        hash: &str,
    ) -> std::result::Result<StoredContentObjectMetadata, AccountedReadError> {
        let (parent, leaf) =
            self.content_fanout(kind, hash, false)
                .map_err(|error| AccountedReadError {
                    error,
                    consumed_bytes: 0,
                })?;
        let (result, consumed_bytes) =
            bound_verify_content_accounted(reads, &parent, &leaf, kind, hash);
        let size_bytes = result.map_err(|error| AccountedReadError {
            error,
            consumed_bytes,
        })?;
        Ok(StoredContentObjectMetadata {
            kind,
            hash: hash.to_owned(),
            size_bytes,
        })
    }

    pub(super) fn write_content_object(
        &self,
        kind: ContentObjectKind,
        bytes: &[u8],
    ) -> Result<String> {
        if bytes.len() as u64 > kind.max_bytes() {
            return Err(content_object_size_error(
                kind,
                kind.max_bytes(),
                bytes.len() as u64,
            ));
        }
        let hash = hash_bytes(bytes);
        let (parent, leaf) = self.content_fanout(kind, &hash, true)?;
        match bound_read_regular(&CasReadAccess::Unmetered, &parent, &leaf, kind.max_bytes()) {
            Ok((_, existing)) => {
                if hash_bytes(&existing) == hash && existing == bytes {
                    return Ok(hash);
                }
                return Err(corrupt_object_error(
                    Path::new(&leaf),
                    "CAS content bytes do not match existing object",
                    &hash,
                    Some(&hash_bytes(&existing)),
                ));
            }
            Err(error) if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => {}
            Err(error) => return Err(error),
        }
        let (temp_name, mut temp) = bound_create_temp(&parent)?;
        let result = (|| {
            temp.write_all(bytes).kio_io(Path::new(&temp_name))?;
            temp.sync_all().kio_io(Path::new(&temp_name))?;
            drop(temp);
            bound_publish_content(&parent, &temp_name, &leaf, kind, &hash, bytes)
        })();
        if result.is_err() {
            let _ = bound_remove(&parent, &temp_name);
        }
        result.map(|()| hash)
    }

    pub(super) fn read_content_object_bytes(
        &self,
        reads: &CasReadAccess,
        kind: ContentObjectKind,
        hash: &str,
        max_bytes: u64,
    ) -> Result<Vec<u8>> {
        if !is_hash(hash) {
            return Err(KioError::invalid_usage("invalid content object hash"));
        }
        let (parent, leaf) = self.content_fanout(kind, hash, false)?;
        bound_read_regular(reads, &parent, &leaf, max_bytes).map(|(_, bytes)| bytes)
    }

    pub(super) fn remove_content(
        &self,
        kind: ContentObjectKind,
        hash: &str,
        control: &crate::control::ControlHandle,
    ) -> Result<bool> {
        if !is_hash(hash) {
            return Err(KioError::invalid_usage("invalid content object hash"));
        }
        let (parent, leaf) = match self.content_fanout(kind, hash, false) {
            Ok(slot) => slot,
            Err(error) if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => return Ok(false),
            Err(error) => return Err(error),
        };
        bound_remove_verified(control, &parent, &leaf, |parent, leaf| {
            bound_verify_content(&CasReadAccess::Unmetered, parent, leaf, kind, hash).map(|_| ())
        })
    }

    pub(super) fn read_object_with_size(
        &self,
        reads: &CasReadAccess,
        kind: ObjectKind,
        hash: &str,
    ) -> Result<(StoredObject, u64)> {
        if !is_hash(hash) {
            return Err(KioError::invalid_usage("invalid hash"));
        }
        let (parent, leaf) = self.fanout(kind, hash, false)?;
        let (size_bytes, bytes) = bound_read_verified(reads, &parent, &leaf, kind, hash, true)?;
        Ok((
            StoredObject {
                kind,
                hash: hash.to_owned(),
                bytes,
            },
            size_bytes,
        ))
    }

    pub(super) fn read_by_hash(&self, reads: &CasReadAccess, hash: &str) -> Result<StoredObject> {
        if !is_hash(hash) {
            return Err(KioError::invalid_usage("invalid hash"));
        }
        for kind in [ObjectKind::Tree, ObjectKind::Commit, ObjectKind::Raw] {
            match self.read_object_with_size(reads, kind, hash) {
                Ok((object, _)) => return Ok(object),
                Err(error) if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => continue,
                Err(error) => return Err(error),
            }
        }
        Err(KioError::not_found(hash))
    }

    pub(super) fn write_object_bytes(
        &self,
        kind: ObjectKind,
        hash: &str,
        bytes: &[u8],
    ) -> Result<()> {
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
        let (parent, leaf) = self.fanout(kind, hash, true)?;
        match bound_read_verified(&CasReadAccess::Unmetered, &parent, &leaf, kind, hash, true) {
            Ok((_, existing)) => {
                if existing == bytes {
                    return Ok(());
                }
                return Err(corrupt_object_error(
                    Path::new(&leaf),
                    "CAS object bytes do not match existing object",
                    hash,
                    Some(&hash_bytes(&existing)),
                ));
            }
            Err(error) if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => {}
            Err(error) => return Err(error),
        }
        let (temp_name, mut temp) = bound_create_temp(&parent)?;
        let result = (|| {
            temp.write_all(bytes).kio_io(Path::new(&temp_name))?;
            temp.sync_all().kio_io(Path::new(&temp_name))?;
            drop(temp);
            bound_publish(
                &parent,
                &temp_name,
                &leaf,
                kind,
                hash,
                bytes.len() as u64,
                Some(bytes),
            )
        })();
        if result.is_err() {
            let _ = bound_remove(&parent, &temp_name);
        }
        result
    }

    pub(super) fn write_raw_reader<R: Read>(
        &self,
        reader: &mut R,
        max_bytes: u64,
    ) -> Result<(String, u64)> {
        let max_bytes = max_bytes.min(MAX_RAW_OBJECT_BYTES);
        let (temp_name, mut temp) = bound_create_temp(&self.raw)?;
        let result = (|| {
            let mut hasher = Sha256::new();
            let mut total = 0_u64;
            let mut buffer = [0_u8; CAS_STREAM_BUFFER_BYTES];
            loop {
                let cap = max_bytes
                    .saturating_sub(total)
                    .saturating_add(1)
                    .min(buffer.len() as u64) as usize;
                let count = reader
                    .read(&mut buffer[..cap])
                    .kio_io(Path::new(&temp_name))?;
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
                temp.write_all(&buffer[..count])
                    .kio_io(Path::new(&temp_name))?;
            }
            temp.sync_all().kio_io(Path::new(&temp_name))?;
            drop(temp);
            let hash = format!("sha256:{}", lower_hex(&hasher.finalize()));
            let (parent, leaf) = self.fanout(ObjectKind::Raw, &hash, true)?;
            // A temp in raw's base cannot be linked across a fanout directory
            // by a path-based fallback; linkat keeps both endpoints capability-bound.
            match bound_read_verified(
                &CasReadAccess::Unmetered,
                &parent,
                &leaf,
                ObjectKind::Raw,
                &hash,
                true,
            ) {
                Ok((_, existing)) => {
                    let (_, staged) = bound_read_regular(
                        &CasReadAccess::Unmetered,
                        &self.raw,
                        &temp_name,
                        ObjectKind::Raw.max_bytes(),
                    )?;
                    if existing != staged {
                        return Err(corrupt_object_error(
                            Path::new(&leaf),
                            "CAS object bytes do not match existing object",
                            &hash,
                            Some(&hash_bytes(&existing)),
                        ));
                    }
                    bound_remove(&self.raw, &temp_name)?;
                }
                Err(error) if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => {
                    bound_publish_between(
                        &self.raw,
                        &temp_name,
                        &parent,
                        &leaf,
                        ObjectKind::Raw,
                        &hash,
                        total,
                        None,
                    )?;
                }
                Err(error) => return Err(error),
            }
            Ok((hash, total))
        })();
        if result.is_err() {
            let _ = bound_remove(&self.raw, &temp_name);
        }
        result
    }
}
