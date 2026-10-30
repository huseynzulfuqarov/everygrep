//! Search history for Back and Forward, kept in memory for the session.
//!
//! A query is recorded when the user pauses typing, presses Enter or opens a
//! result. Like a web browser, recording after going back drops the entries
//! ahead. Empty queries and a repeat of the current entry are not recorded.

/// Most entries kept; the oldest go first.
const MAX_ENTRIES: usize = 200;

#[derive(Debug, Default)]
pub struct History {
    entries: Vec<String>,
    /// Index of the entry shown now, if any.
    pos: Option<usize>,
}

impl History {
    /// Records `query` as the newest entry after the current one.
    pub fn record(&mut self, query: &str) {
        if query.is_empty() || self.current() == Some(query) {
            return;
        }
        if let Some(pos) = self.pos {
            self.entries.truncate(pos + 1);
        }
        self.entries.push(query.to_owned());
        if self.entries.len() > MAX_ENTRIES {
            self.entries.remove(0);
        }
        self.pos = Some(self.entries.len() - 1);
    }

    /// The query to show for Back, given the text now in the search field.
    /// Text that was typed but not yet recorded is recorded first, so Forward
    /// returns to it. With an empty field, Back shows the current entry.
    pub fn back(&mut self, shown: &str) -> Option<&str> {
        self.record(shown);
        let pos = self.pos?;
        if self.entries[pos] == shown {
            self.pos = Some(pos.checked_sub(1)?);
        }
        self.current()
    }

    /// The query to show for Forward, given the text now in the search field.
    pub fn forward(&mut self, shown: &str) -> Option<&str> {
        self.record(shown);
        let pos = self.pos?;
        if pos + 1 >= self.entries.len() {
            return None;
        }
        self.pos = Some(pos + 1);
        self.current()
    }

    fn current(&self) -> Option<&str> {
        self.pos.map(|p| self.entries[p].as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(entries: &[&str]) -> History {
        let mut h = History::default();
        entries.iter().for_each(|e| h.record(e));
        h
    }

    #[test]
    fn back_and_forward_walk_the_entries() {
        let mut h = with(&["a", "b", "c"]);
        assert_eq!(h.back("c"), Some("b"));
        assert_eq!(h.back("b"), Some("a"));
        assert_eq!(h.back("a"), None);
        assert_eq!(h.forward("a"), Some("b"));
        assert_eq!(h.forward("b"), Some("c"));
        assert_eq!(h.forward("c"), None);
    }

    #[test]
    fn empty_and_repeated_queries_are_not_recorded() {
        let mut h = with(&["a", "a", "", "b", "b"]);
        assert_eq!(h.entries, ["a", "b"]);
        h.record("a");
        assert_eq!(h.entries, ["a", "b", "a"]);
    }

    #[test]
    fn unrecorded_text_is_kept_for_forward() {
        let mut h = with(&["a", "b"]);
        assert_eq!(h.back("b-typed"), Some("b"));
        assert_eq!(h.forward("b"), Some("b-typed"));
    }

    #[test]
    fn back_from_an_empty_field_shows_the_last_entry() {
        let mut h = with(&["a", "b"]);
        assert_eq!(h.back(""), Some("b"));
        assert_eq!(h.back("b"), Some("a"));
        assert_eq!(History::default().back(""), None);
        assert_eq!(History::default().forward(""), None);
    }

    #[test]
    fn recording_after_back_drops_the_entries_ahead() {
        let mut h = with(&["a", "b", "c"]);
        h.back("c");
        h.back("b");
        h.record("x");
        assert_eq!(h.entries, ["a", "x"]);
        assert_eq!(h.forward("x"), None);
    }

    #[test]
    fn oldest_entries_go_first() {
        let mut h = History::default();
        for i in 0..MAX_ENTRIES + 5 {
            h.record(&i.to_string());
        }
        assert_eq!(h.entries.len(), MAX_ENTRIES);
        assert_eq!(h.entries[0], "5");
    }
}
