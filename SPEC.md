# Archiver — a native macOS archive manager in Rust

Working name: **Zipper** (rename freely; the name appears in `Cargo.toml`, `Info.plist`, `bundle.sh`).

This file is the brief for the Claude Code session that builds it. Read it fully before touching code. Where it says "verify", the API or crate version may have moved since this brief was written (Oct 2026) — check docs.rs / crates.io before relying on it.

## 1. Purpose

A 7-Zip-style GUI for macOS (Apple Silicon only), written in Rust with a native AppKit front end. The owner uses `7zz` happily in the terminal; this app exists for the cases where the terminal is the wrong tool:

- Double-click a `.zip` / `.7z` attachment in Outlook or Mail and see what's inside.
- Double-click a file inside the archive and have it open in its default app.
- Extract one, several, or all entries to a folder the user picks — reliably, every time.
- Drag entries out of the window into Finder.
- Create and edit archives (add, delete, rename entries) without a terminal.

Existing macOS archivers failed the owner mostly on the "extract a chosen subset to a chosen folder" case. That workflow must be rock solid before anything else is polished.

## 2. Hard constraints

- **Language:** Rust only. No Swift, no Objective-C source files. AppKit is reached through `objc2` and the `objc2-*` framework crates.
- **Target:** `aarch64-apple-darwin`, macOS 14 (Sonoma) or newer. No Intel build, no universal binary.
- **Build:** `cargo build --release` on the Mac must produce the binary; `./bundle.sh` must turn it into `Zipper.app`. Ad-hoc signing (`codesign -s -`) is fine. No Xcode project, no `.xcodeproj`, no Apple developer account required. Xcode Command Line Tools are acceptable as a prerequisite.
- **Units and language:** UI in English. Sizes in metric/SI style with binary suffixes as macOS does (KB, MB, GB per Finder's convention is acceptable; be consistent).
- **No telemetry, no network access, no auto-update.**

## 3. Architecture

Cargo workspace, two crates:

```
zipper/
├── Cargo.toml              # workspace
├── SPEC.md                 # this file
├── CLAUDE.md               # short pointer to SPEC.md + build/test commands
├── bundle.sh               # binary → Zipper.app (Info.plist, icon, codesign -s -)
├── resources/
│   ├── Info.plist          # CFBundleDocumentTypes, UTIs, see §8
│   └── AppIcon.icns
├── crates/
│   ├── archive-core/       # pure Rust, no AppKit, fully unit-tested, builds on Linux too
│   └── zipper-app/         # AppKit UI via objc2; depends on archive-core
```

### 3.1 `archive-core`

A format-agnostic API. Everything the UI needs goes through this; the UI never touches a format crate directly.

```rust
pub struct Entry {
    pub path: PathBuf,          // path inside the archive, '/' separated, normalised
    pub is_dir: bool,
    pub size: u64,
    pub packed_size: Option<u64>,
    pub modified: Option<SystemTime>,
    pub encrypted: bool,
}

pub enum Format { SevenZ, Zip, Tar, TarGz, TarBz2, TarXz, TarZst, Gz, Xz, Bz2, Zst }

pub trait Archive {
    fn format(&self) -> Format;
    fn entries(&self) -> &[Entry];
    fn extract(&self, selection: &[&Path], dest: &Path, opts: &ExtractOptions, progress: &dyn Fn(Progress)) -> Result<(), Error>;
    fn test(&self) -> Result<(), Error>;                   // integrity check
}

pub struct ExtractOptions {
    pub overwrite: Overwrite,       // Ask(callback) | Skip | Replace
    pub password: Option<String>,
    pub into_subfolder: bool,       // dest/<archive-stem>/ vs dest/
    pub preserve_mtime: bool,       // default true
}

pub fn open(path: &Path, password: Option<&str>) -> Result<Box<dyn Archive>, Error>;
pub fn detect_format(path: &Path) -> Result<Format, Error>;  // magic bytes first, extension as fallback

// Writing (zip and 7z only; tar.* writing is a later phase)
pub struct WriteOptions { pub format: Format, pub level: u8 /* 0-9 */, pub password: Option<String>, pub solid: bool /* 7z only */ }
pub fn create(dest: &Path, sources: &[PathBuf], opts: &WriteOptions, progress: &dyn Fn(Progress)) -> Result<(), Error>;

// Editing: implemented as rewrite-to-temp-then-atomic-rename. Never modify in place.
pub fn add(archive: &Path, sources: &[PathBuf], under: Option<&Path>, opts: &WriteOptions, progress: &dyn Fn(Progress)) -> Result<(), Error>;
pub fn remove(archive: &Path, entries: &[&Path], progress: &dyn Fn(Progress)) -> Result<(), Error>;
pub fn rename(archive: &Path, from: &Path, to: &Path) -> Result<(), Error>;
```

Rules for `archive-core`:

- **Path safety is non-negotiable.** Reject or sanitise entries containing `..`, absolute paths, or symlinks pointing outside `dest`. Write a test for each ("zip slip").
- Streaming extraction; never load a whole archive into memory. Large archives (multi-GB) must work.
- Run all archive I/O off the main thread; the `progress` callback is how the UI learns about it. Progress reports bytes done / total and the current entry path.
- Errors are a proper enum (`thiserror`): `Io`, `UnsupportedFormat`, `WrongPassword`, `PasswordRequired`, `Corrupt(String)`, `UnsafePath(PathBuf)`, `Cancelled`.
- Cancellation: `extract`/`create` take an `AtomicBool` (or similar) and stop promptly.
- Unit tests with fixture archives generated in `build.rs` or checked in under `crates/archive-core/tests/fixtures/`. Cover: nested folders, unicode filenames, empty dirs, encrypted zip (AES and legacy ZipCrypto), encrypted 7z, 7z with solid blocks, tar with symlinks, a zip-slip attempt, a truncated file.

Suggested crates (verify current versions and feature flags on crates.io):

| Need | Crate | Notes |
|---|---|---|
| 7z read/write | `sevenz-rust2` | enable `aes256` for passwords; `compress` for writing |
| zip read/write | `zip` | enable `aes-crypto`, `deflate`, `bzip2`, `zstd`, `lzma` |
| tar | `tar` | |
| gzip | `flate2` | |
| bzip2 | `bzip2` | |
| xz | `liblzma` (or `xz2`) | |
| zstd | `zstd` | |
| temp dirs | `tempfile` | |
| errors | `thiserror` | |
| format sniffing | `infer` or hand-rolled magic bytes | hand-rolled is fine and avoids a dep |

Do not add `rar`, `iso` or `dmg` support in the first three phases (see §9).

### 3.2 `zipper-app`

AppKit via `objc2`, `objc2-foundation`, `objc2-app-kit`, `block2`. Verify versions; the `objc2` ecosystem changes its `define_class!`/`declare_class!` macro syntax between releases, so read the current `objc2` docs and the `objc2-app-kit` examples before writing any class.

Structure:

- `main.rs` — `NSApplication` setup, delegate, `applicationShouldOpenUntitledFile` → show the empty window with "Open…" and a drop target.
- `app_delegate.rs` — `application:openFiles:` (this is what fires for double-click/"Open with"), menu bar, Dock behaviour.
- `document.rs` — one open archive = one `NSWindow`. Multiple windows allowed. Not `NSDocument`-based (it fights you when editing archives); plain window controller.
- `outline.rs` — `NSOutlineView` data source + delegate for the entry tree.
- `extract.rs` — "Extract to…" flow, overwrite prompts, progress sheet with cancel.
- `write.rs` — create/add/delete/rename flows.
- `dragdrop.rs` — drag *in* (files → add) and drag *out* (`NSFilePromiseProvider` → extract on drop).
- `open_entry.rs` — double-click-to-open: extract to a per-archive temp dir, `NSWorkspace.openURL`, clean temp dir when the window closes.
- `errors.rs` — map `archive_core::Error` to `NSAlert`s.

Threading: `archive-core` calls run on a `std::thread` (or a small pool); results are marshalled back with `dispatch_async` to the main queue (via `dispatch2` crate, or `NSOperationQueue.mainQueue`). Never call AppKit off the main thread.

## 4. Features — in scope, by phase

Numbering matches the list the owner agreed to. Build strictly in phase order; each phase ends with a working, launchable `Zipper.app`.

### Phase 1 — open and extract (the core use case)

1. Open archives by double-click / "Open with" / drag onto Dock icon / File → Open / ⌘O. Also drag an archive file onto the empty window.
2. Outline view: columns Name, Size, Packed, Modified. Sortable by clicking headers. Folder rows expandable (tree, not flat list). Type-to-select. ⌘A selects all.
3. Breadcrumb path bar above the outline showing the current folder; the outline itself is a tree so this is mostly informational, but "Enter folder" on double-click of a folder row is acceptable as an alternative to expand-in-place if the tree becomes awkward. Pick one and keep it.
4. Double-click a file row → extract that entry to `$TMPDIR/Zipper/<archive-name>-<hash>/<entry-path>` and open with `NSWorkspace`. Temp dir removed on window close and on app quit. If the same entry is double-clicked twice, reuse the already extracted copy.
7. **Extract selected.** Toolbar button + ⌘E + context menu. Zero selection means "all". Folder picker is `NSOpenPanel` with `canChooseDirectories = true`, `canChooseFiles = false`, `canCreateDirectories = true`, starting in the archive's own folder. Remember the last used folder per session. Selected folders extract recursively.
8. **Extract all** with a checkbox in the panel's accessory view: "Into a subfolder named after the archive" (default on when the archive has >1 top-level entry, off otherwise — this is what Finder's built-in does and users expect it).
9. Overwrite handling: on first conflict show an `NSAlert` sheet: Skip / Replace / Keep both (append " 2") with "Apply to all" checkbox. Default button: Skip.
10. Password: if `open()` returns `PasswordRequired` or `WrongPassword`, prompt with a secure text field sheet; retry. Offer "Remember for this window" only; never persist passwords.
15. Read formats: 7z, zip, tar, tar.gz/.tgz, tar.bz2, tar.xz, tar.zst, and single-file gz/xz/bz2/zst (shown as an archive with one entry).
17. Test archive integrity: Archive → Test (⌘T). Reports OK or the first error.

