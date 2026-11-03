//! One window's search tabs. Background tabs own frozen state, never workers.
use crate::history::History;
use crate::results::ResultSnapshot;
use crate::saved_searches::NamedFilter;
use crate::settings::Columns;
use everygrep_core::query::QueryOptions;
use std::collections::HashSet;

pub const CLOSED_LIMIT: usize = 10;

/// A tab remembers match toggles, never the window's Settings-level policy.
/// Canonical placeholders keep policy changes out of search-state comparisons.
pub fn tab_options(options: QueryOptions) -> QueryOptions {
    with_policy(options, QueryOptions::default())
}

pub fn with_policy(mut options: QueryOptions, policy: QueryOptions) -> QueryOptions {
    options.hidden_files = policy.hidden_files;
    options.and_before_or = policy.and_before_or;
    options.literal_operators = policy.literal_operators;
    options.wildcards_anywhere = policy.wildcards_anywhere;
    options
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SearchState {
    pub query: String,
    pub options: QueryOptions,
    pub filter: Option<NamedFilter>,
    pub filter_snapshot: bool,
    pub sort_column: i32,
    pub descending: bool,
    pub columns: Columns,
}

impl SearchState {
    pub fn effective_options(
        &self,
        policy: QueryOptions,
        filter: Option<&NamedFilter>,
    ) -> QueryOptions {
        let mut options = with_policy(self.options, policy);
        if let Some(filter) = filter {
            options.filter = filter.options.filter;
        }
        options
    }

    pub fn launched(
        mut self,
        query: &str,
        options: QueryOptions,
        keep_filter: bool,
        columns: &Columns,
    ) -> Self {
        self.query = query.into();
        self.options = tab_options(options);
        if !keep_filter {
            self.filter = None;
            self.filter_snapshot = false;
        }
        self.columns.show_type |= columns.show_type;
        self.columns.show_ext |= columns.show_ext;
        self.columns.show_created |= columns.show_created;
        self.columns.show_accessed |= columns.show_accessed;
        self.columns.show_attrs |= columns.show_attrs;
        self
    }
}

#[derive(Clone, Default)]
pub struct Tab {
    pub search: SearchState,
    pub folders: crate::folders::View,
    // Runtime data is deliberately absent from the session file.
    pub result: Option<ResultSnapshot>,
    pub submitted: Option<SearchState>,
    pub pending: bool,
    pub history: History,
    pub current: i32,
    pub scroll_y: f32,
    /// How far the result table was scrolled sideways (0 or less).
    pub scroll_x: f32,
    /// The tab's content search, when the user stopped it: shown again, it
    /// stays stopped with its matches so far.
    pub stopped: Option<crate::engine::StoppedSearch>,
    /// The tab's search, when its complete results were shown: shown again
    /// while the index is unchanged, they are not searched again.
    pub settled: Option<crate::engine::SettledSearch>,
    /// When the tab was last shown ([`Tabs::shown`]); runtime only.
    pub shown_at: u64,
}

#[derive(Clone)]
pub struct Tabs {
    pub open: Vec<Tab>,
    pub active: usize,
    /// Oldest first; reopening removes an entry from this list.
    pub closed: Vec<Tab>,
    pub recording: bool,
    /// Cleared/deleted searches stay visible in open tabs but must not return to disk.
    pub forgotten: HashSet<String>,
    /// Counts the times a tab was shown, for [`Tab::shown_at`].
    pub shown: u64,
}

impl Default for Tabs {
    fn default() -> Self {
        Self {
            open: vec![Tab::default()],
            active: 0,
            closed: Vec::new(),
            recording: true,
            forgotten: HashSet::new(),
            shown: 0,
        }
    }
}

impl Tabs {
    pub fn same_layout(&self, other: &Self) -> bool {
        self.active == other.active
            && self
                .open
                .iter()
                .map(|t| &t.search)
                .eq(other.open.iter().map(|t| &t.search))
            && self
                .closed
                .iter()
                .map(|t| &t.search)
                .eq(other.closed.iter().map(|t| &t.search))
    }

    pub fn history_policy(&mut self, max: usize, clear: bool) {
        self.recording = max > 0;
        if clear || !self.recording {
            self.forgotten.extend(
                self.open
                    .iter()
                    .chain(self.closed.iter())
                    .flat_map(|t| std::iter::once(&t.search).chain(t.submitted.iter()))
                    .map(|s| s.query.clone()),
            );
            self.closed.clear();
        }
        self.histories(|h| {
            if clear {
                h.set_max(0);
            }
            h.set_max(max);
        });
    }

    pub fn remove_history(&mut self, query: &str) {
        self.forgotten.insert(query.into());
        self.closed.retain(|t| t.search.query != query);
        self.histories(|h| h.remove(query));
    }

    pub fn histories(&mut self, change: impl Fn(&mut History)) {
        for tab in self.open.iter_mut().chain(self.closed.iter_mut()) {
            change(&mut tab.history);
        }
    }

    /// The active tab is shown now. Of the other tabs, the `keep` shown
    /// last keep their results; the rest give theirs up and search again
    /// when shown, except a content search the user stopped, whose matches
    /// so far cannot be found again without reading the files.
    pub fn shown(&mut self, keep: usize) {
        self.shown += 1;
        let active = self.active;
        self.open[active].shown_at = self.shown;
        let mut held: Vec<usize> = (0..self.open.len())
            .filter(|&i| {
                i != active && self.open[i].result.is_some() && self.open[i].stopped.is_none()
            })
            .collect();
        held.sort_unstable_by_key(|&i| std::cmp::Reverse(self.open[i].shown_at));
        for &i in held.iter().skip(keep) {
            self.open[i].result = None;
            self.open[i].settled = None;
        }
    }

    pub fn add(&mut self, tab: Tab, activate: bool) {
        self.open.push(tab);
        if activate {
            self.active = self.open.len() - 1;
        }
    }

    pub fn duplicate(&mut self) {
        let index = self.active + 1;
        self.open.insert(index, self.open[self.active].clone());
        self.active = index;
    }

    pub fn select(&mut self, index: usize) {
        if index < self.open.len() {
            self.active = index;
        }
    }

    pub fn step(&mut self, forward: bool) {
        let len = self.open.len();
        self.active = (self.active + if forward { 1 } else { len - 1 }) % len;
    }

    /// A batch uses original indexes. Invalid and repeated indexes are harmless.
    /// Keep the same active tab if it survives, otherwise its nearest neighbour.
    pub fn close(&mut self, indexes: &[usize]) {
        let mut indexes: Vec<_> = indexes
            .iter()
            .copied()
            .filter(|i| *i < self.open.len())
            .collect();
        indexes.sort_unstable();
        indexes.dedup();
        let mut replacement = self.open[self.active].search.clone();
        replacement.query.clear();
        let mut closed = Vec::new();
        for &i in indexes.iter().rev() {
            let tab = self.open.remove(i);
            // Closed tabs retain searches, history and layout, but release results.
            if self.recording
                && !self.forgotten.contains(&tab.search.query)
                && !tab.search.query.is_empty()
            {
                closed.push(Tab {
                    result: None,
                    ..tab
                });
            }
            if i < self.active {
                self.active -= 1;
            }
        }
        self.closed.extend(closed.into_iter().rev());
        let extra = self.closed.len().saturating_sub(CLOSED_LIMIT);
        self.closed.drain(..extra);
        if self.open.is_empty() {
            self.open.push(Tab {
                search: replacement,
                ..Default::default()
            });
        }
        self.active = self.active.min(self.open.len() - 1);
    }

    pub fn reopen(&mut self, index: usize) {
        if index < self.closed.len() {
            let tab = self.closed.remove(index);
            self.add(tab, true);
        }
    }

    pub fn reopen_all(&mut self) {
        let closed = std::mem::take(&mut self.closed);
        for tab in closed {
            self.add(tab, true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn filled(n: usize) -> Tabs {
        Tabs {
            open: (0..n)
                .map(|i| Tab {
                    search: SearchState {
                        query: i.to_string(),
                        ..Default::default()
                    },
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }
    fn names(t: &Tabs) -> Vec<&str> {
        t.open.iter().map(|t| t.search.query.as_str()).collect()
    }

    #[test]
    fn global_policy_is_projected_out_and_overlaid_at_activation() {
        use everygrep_core::query::HiddenFiles;
        let saved = QueryOptions {
            regex: true,
            case_sensitive: true,
            hidden_files: HiddenFiles::Only,
            and_before_or: true,
            literal_operators: true,
            wildcards_anywhere: true,
            ..Default::default()
        };
        let local = tab_options(saved);
        assert!(local.regex && local.case_sensitive);
        assert_eq!(
            local,
            tab_options(with_policy(saved, QueryOptions::default()))
        );
        let policy = QueryOptions {
            hidden_files: HiddenFiles::Show,
            ..saved
        };
        let effective = with_policy(local, policy);
        assert_eq!(effective, policy);
        assert_eq!(with_policy(local, QueryOptions::default()), local);
    }

    #[test]
    fn last_close_clears_only_query_and_repeated_blank_closes_keep_real_searches() {
        let mut t = filled(1);
        t.open[0].search.options.case_sensitive = true;
        t.open[0].search.sort_column = 7;
        t.open[0].search.descending = true;
        t.open[0].search.columns.show_type = true;
        t.open[0].search.columns.name = Some(417.0);
        let mut expected = t.open[0].search.clone();
        expected.query.clear();
        t.close(&[0]);
        assert_eq!(t.open[0].search, expected);
        for _ in 0..20 {
            t.close(&[0]);
        }
        assert_eq!(t.open[0].search, expected);
        assert_eq!(t.closed.len(), 1);
        assert_eq!(t.closed[0].search.query, "0");
    }

    #[test]
    fn privacy_controls_remove_closed_queries_and_disable_closed_recording() {
        let mut t = filled(3);
        t.close(&[0, 1]);
        t.remove_history("0");
        assert_eq!(t.closed.len(), 1);
        assert_eq!(t.closed[0].search.query, "1");
        assert!(t.forgotten.contains("0"));
        t.history_policy(10, true);
        assert!(t.closed.is_empty());
        assert!(t.forgotten.contains("1") && t.forgotten.contains("2"));
        t.history_policy(0, false);
        t.open[0].search.query = "private".into();
        t.close(&[0]);
        assert!(t.closed.is_empty());
    }

    #[test]
    fn no_op_navigation_keeps_the_layout() {
        let t = filled(1);
        for action in 0..6 {
            let mut next = t.clone();
            match action {
                0 => next.select(0),
                1 => next.select(4),
                2 => next.step(true),
                3 => next.close(&[]),
                4 => next.reopen(0),
                _ => next.reopen_all(),
            }
            assert!(t.same_layout(&next));
        }
        let mut next = t.clone();
        next.add(next.open[0].clone(), true);
        assert!(!t.same_layout(&next));
    }

    #[test]
    fn batch_reopen_preserves_reading_order() {
        let mut t = filled(5);
        t.close(&[1, 2, 3, 4]);
        assert_eq!(t.closed.last().unwrap().search.query, "4");
        t.reopen_all();
        assert_eq!(names(&t), ["0", "1", "2", "3", "4"]);
    }

    #[test]
    fn history_policy_and_deletion_reach_background_and_closed_tabs() {
        let mut t = filled(3);
        t.histories(|h| {
            h.record("first");
            h.record("second");
        });
        t.close(&[0]);
        t.histories(|h| h.set_max(1));
        for tab in t.open.iter_mut().chain(t.closed.iter_mut()) {
            assert_eq!(tab.history.back("second"), None);
        }
        t.histories(|h| h.remove("second"));
        for tab in t.open.iter_mut().chain(t.closed.iter_mut()) {
            assert_eq!(tab.history.back(""), None);
        }
        t.histories(|h| {
            h.record("new");
            h.set_max(0);
            h.set_max(10);
        });
        for tab in t.open.iter_mut().chain(t.closed.iter_mut()) {
            assert_eq!(tab.history.back(""), None);
        }
    }

    #[test]
    fn new_duplicate_select_and_cycle_keep_independent_searches() {
        let mut t = filled(3);
        t.select(1);
        t.open[1].search.options.regex = true;
        t.add(t.open[1].clone(), true);
        t.open[3].search.query.push('x');
        assert_eq!(names(&t), ["0", "1", "2", "1x"]);
        assert!(t.open[3].search.options.regex);
        t.step(true);
        assert_eq!(t.active, 0);
        t.step(false);
        assert_eq!(t.active, 3);
        t.select(100);
        assert_eq!(t.active, 3);
    }
    #[test]
    fn duplicate_is_inserted_after_its_source() {
        for source in 0..3 {
            let mut tabs = filled(3);
            tabs.select(source);
            tabs.duplicate();
            assert_eq!(tabs.active, source + 1);
            assert_eq!(tabs.open[tabs.active].search, tabs.open[source].search);
            let mut expected = vec!["0".to_owned(), "1".to_owned(), "2".to_owned()];
            expected.insert(source + 1, source.to_string());
            assert_eq!(names(&tabs), expected);
            tabs.open[tabs.active].search.query.push('x');
            assert_eq!(tabs.open[source].search.query, source.to_string());
        }
    }

    #[test]
    fn bulk_close_preserves_active_and_handles_duplicates() {
        let mut t = filled(6);
        t.select(3);
        t.close(&[0, 1, 1, 5, 99]);
        assert_eq!(names(&t), ["2", "3", "4"]);
        assert_eq!(t.active, 1);
        t.close(&[1]);
        assert_eq!(names(&t), ["2", "4"]);
        assert_eq!(t.active, 1);
        t.close(&[0, 1]);
        assert_eq!(t.open.len(), 1);
        assert_eq!(t.active, 0);
        assert!(t.open[0].search.query.is_empty());
    }
    #[test]
    fn close_left_right_other_use_original_positions() {
        for (indexes, expected, active) in [
            (vec![0, 1], vec!["2", "3", "4"], 0),
            (vec![3, 4], vec!["0", "1", "2"], 2),
            (vec![0, 1, 3, 4], vec!["2"], 0),
        ] {
            let mut t = filled(5);
            t.select(2);
            t.close(&indexes);
            assert_eq!(names(&t), expected);
            assert_eq!(t.active, active);
        }
    }
    #[test]
    fn closed_memory_keeps_ten_and_reopen_consumes_it() {
        let mut t = filled(15);
        for _ in 0..14 {
            t.close(&[0]);
        }
        assert_eq!(t.closed.len(), CLOSED_LIMIT);
        assert_eq!(t.closed[0].search.query, "4");
        t.reopen(9);
        assert_eq!(t.open[t.active].search.query, "13");
        assert_eq!(t.closed.len(), 9);
        t.reopen(99);
        assert_eq!(t.closed.len(), 9);
        t.reopen_all();
        assert!(t.closed.is_empty());
        assert_eq!(t.open.len(), 11);
    }
}
