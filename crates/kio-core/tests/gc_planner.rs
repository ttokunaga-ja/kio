use std::fs;

use kio_core::cas::{ChunkObject, ObjectKind, ObjectStore, hash_bytes};
use kio_core::dag::{CommitObject, CommitStats, CommitType, TreeEntry, build_tree};
#[cfg(unix)]
use kio_core::gc::GcPlanLimits;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use kio_core::gc::GcReceiptPublication;
use kio_core::gc::{
    GcAutomationConfig, GcAutomationMode, GcInProgressMarker, GcIndexState, GcPlan, GcPlanner,
    GcSweepSession, ShallowReceipt, validated_final_shallow_receipts,
};
use kio_core::scope::Repository;
use serde_json::json;
use tempfile::TempDir;

const NOW: i64 = 1_767_225_600; // 2026-01-01T00:00:00Z
const TOOL: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const RAW: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

struct Fixture {
    temp: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        Repository::init(temp.path()).unwrap();
        Self { temp }
    }

    fn kio(&self) -> std::path::PathBuf {
        self.temp.path().join(".kio")
    }

    fn canonical_root(&self) -> std::path::PathBuf {
        self.temp.path().canonicalize().unwrap()
    }

    fn tree(&self, name: &str) -> String {
        let tree = build_tree(vec![TreeEntry::raw_file(name, RAW).unwrap()]).unwrap();
        let store = ObjectStore::new(self.kio());
        store
            .write_json(ObjectKind::Tree, &serde_json::to_value(tree).unwrap())
            .unwrap()
            .0
    }

    fn commit(&self, tree: &str, parent: Option<String>, at: &str, kind: CommitType) -> String {
        let commit = CommitObject::new(
            tree.into(),
            parent,
            at.into(),
            "fixture".into(),
            TOOL.into(),
            CommitStats {
                files_added: 0,
                files_modified: 0,
                files_deleted: 0,
            },
            kind,
        )
        .unwrap();
        let store = ObjectStore::new(self.kio());
        store
            .write_json(ObjectKind::Commit, &serde_json::to_value(commit).unwrap())
            .unwrap()
            .0
    }

    fn purged_commit(&self, tree: &str, parent: Option<String>, at: &str) -> String {
        let commit = CommitObject::new_purged(
            tree.into(),
            parent,
            at.into(),
            "fixture purge".into(),
            TOOL.into(),
            CommitStats {
                files_added: 0,
                files_modified: 0,
                files_deleted: 0,
            },
            vec![RAW.into()],
        )
        .unwrap();
        ObjectStore::new(self.kio())
            .write_json(ObjectKind::Commit, &serde_json::to_value(commit).unwrap())
            .unwrap()
            .0
    }

    fn head(&self, hash: &str) {
        fs::write(self.kio().join("HEAD"), format!("{hash}\n")).unwrap();
    }
    fn tag(&self, _name: &str, hash: &str) {
        fs::write(
            self.kio()
                .join("refs/tags-v1")
                .join(format!("tag-{}", &hash[7..])),
            hash,
        )
        .unwrap();
    }
    fn policy(&self, text: &str) {
        fs::write(self.kio().join("config.toml"), text).unwrap();
    }
    fn plan(&self) -> GcPlan {
        GcPlanner::bind(self.canonical_root())
            .unwrap()
            .plan_at(NOW)
            .unwrap()
    }
}

fn hashes(plan: &GcPlan) -> Vec<String> {
    plan.candidates
        .iter()
        .map(|c| c.commit_hash.clone())
        .collect()
}
fn excluded(plan: &GcPlan, reason: &str) -> u64 {
    plan.exclusions
        .iter()
        .find(|x| x.reason == reason)
        .map_or(0, |x| x.count)
}

