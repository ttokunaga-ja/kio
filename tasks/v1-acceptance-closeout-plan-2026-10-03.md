# Kio — 既存v1の受入完了と文書整合

作成日: 2026-10-03 JST。承認済み [保存・復元・引用保持の計画](knowledge-ux-implementation-plan-2026-10-03.md)
のK0〜K2を具体化する。既存M0〜M7・A01〜A12の要件と受入条件を維持する。
本書は実行計画であり、以下の製品変更・試験・workflow実行の完了記録ではない。

2026-10-04に、追加診断と修正→同一候補の3 OS全回帰・配布物検証→専用CI経路・
正式受入の詳細実行計画が承認され、実行を再開した。再開時の製品基準はlocal main `452adbc` と
未commitの49 sourceである。`0d560839`のAGENTS.md追加を保持し、後続の是正をmainへ統合した。
Windowsの既知の失敗とAppContainer起動問題に対応する局所回帰、GC全26件、全workspace
strict Clippyは成功した。監視testの出力回収を是正した固定候補`3d4fa21d`では、3 OSのfmt・
strict Clippy・全workspace/all-targets test・release buildとLinux配布物の検証が成功した。
追加のLinux A08で受入fixtureの中間directory権限の不備が見つかり、製品の安全性検証を維持して
fixtureを是正した。関連する単体試験とstrict Clippyも成功した。修正後の候補`03a2bcdc`では
Windows・WSLの全体回帰が成功したが、Linux配布物のsmoke fixtureにも権限の不備が見つかった。
製品の拒否規則を維持し、試験ツール自身が新規作成するdirectoryを安全に作る修正を局所検証した。
Windowsの関連7件、WSLの関連13件・全library485件・strict Clippy・fmtが成功した。
検証環境の権限不備と準備手順の失敗は保持し、安全な新規checkoutで同一sourceを検証し直した。
この局所是正をmainへ統合し、新候補で3 OSの必須証拠を揃える。
Macの空き容量は回復し、旧候補のfmt・strict Clippyは成功した。新しい修正が必要となったため、
Macの全体testは中断し、修正後の候補SHA/treeで3 OSの必須証拠を揃え直す。
正式受入の完了とは分け、最新状態と証拠は実行記録§22を
正本とする。
専用Windows/WSL host変更・条件付き復旧も今回の承認に含まれるが、旧候補向けbundleを流用せず、
最終候補に合わせて再生成・検証してから適用する。工程C4〜C6の51件と公開の別判定は維持する。

2026-10-03に実行開始した。[実行記録](v1-closeout-execution-2026-10-03.md) にC0/C1/C2の
証拠と残差を記録する。実行中に発見したUnicode tag規則の差について、利用者は修正と保存形式更新も
今回へ含めると追加承認した。C1の「保存形式を増やさない」はtag削除単体の方針であり、この追加是正は
保存形式3.0.0への更新と旧形式拒否の検証を含む。旧storeの移行機能は追加しない。

## 1. 今回の完成地点

既存v1の必須機能欠落を解消し、最終候補のローカル回帰・security検証・配布物と
macOS/Linux/Windowsの必須Actions receiptを揃える。固定matrixの最終verifier成功までを
「実装・自動受入完了」とし、実端末の手動確認とrelease公開は別の状態として報告する。

後続の版別状態表示、引用保持UX、復元操作ID、引用bundle、文書lineage、jj連携をこの候補へ混ぜない。
GUIはv2、cloud・共同編集・user/group ACLはv3、personaCorpusの研究性能評価はv1のnonblockingを維持する。

## 2. 調査基準と現在の残差

