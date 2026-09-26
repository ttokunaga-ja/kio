//! QA15 (`kio ledger reconcile`'s orphan/unknown job-attribution walk,
//! 10-operations.md §7.5.2, step4b-contract-tests-p3a.md L323-338): a
//! provider-side Batch job/upload inventory seam.
//!
//! Historical note (§O): until the Batch send lane landed, no real Adapter
//! "Batch client" existed — both built-in Adapters were single-shot
//! synchronous integrations, so production correctly enumerated zero
//! providers. Since `batch_client::EnvMistralBatchClient` (07 §5.5, the
//! 2026-07-23 Batch-lane ruling), production enumerates the configured
//! Mistral Batch client's own list-jobs / list-uploads calls — one
//! [`ProviderInventory`] per configured client (0 to 2: the built-in Mistral
//! OCR lane and the Gemini embedding lane), scoped by that client's
//! `provider_scope_id`. The `KIO_TEST_BATCH_INVENTORY` fixture seam takes
//! precedence when set so the walk stays deterministic in tests.
//!
//! G1 (2026-07-25): the Gemini embedding lane was missing here. Its
//! `list_jobs` existed but had no caller, so a row stranded in §5.8's
//! job-creation window (相 2b started, job create failed) could never be
//! recovered — `kio ledger reconcile` reported it `unlistable` and its
//! reservation held the device budget cap forever.

use serde::Deserialize;

use crate::Result;
use crate::batch_client::{BatchJobRecord, MistralBatchClient};
use crate::gemini_batch_client::{GeminiBatchClient, display_name_intent_token};

/// 10 §7.5.2's job "帰属" (attribution) key: the same 4-tuple task identity
/// `kio_pipeline::ledger::TaskKey`/`batch_requests`'s PRIMARY KEY uses.
/// Duplicated here (not imported) rather than depended on, because the crate
/// dependency runs the other way (`kio-pipeline` depends on `kio-adapter`,
/// not vice versa) — see `kio_pipeline::ledger::ops::resolve_billing_from_reported_usage`'s
/// doc comment for the same one-directional-dependency note on the adapter
/// side of this boundary.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ProviderTaskKey {
    pub scope_id: String,
    pub adapter_kind: String,
    pub input_hash: String,
    pub tool_profile_hash: String,
}

/// One provider-side Batch job. `intent_token`/`task_key` come from the job
/// metadata the submit lane attaches at job-create time — the fixed 5-key
/// flat schema `{"intent_token", "scope_id", "adapter_kind", "input_hash",
/// "tool_profile_hash"}` (the CLI sends exactly this schema; see
/// [`provider_job_record`]) — 10 §7.5.2: "job の帰属は token 形式ではなく
/// job metadata の task key 4 組が担う...UUIDv7 token 単独では帰属できない".
/// `intent_token` is kept anyway (not just `task_key`) because §5.8's
/// `recovery_candidates` walk (QA15 step c, the OTHER direction of this
/// matching — "does the provider have MY row's job") still matches
/// found/confirmed-absent primarily by `intent_token`, exactly as
/// `04-pipeline.md §5.8`'s "found (job 取得/一覧で intent_token 一致)" rule
/// states; `task_key` alone would not answer that direction.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ProviderJobRecord {
    pub job_id: String,
    #[serde(default)]
    pub intent_token: Option<String>,
    #[serde(default)]
    pub task_key: Option<ProviderTaskKey>,
}

/// One provider-side upload. `filename_token` is the `intent_token` a real
/// upload's filename would embed (10 §7.5.2 / 04 §5.8: "intent_token 埋込
/// filename が upload 残骸の唯一の発見キー") — the ONLY attribution an upload
/// carries; unlike a job, an upload has no separate structured metadata to
/// fall back on, so a missing/unmatched `filename_token` is unconditionally
/// `unknown` (10 §7.5.2: "filename の token しか持たない upload は帰属不能
/// (unknown) として報告のみ").
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ProviderUploadRecord {
    pub upload_id: String,
    #[serde(default)]
    pub filename_token: Option<String>,
}

/// One configured Batch client's full job/upload listing — 10 §7.5.2's scan
/// set is built from `provider_scope_id` values (the "記録済み provider
/// scope と現在構成の各 Batch client の provider_scope_id を合わせた集合").
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct ProviderInventory {
    pub provider_scope_id: String,
    #[serde(default)]
    pub jobs: Vec<ProviderJobRecord>,
    #[serde(default)]
    pub uploads: Vec<ProviderUploadRecord>,
}

