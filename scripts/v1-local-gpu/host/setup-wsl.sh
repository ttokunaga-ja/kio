#!/usr/bin/env bash
set -Eeuo pipefail
umask 077
export LC_ALL=C

die() { printf 'error: %s\n' "$*" >&2; exit 1; }
terminate_kio_test=false
desktop_interop_required=false
while [[ ${1:-} == --* ]]; do
  case $1 in
    --terminate-kio-test) terminate_kio_test=true ;;
    --desktop-interop-required) desktop_interop_required=true ;;
    *) die "Unknown option: $1" ;;
  esac
  shift
done
[[ $# == 5 && $EUID == 0 ]] ||
  die "Usage: sudo --preserve-env=WSL_DISTRO_NAME bash $0 [--terminate-kio-test] [--desktop-interop-required] DISTRO CI_PUBLIC_KEY_FILE TOOLKIT_BASE_DEB CUDA_SMOKE_IMAGE_DIGEST SHARED_DOCKER_SOCKET"
[[ -n ${WSL_DISTRO_NAME:-} && $WSL_DISTRO_NAME == "$1" ]] || die "WSL distro mismatch."
if $desktop_interop_required && [[ $1 != Ubuntu-26.04 ]]; then
  die "The Desktop interop exception is only for the dedicated Ubuntu-26.04 distro."
fi
if [[ $1 == Ubuntu-26.04 ]] && ! $desktop_interop_required; then
  awk '
    function trim(s) { sub(/^[[:space:]]+/, "", s); sub(/[[:space:]]+$/, "", s); return s }
    /^[[:space:]]*[#;]/ { next }
    /^[[:space:]]*\[/ { interop = ($0 ~ /^[[:space:]]*\[interop\][[:space:]]*$/); next }
    interop && /^[[:space:]]*[Ee][Nn][Aa][Bb][Ll][Ee][Dd][[:space:]]*[:=]/ {
      value = $0
      sub(/^[^:=]*[:=]/, "", value)
      entries++
      disabled = tolower(trim(value)) ~ /^(false|no|off|0)$/
    }
    END { exit !(entries == 1 && disabled) }
  ' /etc/wsl.conf || die "Disable interop in the dedicated distro and relaunch that distro before setup."
fi
[[ $5 == /* ]] || die "Supply the absolute shared Docker socket path."
shared_socket=$(realpath -e -- "$5") || die "The selected shared Docker socket is unavailable."
[[ $shared_socket =~ ^/[A-Za-z0-9_./-]+$ && ${#shared_socket} -lt 108 && -S $shared_socket ]] ||
  die "The selected path must resolve to one Unix socket."
smoke=$4
[[ $smoke =~ ^[a-z0-9./:_-]+@sha256:[a-f0-9]{64}$ ]] || die "Use a reviewed GPU smoke image pinned by digest."
grep -qi microsoft /proc/sys/kernel/osrelease && [[ -c /dev/dxg ]] || die "Expected WSL2 GPU support."
gpu_uuid=$(timeout 10 /usr/lib/wsl/lib/nvidia-smi --query-gpu=uuid --format=csv,noheader)
[[ $gpu_uuid =~ ^GPU-[a-fA-F0-9-]+$ ]] || die "Expected exactly one WSL GPU."
# shellcheck source=/dev/null
. /etc/os-release
[[ $ID == ubuntu && ( $VERSION_ID == 24.04 || $VERSION_ID == 26.04 ) ]] || die "Expected Ubuntu 24.04 or 26.04."
[[ $(ps -p 1 -o comm=) == systemd ]] || die "Enable systemd for this distro first."
for tool in /usr/sbin/visudo /usr/bin/sudo /usr/bin/flock /usr/sbin/sshd /usr/bin/ssh-keygen /usr/bin/rootlesskit /usr/bin/slirp4netns /usr/bin/newuidmap /usr/bin/newgidmap /usr/bin/getsubids /usr/bin/jq /usr/bin/setfacl; do
  [[ -x $tool ]] || die "Install prerequisite $tool before applying host configuration."
done
[[ -x /usr/sbin/iptables ]] || die "Install the iptables package before applying host configuration."

user=kio-test
homes=/var/lib/kio-v1-local
state=$homes/state
home=/home/$user
source=$homes/source
tools=/opt/kio-v1-local
engine=$tools/docker
toolkit=$tools/toolkit
cdi_dir=$tools/wsl-cdi
client=$tools/docker-cli
ssh_unit=kio-v1-local-sshd.service
docker_unit=kio-v1-local-docker.service
ssh_config=/etc/ssh/sshd_config_kio_v1_local
keys=/etc/ssh/kio-v1-local-authorized-keys
ssh_service=/etc/systemd/system/$ssh_unit
docker_service=/etc/systemd/user/$docker_unit
dockerd_config=/etc/kio-v1-local-dockerd.json
host_key=/etc/ssh/ssh_host_ed25519_key
legacy_config=/etc/ssh/sshd_config
legacy_deny=/etc/ssh/sshd_config.d/00-kio-v1-local-deny.conf
query=$homes/shared-gpu-count
query_source=$source/scripts/v1-local-gpu/host/shared-gpu-count
query_rule=/etc/sudoers.d/kio-v1-local-shared-gpu
query_config=/etc/kio-v1-local-shared-gpu-socket
maintenance=$homes/maintenance.lock
legacy_keys=$home/.ssh/authorized_keys
readonly rootless_path="$engine:/usr/sbin:/usr/bin:/sbin:/bin"

tmp=$(mktemp -d)
maintenance_next=
cdi_swap=
on_exit() {
  if [[ -n $maintenance_next && -e $maintenance_next ]]; then unlink "$maintenance_next"; fi
  if [[ -n $cdi_swap ]]; then rm -r -- "$cdi_swap"; fi
  rm -r -- "$tmp"
}
trap on_exit EXIT
: > "$tmp/shared-bind-sources"
echo 'Existing SSH services and listeners (retained):'
systemctl show ssh.service ssh.socket -p Id -p UnitFileState -p ActiveState
ss -ltnp
echo 'Shared Docker socket and service (inventory only):'
if ss -H -ltnp | grep -F '("dockerd",' ; then
  die "An existing Docker daemon listens on TCP. Review its access before applying."
fi
if [[ $1 == Ubuntu ]]; then
  [[ $shared_socket == /run/docker.sock ]] || die "Use the personal host's native /run/docker.sock."
  systemctl is-active --quiet docker.service || die "The existing native Engine must already be running."
fi
# Owners and modes of every component matter, including the candidate payload.
trusted_path() {
  local path=$1 mode
  while [[ $path != / ]]; do
    [[ ! -L $path && $(stat -c %u "$path") == 0 ]] || die "Expected root-owned path: $path"
    mode=$(stat -c %a "$path")
    (( (8#$mode & 022) == 0 )) || die "Writable by non-root: $path"
    path=${path%/*}
    path=${path:-/}
  done
}
[[ $(stat -c %u "$shared_socket") == 0 ]] || die "The shared Docker socket must be root-owned."
socket_mode=$(stat -c %a "$shared_socket")
(( (8#$socket_mode & 002) == 0 )) || die "The shared Docker socket must not be world-writable."
trusted_path "$(dirname -- "$shared_socket")"
stat -Lc '%A %U:%G %n' "$shared_socket"
shared_get() {
  curl -q --max-time 10 --silent --show-error --fail --noproxy '*' \
    --unix-socket "$shared_socket" "http://localhost$1"
}
shared_info=$(shared_get /info)
shared_id_filter='.ID | select(type == "string" and length > 0)'
before_id=$(jq -er "$shared_id_filter" <<< "$shared_info") || die "Cannot identify the selected shared Engine."
shared_os=$(jq -r '.OperatingSystem // ""' <<< "$shared_info")
printf 'Shared Engine: %s %s\n' "$before_id" "$shared_os"
if [[ ( $1 == Ubuntu-26.04 && $shared_os != 'Docker Desktop' ) ||
      ( $1 != Ubuntu-26.04 && $shared_os == 'Docker Desktop' ) ]]; then
  die "The selected shared Engine does not match this host."
fi
shared_containers=$(shared_get '/containers/json?all=1')
jq -er '"Shared containers: \(length)\nRunning: \([.[] | select(.State == "running")] | length)"' <<< "$shared_containers"
jq -j '[.[] | .Mounts[]? | select(.Type == "bind") | .Source] | unique[] | ., "\u0000"' \
  <<< "$shared_containers" > "$tmp/shared-bind-sources"
printf '%s\n' "$shared_socket" > "$tmp/query-config"

mount_helper=$(dirname -- "${BASH_SOURCE[0]}")/wsl-windows-mounts.sh
# shellcheck source=/dev/null
source "$mount_helper"
# Send the verified helper contents into namespaces that may hide its original path.
mount_probe=$(cat -- "$mount_helper")$'\n"$@"'
read_mount_plan() { nsenter --target 1 --mount -- /bin/bash -euo pipefail -c "$mount_probe" -- wsl_mount_plan /proc/self/mountinfo; }
mount_plan=$(read_mount_plan)
caller_mount_plan=$(wsl_mount_plan /proc/self/mountinfo)
printf 'Windows filesystem mounts:\n%s\n' "$mount_plan"
wsl_mount_policy "$mount_plan" > "$tmp/windows-policy"
for kind in directory file; do
  wsl_mount_paths "$mount_plan" "$kind" > "$tmp/windows-$kind"
done
mapfile -d '' -t windows_dirs < "$tmp/windows-directory"
mapfile -d '' -t windows_files < "$tmp/windows-file"
masked_path() { wsl_mount_covers "$1" "${windows_dirs[@]}" "${windows_files[@]}"; }

[[ -e $host_key && -f $host_key.pub && ! -L $host_key ]] || die "Prepare the distro Ed25519 host key first."

public_key=$(tr -d '\r' < "$2")
[[ $public_key != *$'\n'* ]] || die "Expected one public key."
read -r key_type key_blob _ <<< "$public_key"
[[ $key_type == ssh-ed25519 && $key_blob =~ ^[A-Za-z0-9+/]{68}$ ]] || die "Expected Ed25519 public key."
printf 'ssh-ed25519 %s kio-v1-local-acceptance\n' "$key_blob" > "$tmp/keys"
ssh-keygen -l -f "$tmp/keys"
[[ $(dpkg-deb -f "$3" Package) == nvidia-container-toolkit-base &&
   $(dpkg-deb -f "$3" Architecture) == "$(dpkg --print-architecture)" ]] ||
  die "Use the reviewed NVIDIA Container Toolkit base package for this architecture."
dpkg --compare-versions "$(dpkg-deb -f "$3" Version)" ge 1.20.0 ||
  die "The base package must include the standalone CDI hook."
dpkg-deb -x "$3" "$tmp/nvidia"
for tool in nvidia-ctk nvidia-cdi-hook; do
  [[ -x $tmp/nvidia/usr/bin/$tool ]] || die "Missing NVIDIA CDI tool $tool."
done

trusted_path "$source"
trusted_path "$legacy_config"
trusted_path /etc/ssh/sshd_config.d
trusted_path /etc/sudoers.d


check_lease() {
  [[ ! -e $state/lease.json && ! -L $state/lease.json ]] ||
    die "An acceptance lease exists. Finish or recover it before applying setup."
}
[[ ! -L $state ]] || die "Unexpected symlink at the acceptance state directory."
check_lease
if [[ ! -e $maintenance && ! -L $maintenance ]] && systemctl is-active --quiet "$ssh_unit"; then
  die "Drain and take the pre-lock dedicated SSH endpoint offline before migration."
fi
take_maintenance_gate() {
  trusted_path "$maintenance"
  [[ -f $maintenance ]] || die "Expected a regular maintenance lock file."
  exec 9<"$maintenance"
  flock -n -x 9 || die "Acceptance dispatcher or another setup is active."
  [[ $(stat -c '%d:%i' "$maintenance") == "$(stat -Lc '%d:%i' /proc/self/fd/9)" ]] ||
    die "The maintenance lock changed while opening it. Retry."
  check_lease
}
restrict_maintenance_file() {
  # An inherited ACL could otherwise become readable when chmod enables group read.
  /usr/bin/setfacl -b -- "$1"
  chgrp "$user" "$1"
  chmod 0640 "$1"
}
publish_maintenance_gate() {
  local metadata
  metadata=$(stat -c '%g:%a' "$maintenance")
  if [[ $metadata == "$gid:640" ]]; then return; fi
  if [[ $metadata == *:600 ]]; then
    # Bootstrap has never been accessible to other local users.
    restrict_maintenance_file "$maintenance"
    return
  fi
  if [[ $metadata != *:644 ]]; then
    printf 'Unexpected maintenance lock permissions: %s\n' "$metadata" >&2
    return 1
  fi
  # A descriptor to the former public inode could survive chmod. Lock the
  # replacement before publishing it, while still holding the old inode.
  maintenance_next=$(mktemp "$homes/.maintenance.XXXXXXXX")
  restrict_maintenance_file "$maintenance_next"
  exec 8<"$maintenance_next"
  flock -n -x 8
  mv -T -- "$maintenance_next" "$maintenance"
  maintenance_next=
  exec 9<&8
  exec 8<&-
}
trusted_path "$engine"
trusted_path "$client/cli-plugins/docker-compose"
for tool in docker dockerd dockerd-rootless.sh containerd runc; do
  trusted_path "$engine/$tool"
  [[ -x $engine/$tool ]] || die "Missing rootless Docker bundle member: $tool."
done
# Begin bootstrap payload validation.
for executable in "$source/scripts/v1-local-gpu/kio-acceptance-tools" "$query_source"; do
  [[ -f $executable && -x $executable ]] || die "Missing reviewed bootstrap executable: $executable"
  trusted_path "$executable"
done
# End bootstrap payload validation.
if [[ -e $toolkit || -L $toolkit ]]; then trusted_path "$toolkit"; fi

account_sessions() {
  loginctl list-sessions --no-legend --no-pager |
    awk -v id="$uid" '$2 == id {print "session", $1}'
}
account_activity() {
  account_sessions &&
  ps -eo ruid=,euid=,pid=,tty=,comm= |
    awk -v id="$uid" '$1 == id || $2 == id {print "process", $3, $4, $5}'
}
signal_account() {
  local signal=$1 selection status
  for selection in -u -U; do
    status=0
    pkill "-$signal" "$selection" "$uid" || status=$?
    if (( status > 1 )); then return "$status"; fi
  done
}
wait_account_idle() {
  local activity attempt
  for ((attempt=0; attempt<=$1; attempt++)); do
    activity=$(account_activity) || return 2
    if [[ -z $activity ]]; then return 0; fi
    if (( attempt < $1 )); then sleep 1; fi
  done
  printf 'Remaining kio-test activity:\n%s\n' "$activity" >&2
  return 1
}
drain_account() {
  local activity sessions status=0
  activity=$(account_activity) || return 1
  if [[ -z $activity ]]; then return 0; fi
  printf 'Existing kio-test activity:\n%s\n' "$activity" >&2
  if ! $terminate_kio_test; then
    printf 'Setup refused: kio-test is active. Use --terminate-kio-test to end its sessions and processes.\n' >&2
    return 1
  fi
  managed=true
  if ! loginctl terminate-user "$user"; then
    sessions=$(account_sessions) || return 1
    if [[ -n $sessions ]]; then return 1; fi
  fi
  signal_account TERM
  wait_account_idle 10 || status=$?
  if (( status == 0 )); then return 0; fi
  if (( status != 1 )); then return "$status"; fi
  signal_account KILL
  wait_account_idle 5
}

have_account=false
if getent passwd "$user" > "$tmp/account"; then
  have_account=true
  IFS=: read -r _ _ uid gid _ existing_home old_shell < "$tmp/account"
  [[ $uid =~ ^[1-9][0-9]*$ && $gid =~ ^[1-9][0-9]*$ &&
     $existing_home == "$home" && ! -L $home && $(id -G "$user") == "$gid" &&
     $(id -gn "$user") == "$user" ]] || die "kio-test needs its dedicated home and private group only."
  [[ $(getent group "$user" | cut -d: -f4) =~ ^(kio-test)?$ &&
     $(getent passwd | awk -F: -v uid="$uid" -v gid="$gid" '$3 == uid || $4 == gid {print $1}') == "$user" ]] ||
    die "The kio-test UID and primary group must belong only to kio-test."
  password_before=$(/usr/bin/passwd -S "$user" | awk '{print $2}')
  password_age=$(getent shadow "$user" | cut -d: -f3)
  [[ $password_age =~ ^[0-9]*$ ]] || die "Cannot classify the password change date."
  [[ $password_before =~ ^(L|P|NP)$ ]] || die "Cannot classify existing kio-test password state."
  activity=$(account_activity) || die "Cannot inspect kio-test sessions and processes."
  if [[ -n $activity ]] && ! $terminate_kio_test; then
    printf 'Existing kio-test activity:\n%s\n' "$activity" >&2
    die "kio-test is active; supply --terminate-kio-test to end its sessions and processes."
  fi
  if runuser -u "$user" -- test -w "$shared_socket"; then
    die "kio-test can write the shared Docker socket. Revoke its access and end old sessions before setup."
  fi
else
  [[ ! -e $home && ! -L $home && ! -e $state && ! -L $state ]] || die "Unexpected files for a new kio-test account."
  if getent group "$user" >/dev/null; then die "Unexpected preexisting kio-test group."; fi
fi
if [[ -e $state || -L $state ]]; then
  [[ ! -L $state && -d $state && $(stat -c '%u:%a' "$state") == "$uid:700" ]] ||
    die "Expected an owner-private state directory."
fi
if [[ -n $(ss -H -ltn 'sport = :2222') ]]; then
  main_pid=$(systemctl show "$ssh_unit" -p MainPID --value)
  [[ $main_pid != 0 && $(ss -H -ltn 'sport = :2222' | awk '{print $4}') == 127.0.0.1:2222 &&
     $(ss -H -ltnp 'sport = :2222') == *"pid=$main_pid,"* ]] ||
    die "TCP 2222 is occupied by another listener. Review existing services separately."
fi
[[ -z $(ss -H -ltn 'sport = :18443') ]] || die "TCP 18443 is already in use."

cat > "$tmp/sshd" <<CONFIG
Port 2222
ListenAddress 127.0.0.1
HostKey $host_key
AllowUsers $user
AuthenticationMethods publickey
PubkeyAuthentication yes
PasswordAuthentication no
KbdInteractiveAuthentication no
UsePAM yes
PermitUserEnvironment no
PermitUserRC no
PermitTTY no
ForceCommand $source/scripts/v1-local-gpu/kio-acceptance-tools dispatch
AuthorizedKeysFile $keys
AllowTcpForwarding local
PermitOpen 127.0.0.1:18443
AllowStreamLocalForwarding no
AllowAgentForwarding no
X11Forwarding no
PermitTunnel no
GatewayPorts no
CONFIG
# A missing /run/sshd only prevents privilege separation at execution time.
# -T under the invoking root also requires it on some distro builds.
[[ -d /run/sshd ]] || die "Prepare root-owned /run/sshd (0755) before setup."
trusted_path /run/sshd
/usr/sbin/sshd -t -f "$tmp/sshd"
# Validate the distro include and refuse unmanaged sshd listeners.
first_directive=$(awk 'NF && $1 !~ /^#/ {print tolower($1), $2; exit}' "$legacy_config")
[[ $first_directive == 'include /etc/ssh/sshd_config.d/*.conf' &&
   $(awk 'tolower($1) ~ /^include(=|$)/ {count++} END {print count+0}' "$legacy_config") == 1 ]] ||
  die "Expected the distro SSH drop-in include before other directives."
if grep -Eiq '^[[:space:]]*Match([[:space:]=]|$)' "$legacy_config"; then
  die "Review Match rules in the existing sshd before adding the global denial."
fi
for snippet in /etc/ssh/sshd_config.d/*.conf; do
  [[ -e $snippet ]] || continue
  trusted_path "$snippet"
  if grep -Eiq '^[[:space:]]*(Match|Include)([[:space:]=]|$)' "$snippet"; then
    die "Review Match or nested Include in the existing SSH drop-in: $snippet"
  fi
done
[[ $(systemctl show ssh.service -p Environment --value) != *SSHD_OPTS=* ]] ||
  die "Existing ssh.service overrides SSHD_OPTS."
legacy_exec=$(systemctl show ssh.service -p ExecStart --value)
[[ $legacy_exec == *"argv[]=/usr/sbin/sshd -D \$SSHD_OPTS ;"* ||
   $legacy_exec == *'argv[]=/usr/sbin/sshd -D ;'* ]] ||
  die "Existing ssh.service uses an unrecognized launch configuration."
if [[ -f /etc/default/ssh ]] &&
   ! awk '
     /^[[:space:]]*SSHD_OPTS[[:space:]]*=/ {
       value = substr($0, index($0, "=") + 1)
       sub(/[[:space:]]+#.*/, "", value)
       gsub(/^[[:space:]]+|[[:space:]]+$/, "", value)
       if (value != "" && value != "\"\"" && value != "\047\047") exit 1
     }
   ' /etc/default/ssh; then
  die "Existing ssh.service has nondefault SSHD_OPTS."
fi
legacy_pid=$(systemctl show ssh.service -p MainPID --value)
dedicated_pid=$(systemctl show "$ssh_unit" -p MainPID --value)
while IFS= read -r line; do
  while [[ $line =~ \"sshd\",pid=([0-9]+), ]]; do
    pid=${BASH_REMATCH[1]}
    [[ $pid == "$legacy_pid" || $pid == "$dedicated_pid" ]] ||
      die "Unmanaged sshd listener (PID $pid). Review its login policy before applying."
    line=${line#*"${BASH_REMATCH[0]}"}
  done
done < <(ss -H -ltnp)
printf 'DenyUsers %s\n' "$user" > "$tmp/legacy-deny"
{ printf 'Include %s\n' "$tmp/legacy-deny"; cat "$legacy_config"; } > "$tmp/legacy-sshd"
test_legacy_deny() {
  /usr/sbin/sshd -t -f "$1" &&
  /usr/sbin/sshd -T -f "$1" -C "user=$user,host=localhost,addr=127.0.0.1" |
    awk 'tolower($1) == "denyusers" {for (i=2; i<=NF; i++) if ($i == "kio-test") found=1} END {exit !found}'
}
test_legacy_deny "$tmp/legacy-sshd" || die "DenyUsers did not take effect."
printf '%s ALL=(root) NOPASSWD:NOSETENV: %s ""\n' "$user" "$query" > "$tmp/query-sudoers"
grep -Eq '^(@|#)includedir[[:space:]]+/etc/sudoers.d/?[[:space:]]*$' /etc/sudoers ||
  die "Expected the distro sudoers.d include."
/usr/sbin/visudo -cf "$tmp/query-sudoers" >/dev/null
# Root system units create these mount namespaces before running the kio-test
# session or its user manager; the normal distro user keeps the host view.
cat > "$tmp/interop-policy" <<'UNIT'
InaccessiblePaths=-/init -/run/WSL
UnsetEnvironment=WSL_INTEROP
UNIT
cat "$tmp/windows-policy" >> "$tmp/interop-policy"
{
  printf '[Service]\n'
  cat "$tmp/interop-policy"
  if ! masked_path "$shared_socket"; then
    printf 'InaccessiblePaths=-%s\n' "$shared_socket"
  fi
} > "$tmp/user-interop"
ssh_socket_bind=
if masked_path "$shared_socket"; then
  # Expose only the configured socket inside the SSH namespace.
  ssh_socket_bind="BindReadOnlyPaths=$shared_socket"
fi
interop_policy=$(cat "$tmp/interop-policy")
cat > "$tmp/ssh-unit" <<UNIT
[Unit]
Description=Kio v1 local GPU SSH
After=network.target

[Service]
$interop_policy
$ssh_socket_bind
UMask=0022
ExecStartPre=/usr/bin/mkdir -p /run/sshd
ExecStartPre=/usr/sbin/sshd -t -f $ssh_config
ExecStart=/usr/sbin/sshd -D -f $ssh_config
Restart=on-failure

[Install]
WantedBy=multi-user.target
UNIT
cat > "$tmp/dockerd" <<CONFIG
{"data-root":"$state/docker","features":{"cdi":true}}
CONFIG
# Validate with the unit's PATH so dockerd finds the bundled docker-proxy.
PATH=$rootless_path "$engine/dockerd" --validate --config-file "$tmp/dockerd"
"$tmp/nvidia/usr/bin/nvidia-ctk" cdi generate --help | grep -F -- --nvidia-cdi-hook-path >/dev/null ||
  die "Toolkit lacks the CDI hook path option."
# CDI discovers files in WSL's driver store, which can itself be a 9p mount.
# Stage only the driver's referenced files on Linux before hiding the store.
mkdir -m 0755 "$tmp/cdi"
nsenter --target 1 --mount -- "$tmp/nvidia/usr/bin/nvidia-ctk" cdi generate --format json \
  --nvidia-cdi-hook-path="$toolkit/nvidia-cdi-hook" --output="$tmp/nvidia-cdi.json"
nsenter --target 1 --mount -- /bin/bash -euo pipefail -c "$mount_probe" -- wsl_stage_cdi \
  "$mount_plan" "$tmp/nvidia-cdi.json" "$tmp/cdi" "$cdi_dir"
cat > "$tmp/docker-unit" <<UNIT
[Unit]
Description=Kio v1 local rootless Docker

[Service]
Environment=PATH=$rootless_path
Environment=DOCKERD=$engine/dockerd
Environment=DOCKERD_ROOTLESS_ROOTLESSKIT_NET=slirp4netns
Environment=DOCKERD_ROOTLESS_ROOTLESSKIT_DISABLE_HOST_LOOPBACK=true
Environment=DOCKERD_ROOTLESS_ROOTLESSKIT_STATE_DIR=%t/kio-v1-local-rootlesskit
UnsetEnvironment=DOCKERD_ROOTLESS_ROOTLESSKIT_FLAGS _DOCKERD_ROOTLESS_CHILD
ExecStart=$engine/dockerd-rootless.sh --config-file=$dockerd_config --host=unix://%t/kio-v1-local-docker.sock --cdi-spec-dir=$cdi_dir
Delegate=yes
Restart=on-failure

[Install]
WantedBy=default.target
UNIT

backup=$(mktemp -d /var/backups/kio-v1-local.XXXXXXXX)
printf 'Backup: %s\n' "$backup"
printf '%s\n' "$mount_plan" > "$backup/windows-mounts"
if [[ -e $cdi_dir || -L $cdi_dir ]]; then
  trusted_path "$cdi_dir"
  [[ -d $cdi_dir ]] || die "Expected a dedicated CDI directory: $cdi_dir"
  cp -a -- "$cdi_dir" "$backup/wsl-cdi"
else
  touch "$backup/wsl-cdi-absent"
fi
paths=("$legacy_deny" "$query" "$query_rule" "$query_config" "$ssh_config" "$keys" "$ssh_service" "$docker_service" "$dockerd_config" "$toolkit/nvidia-ctk" "$toolkit/nvidia-cdi-hook")
manager_policy_dir=
manager_policy_created_dir=false
manager_policy_changed=false
if $have_account; then
  manager_policy_dir=/etc/systemd/system/user@$uid.service.d
  paths+=("$manager_policy_dir/50-kio-v1-local-interop.conf")
fi
[[ -d $toolkit ]] || touch "$backup/new-toolkit"
for path in "${paths[@]}"; do
  [[ ! -L $path ]] || die "Unexpected symlink at $path."
  if [[ -e $path ]]; then
    trusted_path "$path"
    cp -a --parents "$path" "$backup"
  else
    printf '%s\n' "$path" >> "$backup/absent"
  fi
done
cp "$tmp"/account "$backup/account"
printf '%s\n' "${password_before:-new}" > "$backup/password-state"
printf '%s\n' "${password_age:-}" > "$backup/password-last-change"
for table in /etc/subuid /etc/subgid; do
  if [[ -f $table ]]; then awk -F: -v principal="$user" '$1 == principal' "$table" > "$backup/${table##*/}"; fi
done
systemctl is-enabled "$ssh_unit" > "$backup/ssh-enabled" 2>/dev/null || true
systemctl is-active "$ssh_unit" > "$backup/ssh-active" 2>/dev/null || true
case $(cat "$backup/ssh-enabled") in ''|not-found|disabled|enabled) ;; *) die "Unexpected dedicated SSH unit state." ;; esac
case $(cat "$backup/ssh-active") in inactive|active) ;; *) die "Dedicated SSH is transitioning." ;; esac

userctl() { runuser -u "$user" -- env -i HOME="$home" PATH=/usr/bin:/bin XDG_RUNTIME_DIR="/run/user/$uid" systemctl --user "$@"; }
as_user() { runuser -u "$user" -- env -i HOME="$home" PATH="$rootless_path" XDG_RUNTIME_DIR="/run/user/$uid" "$@"; }
manager_before=false
linger_before=false
if $have_account; then
  case $(systemctl show "user@$uid.service" -p ActiveState --value) in
    active) manager_before=true ;;
    inactive) ;;
    *) die "The kio-test user manager is not in a stable running/stopped state." ;;
  esac
