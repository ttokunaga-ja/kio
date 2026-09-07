# Kio 製品完成状況と設計監査 — 2026-09-07

対象ソース: `1df10f248b73910349f80f45c30f801b5bfef5f5` (`v0.1.0-rc.3`)。
本調査の文書変更前の immutable worktree を監査対象にした。古い RC.1 の状態を現状と混同しない。
現時点で v1.0 完成とは判定しない。検索・保存の主要部分は実装済みだが、自動管理、境界の失効、
実 converter/provider の検証証跡に残作業がある。実装量から恣意的な完成率は算出しない。

## 要件ごとの現状

| 要件 | 判定 | 根拠・残作業 |
|---|---|---|
| CLI text/vector/hybrid、画像検索 | 実装済み・契約テストあり | `main.rs:675-735`、`aggregator.rs:2580-2710`。実 provider と品質評価の完了を意味しない |
| `.kio` 正本、scope SQLite、中央 replica、一括検索・再構築 | 主要経路あり | `docs/03-data-model.md`、`main.rs:3976-3980`。中央検索は aggregator を使用する |
| LLM Markdown、PDF/Office 前処理 | 組込み経路あり・実環境の証拠不足 | Mistral OCR、Gemini embedding。DOCX/PPTX は soffice→PDF、XLSX は直接 Markdown |
| 任意の外部 LLM 接続 | 未完成 | 組込み provider と local adapter が中心。任意 cmd/args dispatcher は未提供。対応 provider を限定して明示する必要がある |
| v1 の権限 | 主要制御あり・修正が必要 | scope×adapter×profile の送信承認、revoke、secret hold。複数利用者 ACL は v3 のため不足として数えない |
| 新規フォルダの自動 `.kio` | 部分実装 | 手動 parent index 時、macOS/Linux のファイルがある子のみ。空フォルダ、OS watcher、Windows mutation は未達 |
| 3 OS Actions | 現行候補の通常 CI は成功 | 実 Office の skip と実 provider 経路の証拠不足は通常 CI 成功で埋まらない |
| PersonaScope/personaCorpus 評価 | v1 の阻害要件ではない | 機能・セキュリティ・復旧試験とは区別する |
| v2 GUI / v3 共有・共同編集 | 将来要件 | GUI 全操作の engine は CLI と共有する。多利用者の認可は v3 |

更新した要件の正本: [docs/11-product-requirements.md](../docs/11-product-requirements.md)。

## リリース前に解消する問題

1. **自動管理の不完全さ。** Windows は安全な retained-handle child mutation が未実装で明示的に拒否する
   (`scan.rs:527-534`)。Unix でも direct includable file の存在を登録条件にするため空フォルダは対象外
   (`scan.rs:693-715`)。新規子に対する local enrollment と外部送信の承認を分ける必要がある。
   親の承認後に作った子には `--approve` の再指定が必要になる現行動作を、継続的な root 管理契約に整理する。
2. **Ignore の失効伝播。** 既存の子を親から ignore した場合、新規 discovery の skip だけで十分とはいえない。
   子の検索可視性、生成済みの親 policy、保留タスクも現行 policy に収束させる必要がある。
   個別のセキュリティ証拠と成立条件は非公開の Codex Security scan に保持した。
3. **ローカル adapter の信頼境界。** loopback であることと、期待するサービスであることは別の性質である。
   offline と呼ぶモードにも受信 peer の検証が必要。個別の証拠は同 scan に保持した。
4. **preview の副作用。** `index --preview` でも `repo.lock_store()` を通り、`.kio/.lock` を作成する
   (`main.rs:2213-2231`、`scope.rs:5917-5967`)。no-write 契約と整合させ、必要な一貫性は読取 snapshot 等で得る。
5. **検索 selector の曖昧さ。** `--all-scopes` を `--scope` / `--descendants` と併用しても黙って無視する
   (`main.rs:12015-12039`)。排他条件を CLI parser で強制する。
6. **scope 境界の戦略。** symlink を拒否しても mount 越境は別問題である。自動登録が未登録 volume に
   `.kio` を作らないよう、root identity と mount policy を明示する。検知は許可を拡張しない。
7. **履歴公開の整合性。** 現行形式は多親 DAG を許し、`HEAD` と `refs/heads/main` の二重公開に crash gap が
   ある (`scope.rs:2380`)。線形 schema と単一 HEAD を先に決める。
8. **小さいが独立した表示不整合。** 検索の budget month を ledger snapshot 取得前に捕捉するため、
   UTC 月境界をまたぐ取得では snapshot 時点とのずれが起こり得る (`main.rs:5876-5883`)。
   month と snapshot の取得基準を揃える。予算 enforcement 自体の迂回とは判定していない。

