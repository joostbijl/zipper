#!/usr/bin/env bash
# Package dist/Zipper.app as a distributable disk image: dist/Zipper-<version>.dmg
# with the app and an /Applications shortcut, so it installs like any other app.
#
# Runs on macOS (hdiutil). Signing:
#   - default: ad-hoc (fine for your own Macs; others get a Gatekeeper warning)
#   - SIGN_IDENTITY="Developer ID Application: Name (TEAMID)"  → real signature
#   - NOTARY_PROFILE=<keychain profile from `xcrun notarytool store-credentials`>
#     → also notarize and staple, so it opens cleanly on any Mac
#
#   ./bundle.sh && scripts/make-dmg.sh
#   SIGN_IDENTITY="Developer ID Application: …" NOTARY_PROFILE=zipper scripts/make-dmg.sh
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ "$(uname)" != "Darwin" ]]; then
  echo "make-dmg.sh needs hdiutil, so it runs on macOS (or the macOS job in CI)." >&2
  echo "On the Linux builder, scripts/cross-build.sh produces dist/Zipper.app.zip instead." >&2
  exit 1
fi

APP=dist/Zipper.app
[[ -d "$APP" ]] || { echo "missing $APP — run ./bundle.sh first" >&2; exit 1; }
VERSION=$(/usr/libexec/PlistBuddy -c 'Print CFBundleShortVersionString' "$APP/Contents/Info.plist")
DMG="dist/Zipper-$VERSION.dmg"
IDENTITY="${SIGN_IDENTITY:--}"

# 1. Sign the app (again, with the real identity if given). Hardened runtime
#    is required for notarization and harmless otherwise.
codesign --force --deep --options runtime --timestamp -s "$IDENTITY" "$APP" 2>/dev/null \
  || codesign --force --deep -s "$IDENTITY" "$APP"
codesign --verify --deep --strict "$APP"

# 2. Stage: app + Applications symlink.
STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"

# 3. Build a compressed read-only image.
rm -f "$DMG"
hdiutil create -quiet -volname "Zipper" -srcfolder "$STAGE" -ov -format UDZO -fs HFS+ "$DMG"

# 4. Sign the image too (Gatekeeper checks it).
codesign --force --timestamp -s "$IDENTITY" "$DMG" 2>/dev/null || codesign --force -s "$IDENTITY" "$DMG"

# 5. Notarize + staple when credentials are available.
if [[ -n "${NOTARY_PROFILE:-}" ]]; then
  xcrun notarytool submit "$DMG" --keychain-profile "$NOTARY_PROFILE" --wait
  xcrun stapler staple "$DMG"
fi

echo "$DMG"
if [[ "$IDENTITY" == "-" ]]; then
  echo "ad-hoc signed: fine on this Mac; other Macs will show a Gatekeeper warning (right-click → Open)."
fi
