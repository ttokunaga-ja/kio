# Kio v1 closeout — 実行記録

2026-10-03 JST。[承認済み計画](v1-acceptance-closeout-plan-2026-10-03.md) のC0/C1/C2から着手した。
実装、ローカル検証、3 OS Actions受入、手動受入、release公開を別々に判定する。
**v1.0の受入はまだ完了していない。** 最終候補の固定とC3以降の証跡はこの開始記録から推定しない。

## 1. 開始基準とWIP保護

- 開始HEAD: `436ca21fa1da5d27daecc33e2e18d3f3a3d8d079`、local main。開始時のtracked/staged差分なし。
- 既存の未追跡132ファイル、合計3,356,308 bytesは今回の変更対象に含めない。個別のpath/mode/size/hashと
  HEADのtracked treeをrepo外の `kio-v1-closeout/start-20261003T011043Z` に保存した。
- GitHub main: `1ffdd0ca1bc9fba9ec0a5b0b8c58302daac80545`。RC.3の配布元は `1df10f2`。
  開始HEAD、公開main、配布版を同一候補として扱わない。
- 最終workspace試験・package・securityは、変更が止まった候補で実行する。
  以前の `68690960` 全回帰/scanと後続orphan-pruneの部分試験は履歴の証拠である。

## 2. C1: tag lifecycle

`Repository::delete_tag`、app dispatcher、CLI `tag --delete` を接続した。
名前検証を作成と共有し、scope lock・retained directory・canonical refの正確なbytesに基づきrefのみを削除する。
名前の監査台帳、HEAD、commit/tree/raw/chunk、他tagを保持する。対象commitのCAS欠落は解除を妨げない。
存在しないtagはexit 4、削除とcommit operandの併用はexit 2。作成時のJSONは従来どおりである。

削除途中のatomic intentはrefを読む前に回復する。ready/quarantined/deletedの3停止点からの再試行は
残存intentを片付け、既に解除されたtagにnot-foundを返す。新しい操作IDやtag retarget機能は追加しない。

| 回帰・受入 | 検証する内容 | 開始時点の実証範囲 |
|---|---|---|
| core `delete_tag` unit群 | lifecycle/監査/lock/namespace/破損ref/欠落CAS、実GC plannerのtag解除 | macOSで5件pass |
| core `tag_atomic_crash` | 3停止点からの回復、HEAD/監査/他ref不変、同名再作成 | macOSで2件pass（self-helper含む） |
| CLI `tag_lifecycle` | JSON・exit、作成/削除/再作成、CAS/他tag/HEADの保持 | macOSで2件pass |
| eval A09 public GC | 配布binaryでtag保護→解除→同名再作成→解除→実GC。Unicode sigma aliasesも使用 | runnerへ接続済み。最終packageでの実行は未完了 |
| A10/shared atomic protocol | bounded intent、対象差替え拒否、停止後回復 | 既存試験を維持。最終候補の全回帰は別途必要 |

### 実行中に承認されたUnicode是正

レビューで、現行tag名比較が仕様のsimple case foldingに代えて小文字化を使い、未割当文字も
受け付けている差を発見した。利用者は2026-10-03に **Unicode修正と保存形式更新を今回へ含める** と承認した。
この追加承認は、元のC1「保存形式を増やさない」の範囲を更新する。

是正契約はNFC→locale非依存のsimple case folding、同梱UCD16.0.0の未割当tag文字拒否である。
full foldingやTurkic foldingは使わない。保存形式を `3.0.0` に更新し、旧 `2.0.0` を含むnon-current storeは
schema検査やmutationより先にexit 8で拒否する。旧storeのbytesは変更せず、移行/旧規則fallbackは設けない。
tagのphysical namespaceとJSON論理hash形式は維持する。GUIを含む製品v3の実装を意味しない。
Unicode回帰と旧形式不変の検証を含む是正完了後にC3候補を固定する。

