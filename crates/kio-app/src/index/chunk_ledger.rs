use crate::object_persistence::index_to_kio;
use crate::platform;
use cap_primitives::fs as cap_fs;
use kio_core::ExitCode;
use kio_core::KioError;
use kio_core::Result;
use kio_core::cas::ChunkObject;
use kio_core::cas::is_hash;
use kio_core::control::ControlHandle;
use kio_core::purge::PurgeState;
use kio_core::scope::Repository;
use kio_core::scope::new_ulid;
use kio_core::scope::parse_utc_seconds;
use kio_index::ChunkRow;
use kio_index::chunking::chunk_hash;
use kio_index::fts::ExistingSourceIndexOpenMode;
use kio_index::fts::FtsSchemaConfig;
use kio_index::fts::FtsTokenizer;
use kio_index::fts::SqliteFtsIndex;
use kio_index::fts::open_existing_source_index_connection;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredChunk {
    pub(crate) rowid: u64,
    /// Stable rowid of this `(chunk_id, chunking_config_hash)` association.
    pub(crate) association_rowid: u64,
    #[serde(flatten)]
    pub(crate) row: ChunkRow,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChunkPublicationEvent {
    pub(crate) event: String,
    pub(crate) chunk_id: String,
    pub(crate) chunking_config_hash: String,
    pub(crate) introduction_commit: String,
}

fn index_dir(kio_dir: &Path) -> PathBuf {
    kio_dir.join("index")
}

pub(crate) fn chunks_jsonl_path(kio_dir: &Path) -> PathBuf {
    index_dir(kio_dir).join("chunks.jsonl")
}

/// A capability for the repository-owned `index` directory.  Once this is
/// open, all ledger leaf operations are relative to its handle, so replacing
/// `.kio/index` with a symlink cannot redirect a read, truncate, or append.
pub(crate) struct ChunkLedgerDir {
    path: PathBuf,
    // Retain `.kio` as the authority from which `index` was opened.  Holding
    // `index` alone is insufficient if `.kio` itself is renamed and replaced
    // by a symlink before the index handle is acquired.
    _root: std::fs::File,
    pub(crate) handle: std::fs::File,
}

pub(crate) fn open_chunk_ledger_dir(
    kio_dir: &Path,
    create_missing: bool,
) -> Result<ChunkLedgerDir> {
    let kio = platform::open_chunk_ledger_root(kio_dir)?;
    let root = kio_core::store_dir::StoreDirectory::from_retained(
        kio.try_clone()
            .map_err(|error| KioError::io(error.to_string(), kio_dir.display().to_string()))?,
        kio_dir.to_path_buf(),
    )?;
    kio_core::private_fs::verify_private_directory_handle(&root)?;
    let path = index_dir(kio_dir);
    let handle = match cap_fs::open_dir_nofollow(&kio, Path::new("index")) {
        Ok(handle) => handle,
        // macOS may report `ENOENT` from a no-follow `openat` through its
        // capability wrapper with a non-NotFound Rust kind.  Authorize mkdir
        // only after an independent non-follow metadata probe proves that the
        // final component is absent; an existing symlink/reparse entry never
        // reaches this branch.
        Err(_error)
            if create_missing
                && matches!(
                    cap_fs::stat(&kio, Path::new("index"), cap_fs::FollowSymlinks::No),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound
                ) =>
        {
            let created = match root.create_directory(Path::new("index")) {
                Ok(_) => true,
                Err(error)
                    if error.context().get("io_error_kind").and_then(Value::as_str)
                        == Some("already_exists") =>
                {
                    false
                }
                Err(error) => return Err(error),
            };
            if created {
                // Persist the new `index` entry in `.kio` before its child
                // ledger can be reported durable.
                platform::sync_chunk_ledger_directory(&kio, kio_dir)?;
            }
            cap_fs::open_dir_nofollow(&kio, Path::new("index"))
                .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?
        }
        Err(error) => return Err(KioError::io(error.to_string(), path.display().to_string())),
    };
    let retained = kio_core::store_dir::StoreDirectory::from_retained(
        handle
            .try_clone()
            .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?,
        path.clone(),
    )?;
    kio_core::private_fs::verify_private_directory_handle(&retained)?;
    Ok(ChunkLedgerDir {
        path,
        _root: kio,
        handle,
    })
}

fn try_open_chunk_ledger_file(
    directory: &ChunkLedgerDir,
    read: bool,
    write: bool,
    append: bool,
    create: bool,
) -> std::io::Result<std::fs::File> {
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(read)
        .write(write)
        .append(append)
        .create(create);
    platform::configure_chunk_ledger_open(&mut options);
    let retained = kio_core::store_dir::StoreDirectory::from_retained(
        directory.handle.try_clone()?,
        directory.path.clone(),
    )
    .map_err(|error| std::io::Error::other(error.to_string()))?;
    if create {
        retained
            .append_private(Path::new("chunks.jsonl"), &[])
            .map_err(|error| std::io::Error::other(error.to_string()))?;
    }
    options.create(false);
    let file = cap_fs::open(&directory.handle, Path::new("chunks.jsonl"), &options)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "chunks.jsonl must be a regular file",
        ));
    }
    platform::validate_chunk_ledger_links(&metadata)?;
    kio_core::private_fs::verify_private_regular_file_handle(&file)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    Ok(file)
}

