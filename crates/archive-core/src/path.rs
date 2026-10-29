//! Entry-path normalisation and the single zip-slip guard used by every format.

use std::path::{Component, Path, PathBuf};

use crate::{Error, Result};

/// Normalise a raw entry name from an archive into a safe, relative path.
///
/// Rules:
/// - `\` is treated as a separator (zip files made on Windows);
/// - leading `/`, `./`, drive letters (`C:`) and UNC prefixes are stripped;
/// - `.` components are dropped; empty components (double slashes) are dropped;
/// - any `..` component makes the path unsafe → [`Error::UnsafePath`];
/// - the result is never empty.
pub fn safe_path(raw: &str) -> Result<PathBuf> {
    let cleaned = raw.replace('\\', "/");
    // Drop a Windows drive letter prefix like "C:" — we never honour it.
    let cleaned = match cleaned.as_bytes() {
        [c, b':', ..] if c.is_ascii_alphabetic() => cleaned[2..].to_string(),
        _ => cleaned,
    };
    let mut out = PathBuf::new();
    for part in cleaned.split('/') {
        match part {
            "" | "." => continue,
            ".." => return Err(Error::UnsafePath(PathBuf::from(raw))),
            p => out.push(p),
        }
    }
    if out.as_os_str().is_empty() {
        return Err(Error::UnsafePath(PathBuf::from(raw)));
    }
    // Belt and braces: the std Component view must agree.
    if out.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(Error::UnsafePath(PathBuf::from(raw)));
    }
    Ok(out)
}

/// Normalise a user-supplied selection path the same way, but leniently
/// (a bad selection just selects nothing).
pub(crate) fn normalise_selection(p: &Path) -> PathBuf {
    safe_path(&p.to_string_lossy()).unwrap_or_else(|_| p.to_path_buf())
}

/// `foo.tar.gz` → `foo`, `bar.7z` → `bar`, `baz` → `baz`.
pub(crate) fn archive_stem(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "archive".to_string());
    let lower = name.to_ascii_lowercase();
    for ext in [
        ".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst", ".tgz", ".tbz2", ".tbz", ".txz", ".tzst",
        ".7z", ".zip", ".tar", ".gz", ".xz", ".bz2", ".zst",
    ] {
        if lower.ends_with(ext) && name.len() > ext.len() {
            return name[..name.len() - ext.len()].to_string();
        }
    }
    name
}

/// Is `target`, resolved from the directory containing `link` (both relative
/// to the extraction root), still inside the root? Used for symlink entries.
pub(crate) fn symlink_target_is_inside(link_rel: &Path, target: &Path) -> bool {
    if target.is_absolute() {
        return false;
    }
    let mut depth: i64 = link_rel.components().count() as i64 - 1; // directory depth of the link
    for c in target.components() {
        match c {
            Component::Normal(_) => depth += 1,
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

/// Finder-style "keep both" name: `report.pdf` → `report 2.pdf`, `report 3.pdf`, …
pub(crate) fn keep_both_name(path: &Path) -> PathBuf {
    let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let file = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (stem, ext) = match file.rfind('.') {
        Some(i) if i > 0 => (file[..i].to_string(), file[i..].to_string()),
        _ => (file.clone(), String::new()),
    };
    for n in 2..10_000 {
        let candidate = parent.join(format!("{stem} {n}{ext}"));
        if !candidate.exists() && std::fs::symlink_metadata(&candidate).is_err() {
            return candidate;
        }
    }
    parent.join(format!("{stem} {}{ext}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_common_cases() {
        assert_eq!(safe_path("a/b/c.txt").unwrap(), PathBuf::from("a/b/c.txt"));
        assert_eq!(safe_path("/a/b").unwrap(), PathBuf::from("a/b"));
        assert_eq!(safe_path("./a//b/").unwrap(), PathBuf::from("a/b"));
        assert_eq!(safe_path("a\\b\\c").unwrap(), PathBuf::from("a/b/c"));
        assert_eq!(safe_path("C:\\x\\y").unwrap(), PathBuf::from("x/y"));
        assert_eq!(
            safe_path("ünïcödé/файл.txt").unwrap(),
            PathBuf::from("ünïcödé/файл.txt")
        );
    }

    #[test]
    fn rejects_zip_slip() {
        assert!(matches!(safe_path("../evil"), Err(Error::UnsafePath(_))));
        assert!(matches!(
            safe_path("a/../../evil"),
            Err(Error::UnsafePath(_))
        ));
        assert!(matches!(safe_path("..\\evil"), Err(Error::UnsafePath(_))));
        assert!(matches!(safe_path(""), Err(Error::UnsafePath(_))));
        assert!(matches!(safe_path("/"), Err(Error::UnsafePath(_))));
        assert!(matches!(safe_path("./"), Err(Error::UnsafePath(_))));
    }

    #[test]
    fn stems() {
        assert_eq!(archive_stem(Path::new("/x/foo.tar.gz")), "foo");
        assert_eq!(archive_stem(Path::new("foo.7z")), "foo");
        assert_eq!(archive_stem(Path::new("foo.TGZ")), "foo");
        assert_eq!(archive_stem(Path::new("foo")), "foo");
        assert_eq!(archive_stem(Path::new(".zip")), ".zip");
    }

    #[test]
    fn symlink_targets() {
        assert!(symlink_target_is_inside(
            Path::new("a/link"),
            Path::new("file")
        ));
        assert!(symlink_target_is_inside(
            Path::new("a/link"),
            Path::new("../other")
        ));
        assert!(!symlink_target_is_inside(
            Path::new("a/link"),
            Path::new("../../out")
        ));
        assert!(!symlink_target_is_inside(
            Path::new("link"),
            Path::new("../out")
        ));
        assert!(!symlink_target_is_inside(
            Path::new("link"),
            Path::new("/etc/passwd")
        ));
    }
}
