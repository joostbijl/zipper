#!/usr/bin/env bash
# One-time setup of a Debian/Ubuntu x86_64 machine as a cross-compile builder
# for Zipper (Rust → aarch64-apple-darwin, no Xcode).
#
# What it installs (all in $HOME, no root needed except for apt):
#   - rustup + stable toolchain + the aarch64-apple-darwin std library
#   - zig (used as the C compiler/linker for the Apple target)
#   - cargo-zigbuild (drives cargo with zig)
#   - rcodesign (ad-hoc code signing on Linux)
#
# What it cannot install: the macOS SDK. Apple's licence only allows it on
# Apple hardware, so copy it from the Mac yourself (see README §Cross-compiling).
# The build expects it at $SDKROOT, default ~/sdk/MacOSX.sdk.
set -euo pipefail

ZIG_VERSION="${ZIG_VERSION:-0.14.1}"
SDK_DIR="${SDKROOT:-$HOME/sdk/MacOSX.sdk}"

echo "== apt packages"
sudo apt-get update -qq
sudo apt-get install -y -qq curl xz-utils git build-essential pkg-config

echo "== rustup"
if ! command -v rustup >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
fi
# shellcheck disable=SC1091
source "$HOME/.cargo/env"
rustup toolchain install stable --profile minimal
rustup default stable
rustup component add clippy rustfmt
rustup target add aarch64-apple-darwin

echo "== zig $ZIG_VERSION"
if ! command -v zig >/dev/null 2>&1 || [[ "$(zig version)" != "$ZIG_VERSION" ]]; then
  mkdir -p "$HOME/.local/opt" "$HOME/.local/bin"
  TARBALL="zig-x86_64-linux-$ZIG_VERSION.tar.xz"
  URL="https://ziglang.org/download/$ZIG_VERSION/$TARBALL"
  # Older releases used the other name order; try both.
  if ! curl -fsSL "$URL" -o "/tmp/$TARBALL"; then
    TARBALL="zig-linux-x86_64-$ZIG_VERSION.tar.xz"
    URL="https://ziglang.org/download/$ZIG_VERSION/$TARBALL"
    curl -fsSL "$URL" -o "/tmp/$TARBALL"
  fi
  rm -rf "$HOME/.local/opt/zig"
  mkdir -p "$HOME/.local/opt/zig"
  tar -xJf "/tmp/$TARBALL" -C "$HOME/.local/opt/zig" --strip-components=1
  ln -sf "$HOME/.local/opt/zig/zig" "$HOME/.local/bin/zig"
  rm -f "/tmp/$TARBALL"
fi
export PATH="$HOME/.local/bin:$PATH"
zig version

echo "== cargo-zigbuild + rcodesign"
cargo install --locked cargo-zigbuild
cargo install --locked apple-codesign

echo
if [[ -d "$SDK_DIR" ]]; then
  echo "macOS SDK found at $SDK_DIR"
else
  cat <<EOF
macOS SDK NOT found at $SDK_DIR.

On the Mac, run:
  SDK=\$(xcrun --show-sdk-path)
  tar -C "\$(dirname "\$SDK")" -cJf ~/MacOSX.sdk.tar.xz "\$(basename "\$SDK")"
  scp ~/MacOSX.sdk.tar.xz $(whoami)@$(hostname):~/

Then here:
  mkdir -p ~/sdk && tar -C ~/sdk -xJf ~/MacOSX.sdk.tar.xz
  mv ~/sdk/MacOSX*.sdk ~/sdk/MacOSX.sdk   # if it has a version suffix
EOF
fi

cat <<EOF

Add to ~/.bashrc (or ~/.profile):
  export PATH="\$HOME/.cargo/bin:\$HOME/.local/bin:\$PATH"
  export SDKROOT="$SDK_DIR"

Then build with:  scripts/cross-build.sh
EOF
