#!/bin/zsh
# install.sh — link the sb CLI + wrappers into ~/.local/bin so you can run them
# from anywhere. Symlinks (not copies) so the repo stays the source of truth:
# edit the files here, your live commands update. Re-run any time; it's idempotent.
#
#   ./cli/install.sh            # symlink into ~/.local/bin
#   PREFIX=~/bin ./cli/install.sh   # somewhere else
set -euo pipefail

here="${0:A:h}"                       # this cli/ dir
PREFIX="${PREFIX:-$HOME/.local/bin}"
mkdir -p "$PREFIX"

link() { ln -sf "$1" "$PREFIX/$2"; echo "  linked $2 -> $1"; }

echo "Installing sb CLI into $PREFIX:"
link "$here/sb" sb
for w in "$here"/wrappers/*(.N); do link "$w" "${w:t}"; done

# Seed example configs (only if absent — never clobber yours).
mkdir -p "$HOME/.local/state/switchback"
seed() {  # seed <src> <dest>
  if [[ -e "$2" ]]; then echo "  kept existing $2"; else
    mkdir -p "${2:h}"; cp "$1" "$2"; echo "  seeded $2"
  fi
}
seed "$here/examples/sb.env.example"  "$HOME/.config/switchback/sb.env"
seed "$here/examples/pi-models.json"  "$HOME/.pi/agent/models.json"

# Relay config: substitute __HOME__ → your home at install time (paths aren't
# env-expanded by the relay). Only seeds if you don't already have one.
relay_cfg="$HOME/.config/switchback/switchback.yaml"
if [[ -e "$relay_cfg" ]]; then echo "  kept existing $relay_cfg"; else
  mkdir -p "${relay_cfg:h}"
  sed "s#__HOME__#$HOME#g" "$here/examples/switchback.yaml" > "$relay_cfg"
  echo "  seeded $relay_cfg (relay config — taps + scout pool)"
fi

cat <<'EOF'

Done. Make sure ~/.local/bin is on your PATH, then start the relay and check:
  export OPENROUTER_API_KEY=...        # for scout/opencode/pi lanes (taps need no key)
  switchback serve --config ~/.config/switchback/switchback.yaml &   # or build: cargo run -p sb-server -- serve --config ...
  sb doctor                            # verifies relay + taps + tools + catalog
  sb                                   # interactive menu

Taps run on :18770 (claude) / :18771 (codex); gateway on :18765. See cli/README.md.
EOF
