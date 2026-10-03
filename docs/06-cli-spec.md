# 06 CLI Spec

## v1 foreground watch

```text
kio watch [--root <directory>] run [--reconcile-interval-seconds <1..86400>]
kio watch [--root <directory>] status
kio watch [--root <directory>] stop
kio watch [--root <directory>] service install [--reconcile-interval-seconds <1..86400>]
kio watch [--root <directory>] service (start|stop|status|uninstall)
```

`--root` は省略時に現在のディレクトリを使う。`run` は明示初期化した管理 root を対象に foreground で動作する。
子 scope を直接指定した場合や管理権限を確認できない場合は起動しない。
OS 通知、起動時照合、既定 300 秒ごとの完全照合を、手動 `index` と共通の処理へ渡す。
空の子・孫フォルダも対象で、Ignore と独立した nested root の境界を維持する。

この実装の監視はローカル処理を行い、新規の外部送信承認や secret 承認を作らない。
実行済み provider job のポーリング・ダウンロード・掃除も行わず、HTTP Adapter（認証済み local Adapter を含む）も利用しない。
`status` は状態を作成せず、OS lock による稼働状態と、観測時刻付きの backend/backlog/最後の成功・失敗を表示する。
`stop` はその起動 instance への停止要求を記録し、進行中の照合が終わった後に停止する。停止要求の受理と終了完了は別の状態である。
強制終了後は OS lock が解放され、再起動時に完全照合して未処理変更を回復する。
OS のログインユーザー service への登録・起動は実装済みだが、native scheduler の実機 lifecycle acceptance は OS ごとに別途必要である。

`service install` は明示初期化済みの root だけを検証して、現在のログインユーザーの native scheduler 登録を device-private manifest とともに作成する。manifest には絶対・UTF-8 の `HOME`、`USERPROFILE`、`XDG_CONFIG_HOME`、`XDG_DATA_HOME`、`XDG_CACHE_HOME` を固定する。macOS と systemd は native definition がその値を child watcher に明示する。Windows Task Scheduler は環境変数を definition の説明欄へ保存せず、絶対 manifest locator だけを hidden runner に渡す。runner は private manifest、現在の executable、managed root/scope、Task Scheduler の定義を照合してから、子 watcher の `Command` にだけこの環境を設定する。Windows の runner は kill-on-close Job Object に子を参加させるため、scheduler が runner を終了しても watcher を孤立させない。登録は login 時に有効だが、この操作自体は実行を開始しない。service ID ごとの private OS lock は install/start/stop/uninstall と manifest 更新を直列化し、install の途中で中断されても保存済みの installing intent を同じ定義だけで回復できる。`start` と `stop` は scheduler の登録状態を変更してから開始／停止し、`stop` は再起動を無効化して foreground watcher の通常の停止要求とは別に扱う。`status` は scheduler の登録状態と watcher の OS instance lock による実際の稼働状態を別々に表示する。root が後で削除されても `status` は device-private watcher lock を照合し、`uninstall` は manifest と完全一致する登録だけを削除して queue を残す。native service 名または definition が foreign に変わった場合は lifecycle 操作を拒否する。

native scheduler の実機 lifecycle acceptance は OS ごとに別途実施する。service は `watch run` と同じ offline local reconciliation を起動し、egress、budget、secret の承認を追加しない。

詳細: [変更検知](12-change-detection.md)、[実装契約](14-v1-runtime-contracts.md)、
[検証状況](../tasks/v1-implementation-progress.md)。

Kio の現在の CLI 契約。

> 関連: [03-data-model.md](03-data-model.md) (`.kio` レイアウト) / [04-pipeline.md](04-pipeline.md) (batch / retry / budget) / [05-runtime.md](05-runtime.md) (検索 / export / GC / purge) / [09-mvp-scope.md](09-mvp-scope.md) (Phase plan)

---

# 1. Core Commands

`snapshot` だけを正規コマンド名とし、action は `create` または `auto` を必須とする。
旧 `commit` alias とaction省略形は受理しない。内部の履歴 object名としてのcommitはCLI surfaceとは
区別する。

```bash
kio init [<path>]                       # <path> (省略時 = カレント) の .kio を作成
kio status                              # ファイル状態 / pending タスク / budget
kio index [--preview|--yes] [--online|--offline] [--realtime|--batch]  # 取り込み。local ingest の確認は preview / --yes。
                                        # --realtime/--batch は turnaround (レーン) の選択で、--online (送信可否) とは別軸。
                                        # 解決順は network opt-in と同形: CLI > scope config > user config > 既定 (Batch)。
                                        # config キーは `[adapter] lane = "batch" | "realtime"` (03 §11)。
                                        # --realtime は OCR と embedding を**両方まとめて**即時レーンへ倒す
                                        # (単価は両方 2 倍。07 §5.3 の 2026-07-24 裁定)。--batch はその逆向き上書き。
                                        # 同じ --realtime/--batch を batch resume / batch retry / repair rebuild-db /
                                        # reindex も受ける (いずれも online 送信を駆動するため)。
                                        # --online は configured Adapter のいずれかに active paired grant が
                                        # あることを index 前に要求する。各 HTTP dispatch は当該 Adapter の
                                        # exact grant を別途要求する。--offline は認証済み local Adapter を含む
                                        # 当該実行の全 HTTP dispatch を止める (正本 07-adapter-spec.md §3)。
kio root register [path] (--preview|--yes) [--resume <OPERATION_ID>]
                                        # 明示的に moved managed root を再登録する。path 省略時は cwd。
                                        # --preview と --yes は排他で必須。preview は registry を作成せず、書込みもしない。
                                        # --resume は --yes と同時にだけ受理し、同じ root の pending journal と完全一致する
                                        # OPERATION_ID だけを再開する。通常の index/init は registration recovery を代行しない。
kio repair cancel-child-initialization [PARENT_PATH] --preview [--operation <OPERATION_ID>]
                                        # 未公開 child 初期化の cancel 可否を read-only 表示する。path 省略時は cwd。
kio repair cancel-child-initialization [PARENT_PATH] --yes --operation <OPERATION_ID>
                                        # 同一 operation の未公開 private stage だけを明示取消する。通常の recovery は
                                        # 利用者の取消意思を推測せず、既存の取消 marker のみを再生する。
kio adapter approve (<tool_id> | --all) [--preview|--yes] [--resume <grant_id>] [--send-secrets]
                                        # 現在の scope の Adapter destination/profile を device grant と対にして承認する。
                                        # selector は positional tool_id と --all のいずれか一方が必須。
                                        # --preview と --yes は排他。いずれも省略した場合は対話確認を要求する。
                                        # --resume は --all と排他で、同一の pending grant と現在の identity にだけ使える。
                                        # revoke 済み grant は resume できず、--send-secrets の選択も元の pending grant と一致しなければならない。
kio adapter status [<tool_id>]          # scope 側 approval、device grant、pending と現在の effective match を read-only 表示
kio adapter trust register --ca-pem <absolute-pem> (--preview|--yes) [--resume <registration_id>]
kio adapter trust rotate --ca-pem <absolute-pem> (--preview|--yes)
kio adapter trust (revoke|status)       # device-local managed CA lifecycle。scope を読まず、status は state を作成・修復しない
                                        # register/rotate は --preview と --yes のいずれか一方を必須とする。
                                        # register の --resume は同一の中断 registration intent だけを再開する。
kio ledger init [--resume]              # new device ledger。既存 state は置換しない
kio ledger status                       # device ledger を作成・修復せず状態を表示
kio ledger backup --to <absolute-directory>
                                        # lifecycle lock 下の coherent, create-only backup
kio ledger restore --from <absolute-directory>
                                        # authority-bound backup から missing device ledger だけを restore
kio ledger recover                      # exact private pending-write journal だけを明示回復
kio batch resume [--recheck-budget|--override-budget] [--online|--offline]
                                        # 中断タスクの再開。--recheck-budget は上限を変更した後に
                                        # 現在の hard cap を再判定して budget pause を再開する。
                                        # --override-budget は current cap 自体を無視するため別用途。
                                        # --online は grant を作らず、各 Adapter の既存 exact paired grant を
                                        # 置き換えない。--offline は provider/local peer を含む全 HTTP dispatch を
                                        # 禁止するため、HTTP を要する照会・出力取得・cleanup も実行しない。
                                        # markdownize online タスクと embedding enrichment パスを両方駆動 (04-pipeline.md §5.4)
kio batch retry [--online|--offline] [--reset-violations <selector> --yes] [--resend-unknown <selector> --yes]
                                        # failed タスクの再試行 (markdownize + embedding。backoff/retry 予算を尊重)。
                                        # --reset-violations = 検証済み Adapter 更新後に contract_violation_count を 0 へ戻す
                                        # (selector は abandon と同形: intent_token または 4 組タスクキー — 曖昧時は拒否。
                                        # terminal な sync 行は token NULL 化済みのため 4 組キーで指定 (04 §5.4)。
                                        # 変えるのは count のみ。端末確認または `--yes` 必須（非対話/JSON は `--yes` 必須）。`--yes` はこの確認だけを省略し、egress や budget 承認を与えない — 04 §5.8。監査は cost-ledger の outcome 列に残る)
                                        # --resend-unknown = 結果不明で terminal 化された同期リクエストに、一度だけ fresh request を許可する。
                                        # selector は abandon と同形で、現在の scope・active profile・現在も送信可能な exact task/group に限る。
                                        # この操作自身は provider に接続しない。監査を先に記録して ledger の one-shot fence を CAS 更新し、
                                        # 対象だけを Pending へ戻す。次の明示的 online 実行が current policy / grant / secret grant / budget を再検査し、
                                        # 新しい reservation で送信する。OCR の明示再送は以前の incremental 応答を繰り返さず Full request を選ぶ。
                                        # 以前の同期送信は既に課金されている可能性がある。
                                        # `--reset-violations`、`--online`、`--offline`、`--realtime`、`--batch` とは併用不可。
kio adapter revoke (<tool_id> | --all)  # 対応する scope approval/pending と device grant を revoke。
                                        # selector は approve と同じく positional tool_id と --all のいずれか一方が必須。
                                        # 対象なしは no_target の冪等成功。再承認は fresh paired grant を要する。
kio batch abandon <intent_token|scope/adapter/input_hash/tool_profile_hash> [--yes]
                                        # 照合が恒久不能な in-flight Batch job の打ち切り (estimated 記帳 + terminal 化。
                                        # 指定子は intent_token または batch_requests の 4 組タスクキー (3 組では別
                                        # profile 行と曖昧 — 曖昧時は拒否して token を要求)。tasks.jsonl の task_id は
                                        # 喪失許容のため使わない。kio status が stalled 行の token を表示。
                                        # 端末確認または `--yes` 必須（非対話/JSON は `--yes` 必須）。`--yes` はこの確認だけを省略し、egress や budget 承認を与えない。残骸掃除完了まで intent_token は保持 — 04-pipeline.md §5.8。
                                        # 対象行が無い場合 (terminal 確定済み・device 行の剪定後を含む) は
                                        # 対象なしの冪等成功 — exit 0 + 「対象なし」表示 (04 §5.4))
kio repair rebuild-db [--online|--offline] [--realtime|--batch]   # SQLite 再構築 (10-operations.md §7.5)。
kio repair verify-objects [--prune-orphans [--yes]]               # CAS 整合性検証。
kio repair registry-prune [--yes]                                 # 恒久到達不能な registry stale 行の確認付き退役 (10-operations.md §3)
kio repair recover-restore                                        # 中断した managed restore の journal を明示回復
kio repair all | kio repair -a                                    # device 内の全 indexed scope を objects 検証 → scope SQLite 再構築 → replica 完全再射影の順で修復。
kio repair replica | kio repair -r                                # device replica だけを全 indexed scope の source SQLite から完全再構築。
                                        # 操作は sub-command。exactly-one と入れ子 (--prune-orphans は verify-objects の下、
                                        # online/offline・realtime/batch は rebuild-db の下) は構造で保証される。
                                        # rebuild-db は rebuild 後に enrichment を駆動し得るため online/offline・レーン上書きの対象 (07 §3・04 §5.4)。
                                        # all / -a と replica / -r は device-global であり CWD が scope 内であることを要求しない。
                                        # 対象は scope-registry の indexed=true 全行 (participates_in_global_search=false も含む)。
                                        # registry を開けない場合は current scope へ縮退せず command 全体を失敗させる。
                                        # all は新規 online 送信に加えて既存 Batch job の provider poll / remote cleanup も行わない。
                                        # verify-objects の非破壊側修復、rebuild-db 相当、
                                        # replica 完全射影だけを行い、--prune-orphans / registry-prune は含めない。
                                        # replica / -r は各 .kio を read-only の複製元として扱い、HEAD・objects・sqlite.db を変更しない。
                                        # --prune-orphans = どの manifest からも参照されない orphan prepared/image の削除
                                        # (確認プロンプト必須 — 10 §7.5.1。法務 purge の完結手段)。
                                        # 破壊的 2 操作は削除前に**対象件数を提示して確認**する (--yes で省略)。
                                        # 対象 0 件はプロンプトなしの冪等成功。非対話 (isatty=false) で --yes 無しは
                                        # KIO-E-CONFIRM-REJECTED-001 で拒否し、**何も削除しない**。
kio gc --dry-run                       # Phase 4 milestone 1: retention による shallow 候補のread-only plan (§6.1)
kio gc --dry-run --prune-unreachable [--json]
                                        # Phase 4 milestone 8: Rust-only unreachable-object read-only inventory (§6.2)
kio snapshot create [-m "<message>"]    # manual snapshot。-m省略時は自動 message
kio snapshot auto                        # Phase 4 milestone 4–5: OS scheduler invoked auto snapshot / explicit on_idle GC
kio log [--at <commit>] [--since <dur>]
kio diff <a> <b>                        # raw/path 差分 + derived-only 差分 (下記の差分種別)
kio search "<query>" [options]          # 詳細 §3
kio open <pointer|chunk_hash|raw_hash>  # 原本を解決し path を返す。OS アプリは起動しない。§1.1
kio view <pointer|path> [--at <commit>]  # 全文 view のパス + view-local span を返す (05 §1.7.2)。本文は返さない
kio inspect <hash>                      # object を JSON で表示
kio export <evidence|path|commit> --to <dir> [--force] # 詳細 §5
kio restore <source> [--path <file>]... [--delete-missing <file>]... [--expected-head <hash>] [--preview] [--message <text>] # 詳細 §5 Managed restore
kio tag <name> [<commit>]               # 論理名を refs/tags-v1/names.jsonl (truth) に append してから
                                        # canonical ref を作る (書込順序固定 — 03-data-model.md §2)
kio tag --delete <name>                 # canonical ref を .kio/.lock 下で atomic に除去。names.jsonl の
                                        # 行は残す (監査保全 — 「ref の無い names 行 = 正常」と整合。
                                        # 付替えは削除 → 再作成の 2 操作 — 専用 retarget は持たない)
kio purge <path|--raw-hash <h>> --reason <reason> [--erase-tombstone] [--yes]  # 詳細 §6 (確認プロンプト必須 — --yes で省略)
kio reindex [--regenerate] [--at <commit>] [--yes] [--online|--offline] [--realtime|--batch]  # --at = 過去 snapshot の embedding 再生成 (05-runtime.md §1)。
                                        # --regenerate = 新 gen で再 normalize / 再 embedding (Step 3)。
                                        # (2026-07-24 改名: 旧名 `--force` は `export --force` = 出力先の上書き、という別軸の同名だった。
                                        #  Stable 前のため alias は残さず削除)。
                                        # --regenerate は first-instance-wins の
                                        # 明示経路で gen+1 の新 instance を作る (07-adapter-spec.md §9。もう 1 つの合法経路 =
                                        # prepared_hash 変化起因の自動 gen+1 — 03-data-model.md §2.1)。
                                        # 上書きチェーンは manifest の parent_gen (同一 raw 内) / parent_instance
                                        # (raw 跨ぎ incremental の三つ組 — full では null) で永続記録 — parent_run_id は
                                        # task cache の揮発情報 (03-data-model.md §8、09-mvp-scope.md §5.1)。--regenerate は確認プロンプト必須 (--yes で省略可)
kio evidence verify <pointer> [--strict]
kio evidence verify --batch <pointers.jsonl> [--strict]  # <pointer> と --batch は exactly-one / 相互排他。alias・fallback はない (§7、08 §4.3)
kio evidence retarget <pointer> --at <commit> # exact-only read-only retarget。--at は full canonical sha256 commit 必須。--latest/default/alias/fallback はない (08 §5)
```

