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
(( EUID != 0 )) || { echo 'run as the runner user; privilege elevation is limited to runner package/profile/user-manager setup' >&2; exit 2; }
[[ -n "${RUNNER_TEMP:-}" && -d "$RUNNER_TEMP" ]] || { echo 'RUNNER_TEMP is unavailable' >&2; exit 2; }
[[ -n "${GITHUB_RUN_ID:-}" && -n "${GITHUB_WORKSPACE:-}" ]] || { echo 'GitHub runner context is incomplete' >&2; exit 2; }

[[ -n "${GITHUB_ENV:-}" && -f "$GITHUB_ENV" && -w "$GITHUB_ENV" ]] || { echo 'GITHUB_ENV must be an existing writable file' >&2; exit 2; }

# shellcheck disable=SC1091 # This file is part of the supported Ubuntu runner image.
. /etc/os-release
[[ "${ID:-}" == ubuntu && "${VERSION_ID:-}" == 24.04 ]] || {
  echo "expected Ubuntu 24.04; found ${PRETTY_NAME:-unknown}" >&2
  exit 2
}
command -v curl >/dev/null || { echo 'curl is unavailable' >&2; exit 2; }
command -v sha256sum >/dev/null || { echo 'sha256sum is unavailable' >&2; exit 2; }
command -v sudo >/dev/null || { echo 'sudo is unavailable' >&2; exit 2; }
command -v python3 >/dev/null || { echo 'python3 is unavailable' >&2; exit 2; }
command -v apt-get >/dev/null || { echo 'apt-get is unavailable' >&2; exit 2; }

# Provision only the packages needed by this prerequisite, and only after
# proving this is an ephemeral GitHub-hosted Ubuntu runner.
sudo -n apt-get update
sudo -n apt-get install --yes --no-install-recommends bubblewrap apparmor libcap2-bin dbus-user-session
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

# The user manager owns the resource envelope; the probe itself stays unprivileged.
runner_uid="$(id -u)"
timeout 30s sudo -n loginctl enable-linger "$(id -un)"
timeout 30s sudo -n systemctl start "user@${runner_uid}.service"
export XDG_RUNTIME_DIR="/run/user/${runner_uid}"
export DBUS_SESSION_BUS_ADDRESS="unix:path=${XDG_RUNTIME_DIR}/bus"
python3 - <<'PYTHON'
import os
from pathlib import Path
import stat
import subprocess
import sys
import time
import uuid


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


uid = os.getuid()
runtime = Path(os.environ["XDG_RUNTIME_DIR"])
require(runtime == Path(f"/run/user/{uid}") and runtime.resolve() == runtime,
        "runtime directory is not canonical")
metadata = runtime.stat()
require(stat.S_ISDIR(metadata.st_mode) and metadata.st_uid == uid
        and stat.S_IMODE(metadata.st_mode) == 0o700,
        "runtime directory must be owned by the runner with mode 0700")
bus = runtime / "bus"
bus_metadata = bus.lstat()
require(stat.S_ISSOCK(bus_metadata.st_mode) and bus_metadata.st_uid == uid,
        "user manager bus is not a runner-owned socket")
require(subprocess.check_output(["stat", "-f", "-c", "%T", "/sys/fs/cgroup"],
                                text=True, timeout=5).strip() == "cgroup2fs",
        "unified cgroup v2 is required")
subprocess.run(["systemctl", "--user", "show-environment"], check=True,
               stdout=subprocess.DEVNULL, timeout=10)
subprocess.run(["busctl", "--user", "--timeout=10", "status", "org.freedesktop.systemd1"],
               check=True, stdout=subprocess.DEVNULL, timeout=15)

