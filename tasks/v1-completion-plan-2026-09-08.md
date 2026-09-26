# Kio v1 — 実装と自動受入を完了する計画

作成日: 2026-09-08。開始状態は `main` / `d98e0f659123c2bb7c2959f1bba13ef49137fcfa`
と既存の未commit差分。これは [承認済みM0〜M7計画](v1-implementation-plan.md) の残作業を
完了する実行計画であり、旧計画の完成範囲を縮小するものではない。
現状根拠は [実装進捗](v1-implementation-progress.md) と [Actions照合](v1-actions-coverage-2026-09-08.md)。

## 1. 今回の完成地点

**製品コード、必須自動試験、3 OS Actions、配布物受入、security検証、運用文書を完成させ、
利用者の実端末での手動確認だけを別の確認票に残す。**

| 今回完了するもの | 扱い |
|---|---|
| v1のCLI、保存形式、検索、承認、境界、自動管理、監視、変換、復旧 | 実装のTODOや互換経路を残さず完成させる |
| Windows/Linux/macOSのnative機能試験 | Actionsで必須実行。cross compileやLinux containerだけで他OSの成功にしない |
| 実Office、認証済みlocal Adapter、実Mistral/Gemini | 自動受入に含む。mock成功とは別の証跡を保存する |
| native user service | Actions runner上で実schedulerの登録から解除まで試験する |
| 配布archive、SBOM、再現build、展開binaryの操作 | 同一候補SHAに結び付けて検証する |
| 利用者の実端末での確認 | 普段使う環境の対話操作、実際のsleep/wake・ログイン/ログアウト・物理再起動、実データでの使い勝手を確認票として残す |
| GUI、cloud、共同編集、複数利用者ACL | v2/v3の範囲を維持する |
| PersonaScope/personaCorpusの研究性能評価 | v1の完了待ち条件に含めない |

手動項目を残しても、Actionsで実行可能なサービス試験や復旧試験を手動へ移さない。
全ての必須自動laneがpassした状態を「実装・自動受入完了」とする。
実端末確認の結果と、元のv1完成条件への適合判定は別に表示する。
release公開は自動受入の完了と分ける。

## 2. 工程・依存・完了条件

以下の番号は実行順序であり、既存のM0〜M7やA01〜A12を改名しない。

| 順序 | 対応 | 実装と成果物 | 終了条件 |
|---|---|---|---|
| 0. 現在の統合基盤を固定 | M0/M1 | 全WIPの対象一覧とfingerprint、app分離/format/journalの整合、要件→test→lane対応表。Windows retained handleと3 OS service/renderer環境の小さな実行試験 | 関係する未追跡ファイルも含めたbuild可能なcheckpoint。基盤回帰が成功し、未成立のOS能力に修正先がある |
| 1. 承認と管理境界を完成 | M2/M6 | 専用approve/revoke、端末私有grant、共通の送信許可判定、保持したdirectoryからの更新、旧CLI/consent分岐撤去 | clone/move/profile/宛先/trust変更・Ignore失効・並行revokeで誤許可0。previewは無変更 |
| 2. 自動管理と監視を完成 | M3/M4 | 探索の永続継続、残るpath再open処理、再登録、queue回復、native user service | 手動indexとwatchのscope集合/内容が一致。上限や再起動で未処理を失わず、境界外への変更0 |
| 3. Adapterと運用復旧を完成 | M5 | trust登録/更新/失効、中央ledgerの整合backup/復旧、converterの残る資源制限、実接続用fixture | 偽peerへの本文0、秘密の漏出0、停止後の残留processなし、不明な課金要求の自動再送0 |
| 4. 検索・復元・CLIを統合 | M1/M6 | replica再構築、画像/履歴/全scope検索、全体/選択restore、help/JSON/終了コード、error契約 | 再構築前後の結果同値、restoreはcurrent HEADの唯一の子、未選択/未管理/現行policy保持 |
| 5. 必須Actionsを接続 | M0/M7 | 通常native、実Office、実service、実provider、配布binaryの受入runnerと必須receipt集約 | 各laneが実処理を行い、missing/skip/未準備/別候補の証跡を拒否する |
| 6. securityと文書を閉じる | M7 | 最終のsecurity検証、是正と回帰、操作/復旧/制約の文書、packaging整合 | v1を侵害する既知の未解決指摘・未実装要件なし。文書・CLI・実挙動が一致 |
| 7. 同一候補で自動受入を確定 | M7 | 最終SHAで全必須laneを実行、receipt検証、配布物hashと試験結果、実端末確認票 | 必須自動ケースが全pass。修正が入れば新SHAで必要な証跡を取り直し、最終候補へそろえる |