pub(crate) fn open_chunk_ledger_file(
    directory: &ChunkLedgerDir,
    read: bool,
    write: bool,
    append: bool,
    create: bool,
) -> Result<std::fs::File> {
    try_open_chunk_ledger_file(directory, read, write, append, create).map_err(|error| {
        KioError::io(
            error.to_string(),
            directory.path.join("chunks.jsonl").display().to_string(),
        )
    })
}

pub(crate) fn sync_chunk_ledger_dir(directory: &ChunkLedgerDir) -> Result<()> {
    platform::sync_chunk_ledger_directory(&directory.handle, &directory.path)
}

/// Atomically replace the complete chunk ledger using the already-validated
/// repository-owned index-directory capability.  Purge compaction uses this
/// instead of a path-based truncate/write sequence so `.kio/index` replacement
/// cannot redirect the write outside the scope.
pub(crate) fn replace_chunk_ledger_contents(kio_dir: &Path, bytes: &[u8]) -> Result<()> {
    let directory = open_chunk_ledger_dir(kio_dir, true)?;
    let target = Path::new("chunks.jsonl");
    match cap_fs::stat(&directory.handle, target, cap_fs::FollowSymlinks::No) {
        Ok(metadata) => {
            if !metadata.is_file() {
                return Err(KioError::io(
                    "chunks.jsonl must be a regular file",
                    directory.path.join(target).display().to_string(),
                ));
            }
            platform::validate_chunk_ledger_existing_links(
                &metadata,
                &directory.path.join(target),
            )?;
            open_chunk_ledger_file(&directory, true, false, false, false)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(KioError::io(
                error.to_string(),
                directory.path.join(target).display().to_string(),
            ));
        }
    }

    let temp_name_prefix = format!(
        ".chunks.jsonl.replace.{}-{}",
        process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default()
    );
    let mut temp_name = None;
    let mut temp_file = None;
    for attempt in 0..32_u8 {
        let candidate = format!("{temp_name_prefix}-{attempt}");
        let retained = kio_core::store_dir::StoreDirectory::from_retained(
            directory.handle.try_clone().map_err(|error| {
                KioError::io(error.to_string(), directory.path.display().to_string())
            })?,
            directory.path.clone(),
        )?;
        match retained.create_private_file(Path::new(&candidate)) {
            Ok(file) => {
                temp_name = Some(candidate);
                temp_file = Some(file);
                break;
            }
            Err(error)
                if error.context().get("io_error_kind").and_then(Value::as_str)
                    == Some("already_exists") => {}
            Err(error) => {
                return Err(KioError::io(
                    error.to_string(),
                    directory.path.join(candidate).display().to_string(),
                ));
            }
        }
    }
    let temp_name = temp_name.ok_or_else(|| {
        KioError::io(
            "unable to allocate a unique temporary chunk ledger",
            directory.path.display().to_string(),
        )
    })?;
    let temp = Path::new(&temp_name);
    let result = (|| -> Result<()> {
        let mut file = temp_file.take().expect("temporary chunk ledger was opened");
        let metadata = file.metadata().map_err(|error| {
            KioError::io(
                error.to_string(),
                directory.path.join(temp).display().to_string(),
            )
        })?;
        if !metadata.is_file() {
            return Err(KioError::io(
                "temporary chunk ledger must be a regular file",
                directory.path.join(temp).display().to_string(),
            ));
        }
        platform::validate_chunk_ledger_temp_links(&metadata, &directory.path.join(temp))?;
        file.write_all(bytes).map_err(|error| {
            KioError::io(
                error.to_string(),
                directory.path.join(temp).display().to_string(),
            )
        })?;
        file.sync_all().map_err(|error| {
            KioError::io(
                error.to_string(),
                directory.path.join(temp).display().to_string(),
            )
        })?;
        drop(file);
        cap_fs::rename(&directory.handle, temp, &directory.handle, target).map_err(|error| {
            KioError::io(
                error.to_string(),
                directory.path.join(target).display().to_string(),
            )
        })?;
        sync_chunk_ledger_dir(&directory)
    })();
    if result.is_err() {
        let _ = cap_fs::remove_file(&directory.handle, temp);
    }
    result
}

