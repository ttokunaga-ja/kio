# v1 implementation record

Implementation of the approved [plan](v1-implementation-plan.md) began on
2026-09-07, from `f198c26` on `main`. Entries below distinguish implementation,
local verification, and the final native acceptance evidence.

## Current checkpoint — 2026-09-26

**v1.0 is still incomplete.** This checkpoint prepares the local v1 implementation
milestone from base `d98e0f6`. No push, Actions run, paid-provider call, release,
new route privilege, or new machine privilege occurred in this checkpoint.
Historical results below are diagnostics, not current-candidate acceptance.

### Evidence boundary

- The latest macOS r149 full workspace/all-target run completed with only three
  failing `batch_recovery_key` test fixtures: temporary directories were not
  owner-private. The evaluator passed 477/477, CLI contracts 284/284 and purge
  18/18; all other reported targets passed. Four targets executed zero tests and
  do not count as coverage. The corrected 1,621-path source manifest, Git HEAD and
  status fingerprint were unchanged throughout the run. Linux r156 on immutable
  snapshot `8473e269` completed with the same three fixture failures and no other
  failing target; its source fingerprints matched. The explicit private-directory
  fixture correction passed all three tests in isolated Linux r157 and has now
  been applied to the working tree. Production permission checks are unchanged.
- The completed Daybreak-designated incremental review led to two additional
  correctness fixes, now applied: purge authenticates every chunk-vector owner
  and relevant survivor against retained history, ledger and canonical CAS;
  Windows conversion waits for the entire private Job and performs a final
  scratch scan under the original deadline. Missing attribution stops purge
  before its closure is created. Independent focused review accepted the purge
  repair; Windows ordinary-document exploitability remains unproven and native
  execution remains required. The linear-history GC contract now uses
  `keep_repaired` and rejects the former key without compatibility aliases.
  These changes, their regression tests and operational documentation are applied.
  Final affected validation passed on macOS and Linux: formatting, workspace
  all-target Clippy with warnings denied, core all-target tests, key/recovery
  checks, purge/resurrection and GC CLI suites. macOS also passed native process
  tests and the Windows GNU test compilation check; the latter is not native
  Windows execution. The macOS 1,621-path source manifests are byte-identical.
  Linux r159's anchored result-line total is 478 passes across 26 summaries,
  including child-process test reports; the earlier summary's 231 was an
  arithmetic reporting error, corrected without changing raw logs. All tracked
  Linux source hashes match. Its wrapper marker 97 reflects only `.git/index`
  changing; the source comparison and successful test exits are separately
  recorded. Linux r158's earlier Clippy failure is retained; the final equivalent
  let-chain correction passes lint. Packaged-binary and native Actions acceptance
  remain outstanding.

Prior diagnostics, retained for provenance:

- The resumed macOS r149 run passed Rust 1.98 formatting, workspace/all-target
  Clippy with `-D warnings`, and all **6** supervised-process tests. Its full
  workspace/all-target suite completed with two failing targets: one stale
  approval expectation and seven evaluator tests. Evaluator causes were a
  fixture group-ownership assumption, fork-inherited dispatcher lock references,
  and the default SDK 27 being unsupported by the pinned linker. After correction,
  formatting, workspace Clippy, core/all-target tests, evaluator library **474/474**,
  evaluator main **18/18**, corpus CLI **3/3**, root registration **9/9** and CLI
  watch **4/4** passed. The 1,578-entry validation manifest and original Git status
  were unchanged. This affected-suite result closes those recorded failures;
  it is not a new full-workspace or Actions receipt. Rust acceptance-tools tests passed
  **41/41** in r148; their implementation replaces the twelve temporary Python
  helpers/tests removed after that validation.
- Retrieved Windows r144 logs passed retained-store **13/13**, private filesystem
  **7/7**, atomic crash **3/3**, regular rename **5/5**, with management **20/24**.
  The management fixtures now distinguish offline moves from Windows' deliberate
  retained-handle sharing denial. Fresh r151 could not validate the fix:
  management compilation failed, a fresh LLVM link failed, and application
  control blocked the crash executable (4551). No host policy was changed.
- r151 also exposed **5/13** atomic-store failures from persisted owner identity
  mismatch. Source inspection found JCS converts `u64` through `f64`; a live NTFS
  identity `9851624185183609` demonstrably rounds to `9851624185183608`. Core
  directory/file identities and the analogous persona device fields now use
  strict fixed-width lowercase hexadecimal strings. Regression tests cover this
  exact value, adjacent large IDs and `u64::MAX`; Linux r152 core/all-target and
  generic identity-filter tests passed. The subsequent full run exposed stale
  foreign-platform and materialization fixtures; both fixtures are now corrected,
  and both explicit persona device tests passed in the final macOS suite. Native Windows verification
  remains pending.
- macOS r137 completed with two normalized-fsck failures subsequently fixed;
  focused regression checks passed. Linux r137 also exposed one watch timeout
  and two export-harness deficiencies (missing Git inventory and rustup PATH).
  Linux r150 will use a fresh immutable source snapshot, real Rust 1.98 rustup,
  and an isolated diagnostic Git index containing every approved source file.
  r150 reproduced the watch timeout: Linux OPEN notifications from reconciliation
  re-enqueued its own reads. The adapter now ignores read/open hints while
  preserving mutations, write-close and rescan. Linux r152 passed native-watch
  unit tests and all four CLI watch tests in 7.51 seconds. Its full suite completed
  with three failures in two targets: the two identity fixtures and dispatcher
  lock inheritance. Source hashes matched before and after that immutable run.
- The lab SSH/WSL/Docker connection is live again. Docker reports 29.8.0, no
  containers were initially running. The GPU controller now records a create-only per-run baseline tied to
  GPU UUID/capacity, checks free VRAM before start, and verifies recovery within
  128 MiB after both owned projects disappear. The r153 diagnostic captured an
  8,188 MiB GPU with 1,082 MiB used and 6,875 MiB free. OCR passed HTTPS readiness
  and pinned model/image checks. After stopping it, used memory remained 1,237 MiB,
  above the 1,210 MiB recovery limit; stop/cleanup correctly failed and embedding
  was not started. Both owned project labels had zero containers. NVIDIA reported
  no Linux process explaining the residual use. Subsequent read-only Windows
  WDDM accounting showed desktop GPU allocations; after the original recovery
  gates passed again, one authorized continuation used the same immutable
  baseline and source/helper hashes. Embedding passed HTTPS readiness and its
  pinned image check; stop and final cleanup passed, both project labels were
  absent, and final GPU use was 1,050 MiB (6,907 MiB free). The original failed
  OCR recovery record remains preserved. No unrelated process was killed or
  admission limit weakened. This proves lifecycle recovery, not inference or
  Actions acceptance.
- Controlled-root capture/recheck, retained Windows operation handles, and
  native relative rename are implemented. A GPT-6 Sol static boundary review
  found no concrete vulnerability in those native operations; that result is
  limited to the reviewed paths and does not replace final Codex Security scan.
