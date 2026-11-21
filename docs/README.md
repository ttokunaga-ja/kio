# Kio 製品仕様

01 から順に読む。01〜10 と 12〜13 は設計・実装・運用の契約、11 は v1 の製品要件を定める。

| 順 | ファイル | 役割 |
| --- | --- | --- |
| **01** | [01-positioning.md](01-positioning.md) | **★最初に読む**。core 一文 / ターゲット / 差別化の核 / **Local・将来Cloudの競合分析 + Perkeep失敗分析** / 既存ワークフロー / 発言禁止リスト |
| **02** | [02-philosophy.md](02-philosophy.md) | 理念 (Evidence Pointer の根拠、Markdown 正規化の妥協点、忘れない vs purge) |
| **03** | [03-data-model.md](03-data-model.md) | **★契約**: CAS / `.kio` layout / object 種別 / identity / `tool_profile_hash` / 書き込み境界 / dedup スコープ |
| **04** | [04-pipeline.md](04-pipeline.md) | **★契約**: ingest → prepare → markdownize (incremental) → chunk → embed → index / SQLite schema / batch (retry / budget) |
| **05** | [05-runtime.md](05-runtime.md) | **★契約**: 検索 (paging / MMR / `--at`) / commit_type / GC / purge / restore / time-travel / 並行性 |
| **06** | [06-cli-spec.md](06-cli-spec.md) | CLI 全コマンド / exit code / error code namespace / JSON output / observability |
| **07** | [07-adapter-spec.md](07-adapter-spec.md) | Adapter trait (Prepare / Markdownize / Embedding / etc.) / 実行形態 / **incremental Markdownize プロンプト規約** |
| **08** | [08-evidence-pointer-spec.md](08-evidence-pointer-spec.md) | Evidence Pointer schema / 解決手順 / **Dead Pointer (purge) のセマンティクス** / exact-only retarget / 外部 Agent 相互運用 |
| **09** | [09-mvp-scope.md](09-mvp-scope.md) | MVP の範囲、コード規模と構造の上限、北極星シナリオと評価規約 |
| **10** | [10-operations.md](10-operations.md) | 管理とストレージ保護 / 台帳とバックアップ / 設定 / 観測ログ / 導入 |
| **11** | [11-product-requirements.md](11-product-requirements.md) | **製品要件の正本**: v1 の到達要求、RCとの区別、v2/v3 の境界、検証要求 |
| **12** | [12-change-detection.md](12-change-detection.md) | **承認済み方針・実装対応**: OSイベント、差分走査、欠落復旧、子scope自動管理、後続の版別状態表示 |
| **13** | [13-linear-history.md](13-linear-history.md) | **承認済み方針・実装対応**: 線形履歴、管理対象復元、公開・回復、tag保持、後続の操作表示 |

利用者の決定と背景は [decisions.md](decisions.md) に記録する。
[Cloud の競争仮説](proposals/cloud-competitive-advantage.md) は将来製品の提案で、仕様への採用には明示的な決定が必要である。