是正はcore・schema・fsckへ接続済み。Unicode tag統合試験、foldingのUCD一致・sigma/Cherokee/
sharp-S/dotted-I/NFC比較、未割当文字の拒否、旧2.0.0拒否時のbytes不変、fsckのsigma解決が局所passした。
厳密なcore全target Clippyとapp checkもpass。最終workspaceと配布物受入の成功はまだ別途必要である。

## 3. 要件→試験→laneの対応

機能要件の正本は [docs/11](../docs/11-product-requirements.md)、固定matrixの正本は
`crates/kio-eval/src/acceptance.rs::fixed_v1_requirements()`。以下の各集合を3 OSで必須とする。
局所テストのpass数と51件の必須receipt数を混同しない。

| 要件 | 実装/受入runnerの主な所在 | 必須lane / 各OS件数 | 今回の残差 |
|---|---|---|---|
| A01 初回offline/read-only | app CLI、`acceptance.rs::run_a01` | native/core 1 | 最終binaryで再検証 |
| A02 authority/copy/move | core管理/approval、`acceptance_native.rs` | native/core 1 | 最終binaryで再検証 |
| A03 change/watch | app watch、`acceptance_native.rs`・`acceptance_service.rs` | native/core + service 2 | 実schedulerを含む同一候補受入 |
| A04 policy/trust | app approve/revoke、`acceptance_policy.rs`・`acceptance_local_trust.rs` | native/core + local-trust 2 | 最終binary・trust拒否を再検証 |
| A05 search/history/image | search/index、`acceptance.rs::run_a05` | native/core 1 | 最終binaryで再検証 |
| A06 projection rebuild | app repair/index、`acceptance.rs::run_a06` | native/core 1 | 保存/GC/prune後の再構築を再検証 |
| A07 実Office/画像 | adapter/process、`acceptance_office.rs` | office-real 1 | 3 OS実converter・confinement |
| A08 provider/failure | pipeline/ledger/adapter、`acceptance_failure.rs`・`acceptance_provider.rs`・`acceptance_authenticated_local.rs` | mock + Mistral + Gemini + authenticated-local 4 | mockはlocal、liveはpackage確定後 |
| A09 restore/GC | core restore/GC/tag、`acceptance.rs::run_a09` | native/core 1 | tag解除を接続し、最終binaryで実GC |
| A10 interrupted recovery | core/pipeline journals、`acceptance_fault.rs` | native/core 1 | release + debug contract binaryを候補へ束縛 |
| A11 service lifecycle | app service、`acceptance_service.rs` | service 1 | 実schedulerの登録/反映/停止/解除 |
| A12 distribution | release tooling、`acceptance_distribution.rs` | distribution 1 | 2回archive一致、展開binary/SBOM/lock |

各OS17件、全51件。native supplementは42件、cloud providerは6件、authenticated localは3件。
CI成功はmatrixと別に必須。現在の候補の必須Actions receiptはまだない。

## 4. 既存CI失敗との対応

