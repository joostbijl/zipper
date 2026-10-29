//! 7z support via `sevenz-rust2`.
//!
//! The archive header is parsed once at open time. Extraction decodes only the
//! blocks that contain selected files; inside a (solid) block, unselected
//! files are drained because block data is sequential.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use sevenz_rust2::{Archive as SzArchive, ArchiveEntry, BlockDecoder, EncoderMethod, Password};

use crate::fsutil::{Plan, Sink};
use crate::{
    common, safe_path, Archive, Cancel, Entry, EntryKind, Error, ExtractOptions, ExtractReport,
    Format, Progress, ProgressFn, Result,
};

pub struct SevenZArchive {
    path: PathBuf,
    meta: SzArchive,
    entries: Vec<Entry>,
    /// Entry index → file index in `meta.files`.
    file_index: Vec<usize>,
    any_encrypted: bool,
    password: Option<String>,
}

fn nt_to_system_time(e: &ArchiveEntry) -> Option<SystemTime> {
    if e.has_last_modified_date {
        Some(SystemTime::from(e.last_modified_date))
    } else {
        None
    }
}

fn is_encrypted_block(archive: &SzArchive, block_index: usize) -> bool {
    archive.blocks[block_index]
        .coders
        .iter()
        .any(|c| c.encoder_method_id() == EncoderMethod::ID_AES256_SHA256)
}

/// Windows attribute bits: 0x10 = directory. Unix mode lives in the high 16 bits
/// when bit 0x8000 is set (p7zip convention).
fn unix_mode(e: &ArchiveEntry) -> Option<u32> {
    if e.has_windows_attributes && e.windows_attributes & 0x8000 != 0 {
        Some(e.windows_attributes >> 16)
    } else {
        None
    }
}

fn is_symlink(e: &ArchiveEntry) -> bool {
    unix_mode(e)
        .map(|m| m & 0o170000 == 0o120000)
        .unwrap_or(false)
}

impl SevenZArchive {
    pub fn open(path: &Path, password: Option<&str>) -> Result<Self> {
        let pw = password.map(Password::new).unwrap_or_else(Password::empty);
        let mut file = BufReader::new(File::open(path)?);
        let meta = match SzArchive::read(&mut file, &pw) {
            Ok(a) => a,
            Err(sevenz_rust2::Error::PasswordRequired) => return Err(Error::PasswordRequired),
            Err(sevenz_rust2::Error::BadSignature(_)) => return Err(Error::UnsupportedFormat),
            Err(e) if password.is_some() => {
                // A header that fails to parse with a supplied password is
                // almost always a wrong password (encrypted headers).
                return Err(match e {
                    sevenz_rust2::Error::Io(e, _) => Error::Io(e),
                    _ => Error::WrongPassword,
                });
            }
            Err(e) => return Err(e.into()),
        };

        let mut entries = Vec::with_capacity(meta.files.len());
        let mut file_index = Vec::with_capacity(meta.files.len());
        let mut any_encrypted = false;
        for (i, f) in meta.files.iter().enumerate() {
            if f.is_anti_item {
                continue;
            }
            let Ok(p) = safe_path(&f.name) else { continue };
            let block = meta.stream_map.file_block_index.get(i).copied().flatten();
            let encrypted = block.map(|b| is_encrypted_block(&meta, b)).unwrap_or(false);
            any_encrypted |= encrypted;
            let kind = if f.is_directory {
                EntryKind::Dir
            } else if is_symlink(f) {
                EntryKind::Symlink
            } else {
                EntryKind::File
            };
            entries.push(Entry {
                path: p,
                kind,
                size: Some(f.size),
                packed_size: block.map(|b| {
                    // Packed size of the block, divided only if it holds a single file.
                    let nfiles = meta
                        .stream_map
                        .block_first_file_index
                        .get(b + 1)
                        .copied()
                        .unwrap_or(meta.files.len())
                        .saturating_sub(meta.stream_map.block_first_file_index[b]);
                    let total = meta
                        .pack_sizes()
                        .get(meta.stream_map.block_first_pack_stream_index()[b])
                        .copied()
                        .unwrap_or(0);
                    if nfiles <= 1 {
                        total
                    } else {
                        f.compressed_size.min(total)
                    }
                }),
                modified: nt_to_system_time(f),
                encrypted,
            });
            file_index.push(i);
        }
        // If the headers were encrypted the whole archive is; the supplied
        // password worked, so remember it.
        Ok(Self {
            path: path.to_path_buf(),
            meta,
            entries,
            file_index,
            any_encrypted,
            password: password.map(str::to_owned),
        })
    }

