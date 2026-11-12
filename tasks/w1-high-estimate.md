# 制約の緩和の残り（優先度「高」12 件）：見積もりと作業単位

2026-11-13 に Codex（gpt-6.1-sol）が main `9030de0` を静的に調べた結果である。テストと実機の動作は確かめていない。計画の正本は [v1-plan.md](v1-plan.md) §3.2 で、この資料は各単位の根拠と受入条件の詳細として使う。着手するときは、単位ごとに現在のコードで根拠を確かめ直す。

**W1〜W6 は分類としては妥当ですが、Codex に渡す作業単位としては大きすぎます。共通基盤を含む18単位、合計28〜42回程度の実装・修正を見込む案を推奨します。** 半日を1回とすれば14〜21日程度で、統括のレビュー、CI待ち、環境問題への対応は別です。静的調査に基づく幅のある見積もりです。

調査対象は main `9030de0` と現行作業ツリーです。AGENTS.md、tasks/v1-plan.md を最初に読み、指定の過去記録も確認しました。ファイルの変更、commit、push、テスト実行はしていません。

**1. 項目別の見積もり**

以下の crate 名は `kio-` を省略。ファイル数は製品コード・直接関係するテスト・契約定義の変更候補数で、共通基盤を含む概算です。行の数を足すと重複します。「実装済み」は静的確認であり、今回の実動作検証ではありません。