本表はコマンド全量の spec である。MVP での採否・実装 Step の正本は [09-mvp-scope.md §1 / §3.1](09-mvp-scope.md)。

2026-10-03実装照合: `tag --delete` は上記の要求契約であり、現行の `kio-cli::TagArgs` と
`kio-app::Command::Tag` は作成経路だけを接続している。削除を実装済み・配布済みと扱わず、
[v1完成と後続改善の計画](../tasks/knowledge-ux-implementation-plan-2026-10-03.md) のK2で既存要件の
残差として解消する。tag作成によるretention保護と、tag解除・引用への影響表示の完成は別である。

### Device-global ledger recovery

`kio ledger recover` は scope と `.kio` 設定から独立した device-global command である。成功時の
`--json` 出力は常に `{"status":"ready","recovery":<snake_case>,"path":<path>}` とし、
`recovery` は `no_pending`、`aborted`、`restored_before_checkpoint`、
`finalized_committed`、`published_after_checkpoint` のいずれかである。`no_pending` は current
authority/checkpoint/database の strict validation を通した冪等成功である。

通常の `ledger status`、search、その他の通常操作は pending write を自動回復しない。pending
write がある間の通常 write は fail-closed であり、operator は `ledger recover` を明示実行する。
回復は private journal の exact sequence と旧/新 security token、authority、checkpoint、database
state の組だけを受理する。authority/schema の不一致、破損、未知の状態は blocked のままにする。
回復は checkpoint の巻戻しまたは publication/finalization だけを行い、SQL replay、送信の再実行、
unknown cost の返金を行わない。backup/restore の artifact 契約は
[10-operations.md §7.5.2](10-operations.md) を正本とする。

`snapshot` は action を必須とする。`kio snapshot`、`kio snapshot -m ...`、`kio commit ...` は旧surfaceであり usage error (exit 2) で拒否する。`create` だけが manual message を受け取り、`auto` はmessage引数を受け取らない。

定期設定は省略時disabledである。存在する `[snapshot.auto]` は厳格に `enabled` (boolean)、`interval_seconds` (integer, 1..31536000)、`on_change_threshold` (integer, 1..1000000) の3 fieldすべてを持たなければならない。`[snapshot]` / `[snapshot.auto]` のunknown field、欠落、型違い、範囲外は schema validation error (exit 2) とし、aliasや暗黙defaultはない。

`snapshot auto` はmanual messageを受け取らず、現在のscopeにvalidなsource indexが無い場合は
`status=skipped, reason=not_indexed`、config欠落/disabledなら`reason=disabled`でread-onlyに
終了する。ただしpending checkpoint exchangeがある場合、disabledは
`KIO-E-SNAPSHOT-AUTHORITY-CHANGED-001` / exit 3で無変更のまま拒否する。
pending中は公開state不在もfresh runとみなさず、enabledな明示実行が専用lock下でindex / GC preflight / eligibilityより
先に回復する。通常writer / GCとread-only state検査はpending中にfail-closedする。
eligible分類とstate/normalization/locking契約は[05-runtime.md §8.2](05-runtime.md)を
正本とする。成功JSONの固定fieldは次である。

```json
{
  "operation": "snapshot_auto",
  "status": "skipped|baseline_recorded|not_idle|noop|completed|deferred",
  "reason": "disabled|not_indexed|not_eligible|first_observation|working_set_changed|idle_threshold_not_reached|tree_and_tool_lock_unchanged|snapshot_created|idle_gc_completed|no_gc_candidates|max_runtime_seconds|gc_failed",
  "publication_status": "not_started|completed",
  "snapshot_status": "not_started|noop|completed",
  "eligibility_reason": null,
  "eligible": false,
  "change_count": null,
  "next_eligible_at": null,
  "commit_hash": null,
  "tree_hash": null,
  "stats": null,
  "working_set_digest": null,
  "idle_observed_since": null,
  "idle_observed_seconds": null,
  "idle_threshold_seconds": null,
  "idle_eligible": false,
  "recovered_gc": false,
  "recovery_pending": false,
  "gc": null
}
```

`gc.mode="on_idle"` は enabled `[snapshot.auto]` と indexed scope の OS-scheduler invocation だけで
activation する。first baseline / digest change は state の記録・resetだけでGCを実行せず、unchanged
digest が idle threshold 以上ならGC eligibleである。`kio index`、manual snapshot、preview、失敗、partial
index、`after_index` は cofire しない。GCはsnapshot writer publicationとlock releaseの後に実行する。
`[gc]` 不在は `manual_only`、存在時はmode必須であり、mode別の strict field rules / ranges は
[05-runtime.md §2.3](05-runtime.md)を正本とする。

