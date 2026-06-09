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
seed() {  # seed <src> <dest>
  if [[ -e "$2" ]]; then echo "  kept existing $2"; else
    mkdir -p "${2:h}"; cp "$1" "$2"; echo "  seeded $2"
  fi
}
seed "$here/examples/sb.env.example"  "$HOME/.config/switchback/sb.env"
seed "$here/examples/pi-models.json"  "$HOME/.pi/agent/models.json"

cat <<'EOF'

Done. Make sure ~/.local/bin is on your PATH, then:
  sb            # interactive menu
  sb status     # health at a glance
  sb modes      # what each command does

This CLI assumes a running Switchback relay on 127.0.0.1:18765 with transparent
taps on :18770 (claude) and :18771 (codex). See cli/README.md for that setup.
EOF
