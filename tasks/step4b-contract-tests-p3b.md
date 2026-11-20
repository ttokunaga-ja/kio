# Step4b 契約テスト仕様書: CLI 横断 / exit・error 表 / log time-travel / その他 (P3-B)

> **Historical record, non-authorizing.** 現行 authority は本文が引用する canonical docs と Rust tests に限る。ID は review provenance のためだけに残し、compatibility、migration、CLI、schema、future work を authorize しない。

> 本書は **実装より先にテストを固定する** ための契約仕様。Rust 実装コードは含まない。
> 正本は `docs/06-cli-spec.md` **§7 (Exit Code) / §8 (Error Code Namespace) / log 構文 (§1 L61)**、
> `docs/10-operations.md` **§12 全体 (§12.1〜§12.8) / §3 (Scope Registry) / §4 (フォルダ運用) / §7 (Purge 保証範囲の周辺参照)**、
> `docs/05-runtime.md` **§2 (Commit/Snapshot — commit_type・shallow 時の log/inspect 分担) / §6 (Locking) / §8 (Auto Commit)**、
> `docs/03-data-model.md` **未消化の全域 (§1〜§11 — J 領域の schema/path/CAS/正本表)**、
> `docs/04-pipeline.md` **§1.1 / §2 / §2.2 / §4 全体 / §4.6-4.7 (J 領域の一次 source。06/10/05/03 の精読対象外だが
> J 領域の主要 spec § が本書にあるため引用する)**。各契約は spec の規範文からのみ期待値を導き、実装が
> 「どう書かれそうか」からは導かない。曖昧・spec 沈黙の点は該当契約の「期待」内に `[解釈割れ]` として
> 引用付きで注記し、末尾 §Z に一覧化する (勝手に決めない)。系譜は Phase 1/2 の
> ID 体系・優先度規約。記法は共通指示書 (`p2-contract-instructions.md`) が定める
> `### QB<連番> ... - 正本 / 前提 / 操作 / 期待` 形式。

**担当グループ**: P3-B (CLI 横断 / exit・error 表 / log time-travel / その他)。

**対象 U 項目 (当時の gap inventory)**:
K 領域 **U121-U128** (P2-C = `step4b-contract-tests-p2c.md` で入った分は差分のみ)、
L 領域 **U130-U142** (**U129 は Phase 4+ の GC 機構全体で対象外** — historical inventory が
「項目自身が Phase 4 要件・MVP 対象外と明記しており想定通り」と注記する未実装確認のみの項目のため、
本書でも一切扱わない)、
J 領域の残り **U95, U96, U98, U99, U100, U101, U102, U103, U104, U105, U106, U107, U108, U109, U110, U111,
U112, U114, U115, U116, U117, U118, U119** (**U97 / U113 / U120 は Phase 1 の対象であり本書は扱わない** —
byte_start/byte_end 全域改称・objects/image/ 単数形化・purge/epoch と tombstones/lifecycle-epoch の
layout 新設は Phase 1 で実装・検証済み)、
`log --at/--since` の本実装 (time-travel の log 面 — 06-cli-spec.md §1 L61 の構文のみ確定し
挙動未実装)、
P2 繰越 4 件 (`step4b-contract-tests-p2c.md` の **PC20** [index_generation の embedding/batch-finalize
回転契機]・**PC25/PC26** [query cache 再利用の multi-scope/pruning 境界]・**PC33/PC44** [per-binding
ancestor gate の実装配線])。

**対象外 (他グループ・他 Phase — 混同注意)**:

- A 領域 (cost-ledger 全体)・I 領域 (adapter 契約) — 別 Phase 3 グループ。本書は `KIO-E-ADAPTER-APPROVAL-CONFLICT-001` /
  `KIO-E-ADAPTER-SPECVER-001` (U124 が例示する新規コード群のうち adapter 承認 CAS 競合・spec_version 不一致の
  **具体的な発火条件**) には立ち入らない — これらは承認 publish/self-heal の内部設計 (07-adapter-spec.md §3/§8.1)
  を要し、本書の精読対象 (06/10/05/03) の外側にある。本書が扱うのは DOMAIN 一覧上の呼称・既存コードとの
  整合確認までである
- B/C/D/E/F/G/H 領域 — Phase 1/2 で契約済み（ID は provenance）。
  本書は参照するのみで再契約しない。特に H 領域の cursor/index_generation/query_cache/ancestor gate の
  **一般アルゴリズム自体** は `step4b-contract-tests-p2c.md` の PC19-PC44 が正本 — 本書の §E (P2 繰越) は
  それら契約が明示的に残した**未決の実装配線**にのみ追加契約する
- eval/ 配下の評価ハーネス実装そのもの (golden-queries.jsonl の生成・実行) — U142 の Recall@10 射影式は
  契約として固定するが、ハーネス自体の改修は別セッションの管轄

## 実装対象ファイルの見込み (現状把握の記録 — 実装方針を指図するものではない)

- `crates/kio-cli/src/main.rs` — `scope_unreachable_error` (L8552-8559, 唯一の呼出元 `resolve_scope_target`
  L7727-7749)、`ExitCode` 分岐、`ReadBarrierCheckpoint`/`check_index_generation_current` の呼出順序
  (log/diff/inspect が L627 型、open/view/search が purge-journal 優先型)、`Command::Log` (L623-640,
  `--at`/`--since` 双方が `not_implemented`)、preflight 関連の各種ヘルパ
- `crates/kio-core/src/scope.rs` — `validated_scope_id` (L1720-1741, schema 検証が `kio_format_version`
  判定より先行)・`validate_config` (L1695-1714, 同型のバグ)・`log()` (L1310-1337, HEAD 起点 first-parent
  walk のみ)・`open_scope_file_nofollow`/`stage_scope_file` (L2810-2977, ingest 安全規則)・no-op 判定
  (L1196-1214, tree_hash のみ比較)
- `crates/kio-core/src/dag.rs` — `TreeEntry`/`NormalizeRef` (L15-30, `manifest_hash` のみ)・`CommitObject`
  (`purged_raws` 保有・`CommitType` 検証)・`is_logical_direct_child`/`is_platform_safe_direct_child`
  (L116-136, path 拒否集合)
- `crates/kio-core/src/cas.rs` — `ObjectKind` (Raw/Tree/Commit)・`ContentObjectKind`
  (Prepared/Image/Embedding/Manifest/Toollock) の 2 enum 分離
- `crates/kio-core/src/portable.rs` — `portable_collision_key` (L15-23, NFC + 標準 Unicode lowercase、
  simple case folding ではない)
- `crates/kio-pipeline/src/prepare.rs` — `canonical_unit_key` (L225-233, XLSX sheet 名エスケープ未実装)・
  `lcs_fingerprint_pairs` (L447-477, tie-break が旧 index 優先)
- `crates/kio-pipeline/src/markdownize.rs` — `NormalizedInstanceManifest` (L142-151, `parent_instance`
  field 欠落)
- `crates/kio-index/src/fts.rs` — `chunks`/`chunk_config_generations`/`chunk_fts`/`chunk_vec`/
  `embeddings` の各 DDL、`index_metadata`
- `crates/kio-index/src/embedding_store.rs` — `EmbeddingTargetType::QueryCache` (未配線)・
  `write_chunk_embedding` (target_type='chunk' 固定)
- `crates/kio-index/src/registry.rs` — `scopes` DDL・`retire_stale_kio_path` (到達可能性を見ない削除)
- `crates/kio-core/src/xdg.rs` — 権限設定コード自体は無い (0700 は `registry.rs`/`ledger/schema.rs` に
  重複実装)
- `crates/kio-cli/src/search_history.rs` — `SearchHistoryBinding`/`SearchHistoryPlan` (per-binding 情報は
  保持済みだが per-binding config 解決は未接続、PC33/44 の土台)

---

## 0. ID 体系と優先度

| 接頭辞範囲 | 対象契約領域 | 対応 U 項目 |
| --- | --- | --- |
| QB1-QB9 (§A) | K: dead pointer 再分類・error_code 機械判定・preflight 順序・kio_format_version 検証順序 | U121-U128 |
| QB10-QB24 (§B) | L: lock/registry/scan境界/import-export/observability/エントリ文言 | U130-U142 (U129 除く) |
| QB25-QB49 (§C) | J: 耐久書込・unit/diff schema・SQLite DDL 精密化・tree schema・CAS 種別・path 規則 | U95,96,98-112,114-119 |
| QB50-QB58 (§D) | `kio log --at/--since` 本実装 | (06§1 L61 のみが正本、他は隣接規約からの導出) |
| QB59-QB66 (§E) | P2 繰越 (PC20/25/26/33/44 の未決配線) | (H 領域、PC20/25/26/33/44 の続き) |

**優先度**: **P0** = このロットの完了条件、1 件でも failing なら「K/L/J 残りの spec 追随完了」と呼べない。
**P1** = 推奨 (堅牢性・観測性・将来の schema 安定)。**P2** = 参考 (文書整合・軽微)。

---

# §A. K 領域 — error code / exit code / CLI 表示の横断 (U121-U128)

U123 (evidence verify 系 exit code 規則) は 5 つのサブクレームに分解できる: (1) `--strict` の
scope_unreachable-only → exit 3、(2) `unverifiable` の reason 別分岐、(3) sqlite.db 不在の統一規則、
(4) multi-scope SCOPE-ALL-FAILED の優先順位、(5) `kio open/view/restore` の dead pointer 再分類。
このうち (1)(2)(3) は `step4b-contract-tests-p2b.md` の PB53/PB55/PB56/PB57 が、(4) は
`step4b-contract-tests-p2c.md` の PC56 が既に契約化・的中確認済み (`kio evidence verify` 自身の
exit 分岐)。(5) は QB1 が扱う一般ヘルパのバグそのものであり、`kio evidence verify` 自身は既に
`scope_unreachable_error` の生エラーを構造化 (exit 0/3) に変換する独自経路 (`verify_objects.rs:118-130`)
を持つため QB1 の対象外 — U123 の実装完了は「PB53/55/56/57 + PC56 + QB1」の合成で成立する
(本書はこの事実を記録するのみで、新規契約は追加しない)。

### QB1 dead pointer の exit 3 再分類 (open / view / restore) [P0]
- 正本: 06-cli-spec.md §7 L370 (『kio open / view / restore      dead pointer (tombstoned / not_found) は 4。
  scope_unreachable は 3 (retryable — 08 §4.3)』) / 10-operations.md §12.2 L931 (『dead pointer
  (tombstoned / not_found) は `4`、**scope_unreachable のみは retryable の `3`**』)
- 前提: pointer の `scope_id` が registry / scope_path のどちらでも解決不能 (scope root が unmount・削除済み)。
  `kio open <pointer>` / `kio view <pointer>` / `kio restore <pointer> --to <dir>` の 3 コマンドで
  パラメタ化する。
- 操作: 各コマンドを到達不能な scope を指す pointer で実行する。
- 期待: 3 コマンドいずれも `KIO-E-EVIDENCE-SCOPE-UNREACHABLE-001` を返し、exit code は **3**
  (`ExitCode::PartialFailure`) である。**現状**: `scope_unreachable_error` (main.rs:8552-8559) は
  `ExitCode::PermanentFailure` (4) を返す唯一の実装であり、`resolve_scope_target` (main.rs:7727-7749) 経由で
  open/view (`resolve_pointer_for_cli`・`resolve_object_uri`)・restore (`restore.rs:126`
  `resolve_evidence_source`) の 3 経路すべてがこの exit 4 を継承する。3 コマンドとも同一の単一関数が
  原因であるため、修正は 1 箇所だが検証は 3 コマンドそれぞれで独立に行う (呼出経路が異なるため
  回帰の再発箇所も独立)。

### QB2 成功応答 (exit 0) の error_code は失敗判定に使わない [P1]
- 正本: 06-cli-spec.md §8 L381 (『**成功応答 (exit 0) に載る `error_code` は縮退原因の分類であり、
  失敗判定には使わない** — 失敗判定は exit code (非 0) が正 (例 = text fallback の
  05-runtime.md §1.7 応答契約)』)
- 前提: search が embedding 未承認等の理由で text fallback した応答 (`error_code` に
  `KIO-E-SEARCH-VEC-UNAUTHORIZED-001` 等の非 null 値が入りつつ結果は正常に返る)。
- 操作: プロセスの exit code と、出力 JSON の `error_code` フィールドの両方を機械的に取得する。
- 期待: exit code は **0** であり、`error_code` が非 null であることは exit code の値に一切影響しない
  （非 null 値を持つ実応答で exit 0 を観測する）。private marker や実装位置の文字列探索は正本にせず、
  公開応答の exit と JSON を組で固定する。

### QB3 軽量規範 3 件の現状固定確認 (EMBED domain 適用時点・fallback_reason 自由語彙・CONFIG-SCHEMA-001 確定) [P2]
- 正本: (a) 03-data-model.md §7 (『**modality は `"multimodal"` に固定する**...採用不可であり、tool-lock への
  materialize / adapter 登録の時点で `KIO-E-EMBED-MODALITY-001` (exit 2...) として拒否する』) /
  (b) 06-cli-spec.md §9 L429-431 (『fallback_reason は自由語彙 — 閉 enum にしない。機械判定は error_code 側が
  正であり、Agent は未知の fallback_reason 値を無視してよい』) /
  (c) 06-cli-spec.md §11 L503 (『validation 失敗は **exit 2** + `KIO-E-CONFIG-SCHEMA-001`』)
- 操作: (a) `TEST_ADOPTED_EMBEDDING_ENV=non_multimodal` で `kio index --approve` を起動する。(b) 現行 task
  wire に provider-defined の任意文字列を `fallback_reason` として入れ、公開 `kio status` で読む。(c) 不正な
  config.toml で任意コマンドを実行。
- 期待: (a) exit 2 / `KIO-E-EMBED-MODALITY-001`、tool-lock/index は未公開。(b) unknown 値を拒否・変換せず
  同値で返す。(c) 常に `KIO-E-CONFIG-SCHEMA-001`。型名・enum・実装ファイル探索は行わない。

### QB4 preflight (0)-(4) 同時違反優先順位マトリクス [P0]
- 正本: 10-operations.md §3 L300-305 (『**複合状態の優先順位 (全コマンド共通の preflight 順序)**: (0)
  `kio_format_version` 互換判定...→ (1) purge journal / epoch 検査 → (2) registry live 重複
  (KIO-E-REGISTRY-DUP-001) → (3) index 可用性 (KIO-E-INDEX-REBUILDING-001) → (4) command 固有の検査。
  ...同時成立時は先順の error を返し』)