fi
if [[ -e /var/lib/systemd/linger/$user ]]; then linger_before=true; fi
printf '%s\n' "$manager_before" > "$backup/user-manager"
printf '%s\n' "$linger_before" > "$backup/linger"
# Offline unit enablement is a symlink even when the user manager is down.
enabled_link="$home/.config/systemd/user/default.target.wants/$docker_unit"
docker_enabled=false
if $have_account && as_user test -L "$enabled_link"; then
  [[ $(as_user readlink "$enabled_link") == "$docker_service" ]] || die "Unexpected Docker enablement link."
  docker_enabled=true
fi
docker_active=false
if $manager_before; then
  case $(userctl show "$docker_unit" -p ActiveState --value) in
    active) docker_active=true ;;
    inactive) ;;
    *) die "Dedicated Docker unit is not in a stable running/stopped state." ;;
  esac
fi
printf '%s\n' "$docker_enabled" > "$backup/docker-enabled"
printf '%s\n' "$docker_active" > "$backup/docker-active"
[[ -e $state ]] || touch "$backup/new-state"

created=false
managed=false
ssh_changed=false
shell_changed=false
smoke_added=false
legacy_changed=false
password_changed=false
maintenance_published=false
cdi_changed=false
saved_keys=
changed=()
restore_path() {
  local path=$1
  if [[ -e $backup$path ]]; then
    cp -a --remove-destination "$backup$path" "$path"
  elif [[ -e $path ]]; then
    unlink "$path"
  fi
}
rollback() {
  # ERR is inherited by command substitutions; only the applying process can restore.
  if (( BASH_SUBSHELL > 0 )); then exit 1; fi
  trap - ERR INT TERM
  set +e
  local failed=0 path
  if $managed && systemctl is-active --quiet "user@$uid.service"; then
    if $smoke_added && [[ ! -f $backup/new-state ]]; then
      as_user "${docker[@]}" image rm "$smoke" || failed=1
    fi
    userctl stop "$docker_unit" || failed=1
    if ! $docker_enabled; then userctl disable "$docker_unit" || failed=1; fi
    systemctl stop "user@$uid.service" || failed=1
  fi
  if $cdi_changed; then
    if [[ -d $cdi_dir ]]; then mv -T -- "$cdi_dir" "$cdi_swap/rejected" || failed=1; fi
    if [[ -d $cdi_swap/previous ]]; then mv -T -- "$cdi_swap/previous" "$cdi_dir" || failed=1; fi
  fi
  if $ssh_changed; then
    systemctl stop "$ssh_unit" || failed=1
    if [[ $(cat "$backup/ssh-enabled") != enabled ]]; then systemctl disable "$ssh_unit" || failed=1; fi
  fi
  for path in "${changed[@]}"; do restore_path "$path" || failed=1; done
  if [[ -f $backup/new-toolkit && -d $toolkit ]]; then rmdir "$toolkit" || failed=1; fi
  if $manager_policy_created_dir; then rmdir "$manager_policy_dir" || failed=1; fi
  if $ssh_changed || $manager_policy_changed; then systemctl daemon-reload || failed=1; fi
  if $ssh_changed && [[ $(cat "$backup/ssh-active") == active ]]; then systemctl start "$ssh_unit" || failed=1; fi
  if $managed; then
    if $manager_before; then systemctl start "user@$uid.service" || failed=1; fi
    if systemctl is-active --quiet "user@$uid.service"; then
      userctl daemon-reload || failed=1
      if $docker_active; then userctl start "$docker_unit" || failed=1; fi
    fi
    if ! $manager_before; then systemctl stop "user@$uid.service" || failed=1; fi
    if ! $linger_before; then loginctl disable-linger "$user" || failed=1; fi
  fi
  if [[ -n $saved_keys ]] && { as_user test -e "$saved_keys" || as_user test -L "$saved_keys"; }; then
    if as_user test -e "$legacy_keys" || as_user test -L "$legacy_keys"; then
      printf 'Refusing to overwrite a newly created authorized_keys; original at %s\n' "$saved_keys" >&2
      failed=1
    else
      as_user mv -nT -- "$saved_keys" "$legacy_keys" || failed=1
      if as_user test -e "$saved_keys" || as_user test -L "$saved_keys"; then failed=1; fi
    fi
  fi
  if $password_changed; then
    if [[ $password_before == NP ]]; then
      /usr/bin/passwd -d "$user" || failed=1
    else
      /usr/bin/passwd -u "$user" || failed=1
    fi
    /usr/bin/chage -d "${password_age:--1}" "$user" || failed=1
  fi
  if $legacy_changed && systemctl is-active --quiet ssh.service; then
    systemctl reload ssh.service || failed=1
  fi
  if [[ -f $backup/new-state && -d $state ]]; then mv -- "$state" "$backup/retired-state" || failed=1; fi
  if $shell_changed; then usermod -s "$old_shell" "$user" || failed=1; fi
  if $created; then
    # Preserve new home contents in the root-only backup before removing account metadata.
    if [[ -d $home ]]; then mv -- "$home" "$backup/retired-home" || failed=1; fi
    if $maintenance_published; then
      { chmod 0600 "$maintenance" && chgrp 0 "$maintenance"; } || failed=1
    fi
    userdel "$user" || failed=1
  fi
  printf 'Apply failed; rollback status=%s; backup=%s\n' "$failed" "$backup" >&2
  exit 1
}
check_subids() {
  getsubids "$user" | awk '$NF >= 65536 {found=1} END {exit !found}' &&
  getsubids -g "$user" | awk '$NF >= 65536 {found=1} END {exit !found}'
}
test_shared_access() {
  local path resolved
  if as_user test -w "$shared_socket"; then
    printf '%s\n' 'kio-test can access the shared Docker socket.' >&2
    return 1
  fi
  for path in /var/lib/docker /var/lib/containerd; do
    if [[ -d $path ]] && as_user test -r "$path" && as_user test -x "$path"; then
      printf 'kio-test can read shared data: %s\n' "$path" >&2
      return 1
    fi
  done
  while IFS= read -r -d '' path; do
    # Covered shared bind sources are checked in the dedicated namespaces below.
    if resolved=$(realpath -e -- "$path") && masked_path "$resolved"; then continue; fi
    if as_user test -r "$path" || as_user test -w "$path"; then
      printf 'kio-test can access a shared Docker bind mount: %s\n' "$path" >&2
      return 1
    fi
  done < "$tmp/shared-bind-sources"
}
if $have_account; then
  test_shared_access || die "Revoke shared access before setup."
  check_subids || die "kio-test needs at least 65536 subordinate UIDs and GIDs."
  if as_user test -e "$home/.config/systemd/user/$docker_unit" ||
     as_user test -e "$home/.config/systemd/user/$docker_unit.d"; then
    die "Unexpected override of the dedicated Docker unit."
  fi
