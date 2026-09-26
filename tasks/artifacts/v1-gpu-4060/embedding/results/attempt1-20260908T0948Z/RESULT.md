# Attempt 1 result

The fixed vLLM image and pinned Qwen revision served two identical text and two
identical public-PNG message requests successfully through the loopback TLS
endpoint. Every response had one finite 2,048-component vector. Kio MRL
processing is the adapter's leading-768-component truncation followed by L2
renormalization.

The image pair was bitwise identical. The text pair was not bitwise identical:
native cosine was 0.999998480837 (L2 delta 0.00174307924082; maximum absolute
delta 0.000409446656704), and MRL-768 cosine was 0.999998786911 (L2 delta
0.00155761947058; maximum absolute delta 0.000279609149753). These metrics
are evidence, not a newly invented pass tolerance. `docs/07-adapter-spec.md`
§9 does not require complete Adapter rerun determinism; protocol completion is
therefore recorded separately from bitwise determinism.

The one-second sampler observed peak GPU use of 7,213 MiB, peak WSL memory use
of 7,507,460,096 bytes, and peak swap use of 1,532,710,912 bytes. The sampled
Windows physical C: free space reached 526,079,168,512 bytes. Pre-teardown
container inspection recorded `oom_killed=false`; post-teardown GPU use was
575 MiB with 7,382 MiB free and no project container remaining.

The remote complete evidence archive is
`/home/kio-test/logs/kio-v1-embedding-r32-attempt1-20260908T0948Z`.
