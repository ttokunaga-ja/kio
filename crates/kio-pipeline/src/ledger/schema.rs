//! Canonical device-ledger schema and strict existing-state validation.
//! Lifecycle alone creates a fresh schema. Existing tables, constraints and
//! indexes must match exactly; this module never migrates or repairs them.

use std::path::PathBuf;

use rusqlite::{Connection, OptionalExtension, params};

use crate::{PipelineError, Result};

#[cfg(test)]
use crate::ledger::time::now_millis;

/// `04-pipeline.md §5.4` SQL-of-record, copied verbatim (comments included —
/// comments are inert for `CREATE TABLE`/`CREATE INDEX` and are stripped by
/// [`canonical_sql_tokens`] for shape comparison, so keeping them here is a
/// direct, driftable link back to the spec text rather than a paraphrase).
pub const CREATE_COST_LEDGER_SQL: &str = "CREATE TABLE cost_ledger (               -- 確定・推定課金の追記台帳 (行の UPDATE / DELETE 禁止)
    scope_id          TEXT NOT NULL,
    adapter_kind      TEXT NOT NULL,     -- 'markdownize' | 'embedding' | ...
    input_hash        TEXT NOT NULL,     -- §5.5 のタスク同一性キーと同じ組
    tool_profile_hash TEXT NOT NULL,
    submission_seq    INTEGER NOT NULL,  -- 投入の通算連番。**新しい外部投入の開始 (相 1) ごとに
                                         --  MAX+1 を採番** — 同一 attempt の回復中は不変 (§5.8)
    batch_job_id      TEXT NOT NULL,     -- 値規則: 実 job id。job id 不明の記帳 (期限超・abandon) は
                                         --  当該 intent_token (§5.8 の記帳済み判別の突合キー)。
                                         --  sync 呼出 (Batch 非対応 provider) は provider request id、
                                         --  無ければ当該 attempt の intent_token
    usd               REAL NOT NULL      -- estimated=1 の行は保守的な推定額 (NULL 禁止 — SUM が
        CHECK (usd >= 0 AND               --  負値も禁止 (cap の相殺・過少計上を防ぐ)
               usd < 1e999 AND            --  +Inf 拒否 (typeof は Inf を 'real' として通し SUM を汚染する)
               typeof(usd) IN ('integer', 'real')),
                                         --  NULL を無視すると budget 判定が過少 = 安全側の逆になる。
                                         --  typeof 検査: REAL affinity は TEXT 混入を通し SUM が 0.0
                                         --  扱いにする = cap 過少計上のため型も強制する
    estimated         INTEGER NOT NULL DEFAULT 0 CHECK (estimated IN (0, 1)),
    outcome           TEXT NOT NULL      -- DEFAULT を持たない — INSERT での明示を必須にする
        CHECK (outcome IN ('succeeded', 'contract_violation', 'expired', 'abandoned',
                           'submit_rejected', 'purged', 'unknown_settled',
                           'fallback_to_full')),
                                         -- 終端確定行の到達理由 (§5.8 の対応表と同一 Tx で必須記載。
                                         --  DEFAULT 'succeeded' を許すと省略記帳が成功に化け、
                                         --  ON CONFLICT 冪等の下で訂正不能になる)。
                                         --  reset (--reset-violations) 後も違反履歴が台帳に恒久に残る
    month             TEXT NOT NULL      -- 'YYYY-MM' (確定月配賦 — cap 集計キー。書式と月範囲も強制 —
        CHECK (month GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]'
               AND substr(month, 6, 2) BETWEEN '01' AND '12'),
                                         --  不正書式・00/13〜99 月は当月合算から漏れ cap を過少判定する)
    recorded_at       INTEGER NOT NULL,  -- UTC ミリ秒
    UNIQUE (scope_id, adapter_kind, input_hash, tool_profile_hash, submission_seq)
);";

pub const CREATE_IDX_COST_LEDGER_MONTH_SQL: &str =
    "CREATE INDEX idx_cost_ledger_month ON cost_ledger(month, scope_id, adapter_kind);";

