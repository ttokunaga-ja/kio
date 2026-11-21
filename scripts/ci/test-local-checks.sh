#!/usr/bin/env bash
# Exercise the local runner with stubbed platform and Cargo commands.
set -euo pipefail
cd "$(dirname "$0")/../.."
mkdir -p scratch
fixture="$(mktemp -d "$PWD/scratch/test-local-checks.XXXXXXXX")"
trap 'find "$fixture" -depth -delete' EXIT
mkdir -p "$fixture"/{bin,repo/scripts/ci,home,xdg}
cp scripts/ci/local-checks.sh "$fixture/repo/scripts/ci/"
unset RUSTUP_HOME CARGO_HOME XDG_CONFIG_HOME XDG_CACHE_HOME XDG_STATE_HOME
export TEST_FIXTURE="$fixture" HOME="$fixture/home" XDG_DATA_HOME="$fixture/xdg"
export KIO_LITERT_C_API_ZIP="$fixture/runtime.zip"
touch "$KIO_LITERT_C_API_ZIP"
export PATH="$fixture/bin:$PATH"

cat > "$fixture/bin/uname" <<'MOCK'
#!/usr/bin/env bash
printf '%s\n' "${TEST_OS:-Darwin}"
MOCK
cat > "$fixture/bin/cargo" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$TEST_FIXTURE/calls"
case "$*" in
    *'run -q -p kio-ci --bin kio-code-limits'*)
        printf '{}\n'
        exit "${LIMITS_STATUS:-0}"
        ;;
    *'test --workspace'*)
        run="$TEST_FIXTURE/repo/scratch/validate/smoke/"
        for var in HOME XDG_CONFIG_HOME XDG_DATA_HOME XDG_CACHE_HOME XDG_STATE_HOME TMPDIR; do
            [[ "${!var}" == "$run"* && -d "${!var}" ]]
        done
        [[ "$CARGO_PROFILE_TEST_OPT_LEVEL" == 1 ]]
        [[ "${RUSTUP_HOME:-}" == "$TEST_FIXTURE/home/.rustup" ]]
        [[ "${CARGO_HOME:-}" == "$TEST_FIXTURE/home/.cargo" ]]
        [[ "${IN_SCOPE:-0}" == "${EXPECT_SCOPE:-0}" ]]
        if [[ "${LEAK:-0}" == 1 ]]; then
            mkdir "$TEST_FIXTURE/xdg/kio"
        fi
        exit "${TEST_STATUS:-0}"
        ;;
    *'clippy --workspace'*)
        [[ "$KIO_LITERT_C_API_ZIP" == "$TEST_FIXTURE/repo/scratch/runtime/litert/"* ]]
        [[ -f "$KIO_LITERT_C_API_ZIP" ]]
        ;;
esac
MOCK
cat > "$fixture/bin/systemd-run" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
[[ "$1 $2 $3 $4" == '--user --scope -p Delegate=yes' ]]
shift 4
while [[ "$1" == -E ]]; do
    [[ -n "${!2}" ]]
    shift 2
done
IN_SCOPE=1 exec "$@"
MOCK
cat > "$fixture/repo/scripts/ci/fetch-litert-runtime.sh" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
[[ "$RUNNER_TEMP" == "$TEST_FIXTURE/repo/scratch/runtime/litert" ]]
archive="$RUNNER_TEMP/litert_lm_c_api-1.0.0.zip"
touch "$archive"
printf 'KIO_LITERT_C_API_ZIP=%s\n' "$archive" >> "$GITHUB_ENV"
MOCK
chmod +x "$fixture/bin/"*

runner="$fixture/repo/scripts/ci/local-checks.sh"
expect_status() {
    local expected="$1" actual=0
    shift
    "$@" > "$fixture/output" 2>&1 || actual=$?
    if [[ "$actual" != "$expected" ]]; then
        cat "$fixture/output" >&2
        printf 'expected status %s, got %s\n' "$expected" "$actual" >&2
        return 1
    fi
}

expect_status 0 bash "$runner" smoke fmt code-limits
expect_status 1 env LIMITS_STATUS=1 bash "$runner" smoke code-limits
expect_status 17 env LIMITS_STATUS=17 bash "$runner" smoke code-limits
expect_status 0 bash "$runner" smoke test
expect_status 0 env TEST_OS=Linux EXPECT_SCOPE=1 bash "$runner" smoke confinement test windows-clippy
grep -Fx '+1.98.0 build -p kio-ci --bin kio-prepare-linux-confinement --locked' "$fixture/calls" >/dev/null
grep -F -- '--target x86_64-pc-windows-msvc -- -D warnings' "$fixture/calls" >/dev/null
expect_status 23 env TEST_STATUS=23 LEAK=1 bash "$runner" smoke test
grep -F "New kio storage path: $XDG_DATA_HOME/kio" "$fixture/output" >/dev/null
rmdir "$XDG_DATA_HOME/kio"
expect_status 1 env LEAK=1 bash "$runner" smoke test
expect_status 0 bash "$runner" smoke test # Pre-existing storage is allowed.
expect_status 0 env KIO_LITERT_C_API_ZIP= bash "$runner" smoke clippy
test -f "$fixture/repo/scratch/runtime/litert/litert_lm_c_api-1.0.0.zip"
expect_status 2 bash "$runner" ../escape fmt
expect_status 2 bash "$runner" smoke windows-clippy
printf 'local-checks regression checks passed\n'
