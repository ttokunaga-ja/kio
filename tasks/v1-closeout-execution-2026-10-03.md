# Kio v1 closeout — 実行記録

2026-10-03 JST。[承認済み計画](v1-acceptance-closeout-plan-2026-10-03.md) のC0/C1/C2から着手した。
実装、ローカル検証、3 OS Actions受入、手動受入、release公開を別々に判定する。
**v1.0の受入はまだ完了していない。** 最終候補の固定とC3以降の証跡はこの開始記録から推定しない。

2026-10-04 JSTの更新: WSLの通常Rust整備は完了した。承認済みのTailscale/OIDC/Environment
3件は適用済みで、Windows/WSL host経路と条件付き復旧は2026-10-04の詳細計画承認に含まれた。
host経路はまだ未適用で、最終候補に合わせたbundle更新・検証とローカル回帰後に適用する。
復元した証拠と元132件のWIPを照合し、Windowsのsnapshot競合・GC owner・renderer・watchを
固定overlayで是正している。watchのnative 7件、CLI 4件、service 2件とrenderer全moduleは成功した。
GC全26件とWindows全workspace strict Clippyは成功した。現在の局所結果を、最終候補Sの
3 OS全体回帰・配布物検証・51件の正式受入へ流用しない。詳細と失敗の保存先は§17〜19に記録する。

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

## 13. 452adbcのOS検証とWindows runtime是正

`452adbc0659bc44413d3aebd4a93dbe2dbb1dd73` のmacOS CI相当15 commandはexit0。
全targetは99 suite、3,202 pass/0 fail/0 ignoredで、release・Persona W0等も成功した。
driverの最後のsummary作成だけがschema不一致でexit1となったため、保存した全command exitと
開始終了manifestを主担当が独立照合した。commandを再実行していない。
同じ候補の2回macOS archiveは一致し、展開binaryの11/11 local診断は成功した。
local run ID `900000008` は正式Actionsの42/51件には数えない。

native WSLでもfmt・strict Clippy・全target・release・Persona W0・synthetic-history・
release helper preflightが成功した。初回の全targetはclone作成時のumask0002による775祖先で
CA保護9件がfailした。専用clone内のdirectoryだけgroup/other writeを外し、adapter364件と
全targetを再検証した。元の失敗・mode inventory・最終clone/index不変確認を保持する。
resultsのversions行だけ旧candidate名だったため、rawを変更せず別の訂正記録を作った。
残る15行と実際のsource clone、preflightは452adbcに束縛されている。
追加依頼のWSL通常Rust整備は完了し、`kio-test` のlogin/interactive Bashとfresh SSHで
Rust/Cargo1.98.0、rustfmt1.9.0-stable、Clippy0.1.98を確認した。
`/home/kio-test/.cargo/bin` のrustup proxyを通常PATHへ追加し、既存toolchainを保持したまま
defaultを`1.98.0-x86_64-unknown-linux-gnu`へ設定した。通常の`cargo build --offline`で
小さな依存なしprojectをbuildし、Linux x86_64のELF binaryを実行してexit0を確認した。
記録はcloseout evidenceの`wsl-native-rust-20261003/`に保存した。

Windowsの同候補はfmt・strict Clippyに成功したが、全targetは最初にadapter31件でfailした。
read-onlyのACL調査で、標準Tempに別ユーザーの実効mutation ACE2件があることを確認した。
標準Tempを変更せず、検証processのTEMP/TMPだけ新しいowner-private Tempへ切替えた。
adapter355/355は成功したが、全target再検証はapp189 pass/101 failで停止し、releaseへ進んでいない。

appの24件は普通のTempDirのownerがAdministratorsでDACLも継承されたfixture前提違反である。
productionのprotected owner-only検証は維持し、test-only共通helperでretained parentから
create-onlyのprivate childを作成する。watchの2件も、実際にWindows XMLへ入るbinary pathに
ampersandを含めるfixtureと、canonical absoluteなstale rootへ修正し、既存の検査を維持する。

store列挙の共通エラーは、nofollow open失敗を一律にreparseと表示していた。
通常directory/fileだけの小さな実機probeで、locked cap-primitives4.0.2の既存設定は
普通のdirectoryでもOS error5、`maybe_dir(true)`を付けた設定はdirectory/fileとも成功した。
両fixtureのreparse属性はfalseだった。probe用offline lockの3つの推移dependency差と
先行rustcのnative link-search失敗は別記し、このprobeを正式candidate receiptへ昇格させない。
Windows列挙へdirectory-open設定を加え、nofollow・retained parent・実directory/regular handleの
検証を維持する。positiveのempty/mixed/nested列挙回帰を加える。

11 sourceの局所overlayはmacOS strict Clippyと15件の対象試験で成功した。
このdriverも最後の進捗表示だけTypeErrorでexit1となったため、raw command結果とmanifestから
照合した。Windowsの修正後試験、新しい不変candidateの全体検証/security/配布物は未完了である。
452adbcの静的securityはfinding0・coverage completeで保存したが、後続差分へ流用しない。
先行private route例のforced-command配置とroot所有祖先の記述を実装へ合わせた。
host本適用は個別回答待ちで、push/dispatch/paid provider/GPU/release公開はまだ行っていない。

## 14. Windowsの保持handleとSQLite sidecarの局所是正

11 source overlayのWindows app試験は201 pass/89 failだった。最初のSSH呼出ではwrapperの
最終exitを取得できなかったため、集計とexitの不明を別々に記録した。後続のwrapperはCRLFと
dispatchを固定し、独立exit0/exit7 smokeで確認した。失敗ログや途中のdriver不具合は保持する。

空ファイルと固定20-byteファイルの通常Windows probeでは、exclusive lock中でも別handleの
metadata取得は成功したが、ReadFileはどちらもOS33で失敗した。同じlock handleのseek_readは
正しいbytesを返し、競合try_lockはWouldBlockだった。planned stageの空gateはnofollowの
regular handleとzero-length metadataで確認し、ledger markerは保持中のlock handleから
bounded offset readする。ACL、identity、前後metadata/ChangeTime、正確なmarker照合は維持する。
この14 source overlayのmacOS strict Clippyと対象53件、Windows strict Clippyと対象38件は
exit0だった。Windows appは実exit101、231 pass/59 failとなり、全workspaceへ進めていない。

残るdirectory publicationの通常Windows probeでは、rename用openの
`FILE_FLAG_BACKUP_SEMANTICS`不足でOS5となること、既存cap directoryのdelete-sharing制限で
保持中の同じdirectoryをrenameできないことを切り分けた。renameのnofollow/share設定を保持して
flagを追加し、retained directoryのnormalizationは空のnative relative nameで同じobjectを
read-only・share-allとして再openする。元handleはreal-directory/identity照合後にだけ閉じる。
同一parent・別parentの移動中の保持capabilityと、元の診断名を置換した後の再adoptionを検査する。

registryのreadonly snapshotは、Windows canonicalizeが返すroot付き`VerbatimDisk`を
component検証してそのprefixのまま受け入れる。slashを含むverbatim component、traversal、
device namespace、VerbatimUNCは引き続き拒否する。実canonical pathからのsnapshot試験で
source bytesとsidecar集合が変わらないことも検査する。

