//! `archive-core`: format-agnostic archive listing, extraction and (later) writing.
//!
//! The UI never touches a format crate directly; everything goes through
//! [`open`], the [`Archive`] trait and the free functions in [`write`].
//!
//! Design rules (see SPEC.md §3.1):
//! - path safety is enforced in one place ([`safe_path`]) for every format;
//! - extraction streams, never buffers whole archives;
//! - every long operation takes a cancel flag and reports progress;
//! - archives are re-opened from their path for each operation, so an
//!   `Archive` is cheap to share between threads (it holds metadata only).

mod detect;
mod error;
mod fsutil;
mod path;
mod sevenz;
mod stream;
mod tar_fmt;
pub mod write;
mod zip_fmt;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

pub use detect::detect_format;
pub use error::{Error, Result};
pub use path::safe_path;

/// Archive container / compression format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    SevenZ,
    Zip,
    Tar,
    TarGz,
    TarBz2,
    TarXz,
    TarZst,
    /// Single-stream compressed file (one entry, size usually unknown).
    Gz,
    Xz,
    Bz2,
    Zst,
}

impl Format {
    /// Whether `archive-core` can create / modify archives of this format.
    pub fn is_writable(self) -> bool {
        matches!(self, Format::Zip | Format::SevenZ)
    }

    /// Human readable short name, e.g. for a status bar.
    pub fn display_name(self) -> &'static str {
        match self {
            Format::SevenZ => "7z",
            Format::Zip => "zip",
            Format::Tar => "tar",
            Format::TarGz => "tar.gz",
            Format::TarBz2 => "tar.bz2",
            Format::TarXz => "tar.xz",
            Format::TarZst => "tar.zst",
            Format::Gz => "gzip",
            Format::Xz => "xz",
            Format::Bz2 => "bzip2",
            Format::Zst => "zstd",
        }
    }

    /// Preferred file extension (without the dot).
    pub fn extension(self) -> &'static str {
        match self {
            Format::SevenZ => "7z",
            Format::Zip => "zip",
            Format::Tar => "tar",
            Format::TarGz => "tar.gz",
            Format::TarBz2 => "tar.bz2",
            Format::TarXz => "tar.xz",
            Format::TarZst => "tar.zst",
            Format::Gz => "gz",
            Format::Xz => "xz",
            Format::Bz2 => "bz2",
            Format::Zst => "zst",
        }
    }
}

/// One entry (file, directory or symlink) inside an archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Normalised path inside the archive: relative, `/`-separated, no `.`/`..`,
    /// no trailing slash. See [`safe_path`].
    pub path: PathBuf,
    pub kind: EntryKind,
    /// Uncompressed size. `None` when the format does not record it
    /// (single-stream gz/xz/… files).
    pub size: Option<u64>,
    pub packed_size: Option<u64>,
    pub modified: Option<SystemTime>,
    pub encrypted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    /// Symbolic link; target is kept per-format and validated on extraction.
    Symlink,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Dir
    }
    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

/// Progress report passed to the progress callback.
#[derive(Debug, Clone)]
pub struct Progress {
    /// Bytes written so far (uncompressed).
    pub done: u64,
    /// Total bytes expected, when known. `None` for single-stream formats.
    pub total: Option<u64>,
    /// Entry currently being processed.
    pub current: PathBuf,
    pub entries_done: usize,
    pub entries_total: usize,
}

pub type ProgressFn<'a> = &'a (dyn Fn(&Progress) + Sync);

/// What to do when an extracted file already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictAction {
    Skip,
    Replace,
    /// Write next to the existing file as "name 2.ext", "name 3.ext", …
    KeepBoth,
}

/// A decision returned by the conflict callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConflictDecision {
    pub action: ConflictAction,
    /// Apply the same action to every later conflict without asking again.
    pub apply_to_all: bool,
}

/// Overwrite policy for an extraction.
pub enum Overwrite<'a> {
    /// Ask through the callback on every conflict (until it answers with `apply_to_all`).
    Ask(&'a (dyn Fn(&Path) -> ConflictDecision + Sync)),
    Always(ConflictAction),
}

impl Default for Overwrite<'_> {
    /// Safe default: never overwrite.
    fn default() -> Self {
        Overwrite::Always(ConflictAction::Skip)
    }
}

pub struct ExtractOptions<'a> {
    pub overwrite: Overwrite<'a>,
    pub password: Option<String>,
    /// Extract into `dest/<archive stem>/` instead of `dest/` directly.
    pub into_subfolder: bool,
    /// Restore modification times when the archive records them.
    pub preserve_mtime: bool,
    /// Strip this prefix from entry paths before writing them under `dest`
    /// (used when the UI has navigated into a sub-folder of the archive).
    pub strip_prefix: Option<PathBuf>,
}

impl Default for ExtractOptions<'_> {
    fn default() -> Self {
        Self {
            overwrite: Overwrite::default(),
            password: None,
            into_subfolder: false,
            preserve_mtime: true,
            strip_prefix: None,
        }
    }
}