fi
apply_file() {
  local source_file=$1 mode=$2 destination=$3
  changed+=("$destination")
  install -m "$mode" "$source_file" "$destination"
}
# Acquire the gate before termination, even on an explicitly authorized rerun.
if [[ ! -e $maintenance && ! -L $maintenance ]]; then
  (set -o noclobber; : > "$maintenance") 2>/dev/null || true
fi
take_maintenance_gate
trap rollback ERR INT TERM
if $have_account; then drain_account; fi
if ! $have_account; then
  useradd -m -U -s /bin/sh "$user"
  created=true
  touch "$backup/created-account"
  check_subids
fi
uid=$(id -u "$user")
gid=$(id -g "$user")
publish_maintenance_gate
maintenance_published=true
test_shared_access
legacy_changed=true
apply_file "$tmp/legacy-deny" 0644 "$legacy_deny"
test_legacy_deny "$legacy_config"
if systemctl is-active --quiet ssh.service; then systemctl reload ssh.service; fi
if $have_account && [[ $password_before != L ]]; then
  /usr/bin/passwd -l "$user"
  password_changed=true
fi
[[ $(/usr/bin/passwd -S "$user" | awk '{print $2}') == L ]]
# Recheck after closing the legacy login path; it was still open at preflight.
drain_account
if as_user test -e "$legacy_keys" || as_user test -L "$legacy_keys"; then
  archive=$(as_user mktemp -d "$home/.ssh/kio-disabled.XXXXXXXX")
  saved_keys=$archive/authorized_keys
  printf '%s\n' "$saved_keys" > "$backup/authorized-keys-location"
  as_user mv -T -- "$legacy_keys" "$saved_keys"
