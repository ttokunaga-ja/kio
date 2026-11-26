#!/usr/bin/env bash
# Usage: bash scripts/ci/local-checks.sh WORK [all | CHECK ...]
# WORK uses lowercase letters and hyphens, e.g. reorg-a.
# With no CHECK, "all" runs fmt, clippy, test, code-limits and ci-checks,
# plus confinement and windows-clippy on Linux. On Linux, add synthetic-history
# when checking changes to kio-eval, search, indexing or history (it needs the
# Linux descriptor process boundary; on macOS leave it to CI).
# Examples:
#   bash scripts/ci/local-checks.sh reorg-a fmt code-limits
#   bash scripts/ci/local-checks.sh reorg-a all synthetic-history
# Outputs: scratch/validate/WORK/run.*/; default build cache: scratch/target.
# The first failing check stops the run and returns its status.
# Linux full tests require an already configured bubblewrap runtime and a
# systemd user manager with cgroup v2 delegation. Install the Windows Rust
# target first for windows-clippy. Host provisioning requires separate setup.
set -euo pipefail

usage() {
    sed -n '2,/^set /s/^# //p' "${BASH_SOURCE[0]}"
}

if [[ "${1:-}" == --help ]]; then
    usage
    exit 0
fi
if [[ ! "${1:-}" =~ ^[a-z]+(-[a-z]+)*$ ]]; then
    usage >&2
    exit 2
fi
work="$1"
shift
platform="$(uname -s)"
case "$platform" in
    Darwin|Linux) ;;
    *) printf 'Unsupported OS: %s\n' "$platform" >&2; exit 2 ;;
esac

if (( $# == 0 )); then
    set -- all
fi
checks=()
for check in "$@"; do
    case "$check" in
        all)
            checks+=(ci-checks)
            if [[ "$platform" == Linux ]]; then
                checks+=(confinement)
            fi
            checks+=(fmt clippy test code-limits)
            if [[ "$platform" == Linux ]]; then
                checks+=(windows-clippy)
            fi
            ;;
        fmt|clippy|test|code-limits|ci-checks)
            checks+=("$check")
            ;;
        confinement|windows-clippy|synthetic-history)
            if [[ "$platform" != Linux ]]; then
                printf '%s requires Linux\n' "$check" >&2
                exit 2
            fi
            checks+=("$check")
            ;;
        *) printf 'Unknown check: %s\n' "$check" >&2; usage >&2; exit 2 ;;
    esac
done