Progress: any operation over ~300 ms shows a sheet with a determinate progress bar, current file name, and Cancel.

### Phase 2 — move things out and in

5. **Drag out:** drag one or more rows onto Finder, the Desktop, or another app. Use `NSFilePromiseProvider`; extraction happens when the drop lands, into the drop location. Folders dragged out extract recursively.
11. **Create a new archive:** File → New Archive (⌘N) opens an empty editable window; drag files in or use Archive → Add Files… (⌘⇧A, `NSOpenPanel` allowing files and folders). On first Save (⌘S) show a save panel with format choice. Also: dropping files onto the Dock icon or onto the empty window creates a new archive from them (Finder-style: proposes `<first-item>.zip` next to the source).
12. **Add to existing archive:** same drag-in and Add Files… flow on an already-open archive. Dropping onto a folder row adds under that folder.
13. **Delete** (⌫ with confirmation sheet, or Edit → Delete) and **rename** (click-to-edit the Name cell, or Return) entries. Both rewrite the archive via temp file + atomic rename; show progress for large archives.
14. Save panel options (accessory view): Format (zip default, 7z), Compression level (slider or popup: Store / Fast / Normal / Best), Password (secure field; for zip use AES-256, for 7z encrypt headers too), Split into volumes (size popup: off / 100 MB / 1 GB / 4 GB / custom). Splitting is a 7z-only feature in the first implementation; grey it out for zip.
16. Write formats: zip (required), 7z (required but may be simpler: no solid-block tuning, no filters beyond LZMA2).

