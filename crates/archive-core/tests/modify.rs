//! Editing archives: add / remove / rename via `write::modify`, symlinks,
//! volumes, read-only formats.

use std::fs::{self, File};
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use archive_core::write::{self, Builder, Edit, Source, WriteOptions};
use archive_core::{
    open, Cancel, ConflictAction, EntryKind, Error, ExtractOptions, Format, Overwrite,
};

const NOOP: &(dyn Fn(&archive_core::Progress) + Sync) = &|_| {};

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn names(p: &Path, pw: Option<&str>) -> Vec<String> {
    let a = open(p, pw).unwrap();
    let mut v: Vec<String> = a
        .entries()
        .iter()
        .map(|e| e.path.to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

fn make(dir: &Path, format: Format, password: Option<&str>) -> PathBuf {
    let dest = dir.join(format!("a.{}", format.extension()));
    let opts = WriteOptions {
        format,
        password: password.map(str::to_owned),
        ..Default::default()
    };
    let c = Cancel::new();
    let mut b = Builder::create(&dest, &opts).unwrap();
    b.add_dir("docs", None).unwrap();
    b.add_file("docs/one.txt", Cursor::new(b"one"), None, Some(0o644), &c)
        .unwrap();
    b.add_file("docs/two.txt", Cursor::new(b"two"), None, Some(0o644), &c)
        .unwrap();
    b.add_dir("docs/sub", None).unwrap();
    b.add_file(
        "docs/sub/three.txt",
        Cursor::new(b"three"),
        None,
        Some(0o755),
        &c,
    )
    .unwrap();
    b.add_file("top.txt", Cursor::new(vec![b'x'; 50_000]), None, None, &c)
        .unwrap();
    b.add_symlink("docs/link", Path::new("one.txt"), None)
        .unwrap();
    b.finish().unwrap();
    dest
}

fn extract_all(p: &Path, pw: Option<&str>, out: &Path) {
    let a = open(p, pw).unwrap();
    let opts = ExtractOptions {
        password: pw.map(str::to_owned),
        overwrite: Overwrite::Always(ConflictAction::Replace),
        ..Default::default()
    };
    a.extract(&[], out, &opts, NOOP, &Cancel::new()).unwrap();
}

#[test]
fn remove_rename_add_roundtrip() {
    let d = tmp();
    let newfile = d.path().join("new.bin");
    fs::write(&newfile, b"NEW").unwrap();
    let newdir = d.path().join("folder");
    fs::create_dir_all(newdir.join("inner")).unwrap();
    fs::write(newdir.join("inner/deep.txt"), b"deep").unwrap();

    for format in [Format::Zip, Format::SevenZ] {
        for pw in [None, Some("pw")] {
            let sub = d
                .path()
                .join(format!("{}-{}", format.extension(), pw.is_some()));
            fs::create_dir_all(&sub).unwrap();
            let p = make(&sub, format, pw);
            let edit = Edit {
                remove: vec![PathBuf::from("docs/two.txt")],
                rename: vec![
                    (PathBuf::from("docs/sub"), PathBuf::from("docs/renamed")),
                    (PathBuf::from("top.txt"), PathBuf::from("moved/top.txt")),
                ],
                add: vec![
                    Source::from_path(&newfile).unwrap(),
                    Source {
                        disk: newdir.clone(),
                        name: "docs/folder".into(),
                    },
                ],
            };
            let opts = WriteOptions {
                format,
                password: pw.map(str::to_owned),
                ..Default::default()
            };
            let out = write::modify(&p, &edit, &opts, NOOP, &Cancel::new()).unwrap();
            assert_eq!(out, p);
            assert_eq!(
                names(&p, pw),
                vec![
                    "docs",
                    "docs/folder",
                    "docs/folder/inner",
                    "docs/folder/inner/deep.txt",
                    "docs/link",
                    "docs/one.txt",
                    "docs/renamed",
                    "docs/renamed/three.txt",
                    "moved/top.txt",
                    "new.bin"
                ],
                "{format:?} pw={pw:?}"
            );
            let leftovers: Vec<_> = fs::read_dir(&sub)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".part"))
                .collect();
            assert!(leftovers.is_empty(), "{leftovers:?}");

            let x = sub.join("x");
            extract_all(&p, pw, &x);
            assert_eq!(fs::read(x.join("docs/one.txt")).unwrap(), b"one");
            assert_eq!(
                fs::read(x.join("docs/renamed/three.txt")).unwrap(),
                b"three"
            );
            assert_eq!(
                fs::read(x.join("moved/top.txt")).unwrap(),
                vec![b'x'; 50_000]
            );
            assert_eq!(fs::read(x.join("new.bin")).unwrap(), b"NEW");
            assert_eq!(
                fs::read(x.join("docs/folder/inner/deep.txt")).unwrap(),
                b"deep"
            );
            assert!(!x.join("docs/two.txt").exists());
            assert!(fs::symlink_metadata(x.join("docs/link"))
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(fs::read(x.join("docs/link")).unwrap(), b"one");
            let a = open(&p, pw).unwrap();
            a.test(pw, NOOP, &Cancel::new()).unwrap();
            let link = a
                .entries()
                .iter()
                .find(|e| e.path == Path::new("docs/link"))
                .unwrap();
            assert_eq!(link.kind, EntryKind::Symlink);
        }
    }
}

#[test]
fn add_replaces_existing_entry() {
    let d = tmp();
    let p = make(d.path(), Format::Zip, None);
    let replacement = d.path().join("one.txt");
    fs::write(&replacement, b"REPLACED").unwrap();
    let edit = Edit {
        add: vec![Source {
            disk: replacement,
            name: "docs/one.txt".into(),
        }],
        ..Default::default()
    };
    write::modify(&p, &edit, &WriteOptions::default(), NOOP, &Cancel::new()).unwrap();
    let a = open(&p, None).unwrap();
    assert_eq!(
        a.entries()
            .iter()
            .filter(|e| e.path == Path::new("docs/one.txt"))
            .count(),
        1
    );
    let x = d.path().join("x");
    extract_all(&p, None, &x);
    assert_eq!(fs::read(x.join("docs/one.txt")).unwrap(), b"REPLACED");
}

#[test]
fn tar_is_read_only() {
    let d = tmp();
    let p = d.path().join("a.tar");
    {
        let mut b = tar::Builder::new(File::create(&p).unwrap());
        let mut h = tar::Header::new_gnu();
        h.set_size(1);
        h.set_mode(0o644);
        h.set_entry_type(tar::EntryType::Regular);
        b.append_data(&mut h, "f", Cursor::new(b"x")).unwrap();
        b.finish().unwrap();
    }
    let edit = Edit {
        remove: vec![PathBuf::from("f")],
        ..Default::default()
    };
    let r = write::modify(&p, &edit, &WriteOptions::default(), NOOP, &Cancel::new());
    assert!(
        matches!(r, Err(Error::ReadOnlyFormat(Format::Tar))),
        "{r:?}"
    );
    assert!(p.exists());
}

#[test]
fn cancel_leaves_original_intact() {
    let d = tmp();
    let p = make(d.path(), Format::SevenZ, None);
    let before = fs::read(&p).unwrap();
    let cancel = Cancel::new();
    cancel.cancel();
    let edit = Edit {
        remove: vec![PathBuf::from("docs/two.txt")],
        ..Default::default()
    };
    let r = write::modify(
        &p,
        &edit,
        &WriteOptions {
            format: Format::SevenZ,
            ..Default::default()
        },
        NOOP,
        &cancel,
    );
    assert!(matches!(r, Err(Error::Cancelled)));
    assert_eq!(fs::read(&p).unwrap(), before);
    let leftovers: Vec<_> = fs::read_dir(d.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".part"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn volumes_split_and_concatenate_back() {
    let d = tmp();
    let dest = d.path().join("big.7z");
    let opts = WriteOptions {
        format: Format::SevenZ,
        level: 0,
        volume_size: Some(10_000),
        ..Default::default()
    };
    let c = Cancel::new();
    let mut b = Builder::create(&dest, &opts).unwrap();
    b.add_file(
        "blob.bin",
        Cursor::new((0..35_000u32).map(|i| (i % 251) as u8).collect::<Vec<_>>()),
        None,
        None,
        &c,
    )
    .unwrap();
    let first = b.finish().unwrap();
    assert_eq!(first, d.path().join("big.7z.001"));
    assert!(!dest.exists());
    let mut parts: Vec<_> = fs::read_dir(d.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    parts.sort();
    assert!(parts.len() >= 4, "{parts:?}");
    for p in &parts[..parts.len() - 1] {
        assert_eq!(fs::metadata(p).unwrap().len(), 10_000);
    }
    // 7-Zip reads volumes by concatenation; emulate that and open the result.
    let joined = d.path().join("joined.7z");
    let mut out = File::create(&joined).unwrap();
    for p in &parts {
        out.write_all(&fs::read(p).unwrap()).unwrap();
    }
    drop(out);
    let a = open(&joined, None).unwrap();
    assert_eq!(a.entries().len(), 1);
    a.test(None, NOOP, &Cancel::new()).unwrap();
}

#[test]
fn list_sources_walks_and_skips_ds_store() {
    let d = tmp();
    let root = d.path().join("src");
    fs::create_dir_all(root.join("b")).unwrap();
    fs::write(root.join("a.txt"), b"a").unwrap();
    fs::write(root.join(".DS_Store"), b"junk").unwrap();
    fs::write(root.join("b/c.txt"), b"cc").unwrap();
    let items = write::list_sources(&[Source {
        disk: root.clone(),
        name: "renamed".into(),
    }])
    .unwrap();
    let names: Vec<_> = items.iter().map(|i| i.name.clone()).collect();
    assert_eq!(
        names,
        vec!["renamed", "renamed/a.txt", "renamed/b", "renamed/b/c.txt"]
    );
    assert_eq!(items[3].size, 2);
    assert!(write::list_sources(&[Source {
        disk: root,
        name: "../evil".into()
    }])
    .is_err());
}

// ------------------------------------------------------------ disk images (mount-backed)

#[test]
fn mounted_image_lists_and_extracts_like_a_directory() {
    use archive_core::image::MountedImage;
    use archive_core::Archive;
    let d = tmp();
    let vol = d.path().join("vol");
    fs::create_dir_all(vol.join("App.app/Contents")).unwrap();
    fs::write(vol.join("App.app/Contents/Info.plist"), b"<plist/>").unwrap();
    fs::write(vol.join("README.txt"), b"read me").unwrap();
    fs::create_dir_all(vol.join(".Trashes")).unwrap();
    fs::write(vol.join(".DS_Store"), b"x").unwrap();
    fs::create_dir_all(vol.join(".fseventsd")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("README.txt", vol.join("link")).unwrap();

    let img =
        MountedImage::from_directory(Path::new("/fake/image.dmg"), Format::Dmg, &vol).unwrap();
    let mut got: Vec<String> = img
        .entries()
        .iter()
        .map(|e| e.path.to_string_lossy().into_owned())
        .collect();
    got.sort();
    #[cfg(unix)]
    assert_eq!(
        got,
        vec![
            "App.app",
            "App.app/Contents",
            "App.app/Contents/Info.plist",
            "README.txt",
            "link"
        ]
    );
    assert_eq!(img.format(), Format::Dmg);
    assert!(!img.format().is_writable());

    let out = d.path().join("out");
    let rep = img
        .extract(
            &[PathBuf::from("App.app")],
            &out,
            &ExtractOptions::default(),
            NOOP,
            &Cancel::new(),
        )
        .unwrap();
    assert_eq!(
        fs::read(out.join("App.app/Contents/Info.plist")).unwrap(),
        b"<plist/>"
    );
    assert!(!out.join("README.txt").exists());
    assert_eq!(rep.written.len(), 1);
    img.test(None, NOOP, &Cancel::new()).unwrap();
    let r = img.copy_into(
        &mut Builder::create(&d.path().join("x.zip"), &WriteOptions::default()).unwrap(),
        &|_| None,
        None,
        NOOP,
        &Cancel::new(),
    );
    assert!(matches!(r, Err(Error::ReadOnlyFormat(Format::Dmg))));
}

#[test]
fn detects_iso_and_dmg_signatures() {
    let d = tmp();
    let iso = d.path().join("disc.bin");
    let mut bytes = vec![0u8; 0x8001 + 5 + 100];
    bytes[0x8001..0x8006].copy_from_slice(b"CD001");
    fs::write(&iso, &bytes).unwrap();
    assert_eq!(archive_core::detect_format(&iso).unwrap(), Format::Iso);
    let dmg = d.path().join("image.bin");
    let mut bytes = vec![0u8; 4096];
    let n = bytes.len();
    bytes[n - 512..n - 508].copy_from_slice(b"koly");
    fs::write(&dmg, &bytes).unwrap();
    assert_eq!(archive_core::detect_format(&dmg).unwrap(), Format::Dmg);
    let by_ext = d.path().join("thing.dmg");
    fs::write(&by_ext, vec![0u8; 2048]).unwrap();
    assert_eq!(archive_core::detect_format(&by_ext).unwrap(), Format::Dmg);
    // Opening on a non-macOS host cannot mount.
    if !cfg!(target_os = "macos") {
        assert!(matches!(open(&by_ext, None), Err(Error::UnsupportedFormat)));
    }
}
