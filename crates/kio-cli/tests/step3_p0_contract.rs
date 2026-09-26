use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use kio_core::scope::Repository;
use kio_core::{
    cas::{ContentObjectKind, ObjectKind, ObjectStore},
    dag::CommitObject,
    gc::ShallowReceipt,
    portable::portable_tag_leaf,
};
use kio_index::aggregator::{AggIndexStatus, Aggregator};
use rusqlite::Connection;
use serde_json::Value;
use tempfile::TempDir;

const TEST_ADOPTED_EMBEDDING_ENV: &str = "KIO_TEST_GEMINI_EMBED";
const TEST_DETERMINISTIC_EMBEDDING_ENV: &str = "KIO_EVAL_DETERMINISTIC_EMBED";
const TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV: &str = "KIO_TEST_MISTRAL_OCR";

const KIO_CHILD_ENV_DENYLIST: &[&str] = &[
    "GEMINI_API_KEY",
    "MISTRAL_API_KEY",
    "KIO_FIXED_NOW",
    "KIO_TEST_GEMINI_EMBED",
    "KIO_TEST_GEMINI_BATCH",
    "KIO_EVAL_DETERMINISTIC_EMBED",
    "KIO_TEST_MISTRAL_OCR",
    "KIO_TEST_MISTRAL_BATCH",
    "KIO_TEST_BATCH_INVENTORY",
    "KIO_TEST_MARKDOWNIZE_ADAPTER",
    "KIO_TEST_QUERY_EMBED_TRACE",
    "KIO_TEST_HOLD_LOCK_READY",
    "KIO_TEST_SCOPE_SEARCH_DELAY_SCOPE_ID",
    "KIO_TEST_SCOPE_SEARCH_DELAY_MS",
    "KIO_TEST_AGGREGATOR_PROJECTION_FAULT",
    "KIO_TEST_REPLICA_AFTER_HEAD_FAULT",
    "KIO_TEST_SEARCH_RESPONSE_BARRIER_READY",
    "KIO_TEST_R13_2_AUTH",
    "KIO_TEST_R13_2_DECLARED",
    "KIO_TEST_R13_2_FALLBACK",
    "KIO_TEST_WINDOWS_PROFILE",
];

fn hermetic_kio_command() -> Command {
    let mut command = Command::cargo_bin("kio").unwrap();
    for name in KIO_CHILD_ENV_DENYLIST {
        command.env_remove(name);
    }
    command
}

fn hermetic_process_command(bin: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(bin);
    for name in KIO_CHILD_ENV_DENYLIST {
        command.env_remove(name);
    }
    command
}

fn value_path_ends_with(value: &Value, suffix: &str) -> bool {
    value
        .as_str()
        .is_some_and(|path| Path::new(path).ends_with(suffix))
}

fn kio(dir: &TempDir, args: &[&str]) -> Command {
    let mut command = hermetic_kio_command();
    command
        .current_dir(dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join(".test-config"))
        .env("XDG_DATA_HOME", dir.path().join(".test-data"))
        .env("XDG_CACHE_HOME", dir.path().join(".test-cache"))
        .args(args);
    command
}

fn json_success(dir: &TempDir, args: &[&str]) -> Value {
    let output = kio(dir, args)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

fn json_failure(dir: &TempDir, args: &[&str], code: i32) -> Value {
    let output = kio(dir, args)
        .arg("--json")
        .assert()
        .code(code)
        .get_output()
        .stderr
        .clone();
    serde_json::from_slice(&output).unwrap()
}

/// 05 §1.7.2 / §4.2 (2026-08-11): `kio view --json` stopped returning a
/// `text` field -- it resolves to a full-text normalized view path plus a
/// view-local byte span instead. This is the only way left to recover
/// chunk-adjacent text from `view`'s JSON, and exercising it (read the file,
/// slice by the reported offsets) is what actually proves the unit-local ->
/// view-local offset translation is correct, unlike a plain field-content
/// assertion on the old `text` field ever did.
fn view_slice(viewed: &Value) -> String {
    let view_path = viewed["view_path"]
        .as_str()
        .unwrap_or_else(|| panic!("view_path must be a resolvable path: {viewed}"));
    let start = viewed["view_byte_start"]
        .as_u64()
        .unwrap_or_else(|| panic!("view_byte_start must be present: {viewed}"))
        as usize;
    let end = viewed["view_byte_end"]
        .as_u64()
        .unwrap_or_else(|| panic!("view_byte_end must be present: {viewed}"))
        as usize;
    let bytes = fs::read(view_path)
        .unwrap_or_else(|err| panic!("failed to read view_path {view_path}: {err}"));
    String::from_utf8(bytes[start..end].to_vec()).unwrap()
}

fn json_success_path(path: &Path, data_home: &Path, args: &[&str]) -> Value {
    let output = hermetic_kio_command()
        .current_dir(path)
        .env("XDG_CONFIG_HOME", data_home.join("config"))
        .env("XDG_DATA_HOME", data_home.join("data"))
        .env("XDG_CACHE_HOME", data_home.join("cache"))
        .args(args)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

/// Child-scope failures are reported as a partial result on stdout, rather
/// than an error document on stderr. Keep that transport distinction explicit
/// in cross-platform child-scope contracts.
#[cfg(windows)]
fn json_code_stdout_path(path: &Path, data_home: &Path, args: &[&str], code: i32) -> Value {
    let output = hermetic_kio_command()
        .current_dir(path)
        .env("XDG_CONFIG_HOME", data_home.join("config"))
        .env("XDG_DATA_HOME", data_home.join("data"))
        .env("XDG_CACHE_HOME", data_home.join("cache"))
        .args(args)
        .arg("--json")
        .assert()
        .code(code)
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

#[cfg(windows)]
fn assert_windows_bound_child_unsupported(result: &Value, path: &str) {
    assert_eq!(result["error_code"], "KIO-E-INDEX-PARTIAL-001");
    let child = result["child_scopes"]
        .as_array()
        .expect("partial index output must retain child scope rows")
        .iter()
        .find(|row| row["path"] == path)
        .unwrap_or_else(|| panic!("missing discovered child row for {path}"));
    assert_eq!(child["status"], "skipped_error", "{child}");
    assert_eq!(
        child["error_code"], "KIO-E-SCOPE-BOUND-UNSUPPORTED-001",
        "{child}"
    );
}

fn read_scope_id(path: &Path) -> String {
    let scope: Value =
        serde_json::from_str(&fs::read_to_string(path.join(".kio/scope.json")).unwrap()).unwrap();
    scope["scope_id"].as_str().unwrap().to_owned()
}

/// The device replica is the only candidate source for a cross-scope search.
/// Its generation is included so cursor callers can freeze that corpus.
fn replica_collection_generation(response: &Value) -> &str {
    response["aggregator"]["collection_generation"]
        .as_str()
        .unwrap_or_else(|| panic!("replica collection generation is required: {response:#?}"))
}

/// Temporarily make a source index unavailable without losing it if a test
/// assertion fails.  Every fixture has its own directory, so the fixed sibling
/// backup name cannot collide with another test.  If a buggy command recreates
/// `sqlite.db`, restore intentionally removes that test-only replacement before
/// moving the original file back.
struct HiddenSourceIndex {
    original: PathBuf,
    backup: PathBuf,
}

impl HiddenSourceIndex {
    fn hide(original: &Path) -> Self {
        let backup = original.with_file_name("sqlite.db.replica-only-backup");
        assert!(
            !backup.exists(),
            "test backup path must be unused: {}",
            backup.display()
        );
        fs::rename(original, &backup).unwrap_or_else(|error| {
            panic!(
                "failed to hide source index {} for replica-only search: {error}",
                original.display()
            )
        });
        Self {
            original: original.to_owned(),
            backup,
        }
    }

    fn restore_inner(&self) -> std::io::Result<()> {
        if !self.backup.exists() {
            return Ok(());
        }
        if self.original.exists() {
            fs::remove_file(&self.original)?;
        }
        fs::rename(&self.backup, &self.original)
    }

    fn restore(self) {
        self.restore_inner().unwrap_or_else(|error| {
            panic!(
                "failed to restore source index {} after replica-only search: {error}",
                self.original.display()
            )
        });
    }
}

impl Drop for HiddenSourceIndex {
    fn drop(&mut self) {
        // This is the panic-path safety net. The normal test path calls
        // `restore` so a restoration failure remains visible to the test.
        let _ = self.restore_inner();
    }
}

/// Replace the test registry database file with an empty directory so
/// `RegistryDb::open` fails and search exercises its current-scope fallback.
/// The original registry stays next to it and is restored on both the normal
/// and panic paths.
struct UnavailableRegistry {
    original: PathBuf,
    backup: PathBuf,
}

impl UnavailableRegistry {
    fn block(original: &Path) -> Self {
        let backup = original.with_file_name("scope-registry.sqlite.unavailable-backup");
        assert!(
            original.is_file(),
            "test registry file must exist: {}",
            original.display()
        );
        assert!(
            !backup.exists(),
            "test registry backup path must be unused: {}",
            backup.display()
        );
        fs::rename(original, &backup).unwrap_or_else(|error| {
            panic!(
                "failed to hide registry file {} for fallback search: {error}",
                original.display()
            )
        });
        fs::create_dir(original).unwrap_or_else(|error| {
            panic!(
                "failed to replace registry file {} for fallback search: {error}",
                original.display()
            )
        });
        Self {
            original: original.to_owned(),
            backup,
        }
    }

    fn restore_inner(&self) -> std::io::Result<()> {
        if !self.backup.exists() {
            return Ok(());
        }
        if self.original.exists() {
            fs::remove_dir(&self.original)?;
        }
        fs::rename(&self.backup, &self.original)
    }

    fn restore(self) {
        self.restore_inner().unwrap_or_else(|error| {
            panic!(
                "failed to restore registry file {} after fallback search: {error}",
                self.original.display()
            )
        });
    }
}

impl Drop for UnavailableRegistry {
    fn drop(&mut self) {
        let _ = self.restore_inner();
    }
}

fn replace_scope_id(path: &Path, scope_id: &str) {
    let scope_path = path.join(".kio/scope.json");
    let mut scope: Value = serde_json::from_str(&fs::read_to_string(&scope_path).unwrap()).unwrap();
    scope["scope_id"] = serde_json::json!(scope_id);
    fs::write(scope_path, serde_json::to_vec_pretty(&scope).unwrap()).unwrap();
    // Fixture-only identity pinning before enrollment/index. A management
    // record is part of the authority; changing portable scope.json alone
    // deliberately invalidates membership in v1.
    let record_path = path.join(".kio/management.json");
    let mut record: Value = serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
    assert_eq!(record["authority"]["kind"], "root");
    assert!(record["children"].as_object().unwrap().is_empty());
    record["scope_id"] = serde_json::json!(scope_id);
    fs::write(record_path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
}

fn indexed_scope() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("auth.md"),
        "# 認証仕様\n\n## API Token\nトークン TTL は 3600 秒です。\n\n## Scopes\nスコープは read write admin です。\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("ranking.md"),
        "# 検索ランキング\n\n## RRF 融合\nRRF の定数 k=60 を使います。\n\n## MMR 多様化\nMMR の係数 lambda 0.7 で多様化します。\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    json_success(&dir, &["index"]);
    dir
}

// Ranking needs alternatives. Keep this corpus in the test binary and index it
// exactly once: the contract is about the order of a realistic candidate pool,
// not about repeatedly measuring `kio index`.
struct RankingFixture {
    dir: TempDir,
}

static RANKING_FIXTURE: OnceLock<RankingFixture> = OnceLock::new();

fn ranking_fixture() -> &'static RankingFixture {
    RANKING_FIXTURE.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let documents = [
            ("target-short.md", "# 廃止手続き\n\n管理画面で承認して完了します。\n"),
            ("target-japanese.md", "# 認証トークンの期限\n\n認証トークンの有効期限は 3,600 秒です。認証トークンの有効期限は 3,600 秒です。\n"),
            ("target-path.md", "# Token cache\n\n`src/auth/token.rs` `src/auth/token.rs` で TokenCache を更新します。\n"),
            ("target-mixed.md", "# API v2 認証\n\nAPI v2 認証 API v2 認証は service-token ヘッダーを使います。\n"),
            ("target-number.md", "# 請求上限\n\n請求上限は 3,600 件です。請求上限は 3,600 件です。\n"),
            ("legacy-format-v0.md", "# 旧フォーマット v0 仕様\n\n## 廃止バージョン\n\nv0.1.0 は廃止済み。kio_format_version に統一された。\n\n## 廃止フィールド\n\n旧フィールド tree_id / commit_id は廃止された。\n"),
            // Pure-short ranking first orders by the token's position. Keep
            // this a genuine competing hit without tying that observable and
            // accidentally making the assertion depend on chunk-hash order.
            ("deprecated-approach.md", "# 検索で廃止した手法\n\n## 旧手法\n\n旧手法は TF-IDF、語彙次元は 30,000 だった。\n\n## 結果\n\n旧手法の Recall@10 は 0.52 に留まった。\n"),
            ("vendor-eval.md", "# ベンダー評価メモ\n\n## コスト評価\n\nベンダー A の年間見積は 320万円 だった。\n\n## SLA 評価\n\n提示 SLA は 99.9%、クレジットは 10%。\n"),
            ("leaked-draft-pricing.md", "# 価格改定ドラフト (誤取込)\n\n## 旧価格\n\n旧価格は 1,000 トークンあたり 0.30 USD だった。\n\n## 割引\n\n年契約割引は 40% を提示していた。\n"),
            ("falcon-old-schema.md", "# Falcon 旧スキーマ\n\n## 旧テーブル\n\n旧スキーマは 28 テーブル構成だった。\n\n## 旧インデックス\n\n旧インデックスは B-tree のみで 9 本。\n"),
            ("kestrel-poc-metrics.md", "# Kestrel PoC 計測\n\n## PoC レイテンシ\n\nPoC の p95 は 1,900ms と遅かった。\n\n## PoC コスト\n\nPoC 期間の月額コストは 68万円 だった。\n"),
        ];
        for (name, body) in documents {
            fs::write(dir.path().join(name), body).unwrap();
        }
        for n in 0..30 {
            let body = format!(
                "# Filler {n}\n\narchive lantern meadow quartz river signal tapestry umbrella.\n"
            );
            fs::write(dir.path().join(format!("filler-{n:02}.md")), body).unwrap();
        }
        kio(&dir, &["init"]).assert().success();
        json_success(&dir, &["index"]);
        RankingFixture { dir }
    })
}

fn copy_tree(source: &Path, destination: &Path) {
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if source_path.is_dir() {
            fs::create_dir(&destination_path).unwrap();
            fs::set_permissions(
                &destination_path,
                fs::metadata(&source_path).unwrap().permissions(),
            )
            .unwrap();
            copy_tree(&source_path, &destination_path);
        } else {
            fs::copy(source_path, destination_path).unwrap();
        }
    }
}

fn ranking_search(query: &str) -> Value {
    let fixture = ranking_fixture();
    // Search creates a cursor-signing key in XDG data. Each test gets a copy
    // of the once-indexed corpus so parallel test threads cannot race on it.
    let copy = tempfile::tempdir().unwrap();
    copy_tree(fixture.dir.path(), copy.path());
    json_success(&copy, &["search", query, "--mode", "text", "--limit", "10"])
}

fn assert_ranked_first(search: &Value, target: &str) {
    let results = search["results"].as_array().unwrap();
    assert!(
        results[0]["evidence_pointer"]["path_at_commit"] == target,
        "{target} must outrank every distractor: {search}"
    );
}

fn result_path(result: &Value) -> &str {
    result["evidence_pointer"]["path_at_commit"]
        .as_str()
        .unwrap()
}

// Ranking contract layer: each query has deliberately constructed distractors
// that share its words but do not answer it. These assertions are intentionally
// top-1 assertions; a mere non-empty result would recreate the old blind spot.
#[test]
fn ranking_short_japanese_query_beats_abolition_distractors() {
    assert_ranked_first(&ranking_search("廃止"), "target-short.md");
}

#[test]
fn ranking_natural_japanese_query_beats_token_distractors() {
    assert_ranked_first(
        &ranking_search("認証トークンの有効期限は何秒ですか"),
        "target-japanese.md",
    );
}

#[test]
fn ranking_identifier_path_query_beats_neighboring_source_paths() {
    assert_ranked_first(&ranking_search("src/auth/token.rs"), "target-path.md");
}

#[test]
fn ranking_mixed_script_query_beats_api_operation_distractors() {
    assert_ranked_first(&ranking_search("API v2 認証"), "target-mixed.md");
}

#[test]
fn ranking_grouped_and_ungrouped_numbers_choose_the_same_answer() {
    let grouped = ranking_search("3,600");
    let plain = ranking_search("3600");
    assert_ranked_first(&grouped, "target-number.md");
    assert_ranked_first(&plain, "target-number.md");
    assert_eq!(
        result_path(&grouped["results"][0]),
        result_path(&plain["results"][0]),
        "3,600 and 3600 must not select different answers"
    );
}

#[test]
fn ranking_long_legacy_format_query_beats_heterogeneous_one_word_distractors() {
    let search = ranking_search("廃止した旧フォーマット v0.1.0 の仕様書");
    // Top-1 alone did not expose the measured word-lane regression: its noisy
    // candidates could enter below the target. Precision is the contract here.
    assert_ranked_first(&search, "legacy-format-v0.md");
    let mut paths = search["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(result_path)
        .collect::<Vec<_>>();
    assert!(
        paths.iter().all(|path| !path.starts_with("filler-")),
        "a filler has no query term and must never be retrieved: {search}"
    );
    paths.sort_unstable();
    paths.dedup();
    assert_eq!(
        paths.as_slice(),
        ["legacy-format-v0.md"],
        "this query's result set must contain only its legacy-format document: {search}"
    );
}

// K4 embedding seam helpers: run `kio` with the deterministic adapter mock.
fn json_success_embed(dir: &TempDir, embed: &str, args: &[&str]) -> Value {
    let output = kio(dir, args)
        .env(TEST_ADOPTED_EMBEDDING_ENV, embed)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

fn json_failure_embed(dir: &TempDir, embed: &str, args: &[&str], code: i32) -> Value {
    let output = kio(dir, args)
        .env(TEST_ADOPTED_EMBEDDING_ENV, embed)
        .arg("--json")
        .assert()
        .code(code)
        .get_output()
        .stderr
        .clone();
    serde_json::from_slice(&output).unwrap()
}

/// R11-2: an embedding-adapter run that exits non-zero (auth 5 / budget 6) while
/// still printing its full result JSON to stdout (the search "result + nonzero"
/// shape). Asserts the exit code and returns the stdout payload.
fn json_code_stdout_embed(dir: &TempDir, embed: &str, code: i32, args: &[&str]) -> Value {
    let output = kio(dir, args)
        .env(TEST_ADOPTED_EMBEDDING_ENV, embed)
        .arg("--json")
        .assert()
        .code(code)
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

fn json_success_embed_at(dir: &TempDir, embed: &str, fixed_now: &str, args: &[&str]) -> Value {
    let output = kio(dir, args)
        .env(TEST_ADOPTED_EMBEDDING_ENV, embed)
        .env("KIO_FIXED_NOW", fixed_now)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

/// Paid adapters must opt into a separately initialized device ledger.  Keep
/// this explicit in fixtures: scope initialization only creates `.kio`.
fn initialize_paid_ledger(dir: &TempDir) {
    kio(dir, &["ledger", "init"]).assert().success();
}

/// Grant a configured embedding adapter before an online fixture executes it.
/// Keeping this separate from `index` ensures the fixture cannot accidentally
/// depend on an index-side consent path.
fn approve_embed(dir: &TempDir, embed: &str) {
    json_success_embed(dir, embed, &["adapter", "approve", "--all", "--yes"]);
}

fn run_embed_path(path: &Path, data_home: &Path, embed: &str, args: &[&str]) -> Value {
    let output = hermetic_kio_command()
        .current_dir(path)
        .env("XDG_CONFIG_HOME", data_home.join("config"))
        .env("XDG_DATA_HOME", data_home.join("data"))
        .env("XDG_CACHE_HOME", data_home.join("cache"))
        .env(TEST_ADOPTED_EMBEDDING_ENV, embed)
        .args(args)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

/// The `indexed_scope` fixture indexed with a configured embedding adapter.
fn indexed_scope_embed(embed: &str) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("auth.md"),
        "# 認証仕様\n\n## API Token\nトークン TTL は 3600 秒です。\n\n## Scopes\nスコープは read write admin です。\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("ranking.md"),
        "# 検索ランキング\n\n## RRF 融合\nRRF の定数 k=60 を使います。\n\n## MMR 多様化\nMMR の係数 lambda 0.7 で多様化します。\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_embed(&dir, embed);
    json_success_embed(&dir, embed, &["index"]);
    dir
}

fn chunk_hash_set(search: &Value) -> std::collections::BTreeSet<String> {
    search["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|result| result["chunk_hash"].as_str().unwrap().to_owned())
        .collect()
}

fn first_result(search: &Value) -> &Value {
    &search["results"].as_array().unwrap()[0]
}

// CT3-HYBRID-001 (scenario (d)): with a configured, compatible embedding index,
// auto resolves to hybrid, RRF fuses text+vector, MMR runs on real embeddings, and
// the fallback fields are clean.
#[test]
fn ct3_hybrid_001_auto_resolves_to_hybrid_with_rrf_fusion() {
    let dir = indexed_scope_embed("mock");
    let hybrid = json_success_embed(&dir, "mock", &["search", "認証仕様 トークン"]);
    assert_eq!(hybrid["requested_mode"], "auto");
    assert_eq!(hybrid["resolved_mode"], "hybrid");
    assert_eq!(hybrid["fallback"], false);
    assert!(hybrid["fallback_reason"].is_null());
    // MMR ran on real embeddings — impossible in text-only, which reports
    // "group_by_raw_hash" (the K2 honesty fix). This is the vector-supply proof.
    assert_eq!(hybrid["diversify"]["strategy"], "mmr");
    // Vector recall contributes candidates the text backend never matched: the
    // hybrid result set is a strict superset of the text-only set (RRF fusion —
    // the order/content genuinely changes vs text alone).
    let text = json_success_embed(
        &dir,
        "mock",
        &["search", "認証仕様 トークン", "--mode", "text"],
    );
    let hybrid_set = chunk_hash_set(&hybrid);
    let text_set = chunk_hash_set(&text);
    assert!(
        hybrid_set.len() > text_set.len(),
        "hybrid must add vector-only candidates"
    );
    assert!(text_set.iter().all(|chunk| hybrid_set.contains(chunk)));
}

// CT3-HYBRID-002 (de-tautologized, pair-discriminating): the fallback_reason names
// the ACTUAL cause, and the same scope+query flips to hybrid once the cause is
// removed — so the assertion cannot pass against a permanently-degraded stub.
#[test]
fn ct3_hybrid_002_auto_vector_configured_but_absent_falls_back_visibly() {
    let dir = indexed_scope(); // indexed without an embedding adapter → no vectors
    initialize_paid_ledger(&dir);
    approve_embed(&dir, "mock");
    // (a) endpoint truly unconfigured → the 05 §1.7 example string.
    // "off" is an unrecognized seam value → no adapter, regardless of any
    // ambient GEMINI_API_KEY.
    let unconfigured = json_success_embed(&dir, "off", &["search", "トークン TTL 3600"]);
    assert_eq!(unconfigured["resolved_mode"], "text");
    assert_eq!(
        unconfigured["fallback_reason"],
        "embedding_endpoint_not_configured"
    );
    // (b) endpoint configured (mock) but this scope carries no embeddings →
    // the precise cause, not the generic endpoint string.
    let search = json_success_embed(&dir, "mock", &["search", "トークン TTL 3600"]);
    assert_eq!(search["requested_mode"], "auto");
    assert_eq!(search["resolved_mode"], "text");
    assert_eq!(search["fallback"], true);
    assert_eq!(search["fallback_reason"], "embedding_index_missing");
    assert_eq!(search["error_code"], "KIO-E-SEARCH-VEC-UNAVAIL-001");
    // (c) pair-discrimination: embed the same scope, then the SAME query resolves
    // hybrid with the fallback gone.
    json_success_embed(&dir, "mock", &["index"]);
    let hybrid = json_success_embed(&dir, "mock", &["search", "トークン TTL 3600"]);
    assert_eq!(hybrid["resolved_mode"], "hybrid");
    assert_eq!(hybrid["fallback"], false);
    assert!(hybrid["fallback_reason"].is_null());
    assert!(hybrid["error_code"].is_null());
}

#[test]
fn ct3_hybrid_006_text_mode_uses_text_rank_without_fusion() {
    let dir = indexed_scope();
    let search = json_success(&dir, &["search", "RRF k 60", "--mode", "text"]);
    assert_eq!(search["requested_mode"], "text");
    assert_eq!(search["resolved_mode"], "text");
    assert_eq!(search["fallback"], false);
    assert_eq!(
        first_result(&search)["evidence_pointer"]["section_id"],
        "検索ランキング/rrf-融合"
    );
}

// F2: NFD (decomposed) body content must be found by an NFC (composed) query.
// The FTS index projection and the CLI query are both normalized to NFC, so
// canonically-equivalent forms match regardless of input form. Before the fix
// the trigram index kept the raw NFD bytes and an NFC query returned 0 results
// (a silent false negative — exit 0, empty).
#[test]
fn f2_nfd_body_is_found_by_nfc_query() {
    let dir = tempfile::tempdir().unwrap();
    // Body word "café" written in NFD: "cafe" + U+0301 COMBINING ACUTE ACCENT.
    fs::write(
        dir.path().join("menu.md"),
        "# メニュー\n\n## Cafe\nThe cafe\u{301} latte special is served every morning.\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    json_success(&dir, &["index"]);

    // Composed (NFC) query "café": the byte substring is absent from the NFD
    // content, so only the normalized index projection makes this hit.
    let search = json_success(&dir, &["search", "caf\u{e9} latte", "--mode", "text"]);
    assert!(
        !search["results"].as_array().unwrap().is_empty(),
        "NFC query must find NFD-stored content: {search}"
    );
}

// CT3-EMBED-002 (de-tautologized): a genuinely incompatible embedding profile in
// the index. auto → text fallback with INCOMPAT; --vector → hard error INCOMPAT
// (does not fall back, distinct from UNAVAIL).
#[test]
fn ct3_embed_002_incompatible_profile_falls_back_or_errors() {
    let dir = indexed_scope_embed("mock");
    // A foreign/stale vector profile is derived index state.  Do not attempt to
    // authorize a foreign runtime tool identity merely to create this fixture.
    let conn = index_db(&dir);
    assert!(
        conn.execute(
            "UPDATE embeddings SET profile_hash = ?1 WHERE target_type = 'chunk'",
            ["sha256:00000000000000000000000000000000000000000000000000000000incompat"],
        )
        .unwrap()
            > 0
    );
    drop(conn);
    let replica =
        rusqlite::Connection::open(dir.path().join(".test-cache/kio/aggregator.sqlite")).unwrap();
    let profiles_json: String = replica
        .query_row(
            "SELECT embedding_profiles_json FROM agg_scopes",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let mut profiles: Value = serde_json::from_str(&profiles_json).unwrap();
    profiles[0]["profile_hash"] = serde_json::json!(
        "sha256:00000000000000000000000000000000000000000000000000000000incompat"
    );
    assert_eq!(
        replica
            .execute(
                "UPDATE agg_scopes SET embedding_profiles_json = ?1",
                [serde_json::to_string(&profiles).unwrap()],
            )
            .unwrap(),
        1
    );
    drop(replica);
    let auto = json_success_embed(&dir, "mock", &["search", "トークン TTL 3600"]);
    assert_eq!(auto["resolved_mode"], "text");
    assert_eq!(auto["fallback"], true);
    assert_eq!(auto["error_code"], "KIO-E-SEARCH-VEC-INCOMPAT-001");
    // R23-14(a) (05 §1.8 L425 / 06 §7 L330): explicit --vector's INCOMPAT
    // hard error is exit 8 (IncompatibleProfile), not the generic exit 1.
    let err = json_failure_embed(&dir, "mock", &["search", "トークン", "--mode", "vector"], 8);
    assert_eq!(err["error_code"], "KIO-E-SEARCH-VEC-INCOMPAT-001");
}

// CT3-EMBED-007: --vector with no embedding index at all is a hard error (UNAVAIL,
// not a fallback). Distinct code from the incompatible case above.
#[test]
fn ct3_embed_007_vector_only_without_index_is_an_error() {
    let dir = indexed_scope();
    let err = json_failure_embed(&dir, "mock", &["search", "トークン", "--mode", "vector"], 1);
    assert_eq!(err["error_code"], "KIO-E-SEARCH-VEC-UNAVAIL-001");
}

// R11-7: the `[search]` config (default_mode / fail_behavior) was schema-valid and
// documented but entirely unwired (the [search] version of R10-2 config drift). A
// text-only scope (no embedding adapter → vector unavailable) exercises all three.
fn write_search_config(dir: &TempDir, body: &str) {
    fs::write(
        dir.path().join(".kio/config.toml"),
        format!("[search]\n{body}"),
    )
    .unwrap();
}

#[test]
fn r11_7_default_mode_config_seeds_requested_mode() {
    let dir = indexed_scope();
    write_search_config(&dir, "default_mode = \"hybrid\"\n");
    // PC49/PC50 (05 §1.8 L384-387): folder config.toml applies only for a
    // single, non-`--descendants` `--scope` — a multi-scope search (the bare
    // default used here previously) now uses the user (device) layer only.
    // `--scope .` keeps this test's folder-config premise valid under the new
    // rule.
    // No CLI mode flag → the config default_mode is adopted as requested_mode
    // (previously ignored: requested_mode was always the hardcoded "auto").
    let search = json_success(&dir, &["search", "トークン TTL", "--scope", "."]);
    assert_eq!(search["requested_mode"], "hybrid");
    // An explicit flag still wins over the config default.
    let text = json_success(
        &dir,
        &["search", "トークン TTL", "--mode", "text", "--scope", "."],
    );
    assert_eq!(text["requested_mode"], "text");
}

#[test]
fn r11_7_fail_behavior_error_makes_hybrid_hard_error() {
    let dir = indexed_scope();
    write_search_config(&dir, "fail_behavior = \"error\"\n");
    // PC49/PC50: `--scope .` (single scope) keeps folder config effective —
    // see `r11_7_default_mode_config_seeds_requested_mode`.
    // --hybrid with no vector backend + fail_behavior=error is now the same hard
    // error the explicit --vector path returns, not a silent exit-0 text fallback.
    let err = json_failure(
        &dir,
        &["search", "トークン TTL", "--mode", "hybrid", "--scope", "."],
        1,
    );
    assert_eq!(err["error_code"], "KIO-E-SEARCH-VEC-UNAVAIL-001");
}

#[test]
fn r11_7_fail_behavior_warn_falls_back_with_warnings() {
    let dir = indexed_scope();
    write_search_config(&dir, "fail_behavior = \"warn\"\n");
    // PC49/PC50: `--scope .` (single scope) keeps folder config effective —
    // see `r11_7_default_mode_config_seeds_requested_mode`.
    let search = json_success(
        &dir,
        &["search", "トークン TTL", "--mode", "hybrid", "--scope", "."],
    );
    assert_eq!(search["resolved_mode"], "text");
    assert_eq!(search["fallback"], true);
    // PC3 / §R note-1 ruling (2026-07-22): `warnings[]` array, replacing the
    // pre-ruling singular `warning` field.
    let warnings = search["warnings"]
        .as_array()
        .expect("warnings must be an array");
    assert!(
        warnings.iter().any(|w| w
            .as_str()
            .is_some_and(|w| w.contains("vector search unavailable"))),
        "warn must surface a warnings[] entry: {search}"
    );
}

#[test]
fn ct3_evidence_001_search_results_include_pointer_and_uri() {
    let dir = indexed_scope();
    let search = json_success(&dir, &["search", "トークン TTL 3600"]);
    let result = first_result(&search);
    let pointer = &result["evidence_pointer"];
    // 08 §2.1 required 6: schema_version / commit / raw_hash / tool_profile_hash /
    // chunk_hash / scope_id — all 6, not a subset, at the actual `--json` output
    // level (the response is hand-assembled JSON in the CLI, not a struct
    // serialization, so field presence must be checked here and not assumed from
    // the kio-search type definitions).
    assert_eq!(pointer["schema_version"], 1);
    assert!(pointer["commit"].as_str().unwrap().starts_with("sha256:"));
    assert!(pointer["raw_hash"].as_str().unwrap().starts_with("sha256:"));
    assert!(
        pointer["tool_profile_hash"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert!(
        pointer["chunk_hash"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    // scope_id is a bare 26-char Crockford-base32 ULID (kio-core::scope::is_ulid),
    // not "scope_"-prefixed — that prefix only appears in the spec doc's
    // illustrative fixture strings.
    assert_eq!(pointer["scope_id"].as_str().unwrap().len(), 26);
    // M3-1 completion condition: search-issued pointers additionally carry
    // heading_path + span.
    assert_eq!(pointer["heading_path"][1], "API Token");
    assert!(pointer["byte_start"].as_u64().is_some());
    assert!(pointer["byte_end"].as_u64().is_some());
    assert!(
        result["evidence_uri"]
            .as_str()
            .unwrap()
            .starts_with("kio://")
    );
}

#[test]
fn ct3_evidence_002_live_pointer_commit_matches_searched_scope_snapshot() {
    let dir = indexed_scope();
    let search = json_success(&dir, &["search", "トークン TTL 3600"]);
    let snapshot = search["searched_scopes"][0]["snapshot_at"]
        .as_str()
        .unwrap();
    assert_eq!(
        first_result(&search)["evidence_pointer"]["commit"]
            .as_str()
            .unwrap(),
        snapshot
    );
}

#[test]
fn ct3_evidence_009_eval_reads_raw_hash_and_section_from_pointer() {
    let dir = indexed_scope();
    let search = json_success(&dir, &["search", "トークン TTL 3600"]);
    let pointer = &first_result(&search)["evidence_pointer"];
    assert!(pointer["raw_hash"].as_str().unwrap().starts_with("sha256:"));
    assert_eq!(pointer["section_id"], "認証仕様/api-token");
}

#[test]
fn ct3_open_001_open_prefers_working_tree_raw_hash() {
    let dir = indexed_scope();
    let search = json_success(&dir, &["search", "トークン TTL 3600"]);
    let uri = first_result(&search)["evidence_uri"].as_str().unwrap();
    let opened = json_success(&dir, &["open", uri]);
    assert_eq!(opened["status"], "opened");
    assert_eq!(opened["temporary"], false);
    assert!(opened["path"].as_str().unwrap().ends_with("auth.md"));

    let human = kio(&dir, &["open", uri])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(
        String::from_utf8(human).unwrap().trim_end(),
        opened["path"].as_str().unwrap()
    );
}

#[test]
fn ct3_open_004_view_returns_chunk_text_without_regeneration() {
    let dir = indexed_scope();
    let search = json_success(&dir, &["search", "トークン TTL 3600"]);
    let uri = first_result(&search)["evidence_uri"].as_str().unwrap();
    let viewed = json_success(&dir, &["view", uri]);
    assert!(view_slice(&viewed).contains("トークン TTL"));
}

// R11-9: `view --json` must expose the same `temporary` field as `open --json`,
// since both resolve the identical pointer and Agents branch on it.
#[test]
fn r11_9_view_json_exposes_temporary_field() {
    let dir = indexed_scope();
    let search = json_success(&dir, &["search", "トークン TTL 3600"]);
    let uri = first_result(&search)["evidence_uri"]
        .as_str()
        .unwrap()
        .to_owned();
    // Working-tree file present -> resolves from raw, not a temporary copy.
    let viewed = json_success(&dir, &["view", &uri]);
    assert_eq!(viewed["temporary"], false);
    // Removing the working-tree file forces a temporary expansion, and `view`
    // must surface it just like `open` does (ct3_open_002).
    fs::remove_file(dir.path().join("auth.md")).unwrap();
    let viewed_temp = json_success(&dir, &["view", &uri]);
    assert_eq!(viewed_temp["temporary"], true);
}

// 05 §1.7.2 / §4.2 (2026-08-11) acceptance requirement: a chunk sitting in the
// FIRST unit of a view can pass its offset translation even with the header
// length forgotten entirely, or the "\n\n" join width miscounted, purely by
// accident -- there is nothing accumulated before it for such a bug to get
// wrong. A chunk in a LATER unit is the only fixture shape that actually
// forces `view`'s offset math to accumulate across a prior unit's content, so
// this indexes a two-page PDF (each page becomes its own normalized unit,
// `ct2_pdf_001` in step2_p0_contract.rs) and resolves a chunk that only
// exists on page 2.
#[test]
fn view_offset_translation_is_correct_for_a_chunk_past_the_first_unit() {
    use kio_core::cas::ObjectStore;

    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("report.pdf"),
        fake_pdf(&[
            "pageonefirstunique filler content, not the search target",
            "pagetwosecondunique marker content for view offset translation",
        ]),
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    json_success(&dir, &["index"]);

    let search = json_success(&dir, &["search", "pagetwosecondunique"]);
    let result = first_result(&search);
    let chunk_hash = result["chunk_hash"].as_str().unwrap().to_owned();
    let uri = result["evidence_uri"].as_str().unwrap().to_owned();

    // Confirm the fixture actually landed the hit on the SECOND unit -- the
    // precondition this test exists to cover -- checked directly against the
    // chunk's own CAS object rather than merely inferred from ranking.
    let kio_dir = dir.path().join(".kio");
    let chunk = ObjectStore::new(&kio_dir).read_chunk(&chunk_hash).unwrap();
    assert_eq!(
        chunk.unit_key, "page:2",
        "fixture must resolve to the second unit for this test to mean anything: {chunk:?}"
    );

    let viewed = json_success(&dir, &["view", &uri]);
    let view_path = viewed["view_path"].as_str().unwrap();
    let start = viewed["view_byte_start"].as_u64().unwrap() as usize;
    let end = viewed["view_byte_end"].as_u64().unwrap() as usize;
    let text = fs::read_to_string(view_path).unwrap();

    assert!(
        text[start..end].contains("pagetwosecondunique"),
        "chunk did not slice to its own content: {:?}",
        &text[start..end]
    );
    // The discriminating check: everything BEFORE the resolved span must
    // contain page 1's ENTIRE content. A layout bug that reports every
    // unit's start as just the header length (correct only for the FIRST
    // unit) would place `start` too early here and truncate page 1 out of
    // this prefix.
    assert!(
        text[..start].contains("pageonefirstunique filler content, not the search target"),
        "view_byte_start did not skip over the first unit's full content: {:?}",
        &text[..start]
    );
    // And the page-2 marker must not itself leak into the prefix -- otherwise
    // `start` could be sitting anywhere inside page 1 and still pass the
    // loose "contains" check above.
    assert!(!text[..start].contains("pagetwosecondunique"));
}

// 05 §1.7.2 / §4.2 (2026-08-11): when the normalized instance a chunk's view
// is assembled from cannot be read (GC'd, purged, or otherwise gone), `view`
// must still resolve the pointer -- the chunk and the raw document are
// independent CAS objects -- but degrade `view_path`/`view_byte_start`/
// `view_byte_end` to null, the same posture as `commit_shallow`/
// `manifest_missing`. There is no Step 3 GC command to drive this through, so
// this hand-places the precondition directly (removing the normalized
// instance directory `load_validated_normalized_instance` reads), the same
// style `ct3_evidence_005_shallow_commit_resolves_directly` already uses to
// simulate a GC'd tree object.
#[test]
fn view_degrades_view_fields_to_null_when_the_normalized_instance_is_unreadable() {
    use kio_core::cas::ObjectStore;
    use kio_pipeline::markdownize::normalized_instance_dir;

    let dir = indexed_scope();
    let search = json_success(&dir, &["search", "トークン TTL 3600"]);
    let result = first_result(&search);
    let pointer = result["evidence_pointer"].clone();
    let chunk_hash = result["chunk_hash"].as_str().unwrap().to_owned();
    let kio_dir = dir.path().join(".kio");

    let chunk = ObjectStore::new(&kio_dir).read_chunk(&chunk_hash).unwrap();
    let instance_dir = normalized_instance_dir(
        &kio_dir,
        &chunk.raw_hash,
        &chunk.tool_profile_hash,
        chunk.r#gen,
    );
    assert!(
        instance_dir.is_dir(),
        "fixture precondition: {instance_dir:?}"
    );
    fs::remove_dir_all(&instance_dir).unwrap();

    let ptr = pointer.to_string();
    let viewed = json_success(&dir, &["view", &ptr]);
    assert_eq!(viewed["status"], "viewed", "{viewed}");
    assert!(
        viewed["path"].as_str().is_some(),
        "the raw document must still resolve independently of the view cache: {viewed}"
    );
    assert!(viewed["view_path"].is_null(), "{viewed}");
    assert!(viewed["view_byte_start"].is_null(), "{viewed}");
    assert!(viewed["view_byte_end"].is_null(), "{viewed}");
    // Not conflated with the pre-existing degradation axes: this scope's
    // commit still has its tree, and the tree-entry manifest binding is
    // untouched by deleting the *working* normalized instance directory.
    assert_eq!(viewed["commit_shallow"], false, "{viewed}");
    assert_eq!(viewed["manifest_missing"], false, "{viewed}");

    // open shares `resolve_pointer_for_cli` but never touches the view --
    // 08 §3.1's raw resolution must be completely unaffected.
    let opened = json_success(&dir, &["open", &ptr]);
    assert_eq!(opened["status"], "opened", "{opened}");
    assert!(opened["path"].as_str().unwrap().ends_with("auth.md"));
}

#[test]
fn ct3_open_003_scope_unreachable_returns_exit_3() {
    // QB1 (step4b-contract-tests-p3b.md §A, 06 §7 L370 / 10 §11.2 L931):
    // scope_unreachable is retryable (exit 3), distinct from a dead pointer
    // (tombstoned/not_found) within a reachable scope, which stays exit 4.
    let dir = indexed_scope();
    let bad = "kio://missing/sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc/sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
    let err = json_failure(&dir, &["open", bad], 3);
    assert_eq!(err["error_code"], "KIO-E-EVIDENCE-SCOPE-UNREACHABLE-001");
}

#[test]
fn ct3_cursor_001_same_cursor_recomputes_same_second_page() {
    let dir = indexed_scope();
    let first = json_success(&dir, &["search", "認証仕様", "--limit", "1"]);
    let cursor = first["paging"]["next_cursor"].as_str().unwrap();
    let second_a = json_success(
        &dir,
        &["search", "認証仕様", "--cursor", cursor, "--limit", "1"],
    );
    let second_b = json_success(
        &dir,
        &["search", "認証仕様", "--cursor", cursor, "--limit", "1"],
    );
    assert_eq!(second_a["results"], second_b["results"]);
    // Page 2 is a distinct chunk from page 1 (deterministic recompute + skip).
    assert_ne!(first["results"], second_a["results"]);
}

#[test]
fn ct3_cursor_003_mismatched_cursor_is_rejected() {
    let dir = indexed_scope();
    let first = json_success(&dir, &["search", "認証仕様", "--limit", "1"]);
    let cursor = first["paging"]["next_cursor"].as_str().unwrap();
    let err = json_failure(
        &dir,
        &[
            "search",
            "検索ランキング",
            "--cursor",
            cursor,
            "--limit",
            "1",
        ],
        2,
    );
    assert_eq!(err["error_code"], "KIO-E-SEARCH-CURSOR-001");
}

#[test]
fn ct3_multi_004_single_scope_response_lists_searched_scopes() {
    let dir = indexed_scope();
    let search = json_success(&dir, &["search", "トークン"]);
    let searched = search["searched_scopes"].as_array().unwrap();
    assert_eq!(searched.len(), 1);
    assert!(search["excluded_scopes"].as_array().unwrap().is_empty());
    // 05 §1.8: searched_scopes[] = {scope_id, scope_path, snapshot_at}, same
    // shape for a single-scope search as for multi-scope.
    // Bare 26-char ULID, see the ct3_evidence_001 scope_id note above.
    assert_eq!(searched[0]["scope_id"].as_str().unwrap().len(), 26);
    assert!(!searched[0]["scope_path"].as_str().unwrap().is_empty());
    assert!(
        searched[0]["snapshot_at"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
}

#[test]
fn ct3_obs_002_metrics_jsonl_records_per_search_latency() {
    let dir = indexed_scope();
    json_success(&dir, &["search", "トークン"]);
    let metrics = fs::read_to_string(dir.path().join(".test-data/kio/logs/metrics.jsonl")).unwrap();
    let last: Value = serde_json::from_str(metrics.lines().last().unwrap()).unwrap();
    assert_eq!(last["metric"], "search.latency_ms");
    assert_eq!(last["context"]["mode"], "text");
}

#[test]
fn ct3_obs_003_access_jsonl_records_redacted_search() {
    let dir = indexed_scope();
    json_success(&dir, &["search", "トークン"]);
    let access = fs::read_to_string(dir.path().join(".kio/logs/access.jsonl")).unwrap();
    let last: Value = serde_json::from_str(access.lines().last().unwrap()).unwrap();
    assert_eq!(last["context"]["query"], "[redacted]");
}

#[test]
fn ct3_reindex_003_force_requires_yes_in_noninteractive_mode() {
    let dir = indexed_scope();
    let err = json_failure(&dir, &["reindex", "--regenerate"], 9);
    assert_eq!(err["error_code"], "KIO-E-CONFIRM-REJECTED-001");
}

#[test]
fn ct3_reindex_001_force_creates_new_generation_and_preserves_old_chunks() {
    let dir = indexed_scope();
    let before = line_count(dir.path().join(".kio/index/chunks.jsonl"));
    let out = json_success(&dir, &["reindex", "--regenerate", "--yes"]);
    assert_eq!(out["status"], "reindexed");
    let after = line_count(dir.path().join(".kio/index/chunks.jsonl"));
    assert!(after > before);
}

#[test]
fn ct3_chunk_008_deleted_file_does_not_remove_existing_chunk_rows() {
    let dir = indexed_scope();
    let before = line_count(dir.path().join(".kio/index/chunks.jsonl"));
    fs::remove_file(dir.path().join("auth.md")).unwrap();
    json_success(&dir, &["index"]);
    let after = line_count(dir.path().join(".kio/index/chunks.jsonl"));
    assert!(after >= before);
}

#[test]
fn ct3_chunk_009_chunks_have_durable_publication_after_index() {
    let dir = indexed_scope();
    let text = fs::read_to_string(dir.path().join(".kio/index/chunks.jsonl")).unwrap();
    let row: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert!(row.get("chunking_config_introduction_commit").is_none());
    assert!(
        text.lines().skip(1).any(|line| {
            serde_json::from_str::<Value>(line)
                .ok()
                .is_some_and(|value| {
                    value["event"] == "publication"
                        && value["chunk_id"] == row["chunk_id"]
                        && value["chunking_config_hash"] == row["chunking_config_hash"]
                        && value["introduction_commit"]
                            .as_str()
                            .is_some_and(|commit| commit.starts_with("sha256:"))
                })
        }),
        "every indexed creation must have a durable publication event"
    );
}

// CT3-CHUNK-010: the tree_entries projection is asserted against the real
// `sqlite.db` written by `kio index` (the CLI projects tree_entries with its
// own SQL — `ensure_snapshot_tree_entries` / `write_tree_entries` /
// `rebuild_sqlite_index`; the former kio-index::tree_entries scaffold module
// was dead code and has been removed).
#[test]
fn ct3_chunk_010_head_tree_entries_are_populated_with_gen_after_index() {
    let dir = indexed_scope();
    let conn = rusqlite::Connection::open(dir.path().join(".kio/index/sqlite.db")).unwrap();
    let mut stmt = conn
        .prepare("SELECT commit_hash, path, raw_hash, gen FROM tree_entries")
        .unwrap();
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert!(!rows.is_empty(), "tree_entries must be populated for HEAD");
    // All rows belong to the single HEAD commit (the `indexed_scope` fixture
    // never reindexes), and gen defaults to 0 (04 §4.5 gen projection).
    let head_commit = rows[0].0.clone();
    assert!(rows.iter().all(|(commit, _, _, _)| *commit == head_commit));
    assert!(rows.iter().any(|(_, path, _, _)| path == "auth.md"));
    assert!(rows.iter().any(|(_, path, _, _)| path == "ranking.md"));
    assert!(rows.iter().all(|(_, _, _, r#gen)| *r#gen == 0));
    // The projection is what actually gates search (chunks ⨝ tree_entries(HEAD),
    // 05 §1.6): cross-check a live search result's raw_hash is one of the
    // projected rows, proving this table (not just the unused pure function) is
    // what search reads.
    let search = json_success(&dir, &["search", "トークン TTL 3600"]);
    let raw_hash = first_result(&search)["evidence_pointer"]["raw_hash"]
        .as_str()
        .unwrap();
    assert!(rows.iter().any(|(_, _, rh, _)| rh == raw_hash));
}

#[test]
fn ct3_chunk_012_repair_rebuild_db_preserves_search_result() {
    let dir = indexed_scope();
    let before = json_success(&dir, &["search", "トークン TTL 3600"]);
    fs::remove_file(dir.path().join(".kio/index/sqlite.db")).unwrap();
    json_success(&dir, &["repair", "rebuild-db"]);
    let after = json_success(&dir, &["search", "トークン TTL 3600"]);
    assert_eq!(
        first_result(&before)["evidence_uri"],
        first_result(&after)["evidence_uri"]
    );
    assert!(dir.path().join(".kio/index/sqlite.db").is_file());
}

// R11-4: `build_sqlite_index_at` now wraps all three rebuild loops (chunks,
// tree_entries, preserved embeddings) in one transaction. The rebuild must stay
// functionally identical — the FULL result set (every chunk, not just the top
// hit) must be byte-identical before and after a from-scratch rebuild.
#[test]
fn r11_4_transactional_rebuild_preserves_full_result_set() {
    let dir = indexed_scope();
    let before = json_success(&dir, &["search", "認証仕様", "--limit", "20"]);
    let before_set = chunk_hash_set(&before);
    assert!(!before_set.is_empty(), "fixture must return chunks");
    fs::remove_file(dir.path().join(".kio/index/sqlite.db")).unwrap();
    json_success(&dir, &["repair", "rebuild-db"]);
    let after = json_success(&dir, &["search", "認証仕様", "--limit", "20"]);
    assert_eq!(before_set, chunk_hash_set(&after));
    assert_eq!(before["results"], after["results"]);
}

#[test]
fn ct3_uri_003_inline_json_pointer_is_accepted_by_view() {
    let dir = indexed_scope();
    let search = json_success(&dir, &["search", "トークン TTL 3600"]);
    let pointer = first_result(&search)["evidence_pointer"].to_string();
    let viewed = json_success(&dir, &["view", &pointer]);
    assert!(view_slice(&viewed).contains("3600"));
}

// CT3-URI-003 (gap fill): the `<pointer>` receiver has 5 prefix branches
// (`-` stdin / `kio://` / `{` inline JSON / `sha256:` short form / other ->
// exit 2). Only the `kio://` (ct3_open_001 etc.) and `{` (above) branches had
// a dedicated P0 test; `-` stdin and the exit-2 fallback were untested.
#[test]
fn ct3_uri_003_stdin_dash_prefix_is_accepted_by_view() {
    let dir = indexed_scope();
    let search = json_success(&dir, &["search", "トークン TTL 3600"]);
    let uri = first_result(&search)["evidence_uri"]
        .as_str()
        .unwrap()
        .to_owned();
    let output = kio(&dir, &["view", "-"])
        .arg("--json")
        .write_stdin(uri)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let viewed: Value = serde_json::from_slice(&output).unwrap();
    assert!(view_slice(&viewed).contains("3600"));
}

#[test]
fn ct3_uri_003_unrecognized_pointer_prefix_is_invalid_usage_exit_2() {
    let dir = indexed_scope();
    let err = json_failure(&dir, &["view", "not-a-pointer-at-all"], 2);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-USAGE-001");
}

#[test]
fn ct3_open_002_missing_working_tree_file_expands_temporary_copy() {
    let dir = indexed_scope();
    let search = json_success(&dir, &["search", "トークン TTL 3600"]);
    let uri = first_result(&search)["evidence_uri"].as_str().unwrap();
    fs::remove_file(dir.path().join("auth.md")).unwrap();
    let opened = json_success(&dir, &["open", uri]);
    assert_eq!(opened["temporary"], true);
    assert!(Path::new(opened["path"].as_str().unwrap()).is_file());
}

#[test]
fn ct3_reindex_002_existing_pointer_still_resolves_old_chunk_after_reindex() {
    let dir = indexed_scope();
    let search = json_success(&dir, &["search", "トークン TTL 3600"]);
    let uri = first_result(&search)["evidence_uri"]
        .as_str()
        .unwrap()
        .to_owned();
    json_success(&dir, &["reindex", "--regenerate", "--yes"]);
    let viewed = json_success(&dir, &["view", &uri]);
    assert!(view_slice(&viewed).contains("トークン TTL"));
}

#[test]
fn ct3_chunk_007_chunking_config_change_appends_new_generation_chunks() {
    let dir = indexed_scope();
    let before = line_count(dir.path().join(".kio/index/chunks.jsonl"));
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[chunking]\nstrategy = \"heading\"\nmax_chars = 25\n",
    )
    .unwrap();
    json_success(&dir, &["index"]);
    let after = line_count(dir.path().join(".kio/index/chunks.jsonl"));
    assert!(after > before);
}

// CT3-CHUNK-007 (gap fill): the test above only checks that chunks.jsonl grew
// (append-only), never the "検索対象は現行 chunking_config_hash の chunk のみ"
// clause (04 §4.4/§4.6, K8). The stale-generation chunk row must survive on
// disk but stop being served by search once a newer chunking_config_hash
// generation exists for the same content.
#[test]
fn ct3_chunk_007_search_only_serves_current_chunking_config_generation() {
    let dir = indexed_scope();
    let before = json_success(&dir, &["search", "トークン TTL 3600"]);
    let stale_chunk_hash = first_result(&before)["chunk_hash"]
        .as_str()
        .unwrap()
        .to_owned();

    // A much smaller max_chars forces this section to actually re-split, so the
    // new generation's chunk_hash (byte_start/byte_end shift) differs from the
    // stale one — this isn't a same-hash no-op reindex.
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[chunking]\nstrategy = \"heading\"\nmax_chars = 10\n",
    )
    .unwrap();
    json_success(&dir, &["index"]);

    let after = json_success(&dir, &["search", "トークン TTL 3600"]);
    let after_hashes = chunk_hash_set(&after);
    assert!(
        !after_hashes.is_empty(),
        "new-generation chunks must remain searchable"
    );
    assert!(
        !after_hashes.contains(&stale_chunk_hash),
        "stale chunking_config_hash generation leaked into search results"
    );
    // The stale row genuinely still exists on disk (append-only, CT3-CHUNK-008),
    // it just must not be served by search.
    let jsonl = fs::read_to_string(dir.path().join(".kio/index/chunks.jsonl")).unwrap();
    assert!(
        jsonl.contains(&stale_chunk_hash),
        "stale chunk row must not be deleted from disk"
    );
}

#[test]
fn ct4_current_config_association_is_added_for_deleted_history() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("historical.md"),
        "# Historical\n\nretained-history-config-fixture\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    json_success(&dir, &["index"]);

    let db_path = dir.path().join(".kio/index/sqlite.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let historical_chunk: String = conn
        .query_row(
            "SELECT chunk_id FROM chunks
             WHERE text LIKE '%retained-history-config-fixture%' LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    drop(conn);

    fs::remove_file(dir.path().join("historical.md")).unwrap();
    json_success(&dir, &["index"]);
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[chunking]\nstrategy = \"heading\"\nmax_chars = 5999\n",
    )
    .unwrap();
    json_success(&dir, &["index"]);

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let current_config: String = conn
        .query_row(
            "SELECT chunking_config_hash
             FROM chunk_config_generations
             ORDER BY association_rowid DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let current_association_count: u64 = conn
        .query_row(
            "SELECT COUNT(*) FROM chunk_config_generations
             WHERE chunk_id = ?1 AND chunking_config_hash = ?2",
            rusqlite::params![historical_chunk, current_config],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        current_association_count, 1,
        "a config-preserving boundary must append a current-config association even when the normalized instance is historical-only"
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM chunks WHERE chunk_id = ?1",
            rusqlite::params![historical_chunk],
            |row| row.get::<_, u64>(0),
        )
        .unwrap(),
        1,
        "the immutable chunk row is shared by both config generations"
    );
}

#[test]
fn ct4_historical_only_current_config_chunks_enqueue_embeddings() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("historical.md"),
        "# Historical\n\nretained embedding history alpha bravo charlie delta echo\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    json_success_embed(&dir, "mock", &["index", "--yes"]);
    let conn = rusqlite::Connection::open(dir.path().join(".kio/index/sqlite.db")).unwrap();
    let historical_raw: String = conn
        .query_row(
            "SELECT raw_hash FROM chunks WHERE text LIKE '%retained embedding history%' LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    drop(conn);

    fs::remove_file(dir.path().join("historical.md")).unwrap();
    json_success_embed(&dir, "mock", &["index", "--yes"]);
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[chunking]\nstrategy = \"heading\"\nmax_chars = 24\n",
    )
    .unwrap();
    json_success_embed(&dir, "mock", &["index", "--yes"]);

    let conn = rusqlite::Connection::open(dir.path().join(".kio/index/sqlite.db")).unwrap();
    let current_config: String = conn
        .query_row(
            "SELECT chunking_config_hash FROM chunk_config_generations
             ORDER BY association_rowid DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let mut statement = conn
        .prepare(
            "SELECT c.chunk_id FROM chunks c
             WHERE c.raw_hash = ?1
               AND EXISTS (
                   SELECT 1 FROM chunk_config_generations cg
                   WHERE cg.chunk_id = c.chunk_id AND cg.chunking_config_hash = ?2
               )",
        )
        .unwrap();
    let historical_current_ids = statement
        .query_map(rusqlite::params![historical_raw, current_config], |row| {
            row.get::<_, String>(0)
        })
        .unwrap()
        .collect::<std::result::Result<std::collections::BTreeSet<_>, _>>()
        .unwrap();
    assert!(!historical_current_ids.is_empty());

    let status = json_success_embed(&dir, "mock", &["status"]);
    let task_refs = tasks_of_type(&status, "embedding")
        .into_iter()
        .filter_map(|task| task["output_ref"].as_str())
        .collect::<std::collections::BTreeSet<_>>();
    for chunk_id in historical_current_ids {
        assert!(
            task_refs.contains(format!("embedding:{chunk_id}").as_str()),
            "every retained historical current-config chunk needs an embedding task: {status}"
        );
    }
}

#[test]
fn ct4_rebuild_rederives_historical_tree_projection_from_cas() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("cached.md"), "# C1\n\nfirst snapshot\n").unwrap();
    kio(&dir, &["init"]).assert().success();
    let first = json_success(&dir, &["index"]);
    let first_commit = first["commit_hash"].as_str().unwrap().to_owned();

    fs::write(dir.path().join("cached.md"), "# C2\n\nsecond snapshot\n").unwrap();
    let second = json_success(&dir, &["index"]);
    let second_commit = second["commit_hash"].as_str().unwrap();
    assert_ne!(first_commit, second_commit);

    // The old database is deliberately unavailable: both the ancestor and
    // current rows below must come from commit/tree CAS, not a copied cache.
    fs::remove_file(dir.path().join(".kio/index/sqlite.db")).unwrap();
    json_success(&dir, &["repair", "rebuild-db"]);
    let conn = rusqlite::Connection::open(dir.path().join(".kio/index/sqlite.db")).unwrap();
    for commit in [&first_commit, second_commit] {
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM tree_entries
                 WHERE commit_hash = ?1 AND path = 'cached.md'",
                rusqlite::params![commit],
                |row| row.get::<_, u64>(0),
            )
            .unwrap(),
            1,
            "atomic rebuild must retain immutable historical tree cache rows"
        );
    }
}

#[test]
fn ct4_rebuild_replaces_garbage_sqlite_without_using_it_as_truth() {
    let dir = indexed_scope();
    let path = dir.path().join(".kio/index/sqlite.db");
    fs::write(&path, b"not a sqlite database and not a recovery source").unwrap();

    json_success(&dir, &["repair", "rebuild-db"]);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let count = conn
        .query_row("SELECT COUNT(*) FROM tree_entries", [], |row| {
            row.get::<_, u64>(0)
        })
        .unwrap();
    assert!(
        count > 0,
        "fresh derived projection must replace garbage sqlite"
    );
}

#[test]
fn ct4_rebuild_does_not_carry_tree_rows_that_exist_only_in_old_sqlite() {
    let dir = indexed_scope();
    let path = dir.path().join(".kio/index/sqlite.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "INSERT INTO tree_entries(commit_hash, path, raw_hash, tool_profile_hash, gen)
         VALUES (?1, 'sqlite-only.md', ?2, NULL, 0)",
        rusqlite::params![
            format!("sha256:{}", "f".repeat(64)),
            format!("sha256:{}", "e".repeat(64))
        ],
    )
    .unwrap();
    drop(conn);

    json_success(&dir, &["repair", "rebuild-db"]);
    let rebuilt = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        rebuilt
            .query_row(
                "SELECT COUNT(*) FROM tree_entries WHERE path = 'sqlite-only.md'",
                [],
                |row| row.get::<_, u64>(0),
            )
            .unwrap(),
        0,
        "SQLite-only projection rows must not become rebuild input"
    );
}

#[test]
fn ct4_rebuild_rejects_a_detached_tag_outside_the_linear_history() {
    let dir = indexed_scope();
    let repo = Repository::open(dir.path()).unwrap();
    let head = repo.head_commit_hash().unwrap().unwrap();
    let base = repo.read_commit(&head).unwrap();
    // This commit is deliberately disconnected from HEAD. Its tree is real
    // immutable CAS, and the sole current ref is a canonical tag.
    let detached = CommitObject::new(
        base.tree.clone(),
        None,
        base.created_at.clone(),
        "tag-only detached projection root".to_owned(),
        base.tool_lock_hash.clone(),
        base.stats.clone(),
        base.commit_type,
    )
    .unwrap();
    let (tag_only, _) = ObjectStore::new(repo.kio_dir())
        .write_json(ObjectKind::Commit, &serde_json::to_value(detached).unwrap())
        .unwrap();
    let tag_path = repo
        .kio_dir()
        .join("refs/tags-v1")
        .join(portable_tag_leaf("detached-root"));
    fs::write(tag_path, &tag_only).unwrap();

    fs::remove_file(dir.path().join(".kio/index/sqlite.db")).unwrap();
    let error = json_failure(&dir, &["repair", "rebuild-db"], 2);
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("linear ancestry chain"),
        "{error}"
    );
    assert_eq!(
        repo.head_commit_hash().unwrap().as_deref(),
        Some(head.as_str())
    );
    assert!(!dir.path().join(".kio/index/sqlite.db").exists());
}

#[test]
fn ct4_rebuild_rejects_canonical_tag_when_head_is_unborn() {
    let dir = indexed_scope();
    let repo = Repository::open(dir.path()).unwrap();
    let tagged_commit = repo.head_commit_hash().unwrap().unwrap();
    let tag_path = repo
        .kio_dir()
        .join("refs/tags-v1")
        .join(portable_tag_leaf("sole-root"));
    fs::write(tag_path, &tagged_commit).unwrap();
    fs::write(repo.kio_dir().join("HEAD"), b"unborn\n").unwrap();
    fs::remove_file(dir.path().join(".kio/index/sqlite.db")).unwrap();

    let error = json_failure(&dir, &["repair", "rebuild-db"], 2);
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("HEAD linear ancestry chain"),
        "{error}"
    );
    assert_eq!(fs::read(repo.kio_dir().join("HEAD")).unwrap(), b"unborn\n");
    assert!(!dir.path().join(".kio/index/sqlite.db").exists());
}

#[test]
fn ct4_rebuild_uses_pinned_manifest_unit_status_not_current_working_manifest() {
    let dir = indexed_scope();
    let repo = Repository::open(dir.path()).unwrap();
    let head = repo.head_commit_hash().unwrap().unwrap();
    let tree = repo
        .read_tree(&repo.read_commit(&head).unwrap().tree)
        .unwrap();
    let entry = tree
        .entries
        .iter()
        .find(|entry| entry.path == "auth.md")
        .unwrap();
    let normalize = entry.normalize.as_ref().unwrap();
    // A physical edit without a new tree reference is not a historical
    // snapshot: the manifest's filename hash must still authenticate it.
    let store = ObjectStore::new(repo.kio_dir());
    let manifest_path = store
        .content_path(ContentObjectKind::Manifest, &normalize.manifest_hash)
        .unwrap();
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    for unit in manifest["units"].as_array_mut().unwrap() {
        unit["status"] = Value::from("failed");
        unit["unit_object_hash"] = Value::Null;
    }
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let error = json_failure(&dir, &["repair", "rebuild-db"], 4);
    assert_eq!(error["error_code"], "KIO-E-STORE-CORRUPT-001", "{error}");
}

#[test]
fn ct4_rebuild_uses_immutable_pinned_unit_body_not_mutable_cache_body() {
    let dir = indexed_scope();
    let repo = Repository::open(dir.path()).unwrap();
    let head = repo.head_commit_hash().unwrap().unwrap();
    let tree = repo
        .read_tree(&repo.read_commit(&head).unwrap().tree)
        .unwrap();
    let entry = tree
        .entries
        .iter()
        .find(|entry| entry.path == "auth.md")
        .unwrap();
    let normalize = entry.normalize.as_ref().unwrap();
    let store = ObjectStore::new(repo.kio_dir());
    let manifest_path = store
        .content_path(ContentObjectKind::Manifest, &normalize.manifest_hash)
        .unwrap();
    let manifest: Value = serde_json::from_slice(&fs::read(manifest_path).unwrap()).unwrap();
    let unit_ref = manifest["units"].as_array().unwrap()[0]["unit_ref"]
        .as_str()
        .unwrap();
    let mutable_unit = mutable_unit_path_for(
        &dir.path().join(".kio/objects/normalized_units"),
        &entry.raw_hash,
        &normalize.tool_profile_hash,
        normalize.r#gen,
        unit_ref,
    );
    assert!(
        mutable_unit.exists(),
        "test must modify the current cache unit"
    );
    let mut cache: Value = serde_json::from_slice(&fs::read(&mutable_unit).unwrap()).unwrap();
    cache["markdown"] = Value::from("# forged\n\nMUTABLE-CACHE-ATTACK\n");
    fs::write(&mutable_unit, serde_json::to_vec(&cache).unwrap()).unwrap();

    // Force chunk derivation rather than retaining an old ledger row.
    fs::remove_file(dir.path().join(".kio/index/chunks.jsonl")).unwrap();
    fs::remove_file(dir.path().join(".kio/index/sqlite.db")).unwrap();
    json_success(&dir, &["repair", "rebuild-db"]);

    let forged = json_success(&dir, &["search", "MUTABLE-CACHE-ATTACK", "--mode", "text"]);
    assert!(forged["results"].as_array().unwrap().is_empty(), "{forged}");
    let original = json_success(&dir, &["search", "3600", "--mode", "text"]);
    assert!(
        !original["results"].as_array().unwrap().is_empty(),
        "rebuild must use the pinned immutable unit body: {original}"
    );
}

#[test]
fn ct4_rebuild_missing_pinned_unit_cas_skips_without_reading_mutable_cache() {
    let dir = indexed_scope();
    let repo = Repository::open(dir.path()).unwrap();
    let head = repo.head_commit_hash().unwrap().unwrap();
    let tree = repo
        .read_tree(&repo.read_commit(&head).unwrap().tree)
        .unwrap();
    let entry = tree
        .entries
        .iter()
        .find(|entry| entry.path == "auth.md")
        .unwrap();
    let normalize = entry.normalize.as_ref().unwrap();
    let store = ObjectStore::new(repo.kio_dir());
    let manifest: Value = serde_json::from_slice(
        &fs::read(
            store
                .content_path(ContentObjectKind::Manifest, &normalize.manifest_hash)
                .unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let unit = &manifest["units"].as_array().unwrap()[0];
    let unit_hash = unit["unit_object_hash"].as_str().unwrap();
    let unit_ref = unit["unit_ref"].as_str().unwrap();
    let mutable_unit = mutable_unit_path_for(
        &dir.path().join(".kio/objects/normalized_units"),
        &entry.raw_hash,
        &normalize.tool_profile_hash,
        normalize.r#gen,
        unit_ref,
    );
    let mut cache: Value = serde_json::from_slice(&fs::read(&mutable_unit).unwrap()).unwrap();
    cache["markdown"] = Value::from("# forged\n\nMISSING-CAS-ATTACK\n");
    fs::write(&mutable_unit, serde_json::to_vec(&cache).unwrap()).unwrap();
    fs::remove_file(
        store
            .content_path(ContentObjectKind::NormalizedUnit, unit_hash)
            .unwrap(),
    )
    .unwrap();

    fs::remove_file(dir.path().join(".kio/index/chunks.jsonl")).unwrap();
    fs::remove_file(dir.path().join(".kio/index/sqlite.db")).unwrap();
    let error = json_failure(&dir, &["repair", "rebuild-db"], 4);
    assert_eq!(error["error_code"], "KIO-E-STORE-CORRUPT-001", "{error}");
    assert!(
        !dir.path().join(".kio/index/chunks.jsonl").exists(),
        "a missing immutable unit must fail before a mutable-cache body can be projected"
    );
}

#[test]
fn ct4_reindex_at_does_not_publish_later_same_gen_unit_into_failed_snapshot() {
    let dir = indexed_scope();
    let repo = Repository::open(dir.path()).unwrap();
    let head = repo.head_commit_hash().unwrap().unwrap();
    let tree = repo
        .read_tree(&repo.read_commit(&head).unwrap().tree)
        .unwrap();
    let entry = tree
        .entries
        .iter()
        .find(|entry| entry.path == "auth.md")
        .unwrap();
    let normalize = entry.normalize.as_ref().unwrap();
    // A physical mutation of the pinned object without a new tree binding is
    // corrupt, even if it looks like a same-gen retry state.
    let store = ObjectStore::new(repo.kio_dir());
    let manifest_path = store
        .content_path(ContentObjectKind::Manifest, &normalize.manifest_hash)
        .unwrap();
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    for unit in manifest["units"].as_array_mut().unwrap() {
        unit["status"] = Value::from("failed");
        unit["unit_object_hash"] = Value::Null;
    }
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let error = json_failure(&dir, &["reindex", "--at", &head], 4);
    assert_eq!(error["error_code"], "KIO-E-STORE-CORRUPT-001", "{error}");
}

#[test]
fn ct4_rebuild_rejects_pinned_manifest_for_a_different_raw_identity() {
    let dir = indexed_scope();
    let repo = Repository::open(dir.path()).unwrap();
    let head = repo.head_commit_hash().unwrap().unwrap();
    let tree = repo
        .read_tree(&repo.read_commit(&head).unwrap().tree)
        .unwrap();
    let normalize = tree
        .entries
        .iter()
        .find(|entry| entry.path == "auth.md")
        .unwrap()
        .normalize
        .as_ref()
        .unwrap();
    let store = ObjectStore::new(repo.kio_dir());
    let manifest_path = store
        .content_path(ContentObjectKind::Manifest, &normalize.manifest_hash)
        .unwrap();
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["raw_hash"] = Value::from(format!("sha256:{}", "f".repeat(64)));
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();

    let error = json_failure(&dir, &["repair", "rebuild-db"], 4);
    assert_eq!(error["error_code"], "KIO-E-STORE-CORRUPT-001");
}

#[test]
fn ct4_rebuild_rejects_pinned_done_unit_with_mismatched_prepared_hash() {
    let dir = indexed_scope();
    let repo = Repository::open(dir.path()).unwrap();
    let head = repo.head_commit_hash().unwrap().unwrap();
    let tree = repo
        .read_tree(&repo.read_commit(&head).unwrap().tree)
        .unwrap();
    let normalize = tree
        .entries
        .iter()
        .find(|entry| entry.path == "auth.md")
        .unwrap()
        .normalize
        .as_ref()
        .unwrap();
    let store = ObjectStore::new(repo.kio_dir());
    let manifest_path = store
        .content_path(ContentObjectKind::Manifest, &normalize.manifest_hash)
        .unwrap();
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["units"].as_array_mut().unwrap()[0]["prepared_hash"] =
        Value::from(format!("sha256:{}", "e".repeat(64)));
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();

    let error = json_failure(&dir, &["repair", "rebuild-db"], 4);
    assert_eq!(error["error_code"], "KIO-E-STORE-CORRUPT-001");
}

#[test]
fn ct4_rebuild_fails_closed_when_a_current_tag_targets_a_missing_commit() {
    let dir = indexed_scope();
    let repo = Repository::open(dir.path()).unwrap();
    let head_before = fs::read(repo.kio_dir().join("HEAD")).unwrap();
    let index_before = fs::read(repo.kio_dir().join("index/sqlite.db")).unwrap();
    let tag_path = repo
        .kio_dir()
        .join("refs/tags-v1")
        .join(portable_tag_leaf("missing-root"));
    fs::write(tag_path, format!("sha256:{}", "d".repeat(64))).unwrap();

    let error = json_failure(&dir, &["repair", "rebuild-db"], 2);
    assert_eq!(error["error_code"], "KIO-E-CONFIG-SCHEMA-001");
    assert_eq!(fs::read(repo.kio_dir().join("HEAD")).unwrap(), head_before);
    assert_eq!(
        fs::read(repo.kio_dir().join("index/sqlite.db")).unwrap(),
        index_before,
        "an unauthenticated tag must not authorize rebuilding the source index"
    );
}

#[test]
fn ct3_cursor_006_offset_matches_cursor_page() {
    let dir = indexed_scope();
    let first = json_success(&dir, &["search", "認証仕様", "--limit", "1"]);
    let cursor = first["paging"]["next_cursor"].as_str().unwrap();
    let by_cursor = json_success(
        &dir,
        &["search", "認証仕様", "--cursor", cursor, "--limit", "1"],
    );
    let by_offset = json_success(
        &dir,
        &["search", "認証仕様", "--offset", "1", "--limit", "1"],
    );
    assert_eq!(by_cursor["results"], by_offset["results"]);
}

// Label fix: this is CT3-CURSOR-006 ("--offset は cursor の糖衣 ... 末尾を超え
// たら next_cursor: null で終端"), not a second instance of CT3-CURSOR-001
// (determinism of cursor recompute, already covered above). Renamed off the
// ct3_cursor_001_* prefix to stop misclaiming CURSOR-001 coverage.
#[test]
fn ct3_cursor_006_end_of_stream_has_null_next_cursor() {
    let dir = indexed_scope();
    let search = json_success(&dir, &["search", "認証仕様", "--limit", "100"]);
    assert!(!search["results"].as_array().unwrap().is_empty());
    assert!(search["paging"]["next_cursor"].is_null());
}

/// PC11 (step4b-contract-tests-p2c.md §C; 05-runtime.md §1.3 L95-97): a
/// single short (< 3 Unicode scalar) CJK token no longer forces zero results
/// — trigram MATCH can't carry it (0 rows) so the text backend falls back to
/// a bounded `instr` scan of `chunks.text`, which finds it. Supersedes the
/// pre-PC11 `ct3_fts_003_two_character_query_is_skipped_with_zero_results`,
/// which asserted the opposite (a deliberate short-circuit this rewrite
/// removed).
#[test]
fn pc11_short_cjk_query_falls_back_to_bounded_like_and_finds_the_substring() {
    let dir = indexed_scope();
    let search = json_success(&dir, &["search", "認"]);
    let results = search["results"].as_array().unwrap();
    assert!(
        !results.is_empty(),
        "a single-char CJK query must fall back to a bounded LIKE scan: {search}"
    );
}

// Real-machine scenario (a): default (no flag) search crosses sibling scopes via
// the registry, and a `participates_in_global_search=false` scope is excluded.
#[test]
fn ct3_multi_001_default_searches_participating_indexed_scopes() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let a = parent.path().join("a");
    let b = parent.path().join("b");
    let c = parent.path().join("c");
    for dir in [&a, &b, &c] {
        fs::create_dir_all(dir).unwrap();
    }
    fs::write(a.join("a.md"), "# A\n\n## Local\nalpha only\n").unwrap();
    fs::write(
        b.join("b.md"),
        "# B\n\n## Remote\nunique sibling token 4242\n",
    )
    .unwrap();
    fs::write(
        c.join("c.md"),
        "# C\n\n## Hidden\nunique sibling token 4242 private\n",
    )
    .unwrap();
    for dir in [&a, &b, &c] {
        json_success_path(dir, &data_home, &["init"]);
    }
    // Scope c opts out of global search.
    fs::write(
        c.join(".kio/config.toml"),
        "[scope]\nparticipates_in_global_search = false\n",
    )
    .unwrap();
    for dir in [&a, &b, &c] {
        json_success_path(dir, &data_home, &["index"]);
    }
    // Default search (no --all-scopes) from scope a still reaches sibling b.
    let search = json_success_path(&a, &data_home, &["search", "unique sibling 4242"]);
    // R23-20 (03 §4 L296): scope_path is now the canonical `.kio` directory,
    // not its parent — the suffix check widens to the last two components.
    assert!(value_path_ends_with(
        &first_result(&search)["scope_path"],
        "b/.kio"
    ));
    let searched = search["searched_scopes"].as_array().unwrap();
    assert_eq!(searched.len(), 2, "c (participates=false) must be excluded");
    assert!(
        searched
            .iter()
            .all(|scope| !value_path_ends_with(&scope["scope_path"], "c/.kio"))
    );
}

#[test]
fn ct3_multi_008_all_scopes_flag_targets_all_indexed_scopes() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let a = parent.path().join("a");
    let b = parent.path().join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(a.join("a.md"), "# A\n\n## Local\nalpha only\n").unwrap();
    fs::write(
        b.join("b.md"),
        "# B\n\n## Remote\nunique sibling token 4242\n",
    )
    .unwrap();
    json_success_path(&a, &data_home, &["init"]);
    json_success_path(&b, &data_home, &["init"]);
    json_success_path(&a, &data_home, &["index"]);
    json_success_path(&b, &data_home, &["index"]);
    let search = json_success_path(
        &a,
        &data_home,
        &["search", "unique sibling 4242", "--all-scopes"],
    );
    // R23-20 (03 §4 L296): scope_path is the canonical `.kio` directory.
    assert!(value_path_ends_with(
        &first_result(&search)["scope_path"],
        "b/.kio"
    ));
    assert_eq!(search["searched_scopes"].as_array().unwrap().len(), 2);
}

#[test]
fn ct3_repair_device_replica_rebuilds_all_indexed_scopes_outside_a_scope() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let a = parent.path().join("a");
    let b = parent.path().join("b");
    for (dir, text) in [(&a, "replica device alpha"), (&b, "replica device hidden")] {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("doc.md"), text).unwrap();
        json_success_path(dir, &data_home, &["init"]);
    }
    // This row must still be selected: repair is device-global, not search-global.
    fs::write(
        b.join(".kio/config.toml"),
        "[scope]\nparticipates_in_global_search = false\n",
    )
    .unwrap();
    for dir in [&a, &b] {
        json_success_path(dir, &data_home, &["index"]);
    }
    // Replica-only repair must preserve the canonical source HEAD and SQLite.
    let a_head = fs::read(a.join(".kio/HEAD")).unwrap();
    let b_head = fs::read(b.join(".kio/HEAD")).unwrap();
    let a_sqlite = fs::read(a.join(".kio/index/sqlite.db")).unwrap();
    let b_sqlite = fs::read(b.join(".kio/index/sqlite.db")).unwrap();
    fs::remove_file(data_home.join("cache/kio/aggregator.sqlite")).unwrap();

    let repaired = json_success_path(parent.path(), &data_home, &["repair", "-r"]);
    assert_eq!(repaired["operation"], "replica");
    assert_eq!(repaired["summary"]["repaired"], 2);
    let repaired_ids: Vec<_> = repaired["repaired_scopes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|scope| scope["scope_id"].as_str().unwrap())
        .collect();
    assert!(repaired_ids.contains(&read_scope_id(&a).as_str()));
    assert!(repaired_ids.contains(&read_scope_id(&b).as_str()));
    assert_eq!(fs::read(a.join(".kio/HEAD")).unwrap(), a_head);
    assert_eq!(fs::read(b.join(".kio/HEAD")).unwrap(), b_head);
    assert_eq!(fs::read(a.join(".kio/index/sqlite.db")).unwrap(), a_sqlite);
    assert_eq!(fs::read(b.join(".kio/index/sqlite.db")).unwrap(), b_sqlite);
    let hidden_header = Aggregator::open(&data_home.join("cache/kio/aggregator.sqlite"))
        .unwrap()
        .scope_header(&read_scope_id(&b))
        .unwrap()
        .expect("replica repair publishes the non-participating scope");
    assert_eq!(hidden_header.index_status, AggIndexStatus::Ready);
    assert!(
        hidden_header.current_snapshot_commit.is_some(),
        "the read-only refs/main fallback must not publish a false empty corpus"
    );
    assert_eq!(
        json_success_path(parent.path(), &data_home, &["repair", "replica"])["summary"]["repaired"],
        2
    );
}

#[test]
fn ct3_repair_device_all_recovers_missing_source_and_replica_outside_a_scope() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let a = parent.path().join("a");
    let b = parent.path().join("b");
    for (dir, text) in [(&a, "all repair alpha"), (&b, "all repair beta")] {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("doc.md"), text).unwrap();
        json_success_path(dir, &data_home, &["init"]);
        json_success_path(dir, &data_home, &["index"]);
    }
    let a_kio = a.join(".kio");
    let a_head_before = fs::read_to_string(a_kio.join("HEAD")).unwrap();
    let a_raw_hash: String = Connection::open(a_kio.join("index/sqlite.db"))
        .unwrap()
        .query_row("SELECT raw_hash FROM chunks LIMIT 1", [], |row| row.get(0))
        .unwrap();
    fs::remove_file(object_path(&a_kio, "raw", &a_raw_hash)).unwrap();
    fs::remove_file(a.join(".kio/index/sqlite.db")).unwrap();
    fs::remove_file(data_home.join("cache/kio/aggregator.sqlite")).unwrap();
    let repaired = json_success_path(parent.path(), &data_home, &["repair", "-a"]);
    assert_eq!(repaired["summary"]["repaired"], 2, "{repaired:#?}");
    assert!(a.join(".kio/index/sqlite.db").is_file());
    assert!(
        object_path(&a_kio, "raw", &a_raw_hash).is_file(),
        "all recovers a missing canonical raw from the unchanged working tree"
    );
    assert_ne!(
        fs::read_to_string(a_kio.join("HEAD")).unwrap(),
        a_head_before,
        "canonical recovery records the verifier's repaired commit"
    );
    let search = json_success_path(
        &a,
        &data_home,
        &["search", "all repair beta", "--mode", "text"],
    );
    assert!(!search["results"].as_array().unwrap().is_empty());
    assert_eq!(
        json_success_path(parent.path(), &data_home, &["repair", "all"])["summary"]["repaired"],
        2
    );
}

#[test]
fn ct3_device_repairs_reject_non_current_scope_before_any_mutation() {
    for operation in ["replica", "all"] {
        let parent = tempfile::tempdir().unwrap();
        let data_home = parent.path().join("xdg");
        let current = parent.path().join("a-current");
        let non_current = parent.path().join("z-non-current");
        for (dir, text) in [
            (&current, "current repair boundary"),
            (&non_current, "non-current repair boundary"),
        ] {
            fs::create_dir_all(dir).unwrap();
            fs::write(dir.join("doc.md"), text).unwrap();
            json_success_path(dir, &data_home, &["init"]);
            json_success_path(dir, &data_home, &["index", "--offline"]);
        }

        let aggregator = data_home.join("cache/kio/aggregator.sqlite");
        let conn = Connection::open(&aggregator).unwrap();
        conn.execute_batch(
            "CREATE TABLE phase_a_no_mutation_sentinel (value TEXT NOT NULL);
             INSERT INTO phase_a_no_mutation_sentinel VALUES ('untouched');",
        )
        .unwrap();
        drop(conn);

        let current_head = fs::read(current.join(".kio/HEAD")).unwrap();
        let current_sqlite = fs::read(current.join(".kio/index/sqlite.db")).unwrap();
        let scope_json = non_current.join(".kio/scope.json");
        let mut scope: Value = serde_json::from_slice(&fs::read(&scope_json).unwrap()).unwrap();
        scope["kio_format_version"] = Value::String("9.0.0".to_owned());
        fs::write(&scope_json, serde_json::to_vec_pretty(&scope).unwrap()).unwrap();
        let non_current_scope = fs::read(&scope_json).unwrap();

        let stderr = hermetic_kio_command()
            .current_dir(parent.path())
            .env("XDG_CONFIG_HOME", data_home.join("config"))
            .env("XDG_DATA_HOME", data_home.join("data"))
            .env("XDG_CACHE_HOME", data_home.join("cache"))
            .args(["repair", operation, "--json"])
            .assert()
            .code(8)
            .get_output()
            .stderr
            .clone();
        let error: Value = serde_json::from_slice(&stderr).unwrap();
        assert_eq!(error["error_code"], "KIO-E-STORE-VERSION-001");
        assert_eq!(error["context"]["found"], "9.0.0");

        let sentinel: String = Connection::open(&aggregator)
            .unwrap()
            .query_row(
                "SELECT value FROM phase_a_no_mutation_sentinel",
                [],
                |row| row.get(0),
            )
            .expect("device replica must not be recreated before format rejection");
        assert_eq!(sentinel, "untouched");
        assert_eq!(fs::read(current.join(".kio/HEAD")).unwrap(), current_head);
        assert_eq!(
            fs::read(current.join(".kio/index/sqlite.db")).unwrap(),
            current_sqlite,
            "repair {operation} must not touch a healthy sibling"
        );
        assert_eq!(
            fs::read(&scope_json).unwrap(),
            non_current_scope,
            "format rejection must preserve incompatible bytes"
        );
    }
}

#[test]
fn ct3_repair_device_unreachable_registry_scope_is_partial_without_cwd_fallback() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let a = parent.path().join("a");
    let b = parent.path().join("b");
    for dir in [&a, &b] {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("doc.md"), "partial repair healthy").unwrap();
        json_success_path(dir, &data_home, &["init"]);
        json_success_path(dir, &data_home, &["index"]);
    }
    let b_scope = read_scope_id(&b);
    fs::remove_dir_all(&b).unwrap();
    let stdout = hermetic_kio_command()
        .current_dir(parent.path())
        .env("XDG_CONFIG_HOME", data_home.join("config"))
        .env("XDG_DATA_HOME", data_home.join("data"))
        .env("XDG_CACHE_HOME", data_home.join("cache"))
        .args(["repair", "replica", "--json"])
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let response: Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(response["error_code"], "KIO-E-REPAIR-PARTIAL-001");
    assert_eq!(response["summary"]["repaired"], 1);
    assert_eq!(response["summary"]["failed"], 1);
    let failed = &response["failed_scopes"][0];
    assert_eq!(failed["scope_id"], b_scope);
    assert_eq!(
        failed["path"],
        parent
            .path()
            .canonicalize()
            .unwrap()
            .join("b")
            .to_string_lossy()
            .as_ref()
    );
    assert_eq!(failed["stage"], "open");
    let replica = Aggregator::open(&data_home.join("cache/kio/aggregator.sqlite")).unwrap();
    assert!(
        replica.scope_header(&read_scope_id(&a)).unwrap().is_some(),
        "the healthy scope is reprojected after the explicit reset"
    );
    assert!(
        replica.scope_header(&b_scope).unwrap().is_none(),
        "a scope that fails before projection must not retain stale replica rows"
    );
}

#[test]
fn ct3_repair_device_registry_unavailable_never_falls_back_to_cwd() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let scope = parent.path().join("scope");
    fs::create_dir_all(&scope).unwrap();
    fs::write(scope.join("doc.md"), "registry unavailable repair").unwrap();
    json_success_path(&scope, &data_home, &["init"]);
    json_success_path(&scope, &data_home, &["index"]);
    let blocked = UnavailableRegistry::block(&registry_path(&data_home));

    let output = hermetic_kio_command()
        .current_dir(&scope)
        .env("XDG_CONFIG_HOME", data_home.join("config"))
        .env("XDG_DATA_HOME", data_home.join("data"))
        .env("XDG_CACHE_HOME", data_home.join("cache"))
        .args(["repair", "replica", "--json"])
        .assert()
        .code(2)
        .get_output()
        .stderr
        .clone();
    let error: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(error["error_code"], "KIO-E-CONFIG-SCHEMA-001");
    blocked.restore();
}

#[test]
fn ct3_repair_device_all_failed_homogeneous_promotes_scope_error() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let scope = parent.path().join("scope");
    fs::create_dir_all(&scope).unwrap();
    fs::write(scope.join("doc.md"), "homogeneous repair failure").unwrap();
    json_success_path(&scope, &data_home, &["init"]);
    json_success_path(&scope, &data_home, &["index"]);
    fs::remove_dir_all(&scope).unwrap();

    let output = hermetic_kio_command()
        .current_dir(parent.path())
        .env("XDG_CONFIG_HOME", data_home.join("config"))
        .env("XDG_DATA_HOME", data_home.join("data"))
        .env("XDG_CACHE_HOME", data_home.join("cache"))
        .args(["repair", "replica", "--json"])
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();
    let error: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(error["error_code"], "KIO-E-STORE-IO-001");
    assert_eq!(error["context"]["summary"]["repaired"], 0);
    assert_eq!(error["context"]["summary"]["failed"], 1);
}

#[test]
fn ct3_repair_device_active_purge_is_partial_and_leaves_no_stale_replica_rows() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let blocked = parent.path().join("blocked");
    let healthy = parent.path().join("healthy");
    for (dir, text) in [
        (&blocked, "purge repair blocked-secret"),
        (&healthy, "purge repair healthy-public"),
    ] {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("doc.md"), text).unwrap();
        json_success_path(dir, &data_home, &["init"]);
        json_success_path(dir, &data_home, &["index", "--offline"]);
    }
    let blocked_scope = read_scope_id(&blocked);
    let raw_hash: String = Connection::open(blocked.join(".kio/index/sqlite.db"))
        .unwrap()
        .query_row("SELECT raw_hash FROM chunks LIMIT 1", [], |row| row.get(0))
        .unwrap();
    let state = kio_core::purge::PurgeState::open(blocked.join(".kio")).unwrap();
    let purge_id = kio_core::scope::new_ulid(&blocked);
    let closure = kio_core::purge::PurgeClosure::new(
        purge_id.clone(),
        vec![kio_core::purge::ClosureItem {
            object_type: "raw".to_owned(),
            hash: raw_hash.clone(),
        }],
        Vec::new(),
    )
    .unwrap();
    let closure_hash = kio_core::purge::closure_content_hash(&closure).unwrap();
    state.write_closure(&closure).unwrap();
    let started = state
        .begin(
            vec![raw_hash],
            kio_core::purge::PurgeReason::Legal,
            kio_core::purge::TombstoneMode::Default,
            "test",
            "2026-08-12T00:00:00Z",
            1,
            kio_pipeline::prepare::hash_bytes(b"repair device active purge"),
            closure_hash,
            purge_id,
        )
        .unwrap();
    assert!(matches!(started, kio_core::purge::BeginOutcome::Started(_)));
    let stdout = hermetic_kio_command()
        .current_dir(parent.path())
        .env("XDG_CONFIG_HOME", data_home.join("config"))
        .env("XDG_DATA_HOME", data_home.join("data"))
        .env("XDG_CACHE_HOME", data_home.join("cache"))
        .args(["repair", "replica", "--json"])
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let response: Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(response["error_code"], "KIO-E-REPAIR-PARTIAL-001");
    assert!(
        response["failed_scopes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|scope| scope["scope_id"] == blocked_scope
                && scope["error_code"] == "KIO-E-PURGE-JOURNAL-ACTIVE-001")
    );
    let header = Aggregator::open(&data_home.join("cache/kio/aggregator.sqlite"))
        .unwrap()
        .scope_header(&blocked_scope)
        .unwrap();
    assert!(
        header.is_none(),
        "the reset must not leave a failed scope searchable from its prior projection"
    );
}

// R15-3: a `.kio` deleted and re-`init`ed at the SAME path mints a fresh scope_id.
// The device registry keyed the old row by `(scope_id, kio_path)`, so it survived — and
// multi-scope search then enumerated the SAME `.kio` twice (once per scope_id), double-
// returning every document, the stale copy carrying a dead-pointer scope_id whose
// Evidence can no longer resolve. `register_scope` now retires the stale same-path row on
// re-init, and the search enumeration additionally drops any row whose on-disk scope_id
// no longer matches — a re-init'd scope is searched exactly once.
#[test]
fn r15_3_reinit_same_path_does_not_duplicate_registry_target() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let b = parent.path().join("b");
    fs::create_dir_all(&b).unwrap();
    fs::write(b.join("b.md"), "# B\n\n## Sec\nunique reinit token 7331\n").unwrap();

    // Scope A: init + index at `b` (registers (scope_A, b/.kio), indexed).
    json_success_path(&b, &data_home, &["init"]);
    json_success_path(&b, &data_home, &["index"]);

    // Delete `.kio` and re-init + index → scope B (fresh scope_id) at the SAME path.
    fs::remove_dir_all(b.join(".kio")).unwrap();
    json_success_path(&b, &data_home, &["init"]);
    json_success_path(&b, &data_home, &["index"]);

    // `--all-scopes` enumerates the registry. The stale (scope_A) row must be gone:
    // exactly one scope target, and the document returned exactly once (no dead-pointer
    // duplicate). Pre-fix this was 2 searched scopes and 2 identical results.
    let search = json_success_path(
        &b,
        &data_home,
        &["search", "unique reinit token 7331", "--all-scopes"],
    );
    assert_eq!(
        search["searched_scopes"].as_array().unwrap().len(),
        1,
        "a re-init'd scope must be searched exactly once (stale scope_id retired): {search}"
    );
    assert_eq!(
        search["results"].as_array().unwrap().len(),
        1,
        "the document must not be double-returned via a dead scope_id: {search}"
    );
}

// scope-cross merge is rank-based: both scopes' rank-1 hits get the SAME RRF score
// (1/(60+1)) despite skewed corpus statistics, and tie-break is (scope_id, ...).
#[test]
fn ct3_multi_002_cross_scope_merge_is_rank_based() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let a = parent.path().join("a");
    let b = parent.path().join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    // Byte-identical chunks in both scopes: one corpus, one BM25, so this is a
    // GENUINE tie and the tie-break is what decides the order.
    //
    // This fixture used to differ between the scopes (filler-padded in `a`,
    // bare in `b`) and assert the two scores came out EQUAL — offered as proof
    // that "the merge compares ranks, not raw BM25". It did compare ranks, but
    // PER-SCOPE ones, so it was equally proof that a weak match topping a small
    // folder scored the same as a strong one topping another. Phase 3 measured
    // where that leads (a `--mode vector` rank-1 answer landing 38th under
    // hybrid). `ct3_multi_009` now owns the differing-strength case and expects
    // the corpus-wide order; what remains here is the rank-not-score property
    // and the deterministic tie-break.
    for (dir, name) in [(&a, "a.md"), (&b, "b.md")] {
        fs::write(dir.join(name), "# Doc\n\n## Sec\nzephyrterm\n").unwrap();
    }
    json_success_path(&a, &data_home, &["init"]);
    json_success_path(&b, &data_home, &["init"]);
    // Make immutable scope-id order intentionally oppose mutable path order.
    // This catches accidental reintroduction of the old scope_path tie-break.
    replace_scope_id(&a, "7ZZZZZZZZZZZZZZZZZZZZZZZZZ");
    replace_scope_id(&b, "00000000000000000000000001");
    json_success_path(&a, &data_home, &["index"]);
    json_success_path(&b, &data_home, &["index"]);
    let search = json_success_path(&a, &data_home, &["search", "zephyrterm"]);
    let results = search["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    // The emitted score is a function of RANK, never of the BM25 magnitude:
    // global ranks 1 and 2 give exactly these two values.
    assert!((results[0]["score"].as_f64().unwrap() - 1.0 / 61.0).abs() < 1e-12);
    assert!((results[1]["score"].as_f64().unwrap() - 1.0 / 62.0).abs() < 1e-12);
    // Deterministic tie-break by scope_id: b's low id precedes a's high id even
    // though registry/input order is path a then path b. R23-20 (03 §4 L296):
    // scope_path is the canonical `.kio` directory.
    assert!(value_path_ends_with(&results[0]["scope_path"], "b/.kio"));
    assert!(value_path_ends_with(&results[1]["scope_path"], "a/.kio"));
}

/// The text term is ranked over the WHOLE corpus, so a small scope's rank-1
/// does not buy what a corpus-wide rank-1 buys.
///
/// Phase 3 measured the cost of the previous per-scope arrangement on the
/// dogfood corpus: an answer `--mode vector` ranked 1st came back 38th under
/// hybrid, behind 37 chunks whose only merit was topping their own small
/// folder. BM25 is not comparable across corpora — which is a reason to stop
/// having several corpora, not a reason to sum incomparable ranks. Scoring the
/// query once against a device-level index of every scope's chunks is the same
/// answer `dfs_query_then_fetch` gives a sharded Elasticsearch.
///
/// The fixture makes the two disagree on purpose: scope `a` buries the term in
/// filler (weak BM25), scope `b` states it alone (strong BM25). Per-scope, both
/// are rank 1 and score identically. Globally, `b` must win.
#[test]
fn ct3_multi_009_text_rank_is_global_not_per_scope() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let a = parent.path().join("a");
    let b = parent.path().join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    let filler = "filler ".repeat(60);
    fs::write(
        a.join("a.md"),
        format!("# A\n\n## Sec\nzephyrterm {filler}\n"),
    )
    .unwrap();
    fs::write(b.join("b.md"), "# B\n\n## Sec\nzephyrterm\n").unwrap();
    json_success_path(&a, &data_home, &["init"]);
    json_success_path(&b, &data_home, &["init"]);
    // The weak match gets the LOW scope_id, so if the merge fell back to the
    // deterministic tie-break it would put `a` first — the failure this test
    // would otherwise pass by accident.
    replace_scope_id(&a, "00000000000000000000000001");
    replace_scope_id(&b, "7ZZZZZZZZZZZZZZZZZZZZZZZZZ");
    json_success_path(&a, &data_home, &["index"]);
    json_success_path(&b, &data_home, &["index"]);

    let search = json_success_path(&a, &data_home, &["search", "zephyrterm"]);
    let results = search["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert!(
        value_path_ends_with(&results[0]["scope_path"], "b/.kio"),
        "the corpus-wide better match must lead: {results:#?}"
    );
    assert!(
        results[0]["score"].as_f64().unwrap() > results[1]["score"].as_f64().unwrap(),
        "global ranks 1 and 2 must produce different scores, not a per-scope tie: {results:#?}"
    );
    // Exactly the two global ranks, so the scores are checkable rather than
    // merely ordered.
    assert!((results[0]["score"].as_f64().unwrap() - 1.0 / 61.0).abs() < 1e-12);
    assert!((results[1]["score"].as_f64().unwrap() - 1.0 / 62.0).abs() < 1e-12);
}

/// A departed scope stops contributing to the collection immediately.
///
/// The replica is a device-wide corpus, so a folder that left the registry
/// keeps depressing every surviving chunk's IDF for its terms until something
/// evicts it. An all-scopes search is the one caller that knows the live set,
/// so it is the one that prunes.
#[test]
fn ct3_multi_011_a_departed_scope_stops_skewing_corpus_statistics() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let (a, b, c) = (
        parent.path().join("a"),
        parent.path().join("b"),
        parent.path().join("c"),
    );
    for (dir, body) in [
        (&a, "# A\n\n## Sec\nzephyrterm alpha\n"),
        (&b, "# B\n\n## Sec\nzephyrterm beta\n"),
        (&c, "# C\n\n## Sec\nzephyrterm gamma\n"),
    ] {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("hit.md"), body).unwrap();
    }
    for dir in [&a, &b, &c] {
        json_success_path(dir, &data_home, &["init"]);
        json_success_path(dir, &data_home, &["index"]);
    }
    let replica_path = data_home.join("cache/kio/aggregator.sqlite");
    // Read before the folder goes away — the assertion below needs the id.
    let c_scope_id: String =
        serde_json::from_str::<Value>(&fs::read_to_string(c.join(".kio/scope.json")).unwrap())
            .unwrap()["scope_id"]
            .as_str()
            .unwrap()
            .to_owned();

    let with_c = json_success_path(&a, &data_home, &["search", "--mode", "text", "zephyrterm"]);
    assert_eq!(with_c["searched_scopes"].as_array().unwrap().len(), 3);
    let (scopes_with_c, chunks_with_c) = {
        let replica = Aggregator::open(&replica_path).unwrap();
        let (scopes, chunks, _) = replica.corpus_size().unwrap();
        assert!(
            replica
                .text_scores("zephyrterm", &replica.scope_ids().unwrap(), 100)
                .unwrap()
                .iter()
                .any(|score| score.scope_id == c_scope_id),
            "`c` must be part of the scored collection to begin with"
        );
        (scopes, chunks)
    };
    assert_eq!(scopes_with_c, 3, "every searched scope must be projected");

    // `c` goes away. Nothing else changes.
    fs::remove_dir_all(&c).unwrap();

    // Exit 3 is the established partial-failure contract: `c` is unreachable,
    // so the search returns results AND reports the exclusion.
    let stdout = hermetic_kio_command()
        .current_dir(&a)
        .env("XDG_CONFIG_HOME", data_home.join("config"))
        .env("XDG_DATA_HOME", data_home.join("data"))
        .env("XDG_CACHE_HOME", data_home.join("cache"))
        .args(["search", "--mode", "text", "zephyrterm", "--json"])
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let without_c: Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(without_c["excluded_scopes"][0]["reason"], "unreachable");

    let replica = Aggregator::open(&replica_path).unwrap();
    let (scopes, chunks, _) = replica.corpus_size().unwrap();
    assert_eq!(scopes, 2, "the departed scope must leave `agg_scopes`");
    assert!(
        chunks < chunks_with_c,
        "the departed scope's chunks must leave the collection too ({chunks} vs \
         {chunks_with_c}), or they keep inflating document frequency for every \
         surviving chunk"
    );
    assert!(
        replica
            .text_scores("zephyrterm", &replica.scope_ids().unwrap(), 100)
            .unwrap()
            .iter()
            .all(|score| score.scope_id != c_scope_id),
        "no row of the departed scope may survive in the scored collection"
    );
    let results = without_c["results"].as_array().unwrap();
    assert!(
        results
            .iter()
            .all(|hit| hit["evidence_pointer"]["scope_id"] != c_scope_id.as_str()),
        "a departed scope must not return results either: {without_c:#?}"
    );
}

/// A narrowed search READS the collection; it does not redefine it.
///
/// Pruning to `searched` is right for an all-scopes search and destructive for
/// a `--scope`/`--descendants` one, whose scope set is a deliberate subset —
/// pruning to it would evict every scope the user did not ask about and force a
/// full re-projection on the next default search.
#[test]
fn ct3_multi_012_a_narrowed_search_does_not_prune_the_replica() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let nest = parent.path().join("nest");
    let sub = nest.join("sub");
    let other = parent.path().join("other");
    for dir in [&nest, &sub, &other] {
        fs::create_dir_all(dir).unwrap();
    }
    for (dir, body) in [
        (&nest, "zephyrterm nest"),
        (&sub, "zephyrterm sub"),
        (&other, "zephyrterm other"),
    ] {
        fs::write(dir.join("hit.md"), format!("# H\n\n## Sec\n{body}\n")).unwrap();
    }
    // Leave `sub` uninitialized for the parent's first index. This proves
    // child discovery itself is fail-closed on Windows, then the test directly
    // creates each independent scope before exercising the search contract.
    for dir in [&nest, &other] {
        json_success_path(dir, &data_home, &["init"]);
    }
    #[cfg(not(windows))]
    json_success_path(&nest, &data_home, &["index"]);
    #[cfg(windows)]
    {
        let indexed = json_code_stdout_path(&nest, &data_home, &["index"], 3);
        assert_windows_bound_child_unsupported(&indexed, "sub");
        assert!(
            !sub.join(".kio").exists(),
            "Windows must not initialize a child through a pathname fallback"
        );
    }
    // `nest`'s own index must not pull `sub` in: scopes are non-recursive, and
    // this test needs them to be three independently indexed collection members.
    json_success_path(&sub, &data_home, &["init"]);
    json_success_path(&sub, &data_home, &["index"]);
    json_success_path(&other, &data_home, &["index"]);
    let replica_path = data_home.join("cache/kio/aggregator.sqlite");

    let all = json_success_path(
        &other,
        &data_home,
        &["search", "--mode", "text", "zephyrterm"],
    );
    assert_eq!(all["searched_scopes"].as_array().unwrap().len(), 3);
    let full = Aggregator::open(&replica_path)
        .unwrap()
        .corpus_size()
        .unwrap();
    assert_eq!(full.0, 3, "the default search projects every scope");

    // Narrow to `nest` + its descendant — two scopes, so the merge path that
    // consults the replica is reached, but `other` is deliberately not touched.
    let narrowed = json_success_path(
        &nest,
        &data_home,
        &[
            "search",
            "--mode",
            "text",
            "zephyrterm",
            "--scope",
            ".",
            "--descendants",
        ],
    );
    assert_eq!(narrowed["searched_scopes"].as_array().unwrap().len(), 2);

    let after = Aggregator::open(&replica_path)
        .unwrap()
        .corpus_size()
        .unwrap();
    assert_eq!(
        after, full,
        "a narrowed search must leave the collection intact, not evict the \
         scopes it was told to skip"
    );
}

/// Indexing replicates. The first cross-scope search finds a complete corpus
/// without projecting anything itself.
///
/// The reader-driven refresh this replaces could only notice a scope whose
/// `index_generation` had moved since the replica last stamped it, which made
/// every in-place index writer a potential silent hole: the batch embedding
/// collect, the sync embedding lane, and `reindex --at` all write the live
/// `sqlite.db` without rebuilding it, and two of the three rotated nothing. The
/// fix is direction, not detection — the writer tells the replica.
#[test]
fn ct3_multi_013_indexing_replicates_without_waiting_for_a_search() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let a = parent.path().join("a");
    let b = parent.path().join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(a.join("a.md"), "# A\n\n## Sec\nquillvane appears here\n").unwrap();
    fs::write(
        b.join("b.md"),
        "# B\n\n## Sec\nquillvane appears here too\n",
    )
    .unwrap();
    json_success_path(&a, &data_home, &["init"]);
    json_success_path(&b, &data_home, &["init"]);
    json_success_path(&a, &data_home, &["index"]);
    json_success_path(&b, &data_home, &["index"]);

    // No search has run. Both scopes are already in the collection.
    let replica_path = data_home.join("cache/kio/aggregator.sqlite");
    let replica = Aggregator::open(&replica_path).unwrap();
    let (scopes, chunks, _) = replica.corpus_size().unwrap();
    assert_eq!(
        scopes, 2,
        "both indexed scopes replicated before any search"
    );
    assert!(chunks >= 2, "with their chunks: {chunks}");
    let scored = replica
        .text_scores("quillvane", &replica.scope_ids().unwrap(), 100)
        .unwrap();
    let hit_scopes = scored
        .iter()
        .map(|score| score.scope_id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        hit_scopes.len(),
        2,
        "one BM25 over one corpus, spanning both scopes: {scored:#?}"
    );
    drop(replica);

    // And the search that follows agrees with what was already replicated.
    let search = json_success_path(&a, &data_home, &["search", "quillvane", "--all-scopes"]);
    assert_eq!(search["results"].as_array().unwrap().len(), 2);
}

/// R25-1: a history search is selected and ranked from the same device
/// replica as a live search. The replica retains committed chunks and uses the
/// scope resolver's liveness projection to choose the requested snapshot.
#[test]
fn ct3_multi_014_a_history_search_is_ranked_by_the_replica() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let a = parent.path().join("a");
    let b = parent.path().join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(a.join("keep.md"), "# A\n\n## Sec\nvellichor stays put\n").unwrap();
    fs::write(
        a.join("gone.md"),
        "# G\n\n## Sec\nvellichor will be deleted\n",
    )
    .unwrap();
    fs::write(b.join("b.md"), "# B\n\n## Sec\nvellichor over here\n").unwrap();
    for dir in [&a, &b] {
        json_success_path(dir, &data_home, &["init"]);
        json_success_path(dir, &data_home, &["index"]);
    }

    // The chunk leaves the live index — and, via write-through, the replica.
    fs::remove_file(a.join("gone.md")).unwrap();
    json_success_path(&a, &data_home, &["index"]);

    let live = json_success_path(&a, &data_home, &["search", "vellichor", "--mode", "text"]);
    let live_generation = replica_collection_generation(&live).to_owned();
    assert_eq!(live["searched_scopes"].as_array().unwrap().len(), 2);

    let history = json_success_path(
        &a,
        &data_home,
        &["search", "vellichor", "--mode", "text", "--include-deleted"],
    );
    let history_generation = replica_collection_generation(&history);
    assert_eq!(
        history_generation, live_generation,
        "time selection changes eligible chunks, not the candidate source: {history:#?}"
    );
    let results = history["results"].as_array().unwrap();
    assert!(
        results.len() > live["results"].as_array().unwrap().len(),
        "premise: --include-deleted must resurface the deleted chunk: {history:#?}"
    );
    assert!(
        results
            .iter()
            .all(|hit| hit["score"].as_f64().unwrap() > 0.0),
        "every historical hit must retain its replica text rank: {history:#?}"
    );
}

/// R25-2: short tokens use the replica's bounded `instr` lane, rather than
/// falling back to per-scope candidate selection. Japanese two-character
/// queries are a primary path, not an exceptional one.
#[test]
fn ct3_multi_015_short_tokens_are_ranked_by_the_replica() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let a = parent.path().join("a");
    let b = parent.path().join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(a.join("a.md"), "# A\n\n## Sec\n認証 と halcyon の話\n").unwrap();
    fs::write(b.join("b.md"), "# B\n\n## Sec\n認証 と halcyon の続き\n").unwrap();
    for dir in [&a, &b] {
        json_success_path(dir, &data_home, &["init"]);
        json_success_path(dir, &data_home, &["index"]);
    }

    let long = json_success_path(&a, &data_home, &["search", "halcyon", "--mode", "text"]);
    let long_generation = replica_collection_generation(&long).to_owned();

    let short = json_success_path(&a, &data_home, &["search", "認証", "--mode", "text"]);
    assert_eq!(
        replica_collection_generation(&short),
        long_generation,
        "the short-token lane must query the same device replica: {short:#?}"
    );
    assert!(
        !short["results"].as_array().unwrap().is_empty(),
        "a bounded short-token query must still return its matches: {short:#?}"
    );
    assert!(
        short["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|hit| hit["score"].as_f64().is_some_and(|score| score > 0.0)),
        "short-token matches must carry replica ranks: {short:#?}"
    );
}

/// R25 (found while implementing the guard): a narrowed search is ranked among
/// the scopes it searched, not shut out by a device-wide depth cut.
///
/// The replica's `LIMIT candidate_depth` used to be taken over the whole
/// device. A `--scope`/`--descendants` subtree that ranks below that cut got
/// back no rows at all, so every one of its candidates lost its text term, and
/// the merge fell through to its `(scope_id, chunk_hash)` tie-break — results
/// ordered by hash, every score 0. Measured on the dogfood corpus with the
/// default depth of 200: for the query `the `, 263 scopes held a matching
/// chunk and the cut reached 76 of them, leaving 187 whose narrowed search
/// returned nothing but zeroes.
#[test]
fn ct3_multi_016_a_narrowed_search_is_ranked_among_the_scopes_it_searched() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let nest = parent.path().join("nest");
    let sub = nest.join("sub");
    let loud = parent.path().join("loud");
    for dir in [&nest, &sub, &loud] {
        fs::create_dir_all(dir).unwrap();
    }
    // `loud` matches far more strongly and would fill any small global cut.
    for n in 0..6 {
        fs::write(
            loud.join(format!("l{n}.md")),
            "# L\n\n## Sec\nsemaphore semaphore semaphore\n",
        )
        .unwrap();
    }
    for (dir, body) in [
        (
            &nest,
            "semaphore appears once, in a much longer sentence than the others",
        ),
        (
            &sub,
            "semaphore appears once here too, likewise buried in prose",
        ),
    ] {
        fs::write(dir.join("hit.md"), format!("# H\n\n## Sec\n{body}\n")).unwrap();
    }
    // Leave the nested child uninitialized for the parent's first index so the
    // Windows retained-handle boundary is tested before direct child setup.
    for dir in [&nest, &loud] {
        json_success_path(dir, &data_home, &["init"]);
    }
    #[cfg(not(windows))]
    json_success_path(&nest, &data_home, &["index"]);
    #[cfg(windows)]
    {
        let indexed = json_code_stdout_path(&nest, &data_home, &["index"], 3);
        assert_windows_bound_child_unsupported(&indexed, "sub");
        assert!(
            !sub.join(".kio").exists(),
            "Windows must not initialize a child through a pathname fallback"
        );
    }
    json_success_path(&sub, &data_home, &["init"]);
    json_success_path(&sub, &data_home, &["index"]);
    json_success_path(&loud, &data_home, &["index"]);
    // Multi-scope search resolves `[search]` from the DEVICE layer (05 §1.8
    // step 5), so a small depth has to be set there, not in a folder config.
    let user_config = data_home.join("config/kio");
    fs::create_dir_all(&user_config).unwrap();
    fs::write(
        user_config.join("config.toml"),
        "[search.rrf]\ncandidate_depth = 2\n",
    )
    .unwrap();

    let narrowed = json_success_path(
        &nest,
        &data_home,
        &[
            "search",
            "--mode",
            "text",
            "semaphore",
            "--scope",
            ".",
            "--descendants",
        ],
    );
    assert_eq!(narrowed["searched_scopes"].as_array().unwrap().len(), 2);
    let _generation = replica_collection_generation(&narrowed);
    let results = narrowed["results"].as_array().unwrap();
    assert_eq!(
        results.len(),
        2,
        "both narrowed scopes answer: {narrowed:#?}"
    );
    assert!(
        results
            .iter()
            .all(|hit| hit["score"].as_f64().unwrap() > 0.0),
        "every candidate the user narrowed to must carry a rank — a zero means \
         `loud` consumed the depth cut and the ordering fell through to the \
         chunk-hash tie-break: {narrowed:#?}"
    );
}

/// R25-1 (cursor half): a page replays against the collection that produced
/// page 1, or it fails — it does not silently re-rank against a different one.
///
/// The per-scope `index_generation`s a cursor already freezes pin each searched
/// scope's ROWS, which is not enough. Global BM25 also reads the collection's
/// df/`N`/`avgdl`, so indexing a folder nobody searched moves the ranks of the
/// ones they did while every per-scope stamp still matches — and page 2 orders
/// the stream differently from page 1, dropping or repeating results across
/// the boundary. `KIO-E-SEARCH-CURSOR-001` is the same remedy PC19/PC21
/// already gives for the per-scope case: re-run without a cursor.
#[test]
fn ct3_multi_017_a_cursor_replays_against_the_collection_that_ranked_page_1() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let a = parent.path().join("a");
    let b = parent.path().join("b");
    let late = parent.path().join("late");
    for dir in [&a, &b, &late] {
        fs::create_dir_all(dir).unwrap();
    }
    fs::write(a.join("a.md"), "# A\n\n## Sec\nquokka one\n").unwrap();
    fs::write(b.join("b.md"), "# B\n\n## Sec\nquokka two\n").unwrap();
    fs::write(late.join("l.md"), "# L\n\n## Sec\nquokka three\n").unwrap();
    for dir in [&a, &b] {
        json_success_path(dir, &data_home, &["init"]);
        json_success_path(dir, &data_home, &["index"]);
    }

    let page1 = json_success_path(
        &a,
        &data_home,
        &["search", "quokka", "--mode", "text", "--limit", "1"],
    );
    let _generation = replica_collection_generation(&page1);
    let cursor = page1["paging"]["next_cursor"].as_str().unwrap().to_owned();

    // Neither searched scope moves. A third one joins the device, which
    // write-through lands in the replica immediately.
    json_success_path(&late, &data_home, &["init"]);
    json_success_path(&late, &data_home, &["index"]);

    let stderr = hermetic_kio_command()
        .current_dir(&a)
        .env("XDG_CONFIG_HOME", data_home.join("config"))
        .env("XDG_DATA_HOME", data_home.join("data"))
        .env("XDG_CACHE_HOME", data_home.join("cache"))
        .args([
            "search", "quokka", "--mode", "text", "--limit", "1", "--cursor", &cursor, "--json",
        ])
        .assert()
        .code(2)
        .get_output()
        .stderr
        .clone();
    let error: Value = serde_json::from_slice(&stderr).unwrap();
    assert_eq!(error["error_code"], "KIO-E-SEARCH-CURSOR-001");
    assert_eq!(error["context"]["reason"], "collection_generation_mismatch");
}

/// R25-9: the replica retains every committed chunk. A liveness binding, not
/// physical deletion from the corpus, selects the chunks visible to each time
/// selector.
#[test]
fn ct3_multi_020_the_replica_retains_committed_chunks_across_liveness_changes() {
    let dir = tempfile::tempdir().unwrap();
    for n in 1..=3 {
        fs::write(
            dir.path().join(format!("d{n}.md")),
            format!("# D{n}\n\n## Sec\nliveprobe body {n}\n"),
        )
        .unwrap();
    }
    kio(&dir, &["init"]).assert().success();
    json_success(&dir, &["index", "--offline"]);

    let committed = || -> i64 {
        let conn = rusqlite::Connection::open(dir.path().join(".kio/index/sqlite.db")).unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM chunks c WHERE EXISTS (SELECT 1 FROM chunk_publications p WHERE p.chunk_id = c.chunk_id)",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };
    let replicated = || -> u64 {
        let replica = Aggregator::open(&dir.path().join(".test-cache/kio/aggregator.sqlite"))
            .expect("the replica must exist after indexing");
        replica.corpus_size().unwrap().1
    };
    let all_committed = committed();
    assert_eq!(
        replicated(),
        all_committed as u64,
        "premise: with nothing deleted, live and committed agree"
    );

    fs::remove_file(dir.path().join("d2.md")).unwrap();
    json_success(&dir, &["index", "--offline"]);

    assert_eq!(
        committed(),
        all_committed,
        "premise: the deleted file's chunks stay in the index — that is history"
    );
    assert_eq!(
        replicated(),
        all_committed as u64,
        "the deleted file's committed chunks remain in the one replica corpus; \
         liveness is selected at query time"
    );
}

/// R25-4/R25-12: `reindex --at` rotates `index_generation`.
///
/// It publishes chunk TEXT into the live `sqlite.db` in place — no temp+rename
/// — and until R25 it rotated nowhere in the whole command. Two things break on
/// that. LC25: a command that republishes the text corpus can certainly make a
/// cursor replay rank differently, so outstanding cursors must retire. And
/// R25-4: the stamp retires cursor snapshots whenever the published corpus can
/// change. A failed write-through leaves the replica `Rebuilding` and direct
/// search fails closed; it is never repaired lazily by a reader.
#[test]
fn ct3_multi_018_reindex_at_rotates_the_generation_it_publishes_under() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.md"), "# A\n\n## Sec\nzephyrterm here\n").unwrap();
    kio(&dir, &["init"]).assert().success();
    json_success(&dir, &["index", "--offline"]);

    let generation = || -> String {
        let db = dir.path().join(".kio/index/sqlite.db");
        let conn = rusqlite::Connection::open(db).unwrap();
        conn.query_row(
            "SELECT index_generation FROM index_metadata WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };
    let before = generation();
    json_success(&dir, &["reindex", "--at", "HEAD"]);
    assert_ne!(
        before,
        generation(),
        "an in-place republication of the text corpus must mint a fresh \
         generation, or nothing can ever notice it went wrong"
    );
}

/// R25-5: purge does not report success while the purged text is still
/// readable in the device replica.
///
/// Everywhere else the replica is a cache and a cache may not break a command
/// (03 §4) — a lost write costs one re-projection. Purge is the exception, and
/// not by degree: the "cache" is a second copy, on this device, under the cache
/// root, of the text the user invoked a legal instrument to make stop existing.
#[test]
fn ct3_multi_019_purge_fails_closed_when_the_replica_cannot_be_cleared() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let a = parent.path().join("a");
    fs::create_dir_all(&a).unwrap();
    fs::write(a.join("a.md"), "# A\n\n## Sec\nzephyrterm secret\n").unwrap();
    // Keep one non-purged chunk so the test begins with a non-empty, readable
    // Ready replica. A failed refresh must still take that old replica out of
    // service.
    fs::write(a.join("survivor.md"), "# Survivor\n\nprojection survivor\n").unwrap();
    json_success_path(&a, &data_home, &["init"]);
    json_success_path(&a, &data_home, &["index", "--offline"]);

    let ready = json_success_path(&a, &data_home, &["search", "zephyrterm"]);
    assert!(
        ready["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|result| result["snippet"]
                .as_str()
                .is_some_and(|text| text.contains("zephyrterm secret"))),
        "fixture must start with a readable Ready replica: {ready:#?}"
    );

    // Force the complete replica replacement to fail while preserving a
    // readable, formerly Ready database. This is stronger than making the
    // aggregator impossible to open: after the failed purge, the stale secret
    // row must still be made ineligible to direct search.
    let replica_path = data_home.join("cache/kio/aggregator.sqlite");

    let stderr = hermetic_kio_command()
        .current_dir(&a)
        .env("XDG_CONFIG_HOME", data_home.join("config"))
        .env("XDG_DATA_HOME", data_home.join("data"))
        .env("XDG_CACHE_HOME", data_home.join("cache"))
        .env("KIO_TEST_AGGREGATOR_PROJECTION_FAULT", "refresh")
        .args(["purge", "a.md", "--reason", "legal", "--yes", "--json"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let error: Value = serde_json::from_slice(&stderr).unwrap_or_else(|parse_error| {
        panic!(
            "purge failure must be structured JSON ({parse_error}): {}",
            String::from_utf8_lossy(&stderr)
        )
    });
    assert_eq!(error["error_code"], "KIO-E-PURGE-REPLICA-001");

    let scope_id = read_scope_id(&a);
    let header = Aggregator::open(&replica_path)
        .unwrap()
        .scope_header(&scope_id)
        .unwrap()
        .expect("existing replica header must be retained for fail-closed status");
    assert_eq!(header.index_status, AggIndexStatus::Rebuilding);

    // The reader must not compensate by reopening the source index either.
    let source = a.join(".kio/index/sqlite.db");
    let hidden_source = a.join(".kio/index/sqlite.db.hidden-after-purge");
    fs::rename(&source, &hidden_source).unwrap();
    let search_assert = hermetic_kio_command()
        .current_dir(&a)
        .env("XDG_CONFIG_HOME", data_home.join("config"))
        .env("XDG_DATA_HOME", data_home.join("data"))
        .env("XDG_CACHE_HOME", data_home.join("cache"))
        .args(["search", "zephyrterm", "--json"])
        .assert()
        .code(3);
    let output = search_assert.get_output();
    let search_error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(search_error["error_code"], "KIO-E-INDEX-REBUILDING-001");
    let rendered = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        !rendered.contains("zephyrterm secret"),
        "a failed purge must not leave stale replica text searchable: {rendered}"
    );
}

/// An active purge journal is a candidate-time barrier for the device replica.
/// The replica already contains `a`'s row before the journal starts, so this
/// fails if the replica route stops checking the candidate scopes it returns.
#[test]
fn ct3_multi_021_replica_candidates_exclude_an_active_purge_scope() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let a = parent.path().join("a");
    let b = parent.path().join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(
        a.join("secret.md"),
        "# Secret\n\n## Restricted\npurgebarrierneedle active-scope-secret\n",
    )
    .unwrap();
    fs::write(
        b.join("public.md"),
        "# Public\n\n## Available\npurgebarrierneedle public-answer\n",
    )
    .unwrap();
    for dir in [&a, &b] {
        json_success_path(dir, &data_home, &["init"]);
        json_success_path(dir, &data_home, &["index", "--offline"]);
    }
    let a_scope_id = read_scope_id(&a);
    let raw_hash: String = rusqlite::Connection::open(a.join(".kio/index/sqlite.db"))
        .unwrap()
        .query_row(
            "SELECT c.raw_hash FROM chunks c WHERE EXISTS (SELECT 1 FROM chunk_publications p WHERE p.chunk_id = c.chunk_id) LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();

    // Write a valid prepared journal without performing the destructive part
    // of purge. `ReadBarrierCheckpoint::open` must reject it before a
    // replica-produced candidate can cross the response boundary.
    let state = kio_core::purge::PurgeState::open(a.join(".kio")).unwrap();
    let purge_id = kio_core::scope::new_ulid(&a);
    let closure = kio_core::purge::PurgeClosure::new(
        purge_id.clone(),
        vec![kio_core::purge::ClosureItem {
            object_type: "raw".to_owned(),
            hash: raw_hash.clone(),
        }],
        Vec::new(),
    )
    .unwrap();
    let closure_hash = kio_core::purge::closure_content_hash(&closure).unwrap();
    state.write_closure(&closure).unwrap();
    let started = state
        .begin(
            vec![raw_hash],
            kio_core::purge::PurgeReason::Legal,
            kio_core::purge::TombstoneMode::Default,
            "test",
            "2026-08-12T00:00:00Z",
            1,
            kio_pipeline::prepare::hash_bytes(b"replica candidate purge barrier"),
            closure_hash,
            purge_id,
        )
        .unwrap();
    assert!(matches!(started, kio_core::purge::BeginOutcome::Started(_)));

    let output = hermetic_kio_command()
        .current_dir(&b)
        .env("XDG_CONFIG_HOME", data_home.join("config"))
        .env("XDG_DATA_HOME", data_home.join("data"))
        .env("XDG_CACHE_HOME", data_home.join("cache"))
        .args(["search", "purgebarrierneedle", "--mode", "text", "--json"])
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let response: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(response["searched_scopes"].as_array().unwrap().len(), 1);
    assert!(
        response["excluded_scopes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|scope| {
                scope["scope_id"] == a_scope_id && scope["reason"] == "purge_journal_active"
            }),
        "the active scope must be excluded by the replica route: {response:#?}"
    );
    assert!(
        response["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|hit| { hit["evidence_pointer"]["scope_id"] != a_scope_id }),
        "no candidate from the active-purge scope may survive: {response:#?}"
    );
    assert!(
        !serde_json::to_string(&response)
            .unwrap()
            .contains("active-scope-secret"),
        "the active scope's body must not leak through the replica: {response:#?}"
    );
}

/// A candidate-scope journal can appear after the first replica barrier
/// recheck while pointer/snippet/cursor materialization is still in progress.
/// The final response-boundary recheck must discard the whole assembled body:
/// even a title or Evidence Pointer from the newly blocked scope is a leak.
#[test]
fn ct3_multi_022_replica_response_boundary_rechecks_a_late_purge() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let scope = parent.path().join("scope");
    fs::create_dir_all(&scope).unwrap();
    fs::write(
        scope.join("secret.md"),
        "# Secret\n\n## Restricted\nresponsebarrierneedle response-boundary-secret\n",
    )
    .unwrap();
    json_success_path(&scope, &data_home, &["init"]);
    json_success_path(&scope, &data_home, &["index", "--offline"]);

    let raw_hash: String = rusqlite::Connection::open(scope.join(".kio/index/sqlite.db"))
        .unwrap()
        .query_row(
            "SELECT c.raw_hash FROM chunks c WHERE EXISTS (SELECT 1 FROM chunk_publications p WHERE p.chunk_id = c.chunk_id) LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let ready = parent.path().join("response-boundary.ready");
    let release = ready.with_extension("release");
    let bin = assert_cmd::cargo::cargo_bin("kio");
    let mut child = hermetic_process_command(&bin)
        .current_dir(&scope)
        .env("XDG_CONFIG_HOME", data_home.join("config"))
        .env("XDG_DATA_HOME", data_home.join("data"))
        .env("XDG_CACHE_HOME", data_home.join("cache"))
        .env("KIO_TEST_SEARCH_RESPONSE_BARRIER_READY", &ready)
        .args([
            "search",
            "responsebarrierneedle",
            "--mode",
            "text",
            "--json",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("search exited before the response-boundary hook was reached: {status}");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!(
                "timed out waiting for response-boundary hook: {}",
                ready.display()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    // Publish the same valid prepared journal as CT3-MULTI-021, but only after
    // candidate selection's first barrier check has passed.  The child is held
    // at the exact final response boundary above, so no timing assumption is
    // hidden in this regression.
    let state = kio_core::purge::PurgeState::open(scope.join(".kio")).unwrap();
    let purge_id = kio_core::scope::new_ulid(&scope);
    let closure = kio_core::purge::PurgeClosure::new(
        purge_id.clone(),
        vec![kio_core::purge::ClosureItem {
            object_type: "raw".to_owned(),
            hash: raw_hash.clone(),
        }],
        Vec::new(),
    )
    .unwrap();
    let closure_hash = kio_core::purge::closure_content_hash(&closure).unwrap();
    state.write_closure(&closure).unwrap();
    let started = state
        .begin(
            vec![raw_hash],
            kio_core::purge::PurgeReason::Legal,
            kio_core::purge::TombstoneMode::Default,
            "test",
            "2026-08-12T00:00:00Z",
            1,
            kio_pipeline::prepare::hash_bytes(b"replica response-boundary purge barrier"),
            closure_hash,
            purge_id,
        )
        .unwrap();
    assert!(matches!(started, kio_core::purge::BeginOutcome::Started(_)));
    fs::write(&release, b"continue").unwrap();

    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(3),
        "a late purge must reject the fully assembled response: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        output.stdout.is_empty(),
        "the response body must be discarded after the final purge recheck: {}",
        String::from_utf8_lossy(&output.stdout),
    );
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error_code"], "KIO-E-PURGE-JOURNAL-ACTIVE-001");
    let rendered = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        !rendered.contains("response-boundary-secret"),
        "the late-purged scope must not leak title/snippet/pointer data: {rendered}"
    );
}

/// A single-scope search still uses the collection candidate route. One scope
/// simply contributes all eligible rows, with no cross-scope ranking distinction.
///
/// The write-through is unconditional on purpose. A scope cannot know whether
/// it is alone: `kio index` runs inside one `.kio` folder, and making the write
/// conditional on the device having other scopes would leave the first scope
/// missing from the collection the moment a second one appeared.
#[test]
fn ct3_multi_010_single_scope_search_uses_the_collection_candidate_route() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let a = parent.path().join("a");
    fs::create_dir_all(&a).unwrap();
    fs::write(a.join("a.md"), "# A\n\n## Sec\nzephyrterm alone\n").unwrap();
    json_success_path(&a, &data_home, &["init"]);
    json_success_path(&a, &data_home, &["index"]);

    // Write-through, not a search side effect: nothing has searched yet.
    let replica_path = data_home.join("cache/kio/aggregator.sqlite");
    assert!(
        replica_path.exists(),
        "indexing a scope must replicate it, whether or not anything searches"
    );
    let projected = Aggregator::open(&replica_path)
        .unwrap()
        .corpus_size()
        .unwrap();
    assert_eq!(projected.0, 1, "one scope replicated: {projected:?}");
    assert!(projected.1 > 0, "its chunks came with it: {projected:?}");

    let search = json_success_path(&a, &data_home, &["search", "zephyrterm"]);
    assert_eq!(search["results"].as_array().unwrap().len(), 1);
    assert!(
        (search["results"][0]["score"].as_f64().unwrap() - 1.0 / 61.0).abs() < 1e-12,
        "unchanged collection rank-1 score: {search:#?}"
    );
    let after = Aggregator::open(&replica_path)
        .unwrap()
        .corpus_size()
        .unwrap();
    assert_eq!(
        after, projected,
        "a single-scope search reads nothing and writes nothing here"
    );
}

// diversify runs on the merged pool: max_per_raw_hash caps a raw_hash across scopes.
#[test]
fn ct3_multi_003_diversify_caps_raw_hash_across_scopes() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let dirs = ["s0", "s1", "s2", "s3"]
        .iter()
        .map(|name| parent.path().join(name))
        .collect::<Vec<_>>();
    for dir in &dirs {
        fs::create_dir_all(dir).unwrap();
        // Identical content across scopes -> identical raw_hash.
        fs::write(
            dir.join("dup.md"),
            "# Dup\n\n## Section\nsharedtoken body\n",
        )
        .unwrap();
        json_success_path(dir, &data_home, &["init"]);
        json_success_path(dir, &data_home, &["index"]);
    }
    let search = json_success_path(&dirs[0], &data_home, &["search", "sharedtoken"]);
    assert_eq!(search["searched_scopes"].as_array().unwrap().len(), 4);
    // 4 scopes match the same raw_hash; max_per_raw_hash=3 caps the stream at 3.
    assert_eq!(search["results"].as_array().unwrap().len(), 3);
}

// Real-machine scenario (b): one scope unreachable (chmod 000) -> results + exit 3.
#[cfg(unix)]
#[test]
fn ct3_multi_005_partial_failure_returns_results_with_exit_3() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let a = parent.path().join("a");
    let b = parent.path().join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(a.join("a.md"), "# A\n\n## Sec\nalphaunique token\n").unwrap();
    fs::write(b.join("b.md"), "# B\n\n## Sec\nbetaunique token\n").unwrap();
    json_success_path(&a, &data_home, &["init"]);
    json_success_path(&b, &data_home, &["init"]);
    json_success_path(&a, &data_home, &["index"]);
    json_success_path(&b, &data_home, &["index"]);

    // Make scope b unreachable at discovery (its .kio is unreadable).
    let b_kio = b.join(".kio");
    let mut perms = fs::metadata(&b_kio).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o000);
    fs::set_permissions(&b_kio, perms).unwrap();

    let output = hermetic_kio_command()
        .current_dir(&a)
        .env("XDG_CONFIG_HOME", data_home.join("config"))
        .env("XDG_DATA_HOME", data_home.join("data"))
        .env("XDG_CACHE_HOME", data_home.join("cache"))
        .args(["search", "alphaunique", "--json"])
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();

    // Restore permissions so the tempdir can be cleaned up.
    let mut restore = fs::metadata(&b_kio).unwrap().permissions();
    restore.set_mode(0o755);
    fs::set_permissions(&b_kio, restore).unwrap();

    let search: Value = serde_json::from_slice(&output).unwrap();
    assert!(!search["results"].as_array().unwrap().is_empty());
    assert_eq!(search["searched_scopes"].as_array().unwrap().len(), 1);
    let excluded = search["excluded_scopes"].as_array().unwrap();
    assert_eq!(excluded.len(), 1);
    // R23-20 (03 §4 L296): scope_path is the canonical `.kio` directory.
    assert!(value_path_ends_with(&excluded[0]["scope_path"], "b/.kio"));
    // 05 §1.8: excluded_scopes[] = {scope_id, scope_path, reason} — the reason
    // must be recorded, not just the fact of exclusion.
    assert!(!excluded[0]["reason"].as_str().unwrap().is_empty());
    // The private exit marker never leaks into the payload.
    assert!(search.get("__exit_code").is_none());
}

#[test]
fn ct3_multi_005_all_failed_returns_exit_4() {
    // A scope that is init'd but not indexed is not a search target; with no other
    // indexed scope the search is a permanent all-scope failure.
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.md"), "# A\n\n## Sec\nalpha\n").unwrap();
    kio(&dir, &["init"]).assert().success();
    let err = json_failure(&dir, &["search", "alpha"], 4);
    assert_eq!(err["error_code"], "KIO-E-SEARCH-SCOPE-ALL-FAILED-001");
}

#[test]
fn ct3_multi_006_completion_order_does_not_change_results_or_cursor() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let a = parent.path().join("a");
    let b = parent.path().join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(a.join("a.md"), "# A\n\n## Sec\norderstable token\n").unwrap();
    fs::write(b.join("b.md"), "# B\n\n## Sec\norderstable token\n").unwrap();
    json_success_path(&a, &data_home, &["init"]);
    json_success_path(&b, &data_home, &["init"]);
    replace_scope_id(&a, "7ZZZZZZZZZZZZZZZZZZZZZZZZZ");
    replace_scope_id(&b, "00000000000000000000000001");
    json_success_path(&a, &data_home, &["index"]);
    json_success_path(&b, &data_home, &["index"]);
    fs::write(
        a.join(".kio/config.toml"),
        "[search.multi_scope]\nparallelism = 2\n",
    )
    .unwrap();

    let baseline = json_success_path(&a, &data_home, &["search", "orderstable", "--limit", "1"]);
    let delayed_output = hermetic_kio_command()
        .current_dir(&a)
        .env("XDG_CONFIG_HOME", data_home.join("config"))
        .env("XDG_DATA_HOME", data_home.join("data"))
        .env("XDG_CACHE_HOME", data_home.join("cache"))
        // Registry/input order is a then b. Delay a so b completes first.
        .env(
            "KIO_TEST_SCOPE_SEARCH_DELAY_SCOPE_ID",
            "7ZZZZZZZZZZZZZZZZZZZZZZZZZ",
        )
        .env("KIO_TEST_SCOPE_SEARCH_DELAY_MS", "300")
        .args(["search", "orderstable", "--limit", "1", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let delayed: Value = serde_json::from_slice(&delayed_output).unwrap();

    assert!(baseline["paging"]["next_cursor"].is_string());
    assert_eq!(delayed["results"], baseline["results"]);
    assert_eq!(delayed["searched_scopes"], baseline["searched_scopes"]);
    assert_eq!(delayed["excluded_scopes"], baseline["excluded_scopes"]);
    assert_eq!(delayed["paging"], baseline["paging"]);
}

#[test]
fn ct3_multi_006_timeout_preserves_fresh_all_failed_and_cursor_contracts() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let a = parent.path().join("a");
    let b = parent.path().join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(a.join("a.md"), "# A\n\n## Sec\ntimeouttoken alpha\n").unwrap();
    fs::write(b.join("b.md"), "# B\n\n## Sec\ntimeouttoken beta\n").unwrap();
    json_success_path(&a, &data_home, &["init"]);
    json_success_path(&b, &data_home, &["init"]);
    json_success_path(&a, &data_home, &["index"]);
    json_success_path(&b, &data_home, &["index"]);
    fs::write(
        a.join(".kio/config.toml"),
        "[search.multi_scope]\nparallelism = 2\nper_scope_timeout_seconds = 1\n",
    )
    .unwrap();
    let b_scope_id = read_scope_id(&b);

    // A fresh search isolates the timed-out scope, returns the healthy result,
    // and uses the established partial-failure exit 3 payload contract.
    let partial_output = hermetic_kio_command()
        .current_dir(&a)
        .env("XDG_CONFIG_HOME", data_home.join("config"))
        .env("XDG_DATA_HOME", data_home.join("data"))
        .env("XDG_CACHE_HOME", data_home.join("cache"))
        .env("KIO_TEST_SCOPE_SEARCH_DELAY_SCOPE_ID", &b_scope_id)
        .env("KIO_TEST_SCOPE_SEARCH_DELAY_MS", "2500")
        .args(["search", "timeouttoken", "--json"])
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let partial: Value = serde_json::from_slice(&partial_output).unwrap();
    assert_eq!(partial["searched_scopes"].as_array().unwrap().len(), 1);
    assert_eq!(partial["excluded_scopes"].as_array().unwrap().len(), 1);
    assert_eq!(partial["excluded_scopes"][0]["scope_id"], b_scope_id);
    assert_eq!(partial["excluded_scopes"][0]["reason"], "timeout");
    assert!(!partial["results"].as_array().unwrap().is_empty());
    assert!(partial.get("__exit_code").is_none());

    // A single timed-out scope remains a retryable all-scope failure (exit 3).
    let b_arg = b.display().to_string();
    let all_failed = hermetic_kio_command()
        .current_dir(&a)
        .env("XDG_CONFIG_HOME", data_home.join("config"))
        .env("XDG_DATA_HOME", data_home.join("data"))
        .env("XDG_CACHE_HOME", data_home.join("cache"))
        .env("KIO_TEST_SCOPE_SEARCH_DELAY_SCOPE_ID", &b_scope_id)
        .env("KIO_TEST_SCOPE_SEARCH_DELAY_MS", "2500")
        .args(["search", "timeouttoken", "--scope", &b_arg, "--json"])
        .assert()
        .code(3)
        .get_output()
        .stderr
        .clone();
    let all_failed: Value = serde_json::from_slice(&all_failed).unwrap();
    assert_eq!(
        all_failed["error_code"],
        "KIO-E-SEARCH-SCOPE-ALL-FAILED-001"
    );
    assert_eq!(
        all_failed["context"]["excluded_scopes"][0]["reason"],
        "timeout"
    );

    // Cursor replay cannot shrink its frozen active set. A timeout therefore
    // hard-fails with no partial stdout or replacement cursor.
    let first = json_success_path(&a, &data_home, &["search", "timeouttoken", "--limit", "1"]);
    let cursor = first["paging"]["next_cursor"].as_str().unwrap();
    let replay = hermetic_kio_command()
        .current_dir(&a)
        .env("XDG_CONFIG_HOME", data_home.join("config"))
        .env("XDG_DATA_HOME", data_home.join("data"))
        .env("XDG_CACHE_HOME", data_home.join("cache"))
        .env("KIO_TEST_SCOPE_SEARCH_DELAY_SCOPE_ID", &b_scope_id)
        .env("KIO_TEST_SCOPE_SEARCH_DELAY_MS", "2500")
        .args([
            "search",
            "timeouttoken",
            "--limit",
            "1",
            "--cursor",
            cursor,
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(replay.status.code(), Some(2));
    assert!(
        replay.stdout.is_empty(),
        "cursor failure must not emit a page"
    );
    let replay_error: Value = serde_json::from_slice(&replay.stderr).unwrap();
    assert_eq!(replay_error["error_code"], "KIO-E-SEARCH-CURSOR-001");
    assert_eq!(replay_error["context"]["cause"], "timeout");
}

// Step 4 cursor v2 freezes the complete active scope set. Losing any active
// scope makes replay non-reproducible, so replay hard-fails and instructs a
// fresh search instead of silently shrinking to the surviving scopes.
#[test]
fn ct4_cursor_replay_with_unresolvable_active_scope_hard_fails() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    // "a" sorts first → page 1 serves its chunk (RRF tie-break by scope_path),
    // so the survivor "b" has consumed 0 and still owns results for page 2.
    let a = parent.path().join("a");
    let b = parent.path().join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(
        a.join("a.md"),
        "# 共通仕様\n\n## Alpha\n共通トピック alpha 版です。\n",
    )
    .unwrap();
    fs::write(
        b.join("b.md"),
        "# 共通仕様\n\n## Beta\n共通トピック beta 版です。\n",
    )
    .unwrap();
    json_success_path(&a, &data_home, &["init"]);
    json_success_path(&b, &data_home, &["init"]);
    json_success_path(&a, &data_home, &["index"]);
    json_success_path(&b, &data_home, &["index"]);
    let a_scope: Value =
        serde_json::from_str(&fs::read_to_string(a.join(".kio/scope.json")).unwrap()).unwrap();
    let a_scope_id = a_scope["scope_id"].as_str().unwrap().to_owned();

    let first = json_success_path(&b, &data_home, &["search", "共通トピック", "--limit", "1"]);
    assert_eq!(first["searched_scopes"].as_array().unwrap().len(), 2);
    let cursor = first["paging"]["next_cursor"].as_str().unwrap().to_owned();

    // Make scope a unresolvable: wipe the registry, re-register b only.
    fs::remove_file(registry_path(&data_home)).unwrap();
    json_success_path(&b, &data_home, &["index"]);

    let (code, error) = run_json(
        &b,
        &data_home,
        &[
            "search",
            "共通トピック",
            "--cursor",
            &cursor,
            "--limit",
            "5",
        ],
    );
    assert_eq!(code, 2, "active-scope loss is a cursor misuse: {error}");
    assert_eq!(error["error_code"], "KIO-E-SEARCH-CURSOR-001");
    assert_eq!(error["context"]["reason"], "active_scope_unavailable");
    assert_eq!(error["context"]["scope_id"], a_scope_id);
}

// CT3-EMBED-003 (de-tautologized): cross-scope embedding profiles disagree — scope
// a has a compatible embedding index, scope b an incompatible one. The cross-scope
// search must merge on text only and record the fallback (05 §1.8(5) / 03 §7).
#[test]
fn ct3_embed_003_cross_scope_incompatibility_falls_back_to_text_merge() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let a = parent.path().join("a");
    let b = parent.path().join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(a.join("a.md"), "# A\n\n## One\nalpha 111\n").unwrap();
    fs::write(b.join("b.md"), "# B\n\n## Two\nbeta 222\n").unwrap();
    run_embed_path(&a, &data_home, "mock", &["init"]);
    run_embed_path(&b, &data_home, "mock", &["init"]);
    run_embed_path(&a, &data_home, "mock", &["ledger", "init"]);
    // a: compatible embeddings; b: incompatible embeddings.
    run_embed_path(&a, &data_home, "mock", &["index"]);
    run_embed_path(&b, &data_home, "incompatible_profile", &["index"]);
    let search = run_embed_path(
        &a,
        &data_home,
        "mock",
        &["search", "beta 222", "--all-scopes"],
    );
    assert_eq!(search["resolved_mode"], "text");
    assert_eq!(search["fallback"], true);
    assert!(search["fallback_reason"].is_string());
    // The cross-scope text merge still returns b's content.
    assert!(!search["results"].as_array().unwrap().is_empty());
}

// CT3-EMBED-008 / scenario (e): a non-multimodal embedding profile is rejected at
// `kio index` (tool-lock materialize) with KIO-E-EMBED-MODALITY-001 and exit 2 —
// no embeddings are written.
// Naming convention fix (ct3_<domain>_<nnn>_<description>): was
// ct3_embed_modality_..., missing the CT3-EMBED-008 number.
#[test]
fn ct3_embed_008_non_multimodal_profile_is_rejected_at_index() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.md"), "# A\n\n## One\nalpha 111\n").unwrap();
    kio(&dir, &["init"]).assert().success();
    let err = json_failure_embed(&dir, "non_multimodal", &["index"], 2);
    assert_eq!(err["error_code"], "KIO-E-EMBED-MODALITY-001");
}

// CT3-EMBED-005: `repair rebuild-db` re-derives chunk_vec from the preserved
// `embeddings` rows (source of truth), so hybrid vector search survives the
// rebuild rather than falling back to text.
#[test]
fn ct3_embed_005_rebuild_db_preserves_vector_search() {
    let dir = indexed_scope_embed("mock");
    let before = json_success_embed(&dir, "mock", &["search", "認証仕様 トークン"]);
    assert_eq!(before["resolved_mode"], "hybrid");
    json_success_embed(&dir, "mock", &["repair", "rebuild-db"]);
    let after = json_success_embed(&dir, "mock", &["search", "認証仕様 トークン"]);
    assert_eq!(after["resolved_mode"], "hybrid");
    assert_eq!(after["fallback"], false);
    assert_eq!(after["diversify"]["strategy"], "mmr");
}

/// R25-6: `repair rebuild-db` restores vector search from `objects/`, not from
/// the database it is about to replace.
///
/// The spec has always listed `embedding (CAS)` among the object types and put
/// the rebuild order at `objects/` → `embeddings` → `chunk_vec` (04 §4.3). No
/// code wrote those objects, so `rebuild-db` snapshotted vectors out of the
/// very database it was replacing — a guarantee that held only while that
/// database was intact, which is the one condition under which nobody runs it.
/// Deleting `sqlite.db` meant buying every vector from the API again.
///
/// So this test deletes it. `ct3_embed_005` covers the intact case, which the
/// old snapshot path also passed.
#[test]
fn ct3_embed_009_rebuild_db_restores_vectors_from_objects_after_the_db_is_lost() {
    let dir = indexed_scope_embed("mock");
    let before = json_success_embed(&dir, "mock", &["search", "認証仕様 トークン"]);
    assert_eq!(before["resolved_mode"], "hybrid");

    let objects = dir.path().join(".kio/objects/embeddings");
    assert!(
        objects.is_dir(),
        "indexing must publish embedding objects, or there is nothing to rebuild from"
    );

    // The acceleration layer is gone. Only `objects/` remains.
    fs::remove_file(dir.path().join(".kio/index/sqlite.db")).unwrap();
    // Rebuilt with NO embedding adapter configured, so re-sending is not merely
    // undesirable but impossible: any vector that comes back came off disk.
    json_success(&dir, &["repair", "rebuild-db"]);

    let after = json_success_embed(&dir, "mock", &["search", "認証仕様 トークン"]);
    assert_eq!(
        after["resolved_mode"], "hybrid",
        "vector search must come back without re-sending anything: {after:#?}"
    );
    assert_eq!(after["fallback"], false);
    assert_eq!(after["diversify"]["strategy"], "mmr");
}

/// 04 §4.3: the persisted lock is the rebuild selector.  Removing its
/// `embedding` entry makes the current scope text-only even when old vector
/// objects remain in CAS; a repair must not adopt them by discovery.
#[test]
fn rebuild_db_without_embedding_lock_does_not_replay_vectors() {
    let dir = indexed_scope_embed("mock");
    let lock_path = dir.path().join(".kio/tool-lock.json");
    let mut lock: serde_json::Value =
        serde_json::from_slice(&fs::read(&lock_path).unwrap()).unwrap();
    lock.as_object_mut().unwrap().remove("embedding");
    fs::write(&lock_path, serde_json::to_vec_pretty(&lock).unwrap()).unwrap();

    fs::remove_file(dir.path().join(".kio/index/sqlite.db")).unwrap();
    json_success(&dir, &["repair", "rebuild-db"]);
    let after = json_success_embed(&dir, "mock", &["search", "認証仕様 トークン"]);
    assert_eq!(after["resolved_mode"], "text");
    assert_eq!(after["fallback"], true);
}

/// The post-rebuild enrichment pass is also lock-selected: an active local
/// adapter must not repopulate a scope whose persisted current lock is
/// text-only.
#[test]
fn rebuild_db_with_active_adapter_and_no_embedding_lock_stays_text_only() {
    let dir = indexed_scope_embed("mock");
    let lock_path = dir.path().join(".kio/tool-lock.json");
    let mut lock: serde_json::Value =
        serde_json::from_slice(&fs::read(&lock_path).unwrap()).unwrap();
    lock.as_object_mut().unwrap().remove("embedding");
    fs::write(&lock_path, serde_json::to_vec_pretty(&lock).unwrap()).unwrap();

    fs::remove_file(dir.path().join(".kio/index/sqlite.db")).unwrap();
    let rebuilt = json_success_embed(&dir, "mock", &["repair", "rebuild-db"]);
    assert_eq!(rebuilt["embedding_tasks_executed"], 0);
    let after = json_success_embed(&dir, "mock", &["search", "認証仕様 トークン"]);
    assert_eq!(after["resolved_mode"], "text");
    assert_eq!(after["fallback"], true);
}

/// An adapter profile drift cannot overwrite the persisted lock profile during
/// repair; it is skipped until an explicit `index` accepts and materializes it.
#[test]
fn rebuild_db_with_active_mismatched_profile_skips_enrichment() {
    let dir = indexed_scope_embed("mock");
    let lock_path = dir.path().join(".kio/tool-lock.json");
    let before_lock = fs::read(&lock_path).unwrap();
    fs::remove_file(dir.path().join(".kio/index/sqlite.db")).unwrap();

    let rebuilt = json_success_embed(&dir, "incompatible_profile", &["repair", "rebuild-db"]);
    assert_eq!(rebuilt["embedding_tasks_executed"], 0);
    assert_eq!(fs::read(&lock_path).unwrap(), before_lock);

    let after = json_success_embed(&dir, "mock", &["search", "認証仕様 トークン"]);
    assert_eq!(after["resolved_mode"], "hybrid");
    assert_eq!(after["fallback"], false);
}

#[test]
fn ct3_obs_002_metrics_do_not_record_query_text() {
    let dir = indexed_scope();
    json_success(&dir, &["search", "secret query phrase 3600"]);
    let metrics = fs::read_to_string(dir.path().join(".test-data/kio/logs/metrics.jsonl")).unwrap();
    assert!(!metrics.contains("secret query phrase"));
}

#[test]
fn ct3_obs_003_access_log_has_required_envelope_fields() {
    let dir = indexed_scope();
    json_success(&dir, &["search", "トークン"]);
    let access = fs::read_to_string(dir.path().join(".kio/logs/access.jsonl")).unwrap();
    let last: Value = serde_json::from_str(access.lines().last().unwrap()).unwrap();
    for key in ["ts", "level", "code", "component", "message", "context"] {
        assert!(last.get(key).is_some(), "missing {key}");
    }
}

// Real-machine scenario (c): a cursor freezes the chunk set by max_rowid; chunks
// indexed after the cursor was issued do not leak into page 2.
#[test]
fn pc19_pc21_index_generation_rotation_rejects_a_cursor_after_new_content_is_indexed() {
    // PC19/PC21 (05 §1.5 L180-191): `index_generation` rotates whenever
    // `chunk_fts` content changes (one of the 6 listed triggers — "index /
    // batch finalize で chunk_fts の内容が変化した場合"), and a page-2 replay
    // whose frozen `index_generation` no longer matches the scope's current
    // value is rejected with `KIO-E-SEARCH-CURSOR-001` ("再検索が正") rather
    // than silently degrading via `max_rowid` alone — later-indexed content
    // can shift the BM25 ranking of already-existing rows (FTS5's `bm25()`
    // uses corpus-wide document-frequency/average-length statistics), so a
    // `max_rowid` freeze alone is not sufficient to keep a page-2 replay
    // correct. This supersedes the pre-PC19 contract (a `max_rowid`-only
    // freeze that let a page-2 replay silently continue after new content was
    // indexed, filtering out only the new rows by rowid).
    let dir = indexed_scope();
    let first = json_success(&dir, &["search", "認証仕様", "--limit", "1"]);
    let cursor = first["paging"]["next_cursor"].as_str().unwrap().to_owned();

    // Append a new file that also matches the query, then re-index (HEAD
    // advances, chunk_fts content changes — one of PC20's 6 rotation
    // triggers).
    fs::write(
        dir.path().join("addendum.md"),
        "# 認証仕様の追補\n\n## 追補\nposttoken マーカー を追加しました。\n",
    )
    .unwrap();
    json_success(&dir, &["index"]);

    // A fresh search sees the new chunk (proves the setup is live)...
    let fresh = json_success(&dir, &["search", "認証仕様", "--limit", "100"]);
    let fresh_has_new = fresh["results"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["snippet"].as_str().unwrap_or("").contains("posttoken"));
    assert!(fresh_has_new, "fresh search must include the new chunk");

    // ...and the frozen page-1 cursor is now rejected (re-search is correct),
    // not silently replayed with the new content filtered out by rowid alone.
    let err = json_failure(
        &dir,
        &["search", "認証仕様", "--cursor", &cursor, "--limit", "100"],
        2,
    );
    assert_eq!(err["error_code"], "KIO-E-SEARCH-CURSOR-001");
    assert_eq!(err["context"]["reason"], "index_generation_mismatch");
}

#[test]
fn ct3_cursor_005_unreceipted_missing_tree_is_store_corruption() {
    let dir = indexed_scope();
    let first = json_success(&dir, &["search", "認証仕様", "--limit", "1"]);
    let cursor = first["paging"]["next_cursor"].as_str().unwrap().to_owned();
    // GC never sweeps a ref tip and never removes a tree without a receipt.
    // A manually missing current tree is corruption, not a valid shallow cursor.
    let kio_dir = dir.path().join(".kio");
    let head = head_commit(&kio_dir);
    let commit: Value =
        serde_json::from_slice(&fs::read(object_path(&kio_dir, "commits", &head)).unwrap())
            .unwrap();
    fs::remove_file(object_path(
        &kio_dir,
        "trees",
        commit["tree"].as_str().unwrap(),
    ))
    .unwrap();
    let err = json_failure(
        &dir,
        &["search", "認証仕様", "--cursor", &cursor, "--limit", "100"],
        4,
    );
    assert_eq!(err["error_code"], "KIO-E-STORE-CORRUPT-001");
}

#[test]
fn ct3_obs_001_index_status_reports_partial_enrichment() {
    // R9-2: text-native files no longer enqueue an online task, so the pending
    // enrichment CT3-OBS-001 measures comes from a PDF whose online markdownize
    // stays Pending after an offline index (the deterministic baseline is Done).
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("report.pdf"),
        fake_pdf(&["認証仕様 トークン TTL 3600 のテスト本文"]),
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    json_success(&dir, &["index"]);
    let search = json_success(&dir, &["search", "認証仕様"]);
    let status = &search["index_status"];
    assert!(status.is_object());
    // Offline index leaves online markdownize enhancement pending.
    assert!(status["enriched_ratio"].as_f64().unwrap() < 1.0);
    assert!(status["pending_enrichment_tasks"].as_u64().unwrap() > 0);
    assert_eq!(status["budget_paused"], false);
}

/// A completed index writes its candidates through to the device replica.
/// Once that has happened, direct search must not reopen the per-scope source
/// index merely to select or materialize a result. Renaming (rather than
/// deleting) is safe on Windows and lets the Drop guard restore the source
/// database even if the assertion fails.
#[test]
fn ct3_replica_001_direct_search_survives_a_temporarily_hidden_source_index() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("replica.md"),
        "# Replica-only route\n\nreplicaonlydirectneedle must come from aggregator\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    json_success(&dir, &["index", "--offline"]);

    let scope_id = read_scope_id(dir.path());
    let replica_path = dir.path().join(".test-cache/kio/aggregator.sqlite");
    let replica = Aggregator::open(&replica_path).unwrap();
    assert!(
        replica.scope_generation(&scope_id).unwrap().is_some(),
        "index must write this scope through to the device replica"
    );
    drop(replica);

    let source_index = dir.path().join(".kio/index/sqlite.db");
    assert!(source_index.is_file());
    let hidden = HiddenSourceIndex::hide(&source_index);
    assert!(
        !source_index.exists(),
        "the command must run while the source index is unavailable"
    );

    let response = json_success(
        &dir,
        &["search", "replicaonlydirectneedle", "--mode", "text"],
    );
    let _generation = replica_collection_generation(&response);
    let results = response["results"].as_array().unwrap();
    assert!(
        !results.is_empty(),
        "the write-through replica must return the indexed match: {response:#?}"
    );
    assert!(
        results
            .iter()
            .any(|hit| hit["evidence_pointer"]["scope_id"] == scope_id),
        "the result must still identify the indexed scope: {response:#?}"
    );
    assert!(
        !source_index.exists(),
        "direct search must not recreate or replace the hidden source index"
    );

    hidden.restore();
    assert!(source_index.is_file(), "the source index must be restored");
}

/// A manual snapshot moves HEAD before the source sqlite projection catches
/// up. The previous replica rows remain useful evidence for a later rebuild,
/// but must not be certified as a strict current-snapshot answer.
#[test]
fn ct3_replica_002_snapshot_marks_the_prior_projection_rebuilding() {
    let dir = indexed_scope();
    let scope_id = read_scope_id(dir.path());
    let replica_path = dir.path().join(".test-cache/kio/aggregator.sqlite");
    let head_before = fs::read_to_string(dir.path().join(".kio/HEAD")).unwrap();
    let header_before = Aggregator::open(&replica_path)
        .unwrap()
        .scope_header(&scope_id)
        .unwrap()
        .unwrap();
    assert_eq!(header_before.index_status, AggIndexStatus::Ready);
    assert_eq!(
        header_before.current_snapshot_commit.as_deref(),
        Some(head_before.trim()),
        "the indexed projection must begin at the source HEAD"
    );

    fs::write(
        dir.path().join("auth.md"),
        "# Updated auth specification\n\nnew snapshot-only content\n",
    )
    .unwrap();
    let snapshot = json_success(&dir, &["snapshot", "create", "--message", "snapshot only"]);
    assert_eq!(snapshot["status"], "created", "{snapshot}");
    let head_after = fs::read_to_string(dir.path().join(".kio/HEAD")).unwrap();
    assert_ne!(head_after, head_before, "the test must advance HEAD");

    let header_after = Aggregator::open(&replica_path)
        .unwrap()
        .scope_header(&scope_id)
        .unwrap()
        .unwrap();
    assert_eq!(header_after.index_status, AggIndexStatus::Rebuilding);
    assert_eq!(
        header_after.current_snapshot_commit.as_deref(),
        Some(head_after.trim()),
        "the rebuilding header must name the new HEAD without certifying stale rows ready"
    );
}

/// An auto snapshot advances HEAD before `rebuild_step3_index` has rebuilt the
/// source SQLite cache. Even if a crash lands in the tiny interval before the
/// writer marks its old Ready header Rebuilding, direct current search and a
/// current cursor must reject that stale header by comparing it with CAS HEAD.
#[test]
fn ct3_replica_006_index_head_advance_fails_closed_before_source_rebuild() {
    let dir = indexed_scope();
    let scope_id = read_scope_id(dir.path());
    let replica_path = dir.path().join(".test-cache/kio/aggregator.sqlite");
    let indexed_raw_hash: String = Connection::open(dir.path().join(".kio/index/sqlite.db"))
        .unwrap()
        .query_row(
            "SELECT c.raw_hash FROM chunks c WHERE EXISTS (SELECT 1 FROM chunk_publications p WHERE p.chunk_id = c.chunk_id) ORDER BY c.rowid LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let head_before = fs::read_to_string(dir.path().join(".kio/HEAD")).unwrap();
    let header_before = Aggregator::open(&replica_path)
        .unwrap()
        .scope_header(&scope_id)
        .unwrap()
        .unwrap();
    assert_eq!(header_before.index_status, AggIndexStatus::Ready);
    let cursor =
        json_success(&dir, &["search", "認証仕様", "--limit", "1"])["paging"]["next_cursor"]
            .as_str()
            .unwrap()
            .to_owned();

    fs::write(
        dir.path().join("auth.md"),
        "# Updated auth specification\n\npostheadreplicafailclosed new content\n",
    )
    .unwrap();
    let stderr = kio(&dir, &["index", "--offline"])
        .env("KIO_TEST_REPLICA_AFTER_HEAD_FAULT", "index_before_marker")
        .arg("--json")
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();
    let fault: Value = serde_json::from_slice(&stderr).unwrap();
    assert_eq!(fault["error_code"], "KIO-E-REPLICA-AFTER-HEAD-FAULT-001");

    let head_after = fs::read_to_string(dir.path().join(".kio/HEAD")).unwrap();
    assert_ne!(
        head_after, head_before,
        "the injected path must advance HEAD"
    );
    let header_after = Aggregator::open(&replica_path)
        .unwrap()
        .scope_header(&scope_id)
        .unwrap()
        .unwrap();
    assert_eq!(header_after.index_status, AggIndexStatus::Ready);
    assert_eq!(
        header_after.current_snapshot_commit.as_deref(),
        Some(head_before.trim()),
        "the injected pre-marker interval must retain the old Ready header"
    );

    let search_error = json_failure(
        &dir,
        &["search", "postheadreplicafailclosed", "--mode", "text"],
        3,
    );
    assert_eq!(search_error["error_code"], "KIO-E-INDEX-REBUILDING-001");
    assert_eq!(
        search_error["context"]["excluded_scopes"][0]["reason"],
        "index_rebuilding"
    );
    for selector in ["--all-history", "--include-deleted"] {
        let selector_error =
            json_failure(&dir, &["search", "認証仕様", selector, "--limit", "1"], 3);
        assert_eq!(
            selector_error["error_code"], "KIO-E-INDEX-REBUILDING-001",
            "{selector} must not trust the stale Ready header"
        );
    }

    let cursor_error = json_failure(
        &dir,
        &["search", "認証仕様", "--cursor", &cursor, "--limit", "1"],
        2,
    );
    assert_eq!(cursor_error["error_code"], "KIO-E-SEARCH-CURSOR-001");
    assert_eq!(cursor_error["context"]["cause"], "index_rebuilding");

    // An explicit historical target has its own completed marker/binding
    // relation. It remains independent of the current HEAD mismatch.
    let historical = json_success(
        &dir,
        &[
            "search",
            "認証仕様",
            "--at",
            head_before.trim(),
            "--scope",
            ".",
            "--limit",
            "1",
        ],
    );
    assert!(
        !historical["results"].as_array().unwrap().is_empty(),
        "an explicit completed historical marker must remain usable: {historical:#?}"
    );

    // A committed purge can advance HEAD while its journal remains active and
    // the last Ready replica header still names the prior coherent snapshot.
    // That is not permission to promote the stale-header condition above to
    // `index_rebuilding`: the replica must select the old candidate only far
    // enough to let the candidate-scope purge barrier reject it. Hide source
    // SQLite as well, so this verifies the strict replica/CAS route rather
    // than a source-index fallback.
    let state = kio_core::purge::PurgeState::open(dir.path().join(".kio")).unwrap();
    let purge_id = kio_core::scope::new_ulid(dir.path());
    let closure = kio_core::purge::PurgeClosure::new(
        purge_id.clone(),
        vec![kio_core::purge::ClosureItem {
            object_type: "raw".to_owned(),
            hash: indexed_raw_hash.clone(),
        }],
        Vec::new(),
    )
    .unwrap();
    let closure_hash = kio_core::purge::closure_content_hash(&closure).unwrap();
    state.write_closure(&closure).unwrap();
    let journal = match state
        .begin(
            vec![indexed_raw_hash],
            kio_core::purge::PurgeReason::Legal,
            kio_core::purge::TombstoneMode::Default,
            "test",
            "2026-08-12T00:00:00Z",
            1,
            kio_pipeline::prepare::hash_bytes(b"stale ready header purge priority"),
            closure_hash,
            purge_id,
        )
        .unwrap()
    {
        kio_core::purge::BeginOutcome::Started(journal) => journal,
        other => panic!("test journal must start: {other:?}"),
    };
    let hidden_source = HiddenSourceIndex::hide(&dir.path().join(".kio/index/sqlite.db"));
    let journal_error = json_failure(&dir, &["search", "admin", "--mode", "text"], 3);
    assert_eq!(
        journal_error["error_code"], "KIO-E-PURGE-JOURNAL-ACTIVE-001",
        "an active journal takes priority over this stale header only at the selected candidate scope"
    );
    assert!(
        journal_error["context"]["excluded_scopes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|scope| scope["reason"] == "purge_journal_active"),
        "the stale replica candidate must be discarded by the purge barrier: {journal_error:#?}"
    );
    hidden_source.restore();
    state.abort_before_barrier(&journal).unwrap();

    // A later normal writer pass replaces the source index and publishes a
    // complete Ready projection; the fail-closed transition is recoverable.
    json_success(&dir, &["index", "--offline"]);
    let recovered = json_success(
        &dir,
        &["search", "postheadreplicafailclosed", "--mode", "text"],
    );
    assert!(
        !recovered["results"].as_array().unwrap().is_empty(),
        "a completed writer projection must restore direct search: {recovered:#?}"
    );
}

/// `reindex --regenerate` also publishes a new HEAD before its source rebuild.
/// Its pre-marker crash window is covered by the same reader-side HEAD guard.
#[test]
fn ct3_replica_007_regenerate_head_advance_fails_closed_before_source_rebuild() {
    let dir = indexed_scope();
    let scope_id = read_scope_id(dir.path());
    let replica_path = dir.path().join(".test-cache/kio/aggregator.sqlite");
    let head_before = fs::read_to_string(dir.path().join(".kio/HEAD")).unwrap();

    let stderr = kio(&dir, &["reindex", "--regenerate", "--yes", "--offline"])
        .env("KIO_TEST_REPLICA_AFTER_HEAD_FAULT", "reindex_before_marker")
        .arg("--json")
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();
    let fault: Value = serde_json::from_slice(&stderr).unwrap();
    assert_eq!(fault["error_code"], "KIO-E-REPLICA-AFTER-HEAD-FAULT-001");

    let head_after = fs::read_to_string(dir.path().join(".kio/HEAD")).unwrap();
    assert_ne!(head_after, head_before, "regeneration must advance HEAD");
    let header = Aggregator::open(&replica_path)
        .unwrap()
        .scope_header(&scope_id)
        .unwrap()
        .unwrap();
    assert_eq!(header.index_status, AggIndexStatus::Ready);
    assert_eq!(
        header.current_snapshot_commit.as_deref(),
        Some(head_before.trim())
    );

    let search_error = json_failure(&dir, &["search", "token", "--mode", "text"], 3);
    assert_eq!(search_error["error_code"], "KIO-E-INDEX-REBUILDING-001");
}

/// A checkpoint-only manual snapshot immediately after `kio index` has the
/// same path/raw/type tree as the indexed auto snapshot.  The manual form
/// omits normalize refs, so it receives a distinct CAS tree/commit, but the
/// writer can prove that its existing Ready tree projection remains exact.
/// It must materialize that target identity and publish a full replica before
/// direct search returns; a source-index-free search then proves this is not a
/// reader-side fallback.
#[test]
fn ct3_replica_005_same_content_snapshot_republishes_ready_projection() {
    let dir = indexed_scope();
    let scope_id = read_scope_id(dir.path());
    let replica_path = dir.path().join(".test-cache/kio/aggregator.sqlite");
    let head_before = fs::read_to_string(dir.path().join(".kio/HEAD")).unwrap();
    let header_before = Aggregator::open(&replica_path)
        .unwrap()
        .scope_header(&scope_id)
        .unwrap()
        .unwrap();
    assert_eq!(header_before.index_status, AggIndexStatus::Ready);
    let prior_tree_rows: i64 = Connection::open(dir.path().join(".kio/index/sqlite.db"))
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM tree_entries WHERE commit_hash = ?1",
            rusqlite::params![head_before.trim()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        prior_tree_rows > 0,
        "indexed predecessor must have tree rows"
    );

    let snapshot = json_success(
        &dir,
        &["snapshot", "create", "--message", "checkpoint only"],
    );
    assert_eq!(snapshot["status"], "created", "{snapshot}");
    let head_after = fs::read_to_string(dir.path().join(".kio/HEAD")).unwrap();
    assert_ne!(
        head_after, head_before,
        "manual checkpoint must create its own commit"
    );

    let header_after = Aggregator::open(&replica_path)
        .unwrap()
        .scope_header(&scope_id)
        .unwrap()
        .unwrap();
    assert_eq!(header_after.index_status, AggIndexStatus::Ready);
    assert_eq!(
        header_after.current_snapshot_commit.as_deref(),
        Some(head_after.trim()),
        "the complete replica must name the checkpoint commit"
    );
    assert_eq!(
        header_after.index_generation, header_before.index_generation,
        "tree identity materialization does not alter indexed chunks or their generation"
    );
    let target_tree_rows: i64 = Connection::open(dir.path().join(".kio/index/sqlite.db"))
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM tree_entries WHERE commit_hash = ?1",
            rusqlite::params![head_after.trim()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(target_tree_rows, prior_tree_rows);

    let source_index = dir.path().join(".kio/index/sqlite.db");
    let hidden = HiddenSourceIndex::hide(&source_index);
    let searched = json_success(&dir, &["search", "認証仕様", "--mode", "text"]);
    assert!(
        !searched["results"].as_array().unwrap().is_empty(),
        "the republished checkpoint must search from aggregator.sqlite only: {searched}"
    );
    hidden.restore();
}

/// A fresh all-scopes search is allowed to exclude a temporarily rebuilding
/// scope, but that request-local exclusion must not erase the scope's durable
/// replica projection.  Once the header returns to Ready, a text search must
/// use the preserved projection without requiring another source-index write.
#[test]
fn ct3_replica_003_temporarily_excluded_scope_keeps_its_replica_projection() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let a = parent.path().join("a");
    let b = parent.path().join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(a.join("a.md"), "# A\n\nretainprojectionalpha\n").unwrap();
    fs::write(b.join("b.md"), "# B\n\nretainprojectionbeta\n").unwrap();
    for scope in [&a, &b] {
        json_success_path(scope, &data_home, &["init"]);
        json_success_path(scope, &data_home, &["index"]);
    }

    let b_scope_id = read_scope_id(&b);
    let replica_path = data_home.join("cache/kio/aggregator.sqlite");
    let ready_header = {
        let mut replica = Aggregator::open(&replica_path).unwrap();
        let header = replica
            .scope_header(&b_scope_id)
            .unwrap()
            .expect("indexing must publish B's replica header");
        assert_eq!(header.index_status, AggIndexStatus::Ready);
        let mut rebuilding = header.clone();
        rebuilding.index_status = AggIndexStatus::Rebuilding;
        assert!(
            replica
                .update_scope_header(&b_scope_id, &rebuilding, 1)
                .unwrap(),
            "the test must mark B temporarily unavailable without dropping its rows"
        );
        header
    };

    let partial_stdout = hermetic_kio_command()
        .current_dir(&a)
        .env("XDG_CONFIG_HOME", data_home.join("config"))
        .env("XDG_DATA_HOME", data_home.join("data"))
        .env("XDG_CACHE_HOME", data_home.join("cache"))
        .args([
            "search",
            "retainprojectionalpha",
            "--mode",
            "text",
            "--all-scopes",
            "--json",
        ])
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let partial: Value = serde_json::from_slice(&partial_stdout).unwrap();
    assert!(
        partial["excluded_scopes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|scope| {
                scope["scope_id"] == b_scope_id && scope["reason"] == "index_rebuilding"
            }),
        "B must be excluded only for this rebuilding request: {partial:#?}"
    );

    {
        let mut replica = Aggregator::open(&replica_path).unwrap();
        assert!(
            replica
                .update_scope_header(&b_scope_id, &ready_header, 2)
                .unwrap(),
            "a request-local exclusion must retain B's complete replica projection"
        );
    }

    let after = json_success_path(
        &a,
        &data_home,
        &[
            "search",
            "retainprojectionbeta",
            "--mode",
            "text",
            "--all-scopes",
        ],
    );
    assert!(
        after["results"].as_array().unwrap().iter().any(|hit| {
            hit["evidence_pointer"]["scope_id"] == b_scope_id
                && hit["snippet"]
                    .as_str()
                    .is_some_and(|text| text.contains("retainprojectionbeta"))
        }),
        "the later text search must use B's retained replica projection: {after:#?}"
    );
}

/// A stale Ready header is not authority to search a scope whose on-disk
/// identity can no longer be opened.  This is the fail-closed companion to a
/// writer failing to obtain `replica_scope_stamp`: the writer cannot address
/// the old header without a scope id, and direct search must constrain the
/// replica query to successfully prepared scope stores instead.
#[test]
fn ct3_replica_008_unresolvable_scope_identity_cannot_serve_a_ready_projection() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let a = parent.path().join("a");
    let b = parent.path().join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(
        a.join("a.md"),
        "# A\n\nunresolvableidentityneedle stale-ready-secret\n",
    )
    .unwrap();
    fs::write(
        b.join("b.md"),
        "# B\n\nunresolvableidentityneedle healthy-public-answer\n",
    )
    .unwrap();
    for scope in [&a, &b] {
        json_success_path(scope, &data_home, &["init"]);
        json_success_path(scope, &data_home, &["index", "--offline"]);
    }

    let a_scope_id = read_scope_id(&a);
    let replica_path = data_home.join("cache/kio/aggregator.sqlite");
    assert_eq!(
        Aggregator::open(&replica_path)
            .unwrap()
            .scope_header(&a_scope_id)
            .unwrap()
            .expect("A must begin with a Ready replica header")
            .index_status,
        AggIndexStatus::Ready
    );

    // Keep the stale header and corpus intentionally, but make the scope's
    // source identity unavailable. The registry still names A, so the direct
    // route must explicitly exclude it rather than accidentally querying its
    // otherwise Ready aggregator rows.
    let scope_json = a.join(".kio/scope.json");
    let hidden_scope_json = a.join(".kio/scope.json.hidden-for-replica-test");
    fs::rename(&scope_json, &hidden_scope_json).unwrap();

    let output = hermetic_kio_command()
        .current_dir(&b)
        .env("XDG_CONFIG_HOME", data_home.join("config"))
        .env("XDG_DATA_HOME", data_home.join("data"))
        .env("XDG_CACHE_HOME", data_home.join("cache"))
        .args([
            "search",
            "unresolvableidentityneedle",
            "--mode",
            "text",
            "--all-scopes",
            "--json",
        ])
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let response: Value = serde_json::from_slice(&output).unwrap();
    assert!(
        response["excluded_scopes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|scope| { scope["scope_id"] == a_scope_id && scope["reason"] == "unreachable" }),
        "the scope with no resolvable identity must be excluded: {response:#?}"
    );
    assert!(
        response["results"].as_array().unwrap().iter().all(|hit| {
            hit["evidence_pointer"]["scope_id"] != a_scope_id
                && !hit["snippet"]
                    .as_str()
                    .is_some_and(|snippet| snippet.contains("stale-ready-secret"))
        }),
        "a Ready replica header must not make an unresolvable scope searchable: {response:#?}"
    );
    assert!(
        serde_json::to_string(&response)
            .unwrap()
            .contains("healthy-public-answer"),
        "the reachable sibling should still be returned: {response:#?}"
    );

    fs::rename(&hidden_scope_json, &scope_json).unwrap();
}

/// The all-scopes fallback used when the registry itself is unavailable can
/// search only the current scope. It is therefore not authority to reconcile
/// the device-wide replica and must leave siblings alone.
#[test]
fn ct3_replica_004_registry_fallback_does_not_prune_unenumerated_siblings() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let a = parent.path().join("a");
    let b = parent.path().join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(a.join("a.md"), "# A\n\nregistryfallbackalpha\n").unwrap();
    fs::write(b.join("b.md"), "# B\n\nregistryfallbackbeta\n").unwrap();
    for scope in [&a, &b] {
        json_success_path(scope, &data_home, &["init"]);
        json_success_path(scope, &data_home, &["index"]);
    }

    let b_scope_id = read_scope_id(&b);
    let replica_path = data_home.join("cache/kio/aggregator.sqlite");
    assert!(
        Aggregator::open(&replica_path)
            .unwrap()
            .scope_ids()
            .unwrap()
            .contains(&b_scope_id),
        "indexing must prepopulate B's device-level projection"
    );

    let registry = UnavailableRegistry::block(&data_home.join("data/kio/scope-registry.sqlite"));
    let degraded = json_success_path(
        &a,
        &data_home,
        &[
            "search",
            "registryfallbackalpha",
            "--mode",
            "text",
            "--all-scopes",
        ],
    );
    assert_eq!(
        degraded["searched_scopes"].as_array().unwrap().len(),
        1,
        "registry fallback may search only the current scope: {degraded:#?}"
    );
    registry.restore();

    assert!(
        Aggregator::open(&replica_path)
            .unwrap()
            .scope_ids()
            .unwrap()
            .contains(&b_scope_id),
        "a non-authoritative fallback must not prune B's replica rows"
    );
    let after = json_success_path(
        &a,
        &data_home,
        &[
            "search",
            "registryfallbackbeta",
            "--mode",
            "text",
            "--all-scopes",
        ],
    );
    assert!(
        after["results"].as_array().unwrap().iter().any(|hit| {
            hit["evidence_pointer"]["scope_id"] == b_scope_id
                && hit["snippet"]
                    .as_str()
                    .is_some_and(|text| text.contains("registryfallbackbeta"))
        }),
        "the restored registry must still reach B's preserved projection: {after:#?}"
    );
}

// CT3-HYBRID-003: an auto search keeps working when the source SQLite index is
// unavailable after indexing. Candidate selection and lane availability are
// served by the device replica; source SQLite is a writer-side projection only.
#[test]
fn ct3_hybrid_003_replica_serves_auto_search_without_source_sqlite() {
    let dir = indexed_scope();
    let before = json_success(&dir, &["search", "認証仕様"]);
    assert!(!before["results"].as_array().unwrap().is_empty());
    let generation = replica_collection_generation(&before).to_owned();

    // This removes every source text/vector table. A candidate path that still
    // opens `.kio/index/sqlite.db` fails here instead of returning the replica
    // row below.
    fs::remove_file(dir.path().join(".kio/index/sqlite.db")).unwrap();
    let after = json_success(&dir, &["search", "認証仕様"]);
    assert_eq!(
        replica_collection_generation(&after),
        generation,
        "source-index removal must not change the replica corpus"
    );
    assert!(
        !after["results"].as_array().unwrap().is_empty(),
        "the auto query must be selected by the device replica: {after:#?}"
    );
}

#[test]
fn ct3_obs_002_metrics_use_search_namespace_code_and_component() {
    let dir = indexed_scope();
    json_success(&dir, &["search", "認証仕様"]);
    let metrics = fs::read_to_string(dir.path().join(".test-data/kio/logs/metrics.jsonl")).unwrap();
    let last: Value = serde_json::from_str(metrics.lines().last().unwrap()).unwrap();
    assert_eq!(last["code"], "KIO-M-SEARCH-001");
    assert_eq!(last["component"], "search");
    assert_eq!(last["metric"], "search.latency_ms");
    assert!(last["value"].as_f64().is_some());
    assert!(last["context"]["result_count"].as_u64().is_some());
}

// K8 / CT3-FTS-004: rebuilding the source FTS index is a writer operation. A
// previously published replica continues serving search during that repair.
#[test]
fn ct3_fts_004_replica_serves_search_while_source_fts_is_rebuilt() {
    let dir = indexed_scope();
    fs::remove_file(dir.path().join(".kio/index/sqlite.db")).unwrap();
    let during_repair = json_success(&dir, &["search", "認証仕様"]);
    assert!(
        !during_repair["results"].as_array().unwrap().is_empty(),
        "a missing source FTS index must not take the replica candidate route down"
    );
    json_success(&dir, &["repair", "rebuild-db"]);
    let after = json_success(&dir, &["search", "認証仕様"]);
    assert!(!after["results"].as_array().unwrap().is_empty());
}

/// R23-21 (05 §1.8): each text and vector lane is independently capped at
/// `candidate_depth` before RRF. When their top-Ns are disjoint, the fused
/// collection pool is their union and therefore can carry up to 2x the depth.
/// Proven with candidate_depth=1 and two documents constructed so each backend's
/// sole top-1 pick differs: one document's body is byte-identical to the
/// query (the SHA256-seeded mock embedding has no partial-similarity
/// structure, so identical text is the only way to force cosine=1.0,
/// unbeatable vector rank 1); the other repeats the query term densely
/// (unbeatable BM25 text rank 1) while textually differing from the query
/// (so its mock vector is uncorrelated — effectively random relative to the
/// query's).
#[test]
fn r23_21_hybrid_collection_pool_is_the_union_of_per_lane_candidate_depth() {
    let dir = tempfile::tempdir().unwrap();
    // Contextual-embedding addendum (07 §5.3, 2026-07-24): the mock vector is
    // `deterministic_embedding_vector(item.text)`, and the send path now prepends
    // the humanized filename to a chunk's body. This doc must be the VECTOR top-1,
    // so (a) its filename stem humanizes to nothing (`_` → no alphanumerics →
    // `chunk_embedding_context` is `None`), leaving it embedded bare, and (b) it
    // carries NO heading, so its lone chunk body is the query phrase itself — the
    // embedded seed closest to the plainly-embedded query. `text_favored` below
    // repeats the whole query and so decisively owns the (trigram) TEXT lane; the
    // two lanes' deterministic top-1 picks are therefore disjoint.
    fs::write(dir.path().join("_.md"), "depthprobe divergence fixture\n").unwrap();
    // Contextual-embedding addendum (07 §5.3, 2026-07-24): repeat the WHOLE query
    // (not just one of its terms) so this doc dominates the TRIGRAM text lane for
    // every query trigram. Otherwise `_.md` — which uniquely carries the
    // `divergence`/`fixture` trigrams (once) — would out-rank a one-term-dense doc
    // in the text lane too, and the two backends would no longer be disjoint. Its
    // mock vector (a hash of this repeated text, not the bare query) stays far
    // from the query, so it never challenges `_.md`'s cosine-1.0 vector rank 1.
    let filler = "depthprobe divergence fixture ".repeat(30);
    fs::write(
        dir.path().join("text_favored.md"),
        format!("# T\n\n{filler}\n"),
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_embed(&dir, "mock");
    json_success_embed(&dir, "mock", &["index"]);

    // The folder config applies only to this single explicit scope.  Set it
    // before the lane controls so all three searches exercise depth=1.
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[search.rrf]\ncandidate_depth = 1\n",
    )
    .unwrap();

    // Empirically confirm the fixture gives the two backends disjoint top-1
    // picks (deterministic given the SHA256-seeded mock embedding above, not
    // a coin flip).
    let text_only = json_success_embed(
        &dir,
        "mock",
        &["search", "depthprobe divergence fixture", "--mode", "text"],
    );
    let vector_only = json_success_embed(
        &dir,
        "mock",
        &[
            "search",
            "depthprobe divergence fixture",
            "--mode",
            "vector",
        ],
    );
    let text_top1 = text_only["results"][0]["chunk_hash"].as_str().unwrap();
    let vector_top1 = vector_only["results"][0]["chunk_hash"].as_str().unwrap();
    assert_ne!(
        text_top1, vector_top1,
        "fixture must give the two backends disjoint top-1 picks: text={text_only} \
         vector={vector_only}"
    );

    // With independently bounded lanes, RRF receives both disjoint top-1s.
    let hybrid = json_success_embed(
        &dir,
        "mock",
        &[
            "search",
            "depthprobe divergence fixture",
            "--mode",
            "hybrid",
            "--limit",
            "20",
            "--scope",
            ".",
        ],
    );
    assert_eq!(
        hybrid["results"].as_array().unwrap().len(),
        2,
        "candidate_depth=1 applies to each lane; the fused pool retains both disjoint top-1s: \
         {hybrid}"
    );
    let hybrid_ids = chunk_hash_set(&hybrid);
    assert!(hybrid_ids.contains(text_top1), "{hybrid}");
    assert!(hybrid_ids.contains(vector_top1), "{hybrid}");
}

fn line_count(path: impl AsRef<Path>) -> usize {
    fs::read_to_string(path).unwrap().lines().count()
}

// ---------------------------------------------------------------------------
// K6 — Evidence Pointer resolver (08 §3 / §4). Helpers below run `kio` with a
// caller-chosen cwd + XDG home so scope resolution stages can be isolated, and
// place fixtures (tombstones, shallow commits) that Step 3 has no generator for.
// ---------------------------------------------------------------------------

use kio_index::registry::{RegistryDb, RegistryEntry};

/// Runs `kio <args> --json` and returns `(exit_code, parsed_json)`, reading
/// stdout on success and stderr on failure (mirrors `json_success`/`json_failure`).
fn run_json(cwd: &Path, data_home: &Path, args: &[&str]) -> (i32, Value) {
    let output = hermetic_kio_command()
        .current_dir(cwd)
        .env("XDG_CONFIG_HOME", data_home.join("config"))
        .env("XDG_DATA_HOME", data_home.join("data"))
        .env("XDG_CACHE_HOME", data_home.join("cache"))
        .args(args)
        .arg("--json")
        .output()
        .unwrap();
    let code = output.status.code().unwrap();
    let stream = if output.status.success() {
        &output.stdout
    } else {
        &output.stderr
    };
    (code, serde_json::from_slice(stream).unwrap())
}

/// CAS object path: `<kio>/objects/<kind>/ab/cd/<digest>` (kio_core::cas::fanout).
fn object_path(kio_dir: &Path, kind: &str, hash: &str) -> std::path::PathBuf {
    let digest = hash.strip_prefix("sha256:").unwrap();
    kio_dir
        .join("objects")
        .join(kind)
        .join(&digest[0..2])
        .join(&digest[2..4])
        .join(digest)
}

/// Tombstone path: `<kio>/tombstones/ab/cd/<raw-digest>` (05 §3.5).
fn tombstone_path(kio_dir: &Path, raw_hash: &str) -> std::path::PathBuf {
    let digest = raw_hash.strip_prefix("sha256:").unwrap();
    kio_dir
        .join("tombstones")
        .join(&digest[0..2])
        .join(&digest[2..4])
        .join(digest)
}

fn registry_path(data_home: &Path) -> std::path::PathBuf {
    data_home.join("data/kio/scope-registry.sqlite")
}

#[test]
fn ct3_evidence_003_scope_resolves_via_path_then_registry() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let scope = parent.path().join("research");
    let elsewhere = parent.path().join("elsewhere");
    fs::create_dir_all(&scope).unwrap();
    fs::create_dir_all(&elsewhere).unwrap();
    fs::write(
        scope.join("auth.md"),
        "# 認証仕様\n\n## API Token\nトークン TTL は 3600 秒です。\n",
    )
    .unwrap();
    json_success_path(&scope, &data_home, &["init"]);
    json_success_path(&scope, &data_home, &["index"]);
    let search = json_success_path(&scope, &data_home, &["search", "トークン TTL 3600"]);
    let pointer = first_result(&search)["evidence_pointer"].clone();
    let scope_id = pointer["scope_id"].as_str().unwrap().to_owned();

    // (a) valid scope_path + matching scope_id resolves. Run from a non-scope
    //     cwd with an empty registry so *only* stage 1a (scope_path) can succeed.
    let (code, viewed) = run_json(&elsewhere, &data_home, &["view", &pointer.to_string()]);
    assert_eq!(code, 0, "scope_path stage failed: {viewed}");
    assert!(view_slice(&viewed).contains("トークン TTL"));

    // (b) broken scope_path -> registry lookup by scope_id resolves. Register the
    //     scope directly (index-time registry wiring is Agent A's; §3.1 permits
    //     the registry as the authoritative kio_path source).
    let registry = RegistryDb::open(registry_path(&data_home)).unwrap();
    registry
        .upsert(&RegistryEntry {
            scope_id: scope_id.clone(),
            kio_path: scope.join(".kio").display().to_string(),
            root_path: scope.display().to_string(),
            participates_in_global_search: true,
            indexed: true,
            last_seen_at: "2026-07-04T00:00:00Z".to_owned(),
        })
        .unwrap();
    let mut broken = pointer.clone();
    broken["scope_path"] = serde_json::json!(parent.path().join("gone/.kio").display().to_string());
    let (code, viewed) = run_json(&elsewhere, &data_home, &["view", &broken.to_string()]);
    assert_eq!(code, 0, "registry stage failed: {viewed}");
    assert!(view_slice(&viewed).contains("トークン TTL"));

    // (c) broken scope_path + unknown scope_id + registry miss -> scope_unreachable.
    // QB1: scope_unreachable is retryable, exit 3 (not the dead-pointer exit 4).
    let mut orphan = pointer.clone();
    orphan["scope_path"] = serde_json::json!(parent.path().join("gone/.kio").display().to_string());
    orphan["scope_id"] = serde_json::json!("scope_does_not_exist");
    let (code, err) = run_json(&elsewhere, &data_home, &["view", &orphan.to_string()]);
    assert_eq!(code, 3);
    assert_eq!(err["error_code"], "KIO-E-EVIDENCE-SCOPE-UNREACHABLE-001");
}

#[test]
fn ct3_evidence_004_resolves_through_pointer_commit_tree() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let scope = parent.path().join("research");
    fs::create_dir_all(&scope).unwrap();
    fs::write(
        scope.join("auth.md"),
        "# 認証仕様\n\n## API Token\nトークン TTL は 3600 秒です。\n",
    )
    .unwrap();
    json_success_path(&scope, &data_home, &["init"]);
    json_success_path(&scope, &data_home, &["index"]);
    let search1 = json_success_path(&scope, &data_home, &["search", "トークン TTL 3600"]);
    let p_auth = first_result(&search1)["evidence_pointer"].clone();
    let commit1 = p_auth["commit"].as_str().unwrap().to_owned();

    // Advance HEAD: add a second file, re-index (new tree -> commit2).
    fs::write(
        scope.join("ranking.md"),
        "# 検索ランキング\n\n## RRF 融合\nRRF の定数 k=60 を使います。\n",
    )
    .unwrap();
    json_success_path(&scope, &data_home, &["index"]);
    let search2 = json_success_path(&scope, &data_home, &["search", "RRF 定数 k 60"]);
    let p_rank = pointer_for_path(&search2, "ranking.md").clone();
    let commit2 = p_rank["commit"].as_str().unwrap().to_owned();
    assert_ne!(commit1, commit2, "HEAD did not advance");

    // The auth pointer (commit1) still resolves after HEAD advanced: resolution
    // walks pointer.commit's tree, not HEAD's.
    let (code, viewed) = run_json(&scope, &data_home, &["view", &p_auth.to_string()]);
    assert_eq!(code, 0, "old-commit pointer stopped resolving: {viewed}");
    assert!(view_slice(&viewed).contains("トークン TTL"));

    // Discriminator: ranking.md's raw_hash exists in the working tree, in CAS,
    // and in commit2's tree — but NOT in commit1's tree. Pointing p_rank at
    // commit1 must fail (a working-tree scan would wrongly succeed).
    let mut mismatched = p_rank.clone();
    mismatched["commit"] = serde_json::json!(commit1);
    let (code, err) = run_json(&scope, &data_home, &["view", &mismatched.to_string()]);
    assert_eq!(
        code, 4,
        "raw_hash absent from pointer.commit tree should fail"
    );
    assert_eq!(err["error_code"], "KIO-E-PURGE-NOT-FOUND-001");
}

// 08 §3.2 step 6/7 failure contract: scope, commit, and raw_hash all resolve but
// the pointer's chunk_hash has no materialized chunk row in this scope (a
// different tool_profile_hash produced it) → retarget required, exit 8
// (06 §7), for BOTH view and open; the canonical pointer-resolution contract
// is 08 §3.2.
#[test]
fn ct3_evidence_004_missing_chunk_row_requires_retarget() {
    let dir = indexed_scope();
    let search = json_success(&dir, &["search", "トークン TTL 3600"]);
    let mut pointer = first_result(&search)["evidence_pointer"].clone();
    // Valid sha256 shape, absent from the scope's chunk rows.
    pointer["chunk_hash"] = serde_json::json!(
        "sha256:feedfacefeedfacefeedfacefeedfacefeedfacefeedfacefeedfacefeedface"
    );
    let ptr = pointer.to_string();
    let err_view = json_failure(&dir, &["view", &ptr], 8);
    assert_eq!(
        err_view["error_code"],
        "KIO-E-EVIDENCE-RETARGET-REQUIRED-001"
    );
    assert_eq!(err_view["context"]["chunk_hash"], pointer["chunk_hash"]);
    assert_eq!(
        err_view["context"]["tool_profile_hash"],
        pointer["tool_profile_hash"]
    );
    let err_open = json_failure(&dir, &["open", &ptr], 8);
    assert_eq!(
        err_open["error_code"],
        "KIO-E-EVIDENCE-RETARGET-REQUIRED-001"
    );
}

#[test]
fn ct3_evidence_005_shallow_commit_rejects_pointer_and_preserves_tree_controls() {
    let dir = indexed_scope();
    let search = json_success(&dir, &["search", "トークン TTL 3600"]);
    let pointer = first_result(&search)["evidence_pointer"].clone();
    let other_raw =
        first_result(&json_success(&dir, &["search", "RRF 定数 k 60"]))["evidence_pointer"].clone();
    // A retained tree remains the ordinary positive control.
    assert!(
        view_slice(&json_success(&dir, &["view", &pointer.to_string()])).contains("トークン TTL")
    );
    json_success(&dir, &["reindex", "--regenerate", "--yes"]);
    let later_generation =
        first_result(&json_success(&dir, &["search", "トークン TTL 3600"]))["evidence_pointer"]
            .clone();
    let commit = pointer["commit"].as_str().unwrap();
    let kio_dir = dir.path().join(".kio");

    // Complete a valid markerless shallow state: the indexed Auto commit must
    // be non-tip and have a strict canonical receipt before its tree is gone.
    let commit_bytes = fs::read(object_path(&kio_dir, "commits", commit)).unwrap();
    let commit_obj: Value = serde_json::from_slice(&commit_bytes).unwrap();
    let tree = commit_obj["tree"].as_str().unwrap();
    fs::write(dir.path().join("advance.md"), "# Advance\n\nfixture head\n").unwrap();
    json_success(&dir, &["index", "--yes"]);
    let receipt_path = kio_dir
        .join("gc/shallowed")
        .join(commit.strip_prefix("sha256:").unwrap());
    fs::create_dir_all(receipt_path.parent().unwrap()).unwrap();
    let receipt = ShallowReceipt::new(
        commit.to_owned(),
        tree.to_owned(),
        "2026-08-14T00:00:00Z".into(),
    )
    .unwrap();
    fs::write(receipt_path, receipt.canonical_bytes().unwrap()).unwrap();
    fs::remove_file(object_path(&kio_dir, "trees", tree)).unwrap();

    let mut later_generation_forgery = pointer.clone();
    later_generation_forgery["chunk_hash"] = later_generation["chunk_hash"].clone();
    let mut other_raw_forgery = pointer.clone();
    other_raw_forgery["raw_hash"] = other_raw["raw_hash"].clone();
    other_raw_forgery["chunk_hash"] = other_raw["chunk_hash"].clone();
    for forged in [pointer, later_generation_forgery, other_raw_forgery] {
        for command in ["view", "open"] {
            let error = json_failure(&dir, &[command, &forged.to_string()], 1);
            assert_eq!(
                error["error_code"], "KIO-E-COMMIT-SHALLOW-001",
                "{command}: {error}"
            );
            assert!(
                error["message"]
                    .as_str()
                    .unwrap()
                    .contains("restore the tree object from backup")
            );
        }
    }
}

#[test]
fn ct3_evidence_006_three_valued_resolution_failures() {
    // (a) tombstoned -> status="tombstoned" response, open exit 4.
    let dir = indexed_scope();
    let search = json_success(&dir, &["search", "トークン TTL 3600"]);
    let pointer = first_result(&search)["evidence_pointer"].clone();
    let raw_hash = pointer["raw_hash"].as_str().unwrap().to_owned();
    let commit = pointer["commit"].as_str().unwrap().to_owned();
    let kio_dir = dir.path().join(".kio");
    let tomb = tombstone_path(&kio_dir, &raw_hash);
    fs::create_dir_all(tomb.parent().unwrap()).unwrap();
    fs::write(
        &tomb,
        serde_json::json!({
            "raw_hash": raw_hash,
            "events": [{
                "kind": "purged",
                "at": "2026-04-25T12:00:00Z",
                "in_commit": commit,
                "actor": "operator",
                "reason": "legal",
                "epoch": 1,
                "lifecycle_epoch": 1,
            }],
        })
        .to_string(),
    )
    .unwrap();
    let ptr = pointer.to_string();
    let err = json_failure(&dir, &["open", &ptr], 4);
    assert_eq!(err["error_code"], "KIO-E-PURGE-TOMBSTONED-001");
    assert_eq!(err["context"]["status"], "tombstoned");
    assert_eq!(err["context"]["purged_reason"], "legal");
    assert_eq!(err["context"]["raw_hash"], raw_hash);

    // (b) no marker at all (no tombstone, no erase receipt) but the raw object
    // is gone -> an UNMARKED absence, which Step4b's LC14(a) (08 §3.1 step 5
    // branch (iv)(a)) defines as a corruption suspicion, not a normal purge:
    // "marker が一切存在せず raw object も CAS に存在しない...KIO-E-STORE-CORRUPT-001
    // (marker なしの欠落は corruption の疑い)" — distinct from LC12's branch (ii)
    // (an ACTIVE erase receipt explains the absence), which still reports
    // KIO-E-PURGE-NOT-FOUND-001. The exit code (4) is unchanged.
    let dir_b = indexed_scope();
    let search_b = json_success(&dir_b, &["search", "トークン TTL 3600"]);
    let pointer_b = first_result(&search_b)["evidence_pointer"].clone();
    let raw_hash_b = pointer_b["raw_hash"].as_str().unwrap().to_owned();
    fs::remove_file(dir_b.path().join("auth.md")).unwrap();
    fs::remove_file(object_path(&dir_b.path().join(".kio"), "raw", &raw_hash_b)).unwrap();
    let ptr_b = pointer_b.to_string();
    let err_b = json_failure(&dir_b, &["open", &ptr_b], 4);
    assert_eq!(err_b["error_code"], "KIO-E-STORE-CORRUPT-001");

    // (c) scope unreachable. QB1: retryable, exit 3 (not the dead-pointer exit 4).
    let dir_c = indexed_scope();
    let bad = "kio://scope_missing/sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc/sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
    let err_c = json_failure(&dir_c, &["open", bad], 3);
    assert_eq!(err_c["error_code"], "KIO-E-EVIDENCE-SCOPE-UNREACHABLE-001");
}

#[test]
fn ct3_uri_002_open_rejects_object_raw_uri() {
    // Step4b P2-A PA01 (§A, U22): MVP object URIs accept only `image` type —
    // a `raw`-type object URI is now rejected at parse time (exit 2),
    // superseding this test's old "raw URIs resolve" expectation.
    let dir = indexed_scope();
    let search = json_success(&dir, &["search", "トークン TTL 3600"]);
    let pointer = &first_result(&search)["evidence_pointer"];
    let scope_id = pointer["scope_id"].as_str().unwrap();
    let raw_hash = pointer["raw_hash"].as_str().unwrap();
    let uri = format!("kio://{scope_id}/object/raw/{raw_hash}");
    let error = json_failure(&dir, &["open", &uri], 2);
    assert_eq!(error["error_code"], "KIO-E-CONFIG-USAGE-001");
}

#[test]
fn ct3_uri_002_open_rejects_invalid_object_uri() {
    let dir = indexed_scope();
    let search = json_success(&dir, &["search", "トークン TTL 3600"]);
    let pointer = &first_result(&search)["evidence_pointer"];
    let scope_id = pointer["scope_id"].as_str().unwrap().to_owned();
    let raw_hash = pointer["raw_hash"].as_str().unwrap().to_owned();
    // Unknown object type -> exit 2.
    json_failure(
        &dir,
        &["open", &format!("kio://{scope_id}/object/bogus/{raw_hash}")],
        2,
    );
    // Malformed hash -> exit 2.
    json_failure(
        &dir,
        &["open", &format!("kio://{scope_id}/object/raw/not-a-hash")],
        2,
    );
}

fn pointer_for_path<'a>(search: &'a Value, path: &str) -> &'a Value {
    search["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|result| result["evidence_pointer"]["path_at_commit"] == path)
        .map(|result| &result["evidence_pointer"])
        .unwrap_or_else(|| panic!("no search result for {path}"))
}

#[test]
// QA3 (step4b-contract-tests-p3a.md §A, 04 §5.2 L682-683): rate_limit lands
// `pending` + `next_retry_at`, never `failed`, and `attempts` is NOT consumed
// (max_attempts=∞, 04 §5.3) — this test used to find the tasks via
// `status=="failed"` with `attempts>0`.
fn ct3_embed_009_batch_retry_and_resume_execute_pending_embedding_tasks() {
    // 2026-07-04 実運用で発見した gap の回帰ガード: rate limit で Pending に
    // 積まれた embedding タスクは、`batch retry`/`resume` の executor が
    // Markdownize 専用だったため永遠に実行されなかった。retry → (seam 回復) →
    // 実行完了までを検証する。
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.md"), "# メモ\n回収率のテスト。\n").unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_embed(&dir, "mock");
    let base_now = "2026-07-03T00:00:00Z";
    json_success_embed_at(&dir, "rate_limit", base_now, &["index"]);

    let status = json_success_embed(&dir, "rate_limit", &["status"]);
    let failed: Vec<_> = status["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["type"] == "embedding" && t["status"] == "pending")
        .collect();
    assert!(
        !failed.is_empty(),
        "rate_limit seam should leave pending embedding tasks"
    );
    assert!(
        failed
            .iter()
            .all(|t| t["attempts"].as_u64().unwrap() == 0 && t["next_retry_at"].is_string()),
        "rate_limit failures must persist retry backoff without consuming attempts: {status}"
    );
    let retry_at = failed[0]["next_retry_at"].as_str().unwrap();

    let early = json_success_embed_at(&dir, "mock", base_now, &["batch", "retry"]);
    assert_eq!(
        early["tasks_executed"], 0,
        "retry before next_retry_at must honor backoff: {early}"
    );

    // seam が回復した状態で retry → executor が embedding を実行し done になる
    let retry = json_success_embed_at(&dir, "mock", retry_at, &["batch", "retry"]);
    assert!(
        retry["tasks_executed"].as_u64().unwrap() > 0,
        "retry must execute pending embedding tasks, got {retry}"
    );
    let status = json_success_embed(&dir, "mock", &["status"]);
    let tasks = status["tasks"].as_array().unwrap();
    let emb: Vec<_> = tasks.iter().filter(|t| t["type"] == "embedding").collect();
    assert!(!emb.is_empty());
    assert!(
        emb.iter().all(|t| t["status"] == "done"),
        "all embedding tasks must be done after retry"
    );
}

// R11-5: the enrichment pass now aggregates every embedding task-store update into
// ONE write-back at the end (was a full all()+replace_all per 32-chunk batch =
// O(N²)). The aggregation must not lose the sent group's `fallback_reason`: a
// rate_limit failure must still land that task with reason "rate_limit" and a
// scheduled retry, while later groups remain unsent (the paused-side reason
// "budget_exceeded" is covered by ct3_l2).
// QA3 (step4b-contract-tests-p3a.md §A, 04 §5.2): the landing status is
// `pending`, not `failed` — this test used to assert `status=="failed"`.
#[test]
fn r11_5_aggregated_writeback_preserves_embedding_fallback_reason() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("a.md"),
        "# メモ\n\n## 本文\n集約書き戻しの回帰テスト本文です。\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_embed(&dir, "mock");
    json_success_embed_at(&dir, "rate_limit", "2026-07-03T00:00:00Z", &["index"]);
    let status = json_success_embed(&dir, "rate_limit", &["status"]);
    let emb = tasks_of_type(&status, "embedding");
    assert!(!emb.is_empty(), "must enqueue embedding tasks: {status}");
    let rate_limited = emb
        .iter()
        .filter(|task| {
            task["status"] == "pending"
                && task["fallback_reason"] == "rate_limit"
                && task["next_retry_at"].is_string()
        })
        .count();
    let not_sent = emb
        .iter()
        .filter(|task| task["fallback_reason"] == "ready_for_online_adapter")
        .count();
    assert_eq!(
        rate_limited, 1,
        "only the first provider group may be sent and retain its retry reason: {status}"
    );
    assert!(
        not_sent > 0,
        "later groups must remain unsent after the first rejection: {status}"
    );
    assert_eq!(
        reservation_row_count(&dir, "embedding"),
        1,
        "the unsent groups must not create speculative ledger rows"
    );
}

// R11-8: a retryable, not-yet-recovered enrichment task (rate_limit,
// recoverable by `batch retry`) must count toward
// `index_status.pending_enrichment_tasks` — otherwise the scope reports
// enriched_ratio<1.0 with pending=0 and budget_paused=false, a dead end an
// Agent can't act on (the dual of R9-4's "Partial counts as incomplete").
// QA3 (step4b-contract-tests-p3a.md §A, 04 §5.2): rate_limit now lands
// `pending` (not `failed`), so it is counted by `compute_index_status`'s
// plain `TaskStatus::Pending` arm rather than by the `TaskStatus::Failed if
// task_retry_allowed(..)` arm this test originally exercised (that arm now
// covers network_error/quota_exceeded instead) — the counting BEHAVIOR this
// test pins is unchanged either way.
#[test]
fn r11_8_retryable_failed_enrichment_counts_as_pending_in_index_status() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("a.md"),
        "# メモ\n\n## 本文\n再試行可能な失敗の可視化テスト本文です。\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_embed(&dir, "mock");
    json_success_embed_at(&dir, "rate_limit", "2026-07-03T00:00:00Z", &["index"]);
    let search = json_success_embed(&dir, "rate_limit", &["search", "本文 再試行"]);
    let index_status = &search["index_status"];
    assert!(
        index_status["pending_enrichment_tasks"].as_u64().unwrap() > 0,
        "retryable failed embedding must surface as pending: {index_status}"
    );
    assert!(index_status["enriched_ratio"].as_f64().unwrap() < 1.0);
    // The dead end this closes: ratio<1.0 must no longer coincide with pending=0.
    assert_eq!(index_status["budget_paused"], false);
}

// QA3 (step4b-contract-tests-p3a.md §A, 04 §5.2): the rate-limited task is
// read from the PENDING task (never `failed`) — this test used to find it
// via `status=="failed"`.
#[test]
fn ct3_embed_010_retry_executes_after_snapshot_advances_head() {
    // 2026-07-04 実運用バグ #2 の回帰ガード: `kio snapshot` は tree_entries を
    // 射影せず HEAD だけ進めるため、enrichment の live-chunk JOIN が 0 件になり
    // retry/resume が何も実行しなかった。enrichment は writer 側で source
    // relation を materialize し、search に補完を委ねてはならない。
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.md"), "# メモ\n射影テスト。\n").unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_embed(&dir, "mock");
    let base_now = "2026-07-03T00:00:00Z";
    json_success_embed_at(&dir, "rate_limit", base_now, &["index"]);
    let status = json_success_embed(&dir, "rate_limit", &["status"]);
    let retry_at = tasks_of_type(&status, "embedding")
        .into_iter()
        .find(|t| t["status"] == "pending")
        .and_then(|t| t["next_retry_at"].as_str())
        .unwrap()
        .to_owned();
    // snapshot で HEAD を射影なしに前進させる (replay の各 step と同じ形)
    json_success_embed_at(
        &dir,
        "rate_limit",
        base_now,
        &["snapshot", "create", "-m", "advance"],
    );

    let retry = json_success_embed_at(&dir, "mock", &retry_at, &["batch", "retry"]);
    assert!(
        retry["tasks_executed"].as_u64().unwrap() > 0,
        "retry must project tree_entries lazily and execute, got {retry}"
    );
    let status = json_success_embed(&dir, "mock", &["status"]);
    let emb: Vec<_> = status["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["type"] == "embedding")
        .collect();
    assert!(!emb.is_empty());
    assert!(emb.iter().all(|t| t["status"] == "done"));
}

// ---------------------------------------------------------------------------
// Step 4 checkpoint fixes L1-L7 — real-machine acceptance scenarios (a)-(d).
// ---------------------------------------------------------------------------

/// Run `kio <args> --json` with BOTH online mock seams (markdownize + embedding)
/// so `batch resume`/index can execute both adapters deterministically offline.
fn json_both_mock(dir: &TempDir, args: &[&str]) -> Value {
    json_both_mock_code(dir, args, 0)
}

fn approve_both_mock(dir: &TempDir) {
    json_both_mock(dir, &["adapter", "approve", "--all", "--yes"]);
}

/// `json_both_mock` asserting a specific exit code and reading STDOUT. R11-2: an
/// `index`/`batch` run whose inline enrichment budget-pauses prints its full result
/// JSON to stdout with a non-zero exit (6), the search "result + nonzero" shape.
fn json_both_mock_code(dir: &TempDir, args: &[&str], code: i32) -> Value {
    let output = kio(dir, args)
        .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
        .env(TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV, "mock")
        .arg("--json")
        .assert()
        .code(code)
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

fn tasks_of_type<'a>(status: &'a Value, ty: &str) -> Vec<&'a Value> {
    status["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|task| task["type"] == ty)
        .collect()
}

fn first_online_output_ref(status: &Value) -> String {
    status["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|task| {
            let output_ref = task["output_ref"].as_str()?;
            output_ref
                .starts_with("online:")
                .then(|| output_ref.to_owned())
        })
        .expect("online task output_ref")
}

fn is_budget_paused(status: &Value, ty: &str) -> bool {
    tasks_of_type(status, ty)
        .iter()
        .any(|task| task["status"] == "paused" && task["fallback_reason"] == "budget_exceeded")
}

// Scenario (a) — L1: a chunking-config change + `reindex --force` re-embeds the
// new generation (docs/06 "reindex = 再 normalize / 再 embedding"). The smaller
// max_chars re-splits sections into chunks with fresh text (new text_hash), so
// no content vector can be reused — the enrichment must issue real embeddings.
// Before L1 the rebuild never called enrichment, so the new generation carried
// no embeddings and `index_status` falsely read fully enriched.
#[test]
fn ct3_l1_reindex_enriches_new_generation_embeddings() {
    let dir = indexed_scope_embed("mock");
    let before = tasks_of_type(&json_success_embed(&dir, "mock", &["status"]), "embedding").len();
    assert!(before > 0, "initial index must have embedding tasks");

    // Force a genuine re-split so the new chunks have text unseen by the embedding
    // store (otherwise the DB rebuild would reuse content vectors by text_hash).
    // QA21 (step4b-contract-tests-p3a.md §G, 07-adapter-spec.md §3): the
    // network-approval gate's positive condition needs
    // `[adapter.policy].allow_network = true` to remain SET after this
    // wholesale config.toml rewrite (unset/lost = gate not established) —
    // `indexed_scope_embed`'s earlier dedicated adapter grant set it, so this
    // full overwrite must carry it forward explicitly or the scope silently
    // loses its persisted opt-in.
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[chunking]\nstrategy = \"heading\"\nmax_chars = 10\n[adapter.policy]\nallow_network = true\n",
    )
    .unwrap();
    let out = json_success_embed(&dir, "mock", &["reindex", "--regenerate", "--yes"]);
    assert_eq!(out["status"], "reindexed");

    let status = json_success_embed(&dir, "mock", &["status"]);
    let emb = tasks_of_type(&status, "embedding");
    assert!(
        emb.len() > before,
        "reindex must enqueue embeddings for the new generation (L1): {status}"
    );
    assert!(
        emb.iter().all(|task| task["status"] == "done"),
        "opted-in reindex must embed the new generation, not leave it pending: {status}"
    );
}

// Scenario (a) — L1 offline: with the embedding adapter configured but NOT
// opted-in, `reindex` must still enqueue the new generation's embedding tasks so
// `index_status` surfaces them as pending. Before L1 no tasks were created and
// `index_status` reported enriched_ratio = 1.0 / pending = 0 for them.
#[test]
fn ct3_l1_reindex_offline_surfaces_pending_embeddings_in_index_status() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("note.md"),
        "# ノート\n\n## 本文\n埋め込み保留の可視化テストです。\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    // `--yes` records the opt-in rows with network_opt_in=false → embedding stays
    // offline (enqueue-only), the same as the markdownize online task.
    json_success_embed(&dir, "mock", &["index", "--yes"]);
    let before = tasks_of_type(&json_success_embed(&dir, "mock", &["status"]), "embedding").len();

    json_success_embed(&dir, "mock", &["reindex", "--regenerate", "--yes"]);
    let status = json_success_embed(&dir, "mock", &["status"]);
    let emb = tasks_of_type(&status, "embedding");
    assert!(
        emb.len() > before,
        "offline reindex must still enqueue new-generation embedding tasks (L1)"
    );
    assert!(
        emb.iter().any(|task| task["status"] == "pending"),
        "offline reindex must leave embedding tasks pending: {status}"
    );
    let search = json_success_embed(&dir, "mock", &["search", "本文 埋め込み"]);
    assert!(
        search["index_status"]["pending_enrichment_tasks"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(search["index_status"]["enriched_ratio"].as_f64().unwrap() < 1.0);
}

// Step 4 all-history keeps a deleted chunk active while its exact normalized
// binding remains reachable. Its embedding task is real pending history work,
// not a HEAD-only phantom to terminalize.
#[test]
fn ct4_deleted_historical_chunk_embedding_stays_pending() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("keep.md"),
        "# Keep\n\n## Body\n生き残るチャンクの本文です。\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("drop.md"),
        "# Drop\n\n## Body\n削除されるチャンクの本文です。\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    // Offline embedding (opt-in recorded, network_opt_in=false) → tasks stay Pending.
    json_success_embed(&dir, "mock", &["index", "--yes"]);

    let before = json_success_embed(&dir, "mock", &["status"]);
    let pending_before = tasks_of_type(&before, "embedding")
        .iter()
        .filter(|task| task["status"] == "pending")
        .count();
    assert!(
        pending_before >= 2,
        "both files' chunks start pending: {before}"
    );
    let count_before = json_success_embed(&dir, "mock", &["search", "本文"])["index_status"]
        ["pending_enrichment_tasks"]
        .as_u64()
        .unwrap();

    // Delete drop.md and re-index: its chunk remains reachable through history.
    fs::remove_file(dir.path().join("drop.md")).unwrap();
    json_success_embed(&dir, "mock", &["index", "--yes"]);

    let after = json_success_embed(&dir, "mock", &["status"]);
    let emb_after = tasks_of_type(&after, "embedding");
    assert!(
        emb_after.iter().all(|task| task["status"] == "pending"),
        "retained historical embedding tasks remain genuine pending work: {after}"
    );
    // The surviving file still has a pending embedding task (no over-terminalization).
    assert!(
        emb_after.iter().any(|task| task["status"] == "pending"),
        "the live file's embedding task must stay pending: {after}"
    );
    // Both live and historical gaps remain in index_status.
    let count_after = json_success_embed(&dir, "mock", &["search", "本文"])["index_status"]
        ["pending_enrichment_tasks"]
        .as_u64()
        .unwrap();
    assert!(
        count_after == count_before,
        "deletion must not retire retained-history embedding work: {count_before} -> {count_after}"
    );
}

// R11-2: the embedding enrichment DRIVEN inline by `index` auth-failed but the run
// reported exit 0 with no embedding keys — a silent enrichment failure. It must now
// exit 5 (docs/04 §5.6, user re-auth) with the full result JSON on stdout: local
// index succeeded (`status: indexed`), embedding failures disclosed, and the failure
// visible in `status` as well. QA2 (step4b-contract-tests-p3a.md §A, 04 §5.2): the
// task lands `paused` with `hold_reason="auth"`, never `failed` — this test used to
// assert `status=="failed"`.
#[test]
fn r11_2_index_embedding_auth_error_exits_5() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("note.md"),
        "# ノート\n\n## 本文\n認証失敗の可視化テスト本文です。\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_markdownize(&dir, "mock");
    approve_embed(&dir, "mock");
    let indexed = json_code_stdout_embed(&dir, "auth_error", 5, &["index"]);
    assert_eq!(
        indexed["status"], "indexed",
        "the local index still succeeds: {indexed}"
    );
    assert!(
        indexed["embedding_tasks_failed"].as_u64().unwrap() > 0,
        "the embedding auth failure must be disclosed on stdout: {indexed}"
    );
    let status = json_success_embed(&dir, "auth_error", &["status"]);
    assert!(
        tasks_of_type(&status, "embedding").iter().any(|task| {
            task["status"] == "paused"
                && task["hold_reason"] == "auth"
                && task["fallback_reason"] == "auth_error"
        }),
        "status must show the paused(auth) embedding task: {status}"
    );
}

// Scenario (b) — L2: budget-exceeded Paused tasks are sticky under a plain
// `batch resume`, and `--override-budget` runs both adapters symmetrically.
// Before L2, `resume --override-budget` re-paused markdownize (the override never
// reached the executor's budget judgement) while embedding, being DB-driven, ran
// even a Paused task without any override.
#[test]
fn ct3_l2_budget_paused_resume_symmetry_across_adapters() {
    let dir = tempfile::tempdir().unwrap();
    // R9-2: the markdownize online task only exists for a non-text-native file, so
    // the both-adapters budget-pause symmetry is exercised with a PDF (its
    // deterministic baseline still produces chunks for the embedding adapter).
    fs::write(
        dir.path().join("budget.pdf"),
        fake_pdf(&["スティッキー一時停止の対称性テスト本文です。"]),
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_embed(&dir, "mock");
    // A zero folder cap pauses BOTH adapters on budget.
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[budget]\nmonthly_usd_cap = 0\n",
    )
    .unwrap();
    // R11-2: the embedding enrichment DRIVEN inline by index budget-pauses here, so
    // index reports exit 6 (docs/04 §5.6) with its full result JSON still on stdout.
    let indexed = json_both_mock_code(&dir, &["index"], 6);
    assert!(
        indexed["paused_tasks"].as_u64().unwrap() > 0,
        "index must disclose the budget-paused work: {indexed}"
    );
    let status = json_both_mock(&dir, &["status"]);
    assert!(
        is_budget_paused(&status, "markdownize"),
        "markdownize must be budget-paused: {status}"
    );
    assert!(
        is_budget_paused(&status, "embedding"),
        "embedding must be budget-paused: {status}"
    );

    // resume WITHOUT override → both remain sticky-Paused (symmetry).
    json_both_mock(&dir, &["batch", "resume"]);
    let status = json_both_mock(&dir, &["status"]);
    assert!(
        is_budget_paused(&status, "markdownize"),
        "markdownize must stay paused without override: {status}"
    );
    assert!(
        is_budget_paused(&status, "embedding"),
        "embedding must stay paused without override (L2 ii): {status}"
    );

    // resume WITH override → both execute to done (symmetry).
    json_both_mock(&dir, &["batch", "resume", "--override-budget"]);
    let status = json_both_mock(&dir, &["status"]);
    assert!(
        tasks_of_type(&status, "markdownize")
            .iter()
            .all(|task| task["status"] == "done"),
        "override must run markdownize (L2 i): {status}"
    );
    assert!(
        tasks_of_type(&status, "embedding")
            .iter()
            .all(|task| task["status"] == "done"),
        "override must run embedding: {status}"
    );
}

// An operator who raises a hard cap must be able to recheck the previously
// paused work without using the cap-bypassing `--override-budget`.  The first
// recheck deliberately leaves the cap at zero: it proves the command re-enters
// the normal atomic reservation path and re-pauses both adapters without a send.
// The second recheck raises the configured folder cap and completes both paths.
#[test]
fn ct3_l2_recheck_budget_enforces_the_current_cap_for_both_adapters() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("budget-recheck.pdf"),
        fake_pdf(&["現在の上限を再判定する対称性テスト本文です。"]),
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_embed(&dir, "mock");
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[budget]\nmonthly_usd_cap = 0\n",
    )
    .unwrap();
    json_both_mock_code(&dir, &["index"], 6);

    let denied = json_both_mock_code(
        &dir,
        &["batch", "resume", "--recheck-budget", "--realtime"],
        6,
    );
    assert_eq!(denied["recheck_budget"], true);
    assert_eq!(denied["override_budget"], false);
    let status = json_both_mock(&dir, &["status"]);
    assert!(
        is_budget_paused(&status, "markdownize") && is_budget_paused(&status, "embedding"),
        "the unchanged zero cap must keep both adapters paused: {status}"
    );

    // Change only the configured cap, retaining `hard_stop = true`.  The CLI
    // re-evaluates each reservation under this cap; it has no override bit.
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[budget]\nmonthly_usd_cap = 1\nhard_stop = true\n[adapter.policy]\nallow_network = true\n",
    )
    .unwrap();
    let resumed = json_both_mock(&dir, &["batch", "resume", "--recheck-budget", "--realtime"]);
    assert_eq!(resumed["recheck_budget"], true);
    assert_eq!(resumed["override_budget"], false);
    let status = json_both_mock(&dir, &["status"]);
    assert!(
        tasks_of_type(&status, "markdownize")
            .iter()
            .all(|task| task["status"] == "done"),
        "rechecked markdownize must complete under the raised cap: {status}"
    );
    assert!(
        tasks_of_type(&status, "embedding")
            .iter()
            .all(|task| task["status"] == "done"),
        "rechecked embedding must complete under the raised cap: {status}"
    );
}

// Scenario (c) — L3: a bare `kio snapshot` advances HEAD with a raw-only
// tree. It must not grandfather an older semantic chunk into a new Evidence
// Pointer: the snapshot has no same-snapshot normalized profile/gen/manifest.
// A true raw short hash remains available through the authenticated current HEAD
// path, without exposing the prior chunk body.
#[test]
fn ct3_l3_bare_snapshot_rejects_chunk_evidence_but_opens_current_raw_short_hash() {
    let dir = indexed_scope();
    let search = json_success(&dir, &["search", "トークン TTL 3600"]);
    let row = first_result(&search);
    let chunk_hash = row["chunk_hash"].as_str().unwrap().to_owned();
    let raw_hash = row["evidence_pointer"]["raw_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let pointer = row["evidence_pointer"].clone();
    // Sanity: semantic evidence resolves while the authenticated normalized
    // snapshot is HEAD.
    let sanity = json_success(&dir, &["view", &chunk_hash]);
    assert!(view_slice(&sanity).contains("3600"));
    let snap = json_success(&dir, &["snapshot", "create", "-m", "advance"]);
    assert_eq!(
        snap["status"], "created",
        "snapshot must advance HEAD: {snap}"
    );

    // A prior pointer is still evidence for its own retained, normalized
    // commit. Rewriting it to the raw-only new snapshot must fail, as must the
    // chunk short hash whose source relation now names that bare snapshot.
    let pointer_text = pointer.to_string();
    assert!(
        kio(&dir, &["view", &pointer_text])
            .output()
            .unwrap()
            .status
            .success(),
        "a retained normalized pointer remains independently authenticated"
    );
    let mut bare_pointer = pointer;
    bare_pointer["commit"] = snap["commit_hash"].clone();
    let bare_pointer_text = bare_pointer.to_string();
    for operand in [&chunk_hash, &bare_pointer_text] {
        let output = kio(&dir, &["view", operand]).output().unwrap();
        assert!(
            !output.status.success(),
            "bare snapshot must not serve semantic evidence: {operand}"
        );
    }

    // A true raw short hash is authenticated by the current bare HEAD path and
    // returns only the raw object path, not the former chunk view.
    let raw = json_success(&dir, &["open", &raw_hash]);
    assert_eq!(
        raw["object_type"], "raw",
        "raw short hash must remain raw: {raw}"
    );
    assert_eq!(
        fs::read_to_string(raw["path"].as_str().unwrap()).unwrap(),
        fs::read_to_string(dir.path().join("auth.md")).unwrap(),
        "raw short hash must resolve the authenticated current working file"
    );
}

// Scenario (d) — L4: an embedding adapter rides on its OWN opt-in, not the
// markdownize approval. A scope approved for markdownize only (backward-compat:
// no embedding approval row) must enqueue embedding tasks without ever calling
// the adapter; granting the embedding opt-in then embeds them.
#[test]
fn ct3_l4_embedding_without_own_optin_is_enqueue_only() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("adapter.md"),
        "# アダプタ\n\n## 本文\nアダプタ単位 opt-in のテスト本文です。\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    // Approve WITHOUT the embedding seam → only the markdownize opt-in row exists.
    json_success(&dir, &["index"]);
    // Now configure the embedding adapter (mock) and re-drive enrichment via
    // reindex. The scope has no embedding opt-in row → enqueue-only.
    json_success_embed(&dir, "mock", &["reindex", "--regenerate", "--yes"]);
    let status = json_success_embed(&dir, "mock", &["status"]);
    let emb = tasks_of_type(&status, "embedding");
    assert!(!emb.is_empty(), "embedding tasks must be enqueued");
    assert!(
        emb.iter().all(|task| task["status"] == "pending"),
        "embedding without its own opt-in must stay enqueue-only (L4): {status}"
    );

    // Grant the embedding opt-in explicitly → the same chunks now embed.
    approve_embed(&dir, "mock");
    json_success_embed(&dir, "mock", &["index"]);
    let status = json_success_embed(&dir, "mock", &["status"]);
    assert!(
        tasks_of_type(&status, "embedding")
            .iter()
            .any(|task| task["status"] == "done"),
        "granting the embedding opt-in must embed the pending tasks (L4): {status}"
    );
}

// ===========================================================================
// Exploratory-audit fix regression tests (tasks/step3-bughunt-fixes.md, M2-M8
// + acceptance scenarios (b)-(f) and the two previously-untested error codes).
// ===========================================================================

// M2, superseded by 05 §1.7.2 (2026-08-11): `kio view` stopped returning chunk
// body text altogether -- JSON or not -- in favor of a full-text view path +
// span (the caller reads the body itself, from the path). Non --json `view`
// therefore has no body left to print and falls through to the same bare
// status line `open` already prints (`print_output` has no `text` field to
// special-case for a pointer resolution anymore); it must NOT leak the body.
#[test]
fn m2_view_non_json_no_longer_prints_chunk_body() {
    let dir = indexed_scope();
    let search = json_success(&dir, &["search", "トークン TTL 3600"]);
    let uri = first_result(&search)["evidence_uri"]
        .as_str()
        .unwrap()
        .to_owned();
    let stdout = kio(&dir, &["view", &uri])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(stdout).unwrap();
    assert!(
        !text.contains("トークン TTL"),
        "view (non --json) must no longer leak the body, got: {text}"
    );
    // The path IS the output (05 §1.7.2): a bare "viewed" would leave a
    // non --json caller with nothing to open, so the printed line must be a
    // readable full-text view path.
    let printed = text.trim();
    assert_ne!(
        printed, "viewed",
        "non --json view must print the view path, not a bare status: {text}"
    );
    assert!(
        printed.ends_with(".md") && fs::read_to_string(printed).is_ok(),
        "non --json view must print a readable normalized-view path, got: {text}"
    );
}

// M3 + acceptance (c): a raw_hash short form for a normal multi-heading file
// (auth.md has two `##` sections → two chunks sharing one raw_hash) resolves as
// raw instead of failing "ambiguous". `open` uses the raw path directly.
#[test]
fn m3_short_raw_hash_open_succeeds_for_multi_heading_file() {
    let dir = indexed_scope();
    let search = json_success(&dir, &["search", "トークン TTL 3600"]);
    let raw_hash = first_result(&search)["evidence_pointer"]["raw_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    // Two chunks share this raw_hash; the old code counted chunk matches and
    // rejected this as ambiguous.
    let opened = json_success(&dir, &["open", &raw_hash]);
    assert_eq!(opened["status"], "opened");
    assert_eq!(opened["object_type"], "raw");
    assert!(opened["path"].as_str().unwrap().ends_with("auth.md"));
}

// M3: a chunk_hash short form still resolves to that chunk (the other kind).
#[test]
fn m3_short_chunk_hash_view_resolves_chunk() {
    let dir = indexed_scope();
    let search = json_success(&dir, &["search", "トークン TTL 3600"]);
    let chunk_hash = first_result(&search)["chunk_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let viewed = json_success(&dir, &["view", &chunk_hash]);
    assert!(view_slice(&viewed).contains("トークン TTL"));
}

// M4 + acceptance (d): corrupting a source sqlite.db after indexing cannot
// exclude that scope from a replica-only multi-scope search. The replica has
// already received its candidate material and is the read boundary.
#[test]
fn m4_corrupt_source_sqlite_does_not_exclude_multiscope_replica_search() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let a = parent.path().join("a");
    let b = parent.path().join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(a.join("a.md"), "# A\n\n## Sec\nalphaunique token\n").unwrap();
    fs::write(b.join("b.md"), "# B\n\n## Sec\nbetaunique token\n").unwrap();
    json_success_path(&a, &data_home, &["init"]);
    json_success_path(&b, &data_home, &["init"]);
    json_success_path(&a, &data_home, &["index"]);
    json_success_path(&b, &data_home, &["index"]);
    let a_scope_id = read_scope_id(&a);
    let b_scope_id = read_scope_id(&b);

    // Corrupt scope b's writer-side source index in place. Search must not
    // inspect it after the write-through replica has been published.
    fs::write(
        b.join(".kio/index/sqlite.db"),
        b"this is not a sqlite database",
    )
    .unwrap();

    // A successful replica search still returns both scopes.
    let output = hermetic_kio_command()
        .current_dir(&a)
        .env("XDG_CONFIG_HOME", data_home.join("config"))
        .env("XDG_DATA_HOME", data_home.join("data"))
        .env("XDG_CACHE_HOME", data_home.join("cache"))
        .args(["search", "token", "--mode", "text", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let search: Value = serde_json::from_slice(&output).unwrap();
    let _generation = replica_collection_generation(&search);
    assert_eq!(search["searched_scopes"].as_array().unwrap().len(), 2);
    assert!(search["excluded_scopes"].as_array().unwrap().is_empty());
    let result_scopes = search["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|hit| hit["evidence_pointer"]["scope_id"].as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        result_scopes,
        std::collections::BTreeSet::from([a_scope_id.as_str(), b_scope_id.as_str()]),
        "both replica-published scopes must remain searchable: {search:#?}"
    );
}

// M4: the same isolation holds for a one-scope search. A corrupt source index
// is not a candidate-time error after its replica publication.
#[test]
fn m4_single_corrupt_source_sqlite_is_served_by_the_replica() {
    let dir = indexed_scope();
    fs::write(
        dir.path().join(".kio/index/sqlite.db"),
        b"not a sqlite database at all",
    )
    .unwrap();
    let search = json_success(&dir, &["search", "認証仕様"]);
    let _generation = replica_collection_generation(&search);
    assert!(
        !search["results"].as_array().unwrap().is_empty(),
        "the published replica must survive a corrupt source index: {search:#?}"
    );
}

// M5 + acceptance (e): two consecutive `view`s (and opens) that fall back to the
// read-only CAS open-cache both succeed — the second must reuse the cache, not
// `fs::copy` onto the read-only file (EACCES).
#[test]
fn m5_repeated_view_via_cas_cache_is_idempotent() {
    let dir = indexed_scope();
    let search = json_success(&dir, &["search", "トークン TTL 3600"]);
    let uri = first_result(&search)["evidence_uri"]
        .as_str()
        .unwrap()
        .to_owned();
    // Remove the working-tree file so resolution must expand the CAS raw object
    // into the read-only open cache.
    fs::remove_file(dir.path().join("auth.md")).unwrap();

    let first = json_success(&dir, &["view", &uri]);
    assert!(view_slice(&first).contains("トークン TTL"));
    // The second view previously failed with EACCES (fs::copy onto read-only cache).
    let second = json_success(&dir, &["view", &uri]);
    assert!(view_slice(&second).contains("トークン TTL"));
    // open twice as well.
    let opened1 = json_success(&dir, &["open", &uri]);
    assert_eq!(opened1["temporary"], true);
    let opened2 = json_success(&dir, &["open", &uri]);
    assert_eq!(opened2["temporary"], true);
    assert!(Path::new(opened2["path"].as_str().unwrap()).is_file());
}

// M6: a tampered Evidence Pointer whose raw_hash names file B but whose
// chunk_hash resolves to file A's chunk is rejected — the resolver requires the
// chunk row to bind to the pointer's (raw_hash, tool_profile_hash) identity.
#[test]
fn m6_tampered_pointer_identity_mismatch_is_rejected() {
    let dir = indexed_scope();
    let auth = json_success(&dir, &["search", "トークン TTL 3600"]);
    let pointer_a = first_result(&auth)["evidence_pointer"].clone();
    let ranking = json_success(&dir, &["search", "RRF 定数 k 60"]);
    let raw_hash_b = first_result(&ranking)["evidence_pointer"]["raw_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(pointer_a["raw_hash"].as_str().unwrap(), raw_hash_b);

    // Keep A's chunk_hash / commit, but claim B's raw_hash: "raw is B, body is A".
    let mut tampered = pointer_a.clone();
    tampered["raw_hash"] = serde_json::json!(raw_hash_b);
    let err = json_failure(&dir, &["view", &tampered.to_string()], 4);
    assert_eq!(err["error_code"], "KIO-E-EVIDENCE-POINTER-INVALID-001");
    // The unmodified pointer still resolves (no over-rejection of valid pointers).
    let ok = json_success(&dir, &["view", &pointer_a.to_string()]);
    assert!(view_slice(&ok).contains("トークン TTL"));
}

// M7: an `object` URI dispatches to the correct image CAS directory, but a
// manually planted same-scope image object is not disclosure authority. The
// indexed PNG supplies the authenticated immutable typed owner required to
// open its URI.
#[test]
fn m7_object_uri_dispatches_by_type_directory() {
    use base64::Engine as _;
    use kio_core::cas::hash_bytes;

    let dir = tempfile::tempdir().unwrap();
    let png = base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR4nGP4DwQACfsD/fteaysAAAAASUVORK5CYII=")
        .unwrap();
    fs::write(dir.path().join("scan.png"), png).unwrap();
    kio(&dir, &["init"]).assert().success();
    json_success(&dir, &["index", "--offline"]);
    let search = json_success(&dir, &["search", "scan", "--mode", "text"]);
    let image_uri = first_result(&search)["related_images"].as_array().unwrap()[0]["image_uri"]
        .as_str()
        .unwrap()
        .to_owned();
    let (prefix, image_hash) = image_uri.rsplit_once('/').unwrap();
    let scope_id = prefix
        .strip_prefix("kio://")
        .unwrap()
        .strip_suffix("/object/image")
        .unwrap();

    // Correct dispatch: a typed-owned image resolves from objects/image.
    let opened = json_success(&dir, &["open", &image_uri]);
    assert_eq!(opened["object_type"], "image");
    assert!(Path::new(opened["path"].as_str().unwrap()).is_file());

    // CAS presence alone cannot authorize an unrelated image in the same
    // scope. This remains distinct from type-directory dispatch.
    let planted = b"unowned-image-cas-bytes";
    let planted_hash = hash_bytes(planted);
    let repo = Repository::open(dir.path()).unwrap();
    assert_eq!(
        repo.object_store()
            .write_content_object(ContentObjectKind::Image, planted)
            .unwrap(),
        planted_hash
    );
    let denied = json_failure(
        &dir,
        &[
            "open",
            &format!("kio://{scope_id}/object/image/{planted_hash}"),
        ],
        4,
    );
    assert_eq!(denied["error_code"], "KIO-E-OBJECT-POLICY-001");

    // Same hash under object/raw must NOT resolve — PA01 (§A, U22) now rejects
    // raw-type object URIs categorically at parse time (exit 2).
    let raw_uri_error = json_failure(
        &dir,
        &["open", &format!("kio://{scope_id}/object/raw/{image_hash}")],
        2,
    );
    assert_eq!(raw_uri_error["error_code"], "KIO-E-CONFIG-USAGE-001");
    // `normalized` is path-named (not single-hash addressable) -> invalid usage.
    json_failure(
        &dir,
        &[
            "open",
            &format!("kio://{scope_id}/object/normalized/{image_hash}"),
        ],
        2,
    );
}

// M8 + acceptance (f): a negative budget cap in the USER (device) config.toml is
// rejected at startup with exit 2, exactly like the folder config already was.
#[test]
fn m8_user_config_negative_budget_cap_rejected() {
    let dir = tempfile::tempdir().unwrap();
    kio(&dir, &["init"]).assert().success();
    let user_config = dir.path().join(".test-config/kio/config.toml");
    fs::create_dir_all(user_config.parent().unwrap()).unwrap();
    fs::write(&user_config, "[budget]\nmonthly_usd_cap = -5\n").unwrap();
    let err = json_failure(&dir, &["status"], 2);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-SCHEMA-001");
}

// M8: a valid user config with a non-negative cap passes (no over-rejection).
#[test]
fn m8_user_config_valid_budget_cap_accepted() {
    let dir = tempfile::tempdir().unwrap();
    kio(&dir, &["init"]).assert().success();
    let user_config = dir.path().join(".test-config/kio/config.toml");
    fs::create_dir_all(user_config.parent().unwrap()).unwrap();
    fs::write(&user_config, "[budget]\nmonthly_usd_cap = 25\n").unwrap();
    json_success(&dir, &["status"]);
}

// Step 4 implements exact snapshot search; HEAD is a canonical commit selector.
#[test]
fn ct4_search_at_head_is_implemented() {
    let dir = indexed_scope();
    // PC59/PC60 (06 §3 L226-227): `--at` requires a single, non-`--descendants`
    // `--scope` — an explicit commit cannot be resolved against more than one
    // independent scope DAG. `indexed_scope()` is a single scope, so `--scope .`
    // satisfies the requirement without changing what this test exercises.
    let result = json_success(
        &dir,
        &["search", "認証仕様", "--at", "HEAD", "--scope", "."],
    );
    assert!(!result["results"].as_array().unwrap().is_empty());
    assert_eq!(
        result["results"][0]["evidence_pointer"]["commit"],
        result["searched_scopes"][0]["snapshot_at"]
    );
}

/// PC59 (06 §3 L226-227): `--at` without an explicit single `--scope` is
/// invalid usage (exit 2) — the default multi-scope enumeration cannot
/// resolve one commit across independent scope DAGs.
#[test]
fn pc59_search_at_without_scope_is_invalid_usage() {
    let dir = indexed_scope();
    let err = json_failure(&dir, &["search", "認証仕様", "--at", "HEAD"], 2);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-USAGE-001");
}

/// PC60(a): `--at` combined with `--scope --descendants` (potentially
/// multiple scopes) is also invalid usage, even though a single `--scope`
/// alone (PC60(b), exercised by `ct4_search_at_head_is_implemented`) is fine.
#[test]
fn pc60_search_at_with_descendants_is_invalid_usage() {
    let dir = indexed_scope();
    let err = json_failure(
        &dir,
        &[
            "search",
            "認証仕様",
            "--at",
            "HEAD",
            "--scope",
            ".",
            "--descendants",
        ],
        2,
    );
    assert_eq!(err["error_code"], "KIO-E-CONFIG-USAGE-001");
}

// step4b-contract-tests-p2b.md PB21/22: a scope_id registered against two
// distinct .kio is fail-closed (KIO-E-REGISTRY-DUP-001), regardless of
// last_seen_at — this fixture happens to also share the newest timestamp,
// which used to be the ONLY duplicate shape the old
// KIO-E-EVIDENCE-SCOPE-AMBIGUOUS-001 code detected (PB21 widens detection to
// every live duplicate; PB22 retires the old code in favor of the new
// REGISTRY namespace).
#[test]
fn minor_evidence_scope_ambiguous_error_code_is_emitted() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let a = parent.path().join("a");
    let b = parent.path().join("b");
    let elsewhere = parent.path().join("elsewhere");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::create_dir_all(&elsewhere).unwrap();
    fs::write(
        a.join("auth.md"),
        "# 認証仕様\n\n## API Token\nトークン TTL は 3600 秒です。\n",
    )
    .unwrap();
    json_success_path(&a, &data_home, &["init"]);
    json_success_path(&a, &data_home, &["index"]);
    let search = json_success_path(&a, &data_home, &["search", "トークン TTL 3600"]);
    let pointer = first_result(&search)["evidence_pointer"].clone();
    let scope_id = pointer["scope_id"].as_str().unwrap().to_owned();

    // Init b, then give its scope.json the SAME scope_id as a (a duplicate正本).
    json_success_path(&b, &data_home, &["init"]);
    let b_scope_path = b.join(".kio/scope.json");
    let mut b_scope: Value =
        serde_json::from_str(&fs::read_to_string(&b_scope_path).unwrap()).unwrap();
    b_scope["scope_id"] = serde_json::json!(scope_id);
    fs::write(
        &b_scope_path,
        serde_json::to_string_pretty(&b_scope).unwrap(),
    )
    .unwrap();

    // Register both .kio under the shared scope_id with the SAME last_seen_at,
    // newer than the index-time registration so they form the unique newest set
    // (both resolve to distinct .kio -> ambiguous winner).
    let registry = RegistryDb::open(registry_path(&data_home)).unwrap();
    for root in [&a, &b] {
        registry
            .upsert(&RegistryEntry {
                scope_id: scope_id.clone(),
                kio_path: root.join(".kio").display().to_string(),
                root_path: root.display().to_string(),
                participates_in_global_search: true,
                indexed: true,
                last_seen_at: "2099-01-01T00:00:00Z".to_owned(),
            })
            .unwrap();
    }

    // Force registry resolution (broken scope_path hint) from a neutral cwd.
    let mut orphan = pointer.clone();
    orphan["scope_path"] = serde_json::json!(parent.path().join("gone/.kio").display().to_string());
    let (code, err) = run_json(&elsewhere, &data_home, &["view", &orphan.to_string()]);
    assert_eq!(code, 4, "ambiguous scope must fail: {err}");
    assert_eq!(err["error_code"], "KIO-E-REGISTRY-DUP-001");
}

// ===========================================================================
// Second exploratory-audit round (tasks/step3-bughunt2-fixes.md, N1-N8):
// acceptance scenarios (a)-(f). Scenario (g) (O(N²) chunking) lives as a timing
// proxy in kio-index/src/chunking.rs (n6_chunking_scales_linearly...).
// ===========================================================================

// (a) / N1: a Tier B (candidate-secret) file is ingested locally but its online
// send (embedding here) is HELD until an explicit `--send-secrets` approval, and
// stays visible in `kio status` quarantine + as a held task the whole time.
#[test]
fn n1_tier_b_online_send_held_until_send_secrets() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("notes.md"),
        "# Notes\n\n## Body\n通常の本文です。十分な長さの段落を含みます。\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("api_secret.md"),
        "# 秘密メモ\n\n## Body\n秘匿候補ファイルの本文です。十分な長さの段落。\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_embed(&dir, "mock");
    json_success_embed(&dir, "mock", &["index", "--online"]);

    let status = json_success_embed(&dir, "mock", &["status"]);
    let secret_embed: Vec<_> = tasks_of_type(&status, "embedding")
        .into_iter()
        .filter(|task| task["input_path"] == "api_secret.md")
        .collect();
    assert!(
        !secret_embed.is_empty(),
        "Tier B file must produce an embedding task: {status}"
    );
    assert!(
        secret_embed
            .iter()
            .all(|task| task["status"] == "paused"
                && task["fallback_reason"] == "secrets_tier_b_hold"),
        "Tier B embedding must be held, not sent: {status}"
    );
    assert!(
        tasks_of_type(&status, "embedding")
            .iter()
            .any(|task| { task["input_path"] == "notes.md" && task["status"] == "done" }),
        "non-secret embedding must still execute: {status}"
    );
    assert!(
        status["quarantine"]
            .as_array()
            .unwrap()
            .iter()
            .any(|q| { q["path"] == "api_secret.md" && q["reason"] == "secrets_tier_b" }),
        "Tier B must be recorded in quarantine: {status}"
    );

    // A separate secret-content approval lifts the hold → Tier B chunks embed.
    json_success_embed(
        &dir,
        "mock",
        &["adapter", "approve", "--all", "--yes", "--send-secrets"],
    );
    json_success_embed(&dir, "mock", &["index", "--online"]);
    let status = json_success_embed(&dir, "mock", &["status"]);
    assert!(
        tasks_of_type(&status, "embedding")
            .iter()
            .any(|task| { task["input_path"] == "api_secret.md" && task["status"] == "done" }),
        "adapter approve --send-secrets must release and embed Tier B chunks: {status}"
    );
}

// (b) / N2: a manual `kio snapshot` must not bake Tier A secrets into the CAS or
// the latest tree.
#[test]
fn n2_manual_snapshot_excludes_tier_a() {
    use kio_core::cas::hash_bytes;
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "hello").unwrap();
    fs::write(dir.path().join(".env"), "TOKEN=supersecret").unwrap();
    kio(&dir, &["init"]).assert().success();

    let snap = json_success(&dir, &["snapshot", "create", "-m", "manual"]);
    assert_eq!(
        snap["status"], "created",
        "snapshot must commit a.txt: {snap}"
    );
    let tree = json_success(&dir, &["inspect", snap["tree_hash"].as_str().unwrap()]);
    assert!(
        !tree["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["path"] == ".env"),
        "manual snapshot must not put .env in the tree: {tree}"
    );
    let env_hash = hash_bytes(b"TOKEN=supersecret");
    assert!(
        !object_path(&dir.path().join(".kio"), "raw", &env_hash).exists(),
        "manual snapshot must not write .env plaintext to objects/raw"
    );
}

// (c) / N3: errors.jsonl must mask the `path` field under redact_logs (default on).
#[test]
fn n3_errors_jsonl_redacts_path() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "hello").unwrap();
    kio(&dir, &["init"]).assert().success();
    fs::write(dir.path().join(".kio/tasks.jsonl"), "{ not json\n").unwrap();
    json_failure(&dir, &["status"], 4);
    let errors = fs::read_to_string(dir.path().join(".test-data/kio/logs/errors.jsonl")).unwrap();
    let last: Value = serde_json::from_str(errors.lines().last().unwrap()).unwrap();
    assert_eq!(
        last["context"]["path"], "[redacted]",
        "errors.jsonl must redact path under redact_logs: {last}"
    );
}

// (d) / N4: `diff` / `tag <commit>` reject path-traversal operands with exit 2
// instead of turning `refs/tags`.join into an out-of-scope existence oracle.
#[test]
fn n4_diff_and_tag_reject_traversal_operands() {
    let dir = indexed_scope();
    let err = json_failure(&dir, &["diff", "../../../etc/passwd", "HEAD"], 2);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-USAGE-001");
    let err = json_failure(&dir, &["tag", "mytag", "../../../etc/passwd"], 2);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-USAGE-001");
}

// F4: a tag whose NAME is `HEAD` or a `sha256:` hash is permanently shadowed by
// `resolve_commit` (which resolves those forms before ever consulting
// refs/tags), so creating one must be rejected instead of returning a dead-ref
// "success". Ordinary tag names are unaffected.
#[test]
fn f4_tag_rejects_reserved_head_and_hash_names() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.md"), "# T\n\n## S\nbody text here.\n").unwrap();
    kio(&dir, &["init"]).assert().success();
    // A commit must exist so a legitimate tag (resolving HEAD) can be created.
    json_success(&dir, &["snapshot", "create", "-m", "first"]);

    // Reserved name `HEAD`: rejected before any ref is written.
    let err = json_failure(&dir, &["tag", "HEAD"], 2);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-USAGE-001");
    assert!(!dir.path().join(".kio/refs/tags/HEAD").exists());

    // Reserved name in `sha256:<64hex>` form: also rejected.
    let hash_name = format!("sha256:{}", "a".repeat(64));
    let err = json_failure(&dir, &["tag", &hash_name], 2);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-USAGE-001");

    // Case variants normalize to the same reserved hash operand. This must be
    // rejected semantically as a hash collision, not merely because `:` is an
    // invalid portable-leaf character.
    let uppercase_hash_name = format!("SHA256:{}", "A".repeat(64));
    let err = json_failure(&dir, &["tag", &uppercase_hash_name], 2);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-USAGE-001");
    assert!(err["message"].as_str().unwrap().contains("commit hash"));

    // A normal tag name still resolves HEAD and is created.
    let ok = json_success(&dir, &["tag", "v1"]);
    assert!(
        ok["commit_hash"].as_str().is_some(),
        "tag v1 should succeed: {ok}"
    );
    assert!(Path::new(ok["path"].as_str().unwrap()).is_file());
}

// (e) / N5: after `reindex --force`, a pointer that keeps the OLD commit but
// splices in a NEW-generation chunk_hash is rejected (gen binding), while the
// untampered old pointer still resolves.
#[test]
fn n5_pointer_rejects_generation_mixing_after_reindex() {
    let dir = indexed_scope();
    let before = json_success(&dir, &["search", "トークン TTL 3600"]);
    let old_pointer = first_result(&before)["evidence_pointer"].clone();
    json_success(&dir, &["reindex", "--regenerate", "--yes"]);
    let after = json_success(&dir, &["search", "トークン TTL 3600"]);
    let new_chunk_hash = first_result(&after)["evidence_pointer"]["chunk_hash"].clone();
    assert_ne!(
        old_pointer["chunk_hash"], new_chunk_hash,
        "reindex --force must produce a new-generation chunk_hash"
    );
    let mut tampered = old_pointer.clone();
    tampered["chunk_hash"] = new_chunk_hash;
    let err = json_failure(&dir, &["view", &tampered.to_string()], 4);
    assert_eq!(
        err["error_code"], "KIO-E-EVIDENCE-POINTER-INVALID-001",
        "generation-mixing pointer must be rejected (N5)"
    );
    let ok = json_success(&dir, &["view", &old_pointer.to_string()]);
    assert!(view_slice(&ok).contains("トークン TTL"));
}

// (f) / N7: a single-shot `--online` drives embedding enrichment (previously only
// markdownize honored the flag; embedding stayed Pending).
#[test]
fn n7_online_flag_drives_embedding_enrichment() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("notes.md"),
        "# Notes\n\n## Body\n通常本文の十分な長さのテキストです。\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_embed(&dir, "mock");
    json_success_embed(&dir, "mock", &["index", "--yes", "--online"]);
    let status = json_success_embed(&dir, "mock", &["status"]);
    assert!(
        tasks_of_type(&status, "embedding")
            .iter()
            .any(|task| task["status"] == "done"),
        "single-shot --online must execute embedding (N7): {status}"
    );
}

// ===========================================================================
// Third exploratory-audit round (tasks/step3-bughunt3-fixes.md, O1-O7):
// acceptance scenarios (a)-(f).
// ===========================================================================

// (a) / O1: a cursor minted for another scope cannot bypass a --scope restriction.
// Step 4 v2 rejects any restriction that drops a frozen active scope; a
// tampered/forged cursor also fails HMAC verification.
#[test]
fn o1_cursor_scope_restriction_and_signature() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let safe = parent.path().join("safe");
    let vault = parent.path().join("vault");
    fs::create_dir_all(&safe).unwrap();
    fs::create_dir_all(&vault).unwrap();
    fs::write(
        safe.join("safe.md"),
        "# 公開\n\n## Body\n公開情報の本文です。十分な長さの段落を含みます。\n",
    )
    .unwrap();
    // Same query terms as CT3-CURSOR (認証仕様) so one query yields several chunks
    // (=> a paging cursor), plus a secret marker in the body.
    fs::write(
        vault.join("auth.md"),
        "# 認証仕様\n\n## API Token\nトークン TTL は 3600 秒です。秘密鍵 TOP-SECRET-KEY-XYZ を含みます。\n\n## Scopes\nスコープは read write admin です。\n",
    )
    .unwrap();
    fs::write(
        vault.join("ranking.md"),
        "# 検索ランキング\n\n## RRF 融合\nRRF の定数 k=60 を使います。\n\n## MMR 多様化\nMMR の係数 lambda 0.7 で多様化します。\n",
    )
    .unwrap();
    json_success_path(&safe, &data_home, &["init"]);
    json_success_path(&vault, &data_home, &["init"]);
    json_success_path(&safe, &data_home, &["index"]);
    json_success_path(&vault, &data_home, &["index"]);

    // Legitimate owner pages their own vault: --scope <vault> freezes the cursor's
    // scope set to the vault.
    let first = json_success_path(
        &vault,
        &data_home,
        &[
            "search",
            "認証仕様",
            "--scope",
            vault.to_str().unwrap(),
            "--limit",
            "1",
        ],
    );
    let cursor = first["paging"]["next_cursor"]
        .as_str()
        .expect("vault page 1 must yield a cursor")
        .to_owned();

    // Attacker in the safe scope replays the vault cursor but restricts --scope .
    // The active vault scope cannot be intersected out of a signed v2 replay.
    let (code, resp) = run_json(
        &safe,
        &data_home,
        &[
            "search",
            "認証仕様",
            "--scope",
            ".",
            "--cursor",
            &cursor,
            "--limit",
            "5",
        ],
    );
    let dump = resp.to_string();
    assert!(
        !dump.contains("TOP-SECRET-KEY-XYZ"),
        "a cursor must not leak another scope's content across a --scope restriction: {dump}"
    );
    assert_eq!(
        code, 2,
        "dropping an active cursor scope is rejected: {resp}"
    );
    assert_eq!(resp["error_code"], "KIO-E-SEARCH-CURSOR-001");
    assert_eq!(resp["context"]["reason"], "active_scope_unavailable");

    // O1(b): a forged cursor (payload byte flipped) fails signature verification.
    let (payload, signature) = cursor.rsplit_once('.').unwrap();
    let mut forged = payload.to_owned();
    let last = forged.pop().unwrap();
    forged.push(if last == 'A' { 'B' } else { 'A' });
    let forged = format!("{forged}.{signature}");
    let (code, err) = run_json(
        &vault,
        &data_home,
        &["search", "認証仕様", "--cursor", &forged, "--limit", "5"],
    );
    assert_eq!(code, 2, "a forged cursor is a usage error: {err}");
    assert_eq!(err["error_code"], "KIO-E-SEARCH-CURSOR-001");
}

// (b) / O2: `--text` must never send the query to the embedding endpoint, while a
// vector-resolving search does (proving the send seam works — pair-discriminated).
#[test]
fn o2_text_search_never_sends_query_embedding() {
    let dir = indexed_scope_embed("mock");
    let trace = dir.path().join("query-embed-trace.log");

    // --text: the query embedding must NOT be computed/sent.
    hermetic_kio_command()
        .current_dir(dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join(".test-config"))
        .env("XDG_DATA_HOME", dir.path().join(".test-data"))
        .env("XDG_CACHE_HOME", dir.path().join(".test-cache"))
        .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
        .env("KIO_TEST_QUERY_EMBED_TRACE", &trace)
        .args(["search", "認証仕様 トークン", "--mode", "text", "--json"])
        .assert()
        .success();
    assert!(
        !trace.exists(),
        "--text must not reach the embedding send path (trace file was written)"
    );

    // auto → hybrid: the same seam DOES send, so the trace appears (discriminator).
    hermetic_kio_command()
        .current_dir(dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join(".test-config"))
        .env("XDG_DATA_HOME", dir.path().join(".test-data"))
        .env("XDG_CACHE_HOME", dir.path().join(".test-cache"))
        .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
        .env("KIO_TEST_QUERY_EMBED_TRACE", &trace)
        .args(["search", "認証仕様 トークン", "--json"])
        .assert()
        .success();
    assert!(
        trace.exists() && fs::read_to_string(&trace).unwrap().contains("認証仕様"),
        "a vector-resolving search must send the query embedding"
    );
}

// (c) / O3: `batch resume` now holds the folder store lock end-to-end, so a
// concurrent holder makes it fail fast (KIO-E-STORE-LOCKED-001, exit 3) rather
// than racing tasks.jsonl / the ledger into a double send.
#[test]
fn o3_batch_resume_takes_the_store_lock() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.md"), "# A\n\n## B\n本文です。\n").unwrap();
    kio(&dir, &["init"]).assert().success();
    // A live (non-stale) lock held by "another process".
    fs::write(dir.path().join(".kio/.lock"), "{}").unwrap();
    let err = json_failure(&dir, &["batch", "resume"], 3);
    assert_eq!(err["error_code"], "KIO-E-STORE-LOCKED-001");
}

// (d) / O4: a crafted PDF whose multibyte char straddles the /Page lookahead
// window indexes cleanly (no char-boundary panic / exit 101, no body dump).
#[test]
fn o4_crafted_multibyte_pdf_does_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    // "/PageXあ": the 3-byte あ sits across the +8 byte window from "/Page"; the
    // old str slice panicked here. `BT` gives the file a text layer.
    let mut pdf = Vec::new();
    pdf.extend_from_slice(b"%PDF-1.4\nBT (hi) Tj ET\n");
    pdf.extend_from_slice(
        "/PageXあ /Type /Page trailing padding to extend the length\n".as_bytes(),
    );
    pdf.extend_from_slice(b"%%EOF\n");
    fs::write(dir.path().join("crafted.pdf"), &pdf).unwrap();
    kio(&dir, &["init"]).assert().success();
    let assert = kio(&dir, &["index", "--json"]).assert();
    let code = assert.get_output().status.code().unwrap();
    assert_ne!(code, 101, "crafted PDF must not panic (exit 101)");
    assert_eq!(code, 0, "crafted PDF must index cleanly (exit 0)");
}

// (e) / O5: a 0-chunk scope (empty folder) indexes cleanly (exit 0), instead of a
// half-initialized "commit but no index" that fails every re-index with exit 2.
#[test]
fn o5_empty_scope_indexes_with_exit_0() {
    let dir = tempfile::tempdir().unwrap();
    kio(&dir, &["init"]).assert().success();
    kio(&dir, &["index", "--json"]).assert().success();
    // Re-index is also clean (no stuck "commit, no index" state).
    kio(&dir, &["index", "--json"]).assert().success();
}

// (f) / O6: a too-short `sha256:` operand is a usage error (exit 2), not a slice
// panic in cas_object_path's digest[0..2].
#[test]
fn o6_short_sha256_operand_is_usage_error() {
    let dir = indexed_scope();
    let err = json_failure(&dir, &["open", "sha256:a"], 2);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-USAGE-001");
    let err = json_failure(&dir, &["view", "sha256:ZZZZ"], 2);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-USAGE-001");
}

// (g) / O7: a scope_id collision (a wholesale `.kio` copy) makes a cursor replay
// ambiguous — detected the same way the Evidence path is
// (KIO-E-REGISTRY-DUP-001, step4b-contract-tests-p2b.md PB21/22), not
// silently pinned to one copy. R23-25 (05 §1.8 L425 / 06 §7 L361 "DUP →
// exit 3"): the search-domain occurrence of this code is retryable exit 3
// (dedupe, then retry) — distinct from the Evidence-resolution path's own
// exit 4 default (`registry_duplicate_error`, unaffected by this fix).
#[test]
fn o7_cursor_replay_detects_scope_id_collision() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let a = parent.path().join("a");
    let b = parent.path().join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(
        a.join("auth.md"),
        "# 認証仕様\n\n## API Token\nトークン TTL は 3600 秒です。\n\n## Scopes\nスコープは read write admin です。\n",
    )
    .unwrap();
    fs::write(
        a.join("ranking.md"),
        "# 検索ランキング\n\n## RRF 融合\nRRF の定数 k=60 を使います。\n\n## MMR 多様化\nMMR の係数 lambda 0.7 で多様化します。\n",
    )
    .unwrap();
    json_success_path(&a, &data_home, &["init"]);
    json_success_path(&a, &data_home, &["index"]);
    let first = json_success_path(&a, &data_home, &["search", "認証仕様", "--limit", "1"]);
    let cursor = first["paging"]["next_cursor"]
        .as_str()
        .expect("page 1 must yield a cursor")
        .to_owned();
    let scope_id = first["searched_scopes"].as_array().unwrap()[0]["scope_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Copy a's identity into b and register both under the shared scope_id with the
    // SAME newest last_seen_at → the cursor's scope_id now resolves to two .kio.
    json_success_path(&b, &data_home, &["init"]);
    let b_scope_path = b.join(".kio/scope.json");
    let mut b_scope: Value =
        serde_json::from_str(&fs::read_to_string(&b_scope_path).unwrap()).unwrap();
    b_scope["scope_id"] = serde_json::json!(scope_id);
    fs::write(
        &b_scope_path,
        serde_json::to_string_pretty(&b_scope).unwrap(),
    )
    .unwrap();
    let registry = RegistryDb::open(registry_path(&data_home)).unwrap();
    for root in [&a, &b] {
        registry
            .upsert(&RegistryEntry {
                scope_id: scope_id.clone(),
                kio_path: root.join(".kio").display().to_string(),
                root_path: root.display().to_string(),
                participates_in_global_search: true,
                indexed: true,
                last_seen_at: "2099-01-01T00:00:00Z".to_owned(),
            })
            .unwrap();
    }

    let (code, err) = run_json(&a, &data_home, &["search", "認証仕様", "--cursor", &cursor]);
    assert_eq!(code, 3, "ambiguous cursor scope must fail: {err}");
    assert_eq!(err["error_code"], "KIO-E-REGISTRY-DUP-001");
}

// ---------------------------------------------------------------------------
// Step 3 bug-hunt round 4 (P1-P9) regression tests.
// ---------------------------------------------------------------------------

/// P1: a poisoned tasks.jsonl whose online markdownize task points outside the
/// scope (absolute path / `..` traversal) must be rejected at read time
/// (KIO-E-STORE-PATH-001, exit 2) so `batch resume` never reads the external
/// file or sends it to the online adapter.
#[test]
fn p1_batch_resume_rejects_out_of_scope_task_input_path() {
    for poison_path in [
        "/etc/hosts",
        "../../../../../../etc/hosts",
        "sub/secret.txt",
    ] {
        let dir = tempfile::tempdir().unwrap();
        // R9-2: a PDF so `index` enqueues a real online task whose
        // output_ref the poison row reuses (text-native files enqueue none).
        fs::write(dir.path().join("a.pdf"), fake_pdf(&["hello world content"])).unwrap();
        kio(&dir, &["init"]).assert().success();
        // Records the online opt-in and enqueues legitimate tasks.
        json_success(&dir, &["index"]);
        let online_output_ref = first_online_output_ref(&json_success(&dir, &["status"]));
        // Append a pending online markdownize task escaping the scope.
        let tasks = dir.path().join(".kio/tasks.jsonl");
        let poison = serde_json::json!({
            "task_id": "task_poison",
            "type": "markdownize",
            "mode": "full",
            "input_path": poison_path,
            "input_hash": format!("sha256:{}", "0".repeat(64)),
            "previous_raw_hash": null,
            "parent_run_id": null,
            "changed_unit_keys": [],
            "output_ref": online_output_ref,
            "unit_keys": null,
            "status": "pending",
            "attempts": 0,
            "next_retry_at": null,
            "deadline": null,
            "heartbeat_at": null,
            "fallback_reason": null,
            "bbox_annotation_enabled": true,
            "hold_reason": null,
            "reserved_usd": null,
            "reserved_month": null,
            "reservation_id": null,
            "created_at": "2026-04-25T12:00:00Z"
        });
        let mut line = serde_json::to_string(&poison).unwrap();
        line.push('\n');
        let mut existing = fs::read_to_string(&tasks).unwrap_or_default();
        existing.push_str(&line);
        fs::write(&tasks, existing).unwrap();

        // `batch resume` must reject before reading the external file. The mock
        // The online markdownize seam is set so that if the guard were missing
        // the task would execute, proving the guard blocks it.
        let err = kio(&dir, &["batch", "resume"])
            .env(TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV, "mock")
            .arg("--json")
            .assert()
            .code(2)
            .get_output()
            .stderr
            .clone();
        let err: Value = serde_json::from_slice(&err).unwrap();
        assert_eq!(
            err["error_code"], "KIO-E-STORE-PATH-001",
            "poison {poison_path:?} must be rejected"
        );
        // The offending task never completed (rejection happens at read time,
        // before any adapter call), so no normalized output ref was recorded.
        let tasks_after = fs::read_to_string(&tasks).unwrap();
        assert!(!tasks_after.contains("normalized_output"));
    }
}

/// P4: with `redact_logs` (default true), a corrupt store file error must not
/// leak the scope's absolute path into errors.jsonl — neither in the context
/// (N3) nor in the message (the P4 gap: the `corrupt store file at {path}`
/// Display embedded the path verbatim).
#[test]
fn p4_corrupt_store_error_message_has_no_absolute_path() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "hi").unwrap();
    kio(&dir, &["init"]).assert().success();
    json_success(&dir, &["index"]);
    // Corrupt tasks.jsonl so the next read raises KIO-E-STORE-CORRUPT-001.
    let tasks = dir.path().join(".kio/tasks.jsonl");
    let mut contents = fs::read_to_string(&tasks).unwrap_or_default();
    contents.push_str("this is not valid json{{{\n");
    fs::write(&tasks, contents).unwrap();

    kio(&dir, &["status"]).assert().failure();

    let errors = fs::read_to_string(dir.path().join(".test-data/kio/logs/errors.jsonl")).unwrap();
    let record = errors
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|value| value["code"] == "KIO-E-STORE-CORRUPT-001")
        .expect("a corrupt-store error must be logged");
    // The scope's absolute path (the tempdir) must not appear in message or context.
    let scope_abs = dir.path().to_string_lossy().into_owned();
    assert!(
        !record["message"].as_str().unwrap().contains(&scope_abs),
        "message leaked an absolute path: {}",
        record["message"]
    );
    assert!(
        !serde_json::to_string(&record["context"])
            .unwrap()
            .contains(&scope_abs)
    );
    // The path token is masked in the message, not just dropped.
    assert!(record["message"].as_str().unwrap().contains("[redacted]"));
    assert_eq!(record["context"]["path"], "[redacted]");
}

/// P2: after `kio init` the `.kio` tree and the device data dir are owner-only
/// (0700) so document bytes / usage data are not world/group-readable.
#[cfg(unix)]
#[test]
fn p2_init_restricts_kio_and_data_dir_to_owner() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "secret-ish bytes").unwrap();
    kio(&dir, &["init"]).assert().success();

    let kio_mode = fs::metadata(dir.path().join(".kio"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(kio_mode, 0o700, ".kio must be 0700, got {kio_mode:o}");

    // register_scope runs during init and creates $XDG_DATA_HOME/kio.
    let data_kio = dir.path().join(".test-data/kio");
    let data_mode = fs::metadata(&data_kio).unwrap().permissions().mode() & 0o777;
    assert_eq!(data_mode, 0o700, "data dir must be 0700, got {data_mode:o}");
}

/// P3: a plaintext `plain:` credential is admitted only from an owner-private
/// `tools.toml` whose immediate parent is also owner-private.
#[cfg(unix)]
#[test]
fn p3_plain_auth_tools_toml_requires_private_file_and_parent() {
    use std::os::unix::fs::PermissionsExt;
    let temporary_root = fs::canonicalize(std::env::temp_dir()).unwrap();
    let dir = tempfile::tempdir_in(temporary_root).unwrap();
    fs::write(dir.path().join("a.txt"), "x").unwrap();
    kio(&dir, &["init"]).assert().success();

    let tools_dir = dir.path().join(".test-config/kio");
    fs::create_dir_all(&tools_dir).unwrap();
    fs::set_permissions(&tools_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let tools = tools_dir.join("tools.toml");
    fs::write(&tools, "[markdown]\nauth = \"plain:sk-secret-key\"\n").unwrap();

    // A credential-bearing file cannot be read through the private trust
    // boundary while it is group/world-readable.
    fs::set_permissions(&tools, fs::Permissions::from_mode(0o644)).unwrap();
    let error = json_failure(&dir, &["status"], 2);
    assert_eq!(error["error_code"], "KIO-E-ADAPTER-TOOLS-PRIVATE-001");

    // A 0600 leaf below a 0700 parent remains valid.
    fs::set_permissions(&tools, fs::Permissions::from_mode(0o600)).unwrap();
    kio(&dir, &["status"]).assert().success();
}

/// P5: a concurrent `kio search` during repeated `repair rebuild-db` must
/// never silently return exit 0 with 0 / partial results. The writer marks the
/// replica Rebuilding around its temp+rename source rebuild, so search either
/// observes a complete published replica or fails closed; it never reads the
/// transient source SQLite file.
#[test]
fn p5_concurrent_search_during_rebuild_is_never_silently_empty() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("a.md"),
        "# Alpha\n\n## S\nalphaunique alphaunique alphaunique\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("b.md"),
        "# Beta\n\n## T\nalphaunique betaword\n",
    )
    .unwrap();
    let data = dir.path().join(".test-data");
    let config = dir.path().join(".test-config");
    let cache = dir.path().join(".test-cache");
    let bin = assert_cmd::cargo::cargo_bin("kio");
    let root = dir.path().to_path_buf();

    let run = |args: &[&str]| -> std::process::Output {
        hermetic_process_command(&bin)
            .args(args)
            .current_dir(&root)
            .env("XDG_DATA_HOME", &data)
            .env("XDG_CONFIG_HOME", &config)
            .env("XDG_CACHE_HOME", &cache)
            .output()
            .unwrap()
    };
    assert!(run(&["init"]).status.success());
    assert!(run(&["index"]).status.success());
    let baseline = run(&["search", "alphaunique", "--mode", "text", "--json"]);
    assert!(baseline.status.success());
    let baseline: Value = serde_json::from_slice(&baseline.stdout).unwrap();
    let expected = baseline["results"].as_array().unwrap().len();
    assert!(expected > 0);

    let stop = Arc::new(AtomicBool::new(false));
    let handle = {
        let (bin, root) = (bin.clone(), root.clone());
        let (data, config, cache) = (data.clone(), config.clone(), cache.clone());
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut spins = 0;
            while !stop.load(Ordering::Relaxed) && spins < 200 {
                let _ = hermetic_process_command(&bin)
                    .args(["repair", "rebuild-db"])
                    .current_dir(&root)
                    .env("XDG_DATA_HOME", &data)
                    .env("XDG_CONFIG_HOME", &config)
                    .env("XDG_CACHE_HOME", &cache)
                    .output();
                spins += 1;
            }
        })
    };

    for _ in 0..60 {
        let out = run(&["search", "alphaunique", "--mode", "text", "--json"]);
        // Any exit-0 search must return the full result set — never a silent
        // empty/partial (the P5 false negative). Non-zero exits (REBUILDING /
        // transient) are tolerated by the contract.
        if out.status.success() {
            let json: Value = serde_json::from_slice(&out.stdout).unwrap();
            let n = json["results"].as_array().unwrap().len();
            assert_eq!(
                n, expected,
                "search returned {n} results during rebuild (expected {expected}): silent false negative"
            );
        }
    }
    stop.store(true, Ordering::Relaxed);
    handle.join().unwrap();
}

/// P7: `index --yes` confirms only this local scan; it cannot mint an adapter
/// approval or mutate the durable adapter-grant store.
#[test]
fn p7_index_yes_does_not_create_adapter_approval() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "hello").unwrap();
    kio(&dir, &["init"]).assert().success();
    let approvals = dir.path().join(".kio/approvals.jsonl");

    json_success(&dir, &["index", "--yes"]);
    assert!(
        !approvals.exists(),
        "a local index confirmation must not create adapter approvals"
    );

    for _ in 0..4 {
        json_success(&dir, &["index", "--yes"]);
    }
    assert!(
        !approvals.exists(),
        "repeated local index confirmations must not create adapter approvals"
    );
}

/// P8: the cursor signing key is created 0600 from the first byte (no 0644
/// window) — assert the resulting file is owner-only.
#[cfg(unix)]
#[test]
fn p8_cursor_key_is_created_0600() {
    use std::os::unix::fs::PermissionsExt;
    let dir = indexed_scope();
    // Page-1 search generates and signs a cursor with the device key.
    json_success(&dir, &["search", "認証仕様"]);
    let key = dir.path().join(".test-data/kio/cursor-key");
    assert!(key.is_file(), "cursor-key must exist after a search");
    let mode = fs::metadata(&key).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "cursor-key must be 0600, got {mode:o}");
}

/// P9: the open/view read-only expansion cache lives under $XDG_CACHE_HOME
/// (06 §1.1), not $XDG_DATA_HOME.
#[test]
fn p9_open_expansion_cache_is_under_xdg_cache_home() {
    let dir = indexed_scope();
    let search = json_success(&dir, &["search", "トークン TTL 3600"]);
    let uri = first_result(&search)["evidence_uri"]
        .as_str()
        .unwrap()
        .to_owned();
    // Remove the working-tree file so `open` must expand from the CAS.
    fs::remove_file(dir.path().join("auth.md")).unwrap();
    let opened = json_success(&dir, &["open", &uri]);
    assert_eq!(opened["temporary"], true);
    let path = opened["path"].as_str().unwrap();
    let expected_root = dir.path().join(".test-cache/kio/open");
    assert!(
        Path::new(path).starts_with(&expected_root),
        "expansion cache {path} must be under {}",
        expected_root.display()
    );
    assert!(Path::new(path).is_file());
    // It must NOT be under the data home any more.
    assert!(!Path::new(path).starts_with(dir.path().join(".test-data/kio/open")));
}

/// R9-3: the open/view expansion cache must be owner-only (dir 0700, file 0600 or
/// 0400) like the CAS it mirrors (P2), not world-readable at the umask default
/// (dir 0755 / file 0444). It materializes document bytes / images / pre-OCR raw
/// data, which must not be readable by group/other on a multi-user host.
#[cfg(unix)]
#[test]
fn r9_3_open_expansion_cache_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = indexed_scope();
    let search = json_success(&dir, &["search", "トークン TTL 3600"]);
    let uri = first_result(&search)["evidence_uri"]
        .as_str()
        .unwrap()
        .to_owned();
    // Remove the working-tree file so `open` must expand from the CAS.
    fs::remove_file(dir.path().join("auth.md")).unwrap();
    let opened = json_success(&dir, &["open", &uri]);
    let path = std::path::PathBuf::from(opened["path"].as_str().unwrap());

    // The cache file must be owner-only (created 0600, then 0400 readonly) — never
    // group/other-readable (pre-fix it was 0444).
    let file_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        file_mode & 0o077,
        0,
        "cache file must not be group/other-readable, got {file_mode:o}"
    );

    // Every dir from $XDG_CACHE_HOME/kio down to the leaf must be 0700 (pre-fix
    // they were 0755, exposing the whole subtree to traversal).
    let cache_root = dir.path().join(".test-cache/kio");
    let mut current = path.parent().unwrap().to_path_buf();
    loop {
        let mode = fs::metadata(&current).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode,
            0o700,
            "cache dir {} must be 0700, got {mode:o}",
            current.display()
        );
        if current == cache_root {
            break;
        }
        current = current.parent().unwrap().to_path_buf();
    }
}

/// R9-3 (reuse path): a cache dir/file left world-readable by an earlier (pre-fix)
/// build must be re-hardened on the next `open`, not served as-is.
#[cfg(unix)]
#[test]
fn r9_3_open_reuse_rehardens_stale_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let dir = indexed_scope();
    let search = json_success(&dir, &["search", "トークン TTL 3600"]);
    let uri = first_result(&search)["evidence_uri"]
        .as_str()
        .unwrap()
        .to_owned();
    fs::remove_file(dir.path().join("auth.md")).unwrap();
    let opened = json_success(&dir, &["open", &uri]);
    let path = std::path::PathBuf::from(opened["path"].as_str().unwrap());
    let leaf = path.parent().unwrap().to_path_buf();

    // Simulate a cache materialized by the pre-fix, world-readable code path.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
    fs::set_permissions(&leaf, fs::Permissions::from_mode(0o755)).unwrap();

    // A second open reuses the cache (M5) and must correct the permissions.
    let reopened = json_success(&dir, &["open", &uri]);
    assert_eq!(reopened["path"].as_str().unwrap(), path.to_str().unwrap());
    let file_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        file_mode & 0o077,
        0,
        "reused cache file must be re-hardened, got {file_mode:o}"
    );
    let dir_mode = fs::metadata(&leaf).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        dir_mode, 0o700,
        "reused cache dir must be re-hardened, got {dir_mode:o}"
    );
}

/// P10 (deterministic): once a writer moves the source HEAD before a replacement
/// replica projection exists, the replica header must fail closed.  Search must
/// not reopen the old per-scope sqlite to decide that it can still answer: that
/// would certify stale candidates.  A subsequent index writer publishes a complete
/// replica projection and makes the state searchable again.
#[test]
fn p10_replica_rebuilding_returns_not_silent_empty() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("a.md"),
        "# Alpha\n\n## S\nalphaunique alphaunique content here\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("b.md"),
        "# Beta\n\n## T\nalphaunique betaword more\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    json_success(&dir, &["index"]);

    let baseline = json_success(&dir, &["search", "alphaunique", "--mode", "text"]);
    let expected = baseline["results"].as_array().unwrap().len();
    assert!(expected > 0);

    // `snapshot` advances the source HEAD without rebuilding its derived index.
    // Its writer-side marker must make the previously Ready replica unavailable to
    // direct search until an index writer can publish a replacement projection.
    fs::write(
        dir.path().join("a.md"),
        "# Alpha revised\n\n## S\nalphaunique alphaunique replacement content here\n",
    )
    .unwrap();
    let snapshot = json_success(
        &dir,
        &["snapshot", "create", "--message", "advance source HEAD"],
    );
    assert_eq!(snapshot["status"], "created", "{snapshot}");
    let err = json_failure(&dir, &["search", "alphaunique", "--mode", "text"], 3);
    assert_eq!(err["error_code"], "KIO-E-INDEX-REBUILDING-001");
    // The offending scope is reported as a part-failure exclusion, not dropped.
    assert_eq!(
        err["context"]["excluded_scopes"][0]["reason"],
        "index_rebuilding"
    );

    // The state is transient: a complete writer projection recovers the full set.
    json_success(&dir, &["index"]);
    let recovered = json_success(&dir, &["search", "alphaunique", "--mode", "text"]);
    assert_eq!(recovered["results"].as_array().unwrap().len(), expected);
}

/// P10 false-positive guard: the `kio index` window must NOT be flagged REBUILDING.
/// `kio index` re-generates only the changed documents, so an unchanged document's
/// chunk stays live for the new HEAD — the index is serviceable, not rebuilding.
/// Reproduce that window (change one doc, re-index, restore the pre-change sqlite
/// with HEAD at the new commit) and assert a query for the unchanged doc still
/// returns its hit at exit 0, never a spurious REBUILDING.
#[test]
fn p10_partial_index_window_does_not_false_rebuilding() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("a.md"),
        "# Alpha\n\n## S\nalphaword alphaword\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("b.md"),
        "# Beta\n\n## T\nbetaword betaword betaword\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    json_success(&dir, &["index"]);

    let db = dir.path().join(".kio/index/sqlite.db");
    let backup = dir.path().join("sqlite_c0.db");
    fs::copy(&db, &backup).unwrap();

    // Change ONLY a.md and re-index: HEAD advances to a commit where a.md is a new
    // generation but b.md is unchanged.
    fs::write(
        dir.path().join("a.md"),
        "# Alpha\n\n## S\nalphaword alphaword changed newtext\n",
    )
    .unwrap();
    json_success(&dir, &["index"]);

    // Restore the pre-change sqlite while HEAD is the post-change commit. Unlike
    // reindex, b.md's chunk is still live for HEAD, so a query for the unchanged
    // doc must return its hit (exit 0) — never a spurious REBUILDING.
    fs::copy(&backup, &db).unwrap();
    let search = json_success(&dir, &["search", "betaword", "--mode", "text"]);
    assert!(
        !search["results"].as_array().unwrap().is_empty(),
        "unchanged doc must stay searchable in the index window (not REBUILDING)"
    );
    assert!(search["excluded_scopes"].as_array().unwrap().is_empty());
}

/// P10 non-regression: a genuine no-hit on a healthy index and a query on an empty
/// scope both stay exit-0 empty — the rebuilding detector must not fire when live
/// chunks exist (genuine miss) or when the scope has no tree_entries (empty scope).
#[test]
fn p10_genuine_no_hit_and_empty_scope_stay_exit_zero() {
    // Healthy indexed scope: a matching query has hits; a non-matching query is an
    // honest exit-0 empty page (live chunks exist -> fast-path returns not-rebuilding).
    let dir = indexed_scope();
    assert!(
        !json_success(&dir, &["search", "認証仕様", "--mode", "text"])["results"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let miss = json_success(&dir, &["search", "zzznotpresentquery", "--mode", "text"]);
    assert!(miss["results"].as_array().unwrap().is_empty());
    assert!(miss["excluded_scopes"].as_array().unwrap().is_empty());

    // Empty scope (no documents): exit-0 empty page (no tree_entries for HEAD).
    let empty = tempfile::tempdir().unwrap();
    kio(&empty, &["init"]).assert().success();
    kio(&empty, &["index"]).assert().success();
    let search = json_success(&empty, &["search", "anything", "--mode", "text"]);
    assert!(search["results"].as_array().unwrap().is_empty());
    assert!(search["excluded_scopes"].as_array().unwrap().is_empty());
}

/// P10 (concurrent, end-to-end): a `kio search` running while `reindex --force`
/// spins must never silently return exit 0 with an empty/partial page — every
/// exit-0 search returns the complete result set (old or new generation, both the
/// same content). Non-zero exits are the honest transient (REBUILDING, docs/05:564
/// — proven exactly by `p10_reindex_window_returns_rebuilding_not_silent_empty`)
/// and are tolerated. Mirrors the P5 concurrency harness but drives `reindex`,
/// which re-generates every document and so exposes the HEAD-vs-sqlite window P5's
/// atomic rebuild alone does not close.
#[test]
fn p10_concurrent_search_during_reindex_is_never_silently_empty() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let dir = tempfile::tempdir().unwrap();
    // Enough documents that the all-document re-generation + rebuild spans a window
    // a concurrent search can land in.
    for i in 0..12 {
        fs::write(
            dir.path().join(format!("doc{i}.md")),
            format!("# Doc {i}\n\n## S\nalphaunique alphaunique body {i} filler filler filler\n"),
        )
        .unwrap();
    }
    let data = dir.path().join(".test-data");
    let config = dir.path().join(".test-config");
    let cache = dir.path().join(".test-cache");
    let bin = assert_cmd::cargo::cargo_bin("kio");
    let root = dir.path().to_path_buf();

    // Hermetic adapter seams (no network); text search does not exercise them, but
    // set them so any accidental adapter call is a mock.
    let run = |args: &[&str]| -> std::process::Output {
        hermetic_process_command(&bin)
            .args(args)
            .current_dir(&root)
            .env("XDG_DATA_HOME", &data)
            .env("XDG_CONFIG_HOME", &config)
            .env("XDG_CACHE_HOME", &cache)
            .env(TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV, "mock")
            .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
            .output()
            .unwrap()
    };
    assert!(run(&["init"]).status.success());
    assert!(run(&["ledger", "init"]).status.success());
    assert!(run(&["index"]).status.success());
    let baseline = run(&["search", "alphaunique", "--mode", "text", "--json"]);
    assert!(baseline.status.success());
    let baseline: Value = serde_json::from_slice(&baseline.stdout).unwrap();
    let expected = baseline["results"].as_array().unwrap().len();
    assert!(expected > 0);

    let stop = Arc::new(AtomicBool::new(false));
    let handle = {
        let (bin, root) = (bin.clone(), root.clone());
        let (data, config, cache) = (data.clone(), config.clone(), cache.clone());
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut spins = 0;
            while !stop.load(Ordering::Relaxed) && spins < 50 {
                let _ = hermetic_process_command(&bin)
                    .args(["reindex", "--regenerate", "--yes"])
                    .current_dir(&root)
                    .env("XDG_DATA_HOME", &data)
                    .env("XDG_CONFIG_HOME", &config)
                    .env("XDG_CACHE_HOME", &cache)
                    .env(TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV, "mock")
                    .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
                    .output();
                spins += 1;
            }
        })
    };

    let mut saw_success = false;
    for _ in 0..150 {
        let out = run(&["search", "alphaunique", "--mode", "text", "--json"]);
        if out.status.success() {
            saw_success = true;
            let json: Value = serde_json::from_slice(&out.stdout).unwrap();
            let n = json["results"].as_array().unwrap().len();
            assert_eq!(
                n, expected,
                "search returned {n} results during reindex (expected {expected}): P10 silent false negative"
            );
        }
    }
    stop.store(true, Ordering::Relaxed);
    handle.join().unwrap();
    assert!(
        saw_success,
        "expected at least one exit-0 search across the concurrent run"
    );
}

// Q1 (full cycle): a torn trailing record in `chunks.jsonl` (crash / ENOSPC
// post-state, no trailing '\n') must fully self-heal. Skipping it on read alone
// welds the next append onto the torn bytes, producing a permanently-skipped
// malformed line that re-bricks `repair rebuild-db` on exit 4 and re-appends the
// same chunk forever. After the fix the torn tail is physically truncated before
// the append, so index -> repair -> index all exit 0 and the file stays valid with
// no duplicate chunk_id.
#[test]
fn q1_torn_chunk_tail_fully_self_heals_across_index_repair_index() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("auth.md"),
        "# 認証仕様\n\n## API Token\nトークン TTL は 3600 秒です。\n\n## Scopes\nスコープは read write admin です。\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("ranking.md"),
        "# 検索ランキング\n\n## RRF 融合\nRRF の定数 k=60 を使います。\n\n## MMR 多様化\nMMR の係数 lambda 0.7 で多様化します。\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    json_success(&dir, &["index", "--yes"]);

    let chunks_path = dir.path().join(".kio/index/chunks.jsonl");
    let bytes = fs::read(&chunks_path).unwrap();
    assert_eq!(
        bytes.last(),
        Some(&b'\n'),
        "a fresh index must be newline-terminated"
    );
    // Cut the final record in half: leaves the earlier records intact and a torn
    // tail with no trailing '\n' (== crash between two `write_all`s).
    let last_nl = bytes.iter().rposition(|&byte| byte == b'\n').unwrap();
    let prev_nl = bytes[..last_nl]
        .iter()
        .rposition(|&byte| byte == b'\n')
        .expect("need at least two records so the final one can be torn");
    let record_start = prev_nl + 1;
    let cut = record_start + (last_nl - record_start) / 2;
    fs::write(&chunks_path, &bytes[..cut]).unwrap();
    assert_ne!(
        fs::read(&chunks_path).unwrap().last(),
        Some(&b'\n'),
        "torn tail must not end in a newline"
    );

    // Full self-heal cycle. Every step must exit 0 — the middle `repair` was the
    // one that used to exit 4 (KIO-E-STORE-CORRUPT-001).
    json_success(&dir, &["index", "--yes"]);
    json_success(&dir, &["repair", "rebuild-db"]);
    json_success(&dir, &["index", "--yes"]);

    // chunks.jsonl is now fully valid with no duplicated chunk_id (no torn-line
    // skip -> re-append loop, no permanently-skipped welded line).
    let text = fs::read_to_string(&chunks_path).unwrap();
    let mut ids = std::collections::BTreeSet::new();
    let mut count = 0usize;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let value: Value = serde_json::from_str(line).unwrap_or_else(|err| {
            panic!("every chunks.jsonl line must be valid JSON ({err}): {line}")
        });
        // Publication rows are additive ledger events for an existing creation,
        // so they intentionally repeat its chunk id. This torn-tail check is
        // about duplicate creation records only.
        if value.get("event").is_some() {
            continue;
        }
        let id = value["chunk_id"].as_str().unwrap().to_owned();
        assert!(ids.insert(id), "chunk_id must not be duplicated: {line}");
        count += 1;
    }
    assert!(
        count >= 2,
        "regenerated chunks must be present, got {count}"
    );
}

// ---------------------------------------------------------------------------
// R6 exploratory audit fixes.
// ---------------------------------------------------------------------------

#[test]
fn r6_foreign_approval_rows_do_not_grant_online_embedding() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("note.md"),
        "# Note\nforeign approval probe\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    let other = tempfile::tempdir().unwrap();
    fs::write(other.path().join("other.md"), "# Other\napproval source\n").unwrap();
    kio(&other, &["init"]).assert().success();
    initialize_paid_ledger(&other);
    approve_embed(&other, "mock");
    let foreign_scope: Value =
        serde_json::from_slice(&fs::read(other.path().join(".kio/scope.json")).unwrap()).unwrap();
    let mut local_scope: Value =
        serde_json::from_slice(&fs::read(dir.path().join(".kio/scope.json")).unwrap()).unwrap();
    local_scope["approvals"] = foreign_scope["approvals"].clone();
    fs::write(
        dir.path().join(".kio/scope.json"),
        serde_json::to_vec_pretty(&local_scope).unwrap(),
    )
    .unwrap();

    let out = json_success_embed(&dir, "mock", &["index"]);
    assert_eq!(out["network_allowed"], false);
    assert_eq!(out["network_opt_in"], false);
    let status = json_success_embed(&dir, "mock", &["status"]);
    let embedding = tasks_of_type(&status, "embedding");
    assert!(
        !embedding.is_empty(),
        "embedding task should be enqueued: {status}"
    );
    assert!(
        embedding.iter().all(|task| {
            task["status"] == "pending" && task["fallback_reason"] == "network_opt_in_required"
        }),
        "foreign approvals must not execute embedding tasks: {status}"
    );
}

#[test]
fn r6_empty_approvals_file_does_not_satisfy_online_flag() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("note.md"), "# Note\nempty approval probe\n").unwrap();
    kio(&dir, &["init"]).assert().success();
    fs::write(dir.path().join(".kio/approvals.jsonl"), "").unwrap();

    let err = json_failure_embed(&dir, "mock", &["index", "--online"], 2);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-USAGE-001");
}

#[test]
fn r6_view_open_reject_extra_pointer_arguments() {
    let dir = indexed_scope();
    let search = json_success(&dir, &["search", "トークン TTL 3600"]);
    let uri = first_result(&search)["evidence_uri"].as_str().unwrap();

    let view_err = json_failure(&dir, &["view", uri, "EXTRA"], 2);
    assert_eq!(view_err["error_code"], "KIO-E-CONFIG-USAGE-001");
    let open_err = json_failure(&dir, &["open", uri, "--definitely-invalid"], 2);
    assert_eq!(open_err["error_code"], "KIO-E-CONFIG-USAGE-001");
}

#[test]
fn r6_reindex_rejects_force_at_and_extra_operands() {
    let dir = indexed_scope();
    // Step 4 decision #67: historical enrichment and generation-forcing are
    // mutually exclusive. The extra positional remains a usage error too.
    let at = json_failure(
        &dir,
        &["reindex", "--regenerate", "--yes", "--at", "HEAD"],
        2,
    );
    assert_eq!(at["error_code"], "KIO-E-CONFIG-USAGE-001");
    let extra = json_failure(&dir, &["reindex", "--regenerate", "--yes", "HEAD"], 2);
    assert_eq!(extra["error_code"], "KIO-E-CONFIG-USAGE-001");
}

#[test]
fn r6_inline_json_pointer_rejects_unsupported_schema_version() {
    let dir = indexed_scope();
    let search = json_success(&dir, &["search", "トークン TTL 3600"]);
    let mut pointer = first_result(&search)["evidence_pointer"].clone();
    pointer["schema_version"] = Value::from(999);

    let err = json_failure(&dir, &["view", &pointer.to_string()], 2);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-SCHEMA-001");
}

#[test]
fn r6_default_search_rereads_current_global_opt_out() {
    let data_home = tempfile::tempdir().unwrap();
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    fs::write(a.path().join("a.md"), "# A\nalphaonly optout leak\n").unwrap();
    fs::write(b.path().join("b.md"), "# B\nbetapublic\n").unwrap();
    json_success_path(a.path(), data_home.path(), &["init"]);
    json_success_path(b.path(), data_home.path(), &["init"]);
    json_success_path(a.path(), data_home.path(), &["index"]);
    json_success_path(b.path(), data_home.path(), &["index"]);
    fs::write(
        a.path().join(".kio/config.toml"),
        "[scope]\nparticipates_in_global_search = false\n",
    )
    .unwrap();

    let search = json_success_path(
        b.path(),
        data_home.path(),
        &["search", "alphaonly", "--mode", "text"],
    );
    assert!(
        search["results"].as_array().unwrap().is_empty(),
        "stale registry opt-in must not leak opted-out scope results: {search}"
    );
}

#[test]
fn r6_tool_lock_rejects_future_spec_version() {
    let dir = indexed_scope();
    let path = dir.path().join(".kio/tool-lock.json");
    let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["spec_version"] = Value::from(999);
    fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

    let err = json_failure(&dir, &["status"], 2);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-SCHEMA-001");
    assert!(
        err["message"]
            .as_str()
            .unwrap()
            .contains("unsupported tool-lock spec_version")
    );
}

#[test]
fn r6_corrupt_normalized_unit_is_store_corrupt_not_config_schema() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("note.md"),
        "# Note\nnormalized corruption searchable\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    json_success(&dir, &["index", "--yes"]);
    let unit_path = first_immutable_normalized_unit_object(&dir.path().join(".kio"));
    fs::write(&unit_path, r#"{"torn":"#).unwrap();

    // A historical manifest transitively authenticates this body. Corruption
    // therefore fails closed rather than using its mutable cache counterpart.
    let error = json_failure(&dir, &["repair", "rebuild-db"], 4);
    assert_eq!(error["error_code"], "KIO-E-STORE-CORRUPT-001");
}

// ---------------------------------------------------------------------------
// R7 exploratory audit fixes.
// ---------------------------------------------------------------------------

#[test]
fn r7_empty_secrets_approval_file_does_not_lift_tier_b_hold() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("api_secret.md"),
        "# Secret\nprobable secret body\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_embed(&dir, "mock");
    fs::write(dir.path().join(".kio/secrets-approved.jsonl"), "").unwrap();

    json_success_embed(&dir, "mock", &["index", "--online"]);
    let status = json_success_embed(&dir, "mock", &["status"]);
    assert!(
        status["quarantine"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| {
                entry["path"] == "api_secret.md"
                    && entry["reason"] == "secrets_tier_b"
                    && entry["approval_method"] == "hold"
            }),
        "empty secrets approval file must not mark Tier B as send-approved: {status}"
    );
    assert!(
        tasks_of_type(&status, "embedding").iter().any(|task| {
            task["input_path"] == "api_secret.md"
                && task["status"] == "paused"
                && task["fallback_reason"] == "secrets_tier_b_hold"
        }),
        "Tier B embedding must stay held without a scoped send-secrets row: {status}"
    );
}

#[test]
fn pc4_multiscope_query_embedding_sent_when_any_target_scope_opts_in() {
    // PC4 (05 §1.1 L46-48 / 07 §3 L224-226): the send consent gate is an OR
    // across participating scopes, not an AND — one approved scope is enough
    // to send the query embedding, and the resulting vector is then usable
    // against every profile-compatible participating scope (05 §1.8 "送信は
    // 1 回であり scope 別の再送信は発生しない"). This replaces the pre-PC4
    // AND-gate contract (a single unapproved scope no longer silently vetoes
    // sending for an already-approved sibling).
    let data_home = tempfile::tempdir().unwrap();
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    fs::write(a.path().join("a.md"), "# A\nsharedterm alpha\n").unwrap();
    fs::write(b.path().join("b.md"), "# B\nsharedterm beta\n").unwrap();
    run_embed_path(a.path(), data_home.path(), "mock", &["init"]);
    run_embed_path(b.path(), data_home.path(), "mock", &["init"]);
    run_embed_path(a.path(), data_home.path(), "mock", &["ledger", "init"]);
    run_embed_path(
        a.path(),
        data_home.path(),
        "mock",
        &["adapter", "approve", "gemini_embedding_2", "--yes"],
    );
    let a_effective = |when: &str| {
        let status = run_embed_path(
            a.path(),
            data_home.path(),
            "mock",
            &["adapter", "status", "gemini_embedding_2"],
        );
        assert!(
            status["effective"]
                .as_array()
                .is_some_and(|rows| rows.iter().any(|row| row["permitted"] == true)),
            "A's grant must remain effective {when}: {status}"
        );
    };
    a_effective("after A approval");
    run_embed_path(a.path(), data_home.path(), "mock", &["index", "--online"]);
    // B retains vectors from a prior approved send, but its grant was revoked.
    run_embed_path(
        b.path(),
        data_home.path(),
        "mock",
        &["adapter", "approve", "gemini_embedding_2", "--yes"],
    );
    a_effective("after B approval");
    run_embed_path(b.path(), data_home.path(), "mock", &["index", "--online"]);
    run_embed_path(
        b.path(),
        data_home.path(),
        "mock",
        &["adapter", "revoke", "gemini_embedding_2"],
    );
    a_effective("after B revocation");

    let trace = data_home.path().join("query.trace");
    let output = hermetic_kio_command()
        .current_dir(a.path())
        .env("XDG_CONFIG_HOME", data_home.path().join("config"))
        .env("XDG_DATA_HOME", data_home.path().join("data"))
        .env("XDG_CACHE_HOME", data_home.path().join("cache"))
        .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
        .env("KIO_TEST_QUERY_EMBED_TRACE", &trace)
        .args(["search", "sharedterm", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let search: Value = serde_json::from_slice(&output).unwrap();
    // A's persistent opt-in alone satisfies the OR gate, so auto resolves to
    // hybrid (not a text fallback) even though B never persistently opted in.
    assert_eq!(search["resolved_mode"], "hybrid");
    assert_eq!(search["fallback"], false);
    assert!(
        trace.exists(),
        "query embedding must be sent once at least one searched scope has opt-in"
    );
}

#[test]
fn r7_repair_rejects_unknown_flags_and_extra_operands() {
    let dir = indexed_scope();
    let unknown = json_failure(
        &dir,
        &["repair", "rebuild-db", "--definitely-invalid", "EXTRA"],
        2,
    );
    assert_eq!(unknown["error_code"], "KIO-E-CONFIG-USAGE-001");

    let verify = json_success(&dir, &["repair", "verify-objects"]);
    assert_eq!(verify["status"], "ok");
}

#[test]
fn r7_embedding_profile_change_requires_a_new_explicit_grant() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("doc.md"), "# Doc\nalpha profile flip\n").unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_embed(&dir, "mock");
    json_success_embed(&dir, "mock", &["index"]);

    // Model a retained foreign vector space by changing only rebuildable index
    // metadata; immutable source objects and their bytes remain untouched.
    let conn = index_db(&dir);
    assert!(
        conn.execute(
            "UPDATE embeddings SET profile_hash = ?1 WHERE target_type = 'chunk'",
            ["sha256:00000000000000000000000000000000000000000000000000000000foreign"],
        )
        .unwrap()
            > 0
    );
    drop(conn);
    json_success_embed(&dir, "mock", &["adapter", "revoke", "gemini_embedding_2"]);

    // The prior grant cannot authorize regeneration after the profile drift.
    json_success_embed(&dir, "mock", &["index"]);
    let pending = json_success_embed(&dir, "mock", &["search", "alpha"]);
    assert_eq!(pending["resolved_mode"], "text", "{pending}");
    assert_eq!(pending["fallback"], true, "{pending}");

    approve_embed(&dir, "mock");
    json_success_embed(&dir, "mock", &["index"]);
    let after = json_success_embed(&dir, "mock", &["search", "alpha"]);
    assert_eq!(after["resolved_mode"], "hybrid", "{after}");
    assert_eq!(after["fallback"], false, "{after}");
}

fn first_normalized_unit_json(root: &Path) -> std::path::PathBuf {
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        for entry in fs::read_dir(&path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) == Some("json")
                && path.file_name().and_then(|name| name.to_str()) != Some("manifest.json")
            {
                return path;
            }
        }
    }
    panic!("normalized unit json not found under {}", root.display());
}

// R17-2/R17-6: locate a normalized-instance `manifest.json` (one per document per gen).
fn first_manifest_json(root: &Path) -> std::path::PathBuf {
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        for entry in fs::read_dir(&path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.file_name().and_then(|name| name.to_str()) == Some("manifest.json") {
                return path;
            }
        }
    }
    panic!(
        "normalized instance manifest not found under {}",
        root.display()
    );
}

fn mutable_unit_path_for(
    root: &Path,
    raw_hash: &str,
    tool_profile_hash: &str,
    r#gen: u64,
    unit_ref: &str,
) -> std::path::PathBuf {
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        for entry in fs::read_dir(&path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.file_name().and_then(|name| name.to_str()) != Some("manifest.json") {
                continue;
            }
            let manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            if manifest["raw_hash"] == raw_hash
                && manifest["tool_profile_hash"] == tool_profile_hash
                && manifest["gen"] == r#gen
            {
                return path.parent().unwrap().join(format!("{unit_ref}.json"));
            }
        }
    }
    panic!("normalized cache instance not found for {raw_hash}/{tool_profile_hash}/g{gen}");
}

fn first_immutable_normalized_unit_object(kio_dir: &Path) -> std::path::PathBuf {
    let manifest_path = first_manifest_json(&kio_dir.join("objects/normalized_units"));
    let manifest: Value = serde_json::from_slice(&fs::read(manifest_path).unwrap()).unwrap();
    let unit_hash = manifest["units"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|unit| unit["unit_object_hash"].as_str())
        .expect("indexed manifest must pin a done immutable unit");
    ObjectStore::new(kio_dir)
        .content_path(ContentObjectKind::NormalizedUnit, unit_hash)
        .unwrap()
}

/// R9-5: a normalized gen dir polluted with crash/OS junk — a torn `.tmp-*` left
/// by a killed atomic writer and a `.DS_Store` — must not brick `reindex`. Before
/// the fix, `copy_normalized_instance_gen` read every non-manifest entry as a unit
/// and failed with KIO-E-STORE-CORRUPT-001 (exit 4), which `repair rebuild-db`
/// could not heal. After: junk is skipped, the orphan `.tmp-*` is GC'd from the
/// old gen dir, and reindex succeeds and the index still resolves the document.
#[test]
fn r9_5_reindex_survives_junk_in_gen_dir() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("doc.md"),
        "# Title\n\n## Section\nr9five unique body text here\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    json_success(&dir, &["index"]);

    let units_root = dir.path().join(".kio/objects/normalized_units");
    let gen_dir = gen_dir_under(&units_root).expect("a .g0 gen dir exists after index");
    let torn = gen_dir.join(".tmp-99999-0000abcd");
    fs::write(&torn, b"torn partial write, not json").unwrap();
    fs::write(gen_dir.join(".DS_Store"), b"\0\0mac junk").unwrap();

    // Before the fix this exited 4 (STORE-CORRUPT); now it succeeds.
    json_success(&dir, &["reindex", "--regenerate", "--yes"]);

    // The orphan temp was GC'd from the old gen dir (Q1-style self-heal).
    assert!(
        !torn.exists(),
        "orphan .tmp-* must be cleaned up by reindex"
    );
    // Search still resolves the document (index rebuilt cleanly).
    let search = json_success(&dir, &["search", "r9five", "--mode", "text"]);
    assert!(!search["results"].as_array().unwrap().is_empty());
}

/// A minimal fake PDF with one text-bearing page per string (mirrors the step2
/// helper). Used where a non-text-native fixture is needed so an online
/// markdownize task is enqueued (R9-2 gates text-native files out of online OCR).
fn fake_pdf(pages: &[&str]) -> String {
    let kids = (0..pages.len())
        .map(|index| format!("{} 0 R", index + 2))
        .collect::<Vec<_>>()
        .join(" ");
    let mut out = format!(
        "%PDF-1.4\n1 0 obj << /Type /Pages /Kids [{kids}] /Count {} >> endobj\n",
        pages.len()
    );
    for (index, page) in pages.iter().enumerate() {
        out.push_str(&format!(
            "{} 0 obj << /Type /Page /Parent 1 0 R >> stream\nBT ({page}) Tj ET\nendstream endobj\n",
            index + 2
        ));
    }
    out.push_str("%%EOF\n");
    out
}

fn gen_dir_under(units_root: &Path) -> Option<std::path::PathBuf> {
    let mut stack = vec![units_root.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.ends_with(".g0"))
                .unwrap_or(false)
            {
                return Some(path);
            }
            stack.push(path);
        }
    }
    None
}

// ===========================================================================
// R12-2: [adapter.policy] documented keys — schema accepts the 8 documented keys
// (was: 7 of 8 rejected -> scope/device brick), non-default UNIMPLEMENTED values
// are loudly rejected, redact_logs is actually wired, max_input_bytes gates input.
// ===========================================================================

// docs/07 §7 block (every key at its documented default) must let ALL commands run
// (exit 0) instead of bricking the scope with KIO-E-CONFIG-SCHEMA-001.
#[test]
fn r12_2_adapter_policy_full_default_block_all_commands_ok() {
    let dir = indexed_scope();
    fs::write(
        dir.path().join(".kio/config.toml"),
        "\
         [adapter.policy]\n\
         allow_network = false\n\
         allowed_scope = \".\"\n\
         max_input_bytes = 104857600\n\
         timeout_seconds = 300\n\
         redact_logs = true\n\
         store_request_body = false\n\
         store_response_body = false\n\
         require_command_confirmation = true\n",
    )
    .unwrap();
    // status and search both open the repo (schema + semantic validation) — both
    // succeed with the full default block present.
    let status = json_success(&dir, &["status"]);
    assert!(status.get("scope_path").is_some());
    let search = json_success(&dir, &["search", "トークン TTL"]);
    assert!(!search["results"].as_array().unwrap().is_empty());
}

// A non-default value for an UNIMPLEMENTED enforcement key is a loud
// KIO-E-CONFIG-NOT-IMPLEMENTED-001 (exit 1), not a silent accept.
#[test]
fn r12_2_allowed_scope_non_default_is_loud_rejected() {
    let dir = indexed_scope();
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[adapter.policy]\nallowed_scope = \"sub\"\n",
    )
    .unwrap();
    let err = json_failure(&dir, &["status"], 1);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-NOT-IMPLEMENTED-001");
}

#[test]
fn r12_2_store_request_body_true_is_loud_rejected() {
    let dir = indexed_scope();
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[adapter.policy]\nstore_request_body = true\n",
    )
    .unwrap();
    let err = json_failure(&dir, &["status"], 1);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-NOT-IMPLEMENTED-001");
}

#[test]
fn r12_2_timeout_seconds_non_default_is_loud_rejected() {
    let dir = indexed_scope();
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[adapter.policy]\ntimeout_seconds = 30\n",
    )
    .unwrap();
    let err = json_failure(&dir, &["status"], 1);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-NOT-IMPLEMENTED-001");
    // The documented default (300) is accepted.
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[adapter.policy]\ntimeout_seconds = 300\n",
    )
    .unwrap();
    json_success(&dir, &["status"]);
}

// A typo / unknown key under [adapter.policy] is a schema error (exit 2), distinct
// from the semantic NOT-IMPLEMENTED (exit 1) above.
#[test]
fn r12_2_unknown_policy_key_is_schema_error() {
    let dir = indexed_scope();
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[adapter.policy]\nallow_netwrok = false\n",
    )
    .unwrap();
    let err = json_failure(&dir, &["status"], 2);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-SCHEMA-001");
}

// redact_logs is finally reachable: user config `redact_logs = false` turns off the
// errors.jsonl path masking (was permanently pinned to redacted because the schema
// rejected the key before it could ever be read).
#[test]
fn r12_2_user_config_redact_logs_false_records_path() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "hello").unwrap();
    kio(&dir, &["init"]).assert().success();
    // Device-global user config (XDG_CONFIG_HOME/kio/config.toml).
    let user_cfg = dir.path().join(".test-config/kio");
    fs::create_dir_all(&user_cfg).unwrap();
    fs::write(
        user_cfg.join("config.toml"),
        "[adapter.policy]\nredact_logs = false\n",
    )
    .unwrap();
    // Same corrupt-tasks trigger as n3_errors_jsonl_redacts_path -> errors.jsonl.
    fs::write(dir.path().join(".kio/tasks.jsonl"), "{ not json\n").unwrap();
    json_failure(&dir, &["status"], 4);
    let errors = fs::read_to_string(dir.path().join(".test-data/kio/logs/errors.jsonl")).unwrap();
    let last: Value = serde_json::from_str(errors.lines().last().unwrap()).unwrap();
    assert_ne!(
        last["context"]["path"], "[redacted]",
        "redact_logs = false must record the real path: {last}"
    );
    assert!(
        last["context"]["path"]
            .as_str()
            .is_some_and(|path| path.contains("tasks.jsonl")),
        "path must be the real tasks.jsonl path: {last}"
    );
}

// max_input_bytes is a real input gate: a file larger than the cap is skipped for
// adapter processing (never normalized) but the index still succeeds.
#[test]
fn r12_2_max_input_bytes_gates_oversized_input() {
    let dir = tempfile::tempdir().unwrap();
    kio(&dir, &["init"]).assert().success();
    // Cap at 50 bytes; write a markdown file well over that.
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[adapter.policy]\nmax_input_bytes = 50\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("big.md"),
        "# Big\n\n## Section\nthis body is definitely longer than fifty bytes in total.\n",
    )
    .unwrap();
    let index = json_success(&dir, &["index"]);
    assert_eq!(index["skipped_oversized_files"], 1);
    assert_eq!(index["normalized_files"], 0);
}

// ===========================================================================
// R12-1: [search.rrf] / [search.diversify] / [markdownize.incremental] were
// documented + schema-valid but hardcoded at every call site (dead tuning knobs).
// ===========================================================================

/// A single-file scope whose file has 5 heading sections all sharing one token
/// (so one raw_hash, 5 chunks) — the fixture for diversify dedup behavior.
fn multi_chunk_scope() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("doc.md"),
        "# Doc\n\n\
         ## S1\nsharedtoken alpha section body text.\n\n\
         ## S2\nsharedtoken beta section body text.\n\n\
         ## S3\nsharedtoken gamma section body text.\n\n\
         ## S4\nsharedtoken delta section body text.\n\n\
         ## S5\nsharedtoken epsilon section body text.\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    json_success(&dir, &["index"]);
    dir
}

fn write_scope_config(dir: &TempDir, body: &str) {
    fs::write(dir.path().join(".kio/config.toml"), body).unwrap();
}

// strategy = "off" is a real no-op (no dedup); max_per_raw_hash caps the stream —
// both were dead before R12-1 (default MMR/3 applied regardless of config).
#[test]
fn r12_1_diversify_config_controls_dedup() {
    let dir = multi_chunk_scope();
    // PC49/PC50 (05 §1.8 L384-387): folder config.toml applies only for a
    // single, non-`--descendants` `--scope` — the bare default (multi-scope
    // enumeration) now uses the user (device) layer only. `--scope .` keeps
    // this test's folder-config premise valid under the new rule.

    // Default (no config): text-only -> MMR skipped -> max_per_raw_hash=3 cap.
    let default = json_success(&dir, &["search", "sharedtoken", "--scope", "."]);
    assert_eq!(default["results"].as_array().unwrap().len(), 3);

    // strategy = "off": diversification disabled entirely -> every matching chunk.
    write_scope_config(&dir, "[search.diversify]\nstrategy = \"off\"\n");
    let off = json_success(&dir, &["search", "sharedtoken", "--scope", "."]);
    assert!(
        off["results"].as_array().unwrap().len() >= 4,
        "off must return more than the default cap of 3: {}",
        off["results"].as_array().unwrap().len()
    );
    assert_eq!(off["diversify"]["strategy"], "off");

    // max_per_raw_hash = 1: cap the raw_hash to a single chunk.
    write_scope_config(&dir, "[search.diversify]\nmax_per_raw_hash = 1\n");
    let capped = json_success(&dir, &["search", "sharedtoken", "--scope", "."]);
    assert_eq!(capped["results"].as_array().unwrap().len(), 1);
}

// The cursor query_hash embeds the EFFECTIVE rrf/diversify (05 §1.8:280): changing
// [search.rrf] between pages invalidates an in-flight cursor instead of silently
// replaying a differently-ranked page.
#[test]
fn r12_1_query_hash_depends_on_rrf_config() {
    let dir = multi_chunk_scope();
    // PC49/PC50: `--scope .` (single scope) keeps folder config effective —
    // see `r12_1_diversify_config_controls_dedup`.
    let page1 = json_success(
        &dir,
        &["search", "sharedtoken", "--limit", "1", "--scope", "."],
    );
    let cursor = page1["paging"]["next_cursor"]
        .as_str()
        .expect("cursor present");
    // Same config -> the cursor replays fine (sanity).
    json_success(
        &dir,
        &[
            "search",
            "sharedtoken",
            "--limit",
            "1",
            "--cursor",
            cursor,
            "--scope",
            ".",
        ],
    );
    // Change the effective rrf -> query_hash changes -> the old cursor is rejected.
    write_scope_config(&dir, "[search.rrf]\nk = 1\n");
    let err = json_failure(
        &dir,
        &[
            "search",
            "sharedtoken",
            "--limit",
            "1",
            "--cursor",
            cursor,
            "--scope",
            ".",
        ],
        2,
    );
    assert_eq!(err["error_code"], "KIO-E-SEARCH-CURSOR-001");
}

// An unknown key under the now-typed [search.rrf] is a schema error (exit 2) — the
// [search] block is `additionalProperties: false` after R12-1 (typo detection).
#[test]
fn r12_1_unknown_search_rrf_key_is_schema_error() {
    let dir = multi_chunk_scope();
    write_scope_config(&dir, "[search.rrf]\nnonsense_key = 1\n");
    let err = json_failure(&dir, &["search", "sharedtoken"], 2);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-SCHEMA-001");
}

// [search.multi_scope] is a live execution setting; both documented keys are
// accepted and the hard worker ceiling rejects accidental oversubscription.
#[test]
fn r12_1_multi_scope_config_is_wired_and_bounded() {
    let dir = multi_chunk_scope();
    write_scope_config(
        &dir,
        "[search.multi_scope]\nparallelism = 4\nper_scope_timeout_seconds = 2\n",
    );
    json_success(&dir, &["search", "sharedtoken"]);

    write_scope_config(&dir, "[search.multi_scope]\nparallelism = 5\n");
    let error = json_failure(&dir, &["search", "sharedtoken"], 2);
    assert_eq!(error["error_code"], "KIO-E-CONFIG-SCHEMA-001");
}

// QA61 (step4b-contract-tests-p3a.md §R, arbitration #7): `include_neighbors`
// was removed from config.schema.json entirely (it dropped out of the
// documented config example with no implementation concept ever assigned,
// R12-1) — ANY value (including the old documented default, 1) is now an
// unknown-key schema error, superseding the old "1 is a no-op accept, other
// values are NOT-IMPLEMENTED" behavior this test used to cover.
#[test]
fn qa61_incremental_include_neighbors_key_is_removed_from_schema() {
    let dir = multi_chunk_scope();
    for value in [1, 2] {
        write_scope_config(
            &dir,
            &format!("[markdownize.incremental]\ninclude_neighbors = {value}\n"),
        );
        let err = json_failure(&dir, &["status"], 2);
        assert_eq!(err["error_code"], "KIO-E-CONFIG-SCHEMA-001");
    }
}

// ===========================================================================
// R12-3: R11-5's deferred embedding write-back opened a crash window — a chunk's
// chunk_vec commits per batch but the task Done transition is deferred to after the
// loop. A crash between leaves the chunk embedded yet its task stuck Pending
// forever (no recovery command reconciles it), so index_status reports phantom
// pending enrichment. The reconcile step on the shared enrichment path heals it.
// ===========================================================================
#[test]
fn r12_3_reconcile_completes_committed_embedding_tasks() {
    let dir = indexed_scope_embed("mock");
    let tasks_path = dir.path().join(".kio/tasks.jsonl");

    // Reproduce the crash window: chunk_vec/embeddings are committed (from the mock
    // index above) but the Done write-back was lost -> flip every Done embedding
    // task back to Pending.
    let original = fs::read_to_string(&tasks_path).unwrap();
    let mut flipped = String::new();
    let mut embedding_pending = 0u64;
    for line in original.lines() {
        let mut task: Value = serde_json::from_str(line).unwrap();
        if task["type"] == "embedding" && task["status"] == "done" {
            task["status"] = Value::from("pending");
            task["attempts"] = Value::from(0);
            task["next_retry_at"] = Value::Null;
            embedding_pending += 1;
        }
        flipped.push_str(&serde_json::to_string(&task).unwrap());
        flipped.push('\n');
    }
    assert!(embedding_pending > 0, "fixture must have embedding tasks");
    fs::write(&tasks_path, flipped).unwrap();

    // The phantom: index_status reports the stranded tasks as pending enrichment.
    let before = json_success_embed(
        &dir,
        "mock",
        &["search", "トークン TTL", "--mode", "hybrid"],
    );
    assert_eq!(
        before["index_status"]["pending_enrichment_tasks"], embedding_pending,
        "flipped tasks must read as pending before reconcile"
    );

    // A single recovery command (index) reconciles the accounting WITHOUT re-sending:
    // every chunk is already embedded, so `pending` is empty and nothing is executed.
    let reindex = json_success_embed(&dir, "mock", &["index"]);
    assert_eq!(
        reindex["embedding_tasks_executed"], 0,
        "reconcile must not re-send embeddings: {reindex}"
    );

    // Tasks converge to Done and index_status reports zero pending.
    let after = json_success_embed(
        &dir,
        "mock",
        &["search", "トークン TTL", "--mode", "hybrid"],
    );
    assert_eq!(after["index_status"]["pending_enrichment_tasks"], 0);
    let final_tasks = fs::read_to_string(&tasks_path).unwrap();
    for line in final_tasks.lines() {
        let task: Value = serde_json::from_str(line).unwrap();
        if task["type"] == "embedding" {
            assert_eq!(
                task["status"], "done",
                "every embedding task must reconcile to done: {task}"
            );
        }
    }
}

#[test]
fn r23_materialized_embedding_completes_failed_task_without_charge() {
    let dir = indexed_scope_embed("mock");
    let tasks_path = dir.path().join(".kio/tasks.jsonl");
    let original = fs::read_to_string(&tasks_path).unwrap();
    let mut rewritten = String::new();
    let mut changed_task_id = None;
    for line in original.lines() {
        let mut task: Value = serde_json::from_str(line).unwrap();
        if changed_task_id.is_none() && task["type"] == "embedding" && task["status"] == "done" {
            changed_task_id = task["task_id"].as_str().map(str::to_owned);
            task["status"] = Value::from("failed");
            task["fallback_reason"] = Value::from("contract_violation");
            task["attempts"] = Value::from(1);
        }
        rewritten.push_str(&serde_json::to_string(&task).unwrap());
        rewritten.push('\n');
    }
    let changed_task_id = changed_task_id.expect("fixture must have a done embedding task");
    fs::write(&tasks_path, rewritten).unwrap();
    let ledger_before = embedding_ledger_rows(&dir);

    let reindex = json_success_embed(&dir, "mock", &["index"]);
    assert_eq!(reindex["embedding_tasks_executed"], 0);
    assert_eq!(embedding_ledger_rows(&dir), ledger_before);

    let tasks = fs::read_to_string(&tasks_path).unwrap();
    let recovered = tasks
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|task| task["task_id"] == changed_task_id)
        .unwrap();
    assert_eq!(recovered["status"], "done");
    assert_eq!(recovered["fallback_reason"], "embedding_adapter_done");
}

// ===========================================================================
// R12-4: exit 3/5/6 __exit_code overrides and clap usage errors bypassed
// errors.jsonl, and a failed search dropped its per-search metrics.jsonl line —
// the whole failure surface was invisible to the observability logs.
// ===========================================================================

// Enrichment auth failure (exit 5 via __exit_code) must now reach errors.jsonl.
#[test]
fn r12_4_enrichment_auth_failure_reaches_errors_jsonl() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("note.md"),
        "# ノート\n\n## 本文\n認証失敗の可視化テスト本文です。\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_embed(&dir, "auth_error");
    let indexed = json_code_stdout_embed(&dir, "auth_error", 5, &["index"]);
    assert!(indexed["embedding_tasks_failed"].as_u64().unwrap() > 0);
    let errors = fs::read_to_string(dir.path().join(".test-data/kio/logs/errors.jsonl")).unwrap();
    assert!(
        errors.lines().any(|line| {
            serde_json::from_str::<Value>(line).unwrap()["code"]
                .as_str()
                .is_some_and(|code| code.contains("AUTH"))
        }),
        "the enrichment auth failure must reach errors.jsonl: {errors}"
    );
}

// Multi-scope partial failure (exit 3 via __exit_code) must record the exclusion.
#[cfg(unix)]
#[test]
fn r12_4_multi_scope_partial_records_exclusion_in_errors_jsonl() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let a = parent.path().join("a");
    let b = parent.path().join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(a.join("a.md"), "# A\n\n## Sec\nalphaunique token\n").unwrap();
    fs::write(b.join("b.md"), "# B\n\n## Sec\nbetaunique token\n").unwrap();
    json_success_path(&a, &data_home, &["init"]);
    json_success_path(&b, &data_home, &["init"]);
    json_success_path(&a, &data_home, &["index"]);
    json_success_path(&b, &data_home, &["index"]);
    let b_kio = b.join(".kio");
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(&b_kio).unwrap().permissions();
    perms.set_mode(0o000);
    fs::set_permissions(&b_kio, perms).unwrap();
    hermetic_kio_command()
        .current_dir(&a)
        .env("XDG_CONFIG_HOME", data_home.join("config"))
        .env("XDG_DATA_HOME", data_home.join("data"))
        .env("XDG_CACHE_HOME", data_home.join("cache"))
        .args(["search", "alphaunique", "--json"])
        .assert()
        .code(3);
    let mut restore = fs::metadata(&b_kio).unwrap().permissions();
    restore.set_mode(0o755);
    fs::set_permissions(&b_kio, restore).unwrap();
    let errors = fs::read_to_string(data_home.join("data/kio/logs/errors.jsonl")).unwrap();
    assert!(
        errors.lines().any(|line| {
            let value: Value = serde_json::from_str(line).unwrap();
            value["context"]["excluded_scopes"].is_array()
        }),
        "the multi-scope exclusion must reach errors.jsonl: {errors}"
    );
}

// A failed search still emits a per-search metrics.jsonl line (result_count 0 +
// error_code) so it is not silently dropped from the latency population, and the
// errors.jsonl line (main Err arm) is present too.
#[test]
fn r12_4_failed_search_writes_metrics_and_errors() {
    let dir = indexed_scope();
    json_failure(
        &dir,
        &["search", "トークン", "--cursor", "not-a-real-cursor"],
        2,
    );
    let metrics = fs::read_to_string(dir.path().join(".test-data/kio/logs/metrics.jsonl")).unwrap();
    assert!(
        metrics.lines().any(|line| {
            let value: Value = serde_json::from_str(line).unwrap();
            value["message"] == "search failed" && value["context"]["result_count"] == 0
        }),
        "a failed search must emit a metrics line: {metrics}"
    );
    let errors = fs::read_to_string(dir.path().join(".test-data/kio/logs/errors.jsonl")).unwrap();
    assert!(
        errors.lines().any(|line| {
            serde_json::from_str::<Value>(line).unwrap()["code"]
                .as_str()
                .is_some_and(|code| code.contains("CURSOR"))
        }),
        "a failed search must record errors.jsonl: {errors}"
    );
}

// A clap usage error belongs in the device-global errors.jsonl too (it bypassed
// run() entirely, exiting inside exit_from_clap_error).
#[test]
fn r12_4_clap_usage_error_reaches_errors_jsonl() {
    let dir = tempfile::tempdir().unwrap();
    kio(&dir, &["index", "--this-flag-does-not-exist"])
        .assert()
        .code(2);
    let errors_path = dir.path().join(".test-data/kio/logs/errors.jsonl");
    let errors = fs::read_to_string(&errors_path).unwrap();
    assert!(
        errors.lines().any(|line| {
            serde_json::from_str::<Value>(line).unwrap()["code"] == "KIO-E-CONFIG-USAGE-001"
        }),
        "a clap usage error must record errors.jsonl: {errors}"
    );
}

// ===========================================================================
// R12-5: a metrics.jsonl / access.jsonl append failure must NOT kill the search —
// observability logging must not destroy an already-computed result (device-global
// files would otherwise stop every scope's search on disk-full).
// ===========================================================================
#[cfg(unix)]
#[test]
fn r12_5_search_survives_unwritable_metrics_log() {
    let dir = indexed_scope();
    let metrics = dir.path().join(".test-data/kio/logs/metrics.jsonl");
    fs::create_dir_all(metrics.parent().unwrap()).unwrap();
    fs::write(&metrics, "").unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(&metrics).unwrap().permissions();
    perms.set_mode(0o444);
    fs::set_permissions(&metrics, perms).unwrap();
    // The search still succeeds with results (was exit 1 KIO-E-STORE-IO-001).
    let search = json_success(&dir, &["search", "トークン TTL"]);
    assert!(!search["results"].as_array().unwrap().is_empty());
    let mut restore = fs::metadata(&metrics).unwrap().permissions();
    restore.set_mode(0o644);
    fs::set_permissions(&metrics, restore).unwrap();
}

// ===========================================================================
// R12-7: the manual arg parsers (search/repair/reindex) rejected `--flag=value` as
// an unknown flag even though the flag exists, and `--limit 0` was silently clamped
// to 1 (faking success on a meaningless value).
// ===========================================================================
#[test]
fn r12_7_search_accepts_flag_equals_value_syntax() {
    let dir = indexed_scope();
    // `--limit=5` == `--limit 5`.
    let eq = json_success(&dir, &["search", "トークン TTL", "--limit=5"]);
    assert_eq!(eq["paging"]["limit"], 5);
    let space = json_success(&dir, &["search", "トークン TTL", "--limit", "5"]);
    assert_eq!(eq["paging"]["limit"], space["paging"]["limit"]);

    // `--offset=N` and `--scope=.` are accepted (were "unknown search flag").
    let off_eq = json_success(&dir, &["search", "トークン TTL", "--offset=1"]);
    let off_space = json_success(&dir, &["search", "トークン TTL", "--offset", "1"]);
    assert_eq!(off_eq["results"], off_space["results"]);
    let scoped = json_success(&dir, &["search", "トークン TTL", "--scope=."]);
    assert_eq!(scoped["searched_scopes"].as_array().unwrap().len(), 1);
}

// A positional query containing `=` must NOT be split into a flag.
#[test]
fn r12_7_positional_query_with_equals_is_preserved() {
    let dir = indexed_scope();
    let search = json_success(&dir, &["search", "ranking=test"]);
    assert_eq!(search["query"], "ranking=test");
}

// `--limit 0` (and `--limit=0`) is a usage error, not a silent clamp to 1.
#[test]
fn r12_7_limit_zero_is_a_usage_error() {
    let dir = indexed_scope();
    let err = json_failure(&dir, &["search", "トークン TTL", "--limit", "0"], 2);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-USAGE-001");
    let err_eq = json_failure(&dir, &["search", "トークン TTL", "--limit=0"], 2);
    assert_eq!(err_eq["error_code"], "KIO-E-CONFIG-USAGE-001");
    // The upper clamp (100) is unchanged: a large value still succeeds.
    let big = json_success(&dir, &["search", "トークン TTL", "--limit=500"]);
    assert_eq!(big["paging"]["limit"], 100);
}

/// The pre-stable config has one retention authority. The old `[logs]` spelling
/// is an unknown top-level key, not an alias for `[observability]`.
#[test]
fn logs_retention_days_is_rejected_in_scope_config() {
    let dir = indexed_scope();
    write_scope_config(&dir, "[logs]\nretention_days = 7\n");
    let error = json_failure(&dir, &["status"], 2);
    assert_eq!(error["error_code"], "KIO-E-CONFIG-SCHEMA-001");
}

#[test]
fn logs_retention_days_is_rejected_in_user_config() {
    let dir = indexed_scope();
    let user_cfg = dir.path().join(".test-config/kio");
    fs::create_dir_all(&user_cfg).unwrap();
    fs::write(user_cfg.join("config.toml"), "[logs]\nretention_days = 7\n").unwrap();
    let error = json_failure(&dir, &["status"], 2);
    assert_eq!(error["error_code"], "KIO-E-CONFIG-SCHEMA-001");
}

/// R13-3 (d) / R12-5: a log write that cannot land (here the device metrics path is
/// occupied by a directory, so both rotation and append fail) must NOT fail the
/// command body — the search result still returns exit 0.
#[test]
fn r13_3_unwritable_log_path_does_not_fail_the_search() {
    let dir = indexed_scope();
    let logs = dir.path().join(".test-data/kio/logs");
    fs::create_dir_all(&logs).unwrap();
    // Occupy metrics.jsonl with a directory so the append (and any rotation) fails.
    let metrics = logs.join("metrics.jsonl");
    if metrics.exists() {
        fs::remove_file(&metrics).ok();
    }
    fs::create_dir_all(&metrics).unwrap();
    // Search still succeeds despite the broken device log path.
    json_success(&dir, &["search", "トークン"]);
}

// ---------------------------------------------------------------------------
// Deterministic evaluator embedding adapter.
//
// This release-capable, network-free adapter exercises the production adapter
// wire without a model server. These tests pin its non-transmitting,
// non-billable posture plus the shared vector/image behavior.
// ---------------------------------------------------------------------------

fn json_success_deterministic_embed(dir: &TempDir, args: &[&str]) -> Value {
    let output = kio(dir, args)
        .env(TEST_DETERMINISTIC_EMBEDDING_ENV, "scale-v3")
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

/// A scope indexed entirely through the deterministic evaluator adapter.
fn indexed_scope_deterministic_embed() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("auth.md"),
        "# 認証仕様\n\n## API Token\nトークン TTL は 3600 秒です。\n\n## Scopes\nスコープは read write admin です。\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("ranking.md"),
        "# 検索ランキング\n\n## RRF 融合\nRRF の定数 k=60 を使います。\n\n## MMR 多様化\nMMR の係数 lambda 0.7 で多様化します。\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    json_success_deterministic_embed(&dir, &["index", "--yes"]);
    dir
}

/// A deterministic-library adapter transmits nothing, so vector search works
/// without an embedding approval or network grant.
#[test]
fn deterministic_embedding_serves_vector_search_without_any_approval() {
    let dir = indexed_scope_deterministic_embed();
    let search =
        json_success_deterministic_embed(&dir, &["search", "トークン", "--mode", "vector"]);
    assert_eq!(search["resolved_mode"], "vector", "{search}");
    assert_eq!(search["fallback"], false, "{search}");
    assert!(
        !search["results"].as_array().unwrap().is_empty(),
        "deterministic vector search must return hits: {search}"
    );

    // No embedding approval exists, because none was required. (markdownize is
    // still an online adapter and keeps its own row — the exemption is
    // per-adapter, not per-scope.)
    let scope: Value =
        serde_json::from_slice(&fs::read(dir.path().join(".kio/scope.json")).unwrap()).unwrap();
    let approvals = scope["approvals"].as_array().cloned().unwrap_or_default();
    assert!(
        !approvals
            .iter()
            .any(|row| row["tool_id"] == "kio_eval_deterministic_embedding"),
        "a deterministic embedding adapter must not publish an approvals[] row: {scope}"
    );
}

/// `--offline` forbids new transmission for one run. A deterministic adapter
/// has none to forbid, so the flag must not knock vector search down to text.
#[test]
fn offline_flag_does_not_degrade_the_deterministic_embedding_adapter() {
    let dir = indexed_scope_deterministic_embed();
    let search = json_success_deterministic_embed(
        &dir,
        &["search", "トークン", "--mode", "vector", "--offline"],
    );
    assert_eq!(search["resolved_mode"], "vector", "{search}");
    assert_eq!(search["fallback"], false, "{search}");
    assert!(
        !search["results"].as_array().unwrap().is_empty(),
        "{search}"
    );
}

/// The evaluator adapter declares no billable kinds, so no reservation is
/// taken and no charge is settled.
#[test]
fn deterministic_embedding_never_touches_the_cost_ledger() {
    let dir = indexed_scope_deterministic_embed();
    json_success_deterministic_embed(&dir, &["search", "トークン", "--mode", "vector"]);
    assert!(
        !dir.path()
            .join(".test-data/kio/cost-ledger.sqlite")
            .exists(),
        "local embedding and vector search must not create a ledger"
    );
}

/// A crash after a local vector is committed must converge without creating a
/// paid ledger or sending the already materialized chunk again.
#[test]
fn deterministic_embedding_reconciles_stranded_tasks_without_a_ledger() {
    use kio_pipeline::task::{TaskStatus, TaskStore, TaskType};

    let dir = indexed_scope_deterministic_embed();
    let store = TaskStore::new(dir.path().join(".kio").canonicalize().unwrap());
    let before = store.all().unwrap();
    assert!(
        before
            .iter()
            .any(|task| task.task_type == TaskType::Embedding)
    );
    store
        .update_matching(|task| {
            if task.task_type != TaskType::Embedding {
                return false;
            }
            assert_eq!(task.status, TaskStatus::Done);
            assert!(task.reservation_claim().is_none());
            task.status = TaskStatus::Pending;
            task.fallback_reason = Some("ready_for_online_adapter".to_owned());
            true
        })
        .unwrap();
    json_success_deterministic_embed(&dir, &["index", "--yes", "--offline"]);
    let after = store.all().unwrap();
    assert_eq!(after.len(), before.len());
    assert!(
        after
            .iter()
            .filter(|task| task.task_type == TaskType::Embedding)
            .all(|task| task.status == TaskStatus::Done && task.reservation_claim().is_none())
    );
    assert!(
        !dir.path()
            .join(".test-data/kio/cost-ledger.sqlite")
            .exists()
    );
}

/// The working tool-lock records the active execution posture even though
/// kind/mode are intentionally outside the canonical tool-lock identity.
#[test]
fn tool_lock_records_the_deterministic_execution_mode() {
    let dir = indexed_scope_deterministic_embed();
    let lock: Value =
        serde_json::from_slice(&fs::read(dir.path().join(".kio/tool-lock.json")).unwrap()).unwrap();
    let embedding = &lock["embedding"];
    assert_eq!(embedding["kind"], "deterministic_library", "{lock}");
    assert_eq!(embedding["mode"], "deterministic", "{lock}");
    assert_eq!(
        embedding["tool_id"], "kio_eval_deterministic_embedding",
        "{lock}"
    );
    // 03 §7: a different vector space, and the compat gate has only the hash
    // to notice that with.
    assert_ne!(
        embedding["profile_hash"],
        Value::Null,
        "the deterministic profile must be pinned: {lock}"
    );
}

/// The online seam must behave exactly as before. A second implementation
/// appearing is not allowed to change what `KIO_TEST_GEMINI_EMBED` means.
#[test]
fn the_online_adapter_still_records_online_provenance() {
    let dir = indexed_scope_embed("mock");
    let lock: Value =
        serde_json::from_slice(&fs::read(dir.path().join(".kio/tool-lock.json")).unwrap()).unwrap();
    assert_eq!(lock["embedding"]["kind"], "online_api", "{lock}");
    assert_eq!(lock["embedding"]["mode"], "online", "{lock}");
    assert_eq!(lock["embedding"]["tool_id"], "gemini_embedding_2", "{lock}");
}

/// A scope whose OCR path produced image objects, indexed through the
/// deterministic evaluator adapter (which declares `image_object`).
fn indexed_scope_deterministic_embed_with_images() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    // R9-2: image extraction only happens on the online OCR path, which
    // non-text-native files reach → PDF fixture.
    fs::write(
        dir.path().join("figures.pdf"),
        fake_pdf(&["figure page one"]),
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    // Embedding is local, but the fixture deliberately sends OCR through its
    // billed mock seam during `batch resume`.
    initialize_paid_ledger(&dir);
    json_success_deterministic_embed(&dir, &["index"]);
    approve_markdownize(&dir, "mock_link_image");
    kio(&dir, &["batch", "resume"])
        .env(TEST_DETERMINISTIC_EMBEDDING_ENV, "scale-v3")
        .env(TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV, "mock_link_image")
        .arg("--json")
        .assert()
        .success();
    // The chunk pass runs on the next index, once normalized bodies exist.
    json_success_deterministic_embed(&dir, &["index"]);
    dir
}

fn index_db(dir: &TempDir) -> rusqlite::Connection {
    kio_index::vec::ensure_registered();
    rusqlite::Connection::open(dir.path().join(".kio/index/sqlite.db")).unwrap()
}

fn count(conn: &rusqlite::Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

/// 04 §4.3: `embeddings.target_type` has admitted `'image'` since it was
/// written, but with no vec0 table there was nowhere to search one. The images
/// a chunk body references get embedded into `image_vec`.
#[test]
fn image_objects_referenced_by_chunks_are_embedded_into_image_vec() {
    let dir = indexed_scope_deterministic_embed_with_images();
    let conn = index_db(&dir);
    let images = count(&conn, "SELECT COUNT(*) FROM image_vec");
    assert!(
        images > 0,
        "an OCR'd corpus with image references must populate image_vec"
    );
    // U11: one vector space, so image rows are `multimodal` like every other
    // row — `EmbeddingModality::Image` would name a second space 03 §7 has not.
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM embeddings WHERE target_type = 'image' AND modality <> 'multimodal'"
        ),
        0
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM embeddings WHERE target_type = 'image'"
        ),
        images,
        "every image_vec row must have its backing embeddings row (04 §4.3)"
    );
}

/// The image pass runs before the chunk `pending.is_empty()` return. Restore a
/// missing image vector while every chunk already has a vector, then require the
/// writer (not a later search) to repopulate the replica image relation.
#[test]
fn image_embedding_enrichment_writes_through_the_empty_chunk_branch() {
    let dir = indexed_scope_deterministic_embed_with_images();
    let (source_images, chunks, chunk_vectors) = {
        let conn = index_db(&dir);
        let source_images = count(&conn, "SELECT COUNT(*) FROM image_vec");
        let chunks = count(&conn, "SELECT COUNT(*) FROM chunks");
        let chunk_vectors = count(&conn, "SELECT COUNT(*) FROM chunk_vec");
        conn.execute("DELETE FROM image_vec", []).unwrap();
        conn.execute("DELETE FROM embeddings WHERE target_type = 'image'", [])
            .unwrap();
        (source_images, chunks, chunk_vectors)
    };
    assert!(source_images > 0, "fixture must embed at least one image");
    assert_eq!(
        chunk_vectors, chunks,
        "the resumed pass must reach image enrichment with no chunk work left"
    );

    {
        let replica =
            rusqlite::Connection::open(dir.path().join(".test-cache/kio/aggregator.sqlite"))
                .unwrap();
        replica.execute("DELETE FROM agg_image_refs", []).unwrap();
        replica
            .execute("DELETE FROM agg_image_embeddings", [])
            .unwrap();
    }

    json_success_deterministic_embed(&dir, &["batch", "resume"]);
    let source_after = count(&index_db(&dir), "SELECT COUNT(*) FROM image_vec");
    let replica =
        rusqlite::Connection::open(dir.path().join(".test-cache/kio/aggregator.sqlite")).unwrap();
    let replica_images = count(&replica, "SELECT COUNT(*) FROM agg_image_embeddings");
    assert_eq!(
        source_after, source_images,
        "batch resume must restore the missing source image vector"
    );
    assert_eq!(
        replica_images, source_after,
        "image enrichment must refresh the device projection without a search"
    );
}

/// U9: images deduplicate by `image_hash`, not by the chunk `text_hash` the
/// chunk path groups on. Re-indexing must not re-embed what is already stored.
#[test]
fn image_embedding_is_idempotent_across_reindex() {
    let dir = indexed_scope_deterministic_embed_with_images();
    let before = count(&index_db(&dir), "SELECT COUNT(*) FROM image_vec");
    json_success_deterministic_embed(&dir, &["index"]);
    let after = count(&index_db(&dir), "SELECT COUNT(*) FROM image_vec");
    assert_eq!(
        before, after,
        "a second index must not duplicate image rows"
    );
}

/// The gate: the adopted online adapter reads only `EmbeddingItem::text`, so
/// handing it an image item would embed the empty string. It does not declare
/// `image_object`, and must therefore write no image vectors at all.
#[test]
fn the_online_adapter_embeds_no_images_because_it_declares_no_capability() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("figures.pdf"),
        fake_pdf(&["figure page one"]),
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_markdownize(&dir, "mock");
    approve_embed(&dir, "mock");
    approve_markdownize(&dir, "mock_link_image");
    json_success_embed(&dir, "mock", &["index"]);
    kio(&dir, &["batch", "resume"])
        .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
        .env(TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV, "mock_link_image")
        .arg("--json")
        .assert()
        .success();
    json_success_embed(&dir, "mock", &["index"]);
    let conn = index_db(&dir);
    // Chunks did embed — this proves the corpus reached the embedding path at
    // all, so the zero below is the gate and not an empty run.
    assert!(count(&conn, "SELECT COUNT(*) FROM chunk_vec") > 0);
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM image_vec"),
        0,
        "an adapter that reads only `text` must not be handed image items"
    );
}

/// 04 §4.3: `image_vec` is a derivation of the CAS, so `rebuild-db` must
/// reconstruct it — otherwise a rebuild silently costs the corpus image search.
#[test]
fn rebuild_db_restores_image_vectors_from_objects() {
    let dir = indexed_scope_deterministic_embed_with_images();
    let before = count(&index_db(&dir), "SELECT COUNT(*) FROM image_vec");
    assert!(before > 0);
    fs::remove_file(dir.path().join(".kio/index/sqlite.db")).unwrap();
    kio(&dir, &["repair", "rebuild-db"])
        .env(TEST_DETERMINISTIC_EMBEDDING_ENV, "scale-v3")
        .arg("--json")
        .assert()
        .success();
    assert_eq!(
        count(&index_db(&dir), "SELECT COUNT(*) FROM image_vec"),
        before,
        "rebuild-db must restore image_vec from objects/embeddings/"
    );
}

/// Every `result_type: "image"` row of a response, in rank order.
fn image_rows(search: &Value) -> Vec<&Value> {
    search["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|hit| hit["result_type"] == "image")
        .collect()
}

/// U4 (05 §1.7): an embedded image is returned as its own row — `result_type`
/// says what it is, `payload_uri` is what the Agent opens to get the bytes.
#[test]
fn an_embedded_image_is_returned_as_an_image_row_with_a_payload_uri() {
    let dir = indexed_scope_deterministic_embed_with_images();
    let search =
        json_success_deterministic_embed(&dir, &["search", "figure page one", "--limit", "20"]);
    let images = image_rows(&search);
    assert!(
        !images.is_empty(),
        "an indexed corpus with image vectors must be able to return one: {search}"
    );
    for image in &images {
        let payload = image["payload_uri"].as_str().unwrap_or_default();
        assert!(
            payload.starts_with("kio://") && payload.contains("/object/image/sha256:"),
            "payload_uri must be an image object URI: {image}"
        );
        // `snippet` means "the start of THIS row's chunk body", and this row is
        // not a chunk — carrying the citing chunk's text would make the field
        // mean different things on different rows (05 §1.7).
        assert!(image.get("snippet").is_none(), "{image}");
        // 05 §1.7 is explicit that the response passes URIs and never inlines
        // bytes — base64 would hit the Agent's context and cost directly.
        assert!(image.get("payload").is_none(), "{image}");
        assert!(image.get("image_base64").is_none(), "{image}");
        // `related_images[]` is defined as what a CHUNK body references; on an
        // image row it would re-list this row's own payload.
        assert!(image.get("related_images").is_none(), "{image}");
    }
    // The chunk rows are unchanged, and still carry the field Stage 1.5 added.
    let chunks = search["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|hit| hit["result_type"] == "chunk")
        .collect::<Vec<_>>();
    assert!(!chunks.is_empty(), "{search}");
    assert!(chunks.iter().all(|hit| hit.get("payload_uri").is_none()));
}

/// U4 / V6 (05 §1.7): an image row's `evidence_pointer` is a CHUNK's — an object
/// URI has no commit, tree or `path_at_commit`, so it supports neither
/// time-travel nor `evidence verify`. Anchoring to the referencing chunk is what
/// lets an image be handed over while the citation stays verifiable.
#[test]
fn an_image_row_carries_the_referencing_chunks_evidence_pointer() {
    let dir = indexed_scope_deterministic_embed_with_images();
    let search =
        json_success_deterministic_embed(&dir, &["search", "figure page one", "--limit", "20"]);
    let image = image_rows(&search).first().copied().cloned().unwrap();
    let pointer = &image["evidence_pointer"];
    for field in ["commit", "raw_hash", "chunk_hash", "path_at_commit"] {
        assert!(
            pointer[field].is_string(),
            "image row pointer must carry {field}: {image}"
        );
    }
    assert_eq!(pointer["chunk_hash"], image["chunk_hash"], "{image}");
    // The strongest form of "it is a chunk's pointer": the same key set a chunk
    // row's carries, not a shape of its own.
    let chunk_pointer = search["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|hit| hit["result_type"] == "chunk")
        .map(|hit| hit["evidence_pointer"].clone())
        .unwrap();
    let keys = |value: &Value| {
        value
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(keys(pointer), keys(&chunk_pointer), "{image}");

    // The claim the pointer makes is checkable, which is the entire reason it
    // is a chunk's and not the object URI.
    let uri = image["evidence_uri"].as_str().unwrap();
    let verified = json_success_deterministic_embed(&dir, &["evidence", "verify", uri]);
    assert_eq!(verified["status"], "alive", "{verified}");

    // V6: where several chunks cite one image, the pointer takes the lowest
    // `chunk_hash` in UTF-8 byte order. Compute the citing set from the chunk
    // rows' own `related_images[]` rather than restating the rule.
    let payload = image["payload_uri"].as_str().unwrap();
    let mut citing = search["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|hit| hit["result_type"] == "chunk")
        .filter(|hit| {
            hit["related_images"]
                .as_array()
                .is_some_and(|images| images.iter().any(|item| item["image_uri"] == payload))
        })
        .map(|hit| hit["chunk_hash"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    citing.sort();
    assert!(!citing.is_empty(), "{search}");
    assert_eq!(
        image["chunk_hash"].as_str().unwrap(),
        citing[0],
        "V6: the pointer must be the lowest-`chunk_hash` citing chunk"
    );
}

/// V6 (05 §1.7), on a corpus where the rule actually bites: several chunks cite
/// ONE image, and the pointer must take the lowest `chunk_hash` in UTF-8 byte
/// order.
///
/// The fixture is two single-page PDFs. The OCR mock derives an image's bytes
/// from the unit key, which is the page position — so two different documents'
/// first pages carry byte-identical figures, one CAS object, and two chunks
/// citing it. (Two pages of ONE document would not do: their unit keys differ,
/// so their images do too.)
///
/// The rejected alternative was SQLite rowid order. `index/sqlite.db` is a
/// rebuildable cache (04 §4.3), so a citation an Agent stored would point at a
/// different chunk after `repair rebuild-db` — hence the rebuild half of this
/// test, which is the whole reason the rule names `chunk_hash`.
#[test]
fn v6_the_pointer_is_the_lowest_chunk_hash_citing_the_image() {
    let dir = tempfile::tempdir().unwrap();
    // Different page text so the two files are different units; same page
    // POSITION so the mock gives them the same figure.
    fs::write(dir.path().join("alpha.pdf"), fake_pdf(&["figure alpha"])).unwrap();
    fs::write(dir.path().join("beta.pdf"), fake_pdf(&["figure beta"])).unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_markdownize(&dir, "mock_link_image");
    json_success_deterministic_embed(&dir, &["index"]);
    kio(&dir, &["batch", "resume"])
        .env(TEST_DETERMINISTIC_EMBEDDING_ENV, "scale-v3")
        .env(TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV, "mock_link_image")
        .arg("--json")
        .assert()
        .success();
    json_success_deterministic_embed(&dir, &["index"]);

    // The citing set, read from the index rather than restated from the rule.
    let conn = index_db(&dir);
    let mut stmt = conn
        .prepare("SELECT chunk_id, text FROM chunks ORDER BY chunk_id")
        .unwrap();
    let bodies = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .map(|row| row.unwrap())
        .collect::<Vec<_>>();
    let image_hash: String = conn
        .query_row("SELECT image_id FROM image_vec LIMIT 1", [], |row| {
            row.get(0)
        })
        .unwrap_or_else(|error| panic!("no image vector ({error}); bodies: {bodies:#?}"));
    let citing = bodies
        .iter()
        .filter(|(_, text)| text.contains(&image_hash))
        .map(|(chunk_id, _)| chunk_id.clone())
        .collect::<Vec<_>>();
    assert!(
        citing.len() >= 2,
        "the fixture must produce several citing chunks or this proves nothing; \
         got {citing:?} from {bodies:?}"
    );

    let search = json_success_deterministic_embed(
        &dir,
        &["search", "mock ocr figure", "--scope", ".", "--limit", "50"],
    );
    let image = image_rows(&search)
        .first()
        .copied()
        .cloned()
        .unwrap_or_else(|| panic!("no image row: {search}"));
    assert_eq!(
        image["chunk_hash"].as_str().unwrap(),
        citing[0],
        "V6: lowest chunk_hash in UTF-8 byte order"
    );

    // And it survives the cache being rebuilt, which rowid order would not.
    drop(stmt);
    drop(conn);
    fs::remove_file(dir.path().join(".kio/index/sqlite.db")).unwrap();
    kio(&dir, &["repair", "rebuild-db"])
        .env(TEST_DETERMINISTIC_EMBEDDING_ENV, "scale-v3")
        .arg("--json")
        .assert()
        .success();
    let after = json_success_deterministic_embed(
        &dir,
        &["search", "mock ocr figure", "--scope", ".", "--limit", "50"],
    );
    assert_eq!(
        image_rows(&after).first().unwrap()["chunk_hash"]
            .as_str()
            .unwrap(),
        citing[0],
        "a stored citation must survive `repair rebuild-db`"
    );
}

/// The read-side half of the capability gate: an adapter that writes no image
/// vectors returns no image rows. Without this, "no images came back" could
/// equally mean the search path silently dropped them.
#[test]
fn the_online_adapter_returns_no_image_rows_because_it_embeds_none() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("figures.pdf"),
        fake_pdf(&["figure page one"]),
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    json_success_embed(&dir, "mock", &["index"]);
    kio(&dir, &["batch", "resume"])
        .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
        .env(TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV, "mock_link_image")
        .arg("--json")
        .assert()
        .success();
    json_success_embed(&dir, "mock", &["index"]);
    let search = json_success_embed(
        &dir,
        "mock",
        &["search", "figure page one", "--limit", "20"],
    );
    assert!(
        !search["results"].as_array().unwrap().is_empty(),
        "{search}"
    );
    assert!(image_rows(&search).is_empty(), "{search}");
}

/// 05 §1.7: image rows come from the vector lane only. An image's own score is
/// its vector; ranking one by its citing chunk's text rank alone would return a
/// duplicate of that chunk under a different name. The figure stays reachable in
/// text mode through the chunk row's `related_images[]`, which is what Stage 1.5
/// added it for.
#[test]
fn a_text_mode_search_returns_no_image_rows_but_still_names_the_figures() {
    let dir = indexed_scope_deterministic_embed_with_images();
    let text = json_success_deterministic_embed(
        &dir,
        &[
            "search",
            "figure page one",
            "--mode",
            "text",
            "--limit",
            "20",
        ],
    );
    assert_eq!(text["resolved_mode"], "text", "{text}");
    assert!(!text["results"].as_array().unwrap().is_empty(), "{text}");
    assert!(image_rows(&text).is_empty(), "{text}");
    assert!(
        text["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|hit| hit.get("related_images").is_some()),
        "the figures must still be named on the chunk rows: {text}"
    );
}

/// U5 / §4.2 問題 A: an image is not in the FTS index, so RRF would add two
/// reciprocal terms for a chunk and one for an image and sink every figure
/// structurally. The image inherits the text-lane standing of the chunk that
/// cites it, so a hybrid search scores it on both lanes.
///
/// The evidence is comparative: the same corpus, the same query, `--mode vector`
/// (one lane, no inheritance possible) against `--mode hybrid`. The vector lane
/// is byte-identical between the two runs — same KNN, same `candidate_depth`,
/// same merge — so any score difference IS the inherited text term.
///
/// The query is chosen to land in the OCR body that carries the image reference
/// (`... mock ocr page:1 ![img-0](kio://…)`), not in the page text beside it, so
/// the citing chunk certainly has a text rank to be inherited.
#[test]
fn an_image_inherits_the_text_lane_standing_of_the_chunk_that_cites_it() {
    let dir = indexed_scope_deterministic_embed_with_images();
    let search = |mode: &str| {
        json_success_deterministic_embed(
            &dir,
            &["search", "mock ocr page", "--mode", mode, "--limit", "20"],
        )
    };
    let vector_only = search("vector");
    let hybrid = search("hybrid");
    let image_score = |search: &Value| -> f64 {
        image_rows(search)
            .first()
            .and_then(|hit| hit["score"].as_f64())
            .unwrap_or_default()
    };
    let one_lane = image_score(&vector_only);
    let two_lanes = image_score(&hybrid);
    assert!(one_lane > 0.0, "{vector_only}");
    assert!(
        two_lanes > one_lane,
        "an image whose citing chunk matched the text query must gain a text \
         term: hybrid {two_lanes} vs vector-only {one_lane}"
    );
}

/// U6 / §4.2 問題 B (05 §1.4): `max_per_raw_hash` counts image rows in the same
/// budget as chunk rows — no image quota, no image lane. The cap exists so one
/// document cannot occupy the top of the results, and a document does not become
/// acceptable by flooding with figures.
#[test]
fn images_and_chunks_share_one_max_per_raw_hash_budget() {
    let dir = indexed_scope_deterministic_embed_with_images();
    // PC49/PC50: folder config is only effective under a single-scope search.
    write_scope_config(&dir, "[search.diversify]\nmax_per_raw_hash = 10\n");
    let uncapped = json_success_deterministic_embed(
        &dir,
        &["search", "figure page one", "--scope", ".", "--limit", "20"],
    );
    assert!(
        !image_rows(&uncapped).is_empty(),
        "the cap must be tested against a pool that HAS an image row: {uncapped}"
    );

    write_scope_config(&dir, "[search.diversify]\nmax_per_raw_hash = 1\n");
    let capped = json_success_deterministic_embed(
        &dir,
        &["search", "figure page one", "--scope", ".", "--limit", "20"],
    );
    let by_raw = capped["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["evidence_pointer"]["raw_hash"].as_str().unwrap())
        .collect::<Vec<_>>();
    for raw_hash in &by_raw {
        assert_eq!(
            by_raw.iter().filter(|other| *other == raw_hash).count(),
            1,
            "one budget for both types, counted on evidence_pointer.raw_hash: {capped}"
        );
    }
    // And the cap is what produced that, not an empty corpus. Without the shared
    // budget an image row would sit alongside its citing chunk under the same
    // raw_hash and the count above would be 2.
    assert!(
        uncapped["results"].as_array().unwrap().len() > capped["results"].as_array().unwrap().len(),
        "capped {capped}\nuncapped {uncapped}"
    );
}

/// R13-2: write a user tools.toml under the test's XDG_CONFIG_HOME.
fn write_tools_toml(dir: &TempDir, body: &str) {
    let cfg = dir.path().join(".test-config/kio");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(cfg.join("tools.toml"), body).unwrap();
}

/// R13-2(a): a bogus key / type-mismatch in tools.toml is exit 2 (schema),
/// symmetric with config.toml — before, tools.toml silently accepted it at exit 0.
#[test]
fn r13_2_cli_tools_toml_bogus_key_and_type_are_exit_2() {
    let dir = indexed_scope();
    write_tools_toml(&dir, "[markdown]\ntotally_bogus_key = \"xyz\"\n");
    let err = json_failure(&dir, &["status"], 2);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-SCHEMA-001");

    write_tools_toml(&dir, "[markdown.x]\ncmd = 12345\n");
    let err = json_failure(&dir, &["status"], 2);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-SCHEMA-001");
}

/// CAND-038/039: executable targets unsupported by the built-in runtime fail closed.
#[test]
fn r13_2_cli_unsupported_documented_targets_are_rejected() {
    let dir = indexed_scope();
    write_tools_toml(
        &dir,
        "[markdown.mistral_ocr_markdownize]\n\
         kind = \"online_api\"\n\
         cmd = \"uvx kio-mistral-ocr-adapter\"\n\
         model = \"mistral-ocr-latest\"\n\
         profile_hash = \"sha256:...\"\n\
         capabilities = [\"ocr\", \"layout_detection\", \"table_extraction\"]\n\
         \n\
         [embedding.gemini_embedding_2]\n\
         auth = \"env:GEMINI_API_KEY\"\n",
    );
    let err = json_failure(&dir, &["status"], 2);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-SCHEMA-001");
}

/// CAND-038: a URL on an unknown/custom markdown target is never silently ignored.
#[test]
fn r13_2_cli_custom_url_is_rejected() {
    let dir = indexed_scope();
    write_tools_toml(&dir, "[markdown.x]\nurl = \"plain:\"\n");
    let err = json_failure(&dir, &["status"], 2);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-SCHEMA-001");
}

/// Unsupported auth schemes fail at the configuration boundary rather than
/// being accepted as a deferred adapter capability.
#[test]
fn r13_2_keychain_auth_is_a_schema_error() {
    let dir = indexed_scope();
    write_tools_toml(
        &dir,
        "[embedding.gemini_embedding_2]\nauth = \"keychain:login\"\n",
    );
    let error = json_failure(&dir, &["status"], 2);
    assert_eq!(error["error_code"], "KIO-E-CONFIG-SCHEMA-001");
}

// ---------------------------------------------------------------------------
// R16 exploratory audit fixes — store-corruption / shallow-consistency cluster.
// A *deleted* CAS object (KIO-E-STORE-NOT-FOUND-001) is the shallow class these
// tests place by hand (05 §2.2 has no Step 3 GC generator); a *corrupted* object
// (hash mismatch → KIO-E-STORE-CORRUPT-001) is a distinct class exercised by R16-2.
// ---------------------------------------------------------------------------

/// Read a scope's HEAD commit hash from its `.kio/HEAD`.
fn head_commit(kio_dir: &Path) -> String {
    fs::read_to_string(kio_dir.join("HEAD"))
        .unwrap()
        .trim()
        .to_owned()
}

// R16-1: a missing HEAD *commit* object (not merely its tree) is the same shallow
// corruption class R13-4/R15-4 defend against, but every `read_commit` call site was
// an unconditional `?`. Pure reads (status/log/search) must degrade to exit 0;
// writes (snapshot/index/reindex/repair) must reject with a clear COMMIT-SHALLOW;
// restoring the object heals everything. Before the fix all of these bricked exit 4
// on a raw KIO-E-STORE-NOT-FOUND-001.
// R17-1: `view`/`open` are the Evidence-*authenticity* entry point, NOT pure reads —
// a missing commit object there is rejected as EVIDENCE-POINTER-INVALID (exit 4), not
// degraded, because a forged/absent commit must not bypass the tree-membership + N5
// gen checks. See r17_1_* below; this test asserts the read-degrade + write-reject
// halves and the (now rejecting) view.
#[test]
fn r16_1_missing_commit_object_degrades_reads_and_rejects_writes() {
    let dir = indexed_scope();
    // Capture a valid Evidence pointer while the scope is healthy.
    let search0 = json_success(&dir, &["search", "トークン TTL 3600"]);
    let ptr = first_result(&search0)["evidence_pointer"].to_string();

    let kio_dir = dir.path().join(".kio");
    let head = head_commit(&kio_dir);
    let commit_path = object_path(&kio_dir, "commits", &head);
    let commit_bytes = fs::read(&commit_path).unwrap();

    // Delete the HEAD COMMIT object; its tree survives — exactly the R16-1 corruption.
    fs::remove_file(&commit_path).unwrap();

    // (a) pure reads degrade to exit 0, never a raw KIO-E-STORE-NOT-FOUND-001.
    let status = json_success(&dir, &["status"]);
    assert_eq!(
        status["head_shallow"], true,
        "status must flag the shallow HEAD: {status}"
    );
    let log = json_success(&dir, &["log"]);
    assert_eq!(
        log["truncated"], true,
        "log must flag truncation at the missing HEAD commit: {log}"
    );
    assert!(
        log["commits"].as_array().unwrap().is_empty(),
        "no commit is readable from the missing HEAD: {log}"
    );
    // search degrades via the cached tree_entries rows (ShallowCachedRows) — real
    // results, exit 0 — rather than dying or silently emptying (R16-1 × R16-3 seam).
    let search = json_success(&dir, &["search", "トークン TTL 3600"]);
    assert!(
        !search["results"].as_array().unwrap().is_empty(),
        "search must degrade to cached results, not brick: {search}"
    );
    // R17-1: view/open are the Evidence-authenticity gate, so a missing commit object
    // is REJECTED (EVIDENCE-POINTER-INVALID exit 4), not degraded like the pure reads
    // above — a missing/forged commit must not skip the tree-membership + N5 gen
    // checks. (Before R17-1 this returned commit_shallow:true exit 0.)
    let view_err = json_failure(&dir, &["view", &ptr], 4);
    assert_eq!(
        view_err["error_code"], "KIO-E-EVIDENCE-POINTER-INVALID-001",
        "view must reject a missing commit object, not degrade: {view_err}"
    );
    let open_err = json_failure(&dir, &["open", &ptr], 4);
    assert_eq!(
        open_err["error_code"], "KIO-E-EVIDENCE-POINTER-INVALID-001",
        "open must reject a missing commit object, not degrade: {open_err}"
    );

    // (b) writes reject with COMMIT-SHALLOW (exit 1), not a raw STORE-NOT-FOUND. Edit a
    // file first so `index`'s unchanged-scope short-circuit still reaches the snapshot.
    fs::write(
        dir.path().join("auth.md"),
        "# 認証仕様\n\n## API\n更新された本文です。\n",
    )
    .unwrap();
    for (args, label) in [
        (vec!["snapshot", "create", "-m", "x"], "snapshot"),
        (vec!["index", "--yes"], "index"),
        (vec!["reindex", "--regenerate", "--yes"], "reindex"),
        (vec!["repair", "rebuild-db"], "repair"),
    ] {
        let err = json_failure(&dir, &args, 1);
        assert_eq!(
            err["error_code"], "KIO-E-COMMIT-SHALLOW-001",
            "{label} must reject the missing commit with COMMIT-SHALLOW: {err}"
        );
    }

    // (c) restore the commit object → reads are healthy again.
    fs::write(&commit_path, &commit_bytes).unwrap();
    let status2 = json_success(&dir, &["status"]);
    assert_eq!(
        status2["head_shallow"], false,
        "restore must clear the shallow flag: {status2}"
    );
    let log2 = json_success(&dir, &["log"]);
    assert_eq!(log2["truncated"], false);
    assert!(!log2["commits"].as_array().unwrap().is_empty());
}

// R17-1: `view`/`open` are the Evidence-authenticity entry point. A pointer whose
// `commit` is a FORGED hash (a well-formed sha256 naming no object) must be rejected
// as EVIDENCE-POINTER-INVALID (exit 4), NOT resolved best-effort as if it were a
// shallow commit — otherwise the tree-membership + N5 gen checks are both skipped and
// forged evidence resolves. A genuine shallow commit is also rejected: its receipt
// proves deletion but cannot bind a pointer to a historical tree entry or generation.
#[test]
fn r17_1_forged_and_true_shallow_commit_pointers_are_rejected() {
    // (a) forged commit hash → view/open reject.
    let dir = indexed_scope();
    let search = json_success(&dir, &["search", "トークン TTL 3600"]);
    let genuine = first_result(&search)["evidence_pointer"].clone();
    // Control: the untampered pointer resolves, non-shallow.
    let ok = json_success(&dir, &["view", &genuine.to_string()]);
    assert_eq!(ok["commit_shallow"], false, "control must resolve: {ok}");

    let forged_commit = format!("sha256:{}", "0".repeat(64));
    let mut forged = genuine.clone();
    forged["commit"] = serde_json::json!(forged_commit);
    let view_err = json_failure(&dir, &["view", &forged.to_string()], 4);
    assert_eq!(
        view_err["error_code"], "KIO-E-EVIDENCE-POINTER-INVALID-001",
        "a forged commit hash must be rejected by view: {view_err}"
    );
    let open_err = json_failure(&dir, &["open", &forged.to_string()], 4);
    assert_eq!(
        open_err["error_code"], "KIO-E-EVIDENCE-POINTER-INVALID-001",
        "a forged commit hash must be rejected by open: {open_err}"
    );

    // (b) genuine shallow commit (tree object GC'd, commit present) is rejected.
    // Fresh scope so (a) cannot mask the distinct final-receipt precondition.
    let dir2 = indexed_scope();
    let search2 = json_success(&dir2, &["search", "トークン TTL 3600"]);
    let pointer2 = first_result(&search2)["evidence_pointer"].clone();
    let kio_dir2 = dir2.path().join(".kio");
    let commit2 = pointer2["commit"].as_str().unwrap();
    let commit_obj: Value =
        serde_json::from_slice(&fs::read(object_path(&kio_dir2, "commits", commit2)).unwrap())
            .unwrap();
    let tree2 = commit_obj["tree"].as_str().unwrap();
    fs::write(
        dir2.path().join("advance.md"),
        "# Advance\n\nfixture head\n",
    )
    .unwrap();
    json_success(&dir2, &["index", "--yes"]);
    let receipt_path = kio_dir2
        .join("gc/shallowed")
        .join(commit2.strip_prefix("sha256:").unwrap());
    fs::create_dir_all(receipt_path.parent().unwrap()).unwrap();
    let receipt = ShallowReceipt::new(
        commit2.to_owned(),
        tree2.to_owned(),
        "2026-08-14T00:00:00Z".into(),
    )
    .unwrap();
    fs::write(receipt_path, receipt.canonical_bytes().unwrap()).unwrap();
    fs::remove_file(object_path(&kio_dir2, "trees", tree2)).unwrap();
    for command in ["view", "open"] {
        let error = json_failure(&dir2, &[command, &pointer2.to_string()], 1);
        assert_eq!(
            error["error_code"], "KIO-E-COMMIT-SHALLOW-001",
            "{command}: {error}"
        );
    }
}

// R17-1 / N5 contrast (the core harm): after `reindex --force` advances the
// normalization generation, a pointer that keeps a REAL old commit but splices in a
// gen-1 chunk_hash is rejected by the N5 gen binding (Attack A). Attack B — the SAME
// gen-1 chunk under a FORGED commit — must be rejected TOO (identically), instead of
// resolving best-effort with the gen check skipped. The only difference between the
// two attacks is the truthful-vs-forged commit field.
#[test]
fn r17_1_n5_forged_commit_cannot_bypass_generation_binding() {
    let dir = indexed_scope();
    let before = json_success(&dir, &["search", "トークン TTL 3600"]);
    let old_pointer = first_result(&before)["evidence_pointer"].clone();
    json_success(&dir, &["reindex", "--regenerate", "--yes"]);
    let after = json_success(&dir, &["search", "トークン TTL 3600"]);
    let new_chunk_hash = first_result(&after)["evidence_pointer"]["chunk_hash"].clone();
    assert_ne!(
        old_pointer["chunk_hash"], new_chunk_hash,
        "reindex --force must produce a new-generation chunk_hash"
    );

    // Attack A: real old commit + gen-1 chunk → rejected by the N5 gen binding.
    let mut attack_a = old_pointer.clone();
    attack_a["chunk_hash"] = new_chunk_hash;
    let err_a = json_failure(&dir, &["view", &attack_a.to_string()], 4);
    assert_eq!(
        err_a["error_code"], "KIO-E-EVIDENCE-POINTER-INVALID-001",
        "old commit + new-gen chunk must be rejected (N5): {err_a}"
    );

    // Attack B: forged commit + gen-1 chunk → BEFORE R17-1 this resolved exit 0
    // (commit_shallow:true), because the missing commit collapsed onto the shallow
    // path and skipped the gen binding. Now it is rejected identically to Attack A.
    let mut attack_b = attack_a.clone();
    attack_b["commit"] = serde_json::json!(format!("sha256:{}", "0".repeat(64)));
    let err_b = json_failure(&dir, &["view", &attack_b.to_string()], 4);
    assert_eq!(
        err_b["error_code"], "KIO-E-EVIDENCE-POINTER-INVALID-001",
        "forged commit + new-gen chunk must NOT bypass the gen binding (R17-1): {err_b}"
    );
}

// R16-1: `log` truncates at a missing ANCESTOR commit and returns the healthy prefix
// from HEAD (Sonnet-B's repro: a missing root commit must not swallow the healthy
// recent commits too). Reads that only need HEAD stay fully healthy.
#[test]
fn r16_1_log_truncates_at_missing_ancestor_commit() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("d.md"), "# D\n\n## S\nv1 body\n").unwrap();
    kio(&dir, &["init"]).assert().success();
    let c1 = json_success(&dir, &["snapshot", "create", "-m", "first"]);
    let c1_hash = c1["commit_hash"].as_str().unwrap().to_owned();
    fs::write(dir.path().join("d.md"), "# D\n\n## S\nv2 body\n").unwrap();
    let c2 = json_success(&dir, &["snapshot", "create", "-m", "second"]);
    let c2_hash = c2["commit_hash"].as_str().unwrap().to_owned();

    // (control) a healthy log returns both commits, HEAD-first, not truncated.
    let full = json_success(&dir, &["log"]);
    assert_eq!(full["truncated"], false);
    assert_eq!(full["commits"].as_array().unwrap().len(), 2);

    // Delete the ROOT commit object (c1); HEAD (c2) survives.
    let kio_dir = dir.path().join(".kio");
    fs::remove_file(object_path(&kio_dir, "commits", &c1_hash)).unwrap();

    let log = json_success(&dir, &["log"]);
    assert_eq!(
        log["truncated"], true,
        "a missing ancestor must flag truncation: {log}"
    );
    let commits = log["commits"].as_array().unwrap();
    assert_eq!(
        commits.len(),
        1,
        "the healthy HEAD prefix must still return: {log}"
    );
    assert_eq!(commits[0]["commit_hash"], c2_hash);
    // status only needs HEAD (c2), which is present → not shallow.
    let status = json_success(&dir, &["status"]);
    assert_eq!(
        status["head_shallow"], false,
        "HEAD present → status is not shallow: {status}"
    );
}

// R16-2: a store-corruption failure in ONE scope must not abort a multi-scope search
// and discard the healthy scopes' already-collected results (05 §1.8 per-scope
// isolation). A CORRUPTED (hash-mismatch) HEAD commit object in scope B surfaces a
// KIO-E-STORE-CORRUPT-001 Fatal (a class NOT absorbed as shallow); the loop downgrades
// it to Excluded("store_corrupt") so scope A still returns → exit 3. Before the fix the
// whole search died exit 4.
#[test]
fn r16_2_one_scope_store_corruption_is_partial_not_all_failed() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let a = parent.path().join("a");
    let b = parent.path().join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(a.join("a.md"), "# A\n\n## Sec\nalphaunique survivor\n").unwrap();
    fs::write(b.join("b.md"), "# B\n\n## Sec\nbetaunique other\n").unwrap();
    json_success_path(&a, &data_home, &["init"]);
    json_success_path(&b, &data_home, &["init"]);
    json_success_path(&a, &data_home, &["index"]);
    json_success_path(&b, &data_home, &["index"]);

    // (control) both scopes healthy → the search reaches both.
    let healthy = json_success_path(&a, &data_home, &["search", "alphaunique", "--all-scopes"]);
    assert_eq!(healthy["searched_scopes"].as_array().unwrap().len(), 2);

    // Corrupt scope B's HEAD commit object: garbage bytes → content hash mismatch →
    // read_by_hash returns KIO-E-STORE-CORRUPT-001 (distinct from the shallow /
    // STORE-NOT-FOUND class, so only R16-2's Fatal downgrade can catch it).
    let b_kio = b.join(".kio");
    let b_head = head_commit(&b_kio);
    fs::write(
        object_path(&b_kio, "commits", &b_head),
        b"this is not a valid commit object",
    )
    .unwrap();

    let output = hermetic_kio_command()
        .current_dir(&a)
        .env("XDG_CONFIG_HOME", data_home.join("config"))
        .env("XDG_DATA_HOME", data_home.join("data"))
        .env("XDG_CACHE_HOME", data_home.join("cache"))
        .args(["search", "alphaunique", "--all-scopes", "--json"])
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let search: Value = serde_json::from_slice(&output).unwrap();
    assert!(
        !search["results"].as_array().unwrap().is_empty(),
        "healthy scope A must still return results: {search}"
    );
    assert_eq!(search["searched_scopes"].as_array().unwrap().len(), 1);
    let excluded = search["excluded_scopes"].as_array().unwrap();
    assert_eq!(excluded.len(), 1);
    assert_eq!(excluded[0]["reason"], "store_corrupt");
    // R23-20 (03 §4 L296): scope_path is the canonical `.kio` directory.
    assert!(value_path_ends_with(&excluded[0]["scope_path"], "b/.kio"));
}

// R16-3: a fresh search against a scope whose current tree disappeared without
// a GC receipt must exclude that scope as store corruption, never silently place
// it in searched_scopes with an empty result set.
#[test]
fn r16_3_fresh_search_unreceipted_tree_loss_excludes_not_silent_empty() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let a = parent.path().join("a");
    let b = parent.path().join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(a.join("a.md"), "# A\n\n## Sec\nalphaunique survivor\n").unwrap();
    fs::write(b.join("b.md"), "# B\n\n## Sec\nbetashared token\n").unwrap();
    json_success_path(&a, &data_home, &["init"]);
    json_success_path(&b, &data_home, &["init"]);
    json_success_path(&a, &data_home, &["index"]);
    json_success_path(&b, &data_home, &["index"]);

    // Advance scope B's HEAD with a manual snapshot — the new commit's tree_entries are
    // NOT projected (only index/reindex project) — then discard its tree object. B's
    // HEAD is now shallow with NO cached rows for the new commit (ShallowNoRows).
    fs::write(b.join("b.md"), "# B\n\n## Sec\nbetashared token v2\n").unwrap();
    let snap = json_success_path(&b, &data_home, &["snapshot", "create", "-m", "advance"]);
    let b2 = snap["commit_hash"].as_str().unwrap().to_owned();
    let b_kio = b.join(".kio");
    let b2_obj: Value =
        serde_json::from_slice(&fs::read(object_path(&b_kio, "commits", &b2)).unwrap()).unwrap();
    fs::remove_file(object_path(
        &b_kio,
        "trees",
        b2_obj["tree"].as_str().unwrap(),
    ))
    .unwrap();

    let output = hermetic_kio_command()
        .current_dir(&a)
        .env("XDG_CONFIG_HOME", data_home.join("config"))
        .env("XDG_DATA_HOME", data_home.join("data"))
        .env("XDG_CACHE_HOME", data_home.join("cache"))
        .args(["search", "alphaunique", "--all-scopes", "--json"])
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let search: Value = serde_json::from_slice(&output).unwrap();
    assert!(
        !search["results"].as_array().unwrap().is_empty(),
        "healthy scope A must still return results: {search}"
    );
    // Scope B is EXCLUDED loudly, not silently searched-with-empty-results.
    assert_eq!(search["searched_scopes"].as_array().unwrap().len(), 1);
    let excluded = search["excluded_scopes"].as_array().unwrap();
    assert_eq!(excluded.len(), 1);
    assert_eq!(excluded[0]["reason"], "store_corrupt");
    // R23-20 (03 §4 L296): scope_path is the canonical `.kio` directory.
    assert!(value_path_ends_with(&excluded[0]["scope_path"], "b/.kio"));
}

// R16-4(a): a missing HEAD tree without a canonical receipt is corruption.
// `repair rebuild-db` must not relabel or auto-repair that impossible GC state.
#[test]
fn r16_4_repair_on_unreceipted_missing_head_reports_corruption() {
    let dir = indexed_scope();
    let kio_dir = dir.path().join(".kio");
    let head = head_commit(&kio_dir);
    let commit_obj: Value =
        serde_json::from_slice(&fs::read(object_path(&kio_dir, "commits", &head)).unwrap())
            .unwrap();
    fs::remove_file(object_path(
        &kio_dir,
        "trees",
        commit_obj["tree"].as_str().unwrap(),
    ))
    .unwrap();

    let err = json_failure(&dir, &["repair", "rebuild-db"], 4);
    assert_eq!(
        err["error_code"], "KIO-E-STORE-CORRUPT-001",
        "repair must fail closed on a receiptless missing HEAD tree: {err}"
    );
}

// An absent immutable normalized-unit CAS object is a broken historical closure.
// It must fail closed rather than let repair substitute mutable cache content.
#[test]
fn r16_4_repair_skips_missing_unit_and_rebuilds_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("keep.md"),
        "# Keep\n\n## Body\nkeepsurvivor token here\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("drop.md"),
        "# Drop\n\n## Body\ndropskipped token here\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    json_success(&dir, &["index", "--yes"]);

    // Delete one immutable normalized-unit CAS object.
    let unit_path = first_immutable_normalized_unit_object(&dir.path().join(".kio"));
    fs::remove_file(&unit_path).unwrap();

    let error = json_failure(&dir, &["repair", "rebuild-db"], 4);
    assert_eq!(error["error_code"], "KIO-E-STORE-CORRUPT-001", "{error}");
}

// R16-5: `kio diff` with a shallow side (its commit or tree object discarded) must
// surface a clear COMMIT-SHALLOW that names WHICH side (a/b) is shallow, not a raw
// opaque KIO-E-STORE-NOT-FOUND-001 whose hash the user cannot map to an operand.
#[test]
fn r16_5_diff_with_shallow_side_names_the_side() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("d.md"), "# D\n\n## S\nv1 body\n").unwrap();
    kio(&dir, &["init"]).assert().success();
    let c1 = json_success(&dir, &["index", "--yes"]);
    let c1_hash = c1["commit_hash"].as_str().unwrap().to_owned();
    fs::write(dir.path().join("d.md"), "# D\n\n## S\nv2 body\n").unwrap();
    let c2 = json_success(&dir, &["index", "--yes"]);
    let c2_hash = c2["commit_hash"].as_str().unwrap().to_owned();

    // (control) a healthy diff of the two commits works.
    let ok = json_success(&dir, &["diff", &c1_hash, &c2_hash]);
    assert!(!ok["changes"].as_array().unwrap().is_empty());

    // Make the non-tip Auto C1 legitimately shallow: receipt first, then tree.
    let kio_dir = dir.path().join(".kio");
    let c1_obj: Value =
        serde_json::from_slice(&fs::read(object_path(&kio_dir, "commits", &c1_hash)).unwrap())
            .unwrap();
    let c1_tree = c1_obj["tree"].as_str().unwrap();
    let receipt_path = kio_dir
        .join("gc/shallowed")
        .join(c1_hash.strip_prefix("sha256:").unwrap());
    fs::create_dir_all(receipt_path.parent().unwrap()).unwrap();
    let receipt = ShallowReceipt::new(
        c1_hash.clone(),
        c1_tree.to_owned(),
        "2026-08-14T00:00:00Z".into(),
    )
    .unwrap();
    fs::write(receipt_path, receipt.canonical_bytes().unwrap()).unwrap();
    fs::remove_file(object_path(&kio_dir, "trees", c1_tree)).unwrap();

    // Naming C1 as operand b → COMMIT-SHALLOW, context side="b".
    let err_b = json_failure(&dir, &["diff", &c2_hash, &c1_hash], 1);
    assert_eq!(err_b["error_code"], "KIO-E-COMMIT-SHALLOW-001", "{err_b}");
    assert_eq!(err_b["context"]["side"], "b", "{err_b}");
    // Naming C1 as operand a → COMMIT-SHALLOW, context side="a".
    let err_a = json_failure(&dir, &["diff", &c1_hash, &c2_hash], 1);
    assert_eq!(err_a["error_code"], "KIO-E-COMMIT-SHALLOW-001", "{err_a}");
    assert_eq!(err_a["context"]["side"], "a", "{err_a}");
}

// R17-5: `resolve_commit` (hash-literal + tag-name branches) and `tag`'s implicit-HEAD
// verification read were the 3 read_commit sites R16-1's COMMIT-SHALLOW sweep missed.
// A shallow commit (its whole commit object gone, not merely its tree — the case that
// fails INSIDE resolve_commit, before diff_side_tree's R16-5 absorption) reached via a
// hash literal, a tag name, or the implicit HEAD must fold into KIO-E-COMMIT-SHALLOW-001
// (exit 1) — the same contract the `HEAD` *string* operand already reached — not
// escape as a raw KIO-E-STORE-NOT-FOUND-001 (exit 4).
#[test]
fn r17_5_shallow_commit_via_hash_tag_and_implicit_head_folds_to_commit_shallow() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("d.md"), "# D\n\n## S\nv1 body\n").unwrap();
    kio(&dir, &["init"]).assert().success();
    let c1 = json_success(&dir, &["snapshot", "create", "-m", "first"]);
    let c1_hash = c1["commit_hash"].as_str().unwrap().to_owned();
    // A tag pointing at C1, created while C1 is healthy (control + tag-name coverage).
    json_success(&dir, &["tag", "tagc1", &c1_hash]);
    fs::write(dir.path().join("d.md"), "# D\n\n## S\nv2 body\n").unwrap();
    let c2 = json_success(&dir, &["snapshot", "create", "-m", "second"]);
    let c2_hash = c2["commit_hash"].as_str().unwrap().to_owned();

    // (control) a healthy diff of the two hash literals works.
    let ok = json_success(&dir, &["diff", &c1_hash, &c2_hash]);
    assert!(!ok["changes"].as_array().unwrap().is_empty());

    // Make C1 shallow by deleting its whole COMMIT object (not merely its tree).
    let kio_dir = dir.path().join(".kio");
    fs::remove_file(object_path(&kio_dir, "commits", &c1_hash)).unwrap();

    // hash literal as diff side a, then side b (resolve_commit hash branch, scope.rs:689).
    let err_a = json_failure(&dir, &["diff", &c1_hash, "HEAD"], 1);
    assert_eq!(err_a["error_code"], "KIO-E-COMMIT-SHALLOW-001", "{err_a}");
    let err_b = json_failure(&dir, &["diff", "HEAD", &c1_hash], 1);
    assert_eq!(err_b["error_code"], "KIO-E-COMMIT-SHALLOW-001", "{err_b}");
    // tag creation with a shallow hash-literal operand (tag -> resolve_commit).
    let err_tag = json_failure(&dir, &["tag", "newtag", &c1_hash], 1);
    assert_eq!(
        err_tag["error_code"], "KIO-E-COMMIT-SHALLOW-001",
        "{err_tag}"
    );
    // tag-NAME target now shallow (resolve_commit tag-name branch, scope.rs:696).
    let err_tagname = json_failure(&dir, &["diff", "tagc1", "HEAD"], 1);
    assert_eq!(
        err_tagname["error_code"], "KIO-E-COMMIT-SHALLOW-001",
        "{err_tagname}"
    );

    // Implicit HEAD: make HEAD (C2) shallow too, then `tag` with no operand resolves
    // the implicit HEAD via head_commit_hash() and verifies it (scope.rs:662).
    fs::remove_file(object_path(&kio_dir, "commits", &c2_hash)).unwrap();
    let err_head = json_failure(&dir, &["tag", "headtag"], 1);
    assert_eq!(
        err_head["error_code"], "KIO-E-COMMIT-SHALLOW-001",
        "{err_head}"
    );
}

// R17-2: `reindex --force` must not let ONE document's corrupt normalized unit abort the
// whole scope's re-normalization (docs/10 §7.2). R16-4 gave `rebuild_step3_index` this
// skip-continue resilience, but the pre-rebuild copy loop in `run_reindex` never inherited
// it — a single STORE-CORRUPT unit killed the reindex (exit 4) and the healthy documents
// were never re-normalized, while `repair`'s guidance still pointed users AT this broken
// command. Now the corrupt document is skipped (its previous gen kept) and reported under
// skipped_units; the healthy document is re-normalized and the scope stays searchable.
#[test]
fn r17_2_reindex_force_skips_corrupt_unit_and_renormalizes_healthy() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("healthy.md"),
        "# Healthy\n\n## Body\nhealthytoken alpha content here\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("corrupt.md"),
        "# Corrupt\n\n## Body\ncorrupttoken beta content here\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    json_success(&dir, &["index", "--yes"]);

    // Corrupt ONE document's normalized-instance manifest → copy_normalized_instance_gen
    // raises KIO-E-STORE-CORRUPT-001 for that raw_hash during re-normalization.
    let units_root = dir.path().join(".kio/objects/normalized_units");
    let manifest = first_manifest_json(&units_root);
    let corrupt_raw_hash = {
        let value: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
        value["raw_hash"].as_str().unwrap().to_owned()
    };
    fs::write(&manifest, r#"{"torn":"#).unwrap();

    // Before the fix: exit 4 (KIO-E-STORE-CORRUPT-001), no re-normalization at all.
    let out = json_success(&dir, &["reindex", "--regenerate", "--yes"]);
    // The healthy document IS re-normalized (the loop no longer dies on the corrupt one).
    assert_eq!(
        out["reindexed_files"].as_u64().unwrap(),
        1,
        "the healthy document is re-normalized: {out}"
    );
    let skipped = out["skipped_units"].as_array().unwrap();
    assert_eq!(
        skipped.len(),
        1,
        "exactly the corrupt document is skipped (deduped across both phases): {out}"
    );
    assert_eq!(skipped[0]["reason"], "KIO-E-STORE-CORRUPT-001");
    assert_eq!(
        skipped[0]["raw_hash"], corrupt_raw_hash,
        "the skip names the corrupted document: {out}"
    );
    assert!(
        out["skipped_units_guidance"].as_str().is_some(),
        "the skip is loudly disclosed: {out}"
    );
    // The scope is not bricked — the healthy document still searches after the reindex.
    let search = json_success(&dir, &["search", "healthytoken"]);
    assert!(
        !search["results"].as_array().unwrap().is_empty(),
        "healthy document searchable after reindex: {search}"
    );
}

// R17-4: when EVERY searched scope is excluded for a store-corruption class the search
// must not fall through to the bare KIO-E-SEARCH-SCOPE-ALL-FAILED-001 (exit 4, no
// guidance) — that left an operator/agent with no recovery path, unlike the
// index_missing/index_corrupt case which points at `repair`. A store_corrupt (tampered
// HEAD commit object) all-scope failure keeps the docs-registered
// KIO-E-SEARCH-SCOPE-ALL-FAILED-001 code but now carries class-specific recovery
// guidance in `context.recovery` + the message. Exit stays 4; `repair all` is the
// device-global path that verifies/rebuilds both the store and its derived indexes.
#[test]
fn r17_4_store_corrupt_all_scopes_returns_recovery_guidance() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("a.md"),
        "# A\n\n## Sec\nalphacorrupt token\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    json_success(&dir, &["index", "--yes"]);

    // Corrupt the single scope's HEAD commit object (garbage → content-hash mismatch →
    // STORE-CORRUPT → Excluded("store_corrupt")); with no healthy scope, every searched
    // scope failed for the store-corruption class.
    let kio_dir = dir.path().join(".kio");
    let head = head_commit(&kio_dir);
    fs::write(
        object_path(&kio_dir, "commits", &head),
        b"this is not a valid commit object",
    )
    .unwrap();

    let err = json_failure(&dir, &["search", "alphacorrupt"], 4);
    assert_eq!(
        err["error_code"], "KIO-E-SEARCH-SCOPE-ALL-FAILED-001",
        "store corruption keeps the docs-registered all-failed code (no new code): {err}"
    );
    // Distinguished from the BARE all-failed not by a new code but by carrying
    // class-specific `context.recovery` guidance (asserted below).
    let recovery = err["context"]["recovery"].as_array().unwrap();
    assert!(
        recovery
            .iter()
            .any(|line| line.as_str().unwrap().contains("store_corrupt")),
        "store_corrupt-specific recovery guidance is present: {err}"
    );
    assert!(
        err["message"].as_str().unwrap().contains("repair all"),
        "guidance names the recovery command: {err}"
    );
}

// R17-4: a receiptless missing HEAD tree is store corruption and receives the
// store-corrupt recovery guidance. Valid GC shallow commits are never ref tips.
#[test]
fn r17_4_unreceipted_missing_head_returns_store_corrupt_guidance() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("a.md"),
        "# A\n\n## Sec\nalphashallow token\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    json_success(&dir, &["index", "--yes"]);

    // Advance HEAD with a manual snapshot (tree_entries NOT projected) then discard its
    // tree object → the fresh search sees ShallowNoRows → Excluded("snapshot_shallow").
    fs::write(
        dir.path().join("a.md"),
        "# A\n\n## Sec\nalphashallow token v2\n",
    )
    .unwrap();
    let snap = json_success(&dir, &["snapshot", "create", "-m", "advance"]);
    let c2 = snap["commit_hash"].as_str().unwrap().to_owned();
    let kio_dir = dir.path().join(".kio");
    let c2_obj: Value =
        serde_json::from_slice(&fs::read(object_path(&kio_dir, "commits", &c2)).unwrap()).unwrap();
    fs::remove_file(object_path(
        &kio_dir,
        "trees",
        c2_obj["tree"].as_str().unwrap(),
    ))
    .unwrap();

    let err = json_failure(&dir, &["search", "alphashallow"], 4);
    assert_eq!(
        err["error_code"], "KIO-E-SEARCH-SCOPE-ALL-FAILED-001",
        "{err}"
    );
    let recovery = err["context"]["recovery"].as_array().unwrap();
    assert!(
        recovery
            .iter()
            .any(|line| line.as_str().unwrap().contains("store_corrupt")),
        "store_corrupt recovery guidance is present: {err}"
    );
    assert!(
        err["message"].as_str().unwrap().contains("repair all"),
        "guidance names the fail-closed store recovery command: {err}"
    );
}

// The path-named normalized unit is only a mutable cache. Its corruption must
// neither influence reconstruction nor produce a false stale-source warning.
#[test]
fn r17_6_repair_ignores_corrupt_mutable_cache_when_chunks_survive() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("a.md"),
        "# A\n\n## Body\ncachedserving unique token\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    json_success(&dir, &["index", "--yes"]);

    // Corrupt only the mutable cache and force a derived SQLite rebuild.
    let unit = first_normalized_unit_json(&dir.path().join(".kio/objects/normalized_units"));
    fs::write(&unit, r#"{"torn":"#).unwrap();
    fs::remove_file(dir.path().join(".kio/index/sqlite.db")).unwrap();

    let out = json_success(&dir, &["repair", "rebuild-db"]);
    assert!(out["skipped_units"].as_array().unwrap().is_empty(), "{out}");
    let search = json_success(&dir, &["search", "cachedserving"]);
    assert!(
        !search["results"].as_array().unwrap().is_empty(),
        "cached document still searches after repair: {search}"
    );
}

// Even without persisted chunks, reconstruction must derive from the immutable
// unit CAS rather than a mutable cache body.
#[test]
fn r17_6_repair_rebuilds_from_immutable_unit_when_no_cached_chunks_survive() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("only.md"),
        "# Only\n\n## Body\nonlydoc unique token\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    json_success(&dir, &["index", "--yes"]);

    // Corrupt the cache and remove persisted chunks, forcing CAS-based chunking.
    let unit = first_normalized_unit_json(&dir.path().join(".kio/objects/normalized_units"));
    fs::write(&unit, r#"{"torn":"#).unwrap();
    fs::remove_file(dir.path().join(".kio/index/chunks.jsonl")).unwrap();

    let out = json_success(&dir, &["repair", "rebuild-db"]);
    assert!(out["skipped_units"].as_array().unwrap().is_empty(), "{out}");
    let search = json_success(&dir, &["search", "onlydoc"]);
    assert!(
        !search["results"].as_array().unwrap().is_empty(),
        "immutable unit CAS must restore chunks without reading the cache: {search}"
    );
}

// ===========================================================================
// R16-6: the hand-rolled arg parsers coerced `--flag=<value>` on a value-LESS
// (boolean / SetTrue) flag into `true`, silently dropping the inline value — so
// `reindex --force=false --yes=false` (an explicit negation) bypassed the
// confirmation gate and ran a full reindex (exit 0). Every value-less flag must
// now reject an inline value with KIO-E-CONFIG-USAGE-001 (exit 2), matching clap's
// derived bool flags (which already reject `--json=false`). Value-TAKING flags keep
// consuming their inline value.
// ===========================================================================
#[test]
fn r16_6_valueless_flag_inline_value_is_a_usage_error() {
    let dir = indexed_scope();

    // reindex: the reported gate bypass. `--regenerate=false` must not be
    // coerced to true. B/C (2026-07-24): clap owns this rejection now, and the
    // flag is `--regenerate` (the old `--force` was removed outright, not
    // aliased). The contract asserted here is the rejection plus its
    // code/exit, not the wording.
    let err = json_failure(&dir, &["reindex", "--regenerate=false"], 2);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-USAGE-001", "{err}");
    let message = err["message"].as_str().unwrap();
    assert!(
        message.contains("unexpected value") && message.contains("regenerate"),
        "{err}"
    );
    // The negated confirmation flag is equally rejected (would have bypassed --yes).
    let err = json_failure(&dir, &["reindex", "--regenerate", "--yes=false"], 2);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-USAGE-001", "{err}");

    // repair: the operations are sub-commands, so an inline value cannot reach
    // one at all — `--rebuild-db=false` is simply not an argument any more.
    let err = json_failure(&dir, &["repair", "--rebuild-db=false"], 2);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-USAGE-001", "{err}");

    // search: the mode selector takes a value, so the boolean-coercion hazard
    // is gone by construction; `--prune-orphans=false` covers a real boolean.
    let err = json_failure(
        &dir,
        &["repair", "verify-objects", "--prune-orphans=false"],
        2,
    );
    assert_eq!(err["error_code"], "KIO-E-CONFIG-USAGE-001", "{err}");
    // A boolean search flag, for good measure.
    let err = json_failure(&dir, &["search", "トークン", "--all-scopes=false"], 2);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-USAGE-001", "{err}");

    // Controls: a value-TAKING flag still accepts its inline value, and the real
    // reindex confirmation path (no inline value) still runs.
    let search = json_success(&dir, &["search", "トークン", "--limit=1"]);
    assert!(
        search["results"].as_array().unwrap().len() <= 1,
        "value-taking --limit=1 must be honored, not rejected: {search}"
    );
    let reindexed = json_success(&dir, &["reindex", "--regenerate", "--yes"]);
    assert_eq!(reindexed["status"], "reindexed", "{reindexed}");
}

// ===========================================================================
// R16-7: a retry-able failure re-reserved the FULL cost on every send attempt, so a
// phantom charge accumulated without bound (RateLimit retries are unbounded) and
// could exhaust the device month cap, falsely pausing unrelated tasks in other
// scopes. The gate is error-kind-aware: a resend whose PREVIOUS failure was a
// non-billable rejection (RateLimit / QuotaExceeded) reuses the prior reservation
// and does NOT re-charge; a NetworkError resend (possibly billed server-side,
// bounded by max_attempts) still does.
// ===========================================================================

/// Run `kio <args> --json` with the online markdownize seam pinned to `seam` and an
/// optional frozen clock, tolerating ANY exit (batch resume/retry return non-zero on
/// a retry-able failure while still printing their JSON result to stdout).
fn run_markdownize_seam(
    dir: &TempDir,
    seam: &str,
    fixed_now: Option<&str>,
    args: &[&str],
) -> Value {
    let mut command = kio(dir, args);
    command.env(TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV, seam);
    if let Some(now) = fixed_now {
        command.env("KIO_FIXED_NOW", now);
    }
    let output = command.arg("--json").assert().get_output().stdout.clone();
    serde_json::from_slice(&output).unwrap_or(Value::Null)
}

fn approve_markdownize(dir: &TempDir, seam: &str) {
    run_markdownize_seam(dir, seam, None, &["adapter", "approve", "--all", "--yes"]);
}

/// `cost-ledger.sqlite`, opened read-only-in-spirit (queries only) for test
/// assertions. The harness roots `$XDG_DATA_HOME` at `.test-data`.
fn ledger_db(dir: &TempDir) -> Connection {
    Connection::open(dir.path().join(".test-data/kio/cost-ledger.sqlite")).unwrap()
}

/// Count of DISTINCT task-key reservations EVER made for `adapter_kind` — open
/// or terminal (`batch_requests` rows are never pruned for non-device task
/// rows in this implementation, only `scope_id='device'` rows are, §H/CL55).
/// The SQLite-era analog of the retired JSONL charge ledger's "line count",
/// which also only ever grew (a reservation there was an immediate, permanent
/// append). Under the SQLite ledger a reservation is instead one
/// `batch_requests` row that starts open and later turns terminal in place —
/// this counts the row regardless of which state it is currently in.
fn reservation_row_count(dir: &TempDir, adapter_kind: &str) -> usize {
    let db = ledger_db(dir);
    db.query_row(
        "SELECT COUNT(*) FROM batch_requests WHERE adapter_kind = ?1",
        [adapter_kind],
        |row| row.get::<_, i64>(0),
    )
    .unwrap() as usize
}

/// Count of STILL-OPEN (non-terminal, `state IN (0,1)`) reservations for
/// `adapter_kind` — used to confirm a stranded reservation was actually
/// released (settled terminal), not left dangling open forever.
fn open_reservation_count(dir: &TempDir, adapter_kind: &str) -> usize {
    let db = ledger_db(dir);
    db.query_row(
        "SELECT COUNT(*) FROM batch_requests WHERE adapter_kind = ?1 AND state IN (0, 1)",
        [adapter_kind],
        |row| row.get::<_, i64>(0),
    )
    .unwrap() as usize
}

/// Total USD for `adapter_kind`: confirmed (`cost_ledger`, any month — these
/// tests do not care about calendar-month boundaries) plus still-open
/// (`batch_requests.estimated_usd` for a non-terminal row) — the SQLite-era
/// analog of the retired JSONL charge ledger's "sum of every charged row",
/// which counted a reservation from the moment it was MADE (JSONL wrote a
/// real charge line immediately on reserve). `cost_ledger` alone only gains a
/// row at terminal settlement, so a still-open reservation must be added from
/// `batch_requests` to match the old helper's "as-reserved" semantics.
fn reservation_or_charged_usd(dir: &TempDir, adapter_kind: &str) -> f64 {
    let conn = ledger_db(dir);
    let confirmed: f64 = conn
        .query_row(
            "SELECT COALESCE(SUM(usd), 0) FROM cost_ledger WHERE adapter_kind = ?1",
            [adapter_kind],
            |row| row.get(0),
        )
        .unwrap();
    let open: f64 = conn
        .query_row(
            "SELECT COALESCE(SUM(estimated_usd), 0) FROM batch_requests \
             WHERE adapter_kind = ?1 AND state IN (0, 1)",
            [adapter_kind],
            |row| row.get(0),
        )
        .unwrap();
    confirmed + open
}

fn markdown_ledger_rows(dir: &TempDir) -> usize {
    reservation_row_count(dir, "markdownize")
}

// Discriminator (a): rate_limit ×N retry keeps the online markdownize charge row at 1.
// Before R16-7 each resend re-reserved the full cost. QA3 (step4b-contract-tests-p3a.md
// §A, 04 §5.2/§5.3): the charge-stays-1 core is UNCHANGED by QA3 (ledger reuse keys on
// the still-open row, not on task status) — but the task itself now stays `pending`
// (never `failed`) throughout, and `attempts` is NOT consumed (max_attempts=∞) —
// this test used to assert `attempts >= 2` after N retries.
#[test]
fn r16_7_rate_limit_retry_does_not_reaccrue_charge() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("doc.pdf"),
        fake_pdf(&["レート制限リトライの課金累積回帰テスト本文です。"]),
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_markdownize(&dir, "mock");
    // index only ENQUEUES the online markdownize task (Pending); no send, no charge.
    run_markdownize_seam(&dir, "mock", None, &["index"]);
    assert_eq!(
        markdown_ledger_rows(&dir),
        0,
        "index must not reserve a markdownize charge before any send"
    );

    // First real send under the rate_limit seam: one charge reserved, task -> Pending
    // (QA3: never Failed) with `next_retry_at` set.
    run_markdownize_seam(
        &dir,
        "rate_limit",
        Some("2026-07-03T00:00:00Z"),
        &["batch", "resume"],
    );
    assert_eq!(
        markdown_ledger_rows(&dir),
        1,
        "the first online send reserves exactly one charge"
    );

    // Retry past the backoff repeatedly (each iteration is a minute apart, well past
    // the 2s synthetic backoff, so the Pending-selection loop re-sends it each time).
    // RateLimit is refused before billing, so the prior reservation covers each
    // resend — the charge count must stay 1 (the fix).
    for minute in 1..=4 {
        let now = format!("2026-07-03T00:0{minute}:30Z");
        run_markdownize_seam(&dir, "rate_limit", Some(&now), &["batch", "retry"]);
        assert_eq!(
            markdown_ledger_rows(&dir),
            1,
            "rate_limit retry #{minute} must not re-accrue a phantom charge"
        );
    }

    // QA3: attempts is NOT consumed by rate_limit (max_attempts=∞, 04 §5.3) — the
    // task stays pending with attempts==0 even after repeated resends; only the
    // charge-stays-1 core is the invariant this test pins.
    let status = run_markdownize_seam(&dir, "rate_limit", None, &["status"]);
    let online = tasks_of_type(&status, "markdownize")
        .into_iter()
        .find(|task| task["fallback_reason"] == "rate_limit")
        .expect("a rate-limited online markdownize task");
    assert_eq!(online["status"], "pending");
    assert_eq!(
        online["attempts"].as_u64().unwrap(),
        0,
        "rate_limit retries must not consume the retry budget: {status}"
    );
}

// Discriminator (b), UPDATED for cost-ledger.sqlite (2026-07-21): under the retired
// JSONL ledger a NetworkError resend re-reserved (and immediately, permanently
// charged) each attempt, because a "reservation" there WAS an irrevocable JSONL
// append — RateLimit/QuotaExceeded got a special reuse gate (R16-7) precisely to
// avoid that growth; NetworkError was deliberately left OUT of the gate ("may have
// billed server-side"). Under the SQLite ledger a reservation is instead one
// `batch_requests` row that starts open and does not become a real `cost_ledger`
// charge until a definitive terminal Tx — `reserve_or_reuse_task_charge` reuses ANY
// still-open row for a retry, uniformly across every retryable error kind (CL42/
// CL44: "does an open row already exist for this task key" is the whole rule,
// independent of which kind left it open). This does not reintroduce the R15-5
// silent-cap-bypass NetworkError's old re-reserve-every-attempt behavior guarded
// against: the ONE open reservation stays counted in the cap for the entire retry
// window (`ledger_month_total`'s unterminated-`batch_requests` term), so the risk
// the old gate/growth was managing (real spend silently exceeding what the ledger
// reflects) does not reappear — it is simply no longer visible as row-count growth.
#[test]
fn r16_7_network_error_retry_does_not_reaccrue_charge() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("doc.pdf"),
        fake_pdf(&["ネットワークエラーリトライの課金回帰テスト本文です。"]),
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_markdownize(&dir, "mock");
    run_markdownize_seam(&dir, "mock", None, &["index"]);

    // First send fails NetworkError: one open reservation.
    run_markdownize_seam(
        &dir,
        "network_error",
        Some("2026-07-03T00:00:00Z"),
        &["batch", "resume"],
    );
    assert_eq!(
        markdown_ledger_rows(&dir),
        1,
        "the first online send reserves one row"
    );

    // Each retry REUSES the same open reservation — the row count stays 1 (no
    // growth, and no cap erosion either: the single open row is still counted the
    // whole time it stays open).
    for minute in 1..=3 {
        let now = format!("2026-07-03T00:0{minute}:30Z");
        run_markdownize_seam(&dir, "network_error", Some(&now), &["batch", "retry"]);
        assert_eq!(
            markdown_ledger_rows(&dir),
            1,
            "network_error retry #{minute} must reuse the open reservation, not grow"
        );
    }
}

// ===========================================================================
// R17-3, UPDATED for cost-ledger.sqlite (2026-07-21): a rate_limit-Failed online
// markdownize task keeps its ledger reservation open, which R16-7 (retired form)
// established is a phantom (rate_limit/quota never bill). R15-2's enqueue-time
// supersede retired only Pending/Paused, so after the file was edited the stale
// Failed(rate_limit) task lingered — under the retired JSONL design its phantom
// reservation exhausted the per-adapter markdownize cap, falsely pausing the
// re-indexed (valid) task, and the fix reclaimed it into a sibling positive-only
// ledger (F3: the charge ledger was never negatively amended) so the edited doc
// could proceed. `cost-ledger.sqlite` has no reclaim mechanism at all (CL45: an
// Adapter with no post-hoc query capability always settles `unknown_settled` — the
// conservative reservation estimate becomes a REAL, PERMANENT charge, never
// credited back) — `enqueue_online_placeholder_task`'s stale-task supersede now
// calls `release_task_charge_if_open`, which settles the phantom for real rather
// than reclaiming it. The discriminator pair below therefore now CONVERGES: both
// rate_limit and NetworkError phantoms become a real settled charge on supersede,
// so the edited doc is budget-Paused in both cases (a strictly MORE conservative
// outcome than the retired design's error-kind-aware reclaim — never an
// under-charge, per the ledger's "over-count is safer" posture).
// ===========================================================================

/// Sum the usd of the online-markdownize (`adapter_kind = "markdownize"`) rows,
/// confirmed or still-open — the exact per-document reservation cost, used to
/// size a per-adapter cap between one and two documents.
fn markdown_ledger_usd(dir: &TempDir) -> f64 {
    reservation_or_charged_usd(dir, "markdownize")
}

/// Pin a DEVICE per-adapter markdown cap sized to fit exactly ONE document cost but
/// not two (1.5×), with a generous overall monthly cap so only the per-adapter cap
/// binds. Written to the user (device) config the m8 tests already exercise.
fn set_markdown_adapter_cap(dir: &TempDir, cap: f64) {
    let config = dir.path().join(".test-config/kio/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(
        &config,
        format!("[budget]\nmonthly_usd_cap = 1000\n[budget.per_adapter]\nmarkdownize = {cap}\n"),
    )
    .unwrap();
}

/// Two page bodies of EQUAL byte length (they differ only in one trailing ASCII
/// char), so editing `v1 -> v2` changes `raw_hash` while keeping the document size —
/// and therefore the reservation cost — identical. This lets a cap of `1.5 × cost`
/// discriminate "one reservation fits" from "two reservations exceed".
const R17_3_BODY_V1: &str = "R17-3 phantom reclaim regression 本文あいうえお A";
const R17_3_BODY_V2: &str = "R17-3 phantom reclaim regression 本文あいうえお B";

// Discriminator (a): rate_limit phantom → edit → re-index. The phantom settles as a
// real, permanent `unknown_settled` charge (never reclaimed), so the edited doc's
// online task is budget-Paused under a cap sized for only one document — the same
// outcome as discriminator (b) below (the two converge under the new ledger).
#[test]
fn r17_3_rate_limit_phantom_settles_and_still_pauses_edited_doc() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        R17_3_BODY_V1.len(),
        R17_3_BODY_V2.len(),
        "the two doc bodies must be equal byte length so the cost is identical"
    );
    fs::write(dir.path().join("doc.pdf"), fake_pdf(&[R17_3_BODY_V1])).unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_markdownize(&dir, "mock");
    run_markdownize_seam(&dir, "mock", None, &["index"]);

    // First real send under rate_limit: v1 -> Failed(rate_limit), one open reservation.
    run_markdownize_seam(
        &dir,
        "rate_limit",
        Some("2026-07-03T00:00:00Z"),
        &["batch", "resume"],
    );
    assert_eq!(
        markdown_ledger_rows(&dir),
        1,
        "the rate_limit send reserves exactly one row"
    );
    assert_eq!(
        open_reservation_count(&dir, "markdownize"),
        0,
        "a typed 429 is a known zero-cost rejection, never an open reservation"
    );
    assert_eq!(
        markdown_ledger_usd(&dir),
        0.0,
        "429 must not consume budget"
    );

    // Edit (new raw_hash, identical size) and re-index. The stale v1 task is retired,
    // which settles its still-open reservation as `unknown_settled` — a real charge at
    // the original estimate, not a reclaim-to-zero.
    fs::write(dir.path().join("doc.pdf"), fake_pdf(&[R17_3_BODY_V2])).unwrap();
    run_markdownize_seam(&dir, "mock", Some("2026-07-05T00:00:00Z"), &["index"]);

    assert_eq!(
        open_reservation_count(&dir, "markdownize"),
        0,
        "the stale reservation must be settled (released), not left open forever"
    );
    assert_eq!(
        markdown_ledger_usd(&dir),
        0.0,
        "known rejection stays zero-cost"
    );

    let status = run_markdownize_seam(&dir, "mock", Some("2026-07-05T00:00:00Z"), &["status"]);
    let markdownize = tasks_of_type(&status, "markdownize");
    let paused_budget = markdownize
        .iter()
        .filter(|task| task["status"] == "paused" && task["fallback_reason"] == "budget_exceeded")
        .count();
    assert_eq!(
        paused_budget, 0,
        "a known 429 must not fabricate spend that pauses the edited document: {status}"
    );
    // The stale v1 task is retired (non-retryable retired_non_live), not left rate_limit.
    let still_rate_limited = markdownize
        .iter()
        .filter(|task| task["fallback_reason"] == "rate_limit")
        .count();
    assert_eq!(
        still_rate_limited, 0,
        "the stale rate_limit task must be superseded: {status}"
    );
}

// Discriminator (b): NetworkError phantom → edit → re-index. Its reservation settles
// as a real, permanent charge on supersede — exactly like discriminator (a)'s
// rate_limit case now (`r17_3_rate_limit_phantom_settles_and_still_pauses_edited_doc`):
// the retired design's error-kind-aware reclaim/keep asymmetry does not exist under
// `cost-ledger.sqlite` (CL45 — no Adapter here has post-hoc query capability, so
// every non-success sync settlement is uniformly `unknown_settled`). Kept as its own
// test to pin that a NetworkError phantom's fate is unchanged by this migration (it
// was never reclaimed even under the retired design).
#[test]
fn r17_3_network_error_reservation_settles_pauses_edited_doc() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("doc.pdf"), fake_pdf(&[R17_3_BODY_V1])).unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_markdownize(&dir, "mock");
    run_markdownize_seam(&dir, "mock", None, &["index"]);

    // First real send under network_error: v1 -> Failed(network_error), one open row.
    run_markdownize_seam(
        &dir,
        "network_error",
        Some("2026-07-03T00:00:00Z"),
        &["batch", "resume"],
    );
    assert_eq!(markdown_ledger_rows(&dir), 1, "the send reserves one row");
    let doc_cost = markdown_ledger_usd(&dir);
    assert!(doc_cost > 0.0, "the reservation must be a positive cost");
    set_markdown_adapter_cap(&dir, doc_cost * 1.5);

    // Edit and re-index in the same ledger month.
    fs::write(dir.path().join("doc.pdf"), fake_pdf(&[R17_3_BODY_V2])).unwrap();
    run_markdownize_seam(&dir, "mock", Some("2026-07-05T00:00:00Z"), &["index"]);

    // The NetworkError reservation settles as a real charge (never reclaimed, under
    // either the retired or the current design) and is no longer open.
    assert_eq!(
        open_reservation_count(&dir, "markdownize"),
        0,
        "the stale reservation must be settled, not left open forever"
    );
    assert_eq!(
        markdown_ledger_rows(&dir),
        1,
        "the settled reservation still counts toward the cap"
    );

    let status = run_markdownize_seam(&dir, "mock", Some("2026-07-05T00:00:00Z"), &["status"]);
    let markdownize = tasks_of_type(&status, "markdownize");
    // A potentially billed transport failure is fenced as result_unknown rather
    // than retired/retried after the file changes.
    let unknown = markdownize
        .iter()
        .filter(|task| task["status"] == "failed" && task["fallback_reason"] == "result_unknown")
        .count();
    assert_eq!(
        unknown, 1,
        "the stale task must remain outcome-unknown: {status}"
    );
    let paused_budget = markdownize
        .iter()
        .filter(|task| task["status"] == "paused" && task["fallback_reason"] == "budget_exceeded")
        .count();
    assert_eq!(
        paused_budget, 1,
        "the edited doc must be budget-paused (phantom kept, cap-safe): {status}"
    );
}

// ===========================================================================
// R18-1, UPDATED for cost-ledger.sqlite (2026-07-21): the EMBEDDING pipeline had NO
// reclaim path (only markdownize did, R17-3). An embedding send reserves before the
// send and keeps its reservation OPEN on a RateLimit/Quota failure (R16-7); once the
// chunk is edited/deleted (non-live) the task can never be retried, so a stranded
// reservation would eat the embedding per-adapter cap for the rest of the month and
// falsely pause unrelated future embeddings. `reconcile_committed_embedding_tasks`
// still releases a non-live task's reservation the same way R18-1 originally did —
// via `release_task_charge_if_open`, which settles it `unknown_settled` (a real,
// permanent charge) rather than reclaiming it to zero (CL45's "no post-hoc query
// capability → always unknown_settled" posture, uniform across every error kind —
// see r17_3's updated tests for the markdownize twin of this same convergence).
// ===========================================================================

fn embedding_ledger_rows(dir: &TempDir) -> usize {
    reservation_row_count(dir, "embedding")
}

fn embedding_ledger_usd(dir: &TempDir) -> f64 {
    reservation_or_charged_usd(dir, "embedding")
}

/// A definite provider rejection settles its cost-ledger row at zero but retains
/// the candidate estimate on `batch_requests`; caps for a later fresh attempt
/// must therefore size from that durable estimate, never from settled spend.
fn embedding_request_estimate(dir: &TempDir) -> f64 {
    let conn = ledger_db(dir);
    conn.query_row(
        "SELECT estimated_usd FROM batch_requests
         WHERE adapter_kind = 'embedding' ORDER BY submission_seq DESC LIMIT 1",
        [],
        |row| row.get(0),
    )
    .unwrap()
}

fn set_embedding_adapter_cap(dir: &TempDir, cap: f64) {
    let config = dir.path().join(".test-config/kio/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(
        &config,
        format!("[budget]\nmonthly_usd_cap = 1000\n[budget.per_adapter]\nembedding = {cap}\n"),
    )
    .unwrap();
}

// Two equal-byte-length embedding doc bodies (differ only in a trailing marker), so
// editing v1 -> v2 changes raw_hash (making the old chunk non-live) while keeping the
// per-document embedding cost identical — letting a `1.5 × cost` cap discriminate.
const R18_1_BODY_V1: &str =
    "# R18-1\n\nembedding phantom reclaim regression 本文あいうえお かきくけこ さしすせそ A\n";
const R18_1_BODY_V2: &str =
    "# R18-1\n\nembedding phantom reclaim regression 本文あいうえお かきくけこ さしすせそ B\n";

// Step 4 discriminator: a rate-limited historical chunk remains active work. A
// definite rejection has no outstanding charge to reclaim; its later fresh retry
// competes with the edited document under a cap sized for one send.
#[test]
fn ct4_retained_embedding_reservation_is_not_reclaimed_on_edit() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(R18_1_BODY_V1.len(), R18_1_BODY_V2.len());
    fs::write(dir.path().join("doc.md"), R18_1_BODY_V1).unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_embed(&dir, "rate_limit");

    // A definite 429 settles this attempted group at zero and clears its
    // reservation; later groups were never admitted or reserved.
    json_success_embed_at(
        &dir,
        "rate_limit",
        "2026-07-03T00:00:00Z",
        &["index", "--online"],
    );
    assert_eq!(
        embedding_ledger_rows(&dir),
        1,
        "only the attempted group receives a ledger row"
    );
    assert_eq!(
        embedding_ledger_usd(&dir),
        0.0,
        "a confirmed 429 records zero spend"
    );
    let doc_cost = embedding_request_estimate(&dir);
    assert!(
        doc_cost > 0.0,
        "the rejected request retains its positive estimate"
    );
    set_embedding_adapter_cap(&dir, doc_cost * 1.5);

    // Edit (new chunk_id → old chunk non-live) and re-index with mock in the same month.
    fs::write(dir.path().join("doc.md"), R18_1_BODY_V2).unwrap();
    approve_embed(&dir, "mock");
    // The retained task retries as one fresh attempt. The cap permits exactly
    // one contextualized group, so the other current group remains paused.
    kio(&dir, &["index", "--online"])
        .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
        .env("KIO_FIXED_NOW", "2026-07-05T00:00:00Z")
        .arg("--json")
        .assert()
        .code(6);

    // The retained chunk remains eligible through history. The initial
    // rejection was terminal and zero-cost, so this pass must make an
    // independent fresh admission rather than reuse an old reservation.
    assert_eq!(
        open_reservation_count(&dir, "embedding"),
        0,
        "the definite rejection must not leave an open reservation"
    );

    let status = json_success_embed_at(&dir, "mock", "2026-07-05T00:00:00Z", &["status"]);
    let embedding = tasks_of_type(&status, "embedding");
    let paused_budget = embedding
        .iter()
        .filter(|task| task["status"] == "paused" && task["fallback_reason"] == "budget_exceeded")
        .count();
    assert_eq!(
        paused_budget, 1,
        "one fresh contextualized group consumes the cap; the other remains paused: {status}"
    );
}

// Discriminator (b) — error-kind-awareness (a NetworkError embedding reservation is KEPT,
// not reclaimed, because it may have billed server-side) — is NOT re-tested here: the
// deterministic embedding seam models `network_error` as an unreachable adapter that never
// charges (no phantom to reclaim), and the reclaim runs through the SHARED
// `retire_online_task_reclaiming` helper whose error-kind gate is already validated by
// `r17_3_network_error_reservation_kept_pauses_edited_doc` (markdownize). The embedding
// path passes `EMBEDDING_ADAPTER_KIND` into that identical helper, so its NetworkError
// branch is covered by construction.

// R18-2, UPDATED for cost-ledger.sqlite (2026-07-21): markdownize's R17-3 supersede
// only fired for a re-scanned SAME path. A DELETED (or renamed) file never reappears
// as a scan candidate, so its Failed(rate_limit) phantom lingered open and kept
// eating the markdownize cap. `run_index_pipeline`'s orphan-sweep releases a
// deleted-path phantom the same way R18-2 originally did — but "release" now means
// `release_task_charge_if_open` settling it `unknown_settled` (a real, permanent
// charge), not a reclaim to zero. So the sweep still runs (the reservation stops
// being silently invisible/open-forever), but under a cap sized for only one
// document, doc2 is budget-paused rather than completing (CL45's conservative
// posture: never an under-charge).
#[test]
fn r18_2_markdownize_deleted_file_phantom_settles_still_pauses_doc2() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("doc.pdf"), fake_pdf(&[R17_3_BODY_V1])).unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_markdownize(&dir, "mock");
    run_markdownize_seam(&dir, "mock", None, &["index"]);
    run_markdownize_seam(
        &dir,
        "rate_limit",
        Some("2026-07-03T00:00:00Z"),
        &["batch", "resume"],
    );
    assert_eq!(markdown_ledger_rows(&dir), 1, "the rejection is recorded");
    assert_eq!(markdown_ledger_usd(&dir), 0.0, "known 429 has zero spend");

    // DELETE doc.pdf (its phantom must be settled/released) and add an equal-cost
    // doc2.pdf.
    fs::remove_file(dir.path().join("doc.pdf")).unwrap();
    fs::write(dir.path().join("doc2.pdf"), fake_pdf(&[R17_3_BODY_V2])).unwrap();
    // index sweeps the deleted-path phantom (settling it for real) before evaluating
    // doc2's own cap-check.
    run_markdownize_seam(&dir, "mock", Some("2026-07-05T00:00:00Z"), &["index"]);
    assert_eq!(
        open_reservation_count(&dir, "markdownize"),
        0,
        "the deleted file's phantom must be settled (released), not left open forever"
    );
    assert_eq!(
        markdown_ledger_usd(&dir),
        0.0,
        "deleted known rejection stays zero-cost"
    );
    run_markdownize_seam(
        &dir,
        "mock",
        Some("2026-07-05T00:00:00Z"),
        &["batch", "resume"],
    );
    let status = run_markdownize_seam(&dir, "mock", Some("2026-07-05T00:00:00Z"), &["status"]);
    let markdownize = tasks_of_type(&status, "markdownize");
    let paused_budget = markdownize
        .iter()
        .filter(|task| task["status"] == "paused" && task["fallback_reason"] == "budget_exceeded")
        .count();
    assert_eq!(
        paused_budget, 0,
        "doc2 must not be paused by a known zero-cost rejection: {status}"
    );
}

// R18-3, UPDATED for cost-ledger.sqlite (2026-07-21): R17-3's reclaim ledger was
// netted by the enforcement gate but NOT by the status/warning reports, so
// `kio status` over-reported spend after a reclaim. `cost-ledger.sqlite` has no
// reclaim/netting concept at all — `budget_status_json`/`scope_budget_warning` and
// the enforcement gate (`budget_remaining_for_adapter`) now share the exact same
// `ledger_month_total` read, so they can never diverge (the original R18-3 bug
// class — enforcement and reporting reading two different sums — is structurally
// impossible here, not just fixed for this one case). This test now instead pins
// that a settled phantom's charge IS visible in `device_spent_usd` (the honest,
// conservative complement to the retired design's netting).
#[test]
fn r18_3_status_budget_reports_settled_phantom_as_real_spend() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("doc.pdf"), fake_pdf(&[R17_3_BODY_V1])).unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_markdownize(&dir, "mock");
    run_markdownize_seam(&dir, "mock", None, &["index"]);
    run_markdownize_seam(
        &dir,
        "rate_limit",
        Some("2026-07-03T00:00:00Z"),
        &["batch", "resume"],
    );
    assert_eq!(markdown_ledger_usd(&dir), 0.0, "known 429 is zero-cost");

    // Edit → re-index: the same-path supersede settles the stale phantom for real
    // (`unknown_settled`, billed at its original reservation estimate).
    fs::write(dir.path().join("doc.pdf"), fake_pdf(&[R17_3_BODY_V2])).unwrap();
    run_markdownize_seam(&dir, "mock", Some("2026-07-05T00:00:00Z"), &["index"]);
    assert_eq!(
        open_reservation_count(&dir, "markdownize"),
        0,
        "the phantom must be settled (precondition for the spend-visibility check)"
    );

    let status = run_markdownize_seam(&dir, "mock", Some("2026-07-05T00:00:00Z"), &["status"]);
    let device_spent = status["budget"]["device_spent_usd"].as_f64().unwrap();
    assert_eq!(
        device_spent, 0.0,
        "status and enforcement must agree that a known 429 spent nothing"
    );
}

// R18-4: R17-4 attached store-corruption recovery guidance only when EVERY scope failed.
// A PARTIAL multi-scope exclusion (some scopes healthy) got a bare `reason` with no
// `recovery`. R18-4 attaches the hint to each individual excluded entry. (Models r16_2.)
#[test]
fn r18_4_partial_store_corruption_entry_carries_recovery_hint() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let a = parent.path().join("a");
    let b = parent.path().join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(a.join("a.md"), "# A\n\n## Sec\nalphaunique survivor\n").unwrap();
    fs::write(b.join("b.md"), "# B\n\n## Sec\nbetaunique other\n").unwrap();
    json_success_path(&a, &data_home, &["init"]);
    json_success_path(&b, &data_home, &["init"]);
    json_success_path(&a, &data_home, &["index"]);
    json_success_path(&b, &data_home, &["index"]);

    // Corrupt scope B's HEAD commit object (store_corrupt) — A stays healthy, so this is a
    // PARTIAL exclusion and the all-failed aggregate block is never reached.
    let b_kio = b.join(".kio");
    let b_head = head_commit(&b_kio);
    fs::write(
        object_path(&b_kio, "commits", &b_head),
        b"this is not a valid commit object",
    )
    .unwrap();

    let output = hermetic_kio_command()
        .current_dir(&a)
        .env("XDG_CONFIG_HOME", data_home.join("config"))
        .env("XDG_DATA_HOME", data_home.join("data"))
        .env("XDG_CACHE_HOME", data_home.join("cache"))
        .args(["search", "alphaunique", "--all-scopes", "--json"])
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let search: Value = serde_json::from_slice(&output).unwrap();
    let excluded = search["excluded_scopes"].as_array().unwrap();
    assert_eq!(excluded.len(), 1);
    assert_eq!(excluded[0]["reason"], "store_corrupt");
    let recovery = excluded[0]["recovery"].as_str().unwrap_or_default();
    assert!(
        recovery.contains("store_corrupt") && recovery.contains("repair all"),
        "the partial-exclusion entry must carry the store_corrupt recovery hint: {search}"
    );
}

// ===========================================================================
// R19 探索型監査 第19ラウンド 回帰テスト
// ===========================================================================

fn json_online_both_seams(dir: &TempDir, ocr: &str, embed: &str, args: &[&str]) -> Value {
    let output = kio(dir, args)
        .env(TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV, ocr)
        .env(TEST_ADOPTED_EMBEDDING_ENV, embed)
        .arg("--json")
        .assert()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap_or(Value::Null)
}

fn env_online_tasks<'a>(status: &'a Value, path: &str) -> Vec<&'a Value> {
    tasks_of_type(status, "markdownize")
        .into_iter()
        .chain(tasks_of_type(status, "embedding"))
        .filter(|t| t["input_path"] == path)
        .collect()
}

// R19-1: a Tier A secret explicitly un-ignored (`!pattern`) is ingested locally but its
// ONLINE send (OCR + embedding) must be HELD until `--send-secrets` — exactly like a
// (lower-risk) Tier B file. Before R19-1 the lifted Tier A slipped BOTH online gates
// (a risk-gradient inversion) and left no quarantine audit record.
#[test]
fn r19_1_lifted_tier_a_secret_held_from_online_send() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join(".env"),
        "AWS_SECRET_ACCESS_KEY=FAKE_TESTKEY_abcdefghijklmnop\n",
    )
    .unwrap();
    fs::write(dir.path().join(".kioignore"), "!.env\n").unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_embed(&dir, "rate_limit");
    approve_both_mock(&dir);
    json_online_both_seams(&dir, "mock", "mock", &["index", "--online"]);
    json_online_both_seams(&dir, "mock", "mock", &["batch", "resume"]);
    let status = json_online_both_seams(&dir, "mock", "mock", &["status"]);

    let held = env_online_tasks(&status, ".env")
        .iter()
        .filter(|t| t["fallback_reason"] == "secrets_tier_b_hold")
        .count();
    assert!(
        held >= 1,
        "lifted Tier A online task(s) must be HELD from send: {status}"
    );
    let sent = env_online_tasks(&status, ".env")
        .iter()
        .filter(|t| {
            t["fallback_reason"] == "online_adapter_done"
                || t["fallback_reason"] == "embedding_adapter_done"
        })
        .count();
    assert_eq!(
        sent, 0,
        "no lifted Tier A online task may be SENT without --send-secrets: {status}"
    );
    assert!(
        status["quarantine"]
            .as_array()
            .unwrap()
            .iter()
            .any(|q| q["path"] == ".env" && q["reason"] == "secrets_tier_a"),
        "lifted Tier A must be recorded in quarantine as secrets_tier_a: {status}"
    );

    // A separate secret-content grant releases both online paths.
    json_online_both_seams(
        &dir,
        "mock",
        "mock",
        &["adapter", "approve", "--all", "--yes", "--send-secrets"],
    );
    json_online_both_seams(&dir, "mock", "mock", &["index", "--online"]);
    json_online_both_seams(&dir, "mock", "mock", &["batch", "resume"]);
    let status = json_online_both_seams(&dir, "mock", "mock", &["status"]);
    let sent = env_online_tasks(&status, ".env")
        .iter()
        .filter(|t| {
            t["fallback_reason"] == "online_adapter_done"
                || t["fallback_reason"] == "embedding_adapter_done"
        })
        .count();
    assert!(
        sent >= 1,
        "adapter approve --send-secrets must release + send the lifted Tier A online tasks: {status}"
    );
    // R19-7: the quarantine disposition must transition hold -> send_approved (a path-only
    // dedup previously froze it at "hold", misreporting an approved+sent file as pending).
    let dispositions: Vec<&str> = status["quarantine"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|q| q["path"] == ".env")
        .filter_map(|q| q["approval_method"].as_str())
        .collect();
    assert!(
        dispositions.contains(&"send_approved"),
        "R19-7: quarantine disposition must reach send_approved after --send-secrets: {status}"
    );
}

// Step 4 retains edited-away chunks as active historical work. Reverting exact
// bytes reuses the same task without a retire/revive cycle or duplicate output_ref.
#[test]
fn ct4_reverted_chunk_reuses_retained_embedding_task() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(R18_1_BODY_V1.len(), R18_1_BODY_V2.len());
    fs::write(dir.path().join("doc.md"), R18_1_BODY_V1).unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_embed(&dir, "rate_limit");
    // v1 embedding fails rate_limit -> Failed(rate_limit) phantom.
    json_success_embed_at(
        &dir,
        "rate_limit",
        "2026-07-03T00:00:00Z",
        &["index", "--online"],
    );
    // Edit to v2: v1 remains retained and is retried in this pass.
    fs::write(dir.path().join("doc.md"), R18_1_BODY_V2).unwrap();
    approve_embed(&dir, "mock");
    json_success_embed_at(&dir, "mock", "2026-07-05T00:00:00Z", &["index", "--online"]);
    let status = json_success_embed_at(&dir, "mock", "2026-07-05T00:00:00Z", &["status"]);
    let retired = tasks_of_type(&status, "embedding")
        .iter()
        .filter(|t| t["fallback_reason"] == "retired_non_live")
        .count();
    assert_eq!(
        retired, 0,
        "retained history is never retired_non_live: {status}"
    );
    let tasks_before = tasks_of_type(&status, "embedding").len();

    // Revert to the EXACT v1 bytes -> the v1 chunk_id reappears; it must re-embed.
    fs::write(dir.path().join("doc.md"), R18_1_BODY_V1).unwrap();
    json_success_embed_at(&dir, "mock", "2026-07-07T00:00:00Z", &["index", "--online"]);
    let status = json_success_embed_at(&dir, "mock", "2026-07-07T00:00:00Z", &["status"]);
    let tasks_after = tasks_of_type(&status, "embedding").len();
    // Reverting reuses the retained task in place rather than appending a duplicate
    // `output_ref` task. A duplicate
    // would be double-stamped by `apply_embedding_transitions` (keyed on output_ref) and
    // then double-reclaimed (silent cap fail-open). So the task count must NOT grow; the
    // reappeared chunk still re-embeds (asserted below via `reverted_done`).
    assert_eq!(
        tasks_after, tasks_before,
        "reverting must reuse the retained task, not append a duplicate \
         output_ref task (before={tasks_before}, after={tasks_after}): {status}"
    );
    let reverted_done = tasks_of_type(&status, "embedding")
        .iter()
        .filter(|t| t["status"] == "done")
        .count();
    assert!(
        reverted_done >= 1,
        "R19-3: the reverted chunk must embed (Done): {status}"
    );
}

// R19-4: when two docs share an identical contextualized embedding identity,
// `rebuild_chunk_vec` links the missing chunk through its content-vector twin.
// Reconciliation must then converge that live-and-embedded task to Done. A
// definite 429 is terminal and zero-cost, so this also verifies that twin
// convergence leaves no speculative reservation open.
#[test]
fn r19_4_duplicate_content_failed_chunk_converges_via_twin() {
    let dir = tempfile::tempdir().unwrap();
    let shared = "## 共有セクション\n\n共有される段落です。十分な長さの本文をここに置きます。あいうえお かきくけこ さしすせそ。\n";
    // Contextual-embedding addendum (07 §5.3, 2026-07-24): a content twin now
    // requires the same body AND the same humanized filename context. Two DIFFERENT
    // filenames normally embed to DIFFERENT identities (no cross-file twin — the
    // correct new behavior), so to keep exercising the twin-convergence path these
    // two files use CONTEXT-FREE names (`_.md` / `__.md`, stems that humanize to
    // nothing → `chunk_embedding_context` is `None`). Their shared section is then
    // embedded bare under one identical `embedding_hash`, exactly as the pre-
    // addendum `a.md`/`b.md` pair did. (Kio indexes only a scope's own directory,
    // not subfolders, so same-basename files in sibling subdirs are not an option.)
    fs::write(
        dir.path().join("_.md"),
        format!("# 見出し AAAA\n\n{shared}"),
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    // Pin all passes to one instant so a rate-limited task is not normally
    // retry-due; twin materialization is the only completion route under test.
    let now = "2026-07-03T00:00:00Z";
    // The first _.md group gets a definite rate-limit rejection.
    approve_embed(&dir, "rate_limit");
    json_success_embed_at(&dir, "rate_limit", now, &["index", "--online"]);
    // __.md carries the IDENTICAL section under an equally context-free name (same
    // text_hash, same `None` context, different chunk_id). Indexing it with mock embeds
    // the shared text into the `embeddings` table under the identity _.md's chunk shares.
    fs::write(
        dir.path().join("__.md"),
        format!("# 見出し BBBB\n\n{shared}"),
    )
    .unwrap();
    approve_embed(&dir, "mock");
    json_success_embed_at(&dir, "mock", now, &["index", "--online"]);
    // `rebuild_chunk_vec` runs BEFORE embedding enrichment in a given index pass, so it is
    // the NEXT pass that links a.md's shared chunk_id to the twin's now-persisted vector —
    // and the reconcile then converges x/note.md's stuck Failed chunk (self-heal on re-index).
    json_success_embed_at(&dir, "mock", now, &["index", "--online"]);
    // The first definite 429 was settled at zero, and the healthy follow-up
    // groups have settled their own successful calls. Twin convergence must
    // therefore leave no open reservation behind.
    assert_eq!(
        open_reservation_count(&dir, "embedding"),
        0,
        "R19-4: twin convergence cannot strand an open reservation"
    );
    let status = json_success_embed_at(&dir, "mock", now, &["status"]);
    let a_done = tasks_of_type(&status, "embedding")
        .iter()
        .filter(|t| t["input_path"] == "_.md" && t["status"] == "done")
        .count();
    assert!(
        a_done >= 1,
        "R19-4: _.md's twin-embedded chunk must CONVERGE to Done, not stay Pending: {status}"
    );
}

// R19-2, UPDATED for cost-ledger.sqlite (2026-07-21): an exhausted-quota
// (QuotaExceeded, attempts >= max) online markdownize phantom must still be released
// by the deleted/renamed sweep. Before R19-2 the sweep gated on `task_retry_allowed`
// (false once the finite quota budget is spent), stranding the phantom reservation
// for the month — `is_reservation_bearing_send_failure`'s sweep-eligibility check in
// `run_index_pipeline`'s orphan sweep does not consult `task_retry_allowed` either,
// so the fix carries over unchanged; only the "release" mechanism changed (settles
// `unknown_settled`, a real charge, instead of reclaiming to zero). Quota has no test
// seam, so the terminal state is crafted directly (as the round's control repro did).
#[test]
fn r19_2_exhausted_quota_phantom_settled_on_sweep() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("doc.pdf"), fake_pdf(&[R17_3_BODY_V1])).unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_markdownize(&dir, "mock");
    run_markdownize_seam(&dir, "mock", None, &["index"]);
    // Fail the online send under rate_limit -> Pending(rate_limit) with a phantom
    // reservation (QA3, 04 §5.2: never Failed). The row is rewritten to a crafted
    // Failed(quota_exceeded) terminal state below anyway, so this precondition's
    // exact landing status is immaterial to the sweep this test exercises.
    run_markdownize_seam(
        &dir,
        "rate_limit",
        Some("2026-07-03T00:00:00Z"),
        &["batch", "resume"],
    );
    assert_eq!(
        markdown_ledger_rows(&dir),
        1,
        "the known rejection has one terminal ledger row"
    );

    // Rewrite the online markdownize task to an exhausted quota failure. A known
    // rejection has no reservation stamp, which is exactly the invariant this
    // sweep must preserve.
    let tasks_path = dir.path().join(".kio/tasks.jsonl");
    let text = fs::read_to_string(&tasks_path).unwrap();
    let mut crafted: Option<String> = None;
    for line in text.lines() {
        let t: Value = serde_json::from_str(line).unwrap();
        if t["type"] == "markdownize" && t["output_ref"] == "online:mistral_ocr_markdownize" {
            let mut row = t.clone();
            row["fallback_reason"] = Value::from("quota_exceeded");
            row["attempts"] = Value::from(3);
            row["status"] = Value::from("failed");
            row["next_retry_at"] = Value::Null;
            crafted = Some(serde_json::to_string(&row).unwrap());
        }
    }
    let crafted = crafted.expect("an online markdownize task must exist to craft");
    let mut appended = text.clone();
    appended.push_str(&crafted);
    appended.push('\n');
    fs::write(&tasks_path, appended).unwrap();

    // Delete the file (non-live) + add another, then re-index: the R18-2 sweep runs.
    fs::remove_file(dir.path().join("doc.pdf")).unwrap();
    fs::write(dir.path().join("doc2.pdf"), fake_pdf(&[R17_3_BODY_V2])).unwrap();
    run_markdownize_seam(&dir, "mock", Some("2026-07-05T00:00:00Z"), &["index"]);

    assert_eq!(
        open_reservation_count(&dir, "markdownize"),
        0,
        "R19-2: an exhausted known quota rejection must not create a sweep reservation"
    );
    assert_eq!(
        markdown_ledger_usd(&dir),
        0.0,
        "the sweep must preserve the quota rejection's zero-cost accounting"
    );
}

// R19-6: source-index corruption used to create a partial `index_corrupt`
// exclusion and therefore needed a recovery hint. Once the replica is the only
// candidate source, that writer-side damage must not enter the search response
// at all.
#[test]
fn r19_6_corrupt_source_index_does_not_create_a_search_exclusion() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let a = parent.path().join("a");
    let b = parent.path().join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(a.join("a.md"), "# A\n\n## Sec\nalphaunique survivor\n").unwrap();
    fs::write(b.join("b.md"), "# B\n\n## Sec\nbetaunique other\n").unwrap();
    json_success_path(&a, &data_home, &["init"]);
    json_success_path(&b, &data_home, &["init"]);
    json_success_path(&a, &data_home, &["index"]);
    json_success_path(&b, &data_home, &["index"]);

    // Corrupt scope B's writer-side SQLite after its replica write-through.
    fs::write(b.join(".kio/index/sqlite.db"), b"GARBAGE not a sqlite db").unwrap();

    let output = hermetic_kio_command()
        .current_dir(&a)
        .env("XDG_CONFIG_HOME", data_home.join("config"))
        .env("XDG_DATA_HOME", data_home.join("data"))
        .env("XDG_CACHE_HOME", data_home.join("cache"))
        .args(["search", "alphaunique", "--all-scopes", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let search: Value = serde_json::from_slice(&output).unwrap();
    assert!(
        search["excluded_scopes"].as_array().unwrap().is_empty(),
        "the candidate path must not inspect source SQLite: {search:#?}"
    );
    assert!(
        !search["results"].as_array().unwrap().is_empty(),
        "the healthy replica result must remain available: {search:#?}"
    );
}

// R19-8: a `max_input_bytes` cap tightened AFTER a task is enqueued must be honored at
// send time — a Pending online task is not shipped if the file now exceeds the cap
// (the sibling `allow_network` key is likewise re-checked at send).
#[test]
fn r19_8_lowered_max_input_bytes_blocks_queued_online_send() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = fake_pdf(&[R17_3_BODY_V1]);
    fs::write(dir.path().join("doc.pdf"), &pdf).unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_embed(&dir, "auth_error");
    // Enqueue the online markdownize task under the default (generous) cap.
    run_markdownize_seam(&dir, "mock", None, &["index"]);
    // Tighten the cap below the file size in the scope config. QA21
    // (step4b-contract-tests-p3a.md §G, 07-adapter-spec.md §3): `--approve`
    // already wrote `[adapter.policy]\nallow_network = true` (the network
    // -approval gate's positive condition — unset/lost = gate not
    // established), so this can no longer blindly APPEND a second
    // `[adapter.policy]` header (a duplicate-table TOML parse error) —
    // merge `max_input_bytes` into the existing table via `toml_edit`
    // instead, which also preserves `allow_network` untouched.
    let cap = pdf.len() - 5;
    let cfg = dir.path().join(".kio/config.toml");
    let existing = fs::read_to_string(&cfg).unwrap_or_default();
    let mut document = existing.parse::<toml_edit::DocumentMut>().unwrap();
    document["adapter"]["policy"]["max_input_bytes"] = toml_edit::value(cap as i64);
    fs::write(&cfg, document.to_string()).unwrap();
    // batch resume must NOT send the now-oversized task (no online charge).
    run_markdownize_seam(
        &dir,
        "mock",
        Some("2026-07-05T00:00:00Z"),
        &["batch", "resume"],
    );
    assert_eq!(
        markdown_ledger_rows(&dir),
        0,
        "R19-8: the oversized queued task must not be sent/charged"
    );
    let status = run_markdownize_seam(&dir, "mock", None, &["status"]);
    let sent = tasks_of_type(&status, "markdownize")
        .iter()
        .filter(|t| t["fallback_reason"] == "online_adapter_done")
        .count();
    assert_eq!(
        sent, 0,
        "R19-8: no online markdownize may complete under the lowered cap: {status}"
    );
}

// ===========================================================================
// R21 — exploratory audit round 21 regression tests.
// ===========================================================================

const R21_TWIN_BODY: &str =
    "# Notes\n\n## Body\nSome plain readable content for indexing purposes here alpha bravo.\n";

/// R21-1 [critical]: a byte-identical NON-secret twin of a Tier B file must NOT let the
/// secret file's chunk slip the embedding hold. The content-addressed `chunk_id` fans out
/// across both live paths; before the fix the non-secret instance landed in `sendable`
/// while the secret instance landed in `held`, and the send loop drove the shared held
/// task Done — shipping the Tier B file's text online with no `--send-secrets`.
#[test]
fn r21_1_byte_identical_twin_does_not_bypass_tier_b_hold() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("notes.md"), R21_TWIN_BODY).unwrap();
    fs::write(dir.path().join("password_backup.md"), R21_TWIN_BODY).unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_embed(&dir, "mock");
    // No `--send-secrets`.
    json_success_embed(&dir, "mock", &["index", "--online"]);
    let status = json_success_embed(&dir, "mock", &["status"]);
    let embedding = tasks_of_type(&status, "embedding");
    assert!(
        !embedding.is_empty(),
        "R21-1: expected embedding tasks: {status}"
    );
    for task in &embedding {
        assert_eq!(
            task["status"], "paused",
            "R21-1: every embedding task for a Tier B twin must stay held (paused), \
             not be driven Done by the non-secret twin: {status}"
        );
        assert_eq!(
            task["fallback_reason"], "secrets_tier_b_hold",
            "R21-1: held reason must be secrets_tier_b_hold: {status}"
        );
    }
}

/// R21-2: two byte-identical NON-secret files share one content-addressed `chunk_id`, so
/// they must produce exactly ONE embedding task per `output_ref` — not a duplicate that is
/// double-sent and double-charged (the R20-2 one-task-per-output_ref invariant, broken here
/// from a second source: the `tree_entries` JOIN fan-out).
#[test]
fn r21_2_byte_identical_twins_share_single_embedding_task() {
    let dir = tempfile::tempdir().unwrap();
    let body = "# Shared\nalpha bravo charlie delta echo foxtrot golf hotel india.\n";
    fs::write(dir.path().join("a.md"), body).unwrap();
    fs::write(dir.path().join("b.md"), body).unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_embed(&dir, "mock");
    json_success_embed(&dir, "mock", &["index", "--online"]);
    let status = json_success_embed(&dir, "mock", &["status"]);
    let embedding = tasks_of_type(&status, "embedding");
    let distinct: std::collections::BTreeSet<&str> = embedding
        .iter()
        .map(|t| t["output_ref"].as_str().unwrap())
        .collect();
    assert_eq!(
        embedding.len(),
        distinct.len(),
        "R21-2: byte-identical twins must not create duplicate output_ref tasks \
         (tasks={}, distinct={}): {status}",
        embedding.len(),
        distinct.len()
    );
}

/// R21-4: a TEXT file the extension table folds to `application/octet-stream` — an
/// uppercase-extension text-native file (`README.MD`) or an unknown-extension config file
/// (`.yaml`) — is fully handled by the local passthrough and must NOT also enqueue an
/// online OCR task (R9-2: shipping its bytes to Mistral OCR is a routing violation).
#[test]
fn r21_4_uppercase_and_octet_stream_text_enqueue_no_online_ocr() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("README.MD"),
        "# Upper\nreadme text body here.\n",
    )
    .unwrap();
    fs::write(dir.path().join("config.yaml"), "name: acme\nvalue: 42\n").unwrap();
    fs::write(dir.path().join("Dockerfile"), "FROM alpine\nRUN echo hi\n").unwrap();
    kio(&dir, &["init"]).assert().success();
    json_success(&dir, &["index"]);
    let status = json_success(&dir, &["status"]);
    let online: Vec<_> = status["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| {
            t["output_ref"]
                .as_str()
                .is_some_and(|r| r.starts_with("online:"))
        })
        .collect();
    assert!(
        online.is_empty(),
        "R21-4: text files must not enqueue online OCR tasks: {online:?} in {status}"
    );
    // The text is still locally searchable.
    let search = json_success(&dir, &["search", "acme", "--mode", "text"]);
    assert!(
        !search["results"].as_array().unwrap().is_empty(),
        "R21-4: octet-stream text must still be indexed locally: {search}"
    );
}

/// CAND-013/R21-6: AuthError revival requires explicit `batch resume`; ordinary indexing
/// must not silently revive a failed online operation. QA2 (step4b-contract-tests-p3a.md
/// §A, 04 §5.2): the precondition task now lands `paused` (hold_reason=auth), never
/// `failed` — this test used to assert `status=="failed"`.
#[test]
fn r21_6_auth_error_live_task_recovers_after_credentials_fixed() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("notes.txt"),
        "plain readable content alpha bravo charlie delta echo.\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_embed(&dir, "auth_error");
    // Credentials bad -> AuthError. `index` still exits 0 (enrichment failure is reported,
    // not fatal); read the resulting task state from `status`.
    let _ = kio(&dir, &["index", "--online"])
        .env(TEST_ADOPTED_EMBEDDING_ENV, "auth_error")
        .assert();
    let status = json_success_embed(&dir, "mock", &["status"]);
    assert!(
        tasks_of_type(&status, "embedding")
            .iter()
            .any(|t| t["status"] == "paused" && t["fallback_reason"] == "auth_error"),
        "R21-6: precondition — a Paused(auth) embedding task must exist: {status}"
    );
    // Credentials fixed (mock succeeds) -> explicit resume recovers it.
    json_success_embed(&dir, "mock", &["batch", "resume"]);
    let status = json_success_embed(&dir, "mock", &["status"]);
    let embedding = tasks_of_type(&status, "embedding");
    assert!(
        embedding.iter().any(|t| t["status"] == "done"),
        "R21-6: fixing credentials + explicit resume must recover AuthError to Done: {status}"
    );
    assert!(
        !embedding
            .iter()
            .any(|t| t["fallback_reason"] == "auth_error"),
        "R21-6: no AuthError task may remain stuck after recovery: {status}"
    );
}

/// Step 4 keeps a chunk active while any reachable history binds it. If an already
/// embedded historical chunk later appears under a Tier B path, the durable Done
/// task remains truthful, but the derived vector row is withheld until approval.
#[test]
fn ct4_historical_secret_path_withholds_existing_vector() {
    let dir = tempfile::tempdir().unwrap();
    let v1 = "# Notes\n\nordinary paragraph alpha bravo charlie delta echo foxtrot.\n";
    let v2 = "# Notes\n\nCOMPLETELY different body xray yankee zulu whiskey victor.\n";
    fs::write(dir.path().join("notes.md"), v1).unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_embed(&dir, "rate_limit");
    json_success_embed_at(
        &dir,
        "rate_limit",
        "2026-07-03T00:00:00Z",
        &["index", "--online"],
    );
    let initial_status = json_success_embed_at(&dir, "mock", "2026-07-03T00:00:00Z", &["status"]);
    let v1_output_ref = tasks_of_type(&initial_status, "embedding")[0]["output_ref"]
        .as_str()
        .unwrap()
        .to_owned();
    let v1_chunk_id = v1_output_ref.strip_prefix("embedding:").unwrap().to_owned();
    // Edit: v1 stays retained by history and may complete on the next pass.
    fs::write(dir.path().join("notes.md"), v2).unwrap();
    approve_embed(&dir, "mock");
    json_success_embed_at(&dir, "mock", "2026-07-05T00:00:00Z", &["index", "--online"]);
    // Delete and restore the EXACT v1 bytes under a Tier B name.
    fs::remove_file(dir.path().join("notes.md")).unwrap();
    fs::write(dir.path().join("password_notes.md"), v1).unwrap();
    json_success_embed_at(&dir, "mock", "2026-07-06T00:00:00Z", &["index", "--online"]);
    let status = json_success_embed_at(&dir, "mock", "2026-07-06T00:00:00Z", &["status"]);
    assert!(
        tasks_of_type(&status, "embedding")
            .iter()
            .any(|task| task["output_ref"] == v1_output_ref && task["status"] == "done"),
        "the already materialized embedding task remains a truthful terminal record: {status}"
    );
    kio_index::vec::ensure_registered();
    let conn = rusqlite::Connection::open(dir.path().join(".kio/index/sqlite.db")).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM chunk_vec WHERE chunk_id = ?1",
            rusqlite::params![v1_chunk_id],
            |row| row.get::<_, u64>(0),
        )
        .unwrap(),
        0,
        "any retained secret path must exclude the chunk from derived vector search"
    );
}

/// CT4-BBOX-006: a scanned/text-layer-less PDF (empty local `prepared_units`) must
/// complete OCR-from-scratch and remain idempotent across resume/re-index, rather than
/// staying Pending forever or churning replacement tasks.
#[test]
fn ct4_bbox_006_scanned_pdf_completes_without_churn() {
    let dir = tempfile::tempdir().unwrap();
    // %PDF header but no text layer (no `BT`): prepare_units returns empty.
    let mut scan = b"%PDF-1.4\n".to_vec();
    scan.extend((0u32..4000).map(|i| (i.wrapping_mul(97) & 0x7f) as u8 | 0x80));
    fs::write(dir.path().join("scan.pdf"), &scan).unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_both_mock(&dir);
    // A dedicated adapter grant records the persistent consent deferred work needs.
    json_both_mock(&dir, &["index"]);
    json_both_mock(&dir, &["batch", "resume"]);
    json_both_mock(&dir, &["index"]);
    json_both_mock(&dir, &["batch", "resume"]);
    let status = json_both_mock(&dir, &["status"]);
    let tasks = status["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|task| task["type"] == "markdownize")
        .collect::<Vec<_>>();
    assert_eq!(
        tasks.len(),
        1,
        "the scanned-PDF online task must not churn: {status}"
    );
    assert_eq!(
        tasks[0]["status"], "done",
        "OCR-from-scratch must reach Done: {status}"
    );
    assert!(
        !tasks[0]["output_ref"]
            .as_str()
            .unwrap()
            .starts_with("online:"),
        "Done must retain a normalized-instance output_ref: {status}"
    );
}

// ===========================================================================
// R22: exploratory-audit round 22 fixes (tasks/step3-bughunt22-fixes.md).
// ===========================================================================

/// Step 4 historical search retains old path aliases. Renaming a Tier B path to a
/// public live name therefore does not release its embedding hold: the old secret
/// alias remains reachable and secret-any-path wins until explicit approval.
#[test]
fn ct4_historical_secret_alias_keeps_hold_after_public_rename() {
    let dir = tempfile::tempdir().unwrap();
    let body = "# Notes\n\nalpha bravo charlie delta echo foxtrot golf hotel india juliet.\n";
    fs::write(dir.path().join("password_notes.md"), body).unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_embed(&dir, "mock");
    // Tier B name + online + no --send-secrets → the embedding task is HELD.
    json_success_embed(&dir, "mock", &["index", "--online"]);
    let status = json_success_embed(&dir, "mock", &["status"]);
    let embedding = tasks_of_type(&status, "embedding");
    assert!(
        !embedding.is_empty()
            && embedding
                .iter()
                .all(|t| t["status"] == "paused" && t["fallback_reason"] == "secrets_tier_b_hold"),
        "R22-1 precondition: the Tier B embedding task must be held: {status}"
    );
    // Rename to a NON-secret live name. The historical secret alias is retained.
    fs::rename(
        dir.path().join("password_notes.md"),
        dir.path().join("notes.md"),
    )
    .unwrap();
    json_success_embed(&dir, "mock", &["index", "--online"]);
    let status = json_success_embed(&dir, "mock", &["status"]);
    let embedding = tasks_of_type(&status, "embedding");
    assert!(
        embedding
            .iter()
            .all(|task| task["status"] == "paused"
                && task["fallback_reason"] == "secrets_tier_b_hold"),
        "a reachable historical secret alias must keep the shared task held: {status}"
    );
}

/// R22-1 NEGATIVE control: while a secret twin is STILL live, the hold must NOT release.
/// The content-addressed dedup keeps the secret path as the survivor, so the single shared
/// task stays held even though a byte-identical non-secret twin is also live — and a plain
/// `batch resume` must not un-hold it either (only `--send-secrets` may).
#[test]
fn r22_1b_secret_hold_survives_while_a_secret_twin_is_live() {
    let dir = tempfile::tempdir().unwrap();
    let body = "# Notes\n\nalpha bravo charlie delta echo foxtrot golf hotel india juliet.\n";
    fs::write(dir.path().join("notes.md"), body).unwrap();
    fs::write(dir.path().join("password_backup.md"), body).unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_embed(&dir, "mock");
    json_success_embed(&dir, "mock", &["index", "--online"]);
    let assert_single_hold = |status: &Value, when: &str| {
        let embedding = tasks_of_type(status, "embedding");
        assert_eq!(
            embedding.len(),
            1,
            "R22-1b ({when}): byte-identical twins must share exactly one embedding task: {status}"
        );
        assert!(
            embedding
                .iter()
                .all(|t| t["status"] == "paused" && t["fallback_reason"] == "secrets_tier_b_hold"),
            "R22-1b ({when}): the shared task must stay held while a secret twin is live: {status}"
        );
    };
    assert_single_hold(
        &json_success_embed(&dir, "mock", &["status"]),
        "after index",
    );
    // A plain `batch resume` must not release the hold (N1: only --send-secrets lifts it).
    json_success_embed(&dir, "mock", &["batch", "resume"]);
    assert_single_hold(
        &json_success_embed(&dir, "mock", &["status"]),
        "after batch resume",
    );
}

/// R22-2 [major]: an existing NON-held embedding task (here Pending/`network_opt_in_required`)
/// must be DEMOTED to a `secrets_tier_b_hold` when the chunk's current path becomes a Tier B
/// secret name. Before the fix the "existing task ⇒ skip" idempotency guard left it Pending
/// forever, so `kio status` and the quarantine record permanently disagreed about the hold.
#[test]
fn r22_2_existing_task_is_demoted_to_hold_when_path_becomes_secret() {
    assert_secret_hold_without_a_ledger(false);
}

#[test]
fn r22_2_secret_hold_preserves_a_claim_without_a_ledger() {
    assert_secret_hold_without_a_ledger(true);
}

fn assert_secret_hold_without_a_ledger(with_retained_claim: bool) {
    let dir = tempfile::tempdir().unwrap();
    let body = "# Plain\n\nalpha bravo charlie delta echo foxtrot golf hotel india juliet.\n";
    fs::write(dir.path().join("plain.md"), body).unwrap();
    kio(&dir, &["init"]).assert().success();
    // Offline index with the embedding adapter configured but no opt-in (`--yes` ⇒
    // network_opt_in=false) → the task is Pending/network_opt_in_required (enqueue-only).
    json_success_embed(&dir, "mock", &["index", "--yes"]);
    let status = json_success_embed(&dir, "mock", &["status"]);
    assert!(
        tasks_of_type(&status, "embedding")
            .iter()
            .any(|t| t["status"] == "pending" && t["fallback_reason"] == "network_opt_in_required"),
        "R22-2 precondition: a Pending/network_opt_in_required embedding task must exist: {status}"
    );
    const CLAIM_ID: &str = "01992840-5c00-7000-8000-000000000001";
    if with_retained_claim {
        // A scope can retain a task claim when the device ledger is unavailable.
        // Reclassification must hold the task without discarding recovery data.
        let store =
            kio_pipeline::task::TaskStore::new(dir.path().join(".kio").canonicalize().unwrap());
        store
            .update_matching(|task| {
                if task.task_type != kio_pipeline::task::TaskType::Embedding {
                    return false;
                }
                task.reservation_id = Some(CLAIM_ID.to_owned());
                task.reserved_usd = Some(0.25);
                task.reserved_month = Some("2026-09".to_owned());
                true
            })
            .unwrap();
    }
    // Rename INTO a Tier B name and re-index: the existing task must demote to a hold.
    fs::rename(
        dir.path().join("plain.md"),
        dir.path().join("credentials_backup.md"),
    )
    .unwrap();
    json_success_embed(&dir, "mock", &["index", "--yes"]);
    let status = json_success_embed(&dir, "mock", &["status"]);
    let embedding = tasks_of_type(&status, "embedding");
    let held = embedding
        .iter()
        .find(|t| t["status"] == "paused" && t["fallback_reason"] == "secrets_tier_b_hold");
    assert!(
        held.is_some(),
        "R22-2: the existing task must be demoted to a secrets hold: {status}"
    );
    assert!(
        held.unwrap()["input_path"]
            .as_str()
            .unwrap()
            .ends_with("credentials_backup.md"),
        "R22-2: the demoted hold must name the current secret path: {status}"
    );
    assert!(
        !embedding
            .iter()
            .any(|t| t["fallback_reason"] == "network_opt_in_required"),
        "R22-2: no task may remain Pending/network_opt_in_required after the demotion: {status}"
    );
    assert!(
        !dir.path()
            .join(".test-data/kio/cost-ledger.sqlite")
            .exists()
    );
}

/// R22-2 NEGATIVE control: a DONE embedding task must NOT be demoted when the path later
/// becomes a Tier B secret name — its vector is real spend that already exists, so demoting
/// it would fake outstanding work and strand the stored vector.
#[test]
fn r22_2b_done_task_is_not_demoted_when_path_becomes_secret() {
    let dir = tempfile::tempdir().unwrap();
    let body = "# Plain\n\nalpha bravo charlie delta echo foxtrot golf hotel india juliet.\n";
    fs::write(dir.path().join("plain.md"), body).unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_embed(&dir, "mock");
    // Online mock index → the embedding is sent and Done.
    json_success_embed(&dir, "mock", &["index", "--online"]);
    let status = json_success_embed(&dir, "mock", &["status"]);
    assert!(
        tasks_of_type(&status, "embedding")
            .iter()
            .any(|t| t["status"] == "done"),
        "R22-2b precondition: the embedding task must be Done: {status}"
    );
    // Rename into a Tier B name and re-index: the Done task must stay Done.
    fs::rename(
        dir.path().join("plain.md"),
        dir.path().join("credentials_backup.md"),
    )
    .unwrap();
    json_success_embed(&dir, "mock", &["index", "--online"]);
    let status = json_success_embed(&dir, "mock", &["status"]);
    let embedding = tasks_of_type(&status, "embedding");
    assert!(
        embedding.iter().any(|t| t["status"] == "done"),
        "R22-2b: the Done embedding task must remain Done after the rename-in: {status}"
    );
    assert!(
        !embedding
            .iter()
            .any(|t| t["fallback_reason"] == "secrets_tier_b_hold"),
        "R22-2b: a Done task must never be demoted to a secrets hold: {status}"
    );
}

/// Step 4 all-history keeps each edited-away secret version eligible. Its hold is
/// real outstanding historical vector work, not an orphan to retire.
#[test]
fn ct4_edited_secret_history_keeps_each_version_held() {
    let dir = tempfile::tempdir().unwrap();
    let v1 = "# Secret\n\nalpha bravo charlie delta echo foxtrot golf hotel india.\n";
    let v2 = "# Secret\n\nkilo lima mike november oscar papa quebec romeo sierra.\n";
    let v3 = "# Secret\n\ntango uniform victor whiskey xray yankee zulu juliet.\n";
    fs::write(dir.path().join("password_notes.md"), v1).unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_embed(&dir, "mock");
    json_success_embed_at(&dir, "mock", "2026-07-03T00:00:00Z", &["index", "--online"]);
    // Edit twice; each edit sends the previous chunk non-live under the same Tier B name.
    fs::write(dir.path().join("password_notes.md"), v2).unwrap();
    json_success_embed_at(&dir, "mock", "2026-07-05T00:00:00Z", &["index", "--online"]);
    fs::write(dir.path().join("password_notes.md"), v3).unwrap();
    json_success_embed_at(&dir, "mock", "2026-07-07T00:00:00Z", &["index", "--online"]);
    let status = json_success_embed_at(&dir, "mock", "2026-07-07T00:00:00Z", &["status"]);
    let embedding = tasks_of_type(&status, "embedding");
    let held = embedding
        .iter()
        .filter(|t| t["status"] == "paused" && t["fallback_reason"] == "secrets_tier_b_hold")
        .count();
    let retired = embedding
        .iter()
        .filter(|t| t["status"] == "failed" && t["fallback_reason"] == "retired_non_live")
        .count();
    assert_eq!(
        held, 3,
        "each retained secret version must stay held for historical vector search: {status}"
    );
    assert_eq!(
        retired, 0,
        "retained historical holds are not non-live: {status}"
    );
    // `index_status` counts all three real historical embedding gaps.
    let search = json_success_embed(&dir, "mock", &["search", "juliet"]);
    assert_eq!(
        search["index_status"]["pending_enrichment_tasks"], 3,
        "all retained historical holds count as pending enrichment: {search}"
    );
}

/// R22-4 [major]: a real binary DOCUMENT whose extension is merely absent from the MIME
/// table (a `.bmp`/`.tiff`/`.heic`/legacy `.doc`) must be DISCLOSED via the
/// `skipped_unrecognized_binary_files` counter (the oversized-input visibility pattern),
/// not silently dropped with a false `enriched_ratio: 1.0`. An all-text scope reports 0.
#[test]
fn r22_4_unrecognized_binary_is_disclosed_not_silently_dropped() {
    let dir = tempfile::tempdir().unwrap();
    // `BM` header + high-bit bytes → a genuine binary (no text layer; folds to
    // application/octet-stream because `.bmp` is not in the MIME table).
    let mut bmp = b"BM".to_vec();
    bmp.extend((0u32..2000).map(|i| (i.wrapping_mul(97) & 0x7f) as u8 | 0x80));
    fs::write(dir.path().join("photo.bmp"), &bmp).unwrap();
    fs::write(
        dir.path().join("ok.md"),
        "# OK\n\nplain readable body alpha bravo charlie.\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    let index = json_success(&dir, &["index", "--yes"]);
    assert_eq!(
        index["skipped_unrecognized_binary_files"], 1,
        "R22-4: the unrecognized binary document must be disclosed, not silently dropped: {index}"
    );
    let unsupported_store = dir.path().join(".kio/unsupported-inputs.jsonl");
    let first_len = fs::metadata(&unsupported_store).unwrap().len();
    let repeated = json_success(&dir, &["index", "--yes"]);
    assert_eq!(repeated["skipped_unrecognized_binary_files"], 1);
    assert_eq!(
        fs::metadata(&unsupported_store).unwrap().len(),
        first_len,
        "R23 CAND-014: an unchanged disposition must not grow the durable store"
    );
    let status = json_success(&dir, &["status"]);
    assert_eq!(status["unsupported_inputs"].as_array().unwrap().len(), 1);
    // NEGATIVE control: a scope of only recognized text reports zero.
    let clean = tempfile::tempdir().unwrap();
    fs::write(
        clean.path().join("ok.md"),
        "# OK\n\nplain readable body charlie delta echo.\n",
    )
    .unwrap();
    kio(&clean, &["init"]).assert().success();
    let clean_index = json_success(&clean, &["index", "--yes"]);
    assert_eq!(
        clean_index["skipped_unrecognized_binary_files"], 0,
        "R22-4 control: an all-text scope must report zero unrecognized binaries: {clean_index}"
    );
}

#[test]
fn r23_cand_014_status_fails_closed_on_corrupt_unsupported_store() {
    let dir = tempfile::tempdir().unwrap();
    kio(&dir, &["init"]).assert().success();
    fs::write(
        dir.path().join(".kio/unsupported-inputs.jsonl"),
        b"{not-json}\n",
    )
    .unwrap();

    let stderr = kio(&dir, &["status"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    assert!(
        String::from_utf8_lossy(&stderr).contains("KIO-E-STORE-CORRUPT-001"),
        "corrupt unsupported-input state must be surfaced"
    );
}

/// A persisted online task without its required bbox stamp is rejected at the
/// task-store boundary. It cannot survive to a resume/send path and therefore
/// cannot cause an OCR charge.
#[test]
fn r22_5_missing_online_bbox_stamp_is_rejected_before_send() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("document.pdf"),
        fake_pdf(&["online markdownize bbox stamp regression"]),
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_markdownize(&dir, "mock");
    // Persist a valid current online task first, then simulate a torn/obsolete
    // task record that omits the required current-format stamp.
    run_markdownize_seam(&dir, "mock", None, &["index", "--online"]);
    let tasks_path = dir.path().join(".kio/tasks.jsonl");
    let original = fs::read_to_string(&tasks_path).unwrap();
    let mut rewritten = String::new();
    let mut changed = false;
    for line in original.lines() {
        let mut task: Value = serde_json::from_str(line).unwrap();
        if !changed && task["type"] == "markdownize" && task["status"] == "pending" {
            task.as_object_mut()
                .unwrap()
                .remove("bbox_annotation_enabled");
            changed = true;
        }
        rewritten.push_str(&serde_json::to_string(&task).unwrap());
        rewritten.push('\n');
    }
    assert!(
        changed,
        "fixture must contain a pending online markdownize task"
    );
    fs::write(&tasks_path, rewritten).unwrap();
    assert_eq!(
        markdown_ledger_rows(&dir),
        0,
        "precondition: indexing must not execute a queued markdownize task"
    );
    let stderr = kio(&dir, &["batch", "resume", "--json"])
        .env(TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV, "mock")
        .assert()
        .code(4)
        .get_output()
        .stderr
        .clone();
    let error: Value = serde_json::from_slice(&stderr).unwrap();
    assert_eq!(error["error_code"], "KIO-E-STORE-CORRUPT-001");
    assert_eq!(
        markdown_ledger_rows(&dir),
        0,
        "a malformed task must add no markdownize charge row"
    );
}

/// R22-6 [major], UPDATED for QA2 (step4b-contract-tests-p3a.md §A, 04 §5.2):
/// R21-6's AuthError live-stuck revive must extend to the MARKDOWNIZE pipeline. A
/// 401/403 now leaves the online task `Paused(hold_reason=auth)` — never `Failed` —
/// so `batch retry` (CT2-TASK-005: `max_attempts=0` is that command's contract, and
/// its task-selection loop is Failed-only, which now trivially excludes a Paused
/// row) must NOT revive it; only `batch resume` ("carry on, credentials fixed") may
/// revive and execute it. This test used to assert `status=="failed"` throughout.
#[test]
fn r22_6_auth_error_markdownize_revives_on_resume_not_retry() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("doc.pdf"),
        fake_pdf(&["認証エラー markdownize 復活の回帰テスト本文です。"]),
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_markdownize(&dir, "mock");
    // Enqueue the online markdownize task (Pending) and grant the network opt-in.
    run_markdownize_seam(&dir, "mock", None, &["index", "--online"]);
    // Send under the auth_error seam → the online task fails auth_error.
    run_markdownize_seam(
        &dir,
        "auth_error",
        Some("2026-07-03T00:00:00Z"),
        &["batch", "resume"],
    );
    let has_paused_auth_error = |status: &Value| {
        tasks_of_type(status, "markdownize").iter().any(|t| {
            t["status"] == "paused"
                && t["hold_reason"] == "auth"
                && t["fallback_reason"] == "auth_error"
        })
    };
    assert!(
        has_paused_auth_error(&run_markdownize_seam(&dir, "mock", None, &["status"])),
        "R22-6 precondition: the online markdownize task must be Paused(hold_reason=auth)"
    );
    // `batch retry` must NOT revive an auth_error task (CT2-TASK-005).
    let retry = run_markdownize_seam(
        &dir,
        "mock",
        Some("2026-07-03T01:00:00Z"),
        &["batch", "retry"],
    );
    assert_eq!(
        retry["tasks_executed"], 0,
        "R22-6: batch retry must not execute the auth_error task: {retry}"
    );
    assert!(
        has_paused_auth_error(&run_markdownize_seam(&dir, "mock", None, &["status"])),
        "R22-6: batch retry must leave the auth_error task Paused (contract of that command)"
    );
    // `batch resume` with fixed credentials (mock) must revive AND execute it.
    let resumed = run_markdownize_seam(
        &dir,
        "mock",
        Some("2026-07-03T02:00:00Z"),
        &["batch", "resume"],
    );
    assert_eq!(
        resumed["tasks_executed"], 1,
        "R22-6: batch resume must revive the auth_error markdownize task and execute it: {resumed}"
    );
}

/// R22-7 [minor]: a scope holding only a Tier B `secrets_tier_b_hold` (with budget to spare)
/// must report `budget_paused: false` — a hold is not a budget pause. Before the fix
/// `compute_index_status` mapped every Paused task to `budget_paused: true`, misdirecting the
/// operator to "raise the budget" instead of `--send-secrets`.
#[test]
fn r22_7_budget_paused_false_for_secrets_hold() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("password_reset_flow.md"),
        "# Reset\n\nalpha bravo charlie delta echo foxtrot golf hotel india juliet.\n",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    approve_embed(&dir, "mock");
    json_success_embed(&dir, "mock", &["index", "--online"]);
    let search = json_success_embed(&dir, "mock", &["search", "juliet"]);
    let index_status = &search["index_status"];
    assert_eq!(
        index_status["budget_paused"], false,
        "R22-7: a secrets hold must not report budget_paused=true: {search}"
    );
    assert_eq!(
        index_status["pending_enrichment_tasks"], 1,
        "R22-7: the single secrets hold must count as pending enrichment: {search}"
    );
}
