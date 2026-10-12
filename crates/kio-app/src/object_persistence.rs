use crate::index::rebuild::store_corrupt_error;
use crate::ledger_access::ledger_snapshot_to_kio;
use crate::open_view::cas_object_path;
use crate::open_view::existing_cas_object_path;
use crate::platform;
use kio_adapter::catalog::convert_office_to_pdf_with_control;
use kio_adapter::office_convert::is_office_media;
use kio_core::ExitCode;
use kio_core::KioError;
use kio_core::Result;
use kio_core::cas::ContentObjectKind;
use kio_core::cas::canonical_json_bytes;
use kio_core::cas::is_hash;
use kio_core::control::ControlHandle;
use kio_core::scope::Repository;
use kio_core::scope::new_ulid;
use kio_pipeline::markdownize::NormalizedInstanceManifest;
use kio_pipeline::markdownize::load_validated_normalized_instance_with_control;
use kio_pipeline::markdownize::persist_normalized_instance_with_control;
use kio_pipeline::prepare::PreparedUnit;
use kio_pipeline::prepare::hash_bytes;
use kio_pipeline::prepare::pdf_text_pages_bounded;
use kio_pipeline::task::normalized_task_output_ref;
use serde_json::json;
use std::fs;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::process;

pub(crate) fn atomic_overwrite_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| KioError::io("path has no parent", path.display().to_string()))?;
    fs::create_dir_all(parent)
        .map_err(|err| KioError::io(err.to_string(), parent.display().to_string()))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let tmp = parent.join(format!(
        ".{name}.tmp-{}-{}",
        process::id(),
        new_ulid(parent)
    ));
    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if let Err(err) = result {
        let _ = fs::remove_file(&tmp);
        return Err(KioError::io(err.to_string(), path.display().to_string()));
    }
    Ok(())
}

