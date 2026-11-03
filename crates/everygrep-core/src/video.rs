//! Header-only MP4/MOV and Matroska/WebM metadata. Payloads are skipped.
use std::io::{Read, Seek, SeekFrom};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Video {
    pub width: Option<u64>,
    pub height: Option<u64>,
    pub milliseconds: Option<u64>,
}
fn u32be(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}
fn u64be(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

pub fn mp4(r: &mut (impl Read + Seek)) -> Option<Video> {
    let end = r.seek(SeekFrom::End(0)).ok()?;
    let mut out = Video::default();
    boxes(r, 0, end, 0, &mut out)?;
    Some(out)
}
fn boxes(
    r: &mut (impl Read + Seek),
    mut at: u64,
    end: u64,
    depth: u8,
    out: &mut Video,
) -> Option<()> {
    if depth > 8 {
        return None;
    }
    while at.checked_add(8)? <= end {
        r.seek(SeekFrom::Start(at)).ok()?;
        let mut h = [0; 8];
        r.read_exact(&mut h).ok()?;
        let mut size = u64::from(u32be(&h, 0)?);
        let mut header = 8;
        if size == 1 {
            let mut b = [0; 8];
            r.read_exact(&mut b).ok()?;
            size = u64::from_be_bytes(b);
            header = 16;
        }
        if size == 0 {
            size = end.checked_sub(at)?;
        }
        let next = at.checked_add(size)?;
        if size < header || next > end {
            return None;
        }
        let data = at.checked_add(header)?;
        match &h[4..8] {
            b"moov" | b"trak" | b"mdia" => boxes(r, data, next, depth + 1, out)?,
            b"mvhd" => {
                let mut b = vec![0; usize::try_from((size - header).min(32)).ok()?];
                r.read_exact(&mut b).ok()?;
                let (scale, duration) = if b.first() == Some(&1) {
                    (u32be(&b, 20)? as u64, u64be(&b, 24)?)
                } else if b.first() == Some(&0) {
                    (u32be(&b, 12)? as u64, u32be(&b, 16)? as u64)
                } else {
                    return None;
                };
                if scale > 0 && duration != u64::MAX && duration != u32::MAX as u64 {
                    out.milliseconds = duration.checked_mul(1000).map(|d| d / scale);
                }
            }
            b"tkhd" => {
                let len = size - header;
                if len >= 84 {
                    r.seek(SeekFrom::Start(next - 8)).ok()?;
                    let mut b = [0; 8];
                    r.read_exact(&mut b).ok()?;
                    let w = u32be(&b, 0)? >> 16;
                    let h = u32be(&b, 4)? >> 16;
                    if w > 0
                        && h > 0
                        && u64::from(w) * u64::from(h)
                            > out.width.unwrap_or(0) * out.height.unwrap_or(0)
                    {
                        out.width = Some(w.into());
                        out.height = Some(h.into());
                    }
                }
            }
            _ => {}
        }
        at = next;
    }
    Some(())
}

fn vint(r: &mut impl Read, id: bool) -> Option<(u64, u8, bool)> {
    let mut b = [0];
    r.read_exact(&mut b).ok()?;
    let len = b[0].leading_zeros() as u8 + 1;
    if len > 8 || (id && len > 4) {
        return None;
    }
    let mask = 0x80u8 >> (len - 1);
    let mut v = u64::from(if id { b[0] } else { b[0] & !mask });
    for _ in 1..len {
        r.read_exact(&mut b).ok()?;
        v = (v << 8) | u64::from(b[0]);
    }
    let unknown = !id && v == (1u64 << (7 * len)) - 1;
    Some((v, len, unknown))
}
#[derive(Default)]
struct Mkv {
    out: Video,
    scale: u64,
    duration: Option<f64>,
}
pub fn mkv(r: &mut (impl Read + Seek)) -> Option<Video> {
    let end = r.seek(SeekFrom::End(0)).ok()?;
    let mut state = Mkv {
        scale: 1_000_000,
        ..Mkv::default()
    };
    elements(r, 0, end, 0, &mut state)?;
    if let Some(d) = state.duration {
        let ms = d * state.scale as f64 / 1_000_000.;
        if ms.is_finite() && ms >= 0. && ms < u64::MAX as f64 {
            state.out.milliseconds = Some(ms as u64);
        }
    }
    Some(state.out)
}
fn elements(
    r: &mut (impl Read + Seek),
    mut at: u64,
    end: u64,
    depth: u8,
    s: &mut Mkv,
) -> Option<()> {
    if depth > 8 {
        return None;
    }
    while at < end {
        r.seek(SeekFrom::Start(at)).ok()?;
        let (id, il, _) = vint(r, true)?;
        let (size, sl, unknown) = vint(r, false)?;
        let data = at.checked_add(u64::from(il + sl))?;
        let next = if unknown {
            end
        } else {
            data.checked_add(size)?
        };
        if next > end || next <= at {
            return None;
        }
        match id {
            0x18538067 | 0x1549a966 | 0x1654ae6b | 0xae | 0xe0 => {
                elements(r, data, next, depth + 1, s)?
            }
            0x2ad7b1 | 0xb0 | 0xba if size > 0 && size <= 8 => {
                let mut value = 0u64;
                for _ in 0..size {
                    let mut b = [0];
                    r.read_exact(&mut b).ok()?;
                    value = (value << 8) | u64::from(b[0]);
                }
                match id {
                    0x2ad7b1 => s.scale = value,
                    0xb0 => s.out.width = Some(value),
                    _ => s.out.height = Some(value),
                }
            }
            0x4489 if size == 4 || size == 8 => {
                let mut b = [0; 8];
                r.read_exact(&mut b[..size as usize]).ok()?;
                s.duration = Some(if size == 4 {
                    f32::from_be_bytes(b[..4].try_into().ok()?) as f64
                } else {
                    f64::from_be_bytes(b)
                });
            }
            // Cluster payload can contain unknown-sized children; no metadata follows
            // it that this reader needs, so stop without scanning video packets.
            0x1f43b675 => break,
            _ => {}
        }
        at = next;
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    fn atom(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut b = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        b.extend(kind);
        b.extend(payload);
        b
    }
    #[test]
    fn mp4_headers_and_malformed_boxes() {
        let mut mv = vec![0; 20];
        mv[12..16].copy_from_slice(&1000u32.to_be_bytes());
        mv[16..20].copy_from_slice(&12345u32.to_be_bytes());
        let mut tk = vec![0; 84];
        tk[76..80].copy_from_slice(&(1920u32 << 16).to_be_bytes());
        tk[80..84].copy_from_slice(&(1080u32 << 16).to_be_bytes());
        let mut content = atom(b"mvhd", &mv);
        content.extend(atom(b"trak", &atom(b"tkhd", &tk)));
        let sample = atom(b"moov", &content);
        let dir = std::env::temp_dir().join(format!("everygrep-video-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.mp4");
        std::fs::write(&path, &sample).unwrap();
        assert_eq!(
            mp4(&mut std::fs::File::open(&path).unwrap()),
            Some(Video {
                width: Some(1920),
                height: Some(1080),
                milliseconds: Some(12345)
            })
        );
        assert!(mp4(&mut Cursor::new([0, 0, 0, 2, b'm', b'o', b'o', b'v'])).is_none());
        assert!(mp4(&mut Cursor::new([255; 16])).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn mkv_scale_dimensions_and_float_duration() {
        fn e(id: &[u8], data: &[u8]) -> Vec<u8> {
            let mut b = id.to_vec();
            b.push(0x80 | data.len() as u8);
            b.extend(data);
            b
        }
        let mut info = e(&[0x2a, 0xd7, 0xb1], &[0x0f, 0x42, 0x40]);
        info.extend(e(&[0x44, 0x89], &2500f64.to_be_bytes()));
        let mut v = e(&[0xb0], &[0x05, 0]);
        v.extend(e(&[0xba], &[0x02, 0xd0]));
        let mut b = e(&[0x15, 0x49, 0xa9, 0x66], &info);
        b.extend(e(&[0x16, 0x54, 0xae, 0x6b], &e(&[0xae], &e(&[0xe0], &v))));
        let b = e(&[0x18, 0x53, 0x80, 0x67], &b);
        assert_eq!(
            mkv(&mut Cursor::new(b)),
            Some(Video {
                width: Some(1280),
                height: Some(720),
                milliseconds: Some(2500)
            })
        );
        assert!(mkv(&mut Cursor::new([0, 0])).is_none());
        assert!(mkv(&mut Cursor::new([0xae, 0xfe])).is_none());
    }
}