| ID | 緩和後の仕様・根拠 | 現在残る旧制約・根拠 | 影響概算 | OS差 | 作業量・理由／既存の足場 |
|---|---|---|---|---|---|
| ST05 | tree節：10,000件は警告。reader/writerも10,001件を拒否せず、有界buffer・disk stageで処理。[03:961](../docs/03-data-model.md) | `MAX_TREE_ENTRIES=10_000`。[dag:13](../crates/kio-core/src/dag.rs)、列挙時拒否 [working_tree:454](../crates/kio-core/src/scope/working_tree.rs)、履歴readerにも適用 [history:759](../crates/kio-core/src/history.rs) | core/app/cli/eval中心、4〜6 crate・15〜25ファイル | 件数契約は共通。stage・rename・名前の扱いはOS差あり | **大**。列挙だけでなくsort、canonical JSON、履歴・GC・evalまで整合が必要。既存tree検証・publicationは利用可能 |
| ST04 | 原本保存節：固定512MiB/4GiBを廃止。任意のCLI予算、未保存path表示、statusとの分離。[03:968](../docs/03-data-model.md) | `MAX_RAW_OBJECT_BYTES`、archive既定値。[cas:46](../crates/kio-core/src/cas.rs)、[scope:95](../crates/kio-core/src/scope.rs)。bound writerも固定値でclamp [bound:62](../crates/kio-core/src/cas/bound.rs) | core/app/cli/eval中心、4〜5 crate・12〜22ファイル | handle・publish・cleanupに差 | **大**。writerに加え既存object照合・reader・利用側を確認する必要。64KiB bufferのstreaming stage/hashは既存 [bound:65](../crates/kio-core/src/cas/bound.rs) |
| ST06 | 復元手順：stage identity/digest/長さをjournalから参照し、段階・進捗・crash回復を維持。[13:84](../docs/13-linear-history.md) | `MAX_MANAGED_RESTORE_FILE_BYTES=64MiB`、`TOTAL=512MiB`、raw全体を`Vec`へ読む [restore:20](../crates/kio-core/src/scope/managed_restore.rs)、[restore:628](../crates/kio-core/src/scope/managed_restore.rs) | core/app/cli、3 crate・7〜13ファイル＋共通stage | **大きい**。Unix inode／Windows FileID、共有・rename・永続化 | **大**。安全な中断回復が主工数。現在のjournalは既に本文埋込みではなくold/new raw hashとphaseを保存 [restore:122](../crates/kio-core/src/scope/managed_restore.rs) |
| AD-14 | §2.1・抽出tier節：page逐次処理、固定256page/8,192object廃止、不正と資源不足を区別。[07:90](../docs/07-adapter-spec.md)、[07:380](../docs/07-adapter-spec.md) | `MAX_DETERMINISTIC_PDF_PAGES=256`、object・展開・text・CMap・operand上限 [deterministic:13](../crates/kio-adapter/src/deterministic.rs)、[pdf_decode:38](../crates/kio-adapter/src/pdf_decode.rs) | adapter/pipeline/app/core/cli、5 crate・12〜20ファイル | parserは共通。private stageとOffice経由は差あり | **大**。parser・出力保存・保留再開まで必要。既存のFlate/CMap復号とchecked budgetは足場 |
| AD-16 | XLSX節・抽出tier節：row/sheet逐次処理、疎な列を常駐paddingしない。previewと未処理範囲を表示。[07:348](../docs/07-adapter-spec.md) | `MAX_XLSX_SHEETS/ROWS_PER_SHEET/COLUMNS/CELLS_PER_WORKBOOK`等 [xlsx:47](../crates/kio-adapter/src/xlsx_extract.rs)。prepareは全sheetを収集 [prepare:180](../crates/kio-pipeline/src/prepare.rs) | adapter/pipeline/app/core/cli、5 crate・12〜20ファイル | 抽出は共通、stageは差あり | **大**。shared strings、書式、escape、疎表現の会計が必要。直接抽出・sheet identityは既存。chart/image抽出は仕様上対象外 [07:375](../docs/07-adapter-spec.md) |
| AD-29 | billing lifecycle節：snapshot/backupはstreamingまたはonline backup、完全検証まで公開しない。[14:68](../docs/14-v1-runtime-contracts.md) | snapshot `MAX_LEAF_BYTES=64MiB/TOTAL=128MiB` [snapshot:34](../crates/kio-pipeline/src/ledger/snapshot.rs)。backup `MAX_SQLITE=512MiB`・`Vec`保持 [backup:10](../crates/kio-pipeline/src/ledger/backup.rs)、`serialize().to_vec()` [lifecycle:345](../crates/kio-pipeline/src/ledger/lifecycle.rs) | pipeline/app/cli＋core基盤、3〜4 crate・10〜17ファイル | SQLite handle・sidecar・private copyに差 | **大**。表示用snapshotと課金正本backupを分割すべき。snapshotの有界copy、authority/checkpoint/proof検証は既存 |
| AD-30 | task store節：累積append量で寿命切れにせず、現在状態・attempt・ownershipを保持して整理。[04:759](../docs/04-pipeline.md) | `MAX_TASK_STORE_BYTES=64MiB/RECORDS=100_000` [task:24](../crates/kio-pipeline/src/task.rs)、unsupported `16MiB/100_000` [unsupported:21](../crates/kio-pipeline/src/unsupported.rs) | pipeline/core/app/cli、4〜5 crate・8〜15ファイル | 整理時の置換・回復に差 | **大**。compaction中断とcopy/registration/recoveryの読者まで確認。framed append・fsync・record検証は既存 [unsupported:67](../crates/kio-pipeline/src/unsupported.rs) |
| APP-26 | §2.1 gen：index/watchとも追加確認なし、新旧hash・renderer・producerをimmutable manifestへ記録。[03:273](../docs/03-data-model.md) | driftで`!args.yes`ならexit9 [pipeline:1007](../crates/kio-app/src/index/pipeline.rs)。manifestにrequired provenanceがない [markdownize:201](../crates/kio-pipeline/src/markdownize.rs) | app/pipeline/adapter/core/cli、4〜5 crate・8〜14ファイル | gen契約は共通、renderer情報はOS差 | **中**。確認除去だけなら小だがprovenanceまで必要。gen+1生成経路は既存 [pipeline:1545](../crates/kio-app/src/index/pipeline.rs) |
| APP-11 | watch service節：同一定義は冪等成功、`install --replace`で確認・停止・置換・回復。[06:25](../docs/06-cli-spec.md) | `same_installation`不一致拒否、installed重複も拒否 [watch_service:160](../crates/kio-app/src/watch_service.rs)、[watch_service:208](../crates/kio-app/src/watch_service.rs) | app/cli＋core基盤、2〜3 crate・9〜15ファイル | **大きい**。systemd／launchd／Task Scheduler | **大**。旧新intentとOS登録をまたぐ状態遷移。private manifest、mutation lock、foreign拒否、途中install回復は既存 |
| CLI-01 | §4.1：scope内新規宛先を許可。任意scopeの`.kio`と管理中working file衝突を拒否。[05:1695](../docs/05-runtime.md) | `validate_destination`がscope全域をprefix拒否 [export:731](../crates/kio-app/src/export.rs) | app/cli、2〜3 crate・5〜10ファイル | case/reparse/handleに差 | **中**。prefix除去だけでは内部store侵入・実衝突を防げない。destination handle・no-replace等は既存 [export:900](../crates/kio-app/src/export.rs) |
| AD-17 | Office節：実変換capability、実identity/runtime closureで判断。vendor・prefix・versionだけで拒否しない。[07:296](../docs/07-adapter-spec.md) | probeは`--version`のみ [office:274](../crates/kio-adapter/src/office_convert.rs)。macOS vendor署名固定 [macOS:104](../crates/kio-adapter/src/platform/macos/office.rs)、Linux `/opt/libreoffice…`固定 [package:3](../crates/kio-adapter/src/platform/linux/package.rs) | adapter/process/app/cli、4 crate・14〜24ファイル | **大きい**。署名・runtime発見・隔離方式 | **大**。trust境界を保つ設計が必要。fingerprint、private scratch、隔離実行は既存 |
| PROC-01 | Linux資源境界節：systemd/cgroup必須を維持し、不足機構と代替経路を表示。[07:312](../docs/07-adapter-spec.md) | production probeが診断エラーを`.ok()`で消す [office:244](../crates/kio-adapter/src/office_convert.rs) | process/adapter/app/cli、4 crate・7〜12ファイル | 実検査はLinux限定、表示は共通 | **中**。隔離の置換は不要。既存検査の理由を型付きでCLIまで運ぶ作業が中心 |