- Workflow review corrected the artifact ZIP root and provider private-workspace
  creation. A10 publication recovery now requires the journal's exact linear
  child, manifest, immutable raw/tree content and idempotent release recovery.
  Linux r152 passed all 44 acceptance-tools library tests and the provider
  fixture tests. A10's three publication tests passed in the final macOS suite;
  the actual full-driver diagnostic then reproduced a failure at the first
  `atomic_write_staged` cut: release `init` refused legitimate bootstrap gate
  and atomic-write residue as a non-empty unmanaged store. The bootstrap
  admission checks now share the same exact residue validator; a fresh full
  driver run is pending. The failed immutable diagnostic and its synthetic
  source identity remain preserved; no A10 receipt was issued. A zero-test
  filter is not accepted as verification.
- Latest macOS adapter validation passed **366 unit + 7 layout-capture + 2
  token-recall tests** after the transport, OCR, parser and credential-bound recovery changes.
  Focused core checks passed GC identity **2**, CAS removal recovery **7**,
  existing CAS purge/remove **6**, bound CAS **4**, purge-bound **1**, and private
  filesystem **13**. These focused results do not replace a fresh full-workspace run. CLI confirmation
  checks passed **3/3**. Actual purge CLI checks passed ten interruption cuts in
  one table test, plus independent index-authority rejection, old-closure rejection,
  shared-image preservation and cache-link rejection/retry tests. The subsequent full purge suite passed 18/18;
  the final post-review suite passed 21/21 on both macOS and Linux.
- CAS deletion now uses a deterministic verified quarantine entry so a restart
  can finish a removal whose canonical name is already absent. The application
  fixes embedding deletion authority in the durable closure before SQLite
  deletion; old closure format v1 is explicitly rejected. The actual CLI interruption
  table passed; current full regression and native Windows acceptance remain required.
- Real provider clients previously used configuration labels or constant defaults
  for recovery scope and reread credentials between calls. The replacement
  captures credentials once and binds recovery to a separate device-private
  HMAC key, provider origin and credential. Key rotation deliberately holds old
  attempts instead of querying or deleting through another account. This is
  implemented with separate key lifecycle and redacted diagnostics. Adapter tests
  passed; app key tests **3/3** and terminal cleanup **1/1** passed. One new rotation
  test initially used a non-private tempfile directory; its fixture now explicitly
  restricts the newly created directory without weakening the production check.
  Real-provider Actions evidence is still required.
- GitHub live read confirms provider API secret names and each USD 10 limit;
  `v1-provider-acceptance` permits only `main`. `main` is not branch-protected,
  and no local-GPU Environment exists. Existing CI success still binds only
  `1df10f2`, not this WIP. No secret values were read.
- A deterministic fork test reproduced dispatcher lock retention before the
  fix. The handler now explicitly unlocks its scoped gate; all eight dispatch
  tests passed afterwards, including active-owner rejection. The nonblocking
  lock remains mandatory. Test-only identity and group fixtures were corrected
  without relaxing production validation.
- macOS release builds now select SDK 26.5/build 25F70 explicitly and bind that
  SDK release identity in the closed v2 recipe. Ambient SDK overrides do not
  select inputs. The macOS workflows use `macos-26`; local full lint and affected
  tests, including actual nested linking, passed. No global SDK selection
  or host security policy changed.
- Live private-route preflight found broader existing Windows Tailscale firewall
  rules and enabled WSL Unix-socket forwarding. Corrected proposed SSH fragments
  passed each host's parser/effective-policy checks in temporary input only.
  The owner supplied an authenticated Chrome tab. Its live policy has a wildcard
  grant allowing every source, destination and IP protocol/port. The concrete
  replacement restricts Mac and the CI tag to Windows TCP 22 and retains the SSH
  stanza; all wildcard-only access would be removed. The unsaved UI diff was
  inspected then discarded. Official policy-test execution, runtime route tests
  and installation remain unperformed; no Save action or credential creation occurred.
- r96 real Office and authenticated local GPU results remain zero-SHA
  diagnostics. A10 fault acceptance, actual provider lanes, native service and
  same-candidate three-OS distribution receipts are still outstanding.

Durable resumed-run logs are stored outside the checkout in
`~/Documents/Codex/2026-09-26/kio-resume-validation/`. Old temporary logs are not
assumed to survive a machine restart.

### M0--M7 status

| Milestone | Current implementation state | Remaining gate |
| --- | --- | --- |
| M0 | Contracts, case matrix, candidate/receipt checks and Rust acceptance tooling are implemented. | Complete executable mapping and exact-candidate evidence. |
| M1 | Linear store/lifecycle, journal recovery, retained boundaries and controlled-root bootstrap are implemented. | Current full regression, Windows management diagnosis and native acceptance. |
| M2 | Device authority, dedicated approval, grant/trust binding and current policy gates are implemented. | Same-candidate native acceptance and final security review. |
| M3 | Automatic root/child discovery, retained child creation, registration and retirement are implemented. | Windows management boundary tests, full regression and native three-OS receipts. |
| M4 | Native watchers, persistent queue and user-service lifecycle are implemented; Linux read-event regression passes. | Current full regression; actual native scheduler and recovery receipts. |
| M5 | OCR/embedding, Office confinement, local trust and central ledger recovery are implemented. | Validate GPU recovery logic and binary/container compatibility; approved private Actions route and real provider evidence. |
| M6 | Hybrid/image/history search, replica rebuild, CLI and linear managed restore are implemented. | Current full suite and packaged-binary acceptance. |
| M7 | No current-candidate receipt exists. | Same-SHA three-OS Actions, security review, packaging and all mandatory receipts. |

