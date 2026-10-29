//! Disk images (ISO, DMG) read through a system mount.
//!
//! Parsing UDIF/HFS+/APFS in Rust is out of scope; macOS already does it.
//! On macOS the image is attached read-only with `hdiutil` at a random,
//! non-browsable mount point and detached when the archive is dropped.
//! The listing and extraction code only sees a directory, which is also how
//! the Linux tests drive it ([`MountedImage::from_directory`]).

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::SystemTime;

use crate::fsutil::{Plan, Sink};
use crate::{
    common, safe_path, Archive, Cancel, Entry, EntryKind, Error, ExtractOptions, ExtractReport,
    Format, Progress, ProgressFn, Result,
};

/// Names that are housekeeping on mounted volumes, not content.
const HIDDEN: &[&str] = &[
    ".Trashes",
    ".fseventsd",
    ".Spotlight-V100",
    ".DS_Store",
    ".background",
    ".VolumeIcon.icns",
    ".HFS+ Private Directory Data\r",
    ".metadata_never_index",
    ".TemporaryItems",
    ".com.apple.timemachine.donotpresent",
    ".DocumentRevisions-V100",
];

type Detach = Box<dyn FnOnce() + Send + Sync>;

pub struct MountedImage {
    path: PathBuf,
    format: Format,
    mount: PathBuf,
    entries: Vec<Entry>,
    /// Entry index → path on the mounted volume.
    disk: Vec<PathBuf>,
    link_targets: Vec<Option<PathBuf>>,
    modes: Vec<Option<u32>>,
    detach: Mutex<Option<Detach>>,
}

impl MountedImage {
    /// Attach with `hdiutil` (macOS). Elsewhere this fails with
    /// [`Error::UnsupportedFormat`].
    pub fn open(path: &Path, format: Format, password: Option<&str>) -> Result<Self> {
        let (mount, detach) = hdiutil_attach(path, password)?;
        Self::from_mount(path, format, mount, Some(detach))
    }

    /// Treat an already-mounted volume (or any directory) as the image's
    /// content. Used by tests; `detach` runs when the archive is dropped.
    pub fn from_directory(path: &Path, format: Format, dir: &Path) -> Result<Self> {
        Self::from_mount(path, format, dir.to_path_buf(), None)
    }

    fn from_mount(
        path: &Path,
        format: Format,
        mount: PathBuf,
        detach: Option<Detach>,
    ) -> Result<Self> {
        let mut entries = Vec::new();
        let mut disk = Vec::new();
        let mut link_targets = Vec::new();
        let mut modes = Vec::new();
        let result = walk(
            &mount,
            &mount,
            &mut entries,
            &mut disk,
            &mut link_targets,
            &mut modes,
        );
        if let Err(e) = result {
            if let Some(d) = detach {
                d();
            }
            return Err(e);
        }
        Ok(Self {
            path: path.to_path_buf(),
            format,
            mount,
            entries,
            disk,
            link_targets,
            modes,
            detach: Mutex::new(detach),
        })
    }

    /// Where the image is mounted (for "Show in Finder" style features).
    pub fn mount_point(&self) -> &Path {
        &self.mount
    }
}

impl Drop for MountedImage {
    fn drop(&mut self) {
        if let Some(d) = self.detach.lock().unwrap().take() {
            d();
        }
    }
}