impl std::fmt::Debug for ProviderInventory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderInventory")
            .field("provider_scope_id", &"[REDACTED]")
            .field("jobs", &self.jobs)
            .field("uploads", &self.uploads)
            .finish()
    }
}

/// Env var naming a JSON fixture file (`Vec<ProviderInventory>`, serde) that
/// stands in for [`configured_inventories`]'s real provider calls in tests —
/// see that function's doc comment.
#[cfg(debug_assertions)]
pub const TEST_BATCH_INVENTORY_ENV: &str = "KIO_TEST_BATCH_INVENTORY";

/// `kio ledger reconcile`'s (QA15) provider-side job/upload listing for
/// every currently-configured Batch-capable Adapter client.
///
/// The `KIO_TEST_BATCH_INVENTORY` fixture seam wins when set (a path to a
/// JSON fixture file containing `[{provider_scope_id, jobs: [...],
/// uploads: [...]}, ...]`) so `kio ledger reconcile`'s orphan/unknown-
/// attribution walk can be exercised deterministically without a real
/// provider — the same "env var carries the fixture" convention as
/// `catalog.rs`'s `TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV`/
/// `TEST_ADOPTED_EMBEDDING_ENV` seams (those two pass inline text; this one
/// is a path — a full multi-provider inventory fixture is not a one-liner a
/// test would want to inline into an `env()` call).
///
/// Without the seam, this walks [`crate::batch_client::configured_mistral_batch_client`]:
/// `None` (the Mistral adapter is unconfigured) is an empty `Vec` — walking
/// zero providers is a correct answer, not a stub — and `Some` yields
/// exactly one [`ProviderInventory`] built from that client's own
/// `provider_scope_id` + `list_jobs` + `list_uploads` calls. Note the
/// `KIO_TEST_MISTRAL_BATCH` mock script therefore also reaches here (it IS
/// the configured client when set), which is how the metadata→attribution
/// mapping is tested hermetically.
pub fn configured_inventories(
    mut recovery_context: impl FnMut() -> Result<crate::batch_recovery::BatchRecoveryContext>,
) -> Result<Vec<ProviderInventory>> {
    #[cfg(debug_assertions)]
    if let Some(fixture_path) = crate::debug_test_control().adapters.batch_inventory {
        let text =
            std::fs::read_to_string(&fixture_path).map_err(|err| crate::AdapterError::Io {
                path: fixture_path.display().to_string(),
                message: err.to_string(),
            })?;
        return serde_json::from_str(&text).map_err(|err| {
            crate::AdapterError::ConfigSchema(format!("{}: {err}", fixture_path.display()))
        });
    }
    let mut inventories = Vec::new();
    if let Some(client) =
        crate::batch_client::configured_mistral_batch_client(&mut recovery_context)?
    {
        inventories.push(inventory_from_client(client.as_ref())?);
    }
    // G1: the embedding lane's provider. Unconfigured = contributes nothing,
    // exactly as the Mistral arm does.
    if let Some(client) =
        crate::gemini_batch_client::resolve_gemini_batch_client(&mut recovery_context)?
    {
        inventories.push(gemini_inventory_from_client(client.as_ref())?);
    }
    Ok(inventories)
}

/// One configured Gemini embedding Batch client's job listing.
///
/// `uploads` is ALWAYS empty and that is not an omission: the embedding lane
/// submits its input INLINE (07 §5.3 の 2026-07-24 訂正), so 相 2a does not
/// exist and no upload residue can be created. 10 §7.5.2's upload
/// reconciliation simply has nothing to reconcile for this provider.
fn gemini_inventory_from_client(client: &dyn GeminiBatchClient) -> Result<ProviderInventory> {
    let provider_scope_id = client.provider_scope_id()?;
    let jobs = client
        .list_jobs()?
        .into_iter()
        .map(|job| ProviderJobRecord {
            job_id: job.name,
            // A Gemini batch job carries no structured metadata — the
            // `displayName` is the only field Kio controls, so the
            // intent_token embedded there is its whole attribution
            // (`batch_display_name`). That is enough for §5.8's recovery
            // direction, which matches on `intent_token`.
            intent_token: display_name_intent_token(&job.display_name).map(str::to_owned),
            // No task-key 4-tuple exists to carry. `job_is_accounted_for`
            // falls back to `intent_token`, so attribution still works; a job
            // that matches nothing locally is reported `unknown` rather than
            // claimed — the same report-only posture 10 §7.5.2 gives an
            // upload, whose filename token is likewise its only attribution.
            task_key: None,
        })
        .collect();
    Ok(ProviderInventory {
        provider_scope_id,
        jobs,
        uploads: Vec::new(),
    })
}

