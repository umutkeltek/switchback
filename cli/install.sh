#!/bin/zsh
# install.sh — build/install Switchback and initialize its owned runtime root.
#
#   ./cli/install.sh
#   PREFIX=~/bin ./cli/install.sh
#   SWITCHBACK_RUNTIME_ROOT=/path/to/.switchback ./cli/install.sh
#   SB_BIN=/path/to/prebuilt/switchback ./cli/install.sh
#
# Existing config, secrets, manifests, and runtime data are never overwritten.
set -euo pipefail

here="${0:A:h}"
root="${SWITCHBACK_ROOT:-${here:h}}"
runtime="${SWITCHBACK_RUNTIME_ROOT:-${SB_RUNTIME_ROOT:-$HOME/.switchback}}"
PREFIX="${PREFIX:-$HOME/.local/bin}"
config_root="$runtime/config"
engine_only="${SB_INSTALL_ENGINE_ONLY:-0}"
native_claude_only="${SB_INSTALL_NATIVE_CLAUDE_ONLY:-0}"

case "$engine_only" in
  0|1) ;;
  *) print -u2 "error: SB_INSTALL_ENGINE_ONLY must be 0 or 1"; exit 64 ;;
esac
case "$native_claude_only" in
  0|1) ;;
  *) print -u2 "error: SB_INSTALL_NATIVE_CLAUDE_ONLY must be 0 or 1"; exit 64 ;;
esac
if [[ "$native_claude_only" == 1 && "$engine_only" == 1 ]]; then
  print -u2 "error: native-Claude-only and engine-only modes are mutually exclusive"
  exit 64
fi

link_command() {
  ln -sf "$1" "$PREFIX/$2"
  echo "  linked $2 -> $1"
}

link_profile_wrapper() {
  local destination="$PREFIX/$2"
  if [[ -f "$destination" && ! -L "$destination" && -x "$destination" ]] \
    && grep -Fqx "# switchback-owned: launch-profile-wrapper@1" "$destination"; then
    echo "  kept profile-owned wrapper $destination"
    return
  fi
  link_command "$1" "$2"
}
seed() {  # seed <src> <dest> [private]
  if [[ -e "$2" ]]; then
    echo "  kept existing $2"
    return
  fi
  mkdir -p "${2:h}"
  cp "$1" "$2"
  [[ "${3:-}" == "private" ]] && chmod 600 "$2"
  echo "  seeded $2"
}

json_escape() {
  local value="$1"
  value="${value//\\/\\\\}"
  value="${value//\"/\\\"}"
  value="${value//$'\b'/\\b}"
  value="${value//$'\f'/\\f}"
  value="${value//$'\n'/\\n}"
  value="${value//$'\r'/\\r}"
  value="${value//$'\t'/\\t}"
  print -rn -- "$value"
}

sha256_file() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  elif command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    print -u2 "error: shasum or sha256sum is required to record install provenance"
    return 1
  fi
}

