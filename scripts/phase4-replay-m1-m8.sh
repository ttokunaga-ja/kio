#!/bin/zsh
# Replay the public v0.1.0-rc.1 binary for independent Phase 4 checkpoints.
# The default M1/M8 pass constructs disposable isolated scopes; the M6/M7
# continuation consumes only a frozen M1/M8 fixture.  This runner deliberately
# does not download, build, or select a product binary.

set -eu
setopt noclobber pipefail

readonly EXPECTED_BINARY_BYTES=20603712
readonly EXPECTED_BINARY_SHA256='4bdc913150ecf839f05bac1237360ea2bc1cd48757009e077ead3689c806d02c'
readonly EXPECTED_ARCHIVE_BYTES=8083094
readonly EXPECTED_ARCHIVE_SHA256='590c41518b83eac8b3ba5dba4006ca5afdffd014ebc521817e804f3e77ddfd8c'
readonly EXPECTED_SIDECAR_BYTES=509
readonly EXPECTED_SIDECAR_SHA256='0ea4bbf4e26ac587653c59408dda65a704c6d075582a0f0eef2730eae20ec45b'
readonly EXPECTED_ORIGIN_MAIN='34d4107aece6aca0350295d68645d511b7388766'
readonly EXPECTED_TAG_OBJECT='8895d0e8eece48b3a99e4d67f2c8d3098edee531'
readonly EXPECTED_CANDIDATE_COMMIT='b95efd86d1ee738378edb7171509ae7ca81e8661'
readonly EXPECTED_CANDIDATE_TREE='a4183c874799ab55d2471b726f9b5dc4dd3eb8d8'
readonly EXPECTED_LOCK_SHA256='74059079ef8e69ce3e35c31214c0587616bd4eb6c3199553d5339389fc9ece21'
readonly EXPECTED_MANUAL_CONFIG_SHA256='455e7192e094bc14fdf58beb8b358d49bc2095ec57f03a5fb6ac46d8afcc7650'
readonly EXPECTED_APPROVED_MANUAL_CONFIG_SHA256='290ec55a425915256171b554b30e86018cf1ebdd277db0af42cf0aca78f6c3e5'
readonly MANUAL_CONFIG=$'[gc]\nmode = "manual_only"\n\n[gc.auto_retention]\nkeep_last_hours = 0\nkeep_hourly_days = 0\nkeep_daily_weeks = 0\nkeep_weekly_months = 0\n'

usage() {
  print -u2 -- "usage: $0 --run-root ABSOLUTE_PATH [--checkpoint m1-m8|m6-m7]"
  exit 64
}

sha256() { shasum -a 256 "$1" | awk '{print $1}'; }
file_bytes() { stat -f '%z' "$1"; }
utc_now() { date -u '+%Y-%m-%dT%H:%M:%SZ'; }