    fn effective_password(&self, opts: Option<&str>) -> Password {
        opts.or(self.password.as_deref())
            .map(Password::new)
            .unwrap_or_else(Password::empty)
    }

    /// Run `each` over every file in every block that contains at least one
    /// of `wanted` (by file index). Files not in `wanted` are drained.
    fn decode_blocks<F>(
        &self,
        wanted: &[usize],
        password: &Password,
        cancel: &Cancel,
        mut each: F,
    ) -> Result<()>
    where
        F: FnMut(usize, &ArchiveEntry, &mut dyn Read) -> Result<()>,
    {
        // Group wanted file indices by block.
        let mut by_block: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        let mut streamless: Vec<usize> = Vec::new();
        for &fi in wanted {
            match self.meta.stream_map.file_block_index[fi] {
                Some(b) => by_block.entry(b).or_default().push(fi),
                None => streamless.push(fi),
            }
        }
        let mut source = BufReader::new(File::open(&self.path)?);
        let threads = std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1);

        for (block, files) in &by_block {
            cancel.check()?;
            let encrypted = is_encrypted_block(&self.meta, *block);
            if encrypted && password.is_empty() {
                return Err(Error::PasswordRequired);
            }
            let decoder = BlockDecoder::new(threads, *block, &self.meta, password, &mut source);
            let mut current = self.meta.stream_map.block_first_file_index[*block];
            let mut err: Option<Error> = None;
            let res =
                decoder.for_each_entries(&mut |entry: &ArchiveEntry, reader: &mut dyn Read| {
                    let fi = current;
                    current += 1;
                    if cancel.is_cancelled() {
                        err = Some(Error::Cancelled);
                        return Ok(false);
                    }
                    if files.contains(&fi) {
                        match each(fi, entry, reader) {
                            Ok(()) => Ok(true),
                            Err(e) => {
                                err = Some(e);
                                Ok(false)
                            }
                        }
                    } else {
                        // Drain so the block stream stays in sync.
                        match io::copy(reader, &mut io::sink()) {
                            Ok(_) => Ok(true),
                            Err(e) => Err(sevenz_rust2::Error::Io(e, "".into())),
                        }
                    }
                });
            if let Some(e) = err {
                return Err(e);
            }
            match res {
                Ok(_) => {}
                Err(e) => return Err(map_sz(e, encrypted)),
            }
        }
        for fi in streamless {
            cancel.check()?;
            let entry = &self.meta.files[fi];
            let mut empty: &[u8] = &[];
            each(fi, entry, &mut empty)?;
        }
        Ok(())
    }
}

fn map_sz(e: sevenz_rust2::Error, encrypted: bool) -> Error {
    match e {
        sevenz_rust2::Error::MaybeBadPassword(_) => Error::WrongPassword,
        sevenz_rust2::Error::ChecksumVerificationFailed if encrypted => Error::WrongPassword,
        sevenz_rust2::Error::Io(e, _) if encrypted && e.kind() == io::ErrorKind::InvalidData => {
            Error::WrongPassword
        }
        other => other.into(),
    }
}