pub(crate) fn sqlite_path(kio_dir: &Path) -> PathBuf {
    index_dir(kio_dir).join("sqlite.db")
}

/// LC39/LC40: `.kio/purge/epoch` is fail-closed on read (`PurgeState::
/// read_purge_epoch`, already implemented in `kio-core::purge` and now
/// actually exercised by every §I read-barrier checkpoint this session
/// wired) — a scope that has never gone through `kio purge`'s own
/// `execute_phase_machine` (the only pre-existing caller of
/// `PurgeState::ensure_purge_epoch`) never had this file created at all, so
/// wiring the read barrier alone would permanently fail-closed EVERY read on
/// EVERY scope that has never been purged. LC40's recovery-target priority
/// (active journal's `target_epoch`, else `max_recorded_purge_epoch() + 1`,
/// else `1`) is the same computation `execute_phase_machine` already does
/// inline; this is the general-write-command-entry counterpart LC40's own
/// "回復は書込系のみの責務" (recovery is write-side-only) puts outside the
/// purge command specifically. Idempotent — a no-op once the file exists.
pub(crate) fn ensure_purge_epoch_initialized(
    kio_dir: &Path,
    control: &ControlHandle,
) -> Result<()> {
    let state = PurgeState::open_with_control(kio_dir, control.clone())?;
    let recovery_target = match state.read_journal()? {
        Some(journal) => journal.target_epoch,
        None => state
            .max_recorded_purge_epoch()?
            .map_or(1, |max| max.saturating_add(1)),
    };
    state.ensure_purge_epoch(recovery_target)?;
    Ok(())
}

