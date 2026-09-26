# RTX 4060 fixed Qwen embedding measurement

This directory contains a one-service GPU measurement. It must remain
sequential with OCR: run it only after the owner of Compose project
`kio-v1-ocr-r32` has stopped that project and the primary agent authorizes it.

## Fixed dependencies

- Server: official `vllm/vllm-openai:v0.26.0`, pinned for the authorized x86_64
  host to Linux/amd64 manifest `sha256:770fe65b2c73ee74a5c42165cf3433de4048cc2cd9c57a937ca4e35aba5aa87b`.
  The official multi-architecture tag manifest is `sha256:ffb2d59b1c059a5bd8d781320c9f5189de8293693b7d95da54befddaa54abf52`.
- Model: `Qwen/Qwen3-VL-Embedding-2B`, full revision `9f2f7e710d6d81056aa5c0a4f04764fec6bb7bda`.
- Weight: exactly one 4,255,140,312-byte `model.safetensors`, SHA-256 `c73fa9caeddeb3ff831d46c085a7a5708343248ca777e90f2d486964464509c1`.
- Existing Kio profile: native 2048 output, adapter MRL truncation and
  renormalization to 768, no Kio-provided system instruction, and one
  chat-form `messages` item at `POST /v1/embeddings`.

The image and revision match the V4 measurement. The amd64 child manifest pins
the actual host bytes; the tag and multi-architecture index alone do not.

## 8 GB first-trial envelope

The service uses half precision, 80% GPU memory utilization, a 2048-token
context and batching-token ceiling, one sequence, and eager execution. The
4.26 GB weight file makes this a conservative starting point, not completed
capacity evidence. On the 8,188 MiB RTX 4060, vLLM's budget is 6,550 MiB and
about 1,638 MiB remains outside it. OCR used 65% with a smaller 0.9B model, so
that prior configuration cannot establish this 2B server's capacity.

The runner samples GPU/RAM/swap/WSL disk and `/mnt/c` physical disk once per
second from startup through all probes, records image and resolved-cache weight
identity, and retains bounded Docker logs (10 MiB x 3). It issues two identical
text messages and two identical image messages using `fixture.png` (public,
1448x1086, SHA-256 `269201dc3761f089c1d01f6c3fb50fae3c3bb5a8db2d4dc9a4da4b6d7eceace9`),
then records native and MRL-768 vector hashes, norms, and pairwise cosine/L2/max-absolute deltas. Bitwise equality is reported, never used as the protocol pass condition: `docs/07-adapter-spec.md` §9 does not require complete Adapter rerun determinism. The four public request/response JSON
pairs and health response are retained for protocol diagnosis; they contain no
credentials or user data. Cleanup records every current or exited container's
state, including `OOMKilled`, and the bounded Docker logs before teardown.

## Local Kio endpoint

The service exposes only `https://127.0.0.1:18081`. The runner creates an
experiment-local seven-day CA in `tls-authority/`; its signing key is never
mounted in the model container. Only the leaf server certificate/key and public
CA are mounted read-only from `tls-server/`. Neither directory is copied into
the repository. Kio requires this HTTPS trust binding, so the direct HTTP vLLM
default is not a valid Kio endpoint.

```toml
[embedding.qwen3_vl_embedding_local]
kind = "offline_api"
url = "https://127.0.0.1:18081"
model = "Qwen/Qwen3-VL-Embedding-2B"

[adapter.policy.offline_api]
ca_pem_path = "/home/kio-test/work/kio/v1-embedding-r32/tls-server/ca-cert.pem"
timeout_seconds = 300
```

This device-private configuration is intentionally not created by preparation;
it is meaningful only while its generated CA and measured server exist.

## First authorized run

The 2026-09-08 run used the command below after the OCR project was confirmed stopped. Its concise public evidence is in `results/attempt1-20260908T0948Z/`; private TLS material, the HF cache, image request bodies, and bounded Docker logs remain only on the experiment host. The service was torn down and GPU use returned to 575 MiB.

For a separately authorized future run, stage this directory at
`/home/kio-test/work/kio/v1-embedding-r32` and invoke:

```bash
cd /home/kio-test/work/kio/v1-embedding-r32 && bash ./run_measurement.sh
```

That command may pull the image, download the public model, create certificates, or start a GPU process, so it still requires the OCR guard and primary authorization.