通常SQLite probeは実exit0で、SQLiteが作成したWAL/SHMだけowner-private検証に失敗した。
retained parentから未作成のWAL/SHMをCreateOnlyで空のprivate fileとして用意すると、
WAL切替・checkpoint中のidentity/ACLは保持され、main headerは2/2となり、close/reopenを
2回繰り返して既存rowも保持できた。productionはWindowsのREAD_WRITE openに限り、
既存main/WAL/SHMをすべて検証してから未作成分だけを用意する。既存権限の修復や上書きは行わず、
READ_ONLYはこの作成を行わない。unsafe既存leaf、hardlink、missing main、読み取り専用の不変と、
最初のsidecar作成後の中断から記録済み初期化を明示再開する回帰を加える。

probeとoverlay診断は正式candidate受入ではない。新しい局所回帰、残るWindows全targetの
切り分け、最終不変candidateの3 OS検証/security/配布物、Actionsと最終51 receiptは未完了である。

17 sourceを固定した局所回帰では、macOSのstrict Clippyとdirectory contract7件、registry43件、
ledger lifecycle29件、app321件とそのchild8回がすべてexit0だった。appは約720秒で完了し、
長時間の索引childを待っていた。先行同候補のappは約836秒であり、停止とは判定しなかった。
native WSLもstrict Clippy、planned layout10件、directory contract9件、registry46件、
ledger lifecycle29件、app324件がすべてexit0だった。両方のsourceと元WIPは不変だった。

Windowsの17 sourceはstrict Clippyが実exit0だったが、directory contractは実exit101、
2 pass/3 failだった。3件ともrename後のpublished directory openが失敗したため後続を停止した。
sourceのDELETE handleを保持したまま、delete-sharingを外すcap directory helperでpublished名を
再openしている箇所を追加確認する。Mac/WSLの成功を新しいWindows差分や正式candidateへ流用しない。

published directoryのopenをWindowsのread-only/share-all/nofollowへ限定して修正した。
保持中のsource DELETE handleと衝突せず、real-directory/identity照合は維持する。
v2 overlayのWindows strict Clippy、directory contract5件、registry36件、ledger lifecycle27件は
すべて実exit0だった。appは実exit101、270 pass/20 failとなり、全workspaceへ進めていない。
20件はledger snapshotのcanonical path拒否7件、保持Repositoryを伴うnamespace移動12件、
先行失敗後のmutex poison1件に分かれる。

ledger snapshotにもregistryと同じroot付きVerbatimDiskのcomponent検証を適用し、sourceの
WALとauthority/checkpoint/intent、file集合を変更しない実canonical pathの回帰を加える。
Repositoryのauthority保持時はWindowsだけ、入力scope/kioの元Fileを消費して同一objectの
normalized handleへ替える。ManagementBindingの一時cloneだけをnormalizeしても元handleが
delete-sharingを拒否したまま残るため、CASとbound_root/bound_kioへ保存する前に行う。
名前の再照合・config/scope検証は維持する。Repositoryを保持したまま.kio/rootを移動し、
元CAS bytesを読める一方で置換された名前の操作権限を拒否するWindows回帰を加える。
これらの追加差分は局所試験待ちであり、既存の成功結果やsecurityを昇格させない。

v3 overlayのWindows strict Clippyは実exit0だった。directory contractは5 pass/1 failで、
新しい保持Repository試験の`.kio`移動がOS5になった。Repositoryのscope/kioだけでなく、
CASが保持するobjects・raw/tree/commit・lazy namespaceの元directory handleと、
public `ManagementBinding::bind`の元kio handleにもdelete-sharing制限が残っていた。
Windowsのauthority保持境界でそれぞれ元handleを消費し、同一objectへnormalizeする。
既存のidentity/named-child検査とUnix動作は維持し、public bindingとlazy Image namespaceも
保持したまま移動する回帰へ拡張した。runtime確認は後続の固定overlayで行う。

core失敗と独立したledger差分を調べるため、v3のpipeline `ledger::`だけを追加実行した。
実exit101、74 pass/20 failで、全failがsnapshot fixtureに集中した。
通常`fs::create_dir`で作ったdevice directoryのWindows ACLがowner-onlyでなく、
ledger初期化前のprivate-parent検査で拒否されていた。valid fixtureのprivate作成と、
raw SQLiteが使うsidecarの準備を既存のlifecycle境界へ合わせるfixture修正を行った。
bound接続とraw接続の両方を最初のtable readまで保持し、bound接続をfinishしてから
lifecycle lockを解放する。snapshotを読む前にはそのlockを保持しない。
製品のowner-only検査は維持する。appと全workspaceはまだ実行していない。

v4 overlayのWindows strict Clippyは実exit0だった。独立したpipeline `ledger::`は
94 pass/0 failで実exit0となり、snapshot fixture20件の先行失敗とcanonical WALの回帰を
解消した。一方、directory contractは5 pass/1 failで、新しい保持Repository試験の
`.kio`標準renameは引き続きOS5だった。共有設定の是正だけで解消したとは判断しない。
[Microsoftのdirectory rename契約](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntifs/ns-ntifs-_file_rename_information)
と[下位open検出の仕様](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-fsa/133840e4-778e-44ca-9b41-da2323615075)
を確認し、nameに結び付く下位handleとdirectory移動の関係を、独立した通常Windows probeで
調べる。標準rename、native rename、同一objectのreopen方法を分け、元FileIDとretained-relative
readを検査する。productの追加変更や試験契約の変更を、この調査より先には行わない。
v4のappと全workspaceは実行しておらず、最終受入は未完了である。

独立したWindows directory-retention probeは、17種類の保持条件と4種類のrenameによる
68ケースを実行し、compile/runとも実exit0だった。name openと空native-nameで開き直した
下位directoryを保持するとancestor renameは拒否された。`OpenFileById`で同じdirectoryを
開く条件では全renameが成功し、移動後の52保持handleすべてでvolume/FileIDと元sentinelの
retained-relative readが一致した。失敗条件の28件は想定した共有違反または下位openによる拒否で、
成功40件と区別して記録した。probeは新規のowner-private fixtureだけを使用し、既存directoryの
ACLやWindows/WSLのアカウント・SSH設定は変更していない。

この根拠に基づき、Windowsで長期保持するdirectoryのnormalizationを
`OpenFileById(ExtendedFileIdType)`へ変更した。保持元handleをvolume hintとして使用し、
real-directory/reparse検証と64-bit volume serial・128-bit FileIDの前後照合を行ってから元handleを
解放する。診断pathを解決し直すfallbackは設けない。一時的なoperation handleの空native-name
openは維持する。FileID openを提供しないfilesystemでは拒否するため、今回のNTFS実試験だけで
他filesystemの互換性を認定しない。
[MicrosoftのOpenFileById仕様](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-openfilebyid)
はSMB 3.0での非対応を記載している。保持Repository・public ManagementBinding・lazy Image CASを
伴う移動の回帰は、v5の固定overlayで確認する。probe成功を製品回帰や正式candidate受入へ
置き換えない。

保持境界のreviewで、直接`ObjectStore::from_bound_kio`を呼ぶ場合のstore所有`.kio` cloneにも
name pinが残ることを確認した。Windowsだけその所有cloneをFileIDでnormalizeし、借用元のFileは
呼び出し元の所有のまま保つ。既存rename試験のWindows側caller capabilityも同じ実体をFileIDで保持し、
raw入力からCASを作成してcallerだけを閉じた後の`.kio`/root移動・lazy Image read・置換名の分離を
追加回帰で検査する。

