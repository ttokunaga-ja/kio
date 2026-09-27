#!/usr/bin/env bash
set -euo pipefail

# AppArmor's Ubuntu user-namespace restriction prevents bubblewrap from
# configuring its requested network namespace unless bwrap has a bounded
# profile. This is deliberately limited to GitHub-hosted Ubuntu runners.
readonly PROFILE_COMMIT="84a6bc1b6dcdfeabb1ed3597f01e314f3bcee5c1"
readonly PROFILE_SHA256="a964037f6cf0df1099f14226b037eaedde6237c86e715188e93eb460b30be859"
readonly PROFILE_URL="https://gitlab.com/apparmor/apparmor/-/raw/${PROFILE_COMMIT}/profiles/apparmor/profiles/extras/bwrap-userns-restrict"

profile_file=""

diagnostics() {
  printf '\n=== Linux confinement diagnostics ===\n'
  uname -a || true
  if [[ -r /etc/os-release ]]; then cat /etc/os-release; fi
  id || true
  printf 'AppArmor enabled: '
  cat /sys/module/apparmor/parameters/enabled 2>/dev/null || printf 'unavailable\n'
  printf 'LSM list: '
  cat /sys/kernel/security/lsm 2>/dev/null || printf 'unavailable\n'
  printf 'Current label: '
  cat /proc/self/attr/current 2>/dev/null || printf 'unavailable\n'
  printf 'Userns restriction sysctl: '
  cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns 2>/dev/null || printf 'unavailable\n'
  if command -v sudo >/dev/null 2>&1; then
    local loaded_profiles
    if loaded_profiles="$(sudo -n cat /sys/kernel/security/apparmor/profiles 2>/dev/null)"; then
      printf '%s\n' "$loaded_profiles" | awk '
        / \((enforce|complain|kill|unconfined)\)$/ {
          name = $0
          sub(/ \((enforce|complain|kill|unconfined)\)$/, "", name)
          if (name == "bwrap" || name == "/usr/bin/bwrap" || name == "unpriv_bwrap") print
        }
      '
    else
      printf 'Loaded profile listing: unavailable\n'
    fi
  fi
  if command -v apparmor_parser >/dev/null 2>&1; then
    apparmor_parser --version 2>&1 || true
  fi
  if command -v dpkg-query >/dev/null 2>&1; then
    dpkg-query --show --showformat='${Package} ${Version}\n' apparmor bubblewrap 2>&1 || true
  fi
  if [[ -x /usr/bin/bwrap ]]; then
    stat -c 'bwrap mode=%a owner=%U:%G' /usr/bin/bwrap 2>&1 || true
    getcap /usr/bin/bwrap 2>&1 || true
  fi
  if command -v aa-status >/dev/null 2>&1; then aa-status 2>&1 || true; fi
  printf '=== end confinement diagnostics ===\n'
}

on_exit() {
  local status=$?
  if (( status != 0 )); then diagnostics; fi
  if [[ -n "$profile_file" ]]; then rm -f -- "$profile_file"; fi
  exit "$status"
}
trap on_exit EXIT

[[ "${GITHUB_ACTIONS:-}" == "true" ]] || { echo 'must run inside GitHub Actions' >&2; exit 2; }
[[ "${RUNNER_ENVIRONMENT:-}" == "github-hosted" ]] || { echo 'requires a GitHub-hosted runner' >&2; exit 2; }
[[ "${RUNNER_OS:-}" == "Linux" ]] || { echo 'requires a Linux runner' >&2; exit 2; }
(( EUID != 0 )) || { echo 'run as the runner user; privilege elevation is limited to runner package/profile setup' >&2; exit 2; }
[[ -n "${RUNNER_TEMP:-}" && -d "$RUNNER_TEMP" ]] || { echo 'RUNNER_TEMP is unavailable' >&2; exit 2; }
[[ -n "${GITHUB_RUN_ID:-}" && -n "${GITHUB_WORKSPACE:-}" ]] || { echo 'GitHub runner context is incomplete' >&2; exit 2; }

