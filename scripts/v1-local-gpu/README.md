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

## 受入ホストの構築

| GPU | Windows の WSL 登録所有者 | WSL2 ディストリビューション | Windows の既存 `AllowUsers` | Tailscale IPv4（調査時） |
| --- | --- | --- | --- | --- |
| 研究室 RTX 4060 | `rm2c` | `Ubuntu-26.04` | `AllowUsers rm2c` | `100.68.218.37` |
| 個人 RTX 4070 | `ttokunaga-ja` | `Ubuntu`（24.04） | `AllowUsers ttokunaga-ja@100.102.187.14` | `100.97.109.109` |

両台で Windows NVIDIA ドライバー、WSL2 の `/dev/dxg`、systemd、Windows と WSL の
OpenSSH を用意する。WSL は `sudo`、`util-linux`、`rootlesskit`、`slirp4netns`、`uidmap`、`dbus-user-session`、
`curl`、OpenSSL、Python 3 が必要。 `/run/sshd` は root 所有・0755。既存の WSL Ed25519
ホスト鍵を使う。systemd を初めて有効化する場合は対象 distro だけを作業時間内に再起動する。
既存の `ssh.service` / `ssh.socket` と portproxy は棚卸しされる。TCP 2222 に競合があれば
現地で移行を調整し、既存サービスを維持する。

### 共有 Engine と interop の事前準備