RUN_ROOT=''
CHECKPOINT='m1-m8'
while (( $# > 0 )); do
  case "$1" in
    --run-root) (( $# >= 2 )) || usage; RUN_ROOT="$2"; shift 2 ;;
    --checkpoint) (( $# >= 2 )) || usage; CHECKPOINT="$2"; shift 2 ;;
    *) usage ;;
  esac
done
[[ -n "$RUN_ROOT" && "$RUN_ROOT" = /* && -d "$RUN_ROOT" ]] || usage
[[ "$CHECKPOINT" == m1-m8 || "$CHECKPOINT" == m6-m7 ]] || usage
RUN_ROOT="$(cd "$RUN_ROOT" && pwd -P)"
RUN_ID="$(basename "$RUN_ROOT")"
EVIDENCE_ROOT="$RUN_ROOT/phase4-manual-evidence/$RUN_ID"
BINDING="$EVIDENCE_ROOT/binding"
FIXTURE="$EVIDENCE_ROOT/fixture"
STAGES="$EVIDENCE_ROOT/stages"

[[ -f "$EVIDENCE_ROOT/run.json" && -f "$BINDING/completion.json" && -f "$BINDING/binary.sha256" ]] || {
  print -u2 -- 'binding completion is missing'
  exit 65
}
jq -e \
  --arg archive_sha "$EXPECTED_ARCHIVE_SHA256" --argjson archive_bytes "$EXPECTED_ARCHIVE_BYTES" \
  --arg sidecar_sha "$EXPECTED_SIDECAR_SHA256" --argjson sidecar_bytes "$EXPECTED_SIDECAR_BYTES" \
  --arg binary_sha "$EXPECTED_BINARY_SHA256" --argjson binary_bytes "$EXPECTED_BINARY_BYTES" \
  '.schema == "kio.phase4.binding-completion.v1" and .status == "passed" and .product_binary_executed == false and
   .archive == {bytes:$archive_bytes,sha256:$archive_sha} and
   .sidecar == {bytes:$sidecar_bytes,sha256:$sidecar_sha} and
   .binary.bytes == $binary_bytes and .binary.sha256 == $binary_sha and
   (.binary.path | type == "string" and length > 1)' \
  "$BINDING/completion.json" >/dev/null || {
  print -u2 -- 'binding completion does not authorize product replay'
  exit 65
}
jq -e \
  --arg run_id "$RUN_ID" --arg origin "$EXPECTED_ORIGIN_MAIN" \
  --arg tag_object "$EXPECTED_TAG_OBJECT" --arg commit "$EXPECTED_CANDIDATE_COMMIT" \
  --arg tree "$EXPECTED_CANDIDATE_TREE" --arg lock "$EXPECTED_LOCK_SHA256" \
  --arg archive_sha "$EXPECTED_ARCHIVE_SHA256" --argjson archive_bytes "$EXPECTED_ARCHIVE_BYTES" \
  --arg sidecar_sha "$EXPECTED_SIDECAR_SHA256" --argjson sidecar_bytes "$EXPECTED_SIDECAR_BYTES" \
  --arg binary_sha "$EXPECTED_BINARY_SHA256" --argjson binary_bytes "$EXPECTED_BINARY_BYTES" '
    .schema == "kio.phase4.manual-run.v1" and .schema_version == 1 and
    .run_id == $run_id and .status == "running" and
    .expected_binding.origin_main == $origin and
    .expected_binding.tag_name == "v0.1.0-rc.1" and
    .expected_binding.tag_object == $tag_object and
    .expected_binding.candidate_commit == $commit and
    .expected_binding.candidate_tree == $tree and
    .expected_binding.cargo_lock_sha256 == $lock and
    .expected_binding.archive == {name:"kio-0.1.0-rc.1-aarch64-apple-darwin.tar.gz",bytes:$archive_bytes,sha256:$archive_sha} and
    .expected_binding.sidecar == {name:"kio-0.1.0-rc.1-aarch64-apple-darwin.checksums.json",bytes:$sidecar_bytes,sha256:$sidecar_sha} and
    .expected_binding.binary == {path:"bin/kio",bytes:$binary_bytes,sha256:$binary_sha}' \
  "$EVIDENCE_ROOT/run.json" >/dev/null || {
  print -u2 -- 'run record does not match the fixed binding'
  exit 65
}
PRODUCT_BINARY="$(jq -r '.binary.path' "$BINDING/completion.json")"
EXPECTED_PRODUCT_BINARY="$RUN_ROOT/extracted/kio-0.1.0-rc.1-aarch64-apple-darwin/bin/kio"
PRODUCT_BINARY_PHYSICAL="$(cd "$(dirname "$PRODUCT_BINARY")" 2>/dev/null && pwd -P)/$(basename "$PRODUCT_BINARY")" || {
  print -u2 -- 'bound binary path cannot be resolved physically'
  exit 65
}
[[ "$PRODUCT_BINARY" == "$EXPECTED_PRODUCT_BINARY" && "$PRODUCT_BINARY_PHYSICAL" == "$EXPECTED_PRODUCT_BINARY" && \
  -f "$PRODUCT_BINARY" && ! -L "$PRODUCT_BINARY" && -x "$PRODUCT_BINARY" && "$(stat -f '%l' "$PRODUCT_BINARY")" == 1 ]] || {
  print -u2 -- 'bound binary is not a regular file inside this run root'
  exit 65
}
[[ "$(file_bytes "$PRODUCT_BINARY")" == "$EXPECTED_BINARY_BYTES" && "$(sha256 "$PRODUCT_BINARY")" == "$EXPECTED_BINARY_SHA256" ]] || {
  print -u2 -- 'bound binary bytes or digest mismatch'
  exit 65
}
[[ "$(awk 'NR == 1 {print $1 "  " $2} NR > 1 {exit 1}' "$BINDING/binary.sha256")" == "$EXPECTED_BINARY_SHA256  bin/kio" ]] || {
  print -u2 -- 'binding binary digest sidecar mismatch'
  exit 65
}

if [[ "$CHECKPOINT" == m1-m8 ]]; then
  for collision_target in "$FIXTURE" "$STAGES/M1" "$STAGES/M8"; do
    [[ ! -e "$collision_target" ]] || { print -u2 -- "evidence collision: $collision_target"; exit 65; }
  done
  mkdir -p "$FIXTURE" "$STAGES/M1" "$STAGES/M8"
else
  for collision_target in "$STAGES/M6" "$STAGES/M7" "$EVIDENCE_ROOT/continuation-gate-m6-m7" "$EVIDENCE_ROOT/continuation-gate-m6-m7.json" "$EVIDENCE_ROOT/continuation-gate-m6-m7.sha256"; do
    [[ ! -e "$collision_target" ]] || { print -u2 -- "continuation evidence collision: $collision_target"; exit 65; }
  done
  [[ -d "$FIXTURE/m6-m7" && -d "$FIXTURE/private-m6-m7" && -f "$FIXTURE/isolation-m6-m7.json" ]] || {
    print -u2 -- 'M6/M7 continuation fixture isolation is missing'
    exit 65
  }
  mkdir -p "$STAGES/M6" "$STAGES/M7"
fi

fatal_stage() {
  local stage="$1" reason="$2" code="${3:-1}" dir="$STAGES/$1"
  local existing_terminal=''
  if [[ -f "$dir/result.json" ]]; then
    existing_terminal="$(jq -r '.terminal_status // "unreadable"' "$dir/result.json" 2>/dev/null || print -r -- 'unreadable')"
  fi
  if [[ ! -e "$dir/result.json" ]]; then
    jq -n --arg stage "$stage" --arg reason "$reason" --arg at "$(utc_now)" \
      --argjson exit_code "$code" \
      '{schema:"kio.phase4.stage-result.v1",stage:$stage,terminal_status:"failed",reason:$reason,ended_at:$at,exit_code:$exit_code}' > "$dir/result.json"
    existing_terminal='failed'
  elif [[ "$existing_terminal" != failed ]]; then
    if [[ ! -e "$dir/failure-receipt.json" ]]; then
      jq -n --arg stage "$stage" --arg reason "$reason" --arg at "$(utc_now)" \
        --arg observed_result_terminal_status "$existing_terminal" \
        --arg result_sha256 "$(sha256 "$dir/result.json")" --argjson exit_code "$code" \
        '{schema:"kio.phase4.stage-finalization-failure.v1",stage:$stage,status:"failed",reason:$reason,failed_at:$at,exit_code:$exit_code,observed_result_terminal_status:$observed_result_terminal_status,result_sha256:$result_sha256,completion_written:false}' > "$dir/failure-receipt.json"
    fi
    print -u2 -- "$stage failed after terminal result creation: $reason"
    exit "$code"
  fi
  if [[ ! -e "$dir/completion.json" ]]; then
    jq -n --arg stage "$stage" --arg reason "$reason" --arg at "$(utc_now)" \
      --arg result_sha256 "$(sha256 "$dir/result.json")" \
      '{schema:"kio.phase4.stage-completion.v1",stage:$stage,status:"failed",result_terminal_status:"failed",reason:$reason,completed_at:$at,artifact_sha256:{result:$result_sha256}}' > "$dir/completion.json"
  fi
  print -u2 -- "$stage failed: $reason"
  exit "$code"
}

ACTIVE_STAGE=''
unexpected_failure() {
  local code="$1"
  trap - ZERR
  if [[ -n "$ACTIVE_STAGE" && -d "$STAGES/$ACTIVE_STAGE" ]]; then
    fatal_stage "$ACTIVE_STAGE" 'unexpected_runner_error' "$code"
  fi
  print -u2 -- 'M1/M8 runner failed before a stage started'
  exit "$code"
}
trap 'unexpected_failure $?' ZERR

# A manifest is deterministic, covers directories and every regular file, and
# rejects symlinks, special files, and unsafe hardlinks rather than hiding them.
manifest() {
  local root="$1" output="$2" entry relative nlink
  [[ ! -e "$output" ]] || return 1
  [[ -d "$root" && ! -L "$root" ]] || return 1
  {
    print -r -- '{"schema":"kio.phase4.fixture-manifest.v1","entries":['
    local first=true
    while IFS= read -r -d '' entry; do
      if [[ "$first" == true ]]; then first=false; else print -n -- ','; fi
      relative="${entry#"$root"/}"
      if [[ -L "$entry" ]]; then
        return 1
      elif [[ -d "$entry" ]]; then
        jq -cn --arg path "$relative" --arg mode "$(stat -f '%Lp' "$entry")" \
          '{path:$path,kind:"directory",mode:$mode}'
      elif [[ -f "$entry" ]]; then
        nlink="$(stat -f '%l' "$entry")"
        [[ "$nlink" == 1 ]] || return 1
        jq -cn --arg path "$relative" --arg mode "$(stat -f '%Lp' "$entry")" \
          --arg sha "$(sha256 "$entry")" --argjson bytes "$(file_bytes "$entry")" \
          --argjson nlink "$nlink" \
          '{path:$path,kind:"regular",mode:$mode,bytes:$bytes,nlink:$nlink,sha256:$sha}'
      else
        return 1
      fi
    done < <(find "$root" -mindepth 1 -print0 | LC_ALL=C sort -z)
    print -r -- ']}'
  } > "$output"
}

manifest_diff() {
  local before="$1" after="$2" output="$3"
  [[ -f "$before" && -f "$after" && ! -e "$output" ]] || return 1
  jq -n --slurpfile before "$before" --slurpfile after "$after" '
    def path_map($manifest): reduce $manifest.entries[] as $entry ({}; .[$entry.path] = $entry);
    path_map($before[0]) as $left |
    path_map($after[0]) as $right |
    (($left | keys) + ($right | keys) | unique) as $paths |
    {schema:"kio.phase4.manifest-diff.v1",entries:[
      $paths[] as $path |
      select($left[$path] != $right[$path]) |
      {path:$path,before:($left[$path] // null),after:($right[$path] // null)}
    ]}' > "$output"
}

write_text_once() {
  local target_file="$1" text="$2"
  [[ ! -e "$target_file" ]] || return 1
  print -rn -- "$text" > "$target_file"
}

replace_fixture_text() {
  local target_file="$1" text="$2" temporary="${1}.phase4-replacement"
  [[ -f "$target_file" && ! -L "$target_file" && "$(stat -f '%l' "$target_file")" == 1 && ! -e "$temporary" ]] || return 1
  print -rn -- "$text" > "$temporary" || return 1
  /bin/mv -f "$temporary" "$target_file"
}

validate_released_consent_lock() {
  local lock_file="$1" payload canonical
  [[ -f "$lock_file" && ! -L "$lock_file" && "$(stat -f '%Lp' "$lock_file")" == 644 && \
    "$(stat -f '%l' "$lock_file")" == 1 && "$(file_bytes "$lock_file")" == 97 ]] || return 1
  jq -e '
    (keys == ["created_at","pid","token"]) and
    .pid == 4294967295 and
    (.token | type == "string" and test("^[0-9a-f]{32}$")) and
    (.created_at | type == "string" and test("^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$"))' \
    "$lock_file" >/dev/null || return 1
  payload="$(<"$lock_file")"
  canonical="$(jq -c '{pid,token,created_at}' "$lock_file")" || return 1
  [[ "$payload" == "$canonical" && "${#payload}" == "$(file_bytes "$lock_file")" ]]
}

validate_jsonl_objects() {
  # Exactly one terminal LF is allowed. Every preceding record must be a
  # nonempty JSON object; leading, interior, whitespace-only, or extra trailing
  # blank records are rejected rather than filtered out.
  jq -eRs '
    (contains("\r") | not) and endswith("\n") and
    (split("\n") as $lines |
      ($lines | length >= 2) and $lines[-1] == "" and
      ($lines[:-1] | length > 0 and
        all(.[]; length > 0 and (try (fromjson | type == "object") catch false))))' "$1" >/dev/null
}

validate_batch_input() {
  # Exact regular JSONL input: two identical pointer objects, no hidden blank
  # record, and a receipt that binds its current bytes before/after each batch.
  local input="$1" pointer="$2" receipt="$3"
  [[ -f "$input" && ! -L "$input" && "$(stat -f '%l' "$input")" == 1 && "$(stat -f '%Lp' "$input")" == 644 && \
    "$(file_bytes "$input")" -gt 0 && "$(od -An -tu1 -N1 -j $(($(file_bytes "$input") - 1)) "$input" | tr -d ' ')" == 10 ]] || return 1
  validate_jsonl_objects "$input" || return 1
  jq -s -e --arg pointer "$pointer" '(length == 2 and .[0] == ($pointer | fromjson) and .[1] == ($pointer | fromjson))' "$input" >/dev/null || return 1
  jq -e --arg path "$input" --arg sha "$(sha256 "$input")" --argjson bytes "$(file_bytes "$input")" \
    '.schema == "kio.phase4.batch-input.v1" and .path == $path and .sha256 == $sha and .bytes == $bytes and .mode == "644" and .nlink == 1 and .lines == 2 and .duplicate_rows == true and .final_lf == true' "$receipt" >/dev/null
}

validate_cursor_key() {
  # The 32 opaque bytes are first-use key material, not a line-oriented
  # cursor-issuance receipt. Bind the observed random digest, never a fixed one.
  local key_file="$1" observation="$2" search_output="$3" last_byte
  [[ -f "$key_file" && ! -L "$key_file" && "$(stat -f '%Lp' "$key_file")" == 600 && \
    "$(stat -f '%l' "$key_file")" == 1 && "$(file_bytes "$key_file")" == 32 ]] || return 1
  last_byte="$(od -An -tu1 -N1 -j 31 "$key_file" | tr -d ' ')"
  [[ "$last_byte" != 10 ]] || return 1
  jq -e --arg path "${key_file#"$FIXTURE"/}" --arg sha "$(sha256 "$key_file")" --slurpfile search "$search_output" '
    $search[0] as $response | $response.paging as $paging |
    ($search | length == 1) and
    ($response | has("paging")) and ($paging | type == "object") and ($paging | has("next_cursor")) and
    ($paging.next_cursor == null or ($paging.next_cursor | type == "string" and length > 0)) and
    (keys == ["after","before","cursor_issuance_state","key_helper_independent_of_cursor_issuance","no_terminal_lf","paging_next_cursor","path","reason","schema","sha256"]) and
    .schema == "kio.phase4.cursor-key-observation.v1" and .path == $path and
    .reason == "first_search_cursor_signing_key_helper" and
    .key_helper_independent_of_cursor_issuance == true and .no_terminal_lf == true and
    .paging_next_cursor == $paging.next_cursor and
    .cursor_issuance_state == (if $paging.next_cursor == null then "not_issued" else "issued" end) and
    .before == null and .sha256 == $sha and
    .after == {path:$path,kind:"regular",mode:"600",bytes:32,nlink:1,sha256:$sha}' "$observation" >/dev/null
}

record_harness_text() {
  # record_harness_text STAGE LABEL PATH create|replace CONTRACT_REASON TEXT
  local stage="$1" label="$2" target_file="$3" operation="$4" reason="$5" text="$6"
  local dir="$STAGES/$stage/harness-$label" before="$STAGES/$stage/harness-$label/fixture-manifest.before.json"
  local after="$STAGES/$stage/harness-$label/fixture-manifest.after.json" diff="$STAGES/$stage/harness-$label/manifest-diff.json"
  local relative="${target_file#"$FIXTURE"/}"
  [[ "$target_file" == "$FIXTURE"/* && "$relative" != "$target_file" && ! -e "$dir" ]] || return 1
  mkdir -p "$dir"
  manifest "$FIXTURE" "$before" || return 70
  case "$operation" in
    create) write_text_once "$target_file" "$text" || return 1 ;;
    replace) replace_fixture_text "$target_file" "$text" || return 1 ;;
    *) return 1 ;;
  esac
  manifest "$FIXTURE" "$after" || return 70
  manifest_diff "$before" "$after" "$diff" || return 70
  jq -e --arg path "$relative" '.entries | length == 1 and .[0].path == $path' "$diff" >/dev/null || return 71
  jq -n --arg label "$label" --arg operation "$operation" --arg reason "$reason" \
    --arg path "$relative" --arg content_sha256 "$(sha256 "$target_file")" \
    --arg before_sha256 "$(sha256 "$before")" --arg after_sha256 "$(sha256 "$after")" \
    --slurpfile diff "$diff" \
    '{schema:"kio.phase4.harness-transition.v1",label:$label,operation:$operation,contract_reason:$reason,path:$path,content_sha256:$content_sha256,before_manifest_sha256:$before_sha256,after_manifest_sha256:$after_sha256,entries:[$diff[0].entries[] | . + {contract_reason:$reason}]}' > "$dir/receipt.json"
  jq -n --arg source "harness-$label" --arg reason "$reason" \
    --arg before "$(sha256 "$before")" --arg after "$(sha256 "$after")" \
    --slurpfile diff "$diff" \
    '{schema:"kio.phase4.observation-log-manifest.v1",source:$source,contract_reason:$reason,before_digest:$before,after_digest:$after,entries:[$diff[0].entries[] | . + {contract_reason:$reason}]}' > "$dir/observation-log-manifest.json"
}

make_scope() {
  local name="$1" scope private
  scope="$FIXTURE/$name"
  private="$FIXTURE/private-$name"
  [[ ! -e "$scope" && ! -e "$private" ]] || return 1
  mkdir -p "$scope" "$private/home" "$private/xdg-config" "$private/xdg-cache" "$private/xdg-data" "$private/tmp"
  jq -n --arg scope "$scope" --arg private "$private" \
    --arg home "$private/home" --arg config "$private/xdg-config" \
    --arg cache "$private/xdg-cache" --arg data "$private/xdg-data" --arg tmp "$private/tmp" \
    '{scope:$scope,private_root:$private,HOME:$home,XDG_CONFIG_HOME:$config,XDG_CACHE_HOME:$cache,XDG_DATA_HOME:$data,TMPDIR:$tmp}'
}

if [[ "$CHECKPOINT" == m1-m8 ]]; then
  for scope_name in m1-m2 m8 m6-m7 m3 m4-m5; do
    make_scope "$scope_name" > "$FIXTURE/isolation-$scope_name.json" || {
      print -u2 -- "could not create isolated scope: $scope_name"; exit 65
    }
  done
  jq -s '{schema:"kio.phase4.isolation.v1",subscopes:.}' "$FIXTURE"/isolation-*.json > "$FIXTURE/isolation.json"
  INITIAL_MANIFEST_TMP="$EVIDENCE_ROOT/.fixture-manifest.before.json"
  manifest "$FIXTURE" "$INITIAL_MANIFEST_TMP" || {
    print -u2 -- 'could not record the initial complete fixture manifest'; exit 70
  }
  /bin/mv "$INITIAL_MANIFEST_TMP" "$FIXTURE/manifest.before.json"
  print -r -- "$(sha256 "$FIXTURE/manifest.before.json")  manifest.before.json" > "$FIXTURE/digest.before.sha256"
fi

record_command() {
  # record_command STAGE LABEL SCOPE PRIVATE policy argv...
  local stage="$1" label="$2" scope="$3" private="$4" mutation_policy="$5"; shift 5
  local dir before after observation diff stdout stderr start end exit_code scope_root scope_prefix device_prefix cache_prefix before_entry after_entry before_bytes after_bytes prefix_sha mutation_valid=true
  dir="$STAGES/$stage/$label"
  before="$dir/fixture-manifest.before.json"
  after="$dir/fixture-manifest.after.json"
  observation="$dir/observation-log-manifest.json"
  diff="$dir/manifest-diff.json"
  stdout="$dir/stdout.bin"
  stderr="$dir/stderr.bin"
  [[ ! -e "$dir" ]] || return 1
  mkdir -p "$dir"
  manifest "$FIXTURE" "$before.fixture.json" || return 70
  manifest "$scope" "$before.scope.json" || return 70
  manifest "$private" "$before.private.json" || return 70
  jq -n --arg fixture "$(sha256 "$before.fixture.json")" \
    --arg scope "$(sha256 "$before.scope.json")" --arg private "$(sha256 "$before.private.json")" \
    '{fixture_manifest_sha256:$fixture,scope_manifest_sha256:$scope,private_manifest_sha256:$private}' > "$before"
  start="$(utc_now)"
  {
    print -r -- "cwd=$scope"
    print -r -- 'environment=/usr/bin/env -i'
    print -r -- "  HOME=$private/home"
    print -r -- "  XDG_CONFIG_HOME=$private/xdg-config"
    print -r -- "  XDG_CACHE_HOME=$private/xdg-cache"
    print -r -- "  XDG_DATA_HOME=$private/xdg-data"
    print -r -- "  TMPDIR=$private/tmp"
    print -r -- '  PATH=/usr/bin:/bin:/usr/sbin:/sbin'
    print -r -- '  LC_ALL=C; LANG=C; TZ=UTC'
    print -r -- '  KIO_FIXED_NOW=unset; KIO_TEST_*=unset; proxy/secret variables=unset'
    print -r -- "mutation_policy=$mutation_policy"
    print -r -- 'argv:'; printf '  %q\n' "$@"
  } > "$dir/command.txt"
  jq -cn --args '$ARGS.positional' -- "$@" > "$dir/argv.json"
  jq -n --arg cwd "$scope" --arg home "$private/home" \
    --arg xdg_config "$private/xdg-config" --arg xdg_cache "$private/xdg-cache" \
    --arg xdg_data "$private/xdg-data" --arg tmpdir "$private/tmp" --arg mutation_policy "$mutation_policy" \
    --slurpfile argv "$dir/argv.json" \
    '{cwd:$cwd,mutation_policy:$mutation_policy,environment:{clear_environment:true,HOME:$home,XDG_CONFIG_HOME:$xdg_config,XDG_CACHE_HOME:$xdg_cache,XDG_DATA_HOME:$xdg_data,TMPDIR:$tmpdir,PATH:"/usr/bin:/bin:/usr/sbin:/sbin",LC_ALL:"C",LANG:"C",TZ:"UTC",KIO_FIXED_NOW:"unset",test_fault_proxy_and_secret_variables:"unset_by_env_i"},argv:$argv[0]}' > "$dir/invocation-input.json"
  if (cd "$scope" && /usr/bin/env -i HOME="$private/home" XDG_CONFIG_HOME="$private/xdg-config" XDG_CACHE_HOME="$private/xdg-cache" XDG_DATA_HOME="$private/xdg-data" TMPDIR="$private/tmp" PATH='/usr/bin:/bin:/usr/sbin:/sbin' LC_ALL=C LANG=C TZ=UTC "$@") > "$stdout" 2> "$stderr"; then
    exit_code=0
  else
    exit_code=$?
  fi
  end="$(utc_now)"
  manifest "$FIXTURE" "$after.fixture.json" || return 70
  manifest "$scope" "$after.scope.json" || return 70
  manifest "$private" "$after.private.json" || return 70
  jq -n --arg fixture "$(sha256 "$after.fixture.json")" \
    --arg scope "$(sha256 "$after.scope.json")" --arg private "$(sha256 "$after.private.json")" \
    '{fixture_manifest_sha256:$fixture,scope_manifest_sha256:$scope,private_manifest_sha256:$private}' > "$after"
  manifest_diff "$before.fixture.json" "$after.fixture.json" "$diff" || return 70
  scope_root="${scope#"$FIXTURE"/}"
  scope_prefix="$scope_root/.kio"
  device_prefix="${private#"$FIXTURE"/}/xdg-data/kio"
  cache_prefix="${private#"$FIXTURE"/}/xdg-cache/kio"
  case "$mutation_policy" in
    none)
      jq -e '.entries | length == 0' "$diff" >/dev/null || mutation_valid=false
      ;;
    expected_append_only_logs)
      # Text search is read-only for Kio state, but deliberately appends
      # device metrics and scope access observability records.  No cache or
      # vector-query mutation is authorized here. The independent cursor-key
      # helper is first-search setup, not evidence of next_cursor issuance.
      jq -e --arg scope_log "$scope_prefix/logs/access.jsonl" --arg scope_lock "$scope_prefix/logs/access.scrub.lock" --arg device_log "$device_prefix/logs/metrics.jsonl" --arg device_lock "$device_prefix/logs/scrub.lock" --arg cursor_key "$device_prefix/cursor-key" '
        (.entries | length == 5) and
        ([.entries[].path] | sort) == ([$scope_log, $scope_lock, $device_log, $device_lock, $cursor_key] | sort) and
        ([.entries[] | select(.path == $scope_log or .path == $device_log) | select(
          .after.kind == "regular" and .after.mode == "644" and .after.nlink == 1 and .after.bytes > 0 and
          (if .before == null then true else .before.kind == "regular" and .before.mode == "644" and .before.nlink == 1 and .after.bytes > .before.bytes end)
        )] | length == 2)' "$diff" >/dev/null || mutation_valid=false
      jq -e --arg scope_lock "$scope_prefix/logs/access.scrub.lock" --arg device_lock "$device_prefix/logs/scrub.lock" '
        ([.entries[] | select(.path == $scope_lock and .before == null and .after.kind == "regular" and .after.mode == "644" and .after.nlink == 1 and .after.bytes == 97)] | length) == 1 and
        ([.entries[] | select(.path == $device_lock and .before.kind == "regular" and .before.mode == "644" and .before.nlink == 1 and .before.bytes == 97 and .after.kind == "regular" and .after.mode == "644" and .after.nlink == 1 and .after.bytes == 97 and .before.sha256 != .after.sha256)] | length) == 1' "$diff" >/dev/null || mutation_valid=false
      jq -e --arg cursor_key "$device_prefix/cursor-key" '
        ([.entries[] | select(.path == $cursor_key and .before == null and .after.kind == "regular" and .after.mode == "600" and .after.nlink == 1 and .after.bytes == 32)] | length) == 1' "$diff" >/dev/null || mutation_valid=false
      [[ "$(od -An -tu1 -N1 -j 31 "$private/xdg-data/kio/cursor-key" | tr -d ' ')" != 10 ]] || mutation_valid=false
      jq -n --arg path "${device_prefix}/cursor-key" --arg reason "first_search_cursor_signing_key_helper" --slurpfile diff "$diff" --slurpfile search "$stdout" '
        ($diff[0].entries | map(select(.path == $path))[0]) as $entry |
        {schema:"kio.phase4.cursor-key-observation.v1",path:$path,reason:$reason,key_helper_independent_of_cursor_issuance:true,paging_next_cursor:$search[0].paging.next_cursor,cursor_issuance_state:(if $search[0].paging.next_cursor == null then "not_issued" else "issued" end),no_terminal_lf:true,before:$entry.before,after:$entry.after,sha256:$entry.after.sha256}' > "$dir/cursor-key-observation.json"
      validate_cursor_key "$private/xdg-data/kio/cursor-key" "$dir/cursor-key-observation.json" "$stdout" || mutation_valid=false
      jq -n --argjson entries '[]' > "$dir/search-log-append-observation.json"
      for log_file in "$scope/.kio/logs/access.jsonl" "$private/xdg-data/kio/logs/metrics.jsonl"; do
        before_entry="$(jq -c --arg path "${log_file#"$FIXTURE"/}" '.entries[] | select(.path == $path) | .before' "$diff")"
        after_entry="$(jq -c --arg path "${log_file#"$FIXTURE"/}" '.entries[] | select(.path == $path) | .after' "$diff")"
        [[ -n "$after_entry" && "$after_entry" != null ]] || continue
        after_bytes="$(jq -r '.bytes' <<<"$after_entry")"
        if [[ "$before_entry" == null || -z "$before_entry" ]]; then
          before_bytes=0
          prefix_sha='absent'
        else
          before_bytes="$(jq -r '.bytes' <<<"$before_entry")"
          prefix_sha="$(head -c "$before_bytes" "$log_file" | shasum -a 256 | awk '{print $1}')"
          [[ "$prefix_sha" == "$(jq -r '.sha256' <<<"$before_entry")" ]] || mutation_valid=false
        fi
        jq --arg path "${log_file#"$FIXTURE"/}" --arg before_sha "$(if [[ "$before_entry" == null || -z "$before_entry" ]]; then print -r -- absent; else jq -r '.sha256' <<<"$before_entry"; fi)" \
          --arg after_sha "$(jq -r '.sha256' <<<"$after_entry")" --arg prefix_sha "$prefix_sha" \
          --argjson before_bytes "$before_bytes" --argjson after_bytes "$after_bytes" \
          '. + [{path:$path,before_sha256:$before_sha,after_sha256:$after_sha,prefix_sha256:$prefix_sha,before_bytes:$before_bytes,after_bytes:$after_bytes,reason:"append_search_logs"}]' \
          "$dir/search-log-append-observation.json" > "$dir/.search-log-append-observation.json" && /bin/mv "$dir/.search-log-append-observation.json" "$dir/search-log-append-observation.json"
      done
      jq -e 'length == 2 and all(.before_bytes < .after_bytes and .prefix_sha256 == .before_sha256)' "$dir/search-log-append-observation.json" >/dev/null || mutation_valid=false
      for lock_file in "$scope/.kio/logs/access.scrub.lock" "$private/xdg-data/kio/logs/scrub.lock"; do
        validate_released_consent_lock "$lock_file" || mutation_valid=false
      done
      [[ "$(od -An -tu1 -N1 -j $(($(file_bytes "$scope/.kio/logs/access.jsonl") - 1)) "$scope/.kio/logs/access.jsonl" | tr -d ' ')" == 10 ]] || mutation_valid=false
      [[ "$(od -An -tu1 -N1 -j $(($(file_bytes "$private/xdg-data/kio/logs/metrics.jsonl") - 1)) "$private/xdg-data/kio/logs/metrics.jsonl" | tr -d ' ')" == 10 ]] || mutation_valid=false
      validate_jsonl_objects "$scope/.kio/logs/access.jsonl" || mutation_valid=false
      jq -s -e 'length > 0 and all(.[]; (keys == ["code","component","context","level","message","ts"]) and .code == "KIO-I-SEARCH-ACCESS-001" and .component == "kio-cli" and .level == "info" and .message == "search access" and .context.query == "[redacted]" and .context.mode == "text" and (.context.result_count | type == "number" and . >= 1))' "$scope/.kio/logs/access.jsonl" >/dev/null || mutation_valid=false
      validate_jsonl_objects "$private/xdg-data/kio/logs/metrics.jsonl" || mutation_valid=false
      jq -s -e 'length > 0 and all(.[]; (keys == ["code","component","context","level","message","metric","ts","value"]) and .code == "KIO-M-SEARCH-001" and .component == "search" and .level == "info" and .message == "search completed" and .metric == "search.latency_ms" and .context.mode == "text" and (.context.scope_count | type == "number" and . >= 1) and (.context.result_count | type == "number" and . >= 1) and (.value | type == "number" and . >= 0))' "$private/xdg-data/kio/logs/metrics.jsonl" >/dev/null || mutation_valid=false
      jq --arg scope_log "$scope_prefix/logs/access.jsonl" --arg scope_lock "$scope_prefix/logs/access.scrub.lock" --arg device_log "$device_prefix/logs/metrics.jsonl" --arg device_lock "$device_prefix/logs/scrub.lock" --arg cursor_key "$device_prefix/cursor-key" '
        .entries |= map(select(.path != $scope_log and .path != $scope_lock and .path != $device_log and .path != $device_lock))' \
        "$before.fixture.json" > "$dir/log-protected-fixture-manifest.before.json"
      jq --arg scope_log "$scope_prefix/logs/access.jsonl" --arg scope_lock "$scope_prefix/logs/access.scrub.lock" --arg device_log "$device_prefix/logs/metrics.jsonl" --arg device_lock "$device_prefix/logs/scrub.lock" '
        .entries |= map(select(.path != $scope_log and .path != $scope_lock and .path != $device_log and .path != $device_lock))' \
        "$after.fixture.json" > "$dir/log-protected-fixture-manifest.after.json"
      manifest_diff "$dir/log-protected-fixture-manifest.before.json" "$dir/log-protected-fixture-manifest.after.json" "$dir/cursor-key-only-manifest-diff.json" || mutation_valid=false
      jq -e --arg cursor_key "$device_prefix/cursor-key" '.entries | length == 1 and .[0].path == $cursor_key and .[0].before == null' "$dir/cursor-key-only-manifest-diff.json" >/dev/null || mutation_valid=false
      jq --arg scope_log "$scope_prefix/logs/access.jsonl" --arg scope_lock "$scope_prefix/logs/access.scrub.lock" --arg device_log "$device_prefix/logs/metrics.jsonl" --arg device_lock "$device_prefix/logs/scrub.lock" --arg cursor_key "$device_prefix/cursor-key" '
        .entries |= map(select(.path != $scope_log and .path != $scope_lock and .path != $device_log and .path != $device_lock and .path != $cursor_key))' \
        "$before.fixture.json" > "$dir/protected-fixture-manifest.before.json"
      jq --arg scope_log "$scope_prefix/logs/access.jsonl" --arg scope_lock "$scope_prefix/logs/access.scrub.lock" --arg device_log "$device_prefix/logs/metrics.jsonl" --arg device_lock "$device_prefix/logs/scrub.lock" --arg cursor_key "$device_prefix/cursor-key" '
        .entries |= map(select(.path != $scope_log and .path != $scope_lock and .path != $device_log and .path != $device_lock and .path != $cursor_key))' \
        "$after.fixture.json" > "$dir/protected-fixture-manifest.after.json"
      jq -n --arg before "$(sha256 "$dir/protected-fixture-manifest.before.json")" --arg after "$(sha256 "$dir/protected-fixture-manifest.after.json")" \
        '{schema:"kio.phase4.search-protected-digest.v1",excluded_paths:[".kio/logs/access.jsonl",".kio/logs/access.scrub.lock","private-m6-m7/xdg-data/kio/logs/metrics.jsonl","private-m6-m7/xdg-data/kio/logs/scrub.lock","private-m6-m7/xdg-data/kio/cursor-key"],before_sha256:$before,after_sha256:$after,unchanged:($before == $after),cursor_key_helper_not_cursor_issuance:true}' > "$dir/protected-digest.json"
      jq -e '.unchanged == true' "$dir/protected-digest.json" >/dev/null || mutation_valid=false
      ;;
    kio_init)
      jq -e --arg s "$scope_prefix" --arg d "$device_prefix" '
        ([.entries[].path] | sort) == ([
          $s,
          ($s + "/HEAD"),
          ($s + "/config.toml"),
          ($s + "/logs"),
          ($s + "/manifest.json"),
          ($s + "/objects"),
          ($s + "/objects/commits"),
          ($s + "/objects/raw"),
          ($s + "/objects/trees"),
          ($s + "/purge"),
          ($s + "/purge/epoch"),
          ($s + "/refs"),
          ($s + "/refs/heads"),
          ($s + "/refs/heads/main"),
          ($s + "/refs/tags-v1"),
          ($s + "/scope.json"),
          ($s + "/tool-lock.json"),
          $d,
          ($d + "/scope-registry.sqlite")
        ] | sort) and
        (.entries | all(.before == null and .after != null and
          (if .after.kind == "regular" then .after.nlink == 1 else .after.kind == "directory" end)))' "$diff" >/dev/null || mutation_valid=false
      jq -e '
        ([.entries[].path] | sort) == ([
          "home","tmp","xdg-cache","xdg-config","xdg-data","xdg-data/kio",
          "xdg-data/kio/scope-registry.sqlite"
        ] | sort) and
        ([.entries[] | select(.kind == "regular") | .path] == ["xdg-data/kio/scope-registry.sqlite"])' "$after.private.json" >/dev/null || mutation_valid=false
      ;;
    kio_index_first|kio_index_second|kio_index_second_after_search)
      jq -e --arg s "$scope_prefix" --arg d "$device_prefix" --arg c "$cache_prefix" --arg policy "$mutation_policy" '
        def static_first:
          . == ($s + "/.lock") or . == ($s + "/HEAD") or
          . == ($s + "/approvals.jsonl") or . == ($s + "/config.toml") or
          . == ($s + "/index") or . == ($s + "/index/chunks.jsonl") or
          . == ($s + "/index/sqlite.db") or . == ($s + "/manifest.json") or
          . == ($s + "/purge/epoch") or . == ($s + "/quarantine.jsonl") or
          . == ($s + "/refs/heads/main") or . == ($s + "/scope.json") or
          . == ($s + "/tasks.jsonl") or . == ($s + "/tool-lock.json") or
          . == $c or . == ($c + "/aggregator.sqlite") or
          . == ($d + "/consents.jsonl") or . == ($d + "/consents.lock") or
          . == ($d + "/cost-ledger.sqlite") or . == ($d + "/cost-ledger.sqlite.write-seq") or
          . == ($d + "/logs") or . == ($d + "/logs/events.jsonl") or
          . == ($d + "/logs/scrub.lock") or . == ($d + "/scope-registry.sqlite");
        def static_second:
          . == ($s + "/.lock") or . == ($s + "/HEAD") or
          . == ($s + "/index/chunks.jsonl") or . == ($s + "/index/sqlite.db") or
          . == ($s + "/manifest.json") or . == ($s + "/purge/epoch") or
          . == ($s + "/quarantine.jsonl") or . == ($s + "/refs/heads/main") or
          . == ($s + "/scope.json") or . == ($s + "/tasks.jsonl") or
          . == ($s + "/tool-lock.json") or . == ($c + "/aggregator.sqlite") or
          . == ($d + "/consents.lock") or
          . == ($d + "/cost-ledger.sqlite") or . == ($d + "/cost-ledger.sqlite.write-seq") or
          . == ($d + "/logs/events.jsonl") or . == ($d + "/logs/scrub.lock") or
          . == ($d + "/scope-registry.sqlite");
        def cas_path:
          . as $path |
          ($path | startswith($s + "/objects/")) and
          (($path | ltrimstr($s + "/")) as $logical |
            ($logical | test("^objects/(chunks|commits|manifests|normalized|normalized_unit_objects|normalized_units|prepared|raw|toollocks|trees)$")) or
            ($logical | test("^objects/(chunks|commits|manifests|normalized|normalized_unit_objects|normalized_units|prepared|raw|toollocks|trees)/[0-9a-f]{2}$")) or
            ($logical | test("^objects/(chunks|commits|manifests|normalized|normalized_unit_objects|normalized_units|prepared|raw|toollocks|trees)/[0-9a-f]{2}/[0-9a-f]{2}$")) or
            ((try (($logical | capture("^objects/(chunks|commits|manifests|normalized_unit_objects|prepared|raw|toollocks|trees)/(?<a>[0-9a-f]{2})/(?<b>[0-9a-f]{2})/(?<hash>[0-9a-f]{64})$")) as $m |
              ($m.hash | startswith($m.a + $m.b))) catch false) // false) or
            ((try (($logical | capture("^objects/normalized/(?<a>[0-9a-f]{2})/(?<b>[0-9a-f]{2})/(?<hash>[0-9a-f]{64})\\.[0-9a-f]{64}\\.g[0-9]+\\.md$")) as $m |
              ($m.hash | startswith($m.a + $m.b))) catch false) // false) or
            ((try (($logical | capture("^objects/normalized_units/(?<a>[0-9a-f]{2})/(?<b>[0-9a-f]{2})/(?<hash>[0-9a-f]{64})\\.[0-9a-f]{64}\\.g[0-9]+$")) as $m |
              ($m.hash | startswith($m.a + $m.b))) catch false) // false) or
            ((try (($logical | capture("^objects/normalized_units/(?<a>[0-9a-f]{2})/(?<b>[0-9a-f]{2})/(?<hash>[0-9a-f]{64})\\.[0-9a-f]{64}\\.g[0-9]+/(manifest\\.json|[0-9a-f]{16}\\.json)$")) as $m |
              ($m.hash | startswith($m.a + $m.b))) catch false) // false));
        def valid_change:
          .after != null and
          (if (.path | cas_path) then
             .before == null and
             (if .after.kind == "regular" then .after.nlink == 1 else .after.kind == "directory" end)
           elif .after.kind == "regular" then
             .after.nlink == 1 and
             (if $policy != "kio_index_first" then
                .before.kind == "regular" and .before.nlink == 1
              else
                .before == null or (.before.kind == "regular" and .before.nlink == 1)
              end)
           else
             .before == null and .after.kind == "directory"
           end);
        (.entries | length > 0) and
        (.entries | all(
          ((if $policy == "kio_index_first" then (.path | static_first) else (.path | static_second) end) or
           (.path | cas_path)) and valid_change
        )) and
        ([.entries[] | select(.path | cas_path) | select(.after.kind == "regular")] | length > 0) and
        (if $policy == "kio_index_first" then
           ([
             ($s + "/.lock"),($s + "/HEAD"),($s + "/approvals.jsonl"),($s + "/config.toml"),
             ($s + "/index"),($s + "/index/chunks.jsonl"),($s + "/index/sqlite.db"),
             ($s + "/manifest.json"),($s + "/quarantine.jsonl"),($s + "/refs/heads/main"),
             ($s + "/scope.json"),($s + "/tasks.jsonl"),($s + "/tool-lock.json"),
             $c,($c + "/aggregator.sqlite"),($d + "/consents.jsonl"),($d + "/consents.lock"),
             ($d + "/cost-ledger.sqlite"),($d + "/cost-ledger.sqlite.write-seq"),($d + "/logs"),
             ($d + "/logs/events.jsonl"),($d + "/logs/scrub.lock"),($d + "/scope-registry.sqlite")
           ] - [.entries[].path] | length) == 0
         else
           ([
             ($s + "/HEAD"),($s + "/index/chunks.jsonl"),($s + "/index/sqlite.db"),
             ($s + "/manifest.json"),($s + "/refs/heads/main"),($s + "/scope.json"),
             ($c + "/aggregator.sqlite"),($d + "/consents.lock"),
             ($d + "/logs/events.jsonl"),($d + "/scope-registry.sqlite")
           ] - [.entries[].path] | length) == 0
         end) and
        (if $policy != "kio_index_first" then
           ([.entries[] | select(.path == ($d + "/consents.lock") and
             .before.kind == "regular" and .before.mode == "644" and .before.nlink == 1 and .before.bytes == 97 and
             .after.kind == "regular" and .after.mode == "644" and .after.nlink == 1 and .after.bytes == 97 and
             .before.sha256 != .after.sha256)] | length) == 1
         else true end)' "$diff" >/dev/null || mutation_valid=false
      jq -e --arg policy "$mutation_policy" '
        ([.entries[].path] | sort) == ([
          "home","tmp","xdg-cache","xdg-cache/kio","xdg-cache/kio/aggregator.sqlite",
          "xdg-config","xdg-data","xdg-data/kio","xdg-data/kio/consents.jsonl",
          "xdg-data/kio/consents.lock","xdg-data/kio/cost-ledger.sqlite",
          "xdg-data/kio/cost-ledger.sqlite.write-seq","xdg-data/kio/logs",
          "xdg-data/kio/logs/events.jsonl","xdg-data/kio/logs/scrub.lock",
          "xdg-data/kio/scope-registry.sqlite"
        ] + (if $policy == "kio_index_second_after_search" then ["xdg-data/kio/logs/metrics.jsonl","xdg-data/kio/cursor-key"] else [] end) | sort) and
        (.entries | all(if .kind == "regular" then .nlink == 1 else .kind == "directory" end))' "$after.private.json" >/dev/null || mutation_valid=false
      validate_released_consent_lock "$private/xdg-data/kio/consents.lock" || mutation_valid=false
      if [[ "$mutation_policy" == kio_index_second || "$mutation_policy" == kio_index_second_after_search ]]; then
        jq -e --slurpfile after "$after.scope.json" '
          def entry($manifest; $path): [$manifest.entries[] | select(.path == $path)][0];
          entry(.; ".kio/config.toml") == entry($after[0]; ".kio/config.toml") and
          entry(.; ".kio/approvals.jsonl") == entry($after[0]; ".kio/approvals.jsonl")' "$before.scope.json" >/dev/null || mutation_valid=false
        jq -e --slurpfile after "$after.private.json" '
          def entry($manifest; $path): [$manifest.entries[] | select(.path == $path)][0];
          entry(.; "xdg-data/kio/consents.jsonl") == entry($after[0]; "xdg-data/kio/consents.jsonl")' "$before.private.json" >/dev/null || mutation_valid=false
      fi
      ;;
    *) mutation_valid=false ;;
  esac
  jq -r '.entries[].path' "$diff" | jq -Rsc 'split("\n") | map(select(length > 0))' > "$dir/observed-path-set.json"
  jq -n --arg label "$label" --arg mutation_policy "$mutation_policy" \
    --arg scope_prefix "$scope_prefix" --arg device_prefix "$device_prefix" --arg cache_prefix "$cache_prefix" \
    --arg contract_reason "$(case "$mutation_policy" in none) print -r -- 'read-only command permits no fixture or private-root change' ;; expected_append_only_logs) print -r -- 'first text search permits only append_search_logs metrics/access records, their scrub locks, and independent first-use cursor-key helper creation; all other state remains protected' ;; *) print -r -- 'invocation-specific closed logical leaves and hash-shaped CAS descendants only; retained SQLite sidecars and unlisted regular files fail closed' ;; esac)" \
    --arg before "$(sha256 "$before")" --arg after "$(sha256 "$after")" \
    --slurpfile diff "$diff" --slurpfile observed_path_set "$dir/observed-path-set.json" \
    --argjson mutation_policy_valid "$mutation_valid" \
    '{schema:"kio.phase4.observation-log-manifest.v1",source:$label,mutation_policy:$mutation_policy,mutation_policy_valid:$mutation_policy_valid,contract_reason:$contract_reason,observed_path_set:$observed_path_set[0],transient_observation_limit:"before/after manifests reject retained unlisted files but cannot prove absence of create-delete transient siblings",before_digest:$before,after_digest:$after,entries:[$diff[0].entries[] | . + {contract_reason:$contract_reason}]}' > "$observation"
  print -r -- "$(sha256 "$stdout")  stdout.bin" > "$dir/stdout.sha256"
  print -r -- "$(sha256 "$stderr")  stderr.bin" > "$dir/stderr.sha256"
  jq -n --arg start "$start" --arg end "$end" --argjson exit_code "$exit_code" --argjson mutation_policy_valid "$mutation_valid" \
    --arg stdout_sha256 "$(sha256 "$stdout")" --arg stderr_sha256 "$(sha256 "$stderr")" \
    --arg before_sha256 "$(sha256 "$before")" --arg after_sha256 "$(sha256 "$after")" \
    '{started_at:$start,ended_at:$end,exit_code:$exit_code,mutation_policy_valid:$mutation_policy_valid,stdout_sha256:$stdout_sha256,stderr_sha256:$stderr_sha256,before_manifest_sha256:$before_sha256,after_manifest_sha256:$after_sha256}' > "$dir/receipt.json"
  jq -n --slurpfile input "$dir/invocation-input.json" --slurpfile receipt "$dir/receipt.json" \
    '$input[0] + {receipt:$receipt[0]}' > "$dir/invocation.json"
  [[ "$mutation_valid" == true ]] || return 71
  return "$exit_code"
}

stage_command_manifest() {
  local stage="$1" output="$2"; shift 2
  local dir label first=true
  [[ ! -e "$output" ]] || return 1
  {
    print -r -- '{"schema":"kio.phase4.command-manifest.v1","commands":['
    for label in "$@"; do
      dir="$STAGES/$stage/$label"
      [[ -d "$dir" && -f "$dir/command.txt" && -f "$dir/receipt.json" && -f "$dir/invocation.json" ]] || return 1
      if [[ "$first" == true ]]; then first=false; else print -n -- ','; fi
      jq -cn --arg label "$label" \
        --arg command_sha256 "$(sha256 "$dir/command.txt")" \
        --arg receipt_sha256 "$(sha256 "$dir/receipt.json")" \
        --arg invocation_sha256 "$(sha256 "$dir/invocation.json")" \
        --arg stdout_sha256 "$(sha256 "$dir/stdout.bin")" --arg stderr_sha256 "$(sha256 "$dir/stderr.bin")" \
        --arg before_manifest_sha256 "$(sha256 "$dir/fixture-manifest.before.json")" \
        --arg after_manifest_sha256 "$(sha256 "$dir/fixture-manifest.after.json")" \
        --arg observation_log_manifest_sha256 "$(sha256 "$dir/observation-log-manifest.json")" \
        --rawfile command "$dir/command.txt" --slurpfile receipt "$dir/receipt.json" \
        --slurpfile invocation "$dir/invocation.json" \
        '{label:$label,invocation:$invocation[0],invocation_sha256:$invocation_sha256,command:$command,command_sha256:$command_sha256,receipt:$receipt[0],receipt_sha256:$receipt_sha256,stdout_sha256:$stdout_sha256,stderr_sha256:$stderr_sha256,fixture_manifest_before_sha256:$before_manifest_sha256,fixture_manifest_after_sha256:$after_manifest_sha256,observation_log_manifest_sha256:$observation_log_manifest_sha256}'
    done
    print -r -- ']}'
  } > "$output"
}

stage_manifest_summary() {
  local stage="$1" before_source="$2" after_source="$3" transition_reason="$4" dir="$STAGES/$1" source_label
  shift 4
  local -a observation_files=()
  [[ ! -e "$dir/fixture-manifest.before.json" && ! -e "$dir/fixture-manifest.after.json" && \
    ! -e "$dir/digest.before.sha256" && ! -e "$dir/digest.after.sha256" && \
    ! -e "$dir/observation-log-manifest.json" ]] || return 1
  cp "$before_source" "$dir/fixture-manifest.before.json"
  cp "$after_source" "$dir/fixture-manifest.after.json"
  print -r -- "$(sha256 "$dir/fixture-manifest.before.json")  fixture-manifest.before.json" > "$dir/digest.before.sha256"
  print -r -- "$(sha256 "$dir/fixture-manifest.after.json")  fixture-manifest.after.json" > "$dir/digest.after.sha256"
  for source_label in "$@"; do
    [[ -f "$dir/$source_label/observation-log-manifest.json" ]] || return 1
    observation_files+=("$dir/$source_label/observation-log-manifest.json")
  done
  (( ${#observation_files[@]} > 0 )) || return 1
  jq -s --arg before "$(sha256 "$dir/fixture-manifest.before.json")" \
    --arg after "$(sha256 "$dir/fixture-manifest.after.json")" \
    --arg reason "$transition_reason" \
    '{schema:"kio.phase4.stage-observation-log-manifest.v1",contract_reason:$reason,before_digest:$before,after_digest:$after,transitions:.,entries:[.[] as $observation | $observation.entries[] | . + {source:$observation.source}]}' \
    "${observation_files[@]}" > "$dir/observation-log-manifest.json"
}

stage_primary_invocation() {
  local stage="$1" label="$2" dir="$STAGES/$1" source="$STAGES/$1/$2"
  [[ -f "$source/command.txt" && -f "$source/stdout.bin" && -f "$source/stderr.bin" && \
    ! -e "$dir/command.txt" && ! -e "$dir/stdout.bin" && ! -e "$dir/stderr.bin" && \
    ! -e "$dir/stdout.sha256" && ! -e "$dir/stderr.sha256" ]] || return 1
  cp "$source/command.txt" "$dir/command.txt"
  cp "$source/stdout.bin" "$dir/stdout.bin"
  cp "$source/stderr.bin" "$dir/stderr.bin"
  print -r -- "$(sha256 "$dir/stdout.bin")  stdout.bin" > "$dir/stdout.sha256"
  print -r -- "$(sha256 "$dir/stderr.bin")  stderr.bin" > "$dir/stderr.sha256"
}

json_commit() { jq -er '.. | objects | .commit_hash? // empty | strings | select(test("^sha256:[0-9a-f]{64}$"))' "$1" | head -1; }
json_tree() { jq -er '.. | objects | .tree_hash? // empty | strings | select(test("^sha256:[0-9a-f]{64}$"))' "$1" | head -1; }

validate_offline_approve_index() {
  jq -e '
    .status == "indexed" and .approval_method == "approve" and
    .network_allowed == false and .network_opt_in == true and
    .failed_files == 0 and .pending_files == 0 and .pending_online_tasks == 0 and
    .paused_tasks == 0 and .embedding_tasks_executed == 0 and .embedding_tasks_failed == 0 and
    (.commit_hash | test("^sha256:[0-9a-f]{64}$")) and
    (.tree_hash | test("^sha256:[0-9a-f]{64}$")) and
    .gc.status == "disabled" and .gc.mode == "manual_only" and .gc.reason == "manual_only" and .gc.trigger == "index"' "$1" >/dev/null
}

stage_start() {
  local stage="$1" predecessor="$2" mutation="$3"
  ACTIVE_STAGE="$stage"
  jq -n --arg stage "$stage" --arg predecessor "$predecessor" --arg mutation "$mutation" --arg at "$(utc_now)" \
    '{schema:"kio.phase4.stage-start.v1",stage:$stage,predecessors:(if $predecessor == "" then [] else [$predecessor] end),mutation_class:$mutation,status:"running",started_at:$at}' > "$STAGES/$stage/stage.json"
}

complete_stage() {
  local stage="$1" terminal_state="$2" reason="$3" assertion_file="$4"
  local dir="$STAGES/$stage" manifest_tmp="$EVIDENCE_ROOT/.${stage}-evidence-manifest.json"
  local completion_tmp="$EVIDENCE_ROOT/.${stage}-completion.json" result_terminal required
  for required in stage.json command.txt command-manifest.json stdout.bin stdout.sha256 stderr.bin stderr.sha256 fixture-manifest.before.json fixture-manifest.after.json digest.before.sha256 digest.after.sha256 observation-log-manifest.json result.json assertions.json; do
    [[ -f "$dir/$required" && ! -L "$dir/$required" ]] || return 1
  done
  result_terminal="$(jq -er '.terminal_status' "$dir/result.json")" || return 1
  [[ "$result_terminal" == "$terminal_state" ]] || return 1
  if [[ "$terminal_state" == passed ]]; then
    jq -e '[.. | booleans] | all' "$assertion_file" >/dev/null || return 1
  fi
  [[ ! -e "$manifest_tmp" && ! -e "$completion_tmp" && ! -e "$dir/evidence-manifest.json" && ! -e "$dir/completion.json" ]] || return 1
  manifest "$dir" "$manifest_tmp" || return 1
  /bin/mv "$manifest_tmp" "$dir/evidence-manifest.json"
  jq -n --arg stage "$stage" --arg status "$terminal_state" --arg result_terminal "$result_terminal" \
    --arg reason "$reason" --arg at "$(utc_now)" \
    --arg stage_sha256 "$(sha256 "$dir/stage.json")" --arg result_sha256 "$(sha256 "$dir/result.json")" \
    --arg assertions_sha256 "$(sha256 "$assertion_file")" \
    --arg command_sha256 "$(sha256 "$dir/command.txt")" --arg command_manifest_sha256 "$(sha256 "$dir/command-manifest.json")" \
    --arg stdout_sha256 "$(sha256 "$dir/stdout.bin")" --arg stderr_sha256 "$(sha256 "$dir/stderr.bin")" \
    --arg fixture_before_sha256 "$(sha256 "$dir/fixture-manifest.before.json")" \
    --arg fixture_after_sha256 "$(sha256 "$dir/fixture-manifest.after.json")" \
    --arg digest_before_sha256 "$(sha256 "$dir/digest.before.sha256")" \
    --arg digest_after_sha256 "$(sha256 "$dir/digest.after.sha256")" \
    --arg observation_sha256 "$(sha256 "$dir/observation-log-manifest.json")" \
    --arg evidence_manifest_sha256 "$(sha256 "$dir/evidence-manifest.json")" \
    --slurpfile assertions "$assertion_file" --slurpfile commands "$dir/command-manifest.json" \
    '{schema:"kio.phase4.stage-completion.v1",stage:$stage,status:$status,result_terminal_status:$result_terminal,reason:$reason,completed_at:$at,assertions:$assertions[0],artifact_sha256:{stage:$stage_sha256,result:$result_sha256,assertions:$assertions_sha256,command:$command_sha256,command_manifest:$command_manifest_sha256,stdout:$stdout_sha256,stderr:$stderr_sha256,fixture_manifest_before:$fixture_before_sha256,fixture_manifest_after:$fixture_after_sha256,digest_before:$digest_before_sha256,digest_after:$digest_after_sha256,observation_log_manifest:$observation_sha256,evidence_manifest:$evidence_manifest_sha256},command_artifacts:($commands[0].commands | map({label,invocation_sha256,command_sha256,receipt_sha256,stdout_sha256,stderr_sha256,fixture_manifest_before_sha256,fixture_manifest_after_sha256,observation_log_manifest_sha256}))}' > "$completion_tmp"
  /bin/mv "$completion_tmp" "$dir/completion.json"
  ACTIVE_STAGE=''
}

continuation_gate_m6_m7() {
  local m1_scope="$FIXTURE/m1-m2" m1_private="$FIXTURE/private-m1-m2" scope="$FIXTURE/m6-m7" private="$FIXTURE/private-m6-m7"
  local gate_dir="$EVIDENCE_ROOT/continuation-gate-m6-m7"
  local m1_scope_manifest="$gate_dir/m1-scope-now.json" m1_private_manifest="$gate_dir/m1-private-now.json"
  local scope_manifest="$gate_dir/m6-m7-scope-unused.json" private_manifest="$gate_dir/m6-m7-private-unused.json"
  [[ -f "$STAGES/M1/result.json" && -f "$STAGES/M1/completion.json" && -f "$STAGES/M1/frozen-fixture-manifest.json" && -f "$STAGES/M8/result.json" && -f "$STAGES/M8/completion.json" ]] || return 1
  jq -e '.terminal_status == "passed" and .stop_rule == "continue_to_M8"' "$STAGES/M1/result.json" >/dev/null || return 1
  jq -e '.status == "passed" and .reason == "retention_dry_run_verified"' "$STAGES/M1/completion.json" >/dev/null || return 1
  jq -e '.terminal_status == "blocked" and .reason == "public_cli_unreachable_candidate_unconstructable"' "$STAGES/M8/result.json" >/dev/null || return 1
  jq -e '.status == "blocked" and .reason == "public_cli_unreachable_candidate_unconstructable"' "$STAGES/M8/completion.json" >/dev/null || return 1
  mkdir "$gate_dir" || return 1
  manifest "$m1_scope" "$m1_scope_manifest" && manifest "$m1_private" "$m1_private_manifest" && manifest "$scope" "$scope_manifest" && manifest "$private" "$private_manifest" || return 1
  jq -e --arg scope "$(sha256 "$m1_scope_manifest")" --arg private "$(sha256 "$m1_private_manifest")" '.scope_manifest_sha256 == $scope and .private_manifest_sha256 == $private' "$STAGES/M1/frozen-fixture-manifest.json" >/dev/null || return 1
  jq -e '.entries | length == 0' "$scope_manifest" >/dev/null || return 1
  jq -e '([.entries[].path] | sort) == ["home","tmp","xdg-cache","xdg-config","xdg-data"] and all(.entries[]; .kind == "directory")' "$private_manifest" >/dev/null || return 1
  jq -n --arg binary_sha "$(sha256 "$PRODUCT_BINARY")" --arg m1_scope "$(sha256 "$m1_scope_manifest")" --arg m1_private "$(sha256 "$m1_private_manifest")" --arg scope "$(sha256 "$scope_manifest")" --arg private "$(sha256 "$private_manifest")" \
    '{schema:"kio.phase4.m6-m7-continuation-gate.v1",status:"passed",fixed_binding_binary_sha256:$binary_sha,m1_frozen_scope_manifest_sha256:$m1_scope,m1_frozen_private_manifest_sha256:$m1_private,m6_m7_unused_scope_manifest_sha256:$scope,m6_m7_unused_private_manifest_sha256:$private,unused_scope:true,unused_private:true}' > "$EVIDENCE_ROOT/continuation-gate-m6-m7.json"
  print -r -- "$(sha256 "$EVIDENCE_ROOT/continuation-gate-m6-m7.json")  continuation-gate-m6-m7.json" > "$EVIDENCE_ROOT/continuation-gate-m6-m7.sha256"
}

validate_alive() {
  local output="$1" pointer="$2"
  jq -e --argjson pointer "$pointer" '
    . == {status:"alive",details:{
      scope_id:$pointer.scope_id,
      scope_path:$pointer.scope_path,
      commit:$pointer.commit,
      raw_hash:$pointer.raw_hash,
      tool_profile_hash:$pointer.tool_profile_hash,
      chunk_hash:$pointer.chunk_hash,
      commit_shallow:false,
      manifest_missing:false
    }}' "$output" >/dev/null
}

run_m6_m7() {
  local scope="$FIXTURE/m6-m7" private="$FIXTURE/private-m6-m7" doc="$FIXTURE/m6-m7/evidence.md" stage=M6 pointer pointer_file index_commit raw_hash target
  continuation_gate_m6_m7 || { print -u2 -- 'M6/M7 continuation gate failed'; exit 65; }
  stage_start M6 M8 'public fixture init/index, append-only text-search observability, then read-only evidence verification'
  record_harness_text M6 evidence "$doc" create 'exact public evidence fixture bytes' $'# Evidence\n\nTTL is 3600 seconds.\n' || fatal_stage M6 'evidence_fixture_creation_failed'
  record_command M6 init "$scope" "$private" kio_init "$PRODUCT_BINARY" init --json || fatal_stage M6 'init_failed'
  record_command M6 index "$scope" "$private" kio_index_first "$PRODUCT_BINARY" index --offline --approve --json || fatal_stage M6 'index_failed'
  validate_offline_approve_index "$STAGES/M6/index/stdout.bin" || fatal_stage M6 'index_predicate_failed'
  index_commit="$(json_commit "$STAGES/M6/index/stdout.bin")" || fatal_stage M6 'index_commit_missing'
  record_command M6 search "$scope" "$private" expected_append_only_logs "$PRODUCT_BINARY" search 3600 --mode text --json || fatal_stage M6 'search_failed_or_unexpected_mutation'
  jq -e --arg commit "$index_commit" --arg scope_path "$scope/.kio" '
    .paging as $paging | .results[0].evidence_pointer as $pointer |
    (.results | type == "array" and length > 0) and ($pointer | type == "object") and
    ($paging | type == "object") and ($paging | keys == ["limit","next_cursor"]) and
    $paging.limit == 20 and
    ($paging.next_cursor == null or ($paging.next_cursor | type == "string" and length > 0)) and
    $pointer.schema_version == 1 and $pointer.commit == $commit and
    ([$pointer.raw_hash,$pointer.tool_profile_hash,$pointer.chunk_hash] |
      all(.[]; type == "string" and test("^sha256:[0-9a-f]{64}$"))) and
    ($pointer.scope_id | type == "string" and length > 0) and $pointer.scope_path == $scope_path and
    $pointer.path_at_commit == "evidence.md" and
    (.results[0].evidence_uri == ("kio://" + $pointer.scope_id + "/" + $pointer.commit + "/" + $pointer.raw_hash + "/" + $pointer.tool_profile_hash + "/" + $pointer.chunk_hash))' "$STAGES/M6/search/stdout.bin" >/dev/null || fatal_stage M6 'search_pointer_predicate_failed'
  pointer="$(jq -c '.results[0].evidence_pointer' "$STAGES/M6/search/stdout.bin")"
  raw_hash="$(jq -r '.raw_hash' <<<"$pointer")"
  [[ "$raw_hash" == "sha256:$(sha256 "$doc")" ]] || fatal_stage M6 'pointer_raw_hash_mismatch'
  mkdir "$STAGES/M6/input"
  print -rn -- "$pointer"$'\n'"$pointer"$'\n' > "$STAGES/M6/input/pointers.jsonl"
  pointer_file="$STAGES/M6/input/pointers.jsonl"
  jq -n --arg path "$pointer_file" --arg sha "$(sha256 "$pointer_file")" --argjson bytes "$(file_bytes "$pointer_file")" '{schema:"kio.phase4.batch-input.v1",path:$path,sha256:$sha,bytes:$bytes,mode:"644",nlink:1,lines:2,duplicate_rows:true,final_lf:true}' > "$STAGES/M6/input/receipt.json"
  validate_batch_input "$pointer_file" "$pointer" "$STAGES/M6/input/receipt.json" || fatal_stage M6 'batch_input_shape_invalid'
  record_command M6 verify-single "$scope" "$private" none "$PRODUCT_BINARY" evidence verify "$pointer" --json || fatal_stage M6 'single_verify_failed'
  validate_alive "$STAGES/M6/verify-single/stdout.bin" "$pointer" || fatal_stage M6 'single_verify_predicate_failed'
  validate_batch_input "$pointer_file" "$pointer" "$STAGES/M6/input/receipt.json" || fatal_stage M6 'batch_input_changed'
  record_command M6 verify-batch "$scope" "$private" none "$PRODUCT_BINARY" evidence verify --batch "$pointer_file" --json || fatal_stage M6 'batch_verify_failed'
  validate_batch_input "$pointer_file" "$pointer" "$STAGES/M6/input/receipt.json" || fatal_stage M6 'batch_input_changed'
  record_command M6 verify-single-strict "$scope" "$private" none "$PRODUCT_BINARY" evidence verify "$pointer" --strict --json || fatal_stage M6 'strict_single_verify_failed'
  validate_alive "$STAGES/M6/verify-single-strict/stdout.bin" "$pointer" || fatal_stage M6 'strict_single_verify_predicate_failed'
  validate_batch_input "$pointer_file" "$pointer" "$STAGES/M6/input/receipt.json" || fatal_stage M6 'batch_input_changed'
  record_command M6 verify-batch-strict "$scope" "$private" none "$PRODUCT_BINARY" evidence verify --batch "$pointer_file" --strict --json || fatal_stage M6 'strict_batch_verify_failed'
  validate_batch_input "$pointer_file" "$pointer" "$STAGES/M6/input/receipt.json" || fatal_stage M6 'batch_input_changed'
  for output in "$STAGES/M6/verify-batch/stdout.bin" "$STAGES/M6/verify-batch-strict/stdout.bin"; do
    jq -e --arg input "sha256:$(sha256 "$pointer_file")" --argjson pointer "$pointer" '
      .results as $results |
      (keys == ["input_sha256","results","schema","schema_version","strict","summary","verified_count"]) and
      .schema == "kio.evidence.batch-verify" and .schema_version == 1 and
      .input_sha256 == $input and .verified_count == 2 and
      .summary == {total:2,status_counts:{alive:2,tombstoned:0,not_found:0,scope_unreachable:0,unverifiable:0,registry_duplicate:0}} and
      ($results | length == 2) and $results[0].line == 1 and $results[1].line == 2 and
      all($results[];
        (keys == ["line","result"]) and
        .result == {status:"alive",details:{
          scope_id:$pointer.scope_id,
          scope_path:$pointer.scope_path,
          commit:$pointer.commit,
          raw_hash:$pointer.raw_hash,
          tool_profile_hash:$pointer.tool_profile_hash,
          chunk_hash:$pointer.chunk_hash,
          commit_shallow:false,
          manifest_missing:false
        }})' "$output" >/dev/null || fatal_stage M6 'batch_schema_predicate_failed'
  done
  if ! jq -e '.strict == false' "$STAGES/M6/verify-batch/stdout.bin" >/dev/null || ! jq -e '.strict == true' "$STAGES/M6/verify-batch-strict/stdout.bin" >/dev/null; then
    fatal_stage M6 'batch_strict_flag_mismatch'
  fi
  for label in verify-single verify-single-strict; do jq -c . "$STAGES/M6/$label/stdout.bin" > "$STAGES/M6/$label.compact.json"; done
  for label in verify-batch verify-batch-strict; do for row in 0 1; do jq -c ".results[$row].result" "$STAGES/M6/$label/stdout.bin" > "$STAGES/M6/$label.row-$row.compact.json"; done; done
  for label in verify-batch verify-batch-strict; do
    single=verify-single; [[ "$label" == verify-batch-strict ]] && single=verify-single-strict
    if ! cmp -s "$STAGES/M6/$single.compact.json" "$STAGES/M6/$label.row-0.compact.json" || ! cmp -s "$STAGES/M6/$single.compact.json" "$STAGES/M6/$label.row-1.compact.json"; then
      fatal_stage M6 'batch_nested_result_parity_failed'
    fi
  done
  for label in search verify-single verify-batch verify-single-strict verify-batch-strict; do [[ ! -s "$STAGES/M6/$label/stderr.bin" ]] || fatal_stage M6 'happy_path_stderr_not_empty'; done
  validate_cursor_key "$private/xdg-data/kio/cursor-key" "$STAGES/M6/search/cursor-key-observation.json" "$STAGES/M6/search/stdout.bin" || fatal_stage M6 'cursor_key_changed_after_search'
  jq -n --argjson pointer "$pointer" --arg commit "$index_commit" --arg raw "$raw_hash" --arg document_sha256 "$(sha256 "$doc")" --arg binary_sha256 "$(sha256 "$PRODUCT_BINARY")" --arg batch_sha "$(sha256 "$pointer_file")" --argjson batch_bytes "$(file_bytes "$pointer_file")" --slurpfile cursor "$STAGES/M6/search/cursor-key-observation.json" '{fixed_binding_binary_sha256:$binary_sha256,pointer:$pointer,commit:$commit,raw_hash:$raw,document_sha256:$document_sha256,batch_input_sha256:$batch_sha,batch_input_bytes:$batch_bytes,cursor_key:$cursor[0],predicates:{search_append_only_observability:true,cursor_key_first_search_helper_not_cursor_issuance:true,standalone_alive:true,batch_exact_schema:true,batch_duplicate_order_preserved:true,batch_nested_result_byte_parity:true,strict_alive:true,empty_happy_path_stderr:true,batch_input_unchanged:true}}' > "$STAGES/M6/assertions.json"
  stage_command_manifest M6 "$STAGES/M6/command-manifest.json" init index search verify-single verify-batch verify-single-strict verify-batch-strict || fatal_stage M6 'command_manifest_failed'
  stage_primary_invocation M6 verify-batch-strict || fatal_stage M6 'primary_invocation_failed'
  stage_manifest_summary M6 "$STAGES/M6/evidence/fixture-manifest.before.json" "$STAGES/M6/verify-batch-strict/fixture-manifest.after.json" 'exact evidence fixture, public init/index/text search and read-only evidence verification' evidence init index search verify-single verify-batch verify-single-strict verify-batch-strict || fatal_stage M6 'manifest_summary_failed'
  jq -n --arg before "$(sha256 "$STAGES/M6/evidence/fixture-manifest.before.json")" --arg after "$(sha256 "$STAGES/M6/verify-batch-strict/fixture-manifest.after.json")" --slurpfile a "$STAGES/M6/assertions.json" --slurpfile c "$STAGES/M6/command-manifest.json" --slurpfile o "$STAGES/M6/observation-log-manifest.json" '{schema:"kio.phase4.stage-result.v1",stage:"M6",terminal_status:"passed",primary_invocation:"verify-batch-strict",stop_rule:"continue_to_M7",fixture_manifest_before_sha256:$before,fixture_manifest_after_sha256:$after,commands:$c[0].commands,observations:$o[0],predicates:$a[0]}' > "$STAGES/M6/result.json"
  complete_stage M6 passed 'evidence_batch_verify_verified' "$STAGES/M6/assertions.json"

  stage=M7
  pointer="$(jq -er '.pointer' "$STAGES/M6/assertions.json")" || { print -u2 -- 'M7 cannot recover the M6 original pointer'; exit 65; }
  raw_hash="$(jq -er '.raw_hash' "$STAGES/M6/assertions.json")" || { print -u2 -- 'M7 cannot recover the M6 raw binding'; exit 65; }
  index_commit="$(jq -er '.commit' "$STAGES/M6/assertions.json")" || { print -u2 -- 'M7 cannot recover the M6 commit binding'; exit 65; }
  stage_start M7 M6 'unrelated document index then read-only public log, retarget, and strict verify'
  record_harness_text M7 unrelated "$scope/unrelated.md" create 'exact unrelated later document bytes' $'# Later\n\nNo change.\n' || fatal_stage M7 'unrelated_fixture_creation_failed'
  [[ "$(sha256 "$doc")" == "${raw_hash#sha256:}" ]] || fatal_stage M7 'original_document_changed'
  record_command M7 index-later "$scope" "$private" kio_index_second_after_search "$PRODUCT_BINARY" index --offline --approve --json || fatal_stage M7 'later_index_failed'
  validate_offline_approve_index "$STAGES/M7/index-later/stdout.bin" || fatal_stage M7 'later_index_predicate_failed'
  jq -e --slurpfile after "$STAGES/M7/index-later/fixture-manifest.after.json" '
    def entry($m; $path): [$m.entries[] | select(.path == $path)][0];
    entry(.; "m6-m7/.kio/logs/access.jsonl") == entry($after[0]; "m6-m7/.kio/logs/access.jsonl") and
    entry(.; "m6-m7/.kio/logs/access.scrub.lock") == entry($after[0]; "m6-m7/.kio/logs/access.scrub.lock") and
    entry(.; "private-m6-m7/xdg-data/kio/logs/metrics.jsonl") == entry($after[0]; "private-m6-m7/xdg-data/kio/logs/metrics.jsonl") and
    entry(.; "private-m6-m7/xdg-data/kio/logs/scrub.lock") == entry($after[0]; "private-m6-m7/xdg-data/kio/logs/scrub.lock") and
    entry(.; "private-m6-m7/xdg-data/kio/cursor-key") == entry($after[0]; "private-m6-m7/xdg-data/kio/cursor-key")' "$STAGES/M7/index-later/fixture-manifest.before.json" >/dev/null || fatal_stage M7 'search_protocol_leaves_changed_by_index'
  jq -e --arg old "$index_commit" '.commit.parents == [$old]' "$STAGES/M7/index-later/stdout.bin" >/dev/null || fatal_stage M7 'later_index_parent_chain_invalid'
  record_command M7 log "$scope" "$private" none "$PRODUCT_BINARY" log --json || fatal_stage M7 'log_failed_or_mutated'
  target="$(jq -er '.commits | select(type == "array" and length > 0) | .[0].commit_hash | select(test("^sha256:[0-9a-f]{64}$"))' "$STAGES/M7/log/stdout.bin")" || fatal_stage M7 'log_target_missing'
  jq -e --arg target "$target" --arg old "$index_commit" '(keys == ["commits","truncated"]) and .truncated == false and (.commits | type == "array" and length >= 2 and .[0].commit_hash == $target and .[1].commit_hash == $old)' "$STAGES/M7/log/stdout.bin" >/dev/null || fatal_stage M7 'log_schema_invalid'
  [[ "$target" != "$index_commit" && "$target" == "$(json_commit "$STAGES/M7/index-later/stdout.bin")" ]] || fatal_stage M7 'later_target_not_exact'
  for n in 1 2; do record_command M7 "retarget-$n" "$scope" "$private" none "$PRODUCT_BINARY" evidence retarget "$pointer" --at "$target" --json || fatal_stage M7 "retarget_${n}_failed"; done
  [[ "$(sha256 "$STAGES/M7/retarget-1/fixture-manifest.before.json")" == "$(sha256 "$STAGES/M7/retarget-1/fixture-manifest.after.json")" && "$(sha256 "$STAGES/M7/retarget-1/fixture-manifest.after.json")" == "$(sha256 "$STAGES/M7/retarget-2/fixture-manifest.before.json")" && "$(sha256 "$STAGES/M7/retarget-2/fixture-manifest.before.json")" == "$(sha256 "$STAGES/M7/retarget-2/fixture-manifest.after.json")" ]] || fatal_stage M7 'retarget_fixture_private_changed'
  cmp -s "$STAGES/M7/retarget-1/stdout.bin" "$STAGES/M7/retarget-2/stdout.bin" || fatal_stage M7 'retarget_output_not_byte_identical'
  jq -cS . "$STAGES/M7/retarget-1/stdout.bin" > "$STAGES/M7/retarget-1.normalized.json"
  jq -cS . "$STAGES/M7/retarget-2/stdout.bin" > "$STAGES/M7/retarget-2.normalized.json"
  cmp -s "$STAGES/M7/retarget-1.normalized.json" "$STAGES/M7/retarget-2.normalized.json" || fatal_stage M7 'retarget_normalized_json_not_byte_identical'
  jq -e --arg target "$target" --arg pointer "$pointer" '(keys == ["match_method","new_pointer","retargeted_from","schema","schema_version","status","target_commit"]) and .schema == "kio.evidence.retarget" and .schema_version == 1 and .status == "retargeted" and .target_commit == $target and .target_commit != .retargeted_from.commit and .retargeted_from == ($pointer | fromjson) and .match_method == "heading_path_exact" and .new_pointer.commit == $target and .new_pointer.raw_hash == ($pointer | fromjson | .raw_hash)' "$STAGES/M7/retarget-1/stdout.bin" >/dev/null || fatal_stage M7 'retarget_predicate_failed'
  new_pointer="$(jq -c '.new_pointer' "$STAGES/M7/retarget-1/stdout.bin")"
  record_command M7 verify-new-strict "$scope" "$private" none "$PRODUCT_BINARY" evidence verify "$new_pointer" --strict --json || fatal_stage M7 'new_pointer_verify_failed'
  validate_alive "$STAGES/M7/verify-new-strict/stdout.bin" "$new_pointer" || fatal_stage M7 'new_pointer_verify_predicate_failed'
  for label in log retarget-1 retarget-2 verify-new-strict; do [[ ! -s "$STAGES/M7/$label/stderr.bin" ]] || fatal_stage M7 'happy_path_stderr_not_empty'; done
  [[ "$(sha256 "$doc")" == "${raw_hash#sha256:}" && "$(jq -c '.new_pointer.raw_hash' "$STAGES/M7/retarget-1/stdout.bin")" == "\"$raw_hash\"" ]] || fatal_stage M7 'post_retarget_source_binding_changed'
  validate_cursor_key "$private/xdg-data/kio/cursor-key" "$STAGES/M6/search/cursor-key-observation.json" "$STAGES/M6/search/stdout.bin" || fatal_stage M7 'cursor_key_changed_after_search'
  jq -n --arg target "$target" --arg raw "$raw_hash" --arg document_sha256 "$(sha256 "$doc")" --arg binary_sha256 "$(sha256 "$PRODUCT_BINARY")" --arg pointer "$pointer" --slurpfile m6 "$STAGES/M6/assertions.json" '{fixed_binding_binary_sha256:$binary_sha256,target_commit:$target,raw_hash:$raw,document_sha256:$document_sha256,original_pointer:($pointer|fromjson),cursor_key:$m6[0].cursor_key,predicates:{later_exact_commit:true,retarget_stdout_byte_identical:true,retarget_json_byte_identical:true,new_pointer_strict_alive:true,fixture_private_unchanged_per_retarget:true,cursor_key_unchanged_after_search:true,empty_happy_path_stderr:true,source_binding_stable:true}}' > "$STAGES/M7/assertions.json"
  stage_command_manifest M7 "$STAGES/M7/command-manifest.json" index-later log retarget-1 retarget-2 verify-new-strict || fatal_stage M7 'command_manifest_failed'
  stage_primary_invocation M7 verify-new-strict || fatal_stage M7 'primary_invocation_failed'
  stage_manifest_summary M7 "$STAGES/M7/unrelated/fixture-manifest.before.json" "$STAGES/M7/verify-new-strict/fixture-manifest.after.json" 'one unrelated fixture document and later index; all retarget operations read-only' unrelated index-later log retarget-1 retarget-2 verify-new-strict || fatal_stage M7 'manifest_summary_failed'
  jq -n --arg before "$(sha256 "$STAGES/M7/unrelated/fixture-manifest.before.json")" --arg after "$(sha256 "$STAGES/M7/verify-new-strict/fixture-manifest.after.json")" --slurpfile a "$STAGES/M7/assertions.json" --slurpfile c "$STAGES/M7/command-manifest.json" --slurpfile o "$STAGES/M7/observation-log-manifest.json" '{schema:"kio.phase4.stage-result.v1",stage:"M7",terminal_status:"passed",primary_invocation:"verify-new-strict",stop_rule:"continue_to_M3",fixture_manifest_before_sha256:$before,fixture_manifest_after_sha256:$after,commands:$c[0].commands,observations:$o[0],predicates:$a[0]}' > "$STAGES/M7/result.json"
  complete_stage M7 passed 'evidence_retarget_verified' "$STAGES/M7/assertions.json"
}

run_m1() {
  local stage=M1 scope private doc
  scope="$FIXTURE/m1-m2"
  private="$FIXTURE/private-m1-m2"
  doc="$scope/document.md"
  local old=$'## Retention fixture\nold byte sequence\n' current=$'## Retention fixture\ncurrent byte sequence\n'
  local old_commit old_tree current_commit current_tree plan1="$STAGES/$stage/gc-1/stdout.bin" plan2="$STAGES/$stage/gc-2/stdout.bin"
  stage_start "$stage" '' 'fixture writes through index; dry-run thereafter'
  record_command "$stage" init "$scope" "$private" kio_init "$PRODUCT_BINARY" init --json || fatal_stage "$stage" 'init_failed_or_unlisted_mutation'
  record_harness_text "$stage" config "$scope/.kio/config.toml" replace 'install exact manual-only all-zero retention config' "$MANUAL_CONFIG" || fatal_stage "$stage" 'config_replace_failed_or_unlisted_mutation'
  [[ "$(sha256 "$scope/.kio/config.toml")" == "$EXPECTED_MANUAL_CONFIG_SHA256" ]] || fatal_stage "$stage" 'config_exact_bytes_mismatch'
  print -r -- "$(sha256 "$scope/.kio/config.toml")  .kio/config.toml" > "$STAGES/$stage/config.sha256"
  record_harness_text "$stage" document-old "$doc" create 'old fixture document bytes' "$old" || fatal_stage "$stage" 'old_document_collision_or_unlisted_mutation'
  print -r -- "$(sha256 "$doc")  document.md" > "$STAGES/$stage/document.old.sha256"
  record_command "$stage" index-old "$scope" "$private" kio_index_first "$PRODUCT_BINARY" index --offline --approve --json || fatal_stage "$stage" 'old_index_failed_or_unlisted_mutation'
  validate_offline_approve_index "$STAGES/$stage/index-old/stdout.bin" || fatal_stage "$stage" 'old_index_offline_approve_predicate_failed'
  [[ "$(sha256 "$scope/.kio/config.toml")" == "$EXPECTED_APPROVED_MANUAL_CONFIG_SHA256" ]] || fatal_stage "$stage" 'old_index_approval_config_transition_mismatch'
  old_commit="$(json_commit "$STAGES/$stage/index-old/stdout.bin")" || fatal_stage "$stage" 'old_index_commit_missing'
  old_tree="$(json_tree "$STAGES/$stage/index-old/stdout.bin")" || fatal_stage "$stage" 'old_index_tree_missing'
  record_harness_text "$stage" document-current "$doc" replace 'current fixture document bytes with a distinct tree' "$current" || fatal_stage "$stage" 'current_document_replace_failed_or_unlisted_mutation'
  print -r -- "$(sha256 "$doc")  document.md" > "$STAGES/$stage/document.current.sha256"
  record_command "$stage" index-current "$scope" "$private" kio_index_second "$PRODUCT_BINARY" index --offline --approve --json || fatal_stage "$stage" 'current_index_failed_or_unlisted_mutation'
  validate_offline_approve_index "$STAGES/$stage/index-current/stdout.bin" || fatal_stage "$stage" 'current_index_offline_approve_predicate_failed'
  [[ "$(sha256 "$scope/.kio/config.toml")" == "$EXPECTED_APPROVED_MANUAL_CONFIG_SHA256" ]] || fatal_stage "$stage" 'current_index_approval_config_transition_mismatch'
  current_commit="$(json_commit "$STAGES/$stage/index-current/stdout.bin")" || fatal_stage "$stage" 'current_index_commit_missing'
  current_tree="$(json_tree "$STAGES/$stage/index-current/stdout.bin")" || fatal_stage "$stage" 'current_index_tree_missing'
  [[ "$old_commit" != "$current_commit" && "$old_tree" != "$current_tree" ]] || fatal_stage "$stage" 'index_transition_not_distinct'
  record_harness_text "$stage" config-final "$scope/.kio/config.toml" replace 'restore exact manual-only all-zero retention config after the second required approve index' "$MANUAL_CONFIG" || fatal_stage "$stage" 'final_config_restore_failed_or_unlisted_mutation'
  [[ "$(sha256 "$scope/.kio/config.toml")" == "$EXPECTED_MANUAL_CONFIG_SHA256" ]] || fatal_stage "$stage" 'final_config_exact_bytes_mismatch'
  print -r -- "$(sha256 "$scope/.kio/config.toml")  .kio/config.toml" > "$STAGES/$stage/config.final.sha256"
  record_command "$stage" gc-1 "$scope" "$private" none "$PRODUCT_BINARY" gc --dry-run --json || fatal_stage "$stage" 'dry_run_1_failed_or_mutated'
  record_command "$stage" gc-2 "$scope" "$private" none "$PRODUCT_BINARY" gc --dry-run --json || fatal_stage "$stage" 'dry_run_2_failed_or_mutated'
  jq -e --arg oc "$old_commit" --arg ot "$old_tree" --arg cc "$current_commit" --arg scope "$scope" '
    (keys == ["as_of","baseline_receipts_digest","candidate_count","candidate_tree_count","candidates","estimated_bytes","exclusions","limits","object_kinds_planned","plan_digest","policy","scope_path","stability_check_stats","stable_truth_digest","stats","status","truth_digest"]) and
    .status == "dry_run" and (.as_of | test("^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$")) and .scope_path == $scope and .candidate_count == 1 and .candidate_tree_count == 1 and .object_kinds_planned == ["tree"] and
    (.limits | keys == ["max_commits","max_depth","max_dir_entries","max_graph_steps","max_name_bytes","max_receipts","max_refs","max_tree_entries","max_verified_bytes"]) and
    (.stats | keys == ["commits","dir_entries","graph_steps","receipts","refs","tree_entries","trees_verified","verified_bytes"]) and
    (.stability_check_stats | keys == ["commits","dir_entries","graph_steps","receipts","refs","tree_entries","trees_verified","verified_bytes"]) and
    # These are diagnostic counters from different traversals, not an authority comparison.
    ([.stats[],.stability_check_stats[]] | all(type == "number" and . >= 0 and floor == .)) and
    (.policy | keys == ["keep_daily_weeks","keep_hourly_days","keep_last_hours","keep_repaired","keep_weekly_months"]) and
    .policy.keep_last_hours == 0 and .policy.keep_hourly_days == 0 and .policy.keep_daily_weeks == 0 and .policy.keep_weekly_months == 0 and .policy.keep_repaired == 5 and
    (.exclusions | all(keys == ["count","reason"] and (.count | type == "number" and . >= 0) and (.reason | type == "string"))) and
    (.candidates | length == 1) and (.candidates[0] | keys == ["commit_hash","commit_type","created_at","policy","size_bytes","tree_hash"] and .commit_hash == $oc and .commit_hash != $cc and .tree_hash == $ot and .commit_type == "auto" and .policy == "auto_retention" and (.created_at | test("^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(\\.[0-9]+)?Z$")) and (.size_bytes | type == "number" and . > 0)) and
    .estimated_bytes == .candidates[0].size_bytes and
    ([.truth_digest,.stable_truth_digest,.baseline_receipts_digest,.plan_digest] | all(type == "string" and test("^sha256:[0-9a-f]{64}$"))) and
    ([.exclusions[]? | select(.reason == "ref_tip" and .count >= 1)] | length == 1)' "$plan1" >/dev/null || fatal_stage "$stage" 'dry_run_1_predicate_failed'
  jq -e --slurpfile first "$plan1" '
    (.as_of | test("^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$")) and
    (del(.as_of) == ($first[0] | del(.as_of))) and
    ([.truth_digest,.stable_truth_digest,.baseline_receipts_digest,.plan_digest] | all(type == "string" and test("^sha256:[0-9a-f]{64}$")))' "$plan2" >/dev/null || fatal_stage "$stage" 'dry_run_stability_failed'
  [[ "$(sha256 "$STAGES/$stage/gc-1/fixture-manifest.before.json")" == "$(sha256 "$STAGES/$stage/gc-1/fixture-manifest.after.json")" && "$(sha256 "$STAGES/$stage/gc-2/fixture-manifest.before.json")" == "$(sha256 "$STAGES/$stage/gc-2/fixture-manifest.after.json")" ]] || fatal_stage "$stage" 'dry_run_mutated_protected_manifest'
  [[ "$(sha256 "$STAGES/$stage/gc-1/fixture-manifest.after.json")" == "$(sha256 "$STAGES/$stage/gc-2/fixture-manifest.before.json")" ]] || fatal_stage "$stage" 'dry_run_cross_invocation_state_changed'
  manifest "$scope" "$STAGES/$stage/frozen-scope-manifest.json"
  manifest "$private" "$STAGES/$stage/frozen-private-manifest.json"
  jq -n --arg scope_sha256 "$(sha256 "$STAGES/$stage/frozen-scope-manifest.json")" \
    --arg private_sha256 "$(sha256 "$STAGES/$stage/frozen-private-manifest.json")" \
    '{schema:"kio.phase4.frozen-fixture.v1",scope_manifest_sha256:$scope_sha256,private_manifest_sha256:$private_sha256}' > "$STAGES/$stage/frozen-fixture-manifest.json"
  print -r -- "$(sha256 "$STAGES/$stage/frozen-fixture-manifest.json")  frozen-fixture-manifest.json" > "$STAGES/$stage/frozen-fixture.sha256"
  jq -n --arg old_commit "$old_commit" --arg old_tree "$old_tree" --arg current_commit "$current_commit" --arg current_tree "$current_tree" \
    --arg binary_sha256 "$EXPECTED_BINARY_SHA256" \
    --arg approved_config_sha256 "$EXPECTED_APPROVED_MANUAL_CONFIG_SHA256" \
    --arg config_sha256 "$(sha256 "$scope/.kio/config.toml")" \
    --arg old_document_sha256 "$(awk '{print $1}' "$STAGES/$stage/document.old.sha256")" \
    --arg current_document_sha256 "$(awk '{print $1}' "$STAGES/$stage/document.current.sha256")" \
    --arg plan_1_sha256 "$(sha256 "$plan1")" --arg plan_2_sha256 "$(sha256 "$plan2")" \
    --arg frozen_fixture_sha256 "$(sha256 "$STAGES/$stage/frozen-fixture-manifest.json")" \
    --slurpfile run "$EVIDENCE_ROOT/run.json" \
    '{fixed_binding:$run[0].expected_binding,downloaded_binary_sha256:$binary_sha256,approved_config_sha256:$approved_config_sha256,config_sha256:$config_sha256,documents:{old_sha256:$old_document_sha256,current_sha256:$current_document_sha256},old:{commit:$old_commit,tree:$old_tree},current:{commit:$current_commit,tree:$current_tree},plans:{first_sha256:$plan_1_sha256,second_sha256:$plan_2_sha256},predicates:{index_mutation_path_sets_closed:true,offline_index_network_allowed_false:true,approval_config_transition_exact:true,second_approval_log_consent_record_and_config_unchanged:true,second_consent_lock_update_closed:true,final_manual_config_restored:true,real_candidate:true,tip_excluded:true,tree_only:true,retention_pass_diagnostic_shapes_valid:true,retention_diagnostics_repeat_stable:true,semantic_repeat_stability:true,dry_run_1_no_write:true,dry_run_2_no_write:true,dry_run_cross_invocation_state_unchanged:true},frozen_fixture_sha256:$frozen_fixture_sha256}' > "$STAGES/$stage/assertions.json"
  stage_command_manifest "$stage" "$STAGES/$stage/command-manifest.json" init index-old index-current gc-1 gc-2 || fatal_stage "$stage" 'command_manifest_failed'
  stage_primary_invocation "$stage" gc-2 || fatal_stage "$stage" 'primary_invocation_receipt_failed'
  stage_manifest_summary "$stage" "$STAGES/$stage/init/fixture-manifest.before.json" "$STAGES/$stage/gc-2/fixture-manifest.after.json" 'public init and two approve-index transitions, then exact config restoration; two final dry-runs read-only' init harness-config harness-document-old index-old harness-document-current index-current harness-config-final gc-1 gc-2 || fatal_stage "$stage" 'stage_manifest_summary_failed'
  jq -n --arg stage "$stage" --arg before "$(sha256 "$STAGES/$stage/init/fixture-manifest.before.json")" --arg after "$(sha256 "$STAGES/$stage/gc-2/fixture-manifest.after.json")" \
    --slurpfile assertions "$STAGES/$stage/assertions.json" --slurpfile commands "$STAGES/$stage/command-manifest.json" \
    --slurpfile observations "$STAGES/$stage/observation-log-manifest.json" \
    '{schema:"kio.phase4.stage-result.v1",stage:$stage,terminal_status:"passed",primary_invocation:"gc-2",stop_rule:"continue_to_M8",fixture_manifest_before_sha256:$before,fixture_manifest_after_sha256:$after,commands:$commands[0].commands,observations:$observations[0],predicates:$assertions[0]}' > "$STAGES/$stage/result.json"
  complete_stage "$stage" passed 'retention_dry_run_verified' "$STAGES/$stage/assertions.json"
}

run_m8() {
  local stage=M8 scope private doc
  scope="$FIXTURE/m8"
  private="$FIXTURE/private-m8"
  doc="$scope/document.md"
  local old=$'## Unreachable inventory fixture\nold byte sequence\n' current=$'## Unreachable inventory fixture\ncurrent byte sequence\n'
  local old_commit old_tree current_commit current_tree
  local retention_plan1="$STAGES/$stage/retention-plan-1/stdout.bin" retention_plan2="$STAGES/$stage/retention-plan-2/stdout.bin"
  local inventory1="$STAGES/$stage/inventory-1/stdout.bin" inventory2="$STAGES/$stage/inventory-2/stdout.bin"
  stage_start "$stage" M1 'fixture writes through index; diagnostic dry-run thereafter'
  record_command "$stage" init "$scope" "$private" kio_init "$PRODUCT_BINARY" init --json || fatal_stage "$stage" 'init_failed_or_unlisted_mutation'
  record_harness_text "$stage" config "$scope/.kio/config.toml" replace 'install exact manual-only all-zero retention config' "$MANUAL_CONFIG" || fatal_stage "$stage" 'config_replace_failed_or_unlisted_mutation'
  [[ "$(sha256 "$scope/.kio/config.toml")" == "$EXPECTED_MANUAL_CONFIG_SHA256" ]] || fatal_stage "$stage" 'config_exact_bytes_mismatch'
  print -r -- "$(sha256 "$scope/.kio/config.toml")  .kio/config.toml" > "$STAGES/$stage/config.sha256"
  record_harness_text "$stage" document-old "$doc" create 'old fixture document bytes' "$old" || fatal_stage "$stage" 'old_document_collision_or_unlisted_mutation'
  print -r -- "$(sha256 "$doc")  document.md" > "$STAGES/$stage/document.old.sha256"
  record_command "$stage" index-old "$scope" "$private" kio_index_first "$PRODUCT_BINARY" index --offline --approve --json || fatal_stage "$stage" 'old_index_failed_or_unlisted_mutation'
  validate_offline_approve_index "$STAGES/$stage/index-old/stdout.bin" || fatal_stage "$stage" 'old_index_offline_approve_predicate_failed'
  [[ "$(sha256 "$scope/.kio/config.toml")" == "$EXPECTED_APPROVED_MANUAL_CONFIG_SHA256" ]] || fatal_stage "$stage" 'old_index_approval_config_transition_mismatch'
  old_commit="$(json_commit "$STAGES/$stage/index-old/stdout.bin")" || fatal_stage "$stage" 'old_index_commit_missing'
  old_tree="$(json_tree "$STAGES/$stage/index-old/stdout.bin")" || fatal_stage "$stage" 'old_index_tree_missing'
  record_harness_text "$stage" document-current "$doc" replace 'current fixture document bytes with a distinct tree' "$current" || fatal_stage "$stage" 'current_document_replace_failed_or_unlisted_mutation'
  print -r -- "$(sha256 "$doc")  document.md" > "$STAGES/$stage/document.current.sha256"
  record_command "$stage" index-current "$scope" "$private" kio_index_second "$PRODUCT_BINARY" index --offline --approve --json || fatal_stage "$stage" 'current_index_failed_or_unlisted_mutation'
  validate_offline_approve_index "$STAGES/$stage/index-current/stdout.bin" || fatal_stage "$stage" 'current_index_offline_approve_predicate_failed'
  [[ "$(sha256 "$scope/.kio/config.toml")" == "$EXPECTED_APPROVED_MANUAL_CONFIG_SHA256" ]] || fatal_stage "$stage" 'current_index_approval_config_transition_mismatch'
  current_commit="$(json_commit "$STAGES/$stage/index-current/stdout.bin")" || fatal_stage "$stage" 'current_index_commit_missing'
  current_tree="$(json_tree "$STAGES/$stage/index-current/stdout.bin")" || fatal_stage "$stage" 'current_index_tree_missing'
  [[ "$old_commit" != "$current_commit" && "$old_tree" != "$current_tree" ]] || fatal_stage "$stage" 'index_transition_not_distinct'
  record_harness_text "$stage" config-final "$scope/.kio/config.toml" replace 'restore exact manual-only all-zero retention config after the second required approve index' "$MANUAL_CONFIG" || fatal_stage "$stage" 'final_config_restore_failed_or_unlisted_mutation'
  [[ "$(sha256 "$scope/.kio/config.toml")" == "$EXPECTED_MANUAL_CONFIG_SHA256" ]] || fatal_stage "$stage" 'final_config_exact_bytes_mismatch'
  print -r -- "$(sha256 "$scope/.kio/config.toml")  .kio/config.toml" > "$STAGES/$stage/config.final.sha256"
  record_command "$stage" retention-plan-1 "$scope" "$private" none "$PRODUCT_BINARY" gc --dry-run --json || fatal_stage "$stage" 'retention_plan_1_failed_or_mutated'
  record_command "$stage" retention-plan-2 "$scope" "$private" none "$PRODUCT_BINARY" gc --dry-run --json || fatal_stage "$stage" 'retention_plan_2_failed_or_mutated'
  jq -e --arg oc "$old_commit" --arg ot "$old_tree" --arg cc "$current_commit" --arg scope "$scope" '
    (keys == ["as_of","baseline_receipts_digest","candidate_count","candidate_tree_count","candidates","estimated_bytes","exclusions","limits","object_kinds_planned","plan_digest","policy","scope_path","stability_check_stats","stable_truth_digest","stats","status","truth_digest"]) and
    .status == "dry_run" and (.as_of | test("^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$")) and .scope_path == $scope and .candidate_count == 1 and .candidate_tree_count == 1 and .object_kinds_planned == ["tree"] and
    (.limits | keys == ["max_commits","max_depth","max_dir_entries","max_graph_steps","max_name_bytes","max_receipts","max_refs","max_tree_entries","max_verified_bytes"]) and
    (.stats | keys == ["commits","dir_entries","graph_steps","receipts","refs","tree_entries","trees_verified","verified_bytes"]) and
    (.stability_check_stats | keys == ["commits","dir_entries","graph_steps","receipts","refs","tree_entries","trees_verified","verified_bytes"]) and
    # These are diagnostic counters from different traversals, not an authority comparison.
    ([.stats[],.stability_check_stats[]] | all(type == "number" and . >= 0 and floor == .)) and
    (.policy | keys == ["keep_daily_weeks","keep_hourly_days","keep_last_hours","keep_repaired","keep_weekly_months"]) and
    .policy.keep_last_hours == 0 and .policy.keep_hourly_days == 0 and .policy.keep_daily_weeks == 0 and .policy.keep_weekly_months == 0 and .policy.keep_repaired == 5 and
    (.exclusions | all(keys == ["count","reason"] and (.count | type == "number" and . >= 0) and (.reason | type == "string"))) and
    (.candidates | length == 1) and (.candidates[0] | keys == ["commit_hash","commit_type","created_at","policy","size_bytes","tree_hash"] and .commit_hash == $oc and .commit_hash != $cc and .tree_hash == $ot and .commit_type == "auto" and .policy == "auto_retention" and (.created_at | test("^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(\\.[0-9]+)?Z$")) and (.size_bytes | type == "number" and . > 0)) and
    .estimated_bytes == .candidates[0].size_bytes and
    ([.truth_digest,.stable_truth_digest,.baseline_receipts_digest,.plan_digest] | all(type == "string" and test("^sha256:[0-9a-f]{64}$"))) and
    ([.exclusions[]? | select(.reason == "ref_tip" and .count >= 1)] | length == 1)' "$retention_plan1" >/dev/null || fatal_stage "$stage" 'retention_plan_1_predicate_failed'
  jq -e --slurpfile first "$retention_plan1" '
    (.as_of | test("^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$")) and
    (del(.as_of) == ($first[0] | del(.as_of))) and
    ([.truth_digest,.stable_truth_digest,.baseline_receipts_digest,.plan_digest] | all(type == "string" and test("^sha256:[0-9a-f]{64}$")))' "$retention_plan2" >/dev/null || fatal_stage "$stage" 'retention_plan_stability_failed'
  [[ "$(sha256 "$STAGES/$stage/retention-plan-1/fixture-manifest.before.json")" == "$(sha256 "$STAGES/$stage/retention-plan-1/fixture-manifest.after.json")" && "$(sha256 "$STAGES/$stage/retention-plan-2/fixture-manifest.before.json")" == "$(sha256 "$STAGES/$stage/retention-plan-2/fixture-manifest.after.json")" ]] || fatal_stage "$stage" 'retention_plan_mutated_protected_manifest'
  [[ "$(sha256 "$STAGES/$stage/retention-plan-1/fixture-manifest.after.json")" == "$(sha256 "$STAGES/$stage/retention-plan-2/fixture-manifest.before.json")" ]] || fatal_stage "$stage" 'retention_plan_cross_invocation_state_changed'
  record_command "$stage" inventory-1 "$scope" "$private" none "$PRODUCT_BINARY" gc --dry-run --prune-unreachable --json || fatal_stage "$stage" 'inventory_1_failed_or_mutated'
  record_command "$stage" inventory-2 "$scope" "$private" none "$PRODUCT_BINARY" gc --dry-run --prune-unreachable --json || fatal_stage "$stage" 'inventory_2_failed_or_mutated'
  jq -e --arg old_tree "$old_tree" '
    . as $report |
    (keys == ["diagnostic_only","limits","mutation_authority","objects","operation","read_only","schema_version","shallow_boundaries","stats","status","summary"]) and
    .schema_version == 1 and .operation == "unreachable_object_inventory" and .status == "dry_run" and .read_only == true and .diagnostic_only == true and .mutation_authority == false and
    (.summary | keys == ["candidate_bytes","candidate_count","inventory_only_bytes","inventory_only_count","object_count","physical_bytes","protected_bytes","protected_count"] and
      .candidate_count == 0 and .candidate_bytes == 0 and
      .object_count == ($report.objects | length) and
      .physical_bytes == ([$report.objects[].physical_bytes] | add // 0) and
      .protected_count == ([$report.objects[] | select(.classification == "protected")] | length) and
      .protected_bytes == ([$report.objects[] | select(.classification == "protected") | .physical_bytes] | add // 0) and
      .inventory_only_count == ([$report.objects[] | select(.classification == "inventory_only")] | length) and
      .inventory_only_bytes == ([$report.objects[] | select(.classification == "inventory_only") | .physical_bytes] | add // 0) and
      .object_count == (.candidate_count + .protected_count + .inventory_only_count) and
      .physical_bytes == (.candidate_bytes + .protected_bytes + .inventory_only_bytes)) and
    (.objects | all(keys == ["classification","hash","kind","physical_bytes","reason"] and
      (.kind == "raw" or .kind == "commit" or .kind == "tree" or .kind == "chunk" or
       .kind == "manifest" or .kind == "normalized_unit" or .kind == "embedding" or
       .kind == "toollock" or .kind == "prepared" or .kind == "image") and
      (.hash | test("^sha256:[0-9a-f]{64}$")) and
      (.physical_bytes | type == "number" and . >= 0) and
      (.classification == "candidate" or .classification == "protected" or .classification == "inventory_only") and
      (.reason | type == "string" and length > 0))) and
    (.objects == (.objects | sort_by(.kind, .hash))) and ([.objects[] | [.kind,.hash]] | unique | length == length) and
    (.limits | keys == ["max_depth","max_directory_entries","max_history_steps","max_manifest_units","max_name_bytes","max_objects","max_physical_bytes","max_receipts","max_refs","max_verified_bytes"] and all(type == "number" and . > 0)) and
    # Equal full-scan counters are a read-only stability gate, never mutation authority.
    (.stats | keys == ["inventory_pass","stability_pass"] and
      (.inventory_pass | keys == ["directory_entries","history_steps","manifest_units","objects","physical_bytes","receipts","refs","verified_bytes"]) and
      (.stability_pass | keys == ["directory_entries","history_steps","manifest_units","objects","physical_bytes","receipts","refs","verified_bytes"]) and
      ([.inventory_pass[],.stability_pass[]] | all(type == "number" and . >= 0 and floor == .)) and
      .inventory_pass == .stability_pass) and
    (.shallow_boundaries | type == "array" and all(keys == ["commit_hash","tree_hash"] and
      (.commit_hash | test("^sha256:[0-9a-f]{64}$")) and (.tree_hash | test("^sha256:[0-9a-f]{64}$")))) and
    ([.objects[] | select(.hash == $old_tree and .kind == "tree" and .classification == "protected" and .reason == "retention_gc_owned")] | length == 1)' "$inventory1" >/dev/null || fatal_stage "$stage" 'inventory_1_predicate_failed'
  cmp -s "$inventory1" "$inventory2" || fatal_stage "$stage" 'inventory_output_not_byte_identical'
  [[ "$(sha256 "$STAGES/$stage/inventory-1/fixture-manifest.before.json")" == "$(sha256 "$STAGES/$stage/inventory-1/fixture-manifest.after.json")" && "$(sha256 "$STAGES/$stage/inventory-2/fixture-manifest.before.json")" == "$(sha256 "$STAGES/$stage/inventory-2/fixture-manifest.after.json")" ]] || fatal_stage "$stage" 'inventory_mutated_protected_manifest'
  [[ "$(sha256 "$STAGES/$stage/retention-plan-2/fixture-manifest.after.json")" == "$(sha256 "$STAGES/$stage/inventory-1/fixture-manifest.before.json")" && "$(sha256 "$STAGES/$stage/inventory-1/fixture-manifest.after.json")" == "$(sha256 "$STAGES/$stage/inventory-2/fixture-manifest.before.json")" ]] || fatal_stage "$stage" 'inventory_cross_invocation_state_changed'
  jq -n --arg old_commit "$old_commit" --arg old_tree "$old_tree" \
    --arg current_commit "$current_commit" --arg current_tree "$current_tree" \
    --arg retention_plan_1_sha256 "$(sha256 "$retention_plan1")" --arg retention_plan_2_sha256 "$(sha256 "$retention_plan2")" --arg output_sha256 "$(sha256 "$inventory1")" \
    --arg binary_sha256 "$EXPECTED_BINARY_SHA256" --arg approved_config_sha256 "$EXPECTED_APPROVED_MANUAL_CONFIG_SHA256" \
    --arg config_sha256 "$(sha256 "$scope/.kio/config.toml")" \
    --arg old_document_sha256 "$(awk '{print $1}' "$STAGES/$stage/document.old.sha256")" \
    --arg current_document_sha256 "$(awk '{print $1}' "$STAGES/$stage/document.current.sha256")" \
    --slurpfile run "$EVIDENCE_ROOT/run.json" \
    '{fixed_binding:$run[0].expected_binding,downloaded_binary_sha256:$binary_sha256,approved_config_sha256:$approved_config_sha256,config_sha256:$config_sha256,documents:{old_sha256:$old_document_sha256,current_sha256:$current_document_sha256},old:{commit:$old_commit,tree:$old_tree},current:{commit:$current_commit,tree:$current_tree},retention_plans:{first_sha256:$retention_plan_1_sha256,second_sha256:$retention_plan_2_sha256,real_candidate:true},candidate_count:0,old_tree_classification:"protected",old_tree_reason:"retention_gc_owned",predicates:{index_mutation_path_sets_closed:true,offline_index_network_allowed_false:true,approval_config_transition_exact:true,second_approval_log_consent_record_and_config_unchanged:true,second_consent_lock_update_closed:true,final_manual_config_restored:true,exact_schema:true,exact_summary:true,diagnostic_only_true:true,mutation_authority_false:true,retention_pass_diagnostic_shapes_valid:true,retention_diagnostics_repeat_stable:true,inventory_pass_stats_equal_read_only_stability_gate:true,objects_sorted_unique:true,outputs_byte_identical:true,retention_plan_1_no_write:true,retention_plan_2_no_write:true,retention_plan_cross_invocation_state_unchanged:true,inventory_1_no_write:true,inventory_2_no_write:true,inventory_cross_invocation_state_unchanged:true,public_cli_real_unreachable_candidate_exercised:false},output_sha256:$output_sha256}' > "$STAGES/$stage/assertions.json"
  stage_command_manifest "$stage" "$STAGES/$stage/command-manifest.json" init index-old index-current retention-plan-1 retention-plan-2 inventory-1 inventory-2 || fatal_stage "$stage" 'command_manifest_failed'
  stage_primary_invocation "$stage" inventory-2 || fatal_stage "$stage" 'primary_invocation_receipt_failed'
  stage_manifest_summary "$stage" "$STAGES/$stage/init/fixture-manifest.before.json" "$STAGES/$stage/inventory-2/fixture-manifest.after.json" 'public init and two approve-index transitions, then exact config restoration; two retention plans and two inventories read-only' init harness-config harness-document-old index-old harness-document-current index-current harness-config-final retention-plan-1 retention-plan-2 inventory-1 inventory-2 || fatal_stage "$stage" 'stage_manifest_summary_failed'
  jq -n --arg stage "$stage" --arg before "$(sha256 "$STAGES/$stage/init/fixture-manifest.before.json")" --arg after "$(sha256 "$STAGES/$stage/inventory-2/fixture-manifest.after.json")" \
    --slurpfile assertions "$STAGES/$stage/assertions.json" --slurpfile commands "$STAGES/$stage/command-manifest.json" \
    --slurpfile observations "$STAGES/$stage/observation-log-manifest.json" \
    '{schema:"kio.phase4.stage-result.v1",stage:$stage,terminal_status:"blocked",reason:"public_cli_unreachable_candidate_unconstructable",primary_invocation:"inventory-2",stop_rule:"known_coverage_blocker_continue_independent_stages",fixture_manifest_before_sha256:$before,fixture_manifest_after_sha256:$after,commands:$commands[0].commands,observations:$observations[0],predicates:$assertions[0]}' > "$STAGES/$stage/result.json"
  complete_stage "$stage" blocked 'public_cli_unreachable_candidate_unconstructable' "$STAGES/$stage/assertions.json"
}

if [[ "$CHECKPOINT" == m1-m8 ]]; then
  run_m1
  run_m8
  print -- "M1 passed; M8 blocked only at the public-CLI unreachable-candidate coverage boundary"
else
  run_m6_m7
  print -- "M6 and M7 passed on the frozen M1/M8 continuation fixture"
fi
