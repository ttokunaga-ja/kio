# 09 MVP Scope

統合元: 旧 `north-star-scenarios.md` + 旧 `design-homework.md` + 旧 `consolidation-plan.md` の Phase plan + `01-positioning.md` から MVP/Phase 部分の抜粋 (research 検討メモは 2026-07-18 に撤去 — git 履歴で参照可)。

> 本書は **実装着手前に確定する論点** を一所に集める。Step 1 着手前に §1-§4 を確定する。§5 の各宿題の確定期日と現在の status は **§5.4 の表が正本** (本行に期日を再掲しない — 転記の陳腐化が gate を誤発動させるため)。

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
- restore (--to 必須)
- time-travel search (--at / --all-history / --include-deleted)
- ベースライン index (deterministic 抽出 + FTS。API キーなしで init→index→search→open が成立 — [01-positioning.md §3](01-positioning.md))
- 初回スキャン preview + 明示承認
- budget guardrail (cost ceiling / kill switch)
- purge 最小形 (tombstone + commit_type=purged + 検索除外 + ログスクラブ。M3-3 の完了条件)
- kio evidence verify <pointer> / kio evidence verify --batch <pointers.jsonl> (Phase 4 milestone 6 の implemented target/current)
```

## 1.2 RC platform support policy

本表が RC の platform support level と、子 scope 生成経路が RC 対象かどうかの
**唯一の正本**である。他の spec は個々の実装・CLI・運用上の帰結だけを定め、
support level を別に定義しない。

| platform | RC support level | ユーザーが直接選択した scope の `init` / `index` | 親 scope からの再帰的な子 scope 自動 mutation |
| --- | --- | --- | --- |
| macOS | supported | RC 対象 | RC 対象 (retained-handle launcher) |
| Linux | supported | RC 対象 | RC 対象 (retained-handle launcher) |
| Windows | experimental | RC 対象 | RC 対象外。preview は planned child を報告するが、approve は mutation 前に `KIO-E-SCOPE-BOUND-UNSUPPORTED-001` の structured partial failure として fail-closed する |

Windows で新しい scope を導入する正式な手動手順は次の 3 ステップに限る。

1. `kio init <child-path>` を実行する。
2. `<child-path>` をプロセスの cwd とする。
3. その cwd で `kio index --approve --offline` を実行する。

これはユーザーが直接選択した独立 scope への操作であり、親 scope の discovery 結果から
public pathname を再解決する fallback ではない。Windows の自動子 scope に
`current_dir(path)` 型の handoff を追加せず、junction / reparse point も追跡しない。

# 2. 将来ロードマップ（historical。製品境界は 11 を参照）

直前の RC matrix は RC.3 の制約であり、v1 の platform support を表明しない。特に Windows の
自動 child mutation は RC で未対応である。

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

# 3. 実装 Step とコード規模上限

```
Step 1 (1-2ヶ月): kio-core + kio-cli で init / status / snapshot / log / diff / inspect / tag
                  → CAS と snapshot DAG の正しさを早期検証
Step 2 (2-3ヶ月): kio-pipeline + kio-adapter
                  → 同梱 deterministic Adapter でベースライン抽出 (normalized まで。**検索の成立は
                    Step 3 の chunk/FTS/search 実装と合わせて** — §3 の割当が正)
                  → 推奨構成は大手 LLM API による AI 強化 (opt-in)
                  → tree は manifest_hash と必須 chunking_config_hash を保持し、検索の publication
                    authority は tagged `chunk_publications` event triple に置く (03 §8 / 04 §4.1)
Step 3 (2-3ヶ月): kio-index + kio-search (hybrid + Evidence Pointer)
Step 4 (1.5-2ヶ月): restore + --at + time-travel
                    + purge 最小形 (tombstone) + evidence verify (単発)
```

**コア規模上限 (ripgrep 以下)**:

```
テスト除いて   11,000 - 16,000 LOC (Rust)
テスト含めて   20,000 - 30,000 LOC
```

```
Step 別の目安 (テスト除く):

  Step 1   2,500 -  4,000 LOC   CAS / DAG / init / status / snapshot / log / diff
  Step 2   3,500 -  5,000 LOC   pipeline / adapter / budget / resume / retry
  Step 3   3,500 -  5,000 LOC   FTS / vector / hybrid / Evidence Pointer
  Step 4   1,500 -  2,500 LOC   restore / time-travel / purge 最小形 / verify
  合計    11,000 - 16,000 LOC   (総期間 7-10 ヶ月。Step 別最大の単純合計 16,500 は
                                 総額上限 16,000 に切られる — 全 Step 同時に上限へ達する配分は取らない)
