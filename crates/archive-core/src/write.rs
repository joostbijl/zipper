//! Creating archives (zip and 7z). Editing (add/remove/rename) is Phase 2
//! and will be built on [`Builder`] with a rewrite-to-temp strategy.

use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::{Cancel, Error, Format, Progress, ProgressFn, Result};

/// 0 = store, 1 = fastest … 9 = best. Mapped per format.
#[derive(Debug, Clone)]
pub struct WriteOptions {
    pub format: Format,
    pub level: u8,
    pub password: Option<String>,
    /// 7z only: put all files in one solid block (better ratio, slower random access).
    pub solid: bool,
    /// 7z only: also encrypt the file list (requires `password`).
    pub encrypt_header: bool,
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self {
            format: Format::Zip,
            level: 6,
            password: None,
            solid: false,
            encrypt_header: true,
        }
    }
}

/// Incremental archive writer. Entry names use `/` separators and are
/// validated with [`crate::safe_path`].
pub struct Builder {
    inner: Inner,
    opts: WriteOptions,
    tmp: PathBuf,
    dest: PathBuf,
    finished: bool,
}

enum Inner {
    Zip(Box<zip::ZipWriter<BufWriter<File>>>),
    SevenZ(Box<sevenz_rust2::ArchiveWriter<BufWriter<File>>>),
}

const BUF: usize = 256 * 1024;

impl Builder {
    /// Start a new archive. Data is written to a temporary file next to
    /// `dest` and renamed into place by [`Builder::finish`].
    pub fn create(dest: &Path, opts: &WriteOptions) -> Result<Self> {
        if !opts.format.is_writable() {
            return Err(Error::ReadOnlyFormat(opts.format));
        }
        let dir = dest
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(
            ".{}.{}.part",
            dest.file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            std::process::id()
        ));
        let file = BufWriter::with_capacity(BUF, File::create(&tmp)?);
        let inner = match opts.format {
            Format::Zip => Inner::Zip(Box::new(zip::ZipWriter::new(file))),
            Format::SevenZ => {
                let mut w = sevenz_rust2::ArchiveWriter::new(file)?;
                let mut methods: Vec<sevenz_rust2::EncoderConfiguration> = Vec::new();
                if let Some(pw) = &opts.password {
                    methods.push(
                        sevenz_rust2::encoder_options::AesEncoderOptions::new(
                            sevenz_rust2::Password::new(pw),
                        )
                        .into(),
                    );
                }
                if opts.level == 0 {
                    methods.push(sevenz_rust2::EncoderMethod::COPY.into());
                } else {
                    let level = (opts.level as u32).clamp(1, 9);
                    methods.push(
                        sevenz_rust2::encoder_options::Lzma2Options::from_level(level).into(),
                    );
                }
                w.set_content_methods(methods);
                w.set_encrypt_header(opts.password.is_some() && opts.encrypt_header);
                Inner::SevenZ(Box::new(w))
            }
            other => return Err(Error::ReadOnlyFormat(other)),
        };
        Ok(Self {
            inner,
            opts: opts.clone(),
            tmp,
            dest: dest.to_path_buf(),
            finished: false,
        })
    }

    fn zip_options(
        opts: &WriteOptions,
        modified: Option<SystemTime>,
        mode: Option<u32>,
    ) -> zip::write::SimpleFileOptions {
        let method = if opts.level == 0 {
            zip::CompressionMethod::Stored
        } else {
            zip::CompressionMethod::Deflated
        };
        let mut o = zip::write::SimpleFileOptions::default()
            .compression_method(method)
            .large_file(true)
            .unix_permissions(mode.unwrap_or(0o644));
        if opts.level > 0 {
            o = o.compression_level(Some(opts.level as i64));
        }
        if let Some(m) = modified.and_then(system_time_to_zip) {
            o = o.last_modified_time(m);
        }
        o
    }

    pub fn add_dir(&mut self, name: &str, modified: Option<SystemTime>) -> Result<()> {
        let p = crate::safe_path(name)?;
        let name = p.to_string_lossy().replace('\\', "/");
        match &mut self.inner {
            Inner::Zip(w) => {
                let o = Self::zip_options(&self.opts, modified, Some(0o755));
                // Password is applied per entry; directories carry none.
                w.add_directory(format!("{name}/"), o)?;
            }
            Inner::SevenZ(w) => {
                let mut e = sevenz_rust2::ArchiveEntry::new_directory(&name);
                apply_sz_time(&mut e, modified);
                w.push_archive_entry::<&[u8]>(e, None)?;
            }
        }
        Ok(())
    }

    /// Add a file entry streamed from `reader`.
    pub fn add_file<R: Read>(
        &mut self,
        name: &str,
        mut reader: R,
        modified: Option<SystemTime>,
        mode: Option<u32>,
        cancel: &Cancel,
    ) -> Result<u64> {
        let p = crate::safe_path(name)?;
        let name = p.to_string_lossy().replace('\\', "/");
        match &mut self.inner {
            Inner::Zip(w) => {
                let base = Self::zip_options(&self.opts, modified, mode);
                let mut written = 0u64;
                let mut buf = vec![0u8; BUF];
                let pw = self.opts.password.clone();
                match pw {
                    Some(pw) => {
                        let o = base.with_aes_encryption(zip::AesMode::Aes256, &pw);
                        w.start_file(name, o)?;
                    }
                    None => w.start_file(name, base)?,
                }
                loop {
                    cancel.check()?;
                    let n = match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(e.into()),
                    };
                    w.write_all(&buf[..n])?;
                    written += n as u64;
                }
                Ok(written)
            }
            Inner::SevenZ(w) => {
                let mut e = sevenz_rust2::ArchiveEntry::new_file(&name);
                apply_sz_time(&mut e, modified);
                if let Some(m) = mode {
                    e.has_windows_attributes = true;
                    e.windows_attributes = 0x8000 | (m << 16);
                }
                let counted = Counting {
                    inner: reader,
                    n: 0,
                    cancel,
                };
                let cell = std::cell::Cell::new(0u64);
                let r = Tee {
                    inner: counted,
                    total: &cell,
                };
                let entry = w.push_archive_entry(e, Some(r))?;
                let _ = entry;
                Ok(cell.get())
            }
        }
    }

    /// Finish writing and move the temporary file to its final name.
    pub fn finish(mut self) -> Result<PathBuf> {
        self.finished = true;
        let inner = std::mem::replace(
            &mut self.inner,
            Inner::Zip(Box::new(zip::ZipWriter::new(BufWriter::new(tempfile::tempfile()?)))),
        );
        match inner {
            Inner::Zip(w) => {
                let mut f = w.finish()?;
                f.flush()?;
            }
            Inner::SevenZ(w) => {
                let mut f = w.finish()?;
                f.flush()?;
            }
        }
        if self.dest.exists() {
            fs::remove_file(&self.dest)?;
        }
        fs::rename(&self.tmp, &self.dest)?;
        Ok(self.dest.clone())
    }
}

