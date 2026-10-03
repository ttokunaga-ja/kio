# Kio — 保存・復元・引用保持の実装計画

作成日: 2026-10-03 JST。方針は利用者が同日承認済み。本書は承認された方向性を工程に展開する。
追加機能の CLI 名、JSON schema、永続形式、発売版番号はまだ固定しない。実装前に対応する spec へ
詳細契約を反映する。本書の作成・承認を製品コードの実装や受入完了と扱わない。

## 1. 目的と維持する設計

異種資料の取り込み、特定版の検索、原資料への検証可能な復帰を一貫した体験にする。
原本 CAS、不変の Evidence Pointer、単一 parent / 単一 HEAD の線形履歴、truth と検索 projection の
分離を維持する。履歴の状態復元は current HEAD の新しい子を作り、送信承認・trust・課金台帳を
巻き戻さない。jj の保存基盤、分岐・rebase、ライブラリ依存は本工程に導入しない。

製品要件の正本は [docs/11](../docs/11-product-requirements.md)、現行 CLI の正本は
[docs/06](../docs/06-cli-spec.md)。既存 [v1完成計画](v1-completion-plan-2026-09-08.md) の
M0〜M7・A01〜A12と受入条件を維持し、本書の後続改善を v1 の追加待ち条件にしない。

## 2. 調査時点の基準と差分

以下は 2026-10-03 の読取り調査記録であり、以後の最新状態の保証ではない。

