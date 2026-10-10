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
この局所是正を統合した候補`c763a037`では、3 OSの全体回帰と独立2回の配布物検証が成功した。
同じ候補の通常CIとnative受入を開始したところ、Linuxの資格情報診断、macOSのSHM回転fixture、
native serviceの重複操作で失敗が見つかった。受入の拒否条件を維持して是正し、次の同一候補で
3 OSの必須証拠を揃え直す。正式受入の完了とは分け、最新状態と証拠は実行記録§23を正本とする。
Macの最低空き容量は利用者の追加指示により10 GiBとし、各実行前と実行中に確認する。
利用者の不要データ整理と、作業・証拠・配布物の保持は別に扱う。
専用Windows/WSL host変更・条件付き復旧も今回の承認に含まれるが、旧候補向けbundleを流用せず、
最終候補に合わせて再生成・検証してから適用する。工程C4〜C6の51件と公開の別判定は維持する。

2026-10-03に実行開始した。[実行記録](v1-closeout-execution-2026-10-03.md) にC0/C1/C2の
証拠と残差を記録する。実行中に発見したUnicode tag規則の差について、利用者は修正と保存形式更新も
今回へ含めると追加承認した。C1の「保存形式を増やさない」はtag削除単体の方針であり、この追加是正は
保存形式3.0.0への更新と旧形式拒否の検証を含む。旧storeの移行機能は追加しない。

2026-10-06に、利用者は構造是正と文書削減をv1へ含めることを承認した。製品コードを広く変更するため、
最終候補の固定（C3）より前に構造是正（R工程）を行い、文書削減（D工程）を並行する。工程と見直しの結果は
§10・§11。S7候補 `ad5f28e7` はR工程の後の最終候補にならないため、配布物検証・hosted CI・native受入へ進めない。
利用者が準備中の検証用データによる実データ確認（V工程）は並行し、v1の受入条件にはしない。

## 1. 今回の完成地点

既存v1の必須機能欠落を解消し、最終候補のローカル回帰・security検証・配布物と
macOS/Linux/Windowsの必須Actions receiptを揃える。固定matrixの最終verifier成功までを
「実装・自動受入完了」とし、実端末の手動確認とrelease公開は別の状態として報告する。
最終候補は、[09 §3](../docs/09-mvp-scope.md) の規模・構造の上限とOS分離規則を計測ツールで満たし、
kio-appが型付きのrequest/responseを公開し、D工程を終えた状態で固定する（§10）。

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
| R0〜R5 構造是正 | OS固有層の分離、kio-appの分解と型付きAPI、大きなファイルの分割、計測ツールとCI gate（§10） | 09 §3の上限・分離規則を満たし、CLI契約試験を変更せず3 OSの全体回帰が成功 |
| D 文書削減 | docs/README §6の基準で契約書から実装の機構・転記・経緯を除き、docs/14を各契約書へ統合（§10） | 参照切れなし。現行CLI・コードと一致。C3の文書確定までに完了 |
| C3 ローカル統合と文書を確定 | Rust/lock/flagsを揃えた全回帰、release build、合成CI経路、2回packageと実binary受入。最終差分security検証、操作/制約文書を閉じる | ローカルで再現できる失敗なし。実行できないnative経路と理由を明記 |
| C4 候補SHAを固定して通常CI・native受入 | 承認範囲を確認したpush後、通常CI成功。既存native workflowでarchiveとOffice/serviceを含む3 OS受入 | 同じSHAのCIとnative workflowが成功、42件のnative subset receiptが揃う |
| C5 実provider/local受入 | 成功native runのarchiveを使い、Mistral/Geminiの6laneとauthenticated localの3laneを実行 | 正確な候補・予算・service identityで9件の追加receiptがpass、結果不明要求を重複送信しない |
| C6 最終集約と完成報告 | CI/native/provider/localのrun IDを最終verifierへ渡し、固定matrixとartifactを検証 | 51件の必須receiptと全source runが合格。未解決security・必須機能欠落なし。手動確認票と公開状態を別記 |

主経路はC0→C1→R0〜R5→C3→C4→C5→C6（2026-10-06変更）。D工程はR工程と並行してC3前に終える。
C2はC0から開始し、C1/R/C3と並行する。
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
C4〜C6中にmainへ積んでよいのは、受入証拠に影響しない経路だけを変える修正である（§6の再利用条件）。
それ以外の修正は新SHAを作り、関係するローカル検証を経て必須receiptを取り直す。課金laneは通常CI・native/packageが成功してから実行する。
完成報告はまずrepo外の証跡に保存し、報告文書を後でcommitする場合も受入対象SHAを明記する。

## 6. 既存workflowと最終51件matrix

matrixの正本は `crates/kio-eval/src/acceptance.rs::fixed_v1_requirements()`。
2026-10-03の実装では各OS17件、3 OSで51件であり、単なるA01〜A12×3ではない。
各receiptの内部assertion数と、この必須receipt数を区別する。

