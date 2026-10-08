# 09 MVP Scope

統合元: 旧 `north-star-scenarios.md` + 旧 `design-homework.md` + 旧 `consolidation-plan.md` の Phase plan + `01-positioning.md` から MVP/Phase 部分の抜粋 (research 検討メモは 2026-07-18 に撤去 — git 履歴で参照可)。

> 本書は MVP の範囲、実装 Step、コード規模と構造の上限、北極星シナリオと評価規約、決定済みの設計論点を定める。

---

# 1. MVP に含める / 捨てる

> 本書の Phase、Step、RC matrix と内部評価基準は historical/current implementation contract である。
> v1 の製品要件、ロードマップの製品境界、未充足状態の正本は
> [11-product-requirements.md](11-product-requirements.md)。相違する場合は同書を優先し、
> 本書の古い実装計画から v1 の機能完了を推論してはならない。

## 1.1 MVP に含める (Phase 1〜3)

```
- content-addressed raw object 保存
- Normalized Markdown (incremental Markdownize 含む)
- chunk
- Embedding
- FTS (FTS5 外部 content + trigram tokenizer)
- Hybrid search (paging / MMR / cursor)
- Evidence Pointer
- snapshot DAG (commit / tree)
- kio index 完了時の auto snapshot (定期 auto snapshot / watch の Phase 記述は historical RC plan であり、v1 の変更検出要件は [11-product-requirements.md](11-product-requirements.md) を参照)
- export (--to 必須)
- time-travel search (--at / --all-history / --include-deleted)
- ベースライン index (deterministic 抽出 + FTS。API キーなしで init→index→search→open が成立 — [01-positioning.md §3](01-positioning.md))
- 初回スキャン preview + 明示承認
- budget guardrail (cost ceiling / kill switch)
- purge 最小形 (tombstone + commit_type=purged + 検索除外 + ログスクラブ。M3-3 の完了条件)
- kio evidence verify <pointer> / kio evidence verify --batch <pointers.jsonl> (Phase 4 milestone 6 の implemented target/current)
```

## 1.2 v1 automatic-child implementation boundary

本表が v1 の実装済み child-scope 経路の正本である。各 OS の native Actions は同じ候補へ結び付けた
受入証跡をまだ必要とするため、本表は acceptance pass を表明しない。

| platform | ユーザーが直接選択した scope の `init` / `index` | 管理済み親からの child scope 自動 mutation | acceptance |
| --- | --- | --- | --- |
| macOS | 実装済み | 実装済み。空 directory を含む retained-handle child discovery | native Actions receipt 未完了 |
| Linux | 実装済み | 実装済み。空 directory を含む retained-handle child discovery | native Actions receipt 未完了 |
| Windows | 実装済み | 実装済み。空 directory を含む retained-handle child discovery | native Actions receipt 未完了 |

全 OS で明示 `init` は利用者が選択した root に限る。自動経路は retained parent handle から直接 child を
検証し、public pathname の再解決や `current_dir(path)` 型 handoff を使わない。独立 root、policy 除外、
identity 不一致、symlink / junction / reparse point は採用しない。child の管理開始は device-private egress
grant、trust、ledger authority を作成・継承しない。

# 2. 将来ロードマップ（historical。製品境界は 11 を参照）

直前の RC matrix は RC.3 時点の制約であり、v1 の実装境界や acceptance 結果を表明しない。特に Windows の
自動 child mutation 未対応という RC の制約は現行実装には適用しない。

以下は実装を許可せず、CLI syntax、schema、error code、default、互換性を定めない名称だけの記録である。

- export / import
- bare mutating prune
- non-tree CAS reclamation
- CoW GC
- move tracking
- external Agent API / MCP
- Knowledge Graph / Agent navigation
- multi-device sync / cloud sharing
- Adapter sandboxing / distribution signing
- pack / delta compression
- full-history purge rewriting
- GUI
- cross-scope permissions
- large-scale search backend
- semantic diff / perceptual reuse
- Summary / Classification adapters

`tasks/` は過去の受入記録であり、現在の契約や将来実装を承認する文書ではない。

