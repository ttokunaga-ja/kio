# Step4b 契約テスト仕様書: cost-ledger.sqlite + Online Batch 2 相プロトコル

> **Historical record, non-authorizing.** 現行 authority は本文が引用する canonical docs と Rust tests に限る。ID は review provenance のためだけに残し、compatibility、migration、CLI、schema、future work を authorize しない。

> 本書は **実装より先にテストを固定する** ためのケース仕様。Rust 実装コードは含まない。
> 正本 spec は `docs/04-pipeline.md` **§5.4 (Cost Guardrail / Kill Switch)** と **§5.8 (Online Batch
> 投入の 2 相プロトコル)** — この 2 節が契約の源泉であり、期待値はすべてこの 2 節 (および 2 節が
> 明示的に参照する隣接節) の規範文から導く。ID 体系・優先度規約・「未定義/曖昧の切り出し」方針は
> `tasks/step2a-contract-tests.md (git 56a6831)` と現行 Rust behavior tests から継承するが、
> 各契約の記法は本タスクの指示書 (`step4b-ledger-contract-instructions.md`) が定める
> `### CL<連番> ... - 正本 / 前提 / 操作 / 期待` 形式に従う (自己完結)。

**対象 U 項目**: 当時の gap inventory の **U5, U6, U7, U8, U9, U10, U11, U1 (abandon 部分のみ)**。
U1 のうち `hold_reason` 3 値 enum・`paused`/`pending+next_retry_at` の分離自体は 04 §5.1 契約であり
本書の対象外 (別ロットの契約テストが担う) — 本書が U1 から取り込むのは **`kio batch abandon` CLI と
`stalled` 表示**の部分のみ。U2/U3/U4/U12 は対象外 (U4 の budget cap 判定式 (check-then-reserve /
candidate=0 免除 / per_adapter device 限定) は 04 §5.4 の同一パラグラフに同居するため、指示書の
網羅領域 9 (budget cap) としてやむを得ず一部重複して契約化する — 該当契約の「正本」に明記する)。

**実装対象ファイルの見込み** (契約の対象であり実装方針を指図するものではない — 現状把握の記録):

- `crates/kio-pipeline/src/budget.rs` — 全面置換対象。現状は `CostLedger` / `ReservationLedger` が
  `cost-ledger.jsonl` + `cost-ledger-reservations.jsonl` + `cost-ledger-reclaimed.jsonl` +
  `*.lock` の JSONL 3 ファイル構成 (旧仕様、2026-07-18 に spec 側では廃止済み)。`kio-pipeline` は
  現在 `rusqlite` に依存していない (`Cargo.toml` 確認済み) — 新設 SQLite ストアをこのクレートに置くなら
  依存追加が要る。`rusqlite 0.32` はワークスペース共通依存で `kio-cli` / `kio-index` が既に使用
  (`Cargo.toml` L31, `crates/kio-index/Cargo.toml`)
- `crates/kio-pipeline/src/task.rs` — `TaskDescriptor` の `reservation_id`/`reserved_usd`/
  `reserved_month` (JSONL 予約 claim の残骸) は SQLite 移行後は不要になる可能性が高いが、本書は
  ledger 側の契約のみを扱うため task.rs 側の変更要否は判定しない
- `crates/kio-cli/src/main.rs` — `cost_ledger_path()` (L13438-13440、現状
  `data_home().join("kio/cost-ledger.jsonl")`) の拡張子変更、F8 予約フロー (L12487 付近) の
  `BEGIN IMMEDIATE` Tx への置換、`BatchCommand` enum (L264-268、現状 `Resume`/`Retry` のみ) への
  `Abandon` 追加、`kio batch retry --reset-violations` フラグ追加、`kio status` の `stalled` 表示、
  `kio search` vector|hybrid page 1 の device 行書込み配線
- `crates/kio-index/src/registry.rs` — 直接の変更対象ではないが、device-global SQLite
  (`~/.local/share/kio/scope-registry.sqlite`、WAL + busy_timeout、`kio_core::xdg::xdg_dir`/
  `home_dir` によるパス解決) の**先例実装**として新設 `cost-ledger.sqlite` ストアの参考になる
- `crates/kio-core/src/xdg.rs` — 変更不要の見込み (`xdg_dir`/`home_dir` は既に
  `main.rs::data_home()` が利用しており、`cost-ledger.sqlite` パス解決にもそのまま使える)

---

## 0. ID 体系と優先度

| 接頭辞 | 対象契約領域 | 主根拠 |
| --- | --- | --- |
| CL01-CL08 (§A) | 3 表 DDL (列/型/CHECK/UNIQUE/index の canonical 一致) | 04 §5.4 DDL ブロック (L773-871) |
| CL09-CL12 (§B) | JSONL→SQLite import の 2 相移行プロトコル | 10 §7.5.3 / 04 §5.4 |
| CL13-CL21 (§C) | Online Batch 2 相の状態機械 (相1/2a/2b/3) | 04 §5.8 手順 (L955-1003) |
| CL22-CL25 (§D) | 記帳の冪等性 (ON CONFLICT DO NOTHING / seq+1) | 04 §5.8 (L1005-1016) |
| CL26-CL31 (§E) | outcome 閉 enum + Adapter 報告値の事前検証 | 04 §5.8 (L1018-1047) |
| CL32-CL40 (§F) | crash 回収 (found/confirmed-absent/unknown/恒久) | 04 §5.8 (L950-953, L1049-1091) |
| CL41-CL47 (§G) | sync online 呼出の縮退 2 相 + idempotency 二段構え | 04 §5.4 (L768) / §5.5 (L880) |
| CL48-CL55 (§H) | query embedding device 行 (stale_after_at/sweep/剪定) | 04 §5.4 (L769) |
| CL56-CL61 (§I) | budget cap (check-then-reserve/candidate=0/per_adapter) | 04 §5.4 (L767-768) |
| CL62-CL68 (§J) | `kio batch abandon` CLI (U1 部分) | 04 §5.8 (L1075-1084) / 06 §1 (L44-51) |
| CL69-CL71 (§K) | 横断規約 (error code / 配置 / リネーム grep) | 06 §7 (L343) / 10 §12.4, §12.7 |

**優先度**: **P0** = このロットの完了条件、1 件でも failing なら「cost-ledger.sqlite 移行完了」と呼べない。
**P1** = 推奨 (堅牢性・resource hygiene・観測性)。落ちても致命ではないが実装欠陥の強い兆候。

P0/P1 集計は末尾「集計」節。

---

## A. DDL 契約 (3 表 canonical 一致)

> 04 §5.4 (L771): 「store は 3 表で構成し、**以下の DDL を SQL 正本とする**」。canonical 一致の検証手段は
> 10 §7.5.3: 「形状検出は sqlite_master の CREATE 文 (列・CHECK 制約を含む) の canonical 比較で行う —
> 対象は `cost_ledger` / `batch_requests` / `schema_migrations` の 3 表すべて」「必須 index
> (`idx_cost_ledger_month`・`idx_batch_requests_inflight`) も検出対象とする」。

### CL01 cost_ledger の列集合と型が正本 DDL と canonical 一致 [P0]
- 正本: 04 §5.4 L774-807 (`CREATE TABLE cost_ledger (...)` 全文) / 10 §7.5.3 L700-702 (『形状検出は
  sqlite_master の CREATE 文 (列・CHECK 制約を含む) の canonical 比較で行う』)
- 前提: 空の `:memory:` (または一時ファイル) SQLite 接続。
- 操作: 04 §5.4 の `CREATE TABLE cost_ledger` 文を一字一句そのまま実行する。`SELECT sql FROM
  sqlite_master WHERE type='table' AND name='cost_ledger'` を取得。
- 期待: 列は順に `scope_id TEXT NOT NULL, adapter_kind TEXT NOT NULL, input_hash TEXT NOT NULL,
  tool_profile_hash TEXT NOT NULL, submission_seq INTEGER NOT NULL, batch_job_id TEXT NOT NULL,
  usd REAL NOT NULL CHECK(...), estimated INTEGER NOT NULL DEFAULT 0 CHECK(...), outcome TEXT NOT
  NULL CHECK(...), month TEXT NOT NULL CHECK(...), recorded_at INTEGER NOT NULL` の 11 列 + 末尾
  `UNIQUE (scope_id, adapter_kind, input_hash, tool_profile_hash, submission_seq)`。実装が生成した
  `sqlite_master.sql` を空白・コメントを正規化 (トークン列を比較) した上で、この正本文字列と
  **完全一致**すること (コメントの有無は比較対象外、CHECK 式・型・NOT NULL・DEFAULT・UNIQUE の
  トークン列は完全一致必須)。

### CL02 cost_ledger.usd / batch_requests.estimated_usd の非負・有限 CHECK (パラメタ化) [P0]
- 正本: 04 §5.4 L785-791 (『CHECK (usd >= 0 AND usd < 1e999 AND typeof(usd) IN ('integer', 'real'))』)
  と L846-850 (`estimated_usd` に同型の CHECK。コメント『typeof 検査は cost_ledger.usd と同じ理由』)
- 前提: CL01 の `cost_ledger` と対応する `batch_requests` (CL04) が作成済み。
- 操作: 各表に、他の必須列はダミーの正当値で埋めた上で対象列 (`usd` / `estimated_usd`) を次の値で
  INSERT する: (a) `0`、(b) `0.01`、(c) `1e308` (有限最大級)、(d) `-0.01`、(e) `NaN` (SQLite 経由で
  IEEE754 NaN を注入)、(f) `+Inf`、(g) `1e999` (SQLite 定数解析で Inf になる)、(h) 数値に見える TEXT
  リテラル `'5.0'` を REAL affinity 列へ直接 bind (typeof が 'text' のまま挿入されるケースを模擬)。
- 期待: (a)(b)(c) は成功。(d)(e)(f)(g) は SQLite の CHECK 制約違反で INSERT が失敗する
  (`SQLITE_CONSTRAINT_CHECK`)。(h) も `typeof(...) IN ('integer','real')` に違反し失敗する — 列の
  REAL affinity だけでは TEXT 混入を防げない (コメント根拠: L789-791 『REAL affinity は TEXT 混入を
  通し SUM が 0.0 扱いにする = cap 過少計上のため型も強制する』) ことを実機で確認する。

### CL03 cost_ledger.estimated / outcome / month の CHECK (パラメタ化) [P0]
- 正本: 04 §5.4 L792 (`estimated INTEGER NOT NULL DEFAULT 0 CHECK (estimated IN (0, 1))`)、
  L793-800 (`outcome` 8 値 CHECK、DEFAULT なし)、L801-804 (`month` の GLOB + 月範囲 CHECK)
- 前提: CL01 の `cost_ledger`。
- 操作: (1) `estimated` に `2`, `-1`, `'1'` (TEXT) を INSERT。(2) `outcome` を省略して INSERT (他の列は
  正当値)。(3) `outcome='invalid_value'` で INSERT。(4) `outcome` に 8 値それぞれで INSERT (他 CL では
  すでに個別に確認するため、ここでは membership のみ)。(5) `month` に `'2026-13'`, `'2026-00'`,
  `'26-07'`, `'2026-7'`, `'2026/07'` を INSERT。
- 期待: (1) 全て CHECK 違反で失敗 (`estimated` は 0/1 のみ)。(2) `outcome` を省略した INSERT は
  **NOT NULL 違反で失敗** — DEFAULT が無いため `NULL` になり NOT NULL 制約に落ちる (コメント根拠:
  L793 『DEFAULT を持たない — INSERT での明示を必須にする』)。(3) CHECK 違反で失敗。(4) 8 値すべて
  成功。(5) 5 パターン全て `month GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]' AND substr(month,6,2)
  BETWEEN '01' AND '12'` に違反し失敗する。

### CL04 batch_requests の列集合・型・WITHOUT ROWID・PRIMARY KEY が正本 DDL と canonical 一致 [P0]
- 正本: 04 §5.4 L814-862 (`CREATE TABLE batch_requests (...) WITHOUT ROWID;` 全文)
- 前提: 空の SQLite 接続。
- 操作: 04 §5.4 の `CREATE TABLE batch_requests` 文をそのまま実行し `sqlite_master.sql` を取得。
- 期待: 列は `scope_id, adapter_kind, input_hash, tool_profile_hash, state, request_kind,
  intent_token, upload_id, batch_job_id, provider_scope_id, job_create_started_at, stale_after_at,
  submission_seq, attempts, contract_violation_count, estimated_usd, error, completed_at,
  created_at` の 19 列。`PRIMARY KEY (scope_id, adapter_kind, input_hash, tool_profile_hash)`。
  テーブル定義末尾に `WITHOUT ROWID`。`month` 相当の列は**存在しない** (CL56 で扱う予約合算は月概念を
  持たない未終端行の無条件合算であり month 列を必要としない)。`error` 列に **CHECK 制約が無い**こと
  (state/request_kind/estimated_usd には CHECK があるのに `error` には無い非対称 — §K の CL71 で
  設計意図を注記)。canonical トークン列が正本と完全一致。