ledgerのlocked marker readのreviewでは、retained snapshot parentで`inherited_parent=true`に
したsessionが、保持lockのbytesだけを検査してnamed markerの置換を検出しない差を確認した。
Windowsのowning handleによるbounded readは維持し、その前後でretained parentからnamed lockを
metadata用に開き、private ACL・single-link・identityを保持lockと照合する。別handleでのdata readや
診断用parent pathの再openは行わない。同内容のprivate marker置換も拒否する回帰と、parentを移動した
後に古い診断名でretained sessionを取得して元parentだけを使う回帰を加えた。

v5はWindowsのpatch適用前のdispatch構文エラーで停止し、product試験は行っていない。
primary側で再生成したdispatchにより、remoteがv4の20 source・元HEAD/tree・clean indexと一致し、
Cargo/rustcが停止していることを確認した。文字列からencodingを生成してround-tripを確認する
起動helperを使い、追加是正をまとめたv6で対象回帰を行う。v5の未実行をpass扱いしない。


v6 overlayは21 sourceを固定してWindowsで検証した。strict Clippy、directory contract6件、
bound store6件、pipeline `ledger::`96件はすべて実exit0だった。保持Repository・public binding・
lazy Image CASの移動、直接CAS構築後にcallerを閉じる移動、named lifecycle markerの同内容置換拒否を
含む。これは局所的な診断結果であり、未commit差分の正式candidate受入ではない。

appは実exit101、外側のsummaryで284 pass/6 failだった。4件のcontextual embeddingと1件の
深いindex試験は、ledgerがまだ存在しない場合のparent ACL検査に失敗した。残る1件は先行失敗後の
mutex poisonである。nested childのsummaryは外側の290件に合算しない。raw receiptとlogを再読し、
actual exit・summary・SHA・21 sourceの一致、clean index、停止したCargo/rustcを確認した。

原因はWindowsのregistry writerが`XDG_DATA_HOME/kio`を通常の`create_dir_all`で作り、
ledgerと共有する保存先のowner-only DACLを設定しない製品コードだった。新しいWindows writerは、
最も近い既存ancestorの信頼性を検証し、欠けたcomponentだけを既存coreのcreate-only/private作成経路で
作る。新しく作ったcomponentと競合して現れたcomponentのprivate ACLを検証する。既存ancestorの
ACLは変更せず、不適切なACL・file・reparse・作成エラーは拒否する。SQLite接続中はparent capabilityを
保持する。read-only snapshotの検査・未作成時の非変更契約・Unix側の動作は変更しない。

このwriterはcoreのprivate creation parentが扱うローカルdrive pathを使う。UNCでのwriter動作を
今回のNTFS試験から認定しない。Windowsの新規private nested parent、reopen時のACL/identity保持、
既存trusted parentの非変更、unsafe/non-directory ancestor拒否、read-only missingの非作成という
5回帰を追加した。次のv7 overlayはregistry1 sourceだけをv6から更新し、strict Clippyとregistry/appを
検証する。他のv6で変更のないcore/ledger対象試験は繰り返さない。全workspace・3 OS・配布物・security・
CIと正式受入は、最終candidateを固定してから確認する。


v7 overlayはpatchを一度だけ適用し、21 source・元HEAD/tree・clean indexを確認した。
strict Clippyで追加Windows test helperの`let_and_return`が実exit101となり、registry/appは
実行前に停止した。補助関数の不要な一時bindingだけを除去し、警告抑制や検査緩和は行わない。
v8はこの1 sourceの局所差分を固定して、同じstrict Clippyから再開する。


v8 overlayはstrict Clippyとregistry41件が実exit0だった。appは実exit101、外側summaryは
288 pass/2 failで、4件のcontextual失敗は解消した。残る直接failは513個のflat childと64段の
deep childをindexする試験のabsent ledger parent ACL拒否であり、1件はmutex poisonの連鎖である。
raw receipt・log・source guardをprimaryで再読し、21 sourceと元HEAD/tree/index、停止した
Cargo/rustcを確認した。最終workspaceや受入へ昇格しない。

同試験はmanagementのroot初期化を直接呼び、CLI Initのregistry登録を経由しない。
registry以外のordinary作成経路を調べ、Windowsのlog scrub-lock parentが欠けたcomponentを
default DirOptionsで作る経路を確認した。log-firstの共有device parent作成をprivate作成へ
是正する対象とする。既存componentのACL修復・Unixの変更は行わない。直接failに至る最初の
runtime creatorの完全な観測はまだないため、source traceと次のnative回帰を区別して記録する。


Windows lock-parentの欠けたcomponentだけを、retained parentのcreate-only/private作成へ
変更した。既存componentとUnixの処理は維持し、既存ACLは変更しない。競合時のreopenは
structured already-existsだけに限定し、nofollowとreal-directory検査を継続する。
実`append_event_log`を先に呼ぶWindows回帰を追加し、新規device/kio/logsのowner-private DACL、
64-bit volume/128-bit FileID、reopenと2回目append後の不変、既存prefixのACL/identity非変更を検査する。
v9 overlayではstrict Clippy、core library、app libraryを実行する。未変更registry41件の
成功は保持するが、正式candidateの全回帰やsecurityには代用しない。


v9 overlayはstrict Clippyが実exit0だった。core library全268件は257 pass/11 fail、
実exit101となり、app libraryは実行前に停止した。追加したWindows log-first回帰は成功した。
9件はmanaged restoreのprivate owner検査、1件はCAS移動時のOS32、1件は巨大sparse inputの
error code不一致だった。raw receipt・logをprimaryで再読し、終了後のreadonly guardでも
21 source・元HEAD/tree・clean index・停止したRust processesを確認した。

CAS試験では借用元のraw capabilityを試験側が保持したままrenameしていた。製品が所有する
cloneのFileID保持は維持し、試験側callerを閉じてから移動する。巨大inputはWindowsの
source openで汎用store byte limitが先に拒否するため、archive layerの仕様errorに届かない。
各callerのmetadata/stream budgetを検証してからsource層だけを是正する。managed restoreの
owner拒否は共通経路を調査中であり、既存ACLの修復や検査の緩和は行わない。


新規fixtureだけのWindows実機probeで、現sessionの管理者role有効と、通常CreateNewで作った
fileのownerがAdministrators SID、TokenUserとは不一致であることを確認した。これは既存
`managed_restore`の通常fixture writeとcurrent-user owner前提の相違を説明する。
`14-v1-runtime-contracts.md`のforeign-owner working-file removal拒否は維持し、test-only
helperで新規working fixtureだけをkernelのcurrent-user/private CreateNew経路で作成する。
既存leafへのrewriteは同じownerのまま行い、productionのACL・owner policyは変更しない。

次のv10ではCAS testのcaller close、Windows source層の容量error、managed restoreのfixture
ownerを一括固定する。strict Clippy後、全workspace/all-targetsの`--no-fail-fast`診断で
core/appと未確認integration targetをまとめて調べる。局所試験の重複を抑え、複数targetの
失敗を1つの固定差分で回収する目的であり、正式candidate・正式受入には昇格しない。

v10の全workspace/all-targets診断は35分53秒で終了し、外側88 executableで2,758 pass /
138 fail / 0 ignored、実exit101だった。stderrの失敗targetは24件で、core libraryの
managed restore・CAS移動・巨大inputとappのdeep child indexの前回失敗は解消した。
raw stdoutには8件の成功したself-helper summaryも含まれるため、それらを外側試験数へ
重複計上しない。終了後のguardは22 source、元HEAD/tree、clean index、Rust process不在を
確認し、実exit0・exact marker0だった。