v2 は GUI で CLI の全機能を提供し、履歴の選択と最新版への復元を含む。v3 は cloud sharing、
collaboration、複数利用者を扱う。revert の transaction、CLI、線形履歴 schema は未決の提案であり、
この文書は実装詳細を課さない。v1 で user/group ACL は要求しない。外部送信への consent は
multiuser ACL ではない。

---

# 3. 実装 Step とコード規模・構造の上限

```text
Step 1: kio-core + kio-cli で init / status / snapshot / log / diff / inspect / tag
Step 2: kio-pipeline + kio-adapter (ベースライン抽出、opt-in の AI 強化、batch / budget)
Step 3: kio-index + kio-search (hybrid + Evidence Pointer)
Step 4: export + --at + time-travel + purge 最小形 (tombstone) + evidence verify (単発)
```

Step は Phase 1〜3 の実装順序の記録である。現在の v1 工程は
[v1完成計画](../tasks/v1-completion-plan-2026-09-08.md) と
[受入完了計画](../tasks/v1-acceptance-closeout-plan-2026-10-03.md) を参照する。

2026-10-05 に旧上限 (テスト除き 11,000〜16,000 行、ripgrep 以下) を廃止した。v1 は 3 OS の
handle-bound 保存、送信承認、課金台帳、回復、監視を含み、旧上限の前提を外れた。上限は実測した
現状規模を基準に置き、OS 共通ロジックと OS 固有層を分けて管理する。

**前提: OS 共通ロジックと OS 固有層の分離**

- OS 固有の処理は各製品 crate の `src/platform/` に置く。`platform/mod.rs` は OS 非依存の型と関数
  signature だけを公開する。OS ごとの実装 (`platform/` 直下の `windows`、`unix`、`linux`、`macos` の
  `.rs` または directory) の選択はここだけで、`mod` と `use` の宣言に OS 条件を付けて行う。
  `platform/` にはこれ以外の file を置かない。
- 共通ロジックのファイルは OS 条件を持たない。OS 条件とは、`#[cfg(...)]`、`#[cfg_attr(...)]`、`cfg!(...)` の
  条件式に `unix`、`windows`、`target_os`、`target_family`、`target_vendor`、`target_env` を含むものをいう。
  OS によって変わる値、エラー、制約は platform の interface から受け取る。
- 複数 crate が使う OS primitive (保持 handle、private file、atomic publication、file identity 等) は
  `kio-core` の platform に置き、OS 用の crate を新設しない。process の隔離は `kio-process` に閉じる。
- 対応 OS は macOS / Linux / Windows に限る。それ以外の target 向けの代替実装を置かず、build 時に拒否する。
- テストコードと検証 tooling は本規則の対象外とする。テストコードは、計測方法で数えない file と、`test` を
  必須とする cfg 条件 (`cfg(test)`、`cfg(all(test, ...))`) の付いた item である。`cfg(any(test, ...))` と
  `cfg(not(test))` の item は製品コードとして扱う。
- 規則の例外は計測ツールの許可リストに file と規則ごとに理由とともに登録し、件数を報告する。上限には例外を設けない。

**計測方法**

コード行は、各 crate directory 配下の Rust source (`src/`、`build.rs`、`examples/` を含む) から空行と
コメントだけの行 (doc comment を含む) を除いた行である。コードとコメントが同じ行にあれば数える。
`tests/`、`benches/`、`tests.rs`、`*_tests.rs`、`test_support.rs` とテストコードの item は数えない。
テストコードの module を別 file に置くときはこの file 名にし、この file 名を製品コードの module に使わない。
symbolic link、UTF-8 でない file、`platform/` の規定外の file、この file 名の規則に反する module 宣言、
テストコードの範囲や module の file を静的に決められない構文 (macro 内の out-of-line module 宣言等) は、
飛ばさず計測失敗とする。

| 区分 | 範囲 |
| --- | --- |
| OS 共通の製品ロジック | 製品 crate (`kio-core`、`kio-pipeline`、`kio-adapter`、`kio-index`、`kio-search`、`kio-app`、`kio-cli`、`kio-process`) のうち OS 固有層以外。`platform/mod.rs` を含む |
| OS 固有層 | 製品 crate の `src/platform/` 配下の OS 別実装。OS 別に数え、`unix` は Linux と macOS の共通部分とする |
| 検証 tooling | `kio-eval`、`kio-ci` (OS 固有部分を含む) |

**上限**

