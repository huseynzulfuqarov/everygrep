//! Typing in the result list to jump to a name.
//!
//! With the list focused, typed characters collect into a jump text; a
//! pause of [`PAUSE`] starts a new one. The cursor moves to the first row
//! after it whose name starts with the text, wrapping around at the end;
//! when the text grows, the current row itself is tried first, so typing
//! `rep` stays on `report.pdf`. A text of one repeated letter (`aaa`)
//! steps through the names starting with that letter, one row a key.
//! Names are compared as searches compare them by default: case and
//! diacritics ignored, so `s` finds `Şəkil.png`.

use std::time::{Duration, Instant};

use everygrep_core::fold;

/// A pause this long between keys starts a new jump text.
pub const PAUSE: Duration = Duration::from_millis(1000);

/// The jump text being typed.
#[derive(Debug, Default)]
pub struct Jump {
    text: String,
    last: Option<Instant>,
}

/// Where to look after a key.
#[derive(Debug, PartialEq, Eq)]
pub struct Look {
    /// The folded start of the names to find.
    pub prefix: Vec<u8>,
    /// Whether the current row is tried first (the text grew), or the
    /// search starts after it (a new text, or a repeated letter).
    pub from_current: bool,
    /// The text as typed, for the status line.
    pub shown: String,
}

impl Jump {
    /// A key with `text` was typed at `now`. `None` when it does not
    /// belong to a jump: not one printable character, or a space that
    /// would start a new text.
    pub fn typed(&mut self, text: &str, now: Instant) -> Option<Look> {
        let mut chars = text.chars();
        let (Some(c), None) = (chars.next(), chars.next()) else {
            return None;
        };
        // Control characters, and the private-use characters the toolkit
        // gives the arrow and function keys.
        if c.is_control() || ('\u{F700}'..='\u{F8FF}').contains(&c) {
            return None;
        }
        if self.last.is_none_or(|t| now.duration_since(t) >= PAUSE) {
            self.text.clear();
        }
        if c == ' ' && self.text.is_empty() {
            return None;
        }
        self.last = Some(now);
        self.text.push(c);
        let mut letters = self.text.chars();
        let first = letters.next().unwrap_or(c);
        let repeated = letters.all(|l| l == first);
        let wanted = if repeated {
            &self.text[..first.len_utf8()]
        } else {
            &self.text
        };
        Some(Look {
            prefix: folded(wanted.as_bytes()),
            from_current: !repeated,
            shown: self.text.clone(),
        })
    }

    /// Forgets the text (the list lost the focus, or a key moved the
    /// cursor).
    pub fn clear(&mut self) {
        self.text.clear();
        self.last = None;
    }
}

/// A name as jumps compare it: case and diacritics folded.
pub fn folded(name: &[u8]) -> Vec<u8> {
    fold::strip_diacritics(&fold::fold(name))
}

