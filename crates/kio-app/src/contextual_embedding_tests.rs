//! Product regression for embedding a retained chunk under multiple filenames.

use super::*;
use crate::commands::{CommandRequest, InitArgs, RepairArgs, RepairOperation, RepairRebuildDbArgs};
use crate::context::{AppContext, Interaction};
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
}

fn private_tempdir() -> tempfile::TempDir {
    let parent = std::env::temp_dir().canonicalize().unwrap();
    let root = tempfile::tempdir_in(parent).unwrap();
    let directory = kio_core::store_dir::StoreDirectory::open(root.path()).unwrap();
    kio_core::store_dir::restrict_new_private_directory(&directory.root_handle()).unwrap();
    root
}

pub(super) fn isolated_child_for(test_name: &str) -> bool {
    if std::env::var_os(CHILD).is_some() {
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
    let retained = retained_history_instances(repo.kio_dir(), &head).unwrap();
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
    let retained = retained_history_instances(repo.kio_dir(), &head).unwrap();
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
    if !isolated_child_for(
        "contextual_embedding_tests::rename_retains_contexts_and_rebuild_selects_allowed_head_then_history",
    ) {
        return;
    }
    // execute() installs command-scoped adapter runtime settings itself; the
    // deterministic adapter is selected directly by its release-capable env
    // resolver and requires no CLI-only bootstrap.
    assert!(matches!(
        active_embedding_execution(),
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
    execute(&context, CommandRequest::Init(InitArgs { path: None })).unwrap();
    index(&context);
    let repo = open_existing_managed_root(root.path()).unwrap();
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
        assert_eq!(&object.context, stored_context);
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
    execute(&context, CommandRequest::Init(InitArgs { path: None })).unwrap();
    index(&context);
    let repo = open_existing_managed_root(root.path()).unwrap();
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
        Connection::open(aggregator_path())
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
            let _lock = repo.lock_store().unwrap();
            invalidate_embedding_projection(&repo).unwrap();
            assert_eq!(projection(&repo), expected);
            let replica = Aggregator::open(&aggregator_path()).unwrap();
            assert_eq!(
                replica.scope_header(&scope).unwrap().unwrap().index_status,
                AggIndexStatus::Rebuilding
            );
        } else {
            let mut replica = Aggregator::open(&aggregator_path()).unwrap();
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
        assert_eq!(result["tasks_executed"], 0, "{state}");
        assert_eq!(result["tasks_attempted"], 0, "{state}");
        assert_eq!(result["tasks_updated"], 0, "{state}");
        assert_eq!(result["tasks_failed"], 0, "{state}");
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
            let refreshed_at: i64 = Connection::open(aggregator_path())
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
    execute(&context, CommandRequest::Init(InitArgs { path: None })).unwrap();
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
        .unwrap()
    };
    assert_eq!(index_result()["embedding_tasks_executed"], 6);
    let repo = open_existing_managed_root(root.path()).unwrap();
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
        assert_eq!(result["embedding_tasks_executed"], executed);
        assert_eq!(result["embedding_tasks_failed"], 0);
        assert_eq!(
            projection(&repo).sources.len(),
            before.sources.len() + executed as usize
        );

        let head = repo.head_commit_hash().unwrap().unwrap();
        let retained = retained_history_instances(repo.kio_dir(), &head).unwrap();
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
        assert_eq!(noop["embedding_tasks_executed"], 0);
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
    let root = private_tempdir();
    let context = AppContext {
        working_directory: root.path().to_path_buf(),
        interaction: Arc::new(QuietInteraction),
    };
    let document = "# Retained\n\nA purged document can be explicitly restored.\n";
    fs::write(root.path().join(CURRENT_PATH), document).unwrap();
    execute(&context, CommandRequest::Init(InitArgs { path: None })).unwrap();
    index(&context);
    let repo = open_existing_managed_root(root.path()).unwrap();
    let old_head = repo.head_commit_hash().unwrap().unwrap();
    let retained = retained_history_instances(repo.kio_dir(), &old_head).unwrap();
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
    fs::write(&manifest, &manifest_bytes).unwrap();

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
    assert!(!purge_blocks_rebuild_raw(repo.kio_dir(), &instance.raw_hash).unwrap());
    assert!(
        collect(&retained).unwrap().is_empty(),
        "the validated old owner may be skipped"
    );

    let mut wrong_introduction = retained.clone();
    wrong_introduction[0].introductions.push(resurrection);
    assert_eq!(
        collect(&wrong_introduction).unwrap_err().error_code(),
        "KIO-E-STORE-NOT-FOUND-001",
        "even one post-purge introduction must prevent the exception"
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
    let first = hash_bytes(b"first contextual input");
    let second = hash_bytes(b"second contextual input");
    let input_hash = embedding_job_input_hash(&BTreeSet::from([first.clone(), second.clone()]));
    let profile = DeclaredEmbeddingProfile {
        tool_id: "test".to_owned(),
        dimensions: 3,
        distance: "cosine".to_owned(),
        modality: "multimodal".to_owned(),
        profile_hash: hash_bytes(b"profile"),
    };
    let output = |key: &str| GeminiBatchEmbedOutput {
        key: key.to_owned(),
        values: Some(vec![2.0, 0.0, 0.0]),
        error: None,
        prompt_tokens: Some(1),
    };
    let valid = vec![output(&first), output(&second)];
    let accepted = validated_embedding_batch_vectors(&valid, &input_hash, Some(&profile)).unwrap();
    assert_eq!(accepted.len(), 2);
    assert_eq!(accepted[&first], vec![1.0, 0.0, 0.0]);
    let mut reversed = valid.clone();
    reversed.reverse();
    assert_eq!(
        validated_embedding_batch_vectors(&reversed, &input_hash, Some(&profile)).unwrap(),
        accepted
    );
    assert!(
        validated_embedding_batch_vectors(&valid, &input_hash, None)
            .unwrap()
            .is_empty()
    );

    let mut malformed = vec![
        vec![],
        vec![output(&first)],
        vec![output(&first), output(&first)],
        vec![output(&first), output(&hash_bytes(b"foreign"))],
        vec![output(&first), output("not-canonical")],
    ];
    for values in [
        None,
        Some(vec![1.0]),
        Some(vec![0.0; 3]),
        Some(vec![f32::NAN, 0.0, 1.0]),
    ] {
        let mut results = valid.clone();
        results[1].values = values;
        malformed.push(results);
    }
    let mut conflicting_error = valid.clone();
    conflicting_error[1].error = Some(json!({"code": "invalid"}));
    malformed.push(conflicting_error);
    for results in malformed {
        assert!(
            validated_embedding_batch_vectors(&results, &input_hash, Some(&profile)).is_err(),
            "must not return the valid first line when a later line fails: {results:?}"
        );
    }
}

#[test]
fn contextual_task_mixed_outcomes_crash_recovery_and_retry_are_independent() {
    if !isolated_child_for(
        "contextual_embedding_tests::contextual_task_mixed_outcomes_crash_recovery_and_retry_are_independent",
    ) {
        return;
    }
    let root = private_tempdir();
    let repo = match initialize_explicit_root(root.path()).unwrap() {
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
    enqueue_embedding_tasks(&store, &repo, &work, true, now).unwrap();
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
    let retry = filter_embeddable_by_task_state(&store, work[1..].to_vec(), false).unwrap();
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
    assert!(validate_embedding_task_bindings(&store, &work).is_err());
    assert!(filter_embeddable_by_task_state(&store, work.clone(), false).is_err());
    assert!(enqueue_embedding_tasks(&store, &repo, &work, true, now).is_err());
    assert_eq!(
        fs::read(repo.kio_dir().join("tasks.jsonl")).unwrap(),
        before
    );
}

#[test]
fn persisted_embedding_consent_requires_the_exact_builtin_identity() {
    use kio_adapter::catalog::AdoptedEmbeddingExecution;
    use kio_adapter::tool_lock::{EmbeddingToolLockEntry, ToolLock};
    if !isolated_child_for(
        "contextual_embedding_tests::persisted_embedding_consent_requires_the_exact_builtin_identity",
    ) {
        return;
    }
    assert!(matches!(
        active_embedding_execution(),
        Some(EmbeddingExecution::DeterministicEvaluator)
    ));
    let root = private_tempdir();
    let repo = match initialize_explicit_root(root.path()).unwrap() {
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