**2. 必要なテストと、旧制約を前提とするテスト**

件数は「書換え候補のテスト関数数」の概算です。境界ケース数や必要な新規テスト数とは異なり、全件の確定棚卸しは未確認です。破損・identity・承認保護のテストは維持します。

| ID | 新規・書換えで確かめる動作 | 旧制約を前提とする候補 |
|---|---|---|
| ST05 | 10,001件で警告＋全件保存、履歴読取り・GC・eval整合、stage不足時HEAD不変、sort/hash決定性 | 約2〜4件。[cardinality拒否:4394](../crates/kio-core/src/scope.rs)、[snapshot拒否:4421](../crates/kio-core/src/scope.rs) |
| ST04 | 512MiB超単体・4GiB超合計の保存、明示予算のexact/+1、status継続、disk full・既存CAS照合・残骸回復 | 約3〜6件。[archive境界:4097](../crates/kio-core/src/scope.rs)、[固定超過:4137](../crates/kio-core/src/scope.rs)。明示予算の保護は残す |
| ST06 | 64MiB超・合計512MiB超、各durable phaseのcrash、同内容別実体、stage改変、公開後projection失敗、進捗 | 固定容量の直接assertは今回特定できず**未確認**。回復系約8〜12件は内部構造変更の影響候補 [restore:1344](../crates/kio-core/src/scope/managed_restore.rs) |
| AD-14 | 正常257page・8,193object、両tierのexact/+1、保持bytes/work計測、循環・爆弾、unsupported filterの既存意味論 | 約6〜10件。[page拒否:1059](../crates/kio-adapter/src/deterministic.rs)、[爆弾をcontract扱い:1738](../crates/kio-adapter/src/pdf_decode.rs) |
| AD-16 | 257sheet・50,001row・遠い疎列、shared strings/escape会計、preview・中断再開、sheet名identity、ZIP/XML拒否 | 約5〜8件。[sheet:1313](../crates/kio-adapter/src/xlsx_extract.rs)、[row/cell:1488](../crates/kio-adapter/src/xlsx_extract.rs)、[budget:1578](../crates/kio-adapter/src/xlsx_extract.rs) |
| AD-29 | 64/128/512MiB超、WAL整合、source不変、途中backup非公開、容量不足分類、stale proof拒否、restore往復 | 少なくとも1件＋backup側候補、約1〜3件。[oversized=unsafe:1759](../crates/kio-pipeline/src/ledger/snapshot.rs) |
| AD-30 | 旧累積上限超のappend、整理前後の現在状態同一、整理各段階のcrash、破損record拒否、登録・copy・resume整合 | 約3〜5件。[task容量・件数:1734](../crates/kio-pipeline/src/task.rs)、[unsupported容量:404](../crates/kio-pipeline/src/unsupported.rs)。record単体上限は別扱い |
| APP-26 | `--yes`なしindex/watchでg1、raw/g0/Evidence不変、provenance必須、同出力で世代増加なし、外部・有料承認不変 | 主に1件。[driftで確認要求:540](../crates/kio-cli/tests/step4b_office_contract.rs) |
| APP-11 | 同一定義no-op、確認前無変更、旧新差分、停止・置換・回復、foreign拒否、各OS scheduler整合 | 約1〜3件。[duplicate拒否:1451](../crates/kio-app/src/watch_service.rs)。CLIは現状help中心 [test:9](../crates/kio-cli/tests/v1_watch_service.rs) |
| CLI-01 | `scope/Recovered`成功、nested scopeの`.kio`拒否、tracked衝突をforceでも拒否、検査後の`.kio`生成・消失・置換race | 約1〜2件。[宛先拒否群:306](../crates/kio-cli/tests/step4_export.rs)。store保護assertを丸ごと削除しない |
| AD-17 | version形式差・他vendorの有効署名、実変換失敗、runtime内symlinkとclosure外escape、probe後実体変更 | 約4〜7件。[version・layoutテスト群:1251](../crates/kio-adapter/src/office_convert.rs) |
| PROC-01 | user manager/controller/cgroup.kill/helper/descriptor解決の不足を個別表示、代替経路利用、隔離なし実行ゼロ | 固定制約そのものは維持。書換えは約0〜1件、理由表示テストを追加。[不在契約:458](../crates/kio-cli/tests/step4b_office_contract.rs) |

