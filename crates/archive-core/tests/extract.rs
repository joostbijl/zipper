//! Integration tests for listing, extraction, selection, overwrite handling,
//! passwords, cancellation and path safety across all formats.
//!
//! Fixtures are generated at test time: zip/7z through the crate's own
//! writer (and Info-ZIP's `zip` CLI for legacy ZipCrypto when available),
//! tar.* through the `tar` crate plus the stream encoders.

use std::fs::{self, File};
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, UNIX_EPOCH};

use archive_core::write::{Builder, WriteOptions};
use archive_core::{
    detect_format, open, Cancel, ConflictAction, ConflictDecision, Entry, EntryKind, Error,
    ExtractOptions, Format, Overwrite,
};

const NOOP: &(dyn Fn(&archive_core::Progress) + Sync) = &|_| {};

/// The canonical content every fixture carries.
fn tree() -> Vec<(&'static str, Option<&'static [u8]>)> {
    vec![
        ("docs", None),
        ("docs/readme.txt", Some(b"hello\n")),
        ("docs/sub", None),
        (
            "docs/sub/deep.bin",
            Some(&[0u8, 1, 2, 3, 255, 254, 253][..]),
        ),
        ("docs/empty-dir", None),
        ("ünïcödé/файл.txt", Some("tekst\n".as_bytes())),
        ("ünïcödé", None), // a dir listed after its child: must still work
        ("big.dat", Some(&BIG[..])),
        ("empty.txt", Some(b"")),
    ]
}

static BIG: [u8; 300_000] = {
    let mut b = [0u8; 300_000];
    let mut i = 0;
    while i < b.len() {
        b[i] = (i % 251) as u8;
        i += 1;
    }
    b
};

fn mtime(secs: u64) -> std::time::SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

fn build(dest: &Path, opts: &WriteOptions) -> PathBuf {
    let cancel = Cancel::new();
    let mut b = Builder::create(dest, opts).unwrap();
    for (name, data) in tree() {
        match data {
            None => b.add_dir(name, Some(mtime(1_600_000_000))).unwrap(),
            Some(d) => {
                b.add_file(
                    name,
                    Cursor::new(d),
                    Some(mtime(1_600_000_000)),
                    Some(0o644),
                    &cancel,
                )
                .unwrap();
            }
        }
    }
    b.finish().unwrap()
}

fn tar_bytes() -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut b = tar::Builder::new(&mut out);
        for (name, data) in tree() {
            let mut h = tar::Header::new_gnu();
            h.set_mtime(1_600_000_000);
            match data {
                None => {
                    h.set_entry_type(tar::EntryType::Directory);
                    h.set_mode(0o755);
                    h.set_size(0);
                    b.append_data(&mut h, format!("{name}/"), std::io::empty())
                        .unwrap();
                }
                Some(d) => {
                    h.set_entry_type(tar::EntryType::Regular);
                    h.set_mode(0o644);
                    h.set_size(d.len() as u64);
                    b.append_data(&mut h, name, Cursor::new(d)).unwrap();
                }
            }
        }
        // A safe symlink and an escaping one.
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        h.set_mode(0o777);
        b.append_link(&mut h, "docs/link-to-readme", "readme.txt")
            .unwrap();
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        h.set_mode(0o777);
        b.append_link(&mut h, "docs/escape", "../../../../etc/passwd")
            .unwrap();
        b.finish().unwrap();
    }
    out
}

