#!/usr/bin/env bash
# Sequential controller for the measured kio-lab GPU services; run as kio-test.
set -euo pipefail
umask 077

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
TOOLS="$SCRIPT_DIR/kio-acceptance-tools"
ROOT=${KIO_GPU_ROOT:-/var/lib/kio-v1-local/state}
OCR_PROJECT=kio-v1-local-gpu-ocr
OCR_WEIGHT=85a479d506a11e724e7285d395c551be69f41dbc16b6342d3cacfb189aed71db
OCR_API_IMAGE=ccr-2vdh3abv-pub.cnc.bj.baidubce.com/paddlepaddle/paddleocr-vl@sha256:6c735bdf9e758ffdd58ccc067db0c2d84e37e5e6a2cbd47156069d4d7ea5d709
OCR_VLM_IMAGE=ccr-2vdh3abv-pub.cnc.bj.baidubce.com/paddlepaddle/paddleocr-genai-vllm-server@sha256:d0d32c04a2119613d25a0a4c292e165ccc107954b74580613cf59e378037f8f5

die() { printf 'error: %s\n' "$*" >&2; exit 1; }
note() { printf '%s\n' "$*"; }

inside_root() {
  local path
  path=$(realpath -m -- "$1")
  [[ "$path" == "$ROOT" || "$path" == "$ROOT"/* ]] || die "path escapes controller root: $1"
}

ensure_root() {
  [ ! -L "$ROOT" ] || die "controller root may not be a symlink"
  if [ -e "$ROOT" ]; then
    [ -d "$ROOT" ] && [ "$(stat -c '%u:%a' "$ROOT")" = "$(id -u):700" ] || die "controller root must be a private user-owned directory"
  else
    mkdir -m 700 -- "$ROOT"
  fi
}

private_dir() {
  inside_root "$1"
  [ ! -L "$1" ] || die "symlink forbidden: $1"
  [ -d "$1" ] || mkdir -m 700 -- "$1"
  [ "$(stat -c '%u:%a' "$1")" = "$(id -u):700" ] || die "private directory required: $1"
}

private_file() {
  inside_root "$1"
  [ ! -L "$1" ] && [ -f "$1" ] && [ "$(stat -c '%u:%a' "$1")" = "$(id -u):600" ] || die "private regular file required: $1"
}

lock() {
  ensure_root
  exec 9>"$ROOT/.controller.lock"
  flock -n 9 || die "controller is busy"
  chmod 600 "$ROOT/.controller.lock"
}

docker() {
  unset DOCKER_HOST DOCKER_CONTEXT
  command docker --host "unix:///run/user/$(id -u)/kio-v1-local-docker.sock" \
    --config /opt/kio-v1-local/docker-cli "$@"
}

compose() { docker compose --project-name "$1" --file "$2" "${@:3}"; }
project_ids() {
  local ids
  ids=$(docker ps "$@" -q) || die "cannot inspect Docker project state"
  printf '%s' "$ids"
}
project_gone() {
  local ids
  ids=$(project_ids -a --filter "label=com.docker.compose.project=$1") || die "cannot inspect stopped project"
  [ -z "$ids" ]
}
project_running() {
  local ids
  ids=$(project_ids --filter "label=com.docker.compose.project=$1") || die "cannot inspect running project"
  [ -n "$ids" ]
}

gpu_command() {
  if command -v nvidia-smi >/dev/null; then
    command -v nvidia-smi
  elif [ -x /usr/lib/wsl/lib/nvidia-smi ]; then
    printf '%s\n' /usr/lib/wsl/lib/nvidia-smi
  else
    return 1
  fi
}

gpu_sample() {
  timeout 5s "$(gpu_command)" --query-gpu=uuid,memory.total,memory.used,memory.free --format=csv,noheader,nounits
}

gpu_memory() {
  local sample
  sample=$(gpu_sample) || die "cannot read GPU memory"
  printf '%s\n' "$sample" | "$TOOLS" gpu-memory "$@" --state-dir "$ROOT/state"
}

ocr_project_gone() {
  project_gone "$OCR_PROJECT" || die "owned OCR project is not absent"
}

shared_gpu_guard() {
  local count
  count=$(sudo -n -- /var/lib/kio-v1-local/shared-gpu-count) ||
    die "cannot inspect shared Docker GPU containers"
  [[ $count =~ ^(0|[1-9][0-9]*)$ ]] || die "invalid shared Docker GPU count"
  [[ $count == 0 ]] || die "shared Docker GPU container is running (count: $count)"
}

gpu_guard() {
  local id project ids
  shared_gpu_guard
  ids=$(project_ids) || die "cannot enumerate dedicated Docker containers"
  while IFS= read -r id; do
    [ -n "$id" ] || continue
    project=$(docker inspect -f '{{index .Config.Labels "com.docker.compose.project"}}' "$id") || die "cannot inspect container $id"
    [ "$project" = "$OCR_PROJECT" ] || die "unrelated container is running on the dedicated engine: $id"
  done <<< "$ids"
}

wait_gone() {
  local project=$1 compose_file=$2
  for _ in $(seq 1 60); do
    project_gone "$project" "$compose_file" && return 0
    sleep 1
  done
  die "owned containers did not stop within 60 seconds"
}

wait_vram() {
  local deadline=$((SECONDS + 60)) remaining
  ocr_project_gone
  while [ "$SECONDS" -lt "$deadline" ]; do
    remaining=$((deadline - SECONDS))
    # Bound the sample producer as well as the helper's stdin reader.
    if timeout "${remaining}s" bash -c '
      set -euo pipefail
      sample=$("$1" --query-gpu=uuid,memory.total,memory.used,memory.free --format=csv,noheader,nounits)
      printf "%s\n" "$sample" | "$2" gpu-memory check-recovered --state-dir "$3" "${@:4}"
    ' bash "$(gpu_command)" "$TOOLS" "$ROOT/state" "$@"; then
      ocr_project_gone
      return 0
    fi
    sleep 1
  done
  die "GPU memory did not recover to the captured baseline plus 128 MiB within 60 seconds"
}

project_exited() {
  local project=$1 compose_file=$2 id state
  while IFS= read -r id; do
    [ -n "$id" ] || continue
    state=$(docker inspect -f '{{.State.Status}}' "$id") || die "cannot inspect owned container $id"
    case "$state" in
      exited|dead) return 0 ;;
    esac
  done < <(compose "$project" "$compose_file" ps -aq)
  return 1
}

wait_https() {
  local port=$1 project=$2 compose_file=$3 deadline=$((SECONDS + 360))
  while [ "$SECONDS" -lt "$deadline" ]; do
    curl --fail --silent --connect-timeout 2 --max-time 3 --cacert "$ROOT/tls-server/ca-cert.pem" "https://127.0.0.1:${port}/health" >/dev/null && return 0
    project_exited "$project" "$compose_file" && return 1
    sleep 1
  done
  return 1
}

sha() { sha256sum "$1" | awk '{print $1}'; }
sha256_id() { printf 'sha256:%s\n' "$(sha "$1")"; }

copy_once() {
  local source=$1 target=$2 mode=${3:-600}
  case "$mode" in 600|700) ;; *) die "invalid staging mode" ;; esac
  [ ! -L "$source" ] && [ -f "$source" ] && [ "$(stat -c '%h' "$source")" = 1 ] || die "invalid source: $source"
  if [ -e "$target" ]; then
    [ ! -L "$target" ] && [ "$(stat -c '%u:%a:%h' "$target")" = "$(id -u):${mode}:1" ] && [ "$(sha "$source")" = "$(sha "$target")" ] || die "refusing to overwrite staged file: $target"
  else
    install -m "$mode" -- "$source" "$target"
  fi
}

verify_ocr_weight() {
  local actual
  actual=$(compose "$OCR_PROJECT" "$ROOT/ocr/compose.yaml" exec -T paddleocr-vlm-server sha256sum /home/paddleocr/.paddlex/official_models/PaddleOCR-VL-1.6/model.safetensors | awk '{print $1}')
  [ "$actual" = "$OCR_WEIGHT" ] || die "OCR model hash differs from measured value"
}

stage_ocr_compose() {
  local target="$ROOT/ocr/compose.yaml"
  if [ -e "$target" ]; then
    [ ! -L "$target" ] || die "OCR compose may not be a symlink"
    grep -Fq '127.0.0.1:18443:8443' "$target" || die "OCR TLS mapping differs"
    grep -Fq './kio-acceptance-tools:/opt/kio/kio-acceptance-tools:ro' "$target" || die "OCR proxy mount differs"
    grep -Fq './ocr_api_entrypoint.sh:/opt/kio/ocr_api_entrypoint.sh:ro' "$target" || die "OCR entrypoint mount differs"
    grep -Fq '../tls-server:/tls:ro' "$target" || die "OCR TLS mount differs"
    return 0
  fi
  awk '
    $0 == "      - 127.0.0.1:18080:8080" {
      print "      - 127.0.0.1:18443:8443"
      next
    }
    $0 == "    command: /bin/bash -c \"paddlex --serve --pipeline /home/paddleocr/pipeline_config_vllm.yaml\"" {
      print "    volumes:"
      print "      - ./kio-acceptance-tools:/opt/kio/kio-acceptance-tools:ro"
      print "      - ./ocr_api_entrypoint.sh:/opt/kio/ocr_api_entrypoint.sh:ro"
      print "      - ../tls-server:/tls:ro"
      print "    command: /bin/bash /opt/kio/ocr_api_entrypoint.sh"
      next
    }
    { print }
  ' "$SCRIPT_DIR/config/compose.yaml" > "$target"
  chmod 600 "$target"
}

identity_path() { printf '%s/state/identity.json' "$ROOT"; }
identity_sha() { sha "$(identity_path)"; }

identity_binding() {
  "$TOOLS" gpu-identity "$@" \
    --state-dir "$ROOT/state" --controller "$SCRIPT_DIR/gpu-phase.sh" \
    --proxy "$TOOLS" --entrypoint "$SCRIPT_DIR/ocr_api_entrypoint.sh" \
    --ocr-compose "$ROOT/ocr/compose.yaml" \
    --backend-config "$ROOT/ocr/backend-config.yaml" \
    --ca-cert "$ROOT/tls-server/ca-cert.pem" --leaf-cert "$ROOT/tls-server/server-cert.pem" \
    --ocr-image "$OCR_API_IMAGE" --ocr-revision PaddleOCR-VL-1.6 --ocr-weight-sha256 "sha256:$OCR_WEIGHT"
}

write_identity() {
  identity_binding create --run-nonce "$(openssl rand -hex 16)"
}

verify_identity() {
  private_file "$(identity_path)"
  private_file "$ROOT/tls-authority/ca-cert.pem"
  private_file "$ROOT/tls-server/server-key.pem"
  private_file "$ROOT/tls-server/server-cert.pem"
  private_file "$ROOT/tls-server/ca-cert.pem"
  openssl verify -CAfile "$ROOT/tls-server/ca-cert.pem" "$ROOT/tls-server/server-cert.pem" >/dev/null
  openssl x509 -in "$ROOT/tls-server/server-cert.pem" -noout -ext subjectAltName | grep -Fq 'IP Address:127.0.0.1, IP Address:0:0:0:0:0:0:0:1'
  identity_binding check --staged-proxy "$ROOT/ocr/kio-acceptance-tools" --staged-entrypoint "$ROOT/ocr/ocr_api_entrypoint.sh"
}

assert_container_image() {
  local container=$1 expected=$2 image_id repo_digests
  image_id=$(docker inspect -f '{{.Image}}' "$container") || die "cannot inspect running container"
  repo_digests=$(docker image inspect -f '{{range .RepoDigests}}{{println .}}{{end}}' "$image_id") || die "cannot inspect running image"
  grep -Fxq "$expected" <<< "$repo_digests" || die "running image differs from measured digest: $container"
  printf '%s\n' "$image_id"
}

write_phase_evidence() {
  local phase=$1 first_image=$2 second_image=${3:-}
  local extra=()
  if [ -n "$second_image" ]; then extra=(--second-image-id "$second_image"); fi
  "$TOOLS" gpu-identity observe --state-dir "$ROOT/state" --phase "$phase" \
    --first-image-id "$first_image" "${extra[@]}"
}

init() {
  local name
  type -P docker >/dev/null || die "missing command: docker"
  for name in openssl curl timeout; do command -v "$name" >/dev/null || die "missing command: $name"; done
  [ -x "$TOOLS" ] && [ ! -L "$TOOLS" ] || die "reviewed acceptance tools binary is missing"
  gpu_command >/dev/null || die "missing nvidia-smi"
  ensure_root
  ocr_project_gone
  for name in state tls-authority tls-server ocr; do private_dir "$ROOT/$name"; done
  ocr_project_gone
  gpu_memory capture

  if [ ! -e "$ROOT/tls-authority/ca-key.pem" ]; then
    openssl req -x509 -newkey rsa:4096 -nodes -sha256 -days 7 -subj /CN=kio-v1-local-gpu-ca -keyout "$ROOT/tls-authority/ca-key.pem" -out "$ROOT/tls-authority/ca-cert.pem"
    chmod 600 "$ROOT/tls-authority/"*.pem
  fi
  private_file "$ROOT/tls-authority/ca-key.pem"
  private_file "$ROOT/tls-authority/ca-cert.pem"

  if [ ! -e "$ROOT/tls-server/server-key.pem" ]; then
    openssl req -newkey rsa:2048 -nodes -subj /CN=localhost -addext 'subjectAltName=IP:127.0.0.1,IP:::1,DNS:localhost' -keyout "$ROOT/tls-server/server-key.pem" -out "$ROOT/tls-server/server.csr"
    openssl x509 -req -sha256 -days 7 -in "$ROOT/tls-server/server.csr" -CA "$ROOT/tls-authority/ca-cert.pem" -CAkey "$ROOT/tls-authority/ca-key.pem" -CAcreateserial -extfile <(printf 'subjectAltName=IP:127.0.0.1,IP:::1,DNS:localhost\n') -out "$ROOT/tls-server/server-cert.pem"
    rm -f -- "$ROOT/tls-server/server.csr" "$ROOT/tls-authority/ca-cert.srl"
    cp "$ROOT/tls-authority/ca-cert.pem" "$ROOT/tls-server/ca-cert.pem"
    chmod 600 "$ROOT/tls-server/"*.pem
  fi
  for name in server-key.pem server-cert.pem ca-cert.pem; do private_file "$ROOT/tls-server/$name"; done
  openssl verify -CAfile "$ROOT/tls-server/ca-cert.pem" "$ROOT/tls-server/server-cert.pem" >/dev/null
  openssl x509 -in "$ROOT/tls-server/server-cert.pem" -noout -ext subjectAltName | grep -Fq 'IP Address:127.0.0.1, IP Address:0:0:0:0:0:0:0:1'

  copy_once "$SCRIPT_DIR/config/backend-config.yaml" "$ROOT/ocr/backend-config.yaml"
  copy_once "$TOOLS" "$ROOT/ocr/kio-acceptance-tools" 700
  copy_once "$SCRIPT_DIR/ocr_api_entrypoint.sh" "$ROOT/ocr/ocr_api_entrypoint.sh"
  stage_ocr_compose
  write_identity
  note "initialized $ROOT; no service started"
}

status() {
  gpu_guard
  note "GPU UUID, total, used, free MiB: $(gpu_sample || die 'cannot read GPU memory')"
  compose "$OCR_PROJECT" "$ROOT/ocr/compose.yaml" ps || true
}

start_ocr() {
  local api_id vlm_id
  lock
  [ ! -e "$ROOT/state/ocr-observed.json" ] && [ ! -L "$ROOT/state/ocr-observed.json" ] || die "OCR already observed; use a fresh controller root"
  gpu_guard
  [ -f "$ROOT/ocr/compose.yaml" ] || die "run init first"
  verify_identity
  ocr_project_gone
  gpu_memory check-start
  compose "$OCR_PROJECT" "$ROOT/ocr/compose.yaml" up -d
  wait_https 18443 "$OCR_PROJECT" "$ROOT/ocr/compose.yaml" || { compose "$OCR_PROJECT" "$ROOT/ocr/compose.yaml" down; die "OCR HTTPS endpoint was not ready"; }
  api_id=$(assert_container_image "$(compose "$OCR_PROJECT" "$ROOT/ocr/compose.yaml" ps -q paddleocr-vl-api)" "$OCR_API_IMAGE")
  vlm_id=$(assert_container_image "$(compose "$OCR_PROJECT" "$ROOT/ocr/compose.yaml" ps -q paddleocr-vlm-server)" "$OCR_VLM_IMAGE")
  verify_ocr_weight
  write_phase_evidence ocr "$api_id" "$vlm_id"
  note "OCR ready at https://127.0.0.1:18443"
}

stop_ocr() {
  lock
  shared_gpu_guard
  compose "$OCR_PROJECT" "$ROOT/ocr/compose.yaml" down
  wait_gone "$OCR_PROJECT" "$ROOT/ocr/compose.yaml"
  wait_vram
}

cleanup_project() {
  local project=$1 compose_file=$2
  if [ -e "$compose_file" ] || [ -L "$compose_file" ]; then
    private_file "$compose_file"
    compose "$project" "$compose_file" down || die "owned project cleanup failed"
  else
    # An interrupted init may not have published its compose file. It is safe
    # only when a successful Docker query proves this project has no containers.
    project_gone "$project" || die "owned project has containers but no verified compose file"
  fi
  wait_gone "$project" "$compose_file"
}

cleanup() {
  lock
  shared_gpu_guard
  cleanup_project "$OCR_PROJECT" "$ROOT/ocr/compose.yaml"
  private_dir "$ROOT/state"
  wait_vram --allow-missing
  note "owned services stopped"
}

case ${1:-} in
  init) lock; gpu_guard; init ;;
  status) lock; status ;;
  start-ocr) start_ocr ;;
  stop-ocr) stop_ocr ;;
  cleanup) cleanup ;;
  *) die "usage: init|status|start-ocr|stop-ocr|cleanup" ;;
esac
