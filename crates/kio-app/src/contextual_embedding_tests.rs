//! Product regression for embedding a retained chunk under multiple filenames.

use super::*;
use crate::commands::IndexArgs;
use crate::commands::{
    CommandRequest, InitArgs, LedgerArgs, LedgerCommand, RepairArgs, RepairOperation,
    RepairRebuildDbArgs,
};
use crate::context::{AppContext, Interaction};
use crate::embedding::admission::declared_embedding_profile;
use crate::embedding::admission::embedding_execution;
use crate::embedding::batch::completed_batch_result;
use crate::embedding::batch::embedding_job_input_hash;
use crate::embedding::executor::apply_embedding_transitions;
use crate::embedding::executor::embedding_done_transition;
use crate::embedding::executor::embedding_fail_transition;
use crate::embedding::executor::embedding_replica_projection_is_current;
use crate::embedding::executor::invalidate_embedding_projection;
use crate::embedding::executor::record_embedding_transitions;
use crate::embedding::plan::EmbeddingReconcileContext;
use crate::embedding::plan::bind_embedding_work;
use crate::embedding::plan::enqueue_embedding_tasks;
use crate::embedding::plan::filter_embeddable_by_task_state;
use crate::embedding::plan::reconcile_committed_embedding_tasks;
use crate::embedding::plan::retained_history_chunks;
use crate::embedding::plan::selected_embedding_contexts;
use crate::embedding::plan::validate_embedding_task_bindings;
use crate::embedding_owners::RetainedEmbeddingChunk;
use crate::historical_reindex::RetainedNormalizedInstance;
use crate::historical_reindex::retained_history_instances;
use crate::index::chunk_ledger::sqlite_path;
use crate::management::ExplicitRoot;
#[cfg(test)]
use crate::management::initialize_explicit_root;
use crate::management::open_existing_managed_root;
use crate::open_view::cas_object_path;
use crate::scope_access::purge_blocks_rebuild_raw;
use crate::search::projection::replica_scope_stamp;
use crate::search::query::snapshot_chunking_config_hash;
use crate::search::scope::aggregator_path;
use crate::search::scope::current_embedding_policy;
use crate::settings::embedding_profile_summary;
use crate::settings::persisted_embedding_is_deterministic;
use crate::settings::rebuild_embedding_entry_from_tool_lock;
use kio_adapter::catalog::DeclaredEmbeddingProfile;
use kio_adapter::catalog::EmbeddingExecution;
use kio_adapter::types::ExecutionMode;
use kio_core::control::ControlHandle;
use kio_core::purge::EventKind;
use kio_core::purge::PurgeState;
use kio_core::scope::PendingNormalizeRef;
use kio_core::scope::Repository;
use kio_index::embedding_store;
use kio_index::embedding_store::f32_to_le_bytes;
use kio_pipeline::prepare::hash_bytes;
use kio_pipeline::task::EmbeddingWorkKey;
use kio_pipeline::task::RetryErrorKind;
use kio_pipeline::task::TaskStatus;
use kio_pipeline::task::TaskStore;
use kio_pipeline::task::TaskType;
use rusqlite::Connection;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::Arc;

const CHILD: &str = "KIO_CONTEXTUAL_EMBEDDING_TEST_CHILD";
const OLD_PATH: &str = "a-old-topic.md";
const CURRENT_PATH: &str = "z-current-topic.md";

struct QuietInteraction;

impl Interaction for QuietInteraction {
    fn confirm(&self, _: &str) -> Result<bool> {
        Ok(true)
    }
    fn read_input(&self, _: usize) -> Result<Vec<u8>> {
        Ok(Vec::new())
    }
    fn is_interactive(&self) -> bool {
        false
    }
    fn diagnostic(&self, _: &str) {}
    fn write_diagnostic(&self, _: &str) -> kio_core::Result<()> {
        Ok(())
    }
    fn flush_diagnostics(&self) -> kio_core::Result<()> {
        Ok(())
    }
}

fn private_tempdir() -> crate::test_support::PrivateTempDir {
    crate::test_support::PrivateTempDir::new()
}