```

これを超えるなら設計肥大化の兆候。テスト除き 16,000 LOC を超えたら削減先を検討する。総額上限 11,000-16,000 LOC 自体は動かさない。7 クレートを一度に書こうとしないこと。

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
| `kio reindex` (gen+1 の再 Markdownize / 再 index) | [07-adapter-spec.md §9](07-adapter-spec.md) / [09-mvp-scope.md §5.1](09-mvp-scope.md) | Step 3 |
| 観測ログ `metrics.jsonl` / `access.jsonl` (M3 の latency 計測に必要) | [06-cli-spec.md §12](06-cli-spec.md) / [05-runtime.md §7](05-runtime.md) | Step 3 |
| `restore --to` / `--at` / `--all-history` / `--include-deleted` | [05-runtime.md §4](05-runtime.md) | Step 4 |
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

## 3.2 Step 1 着手ゲート

Step N の着手条件は「§5.4 で期日が『Step N 着手前』の行がすべて decided」という機械的チェックとする。**期日 cell に未完注記 (「〜を除き充足」等の but 書き) が残る行は decided 扱いしない** — #5 は M3-1 の増補完了時に件数と query set digest (凍結済み `eval/golden-queries.jsonl` の raw UTF-8 bytes の sha256 — `sha256:<lowercase-hex>` 表記) を当該行へ追記して注記を除去する (= 再凍結の機械記録。それまで Step 3 の着手条件を満たさない)。主観判定 (「だいたい固まった」) は用いない。

Step 1 開始日: **2026-07-16**。本日 (2026-07-02) 時点の Step 1 ブロッカーは #1 / #4 で、いずれも decided 済み (§5.4) のため、上記日付までに残る作業は本改訂のドキュメント反映のみ。開始日を過ぎても着手しない場合、その理由を本節に追記する (理由なき延期の可視化)。

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
操作:  kio search "API リミット 1000" --include-deleted → kio restore <ev> --to ./recovered/
検証:  CAS 永続性 / --include-deleted / restore の working tree 非破壊
完了:  - 削除済みファイルの chunk が結果に出る
       - kio restore は --to <dir> を必須 (working tree 直接書き戻し禁止)
       - purge 済み (canonical final event = purged — 08 §3.1 手順 5。commit_type=purged はその監査痕跡) は検索結果から除外される (purged chunk 行は物理削除済み — search 経由では到達しない)。tombstone 応答は既存 Evidence Pointer (過去回答の保存分) を restore / verify / open に与えた場合の挙動 (08 §4)
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

macOS の rga comparator は、ユーザー所有 package prefix を信頼根にしてはならない。baseline 実行は
管理者提供の root-owned / group-other 非書込み runtime root を明示し、rga、rga-preproc、pandoc、pdftotext、rg
と custom adapter を空に固定した設定ファイル、および Mach-O dynamic dependency closure を canonical path・digest
とともに束縛する。`@loader_path`、
`@executable_path`、`@rpath`、symlink の runtime 外 escape、未解決または非sealed dependency は
`blocked-unmeasured` とし、比較の pass を成立させない。明示的に sealed と検証した macOS system library root
だけを runtime 外 terminal dependency として許し、dynamic loader は sealed `/usr/lib/dyld` に固定し
`LC_DYLD_ENVIRONMENT` は拒否する。各 comparator subprocess の前後と計測 finalization で closure を再帰的に
再解決し、初期 binding の canonical path・trust class・SHA-256・closure digest と完全一致しない場合は、
高優先度の `@rpath` 候補追加を含めて `blocked-unmeasured` とする。
さらに runtime root は macOS `MNT_RDONLY` の read-only mount に限定し、bind・各 comparator subprocess の前後・
finalization で public path と retained root descriptor の mount identity が初期値と一致しなければ
`blocked-unmeasured` とする。report は read-only 判定と mount identity を closure provenance に含める。

## 4.2 シナリオ凍結規律

Step 1 着手後は **シナリオの追加・差し替えしない**。Phase 1-3 完了までシナリオを動かさない。例外: 物理的に実装不可能と判明した場合のみ本書で撤回 + 代替採用。**一回限りの例外**: M3-1 の Q_hard を §4.1 の「20 問以上」へ増補する**追加のみ**、**Step 3 着手前**に限り認める (既存問の差し替えは不可 — 増補後に再凍結し、以後この例外は消滅する)。この増補に伴う #5 行の件数・digest 追記は本例外の完遂手続きであり、§6.2 のドキュメント凍結の対象外とする。

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

クエリの追加・差し替えは §4.2 の凍結規律に従う — 認められるのは M3-1 の一回限り増補 (Step 3 着手前) のみで、他のクエリ集合は Step 1 着手後は動かさない (悪化を隠すための削除は禁止)。

---

# 5. 設計上の宿題 (実装で必ずぶつかる論点)

## 5.1 Markdown 非決定性の運用 — first-instance-wins

```
問題: 同じ (raw_hash, tool_profile_hash) から複数回生成した結果が LLM 非決定性により異なりうる。
採用: 最初に確定したインスタンスを永続化、以後は再生成しない (first-instance wins)。
実装:
  - normalization_run のキャッシュヒット判定で短絡
  - 新 generation (gen+1) の instance 作成は kio reindex --regenerate、または prepared_hash 変化起因の自動 gen+1 ([03-data-model.md §2.1](03-data-model.md) の例外) のみ許可 (上書き・削除はしない)
  - 新 instance 作成時 (raw 跨ぎ incremental の g0 を含む) は manifest の parent_gen (同一 raw 内) / parent_instance = {raw_hash, tool_profile_hash, gen} (raw 跨ぎ incremental のみ必須 — full では null) でチェーンを残す (parent_run_id は task cache の揮発情報 — 永続 provenance ではない。[03-data-model.md §8](03-data-model.md))
  - 過去 commit / 既存 Evidence Pointer は tree entry の gen により旧 instance を参照し続ける
