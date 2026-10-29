#!/usr/bin/env bash
# Build, bundle and install Zipper into /Applications (macOS only), then
# register it with LaunchServices and the Services menu.
#
#   scripts/install.sh            # /Applications/Zipper.app
#   DEST=~/Applications scripts/install.sh
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ "$(uname)" != "Darwin" ]]; then
  echo "install.sh runs on the Mac; on the builder use scripts/cross-build.sh" >&2
  exit 1
fi

DEST="${DEST:-/Applications}"
cargo build --release -p zipper-app --target aarch64-apple-darwin
./bundle.sh >/dev/null

# Quit a running copy so the bundle can be replaced.
osascript -e 'tell application "Zipper" to quit' >/dev/null 2>&1 || true
rm -rf "$DEST/Zipper.app"
cp -R dist/Zipper.app "$DEST/Zipper.app"

LSREG=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
"$LSREG" -f "$DEST/Zipper.app" >/dev/null 2>&1 || true
# Refresh the Services menu so "Add to Archive with Zipper" shows up in Finder.
/System/Library/CoreServices/pbs -update >/dev/null 2>&1 || true

echo "Installed $DEST/Zipper.app"
echo "Make it the default for a file type: right-click a .zip → Get Info → Open With → Zipper → Change All,"
echo "or use the button in Zipper → Settings…"
echo "If the Finder service does not appear: System Settings → Keyboard → Keyboard Shortcuts… → Services → Files and Folders."
