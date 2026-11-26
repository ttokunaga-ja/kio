#!/bin/bash
# Mountinfo paths stay escaped in plans, so even a tab or newline is one field.
wsl_mount_records() {
  LC_ALL=C awk '
    {
      for (i = 7; i <= NF && $i != "-"; i++) {}
      if (i + 3 != NF || $5 !~ /^\// || $3 !~ /^[0-9]+:[0-9]+$/) exit 1
      print $(i+1), $3, $6, $5
    }' "$1"
}

wsl_windows_type() {
  case $1 in
    9p|drvfs|plan9|virtio-plan9|virtiofs|fuse.drvfs|fuse.virtiofs) return 0 ;;
    *) return 1 ;;
  esac
}

wsl_readonly_gpu_mount() {
  [[ ,$2, == *,ro,* && ( $1 == /usr/lib/wsl/drivers || $1 == /usr/lib/wsl/lib ) ]]
}

wsl_mount_decode() {
  local rest=$1 prefix octal byte
  decoded=
  while [[ $rest == *\\* ]]; do
    prefix=${rest%%\\*}
    rest=${rest#*\\}
    octal=${rest:0:3}
    [[ $octal =~ ^[0-3][0-7][0-7]$ && $octal != 000 ]] || return 1
    printf -v byte '%b' "\\0$octal"
    decoded+=$prefix$byte
    rest=${rest:3}
  done
  decoded+=$rest
}

wsl_mount_covers() {
  local path=$1 root
  shift
  for root; do
    if [[ $path == "$root" || $path == "$root/"* ]]; then return 0; fi
  done
  return 1
}

wsl_mount_plan() {
  local records targets fs device opts encoded decoded
  local -a dirs=()
  records=$(wsl_mount_records "$1") || return 1
  targets=$(
    while read -r fs device opts encoded; do
      if wsl_readonly_gpu_mount "$encoded" "$opts"; then continue; fi
      if wsl_windows_type "$fs" || [[ $encoded == /mnt/wsl || $encoded == /mnt/wslg ]]; then
        printf '%s\n' "$encoded"
      fi
    done <<< "$records" | LC_ALL=C sort -u
  ) || return 1
  if [[ -z $targets ]]; then return 0; fi
  while read -r fs device opts encoded; do
    if wsl_windows_type "$fs" && ! wsl_readonly_gpu_mount "$encoded" "$opts"; then printf 'windows %s %s %s\n' "$encoded" "$fs" "$device"; fi
  done <<< "$records" | LC_ALL=C sort -u
  while IFS= read -r encoded; do
    wsl_mount_decode "$encoded" || return 1
    if wsl_mount_covers "$decoded" ${dirs[@]+"${dirs[@]}"}; then continue; fi
    if [[ -d $decoded ]]; then
      dirs+=("$decoded")
      printf 'directory %s\n' "$encoded"
    else
      printf 'file %s\n' "$encoded"
    fi
  done <<< "$targets"
}

wsl_mount_policy() {
  local kind encoded decoded
  while read -r kind encoded; do
    [[ $kind == directory || $kind == file ]] || continue
    wsl_mount_decode "$encoded" || return 1
    decoded=${decoded//\\/\\\\}
    decoded=${decoded//\"/\\\"}
    decoded=${decoded//$'\t'/\\t}
    decoded=${decoded//$'\n'/\\n}
    decoded=${decoded//%/%%}
    if [[ $kind == directory ]]; then
      [[ $decoded != / && $decoded != *:* ]] || { printf 'Cannot mask %s with tmpfs\n' "$encoded" >&2; return 1; }
      printf 'TemporaryFileSystem="%s:ro"\n' "$decoded"
    else
      printf 'InaccessiblePaths="%s"\n' "$decoded"
    fi
  done <<< "$1"
}

wsl_mount_paths() {
  local kind encoded decoded
  while read -r kind encoded; do
    [[ $kind == "$2" ]] || continue
    wsl_mount_decode "$encoded" || return 1
    printf '%s\0' "$decoded"
  done <<< "$1"
}

wsl_mount_device() { stat -Lc '%Hd:%Ld' -- "$1"; }

# A mask can leave an underlying record in mountinfo. Compare the effective device.
wsl_mount_verify() (
  local plan=$1 socket=$2 resolver=${4:-} records fs dev opts encoded decoded kind path effective
  local rest component found mount_kind mount_dev mount_options mount_path
  local -a entries
  records=$(wsl_mount_records "$3") || return 1
  shopt -s nullglob dotglob
  while read -r fs dev opts encoded; do
    wsl_windows_type "$fs" || continue
    if wsl_readonly_gpu_mount "$encoded" "$opts"; then continue; fi
    wsl_mount_decode "$encoded" || return 1
    if effective=$(wsl_mount_device "$decoded" 2>/dev/null) && [[ $effective == "$dev" ]]; then
      printf 'Windows filesystem still visible: %s\n' "$encoded" >&2
      return 1
    fi
  done <<< "$records"
  while read -r kind encoded _; do
    [[ -n $kind ]] || continue
    wsl_mount_decode "$encoded" || return 1
    path=$decoded
    if [[ -w $path ]]; then printf 'Writable mount target: %s\n' "$encoded" >&2; return 1; fi
    case $kind in
      windows) ;;
      file) [[ ! -r $path && ! -x $path ]] || return 1 ;;
      directory)
        effective=$(wsl_mount_device "$path") || return 1
        found=false
        while read -r mount_kind mount_dev mount_options mount_path; do
          if [[ $mount_kind == tmpfs && $mount_dev == "$effective" && $mount_path == "$encoded" &&
            ,$mount_options, == *,ro,* ]]; then found=true; break; fi
        done <<< "$records"
        $found || { printf 'Expected a read-only tmpfs at %s\n' "$encoded" >&2; return 1; }
        entries=("$path"/*)
        if [[ -n $resolver && $path == "${resolver%/*}" ]]; then
          [[ ${#entries[@]} == 1 && ${entries[0]} == "$resolver" &&
            -f $resolver && ! -L $resolver && -r $resolver && ! -w $resolver ]] || return 1
          found=false
          while read -r mount_kind mount_dev mount_options mount_path; do
            if [[ $mount_path == "$resolver" && ,$mount_options, == *,ro,* ]]; then
              found=true
              break
            fi
          done <<< "$records"
          $found || { printf 'Expected a read-only resolver bind at %s\n' "$resolver" >&2; return 1; }
        elif [[ $socket == "$path/"* ]]; then
          rest=${socket#"$path/"}
          while [[ -n $rest ]]; do
            component=${rest%%/*}
            [[ ${#entries[@]} == 1 && ${entries[0]} == "$path/$component" && ! -L $path/$component ]] || return 1
            path=$path/$component
            if [[ $rest != */* ]]; then break; fi
            rest=${rest#*/}
            [[ ! -w $path && $(wsl_mount_device "$path") == "$effective" ]] || return 1
            entries=("$path"/*)
          done
          [[ -S $path && ! -w $path ]] || return 1
        else
          [[ ${#entries[@]} == 0 ]] || return 1
        fi
        ;;
      *) return 1 ;;
    esac
  done <<< "$plan"
)

wsl_cdi_driver_file() {
  local path=$1 mode
  [[ $path == /usr/lib/wsl/* && -f $path ]] || return 1
  while [[ -n $path ]]; do
    mode=$(stat -c '%u:%a' -- "$path") || return 1
    [[ $mode == 0:* ]] && (( (8#${mode#*:} & 022) == 0 )) || return 1
    path=${path%/*}
  done
}

wsl_stage_cdi() {
  local plan=$1 spec=$2 destination=$3 installed=$4 path resolved name previous count=0
  local -a masks=()
  wsl_mount_paths "$plan" directory > "$destination/.masks" || return 1
  wsl_mount_paths "$plan" file >> "$destination/.masks" || return 1
  while IFS= read -r -d '' path; do masks+=("$path"); done < "$destination/.masks"
  jq -e '.devices | type == "array"' "$spec" >/dev/null || return 1
  jq -j '(.containerEdits.mounts[]?, .devices[].containerEdits.mounts[]?) |
    .hostPath | if type == "string" and startswith("/") then ., "\u0000" else error("invalid CDI host path") end' \
    "$spec" > "$destination/.paths" || return 1
  printf '{}\n' > "$destination/.map"
  while IFS= read -r -d '' path; do
    IFS= read -r -d '' resolved < <(realpath -ez -- "$path") || return 1
    if ! wsl_mount_covers "$path" ${masks[@]+"${masks[@]}"} &&
      ! wsl_mount_covers "$resolved" ${masks[@]+"${masks[@]}"}; then continue; fi
    if ! wsl_cdi_driver_file "$resolved"; then
      printf 'Untrusted WSL CDI driver file: %q\n' "$resolved" >&2
      return 1
    fi
    previous=$(jq -r --arg key "$resolved" '.[$key] // empty' "$destination/.map") || return 1
    if [[ -z $previous ]]; then
      name=driver-$count
      count=$((count + 1))
      install -m 0555 -- "$resolved" "$destination/$name" || return 1
      previous=$installed/$name
    fi
    jq --arg path "$path" --arg canonical "$resolved" --arg copy "$previous" \
      '. + {($path): $copy, ($canonical): $copy}' "$destination/.map" > "$destination/.next" || return 1
    mv -- "$destination/.next" "$destination/.map" || return 1
  done < "$destination/.paths"
  jq --slurpfile replacements "$destination/.map" \
    '(.containerEdits.mounts[]?.hostPath, .devices[].containerEdits.mounts[]?.hostPath) |= ($replacements[0][.] // .)' \
    "$spec" > "$destination/nvidia.json" || return 1
  chmod 0444 "$destination/nvidia.json" || return 1
  rm -- "$destination/.masks" "$destination/.paths" "$destination/.map"
}