cd "$(dirname "${BASH_SOURCE[0]}")/../.."
root="$(pwd -P)"
mkdir -p "scratch/validate/$work"
run="$(mktemp -d "$root/scratch/validate/$work/run.XXXXXXXX")"
# Unix socket paths must stay under SUN_LEN (104 bytes on macOS), so tests get
# a short temporary directory outside the deep scratch path.
short_tmp="$(mktemp -d "${TMPDIR:-/tmp}/kio-lc.XXXXXX")"
trap 'rm -rf "$short_tmp"' EXIT
export TMPDIR="$short_tmp"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/scratch/target}"
case "$CARGO_TARGET_DIR" in
    /*) ;;
    *) export CARGO_TARGET_DIR="$root/$CARGO_TARGET_DIR" ;;
esac
cargo_pinned=(cargo +1.98.0)
printf 'Local checks on %s. Output directory: %s\n' "$platform" "$run"

ensure_litert() {
    if [[ "${KIO_LITERT_C_API_ZIP:-}" == /* && -f "$KIO_LITERT_C_API_ZIP" ]]; then
        return
    fi
    local runtime="$root/scratch/runtime/litert"
    local archive="$runtime/litert_lm_c_api-1.0.0.zip"
    if [[ ! -f "$archive" ]]; then
        mkdir -p "$runtime"
        RUNNER_TEMP="$runtime" GITHUB_ENV=/dev/null RUNNER_OS="$platform" \
            bash scripts/ci/fetch-litert-runtime.sh
    fi
    export KIO_LITERT_C_API_ZIP="$archive"
}

isolate_home() {
    mkdir "$run"/{home,config,data,cache,state}
    export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}" CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
    export HOME="$run/home" XDG_CONFIG_HOME="$run/config" XDG_DATA_HOME="$run/data"
    export XDG_CACHE_HOME="$run/cache" XDG_STATE_HOME="$run/state"
    # Kio resolves device paths via XDG, including on macOS.
    # Keep XDG_RUNTIME_DIR and the session bus for the Linux user manager.
}

workspace_tests() (
    local -a storage_paths absent_paths
    local path status=0 found=0
    storage_paths=("$HOME"/{.config/kio,.local/share/kio,.local/state/kio,.cache/kio,.kio})
    for path in "${XDG_CONFIG_HOME:-}" "${XDG_DATA_HOME:-}" "${XDG_CACHE_HOME:-}" "${XDG_STATE_HOME:-}"; do
        if [[ "$path" == /* ]]; then
            storage_paths+=("$path/kio")
        fi
    done
    absent_paths=()
    for path in "${storage_paths[@]}"; do
        if [[ ! -e "$path" && ! -L "$path" ]]; then
            absent_paths+=("$path")
        fi
    done

    isolate_home
    export CARGO_PROFILE_TEST_OPT_LEVEL=1
    local -a test_command=("${cargo_pinned[@]}" test --workspace --all-targets --locked --no-fail-fast)
    if [[ "$platform" == Linux ]]; then
        systemd-run --user --scope -p Delegate=yes \
            -E HOME -E XDG_CONFIG_HOME -E XDG_DATA_HOME -E XDG_CACHE_HOME -E XDG_STATE_HOME \
            -E TMPDIR -E RUSTUP_HOME -E CARGO_HOME -E CARGO_PROFILE_TEST_OPT_LEVEL \
            -E CARGO_TARGET_DIR -E KIO_LITERT_C_API_ZIP "${test_command[@]}" || status=$?
    else
        "${test_command[@]}" || status=$?
    fi

    for path in ${absent_paths[@]+"${absent_paths[@]}"}; do
        if [[ -e "$path" || -L "$path" ]]; then
            printf 'New kio storage path: %s\n' "$path" >&2
            found=1
        fi
    done
    if (( found == 0 )); then
        printf 'No new kio storage paths in the original HOME or XDG directories.\n'
    elif (( status == 0 )); then
        status=1
    fi
    exit "$status"
)

ci_checks() (
    shopt -s nullglob
    local -a files=(.github/workflows/*.yml .github/workflows/*.yaml)
    test "${#files[@]}" -gt 0
    local uses_count references reference_count reference invalid=0 script
    uses_count="$({
        grep -hEo "(^|[[:space:]{,])([\"']?uses[\"']?)[[:space:]]*:" "${files[@]}" || test "$?" -eq 1
    } | wc -l | tr -d '[:space:]')"
    references="$(sed -nE 's/^[[:space:]]*(-[[:space:]]+)?uses:[[:space:]]*([^[:space:]#]+).*/\2/p' "${files[@]}")"
    reference_count="$(printf '%s\n' "$references" | sed '/^$/d' | wc -l | tr -d '[:space:]')"
    if [[ "$uses_count" != "$reference_count" ]]; then
        printf 'Workflow uses entries must use the canonical block form\n' >&2
        return 1
    fi
    while IFS= read -r reference; do
        [[ -n "$reference" ]] || continue
        if [[ ! "$reference" =~ ^[^@[:space:]]+/[^@[:space:]]+@[0-9a-f]{40}$ ]]; then
            printf 'Mutable or malformed external action reference: %s\n' "$reference" >&2
            invalid=1
        fi
    done <<< "$references"
    test "$invalid" -eq 0

    for script in scripts/v1-local-gpu/gpu-phase.sh scripts/v1-local-gpu/ocr_api_entrypoint.sh \
        scripts/ci/prepare-linux-confinement.sh; do
        bash -n "$script"
    done
    "${cargo_pinned[@]}" test -p kio-ci --bin kio-code-limits --locked
)

