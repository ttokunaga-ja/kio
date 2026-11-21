#!/bin/bash
# shellcheck disable=SC2329
set -euo pipefail
# shellcheck source=/dev/null
source "$(dirname "$0")/shared-gpu-count"
id_a=$(printf '%064d' 1)
id_b=$(printf '%064d' 2)
docker_query() {
  [[ $1 == /run/selected.sock ]]
  case $2 in
    ls) printf '%s\n' "$id_a" "$id_b" ;;
    inspect)
      [[ $3 == --format && $4 == *'.State.Running .HostConfig.DeviceRequests'* ]]
      case $5 in "$id_a") echo 1 ;; "$id_b") echo 0 ;; *) return 1 ;; esac ;;
    *) return 1 ;;
  esac
}
export DOCKER_HOST=unix:///run/ignored.sock DOCKER_CONTEXT=ignored DOCKER_API_VERSION=ignored
[[ $(running_requests /run/selected.sock) == 1 ]]
docker_query() { return 1; }
if running_requests /run/selected.sock; then exit 1; fi
docker_query() { printf '%s\n' invalid; }
if running_requests /run/selected.sock; then exit 1; fi
docker_query() { [[ $2 == ls ]]; }
[[ $(running_requests /run/selected.sock) == 0 ]]
if main unexpected; then exit 1; fi

# The real command wrapper must discard the invoking environment and pin one socket.
# shellcheck source=/dev/null
source "$(dirname "$0")/shared-gpu-count"
/usr/bin/env() {
  [[ $* == "-i LC_ALL=C PATH=/usr/bin:/bin /usr/bin/timeout -k 1 4 /opt/kio-v1-local/docker/docker --config /etc/kio-v1-local-shared-gpu-cli --host unix:///run/selected.sock container ls --quiet --no-trunc" ]]
}
SECONDS=0
docker_query /run/selected.sock ls --quiet --no-trunc
SECONDS=20
if docker_query /run/selected.sock ls --quiet --no-trunc; then exit 1; fi
printf 'Shared GPU fixtures passed\n'