Editing an archive the app does not know how to write (any tar.*) is read-only: grey out Add/Delete/Rename and show a small status-bar note "Read-only: tar archives cannot be edited yet".

### Phase 3 — polish

6. Quick Look: spacebar on a selected file row opens `QLPreviewPanel` with the file extracted to temp (reuse the Phase 1 temp mechanism). Crate: `objc2-quick-look-ui`, verify it exists in current form.
20. Preferences (⌘,): default extract location (ask / archive's folder / fixed folder), default new-archive format, "Open folder in Finder after extracting" toggle, "Delete archive after extracting" toggle (off by default). Stored with `NSUserDefaults`.
- Keyboard: everything reachable without the mouse. Menu items with standard shortcuts.
- Status bar at the bottom: "123 items, 45.6 MB (12.3 MB packed)" and selection count.
- Sort stability, column width persistence, window size/position persistence (`setFrameAutosaveName`).
- Dark mode is automatic with AppKit; verify custom-drawn parts (none expected) follow it.

### Phase 4 — stretch, only if everything above is solid

- rar (extract only; `unrar` crate links the proprietary lib — check licensing), iso, dmg reading.
- tar.* writing.
- 18: Finder context-menu items ("Extract here", "Compress"). **Note:** proper Finder extensions require an app extension bundle, which in practice means Xcode and Swift — outside the Rust-only constraint. Offer instead: Services menu entries via `NSServices` in `Info.plist`, which work from Finder's right-click → Services submenu with no extension. Implement that; do not attempt a Finder Sync extension.
- 19: CLI entry point (`zipper-cli`) over `archive-core`. Low priority; the owner has `7zz`.

## 5. UX details that matter

- Launch-to-window for a 50 MB zip must be under ~500 ms. Listing is cheap for zip/7z (central directory); for tar.* the listing requires a full decompress pass — show the window immediately with a spinner and fill the outline when done.
- When opened from Outlook/Mail, the archive lives in a sandboxed temp folder the user cannot easily find. The default "Extract to…" starting folder must therefore be `~/Downloads`, not the archive's folder, when the archive's path is under `~/Library` or `$TMPDIR`. Elsewhere, start in the archive's folder.
- After extraction: reveal the destination in Finder (`NSWorkspace.activateFileViewerSelecting`) if the preference is on (default on).
- Never silently overwrite. Never extract outside the chosen folder.
- Modal sheets, not floating alert windows.
- The empty window (no archive) shows a drop zone with "Drop an archive here to open it, or files to create one".

## 6. App bundle and file associations

`resources/Info.plist` must declare:

- `CFBundleDocumentTypes` with `LSHandlerRank = Alternate` for: `public.zip-archive`, `org.7-zip.7z-archive`, `public.tar-archive`, `org.gnu.gnu-zip-archive`, `org.gnu.gnu-zip-tar-archive`, `public.bzip2-archive`, `org.tukaani.xz-archive`, `org.tukaani.tar-xz-archive`, `public.zstd-archive` (verify UTI identifiers; define `UTImportedTypeDeclarations` for any that macOS does not know, e.g. `.zst`, `.7z` on older systems).
- `NSSupportsAutomaticTermination = false`, `NSSupportsSuddenTermination = false` (temp-dir cleanup needs a real quit).
- `LSMinimumSystemVersion = 14.0`.
- `NSHighResolutionCapable = true`.

`bundle.sh`:
1. `cargo build --release --target aarch64-apple-darwin`
2. Assemble `target/Zipper.app/Contents/{MacOS,Resources}` with the binary, `Info.plist`, `AppIcon.icns`, `PkgInfo` (`APPL????`).
3. `codesign --force --deep -s - target/Zipper.app`
4. Print the path. Optionally `open target/Zipper.app`.

Icon: generate a simple one with `iconutil` from a 1024 px PNG; a placeholder is fine. No third-party logos.

To make it the default opener for zip: System Settings or right-click → Get Info → Open With → Change All. Document this in the README; do not attempt to set it programmatically.

## 7. Testing

- `archive-core`: `cargo test` must pass on macOS and Linux. CI on the owner's Debian x86 builder is optional; the crate has no platform dependencies.
- `zipper-app`: no automated UI tests expected. A `scripts/smoke.sh` that builds, bundles, creates a test zip with `zip`, opens it with `open -a target/Zipper.app`, and prints a manual checklist is enough.
- Manual acceptance checklist (keep in `TESTING.md`, tick per phase):
  - open zip from Finder double-click; open 7z via Open With; drag tar.xz onto Dock
  - double-click a `.pdf` inside a zip → opens in Preview
  - select 3 files + 1 folder, ⌘E, choose `~/Desktop/x` → exactly those appear, structure preserved
  - extract all with and without subfolder
  - overwrite dialog with Skip / Replace / Keep both / Apply to all
  - encrypted zip and 7z with wrong then right password
  - drag two files to Desktop
  - new archive from three dropped files, save as 7z with password, reopen it
  - delete one entry, rename one entry, reopen — changes present, archive still valid in `7zz t`
  - cancel a long extraction halfway — no partial files left behind beyond what was fully written

## 8. Working agreement for the Claude Code session

- Commit per logical step with clear messages; the owner prefers not to manage git himself, so keep `main` always buildable and do the branching.
- Before the first commit of each phase, write a short plan in the PR/commit description listing the files touched.
- When an `objc2` API is unclear, read the crate source or docs rather than guessing a selector name; wrong selectors fail at runtime, not compile time.
- When something in this spec turns out impossible or wrong in practice, say so and propose the alternative; do not silently drop a feature.
- Prefer boring, well-maintained crates over clever ones. Keep the dependency list short enough to audit.
- Ask the owner before: adding any crate with a non-permissive licence, adding a C dependency beyond the compression libraries, changing the app name, or changing a keyboard shortcut from the macOS standard.

## 9. Explicitly out of scope (for now)

rar/iso/dmg, tar writing, Finder Sync extension, CLI, auto-update, iCloud/sandboxing for App Store, Intel builds, localisation.