正本: 03-data-model.md §6, 04-pipeline.md §5.5
Status: decided (Step 1 着手前確定)
```

## 5.2 Dead Evidence Pointer のセマンティクス

```
問題: 「Evidence Pointer の不変性」と「法務 purge」の緊張領域。purge 後の pointer 挙動が未定義。

設計案:
  1. raw_hash の canonical final event = `purged` (全 marker 正本化 — 08 §3.1 手順 5) → tombstone レスポンス
     { "status": "tombstoned", "purged_at", "purged_reason", "purged_in_commit", "raw_hash" } (正本 08 §4.1)
  2. raw_hash が完全削除 → KIO-E-PURGE-NOT-FOUND-001

  検出 API:
  kio evidence verify <pointer> [--strict]
    → status = 6 値 union (正本 08 §4.3 — alive | tombstoned | not_found |
               scope_unreachable | unverifiable | registry_duplicate)

決定済み:
  - デフォルトは tombstone。完全削除 (`--erase-tombstone` — public tombstone なしの NOT-FOUND 化。
    tree/commit 再結線・filename 秘匿の履歴書き換えは含まない — §3.1 のとおり v2+/Phase 4+) は
    法的要件上必要な場合のみ (正本 08 §4.2)
  - tombstone レスポンス schema (正本 08 §4.1)
  - 完全削除時は KIO-E-PURGE-NOT-FOUND-001 (正本 08 §4.2)
  - 検出 API: kio evidence verify <pointer> [--strict] → 6 値 union (正本 08 §4.3)

残未決: なし
  (二重 purge は 2026-07-18 に確定済み — 再 purge は lifecycle events[] へ `purged` を追加 append する。
   tombstone 判定は「active = 末尾 event が purged」であり、存在だけでは dead にしない — marker 単独の
   規則。解決は 08 §3.1 手順 5 の canonical final event に正本化してから評価する — 正本 05 §3.5)

正本: 08-evidence-pointer-spec.md §4 / 05-runtime.md §3
Status: decided。batch verify は Phase 4 milestone 6 の implemented target/current。
```

## 5.3 Incremental Markdownize のプロンプト規約

```
問題: 「旧 raw + 旧 Markdown + 新 raw を Adapter に渡して差分更新」の挙動を Adapter 任せにすると揺れる。