pub const CREATE_BATCH_REQUESTS_SQL: &str = "CREATE TABLE batch_requests (            -- in-flight Batch intent の正本 (§5.8 の状態機械)
    scope_id          TEXT NOT NULL,
    adapter_kind      TEXT NOT NULL,
    input_hash        TEXT NOT NULL,
    tool_profile_hash TEXT NOT NULL,
    state             INTEGER NOT NULL DEFAULT 0
        CHECK (state IN (0, 1, 2, 3)),   -- 0=投入前/中 1=job 作成済み 2=完了 3=terminal error
    request_kind      TEXT NOT NULL DEFAULT 'batch'
        CHECK (request_kind IN ('batch', 'sync')),
                                         -- 縮退 2 相 (sync online) 行の判別 (§5.4)。回復の適用規則を
                                         --  分岐する — sync 行は job/upload 照合・猶予・期限の対象外
    intent_token      TEXT,              -- UUIDv7 (相 1 で発行)。NULL 化は残骸掃除の完了時のみ (§5.8)
    upload_id         TEXT,              -- 相 2a 成功直後に記録
    batch_job_id      TEXT,              -- 相 2b 成功後・または回復の found 自己記述化で記録。
                                         --  sync 行では provider request id (応答受信直後に記録 — §5.4)
    provider_scope_id TEXT,              -- 相 2a の upload 直前に記録 (§5.8 手順 2 — 非 NULL は
                                         --  「相 2a 着手」の印)。相 1 の再発行で NULL へ戻る (手順 1)
    job_create_started_at INTEGER,       -- UTC ミリ秒。batch 行 = 可視化猶予・回復期限の起点 (§5.8)。
                                         --  sync 行 = 相 1 の開始時刻 (bounded sweep の選択順キー — §5.4)
    stale_after_at    INTEGER,           -- UTC ミリ秒。sync 行のみ: 相 1 で耐久保存する回収期限 (§5.4 —
                                         --  実効 timeout の最大値 + 60 秒、下限 600 秒。Retry-After 受信で
                                         --  自 token CAS により延長)。batch 行は NULL (§5.8 の期限が担う)。
                                         --  列追加の migration は既存の未終端 sync 行へ backfill が必須
                                         --  (10 §7.5.3 の例外規範 — NULL 残置は回収から恒久に漏れる)
    submission_seq    INTEGER NOT NULL DEFAULT 0,
                                         -- 行 (再) 作成時は cost_ledger 同キーの MAX(submission_seq)
                                         --  から継承する (通算連番の高水位の正本は ledger — 0 から
                                         --  数え直すと既存記帳と UNIQUE 衝突する)
    attempts          INTEGER NOT NULL DEFAULT 0,
    contract_violation_count INTEGER NOT NULL DEFAULT 0,
                                         -- reject 終端 Tx (§5.8 相 3) で increment。相 1 の NULL 戻しの
                                         --  対象外 — 「同一 mode で 1 回のみ」の durable 判定源
    estimated_usd     REAL NOT NULL      -- budget 予約額 (§5.4 判定式)。相 1 作成時に保守見積を必須設定
        CHECK (estimated_usd >= 0 AND    --  (NULL/負を許すと SUM が予約を取りこぼし cap を過少判定。
               estimated_usd < 1e999 AND --  +Inf 拒否 (cost_ledger.usd と同じ理由)
               typeof(estimated_usd) IN ('integer', 'real')),
                                         --   typeof 検査は cost_ledger.usd と同じ理由)
    error             TEXT,              -- 'submit_rejected' | 'expired' | 'abandoned' | ...
                                         --  拒否課金 provider (07 §5.5 条件 6) の submit_rejected は
                                         --  terminal 化と同一 Tx で記帳 (Adapter 返却の usage
                                         --  (usd = 宣言請求額 | billable_units — 07 §4) が有効なら
                                         --  provider 値 (estimated=0)、無効・欠落は行の estimated_usd
                                         --  を estimated=1 で — §5.4 の事前検証。
                                         --  ledger 0 行のままの terminal 化を許さない)
    completed_at      INTEGER,           -- state を 2/3 へ確定する全ての UPDATE で同時に書く。
                                         --  未終端は NULL (status の滞留検知に使う)
    created_at        INTEGER NOT NULL,
    PRIMARY KEY (scope_id, adapter_kind, input_hash, tool_profile_hash)
) WITHOUT ROWID;";