The account advisory reports Daybreak Blue access as granted. The collaboration
runtime rejected invocation for missing `access_programs.cyber=daybreak_blue`.
Subsequent CLI invocations explicitly requested `gpt-daybreak-blue-latest` and
completed, but emitted that the extra `access_programs` configuration was ignored;
they did not expose the resolved model identifier. Requested model and successful
process completion are therefore recorded separately from unverified actual
model identity. No fallback model was intentionally selected. Raw reports and
hashes are preserved. The architecture review used immutable snapshot
`1cbe555c7893b28ecfca8ef43c0ae509c271d5a3`; incorrect source paths were corrected
only in the parent threat model after checking their anchors. Six disjoint CLI
discovery packets completed all 167 assigned changed source-like files and
returned twelve candidate findings. All twelve received independent validation
and all nine non-suppressed candidates received attack-path adjudication. The
immutable-baseline scan is now sealed: two low-severity parser resource-admission
findings, three deferred candidates, and partial coverage. Bounded Rust tests
reproduced missing PDF operand and XLSX sheet admission on the baseline and passed
with the fixed parser files; no OOM was attempted or claimed. This report is not
an acceptance verdict for later edits. After the user switched models and asked
for retry, the new Daybreak-designated collaboration review completed on
`8473e269ba5af2df6f2248722dc21913bb91d64b`. All 50 changed files received one of
four disjoint discovery reviews, followed by independent validation and one
attack-path adjudication. The sealed incremental scan
`ed677cee-f18b-44fc-b944-ac43b269dbe3` reports no new validated vulnerabilities
and retains one distinct deferred Windows renderer candidate. The SDK candidate
was excluded by the explicit trusted-administrator boundary. Requested model is
`gpt-daybreak-blue-latest`; actual resolved model metadata remains unavailable.
The sealed report retained two obsolete checkpoint entries; the supplemental
`completed-scan-reconciliation.md` records their supersession without modifying
the sealed report. Later purge and Windows fixes have separate focused reviews,
not implied coverage by that immutable scan. Tool usage aggregates for the eight
incremental-scan threads are total 29,198,592, input 29,088,097, cached input
27,888,000 and output 110,495 tokens; these are thread aggregates, not incremental
billing or a USD estimate. Independent GPT-6 Sol reviews and Astra integration
are recorded separately; legacy Terra results below are historical only. The private Actions
route proposal exists, but new route credentials/accounts/privileges remain
uninstalled and require approval after its concrete preflight is ready.

The existing independent monitor was resumed at a ten-minute interval with
GPT-6 Luna. Its active prompt requires Daybreak for security work and prohibits
silent fallback. It watches this v1 task read-only and does not treat normal long
builds or unchanged progress as an incident.

## Historical checkpoint — 2026-09-08, r24 / r25

残工程の実行順序と「実端末の手動確認を除く完了条件」は
[v1 completion plan](v1-completion-plan-2026-09-08.md) に整理した。
計画の作成は、実装やActionsの追加完了を意味しない。

**r24/r25 時点でも v1.0 は未完成だった。** 当時のローカル HEAD は `d98e0f6`
(`fix: bound aggregate PDF and XLSX decoding resources`)。app 分離、保存形式、台帳、
管理中 restore、承認・監視の統合差分は引き続き作業ツリーにあった。
以下の「latest」や未実装という記述は、その記録時点の状況を表す。現在の判定は
2026-09-26 の checkpoint を優先する。

### M0〜M7 と Actions の再照合 — 2026-09-08

利用者の指定どおり、native 3 OS の受入は GitHub Actions を基本とする。
「端末側の承認管理」は利用者による3台の手動試験という意味ではない。
runner 上の新規・私有な利用者設定領域を使い、grant の作成、コピー拒否、変更・失効を自動検証する。
ローカルでは push 前に再現可能な検査を通し、Actions で OS 固有の実行結果を確定する。

