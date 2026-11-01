//! Natural, diacritic-folded order of file names.
//!
//! [`collate`] compares two case-folded names (as the index stores them) the
//! way a reader expects a file list to be sorted:
//!
//! - **Numbers by value.** A run of ASCII digits is one unit compared by its
//!   value: `file2 < file10`, `v1.9 < v1.10`. Leading zeros do not change the
//!   value; between equal values, more leading zeros come first
//!   (`002 < 02 < 2`). Runs of any length are compared without overflow.
//! - **Diacritics ignored.** Letters with marks weigh as their base letters
//!   from [`base_letters`] (`ç` = `c`, `ş` = `s`, `ö` = `o`, `ı` = `i`), letters
//!   such as `ß` count as two (`ss`), and combining marks U+0300–U+036F are
//!   skipped, so a decomposed `e` + U+0301 equals `é`.
//! - **`ə` is a letter of its own**, right after `e` and before `f`. (Search
//!   still matches it as `e` when diacritics are ignored.)
//! - **Hyphens, apostrophes and control characters are skipped**, so `a-b`
//!   equals `ab`.
//! - **Character classes** in this order: space; the punctuation
//!   ``! " # $ % & ( ) * , . / : ; ? @ [ \ ] ^ _ ` { | } ~``; then `+ < = >`;
//!   then numbers; then the letters `a`–`z`; then every other character by
//!   code point; bytes that are not valid UTF-8 come last, by byte value.
//!
//! Units are compared from the left; a name that runs out of units first is
//! the smaller one. Different names can compare equal (`a-b` and `ab`, `çay`
//! and `cay`); callers break such ties with the raw bytes.
//!
//! The input is expected to be folded with [`crate::fold::fold`]; upper case
//! ASCII letters weigh as lower case, other upper case letters as their base
//! letters or by code point.
//!
//! Letter weights leave gaps between neighbouring letters, so an order for a
//! particular alphabet (for example `c < ç < d`) can be added by giving the
//! extra letters weights in those gaps in [`char_units`].

use std::cmp::Ordering;

use crate::fold::base_letters;

/// The weight of one unit. Equal weights compare equal; weight 0 marks a
/// character that is skipped.
type Weight = u32;

/// Weight of a run of digits, between punctuation and letters. Two runs are
/// compared by value (see [`cmp_digits`]).
const DIGITS: Weight = 0x100;

/// Weight of `a`; each next letter is [`LETTER_STEP`] higher.
const LETTER_A: Weight = 0x200;

/// Distance between the weights of two neighbouring letters.
const LETTER_STEP: Weight = 8;

/// Weight of `ə`, between `e` and `f`.
const SCHWA: Weight = letter(b'e') + LETTER_STEP / 2;

/// Base weight of characters without a place of their own; the code point is
/// added to it.
const OTHER: Weight = 0x1000;

/// Base weight of bytes that are not valid UTF-8; the byte is added to it.
const INVALID: Weight = OTHER + 0x11_0000;

/// Space and punctuation, in increasing weight.
const PUNCTUATION: &[u8] = b" !\"#$%&()*,./:;?@[\\]^_`{|}~+<=>";

const fn letter(lower: u8) -> Weight {
    LETTER_A + (lower - b'a') as Weight * LETTER_STEP
}

/// Weights of the ASCII characters; digits are handled as runs and keep 0
/// here, as do the skipped characters.
const ASCII: [Weight; 128] = {
    let mut table = [0; 128];
    let mut i = 0;
    while i < PUNCTUATION.len() {
        table[PUNCTUATION[i] as usize] = 1 + i as Weight;
        i += 1;
    }
    let mut c = b'a';
    while c <= b'z' {
        table[c as usize] = letter(c);
        table[c.to_ascii_uppercase() as usize] = letter(c);
        c += 1;
    }
    table
};

/// Compares two folded names in natural, diacritic-folded order (see the
/// module documentation).
pub fn collate(a: &[u8], b: &[u8]) -> Ordering {
    let start = shared_start(a, b);
    if start == a.len() && start == b.len() {
        return Ordering::Equal;
    }
    let (mut x, mut y) = (Units::new(&a[start..]), Units::new(&b[start..]));
    loop {
        match (x.next(), y.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(u), Some(v)) => match cmp_units(u, v) {
                Ordering::Equal => {}
                other => return other,
            },
        }
    }
}

