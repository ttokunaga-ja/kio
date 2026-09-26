#!/usr/bin/env bash
set -euo pipefail
# No invocation before OCR project kio-v1-ocr-r32 is stopped and primary authorizes.
root=/home/kio-test/work/kio/v1-embedding-r32; logs=/home/kio-test/logs/kio-v1-embedding-r32
project=kio-v1-embedding-r32; ocr_project=kio-v1-ocr-r32
compose=(docker compose -p "$project" -f "$root/compose.yaml"); gpu_smi=/usr/lib/wsl/lib/nvidia-smi
model_sha=c73fa9caeddeb3ff831d46c085a7a5708343248ca777e90f2d486964464509c1; model_size=4255140312
model_revision=9f2f7e710d6d81056aa5c0a4f04764fec6bb7bda
image_digest=sha256:770fe65b2c73ee74a5c42165cf3433de4048cc2cd9c57a937ca4e35aba5aa87b
cache_root="$root/models"; snapshot_dir="$cache_root/models--Qwen--Qwen3-VL-Embedding-2B/snapshots/$model_revision"
tls_server_dir="$root/tls-server"; tls_authority_dir="$root/tls-authority"; fixture="$root/fixture.png"; sampler_pid=''
mkdir -p "$logs" "$cache_root" "$tls_server_dir" "$tls_authority_dir"; chmod 700 "$tls_server_dir" "$tls_authority_dir"
exec > >(tee -a "$logs/runner.log") 2>&1
snapshot() { local label=$1; { printf '=== %s %s ===\n' "$label" "$(date -u +%FT%TZ)"; free -b | awk '/Mem:/ {print "mem_total=" $2 " mem_used=" $3 " mem_available=" $7} /Swap:/ {print "swap_total=" $2 " swap_used=" $3 " swap_free=" $4}'; df -B1 "$root" | awk 'NR==2 {print "wsl_disk_total=" $2 " wsl_disk_used=" $3 " wsl_disk_available=" $4}'; if [[ -d /mnt/c ]]; then df -B1 /mnt/c | awk 'NR==2 {print "windows_c_disk_total=" $2 " windows_c_disk_used=" $3 " windows_c_disk_available=" $4}'; fi; "$gpu_smi" --query-gpu=name,memory.total,memory.used,memory.free --format=csv,noheader,nounits | awk -F, '{gsub(/^ +| +$/, "", $1); gsub(/^ +| +$/, "", $2); gsub(/^ +| +$/, "", $3); gsub(/^ +| +$/, "", $4); print "gpu_name=" $1 " gpu_memory_total_mib=" $2 " gpu_memory_used_mib=" $3 " gpu_memory_free_mib=" $4}'; } | tee -a "$logs/resources.log"; }
assert_ocr_stopped() { docker ps --filter "label=com.docker.compose.project=$ocr_project" --format '{{.ID}} {{.Names}} {{.Status}}' > "$logs/ocr-active-containers.txt"; if [[ -s "$logs/ocr-active-containers.txt" ]]; then cat "$logs/ocr-active-containers.txt"; echo "ocr_project_active=$ocr_project"; exit 4; fi; }
record_container_state() { mapfile -t container_ids < <("${compose[@]}" ps --all -q); if (( ${#container_ids[@]} > 0 )); then docker inspect "${container_ids[@]}" --format '{{.Name}} running={{.State.Running}} oom_killed={{.State.OOMKilled}} exit_code={{.State.ExitCode}} error={{.State.Error}} log={{json .HostConfig.LogConfig}}' > "$logs/container-state.txt"; fi; }
cleanup() { set +e; if [[ -n "$sampler_pid" ]]; then kill "$sampler_pid" 2>/dev/null; wait "$sampler_pid" 2>/dev/null; fi; record_container_state; "${compose[@]}" logs --no-color > "$logs/compose-final.log" 2>&1; "${compose[@]}" down --remove-orphans >>"$logs/compose-down.log" 2>&1; snapshot after_down; docker ps --filter "label=com.docker.compose.project=$project" --format '{{.Names}} {{.Status}}' | tee "$logs/remaining-containers.txt"; }
trap cleanup EXIT; trap 'exit 130' INT; trap 'exit 143' TERM
make_tls() { if [[ -s "$tls_authority_dir/ca-key.pem" && -s "$tls_server_dir/ca-cert.pem" && -s "$tls_server_dir/server-cert.pem" && -s "$tls_server_dir/server-key.pem" ]]; then return; fi; umask 077; openssl req -x509 -newkey rsa:3072 -nodes -days 7 -keyout "$tls_authority_dir/ca-key.pem" -out "$tls_authority_dir/ca-cert.pem" -subj '/CN=kio-v1-embedding-r32 local CA'; openssl req -newkey rsa:3072 -nodes -keyout "$tls_server_dir/server-key.pem" -out "$tls_authority_dir/server.csr" -subj '/CN=127.0.0.1'; openssl x509 -req -days 7 -in "$tls_authority_dir/server.csr" -CA "$tls_authority_dir/ca-cert.pem" -CAkey "$tls_authority_dir/ca-key.pem" -CAcreateserial -out "$tls_server_dir/server-cert.pem" -extfile <(printf 'subjectAltName=IP:127.0.0.1'); cp "$tls_authority_dir/ca-cert.pem" "$tls_server_dir/ca-cert.pem"; chmod 600 "$tls_authority_dir/ca-key.pem" "$tls_server_dir/server-key.pem"; }
verify_weight() { [[ -d "$snapshot_dir" && -L "$snapshot_dir/model.safetensors" ]]; model_path=$(python3 - "$cache_root" "$snapshot_dir/model.safetensors" "$model_size" <<'PY'
import pathlib,sys
cache=pathlib.Path(sys.argv[1]).resolve(strict=True); weight=pathlib.Path(sys.argv[2]).resolve(strict=True)
if not weight.is_file() or weight.stat().st_size != int(sys.argv[3]): raise SystemExit('invalid_pinned_weight')
try: weight.relative_to(cache)
except ValueError: raise SystemExit('weight_target_escapes_owned_cache')
print(weight)
PY
); printf 'model_revision=%s\nmodel_snapshot_dir=%s\nmodel_weight_resolved_path=%s\n' "$model_revision" "$snapshot_dir" "$model_path" | tee "$logs/model-identity.txt"; sha256sum "$model_path" | tee "$logs/weights.sha256"; grep -F "$model_sha" "$logs/weights.sha256" >/dev/null; }
probe() { local modality=$1 request=$2 response=$3; curl --fail --silent --show-error --cacert "$tls_server_dir/ca-cert.pem" --max-time 300 -H 'Content-Type: application/json' --data-binary @"$request" https://127.0.0.1:18081/v1/embeddings > "$response"; snapshot "after_${modality}_request"; }
assert_ocr_stopped; snapshot before_pull; "${compose[@]}" config > "$logs/compose.rendered.yaml"; "${compose[@]}" pull 2>&1 | tee "$logs/compose-pull.log"
docker image inspect "vllm/vllm-openai@$image_digest" --format '{{json .RepoDigests}} {{.Id}} {{.Architecture}} {{.Os}} {{.Created}}' | tee "$logs/image-identity.txt"; grep -F "$image_digest" "$logs/image-identity.txt" >/dev/null; grep -F 'amd64 linux' "$logs/image-identity.txt" >/dev/null
assert_ocr_stopped; "${compose[@]}" run --rm --no-deps --entrypoint vllm qwen3-vl-embedding serve --help=all > "$logs/vllm-serve-help.txt" 2>&1
for arg in --runner --revision --gpu-memory-utilization --max-model-len --max-num-batched-tokens --max-num-seqs --enforce-eager --ssl-keyfile --ssl-certfile; do grep -F -- "$arg" "$logs/vllm-serve-help.txt" >/dev/null || { echo "unsupported_required_argument=$arg" | tee -a "$logs/protocol-summary.txt"; exit 2; }; done
make_tls; assert_ocr_stopped; snapshot before_up; (while :; do snapshot sampled; sleep 1; done) & sampler_pid=$!; "${compose[@]}" up -d 2>&1 | tee "$logs/compose-up.log"
deadline=$((SECONDS + 600)); while (( SECONDS < deadline )); do if curl --fail --silent --show-error --cacert "$tls_server_dir/ca-cert.pem" --max-time 10 https://127.0.0.1:18081/health > "$logs/health.json"; then break; fi; "${compose[@]}" ps >> "$logs/compose-ps.log"; sleep 5; done
if (( SECONDS >= deadline )); then "${compose[@]}" logs --no-color > "$logs/compose-failure.log" || true; echo health_timeout | tee -a "$logs/protocol-summary.txt"; exit 3; fi
snapshot ready; verify_weight; [[ -s "$fixture" ]]
python3 - "$fixture" "$logs/text-1.json" "$logs/text-2.json" "$logs/image-1.json" "$logs/image-2.json" <<'PY'
import base64,json,pathlib,sys
raw=pathlib.Path(sys.argv[1]).read_bytes(); text={"model":"Qwen/Qwen3-VL-Embedding-2B","encoding_format":"float","messages":[{"role":"user","content":[{"type":"text","text":"kio embedding protocol control"}]}]}; image={"model":"Qwen/Qwen3-VL-Embedding-2B","encoding_format":"float","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,"+base64.b64encode(raw).decode()}}]}]}
for path,body in zip(sys.argv[2:],(text,text,image,image)): pathlib.Path(path).write_text(json.dumps(body,separators=(',',':')))
PY
probe text "$logs/text-1.json" "$logs/text-1.response.json"; probe text "$logs/text-2.json" "$logs/text-2.response.json"; probe image "$logs/image-1.json" "$logs/image-1.response.json"; probe image "$logs/image-2.json" "$logs/image-2.response.json"
python3 - "$logs/protocol-summary.txt" "$logs/text-1.response.json" "$logs/text-2.response.json" "$logs/image-1.response.json" "$logs/image-2.response.json" <<'PY'
import hashlib,json,math,pathlib,sys
summary,*paths=map(pathlib.Path,sys.argv[1:]); rows=[]
def canonical_hash(vector):
 return hashlib.sha256(json.dumps(vector,separators=(',',':'),allow_nan=False).encode()).hexdigest()