既存の scope/profile gate、redirect 禁止、同一 content hash を越えた scope binding、no-follow filesystem
操作には具体的な防御が確認できた。device-wide `allow_network=true` だけで送信できるという候補は、
下流の persistent scope gate を確認して棄却した。現状を一律に危険と評価する根拠はない。

## CLI の整理案

`search --mode auto|text|vector|hybrid` は適切な enum である。問題は権限とスコープの意味が混在する部分にある。

| 概念 | 推奨する契約 |
|---|---|
| local enrollment | 登録した root 配下を継続管理する許可。子の新規作成ごとに同じ承認を再要求しない |
| persistent egress consent | scope / adapter / destination / execution profile に結び付く独立した approve/revoke 操作 |
| `--online` / `--offline` | 当該実行の通信意図。永続承認とは区別し、各コマンドで同じ意味にする |
| `--yes` | 非対話の確認。既存の認可、secret hold、予算、競合検証を省く旗にしない |
| `--send-secrets` | 通常送信と分けた、対象と範囲が見える操作に保つ |
| search scope selection | all と単一 scope / descendants は排他。default と明示指定の意味を help に出す |
| export / managed restore | 別ディレクトリへの書出しと、最新版を更新する transaction を分ける |

現在の `index --approve` は scan/network の複数意味を持つ。pre-stable の段階で分離し、旧旗の alias を残さない。
service から CLI subprocess を繰り返し起動することを中核設計にせず、共通 Rust engine と typed request を
CLI と今後の GUI が呼ぶ構造にする。preview、apply、index、watch が同じ boundary/policy 判定を使う。

## 変更検知と線形復元の結論

OS イベントから変更位置を絞る方向は妥当である。フォルダ size/mtime は補助情報に限定し、
イベント駆動の差分走査を基本に、欠落・停止・起動時と定期照合で回復する。
既存 `snapshot auto` は一つの scope の直下だけを走査し、子の discovery も watcher も行わない。
設計と 3 OS 受入ケースは [12-change-detection.md](../docs/12-change-detection.md) にまとめた。

復元は、過去の状態を現在の HEAD の唯一の子として追加する方式を推奨する。全体復元と選択パス復元は
両方提供でき、元 commit は provenance として記録する。後続履歴を消さない。
線形 schema、raw と現行 policy の分離、journal、子 scope ごとの部分完了は
[13-linear-history.md](../docs/13-linear-history.md) にまとめた。

## CI の証跡

現行 SHA の [main CI](https://github.com/ttokunaga-ja/kio/actions/runs/34099743495) と
[tag CI](https://github.com/ttokunaga-ja/kio/actions/runs/34110009796) は成功。
[release run](https://github.com/ttokunaga-ja/kio/actions/runs/34100074839) も 3 OS の再現 build・archive 検証が成功している。

ただし `office_convert.rs:801-812,850-861` の実 soffice テストは renderer が見つからない場合に early return する。
ログの `ok` だけから実際の DOCX/PPTX→PDF 実行を証明できない。workflow に converter の install/pin/存在確認と、
version・入力 hash・出力 hash・実行結果を残す必須 lane が必要である。

Mistral/Gemini の実 client はあるが、Actions に実 provider 呼出しを証明する lane は確認できなかった。
mock と real を別の証跡として扱う。実 provider lane は信頼した release candidate、合成 fixture、保護された secret、
上限を持つ実行に限定し、認証不足・skip を pass にしない。通常 PR への secret 提供は行わない。
[GitHub Actions secrets](https://docs.github.com/en/actions/how-tos/write-workflows/choose-what-workflows-do/use-secrets)、
[Secure use](https://docs.github.com/en/actions/reference/security/secure-use)

CodeQL は成功しているが、現行 SHA の alert は 16 件 open だった。静的に triage した結果、
3 件は該当する cache sink なし、9 件はテスト内の非 credential 診断、4 件は Windows API の lifetime に
反証があり、今回 actionable な脆弱性には数えていない。GitHub 上の dismiss は行っていない。
Windows API の実環境確認は残るため、16 件の無条件な安全宣言もしない。

## 実施範囲

Standard `codex-security:security-scan` に Daybreak Blue の境界監査と独立 baseline/CodeQL triage を組み込んだ。
静的確認で medium の問題を 2 件記録した。巨大ファイルの一部と外部 converter の内部は未網羅であり、
repository 全行の完全監査や動的再現が終わったとは扱わない。
scan ID: `2a1355c7-462e-4b6e-8ee7-a43e0eea0253`。詳細はローカル Codex Security workspace に保持する。

今回は調査・設計・要件文書の更新である。product code の修正、実 provider 課金、Actions 再実行、push は行わない。
文書の整合・リンク・差分を検証する。既存 CI の成功と今回新たに実行した試験を混同しない。
