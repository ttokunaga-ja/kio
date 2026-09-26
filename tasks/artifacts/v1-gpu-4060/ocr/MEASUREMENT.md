# Measured result

The fixed public PDF completed two deterministic `POST /layout-parsing` calls.
The response contained one page, one block, and zero returned Markdown images;
this does not establish image-path behavior.

The first ready run returned HTTP 500 because PaddleX requests 4096 completion
tokens plus a 14-token prompt while `max_model_len` was also 4096. The retained
bounded trace records that incompatibility and `oom=false` for both containers.
Changing only the resource-related context ceiling to 4160 fixed it. Peak VRAM
was 6716 MiB, 6141 MiB above the 575 MiB baseline, and returned to baseline
after Compose teardown.

`layout-weights.inventory` on the remote run recorded PP-DocLayoutV3,
UVDoc, PP-LCNet document-orientation weights, and PaddleOCR-VL weights. The
required VLM `model.safetensors` digest matched the pinned value.
