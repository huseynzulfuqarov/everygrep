//! Case folding used for case-insensitive matching.
//!
//! File names on Linux are arbitrary bytes. Valid UTF-8 runs are lowercased with
//! the Unicode rules from the standard library; bytes that are not valid UTF-8 are
//! copied with only ASCII lowercasing, so folding never fails and never panics.

/// Appends the folded form of `name` to `out`.
pub fn fold_into(name: &[u8], out: &mut Vec<u8>) {
    if name.is_ascii() {
        out.extend(name.iter().map(u8::to_ascii_lowercase));
        return;
    }
    for chunk in name.utf8_chunks() {
        for c in chunk.valid().chars() {
            if c.is_ascii() {
                out.push(c.to_ascii_lowercase() as u8);
            } else {
                let mut buf = [0u8; 4];
                for lower in c.to_lowercase() {
                    out.extend_from_slice(lower.encode_utf8(&mut buf).as_bytes());
                }
            }
        }
        out.extend(chunk.invalid().iter().map(u8::to_ascii_lowercase));
    }
}

/// Returns the folded form of `name` as a new buffer.
pub fn fold(name: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(name.len());
    fold_into(name, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_is_lowercased() {
        assert_eq!(fold(b"ReadMe.TXT"), b"readme.txt");
    }

    #[test]
    fn unicode_is_lowercased() {
        assert_eq!(fold("ŞƏKİL Ölçü".as_bytes()), "şəki̇l ölçü".as_bytes());
    }

    #[test]
    fn invalid_utf8_is_kept() {
        assert_eq!(fold(b"A\xffB"), b"a\xffb");
    }
}