**3. 依存関係と、先に作る共通機構**

| 機構 | 現状と推奨 |
|---|---|
| 型付き`resource_limit` | `RetryErrorKind`には未追加 [task:364](../crates/kio-pipeline/src/task.rs)。一方、局所的なCLIエラー生成は存在 [local_execution:872](../crates/kio-app/src/local_execution.rs)。**coreの共通descriptor→Adapter/Pipeline/Task/manifest/CLIを損失なく通す独立単位**を先行。paused/partial、attempt不増、自動retryなしは [04:856](../docs/04-pipeline.md)、contextは [04:957](../docs/04-pipeline.md) |
| 永続stage identity | 現行atomic identityはdevice/inodeまたはvolume/file index [atomic:52](../crates/kio-core/src/store_dir/atomic.rs)。中断後の識別方針を確定してから、raw・restore・抽出・journal整理のstageへ共有 |
| 抽出tier | standard/largeの数値は既に仕様化済み。新しい値決めは不要 [07:382](../docs/07-adapter-spec.md)。保持memory、累積work、unit text、ZIP量を別々に会計する |
| 逐次抽出→保存 | 現在はprepare後、保存時にPDF/Office/XLSXを再抽出している [objects:194](../crates/kio-app/src/object_persistence.rs)。**一度抽出したunitをprivate stageから公開する経路**を共有。汎用ジョブ基盤へ広げない |
| normalized reader/writer | `MAX_NORMALIZED_INSTANCE_BYTES=256MiB`が後段の壁 [markdownize:28](../crates/kio-pipeline/src/markdownize.rs)。PDF/XLSXだけ直しても未達。unit単位の保存・読取りと必要consumerを同じ変更系列に含める |
| journal整理 | task/unsupportedは再構築可能projection。課金台帳・送信承認の正本とは責務を分ける [04:759](../docs/04-pipeline.md)。現行状態だけのatomic compactionを第一候補とし、segment管理を先回りして追加しない |
| 依存の主線 | `資源型 → stage → raw → tree/restore`、`資源型＋stage → 抽出保存 → PDF/XLSX`。APP-26・CLI-01・service・Office診断は相互の機能依存が薄い。AD-17がrenderer変更を許す前にAPP-26を完了させる |

**4. Codexへ渡す18作業単位の案**

