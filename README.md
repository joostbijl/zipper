# Zipper

A 7-Zip-style archive manager for macOS (Apple Silicon), written in Rust with
a native AppKit front end. Open a zip from an e-mail, double-click a file
inside it, or extract exactly the files you want to exactly the folder you
want. See [SPEC.md](SPEC.md) for the full brief and [TESTING.md](TESTING.md)
for the acceptance checklist.

Status: **Phases 1–2 working on macOS; Phase 3 written, awaiting first run** (Quick Look, Settings).

## Formats

Read: 7z, zip, tar, tar.gz, tar.bz2, tar.xz, tar.zst, gz, xz, bz2, zst.
Write: zip (AES-256), 7z (AES-256, encrypted headers).

## Building on the Mac

Requires Xcode Command Line Tools (`xcode-select --install`) and Rust (`rustup`).

```sh
rustup target add aarch64-apple-darwin
cargo build --release -p zipper-app --target aarch64-apple-darwin
./bundle.sh            # → dist/Zipper.app, ad-hoc signed
open dist/Zipper.app
```

Make it the default for zip files: right-click a `.zip` → Get Info →
Open With → Zipper → Change All.

## Cross-compiling on a Linux x86_64 builder

No Xcode, but the macOS SDK must be copied from a Mac (Apple's licence allows
it only on Apple hardware; the SDK is just headers and `.tbd` link stubs).

```sh
scripts/builder-setup.sh      # rustup, zig, cargo-zigbuild, rcodesign — one time
# copy the SDK from the Mac as the script explains, then:
export SDKROOT=~/sdk/MacOSX.sdk
scripts/cross-build.sh        # → dist/Zipper.app and dist/Zipper.app.zip
```

Copy `dist/Zipper.app.zip` to the Mac, unzip, run. The signature is ad-hoc,
which is fine for your own machine; Gatekeeper may ask once (right-click →
Open).

`scripts/cross-build.sh --check` (or `cargo check -p zipper-app --features linux-check`)
type-checks the AppKit crate on Linux without the SDK or the Apple target: it tells the
`objc2` crates to assume the GNUstep runtime, which is enough for the compiler to verify
every method name and signature. It is a check only, never a build.

## Tests

```sh
cargo test -p archive-core
```

The engine tests generate their own fixtures (zip, 7z with and without
passwords, solid 7z, every tar variant, zip-slip attempts, truncated files)
and run on Linux and macOS.

## Layout

```
crates/archive-core   engine: list / extract / test / create, all formats
crates/zipper-app     AppKit UI (objc2)
resources/            Info.plist, icon
bundle.sh             binary → .app
scripts/              builder setup and cross build
```
