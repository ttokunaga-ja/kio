#!/usr/bin/env bash
set -euo pipefail

root=/home/kio-test/work/kio/v1-ocr-r32
logs=/home/kio-test/logs/kio-v1-ocr-r32
project=kio-v1-ocr-r32
compose=(docker compose -p "$project" -f "$root/compose.yaml")
gpu_smi=/usr/lib/wsl/lib/nvidia-smi

mkdir -p "$logs"
exec > >(tee -a "$logs/runner.log") 2>&1

snapshot() {
  local label=$1
  {
    printf '=== %s %s ===\n' "$label" "$(date -u +%FT%TZ)"
    free -b | awk '/Mem:/ {print "mem_total=" $2 " mem_used=" $3 " mem_available=" $7} /Swap:/ {print "swap_total=" $2 " swap_used=" $3 " swap_free=" $4}'
    df -B1 "$root" | awk 'NR==2 {print "disk_total=" $2 " disk_used=" $3 " disk_available=" $4}'
    df -B1 /mnt/c | awk 'NR==2 {print "physical_c_total=" $2 " physical_c_used=" $3 " physical_c_available=" $4}'
    "$gpu_smi" --query-gpu=name,memory.total,memory.used,memory.free --format=csv,noheader,nounits | awk -F, '{gsub(/^ +| +$/, "", $1); gsub(/^ +| +$/, "", $2); gsub(/^ +| +$/, "", $3); gsub(/^ +| +$/, "", $4); print "gpu_name=" $1 " gpu_memory_total_mib=" $2 " gpu_memory_used_mib=" $3 " gpu_memory_free_mib=" $4}'
  } | tee -a "$logs/resources.log"
}

sampler_pid=''
cleanup() {
  set +e
  if [[ -n "$sampler_pid" ]]; then kill "$sampler_pid" 2>/dev/null; wait "$sampler_pid" 2>/dev/null; fi
  "${compose[@]}" logs --no-color --tail 300 > "$logs/compose-before-down.log" 2>&1
  "${compose[@]}" ps --all > "$logs/compose-before-down-ps.txt" 2>&1
  docker inspect $("${compose[@]}" ps --all -q) --format '{{.Name}} oom={{.State.OOMKilled}} exit={{.State.ExitCode}} error={{.State.Error}}' > "$logs/container-before-down-state.txt" 2>&1
  "${compose[@]}" down --remove-orphans >>"$logs/compose-down.log" 2>&1
  snapshot after_down
  docker ps --filter "label=com.docker.compose.project=$project" --format '{{.Names}} {{.Status}}' | tee "$logs/remaining-containers.txt"
}
trap cleanup EXIT

snapshot before_pull
"${compose[@]}" config > "$logs/compose.rendered.yaml"
if "${compose[@]}" config --images | while IFS= read -r image; do docker image inspect "$image" >/dev/null 2>&1 || exit 1; done; then
  echo images_already_present_by_pinned_digest | tee "$logs/compose-pull.log"
else
  "${compose[@]}" pull 2>&1 | tee "$logs/compose-pull.log"
fi
snapshot after_pull

"${compose[@]}" run --rm --no-deps paddleocr-vlm-server paddleocr genai_server --help > "$logs/genai-server-help.txt" 2>&1 || true
grep -F -- '--backend_config' "$logs/genai-server-help.txt" >/dev/null || { echo "missing_backend_config_option" | tee -a "$logs/protocol-summary.txt"; exit 2; }
for key in trust_remote_code gpu_memory_utilization max_model_len max_num_batched_tokens api_server_count enforce_eager max_num_seqs; do
  grep -E "^${key}:" "$root/backend-config.yaml" >/dev/null || { echo "missing_backend_config_key=$key" | tee -a "$logs/protocol-summary.txt"; exit 2; }
done
echo 'backend_resource_keys_delivered_via_backend_config=true' >> "$logs/protocol-summary.txt"

snapshot before_up
(while :; do snapshot sampled; sleep 5; done) & sampler_pid=$!
"${compose[@]}" up -d 2>&1 | tee "$logs/compose-up.log"
deadline=$((SECONDS + 600))
while (( SECONDS < deadline )); do
  if curl --fail --silent --show-error --max-time 10 http://127.0.0.1:18080/health > "$logs/health.json"; then break; fi
  "${compose[@]}" ps >> "$logs/compose-ps.log"
  sleep 5