/// LC25/LC42-LC44: writer-side `index_metadata` synchronization — the
/// SQLite half of the lifecycle-epoch rotation whose counter/detection
/// logic (`PurgeState::recover_lifecycle_epoch`/`max_recorded_lifecycle_epoch`)
/// is already implemented in `kio-core::purge`. Called both from every write
/// command that actually touches the search index (`kio index`/`kio
/// reindex`/`kio repair rebuild-db`, after `rebuild_step3_index` — whose
/// temp-build-then-rename replaces `sqlite.db` wholesale, so writing
/// `index_metadata` any earlier in the same command would be silently
/// discarded by that rename) and from `kio purge` (whose own marker-append
/// writes `sqlite.db` in place, no rename involved, so this runs right after
/// its phase machine completes). `kio batch *` never rebuilds or otherwise
/// touches the index, so it has no stale `index_metadata` to reconcile. A
/// never-yet-initialized row (a fresh store, or a `sqlite.db` that predates
/// this table) is seeded to the *current* lifecycle-epoch counter (LC42)
/// rather than misread as a rollback from the column's own `DEFAULT 0`.
pub(crate) fn recover_index_generation(kio_dir: &Path, control: &ControlHandle) -> Result<()> {
    let fts = SqliteFtsIndex::open(
        sqlite_path(kio_dir),
        FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        },
    )
    .map_err(index_to_kio)?;
    let conn = fts.connection();
    let purge = PurgeState::open_with_control(kio_dir, control.clone())?;
    match kio_index::fts::read_index_metadata(conn).map_err(index_to_kio)? {
        None => {
            let generation = new_ulid(kio_dir);
            let current = purge.read_lifecycle_epoch()?;
            kio_index::fts::ensure_index_metadata(conn, &generation, current)
                .map_err(index_to_kio)?;
        }
        Some(metadata) => {
            // LC43: `max(last_lifecycle_epoch, max event lifecycle_epoch)`.
            let max_event_lifecycle_epoch = purge.max_recorded_lifecycle_epoch()?;
            let recovery = purge.recover_lifecycle_epoch(
                metadata.last_lifecycle_epoch,
                max_event_lifecycle_epoch,
            )?;
            // Rotate whenever the tracked value is out of sync with the
            // counter — either because `recover_lifecycle_epoch` detected
            // and repaired a genuine rollback (`rotated=true`; LC44's
            // "unconditional 1 rotation", `recovery.value` = the
            // freshly-recreated `max+1` counter), or simply because a
            // normal retire/re-purge advanced the counter
            // since this row was last written (`rotated=false`, but
            // `recovery.value` — the current counter — still differs from
            // the stale `metadata.last_lifecycle_epoch`). LC25 requires a
            // rotation on EVERY lifecycle event, not only a crash-recovered
            // one ("回転は retire append と同一 locked mutation 内で直後に行う" —
            // any of them can invalidate an outstanding search cursor), so
            // both cases take the same unconditional-rotation path here.
            if recovery.value != metadata.last_lifecycle_epoch {
                let generation = new_ulid(kio_dir);
                kio_index::fts::rotate_index_generation(conn, &generation, recovery.value)
                    .map_err(index_to_kio)?;
            }
        }
    }
    Ok(())
}

/// PC20 (05 §1.5 L180-184): mint a fresh `index_generation` ULID
/// unconditionally, invalidating every outstanding search cursor for this
/// scope. §R ruling 2 ("併存が正 — 統合しない"): this is a SEPARATE trigger
/// from `recover_index_generation`'s own lifecycle-epoch-conditional
/// rotation, not a replacement for it — callers invoke both (this one first,
/// so a write that ALSO happens to move the lifecycle-epoch counter is not
/// short-circuited into a single rotation by an early return here).
///
/// `index`/`reindex`/`repair rebuild-db` already rotate on every pass via
/// `build_sqlite_index_at`'s unconditional `ensure_index_metadata` call on
/// its always-fresh temp db (PB28) — this helper is for the writers that
/// mutate `sqlite.db` IN PLACE instead of via that temp+rename (purge's
/// `delete_derived_surfaces`, an embedding-enrichment finalize): callers
/// invoke it once, right after their own SQLite write commits.
/// `last_lifecycle_epoch` is read fresh (never assumed unchanged) so this
/// never regresses that column against a concurrent lifecycle-epoch update.
///
/// Not a same-transaction guarantee (05 §1.5 L188's "回転は... 同一の SQLite
/// Tx で行う"): the write this reacts to has already committed and closed
/// its own connection by the time this opens a new one, unlike the
/// rebuild/lifecycle-epoch triggers, which rotate inside their own already-
/// open transaction. A crash strictly between that commit and this call
/// would leave a stale-but-valid cursor reusable — a narrow window this
/// scope's own completion gate accepts (no durable "rotation pending"
/// marker exists to close it, unlike the lifecycle-epoch counter file
/// `PurgeState` already maintains for §3.5's crash-safe recovery).
pub(crate) fn rotate_index_generation_unconditionally(
    kio_dir: &Path,
    control: &ControlHandle,
) -> Result<()> {
    let path = sqlite_path(kio_dir);
    if !path.exists() {
        return Ok(());
    }
    let fts = SqliteFtsIndex::open(
        &path,
        FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        },
    )
    .map_err(index_to_kio)?;
    let conn = fts.connection();
    let current_lifecycle_epoch =
        PurgeState::open_with_control(kio_dir, control.clone())?.read_lifecycle_epoch()?;
    let generation = new_ulid(kio_dir);
    kio_index::fts::rotate_index_generation(conn, &generation, current_lifecycle_epoch)
        .map_err(index_to_kio)?;
    Ok(())
}

