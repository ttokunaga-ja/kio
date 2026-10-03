# GPU受入試験のActions専用接続経路

2026-09-08作成。以下の提案と事前調査は各記録日の状態を示す。
2026-10-03には所有者がTailscale接続ルールの縮小、CI専用OIDC認証、main専用の
`v1-local-acceptance` Environmentを承認し、この3件を適用して再読確認した。
Environmentには専用SSH秘密鍵1件と接続・認証情報5件を登録した。
2026-10-04に詳細実行計画が承認され、Windows/WSLの専用アカウント・鍵登録・
dispatcher配置・SSH設定と条件付き復旧の本適用も承認範囲に入った。まだ未適用であり、
最終候補に束縛したbundleの更新・検証とローカル回帰の後に進める。既存Mac/admin接続は維持されている。
最新の状態と検証範囲は[実行記録](v1-closeout-execution-2026-10-03.md)を参照する。
private CI routeとauthenticated local acceptanceの完了は主張しない。

## 2026-09-26までの事前確認状態

Windows の live read では OpenSSH は port 22、`AllowUsers rm2c`、global
`AuthorizedKeysFile __PROGRAMDATA__/ssh/administrators_authorized_keys` である。Mac の
Tailscale SSH 用の Mac 限定 rule は有効で、generic OpenSSH rule は disabled である。
ただし 2026-09-26 の再確認では、現在有効な Private profile に対して
Tailscale の IPv4/IPv6 address 宛ての Any protocol/port/source を許可する
別の `Tailscale-In` rule も有効だった。Mac 限定 rule だけでは実効アクセス範囲を
限定できない。到達可能性の判定には実際の tailnet ACL/grants も必要である。
`kio-ci` は存在しない。この状態は CI route の構成済みを意味しないため、そのまま保存する。

## 提案する限定経路

GitHub repository は `ttokunaga-ja/kio`、Environment は `v1-local-acceptance` に固定し、main の exact 40-character candidate SHA だけを受け入れる。GitHub-hosted Linux / macOS / Windows の job は OIDC で一時的な `tag:kio-ci` Tailscale node となる。tailnet policy で許可する到達先は lab Windows の TCP 22 だけである。tailnet の live policy は下記のとおり read 済みであり、wildcard を置換する具体案と既存 firewall への影響を所有者が承認してから変更する。

Windows には新規の非管理者 local user `kio-ci` を作る。`sshd_config` の per-user `Match` で同 user だけを `.ssh/authorized_keys` に切り替え、global の administrator key file と既存 `rm2c` 経路へ影響させない。Match の必要な制約は次である。

- `PermitOpen 127.0.0.1:2222`、local forwarding のみ。
- `MaxSessions 0` で shell・PTY・subsystem の session を拒否し、`AllowAgentForwarding no` を適用する。Windows で未対応の設定項目を拒否の根拠にしない。
- Administrators group、Docker group、Docker daemon 操作権限を付与しない。

Windows の `kio-ci` key は WSL の既存 `kio-test` へ接続するためだけに使う。WSL 側にも同じ新規 CI 専用鍵の public key を `authorized_keys` の forced-command entry として登録し、port forwarding と `127.0.0.1:18443` / `127.0.0.1:18444` だけを許可する。shell、PTY、SFTP、任意 path、任意 Docker 引数を許可しない。`kio-test` は Docker 操作権限を持つ信頼済み試験ユーザーであり、低権限の実行 sandbox とは扱わない。WSL の dispatcher は fixed deployment record と Rust 定義の `FIXED_SOURCES` を照合し、lease と phase state を管理する実装である。forced command は owner-private に設置した reviewed `kio-acceptance-tools` binary の `kio-acceptance-tools dispatch` で終端し、mutable repository script を直接実行しない。2026-09-26 に Daybreak Blue の account entitlement と専用設定を付けた CLI 応答を確認し、独立 review を開始した。専用設定で要求したモデルは Daybreak Blue だが、返却 metadata から actual model は確認できていない。route review 報告は回収済みであり、このモデル provenance 制約を残す。疎通確認や報告回収は route acceptance を意味しない。先行する Rust 実装の GPT-6 Sol 独立 review と Astra crosscheck も、route の実効構成や acceptance を意味しない。

