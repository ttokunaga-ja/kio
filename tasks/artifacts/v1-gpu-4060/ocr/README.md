# RTX 4060 fixed OCR measurement

`compose.source.pinned.yaml` is an unmodified preservation copy of the two
digest-pinned source images. `compose.yaml` changes only experiment isolation
and resource sizing: Compose project `kio-v1-ocr-r32`, no global container
names, loopback-only `127.0.0.1:18080`, no restart policy, and local rotating
Docker logs (10 MiB x 3 per service).

The fixed public fixture is
`crates/kio-eval/acceptance-fixtures/v1/provider-public/ocr.pdf`; it is sent
as `fileType: 0` to `POST /layout-parsing` with `useLayoutDetection: true`.
The expected response is an envelope whose `result.layoutParsingResults` is
the page array. The remote runner retains only response canonical hashes and
compact schema/count summaries, never base64 payloads or raw OCR response
artifacts.