依存は 0→1→2、1→3、1/2/3→4、4/5/6→7。
工程3のledger/converterと工程5のfixture/runnerは、担当ファイルを分けて先行できる。
3 OSの方式の成立確認は工程0から始め、最後までWindowsやschedulerの問題を放置しない。

## 3. 先に固定する実装判断

### 承認・端末状態

- `adapter approve` / `adapter revoke` を永続送信承認の唯一の変更経路にする。
  状態照会とpreviewを用意し、対象scope、Adapter、宛先、処理profile、trust identityを確認できるようにする。
  引数とselector排他を `docs/06-cli-spec.md` に確定してから実装する。
- `index --approve` / `--revoke-network` と旧 `consents.jsonl` の許可fallbackを撤去する。
  `--online`は今回のremote処理選択、`--yes`は明示操作の非対話確認に限定する。
  config boolean、pending、scopeのコピーや履歴から承認を作り直さない。
- local管理許可と外部送信許可を分ける。新しい子に管理領域を作っても送信承認を与えない。
- portableな知識は `.kio`、端末の実行許可とcredential/trustは中央の私有領域に置く。
  scopeの現行approval参照と端末grantの両方を照合し、片方だけで送信できないようにする。
  Windows DACL、Unix permission、macOS ACLとretained handleで実ファイルを検査する。
- 複数ファイルへの承認公開は一括atomicとみなさない。端末側の未有効intent、scope側の対応参照、
  端末側の有効化という段階を持たせ、両方が一致して初めて許可する。revokeは端末grantの失効を
  先に確定し、scope側の後処理が止まっても新規送信を止める。中断したapproveは明示操作でのみ
  再開でき、revoke済みのintentを再有効化しない。各段階の停止とlock取得順の競合を試験する。
- grantはroot/scopeの現在の所属、処理profile、宛先、credentialの参照先、trust世代へ結び付ける。
  credential値を共有metadata・hash入力・ログへ入れない。出力再利用のprofileと実行許可identityを分ける。
- scopeのコピー・移動で旧grantを暗黙に再利用しない。端末の完全な私有状態まで操作できる
  同一OSアカウントの攻撃者を防げるとは主張しない。任意のdevice IDを物理端末の認証と扱わない。
- 送信開始とrevokeの順序を共通のauthority boundaryで決める。OCR、query/document embedding、
  rerank、batch upload/job作成などの実際の送信経路を漏れなく移行する。
  送信許可前の失効は拒否し、送信開始済みの本文は取り戻せないという境界を明記する。
- 管理rootを一度検査した後にpathから別directoryを開き直す更新をなくす。
  承認更新APIは呼出元が保持したdirectoryとlockを使い、差し替え競合を終端まで拒否する。

### 探索・監視

- 現在の512 directory・深さ32の打切りを、フォルダを永久に取り込まない条件として残さない。
  1回の処理量を制限しながら、永続frontierと再検証で未処理範囲を完了まで進める。
  深さによる再帰依存を減らし、OSが対応するpathの限界と未対応状態を明示する。
- OS通知は変更候補として利用し、起動・手動index・定期・overflow回復を共通処理へ統合する。
  size/mtimeが同じ変更も照合で回復させ、queue上限時も新しい変更を捨てない。
