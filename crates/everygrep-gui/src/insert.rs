//! The search field's "Insert" menu: words and operators of the query
//! language (see `everygrep_core::query`), in sections, put in at the text
//! cursor.
//!
//! A part that wraps (a group, a quote, a modifier) goes around the
//! selected text; an operator goes after it, so the selection is kept.

/// One entry of the menu: what goes before the selection (or the cursor)
/// and what after it; an operator is put after the selection instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Part {
    pub open: &'static str,
    pub close: &'static str,
    pub operator: bool,
}

const fn wrap(open: &'static str, close: &'static str) -> Part {
    Part {
        open,
        close,
        operator: false,
    }
}

const fn operator(open: &'static str) -> Part {
    Part {
        open,
        close: "",
        operator: true,
    }
}

/// Where to look: the kind of entry, its type, contents and place.
pub const WHERE: [Part; 11] = [
    wrap("file:", ""),
    wrap("folder:", ""),
    wrap("ext:", ""),
    wrap("type:", ""),
    wrap("attributes:", ""),
    wrap("content:", ""),
    wrap("path:", ""),
    wrap("parent:", ""),
    wrap("child:", ""),
    wrap("depth:", ""),
    wrap("children:", ""),
];

/// Dates, sizes and the name length.
pub const WHEN_AND_SIZE: [Part; 8] = [
    wrap("modified:<", ""),
    wrap("modified:>", ""),
    wrap("modified:today", ""),
    wrap("created:", ""),
    wrap("accessed:", ""),
    wrap("size:>", ""),
    wrap("size:<", ""),
    wrap("len:", ""),
];

/// How a term matches: what is compared, where it must match, regex.
pub const HOW: [Part; 10] = [
    wrap("case:", ""),
    wrap("nocase:", ""),
    wrap("diacritics:", ""),
    wrap("nopunct:", ""),
    wrap("nospace:", ""),
    wrap("wholeword:", ""),
    wrap("wholename:", ""),
    wrap("prefix:", ""),
    wrap("suffix:", ""),
    wrap("regex:", ""),
];

/// Combining terms: either or, not, a group and exact text. A space (both
/// terms) needs no entry.
pub const COMBINE: [Part; 4] = [
    operator("|"),
    wrap("!", ""),
    wrap("<", ">"),
    wrap("\"", "\""),
];

/// Settings for the whole result list.
pub const RESULTS: [Part; 2] = [wrap("limit:", ""), wrap("duplicates:", "")];

/// The part at `index` of a section of the menu (0 where, 1 when and size,
/// 2 how, 3 combine, 4 results).
pub fn part(section: i32, index: i32) -> Option<Part> {
    let list: &[Part] = match section {
        0 => &WHERE,
        1 => &WHEN_AND_SIZE,
        2 => &HOW,
        3 => &COMBINE,
        4 => &RESULTS,
        _ => return None,
    };
    list.get(usize::try_from(index).ok()?).copied()
}