#[test]
fn automation_binding_is_capability_bound_strict_and_defaults_manual() {
    let f = Fixture::new();
    let session = GcSweepSession::bind(f.canonical_root()).unwrap();
    assert_eq!(
        session.automation_binding().unwrap().config,
        GcAutomationConfig {
            mode: GcAutomationMode::ManualOnly,
            max_runtime_seconds: None,
            idle_threshold_seconds: None,
        }
    );

    f.policy("[gc]\nmode = \"after_index\"\nmax_runtime_seconds = 17\n");
    assert_eq!(
        session.automation_binding().unwrap().config,
        GcAutomationConfig {
            mode: GcAutomationMode::AfterIndex,
            max_runtime_seconds: Some(17),
            idle_threshold_seconds: None,
        }
    );

    // Automatic deletion authority is the complete validated `[gc]` subtree,
    // not unrelated configuration that `index --approve` may durably update
    // during its own writer transaction.
    f.policy(
        "[gc]\nmode = \"after_index\"\nmax_runtime_seconds = 17\n\
         [gc.auto_retention]\nkeep_last_hours = 1\n\
         [adapter.policy]\nallow_network = false\n",
    );
    let authority = session.automation_binding().unwrap();
    f.policy(
        "[gc]\nmode = \"after_index\"\nmax_runtime_seconds = 17\n\
         [gc.auto_retention]\nkeep_last_hours = 1\n\
         [adapter.policy]\nallow_network = true\n",
    );
    assert_eq!(session.automation_binding().unwrap(), authority);
    f.policy(
        "[gc]\nmode = \"after_index\"\nmax_runtime_seconds = 17\n\
         [gc.auto_retention]\nkeep_last_hours = 2\n\
         [adapter.policy]\nallow_network = true\n",
    );
    assert_ne!(session.automation_binding().unwrap(), authority);

    f.policy("[gc]\nmode = \"after_index\"\nunknown = true\n");
    assert!(session.automation_binding().is_err());

    for invalid in [
        "[gc]\nmode = \"manual_only\"\nmax_runtime_seconds = 1\n",
        "[gc]\nmode = \"manual_only\"\nidle_threshold_seconds = 1\n",
        "[gc]\nmode = \"after_index\"\n",
        "[gc]\nmode = \"after_index\"\nmax_runtime_seconds = 1\nidle_threshold_seconds = 1\n",
        "[gc]\nmode = \"on_idle\"\nmax_runtime_seconds = 1\n",
        "[gc]\nmode = \"on_idle\"\nidle_threshold_seconds = 1\n",
        "[gc]\nmode = \"on_idle\"\nmax_runtime_seconds = 86401\nidle_threshold_seconds = 1\n",
        "[gc]\nmode = \"on_idle\"\nmax_runtime_seconds = 1\nidle_threshold_seconds = 31536001\n",
    ] {
        f.policy(invalid);
        assert!(session.automation_binding().is_err(), "{invalid}");
    }
    f.policy("[gc]\nmode = \"on_idle\"\nmax_runtime_seconds = 17\nidle_threshold_seconds = 23\n");
    assert_eq!(
        session.automation_binding().unwrap().config,
        GcAutomationConfig {
            mode: GcAutomationMode::OnIdle,
            max_runtime_seconds: Some(17),
            idle_threshold_seconds: Some(23),
        }
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn recovery_rejects_preexisting_receipt_timestamp_or_inode_replacement() {
    fn fixture() -> (Fixture, String, String, GcInProgressMarker, GcSweepSession) {
        let f = Fixture::new();
        let shallow_tree = f.tree("old");
        let shallow = f.commit(
            &shallow_tree,
            None,
            "2025-01-01T00:00:00Z",
            CommitType::Auto,
        );
        let head_tree = f.tree("head");
        let head = f.commit(
            &head_tree,
            Some(shallow.clone()),
            "2026-01-01T00:00:00Z",
            CommitType::Manual,
        );
        f.head(&head);
        f.tag("main", &head);
        let receipt_dir = f.kio().join("gc/shallowed");
        fs::create_dir_all(&receipt_dir).unwrap();
        let leaf = &shallow[7..];
        fs::write(
            receipt_dir.join(leaf),
            ShallowReceipt::new(
                shallow.clone(),
                shallow_tree.clone(),
                "2026-01-01T00:00:00Z".into(),
            )
            .unwrap()
            .canonical_bytes()
            .unwrap(),
        )
        .unwrap();
        let store = ObjectStore::new(f.kio());
        fs::remove_file(store.object_path(ObjectKind::Tree, &shallow_tree).unwrap()).unwrap();
        let plan = f.plan();
        let marker = GcInProgressMarker::from_plan(
            &plan,
            "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            "2026-01-01T00:00:00Z".into(),
            GcIndexState::Absent,
        )
        .unwrap();
        let session = GcSweepSession::bind(f.canonical_root()).unwrap();
        session.publish_marker(&marker).unwrap();
        (f, shallow, shallow_tree, marker, session)
    }
    let (f, commit, tree, marker, session) = fixture();
    let leaf = &commit[7..];
    fs::write(
        f.kio().join("gc/shallowed").join(leaf),
        ShallowReceipt::new(commit.clone(), tree.clone(), "2026-01-01T00:00:01Z".into())
            .unwrap()
            .canonical_bytes()
            .unwrap(),
    )
    .unwrap();
    assert!(
        session
            .validate_frozen_marker_current_truth(&marker)
            .is_err()
    );

    let (f, commit, _tree, marker, session) = fixture();
    let path = f.kio().join("gc/shallowed").join(&commit[7..]);
    let original = fs::read(&path).unwrap();
    let replacement = path.with_extension("replacement");
    fs::write(&replacement, &original).unwrap();
    fs::rename(&replacement, &path).unwrap();
    assert!(
        session
            .validate_frozen_marker_current_truth(&marker)
            .is_err()
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn recovery_recomputes_retention_and_rejects_forged_marker_candidates() {
    fn fixture() -> (
        Fixture,
        String,
        String,
        String,
        String,
        String,
        String,
        GcInProgressMarker,
        GcSweepSession,
    ) {
        let f = Fixture::new();
        let expired_tree = f.tree("expired");
        let expired = f.commit(
            &expired_tree,
            None,
            "2025-01-01T00:00:00Z",
            CommitType::Auto,
        );
        let recent_tree = f.tree("recent");
        let recent = f.commit(
            &recent_tree,
            Some(expired.clone()),
            "2026-01-01T00:00:00Z",
            CommitType::Auto,
        );
        let head_tree = f.tree("head");
        let head = f.commit(
            &head_tree,
            Some(recent.clone()),
            "2026-01-02T00:00:00Z",
            CommitType::Manual,
        );
        f.head(&head);
        f.tag("main", &head);
        let plan = f.plan();
        assert_eq!(hashes(&plan), vec![expired.clone()]);
        let marker = GcInProgressMarker::from_plan(
            &plan,
            "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            "2026-01-01T00:00:00Z".into(),
            GcIndexState::Absent,
        )
        .unwrap();
        let session = GcSweepSession::bind(f.canonical_root()).unwrap();
        (
            f,
            expired,
            expired_tree,
            recent,
            recent_tree,
            head,
            head_tree,
            marker,
            session,
        )
    }

    fn forged_with_receipt(
        f: &Fixture,
        session: &GcSweepSession,
        mut marker: GcInProgressMarker,
        commit: String,
        tree: String,
    ) -> GcInProgressMarker {
        marker.phase = kio_core::gc::GcSweepPhase::Receipting;
        marker.candidates = vec![kio_core::gc::GcMarkerCandidate {
            commit_hash: commit.clone(),
            tree_hash: tree.clone(),
            size_bytes: fs::metadata(
                f.kio()
                    .join("objects/trees")
                    .join(&tree[7..9])
                    .join(&tree[9..11])
                    .join(&tree[7..]),
            )
            .unwrap()
            .len(),
        }];
        marker.trees = vec![tree.clone()];
        marker.estimated_bytes = marker.candidates[0].size_bytes;
        marker.validate().unwrap();
        fs::create_dir_all(f.kio().join("gc/shallowed")).unwrap();
        fs::write(
            f.kio().join("gc/shallowed").join(&commit[7..]),
            ShallowReceipt::new(commit, tree, "2026-01-01T00:00:00Z".into())
                .unwrap()
                .canonical_bytes()
                .unwrap(),
        )
        .unwrap();
        session.publish_marker(&marker).unwrap();
        marker
    }

    // A protected Manual commit is syntactically valid in a receipt/marker,
    // but can never be selected by retention.
    let (f, _expired, _expired_tree, _recent, _recent_tree, head, head_tree, marker, session) =
        fixture();
    let forged = forged_with_receipt(&f, &session, marker, head, head_tree);
    assert!(
        session
            .validate_frozen_marker_current_truth(&forged)
            .is_err()
    );

    // A non-tip Auto commit that is still in its recent bucket is similarly
    // not an authorized victim even with a matching durable receipt.
    let (f, _expired, _expired_tree, recent, recent_tree, _head, _head_tree, marker, session) =
        fixture();
    assert_ne!(hashes(&f.plan()), vec![recent.clone()]);
    let forged = forged_with_receipt(&f, &session, marker, recent, recent_tree);
    assert!(
        session
            .validate_frozen_marker_current_truth(&forged)
            .is_err()
    );

    // Adding an otherwise protected pair to the genuine selection is no more
    // authorized than replacing it; exact equality prevents both shapes.
    let (f, _expired, _expired_tree, _recent, _recent_tree, head, head_tree, mut marker, session) =
        fixture();
    marker.phase = kio_core::gc::GcSweepPhase::Receipting;
    marker.candidates.push(kio_core::gc::GcMarkerCandidate {
        commit_hash: head.clone(),
        tree_hash: head_tree.clone(),
        size_bytes: fs::metadata(
            f.kio()
                .join("objects/trees")
                .join(&head_tree[7..9])
                .join(&head_tree[9..11])
                .join(&head_tree[7..]),
        )
        .unwrap()
        .len(),
    });
    marker.estimated_bytes = marker
        .candidates
        .iter()
        .map(|candidate| candidate.size_bytes)
        .sum();
    marker
        .candidates
        .sort_by(|left, right| left.commit_hash.cmp(&right.commit_hash));
    marker.trees = marker
        .candidates
        .iter()
        .map(|candidate| candidate.tree_hash.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    marker.validate().unwrap();
    fs::create_dir_all(f.kio().join("gc/shallowed")).unwrap();
    fs::write(
        f.kio().join("gc/shallowed").join(&head[7..]),
        ShallowReceipt::new(head, head_tree, "2026-01-01T00:00:00Z".into())
            .unwrap()
            .canonical_bytes()
            .unwrap(),
    )
    .unwrap();
    session.publish_marker(&marker).unwrap();
    assert!(
        session
            .validate_frozen_marker_current_truth(&marker)
            .is_err()
    );

    // Omitting a currently eligible item is also forbidden: a forged marker
    // may not turn a full plan into a convenient subset.
    let (
        _f,
        _expired,
        _expired_tree,
        _recent,
        _recent_tree,
        _head,
        _head_tree,
        mut marker,
        session,
    ) = fixture();
    marker.candidates.clear();
    marker.trees.clear();
    marker.estimated_bytes = 0;
    marker.validate().unwrap();
    session.publish_marker(&marker).unwrap();
    assert!(
        session
            .validate_frozen_marker_current_truth(&marker)
            .is_err()
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn recovery_authorization_stays_bound_to_marker_start_across_retention_boundary() {
    let f = Fixture::new();
    let expired_tree = f.tree("expired-at-start");
    let expired = f.commit(
        &expired_tree,
        None,
        "2025-01-01T00:00:00Z",
        CommitType::Auto,
    );
    let recent_tree = f.tree("recent-at-start");
    let recent = f.commit(
        &recent_tree,
        Some(expired.clone()),
        "2026-01-01T00:00:00Z",
        CommitType::Auto,
    );
    let head = f.commit(
        &f.tree("head"),
        Some(recent.clone()),
        "2026-01-02T00:00:00Z",
        CommitType::Manual,
    );
    f.head(&head);
    f.tag("main", &head);

    let start_plan = f.plan();
    assert_eq!(hashes(&start_plan), vec![expired]);
    let later = GcPlanner::bind(f.canonical_root())
        .unwrap()
        .plan_at(NOW + 181 * 24 * 60 * 60)
        .unwrap();
    assert!(hashes(&later).contains(&recent));

    let mut marker = GcInProgressMarker::from_plan(
        &start_plan,
        "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
        "2026-01-01T00:00:00Z".into(),
        GcIndexState::Absent,
    )
    .unwrap();
    let session = GcSweepSession::bind(f.canonical_root()).unwrap();
    session.publish_marker(&marker).unwrap();
    marker.phase = kio_core::gc::GcSweepPhase::Receipting;
    session.advance_marker(&marker).unwrap();
    for candidate in &start_plan.candidates {
        session
            .create_receipt(candidate, marker.started_at.clone())
            .unwrap();
    }
    marker = session.bind_operation_receipts(&marker).unwrap();
    marker.phase = kio_core::gc::GcSweepPhase::Sweeping;
    session.advance_marker(&marker).unwrap();

    // The later wall clock changes a fresh plan, but an irreversible recovery
    // remains authorized by the exact retention decision frozen at started_at.
    session
        .validate_frozen_marker_current_truth(&marker)
        .unwrap();
}

#[test]
fn same_byte_ref_replacement_changes_plan_truth_binding() {
    let f = Fixture::new();
    let tree = f.tree("old");
    let old = f.commit(&tree, None, "2025-01-01T00:00:00Z", CommitType::Auto);
    let head_tree = f.tree("head");
    let head = f.commit(
        &head_tree,
        Some(old),
        "2026-01-01T00:00:00Z",
        CommitType::Manual,
    );
    f.head(&head);
    let first = f.plan();
    let path = f.kio().join("HEAD");
    let bytes = fs::read(&path).unwrap();
    let replacement = path.with_extension("same-bytes");
    fs::write(&replacement, bytes).unwrap();
    fs::rename(&replacement, &path).unwrap();
    let second = f.plan();
    assert_ne!(first.truth_digest, second.truth_digest);
    assert_ne!(first.stable_truth_digest, second.stable_truth_digest);
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn marker_owned_receipt_must_use_marker_timestamp() {
    let f = Fixture::new();
    let tree = f.tree("expired");
    let expired = f.commit(&tree, None, "2025-01-01T00:00:00Z", CommitType::Auto);
    let head_tree = f.tree("head");
    let head = f.commit(
        &head_tree,
        Some(expired),
        "2026-01-01T00:00:00Z",
        CommitType::Manual,
    );
    f.head(&head);
    f.tag("main", &head);
    let plan = f.plan();
    let mut marker = GcInProgressMarker::from_plan(
        &plan,
        "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
        "2026-01-01T00:00:00Z".into(),
        GcIndexState::Absent,
    )
    .unwrap();
    let session = GcSweepSession::bind(f.canonical_root()).unwrap();
    session.publish_marker(&marker).unwrap();
    marker.phase = kio_core::gc::GcSweepPhase::Receipting;
    session.advance_marker(&marker).unwrap();
    let candidate = plan.candidates.first().unwrap();
    assert_eq!(
        session
            .create_receipt(candidate, marker.started_at.clone())
            .unwrap(),
        GcReceiptPublication::NewlyPublished
    );
    assert_eq!(
        session
            .create_receipt(candidate, marker.started_at.clone())
            .unwrap(),
        GcReceiptPublication::AlreadyPresent
    );
    let path = f
        .kio()
        .join("gc/shallowed")
        .join(&candidate.commit_hash[7..]);
    fs::write(
        &path,
        ShallowReceipt::new(
            candidate.commit_hash.clone(),
            candidate.tree_hash.clone(),
            "2026-01-01T00:00:01Z".into(),
        )
        .unwrap()
        .canonical_bytes()
        .unwrap(),
    )
    .unwrap();
    assert!(session.validate_recovery_state(&marker).is_err());
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn sweeping_receipt_binding_rejects_same_bytes_inode_replacement() {
    let f = Fixture::new();
    let tree = f.tree("expired");
    let expired = f.commit(&tree, None, "2025-01-01T00:00:00Z", CommitType::Auto);
    let head_tree = f.tree("head");
    let head = f.commit(
        &head_tree,
        Some(expired),
        "2026-01-01T00:00:00Z",
        CommitType::Manual,
    );
    f.head(&head);
    f.tag("main", &head);
    let plan = f.plan();
    let mut marker = GcInProgressMarker::from_plan(
        &plan,
        "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
        "2026-01-01T00:00:00Z".into(),
        GcIndexState::Absent,
    )
    .unwrap();
    let session = GcSweepSession::bind(f.canonical_root()).unwrap();
    session.publish_marker(&marker).unwrap();
    marker.phase = kio_core::gc::GcSweepPhase::Receipting;
    session.advance_marker(&marker).unwrap();
    for candidate in &plan.candidates {
        session
            .create_receipt(candidate, marker.started_at.clone())
            .unwrap();
    }
    marker = session.bind_operation_receipts(&marker).unwrap();
    marker.phase = kio_core::gc::GcSweepPhase::Sweeping;
    session.advance_marker(&marker).unwrap();
    let candidate = plan.candidates.first().unwrap();
    let path = f
        .kio()
        .join("gc/shallowed")
        .join(&candidate.commit_hash[7..]);
    let bytes = fs::read(&path).unwrap();
    let replacement = path.with_extension("replacement");
    fs::write(&replacement, bytes).unwrap();
    fs::rename(&replacement, &path).unwrap();
    assert!(session.validate_recovery_state(&marker).is_err());
}

#[test]
fn retention_tier_boundaries_keep_each_tier_and_plan_only_expired_auto() {
    let f = Fixture::new();
    f.policy("[gc]\nmode=\"manual_only\"\n[gc.auto_retention]\nkeep_last_hours=1\nkeep_hourly_days=1\nkeep_daily_weeks=1\nkeep_weekly_months=1\n");
    let old = f.commit(
        &f.tree("old"),
        None,
        "2025-12-02T00:00:00Z",
        CommitType::Auto,
    );
    let weekly = f.commit(
        &f.tree("weekly"),
        Some(old.clone()),
        "2025-12-25T00:00:00Z",
        CommitType::Auto,
    );
    let daily = f.commit(
        &f.tree("daily"),
        Some(weekly),
        "2025-12-31T00:00:00Z",
        CommitType::Auto,
    );
    let hourly = f.commit(
        &f.tree("hourly"),
        Some(daily),
        "2025-12-31T23:00:00Z",
        CommitType::Auto,
    );
    let recent = f.commit(
        &f.tree("recent"),
        Some(hourly),
        "2025-12-31T23:00:01Z",
        CommitType::Auto,
    );
    let head = f.commit(
        &f.tree("head"),
        Some(recent),
        "2026-01-01T00:00:00Z",
        CommitType::Manual,
    );
    f.head(&head);
    let p = f.plan();
    assert_eq!(hashes(&p), vec![old]);
    assert_eq!(excluded(&p, "retained_recent"), 1);
    assert_eq!(excluded(&p, "retained_hourly"), 1);
    assert_eq!(excluded(&p, "retained_daily"), 1);
    assert_eq!(excluded(&p, "retained_weekly"), 1);
}

#[test]
fn retention_policy_rejects_unknown_fields_and_non_monotonic_horizons() {
    let unknown = Fixture::new();
    unknown.policy(
        "[gc]\nmode=\"manual_only\"\n[gc.auto_retention]\nkeep_last_hours=24\nkeep_forever=true\n",
    );
    assert_eq!(
        GcPlanner::bind(unknown.canonical_root())
            .unwrap()
            .plan_at(NOW)
            .unwrap_err()
            .error_code(),
        "KIO-E-CONFIG-SCHEMA-001"
    );

    let non_monotonic = Fixture::new();
    non_monotonic.policy(
        "[gc]\nmode=\"manual_only\"\n[gc.auto_retention]\nkeep_last_hours=25\nkeep_hourly_days=1\n",
    );
    assert_eq!(
        GcPlanner::bind(non_monotonic.canonical_root())
            .unwrap()
            .plan_at(NOW)
            .unwrap_err()
            .error_code(),
        "KIO-E-CONFIG-SCHEMA-001"
    );
}

#[test]
fn fractional_timestamps_preserve_true_future_and_latest_bucket_member() {
    let f = Fixture::new();
    f.policy("[gc]\nmode=\"manual_only\"\n[gc.auto_retention]\nkeep_last_hours=0\nkeep_hourly_days=7\nkeep_daily_weeks=1\nkeep_weekly_months=1\n");
    let older = f.commit(
        &f.tree("fraction-older"),
        None,
        "2025-12-31T12:00:00.100Z",
        CommitType::Auto,
    );
    let newer = f.commit(
        &f.tree("fraction-newer"),
        Some(older.clone()),
        "2025-12-31T12:00:00.900Z",
        CommitType::Auto,
    );
    let future = f.commit(
        &f.tree("fraction-future"),
        Some(newer),
        "2026-01-01T00:00:00.100Z",
        CommitType::Auto,
    );
    let head = f.commit(
        &f.tree("head"),
        Some(future),
        "2026-01-01T00:00:01Z",
        CommitType::Manual,
    );
    f.head(&head);

    let plan = f.plan();
    assert_eq!(hashes(&plan), vec![older]);
    assert_eq!(excluded(&plan, "retained_hourly"), 1);
    assert_eq!(excluded(&plan, "future_timestamp"), 1);
}

#[test]
fn head_and_tag_tips_and_protected_types_are_excluded() {
    let f = Fixture::new();
    f.policy("[gc]\nmode=\"manual_only\"\n[gc.auto_retention]\nkeep_last_hours=0\nkeep_hourly_days=0\nkeep_daily_weeks=0\nkeep_weekly_months=0\n");
    let auto = f.commit(
        &f.tree("auto"),
        None,
        "2025-01-01T00:00:00Z",
        CommitType::Auto,
    );
    let manual = f.commit(
        &f.tree("manual"),
        Some(auto.clone()),
        "2025-01-02T00:00:00Z",
        CommitType::Manual,
    );
    let intermediate = f.commit(
        &f.tree("intermediate"),
        Some(manual),
        "2025-01-03T00:00:00Z",
        CommitType::Manual,
    );
    let protected_manual = f.commit(
        &f.tree("protected-manual"),
        Some(intermediate),
        "2025-01-04T00:00:00Z",
        CommitType::Manual,
    );
    let purged = f.purged_commit(
        &f.tree("purged"),
        Some(protected_manual),
        "2025-01-05T00:00:00Z",
    );
    let head = f.commit(
        &f.tree("head"),
        Some(purged),
        "2025-01-06T00:00:00Z",
        CommitType::Auto,
    );
    let tagged_history = f.commit(
        &f.tree("tagged-history"),
        None,
        "2025-01-07T00:00:00Z",
        CommitType::Auto,
    );
    let tag = f.commit(
        &f.tree("tag"),
        None,
        "2025-01-08T00:00:00Z",
        CommitType::Auto,
    );
    f.head(&head);
    f.tag("topic", &tagged_history);
    fs::write(
        f.kio()
            .join("refs/tags-v1")
            .join(format!("tag-{}", &tag[7..])),
        &tag,
    )
    .unwrap();
    let p = f.plan();
    assert_eq!(hashes(&p), vec![auto]);
    assert_eq!(excluded(&p, "ref_tip"), 3);
    assert_eq!(excluded(&p, "protected_commit_type"), 4);
}

#[test]
fn raw_chunk_and_commit_objects_are_never_planned() {
    let f = Fixture::new();
    f.policy("[gc]\nmode=\"manual_only\"\n[gc.auto_retention]\nkeep_last_hours=0\nkeep_hourly_days=0\nkeep_daily_weeks=0\nkeep_weekly_months=0\n");
    let store = ObjectStore::new(f.kio());
    let raw_hash = store.write_raw(b"retained raw bytes").unwrap();
    let text = "retained chunk text";
    let chunk = ChunkObject {
        spec_version: 1,
        raw_hash: raw_hash.clone(),
        tool_profile_hash: TOOL.into(),
        r#gen: 0,
        unit_key: "unit-1".into(),
        unit_content_hash: hash_bytes(text.as_bytes()),
        heading_path: vec!["heading".into()],
        section_id: None,
        byte_start: 0,
        byte_end: text.len() as u64,
        text_hash: hash_bytes(text.as_bytes()),
        text: text.into(),
    };
    let chunk_hash = store.write_chunk(&chunk).unwrap();
    let auto = f.commit(
        &f.tree("auto"),
        None,
        "2025-01-01T00:00:00Z",
        CommitType::Auto,
    );
    let head = f.commit(
        &f.tree("head"),
        Some(auto.clone()),
        "2025-01-02T00:00:00Z",
        CommitType::Manual,
    );
    f.head(&head);

    let raw_path = store.object_path(ObjectKind::Raw, &raw_hash).unwrap();
    let chunk_path = store.chunk_path(&chunk_hash).unwrap();
    let commit_path = store.object_path(ObjectKind::Commit, &auto).unwrap();
    let plan = f.plan();

    assert_eq!(plan.object_kinds_planned, vec!["tree"]);
    assert_eq!(hashes(&plan), vec![auto]);
    assert!(raw_path.is_file());
    assert!(chunk_path.is_file());
    assert!(commit_path.is_file());
}

#[test]
fn repaired_retention_follows_head_and_shared_trees_are_not_candidates() {
    let f = Fixture::new();
    f.policy("[gc]\nmode=\"manual_only\"\n[gc.auto_retention]\nkeep_last_hours=0\nkeep_hourly_days=0\nkeep_daily_weeks=0\nkeep_weekly_months=0\n[gc.derived_retention]\nkeep_repaired=1\n");
    let repaired_old = f.commit(
        &f.tree("old"),
        None,
        "2025-01-01T00:00:00Z",
        CommitType::Repaired,
    );
    let repaired_new = f.commit(
        &f.tree("new"),
        Some(repaired_old.clone()),
        "2025-01-02T00:00:00Z",
        CommitType::Repaired,
    );
    let shared_tree = f.tree("shared");
    let auto = f.commit(
        &shared_tree,
        Some(repaired_new),
        "2025-01-03T00:00:00Z",
        CommitType::Auto,
    );
    let manual = f.commit(
        &shared_tree,
        Some(auto),
        "2025-01-04T00:00:00Z",
        CommitType::Manual,
    );
    f.head(&manual);
    f.tag("main", &manual);
    let outside_head = f.commit(
        &f.tree("outside-head"),
        None,
        "2025-01-05T00:00:00Z",
        CommitType::Repaired,
    );
    let tag_tip = f.commit(
        &f.tree("tag-tip"),
        Some(outside_head),
        "2025-01-06T00:00:00Z",
        CommitType::Manual,
    );
    f.tag("detached", &tag_tip);
    let p = f.plan();
    let output = serde_json::to_value(&p).unwrap();
    assert_eq!(output["policy"]["keep_repaired"], 1);
    assert!(output["policy"].get("keep_repaired_per_branch").is_none());
    assert_eq!(excluded(&p, "repaired_outside_head"), 1);
    assert_eq!(hashes(&p), vec![repaired_old]);
    assert_eq!(excluded(&p, "retained_repaired"), 1);
    assert_eq!(excluded(&p, "shared_tree_non_shallow"), 1);
    assert_eq!(p.object_kinds_planned, vec!["tree"]);
    assert_eq!(p.candidate_tree_count, 1);
}

#[test]
fn repaired_retention_rejects_legacy_key_instead_of_defaulting() {
    let f = Fixture::new();
    for fields in [
        "keep_repaired_per_branch=1",
        "keep_repaired=1\nkeep_repaired_per_branch=1",
    ] {
        f.policy(&format!(
            "[gc]\nmode=\"manual_only\"\n[gc.derived_retention]\n{fields}\n"
        ));
        let error = GcPlanner::bind(f.canonical_root())
            .unwrap()
            .plan_at(NOW)
            .unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-CONFIG-SCHEMA-001");
    }
}

#[test]
fn an_existing_shallow_receipt_is_idempotent_and_skips_missing_tree() {
    let f = Fixture::new();
    let tree = f.tree("gone");
    let shallow = f.commit(&tree, None, "2025-01-01T00:00:00Z", CommitType::Auto);
    let head = f.commit(
        &f.tree("head"),
        Some(shallow.clone()),
        "2025-01-02T00:00:00Z",
        CommitType::Manual,
    );
    f.head(&head);
    let leaf = &shallow[7..];
    let receipt_dir = f.kio().join("gc/shallowed");
    fs::create_dir_all(&receipt_dir).unwrap();
    fs::write(
        receipt_dir.join(leaf),
        kio_core::gc::ShallowReceipt::new(
            shallow.clone(),
            tree.clone(),
            "2026-01-01T00:00:00Z".into(),
        )
        .unwrap()
        .canonical_bytes()
        .unwrap(),
    )
    .unwrap();
    let path = f
        .kio()
        .join("objects/trees")
        .join(&tree[7..9])
        .join(&tree[9..11])
        .join(&tree[7..]);
    fs::remove_file(path).unwrap();
    let p = f.plan();
    assert!(p.candidates.is_empty());
    assert_eq!(p.stats.trees_verified, 1); // only the live HEAD tree
    assert_eq!(excluded(&p, "already_shallow"), 1);
}

#[test]
fn final_shallow_validation_requires_every_shared_tree_receipt() {
    let f = Fixture::new();
    let shared_tree = f.tree("shared-gone");
    let first = f.commit(&shared_tree, None, "2025-01-01T00:00:00Z", CommitType::Auto);
    let second = f.commit(
        &shared_tree,
        Some(first.clone()),
        "2025-01-01T00:00:01Z",
        CommitType::Auto,
    );
    let head = f.commit(
        &f.tree("shared-head"),
        Some(second),
        "2025-01-02T00:00:00Z",
        CommitType::Manual,
    );
    f.head(&head);
    let receipts = f.kio().join("gc/shallowed");
    fs::create_dir_all(&receipts).unwrap();
    fs::write(
        receipts.join(&first[7..]),
        ShallowReceipt::new(
            first.clone(),
            shared_tree.clone(),
            "2026-01-01T00:00:00Z".into(),
        )
        .unwrap()
        .canonical_bytes()
        .unwrap(),
    )
    .unwrap();
    let path = f
        .kio()
        .join("objects/trees")
        .join(&shared_tree[7..9])
        .join(&shared_tree[9..11])
        .join(&shared_tree[7..]);
    fs::remove_file(path).unwrap();

    let error = validated_final_shallow_receipts(&f.kio()).unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
}

#[cfg(unix)]
#[test]
fn final_shallow_inventory_rejects_a_symlinked_kio_boundary() {
    use std::os::unix::fs::symlink;

    let source = Fixture::new();
    let replacement = Fixture::new();
    let source_kio = source.kio();
    let parked = source.temp.path().join(".kio.parked");
    fs::rename(&source_kio, &parked).unwrap();
    symlink(replacement.kio(), &source_kio).unwrap();

    let error = validated_final_shallow_receipts(&source_kio).unwrap_err();
    assert!(matches!(
        error.error_code(),
        "KIO-E-STORE-CORRUPT-001" | "KIO-E-STORE-IO-001"
    ));
}

#[test]
fn marker_estimated_bytes_must_exactly_bind_candidate_tree_sizes() {
    let f = Fixture::new();
    let tree = f.tree("expired-size-bound");
    let expired = f.commit(&tree, None, "2025-01-01T00:00:00Z", CommitType::Auto);
    let head = f.commit(
        &f.tree("size-bound-head"),
        Some(expired),
        "2026-01-01T00:00:00Z",
        CommitType::Manual,
    );
    f.head(&head);
    f.tag("main", &head);
    let plan = f.plan();
    let mut marker = GcInProgressMarker::from_plan(
        &plan,
        "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
        "2026-01-01T00:00:00Z".into(),
        GcIndexState::Absent,
    )
    .unwrap();
    assert!(marker.estimated_bytes > 0);
    marker.estimated_bytes = 0;
    assert!(marker.validate().is_err());
    marker.estimated_bytes = plan.estimated_bytes;
    marker.candidates[0].size_bytes = 0;
    assert!(marker.validate().is_err());
}

#[test]
fn malformed_receipt_and_missing_unreceipted_tree_fail_safely() {
    let f = Fixture::new();
    let tree = f.tree("missing");
    let auto = f.commit(&tree, None, "2025-01-01T00:00:00Z", CommitType::Auto);
    let head = f.commit(
        &f.tree("head"),
        Some(auto),
        "2025-01-02T00:00:00Z",
        CommitType::Manual,
    );
    f.head(&head);
    let tree_path = f
        .kio()
        .join("objects/trees")
        .join(&tree[7..9])
        .join(&tree[9..11])
        .join(&tree[7..]);
    fs::remove_file(tree_path).unwrap();
    assert_eq!(
        GcPlanner::bind(f.canonical_root())
            .unwrap()
            .plan_at(NOW)
            .unwrap_err()
            .error_code(),
        "KIO-E-STORE-CORRUPT-001"
    );

    let f = Fixture::new();
    let receipts = f.kio().join("gc/shallowed");
    fs::create_dir_all(&receipts).unwrap();
    fs::write(receipts.join("a".repeat(64)), b"not json").unwrap();
    assert_eq!(
        GcPlanner::bind(f.canonical_root())
            .unwrap()
            .plan_at(NOW)
            .unwrap_err()
            .error_code(),
        "KIO-E-STORE-CORRUPT-001"
    );

    let f = Fixture::new();
    let tree = f.tree("invalid-receipt-time");
    let commit = f.commit(&tree, None, "2025-01-01T00:00:00Z", CommitType::Auto);
    let receipts = f.kio().join("gc/shallowed");
    fs::create_dir_all(&receipts).unwrap();
    fs::write(
        receipts.join(&commit[7..]),
        serde_json::to_vec(&json!({
            "commit_hash": commit,
            "tree_hash": tree,
            "gc_policy": "shallow",
            "shallowed_at": "2026-02-31T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        GcPlanner::bind(f.canonical_root())
            .unwrap()
            .plan_at(NOW)
            .unwrap_err()
            .error_code(),
        "KIO-E-STORE-CORRUPT-001"
    );
}

#[cfg(unix)]
#[test]
fn rejects_symlink_hardlink_and_traversal_limits() {
    use std::os::unix::fs::symlink;
    let real = Fixture::new();
    let parent = tempfile::tempdir().unwrap();
    let scope_link = parent.path().join("scope-link");
    symlink(real.temp.path(), &scope_link).unwrap();
    assert!(GcPlanner::bind(&scope_link).is_err());

    let f = Fixture::new();
    let saved = f.kio().join("HEAD.saved");
    fs::rename(f.kio().join("HEAD"), &saved).unwrap();
    symlink("HEAD.saved", f.kio().join("HEAD")).unwrap();
    assert!(
        GcPlanner::bind(f.canonical_root())
            .unwrap()
            .plan_at(NOW)
            .is_err()
    );

    let f = Fixture::new();
    fs::hard_link(f.kio().join("HEAD"), f.kio().join("HEAD.copy")).unwrap();
    assert!(
        GcPlanner::bind(f.canonical_root())
            .unwrap()
            .plan_at(NOW)
            .is_err()
    );

    let f = Fixture::new();
    let auto = f.commit(
        &f.tree("depth"),
        None,
        "2025-01-01T00:00:00Z",
        CommitType::Auto,
    );
    f.head(&auto);
    let err = GcPlanner::bind(f.canonical_root())
        .unwrap()
        .with_limits(GcPlanLimits {
            max_depth: 3,
            ..GcPlanLimits::default()
        })
        .plan_at(NOW)
        .unwrap_err();
    assert_eq!(err.error_code(), "KIO-E-GC-PLAN-LIMIT-001");

    let f = Fixture::new();
    let tag_target = format!("sha256:{}", "a".repeat(64));
    f.tag("retained", &tag_target);
    let err = GcPlanner::bind(f.canonical_root())
        .unwrap()
        .with_limits(GcPlanLimits {
            max_dir_entries: 0,
            ..GcPlanLimits::default()
        })
        .plan_at(NOW)
        .unwrap_err();
    assert_eq!(err.error_code(), "KIO-E-GC-PLAN-LIMIT-001");

    let f = Fixture::new();
    let err = GcPlanner::bind(f.canonical_root())
        .unwrap()
        .with_limits(GcPlanLimits {
            max_verified_bytes: 0,
            ..GcPlanLimits::default()
        })
        .plan_at(NOW)
        .unwrap_err();
    assert_eq!(err.error_code(), "KIO-E-GC-PLAN-LIMIT-001");

    let f = Fixture::new();
    let auto = f.commit(
        &f.tree("graph"),
        None,
        "2025-01-01T00:00:00Z",
        CommitType::Auto,
    );
    f.head(&auto);
    let err = GcPlanner::bind(f.canonical_root())
        .unwrap()
        .with_limits(GcPlanLimits {
            max_graph_steps: 0,
            ..GcPlanLimits::default()
        })
        .plan_at(NOW)
        .unwrap_err();
    assert_eq!(err.error_code(), "KIO-E-GC-PLAN-LIMIT-001");
}

#[cfg(unix)]
#[test]
fn retained_scope_binding_rejects_kio_path_replacement() {
    let f = Fixture::new();
    let planner = GcPlanner::bind(f.canonical_root()).unwrap();
    fs::rename(f.kio(), f.temp.path().join(".kio.replaced")).unwrap();
    fs::create_dir(f.kio()).unwrap();
    let error = planner.plan_at(NOW).unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
}

#[test]
fn plan_serialization_is_deterministic() {
    let f = Fixture::new();
    let auto = f.commit(
        &f.tree("auto"),
        None,
        "2025-01-01T00:00:00Z",
        CommitType::Auto,
    );
    let head = f.commit(
        &f.tree("head"),
        Some(auto),
        "2025-01-02T00:00:00Z",
        CommitType::Manual,
    );
    f.head(&head);
    let planner = GcPlanner::bind(f.canonical_root()).unwrap();
    assert_eq!(
        serde_json::to_vec(&planner.plan_at(NOW).unwrap()).unwrap(),
        serde_json::to_vec(&planner.plan_at(NOW).unwrap()).unwrap()
    );
}