fn walk(
    root: &Path,
    dir: &Path,
    entries: &mut Vec<Entry>,
    disk: &mut Vec<PathBuf>,
    links: &mut Vec<Option<PathBuf>>,
    modes: &mut Vec<Option<u32>>,
) -> Result<()> {
    let mut children: Vec<_> = match fs::read_dir(dir) {
        Ok(rd) => rd.collect::<io::Result<Vec<_>>>()?,
        // Unreadable subfolders (permissions) are skipped, not fatal.
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    children.sort_by_key(|e| e.file_name());
    for child in children {
        let name = child.file_name().to_string_lossy().into_owned();
        if dir == root && HIDDEN.iter().any(|h| h.trim_end() == name.trim_end()) {
            continue;
        }
        if name == ".DS_Store" {
            continue;
        }
        let full = child.path();
        let rel = full.strip_prefix(root).unwrap_or(&full);
        let Ok(path) = safe_path(&rel.to_string_lossy()) else {
            continue;
        };
        let meta = fs::symlink_metadata(&full)?;
        let ft = meta.file_type();
        let modified = meta.modified().ok();
        let (kind, size, target) = if ft.is_symlink() {
            (EntryKind::Symlink, Some(0), fs::read_link(&full).ok())
        } else if ft.is_dir() {
            (EntryKind::Dir, Some(0), None)
        } else if ft.is_file() {
            (EntryKind::File, Some(meta.len()), None)
        } else {
            continue;
        };
        entries.push(Entry {
            path,
            kind,
            size,
            packed_size: None,
            modified,
            encrypted: false,
        });
        disk.push(full.clone());
        links.push(target);
        modes.push(unix_mode(&meta));
        if kind == EntryKind::Dir {
            walk(root, &full, entries, disk, links, modes)?;
        }
    }
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

impl Archive for MountedImage {
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
        for &k in &idx {
            let e = &self.entries[k];
            let Some(target) = sink.target_for(&e.path) else {
                sink.finish_entry();
                continue;
            };
            sink.emit(&e.path);
            match e.kind {
                EntryKind::Dir => sink.make_dir(e, &target)?,
                EntryKind::Symlink => match &self.link_targets[k] {
                    Some(t) => sink.make_symlink(e, &target, t)?,
                    None => sink.note_unsafe(&e.path.to_string_lossy()),
                },
                EntryKind::File => match sink.prepare(&e.path, &target)? {
                    Plan::Skip => {}
                    Plan::Write(p) => {
                        let f = File::open(&self.disk[k])?;
                        sink.write_file(e, &p, f, self.modes[k])?;
                    }
                },
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
        let total = common::total_size(&self.entries, &(0..self.entries.len()).collect::<Vec<_>>());
        let mut done = 0u64;
        for (n, (e, d)) in self.entries.iter().zip(&self.disk).enumerate() {
            cancel.check()?;
            progress(&Progress {
                done,
                total,
                current: e.path.clone(),
                entries_done: n,
                entries_total: self.entries.len(),
            });
            if e.kind == EntryKind::File {
                let mut f = File::open(d)?;
                done += io::copy(&mut f, &mut io::sink())?;
            }
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

// ------------------------------------------------------------------ hdiutil

/// Attach the image read-only at a random mount point under the temp dir.
/// Returns the mount point and a detach closure.
fn hdiutil_attach(path: &Path, password: Option<&str>) -> Result<(PathBuf, Detach)> {
    if !cfg!(target_os = "macos") {
        return Err(Error::UnsupportedFormat);
    }
    let mount_root = std::env::temp_dir().join("Zipper").join("mounts");
    fs::create_dir_all(&mount_root)?;

    if password.is_none() && is_encrypted(path) {
        return Err(Error::PasswordRequired);
    }

    let mut cmd = Command::new("hdiutil");
    cmd.arg("attach")
        .arg("-readonly")
        .arg("-nobrowse")
        .arg("-noautoopen")
        .arg("-noverify")
        .arg("-plist")
        .arg("-mountrandom")
        .arg(&mount_root);
    if password.is_some() {
        cmd.arg("-stdinpass");
    }
    cmd.arg(path);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            Error::UnsupportedFormat
        } else {
            Error::Io(e)
        }
    })?;
    if let Some(pw) = password {
        use std::io::Write;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(pw.as_bytes());
        }
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        let lower = err.to_ascii_lowercase();
        if lower.contains("authentication") || lower.contains("password") {
            return Err(if password.is_some() {
                Error::WrongPassword
            } else {
                Error::PasswordRequired
            });
        }
        if lower.contains("not recognized")
            || lower.contains("image not recognized")
            || lower.contains("no mountable")
        {
            return Err(Error::Corrupt(format!("hdiutil: {}", err.trim())));
        }
        return Err(Error::Corrupt(format!("hdiutil: {}", err.trim())));
    }
    let plist = String::from_utf8_lossy(&out.stdout).into_owned();
    let mounts = parse_mount_points(&plist);
    let Some(mount) = mounts.first().cloned() else {
        return Err(Error::Corrupt(
            "hdiutil attached the image but reported no mount point (no mountable file system?)"
                .into(),
        ));
    };
    let all = mounts.clone();
    // Detaching takes a moment; never block the thread that drops the archive.
    let detach: Detach = Box::new(move || {
        std::thread::spawn(move || {
            for m in &all {
                let _ = Command::new("hdiutil")
                    .arg("detach")
                    .arg(m)
                    .arg("-force")
                    .arg("-quiet")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        });
    });
    Ok((mount, detach))
}

fn is_encrypted(path: &Path) -> bool {
    let out = Command::new("hdiutil")
        .arg("isencrypted")
        .arg("-plist")
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            // <key>encrypted</key> <true/>
            if let Some(i) = s.find("<key>encrypted</key>") {
                s[i..]
                    .trim_start_matches("<key>encrypted</key>")
                    .trim_start()
                    .starts_with("<true/>")
            } else {
                false
            }
        }
        Err(_) => false,
    }
}

/// `mount-point` strings from `hdiutil attach -plist` output.
pub(crate) fn parse_mount_points(plist: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut rest = plist;
    while let Some(i) = rest.find("<key>mount-point</key>") {
        rest = &rest[i + "<key>mount-point</key>".len()..];
        let Some(s) = rest.find("<string>") else {
            break;
        };
        let Some(e) = rest[s..].find("</string>") else {
            break;
        };
        let value = &rest[s + "<string>".len()..s + e];
        out.push(PathBuf::from(unescape_xml(value)));
        rest = &rest[s + e..];
    }
    out
}

fn unescape_xml(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

#[allow(dead_code)]
fn _time(_: SystemTime) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hdiutil_plist() {
        let plist = r#"<plist><dict><key>system-entities</key><array>
<dict><key>content-hint</key><string>GUID_partition_scheme</string><key>dev-entry</key><string>/dev/disk4</string></dict>
<dict><key>content-hint</key><string>Apple_HFS</string><key>dev-entry</key><string>/dev/disk4s2</string>
<key>mount-point</key><string>/private/tmp/Zipper/mounts/dmg.abc&amp;x</string></dict>
</array></dict></plist>"#;
        assert_eq!(
            parse_mount_points(plist),
            vec![PathBuf::from("/private/tmp/Zipper/mounts/dmg.abc&x")]
        );
    }
}