138件は独立した138個の製品欠陥を意味しない。ledger・local OCRなどには新規fixtureの
Windows owner/private DACL不足が共通し、appの残るembedding admission試験はambient
grant storeを参照していた。製品のcurrent-user/protected-DACL拒否を維持し、fixtureを
既存の安全な作成APIと隔離device stateへ合わせる。GCでは既存receiptのread-only handle
に対するflushと、read-only SQLite保持handleのwrite sharingを限定して是正する。
marker exchange、watch、processなどの残差は別に切り分け、局所再検証前に最終候補を固定しない。

## 15. WSL整備の完了確認とWindows追加診断

WSLの`kio-test`では、2026-10-04 02:16 JSTのfresh SSHでも、環境変数の手動指定なしに
`/home/kio-test/.cargo/bin`のRustup/Cargoとdefault `1.98.0-x86_64-unknown-linux-gnu`を
確認した。Rust/Cargo 1.98.0、rustfmt 1.9.0-stable、Clippy 0.1.98が利用できる。
通常のoffline Cargo buildでLinux x86-64 ELFを生成し、実行exit0を確認済みである。
記録はrepo外の`wsl-native-rust-20261003/receipt.json`、`native-build-smoke.json`、
`fresh-shell-readback-20261003T171632479820Z.json`へ保存した。WSL Rust整備の依頼分は完了した。

Windows追加是正は、台帳fixtureの新規private parent/sidecar作成、child scopeの現行動作と
隔離検査、foreign-history tree、GC receiptの同一handleでの検証とflush、GC read-only
source保持、watch lockのmetadata検証、scratch directoryの強い保持へ分けている。
実CLIによる新規隔離台帳のinit・ledger init・reconcileはいずれもexit0だった。
QA15では、試験のraw SQLite connectionを保持したままreconcileを再実行していたため、
既存のtyped ledger APIによる同じkeyのabsence検査へ変更した。raw row-count/contender
fixtureだけは、既存leafを検証してから欠けたsidecarをprivate/create-onlyで用意する。
既存owner/ACLの修復や製品の拒否条件の緩和は行わない。

Windowsの短縮toolchain指定`+1.98.0`がoperator default hostのGNUを選ぶことも実機で
確認した。release recipeのWindows x86-64 pinは完全なMSVC tripleへ統一した。
v11ではWindowsで不要になった旧GC helperのdead code、v12では追加した回帰試験の
API名誤りでstrict Clippyがexit101となった。Mac v11は新規2行の整形検査で停止した。
これらの失敗receiptを保存し、修正後のv13を49 source（48 tracked + test helper 1件）で
固定した。元132件のuntracked WIPの内容・metadataとindexは不変である。
v13はprimaryのfmtとWindows strict workspace Clippyがexit0で、対象試験を実行中である。
Windows owner-private scratch fixtureには残るassertion failureがあり、原因を追加確認する。
局所診断結果は正式candidate、全OS回帰、配布物受入、security完了には代用しない。

AppContainerの環境変数比較は別の隔離v10 cloneで行った。SystemRootのみはSpawn203で、
LOCALAPPDATAを追加するとCreateProcessは成功するが、3変種とも子のexitは
`0xc0000142`、stdout/stderrと開始markerは空で、fixture出力もなかった。
診断test自体のexit0をrendererの起動成功やconfinement成功と扱わない。
調査用cloneの初回checkoutはパス長制限で失敗し、失敗cloneを保存したまま、
新規clone内だけのlong-path設定を追加して再作成した。ホスト全体のGit設定は変更していない。
private window station/desktopの比較案はrepo外へ準備した段階で、まだ実行していない。

## 16. v13の対象検証結果

Windows v13のstrict workspace/all-targets Clippyは実exit0だった。対象25 Cargo test
invocationの外側summaryは1,192 pass / 9 fail / 0 ignoredで、6 stageに失敗が残った。
ledger、reconcile、current policy、offline egress、local OCR/secrets、backup、managed restore、
Step 2/3、foreign-history portability、index library、management contract、embedding admission、
watch lock、fsck、on-idle GCは成功した。v1_watchがexit101だったため、同stageの後続
v1_watch_serviceは未実行である。全workspace・release/package・正式受入はこの対象検証に
含まれない。v10の全workspace 2,758 pass / 138 failと試験範囲が異なるため、失敗数だけで
全体の残差が9件まで減ったとは判断しない。

残る9件は、empty scratch保護のProfile/Win32 5、OCR HTTP timeoutのvariant不一致、
watcher enrollmentの待機timeout、GC exchange ownerのentry count 3件、backup mismatchの
初回中断exit不一致、quarantine rename拒否の不一致、snapshot checkpoint差替えのexit不一致
である。GC ownerの3件はchild exitではなく、owner directoryのentry count 1対期待0の
assertionである。sourceはcleanな`.kio-atomic` workspaceを許容するが、残ったentryの実機
確認を伴わずにassertionを緩和しない。quarantine試験はrenameで停止し、hardlinkは未到達。
ready markerだけでは5秒のbarrierが継続中だったと証明できず、子の生存・保持handleと時刻を
追加観測する。HTTP試験は実際のerror variantを記録しておらず、単発失敗をflakyと分類しない。
watcherは300秒周期の試験で、以前の1秒周期A02の待機条件とは別に調査する。

終了後のWindows guardは49 source、元HEAD/tree、48 tracked差分とtest helper 1件、clean
index、reparseなし、Rust process不在を確認し、SSHの実exit0とexact marker0だった。
primaryでnative log 52件のsize/SHAと全25件の外側summaryを再読し、合計を照合した。
記録はrepo外の`windows-v13-runner-preparation/windows-final-overlay-diagnostic-v13`に保存した。

Mac v13はfmt・strict Clippy、CLI 6 targetの120件、management contract 30件、
embedding admissionの指定filter 1件が実exit0だった。evalの外側497件は全成功したが、
runnerが直後のprimary guardで停止し、Cargo processの終了値を保存できていなかったため、
exit0は完了footerからの推定として記録する。nested helper 2件は497件へ合算しない。
停止原因はrootが追記した本書1ファイルのSHA差分であり、49 sourceや既存WIPの変化では
なかった。停止receiptを保存し、その正確なdoc差分だけを承認したguardで残る対象を実行した。
evalは再実行していない。Macのapp試験は名前filterで1件を実行し、`--exact` flagの実行とは
記載しない。記録は`macos-windows-corrections-v13/run`に保存した。

2026-10-04 02:50 JSTのprimary guardでも、元132件のuntracked WIPの内容・metadata、
全49 source、対象外tracked 1,601件とindexは一致した。WSL Rust整備の完了は維持し、
Windows/WSLの専用CI host経路の本適用とv1全体の受入完了は別の未完了事項として管理する。

次の原因切り分け用に、scratchのWin32 phase probe、既存assertionを維持したHTTP/watch/GC
diagnostic patch、隔離AppContainer desktop probe runnerをrepo外へ準備した。前2件は
patch application・整形・byte比較、desktop runnerはinput hash・Python AST・PowerShell
引数と重複実行拒否の静的検査を行った段階である。これらの追加probeはまだnative実行して
おらず、製品の是正や受入の成功には計上しない。保存先はそれぞれ
`windows-scratch-phase-probe-preparation`、`windows-v13-residual-probe-preparation`、
`windows-appcontainer-desktop-runner-preparation`である。primaryの49 sourceは維持している。