eligible resultでは`eligibility_reason`は
`first_run|interval_elapsed|change_threshold|interval_and_change_threshold`、`change_count`はinteger、
`next_eligible_at`はcanonical UTC seconds、
`completed` / `noop` はcommit/tree/statsを該当値へ置換する。usage/configはexit 2、clock/lock/state/authority
競合はretryable exit 3、unsafe filesystem/store corruptionはexit 4である。scheduled mutationの
実装経路はmacOS / Linux / Windowsにあり、その他ではlock・HEAD・state publication前に
`KIO-E-SNAPSHOT-PLATFORM-UNSUPPORTED-001` / exit 4でfail-closedする。Windows checkpoint置換は
独立した`gc/internal/snapshot-state-exchange/`の5状態journalとexact HANDLEで行い、単純なreplace renameを
atomic exchangeとしない。persisted scope / `.kio` identityとHEAD / tool-lock / config / ignore byte digest・
GC semantic digestを再検証し、値が変われば拒否する。正確な値を戻したfileは旧inodeだけでは拒否しない。
stateの許可された単調遷移とactual targetを検証し、旧source退役・prepared名不在までintentを保持する。
Windowsはnative 3 OS Actions acceptanceとsecurity review待ちであり、process interruption検証を
power-loss耐久性の証明としない。詳細は[05-runtime.md §8.2](05-runtime.md)に従う。
eligible attemptのdurable state CASはimmutable object準備後かつHEAD/ref/manifestより前に行う。
state競合ではrefを進めず、state成功後に別のauthority再検証が失敗した場合は保守的cooldownとして
stateを残し、ref不達objectを履歴authorityとして扱わない。

**`kio diff` の差分種別**: raw / path の差分に加え、tree schema v2/v3 ([03-data-model.md §8](03-data-model.md)) が生む derived-only の変化 — `normalize_manifest_changed` (unit の failed → done 完成を含む) / `chunking_config_changed` / `chunk_set_changed` (公開 chunk 集合のみの変化) / `tool_lock_changed` (旧新 tool_lock_hash と変更 role) / `resurrection_published` (no-op 例外 (a) の publication commit — [05-runtime.md §8.1](05-runtime.md)) — を差分として表示する (`--json` も同種別を持つ)。derived-only commit を「差分なし」と表示してはならない。current tree の required field が欠落する場合は `unknown` へ縮退せず corruption / incompatible format として fail-closed にする。

`kio init` は指定フォルダ (省略時 = カレント) の `.kio` を 1 つだけ作成する (子孫には作らない)。自動子 scope mutation が RC 対象の platform では、子フォルダの `.kio` は `kio index` の探索が対象を検出した時点で必要に応じて生成される (**VCS repo root 配下には既定で生成しない**。既存子 scope を grandfather する分岐は置かない — [03-data-model.md §3](03-data-model.md))。この結果、深いフォルダ木では scope 数が多くなる。`kio search` のデフォルトが全 indexed scope 横断である ([05-runtime.md §1.8](05-runtime.md)) のはこの帰結を受けた設計である。また `kio init` は生成する `.kio/config.toml` の `[chunking] unicode_version` に実装同梱の UCD 版 (現在の既定 = 17.0.0) を明示記録する ([03-data-model.md §5.3](03-data-model.md) — 省略不可・default なし。schema でも required — [10-operations.md §11.3](10-operations.md))。

明示 root の `init` は中断後も同じ root に対してだけ再開する。初期化中の root は通常の管理操作で
利用できず、既知の初期化記録と directory identity が一致しない `.kio`、旧形式、または unmanaged な非空 store は
変更せず拒否する。直近の managed parent が同じ child を enrollment したまま child の `.kio` が失われた場合も、
独立 root への作り直しを拒否する。直近 parent のみを認証して lease を保持し、既に有効な Child は冪等に開く。
read-only command は初期化残骸を cleanup しない。

### Controlled root

管理済み root を受ける全 command（`status`、`search`、`preview` を含む read-only command も含む）は、root と
ancestor が current user または trusted OS administrator により controlled であることを capture / recheck 時に検証する。
read sharing だけの `0755` は許容するが、他 OS principal が write、delete、または ACL mutation できる場合は
`KIO-E-MANAGEMENT-ROOT-UNSAFE-001` / exit 4 で mutation 前に拒否する。Kio は既存 root を chmod、chown、re-owner、
または ACL repair しない。`.kio` の exact current-owner private 規約はこの共有 read 規約より厳格なまま維持する。
v1 は multi-user ACL mode を定義しない。この制約は将来の local OS folder model を約束するものではない。

### Root registration

`kio root register [path] --preview` は、指定 root（省略時 cwd）の live management record と enrolled subtree を
read-only で検査し、detach の有無、対象 scope 数、rebind 先、grant revoke を表示する。registry が無い場合も
cache miss として扱い、作成・repair・row upsert をしない。preview 出力や過去の出力は mutation authority にならない。

`--yes` は subtree 全体を一つの明示 operation として再登録する。対象が moved Child ならその scope を独立 Root に
detach し、配下の Child は reciprocal enrollment token と parent 関係を保ったまま新しい root_scope_id に束縛する。
旧 root path と registry の各旧 row を実地検査し、同じ scope_id の live duplicate があれば拒否する。cross-volume の
移動もこの明示 operation だけが扱える。通常 discovery/index は cross-volume の child を採用せず、再登録を暗黙に
開始しない。

操作は知識 object、HEAD、scope approval を変更せず保持する。各 management record の
`registration_generation` を必ず 1 増やし、path / directory identity / root authority を更新する。送信 authority は
持ち越さず、対象 subtree の device-global central grant を revoke し、registry は atomic rebind 後に unindexed として
再登録する。pending marker と root journal は phase ごとに相互照合する。中断時は marker、journal、before/after
record、旧 path、現在 identity のいずれかが一致しなければ fail-closed であり、`--yes --resume <OPERATION_ID>` 以外は
回復を進めない。current v2 management record より古い形式、missing `registration_generation`、または unknown field は
互換読取りせず拒否する。

本操作は v1 の単一 authority subtree に限る。multi-authority の統合・継承・移送を定義しない。

root register は active な root/child 初期化 operation、cancellation marker、または未回復の atomic state を
含む root を拒否する。通常の planner は read-only で、lock や archive を作成せず、残骸を cleanup しない。子の pending 初期化は current Ignore により
inert になっても他の child を妨げず、後に同じ planned identity が policy admission を満たす場合だけ再開する。
取消は未公開かつ親 private の exact stage だけを対象とし、child root や user file を削除しない。stage を
退役する前に、その stage の verified exact case probe だけを cleanup する。公開済みの pending `.kio` は cancel
できず、Ignore が外れた後に通常の recovery で再開する。

子 scope 経路を含む v1 platform support policy の正本は [09-mvp-scope.md §1.2](09-mvp-scope.md)
である。macOS / Linux / Windows の自動管理は、retained directory capability から child を検証し、
保持済み Repository を共通 app 層へ渡す。public pathname の再解決や `current_dir(path)` 型 handoff
は使わない。現行 policy・所属・identity の検証に失敗した child は採用しない。各 OS の native Actions
受入は別途必要である。

公開 `index` の JSON は `child_scopes` と `child_scope_discovery` を含む。通常実行の discovery は
`page_size` (現在 128)、`has_more`、`continuation_committed`、`restarted_from_root`、`rows_total`、
`rows_omitted` の閉じた object である。`rows_total` は報告対象となった子候補の全行数で、作成成功数ではない。
`rows_omitted` は表示上限により `child_scopes` から省略した行数である。`has_more` は継続すべき探索・処理が
残ることを示す。`continuation_committed` は少なくとも一つの完了ページについて継続状態を保存したことを示し、
最終ページで状態を消した場合も true となる。この値だけで全体の成功を判定してはならない。
`restarted_from_root` は保存済みの探索状態を現行条件で再使用できず、root から再探索したことを示す。
再探索後に全ページが完了すれば `has_more=false` と両立する。

`index --preview` の discovery は `page_size`、`has_more`、`preview=true` の別の閉じた object であり、
継続状態を保存しない。評価用の平坦な fixture は通常実行の完了形を検証する：子行数と省略数がともに 0、
`has_more=false`、`continuation_committed=true`。未知 field、未完了状態、preview の取り違えを受理しない。

`<pointer>` 引数の受理形式 (URI / inline JSON / stdin / hash 短縮形) は [08-evidence-pointer-spec.md §2.3](08-evidence-pointer-spec.md) を正本とする。

本節が CLI コマンドの **正本一覧** である。他 spec が新しいコマンド・フラグに言及する場合、本節への追加を伴う (破壊的変更扱い)。

`kio tag` の新規 `<name>` は OS 非依存の portable leaf 規則に従い、実装同梱の UCD 版で未割当の
code point を含む名前を拒否し ([03-data-model.md §2](03-data-model.md) と同一規則 —
`KIO-E-CONFIG-USAGE-001`)、Windows 予約名・禁止文字・
末尾 dot/space を拒否する。NFC 正規化 + Unicode simple case folding (locale 非依存 —
[03-data-model.md §2](03-data-model.md) と同一規則) が同じ tag は case-insensitive collision
として重複作成を拒否し、`HEAD` の case variant は予約する。canonical ref は
`refs/tags-v1/tag-<digest64>` の 1 表現だけに保存する。raw-name ref や第二物理 leaf を読む分岐は持たない。
物理 ref leaf は [03-data-model.md §2](03-data-model.md) を正本とする。

## 1.1 open の原本解決

`kio open <pointer|chunk_hash|raw_hash>` は以下の順で原本を解決し、OS アプリを起動せず
path を返す。human mode は path を1行で出力し、`--json` の pointer 成功時の必須形は
`{status:"opened", path, raw_hash, chunk_hash, temporary, commit_shallow, manifest_missing}` である。
object URI / hash 短縮形は同じ `status` / `path` / `temporary` に `object_type` と該当 identity を加える。