pub(crate) fn copy_normalized_instance_gen(
    kio_dir: &Path,
    raw_hash: &str,
    tool_profile_hash: &str,
    old_gen: u64,
    new_gen: u64,
    control: &ControlHandle,
) -> Result<()> {
    let old_dir = kio_pipeline::markdownize::normalized_instance_dir(
        kio_dir,
        raw_hash,
        tool_profile_hash,
        old_gen,
    );
    if let Ok(entries) = fs::read_dir(&old_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_str().is_some_and(is_orphan_temp_name)
                && entry
                    .file_type()
                    .map(|file_type| file_type.is_file())
                    .unwrap_or(false)
            {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    let mut instance = load_validated_normalized_instance_with_control(
        kio_dir,
        raw_hash,
        tool_profile_hash,
        old_gen,
        control,
    )
    .map_err(pipeline_to_kio)?;
    let generated_at = control.now_utc_seconds();
    instance.manifest.parent_gen = Some(old_gen);
    instance.manifest.r#gen = new_gen;
    instance.manifest.run_id = format!("run_{}", new_ulid(kio_dir));
    instance.manifest.generated_at = generated_at.clone();
    for unit in &mut instance.units {
        unit.r#gen = new_gen;
        unit.generated_at.clone_from(&generated_at);
    }
    persist_normalized_instance_with_control(kio_dir, &instance.manifest, &instance.units, control)
        .map(|_| ())
        .map_err(pipeline_to_kio)
}

/// Whether `name` matches the normalized-unit file naming convention written by
/// `persist_normalized_instance`: a 16-hex-char `unit_ref` digest plus `.json`
/// (e.g. `1a2b3c4d5e6f7089.json`). Anything else in a gen dir is not store truth
/// (R9-5) — `manifest.json`, a crashed writer's `.tmp-*`, a `.DS_Store`.
#[cfg(test)]
pub(crate) fn is_normalized_unit_file(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".json") else {
        return false;
    };
    stem.len() == 16
        && stem
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// Whether `name` looks like one of Kio's own atomic-write temp files
/// (`.tmp-<pid>-...` or `.<name>.tmp-<pid>-...`). Used by R9-5 to GC a
/// crash-orphaned temp left inside a gen dir by a killed writer.
pub(crate) fn is_orphan_temp_name(name: &str) -> bool {
    name.starts_with('.') && name.contains(".tmp-")
}

pub(crate) fn index_to_kio(error: kio_index::IndexError) -> KioError {
    KioError::schema(error.to_string())
}

pub(crate) fn search_to_kio(error: kio_search::SearchError) -> KioError {
    match error {
        kio_search::SearchError::Cursor(message) => KioError::new(
            "KIO-E-SEARCH-CURSOR-001",
            message,
            json!({}),
            ExitCode::InvalidUsage,
        ),
        kio_search::SearchError::Evidence(message) => KioError::schema(message),
        kio_search::SearchError::Contract(message) => KioError::schema(message),
    }
}

/// Q2: crash-atomic write of a derived CAS byte object (prepared / image). Writes
/// to a uniquely-named temp file in the destination directory, fsyncs it, then
/// renames into place, so a crash / ENOSPC mid-write can never leave a partial
/// file under the final `sha256:` name. `cas::atomic_write` is `pub(crate)` and
/// unreachable from here, so this mirrors it locally. The caller keeps its
/// `if !path.exists()` dedup skip before calling this.
fn ensure_contained_directory_chain(kio_dir: &Path, target: &Path) -> Result<()> {
    let canonical_kio = kio_dir
        .canonicalize()
        .map_err(|error| KioError::io(error.to_string(), kio_dir.display().to_string()))?;
    let relative = target
        .strip_prefix(kio_dir)
        .map_err(|_| KioError::schema("derived object directory escapes the repository store"))?;
    let mut current = kio_dir.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(KioError::schema(
                "derived object directory contains a non-local path component",
            ));
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => {
                return Err(store_corrupt_error(
                    &current,
                    "derived object ancestor is not a real directory",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&current)
                .map_err(|error| KioError::io(error.to_string(), current.display().to_string()))?,
            Err(error) => {
                return Err(KioError::io(
                    error.to_string(),
                    current.display().to_string(),
                ));
            }
        }
        let canonical = current
            .canonicalize()
            .map_err(|error| KioError::io(error.to_string(), current.display().to_string()))?;
        if !canonical.starts_with(&canonical_kio) {
            return Err(store_corrupt_error(
                &current,
                "derived object ancestor resolves outside the repository store",
            ));
        }
    }
    Ok(())
}

pub(crate) fn atomic_write_cas_object(kio_dir: &Path, path: &Path, bytes: &[u8]) -> Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .ok_or_else(|| KioError::io("path has no parent", path.display().to_string()))?;
    ensure_contained_directory_chain(kio_dir, parent)?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".tmp-{}-{}-{}", process::id(), nanos, seq));
    match fs::symlink_metadata(path) {
        Ok(_) => return verify_exact_cas_object(path, bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(KioError::io(error.to_string(), path.display().to_string())),
    }
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        platform::configure_owner_only_file(&mut options);
        let mut file = options
            .open(&temp)
            .map_err(|err| KioError::io(err.to_string(), temp.display().to_string()))?;
        file.write_all(bytes)
            .map_err(|err| KioError::io(err.to_string(), temp.display().to_string()))?;
        file.sync_all()
            .map_err(|err| KioError::io(err.to_string(), temp.display().to_string()))?;
        drop(file);
        let mut permissions = fs::metadata(&temp)
            .map_err(|err| KioError::io(err.to_string(), temp.display().to_string()))?
            .permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&temp, permissions)
            .map_err(|err| KioError::io(err.to_string(), temp.display().to_string()))?;
        match fs::hard_link(&temp, path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                verify_exact_cas_object(path, bytes)?;
            }
            Err(error) => {
                return Err(KioError::io(error.to_string(), path.display().to_string()));
            }
        }
        fs::remove_file(&temp)
            .map_err(|err| KioError::io(err.to_string(), temp.display().to_string()))?;
        verify_exact_cas_object(path, bytes)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn verify_exact_cas_object(path: &Path, expected: &[u8]) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|err| KioError::io(err.to_string(), path.display().to_string()))?;
    if !metadata.file_type().is_file() || metadata.len() != expected.len() as u64 {
        return Err(store_corrupt_error(
            path,
            "content-addressed object is not the expected regular file",
        ));
    }
    platform::validate_exact_cas_links(&metadata, path)?;
    let mut file = fs::File::open(path)
        .map_err(|err| KioError::io(err.to_string(), path.display().to_string()))?;
    let opened = file
        .metadata()
        .map_err(|err| KioError::io(err.to_string(), path.display().to_string()))?;
    if !opened.is_file() || opened.len() != expected.len() as u64 {
        return Err(store_corrupt_error(
            path,
            "content-addressed object changed while it was verified",
        ));
    }
    let mut actual = Vec::with_capacity(expected.len());
    (&mut file)
        .take((expected.len() as u64).saturating_add(1))
        .read_to_end(&mut actual)
        .map_err(|err| KioError::io(err.to_string(), path.display().to_string()))?;
    if actual != expected || hash_bytes(&actual) != hash_bytes(expected) {
        return Err(store_corrupt_error(
            path,
            "content-addressed object bytes do not match their identity",
        ));
    }
    Ok(())
}