## 17. 詳細計画承認後の再開と証拠保存先の中断

2026-10-04 03:47 JSTに、承認された詳細計画の実行を再開した。開始時のguardで元132件の
WIPの内容・metadata、49 source、HEAD452adbcとclean indexを確認した。通常WSL Rust、
SSH、GPUも正常だった。Windows/WSL専用CI host変更と条件付き復旧も今回の承認に含まれるが、
まだ本適用していない。public repo、3 Environmentのmain限定、provider ledgerの2 rulesetと
実効ruleはread-only確認した。ledger branchは未存在で、初期化・課金runは行っていない。

AppContainer診断runnerの転送対象不足とProcessIdToSessionIdの参照先不一致を、repo外の
診断コードだけで是正した。失敗した転送・compileの記録は別に保持した。実行したdesktop-v3は
HEAD452adbcとv10 overlayに束縛した原因調査であり、最終候補の受入ではない。外側の診断testは
1 pass・実exit0だったが、通常とprivate desktopの子はいずれも0xc0000142、出力・marker・
scratch fileなしだった。private window station/desktopの作成、元接続への復元、両handleの
閉鎖は成功した。両variantのbounded completionとfixture cleanupも確認した。この比較だけでは
原因を確定できず、AppContainer起動問題は未解決である。native sourceの前後guardは成功した。

Windows対象9失敗用のv14 runnerはrepo外に準備・reviewした段階で、native実行していない。
その後Documents/Codex全体が元の保存先からなくなり、ゴミ箱内への移動を確認した。
macOSが内容へのアクセスを拒否しているため、元WIPの照合基準とrunnerの復元を依頼し、
追加修正・試験を停止した。移動された証拠を未確認のまま再作成・上書きしていない。

mainの追加commit0d560839は新しいAGENTS.mdだけで、利用者の新指示と整合するため保持する。
今回の製品sourceには変更を加えていない。復元後は旧記録と現在のsource/WIPを再照合し、
AGENTS.mdのcommitによるHEAD差を明示して診断と候補固定を再開する。

Windows側に残ったdesktop-v3の7 logをsize/SHA-256と実SSH exit0で回収し、
`/Users/ttokunaga-ja/.codex/validation/kio-v1-closeout-20261004-recovery/desktop-v3-readback-20261003T190512506378Z`
へ保存した。51件の正式受入、全OS回帰、配布物、host適用、push/dispatchは未完了である。

2026-10-04 04:24 JST、利用者の復元連絡を受けて元の保存先を再確認した。製品source49件、
元132件のWIPの内容・metadata（directory symlinkはtargetを照合）、v14 payload18件、
manifestとdispatcherが移動前の記録と一致した。HEAD差はAGENTS.md追加だけであり、
indexは空、開始時snapshotとの差は本作業の文書3件だけだった。desktop-v3の復旧先に
初めて回収した7 logもsize/SHA-256が一致した。WSL SSH、native Rust/Cargo1.98.0、
kio-sshd、710GiBの空きとRust process不在を実確認し、追加診断を再開した。
復元guardは `execution-resume-20261003T184751394418Z/restoration-guard-20261003T192444767780Z.json`。
照合scriptのdirectory symlink扱いと、元runnerには未回収だったnative logの保存先指定を
修正した記録も保持した。製品sourceの変更・native試験・CI host変更は再開時点では未実施。

## 18. 復元後のWindows追加診断

2026-10-04 04:40 JST時点。v14のbootstrap、transfer、PowerShell parser、prepare、source
guardは実exit0だった。owner-private scratchの既存testは再び実exit101、Profile Win32
code5で失敗した。v1 runnerの `echo 0>file` はCMDでdescriptor指定に解釈され、postguard
fileが空になることが分かったため、v1を保持し、prefix redirectionを使う新しいv2で続行した。

新規TokenUser-owned fixture上のAPI診断では、WRITE_DACだけのhandleで一覧取得がWin32
code5となり、SetSecurityInfoもcode5だった。一覧取得のbufferには142要素に対して
1136 bytesを確保していたが142 bytesだけを渡していた。独立matrixでLIST_DIRECTORYと
READ_ATTRIBUTESを追加すると一覧取得が成功し、空directoryでも `.` と `..` が返された。
この診断testのpassは結果記録の完了であり、保護処理の成功ではない。

v2 read-access variantはsource hashが正しくてもCargoが旧binaryをFreshとして再利用し、
baselineのAPI logを出したため、比較結果は無効・未確定とした。sourceコピー間でshared
Cargo targetを使う手順を中断し、variantごとの空target/build directoryを持つv3を準備する。
正式な同一候補の全体回帰・配布物buildにも新しい候補専用build先を使い、source guardだけ
でbinaryとの対応を証明したとは扱わない。稼働中のgc-hookが終了するまで待ち、Mac側の
本作業の外側orchestratorだけを停止した。既存Cargo cacheやbinaryを削除していない。

v2 residualで実際に再compileしたOCR timeout testは実exit0、Timeout variant・34ms
だった。この一回では旧失敗の原因を確定できず、OCR製品codeは変更していない。
gc-hookは実exit101・postguard0で、残ったentryは `.kio-atomic` directory1件だけ、
inspect_atomic_exchange_directoryはClean、intent/backupは存在しなかった。契約と実装を
照合して是正対象を判定する。AppContainerではcmdとkernel32-only最小binaryの比較を
repo外に準備中で、desktop-v3の0xc0000142の原因はまだ確定していない。

2026-10-04 05:15 JSTまでの追加結果。v3はvariantごとの空target/build先で8診断を実行し、
全postguardが実exit0だった。read-access variantで一覧取得は成功したが、空directoryの
`.` を実entryとして拒否する不具合も再現した。scratch保護のアクセス権、bufferのbyte数・
alignment、dot entryの扱いを修正し、sole TokenUserのprotected DACLという契約を維持した。
修正後のowner-private実機回帰は準備中で、静的reviewとformat確認だけを成功扱いする。

GCの3失敗には、契約上Cleanである常設 `.kio-atomic` を残留物と誤判定したtestが含まれた。
testはintent/backupなし・Clean spoolだけ許可し、不明entryを拒否する既存core testを維持した。
quarantineでは5秒barrier中にrenameは拒否、hardlinkは成功し、barrier timeoutとは無関係だった。
tree victimだけをshare-noneの保持handleで固定する修正を実装し、既存のrename/hardlink拒否条件と
foreign bytes・削除・receiptのpostconditionを維持した。native Greenは未確認。

indexのuntampered SourceCaptured faultは、coreの中断errorがSchema文字列に変換され、appで
破損errorに変わっていた。GC exchange専用のtyped errorで保持する修正を実装し、Macの対象2test
はRedからGreenになった。snapshot競合は、replacementのwriter handleが存続して初期mutation
pinが共有違反になる場合を切り分け、変更済みidentity/stateが証明できる場合だけCAS競合を返す
修正と、変更がない場合に元IO errorを保持するtestを追加した。どちらもWindows確認は未完了。

AppContainerの追加比較で、HANDLE_LISTにBox pointer分の8 bytesだけを渡し、3 HANDLE分の
24 bytesを渡していない不具合が見つかった。元49sourceへの変更をこの1行だけに限定したv2で、
kernel32-only fixtureの非隔離controlはchild exit0・stdout/scratch一致、修正後の隔離実行も
child exit0・stdout/scratch一致となった。fixture/import/source hashとcleanupを確認し、
System32のACLは変更していない。cmdは起動時例外からstderrのNonUtf8へ変わったが、cmd経路は
未解決として残す。診断testのouter passは正式受入に数えない。