/// The first of `len` rows, from `current` (or after it), wrapping around,
/// for which `starts` holds. `current` -1 starts at the top.
pub fn find(
    len: usize,
    current: Option<usize>,
    from_current: bool,
    mut starts: impl FnMut(usize) -> bool,
) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let first = match current {
        Some(c) if from_current => c.min(len - 1),
        Some(c) => (c + 1) % len,
        None => 0,
    };
    (0..len).map(|i| (first + i) % len).find(|&row| starts(row))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAMES: [&str; 6] = [
        "alpha.txt",
        "Report.pdf",
        "readme.md",
        "Şəkil.png",
        "report 2.pdf",
        "apple",
    ];

    /// The row a jump with `look` from `current` lands on.
    fn land(look: &Look, current: Option<usize>) -> Option<usize> {
        find(NAMES.len(), current, look.from_current, |r| {
            folded(NAMES[r].as_bytes()).starts_with(&look.prefix)
        })
    }

    #[test]
    fn typing_moves_to_the_next_name_that_starts_so() {
        let t = Instant::now();
        let mut j = Jump::default();
        let look = j.typed("r", t).unwrap();
        assert_eq!(land(&look, Some(0)), Some(1));
        // The text grows: the current row is tried first and kept.
        let look = j.typed("e", t).unwrap();
        assert_eq!(look.shown, "re");
        assert_eq!(land(&look, Some(1)), Some(1));
        let look = j.typed("a", t).unwrap();
        assert_eq!(land(&look, Some(1)), Some(2));
        // Wraps around at the end.
        let mut j = Jump::default();
        let look = j.typed("a", t).unwrap();
        assert_eq!(land(&look, Some(5)), Some(0));
        // Case and diacritics are ignored.
        let mut j = Jump::default();
        let look = j.typed("S", t).unwrap();
        assert_eq!(land(&look, None), Some(3));
        // Nothing starts so.
        let mut j = Jump::default();
        let look = j.typed("z", t).unwrap();
        assert_eq!(land(&look, Some(2)), None);
    }

    #[test]
    fn a_repeated_letter_steps_through_its_names() {
        let t = Instant::now();
        let mut j = Jump::default();
        let mut row = Some(0);
        let mut seen = Vec::new();
        for _ in 0..4 {
            let look = j.typed("r", t).unwrap();
            row = land(&look, row);
            seen.push(row.unwrap());
        }
        assert_eq!(seen, [1, 2, 4, 1]);
        assert_eq!(j.typed("r", t).unwrap().shown, "rrrrr");
    }

    #[test]
    fn a_pause_starts_a_new_text() {
        let t = Instant::now();
        let mut j = Jump::default();
        j.typed("r", t).unwrap();
        assert_eq!(j.typed("e", t + PAUSE / 2).unwrap().shown, "re");
        assert_eq!(j.typed("a", t + PAUSE * 2).unwrap().shown, "a");
        j.clear();
        assert_eq!(j.typed("b", t + PAUSE * 2).unwrap().shown, "b");
    }

    #[test]
    fn only_printable_characters_jump() {
        let t = Instant::now();
        let mut j = Jump::default();
        assert_eq!(j.typed("", t), None);
        assert_eq!(j.typed("\u{1b}", t), None);
        assert_eq!(j.typed("\u{F700}", t), None);
        assert_eq!(j.typed("ab", t), None);
        // A space does not start a text, but belongs to one.
        assert_eq!(j.typed(" ", t), None);
        j.typed("r", t).unwrap();
        let look = j.typed(" ", t).unwrap();
        assert_eq!(look.shown, "r ");
        assert_eq!(look.prefix, b"r ");
        // Others than letters jump too.
        let mut j = Jump::default();
        assert_eq!(j.typed("2", t).unwrap().prefix, b"2");
    }

    /// The arrow, page, Home and End keys of the result list forget the
    /// letters typed so far, so the next letter starts a new jump text.
    #[test]
    fn moving_the_cursor_forgets_the_typed_letters() {
        let ui = include_str!("../ui/app.slint");
        let ui = &ui[ui.find("keys := FocusScope").expect("the list's keys")..];
        let keys = ["DownArrow", "UpArrow", "PageDown", "PageUp", "Home", "End"];
        for key in keys {
            let at = ui
                .find(&format!("if (event.text == Key.{key})"))
                .unwrap_or_else(|| panic!("{key} handled"));
            let handler = &ui[at..at + ui[at..].find("return accept;").unwrap()];
            assert!(handler.contains("root.jump-reset();"), "{key}: {handler}");
        }
    }

    #[test]
    fn find_handles_empty_lists_and_no_cursor() {
        assert_eq!(find(0, None, false, |_| true), None);
        assert_eq!(find(3, None, false, |r| r == 2), Some(2));
        assert_eq!(find(3, Some(7), true, |_| true), Some(2));
        assert_eq!(find(3, Some(2), false, |_| true), Some(0));
    }
}