```text
1. pointer を解決して raw_hash を得る (08-evidence-pointer-spec.md §3)
1a. object URI (kio://<scope_id>/object/image/<image_hash> — 08 §2) の場合: type / hash を検証し
   (**MVP で発行・受理される object URI は type=image のみ** — 08 §2.3。他 type は
   KIO-E-CONFIG-USAGE-001 (exit 2) で拒否)、
   宣言された scope_id を通常解決する。別 scope に同じ image_hash の bytes が存在しても
   scope authority を差し替えない。scope が到達不能なら `KIO-E-EVIDENCE-SCOPE-UNREACHABLE-001`
   (exit 3) で拒否する。image object を `~/.cache/kio/open/image/<image_hash digest64>/` へ
   read-only materialize し、その path を返す (dir キーは image_hash — **`image/` の type segment で raw 系 dir と
   分離する**。raw と image は同一バイト列で同一 digest になり得るため ([03-data-model.md §1](03-data-model.md)
   のバイト列 content hash — CAS は `objects/<type>/` が分離を担うが cache は担わない)、segment なしの
   平坦 namespace では衝突する)。**materialize の書込・照合は raw の一時展開 (下記) と同じ規約に従う** —
   private temp → no-replace publish・EEXIST は image_hash の再計算照合・不一致は同じ fail-closed
   終端 (KIO-E-STORE-CORRUPT-001 / exit 4)・拒否時 cleanup (dir key と照合キーは image_hash)。
   **返却直前に journal barrier と現行 policy / scope / HEAD を再検証する**。画像には現在の policy で
   許可された、履歴から認証できる typed ownership (`owned_image_hashes`) が必要である。Markdown に
   URI を書くだけでは読み出しを許可しない。tombstone は raw_hash 単位の
   marker であり image には適用しない (同一バイト列の無関係 raw の tombstone に image_hash で
   照合しない)。image の purge 帰結は object の物理削除 (live 参照 0 — [05-runtime.md §3.5](05-runtime.md))
   そのものが表し、object 不在は手順 5 と同じ not_found / exit 4。purge closure には「closure で
   物理削除対象となった live 参照 0 の image」の cache dir として含まれる
   ([05-runtime.md §3.5](05-runtime.md))。以降の手順 2-5 は raw 系入力のみ
2. tombstone 判定 (最優先): raw_hash の **canonical final event が `purged`** (全 marker の正本化 —
   08-evidence-pointer-spec.md §3.1 手順 5) なら、working tree・cache の状態に
   関わらず §7 の規約どおり exit 4 — purge 済み原本が folder に残っていても Kio 経由では開かない
   (canonical が `retired` (退役) なら対象外 — 再 ingest による退役は 05-runtime.md §3.5 の resurrection 規則)
3. working tree 解決:
   immutable tree の帰属と現在の policy により選択した正確な path が通常ファイルとして存在し、
   bytes の再 hash が一致すれば、その path を返す (`temporary=false`)。許可された別名と内容が
   同じでも、Ignore 対象の別名を探索して返さない。選択した path が移動・変更済みなら CAS 展開へ進む。
4. 一時展開 (working tree に存在しない = 削除済み・過去版・raw_hash 直指定):
   raw object を ~/.cache/kio/open/<raw_hash digest64>/<basename から導出した portable leaf> に
   read-only で展開し、その path を返す (`temporary=true`)。basename の拡張子は caller が
   OS のアプリ関連付けを利用できるよう保持するが、元 basename 自体は物理名に使用しない
   (path_at_commit が無い場合は kind から推定した拡張子)
5. raw object が not_found → §7 の規約どおり exit 4
```

一時展開は **export ではない**: working tree に書かず read-only であるため、[§5](06-cli-spec.md) の安全要件 (`--to` 必須 / `--force`) の対象外。**展開は同じ `<raw_hash digest64>/` 配下の private temp に書き (purge closure が temp ごと掃く)、cache path へ no-replace で publish してから、path 返却直前の最終検査 ([05-runtime.md §3.5](05-runtime.md) の 3 点) を行い、通過した場合のみ返す — 検査で拒否した場合は publish 済み cache を dev/inode 対照 (自らの publish と検証) の上で除去し、temp も残さない** ([04-pipeline.md §1.1](04-pipeline.md) の temp 掃除規約)。**publish が既存 cache と衝突 (EEXIST) した場合** — MVP では cache が自動掃除されないため同一 raw の再 open で通常発生する — は [04-pipeline.md §1.1](04-pipeline.md) の no-replace 規則と同じく既存との内容一致を照合して自分の temp を破棄し、既存 cache を対象に返却直前の最終検査以降を続行する (**照合 = 既存 cache leaf の内容 sha256 が dir key の raw_hash と一致することの再計算** — 展開 leaf は raw object の byte 列そのもの。**不一致は改変・破損の残骸として KIO-E-STORE-CORRUPT-001 / exit 4 で fail-closed に終端する** (§7 の「4 = 再試行で進展しない」— 回復はユーザーの cache 削除。context に cache path と「削除後の再実行で回復」を載せる)。既存 cache には触れず自 temp も残さない。この経路の検査拒否では cache を除去しない — 除去は自らの publish と検証できた場合に限る。削除主体は purge closure と [10-operations.md §7.5.1](10-operations.md) の残骸回収)。**返却直前検査で拒否した場合の終端は拒否理由の code に従う** — tombstone 検出は手順 2 と同じ §7 規約どおり exit 4、active journal は KIO-E-PURGE-JOURNAL-ACTIVE-001 (exit 3 — 回復後に再試行可)。publish 後検査により purge 完遂後の平文 cache path の**返却**を閉じる (publish と検査の間の crash による cache 残存は返却には至らず、`kio repair verify-objects --prune-orphans` が purge 済み raw の cache 残骸として回収する — [10-operations.md §7.5.1](10-operations.md)。検査通過後の purge は caller が既に開いた path と同格)。展開先はキャッシュであり、tree-only の `on_idle` GC 対象には含めない。必要ならユーザーが削除してよい (正本は `objects/` に無傷)。**purge はこの展開 cache を削除 closure に含める** ([05-runtime.md §3.5](05-runtime.md))。永続的なコピーが必要な場合は `kio export <pointer> --to <dir>` を使う。caller は `temporary=true` を、一時展開であり永続コピーには `export --to` が必要という機械判定に使う。

---

# 2. 初回スキャン承認 (init / index preview)

未承認 scope に対する `kio index` は、raw object 保存・Adapter 実行を始める前に **対象範囲 preview** を表示し、明示承認を要求する。

```bash
kio index --preview     # preview のみ。何も書き込まない
kio index --yes         # 非対話: ローカル取り込み承認のみ自動化 (CI 用。制約は下記)
```

preview 内容:

```
- 対象 root / scope
- 推定ファイル数 / 推定容量
- 大容量ファイル一覧 (上位 N)
- 現在有効な ignore (.kioignore + config)
- 除外候補 (提案。自動除外しない)
- 機微ファイル候補の警告 (secrets Tier A: デフォルト除外済み / Tier B: 要確認。10-operations.md §1.1)
- network transmission policy (どの Adapter がオンライン送信するか)
- 別 .kio と重複する可能性のある容量 (ユーザー配置由来のみ)
- 推定 LLM コスト (markdownize / embedding 別。現行 `tools.toml` の `[pricing]` 単価による桁の目安 — [10-operations.md §1](10-operations.md))
- 現行 budget cap での推定完了時期 (cap 超過が予見される場合は承認前に警告 + 選択肢提示。[10-operations.md §1](10-operations.md))
```

**非対話環境** (`isatty=false` / CI) では、承認済み scope または `--yes` がない限り `kio index` は **exit 2** で失敗する。

**`--yes` の制約**: `--yes` が自動化できるのはローカル取り込みの承認のみである。

```text
1. network opt-in を付与しない。opt-in 未成立の scope では、--yes で index を
   開始しても online_api Adapter への送信 task は発行されず pending のまま残る
   (07-adapter-spec.md §3)。非対話環境で永続 opt-in が必要な場合は、事前に対話環境または
   `kio adapter approve <tool_id> --yes` または対話操作で**paired grant を成立させておく**。
   config の `allow_network = true`、scope 側の行、または device record のいずれか一つだけでは
   送信 gate を満たさない。pending からの自動 publish もない。`--online` は grant を作らず、
   index 前に configured Adapter のいずれかの active grant を要求する (07-adapter-spec.md §3)。
2. secrets の built-in デフォルト除外 (10-operations.md §1.1 Tier A) を解除できない。
3. `--yes` は local ingest 確認だけを省略し、Adapter grant、secret grant、trust state を変更しない。
```

---

# 3. Search

デフォルトは全 indexed scope を対象とし、mode は実効 `[search].default_mode` (既定 auto = hybrid → text fallback — [05-runtime.md §1](05-runtime.md)) に従う。scope の列挙・結果統合・部分失敗・cursor は [05-runtime.md §1.8](05-runtime.md) の multi-scope search 契約に従う。

```bash
kio search "認証仕様"

# scope 制限
kio search "..." --scope .                  # カレントフォルダのみ
kio search "..." --scope . --descendants    # カレントとその配下
kio search "..." --scope ./Research [--descendants]
# path 引数は canonical 化 (絶対化 → lexical 解決 → 末尾 separator 除去 → realpath) して
# registry の root_path と byte 比較する (05-runtime.md §1.8)
kio search "..." --all-scopes

# モード
kio search "..."              # 実効 [search].default_mode (既定 auto = hybrid → text fallback — 05 §1.8)
kio search "..." --mode <auto|text|image|vector|hybrid>   # 当該実行の上書き。config の同名 enum と 1 対 1。
                              #   text   = text only
                              #   image  = 候補を画像に限定 (2026-08-11 追加、05 §1.1)。
                              #            text と対になる軸。ベクトル不可時は error
                              #   vector = vector only。失敗時は error
                              #   hybrid = hybrid 強制。失敗時は fail_behavior 設定に従う
                              # (承認なし (embedding_not_authorized)・--offline は対象外 — 通常は text
                              #  fallback。--online 明示時は vector 経路の precheck 不成立を error にする。
                              #  embedding_in_flight は技術的失敗として対象。
                              #  正本 05-runtime.md §1.1 consent gate)
kio search "..." [--online|--offline]   # HTTP query embedding は current exact paired grant を要する
                                       # --online は cloud / authenticated local HTTPS と vector 経路を要求する。
                                       # 一時承認ではなく、Adapter / grant / index の不足は error。
                                       # --offline は local HTTPS も含む全 HTTP dispatch を禁止する。
                                       # in-process deterministic は禁止対象外 (--online は指定不可)。
                                       # cursor replay は cache vector を再利用するが current gate / cursor 検査は残る。
                                       # 正本 07-adapter-spec.md §3・05-runtime.md §1.1 / §1.5

# time-travel
kio search "..." --at <commit> --scope <path>   # --at は --scope 単一指定を必須とする (独立 DAG の
                                                # multi-scope に単一 commit は適用不能 — 05 §1.6)
kio search "..." --all-history          # 削除済み・移動済み含む全 commit
kio search "..." --include-deleted
kio search "..." --since 7d

# paging / 結果制御
kio search "..." --limit 20 [--offset 20|--cursor <token>]
kio search "..." --json                 # 機械可読
```

レスポンス schema は [05-runtime.md §1.7](05-runtime.md)。`json` モードでは Evidence Pointer フル構造 + `next_cursor` を返す。

