# v1 Actions coverage audit — 2026-09-08

利用者の指定に従い、Windows・Linux・macOS の必須機能受入を GitHub Actions 上で行う。
これは承認済み実装計画 §7 の方針を具体化する記録であり、workflow追加や受入成功の宣言ではない。
研究用 PersonaScope/personaCorpus 性能評価は v1 の必須gateにしない。

## 現在の実行結果

GitHub API の `commits/main`、workflow一覧、直近runと各jobを確認した。

| 対象 | 確認結果 |
|---|---|
| GitHub main | `1df10f248b73910349f80f45c30f801b5bfef5f5`、RC.3 |
| ローカル HEAD | `d98e0f659123c2bb7c2959f1bba13ef49137fcfa`、mainはremoteより5 commit先。v1の統合実装には未commit差分もある |
| [CI 34110009796](https://github.com/ttokunaga-ja/kio/actions/runs/34110009796) | 2026-09-07、上記remote SHA。Linux `rust`、`macos-security-r23`、`windows-security-r23`、persona機能、synthetic-historyの5 jobs成功 |
| [Draft 34100074839](https://github.com/ttokunaga-ja/kio/actions/runs/34100074839) | 同じremote SHA。Linux/macOS/Windowsの3 jobsで再現archive、検証、展開binaryの基本smoke成功 |
| 現在のv1実装に対するActions | 未実行。上記成功を流用して現在の実装が通ったとは扱わない |

ローカル既存ログも再読した。r24 CLIは43 targets/964 passed/0 failed、appは172 passed。
r25ではその後のcredential parser修正をadapter 323 passed、追加CLI 1 passed、関連4 packageの
all-targets Clippyで確認している。r24全体の成功はr25追加差分の全体再実行ではない。

## 既存workflowの範囲と不足

| 範囲 | 現在の実装 | v1受入への不足 |
|---|---|---|
| 通常3 OS回帰 | `ci.yml`は3 OSすべてでworkspace/all-targets/lockedのcargo test。Linuxでfmt/clippyと合成機能E2Eも実行 | 新しいapp/testを含む候補で再実行。OSごとの対象ケース欠落とearly returnを別途検出する |
| 実Office | adapterに実DOCX/PPTX→PDFテストがあり、macOSで局所確認済み | workflowにrenderer導入と `KIO_REAL_OFFICE=1` がない。変数なしではテストが早期returnしRustの成功数に含まれる |
| Office CLI契約 | `step4b_office_contract.rs`はfixture PDFを返すdebug seamを使用 | 実rendererの起動・sandbox・PDF出力を証明しない。実binaryからの変換を別に必須化する |
| 実provider/local | mockの成功・拒否・batch・予算・再送契約とlocal TLS認証テストが存在 | 実Mistral/Gemini/authenticated localを実行するprotected候補workflowとreceiptがない |
| watch service | `v1_watch_service.rs`はhelpの2テスト。実OS schedulerの実装は別にある | install/start/status/stop/restart/uninstallのnative lifecycleをrunner上で実行する必要がある |
| 配布binary | `draft-release.yml`はexact candidate SHA、locked graph、2回のbuild/archive一致、検証、smoke、artifact保存 | `release::smoke_candidate`の操作はversion/init/index/text search/openだけ。A02〜A11の受入は未接続 |
| Windows製品対応 | draft matrixはWindowsをexperimentalと表示 | 必須ケースを実証した候補でsupportedへ変更する。表示だけを先に変更しない |
| 最終判定 | 既存基本smoke receiptとartifactはある | A01〜A12×OS×実行laneの必須集合を照合する集約gateが未実装 |

確認箇所: `.github/workflows/ci.yml`、`.github/workflows/draft-release.yml`、
`crates/kio-adapter/src/office_convert.rs::required_real_office_converter`、
`crates/kio-cli/tests/step4b_office_contract.rs`、`crates/kio-cli/tests/v1_watch_service.rs`、
`crates/kio-eval/src/release.rs::smoke_candidate`。

## Actionsで完成させる受入構成

以下は追加・拡張する受入責任であり、実装済みworkflow名ではない。
既存workspace testを別jobで無目的に重複実行せず、実環境と配布物の不足を埋める。

| 責任 | 対象ケース | 実行と判定 |
|---|---|---|
| 通常native CI | A01〜A06、A09〜A10の契約、A08のmock障害 | 現行3 OS jobsを維持。承認・copy/move・Ignore失効・境界差替え・restore・queue復旧の回帰を接続する |
| 実変換 | A07 | 3 OSにrendererを用意しversion/実体identityを保存。実行必須フラグ、DOCX/PPTX/PDF/image/XLSXの構造・次段処理・拒否/timeoutを確認。変換環境不在はnon-pass |
| 利用者service | A03、A11 | runner上の使い捨てscopeと専用IDでnative schedulerへ登録。起動後の変更反映、停止中変更、再起動回復、解除と後処理を確認 |
| 実接続 | A04、A08 | 一つのprotected 3 OS matrix。実Mistral/Gemini/localの通常/batch/queryを小さな合成fixtureで確認。mockによる受付後障害試験とは別receiptにする |
| 配布物受入 | A01〜A12 | exact SHAから作ったarchiveを展開し、そのbinaryで共通受入runnerを実行。debug mock経路を実接続と数えない |
| 必須証跡の集約 | 全ケース | candidate SHA、binary/fixture hash、OS、tool/profile、lane、結果を照合。missing/skip/unpreparedはpassにせず、異なる候補の結果を混ぜない |

service laneはまずGitHub-hosted runnerでnative user session/schedulerを利用できるかを検査する。
必要なuser sessionが利用できないOSは、そのケースを通常process成功で代替しない。
必要なら隔離されたself-hosted Actions runnerを使う。現時点でその必要性は未判定であり、
利用者の実Macにserviceをインストールしなければならないと決まったわけではない。

端末側grantの検証もrunnerの新しい私有設定領域で行う。明示approve/revoke、read-only preview、
scopeだけ/端末状態だけの複製、root移動、profile/destination/trust変更、revoke中の送信競合を試験する。
`--yes`は指定操作の非対話確認にだけ使用し、送信先・予算・競合判定を迂回しない。
このために必要な専用approve・端末grant統合はM2の実装作業であり、現行Actionsを実行するだけでは補えない。

実provider laneには候補SHA、fixture/要求数上限、総予算と各jobへの割当、保護したcredentialが必要。
通常pushでは課金要求を起動しない。不明なprovider結果を自動再送せず、receiptは秘密を含まない。
認証情報や支出条件が未準備なら未受入として明示する。今回の「Actionsで試験」の指定を、
金額上限のないprovider課金やrelease公開の承認とは解釈しない。

## 実行順序

1. M2の専用承認CLI/端末grant、M3の残るdirectory境界/大規模探索、M5の復旧/trust/資源制限を実装する。
   fixtureとActions受入runnerの準備はこの間に進める。
2. coherentな実装単位で差分を確認し、ローカルで安全かつ再現可能なCI検査を通す。
   保存済みの部分試験だけでworkspace全体・release buildの成功とはしない。
3. テスト可能なcommitをpushし、通常3 OS Actionsでnative回帰を確認する。
   失敗時はjob全体とannotationsを読み、可能な限りローカル再現後に修正をまとめる。
4. 最終候補SHAで配布物・実変換・service・実接続を実行し、必須receiptを集約する。
   最終security検証と運用文書もその候補へ結び付けてM7を判定する。

この照合で変更したのは進捗文書と計画への参照のみ。workflow変更、push、dispatch、
実provider呼出し、service登録、release公開は行っていない。