| 対象 | 調査時点の基準 | 判定 |
|---|---|---|
| ローカル main | `2fe259ee434f49264752b437223b19e8d0929c20` | 公開 main より11コミット先行。未追跡WIPあり |
| GitHub main | `1ffdd0ca1bc9fba9ec0a5b0b8c58302daac80545` | 比較資料のソース基準と一致 |
| 最新配布版 | `v0.1.0-rc.3` / `1df10f248b73910349f80f45c30f801b5bfef5f5` | 現行 main の機能と同一とは扱わない |
| 公開 main の CI | [run 36287581490](https://github.com/ttokunaga-ja/kio/actions/runs/36287581490) | macOS SDK準備、Windows Clippy、Linux renderer環境導入が失敗 |
| ローカル最終候補 | [実装・受入記録](v1-implementation-progress.md) | 全回帰、最終差分のsecurity検証、配布物、同一SHAのnative受入が残る |

既存機能と追加対象を区別する。

| 領域 | 既存実装 | 今回の後続改善 |
|---|---|---|
| watch | native通知、永続queue、共通reconciliation、service、backlog / degraded / 最終成功・失敗 | ファイル・版別の保存と検索準備状態 |
| restore | preview、期待HEAD、変更一覧、journal、明示回復、projection結果の分離 | 人間向けの影響表示、操作識別・状態照会 |
| tag / retention | commitへの名前付け、HEAD/tag tipとmanual/restored commitのshallow保護 | 引用した正確なcommitを既存tagで保持する導線 |
| export | 指定版・原本ファイルの書き出し | 自立した引用bundleは需要確認後の別機能 |

追加照合で判明した既存要件の残差: docs/06には `tag --delete` の契約があるが、現行CLIの
`TagArgs` とapp dispatcherは作成しか接続していない。タグの削除・再作成を現状機能として扱わない。
この削除経路の実装とCLI/回帰試験はK2の既存v1要件に含め、K5の表示改善と二重計上しない。

ソース根拠: [watch](../crates/kio-app/src/watch_command.rs)、
[restore app](../crates/kio-app/src/managed_restore.rs)、
[restore journal](../crates/kio-core/src/scope/managed_restore.rs)、
[tag](../crates/kio-core/src/scope.rs)、[retention policy](../crates/kio-core/src/dag.rs)、
[export](../crates/kio-app/src/export.rs)。新しいビルド・実機試験・性能比較はこの調査では行っていない。

## 3. 工程・依存・完了条件

K0〜K6は本計画の識別子であり、既存M0〜M7・A01〜A12を置き換えない。

| 工程 | 実装・成果物 | 完了条件 | 依存 |
|---|---|---|---|
| K0 作業基準を固定 | HEAD、tracked/staged/untracked差分と対象fingerprint、既存処理・検証記録、未完了一覧を再確認 | 他作業の変更を識別でき、要件→実装→試験→receiptの残差が明示される | なし |
| K1 説明を揃える | README、docs/06・08・11・12・13、索引に既存機能・保持範囲・配布版・追加方針を反映 | 現行CLIと文書が一致。実装・ローカル検証・native受入・配布を混同しない | K0 |
| K2 既存v1を受入まで閉じる | 現在の保存/回復/renderer等の修正と未接続tag削除を統合し、既存完成計画の残りを実行 | 最終候補SHAで全必須自動receiptがpass。実端末確認は別票、公開も別操作 | K0、K1 |
| K3 状態モデルを固定 | ファイル・版・処理profile別の状態、観測時点、失敗/待機理由、read-only契約をspecへ反映 | 状態の根拠と鮮度、未観測、旧版と最新版、検索方式別の準備状況を定義 | K1。設計はK2と並行可 |
| K4 状態表示を実装 | 共通app API、CLIの人間向け/JSON表示、watch・手動index・restore後の共通状態投影 | U01〜U06がpass。照会で送信/処理再開/承認/修復を起こさない | K2、K3 |
| K5 引用保持の導線を実装 | 既存tagを使う版保持、保持状況の照会、tag解除・GC・purgeの影響表示 | U07〜U10がpass。元のpointerを変更せず、欠損版や権限拒否を保持成功にしない | K2、K3。K4と所有範囲を分けて並行可 |
| K6 復元の操作表示を改善 | 復元前後の版・変更・残る履歴・projection状態を表示。操作識別が必要な範囲の永続契約を固定して追加 | U11〜U12がpass。復元成功と検索準備完了を区別し、回復・再実行の範囲を説明できる | K4。共通表示/API統合はK5後 |

K3の設計準備はv1受入を待たず進められるが、K4〜K6の製品変更はK2の候補に混ぜない。
承認後のK0〜K2の実行工程、現在の残差、最終51件matrixは
[既存v1の受入完了と文書整合の計画](v1-acceptance-closeout-plan-2026-10-03.md) に具体化した。
工程日数はK0で残差と実行環境を確認してから見積もる。未準備のnative/provider環境を実装日数に
埋め込んだ固定納期は置かない。

## 4. K3 / K4 — 状態表示の契約

状態を一つの進捗パーセントや一本道の enum に集約せず、次の軸を分ける。名称は概念でありwire schemaではない。

- 観測: 未走査 / 変更候補 / 安定した内容確認済み。観測時点と監視の能力低下も示す。
- 原本: CAS保存と対応する版の公開が確認できるか。検知しただけで保存済みにしない。
- 派生処理: 抽出・chunk・embeddingの実効profileと準備状況。承認待ち、予算待ち、処理中、失敗を分ける。
- 検索: 全文 / vector / imageとscope / replicaの準備状況。原本保存だけで検索可能にしない。
- 履歴: 最新版の状態と、最後に検索可能だった旧版の状態を区別する。
- 対象外: 現行Ignore、管理境界、未対応形式を理由として扱う。除外対象の本文やhashを照会のために新規取得しない。

pathは表示・選択に用い、保存済みの状態はscope identity、commit、raw hash、profile、projection世代へ
結び付ける。まだhashを確認していない通知を版の証拠にしない。安定した読み取りで確定できなければ
unknown / stale / busy等の理由を返し、時計・古いqueue・cacheだけで最新版の準備完了を推定しない。

状態投影は再構築可能なcacheに置くことを基本とし、現行policyと正本で裏付ける。cacheを原本・承認の
正本にしない。対応するspecはdocs/04・05・06・12。初期実装は既存status/watch status等への追加を
優先し、専用コマンド・永続schema・性能上限はK3で比較して決める。

## 5. K5 — 既存tagで引用版を保持する

引用が示す元commitを保持対象にする。現在の版をmanual snapshotするだけでは、過去のpointerが
示すauto commitを保護したことにならない。複数scopeの引用はscopeごとの結果を示し、一括atomicな
保持を約束しない。照会とpreviewは無変更、保持適用は既存tagの検証・lock・現行policyを使う。

tag作成前に対象commit/treeとpointerの解決可能性を検証する。tagは通常の保持期限GCから対象tipを
保護するが、purge/erase、破損、scope喪失、現行policy拒否からの万能な保証ではない。
shallow化済みのcommitは保持成功にせず、backup等の回復手順を示す。K2で完成させるtag削除・再作成と、
引用が動かないことを区別する。pointer解決時にtagや最新版へ黙って付け替えない。

新しいpin正本や一般的な参照カウントを先に作らず、既存tagとretentionを利用する。引用群の登録が
必要なら、利用者の明示登録を基本とする。Kioが外部アプリで発行された全引用を把握できると表示しない。
削除影響は把握している対象と範囲を示し、未登録の外部引用まで安全と推定しない。
現行GC previewとpurge確認を拡張する詳細契約はdocs/05・06・08・10で固定する。

## 6. K6 — 復元と操作の表示

既存previewの変更一覧、expected HEAD、新commit、provenanceを再利用し、上書き/明示削除、
維持されるファイル、残る履歴、検索再構築、外部送信の有無を人間向けに説明する。
原本適用済みでprojection失敗の状態を「何も変更されていない失敗」と表示しない。

まず既存journalと結果の読取りで実現できる範囲を実装する。永続operation IDを追加する場合は、
before/after、source/expected HEAD、進行段階、重複要求、中断後の回復、完了記録の保持期間を
docs/03・05・06・13で先に固定する。previewから適用まで同じ状態であるとは仮定せず、必ず再検証する。
過去操作の詳細照会でprofile/承認/予算を巻き戻さず、送信・課金・公開済みデータの取消を約束しない。

## 7. 追加改善の受入ケース

既存A01〜A12に次のUケースを追加対応付けする。既存の検証を減らしたり、U成功をv1受入の代用にしない。

| ID | ケース | 期待結果 |
|---|---|---|
| U01 | 原本保存済み、OCR送信未承認 | 原本保存と承認待ちを表示。本文送信0、抽出/検索成功を捏造しない |
| U02 | 全文検索可能、embedding未作成 | 検索方式ごとの状態を分離。全方式準備完了にしない |
| U03 | 旧版が検索可能、最新版は処理中 | 版と観測時点を表示し、旧版を最新版として報告しない |
| U04 | watch停止、overflow、再起動、処理失敗 | 監視の継続性と保存状態を分離。再照合後に手動indexと収束する |
| U05 | Ignore変更、scope移動、複製、cacheだけ残存 | 現行policyを再確認。拒否対象の内容・古いauthorityを公開しない |
| U06 | read-only照会、cache喪失、並行更新 | scope/ledger/承認等を変更せず、HTTPも発生しない。不明状態を理由付きで示す |
| U07 | 引用元auto commitをtag保持して保持期限GC | 同じpointerが同じ原資料へ解決する。変更前後のbytes/hashを検証 |
| U08 | 既にshallow、purged、破損、policy拒否の引用を保持 | 保持成功とせず正確な拒否・回復案内。raw/cacheへのfallbackなし |
| U09 | tag解除・再作成と後続GC、purge preview | 把握している引用への影響と保持範囲を表示。既発行pointerは書換えない |
| U10 | 同内容の別path、rename、同名tag、保持とGCの競合 | 正確なcommitとpointerを検証。対象を取り違えず、競合時は再検証または拒否 |
| U11 | 復元原本適用成功後にprojection失敗 | 適用済み版と検索未準備を分離。現在のpolicy/台帳/承認を保持 |
| U12 | 復元中断、回復、再実行、preview後のHEAD/ファイル変更 | 安全な回復・no-op/競合拒否。操作ID導入時は重複実行契約も確認 |

## 8. 実装責任・検証・commit

- 主担当は要件、状態/CLI/schema、Cargo lock、保存・権限境界、統合と最終受入を持つ。
- Solの実装workerは、状態導出（pipeline/index）、表示（app/CLI）、保持（core/app）、復元表示を
  所有ファイルで分ける。共通型を固定する前に並行編集しない。探索と既知の検証はLunaを使う。
- 既存WIPをfingerprintして保護し、mainで論理単位をcommitする。無関係な未追跡物を含めない。
  この計画の承認だけをpush、release公開、課金処理、端末の権限変更の承認として使わない。
- 文書変更は差分・相対リンク・現行CLIとの整合を確認する。製品変更は既存fixtureとfault injectionを
  用い、fmt→strict Clippy→関係する試験→必要なworkspace回帰/build/合成E2Eをローカルで実行する。
- push前にworkflow/hooksと同じruntime・lockfile・flagsを確認し、安全に再現できるCI処理を完了する。
  Actionsはnative最終確認に用いる。実providerは既存のcredential/予算/receipt契約に従い別実行する。
- 新しい保存/保持/削除/権限境界を変更した差分はsecurity検証を行う。表示だけの変更に同一内容の
  重複監査を増やさない。過去のsecurity/CI成功を別SHAの完成証拠にしない。
- 報告は製品実装、ローカル検証、3 OS受入、配布物、実端末確認を分ける。性能・personaCorpus評価は
  v1のnonblockingを維持し、状態照会の不要な全件hash走査等は小規模の回帰で確認する。

commitの単位はK1文書、K2既存修正の論理単位、K3契約、K4状態表示、K5引用保持、K6復元表示を基本とする。
契約と対応実装を不可分に保つ必要がある場合は同じcommitにまとめる。

## 9. 条件が成立したら検討する後続案

| 案 | 着手条件 | 最小案と受入条件 |
|---|---|---|
| 自立した引用bundle | local `.kio` のない環境で引用を検証・解決する利用要求 | pointerと必要closure、manifest/digestを持ち出し、別環境で検証。原本exportと別機能 |
| 文書lineage ID | v2履歴閲覧でrename/編集の追跡要求 | 候補提示と利用者確認。コピー・同内容の別資料を誤結合せず、pointer identityは維持 |
| 任意jj読み取りAdapter | jj原稿の特定過去版を検索したい利用要求 | 不変commit、repository identity、hash方式、path、内容digestを記録しCASへ取り込む。元jjのrewrite/GC後も保持対象の引用が解決 |
| 競合の構造化 | jj取り込み、v3共同編集の具体化 | 未解決候補を通常回答へ混入させず、複数版と採用状態を示す |

これらの個別機能の実装契約・時期は未承認。jj-lib/jj-coreの内部採用や保存基盤置換は、連携実験と
代表的なPDF/Office/image workloadの容量・変換重複・復元時間・メモリ実測で必要性を示してから再判断する。
jjのCLI/ライブラリAPIには安定性の制約があるため、連携時は対応版検出・固定と更新試験を用意する。
参考: [jj operation log](https://docs.jj-vcs.dev/latest/operation-log/)、
[連携方式の公式FAQ](https://docs.jj-vcs.dev/latest/faq/#i-want-to-write-a-tool-which-integrates-with-jujutsu-should-i-use-the-library-or-parse-the-cli)。

## 10. 文書反映の記録

2026-10-03: K1の文書整合作業として本計画を追加し、README、docs/06・08・11・12・13と索引、
既存v1完成計画に対応を反映した。実施範囲は文書変更と差分・リンク・CLIソースとの照合であり、
K2〜K6の製品実装、ビルド、追加受入ケース、Actions、配布公開はこの作業では実行していない。
