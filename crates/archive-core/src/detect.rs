//! Format detection: magic bytes first, file extension as fallback / tie-breaker.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use crate::{Error, Format, Result};

const SEVENZ_MAGIC: &[u8] = &[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];
const XZ_MAGIC: &[u8] = &[0xFD, 0x37, 0x7A, 0x58, 0x5A, 0x00];
const ZSTD_MAGIC: &[u8] = &[0x28, 0xB5, 0x2F, 0xFD];
const GZ_MAGIC: &[u8] = &[0x1F, 0x8B];
const BZ2_MAGIC: &[u8] = b"BZh";

/// Compression wrapper detected from magic bytes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Outer {
    Gz,
    Bz2,
    Xz,
    Zst,
}

pub fn detect_format(path: &Path) -> Result<Format> {
    let mut file = File::open(path)?;
    let mut head = [0u8; 512];
    let n = read_fully(&mut file, &mut head)?;
    let head = &head[..n];

    if head.starts_with(SEVENZ_MAGIC) {
        return Ok(Format::SevenZ);
    }
    if head.starts_with(b"PK\x03\x04")
        || head.starts_with(b"PK\x05\x06")
        || head.starts_with(b"PK\x07\x08")
    {
        return Ok(Format::Zip);
    }
    if is_tar_header(head) {
        return Ok(Format::Tar);
    }
    if let Some(f) = detect_image(&mut file, path)? {
        return Ok(f);
    }

    let outer = if head.starts_with(GZ_MAGIC) {
        Some(Outer::Gz)
    } else if head.starts_with(BZ2_MAGIC) {
        Some(Outer::Bz2)
    } else if head.starts_with(XZ_MAGIC) {
        Some(Outer::Xz)
    } else if head.starts_with(ZSTD_MAGIC) {
        Some(Outer::Zst)
    } else {
        None
    };

    if let Some(outer) = outer {
        // Content wins: a `.gz` that holds a tar is shown as a tarball. The
        // extension only decides when the first block cannot be inspected.
        let is_tar = inner_looks_like_tar(path, outer)
            .unwrap_or_else(|| tar_by_extension(path).unwrap_or(false));
        return Ok(match (outer, is_tar) {
            (Outer::Gz, true) => Format::TarGz,
            (Outer::Gz, false) => Format::Gz,
            (Outer::Bz2, true) => Format::TarBz2,
            (Outer::Bz2, false) => Format::Bz2,
            (Outer::Xz, true) => Format::TarXz,
            (Outer::Xz, false) => Format::Xz,
            (Outer::Zst, true) => Format::TarZst,
            (Outer::Zst, false) => Format::Zst,
        });
    }

    // No recognisable magic. A zip whose central directory sits behind a
    // self-extractor stub is still a zip; let the zip crate find it, but only
    // when the extension says so.
    match extension_hint(path) {
        Some(Format::Zip) => Ok(Format::Zip),
        Some(Format::Tar) => Ok(Format::Tar),
        _ => Err(Error::UnsupportedFormat),
    }
}

