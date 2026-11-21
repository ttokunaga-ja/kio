# Authenticated OCR GPU controller

`gpu-phase.sh` is the host-side PaddleOCR controller for both acceptance hosts.
Run it as `kio-test`. The private route binds the pinned
OCR service and public TLS identity to the exact release candidate.
A08 stops OCR and releases its GPU lease before the Actions client explicitly
installs and verifies EG2 in its isolated device state. The packaged native
runtime on the client runs indexing and queries with `--offline`.

The controller uses the reviewed `kio-acceptance-tools` binary from
`crates/kio-eval/src/acceptance_tools` and `acceptance_tools_main.rs`.
`init` stages [compose.yaml](config/compose.yaml) and
[backend-config.yaml](config/backend-config.yaml) and copies this binary into
the OCR directory. It switches the staged Compose API mapping to TLS and adds
the proxy, entrypoint, and certificate mounts.

## Native WSL Rust environment

The `kio-test` account has a native Linux Rust environment. Rust/Cargo 1.98.0
are available through `/home/kio-test/.cargo/bin` in normal login and interactive
Bash sessions. A fresh SSH session and an ordinary offline Cargo build/run
verified this setup. Existing toolchains were retained.

For a shell opened before setup, load `source ~/.cargo/env`, then check
`rustc --version` and `cargo --version`. This setup does not install the private
Actions account, keys, dispatcher, or SSH configuration; those host changes
remain covered by the separate host-application approval.

## Controller contract

`init` creates the private runtime directories, one seven-day CA and localhost
leaf certificate (SANs `127.0.0.1`, `::1`, and `localhost`), stages the fixed
OCR inputs, then writes the create-only `kio.local-gpu.ocr-identity/v1` record.
The record binds the compose and runtime inputs, public TLS identities and
measured OCR image. The phase observation binds the same identity to two
digest-verified running image IDs.

Use `init`, `status`, `start-ocr`, `stop-ocr` and `cleanup`.
OCR exposes `127.0.0.1:18443:8443`. The API container runs the fixed PaddleX
command behind the bounded TLS proxy.

The Actions client publishes its owner-private 256-bit capability before
remote `init` and sends it to the dispatcher on standard input. A lost reply
leaves the client with the capability needed for authenticated `finish`.
`finish` removes the lease after cleanup proves the exact OCR project absent,
including exited containers. A failed cleanup preserves the lease as unknown.

Before starting, the controller proves the OCR project absent and captures an
immutable private GPU baseline. It checks the GPU UUID and total memory,
requires 6,141 MiB plus 128 MiB of free memory, and refuses unrelated running containers on the dedicated engine. Cleanup waits for the same GPU to return to within 128 MiB of
its recorded used memory. After interrupted init, cleanup without a baseline
requires a valid GPU sample and proof that the exact OCR project is absent.

## Candidate-bound deployment proposal

The forced private route uses an root-controlled deployment record with schema
`kio.v1.local_gpu.deployment/v2`. Do not handwrite this record or a stale
fixed-source digest list. Generate the proposal from a clean exact candidate
with the candidate-built helper. The source map includes both OCR YAML files
above, and the installed dispatcher verifies those paths and digests before
invoking the controller.

```bash
target/debug/kio-acceptance-tools route-preflight \
  --repository "$PWD" \
  --candidate <reviewed-main-sha> \
  --tools-binary target/debug/kio-acceptance-tools
```

Preflight requires the requested lowercase 40-hex candidate to resolve and be
the exact local `HEAD`; every member of the Rust-defined `FIXED_SOURCES` list
must be a clean committed regular blob; and the helper binary must pass its
bounded trusted-file digest check. Its output includes the v2 schema,
`tools_binary_sha256`, and the complete source map. It is review material only:
it proves neither that GitHub `main` is protected nor that a remote deployment
or Actions package exists.

For source-derived routes, the exact default `main` candidate approval is the
authority trust root. Same-job transfer of a helper hash proves only transfer
integrity; it is not independent provenance for a candidate-built executable.
The dispatcher must be installed as the reviewed binary and the forced command
must end in `kio-acceptance-tools dispatch`, never a mutable repository script.

## 受入ホストの構築