- 再開時にはdirectory identityと最新Ignoreを再確認する。stale cursorやregistryは権限に使わない。
  independentな既存scope、VCS、mount、symlink/junction、case/Unicode、移動/削除/再登録を試験する。
- Windowsのhandle-based操作とread barrierをnative runnerで確認する。
  root外のfile/directoryに対する作成・置換・削除がないことをfixtureのhashで確認する。
- user serviceの登録/起動/停止/status/再起動/解除はforegroundと同じapp処理を使う。
  schedulerへ渡す実行ファイル、環境、manifest、rootのidentityを検証する。

### 台帳・trust・変換

- WALを含めた一貫したbackupを、検証済みのledger connectionとlifecycle lockから取得する。
  DBだけでなくauthority/era/sequenceとmanifestを結び付け、途中backupを完成品として公開しない。
- 復旧は初期化と分けた明示操作にする。pending intent、reservation、job/request identity、
  結果不明の状態を保持する。provider一覧だけで失われた課金履歴を再構成したと扱わない。
  checkpointだけ新しい場合、古いbackup、完全な履歴喪失、scopeだけの復元を個別に試験する。
- 外部の記録なしでは判別できない履歴喪失は未解決として表示し、課金再開を許可しない。
  復旧時に架空の残高や成功結果を作らない。運用者が用意すべき証拠と照合手順を文書化する。
- local trustは利用者が明示指定した認証材料を端末へ登録する。初回接続先の証明書を自動信頼しない。
  rotation/失効時は関連grantを再検証し、古いtrustで送信しない。再起動後の正規利用も確認する。
- Unixで残るprocess treeとscratch総量の制限を完成させる。既存macOSクラッシュ修正、
  Windows Job Object、Linux confinementも実rendererで確認する。
  kernelによる強制上限と観測による停止を区別し、未対応時に無制限で続行しない。

### 履歴・復元・CLI

- 新format、単一parent/HEAD、publication journalを維持する。旧形式・旧コマンドの互換aliasを残さない。
- `restore`は過去のraw内容を採用したcurrent HEADの新しい子を作る。復元元はprovenanceに保存し、第二parentにしない。
  全体/選択の両方でdirty、HEAD競合、purged、未選択/未管理ファイルを検査する。
  復元元にないpathの削除は明示指定時だけ行い、複数scopeを跨ぐatomicな復元は約束しない。
- `.kio`、Ignore、config、consent/grant、課金状態は復元対象にしない。
  SQLite/replicaは再構築可能な派生情報とし、更新遅延時にも現行policyで検索を絞る。
- read-only操作はlock fileの新設、暗黙修復、台帳初期化、外部送信を行わない。
  help、JSON schema、終了コード、selector排他、offline、予算月境界を共通契約で確認する。

## 4. Actionsと証跡

既存の通常CIとdraft packagingを拡張し、共通のRust受入runnerを `kio-eval` 側に置く。
受入runnerは対象binaryの絶対pathを受け取り、製品処理を再実装せずCLIを操作して検証する。
unit/contract laneと展開release binary laneの証跡を区別する。

| lane | OS | 必須内容 |
|---|---|---|
| 通常CI | Windows/Linux/macOS | 全workspace回帰。Linuxのfmt/clippy・既存合成機能E2Eを維持 |
| 実Office/変換 | 3 OS | rendererの導入、version/identity、必須実行、DOCX/PPTX→PDF、PDF/image/表形式の次段処理と拒否 |
| native service | 3 OS | 実schedulerの登録→起動→変更反映→停止→停止中変更→再起動回復→解除、常駐残留なし |
| 実provider/local | 3 OS | Mistral/Gemini/localの実処理、batch/realtime/query、usage/provenance。mockによる故障契約は別に実行 |
| 配布物 | 3 OS | 同じSHAから2回の再現build、archive/hash/SBOM、展開binaryによるfresh環境の受入 |
| 集約 | 全必須receipt | A01〜A12の必要なOS/lane集合と対応。未実行・失敗・欠落・環境未準備を成功にしない |

