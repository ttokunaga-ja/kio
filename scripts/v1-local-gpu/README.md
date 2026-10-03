# Sequential local GPU controller

`gpu-phase.sh` is a host-side controller for the measured `kio-lab` GPU. Run
it only as `kio-test`. It creates its private, create-only runtime root at
`/home/kio-test/work/kio/v1-local-gpu`, stages the fixed inputs, and starts at
most one measured service phase at a time. It is not an Actions route installer
and it does not establish an acceptance result.

The controller is driven by the reviewed `kio-acceptance-tools` binary, built
from `crates/kio-eval/src/acceptance_tools` and `acceptance_tools_main.rs`.
`init` copies that binary into the staged OCR directory, while the script uses
it for model validation and runtime identity. The binary must be a reviewed
regular executable; a missing or changed binary causes refusal.

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
OCR inputs and offline embedding manifest, then writes a create-only identity.
It never starts a service, replaces keys or state, mounts the CA signing key,
or puts private-key material into the public identity receipt.

Use `init`, `status`, and exactly one phase (`start-ocr` or
`start-embedding`). Stop the active phase before the other begins. `stop`
stops both owned projects. The controller never prunes Docker, installs a
driver or daemon, or adds a runner.

The private Actions client creates and publishes its durable owner-private
256-bit capability before its first remote `init`; every dispatcher verb sends
that capability on standard input. `init` returns only a status, never the
capability. Therefore a lost `init` response leaves the client holding the
same authenticated cleanup authority. Client capability publication is
create-only.

Authenticated `finish` is permitted from an unknown or interrupted phase. It
removes the lease only after controller cleanup succeeds and cleanup has
successfully queried both exact Docker project labels, including exited
containers. A failed query or cleanup leaves the lease and its phase `unknown`.
During `init`, a missing compose file is safe only when the corresponding exact
project is proved absent. A pre-lease failure preserves its diagnostic runtime
root but creates neither a global lease nor a controller run; the same attempt
remains create-only and a new GitHub attempt can retry.

OCR exposes only `127.0.0.1:18443:8443`; its fixed PaddleX command and bounded
TLS proxy run in the API container. Embedding exposes only
`127.0.0.1:18444:8000`, uses native TLS, mounts the measured model bundle
read-only at `/model`, and uses offline Hugging Face and Transformers flags
with a separate transient cache. Hugging Face cache symlinks are not mounted
into the service.

Starts wait for HTTPS readiness and validate the measured model/image digests.
Before any service starts, `init` proves both exact owned projects absent and
captures one private, create-only `state/gpu-memory.json` baseline. It binds the
absolute run state directory, schema `kio.local-gpu.memory/v1`, GPU UUID, total,
used and free MiB in canonical JSON. No retry or cleanup replaces that baseline.
The helper accepts only one bounded NVIDIA CSV row; reserved VRAM may make
used plus free less than total.

Each start again proves both owned projects absent, checks the same GPU UUID
and total memory, and requires free memory of at least 6,141 + 128 MiB for OCR,
or max(6,638 MiB, ceil(total MiB × 0.8)) + 128 MiB for embedding. These thresholds
use the fixed services' recorded incremental peaks and embedding's 80% budget,
with 128 MiB headroom. They are admission guards, not acceptance evidence.

Stops require both owned projects absent and wait at most 60 seconds for the
same GPU to return to captured used memory plus 128 MiB. The historical 575 MiB
idle reading is not a host-wide constant. A failed recovery exits nonzero so
dispatch preserves the unknown lease. Interrupted-init cleanup may succeed with
an absent baseline only after both projects are proved absent and a valid GPU
sample is available; malformed or unsafe baseline files always fail. GPU sample
reads at init/start are bounded to five seconds. Starts always require a valid baseline. An unrelated running GPU container makes the
controller refuse to proceed.

The identity uses `kio.local-gpu.identity/v1`, compact sorted JSON without a
trailing newline and below 64 KiB. It separately binds OCR and embedding
composition digests, controller inputs, TLS public material, and measured
images. Phase observations bind that identity to observed digest-verified
running image IDs. No private key is copied into or hashed by either record.

## Candidate-bound deployment proposal

The forced private route uses an owner-private deployment record with schema
`kio.v1.local_gpu.deployment/v2`. Do not handwrite this record or a stale
fixed-source digest list. Generate the proposal from a clean exact candidate
with the candidate-built helper:

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

As of the 2026-09-26 snapshot, there was no installed private CI route or authorized new accounts,
keys, ACLs, firewall changes, or Environment configuration, and no local
acceptance receipt. On 2026-10-03, the owner approved the narrowed Tailscale
policy, CI OIDC credential, and main-only `v1-local-acceptance` Environment;
these three settings were applied and read back. Windows/WSL account, key,
dispatcher installation, and SSH configuration changes were additionally
approved with the detailed execution plan on 2026-10-04. They remain unapplied
until the final candidate bundle is regenerated and validated. See the dated
[execution record](../../tasks/v1-closeout-execution-2026-10-03.md) for evidence
and candidate-specific validation; no authenticated-local receipt is claimed.
On 2026-09-26, the account's Daybreak Blue entitlement was
confirmed. Collaboration invocation lacked `access_programs.cyber=daybreak_blue`;
a process-scoped CLI probe succeeded and an independent architecture review
started. Neither the probe nor a running review establishes completed audit
coverage. Prior GPT-6 Sol reviews and Astra cross-checks are separate evidence.
Earlier Terra reviews remain historical; no new work uses a legacy model.