/// Length of the common prefix of `a` and `b`, shortened so that it ends
/// where both names start a new unit: not inside a character and not inside
/// a run of digits. The units before it are the same in both names.
fn shared_start(a: &[u8], b: &[u8]) -> usize {
    let mut start = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let continues = |s: &[u8], at: usize| s.get(at).is_some_and(|&c| c & 0xC0 == 0x80);
    // A byte that is not a continuation byte always starts a unit.
    while start > 0 && (continues(a, start) || continues(b, start)) {
        start -= 1;
    }
    let digit = |s: &[u8], at: usize| s.get(at).is_some_and(u8::is_ascii_digit);
    if digit(a, start) || digit(b, start) {
        while start > 0 && a[start - 1].is_ascii_digit() {
            start -= 1;
        }
    }
    start
}

/// One unit of a name.
#[derive(Clone, Copy)]
enum Unit<'a> {
    /// A character (or one letter of an expansion).
    Char(Weight),
    /// A run of ASCII digits, leading zeros included.
    Digits(&'a [u8]),
}

fn cmp_units(a: Unit, b: Unit) -> Ordering {
    match (a, b) {
        (Unit::Char(x), Unit::Char(y)) => x.cmp(&y),
        (Unit::Char(x), Unit::Digits(_)) => x.cmp(&DIGITS),
        (Unit::Digits(_), Unit::Char(y)) => DIGITS.cmp(&y),
        (Unit::Digits(x), Unit::Digits(y)) => cmp_digits(x, y),
    }
}

/// Compares two digit runs by value, then puts the one with more leading
/// zeros first.
fn cmp_digits(a: &[u8], b: &[u8]) -> Ordering {
    let zeros = |s: &[u8]| s.iter().take_while(|&&c| c == b'0').count();
    let (za, zb) = (zeros(a), zeros(b));
    let (sa, sb) = (&a[za..], &b[zb..]);
    sa.len()
        .cmp(&sb.len())
        .then_with(|| sa.cmp(sb))
        .then_with(|| zb.cmp(&za))
}

/// The units of a name, skipping characters of weight 0.
struct Units<'a> {
    rest: &'a [u8],
    /// Letters of an expansion not returned yet, last one first.
    pending: [Weight; 3],
    pending_len: usize,
}

impl<'a> Units<'a> {
    fn new(name: &'a [u8]) -> Self {
        Self {
            rest: name,
            pending: [0; 3],
            pending_len: 0,
        }
    }

    fn next(&mut self) -> Option<Unit<'a>> {
        if self.pending_len > 0 {
            self.pending_len -= 1;
            return Some(Unit::Char(self.pending[self.pending_len]));
        }
        loop {
            let &first = self.rest.first()?;
            if first.is_ascii() {
                if first.is_ascii_digit() {
                    let end = self
                        .rest
                        .iter()
                        .position(|c| !c.is_ascii_digit())
                        .unwrap_or(self.rest.len());
                    let (run, rest) = self.rest.split_at(end);
                    self.rest = rest;
                    return Some(Unit::Digits(run));
                }
                self.rest = &self.rest[1..];
                match ASCII[first as usize] {
                    0 => continue,
                    w => return Some(Unit::Char(w)),
                }
            }
            let Some((c, len)) = decode(self.rest) else {
                self.rest = &self.rest[1..];
                return Some(Unit::Char(INVALID + Weight::from(first)));
            };
            self.rest = &self.rest[len..];
            let mut weights = [0; 3];
            let n = char_units(c, &mut weights);
            if n == 0 {
                continue;
            }
            for (slot, &w) in self.pending.iter_mut().zip(weights[1..n].iter().rev()) {
                *slot = w;
            }
            self.pending_len = n - 1;
            return Some(Unit::Char(weights[0]));
        }
    }
}

/// Decodes the non-ASCII character at the start of `s` and its length in
/// bytes, or `None` when `s` does not start with valid UTF-8.
fn decode(s: &[u8]) -> Option<(char, usize)> {
    let len = match s[0] {
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => return None,
    };
    let text = std::str::from_utf8(s.get(..len)?).ok()?;
    text.chars().next().map(|c| (c, len))
}