watchの2管理record作成・fresh観測は成功していたが、25秒中backlog1・generation増加が続き、
queue idle条件が失敗していた。sanitized path分類とnative event kindを128件まで採取する
診断を別cloneで実行する。通知filter、timer、受入predicateは変更していない。
同一候補の3OS全体回帰・配布物検証、専用CI経路適用、51件の正式受入は引き続き未完了。

2026-10-04 05:46 JSTまでの追試。`windows-corrected-regression-runner-v1` は17試験を
variantごとの空target/buildで実行し、全source postguardが0だった。scratchの空・非空・junction
3試験、GC owner hook・中断再開・mismatched backup拒否、unknown residue拒否、index交換の
6 checkpointが実exit0となった。新しいscratch非空拒否とsnapshot writer競合のRedは101、
変更のないwriterの対照は0、修正後のwriter競合2試験はいずれも0だった。

snapshot CLIは期待する競合exit3・error code・競合側bytesの保持まで成功し、準備済みtemporary
の残留で101となった。initial source pinで競合を証明でき、exchange ownerがPendingでない場合
だけ、この呼出しが作ったidentity/bytes一致のtemporaryを退役する修正を追加した。journal作成後の
faultでは回復用targetを保持するtestも追加し、native確認は次のrunnerで行う。

GC sweep中の許可済みsearchによるsnapshot差分は `logs/access.jsonl` の1行appendだけだった。
監査仕様どおりの時刻・redaction・mode・result count・event envelopeと旧bytesの完全prefix一致を
要求し、このvalidated append以外の全path/bytesを比較するtestへ修正した。拒否されたsearchは
引き続き全snapshot一致を要求する。quarantineのshare-none版はattack成功で101のままであり、
共有mode修正だけで解決したとは扱わず、attack種別・child生存時間・直接Win32 APIを追加診断する。

AppContainerのkernel32-only fixtureは、HANDLE_LIST修正後もSystemRootだけではCreateProcessの
Spawn203となった。`windows-appcontainer-environment-probe-runner-v1` の同一binary比較では、
専用scratch内のLOCALAPPDATAだけを追加するとchild exit0・stdout/scratch完全一致・cleanup成功と
なった。imports/hash/controlと全source guardを確認した。通常ユーザーのLOCALAPPDATAを渡さず、
既存のprivate profileを指定するadapter修正と、実際のfile/networkアクセス試行を要求する隔離testを
進める。これらのnative回帰と実Office配布物の試験は未完了である。

cmdのNonUtf8 stderrはCP932の「verbatim drive pathをUNCと扱い、Windows directoryへfallback」
という診断だった。通常drive表記が同じcanonical UTF-16 pathへhandleで厳密roundtripできる場合だけ
lpCurrentDirectoryに使う修正を加えた。canonical scratch・DACL・runtime root・capabilityは保持し、
reserved/trailing name等は元表記を維持する。静的security reviewは指摘なし、native Red/Greenは未実施。

watch trace v1はchild stderr pipeが未排出で29件目に止まり、liveness判定には無効だった。既存記録を
保持し、監視root外の専用sidecarへ上限128件を書くv2を作成した。v2は128件を完整に回収し、試験101・
postguard0だった。generated file通知は抑止される一方、`.kio` directoryのModify(Any)がpath hintを
作ることを確認した。control変更・削除・rename・不明通知を落とさない修正方針を検討中である。

Macのformat、対象kio-index/kio-appのall-targets strict clippyは実exit0。現時点の変更を固定した
同一候補の全体回帰には数えない。元132件のWIPは05:15 JSTの再照合でもbytes/metadata一致、
HEADは0d560839、indexは空である。本適用・push・workflow dispatch・release/tagは行っていない。

## 19. 復元後の是正回帰とGC共有契約の再確認

2026-10-04 06:31 JSTまで。`windows-corrected-regression-runner-v2`でCWDの旧表記を使う
unitとrelative-output integrationのRedを各101で確認し、先行runnerの修正版unit2件と
relative-output integrationはいずれも0だった。通常drive表記への限定変換のnative証拠が揃った。
snapshotのowned temporary cleanup、Pending recovery target保持、changed/unchanged writer、
6 fault checkpoint、CLI競合差替えの6 invocationはすべて実exit0、postguard0だった。
GC sweepの許可済みaccess audit append検査も実exit0となった。変化のない試験を重複実行せず、
同runnerの未実行stageを成功扱いしていない。

`windows-confinement-regression-runner-v2`の旧ping fixtureは実exit101で、実際のerrorが
`Process(NonUtf8 { stream: "stderr" })`だった。外部command依存を除いたchild-ready barrierで
Job timeoutを検証する修正版は実exit0、2.62秒だった。loopback fixtureは同一listenerの
非隔離connect/send/accept/readを対照として証明し、隔離childの実アクセス試行を要求する。
PermissionDenied/10013とbounded connect timeoutを別markerで区別し、接続成功やその他errorを
拒否する。timeoutは明示的なOS拒否codeとは記載しない。修正版単体と全moduleは実exit0で、
全moduleは14 pass、child actor helper 1 ignoredだった。このhelperは親試験から隔離childとして
実行される。全source/postguardは0だった。private profileだけをUSERPROFILE/LOCALAPPDATAへ
指定するadapter unitも先行runnerでRed101→Green0となり、ambient profileを転送していない。

Win32の独立sharing matrixでは、ownerの保持handleが生存中でもshare0とshareREADの両方で
hardlink作成が成功した。share0ではreader openが32で拒否され、shareREADでは成功し、renameは
どちらも32で拒否された。この実測から、§18のtree専用share-none helperは採用を取り消して削除し、
docs/05のread-only共有・retained reader回収契約に沿う既存acquisitionへ戻した。hardlinkが追加された
場合の受入条件は、GCの正確なcorrupt/unsafe拒否、alias/archive/foreign bytesとactive markerの保持、
再実行時の拒否である。別名が増えたfileはdisposition後もlink数を再検証してからtruncateし、aliasの
内容を失わせない既存境界を維持する。readerのidentity・link数0・size0/EOFまで確認する新しい
native Red/Greenと全GC sweep moduleは実行中であり、まだ成功へ計上しない。

watchのv3詳細trace runnerは転送対象からpayload-files.jsonが抜けてprepareで停止した。
clone・compile前の失敗を保持し、入力の依存を補った別rootのv4を準備した。製品backendでは
dataとattributes/securityを独立handleで購読し、通常directoryのdata Modifiedだけを抑止する
修正を実装した。control、name event、authority、未知・失敗・overflowは保守的に再照合する。
静的reviewで見つかったdirectory-valued controlの取りこぼしは既存control分類のexemptionと
Red/Green testで是正した。Macのcommon/native test5件、format、単独Windows backendの型検査は
成功したが、実Windowsのwatch・停止・authority通知と全workspace lintは次のrunnerで確認する。
全repoのcross checkはringが必要とするMinGW compiler不在で未成立であり、native成功とは扱わない。

この節の局所native結果は同一候補Sの3 OS全体回帰・配布物受入・最終securityを代替しない。
専用CI host経路の本適用、push・Actions、51件の正式受入も未完了である。