pub const CREATE_IDX_BATCH_REQUESTS_INFLIGHT_SQL: &str =
    "CREATE INDEX idx_batch_requests_inflight ON batch_requests(state) WHERE state IN (0, 1);";

pub const CREATE_SCHEMA_MIGRATIONS_SQL: &str =
    "CREATE TABLE schema_migrations (         -- durable operational markers
    name        TEXT NOT NULL PRIMARY KEY,
    applied_at  INTEGER NOT NULL         -- UTC ミリ秒
);";

/// Immutable identity bound to the authority and checkpoint records.  The
/// SQLite header's `user_version` is intentionally not used as a counter.
pub const CREATE_LEDGER_METADATA_SQL: &str = "CREATE TABLE ledger_metadata (
    singleton INTEGER NOT NULL PRIMARY KEY CHECK (singleton = 1),
    ledger_id TEXT NOT NULL,
    era TEXT NOT NULL,
    sequence INTEGER NOT NULL CHECK (sequence >= 0),
    security_token TEXT NOT NULL
) WITHOUT ROWID;";

/// Retired pre-release ledger files, including the abandoned cutover's
/// `.migrated` outputs. Their bytes have no lossless mapping to the current
/// SQLite schema, so startup refuses them rather than importing, renaming, or
/// otherwise modifying them.
pub const RETIRED_LEDGER_BASENAMES: &[&str] = &[
    "cost-ledger.jsonl",
    "cost-ledger-reservations.jsonl",
    "cost-ledger-reclaimed.jsonl",
    "cost-ledger.lock",
    "cost-ledger.jsonl.migrated",
    "cost-ledger-reservations.jsonl.migrated",
    "cost-ledger-reclaimed.jsonl.migrated",
    "cost-ledger.lock.migrated",
];

/// `$XDG_DATA_HOME/kio/cost-ledger.sqlite`, falling back to
/// `$HOME/.local/share/kio/cost-ledger.sqlite` (04 §5.4: "デバイスグローバル 1 個").
/// Mirrors `kio_index::registry::default_registry_path`'s XDG resolution exactly.
pub fn default_ledger_path() -> Result<PathBuf> {
    let data_home = kio_core::xdg::xdg_dir("XDG_DATA_HOME")
        .or_else(|| kio_core::xdg::home_dir().map(|home| home.join(".local/share")))
        .ok_or_else(|| {
            PipelineError::Schema(
                "cannot resolve an absolute user data directory; refusing a CWD-relative cost ledger"
                    .to_owned(),
            )
        })?;
    Ok(data_home.join("kio/cost-ledger.sqlite"))
}

/// A historical restore-reconcile marker.  It is a refusal artifact only:
/// ordinary lifecycle work never clears or repairs it, because missing remote
/// spend cannot be proven from a local SQLite file.
pub const RESTORE_RECONCILE_PENDING_MARKER: &str = "restore-reconcile-pending";

/// Whether the QA14 restore-reconcile marker is currently set — the gate
/// `ops::phase1_intent` checks before issuing a new submission.
pub(crate) fn restore_reconcile_marker_present(conn: &Connection) -> Result<bool> {
    marker_present(conn, RESTORE_RECONCILE_PENDING_MARKER)
}

/// Returns whether a named operational marker is durable in this database.
pub(crate) fn marker_present(conn: &Connection, name: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM schema_migrations WHERE name = ?1",
        params![name],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Test fixture helper for the historical refusal marker.  Runtime code has
/// no generic marker writer and cannot clear or repair this artifact.
#[cfg(test)]
pub(crate) fn record_marker(conn: &Connection, name: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO schema_migrations (name, applied_at) VALUES (?1, ?2)",
        params![name, now_millis()],
    )?;
    Ok(())
}