/// LC45: read-command-entry check — the current `.kio/tombstones/lifecycle-epoch`
/// counter must equal `index_metadata.last_lifecycle_epoch` exactly (a
/// mismatch in EITHER direction is retryable, not just counter-ahead). A
/// never-yet-initialized `index_metadata` (no write command has visited this
/// scope's index yet, or it predates this table) has nothing to roll back
/// from, so it is not a violation — `recover_index_generation`'s write-side
/// seeding is what will populate it, not this read-only check.
pub(crate) fn check_index_generation_current(
    kio_dir: &Path,
    control: &ControlHandle,
) -> Result<()> {
    let db_path = sqlite_path(kio_dir);
    if !db_path.exists() {
        return Ok(());
    }
    let conn = open_existing_source_index_connection(
        &db_path,
        ExistingSourceIndexOpenMode::ReadOnly,
        &FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        },
    )
    .map_err(index_to_kio)?;
    let Some(metadata) = kio_index::fts::read_index_metadata(&conn).map_err(index_to_kio)? else {
        return Ok(());
    };
    let current =
        PurgeState::open_with_control(kio_dir, control.clone())?.read_lifecycle_epoch()?;
    if current != metadata.last_lifecycle_epoch {
        return Err(KioError::new(
            "KIO-E-INDEX-REBUILDING-001",
            "the search index's lifecycle-epoch bookkeeping is out of date; retry",
            json!({}),
            ExitCode::PartialFailure,
        ));
    }
    Ok(())
}

pub(crate) struct ParsedChunkLedger {
    creations: Vec<(usize, StoredChunk)>,
    events: Vec<ChunkPublicationEvent>,
}

/// Parse the append-only ledger as framed raw bytes.  Only an unterminated final
/// frame is crash residue: every newline-terminated frame must be valid UTF-8
/// and valid JSON.  In particular, invalid UTF-8 is never silently accepted in
/// an earlier record.
pub(crate) fn parse_chunk_ledger(kio_dir: &Path) -> Result<ParsedChunkLedger> {
    let path = chunks_jsonl_path(kio_dir);
    let directory = match open_chunk_ledger_dir(kio_dir, false) {
        Ok(directory) => directory,
        Err(_error)
            if matches!(
                fs::symlink_metadata(index_dir(kio_dir)),
                Err(metadata_error) if metadata_error.kind() == std::io::ErrorKind::NotFound
            ) =>
        {
            return Ok(ParsedChunkLedger {
                creations: Vec::new(),
                events: Vec::new(),
            });
        }
        Err(error) => return Err(error),
    };
    let mut file = match try_open_chunk_ledger_file(&directory, true, false, false, false) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ParsedChunkLedger {
                creations: Vec::new(),
                events: Vec::new(),
            });
        }
        Err(error) => return Err(KioError::io(error.to_string(), path.display().to_string())),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?;
    let complete_len = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |i| i + 1);
    let complete = std::str::from_utf8(&bytes[..complete_len]).map_err(|error| {
        corrupt_chunk_ledger_error(
            &path,
            bytes[..error.valid_up_to()]
                .iter()
                .filter(|b| **b == b'\n')
                .count()
                + 1,
            "invalid UTF-8 in framed record",
        )
    })?;
    // The bytes after the final newline are one incomplete frame.  Deliberately
    // ignore it without decoding: it may end inside a multi-byte UTF-8 scalar.
    let mut creations = Vec::new();
    let mut events = Vec::new();
    let mut event_keys = BTreeSet::new();
    let mut created_pairs = BTreeSet::new();
    for (line_no, line) in complete.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line)
            .map_err(|e| corrupt_chunk_ledger_error(&path, line_no + 1, &e.to_string()))?;
        if value.get("event").is_some() {
            let event: ChunkPublicationEvent = serde_json::from_value(value)
                .map_err(|e| corrupt_chunk_ledger_error(&path, line_no + 1, &e.to_string()))?;
            if event.event != "publication"
                || !is_hash(&event.chunk_id)
                || !is_hash(&event.chunking_config_hash)
                || !is_hash(&event.introduction_commit)
            {
                return Err(corrupt_chunk_ledger_error(
                    &path,
                    line_no + 1,
                    "invalid publication event",
                ));
            }
            if !created_pairs
                .contains(&(event.chunk_id.clone(), event.chunking_config_hash.clone()))
            {
                return Err(corrupt_chunk_ledger_error(
                    &path,
                    line_no + 1,
                    "publication event precedes its creation association",
                ));
            }
            if event_keys.insert((
                event.chunk_id.clone(),
                event.chunking_config_hash.clone(),
                event.introduction_commit.clone(),
            )) {
                events.push((line_no + 1, event));
            }
        } else {
            let chunk: StoredChunk = serde_json::from_value(value)
                .map_err(|e| corrupt_chunk_ledger_error(&path, line_no + 1, &e.to_string()))?;
            validate_stored_chunk_ledger_row(&chunk)
                .map_err(|error| corrupt_chunk_ledger_error(&path, line_no + 1, &error))?;
            created_pairs.insert((
                chunk.row.chunk_id.clone(),
                chunk.row.chunking_config_hash.clone(),
            ));
            creations.push((line_no + 1, chunk));
        }
    }
    Ok(ParsedChunkLedger {
        creations,
        events: events.into_iter().map(|(_, event)| event).collect(),
    })
}

