# Authenticated OCR GPU controller

`gpu-phase.sh` is the host-side PaddleOCR controller for the measured
`kio-lab` GPU. Run it as `kio-test`. The private route binds the pinned
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
requires 6,141 MiB plus 128 MiB of free memory, and refuses unrelated running
GPU containers. Cleanup waits for the same GPU to return to within 128 MiB of
its recorded used memory. After interrupted init, cleanup without a baseline
requires a valid GPU sample and proof that the exact OCR project is absent.

## Candidate-bound deployment proposal

The forced private route uses an owner-private deployment record with schema
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

As of the 2026-09-26 snapshot, there was no installed private CI route or authorized new accounts,
keys, ACLs, firewall changes, or Environment configuration, and no local
acceptance receipt. On 2026-10-03, the owner approved the narrowed Tailscale
policy, CI OIDC credential, and main-only `v1-local-acceptance` Environment;
these three settings were applied and read back. Windows/WSL account, key,
dispatcher installation, and SSH configuration changes were additionally
approved with the detailed execution plan on 2026-10-04. They remain unapplied
until the final candidate bundle is regenerated and validated. See the
[v1 acceptance procedure](../../tasks/v1-acceptance.md) for current preparation
and candidate-specific validation; no authenticated-local receipt is claimed.
On 2026-09-26, the account's Daybreak Blue entitlement was
confirmed. Collaboration invocation lacked `access_programs.cyber=daybreak_blue`;
a process-scoped CLI probe succeeded and an independent architecture review
started. Neither the probe nor a running review establishes completed audit
coverage. Prior GPT-6 Sol reviews and Astra cross-checks are separate evidence.
Earlier Terra reviews remain historical; no new work uses a legacy model.

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

1. From the repository root on an RTX 4060 8 GiB WSL2 host with Docker Compose
   and NVIDIA GPU support, use
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