各行は範囲を固定した依頼単位です。複数回かかる場合は同じ単位を継続します。実装担当の出力・一時物はrepo内の指定先または`$TMPDIR`、レビューは読み取り専用とします。

検証略号：`T(core,pipeline…)`＝`cargo +1.98.0 test -p kio-<crate> --all-targets --locked`を列挙crateに適用。`F/C`＝fmt check／strict clippy。各単位はTDDとF/Cを実施し、区切りで全workspace検証。正式コマンド・実行主体は [plan:75](../tasks/v1-plan.md) に従います。

| 単位・範囲 | 先行 | 観察できる受入条件（各3〜5点） | 変更可／変更しない範囲 | ローカル検証／3 OS CI |
|---|---|---|---|---|
| R0 資源結果の共通契約 | — | ①exit3/context一致 ②paused/partial保持 ③attempt不増 ④通常resumeは再投入しない ⑤不正入力を資源不足に変換しない | core/error、adapter/pipeline型、Task/manifest、app/CLI／parser刷新・課金規則変更なし | T(core,adapter,pipeline,app,cli)、F/C／共通変更としてまとめて3 OS |
| R1 永続stage識別・stream API | 設計判断、R0 | ①同内容別実体を拒否 ②ID再利用模擬で誤採用しない ③nonce非対応を明示 ④各phase crash回復 ⑤曖昧な残骸を消さない | core/store_dir・platformと直接caller／一般ユーザーファイルへnonce追加なし | T(core)、F/C、障害注入／**3 OS必須、独立区切り** |
| S1 ST04 原本保存 | R0,R1 | ①512MiB/4GiB超を保存 ②明示予算exact/+1 ③未保存path表示 ④不足時HEAD不変 ⑤statusは容量で停止しない | CAS、archive、CLI引数、raw consumer／prepared/image等の別上限を一括撤廃しない | T(core,app,cli,eval)、F/C、eval／**3 OS必須** |
| S2 ST05 tree | R0,R1、S1のAPI | ①10,001件全件＋警告 ②reader/writer/hash一致 ③履歴・GC成功 ④stage不足で不完全tree非公開 | tree、working tree、history/GC、関連reader/eval／履歴walk予算全般の緩和なし | T(core,app,index,search,cli,eval)、F/C、eval／**3 OS必須** |
| S3 ST06 restore | S1、R1 | ①旧容量超で復元 ②file/bytes/phase表示 ③各phaseで回復 ④外部変更を上書きしない ⑤projection失敗でも公開済HEAD維持 | managed_restore、stage、app応答・CLI／送信・policy巻戻しなし | T(core,app,cli)、F/C、history eval／**3 OS必須** |
| P0 共通抽出保存・保留 | R0,R1、保留方針判断 | ①再抽出せずstage再利用 ②unit合計256MiB超を逐次保存 ③未完成を完成扱いしない ④再起動後保留表示 ⑤明示再開で完了unit保持 | prepare出力、object_persistence、normalized reader/writer、status/watch／PDF/XLSX意味論変更なし | T(core,pipeline,app,cli)、F/C、eval／**3 OS必須** |
| P1 AD-14 PDF | P0 | ①257page/8,193object正常処理 ②tier会計exact/+1 ③循環・爆弾が有限で停止 ④unsupported filterの意味論維持 ⑤無断OCR送信ゼロ | PDF decoder、deterministic、prepare配線／Office配置・OCR承認変更なし | T(adapter,pipeline,app,cli)、F/C、eval／3 OS、P2と区切り共用可 |
| P2 AD-16 XLSX | P0 | ①旧sheet/row/column上限超成功 ②疎列・共有文字列の会計 ③previewで未処理範囲表示 ④再開でsheet/row identity保持 ⑤不正ZIP/XML拒否 | XLSX、必要なZIP資源API、prepare配線／chart画像lane追加なし | T(adapter,pipeline,app,cli)、F/C、eval／3 OS、P1と区切り共用可 |
| L1 AD-29 snapshot | R0、必要stageはR1 | ①64/128MiB超を照会 ②source/sidecar不変 ③WAL整合 ④資源不足と破損を別表示 | ledger snapshotとOS層・表示／billing authority変更なし | T(pipeline,app,cli)、F/C／**3 OS必須** |
| L2 AD-29 backup/restore | R0,R1 | ①512MiB超backup ②完全検証後のみ公開 ③source不変 ④中断・容量不足で不完全成功なし ⑤stale proof拒否 | backup/lifecycle・必要Fs API・CLI／課金履歴初期化・移行なし | T(pipeline,app,cli)、F/C、backup往復／**3 OS必須** |
| J1 AD-30 journal整理 | R0,R1 | ①旧累積上限超で継続 ②整理前後の現在状態一致 ③全phase crash回復 ④破損record拒否 ⑤登録/copy/resume整合 | task/unsupported、整理・読者／billing・grant正本の整理なし | T(core,pipeline,app,cli,eval)、F/C／**3 OS必須** |
| A1 APP-26 gen | —、P0前後でAPI調整 | ①無確認index/watchでg1 ②raw/g0/Evidence保持 ③provenance必須 ④同出力で増殖なし ⑤有料・外部権限不増 | drift経路、manifest契約、provenance producer／明示有料regenerateの確認は維持 | T(adapter,pipeline,app,cli)、F/C、eval／3 OS、Office変更前に完了 |
| E1 CLI-01 export | —、R1利用時は後 | ①scope内新規宛先成功 ②任意`.kio`拒否 ③tracked衝突をforceでも拒否 ④検査後の置換raceでも保護 | export・scope認証・直接テスト／CLI-02〜04の別制約は変更しない | T(app,cli)、F/C／**3 OS必須** |
| W1 APP-11 service | R1を使うintent API確定 | ①同一定義no-op ②replace差分＋確認 ③停止・置換・再検証 ④途中回復 ⑤foreign不変 | watch_service、3 OS scheduler、CLI／foreground watch処理・host設定変更なし | T(app,cli)、fake scheduler・障害注入、F/C／**3 OS必須** |
| O0 PROC-01 診断 | — | ①不足機構を個別表示 ②不在と検出済利用不能を区別 ③代替抽出・検索利用可 ④隔離なし起動ゼロ | process診断、Office解決結果、status/search/CLI／隔離要件は維持 | T(process,adapter,app,cli)、Linux障害注入、F/C／Linux必須＋3 OS compile/test |
| O1 AD-17 実変換probe | O0、A1 | ①隔離内fixture変換で判定 ②version形式だけで拒否しない ③実体変更拒否 ④時間・出力上限維持 | Office共通probe・結果型／runtime公開範囲拡大なし | T(adapter,process,app,cli)、F/C／**3 OS実renderer確認必須** |
| O2 AD-17 Linux closure | O1 | ①検証可能なdistro/TDF/portableを受理 ②closure内symlink許可 ③外escape拒否 ④不明配置は理由表示 | Linux package/runtime解決／未検証directory追加・sudoなし | T(adapter,process)、Linux fixture、F/C／Linux実動作＋3 OS回帰 |
| O3 AD-17 macOS/Windows | O1 | ①有効署名と実コードbinding ②vendor固定除去 ③runtime closure外拒否 ④native実変換成功 | macOS署名/runtime、Windows必要配置／CI配布物SHA pin・隔離維持 | WSLで共通T・F/C、nativeはCI／**macOS/Windows必須** |

