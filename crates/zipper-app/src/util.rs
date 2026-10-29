//! Small helpers: string conversion, size/date formatting, paths.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use objc2::rc::Retained;
use objc2_foundation::{
    NSByteCountFormatter, NSByteCountFormatterCountStyle, NSDate, NSDateFormatter,
    NSDateFormatterStyle, NSString, NSURL,
};

pub fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

pub fn path_to_url(p: &Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath(&ns(&p.to_string_lossy()))
}

pub fn url_to_path(u: &NSURL) -> Option<PathBuf> {
    u.path().map(|s| PathBuf::from(s.to_string()))
}

/// "1.2 MB" style, as Finder shows it.
pub fn format_size(bytes: Option<u64>) -> String {
    match bytes {
        None => "—".to_string(),
        Some(b) => unsafe {
            NSByteCountFormatter::stringFromByteCount_countStyle(
                b as i64,
                NSByteCountFormatterCountStyle::File,
            )
            .to_string()
        },
    }
}

pub fn format_date(t: Option<SystemTime>) -> String {
    let Some(t) = t else { return String::new() };
    let secs = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    unsafe {
        let date = NSDate::dateWithTimeIntervalSince1970(secs);
        let f = NSDateFormatter::new();
        f.setDateStyle(NSDateFormatterStyle::MediumStyle);
        f.setTimeStyle(NSDateFormatterStyle::ShortStyle);
        f.stringFromDate(&date).to_string()
    }
}

/// `$TMPDIR/Zipper/<stem>-<hash>/` for an archive; stable for the same path.
pub fn temp_dir_for(archive: &Path) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    archive.hash(&mut h);
    let stem = archive
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "archive".into());
    std::env::temp_dir()
        .join("Zipper")
        .join(format!("{stem}-{:08x}", h.finish() as u32))
}

/// Where "Extract to…" should start (SPEC §5): archives living in hidden
/// system or temp locations (Mail/Outlook attachments) default to ~/Downloads.
pub fn default_extract_dir(archive: &Path) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let tmp = std::env::temp_dir();
    let hidden = archive.starts_with(&tmp)
        || archive.starts_with("/private/var")
        || archive.starts_with("/var/folders")
        || home
            .as_ref()
            .map(|h| archive.starts_with(h.join("Library")))
            .unwrap_or(false);
    if hidden {
        if let Some(h) = home {
            let dl = h.join("Downloads");
            if dl.is_dir() {
                return dl;
            }
            return h;
        }
    }
    archive
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_locations_go_to_downloads() {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap();
        let p = home.join("Library/Containers/com.microsoft.Outlook/Data/x.zip");
        let d = default_extract_dir(&p);
        assert!(d == home.join("Downloads") || d == home);
        let p2 = home.join("Desktop/x.zip");
        assert_eq!(default_extract_dir(&p2), home.join("Desktop"));
    }

    #[test]
    fn temp_dir_is_stable() {
        let a = temp_dir_for(Path::new("/x/y/foo.zip"));
        let b = temp_dir_for(Path::new("/x/y/foo.zip"));
        assert_eq!(a, b);
        assert!(a.ends_with(format!(
            "foo-{}",
            a.file_name()
                .unwrap()
                .to_string_lossy()
                .split('-')
                .last()
                .unwrap()
        )));
    }
}
