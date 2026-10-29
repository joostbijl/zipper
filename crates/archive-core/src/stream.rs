//! Single-stream compressed files (`foo.gz`, `foo.xz`, `foo.bz2`, `foo.zst`):
//! presented as an archive with exactly one entry named after the file.

use std::io;
use std::path::{Path, PathBuf};

use crate::fsutil::{Plan, Sink};
use crate::tar_fmt::open_stream;
use crate::{
    common, Archive, Cancel, Entry, EntryKind, Error, ExtractOptions, ExtractReport, Format,
    Progress, ProgressFn, Result,
};

pub struct StreamArchive {
    path: PathBuf,
    format: Format,
    entries: Vec<Entry>,
}

impl StreamArchive {
    pub fn open(path: &Path, format: Format) -> Result<Self> {
        let name = crate::path::archive_stem(path);
        let modified = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());
        let packed = std::fs::metadata(path).ok().map(|m| m.len());
        let entries = vec![Entry {
            path: PathBuf::from(name),
            kind: EntryKind::File,
            size: None,
            packed_size: packed,
            modified,
            encrypted: false,
        }];
        Ok(Self {
            path: path.to_path_buf(),
            format,
            entries,
        })
    }
}

impl Archive for StreamArchive {
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
        let root = common::target_root(&self.path, dest, opts);
        let sink = Sink::new(root, opts, progress, cancel, None, idx.len())?;
        if idx.is_empty() {
            return Ok(sink.into_report());
        }
        let e = &self.entries[0];
        let Some(target) = sink.target_for(&e.path) else {
            return Ok(sink.into_report());
        };
        sink.emit(&e.path);
        match sink.prepare(&e.path, &target)? {
            Plan::Skip => {}
            Plan::Write(p) => {
                let reader = open_stream(&self.path, self.format)?;
                sink.write_file(e, &p, reader, None)?;
            }
        }
        sink.finish_entry();
        Ok(sink.into_report())
    }

    fn test(
        &self,
        _password: Option<&str>,
        progress: ProgressFn<'_>,
        cancel: &Cancel,
    ) -> Result<()> {
        let mut reader = open_stream(&self.path, self.format)?;
        let mut buf = vec![0u8; 256 * 1024];
        let mut done = 0u64;
        loop {
            cancel.check()?;
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    done += n as u64;
                    if done % (1 << 24) < n as u64 {
                        progress(&Progress {
                            done,
                            total: None,
                            current: self.entries[0].path.clone(),
                            entries_done: 0,
                            entries_total: 1,
                        });
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    return Err(match e.kind() {
                        io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof => {
                            Error::Corrupt(e.to_string())
                        }
                        _ => Error::Io(e),
                    })
                }
            }
        }
        Ok(())
    }
}