元の [§3.2:38](../tasks/v1-plan.md) に対する評価は次のとおりです。

| 元の単位 | 評価・置換案 |
|---|---|
| W1：ST05/ST04/ST06 | 最大の過大単位。R0/R1＋S1〜S3へ分割。原本保存と復元を別の安全性レビュー対象にする |
| W2：PDF/XLSX | P0を共通基盤として独立させ、P1/P2に分割 |
| W3：台帳/journal | 課金正本とprojectionの責務が異なる。L1/L2/J1へ分割 |
| W4：APP-26 | 独立単位は適切。ただし確認除去だけで完了にせずprovenanceを含める |
| W5：service/export | 共通の変更理由が薄い。W1/E1へ分割 |
| W6：Office/Linux診断 | O0〜O3へ分割。診断を先に通し、OSごとのtrust変更を小さく検証 |
| 全体 | 「高12件をv1に入れる判断待ち」は今回の明示決定で陳腐化 [plan:34](../tasks/v1-plan.md)。計画更新時に解消すべき。今回ファイルは変更していない |

**5. 過去記録から拾えた判断・つまずき**

以下のリンク先は指定されたarchive内の記録です。旧候補の成功を現行mainの成功とは扱っていません。

| 記録 | 得られた判断・注意 |
|---|---|
| 永続identity方式 | [r33進捗:3](/home/ttokunaga-ja/archive/mac-20261112/home/Documents/Codex/2026-10-22/kio-relax/runs/W1/report-r33-progress-ja.md:3) は「ユーザー承認の案」として物理ID＋256-bit nonce、Unix xattr／Windows EA、非対応時fallbackなしを記録。ただしWindows初期化失敗cleanupの共有競合と最終3 OS検証が未完了 [同:13](/home/ttokunaga-ja/archive/mac-20261112/home/Documents/Codex/2026-10-22/kio-relax/runs/W1/report-r33-progress-ja.md:13)。**2026-10-31の原承認文面自体は未確認** |
| 資源保留 | [回答18:7](/home/ttokunaga-ja/archive/mac-20261112/home/Documents/Codex/2026-10-22/kio-relax/runs/W1/supervisor-answer-18.md:7) は、機械状態不足は永続記録なしで次回watch再検査、入力依存不足はcontent identity＋tierの小markerと決定。新Task種別を作らない |
| PDF/XLSX統合 | [D進捗:45](/home/ttokunaga-ja/archive/mac-20261112/home/Documents/Codex/2026-10-22/kio-relax/runs/W1/W1-d-development/progress-ja.md:45) はCLIにresource_limitが出てもholdが永続化されない問題、[同:46](/home/ttokunaga-ja/archive/mac-20261112/home/Documents/Codex/2026-10-22/kio-relax/runs/W1/W1-d-development/progress-ja.md:46) はstage完了直後crashのdata loss、[同:47](/home/ttokunaga-ja/archive/mac-20261112/home/Documents/Codex/2026-10-22/kio-relax/runs/W1/W1-d-development/progress-ja.md:47) は後段256MiB制約を記録 |
| テストの信頼性 | [D進捗:40](/home/ttokunaga-ja/archive/mac-20261112/home/Documents/Codex/2026-10-22/kio-relax/runs/W1/W1-d-development/progress-ja.md:40) はstale binary、[同:43](/home/ttokunaga-ja/archive/mac-20261112/home/Documents/Codex/2026-10-22/kio-relax/runs/W1/W1-d-development/progress-ja.md:43) はCMapなしPDF fixture、[同:49](/home/ttokunaga-ja/archive/mac-20261112/home/Documents/Codex/2026-10-22/kio-relax/runs/W1/W1-d-development/progress-ja.md:49) はunit上限違反fixtureを無効と判定。旧上限超fixtureは他の契約も満たす必要がある |
| APP-26/export/service試作 | [E再統合調査:9](/home/ttokunaga-ja/archive/mac-20261112/home/Documents/Codex/2026-10-22/kio-relax/runs/W1/W1-e-development/r33-rebase-inspection-20261031-ja.md:9) に22ファイルのdraftと局所Red/Green。ただしWindows COM・最終3 OSは未確認。exportの`.kio`消失raceも別途記録 [D進捗:48](/home/ttokunaga-ja/archive/mac-20261112/home/Documents/Codex/2026-10-22/kio-relax/runs/W1/W1-d-development/progress-ja.md:48) |
| Office配置 | [回答13:5](/home/ttokunaga-ja/archive/mac-20261112/home/Documents/Codex/2026-10-22/kio-relax/runs/W1/supervisor-answer-13.md:5) は、信頼できるpackage manifest等で必要資源を確認できない非標準配置をunavailableとする判断。未知rootを隔離に足さない |

