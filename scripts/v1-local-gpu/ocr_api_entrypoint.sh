#!/usr/bin/env bash
# Run both OCR API processes in one container namespace and stop them together.
set -euo pipefail

proxy_pid=''
paddlex_pid=''

stop_children() {
  [ -n "$proxy_pid" ] && kill "$proxy_pid" 2>/dev/null || true
  [ -n "$paddlex_pid" ] && kill "$paddlex_pid" 2>/dev/null || true
  wait "$proxy_pid" 2>/dev/null || true
  wait "$paddlex_pid" 2>/dev/null || true
}

trap stop_children EXIT
trap 'exit 143' INT TERM

/opt/kio/kio-acceptance-tools ocr-proxy \
  --host 0.0.0.0 \
  --port 8443 \
  --upstream-port 8080 \
  --cert /tls/server-cert.pem \
  --key /tls/server-key.pem &
proxy_pid=$!

paddlex --serve --pipeline /home/paddleocr/pipeline_config_vllm.yaml &
paddlex_pid=$!

if wait -n "$proxy_pid" "$paddlex_pid"; then
  status=0
else
  status=$?
fi
exit "$status"