- 前提: 単一 scope が同時に 2 つ以上の preflight 条件に違反する組合せをパラメタ化する: (a) (0)+(1)
  (format-version 非互換 かつ active purge journal)、(b) (1)+(2) (active purge journal かつ registry
  live 重複)、(c) (2)+(3) (registry live 重複 かつ sqlite.db 不在)、(d) (0)+(3) (format-version 非互換
  かつ sqlite.db 不在)。
- 操作: 各組合せで `kio log <path>`/`kio view <pointer>` を実行する (読取系代表 2 種)。
- 期待: いずれの組合せでも、より若い番号の条件に対応する error_code が返る ((a)→`KIO-E-STORE-VERSION-001`
  exit 8、(b)→`KIO-E-PURGE-JOURNAL-ACTIVE-001` exit 3、(c)→`KIO-E-REGISTRY-DUP-001`、
  (d)→`KIO-E-STORE-VERSION-001` exit 8)。より若くない番号の error は一切観測されない。**現状**:
  `LC53` (lifecycle.md) が (1)-(3) の単独発火は個別に契約化済みだが、**同時発生時にどちらが勝つか**を
  証明する組合せテストは存在しない (単一違反の直列列挙のみ)。(0) 自体 (kio_format_version) を preflight
  順序に組み込む契約も無い。

### QB5 restore の preflight 順序異常 (format-version が purge-journal より先行) [P0]
- 正本: 10-operations.md §3 L300-302 (『(0) kio_format_version 互換判定...(1) purge journal / epoch 検査』、
  (0) は「他のすべての検査に先行する」と明記) — この一般順序が restore にも適用されるべきという前提のもと、
  現行の open/view の実装順序 (purge-journal → index-generation → format-version、後述 QB6 参照) との
  **内部不整合**を pin する。
- 前提: 同一 pointer に対し (i) format-version 非互換、(ii) active purge journal、を同時に満たす scope
  への `kio restore <pointer> --to <dir>` 実行。
- 操作: restore を実行し、返る error_code を観測する。
- 期待: `KIO-E-STORE-VERSION-001` (exit 8) が返る — (0) が最優先という一般規則どおり。**現状**:
  `restore.rs:113-129` (`resolve_evidence_source`) は `resolve_scope_target` (purge-journal 検査より前に
  format-version チェックを含む `Repository::open` を内包) を先に呼び、呼出元 `run()` が
  `ReadBarrierCheckpoint::open`/`check_index_generation_current` (purge-journal/index-generation 検査) を
  **その後**に実行する — つまり restore は (0)→(1)→(3) の順で、open/view の (1)→(3)→(0) 順 (QB6) とは
  逆順であることが確認されている。restore 単体では現状のまま (0) が先に評価されるため、本契約自体は
  **現状で通過している可能性が高い** (現状固定寄り) が、QB6 (open/view) との**相互不整合**自体が
  U127 の「全コマンド共通の順序」という規範に反する — 契約として両方を固定し、どちらか一方に統一する
  実装判断を促す。[解釈割れ] は §Z を参照。

### QB6 読取系コマンド間の preflight 順序不整合 (log/diff/inspect vs open/view/search/evidence-verify) [P0]
- 正本: 10-operations.md §3 L300-311 (同上、「全コマンド共通の preflight 順序」と明記し「読取系はこの順序を
  冒頭 1 回適用し」と続く — コマンド種別による順序差異を許容する文言は無い)
- 前提: (i) format-version 非互換 かつ (ii) active purge journal を同時に満たす scope。
  `kio diff <a> <b>` (main.rs:641-651 型) と `kio open <pointer>` (resolve_scope_target 経由) の
  2 コマンドでパラメタ化する。
- 操作: 各コマンドを実行し、返る error_code を比較する。
- 期待: 両コマンドとも同一の error_code (`KIO-E-STORE-VERSION-001`、(0) 優先) を返す — コマンド種別に
  依らず同じ判定結果になる。**現状**: `diff`/`log`/`inspect` は `Repository::open_current()` (format-version
  含む) → `ReadBarrierCheckpoint::open` (purge-journal) の順 (main.rs:627-633 型) で (0)→(1)。一方
  `open`/`view` は `resolve_scope_target` の内部で purge-journal 相当の `ReadBarrierCheckpoint::open` を
  format-version 判定 (`Repository::open` 内) より**先に**実行する (main.rs:7417-7430 型) — (1)→(0) の
  逆順。したがって同一の 2 条件同時違反シナリオで、`diff` は exit 8 (VERSION) を返すが `open` は exit 3
  (journal) を返す可能性が高く、コマンド間で結果が食い違う。**具体的な回帰対象**: この不一致自体を
  failing test として固定する。

### QB7 書き込み系コマンドの (0)(2)(3) 適用確認 [P1]
- 正本: 10-operations.md §3 L305-307 (『(3) は復旧・初期化コマンド自身...には適用しない...kio status も
  拒否対象外』の除外規定から逆に、除外されない書き込み系コマンドには (0)(2)(3) が適用されると読める。
  06-cli-spec.md §10 の REGISTRY-DUP 引用文 (10§3 L296-299)『live 重複が解消するまでは、当該 scope_id での
  **書き込み系コマンドと online タスク起動 (相 1) も** `KIO-E-REGISTRY-DUP-001` で fail-closed とする』)
- 前提: registry に同一 scope_id の live clone が 2 つ存在する状態 (KIO-E-REGISTRY-DUP-001 の発火条件)。
  `kio index` (復旧・初期化コマンドではない通常の書き込み系) を対象にする。
- 操作: `kio index --approve` を実行する。
- 期待: `KIO-E-REGISTRY-DUP-001` で fail-closed に拒否される (index を開始しない)。**現状**: 書き込み系
  コマンド (`run_index`/`run_reindex`/`run_repair`/`run_batch`/`purge::run`) はいずれも `Repository::open_current()`
  (format-version = (0)) → `lock_store()` → tool-lock 検証、という直線的な流れのみで、registry live 重複
  ((2)) や sqlite.db 可用性 ((3)) を明示的に検査する preflight 呼出は main.rs の各 `run_*` 冒頭に見当たらない
  (`ReadBarrierCheckpoint`/`check_index_generation_current` は読取系専用ヘルパで書き込み系からは呼ばれない) —
  (2) の適用有無は未確認区分として本契約が検証対象にする。

### QB8 kio_format_version 判定の schema validation 前倒し (scope.json) [P0]
- 正本: 03-data-model.md §2 L154 (『**互換判定は scope.json の schema validation より先に評価する** —
  自己の対応上限より新しい version の store は未知 key の schema error に入らず **read-only + 新版誘導**
  で縮退する』) / 10-operations.md §12.3 L948 (『この検証は `kio_format_version` の互換判定より**後**に
  走る — 自己の対応上限より新しい version の store は schema validation に入らず read-only + 新版誘導で
  縮退する』)
- 前提: `.kio/scope.json` の `kio_format_version` が自己の対応上限より新しく、かつ同じ scope.json が
  現行 schema にとって未知の key を含む (将来 MINOR bump で追加されたであろう key を模擬)。
- 操作: このスコープに対し任意の読取系コマンド (`kio status`) を実行する。
- 期待: `KIO-E-STORE-VERSION-001` (exit 8) が返り、`KIO-E-CONFIG-SCHEMA-001` (exit 2、未知 key の
  schema error) は返らない。**現状**: `validated_scope_id` (scope.rs:1720-1741) は
  `validate_json_schema(SchemaKind::Scope, &value)?` を**先に**実行し、`kio_format_version` の
  `validate_format_version(version)?` はその後・かつ `Some(version)` の場合のみ実行される — 未知 key を
  含む scope.json は `kio_format_version` の値を見る前に `KIO-E-CONFIG-SCHEMA-001` で弾かれる。QB4/QB6 の
  一般順序不整合とは独立に、**この 1 関数内の順序そのもの**が規範と逆であることを直接 pin する。
- [解釈割れ]: `.kio/config.toml` も `kio_format_version` を保持し (`Repository::init` が両ファイルに
  書き込む、scope.rs:243-257)、`validate_config` (scope.rs:1695-1714) が全く同型の順序バグを持つ。
  しかし 03-data-model.md §2 L154 が明示するのは `scope.json` の `kio_format_version` フィールドのみで
  あり (『保存場所 = `.kio/scope.json` の `kio_format_version` フィールド』)、config.toml 側のコピーが
  同じ規範の対象かは spec が明言しない。config.toml 側の同型バグは注記に留め、本契約は scope.json に
  限定する。

### QB9 新版 store + 未知 key の縮退シナリオ (書込ゼロ・即時拒否の確認) [P0]
- 正本: 10-operations.md §12.5 L1000 (『書き込み系コマンド...は当該 store に対して**即時拒否** —
  error_code `KIO-E-STORE-VERSION-001`・exit 8...単独 scope 指定の読み取り系 (log / view / open / inspect /
  evidence verify / status / diff / 単独 search) は store への**書込ゼロ**で best-effort 動作する』)
- 前提: QB8 と同一の「新版 store + 未知 key」シナリオ。書き込み系 (`kio index`) と読取系 (`kio status`)
  の 2 コマンドでパラメタ化する。
- 操作: 各コマンドを実行する。
- 期待: 書き込み系は即時 `KIO-E-STORE-VERSION-001` (exit 8) で何も書き込まない (raw object 保存や
  SQLite への書込が一切発生しないことを、実行前後のファイル一覧・mtime 差分で確認)。読取系も同じ
  error_code を返すが、これは「書込ゼロの best-effort」であり検索や inspect 等の**副作用を伴わない**
  操作である限りにおいて許容される、という区別を明示する。**現状**: QB8 の順序バグにより両方とも
  実際には `KIO-E-CONFIG-SCHEMA-001` (exit 2) を返すため、この期待そのものが QB8 の修正を前提とする —
  QB8 が直る前提での事後条件として記述する。

---

# §B. L 領域 — その他 (U130-U142、U129 除く)

### QB10 kio view の構文修正確認 (`<commit>` 単独 → `<path> --at <commit>`) [P1]
- 正本: 05-runtime.md §2.2 L534 (『shallow 後の commit を対象に view した場合 (`kio view <path> --at <commit>`
  — 文法の正本は 06-cli-spec.md §1。commit の metadata 表示は `kio log` / `kio inspect` 系が担う)』) /
  05-runtime.md §4.2 L1009-1011 (『kio view <evidence-at-commit-X> / kio view <path> --at <commit>』)
- 前提: `View(UnsupportedArgs)` (main.rs:180, `Vec<String>` の柔軟パーサ) 経由で `kio view` を呼ぶ。
- 操作: (a) `kio view <path> --at <commit>` (path 位置引数 + `--at` flag) の形式で呼ぶ。(b) 過去の
  `kio view <commit>` 単独形式 (commit のみで path を指さない) で呼ぶ。
- 期待: (a) は受理され、当該 commit 時点の `<path>` の内容を返す (`normalize.manifest_hash` が指す
  manifest object 由来の unit 完成状態を使う — PB45 で既に契約化済みの部分、参照のみ)。(b) は
  `KIO-E-CONFIG-USAGE-001` (exit 2) で拒否される (commit 単独ではメタ情報しか持たず、表示すべき path が
  一意に定まらないため — commit のメタ表示自体は `kio log`/`kio inspect` の責務であるという spec の
  役割分担に従う)。**現状**: `read_pointer_input` (main.rs:6840-6845) は最初の引数を pointer として
  読むのみで、`<path> --at <commit>` という 2 引数 (位置引数 + flag) の組合せを明示的に処理する分岐は
  未確認 — 本契約が実際の受理形式を固定する。

### QB11 writer/read contention の既存 behavior coverage [P1]
- 正本: 05-runtime.md §6 L1034-1038 (『`.kio/.lock` を取得するコマンド (書き込み系): kio index /
  kio snapshot ... / kio batch resume / kio batch retry / kio batch abandon / kio reindex /
  kio adapter revoke』) / 同 L1052 (『読み取り系 (search / log / view / open / inspect / evidence verify /
  restore / status / diff) は `.kio/.lock` を取得しない』)
- 操作・期待: 実際の writer/read 排他は `contract_cli::m1_concurrent_index...`、batch resume は
  `step3::o3_batch_resume...`、reindex/search は P10 の runtime contract が担当する。本書は private call-site
  数や位置を正本化しない。

### QB12 purge competition の runtime coverage [P1]
- 正本: 05-runtime.md §6 L1058 (『複合 lock 順序は scope store → cost-ledger.sqlite (Tx) → device
  observability → scope access とし、逆順取得を禁止する』)
- 操作・期待: `step4_purge::ct4_purge_acquires_publication_lock_before_publishing_barrier` ほかの実競合と
  non-mutation assertion を採用する。source offset・transaction count・cost-ledger との未実証の相対順序は
  主張しない。

### QB13 scope 由来 log append の scrub lock + 3 点検査 同一 critical section 化 [P1]
- 正本: 05-runtime.md §6 L1058 (『**scope 由来 log の append 順序**: 読取系が対象の path / query /
  raw_hash を含む行を append する場合、当該 append は scrub lock を保持したまま、3 点検査 (§6 —
  journal 不在 + epoch 不変 + lifecycle counter 不変) の**最終検査と同一 critical section** で行う —
  scrub 完了後の再 append で purge の削除 postcondition を破らない。最終検査で拒否した場合の記録には
  対象 path / query / raw_hash を含めない』)
- 前提: `access.jsonl` へ `raw_hash` を含む行を append する読取系コマンド (`kio open <raw_hash>`) の
  実行中に、並行して同一 raw_hash への purge が完了する競合。
- 操作: (a) 3 点検査通過 → append 正常系。(b) 3 点検査中に purge journal が active化 (競合検出) →
  append 拒否系、をそれぞれ発生させる。