GitHub を読み直した時点の `origin/main` は `1df10f248b73910349f80f45c30f801b5bfef5f5`。
[CI 34110009796](https://github.com/ttokunaga-ja/kio/actions/runs/34110009796) の Linux/macOS/Windows
全 workspace test と、[Draft 34100074839](https://github.com/ttokunaga-ja/kio/actions/runs/34100074839)
の3 OS配布物 build/verify/smoke は成功している。いずれも今回の未公開実装差分を含まない。
ローカルは5 commit先の `d98e0f6` と作業ツリー差分であり、現在の v1 候補の Actions 成功はまだない。

| 工程 | 現在の到達点 | 完了までの残り |
|---|---|---|
| M0 | 保存・policy・CLI・A01〜A12・OS/lane の契約、再現fixtureを定義 | 専用承認CLIの引数確定、全ケースから実行test/receiptへの対応、Windows native feasibility の証跡 |
| M1 | app分離、format 1.0.0、単一parent/HEAD、journal、履歴/GC等を実装しローカル統合確認 | 最終候補で旧形式拒否・各公開段階の復旧・回帰を3 OSで確認 |
| M2 | root/child所属、現行Ignore/policy、検索/送信の再検証、revoke競合対策を実装 | 専用approve、端末私有grant、scope/profile/destination/trustを通した統一と旧承認経路の撤去 |
| M3 | 空/新規子の発見、共通index、保持したdirectoryに結び付いた子作成を実装 | 残るpath再open経路、大規模探索の継続/再開、Windows固有境界と3 OSの集合一致 |
| M4 | native watcher、永続queue、起動/定期照合、foreground/serviceコマンドを実装 | 実OS schedulerの登録/起動/停止/再起動/解除、欠落/上限/自己通知を含む3 OS受入 |
| M5 | local TLS認証、変換制限、macOS Office crash修正、ledger lifecycle、mock障害契約を実装 | trust登録/更新/失効、中央台帳backup/履歴喪失復旧、Unix資源制限の残り、3 OS実Office/実provider |
| M6 | replica/画像/履歴検索、export、全体/選択restore、preview/競合拒否を実装 | M2のCLI移行への追随とhelp/JSON/終了コードの最終整合、配布binaryでの3 OS受入 |
| M7 | 旧候補に3 OS CI・再現build・基本smokeの実績 | 現在の候補は未受入。必須A01〜A12、security再検証、文書/packagingを同じ候補へ結び付ける |

現行CIは `cargo test --workspace --all-targets --locked` を3 OSで実行するが、
実Officeの `KIO_REAL_OFFICE=1` を設定せず、実provider用のprotected workflowもない。
service CLI testはhelpのみ、配布smokeは version/init/index/text search/open のみである。
したがって、既存workflowの再実行だけでM7を完了とは判定できない。
不足を埋めるjob・receiptの対応は [Actions coverage audit](v1-actions-coverage-2026-09-08.md) に記録した。
今回の確認では workflowの変更・push・dispatch・課金・releaseは実行していない。

### 今回取り込んだ修正

- PDF の object stream 件数、展開・複写総量、page content、CMap、抽出文字列に割り当て前の
  上限を設けた。XLSX の self-closing row/cell も通常要素と同じ上限で検査し、workbook 全体の
  cell 数、文字列量、Markdown 量を制限した。通常の sparse table・数値形式・PDF 復号は維持する。
  この変更と対応する仕様だけを `d98e0f6` にコミットした。
- 外部送信承認の読み取りは純粋な判定になった。config の boolean や `approval_pending` から
  active 行を自動生成・自己修復しない。core の承認公開と revoke は同じ retained store lock を使い、
  revoke 後に古い pending を公開して承認を復活させる競合を拒否する。
- `batch resume/retry --offline` は Markdownize の同期・batch 送信も抑止する。
  `status` は予算と stalled rows を一つの owned read-only ledger snapshot から取得する。
  未初期化の台帳は未初期化のまま表示し、読み取りで台帳や lifecycle lock を作らない。
- 台帳の job 作成前の再開 は upload 済み・job 作成前・同じ intent token の行だけに限定した。
  lifecycle session は終了時に明示的に OS unlock し、fork/exec 中に複製された FD が lock を
  保持し続ける窓を閉じた。異なる terminal cleanup の状態条件は維持する。
- `open/view` は tree が失われた genuine shallow commit の Evidence Pointer を
  `KIO-E-COMMIT-SHALLOW-001` で拒否する。削除 receipt だけでは元の raw/世代/chunk の所属を
  証明できないためである。backup から正しい tree を戻せば通常の検証が可能になる。
  `log/status` の履歴表示と `evidence verify` の shallow 観測は別契約のまま維持する。
- 平文 `plain:` credential は owner-private な設定ファイルを使用時に再読し、読込み済みの
  Adapter 宣言との一致を確認する。unsafe な既存 permission は自動変更しない。
  macOS の extended ACL も private file / scheduler executable の検証に含める。
- `kio-eval` の current-tree attestation は廃止済み branch ref を要求せず、単一 HEAD の
  canonical 形式を検証する。これは機能試験の整合修正であり、研究性能評価とは別である。

### r21 全 CLI 診断からの統合修正

r21 は 43 target 合計 **887 passed / 74 failed**。失敗をすべて取得した後、次を修正した。

- ローカル deterministic embedding が既定の Batch 分岐に入り、存在しない有料台帳を
  `expect` して panic する回帰を修正した。provider Batch の分岐は online execution に限定し、
  ローカル処理は台帳の mutable handle を取得しない。台帳なしの vector search を回復した。
- 台帳がない場合も、既に出力が materialize 済みの未課金 task を Done に復旧する。
  現在の path が secret に分類された task の hold も反映する。既存の課金 reservation claim は
  台帳が利用できるまで消さない。クラッシュ後の再実行・既存 claim 保持の回帰ケースを追加した。
- `index --offline` で mutable ledger を開かないことと、台帳が未初期化であることを分離した。
  offline の enqueue 分類と予算 warning は owned read-only snapshot を使う。
  initialized ledger の7種の leaf の内容と Present/Absent を、ローカル index/vector search が
  変更しないことを検証した。未初期化の場合は課金残高を unknown として保持する。
- 無料 baseline の課金行を廃止した後も、重複 scope identity の拒否を維持するため、
  `index` の GC recovery・承認変更・object/index 公開より前に registry snapshot 検査を置いた。
  ledger の有無にかかわらず競合 scope は変更せず拒否する。
- paid mock / budget 試験は各 fixture で `ledger init` を明示する。共通 command helper での
  暗黙初期化は追加していない。offline/readonly fixture は未初期化のまま保持し、
  fictional baseline billing や config による承認自動作成の旧期待値を除去した。

r22 は security CLI **4/4**、Step2 **106/107**、Step3 **282/283**。
残る2件の原因（offline の台帳誤分類、画像ポインタ fixture の有料OCR初期化漏れ）を修正した。
r23 の変更後は purge **14/14**、purge resurrection **3/3**、time travel **16/16**、
Flate PDF **3/3**、Office **7/7**、P3B **34/34**、ledger lifecycle **8/8**、
offline egress **1/1**。P2C の4件の fixture/期待値修正後、r24 全体は **43 target、964 passed / 0 failed**。
実行前後に app / CLI test source の fingerprint 一致を確認した。
App library **172/172** と関連4 packageの all-targets Clippy は r24 で通過した。

r22 の追加 crash-recovery fixture は最初 macOS の `/var` alias を TaskStore に渡して失敗し、
canonical path に修正した。r23 初回は time-travel fixture helper の引数不足で compile failure、
修正後の rerun1 を上記の結果としている。これらの失敗もログに保持する。

### r25 credential diagnostic の追加修正

r24 実装を合成キーで調べ、閉じ quote の欠けた `tools.toml` の TOML error が
平文 credential を標準エラーと `errors.jsonl` に転記することを再現した。
これは既存 scan snapshot の外で追加確認した不具合で、実 credential は使用していない。
共有 parser は入力本文を含む TOML error Display を使わず、byte offset だけを診断へ出す。
起動時の分類・検証と credential 使用時の再解析を同じ parser に統合した。
Adapter library は **323/323**。CLI の text/JSON 出力と保存 error log を検査する新テストは **1/1**。
最新 Clippy も関連4 package / all targets / warnings denied で通過。
r24 の全 CLI 成功証跡は、この追加 parser 修正前の source に対するものであり、
追加修正の Adapter / CLI diagnostic 経路を r25 で個別に検証した。
再現 fixture は `/private/var/folders/3l/x2mqg_bx7pv8l8lkw2fqrwf80000gn/T/kio-credential-parse-canary-231lulnf`、
修正前の source と検証用候補は `/private/tmp/kio-r25-credential-parser` に保持した。

### 確認できたローカル証跡

| 検証 | 最新結果 |
|---|---|
| Adapter library | r25: 323 passed |
| App library | r24: 172 passed |
| Pipeline ledger library | r19: 77 passed |
| CLI ledger lifecycle / offline egress | r23: 8 / 1 passed |
| CLI ledger / reconcile / P3A | r19: 65 / 4 / 35 passed |
| CLI pending approval / explicit reapproval | r20: 4 passed |
| CLI shallow history pointer | r20: 4 passed（retained-tree 正系を含む）|
| CLI plaintext credential boundary | r20: 1 passed |
| Core approval/revoke concurrency | r18: 1 passed |
| Core restored provenance | r20: 2 passed |
| Native macOS private filesystem / process confinement | r17: 6 / r18: 10 passed |
| Eval missing-normalize attestation | r18: 1 passed |
| Clippy: adapter / app / pipeline / CLI, all targets, warnings denied | r25: passed |
| CLI 全体（macOS）| r24: 43 target、964 passed / 0 failed |

ログは `/private/tmp/kio-v1-focused-r17-*` 〜 `r20-*` と
`/private/tmp/kio-v1-cli-integrated-r21-20260908.log`、`/private/tmp/kio-r22-*`、
`/private/tmp/kio-r23-*`、`/private/tmp/kio-r24-*` に保存している。
r19 の最初の Adapter 実行は新テストの `Debug` 不足で compile failure、次の実行は
旧 fixture が private credential source を渡していなかったため失敗した。修正後の r20 が
上表の成功結果であり、失敗ログを成功と数えていない。

Standard security scan `fc068521-47eb-4c47-9ce1-be8ebbca9911` は完了。
中程度 6 件の元実装の指摘と、現在の修正・ローカル回帰確認を保存した。対象 snapshot は修正前の
`d77d737` worktree に結び付いており、最終候補の security acceptance ではない。
製品 runtime、release packaging、CI の境界を調査したが、研究用 eval orchestration の全体は
未網羅として明記した。レポートは private temporary scan directory の `report.md` にあり、
脆弱性の詳細を公開リポジトリへコピーしていない。
ツールの使用量集計は関連 14 タスクの rollout 全体で 106,374,209 tokens
（cached input 100,588,815 を含む）。今回の追加修正だけの消費量を表す数値ではない。

### 次の実装・受入作業

1. `adapter approve/revoke` へ専用操作を統一し、`index --approve` / `--revoke-network` を除去する。
   device/root/profile/destination/trust identity に結び付いた承認と送信開始・revoke の順序を統合する。
   この CLI 移行はまだ未実装であり、旧オプションは現時点で残っている。
   専用 approve の preview は scope/profile/destination を表示し、index・ledger・provider を起動しない。
   実装前に承認更新の core API を retained directory から呼べる形へ整理する必要がある。
   現行 free function は path を再度開くため、呼出元が保持した管理 root と同じ directory へ
   公開することを最終書込みまで保証するには、path の再検査だけでは足りない。
   私有 grant と scope の current approval ID を両方照合し、どちらかだけのコピー・pending・
   失効前の履歴では許可を復活させない形へ進める。これは次工程の設計メモであり未実装。
2. M3 の残る retained path consumer と大規模 discovery の進捗・再開を完成させる。
3. 中央台帳の history-loss recovery / consistent backup、Unix renderer の process-tree / scratch 総量、
   local peer の trust lifecycle を完成させる。
4. 同一候補で native Windows / Linux / macOS、実 Office・実 provider・service lifecycle・配布物の
   必須証跡をそろえる。push、Actions dispatch、実 provider 課金、OS service installation は未実行。

PersonaScope/personaCorpus による研究性能評価の未実施は v1 の blocker にしない。
補助エージェントが利用上限に達したため、r20 の fixture 修正以降の検証は主担当が引き継いだ。

## Fixed implementation contracts

- Store format: `1.0.0`. A commit has a required `parent` field containing
  `null` or one commit hash. Old `parents` arrays and format `0.1.0` are rejected
  before mutation. No implicit migration, fallback ref, or branch aliases.
- Publication: immutable objects first, then a durable publication journal,
  conditional HEAD publication, manifest publication, journal removal. Recovery
  validates the expected/current/new HEAD relationship and the staged manifest.
  SQLite and replica publication are separate, rebuildable operations.
- Local peer authentication: explicit device CA PEM file; HTTPS loopback only,
  exclusive configured trust roots, certificate name/validity verification,
  redirects and proxies disabled. Trust is never imported from scope contents.
- Scope management and external-send permissions remain separate. Parent
  membership and current ancestor policy establish eligibility; registry rows,
  watcher cursors, and copied inherited policy do not establish authority.
- Runtime API: `kio-app` owns typed commands without a Clap dependency. CLI
  parsing, terminal interaction, and presentation stay in `kio-cli`.

## Work and evidence

### Ledger and restore integration, 2026-09-08

- The billing API now exposes typed domain operations on an immutable ledger
  locator. Operational callers cannot obtain a writable SQLite connection or
  supply generic SQL transactions. Each operation validates and uses one bound
  connection under the retained device lifecycle lock.
- Explicit `ledger init`, exact pending-init `--resume`, and read-only `ledger
  status` are connected. Checkpoints are published before SQL COMMIT; sequence
  mismatch, partial authority, missing schema objects and another era are
  refused without self-healing. Offline local work does not create billing state
  or fictional zero-cost charge rows. History-loss recovery remains open.
- Pipeline local run r5 passed 209 unit tests and 10 policy integration tests.
  This includes initialization interruption, checkpoint-before-COMMIT failure,
  private file/lock substitution, source snapshot immutability and missing
  database with surviving authority. Later cleanup and restore changes require
  an integrated rerun; this is not final candidate acceptance.
- App run r6 passed 166 tests; its sole failure was a test fixture initializing
  directly in a nonprivate temporary parent. The fixture now uses a newly
  created private device directory. The next app compile overlapped the
  authorized export rename, so it does not provide a new acceptance result.
- Windows service XML now starts a manifest-bound runner that passes captured
  device directories to the watcher child. The child is created suspended and
  assigned to a kill-on-close Job Object before execution. macOS/Linux retain
  their native environment configuration. Native service installation and
  three-OS execution evidence remain pending.
- Destination-only `restore` has been renamed to `export` with no alias.
  Managed `restore` is being implemented as a new current-HEAD child with typed
  historical provenance, selected-path guards and a durable working-file
  recovery journal. It is not yet accepted.

| Work | State | Evidence / next check |
|---|---|---|
| CLI preview side effects, conflicting scope flags, budget-month observation | Implemented in `7204da1` | Initial 2 integration tests and rollover unit passed; strengthened test cases added for the integrated rerun |
| Adapter baseline | Local unit tests passed | Adapter library 309/309 in the latest provider-pagination rerun. Runtime fixture credentials are synthetic; real provider acceptance is separate |
| Single parent / HEAD / publication recovery | Implemented; integration validation continues | Core: 198 unit tests and 75 integration tests passed after the retained-lock fixes. Broad CLI acceptance is being rerun against the integrated application |
| Authenticated local adapters | Implemented and independently reviewed | Exclusive device CA and HTTPS loopback. Native Windows peer acceptance remains required |
| Windows retained-handle CAS / child execution | Implementation extended; native acceptance pending | Child enrollment now acquires the retained Windows store lock. Working-file reads retain directory authority. All core Windows targets cross-compile; this is not native execution proof |
| Common app API | Extracted and locally validated | `kio-app` owns runtime requests. App library 161/161 after retained normalized-unit and CAS-before-projection changes. Prior scoped Clippy passed; service changes require a fresh check |
| Current ancestor policy / enrollment / egress split | Application paths implemented and regression-tested | Current, historical, deleted, cursor replay, final response and final query admission tests passed. Empty/deep children enroll under their immediate parent. Parent confirmation grants no child egress. A positive-control historical batch test verifies denied input reserves/sends nothing |
| Native watch / durable queue / service lifecycle | Foreground CLI connected; user service lifecycle implementation and review in progress | `watch run/status/stop`, startup and periodic reconciliation, instance-bound stop, OS-lock liveness, current authority/config reload. Three CLI tests passed on macOS, including empty grandchildren, Ignore, no self-event loop, missed changes, crash/restart and manual-index convergence. Native Linux/Windows acceptance remains required |
| Converter confinement / real-provider paths / operational-ledger recovery | In progress | Both macOS crashes fixed in direct LibreOffice and exact Homebrew wrapper DOCX/PPTX runs. macOS process confinement 10/10. Linux aggregate descendant limits, executable-to-exec binding, native Linux/Windows, real providers and operational ledger recovery remain open; A07, A08, A10 |
| Managed restore / export CLI / projection acceptance | Pending | A05, A06, A09 |
| Exact-candidate three-OS Actions and package acceptance | Pending | A01–A12; no workflow dispatched or provider spending performed by this implementation run |

No row in this file substitutes for an acceptance receipt. PersonaScope and
personaCorpus performance evaluation are outside the v1 completion gate.

The [Office crash investigation](v1-office-crash-investigation.md) records the
controlled native startup diagnosis separately from conversion acceptance.

## 2026-09-08 integration follow-up

`index --offline` now suppresses existing provider-job polling as well as new
submissions. The regression first creates a synthetic Gemini batch job, verifies
that an offline index performs zero additional provider calls and ledger writes,
and then verifies that an online index really polls it. The foreground watcher
uses this local execution mode and does not create network or secret grants.
Authenticated local adapters remain eligible under the existing local contract.

Daybreak Blue reviewed the watcher command, durable queue and current index
entrypoint after this correction and reported no additional concrete
watcher-specific security finding. This does not close the remaining M3 and M5
architecture work or replace three-OS acceptance.

The complete diagnostic CLI run finished with failures in 14 targets. Fixes and
contract migrations below were then verified in focused reruns. This is not yet
full-suite acceptance of a final candidate.

### Regressions repaired after the complete diagnostic run

- `Repository::inspect` and accounted CAS reads now use retained namespaces.
  Raw inspection/copy and content verification stream bounded buffers; corrupt
  reads still consume the verification budget. A retained `ObjectStore` no
  longer holds `.` as an ambient fallback. Chunk and semantic embedding
  read/write paths retain their own namespaces and compare complete object bytes
  on immutable publication. The corresponding replacement/accounting tests pass;
  other bound repair/removal/enumeration APIs and application callers remain M3 work.
- Initial tag creation creates its bounded name ledger before append. Init
  rejects missing HEAD when there is no publication intent to recover. CAS/tag
  fixes passed all 15 basic CLI contracts, 33 snapshot-auto tests and two portable
  name tests. GC after-index tests passed 15/15 after fixtures were aligned to the
  single ancestry chain and retained-identity error classification.
- Managed `.kio` lookup now stats that direct name instead of opening every
  working-tree sibling. Unrelated symlinks and hardlinks no longer prevent init
  or read preflight. Creation is exclusive. Store regular-file reads reject FIFOs
  without blocking. Six directory-capability tests pass, including this regression.
- Current policy failures exclude only the affected scope before egress/ranking;
  healthy scopes return an explicit partial result. Cursor replay rejects a lost
  authority. Duplicate-only scope errors precede vector endpoint resolution.
  Offline/current-policy tests remain 9/9. Step3 improved from 258/282 to 281/282;
  its final online-approval failure received the preflight correction below.
- Explicit `index --online` now rejects absent/revoked approval before indexing
  rather than returning offline success. Membership still authorizes local index
  without a confirmation flag. The Step2 suite passes 107/107 after updating the
  revoked-request fixture to require rejection and an explicit offline retry.
- Mock OCR now emits decodable deterministic PNGs instead of ASCII marked as PNG.
  The ten image-search failures pass without weakening production MIME checks.
- Known failed embedding batches can retry after terminal settlement; known
  in-flight jobs still cannot create a second job. Batch lane tests pass 17/17.
  Daybreak Blue found no new double-billing/unauthorized-send path in this gate;
  live policy, cleanup-token and fresh budget reservation checks remain effective.
- Empty-folder discovery now intentionally includes a VCS-opted-in directory
  containing only a Git marker. Tests no longer expect copied parent policy or
  unsupported Windows child execution. P2C search/ledger contracts pass 46/46 and
  P3B CLI/scope contracts pass 34/34 on macOS. Updated Windows expectations still
  require native execution; cross-compilation alone is not an acceptance result.

`d77d737` is a local milestone commit for bounded process execution and renderer
confinement. It was validated from an isolated checkout of that exact staged
tree before commit. The larger integration work remains uncommitted; no push or
Actions dispatch has occurred.

### Additional unresolved boundaries

- Remaining app/pipeline path-based consumers, end-to-end read accounting, and
  native Windows read barriers. The bound CAS APIs and normal writer leases have
  passed the focused core checks recorded below.
- Terminal Gemini embedding cleanup now replays for attributed terminal inline
  jobs. Missing/unreadable/unattributed jobs deliberately stay pending; their
  operator recovery remains part of M5 operational-ledger lifecycle work.
- M4 user service installation/start lifecycle, M5 device-private approval and
  operational-ledger recovery, M6 managed restore/export, Linux aggregate renderer
  limits, native three-OS runtime/provider acceptance remain unfinished.

### Subsequent ancestry, CLI and recovery work

- Tags and imported ref targets now require membership in current HEAD ancestry.
  Authenticated chunk publication records pass the same final proof before they
  can become rebuild roots. An unborn HEAD cannot be repaired implicitly from
  tags; detached and unpublished future commits cannot be promoted by a tag.
  Nine focused core tests passed. Daybreak Blue found no ancestry-gate bypass.
  The proof is bounded by commit count and verified bytes and uses retained CAS.
- The two time-travel failures were stale cursor-version assertions (`2` versus
  the implemented `3`), not result-count failures. Expectations now match v3;
  selector/cutoff assertions remain. All 16 time-travel tests passed in the next
  retained-history CLI rerun.
- Batch abandon and reset-violations now accept explicit `--yes` for operation
  confirmation. JSON/noninteractive piped `y` cannot grant confirmation. Reset
  rejects send-lane flags and bare retry rejects `--yes`, at both parser and app
  API boundaries. These ledger actions dispatch before provider polling or other
  recovery. All 65 ledger CLI tests passed in the next retained-history rerun. A later
  all-tests build found two parser-test borrow errors; those were fixed before
  restarting the broad run. This flag does not grant egress or override a budget.
- The last online-approval regression passed independently. Integrated library
  results before the subsequent changes: adapter 305, app 152, core 192 passed.
  History/ledger diagnostic results: purge 14/14, export-style restore 11/11,
  time-travel 14/16 and ledger 62/65. Failures above are not counted as passing
  until the updated source is rebuilt and rerun.
- Retained CAS repair/removal/inventory and identity-based shared writer leases
  are being implemented. Security review found repair quarantine races and a
  Windows link-count mismatch, plus stale optional namespace absence; these are
  undergoing correction and are not accepted merely on cross-compilation.
- Gemini terminal cleanup replay now has a narrowly scoped implementation under
  test. It clears cleanup only for the same provider/job/intent, confirmed
  terminal state, and inline output without a provider file. Missing or
  unattributed jobs remain pending. Separately, provider pagination is being
  changed to reject incomplete inventories instead of supplying false absence
  evidence to billing recovery.

Gemini lifecycle reference checked 2026-09-08:
[Batch API REST reference](https://ai.google.dev/api/batch-api#method:-batches.delete)
says deleting an operation does not cancel it; cancellation is asynchronous and
requires status confirmation. Cleanup therefore cannot treat successful delete
as proof that execution stopped. No real-provider request was made for this check.

### Retained publication, normalized units and service follow-up

- Generic writer-lock archives now live under `.kio/internal/locks`, separate
  from GC state. Purge/export publication uses a retained
  `.kio/internal/publication` child with the same OS lock, physical identity,
  nested lifetime and stale-lock recovery protocol. It can coexist with the
  outer writer lock. Daybreak Blue found no lock-introduced authorization bypass.
  The CLI contention fixtures now hold that actual lock rather than fabricating
  the retired publication-lock filename. Core all-target tests: 273/273; core
  Clippy with warnings denied and Windows cross-compilation passed.
- Historical Done-unit reads receive the same retained ObjectStore as their
  manifest. A real Done-unit regression replaces public `.kio` after binding and
  proves the original manifest/unit bytes remain selected without writing into
  the replacement. Promotion, manifest publication, chunk persistence and vector
  replay also receive their existing Repository instead of reconstructing CAS
  authority from a pathname. Remaining path-based consumers are still open.
- Vector publication now propagates immutable CAS failure before writing source
  SQLite or replica additions. A positive/corrupt-CAS regression asserts the
  exact row/vector counts and retention of the corrupt object for diagnosis.
  Search retains the repository used to validate each eligible scope through
  response-time figure loading.
- Fsck reports invalid tags/history publication authority as findings and only
  traverses HEAD ancestry. Rebuild still rejects unauthenticated roots before
  replacing SQLite or changing HEAD. App library tests: 161/161, including the
  missing-tag finding, terminal-cleanup matrix and new vector-publication test.
- `watch service install/start/stop/status/uninstall` is under implementation
  and independent review. Scheduler ownership, exact argument parsing, login
  startup, restart, durable partial-operation recovery, device environment and
  private manifest validation are required before accepting it. Help tests or
  fake scheduler tests do not prove native lifecycle correctness.
- Central ledger inspection confirmed that the current opener can recreate a
  missing operational DB and treat a missing/malformed write-sequence companion
  as an initial observation. This remains an M5 blocker: explicit initialization,
  mandatory device authority/era, strict existing-ledger opening and consent
  binding must be integrated before paid-path acceptance.

Latest broad CLI run: `/private/tmp/kio-v1-cli-broad-retained-r4-20260908.log`.
It is a local regression run, not a three-OS or final-candidate receipt. No push,
workflow dispatch, service installation or real-provider spending has occurred.

## 2026-09-08 r33 checkpoint

This checkpoint supersedes earlier broad-run counts where they conflict. M0--M6
implementation is broad but not fully validated. Explicit paired grants and
managed trust, retained-FD traversal, and authority-bound ledger backup/restore
are under active correction and retest; they are not acceptance claims.

- The r32 full-workspace run failed in 16 targets, including fixture-related
  failures. r33 targeted retests are still running and are not green evidence.
- Native local provisional diagnostics for A01, A02, and A04 passed only with
  candidate-zero inputs. They are diagnostic evidence, not final candidate or
  matrix acceptance. Real Office monitoring has a 10 ms issue under repair.
- The authenticated-local A08 executor, its private execution path, and Actions
  coverage remain incomplete. A fixed 51-case matrix and its verifier exist
  ([matrix](v1-implementation-plan.md), [verifier](../.github/workflows/v1-acceptance-verify.yml)),
  but no one candidate has passed the complete matrix and verifier together.
- The fixed RTX 4060 OCR experiment is bounded evidence only: two public-PDF
  requests returned HTTP 200 and `errorCode=0`, with one page, one block, and
  zero images. Its canonical response hash excluding `logId` is
  `b765ca3a8fd7967d1720e2acb2c97cc2ebdb5a1d8c4e3cf959aafcf33c54e608`;
  required weights matched their recorded hash. Raising `max_model_len` to 4160
  accounts for a 4096-token completion plus a 14-token prompt. Peak VRAM was
  6716 MiB; baseline and post-teardown VRAM were both 575 MiB; no OOM occurred.
  See [measurement summary](artifacts/v1-gpu-4060/ocr/measurement-summary.json),
  [measurement notes](artifacts/v1-gpu-4060/ocr/MEASUREMENT.md), and the
  [weight inventory](artifacts/v1-gpu-4060/ocr/layout-weights.inventory).
- The fixed embedding experiment remains in progress and supplies no pass claim;
  its prepared inputs are under [the 4060 embedding artifact](artifacts/v1-gpu-4060/embedding/README.md).

No paid API spend, Actions run, or push occurred in this round. Manual acceptance
and PersonaScope/personaCorpus performance evaluation remain unclaimed.

## 2026-09-08 r36/r37 checkpoint

This checkpoint records local and evaluator-bound observations; it is not a
same-candidate, three-OS acceptance result.

- The fixed embedding experiment sent two identical text requests and two
  identical public-PNG image requests over the actual local HTTP/TLS path. All
  four returned finite 2,048-dimensional vectors. The images were bitwise
  identical; the native text vectors were not bitwise identical (cosine
  `0.999998480837`), nor were their MRL-768 projections (cosine
  `0.999998786911`). Peak VRAM was 7,213 MiB; cleanup returned to 575 MiB used
  and 7,382 MiB free, with no OOM. The recorded evidence is
  [`artifacts/v1-gpu-4060/embedding/results/attempt1-20260908T0948Z/RESULT.md`](artifacts/v1-gpu-4060/embedding/results/attempt1-20260908T0948Z/RESULT.md).
  This is an endpoint/resource observation, not a bitwise-determinism or
  acceptance claim.
- Frozen-release macOS A07 LibreOffice passed DOCX, PPTX, XLSX, malformed-input
  refusal, and no-outside-writes checks. Its local receipt is
  `/private/tmp/kio-r36-local-acceptance/a07-office-receipt.json`. The host used
  LibreOffice 26.8.0.3 while Actions uses 26.2.5, and the candidate SHA is zero
  with evaluator binding; treat this as local diagnostic evidence only.
- Core A01 passed locally. A02 native convergence failed and remains under
  diagnosis; A03 is still running. The matrix is therefore not green.
- Narrow r36 checks passed: app trust and OCR units, `eval` (21), promotion (7),
  p3a (35), self-heal (4), local OCR (12), and current policy (9). r37 step 3
  reported 284 passes. These counts do not establish the full matrix.
- The authenticated-local executor is wired, but its compilation and correctness
  remain pending. Phased GPU TLS scripts remain under implementation. The
  three-OS Actions route, runner, and secrets are not configured.
- The requested review pattern remains a Daybreak Blue security review followed
  by the root agent's independent crosscheck. Blue identified a harness-config
  and binary-fixture TOCTOU issue; its fix is in progress.

No paid API calls, pushes, or Actions runs occurred in this checkpoint.

## 2026-09-08 r42 checkpoint

The workspace remains RUNNING; this is not a full-v1 or same-candidate acceptance
claim. Daybreak Blue's journal and budget review found no actionable issue. The
proxy P2 unbounded-hold defect was fixed and independently tested two-for-two;
the Python budget suite has six passing tests.

Current targeted counts are adapter 333 and app 195 passing, with other groups
not yet complete. The real controller TLS OCR path is healthy through macOS SSH,
but authenticated Kio GPU execution has not run. Three-OS Actions, the private
CI route, runner, and secrets remain unconfigured. No paid calls, push, or
workflow dispatch occurred.

## 2026-09-08 r54 implementation checkpoint

This checkpoint supersedes r42's local-execution status. It is still not a
release-candidate or three-OS acceptance result.

- Frozen macOS diagnostics passed ten native/mock cases in r48: A01, A02,
  A03, A04 policy, A04 local trust, A05, A06, A09, A08 failure, and A10.
  Their candidate SHA is zero. Logs are at
  `/private/tmp/kio-r48-local-cases.log`; they cannot satisfy the final matrix.
- Kio's actual authenticated GPU OCR phase processed the public PDF and PNG
  successfully. Full embedding acceptance has not passed: the exercise exposed
  a missing original-image reference after text-only OCR and then current-policy
  gaps in direct image access. The failing stores and logs are retained.
- The r53c focused app checks passed image decode/retention (8), unchanged reuse,
  secret hold, final admission, and CAS publication failure (1 each). The CLI
  local OCR suite passed 11 and failed the newly added Ignore/open regression.
  Its repair is under implementation and independent review. r54 then caught a
  pointer-alias type-inference compile error; the fix is written but not yet
  retested. These failures are not green evidence.
- Current repairs add typed immutable image ownership, exact-scope image
  references, current-policy read and send gates, per-Adapter secret grants, and
  authorized alias materialization. Markdown links cannot grant ownership of
  retained image bytes. The normalized-unit schema changes deliberately reject
  missing ownership fields; no implicit migration is introduced.
- Earlier Daybreak Blue reviews covered the GPU controller/dispatcher and
  budget boundary. A new Blue invocation failed because
  `gpt-daybreak-blue-latest` is not supported for the current ChatGPT-account
  session. New product changes are receiving Terra security review and the
  primary agent's independent crosscheck. This fallback is not Blue evidence
  and is not a completed new formal Codex Security scan.
- The offline Python GPU tooling (25) and provider-budget tooling (6) passed.
  Private Actions route code and a concrete configuration proposal exist at
  [v1-private-actions-route.md](v1-private-actions-route.md), but the Windows
  CI user, Tailscale OIDC/ACL route, dedicated key and Environment route values
  have not been installed. Existing Mac SSH access is working.

No paid provider call, push, workflow dispatch, release publication or new
machine privilege was performed in this checkpoint. The cumulative spending
ceiling remains USD 10 per provider across all trial attempts. A same-candidate
Actions matrix, its verifier, and the remaining implementation regression checks
are still required.

### r55b--r58 follow-up

The required ownership schema now passes 33 pipeline markdownize tests, 333
Adapter tests and 14 strict GC inventory tests (`/private/tmp/kio-r55b-*.log`).
The subsequent query allowlist passes all 158 index tests
(`/private/tmp/kio-r57-index.log`), including a depth-one case where a denied
higher-scoring image must not hide an allowed image. Ownership filters now run
before vector ranking/limiting as well as at final result assembly.

The r58 app run completed with 211 passing tests and one failure in 399 seconds.
The new batch-image test failed because its PNG fixture had an invalid CRC;
an isolated run of the same compiled binary confirmed the decoder's rejection.
The fixture has been replaced by the
repository's verified PNG. The test also now persists and reopens the image CAS
and normalized unit. This correction has not yet passed a new Cargo run.

Batch OCR previously discarded embedded images. It now uses the synchronous
lane's bounded parser and a shared borrowed conversion before publishing image
ownership. Local provider-image CAS failure preserves the known job for
recollection. Independent review found the analogous source-image CAS error
was still flattened into provider contract failure; that error classification
is being corrected before batch acceptance. No new paid request was made to
diagnose either case.

## 2026-09-08 r59–r63 checkpoint

This remains an uncommitted local integration checkpoint, not a candidate or
three-OS acceptance result.

- Paid synchronous OCR and document embedding now reserve immediately before
  each attempted request. A definite provider rejection settles at zero; an
  uncertain result is conservatively charged once and durably fenced across
  restart. Ordinary indexing and retries cannot resend that request. Explicit
  `batch retry --resend-unknown <selector> --yes` audits and authorizes one new
  attempt under current scope/profile/policy, retaining the previous charge.
- Embedding request identity uses the actual contextualized embedding hash.
  Recovery resolves stamped attempts by their ledger token, without borrowing
  a current path or another scope's reservation. Old unknown-result authority
  rows survive ledger pruning.
- Full and Incremental OCR results must pass the shared strict response
  validator before normalization. Legal partial responses remain Partial;
  failed-unit retries validate the requested subset and preserve earlier
  immutable units. An accepted all-failed response persists a Failed manifest
  and is not misclassified as a transport failure. Paid Incremental control or
  invalid responses no longer trigger an implicit second Full request.
- Image ownership remains tied to immutable normalized units. Corrupt image
  authority excludes its scope without aborting healthy sibling searches;
  shallow cached text cannot lend authority to images. Exclusions occur before
  cursor scope hashing. Raw-only snapshots cannot authenticate semantic chunk
  pointers, while authenticated current raw access remains available.
- r60 pipeline library tests passed (223). r63 app library tests passed (217,
  with the unchanged 578-scope stress case deferred to the final full run).
  r61c Step3 exposed 14 failures: obsolete billing/ownership fixtures, an
  incorrect JSON field lookup, and the per-scope search regression. Corrections
  passed in r63 (Step3: 284; P2B: 58). The new sync-accounting fixture's actual
  setup failure was an absent TMPDIR, causing registry snapshot creation to
  fail with ENOENT. The corrected fixture creates private operational
  directories beside its managed scope. In r66, both sync-accounting tests,
  both known-unit retry tests, all three local secret-consent tests, and all
  seven object-policy tests passed. Earlier failed runs are retained as failed
  evidence rather than relabeled.
- The narrow independent security review and primary-agent crosscheck found
  and corrected the sync resend/accounting and OCR validation paths. The new
  Daybreak Blue request was rejected by the account backend; this checkpoint
  uses Terra plus the primary agent's crosscheck, and does not claim a new Blue
  result or a completed final Codex Security scan.

The all-failed unit retry issue was subsequently calibrated as common-contract
hardening, not a currently reachable production vulnerability: shipped Mistral
(sync and Batch), local OCR, and deterministic adapters return an empty
`failed_units` list, while debug fixtures exercise the richer accepted-response
contract. Unit retries preserve the actual failure kind and do not borrow an
unlimited retry budget from another unit. Final subset-state preservation and
CLI exit-code checks remain under integration.

The authenticated-local GPU rerun, complete local CI equivalents, final
candidate-bound security review, private Actions route setup, and same-SHA
three-OS receipts remain outstanding. No paid API call, push, Actions dispatch,
new SSH privilege, or release publication occurred in this checkpoint.
