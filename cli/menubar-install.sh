#!/bin/bash
# Build by default. Installation and next-login launch registration require explicit flags.
set -euo pipefail
install=false
login=false
scratch="${SWITCHBACK_MENUBAR_SCRATCH:-}"
while [ "$#" -gt 0 ]; do
  case "$1" in
    --install) install=true ;;
    --launch-at-login) login=true ;;
    --scratch-path) shift; scratch="${1:?Missing scratch path}" ;;
    --help) echo "Usage: menubar-install.sh [--scratch-path EXTERNAL_BUILD_DIR] [--install [--launch-at-login]]"; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done
if $login && ! $install; then echo "--launch-at-login requires --install" >&2; exit 2; fi
if [ -z "$scratch" ]; then
  echo "Set --scratch-path or SWITCHBACK_MENUBAR_SCRATCH to an external build directory." >&2; exit 2
fi
case "$scratch" in /*) ;; *) echo "Build scratch must be an absolute path" >&2; exit 2 ;; esac
scratch="$(python3 -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$scratch")"
repo="$(cd "$(dirname "$0")/.." && pwd)"
package="$repo/apps/menubar"
mkdir -p "$scratch"
swift build -c release --package-path "$package" --scratch-path "$scratch/swift"
binary_dir="$(swift build -c release --package-path "$package" --scratch-path "$scratch/swift" --show-bin-path)"
bundle_dir="$(mktemp -d "$scratch/bundle.XXXXXXXX")"
app="$bundle_dir/SwitchbackBar.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$binary_dir/SwitchbackBar" "$app/Contents/MacOS/SwitchbackBar"
cat > "$app/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>dev.switchback.menubar</string>
<key>CFBundleName</key><string>Switchback Bar</string>
<key>CFBundleExecutable</key><string>SwitchbackBar</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>1.0.0</string>
<key>CFBundleVersion</key><string>1</string>
<key>LSMinimumSystemVersion</key><string>14.0</string>
<key>LSUIElement</key><true/>
<key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
printf '%s\n' 'switchback-menubar-owned-v1' > "$app/Contents/Resources/switchback-owner"
codesign --force --sign - "$app"
codesign --verify --strict "$app"
echo "Built: $app"
if ! $install; then exit 0; fi
runtime="${SWITCHBACK_RUNTIME_ROOT:-${SB_RUNTIME_ROOT:-$HOME/.switchback}}"
python3 "$package/install.py" "$runtime" "$app" "$login"
echo "Launch the installed app explicitly when ready."