### CL05 batch_requests.state / request_kind の CHECK とデフォルト値 (パラメタ化) [P0]
- 正本: 04 §5.4 L819-820 (`state INTEGER NOT NULL DEFAULT 0 CHECK (state IN (0, 1, 2, 3))`)、
  L821-822 (`request_kind TEXT NOT NULL DEFAULT 'batch' CHECK (request_kind IN ('batch', 'sync'))`)
- 前提: CL04 の `batch_requests`。
- 操作: (1) `state`/`request_kind` を省略して INSERT (他の必須列は正当値)。(2) `state=4`,
  `state=-1` で INSERT。(3) `request_kind='async'` で INSERT。
- 期待: (1) `state=0`・`request_kind='batch'` が既定値として入る (DEFAULT 適用を確認)。(2)(3) は
  CHECK 違反で失敗。

### CL06 schema_migrations の列集合が正本 DDL と canonical 一致 [P0]
- 正本: 04 §5.4 L867-870 (`CREATE TABLE schema_migrations (name TEXT NOT NULL PRIMARY KEY,
  applied_at INTEGER NOT NULL);`)
- 前提: 空の SQLite 接続。
- 操作: 文をそのまま実行し `sqlite_master.sql` を取得。同一 `name` で 2 回 INSERT する。
- 期待: 列は `name TEXT NOT NULL PRIMARY KEY` / `applied_at INTEGER NOT NULL` の 2 列のみ。2 回目の
  INSERT は主キー制約違反で失敗 (marker は 1 回きりの記録 — コメント根拠: L867 『一度きりの移行の
  marker』)。rowid 表であるにも関わらず `name` に明示 `NOT NULL` が付いていること (コメント根拠:
  L868 『rowid 表の TEXT PRIMARY KEY は NULL を拒否しないため NOT NULL 必須』)。

### CL07 必須 index (idx_cost_ledger_month / idx_batch_requests_inflight) の canonical 一致 [P0]
- 正本: 04 §5.4 L808 (`CREATE INDEX idx_cost_ledger_month ON cost_ledger(month, scope_id,
  adapter_kind);`)、L863 (`CREATE INDEX idx_batch_requests_inflight ON batch_requests(state) WHERE
  state IN (0, 1);`) / 10 §7.5.3 L703-705 (『必須 index... も検出対象とする』)
- 前提: CL01・CL04 の 2 表が作成済み。
- 操作: 2 つの `CREATE INDEX` 文を実行。`SELECT sql FROM sqlite_master WHERE type='index' AND name
  IN ('idx_cost_ledger_month','idx_batch_requests_inflight')` を取得。
- 期待: `idx_cost_ledger_month` は `(month, scope_id, adapter_kind)` の複合 index。
  `idx_batch_requests_inflight` は `state` 列への **部分 index** (`WHERE state IN (0, 1)`) —
  `EXPLAIN QUERY PLAN SELECT SUM(estimated_usd) FROM batch_requests WHERE state IN (0,1)` が
  `idx_batch_requests_inflight` を使用し、`state=2` 行を含む全表 SCAN にならないことを確認する
  (コメント根拠: L864-865 『生涯 task 数に依存させない partial index』)。両 index の `sql` が canonical
  トークン列で正本と完全一致。

### CL08 shape-mismatch (欠落 index / 異形 index) の検出と補完 [P0]
- 正本: 10 §7.5.3 L703-708 (『欠落は同一 savepoint 内で CREATE INDEX IF NOT EXISTS により補完して
  schema_migrations へ記録する』『同名で定義が canonical と一致しない index は... DROP INDEX →
  canonical CREATE INDEX の再作成で収束させ、同様に記録する』)
- 前提: CL01・CL04 のテーブルは正本どおり作成済みだが、`idx_batch_requests_inflight` が (a) 存在しない
  場合、(b) `ON batch_requests(state)` (WHERE 句なし = 異形) で存在する場合、の 2 パターン。
- 操作: shape 検出ルーチンを実行 (両パターンについて)。
- 期待: (a) `CREATE INDEX IF NOT EXISTS idx_batch_requests_inflight ...` が同一 savepoint 内で実行され、
  `schema_migrations` に完了が記録される。(b) 既存の異形 index が `DROP INDEX` された上で canonical
  版が再作成され、同様に記録される (`IF NOT EXISTS` だけでは同名異形を直さないことをコメントが明記 —
  L708 『IF NOT EXISTS は同名異形を修復しない』)。両パターンとも実行後の `sqlite_master.sql` が CL07 の
  正本と完全一致する。

---

## B. JSONL → SQLite import の 2 相移行

> 10 §7.5.3 L692-699: 「旧 JSONL 3 ファイル構成からの移行も... 一度だけ行う」「移行は 2 相: (1) SQLite
> への import と `schema_migrations` 表への marker 行 (name='jsonl-cutover') の確定を同一 Tx で行い →
> (2) 旧 JSONL を `.migrated` へ rename する」「再開時は marker の存在で import を skip し rename のみ
> 再試行する」。

### CL09 import + marker 行の同一 Tx コミット (相 1) [P0]
- 正本: 10 §7.5.3 L696-698 (『(1) SQLite への import と schema_migrations 表... への marker 行
  (name='jsonl-cutover') の確定を同一 Tx で行い』)
- 前提: 旧 `cost-ledger.jsonl` (課金確定行 N 件) + `cost-ledger-reservations.jsonl` (未消費予約 M 件)
  + `cost-ledger-reclaimed.jsonl` が存在し、新 SQLite ファイルは未作成。
- 操作: 移行ルーチンを実行するが、`schema_migrations` へのマーカー行 INSERT の直前でプロセスを
  kill (Tx 未コミットのままクラッシュさせる)。
- 期待: 再起動後に SQLite ファイルを検査すると `cost_ledger` は空 (Tx がロールバックされ import 行は
  一切残らない — 部分 import が残らないことを確認)。`schema_migrations` に `jsonl-cutover` 行が
  無い。旧 JSONL 3 ファイルは rename されず元のまま残っている (相 2 に進んでいない)。

### CL10 import 冪等性 (marker 存在時は import を skip、rename のみ再試行) [P0]
- 正本: 10 §7.5.3 L698-699 (『再開時は marker の存在で import を skip し rename のみ再試行する —
  savepoint は外部ファイルの rename を含められない』)
- 前提: `schema_migrations` に `jsonl-cutover` 行が既に存在し `cost_ledger` に import 済みの行がある
  (相 1 完了済み) が、旧 JSONL の rename (相 2) は未実施のままクラッシュした状態。
- 操作: 移行ルーチンを再実行する。
- 期待: SQLite への再 import は発生しない (`cost_ledger` の行数が実行前後で不変 — 二重 import しない)。
  旧 JSONL 3 ファイルが `.migrated` サフィックス付きへ rename される (相 2 のみ再試行)。再度実行しても
  (旧ファイルが既に rename 済みのため) 冪等に成功する。

### CL11 空の旧 JSONL でも marker が「0 行 import 済み」を示す [P1]
- 正本: 10 §7.5.3 L699-700 (『空の旧 JSONL でも marker が「0 行 import 済み」と「未 import」を判別
  する』)
- 前提: `cost-ledger.jsonl` が 0 バイト (課金記録が一度も無い新規デバイス)。
- 操作: 移行ルーチンを実行。
- 期待: `schema_migrations` に `jsonl-cutover` 行が作成される (0 行の import でも marker は必須)。
  この状態で移行ルーチンを再度実行しても "未 import" と誤認して再 import を試みない (marker の
  有無のみで判定する、行数では判定しない)。

### CL12 単一 savepoint による原子性 (失敗時の torn state 不在) [P0]
- 正本: 10 §7.5.3 L720-728 (in-place migration の要件: 『全体を単一 savepoint で包み、失敗時は
  rollback して torn state を残さない』)
- 前提: 旧 JSONL に、SQLite の CHECK 制約に違反する破損行 (例: `usd: -5.0`) が 1 行混入している。
- 操作: 移行ルーチンを実行する (破損行の import 中に CHECK 違反で失敗する想定)。
- 期待: savepoint 全体が rollback され、`cost_ledger` に部分的な import 行が一切残らない。
  `schema_migrations` にも marker が残らない。旧 JSONL は rename されない (相 2 未到達)。エラーが
  呼び出し元へ伝播する (実装エラー相当 — 破損 JSONL 行は本書の対象外の別契約が扱う旨のみ記す)。

---

## C. Online Batch 投入の 2 相プロトコル (相 1〜相 3)

> 04 §5.8 冒頭 (L942-948): 「原則 = **外部に副作用を起こす前に意図を耐久記録する**。課金の記録喪失は
> 有界だが、無記録の in-flight job は無制限に残る」。以下 CL13-CL21 は手順 (L955-1003) を機械検証可能な
> 単位に分解する。

### CL13 相 1 (新規): state=0・新規 UUIDv7・estimated_usd 予約の INSERT [P0]
- 正本: 04 §5.8 L957-958 (『**相 1 — intent 記録**: batch_requests 行を INSERT / UPDATE する
  (state=0、intent_token = **新規 UUIDv7**... estimated_usd = 予約額)』)
- 前提: 対象 4 組キー (scope_id, adapter_kind, input_hash, tool_profile_hash) の `batch_requests` 行が
  存在しない。
- 操作: markdownize タスクの Batch 投入を開始する (相 1 を実行)。
- 期待: 新規行が `state=0`・`request_kind='batch'`・`intent_token` = 有効な UUIDv7 (バージョン
  nibble = 7)・`estimated_usd` = 予約額 (非負・有限)・`upload_id`/`batch_job_id`/
  `provider_scope_id`/`job_create_started_at`/`error`/`completed_at` は全て NULL・
  `submission_seq` = CL15 の規則どおり採番されて INSERT される。

### CL14 相 1 (再発行): retry / reindex --force による NULL 戻し [P0]
- 正本: 04 §5.8 L958-961 (『再投入... で相 1 を再発行する場合、**同じ UPDATE で upload_id /
  batch_job_id / job_create_started_at / stale_after_at / provider_scope_id / error / completed_at
  を NULL へ戻す**』)
- 前提: 既存の terminal 行 (state=3、`error='network_error'` 相当、`upload_id`/`batch_job_id`/
  `provider_scope_id`/`completed_at` が非 NULL・旧 attempt の残骸掃除は完了済み・`intent_token` は
  既に NULL 化済み) がある。
- 操作: `kio batch retry` により当該行の相 1 を再発行する。
- 期待: 同一 UPDATE 文で `upload_id`・`batch_job_id`・`job_create_started_at`・`stale_after_at`・
  `provider_scope_id`・`error`・`completed_at` の全てが NULL に戻り、`intent_token` に新しい
  UUIDv7・`state=0`・`estimated_usd` に新しい予約額が設定される。`attempts` は前 attempt から
  引き継がれる (相 1 の NULL 戻しの対象に `attempts` は含まれない — L843-845 の
  `contract_violation_count` 除外規定とは別列)。

