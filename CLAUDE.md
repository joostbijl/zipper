# Zipper — working notes for Claude Code

Read `SPEC.md` first; it is the brief and the source of truth for scope,
phases and the working agreement. This file only holds the practical bits.

## Layout

- `crates/archive-core` — pure Rust archive engine. Builds and tests on Linux
  and macOS. **All format work goes here, never in the UI crate.**
- `crates/zipper-app` — AppKit UI via `objc2`. Only builds for
  `aarch64-apple-darwin`.
- `resources/Info.plist` — bundle metadata and file associations.
- `bundle.sh` — binary → `dist/Zipper.app` (+ ad-hoc signature).
- `scripts/builder-setup.sh`, `scripts/cross-build.sh` — Linux x86_64 cross
  build (needs the macOS SDK copied from a Mac).
- `TESTING.md` — manual acceptance checklist per phase.

## Commands

```sh
cargo test -p archive-core                       # engine tests (any OS)
cargo clippy --workspace --all-targets           # on macOS; on Linux add -p archive-core
cargo build --release -p zipper-app --target aarch64-apple-darwin   # on the Mac
./bundle.sh && open dist/Zipper.app              # on the Mac
scripts/cross-build.sh                           # on the Debian builder
```

## Rules of thumb

- The UI never imports `zip`, `sevenz_rust2`, `tar` etc. directly.
- Every archive operation runs off the main thread; results come back via
  `dispatch_async` to the main queue. AppKit is main-thread only.
- Path safety lives in `archive_core::safe_path`. Do not add a second guard.
- Keep `main` buildable. Commit per logical step.
- When an `objc2` API is unclear, read the crate source under
  `~/.cargo/registry/src/*/objc2-app-kit-*/` rather than guessing selectors.
- Do not add Swift or Objective-C source files.
