#!/usr/bin/env bash
set -euo pipefail

# Release builds run Cargo offline. The build script validates this exact
# archive and extracts the same pinned files as an ordinary online build.
archive="$RUNNER_TEMP/litert_lm_c_api-1.0.0.zip"
curl --fail --location --proto '=https' --proto-redir '=https' --max-time 300 \
  --output "$archive" \
  https://github.com/google-ai-edge/LiteRT-LM/releases/download/v0.19.1/litert_lm_c_api-1.0.0.zip
printf 'KIO_LITERT_C_API_ZIP=%s\n' "$archive" >> "$GITHUB_ENV"
