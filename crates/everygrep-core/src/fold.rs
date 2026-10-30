//! Case folding used for case-insensitive matching, and diacritic folding
//! used when diacritics are ignored (see [`strip_diacritics_into`]).
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

/// Appends `text` with diacritics removed to `out`: letters with marks become
/// their base letters and combining marks are dropped. Case is kept, so it
/// works on folded and unfolded text alike. Bytes that are not valid UTF-8 are
/// copied unchanged.
///
/// The table covers Latin-1, Latin Extended-A and a few more letters used in
/// Azerbaijani, Turkish, Romanian and Vietnamese:
///
/// | Letters | Base |
/// |---|---|
/// | à á â ã ä å ā ă ą ǎ | a |
/// | ç ć ĉ ċ č | c |
/// | ð ď đ | d |
/// | è é ê ë ē ĕ ė ę ě ə | e |
/// | ĝ ğ ġ ģ | g |
/// | ĥ ħ | h |
/// | ì í î ï ĩ ī ĭ į ı ǐ, and İ | i |
/// | ĵ | j |
/// | ķ ĸ | k |
/// | ĺ ļ ľ ŀ ł | l |
/// | ñ ń ņ ň ŉ ŋ | n |
/// | ò ó ô õ ö ø ō ŏ ő ơ ǒ | o |
/// | ŕ ŗ ř | r |
/// | ś ŝ ş š ș ſ | s |
/// | ţ ť ŧ ț | t |
/// | ù ú û ü ũ ū ŭ ů ű ų ư ǔ | u |
/// | ŵ | w |
/// | ý ÿ ŷ | y |
/// | ź ż ž | z |
/// | æ, œ, ĳ, ß, þ | ae, oe, ij, ss, th |
///
/// Upper case letters map to the upper case base (`Ş` → `S`, `Ə` → `E`).
/// Combining marks U+0300–U+036F are removed, so decomposed names (`e` +
/// U+0301) match too, and the lowercase `İ` (`i` + U+0307) becomes `i`.
pub fn strip_diacritics_into(text: &[u8], out: &mut Vec<u8>) {
    if text.is_ascii() {
        out.extend_from_slice(text);
        return;
    }
    for chunk in text.utf8_chunks() {
        let valid = chunk.valid();
        let mut copied = 0;
        for (at, c) in valid.char_indices() {
            if c.is_ascii() {
                continue;
            }
            if let Some(base) = base_letters(c) {
                out.extend_from_slice(&valid.as_bytes()[copied..at]);
                out.extend_from_slice(base.as_bytes());
                copied = at + c.len_utf8();
            }
        }
        out.extend_from_slice(&valid.as_bytes()[copied..]);
        out.extend_from_slice(chunk.invalid());
    }
}

/// Returns `text` with diacritics removed, see [`strip_diacritics_into`].
pub fn strip_diacritics(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    strip_diacritics_into(text, &mut out);
    out
}

/// The base letters of `c` when it carries a diacritic (empty for a combining
/// mark), or `None` to keep `c`.
pub fn base_letters(c: char) -> Option<&'static str> {
    let base = match c {
        '\u{300}'..='\u{36f}' => "",
        'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' | 'Ā' | 'Ă' | 'Ą' | 'Ǎ' => "A",
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' | 'ǎ' => "a",
        'Æ' => "AE",
        'æ' => "ae",
        'Ç' | 'Ć' | 'Ĉ' | 'Ċ' | 'Č' => "C",
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => "c",
        'Ð' | 'Ď' | 'Đ' => "D",
        'ð' | 'ď' | 'đ' => "d",
        'È' | 'É' | 'Ê' | 'Ë' | 'Ē' | 'Ĕ' | 'Ė' | 'Ę' | 'Ě' | 'Ə' => "E",
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' | 'ə' => "e",
        'Ĝ' | 'Ğ' | 'Ġ' | 'Ģ' => "G",
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => "g",
        'Ĥ' | 'Ħ' => "H",
        'ĥ' | 'ħ' => "h",
        'Ì' | 'Í' | 'Î' | 'Ï' | 'Ĩ' | 'Ī' | 'Ĭ' | 'Į' | 'İ' | 'Ǐ' => "I",
        'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' | 'ǐ' => "i",
        'Ĳ' => "IJ",
        'ĳ' => "ij",
        'Ĵ' => "J",
        'ĵ' => "j",
        'Ķ' => "K",
        'ķ' | 'ĸ' => "k",
        'Ĺ' | 'Ļ' | 'Ľ' | 'Ŀ' | 'Ł' => "L",
        'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => "l",
        'Ñ' | 'Ń' | 'Ņ' | 'Ň' | 'Ŋ' => "N",
        'ñ' | 'ń' | 'ņ' | 'ň' | 'ŉ' | 'ŋ' => "n",
        'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' | 'Ø' | 'Ō' | 'Ŏ' | 'Ő' | 'Ơ' | 'Ǒ' => "O",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' | 'ơ' | 'ǒ' => "o",
        'Œ' => "OE",
        'œ' => "oe",
        'Ŕ' | 'Ŗ' | 'Ř' => "R",
        'ŕ' | 'ŗ' | 'ř' => "r",
        'Ś' | 'Ŝ' | 'Ş' | 'Š' | 'Ș' => "S",
        'ś' | 'ŝ' | 'ş' | 'š' | 'ș' | 'ſ' => "s",
        'ß' => "ss",
        'Ţ' | 'Ť' | 'Ŧ' | 'Ț' => "T",
        'ţ' | 'ť' | 'ŧ' | 'ț' => "t",
        'Þ' => "TH",
        'þ' => "th",
        'Ù' | 'Ú' | 'Û' | 'Ü' | 'Ũ' | 'Ū' | 'Ŭ' | 'Ů' | 'Ű' | 'Ų' | 'Ư' | 'Ǔ' => "U",
        'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' | 'ư' | 'ǔ' => "u",
        'Ŵ' => "W",
        'ŵ' => "w",
        'Ý' | 'Ÿ' | 'Ŷ' => "Y",
        'ý' | 'ÿ' | 'ŷ' => "y",
        'Ź' | 'Ż' | 'Ž' => "Z",
        'ź' | 'ż' | 'ž' => "z",
        _ => return None,
    };
    Some(base)
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

    #[test]
    fn diacritics_fold_to_base_letters() {
        let s = |t: &str| String::from_utf8(strip_diacritics(t.as_bytes())).unwrap();
        assert_eq!(s("Şəkil Ölçü ğıç"), "Sekil Olcu gic");
        assert_eq!(s("café Ærø Straße"), "cafe AEro Strasse");
        assert_eq!(s("Țară Łódź"), "Tara Lodz");
        assert_eq!(s("plain.txt"), "plain.txt");
        // Combining marks are dropped: decomposed "é" and lowercase "İ".
        assert_eq!(s("cafe\u{301}"), "cafe");
        assert_eq!(
            s(&String::from_utf8(fold("İzmir".as_bytes())).unwrap()),
            "izmir"
        );
        // Letters outside the table stay.
        assert_eq!(s("Ωмя 名"), "Ωмя 名");
    }

    #[test]
    fn diacritic_folding_keeps_invalid_bytes() {
        assert_eq!(strip_diacritics(b"\xffe\xcc\x81\xfe"), b"\xffe\xfe");
        assert_eq!(strip_diacritics("é\u{0}".as_bytes()), b"e\0");
    }
}
