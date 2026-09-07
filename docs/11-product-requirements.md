# 11 Product Requirements

本書は Kio の製品要件の正本である。01〜10 の設計・RC・Phase・CLI 契約は、実装済みの範囲または
提案を記録し得る。製品要件との矛盾は本書を優先するが、未決の transaction、CLI、schema を
本書から導入してはならない。

## 1. v1 の到達要求

- canonical な管理領域名は `.kio`。`.kcs` は指定誤りであり、alias、migration、互換 reader は要件にしない。
- CLI は hybrid search、vector search、image search を提供する。LLM による Markdown/PDF 前処理と
  外部 Adapter を利用できる。特定 provider やモデルの同梱・利用可能性は別途実証する。
- 知識の正本はフォルダごとの `.kio` にある。各 `.kio/index/sqlite.db` と central replica は、
  per-folder CAS / metadata から再構築できる検索用 projection であり、SQLite 単体は knowledge の正本ではない。
- scope と外部送信の consent を機能させる。consent は scope × Adapter の egress gate であり、
  user/group ACL ではない。user/group ACL は v3 の要件である。
- ignore されない新規子フォルダを自動発見し、`.kio` を作成して独立 scope として管理する。
  空フォルダも対象に含む。exact debounce/SLA と、一般的でない filesystem の対応範囲は未決である。

変更検出は root からの native OS event を契機に reconciliation し、手動 `kio index` は同じ engine を使う。
起動・再接続・event overflow の後と定期 fallback では rescan し、取りこぼしから回復する。event stream は
最適化であって consent を与えず、directory の size/mtime も再帰的な完全性の証明にはならない。この方針は
2026-09-07 に承認済みであり、現時点で実装済みとは表明しない。詳細は [12-change-detection.md](12-change-detection.md)。

## 2. 正本と復旧

knowledge、scope 設定、送信承認は folder-local `.kio` に置く。device/central operational state は
課金台帳、in-flight intent、provider 回復・照合記録などの knowledge ではない正本を置ける。
この運用正本は再構築不能であり、`.kio` restore から reset せず、別途 backup と reconcile を行う。
現行の backup 手順は [10-operations.md §7.5.2](10-operations.md) にある。

## 3. 現状と検証

本書は v1 の充足を表明しない。RC.3 の child scope 自動 mutation は macOS/Linux の限定経路であり、
Windows の自動 child mutation は RC で未対応である。RC の platform matrix は [09-mvp-scope.md §1.2](09-mvp-scope.md)。

v1 の受入では macOS、Linux、Windows の 3 OS で機能的 CI を行い、実際の converter と provider 接続経路の
証拠を確認する。converter/provider を実行しない skip を pass と数えず、証拠がない経路を CI 成功だけで
充足と扱わない。

PersonaScope/personaCorpus による性能・品質評価は準備後に行い、v1 の nonblocking とする。これは機能、
security、recovery の受入を不要にするものではない。

## 4. バージョン境界と破壊的変更

v2 は GUI で CLI の全機能を提供し、履歴選択と最新版への復元を提供する。v3 は cloud sharing、
collaboration、複数利用者と user/group ACL を扱う。ブランチを持たず現在の HEAD の子として過去状態を復元する
方針は 2026-09-07 に承認済みである。transaction、CLI、schema の詳細は実装契約として別途固定する。
設計は [13-linear-history.md](13-linear-history.md)、工程と受入条件の提案は
[v1-implementation-plan.md](../tasks/v1-implementation-plan.md) に記録する。

安定版前は破壊的変更を許可する。後方互換性、alias、migration は要求しない。旧 format は明示的に
reject し、曖昧な読み替えをしない。ただし、breaking code は knowledge や利用者ファイルを破棄する
権限ではない。既存の user files と `.kio` 内の knowledge は明示的な操作なしに破壊しない。
