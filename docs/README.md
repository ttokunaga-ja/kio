# Kio 設計ドキュメント

> **Local-first knowledge archive, powered by frontier AI.**
> **データはローカル、計算は最強の AI を使う。**

Kio は **local-first** な知識アーカイブ。データの主権はあなたのマシンにあり、Markdownize や Embedding には外部 Adapter を opt-in で利用できる。現行 RC の組込み接続は Mistral OCR と Gemini embedding が中心であり、任意の Claude / GPT 等を呼ぶ汎用 dispatcher は未提供である。実 provider の利用可能性は接続試験で確認する。

二次表現: **Evidence-grounded local knowledge archive (原文根拠付きローカル知識アーカイブ)**。

> **第一価値命題**: 「探せなかったファイルがすぐ見つかる」「根拠が死なない」。

---

# 0. Kio の中核 (3 点)

```
1. Evidence Pointer        path ではなく commit / tree / raw_hash / chunk_hash / span で根拠を指す
2. Markdown 正規化         全ファイル種別を Normalized Markdown に変換、人間と AI が同じビューを使う
3. Content-addressed CAS   全ファイルを CAS object として保存。削除済み・過去版・移動済みでも到達可能
                           (必要な履歴の保持と現行policyによる許可が必要。shallow / purge / erase は別扱い)
```

最低体験ライン:

引用の意味は不変でも、本文解決には必要な履歴の保持と現在のpolicyが必要である。
重要な版のtag保護とshallow / purge / eraseの違いは
[08-evidence-pointer-spec.md §2.4](08-evidence-pointer-spec.md#24-識別子の安定性と引用の保持) を参照する。

```bash
kio init
kio index --preview      # ローカル取り込み対象と送信policyを確認
kio index --offline --yes # ローカル取り込み + ベースライン index。HTTPを禁止
kio search "あの PDF"
kio open <検索結果の pointer>
```

# 0.1 ターゲットユーザー

```
- 大量の PDF・Markdown・コード・画像・研究資料を扱う
- 開発者・研究者・技術者
- Git や CLI に抵抗がない
- ローカルファイルが散らかっている
- AI 検索を試したいが、クラウド丸投げは嫌
```

# 0.2 二層構造 — truth と cache

```
truth = folder-local .kio                知識・scope・送信承認の正本: raw object / normalized / chunks / commits / refs
truth = device/central operational state 課金台帳・in-flight intent 等の非知識運用正本
cache = scope_registry                   探索対象一覧 / stale 検出
cache = aggregator                       全 scope の chunk (live + 過去) の read replica
                                         (横断検索の採点・候補選択 / 権限状態の横断投影)
```

`scope_registry` / `aggregator` のみで `.kio` の状態を変える実装は禁止。aggregator は安全性判定の
最終権限を持たず、結果を返す scope は live `.kio` で再確認する。詳細 [03-data-model.md §4](03-data-model.md) /
[05-runtime.md §1.8](05-runtime.md)。

---

# 1. ドキュメント構成 と Reading Path

`docs/` 直下に実装スペック、製品要件、明示した設計提案を置く。`README.md` (本書) を最初に読み、
続いて `01-` から `13-` の順に読む。`11-` は製品要件、`12-` と `13-` は承認済み方針、
現行実装との対応、未実装の追加改善を区別して記録する。実装済みを3 OS受入・配布済みと同一視しない。

| 順 | ファイル | 役割 |
| --- | --- | --- |
| 0 | [README.md](README.md) | 全体俯瞰・Reading Path (本書) |
| **01** | [01-positioning.md](01-positioning.md) | **★最初に読む**。core 一文 / ターゲット / 差別化の核 / **Local・将来Cloudの競合分析 + Perkeep失敗分析** / 既存ワークフロー / 発言禁止リスト |
| **02** | [02-philosophy.md](02-philosophy.md) | 理念 (Evidence Pointer の根拠、Markdown 正規化の妥協点、忘れない vs purge) |
| **03** | [03-data-model.md](03-data-model.md) | **★契約**: CAS / `.kio` layout / object 種別 / identity / `tool_profile_hash` / 書き込み境界 / dedup スコープ |
| **04** | [04-pipeline.md](04-pipeline.md) | **★契約**: ingest → prepare → markdownize (incremental) → chunk → embed → index / SQLite schema / batch (retry / budget) |
| **05** | [05-runtime.md](05-runtime.md) | **★契約**: 検索 (paging / MMR / `--at`) / commit_type / GC / purge / restore / time-travel / 並行性 |
| **06** | [06-cli-spec.md](06-cli-spec.md) | CLI 全コマンド / exit code / error code namespace / JSON output / observability |
| **07** | [07-adapter-spec.md](07-adapter-spec.md) | Adapter trait (Prepare / Markdownize / Embedding / etc.) / 実行形態 / **incremental Markdownize プロンプト規約** |
| **08** | [08-evidence-pointer-spec.md](08-evidence-pointer-spec.md) | Evidence Pointer schema / 解決手順 / **Dead Pointer (purge) のセマンティクス** / exact-only retarget / 外部 Agent 相互運用 |
| **09** | [09-mvp-scope.md](09-mvp-scope.md) | MVP scope / RC platform support matrix / non-authorizing roadmap / Step 1-4 + 規模上限 / 北極星シナリオ / 凍結ゲート |
| **10** | [10-operations.md](10-operations.md) | 横断規約 (semver / 観測ログ / 命名リネーム表 / 初回スキャン承認 / Adapter セキュリティ) |
| **11** | [11-product-requirements.md](11-product-requirements.md) | **製品要件の正本**: v1 の到達要求、RCとの区別、v2/v3 の境界、検証要求 |
| **12** | [12-change-detection.md](12-change-detection.md) | **承認済み方針・実装対応**: OSイベント、差分走査、欠落復旧、子scope自動管理、後続の版別状態表示 |
| **13** | [13-linear-history.md](13-linear-history.md) | **承認済み方針・実装対応**: 線形履歴、管理対象復元、公開・回復、tag保持、後続の操作表示 |
01〜10 は実装・運用契約、11 は製品要件である。契約が RC の現状や提案を記録する場合、v1 の到達要求と実装済みを混同しない。旧統合要件ドラフトは current consumer がなく、旧 CLI/schema を残すだけだったため削除済みである。

## 1.1 設計検討メモ (撤去済み)

旧 `docs/research/` (LLM 出力由来の設計検討メモ + folder-history 独立設計書) は 2026-07-18 に docs から
撤去した — 実装・運用契約は `01-` 〜 `10-`、製品要件は `11-` を参照する。経緯を参照する場合は git 履歴 (撤去直前のコミット) を辿る。

## 1.2 非規範の戦略文書

[`strategy/`](../strategy/) は市場・事業・将来製品の意思決定材料を置く領域であり、`01-`〜`13-` のReading Pathや実装specには含めない。現在の将来Cloud仮説は [cloud-competitive-advantage.md](../strategy/cloud-competitive-advantage.md) を参照する。同文書の機能・roadmap・設計判断は、明示的にspecへ採用されるまで未承認である。

---

# 2. Phase Plan と Step 計画

詳細は [09-mvp-scope.md](09-mvp-scope.md)。

現在のv1実行工程は [v1完成計画](../tasks/v1-completion-plan-2026-09-08.md)、候補別の実装・検証は
[進捗記録](../tasks/v1-implementation-progress.md) を参照する。2026-10-03承認の
[保存・復元・引用保持の実装計画](../tasks/knowledge-ux-implementation-plan-2026-10-03.md) は、
既存v1受入を維持し、後続に版別状態表示、既存tagによる引用保持、復元操作表示を追加する。
直近の実行順序と最終候補の証跡は
[既存v1の受入完了と文書整合](../tasks/v1-acceptance-closeout-plan-2026-10-03.md) を参照する。
jj連携・引用bundle・文書lineageは着手条件付きの検討候補であり、追加機能はまだ実装済みではない。

```
Phase 1: Evidence 基盤    raw / normalized / chunk / Evidence Pointer
Phase 2: 検索             FTS5 / sqlite-vec / hybrid (paging / MMR)
Phase 3: 履歴             tree / commit / restore / --at / time-travel
```

Step 計画 (Phase 1-3 を実装):

```
Step 1 (1-2ヶ月): kio-core + kio-cli (init / status / snapshot create / log / diff / inspect / tag)
Step 2 (2-3ヶ月): kio-pipeline + kio-adapter (frontier AI default)
Step 3 (2-3ヶ月): kio-index + kio-search (hybrid + Evidence Pointer)
Step 4 (1.5-2ヶ月): restore + --at + time-travel + purge 最小形 (tombstone) + evidence verify
```

(Step の期間・内容の正本は [09-mvp-scope.md §3](09-mvp-scope.md) — 差分が生じた場合は 09 が正)

**コア規模上限** (ripgrep 以下): テスト除いて 11-16k LOC、テスト含めて 20-30k LOC。

---

# 3. 北極星シナリオ (Phase 3 完成時の Done 条件)

詳細は [09-mvp-scope.md §4](09-mvp-scope.md)。

```
M3-1: 「3ヶ月前に書いた結論の根拠 PDF を 5 秒以内に出す」
M3-2: 「リネーム済みファイルの過去版を含めて検索」
M3-3: 「削除したはずの資料から特定の数字を再発見」
```

実装中の機能追加は「3 シナリオのどれに resp するか」で判断する。

---

# 4. 設計上の宿題 (4 論点)

**status・期限の正本は [09-mvp-scope.md §5.4](09-mvp-scope.md)** — 本書には転記しない (転記は陳腐化して
「draft なら着手しない」規則を誤発動させた実績があるため、一覧・現在の status は必ず正本を見る)。

未確定 (draft) のままステップに到達したら **そのステップを着手しない** (該当判定も 09 §5.4 の status で行う)。

---

# 5. 設計判断の正本は spec に閉じる

ADR (Architecture Decision Records) フォルダは廃止しました。本プロジェクトでは:

- **実装・運用契約の正本は01〜10、製品要件は11**。12・13は承認済み方針、実装対応、後続改善を区別する。
- 「なぜそう決めたか」は spec の各セクション冒頭に短く埋め込む (例: `01-positioning.md §1.1`「なぜ local-first であって offline-first ではないか」)
- 設計検討メモ (旧 `docs/research/`) は 2026-07-18 に撤去済み — 経緯は git history で辿れる

将来、本当に「逆方向の判断もありえた」「外部から問われたら答える義務がある」決定が出てきた時点で、改めて `adr/` を作る方針です。Phase 1 着手前の今、ADR を運用するコストはメリットを上回らないと判断しています ([09-mvp-scope.md §6](09-mvp-scope.md))。

---

# 6. 編集規約

- **形式**: GitHub-flavored Markdown。
- **言語**: 日本語。固有名詞・コード片は原語のまま。
- **コードブロック**: 言語タグ必須 (`bash`, `toml`, `json`, `rust`, `sql`, `text`)。既存の列挙・図示 block の無タグ fence は `text` 扱い (新規追加時にタグ必須)。
- **相対リンク**: docs/ ルート相対。
- **スキーマ変更**: 03-data-model.md / 07-adapter-spec.md の変更は破壊的変更扱い。安定版前は旧 format を明示的に reject し、migration / alias / compatibility reader を要件にしない。利用者ファイルと knowledge を明示操作なしに破壊しない。
- **発言禁止フレーズ**:
  - ✗ "Git for knowledge" / "個人 AI アシスタント" / "OS 級" / "Knowledge Graph for personal data" / "Notion / Obsidian キラー"
  - ✗ "offline-first" (誤解を招く。"local-first" を使う — 禁止はプロダクトの呼称・訴求としての使用であり、否定・対比文での言及は可)
  - ✗ "private AI" / "機密 AI"
  - ✗ "データはあなたのマシンから出ない" (デフォルト構成では偽。「保管と主権はローカル」と言い換える)
- **採用する語**:
  - ✓ Local-first knowledge archive, powered by frontier AI. (core)
  - ✓ データはローカル、計算は最強の AI を使う。(core 日)
  - ✓ local-first / Evidence-grounded local knowledge archive
  - ✓ Evidence Pointer / time-travel knowledge navigation
- **凍結中の修正**: ドキュメント統合ゲート ([09-mvp-scope.md §6](09-mvp-scope.md)) 完了後の本文書き換えは、Step 1-4 で実装が物理的に不可能と判明した場合 / 外部 Agent 互換性を破壊する変更 / データ破壊リスク、の 3 ケースに限る (これに加え、[09-mvp-scope.md §4.2](09-mvp-scope.md) の一回限りの Q_hard 増補とその #5 追記の完遂手続きのみ凍結対象外)。それ以外の「綺麗にする」修正は Step 4 完了後。
