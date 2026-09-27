# Linux confinement prerequisites and preflights

`prepare-linux-confinement.sh` is for ephemeral GitHub-hosted Ubuntu 24.04
runners only. It verifies and loads the AppArmor project's `v4.0.2` profile at
commit `84a6bc1b6dcdfeabb1ed3597f01e314f3bcee5c1` by its pinned SHA-256, then
runs the CI network/PID namespace probe. It fails closed unless the `bwrap`
profile is loaded in enforce mode and the child is stacked into
`unpriv_bwrap`, has no effective capabilities, and has distinct PID and network
namespace identities.

After its runner-context guards, the script installs only `bubblewrap`,
`apparmor`, `libcap2-bin`, and `dbus-user-session` through the Ubuntu package manager. It uses the
runner's existing `curl`, `sha256sum`, and Python 3 tools.

The profile is loaded into the ephemeral runner's kernel policy for that job;
the script does not change sysctls, install setuid bits, or use an unconfined
profile. Do not use it on persistent/self-hosted machines. Failure diagnostics
include the runner kernel, AppArmor status, parser/package versions, bwrap file
mode/capabilities, and loaded profile names.

The profile source and GPL-2.0 license are in the upstream AppArmor repository:
[profile at the pinned commit](https://gitlab.com/apparmor/apparmor/-/blob/84a6bc1b6dcdfeabb1ed3597f01e314f3bcee5c1/profiles/apparmor/profiles/extras/bwrap-userns-restrict), [repository license](https://gitlab.com/apparmor/apparmor/-/blob/84a6bc1b6dcdfeabb1ed3597f01e314f3bcee5c1/LICENSE), [AppArmor 4.0.2 release notes](https://gitlab.com/apparmor/apparmor/-/wikis/Release_Notes_4.0.2). Local parser-only validation does not prove kernel enforcement on GitHub's hosted runner; that runner must pass the unchanged namespace probe before dependent CI steps proceed.

The helper also enables linger for the runner user and starts only that user's
`user@UID.service`, using sudo after the same runner guards. It requires a
canonical `/run/user/UID` directory owned by that UID with mode 0700, a usable
user-manager bus, and a unified cgroup v2 mount with delegated CPU, memory, and
PID controllers. It does not change cgroup permissions globally.

An unprivileged, uniquely named transient user scope tests `MemoryMax=64M`
(64 MiB), `MemorySwapMax=0`, `TasksMax=32`, `CPUQuota=100%`, `OOMPolicy=kill`,
`RuntimeMaxSec=15s`, `KillMode=control-group`, `KillSignal=SIGKILL`, and
`TimeoutStopSec=5s`. The Python child resolves its
own `/proc/self/cgroup` membership and verifies `memory.max`, `memory.swap.max`,
`pids.max`, `cpu.max`, and `memory.oom.group`, plus the parent controller
delegation. While alive, it also reads back `TimeoutStopUSec`, `KillSignal`,
`KillMode`, and `RuntimeMaxUSec` from the user manager. Explicit SIGKILL and the
five-second stop timeout bound termination when the independent runtime limit
expires; this preflight verifies configured properties without exercising the
runtime-expiry path. `TimeoutStopSec` is supported by
[systemd v255 scope D-Bus properties](https://github.com/systemd/systemd/blob/v255/src/core/dbus-scope.c).
`OOMPolicy=kill` is supported by
[systemd v255 scopes](https://github.com/systemd/systemd/blob/v255/man/systemd.scope.xml);
its [documented behavior](https://github.com/systemd/systemd/blob/v255/man/systemd.service.xml)
sets `memory.oom.group=1`. The helper stops its unique scope on success, failure,
or timeout and allows up to five seconds for its cgroup to disappear or become
unpopulated. The probe command and user-manager operations also have timeouts.

After both preflights pass, the helper appends canonical `XDG_RUNTIME_DIR` and
`DBUS_SESSION_BUS_ADDRESS` values once to the existing writable `GITHUB_ENV` file
for subsequent renderer and watcher steps. Conflicting or duplicate values fail
closed. Consumers should not repeat user-manager setup or these exports.

Local shell/static and mocked-probe checks do not provision a host or establish
Ubuntu 24.04 resource enforcement. Native hosted CI must pass both preflights.
The resource probe checks installed limits and cleanup, not stress-induced OOM,
PID exhaustion, CPU scheduling, or the complete renderer containment path.