fn verify_existing_cas_object(
    kio_dir: &Path,
    subdir: &str,
    hash: &str,
    bytes: &[u8],
) -> Result<bool> {
    if let Some(path) = existing_cas_object_path(kio_dir, subdir, hash)? {
        let parent = path
            .parent()
            .ok_or_else(|| KioError::io("path has no parent", path.display().to_string()))?;
        ensure_contained_directory_chain(kio_dir, parent)?;
        verify_exact_cas_object(&path, bytes)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

pub(crate) fn write_canonical_cas_object_or_reuse(
    kio_dir: &Path,
    subdir: &str,
    hash: &str,
    bytes: &[u8],
) -> Result<()> {
    if verify_existing_cas_object(kio_dir, subdir, hash, bytes)? {
        return Ok(());
    }

    // Close the preflight/publication window as far as possible before entering the
    // atomic canonical writer. Its own create-new publication handles a canonical
    // race, which is re-verified below.
    if verify_existing_cas_object(kio_dir, subdir, hash, bytes)? {
        return Ok(());
    }

    let canonical = cas_object_path(kio_dir, subdir, hash)?;
    atomic_write_cas_object(kio_dir, &canonical, bytes)?;
    if !verify_existing_cas_object(kio_dir, subdir, hash, bytes)? {
        return Err(KioError::not_found(hash));
    }
    Ok(())
}

pub(crate) fn write_prepared_objects(
    repo: &Repository,
    prepared_units: &[PreparedUnit],
    prepared_hashes: &[String],
    bytes: &[u8],
    media_type: &str,
) -> Result<()> {
    if prepared_units.len() != prepared_hashes.len() {
        return Err(KioError::schema(
            "prepared unit and object hash cardinalities differ",
        ));
    }
    // 07 §5.1: a DOCX/PPTX unit is a page/slide of the CONVERTED PDF, not the
    // original file -- its declared `prepared_hash` (kio-pipeline's
    // `prepare_units_from_bytes`, which converts+extracts the same way) is the
    // per-page TEXT of that converted PDF, exactly like the `application/pdf`
    // branch below but sourced from a freshly reconverted PDF (deterministic
    // renderer output, 07 §5.1) instead of `bytes` itself. The `!is_empty()`
    // guard skips conversion entirely when Prepare already reported no units
    // (no converter resolved, or a non-Office/media caller) -- nothing to write
    // and no need to invoke (and possibly fail on) a converter for a no-op.
    // The `!prepared_units.is_empty()` guard (both arms) skips the page
    // recompute when Prepare already reported no units: there is nothing to
    // write, and re-running the extractor here can DISAGREE with Prepare's
    // verdict — a scanned/raster PDF whose compressed bytes contain a chance
    // "BT" passes the text-layer gate and lossy-scans to one garbage page,
    // which Prepare's R20-4 garbage gate then rejects (0 units). Comparing
    // that recomputed 1 against 0 declared hashes failed the whole `kio
    // index` with KIO-E-CONFIG-SCHEMA-001 (found on real img2pdf rasters,
    // 2026-07-23 fixture registration). The office arm always had this
    // guard; the PDF arm was the asymmetry.
    //
    // 07 §5.1 (2026-07-25): XLSX joins the same shape with a third source. Its
    // unit is a worksheet and its declared `prepared_hash` is that sheet's
    // extracted Markdown, so the bytes to store are re-derived by re-running
    // the extractor — deterministic and local, no converter involved.
    let unit_texts = if media_type == "application/pdf" && !prepared_units.is_empty() {
        Some(pdf_text_pages_bounded(bytes).map_err(pipeline_to_kio)?)
    } else if is_office_media(media_type) && !prepared_units.is_empty() {
        let converted = convert_office_to_pdf_with_control(bytes, media_type, repo.control())
            .map_err(adapter_to_kio)?;
        Some(pdf_text_pages_bounded(&converted).map_err(pipeline_to_kio)?)
    } else if kio_adapter::xlsx_extract::is_xlsx_media(media_type) && !prepared_units.is_empty() {
        Some(
            kio_adapter::xlsx_extract::extract_xlsx(bytes)
                .map_err(adapter_to_kio)?
                .sheets
                .into_iter()
                .map(|sheet| sheet.markdown)
                .collect(),
        )
    } else {
        None
    };
    if unit_texts
        .as_ref()
        .is_some_and(|texts| texts.len() != prepared_hashes.len())
    {
        return Err(KioError::schema(
            "prepared unit text and object hash cardinalities differ",
        ));
    }
    for (index, prepared_hash) in prepared_hashes.iter().enumerate() {
        if !is_hash(prepared_hash) {
            return Err(KioError::schema("prepared object hash is invalid"));
        }
        let path = cas_object_path(repo.kio_dir(), "prepared", prepared_hash)?;
        let object_bytes = unit_texts
            .as_ref()
            .and_then(|texts| texts.get(index))
            .map_or(bytes, |text| text.as_bytes());
        if hash_bytes(object_bytes) != *prepared_hash
            || prepared_units[index].prepared_hash != *prepared_hash
        {
            return Err(store_corrupt_error(
                &path,
                "prepared object bytes do not match the declared hash",
            ));
        }
        write_canonical_cas_object_or_reuse(
            repo.kio_dir(),
            "prepared",
            prepared_hash,
            object_bytes,
        )?;
    }
    Ok(())
}

pub(crate) fn normalized_output_ref(
    _repo: &Repository,
    raw_hash: &str,
    tool_profile_hash: &str,
    r#gen: u64,
) -> String {
    normalized_task_output_ref(raw_hash, tool_profile_hash, r#gen)
}

/// PB04 (step4b-contract-tests-p2b.md §B; 10 §7.5.1 L493-497; 03 §8.1):
/// compute this normalized instance's *current* manifest.json canonical JCS
/// content hash and durably CAS-write it as a `ContentObjectKind::Manifest`
/// object (`objects/manifests/` — 03 §2.1), for `NormalizeRef::manifest_hash`
/// (tree schema v2). Reuses the same provenance-rebinding loader
/// `verify_objects`/purge's orphan scan already use rather than reading
/// `manifest.json` bytes ad hoc, so the hashed content is bound to the
/// requested `(raw_hash, tool_profile_hash, gen)` identity. `write_content_object`
/// is idempotent (verifies existing bytes rather than re-writing), matching
/// same-gen finalize's repeated manifest updates (03 §8, "unit の failed →
/// done 遷移で変わるため" — each transition yields a new manifest_hash).
///
/// This is a strict provenance boundary: callers must propagate any failure.
/// Every current-format `NormalizeRef` carries the returned manifest hash;
/// there is no missing-hash compatibility shape.
pub(crate) fn compute_manifest_hash(
    repo: &Repository,
    raw_hash: &str,
    tool_profile_hash: &str,
    r#gen: u64,
) -> Result<String> {
    let instance = load_validated_normalized_instance_with_control(
        repo.kio_dir(),
        raw_hash,
        tool_profile_hash,
        r#gen,
        repo.control(),
    )
    .map_err(pipeline_to_kio)?;
    hash_and_write_manifest_object(repo, &instance.manifest)
}

/// Low-level half of [`compute_manifest_hash`]: canonicalize an
/// already-in-memory `NormalizedInstanceManifest` and CAS-write it. Callers
/// that just finished `persist_normalized_instance` (the manifest is already
/// in hand, no need to re-read+re-validate it from disk) use this directly.
pub(crate) fn hash_and_write_manifest_object(
    repo: &Repository,
    manifest: &NormalizedInstanceManifest,
) -> Result<String> {
    let value =
        serde_json::to_value(manifest).map_err(|error| KioError::schema(error.to_string()))?;
    let bytes = canonical_json_bytes(&value)?;
    repo.object_store()
        .write_content_object(ContentObjectKind::Manifest, &bytes)
}

pub(crate) fn adapter_to_kio(error: kio_adapter::AdapterError) -> KioError {
    match error {
        kio_adapter::AdapterError::LocalPeerAuth(message) => KioError::new(
            "KIO-E-LOCAL-PEER-AUTH-001",
            message,
            json!({}),
            ExitCode::AuthError,
        ),
        kio_adapter::AdapterError::LocalPeerConfig { code, message } => {
            KioError::new(code, message, json!({}), ExitCode::InvalidUsage)
        }
        // A config violation that named its own operator-facing code (06 §8).
        // `AdapterError::error_code()` cannot carry it — that one is pinned to
        // the retry-policy table — so the code travels on the variant and is
        // read structurally here rather than sniffed out of the message text.
        // Every code in this class is exit 2.
        kio_adapter::AdapterError::ConfigSchemaCoded { code, message } => {
            KioError::new(code, message, json!({}), ExitCode::InvalidUsage)
        }
        other => KioError::schema(other.to_string()),
    }
}

pub(crate) fn pipeline_to_kio(error: kio_pipeline::PipelineError) -> KioError {
    match error {
        // M1(c): a corrupt persisted store file is exit 4 (KIO-E-STORE-CORRUPT-001),
        // not a schema/config error (exit 2). The path is preserved in context.
        kio_pipeline::PipelineError::Corrupt { path, message } => KioError::new(
            "KIO-E-STORE-CORRUPT-001",
            format!("corrupt store file at {path}: {message}"),
            json!({ "path": path }),
            ExitCode::PermanentFailure,
        ),
        // P1: a task whose input_path escapes the scope is KIO-E-STORE-PATH-001
        // (exit 2), the same contract as an out-of-scope tree entry path. The
        // offending path stays in context (redacted in logs), never in the message.
        kio_pipeline::PipelineError::Path { path } => KioError::new(
            "KIO-E-STORE-PATH-001",
            "task input_path must be a scope-local file name (no separators or `..` traversal)",
            json!({ "path": path }),
            ExitCode::InvalidUsage,
        ),
        kio_pipeline::PipelineError::Locked { path } => KioError::locked(path),
        kio_pipeline::PipelineError::Io { path, message } => KioError::io(message, path),
        kio_pipeline::PipelineError::LedgerSnapshot { path, source } => {
            ledger_snapshot_to_kio(&path, source)
        }
        // Lifecycle errors distinguish an explicit setup operation, recoverable
        // incomplete state, and unsafe durable state. A caller must not retry
        // a missing authority as if it were a transient provider failure.
        kio_pipeline::PipelineError::Contract { code, message } => {
            let exit = if matches!(
                code,
                "KIO-E-LEDGER-UNINITIALIZED-001"
                    | "KIO-E-LEDGER-ALREADY-EXISTS-001"
                    | "KIO-E-LEDGER-PATH-001"
            ) {
                ExitCode::InvalidUsage
            } else if matches!(
                code,
                "KIO-E-LEDGER-INIT-PARTIAL-001" | "KIO-E-LEDGER-SEQUENCE-MISMATCH-001"
            ) {
                ExitCode::PartialFailure
            } else if code.starts_with("KIO-E-LEDGER-") || code == "KIO-E-STORE-CONSTRAINT-001" {
                ExitCode::PermanentFailure
            } else {
                ExitCode::Failure
            };
            KioError::new(code, message, json!({}), exit)
        }
        other => KioError::schema(other.to_string()),
    }
}

pub(crate) fn diagnostic_safe_text(input: &str, allow_newlines: bool) -> String {
    let mut output = String::with_capacity(input.len());
    for ch in input.chars() {
        if allow_newlines && ch == '\n' {
            output.push(ch);
            continue;
        }
        let code = ch as u32;
        let terminal_active = matches!(code, 0x00..=0x1f | 0x7f..=0x9f)
            || matches!(code, 0x061c | 0x200e..=0x200f | 0x2028..=0x202e | 0x2066..=0x2069);
        if terminal_active {
            if code <= 0xff {
                output.push_str(&format!("\\x{code:02x}"));
            } else {
                output.push_str(&format!("\\u{{{code:04x}}}"));
            }
        } else {
            output.push(ch);
        }
    }
    output
}