/// One configured client's full listing, mapped to the reconcile walk's
/// records.
fn inventory_from_client(client: &dyn MistralBatchClient) -> Result<ProviderInventory> {
    let provider_scope_id = client.provider_scope_id()?;
    let jobs = client
        .list_jobs()?
        .into_iter()
        .map(provider_job_record)
        .collect();
    let uploads = client
        .list_uploads()?
        .into_iter()
        .map(|upload| ProviderUploadRecord {
            // 04 §5.8 / 10 §7.5.2: the filename-embedded intent token is the
            // ONLY attribution an upload carries; a foreign filename maps to
            // `None` = unknown, report-only.
            filename_token: crate::batch_client::filename_intent_token(&upload.filename)
                .map(str::to_owned),
            upload_id: upload.upload_id,
        })
        .collect();
    Ok(ProviderInventory {
        provider_scope_id,
        jobs,
        uploads,
    })
}

/// Job attribution reads the fixed 5-key flat metadata schema the submit
/// lane attaches at job-create (`intent_token` + the task key 4 組
/// `{"scope_id", "adapter_kind", "input_hash", "tool_profile_hash"}` — the
/// CLI sends this same schema; it is the contract between the two sides).
/// Any key that cannot be read as a string stays `None`: a partial 4 組 is
/// no `task_key` at all (unknown attribution, report-only — 10 §7.5.2),
/// never a guessed one.
fn provider_job_record(job: BatchJobRecord) -> ProviderJobRecord {
    let field = |key: &str| Some(job.metadata.get(key)?.as_str()?.to_owned());
    let intent_token = field("intent_token");
    let task_key = (|| {
        Some(ProviderTaskKey {
            scope_id: field("scope_id")?,
            adapter_kind: field("adapter_kind")?,
            input_hash: field("input_hash")?,
            tool_profile_hash: field("tool_profile_hash")?,
        })
    })();
    ProviderJobRecord {
        job_id: job.job_id,
        intent_token,
        task_key,
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    /// Production wiring (no fixture seam): one inventory per configured
    /// Batch client, jobs attributed through the 5-key metadata schema and
    /// uploads through the filename-embedded intent token. The configured
    /// client here is the hermetic `KIO_TEST_MISTRAL_BATCH` mock listing.
    #[test]
    fn configured_client_listing_maps_metadata_and_filename_attribution() {
        let _guard = kio_core::test_control::test_env_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _fixture = kio_core::test_control::TestEnvGuard::remove(TEST_BATCH_INVENTORY_ENV);
        let script = serde_json::json!({
            "provider_scope_id": "ws-live",
            "jobs_listing": [
                {
                    "job_id": "batch-attributed",
                    "status": "SUCCESS",
                    "output_file_id": "file-out-1",
                    "metadata": {
                        "intent_token": "01HTOKEN",
                        "scope_id": "01HSCOPE",
                        "adapter_kind": "markdownize",
                        "input_hash": "sha256:abc",
                        "tool_profile_hash": "sha256:def"
                    }
                },
                { "job_id": "batch-foreign", "status": "QUEUED" },
                {
                    "job_id": "batch-partial",
                    "status": "RUNNING",
                    "metadata": { "intent_token": "01HONLY", "scope_id": "01HSCOPE" }
                }
            ],
            "uploads_listing": [
                { "upload_id": "file-kio", "filename": "kio-01HTOKEN.jsonl" },
                { "upload_id": "file-stray", "filename": "notes.bin" }
            ]
        });
        let _script = kio_core::test_control::TestEnvGuard::set(
            crate::batch_client::TEST_MISTRAL_BATCH_ENV,
            script.to_string(),
        );
        let inventories = configured_inventories(|| {
            panic!("mock or unconfigured inventory must not load a private key")
        })
        .unwrap();

        assert_eq!(inventories.len(), 1);
        let inventory = &inventories[0];
        assert_eq!(inventory.provider_scope_id, "ws-live");
        assert_eq!(inventory.jobs.len(), 3);
        assert_eq!(inventory.jobs[0].job_id, "batch-attributed");
        assert_eq!(inventory.jobs[0].intent_token.as_deref(), Some("01HTOKEN"));
        assert_eq!(
            inventory.jobs[0].task_key,
            Some(ProviderTaskKey {
                scope_id: "01HSCOPE".to_owned(),
                adapter_kind: "markdownize".to_owned(),
                input_hash: "sha256:abc".to_owned(),
                tool_profile_hash: "sha256:def".to_owned(),
            })
        );
        // No metadata at all → fully unknown attribution.
        assert_eq!(inventory.jobs[1].intent_token, None);
        assert_eq!(inventory.jobs[1].task_key, None);
        // A partial 4 組 yields NO task_key (never a guessed one); the
        // intent_token still matches §5.8's recovery_candidates direction.
        assert_eq!(inventory.jobs[2].intent_token.as_deref(), Some("01HONLY"));
        assert_eq!(inventory.jobs[2].task_key, None);
        assert_eq!(inventory.uploads.len(), 2);
        assert_eq!(
            inventory.uploads[0].filename_token.as_deref(),
            Some("01HTOKEN")
        );
        assert_eq!(inventory.uploads[1].filename_token, None);
    }

    /// The test seam parses a well-formed fixture, including the `Option`
    /// fields' `#[serde(default)]` (absent `intent_token`/`task_key`/
    /// `filename_token` deserialize to `None`, not a schema error).
    #[test]
    fn test_seam_parses_a_well_formed_fixture() {
        let _guard = kio_core::test_control::test_env_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let fixture_path = dir.path().join("inventory.json");
        std::fs::write(
            &fixture_path,
            r#"[
                {
                    "provider_scope_id": "scope-a",
                    "jobs": [
                        {
                            "job_id": "job-1",
                            "intent_token": "01HZZZ",
                            "task_key": {
                                "scope_id": "local-scope",
                                "adapter_kind": "markdownize",
                                "input_hash": "sha256:abc",
                                "tool_profile_hash": "sha256:def"
                            }
                        },
                        { "job_id": "job-2" }
                    ],
                    "uploads": [
                        { "upload_id": "upload-1", "filename_token": "01HZZZ" },
                        { "upload_id": "upload-2" }
                    ]
                }
            ]"#,
        )
        .unwrap();
        let _fixture = kio_core::test_control::TestEnvGuard::set(
            TEST_BATCH_INVENTORY_ENV,
            fixture_path.as_os_str(),
        );
        let inventories = configured_inventories(|| {
            panic!("mock or unconfigured inventory must not load a private key")
        })
        .unwrap();

        assert_eq!(inventories.len(), 1);
        let inventory = &inventories[0];
        assert_eq!(inventory.provider_scope_id, "scope-a");
        assert_eq!(inventory.jobs.len(), 2);
        assert_eq!(inventory.jobs[0].job_id, "job-1");
        assert_eq!(inventory.jobs[0].intent_token.as_deref(), Some("01HZZZ"));
        assert_eq!(
            inventory.jobs[0].task_key,
            Some(ProviderTaskKey {
                scope_id: "local-scope".to_owned(),
                adapter_kind: "markdownize".to_owned(),
                input_hash: "sha256:abc".to_owned(),
                tool_profile_hash: "sha256:def".to_owned(),
            })
        );
        assert_eq!(inventory.jobs[1].job_id, "job-2");
        assert_eq!(inventory.jobs[1].intent_token, None);
        assert_eq!(inventory.jobs[1].task_key, None);
        assert_eq!(inventory.uploads.len(), 2);
        assert_eq!(
            inventory.uploads[0].filename_token.as_deref(),
            Some("01HZZZ")
        );
        assert_eq!(inventory.uploads[1].filename_token, None);
    }

    /// A missing fixture path (env var set but the file does not exist) is a
    /// loud error, never silently treated as "empty" — an operator who set
    /// the env var expected fixture content to be honored.
    #[test]
    fn missing_fixture_file_is_a_loud_error() {
        let _guard = kio_core::test_control::test_env_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _fixture = kio_core::test_control::TestEnvGuard::set(
            TEST_BATCH_INVENTORY_ENV,
            "/nonexistent/path/inventory.json",
        );
        let result = configured_inventories(|| {
            panic!("mock or unconfigured inventory must not load a private key")
        });
        assert!(result.is_err());
    }

    /// Malformed JSON is a loud `ConfigSchema` error, not a panic or a
    /// silent empty inventory.
    #[test]
    fn malformed_fixture_json_is_a_config_schema_error() {
        let _guard = kio_core::test_control::test_env_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let fixture_path = dir.path().join("bad.json");
        std::fs::write(&fixture_path, "{ not valid json").unwrap();
        let _fixture = kio_core::test_control::TestEnvGuard::set(
            TEST_BATCH_INVENTORY_ENV,
            fixture_path.as_os_str(),
        );
        let result = configured_inventories(|| {
            panic!("mock or unconfigured inventory must not load a private key")
        });
        match result {
            Err(crate::AdapterError::ConfigSchema(_)) => {}
            other => panic!("expected ConfigSchema error, got {other:?}"),
        }
    }
}
