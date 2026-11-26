#!/bin/bash
# shellcheck disable=SC2329
set -euo pipefail
# shellcheck source=/dev/null
source "$(dirname "$0")/wsl-windows-mounts.sh"
tmp=$(mktemp -d)
trap 'rm -r -- "$tmp"' EXIT
mkdir -p "$tmp/Docker/host/nested" "$tmp/One Drive" "$tmp"/$'tab\tline\nback\\040' "$tmp/native"
touch "$tmp/file"
cat > "$tmp/mountinfo" <<FIXTURE
42 1 0:30 / $tmp/Docker/host rw - 9p source rw
43 42 0:31 / $tmp/Docker/host/nested rw - virtiofs source rw
44 1 0:32 / $tmp/One\040Drive rw - drvfs source rw
45 1 0:33 / $tmp/tab\011line\012back\134040 rw - fuse.virtiofs source rw
46 1 0:34 / $tmp/file rw - 9p source rw
47 1 0:35 / $tmp/native rw - ext4 source rw
FIXTURE
plan=$(wsl_mount_plan "$tmp/mountinfo")
[[ $(grep -c '^windows ' <<< "$plan") == 5 ]]
[[ $(grep -c '^directory ' <<< "$plan") == 3 ]]
[[ $plan == *"file $tmp/file"* ]]
[[ $plan != *"$tmp/native"* ]]
decoded=
wsl_mount_decode "$tmp/tab\011line\012back\134040"
[[ $decoded == "$tmp/"$'tab\tline\nback\\040' ]]
policy=$(wsl_mount_policy "$plan")
[[ $policy == *"TemporaryFileSystem=\"$tmp/One Drive:ro\""* ]]
[[ $policy == *"InaccessiblePaths=\"$tmp/file\""* ]]
[[ $policy != *"/host/nested"* ]]
[[ $(wsl_mount_policy 'directory /a%"b') == 'TemporaryFileSystem="/a%%\"b:ro"' ]]

cat > "$tmp/gpu-mountinfo" <<FIXTURE
42 1 0:40 / /usr/lib/wsl/drivers ro - 9p source ro
43 1 0:41 / /usr/lib/wsl/lib ro - 9p source ro
44 1 0:42 / /usr/lib/wsl/drivers/other ro - 9p source ro
45 1 0:43 / /usr/lib/wsl/drivers-other ro - 9p source ro
FIXTURE
gpu_plan=$(wsl_mount_plan "$tmp/gpu-mountinfo")
gpu_policy=$(wsl_mount_policy "$gpu_plan")
[[ $gpu_policy != *'"/usr/lib/wsl/drivers"'* && $gpu_policy != *'"/usr/lib/wsl/drivers:ro"'* ]]
[[ $gpu_policy != *'"/usr/lib/wsl/lib"'* && $gpu_policy != *'"/usr/lib/wsl/lib:ro"'* ]]
[[ $gpu_policy == *'/usr/lib/wsl/drivers/other'* && $gpu_policy == *'/usr/lib/wsl/drivers-other'* ]]
wsl_mount_device() {
  case $1 in
    /usr/lib/wsl/drivers) printf '0:40\n' ;;
    /usr/lib/wsl/lib) printf '0:41\n' ;;
    *) return 1 ;;
  esac
}
sed -n '1,2p' "$tmp/gpu-mountinfo" > "$tmp/gpu-live"
wsl_mount_verify '' '' "$tmp/gpu-live"
sed 's@/drivers ro @/drivers rw @' "$tmp/gpu-live" > "$tmp/gpu-live-rw"
if wsl_mount_verify '' '' "$tmp/gpu-live-rw"; then exit 1; fi

mkdir -p "$tmp/mask" "$tmp/stage"
chmod 0555 "$tmp/mask"
trap 'chmod 0755 "$tmp/mask"; rm -r -- "$tmp"' EXIT
wsl_mount_device() { printf '%s\n' 0:40; }
printf '42 1 0:40 / %s/mask ro - tmpfs none rw\n' "$tmp" > "$tmp/live"
verify_plan=$(printf 'windows %s/mask 9p 0:30\nwindows %s/mask/nested 9p 0:31\ndirectory %s/mask\n' "$tmp" "$tmp" "$tmp")
if [[ $EUID != 0 ]]; then
  wsl_mount_verify "$verify_plan" '' "$tmp/live"
  chmod 0755 "$tmp/mask"
  printf 'nameserver 10.0.0.1\n' > "$tmp/mask/resolv.conf"
  chmod 0444 "$tmp/mask/resolv.conf"
  printf '43 42 0:30 /resolv.conf %s/mask/resolv.conf ro - tmpfs none rw\n' "$tmp" >> "$tmp/live"
  chmod 0555 "$tmp/mask"
  wsl_mount_verify "$verify_plan" '' "$tmp/live" "$tmp/mask/resolv.conf"
  sed 's@/resolv.conf ro @/resolv.conf rw @' "$tmp/live" > "$tmp/live-resolver-rw"
  if wsl_mount_verify "$verify_plan" '' "$tmp/live-resolver-rw" "$tmp/mask/resolv.conf"; then exit 1; fi
  chmod 0755 "$tmp/mask"
  mkdir "$tmp/mask/shared-sockets"
  chmod 0555 "$tmp/mask"
  if wsl_mount_verify "$verify_plan" '' "$tmp/live" "$tmp/mask/resolv.conf"; then exit 1; fi
  chmod 0755 "$tmp/mask"
  rmdir "$tmp/mask/shared-sockets"
  rm "$tmp/mask/resolv.conf"
  sed '2d' "$tmp/live" > "$tmp/live-clean"
  mv "$tmp/live-clean" "$tmp/live"
  chmod 0555 "$tmp/mask"
fi
sed 's/ ro / rw /' "$tmp/live" > "$tmp/live-rw"
if wsl_mount_verify "$verify_plan" '' "$tmp/live-rw"; then exit 1; fi
printf '42 1 0:40 / %s/mask ro - 9p source rw\n' "$tmp" > "$tmp/live-exposed"
if wsl_mount_verify "$verify_plan" '' "$tmp/live-exposed"; then exit 1; fi

touch "$tmp/Docker/host/nvidia-smi" "$tmp/native/libdxcore.so"
realpath() {
  [[ $1 == -ez && $2 == -- && -f $3 ]] || return 1
  printf '%s\0' "$3"
}
wsl_cdi_driver_file() { [[ $1 == "$tmp/Docker/host/nvidia-smi" ]]; }
jq -n --arg driver "$tmp/Docker/host/nvidia-smi" --arg native "$tmp/native/libdxcore.so" \
  '{devices: [], containerEdits: {mounts: [
    {hostPath: $driver, containerPath: "/nvidia-smi", options: ["ro"]},
    {hostPath: $native, containerPath: "/libdxcore.so"}]}}' > "$tmp/spec"
wsl_stage_cdi "$plan" "$tmp/spec" "$tmp/stage" /opt/kio/cdi
jq -e --arg native "$tmp/native/libdxcore.so" '.containerEdits.mounts == [
  {hostPath: "/opt/kio/cdi/driver-0", containerPath: "/nvidia-smi", options: ["ro"]},
  {hostPath: $native, containerPath: "/libdxcore.so"}]' "$tmp/stage/nvidia.json" >/dev/null
cmp "$tmp/Docker/host/nvidia-smi" "$tmp/stage/driver-0"
wsl_cdi_driver_file() { return 1; }
if wsl_stage_cdi "$plan" "$tmp/spec" "$tmp/stage" /opt/kio/cdi; then exit 1; fi
printf 'Windows mount and CDI fixtures passed\n'