06:45 JST追記。read-sharing版のretained reader試験は実exit0で、同じfile identityの
link数0・size0/EOF、canonical treeとmarkerの不在、receiptを確認した。hardlink試験は
初回GCのexit4/CORRUPTとalias/archive/marker/foreign bytes保持まで成功したが、再実行で
期待したUNSAFEに対して実際はCORRUPTだった。resumeではmutation openerより先に
frozen-markerのarchive検証がnlink2をCORRUPTとして拒否するため、同じ正確なcodeへ
試験の期待値1行だけを修正した。許容codeを広げず、bytesの全保持条件を維持する。
元のnative失敗は保持し、修正後の全GC sweep moduleを新しいrunnerで確認する。

watchの指定toolchainによるfmtでimport順、strict Clippyでparserの3 lintを検出した。
失敗を保持し、owned fileだけを整形・is_multiple_of/as_chunksへ修正した。正確な
`cargo +1.98.0 clippy --workspace --all-targets --locked -- -D warnings`は実exit0、
common/native test5件も0となった。Windows実行はこの最終59sourceに束縛したv3で進める。
traceのv4は準備済みだが未実行であり、旧backendと新backendの同じ強いCLI条件を
直接Red/Green比較する。修正済みprocess/adapterの3 sourceはstaged diffとhashを確認し、
local main `fda1dc0`へ論理単位としてcommitした。pushは行っておらず、最終候補Sではない。

07:08 JST追記。`windows-watch-regression-runner-v3`は旧backendの強いCLI収束条件で
Red101を確認した後、修正版のnative 7件・service 2件・CLI全4件が実exit0となった。
directory-valued control、attributes/security通知、pending I/Oの停止・callback禁止を含み、
25秒の受入deadlineを延長していない。全source/postguardは0で、fmtも0だった。
watchの実装とdocs/12の対応説明をlocal main `69c88ba`へcommitした。

Windowsのstrict Clippyはprocess parserの剰余判定2箇所で101となり、同じ判定の
`is_multiple_of`へ修正した。次の全workspace追試ではWindows watch起動の`manual_inspect`だけが
報告され、errorとside effectを保持した`inspect_err`へ機械的に修正した。各失敗のraw captureと
source guardを保持している。Macの指定toolchainによるfmt・locked全workspace strict Clippyは
修正後も0だが、Windowsの最終lintは新しい59source freezeで再確認する。既存commitは改変しない。

07:20 JST追記。`windows-gc-sharing-regression-runner-v2`の全`phase4_gc_sweep`は
26 pass / 0 fail、実exit0、123.09秒だった。retained readerのidentity・link数0・size0/EOFと
hardlink追加時の正確なCORRUPT拒否・alias/archive/marker/foreign bytes保持を含む。
source/postguard・回収・最終guardもすべて0だった。

Windows lint runner v2では、省略形の`+1.98.0`がホスト既定のGNU版を選び、ringが要求する
gcc.exe不在で101となった。製品sourceは59件すべて一致し、postguard0だった。既定toolchainや
host設定は変更せず、明示的な`+1.98.0-x86_64-pc-windows-msvc`とversion/hostの事前確認を
加えたv3で同じsourceを追試する。この環境失敗もraw captureとして保持する。

07:23 JST追記。lint v3のrustc/cargoはrelease 1.98.0・host x86_64-pc-windows-msvcを
実出力で確認した。fmtと`clippy --workspace --all-targets --locked -- -D warnings`は
実exit0、全source/postguard・回収・最終guardも0だった。store・registry・ledger・GCと
直接関連するprivate fixtureをlocal main `cca9887`へ保存した。Windows releaseのMSVC pinと
portable fixtureは`06ed68c`へ保存した。元132件の未追跡WIPは
bytes/size/mode/mtime/inode/device一致を再確認し、一切stageしていない。

## 20. 同一候補の全体回帰とMac監視テストの出力回収

2026-10-04 08:31 JST記録。`1820d3fbcf0170a8d206045984c4b7c9b4685809` /
tree `5ff7b3dc215a685ee3a51540d93113f40f30e3b8` を固定し、tracked 1,655件の
bytes・Git blob・indexと、元132件の未追跡WIPのmetadata/contentを照合した。
bundle SHA-256は `3acd73a9c98dbbd0a37ce962f5601ca39f61f693097b8be056722630d16c5691`。
以下はこの候補で得た結果であり、後続候補の全体回帰や51件の正式受入に流用しない。

WSLでは、bundleから作成したclean detached cloneと新規target/build directoryを用い、
Rust 1.98.0のfmt、locked workspace/all-targets strict Clippy、全workspace/all-targets test、
locked release workspace buildがすべて実exit0だった。testは102 executable summary、
3,221 pass / 0 fail / 1 ignoredで、789.42秒。全1,655 sourceの前後fingerprint、HEAD/tree、
indexとclean statusも一致した。証拠は `linux-wsl-1820d3fb-workspace-v1/`。
これはWindows native、配布物、実service/provider/local受入の成功を表さない。

Macのfmtとstrict Clippyは実exit0だったが、全体testは実exit101で、`v1_watch`の
停止3件が25秒の期限を超過した。release buildは順序ゲートにより未実行である。
`-p kio-cli --test v1_watch`の4件成功時にはCLIの依存feature統合が変わって再buildされて
いたため、その結果で全体失敗を解消済みとはしなかった。元のfull variantのCLIとharnessを
SHA-256で固定して保存し、再現時の3 processをsampleしたところ、いずれも監視処理終了後の
`main.rs`のJSON出力でstdout pipeへのwrite/flushを待っていた。testがprocess終了まで
pipeを読まないことによる停止待ちとの循環で、notifyの停止不具合を示すstackではなかった。
再現は実exit101、1 pass / 3 fail。証拠は
`macos-full-variant-v1-watch-diagnostic-1820d3fb-v1/`。

是正は`crates/kio-cli/tests/v1_watch.rs`だけに限定し、起動直後からstdout/stderrを並行して
回収し、正常終了・crash・Dropでchildとreaderを回収する。25秒期限、並行test、既存assertionを
維持し、正常停止結果の完全なJSONと`status: stopped`も検証する。全workspace/all-targetsの
locked `--no-run`は実exit0。新harnessを元のfull CLI（SHA-256
`482f079a0907c3db173fcee3c6a143cc6f3f3f68b1a577db6d1b4a6965fc2095`）へ明示的に
結び付けたGreenは4 pass / 0 fail、実exit0で、test時間12.28秒、command全体13.308秒だった。
前後1,655 sourceは一致した。証拠は `macos-watch-pipe-green-1820d3fb-v1/`。
この局所GreenをMac全体回帰へ昇格しない。

Windows全体runner v1はPowerShell 5.1の261文字pathの観測失敗、v2はRust version確認用の
findstr判定で準備段階に停止し、いずれもCargo全体回帰を実行していない。v2の実出力は
release 1.98.0 / host x86_64-pc-windows-msvcだった。新しいv3は短いroot、semantic version/host
確認、未起動Cargoをnullで区別するterminal receipt、転送対象の完全なclosureを検証した準備物。
Macのtest是正で候補が変わるため、この旧候補向けv3は実行せず、新候補へ再生成する。
失敗したprepare/preflightのraw receiptは保持する。

この候補の `452adbc..1820d3fb` security diff scanは変更Rust 57件を確認し、候補0件、
coverage completeでseal/readbackまで完了した。Daybreak Blueを指定した独立CLIも実exit0だったが、
返却metadataから実行backendを独立確認できず、指定modelをbackend実証として扱わない。
scan IDは `8a2364ec-9337-4bd9-897a-ebf08cdbef2c`。この静的結果はruntime受入を表さず、
後続のtest差分も新候補のreview対象に含める。

