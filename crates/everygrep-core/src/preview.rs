//! Bounded preview input and interoperable thumbnail records. No GUI or processes.
use crate::content::{CodePage, open_regular};
use crate::progress::Progress;
use md5::Digest;
use std::io::{self, Cursor, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const TEXT_CAP: usize = 256 * 1024;
pub const INPUT_CAP: usize = 32 * 1024 * 1024;
pub const OUTPUT_CAP: usize = 4 * 1024 * 1024;
pub const PIXEL_CAP: u64 = 25_000_000;
pub const TIME_CAP: Duration = Duration::from_secs(5);

pub fn name(uri: &str) -> String {
    format!(
        "{}.png",
        crate::hash::hex(&md5::Md5::digest(uri.as_bytes()))
    )
}
pub fn cache_root() -> Option<PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".cache")))
        .map(|p| p.join("thumbnails"))
}
/// Files inside the thumbnail directory itself never get thumbnails.
pub fn cacheable(root: &Path, path: &Path) -> bool {
    !path.starts_with(root)
}
pub fn active(progress: &Progress, started: Instant) -> bool {
    !progress.is_cancelled() && started.elapsed() < TIME_CAP
}
/// Opens only a regular non-symlink file. All allocation and reads are capped.
pub fn open(path: &Path) -> Option<std::fs::File> {
    open_regular(path).ok()
}

pub fn read(path: &Path, cap: usize, progress: &Progress) -> Option<Vec<u8>> {
    let mut file = open_regular(path).ok()?;
    let mut bytes = Vec::new();
    let started = Instant::now();
    let mut buffer = [0; 16384];
    while bytes.len() < cap && active(progress, started) {
        let limit = buffer.len().min(cap - bytes.len());
        let n = file.read(&mut buffer[..limit]).ok()?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..n]);
    }
    active(progress, started).then_some(bytes)
}
/// Same BOM / UTF-8 / selected legacy code-page order as content search.
pub fn text(bytes: &[u8], page: CodePage) -> Option<String> {
    let (encoding, skip) = encoding_rs::Encoding::for_bom(bytes).unwrap_or((encoding_rs::UTF_8, 0));
    let data = &bytes[skip..];
    let (decoded, errors) = encoding.decode_without_bom_handling(data);
    let decoded = if errors && skip == 0 {
        // A truncated UTF-8 character at the byte limit is not a legacy file.
        if std::str::from_utf8(data).is_err_and(|e| e.error_len().is_none()) {
            String::from_utf8_lossy(data)
        } else {
            page.encoding().decode_without_bom_handling(data).0
        }
    } else {
        decoded
    };
    if decoded
        .chars()
        .any(|c| c == '\0' || (c.is_control() && !matches!(c, '\n' | '\r' | '\t' | '\u{c}')))
    {
        None
    } else {
        Some(decoded.into_owned())
    }
}
/// Only PNG/JPEG are ever passed to the in-process decoder.
pub fn dimensions(bytes: &[u8], cap: u64) -> Option<(u32, u32)> {
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") && !bytes.starts_with(&[255, 216]) {
        return None;
    }
    let (w, h) = crate::media::image_size(&mut Cursor::new(bytes))?;
    (w > 0 && h > 0 && w.checked_mul(h)? <= cap).then_some((w as u32, h as u32))
}
pub fn orientation(bytes: &[u8]) -> u32 {
    exif::Reader::new()
        .read_from_container(&mut Cursor::new(bytes))
        .ok()
        .and_then(|e| {
            e.get_field(exif::Tag::Orientation, exif::In::PRIMARY)?
                .value
                .get_uint(0)
        })
        .filter(|n| (1..=8).contains(n))
        .unwrap_or(1)
}