/// Outcome of a successful extraction.
#[derive(Debug, Default, Clone)]
pub struct ExtractReport {
    /// Files actually written (final on-disk paths).
    pub written: Vec<PathBuf>,
    /// Entries skipped because they already existed and the policy was Skip.
    pub skipped_existing: Vec<PathBuf>,
    /// Entries skipped because their path or symlink target was unsafe.
    pub skipped_unsafe: Vec<PathBuf>,
    pub bytes_written: u64,
}

/// Cancellation flag shared between the UI and a running operation.
#[derive(Debug, Default)]
pub struct Cancel(AtomicBool);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    pub(crate) fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// An opened archive. Holds the entry list and the path; every operation
/// re-opens the file, so the value is `Send + Sync` and cheap to clone into
/// a worker thread.
pub trait Archive: Send + Sync {
    fn format(&self) -> Format;
    fn path(&self) -> &Path;
    fn entries(&self) -> &[Entry];

    /// `true` when at least one entry is encrypted and no (valid) password is known.
    fn needs_password(&self) -> bool;

    /// Extract `selection` (entry paths; a directory selects everything below it;
    /// an empty selection means every entry) into `dest`.
    fn extract(
        &self,
        selection: &[PathBuf],
        dest: &Path,
        opts: &ExtractOptions<'_>,
        progress: ProgressFn<'_>,
        cancel: &Cancel,
    ) -> Result<ExtractReport>;

    /// Verify every entry can be decoded and checksums match.
    fn test(&self, password: Option<&str>, progress: ProgressFn<'_>, cancel: &Cancel)
        -> Result<()>;

    /// Stream every entry for which `map` returns a new path into `builder`
    /// (used by [`write::modify`]). Formats that cannot be written return
    /// [`Error::ReadOnlyFormat`].
    fn copy_into(
        &self,
        builder: &mut write::Builder,
        map: &dyn Fn(&Entry) -> Option<PathBuf>,
        password: Option<&str>,
        progress: ProgressFn<'_>,
        cancel: &Cancel,
    ) -> Result<()>;
}

/// Open an archive, detecting its format from magic bytes (extension as fallback).
///
/// For zip files with encrypted entries this succeeds without a password
/// (the listing is in the clear); [`Archive::needs_password`] reports it.
/// For 7z files with encrypted headers it returns [`Error::PasswordRequired`].
pub fn open(path: &Path, password: Option<&str>) -> Result<Box<dyn Archive>> {
    let format = detect_format(path)?;
    open_as(path, format, password)
}

/// Open with a known format (skips detection).
pub fn open_as(path: &Path, format: Format, password: Option<&str>) -> Result<Box<dyn Archive>> {
    Ok(match format {
        Format::Zip => Box::new(zip_fmt::ZipArchiveImpl::open(path, password)?),
        Format::SevenZ => Box::new(sevenz::SevenZArchive::open(path, password)?),
        Format::Tar | Format::TarGz | Format::TarBz2 | Format::TarXz | Format::TarZst => {
            Box::new(tar_fmt::TarArchive::open(path, format)?)
        }
        Format::Gz | Format::Xz | Format::Bz2 | Format::Zst => {
            Box::new(stream::StreamArchive::open(path, format)?)
        }
    })
}

/// Helpers shared by the format implementations.
pub(crate) mod common {
    use super::*;

    /// Resolve a selection against the entry list: returns the indices of
    /// entries to extract, in archive order. Empty selection = everything.
    pub fn select(entries: &[Entry], selection: &[PathBuf]) -> Vec<usize> {
        if selection.is_empty() {
            return (0..entries.len()).collect();
        }
        let wanted: Vec<PathBuf> = selection
            .iter()
            .map(|p| path::normalise_selection(p))
            .collect();
        entries
            .iter()
            .enumerate()
            .filter(|(_, e)| wanted.iter().any(|w| e.path == *w || e.path.starts_with(w)))
            .map(|(i, _)| i)
            .collect()
    }

    /// The directory everything goes under, honouring `into_subfolder`.
    pub fn target_root(archive_path: &Path, dest: &Path, opts: &ExtractOptions<'_>) -> PathBuf {
        if opts.into_subfolder {
            dest.join(path::archive_stem(archive_path))
        } else {
            dest.to_path_buf()
        }
    }

    /// Relative on-disk path for an entry, after `strip_prefix`.
    pub fn relative_target(entry_path: &Path, opts: &ExtractOptions<'_>) -> Option<PathBuf> {
        match &opts.strip_prefix {
            Some(prefix) => entry_path.strip_prefix(prefix).ok().map(Path::to_path_buf),
            None => Some(entry_path.to_path_buf()),
        }
    }

    pub fn total_size(entries: &[Entry], idx: &[usize]) -> Option<u64> {
        let mut total = 0u64;
        for &i in idx {
            total += entries[i].size?;
        }
        Some(total)
    }
}