/// Validate every creation record before it participates in any durable-ledger
/// relation.  This is intentionally independent of a history walk: parsing a
/// complete framed row must reject malformed identity metadata even if the
/// relevant commit has subsequently become unreachable.
fn validate_stored_chunk_ledger_row(chunk: &StoredChunk) -> std::result::Result<(), String> {
    if chunk.rowid == 0 || chunk.association_rowid == 0 {
        return Err("chunk and association rowids must be positive".to_owned());
    }
    let row = &chunk.row;
    for (name, value) in [
        ("chunk_id", row.chunk_id.as_str()),
        ("raw_hash", row.raw_hash.as_str()),
        ("tool_profile_hash", row.tool_profile_hash.as_str()),
        ("unit_content_hash", row.unit_content_hash.as_str()),
        ("chunking_config_hash", row.chunking_config_hash.as_str()),
        ("text_hash", row.text_hash.as_str()),
    ] {
        if !is_hash(value) {
            return Err(format!("invalid {name}"));
        }
    }
    if parse_utc_seconds(&row.created_at).is_none() {
        return Err("invalid created_at timestamp".to_owned());
    }
    if row.unit_key.is_empty() || row.raw_path.is_empty() {
        return Err("chunk identity fields must not be empty".to_owned());
    }
    if row.byte_start > row.byte_end {
        return Err("chunk byte range is inverted".to_owned());
    }
    if kio_core::cas::hash_bytes(row.text.as_bytes()) != row.text_hash {
        return Err("chunk text_hash does not match text".to_owned());
    }
    let canonical_chunk_id = chunk_hash(row).map_err(|error| error.to_string())?;
    if canonical_chunk_id != row.chunk_id {
        return Err("chunk identity hash does not match canonical row fields".to_owned());
    }
    Ok(())
}

pub(crate) fn read_stored_chunks(kio_dir: &Path) -> Result<Vec<StoredChunk>> {
    let path = chunks_jsonl_path(kio_dir);
    let chunks = parse_chunk_ledger(kio_dir)?.creations;

    for (line, chunk) in &chunks {
        if chunk.rowid == 0 || chunk.association_rowid == 0 {
            return Err(corrupt_chunk_ledger_error(
                &path,
                *line,
                "chunk and association rowids must be positive",
            ));
        }
    }
    let mut association_owners = BTreeMap::<u64, (String, String)>::new();
    let mut known_associations = BTreeSet::<(String, String)>::new();
    let mut chunk_rowid_owners = BTreeMap::<u64, String>::new();
    let mut rowid_for_chunk = BTreeMap::<String, u64>::new();
    for (line, chunk) in &chunks {
        let association = (
            chunk.row.chunk_id.clone(),
            chunk.row.chunking_config_hash.clone(),
        );
        if !known_associations.insert(association.clone()) {
            return Err(corrupt_chunk_ledger_error(
                &path,
                *line,
                "duplicate chunk/config association",
            ));
        }
        let association_rowid = chunk.association_rowid;
        if association_owners
            .insert(association_rowid, association.clone())
            .is_some()
        {
            return Err(corrupt_chunk_ledger_error(
                &path,
                *line,
                "association rowid is assigned more than once",
            ));
        }
        if chunk_rowid_owners
            .insert(chunk.rowid, chunk.row.chunk_id.clone())
            .is_some_and(|owner| owner != chunk.row.chunk_id)
        {
            return Err(corrupt_chunk_ledger_error(
                &path,
                *line,
                "chunk rowid is assigned to multiple chunk ids",
            ));
        }
        if rowid_for_chunk
            .insert(chunk.row.chunk_id.clone(), chunk.rowid)
            .is_some_and(|rowid| rowid != chunk.rowid)
        {
            return Err(corrupt_chunk_ledger_error(
                &path,
                *line,
                "one chunk id is assigned multiple chunk rowids",
            ));
        }
    }
    Ok(chunks.into_iter().map(|(_, chunk)| chunk).collect())
}

