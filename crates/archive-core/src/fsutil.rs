//! Shared on-disk writing: conflict resolution, streaming copy with progress
//! and cancellation, mtime restoration, safe symlink creation.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use crate::path::{keep_both_name, symlink_target_is_inside};
use crate::{
    Cancel, ConflictAction, Entry, Error, ExtractOptions, ExtractReport, Overwrite, Progress,
    ProgressFn, Result,
};

const BUF_SIZE: usize = 256 * 1024;

/// Per-extraction state: the conflict policy once "apply to all" was chosen,
/// the progress counters and the report being built.
pub(crate) struct Sink<'a> {
    pub root: PathBuf,
    pub opts: &'a ExtractOptions<'a>,
    pub progress: ProgressFn<'a>,
    pub cancel: &'a Cancel,
    sticky: Mutex<Option<ConflictAction>>,
    pub report: Mutex<ExtractReport>,
    pub done: Mutex<u64>,
    pub total: Option<u64>,
    pub entries_total: usize,
    pub entries_done: Mutex<usize>,
}

/// What `prepare` decided for one entry.
pub(crate) enum Plan {
    Write(PathBuf),
    Skip,
}

impl<'a> Sink<'a> {
    pub fn new(
        root: PathBuf,
        opts: &'a ExtractOptions<'a>,
        progress: ProgressFn<'a>,
        cancel: &'a Cancel,
        total: Option<u64>,
        entries_total: usize,
    ) -> Result<Self> {
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            opts,
            progress,
            cancel,
            sticky: Mutex::new(match opts.overwrite {
                Overwrite::Always(a) => Some(a),
                Overwrite::Ask(_) => None,
            }),
            report: Mutex::new(ExtractReport::default()),
            done: Mutex::new(0),
            total,
            entries_total,
            entries_done: Mutex::new(0),
        })
    }

    pub fn emit(&self, current: &Path) {
        let p = Progress {
            done: *self.done.lock().unwrap(),
            total: self.total,
            current: current.to_path_buf(),
            entries_done: *self.entries_done.lock().unwrap(),
            entries_total: self.entries_total,
        };
        (self.progress)(&p);
    }

    fn add_done(&self, n: u64) {
        *self.done.lock().unwrap() += n;
    }

    pub fn finish_entry(&self) {
        *self.entries_done.lock().unwrap() += 1;
    }

    /// Absolute target path for an entry, or `None` if it falls outside the
    /// root after `strip_prefix` (then it is simply not part of the selection).
    pub fn target_for(&self, entry_path: &Path) -> Option<PathBuf> {
        let rel = crate::common::relative_target(entry_path, self.opts)?;
        if rel.as_os_str().is_empty() {
            return None;
        }
        Some(self.root.join(rel))
    }

    /// Decide whether and where to write a *file* entry at `target`.
    pub fn prepare(&self, entry_path: &Path, target: &Path) -> Result<Plan> {
        self.cancel.check()?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let exists = fs::symlink_metadata(target).is_ok();
        if !exists {
            return Ok(Plan::Write(target.to_path_buf()));
        }
        let action = {
            let sticky = *self.sticky.lock().unwrap();
            match sticky {
                Some(a) => a,
                None => match &self.opts.overwrite {
                    Overwrite::Always(a) => *a,
                    Overwrite::Ask(cb) => {
                        let d = cb(target);
                        if d.apply_to_all {
                            *self.sticky.lock().unwrap() = Some(d.action);
                        }
                        d.action
                    }
                },
            }
        };
        match action {
            ConflictAction::Skip => {
                self.report
                    .lock()
                    .unwrap()
                    .skipped_existing
                    .push(entry_path.to_path_buf());
                Ok(Plan::Skip)
            }
            ConflictAction::Replace => {
                let meta = fs::symlink_metadata(target)?;
                if meta.is_dir() {
                    fs::remove_dir_all(target)?;
                } else {
                    fs::remove_file(target)?;
                }
                Ok(Plan::Write(target.to_path_buf()))
            }
            ConflictAction::KeepBoth => Ok(Plan::Write(keep_both_name(target))),
        }
    }

    /// Stream `reader` into `target`, reporting progress and honouring cancel.
    /// A cancelled or failed write removes the partial file.
    pub fn write_file<R: Read>(
        &self,
        entry: &Entry,
        target: &Path,
        mut reader: R,
        mode: Option<u32>,
    ) -> Result<()> {
        let mut file = File::create(target)?;
        let result = self.copy(&mut reader, &mut file, &entry.path);
        drop(file);
        match result {
            Ok(()) => {}
            Err(e) => {
                let _ = fs::remove_file(target);
                return Err(e);
            }
        }
        apply_mode(target, mode);
        if self.opts.preserve_mtime {
            if let Some(m) = entry.modified {
                set_mtime(target, m);
            }
        }
        let mut r = self.report.lock().unwrap();
        r.written.push(target.to_path_buf());
        Ok(())
    }

    fn copy<R: Read, W: Write>(
        &self,
        reader: &mut R,
        writer: &mut W,
        current: &Path,
    ) -> Result<()> {
        let mut buf = vec![0u8; BUF_SIZE];
        let mut since_emit = 0u64;
        loop {
            self.cancel.check()?;
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(map_read_error(e)),
            };
            writer.write_all(&buf[..n])?;
            self.add_done(n as u64);
            self.report.lock().unwrap().bytes_written += n as u64;
            since_emit += n as u64;
            if since_emit >= 1 << 20 {
                since_emit = 0;
                self.emit(current);
            }
        }
        writer.flush()?;
        self.emit(current);
        Ok(())
    }

    pub fn make_dir(&self, entry: &Entry, target: &Path) -> Result<()> {
        self.cancel.check()?;
        fs::create_dir_all(target)?;
        if self.opts.preserve_mtime {
            if let Some(m) = entry.modified {
                set_mtime(target, m);
            }
        }
        Ok(())
    }

    /// Create a symlink only when its target cannot escape the extraction root.
    pub fn make_symlink(&self, entry: &Entry, target: &Path, link_target: &Path) -> Result<()> {
        let rel = target.strip_prefix(&self.root).unwrap_or(target);
        if !symlink_target_is_inside(rel, link_target) {
            self.report
                .lock()
                .unwrap()
                .skipped_unsafe
                .push(entry.path.clone());
            return Ok(());
        }
        match self.prepare(&entry.path, target)? {
            Plan::Skip => return Ok(()),
            Plan::Write(p) => {
                #[cfg(unix)]
                std::os::unix::fs::symlink(link_target, &p)?;
                #[cfg(not(unix))]
                {
                    let _ = (&p, link_target);
                    self.report
                        .lock()
                        .unwrap()
                        .skipped_unsafe
                        .push(entry.path.clone());
                    return Ok(());
                }
                self.report.lock().unwrap().written.push(p);
            }
        }
        Ok(())
    }

    pub fn note_unsafe(&self, raw: &str) {
        self.report
            .lock()
            .unwrap()
            .skipped_unsafe
            .push(PathBuf::from(raw));
    }

    pub fn into_report(self) -> ExtractReport {
        self.report.into_inner().unwrap()
    }
}