client は remote `init` より前に durable owner-private 256-bit capability を create-only で保存し、全 verb でこれを stdin に渡す。`init` reply は status だけで capability を返さないため、reply 消失時も client は同じ capability を保持し authenticated `finish` を実行できる。`finish` は unknown/interrupted phase でも許可されるが、controller cleanup が成功し、exited container を含め exact Docker project label の両方を照会できた後だけ lease を削除する。cleanup または照会失敗時は unknown lease を残す。`init` 中の compose 欠落は対応する exact project の不在を証明できる場合だけ安全である。pre-lease failure は diagnostic root を残すが global lease/controller は作らず、同一 attempt は create-only のまま、新しい GitHub attempt だけが retry できる。

この二段の forward により、WSL の controller/API は loopback のまま保ち、Actions job が直接 WSL service を公開しない。TLS service identity と Kio trust registration は receipt binding の対象になり得るが、本提案は mTLS を主張しない。

## Tailscale: full replacement policy proposed for approval

2026-09-26、利用者のログイン後に primary agent が Chrome の policy editor 全文を
read した。active policy は `grants: [{src:["*"], dst:["*"], ip:["*"]}]` と
下記の既存 `ssh` stanza だけで、残りは example comments だった。
現在の2台は Mac `100.102.187.14` と Windows `100.68.218.37`。
この記録は UI read の active semantics を正規化したもので、元 HuJSON の
byte-for-byte export ではない。UI の編集・保存は行っていない。

以下は追加だけでなく **wildcard grant の置換**を含む full policy proposal である。
既存 Mac→Windows TCP22 とその SSH 内の WSL/TCP tunnel を維持し、CI は同じ
Windows TCP22 だけに到達できる。Mac 宛て、Windows の他ポート、他ノード宛ての
新規接続は許可しない。元 policy が許可していた Windows→Mac や RDP などの
経路は失われるため、この縮小も所有者承認の対象となる。`ssh` stanza はそのまま
保持するが、それ自体は network grant を追加しない。これは Windows OpenSSH
への経路であり、Tailscale SSH の新規有効化を求めない。

```json
{
  "tagOwners": {
    "tag:kio-ci": [
      "autogroup:admin"
    ]
  },
  "grants": [
    {
      "src": [
        "100.102.187.14"
      ],
      "dst": [
        "100.68.218.37"
      ],
      "ip": [
        "tcp:22"
      ]
    },
    {
      "src": [
        "tag:kio-ci"
      ],
      "dst": [
        "100.68.218.37"
      ],
      "ip": [
        "tcp:22"
      ]
    }
  ],
  "ssh": [
    {
      "action": "check",
      "src": [
        "autogroup:member"
      ],
      "dst": [
        "autogroup:self"
      ],
      "users": [
        "autogroup:nonroot",
        "root"
      ]
    }
  ],
  "tests": [
    {
      "src": "100.102.187.14",
      "proto": "tcp",
      "accept": [
        "100.68.218.37:22"
      ],
      "deny": [
        "100.68.218.37:443",
        "100.68.218.37:2222",
        "100.68.218.37:3389",
        "100.68.218.37:18443",
        "100.68.218.37:18444"
      ]
    },
    {
      "src": "tag:kio-ci",
      "proto": "tcp",
      "accept": [
        "100.68.218.37:22"
      ],
      "deny": [
        "100.102.187.14:22",
        "100.102.187.14:443",
        "100.102.187.14:5900",
        "100.68.218.37:21",
        "100.68.218.37:23",
        "100.68.218.37:443",
        "100.68.218.37:2222",
        "100.68.218.37:2375",
        "100.68.218.37:2376",
        "100.68.218.37:3389",
        "100.68.218.37:18443",
        "100.68.218.37:18444",
        "tag:kio-ci:22"
      ]
    },
    {
      "src": "tag:kio-ci",
      "proto": "udp",
      "deny": [
        "100.68.218.37:22",
        "100.68.218.37:53",
        "100.68.218.37:443",
        "100.102.187.14:22",
        "100.102.187.14:53"
      ]
    },
    {
      "src": "100.68.218.37",
      "proto": "tcp",
      "deny": [
        "100.102.187.14:22",
        "100.102.187.14:443",
        "100.102.187.14:5900",
        "tag:kio-ci:22"
      ]
    }
  ]
}
```