/// Create the complete schema only while an explicit initialization owns a
/// fresh database.  Ordinary opens never create, migrate, or repair objects.
pub(crate) fn create_fresh_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(CREATE_COST_LEDGER_SQL)?;
    conn.execute_batch(CREATE_IDX_COST_LEDGER_MONTH_SQL)?;
    conn.execute_batch(CREATE_BATCH_REQUESTS_SQL)?;
    conn.execute_batch(CREATE_IDX_BATCH_REQUESTS_INFLIGHT_SQL)?;
    conn.execute_batch(CREATE_SCHEMA_MIGRATIONS_SQL)?;
    conn.execute_batch(CREATE_LEDGER_METADATA_SQL)?;
    Ok(())
}

/// Strictly validate every required table and index.  This intentionally has
/// no recovery path: a missing or changed object is recovery-required.
pub(crate) fn validate_schema(conn: &Connection) -> Result<()> {
    detect_table_shape_mismatch(conn)?;
    for (name, sql) in [
        ("idx_cost_ledger_month", CREATE_IDX_COST_LEDGER_MONTH_SQL),
        (
            "idx_batch_requests_inflight",
            CREATE_IDX_BATCH_REQUESTS_INFLIGHT_SQL,
        ),
    ] {
        let actual = object_sql(conn, "index", name)?.ok_or_else(|| {
            PipelineError::contract(
                "KIO-E-LEDGER-SCHEMA-001",
                format!("required index {name} is missing"),
            )
        })?;
        if canonical_sql_tokens(&actual) != canonical_sql_tokens(sql) {
            return Err(PipelineError::contract(
                "KIO-E-LEDGER-SCHEMA-001",
                format!("required index {name} shape differs"),
            ));
        }
    }
    Ok(())
}

/// Validate all four required tables against their canonical DDL, including
/// constraints. A missing or differently shaped table is corrupt/unsupported
/// operational truth and cannot be adopted as an empty accounting baseline.
fn detect_table_shape_mismatch(conn: &Connection) -> Result<()> {
    for (table_name, create_sql) in [
        ("cost_ledger", CREATE_COST_LEDGER_SQL),
        ("batch_requests", CREATE_BATCH_REQUESTS_SQL),
        ("schema_migrations", CREATE_SCHEMA_MIGRATIONS_SQL),
        ("ledger_metadata", CREATE_LEDGER_METADATA_SQL),
    ] {
        let current = object_sql(conn, "table", table_name)?.ok_or_else(|| {
            PipelineError::corrupt(
                table_name,
                format!(
                    "cost-ledger.sqlite is missing table `{table_name}` while other \
                     cost-ledger.sqlite tables already exist (10-operations.md §7.5.3 shape \
                     detection) — a legitimate store always creates all 3 tables together in one \
                     savepoint; this is a torn or hand-edited store, not a supported partial shape."
                ),
            )
        })?;
        if canonical_sql_tokens(&current) != canonical_sql_tokens(create_sql) {
            return Err(PipelineError::corrupt(
                table_name,
                format!(
                    "cost-ledger.sqlite table `{table_name}` shape does not match this build's \
                     DDL-of-record (04-pipeline.md §5.4 SQL 正本) — refusing to open a store whose \
                     row invariants this build cannot verify (10-operations.md §7.5.3 canonical \
                     shape detection: table/column/CHECK constraint comparison). In-place \
                     table-shape migration is not implemented; recovery requires a \
                     schema-compatible build."
                ),
            ));
        }
    }
    Ok(())
}