| GPU | Windows の WSL 登録所有者 | WSL2 ディストリビューション | Windows の既存 `AllowUsers` | Tailscale IPv4（調査時） |
| --- | --- | --- | --- | --- |
| 研究室 RTX 4060 | `rm2c` | `Ubuntu-26.04` | `AllowUsers rm2c` | `100.68.218.37` |
| 個人 RTX 4070 | `ttokunaga-ja` | `Ubuntu`（24.04） | `AllowUsers ttokunaga-ja@100.102.187.14` | `100.97.109.109` |

両台で Windows NVIDIA ドライバー、WSL2 の `/dev/dxg`、systemd、Windows と WSL の
OpenSSH を用意する。WSL は `sudo`、 `rootlesskit`、`slirp4netns`、`uidmap`、`dbus-user-session`、
`curl`、OpenSSL、Python 3 が必要。 `/run/sshd` は root 所有・0755。既存の WSL Ed25519
ホスト鍵を使う。systemd を初めて有効化する場合は対象 distro だけを作業時間内に再起動する。
既存の `ssh.service` / `ssh.socket` と portproxy は棚卸しされる。TCP 2222 に競合があれば
現地で移行を調整し、既存サービスを維持する。

1. 同じレビュー済み版の Docker Engine 静的配布物（28.3 以降）を `/opt/kio-v1-local/docker`
   に展開し、同じ版の rootless extras から `dockerd-rootless.sh` を同ディレクトリに置く。
   RootlessKit は Ubuntu の `/usr/bin/rootlesskit` を使う。CDI 対応の Compose V2 は
   `/opt/kio-v1-local/docker-cli/cli-plugins/docker-compose` に置く。各実行ファイル、構成、
   祖先を root 所有・group/other 書込不可にする。Ubuntu の AppArmor では対象となる
   ディストリビューションの RootlessKit プロファイルを使う。
   [Docker Rootless](https://docs.docker.com/engine/security/rootless/) ·
   [Docker の CDI](https://docs.docker.com/reference/cli/dockerd/#configure-cdi-devices)
2. [候補固定の配置](#candidate-bound-deployment-proposal)に従い、同じ候補の固定入力、
   Linux helper、canonical な `deployment.json` を `/var/lib/kio-v1-local/source` 以下に
   配置する。ファイル・全祖先は root 所有・group/other 書込不可・非 symlink。
   `kio-test` が読めるよう、ディレクトリとバイナリは 0755、deployment は 0644 とする。
   deployment の内容は公開 digest。候補の更新は受入とリース回収が終わってから行う。
3. 両台の各段に同じ受入用 Ed25519 公開鍵を渡す。秘密鍵は GitHub Environment と承認済みの
   保管先で保持する。NVIDIA の署名付き APT リポジトリから取得・照合した
   `nvidia-container-toolkit-base` 1.20 以降の .deb と、レビュー済み CUDA smoke image の
   digest を用意する。研究室の VHD 空き容量は現地で再確認し、30〜50 GiB を見込む。

   ```bash
   sudo --preserve-env=WSL_DISTRO_NAME bash scripts/v1-local-gpu/host/setup-wsl.sh \
     Ubuntu-26.04 /path/kio-ci.pub /path/toolkit-base.deb "$CUDA_SMOKE_IMAGE"
   ```

   個人機では `Ubuntu` を渡す。Toolkit は .deb の CDI 用バイナリだけを専用パスへ展開する。
   GPU は CDI で渡し、グローバルな `no-cgroups` や共有 Docker の設定を変更する必要はない。
   WSL が生成する `nvidia.com/gpu=all` を使い、導入時に GPU が 1 台であることとコンテナ内外の
   UUID 一致を確認する。ソケットは `/run/user/<uid>/kio-v1-local-docker.sock`、data-root は
   `/var/lib/kio-v1-local/state/docker`。 `kio-test` が他グループに所属するか共有ソケットに
   書き込める場合は適用を止める。起動中の共有 Engine の bind mount にアクセスできる場合も
   止める。共有アプリの独自データパスや Windows マウントの ACL は現地で確認する。
   既存の管理者は別の管理アカウントを使う。WSL の既存 sshd は drop-in の
   `DenyUsers kio-test` を読み、稼働中は reload する。 `kio-test` のパスワードは lock し、
   旧 `~kio-test/.ssh/authorized_keys` は同じディレクトリ内の専用退避先へ移す。
   既存のセッションまたは kio-test UID のプロセス（専用 Docker、user manager を含む）があれば、
   一覧を表示して変更前に止まる。終了を許可する場合だけ `bash scripts/v1-local-gpu/host/setup-wsl.sh`
   の直後に `--terminate-kio-test` を渡す。リースの不在を確認後、当該アカウントのセッションと
   残存プロセスを終了し、消失を確認してから進む。専用 sshd は root 管理の鍵と ForceCommand を使う。 [Ubuntu の SSH drop-in](https://ubuntu.com/server/docs/openssh-server/)
   [NVIDIA CDI](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/cdi-support.html) ·
   [WSL の CDI デバイス名](https://github.com/NVIDIA/nvidia-container-toolkit/blob/v1.20.1/pkg/nvcdi/lib-wsl.go)
4. Windows のローカルコンソールで WSL 登録所有者として昇格した 64-bit PowerShell を開く。
   管理用 Mac と検証用 CI の実際の Tailscale IPv4 を渡す。最初はタスク用 Windows 資格情報が必要。
   スクリプトは既存の portproxy、待受、広い firewall 許可を検査し、受信許可を Tailscale IF、
   宛先 Tailscale IPv4、`sshd.exe`、送信元 `100.64.0.0/10`、TCP 22 に限定する。

   ```powershell
   $cred = Get-Credential -UserName "$env:COMPUTERNAME\rm2c" -Message 'WSL keepalive task'
   .\scripts\v1-local-gpu\host\setup-windows.ps1 -WslOwner rm2c -Distro Ubuntu-26.04 `
       -ExistingAllowUsers 'AllowUsers rm2c' -PublicKeyFile C:\path\kio-ci.pub `
       -AdminSource 100.102.187.14 -CiSource $ObservedCiIpv4 -KeepaliveCredential $cred
   Remove-Variable cred
   ```

   個人機は表の値に置き換える。対応している OpenSSH オプションはインストール済みの
   `sshd.exe` で判定する。専用タスクは起動・ログオン時に WSL の foreground プロセスを
   維持する。再起動とサインアウト後の疎通が確認できるまで旧タスクを保管する。
   [Windows OpenSSH](https://learn.microsoft.com/windows-server/administration/openssh/openssh-server-configuration) ·
   [WSL と systemd](https://learn.microsoft.com/windows/wsl/systemd/)

### Tailscale の設定

Tailscale 管理者が現在の policy に次の条件を反映する。既存の Mac 管理経路を維持し、
接続先端末の現行アドレスを Machines で確認する。下は該当部分の例。

```json
{
  "tagOwners": { "tag:kio-ci": ["autogroup:admin"] },
  "grants": [
    { "src": ["tag:kio-ci"], "dst": ["100.68.218.37", "100.97.109.109"], "ip": ["tcp:22"] }
  ],
  "tests": [
    {
      "src": "tag:kio-ci",
      "accept": ["100.68.218.37:22", "100.97.109.109:22"],
      "deny": ["100.68.218.37:2222", "100.97.109.109:2222", "100.68.218.37:18443", "100.97.109.109:18443", "100.102.187.14:22"]
    }
  ]
}
```

重複する `acls` / `grants` も見直し、CI の tag から他の接続を許す規則を除く。
`tag:kio-ci` は短命の GitHub runner に割り当て、Windows には既存の端末 ID を使う。
OIDC Trust credential は対象 repo と `v1-local-acceptance` Environment の正確な subject、
`ref=refs/heads/main`、対象 `workflow_ref`、`event_name=workflow_dispatch`、
`runner_environment=github-hosted`、当該 tag に束縛する。接続は Windows OpenSSH を使う。
[Tailscale policy](https://tailscale.com/docs/reference/syntax/policy-file) ·
[GitHub Action と OIDC](https://tailscale.com/docs/integrations/github/github-action)

### 構築後の確認

検証用 SSH 設定では `kio-lab-jump`（選択した Windows、`kio-ci`、22）と `kio-lab-wsl`
（`127.0.0.1`、`kio-test`、2222、`ProxyJump kio-lab-jump`）を設定する。各 `HostKeyAlias`
には同じ別名を使い、`IdentitiesOnly yes`、受入用 `IdentityFile`、`StrictHostKeyChecking yes`、
4 本の pin を記載した `UserKnownHostsFile`、`GlobalKnownHostsFile none` を指定する。
Windows・WSL の公開ホスト鍵は現地コンソールの出力と照合する。

```bash
ssh -F "$cfg" -o BatchMode=yes -vv -W 127.0.0.1:2222 kio-lab-jump </dev/null
ssh -F "$cfg" -o BatchMode=yes -tt kio-lab-jump whoami
ssh -F "$cfg" -o BatchMode=yes kio-lab-wsl id
ssh -F "$cfg" -o BatchMode=yes -W 127.0.0.1:3389 kio-lab-jump </dev/null
ssh -F "$cfg" -o BatchMode=yes -W 127.0.0.1:22 kio-lab-wsl </dev/null
```

最初のコマンドで WSL のバナー、残りで認証後の拒否を確認する。両段の remote forward
には `-N -R 127.0.0.1:39123:127.0.0.1:22 -o ExitOnForwardFailure=yes` を使う。
WSL の Unix socket 転送は `-N -L 127.0.0.1:39124:/var/run/docker.sock` で開始し、
ローカルポートへの接続で SSH チャネルが拒否されることを確認する。WSL 段の IdentityFile
だけを旧管理用鍵へ差し替えた検証用設定でも、専用 listener が認証を拒否することを確認する。
Windows 側で省略されたオプションの機能も実接続で確認する。

Windows の LAN IPv4 を `WIN_LAN_IP`、Tailscale IPv4 を `WIN_TAILNET_IP` に設定し、それぞれ
指定の端末から TCP 接続を試す。4 件とも失敗し、管理 Mac と許可した CI の TCP 22 は成功することを
確認する。Tailscale 管理画面でも現在の grant・OIDC の条件を確認する。

```bash
# LAN 上の別端末から
nc -vz -w 5 "$WIN_LAN_IP" 2222
nc -vz -w 5 "$WIN_LAN_IP" 22
# tailnet の別端末から。TCP 22 は許可対象外の端末で試す。
nc -vz -w 5 "$WIN_TAILNET_IP" 2222
nc -vz -w 5 "$WIN_TAILNET_IP" 22
```

新規の Mac 管理者ログインにも成功したら、Windows コンソールに `verified` と入力する。
確認が失敗した場合は別の入力で設定を復元する。

WSL の管理コンソールで次を確認する。通常の SSH では `denyusers kio-test`、パスワード状態は
`L`。通常の SSH ポートが稼働中なら、旧鍵でのログインも拒否されることを別の管理端末で確かめる。
共有 socket に対する `test` と、追加引数付きの sudo は失敗が期待値。`sudo -l` では引数なしの
照会だけが許可されていることを確認する。照会は件数だけを返す。共有 Engine が構成済みで利用できない
場合は、稼働コンテナを確認できないため失敗する。

```bash
sudo /usr/sbin/sshd -T -C user=kio-test,host=localhost,addr=127.0.0.1 | grep -i '^denyusers '
sudo passwd -S kio-test
sudo -u kio-test test -w /run/docker.sock
sudo -l -U kio-test
sudo -u kio-test sudo -n -- /var/lib/kio-v1-local/shared-gpu-count
sudo -u kio-test sudo -n -- /var/lib/kio-v1-local/shared-gpu-count unexpected
```

Windows マウント・共有アプリ配下も、管理者が存在を確認済みの対象で実効権限を検査する。
次の `probe` は実在する保護対象（Windows の利用者ディレクトリ、または bind mount 配下の保護対象）に
置き換える。読み取り・書き込みテストはどちらも失敗が期待値。 `findmnt` で対象マウントが
未接続だった場合はその状態を記録し、権限拒否とは区別する。

```bash
probe='/mnt/c/Users/<実在する保護対象の Windows ユーザー>'
findmnt -T "$probe"
sudo test -e "$probe" && sudo -u kio-test test -r "$probe"
sudo test -e "$probe" && sudo -u kio-test test -w "$probe"
```

固定 OCR image は専用 Engine に pull する。受入直前に空き VRAM が 6,269 MiB 以上で、
Windows・WSL の他ワークロードによる GPU 使用がないことを管理者が確かめる。コントローラは
専用 Engine のコンテナを列挙し、VRAM の起動前後はホスト全体の実測で照合する。
共有 Engine には root 管理の引数なし照会だけを使う。sudoers の規則は `NOSETENV` を指定する。
構築時の CUDA スモークテストにも件数 0 が必要。admission・停止・cleanup の前に、
DeviceRequests を持つ稼働コンテナが 1 件以上、または照会に失敗した場合は中断する。
cleanup の中断時はリースを保持する。

```bash
uid=$(id -u kio-test)
docker=(/opt/kio-v1-local/docker/docker --host "unix:///run/user/$uid/kio-v1-local-docker.sock"
        --config /opt/kio-v1-local/docker-cli)
sudo -u kio-test env -u DOCKER_HOST -u DOCKER_CONTEXT "${docker[@]}" compose \
  -f scripts/v1-local-gpu/config/compose.yaml pull
```

### ロールバック

WSL の再適用は dispatcher と共通のロックを取得し、`lease.json` が存在する間は専用 Engine の
再起動前に拒否する。OCR 未起動・停止後でもリースは保護される。ロック導入前の専用 SSH が
稼働中の移行は、受入を完了して専用経路を閉じた状態で行う。ロックは root 所有、
kio-test 専用グループの 0640。旧 0644 からの移行では旧ファイルと新ファイルの両方に
ロックを取って更新する。
専用 Engine のコンテナの実行状態は復元の対象外で、稼働中の受入はこの適用前の拒否で保護する。

適用に失敗すると両スクリプトが今回の変更を復元する。明示パスに保存した退避と復元結果を確認する。
成功後の取り消しでは、適用直前の退避を使用する。

- Windows はローカルコンソールで退避版を `sshd.exe -t -f <backup>\sshd_config` に通す。
  `%ProgramData%\ssh\sshd_config` と鍵ファイル、保存した SDDL を復元する。元からなかった
  鍵ファイル、今回新設した `KioV1LocalGpuSshAllow` と `KioV1LocalGpuKeepalive` だけを撤去する。
  既存のタスクが Ready から起動された場合は止める。 `created-account` のある適用に限り作成した
  `kio-ci` を無効化・撤去する。sshd 再起動後に Mac 管理者の新規ログインを確認する。
- WSL は専用 SSH と `kio-test` の user systemd 上の専用 Docker を止める。退避ファイルを
  所有者・モード付きで復元し、`absent` に記録された今回の導入分を除く。system/user の
  unit を読み直し、保存した enabled・active・linger を復元する。既存アカウントは元の login shell
  に戻す。 `password-state` と `password-last-change` から元のパスワード状態を復元し、
  `authorized-keys-location` に退避した鍵がある場合は使用中のファイルを上書きせず戻す。
  元の SSH drop-in を復元後、稼働中の通常 sshd は reload する。新規アカウントを撤去する場合は
  専用データとホームを先に退避する。共有 dockerd の状態は維持する。
  `--terminate-kio-test` で終了したセッションやプロセスは復元の対象外。

## ホストの切り替え

`main` 専用の GitHub Environment `v1-local-acceptance` に以下を登録する。

| 種別 | 名前 | 値 |
| --- | --- | --- |
| Secret | `KIO_LAB_SSH_PRIVATE_KEY` | 両台・両段で使う専用秘密鍵 |
| Variable | `KIO_LAB_KNOWN_HOSTS` | Windows 2 本を `kio-lab-jump`、WSL 2 本を `kio-lab-wsl` とした計 4 行の公開ホスト鍵 |
| Variable | `KIO_LAB_JUMP_USER` | `kio-ci` |
| Variable | `KIO_LAB_JUMP_HOST` | 選んだ Windows の Tailscale IPv4 または DNS 名 |
| Variable | `KIO_TS_CLIENT_ID` | Tailscale OIDC client ID |
| Variable | `KIO_TS_AUDIENCE` | Tailscale OIDC audience |

`known_hosts` は `kio-lab-jump ssh-ed25519 <Windows公開鍵本体>` の行を 2 本、
`kio-lab-wsl ssh-ed25519 <WSL公開鍵本体>` の行を 2 本入れる。別名にポートは付けない。
鍵のローテーション時は該当する公開ホスト鍵をコンソールで再確認する。

Mac で開発中は研究室 4060 を優先し、停止・利用不可なら個人 4070 を選ぶ。4070 で開発中は
4070 を選ぶ。接続確認と候補配置の照合後、GitHub の Settings → Environments →
`v1-local-acceptance` で **`KIO_LAB_JUMP_HOST` のみ**を変更し、該当候補を dispatch する。
キュー中の run、3 OS のジョブ実行中、リース回収が未確認の間は接続先を固定する。

## RTX 4060 OCR measurement

The 2026-09-26 R32 measurement used an 8 GiB RTX 4060 under WSL2. The two image
digests and the resource settings in `config/` come from that run. PaddleX
requested 4,096 completion tokens plus a 14-token prompt, which exceeded the
initial 4,096-token context ceiling and returned HTTP 500. Raising
`max_model_len` to 4,160 resolved it while keeping
`max_num_batched_tokens: 4096`. VRAM rose from 575 to 6,716 MiB and returned to
575 MiB after teardown. Neither service was OOM-killed. The model inventory
included PP-DocLayoutV3, UVDoc, PP-LCNet document orientation and the pinned
PaddleOCR-VL-1.6 VLM weight file.

The public PDF produced two HTTP 200 responses that matched after removing
the top-level `logId` and serializing with sorted keys and compact separators.
The resulting SHA-256 was
`b765ca3a8fd7967d1720e2acb2c97cc2ebdb5a1d8c4e3cf959aafcf33c54e608`.
Both responses contained one page, one block and zero Markdown images.
This measurement covered the PDF text response. The controller's free VRAM
requirement uses the observed 6,141 MiB increase plus its 128 MiB margin.

### Reproduce the measurement

1. From the repository root on an RTX 4060 8 GiB WSL2 host, select the dedicated
   rootless Docker endpoint above and use
   `docker compose -p kio-v1-ocr-r32 -f scripts/v1-local-gpu/config/compose.yaml`.
   Run `config`, then `pull` if either pinned image is absent, and `up -d`.
   The measured `backend-config.yaml` SHA-256 was
   `b4d9319afd04eb011e02379245fa3a373e0d4ced6eccc482b99023fe09d79642`.
   The project publishes the API at `127.0.0.1:18080` and rotates local logs
   at 10 MiB × 3 per service. Sample used VRAM before startup and every five
   seconds through the requests using `nvidia-smi` (on WSL, available under
   `/usr/lib/wsl/lib/` if absent from PATH).
2. Wait for `http://127.0.0.1:18080/health`. In `paddleocr-vlm-server`,
   verify that
   `/home/paddleocr/.paddlex/official_models/PaddleOCR-VL-1.6/model.safetensors`
   hashes to `85a479d506a11e724e7285d395c551be69f41dbc16b6342d3cacfb189aed71db`.
   Verify the [public PDF](../../crates/kio-eval/acceptance-fixtures/v1/provider-public/ocr.pdf)
   hashes to `b48ae7a300b21746e078ffceb3865d324940d2d62595fb7fd201bac9f7906f90`.
3. Send two JSON POST requests to `/layout-parsing` with the base64 PDF as
   `file`, `fileType: 0` and `useLayoutDetection: true`. Pass the JSON via
   `curl --data-binary @<request-file>` with `Content-Type: application/json`.
   Check HTTP 200, `errorCode: 0`, and the counts in
   `result.layoutParsingResults`, then compare the canonical SHA-256 values.
   Retain the hashes and counts and discard the raw request and response bodies.
4. Inspect the two containers for `State.OOMKilled`, run `down --remove-orphans`
   with the same Compose project, and confirm VRAM returns to baseline.

The historical [capture conditions](../../crates/kio-adapter/tests/fixtures/layout-parsing/README.md)
describe the RTX 4070 configuration from which R32 took its pinned images.
The R32 records, source snapshot and original runner are in
`6dc109e9:tasks/artifacts/v1-gpu-4060/ocr/`.