synthetic_history() (
    # Keep the scenarios and fixture inputs in sync with CI's synthetic-history-eval.
    isolate_home
    "${cargo_pinned[@]}" build --release --locked --all-features -p kio-cli -p kio-eval --bins
    local evaluator="$CARGO_TARGET_DIR/release/kio-eval" kio="$CARGO_TARGET_DIR/release/kio"
    local current="$run/tiny-current" history="$run/tiny-history" corpus="$run/corpus"
    local golden=eval/golden-queries-short.jsonl
    "$evaluator" scale generate --out "$current" --profile tiny --lane current-text
    "$evaluator" scale prepare --corpus "$current" --bin "$kio"
    "$evaluator" scale attest --corpus "$current"
    "$evaluator" scale generate --out "$history" --profile tiny --lane history-overlay
    "$evaluator" scale prepare --corpus "$history" --bin "$kio"
    "$evaluator" scale attest --corpus "$history"
    "$evaluator" scale benchmark --current-corpus "$current" --history-corpus "$history" \
        --bin "$kio" --warmups 1 --samples 1 --out "$run/smoke.json"
    "$evaluator" generate-corpus --out "$corpus"
    install -m 0755 "$kio" "$run/kio-replay-kio"
    "$evaluator" replay-history --corpus "$corpus" --bin "$run/kio-replay-kio"
    "$evaluator" crossscope --dry-run --corpus "$corpus" --bin "$kio" --out "$run/crossscope-unused.json"
    "$evaluator" crossscope --corpus "$corpus" --bin "$kio" --out "$run/crossscope.json"
    "$evaluator" rerank dump --dataset synthetic --golden "$golden" --corpus "$corpus" \
        --bin "$kio" --limit 10 --out "$run/rerank-input.json"
    gzip -dc eval/rerank/rerank-input-fixture.json.gz > "$run/rerank-fixture-input.json"
    gzip -dc eval/rerank/rerank-output-bge-reranker-v2-m3.json.gz > "$run/rerank-fixture-output.json"
    "$evaluator" rerank apply --input "$run/rerank-fixture-input.json" \
        --output "$run/rerank-fixture-output.json" --report "$run/rerank-report.json"
    "$evaluator" --scenario M3-1 --corpus "$corpus" --bin "$kio"
    "$evaluator" --golden "$golden" --scenario M3-1 --min-recall 0.9166666666666666 \
        --corpus "$corpus" --bin "$kio"
    "$evaluator" --scenario M3-2 --corpus "$corpus" --bin "$kio"
    "$evaluator" --scenario M3-3 --corpus "$corpus" --bin "$kio"
)

for check in "${checks[@]}"; do
    printf '\n== %s ==\n' "$check"
    case "$check" in
        fmt) "${cargo_pinned[@]}" fmt --all -- --check ;;
        clippy)
            ensure_litert
            "${cargo_pinned[@]}" clippy --workspace --all-targets --locked -- -D warnings
            ;;
        test)
            ensure_litert
            workspace_tests
            ;;
        code-limits)
            "${cargo_pinned[@]}" run -q -p kio-ci --bin kio-code-limits --locked -- --root . \
                | tee "$run/code-limits.json"
            ;;
        ci-checks) ci_checks ;;
        confinement)
            # The hosted provisioning script installs packages and loads kernel policy.
            "${cargo_pinned[@]}" build -p kio-ci --bin kio-prepare-linux-confinement --locked
            "${cargo_pinned[@]}" test -p kio-ci --bin kio-prepare-linux-confinement --locked
            ;;
        windows-clippy)
            CARGO_TARGET_DIR="$root/scratch/target-win" "${cargo_pinned[@]}" clippy \
                -p kio-core -p kio-process --all-targets --locked --target x86_64-pc-windows-msvc -- -D warnings
            ;;
        synthetic-history)
            ensure_litert
            synthetic_history
            ;;
    esac
done
printf '\nLocal checks passed.\n'
