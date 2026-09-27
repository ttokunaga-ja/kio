# Linux bubblewrap confinement preflight

`prepare-linux-confinement.sh` is for ephemeral GitHub-hosted Ubuntu 24.04
runners only. It verifies and loads the AppArmor project's `v4.0.2` profile at
commit `84a6bc1b6dcdfeabb1ed3597f01e314f3bcee5c1` by its pinned SHA-256, then
runs the CI network/PID namespace probe. It fails closed unless the `bwrap`
profile is loaded in enforce mode and the child is stacked into
`unpriv_bwrap`, has no effective capabilities, and has distinct PID and network
namespace identities.

After its runner-context guards, the script installs only `bubblewrap`,
`apparmor`, and `libcap2-bin` through the Ubuntu package manager. It uses the
runner's existing `curl` and `sha256sum` tools.

The profile is loaded into the ephemeral runner's kernel policy for that job;
the script does not change sysctls, install setuid bits, or use an unconfined
profile. Do not use it on persistent/self-hosted machines. Failure diagnostics
include the runner kernel, AppArmor status, parser/package versions, bwrap file
mode/capabilities, and loaded profile names.

The profile source and GPL-2.0 license are in the upstream AppArmor repository:
[profile at the pinned commit](https://gitlab.com/apparmor/apparmor/-/blob/84a6bc1b6dcdfeabb1ed3597f01e314f3bcee5c1/profiles/apparmor/profiles/extras/bwrap-userns-restrict), [repository license](https://gitlab.com/apparmor/apparmor/-/blob/84a6bc1b6dcdfeabb1ed3597f01e314f3bcee5c1/LICENSE), [AppArmor 4.0.2 release notes](https://gitlab.com/apparmor/apparmor/-/wikis/Release_Notes_4.0.2). Local parser-only validation does not prove kernel enforcement on GitHub's hosted runner; that runner must pass the unchanged namespace probe before dependent CI steps proceed.