fi
apply_file "$tmp/query-config" 0600 "$query_config"
apply_file "$query_source" 0755 "$query"
apply_file "$tmp/query-sudoers" 0440 "$query_rule"
/usr/sbin/visudo -cf /etc/sudoers >/dev/null
if /usr/bin/sudo -l -U "$user" -- "$query" unexpected >/dev/null 2>&1; then
  printf 'Unexpected sudo permission for query arguments.\n' >&2
  rollback
fi
shared_count=$(as_user /usr/bin/sudo -n -- "$query")
[[ $shared_count =~ ^(0|[1-9][0-9]*)$ ]]
if $have_account && [[ $old_shell != /bin/sh ]]; then
  usermod -s /bin/sh "$user"
  shell_changed=true
fi
if [[ -f $backup/new-state ]]; then install -d -m 0700 -o "$user" -g "$user" "$state"; fi
[[ -d $toolkit ]] || install -d -m 0755 "$toolkit"
apply_file "$tmp/nvidia/usr/bin/nvidia-ctk" 0755 "$toolkit/nvidia-ctk"
apply_file "$tmp/nvidia/usr/bin/nvidia-cdi-hook" 0755 "$toolkit/nvidia-cdi-hook"
apply_file "$tmp/dockerd" 0644 "$dockerd_config"
trusted_path "$tools"
cdi_swap=$(mktemp -d "$tools/.wsl-cdi.XXXXXXXX")
cp -a -- "$tmp/cdi" "$cdi_swap/next"
if [[ -d $cdi_dir ]]; then mv -T -- "$cdi_dir" "$cdi_swap/previous"; fi
cdi_changed=true
mv -T -- "$cdi_swap/next" "$cdi_dir"
apply_file "$tmp/docker-unit" 0644 "$docker_service"
if ! $have_account; then
  manager_policy_dir=/etc/systemd/system/user@$uid.service.d
  [[ ! -e $manager_policy_dir && ! -L $manager_policy_dir ]]
  printf '%s\n' "$manager_policy_dir/50-kio-v1-local-interop.conf" >> "$backup/absent"