/// Read durable publication event rows from the chunk ledger. Creation rows
/// remain parsed by `read_stored_chunks`; publication rows are additive and do
/// not create another chunk/config association.
pub(crate) fn read_chunk_publication_events(kio_dir: &Path) -> Result<Vec<ChunkPublicationEvent>> {
    Ok(parse_chunk_ledger(kio_dir)?.events)
}

fn corrupt_chunk_ledger_error(path: &Path, line: usize, message: &str) -> KioError {
    KioError::new(
        "KIO-E-STORE-CORRUPT-001",
        "corrupt chunks.jsonl record",
        json!({
            "path": path.display().to_string(),
            "line": line,
            "message": message,
        }),
        ExitCode::PermanentFailure,
    )
}

/// Publish the exact semantic chunk CAS object before the JSONL/SQLite
/// acceleration record can make the chunk discoverable.
pub(crate) fn persist_chunk_object(repo: &Repository, row: &ChunkRow) -> Result<()> {
    let object = ChunkObject {
        spec_version: 1,
        raw_hash: row.raw_hash.clone(),
        tool_profile_hash: row.tool_profile_hash.clone(),
        r#gen: row.r#gen,
        unit_key: row.unit_key.clone(),
        unit_content_hash: row.unit_content_hash.clone(),
        heading_path: row.heading_path.clone().unwrap_or_default(),
        section_id: row.section_id.clone().filter(|value| !value.is_empty()),
        byte_start: row.byte_start,
        byte_end: row.byte_end,
        text_hash: row.text_hash.clone(),
        text: row.text.clone(),
    };
    let stored_hash = repo.object_store().write_chunk(&object)?;
    if stored_hash != row.chunk_id {
        return Err(KioError::new(
            "KIO-E-STORE-CORRUPT-001",
            "chunk ledger identity does not match the canonical chunk object",
            json!({
                "ledger_chunk_hash": row.chunk_id,
                "canonical_chunk_hash": stored_hash,
            }),
            ExitCode::PermanentFailure,
        ));
    }
    Ok(())
}

/// Q1: physically drop a torn trailing record from `chunks.jsonl` before the next
/// append. `read_stored_chunks` already tolerates a torn LAST line at read time,
/// but read-skip alone is not self-healing: a torn line has no trailing `'\n'`, so
/// the next `append_stored_chunks` welds its record onto the torn bytes, forming a
/// newline-terminated *malformed* line that is then skipped forever — its chunk
/// re-generated and re-appended on every rebuild, and reclassified as
/// `KIO-E-STORE-CORRUPT-001` the instant that welded line stops being last (which
/// re-bricks `index` / `reindex` / `repair`).
///
/// A well-formed `chunks.jsonl` always ends in `'\n'`, so "file exists and its last
/// byte is not `'\n'`" is a reliable torn-tail signal. Truncate to just after the
/// last `'\n'` (or to 0 when none exists) so the append lands on a clean record
/// boundary and the whole index / reindex / repair path fully self-heals.
///
/// Only reached after `read_stored_chunks` returned `Ok`: a genuinely corrupt
/// NON-final line surfaces as `KIO-E-STORE-CORRUPT-001` there and never reaches
/// here, so real corruption is never silently truncated (multi-layer defense).
pub(crate) fn truncate_torn_chunk_tail(kio_dir: &Path) -> Result<()> {
    let path = chunks_jsonl_path(kio_dir);
    let directory = match open_chunk_ledger_dir(kio_dir, false) {
        Ok(directory) => directory,
        Err(_error)
            if matches!(
                fs::symlink_metadata(index_dir(kio_dir)),
                Err(metadata_error) if metadata_error.kind() == std::io::ErrorKind::NotFound
            ) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let mut file = match try_open_chunk_ledger_file(&directory, true, true, false, false) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(KioError::io(error.to_string(), path.display().to_string())),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|err| KioError::io(err.to_string(), path.display().to_string()))?;
    if bytes.is_empty() || bytes.last() == Some(&b'\n') {
        return Ok(());
    }
    // Keep everything up to and including the last newline (0 when none exists).
    let keep = bytes
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map_or(0, |index| index + 1);
    file.set_len(keep as u64)
        .map_err(|err| KioError::io(err.to_string(), path.display().to_string()))?;
    file.sync_all()
        .map_err(|err| KioError::io(err.to_string(), path.display().to_string()))?;
    Ok(())
}