2026-10-03読取り確認時のローカルmainは `8337c2c1`（直前の製品変更は `2fe259ee`）。
tracked/staged差分はなく、既存の未追跡WIPは残っている。GitHub mainは `1ffdd0ca`、
最新配布版はRC.3 / `1df10f2`。これらを同じ候補として扱わない。
[CI 36287581490](https://github.com/ttokunaga-ja/kio/actions/runs/36287581490) の失敗は残り、
最新CodeQL成功も別SHAの製品変更や必須受入を証明しない。

| 残差 | 今回の処置 | 必要な証拠 |
|---|---|---|
| `tag --delete` が仕様にありCLI未接続 | 既存契約どおりcore→app→CLIと試験を完成 | 作成/削除/同名再作成、監査名台帳保持、HEAD・CAS・他tag不変、GCへの反映 |
| 最終Windows保存・GC/checkpoint回復 | 既存実装を最終候補でnative検証し、再現した問題を是正 | Windows HANDLE/SQLite、journal境界、実GC、snapshot、回復のruntime証拠 |
| orphan prune / purge後再取り込み | 最終差分の全回帰とsecurity検証 | purge→re-ingest→search→prune→search、shallow履歴と残存破損の拒否 |
| Linux confinement / Office、macOS SDK、Windows lint | ローカル修正を統合し既存CI失敗と対応付ける | 各OSの実環境、実converter、資源制限・cleanupとworkspaceの成功 |
| 配布物が以前の候補のまま | 最終SHAから3 OS archiveを2回生成し検証 | 一致archive、lock/SBOM、展開binaryと候補に結び付いたreceipt |
| 実service・provider/local経路 | 実行環境と外部設定を早期に準備し、候補確定後に実行 | native scheduler、Mistral/Gemini/authenticated localの3 OS証拠 |
| 検証記録と仕様の年代差 | 現行/歴史/未実装/未受入を明示し、文書参照を整理 | 要件→実装→試験→lane→receipt対応表と候補別完成表 |

既存の [進捗記録](v1-implementation-progress.md)、[Windows GC契約](v1-windows-gc-recovery.md)、
[orphan prune契約](v1-orphan-prune-recovery.md) は調査の根拠であり、過去の部分成功を最終候補へ流用しない。
private route・credential・課金台帳の設定は記録上の状態に留まり、この計画作成ではlive runtimeを再検証していない。

## 3. 実行工程

| 工程 | 作業・成果物 | 終了条件 |
|---|---|---|
| C0 基準と対応表を固定 | HEAD、既存WIPの一覧/fingerprint、要件→実装→試験→laneの残差表。既存CI失敗のログ/annotationsと修正commitを照合 | 他作業を保護でき、必須ケース・環境・未接続経路を明示 |
| C1 既存機能と試験を閉じる | tag削除を完成。保存/回復/prune/confinement等の最終差分を統合し、残差表で見つかった既存要件だけを是正 | 関係する回帰がpass、既知の必須機能欠落がなく、CLI/spec/testの対応がある |
| C2 環境と権限を準備 | 3 OS runtime/Office/service preflight、private local route、provider ledger/credential/予算の準備と確認 | 各laneの実行可能性と承認根拠が明示。未準備をskip/pass扱いしない |
| C3 ローカル統合と文書を確定 | Rust/lock/flagsを揃えた全回帰、release build、合成CI経路、2回packageと実binary受入。最終差分security検証、操作/制約文書を閉じる | ローカルで再現できる失敗なし。実行できないnative経路と理由を明記 |
| C4 候補SHAを固定して通常CI・native受入 | 承認範囲を確認したpush後、通常CI成功。既存native workflowでarchiveとOffice/serviceを含む3 OS受入 | 同じSHAのCIとnative workflowが成功、42件のnative subset receiptが揃う |
| C5 実provider/local受入 | 成功native runのarchiveを使い、Mistral/Geminiの6laneとauthenticated localの3laneを実行 | 正確な候補・予算・service identityで9件の追加receiptがpass、結果不明要求を重複送信しない |
| C6 最終集約と完成報告 | CI/native/provider/localのrun IDを最終verifierへ渡し、固定matrixとartifactを検証 | 51件の必須receiptと全source runが合格。未解決security・必須機能欠落なし。手動確認票と公開状態を別記 |

主経路はC0→C1→C3→C4→C5→C6。C2はC0から開始し、C1/C3と並行する。
後になってprivate route未準備が全体を止めないよう、C2のread-only preflightと具体的な設定差分の準備を先行する。
実装と環境準備は所有領域を分ける。3 OS試験は独立して実行できるが、1台のGPUを使うlocal laneは既存leaseに従い直列化する。

## 4. 最初の実装単位: tag削除

現行契約は [docs/03 §2](../docs/03-data-model.md) と [docs/06](../docs/06-cli-spec.md) にある。
tag削除自体は新しい保存形式を設けず、scopeの既存lock/retained directoryからcanonical tag refだけを削除し、
`names.jsonl`の監査行は残す。HEAD、commit/tree/raw/chunk、他のrefを削除しない。
追加承認されたUnicode是正は別の保存契約変更として、保存形式3.0.0と旧形式拒否を適用する。

coreが名前・対象ref・lock・削除境界を検証し、appが現在のscopeと実行結果を扱い、CLIが
`tag --delete <name>`を接続する。作成時のcommit指定と削除指定は排他とし、予約名・portable名・
Unicode正規化/衝突規則を既存処理と共通化する。存在しないtagの終了コード/JSONは既存error体系と
照合してdocs/06に固定し、削除成功時の監査とno-opの意味を曖昧にしない。

試験は正常な作成→削除→同名再作成、他tag/HEAD/正本bytes保持、監査名台帳、lock競合、対象差替えの拒否、
削除中断の整合、tag解除後のGC候補への反映を含める。既存core/CLI回帰に追加し、A09の保持/GCと
A10の中断回復へ対応付ける。新しいpinや引用登録機能は追加しない。

## 5. ローカル検証と候補の固定

Rustは `rust-toolchain.toml` と同じ1.98.0、Cargo.lockと `--locked`、各workflowの順序/flagsを使う。
まず変更箇所の回帰を通し、次に安全に実行できる通常CIの全ローカル相当を通す。

```bash
cargo +1.98.0 fmt --all -- --check
cargo +1.98.0 clippy --workspace --all-targets --locked -- -D warnings
cargo +1.98.0 test --workspace --all-targets --locked
cargo +1.98.0 build --release --workspace --locked
```

上記に加えて `.github/workflows/ci.yml` のshell/controller構文、action pin、Persona W0、
synthetic-history等の既存コマンドを同じ順序で実行する。これらの合成功能gateは維持するが、
personaCorpusのFull研究性能評価を追加待ち条件にしない。package/evaluatorは候補所有の実装を使い、
別SHAのverifier、source overlay、stale Cargo targetによる混合証跡を成功に数えない。

Linuxは既存の隔離WSL/実行環境でconfinement/Officeを確認し、hosted Ubuntuとの差を記録する。
Windowsは既存Smart App Control拒否を迂回せず、ローカルmetadata/cross compileはruntimeと別記する。
macOSのCI専用SDK準備やLinux provisioning scriptを利用者端末へそのまま適用しない。
成立しないnative経路はActionsで確認する。環境preflightを実Office/serviceの受入成功と扱わない。

仕様/操作文書と受入fixtureをC3までに整合し、最後にcandidate SHAを固定する。
C4〜C6中はmainに別の変更を積まない。修正が必要なら新SHAを作り、関係するローカル検証を経て
同一候補の必須receiptを取り直す。課金laneは通常CI・native/packageが成功してから実行する。
完成報告はまずrepo外の証跡に保存し、報告文書を後でcommitする場合も受入対象SHAを明記する。

## 6. 既存workflowと最終51件matrix

matrixの正本は `crates/kio-eval/src/acceptance.rs::fixed_v1_requirements()`。
2026-10-03の実装では各OS17件、3 OSで51件であり、単なるA01〜A12×3ではない。
各receiptの内部assertion数と、この必須receipt数を区別する。

| 実行 | 必須証拠 | 件数 |
|---|---|---|
| `ci.yml` | 同じ候補mainのpush run、全必須job成功 | receipt matrixと別に必須 |
| `v1-acceptance.yml` | native core A01〜A06/A09/A10、A04-local-trust、A03/A11-service、A07-real、A08-mock、A12-distribution | 14×3 OS = 42 |
| `v1-provider-acceptance.yml` | A08-Mistral/Gemini、成功native archiveと候補の一致、実provider/予算台帳 | 2×3 OS = 6 |
| `v1-local-acceptance.yml` | A08-authenticated-local、実OCR/embedding、service/model/checkpoint identity | 1×3 OS = 3 |
| `v1-acceptance-verify.yml` | CI/native/provider/localの4 run IDを検証し、固定matrixを組立てて全receiptを照合 | 51件全部 |

native A08-mockとA10は、展開release binaryに加えて別hashのdebug contract binaryを結び付ける。
実providerや実Officeをdebug seamで代替しない。source runのevent/path/main SHA/conclusion/attempt、
候補・binary/archive/lock/fixture/workflow digest、OS/arch、必要なservice identityを照合する。
missing/skip/cancel/failure、別候補・別workflow、自己申告だけのpassを拒否する。
実行済み結果から期待集合を減らさず、51件すべての固定要件を満たす。

最終workflowは候補と4 run IDを入力として、候補自身の `kio-eval acceptance assemble-expected` と
`kio-eval acceptance verify` を実行する。native workflow単体の緑、CodeQLの緑、ローカルreceiptだけで
v1完了を宣言しない。

## 7. 外部前提の扱い

private local routeは [具体的な設定案](v1-private-actions-route.md) を使う。Tailscaleの既存経路への
影響、OIDC、Windows/WSLの専用account/key/SSH、Environment設定をliveで再確認し、既存の承認済み
設定と今回必要な新規差分を区別する。新規設定の必要性を検証可能なpacketへまとめ、必要な承認を得た後に
適用・接続/拒否/cleanupを確認する。利用者の端末を無断でself-hosted runner化しない。

providerは [既存budget契約](../scripts/v1-provider-budget/README.md) とlive ledgerを照合する。
記録上は各provider累積USD 10、現在の1lane USD 0.10、3 OSで各USD 0.30のreservationである。
過去失敗・結果不明も累積に含め、campaignやworkflow変更で台帳をresetしない。
App/Environmentの設定完了記録と、初期化・予約・実呼出のruntime証拠は別に確認する。
既存の支出承認根拠を確認し、確認済みの承認を重複要求しない。新規・増額・権限変更が必要なら
その具体的差分を提示する。本書の計画作成ではpush、dispatch、設定変更、課金、GPU起動を行わない。

## 8. 文書整合と完了報告

文書整合は前commit `8337c2c1`で一部完了しており、watch/restoreを新規実装と扱う説明は修正済み。
次の更新は製品変更と同じ論理単位に含める。

- docs/03・06: tag削除の実装/CLI/error/監査契約。未接続注記は実装後に実証範囲を示して更新する。
- docs/08・13: tagの通常GC保護、shallow/purge/erase、復元provenanceと引用保持の違いを維持する。
- README・docs/README・09・10・11: 配布版/開発版、現行保存形式3.0.0と旧2.0.0拒否、製品v1、3 OS対応、実送信/復旧/制約を整合する。
  RC.3のhistorical platform matrixは保存し、新候補のsupported表記はnative証拠の後に確定する。
- 実装進捗・Actions coverage: 作成当時の記録を残し、現在の対応表とreceiptを追記/参照する。
  以前の全回帰/配布物/security成功を最終候補の結果へ読み替えない。
- 操作確認: offline fresh利用、approve/revoke、watch/service、tag、restore/export、ledger backup/recovery、
  GC/purge/repairのhelp・JSON・終了コードと文書を実binaryで照合する。

完成報告にはcandidate SHA、各OSのarchive/binary hash、4 source runとfinal verifier URL、
51件対応表、最終security検証範囲/未解決件数、実行できなかった確認を記載する。
実端末の対話操作、実際のsleep/wake・ログイン/物理再起動・利用者実データは別の手動確認票に残す。
手動確認未完了を自動受入の失敗と混ぜず、全体の製品受入済みとも表明しない。release公開の有無も別記する。

## 9. 担当と実行開始順

主担当Astraは要件、共通contract、保存/authority境界、統合と最終判定を持つ。
Sol workerはtag lifecycle、必要な保存/回復修正、eval/workflowと文書を所有範囲で分ける。
Lunaはbounded探索と確定済み検証コマンド、Sol reviewerは機能/回帰を担当する。
最終差分のsecurity検証は既存計画の指定方法・モデルを維持し、要求モデルと確認できた実体/範囲を分けて記録する。

着手順はC0の残差表とC2のread-only環境確認、続いてC1のtag削除実装/回帰。
一つのlogical milestoneごとにstaged diffを確認してmainへlocal commitする。
全ローカル可能gateが通るまではpushをdebugループに使わず、外部実行は確認済みの承認範囲で行う。
所要日数はC0/C2の結果で見積もる。環境待ち・課金結果不明・機能不具合の3種類を区別して進捗を報告する。