`index_status.budget_paused` は task の budget pause と、cost ledger から観測した device / folder 月次 cap 枯渇の OR である。この観測は searched scope ごとに source ledger を reopen せず、1 invocation の command preflight で取得した同じ owned read-only snapshot から device total を1回、folder total を scope ごとに読む。snapshot は既存の入力・mode 判定後、fresh vector / hybrid page 1 の writable ledger open・claim / reservation・query embedding 送信より前に取得する。集計対象の UTC 月も preflight 時点で固定し、取得後の別 writer や同一 invocation の charge はその response の月次 cap 観測へ混ぜない。explicit text、cursor replay、および offline / profile incompatibility / final consent rejection による**pre-attempt** auto→text は source ledger の main / `.write-seq` / SQLite sidecar を作成・変更しない。final consent check を通過した fresh vector / hybrid page 1 は query embedding 記帳の既存例外に従い、claim / reservation / send 後に最終結果が text fallback になった場合も同じである ([05-runtime.md §6](05-runtime.md))。snapshot が unsafe / unstable な場合は `budget_paused=false` に縮退せず、下記の command-level error で stdout を空のまま停止する。

---

# 4. Output Format

すべての CLI は `--json` を持つ。デフォルトは人間向け整形、`--json` で機械可読。

```bash
kio <command> --json
```

人間向け表示は色付き + path 短縮形 (`~/Documents/...`)。`--json` は色なし + 絶対 path + 完全 hash。エラーも `{ "error_code": "...", "message": "...", "context": {...} }` 形式で返る。

---

# 5. Export

過去 commit の raw bytes を別の宛先へ書き出す。`export` は管理中の最新版へ直接書き戻さない。`restore` は export の互換 alias ではなく、次節の journaled managed restore だけを指す:

```bash
kio export <evidence|path|commit> --to <dir>
kio export <commit> --to ~/Recovered/<commit>     # 通常
kio export <pointer> --to ./recovered/ --force    # 既存上書き許可 (確認 prompt)
```

安全要件:

```
- --to <dir> は必須 (canonical 解決先が当該 scope root 配下 (`.kio` 含む) は KIO-E-CONFIG-USAGE-001
  (exit 2) で拒否 — working tree への直接書き戻し禁止の迂回を許さない。canonical 解決は
  05 §1.8 の算出規則と同一 (realpath 含む)。展開前に --to を open した fd の実体 (dev/inode) を
  canonical 解決先と対照し (不一致 = KIO-E-CONFIG-USAGE-001 で mutation 前拒否)、以後の展開は
  同一 fd 配下に限定する — 05 §4)
- 全出力 path の退避 / 隔離の同名残存は --force の有無・宛先の存否に関わらず mutation 前に検査し、
  残存 = 先行未完として拒否 + 回復案内 (正本 05 §3.5)
- 既存ファイル上書きは --force + 確認 prompt
- --force 上書きは旧ファイルを同 directory の退避名 `<basename>.kio-export-bak` へ no-replace で
  保全 (同名残存 = 先行未完として拒否 + 回復案内。退避名は stderr に表示・dev/inode を記録) して
  から publish し、rename 後再検査の purge / erase / journal 終端時のみ原状復帰する (対象 alive の
  無関係変化は publish 維持 — 05 §3.5。成功時に退避を除去)
- publish (--force 含む)・隔離・復帰の rename は全て no-replace。巻き戻しの削除も退避の復帰・
  除去も、path 上の対照ではなく決定的隔離名 `<basename>.kio-export-quarantine` への隔離 rename +
  rename した実体の dev/inode 検証で行う (隔離名は stderr に表示。同名残存 = 先行未完として拒否 +
  回復案内。隔離・退避はユーザー領域 — Kio は自動削除しない)
- 競合処置は段階別 (--force publish 競合 = 退避を復帰 / 隔離実体の不一致 = 元 path へ復帰を試行 /
  退避の不一致・復帰 rename 失敗 = 不触) — いずれも両所在を表示して
  KIO-E-COMMIT-EXPORT-CONFLICT-001 (retryable exit 3、context に conflict_kind・retry_disposition)
  で終端 (05 §3.5)
- 出力名・上書き対象名が `.kio-export-bak` / `.kio-export-quarantine` で終わる場合は展開前に
  明示拒否 (改名復元を案内)
- export は raw object をそのまま展開 (再 Markdownize しない)
- evidence は pointer URI / inline JSON / stdin、path は論理 direct-child 名、commit は HEAD / tag / full commit hash。tag と同名の path は tag を優先する。raw_hash shorthand は export では受理しない
- shallow commit からの export は KIO-E-COMMIT-SHALLOW-001 で拒否
- purged 対象は KIO-E-PURGE-NOT-FOUND-001 / tombstone
```

## Managed restore

```bash
kio restore <source> [--path <file>]... [--delete-missing <file>]...
                       [--expected-head <hash>] [--preview] [--message <text>]
kio repair recover-restore
```

`source` は `HEAD`、full commit hash、または tag である。restore は current HEAD を過去へ移動せず、
current HEAD を唯一の親とする `restored` commit を新規に作る。source はその親の祖先でなければならず、
branch は持たない。`--preview` は plan、working tree、HEAD、source SQLite、ledger、replica を一切変更しない。

`--path` がなければ、source tree のうち現在の management / ignore policy が許可する path を戻す。
source に存在しない current-only file は自動削除せず、削除する場合だけ `--delete-missing <file>` を明示する。
`--path` と `--delete-missing` の path は current policy が拒否していれば失敗する。apply 中にも各 working
tree mutation と HEAD publication の直前に policy を再検証する。restore 後は replica を `rebuilding` にして
検索を fail closed にし、adapter や外部送信を伴う同期再構築を暗黙には開始しない。

中断後に `.kio` の managed-restore journal が残った場合、通常の操作は拒否される。`repair recover-restore`
だけが current HEAD と journal を照合し、完了した publication を仕上げるか、working tree を安全に戻す。これは
network、secret、budget の承認を追加しない。

---

# 6. Delete / Archive / Purge

通常削除 (`rm`) や archive は最新状態から対象を消すだけで、過去履歴は保持する。法務・秘匿・誤取り込みで履歴ごと消す場合のみ `purge` を使う。

```bash
kio purge <path|--raw-hash <h>> --reason <legal|privacy|misingest|copyright|other>
kio purge --raw-hash sha256:abc... --reason misingest --erase-tombstone
```

purge は常に**全履歴**の raw 本文・派生 artifact を対象とする (commit / tree object は書き換えない。[05-runtime.md §3.5](05-runtime.md))。デフォルトでは tombstone を記録し、`--erase-tombstone` は public tombstone を残さない (Evidence Pointer は not_found)。後者の non-public non-content erase receipt は public tombstone にならず re-ingest も阻止しない (pointer 解決内部の not_found 分類等の用途列挙は [08-evidence-pointer-spec.md §4.2](08-evidence-pointer-spec.md))。

- `--reason` は必須引数 (5 値の閉 enum: legal | privacy | misingest | copyright | other — [08-evidence-pointer-spec.md §4.1](08-evidence-pointer-spec.md) の purged_reason と同一)
- 確認 prompt 必須 (`--yes` でスキップ可)
  - (purge の `--yes` は確認プロンプトのスキップのみで、§2 の初回スキャン承認の `--yes` とは
    独立。network opt-in を付与する効果はどちらにもない)
- 結果 commit は `commit_type=purged`
- 詳細は [05-runtime.md §3](05-runtime.md)

## 6.1 Retention GC: plan, crash-safe shallow sweep, and bounded hook (Phase 4 milestones 1–3)

この段階で公開する GC 操作は、retention による shallow 候補を読み取り専用で計算する形式と、明示確認付きの tree-only shallow sweep である。

```bash
kio gc --dry-run
kio gc --dry-run --json
kio gc [--yes]
kio gc --yes --json
```