fn chunks(bytes: &[u8]) -> Option<Vec<(&[u8], &[u8])>> {
    if bytes.len() > OUTPUT_CAP || !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return None;
    }
    let mut rest = &bytes[8..];
    let mut out = Vec::new();
    while !rest.is_empty() {
        let len = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
        let chunk = rest.get(4..8usize.checked_add(len)?)?;
        let crc = u32::from_be_bytes(rest.get(8 + len..12 + len)?.try_into().ok()?);
        if crc32fast::hash(chunk) != crc {
            return None;
        }
        out.push((&chunk[..4], &chunk[4..]));
        rest = rest.get(12 + len..)?;
        if &chunk[..4] == b"IEND" {
            return rest.is_empty().then_some(out);
        }
    }
    None
}
pub fn valid(bytes: &[u8], uri: &str, mtime: i64, size: u64) -> bool {
    let Some(chunks) = chunks(bytes) else {
        return false;
    };
    let mut tags = std::collections::HashMap::new();
    for (kind, data) in chunks {
        if kind == b"tEXt"
            && let Some(at) = data.iter().position(|b| *b == 0)
        {
            tags.insert(&data[..at], &data[at + 1..]);
        }
    }
    tags.get(b"Thumb::URI".as_slice())
        .is_some_and(|v| *v == uri.as_bytes())
        && tags
            .get(b"Thumb::MTime".as_slice())
            .is_some_and(|v| *v == mtime.to_string().as_bytes())
        && tags
            .get(b"Thumb::Size".as_slice())
            .is_none_or(|v| *v == size.to_string().as_bytes())
}
pub fn lookup(
    root: &Path,
    uri: &str,
    mtime: i64,
    size: u64,
    progress: &Progress,
) -> Option<Vec<u8>> {
    for tier in ["xx-large", "x-large", "large", "normal"] {
        if let Some(bytes) = read(&root.join(tier).join(name(uri)), OUTPUT_CAP + 1, progress)
            && valid(&bytes, uri, mtime, size)
            && dimensions(&bytes, 1024 * 1024).is_some()
        {
            return Some(bytes);
        }
    }
    None
}
fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend((data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend(kind);
    out.extend(data);
    out.extend(crc32fast::hash(&out[start..]).to_be_bytes());
}
/// Small RGBA PNG encoder using stored DEFLATE blocks. Only thumbnail pixels.
pub fn png(w: u32, h: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    if w == 0
        || h == 0
        || u64::from(w) * u64::from(h) > 256 * 256
        || rgba.len() != (w as usize) * (h as usize) * 4
    {
        return None;
    }
    let mut raw = Vec::new();
    for row in rgba.chunks_exact(w as usize * 4) {
        raw.push(0);
        raw.extend(row);
    }
    let mut z = vec![0x78, 0x01];
    for (i, b) in raw.chunks(65535).enumerate() {
        z.push(u8::from((i + 1) * 65535 >= raw.len()));
        z.extend((b.len() as u16).to_le_bytes());
        z.extend((!(b.len() as u16)).to_le_bytes());
        z.extend(b);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for v in raw {
        a = (a + u32::from(v)) % 65521;
        b = (b + a) % 65521;
    }
    z.extend(((b << 16) | a).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend(w.to_be_bytes());
    ihdr.extend(h.to_be_bytes());
    ihdr.extend([8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    Some(out)
}
/// Atomic replacement in a private directory; never follows an existing file.
pub fn save(
    root: &Path,
    tier: &str,
    bytes: &[u8],
    uri: &str,
    mtime: i64,
    size: u64,
) -> io::Result<()> {
    if !matches!(tier, "normal" | "large" | "fail/everygrep") {
        return Err(io::Error::other("invalid thumbnail tier"));
    }
    let parsed = chunks(bytes).ok_or_else(|| io::Error::other("invalid thumbnail"))?;
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    for (kind, data) in parsed {
        if kind == b"tEXt" || kind == b"iTXt" || kind == b"zTXt" {
            continue;
        }
        if kind == b"IEND" {
            for (key, value) in [
                ("Thumb::URI", uri.to_string()),
                ("Thumb::MTime", mtime.to_string()),
                ("Thumb::Size", size.to_string()),
                ("Software", "EveryGrep".into()),
            ] {
                let mut text = key.as_bytes().to_vec();
                text.push(0);
                text.extend(value.as_bytes());
                chunk(&mut out, b"tEXt", &text);
            }
        }
        chunk(&mut out, kind.try_into().expect("chunk kind"), data);
    }
    let dir = root.join(tier);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    // Refuse a symlinked/foreign/world-accessible cache rather than changing it.
    let mut directories = vec![root.to_path_buf(), dir.clone()];
    if tier.starts_with("fail/") {
        directories.push(root.join("fail"));
    }
    for p in directories {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::symlink_metadata(p)?;
        if !m.is_dir() || m.mode() & 0o077 != 0 {
            return Err(io::Error::other("non-private thumbnail directory"));
        }
    }
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp = dir.join(format!(".thumb-{}-{serial}", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    let result = (|| {
        file.write_all(&out)?;
        file.sync_all()?;
        std::fs::rename(&temp, dir.join(name(uri)))
    })();
    let _ = std::fs::remove_file(temp);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uri_digest_and_validation() {
        assert_eq!(
            name("file:///tmp/a.png"),
            "a04bfd79b77efaccf5f6adb271b86f1e.png"
        );
        let dir = std::env::temp_dir().join(format!("everygrep-thumbs-{}", std::process::id()));
        let bytes = png(1, 1, &[1, 2, 3, 255]).unwrap();
        save(&dir, "large", &bytes, "file:///tmp/a.png", 123, 4).unwrap();
        let path = dir.join("large").join(name("file:///tmp/a.png"));
        let cached = std::fs::read(&path).unwrap();
        assert!(valid(&cached, "file:///tmp/a.png", 123, 4));
        assert!(!valid(&cached, "file:///tmp/a.png", 124, 4));
        assert!(!valid(&cached, "file:///tmp/a.png", 123, 5));
        assert!(!valid(&cached, "file:///tmp/b.png", 123, 4));
        use std::os::unix::fs::MetadataExt;
        assert_eq!(std::fs::metadata(path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn decode_text_and_caps() {
        assert_eq!(
            text(b"\xef\xbb\xbfhello", CodePage::default()).as_deref(),
            Some("hello")
        );
        assert_eq!(
            text(b"\xff\xfeA\0B\0", CodePage::default()).as_deref(),
            Some("AB")
        );
        assert_eq!(
            text(b"\xfe\xff\0A\0B", CodePage::default()).as_deref(),
            Some("AB")
        );
        assert!(text(b"binary\0data", CodePage::default()).is_none());
        assert!(dimensions(&png(1, 1, &[0; 4]).unwrap(), 0).is_none());
        let progress = Progress::new();
        progress.cancel();
        assert!(!active(&progress, Instant::now()));
        assert!(!active(&Progress::new(), Instant::now() - TIME_CAP));
        assert!(png(257, 256, &[]).is_none());
        assert_eq!(text(b"\xfe", CodePage::Windows1254).as_deref(), Some("ş"));
        assert_eq!(
            text(b"valid\xe2\x82", CodePage::default()).as_deref(),
            Some("valid\u{fffd}")
        );
    }
    #[test]
    fn capped_reads_reject_links_and_observe_cancellation() {
        let path =
            std::env::temp_dir().join(format!("everygrep-preview-cap-{}", std::process::id()));
        let link = path.with_extension("link");
        std::fs::write(&path, vec![b'x'; TEXT_CAP + 100]).unwrap();
        let p = Progress::new();
        assert_eq!(read(&path, TEXT_CAP, &p).unwrap().len(), TEXT_CAP);
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read(&link, TEXT_CAP, &p).is_none());
        p.cancel();
        assert!(read(&path, TEXT_CAP, &p).is_none());
        std::fs::remove_file(link).unwrap();
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn malformed_records_and_oversized_images_are_refused() {
        let original = png(1, 1, &[1, 2, 3, 4]).unwrap();
        let mut bad = original.clone();
        bad[20] ^= 1;
        assert!(chunks(&bad).is_none());
        assert!(chunks(&vec![0; OUTPUT_CAP + 1]).is_none());
        assert!(chunks(&original[..original.len() - 1]).is_none());
        let mut big = original;
        big[16..20].copy_from_slice(&10000u32.to_be_bytes());
        big[20..24].copy_from_slice(&10000u32.to_be_bytes());
        assert!(dimensions(&big, PIXEL_CAP).is_none());
        let mut optional = b"\x89PNG\r\n\x1a\n".to_vec();
        chunk(&mut optional, b"tEXt", b"Thumb::URI\0file:///a");
        chunk(&mut optional, b"tEXt", b"Thumb::MTime\x00123");
        chunk(&mut optional, b"IEND", &[]);
        assert!(valid(&optional, "file:///a", 123, 999));
        assert!(!valid(b"not a PNG", "file:///a", 123, 999));
        let root = Path::new("/home/u/.cache/thumbnails");
        assert!(!cacheable(root, &root.join("large/0123.png")));
        assert!(cacheable(
            root,
            Path::new("/home/u/.cache/thumbnails-old/a.png")
        ));
    }
}