/// The nearest character boundary at or before `offset`.
fn boundary(text: &str, offset: usize) -> usize {
    let mut at = offset.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// `text` with `part` put in at the selection from `anchor` to `cursor`
/// (byte offsets, in either order; equal for no selection), and the byte
/// offset for the cursor after it: inside an empty pair, after a wrapped
/// selection, after an operator.
pub fn insert(text: &str, anchor: usize, cursor: usize, part: Part) -> (String, usize) {
    let (a, b) = (boundary(text, anchor), boundary(text, cursor));
    let (start, end) = (a.min(b), a.max(b));
    let mut out = String::with_capacity(text.len() + part.open.len() + part.close.len());
    if part.operator {
        out.push_str(&text[..end]);
        out.push_str(part.open);
        let caret = out.len();
        out.push_str(&text[end..]);
        return (out, caret);
    }
    out.push_str(&text[..start]);
    // With no selection this starts a new term. Preserve modifier chains
    // and the boundaries provided by a group, OR or NOT operator.
    let separate = start == end;
    let new_term = separate
        && text[..start]
            .chars()
            .next_back()
            .is_some_and(|c| !c.is_whitespace() && !matches!(c, '<' | '|' | '!' | ':'));
    if new_term {
        out.push(' ');
    }
    out.push_str(part.open);
    let inside = out.len();
    out.push_str(&text[start..end]);
    out.push_str(part.close);
    let caret = if start == end { inside } else { out.len() };
    if new_term
        && text[end..]
            .chars()
            .next()
            .is_some_and(|c| !c.is_whitespace() && !matches!(c, '>' | '|'))
    {
        out.push(' ');
    }
    out.push_str(&text[end..]);
    (out, caret)
}

#[cfg(test)]
mod tests {
    use super::*;
    use everygrep_core::query::{Query, QueryOptions};

    #[test]
    fn a_wrapping_part_goes_around_the_selection() {
        let group = wrap("<", ">");
        assert_eq!(insert("a b", 2, 3, group), ("a <b>".into(), 5));
        // Selected from right to left: the same.
        assert_eq!(insert("a b", 3, 2, group), ("a <b>".into(), 5));
        // No selection: an empty pair with the cursor inside.
        assert_eq!(insert("ab", 1, 1, group), ("a <> b".into(), 3));
        assert_eq!(insert("", 0, 0, wrap("case:", "")), ("case:".into(), 5));
        assert_eq!(
            insert("report", 0, 6, wrap("case:", "")),
            ("case:report".into(), 11)
        );
    }

    #[test]
    fn inserted_terms_have_boundaries_and_leave_the_caret_in_the_new_term() {
        assert_eq!(
            insert("case:beach", 10, 10, WHERE[2]),
            ("case:beach ext:".into(), 15)
        );
        assert_eq!(
            insert("case:beach ", 11, 11, WHERE[2]),
            ("case:beach ext:".into(), 15)
        );
        assert_eq!(insert("əx", 2, 2, WHERE[2]), ("ə ext: x".into(), 7));
        assert_eq!(insert("<", 1, 1, HOW[0]), ("<case:".into(), 6));
        assert_eq!(insert("case:", 5, 5, WHERE[2]), ("case:ext:".into(), 9));
        assert_eq!(insert("x>", 1, 1, WHERE[2]), ("x ext:>".into(), 6));
        assert_eq!(insert("a|", 2, 2, WHERE[2]), ("a|ext:".into(), 6));
        // A selection is wrapped, retaining its existing term boundaries.
        assert_eq!(insert("beach", 0, 5, HOW[0]), ("case:beach".into(), 10));
        assert!(Query::parse("case:beach ext:jpg", QueryOptions::default()).is_ok());
    }

    #[test]
    fn an_operator_goes_after_the_selection() {
        let or = operator("|");
        assert_eq!(insert("jpg", 0, 3, or), ("jpg|".into(), 4));
        assert_eq!(insert("jpg png", 3, 3, or), ("jpg| png".into(), 4));
    }

    #[test]
    fn offsets_inside_a_character_or_past_the_end_are_safe() {
        // "ə" is two bytes: offset 1 lies inside it.
        assert_eq!(insert("ə", 1, 1, wrap("!", "")), ("!ə".into(), 1));
        assert_eq!(insert("ab", 99, 99, operator("|")), ("ab|".into(), 3));
        assert_eq!(insert("şə", 3, 99, wrap("\"", "\"")), ("ş\"ə\"".into(), 6));
    }

    #[test]
    fn every_entry_is_known_to_the_query_language() {
        for section in 0..5 {
            let mut i = 0;
            while let Some(p) = part(section, i) {
                let text = format!("{}x{}", p.open, p.close);
                let text = if p.operator { format!("a{text}") } else { text };
                // A value the function takes, where it needs one.
                let text = match p.open {
                    "size:>" | "size:<" => format!("{}1mb", p.open),
                    "modified:>" | "modified:<" => format!("{}7d", p.open),
                    "created:" | "accessed:" => format!("{}today", p.open),
                    "limit:" => "limit:5".into(),
                    "duplicates:" => "duplicates:".into(),
                    "modified:today" => "modified:today".into(),
                    "type:" => "type:image".into(),
                    "attributes:" => "attributes:R".into(),
                    "depth:" => "depth:2".into(),
                    "children:" => "children:0".into(),
                    "len:" => "len:>20".into(),
                    _ => text,
                };
                assert!(
                    Query::parse(&text, QueryOptions::default()).is_ok(),
                    "{text}"
                );
                i += 1;
            }
            assert!(i > 0);
        }
        assert_eq!(part(5, 0), None);
        assert_eq!(part(0, -1), None);
    }
}
