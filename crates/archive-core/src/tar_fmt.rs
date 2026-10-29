//! tar and compressed tar (gz/bz2/xz/zst) support.
//!
//! Listing needs a full pass over the (decompressed) stream, so `open` can be
//! slow for large tarballs; the UI shows a spinner for this case (SPEC §5).

use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use crate::fsutil::{Plan, Sink};
use crate::{
    common, safe_path, Archive, Cancel, Entry, EntryKind, Error, ExtractOptions, ExtractReport,
    Format, Progress, ProgressFn, Result,
};

pub struct TarArchive {
    path: PathBuf,
    format: Format,
    entries: Vec<Entry>,
    /// Entry index → ordinal of the tar entry in stream order.
    ordinals: Vec<usize>,
    modes: Vec<Option<u32>>,
    link_targets: Vec<Option<PathBuf>>,
}

pub(crate) fn open_stream(path: &Path, format: Format) -> Result<Box<dyn Read>> {
    let file = BufReader::with_capacity(1 << 20, File::open(path)?);
    Ok(match format {
        Format::Tar => Box::new(file),
        Format::TarGz | Format::Gz => Box::new(flate2::read::MultiGzDecoder::new(file)),
        Format::TarBz2 | Format::Bz2 => Box::new(bzip2::read::MultiBzDecoder::new(file)),
        Format::TarXz | Format::Xz => Box::new(liblzma::read::XzDecoder::new_multi_decoder(file)),
        Format::TarZst | Format::Zst => Box::new(zstd::stream::read::Decoder::new(file)?),
        Format::Zip | Format::SevenZ | Format::Iso | Format::Dmg => {
            return Err(Error::UnsupportedFormat)
        }
    })
}

fn map_io(e: io::Error) -> Error {
    match e.kind() {
        io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof => Error::Corrupt(e.to_string()),
        _ => Error::Io(e),
    }
}

impl TarArchive {
    pub fn open(path: &Path, format: Format) -> Result<Self> {
        let stream = open_stream(path, format)?;
        let mut archive = tar::Archive::new(stream);
        let mut entries = Vec::new();
        let mut ordinals = Vec::new();
        let mut modes = Vec::new();
        let mut link_targets = Vec::new();
        let mut seen: std::collections::HashMap<PathBuf, usize> = std::collections::HashMap::new();

        for (ordinal, entry) in archive.entries().map_err(map_io)?.enumerate() {
            let entry = entry.map_err(map_io)?;
            let header = entry.header();
            let raw = entry.path().map_err(map_io)?.to_string_lossy().into_owned();
            let Ok(p) = safe_path(&raw) else { continue };
            use tar::EntryType as T;
            let kind = match header.entry_type() {
                T::Directory => EntryKind::Dir,
                T::Symlink => EntryKind::Symlink,
                T::Regular | T::Continuous | T::GNUSparse | T::Link => EntryKind::File,
                // Extended-header pseudo entries and device nodes are not listed.
                _ => continue,
            };
            let modified = header
                .mtime()
                .ok()
                .map(|s| UNIX_EPOCH + Duration::from_secs(s));
            let size = entry.size();
            let link = if kind == EntryKind::Symlink {
                entry.link_name().ok().flatten().map(|l| l.into_owned())
            } else {
                None
            };
            let rec = Entry {
                path: p.clone(),
                kind,
                size: Some(size),
                packed_size: None,
                modified,
                encrypted: false,
            };
            // A tar can contain the same path twice (appended archives); the
            // last occurrence wins, as it would on extraction with GNU tar.
            if let Some(&k) = seen.get(&p) {
                entries[k] = rec;
                ordinals[k] = ordinal;
                modes[k] = header.mode().ok();
                link_targets[k] = link;
            } else {
                seen.insert(p, entries.len());
                entries.push(rec);
                ordinals.push(ordinal);
                modes.push(header.mode().ok());
                link_targets.push(link);
            }
        }
        Ok(Self {
            path: path.to_path_buf(),
            format,
            entries,
            ordinals,
            modes,
            link_targets,
        })
    }

