# Authenticated OCR GPU controller

`gpu-phase.sh` is the host-side PaddleOCR controller for the measured
`kio-lab` GPU. Run it as `kio-test`. The private route binds the pinned
OCR service and public TLS identity to the exact release candidate.
A08 stops OCR and releases its GPU lease before the Actions client explicitly
installs and verifies EG2 in its isolated device state. The packaged native
runtime on the client runs indexing and queries with `--offline`.

The controller uses the reviewed `kio-acceptance-tools` binary from
`crates/kio-eval/src/acceptance_tools` and `acceptance_tools_main.rs`.
`init` stages the fixed OCR inputs and copies this binary into the OCR directory.

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
requires 6,141 MiB plus 128 MiB of free memory, and refuses unrelated running
GPU containers. Cleanup waits for the same GPU to return to within 128 MiB of
its recorded used memory. After interrupted init, cleanup without a baseline
requires a valid GPU sample and proof that the exact OCR project is absent.

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
[v1 plan](../../tasks/v1-plan.md) for evidence
and candidate-specific validation; no authenticated-local receipt is claimed.
On 2026-09-26, the account's Daybreak Blue entitlement was
confirmed. Collaboration invocation lacked `access_programs.cyber=daybreak_blue`;
a process-scoped CLI probe succeeded and an independent architecture review
started. Neither the probe nor a running review establishes completed audit
coverage. Prior GPT-6 Sol reviews and Astra cross-checks are separate evidence.
Earlier Terra reviews remain historical; no new work uses a legacy model.