- `--dry-run` は read-only preview を明示する。省略時の `kio gc` は確認付き shallow sweep である。
- planner は `[gc.auto_retention]` / `[gc.derived_retention]` を読み、現在時刻・全 ref tip・全 commit object・既存 shallow receipt を同一の bounded read-only plan へ束縛する。計画後に同じ truth 一式を独立した bounded pass で再検証し、一致しなければ成功を返さない。`HEAD` / tag の tip は常に候補外。
- `auto` は UTC の半開区間で `all -> hourly -> daily -> weekly` と減衰させ、bucket 内では fractional seconds を含む `created_at` が最新の commit (同一 `created_at` なら commit hash の byte 順で先のもの) を保持する。hour は UTC hour、day は UTC civil day、week は月曜開始、`keep_weekly_months` の 1 month は retention 計算上 30 days とする。未来時刻は安全側で保持する。
- `repaired` は 単一 HEAD の到達履歴で最新 `keep_repaired` 件を保持する。HEAD から到達しない `repaired` は `repaired_outside_head` として保持する。旧キー `keep_repaired_per_branch` は schema error として拒否する。`manual` / `purged` は常に候補外。
- 全 ref root から到達しない commit は `unreachable_commit` として候補外にする。未到達 object を retention 候補へ読み替えない。
- candidate は commit 単位で列挙するが、対象 object kind は `tree` だけである。同じ tree hash を候補外の非 shallow commit が 1 件でも参照する場合、その tree を共有する候補はすべて保護する。`estimated_bytes` は候補となる一意 tree の物理 byte 数で、commit / raw / chunk / toollock / manifest はこの plan に含めない。
- `.kio/gc/shallowed/<commit64>` は厳格に検証し、receipt 済み commit を再候補化しない。receipt がない commit の tree 欠落、malformed receipt、探索上限超過、symlink / reparse point / 非 regular file / unsafe hardlink、走査中の identity 変更は候補 0 件へ縮退せず structured error とする。上限超過は `KIO-E-GC-PLAN-LIMIT-001` (exit 4)。
- 成功結果は `status=\"dry_run\"`、適用時刻と policy、candidate commit/tree 数、一意 tree の推定 byte 数、commit-hash byte 順の候補、理由別の除外数、走査 limit/stat を返す。同じ store・policy・注入時刻に対する JSON / human output は決定的である。
- `--dry-run` は完全に read-only である。一方 `kio gc` は fresh plan 全体（policy、sorted candidates、exclusions、plan/truth digestを含む）を対話時にそのまま表示して `y` / `yes` の確認を要求する。active marker の resume では要約でなく完全なfrozen marker/stateを表示する。非 TTY または `--json` では候補数が0でも `--yes` が必須である。`--dry-run --yes` は usage error。外部 JSON や過去の preview は mutation authority にならない。
- 確認後は `.kio/.lock` の下で capability-relative に再bind/replanし、candidate集合・policy・plan/truth digest が preview と一致したときだけ実行する。不一致は `KIO-E-GC-PLAN-CHANGED-001` (retryable exit 3) で、marker / receipt / tree を変更しない。
- 実行は `.kio/gc/in_progress` を atomic publish + fsync してから `prepared → receipting → sweeping → finalizing` を進める。marker はcandidate/tree各100,000件、canonical body 8 MiB、推定対象4 GiBを上限とし、publish前に超過を拒否する。receipt は `.kio/gc/shallowed/<commit64>` に canonical JSON+LF で create-new/fsyncし、全 shared-tree receipt が耐久化されるまで tree を一つも削除しない。commit/raw/chunk/manifest/toollock/index/chunks ledger は削除対象外である。
- tree leafはretained descriptorからnofollow・single-link・hash/schema/identityを再検証し、no-replaceでCAS fanout外の`.kio/gc/internal/trees/`へ隔離する。Unixでは隔離 leafを同じbound directoryでunlinkし、link count 0 を確認してから同じretained file handleをtruncate+fsyncする。Windowsでは競合write/delete openを許さないexact HANDLEでno-replace隔離・dispositionを行い、同一identityとlink count 0を確認して同じHANDLEをtruncate/flushする。canonical tree leafと隔離 leafは消失する。ambient pathname unlinkやempty fanout掃除は行わない。`.kio/gc/internal/`はoperation-reserved namespaceであり、検出可能な差替えはfail-closedにする。POSIXにidentity条件付きunlinkが存在しないため、検証直後のreserved nameへの直接第三者書込みだけは[05-runtime.md §2.5](05-runtime.md)・同§3.5と同じ保護契約外の残余窓であり、public CAS path・scope/fanout・receipt/marker public name・hardlinkの保護を緩めない。
- marker がある間、`kio gc --dry-run` は recovery pending を read-only で報告する。`kio gc --yes` は凍結 marker を validator で再検証して再開する。receipt または tree deletion 後に truth が矛盾すれば fail-closed し、marker は残す。最初の physical tree deletion 前と**finalizing の各実行・再開時**に index generation を descriptor-bound に回転する（sqlite がない scope は `index_absent` として記録する）。回転は公開DBのin-place更新ではなく、`.kio/gc/internal/index/`のprivate copyを更新・file/directory fsyncし、source file stateとsource/target/private-directory identityをmarkerへ耐久化してexchange直前に再照合してから、公開`index/sqlite.db`を切り替える。macOS / Linuxはatomic exchangeと両directoryのfsync、Windowsは独立した`gc/internal/index-exchange/` ownerの5状態journal付きno-replace moveを使う（[05-runtime.md §2.5](05-runtime.md)）。Windowsではactual SQLite main-file HANDLEもpager access前に照合し、matching journalが説明する公開index不在をCREATEで補わない。pre-sweep private copyではgeneration更新と同一SQLite transactionにstrict singleton attestation（sweep ID、role、plan digest、source/target generation）を記録し、treeごとに公開DBのgeneration/identityとattestationを再検証したprocess-local permitだけをcore除去APIへ渡す。完了 rotation の耐久化後だけ marker を削除する。descriptor-bound SQLite rotationを安全に実装できないplatformでは、marker/receiptのpublishより前にsweepをfail-closedする。Windows実装はnative acceptance完了の主張ではなく、3 OS GitHub Actionsと最終security reviewをrelease gateとして残す。process interruption回復とpower-loss耐久性は別であり、file flushからUnix相当のdirectory fsyncを推論しない。
- Windowsのpending marker exchangeは`gc/internal/marker-exchange/`でindex交換と独立に管理する。未公開intent残骸やpublic `gc/in_progress`不在も検出し、`kio gc --dry-run`はread-onlyで`status="recovery_pending", reason="marker_exchange"`を返す。この状態のmutationには対話時も`--yes`が必須であり、省略時はusage error / exit 2。`kio gc --yes`は専用writer lock下でordinary marker読取前に回復する（明示automatic modeの同mode回復入口も既存規約に従う）。通常read/write/searchは`KIO-E-STORE-CORRUPT-001` / exit 4でfail-closedする。marker schema/phase遷移と現在のpolicy/receipt/truthを検証し、旧sourceのexact-handle退役、zero links・truncate/flush、owner HANDLE close後のprepared名不在までintentを保持する。source退役済みならactual targetと現在authorityを検証してintent cleanupだけを行い、旧sourceを推測・再作成しない。
- 交換pendingではないcanonical active marker は通常 writer を retryable に拒否し、search は新規 cursor を発行しない。ページ 1 は結果を返せても `next_cursor=null` と recovery-pending 注記を含める。明示 `after_index` の index/manual snapshot入口と、`on_idle` の `snapshot auto` 入口だけは、通常writer lockより前に同modeのbounded recoveryを行う。
- milestone 3 は `[gc] mode="after_index"` を**明示したscopeだけ**で、成功かつnon-partialな `kio index` / manual `kio snapshot create` のdurable publication後に同じexecutorを呼ぶ。既存writer lockは先に解放し、GCは専用bound lockの下でfresh replan/revalidationする。preview、revoke、usage error、失敗・partial indexからは発火しない。`manual_only`は現行defaultのままであり自動mutationを行わない。milestone 5の`on_idle`はOS scheduler起動の`kio snapshot auto`だけで発火し、after_indexとはcofireしない。
- automatic authority はwriter開始前のcanonicalな`[gc]` subtree digestとretained scope / `.kio` identityへ固定し、publication後およびGC lock下のlocked re-plan前後で一致を要求する。mode/runtime/retentionまたはscope bindingが途中で変わればGC mutationを開始せず `KIO-E-GC-CONFIG-CHANGED-001` / exit 3 とする。既にdurableなpublicationは`publication_status="completed"`のままであり、index自身が更新する非GCのadapter/network設定はこのdigestの対象外である。
- `max_runtime_seconds` はmonotonic soft deadlineである。安全なdurable checkpointで `status="deferred"`、`reason="max_runtime_seconds"`、`recovery_pending=true` を返しmarkerを残す。次回のautomatic writer入口は通常lockより前にresumeし、未完ならindex/snapshotを開始しない。shared treeは全candidate receiptが耐久化するまでtree phaseへ移らないためbatch境界でsharing closureを分割しない。
- automatic resultはindex/snapshot payloadの`gc` objectに載せる。post-publication timeout/errorは`publication_status="completed"`を保持し、timeoutは`KIO-E-GC-RUNTIME-LIMIT-001` / exit 3、permanent integrity failureはexit 4、それ以外のpost-publication failureはpartial exit 3とする。pre-publication recovery timeoutは`publication_status="not_started"` / exit 3である。human outputにも`gc: <status> (<reason>)`を追記する。
- internal child scope は保持済み Repository を使う共通 app 層の呼出しで、その scope へ1回だけ hook を適用する。保持済み child capability と再確認した identity が一致しない場合は fail-closed する。親 scope の hook が child へ代理適用されることはなく、child の GC 結果は親の `child_scopes[].gc` へ保持する。

scheduled snapshotはPhase 4 milestone 4、Rust-only `on_idle` はmilestone 5で公開済みである ([05-runtime.md §2.2-§2.6](05-runtime.md))。

## 6.2 Unreachable-object read-only inventory (Phase 4 milestone 8)

canonical CLI は次の一形態だけである。

```text
kio gc --dry-run --prune-unreachable [--json]
```

`--dry-run` は必須であり、`kio gc --prune-unreachable` は usage error / exit 2 とする。
`--yes` とは同時指定できない。alias、短縮形、別 schema、bare mutating prune は存在しない。
この操作は retention planner / sweep executor、`after_index`、`on_idle`、receipt、marker、resume の
どれにも接続せず、scope の byte を一切変更しない。report は診断資料に限り、現在または将来の
mutation authority にはならない。

走査全体で実writerと同じgateのnonblocking shared lockを保持する。macOS / Linuxはdirectory
`flock`、Windowsはstorage format `2.0.0`のimmutableな空file `.kio/.store-gate`を
`GENERIC_READ`で開くshared `LockFileEx`を用いる。writerは同じgateのexclusive lockを
`.lock`作成・stale回収前からowner closeまで保持する。inventoryはgateを作成・修復・変更しない。
2.0.0のgate欠落・unsafeはfail-closedし、旧1.0.0はgate検査前に
`KIO-E-STORE-VERSION-001` / exit 8で拒否する。非対応platformは無変更でfail-closedする。
Windows実装についてもnative Windowsを含む3 OS Actions acceptanceは未完了のrelease gateである。

truth は SQLite cache ではなく、retained descriptor から nofollow で読んだ refs、commit/tree CAS、
manifest と normalized-unit pin、embedding target、immutable tool-lock、shallow receipt、purge lifecycle、
GC/writer barrier から導出する。全 object は `(kind, hash)` の辞書順で一度だけ現れ、各行は次の形である。

```json
{
  "kind": "manifest",
  "hash": "sha256:<64 lowercase hex>",
  "physical_bytes": 123,
  "classification": "candidate",
  "reason": "zero_tree_references"
}
```

top-level schema は `schema_version=1`、`operation="unreachable_object_inventory"`、
`status="dry_run"`、`read_only=true`、`diagnostic_only=true`、`mutation_authority=false`、
`objects[]`、分類別 count/physical bytes を持つ `summary`、検証済み `shallow_boundaries[]`、
実際に適用した `limits`、独立2 pass の `stats` だけを持つ。ambient absolute path、秘密、時刻、
削除指示は出力しない。`--json` はこの schema を compact JSON で返し、指定しない場合も同じ
全 report を terminal-safe な pretty JSON で表示する。同一安定scopeへの出力は決定的である。

分類の current contract は [05-runtime.md §2.7](05-runtime.md) と
[03-data-model.md §8.3](03-data-model.md) を正本とする。候補は参照ゼロを正本graphから証明した
manifest、normalized unit、embedding、未公開tool-lockだけである。commit/tree/raw/chunk、
live参照、prepared/image、および証明不能なobjectは候補にしない。validなshallow receiptが
一つでも存在するとき、欠落treeが隠すsemantic closureを復元できないため、その不確実性に
属するorphan manifest / normalized unitは `inventory_only / shallow_history_unavailable` とする。

