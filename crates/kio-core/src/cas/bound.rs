#[cfg(test)]
thread_local! { static FAIL_RAW_WRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }

#[cfg(test)]
pub(crate) fn fail_next_raw_write() {
    FAIL_RAW_WRITE.with(|fault| fault.set(true));
}

use super::*;

struct RawStageDirectories {
    stages: crate::store_dir::StoreDirectory,
    journals: Option<crate::store_dir::StoreDirectory>,
}

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

    fn raw_stage_directories(&self, create: bool) -> Result<Option<RawStageDirectories>> {
        let internal = match bound_open_or_create_dir(&self.kio, "internal", create) {
            Err(error) if !create && error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => {
                return Ok(None);
            }
            result => result?,
        };
        let open = |name: &str| -> Result<Option<crate::store_dir::StoreDirectory>> {
            match bound_open_or_create_dir(&internal, name, create) {
                Ok(handle) => {
                    crate::store_dir::StoreDirectory::from_retained(handle, PathBuf::from(name))
                        .map(Some)
                }
                Err(error) if !create && error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => {
                    Ok(None)
                }
                Err(error) => Err(error),
            }
        };
        // Keep journals separate from payload inventory, and create their
        // directory first. Missing journals never authorize residue deletion.
        let journals = open("raw-stage-identities")?;
        let Some(stages) = open("raw-stages")? else {
            return Ok(None);
        };
        Ok(Some(RawStageDirectories { stages, journals }))
    }

    pub(super) fn inspect_raw_stages(
        &self,
        cleanup: bool,
    ) -> Result<Vec<crate::cas::RawStageResidue>> {
        use crate::store_dir::{StageEntryState, StageIdentity};
        let mut residues = Vec::new();
        if let Some(RawStageDirectories { stages, journals }) = self.raw_stage_directories(false)? {
            let mut expected = Vec::new();
            if let Some(journals) = &journals {
                if cleanup {
                    journals.recover_atomic(&[journals])?;
                }
                for entry in journals.inspect_stages(&[], &[])? {
                    if entry.name == crate::store_dir::ATOMIC_WORKSPACE_DIR {
                        continue;
                    }
                    let leaf = PathBuf::from(&entry.name);
                    match journals.read_optional(&leaf, 4096).and_then(|bytes| {
                        serde_json::from_slice::<StageIdentity>(&bytes.unwrap_or_default())
                            .map_err(|error| bound_stage_corrupt(&error.to_string()))
                    }) {
                        Ok(identity) => expected.push((leaf, identity)),
                        Err(error) => residues.push(RawStageResidue {
                            path: format!("internal/raw-stage-identities/{}", leaf.display()),
                            reason: error.to_string(),
                        }),
                    }
                }
            }
            let mut retired = std::collections::BTreeSet::new();
            for entry in stages.inspect_stages(&expected, &[])? {
                let leaf = PathBuf::from(&entry.name);
                let source = match &entry.state {
                    StageEntryState::Captured { source, .. } => source,
                    _ => &leaf,
                };
                if retired.contains(source) {
                    continue;
                }
                let identity = expected
                    .iter()
                    .find(|(name, _)| name == source)
                    .map(|(_, id)| id);
                if cleanup
                    && let Some(identity) = identity
                    && !matches!(entry.state, StageEntryState::Unidentified { .. })
                {
                    stages.discard_stage(source, identity)?;
                    journals
                        .as_ref()
                        .expect("saved stage journal")
                        .remove_file(source)?;
                    retired.insert(source.clone());
                    continue;
                }
                if !matches!(entry.state, StageEntryState::Missing) {
                    residues.push(RawStageResidue {
                        path: format!("internal/raw-stages/{}", leaf.display()),
                        reason: format!("{:?}", entry.state),
                    });
                }
            }
        }
        // A name or nonce alone is never deletion authority, including residue
        // left before a durable identity could be saved.
        for entry in cap_primitives::fs::read_base_dir(&self.raw)
            .map_err(|e| KioError::io(e.to_string(), "raw"))?
        {
            let name = entry
                .map_err(|e| KioError::io(e.to_string(), "raw"))?
                .file_name();
            if name.as_encoded_bytes().starts_with(b".ingest-") {
                residues.push(RawStageResidue {
                    path: format!("objects/raw/{}", name.to_string_lossy()),
                    reason: "unidentified stage: no saved identity".into(),
                });
            }
        }
        Ok(residues)
    }

    pub(super) fn stage_raw_from_reader<R: Read>(
        &self,
        reader: &mut R,
        max_bytes: u64,
    ) -> Result<BoundRawStage> {
        ensure_bound_directory(&self.kio, Path::new(".kio"))?;
        let RawStageDirectories {
            stages: directory,
            journals,
        } = self
            .raw_stage_directories(true)?
            .expect("created stage directories");
        let journals = journals.expect("created stage journals");
        let name = format!("raw-{}-{}", std::process::id(), unix_nanos());
        let writer = crate::store_dir::StageWriter::create(&directory, Path::new(&name))?;
        let identity = writer.identity().clone();
        let mut stage = BoundRawStage {
            parent: Arc::clone(&self.raw),
            directory,
            journals,
            name,
            writer: Some(writer),
            identity,
            raw_hash: String::new(),
            size_bytes: 0,
        };
        stage.journals.write_atomic(
            Path::new(&stage.name),
            &serde_json::to_vec(&stage.identity)
                .map_err(|e| bound_stage_corrupt(&e.to_string()))?,
            crate::store_dir::Publication::CreateOnly,
        )?;
        let mut hasher = Sha256::new();
        let mut buffer = [0; CAS_STREAM_BUFFER_BYTES];
        loop {
            let cap = max_bytes
                .saturating_sub(stage.size_bytes)
                .saturating_add(1)
                .min(buffer.len() as u64) as usize;
            let count = reader
                .read(&mut buffer[..cap])
                .kio_io(Path::new(&stage.name))?;
            if count == 0 {
                break;
            }
            stage.size_bytes = stage
                .size_bytes
                .checked_add(count as u64)
                .ok_or_else(|| object_size_error(ObjectKind::Raw, max_bytes, u64::MAX))?;
            if stage.size_bytes > max_bytes {
                return Err(object_size_error(
                    ObjectKind::Raw,
                    max_bytes,
                    stage.size_bytes,
                ));
            }
            hasher.update(&buffer[..count]);
            #[cfg(test)]
            if FAIL_RAW_WRITE.with(|fault| fault.replace(false)) {
                return Err(crate::store_dir::ioerr(
                    Path::new(&stage.name),
                    std::io::Error::from(std::io::ErrorKind::StorageFull),
                ));
            }
            stage
                .writer
                .as_mut()
                .expect("live stage")
                .write_chunk(&buffer[..count])?;
        }
        stage.writer.as_ref().expect("live stage").sync()?;
        stage.raw_hash = format!("sha256:{}", lower_hex(&hasher.finalize()));
        Ok(stage)
    }

    pub(super) fn publish_raw_stage(&self, mut stage: BoundRawStage) -> Result<(String, u64)> {
        if !same_bound_directory(&stage.parent, &self.raw)? {
            return Err(KioError::invalid_usage(
                "raw stage belongs to another store",
            ));
        }
        let mut file = stage
            .directory
            .open_regular_read(Path::new(&stage.name), u64::MAX)?;
        stage.identity.verify(&file)?;
        if file.metadata().kio_io(Path::new(&stage.name))?.len() != stage.size_bytes
            || bound_hash_open_regular(&CasReadAccess::Unmetered, &mut file, u64::MAX)?
                != stage.raw_hash
        {
            return Err(bound_stage_corrupt("raw stage changed before publication"));
        }
        drop(file);
        let (parent, leaf) = self.fanout(ObjectKind::Raw, &stage.raw_hash, true)?;
        match bound_read_verified(
            &CasReadAccess::Unmetered,
            &parent,
            &leaf,
            ObjectKind::Raw,
            &stage.raw_hash,
            false,
        ) {
            Ok((size, _)) if size == stage.size_bytes => {
                drop(stage.writer.take());
                stage
                    .directory
                    .discard_stage(Path::new(&stage.name), &stage.identity)?;
            }
            Ok(_) => return Err(bound_stage_corrupt("existing raw object size mismatch")),
            Err(error) if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => {
                let target = crate::store_dir::StoreDirectory::from_retained(
                    parent.try_clone().kio_io(Path::new(&leaf))?,
                    PathBuf::from("raw fanout"),
                )?;
                stage.writer.take().expect("live stage").publish(
                    &target,
                    Path::new(&leaf),
                    crate::store_dir::Publication::CreateOnly,
                )?;
            }
            Err(error) => return Err(error),
        }
        let (size, _) = bound_read_verified(
            &CasReadAccess::Unmetered,
            &parent,
            &leaf,
            ObjectKind::Raw,
            &stage.raw_hash,
            false,
        )?;
        if size != stage.size_bytes {
            return Err(bound_stage_corrupt("published raw object size mismatch"));
        }
        stage.journals.remove_file(Path::new(&stage.name))?;
        stage.name.clear();
        Ok((stage.raw_hash.clone(), size))
    }

    pub(super) fn cleanup_raw_ingest_orphans(&self) -> Result<()> {
        self.inspect_raw_stages(true).map(|_| ())
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
        ensure_bound_directory(&self.kio, Path::new(".kio"))?;
        ensure_bound_directory(&self.objects, Path::new("objects"))?;
        let digest = hash_path_component(hash)?;
        let first = &digest[..2];
        let second = &digest[2..4];
        let base = self.dir(kind);
        let first_dir = bound_open_or_create_dir(base, first, create)?;
        let second_dir = bound_open_or_create_dir(&first_dir, second, create)?;
        Ok((second_dir, digest.to_owned()))
    }

    pub(super) fn chunk_fanout(&self, hash: &str, create: bool) -> Result<(File, String)> {
        ensure_bound_directory(&self.kio, Path::new(".kio"))?;
        ensure_bound_directory(&self.objects, Path::new("objects"))?;
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
        ensure_bound_directory(&self.kio, Path::new(".kio"))?;
        ensure_bound_directory(&self.objects, Path::new("objects"))?;
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
        ensure_bound_directory(&self.kio, Path::new(".kio"))?;
        ensure_bound_directory(&self.objects, Path::new("objects"))?;
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
        write_bound_content_at(&parent, &leaf, kind, &hash, bytes).map(|()| hash)
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
        self.publish_raw_stage(self.stage_raw_from_reader(reader, max_bytes)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn abandoned_stage(
        repo: &crate::scope::Repository,
    ) -> (String, crate::store_dir::StageIdentity) {
        let mut stage = repo
            .object_store()
            .stage_raw_from_reader(&mut &b"stage payload"[..], 100)
            .unwrap();
        let name = stage.name.clone();
        let identity = stage.identity.clone();
        drop(stage.writer.take());
        stage.name.clear();
        (name, identity)
    }

    #[test]
    fn st04_recovery_uses_saved_identity_and_preserves_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let repo = crate::scope::Repository::init(dir.path()).unwrap();
        let (name, _) = abandoned_stage(&repo);
        let stage = repo.kio_dir().join("internal/raw-stages").join(&name);
        let journal = repo
            .kio_dir()
            .join("internal/raw-stage-identities")
            .join(&name);
        assert!(
            repo.object_store()
                .inspect_raw_stages()
                .unwrap()
                .iter()
                .any(|entry| entry.path.ends_with(&name))
        );
        repo.object_store().cleanup_bound_raw_stages().unwrap();
        assert!(!stage.exists());
        assert!(!journal.exists());
        let (name, _) = abandoned_stage(&repo);
        let stage = repo.kio_dir().join("internal/raw-stages").join(&name);
        let held = File::open(&stage).unwrap();
        fs::rename(&stage, stage.with_extension("original")).unwrap();
        fs::write(&stage, b"replacement").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&stage, fs::Permissions::from_mode(0o600)).unwrap();
        }
        repo.object_store().cleanup_bound_raw_stages().unwrap();
        assert_eq!(fs::read(&stage).unwrap(), b"replacement");
        let entries = repo.object_store().inspect_raw_stages().unwrap();
        assert!(
            entries
                .iter()
                .any(|entry| entry.path.ends_with(&name) && entry.reason.contains("Unidentified"))
        );
        drop(held);
    }

    #[test]
    fn st04_publication_rejects_changed_stage_and_existing_cas() {
        let dir = tempfile::tempdir().unwrap();
        let repo = crate::scope::Repository::init(dir.path()).unwrap();
        let store = repo.object_store();
        let stage = store
            .stage_raw_from_reader(&mut &b"first"[..], 100)
            .unwrap();
        let path = repo.kio_dir().join("internal/raw-stages").join(&stage.name);
        fs::write(&path, b"other").unwrap();
        assert!(store.publish_bound_raw_stage(stage).is_err());
        let hash = store.write_raw_reader(&mut &b"first"[..], 100).unwrap().0;
        let stage = store
            .stage_raw_from_reader(&mut &b"first"[..], 100)
            .unwrap();
        let path = crate::cas::fanout_path(repo.kio_dir().join("objects/raw"), &hash).unwrap();
        fs::write(path, b"other").unwrap();
        assert!(store.publish_bound_raw_stage(stage).is_err());
    }
}