def compare(left,right):
 dot=sum(a*b for a,b in zip(left,right)); ln=math.sqrt(sum(v*v for v in left)); rn=math.sqrt(sum(v*v for v in right))
 return max(abs(a-b) for a,b in zip(left,right)), math.sqrt(sum((a-b)*(a-b) for a,b in zip(left,right))), dot/(ln*rn)
for path in paths:
 body=json.loads(path.read_text()); data=body.get('data');
 if not isinstance(data,list) or len(data)!=1 or not isinstance(data[0].get('embedding'),list): raise SystemExit('invalid_embedding_response_shape')
 vector=data[0]['embedding'];
 if len(vector)!=2048 or not all(isinstance(v,(int,float)) and math.isfinite(v) for v in vector): raise SystemExit('unexpected_embedding_vector')
 native_norm=math.sqrt(sum(v*v for v in vector))
 prefix=vector[:768]; prefix_norm=math.sqrt(sum(v*v for v in prefix))
 if native_norm == 0 or prefix_norm == 0: raise SystemExit('zero_embedding_vector')
 mrl=[v/prefix_norm for v in prefix]
 rows.append((vector, native_norm, prefix_norm, mrl))
with summary.open('a') as out:
 for label,(vector,native_norm,prefix_norm,mrl) in zip(('text_1','text_2','image_1','image_2'),rows):
  out.write(f'{label}_native_vector_sha256={canonical_hash(vector)}\n{label}_native_vector_l2={native_norm:.9f}\n')
  out.write(f'{label}_mrl768_vector_sha256={canonical_hash(mrl)}\n{label}_mrl768_pre_normalize_l2={prefix_norm:.9f}\n{label}_mrl768_vector_l2={math.sqrt(sum(v*v for v in mrl)):.9f}\n')
 for label,left,right in (('text',rows[0],rows[1]),('image',rows[2],rows[3])):
  raw_max,raw_l2,raw_cos=compare(left[0],right[0]); mrl_max,mrl_l2,mrl_cos=compare(left[3],right[3])
  out.write(f'{label}_native_bitwise_deterministic={str(canonical_hash(left[0]) == canonical_hash(right[0])).lower()}\n')
  out.write(f'{label}_native_pair_max_abs_delta={raw_max:.12g}\n{label}_native_pair_l2_delta={raw_l2:.12g}\n{label}_native_pair_cosine={raw_cos:.12g}\n')
  out.write(f'{label}_mrl768_bitwise_deterministic={str(canonical_hash(left[3]) == canonical_hash(right[3])).lower()}\n')
  out.write(f'{label}_mrl768_pair_max_abs_delta={mrl_max:.12g}\n{label}_mrl768_pair_l2_delta={mrl_l2:.12g}\n{label}_mrl768_pair_cosine={mrl_cos:.12g}\n')
 out.write('embedding_native_dimensions=2048\nembedding_mrl_dimensions=768\nprotocol_complete=true\n')
PY
record_container_state; grep -F 'oom_killed=true' "$logs/container-state.txt" && exit 5 || true
