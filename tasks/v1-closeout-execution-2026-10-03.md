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
