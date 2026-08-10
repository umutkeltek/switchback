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
mkdir -p "$PREFIX" "$config_root"
chmod 700 "$runtime" "$config_root" 2>/dev/null || true

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
