//! Bounded metadata readers. No content decoder or cover image is loaded.
use crate::progress::Progress;
use crate::property::{Property, Value};
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{Duration, Instant};

pub const BYTE_CAP: u64 = 8 * 1024 * 1024;
pub const TIME_CAP: Duration = Duration::from_millis(250);

struct Budget<'a> {
    file: File,
    remaining: u64,
    started: Instant,
    progress: &'a Progress,
}
impl Budget<'_> {
    fn check(&self) -> io::Result<()> {
        if self.progress.is_cancelled() || self.started.elapsed() >= TIME_CAP || self.remaining == 0
        {
            Err(io::Error::new(io::ErrorKind::TimedOut, "metadata budget"))
        } else {
            Ok(())
        }
    }
}
impl Read for Budget<'_> {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.check()?;
        let len = b.len().min(self.remaining as usize);
        let n = self.file.read(&mut b[..len])?;
        self.remaining -= n as u64;
        Ok(n)
    }
}
impl Seek for Budget<'_> {
    fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
        self.check()?;
        self.file.seek(p)
    }
}

/// Only regular, non-followed files. All readers share one byte/time budget.
/// The files are untrusted: a parser that panics on a malformed file gives
/// no values instead of ending the reading thread.
pub fn read(path: &Path, wanted: &[Property], progress: &Progress) -> Vec<(Property, Value)> {
    guarded(|| read_unguarded(path, wanted, progress))
}

/// The answer of `f`, or the default when it panics.
fn guarded<T: Default>(f: impl FnOnce() -> T) -> T {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_default()
}