| 実行 | 必須証拠 | 件数 |
|---|---|---|
| `ci.yml` | 最終verifierを実行するmainのcommit（検証SHA）のpush run、全必須job成功 | receipt matrixと別に必須 |
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

**受入証拠の再利用（2026-10-06利用者承認）。** native・provider・localのreceiptは、それらを実行した製品の
commit（証拠SHA。receiptの`candidate_sha`）に結び付く。配布binaryはcommit SHAとgit treeを埋め込むため、
binary digestの一致では再利用を判定できない。最終verifierはmainのcommit（検証SHA）で起動し、証拠SHAの
`kio-eval`を使って `acceptance verify-reuse` を実行する。次をすべて満たす場合に限り、証拠SHAのreceiptを受け付ける。

- 証拠SHAが検証SHAの祖先である。
- 証拠SHAから検証SHAへの差分（rename検出なし）が、次の経路だけを変更している:
  `docs/`、`tasks/`、`strategy/`、`crates/kio-ci/`、`scripts/ci/`、製品crate（kio-core、kio-pipeline、
  kio-adapter、kio-index、kio-search、kio-app、kio-cli、kio-process）の`tests/`、`.github/workflows/ci.yml`、
  `README.md`、`CONTRIBUTING.md`、`SECURITY.md`、`.gitignore`。
- 通常CI（`ci.yml`のpush run）が検証SHAで成功している。

製品crateの`src/`（`#[cfg(test)]`だけの変更を含む）と`build.rs`、`Cargo.toml`・`Cargo.lock`、
`rust-toolchain.toml`、`kio-eval`（acceptance fixtureを含む）、受入とreleaseのworkflowを変更した場合は、
新しい証拠SHAでreceiptを取り直す。`kio-ci`と`scripts/ci`は受入とreleaseの準備にも使うが、receiptと
配布物は証拠SHAの版で作られているため、後の変更は既存の証拠を変えない。受入対象の製品は証拠SHAであり、
配布物、release tag、release verifierの`--expected-commit`には証拠SHAを使う。判定した変更経路の一覧は
最終verifierのartifactに残す。証拠SHAと検証SHAが同じ場合は従来の判定と同じになる。

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

## 10. 構造是正（R工程）と文書削減（D工程）— 2026-10-06追加

目的は、v2のGUIが同じRust engineを型付きのrequest/responseで呼べる構造にし、OS固有の処理を共通ロジックから
分離し、[09 §3](../docs/09-mvp-scope.md) の上限を計測できる状態にすることである。CLIの構文・JSON出力・
exit code・保存形式・送信承認の振る舞いは変えない。新機能、CLI/JSON/保存形式の変更、K3〜K6の製品実装、
OS用crateの新設は含めない。

着手時点の実測（2026-10-05、近似）: OS共通の製品ロジック約101,000行、OS固有約22,000行（Windows約11,000、
Unix共通約7,600、Linux約2,300、macOS約1,300）、検証tooling約63,000行。OS条件の属性は製品・tooling合計で
約2,200箇所あり、`kio-core/src/scope.rs`（160）、`gc.rs`・`cas.rs`（各80台）、`kio-index`の`fts.rs`・
`registry.rs`（各80台）、`kio-pipeline/src/ledger/snapshot.rs`（80）、`kio-adapter/src/office_convert.rs`（70）、
`kio-app/src/lib.rs`（54）に集中する。対象外OS向けの代替実装（`not(any(unix, windows))`等）は約85箇所ある。
`kio-app::execute` は `serde_json::Value` を返し、終了コードを `__exit_code` field で上書きし、
実行contextをthread-localで暗黙に渡している。

| 工程 | 作業・成果物 | 終了条件 |
|---|---|---|
| R0 基準 | 開始SHA（2026-10-06の小修正を含むmain）で3 OSの正式四工程（fmt・strict Clippy・全workspace test・release build）とlibtest一覧を取得 | 3 OSで実exit 0。test一覧を候補別に保存 |
| R1 計測ツール | `kio-ci`に09 §3の計測（区分別のコード行、製品ファイルの上限、共通ロジック内のOS条件）を実装。例外の許可リストは理由を必須にする。まず報告だけをCIへ接続 | 手計測の推定値と照合できる。分類・除外・許可リストの単体試験がpass |
| R2 OS固有層の分離 | crateごとに`src/platform/`（interfaceは`mod.rs`、実装は`windows`/`unix`/`linux`/`macos`）へ移す。順序はkio-core→kio-index・kio-pipeline・kio-adapter・kio-process→kio-app。複数crateが使うprimitiveはkio-coreのplatformへ置く。対象外OS向けの代替実装を削除し、未対応targetはbuild時に拒否する | 共通ロジック内のOS条件が許可リスト以外0。crateごとにWindows/WSLのbuildとtestが成功 |
| R3 kio-appの分解と型付きAPI | `execute`を型付きresponseに変え、JSON化と終了コードの決定をCLIへ移す（`__exit_code`を廃止）。thread-localの暗黙contextを明示引数にする。`lib.rs`をコマンド群ごとのmodule（index、search、embedding・batch、snapshot・履歴、adapter・ledger、repair、open・view等）へ分ける | CLI契約試験を変更せずpass。`lib.rs`はdispatchだけになる。各ファイル3,000行以下 |
| R4 kio-coreほかの分割 | `scope.rs`（repositoryのopen・lock、refs・HEAD・tag、承認記録、log）、`gc.rs`、`cas.rs`、`kio-app/src/verify_objects.rs`を責務別のmoduleへ分ける | 製品ファイルの上限超過0 |
| R5 gate | R1のツールをCIの必須gateにし、上限・分離規則・ファイル上限を検査する。計測値を09 §3へ記録する | 全区分が上限内。3 OSの正式四工程が成功。C3へ進む |

