//! Creating and editing archives (zip and 7z).
//!
//! Editing never touches the original in place: [`modify`] streams the kept
//! entries of the existing archive plus any new files into a `.part` file
//! next to it and renames that over the original when everything succeeded.

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
    /// Split the finished archive into volumes of this many bytes
    /// (`name.7z.001`, `.002`, …). 7-Zip style; `None` = single file.
    pub volume_size: Option<u64>,
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self {
            format: Format::Zip,
            level: 6,
            password: None,
            solid: false,
            encrypt_header: true,
            volume_size: None,
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

    /// Add a symbolic link entry.
    pub fn add_symlink(
        &mut self,
        name: &str,
        target: &Path,
        modified: Option<SystemTime>,
    ) -> Result<()> {
        let p = crate::safe_path(name)?;
        let name = p.to_string_lossy().replace('\\', "/");
        let target_s = target.to_string_lossy().into_owned();
        match &mut self.inner {
            Inner::Zip(w) => {
                let o = Self::zip_options(&self.opts, modified, Some(0o777));
                w.add_symlink(name, target_s, o)?;
            }
            Inner::SevenZ(w) => {
                let mut e = sevenz_rust2::ArchiveEntry::new_file(&name);
                apply_sz_time(&mut e, modified);
                e.has_windows_attributes = true;
                e.windows_attributes = 0x8000 | (0o120777 << 16);
                w.push_archive_entry(e, Some(target_s.as_bytes()))?;
            }
        }
        Ok(())
    }

    /// Zip only: copy an entry from another zip without recompressing or
    /// re-encrypting it. Used by [`modify`] so edits are fast and lossless.
    pub(crate) fn raw_copy_zip<R: Read>(
        &mut self,
        file: zip::read::ZipFile<'_, R>,
        new_name: &str,
    ) -> Result<()> {
        match &mut self.inner {
            Inner::Zip(w) => {
                w.raw_copy_file_rename(file, new_name)?;
                Ok(())
            }
            Inner::SevenZ(_) => Err(Error::Corrupt("raw copy into a 7z writer".into())),
        }
    }

    /// Finish writing and move the temporary file to its final name
    /// (or split it into volumes when `volume_size` is set).
    pub fn finish(mut self) -> Result<PathBuf> {
        self.finished = true;
        let inner = std::mem::replace(
            &mut self.inner,
            Inner::Zip(Box::new(zip::ZipWriter::new(BufWriter::new(
                tempfile::tempfile()?,
            )))),
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
        if let Some(size) = self.opts.volume_size.filter(|&s| s > 0) {
            return self.split_volumes(size);
        }
        if self.dest.exists() {
            fs::remove_file(&self.dest)?;
        }
        fs::rename(&self.tmp, &self.dest)?;
        Ok(self.dest.clone())
    }

    fn split_volumes(&self, size: u64) -> Result<PathBuf> {
        let total = fs::metadata(&self.tmp)?.len();
        let count = total.div_ceil(size).max(1);
        let name = |i: u64| {
            let base = self.dest.to_string_lossy().into_owned();
            PathBuf::from(format!("{base}.{i:03}"))
        };
        // Remove stale volumes from an earlier run.
        for i in 1..=9999u64 {
            let p = name(i);
            if p.exists() {
                fs::remove_file(&p)?;
            } else if i > count {
                break;
            }
        }
        let mut src = File::open(&self.tmp)?;
        let mut buf = vec![0u8; BUF];
        for i in 1..=count {
            let mut out = BufWriter::new(File::create(name(i))?);
            let mut left = size;
            while left > 0 {
                let want = buf.len().min(left as usize);
                let n = src.read(&mut buf[..want])?;
                if n == 0 {
                    break;
                }
                out.write_all(&buf[..n])?;
                left -= n as u64;
            }
            out.flush()?;
        }
        fs::remove_file(&self.tmp)?;
        if self.dest.exists() {
            fs::remove_file(&self.dest)?;
        }
        Ok(name(1))
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

/// A file or folder on disk to add, with the entry path it gets inside the
/// archive (`name` may contain `/` to place it under a folder).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub disk: PathBuf,
    pub name: String,
}

impl Source {
    /// Top-level entry named after the file name.
    pub fn from_path(disk: &Path) -> Result<Self> {
        let name = disk
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .ok_or_else(|| Error::UnsafePath(disk.to_path_buf()))?;
        Ok(Self {
            disk: disk.to_path_buf(),
            name,
        })
    }
}

/// One item produced by walking sources: what `create`/`modify` will write.
#[derive(Debug, Clone)]
pub struct SourceItem {
    pub disk: PathBuf,
    pub name: String,
    pub kind: crate::EntryKind,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

/// Expand sources recursively (directories first, children sorted).
/// `.DS_Store` files are skipped; symlinks are kept as symlinks.
pub fn list_sources(sources: &[Source]) -> Result<Vec<SourceItem>> {
    let mut out = Vec::new();
    for s in sources {
        crate::safe_path(&s.name)?;
        walk(&s.disk, &s.name, &mut out)?;
    }
    Ok(out)
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
    let srcs = sources
        .iter()
        .map(|p| Source::from_path(p))
        .collect::<Result<Vec<_>>>()?;
    create_from(dest, &srcs, opts, progress, cancel)
}

/// Create an archive from named sources.
pub fn create_from(
    dest: &Path,
    sources: &[Source],
    opts: &WriteOptions,
    progress: ProgressFn<'_>,
    cancel: &Cancel,
) -> Result<PathBuf> {
    let items = list_sources(sources)?;
    let mut b = Builder::create(dest, opts)?;
    add_items(&mut b, &items, 0, progress, cancel)?;
    b.finish()
}

fn add_items(
    b: &mut Builder,
    items: &[SourceItem],
    entries_before: usize,
    progress: ProgressFn<'_>,
    cancel: &Cancel,
) -> Result<u64> {
    let total: u64 = items
        .iter()
        .filter(|i| i.kind == crate::EntryKind::File)
        .map(|i| i.size)
        .sum();
    let mut done = 0u64;
    for (i, item) in items.iter().enumerate() {
        cancel.check()?;
        progress(&Progress {
            done,
            total: Some(total),
            current: PathBuf::from(&item.name),
            entries_done: entries_before + i,
            entries_total: entries_before + items.len(),
        });
        match item.kind {
            crate::EntryKind::Dir => b.add_dir(&item.name, item.modified)?,
            crate::EntryKind::Symlink => {
                let target = fs::read_link(&item.disk)?;
                b.add_symlink(&item.name, &target, item.modified)?;
            }
            crate::EntryKind::File => {
                let meta = fs::metadata(&item.disk)?;
                let f = File::open(&item.disk)?;
                done += b.add_file(&item.name, f, item.modified, unix_mode(&meta), cancel)?;
            }
        }
    }
    Ok(done)
}

fn walk(path: &Path, name: &str, out: &mut Vec<SourceItem>) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    let modified = meta.modified().ok();
    if meta.file_type().is_symlink() {
        out.push(SourceItem {
            disk: path.to_path_buf(),
            name: name.to_string(),
            kind: crate::EntryKind::Symlink,
            size: 0,
            modified,
        });
    } else if meta.is_dir() {
        out.push(SourceItem {
            disk: path.to_path_buf(),
            name: name.to_string(),
            kind: crate::EntryKind::Dir,
            size: 0,
            modified,
        });
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
        out.push(SourceItem {
            disk: path.to_path_buf(),
            name: name.to_string(),
            kind: crate::EntryKind::File,
            size: meta.len(),
            modified,
        });
    }
    // Special files (sockets, devices) are skipped.
    Ok(())
}

/// A set of changes to apply to an existing archive in one rewrite.
#[derive(Debug, Default, Clone)]
pub struct Edit {
    /// Entries to drop; a directory drops everything below it.
    pub remove: Vec<PathBuf>,
    /// `(from, to)` renames; a directory rename moves everything below it.
    pub rename: Vec<(PathBuf, PathBuf)>,
    /// New files/folders to add. An added name that already exists replaces
    /// the old entry (and, for a folder, everything below it).
    pub add: Vec<Source>,
}

impl Edit {
    pub fn is_empty(&self) -> bool {
        self.remove.is_empty() && self.rename.is_empty() && self.add.is_empty()
    }

    /// Where an existing entry ends up: `None` = dropped.
    pub fn map(&self, path: &Path) -> Option<PathBuf> {
        for r in &self.remove {
            if path == r || path.starts_with(r) {
                return None;
            }
        }
        for a in &self.add {
            let ap = Path::new(&a.name);
            if path == ap || path.starts_with(ap) {
                return None;
            }
        }
        let mut p = path.to_path_buf();
        for (from, to) in &self.rename {
            if p == *from {
                p = to.clone();
                break;
            }
            if let Ok(rest) = p.strip_prefix(from) {
                p = to.join(rest);
                break;
            }
        }
        Some(p)
    }
}

/// Rewrite `archive` applying `edit`. Compression level, password and
/// volume settings come from `opts`; the format is the archive's own.
/// For zip, kept entries are copied byte-for-byte (no recompression).
pub fn modify(
    archive_path: &Path,
    edit: &Edit,
    opts: &WriteOptions,
    progress: ProgressFn<'_>,
    cancel: &Cancel,
) -> Result<PathBuf> {
    let archive = crate::open(archive_path, opts.password.as_deref())?;
    let format = archive.format();
    if !format.is_writable() {
        return Err(Error::ReadOnlyFormat(format));
    }
    let items = list_sources(&edit.add)?;
    let write_opts = WriteOptions {
        format,
        ..opts.clone()
    };
    let mut b = Builder::create(archive_path, &write_opts)?;
    let kept = archive
        .entries()
        .iter()
        .filter(|e| edit.map(&e.path).is_some())
        .count();
    let map = |e: &crate::Entry| edit.map(&e.path);
    archive.copy_into(&mut b, &map, opts.password.as_deref(), progress, cancel)?;
    add_items(&mut b, &items, kept, progress, cancel)?;
    b.finish()
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