impl Archive for SevenZArchive {
    fn format(&self) -> Format {
        Format::SevenZ
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

        // Directories first.
        let mut wanted_files = Vec::new();
        for &k in &idx {
            let e = &self.entries[k];
            if e.kind == EntryKind::Dir {
                if let Some(target) = sink.target_for(&e.path) {
                    sink.make_dir(e, &target)?;
                }
                sink.finish_entry();
            } else {
                wanted_files.push(self.file_index[k]);
            }
        }
        // Map file index → entry index for the callback.
        let mut entry_of_file = vec![usize::MAX; self.meta.files.len()];
        for (k, &fi) in self.file_index.iter().enumerate() {
            entry_of_file[fi] = k;
        }

        self.decode_blocks(&wanted_files, &password, cancel, |fi, sz_entry, reader| {
            let e = &self.entries[entry_of_file[fi]];
            let Some(target) = sink.target_for(&e.path) else {
                sink.finish_entry();
                return Ok(());
            };
            sink.emit(&e.path);
            match e.kind {
                EntryKind::Symlink => {
                    let mut buf = Vec::new();
                    reader.read_to_end(&mut buf)?;
                    let link_target = PathBuf::from(String::from_utf8_lossy(&buf).into_owned());
                    sink.make_symlink(e, &target, &link_target)?;
                }
                _ => match sink.prepare(&e.path, &target)? {
                    Plan::Skip => {
                        io::copy(reader, &mut io::sink())?;
                    }
                    Plan::Write(p) => sink.write_file(e, &p, reader, unix_mode(sz_entry))?,
                },
            }
            sink.finish_entry();
            Ok(())
        })?;
        Ok(sink.into_report())
    }

    fn test(
        &self,
        password: Option<&str>,
        progress: ProgressFn<'_>,
        cancel: &Cancel,
    ) -> Result<()> {
        let password = self.effective_password(password);
        let all: Vec<usize> = self.file_index.clone();
        let total = common::total_size(&self.entries, &(0..self.entries.len()).collect::<Vec<_>>());
        let mut done = 0u64;
        let mut n = 0usize;
        let entries_total = self.entries.len();
        let mut entry_of_file = vec![usize::MAX; self.meta.files.len()];
        for (k, &fi) in self.file_index.iter().enumerate() {
            entry_of_file[fi] = k;
        }
        self.decode_blocks(&all, &password, cancel, |fi, _sz, reader| {
            let e = &self.entries[entry_of_file[fi]];
            progress(&Progress {
                done,
                total,
                current: e.path.clone(),
                entries_done: n,
                entries_total,
            });
            // Reading to EOF triggers the CRC check inside the crate.
            done += io::copy(reader, &mut io::sink()).map_err(|err| match err.kind() {
                io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof => {
                    Error::Corrupt(err.to_string())
                }
                _ => Error::Io(err),
            })?;
            n += 1;
            Ok(())
        })
    }

    fn copy_into(
        &self,
        builder: &mut crate::write::Builder,
        map: &dyn Fn(&Entry) -> Option<PathBuf>,
        password: Option<&str>,
        progress: ProgressFn<'_>,
        cancel: &Cancel,
    ) -> Result<()> {
        let password = self.effective_password(password);
        let entries_total = self.entries.len();
        let mut entry_of_file = vec![usize::MAX; self.meta.files.len()];
        for (k, &fi) in self.file_index.iter().enumerate() {
            entry_of_file[fi] = k;
        }
        // Directories and streamless entries first, then everything with data.
        let mut wanted = Vec::new();
        let mut n = 0usize;
        for (k, e) in self.entries.iter().enumerate() {
            let Some(new_path) = map(e) else { continue };
            if e.kind == EntryKind::Dir {
                builder.add_dir(&new_path.to_string_lossy(), e.modified)?;
                n += 1;
            } else {
                wanted.push(self.file_index[k]);
            }
        }
        let mut done = 0u64;
        self.decode_blocks(&wanted, &password, cancel, |fi, sz_entry, reader| {
            let e = &self.entries[entry_of_file[fi]];
            let Some(new_path) = map(e) else {
                io::copy(reader, &mut io::sink())?;
                return Ok(());
            };
            progress(&Progress {
                done,
                total: None,
                current: new_path.clone(),
                entries_done: n,
                entries_total,
            });
            n += 1;
            let name = new_path.to_string_lossy().into_owned();
            match e.kind {
                EntryKind::Symlink => {
                    let mut buf = Vec::new();
                    reader.read_to_end(&mut buf)?;
                    let target = PathBuf::from(String::from_utf8_lossy(&buf).into_owned());
                    builder.add_symlink(&name, &target, e.modified)?;
                }
                _ => {
                    done +=
                        builder.add_file(&name, reader, e.modified, unix_mode(sz_entry), cancel)?;
                }
            }
            Ok(())
        })
    }
}