/// The literal `sql` text sqlite_master stores for a table/index, or `None` if
/// it does not exist.
pub fn object_sql(conn: &Connection, kind: &str, name: &str) -> Result<Option<String>> {
    conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type = ?1 AND name = ?2",
        rusqlite::params![kind, name],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

/// Named-savepoint helper (same idiom as `kio_index::embedding_store`'s
/// `with_savepoint` / `kio_index::fts`'s — duplicated locally per this
/// codebase's existing convention of not sharing this tiny helper cross-crate).
pub(crate) fn with_savepoint<T>(
    conn: &Connection,
    name: &str,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    conn.execute_batch(&format!("SAVEPOINT {name};"))?;
    match operation() {
        Ok(value) => {
            conn.execute_batch(&format!("RELEASE {name};"))?;
            Ok(value)
        }
        Err(err) => {
            let _ = conn.execute_batch(&format!("ROLLBACK TO {name}; RELEASE {name};"));
            Err(err)
        }
    }
}

/// Normalize SQL text into a token stream for canonical-shape comparison (10
/// §7.5.3: "形状検出は sqlite_master の CREATE 文...の canonical 比較で行う").
/// Strips `--` line comments, then splits on whitespace while making `(`, `)`,
/// `,` their own tokens (so `typeof(usd)` and `typeof (usd)` compare equal, but
/// `usd>=0` and `usd >= 0` are unaffected since `>`/`=` are not punctuation
/// boundaries here — the DDL-of-record always spaces its operators, so exact
/// token equality on those still holds byte-for-byte). `;` is dropped entirely
/// (treated as a separator, not a token) — `sqlite_master.sql` never stores the
/// terminating semicolon of the statement it was created from, so keeping it as
/// a token would make every comparison against a hand-authored DDL constant
/// (which does end in `;`) spuriously mismatch.
#[must_use]
pub fn canonical_sql_tokens(sql: &str) -> Vec<String> {
    let mut without_comments = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '-' && chars.peek() == Some(&'-') {
            chars.next();
            for c2 in chars.by_ref() {
                if c2 == '\n' {
                    without_comments.push('\n');
                    break;
                }
            }
            continue;
        }
        without_comments.push(c);
    }
    let mut tokens = Vec::new();
    let mut current = String::new();
    for c in without_comments.chars() {
        if c.is_whitespace() || c == ';' {
            // `;` is a separator like whitespace, never a token of its own —
            // see the doc comment above.
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        } else if matches!(c, '(' | ')' | ',') {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            tokens.push(c.to_string());
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        create_fresh_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn fresh_schema_has_all_authority_bound_objects() {
        let conn = fresh();
        validate_schema(&conn).unwrap();
        for table in [
            "cost_ledger",
            "batch_requests",
            "schema_migrations",
            "ledger_metadata",
        ] {
            assert!(
                object_sql(&conn, "table", table).unwrap().is_some(),
                "{table}"
            );
        }
        for index in ["idx_cost_ledger_month", "idx_batch_requests_inflight"] {
            assert!(
                object_sql(&conn, "index", index).unwrap().is_some(),
                "{index}"
            );
        }
    }

    #[test]
    fn strict_schema_validation_refuses_missing_index() {
        let conn = fresh();
        conn.execute_batch("DROP INDEX idx_batch_requests_inflight;")
            .unwrap();
        let error = validate_schema(&conn).unwrap_err();
        assert!(matches!(
            error,
            PipelineError::Contract {
                code: "KIO-E-LEDGER-SCHEMA-001",
                ..
            }
        ));
    }

    #[test]
    fn strict_schema_validation_refuses_malformed_metadata_table() {
        let conn = fresh();
        conn.execute_batch("DROP TABLE ledger_metadata; CREATE TABLE ledger_metadata (singleton INTEGER PRIMARY KEY);").unwrap();
        assert!(matches!(
            validate_schema(&conn),
            Err(PipelineError::Corrupt { .. })
        ));
    }

    #[test]
    fn metadata_sequence_is_a_signed_sqlite_value() {
        let conn = fresh();
        conn.execute("INSERT INTO ledger_metadata (singleton, ledger_id, era, sequence, security_token) VALUES (1, 'a', 'b', ?1, 'c')", params![i64::MAX]).unwrap();
        let value: i64 = conn
            .query_row(
                "SELECT sequence FROM ledger_metadata WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(value, i64::MAX);
    }

    #[test]
    fn canonical_sql_tokens_ignores_comments_and_layout() {
        assert_eq!(
            canonical_sql_tokens("CREATE INDEX foo ON t(a); -- comment\n"),
            canonical_sql_tokens("CREATE INDEX foo ON t(a);")
        );
    }
}
