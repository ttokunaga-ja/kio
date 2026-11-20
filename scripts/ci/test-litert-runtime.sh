#!/usr/bin/env bash
set -euo pipefail

# Regression checks for the job prefetch. Pass the pinned ZIP as argument and
# run with TMPDIR pointing at a disposable work directory.
fixture="$1"
root="$(cd "$(dirname "$0")/../.." && pwd)"
tmp="$(mktemp -d)"
trap 'find "$tmp" -depth -delete' EXIT
mkdir "$tmp/bin"
export RUNNER_TEMP="$tmp" GITHUB_ENV="$tmp/github-env"
export KIO_LITERT_TEST_FIXTURE="$fixture"
export KIO_LITERT_TEST_CALLS="$tmp/calls"
cat > "$tmp/bin/curl" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
printf '.\n' >> "$KIO_LITERT_TEST_CALLS"
output=-
while (($#)); do
    if [[ "$1" == --output ]]; then
        output="$2"
        shift
    fi
    shift
done
body() {
    case "$KIO_LITERT_TEST_CASE" in
        valid) cat "$KIO_LITERT_TEST_FIXTURE" ;;
        bad-hash) head -c 123469788 /dev/zero ;;
        oversized) head -c 124000000 /dev/zero ;;
    esac
}
if [[ "$output" == - ]]; then body; else body > "$output"; fi
MOCK
chmod +x "$tmp/bin/curl"
export PATH="$tmp/bin:$PATH"

for KIO_LITERT_TEST_CASE in bad-hash oversized; do
    export KIO_LITERT_TEST_CASE
    if bash "$root/scripts/ci/fetch-litert-runtime.sh"; then
        echo "accepted $KIO_LITERT_TEST_CASE download" >&2
        exit 1
    fi
    test ! -s "$GITHUB_ENV"
done
export KIO_LITERT_TEST_CASE=valid
bash "$root/scripts/ci/fetch-litert-runtime.sh"
cmp "$fixture" "$tmp/litert_lm_c_api-1.0.0.zip"
grep -Fx "KIO_LITERT_C_API_ZIP=$tmp/litert_lm_c_api-1.0.0.zip" "$GITHUB_ENV"
test "$(wc -l < "$KIO_LITERT_TEST_CALLS")" -eq 3

if [[ "${2:-}" == --build ]]; then
    (
        cd "$root"
        export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$tmp/target}"
        export CARGO_NET_OFFLINE=true KIO_LITERT_C_API_ZIP="$fixture"
        cargo +1.98.0 check -p kio-cli --bin kio --locked --offline --message-format=json > "$tmp/first.json"
        cargo +1.98.0 check -p kio-cli --bin kio --locked --offline --message-format=json > "$tmp/next.json"
        grep '"name":"kio","src_path".*"fresh":true' "$tmp/next.json" >/dev/null

        # Exercise normal build-script mode with a verified cache and no
        # explicit ZIP. Cargo itself still resolves dependencies offline.
        cache="$CARGO_TARGET_DIR/.kio-litert-cache"
        mkdir -p "$cache"
        cp "$fixture" "$cache/1d430d80eaf536f696a4922270ba55eb3d112189ac198c3066d487a45e3b4d4c.zip"
        unset CARGO_NET_OFFLINE KIO_LITERT_C_API_ZIP
        cargo +1.98.0 check -p kio-cli --bin kio --locked --offline
    )
fi