pub(crate) fn append_stored_chunks(kio_dir: &Path, chunks: &[StoredChunk]) -> Result<()> {
    if chunks.is_empty() {
        return Ok(());
    }
    let path = chunks_jsonl_path(kio_dir);
    let directory = open_chunk_ledger_dir(kio_dir, true)?;
    let mut file = open_chunk_ledger_file(&directory, false, true, true, true)?;
    for chunk in chunks {
        // M1(b): one framed record per single write_all (no interleaving).
        let mut line = serde_json::to_string(chunk)
            .map_err(|err| KioError::io(err.to_string(), path.display().to_string()))?;
        line.push('\n');
        file.write_all(line.as_bytes())
            .map_err(|err| KioError::io(err.to_string(), path.display().to_string()))?;
    }
    file.sync_all()
        .map_err(|err| KioError::io(err.to_string(), path.display().to_string()))?;
    sync_chunk_ledger_dir(&directory)?;
    Ok(())
}

pub(crate) fn append_chunk_publication_events(
    kio_dir: &Path,
    events: &[ChunkPublicationEvent],
) -> Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    // Idempotence belongs at the durable boundary, not only at individual
    // callers: retries may re-enter after a successful append but before their
    // next projection step.
    let creations = read_stored_chunks(kio_dir)?
        .into_iter()
        .map(|chunk| (chunk.row.chunk_id, chunk.row.chunking_config_hash))
        .collect::<BTreeSet<_>>();
    let mut seen = read_chunk_publication_events(kio_dir)?
        .into_iter()
        .map(|event| {
            (
                event.chunk_id,
                event.chunking_config_hash,
                event.introduction_commit,
            )
        })
        .collect::<BTreeSet<_>>();
    let mut to_append = Vec::new();
    for event in events {
        if event.event != "publication"
            || !is_hash(&event.chunk_id)
            || !is_hash(&event.chunking_config_hash)
            || !is_hash(&event.introduction_commit)
            || !creations.contains(&(event.chunk_id.clone(), event.chunking_config_hash.clone()))
        {
            return Err(KioError::schema("invalid publication event"));
        }
        if seen.insert((
            event.chunk_id.clone(),
            event.chunking_config_hash.clone(),
            event.introduction_commit.clone(),
        )) {
            to_append.push(event);
        }
    }
    if to_append.is_empty() {
        return Ok(());
    }
    let path = chunks_jsonl_path(kio_dir);
    let directory = open_chunk_ledger_dir(kio_dir, true)?;
    let mut file = open_chunk_ledger_file(&directory, false, true, true, true)?;
    for event in to_append {
        let mut line = serde_json::to_string(event).map_err(|e| KioError::schema(e.to_string()))?;
        line.push('\n');
        file.write_all(line.as_bytes())
            .map_err(|e| KioError::io(e.to_string(), path.display().to_string()))?;
    }
    file.sync_all()
        .map_err(|e| KioError::io(e.to_string(), path.display().to_string()))?;
    sync_chunk_ledger_dir(&directory)?;
    Ok(())
}