| 区分 | 上限 (コード行) | 2026-10-05 の推定値 |
| --- | ---: | ---: |
| OS 共通の製品ロジック | 100,000 | 約 101,000 |
| OS 固有層: Windows | 14,000 | 約 11,000 |
| OS 固有層: Unix 共通 | 9,000 | 約 7,600 |
| OS 固有層: Linux | 4,000 | 約 2,300 |
| OS 固有層: macOS | 3,000 | 約 1,300 |
| 検証 tooling | 65,000 | 約 63,000 |
| 製品 crate の 1 ファイル | 3,000 | 超過 5 本 |

1 ファイルの超過は `kio-app/src/lib.rs` (約 21,500)、`kio-core/src/scope.rs` (約 6,800)、
`kio-core/src/gc.rs` (約 4,800)、`kio-core/src/cas.rs` (約 4,700)、`kio-app/src/verify_objects.rs`
(約 3,800) である。推定値は OS 条件付きの item と OS 名を持つファイルを走査した近似であり、
OS 固有層を分離する前の値である。OS 共通ロジックの上限は現状を下回るため、v1 の構造是正で
対象外 OS 向けの代替実装と重複を削除して収める。

**運用**

- 上限と分離規則は `kio-ci` の `kio-code-limits` で計測し、CI で検査する。終了値は違反なしで 0、
  違反ありで 1、計測失敗で 2 とする。R 工程の途中の扱いは
  [受入完了計画 §10](../tasks/v1-acceptance-closeout-plan-2026-10-03.md) に従う。
- 上限を超える変更は、同じ変更の中で削減する。上限自体の変更は利用者の承認を得て、理由と日付を本節に記録する。
- 上限は設計肥大化の検知に使う。行数を減らすために必要な検証、エラー処理、回復処理を削らない。

## 3.1 機能 × Step 割当表

05/06/08 の契約機能がどの Step / Phase で実装されるかの **正本は本表**。各契約 spec の記述は「契約の内容」を定め、本表は「実装時期」を定める。本表にない機能を実装したくなったら、まず本表への追加 (と北極星シナリオとの対応確認) を行う。

