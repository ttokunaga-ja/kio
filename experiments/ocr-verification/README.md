# OCR verification boundaries

OCR evaluation is Rust-owned. `kio-eval` parses typed ground truth and OCR
responses, applies table-recall/Japanese-CER/image-count thresholds,
classifies formula output, and creates report data and verdicts. This directory
is not an executable evaluation harness.

The Mistral provider is a Rust direct-HTTP boundary. With an explicitly set
`MISTRAL_API_KEY`, `kio-eval` sends one bounded POST request to
`https://api.mistral.ai/v1/ocr`, using the exact versioned model
`mistral-ocr-4-1`. It does not retry, follow redirects, or inherit proxy
configuration. The official Mistral API documentation is the wire reference;
Kio makes no Python SDK parity promise.

The provider request and response are bounded before parsing or publication.
The normalized response is create-only `kio.ocr.response/v2` and binds the
request ID, document SHA-256, and exact model. Only `image_count` is persisted;
provider image payloads are not retained. Provider page indices must be unique
and strictly increasing. Because official examples and retained responses
differ on whether the first source index is zero or one, Rust converts the
received array order to the normalized artifact's canonical zero-based
`index`; duplicate, decreasing, or missing required fields fail closed.

`render-native.py` remains a distinct Python-native boundary. It uses
Pillow and reportlab to render explicitly named PNG inputs to one explicitly
named create-only PDF. Its request and response schemas are
`kio.ocr.fixture-render.request/v1` and `kio.ocr.fixture-render.response/v1`;
its response binds the final PDF's byte count and SHA-256. Its parent directory
must already exist and output creation is exclusive/nofollow, so it cannot
replace an archive artifact.

Both lanes are manual only: normal push/PR CI makes no live provider call.
`MISTRAL_API_KEY` is used only for the direct provider request and is never
persisted. The renderer receives no credentials.

## Fixed OCR inputs

[fixtures/images/](fixtures/images/) contains the 15 synthetic PNGs and their
[ground truth](fixtures/images/ground-truth.json). The renderer converts those
images to PDF. The [capture manifest](../../crates/kio-adapter/tests/fixtures/layout-parsing/capture-manifest.json)
maps the stored OCR responses to these inputs for the `token_recall` regression test.

## Manual Rust-owned flow

Documents and outputs are explicit canonical absolute paths. A provider call
is a paid, networked manual operation; normal CI never invokes it.

```bash
MISTRAL_API_KEY=... kio-eval ocr provider \
  --document /absolute/path/to/input.pdf \
  --model mistral-ocr-4-1 --request-id manual-001 \
  --out /absolute/path/to/normalized-response.json

kio-eval ocr evaluate \
  --ground-truth /absolute/path/to/ground-truth-v1.json \
  --response /absolute/path/to/normalized-response.json \
  --out /absolute/path/to/evaluation-report.json

kio-eval ocr render \
  --python /absolute/path/to/venv/bin/python3 \
  --adapter /absolute/path/to/kio/experiments/ocr-verification/render-native.py \
  --request-id render-001 \
  --image /absolute/path/to/kio/experiments/ocr-verification/fixtures/images/invoice-photo.png \
  --out /absolute/path/to/rendered.pdf
```

All three outputs are create-only. The provider child receives only
`MISTRAL_API_KEY`; the renderer receives no credentials. Rust owns the
normalized OCR response, threshold verdict, and report schemas. Before the
renderer starts, Rust descriptor-binds every PNG and copies its exact bytes to
a private snapshot directory; the adapter never reopens caller-controlled
input names. Per-image, aggregate-byte, aggregate-pixel, and 64 MiB output
bounds are enforced for renderer inputs and outputs.

Existing PDFs, PNGs, and `out*/` response/report files are retained as
non-authorizing archive artifacts. They are neither fixture-discovery inputs
nor evidence of a current OCR verdict.