fi
if [[ -e $manager_policy_dir || -L $manager_policy_dir ]]; then
  trusted_path "$manager_policy_dir"
else
  install -d -m 0755 "$manager_policy_dir"
  manager_policy_created_dir=true
fi
manager_policy_changed=true
apply_file "$tmp/user-interop" 0644 "$manager_policy_dir/50-kio-v1-local-interop.conf"
systemctl daemon-reload
managed=true
loginctl enable-linger "$user"
systemctl start "user@$uid.service"
userctl daemon-reload
[[ $(userctl show "$docker_unit" -p FragmentPath --value) == "$docker_service" ]]
[[ -z $(userctl show "$docker_unit" -p DropInPaths --value) ]]
userctl enable "$docker_unit"
# Always reload the dedicated processes so an unchanged file cannot hide stale runtime state.
userctl restart "$docker_unit"
docker=("$engine/docker" --host "unix:///run/user/$uid/kio-v1-local-docker.sock" --config "$client")
for _ in {1..30}; do
  if security=$(as_user "${docker[@]}" info --format '{{json .SecurityOptions}}' 2>/dev/null); then break; fi
  sleep 1
done
[[ ${security:-} == *'"name=rootless"'* ]]
[[ $(as_user "${docker[@]}" info --format '{{.DockerRootDir}}') == "$state/docker" ]]
as_user "${docker[@]}" compose version
if ! as_user "${docker[@]}" image inspect "$smoke" >/dev/null 2>&1; then
  as_user "${docker[@]}" pull "$smoke"
  smoke_added=true