# shellcheck disable=SC1091 # This file is part of the supported Ubuntu runner image.
. /etc/os-release
[[ "${ID:-}" == ubuntu && "${VERSION_ID:-}" == 24.04 ]] || {
  echo "expected Ubuntu 24.04; found ${PRETTY_NAME:-unknown}" >&2
  exit 2
}
command -v curl >/dev/null || { echo 'curl is unavailable' >&2; exit 2; }
command -v sha256sum >/dev/null || { echo 'sha256sum is unavailable' >&2; exit 2; }
command -v sudo >/dev/null || { echo 'sudo is unavailable' >&2; exit 2; }
command -v apt-get >/dev/null || { echo 'apt-get is unavailable' >&2; exit 2; }

# Provision only the packages needed by this prerequisite, and only after
# proving this is an ephemeral GitHub-hosted Ubuntu runner.
sudo -n apt-get update
sudo -n apt-get install --yes --no-install-recommends bubblewrap apparmor libcap2-bin
[[ -x /usr/bin/bwrap ]] || { echo 'bubblewrap package did not provide /usr/bin/bwrap' >&2; exit 1; }
command -v apparmor_parser >/dev/null || { echo 'apparmor package did not provide apparmor_parser' >&2; exit 1; }
command -v getcap >/dev/null || { echo 'libcap2-bin package did not provide getcap' >&2; exit 1; }

diagnostics
profile_file="$(mktemp "$RUNNER_TEMP/kio-bwrap-userns-restrict.XXXXXX")"
curl --fail --location --proto '=https' --tlsv1.2 \
  --connect-timeout 15 --max-time 120 --retry 3 --retry-delay 1 --retry-max-time 90 \
  --output "$profile_file" "$PROFILE_URL"
printf '%s  %s\n' "$PROFILE_SHA256" "$profile_file" | sha256sum --check --status

# The upstream ABI-4 profile gives bwrap the privileges needed for namespace
# setup, then stacks every child into unpriv_bwrap, which denies capabilities.
# Load only this profile; do not change sysctls, binary privilege bits, or the
# runner's global AppArmor mode.
sudo -n apparmor_parser --replace "$profile_file"
loaded_profiles="$(sudo -n cat /sys/kernel/security/apparmor/profiles)"
printf '%s\n' "$loaded_profiles" | awk '
  / \(enforce\)$/ {
    name = $0
    sub(/ \(enforce\)$/, "", name)
    if (name == "bwrap" || name == "/usr/bin/bwrap") found = 1
  }
  END { exit !found }
' || { echo 'the pinned bwrap profile is not loaded in enforce mode' >&2; exit 1; }

host_pidns="$(readlink /proc/self/ns/pid)"
host_netns="$(readlink /proc/self/ns/net)"
# shellcheck disable=SC2016 # This script text must expand inside the bwrap child.
child_output="$(/usr/bin/bwrap \
  --unshare-net --unshare-pid --die-with-parent --new-session \
  --ro-bind / / --proc /proc --dev /dev --tmpfs /tmp -- \
  /bin/sh -eu -c '
    printf "label=%s\\n" "$(cat /proc/self/attr/current)"
    printf "capeff=%s\\n" "$(awk "/^CapEff:/ { print \$2 }" /proc/self/status)"
    printf "pidns=%s\\n" "$(readlink /proc/self/ns/pid)"
    printf "netns=%s\\n" "$(readlink /proc/self/ns/net)"
  ')"
printf '%s\n' "$child_output"

child_label="$(printf '%s\n' "$child_output" | sed -n 's/^label=//p')"
child_capeff="$(printf '%s\n' "$child_output" | sed -n 's/^capeff=//p')"
child_pidns="$(printf '%s\n' "$child_output" | sed -n 's/^pidns=//p')"
child_netns="$(printf '%s\n' "$child_output" | sed -n 's/^netns=//p')"
[[ "$child_label" == *unpriv_bwrap* ]] || { echo 'bwrap child did not enter the stacked unpriv_bwrap profile' >&2; exit 1; }
[[ "$child_capeff" =~ ^0+$ ]] || { echo "bwrap child retains effective capabilities: $child_capeff" >&2; exit 1; }
[[ -n "$child_pidns" && "$child_pidns" != "$host_pidns" ]] || { echo 'bwrap child did not enter a distinct PID namespace' >&2; exit 1; }
[[ -n "$child_netns" && "$child_netns" != "$host_netns" ]] || { echo 'bwrap child did not enter a distinct network namespace' >&2; exit 1; }

echo 'Linux AppArmor/bubblewrap confinement preflight passed.'
