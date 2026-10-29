#!/usr/bin/env bash
# Turn the compiled binary into Zipper.app and ad-hoc sign it.
#
# Works on macOS (codesign) and on Linux (rcodesign, from `cargo install apple-codesign`).
#
#   ./bundle.sh                 # uses target/aarch64-apple-darwin/release/Zipper
#   ./bundle.sh path/to/Zipper  # explicit binary
#   OPEN=1 ./bundle.sh          # macOS: open the app afterwards
set -euo pipefail

cd "$(dirname "$0")"
APP_NAME=Zipper
BIN="${1:-target/aarch64-apple-darwin/release/$APP_NAME}"
OUT_DIR="${OUT_DIR:-dist}"
APP="$OUT_DIR/$APP_NAME.app"

if [[ ! -x "$BIN" ]]; then
  echo "binary not found: $BIN (run cargo build first)" >&2
  exit 1
fi

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/$APP_NAME"
chmod 755 "$APP/Contents/MacOS/$APP_NAME"
cp resources/Info.plist "$APP/Contents/Info.plist"
printf 'APPL????' > "$APP/Contents/PkgInfo"
if [[ -f resources/AppIcon.icns ]]; then
  cp resources/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
else
  echo "note: resources/AppIcon.icns missing, app will use the generic icon" >&2
fi

# Ad-hoc signature: required on Apple Silicon for the binary to launch at all.
if command -v codesign >/dev/null 2>&1; then
  codesign --force --deep -s - "$APP"
  echo "signed with codesign (ad-hoc)"
elif command -v rcodesign >/dev/null 2>&1; then
  rcodesign sign "$APP" >/dev/null
  echo "signed with rcodesign (ad-hoc)"
else
  echo "warning: neither codesign nor rcodesign found; the app is unsigned and will not launch on Apple Silicon" >&2
fi

echo "$APP"

if [[ "${OPEN:-0}" == "1" ]] && command -v open >/dev/null 2>&1; then
  open "$APP"
fi
