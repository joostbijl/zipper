# Zipper

A 7-Zip-style archive manager for macOS, written in Rust with a native AppKit
interface. Built for the cases where the terminal is the wrong tool: double-click
a zip attachment, look inside, open one file, or extract exactly the files you
want to exactly the folder you want.

Apple Silicon, macOS 14 or newer. No Electron, no Swift, no Xcode project:
Rust all the way down, AppKit through [`objc2`](https://github.com/madsmtm/objc2).

## What it does

- **Open** zip, 7z, tar, tar.gz/bz2/xz/zst, gz/xz/bz2/zst, and (through a
  read-only `hdiutil` mount) iso and dmg — from Finder, Mail, Outlook, the
  Dock, drag-and-drop, or ⌘O.
- **Browse** the contents as a tree with sortable Name / Size / Packed /
  Modified columns. Double-click a file to open it in its default app;
  space bar for Quick Look.
- **Extract** the selection or everything to a folder you choose (⌘E),
  with Skip / Replace / Keep Both conflict handling, a progress sheet with
  Cancel, and password prompts for encrypted zip and 7z. Attachments opened
  from mail clients default to `~/Downloads` instead of their hidden temp
  folder.
- **Drag out**: drop files or folders from the window onto Finder, the
  Desktop or another app.
- **Create**: ⌘N, drop files in, ⌘S. Format (zip / 7z), compression level,
  AES-256 password, 7z volume splitting.
- **Edit** existing zip and 7z archives: add files (drag in or ⇧⌘A),
  delete (⌘⌫), rename (⇧⌘R). Edits rewrite the archive to a temporary file
  and replace the original only when everything succeeded; zip entries are
  copied without recompression.
- **Finder right-click**: "Add to Archive with Zipper" and "Extract with
  Zipper".
- **Safety**: entries with `..` or absolute paths are never listed, symlinks
  that point outside the destination are skipped, nothing is overwritten
  without asking.
- Settings (⌘,): default extract location, default format, reveal in Finder,
  move archive to Trash after extracting, and a button to make Zipper the
  default app for zip / 7z / tar.

Out of scope for now: rar, writing tar, iso or dmg, localisation, Intel Macs.

## Install

Requires Rust ([rustup](https://rustup.rs)) and the Xcode Command Line Tools
(`xcode-select --install`).

```sh
git clone https://github.com/joostbijl/zipper.git
cd zipper
scripts/install.sh
```

That builds a release binary, bundles `Zipper.app`, ad-hoc signs it, copies it
to `/Applications`, registers it with LaunchServices and installs two Finder
Quick Actions. The ad-hoc signature is fine for your own machine; Gatekeeper
may ask once (right-click → Open).

Then, one-time, because macOS keeps new third-party menu items off by default:

- **Quick Actions** ("Add to Zipper Archive", "Open in Zipper"): right-click
  any file → Quick Actions → Customize… → tick them.
- **Services** ("Add to Archive with Zipper" goes straight to the Save panel,
  "Extract with Zipper" extracts everything): System Settings → Keyboard →
  Keyboard Shortcuts… → Services → Files and Folders → tick them.
- **Default app**: Zipper → Settings… → "Open zip, 7z and tar files with
  Zipper…" (macOS asks once per type), or right-click a `.zip` → Get Info →
  Open With → Change All.

Updating is the same `git pull && scripts/install.sh`.

### Disk image

```sh
./bundle.sh && scripts/make-dmg.sh      # → dist/Zipper-<version>.dmg
```

Pushing a tag (`git tag v0.1.0 && git push --tags`) makes the GitHub macOS
runner build the DMG and attach it to a release. Signing is ad-hoc unless the
repository has a Developer ID certificate and notarization credentials as
secrets (see `.github/workflows/release.yml`); ad-hoc images open on any Mac
after one right-click → Open. The Linux builder cannot make a DMG: it has no
`hdiutil`, and the Linux HFS+ tools are not something to ship an installer
with. It produces `dist/Zipper.app.zip` instead.

### Build without installing

```sh
cargo build --release -p zipper-app --target aarch64-apple-darwin
./bundle.sh && open dist/Zipper.app
```

Start the app with `open` or from Finder. Running the binary directly from a
terminal is useful to see panics, but since macOS 14 an app launched that way
may not take focus and its window appears behind the terminal.

## Cross-compiling from Linux

The app can be built on an x86_64 Linux box with `cargo-zigbuild`. The one
thing Linux cannot provide is the macOS SDK, which Apple licenses only for
Apple hardware; copy it from a Mac as `scripts/builder-setup.sh` explains.

```sh
scripts/builder-setup.sh        # rustup, zig, cargo-zigbuild, rcodesign (once)
export SDKROOT=~/sdk/MacOSX.sdk
scripts/cross-build.sh          # → dist/Zipper.app and dist/Zipper.app.zip
```

Without the SDK, `scripts/cross-build.sh --check` still type-checks the AppKit
code on Linux (`cargo check -p zipper-app --features linux-check`): it tells
the `objc2` crates to assume the GNUstep runtime, which is enough for the
compiler to verify every method name and signature. A check only, never a
build — but it catches most of what would otherwise fail on the Mac.

## Layout

```
crates/archive-core   engine: detect / list / extract / test / create / modify
                      for every format; pure Rust, builds and tests on Linux
crates/zipper-app     AppKit UI via objc2: windows, outline view, sheets,
                      drag & drop, file promises, Quick Look, settings
resources/            Info.plist (document types, UTIs, services), app icon
bundle.sh             binary → Zipper.app (+ ad-hoc signature)
scripts/              install, Quick Actions, Linux builder setup and cross build
SPEC.md               the original brief and the phased plan
TESTING.md            manual acceptance checklist
```

The UI never touches a format crate directly; everything goes through
`archive_core::Archive`. Archive work runs on background threads and reports
back to AppKit on the main queue; a worker that needs an answer from the user
(overwrite?) blocks on a channel while the main thread shows a sheet.

## Tests

```sh
cargo test -p archive-core
```

39 tests generate their own fixtures — zip (deflate, stored, AES, legacy
ZipCrypto via Info-ZIP when installed), 7z (plain, encrypted headers, solid
blocks), every tar variant, single-stream files, zip-slip attempts, truncated
archives, edits with and without passwords, symlinks, volume splitting, and a
mount-backed image driven by a plain directory.

CI runs the engine tests and the Linux type-check on every push, plus a real
build on a macOS runner.

## Dependencies

[`zip`](https://crates.io/crates/zip), [`sevenz-rust2`](https://crates.io/crates/sevenz-rust2),
[`tar`](https://crates.io/crates/tar), [`flate2`](https://crates.io/crates/flate2),
[`bzip2`](https://crates.io/crates/bzip2), [`liblzma`](https://crates.io/crates/liblzma),
[`zstd`](https://crates.io/crates/zstd) for the formats;
[`objc2`](https://crates.io/crates/objc2), [`objc2-app-kit`](https://crates.io/crates/objc2-app-kit),
[`objc2-foundation`](https://crates.io/crates/objc2-foundation), [`block2`](https://crates.io/crates/block2),
[`dispatch2`](https://crates.io/crates/dispatch2) for AppKit. Disk images use the
system's `hdiutil`.

## How it was made

The spec was written first, then the code was produced with Claude working
from a Linux machine that could run the engine tests and type-check the UI
but never launch it; the Mac side of the loop was a human reporting crash logs
and screenshots. Two real bugs came out of that loop: a re-entrant `RefCell`
borrow when closing a window (a Rust panic that surfaced as a HIServices
abort), and the macOS 14+ activation rule for Terminal-launched apps.
Everything else worked on first launch, which says more about `objc2` than
about anyone else.

## License

MIT — see [LICENSE](LICENSE).