pub(super) fn isolated_child_for(test_name: &str) -> bool {
    if std::env::var_os(CHILD).is_some() {
        let context = AppContext {
            working_directory: std::env::current_dir().unwrap(),
            interaction: Arc::new(QuietInteraction),
        };
        execute(
            &context,
            CommandRequest::Ledger(LedgerArgs {
                command: Some(LedgerCommand::Init { resume: false }),
            }),
        )
        .unwrap();
        return true;
    }
    let private = private_tempdir();
    let parent = kio_core::store_dir::StoreDirectory::open(private.path()).unwrap();
    for leaf in ["home", "config", "data", "cache", "tmp"] {
        let directory = parent.create_directory(Path::new(leaf)).unwrap();
        kio_core::store_dir::restrict_new_private_directory(&directory).unwrap();
    }
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command.env_clear();
    for name in ["PATH", "SystemRoot", "WINDIR"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let output = command
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD, "1")
        .env("HOME", private.path().join("home"))
        .env("USERPROFILE", private.path().join("home"))
        .env("XDG_CONFIG_HOME", private.path().join("config"))
        .env("XDG_DATA_HOME", private.path().join("data"))
        .env("XDG_CACHE_HOME", private.path().join("cache"))
        .env("TMPDIR", private.path().join("tmp"))
        .env("TMP", private.path().join("tmp"))
        .env("TEMP", private.path().join("tmp"))
        .env("KIO_EVAL_DETERMINISTIC_EMBED", "scale-v3")
        .current_dir(private.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "isolated contextual embedding regression failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    false
}

fn index(context: &AppContext) {
    execute(
        context,
        CommandRequest::Index(IndexArgs {
            preview: false,
            yes: true,
            online: false,
            offline: true,
            realtime: false,
            batch: false,
        }),
    )
    .unwrap();
}

fn rebuild(context: &AppContext) {
    execute(
        context,
        CommandRequest::Repair(RepairArgs {
            operation: RepairOperation::RebuildDb(RepairRebuildDbArgs {
                online: false,
                offline: true,
                realtime: false,
                batch: false,
            }),
        }),
    )
    .unwrap();
}

#[derive(Debug, PartialEq, Eq)]
struct Projection {
    sources: Vec<(String, String, Option<String>, Vec<u8>)>,
    scalar: Vec<(String, Vec<u8>)>,
}

fn connection(repo: &Repository) -> Connection {
    kio_index::vec::ensure_registered();
    Connection::open(sqlite_path(repo.kio_dir())).unwrap()
}

fn projection(repo: &Repository) -> Projection {
    let conn = connection(repo);
    let sources = conn
        .prepare(
            "SELECT id, target_id, context_key, vector FROM embeddings
             WHERE target_type = 'chunk' ORDER BY id",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let scalar = conn
        .prepare("SELECT chunk_id, embedding FROM chunk_vec ORDER BY chunk_id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    Projection { sources, scalar }
}

fn assert_scalar_context(repo: &Repository, path: &str) {
    let expected_context = embedding_store::chunk_embedding_context(path).unwrap();
    let conn = connection(repo);
    let rows = conn
        .prepare(
            "SELECT c.chunk_id, v.embedding, e.vector FROM chunks c
             JOIN chunk_vec v ON v.chunk_id = c.chunk_id
             JOIN embeddings e ON e.target_type = 'chunk' AND e.target_id = c.text_hash
             WHERE e.context_key = ?1 ORDER BY c.chunk_id",
        )
        .unwrap()
        .query_map([expected_context], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert!(
        !rows.is_empty(),
        "fixture must have contextual scalar vectors"
    );
    let total: usize = conn
        .query_row("SELECT COUNT(*) FROM chunk_vec", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows.len(), total);
    for (chunk, actual, expected) in rows {
        assert_eq!(actual, expected, "wrong context selected for {chunk}");
    }
}

fn assert_selection_order_independent(repo: &Repository, expected_path: &str) {
    let conn = connection(repo);
    let head = repo.head_commit_hash().unwrap().unwrap();
    let retained = retained_history_instances(repo, &head).unwrap();
    let config = snapshot_chunking_config_hash(repo, &head).unwrap();
    let policy = current_embedding_policy(repo).unwrap();
    let mut candidates =
        retained_history_chunks(&conn, repo, &retained, &config, Some(&policy)).unwrap();
    let selected = selected_embedding_contexts(&candidates, &policy).unwrap();
    assert!(!selected.is_empty());
    assert!(
        selected
            .values()
            .all(|context| *context == embedding_store::chunk_embedding_context(expected_path))
    );
    candidates.reverse();
    assert_eq!(
        selected_embedding_contexts(&candidates, &policy).unwrap(),
        selected,
        "HEAD and retained fallback selection must ignore candidate iteration order"
    );
}

fn assert_sqlite_text_is_not_embedding_authority(repo: &Repository) {
    let conn = connection(repo);
    let head = repo.head_commit_hash().unwrap().unwrap();
    let retained = retained_history_instances(repo, &head).unwrap();
    let config = snapshot_chunking_config_hash(repo, &head).unwrap();
    let policy = current_embedding_policy(repo).unwrap();
    let collect =
        || retained_history_chunks(&conn, repo, &retained, &config, Some(&policy)).unwrap();
    let expected = collect();
    assert!(!expected.is_empty());
    let directory = kio_core::store_dir::StoreDirectory::from_retained(
        repo.bound_kio_handle().unwrap().try_clone().unwrap(),
        repo.kio_dir().to_path_buf(),
    )
    .unwrap();
    let read_chunk_bytes = |path: &Path| {
        directory
            .read_optional(path, kio_core::cas::MAX_CHUNK_OBJECT_BYTES)
            .unwrap()
            .unwrap()
    };
    let cas_before = expected
        .iter()
        .map(|chunk| {
            let path = kio_core::cas::fanout_path("objects/chunks", &chunk.chunk_id).unwrap();
            let bytes = read_chunk_bytes(&path);
            (path, bytes)
        })
        .collect::<BTreeMap<_, _>>();
    let fingerprint = |chunks: Vec<RetainedEmbeddingChunk>| {
        chunks
            .into_iter()
            .map(|chunk| {
                (
                    chunk.chunk_id,
                    chunk.text,
                    chunk.text_hash,
                    chunk.raw_path,
                    chunk.is_head_owner,
                )
            })
            .collect::<BTreeSet<_>>()
    };
    let expected = fingerprint(expected);
    const POISON: &str = "MALICIOUS SQLITE CACHE TEXT MUST NEVER REACH EMBEDDING INPUT";
    conn.execute_batch("SAVEPOINT poisoned_chunk_text").unwrap();
    assert!(
        conn.execute("UPDATE chunks SET text = ?1", [POISON])
            .unwrap()
            > 0
    );
    let actual = collect();
    assert!(actual.iter().all(|chunk| !chunk.text.contains(POISON)));
    assert_eq!(fingerprint(actual), expected);
    conn.execute_batch("ROLLBACK TO poisoned_chunk_text; RELEASE poisoned_chunk_text")
        .unwrap();
    assert_eq!(fingerprint(collect()), expected);
    for (path, bytes) in cas_before {
        assert_eq!(
            read_chunk_bytes(&path),
            bytes,
            "collector must preserve exact canonical CAS bytes"
        );
    }
}

#[test]
fn rename_retains_contexts_and_rebuild_selects_allowed_head_then_history() {
    let adapter_settings =
        &std::sync::Arc::new(kio_adapter::tool_lock::AdapterRuntimeSettings::default());

    if !isolated_child_for(
        "contextual_embedding_tests::rename_retains_contexts_and_rebuild_selects_allowed_head_then_history",
    ) {
        return;
    }
    let control = ControlHandle::new(DebugTestControl::from_env());
    // Direct queries use the child-root selector snapshot; actual execute()
    // calls capture their own command-root snapshot.
    assert!(matches!(
        embedding_execution(adapter_settings, &control),
        Some(EmbeddingExecution::DeterministicEvaluator)
    ));
    let root = private_tempdir();
    let context = AppContext {
        working_directory: root.path().to_path_buf(),
        interaction: Arc::new(QuietInteraction),
    };
    fs::write(
        root.path().join(OLD_PATH),
        "# Knowledge\n\nThe retained document explains orbital telescope calibration.\n",
    )
    .unwrap();
    execute(
        &context,
        CommandRequest::Init(InitArgs {
            path: None,
            independent: false,
            yes: false,
        }),
    )
    .unwrap();
    index(&context);
    let repo = open_existing_managed_root(root.path(), &control).unwrap();
    let before = projection(&repo);
    assert!(!before.sources.is_empty());
    assert_scalar_context(&repo, OLD_PATH);
    assert_sqlite_text_is_not_embedding_authority(&repo);

    fs::rename(root.path().join(OLD_PATH), root.path().join(CURRENT_PATH)).unwrap();
    index(&context);
    let renamed = projection(&repo);
    assert_eq!(renamed.sources.len(), before.sources.len() * 2);
    assert_eq!(renamed.scalar.len(), before.scalar.len());
    for source in &before.sources {
        assert!(
            renamed.sources.contains(source),
            "rename lost old contextual source"
        );
    }
    for (hash, text_hash, stored_context, vector) in &renamed.sources {
        let object = repo.object_store().read_embedding(hash).unwrap();
        assert_eq!(&object.target_hash, text_hash);
        assert_eq!(
            object
                .context
                .as_ref()
                .and_then(kio_core::cas::EmbeddingContext::as_text),
            stored_context.as_deref()
        );
        assert_eq!(&f32_to_le_bytes(&object.vector), vector);
    }
    assert_ne!(
        before.scalar, renamed.scalar,
        "different filenames must yield distinct vectors"
    );
    assert_scalar_context(&repo, CURRENT_PATH);
    assert_selection_order_independent(&repo, CURRENT_PATH);

    // The task journal must retain both independently completed contextual jobs.
    let tasks = TaskStore::new(repo.kio_dir()).all().unwrap();
    let completed = tasks
        .iter()
        .filter(|task| task.task_type == TaskType::Embedding && task.status == TaskStatus::Done)
        .map(|task| EmbeddingWorkKey::parse(&task.output_ref).unwrap())
        .collect::<BTreeSet<_>>();
    for (chunk, _) in &renamed.scalar {
        let contexts = completed
            .iter()
            .filter(|key| key.chunk_id() == chunk)
            .collect::<Vec<_>>();
        assert_eq!(
            contexts.len(),
            2,
            "rename must not retire or overwrite the old work key"
        );
        assert_ne!(contexts[0].embedding_hash(), contexts[1].embedding_hash());
    }

    // Recreate the SQLite projections from retained objects, without any send.
    rebuild(&context);
    assert_eq!(projection(&repo), renamed);
    assert_scalar_context(&repo, CURRENT_PATH);

    // Current policy excludes the HEAD filename. Its allowed retained name is
    // still a legitimate historical candidate, despite sorting before HEAD.
    fs::write(root.path().join(".kioignore"), format!("{CURRENT_PATH}\n")).unwrap();
    rebuild(&context);
    assert_scalar_context(&repo, OLD_PATH);
    assert_selection_order_independent(&repo, OLD_PATH);
    let fallback = projection(&repo);
    rebuild(&context);
    assert_eq!(projection(&repo), fallback);

    fs::write(
        root.path().join(".kioignore"),
        format!("{CURRENT_PATH}\n{OLD_PATH}\n"),
    )
    .unwrap();
    rebuild(&context);
    assert!(
        projection(&repo).scalar.is_empty(),
        "denied contexts must have no scalar projection"
    );
    // Policy changes projection authority, never deletion authority for CAS.
    for (hash, _, _, _) in &renamed.sources {
        repo.object_store().read_embedding(hash).unwrap();
    }
}

#[test]
fn batch_resume_repairs_interrupted_replica_publication_without_embedding_work() {
    use crate::commands::{BatchArgs, BatchCommand, ResumeArgs};
    use kio_index::aggregator::{AggIndexStatus, Aggregator};

    if !isolated_child_for(
        "contextual_embedding_tests::batch_resume_repairs_interrupted_replica_publication_without_embedding_work",
    ) {
        return;
    }
    let control = ControlHandle::new(DebugTestControl::from_env());
    let root = private_tempdir();
    let context = AppContext {
        working_directory: root.path().to_path_buf(),
        interaction: Arc::new(QuietInteraction),
    };
    fs::write(
        root.path().join(CURRENT_PATH),
        "# Knowledge\n\nOrbital telescope calibration uses a stable reference star.\n",
    )
    .unwrap();
    execute(
        &context,
        CommandRequest::Init(InitArgs {
            path: None,
            independent: false,
            yes: false,
        }),
    )
    .unwrap();
    index(&context);
    let repo = open_existing_managed_root(root.path(), &control).unwrap();
    let scope = repo.scope_identity().unwrap().scope_id;
    let expected = projection(&repo);
    assert!(!expected.scalar.is_empty());
    let tasks = TaskStore::new(repo.kio_dir()).all().unwrap();
    assert!(
        tasks
            .iter()
            .filter(|task| task.task_type == TaskType::Embedding)
            .all(|task| task.status == TaskStatus::Done)
    );
    let replica_vectors = || {
        Connection::open(aggregator_path(&control))
            .unwrap()
            .prepare(
                "SELECT c.chunk_id, e.vector FROM agg_embeddings e
                      JOIN agg_chunks c ON c.rowid = e.chunk_rowid
                      WHERE c.scope_id = ?1 ORDER BY c.chunk_id",
            )
            .unwrap()
            .query_map([&scope], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert_eq!(replica_vectors(), expected.scalar);
    let original_generation = replica_scope_stamp(repo.kio_dir()).unwrap().1;

    for state in [
        "rebuilding",
        "generation",
        "head",
        "config",
        "missing",
        "healthy",
    ] {
        if state == "rebuilding" {
            // Faithfully reproduce the post-scalar-commit/pre-publication cut:
            // the final vectors and Done tasks already exist, while the old
            // replica is invalidated and source generation has advanced.
            let _lock = repo.lock_store_in_session(repo.lock_session()).unwrap();
            invalidate_embedding_projection(&repo).unwrap();
            assert_eq!(projection(&repo), expected);
            let replica = Aggregator::open(&aggregator_path(&control)).unwrap();
            assert_eq!(
                replica.scope_header(&scope).unwrap().unwrap().index_status,
                AggIndexStatus::Rebuilding
            );
        } else {
            let mut replica = Aggregator::open(&aggregator_path(&control)).unwrap();
            if state == "missing" {
                replica.retain_scopes(&BTreeSet::new()).unwrap();
                assert!(replica.scope_header(&scope).unwrap().is_none());
            } else {
                let mut header = replica.scope_header(&scope).unwrap().unwrap();
                match state {
                    "generation" => header.index_generation = original_generation.clone(),
                    "head" => header.current_snapshot_commit = Some(hash_bytes(b"stale head")),
                    "config" => {
                        header.current_chunking_config_hash = Some(hash_bytes(b"stale config"))
                    }
                    "healthy" => {}
                    _ => unreachable!(),
                }
                // A healthy no-op must leave this publication timestamp alone.
                replica.update_scope_header(&scope, &header, -1).unwrap();
            }
        }
        let source_stamp = replica_scope_stamp(repo.kio_dir()).unwrap();
        assert_eq!(
            embedding_replica_projection_is_current(&repo),
            state == "healthy"
        );
        let result = execute(
            &context,
            CommandRequest::Batch(BatchArgs {
                command: Some(BatchCommand::Resume(ResumeArgs {
                    recheck_budget: false,
                    override_budget: false,
                    online: false,
                    offline: true,
                    realtime: false,
                    batch: false,
                })),
            }),
        )
        .unwrap();

        let crate::response::ResponseBody::Batch(crate::response::batch::BatchResponse::Resume(
            result,
        )) = result.body
        else {
            panic!("expected Batch Resume response");
        };
        assert_eq!(result.counts.tasks_executed, 0, "{state}");
        assert_eq!(result.counts.tasks_attempted, 0, "{state}");
        assert_eq!(result.counts.tasks_updated, 0, "{state}");
        assert_eq!(result.counts.tasks_failed, 0, "{state}");
        assert!(embedding_replica_projection_is_current(&repo), "{state}");
        assert_eq!(
            replica_scope_stamp(repo.kio_dir()).unwrap(),
            source_stamp,
            "no source generation churn: {state}"
        );
        assert_eq!(projection(&repo), expected, "{state}");
        assert_eq!(replica_vectors(), expected.scalar, "{state}");
        assert_eq!(
            TaskStore::new(repo.kio_dir()).all().unwrap(),
            tasks,
            "no task retry or recharge: {state}"
        );
        if state == "healthy" {
            let refreshed_at: i64 = Connection::open(aggregator_path(&control))
                .unwrap()
                .query_row(
                    "SELECT refreshed_at FROM agg_scopes WHERE scope_id = ?1",
                    [&scope],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                refreshed_at, -1,
                "healthy replica must skip full publication"
            );
        }
    }
}

#[test]
fn new_chunk_associations_with_cached_contexts_complete_without_execution() {
    if !isolated_child_for(
        "contextual_embedding_tests::new_chunk_associations_with_cached_contexts_complete_without_execution",
    ) {
        return;
    }
    let control = ControlHandle::new(DebugTestControl::from_env());
    let root = private_tempdir();
    let context = AppContext {
        working_directory: root.path().to_path_buf(),
        interaction: Arc::new(QuietInteraction),
    };
    let document = |revision: &str| {
        format!(
            "# Lens\n\nThe lens uses a fixed focal length.\n\n# Timing\n\nThe clock uses a stable frequency.\n\n# Calibration\n\nThe reference star is {revision}.\n"
        )
    };
    fs::write(root.path().join("document.md"), document("Sirius")).unwrap();
    fs::write(root.path().join(OLD_PATH),
        "# Orchard\n\nThe orchard grows apples.\n\n# Soil\n\nThe soil contains clay.\n\n# Harvest\n\nThe harvest starts in autumn.\n",
    ).unwrap();
    execute(
        &context,
        CommandRequest::Init(InitArgs {
            path: None,
            independent: false,
            yes: false,
        }),
    )
    .unwrap();
    let index_result = || {
        execute(
            &context,
            CommandRequest::Index(IndexArgs {
                preview: false,
                yes: true,
                online: false,
                offline: true,
                realtime: false,
                batch: false,
            }),
        )
        .map(|response| match response.body {
            crate::response::ResponseBody::Index(
                crate::response::index::IndexResponse::Published(report),
            ) => report,
            _ => panic!("expected published Index response"),
        })
        .unwrap()
    };
    assert_eq!(index_result().embedding_tasks_executed, 6);
    let repo = open_existing_managed_root(root.path(), &control).unwrap();
    let profile = declared_embedding_profile(EmbeddingExecution::DeterministicEvaluator);

    for (revision, rename, executed) in [("Vega", false, 1), ("Rigel", true, 4)] {
        let before = projection(&repo);
        let cached_hashes = before
            .sources
            .iter()
            .map(|row| row.0.clone())
            .collect::<BTreeSet<_>>();
        let before_refs = TaskStore::new(repo.kio_dir())
            .all()
            .unwrap()
            .into_iter()
            .map(|task| task.output_ref)
            .collect::<BTreeSet<_>>();
        fs::write(root.path().join("document.md"), document(revision)).unwrap();
        if rename {
            fs::rename(root.path().join(OLD_PATH), root.path().join(CURRENT_PATH)).unwrap();
        }
        let result = index_result();
        assert_eq!(result.embedding_tasks_executed, executed);
        assert_eq!(result.embedding_tasks_failed, 0);
        assert_eq!(
            projection(&repo).sources.len(),
            before.sources.len() + executed
        );

        let head = repo.head_commit_hash().unwrap().unwrap();
        let retained = retained_history_instances(&repo, &head).unwrap();
        let config = snapshot_chunking_config_hash(&repo, &head).unwrap();
        let policy = current_embedding_policy(&repo).unwrap();
        let current =
            retained_history_chunks(&connection(&repo), &repo, &retained, &config, Some(&policy))
                .unwrap()
                .into_iter()
                .filter(|chunk| chunk.is_head_owner)
                .collect();
        let current = bind_embedding_work(current, &profile).unwrap();
        assert_eq!(current.len(), 6);
        let tasks = TaskStore::new(repo.kio_dir()).all().unwrap();
        let mut newly_accounted_cached = 0;
        for chunk in &current {
            let output_ref = chunk.work_key.output_ref();
            let matching = tasks
                .iter()
                .filter(|task| task.output_ref == output_ref)
                .collect::<Vec<_>>();
            assert_eq!(matching.len(), 1, "one task per contextual association");
            let task = matching[0];
            assert_eq!(task.status, TaskStatus::Done);
            if !before_refs.contains(&output_ref)
                && cached_hashes.contains(chunk.work_key.embedding_hash())
            {
                newly_accounted_cached += 1;
                assert_eq!(task.attempts, 0);
                assert!(
                    task.reservation_claim().is_none(),
                    "cached association must not reserve or recharge"
                );
                assert_eq!(
                    task.fallback_reason.as_deref(),
                    Some("embedding_adapter_done")
                );
                assert_eq!(task.input_hash, chunk.text_hash);
            }
        }
        assert_eq!(
            newly_accounted_cached, 2,
            "unchanged sections need fresh Done records only"
        );
        let materialized = projection(&repo);
        let noop = index_result();
        assert_eq!(noop.embedding_tasks_executed, 0);
        assert_eq!(projection(&repo), materialized);
        assert_eq!(TaskStore::new(repo.kio_dir()).all().unwrap(), tasks);
    }
}

#[test]
fn collector_missing_manifest_requires_retired_purge_for_every_introduction() {
    use crate::commands::PurgeArgs;

    if !isolated_child_for(
        "contextual_embedding_tests::collector_missing_manifest_requires_retired_purge_for_every_introduction",
    ) {
        return;
    }
    let control = ControlHandle::new(DebugTestControl::from_env());
    let root = private_tempdir();
    let context = AppContext {
        working_directory: root.path().to_path_buf(),
        interaction: Arc::new(QuietInteraction),
    };
    let document = "# Retained\n\nA purged document can be explicitly restored.\n";
    fs::write(root.path().join(CURRENT_PATH), document).unwrap();
    execute(
        &context,
        CommandRequest::Init(InitArgs {
            path: None,
            independent: false,
            yes: false,
        }),
    )
    .unwrap();
    index(&context);
    let repo = open_existing_managed_root(root.path(), &control).unwrap();
    let old_head = repo.head_commit_hash().unwrap().unwrap();
    let retained = retained_history_instances(&repo, &old_head).unwrap();
    assert_eq!(retained.len(), 1);
    let instance = &retained[0];
    // Test-only corruption targets the authenticated fixture repo's physical
    // CAS leaf; bound ObjectStore deliberately exposes no ambient content path.
    let manifest = cas_object_path(
        repo.kio_dir(),
        "manifests",
        &instance.normalize.manifest_hash,
    )
    .unwrap();
    let manifest_bytes = fs::read(&manifest).unwrap();
    let collect = |instances: &[RetainedNormalizedInstance]| {
        embedding_owners::collect_retained_embedding_chunks(
            &repo,
            &connection(&repo),
            instances,
            None,
            None,
        )
    };
    assert!(!collect(&retained).unwrap().is_empty());
    fs::remove_file(&manifest).unwrap();
    assert_eq!(
        collect(&retained).unwrap_err().error_code(),
        "KIO-E-STORE-NOT-FOUND-001",
        "an unexplained missing manifest must not be skipped"
    );
    assert_eq!(
        repo.object_store()
            .write_content_object(kio_core::cas::ContentObjectKind::Manifest, &manifest_bytes)
            .unwrap(),
        instance.normalize.manifest_hash,
    );

    fs::remove_file(root.path().join(CURRENT_PATH)).unwrap();
    execute(
        &context,
        CommandRequest::Purge(PurgeArgs {
            path: None,
            raw_hash: Some(instance.raw_hash.clone()),
            reason: "misingest".to_owned(),
            erase_tombstone: true,
            yes: true,
        }),
    )
    .unwrap();
    assert!(
        !manifest.exists(),
        "purge must remove the old immutable closure"
    );
    fs::write(root.path().join(CURRENT_PATH), document).unwrap();
    // Republish raw bytes and retire the receipt without creating replacement
    // normalized content that could conceal the old pinned manifest's absence.
    let resurrection = repo
        .auto_snapshot_with_bound_normalize(
            Some("resurrection without replacement manifest"),
            None,
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
        )
        .unwrap()
        .commit_hash
        .unwrap();
    let state = PurgeState::open(repo.kio_dir()).unwrap();
    let receipt = state
        .read_erase_receipt(&instance.raw_hash)
        .unwrap()
        .unwrap();
    assert_eq!(receipt.tail().kind, EventKind::Retired);
    assert!(!purge_blocks_rebuild_raw(repo.kio_dir(), &instance.raw_hash, repo.control()).unwrap());
    assert!(
        collect(&retained).unwrap().is_empty(),
        "the validated old owner may be skipped"
    );

    // Caller-supplied introduction metadata cannot replace the committed
    // history used to classify a missing immutable closure.
    let mut wrong_introduction = retained.clone();
    wrong_introduction[0].introductions.push(resurrection);
    assert!(
        collect(&wrong_introduction).unwrap().is_empty(),
        "forged caller introductions cannot override the committed history"
    );

    // Actually publish the same missing NormalizeRef after resurrection. The
    // new retained tree makes this a post-purge appearance, which the retired
    // receipt cannot explain even though the original owner remains old.
    let pending = BTreeMap::from([(
        CURRENT_PATH.to_owned(),
        PendingNormalizeRef {
            expected_raw_hash: instance.raw_hash.clone(),
            normalize: instance.normalize.clone(),
        },
    )]);
    let post_purge = repo
        .auto_snapshot_with_bound_normalize(
            Some("post-purge normalized binding"),
            None,
            &BTreeSet::new(),
            &pending,
            &BTreeSet::new(),
            None,
        )
        .unwrap();
    assert!(!post_purge.noop);
    let latest = post_purge.commit_hash.unwrap();
    let latest_commit = repo.read_commit(&latest).unwrap();
    let latest_tree = repo.read_tree(&latest_commit.tree).unwrap();
    assert_eq!(
        latest_tree.entries[0].normalize,
        Some(instance.normalize.clone())
    );
    let post_purge_retained = retained_history_instances(&repo, &latest).unwrap();
    assert_eq!(
        collect(&post_purge_retained).unwrap_err().error_code(),
        "KIO-E-STORE-NOT-FOUND-001",
        "an actual post-purge binding must prevent the exception"
    );

    // A valid retired receipt explains absence, never corrupt bytes at the
    // immutable object address. It must not turn malformed content into a skip.
    fs::create_dir_all(manifest.parent().unwrap()).unwrap();
    fs::write(&manifest, b"not an authenticated manifest").unwrap();
    let corrupt = collect(&retained).unwrap_err();
    assert_ne!(corrupt.error_code(), "KIO-E-STORE-NOT-FOUND-001");
}

#[test]
fn batch_result_admission_validates_all_keys_and_vectors_before_returning_any() {
    use kio_adapter::gemini_batch_client::GeminiBatchEmbedOutput;
    use kio_adapter::types::EmbeddingLogicalJobId;
    use kio_pipeline::ledger::embedding_job::{
        EmbeddingJob, EmbeddingJobBinding, EmbeddingJobMember,
    };
    use kio_pipeline::task::EmbeddingTaskTarget;
    let first = hash_bytes(b"first contextual input");
    let second = hash_bytes(b"second contextual input");
    let input_hash = embedding_job_input_hash(&BTreeSet::from([first.clone(), second.clone()]));
    let profile_hash = hash_bytes(b"profile");
    let job = EmbeddingJob {
        key: kio_pipeline::ledger::TaskKey::new(
            "scope_test",
            "embedding",
            input_hash.clone(),
            profile_hash.clone(),
        ),
        submission_seq: 1,
        intent_token: "test_token".to_owned(),
        binding: EmbeddingJobBinding {
            logical_job_id: EmbeddingLogicalJobId(input_hash),
            dimensions: 3,
            distance: "cosine".to_owned(),
            modality: "multimodal".to_owned(),
            members: [first.clone(), second.clone()]
                .into_iter()
                .map(|id| EmbeddingJobMember {
                    target: EmbeddingTaskTarget::Chunk {
                        chunk_hash: id.clone(),
                        embedding_hash: id.clone(),
                        profile_hash: profile_hash.clone(),
                        input_hash: id.clone(),
                    },
                    id,
                })
                .collect(),
        },
        known_result: None,
    };
    let output = |key: &str| GeminiBatchEmbedOutput {
        key: key.to_owned(),
        values: Some(vec![2.0, 0.0, 0.0]),
        error: None,
        usage: kio_adapter::types::EmbeddingUsage::Metered {
            usage: kio_adapter::types::AdapterUsage::BillableUnits {
                billable_units: vec![kio_adapter::types::BillableUnit {
                    kind: kio_adapter::types::BillableUnitKind::TokensIn,
                    count: 1,
                }],
            },
        },
    };
    let valid = vec![output(&first), output(&second)];
    let accepted = completed_batch_result(&job, &valid).unwrap();
    assert_eq!(accepted.members.len(), 2);
    assert_eq!(accepted.members[0].id, first);
    assert!(
        matches!(&accepted.members[0].result, kio_adapter::types::EmbeddingMemberResult::Vector { vector } if vector == &vec![1.0, 0.0, 0.0])
    );
    let mut reversed = valid.clone();
    reversed.reverse();
    let accepted_by_id = accepted
        .members
        .iter()
        .map(|member| (member.id.clone(), member.result.clone()))
        .collect::<BTreeMap<_, _>>();
    let reversed_by_id = completed_batch_result(&job, &reversed)
        .unwrap()
        .members
        .into_iter()
        .map(|member| (member.id, member.result))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(reversed_by_id, accepted_by_id);

    let mut malformed = vec![
        vec![],
        vec![output(&first)],
        vec![output(&first), output(&first)],
        vec![output(&first), output(&hash_bytes(b"foreign"))],
        vec![output(&first), output("not-canonical")],
    ];
    // An absent result without an explicit error is an ambiguous envelope.
    // Keyed vector validation failures are isolated by the O3 matrix below.
    let mut absent_result = valid.clone();
    absent_result[1].values = None;
    malformed.push(absent_result);
    let mut conflicting_error = valid.clone();
    conflicting_error[1].error = Some(json!({"code": "invalid"}));
    malformed.push(conflicting_error);
    for results in malformed {
        assert!(
            completed_batch_result(&job, &results).is_err(),
            "must not return the valid first line when a later line fails: {results:?}"
        );
    }
    let mut failed_member = valid;
    failed_member[1].values = None;
    failed_member[1].error = Some(json!({"code": "invalid"}));
    let partial = completed_batch_result(&job, &failed_member).unwrap();
    assert!(matches!(
        partial.members[1].result,
        kio_adapter::types::EmbeddingMemberResult::Failed { .. }
    ));
}

// O3 typed acceptance tests: these call the production Batch converter. They
// are not HTTP, durable accounting, publication, or retry-scheduling evidence.
fn o3_batch_acceptance_fixture(
    image: bool,
) -> (
    kio_pipeline::ledger::embedding_job::EmbeddingJob,
    Vec<kio_adapter::gemini_batch_client::GeminiBatchEmbedOutput>,
) {
    use kio_adapter::types::{BillableUnitKind, EmbeddingLogicalJobId};
    use kio_pipeline::ledger::embedding_job::{
        EmbeddingJob, EmbeddingJobBinding, EmbeddingJobMember,
    };
    use kio_pipeline::task::EmbeddingTaskTarget;
    // An image job has one member; text may batch. Synthetic canonical target
    // hashes suffice at this typed result boundary; no source owner is forged.
    let ids = if image {
        vec![hash_bytes(b"original PNG member")]
    } else {
        vec![hash_bytes(b"text member A"), hash_bytes(b"text member B")]
    };
    let input_hash = embedding_job_input_hash(&ids.iter().cloned().collect());
    let profile_hash = hash_bytes(b"O3 acceptance profile");
    let members = ids
        .iter()
        .map(|id| EmbeddingJobMember {
            id: id.clone(),
            target: if image {
                EmbeddingTaskTarget::Image {
                    image_hash: id.clone(),
                    embedding_hash: id.clone(),
                    profile_hash: profile_hash.clone(),
                    input_hash: id.clone(),
                    image_input_receipt: None,
                }
            } else {
                EmbeddingTaskTarget::Chunk {
                    chunk_hash: id.clone(),
                    embedding_hash: id.clone(),
                    profile_hash: profile_hash.clone(),
                    input_hash: id.clone(),
                }
            },
        })
        .collect();
    let outputs = ids
        .into_iter()
        .map(|key| {
            let mut vector = vec![0.0; 768];
            vector[0] = 1.0;
            kio_adapter::gemini_batch_client::GeminiBatchEmbedOutput {
                key,
                values: Some(vector),
                error: None,
                usage: o3_member_usage(
                    if image {
                        BillableUnitKind::ImageTokensIn
                    } else {
                        BillableUnitKind::TokensIn
                    },
                    7,
                ),
            }
        })
        .collect();
    (
        EmbeddingJob {
            key: kio_pipeline::ledger::TaskKey::new(
                "scope_test",
                "embedding",
                input_hash.clone(),
                profile_hash,
            ),
            submission_seq: 1,
            // No ledger API is called by this typed acceptance fixture.
            intent_token: "typed-boundary-only".to_owned(),
            binding: EmbeddingJobBinding {
                logical_job_id: EmbeddingLogicalJobId(input_hash),
                dimensions: 768,
                distance: "cosine".to_owned(),
                modality: "multimodal".to_owned(),
                members,
            },
            known_result: None,
        },
        outputs,
    )
}

fn o3_member_usage(
    kind: kio_adapter::types::BillableUnitKind,
    count: u64,
) -> kio_adapter::types::EmbeddingUsage {
    use kio_adapter::types::{AdapterUsage, BillableUnit, EmbeddingUsage};
    EmbeddingUsage::Metered {
        usage: AdapterUsage::BillableUnits {
            billable_units: vec![BillableUnit { kind, count }],
        },
    }
}

#[test]
fn o3_batch_keyed_invalid_vector_preserves_valid_sibling_and_usage() {
    use kio_adapter::types::{BillableUnitKind, EmbeddingMemberResult};
    let (job, valid) = o3_batch_acceptance_fixture(false);
    let mut nan = vec![0.0; 768];
    nan[0] = f32::NAN;
    let mut positive_infinity = vec![0.0; 768];
    positive_infinity[0] = f32::INFINITY;
    let mut negative_infinity = vec![0.0; 768];
    negative_infinity[0] = f32::NEG_INFINITY;
    for (label, invalid) in [
        ("wrong_width", vec![1.0; 767]),
        ("zero_norm", vec![0.0; 768]),
        ("nan", nan),
        ("positive_infinity", positive_infinity),
        ("negative_infinity", negative_infinity),
    ] {
        for reverse in [false, true] {
            let mut outputs = valid.clone();
            outputs[1].values = Some(invalid.clone());
            if reverse {
                outputs.reverse();
            }
            // Intended O3 Runtime Red on O2: a bad keyed vector currently
            // returns Err and discards its independently valid sibling.
            let completed = completed_batch_result(&job, &outputs)
                .unwrap_or_else(|error| panic!("{label}, reverse={reverse}: {error}"));
            assert_eq!(completed.members.len(), 2);
            let by_id = completed
                .members
                .iter()
                .map(|member| (member.id.as_str(), &member.result))
                .collect::<BTreeMap<_, _>>();
            let EmbeddingMemberResult::Vector { vector } = by_id[valid[0].key.as_str()] else {
                panic!("{label}: successful sibling was discarded");
            };
            let expected = valid[0].values.as_ref().unwrap();
            assert_eq!(
                vector
                    .iter()
                    .map(|value| value.to_bits())
                    .collect::<Vec<_>>(),
                expected
                    .iter()
                    .map(|value| value.to_bits())
                    .collect::<Vec<_>>(),
                "{label}: successful f32 bits must survive neighbor failure",
            );
            assert!(
                matches!(
                    by_id[valid[1].key.as_str()],
                    EmbeddingMemberResult::Failed { reason } if reason == "contract_violation"
                ),
                "{label}: only the invalid keyed member fails"
            );
            // Reported provider usage remains independent of vector validity.
            assert_eq!(
                completed.usage,
                o3_member_usage(BillableUnitKind::TokensIn, 14)
            );
        }
    }
}

#[test]
fn o3_batch_usage_is_modality_complete_and_does_not_discard_valid_vectors() {
    use kio_adapter::types::{AdapterUsage, BillableUnit, BillableUnitKind, EmbeddingUsage};
    for image in [false, true] {
        let (job, valid) = o3_batch_acceptance_fixture(image);
        let baseline = completed_batch_result(&job, &valid).unwrap();
        let correct = if image {
            BillableUnitKind::ImageTokensIn
        } else {
            BillableUnitKind::TokensIn
        };
        let wrong = if image {
            BillableUnitKind::TokensIn
        } else {
            BillableUnitKind::ImageTokensIn
        };
        assert_eq!(
            baseline.usage,
            o3_member_usage(correct, 7 * valid.len() as u64)
        );
        for (label, usage) in [
            ("missing", EmbeddingUsage::Estimated),
            ("unmetered_online", EmbeddingUsage::Unmetered),
            ("wrong_modality", o3_member_usage(wrong, 7)),
            ("over_bound", o3_member_usage(correct, 8193)),
            (
                "empty_units",
                EmbeddingUsage::Metered {
                    usage: AdapterUsage::BillableUnits {
                        billable_units: vec![],
                    },
                },
            ),
            (
                "duplicate_units",
                EmbeddingUsage::Metered {
                    usage: AdapterUsage::BillableUnits {
                        billable_units: vec![
                            BillableUnit {
                                kind: correct,
                                count: 3,
                            },
                            BillableUnit {
                                kind: correct,
                                count: 4,
                            },
                        ],
                    },
                },
            ),
            (
                "mixed_units",
                EmbeddingUsage::Metered {
                    usage: AdapterUsage::BillableUnits {
                        billable_units: vec![
                            BillableUnit {
                                kind: correct,
                                count: 7,
                            },
                            BillableUnit {
                                kind: wrong,
                                count: 1,
                            },
                        ],
                    },
                },
            ),
        ] {
            let mut outputs = valid.clone();
            outputs.last_mut().unwrap().usage = usage;
            let completed = completed_batch_result(&job, &outputs).unwrap();
            assert_eq!(
                completed.members, baseline.members,
                "image={image}, {label}"
            );
            assert_eq!(
                completed.usage,
                EmbeddingUsage::Estimated,
                "image={image}, {label}"
            );
        }
    }
}

#[test]
fn o3_batch_explicit_member_errors_preserve_sibling_and_classify_rejection() {
    use kio_adapter::types::EmbeddingMemberResult;
    let (job, valid) = o3_batch_acceptance_fixture(false);
    for (error, reason) in [
        (json!({"code": 401}), "auth_error"),
        (json!({"code": 403}), "auth_error"),
        (json!({"code": 429}), "rate_limit"),
        (json!({"code": 503}), "network_error"),
        (json!({"code": 400}), "invalid_input"),
        (json!({"code": "unknown"}), "contract_violation"),
        (json!({}), "contract_violation"),
    ] {
        let mut outputs = valid.clone();
        outputs[1].values = None;
        outputs[1].error = Some(error.clone());
        let completed = completed_batch_result(&job, &outputs).unwrap();
        assert!(matches!(
            &completed.members[0].result,
            EmbeddingMemberResult::Vector { .. }
        ));
        assert!(
            matches!(
                &completed.members[1].result,
                EmbeddingMemberResult::Failed { reason: actual } if actual == reason
            ),
            "{error}: {completed:?}"
        );
    }
    // Classification is not retry scheduling: this response type has no HTTP
    // Retry-After, attempt counter, or durable result-unknown authority.
}

#[test]
fn contextual_task_mixed_outcomes_crash_recovery_and_retry_are_independent() {
    if !isolated_child_for(
        "contextual_embedding_tests::contextual_task_mixed_outcomes_crash_recovery_and_retry_are_independent",
    ) {
        return;
    }
    let control = ControlHandle::new(DebugTestControl::from_env());
    let root = private_tempdir();
    let repo = match initialize_explicit_root(root.path(), &control).unwrap() {
        ExplicitRoot::Created(repo) => repo,
        ExplicitRoot::Existing(_) => panic!("expected new fixture"),
    };
    let profile = declared_embedding_profile(EmbeddingExecution::DeterministicEvaluator);
    let text = "same retained chunk body";
    let candidates = [OLD_PATH, CURRENT_PATH]
        .into_iter()
        .map(|path| RetainedEmbeddingChunk {
            chunk_id: hash_bytes(b"chunk identity"),
            text: text.into(),
            text_hash: hash_bytes(text.as_bytes()),
            raw_path: path.to_owned(),
            requires_secret_approval: false,
            is_head_owner: path == CURRENT_PATH,
        })
        .collect();
    let work = bind_embedding_work(candidates, &profile).unwrap();
    assert_eq!(work.len(), 2);
    assert_ne!(work[0].work_key, work[1].work_key);
    let store = TaskStore::new(repo.kio_dir());
    let now = "2026-09-26T00:00:00Z";
    enqueue_embedding_tasks(&store, &repo, &work, &profile, true, now).unwrap();
    let mut transitions = BTreeMap::new();
    record_embedding_transitions(&mut transitions, [&work[0]], embedding_done_transition());
    record_embedding_transitions(
        &mut transitions,
        [&work[1]],
        embedding_fail_transition(RetryErrorKind::NetworkError),
    );
    assert_eq!(
        apply_embedding_transitions(&store, &transitions, now, &BTreeMap::new()).unwrap(),
        1
    );
    let tasks = store.all().unwrap();
    let first_ref = work[0].work_key.output_ref();
    let second_ref = work[1].work_key.output_ref();
    assert_eq!(
        tasks
            .iter()
            .find(|task| task.output_ref == first_ref)
            .unwrap()
            .status,
        TaskStatus::Done
    );
    let failed = tasks
        .iter()
        .find(|task| task.output_ref == second_ref)
        .unwrap()
        .clone();
    assert_eq!(failed.status, TaskStatus::Failed);
    assert_eq!(failed.attempts, 1);

    store
        .update_matching(|task| {
            if task.output_ref == first_ref {
                task.status = TaskStatus::Running;
                true
            } else {
                false
            }
        })
        .unwrap();
    let active = work
        .iter()
        .map(|chunk| chunk.work_key.output_ref())
        .collect();
    let hashes = work
        .iter()
        .map(|chunk| {
            (
                chunk.work_key.output_ref(),
                chunk.work_key.embedding_hash().to_owned(),
            )
        })
        .collect();
    reconcile_committed_embedding_tasks(
        &repo,
        &store,
        None,
        EmbeddingReconcileContext {
            active_work_refs: &active,
            active_embedding_hashes_by_work_ref: &hashes,
            pending: &work[1..],
            now,
            profile_hash: &profile.profile_hash,
            allow_auth_revive: false,
        },
    )
    .unwrap();
    let tasks = store.all().unwrap();
    assert_eq!(
        tasks
            .iter()
            .find(|task| task.output_ref == first_ref)
            .unwrap()
            .status,
        TaskStatus::Done
    );
    assert_eq!(
        tasks
            .iter()
            .find(|task| task.output_ref == second_ref)
            .unwrap(),
        &failed
    );
    store
        .update_matching(|task| {
            if task.output_ref == second_ref {
                task.next_retry_at = None;
                true
            } else {
                false
            }
        })
        .unwrap();
    let retry =
        filter_embeddable_by_task_state(&store, work[1..].to_vec(), &profile, false, &control)
            .unwrap();
    assert_eq!(retry.len(), 1);
    assert_eq!(retry[0].work_key, work[1].work_key);

    store
        .update_matching(|task| {
            if task.output_ref == second_ref {
                task.input_hash = hash_bytes(b"forged body");
                true
            } else {
                false
            }
        })
        .unwrap();
    let before = fs::read(repo.kio_dir().join("tasks.jsonl")).unwrap();
    assert!(validate_embedding_task_bindings(&store, &work, &profile).is_err());
    assert!(
        filter_embeddable_by_task_state(&store, work.clone(), &profile, false, &control).is_err()
    );
    assert!(enqueue_embedding_tasks(&store, &repo, &work, &profile, true, now).is_err());
    assert_eq!(
        fs::read(repo.kio_dir().join("tasks.jsonl")).unwrap(),
        before
    );
}

#[test]
fn persisted_embedding_consent_requires_the_exact_builtin_identity() {
    let adapter_settings =
        &std::sync::Arc::new(kio_adapter::tool_lock::AdapterRuntimeSettings::default());

    use kio_adapter::catalog::AdoptedEmbeddingExecution;
    use kio_adapter::tool_lock::{EmbeddingToolLockEntry, ToolLock};
    if !isolated_child_for(
        "contextual_embedding_tests::persisted_embedding_consent_requires_the_exact_builtin_identity",
    ) {
        return;
    }
    let control = ControlHandle::new(DebugTestControl::from_env());
    assert!(matches!(
        embedding_execution(adapter_settings, &control),
        Some(EmbeddingExecution::DeterministicEvaluator)
    ));
    let root = private_tempdir();
    let repo = match initialize_explicit_root(root.path(), &control).unwrap() {
        ExplicitRoot::Created(repo) => repo,
        ExplicitRoot::Existing(_) => panic!("expected new fixture"),
    };
    let entry_for = |profile: DeclaredEmbeddingProfile, kind| EmbeddingToolLockEntry {
        tool_id: profile.tool_id,
        dimensions: profile.dimensions.try_into().unwrap(),
        distance: profile.distance,
        modality: profile.modality,
        profile_hash: profile.profile_hash,
        kind,
        mode: None,
    };
    let builtin = entry_for(
        declared_embedding_profile(EmbeddingExecution::DeterministicEvaluator),
        Some(ExecutionMode::DeterministicLibrary),
    );
    let external = entry_for(
        declared_embedding_profile(EmbeddingExecution::Online(AdoptedEmbeddingExecution::Real)),
        Some(ExecutionMode::OnlineApi),
    );
    assert_ne!(external.tool_id, builtin.tool_id);
    assert_ne!(external.profile_hash, builtin.profile_hash);
    let mut forged = external.clone();
    forged.kind = Some(ExecutionMode::DeterministicLibrary);
    let mut missing_kind = builtin.clone();
    missing_kind.kind = None;
    let lock_path = repo.kio_dir().join("tool-lock.json");
    for (entry, exempt) in [
        (builtin.clone(), true),
        (external.clone(), false),
        (forged, false),
        (missing_kind, false),
    ] {
        let lock = ToolLock {
            spec_version: 1,
            prepare: None,
            markdown: None,
            embedding: Some(entry.clone()),
        };
        let bytes = serde_json::to_vec(&lock).unwrap();
        fs::write(&lock_path, &bytes).unwrap();
        let persisted = rebuild_embedding_entry_from_tool_lock(repo.kio_dir())
            .unwrap()
            .unwrap();
        assert_eq!(persisted, entry);
        assert_eq!(persisted_embedding_is_deterministic(&persisted), exempt);
        assert_eq!(
            embedding_profile_summary(&persisted).profile_hash,
            entry.profile_hash,
            "the active evaluator must not replace the persisted recipient"
        );
        if !exempt {
            assert!(
                !approvals::allowed(
                    adapter_settings,
                    &repo,
                    approvals::AdapterRole::Embedding,
                    &persisted.tool_id,
                    &persisted.profile_hash,
                    grants::GrantOperation::SendSecrets,
                )
                .unwrap()
            );
        }
        assert_eq!(fs::read(&lock_path).unwrap(), bytes);
        assert!(repo.read_network_approvals().unwrap().is_empty());
    }
    for field in ["tool", "profile", "dimensions", "distance", "modality"] {
        let mut changed = builtin.clone();
        match field {
            "tool" => changed.tool_id = external.tool_id.clone(),
            "profile" => changed.profile_hash = external.profile_hash.clone(),
            "dimensions" => changed.dimensions += 1,
            "distance" => changed.distance = "euclidean".to_owned(),
            "modality" => changed.modality = "text".to_owned(),
            _ => unreachable!(),
        }
        assert!(
            !persisted_embedding_is_deterministic(&changed),
            "{field} is part of the exact built-in identity"
        );
    }
}

// O3 local-claim regression: below the scope writer lock, not two CLI writers.
#[test]
fn o3_fresh_local_image_claim_survives_competing_enrichment() {
    use kio_core::scope::LockSession;
    use kio_core::test_control::TestEnvGuard;
    use kio_pipeline::ledger::TaskKey;
    use kio_pipeline::ledger::embedding_job::EmbeddingJobMember;
    use kio_pipeline::ledger::ops::{cost_ledger_rows_for_key, get_batch_request};
    use std::sync::mpsc;

    if !isolated_child_for(
        "contextual_embedding_tests::o3_fresh_local_image_claim_survives_competing_enrichment",
    ) {
        return;
    }
    let control = ControlHandle::new(DebugTestControl::from_env());
    let settings = Arc::new(kio_adapter::tool_lock::AdapterRuntimeSettings::default());
    let root = private_tempdir();
    let context = AppContext {
        working_directory: root.path().to_path_buf(),
        interaction: Arc::new(QuietInteraction),
    };
    // Complete, CRC-valid 1x1 RGBA PNG, filter0 + ffffffff. An unknown
    // evaluator selector disables embedding only while native source ownership
    // is indexed; it does not fabricate a CAS owner or remove an existing result.
    let png: [u8; 68] = [
        0x89, b'P', b'N', b'G', 13, 10, 26, 10, 0, 0, 0, 13, b'I', b'H', b'D', b'R', 0, 0, 0, 1, 0,
        0, 0, 1, 8, 6, 0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 11, b'I', b'D', b'A', b'T', 120, 156,
        99, 248, 15, 4, 0, 9, 251, 3, 253, 251, 94, 107, 43, 0, 0, 0, 0, b'I', b'E', b'N', b'D',
        174, 66, 96, 130,
    ];
    fs::write(root.path().join("claim.png"), png).unwrap();
    {
        let _disabled = TestEnvGuard::set("KIO_EVAL_DETERMINISTIC_EMBED", "fixture-disabled");
        execute(
            &context,
            CommandRequest::Init(InitArgs {
                path: None,
                independent: false,
                yes: false,
            }),
        )
        .unwrap();
        index(&context);
    }
    let execution = embedding_execution(&settings, &control).unwrap();
    assert_eq!(execution, EmbeddingExecution::DeterministicEvaluator);
    let profile = declared_embedding_profile(execution);
    assert_eq!(profile.dimensions, 768);
    let locks = LockSession::default();
    let repo = Repository::open_in_session(root.path(), control.clone(), &locks).unwrap();
    // The whole below-lock operation has real production writer authority.
    // The reservation owner and competing sub-operation share that authority;
    // no second public writer CLI is claimed to bypass the lock.
    let _writer = repo.lock_store_in_session(&locks).unwrap();
    let image_hash = hash_bytes(&png);
    let (plan, _) = crate::embedding::input::prepare_owned_embedding_image(
        &repo.object_store(),
        &image_hash,
        &profile,
    )
    .unwrap();
    let target = plan.target.clone();
    let kio_pipeline::task::EmbeddingTaskTarget::Image { embedding_hash, .. } = &target else {
        panic!("owned image fixture must produce an image target");
    };
    let embedding_hash = embedding_hash.clone();
    assert!(repo.object_store().read_embedding(&embedding_hash).is_err());
    let scope_id = repo.scope_identity().unwrap().scope_id;
    let key = TaskKey::new(
        &scope_id,
        "embedding",
        &embedding_hash,
        &profile.profile_hash,
    );
    let identity = crate::embedding::transport::local_embedding_identity(
        crate::local_execution::LocalTarget::Image {
            scope_id,
            target: embedding_hash.clone(),
        },
        embedding_hash.clone(),
        &profile,
        execution,
        vec![EmbeddingJobMember {
            id: image_hash,
            target,
        }],
    )
    .unwrap();
    let ledger = crate::ledger_access::open_optional_ledger_db(&control)
        .unwrap()
        .unwrap();
    assert!(get_batch_request(&ledger, &key).unwrap().is_none());
    assert!(cost_ledger_rows_for_key(&ledger, &key).unwrap().is_empty());
    let owner_control = control.clone();
    let owner_identity = identity.clone();
    let (ready_tx, ready_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::sync_channel(0);
    let owner = std::thread::spawn(move || {
        let store = crate::local_execution::LocalExecution::new(
            crate::local_execution::LocalExecution::default_root(&owner_control).unwrap(),
            owner_control.clone(),
        )
        .unwrap();
        let crate::local_execution::Claim::Claimed(guard) = store
            .claim(
                &owner_identity,
                0,
                None,
                crate::embedding::transport::local_max_attempts(execution),
                owner_control.now_millis(),
            )
            .unwrap()
        else {
            panic!("fixture must own actual free image execution")
        };
        let before = (
            guard.token().to_owned(),
            guard.attempts(),
            guard.result().unwrap(),
        );
        assert_eq!(before.1, 1);
        assert!(before.2.is_none());
        ready_tx.send((owner_identity, before.clone())).unwrap();
        release_rx.recv().unwrap();
        (
            guard.token().to_owned(),
            guard.attempts(),
            guard.result().unwrap(),
        )
    });
    let (owned_identity, before) = ready_rx.recv().unwrap();
    assert_eq!(
        owned_identity, identity,
        "live owner uses actual image plan/profile identity"
    );
    let competing = crate::embedding::executor::run_embedding_enrichment(
        &crate::invocation::Invocation {
            context: &context,
            settings: &settings,
            control: &control,
            locks: &locks,
        },
        &repo,
        false,
        false,
        false,
    );
    // Always release and join the real owner before checking the competitor.
    release_tx.send(()).unwrap();
    assert_eq!(
        owner.join().unwrap(),
        before,
        "competitor must preserve live owner token/attempt/result"
    );
    let outcome = competing.expect("competing image runner must return normally");
    assert_eq!(
        outcome.inflight, 1,
        "exact owned image claim must be observed busy"
    );
    assert!(
        repo.object_store().read_embedding(&embedding_hash).is_err(),
        "competitor cannot publish live owner's image"
    );
    assert!(
        get_batch_request(&ledger, &key).unwrap().is_none(),
        "free image cannot create SQL reservation"
    );
    assert!(
        cost_ledger_rows_for_key(&ledger, &key).unwrap().is_empty(),
        "free image cannot settle SQL cost"
    );
}
