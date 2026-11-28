# v1 の計画

この文書が、kio の計画のただ一つの入口である。利用者の決定は [docs/decisions.md](../docs/decisions.md)、製品の仕様は `docs/01`〜`docs/13`、役割・モデル・停止の基準は [AGENTS.md](../AGENTS.md) が正本で、ここには写さない。作業が進んだら、この文書の「現状」と「残作業」を書き換える。経過の記録は commit に残し、ここには積まない。

## 1. 現状（2026-11-21）

v1 の実装と F3 の判定は完了した。F4（正式な受入）も 2026-11-28 に候補 `18eead30` で完了した（`v1-acceptance-verify.yml` の run 46146328760。詳細は [引き継ぎ](handoff.md) §0）。2026-11-21 に main の `2c270c5` でローカル検証と通常 CI の 3 OS の全 job が成功した（run 44786625755）。整理後の候補では、その SHA の通常 CI を改めて確認する。

## 2. 完了条件

二段に分ける（D-20261113-02）。

1. **実装完了**：v1 の実装がすべて終わり、main で 4.1 のローカル検証と通常の CI の 3 つの OS が成功している。
2. **正式な受入**：整理後の通常 CI が成功した main の commit を候補として固定し、51 件（3 つの OS のネイティブ 42 件、有料 provider 6 件、GPU 経由のローカル 3 件）と最終の集約を一度だけ回す。51 件は減らさない。

実装完了までは、正式な受入の workflow（`v1-acceptance*.yml`、`v1-provider-acceptance.yml`、`v1-local-acceptance.yml`）を回さない。

## 3. 完了済みの作業と残る受入

- **検証基盤（V1・V2・WP）**。テストの保存先を隔離し、最適化した test profile で Linux の全テストを短縮した。Windows の `step3_p0_contract` は reconciliation に移した。
- **制約緩和（W1）と統合**。2026-10-22 承認の 62 件のうち 9 件は先行実装済み。残り 53 件から優先度「高」の 12 件を v1 に取り込んだ（D-20261113-09）。判断の根拠は [制約一覧](post-v1/constraint-backlog.tsv) と実装当時の計画・静的見積もり（`git show 1b46cbb5:tasks/v1-plan.md`、`git show 1b46cbb5:tasks/w1-high-estimate.md`）に残る。Office の隔離初期化（OM）と統合時の退行修正も完了した。
- **利用者の追加決定**。ソースコードの対象外化と再索引時の掃除（NC・NC2）、working tree 走査の有界化（S2b）、persona 用経路の廃止（PX）、派生差分・論理名と物理名・HTTP embedding 未承認時の仕様整合（H1・H2）を反映した。
- **ローカル embedding（LW1・G0〜G3）**。HTTP 接続を汎用化し、EG2 を既定にした。LiteRT-LM v0.19.1 の C API 1.0.0 を固定 URL とハッシュで取得する（D-20261120-01・02）。
- **仕上げ（F1〜F3）**。コード規模上限と CI ゲートを確定し、旧統合ドラフトの必要な契約を製品仕様へ統合した。F3 の検証結果は §1 を参照する。

**F4** は整理の全単位を統合してから [v1 正式受入](v1-acceptance.md) の手順で一度だけ実行する。v1 の範囲を追加する場合は利用者の決定を要する（D-20261113-10）。それ以外の追加は §6 に記録する。

## 4. 検証の回し方

### 4.1 ローカル

現在の環境で [ローカル検証スクリプト](../scripts/ci/local-checks.sh) を使う。LiteRT ZIP の準備と OS ごとの検証範囲は [引き継ぎ](handoff.md) §2〜3 を参照する。作業中は影響範囲のテスト、統合前は通常 CI 相当の検証を行う。Linux の全テストは統括がサンドボックス外で実行する。

### 4.2 CI

push すると通常の CI が動き、全体で約 85 分かかる（Windows の remaining の job が最も長い）。Linux の全テストの job は約 20 分で、制限は 45 分である。push は、ローカル検証が通った区切りでまとめて行う。今の作業環境以外の OS（Mac で作業中なら Linux と Windows、WSL で作業中なら macOS と Windows）の結果はここでしか分からないので、OS 固有の変更は小さい単位で push する。

### 4.3 高いコストの検証

正式受入は [v1 正式受入](v1-acceptance.md) を参照する。Full 規模の性能計測、比較用の GPU・有料 provider の計測は [手動検証](manual-full-cold-gates.md) を参照する。

## 5. 現在の環境だけでは回せないもの

- 作業環境以外の OS のネイティブ動作は、通常 CI で確かめる。
- 有料 provider の 6 件と GPU 経由の 3 件：正式な受入のときに、外部の資源（provider の鍵と予算、Tailscale、SSH、GPU の host）の準備を確かめてから回す。過去の設定の記録から、現在使えるとは判断しない。

## 6. v1 後

- 文書単位の検索結果（D-20261113-01、[post-v1/search-result-presentation.md](post-v1/search-result-presentation.md)）。
- 版ごとの状態の表示、tag による引用の保持、復元の操作の表示（D-20261003-01、[post-v1/knowledge-ux.md](post-v1/knowledge-ux.md)）。
- 制約の緩和の中・低の 41 件（D-20261113-09。[post-v1/constraint-backlog.tsv](post-v1/constraint-backlog.tsv) の「今回の整理」が「残件候補」で、優先度が「中」「低」の行）。
- 品質・性能の評価。評価用の corpus の原本は `~/archive/mac-20261112/home/` の `kio-dogfood/`（`corpus-v1` と最新の fixture）と `kio-baseline-corpus*` にある。使うときは repo の `scratch/validate/<作業名>/` に写し、`kio-eval benchmark` の `--fixture-root`・`--baseline-corpus` で指定する。v1 のテストと受入はこれらを使わない（D-20261113-11）。