設計 (schema は確定済み, [04-pipeline.md §3.1](04-pipeline.md)):
  入力 schema / 出力 schema は Kio が固定。
  Adapter 側プロンプト規約:
  - "unchanged" と判断した unit は出力に含めない (旧 unit を再利用)
  - 変更 unit は完全に書き直す (部分編集ではなく)
  - heading 構造の変更は Kio には影響しない (chunk side で対応)
  - Adapter が「軽微とは言えない」と判断したら fallback_to_full=true

決定済み:
  - 入出力 schema (正本 04 §3.1)
  - プロンプト規約 5 項: unchanged unit 非出力 / full unit replacement /
    heading 変更は chunk side 対応 / fallback_to_full 短絡 (正本 07 §8.1)
  - fallback_to_full の閾値 hint 衝突時は Kio 側を優先 (正本 07 §8.1)
  - ストリーミング応答: 許容。staging に保持し全体検査後に一括公開、中断は failed (retryable) — pending 状態は無い (正本 07 §8.3)
  - spec_version 不一致は Adapter が invalid_input として失敗、当該 Adapter は failed permanent (full fallback は incremental capability 非互換のみ — 正本 07 §8.1)
  - spec_version の bump 規約 (正本 10 §11.5)

残未決: なし

正本: 07-adapter-spec.md §8 / 04-pipeline.md §3.1 / 10-operations.md §11.5
Status: decided
```

## 5.4 進行状況テーブル

| # | 項目 | Status | 残未決 | 期日 |
| --- | --- | --- | --- | --- |
| 1 | Markdown 非決定性 = first-instance-wins | decided | なし | Step 1 着手前 (充足済み) |
| 3 | Dead Evidence Pointer | decided | なし（batch verify は Phase 4 milestone 6 の implemented target/current） | 充足済み |
| 4 | Incremental Markdownize プロンプト規約 | decided | なし | Step 1 着手前 (充足済み) |
| 5 | 検索評価ハーネス (合成コーパス + ゴールデンクエリ、§4.3) | decided | なし (2026-07-03 完了: `eval/` に合成コーパス 305 ファイル / 7 scope + 履歴 fixture + ゴールデンクエリ 50 件 (M3-1: 18 / M3-2: 16 / M3-3: 16)。dry-run 検証済み。以後のクエリ追加・差し替えは §4.2 凍結規律。**M3-1 Q_hard 増補は 2026-07-23 完了・再凍結** (§4.2 の一回限り例外の完遂 — 本追記は §6.2 凍結対象外の完遂手続き): 「Step 3 着手前」の期日は失効していたため同日のユーザー裁定で失効後実行。増補 8 問 (hard1 ×4 + hard3 ×4、全問を結果測定前に投入 = 事前コミット) は実データ fixture (raster PDF / PPTX 図表・画像) を正解担体とするため合成コーパスに載らず、**別ファイル方式**で再凍結する: 既存 `eval/golden-queries.jsonl` は 50 件のまま不変 (digest sha256:b7183fa3586383883ec522256696268eab8e607c1a032020e09223158a5bf08d)、増補分は `eval/golden-queries-qhard.jsonl` 8 件 (digest sha256:d5c30eccc664e6bd4d96e1068970e225d209d04bde34c50eab300d6245d4e163、Rust runner `kio-eval benchmark qhard`)。M3-1 の Done 判定は以後**合算 26 問で Recall@10 >= 0.8 (= 21 問以上)**)。**横断増補は 2026-07-26 完了・再凍結** (§4.2 の別ファイル方式を再適用): 既存 50 問は
**全問 expected が単一 scope に閉じており**、`--all-scopes` で 7 scope を横断はするものの「複数 scope から答えを
組み立てる」形が 1 問も無かった。増補 16 問 (M3-1 ×8 / M3-2 ×4 / M3-3 ×4、全問 expected が 2 scope に跨る) を
`eval/golden-queries-crossscope.jsonl`
(digest sha256:1fe0ebf2b51f35323d91bb1a235a282b5fa68a59de7a9c0bac2bc0f4ebade868、専用ランナー
`kio-eval crossscope`) として凍結する。**正解担体は合成コーパスの既存 anchor そのもの**であり
既存 corpus/history scenario の担体と決定論には手を入れていない。現行の Rust-only
history plan は、その操作列と実際の edit 後 bytes を current schema で固定する。
Rust専用ランナーである理由は `HISTORY_QUERY_COUNT`(=16 厳密一致) と `assess_history_coverage`(rename 7/edit 3/delete 9 の
全 anchor 掘り起こし) が**セット全体の契約**であり、部分集合に当てると必ず落ちるためである。
**重要な計測所見: Recall@10 はこの欠陥クラスをほぼ検出できない** — replica を無効化しても 16 問すべて 1.000 のままだった
(合成コーパスは小さく各 expected が固有数値を持つので per-scope 順位でも 10 位以内に入る)。横断融合の欠陥が動かすのは
**順位**なので、専用ランナーは診断値 `worst_expected_rank` (2 つの expected の遅い方の 1-based 順位) を併記する。
実測: **replica が採点する 8 問で 4.75 → 2.00** (2.00 は expected 2 件時の理論下限、8 問すべて到達)、
**replica が辞退する履歴 8 問は 5.38 → 5.38 で完全同値** (時間選択子ガードにより replica が触れないことの裏付け)。**[2026-08-11] この計測手法は再現不能になった** — (1) `time_selector` ガードを撤回し replica が履歴も採点するようになったため「辞退する 8 問」が存在しない、(2) scatter-gather 経路を廃止したため「replica を無効化する」比較対象が無い ([05-runtime.md §1.8](05-runtime.md))。**[2026-08-12 replica 単独経路の再測定]** 同一の 7 scope / 16 問で Recall@10 は全シナリオ 1.000 を維持した。移行前は `worst_expected_rank` が **mean 3.69 / max 10**（M3-1 2.00、M3-2 5.75、M3-3 5.00）、移行後は **mean 2.50 / max 4**（M3-1 2.00、M3-2 3.50、M3-3 2.50）。履歴 8 問も per-scope 順位での融合ではなく collection 内順位となり、期待どおり改善した。短語 24 問も Recall@10 **0.9167 (22/24)** を維持し、同一環境で記録した p95 は 171.27ms → 166.98ms だった（latency は実行ごとに変動する） | 充足 (2026-07-23 増補完了 — 窓失効後実行の裁定含め本行が機械記録) |
| 6 | Markdownize Adapter 選定 = Mistral OCR 系 ([07 §5.2](07-adapter-spec.md)) | decided | なし (実地検証 2026-07-03 完了: sync/batch 両モードで表 1.0 / 日本語 CER 0.0 / 画像 1/1 / 数式 LaTeX 化。`experiments/ocr-verification`) | Step 2 着手前 (充足済み) |

Step N の着手条件は「期日が『Step N 着手前』の行がすべて decided」の機械的チェック (§3.2)。2026-07-02 の本改訂適用後、Step 1 のブロッカーは 0 件。

---

# 6. ドキュメント統合ゲート

実装着手前にドキュメントを **10-12 本に圧縮** する (統合済み)。

## 6.1 現在の構造 (確定)

```
docs/
  README.md
  01-positioning.md            ★core / 競合 / 差別化
  02-philosophy.md             理念
  03-data-model.md             ★契約: CAS / identity / 書き込み境界
  04-pipeline.md               ★契約: パイプライン / SQLite / batch
  05-runtime.md                ★契約: 検索 / commit / GC / purge / restore
  06-cli-spec.md               CLI / exit code / error / JSON output
  07-adapter-spec.md           Adapter / incremental プロンプト規約
  08-evidence-pointer-spec.md  Evidence Pointer / Dead Pointer
  09-mvp-scope.md              本書
  10-operations.md             横断規約 (semver / 観測 / リネーム表)
```

## 6.2 凍結ゲート

```
Step 1 着手後はドキュメントを凍結する。
凍結を破る条件:
  1. Step 1-4 で実装が物理的に不可能と判明した設計
  2. 外部 Agent との互換性を破壊する変更
  3. データ破壊リスクのある誤り
それ以外の「綺麗にする」「より良い表現にする」は Step 4 完了後に回す。
```

設計判断の経緯は git history で追える。本プロジェクトでは ADR フォルダを採用しない (Phase 1 着手前の小規模プロジェクトでは spec 一本化の方が運用コストが低い)。