fn map_read_error(e: io::Error) -> Error {
    // Format crates often wrap their own errors inside io::Error::other.
    if let Some(inner) = e.get_ref() {
        if let Some(z) = inner.downcast_ref::<zip::result::ZipError>() {
            return Error::Corrupt(z.to_string());
        }
        let msg = inner.to_string();
        if msg.contains("password") || msg.contains("Password") {
            return Error::WrongPassword;
        }
        if e.kind() == io::ErrorKind::InvalidData || e.kind() == io::ErrorKind::UnexpectedEof {
            return Error::Corrupt(msg);
        }
    } else if e.kind() == io::ErrorKind::InvalidData || e.kind() == io::ErrorKind::UnexpectedEof {
        return Error::Corrupt(e.to_string());
    }
    Error::Io(e)
}

pub(crate) fn set_mtime(path: &Path, when: SystemTime) {
    let ft = filetime::FileTime::from_system_time(when);
    let _ = filetime::set_symlink_file_times(path, ft, ft);
}

#[cfg(unix)]
fn apply_mode(path: &Path, mode: Option<u32>) {
    use std::os::unix::fs::PermissionsExt;
    if let Some(m) = mode {
        // Keep it sane: only rwx bits, and always user-readable/writable.
        let bits = (m & 0o777) | 0o600;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(bits));
    }
}

#[cfg(not(unix))]
fn apply_mode(_path: &Path, _mode: Option<u32>) {}