- 移動と振る舞いの変更を同じcommitに混ぜない。移動だけのcommitでは振る舞いを変えない。
- 局所検証はMacの全workspace testと、Windows/WSLの関係crate。3 OSの全体回帰はR2・R3・R5の終了時に行う。
- testの期待値を変えて通さない。CLI契約試験（`crates/kio-cli/tests`）の変更は、移動に伴うpath・importの修正に限る。ただし、対象外targetのbuild拒否で対応3 OSのどれでもcompileされなくなったtest・assertion・guard・commentの削除はR2の対象外OS向け代替実装の削除に含め、この制限の例外とする。対応3 OSのtest集合と期待値は変えない。
- K3（状態モデルの仕様）は並行してよい。K4以降の実装はR3の型付きAPIの上に作る。

D工程は [docs/README §6](../docs/README.md) の「仕様書に書くもの / 書かないもの」を基準にする。

1. docs/14を03・05・07・12・13へ日本語で統合し、14を削除する。
2. OS固有の機構と実装手順を除く。例: store-gateのOS別lock手順（03・05・06・10に重複）、SQLiteのleaf複写手順、
   Windows HANDLEやmacOS SDKの扱い。
3. 同じ規則の転記を正本への参照に置き換える。例: 10 §11.3・§11.4と06 §10・§11。
4. 日付付きの裁定・経緯・計測記録をgit履歴に任せて削除し、09 §4.1の評価harnessの詳細はeval/READMEへ寄せる。
5. R工程で移動したコードを指す記述を更新する。

目安はdocs/01〜14の合計を約59万字から約40万字以下にすることで、必要な契約を削って達成しない。
文書だけの変更のためRed→Greenは適用せず、リンク・参照・現行CLIとコードとの照合で検証する。

V工程（実データ確認）は、利用者の検証用データが揃った時点の最新候補でM3-1〜M3-3を確認し、所要時間と
失敗例を記録する。v1の受入条件にはしない（[docs/11 §3](../docs/11-product-requirements.md)）。結果はK3〜K6の
優先度と検索体験の改善に使う。

## 11. 計画の見直し（2026-10-06）

1. **工程順序。** 主経路をC0→C1→R0〜R5→C3→C4→C5→C6に変えた。製品を広く変更するR工程の後に候補を固定しないと、
   C3以降の配布物・51件のreceiptを取り直すことになる。
2. **S7候補の扱い（2026-10-06利用者承認）。** S7の配布物検証、hosted CI、native受入はR後の最終候補に流用できないため
   実行しない。R0の基準は小修正後のmainで取る。C2の専用経路は既存方針どおり最終候補に合わせて適用する。
3. **受入証拠の再利用（2026-10-06利用者承認）。** 製品と無関係なtest・CIだけの修正でもSHAが変わり、51件を
   取り直していた。当初はbinary digestの一致で判定する案だったが、配布binaryがcommit SHAとgit treeを埋め込むため
   成り立たない。証拠SHAから検証SHAへの差分経路で判定する方式を§6に定めた。S4〜S7の是正の多くは製品`src/`内の
   試験やkio-evalの変更を含んでおり、この方式でも取り直しは残る。効果は終盤の文書・CI・統合試験だけの修正に限られる。
4. **検証の粒度。** R工程中の3 OS全体回帰はmilestone単位に限る。S6で導入したWindows通常CIの2分割は維持する。
5. **完了条件。** §1に構造是正とD工程を加えた。必須receipt 51件とA01〜A12は変えない。
6. **リスク。** (a) R3は出力の微差を生みやすい。CLI契約試験を変更しないことで検出する。(b) OS分離で
   Windowsだけの経路が初めてbuildエラーになり得る。crate単位でWindows/WSLのbuildを完了条件にする。
   (c) OS共通ロジックの上限（100,000行）は現状（約101,000行）を下回る。対象外OS向け代替実装と分割時の重複を
   削除して収める見込みだが、収まらない場合は削減候補を示して利用者の判断を求める。