- 期待: (a) は raw_hash を含む access.jsonl 行が append される。(b) は raw_hash を含む行が
  **append されない** (拒否記録自体には raw_hash/path/query を含めない — 拒否理由の error_code のみ
  記録)。append の可否判定 (3 点検査) と実際の write が同一 critical section (scrub lock 保持区間) 内で
  行われ、その間に他プロセスの scrub が割り込まないことを、lock 保持区間の排他性で確認する。**現状**:
  `enforce_purge_read_barrier`/`ReadBarrierCheckpoint` による 2 点/3 点検査自体は Phase 1 で契約済み
  (LC52-56) だが、**access.jsonl append と scrub lock の結合**は本書が初めて検証対象にする。

### QB14 registry 再構築の入力範囲 + `scrub.lock` パス解決 (現状固定、パラメタ化 2 件) [P2]
- 正本: (a) 05-runtime.md §6 L1057 (『**再構築の入力はユーザーが知る探索 root** — registry 喪失後は
  `.kio` の所在一覧も失われるため、各 root での `kio index` 再実行が再登録を兼ねる。Kio が自力で
  全ディスクを走査することはしない』) / (b) 同 L1058 (『device logs では
  `${XDG_DATA_HOME:-$HOME/.local/share}/kio/logs/scrub.lock`』)
- 前提: (a) 別 XDG state で初期化済みだがこの registry には未登録の sibling scope。(b) `XDG_DATA_HOME`
  環境変数が未設定の実行環境。
- 操作: (a) primary root の `kio index` 後に observable registry rows を読む。(b) `XDG_DATA_HOME` 未設定で
  search log を生成し、`$HOME/.local/share/kio/logs/scrub.lock` を保持したまま再検索する。
- 期待: (a) primary だけが登録され、sibling を発見・登録しない（recursive/full-disk scan は実行しないことを
  registry の結果で確認）。(b) `scrub.lock` は
  `$HOME/.local/share/kio/logs/scrub.lock` に解決され、保持中は対応する metrics append が抑止される。
  カレントディレクトリ相対の不正パスにはならない。Cargo.lock/dependency の文字列検査を正本にしない。

### QB15 VCS リポジトリ root 配下の子 `.kio` 生成除外 [P1]
- 正本: 03-data-model.md §3 L267 (『**VCS リポジトリ root (`.git` 等の VCS 管理ディレクトリを持つフォルダ)
  とその配下にも既定では子 `.kio` を生成しない** (skip + status 表示。`[scope] index_vcs_repos = true`
  で opt-in) ... **本既定の導入以前に生成済みの既存子 `.kio` は grandfathered**』)
- 操作: 通常の file-bearing subdirectory、`.git` directory を持つ subdirectory、および regular
  gitfile の `.git` を持つ subdirectory を置き、`kio index --preview` と通常の `kio index` を実行する。
  次に親 scope の `[scope] index_vcs_repos = true` を設定して再実行する。
- 期待: preview は `child_scopes` に `planned` / `skipped_vcs` を返し、子 `.kio` を作らない。通常実行は
  通常の file-bearing directory を `indexed` として child scope 化する一方、VCS root とその descendants
  は `skipped_vcs` として表示し生成しない。opt-in 時だけ VCS root も child scope 化する。探索は parent
  scope の ignore を先に、root `.kioignore` を後に適用し、`.kio`、directory symlink、unreadable directory
  を安全側に扱う。既存 child scope の grandfather 専用分岐は置かない。

### QB16 `kio import --as-new-scope` の fork 機構 [P1]
- 正本: 06-cli-spec.md §10 (『bundle の scope_id が registry に live 登録済みなら拒否
  (`KIO-E-REGISTRY-DUP-001`)...複製として取り込むには `--as-new-scope` で新 scope_id を採番...fork は
  旧 scope の approvals[]・初回スキャン承認 (scan_approval)・adapter.policy.allow_network を引き継がない
  ...`.kio/logs/` は継承しない (空で開始)』)
- 前提: `kio import` コマンド自体が `Command` enum に存在しない (`Import` variant 無し、`Gc`/`Move`/
  `Evidence` のような `UnsupportedArgs` placeholder としてすら存在しない — コマンド文字列自体が
  未定義)。
- 操作: (将来実装後) 既に live 登録済みの scope_id を持つ `.kioz` bundle を、`--as-new-scope` 付き/
  無しでそれぞれ import する。
- 期待: `--as-new-scope` 無しは `KIO-E-REGISTRY-DUP-001` で拒否。付きは新 ULID の scope_id を採番し、
  展開後の scope.json には旧 `approvals[]`/`scan_approval`/`approvals_initialized`/`approval_pending`
  が存在せず、`config.toml` の `allow_network` は `false` にリセットされ、`.kio/logs/` は空
  (旧 scope の行を一切持たない) で開始する。展開・sanitize は private directory 内で完結させてから
  atomic に publish する (scope.json のみ新・config のみ旧という中間状態を外部に見せない)。**現状**:
  機構自体が完全に未実装 (0 件) — 本契約は将来実装への先行固定。

### QB17 `.kioz` bundle の機微 metadata 含有警告 [P2]
- 正本: 06-cli-spec.md §10 末尾 (『bundle には scope.json の approvals[]・logs/ の運用記録・登録 path
  等の機微 metadata が含まれる — 共有は同一信頼境界内 (自分の別端末・バックアップ) を想定し、第三者
  公開用の sanitize (承認・log・path の除去) は Phase 4+ の export mode で扱う』)
- 前提: `.kioz` の export/import 機構自体が未実装 (`kioz` grep 0 件、`export`/`Export` grep 0 件)。
- 操作: (将来実装後) `kio export <scope> --to <bundle.kioz>` を実行する。
- 期待: 生成された bundle には approvals[]・logs/・登録 path がそのまま含まれ、CLI は出力またはドキュメント
  上でこれらが機微情報であり第三者への公開を想定しない旨を明示する。第三者公開用の sanitize (承認・log・
  path 除去) は明示的に提供しない (Phase 4+ の別モード)。**現状**: 機構自体が未実装 — 本契約は将来
  実装への先行固定 (export/import 実装 Step の一部として満たすべき安全要件)。

### QB18 observability config key の改称 (`[logs]` → `[observability]`) と scope_id 必須化 [P1]
- 正本: 10-operations.md §12.3 L954 (『log 保持の正規 key = **`[observability] retention_days`**
  (整数 1〜3650・既定 30)...device logs...と scope-local `.kio/logs/access.jsonl` の双方に適用する』) /
  §12.6 L1033-1038 (『**scope 由来の行は context.scope_id を必須とする**...複数 scope に跨る行は
  scope_id を持たない — そのためこれらの行には raw_hash / path / query 等の対象由来値を記録しない』)
- 前提: `~/.config/kio/config.toml` に `retention_days` を設定した状態。
- 操作: (a) `[observability] retention_days = 60` を設定して読み込む。(b) 現行の `[logs] retention_days`
  設定を読み込む。(c) scope 由来のログ行 (`access.jsonl` 1 行) の必須 field を検査する。
- 期待: (a) が実効値として反映される (現行は無視されるべき形式)。(c) 全ての scope 由来行が
  `context.scope_id` を持つ。**現状**: `config.schema.json:101-107` は `"logs"` セクションのみを定義し
  `"observability"` は grep 0 件 — `read_logs_retention_days` (scope.rs:2033-2042) も `"logs"` key を
  読む。scope_id 必須化の検査は既存の `access.jsonl` 書込コード (main.rs:6593-6607 型) が `scope_id`
  を含めているかの確認から行う (このサブクレーム自体は概ね充足している可能性が高いが、schema レベルで
  required 化されているかは未確認)。

### QB19 scrub lock 下の search log append behavior [P2]
- 正本: 10-operations.md §12.6 L1041-1045 (『**scope-local の `.kio/logs/access.jsonl` も同じ規範の
  対象とする** (日次 rotation + 保持日数は同 config・既定 30 日 — 無操作でも検索対象であり続ける scope
  の unbounded 成長を防ぐ。purge の scrub は**全保持世代**に適用する — rotation は scrub の対象範囲を
  狭めない)』)
- 前提・操作: device `scrub.lock`、scope `access.scrub.lock` をそれぞれ保持して search を実行し、対応する
  metrics/access log の bytes を前後比較する。解除後に再検索する。
- 期待: search は成功し、保持中は該当 log append だけが抑止され、解除後はそれぞれ再開する。writer 定義数や
  source scan は採用しない。

### QB20 ingest 走査時の symlink TOCTOU 対策 (現状固定) [P0]
- 正本: 10-operations.md §4 L359-363 (『symlink lstat 基準で検出し、**追跡しない**...**判定と open の
  TOCTOU も閉じる**: 取り込みの open は scope root の dirfd からの相対 open + `O_NOFOLLOW` 相当で行い、
  open 後の fstat で regular file・同一 device/inode を検証する』)
- 前提: scope 直下に、取り込み対象の実ファイルへの symlink を配置する (走査時に TOCTOU 攻撃を模擬:
  lstat 判定後に symlink 先を差し替える)。
- 操作: `kio index` を実行し、symlink エントリの取り込み可否と、判定後の差し替えに対する耐性を確認する。
- 期待: symlink は追跡されず skip + status 表示される。判定後にファイル実体が差し替わった場合は
  `scope_file_changed` エラーで当該ファイルの取り込みを拒否する (中間状態を取り込まない)。**現状**:
  `open_scope_file_nofollow` (scope.rs:2810-2841) が lstat → O_NOFOLLOW open → fstat 照合の 3 段階検査
  を既に実装しており規範と一致する。本契約は現状固定の回帰ロック。

### QB21 system directory の built-in ignore パターン欠如 [P1]
- 正本: 10-operations.md §1.1 L128-130 (『system directory (§4 の走査境界既定) も Tier A 相当の
  built-in 除外に含め、**OS 別の対象パターンは built-in template に列挙し、その template の版を
  `effective_ignore_hash` の入力に含める**』)
- 前提: OS 別の system directory (Unix の `/proc`・`/sys`、Windows の `C:\Windows\System32` 等) を
  スコープ配下に模擬できる環境 (シンボリックリンクやマウントで代用可能な範囲)。
- 操作: system directory 相当のパスを含む scope で `kio index --preview` を実行する。
- 期待: system directory は Tier A 相当の built-in ignore として自動的に除外され、preview の
  「除外済み」欄に表示される。**現状**: `is_tier_a_secret_name` (scope.rs:2710-2717 型) は secrets
  パターンのみを判定し、system directory 用の別パターンリストは grep 0 件 (`/proc`・`/sys`・
  `system director` いずれも crates 全域で 0 件)。

### QB22 `effective_ignore_hash` の template 版連動欠如 [P1]
- 正本: 10-operations.md §1.1 L129-130 (『その template の版を `effective_ignore_hash` の入力に
  含める』 — パターン更新が承認記録の同一性判定に反映されるように)
- 前提: built-in ignore template (Tier A + system directory、実装されれば) のバージョンが将来更新される
  シナリオ。
- 操作: `effective_ignore_hash` の算出コードを検査し、その入力に template バージョン識別子が含まれるか
  確認する。
- 期待: template のバージョンが変われば `effective_ignore_hash` も変わる (承認記録の再確認契機になる)。
  **現状**: `effective_ignore_hash` は `hash_bytes(b"built-in-tier-a-v1")` (main.rs:15299 型) という
  **固定リテラル**の hash であり、実際の `.kioignore` 内容や config ignore パターンを一切入力に含めない
  — 「パターン更新が承認記録に反映される」という規範の意図 (ユーザー設定込みの実効 ignore 内容が
  変われば再確認が促される) を現状のハードコード定数は満たさない。QB21 の system directory 追加時に
  この値も連動して変わるべきだが、現状は変わらない。

### QB23 U138/U139/U140/U141 複合現状固定確認 (エントリコマンド文言・purge/erase 例外注記・
  Phase4 auto snapshot 改称・構造化 API 除外、パラメタ化 4 件) [P2]
- 正本（historical inventory 統合要約からの二次引用 — 原典 README.md / 01-positioning.md は本書の精読対象外）:
  (a) 『最低体験ラインの入口を `kio snapshot` から `kio index --approve` に変更する...`kio open` も
  引数なしから `kio open <検索結果のpointer>` に変更する』(U138)。
  (b) 『Evidence Pointer/CAS の恒久性の説明に「ユーザー明示の purge/erase を除く」という例外を明記する』
  (U139)。
  (c) 『「Phase 4: 自動化/auto snapshot」を「定期 auto snapshot」に改称し、取り込み完了時 auto snapshot
  は MVP である旨を明記する』(U140)。
  (d) 『MVP では外部 Agent 向けの構造化 API サーフェスを持たず、`--json` フラグ出力のみで足りる』(U141)。
- 前提: (a) `kio index`/`kio open` の引数要件。(b)-(d) はいずれも文書表現の確認であり実装への直接
  影響が薄いと historical inventory が注記する項目 (「過剰抽出の疑い」「適合済みの可能性」)。
- 操作: (a) `kio index` を引数無しで実行、`kio open` を引数無しで実行。(b)-(d) は README.md /
  01-positioning.md の該当箇所 (本書の管轄外) を確認する。
- 期待: (a) `kio index` 単独実行は非対話環境で `KIO-E-CONFIG-USAGE-001` (exit 2) となり、`--approve`/
  `--yes` が実質的なエントリゲートである。`kio open` 単独実行 (pointer 無し) も同じく exit 2。
  (b)(c)(d) は実装への影響が無いことを確認する現状固定 (b: purge/erase 後の到達不能性は
  Phase 1 の LC 系が実装確認済み。c: 取り込み完了時 auto snapshot は
  `main.rs:723` 型で MVP コード経路に既に存在。d: `--json` 以外の構造化 API 面は grep 0 件)。
  **現状**: (a) `IndexArgs`/`read_pointer_input` は clap レベルでは required 化されていないが
  `run_index`/`read_pointer_input` の手書き検証で実質的に必須化されている (main.rs:744-754/6840-6845) —
  機能的には充足。本契約は 4 項目とも軽量な現状固定確認として 1 本に圧縮する。

### QB24 U142 Step 割当表整合 + Recall@10 射影の `path_at_commit` 追加 [P1]
- 正本（historical inventory 統合要約からの二次引用 — 原典 09-mvp-scope.md §3/§4.3 は本書の精読対象外）:
  『実装割当表に `kio adapter revoke` (Step 2)、`kio repair --registry-prune` (Step 3)、
  `kio repair --rebuild-db` (Step 3)、`--prune-orphans` (Step 4) を新規追加する...`--all-history`
  シナリオ (M3-2) の Recall@10 計算を、旧 distinct 射影 `(raw_hash, section)` から新
  `(raw_hash, section, path_at_commit)` に変更し、リネーム前後を別要素として数える (golden-queries.jsonl
  の expected 要素 format にも `path_at_commit` フィールドを追加)』