走査は object数、physical/verified bytes、manifest unit数、ref/receipt数、履歴step、directory entry、
name bytes、depth を上限化する。retained descriptor・capability-relative・nofollow、regular file・
link count・identity・size・hash検証を行い、共有writer barrierの下で独立した全走査を2回実施する。
mutation-free shared descriptor writer barrierを実装済みのplatformはmacOS / Linuxであり、その他では
scopeを変更せずfail-closedする。
active purge / GC recovery / writer、malformed・欠落・hash mismatch、symlink/reparse、unsafe hardlink、
inode/directory交換、上限超過、pass間差分は候補0へ縮退せず既存のcause-specific structured errorで
fail-closedする。failure時はstdoutを空のままにする。

---

# 7. Exit Code (横断規約)

```
0   成功 / 全 up_to_date
1   汎用 failure (詳細不明)
2   invalid usage / config 不正 / schema validation 失敗
3   retryable な失敗が残っている (部分成功を含む。lock 取得失敗のような全体 retryable もここ)
4   permanent な失敗のみが残っている (全失敗 permanent、および settled partial
    (部分成功 + 残り全 permanent — 04-pipeline.md §5.2) を含む — 再試行で進展しない)
5   auth_error (user action 必要)
6   budget_exceeded により paused
7   user 中断 (SIGINT/SIGTERM)
8   incompatible profile / format version
9   confirm 拒否 (purge 等の確認プロンプトで no)
```

device-global の `repair all` / `repair replica` は scope ごとに結果を集約する。全 scope 成功は 0、
一部成功・一部失敗は `KIO-E-REPAIR-PARTIAL-001` / exit 3 とし、成功結果と `failed_scopes` を同じ
JSON に返す。全 scope 失敗で code / exit が同一ならその scope error を昇格し、混在時は
retryable な失敗を 1 件でも含めば `KIO-E-REPAIR-ALL-FAILED-001` / exit 3、すべて permanent なら
同 code / exit 4 とする。live clone 重複は `KIO-E-REGISTRY-DUP-001`、on-disk identity / path と
一致しない stale 行は `KIO-E-REGISTRY-STALE-001`、active purge journal は
`KIO-E-PURGE-JOURNAL-ACTIVE-001` として当該 scope を fail-closed にする。ただし format-version
不一致は集約対象ではない。全 selected scope を device replica reset / scope repair より先に検証し、
1 件でも不一致なら `KIO-E-STORE-VERSION-001` / exit 8 で command 全体を無変更のまま停止する。

**Evidence Pointer 系コマンドへの割当** ([08-evidence-pointer-spec.md §4.3](08-evidence-pointer-spec.md)):

```text
kio evidence verify            検査完了で 0 (結果は status フィールド)。parse 失敗は 2
kio evidence verify --strict   全 alive なら 0。tombstoned / not_found が 1 件でもあれば 4。
                               scope_unreachable のみの失敗は 3 (retryable — 08 §4.3)。
                               unverifiable は reason で分岐 (08 §4.3): manifest_missing を
                               1 件でも含めば 4 (恒久 — 再試行で進展しない)、commit_shallow のみなら 3
                               (unshallow で解消し得る)。registry_duplicate も 3
KIO-E-STORE-CONSTRAINT-001     記帳 CHECK 到達 = 実装エラー (04 §5.8)。permanent・非再試行で
                               command を即時中止・exit 4
sqlite.db 不在・利用不能       全経路 (verify / open / view / export / search) で status に混ぜず
                               command-level の retryable error KIO-E-INDEX-REBUILDING-001・exit 3
                               (再構築中に既存 sqlite.db が読めても通常応答へ戻らず fail-closed — 05 §6。ただし
                               **HEAD ref 不在の scope は、HEAD 依存経路 (現在状態の search・
                               sha256: 短縮形 — 08 §2.3 規則 4) に限り、sqlite.db が読めても
                               REBUILDING 扱い** (05 §1.6 — 未公開行を検索に見せない)。**明示の
                               commit / Evidence Pointer 指定による verify / open / view / export・
                               単一 scope の search `--at <commit>`
                               は HEAD 非依存に解決して通常応答する** (08 §3.1 の解決手順・08 §6 の
                               不変性保証)。error は不在・利用不能・HEAD 不在 (HEAD 依存経路のみ)
                               の場合のみ。verify は検査未完了のため --strict なしでも
                               0 を返さない。multi-scope search は当該 scope を excluded_scopes として
                               継続し、全 scope 該当なら exit 3 — SCOPE-ALL-FAILED (3/4) より優先。
                               全 scope の除外理由が同一 code なら当該 code の単独時 exit へ昇格
                               (一般規則 — REBUILDING→3・INCOMPAT→8・
                               journal (KIO-E-PURGE-JOURNAL-ACTIVE-001)→3・
                               DUP→3 (dedupe 後に回復可能 — 08 §4.3 registry_duplicate と同一分類)。
                               05 §1.8 / 10 §11.5)。混在は SCOPE-ALL-FAILED — retryable 理由を
                               含めば exit 3・全て permanent なら exit 4 (05 §1.8)。
                               優先順位は journal → DUP → REBUILDING (10 §3)。format-version
                               不一致は scope 除外より前に command 全体を停止する (10 §11.5)。05 §2.6・08 §3.1)
kio evidence verify --batch <pointers.jsonl>   一括 verify。入力の構造・filesystem・integrity error は
                               command-level であり partial result を publish しない（08 §4.3）。
                               malformed/UTF-8/blank は KIO-E-EVIDENCE-BATCH-INPUT-001 (2)、
                               file/line/record/scope 上限は KIO-E-EVIDENCE-BATCH-LIMIT-001 (2)、
                               aggregate 認証済み CAS bytes 上限は
                               KIO-E-STORE-VERIFIED-BYTES-LIMIT-001 (4)、検査中の authority /
                               registry/index generation drift は
                               KIO-E-EVIDENCE-BATCH-CHANGED-001 (3)。
                               --strict 時は permanent (tombstoned / not_found / manifest_missing) を
                               1件でも含めば 4、retryable (scope_unreachable / registry_duplicate /
                               commit_shallow) を含むが permanent がなければ 3、全 alive なら 0。
                               --strict なしは単発の semantics を保ち、検査完了時は 0（ただし
                               registry_duplicate は 3）。
kio open / view / export      dead pointer (tombstoned / not_found) は 4。scope_unreachable は 3 (retryable — 08 §4.3)
kio evidence retarget          canonical heading_path exact match が 0 件なら KIO-E-EVIDENCE-RETARGET-NOT-FOUND-001 / 4、複数なら KIO-E-EVIDENCE-RETARGET-AMBIG-001 / 4。invalid pointer / --at は 2、shallow/drift は既存 retryable 分類、CAS 矛盾は 4。
```

Evidence の registry snapshot は command-level preflight である。source registry の symlink /
hardlink / non-regular leaf、pre/open/post の identity 不一致、または source 再観測が一致した
owned copy の SQLite integrity failure は `KIO-E-REGISTRY-SNAPSHOT-UNSAFE-001` / exit 4 とする。
source leaf の presence/hash drift、busy、bounded retry 内に安定しない snapshot、temp I/O、または
copy/open/query 中に integrity を確定できない failure は `KIO-E-REGISTRY-SNAPSHOT-001` / exit 3 とし、
いずれも stdout を空にして single verify / batch verify / retarget へ同じまま伝播する。これらは
scope status や `batch_changed` に変換しない。registry main / `-wal` / `-shm` の **全 3 leaf が absent**
の場合だけは cache miss であり、既存の validated hint / CWD 候補規則を継続できる。main absent で
sidecar が存在する形は unsafe integrity である。

Search の cost-ledger snapshot も command-level preflight である。source ledger のmain / `-wal` /
`-shm` の全3 leaf absent の場合だけは no-create / zero-total として cap 判定を継続する。
source symlink / hardlink / non-regular / oversize leaf、pre/open/post identity 不一致、main absent +
sidecar present、または source 再観測が一致した owned copy の SQLite integrity failure は
`KIO-E-LEDGER-SNAPSHOT-UNSAFE-001` / exit 4 とする。source leaf の presence/hash drift、busy、
bounded retry exhaustion、temp I/O、または copy/open/query 中に integrity を確定できない
failure は `KIO-E-LEDGER-SNAPSHOT-001` / exit 3 とする。どちらも stdout を空にし、
scope fallback / excluded scope / `budget_paused=false` / 他 domain の error code へ変換しない。

スクリプト連携 (`kio index && kio search`) はこれらを参照する。コマンド固有の補足は各 sub-command の docstring で明記。

---

# 8. Error Code Namespace

すべてのエラーは `KIO-E-<DOMAIN>-<SUBDOMAIN>-<NNN>` 形式の `error_code` を持つ。`error_kind` などのフリーテキストはユーザー向け表示専用。機械判定は `error_code` (明示例外 = manifest `units[]` / Adapter 出力 `failed_units` の `error_kind` — [04-pipeline.md §5.3](04-pipeline.md) の閉 enum であり unit 単位の retry 可否判定に使う、[10-operations.md §11.1](10-operations.md))。**成功応答 (exit 0) に載る `error_code` は縮退原因の分類であり、失敗判定には使わない** — 失敗判定は exit code (非 0) が正 (例 = text fallback の [05-runtime.md §1.7](05-runtime.md) 応答契約)。

DOMAIN 一覧の正本は [10-operations.md §11.1](10-operations.md)。本節は同一リストの転記であり、差分が生じた場合は 10 側を正とする。

```
DOMAIN:
  BATCH    バッチ処理 (markdownize / embedding / etc.)
  INDEX    インデックス更新
  REPAIR   device / scope 修復の集約
  SEARCH   検索 (FTS / vector / hybrid)
  COMMIT   commit / snapshot / export
  GC       garbage collection
  PURGE    purge 操作
  EVIDENCE Evidence Pointer 解決 / verify
  REGISTRY scope registry (live clone 重複・退役)
  ADAPTER  Adapter ロード・実行
  EMBED    embedding profile / modality 検証
  CONFIG   config / schema / 設定
  STORE    object store / fs IO
  AUTH     認証・認可
```

GC planner 固有の `KIO-E-GC-PLAN-LIMIT-001` は commit / tree entry / verified byte / ref / receipt / directory entry / path depth / graph traversal cap 超過を表し、exit 4 とする (§6.1)。`KIO-E-GC-RUNTIME-LIMIT-001` はbounded automatic sliceが安全なcheckpointで期限へ達したことを表すretryable exit 3であり、corruptionではない。`KIO-E-GC-CONFIG-CHANGED-001` はautomatic writer開始時に固定した`[gc]` authorityがpublication-to-GC handoff中に変化したため、publicationを保持したままGCを開始しなかったことを表すretryable exit 3である。

