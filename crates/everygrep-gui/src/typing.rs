//! When the search runs: at every change of the search text or the
//! matching options ("search as you type", the default), or only when
//! Enter is pressed in the search field.

use std::cell::RefCell;

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
    waiting: bool,
}

impl Default for Typing {
    fn default() -> Self {
        Self {
            as_you_type: true,
            waiting: false,
        }
    }
}

impl Typing {
    /// The search text or an option changed. Returns whether to search
    /// now; otherwise the search waits for Enter.
    pub fn edited(&mut self) -> bool {
        self.waiting = !self.as_you_type;
        self.as_you_type
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

    /// Changes the setting. Returns whether a search that was waiting
    /// should run now, because searching follows changes again.
    pub fn set_as_you_type(&mut self, on: bool) -> bool {
        self.as_you_type = on;
        on && std::mem::take(&mut self.waiting)
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
        assert!(t.edited());
        assert!(!t.waiting());
        assert_eq!(t.enter(), Enter::Open);
    }

    #[test]
    fn off_searches_on_enter_only() {
        let mut t = Typing::default();
        assert!(!t.set_as_you_type(false));
        assert!(!t.as_you_type());
        assert!(!t.edited(), "typing does not search");
        assert!(!t.edited());
        assert!(t.waiting());
        assert_eq!(t.enter(), Enter::Search, "Enter runs the search");
        assert_eq!(t.enter(), Enter::Open, "then Enter opens the current row");
        // A history step searches, so nothing waits any more.
        t.edited();
        t.searched();
        assert_eq!(t.enter(), Enter::Open);
    }

    #[test]
    fn turning_it_on_runs_a_waiting_search() {
        let mut t = Typing::default();
        t.set_as_you_type(false);
        t.edited();
        assert!(t.set_as_you_type(true));
        assert!(!t.waiting());
        assert!(!t.set_as_you_type(true), "nothing waits now");
    }
}