- 前提: (a) 各コマンドの実装 Step 割当が spec の表と矛盾していないか (`--rebuild-db` は実装済み、
  `--registry-prune`/`--prune-orphans` は Step 3/4 に割り当てられ現時点で未実装であるべき)。(b)
  `eval/` 配下の Recall@10 計算ロジックと `golden-queries.jsonl` の expected 要素 schema。
- 操作: (a) `--rebuild-db`/`--registry-prune`/`--prune-orphans` それぞれの実装有無を確認する。(b)
  `eval/` 配下で Recall@10 を計算する既存コード (存在すれば) の distinct 射影キーを確認する。
- 期待: (a) `--rebuild-db` は実装済み (main.rs:922-972 型)、`--registry-prune`/`--prune-orphans` は
  spec の Step 3/4 割当どおり現時点で未実装 (grep 0 件) であり、この不在自体が Step 割当違反ではない
  ことを確認する (誤って先行実装されていないかの回帰ロック)。(b) 将来 `--all-history` の評価を実行する
  際、Recall@10 の distinct 射影キーに `path_at_commit` を含める (同一 chunk がリネーム前後で別要素と
  数えられる)。**現状**: `search_history.rs` の `SearchHistoryBinding` は既に `path_at_commit` field
  (L33) を保持しており射影キー変更の基盤は存在するが、`eval/` 配下の実際の Recall@10 計算式が
  この 3 要素射影を採用しているかは本書の対象ファイル (crates/) 外であり別途確認を要する
  ([解釈割れ] §Z に記載)。

---

# §C. J 領域 — schema / path / CAS / 正本表の残り (U95,96,98-112,114-119)

**U116 について**: 「kio_format_version の保存場所・判定タイミング確定」の判定タイミング部分は
§A QB8/QB9 が scope.json を対象に既に扱う (同一関数 `validated_scope_id` の同一バグ) — 本節では
再契約しない。以下 J 領域の契約は U95〜U119 のうち U97/U113/U120 (Phase 1 済み) と U116 (§A 参照) を
除いた残りを扱う。

### QB25 単一 open + 読後 stat 照合、mtime 同一秒トラップ回避規則 (パラメタ化 2 件) [P0]
- 正本: 04-pipeline.md §1.1 L37-38 (『**単一 open**: raw_hash の計算と保存する bytes は**同一の open・
  同一のストリーム**から得る...hash 用と保存用に 2 回 open すると、その間の書き換えで「hash A の名前に
  内容 B」が保存され得る』) / L39-40 (『読み取りの前後で stat (size, mtime) が同一であることを確認し、
  変化していたら当該ファイルはこの実行では取り込まず次回へ回す』) / L41-43 (『**racy 規則**...「stat が
  前回と同じなら再 hash を省略する」最適化は、ファイルの mtime が前回判定時刻と**同一秒以降**の場合は
  適用してはならない』)
- 前提: (a) 通常の ingest 対象ファイル 1 件。(b) 「stat 一致なら再 hash 省略」という最適化が実装されて
  いると仮定した場合の、mtime が判定時刻と同一秒であるファイル。
- 操作: (a) `stage_scope_file` (scope.rs:2916-2977) の読み取りループを追跡し、hash 計算用の read と
  staging 書込用の write が同一 `File` ハンドルの同一読み取りループ内で行われているか確認する。
  読み取り完了後に size を再 stat して比較していることも確認する。(b) 該当最適化のコードパスを
  grep する。
- 期待: (a) は既に充足 — `source.read(...)` の同一バッファを `hasher.update()` と `staged.write_all()`
  の両方に渡し (単一 open・単一ストリーム)、読み取り完了後に `source.metadata()?.len() != total` で
  size 照合する。**ただし mtime は照合しない** (現状は size のみ、`same_scope_file_identity` の
  mtime 比較は unix/windows では有効化されない) — spec の「stat (size, mtime) が同一」という要求に
  対し mtime 側が欠けている点を pin する。(b) は当該最適化自体が実装に存在しないため、racy 規則も
  vacuous に充足する — 本サブクレームは「実装する場合の要件」として先行固定するのみで、現時点では
  failing にならない。

### QB26 新規中間 directory (fan-out shard) の mkdir→親 fsync 連鎖 + 削除操作の fsync 順序 [P1]
- 正本: 04-pipeline.md §1.1 L57-62 (『**新規に作成した中間 directory (fan-out shard 等) は、mkdir →
  親 directory fsync を既存の耐久済み directory に到達するまで連鎖してから、当該 subtree 配下の
  publish を行う**...**削除 (unlink / rmdir — purge の deleted 相等) も同様に、各削除後に包含
  directory を fsync してから journal phase / postcondition を前進させる**』)
- 前提: CAS object の fan-out shard (`objects/<type>/ab/cd/`) がまだ存在しない新規 raw_hash の
  ingest (`ab`/`cd` の 2 階層ディレクトリを新規作成する必要がある状態)。
- 操作: 新規 fan-out shard を要する raw object の書込みを実行し、`ab` 作成 → fsync → `cd` 作成 →
  fsync → object publish、の順序が守られているかをファイルシステム操作トレースで確認する。
- 期待: 各中間ディレクトリの mkdir が親ディレクトリの fsync 完了後に行われ、最終的な object publish は
  全中間ディレクトリの fsync 完了後にのみ行われる。**現状**: CAS 書込みの中間ディレクトリ作成箇所
  (`cas.rs` の fan-out 書込み関数) における mkdir→fsync 連鎖の有無は未確認区分 — 本契約が具体的な
  検証対象を固定する。