fn read_unguarded(path: &Path, wanted: &[Property], progress: &Progress) -> Vec<(Property, Value)> {
    use rustix::fs::{Mode, OFlags};
    let Ok(fd) = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    ) else {
        return Vec::new();
    };
    let file = File::from(fd);
    let Ok(meta) = file.metadata() else {
        return Vec::new();
    };
    if !meta.is_file() {
        return Vec::new();
    }
    let mut reader = Budget {
        file,
        remaining: BYTE_CAP,
        started: Instant::now(),
        progress,
    };
    let mut values = Vec::new();
    let has = |p| wanted.contains(&p);
    let ext = path
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    let kind = crate::kind::classify(
        path.file_name().unwrap_or_default().as_encoded_bytes(),
        false,
    );
    if matches!(kind, crate::kind::FileKind::Audio)
        && let Ok(probe) = lofty::probe::Probe::new(BufReader::new(&mut reader)).guess_file_type()
        && let Ok(tagged) = probe
            .options(
                lofty::config::ParseOptions::new()
                    .read_cover_art(false)
                    .max_junk_bytes(1024),
            )
            .read()
    {
        use lofty::prelude::*;
        let props = tagged.properties();
        if has(Property::Length) {
            values.push((
                Property::Length,
                Value::Number(props.duration().as_millis().min(u64::MAX as u128) as u64),
            ));
        }
        if has(Property::BitRate)
            && let Some(n) = props.audio_bitrate()
        {
            values.push((Property::BitRate, Value::Number(u64::from(n) * 1000)));
        }
        if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
            for (p, v) in [
                (Property::Title, tag.title()),
                (Property::Artist, tag.artist()),
                (Property::Album, tag.album()),
                (Property::Genre, tag.genre()),
            ] {
                if has(p)
                    && let Some(v) = v
                {
                    values.push((p, Value::Text(v.chars().take(4096).collect())));
                }
            }
            for (p, v) in [
                (Property::Year, tag.date().map(|d| u32::from(d.year))),
                (Property::Track, tag.track()),
            ] {
                if has(p)
                    && let Some(v) = v
                {
                    values.push((p, Value::Number(v.into())));
                }
            }
        }
    }
    if matches!(kind, crate::kind::FileKind::Image) {
        let mut dimensions = None;
        if wanted
            .iter()
            .any(|p| matches!(p, Property::Width | Property::Height | Property::Dimensions))
        {
            let _ = reader.seek(SeekFrom::Start(0));
            dimensions = image_size(&mut reader);
        }
        let _ = reader.seek(SeekFrom::Start(0));
        if let Ok(exif) = exif::Reader::new().read_from_container(&mut BufReader::new(&mut reader))
        {
            use exif::{In, Tag};
            let field = |t| exif.get_field(t, In::PRIMARY);
            if dimensions.is_none() {
                dimensions = field(Tag::PixelXDimension)
                    .and_then(|f| f.value.get_uint(0))
                    .zip(field(Tag::PixelYDimension).and_then(|f| f.value.get_uint(0)))
                    .map(|(w, h)| (w as u64, h as u64));
            }
            if field(Tag::Orientation)
                .and_then(|f| f.value.get_uint(0))
                .is_some_and(|v| (5..=8).contains(&v))
            {
                dimensions = dimensions.map(|(w, h)| (h, w));
            }
            for (p, t) in [
                (Property::CameraMaker, Tag::Make),
                (Property::CameraModel, Tag::Model),
                (Property::DateTaken, Tag::DateTimeOriginal),
            ] {
                if has(p)
                    && let Some(f) = field(t)
                {
                    let text = match &f.value {
                        exif::Value::Ascii(v) => v
                            .first()
                            .map(|v| String::from_utf8_lossy(v).trim_end_matches('\0').to_owned())
                            .unwrap_or_default(),
                        _ => f.display_value().to_string(),
                    };
                    values.push((p, Value::Text(text.chars().take(4096).collect())));
                }
            }
            for (p, t, reference, negative) in [
                (
                    Property::GpsLat,
                    Tag::GPSLatitude,
                    Tag::GPSLatitudeRef,
                    b'S',
                ),
                (
                    Property::GpsLong,
                    Tag::GPSLongitude,
                    Tag::GPSLongitudeRef,
                    b'W',
                ),
            ] {
                if has(p)
                    && let Some(f) = field(t)
                    && let exif::Value::Rational(r) = &f.value
                    && r.len() == 3
                    && r.iter().all(|r| r.denom != 0)
                {
                    let degrees = r[0].to_f64() + r[1].to_f64() / 60. + r[2].to_f64() / 3600.;
                    let sign=if field(reference).is_some_and(|f|matches!(&f.value,exif::Value::Ascii(a) if a.first().and_then(|b|b.first())==Some(&negative))){-1.}else{1.};
                    if degrees.is_finite() && degrees <= 180. {
                        values.push((
                            p,
                            Value::Decimal((degrees * sign * 1_000_000.).round() as i64),
                        ));
                    }
                }
            }
        }
        if let Some((w, h)) = dimensions {
            dimension_values(w, h, wanted, &mut values);
        }
    }
    if matches!(ext.as_str(), "mp4" | "mov" | "m4v" | "mkv" | "webm") {
        let _ = reader.seek(SeekFrom::Start(0));
        let video = if matches!(ext.as_str(), "mkv" | "webm") {
            crate::video::mkv(&mut reader)
        } else {
            crate::video::mp4(&mut reader)
        };
        if let Some(v) = video {
            if let Some((w, h)) = v.width.zip(v.height) {
                dimension_values(w, h, wanted, &mut values);
            }
            if has(Property::Length)
                && let Some(ms) = v.milliseconds
            {
                values.push((Property::Length, Value::Number(ms)));
            }
            if has(Property::BitRate)
                && let Some(ms) = v.milliseconds.filter(|ms| *ms > 0)
            {
                values.push((
                    Property::BitRate,
                    Value::Number(meta.len().saturating_mul(8000) / ms),
                ));
            }
        }
    }
    if has(Property::PageCount)
        && ext == "pdf"
        && let Some(bytes) = crate::property_process::output(
            "pdfinfo",
            &[std::ffi::OsStr::new("--"), path.as_os_str()],
            TIME_CAP,
            progress,
        )
        && let Some(n) = pdf_pages(&bytes)
    {
        values.push((Property::PageCount, Value::Number(n)));
    }
    if has(Property::WordCount)
        && matches!(
            kind,
            crate::kind::FileKind::Text | crate::kind::FileKind::Code
        )
        && meta.len() < BYTE_CAP
    {
        let _ = reader.seek(SeekFrom::Start(0));
        let mut bytes = Vec::new();
        if reader.read_to_end(&mut bytes).is_ok()
            && !bytes.contains(&0)
            && let Ok(text) = std::str::from_utf8(&bytes)
        {
            values.push((
                Property::WordCount,
                Value::Number(text.split_whitespace().count() as u64),
            ));
        }
    }
    if (has(Property::Rating) || has(Property::Tags)) && reader.check().is_ok() {
        let _ = reader.seek(SeekFrom::Start(0));
        let mut bytes = Vec::new();
        let cap = reader.remaining.min(1024 * 1024);
        let _ = reader.by_ref().take(cap).read_to_end(&mut bytes);
        let mut xmp = xmp_values(&bytes, wanted);
        if xmp.is_empty() && ext != "xmp" {
            let sidecar = path.with_extension("xmp");
            if let Ok(fd) = rustix::fs::open(
                &sidecar,
                OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
                Mode::empty(),
            ) {
                let f = File::from(fd);
                if f.metadata()
                    .is_ok_and(|m| m.is_file() && m.len() <= reader.remaining.min(1024 * 1024))
                {
                    reader.file = f;
                    let mut data = Vec::new();
                    if reader.read_to_end(&mut data).is_ok() {
                        xmp = xmp_values(&data, wanted);
                    }
                }
            }
        }
        values.extend(xmp);
    }
    if progress.is_cancelled() {
        Vec::new()
    } else {
        values
    }
}
fn dimension_values(w: u64, h: u64, wanted: &[Property], values: &mut Vec<(Property, Value)>) {
    if w == 0 || h == 0 || w > u32::MAX as u64 || h > u32::MAX as u64 {
        return;
    }
    for (p, n) in [
        (Property::Width, w),
        (Property::Height, h),
        (Property::Dimensions, (w << 32) | h),
    ] {
        if wanted.contains(&p) {
            values.push((p, Value::Number(n)));
        }
    }
}
fn pdf_pages(bytes: &[u8]) -> Option<u64> {
    std::str::from_utf8(bytes)
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("Pages:")?.trim().parse().ok())
}
fn xmp_values(bytes: &[u8], wanted: &[Property]) -> Vec<(Property, Value)> {
    let text = String::from_utf8_lossy(bytes);
    let mut values = Vec::new();
    if wanted.contains(&Property::Rating) {
        let value = text
            .split("xmp:Rating=\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .or_else(|| {
                text.split("<xmp:Rating>")
                    .nth(1)
                    .and_then(|s| s.split('<').next())
            });
        if let Some(n) = value
            .and_then(|s| s.trim().parse::<i64>().ok())
            .filter(|n| (-1..=5).contains(n))
        {
            values.push((Property::Rating, Value::Signed(n)));
        }
    }
    if wanted.contains(&Property::Tags)
        && let Some(subject) = text
            .split("<dc:subject>")
            .nth(1)
            .and_then(|s| s.split("</dc:subject>").next())
    {
        let tags: Vec<_> = subject
            .split("<rdf:li>")
            .skip(1)
            .take(128)
            .filter_map(|s| s.split("</rdf:li>").next())
            .map(|s| {
                s.replace("&amp;", "&")
                    .replace("&lt;", "<")
                    .replace("&gt;", ">")
                    .replace("&quot;", "\"")
                    .replace("&apos;", "'")
            })
            .collect();
        if !tags.is_empty() {
            values.push((
                Property::Tags,
                Value::Text(tags.join("; ").chars().take(4096).collect()),
            ));
        }
    }
    values
}
pub(crate) fn image_size(r: &mut (impl Read + Seek)) -> Option<(u64, u64)> {
    let mut h = [0; 32];
    let n = r.read(&mut h).ok()?;
    let be = |at| Some(u32::from_be_bytes(h.get(at..at + 4)?.try_into().ok()?) as u64);
    let le = |at| Some(u32::from_le_bytes(h.get(at..at + 4)?.try_into().ok()?) as u64);
    if n >= 24 && h.starts_with(b"\x89PNG\r\n\x1a\n") && &h[12..16] == b"IHDR" {
        return Some((be(16)?, be(20)?));
    }
    if n >= 10 && (h.starts_with(b"GIF87a") || h.starts_with(b"GIF89a")) {
        return Some((
            u16::from_le_bytes(h[6..8].try_into().ok()?) as u64,
            u16::from_le_bytes(h[8..10].try_into().ok()?) as u64,
        ));
    }
    if n >= 26 && h.starts_with(b"BM") && le(14)? >= 40 {
        return Some((le(18)?, (le(22)? as i32).unsigned_abs() as u64));
    }
    if n >= 30 && &h[..4] == b"RIFF" && &h[8..12] == b"WEBP" && &h[12..16] == b"VP8X" {
        let w = u32::from_le_bytes([h[24], h[25], h[26], 0]) + 1;
        let h = u32::from_le_bytes([h[27], h[28], h[29], 0]) + 1;
        return Some((w.into(), h.into()));
    }
    if n >= 2 && h[..2] == [255, 216] {
        r.seek(SeekFrom::Start(2)).ok()?;
        loop {
            let mut b = [0];
            r.read_exact(&mut b).ok()?;
            if b[0] != 255 {
                return None;
            }
            while b[0] == 255 {
                r.read_exact(&mut b).ok()?;
            }
            if matches!(b[0], 0xd9 | 0xda) {
                return None;
            }
            if b[0] == 0x01 || (0xd0..=0xd7).contains(&b[0]) {
                continue;
            }
            let marker = b[0];
            let mut len = [0; 2];
            r.read_exact(&mut len).ok()?;
            let len = u16::from_be_bytes(len);
            if len < 2 {
                return None;
            }
            if matches!(marker,0xc0..=0xc3|0xc5..=0xc7|0xc9..=0xcb|0xcd..=0xcf) && len >= 7 {
                let mut size = [0; 5];
                r.read_exact(&mut size).ok()?;
                return Some((
                    u16::from_be_bytes([size[3], size[4]]) as u64,
                    u16::from_be_bytes([size[1], size[2]]) as u64,
                ));
            }
            r.seek(SeekFrom::Current(i64::from(len) - 2)).ok()?;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample(name: &str, bytes: &[u8], run: impl FnOnce(&Path)) {
        let dir = std::env::temp_dir().join(format!(
            "everygrep-media-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        run(&path);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn png_and_words_and_cancellation() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        png.extend(640u32.to_be_bytes());
        png.extend(480u32.to_be_bytes());
        png.extend([0; 8]);
        sample("image.png", &png, |path| {
            assert!(
                read(path, &[Property::Width, Property::Height], &Progress::new())
                    .contains(&(Property::Width, Value::Number(640)))
            )
        });
        sample("words.txt", b"one two\nthree", |path| {
            assert_eq!(
                read(path, &[Property::WordCount], &Progress::new()),
                vec![(Property::WordCount, Value::Number(3))]
            );
            let p = Progress::new();
            p.cancel();
            assert!(read(path, &[Property::WordCount], &p).is_empty());
        });
    }
    #[test]
    fn xmp_and_pdf_are_typed() {
        let xmp=br#"<rdf:Description xmp:Rating="4"><dc:subject><rdf:Bag><rdf:li>A &amp; B</rdf:li></rdf:Bag></dc:subject></rdf:Description>"#;
        assert_eq!(
            xmp_values(xmp, &[Property::Rating, Property::Tags]),
            vec![
                (Property::Rating, Value::Signed(4)),
                (Property::Tags, Value::Text("A & B".into()))
            ]
        );
        assert_eq!(pdf_pages(b"Title: A\nPages: 17\n"), Some(17));
        assert_eq!(pdf_pages(b"Pages: broken"), None);
    }
    #[test]
    fn wav_duration_without_cover_art() {
        let mut wav = b"RIFF".to_vec();
        wav.extend(2036u32.to_le_bytes());
        wav.extend(b"WAVEfmt ");
        wav.extend(16u32.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(1000u32.to_le_bytes());
        wav.extend(2000u32.to_le_bytes());
        wav.extend(2u16.to_le_bytes());
        wav.extend(16u16.to_le_bytes());
        wav.extend(b"data");
        wav.extend(2000u32.to_le_bytes());
        wav.extend(vec![0; 2000]);
        sample("audio.wav", &wav, |path| {
            assert!(
                read(path, &[Property::Length], &Progress::new())
                    .contains(&(Property::Length, Value::Number(1000)))
            )
        });
    }
    #[test]
    fn exif_orientation_swaps_display_dimensions() {
        let mut tiff = b"II\x2a\0\x08\0\0\0".to_vec();
        tiff.extend(1u16.to_le_bytes());
        tiff.extend(0x112u16.to_le_bytes());
        tiff.extend(3u16.to_le_bytes());
        tiff.extend(1u32.to_le_bytes());
        tiff.extend(6u32.to_le_bytes());
        tiff.extend(0u32.to_le_bytes());
        let mut jpeg = vec![255, 216, 255, 225];
        jpeg.extend(((tiff.len() + 8) as u16).to_be_bytes());
        jpeg.extend(b"Exif\0\0");
        jpeg.extend(tiff);
        jpeg.extend([255, 192, 0, 7, 8, 1, 224, 2, 128, 255, 217]);
        sample("portrait.jpg", &jpeg, |path| {
            let values = read(path, &[Property::Width, Property::Height], &Progress::new());
            assert!(values.contains(&(Property::Width, Value::Number(480))));
            assert!(values.contains(&(Property::Height, Value::Number(640))));
        });
    }
    #[test]
    fn byte_budget_stops_reads_and_special_files_are_skipped() {
        sample("budget.txt", b"hello", |path| {
            let progress = Progress::new();
            let mut reader = Budget {
                file: File::open(path).unwrap(),
                remaining: 2,
                started: Instant::now(),
                progress: &progress,
            };
            let mut bytes = Vec::new();
            assert!(reader.read_to_end(&mut bytes).is_err());
            assert_eq!(bytes, b"he");
            reader.remaining = 2;
            reader.started = Instant::now() - TIME_CAP;
            assert!(reader.read(&mut [0]).is_err());
        });
        assert!(
            read(
                Path::new("/dev/null"),
                &[Property::WordCount],
                &Progress::new()
            )
            .is_empty()
        );
    }
    #[test]
    fn media_sort_uses_numeric_values_and_cancel_keeps_the_order() {
        use crate::sort::{Sort, SortKey, sort_with_progress};
        let dir = std::env::temp_dir().join(format!("everygrep-media-sort-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut store = crate::store::NodeStore::new();
        let root = store.add(
            None,
            dir.as_os_str().as_encoded_bytes(),
            true,
            Default::default(),
        );
        let mut ids = Vec::new();
        for (name, width) in [("a.png", 100u32), ("b.png", 20u32)] {
            let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
            png.extend(width.to_be_bytes());
            png.extend(10u32.to_be_bytes());
            png.extend([0; 8]);
            std::fs::write(dir.join(name), png).unwrap();
            ids.push(store.add(Some(root), name.as_bytes(), false, Default::default()));
        }
        let original = ids.clone();
        let index = crate::index::Index::new(store);
        let sort = Sort {
            key: SortKey::Property(Property::Width),
            ..Sort::default()
        };
        sort_with_progress(&index, &mut ids, sort, 1, &Progress::new()).unwrap();
        assert_eq!(ids, [original[1], original[0]]);
        let progress = Progress::new();
        progress.cancel();
        ids.clone_from(&original);
        assert!(sort_with_progress(&index, &mut ids, sort, 1, &progress).is_err());
        assert_eq!(ids, original);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn a_panicking_reader_gives_no_values() {
        let values: Vec<(Property, Value)> = guarded(|| panic!("malformed file"));
        assert!(values.is_empty());
    }
    #[test]
    fn garbage_and_truncated_media_files_are_read_without_panics() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut noise = |n: usize| {
            (0..n)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    seed as u8
                })
                .collect::<Vec<u8>>()
        };
        let heads: [&[u8]; 9] = [
            b"ID3\x04\0\0\0\0\x01\x7f",
            b"fLaC\0\0\0\x22",
            b"OggS\0\x02",
            b"RIFF\xff\xff\xff\x7fWAVEfmt ",
            b"\xff\xd8\xff\xe1\xff\xffExif\0\0II*\0",
            b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR",
            b"\0\0\0\x18ftypisom\0\0\0\x01moov",
            b"\x1a\x45\xdf\xa3\x01\0\0\0\0\0\0\x1f\x18\x53\x80\x67",
            b"II*\0\x08\0\0\0\xff\xff",
        ];
        let names = [
            "a.mp3", "a.flac", "a.ogg", "a.wav", "a.jpg", "a.png", "a.mp4", "a.mkv", "a.tif",
        ];
        for (name, head) in names.iter().zip(heads) {
            for len in [0, 1, 7, 16, 33, 200, 4096] {
                let mut bytes = head.to_vec();
                bytes.extend(noise(len));
                for cut in [bytes.len(), bytes.len() / 2, head.len()] {
                    sample(name, &bytes[..cut], |path| {
                        let _ = read_unguarded(path, &Property::ALL, &Progress::new());
                    });
                }
            }
        }
    }
    #[test]
    fn malformed_and_links_are_empty() {
        sample("bad.jpg", &[255, 216, 255, 192, 0, 1], |path| {
            assert!(read(path, &[Property::Width], &Progress::new()).is_empty());
            let link = path.with_file_name("link.jpg");
            std::os::unix::fs::symlink(path, &link).unwrap();
            assert!(read(&link, &[Property::Width], &Progress::new()).is_empty());
        });
    }
}