# Explicit repair seam for CLI owner drift. It updates only the installed
# engine and its provenance; config, launchers, wrappers, native clients, and
# service copies are outside this mode's authority. Its safety boundary is a
# trusted same-owner filesystem: it serializes concurrent installers and
# recovers process crashes, but does not claim hostile same-UID path-swap or
# power-loss durability.
if (( engine_only )); then
  engine_only_require_safe_dir() {
    local target_path="$1" label="$2" mode=""
    [[ -d "$target_path" && ! -L "$target_path" && -O "$target_path" ]] || {
      print -u2 "error: engine-only $label must be an owned, non-symlink directory: $target_path"
      return 74
    }
    if mode="$(stat -f '%Lp' "$target_path" 2>/dev/null)" || mode="$(stat -c '%a' "$target_path" 2>/dev/null)"; then
      [[ "$mode" == <-> ]] && (( (8#$mode & 8#22) == 0 )) || {
        print -u2 "error: engine-only $label must not be group/world writable: $target_path"
        return 74
      }
    else
      print -u2 "error: engine-only could not inspect $label mode: $target_path"
      return 74
    fi
  }

  engine_only_require_regular_or_absent() {
    local target_path="$1" label="$2" link_count=""
    if [[ -e "$target_path" || -L "$target_path" ]]; then
      [[ -f "$target_path" && ! -L "$target_path" && -O "$target_path" ]] || {
        print -u2 "error: engine-only $label must be an owned, non-symlink regular file: $target_path"
        return 74
      }
      if link_count="$(stat -f '%l' "$target_path" 2>/dev/null)" || link_count="$(stat -c '%h' "$target_path" 2>/dev/null)"; then
        [[ "$link_count" == 1 ]] || {
          print -u2 "error: engine-only $label must not have multiple hard links: $target_path"
          return 74
        }
      else
        print -u2 "error: engine-only could not inspect $label link count: $target_path"
        return 74
      fi
    fi
  }

  [[ -n "${SWITCHBACK_RUNTIME_ROOT:-}" ]] || {
    print -u2 "error: engine-only install requires explicit SWITCHBACK_RUNTIME_ROOT"
    exit 64
  }
  [[ "$runtime" == /* ]] || {
    print -u2 "error: engine-only install requires an existing absolute Switchback runtime"
    exit 64
  }
  engine_only_require_safe_dir "$runtime" "runtime root" || exit $?
  engine_only_require_safe_dir "$runtime/bin" "runtime bin parent" || exit $?
  engine_only_require_regular_or_absent "$runtime/manifest.json" "runtime manifest" || exit $?
  [[ -f "$runtime/manifest.json" ]] || {
    print -u2 "error: engine-only install requires an existing runtime manifest"
    exit 64
  }
  command -v jq >/dev/null 2>&1 || {
    print -u2 "error: engine-only install requires jq to validate runtime ownership"
    exit 69
  }
  jq -e -s \
    'length == 1 and (.[0] | type == "object") and .[0].schema == "switchback/runtime-manifest@1" and .[0].owner == "switchback"' \
    "$runtime/manifest.json" >/dev/null 2>&1 || {
    print -u2 "error: engine-only install requires a valid Switchback-owned runtime manifest"
    exit 64
  }
  [[ -n "${SB_BIN:-}" && "$SB_BIN" == /* && -x "$SB_BIN" ]] || {
    print -u2 "error: engine-only install requires explicit absolute executable SB_BIN"
    exit 64
  }
  engine_only_require_regular_or_absent "$SB_BIN" "source engine" || exit $?

  installed_engine="$runtime/bin/switchback-bin"
  provenance="$runtime/bin/install-provenance.json"
  lock_file="$runtime/bin/.engine-only-install.lock"
  journal_dir="$runtime/bin/.engine-only-install.journal"
  tmp_engine="$journal_dir/switchback-bin.new"
  tmp_provenance="$journal_dir/install-provenance.json.new"
  recovery_engine="$journal_dir/switchback-bin.old"
  recovery_provenance="$journal_dir/install-provenance.json.old"
  restore_engine="$journal_dir/switchback-bin.restore"
  restore_provenance="$journal_dir/install-provenance.json.restore"
  old_engine="$runtime/bin/switchback-bin.bak-engine-only"
  old_provenance="$runtime/bin/install-provenance.json.bak-engine-only"
  had_engine=0
  had_provenance=0
  prepared=0
  committed=0
  journal_owned=0
  kernel_lock_fd=""

  for target label in \
    "$installed_engine" "installed engine" \
    "$provenance" "install provenance" \
    "$old_engine" "engine backup" \
    "$old_provenance" "provenance backup"; do
    engine_only_require_regular_or_absent "$target" "$label" || exit $?
  done

  engine_only_clear_journal() {
    rm -f "$tmp_engine" "$tmp_provenance" "$recovery_engine" "$recovery_provenance" \
      "$restore_engine" "$restore_provenance" \
      "$journal_dir/had-engine" "$journal_dir/had-provenance" \
      "$journal_dir/expected-sha256" "$journal_dir/prepared" "$journal_dir/committed"
    rmdir "$journal_dir"
  }

  engine_only_validate_journal() {
    engine_only_require_safe_dir "$journal_dir" "recovery journal" || return $?
    for target label in \
      "$tmp_engine" "staged engine" \
      "$tmp_provenance" "staged provenance" \
      "$recovery_engine" "engine recovery preimage" \
      "$recovery_provenance" "provenance recovery preimage" \
      "$restore_engine" "engine restore staging" \
      "$restore_provenance" "provenance restore staging" \
      "$journal_dir/had-engine" "engine recovery marker" \
      "$journal_dir/had-provenance" "provenance recovery marker" \
      "$journal_dir/expected-sha256" "expected digest marker" \
      "$journal_dir/prepared" "prepared marker" \
      "$journal_dir/committed" "committed marker"; do
      engine_only_require_regular_or_absent "$target" "$label" || return $?
    done
  }

  engine_only_restore_preimages() {
    engine_only_validate_journal || return $?
    engine_only_require_regular_or_absent "$installed_engine" "installed engine recovery target" || return $?
    engine_only_require_regular_or_absent "$provenance" "provenance recovery target" || return $?
    if [[ -f "$journal_dir/had-engine" ]]; then
      [[ -f "$recovery_engine" ]] || {
        print -u2 "error: engine-only journal is missing engine preimage"
        return 74
      }
      cp "$recovery_engine" "$restore_engine" || return $?
      chmod 755 "$restore_engine" || return $?
    fi
    if [[ -f "$journal_dir/had-provenance" ]]; then
      [[ -f "$recovery_provenance" ]] || {
        print -u2 "error: engine-only journal is missing provenance preimage"
        return 74
      }
      cp "$recovery_provenance" "$restore_provenance" || return $?
      chmod 600 "$restore_provenance" || return $?
    fi
    if [[ -f "$journal_dir/had-engine" ]]; then mv "$restore_engine" "$installed_engine" || return $?; else rm -f "$installed_engine" || return $?; fi
    if [[ -f "$journal_dir/had-provenance" ]]; then mv "$restore_provenance" "$provenance" || return $?; else rm -f "$provenance" || return $?; fi
  }

  engine_only_recover_journal() {
    engine_only_validate_journal || return $?
    local expected_sha="" accept_committed=0
    [[ -f "$journal_dir/expected-sha256" ]] && IFS= read -r expected_sha < "$journal_dir/expected-sha256"
    if [[ -f "$journal_dir/committed" && -n "$expected_sha" && -f "$installed_engine" && -f "$provenance" ]]; then
      if [[ "$(sha256_file "$installed_engine")" == "$expected_sha" ]] && \
          grep -Fq "\"sha256\": \"$expected_sha\"" "$provenance"; then
        accept_committed=1
      fi
    fi
    if (( ! accept_committed )) && [[ -f "$journal_dir/prepared" ]]; then
      engine_only_restore_preimages || return $?
    fi
    engine_only_clear_journal
  }

  engine_only_cleanup() {
    set +e
    if (( journal_owned )); then
      local journal_ok=1
      if (( ! committed && prepared )); then
        engine_only_restore_preimages || journal_ok=0
      fi
      (( journal_ok )) && engine_only_clear_journal
      journal_owned=0
    fi
    if [[ -n "$kernel_lock_fd" ]]; then
      zsystem flock -u "$kernel_lock_fd" >/dev/null 2>&1
      kernel_lock_fd=""
    fi
  }

  if [[ ! -e "$lock_file" && ! -L "$lock_file" ]]; then
    ( setopt noclobber; : > "$lock_file" ) 2>/dev/null || true
  fi
  engine_only_require_regular_or_absent "$lock_file" "kernel lock file" || exit $?
  [[ -f "$lock_file" ]] || {
    print -u2 "error: engine-only could not create kernel lock file: $lock_file"
    exit 75
  }
  chmod 600 "$lock_file"
  zmodload zsh/system 2>/dev/null || {
    print -u2 "error: engine-only install requires zsh/system kernel locking"
    exit 75
  }
  zsystem flock -t 0 -f kernel_lock_fd "$lock_file" 2>/dev/null || {
    print -u2 "error: engine-only install already in progress: $lock_file"
    exit 75
  }
  trap engine_only_cleanup EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM HUP

  if [[ -e "$journal_dir" || -L "$journal_dir" ]]; then
    engine_only_recover_journal || exit $?
  fi
  mkdir "$journal_dir"
  journal_owned=1

  if [[ -f "$installed_engine" ]]; then
    cp "$installed_engine" "$recovery_engine"
    cp "$installed_engine" "$old_engine"
    : > "$journal_dir/had-engine"
    had_engine=1
  fi
  if [[ -f "$provenance" ]]; then
    cp "$provenance" "$recovery_provenance"
    cp "$provenance" "$old_provenance"
    : > "$journal_dir/had-provenance"
    had_provenance=1
  fi

  cp "$SB_BIN" "$tmp_engine"
  chmod 755 "$tmp_engine"
  engine_sha256="$(sha256_file "$tmp_engine")"
  version="unknown"
  if version_output="$("$tmp_engine" --version 2>/dev/null)"; then
    version="${version_output%%$'\n'*}"
  fi
  git_commit="${SB_BUILD_COMMIT:-unknown}"
  source_engine="${SB_BIN:A}"
  installed_engine_path="${installed_engine:A}"
  runtime_path="${runtime:A}"
  installed_at="$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
  {
    print -r -- '{'
    print -r -- '  "schema": "switchback/install-provenance@1",'
    print -r -- "  \"runtime_root\": \"$(json_escape "$runtime_path")\","
    print -r -- "  \"version\": \"$(json_escape "$version")\","
    print -r -- "  \"git_commit\": \"$(json_escape "$git_commit")\","
    print -r -- "  \"source_engine\": \"$(json_escape "$source_engine")\","
    print -r -- "  \"installed_engine\": \"$(json_escape "$installed_engine_path")\","
    print -r -- "  \"sha256\": \"$engine_sha256\","
    print -r -- '  "services_refreshed": [],'
    print -r -- '  "install_scope": "engine_and_provenance_only",'
    print -r -- "  \"installed_at\": \"$installed_at\""
    print -r -- '}'
  } > "$tmp_provenance"
  chmod 600 "$tmp_provenance"

  print -r -- "$engine_sha256" > "$journal_dir/expected-sha256"
  : > "$journal_dir/prepared"
  prepared=1
  mv "$tmp_engine" "$installed_engine"
  mv "$tmp_provenance" "$provenance"
  [[ "$(sha256_file "$installed_engine")" == "$engine_sha256" ]] || {
    print -u2 "error: installed engine checksum does not match provenance"
    exit 74
  }
  : > "$journal_dir/committed"
  committed=1
  engine_only_cleanup
  trap - EXIT INT TERM HUP
  print -r -- "installed engine + provenance only: $installed_engine"
  exit 0
fi

if [[ "$native_claude_only" == 1 ]]; then
  # Repair only an existing runtime; never initialize or refresh service copies.
  if [[ -z "${SWITCHBACK_RUNTIME_ROOT:-}" || "$runtime" != /* || ! -d "$runtime" || -L "$runtime" ||
        ! -d "$runtime/bin" || -L "$runtime/bin" || ! -d "$PREFIX" || -L "$PREFIX" ||
        ! -f "$runtime/manifest.json" || -L "$runtime/manifest.json" ]]; then
    print -u2 "error: native-Claude-only requires an explicit existing owned runtime and install directory"
    exit 64
  fi
  if ! jq -e '.schema == "switchback/runtime-manifest@1" and .owner == "switchback"' "$runtime/manifest.json" >/dev/null 2>&1; then
    print -u2 "error: native-Claude-only requires a Switchback-owned runtime manifest"
    exit 64
  fi
  if [[ ! -f "$PREFIX/claude" || -L "$PREFIX/claude" ||
        ! -f "$runtime/bin/native-claude-entrypoint-provenance.json" || -L "$runtime/bin/native-claude-entrypoint-provenance.json" ]]; then
    print -u2 "error: native-Claude-only repairs an existing owned entrypoint; use the full installer for initial setup"
    exit 64
  fi
else
  mkdir -p "$PREFIX" "$config_root"
  chmod 700 "$runtime" "$config_root" 2>/dev/null || true
fi

# Native Claude is special: its vendor installer owns a public symlink that we
# replace with Switchback's Mode D entrypoint. Resolve and preserve the real
# executable before any install mutation, and refuse files whose ownership
# cannot be proven by the prior install provenance/hash.
native_claude_source="$here/entrypoints/claude"
native_claude_entry="${PREFIX:A}/claude"
native_claude_versions="${SB_NATIVE_CLAUDE_VERSIONS_DIR:-${HOME}/.local/share/claude/versions}"
native_claude_pin="${SB_NATIVE_CLAUDE_PIN_FILE:-${HOME}/.local/share/claude/.switchback-real}"
native_claude_provenance="$runtime/bin/native-claude-entrypoint-provenance.json"
native_claude_artifact="native-claude-mode-d-entrypoint@1"
native_claude_schema="switchback/native-claude-entrypoint-provenance@1"
native_claude_source_sha="$(sha256_file "$native_claude_source")"
native_claude_vendor="${SB_NATIVE_CLAUDE_BIN:-}"
install_native_claude=0

grep -Fqx "# switchback-owned: ${native_claude_artifact}" "$native_claude_source" || {
  print -u2 "error: tracked native Claude entrypoint is missing its Switchback ownership marker: $native_claude_source"
  exit 1
}

if [[ -n "$native_claude_vendor" ]]; then
  [[ -x "$native_claude_vendor" ]] || {
    print -u2 "error: SB_NATIVE_CLAUDE_BIN is not executable: $native_claude_vendor"
    exit 1
  }
  native_claude_vendor="${native_claude_vendor:A}"
fi

if [[ -L "$native_claude_entry" ]]; then
  existing_target="${native_claude_entry:A}"
  versions_root="${native_claude_versions:A}"
  if [[ ! -x "$existing_target" ]]; then
    print -u2 "error: refusing to overwrite unowned native Claude entrypoint: $native_claude_entry (broken symlink)"
    exit 1
  fi
  if [[ -n "$native_claude_vendor" && "$existing_target" != "$native_claude_vendor" ]]; then
    print -u2 "error: refusing to overwrite unowned native Claude entrypoint: $native_claude_entry (target differs from SB_NATIVE_CLAUDE_BIN)"
    exit 1
  fi
  if [[ -z "$native_claude_vendor" && "$existing_target" != "${versions_root}/"* ]]; then
    print -u2 "error: refusing to overwrite unowned native Claude entrypoint: $native_claude_entry (target is outside the vendor versions directory)"
    exit 1
  fi
  native_claude_vendor="$existing_target"
  install_native_claude=1
elif [[ -e "$native_claude_entry" ]]; then
  [[ -f "$native_claude_entry" ]] || {
    print -u2 "error: refusing to overwrite unowned native Claude entrypoint: $native_claude_entry"
    exit 1
  }
  existing_sha="$(sha256_file "$native_claude_entry")"
  recorded_schema=""
  recorded_sha=""
  if [[ -r "$native_claude_provenance" ]]; then
    recorded_schema="$(awk -F'"' '/^[[:space:]]*"schema"[[:space:]]*:/ { print $4; exit }' "$native_claude_provenance")"
    recorded_sha="$(awk -F'"' '/^[[:space:]]*"sha256"[[:space:]]*:/ { print $4; exit }' "$native_claude_provenance")"
  fi
  if [[ "$existing_sha" != "$native_claude_source_sha" && ( "$recorded_schema" != "$native_claude_schema" || "$existing_sha" != "$recorded_sha" ) ]]; then
    print -u2 "error: refusing to overwrite unowned native Claude entrypoint: $native_claude_entry"
    print -u2 "  expected the tracked source hash or a hash recorded by $native_claude_provenance"
    exit 1
  fi
  install_native_claude=1
fi

if [[ -z "$native_claude_vendor" && -r "$native_claude_pin" ]]; then
  native_claude_vendor="$(<"$native_claude_pin")"
  [[ -x "$native_claude_vendor" ]] && native_claude_vendor="${native_claude_vendor:A}" || native_claude_vendor=""
fi
if [[ -z "$native_claude_vendor" && -d "$native_claude_versions" ]]; then
  native_claude_vendor="$(find "$native_claude_versions" -maxdepth 1 -type f -perm -u+x 2>/dev/null | sort -V | tail -1)"
  [[ -n "$native_claude_vendor" ]] && native_claude_vendor="${native_claude_vendor:A}"
fi
if [[ -n "$native_claude_vendor" ]]; then
  [[ "$native_claude_vendor" != "$native_claude_entry" ]] || {
    print -u2 "error: native Claude vendor pin resolves back to the public entrypoint: $native_claude_entry"
    exit 1
  }
  install_native_claude=1
elif (( install_native_claude )); then
  print -u2 "error: cannot update the owned native Claude entrypoint without an executable vendor pin"
  print -u2 "  set SB_NATIVE_CLAUDE_BIN to the real Claude Code binary and rerun"
  exit 1
fi

install_native_claude_entrypoint() {
  local installed_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  mkdir -p "${native_claude_pin:h}"
  tmp_native_pin="${native_claude_pin:h}/.${native_claude_pin:t}.$$.tmp"
  print -r -- "$native_claude_vendor" > "$tmp_native_pin"
  chmod 600 "$tmp_native_pin"
  mv "$tmp_native_pin" "$native_claude_pin"

  tmp_native_entry="${native_claude_entry:h}/.${native_claude_entry:t}.$$.tmp"
  cp "$native_claude_source" "$tmp_native_entry"
  chmod 755 "$tmp_native_entry"
  mv "$tmp_native_entry" "$native_claude_entry"
  native_claude_installed_sha="$(sha256_file "$native_claude_entry")"

  tmp_native_provenance="$runtime/bin/.native-claude-entrypoint-provenance.$$.tmp"
  {
    print -r -- '{'
    print -r -- "  \"schema\": \"$native_claude_schema\","
    print -r -- "  \"artifact\": \"$native_claude_artifact\","
    print -r -- "  \"source_path\": \"$(json_escape "${native_claude_source:A}")\","
    print -r -- "  \"source_sha256\": \"$native_claude_source_sha\","
    print -r -- "  \"installed_path\": \"$(json_escape "${native_claude_entry:A}")\","
    print -r -- "  \"sha256\": \"$native_claude_installed_sha\","
    print -r -- "  \"vendor_binary\": \"$(json_escape "$native_claude_vendor")\","
    print -r -- "  \"installed_at\": \"$installed_at\""
    print -r -- '}'
  } > "$tmp_native_provenance"
  chmod 600 "$tmp_native_provenance"
  mv "$tmp_native_provenance" "$native_claude_provenance"
  echo "  installed owned native Claude entrypoint -> $native_claude_entry"
  echo "  pinned real Claude binary -> $native_claude_vendor"
}

if [[ "$native_claude_only" == 1 ]]; then
  (( install_native_claude )) || { print -u2 "error: no owned native Claude entrypoint to repair"; exit 1; }
  install_native_claude_entrypoint
  print -r -- "installed native Claude entrypoint + pin + provenance only; services unchanged"
  exit 0
fi

# Resolve a current engine before installing wrappers. A caller can provide a
# verified prebuilt binary with SB_BIN; otherwise a source install builds the
# checkout instead of silently reusing an unrelated command from PATH.
engine="${SB_BIN:-}"
if [[ -n "$engine" ]]; then
  [[ -x "$engine" ]] || { print -u2 "error: SB_BIN is not executable: $engine"; exit 1; }
else
  command -v cargo >/dev/null 2>&1 || {
    print -u2 "error: cargo is required for a source install (or set SB_BIN to a prebuilt binary)"
    exit 1
  }
  echo "Building switchback from $root:"
  (cd "$root" && cargo build --release -p sb-server)
  engine="$root/target/release/switchback"
fi

# Seed only absent files. The relay template is installed before `setup` so the
# runtime initializer validates and preserves it instead of replacing it with
# the credential-free quickstart template.
seed "$here/examples/sb.env.example" "$config_root/sb.env" private
seed "$here/examples/pi-models.json" "$HOME/.pi/agent/models.json"

relay_cfg="$config_root/switchback.yaml"
if [[ -e "$relay_cfg" ]]; then
  echo "  kept existing $relay_cfg"
else
  sed \
    -e "s#__HOME__#$HOME#g" \
    -e "s#__SWITCHBACK_ROOT__#$root#g" \
    -e "s#__SWITCHBACK_RUNTIME_ROOT__#$runtime#g" \
    "$here/examples/relay.example.yaml" > "$relay_cfg"
  chmod 600 "$relay_cfg"
  echo "  seeded $relay_cfg (relay config — taps + scout pool)"
fi

# First install owns the runtime layout. Upgrades must not re-run full setup:
# setup validates the live config and may resolve interactive credential
# sources such as macOS Keychain, which can hang a non-interactive installer.
runtime_manifest="$runtime/manifest.json"
if [[ -f "$runtime_manifest" ]]; then
  echo "  kept existing runtime layout $runtime_manifest"
else
  SWITCHBACK_RUNTIME_ROOT="$runtime" "$engine" --json setup --root "$runtime" >/dev/null
fi

# Keep the real executable and its relocatable launcher inside the owned
# runtime tree. The launcher derives the runtime from its own installed path,
# so a symlinked command works from any current directory.
installed_engine="$runtime/bin/switchback-bin"
tmp_engine="$runtime/bin/.switchback-bin.$$.tmp"
cp "$engine" "$tmp_engine"
chmod 755 "$tmp_engine"
mv "$tmp_engine" "$installed_engine"
engine_sha256="$(sha256_file "$installed_engine")"

# Long-lived services run their OWN copy of the engine, not the launcher:
# `ai.switchback.scout` execs switchback-scout-bin, `ai.switchback.mode-d`
# execs switchback-mode-d-bin. Installing only switchback-bin upgrades the CLI
# and leaves every server on whatever build was last copied by hand — measured
# 2026-08-01, the gateway answered `0 accounts` from a fully populated database
# because its binary was 23 hours behind the CLI, and nothing reported the
# divergence. Refresh each service copy that already exists; never create one,
# which would invent a service the operator never installed.
typeset -a refreshed_services
for service_bin in "$runtime"/bin/switchback-*-bin(.N); do
  service_name="${service_bin:t}"
  if [[ "$(sha256_file "$service_bin")" == "$engine_sha256" ]]; then
    echo "  service $service_name already current"
    continue
  fi
  # One rolling backup per service. The previous convention stamped each with a
  # commit and never pruned, leaving 8 copies of a 23MB binary behind.
  cp "$service_bin" "$service_bin.bak-previous"
  tmp_service="$runtime/bin/.${service_name}.$$.tmp"
  cp "$installed_engine" "$tmp_service"
  chmod 755 "$tmp_service"
  mv "$tmp_service" "$service_bin"
  refreshed_services+=("$service_name")
  echo "  refreshed $service_name (previous kept as $service_name.bak-previous)"
done

# Replacing the file does not replace the RUNNING process: a loaded service
# holds the old inode until it is restarted. Say so loudly — silence here is
# the whole failure this refresh exists to end.
typeset -a services_needing_restart
if command -v launchctl >/dev/null 2>&1; then
  for service_name in $refreshed_services; do
    label="ai.switchback.${${service_name#switchback-}%-bin}"
    launchctl list "$label" >/dev/null 2>&1 && services_needing_restart+=("$label")
  done
fi

version="unknown"
if version_output="$("$installed_engine" --version 2>/dev/null)"; then
  version="${version_output%%$'\n'*}"
fi
git_commit="${SB_BUILD_COMMIT:-}"
if [[ -z "$git_commit" ]] && command -v git >/dev/null 2>&1; then
  git_commit="$(git -C "$root" rev-parse --verify HEAD 2>/dev/null || true)"
fi
git_commit="${git_commit:-unknown}"
source_engine="${engine:A}"
installed_engine_path="${installed_engine:A}"
runtime_path="${runtime:A}"
installed_at="$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
provenance="$runtime/bin/install-provenance.json"
tmp_provenance="$runtime/bin/.install-provenance.$$.tmp"
{
  print -r -- '{'
  print -r -- '  "schema": "switchback/install-provenance@1",'
  print -r -- "  \"runtime_root\": \"$(json_escape "$runtime_path")\","
  print -r -- "  \"version\": \"$(json_escape "$version")\","
  print -r -- "  \"git_commit\": \"$(json_escape "$git_commit")\","
  print -r -- "  \"source_engine\": \"$(json_escape "$source_engine")\","
  print -r -- "  \"installed_engine\": \"$(json_escape "$installed_engine_path")\","
  print -r -- "  \"sha256\": \"$engine_sha256\","
  # Which service copies this install brought up to `sha256`. An empty list
  # means every service binary already matched, not that none were checked.
  typeset -a refreshed_json
  for service_name in $refreshed_services; do
    refreshed_json+=("\"$(json_escape "$service_name")\"")
  done
  print -r -- "  \"services_refreshed\": [${(j:, :)refreshed_json}],"
  print -r -- "  \"installed_at\": \"$installed_at\""
  print -r -- '}'
} > "$tmp_provenance"
chmod 600 "$tmp_provenance"
mv "$tmp_provenance" "$provenance"

launcher="$runtime/bin/switchback"
tmp_launcher="$runtime/bin/.switchback-launcher.$$.tmp"
cat > "$tmp_launcher" <<'EOF_LAUNCHER'
#!/bin/zsh
set -euo pipefail

launcher="${0:A}"
launcher_dir="${launcher:h}"
inferred_runtime="${launcher_dir:h}"
runtime="${SWITCHBACK_RUNTIME_ROOT:-${SB_RUNTIME_ROOT:-$inferred_runtime}}"
runtime_env="$runtime/config/sb.env"
[[ -f "$runtime_env" ]] && source "$runtime_env"

# A pre-ownership install may keep an operator env elsewhere. Loading it is
# explicit so the canonical runtime remains the default authority.
legacy_env="${SWITCHBACK_LEGACY_ENV:-}"
if [[ -n "$legacy_env" && -f "$legacy_env" && "${legacy_env:A}" != "${runtime_env:A}" ]]; then
  source "$legacy_env"
fi

# Runtime env files may contain historical root exports; the launcher location
# (or an explicit process override) is authoritative for this execution.
export SWITCHBACK_RUNTIME_ROOT="$runtime"
export SB_RUNTIME_ROOT="$runtime"

engine="$launcher_dir/switchback-bin"
[[ -x "$engine" ]] || {
  print -u2 "error: Switchback engine is missing or not executable: $engine"
  exit 127
}
exec "$engine" "$@"
EOF_LAUNCHER
chmod 755 "$tmp_launcher"
mv "$tmp_launcher" "$launcher"

echo "Installing Switchback commands into $PREFIX:"
link_command "$launcher" switchback
link_command "$here/sb" sb
for w in "$here"/wrappers/*(.N); do link_profile_wrapper "$w" "${w:t}"; done

if (( install_native_claude )); then
  install_native_claude_entrypoint
else
  echo "  skipped native Claude entrypoint (no vendor Claude binary found)"
fi

# Compatibility only: canonical config is runtime/config. New installs receive
# the conventional ~/.config path as a symlink; an existing directory or a
# different symlink is never moved or replaced implicitly.
legacy_config="$HOME/.config/switchback"
if [[ "$legacy_config" != "$config_root" ]]; then
  if [[ ! -e "$legacy_config" && ! -L "$legacy_config" ]]; then
    mkdir -p "${legacy_config:h}"
    ln -s "$config_root" "$legacy_config"
    echo "  linked $legacy_config -> $config_root"
  elif [[ -L "$legacy_config" && "${legacy_config:A}" == "${config_root:A}" ]]; then
    echo "  kept compatibility link $legacy_config"
  else
    print -u2 "warning: kept existing $legacy_config; canonical config is $config_root"
  fi
fi

if (( ${#services_needing_restart} )); then
  print -u2 ""
  print -u2 "WARNING: these services still run the PREVIOUS binary until restarted:"
  for label in $services_needing_restart; do
    print -u2 "  launchctl kickstart -k gui/$(id -u)/$label"
  done
  print -u2 "  (the relay is also 'sb restart')"
fi

cat <<EOF_DONE

Done.
  runtime: $runtime
  config:  $relay_cfg
  binary:  $installed_engine
  provenance: $provenance
  services refreshed: ${refreshed_services:-none (all current)}

Make sure $PREFIX is on PATH, then:
  export OPENROUTER_API_KEY=...        # scout/free lanes; taps need no key
  switchback serve --config "$relay_cfg" &
  sb doctor
  sb

Inspect ownership any time with: switchback paths --json
Plan a user service without loading it: switchback setup launch-agent --plan
Taps run on :18770 (claude) / :18771 (codex); gateway on :18765.
EOF_DONE