fn write_tar(dir: &Path, format: Format) -> PathBuf {
    let raw = tar_bytes();
    let path = dir.join(format!("fixture.{}", format.extension()));
    let mut f = File::create(&path).unwrap();
    match format {
        Format::Tar => f.write_all(&raw).unwrap(),
        Format::TarGz => {
            let mut e = flate2::write::GzEncoder::new(&mut f, flate2::Compression::default());
            e.write_all(&raw).unwrap();
            e.finish().unwrap();
        }
        Format::TarBz2 => {
            let mut e = bzip2::write::BzEncoder::new(&mut f, bzip2::Compression::default());
            e.write_all(&raw).unwrap();
            e.finish().unwrap();
        }
        Format::TarXz => {
            let mut e = liblzma::write::XzEncoder::new(&mut f, 6);
            e.write_all(&raw).unwrap();
            e.finish().unwrap();
        }
        Format::TarZst => {
            let mut e = zstd::stream::write::Encoder::new(&mut f, 3).unwrap();
            e.write_all(&raw).unwrap();
            e.finish().unwrap();
        }
        _ => unreachable!(),
    }
    path
}

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn names(entries: &[Entry]) -> Vec<String> {
    let mut v: Vec<String> = entries
        .iter()
        .map(|e| e.path.to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

fn assert_tree_extracted(root: &Path) {
    for (name, data) in tree() {
        let p = root.join(name);
        match data {
            None => assert!(p.is_dir(), "missing dir {name}"),
            Some(d) => assert_eq!(fs::read(&p).unwrap(), d, "content of {name}"),
        }
    }
}

// ---------------------------------------------------------------- listing

fn check_listing(path: &Path, expect_format: Format, has_dirs: bool) {
    assert_eq!(detect_format(path).unwrap(), expect_format);
    let a = open(path, None).unwrap();
    assert_eq!(a.format(), expect_format);
    let got = names(a.entries());
    let mut want: Vec<String> = tree()
        .iter()
        .filter(|(_, d)| has_dirs || d.is_some())
        .map(|(n, _)| n.to_string())
        .collect();
    if matches!(
        expect_format,
        Format::Tar | Format::TarGz | Format::TarBz2 | Format::TarXz | Format::TarZst
    ) {
        want.push("docs/link-to-readme".into());
        want.push("docs/escape".into());
    }
    want.sort();
    assert_eq!(got, want);
    let readme = a
        .entries()
        .iter()
        .find(|e| e.path == Path::new("docs/readme.txt"))
        .unwrap();
    assert_eq!(readme.size, Some(6));
    assert_eq!(readme.kind, EntryKind::File);
    assert_eq!(readme.modified, Some(mtime(1_600_000_000)));
    let dir = a
        .entries()
        .iter()
        .find(|e| e.path == Path::new("docs/empty-dir"))
        .unwrap();
    assert!(dir.is_dir());
}

#[test]
fn list_zip() {
    let d = tmp();
    let p = build(&d.path().join("f.zip"), &WriteOptions::default());
    check_listing(&p, Format::Zip, true);
}

#[test]
fn list_7z() {
    let d = tmp();
    let p = build(
        &d.path().join("f.7z"),
        &WriteOptions {
            format: Format::SevenZ,
            ..Default::default()
        },
    );
    check_listing(&p, Format::SevenZ, true);
}

#[test]
fn list_tar_variants() {
    let d = tmp();
    for f in [
        Format::Tar,
        Format::TarGz,
        Format::TarBz2,
        Format::TarXz,
        Format::TarZst,
    ] {
        let p = write_tar(d.path(), f);
        check_listing(&p, f, true);
    }
}

#[test]
fn detect_by_content_not_extension() {
    let d = tmp();
    let p = write_tar(d.path(), Format::TarGz);
    let renamed = d.path().join("mystery.gz");
    fs::rename(&p, &renamed).unwrap();
    assert_eq!(
        detect_format(&renamed).unwrap(),
        Format::TarGz,
        "peeks inside to find the tar header"
    );
    let renamed2 = d.path().join("nothing-at-all");
    fs::rename(&renamed, &renamed2).unwrap();
    assert_eq!(detect_format(&renamed2).unwrap(), Format::TarGz);

    let z = build(&d.path().join("f.zip"), &WriteOptions::default());
    let zr = d.path().join("renamed.dat");
    fs::rename(&z, &zr).unwrap();
    assert_eq!(detect_format(&zr).unwrap(), Format::Zip);

    fs::write(
        d.path().join("garbage.zip"),
        b"this is not an archive at all",
    )
    .unwrap();
    // Extension says zip, content does not; the zip crate gets to decide and fails.
    assert!(open(&d.path().join("garbage.zip"), None).is_err());
    fs::write(d.path().join("plain.txt"), b"hello").unwrap();
    assert!(matches!(
        detect_format(&d.path().join("plain.txt")),
        Err(Error::UnsupportedFormat)
    ));
}

#[test]
fn single_stream_files() {
    let d = tmp();
    let p = d.path().join("notes.txt.gz");
    {
        let mut e =
            flate2::write::GzEncoder::new(File::create(&p).unwrap(), flate2::Compression::fast());
        e.write_all(b"gzipped text").unwrap();
        e.finish().unwrap();
    }
    assert_eq!(detect_format(&p).unwrap(), Format::Gz);
    let a = open(&p, None).unwrap();
    assert_eq!(names(a.entries()), vec!["notes.txt"]);
    assert_eq!(a.entries()[0].size, None);
    let out = d.path().join("out");
    let rep = a
        .extract(&[], &out, &ExtractOptions::default(), NOOP, &Cancel::new())
        .unwrap();
    assert_eq!(fs::read(out.join("notes.txt")).unwrap(), b"gzipped text");
    assert_eq!(rep.written.len(), 1);
    a.test(None, NOOP, &Cancel::new()).unwrap();
}

// ------------------------------------------------------------- extraction

fn all_fixtures(d: &Path) -> Vec<PathBuf> {
    let mut v = vec![
        build(&d.join("f.zip"), &WriteOptions::default()),
        build(
            &d.join("f-stored.zip"),
            &WriteOptions {
                level: 0,
                ..Default::default()
            },
        ),
        build(
            &d.join("f.7z"),
            &WriteOptions {
                format: Format::SevenZ,
                ..Default::default()
            },
        ),
        build(
            &d.join("f-copy.7z"),
            &WriteOptions {
                format: Format::SevenZ,
                level: 0,
                ..Default::default()
            },
        ),
    ];
    for f in [
        Format::Tar,
        Format::TarGz,
        Format::TarBz2,
        Format::TarXz,
        Format::TarZst,
    ] {
        v.push(write_tar(d, f));
    }
    v
}

#[test]
fn extract_everything() {
    let d = tmp();
    for (i, p) in all_fixtures(d.path()).iter().enumerate() {
        let out = d.path().join(format!("out{i}"));
        let a = open(p, None).unwrap();
        let rep = a
            .extract(&[], &out, &ExtractOptions::default(), NOOP, &Cancel::new())
            .unwrap();
        assert_tree_extracted(&out);
        assert!(rep.bytes_written >= BIG.len() as u64, "{p:?}");
        assert!(rep.skipped_existing.is_empty());
        // mtime restored
        let m = fs::metadata(out.join("docs/readme.txt"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(m, mtime(1_600_000_000), "{p:?}");
        a.test(None, NOOP, &Cancel::new()).unwrap();
    }
}

#[test]
fn tar_symlinks_safe_and_unsafe() {
    let d = tmp();
    let p = write_tar(d.path(), Format::TarXz);
    let out = d.path().join("out");
    let a = open(&p, None).unwrap();
    let rep = a
        .extract(&[], &out, &ExtractOptions::default(), NOOP, &Cancel::new())
        .unwrap();
    let link = out.join("docs/link-to-readme");
    assert!(fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read(&link).unwrap(), b"hello\n");
    assert!(
        fs::symlink_metadata(out.join("docs/escape")).is_err(),
        "escaping symlink must not be created"
    );
    assert_eq!(rep.skipped_unsafe, vec![PathBuf::from("docs/escape")]);
}

#[test]
fn extract_into_subfolder() {
    let d = tmp();
    let p = build(&d.path().join("report-2024.zip"), &WriteOptions::default());
    let out = d.path().join("out");
    let a = open(&p, None).unwrap();
    let opts = ExtractOptions {
        into_subfolder: true,
        ..Default::default()
    };
    a.extract(&[], &out, &opts, NOOP, &Cancel::new()).unwrap();
    assert_tree_extracted(&out.join("report-2024"));
}

#[test]
fn extract_selection_preserves_structure() {
    let d = tmp();
    for (i, p) in all_fixtures(d.path()).iter().enumerate() {
        let out = d.path().join(format!("sel{i}"));
        let a = open(p, None).unwrap();
        let sel = vec![PathBuf::from("docs/sub"), PathBuf::from("empty.txt")];
        let rep = a
            .extract(&sel, &out, &ExtractOptions::default(), NOOP, &Cancel::new())
            .unwrap();
        assert_eq!(
            fs::read(out.join("docs/sub/deep.bin")).unwrap(),
            [0u8, 1, 2, 3, 255, 254, 253]
        );
        assert!(out.join("empty.txt").is_file());
        assert!(
            !out.join("docs/readme.txt").exists(),
            "{p:?} extracted unselected file"
        );
        assert!(!out.join("big.dat").exists());
        assert_eq!(rep.written.len(), 2, "{p:?}");
    }
}

#[test]
fn extract_selection_with_strip_prefix() {
    let d = tmp();
    let p = build(
        &d.path().join("f.7z"),
        &WriteOptions {
            format: Format::SevenZ,
            ..Default::default()
        },
    );
    let out = d.path().join("out");
    let a = open(&p, None).unwrap();
    let opts = ExtractOptions {
        strip_prefix: Some(PathBuf::from("docs")),
        ..Default::default()
    };
    a.extract(
        &[PathBuf::from("docs/sub/deep.bin")],
        &out,
        &opts,
        NOOP,
        &Cancel::new(),
    )
    .unwrap();
    assert!(out.join("sub/deep.bin").is_file());
    assert!(!out.join("docs").exists());
}

// --------------------------------------------------------------- overwrite

fn prefill(out: &Path) {
    fs::create_dir_all(out.join("docs")).unwrap();
    fs::write(out.join("docs/readme.txt"), b"OLD").unwrap();
    fs::write(out.join("big.dat"), b"OLD").unwrap();
}

#[test]
fn overwrite_skip_is_default() {
    let d = tmp();
    let p = build(&d.path().join("f.zip"), &WriteOptions::default());
    let out = d.path().join("out");
    prefill(&out);
    let a = open(&p, None).unwrap();
    let rep = a
        .extract(&[], &out, &ExtractOptions::default(), NOOP, &Cancel::new())
        .unwrap();
    assert_eq!(fs::read(out.join("docs/readme.txt")).unwrap(), b"OLD");
    assert_eq!(fs::read(out.join("big.dat")).unwrap(), b"OLD");
    let mut skipped = rep.skipped_existing.clone();
    skipped.sort();
    assert_eq!(
        skipped,
        vec![PathBuf::from("big.dat"), PathBuf::from("docs/readme.txt")]
    );
    assert!(
        out.join("docs/sub/deep.bin").is_file(),
        "non-conflicting files still written"
    );
}

#[test]
fn overwrite_replace_and_keep_both() {
    let d = tmp();
    let p = build(
        &d.path().join("f.7z"),
        &WriteOptions {
            format: Format::SevenZ,
            ..Default::default()
        },
    );
    let out = d.path().join("out");
    prefill(&out);
    let a = open(&p, None).unwrap();
    let opts = ExtractOptions {
        overwrite: Overwrite::Always(ConflictAction::Replace),
        ..Default::default()
    };
    a.extract(&[], &out, &opts, NOOP, &Cancel::new()).unwrap();
    assert_tree_extracted(&out);

    prefill(&out); // readme/big back to OLD
    let opts = ExtractOptions {
        overwrite: Overwrite::Always(ConflictAction::KeepBoth),
        ..Default::default()
    };
    let rep = a
        .extract(
            &[PathBuf::from("docs/readme.txt")],
            &out,
            &opts,
            NOOP,
            &Cancel::new(),
        )
        .unwrap();
    assert_eq!(fs::read(out.join("docs/readme.txt")).unwrap(), b"OLD");
    assert_eq!(fs::read(out.join("docs/readme 2.txt")).unwrap(), b"hello\n");
    assert_eq!(rep.written, vec![out.join("docs/readme 2.txt")]);
    a.extract(
        &[PathBuf::from("docs/readme.txt")],
        &out,
        &opts,
        NOOP,
        &Cancel::new(),
    )
    .unwrap();
    assert!(out.join("docs/readme 3.txt").is_file());
}

#[test]
fn overwrite_ask_with_apply_to_all() {
    let d = tmp();
    let p = build(&d.path().join("f.zip"), &WriteOptions::default());
    let out = d.path().join("out");
    prefill(&out);
    let asked = AtomicUsize::new(0);
    let cb = |_path: &Path| {
        asked.fetch_add(1, Ordering::SeqCst);
        ConflictDecision {
            action: ConflictAction::Replace,
            apply_to_all: true,
        }
    };
    let a = open(&p, None).unwrap();
    let opts = ExtractOptions {
        overwrite: Overwrite::Ask(&cb),
        ..Default::default()
    };
    a.extract(&[], &out, &opts, NOOP, &Cancel::new()).unwrap();
    assert_eq!(
        asked.load(Ordering::SeqCst),
        1,
        "asked once, then applied to all"
    );
    assert_tree_extracted(&out);

    // Without apply_to_all every conflict asks.
    prefill(&out);
    let asked2 = AtomicUsize::new(0);
    let cb2 = |_: &Path| {
        asked2.fetch_add(1, Ordering::SeqCst);
        ConflictDecision {
            action: ConflictAction::Skip,
            apply_to_all: false,
        }
    };
    let opts = ExtractOptions {
        overwrite: Overwrite::Ask(&cb2),
        ..Default::default()
    };
    a.extract(&[], &out, &opts, NOOP, &Cancel::new()).unwrap();
    assert!(asked2.load(Ordering::SeqCst) >= 2);
}

// --------------------------------------------------------------- passwords

#[test]
fn zip_aes_password_flow() {
    let d = tmp();
    let p = build(
        &d.path().join("secret.zip"),
        &WriteOptions {
            password: Some("hunter2".into()),
            ..Default::default()
        },
    );
    let a = open(&p, None).unwrap();
    assert!(a.needs_password());
    assert!(a
        .entries()
        .iter()
        .filter(|e| !e.is_dir())
        .all(|e| e.encrypted));
    let out = d.path().join("out");
    let err = a
        .extract(&[], &out, &ExtractOptions::default(), NOOP, &Cancel::new())
        .unwrap_err();
    assert!(matches!(err, Error::PasswordRequired), "{err:?}");
    let opts = ExtractOptions {
        password: Some("wrong".into()),
        ..Default::default()
    };
    let err = a
        .extract(&[], &out, &opts, NOOP, &Cancel::new())
        .unwrap_err();
    assert!(matches!(err, Error::WrongPassword), "{err:?}");
    let opts = ExtractOptions {
        password: Some("hunter2".into()),
        ..Default::default()
    };
    a.extract(&[], &out, &opts, NOOP, &Cancel::new()).unwrap();
    assert_tree_extracted(&out);
    assert!(matches!(
        a.test(Some("wrong"), NOOP, &Cancel::new()),
        Err(Error::WrongPassword)
    ));
    a.test(Some("hunter2"), NOOP, &Cancel::new()).unwrap();
    // Password given at open time sticks.
    let b = open(&p, Some("hunter2")).unwrap();
    assert!(!b.needs_password());
}

#[test]
fn zip_legacy_zipcrypto_via_infozip() {
    if std::process::Command::new("zip")
        .arg("-v")
        .output()
        .is_err()
    {
        eprintln!("skipping: Info-ZIP `zip` not installed");
        return;
    }
    let d = tmp();
    let src = d.path().join("src");
    fs::create_dir_all(src.join("docs")).unwrap();
    fs::write(src.join("docs/readme.txt"), b"hello\n").unwrap();
    let status = std::process::Command::new("zip")
        .current_dir(&src)
        .args(["-r", "-P", "pw123", "../legacy.zip", "docs"])
        .status()
        .unwrap();
    assert!(status.success());
    let p = d.path().join("legacy.zip");
    let a = open(&p, None).unwrap();
    assert!(a.needs_password());
    let out = d.path().join("out");
    let opts = ExtractOptions {
        password: Some("definitely-wrong".into()),
        ..Default::default()
    };
    let r = a.extract(&[], &out, &opts, NOOP, &Cancel::new());
    // ZipCrypto only has a 1/256 check; a wrong password is *usually* rejected.
    assert!(
        r.is_err()
            || fs::read(out.join("docs/readme.txt"))
                .map(|b| b != b"hello\n")
                .unwrap_or(true)
    );
    let opts = ExtractOptions {
        password: Some("pw123".into()),
        overwrite: Overwrite::Always(ConflictAction::Replace),
        ..Default::default()
    };
    a.extract(&[], &out, &opts, NOOP, &Cancel::new()).unwrap();
    assert_eq!(fs::read(out.join("docs/readme.txt")).unwrap(), b"hello\n");
}

#[test]
fn sevenz_password_encrypted_headers() {
    let d = tmp();
    let p = build(
        &d.path().join("secret.7z"),
        &WriteOptions {
            format: Format::SevenZ,
            password: Some("hunter2".into()),
            encrypt_header: true,
            ..Default::default()
        },
    );
    assert!(matches!(open(&p, None), Err(Error::PasswordRequired)));
    assert!(matches!(open(&p, Some("wrong")), Err(Error::WrongPassword)));
    let a = open(&p, Some("hunter2")).unwrap();
    assert!(!a.needs_password());
    assert!(a
        .entries()
        .iter()
        .filter(|e| !e.is_dir() && e.size != Some(0))
        .all(|e| e.encrypted));
    let out = d.path().join("out");
    a.extract(&[], &out, &ExtractOptions::default(), NOOP, &Cancel::new())
        .unwrap();
    assert_tree_extracted(&out);
    a.test(None, NOOP, &Cancel::new()).unwrap();
}

#[test]
fn sevenz_password_plain_headers() {
    let d = tmp();
    let p = build(
        &d.path().join("secret2.7z"),
        &WriteOptions {
            format: Format::SevenZ,
            password: Some("hunter2".into()),
            encrypt_header: false,
            ..Default::default()
        },
    );
    let a = open(&p, None).unwrap();
    assert!(a.needs_password());
    let out = d.path().join("out");
    assert!(matches!(
        a.extract(&[], &out, &ExtractOptions::default(), NOOP, &Cancel::new()),
        Err(Error::PasswordRequired)
    ));
    let opts = ExtractOptions {
        password: Some("wrong".into()),
        ..Default::default()
    };
    let err = a
        .extract(&[], &out, &opts, NOOP, &Cancel::new())
        .unwrap_err();
    assert!(
        matches!(err, Error::WrongPassword | Error::Corrupt(_)),
        "{err:?}"
    );
    let opts = ExtractOptions {
        password: Some("hunter2".into()),
        overwrite: Overwrite::Always(ConflictAction::Replace),
        ..Default::default()
    };
    a.extract(&[], &out, &opts, NOOP, &Cancel::new()).unwrap();
    assert_tree_extracted(&out);
}

// ------------------------------------------------------------ 7z solid block

#[test]
fn sevenz_solid_block_selection() {
    // Build a solid archive directly with the crate (one block, many files).
    let d = tmp();
    let p = d.path().join("solid.7z");
    {
        let mut w = sevenz_rust2::ArchiveWriter::create(&p).unwrap();
        let mut entries = Vec::new();
        let mut readers = Vec::new();
        for i in 0..5u8 {
            entries.push(sevenz_rust2::ArchiveEntry::new_file(&format!(
                "part{i}.txt"
            )));
            let data = vec![b'a' + i; 10_000];
            readers.push(sevenz_rust2::SourceReader::from(Cursor::new(data)));
        }
        w.push_archive_entries(entries, readers).unwrap();
        w.finish().unwrap();
    }
    let a = open(&p, None).unwrap();
    assert_eq!(a.entries().len(), 5);
    let out = d.path().join("out");
    // Select a file in the middle of the block: earlier files must be drained, not written.
    let rep = a
        .extract(
            &[PathBuf::from("part3.txt")],
            &out,
            &ExtractOptions::default(),
            NOOP,
            &Cancel::new(),
        )
        .unwrap();
    assert_eq!(rep.written, vec![out.join("part3.txt")]);
    assert_eq!(fs::read(out.join("part3.txt")).unwrap(), vec![b'd'; 10_000]);
    assert!(!out.join("part0.txt").exists());
    a.test(None, NOOP, &Cancel::new()).unwrap();
}

// ------------------------------------------------------------- cancellation

#[test]
fn cancel_stops_and_removes_partial_file() {
    let d = tmp();
    for p in all_fixtures(d.path()) {
        let out = d.path().join("out");
        let _ = fs::remove_dir_all(&out);
        let a = open(&p, None).unwrap();
        let cancel = Cancel::new();
        let seen = Mutex::new(Vec::new());
        let progress = |pr: &archive_core::Progress| {
            seen.lock().unwrap().push(pr.current.clone());
            // Cancel as soon as the big file starts.
            if pr.current == Path::new("big.dat") {
                cancel.cancel();
            }
        };
        let r = a.extract(&[], &out, &ExtractOptions::default(), &progress, &cancel);
        assert!(matches!(r, Err(Error::Cancelled)), "{p:?}: {r:?}");
        // big.dat is either absent or complete-sized never partial.
        if let Ok(m) = fs::metadata(out.join("big.dat")) {
            assert_eq!(m.len(), BIG.len() as u64, "{p:?} left a partial file");
        }
        assert!(!seen.lock().unwrap().is_empty());
    }
}

#[test]
fn progress_totals() {
    let d = tmp();
    let p = build(&d.path().join("f.zip"), &WriteOptions::default());
    let a = open(&p, None).unwrap();
    let last = Mutex::new(None);
    let progress = |pr: &archive_core::Progress| {
        *last.lock().unwrap() = Some(pr.clone());
    };
    a.extract(
        &[],
        &d.path().join("out"),
        &ExtractOptions::default(),
        &progress,
        &Cancel::new(),
    )
    .unwrap();
    let last = last.lock().unwrap().clone().unwrap();
    let expected: u64 = tree()
        .iter()
        .filter_map(|(_, d)| d.map(|d| d.len() as u64))
        .sum();
    assert_eq!(last.total, Some(expected));
    assert_eq!(last.done, expected);
}

// --------------------------------------------------------------- zip slip

#[test]
fn zip_slip_entries_are_never_listed_or_written() {
    let d = tmp();
    let p = d.path().join("evil.zip");
    {
        let mut w = zip::ZipWriter::new(File::create(&p).unwrap());
        let o = zip::write::SimpleFileOptions::default();
        w.start_file("../evil.txt", o).unwrap();
        w.write_all(b"pwned").unwrap();
        w.start_file("/abs/evil.txt", o).unwrap();
        w.write_all(b"abs").unwrap();
        w.start_file("ok/../../evil2.txt", o).unwrap();
        w.write_all(b"pwned2").unwrap();
        w.start_file("ok/fine.txt", o).unwrap();
        w.write_all(b"fine").unwrap();
        w.finish().unwrap();
    }
    let a = open(&p, None).unwrap();
    assert_eq!(
        names(a.entries()),
        vec!["abs/evil.txt", "ok/fine.txt"],
        "absolute path is made relative, `..` dropped"
    );
    let out = d.path().join("out/inner");
    a.extract(&[], &out, &ExtractOptions::default(), NOOP, &Cancel::new())
        .unwrap();
    assert!(!d.path().join("out/evil.txt").exists());
    assert!(!d.path().join("evil.txt").exists());
    assert!(!d.path().join("evil2.txt").exists());
    assert!(out.join("ok/fine.txt").is_file());
    assert!(out.join("abs/evil.txt").is_file());

    // Same for tar. The tar crate refuses to write `..` itself, so build the
    // header by hand.
    let t = d.path().join("evil.tar");
    {
        let mut h = [0u8; 512];
        h[..15].copy_from_slice(b"../evil.tar.txt");
        h[100..107].copy_from_slice(b"0000644");
        h[124..135].copy_from_slice(b"00000000005");
        h[136..147].copy_from_slice(b"00000000000");
        h[156] = b'0';
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        let sum: u32 = h
            .iter()
            .enumerate()
            .map(|(i, b)| {
                if (148..156).contains(&i) {
                    32
                } else {
                    *b as u32
                }
            })
            .sum();
        h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        let mut f = File::create(&t).unwrap();
        f.write_all(&h).unwrap();
        let mut body = [0u8; 512];
        body[..5].copy_from_slice(b"pwned");
        f.write_all(&body).unwrap();
        f.write_all(&[0u8; 1024]).unwrap();
    }
    assert_eq!(detect_format(&t).unwrap(), Format::Tar);
    let a = open(&t, None).unwrap();
    assert!(a.entries().is_empty(), "{:?}", names(a.entries()));
    a.extract(
        &[],
        &d.path().join("tarout"),
        &ExtractOptions::default(),
        NOOP,
        &Cancel::new(),
    )
    .unwrap();
    assert!(!d.path().join("evil.tar.txt").exists());
}

#[test]
fn truncated_archives_report_corrupt() {
    let d = tmp();
    for p in all_fixtures(d.path()) {
        let bytes = fs::read(&p).unwrap();
        let cut = d
            .path()
            .join(format!("cut-{}", p.file_name().unwrap().to_string_lossy()));
        fs::write(&cut, &bytes[..bytes.len() / 2]).unwrap();
        let opened = open(&cut, None);
        match opened {
            Err(e) => assert!(
                matches!(
                    e,
                    Error::Corrupt(_) | Error::UnsupportedFormat | Error::Io(_)
                ),
                "{p:?}: {e:?}"
            ),
            Ok(a) => {
                let r = a.test(None, NOOP, &Cancel::new());
                assert!(r.is_err(), "{p:?}: truncated archive passed test()");
                let out = d.path().join("out");
                let r = a.extract(&[], &out, &ExtractOptions::default(), NOOP, &Cancel::new());
                assert!(r.is_err(), "{p:?}: truncated archive extracted cleanly");
                let _ = fs::remove_dir_all(&out);
            }
        }
    }
}

// --------------------------------------------------------------- creation

#[test]
fn create_from_filesystem_roundtrip() {
    let d = tmp();
    let src = d.path().join("src");
    fs::create_dir_all(src.join("folder/nested")).unwrap();
    fs::write(src.join("folder/a.txt"), b"A").unwrap();
    fs::write(src.join("folder/nested/b.txt"), b"BB").unwrap();
    fs::write(src.join("loose.txt"), b"loose").unwrap();
    for format in [Format::Zip, Format::SevenZ] {
        let dest = d.path().join(format!("made.{}", format.extension()));
        let opts = WriteOptions {
            format,
            ..Default::default()
        };
        archive_core::write::create(
            &dest,
            &[src.join("folder"), src.join("loose.txt")],
            &opts,
            NOOP,
            &Cancel::new(),
        )
        .unwrap();
        let a = open(&dest, None).unwrap();
        assert_eq!(a.format(), format);
        assert_eq!(
            names(a.entries()),
            vec![
                "folder",
                "folder/a.txt",
                "folder/nested",
                "folder/nested/b.txt",
                "loose.txt"
            ]
        );
        let out = d.path().join(format!("out-{}", format.extension()));
        a.extract(&[], &out, &ExtractOptions::default(), NOOP, &Cancel::new())
            .unwrap();
        assert_eq!(fs::read(out.join("folder/nested/b.txt")).unwrap(), b"BB");
        assert_eq!(fs::read(out.join("loose.txt")).unwrap(), b"loose");
        a.test(None, NOOP, &Cancel::new()).unwrap();
    }
}

#[test]
fn builder_leaves_no_temp_file_on_drop() {
    let d = tmp();
    let dest = d.path().join("never.zip");
    {
        let mut b = Builder::create(&dest, &WriteOptions::default()).unwrap();
        b.add_file("x", Cursor::new(b"x"), None, None, &Cancel::new())
            .unwrap();
        // dropped without finish()
    }
    let leftovers: Vec<_> = fs::read_dir(d.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn infozip_made_archive_lists_and_extracts() {
    if std::process::Command::new("zip")
        .arg("-v")
        .output()
        .is_err()
    {
        return;
    }
    let d = tmp();
    let src = d.path().join("src");
    fs::create_dir_all(src.join("docs/sub")).unwrap();
    fs::write(src.join("docs/readme.txt"), b"hello\n").unwrap();
    fs::write(src.join("docs/sub/deep.bin"), [1u8, 2, 3]).unwrap();
    assert!(std::process::Command::new("zip")
        .current_dir(&src)
        .args(["-r", "../real.zip", "docs"])
        .status()
        .unwrap()
        .success());
    let a = open(&d.path().join("real.zip"), None).unwrap();
    assert_eq!(
        names(a.entries()),
        vec!["docs", "docs/readme.txt", "docs/sub", "docs/sub/deep.bin"]
    );
    let out = d.path().join("out");
    a.extract(
        &[PathBuf::from("docs/sub")],
        &out,
        &ExtractOptions::default(),
        NOOP,
        &Cancel::new(),
    )
    .unwrap();
    assert_eq!(
        fs::read(out.join("docs/sub/deep.bin")).unwrap(),
        [1u8, 2, 3]
    );
    assert!(!out.join("docs/readme.txt").exists());
}
