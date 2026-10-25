//! Name matching: every term of a query must occur somewhere in the name.

use crate::fold::fold;

/// Options that change how a query matches names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MatchOptions {
    /// Compare names exactly instead of case-insensitively.
    pub case_sensitive: bool,
}

/// A compiled query: a list of terms that must all be found in a name.
#[derive(Clone, Debug)]
pub struct Matcher {
    terms: Vec<Vec<u8>>,
    options: MatchOptions,
}

impl Matcher {
    /// Compiles `query`. Terms are separated by whitespace; an empty query matches
    /// every name.
    pub fn new(query: &str, options: MatchOptions) -> Self {
        let terms = query
            .split_whitespace()
            .map(|t| {
                if options.case_sensitive {
                    t.as_bytes().to_vec()
                } else {
                    fold(t.as_bytes())
                }
            })
            .collect();
        Self { terms, options }
    }

    /// Whether the matcher uses folded names.
    pub fn uses_folded_names(&self) -> bool {
        !self.options.case_sensitive
    }

    /// Whether the matcher accepts every name.
    pub fn matches_all(&self) -> bool {
        self.terms.is_empty()
    }

    /// Tests a name. Pass the folded name unless the matcher is case-sensitive.
    pub fn is_match(&self, name: &[u8]) -> bool {
        self.terms.iter().all(|t| contains(name, t))
    }
}

/// Returns whether `needle` occurs in `haystack`.
pub fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    let Some((&first, rest)) = needle.split_first() else {
        return true;
    };
    if haystack.len() < needle.len() {
        return false;
    }
    let last_start = haystack.len() - needle.len();
    let mut i = 0;
    while i <= last_start {
        match haystack[i..=last_start].iter().position(|&b| b == first) {
            None => return false,
            Some(off) => {
                let at = i + off;
                if haystack[at + 1..at + needle.len()] == *rest {
                    return true;
                }
                i = at + 1;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_finds_substrings_anywhere() {
        assert!(contains(b"holiday-photo.jpg", b"photo"));
        assert!(contains(b"abc", b"abc"));
        assert!(contains(b"aab", b"ab"));
        assert!(contains(b"abc", b""));
        assert!(!contains(b"abc", b"abcd"));
        assert!(!contains(b"abc", b"bd"));
    }

    #[test]
    fn terms_are_combined_with_and() {
        let m = Matcher::new("rep pdf", MatchOptions::default());
        assert!(m.is_match(&fold(b"Report.PDF")));
        assert!(!m.is_match(&fold(b"Report.docx")));
    }

    #[test]
    fn middle_of_word_matches() {
        let m = Matcher::new("pintil", MatchOptions::default());
        assert!(m.is_match(&fold(b"linux-tapintilar.md")));
    }

    #[test]
    fn case_sensitive_mode_uses_raw_names() {
        let m = Matcher::new(
            "PDF",
            MatchOptions {
                case_sensitive: true,
            },
        );
        assert!(!m.uses_folded_names());
        assert!(m.is_match(b"Report.PDF"));
        assert!(!m.is_match(b"report.pdf"));
    }

    #[test]
    fn empty_query_matches_all_names() {
        let m = Matcher::new("   ", MatchOptions::default());
        assert!(m.matches_all());
        assert!(m.is_match(b"anything"));
    }
}