### CL15 相 1 の submission_seq 採番規則 (MAX+1、欠落時の UNIQUE 衝突) [P0]
- 正本: 04 §5.8 L962-965 (『submission_seq はこの相 1 で必ず MAX + 1 へ採番する (基準 = cost_ledger
  同キーの MAX と自行現値の大きい方... 採番を怠ると、次の実課金記帳が旧 attempt の seq と UNIQUE
  衝突して ON CONFLICT DO NOTHING に黙って吸収される』)
- 前提: 同一 4 組キーで `cost_ledger` に `submission_seq=3` の確定行が既にあり、`batch_requests` の
  現行行は `submission_seq=2` (旧値のまま)。
- 操作: (a) 正しい実装 (相 1 で MAX+1 = 4 を採番) と (b) 誤った実装 (採番せず `submission_seq=2` を
  維持) の両方をシミュレートし、その後相 3 で確定課金を `INSERT ... ON CONFLICT DO NOTHING` する。
- 期待: (a) は `submission_seq=4` で `cost_ledger` へ正しく記帳される。(b) は UNIQUE
  `(scope_id,adapter_kind,input_hash,tool_profile_hash,submission_seq=2)` が既存の別行 (旧 attempt の
  記帳) と衝突し、`ON CONFLICT DO NOTHING` により**無音で記帳が失われる** (この回帰を実機で再現し、
  採番規則が必須であることを立証する)。

### CL16 相 2a: provider_scope_id を upload 直前に記録、upload_id を成功直後に記録 [P0]
- 正本: 04 §5.8 L966-969 (『**相 2a — upload**: upload の**直前に** `provider_scope_id` を行へ記録する
  ...**成功直後に upload_id を行へ記録**する (job 作成が失敗しても残骸の handle を失わない)』)
- 前提: state=0 の行 (相 1 完了済み)。
- 操作: 相 2a を実行し、upload 呼出の直前と直後の DB 状態をそれぞれスナップショットする。upload 呼出
  成功後、job 作成 (相 2b) を意図的に失敗させる。
- 期待: upload 呼出**直前**の時点で既に `provider_scope_id` が非 NULL (upload 呼出自体が失敗しても
  scope の記録は残る)。upload 成功**直後**に `upload_id` が記録される。相 2b が失敗しても
  `upload_id` は行に残ったまま (残骸 handle を失わない) で `state` は 0 のまま。

### CL17 相 2b: job_create_started_at を単独小 Tx で記録、scope 不一致時の再開規則 [P0]
- 正本: 04 §5.8 L970-975 (『**相 2b — job 作成**: 呼出の**直前**に job_create_started_at = now を
  **単独の小 Tx**で行へ記録する... 現 instance の scope が記録値と一致しない場合は呼び出さず、旧
  upload を掃除して相 2a からやり直す』)
- 前提: state=0、`provider_scope_id`・`upload_id` 記録済み (相 2a 完了)。
- 操作: (a) 現在の client instance の `provider_scope_id()` が記録値と一致するケースで相 2b を実行。
  (b) 一致しないケース (例: 認証切替後の再実行) で相 2b を試みる。
- 期待: (a) `job_create_started_at` が job 作成呼出の**直前**に単独 Tx で耐久記録され (呼出結果を
  待たずに先に確定していることを別 Tx 分離で確認)、job metadata に `intent_token` と 4 組キーが
  埋め込まれ、成功後に `batch_job_id` と `state=1` が記録される。(b) job は呼び出されず、旧
  `upload_id` の削除 (掃除) が行われた上で相 2a (新しい `provider_scope_id` 記録 → 再 upload) から
  やり直される。

### CL18 相 3 成功: tombstone 再検査 → 確定記帳+state=2+completed_at 同一 Tx → upload 削除 [P0]
- 正本: 04 §5.8 L976-980 (『**相 3 — collect**: 出力の取得・persist 後、確定課金の cost_ledger 記帳と
  state=2 + completed_at を**同一 Tx** で行い、upload を削除する... **persist 直前に対象 raw の
  tombstone を再検査する**』)
- 前提: state=1、`batch_job_id` 記録済み。出力が正常に fetch でき、受け入れ検査 (§3.2) を通過する。
  対象 raw は tombstone されていない。
- 操作: 相 3 を実行する。
- 期待: persist の直前に tombstone 再検査が走る (テスト二重化: 検査タイミングを検証する別テストは
  CL19)。出力 persist 後、**同一 Tx** 内で `cost_ledger` へ `outcome='succeeded'` の確定行が
  `INSERT ... ON CONFLICT DO NOTHING` され、`batch_requests.state=2`・`completed_at` が設定される。
  Tx コミット後に upload 削除が (Tx 外で) 試行され、成功または 404 のいずれでも削除成功として扱われる。
  全 upload 削除完了をもって `intent_token` が NULL 化される (削除未完なら NULL 化しない — CL39 で
  詳細検証)。

### CL19 相 3: persist 直前の tombstone 再検査で purge 検出 → 出力破棄 + reject 終端 [P0]
- 正本: 04 §5.8 L979-980 (『**persist 直前に対象 raw の tombstone を再検査する** — purge 済みなら
  出力を破棄し、下記の reject 終端と同形 (error='purged') で閉じる』)
- 前提: state=1、`batch_job_id` 記録済み。出力の fetch には成功するが、job 投入後・collect 前に
  当該 raw が `kio purge` された (tombstone が存在)。
- 操作: 相 3 を実行する。
- 期待: fetch した出力は persist されない (normalized manifest / unit object は一切書き込まれない)。
  reject 終端と同形の Tx: `cost_ledger` に `error='purged'` に対応する `outcome='purged'` 行が確定記帳
  され (実額が有効なら provider 値、無効なら estimated=1 — CL27 と同一規則)、
  `batch_requests.state=3`・`error='purged'`・`completed_at` が同一 Tx で設定される。upload 掃除は
  相 3 と同じ回復規則 (Tx 外、冪等再試行、404=成功) に従う。
  **解釈が割れうる点**: `contract_violation_count` をこの経路で increment するかは spec 上明記が無い
  — §L (解釈が割れうる点) の note-1 を参照し、実装判断で固定した上で spec 追記を促す。

### CL20 相 3: 受け入れ検査 reject (contract_violation) → 出力非 persist + Tx 内 attempts 更新 [P0]
- 正本: 04 §5.8 L981-984 (『**出力が受け入れ検査 (§3.2) で reject された場合 (contract_violation) も
  persist しない**: 同一 Tx で確定課金 (provider 報告値) の記帳 + state=3・error='contract_violation'・
  completed_at を行い、attempts を耐久更新する（**upload 掃除は Tx に含めない**）』)
- 前提: state=1。出力の受け入れ検査 (§3.2 V1〜V6 のいずれか) が失敗する。
- 操作: 相 3 を実行する。
- 期待: unit は 1 つも persist されない。同一 Tx 内で `cost_ledger` に `outcome='contract_violation'`
  の確定行 (provider 報告 usage が有効ならその値、無効なら estimated=1 — CL27) が記帳され、
  `batch_requests.state=3`・`error='contract_violation'`・`completed_at`・
  `contract_violation_count` が **+1** されて同一 Tx でコミットされる。upload 削除はこの Tx に
  含まれず、Tx コミット後に別途 (冪等・404=成功) 実行される。

### CL21 「同一 mode で 1 回のみ」の順序規範と durable な contract_violation_count 判定 [P0]
- 正本: 04 §5.8 L985-995 (『§3.2 の「同一 mode で 1 回のみ再試行」は**この終端 Tx の完了後、かつ旧
  attempt の残骸掃除完了... 後に**、新 intent_token・新 submission_seq の相 1 として開始する...
  再投入できるのは count <= 1 のとき... count >= 2 は failed permanent... count は**タスクキー単位の
  通算**であり mode 別に数えない』)
- 前提: CL20 で `state=3`・`error='contract_violation'`・`contract_violation_count=1` になった行。
  upload 削除がまだ完了していない (残骸掃除未完)。
- 操作: (a) 残骸掃除が未完のまま `kio batch retry` を試みる。(b) 残骸掃除 (upload 全削除) 完了後に
  `kio batch retry` を実行し、再度 contract_violation で終端させる (`contract_violation_count` が
  2 になる)。(c) その状態でさらに `kio batch retry` を試みる (mode を full に切り替えて試す)。
- 期待: (a) 新しい相 1 は開始されない (旧 token の残骸掃除完了が前提条件)。(b)
  `contract_violation_count=2` になった時点でこのタスクキーは failed permanent
  (`task_retry_allowed` 相当が false) — mode を full に切り替えても (c) は再投入不可 (count は
  mode 非依存のタスクキー通算のため、mode 切替では回避できない)。`error` 列は最新状態の表示に
  過ぎず判定源ではない (相 1 の NULL 戻しで `error` が NULL に戻っても `contract_violation_count`
  読み取りで failed permanent と判定できることを確認する)。

---

## D. 記帳の冪等性

> 04 §5.8 L1005-1007: 「cost_ledger への記帳は `INSERT ... ON CONFLICT DO NOTHING`... 記帳前の
> 「記帳済み判別」は同一タスクキー × **batch_job_id IN (発見 job id, 当該 intent_token)** の既存行で
> 行う」。

### CL22 ON CONFLICT DO NOTHING による二重記帳防止 (クラッシュ再実行) [P0]
- 正本: 04 §5.8 L1005-1006 (『cost_ledger への記帳は INSERT ... ON CONFLICT DO NOTHING (§5.4 の
  UNIQUE が実体)。記帳前の「記帳済み判別」は同一タスクキー × batch_job_id IN (発見 job id、当該
  intent_token) の既存行で行う』) / 04 §5.4 L811-812 (『記帳は必ず INSERT ... ON CONFLICT DO
  NOTHING (再試行・クラッシュ再実行で二重計上しない）。UNIQUE キーが冪等性の実体のため、
  submission_seq を進めずに別内容を記帳してはならない』)
- 前提: 相 3 の確定記帳 Tx がコミット直後 (`cost_ledger` に 1 行、`batch_requests.state=2`) にプロセスが
  クラッシュしたと仮定し、書き込み系コマンドを再実行する。
- 操作: 回復ルーチンが同じ `(scope_id, adapter_kind, input_hash, tool_profile_hash, submission_seq)`
  で同一内容の `INSERT ... ON CONFLICT DO NOTHING` を再実行する。
- 期待: `cost_ledger` の該当行数は 1 のまま増えない (UNIQUE 制約 + ON CONFLICT DO NOTHING により
  二重計上されない)。`usd` の SUM は 1 回分のみ。

### CL23 job id 不明の記帳: submission_seq+1 UPDATE してから estimated 行を記帳 [P0]
- 正本: 04 §5.8 L1008-1010 (『job id 不明の記帳 (期限超・abandon) は **submission_seq を +1 へ行
  UPDATE し、その新値で token キー・usd = 行の estimated_usd の estimated 行を記帳する** (seq
  現値のまま記帳すると、次の正規 close が同じ seq を計算して UNIQUE 衝突し、実課金が DO NOTHING に
  黙って吸収される)』)
- 前提: `batch_requests` 行 (state=1、`submission_seq=5`、`intent_token=T1`、`batch_job_id` は
  未記録=NULL、`estimated_usd=2.0`) が、回復期限超過で unknown 精算されるケース。
- 操作: 期限超過による estimated 精算を実行する。
- 期待: 精算前に `batch_requests.submission_seq` が `5→6` へ UPDATE される。`cost_ledger` へは
  `submission_seq=6`・`batch_job_id=T1` (intent_token をキーに使用)・`usd=2.0`・`estimated=1`・
  `outcome='unknown_settled'` の行が記帳される。`submission_seq=5` のままで記帳した場合との対比
  (別ケース) で、後続の相 1 再発行が `submission_seq=6` の**さらに +1 = 7** から始まり (CL15 の
  MAX+1 規則が精算行も基準に含む)、精算行の seq (6) と新 attempt の実課金行が衝突しないことを
  確認する。

### CL24 abandon による job id 不明記帳も同一規則 (submission_seq+1) [P0]
- 正本: 04 §5.8 L1008-1011 (『job id 不明の記帳 (期限超・**abandon**）は submission_seq を +1 へ行
  UPDATE し...』『この +1 は... 精算 estimated 行の採番 (期限超の +1 = 精算行の採番、**abandon の
  +1 = 最終 attempt の終端採番**) である』)
- 前提: `batch_requests` 行 (state=0、`submission_seq=2`、`intent_token=T2`、`estimated_usd=0.8`) を
  ユーザーが `kio batch abandon` する。
- 操作: abandon を実行する (CL62-CL68 の CLI 契約と接続する記帳部分のみ検証)。
- 期待: `submission_seq` が `2→3` へ UPDATE されてから、`outcome='abandoned'`・`usd=0.8`・
  `estimated=1`・`batch_job_id=T2` の行が `submission_seq=3` で `cost_ledger` に記帳される。

### CL25 estimated 記帳行は当該 attempt の最終記録 (事後の確定し直し禁止) [P1]
- 正本: 04 §5.8 L1014-1016 (『**estimated 行は当該 attempt の最終記録であり、後日 job が確認できても
  書き換え・確定し直しはしない** (UPDATE 禁止と整合。二重計上は記帳済み判別が防ぎ、実額との差は
  既知の有界誤差として受容する)』)
- 前提: CL23 のとおり `submission_seq=6` で `outcome='unknown_settled'` の estimated 行が記帳済み。
  その後、同じ job が偶然 provider 側で発見され実額が判明した。
- 操作: 発見された実額で当該行を「確定し直す」試み (UPDATE) を行う。
- 期待: `cost_ledger` に対する UPDATE は (append-only 台帳の一般規則として) 拒否される、または
  実装が UPDATE 経路自体を持たないことを確認する。実額との差は記帳されず、既知の誤差として
  許容される — 新しい行の追記による訂正も spec 上明記されていない (行うなら別の正当な attempt
  としてのみ、この estimated 行自体は不変)。

---

## E. outcome 閉 enum と Adapter 報告値の事前検証

### CL26 outcome 8 値の対応表 (終端理由 ⇔ outcome 値) [P0]
- 正本: 04 §5.8 L1018-1023 (『**outcome の対応**... 正常完了 = succeeded / §3.2 reject 終端 =
  contract_violation / expired 終端 = expired / abandon = abandoned / 拒否課金 provider の submit
  拒否 = submit_rejected / purge 起因の terminal 化 (error='purged') = purged / 回復期限超過・照会
  不能の estimated 確定 = unknown_settled / 正常な制御応答 (fallback_to_full=true、§3.2) の request
  終端 = fallback_to_full』)
- 前提: 8 通りの終端シナリオをそれぞれ用意する: (1) 正常 collect 成功、(2) §3.2 受け入れ検査 reject、
  (3) provider が expired 報告、(4) `kio batch abandon`、(5) 拒否課金 provider の submit 拒否
  (07§5.7 条件 6)、(6) persist 直前 tombstone 検出、(7) 回復期限超過での estimated 精算、
  (8) Adapter が `fallback_to_full=true` を返す制御応答。
- 操作: 各シナリオの終端 Tx を実行する。
- 期待: (1)→`succeeded`、(2)→`contract_violation`、(3)→`expired`、(4)→`abandoned`、
  (5)→`submit_rejected`、(6)→`purged`、(7)→`unknown_settled`、(8)→`fallback_to_full`。9 番目の
  値は CHECK 制約 (CL03) により INSERT 不能。

### CL27 usd の事前検証: 有限・非負なら provider 値、そうでなければ estimated 縮退 [P0]
- 正本: 04 §5.8 L1025-1026 (『**記帳値の事前検証**: Adapter 報告値は INSERT 前に検証する — usd は
  有限・非負の数値』) / L1030-1031 (『不正値は provider 報告値を使わず、行の estimated_usd を
  estimated=1 で記帳して同一 Tx で terminal 化する』)
- 前提: billable な terminal 応答で `usage={usd: X}` を Adapter が報告する。
- 操作: X に (a) `2.50` (正当)、(b) `-1.0`、(c) `NaN`、(d) `Infinity` を与えて終端 Tx を実行する。
- 期待: (a) は `usd=2.50`・`estimated=0` で記帳。(b)(c)(d) は provider 値を使わず
  `batch_requests.estimated_usd` (相 1 で確定済みの保守推定額) を `estimated=1` として同一 Tx で
  記帳し terminal 化する。応答の受理判定・`outcome`・`contract_violation_count` はいずれの場合も
  変化しない (CL31)。

### CL28 billable_units の事前検証 (パラメタ化: 空配列/重複kind/enum外kind/非整数count) [P0]
- 正本: 04 §5.8 L1026-1030 (『billable_units は **1 要素以上の配列で、各要素の count が有限・非負の
  整数、kind が閉 enum・宣言集合 (billable_kinds) 内・配列内で一意、かつ全要素の単価が解決可能で
  あること**... 空配列・kind の重複・宣言集合外の kind・非整数 count を含む不正値・欠落は... estimated
  で記帳して同一 Tx で terminal 化する』) / 07 §4 L294-296 (『billable_units = unique-kind の配列
  [{kind, count}, ...] (1 要素以上。kind = "pages" | "tokens_in" | "tokens_out" の閉 enum）』)
- 前提: billable な terminal 応答。tools.toml の `[pricing]` に `pages=0.01`, `tokens_in=0.000002`,
  `tokens_out=0.000006` が設定済み。
- 操作: `billable_units` に (a) `[{kind:"pages",count:10}]` (正当・単一要素)、(b) `[]` (空配列)、
  (c) `[{kind:"pages",count:5},{kind:"pages",count:3}]` (kind 重複)、
  (d) `[{kind:"images",count:2}]` (宣言集合外 kind)、(e) `[{kind:"pages",count:2.5}]` (非整数
  count)、(f) `[{kind:"pages",count:-1}]` (負の count)、(g)
  `[{kind:"tokens_in",count:1000},{kind:"tokens_out",count:200}]` (正当・複数要素) を与える。
- 期待: (a) `usd = 10 * 0.01 = 0.10` で provider 値記帳、`estimated=0`。(b)(c)(d)(e)(f) は全て
  provider 値を使わず estimated 縮退 + terminal 化 (CL27 と同一 Tx 規則)。(g) は要素ごとの単価 ×
  count を**合算**: `1000*0.000002 + 200*0.000006 = 0.002 + 0.0012 = 0.0032` で provider 値記帳。

### CL29 非 billable な正当応答は usd=0/estimated=0 で記帳 (courant縮退と区別) [P0]
- 正本: 04 §5.8 L1031-1034 (『この縮退は usage が必須の応答 (billable terminal 応答) に限る — 非
  billable な応答 (単価 0 のローカル LLM・拒否課金を宣言しない (reject_billing="nonbillable")
  provider の reject 等) の usage 欠落は正当であり、確定額 0 (usd=0・estimated=0) で記帳する』)
- 前提: (a) ローカル LLM (単価 0 declared) タスクの成功終端、usage 欠落。(b)
  `reject_billing="nonbillable"` を宣言する provider の投入拒否 (permanent 4xx)、usage 欠落。
- 操作: 両ケースの終端 Tx を実行する。
- 期待: (a)(b) いずれも `usd=0`・`estimated=0` で記帳される — CL27/CL28 の「不正値・欠落 →
  estimated=1」縮退とは**異なる経路**であることを、同じ「usage 欠落」という表面条件でも
  `reject_billing`/adapter 単価宣言によって分岐することを確認する。

### CL30 pricing 表で単価が解決不能な kind は estimated 縮退 + warning [P0]
- 正本: 04 §5.8 L1035-1037 (『報告された billable_units.kind の単価が tools.toml の [pricing] で
  解決できない場合 (未設定・表の欠落) も「欠落」と同じ estimated 縮退 + warning とする... 0 円確定には
  しない』)
- 前提: Adapter が `billable_kinds=["pages","tokens_in"]` を宣言済み (送信前の被覆検査は通過済み)
  だが、終端応答受領時点で tools.toml の `[pricing]` から `tokens_in` の行が (実行中に設定ファイルが
  書き換わる等で) 欠落している。応答は `billable_units=[{kind:"tokens_in",count:500}]`。
- 操作: 終端 Tx を実行する。
- 期待: provider 値 (`500 * 単価`) は使われず、`estimated_usd` を `estimated=1` で記帳する。
  **`usd=0` の確定記帳にはしない** (CL29 の非 billable 経路と区別)。`usage_validation=invalid` /
  `billing_source=estimated` の warning ログ (event code `KIO-EV-ADAPTER-USAGE-001`、07§7
  L622-624) が出力される (ログ内容自体は本書の対象外、event code の発火有無のみ確認)。

### CL31 課金 field 単独の不良は受理判定・outcome・contract_violation_count を変えない [P0]
- 正本: 04 §5.8 L1038-1041 (『**課金 field 単独の不良は応答の受否・outcome・contract_violation_count
  を変えない** — 成功は成功のまま、正常な制御応答は outcome='fallback_to_full' のまま (構造違反
  だけが contract violation。課金 field の不良は warning log で可視化する）』)
- 前提: (a) 正常な成功応答だが `usage.usd=-1` (不正)。(b) `fallback_to_full=true` の正常な制御応答
  だが `billable_units=[]` (不正)。
- 操作: それぞれの終端 Tx を実行する。
- 期待: (a) `outcome='succeeded'` のまま (`contract_violation` に変わらない)、`usd` のみ
  estimated 縮退。`contract_violation_count` は増加しない。(b) `outcome='fallback_to_full'` のまま
  (task は非終端のまま — §3.2 規則どおり)、`usd` のみ estimated 縮退。`contract_violation_count` は
  増加しない。両ケースとも「構造違反 (§3.2 V1-V6 等) だけが contract_violation を誘発する」ことを
  対比で示す。

---

## F. crash 回収 (書き込み系コマンド冒頭)

> 04 §5.8 L1049-1057: 回復は `kio index` / `kio batch resume` / `kio batch retry` /
> `kio batch abandon` / `kio reindex` / `kio repair --rebuild-db` の**冒頭**、`.kio/.lock` 保持下で
> 行う。未終端行 (state 0/1) と `intent_token` 非 NULL の終端行 (残骸掃除未完) を三値で照合する。
> `request_kind='sync'` の行は対象外 (§G で別途扱う)。

### CL32 回収の起動条件・lock 保持・三値照合の対象行選定 [P0]
- 正本: 04 §5.8 L1049-1055 (『**回復**（書き込み系 batch コマンド... の冒頭。これらは .kio/.lock を
  取得する書き込み系であり、相 1〜2b の遷移・token の発行も lock 保持下で行う）... 未終端の行
  (state 0/1) と intent_token 非 NULL の終端行 (= 残骸掃除未完) を三値で照合する。
  request_kind='sync' の行は job / upload 照合の対象外』)
- 前提: `batch_requests` に (a) state=0 の行、(b) state=1 の行、(c) state=3・
  `intent_token` 非 NULL の行 (残骸掃除未完)、(d) state=2・`intent_token` NULL の行 (掃除済み完了行)、
  (e) `request_kind='sync'` の state=1 行、が混在。
- 操作: `.kio/.lock` を取得したうえで `kio index` を実行し、回収対象行の列挙結果を検査する。
- 期待: (a)(b)(c) が回収対象に含まれる。(d) は対象外 (既に完全終端)。(e) は本節の対象外 (§5.4
  crash 回収 = §G が扱う) — job/upload 照合ルーチンには一切渡されない。回収処理全体が
  `.kio/.lock` 保持中に実行される (lock 未取得での回収呼び出しはエラー、または lock 取得を伴わない
  経路が存在しないことを確認)。

### CL33 回収は新規送信ではないため network opt-in 不要 [P1]
- 正本: 04 §5.8 L1055-1057 (『回復の照会・出力取得・upload 掃除は既存 request に対する受信・掃除で
  あり**新規送信に当たらない — network opt-in / --online なしで実行できる**』)
- 前提: 対象 scope の network 承認が無い (`approvals[]` 空、`allow_network=false`)。CL32 の
  回収対象行が存在する。
- 操作: `--online` を付けずに `kio index` を実行する。
- 期待: 回収 (job 一覧照会・出力取得・upload 削除) は承認なしでも実行される (新規送信ゲートの
  対象外)。ただしこの回収を経て新しい相 1 (再投入) が必要になった場合、その**新規送信**は通常どおり
  opt-in を要求する (回収自体と再投入は別ゲート)。

### CL34 found: token 一致で追跡続行、batch_job_id 未記録なら自己記述化 [P0]
- 正本: 04 §5.8 L1059-1060 (『**found** (job 取得/一覧で intent_token 一致): 追跡を続行し相 3 へ。
  batch_job_id 未記録なら発見値を行へ書く (自己記述化 — 以後この行は token 照合の対象から外れる)』)
- 前提: state=1、`batch_job_id` が (a) 既に記録済み、(b) NULL のまま (相 2b 成功直後にクラッシュし
  batch_job_id 記録前だったケースを模擬)、の 2 パターン。provider の job 一覧照会で
  `intent_token` に一致する job が見つかる。
- 操作: 回収ルーチンを実行する。
- 期待: (a)(b) いずれも相 3 (collect) へ進む。(b) は発見された job id が `batch_job_id` へ
  書き込まれる (自己記述化)。自己記述化後は以後の回収でこの行を「token 一覧照合」ではなく
  `batch_job_id` 直接照会で扱うこと (再回収時に一覧走査ではなく直接照会が使われることを確認)。

### CL35 confirmed-absent: 全ページ走査 + 可視化猶予、相2b未着手/相2a着手の分岐 [P0]
- 正本: 04 §5.8 L1061-1067 (『**confirmed-absent**: 「不在」と断定できるのは、記録済み
  provider_scope_id と同一 scope での全ページ走査済み一覧に無く、かつ可視化猶予 (既定 10 分) を
  経過したときのみ... 相 2b 未着手 (job_create_started_at IS NULL) の行は job 一覧照合の対象にしない
  — job 不存在は記録から確定している。ただし provider_scope_id 非 NULL... の行は... list_uploads を
  token で照合し、発見した upload の削除... または採用... を完了してから、... 猶予経過で再投入して
  よい』)
- 前提: 4 パターン: (A) `provider_scope_id` NULL (相 2a 未着手)。(B) `provider_scope_id` 非 NULL・
  `job_create_started_at` NULL (相 2a 完了・相 2b 未着手)。(C) 両方非 NULL (相 2b 着手) だが
  job 一覧の**部分応答**しか得られない。(D) 両方非 NULL、全ページ走査完了・job 一覧に不在、
  可視化猶予経過前。
- 操作: 回収ルーチンを各パターンで実行する。
- 期待: (A) job/upload いずれの照合も行わない (相 2a 未着手なので upload 自体存在しない)。(B) job
  一覧照合は**行わない** (job_create_started_at IS NULL のため job 不存在は記録から確定) が、
  `list_uploads` による upload 照合は必要 (provider_scope_id 非 NULL のため)。(C) 部分応答は不在の
  証明にならない — unknown 扱い (CL36) に落ちる、confirmed-absent と誤判定しない。(D) 猶予未経過
  のため再投入せず保持 (猶予経過を待つ)。全ページ走査完了 + 猶予経過の**両方**を満たして初めて
  confirmed-absent → upload 照合完了後に再投入可。

### CL36 unknown: 保持継続、回復期限超過で estimated 精算 + 掃除完了ゲート付き再投入 [P0]
- 正本: 04 §5.8 L1068-1074 (『**unknown**（照会失敗・scope 不一致・部分応答）: 何も変更せず保持し、
  次回再試行する。回復期限（既定 48h）を超えたら... estimated 記帳を... 行ってから再投入する...
  ただし再投入（新 token の相 1）は、旧 intent_token による upload / job の照合・掃除が完了している
  場合に限る — 照会不能のまま掃除未完の行は再投入せず stalled として表示し続ける』)
- 前提: state=1、`intent_token` 時刻・`job_create_started_at` が共に回復期限 (48h) を (a) 超えて
  いない、(b) 超えている・かつ旧 upload/job の掃除が完了している、(c) 超えている・かつ旧 upload の
  掃除が未完。
- 操作: 3 パターンで回収ルーチンを実行する。
- 期待: (a) 行は変更せず保持、次回再試行。(b) CL23 と同一の estimated 精算 (`submission_seq+1`)
  を行った後、新しい相 1 (新 `intent_token`・新 `submission_seq`) が開始される。(c) estimated
  精算は行われるが**新しい相 1 は開始されない** — 掃除が完了するまで `kio status` に stalled として
  表示され続ける (CL39/CL66 と接続)。回復期限は `max(intent_token 時刻, job_create_started_at) +
  48h` で算出され、config で変更可能。

### CL37 恒久 unknown: kio status の stalled 表示、abandon が唯一の脱出路 [P0]
- 正本: 04 §5.8 L1075-1076 (『**恒久 unknown**（資格情報喪失等）の行は kio status に **stalled** として
  表示し（表示には intent_token を含める）、kio batch abandon... を脱出路とする』)
- 前提: CL36(c) のように掃除未完のまま何度回収を試みても job/upload の存在が確認できない行。
- 操作: `kio status` を実行する。
- 期待: 当該行が `stalled` として表示され、表示に `intent_token` が含まれる。`kio batch retry`
  (通常の再試行) では解消できない (CL36(c) が再投入をブロックし続けることの再確認)。唯一の解消
  手段が `kio batch abandon` であることを、他の全コマンド (`resume`/`retry`/`reindex`) を試しても
  状態が変わらないことで示す。

### CL38 残骸掃除: terminal task の upload 削除、abandon済みは照合・記帳せず掃除のみ [P0]
- 正本: 04 §5.8 L1086-1087 (『**残骸掃除**: terminal な task の upload（upload_id 記録分 +
  intent_token 埋込 filename の一覧照合分）を削除する。abandon 済み task は照合・記帳を行わず掃除
  のみ行う』)
- 前提: (a) `state=3`・`error='contract_violation'` (通常の reject 終端)・`upload_id` 記録済み。
  (b) `state=3`・`error='abandoned'` (CL24 で abandon 済み)・`upload_id` 記録済み。
- 操作: 両方に対して残骸掃除ルーチンを実行する。
- 期待: (a)(b) いずれも `upload_id` 記録分の削除 + `intent_token` 埋込 filename での一覧照合分の
  削除が実行される。(b) は job/upload の**照合・記帳を追加で行わない** (abandon 時点で既に確定記帳
  済みのため、掃除ルーチンが二重に found/confirmed-absent 判定をしない) ことを、(a) との処理経路の
  差分として確認する。

### CL39 順序規範: 旧 attempt の照合・記帳・消し込み完了後にのみ retry 予算リセット + 新相1 [P0]
- 正本: 04 §5.8 L1089-1091 (『**順序規範**: 明示 retry / kio reindex --force が terminal task を
  再投入する場合、**旧 intent_token の照合・記帳・消し込みを完了してから**、retry 予算のリセットと
  新しい相 1 を行う（逆順だと旧 attempt の発見・記帳が新 attempt の予算・記録を汚す）』)
- 前提: terminal task (state=3) で残骸掃除が未完 (`intent_token` 非 NULL)。
- 操作: `kio batch retry` を実行する。
- 期待: 実装が (誤って) 先に retry 予算をリセットして新しい相 1 を発行した場合、旧 attempt の
  found/confirmed-absent 判定が新 attempt の行 (別 `intent_token`) に対して行われてしまい、記帳・
  予算が汚染される回帰を再現できることを示す。正しい実装は旧 `intent_token` の照合・記帳・
  `intent_token` NULL 化が完了するまで新しい相 1 を開始しない。

### CL40 Markdownize 部分回復の再導出 (mode 不明→full、差集合→failed_units、error_kind 固定) [P0]
- 正本: 04 §5.8 L953 (『tasks.jsonl の task 記述子（mode / unit_keys / output_ref）は喪失しうるが、
  確定先と対象 unit は決定論的に再導出できる... mode が不明な場合は full として扱う... この full
  扱いの受け入れ検査では、差集合の unit を当該 job の failed_units と見なして §3.2（V6 を含む）を
  評価する... 合成する failed_units の error_kind は network_error（retryable）に固定する』)
- 前提: `tasks.jsonl` が失われた状態で、provider 出力 JSONL には custom_id (=unit_key) 5 件中 3 件
  分のみが含まれる (2 件は転送中に欠落 — provider は元々 5 件処理していたと仮定)。当該タスクキーの
  prepared units (raw から再導出) は 5 件。
- 操作: 回復処理を実行する。
- 期待: 出力先は当該タスクキー (input_hash, tool_profile_hash) の最新 instance の未完了 unit を
  補完する先として決定論的に再導出される。mode は不明なので `full` として扱われる。出力に現れない
  2 件 (prepared units 5 件 − 出力 custom_id 3 件の差集合) は当該 job の `failed_units` とみなされ、
  §3.2 V6 (full 出力契約: `updated∪added∪failed = prepared unit 全集合`) の評価にこの差集合を
  含めることで V6 違反にならない。合成された 2 件の `failed_units[].error_kind` は実際の失敗原因に
  関わらず `network_error` に固定され、通常の retry 経路 (§5.3 exp backoff) に乗る。既に done な
  unit は first-instance-wins で保全される。

---

## G. sync online 呼出の縮退 2 相 + idempotency 二段構え (U9, U11)

### CL41 idempotency の二段構え: sync は provider key、Batch は §5.8 が正本 (U11) [P0]
- 正本: 04 §5.5 L880 (『LLM API の二重課金防止は二段構え: sync 呼出は provider が idempotency key を
  提供する場合にそれを要求し、**Batch 投入（job 作成に idempotency key の無い provider が現実）は
  §5.8 の 2 相プロトコルを正本とする**』)
- 前提: (a) sync 呼出対応の Adapter で provider が idempotency key をサポートする場合。(b) sync
  呼出対応だが provider が idempotency key を提供しない場合。(c) Batch 呼出。
- 操作: 各ケースの二重送信防止機構を確認する。
- 期待: (a) provider の idempotency key 機構の使用が要求される (Adapter レベルの契約 — 本書では
  「要求されること」の事実確認のみ、実装詳細は 07-adapter-spec 側の契約)。(b) provider 側
  idempotency key が無くても、§G の縮退 2 相 (batch_requests 行ベースの記帳冪等性) のみで二重課金を
  防止する — Adapter 層に idempotency_key を一律要求**しない**。(c) 常に §5.8 の 2 相プロトコル
  (§C) が正本であり、provider の idempotency key 有無に関わらずこれが唯一の防止手段。

### CL42 sync 行モデル: request_kind='sync'、reservation は batch_requests 側 [P0]
- 正本: 04 §5.4 L768 (『**sync online 呼出は縮退 2 相に従う**: reservation は cost_ledger ではなく
  batch_requests 行で行う — 相 1 = 行作成 + estimated_usd 予約を cap 判定と同一 Tx で（intent_token
  = attempt token。§5.8 と同じ状態機械の縮約 — upload / job 相は無い）』)
- 前提: sync 呼出 (例: provider が Batch モードを持たない markdownize Adapter) を発行する。
- 操作: 相 1 を実行する。
- 期待: `batch_requests` に `request_kind='sync'` の行が作成され、`intent_token` = 新規 UUIDv7
  (= attempt token)、`estimated_usd` = 予約額が cap 判定 (§I) と同一 `BEGIN IMMEDIATE` Tx で
  設定される。`upload_id`・`provider_scope_id`・`batch_job_id` (この時点) はいずれも NULL のまま
  (upload/job 相が存在しないため相 2a/2b に相当する記録は発生しない)。

### CL43 provider request id の耐久記録タイミングと終端 Tx の分離 [P0]
- 正本: 04 §5.4 L768 (『provider request id は応答受信直後・終端 Tx より前に行の batch_job_id へ
  耐久記録する（下記 DDL — sync 行の照会キー）』)
- 前提: sync 呼出を実行し、provider から応答 (と request id) を受信した直後にプロセスをクラッシュ
  させる (終端記帳前)。
- 操作: 応答受信直後の DB 状態を検査したのち、書き込み系コマンドで回収する。
- 期待: クラッシュ時点で `batch_job_id` に provider request id が既に記録されている (終端 Tx とは
  別の耐久書込み)。回収時、この `batch_job_id` を使って結果を照会できる (CL45 の crash 回収へ接続)。

### CL44 複数 external call の直列化 (request 単位の相1→終端→次の相1) [P0]
- 正本: 04 §5.4 L768 (『複数 external call を行うタスクは request を直列化し、request ごとに新しい
  相 1（submission_seq = MAX+1）→ 終端を完了してから次の request を開始する（request 単位の冪等
  記帳 — 並行 request は作らない。課金済み call の盲目再試行を禁止）』)
- 前提: 1 タスクが 3 回の sync external call を必要とする (例: 3 ページ分の個別呼出)。
- 操作: タスクを実行する。
- 期待: 3 つの相 1 が**時系列に直列**発行される (`submission_seq` が呼出ごとに MAX+1 で単調増加)。
  2 番目の call の相 1 は 1 番目の call の終端 Tx (state=2 or 3) が完了して初めて発行される —
  並行して複数の相 1 が同時に state=0/1 で存在することはない (同一タスク内で)。

### CL45 sync 行の crash 回収: 照会可能なら確定、不能なら unknown 精算 [P0]
- 正本: 04 §5.4 L768 (『**crash 回収**（書き込み系コマンド冒頭 — §5.8 の回復と同時）: 残った state
  0/1 の request_kind='sync' 行は、batch_job_id（provider request id）が記録済みで照会可能なら
  結果を確定し、未記録・照会不能なら unknown として estimated を確定記帳し state=3 で terminal 化
  する（過大計上を許容 — 未記帳の過少計上より安全側）』)
- 前提: (a) `batch_job_id` 記録済み・provider に照会可能な sync 行。(b) `batch_job_id` NULL の
  sync 行。(c) `batch_job_id` 記録済みだが provider 照会が失敗する sync 行。
- 操作: 書き込み系コマンド冒頭の crash 回収を実行する。
- 期待: (a) 照会結果に基づき結果を確定記帳する (成功なら `outcome='succeeded'`)。(b)(c) は
  ともに unknown として `estimated_usd` を `estimated=1` で確定記帳し `state=3` で terminal 化する
  (CL23 と同一の submission_seq+1 記帳規則)。sync 行の「照会」は provider が任意で提供する経路
  であり (07§5.7 の Batch trait 契約には含まれない)、提供の無い Adapter は常に (b)(c) 相当の
  unknown 精算になる。

### CL46 sync 行の照会結果が fallback_to_full だった場合の扱い [P0]
- 正本: 04 §5.4 L768 (『照会で得た応答が §3.2 の正常な制御応答（fallback_to_full=true）だった場合も
  同節の規則を適用し outcome='fallback_to_full' で確定記帳する（task 非終端 — 通常規則どおり）』)
- 前提: CL45(a) の crash 回収照会で、provider の応答が `fallback_to_full=true` の制御応答だった。
- 操作: 回収を実行する。
- 期待: `outcome='fallback_to_full'` で確定記帳・`state=3`・`intent_token` NULL 化 (CL47) が同一 Tx
  で行われる。task 自体は非終端のまま (§3.2 規則) — この Tx 群の完了後に `mode=full` の新
  request が相 1 として開始される (CL44 の直列化規則どおり)。crash 回収が確定するのは記帳と
  state のみで、照会で得た出力自体は persist しない (CL18 と同じく新しい相 1 での再実行が必要)。

### CL47 sync 行は全終端 Tx で intent_token を即時 NULL 化 (batch 行の規則と対比) [P0]
- 正本: 04 §5.4 L768 (『sync 行は provider 側に残骸（upload / job）を作らないため、**全ての終端 Tx
  （成功・reject・unknown 精算・abandon・fallback_to_full — §3.2）で同一 Tx 内に intent_token を
  NULL 化する** — 「NULL 化は残骸掃除の完了時のみ」（§5.8）は batch 行の規則であり、sync では
  終端 = 掃除完了である』)
- 前提: sync 行が (a) 成功、(b) reject (contract_violation)、(c) unknown 精算、(d) abandon、
  (e) fallback_to_full のいずれかで終端する。
- 操作: 各終端 Tx を実行する。
- 期待: (a)〜(e) の**いずれも**終端 Tx と同一 Tx 内で `intent_token` が NULL 化される
  (batch 行 (CL18/CL38) のように upload 削除完了を待たない)。同一タスクキーの再投入 (新しい相 1)
  がこの Tx 直後から (掃除完了を待たずに) 可能であることを確認する — これが無いと「旧 token の
  消し込み完了後にのみ再投入可」という batch 行の順序規範 (CL39) と衝突し、sync タスクの再投入が
  恒久停止することを対比で示す。

---

## H. query embedding の device 行

### CL48 device 行の identity (scope_id='device' 予約値・input_hash・folder cap 除外) [P0]
- 正本: 04 §5.4 L769 (『query embedding request... は scope_id = 'device'（予約値 — scope_id は
  ULID のため実 scope と衝突しない）の request_kind='sync' 行として上記縮退 2 相に載せる。
  adapter_kind = 'embedding'・input_hash = NFC 正規化した query 文字列の sha256... folder cap
  判定（scope 別集計）には現れず、device cap / per_adapter（embedding）の合算には通常どおり
  含まれる』)
- 前提: vector|hybrid 検索 (page 1) を実行し、query embedding が必要になる。
- 操作: 検索を実行する。
- 期待: `batch_requests` に `scope_id='device'`・`adapter_kind='embedding'`・`request_kind='sync'`・
  `input_hash = sha256(NFC正規化(query文字列))` の行が作られる (query 本文自体は保存されない)。
  folder cap の scope 別集計クエリ (`WHERE scope_id = <folder scope>`) にはこの行が一切現れない。
  device cap の全合算クエリと `per_adapter='embedding'` の合算クエリには通常どおり含まれる。

### CL49 stale_after_at の算出 (実効 timeout 最大値+60秒、下限600秒、相1で耐久保存) [P0]
- 正本: 04 §5.4 L769 (『stale_after_at は相 1 Tx で「当該 request に適用する実効 timeout_seconds
  （device 行では参加 scope の実効値の最大値）+ 60 秒マージン（下限 600 秒）」から算出して保存し、
  回収は保存値のみを参照する』) / 07 §7 L600 (`[adapter.policy] timeout_seconds = 300`)
- 前提: 複数 scope 横断検索で、参加 scope A の実効 `timeout_seconds=300` (既定)、scope B の実効
  `timeout_seconds=120` (config で短縮)。
- 操作: 相 1 を実行する。
- 期待: `stale_after_at` = 相 1 実行時刻 + `max(300, 120) + 60` = +360 秒。下限 600 秒の適用も
  別ケースで確認: 両 scope とも `timeout_seconds=100` の場合、`max(100,100)+60=160 < 600` のため
  `stale_after_at` = 相 1 実行時刻 + 600 秒 (下限が優先)。回収ルーチンは以後、config を再読みせず
  `stale_after_at` の保存値のみを参照する (config 変更後も既存行の判定は変わらない)。
  **解釈が割れうる点**: 「参加 scope の実効値」を各 scope のどの config 層 (`~/.config/kio/config.toml`
  vs `.kio/config.toml`) から解決するかは spec 上に明示された優先順位規則が無い — §L note-2 参照。

### CL50 Retry-After 受信時の単調 CAS 延長 [P0]
- 正本: 04 §5.4 L769 (『Retry-After を受信した保持プロセスは自 token の CAS UPDATE で
  stale_after_at を **max(現行値, 現在 + Retry-After + timeout + 60 秒)** へ延長する — 単調:
  短い Retry-After で期限を縮めない。Retry-After は有限・非負を検証する（**不正値のみ 3600 秒の
  代替値とし、有効な実値は clamp しない**）』)
- 前提: `stale_after_at` = T0 の device 行 (自 token 保持中)。provider から Retry-After を受信する。
- 操作: (a) `Retry-After=30s` (有効・短い) を受信。(b) `Retry-After=7200s` (有効・長い) を受信。
  (c) `Retry-After=-5` (不正値) を受信。
- 期待: (a) `now+30+timeout+60` が現行値 T0 より小さければ延長は起きない (`max` により T0 のまま
  = 単調性、短い Retry-After で縮めない)。(b) `now+7200+timeout+60` が T0 より大きければ
  `stale_after_at` がその値へ延長される (clamp されない、7200 秒がそのまま反映)。(c) 不正値
  (負・非有限) は 3600 秒の代替値に置き換えられた上で同じ `max` 延長式に適用される。

### CL51 延長 UPDATE 0 行 = claim 喪失 → 以後の待機・再呼出・記帳を全中止 [P0]
- 正本: 04 §5.4 L769 (『延長 UPDATE が 0 行（= 他プロセスが回収済み）なら claim 喪失として以後の
  待機・provider 再呼出・記帳を全て中止する — 下記の状態遷移 CAS 敗者規則と同じ非記帳。累積延長に
  上限は設けない... 解放の脱出路は kio batch abandon』)
- 前提: 自プロセスが device 行を保持中 (token=T1) だが、CAS 延長 UPDATE の直前に別プロセスの
  stale 回収がこの行を `unknown_settled` として確定済み・`intent_token` を NULL 化済みだったとする。
- 操作: 自プロセスが `WHERE intent_token = T1` 条件で延長 UPDATE を実行する。
- 期待: UPDATE の影響行数が 0 (行は既に別 token/NULL になっている)。自プロセスはこれ以降の
  provider 待機・再呼出・課金記帳を一切行わない (二重計上防止)。累積延長回数に上限はない (脱出路は
  `kio batch abandon` のみ)。

### CL52 bounded sweep の 256 行上限と配分順序 (自key最優先/剪定>=128/決定的順序) [P0]
- 正本: 04 §5.4 L769 (『**1 回の実行あたり合計 256 行を上限とする bounded 処理**とし、配分と順序を
  固定する: (1) 自 key... の stale 行は上限枠外で常に最優先に回収する / (2) 剪定に最低 128 行を
  保証する / (3) 残余枠を一般 stale 回収に充てる... 各集合の処理・持ち越しの選択順は... 昇順 + 4 組
  PK の byte 順で完全に決定的とする。残余は次回実行へ持ち越す』)
- 前提: 自 key の stale 行 3 件、一般 stale 行 500 件、terminal 剪定対象行 400 件が存在する。
- 操作: 相 1 の claim 直前の sweep を実行する。
- 期待: 自 key の 3 件は上限枠 (256) 外で無条件に全て回収される。残り 256 行の枠のうち、剪定に
  最低 128 行が保証され (剪定対象 400 件中 128 件以上を剪定)、残り最大 128 行が一般 stale 回収に
  充てられる (一般 stale 500 件中 128 件のみ処理・残り 372 件は次回へ持ち越し)。同一実行内で
  一般 stale 対象が不足する場合は剪定側へ枠を融通し、逆も同様 (対象不足側の未使用枠は相互融通)。
  各集合内の処理順は `job_create_started_at`(sync stale) / `completed_at`(terminal) の昇順 + 4 組
  PK の byte 順で完全決定的 (同一入力なら毎回同じ 256 行が選ばれる)。

### CL53 device 行の書込み主体は kio search のみ、.kio/.lock 不要、inline sweep は照会しない [P1]
- 正本: 04 §5.4 L769 (『.kio/.lock は不要 — device 行はどの scope にも属さず、直列化は cost-ledger
  側の Tx が担う。**inline 回収では provider 照会を行わない** — 常に unknown 精算とする（検索
  応答性の保護。照会つき回収は書き込み系冒頭の crash 回収のみ）』)
- 前提: `kio search --vector` の page 1 実行中に device 行の inline sweep が発火する状況。
- 操作: inline sweep 対象の stale device 行に対して回収を行う。
- 期待: `.kio/.lock` は一切取得されない (device 行の直列化は `BEGIN IMMEDIATE` Tx のみで担保)。
  inline sweep で回収される stale 行は provider へ照会せず、常に `unknown_settled` として精算される
  (書き込み系コマンド冒頭の CL45 のような「照会可能なら確定」経路は inline では使われない —
  検索の応答性を優先するため)。

### CL54 同一key非stale in-flightはtext fallback (embedding_in_flight)、送信しない [P0]
- 正本: 04 §5.4 L769 (『同一 key が stale でない in-flight（他プロセスの生存 claim）のときは当該
  実行を text fallback（fallback_reason="embedding_in_flight"）に落とし、送信しない（同一 query の
  並行 claim・token 上書きを作らない）』) / 05 §1.1 L31, L60-61 (同一規範のクロスリファレンス)
- 前提: 自 4 組 key の device 行が別プロセスにより `state=0`・`stale_after_at` 未経過 (生存中) で
  保持されている。
- 操作: 同一 query で 2 プロセス目から検索を実行する。
- 期待: 2 プロセス目は新しい相 1 を発行せず、`fallback_reason="embedding_in_flight"` の text
  fallback に落ちる (vector/hybrid embedding は送信されない)。既存行の `intent_token` は上書き
  されない。

### CL55 device 行の全 UPDATE は自 token CAS、terminal 剪定条件、abandon の対象なし成功 [P0]
- 正本: 04 §5.4 L769 (『device 行の全ての状態遷移 UPDATE（request id 記録・終端）は WHERE
  intent_token = <自 token> の条件付き（CAS）で行う... terminal device 行の剪定: scope_id='device'
  ∧ state IN (2, 3)（成功終端 = state 2 を含む）∧ intent_token IS NULL ∧
  contract_violation_count = 0 ∧ completed_at が前月以前... 剪定・確定済みの 4 組 key への
  kio batch abandon は対象なしの冪等成功』)
- 前提: (a) device 行が `state=2`（成功）・`intent_token IS NULL`・`contract_violation_count=0`・
  `completed_at`=前月。(b) 同条件だが `contract_violation_count=1`。(c) 同条件だが
  `completed_at`=当月。
- 操作: terminal 剪定ルーチンを実行したのち、剪定された 4 組 key に対して `kio batch abandon` を
  実行する。
- 期待: (a) のみ剪定 (DELETE) 対象になる — 成功終端 (state=2) も剪定対象に含まれることを確認
  (state=3 のみを剪定する誤実装との対比)。(b)(c) は剪定されない (`contract_violation_count=0`・
  「前月以前」の両条件を厳密に要求)。(a) が剪定された後にその 4 組 key で `kio batch abandon` を
  実行すると、対象行が存在しないため**対象なしの冪等成功** (exit 0) になる。

---

## I. budget cap (check-then-reserve)

> 04 §5.4 L767-768 が源泉。本節は指示書の網羅領域 9 に対応する — 当時の gap inventory の
> U4 (対象外) と一部同一パラグラフを共有するが、契約化の対象は「ledger スキーマ・batch_requests
> 予約行との結合」に限定し、U4 固有の config パース詳細 (per_adapter の folder 側廃止等) には立ち
> 入らない。

### CL56 二層 cap: device cap が正、folder cap は任意の追加制限 [P0]
- 正本: 04 §5.4 L767 (『cap は二層で判定する。device cap（... デバイス上の全 .kio の当月合算に
  適用、既定 $50）が正であり、folder cap（... その .kio の当月消費のみに適用）は任意の追加制限。
  folder cap 未設定なら device cap のみが効く』)
- 前提: folder A (`.kio/config.toml` に `monthly_usd_cap` 未設定)、folder B
  (`monthly_usd_cap=10.0` 設定済み)。device cap = $50。
- 操作: 両 folder でタスクを起動しようとする。
- 期待: folder A は device cap ($50) のみで判定される。folder B は `min(folder残余, device残余)`
  で判定される (CL57 の判定式)。

### CL57 判定式: ledger+candidate<cap の同一 Tx check-then-reserve、per_adapter は device限定 [P0]
- 正本: 04 §5.4 L768 (『判定式: scope S の新規タスクを起動できるのは ledger(S, 当月) + candidate <
  folder_cap(S) かつ ledger(device, 当月) + candidate < device_cap のとき... per_adapter の下限は
  device 層専用（folder cap は total のみ）で、第三条件として同様に判定する: ledger(device,
  adapter_kind, 当月) + candidate < per_adapter_cap(adapter_kind)... 判定と相 1 の reservation
  作成は同一の BEGIN IMMEDIATE Tx で行う』)
- 前提: device cap=$50・当月消費$45、folder cap=$10・当月消費$8、per_adapter(markdownize) の
  device cap=$30・当月消費$29。新規タスクの candidate=$3 (markdownize)。
- 操作: cap 判定を実行する。
- 期待: 3 条件全てを評価: `45+3=48<50` (device OK)、`8+3=11 !< 10` (folder NG) →
  **起動不可**(folder cap 超過)。別ケースで folder cap を緩めて `8→5` にすると folder は
  `5+3=8<10` で OK になるが、`per_adapter` 条件 `29+3=32 !< 30` で NG → 依然として起動不可
  (3 条件は AND)。判定処理と (許可された場合の) `batch_requests` 相 1 行の INSERT/UPDATE が
  同一 `BEGIN IMMEDIATE` Tx で行われ、cap 超過時は相 1 行が作られない (Tx がロールバックまたは
  未着手のまま終わる) ことを確認する。

### CL58 candidate=0 のタスクは cap 判定対象外で常に起動可能 [P0]
- 正本: 04 §5.4 L768 (『candidate = 0 のタスク（単価 0 のローカル LLM — 下記）は cap 判定の対象外
  として起動できる（cap は外部支出の上限であり、超過状態でも無償タスクは封鎖しない）』)
- 前提: device cap・folder cap・per_adapter cap の全てが既に超過している状態。
- 操作: ローカル LLM (単価 0 宣言) のタスクを起動する。
- 期待: 3 条件の判定を経ずに起動が許可される (`candidate=0` は判定式そのものを評価しない
  ショートカット)。cap 超過状態の他タスクは `paused` のままだが、このタスクだけは影響を受けない。

### CL59 ledger(...) の構成 = cost_ledger当月合算 + batch_requests未終端予約合算 [P0]
- 正本: 04 §5.4 L768 (『ledger(...) は cost_ledger の当月合算（estimated 行も usd 非 NULL のため
  数値として効く — §5.8）+ 未終端 batch_requests（state 0/1）の estimated_usd 合算（= 予約）』)
- 前提: `cost_ledger` に当月確定行 (`usd=10.0`, `estimated=0`) と (`usd=2.0`, `estimated=1`) の
  2 行。`batch_requests` に未終端 (state=1) 行 (`estimated_usd=5.0`) が 1 行、terminal (state=2)
  行 (`estimated_usd=1.0`) が 1 行。
- 操作: `ledger(S, 当月)` を算出する。
- 期待: `10.0 + 2.0 + 5.0 = 17.0` (estimated 行も usd として合算され、未終端予約も合算されるが
  terminal 行の `estimated_usd` は cost_ledger に確定転記済みのため二重に数えない — この
  ケースでは terminal 行はもはや budget 判定の対象ではないことを確認)。

### CL60 sync 呼出の reservation は batch_requests 行 (cost_ledger ではない) [P0]
- 正本: 04 §5.4 L768 (『sync online 呼出は縮退 2 相に従う: reservation は cost_ledger ではなく
  batch_requests 行で行う』) — CL42 と同一根拠、budget cap の観点から再確認。
- 前提: sync 呼出 (query embedding 含む) の相 1 を実行する。
- 操作: cap 判定 + 相 1 を実行した直後の `cost_ledger` を検査する。
- 期待: `cost_ledger` には**何も追記されない** (相 1 段階では常に batch_requests のみが変化する)。
  予約額は `batch_requests.estimated_usd` としてのみ存在し、CL59 の「未終端 batch_requests 合算」
  経由で次の cap 判定に効く。

### CL61 per_adapter 設定キーの enum (markdownize/embedding/summary)、enum外はschema error [P1]
- 正本: 04 §5.4 L768 (『設定キー名 = adapter_kind と同一 enum: markdownize / embedding / summary。
  enum 外の未知キーは schema error』)
- 前提: `~/.config/kio/config.toml` の `[budget.per_adapter]` に `unknown_kind = 5.0` を設定。
- 操作: config を読み込む (schema validation)。
- 期待: `KIO-E-CONFIG-SCHEMA-001` (exit 2) で拒否される。`markdownize`/`embedding`/`summary` の
  3 キーのみが受理される。

---

## J. `kio batch abandon` CLI (U1 部分)

### CL62 CLI 構文と曖昧指定の拒否 [P0]
- 正本: 06 §1 L44 (`kio batch abandon <intent_token|scope/adapter/input_hash/tool_profile_hash>`)、
  L46-47 (『指定子は intent_token または batch_requests の 4 組タスクキー（3 組では別 profile 行と
  曖昧 — 曖昧時は拒否して token を要求する』)
- 前提: 同一 `(scope_id, adapter_kind, input_hash)` だが `tool_profile_hash` が異なる 2 行が
  `batch_requests` に存在する。
- 操作: (a) 有効な `intent_token` を指定。(b) 完全な 4 組キーを指定。(c) 3 組 (tool_profile_hash
  省略) のみを指定。
- 期待: (a)(b) は一意に対象行を特定して abandon を実行 (確認プロンプトへ進む)。(c) は 2 行の
  どちらを指すか曖昧なため拒否され、token を要求するエラーメッセージで終了する (usage error 相当、
  変更なし)。

### CL63 terminal な sync 行は intent_token が NULL 化済みのため4組キーで指定 [P0]
- 正本: 04 §5.8 L997-998 (『terminal な sync 行は intent_token が NULL 化済みのため 4 組キーで
  指定する』) — abandon selector にも同一規則が適用される (06§1 L47 の同形規定)。
- 前提: `request_kind='sync'` の terminal 行 (CL47 のとおり `intent_token` は既に NULL)。
- 操作: この行を `intent_token` で指定して abandon しようとする。
- 期待: 該当する `intent_token` が存在しない (NULL 化済み) ため一致せず、「対象なし」の冪等成功
  (CL66) になる — token 指定では絶対にこの行へ到達できない。4 組キーで指定した場合のみ正しく
  対象行を特定できる。

### CL64 abandon の効果: 確認済みユーザー操作で estimated 記帳 + terminal 化を同一 Tx [P0]
- 正本: 04 §5.8 L1079-1080 (『kio batch abandon... を脱出路とする: ユーザー確認で estimated
  記帳 + state=3（error='abandoned'）+ completed_at』)
- 前提: stalled 行 (CL37) に対して abandon を実行し、確認プロンプトで承諾する。
- 操作: abandon を実行する。
- 期待: 同一 Tx 内で (CL24 の submission_seq+1 規則に従い) `outcome='abandoned'` の
  `cost_ledger` 行が記帳され、`batch_requests.state=3`・`error='abandoned'`・`completed_at` が
  設定される。

### CL65 確認プロンプト必須、非対話/拒否は exit 9・無変更 [P0]
- 正本: 04 §5.8 L1079 (『ユーザー確認で estimated 記帳...』) / 06 §1 L49 (『確認プロンプト必須』) /
  04 §5.6, 10 §12.2 (exit code 9 = 『confirm 拒否（purge 等の確認プロンプトで no）』)
- 前提: stalled 行が存在する。
- 操作: (a) 確認プロンプトで "no" と応答。(b) 非 TTY 環境で確認手段を与えずに実行。
- 期待: (a)(b) いずれも exit 9 で終了し、`batch_requests`/`cost_ledger` に一切変更が無い
  (byte-for-byte 不変)。
  **解釈が割れうる点**: 06§1 の abandon 構文行 (L44) には他の破壊的コマンド (`purge --yes`,
  `restore --yes`) と異なり `[--yes]` の明記が無い — 非対話実行での確認スキップ手段が定義されて
  いるかは spec 上不明。§L note-4 参照。

### CL66 対象なしは冪等成功 (exit 0)、terminal確定済み・device行剪定後・re-abandonを包含 [P0]
- 正本: 04 §5.4 L769 (『剪定・確定済みの 4 組 key への kio batch abandon は対象なしの冪等成功
  （exit 0 + 「対象なし」表示）』) / 06 §1 L50-51 (『対象行が無い場合（terminal 確定済み・device
  行の剪定後を含む）は対象なしの冪等成功 — exit 0 + 「対象なし」表示』)
- 前提: (a) 既に `state=2`（成功）で完全終端・`intent_token` NULL の行を指定。(b) CL55 で剪定済み
  (行自体が存在しない) 4 組キーを指定。(c) CL64 で既に abandon 済み (state=3,
  error='abandoned') の行を再度 abandon しようとする (CL63 のとおり 4 組キー指定、かつ残骸掃除も
  完了済み)。(d) 存在したことのない架空の 4 組キー。
- 操作: 各ケースで `kio batch abandon` を実行する。
- 期待: (a)(b)(c)(d) の**全て**が exit 0 + 「対象なし」表示で終了する。(c) は二重に
  `submission_seq+1` の estimated 記帳を行わない (最初の abandon で既に確定済みの行を再度
  変更しない — 冪等)。確認プロンプトの要否 (対象が無いと分かった時点でプロンプト自体を出すか) は
  実装判断とする。

### CL67 abandon は sync 行にも適用可能 (job/upload 相の有無に関わらず) [P0]
- 正本: 04 §5.4 L768 (『sync 行は §5.8 の job / upload 照合・可視化猶予・回復期限の対象外（job /
  upload 相が無い）だが、abandon（同じ intent_token / 4 組指定）は適用できる』)
- 前提: sync 行 (`request_kind='sync'`, state=0, 生存中 = stale ではない)。
- 操作: この行を対象に `kio batch abandon` を実行する。
- 期待: batch 行と同じ CLI・同じ確認プロンプト経路で abandon が成功する。sync 行には
  upload/job 掃除の概念が無いため、CL64 の記帳+terminal化のみが行われ、CL47 のとおり同一 Tx で
  `intent_token` が即座に NULL 化される (batch 行のような「掃除完了待ち」は発生しない)。

### CL68 kio status の stalled 表示から abandon 実行までの一貫性 (U1) [P0]
- 正本: 04 §5.8 L1075-1076 (『恒久 unknown... の行は kio status に stalled として表示し（表示には
  intent_token を含める）』) + CL37/CL64 の接続確認。
- 前提: CL37 のとおり stalled 表示されている行。
- 操作: `kio status` の出力から `intent_token` を読み取り、それをそのまま `kio batch abandon
  <intent_token>` へ渡す。
- 期待: `kio status` が表示した `intent_token` が abandon の有効な selector として機能し
  (CL62(a))、abandon 成功後は同じ `kio status` の再実行でこの行がもはや stalled として表示されない
  (`intent_token` が NULL 化されるまでは掃除完了待ちとして別表示になり得るが、少なくとも
  「照会不能な in-flight」としての stalled 表示ではなくなることを確認する)。

---

## K. 横断規約

### CL69 KIO-E-STORE-CONSTRAINT-001: CHECK到達は実装エラー、permanent・exit4即時中止 [P0]
- 正本: 04 §5.8 L1045-1047 (『DDL の CHECK は最終防衛線であり、CHECK 違反で Tx が失敗した場合は
  実装エラー KIO-E-STORE-CONSTRAINT-001（permanent — ON CONFLICT DO NOTHING には吸収されず、
  同じ値での再試行はループするだけのため再試行しない）』) / 06 §7 L343-344 (『記帳 CHECK 到達 =
  実装エラー（04 §5.8）。permanent・非再試行で command を即時中止・exit 4』)
- 前提: 事前検証 (§E) を全て素通りしたにも関わらず (実装バグを想定して検証層を意図的に迂回した
  テストダブルで) CHECK 制約に違反する値を INSERT する。
- 操作: 記帳を実行する。
- 期待: `SQLITE_CONSTRAINT_CHECK` が実装エラー `KIO-E-STORE-CONSTRAINT-001` として即座に
  伝播し、コマンドが exit 4 で中止する。同じ入力での自動再試行は行われない (ON CONFLICT DO
  NOTHING はこの種の失敗を吸収しない — UNIQUE 違反と CHECK 違反は別の SQLite エラークラス)。

### CL70 cost-ledger.sqlite の配置・単一性・WAL/busy_timeout・schema変更regime [P0]
- 正本: 04 §5.4 L770 (『累積コストは... ~/.local/share/kio/cost-ledger.sqlite（デバイスグローバル
  1 個。WAL + busy_timeout）に記録する（.kio 内に ledger は置かない...）』) / 10 §7.5.3
  L690-692 (『cost-ledger.sqlite はこのデフォルトの対象外 — 再構築不可の運用台帳... であり、schema
  変更は常に... in-place migration 要件に従う』) / 03 §4.1 L319, L321 (第三分類の位置づけ)
- 前提: `XDG_DATA_HOME` 未設定・`HOME=/home/testuser` の環境。複数 `.kio` scope が存在。
- 操作: 各 scope でタスクを実行し、`cost-ledger.sqlite` の実ファイルパスを確認する。
- 期待: 全 scope が同一の単一ファイル `/home/testuser/.local/share/kio/cost-ledger.sqlite` を
  共有する (scope ごとに別ファイルにならない、`.kio/` 配下にも作られない)。接続時に
  `PRAGMA journal_mode` が `wal`、`PRAGMA busy_timeout` が 0 より大きい値に設定される
  (`crates/kio-index/src/registry.rs` の scope-registry.sqlite と同型の先例パターン)。
  `index/sqlite.db` や `scope-registry.sqlite` が `kio repair --rebuild-db` 等でデフォルト
  rebuild 可能なのと異なり、`cost-ledger.sqlite` の schema 変更は常に in-place migration
  (既存行保全) のみが許される — rebuild 相当のコマンドが `cost-ledger.sqlite` を re-create しない
  ことを確認する。

### CL71 非 current JSONL ledger の runtime fail-closed 契約 [P1]
- 正本: 10 §12.7 L1070 (リネーム表: 『cost-ledger.jsonl (+ -reservations / -reclaimed / .lock) |
  cost-ledger.sqlite（cost_ledger / batch_requests / schema_migrations の 3 表）| 04-pipeline.md
  §5.4』)
- 前提: current `cost-ledger.sqlite` と、旧 4 名および `.migrated` 名の非 current ファイルを用意する。
- 操作: `LedgerDb::open` を実行し、DB と非 current ファイルの bytes を前後比較する。
- 期待: `KIO-E-LEDGER-LEGACY-JSONL-001` で書込み前に失敗し、全 bytes が不変。非 current 形式の
  migration/rename/read path は存在しない。production source 内の文字列位置を走査する旧テストは、
  runtime failure を検出する固有 signal がないため `retired_jsonl_files_fail_closed_without_modification`へ統合した。

---

## L. 解釈が割れうる点 (spec の文言からは一意に決まらない — 勝手に決めない)

1. **purged 終端は `contract_violation_count` を increment するか**: 04 §5.8 L979-980 は
   「purge 済みなら出力を破棄し、下記の reject 終端と同形（error='purged'）で閉じる」と述べるが、
   「同形」が `contract_violation_count` の increment (L991 は「reject 終端 Tx で
   contract_violation_count を increment する」と述べるが、この一文は直前の contract_violation
   固有の文脈中にあり、purged 終端にも同一の increment を適用するかは文面上一意に読めない) まで
   含むのかは不明。expired 終端は明示的に「contract_violation_count は増やさない」(L1003) と
   除外規定があるのに対し、purged にはこの除外規定が無い — 除外規定の有無を根拠に「purged は
   increment する」と読むことも、「そもそも purged は §3.2 の受け入れ検査文脈の外 (tombstone は
   §3.5 purge の話) なので increment しない」と読むこともできる。CL19 はこの点を実装判断で
   固定した上でこの注記を参照するよう促す。

2. **device 行 stale_after_at の「参加 scope の実効値」の config 解決層**: 04 §5.4 L769 は
   「実効 timeout_seconds（device 行では参加 scope の実効値の最大値）」とだけ述べ、
   `[adapter.policy] timeout_seconds` が `~/.config/kio/tools.toml` (07§7 の掲載箇所から見ると
   device 側設定に見える) と各 scope の `.kio/config.toml` のどちらでオーバーライド可能かの
   優先順位、および scope ごとに異なる Adapter 実装 (異なる embedding provider) を使っている
   場合の「実効値」の定義が、本 2 節の文言のみからは確定できない。CL49 で実装判断として
   「素朴な max(各参加 scope の解決済み timeout_seconds)」を仮に固定したが、config 優先順位の
   詳細は 07-adapter-spec.md 側の該当契約 (本書の対象外) で確定させる必要がある。

3. **sync 行の unknown 精算 (§5.4 crash 回収) が §5.8 の submission_seq+1 規則を暗黙に共有するか**:
   04 §5.8 L1008-1011 の「記帳の冪等性」パラグラフは §5.8 (Batch 2 相) の文脈で
   `submission_seq+1` UPDATE を明記するが、04 §5.4 L768 の sync crash 回収パラグラフ自身は
   「未記録・照会不能なら unknown として estimated を確定記帳し state=3 で terminal 化する」と
   述べるのみで `submission_seq+1` を明示的に再言及しない。両者が同じ `cost_ledger` UNIQUE
   キー・同じ「job id 不明の記帳」パターンを共有する以上、同一規則が適用されると読むのが自然
   ではあるが (CL45 はこの読みを採用)、spec の 2 節が独立して改訂され得ることを踏まえると、
   明示的な相互参照が無い点は「解釈が割れうる」として記録しておく。

4. **`kio batch abandon` / `kio batch retry --reset-violations` の非対話確認スキップ手段**:
   06§1 L44 の abandon 構文行、および L26 の `--reset-violations` 構文行は、いずれも
   「確認プロンプト必須」とだけ述べ、`kio purge`/`kio restore` が持つ `[--yes]` 相当のフラグを
   構文上明記していない。CI やスクリプトから非対話実行する手段が「存在しない (常に TTY 確認が
   必須)」のか「単に構文表記から省略されているだけで `--yes` 相当が別途あるはず」なのかは
   06-cli-spec.md の当該行のみからは断定できない。CL65 は「非対話は exit 9」までを契約化し、
   `--yes` 相当フラグの有無自体は仕様確定待ちとして扱う。

5. **abandon が phase-1-only (相2a未着手、provider 側に一切残骸が無い) 行に対して実行された場合の
   残骸掃除の扱い**: 04 §5.8 の残骸掃除規定 (L1086-1087) は「terminal な task の upload
   （upload_id 記録分 + intent_token 埋込 filename の一覧照合分）を削除する」ことを述べるが、
   upload が一度も作られていない行 (相 1 のみで abandon された行) について「掃除対象が存在しない
   ため掃除は即時完了とみなす」と明記されていない。CL64/CL67 はこれを「掃除対象 0 件 = 掃除完了」
   と実装上解釈しているが、spec の文言はこの空集合ケースを明示していない。

6. **`--reset-violations` を非 terminal (state 0/1、count はまだ 0 or 1) の行に適用した場合の挙動**:
   06§1 L27 は「--reset-violations = 検証済み Adapter 更新後に contract_violation_count を 0 へ
   戻す」とだけ述べ、対象行が既に count<=1 (reset が実質無意味) の場合や、対象行が現在
   in-flight (state=1) の場合にエラーにするか no-op 成功にするかを規定しない。CL には含めず、
   本節の記録に留める。

## M. 裁定 (§L の解釈割れ — Phase 1 実装用、2026-07-21 オーケストレータ裁定)

1. **note-1 (purged 終端 × contract_violation_count)**: **increment しない**。violation カウンタは「Adapter の出力揺れ対策」であり、purged 終端は Adapter の落ち度と無関係 (§3.5 由来)。「同形」は Tx 構造 (state=3 + 記帳 + terminal 化を同一 Tx) の同形を指す。テストでこの挙動を固定し、spec への 1 句追記は Phase 4 の実装フィードバックへ記録。
2. **note-2 (device 行の実効 timeout)**: 仕様書の暫定を確定 — **max(各参加 scope の config 階層解決済み timeout_seconds)**。参加 scope = profile 互換で vector 検索に参加する scope (embedding adapter は参加 scope 間で同一 profile のため adapter 差は生じない)。
3. **note-3 (sync unknown 精算の submission_seq+1)**: **適用する** (CL45 の読みを確定)。同一 UNIQUE キー構造・同一「job id 不明の記帳」パターンであり、適用しないと同じ衝突問題が sync でも再現する。
4. **note-4 (abandon / --reset-violations の非対話)**: **--yes は追加しない** — spec の構文が正本であり、実装が構文外フラグを生やさない。非対話は exit 9 終端 + 対話端末での再実行を案内。--yes 追記の要望は Phase 4 の実装フィードバックへ記録。
5. **note-5 (phase-1-only abandon の掃除)**: 精密化 — **provider_scope_id 未記録なら upload は構造的に不可能のため照合不要で即掃除完了 (token NULL 化可)。provider_scope_id 記録済み・upload_id 不在は「記録前 crash の残骸」があり得るため token 埋込 filename の一覧照合を経てから完了**。CL64/CL67 の「掃除対象 0 件 = 完了」は前者のケースに限定して採用。
6. **note-6 (--reset-violations の no-op/in-flight)**: count=0 への適用は **no-op 成功** (exit 0・「変更なし」表示)。state 0/1 (in-flight) の行は**スキップして表示** (terminal 行のみ reset — claim 系との競合を避ける保守的既定)。仕様確定待ちタグ付きでテスト固定。

---

## 集計 (報告用)

- **契約総数**: 71 (CL01-CL71、番号連番に欠番・重複なし — grep 実カウント済み)
- **領域別内訳**: §A DDL 8 / §B 移行 4 / §C Batch 2相 9 / §D 冪等記帳 4 / §E outcome+事前検証 6 /
  §F crash回収 9 / §G sync縮退2相+idempotency 7 / §H device行 8 / §I budget cap 6 /
  §J abandon CLI 7 / §K 横断規約 3 (合計 8+4+9+4+6+9+7+8+6+7+3 = 71)
- **優先度内訳**: P0 = 65 / P1 = 6
- **解釈が割れうる点**: 6 件 (§L note-1〜6)。いずれも実装判断で暫定固定した上で spec 追記を促す
  形とし、勝手に spec 側を確定させていない。
- **対象外として明示的に切り出した項目**: U2 (Tier A/B 承認ゲート)・U3 (tasks.jsonl bounded
  compaction・hints 合成)・U4 の config パース詳細 (per_adapter folder 側廃止、markdown→
  markdownize キー rename の実装確認)・U12 (cost-ledger バックアップ/復元後 reconcile) は本書の
  対象外 — 別ロットの契約テストが担う。U1 のうち `hold_reason` 3 値 enum 化・
  `pending→paused→pending` 遷移・rate_limit の `paused` からの分離は 04 §5.1 契約であり同様に
  対象外 (本書は `kio batch abandon` + `stalled` 表示の部分のみ)。