[CI 36287581490](https://github.com/ttokunaga-ja/kio/actions/runs/36287581490) の全失敗jobと
修正commitを読取り照合した。修正の存在と最終OS受入を分ける。

| 失敗 | ローカル履歴の是正 | 残る確認 |
|---|---|---|
| macOS SDKのhardlink拒否 | `8d173311` のSDK detach/permission normalization | hosted macOSのSDK準備と最終workspace |
| Windows strict Clippy | `2f43c33b` のWindows-only lint是正 | Windows target Clippyとnative runtime |
| Linux bwrap loopback EPERM/setup | `94dcea9a` のuser scope/cgroup preflight、`c69fdb57` resource boundary、`b45ec138` Office preset | Ubuntu 24.04/systemd255/AppArmorの実enforcementとOffice |

当該runのacceptance-toolingとPersona W0の成功はそのSHAの結果である。
synthetic-historyは依存job失敗で未実行。最終候補でローカル相当を再実行する。
hosted runner用provisioningを利用者のpersistent hostへ適用しない。

## 5. C2のread-only確認

2026-10-03にGitHub metadataと既存SSH routeを確認した。外部設定、service起動、provider送信は行っていない。

| 前提 | 確認できた状態 | 未実証の部分 |
|---|---|---|
| provider設定 | provider/authority Environmentと期待する変数・secretの名前が存在 | 値の妥当性/App token runtime/paid呼出 |
| provider ledger | ledger branch APIは404、provider workflow runは0件 | campaign bootstrap/reservationは未初期化・未実証 |
| WSL lab | 既存routeでBatchMode SSH成功、kio-sshd active、Docker29.8.1、RTX4060 8188MiB | Actions OIDC/Tailscale→専用accountへのend-to-end接続 |
| Windows runtime | 以前のSmart App Control拒否記録を再確認 | fresh native実行。制限を迂回しない |
| public candidate | main `1ffdd0ca`、そのCIの3job失敗 | 新候補push/通常CI/native42件/provider6件/local3件/最終verifier |

providerの既存承認額・累積ledger・未知結果の扱いは [budget契約](../scripts/v1-provider-budget/README.md) を維持する。
routeの具体差分は [private route計画](v1-private-actions-route.md) をlive確認と照合する。
値の存在/可用性を名前一覧から推定しない。環境の応答をOffice/service/provider受入成功と扱わない。

## 6. C3以降の進め方と判定

tag/Unicode是正と関連文書をlocal commitし、tracked sourceのhashを固定する。
Rust1.98.0/lockedでfmt→strict workspace Clippy→all-target workspace test→release build、
shell/action-pin、Persona W0、synthetic-history、2回packageと配布binary受入を実行する。
最終差分securityは既存指定のCodex Security/Daybreak Blueで行い、範囲・要求モデル・確認できた実体を記録する。
native Windows/hosted setup、実scheduler、live provider/localの未実行gateをpassにしない。

実行ログ、候補SHA、archive/binary hash、source hash不変確認はrepo外に保存し、途中の失敗ログも残す。
外部設定変更とpush/dispatchの承認範囲を確認してからC4へ進む。C4〜C6中のmain凍結、
修正時の候補再束縛、51件verifier、手動受入とrelease公開の別判定は元の計画に従う。

## 7. 初回C3候補と是正

初回候補は `3494f492bb1df4b3f60ad633183a29bbf5f5809c`。
tag削除・Unicode是正・保存形式3.0.0と関連文書をmainへlocal commitした。pushはまだ行っていない。
以下はこのSHAの結果であり、是正後の候補へ流用しない。

| 検証 | 結果と残差 |
|---|---|
| macOS Rust 1.98 | fmt・strict Clippy・release workspace・shell/action-pin・Persona W0はpass。workspace testは履歴収集テスト1件がfail |
| native WSL Rust 1.98 | 同じ履歴テスト1件がfail。fmt・strict Clippy・release・Persona W0、CI相当umaskでadapter 364件はpass |
| macOS arm64配布物 | 隔離した2回build/packageのarchive・binary hashが一致し、release verify・展開・smokeはpass |
| macOS配布binary受入 | 試行した11件のうち10件pass、A02 native watchは収束待ちでfail。A09のUnicode tag解除/再作成/実GCはpass |
| Linux synthetic-history | WSLの証拠ディレクトリへの書込みがEROFSとなり開始できず。その後SSHも拒否。hostの再起動/remount/設定変更は行っていない |
| macOS synthetic-history | descriptor-bound executionを要求するscale/replayはplatform非対応。Linux laneの成功へ代替しない |
| Office/service/3 OS受入 | 実Officeの配布元一致、実scheduler、Windows runtime、hosted enforcementは未受入。ローカル診断receiptをActions receiptへ数えない |

macOS archive SHA-256は `389c593d1d99e6669e9c62ca73ff288ac65edb24c3b84ce9f551cc6fc5dc291c`、
binaryは `5fb1fc2e247b858b8c8fec14aad6514ba093959375139b53e4557a0843aeba2e`。
失敗ログ・watch queue/status・配布物・実行環境はrepo外の開始証拠ディレクトリへ保存した。

履歴テストは、実装がHEAD ancestryを検証するのに対し、callerのin-memory introductionだけを書き換えていた。
実際のpost-purge commitへ欠損NormalizeRefを保存して拒否を検証するfixtureへ修正し、
未説明の欠損・正当な旧owner例外・残存破損の拒否を維持した。対象テスト1件は局所passした。

A02は1秒間隔のperiodic full scanでbacklogが補充され、正常なwatcherでもidle条件を満たせなかった。
native通知とstartupを検証するこのlegのperiodic間隔を600秒へ変更し、timeout診断を追加した。
backlogゼロ・新しい成功時刻・非degraded・fixture/binary hash・停止後manifest比較は維持する。
製品watcherのdefaultや、A03の独立したperiodic回復検証は変更しない。関連unit 17件は局所passした。
これら2件を含む新候補でworkspaceとpackage受入を取り直す。

security差分検証は `68690960..3494f492` の23 production pathsを完了し、確認されたfindingは0件。
Daybreak Blueを指定したCLI出力とworkbenchのsealed reportを保存した。実際のbackendは独立に確認できていない。
追加是正の差分は新候補へ固定して別途検証する。

## 8. C2の追加readback

利用者のTailscaleログインとGitHub再認証後、consoleを読取り確認した。
Tailscaleのactive policyは全source→全destinationの全IP許可と既存SSH checkであり、CI tag/OIDC credentialはない。
限定policyは提案ファイル、OIDC scopeは未保存formである。新規grantやcredential生成はまだ行っていない。

既存GitHub App `kio-provider-ledger` はowner `ttokunaga-ja`、App ID `5091330`、
installation `165299841`。対象は選択された `ttokunaga-ja/kio` 1 repository、Contents read/writeと
Metadata readのみである。新しいApp権限は不要。private keyの存在確認は値の妥当性やruntime token成功を証明しない。
ledgerの2 rulesetは予定したimmutable-historyとApp-only writerに一致するが、campaignは未初期化である。

Windows専用accountとSSH forward制限、WSLのstream-local forward拒否の具体差分をrepo外へ生成し、
一時configでparser検証した。稼働設定の変更やservice reloadは行っていない。
Tailscale policy/OIDC、専用key/account、Environmentへの設定適用は、具体差分の承認と
fresh状態確認の後に実施する。readback、parser成功、接続/拒否/cleanup、GPU runtimeは別の判定である。

## 9. 次候補に向けたC3再検証と待機上限の是正

`9a37b0f13af73bd89735a4c38c97892f08aeb7ce` は履歴fixtureとnative watcherのperiodic間隔を
是正した候補。mainへlocal commitし、元からあった132件の未追跡WIPはpath・内容・metadataを保持した。
この候補もpushしておらず、以下は最終受入の代替ではない。

| 検証 | 結果と扱い |
|---|---|
| macOS全体回帰 | app 321件はpass。evalは493件pass・ACL 3件fail。検証用のminimal environmentで標準USERを除外したことが原因。実UID由来のUSER/LOGNAMEを渡すと3件の個別再試験はpass。全体成功とはまだ数えない |
| macOSその他CI相当 | fmt・strict Clippy・release workspace・shell/action-pin・Persona W0はpass |
| macOS配布物再生成 | 隔離した2回のpackage hashが一致。verify・展開・smokeはpass。archive `d13b47ab20096f727f43141646e671b535a7a71211a6bd23ae5fc3fbeaae0e4c`、binary `bb55fd1c414976be1954e068f5b2019ab5d65c240f96b7b1de6110328fbceeb4` |
| macOS native診断 | 10件pass、A02はnative full scanのbacklogが残り120秒でfail。periodic間隔600秒への修正だけでは足りなかった。失敗記録とqueueを保存 |
| Docker Linux回帰 | 固定候補のfmt・strict Clippy・shell/action-pin・Persona W0はpass。全体試験は検証containerのmemory上限、次いで12 GiB tmpfs満杯によるlinker停止。製品assertion失敗とは判定しないが全体passでもない。容量を調整した隔離環境で再検証する |
| security追加差分 | `3494f492..9a37b0f1` の変更source 2件と文書2件を検証し、sealed reportの確認findingは0件。要求したDaybreak Blueの実backend独立確認は未成立のまま |

A02の保存記録を照合すると、41件のauto commitは39スコープの現HEADと置き換わったルートの2件に
対応する。既存childを毎回再登録する実装経路はなく、繰り返し同一treeのcommitを作った証拠でもない。
初回登録のcontrol-file通知による後続走査と整合するが、保存ログだけでOSイベントの全path/kindは断定できない。

そこで、隔離cloneのevaluatorだけに変更後の待機上限300秒を与えたcreate-only診断を1回実行した。
元の候補binaryを使用し、初期/停止120秒、periodic600秒、running・新しい成功時刻・backlogゼロ・
非degraded・各pollのinput hash・境界確認・停止後の厳密manifest比較をすべて維持した。
初期成功から約229秒後に空queueへ収束し、全体381秒で比較もpassした。
このsource overlayの結果を候補束縛されたA02 receiptとして扱わない。

次の是正はA02の変更後待機だけを300秒にする。製品watcherのdefault、初期/停止上限、A03回復、
既存の合否predicateは変更しない。正式な新候補で全体回帰とpackage/nativeの必要な検証を取り直す。
各候補の失敗・環境補正・診断・受入の4状態を分けて保存する。

C2ではWindows C:が約11 MiBまで不足し、WSLはuser lookup errorの後Stoppedと観測された。
停止原因は未確定で、こちらからshutdown/restart/remountは実行していない。固定volumeはC:だけであり、
TEMP調査には安全な容量回収対象を特定できなかった。WSL/DockerのVHDXと研究WIPは保存する。
host healthと空き容量の条件が成立するまで、SSH route本適用とGPU試験を進めない。

新規Tailscale policy/OIDCとGitHub Environmentの具体案はrepo外の確認packetへ整理した。
Mac→WindowsのSSHを保持する一方、既存wildcardの削除はWindows→Mac/RDP等に影響する。
新しい永続credentialの保存前の承認を待ち、設定完了・runtime・最終51件受入は別々に判定する。

## 10. a8bb376候補の回帰結果と再開確認

`a8bb3762d895e99344b25a532de58b3bc675114c` のmacOS全体回帰は、標準USER/LOGNAMEを
実UIDから渡した環境でpassした。fmt、strict Clippy、workspace全target test、release build、
shell/action-pin、Persona W0もpass。eval libは496件成功で、132件の元のWIPも不変だった。

同じ候補の隔離した2回packageはarchive hash
`e6f56e00ddfdb02de373daac1fc27ec95fa8663d1e1f23244406adee027e7b0f` が一致し、
展開binaryは `0b5ec7633cd796fd0264c076507c247745dee25d6baf218fa01d19e7792c7831`。
verify・smoke・A12配布はpass。実binaryの10 case/subcase中7件pass・3件failであり、
A02は正式な候補evaluatorで309秒の全工程を完了した。A09のUnicode tag削除・再作成と実GCもpass。
これらはローカル診断IDの証拠であり、必須Actions receiptやv1全体受入とは数えない。

残る失敗を保存記録とsourceで切り分けた。

- A03: 1秒のperiodic回復が終わった後、idle検査がreconcile_onceを呼び、次のperiodicを自ら投入していた。
  保存queueは空。通知欠落後の実回復・新しい呼出・最終内容照合を維持し、この区間だけ
  新規dispatchなしのqueueゼロ・非degraded・呼出回数安定を検査する。
- A06: concurrent CLIのregistry SHMがsource stat/open間で変化し、直ちにunsafeで停止した。
  regular sourceのidentity/size driftを既存のbounded retryへ分類する。unsafe leafとprivate copyの
  照合拒否は保持し、CLI/運用/Evidence Pointer文書もこの区別へ整合する。
- A10: debug barrierが最終ready名を作成した後、payloadを書き込む前にreaderが0 byteを観測した。
  完全payloadを同期した同directoryのpendingからcreate-onlyでready名を公開する。
  point/PIDの厳密検査は維持し、既存readyを上書きしない。

是正ごとに原因に対応する回帰を追加し、新候補の必要な検証を実行する。失敗の無変更再試行や、
合否条件の削減では閉じない。修正前の候補の成功を新候補の証明へ読み替えない。

利用者が不要データを削除し、再開可能と連絡した後、2026-10-03 13:30 JSTのread-only確認で
Windows C:の空きは383,960,043,520 bytes（約357.7 GiB）へ回復した。Ubuntu/Docker WSLはRunning、
既存SSH接続は成功、guest ext4はrw、kio-test home/repoは書込み可能なmetadataだった。
kio-sshd・Docker 29.8.1・RTX 4060のmetadata確認も成功した。
KioLabWslKeepaliveはReadyで、最後の結果は0xC000013A。終了理由は断定せず、常駐の将来継続を
このsnapshotだけで保証しない。こちらから削除・修復・再起動・GPU workloadは行っていない。

容量と既存SSHの停止条件は解消した。新規Tailscale/OIDC/Environmentの適用承認、専用SSH経路の
本適用、実GPU/service受入と51件verifierは引き続き別の終了条件である。

## 11. 追加是正の対象検証と外部設定の適用

本差分の対象検証ではregistry lib 43件、durability lib 4件がpassした。
新しいA03回帰は最初にfixture tempdirのowner-private権限不足で起動前にfailしたため、
既存のprivate_dirを適用してproductionと同じ前提へ揃えた。修正後のnative evaluator検査は
18件passし、fmt、core release testのno-runコンパイル、strict workspace Clippyもpassした。
原失敗と再検証の記録を保持し、132件の元のWIPはsymlinkを含めて不変だった。
独立したGPT-6.1 Solのregistry source reviewでも追加是正は見つからなかったが、
これはWindows runtimeや正式受入、最終差分securityの完了を代替しない。

利用者はTailscale policy/OIDC/GitHub Environmentの具体的な3項目を承認した。
2026-10-03 14:05 JSTまでに、policyを保存してserver整形後の全文を再読し、提案とのJSON内容一致を
確認した。既存Mac→Windows→WSLのstrict SSH接続も成功した。OIDC credentialを作成し、
保存済みissuer/subjectと4つのexact claims、Auth Keys scope、tag:kio-ciを読み戻した。
GitHub v1-local-acceptance Environmentはmain branchだけのpolicy、reviewersなし、指定variable5件と
専用SSH secret1件で作成し、API/UIで検証した。Windows/WSLのpublic host keyは既存strict SSH pinと
照合してから登録した。private keyの本文やdigestは文書・repo・ログに保存していない。

これらは設定適用の証拠であり、Actions OIDC接続・専用SSH account/key/configの本適用・GPU workload・
必須51件受入の成功を意味しない。今回承認された3項目にhost側の専用account/SSH本適用は含めない。

## 12. e837020の全体検証とWSL Rust整備、Windowsの追加是正

`e83702085792737e2cde4400d1d52cdc9a8fc41d` のmacOS CI相当15 gateは成功した。
workspace全target試験、strict Clippy、release build、shell/action-pin、Persona W0を含む。
同じ候補のmacOS archiveを2回生成して一致を確認し、展開binaryの11件のlocal診断も成功した。
A03-native/A06/A10の元の失敗はこの候補で解消したが、local run ID `900000007` は正式Actions receiptではない。

利用者の追加依頼に基づき、WSLの既存`kio-test`へ公式Rustup 1.29.1と標準proxyを導入し、
既存Rust/Cargo 1.98.0、rustfmt、Clippyを保持した。変更前のprofile/bashrcとhashを保存し、
login/interactive Bashおよび主担当のfresh SSHで検証した。root/system/SSH/他ユーザー設定は変更していない。
この整備後、e837020のWSL全体試験、strict Clippy、release build、Persona W0、
synthetic-history CI sequenceとrelease helperのroute preflightが成功した。
元のnested Cargo失敗は環境差として保持し、製品sourceの変更や検査削減で処理していない。
WSLのAppArmor無効、Ubuntu 26.04とhosted Ubuntu 24.04、Mac SDK27とCI SDK26.5の差は残る。

WindowsにもRust 1.98.0/MSVC BuildToolsがあることを実機確認し、owner-privateの専用source/cache/targetと
process限定developer環境でCI相当を開始した。既存default GNU toolchainは変更していない。
fmtはexit0で成功したが、strict Clippyはledger snapshotの未使用関数2件とWindowsの未使用method1件で
実コード失敗を示した。full raw stderrを保持した。PowerShell wrapperがchild終了後にhangしたため
Cargo process exitは未知だが、compile診断の失敗とは別に記録する。残存cargo/rustcがないことを確認し、
今回のowned wrapperだけを終了した。full test/releaseはこの失敗で停止し、未実行である。

追加是正は、Unix側からだけ呼ばれる`verify_private_leaf`/`verify_private_snapshot`へ`cfg(unix)`を付け、
呼出元のないWindowsの`capability()` clone wrapperを除く。Windowsのretained directory handle、
owner DACL、pre-SQLite leaf/manifest検証は維持する。macOSのpipeline全target strict Clippyと
ledger snapshot関連25件は局所passした。

その後のWindows診断では、wrapperをactual exit 0/7のsmoke付き`.cmd`へ置換し、各変更後に
strict Clippyを1回ずつ実行した。Cargoが次のcrate/test targetへ進むごとに露出した失敗とexit101を
別々に保存した。補助関数・importは既存callerのUnix/Linux/macOS条件へ揃え、Windowsの検査は
削除していない。metadata取得とエラー伝播、lock handleの全platformでの保持、Unixの0700/0600設定、
identity/link検証は維持する。Windowsのprivate CA fixtureには既存のowner-only ACL helperを適用し、
file URLの予約文字試験にはWindows drive pathと期待値を加える。

Windows限定cache fixtureのREADONLY解除だけは、RustのWindows属性動作を確認した理由付きの
狭い`clippy::permissions_set_readonly_false` expectationを付ける。Unixは既存mode0600を維持する。
Job Object子孫終了試験のhelperは、既存の30秒sleepを保ち、生存した場合にchildをwaitする。
replayのfault injectionは既存Linux/macOS callerだけに合わせ、production rollbackは変更しない。
macOSの追加test helper是正前497件のeval lib診断と、最新26 sourceのfmt・workspace全target strict Clippyは成功した。
Windowsも26 sourceの照合済みoverlayでstrict Clippyが実exit0（22.6秒）となった。
これらの局所診断とWindows overlayは新しい不変候補の正式全体検証を代替しない。

e837020の静的security追加差分検証は確認finding0、canonical coverage completeで保存した。
新しい是正候補には別のsource/security/runtime証拠を束縛し、以前の成功を流用しない。
host適用の具体案と5分rollbackを用意し、Windows/WSL account/key/SSH本適用は非同期の承認回答を待つ。
dispatcherは実装どおりsource下の`scripts/v1-local-gpu/kio-acceptance-tools`へ設置する案であり、
先行提案のsource root直下の例は適用しない。新候補のhelper/deployment/bundleも同じ候補へ再束縛する。
現在まで新しいpush/Actions dispatch、paid provider/GPU workload、release公開は行っていない。