`KIO-E-EMBED-PLAN-LIMIT-001` は保持済み履歴からの埋め込み候補収集が、100,000 件または
128 MiB の文字列 payload 上限を超過したことを表す（exit 4）。context の `dimension` は
`candidate_pairs` / `payload_bytes`、`count` は超過予定値（算術 overflow 時は `"overflow"`）、
`limit` はその上限である。部分的な候補集合は返さない。計上範囲は [04-pipeline.md §4.3](04-pipeline.md) を参照。

`KIO-E-EMBED-BATCH-CONTRACT-HOLD-001` は同じ scope / embedding adapter に未解除の
Batch 応答契約違反があるため、新規 Batch の予約・送信を停止したことを表す（exit 4）。
context は `scope_id`、`adapter_kind` と、各 `blocked_attempts` の `input_hash`、
`tool_profile_hash`、`submission_seq`、完全な selector を含む。原因確認後、対象ごとに
`kio batch retry --reset-violations <selector> --yes` を実行して解除する。
既に送信した job の照会・精算、別途承認された sync レーンはこの停止の対象外とする。
停止判定の正本は [07-adapter-spec.md §5.3](07-adapter-spec.md)。

Evidence 解決の現行 code は、purged raw の tombstone 応答を
`KIO-E-PURGE-TOMBSTONED-001` (exit 4)、raw/chunk の両方に一致する短縮 hash を
`KIO-E-EVIDENCE-SCOPE-AMBIGUOUS-001` (exit 2)、同一 profile の chunk 未実体化を
`KIO-E-EVIDENCE-RETARGET-REQUIRED-001` (exit 8) とする。live scope 重複は別の
`KIO-E-REGISTRY-DUP-001` (exit 4) である。

例: `KIO-E-BATCH-NET-001`, `KIO-E-SEARCH-VEC-INCOMPAT-001`, `KIO-E-COMMIT-SHALLOW-001`, `KIO-E-COMMIT-HISTORY-LIMIT-001` (bounded history walk の aggregate cap 超過、単独操作 exit 4 / multi-scope は既存 partial 規則、[05-runtime.md §1.6](05-runtime.md)), `KIO-E-PURGE-NOT-FOUND-001`, `KIO-E-PURGE-JOURNAL-ACTIVE-001` (未完了 purge journal / epoch 不変違反 — **読み取り系** preflight の拒否 (書き込み系は journal 回復を再開 — [05-runtime.md §3.5](05-runtime.md)、直列化は `.kio/.lock` が担う)。**export の rename 後再検査による publish 後巻き戻し終端にも用いる** (05 §3.5)、retryable exit 3), `KIO-E-COMMIT-EXPORT-CONFLICT-001` (export の publish / 巻き戻しの no-replace 競合・dev/inode 不一致・退避 / 隔離の同名残存 — context に閉 enum `conflict_kind`・`retry_disposition` (transient / manual_action) と両者の所在を含む、retryable exit 3、[05-runtime.md §3.5](05-runtime.md)), `KIO-E-ADAPTER-APPROVAL-CONFLICT-001` (承認の serialized compare/write 不一致 — 並行 revoke による pending 除去・fresh な明示承認が必要、exit 5、[07-adapter-spec.md §3](07-adapter-spec.md)), `KIO-E-ADAPTER-SPECVER-001` (Adapter spec_version 不一致 — invalid_input / 非再試行、[07-adapter-spec.md §8.1](07-adapter-spec.md)), `KIO-E-STORE-PATH-001` (パス区切りを含む path の schema violation、[03-data-model.md §3](03-data-model.md)), `KIO-E-SEARCH-SCOPE-ALL-FAILED-001` (multi-scope search の全 scope 失敗、[05-runtime.md §1.8](05-runtime.md)), `KIO-E-SEARCH-CURSOR-001` (別クエリ・別条件の cursor 誤用、[05-runtime.md §1.5](05-runtime.md)), `KIO-E-INDEX-REBUILDING-001` (index 再構築中、[05-runtime.md §6](05-runtime.md)), `KIO-E-EVIDENCE-SCOPE-UNREACHABLE-001` (pointer の scope が scope_path・registry のどちらでも解決不能、[08-evidence-pointer-spec.md §3.2](08-evidence-pointer-spec.md))、`KIO-E-REGISTRY-DUP-001` (同一 scope_id の複数 live clone — 検索 skip・解決 error、[10-operations.md §3](10-operations.md))、`KIO-E-STORE-CORRUPT-001` (CAS object の content hash 不一致・欠落、`kio repair verify-objects`、[10-operations.md §7.5](10-operations.md))、`KIO-E-STORE-LOCKED-001` (`.kio/.lock` 取得失敗 — 待機せず即失敗、exit 3、[05-runtime.md §6](05-runtime.md))、`KIO-E-STORE-DUP-001` (単一 tree 内の重複 `path`、[03-data-model.md §8.1](03-data-model.md)。`/` 入り path の `KIO-E-STORE-PATH-001` とは区別する)、`KIO-E-CONFIG-USAGE-001` (invalid usage / 不正オペランド — 例: `init` path 不存在、`.kio` scope 外での実行、不正 hash 引数。schema violation の `KIO-E-CONFIG-SCHEMA-001` とは区別。exit 2)、`KIO-E-EMBED-MODALITY-001` (`modality != "multimodal"` の embedding profile の採用拒否 — tool-lock materialize / adapter 登録時に検証、[03-data-model.md §7](03-data-model.md)。exit 2)、`KIO-E-SEARCH-VEC-UNAUTHORIZED-001` (query embedding の embedding 承認なし — 通常の auto/`--mode hybrid` は text fallback、`--mode vector` / `--online` 明示時は error、[05-runtime.md §1.1](05-runtime.md))、`KIO-E-STORE-VERSION-001` (`KIO_FORMAT_VERSION` と完全一致しない `kio_format_version` — 欠落・非 string・非 parseable・older/newer/unknown を current schema 検証前に reader / search / repair / historical を含む全 command で即時拒否し、multi-scope search も command 全体を停止、store bytes 不変。正本 [10-operations.md §11.5](10-operations.md)。exit 8)、`KIO-E-PURGE-REPLICA-001` (purge 後の device replica 再射影に失敗 — 本文が cache root に読める状態で成功と報告しないための fail-closed 終端。exit 1、[05-runtime.md §3.5](05-runtime.md))、`KIO-E-CONFIG-OFFLINE-URL-001` (`execution_mode = "offline_api"` の Adapter の `url` が loopback リテラル以外 — tool-lock materialize / adapter 登録時に検証、[07-adapter-spec.md §3](07-adapter-spec.md)。exit 2)。

device-global repair の scope 集約 code は `KIO-E-REPAIR-PARTIAL-001` と
`KIO-E-REPAIR-ALL-FAILED-001` とする (§7)。
registry 行が on-disk の scope identity / path と一致しない場合は
`KIO-E-REGISTRY-STALE-001` とする。

新規 code 追加は本書および各 spec の更新を伴う (破壊的変更扱い)。

---

# 9. JSON output と Adapter 境界

外部利用者が現在利用できる構造化出力は CLI の `--json` だけである。Adapter 境界は内部の task / artifact descriptor 契約であり、[07-adapter-spec.md](07-adapter-spec.md) を正本とする。

```
Adapter 境界で記録するもの:
  - 入力 object hash を明示
  - 処理対象 scope を明示
  - execution_mode (online_api | offline_api | deterministic_library) を明示
  - ネットワーク送信の許可状態を明示
  - 出力 artifact hash を記録
  - tool_profile_hash / agent_profile_hash を記録
  - 検索時は searched_scopes / excluded_scopes / fallback_reason を返す
    (fallback_reason は自由語彙 — 閉 enum にしない。機械判定は error_code 側が正であり、
     Agent は未知の fallback_reason 値を無視してよい)
```

URL、認証情報、コマンドパス、ライブラリ選択などの実行設定は **device-local config** に置き、`.kio/` には保存しない。

```
Kio core
  → task descriptor
  → device-local Adapter
  → online API / offline API / deterministic library
  → artifact descriptor
  → Kio core
```

Adapter 種別と契約は [07-adapter-spec.md](07-adapter-spec.md)。

---

# 10. Settings / Schema

すべての設定ファイルは JSON Schema (TOML は JSON 等価表現) で validate。CLI 起動時に schema-driven validation を行う:

```
~/.config/kio/tools.toml          tools.schema.json
~/.config/kio/config.toml         user-config.schema.json
.kio/config.toml                  folder-config.schema.json
.kio/scope.json                   scope.schema.json
.kio/tool-lock.json               tool-lock.schema.json
.kio/manifest.json                manifest.schema.json
```

`tools.schema.json` の認証情報フィールド (`auth`) の形式は [07-adapter-spec.md §1](07-adapter-spec.md) に従う (`env:` / `plain:` prefix)。同じく `url` フィールドの受理条件は [07-adapter-spec.md §3](07-adapter-spec.md) に従う (`execution_mode = "offline_api"` では loopback リテラルのみ — 違反は `KIO-E-CONFIG-OFFLINE-URL-001`)。

validation 失敗は **exit 2** + `KIO-E-CONFIG-SCHEMA-001`。現在の schema は厳格に検証し、legacy reader や migration branch は置かない。

---

# 11. 時刻 / TZ

すべての永続データ (commit timestamps / normalization_runs / access_events / snapshot lineage) は **UTC ISO8601 拡張形式 + suffix `Z`** に固定 (例外 = cost-ledger.sqlite の内部時刻列は UTC epoch ミリ秒 INTEGER — 正本 [10-operations.md §11.4](10-operations.md)):

```
正:   2026-04-25T12:00:00Z
正:   2026-04-25T12:00:00.123456Z
誤:   2026-04-25T12:00:00         (TZ 欠落)
誤:   2026-04-25T12:00:00+09:00   (local 表記)
```

ユーザー向け UI 表示時のみ local TZ に変換する。Lamport/HLC は v0 で採用しない。

---

# 12. Observability

`logs/access.jsonl` 以外に、以下の構造化ログを `~/.local/share/kio/logs/` に出力
(scope-local の `.kio/logs/access.jsonl` 自体も日次 rotation + 保持 config の対象 —
[10-operations.md §11.6](10-operations.md)):

```
events.jsonl       重要イベント (commit, gc, purge, schema migration)
metrics.jsonl      数値メトリクス (デフォルト 1h 間隔)
errors.jsonl       error_code 付きの全エラー
```

各行 JSON 必須フィールド: `ts, level, code, component, message, context`。日次ローテーション、保持 30 日 (config 上書き可 — 正規 key = `[observability] retention_days`、10-operations.md §11.3)。

`redact_logs` のデフォルトは true (ログ全域。正本は [10-operations.md §11.6](10-operations.md))。true 時は `context` の `query`, `path`, `prompt` 等の機微フィールドをマスク。

---