    fn entry_by_ordinal(&self) -> std::collections::HashMap<usize, usize> {
        self.ordinals
            .iter()
            .enumerate()
            .map(|(k, &o)| (o, k))
            .collect()
    }
}

impl Archive for TarArchive {
    fn format(&self) -> Format {
        self.format
    }
    fn path(&self) -> &Path {
        &self.path
    }
    fn entries(&self) -> &[Entry] {
        &self.entries
    }
    fn needs_password(&self) -> bool {
        false
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
        let wanted: std::collections::HashSet<usize> =
            idx.iter().map(|&k| self.ordinals[k]).collect();
        let by_ordinal = self.entry_by_ordinal();

        // Directories first (cheap, no stream needed).
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

        let stream = open_stream(&self.path, self.format)?;
        let mut archive = tar::Archive::new(stream);
        for (ordinal, entry) in archive.entries().map_err(map_io)?.enumerate() {
            cancel.check()?;
            let mut entry = entry.map_err(map_io)?;
            if !wanted.contains(&ordinal) {
                continue;
            }
            let k = by_ordinal[&ordinal];
            let e = &self.entries[k];
            if e.kind == EntryKind::Dir {
                continue;
            }
            let Some(target) = sink.target_for(&e.path) else {
                sink.finish_entry();
                continue;
            };
            sink.emit(&e.path);
            match e.kind {
                EntryKind::Symlink => {
                    if let Some(t) = &self.link_targets[k] {
                        sink.make_symlink(e, &target, t)?;
                    } else {
                        sink.note_unsafe(&e.path.to_string_lossy());
                    }
                }
                _ => {
                    // Hard links are written as a copy of their target when it
                    // was already extracted; otherwise as an empty file.
                    if entry.header().entry_type() == tar::EntryType::Link {
                        let link = entry.link_name().ok().flatten().map(|l| l.into_owned());
                        match sink.prepare(&e.path, &target)? {
                            Plan::Skip => {}
                            Plan::Write(p) => {
                                let src = link
                                    .and_then(|l| safe_path(&l.to_string_lossy()).ok())
                                    .and_then(|l| sink.target_for(&l));
                                match src {
                                    Some(src) if src.is_file() => {
                                        let f = File::open(&src)?;
                                        sink.write_file(e, &p, f, self.modes[k])?;
                                    }
                                    _ => sink.write_file(e, &p, io::empty(), self.modes[k])?,
                                }
                            }
                        }
                    } else {
                        match sink.prepare(&e.path, &target)? {
                            Plan::Skip => {}
                            Plan::Write(p) => sink.write_file(e, &p, &mut entry, self.modes[k])?,
                        }
                    }
                }
            }
            sink.finish_entry();
        }
        Ok(sink.into_report())
    }

    fn test(
        &self,
        _password: Option<&str>,
        progress: ProgressFn<'_>,
        cancel: &Cancel,
    ) -> Result<()> {
        let stream = open_stream(&self.path, self.format)?;
        let mut archive = tar::Archive::new(stream);
        let total = common::total_size(&self.entries, &(0..self.entries.len()).collect::<Vec<_>>());
        let mut done = 0u64;
        let entries_total = self.entries.len();
        for (n, entry) in archive.entries().map_err(map_io)?.enumerate() {
            cancel.check()?;
            let mut entry = entry.map_err(map_io)?;
            progress(&Progress {
                done,
                total,
                current: entry.path().map(|p| p.into_owned()).unwrap_or_default(),
                entries_done: n.min(entries_total),
                entries_total,
            });
            done += io::copy(&mut entry, &mut io::sink()).map_err(map_io)?;
        }
        Ok(())
    }

    fn copy_into(
        &self,
        _builder: &mut crate::write::Builder,
        _map: &dyn Fn(&Entry) -> Option<PathBuf>,
        _password: Option<&str>,
        _progress: ProgressFn<'_>,
        _cancel: &Cancel,
    ) -> Result<()> {
        Err(Error::ReadOnlyFormat(self.format))
    }
}