`tag:kio-ci` は既存 workflow と一致させる。`tagOwners` は
`autogroup:admin` だけとし、federated identity credential の `auth_keys`
権限もこの tag だけに制限する。現在の UI では Auth Keys の Write が必要で、
Read も自動的に有効になるため、承認対象は **Auth Keys Write + Read** と
`tag:kio-ci` だけである。他の API 権限は付けない。tag の自己付与権限、他 tag、device 管理や policy
編集権限は付けない。Mac と Windows を retag せず、GitHub ephemeral runner に
だけこの tag を付ける。credential の具体的 UI scopes と許可 tag を承認前に
再確認し、余分な権限を要求された場合は停止して差分を review する。

OIDC issuer は `https://token.actions.githubusercontent.com`、subject は完全一致の
`repo:ttokunaga-ja@141212911/kio@1220844216:environment:v1-local-acceptance` とする。audience は新規
federated credential が指定する値を `KIO_TS_AUDIENCE` と一致させ、client ID を
`KIO_TS_CLIENT_ID` に設定する計画である。値はまだ発行・保存していない。
この repository の live OIDC API は `use_default: true`、
`use_immutable_subject: true`、prefix
`repo:ttokunaga-ja@141212911/kio@1220844216` を返した。repository API の
owner ID `141212911` と repository ID `1220844216` も一致した。
名前だけの旧 subject は現在の token と一致しないため使わない。
[GitHub の immutable subject 契約](https://docs.github.com/en/actions/reference/security/oidc#immutable-subject-claims)
と live 設定を根拠とし、GitHub 側の subject 設定自体は変更しない。

Tailscale federated identity の Custom claims に次の4条件をすべて追加する。
subject と各 claim は完全一致とし、asterisk や pattern を含めない。

| Claim | Exact required value |
|---|---|
| `ref` | `refs/heads/main` |
| `workflow_ref` | `ttokunaga-ja/kio/.github/workflows/v1-local-acceptance.yml@refs/heads/main` |
| `event_name` | `workflow_dispatch` |
| `runner_environment` | `github-hosted` |

この workflow は reusable workflow ではないため `job_workflow_ref` は条件に
含めない。subject 自体に branch は含まれないが、`ref` が独立して main に束縛する。
Environment の main-only deployment branch、承認設定、workflow 内の
exact-current-main SHA/package gate は別々の防御として維持・確認する。
repository-only subject、旧 names-only subject、別 workflow、別 Environment、
非 main ref、非 dispatch event、self-hosted runner を許可しない。
[Tailscale の claim value 契約](https://tailscale.com/docs/features/workload-identity-federation#claim-value-format)
に従い、作成前のレビューで全条件と scope/tag を確認する。
exact proposal は外部 `tailscale-oidc-proposed-identity.json` に保存し、SHA-256 は
`cac05cc9e9a2b220219a7d1eb96c50aa38969a3c68b5f6c458328a5aedb35225`。client ID と audience の未発行値は null とし、
この JSON を API request schema や発行済み credential として扱わない。

[Tailscale grants syntax](https://tailscale.com/docs/reference/syntax/grants) の
`tcp:22` と [policy tests](https://tailscale.com/docs/reference/syntax/policy-file#tests)
に従う。tests の deny は policy 内の deny rule ではなく、拒否を期待する assertion。
各 destination は単一 numeric port とし wildcard/range を使わない。
上記は JSON と限定的なローカル構造検証が完了している。UI の unsaved preview は
wildcard 削除と2 grant追加を表示したが、29 assertions の公式 engine 成功は
確認していない。user-keyed Preview rules の既存 user 表示は IP selector や CI tag の
評価証拠にならない。draft は Discard し、live wildcard policy と disabled の
Save/Discard を再確認した。Save は押していない。代表ポートの tests は全通信の実測ではない。独立 review と
所有者承認後、適用時の policy validation と既存 Mac 接続・CI denial tests を行う。
[OIDC federation](https://tailscale.com/docs/features/workload-identity-federation) と
[GitHub Action](https://tailscale.com/docs/integrations/github/github-action) の現在の
scope/tag 条件を credential 作成直前にも確認する。

外部 evidence の `tailscale-proposed-policy.json` が正本で、その SHA-256 は
`7c85bbfc624aecfaf6997e30ce76e6fce97d2ea85b075cdfc229c6701fac29a9`。
同じディレクトリに live active semantics、authority delta、JSON検証結果を保存する。
適用直前に live policy 全文を再読し、変更があればこの proposal を再生成する。
現行 broad `Tailscale-In` firewall rules はこの proposal では変更しない。
Tailscale grant は overlay の到達先を制限し、host firewall はその到着 packet の
admission を担う。Windows OpenSSH の account/key 認証と、認証後の session・
forwarding channel 制限はさらに別の制御である。TCP22 に到達できることだけで
Windows shell、WSL login、Docker 権限は得られない。普通の Windows OpenSSH に
接続する構成であり、既存 Tailscale SSH stanza の保持は Tailscale SSH の有効化を
意味しない。Windows firewall 自体を narrow と主張しない。firewall を狭める場合は既存 route への影響を含む別の具体案を承認する。

## GitHub 側で必要な設定値

`v1-local-acceptance` Environment は primary agent の live read 時点で存在しない。
具体案は deployment branch を `main` だけにし、required reviewers は追加しない
（空配列）。個人所有者が `workflow_dispatch` を行う運用を意図するが、これは
owner-only の技術的強制ではない。今回の read-only GitHub API 再確認では
`collaborators?affiliation=all` の全ページに `ttokunaga-ja` だけがあり、role は
`admin`、`admin/maintain/pull/push/triage` はすべて true だった。
repository Actions policy は `enabled: true`、`allowed_actions: all`、
`sha_pinning_required: false`。外部 evidence は
`github-dispatch-principals-live.json` に保存した。
これは現在の collaborator 一覧の snapshot であり、所有者だけに権限を固定する
allowlist ではない。将来 write/admin を与えられた人も、適用される branch rules の
範囲で main を編集し、workflow dispatch できる。main-only/no-required-reviewer
Environment 自体はその人の実行を止めず、将来の権限追加時にもこの authority を
再評価する必要がある。これらの API read は全 GitHub Apps/token の権限監査ではない。
この main-only/no-required-reviewer 構成自体が所有者承認の対象であり、
reviewer 制限が既にあると主張しない。OIDC の main/workflow/event/hosted-runner
条件と workflow 内の exact SHA gate は独立した制約として残す。

`.github/workflows/v1-local-acceptance.yml` が要求する設定値は次だけである。

| 種別 | 名前 | 用途 |
|---|---|---|
| Environment secret | `KIO_LAB_SSH_PRIVATE_KEY` | 新規 route 専用 SSH private key |
| Environment variable | `KIO_TS_CLIENT_ID` | Tailscale GitHub Action の OIDC client ID |
| Environment variable | `KIO_TS_AUDIENCE` | 同 Action の OIDC audience |
| Environment variable | `KIO_LAB_KNOWN_HOSTS` | `kio-lab-jump` と `kio-lab-wsl` の pinned host-key records |
| Environment variable | `KIO_LAB_JUMP_HOST` | Windows jump host の Tailscale address/name |
| Environment variable | `KIO_LAB_JUMP_USER` | 固定値 `kio-ci` |

route key は新規生成し、既存 Mac key を複製・再利用しない。`known_hosts` は `HostKeyAlias kio-lab-jump` と `kio-lab-wsl` を固定し、host-key rotation は別途 review して両方の pin を更新する。Environment protection は repository、Environment、main-only ref、OIDC tag request の組を束縛する必要がある。

## 事前検証と導入後の検証

外部変更を申請する前に、Windows で現行 `sshd_config` と effective configuration、port 22 listener、enabled firewall rules、`rm2c` の existing authorization path を記録する。Windows `kio-ci` 不在と、WSL の既存 `kio-test` に新規CI鍵が未登録であることを確認する。導入後の `kio-ci` は Administrators/Docker group 非所属とする。18443/18444 の loopback binding、両 host alias の現在の host key も確認する。Tailscale は適用直前に live policy を再読し、上記 full replacement の削除・追加範囲と owner の変更権限を確認する。

承認後は dedicated key の fingerprint を記録してから、Windows Match、Windows user key、WSL forced-command key と stream-local 制限、承認済み full tailnet policy、Tailscale OIDC binding、Environment values を導入する。現案は既存 host firewall rules を変更しない。各段階で `kio-ci` の local forward 以外の port、interactive shell、PTY、SFTP、agent/X11 forwarding、Docker access が拒否されることを確認する。続けて three hosted OS から pinned aliases による forward、fixed dispatcher verb、service identity の read-only observation、lease cleanup を確認する。candidate execution、paid provider call、workflow dispatch はこの導入前検証に含めない。

## ロールバック

接続または権限検証が失敗した場合は、追加した Windows Match stanza と `kio-ci` authorized key、WSL `kio-test` forced-command key、専用 firewall route、`tag:kio-ci` OIDC/ACL grant、Environment route secret/variables を撤去または無効化する。専用 key は失効させる。通常の rollback は CI grant/tag/credential を取り除き Mac→Windows TCP22 を残す。元の wildcard policy 全体への復元は広い権限を再付与するため、別途所有者承認が必要である。最後に Windows OpenSSH の global administrator key file、`AllowUsers rm2c`、既存 Mac Tailscale firewall rule、generic OpenSSH rule の disabled 状態を再読して、既存 Mac/admin route に変更がないことを確認する。

## Appendix: dispatcher deployment record

The preinstalled canonical `deployment.json` binds the installed candidate,
the installed tools binary, and all fixed controller inputs. Its schema is
`kio.v1.local_gpu.deployment/v2`; its `tools_binary_sha256` and every entry in
its `sources` map are lowercase SHA-256 hex. The exact source set is the
Rust-owned `FIXED_SOURCES` in
`crates/kio-eval/src/acceptance_tools/route_preflight.rs`, including Cargo
manifests/lockfile, acceptance-tools modules, GPU controller inputs, and fixed
compose/configuration files. Do not handwrite the old 19-field/v1 digest JSON
or copy a source list into this document: generated preflight output is
authoritative.

Before any installation, the trusted Mac operator can produce this proposal
without writing it or contacting a remote system, after building the helper
from the reviewed candidate:

```
target/debug/kio-acceptance-tools route-preflight --repository "$PWD" --candidate <reviewed-main-sha> --tools-binary target/debug/kio-acceptance-tools
```

The result is review material only. It is emitted only when the requested SHA
resolves locally, is the exact current `HEAD`, every Rust-defined fixed source
is a clean regular committed blob, and the helper binary passes its bounded
trusted-file digest check. It does not establish GitHub `main`, branch
protection, an Actions package, independent binary provenance, or remote
installation. For this source-derived route, the approved exact default
`main` candidate is the authority trust root; same-job helper hash transfer
establishes transfer integrity only.

The deployment record is a future owner-private WSL file beside the installed
binary dispatcher, not a repository artifact. The proposed WSL source tree is
`/home/kio-test/work/kio/v1-actions-source`, preserving the reviewed candidate
layout below it. Install the dispatcher and deployment record at
`scripts/v1-local-gpu/kio-acceptance-tools` and
`scripts/v1-local-gpu/deployment.json` below that source root. The forced command
ends in `kio-acceptance-tools dispatch`; it
does not name a source-tree Python dispatcher. The dispatcher's fixed repository
root resolves to `/home/kio-test/work/kio/v1-actions-source`. The source root
must be owned by `kio-test`; its trusted ancestors may be owned by `kio-test`
or root, and must not be writable by group or others;
the dispatcher directory is `0700`, its deployment record is `0600`, and the
installed fixed source files are not group- or other-writable. The separate
create-only runtime root remains `/home/kio-test/work/kio/v1-actions-gpu`; it
is not a source tree and must not contain the dispatcher.

## Appendix: proposed configuration for approval review

These are exact *proposed* fragments, not commands to run and not evidence that
the current hosts support every option. First capture the live Windows and WSL
effective configurations and validate the resulting candidate configuration
with `sshd -t`. On Windows, evaluate the Match stanza with
`sshd -T -C user=kio-ci,host=localhost,addr=127.0.0.1`; on WSL, evaluate the
forced-command user's policy with
`sshd -T -C user=kio-test,host=localhost,addr=127.0.0.1`.

Windows requires a new `kio-ci` standard user, a dedicated key in that user's
authorized-keys file, and an addition to the existing `AllowUsers` policy that
preserves `rm2c`. The reviewed per-user stanza should be equivalent to:

```
Match User kio-ci
    AuthorizedKeysFile .ssh/authorized_keys
    AllowTcpForwarding local
    PermitOpen 127.0.0.1:2222
    GatewayPorts no
    PermitTTY no
    X11Forwarding no
    AllowAgentForwarding no
    AllowStreamLocalForwarding no
    PermitTunnel no
    MaxSessions 0
```

The existing OpenSSH build must prove these options effective; unsupported
options must not be treated as restrictions. The `MaxSessions 0` rule prevents
session channels while `AllowTcpForwarding local` plus `PermitOpen` leaves only
the WSL loopback SSH hop. Confirm the account is outside Administrators and all
Docker-authorizing groups after creation.

The installed Windows OpenSSH rejects `PermitUserEnvironment` inside `Match`.
Its global effective value must remain `no`; that is an installation precondition,
not a per-user directive. On 2026-09-26 the corrected temporary configuration
passed `sshd -t` and effective-policy checks for both users. The only `rm2c`
policy difference was the additional `kio-ci` entry in `AllowUsers`. Syntax and
effective-policy checks do not prove admission or runtime denial behavior.

For the same newly generated CI public key, the *additional* WSL
`kio-test/.ssh/authorized_keys` line should restrict the forced dispatcher and
only its two local API destinations. Substitute the actual reviewed public key;
do not put a private key, candidate, or capability into this file:

```
command="/home/kio-test/work/kio/v1-actions-source/scripts/v1-local-gpu/kio-acceptance-tools dispatch",no-agent-forwarding,no-pty,no-user-rc,no-X11-forwarding,permitopen="127.0.0.1:18443",permitopen="127.0.0.1:18444" ssh-ed25519 <dedicated-ci-public-key> kio-v1-local-acceptance
```

`no-port-forwarding` is intentionally absent because the two `permitopen`
destinations are required for the acceptance tunnel. The final WSL `sshd`
policy must reject remote forwarding and any other destination; test that
explicitly rather than relying on the key comment. TCP `permitopen` does not
restrict Unix-domain socket forwarding. Append the following to the active WSL
configuration, `/etc/ssh/sshd_config_kio`, only after owner approval:

```
Match User kio-test
    AllowStreamLocalForwarding no
```

This restriction applies to the existing Mac key as well as the future CI key.
It removes direct Unix-domain socket forwarding; ordinary SSH commands, file
transfer and the documented local TCP forwarding remain available. The 2026-09-26
stdin-only parser/effective-policy check passed and showed this as the sole
WSL policy difference. No active configuration was changed. The denial matrix
must include a Unix-domain socket target, including the Docker socket; the
forced command alone does not restrict forwarding channels. The distinct
controls are documented by [OpenSSH](https://man.openbsd.org/sshd_config).

The full WSL candidate is retained as external
`wsl-proposed-sshd_config_kio.conf`, SHA-256
`584f92673fe7146fa46382455a396e1bf102ca6ad2399f8ae023ad7c645bce4c`.
Its live source, `/etc/ssh/sshd_config_kio`, was reread without mutation and
matched SHA-256 `8f5eba942b10bea3ed2c213f41691c5d0f65b6c2f38d9f4fa45f94b6c0989de4`.
The complete candidate retains the existing global `AllowTcpForwarding local`,
which supplies the remote-TCP-forwarding refusal omitted from the short Match
fragment above. The new Match supplies only `AllowStreamLocalForwarding no`.
These are the exact bytes used by the earlier successful stdin parser/effective
check, not a newly installed configuration. Runtime denial tests for remote TCP
forwarding remain required; parser acceptance alone does not establish them. The Windows full candidate remains
`windows-proposed-v2.conf`, SHA-256
`cd7d2fa0aeb097b3db3f9673ca2d9e2b3d216575139441ce07ed3fdfa029b286`.
Both are retained with the external 2026-09-26 preflight evidence. Before installation, reread the live
configuration and compare its recorded hash; any intervening change requires
regenerating the proposal. The broad existing Windows Tailscale rules and
the pending approval of the concrete tailnet replacement remain preconditions.
Adding a narrow allow rule cannot narrow an existing broader allow rule.

Pre-approval package checklist: live effective Windows and WSL SSH
configuration; current listener, firewall, Tailscale ACL/grant, and policy-owner
evidence; a new-key generation plan and expected fingerprint format;
`kio-acceptance-tools route-preflight` output for the reviewed main SHA; and the exact proposed
Windows/WSL configuration fragments above. Only after this package is
reviewable should the owner approve the concrete creation of users, keys,
ACL/firewall rules, Environment values, and the WSL forced-command entry.

Post-approval installation and verification checklist: install the exact
owner-private `deployment.json` generated by the approved clean preflight; then
perform denial tests for shell, PTY, SFTP, agent/X11, remote forwarding,
non-2222 Windows forwarding, and non-18443/18444 WSL forwarding. Record the
new key fingerprint and re-read the affected effective configurations, policy,
and listener/firewall state before any candidate workflow dispatch.

## 2026-09-26時点の未完了記録

この文書は提案と review checklist だけである。tailnet policy の live read と具体的 replacement proposal は用意済みだが、所有者承認、Windows/WSL account and SSH configuration、GitHub Environment values、dedicated key generation、route connectivity、3 OS receipt は未構成・未実行である。2026-09-26 に Daybreak Blue の account entitlement と専用設定を付けた CLI 応答を確認し、独立 review を開始した。route review 報告は回収済みだが requested model は Blue、actual model は metadata で未確認である。CLI の疎通や review 報告は installed route の完了証拠ではない。dispatcher/client 実装と GPT-6 Sol 独立 review・Astra crosscheck があっても、private CI route、3 OS Actions、authenticated local acceptance、または Blue review 完了を主張しない。

## 設定の一次資料

[Microsoft の Windows OpenSSH 設定](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh-server-configuration)、[OpenSSH の Match・MaxSessions・PermitOpen 契約](https://man.openbsd.org/sshd_config)、[Tailscale GitHub Action の OIDC 接続](https://tailscale.com/docs/integrations/github/github-action) を参照する。実装対象 OS の `sshd -t` と `sshd -T -C` でも新しい設定だけを事前検証する。
