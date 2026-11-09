#!/bin/zsh
set -euo pipefail
export PYTHONDONTWRITEBYTECODE=1
exec python3 "${0:A:h}/sb_native_accounts.py"
