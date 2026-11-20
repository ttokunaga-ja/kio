# Manual full and cold Rust gates

This is the single entry point for high-cost or external Kio validation that
does not belong in every push/pull-request run. The commands below are explicit
Rust entry points, not ignored tests or compatibility wrappers. They do not
authorize a scheduled workflow, a paid request, a GPU allocation, fixture
discovery, deployment, or publication.

The ordinary six-job responsibility and timeouts remain in
[the CI workflow](../.github/workflows/ci.yml):

| Job | Signal | Timeout |
| --- | --- | ---: |
| `acceptance-tooling` | acceptance tooling checks | 10 min |
| `rust` | format, clippy, complete workspace/all-target tests | 45 min |
| `synthetic-history-eval` | release/all-features synthetic, scale, replay, cross-scope, rerank, M3-1/2/3 | 10 min |
| `macos-security-r23` | native macOS workspace/all-target behavior | 90 min |
| `windows-workspace-tests` | native Windows workspace/all-target behavior | 120 min |
| `windows-security-r23` | Windows security checks | 5 min |

Do not shorten these bounds or merge jobs until several matching successful
GitHub runs exist. Do not generate measurement samples with empty commits,
reruns, dispatches, or repeated pushes.

## 1. Complete cold build

Use a fresh target directory to run a complete offline cold workspace test and
release all-features build.

```sh
KIO_COLD_A=$(mktemp -d "${TMPDIR:-/tmp}/kio-cold-a.XXXXXX")
mkdir -p "$KIO_COLD_A/tmp"
CARGO_TARGET_DIR="$KIO_COLD_A/target" TMPDIR="$KIO_COLD_A/tmp" \
  LC_ALL=C LANG=C TZ=UTC cargo +1.98.0 test --workspace --all-targets --locked --offline
CARGO_TARGET_DIR="$KIO_COLD_A/target" TMPDIR="$KIO_COLD_A/tmp" \
  LC_ALL=C LANG=C TZ=UTC cargo +1.98.0 build --release --all-features --locked --offline
```

`--offline` makes a missing local Cargo cache an explicit environment blocker.

## 2. Full scale acceptance

The Full scale fixture is 20 scopes, 4,000 files, and 120,000 expected current
chunks. Only Full with exactly five warmups and 100 samples is formal-eligible.
The first command builds the shared release binaries used by this section and
sections 3-4. Run that command first when starting directly at U7 or OCR.

```sh
KIO_FULL_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/kio-scale-full.XXXXXX")

cargo +1.98.0 build --release --all-features --locked --offline \
  -p kio-cli -p kio-eval --bins
target/release/kio-eval scale generate \
  --out "$KIO_FULL_ROOT/current" --profile full --lane current-text
target/release/kio-eval scale generate \
  --out "$KIO_FULL_ROOT/history" --profile full --lane history-overlay
target/release/kio-eval scale prepare \
  --corpus "$KIO_FULL_ROOT/current" --bin target/release/kio
target/release/kio-eval scale prepare \
  --corpus "$KIO_FULL_ROOT/history" --bin target/release/kio
target/release/kio-eval scale attest \
  --corpus "$KIO_FULL_ROOT/current"
target/release/kio-eval scale attest \
  --corpus "$KIO_FULL_ROOT/history"
jq -e '
  .schema_version == 3 and .lane == "current_text" and
  .edit_operations == 0 and .rename_operations == 0 and .delete_operations == 0 and
  .current_chunks == 120000 and .historical_only_chunks == 0 and
  .deleted_chunks == 0 and .physical_chunks == 120000
' "$KIO_FULL_ROOT/current/scale-attestation.json"
jq -e '
  .schema_version == 3 and .lane == "history_overlay" and
  .edit_operations == 20 and .rename_operations == 20 and .delete_operations == 20 and
  .current_chunks == 119400 and .historical_only_chunks == 1200 and
  .deleted_chunks == 600 and .physical_chunks == 120600
' "$KIO_FULL_ROOT/history/scale-attestation.json"
target/release/kio-eval scale benchmark \
  --current-corpus "$KIO_FULL_ROOT/current" \
  --history-corpus "$KIO_FULL_ROOT/history" --bin target/release/kio \
  --warmups 5 --samples 100 \
  --out "$KIO_FULL_ROOT/benchmark.json"
jq -e '
  .measurement_class == "full_manual_p4_gate" and
  .full_formal_manual_gate == true and
  .acceptance_failed == false and
  .passed_p95_thresholds == true and
  .warmups == 5 and .samples == 100 and
  ([.lanes[].name] | sort) == ["current-text", "deleted", "history", "hybrid", "vector"] and
  ([.lanes[].name] | unique | length) == 5 and
  (all(.lanes[]; .requested_mode == .resolved_mode)) and
  (all(.lanes[]; .pointer_attestations > 0)) and
  ([.lanes[] | select(.name == "current-text") | .population] == [120000]) and
  ([.lanes[] | select(.name == "vector") | .population] == [120000]) and
  ([.lanes[] | select(.name == "hybrid") | .population] == [120000]) and
  ([.lanes[] | select(.name == "history") | .population] == [1200]) and
  ([.lanes[] | select(.name == "deleted") | .population] == [600]) and
  ([.lanes[] | select(.name == "deleted") |
    .restore_raw_verified and .restore_working_tree_unchanged] == [true])
' "$KIO_FULL_ROOT/benchmark.json"
```

