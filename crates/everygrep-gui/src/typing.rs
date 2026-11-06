//! When the search runs: at every change of the search text or the
//! matching options ("search as you type", the default), or only when
//! Enter is pressed in the search field. A search that reads files
//! (`content:`, duplicates by contents) waits for Enter even while
//! searching as you type, unless that is turned off in Settings, so typing
//! does not read the disk at every key.
//!
//! A search delay (Settings, 0 by default) holds back a search that would
//! run as you type until no key was pressed for that long; Enter runs it at
//! once.

use std::cell::RefCell;
use std::time::Duration;

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
    /// The current query has been submitted but its rows have not arrived.
    Wait,
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
    /// How long a search as you type waits for the next key; zero runs it
    /// at once.
    delay: Duration,
    /// A search held back by the delay.
    delayed: bool,
}

impl Default for Typing {
    fn default() -> Self {
        Self {
            as_you_type: true,
            contents_on_enter: true,
            waiting: false,
            delay: Duration::ZERO,
            delayed: false,
        }
    }
}

impl Typing {
    /// The search text or an option changed; `reads_files` tells whether
    /// the new search reads files. Returns whether to search now;
    /// otherwise the search waits for Enter.
    pub fn edited(&mut self, reads_files: bool) -> bool {
        self.delayed = false;
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
    pub fn enter(&mut self, results_current: bool) -> Enter {
        let delayed = std::mem::take(&mut self.delayed);
        if std::mem::take(&mut self.waiting) || delayed {
            Enter::Search
        } else if !results_current {
            Enter::Wait
        } else {
            Enter::Open
        }
    }

    /// A search ran for another reason (Back or Forward in the history).
    pub fn searched(&mut self) {
        self.waiting = false;
        self.delayed = false;
    }

    /// Sets the search delay in milliseconds (0 for none).
    pub fn set_delay(&mut self, ms: u32) {
        self.delay = Duration::from_millis(u64::from(ms));
    }

    /// After [`edited`](Self::edited) said to search now: how long to hold
    /// the search back, `None` to run it at once. A held search is marked,
    /// so Enter runs it and [`due`](Self::due) knows it is still wanted.
    pub fn hold(&mut self) -> Option<Duration> {
        if self.delay.is_zero() {
            return None;
        }
        self.delayed = true;
        Some(self.delay)
    }

    /// The delay ran out: whether the held search should run now (it did
    /// not run meanwhile for another reason).
    pub fn due(&mut self) -> bool {
        std::mem::take(&mut self.delayed)
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
    fn enter_searches_a_delayed_query_and_blocks_old_rows_until_delivery() {
        let mut t = Typing::default();
        t.set_delay(1000);
        assert!(t.edited(false));
        assert!(t.hold().is_some());
        assert_eq!(t.enter(false), Enter::Search);
        assert!(!t.due(), "Enter cancels the delayed submission");
        assert_eq!(t.enter(false), Enter::Wait, "old rows cannot open");
        assert_eq!(t.enter(true), Enter::Open, "new rows may open");
        // Zero delay still needs to wait for the engine's new rows.
        t.set_delay(0);
        assert!(t.edited(false));
        assert!(t.hold().is_none());
        assert_eq!(t.enter(false), Enter::Wait);
        assert_eq!(t.enter(true), Enter::Open);
    }

    #[test]
    fn as_you_type_searches_at_every_change() {
        let mut t = Typing::default();
        assert!(t.as_you_type());
        assert!(t.edited(false));
        assert!(!t.waiting());
        assert_eq!(t.enter(true), Enter::Open);
    }

    #[test]
    fn off_searches_on_enter_only() {
        let mut t = Typing::default();
        assert!(!t.set_as_you_type(false, true, false));
        assert!(!t.as_you_type());
        assert!(!t.edited(false), "typing does not search");
        assert!(!t.edited(false));
        assert!(t.waiting());
        assert_eq!(t.enter(true), Enter::Search, "Enter runs the search");
        assert_eq!(
            t.enter(true),
            Enter::Open,
            "then Enter opens the current row"
        );
        // A history step searches, so nothing waits any more.
        t.edited(false);
        t.searched();
        assert_eq!(t.enter(true), Enter::Open);
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
        assert_eq!(t.enter(true), Enter::Search, "Enter runs it");
        assert_eq!(t.enter(true), Enter::Open);
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
    fn no_delay_searches_at_once() {
        let mut t = Typing::default();
        assert!(t.edited(false));
        assert_eq!(t.hold(), None);
        assert!(!t.due());
        assert_eq!(t.enter(true), Enter::Open);
    }

    #[test]
    fn a_delay_holds_the_search_until_it_runs_out_or_enter() {
        let mut t = Typing::default();
        t.set_delay(300);
        assert!(t.edited(false));
        assert_eq!(t.hold(), Some(Duration::from_millis(300)));
        // The next key holds it again; the timer of the first is stale.
        assert!(t.edited(false));
        assert_eq!(t.hold(), Some(Duration::from_millis(300)));
        assert!(t.due(), "the delay ran out: search");
        assert!(!t.due(), "only once");
        assert_eq!(t.enter(true), Enter::Open, "nothing held: Enter opens");
        // Enter runs a held search at once; the timer then finds nothing.
        t.edited(false);
        t.hold();
        assert_eq!(t.enter(true), Enter::Search);
        assert!(!t.due());
        // A search for another reason (history, options) drops the held one.
        t.edited(false);
        t.hold();
        t.searched();
        assert!(!t.due());
        // A content search that waits for Enter is not held.
        t.edited(false);
        t.hold();
        assert!(!t.edited(true));
        assert!(!t.due());
        assert_eq!(t.enter(true), Enter::Search);
        t.set_delay(0);
        assert!(t.edited(false));
        assert_eq!(t.hold(), None);
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
