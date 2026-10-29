# Manual acceptance checklist

The archive engine is covered by `cargo test -p archive-core`. The UI is
checked by hand on the Mac. Tick per phase; add a line when something new
breaks and gets fixed.

## Phase 1 — open and extract

- [ ] Finder: double-click a `.zip` → window with tree, correct sizes/dates
- [ ] Finder: right-click a `.7z` → Open With → Zipper
- [ ] Drag a `.tar.xz` onto the Dock icon
- [ ] Outlook/Mail: double-click a zip attachment → opens; "Extract to…" starts in ~/Downloads
- [ ] File → Open (⌘O) and dropping an archive on the empty window
- [ ] Double-click a `.pdf` inside a zip → opens in Preview; second double-click reuses the temp copy
- [ ] Select 3 files + 1 folder, ⌘E, choose `~/Desktop/x` → exactly those appear, structure preserved
- [ ] Nothing selected, ⌘E → everything extracted
- [ ] "Into a subfolder named after the archive" default: on for multi-root archives, off for single-root
- [ ] Overwrite sheet: Skip / Replace / Keep both; "Apply to all" asks only once
- [ ] Encrypted zip (AES, made by 7-Zip or `zip -e`): wrong password → prompt again; right password → extracts
- [ ] Encrypted 7z with encrypted headers: password prompt before the list appears
- [ ] Archive → Test (⌘T) on a good archive → "OK"; on a truncated file → error with the entry name
- [ ] Cancel a long extraction → no partial file left behind
- [ ] Window close / app quit removes `$TMPDIR/Zipper/...`
- [ ] Quit with an extraction running → extraction cancelled cleanly
- [ ] Dark mode looks right
- [ ] `7zz t` on an archive made by `archive-core` (zip and 7z, with and without password) → OK

## Phase 2 — move things out and in

- [ ] Drag two files from the window to the Desktop → both appear
- [ ] Drag a folder out → recursive
- [ ] ⌘N → empty editable window; drop three files; ⌘S → save panel with format/level/password
- [ ] Save as 7z with password, reopen, extract
- [ ] Add a file to an existing zip via drag-in and via Add Files…
- [ ] Delete one entry, rename one entry, reopen → changes present; `7zz t` still OK
- [ ] tar.* archives are read-only: Add/Delete/Rename greyed out with status-bar note

## Phase 3 — polish

- [ ] Spacebar Quick Look on a file row
- [ ] Preferences: default extract location, default format, reveal-in-Finder toggle
- [ ] Column widths and window frame persist across launches
- [ ] Everything reachable from the keyboard
