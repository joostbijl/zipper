//! Zip support via the `zip` crate.

use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use zip::ZipArchive;

use crate::fsutil::{Plan, Sink};
use crate::{
    common, safe_path, Archive, Cancel, Entry, EntryKind, Error, ExtractOptions, ExtractReport,
    Format, Progress, ProgressFn, Result,
};

pub struct ZipArchiveImpl {
    path: PathBuf,
    entries: Vec<Entry>,
    /// Index into the zip for each entry (entries with unsafe names are dropped
    /// from the list but their indices must not shift).
    indices: Vec<usize>,
    unix_modes: Vec<Option<u32>>,
    any_encrypted: bool,
    password: Option<String>,
}

fn open_zip(path: &Path) -> Result<ZipArchive<BufReader<File>>> {
    let file = File::open(path)?;
    Ok(ZipArchive::new(BufReader::new(file))?)
}

fn dos_to_system_time(dt: zip::DateTime) -> Option<SystemTime> {
    // Civil → unix seconds, treating the DOS timestamp as local-time-agnostic UTC.
    let (y, m, d) = (dt.year() as i64, dt.month() as i64, dt.day() as i64);
    let (hh, mm, ss) = (dt.hour() as i64, dt.minute() as i64, dt.second() as i64);
    // days from civil (Howard Hinnant's algorithm)
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hh * 3600 + mm * 60 + ss;
    if secs < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(secs as u64))
}

impl ZipArchiveImpl {
    pub fn open(path: &Path, password: Option<&str>) -> Result<Self> {
        let zip = open_zip(path)?;
        let mut entries = Vec::with_capacity(zip.len());
        let mut indices = Vec::with_capacity(zip.len());
        let mut unix_modes = Vec::with_capacity(zip.len());
        let mut any_encrypted = false;
        for i in 0..zip.len() {
            let f = zip.by_index_data(i)?;
            let raw_name = match f.name() {
                Ok(n) => n.into_owned(),
                Err(_) => String::from_utf8_lossy(f.name_raw()).into_owned(),
            };
            let Ok(p) = safe_path(&raw_name) else {
                // Unsafe names are not listed at all; they can never be extracted.
                continue;
            };
            let kind = if f.is_dir() {
                EntryKind::Dir
            } else if f.is_symlink() {
                EntryKind::Symlink
            } else {
                EntryKind::File
            };
            any_encrypted |= f.encrypted();
            entries.push(Entry {
                path: p,
                kind,
                size: Some(f.size()),
                packed_size: Some(f.compressed_size()),
                modified: f.last_modified().and_then(dos_to_system_time),
                encrypted: f.encrypted(),
            });
            indices.push(i);
            unix_modes.push(f.unix_mode());
        }
        Ok(Self {
            path: path.to_path_buf(),
            entries,
            indices,
            unix_modes,
            any_encrypted,
            password: password.map(str::to_owned),
        })
    }

    fn effective_password<'a>(&'a self, opts: Option<&'a str>) -> Option<&'a str> {
        opts.or(self.password.as_deref())
    }
}

/// Open entry `i` with or without a password.
fn entry_reader<'a>(
    zip: &'a mut ZipArchive<BufReader<File>>,
    i: usize,
    password: Option<&str>,
) -> Result<zip::read::ZipFile<'a, BufReader<File>>> {
    let encrypted = zip.by_index_data(i)?.encrypted();
    match (encrypted, password) {
        (true, None) => Err(Error::PasswordRequired),
        (true, Some(pw)) => Ok(zip.by_index_decrypt(i, pw.as_bytes())?),
        (false, _) => Ok(zip.by_index(i)?),
    }
}

impl Archive for ZipArchiveImpl {
    fn format(&self) -> Format {
        Format::Zip
    }
    fn path(&self) -> &Path {
        &self.path
    }
    fn entries(&self) -> &[Entry] {
        &self.entries
    }
    fn needs_password(&self) -> bool {
        self.any_encrypted && self.password.is_none()
    }

    fn extract(
        &self,
        selection: &[PathBuf],
        dest: &Path,
        opts: &ExtractOptions<'_>,
        progress: ProgressFn<'_>,
        cancel: &Cancel,
    ) -> Result<ExtractReport> {
        let idx = common::select(&self.entries, selection);
        let total = common::total_size(&self.entries, &idx);
        let root = common::target_root(&self.path, dest, opts);
        let sink = Sink::new(root, opts, progress, cancel, total, idx.len())?;
        let password = self.effective_password(opts.password.as_deref());

        let mut zip = open_zip(&self.path)?;

        // Directories first so empty ones exist, then files and links.
        for &k in idx
            .iter()
            .filter(|&&k| self.entries[k].kind == EntryKind::Dir)
        {
            let e = &self.entries[k];
            if let Some(target) = sink.target_for(&e.path) {
                sink.make_dir(e, &target)?;
            }
            sink.finish_entry();
        }
        for &k in idx
            .iter()
            .filter(|&&k| self.entries[k].kind != EntryKind::Dir)
        {
            let e = &self.entries[k];
            let Some(target) = sink.target_for(&e.path) else {
                sink.finish_entry();
                continue;
            };
            sink.emit(&e.path);
            let mut zf = entry_reader(&mut zip, self.indices[k], password)?;
            match e.kind {
                EntryKind::Symlink => {
                    let mut buf = Vec::new();
                    zf.read_to_end(&mut buf).map_err(map_zip_io)?;
                    let link_target = PathBuf::from(String::from_utf8_lossy(&buf).into_owned());
                    drop(zf);
                    sink.make_symlink(e, &target, &link_target)?;
                }
                _ => match sink.prepare(&e.path, &target)? {
                    Plan::Skip => {}
                    Plan::Write(p) => sink.write_file(e, &p, &mut zf, self.unix_modes[k])?,
                },
            }
            sink.finish_entry();
        }
        Ok(sink.into_report())
    }

    fn test(
        &self,
        password: Option<&str>,
        progress: ProgressFn<'_>,
        cancel: &Cancel,
    ) -> Result<()> {
        let password = self.effective_password(password);
        let mut zip = open_zip(&self.path)?;
        let total = common::total_size(&self.entries, &(0..self.entries.len()).collect::<Vec<_>>());
        let mut done = 0u64;
        let mut buf = vec![0u8; 256 * 1024];
        for (n, (e, &i)) in self.entries.iter().zip(&self.indices).enumerate() {
            cancel.check()?;
            progress(&Progress {
                done,
                total,
                current: e.path.clone(),
                entries_done: n,
                entries_total: self.entries.len(),
            });
            if e.kind == EntryKind::Dir {
                continue;
            }
            // Reading to EOF makes the zip crate verify the CRC.
            let mut zf = entry_reader(&mut zip, i, password)?;
            loop {
                cancel.check()?;
                match zf.read(&mut buf) {
                    Ok(0) => break,
                    Ok(k) => done += k as u64,
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                    Err(err) => return Err(map_zip_io(err)),
                }
            }
        }
        Ok(())
    }
}

fn map_zip_io(e: io::Error) -> Error {
    if let Some(inner) = e.get_ref() {
        if let Some(z) = inner.downcast_ref::<zip::result::ZipError>() {
            return match z {
                zip::result::ZipError::InvalidPassword => Error::WrongPassword,
                other => Error::Corrupt(other.to_string()),
            };
        }
    }
    match e.kind() {
        io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof => Error::Corrupt(e.to_string()),
        _ => Error::Io(e),
    }
}