done
if (( SECONDS >= deadline )); then
  "${compose[@]}" logs --no-color > "$logs/compose-failure.log" || true
  echo health_timeout | tee -a "$logs/protocol-summary.txt"
  exit 3
fi
snapshot ready

weights=$("${compose[@]}" exec -T paddleocr-vlm-server sh -lc 'find / -type f -name "*.safetensors" -print0 2>/dev/null | xargs -0 -r sha256sum' 2>/dev/null || true)
printf '%s\n' "$weights" > "$logs/weights.sha256"
grep -F '85a479d506a11e724e7285d395c551be69f41dbc16b6342d3cacfb189aed71db' "$logs/weights.sha256" >/dev/null
find_count=$(wc -l < "$logs/weights.sha256" | tr -d ' ')
printf 'safetensors_inventory_count=%s\n' "$find_count" > "$logs/protocol-summary.txt"

fixture=$root/ocr.pdf
request=$logs/request.json
sha256sum "$fixture" > "$logs/fixture.sha256"
python3 - "$fixture" "$request" <<'PY'
import base64, json, pathlib, sys
data = base64.b64encode(pathlib.Path(sys.argv[1]).read_bytes()).decode('ascii')
pathlib.Path(sys.argv[2]).write_text(json.dumps({'file': data, 'fileType': 0, 'useLayoutDetection': True}, separators=(',', ':')))
PY
for run in 1 2; do
  status=$(curl --silent --show-error --max-time 300 -X POST http://127.0.0.1:18080/layout-parsing -H 'Content-Type: application/json' --data-binary @"$request" -o "$logs/response-$run.json" -w '%{http_code}')
  if [[ "$status" != 200 ]]; then
    printf 'layout_parsing_http_status=%s\n' "$status" | tee -a "$logs/protocol-summary.txt"
    head -c 65536 "$logs/response-$run.json" > "$logs/layout-parsing-error-body-$run.txt"
    exit 4
  fi
done
python3 - "$logs/response-1.json" "$logs/response-2.json" "$logs/protocol-summary.txt" <<'PY'
import hashlib, json, pathlib, sys
paths = [pathlib.Path(p) for p in sys.argv[1:3]]
def canonical(data):
    data = dict(data); data.pop('logId', None)
    return json.dumps(data, sort_keys=True, separators=(',', ':')).encode()
summaries=[]
for path in paths:
    data=json.loads(path.read_text())
    result=data.get('result', {})
    pages=result.get('layoutParsingResults', [])
    if data.get('errorCode') != 0:
        raise SystemExit('layout_parsing_error_code=' + repr(data.get('errorCode')))
    if not isinstance(pages, list) or not pages:
        raise SystemExit('missing_or_empty_result_layoutParsingResults')
    blocks=[]
    image_counts=[]
    for p in pages:
        blocks.extend((p.get('prunedResult') or {}).get('parsing_res_list') or [])
        image_counts.append(len((p.get('markdown') or {}).get('images') or {}))
    summaries.append((hashlib.sha256(canonical(data)).hexdigest(), data.get('errorCode'), len(pages), len(blocks), sum(image_counts), sorted(data.keys()), sorted(result.keys())))
if summaries[0][0] != summaries[1][0]: raise SystemExit('non_deterministic_after_logId_exclusion')
out=pathlib.Path(sys.argv[3])
with out.open('a') as f:
    f.write('response_canonical_sha256=' + summaries[0][0] + '\n')
    f.write('response_error_code=' + str(summaries[0][1]) + '\n')
    f.write('response_pages=' + str(summaries[0][2]) + '\n')
    f.write('response_blocks=' + str(summaries[0][3]) + '\n')
    f.write('response_images=' + str(summaries[0][4]) + '\n')
    f.write('response_top_level_keys=' + ','.join(summaries[0][5]) + '\n')
    f.write('response_result_keys=' + ','.join(summaries[0][6]) + '\n')
    f.write('deterministic_after_logId_exclusion=true\n')
for path in paths: path.unlink()
PY
rm -f "$request"
snapshot after_requests
docker inspect $("${compose[@]}" ps -q) --format '{{.Name}} {{json .HostConfig.LogConfig}}' > "$logs/docker-log-config.txt"