**6. 先に解決すべき設計判断・重要な不整合**

| 論点・現状 | 選択肢と推奨／判断者 |
|---|---|
| 永続identity：archiveではnonce案承認、現行atomicは物理IDのみ。現行仕様への採用状態が未確認 | **nonce案を現行契約として採用し直して実装**／曖昧stageを保全して自動回復しない、の二案。推奨は前者。統括が原判断・現行適用範囲を確認し、非対応FSの扱いが未承認なら利用者判断 |
| 無料prepareの再開：回答18の機械状態自動再検査と、現行 [04:964](../docs/04-pipeline.md) の「同条件watchで再実行しない」の関係が未整理 | 全て明示再開／**Taskの資源保留と無料prepareの機械状態再検査を区別**。推奨は後者だが、archiveを無報告で優先せず、統括が後続決定の現行性を確認して契約を揃える |
| AD-29 source不変：仕様はsource bytes/permissions不変、現実装backupは`wal_checkpoint(TRUNCATE)`してserialize [lifecycle:340](../crates/kio-pipeline/src/ledger/lifecycle.rs) | 意図的checkpointを許す仕様変更／**sourceを変更しないonline backupまたは検証済private image**。現行要件への実装不一致と判断し後者を推奨。統括が方式選定 |
| APP-26 provenance：仕様requiredだが現行manifestに存在しない | 確認除去だけ先行／**provenance・writer/reader・fixtureまで一単位**。後者を推奨。単なる小修正として見積もらない |
| journal整理方式・key | segment方式／**現在状態snapshotへのatomic compaction**。後者を推奨。taskの同一性・ownership、unsupportedの「現在」のkeyを既存consumerから確定し、未終端状態を捨てない。統括判断 |
| 抽出tierと既存入力policy | 原本サイズでskipする現行経路 [pipeline:701](../crates/kio-app/src/index/pipeline.rs) と、AdapterRunのprepared inputへ適用する仕様 [07:828](../docs/07-adapter-spec.md) に適用位置の不整合がある。tierを上げても手前で止まる可能性 | **原本archive・ローカル抽出予算・外部Adapter入力policyを明確に分ける**案を推奨。具体的にどの経路へ現在のpolicyを適用するか統括が確認。別制約を黙って無効化しない |
| Office対応範囲 | 全配置を推測受理／**検証可能なruntime closureを受理し、不明は具体理由付きunavailable**。後者を推奨。PROC-01のsystemd/cgroup維持、chart抽出対象外、tier数値は既に決まっており再決定不要 |

