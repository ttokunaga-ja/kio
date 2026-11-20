#!/usr/bin/env bash
set -euo pipefail

# Keep these pins in sync with crates/kio-core/src/litert.rs.
bytes=123469788
sha256=1d430d80eaf536f696a4922270ba55eb3d112189ac198c3066d487a45e3b4d4c
url=https://github.com/google-ai-edge/LiteRT-LM/releases/download/v0.19.1/litert_lm_c_api-1.0.0.zip
archive="$RUNNER_TEMP/litert_lm_c_api-1.0.0.zip"
partial="$(mktemp "$RUNNER_TEMP/kio-litert.XXXXXXXX")"
trap 'if [[ -f "$partial" ]]; then unlink "$partial"; fi' EXIT

# Bound the bytes written even on older curl versions whose filesize option
# can only reject an oversized Content-Length.
curl --fail --location --proto '=https' --proto-redir '=https' --max-time 90   --max-filesize "$bytes" --output - "$url" | head -c "$((bytes + 1))" > "$partial"
if [[ "$(wc -c < "$partial")" -ne "$bytes" ]]; then
    echo 'LiteRT-LM ZIP size differs from the pin' >&2
    exit 1
fi
if command -v sha256sum >/dev/null; then
    actual="$(sha256sum < "$partial")"
else
    actual="$(shasum -a 256 < "$partial")"
fi
if [[ "${actual%% *}" != "$sha256" ]]; then
    echo 'LiteRT-LM ZIP SHA-256 differs from the pin' >&2
    exit 1
fi

mv "$partial" "$archive"
if [[ "${RUNNER_OS:-}" == Windows ]]; then
    archive="$(cygpath -w "$archive")"
fi
printf 'KIO_LITERT_C_API_ZIP=%s\n' "$archive" >> "$GITHUB_ENV"