# Check the controls of the process actually launched in the transient scope,
# rather than trusting requested systemd properties or the runner's own cgroup.
probe = r"""
import os
from pathlib import Path
import subprocess
import sys


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


require(os.getuid() == int(sys.argv[1]) != 0, "probe must remain unprivileged")
entries = Path("/proc/self/cgroup").read_text().splitlines()
require(len(entries) == 1 and entries[0].startswith("0::/"),
        "probe must have a unified cgroup membership")
relative = Path(entries[0][3:])
require(relative.name == sys.argv[2] and ".." not in relative.parts,
        "probe did not enter its unique scope")
root = Path("/sys/fs/cgroup")
cgroup = root / str(relative).lstrip("/")
require(cgroup.resolve().is_relative_to(root), "cgroup escaped unified mount")
print(f"cgroup={cgroup}", flush=True)
required = {"cpu", "memory", "pids"}
require(required <= set((cgroup.parent / "cgroup.controllers").read_text().split()),
        "user manager lacks required delegated controllers")
require(required <= set((cgroup.parent / "cgroup.subtree_control").read_text().split()),
        "required controllers are not enabled for the scope")
for filename, expected in {"memory.max": "67108864", "memory.swap.max": "0",
                           "pids.max": "32", "memory.oom.group": "1"}.items():
    actual = (cgroup / filename).read_text().strip()
    require(actual == expected, f"{filename}: expected {expected}, got {actual}")
quota, period = (cgroup / "cpu.max").read_text().split()
require(quota != "max" and int(quota) == int(period) > 0,
        "cpu.max must impose a 100% CPU quota")
expected_properties = {"TimeoutStopUSec": "5s", "KillSignal": "9",
                       "KillMode": "control-group", "RuntimeMaxUSec": "15s"}
manager_output = subprocess.check_output(
    ["systemctl", "--user", "show", sys.argv[2],
     "--property=" + ",".join(expected_properties)], text=True, timeout=5)
actual_properties = dict(line.split("=", 1) for line in manager_output.splitlines())
require(actual_properties == expected_properties,
        f"unexpected scope termination properties: {actual_properties}")
print(manager_output, end="")
print(f"Verified resource controls and termination properties in {cgroup}")
"""
unit = f"kio-confinement-preflight-{uuid.uuid4().hex}.scope"
properties = ["MemoryMax=64M", "MemorySwapMax=0", "TasksMax=32", "CPUQuota=100%",
              "OOMPolicy=kill", "RuntimeMaxSec=15s", "KillMode=control-group",
              "KillSignal=SIGKILL", "TimeoutStopSec=5s"]
command = ["systemd-run", "--user", "--scope", "--quiet", "--collect", f"--unit={unit}"]
command += [f"--property={value}" for value in properties]
command += ["--", "/usr/bin/python3", "-c", probe, str(uid), unit]
probe_output = ""
try:
    result = subprocess.run(command, capture_output=True, text=True, timeout=20)
    probe_output = result.stdout
    print(probe_output, end="")
    if result.stderr:
        print(result.stderr, end="", file=sys.stderr)
    result.check_returncode()
except subprocess.TimeoutExpired as error:
    probe_output = error.stdout or ""
    if isinstance(probe_output, bytes):
        probe_output = probe_output.decode("utf-8", errors="replace")
    raise
finally:
    # --collect normally removes a completed scope. Stop only this unique unit
    # as well, including on probe failure or timeout; never touch other scopes.
    stopped = subprocess.run(["systemctl", "--user", "stop", unit],
                             capture_output=True, text=True, timeout=10)
    state = subprocess.run(["systemctl", "--user", "show", unit,
                            "--property=LoadState", "--value"],
                           capture_output=True, text=True, timeout=5)
    require(stopped.returncode == 0 or
            state.stdout.strip() == "not-found",
            f"could not clean up preflight scope: {stopped.stderr.strip()}")
    for line in probe_output.splitlines():
        if not line.startswith("cgroup="):
            continue
        cgroup = Path(line.removeprefix("cgroup="))
        require(cgroup.name == unit and cgroup.is_relative_to("/sys/fs/cgroup"),
                "invalid cleanup cgroup path")
        deadline = time.monotonic() + 5
        while cgroup.exists():
            try:
                events = dict(line.split() for line in (cgroup / "cgroup.events").read_text().splitlines())
            except FileNotFoundError:
                break
            if events.get("populated") == "0":
                break
            require(time.monotonic() < deadline, "preflight scope still has processes after stop")
            time.sleep(0.1)

# Export only after both preflights pass. Repeated invocation must not append
# duplicate environment entries or silently accept a conflicting value.
envfile = Path(os.environ["GITHUB_ENV"])
require(envfile.is_file(), "GITHUB_ENV disappeared")
lines = envfile.read_text().splitlines()
missing = []
for key in ("XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"):
    expected = f"{key}={os.environ[key]}"
    existing = [line for line in lines if line.startswith((f"{key}=", f"{key}<<"))]
    require(not existing or existing == [expected], f"conflicting {key} in GITHUB_ENV")
    if not existing:
        missing.append(expected)
if missing:
    with envfile.open("a") as output:
        if lines:
            output.write("\n")
        output.write("\n".join(missing) + "\n")
PYTHON

echo 'Linux systemd-user/cgroup v2 resource preflight passed.'