receiptには少なくともcase ID、lane、OS/arch、candidate SHA、binary hash、fixture hash、
tool/profile identity、workflow実装SHA、run ID/attempt、結果を記録する。
検証器も候補に結び付け、自己申告の `passed` だけを信頼しない。
最終集約の期待case集合は実行結果から生成せず、固定した要件表から得る。
workflow/job失敗時も診断を保存し、集約job自体がskipして成功相当に見えないようにする。

runnerのservice session、OS権限、confinementは初期にpreflightする。
GitHub-hostedで成立しない必須機能は、通常processで代用せず専用の隔離されたself-hosted Actionsで実行する。
その環境が未準備なら該当自動laneは未完了である。利用者の実端末を無断でrunner化しない。
実際のsleep/wake・物理再起動の手動確認を残す場合も、停止中変更・再起動相当の自動回復試験は必須とする。

実provider workflowは通常pushから起動しない。公開可能な小さいfixtureと、候補SHA、要求数上限、
全体予算、各jobへの配分を確定した実行だけを行う。配分合計は全体予算以下とする。
credentialはprotected環境に置き、本文・credentialを公開artifactへ保存しない。
workflow/jobを再実行して未解決の課金要求を重複させない。run/attemptの扱いと再開手順を試験する。
実APIのcredentialと支出上限が未準備なら、harness完成とは別に「実接続未受入」を残す。
この外部依存を手動実機確認に分類して完了扱いにはしない。

## 5. 検証・commit・担当

- 主担当が要求、authority/保存形式、共通app API、CLIの最終仕様、Cargo lock、統合、完了判定を持つ。
  利用可能ならboundedな実装/レビューはTerra、探索/既知検証はLunaへ分ける。
  securityは指定済みCodex Security skillとDaybreak Blueによる境界検証を使う。
  同じ問題の重複監査は避け、最終候補の差分と未検証範囲に集中する。
- 作業分担はcoreのdirectory/承認API、appのgrant/send gate、scan/watch、ledger、adapter/process、
  eval/workflowで所有ファイルを分ける。共通境界を決める前に複数workerへ同じコードを渡さない。
  補助エージェントが利用できない場合も、主担当が同じ工程を継続する。
- 既存WIPは破棄しない。`main`で関連ファイルを明示選択し、build可能な論理単位でcommitする。
  基盤分離と保存形式のように不可分な変更は一つの整合した単位とする。無関係な未追跡ファイルは含めない。
- 各push前に発火workflowと最終diffを確認し、現行CIの安全で再現可能なローカル手順を通す。
  Rust 1.98.0、lockfile、flagsをそろえ、fmt→clippy→workspace tests→release build→合成機能E2Eを確認する。
  失敗があれば原因と影響範囲を修正してからpushする。
- Actionsはnative最終確認に使う。失敗jobの全ログ/annotationsを調べ、可能な再現と関連修正をまとめる。
  原因のない再実行や、試験を無効にして緑にする変更は行わない。
- security指摘は妥当性、修正、回帰試験を対応付ける。最終SHAの再確認後に未解決件数を判定する。
  privateな脆弱性詳細やcredentialを公開文書に混ぜない。

## 6. 完了報告の内容

1. M0〜M7の実装・自動受入の終了表と、要件A01〜A12からtest/receiptへの対応。
2. 最終candidate SHA、3 OS ActionsのURL、配布archive/binary hash、検証結果。
3. dedicated approval、root管理、watch service、restore/export、ledger backup/recovery、local trustの操作文書。
4. 必須自動試験のskip/missingがないこと、既知の未解決security問題・機能欠落がないことの判定。
5. 利用者の実端末で行う確認票。OS、前提、操作、期待結果、記録欄を記載し、完了済み自動試験と混ぜない。

当面の着手単位は工程0のcheckpoint/対応表と、工程1のretained承認API・専用CLI・端末grantの統合。
工程5の受入runnerとOS環境preflightを並行準備する。本書の作成を実装・Actions実行の完了とは数えない。