/// ISO 9660 ("CD001" in a volume descriptor at 32 KiB) and Apple UDIF disk
/// images ("koly" trailer in the last 512 bytes). Both are only readable
/// through a system mount, so detection is enough here.
fn detect_image(file: &mut File, path: &Path) -> Result<Option<Format>> {
    use std::io::{Seek, SeekFrom};
    let len = file.metadata()?.len();
    if len >= 512 {
        file.seek(SeekFrom::End(-512))?;
        let mut trailer = [0u8; 4];
        if read_fully(file, &mut trailer)? == 4 && &trailer == b"koly" {
            return Ok(Some(Format::Dmg));
        }
    }
    for offset in [0x8001u64, 0x8801, 0x9001] {
        if len < offset + 5 {
            break;
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut sig = [0u8; 5];
        if read_fully(file, &mut sig)? == 5 && &sig == b"CD001" {
            return Ok(Some(Format::Iso));
        }
    }
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name.ends_with(".dmg") || name.ends_with(".sparseimage") || name.ends_with(".sparsebundle") {
        return Ok(Some(Format::Dmg));
    }
    if name.ends_with(".iso") || name.ends_with(".cdr") || name.ends_with(".toast") {
        return Ok(Some(Format::Iso));
    }
    Ok(None)
}

fn read_fully<R: Read>(r: &mut R, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// POSIX ustar or GNU tar magic at offset 257; also accept an old-style
/// (pre-POSIX) header whose checksum verifies.
fn is_tar_header(head: &[u8]) -> bool {
    if head.len() < 512 {
        return false;
    }
    if &head[257..262] == b"ustar" {
        return true;
    }
    // Old V7 tar: validate the header checksum field.
    let stored = std::str::from_utf8(&head[148..156])
        .ok()
        .and_then(|s| u64::from_str_radix(s.trim_matches(|c| c == ' ' || c == '\0'), 8).ok());
    let Some(stored) = stored else { return false };
    if stored == 0 {
        return false;
    }
    let mut sum: u64 = 0;
    for (i, b) in head[..512].iter().enumerate() {
        sum += if (148..156).contains(&i) {
            32
        } else {
            *b as u64
        };
    }
    sum == stored && head[..100].iter().any(|&b| b != 0)
}

/// `Some(true)` for `.tar.gz`/`.tgz`…, `Some(false)` for a plain `.gz`…,
/// `None` when the extension says nothing useful.
fn tar_by_extension(path: &Path) -> Option<bool> {
    let name = path.file_name()?.to_string_lossy().to_ascii_lowercase();
    if name.ends_with(".tar.gz")
        || name.ends_with(".tar.bz2")
        || name.ends_with(".tar.xz")
        || name.ends_with(".tar.zst")
        || name.ends_with(".tgz")
        || name.ends_with(".tbz2")
        || name.ends_with(".tbz")
        || name.ends_with(".txz")
        || name.ends_with(".tzst")
    {
        return Some(true);
    }
    if name.ends_with(".gz")
        || name.ends_with(".bz2")
        || name.ends_with(".xz")
        || name.ends_with(".zst")
    {
        return Some(false);
    }
    None
}

fn extension_hint(path: &Path) -> Option<Format> {
    let name = path.file_name()?.to_string_lossy().to_ascii_lowercase();
    if name.ends_with(".zip") || name.ends_with(".jar") || name.ends_with(".cbz") {
        Some(Format::Zip)
    } else if name.ends_with(".tar") {
        Some(Format::Tar)
    } else {
        None
    }
}

/// Decompress the first 512 bytes and look for a tar header.
/// `None` when the stream could not be inspected at all.
fn inner_looks_like_tar(path: &Path, outer: Outer) -> Option<bool> {
    let Ok(file) = File::open(path) else {
        return None;
    };
    let reader = BufReader::new(file);
    let mut decoder: Box<dyn Read> = match outer {
        Outer::Gz => Box::new(flate2::read::MultiGzDecoder::new(reader)),
        Outer::Bz2 => Box::new(bzip2::read::MultiBzDecoder::new(reader)),
        Outer::Xz => Box::new(liblzma::read::XzDecoder::new_multi_decoder(reader)),
        Outer::Zst => match zstd::stream::read::Decoder::new(reader) {
            Ok(d) => Box::new(d),
            Err(_) => return None,
        },
    };
    let mut head = [0u8; 512];
    match read_fully(&mut decoder, &mut head) {
        Ok(512) => Some(is_tar_header(&head)),
        Ok(_) => Some(false),
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tar_header_detection() {
        let mut h = [0u8; 512];
        h[..4].copy_from_slice(b"file");
        h[257..262].copy_from_slice(b"ustar");
        assert!(is_tar_header(&h));
        assert!(!is_tar_header(&[0u8; 512]));
        assert!(!is_tar_header(&[0u8; 100]));
    }
}