| 機能 | 正本 | 実装 |
| --- | --- | --- |
| CAS raw object store + snapshot DAG (tree / commit) | [03-data-model.md](03-data-model.md) | Step 1 |
| `init` / `status` / `snapshot create` / `snapshot auto` / `log` / `diff` / `inspect` / `tag` | [06-cli-spec.md §1](06-cli-spec.md) | Step 1 + Phase 4 milestones 4–5 |
| `gc_policy` × `commit_type` 対応の schema 遵守 (GC 実行はしない) | [05-runtime.md §2.2](05-runtime.md) | Step 1 |
| JSON Schema validation (Step 1 は scope / manifest / config。以後各 Step で対象 schema を追加) | [06-cli-spec.md §10](06-cli-spec.md) | Step 1〜 |
| 観測ログ `events.jsonl` / `errors.jsonl` | [06-cli-spec.md §12](06-cli-spec.md) | Step 1 |
| 初回スキャン preview + 明示承認 / `.kioignore` | [06-cli-spec.md §2](06-cli-spec.md) / [10-operations.md §1](10-operations.md) | Step 2 |
| preview のコスト概算・budget 超過警告 | [06-cli-spec.md §2](06-cli-spec.md) / [10-operations.md §1](10-operations.md) | Step 2 |
| Prepare / Markdownize (full + incremental) / Adapter 実行 | [07-adapter-spec.md](07-adapter-spec.md) / [04-pipeline.md §3](04-pipeline.md) | Step 2 |
| 同梱 deterministic Adapter によるベースライン抽出 (normalized まで — 検索成立は Step 3 の index/search と合わせて) | [07-adapter-spec.md §2.1](07-adapter-spec.md) | Step 2 |
| Mistral OCR 系標準 Markdownize Adapter + embedded image 抽出・image object 保存 | [07-adapter-spec.md §5.2](07-adapter-spec.md) / [03-data-model.md §2](03-data-model.md) | Step 2 |
| batch / retry / resume / budget guardrail | [04-pipeline.md §5](04-pipeline.md) | Step 2 |
| `kio index` 完了時の auto snapshot (no-op 条件・HEAD 更新 — §1.1 の MVP 項目) | [05-runtime.md §8](05-runtime.md) | Step 2 |
| `kio adapter revoke` (network 承認の取り消し — opt-in 系と同時) | [07-adapter-spec.md §3](07-adapter-spec.md) / [06-cli-spec.md §1](06-cli-spec.md) | Step 2 |
| `kio repair registry-prune` (恒久到達不能 registry 行の確認付き退役) | [10-operations.md §3](10-operations.md) | Step 3 |
| 構造化 task/artifact descriptor (Adapter 境界の内部 API) | [06-cli-spec.md §9](06-cli-spec.md) | Step 2 |
| secrets Tier A/B 除外 + quarantine + `--yes` 制約 + `approval_method` 記録 | [10-operations.md §1.1](10-operations.md) / [06-cli-spec.md §2](06-cli-spec.md) | Step 2 |
| chunk / Embedding / FTS5 / sqlite-vec | [04-pipeline.md §4](04-pipeline.md) | Step 3 |
| hybrid search (RRF / MMR / paging / cursor) | [05-runtime.md §1](05-runtime.md) | Step 3 |
| Evidence Pointer 発行・解決 / `kio open` / `kio view` | [08-evidence-pointer-spec.md §2-3](08-evidence-pointer-spec.md) | Step 3 |
| `kio search --json` (外部 Agent 向け最小契約) + `index_status` | [05-runtime.md §1.7](05-runtime.md) | Step 3 |
| `kio reindex` (gen+1 の再 Markdownize / 再 index) | [07-adapter-spec.md §9](07-adapter-spec.md) / [03-data-model.md §6](03-data-model.md) | Step 3 |
| 観測ログ `metrics.jsonl` / `access.jsonl` (M3 の latency 計測に必要) | [06-cli-spec.md §12](06-cli-spec.md) / [05-runtime.md §7](05-runtime.md) | Step 3 |
| `export --to` / `--at` / `--all-history` / `--include-deleted` | [05-runtime.md §4](05-runtime.md) | Step 4 |
| purge 最小形 (tombstone + `commit_type=purged` + 検索除外 + `--erase-tombstone` + ログスクラブ [10-operations.md §7](10-operations.md)) | [05-runtime.md §3](05-runtime.md) / [08-evidence-pointer-spec.md §4.1](08-evidence-pointer-spec.md) | Step 4 |
| `kio repair rebuild-db` (SQLite index 再構築 — 破損時の復旧経路) | [10-operations.md §7.5.3](10-operations.md) | Step 3 |
| `kio repair verify-objects` (CAS object 整合性検証) / `--prune-orphans` (orphan prepared/image 削除 — 法務 purge の完結手段) | [10-operations.md §7.5](10-operations.md) | Step 4 |
| `kio evidence verify <pointer>` (単発) | [08-evidence-pointer-spec.md §4.3](08-evidence-pointer-spec.md) | Step 4 |
| retention shallow 候補の read-only planner (`kio gc --dry-run`) | [06-cli-spec.md §6.1](06-cli-spec.md) / [05-runtime.md §2.2-2.4](05-runtime.md) | Phase 4 milestone 1 |
| receipt先行・crash recovery付きの on-demand tree-only shallow sweep (`kio gc [--yes]`) | [06-cli-spec.md §6.1](06-cli-spec.md) / [05-runtime.md §2.2-2.3](05-runtime.md) | Phase 4 milestone 2 |
| 明示opt-inのbounded tiered retention hook (`gc.mode="after_index"`、successful index/manual snapshot後) | [06-cli-spec.md §6.1](06-cli-spec.md) / [05-runtime.md §2.3-2.5](05-runtime.md) | Phase 4 milestone 3 |
| 定期 auto snapshot (OS スケジューラ委譲、常駐なし) | [05-runtime.md §8](05-runtime.md) | Phase 4 milestone 4 |
| Rust-only on_idle GC (OS scheduler が起動する `kio snapshot auto` に限定、常駐なし) | [05-runtime.md §2.3](05-runtime.md) | Phase 4 milestone 5 |
| `kio evidence verify --batch` | [08-evidence-pointer-spec.md §4.3](08-evidence-pointer-spec.md) | Phase 4 milestone 6 (implemented target/current) |
| `kio evidence retarget <pointer> --at <commit>` | [08-evidence-pointer-spec.md §5](08-evidence-pointer-spec.md) | Phase 4 milestone 7 (implemented current) |
| Rust-only unreachable-object read-only inventory (`kio gc --dry-run --prune-unreachable`) | [06-cli-spec.md §6.2](06-cli-spec.md) / [05-runtime.md §2.7](05-runtime.md) | Phase 4 milestone 8 (implemented current) |