- **研究室**。`rm2c` の対話セッションを維持し、Docker Desktop の **Settings → Resources →
  WSL Integration** で `Ubuntu-26.04` だけを有効にする。既存の Linux Engine と他のトグルは
  維持する。通常の **Apply** が使える場合だけ適用する。**Apply & restart** が表示される場合や、
  反映に Desktop・Engine の再起動が必要な場合はその場で止める。公式資料は Apply を案内しているが、
  研究室のインストール版での無停止反映は未確認。次を Windows で前後に実行し、Engine ID、
  競馬コンテナの ID と StartedAt が一致することを確認する。WSL 全体の終了、Desktop の再起動、
  rm2c のサインアウトは行わない。
  [Docker Desktop の WSL integration](https://docs.docker.com/desktop/features/wsl/#enable-docker-in-a-wsl-2-distribution)

  ```powershell
  $desktop = 'npipe:////./pipe/dockerDesktopLinuxEngine'
  docker -H $desktop info --format '{{.ID}}'
  docker -H $desktop ps -q | ForEach-Object {
      docker -H $desktop inspect --format '{{.Id}} {{.State.StartedAt}}' $_
  }
  ```

- 研究室の専用 distro の `/etc/wsl.conf` を退避し、`[interop]` 節に `enabled=false` と
  `appendWindowsPath=false` を設定する。既存の `[boot]` は保持する。設定の反映には影響を確認して
  **Ubuntu-26.04 だけ**を終了・起動する。管理者が WSL から Windows の `cmd.exe` を起動できなく
  なったことを確認し、次の Unix socket 照会で Windows 側と同じ Engine ID が出ることを確認する。
  Docker Desktop の WSL 2 backend はソケットを提供する。Windows `docker.exe` の起動に依存し、
  distro 内にソケットを提供しない Docker VMM の経路は使えない。
  研究室の版で interop 無効化によりソケットが利用できなくなり、専用 distro の再起動だけで
  復旧できる場合は、`wsl.conf` を戻して Ubuntu-26.04 だけを再起動し、ソケットを再確認する。
  この場合は setup の `--desktop-interop-required` を渡し、受入専用サービスの名前空間で
  制限する。Desktop の再起動が必要なら保留する。
  [WSL の distro 別設定](https://learn.microsoft.com/windows/wsl/wsl-config) ·
  [WSL の interop 実装](https://github.com/microsoft/WSL/blob/master/doc/docs/technical-documentation/interop.md)

  ```bash
  shared_socket=$(readlink -e /var/run/docker.sock)
  sudo test -S "$shared_socket"
  sudo curl -q --fail --silent --show-error --max-time 10 --noproxy '*' \
    --unix-socket "$shared_socket" http://localhost/info | jq '{ID,OperatingSystem}'
  ```

- **個人機**。通常の Ubuntu の `wsl.conf`、interop、既存 native Engine は維持する。
  `shared_socket=/run/docker.sock` を使い、上と同じ API で起動中の native Engine を照合する。
  setup は、両ホストとも指定した 1 ソケットが利用できなければ適用前に止まる。
  接続先は root 所有で、全ユーザーに書込可能な socket は使わない。`kio-test` は Docker グループに
  加えず、接続できないことを確認する。sudo は引数なしの件数照会だけを許可する。

### ホストへの配置

1. 同じレビュー済み版の Docker Engine 静的配布物（28.3 以降）を `/opt/kio-v1-local/docker`
   に展開し、同じ版の rootless extras から `dockerd-rootless.sh` を同ディレクトリに置く。
   RootlessKit は Ubuntu の `/usr/bin/rootlesskit` を使う。CDI 対応の Compose V2 は
   `/opt/kio-v1-local/docker-cli/cli-plugins/docker-compose` に置く。各実行ファイル、構成、
   祖先を root 所有・group/other 書込不可にする。Ubuntu の AppArmor では対象となる
   ディストリビューションの RootlessKit プロファイルを使う。
   [Docker Rootless](https://docs.docker.com/engine/security/rootless/) ·
   [Docker の CDI](https://docs.docker.com/reference/cli/dockerd/#configure-cdi-devices)
2. 初期構築用に、検証済みの `main` コミット
   `77016e56d7532401b7c3ad22c79ab4e5849142ad` を使う。このコミットの dispatcher は、
   候補未配置時の拒否と専用ロックの検査を備えている。受入ホストとは別の信頼済み
   Ubuntu 24.04 x86_64 Linux ビルド環境で、クリーンな当該コミットから Linux 版を作り、両ホストに渡す。
   Mac を使う場合は Linux で作ったファイルの中継に使う。ビルド元でコミットと SHA-256 を記録する。

   ```bash
   (
     set -euo pipefail
     bootstrap_sha=77016e56d7532401b7c3ad22c79ab4e5849142ad
     test "$(git rev-parse HEAD)" = "$bootstrap_sha"
     test -z "$(git status --porcelain)"
     cargo +1.98.0 build --release -p kio-eval --bin kio-acceptance-tools --locked
     bundle=$(mktemp -d)
     cp target/release/kio-acceptance-tools "$bundle/"
     (cd "$bundle" && sha256sum kio-acceptance-tools > SHA256SUMS)
     printf '%s\n' "$bundle"
   )
   ```

   `77016e56` の既存 Linux ビルドを使う場合は、ビルド記録の SHA-256 と照合する。
   `shared-gpu-count` は明示ソケット対応を含む **setup と同じレビュー済み commit** から採り、
   転送前にその SHA-256 を `SHA256SUMS` に追加する。
   転送後は WSL でレビュー済みの `SHA256SUMS` と照合し、 `file kio-acceptance-tools` が対象の
   Linux x86-64 ELF であることを確認する。下の配置例は `--help` の実行も確かめる。
   各祖先は root 所有、group/other 書込不可、非 symlink とし、管理者が照合した 2 ファイルを
   以下のパスに置く。root 管理下に置いた後のファイルも同じ SHA-256 と照合する。
   初期構築では `deployment.json` と F4 の固定入力を置かない。

   ```bash
   (
     set -euo pipefail
     sha256sum -c SHA256SUMS
     file kio-acceptance-tools
     ./kio-acceptance-tools --help
     dest=/var/lib/kio-v1-local/source/scripts/v1-local-gpu
     sudo install -d -o root -g root -m 0755 "$dest/host"
     sudo install -o root -g root -m 0755 kio-acceptance-tools "$dest/kio-acceptance-tools"
     sudo install -o root -g root -m 0755 shared-gpu-count "$dest/host/shared-gpu-count"
     sudo cmp kio-acceptance-tools "$dest/kio-acceptance-tools"
     sudo cmp shared-gpu-count "$dest/host/shared-gpu-count"
   )
   ```

3. 両台の各段に同じ受入用 Ed25519 公開鍵を渡す。秘密鍵は GitHub Environment と承認済みの
   保管先で保持する。NVIDIA の署名付き APT リポジトリから取得・照合した
   `nvidia-container-toolkit-base` 1.20 以降の .deb と、レビュー済み CUDA smoke image の
   digest を用意する。研究室の VHD 空き容量は現地で再確認し、30〜50 GiB を見込む。

   ```bash
   sudo --preserve-env=WSL_DISTRO_NAME bash scripts/v1-local-gpu/host/setup-wsl.sh \
     Ubuntu-26.04 /path/kio-ci.pub /path/toolkit-base.deb "$CUDA_SMOKE_IMAGE" "$shared_socket"
   ```

   個人機では `Ubuntu` と `/run/docker.sock` を渡す。Toolkit は .deb の CDI 用バイナリだけを専用パスへ展開する。
   GPU は CDI で渡し、グローバルな `no-cgroups` や共有 Docker の設定を変更する必要はない。
   WSL が生成する `nvidia.com/gpu=all` を使い、導入時に GPU が 1 台であることとコンテナ内外の
   UUID 一致を確認する。ソケットは `/run/user/<uid>/kio-v1-local-docker.sock`、data-root は
   `/var/lib/kio-v1-local/state/docker`。 `kio-test` が他グループに所属するか共有ソケットに
   書き込める場合は適用を止める。起動中の共有 Engine の bind mount にアクセスできる場合も
   止める。照会先は root 所有 0600 の `/etc/kio-v1-local-shared-gpu-socket` に保存する。
   照会はそのソケットだけに接続し、応答不能なら失敗する。
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
   管理用 Mac と検証用 CI の実際の Tailscale IPv4 を渡す。
   スクリプトは既存の portproxy、待受、広い firewall 許可を検査し、受信許可を Tailscale IF、
   宛先 Tailscale IPv4、`sshd.exe`、送信元 `100.64.0.0/10`、TCP 22 に限定する。

   ```powershell
   .\scripts\v1-local-gpu\host\setup-windows.ps1 -WslOwner rm2c -Distro Ubuntu-26.04 `
       -ExistingAllowUsers 'AllowUsers rm2c' -PublicKeyFile C:\path\kio-ci.pub `
       -AdminSource 100.102.187.14 -CiSource $ObservedCiIpv4
   ```

   個人機は表の値に置き換える。対応している OpenSSH オプションはインストール済みの
   `sshd.exe` で判定する。専用タスクはディストリビューションの登録所有者を S4U の principal にし、
   起動・ログオン時に WSL の foreground プロセスを維持する。S4U はアカウントのパスワードを要求せず、
   保存もしない。登録所有者に有効なバッチ ログオン権限があり、distro の VHDX は
   所有者がアクセスできるローカルの EFS 非暗号化ファイルであることを確認する。
   同名の旧 Password タスクが残っていれば適用前に止まる。旧タスクは管理者が退避し、
   設定移行を調整する。Password タスクへの自動切り替えはない。
   [Task Scheduler の S4U 制約](https://learn.microsoft.com/windows/win32/api/taskschd/ne-taskschd-task_logon_type) ·
   [バッチ ログオン権限](https://learn.microsoft.com/windows/win32/taskschd/security-contexts-for-running-tasks) ·
   [WSL の Session 0 対応履歴](https://github.com/microsoft/WSL/releases/tag/2.0.1)
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
WSL の Unix socket 転送は `-N -L "127.0.0.1:39124:$shared_socket"` で開始し、
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
sudo -u kio-test test -w "$shared_socket"
sudo -l -U kio-test
sudo -u kio-test sudo -n -- /var/lib/kio-v1-local/shared-gpu-count
sudo -u kio-test sudo -n -- /var/lib/kio-v1-local/shared-gpu-count unexpected
```

専用 sshd と `user@<kio-test UID>.service` に root 管理の mount namespace を使う。
両方で `/mnt` は読み取り専用の空 tmpfs、`/init` と `/run/WSL` は不可視とし、
`WSL_INTEROP` を削除する。専用 sshd には `/mnt` 下にある場合だけ設定したソケット 1 個を
bind する。rootless Docker 側には共有 socket を見せない。Desktop が bind 元のソケットを再生成して
接続不能になった場合は照会が失敗する。受入のリースを回収してから専用 sshd の bind を更新する。

setup は両名前空間を検査し、CUDA smoke のコンテナ内でも Engine から見た `/mnt` が空である
ことを検証する。現地では管理者が保護対象 Windows パスの存在を通常の名前空間で確かめ、
専用サービスの両名前空間では `kio-test` が書込・起動できないことを確認する。

```bash
uid=$(id -u kio-test)
probe='/mnt/c/Users/<実在する保護対象の Windows ユーザー>'
sudo test -e "$probe"
for unit in kio-v1-local-sshd.service "user@$uid.service"; do
  pid=$(systemctl show "$unit" -p MainPID --value)
  sudo nsenter --target "$pid" --mount -- runuser -u kio-test -- test -w "$probe"
  sudo nsenter --target "$pid" --mount -- runuser -u kio-test -- test -x /init
done
```

両テストは失敗が期待値。`binfmt_misc` の `F` フラグは `/init` を固定できるため、`/init` の
非表示だけで Windows 実行を防げたと判定しない。`/run/WSL` も遮断する。実際の PE 実行も
確かめる場合は、同じ Windows の `cmd.exe` を管理者が一時的に Linux 側に置き、
両名前空間内の `kio-test` から副作用のない終了コマンドを実行する。

```bash
exe_probe=$(mktemp /tmp/kio-pe-probe.XXXXXXXX)
sudo install -m 0755 /mnt/c/Windows/System32/cmd.exe "$exe_probe"
for unit in kio-v1-local-sshd.service "user@$uid.service"; do
  pid=$(systemctl show "$unit" -p MainPID --value)
  sudo nsenter --target "$pid" --mount -- runuser -u kio-test --     timeout 5 "$exe_probe" /d /c 'echo KIO_INTEROP_PROBE'
done
sudo unlink "$exe_probe"
```

Windows 側の `KIO_INTEROP_PROBE` が出た場合は受入を止める。終了コード 124 の時間切れも
拒否の証拠にはしない。
通常の利用者の interop は個人機と研究室の代替手順で保持される。
Linux カーネルや WSL 自体の隔離、後から追加する別マウント・代理ソケット、管理者が
専用サービスの外から直接起動した同 UID のプロセスまではこの対策の対象外。
Windows ドライブが `/mnt` 外にある状態での適用は止める。マウント変更時には再検証する。
[systemd のファイルシステム名前空間](https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html) ·
[binfmt の固定インタープリター](https://docs.kernel.org/admin-guide/binfmt-misc.html)

初期構築では、候補と manifest が未配置であることによる拒否を 2 段 SSH で確認する。
WSL 側で `maintenance.lock` が root:kio-test の 0640 で存在し、`deployment.json` が未配置であることを
先に確かめる。`id` の拒否だけでは manifest の検査まで到達したことを確認できない。試験用の値で
読み取り専用 `identity` 要求を送り、失敗し、標準エラーに `deployment path is missing` が
含まれることを確認する。

```bash
bootstrap_sha=77016e56d7532401b7c3ad22c79ab4e5849142ad
printf '%064d\n' 0 | ssh -F "$cfg" -o BatchMode=yes kio-lab-wsl \
  "v1 identity $bootstrap_sha 1 1 linux"
```

研究室では競馬サービスの Docker Desktop を維持するため、rm2c の対話セッションを保ったまま
S4U タスクを開始する。対象の専用 Ubuntu が停止している状態からタスクが起動・維持できることを、
タスク状態と別端末からの Windows 経由の SSH 2222 で確認する。対象 Ubuntu を停止して試す場合は、
受入リースがないことを確認し、Ubuntu-26.04 だけを対象とする。サインアウト後の挙動は未確認。
Windows のコンソールでは以下を確認する。

```powershell
Get-ScheduledTask -TaskName KioV1LocalGpuKeepalive |
    Select-Object State, @{N='LogonType';E={$_.Principal.LogonType}}
Get-ScheduledTaskInfo -TaskName KioV1LocalGpuKeepalive | Select-Object LastTaskResult
```

S4U で起動できない場合は、Task Scheduler の Operational ログとバッチ ログオン権限、
ローカル VHDX へのアクセスを確認する。

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
  ソケット設定と専用 user manager の drop-in は退避から復元する。
  元の SSH drop-in を復元後、稼働中の通常 sshd は reload する。新規アカウントを撤去する場合は
  専用データとホームを先に退避する。共有 dockerd の状態は維持する。
  `--terminate-kio-test` で終了したセッションやプロセスは復元の対象外。

## Candidate-bound deployment proposal

初期構築と候補未配置の拒否確認が済んだら、F4 の前に、通常 CI が通った exact `main` 候補から
Linux helper と固定入力を準備する。初期構築用のコミットは F4 の候補を固定しない。

1. クリーンな候補の Linux ビルド環境で、候補からビルドした helper を使って proposal を生成する。
   `F4_CANDIDATE_SHA` に承認済み候補の完全な SHA を、`bundle` にリポジトリ外の一時ディレクトリを
   指定する。canonical manifest は proposal の
   `deployment` オブジェクトだけを、キー順でコンパクトにシリアライズし、末尾改行なしで保存する。

   ```bash
   (
     set -euo pipefail
     candidate=$F4_CANDIDATE_SHA
     test "$(git rev-parse HEAD)" = "$candidate"
     test -z "$(git status --porcelain)"
     cargo +1.98.0 build -p kio-eval --bin kio-acceptance-tools --locked
     target/debug/kio-acceptance-tools route-preflight \
       --repository "$PWD" --candidate "$candidate" \
       --tools-binary target/debug/kio-acceptance-tools > "$bundle/proposal.json"
     jq -cSj -e '.deployment | objects' "$bundle/proposal.json" > "$bundle/deployment.json"
   )
   ```

   Preflight は exact HEAD、Rust 定義の `FIXED_SOURCES` と helper の SHA-256 を検査する。
   candidate、scope、各 digest を独立したレビュー経路で確認する。同じ経路から受け取った hash
   との一致は転送の完全性を示す。候補ビルドの出どころは承認済みの exact `main` とビルド記録で確認する。
2. manifest の `sources` に列挙されたパスと同一の相対パスを保って固定入力を転送し、
   `sources` の SHA-256 と `tools_binary_sha256` の両方を照合する。
   両機で `/var/lib/kio-v1-local/maintenance.lock` を root が排他取得し、
   `/var/lib/kio-v1-local/state/lease.json` が存在しない間に配置する。
   `/var/lib/kio-v1-local/source` の下に固定入力を置き、初期構築用 helper を候補の
   `scripts/v1-local-gpu/kio-acceptance-tools` で置き換える。 `shared-gpu-count` は
   候補の `scripts/v1-local-gpu/host/shared-gpu-count` と実行用
   `/var/lib/kio-v1-local/shared-gpu-count` の両方を照合して配置する。
   実行ファイルは 0755、manifest は 0644。ファイルと全祖先は root 所有、
   group/other 書込不可、非 symlink とする。配置後の digest を再確認し、
   最後に canonical な `deployment.json` を `source/scripts/v1-local-gpu/` に置いてロックを解放する。
   dispatcher は要求された候補 SHA、manifest、実行中の helper、固定入力、実行用照会を照合する。
3. F4 に使うホストを固定し、専用 Engine に固定 OCR image を pull する。

受入直前に空き VRAM が 6,269 MiB 以上で、
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
  -f /var/lib/kio-v1-local/source/scripts/v1-local-gpu/config/compose.yaml pull
```

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