impl Drop for Builder {
    fn drop(&mut self) {
        if !self.finished {
            let _ = fs::remove_file(&self.tmp);
        }
    }
}

struct Counting<'a, R> {
    inner: R,
    n: u64,
    cancel: &'a Cancel,
}
impl<R: Read> Read for Counting<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancel.is_cancelled() {
            return Err(io::Error::other("cancelled"));
        }
        let k = self.inner.read(buf)?;
        self.n += k as u64;
        Ok(k)
    }
}
struct Tee<'a, R> {
    inner: R,
    total: &'a std::cell::Cell<u64>,
}
impl<R: Read> Read for Tee<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let k = self.inner.read(buf)?;
        self.total.set(self.total.get() + k as u64);
        Ok(k)
    }
}

fn apply_sz_time(e: &mut sevenz_rust2::ArchiveEntry, modified: Option<SystemTime>) {
    if let Some(m) = modified {
        if let Ok(t) = sevenz_rust2::NtTime::try_from(m) {
            e.last_modified_date = t;
            e.has_last_modified_date = true;
        }
    }
}

fn system_time_to_zip(t: SystemTime) -> Option<zip::DateTime> {
    let secs = t.duration_since(SystemTime::UNIX_EPOCH).ok()?.as_secs() as i64;
    // civil from days (Howard Hinnant)
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    zip::DateTime::from_date_and_time(
        y.clamp(1980, 2107) as u16,
        m as u8,
        d as u8,
        (rem / 3600) as u8,
        ((rem % 3600) / 60) as u8,
        (rem % 60) as u8,
    )
    .ok()
}

/// Create an archive from filesystem paths. Each source becomes a top-level
/// entry named after its file name; directories are added recursively.
pub fn create(
    dest: &Path,
    sources: &[PathBuf],
    opts: &WriteOptions,
    progress: ProgressFn<'_>,
    cancel: &Cancel,
) -> Result<PathBuf> {
    // Collect the file list first so progress has a total.
    let mut files: Vec<(PathBuf, String, bool)> = Vec::new(); // (disk path, entry name, is_dir)
    for src in sources {
        let name = src
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .ok_or_else(|| Error::UnsafePath(src.clone()))?;
        walk(src, &name, &mut files)?;
    }
    let total: u64 = files
        .iter()
        .filter(|(_, _, d)| !d)
        .filter_map(|(p, _, _)| fs::metadata(p).ok().map(|m| m.len()))
        .sum();

    let mut b = Builder::create(dest, opts)?;
    let mut done = 0u64;
    for (i, (disk, name, is_dir)) in files.iter().enumerate() {
        cancel.check()?;
        progress(&Progress {
            done,
            total: Some(total),
            current: PathBuf::from(name),
            entries_done: i,
            entries_total: files.len(),
        });
        let meta = fs::symlink_metadata(disk)?;
        let modified = meta.modified().ok();
        if *is_dir {
            b.add_dir(name, modified)?;
        } else {
            let mode = unix_mode(&meta);
            let f = File::open(disk)?;
            done += b.add_file(name, f, modified, mode, cancel)?;
        }
    }
    b.finish()
}

fn walk(path: &Path, name: &str, out: &mut Vec<(PathBuf, String, bool)>) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        out.push((path.to_path_buf(), name.to_string(), true));
        let mut children: Vec<_> = fs::read_dir(path)?.collect::<io::Result<Vec<_>>>()?;
        children.sort_by_key(|e| e.file_name());
        for child in children {
            let cname = child.file_name().to_string_lossy().into_owned();
            if cname == ".DS_Store" {
                continue;
            }
            walk(&child.path(), &format!("{name}/{cname}"), out)?;
        }
    } else if meta.is_file() {
        out.push((path.to_path_buf(), name.to_string(), false));
    }
    // Symlinks and special files are skipped when creating (Phase 2 may revisit).
    Ok(())
}

#[cfg(unix)]
fn unix_mode(meta: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(meta.permissions().mode() & 0o777)
}
#[cfg(not(unix))]
fn unix_mode(_meta: &fs::Metadata) -> Option<u32> {
    None
}

#[allow(dead_code)]
fn _assert_seek<W: Write + Seek>(_w: &W) {}