**7. 共通コード行数への影響**

実装方式未確定のため、以下はテストを除く製品コードの**純増減の桁感**です。既存実装を置換・削除する前提で、新旧併存は含めません。計測器はテストとplatformを別扱いにしています [code_limits:1037](../crates/kio-ci/src/code_limits.rs)。

| 変更群 | 共通コードの純増減見込み |
|---|---:|
| 資源型・tier・状態伝搬 | +400〜800行 |
| stage共通API・識別情報 | +250〜600行。xattr/EA等のOS実装は別枠 |
| raw/tree/restoreの置換 | −500〜+1,000行 |
| 共通抽出保存・hold・normalized consumer | +800〜1,500行 |
| PDF/XLSXの置換 | +500〜1,500行 |
| ledger snapshot/backup | +200〜700行 |
| task/unsupported整理 | +300〜800行 |
| gen/provenance・export | +200〜500行 |
| service更新 | +300〜700行 |
| Office共通probe・診断 | +150〜500行。OS closureは別枠 |
| **合計** | **約+2,500〜8,500行。減少より増加を見込む** |
| 提示された111,757行を基準 | 約114,257〜120,257行、106,000行を約8,257〜14,257行超過 |
| 今回の参考計測 | 既存`target/debug/kio-code-limits --root .`は**exit1、common 118,455行**。提示値と6,698行差。binaryの現行ソース対応は**未確認**のため確定値に採用しない |
| 再計測の扱い | まず現行ソースから正式計測し基準値を確定。W1だけで上限内に収まる見込みは薄い。上限確定は既存F1どおりEG2後の実測で行う [plan:65](../tasks/v1-plan.md) |

今回確認したのはソース・仕様・履歴と既存計測binaryの出力です。テスト、実renderer、3 OS動作、ピークmemory、処理時間、最終変更ファイル数は**未確認**です。調査中に他作業による作業ツリーの変更も観察しましたが、触れていません。正式受入・GPU・有料provider検証は今回実施していません。