/// Writes the weights of the non-ASCII character `c` to `out` and returns how
/// many there are (0 for a skipped character).
fn char_units(c: char, out: &mut [Weight; 3]) -> usize {
    if c == 'ə' || c == 'Ə' {
        out[0] = SCHWA;
        return 1;
    }
    match base_letters(c) {
        Some(base) => {
            let mut n = 0;
            for &b in base.as_bytes() {
                out[n] = ASCII[usize::from(b)];
                n += 1;
            }
            n
        }
        None => {
            out[0] = OTHER + c as Weight;
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fold::fold;
    use std::cmp::Reverse;

    /// One unit of [`slow_key`]: the weight, and for digit runs the
    /// significant digits and the number of leading zeros.
    type SlowUnit = (Weight, usize, Vec<u8>, Reverse<usize>);

    /// The order spelled out without shortcuts: every unit of the whole name
    /// as a sortable value.
    fn slow_key(name: &[u8]) -> Vec<SlowUnit> {
        let mut key = Vec::new();
        let char_unit = |w| (w, 0, Vec::new(), Reverse(0));
        for chunk in name.utf8_chunks() {
            let chars: Vec<char> = chunk.valid().chars().collect();
            let mut i = 0;
            while i < chars.len() {
                let c = chars[i];
                if c.is_ascii_digit() {
                    let mut run = Vec::new();
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        run.push(chars[i] as u8);
                        i += 1;
                    }
                    let zeros = run.iter().take_while(|&&d| d == b'0').count();
                    let digits = run[zeros..].to_vec();
                    key.push((DIGITS, digits.len(), digits, Reverse(zeros)));
                    continue;
                }
                i += 1;
                if c == 'ə' || c == 'Ə' {
                    key.push(char_unit(SCHWA));
                } else if c.is_ascii() {
                    let w = ASCII[c as usize];
                    if w != 0 {
                        key.push(char_unit(w));
                    }
                } else if let Some(base) = base_letters(c) {
                    for b in base.bytes() {
                        key.push(char_unit(ASCII[usize::from(b)]));
                    }
                } else {
                    key.push(char_unit(OTHER + c as Weight));
                }
            }
            for &b in chunk.invalid() {
                key.push(char_unit(INVALID + Weight::from(b)));
            }
        }
        key
    }

    fn slow_collate(a: &[u8], b: &[u8]) -> Ordering {
        slow_key(a).cmp(&slow_key(b))
    }

    /// Sorts folded `names` and checks that they come out as given.
    fn assert_order(names: &[&str]) {
        let mut sorted: Vec<&str> = names.to_vec();
        sorted.reverse();
        sorted.sort_by(|a, b| {
            collate(&fold(a.as_bytes()), &fold(b.as_bytes())).then_with(|| a.cmp(b))
        });
        assert_eq!(sorted, names);
        for w in names.windows(2) {
            let (a, b) = (fold(w[0].as_bytes()), fold(w[1].as_bytes()));
            assert_eq!(collate(&a, &b), slow_collate(&a, &b), "{:?}", w);
        }
    }

    fn eq(a: &str, b: &str) -> bool {
        collate(&fold(a.as_bytes()), &fold(b.as_bytes())) == Ordering::Equal
    }

    #[test]
    fn numbers_sort_by_value() {
        assert_order(&["file1", "file002", "file02", "file2", "file10", "file100"]);
        assert_order(&["v1.9", "v1.9.1", "v1.10", "v2"]);
        assert_order(&["00", "0", "1", "9", "10"]);
        assert_order(&[
            "x99999999999999999999999",
            "x100000000000000000000000",
            "x100000000000000000000001",
        ]);
        assert_order(&["+", ">", "9lives", "a"]);
    }

    #[test]
    fn case_is_ignored_and_raw_bytes_break_ties() {
        assert!(eq("Zoo.txt", "zoo.txt"));
        assert_order(&["apple", "Banana", "cherry", "Zoo.txt", "zoo.txt"]);
    }

    #[test]
    fn diacritics_weigh_as_their_base_letters() {
        assert!(eq("Çay.txt", "cay.txt"));
        assert!(eq("Ölçü", "olcu"));
        assert!(eq("ğ", "g"));
        assert!(eq("Straße", "strasse"));
        assert!(eq("cafe\u{301}", "café"));
        assert_order(&["çay", "Cay.txt", "cay.txt", "Çay.txt", "cbx", "cz"]);
        assert_order(&["öa", "olcu", "Ölçü", "oz"]);
        assert_order(&["şa", "Sekil.png", "Səkil.png", "Şəkil.png", "Straße", "sz"]);
    }

    #[test]
    fn schwa_is_a_letter_after_e() {
        assert!(!eq("ə", "e"));
        assert_order(&["eli.doc", "ez", "ezan.doc", "əa", "Əli.doc", "fa"]);
    }

    #[test]
    fn every_i_weighs_as_i() {
        assert!(eq("ılıq", "ILIQ"));
        assert!(eq("İzmir", "izmir"));
        assert_order(&["ia", "ilıq", "ılıq", "iz", "ız", "Izmir", "İzmir"]);
    }

    #[test]
    fn punctuation_order_and_skipped_characters() {
        assert!(eq("a-b", "ab"));
        assert!(eq("rock'n'roll", "rocknroll"));
        assert!(eq("a\u{1}b", "ab"));
        assert_order(&[
            " lead", "!bang", ".hidden", "_under", "~tilde", "+plus", "9lives", "a b", "a.b",
            "a_b", "a1", "a-b", "ab",
        ]);
        assert_order(&["file.txt", "file1.txt", "file-2.txt"]);
    }

    #[test]
    fn other_characters_and_invalid_bytes_come_last() {
        assert_order(&["z", "ω", "я", "名"]);
        let (z, omega, bad) = (b"z".as_slice(), "ω".as_bytes(), b"\xff".as_slice());
        assert_eq!(collate(z, omega), Ordering::Less);
        assert_eq!(collate(omega, bad), Ordering::Less);
        assert_eq!(collate(b"\xfe", b"\xff"), Ordering::Less);
        assert_eq!(collate(b"", b""), Ordering::Equal);
        assert_eq!(collate(b"", b"-"), Ordering::Equal);
        assert_eq!(collate(b"", b"a"), Ordering::Less);
    }

    /// A small xorshift generator, so the tests need no crate.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// Pieces random names are made of: the characters with special rules,
    /// and broken UTF-8.
    const PIECES: &[&[u8]] = &[
        b"0",
        b"0",
        b"1",
        b"7",
        b"9",
        b"-",
        b"'",
        b".",
        b" ",
        b"+",
        b"\x01",
        b"a",
        b"A",
        b"c",
        "ç".as_bytes(),
        b"e",
        "ə".as_bytes(),
        "Ə".as_bytes(),
        b"i",
        "ı".as_bytes(),
        "i\u{307}".as_bytes(),
        "ß".as_bytes(),
        b"s",
        "\u{301}".as_bytes(),
        "ω".as_bytes(),
        b"\xff",
        b"\xe2\x82",
        b"\x82",
    ];

    fn random_name(rng: &mut Rng) -> Vec<u8> {
        let mut name = Vec::new();
        for _ in 0..rng.below(9) {
            name.extend_from_slice(PIECES[rng.below(PIECES.len())]);
        }
        name
    }

    #[test]
    fn fast_order_equals_the_slow_one() {
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        for _ in 0..100_000 {
            let a = random_name(&mut rng);
            // Half of the pairs share a prefix, which the fast path skips.
            let b = if rng.below(2) == 0 {
                random_name(&mut rng)
            } else {
                let mut b = a[..rng.below(a.len() + 1)].to_vec();
                b.extend(random_name(&mut rng));
                b
            };
            let expected = slow_collate(&a, &b);
            assert_eq!(collate(&a, &b), expected, "{a:?} vs {b:?}");
            assert_eq!(collate(&b, &a), expected.reverse(), "{b:?} vs {a:?}");
        }
    }

    #[test]
    fn order_is_transitive() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut names: Vec<Vec<u8>> = (0..400).map(|_| random_name(&mut rng)).collect();
        names.sort_by(|a, b| slow_collate(a, b));
        for i in 0..names.len() {
            for j in i..names.len() {
                assert_ne!(
                    collate(&names[i], &names[j]),
                    Ordering::Greater,
                    "{:?} vs {:?}",
                    names[i],
                    names[j]
                );
            }
        }
    }
}
