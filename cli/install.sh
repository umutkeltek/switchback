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
runtime="${SWITCHBACK_RUNTIME_ROOT:-${SB_RUNTIME_ROOT:-$root/.switchback}}"
PREFIX="${PREFIX:-$HOME/.local/bin}"
config_root="$runtime/config"
mkdir -p "$PREFIX" "$config_root"
chmod 700 "$runtime" "$config_root" 2>/dev/null || true

link() { ln -sf "$1" "$PREFIX/$2"; echo "  linked $2 -> $1"; }
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

# The Rust setup command is the owner of the runtime layout and manifest.
SWITCHBACK_RUNTIME_ROOT="$runtime" "$engine" --json setup --root "$runtime" >/dev/null

# Keep the real executable and its relocatable launcher inside the owned
# runtime tree. The launcher derives the runtime from its own installed path,
# so a symlinked command works from any current directory.
installed_engine="$runtime/bin/switchback-bin"
tmp_engine="$runtime/bin/.switchback-bin.$$.tmp"
cp "$engine" "$tmp_engine"
chmod 755 "$tmp_engine"
mv "$tmp_engine" "$installed_engine"

version="unknown"
if version_output="$("$installed_engine" --version 2>/dev/null)"; then
  version="${version_output%%$'\n'*}"
fi
git_commit="${SB_BUILD_COMMIT:-}"
if [[ -z "$git_commit" ]] && command -v git >/dev/null 2>&1; then
  git_commit="$(git -C "$root" rev-parse --verify HEAD 2>/dev/null || true)"
fi
git_commit="${git_commit:-unknown}"
engine_sha256="$(sha256_file "$installed_engine")"
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
link "$launcher" switchback
link "$here/sb" sb
for w in "$here"/wrappers/*(.N); do link "$w" "${w:t}"; done

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

cat <<EOF_DONE

Done.
  runtime: $runtime
  config:  $relay_cfg
  binary:  $installed_engine

Make sure $PREFIX is on PATH, then:
  export OPENROUTER_API_KEY=...        # scout/free lanes; taps need no key
  switchback serve --config "$relay_cfg" &
  sb doctor
  sb

Inspect ownership any time with: switchback paths --json
Taps run on :18770 (claude) / :18771 (codex); gateway on :18765.
EOF_DONE
