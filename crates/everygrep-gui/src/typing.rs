//! When the search runs: at every change of the search text or the
//! matching options ("search as you type", the default), or only when
//! Enter is pressed in the search field. A search that reads files
//! (`content:`, duplicates by contents) waits for Enter even while
//! searching as you type, unless that is turned off in Settings, so typing
//! does not read the disk at every key.

use std::cell::RefCell;

use everygrep_core::query::{Query, QueryOptions};

/// Whether the search text with its options, and the named filter chosen
/// with it, read files when they run. A query that does not parse reads
/// nothing (it shows its error). Found without building the expressions,
/// which the search does: this runs at every key.
pub fn reads_files(
    text: &str,
    options: QueryOptions,
    filter: Option<&(String, QueryOptions)>,
) -> bool {
    let reads = |text: &str, options| Query::text_reads_files(text, options);
    reads(text, options) || filter.is_some_and(|(text, options)| reads(text, *options))
}

/// What Enter in the search field does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Enter {
    /// Run the search that was waiting for Enter.
    Search,
    /// The results are up to date: open the current row.
    Open,
}

/// Whether searches follow every change, and whether one is waiting.
#[derive(Debug)]
pub struct Typing {
    as_you_type: bool,
    /// Searches that read files wait for Enter while searching as you
    /// type.
    contents_on_enter: bool,
    waiting: bool,
}

impl Default for Typing {
    fn default() -> Self {
        Self {
            as_you_type: true,
            contents_on_enter: true,
            waiting: false,
        }
    }
}

impl Typing {
    /// The search text or an option changed; `reads_files` tells whether
    /// the new search reads files. Returns whether to search now;
    /// otherwise the search waits for Enter.
    pub fn edited(&mut self, reads_files: bool) -> bool {
        let now = !self.waits(reads_files);
        self.waiting = !now;
        now
    }

    /// Whether a changed search that does (or does not) read files waits
    /// for Enter.
    pub fn waits(&self, reads_files: bool) -> bool {
        !self.as_you_type || self.contents_on_enter && reads_files
    }

    /// Enter was pressed in the search field.
    pub fn enter(&mut self) -> Enter {
        if std::mem::take(&mut self.waiting) {
            Enter::Search
        } else {
            Enter::Open
        }
    }

    /// A search ran for another reason (Back or Forward in the history).
    pub fn searched(&mut self) {
        self.waiting = false;
    }

    pub fn as_you_type(&self) -> bool {
        self.as_you_type
    }

    /// Whether a change is waiting for Enter.
    #[cfg(test)]
    pub fn waiting(&self) -> bool {
        self.waiting
    }

    /// Changes the settings; `reads_files` tells whether the search in the
    /// field reads files. Returns whether a search that was waiting should
    /// run now, because a changed setting no longer makes it wait.
    pub fn set_as_you_type(
        &mut self,
        on: bool,
        contents_on_enter: bool,
        reads_files: bool,
    ) -> bool {
        let changed = (on, contents_on_enter) != (self.as_you_type, self.contents_on_enter);
        self.as_you_type = on;
        self.contents_on_enter = contents_on_enter;
        changed && !self.waits(reads_files) && std::mem::take(&mut self.waiting)
    }
}

thread_local! {
    static TYPING: RefCell<Typing> = RefCell::default();
}

/// Runs `f` with the main window's search state.
pub fn with<R>(f: impl FnOnce(&mut Typing) -> R) -> R {
    TYPING.with(|t| f(&mut t.borrow_mut()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_you_type_searches_at_every_change() {
        let mut t = Typing::default();
        assert!(t.as_you_type());
        assert!(t.edited(false));
        assert!(!t.waiting());
        assert_eq!(t.enter(), Enter::Open);
    }

    #[test]
    fn off_searches_on_enter_only() {
        let mut t = Typing::default();
        assert!(!t.set_as_you_type(false, true, false));
        assert!(!t.as_you_type());
        assert!(!t.edited(false), "typing does not search");
        assert!(!t.edited(false));
        assert!(t.waiting());
        assert_eq!(t.enter(), Enter::Search, "Enter runs the search");
        assert_eq!(t.enter(), Enter::Open, "then Enter opens the current row");
        // A history step searches, so nothing waits any more.
        t.edited(false);
        t.searched();
        assert_eq!(t.enter(), Enter::Open);
    }

    #[test]
    fn turning_it_on_runs_a_waiting_search() {
        let mut t = Typing::default();
        t.set_as_you_type(false, true, false);
        t.edited(false);
        assert!(t.set_as_you_type(true, true, false));
        assert!(!t.waiting());
        assert!(!t.set_as_you_type(true, true, false), "nothing waits now");
    }

    #[test]
    fn searches_that_read_files_wait_for_enter_while_typing() {
        let mut t = Typing::default();
        assert!(t.edited(false), "a name search runs at once");
        assert!(!t.edited(true), "a content search waits");
        assert!(t.waiting());
        // Another setting changing does not run it.
        assert!(!t.set_as_you_type(true, true, true));
        assert_eq!(t.enter(), Enter::Search, "Enter runs it");
        assert_eq!(t.enter(), Enter::Open);
        // Typing on to a name search runs again without Enter.
        t.edited(true);
        assert!(t.edited(false));
        assert!(!t.waiting());
        // Turned off in Settings: a waiting content search runs now, and
        // the next ones follow every change.
        t.edited(true);
        assert!(t.set_as_you_type(true, false, true));
        assert!(t.edited(true));
        // Searching only on Enter: all searches wait.
        t.set_as_you_type(false, false, true);
        assert!(!t.edited(false));
        assert!(t.waits(false) && t.waits(true));
    }

    #[test]
    fn the_query_and_the_filter_tell_whether_files_are_read() {
        let o = QueryOptions::default();
        assert!(reads_files("content:x ext:txt", o, None));
        assert!(reads_files("~/Pictures duplicates:", o, None));
        assert!(!reads_files("duplicates:name", o, None));
        assert!(!reads_files("report", o, None));
        assert!(
            !reads_files("content:x <a", o, None),
            "an error reads nothing"
        );
        let filter = ("content:invoice".to_owned(), o);
        assert!(reads_files("report", o, Some(&filter)));
    }
}