注: milestone 1–5 は各 current spec の実装済み契約、milestone 6 の batch verify、milestone 7 の exact-only retarget、milestone 8 の read-only inventory は implemented current である。milestone 8 は物理 prune、non-tree CAS sweep、CoW GC を承認しない。これらは Phase 4 全体の実装済み表明ではない。

---

# 4. 北極星シナリオ (Phase 3 完成時の Done 条件)

この節の PersonaScope/personaCorpus による性能・品質評価は v1 の nonblocking とし、準備後に行う。
これは機能、security、recovery の受入を不要にするものではなく、v1 の定義を上書きしない。

実装中の機能追加判断は「**3 シナリオのどれに resp するか**」で評価する。該当しないなら Phase 4-5 へ送る。

## M3-1: 「3ヶ月前に書いた結論の根拠 PDF を 5 秒以内に出す」

```
状況:  PDF のファイル名は覚えていない。本文の数値や用語の一部だけ覚えている。
操作:  kio search "X の根拠 数値Y" → kio open <evidence>
検証:  hybrid search / Evidence Pointer 表示 / 原本回帰
完了:  - p95 < 5 秒 (20 scopes / 合計 10 万 chunk indexed、横断検索デフォルトで計測)
       - Evidence Pointer に commit + raw_hash + chunk_hash + heading_path + span
       - kio open は OS 規定アプリで原本を開く (working tree 優先、無ければ CAS から
         read-only 一時展開。06-cli-spec.md §1.1)
       - ベースライン優位: 既存手段で失敗しやすいクエリ集合 Q_hard (スキャン PDF の
         画像内テキスト / 語彙一致しない言い換え / 図表・画像の内容参照、20 問以上) で、
         Spotlight (mdfind) と ripgrep-all をベースラインに Recall@10 を比較し、
         Kio >= 0.8 かつ各ベースラインを 0.3 以上上回る
```

## M3-2: 「リネーム済みファイルの過去版を含めて検索」

```
状況:  資料をリネームした。過去名で書いた他メモから「あの資料」を探したい。
操作:  kio search "認証仕様" --all-history → kio view <evidence-at-commit-X>
検証:  --all-history / raw_hash 同一性 (リネームで死なない) / 過去版閲覧
完了:  - リネーム前後で同じ raw_hash の chunk が両方ヒット
       - 結果に path_at_commit と現在 path を併記
       - 過去版 Markdown は再生成せず当該 commit の object をそのまま返す
```

## M3-3: 「削除したはずの資料から特定の数字を再発見」

```
状況:  半年前に削除した資料の中の数字をもう一度見たい。
操作:  kio search "API リミット 1000" --include-deleted → kio export <ev> --to ./recovered/
検証:  CAS 永続性 / --include-deleted / export の working tree 非破壊
完了:  - 削除済みファイルの chunk が結果に出る
       - kio export は --to <dir> を必須 (working tree 直接書き戻し禁止)
       - purge 済み (canonical final event = purged — 08 §3.1 手順 5。commit_type=purged はその監査痕跡) は検索結果から除外される (purged chunk 行は物理削除済み — search 経由では到達しない)。tombstone 応答は既存 Evidence Pointer (過去回答の保存分) を export / verify / open に与えた場合の挙動 (08 §4)
```

## 4.1 計測項目