The two corpus paths are create-only and must remain distinct. The Rust command
publishes its create-only report after its typed search-contract checks. At the
formal Full sampling shape it retains the M3-1 current-text p95 < 5 seconds and
M3-2/M3-3 history/deleted p95 < 7 seconds gates for both product and process-wall
signals; vector/hybrid are recorded without inventing a new threshold. Tiny
remains a contract smoke and cannot satisfy this gate. The report's D1 TTFV/cost
fields remain typed `not-measured` for this scale-only run: P4 owns actual D1
data, and dogfood is a separate manual gate. See [the evaluator contract](../eval/README.md).

## 3. Local endpoint and native embedding gates

This lane requires an explicitly started serving runtime, a reviewed local
reference-model directory, a Python environment containing the reference ML
runtime, and GPU capacity. Rust owns HTTP, comparison, verdict, and create-only
reporting; Python owns only local reference inference.

```sh
target/release/kio-eval u7 \
  --base-url http://127.0.0.1:8000 \
  --model "my-embedding-model" \
  --reference-python /absolute/path/to/reference-venv/bin/python3 \
  --reference-adapter /absolute/path/to/kio/eval/u7/reference_adapter.py \
  --reference-model /absolute/path/to/pinned-local-model \
  --text "same-space text control" \
  --image /absolute/path/to/control-image.png \
  --out /absolute/new/u7-report.json
jq -e '.verdict.adoptable == true and .verdict.reason == "both-agree"' \
  /absolute/new/u7-report.json
```

`image-diverged`, `harness-suspect`, `image-not-measured`, malformed vectors,
or a nonzero command rejects/blocks adoption. This command must not download a
model implicitly. Full operating details remain in [the U7 manual](../eval/u7/README.md).

The native EG2 smoke runs through the packaged command using the official
model and pinned LiteRT-LM bundle.

```sh
KIO_LITERT_C_API_ZIP=/absolute/path/to/c_api.zip \
KIO_EG2_TEST_MODEL=/absolute/path/to/embeddinggemma-2-text-vision-440m.litertlm \
cargo +1.98.0 test -p kio-cli --locked --test v1_free_local_embedding \
  default_eg2_publishes_native_text_and_images_and_searches_offline -- --ignored
```

## 4. OCR fixture and paid provider gates

Fixture rendering is the sole Python-native OCR boundary and receives no
credentials. Inputs and outputs are explicit absolute paths and create-only.

```sh
target/release/kio-eval ocr render \
  --python /absolute/path/to/renderer-venv/bin/python3 \
  --adapter /absolute/path/to/kio/experiments/ocr-verification/fixtures/render_native.py \
  --request-id fixture-001 \
  --image /absolute/path/to/page-001.png \
  --out /absolute/new/fixture.pdf
```

The paid provider lane is a separate explicit operation. It requires network,
a valid `MISTRAL_API_KEY`, and cost approval immediately before execution.
Normal local gates and CI must not run it.

```sh
MISTRAL_API_KEY=... target/release/kio-eval ocr provider \
  --document /absolute/path/to/fixture.pdf \
  --model mistral-ocr-4-1 \
  --request-id paid-001 \
  --out /absolute/new/ocr-response.json
target/release/kio-eval ocr evaluate \
  --ground-truth /absolute/path/to/ground-truth.json \
  --response /absolute/new/ocr-response.json \
  --out /absolute/new/ocr-report.json
jq -e '.verdict == "passed"' /absolute/new/ocr-report.json
```

The provider is one fixed direct-HTTP POST with no retry, redirect, or proxy
inheritance. Timeout, status/header/schema errors, document replacement,
create-only collision, or a rejected evaluation is a failure. See
[the OCR boundary manual](../experiments/ocr-verification/README.md).

## Deliberate exclusions

- No nightly/weekly Actions schedule is enabled by this runbook.
- No paid API, GPU/model runtime, or external fixture is implied by a normal
  local or push/PR gate.
- No physical prune, non-tree CAS reclamation, CoW GC, M9, deployment, or
  publication command is included.
- Operational snapshot, batch evidence, retarget, dry-run GC, purge, fsck, and
  recovery commands remain in [the operations manual](../docs/10-operations.md);
  this file does not create a second entry point for them.