fi
shared_count=$(as_user /usr/bin/sudo -n -- "$query")
if [[ $shared_count != 0 ]]; then
  printf 'CUDA smoke refused: shared GPU count is not zero.\n' >&2
  rollback
fi
verify_windows_namespace() {
  local unit=$1 visible_socket=$2 pid
  pid=$(systemctl show "$unit" -p MainPID --value)
  [[ $pid =~ ^[1-9][0-9]*$ ]]
  nsenter --target "$pid" --mount -- runuser -u "$user" -- \
    /bin/bash -euo pipefail -c "$mount_probe" -- wsl_mount_verify "$mount_plan" "$visible_socket" /proc/self/mountinfo
  # shellcheck disable=SC2016
  nsenter --target "$pid" --mount -- /bin/bash -ec '
    shopt -s dotglob nullglob
    entries=(/run/WSL/*)
    if [[ -x /init || ${#entries[@]} != 0 ]]; then
      printf "%s\n" "Windows interop is available inside the acceptance namespace" >&2
      exit 1
    fi
  '
}
die_after_apply() { printf '%s\n' "$*" >&2; rollback; }
verify_windows_namespace "user@$uid.service" ""
# Bind only the tested mask roots, read-only, from the rootless Engine's view.
# CSV quotes keep whitespace, commas and quotes in source names literal to Docker.
container_mounts=()
for kind in dir file; do
  if [[ $kind == dir ]]; then targets=("${windows_dirs[@]}"); else targets=("${windows_files[@]}"); fi
  for i in "${!targets[@]}"; do
    path=${targets[i]//\"/\"\"}
    container_mounts+=(--mount "type=bind,\"source=$path\",target=/kio-host-mounts/$kind-$i,readonly")
  done
done
# shellcheck disable=SC2016
container_gpu=$(as_user "${docker[@]}" run --rm --pull=never --device nvidia.com/gpu=all \
  "${container_mounts[@]}" --entrypoint /bin/sh "$smoke" -ec '
    for kind in dir file; do
      count=$1
      shift
      i=0
      while [ "$i" -lt "$count" ]; do
        path=/kio-host-mounts/$kind-$i
        test "$(stat -f -c %T "$path")" = tmpfs
        if [ "$kind" = dir ]; then
          entries=$(ls -A "$path")
          test -z "$entries"
        else
          test ! -r "$path" && test ! -w "$path" || exit 1
        fi
        i=$((i + 1))
      done
    done
    test ! -x /init
    exec nvidia-smi --query-gpu=uuid --format=csv,noheader
  ' kio-mount-probe "${#windows_dirs[@]}" "${#windows_files[@]}")
[[ $container_gpu == "$gpu_uuid" ]]
printf 'Rootless container GPU: %s\n' "$container_gpu"
test_shared_access

ssh_changed=true
apply_file "$tmp/sshd" 0644 "$ssh_config"
apply_file "$tmp/keys" 0644 "$keys"
apply_file "$tmp/ssh-unit" 0644 "$ssh_service"
systemctl daemon-reload
systemctl enable "$ssh_unit"
systemctl restart "$ssh_unit"
live=false
for _ in {1..10}; do
  if [[ $(ss -H -ltn 'sport = :2222' | awk '{print $4}') == 127.0.0.1:2222 ]] &&
    scan=$(ssh-keyscan -T 2 -p 2222 -t ed25519 127.0.0.1 2>/dev/null) &&
    [[ $(awk '{print $2, $3}' <<< "$scan") == "$(awk '{print $1, $2}' "$host_key.pub")" ]]; then
    live=true
    break
  fi
  sleep 1
done
$live
verify_windows_namespace "$ssh_unit" "$shared_socket"
[[ $(read_mount_plan) == "$mount_plan" &&
   $(wsl_mount_plan /proc/self/mountinfo) == "$caller_mount_plan" ]] ||
  die_after_apply "Windows filesystem mounts changed during setup."
after_id=$(shared_get /info | jq -er "$shared_id_filter")
[[ $before_id == "$after_id" ]] || die_after_apply "Shared Engine ID changed during setup."
pid=$(systemctl show "$ssh_unit" -p MainPID --value)
shared_count=$(nsenter --target "$pid" --mount -- runuser -u "$user" -- /usr/bin/sudo -n -- "$query")
[[ $shared_count =~ ^(0|[1-9][0-9]*)$ ]]
ss -ltnp
printf 'kio-lab-wsl %s\n' "$(awk '{print $1, $2}' "$host_key.pub")"