### QB27 XLSX sheet 名の `#` エスケープ + 重複連番規則 [P1]
- 正本: 04-pipeline.md §2 L125-128 (『sheet: シート名 (NFC 正規化のみ...)。**元名に含まれる `#` は
  `##` へ escape** してから、同名重複の 2 つ目以降に "#2", "#3" を付す (可逆・決定的 — sheet:Sheet1,
  sheet:Sheet1#2 — 出現順。実名 "A#2" は sheet:A##2 となり "A" の 2 枚目 sheet:A#2 と衝突しない)』)
- 前提: XLSX ファイルに `Sheet#1` という `#` を含むシート名と、同名シートが複数存在する状態。
- 操作: `canonical_unit_key(UnitType::Sheet, "Sheet#1")` を呼ぶ。同名シートが 2 回登場するケースも
  試す。
- 期待: `"Sheet#1"` → `unit_key = "sheet:Sheet##1"` (エスケープ後)。同名 2 回目は `"sheet:Sheet1#2"`
  のように出現順の連番が付く。エスケープ後の `##` と連番の `#N` は互いに衝突しない。**現状**:
  `canonical_unit_key` (prepare.rs:225-233) の `UnitType::Sheet` 分岐は `format!("sheet:{selector}")`
  で無条件にそのまま埋め込み、エスケープも連番も行わない。さらに `prepare_units_from_bytes`
  (prepare.rs:169) の sheet 名生成自体が `"Sheet1"` 固定のハードコードであり、XLSX 実ファイルの
  MIME type は現行の prepare エントリゲートで `empty_prepare_output()` に落ちるため、この分岐は
  現状到達不能である (XLSX 取り込み自体が未実装)。本契約は `canonical_unit_key` 関数の単体契約として
  記述し、XLSX 全体の取り込み実装状況とは独立に検証できるようにする。

### QB28 LCS unit-mapping の tie-break を辞書順最小に固定 [P1]
- 正本: 04-pipeline.md §2.2 L189-192 (『同スコアの LCS 対応が複数ありうる...ため、**tie-break = 対応
  ペア列を (旧 index 列, 新 index 列) の辞書順で最小になるものを選ぶ** (完全順序 — 旧 index 昇順だけ
  では新側の重複を順序付けられない)』)
- 前提: 旧 unit 列 `[A, A]` (fingerprint 同一の 2 unit) と新 unit 列 `[A]` (1 unit) という、複数の
  LCS 対応が同スコアで存在するケース (`(0,0)` と `(1,0)` のどちらを選んでも LCS 長は 1)。
- 操作: `lcs_fingerprint_pairs(&[A,A], &[A])` を呼び、返るペアを観測する。逆方向 (旧 `[A]` × 新
  `[A,A]`) も試す。
- 期待: 辞書順最小の対応ペア列が選ばれる — `[A,A]` × `[A]` では `(0,0)` (旧 index 0 を選ぶ方が
  `(0,0) < (1,0)` として辞書順最小)。**現状**: `lcs_fingerprint_pairs` (prepare.rs:447-477) の
  backtrack は `dp[i+1][j] >= dp[i][j+1]` で `i` (旧 index) を優先的に進める実装であり、これは
  「常に旧 index を先に消費する」という決定的だが辞書順最小性を明示的に証明しない発見的規則である。
  規範の「(旧 index 列, 新 index 列) の辞書順最小」という完全順序の定義と、現在の DP backtrack が
  生成する対応列が全ケースで一致するかは未証明 — 本契約は具体的な複数対応シナリオで両者が一致する
  ことを機械検証する (不一致があれば tie-break 規則の明示的な実装が必要になる)。

### QB29 `chunks` / `chunk_config_generations` DDL の精密化 (パラメタ化 4 列) [P0]
- 正本: 04-pipeline.md §4.1 L385-386 (『chunk_id TEXT NOT NULL PRIMARY KEY, -- rowid 表の TEXT PRIMARY KEY
  は NOT NULL を含意しないため明示』) / L432-434 (『UNIQUE(chunk_id, chunking_config_hash) -- pair-only
  creation/order metadata』) / 03-data-model.md §8 (`chunks.chunk_id`・
  `embeddings.id`・`schema_migrations.name` の 3 列を `TEXT NOT NULL PRIMARY KEY` にする、U98 統合要約)
- 前提: 現行 `sqlite_master` の DDL 文字列。
- 操作: (a) `chunks.chunk_id` の列定義。(b) `embeddings.id` の列定義。(c)
  `chunk_config_generations` の UNIQUE 制約の列数。(d) それぞれの `sqlite_master.sql` を照会する。
- 期待: (a)(b) は `TEXT NOT NULL PRIMARY KEY` (NOT NULL 明示)。(c) は `(chunk_id, chunking_config_hash)`
  の pair UNIQUE。**現状**: (a) `fts.rs:568-590` の現行 DDL は
  `chunk_id TEXT PRIMARY KEY` (`NOT NULL` の明示なし — SQLite の rowid 表では `TEXT PRIMARY KEY` 単独は
  NOT NULL を含意しないため、規範上は明示が必須)。(b)(c) は本契約作成時点で `embeddings`/
  `chunk_config_generations` の DDL 全文を再確認する必要がある未検証区分 (先行する 4.3 節の抜粋は
  `embeddings.id` の型を明示していない箇所があった)。本契約は 4 列 (a)-(d 相当) の DDL 文字列を
  `sqlite_master` から取得し canonical 比較する形で機械検証する。

### QB30 chunk 境界正準規則: UCD Script property 化 + `unicode_version` の hash 組込み (パラメタ化 2 件) [P0]
- 正本: 04-pipeline.md §4.1 L448-453 (『日本語文字...(**UCD の `Script` property** (Script_Extensions は
  使わない — U+30FB 等で判定が分かれる) が Hiragana / Katakana / Han の文字 + 長音記号 ー U+30FC・々
  U+3005 に固定...使用する UCD 版は chunking config の `unicode_version` として hash 入力に含める』) /
  03-data-model.md §5.3 (`chunking_config_hash` の入力に `unicode_version` を含める式)
- 前提: (a) 々 (U+3005、繰り返し記号) を含む見出しテキスト。(b) `chunking_config_hash` の算出関数。
- 操作: (a) `is_japanese('々')` を呼ぶ。(b) `chunking_config_hash` の JCS 入力 JSON を検査し
  `unicode_version` キーが含まれるか確認する。
- 期待: (a) は `true` を返す (々 は UCD Script=Han に準じる日本語文字として扱われるべき)。(b) は
  `unicode_version` (既定 "17.0.0") が hash 入力に含まれる。**現状**: `is_japanese()`
  (chunking.rs:362-367) はハードコードされた Unicode range 判定 (平仮名/片仮名/CJK 統合漢字ブロック)
  であり UCD `Script` property を参照しない。々 (U+3005) は現行の range 判定から漏れている (range が
  CJK 統合漢字ブロックのみで拡張領域の記号を含まない)。`unicode_version` は grep 0 件で
  `chunking_config_hash` の入力に一切含まれない。

### QB31 `chunk_fts` / `chunk_vec` DDL の現状確認 (パラメタ化 2 件、現状固定寄り) [P1]
- 正本: 04-pipeline.md §4.2 L482-489 (『tokenize='trigram' -- 既定。設定で 'unicode61 remove_diacritics 2'
  へ切替可 (切替時は許可値 enum から DDL を生成する。プレースホルダの literal 実行は parse error —
  掲載 DDL は常に実行可能形とする)』) / §4.3 L538-541 (『embedding float[768] distance_metric=cosine』)
- 前提: 現行の `chunk_fts`/`chunk_vec` 仮想表 DDL。
- 操作: (a) `ensure_schema_on_connection` (fts.rs:633-660) が生成する `CREATE VIRTUAL TABLE chunk_fts`
  文字列を、既定設定 (`FtsTokenizer::Trigram`) で取得する。(b) `chunk_vec` の DDL 文字列を取得する。
- 期待: (a) 常に実行可能な `tokenize='trigram'` (プレースホルダ literal ではない) が埋め込まれる。
  (b) `float[768] distance_metric=cosine` が固定で埋め込まれる。**現状**: 両方とも既に規範と一致する
  (tokenizer は `FtsTokenizer` enum から `format!` で埋め込まれ、本番呼出は全て `Trigram` を渡す —
  fts.rs:633-660。次元は `CHUNK_VEC_DIMENSIONS = 768` の定数固定 — fts.rs:664-674/885)。本契約は
  現状固定の回帰ロックとして機械検証を残す (トークナイザ切替時に別の許容値へ変わった場合に検知する)。

### QB32 `embeddings(target_type)` 専用 index の新設 [P1]
- 正本: 04-pipeline.md §4.3 L534-536 (『CREATE INDEX idx_embeddings_type ON embeddings(target_type);
  -- query_cache の 256 行剪定・列挙が corpus 全 embeddings を SCAN しないための index』)
- 前提: `embeddings` 表が corpus 規模 (数万行) の embedding を保持する状態。
- 操作: `sqlite_master` を照会し `embeddings(target_type)` を対象とする index の存在を確認する。
  `EXPLAIN QUERY PLAN` で `target_type='query_cache'` の絞り込みクエリが index scan になるか確認する。
- 期待: `idx_embeddings_type` (または同等の index) が存在し、`target_type` での絞り込みが
  table scan ではなく index scan になる。**現状**: `CREATE INDEX ... ON embeddings` は grep 0 件 —
  index が存在しない。

### QB33 query_cache の書込パス + 読出しパス新設 (パラメタ化 2 件、PC25/26 の schema 面) [P0]
- 正本: 04-pipeline.md §4.3 L548 (『**query cache (`target_type='query_cache'`)**: cursor replay が
  pin する page 1 の query vector の正本...**INSERT と 256 行剪定は同一 SQLite Tx で cursor 返却前に
  完了する**...書込は検索に参加した各 scope の sqlite.db へ行い、上限は**scope あたり 256 行**
  (超過時は最小 rowid の行から削除)』)
- 前提: vector|hybrid 検索の page 1 実行 (query embedding 成功、まだ `embeddings` 表に query_cache 行が
  無い状態)。
- 操作: (a) page 1 検索を実行し、実行直後の `embeddings` 表を照会する。(b) 256 行を超える連続 page-1
  検索を発生させる。
- 期待: (a) `target_type='query_cache'` の行が 1 件 INSERT され、`target_id` = query_vector_digest
  (`"sha256:" + base16(sha256(vector BLOB))`)、cursor が呼出元へ返る前に INSERT が完了している。
  (b) 257 件目の INSERT 時に scope あたりの行数が 256 を超えないよう最小 rowid の行が削除される
  (同一 Tx 内)。**現状**: `EmbeddingTargetType::QueryCache` (rows.rs:37-44) は enum variant として
  存在するが、`grep "target_type.*query_cache"` は crates 全域で INSERT/SELECT の実処理コードに
  一致しない (唯一の `INSERT INTO embeddings` である `write_chunk_embedding` は `target_type='chunk'`
  固定)。query embedding のキャッシュ機構自体は `compute_query_embedding_page1`
  (main.rs:10860-10958) が実装するが、これは `kio_pipeline::ledger` (cost-ledger.sqlite 側の
  device-row protocol) を経由するものであり、本節が規定する `kio-index` 側の `embeddings` 表への
  書込とは**別の仕組み**である — 本契約が指す schema 面の書込は未実装。

### QB34 query_cache の読出し (cursor replay 再利用) パス新設 [P0]
- 正本: 05-runtime.md §1.5 (該当節、QB33 の隣接文脈 — `step4b-contract-tests-p2c.md` PC25 が既に引用
  済み: 『vector / hybrid の replay は page 1 の query vector を再利用する — query の再 embedding は
  行わない』) / 04-pipeline.md §4.3 L548 (『この行だけは `objects/` から再構築できないため
  `kio repair --rebuild-db` では復元せず破棄する』)
- 前提: vector|hybrid 検索の page 1 を実行済み (QB33 が正しく機能していれば query_cache 行が存在する
  状態)。embedding adapter を「呼び出しごとに異なるベクトルを返すモック」に差し替える。
- 操作: `--cursor <token>` で page 2 を replay する。
- 期待: embedding adapter が page 2 の実行中に**呼び出されない** (モックが 2 回目に呼ばれないことを
  確認)。page 1 で保存済みの query vector を読み出して vector 検索に使う。**現状**:
  `compute_query_embedding` (main.rs:10968-10989、page 2+ replay 用) は doc comment 自身が
  「does not participate in the device row protocol」と明記したうえで、**無条件に embedding
  adapter を再呼出しする** — 本契約は QB33 と対になる読出し側の未実装を pin する (この 2 契約が
  揃って初めて PC25 の期待が満たされる)。

### QB35 `chunk_vec` rebuild 導出の profile_hash 限定 + 0/1 件検証・2 件以上 corruption [P1]
- 正本: 04-pipeline.md §4.3 (『**結合は現行 tool-lock の embedding profile に限定する** —
  `(profile_hash, dimensions, distance, modality)` が現行 lock と一致する embedding 行のみ...chunk
  ごとに候補が **0 件または 1 件**であることを検証する...**2 件以上のみ corruption**として rebuild
  停止する』)
- 前提: 同一 chunk (同一 `text_hash`) に対し、旧 profile と現行 profile の 2 件の embedding 行が
  並存する状態 (embedding profile 更新後の rebuild シナリオ)。
- 操作: `kio repair --rebuild-db` を実行し `chunk_vec` の再構築を観測する。同様に、同一 chunk に
  現行 profile の embedding 行が偶発的に 2 件重複する (データ破損模擬) ケースも試す。
- 期待: 旧 profile の行は無視され、現行 profile の 1 件のみが `chunk_vec` に展開される
  (0 件なら pending として text-only 継続、chunk_vec 行を作らない)。現行 profile の重複 2 件以上は
  corruption として rebuild を停止する。**現状**: `rebuild_chunk_vec`
  (embedding_store.rs 該当関数) の実装がこの profile 限定フィルタと 2 件以上検知ロジックを持つかは
  historical inventory 記載時点で「不在」と判定されており、本契約が具体的な検証シナリオを固定する。

### QB36 embedding CAS object の bytes 構築 (JCS+LF+base64+LF+digest) [P1]
- 正本: 03-data-model.md §8.1 (『embedding object の保存 bytes は **`JCS(identity fields) + LF +
  base64(vector, float32 little-endian) + LF + lower_hex64(sha256(vector bytes))`** に固定する』)
- 前提: 1 件の embedding (768 次元 float32 vector) を CAS object として書き込む操作。
- 操作: embedding object を `objects/embeddings/ab/cd/<embedding64>` へ書き込み、生成された bytes を
  3 行 (JCS 行・base64 行・digest 行) に分解して検証する。
- 期待: 1 行目は identity fields (`spec_version`/`target_type`/`target_hash`/`profile_hash`/
  `modality`/`dimensions`/`distance`) の canonical JCS。2 行目は vector の float32 little-endian
  base64。3 行目は 2 行目がデコードした bytes の sha256 (小文字 hex、64 文字)。fsck はこの digest 行を
  再計算し bit flip を検出できる。**現状**: SQLite 側の `embeddings.vector` は生の little-endian f32
  BLOB (`f32_to_le_bytes`、embedding_store.rs:416-433) であり base64 化されない。CAS 側の
  `EmbeddingObject` (cas.rs:61-103) は JSON (JCS) 直列化された `{dimensions, vector: Vec<f64>}` で
  あり、base64/LF 区切り/digest フッタの形式ではない。さらに `ContentObjectKind::Embedding` を書き込む
  `write_content_object` の呼出元は grep 0 件 (fsck の読み手 `verify_embeddings` のみが存在) — 本契約が
  指す書込側の bytes 構築は完全に未実装。

### QB37 non-current tables 非採用 + UTF-8 preimage 規則 + commit_type 検証機構の
  記述訂正 (複合 regression confirmation、パラメタ化 3 件) [P2]
- 正本: (a) 03-data-model.md §4.1 L321 (『旧 spec が SQLite テーブルとして定義していた `files` /
  `normalization_runs` / `prepared_units` は採用しない』)。(b) 03-data-model.md §8.1 (『文字列を
  preimage とする hash...は、規定の正規化を適用した後の UTF-8 バイト列に対して sha256 を計算する』)。
  (c) 05-runtime.md §2.1 (『**enum の強制点は commit object の schema 検証 (publication 時の loader)**
  である...commit は CAS JSON object であり SQLite に commit 表は存在しない』)
- 前提: (a) 初期化・index 後の現行 SQLite schema。(b) 公開 `unit_ref` API。(c) public commit object boundary。
- 操作: (a) `sqlite_master` で `files`/`normalization_runs`/`prepared_units`/`commits` が不在であることを
  観測する。(b) Unicode input に対する frozen `unit_ref` を照合する。(c) 不正な purged commit object を
  public validation boundary に渡す。
- 期待: (a)(b)(c) は runtime/API 境界で規範と一致する。DDL/関数位置/grep を正本にしない。

#### Fixture reachability audit

M3-3 の `legacy-format-v0.md` と `deprecated-approach.md` は Rust manifest/test から到達し、固有の
deleted-history signal を作るため保持する。current Rust/manual manifests から到達しない tracked evaluator
fixtures は 0 件である。`normalized-corpus` は current fixture ではなく、意図的に非 authorizing な paid OCR
archive として保持する。歴史 measurement outputs も fixture producer ではないため、この cleanup の削除対象外である。

### QB39 `kio diff` の derived-only 変化検出義務 [P1]
- 正本: 06-cli-spec.md §1 L94 (『`kio diff` は raw / path の差分に加え、
  `normalize_manifest_changed` / `chunking_config_changed` / `tool_lock_changed` /
  `resurrection_published` を差分として表示する。derived-only commit を「差分なし」と表示してはならない』)
- 前提: 2 つの commit `Ca`/`Cb` で各 path とその `raw_hash` は同一だが、same-gen partial
  retry の finalize によって `Cb` の tree では `manifest_hash` が変わり、別の `tree_hash` になる。
- 操作: `kio diff Ca Cb` を実行する。
- 期待: raw/path 差分が 0 件であっても `normalize_manifest_changed` を含む差分エントリが返り、
  「差分なし」ではなく derived-only の変化として表示される。current tree の必須 field が欠落すれば
  diff は fail-closed する。**現状**: `Repository::diff` (scope.rs:1339-1374) は
  `tree_map` (path→raw_hash のみ) を比較し `added`/`deleted`/`modified` のみを返す — derived-only の
  5 種変化検出は完全に未実装であり、上記シナリオでは無条件に空の差分 (=「差分なし」相当) を返す。

### QB40 `parent_instance` フィールドの新設 (raw 跨ぎ incremental の三つ組) [P0]
- 正本: 03-data-model.md §8 (normalization_runs 節): 『**incremental で親の raw が異なる場合 (raw 更新を
  またぐ通常 incremental) は `parent_instance = {raw_hash, tool_profile_hash, gen}` を必須で記録する**
  — `parent_gen` は同一 raw 内の局所番号であり、整数だけでは親 instance を一意に復元できない
  (full では null)』
- 前提: raw_hash が変化したファイルに対する incremental Markdownize (旧 raw の instance を参照する
  再利用を含む)。
- 操作: incremental モードで新 instance の manifest を生成する。
- 期待: 新 instance の manifest は `parent_instance: {raw_hash: <旧raw_hash>, tool_profile_hash: <...>,
  gen: <...>}` を持つ (旧 raw_hash が現行と異なるため `parent_gen` の整数だけでは復元不能)。full 実行
  では `parent_instance` は null。**現状**: `NormalizedInstanceManifest`
  (markdownize.rs:142-151) は `parent_gen`/`run_id` のみを持ち `parent_instance` field は grep 0 件 —
  未実装。

### QB41 prepared_hash 変化駆動の自動 gen+1 経路 (第二の合法経路) [P1]
- 正本: 03-data-model.md §2.1 (『`gen = 現最大 + 1` の新 instance を作れるのは `kio reindex --force` と、
  **prepare profile / renderer 変更による `prepared_hash` 変化が駆動する再 Markdownize** (§6 —
  first-instance-wins の第二の合法経路。オンライン課金を伴うため 04 §4.6 と同型の確認プロンプト +
  budget guardrail の対象) だけであり』)
- 前提: 既存 instance (gen=0) を持つ raw_hash に対し、prepare Adapter の renderer version が変更され
  `prepared_hash` が変化する状態 (`--force` 明示指定は無い)。
- 操作: `kio index` (通常の incremental 経路) を実行する。
- 期待: `prepared_hash` 変化を検出し、`--force` 無しでも新 gen (gen=1) の instance が作られる (自動
  gen+1)。オンライン課金を伴うため確認プロンプト (または `--yes` 相当) を要求し、budget guardrail の
  対象になる。**現状**: gen+1 を駆動する経路は `kio reindex --force` のみが確認されており (U105 の
  historical inventory 統合要約に基づく)、prepared_hash 変化起因の自動トリガーは未確認区分 — 本契約が
  具体シナリオを固定する。

### QB42 `up_to_date` 判定 state machine の全面新設 [P0]
- 正本: 03-data-model.md §6 (完全な python-like 疑似コード): 『`elif not inst.units: up_to_date` (空
  unit 集合を最優先判定)...`elif all(u.status == "failed" for u in inst.units): if all(permanent):
  settled else: failed`...`elif any(u.status == "done" and not unit_object_exists(u)):
  missing_output`...`elif any(u.status == "failed"): partial else: up_to_date`』
- 前提: 4 種類の instance 状態をパラメタ化する: (a) unit 集合が空 (空文書)。(b) 全 unit が失敗かつ
  全て permanent。(c) 一部 unit が `done` 宣言だが object が欠落し、かつ他の unit が failed
  (retryable) である併存ケース。(d) 一部 unit のみ failed (retryable)。
- 操作: 各状態の instance に対しファイル状態判定関数を呼ぶ。
- 期待: (a) `up_to_date` (空虚真で `failed` に誤分類されない)。(b) `settled` (terminal、再投入対象
  なし)。(c) `missing_output` (partial 判定より前に検出され、failed unit との併存下でも欠落を
  見逃さない)。(d) `partial`。**現状**: `up_to_date`/`tool_changed`/`missing_output`/`settled` という
  状態機械のラベル・判定関数は grep 0 件 — 最も近い実装は `Repository::status()`/`FileStatus`
  (scope.rs:111-135、working-tree-vs-HEAD の `new`/`modified`/`deleted`/`unchanged` のみ) と
  `NormalizationRunStatus`/`UnitStatus` (markdownize.rs、5 値/2 値の簡略状態) であり、本節が規定する
  9 状態の統合判定ロジックは存在しない。

### QB43 no-op snapshot 判定への `tool_lock_hash` 比較の追加 [P0]
- 正本: 05-runtime.md §8.1 (『**no-op 判定は tree_hash に加えて commit の `tool_lock_hash` も比較する**
  — embedding profile のみの更新でも lock が変われば commit を作る』) / 03-data-model.md §8.2
  (『例外 = resurrection finalize と tool_lock_hash の変化 — no-op 判定は tree_hash と commit の
  tool_lock_hash の両方を比較する』)
- 前提: tree_hash が HEAD と一致するが、embedding profile 更新により `tool_lock_hash` が変化した
  状態 (resurrection ではない通常のケース)。
- 操作: `kio index`/`kio reindex --force` の auto snapshot 判定を実行する。
- 期待: tree_hash が一致していても `tool_lock_hash` が HEAD と異なる場合は no-op とせず新規 commit を
  作る。**現状**: no-op 判定 (scope.rs:1196-1214) は `resurrection_candidates.is_empty()` と
  `head_tree_hash == tree_hash` のみを条件にし、`tool_lock_hash` の比較は行わない (tool_lock_hash は
  この判定より後、新 commit を実際に作る段階で読み取られるのみ) — tool_lock_hash のみが変化し
  tree_hash が不変のケースは誤って no-op になる。

### QB44 auto snapshot 契機 3 (`kio batch resume`/`kio batch retry`) の未配線確認 [P1]
- 正本: 05-runtime.md §8.1 (『**3. `kio batch resume` / `kio batch retry` / `kio reindex --force`
  がオンライン成果 (normalized / chunk) を finalize した成功完了時も同様に auto snapshot を作る**』)
- 前提: pending の online task を持つ scope で `kio batch resume`/`kio batch retry` を実行し、
  online 成果 (normalized/chunk) の finalize が成功する状態。
- 操作: 各コマンドの実行後、HEAD の tree_hash が finalize 成果を反映した新 commit を指すか確認する。
- 期待: `kio batch resume`/`kio batch retry` いずれも成功 finalize 後に auto snapshot (commit_type=auto)
  を作る。**現状**: `auto_snapshot_with_bound_normalize` の呼出元は `run_index` (main.rs:790-809) と
  `run_reindex` (main.rs:4503-4522) の 2 箇所のみ確認されており、batch resume/retry の経路からの
  呼出は grep 0 件 — 3 契機のうち 2 つのみ配線済みで、規範が要求する 3 番目 (batch resume/retry) が
  欠落している。

### QB45 manifest / toollock object の write タイミング契約 [P0]
- 正本: 03-data-model.md §2.1 (『manifest の**finalize** (初回確定と、partial retry で failed → done
  を反映した各確定) のたびに、canonical JCS bytes を `objects/manifests/ab/cd/<manifest64>` へ
  content-addressed で書く』) / §5.2 (『tool-lock の**materialize**時に、この canonical JCS bytes を
  `objects/toollocks/ab/cd/<hash64>` へ content-addressed で保存する』)
- 前提: (a) normalized instance の manifest が初回確定する (unit 全件が `done`/`failed` の終端状態に
  達する)。(b) tool-lock が新規 materialize される (新しい tool_profile の組合せが初めて確定する)。
- 操作: (a) manifest finalize を発生させ `objects/manifests/` を確認する。(b) tool-lock materialize
  を発生させ `objects/toollocks/` を確認する。
- 期待: (a) 対応する `manifest64` の CAS object が書き込まれる (`ContentObjectKind::Manifest`)。
  (b) 対応する `toollock64` の CAS object が書き込まれる (`ContentObjectKind::Toollock`)。**現状**:
  `ContentObjectKind::Manifest`/`Toollock` は enum として存在する (cas.rs:21-35) が、
  `write_content_object(ContentObjectKind::Manifest)` の呼出は main.rs:15681 の 1 箇所のみ確認され
  (呼出タイミングが finalize と一致するかは未検証)、`ContentObjectKind::Toollock` の書込呼出は
  grep 0 件 — toollock 側は CAS 種別が定義されているのみで実際の materialize 時書込みが未実装。

### QB46 `.kio/staging/` descriptor 構造 + 耐久 publish 手順の新設 [P0]
- 正本: 03-data-model.md §2 (『配置 = `staging/<raw64>.<tool64>.<adapter_kind>/`、各 root 直下に耐久
  descriptor.json (scope_id / raw_hash / tool_profile_hash / adapter_kind)。**root の公開 = private
  temp directory に descriptor ごと完書き → fsync → root 名へ atomic rename (no-replace) → 親
  directory fsync...payload の書込みは公開後にのみ行う** (descriptor より先に payload が存在する
  窓を作らない)』)
- 前提: 外部実行 (online Markdownize/embedding) task が streaming staging を必要とする状態。
- 操作: task 起動時の staging root 公開手順を追跡する。
- 期待: private temp に descriptor を完書き → fsync → 最終 root 名への no-replace atomic rename →
  親ディレクトリ fsync、の順で root が公開され、payload の書込みは root 公開より後にのみ行われる
  (descriptor 無しで payload だけが存在する crash 窓が生じない)。**現状**: `StagingDescriptor` 型は
  grep 0 件。既存の `delete_target_staging` (purge.rs:1361-1372、purge の削除経路) は
  「本 walk はこの directory の**契約**に対して書かれており、現在この契約を満たす producer が
  存在するかとは独立」という doc comment 付きの**読み手のみ**であり、descriptor を書き込む producer
  は皆無 — 本契約が指す耐久 publish 手順は完全に未実装。

### QB47 tag 正規化の simple case folding 化 + 未割当 code point 拒否 (パラメタ化 2 件) [P0]
- 正本: 03-data-model.md §2 (『NFC 正規化 + Unicode **simple case folding (locale 非依存 — full
  folding・locale 別規則は使わない)** が同じ名前は case-insensitive collision として同一 slot を
  占める...**実装同梱の UCD 版で未割当の code point を含む tag 名は `KIO-E-CONFIG-USAGE-001` で
  拒否する**』)
- 前提: (a) full Unicode lowercase と simple case folding が異なる結果を生む文字 (例: ドイツ語の
  `ß` — full lowercase では変化しないが、`ẞ` (大文字 ß) の lowercase 先は実装依存差が生じ得る一方、
  simple case folding は 1:1 対応が保証された固定表を使う)。(b) 実装同梱 UCD 版で未割当の code point
  を含む tag 名。
- 操作: (a) `portable_collision_key` に該当文字を含む 2 つの tag 候補名を渡し、衝突判定を比較する。
  (b) 未割当 code point を含む tag 名で `kio tag <name>` を実行する。
- 期待: (a) simple case folding の結果に基づいて衝突判定される (full lowercase 由来の判定と食い違う
  具体ケースが存在すれば、それが規範との乖離を証明する)。(b) `KIO-E-CONFIG-USAGE-001` (exit 2) で
  拒否される。**現状**: `portable_collision_key` (portable.rs:15-23) は
  `value.nfc().flat_map(char::to_lowercase).collect()` — Rust 標準の **full Unicode lowercase**
  マッピングであり、専用の simple case folding テーブルを実装しない。未割当 code point の拒否ロジックも
  grep 0 件 (`KIO-E-CONFIG-USAGE-001` が tag 名検証コンテキストで発火する箇所は未確認)。

### QB48 正規化 view 組立規則: `order` 一意性制約 + comment-safe percent-encode (パラメタ化 2 件) [P1]
- 正本: 03-data-model.md §2.1 (『manifest.units[] を `order` 昇順に走査する (`order` は unit 間で一意 —
  **重複は KIO-E-STORE-CORRUPT-001 の corruption**。値自体で順序が確定するため tie-break は存在しない)』
  / 『固定文字列 `<!-- KIO-MISSING-UNIT <unit_key> <error_kind> -->` を採用する (unit_key / error_kind は
  **comment-safe に挿入する** — `--` を含む値は percent-encode。生値の挿入は comment を途中終端し
  view の構造を壊す)』)
- 前提: (a) 同一 manifest 内に `order` が重複する 2 つの unit エントリ。(b) `unit_key` に `--` を含む
  failed unit (例: `unit_key = "page:1--injected"`、対応する実装上想定しづらいが manifest 直接編集や
  将来の Adapter 出力異常で発生し得るケースとして扱う)。
- 操作: (a) 重複 `order` を含む manifest で全文 view を再生成する。(b) `--` を含む `unit_key`/
  `error_kind` を持つ failed unit で view を再生成する。
- 期待: (a) `KIO-E-STORE-CORRUPT-001` で corruption として拒否される (view 生成を続行しない)。
  (b) 生成される `KIO-MISSING-UNIT` コメント行の `unit_key`/`error_kind` 中の `--` が percent-encode
  され、Markdown コメント構造が途中終端しない。**現状**: `markdownize.rs:1359-1362` 型の
  `KIO-MISSING-UNIT` マーカー生成コードは `unit_key`/`error_kind` を無エスケープで直接埋め込む
  (percent-encode 処理は grep 0 件)。`order` 重複検知ロジックも view 生成コード内に見当たらない
  (未検証区分)。

### QB49 scope-registry の「旧 path 到達可能なら move 非認定」判定 + device data dir 0700 + path
  validation 拒否集合拡大 (パラメタ化 3 件) [P0]
- 正本: (a) 10-operations.md §3 (『逆方向 (scope の移動) も同様に退役する: 同一 scope_id を新しい
  kio_path で観測 (再発見) したら、同一 scope_id の旧 path 行を削除する — **ただし旧 path がなお
  到達可能 (存在し有効な `.kio`) な場合は move と認定せず、削除しない**』)。(b) 10-operations.md §3
  (『device data dir (`~/.local/share/kio/`) は owner-only (0700) に制限する』)。(c) 03-data-model.md
  §3 (『`\` ・単独の `.` / `..`・NUL・control 文字を含む path、および well-formed UTF-8 でない byte
  列の path も拒否する』)
- 前提: (a) 同一 scope_id が旧 `kio_path` (到達可能・有効な `.kio` が現存) と新 `kio_path` (新規
  観測) の両方で registry に存在する状態 (真の move ではなく、`.kio` をコピーして別 path に複製した
  ケース = clone 併存)。(b) `~/.local/share/kio/` 配下のディレクトリ (`logs/` を含む) のパーミッション。
  (c) tree entry の `path` にバックスラッシュ (`\`) または制御文字 (0x01 等) を含む新規 ingest。
- 操作: (a) registry upsert (新 `kio_path` の観測) を実行し、旧 `kio_path` 行が削除されるか確認する。
  (b) `~/.local/share/kio/logs/` のパーミッションを確認する。(c) 新規 ingest 時に該当 path を持つ
  tree entry を作成しようとする。
- 期待: (a) 旧 `kio_path` が到達可能である限り、旧行は削除**されず** (真の move ではなく clone 併存と
  して扱われ、live 重複の fail-closed 規則 (`KIO-E-REGISTRY-DUP-001`) の対象になる)。(b) `logs/` を
  含む device data dir 配下が owner-only (0700 相当、他ユーザーから読めない) である。(c) `\`/制御
  文字を含む path は `KIO-E-STORE-PATH-001` で拒否される。**現状**: (a) `retire_stale_kio_path`
  (registry.rs:103-109) は同一 `kio_path` に別 `scope_id` がある場合の削除のみを扱い、
  「同一 scope_id・新 kio_path 観測時に旧 path の到達可能性を検査する」ロジック自体が存在しない —
  到達可能性チェックを行うのは `registry_prune` (`--registry-prune` 明示実行時のみ) であり、通常の
  upsert 経路には組み込まれていない。(b) `xdg.rs` 自体に権限設定コードは無いが、`registry.rs:55-64`
  と `ledger/schema.rs:146-152` がそれぞれ独立に対象ファイルの親ディレクトリ (実質
  `~/.local/share/kio/`) を 0700 化しており、機能的には既に充足している可能性が高い (2 箇所の
  重複実装という設計上の指摘は残る)。(c) `is_logical_direct_child` (dag.rs:116-121) は `/` と NUL の
  みを拒否し、`\`・その他の制御文字は許容している — 拡大が必要。

---

# §D. `kio log --at/--since` 本実装

**精読対象の限界について明示する**: `kio log` 自体の挙動を規定する規範文は、リポジトリ全体で
06-cli-spec.md §1 L61 の 1 行 (『kio log [--at <commit>] [--since <dur>]』) のみである。
05-runtime.md §2.2 L534 が触れるのは「shallow commit のメタ情報表示は `kio log`/`kio inspect` が担う」
という**責務分担**の言及だけで、`--at`/`--since` 自体の意味論には立ち入らない。本節の契約群は
やむを得ず、(a) 同名フラグが search/view/reindex で持つ確立済みの意味論 (『時点指定』としての
`--at <commit>`、search の『期間指定』としての `--since <duration>`) からの**類推**、(b) 一般的な
exit code/error code の横断規約 (§7/§8, §12) への当てはめ、の 2 方法でのみ期待値を構成する。
類推・当てはめに依存する箇所は個々の契約内で `[解釈割れ]` として明示し、実装時の裁定を要求する
(spec が log 自身について明言していない以上、本書はこれらを「確定した規範」ではなく「先行固定すべき
候補」として提示する)。

### QB50 `--at <commit>`: history walk の起点を HEAD から指定 commit へ変更 [P0] [解釈割れ]
- 正本: 06-cli-spec.md §1 L61 (『kio log [--at <commit>] [--since <dur>]』のみ — 挙動は無規定)。
  類推元: 06-cli-spec.md §3 L226 (search の『--at は --scope 単一指定を必須とする...05 §1.6』) /
  05-runtime.md §1.6 L214 (『--at <commit> 指定 commit 時点で indexed だった chunks のみ対象』) —
  「`--at <commit>` = 現在 (HEAD) ではなく指定 commit を基点として扱う」という Kio 全体で一貫した
  フラグ意味論からの類推。
- 前提: 3 世代の commit 履歴 `C1 → C2 → C3(HEAD)` を持つ scope。
- 操作: `kio log --at C2` を実行する。
- 期待: `[解釈割れ]` 返る `entries` は `C2` を起点とした history (すなわち `C2, C1`) であり、`C3`
  (HEAD、`C2` の子孫) を含まない — `--at` 指定 commit そのものと、その祖先のみを返す。**現状**:
  `Command::Log` (main.rs:623-640) は `args.at.is_some()` の場合に無条件で
  `KioError::not_implemented("log --at/--since")` を返す (未実装)。`repo.log()`
  (scope.rs:1310-1337) は `head_commit_hash()` のみを起点にする固定実装であり、任意 commit を起点に
  する引数を受け付けない。

### QB51 `--at <commit>` で shallow commit を指定した場合の拒否 [P0] [解釈割れ]
- 正本: 05-runtime.md §2.2 L534-545 (shallow commit の一般規則列挙: 『kio restore <shallow-commit> は
  KIO-E-COMMIT-SHALLOW-001 で拒否』『kio search --at <shallow-commit>...も KIO-E-COMMIT-SHALLOW-001 で
  失敗する (tree 全体を要するため)』) — この列挙に `kio log --at` は明示されていないが、`kio log --at`
  も対象 commit 以前の履歴を辿るには tree ではなく commit object の parent chain のみを要するため、
  厳密には tree を要求しない可能性がある ([解釈割れ] の核心)。
- 前提: shallow 化された commit `Cs` (tree 破棄済み、commit object は現存) を含む履歴。
- 操作: `kio log --at Cs` を実行する。
- 期待: `[解釈割れ]` 2 つの候補: (a) shallow commit の tree 欠落は log の「commit 履歴列挙」という
  性質上 **無関係** であり (`kio log` は tree ではなく commit object の `parents` chain のみを辿る、
  scope.rs:1310-1337 の現行実装と整合)、正常に応答してよい。(b) 05-runtime.md の shallow 規則列挙が
  「`--at` を受け付ける全コマンドで一律 KIO-E-COMMIT-SHALLOW-001」という原則の具体例に過ぎないなら、
  `kio log --at` も拒否すべき。本書は (a) を推奨解釈とする (tree を読まない操作に shallow 制約を
  課す理由が無いため) が、実装時の明示裁定を要求する。

### QB52 `--since <duration>`: `commit.created_at >= now - <duration>` によるフィルタ [P0] [解釈割れ]
- 正本: 06-cli-spec.md §1 L61 のみ (挙動無規定)。類推元: 05-runtime.md §1.6 L221/L234 (search の
  『--since <duration> `--since 7d` のように期間指定』『--all-history 集合を
  chunks.created_at >= now - <duration> で絞る』) — search の `--since` が commit ではなく
  chunk の `created_at` を絞る一方、`kio log` は commit を列挙する操作であるため、絞り対象は
  自然に `commit.created_at` になるという類推。
- 前提: 3 commit `C1(created_at=T-10d) → C2(T-3d) → C3(T, HEAD)` を持つ scope。
- 操作: `kio log --since 7d` を実行する (現在時刻 = T)。
- 期待: `[解釈割れ]` `entries` は `created_at >= T-7d` を満たす `C3, C2` のみを含み `C1` を除外する
  (HEAD からの history walk は継続するが、結果配列を `created_at` でフィルタする)。デフォルトの
  history walk 起点 (HEAD、`--at` 未指定時) は変更しない。**現状**: 未実装 (`not_implemented`)。

### QB53 `--at`/`--since` 未指定時の既存挙動への非破壊性 (回帰確認) [P1]
- 正本: 06-cli-spec.md §1 L61 (両オプションとも `[...]` で optional と明記 — 省略時は現行の HEAD 起点
  history walk のままであるべき、という構文上の自明な要件)。
- 前提: `--at`/`--since` の実装後も、フラグ無しの `kio log` 呼出が既存挙動を維持する必要がある。
- 操作: `--at`/`--since` いずれも指定せず `kio log` を実行する。
- 期待: 現行の `repo.log()` (HEAD 起点、first-parent-only 相当の `commit.parents.first()` walk、
  祖先 commit object 欠落時は `truncated: true` を伴う healthy prefix を返す R16-1 の挙動) が
  そのまま維持される。**現状**: 現行実装そのもの (回帰ロック)。

### QB54 `--at <commit>` の commit 解決規則 (HEAD / tag / full hash) [P1] [解釈割れ]
- 正本: 06-cli-spec.md §5 (restore の commit 引数規則、類推元): 『commit は HEAD / tag / full commit
  hash』。`kio log --at` の commit 引数が同じ受理形式を共有するかは spec が明言しない。
- 前提: (a) `HEAD` という文字列。(b) 既存 tag 名。(c) full commit hash (64 hex)。(d) 短縮 hash
  (12 hex 等)。
- 操作: `kio log --at <各形式>` を実行する。
- 期待: `[解釈割れ]` (a)(b)(c) は restore/diff と同じ `resolve_commit` 相当の解決規則を再利用して
  受理される可能性が高い (diff コマンドが既に `resolve_commit(a)`/`resolve_commit(b)` で HEAD/tag/
  hash を解決している — scope.rs:1339-1341 型)。(d) 短縮 hash を受理するかは restore の「raw_hash
  shorthand は restore では受理しない」という限定 (06§5 L289) との整合が不明瞭 — commit hash の
  短縮形と raw_hash shorthand は別概念のため、restore の限定が log の commit 引数にまで及ぶかは
  裁定を要する。

### QB55 `--at` + `--since` 併用時の挙動 [P0] [解釈割れ]
- 正本: 06-cli-spec.md §1 L61 (両者は同一コマンドラインで `[--at <commit>] [--since <dur>]` と
  並記されており、構文上は併用可能に見える — search の `--at`/`--since` も相互排他とは明記されない
  05-runtime.md §1.6)。
- 前提: `--at C2 --since 5d` の同時指定。
- 操作: `kio log --at C2 --since 5d` を実行する。
- 期待: `[解釈割れ]` 2 候補: (a) `--at C2` で history walk の起点を `C2` に変更したうえで、
  その結果 (`C2` とその祖先) をさらに `created_at >= now-5d` で絞る (積集合)。(b) 相互排他として
  `KIO-E-CONFIG-USAGE-001` (exit 2) で拒否する (search の `--at` が『`--scope` 単一指定を必須とする』
  ような特殊な相互作用規則を持つのと同様、log でも何らかの制約があり得る)。本書は spec の並記構文
  ([...]  [...] の独立した optional 表記) を根拠に (a) を推奨解釈とするが確定はしない。

### QB56 `--since <dur>` の duration 構文 [P1] [解釈割れ]
- 正本: 06-cli-spec.md §1 L61 (`<dur>`という表記のみ)。類推元: 05-runtime.md §1.6 L221
  (『--since <duration> `--since 7d` のように期間指定』— search の例示は `"7d"` のみ)。
- 前提: `"7d"` (日数)・`"24h"` (時間、search 側で明示例は無いが `"7d"` 形式からの単位拡張の可能性)・
  不正な文字列 `"abc"`。
- 操作: `kio log --since <各値>` を実行する。
- 期待: `[解釈割れ]` `"7d"` は受理される (search と同一の構文パーサを再利用すると仮定)。`"24h"` 等の
  他単位が受理されるかは search 側の実装 (`search_time::TimeSelector` 型) の対応単位に従う
  ([解釈割れ] — 本書は search 用のパーサをそのまま流用することを推奨する)。不正な文字列は
  `KIO-E-CONFIG-USAGE-001` (exit 2) で拒否される。

### QB57 `--json` 出力形式の一貫性 (`entries`/`truncated` の維持) [P1]
- 正本: 06-cli-spec.md §4 (『すべての CLI は `--json` を持つ...`--json` で機械可読』) — 既存の
  `LogReport { entries: Vec<LogEntry>, truncated: bool }` (scope.rs:174-178) という shape 自体は
  spec 上明記されないが、`--at`/`--since` 実装がこの既存 shape を破壊しないことを回帰確認する。
- 前提: `--at`/`--since` いずれかを指定した `kio log --json` 呼出。
- 操作: 出力 JSON の top-level キーを検査する。
- 期待: `entries` (配列) と `truncated` (boolean) の 2 キーが引き続き存在する (`--at`/`--since` は
  対象範囲を絞るだけで、レスポンスの shape 自体は変えない)。**現状**: 該当分岐が `not_implemented`
  のため未検証。

### QB58 `--at <commit>` の解決不能 commit 指定に対するエラー分類 [P1]
- 正本: 06-cli-spec.md §8 (『`KIO-E-CONFIG-USAGE-001` (invalid usage / 不正オペランド — 例: ...不正
  hash 引数)』) — 存在しない commit hash や tag 名を指定した場合の一般的な不正オペランド分類からの
  適用。
- 前提: 存在しない commit hash (64 hex だが未知の値) を `--at` に指定する。
- 操作: `kio log --at sha256:<未知の64hex>` を実行する。
- 期待: `KIO-E-CONFIG-USAGE-001` (exit 2) で拒否される (`KIO-E-STORE-NOT-FOUND-001` のような内部
  store エラーをそのまま外部に漏らさない — diff の commit 解決失敗時の扱いと同型であるべき、
  という一般規約からの適用)。

---

# §E. P2 繰越 (`step4b-contract-tests-p2c.md` PC20 / PC25 / PC26 / PC33 / PC44 の未決配線)

PC20/PC25/PC26/PC33/PC44 は `step4b-contract-tests-p2c.md` (Phase 2-C) で既に「正本引用・前提・操作・
期待」を備えた契約として存在する — 本節はそれらを**再契約しない**。以下は、それらの契約文自身が
現状分析 (「現状」欄) で認めている**未決の実装配線・分解不足**にのみ、追加の契約を与える。

### QB59 PC20: embedding enrichment finalize における index_generation 回転点の特定 [P0]
- 正本: PC20 (p2c.md) が引用する 05-runtime.md §1.5 (『rebuild...purge...**embedding enrichment の
  finalize**...index / batch finalize で chunk_fts の内容が変化した場合...のいずれでも新規採番する
  ULID』『回転はそれを引き起こした SQLite 書込...と同一の SQLite Tx で行う』)
- 前提: PC20 は (a)-(f) の 6 契機を単一の 期待 で一括検証するのみで、embedding enrichment finalize
  ((c)) が cost-ledger の claim/settle 2 相プロトコル (`step4b-contract-tests-ledger.md` の CL13-21
  系) を経る**多段 Tx**の中の**どの Tx**で回転すべきかを特定していない。
- 操作: embedding タスクが device-row claim (相 1) → adapter 呼出 → 成果 finalize (相 2/3 相当) の
  多段プロセスを経て完了する状態で、各段階終了直後の `index_metadata.index_generation` を観測する。
- 期待: `index_generation` が変化するのは、embedding 成果が SQLite の `embeddings`/`chunk_vec` へ
  実際に反映される最終 Tx (成果 finalize) の**時点のみ**であり、claim/settle の中間 Tx (課金記帳のみ
  行う cost-ledger 側の Tx) では回転しない。この特定により「finalize Tx」の境界が一意に定まる。

### QB60 PC20: index/batch finalize (chunk_fts 内容変化を伴う再インデックス) の回転点特定 [P0]
- 正本: PC20 と同一引用 (『index / batch finalize で chunk_fts の内容が変化した場合』)
- 前提: `kio index` が複数ファイルの Markdownize → chunk 化 → auto snapshot finalize という複数段階を
  経て完了する状態。PC20 自身の「現状」記述は (a)(b)(c)(f) の 4 契機の不在のみを述べ (d) (index/batch
  finalize) を明示的に current-state 記述から欠落させている — この非対称自体が本項目の未確定を
  示唆する。
- 操作: `kio index` の実行中、auto snapshot finalize (05-runtime.md §8.1 の 3 段階耐久順序: (1)
  chunks.jsonl append+fsync → (2) SQLite 反映 → (3) commit/ref publish) の各段階終了直後に
  `index_generation` を観測する。
- 期待: `index_generation` が変化するのは段階 (2) (SQLite 反映) 完了時点であり、段階 (1)/(3) では
  変化しない (chunk_fts の内容変化そのものが SQLite 反映段階で確定するため)。

### QB61 PC20: `index_generation` と `last_lifecycle_epoch` の併存関係の実装確認 [P1]
- 正本: p2c.md §Q note-2 への §R 裁定 (『**併存が正 (統合しない)** — index_generation (6 契機の
  cursor 無効化版) と last_lifecycle_epoch (tombstone lifecycle 単調性の crash-safe 検証・§3.5 の
  独立補完規則付き) は目的が異なる。tombstone lifecycle 更新時は両方が動く (lifecycle-epoch +1 と
  generation 回転)』)
- 前提: tombstone の retire (lifecycle event append) が発生する状態。
- 操作: retire を発生させ、`index_metadata.last_lifecycle_epoch` と `index_metadata.index_generation`
  の両方を観測する。
- 期待: 両方とも変化する (`last_lifecycle_epoch` は現行の tombstone lifecycle 検出専用カウンタとして
  引き続き機能し、`index_generation` は PC20 の 6 契機の 1 つとして独立に回転する — どちらか一方への
  統合・置換は行わない)。実装時にこの併存を壊さないことを固定する回帰契約。

### QB62 PC25: multi-scope search における scope 別独立 query_cache 行の再利用 [P0]
- 正本: PC25 (p2c.md) が引用する 05-runtime.md §1.5 (『page 1 の正規化済み query vector は参加各
  scope の embeddings 表...に保持し』— 「各 scope の」という複数形表現)
- 前提: 2 scope (S1, S2) 横断の vector|hybrid page 1 検索を実行済み (両 scope の sqlite.db にそれぞれ
  独立した `query_cache` 行が書き込まれている想定、QB33 の実装後を前提とする)。
- 操作: `--cursor <token>` で page 2 を replay する。embedding adapter をモックに差し替える。
- 期待: S1・S2 それぞれの `query_cache` 行が独立に読み出され (S1 用の digest と S2 用の digest は
  各 scope の embedding profile が異なれば異なり得る)、adapter は一度も呼ばれない。一方の scope の
  `query_cache` 行が欠落 (削除・剪定済み) している場合は、その scope のみ `KIO-E-SEARCH-CURSOR-001`
  相当で除外され、他方の scope は正常に replay を継続する ([解釈割れ]: multi-scope 全体を
  cursor エラーにするか、当該 scope のみ除外するかは PC25/26 の単一 scope 前提の記述からは
  確定しない — 09 §1.8 の multi-scope 部分失敗規則 (05-runtime.md §1.8) との整合を裁定に委ねる)。

### QB63 PC25/26: query_cache 行の 256 行剪定との競合タイミング [P1]
- 正本: PC26 (p2c.md) が引用する 05-runtime.md §1.5 (『不一致は corruption として当該行を削除し...
  KIO-E-SEARCH-CURSOR-001』) / 04-pipeline.md §4.3 (256 行剪定規則、QB33 参照)
- 前提: ある scope の `query_cache` 行数が既に 256 件に達している状態で、別の新規 vector|hybrid
  検索の page 1 が実行され、剪定によって最小 rowid の行 (= 直前の page 1 が書き込んだ行だった場合) が
  削除される競合。
- 操作: (a) page 1 実行 → query_cache 行 A が最小 rowid として書き込まれる。(b) 別クエリの page 1 が
  256 件超過の剪定を発火し行 A を削除する。(c) 元の cursor で page 2 を replay する。
- 期待: (c) は行 A が存在しないため `KIO-E-SEARCH-CURSOR-001` で拒否される (再検索に誘導する
  メッセージを含む) — 256 行という上限が「同時に有効な cursor 数の上限」を暗に意味することを
  明示する回帰的な境界確認。

### QB64 PC33: per-binding config 解決の `fts_scope_search`/`vector_scope_search` への配線 [P0]
- 正本: PC33 (p2c.md) が引用する 05-runtime.md §1.6 L238 (『--all-history / --include-deleted は各
  binding tree の値で判定する』) — PC33 自身の「現状」記述: 『`fts_scope_search`/`vector_scope_search`
  は呼出全体で単一の `chunking_config_hash: &str` パラメータしか受け取らない構造...binding 単位での
  config 値切替えという概念が存在しない』
- 前提: `search_history.rs` の `SearchHistoryBinding` (raw_hash/tool_profile_hash/gen/path_at_commit/
  pointer_commit を保持済み) を、各 binding の `pointer_commit` から解決した `chunking_config_hash`
  も保持するよう拡張した状態を仮定する。
- 操作: `--all-history` 検索で、config 値が異なる 2 binding (`pointer_commit` が異なる 2 commit に
  由来) を含む結果セットに対し `fts_scope_search`/`vector_scope_search` 相当の filter を適用する。
- 期待: 各 binding が自身の `pointer_commit` に対応する `chunking_config_hash` でフィルタされ、単一のグローバル
  `chunking_config_hash` パラメータでは実現できない per-binding 判定が成立する。**現状**:
  QB29/QB30 と同根で、`chunk_config_generations` の pair UNIQUE・`chunk_publications` 経由の
  時点条件判定 (PC37-43、既に p2c.md で契約済み) が前提として先に必要 — 本契約はそれらが揃った上での
  **配線側**の end-to-end 確認として位置づける。

### QB65 PC44: `--include-deleted` 補完 binding 個別 ancestor-or-equal 判定の実装配線 [P0]
- 正本: PC44 (p2c.md) が引用する 05-runtime.md §1.6 L266 (『--include-deleted の補完 binding にも
  同条件を適用する (introduction が当該 binding commit の ancestor-or-equal であること）』) — PC44
  自身の「現状」記述: 『PC37-38 と同根で未実装』
  (chunk_publications 表自体が存在しないため補完 binding 側の判定ロジックも存在しない)
- 前提: `--include-deleted` の補完対象 (削除済みファイルの最終版、`pointer_commit=Cdel`) が指す
  chunk の `introduction_commit` が `Cdel` の子孫であるシナリオ (chunk_publications 表が QB29/PC37 の
  実装後に存在する前提)。
- 操作: `kio search "<query>" --include-deleted` を実行する。
- 期待: 当該 chunk は補完結果に含まれない (introduction が binding commit の祖先でも自分自身でもない
  ため)。PC33 (通常の `--all-history` binding) と PC44 (`--include-deleted` の補完 binding) が
  **同一の correlated EXISTS 判定関数**を共有することも確認する (05-runtime.md §1.6 の実装規範
  『時点条件の判定は correlated EXISTS...で評価する』が両モードで同一である以上、判定ロジックの
  重複実装ではなく共有実装であるべき)。

### QB66 PC33/PC44: 3 binding 以上混在時の独立性確認 [P1]
- 正本: PC33/PC44 の 2 引用と同一 (05-runtime.md §1.6 L238/L266)。2 binding のみの検証では
  「グローバル値を binding A の値に固定しただけ」という誤実装を排除できないため、3 値以上での
  独立性を追加確認する。
- 前提: 3 binding (config 値 `Hx`/`Hy`/`Hz`、すべて相異なる) が同一 `--all-history` 検索結果に
  含まれる状態。
- 操作: `kio search "<query>" --all-history --text` を実行する。
- 期待: 3 binding それぞれが自身の config 値で独立にフィルタされ、いずれか 1 つの値へ暗黙に
  収斂しない (例えば "最初に評価された binding の値が残り 2 件にも適用される" ような実装バグを
  この 3 値ケースが検出する)。

---

# §Z. 解釈が割れうる点 (勝手に決めず引用付きで列挙)

### Z1. preflight (0)-(4) の実装単位 (QB5/QB6/QB7 関連)
10-operations.md §3 L300-311 は「全コマンド共通の preflight 順序」を規定するが、これが**単一の
共有実装 (1 関数)** を要求するのか、**各コマンドが独立に同じ順序を再現すればよい**のかは spec の
記述からは判断できない。現状の実装は log/diff/inspect 系と open/view/search/evidence-verify 系で
異なる順序を持ち (QB6)、restore はさらに別のコード経路を持つ (QB5) — これらは偶然の実装差であり、
共有関数化を怠った設計判断の帰結である可能性が高いが、spec 自体は実装戦略に立ち入らない。本書は
QB5-QB7 で「結果として観測される順序」のみを契約化し、実装が単一関数への統合を選ぶか各コマンド
個別修正を選ぶかは実装判断に委ねる。

### Z2. `config.toml` の `kio_format_version` 重複コピーが同一規範の対象か (QB8 関連)
03-data-model.md §2 L154 が明示するのは `.kio/scope.json` の `kio_format_version` フィールドのみ
(『保存場所 = `.kio/scope.json` の `kio_format_version` フィールド』)。しかし現行実装は
`Repository::init` (scope.rs:243-257) が `config.toml` にも同一内容の `kio_format_version` を書き込み、
`validate_config` (scope.rs:1695-1714) が scope.json と同型の検証順序バグを独立に持つ。spec が
config.toml 側のこのフィールドの規範的地位に触れていない以上、(a) config.toml 側は単なる vestigial
な複製であり修正対象外、(b) 実質的に同じ `kio_format_version` 概念である以上 config.toml 側も
同じ順序規範の対象、のどちらであるべきかは裁定を要する。QB8 は scope.json のみを対象とし、本項目は
config.toml 側の扱いを保留する。

### Z3. QB12 の境界
QB12 は purge の公開競合・non-mutation behavior だけを扱う。cost-ledger の相対 lock 順序について、
この Phase の runtime evidence からは結論を出さない。

<!-- historical analysis retained below; it is not a current contract claim.
05-runtime.md §6 L1058 の『複合 lock 順序は scope store → cost-ledger.sqlite (Tx) → device
observability → scope access とし、逆順取得を禁止する』という文言は、cost-ledger.sqlite が
`.kio/.lock` (`StoreLock`) 系とは別の `BEGIN IMMEDIATE` Tx による独自シリアライズ機構を持つという
実装事実 (scope.rs:3345-3351 のコメントが明示) と表面的に緊張する。この文言は (a) 「scope store lock
を保持したまま cost-ledger の Tx を開始してはならない (Tx 取得中に scope store lock を待たせない)」
という具体的な待機順序制約、(b) 「両者は独立した直列化機構であり、列挙順は概念上の相対的な粗さの
表現に過ぎず具体的な待機順序を強制しない」という緩い解釈、のどちらを意図するか確定しない。QB12 は
`StoreLock` 系の実装順序 (scope-store→publication→device-scrub→scope-access) のみを現状固定し、
cost-ledger との相対順序は本項目として保留する。 -->

### Z4. `eval/` の Recall@10 射影実装確認の要否 (QB24 関連)
U142 の Recall@10 射影変更 (`(raw_hash, section)` → `(raw_hash, section, path_at_commit)`) は
09-mvp-scope.md の評価規約であり、その実行コードは (存在すれば) `eval/` 配下にある。本書の
「精読する spec」「現行実装」の対象範囲はいずれも `docs/` の特定節と `crates/` であり、`eval/` は
指示書に明記された対象ファイルに含まれない。QB24 は `search_history.rs` 側の基盤 (`path_at_commit`
field) の存在確認までに限定し、`eval/` 配下の実際の計算式が新射影を採用しているかどうかの検証は
本書の対象範囲内か外かを含めて未確定のまま残す。

**Z4-決着 (2026-07-22 Phase 4 回帰補修)**: 懸念は的中した — 現行Rust `kio-eval`へ移植済みの Recall@10 射影は
QB24 の 3 要素へ更新済みだったが、同ファイル内 `assess_history_coverage` の rename 網羅ガード
(`{old_file, new_file} ⊆ correctly_recalled の paths`) に非伝播だった。golden は旧名しか記さないため
新 path の alias 行は expected_set 経由で correctly recalled になり得ず、ガードは**構造的に充足不能**
(Recall 32/32 = 1.0 でも `passes_m3_2 = false` に固定)。補修 = 新 path 側を「旧 identity の
`new_file` 双子」として当該 raw を expected に持つ query 自身の top-10 から直接クレジット
(無関係 query のノイズでは満たせない原則・Recall 指標本体・edited/deleted ガードは不変)。
「fix が開けた穴」36 例目 (射影 fix → 下流ガード非伝播)。

### Z5. `kio log --at/--since` の意味論全体 (QB50/51/52/54/55/56 関連)
06-cli-spec.md における `kio log` の規範は §1 L61 の 1 行 (フラグの存在のみ) に尽きる。§D の
6 契約 (QB50: `--at` の history walk 起点変更、QB51: shallow commit 指定時の拒否要否、QB52:
`--since` のフィルタ対象、QB54: commit 引数の受理形式、QB55: `--at`+`--since` 併用時の意味、QB56:
`--since` の duration 構文) はいずれも search/view/restore の確立済みフラグ意味論からの**類推**で
期待値を構成しており、spec 自身がこれらを明言しているわけではない。実装着手前に、これら 6 点を
一括して spec 側 (06-cli-spec.md への追記) で確定させることを強く推奨する — 本書はあくまで
「先行固定すべき候補」を提示するに留まる。

### Z6. multi-scope search で一部 scope のみ query_cache 行が欠落した場合の扱い (QB62 関連)
PC25/PC26 (p2c.md) の正本引用はいずれも単一 scope の replay を前提に書かれており、multi-scope
検索で複数 scope がそれぞれ独立の `query_cache` 行を持つ状況で、**一部の scope のみ**行が欠落
(剪定・破損等) した場合に、(a) 05-runtime.md §1.8 の multi-scope 部分失敗規則に従い当該 scope のみ
`excluded_scopes` へ計上して他 scope の replay は継続する、(b) cursor 全体を `KIO-E-SEARCH-CURSOR-001`
で無効化し全 scope の再検索を要求する、のどちらであるべきかは明記が無い。QB62 は (a) を推奨解釈と
して記述するが確定はしない。

# §裁定 (§Z の解釈割れ — 実装用、2026-07-22 オーケストレータ裁定)

1. **Z1 (preflight 実装単位)**: **共有関数へ統合する** — 契約は観測順序のみだが、3 経路の独立実装は非伝播バグの温床 (本プロジェクトで実証済みのパターン) のため実装戦略として統合を指示。
2. **Z2 (config.toml の kio_format_version)**: **config.toml 側の書込・検証を廃止し scope.json へ一本化** — spec の保存場所規範 (03 §2) どおり。再 init 方針で互換負債なし。
3. **Z3 (複合 lock 順序)**: **(a) 待機順序制約** — cost-ledger Tx 保持中に scope store lock 系を取得することを禁止 (逆順禁止の字義)。順方向 (store lock 保持中の Tx 開始) は可。契約は逆順取得の不在確認。
4. **Z4 (eval/ の Recall 射影)**: **対象範囲内** — U142 の射影 ((raw_hash, section, path_at_commit)) はRust `kio-eval`とfrozen wire testsで固定する。
5. **Z5 (log --at/--since)**: **QB50〜QB56 の類推期待値を採用して実装** — search/view/restore の確立済み意味論からの類推は本プロジェクトの正当な導出。spec 側への明文追記は Phase 4 の実装フィードバック記録へ (凍結例外ではなく提案として)。
6. **Z6 (multi-scope の部分 query_cache 欠落)**: **(a) を確定** — 当該 scope のみ excluded_scopes へ計上し他 scope の replay は継続 (05 §1.8 の部分失敗規則と整合)。

---

# §集計

| 節 | 対象 U 項目 | 契約数 (P0/P1/P2) |
| --- | --- | --- |
| §A K 領域 (error code / exit code 横断) | U121-U128 | QB1-QB9 (9 件: P0 6 / P1 2 / P2 1) |
| §B L 領域 (lock/registry/scan境界/import-export/observability/文言) | U130-U142 (U129 除外) | QB10-QB24 (15 件: P0 1 / P1 10 / P2 4) |
| §C J 領域 (耐久書込・schema・DDL・tree・CAS・path) | U95,96,98-112,114-119 | QB25-QB49 (24 件: P0 12 / P1 11 / P2 1) |
| §D `kio log --at/--since` 本実装 | (06§1 L61 のみ正本) | QB50-QB58 (9 件: P0 4 / P1 5) |
| §E P2 繰越 (PC20/25/26/33/44 の未決配線) | H 領域 (継続) | QB59-QB66 (8 件: P0 5 / P1 3) |
| **合計** | | **66 件 (P0 29 / P1 31 / P2 6)** |

解釈割れ: **6 件** (§Z Z1-Z6)。

対象外として明示的に除外した項目: U129 (Phase 4+ GC、対象外)、U97/U113/U120 (Phase 1 済み)、
B-H 領域全体 (Phase 1/2 契約済み)、A/I 領域 (別 Phase 3 グループ)、`KIO-E-ADAPTER-APPROVAL-CONFLICT-001`/
`KIO-E-ADAPTER-SPECVER-001` の具体的発火条件 (I 領域の管轄、07-adapter-spec.md 精読を要するため本書の
対象外)。