```
Latency       p50 / p95 / p99       目標: M3-1 p95 < 5秒, M3-2/3 p95 < 7秒
                                    (前提: 20 scopes / 合計 10 万 chunk。05-runtime.md §1.8)
Recall        Recall@10 / @20       目標: 各シナリオで Recall@10 >= 0.8
Baseline      Q_hard での対 Spotlight/rga 優位   目標: M3-1 完了条件のとおり (Kio >= 0.8, 差 >= 0.3)
Evidence      必須フィールド充足率   目標: 100%
Working tree  上書き 0 件            CI で常時検出。違反はリリースブロッカー

初回体験 (基準データセット D1: PDF 1,000 本 / 5GB 相当)
TTFV (baseline)   kio init → ベースライン index 完了 → 初回 kio search 成功
                                       目標: 30 分以内 / LLM コスト $0
TTFV (enriched)   online 承認 → 最初の 100 ファイルが AI 強化済みで検索可能
                                       目標: 承認から 15 分以内
Cost 予実比       preview 概算 vs 実績  目標: 乖離 ±30% 以内 (D1 全量 AI 強化時)
試算根拠          Markdownize 単価      Mistral OCR 4 Batch $2 / 1,000 pages 前提
                                       (研究メモ: 旧 research/markdown.md — git 履歴。単価改定時は本表を更新)
```

### P2 scale evaluator contract

P2 の性能 fixture は Rust v3 の二つの create-only lane である。`current-text` は base source
だけを index し、`history-overlay` は同じ base を index 後に全20 scopeで edit・rename・deleteを
各1件適用して final HEAD を index する。二つの destination を共有・上書き・adoptしてはならない。

| profile / lane | current | historical-only | deleted | physical |
| --- | ---: | ---: | ---: | ---: |
| tiny / current-text | 180 | 0 | 0 | 180 |
| tiny / history-overlay | 120 | 120 | 60 | 240 |
| full / current-text | 120,000 | 0 | 0 | 120,000 |
| full / history-overlay | 119,400 | 1,200 | 600 | 120,600 |

`scale prepare` は正式な `init → offline index` だけを使い、history overlayにも同じ再index経路を
使う。`scale attest` は生CAS、commit/tree、SQLite/FTS/vector、registryをread-onlyで再計算する。
top-10 Pointer の再検証は evaluator-local strict wire と生CASで完結し、production の
`EvidencePointer::validate` を呼ばない。これは評価証跡が被評価のproduction verifierへ権威を委譲しない
ための境界である。

5 benchmark lane は `current-text` (text)、`vector`、`hybrid`、`history` (all-history text)、
`deleted` (include-deleted text) である。requested mode と resolved mode は一致し、fallbackは常に
falseでなければならない。vector/hybridのtext fallbackは測定ではなく失敗とする。評価用の決定論embeddingは
exact selectorでのみ有効な実Adapter wireで、network不可・非課金である。deleted laneは独立attest済みの
正解Pointerをprivateな`--to`先へ復元し、raw hash一致とfixture working tree不変の両方をreportで証明する。

D1 の baseline/enriched TTFV と preview/actual cost は tagged `measured` / `not-measured` /
`blocked` 状態で表す。証拠のない値を0やpassへ変換しない。Full の5 warmup/100 samplesは手動の
scale acceptanceであり、actual D1とdogfoodはP4の別ゲートである。push CIはTiny二laneの契約smokeだけを
実行し、Fullや性能合格を主張しない。

Q_hard の Rust 計測は `kio-eval benchmark qhard` を正本とする。これは外部 fixture の
attestation (tree / XDG environment / registered scopes / frozen golden digest) を要求し、fixture
未配置・未 attest を pass や historical result の再利用として扱わない。Q_hard 8 問だけの
report は測定値であり、M3-1 の 26 問 / 21 hit 合算判定には `--synthetic-corpus` により
同一実行内で再測定された frozen synthetic M3-1 18 問が必要である。外部結果 artifact は
受け付けない。

Spotlight/rga との baseline 比較は、Q_hard 8 問や synthetic M3-1 18 問とは混同しない
別の凍結母集団 `eval/golden-queries-fixture-b.jsonl`（hard1/2/3 各8、24問、
`sha256:bdad3e02c4b70f721e882d7f24c8b5b442621be7c0c03593afde41b8ebca7d45`）で行う。
正本は Rust の `kio-eval benchmark baseline` である。実行前に
`kio-eval benchmark baseline-attest` が indexed fixture と `.kio` を除いた pristine
tree の同値性、p01..p20、golden を束縛する attestation を生成する。baseline runnerは
Rust実装だけを保持する。保存済み JSON は計測証拠ではなく、実測の pass を主張しない。
macOS の rga comparator が信頼する runtime と封印の条件は [eval/README.md](../eval/README.md) に従い、
条件を満たさない実行は `blocked-unmeasured` として比較の pass を成立させない。

