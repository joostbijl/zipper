#!/usr/bin/env bash
# Cross-compile Zipper for Apple Silicon on a Linux x86_64 builder and
# package it as dist/Zipper.app (+ dist/Zipper.app.zip for copying to the Mac).
#
# Prerequisites: scripts/builder-setup.sh, and the macOS SDK at $SDKROOT.
#
#   scripts/cross-build.sh            # release build
#   PROFILE=dev scripts/cross-build.sh
#   scripts/cross-build.sh --check    # type-check only (no SDK needed)
set -euo pipefail

cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"
TARGET=aarch64-apple-darwin
PROFILE="${PROFILE:-release}"

# Always run the host-side tests first; they cover the archive engine.
cargo test -p archive-core --quiet

if [[ "${1:-}" == "--check" ]]; then
  # Type-check the AppKit crate on the Linux host. No SDK and no Apple
  # target needed: the objc2 crates are told to assume the GNUstep runtime,
  # which is enough for `cargo check` (never for a build).
  cargo check -p zipper-app --features linux-check
  cargo clippy -p zipper-app --features linux-check
  exit 0
fi

SDKROOT="${SDKROOT:-$HOME/sdk/MacOSX.sdk}"
if [[ ! -d "$SDKROOT" ]]; then
  echo "macOS SDK not found at $SDKROOT — see scripts/builder-setup.sh" >&2
  exit 1
fi
export SDKROOT

# cargo-zigbuild passes -isysroot/-F to zig's linker so the Apple frameworks
# (AppKit, Foundation, ...) resolve against the SDK's .tbd stubs.
if [[ "$PROFILE" == "release" ]]; then
  cargo zigbuild --release --target "$TARGET" -p zipper-app
  BIN="target/$TARGET/release/Zipper"
else
  cargo zigbuild --target "$TARGET" -p zipper-app
  BIN="target/$TARGET/debug/Zipper"
fi

file "$BIN" || true
./bundle.sh "$BIN"

# Zip preserves the bundle layout and the signature when unpacked by Finder/ditto.
( cd dist && rm -f Zipper.app.zip && zip -qry Zipper.app.zip Zipper.app )
echo "dist/Zipper.app.zip ready — copy to the Mac, unzip, run."