配布物runnerと専用host適用packetは実行前reviewで見つかったscript不整合を是正中で、
hostへの本適用、push、Actions、provider/localの外部呼出しは行っていない。Mac空き容量は
約12 GiBで、独立2回build用の18 GiB開始基準を下回るため配布物試験を保留し、利用者へ
整理方法を確認した。復元したreceipt・source・package・compiled proofを削除していない。
今回のtest是正と文書をcommitした後、repo外freezeで新候補SHA/treeを固定し、
その候補の3 OS全体回帰・配布物・security・専用経路・正式受入を順に確認する。

## 21. 復元照合、S2全体回帰とA08 fixtureの是正

2026-10-04記録。固定候補S2は
`3d4fa21dea4fedca1aece82c36e4f0f6c46b1e14`、treeは
`ed14e68b94e6b0d352804eec74188a2d22a816c4`、trackedは1,655件。
利用者の復元後、10:55 JSTにtracked bytes・size・mode、HEAD/tree/index、
元132件の未追跡WIPの内容・mode・mtime・inode・deviceとbundleを照合し、すべて一致した。
bundle SHA-256は `1b22f6161d643b9447f56d3d4e25891e5361d88f582d4d48d3eb1ce26ffd308b`。
証拠は `restore-confirmation-20261004T0155Z/confirmation.json`。是正作業はこの照合後に再開した。

S2では、各OSの実環境でfmt、locked workspace/all-targets strict Clippy、
全workspace/all-targets test、locked release workspace buildがすべて実exit0だった。
各実行の前後で固定sourceとclean cloneを照合している。

| OS | test executable summary | pass | fail | ignored |
|---|---:|---:|---:|---:|
| macOS | 98 | 3,211 | 0 | 0 |
| Linux / WSL | 102 | 3,221 | 0 | 1 |
| Windows MSVC | 96 | 2,923 | 0 | 1 |

統合証拠は `three-os-s2-formal-completion-v1/completion.json`。Macは短いprivate TMPを使った
v2が全体成功であり、長いTMPでUnix socket path上限に達したv1の失敗も保持している。
S2の差分security検証はscan `0352fd16-5056-4787-8f48-370cf4e5c7ea`で候補0件、
coverage complete、seal/readback完了。いずれも後続候補や正式51件の受入成功へ流用しない。

Linux配布物は独立2回のbuild・package・verify・smoke・archive比較を完了し、
17段階すべて実exit0だった。両archiveは11,307,212 bytesでSHA-256が
`5f23f5b4f88d286251e7e7b7ce8b7feed0a37b75dde0956a8f95eb44bf1b54fb`と一致した。
証拠は `linux-s2-package-v1/`。Linuxのsafe CI parityもtooling・W0・syntheticの実exit0で、
証拠は `linux-s2-ci-parity-v1/`。これらをnative/provider/localの正式受入と合算しない。

Linux local native試験はA01〜A06の7 receiptが成功し、A07で停止した。
追加診断はconverterのexit127を再現し、抽出配置の`soffice` wrapperがsandbox内で
`oosplash`を実行できないことを確認した。物理fileはhostに存在するが、現行のroot所有
`/opt/libreoffice<major>.<minor>`配置の信頼規則に抽出先のhome配下が合致しない。
製品のmount・信頼規則やhostのOffice配置を変更して通さず、正式CIの所定配置で必須A07を検証する。
証拠は `linux-s2-office-probe-stderr-diagnostic-v3/`。診断overlayをS2の受入候補にしていない。

残り4件のlocal追試はA08のrelease `init`が
`KIO-E-PRIVATE-FILE-UNSAFE-001`で実exit1となり、A09/A10/A12を実行せず停止した。
`Device::new`は各scenarioのprivate/XDG等を0700にするが、その上のscenario directoryを
ambient umaskの0775のまま残していた。保存したmanifestもこのmodeを示し、製品の全祖先に対する
所有・書込権限検査が正しく拒否している。証拠は `linux-s2-remaining-native-v1/`。
外側SSHの終了値はこの回で独立回収できておらず、A08子processの実exitとsource guardを証拠にする。

是正対象は`crates/kio-eval/src/acceptance_failure.rs`のfixture準備に限定する。
全6 scenarioの中間directoryを明示的に0775で作り、既存のprivate creation-parent検査と
0700を要求する回帰testを先に用意した。新規private WSL cloneとtargetで、意図した
`missing-credential/private`の祖先拒否によるRed101を確認した。子processと外側SSHの
実exit101、timeoutなし、承認した1 source以外の一致を回収した。
証拠は `a08-private-scenario-tdd-red-run-v1/`。
Greenではscenarioを最初に作成・0700化し、その後に従来のprivate/scope/XDG/tmpを作る。
製品の検査、全6 scenarioのassertion、期限、依存関係、固定受入matrixを維持する。
同じclone・targetのGreen focused testは1 pass / 0 fail、実exit0、4.08秒だった。
続く`kio-eval --lib`全単体試験は472 pass / 0 fail、実exit0、218.15秒だった。
`clippy -p kio-eval --all-targets --locked -- -D warnings`も実exit0、35.18秒で、
各実行後のsource guardが一致した。子processと外側SSHの実exitを回収し、
27 JSONのstrict parseと回収45 entryのsize/hash一致を確認した。
証拠は `a08-private-scenario-tdd-green-run-v1/`。
この局所Greenを正式A08のnative成功へ昇格しない。

専用CI経路のS2設定案は、Windows PowerShell 5.1のrollback構文検査、Windows sshdの
`-t`と`-T -C`によるrm2c/kio-ciの設定値確認、WSL sshdの`-t`と`-T -C`による
kio-testの設定値確認をすべて実exit0で完了した。共有TEMPのACLは変更せず、既存の安全な
ProgramData祖先の下にprivate検証用directoryを新規作成し、検証後は自身の一致するfileと
空directoryだけを削除した。両OSの既存SSH configのmetadata/hashは前後一致した。
証拠は `c2-s2-native-preflight-execution-v4/root-verification.json`。
これは構文と設定案の検証であり、account/key/configの本適用・専用経路の接続受入ではない。
WSL daemon-global `permitopen any`は、未適用のCI keyの宛先制限の証明でもない。
source bundle・helper build receipt・適用packetは新候補へ結び直してから本適用する。

Windows配布物runner v1はtoolchain引数の結合、v2は検証用子PowerShellのExecutionPolicyで
Cargo前に停止した。v2のpinned rustcは正確な2引数で1.98.0/MSVC hostの実exit0だった。
v3は子processだけに`-ExecutionPolicy Bypass`を指定し、persistent host policyは変更しない。
10件の準備物testは成功したが、v3のnative selftestと配布物試験は未実行である。
旧packetの失敗とraw receiptを保持し、新候補に再生成してから実行する。

Mac空き容量は現在約2.4 GiBで、配布物/native試験の18 GiB開始基準を下回る。
復元した証拠・source・bundle・compiled proofを削除していない。A03/A11のserviceは正式CI待ちで、
localで`GITHUB_ACTIONS=true`を装っていない。account/key/SSHの本適用、push、Actions、
provider/local外部呼出し、正式集計・release公開は未実行。A08是正と直接関連する文書をcommitした後、
repo外で新候補SHA/treeを固定し、同一候補の3 OS全回帰・配布物・security・専用経路・正式受入を揃える。