## 4.2 評価クエリの凍結規律

シナリオ M3-1〜M3-3 は追加・差し替えしない。ゴールデンクエリは凍結し、既存クエリの差し替えや
削除をしない (悪化を隠すための削除を禁止する)。クエリを増やす場合は、結果を測定する前に別ファイル
として追加し、件数と digest を [eval/README.md](../eval/README.md) に記録して再凍結する。
Q_hard 8 問と横断 16 問の増補はこの方式による。

## 4.3 Recall 評価規約 (ゴールデンクエリ)

§4.1 の Recall@10 >= 0.8 は次の規約で計測する。

評価コーパス (2 種):

```text
synthetic  リポジトリ同梱の合成コーパス (公開可能な文書 + 生成文書、200-500 ファイル規模)。
           複数 scope (.kio) 構成で fixture 化し、fixture script が
           「編集 → commit → リネーム → commit → 削除 → commit」の履歴シナリオを
           決定論的に再現する。CI / Done 判定の正本
dogfood    開発者自身の実フォルダ (非公開)。数値は公開せず、3 シナリオの主観成功確認に使う
```

ゴールデンクエリ:

- シナリオ M3-1 / M3-2 / M3-3 ごとに **15 件以上**、`eval/golden-queries.jsonl` としてリポジトリに保持する
- 各行: `{ "scenario": "M3-2", "query": "...", "flags": ["--all-history"], "expected": [{ "scope": "research", "file": "auth-spec.md", "path_at_commit": "auth-spec.md", "section": "api-token" }] }`
- expected は `{ scope, file, section }` の分離形式で書く。**M3-2 (rename / 編集を含む履歴シナリオ) の expected 要素には `path_at_commit` (または対象 commit) を併記し、同一 file の版を一意化する** (rename 前後は別の expected 要素) (section = chunk の `section_id` (slug — [04-pipeline.md §4.1](04-pipeline.md))。heading 原文ではない) (path 区切りを含む文字列にしない。スコープ境界は [03-data-model.md §3](03-data-model.md) の「直下のみ」規則)。raw_hash は取り込み後に確定するため、評価ハーネスが取り込み時に `{ scope, file }` → raw_hash / chunk へ解決する
- M3-2 は `--all-history`、M3-3 は `--include-deleted` で実行する

判定:

```text
Recall@10 = |expected ∩ 上位10件の distinct (raw_hash, section)| / |expected| のクエリ平均
            (--all-history シナリオ (M3-2) は distinct 射影と expected 解決を
             (raw_hash, section, path_at_commit) で行う — リネーム前後の両ヒットを別要素として
             数える。raw_hash はリネームで不変のため、この拡張なしには M3-2 完了条件
             「両方ヒット」が計測不能)
Done 条件 = synthetic で各シナリオ Recall@10 >= 0.8
          + dogfood で 3 シナリオの手動成功確認
```

クエリの追加は §4.2 の方式に限る。

---

# 5. 決定済みの設計論点

Phase 1 着手前に確定した論点の結論と正本である。

| 論点 | 結論 | 正本 |
| --- | --- | --- |
| Markdown 非決定性 | first-instance-wins。gen+1 は `kio reindex --regenerate` か prepared_hash の変化に限る | [03-data-model.md §6](03-data-model.md) / [04-pipeline.md §5.5](04-pipeline.md) |
| Dead Evidence Pointer | 既定は tombstone。`--erase-tombstone` は法的要件のある場合だけ。`kio evidence verify` は 6 値の status を返す | [08-evidence-pointer-spec.md §4](08-evidence-pointer-spec.md) / [05-runtime.md §3](05-runtime.md) |
| Incremental Markdownize の規約 | 入出力 schema は Kio が固定し、プロンプト規約 5 項に従う | [07-adapter-spec.md §8](07-adapter-spec.md) / [04-pipeline.md §3.1](04-pipeline.md) |
| 検索評価ハーネス | 合成コーパス、履歴 fixture、凍結したゴールデンクエリで計測する | §4.3 / [eval/README.md](../eval/README.md) |
| Markdownize Adapter | Mistral OCR 系を標準にする | [07-adapter-spec.md §5.2](07-adapter-spec.md) |
