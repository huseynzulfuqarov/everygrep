//! Which result rows are selected.
//!
//! The selection holds node ids, not row numbers, so it stays on the same
//! files when the list is sorted again or refreshed. "Select all" is kept as a
//! flag with a list of exceptions, so it costs nothing however many results
//! there are.

use std::collections::HashSet;

use everygrep_core::store::NodeId;

/// Selection change sets up to this size are reported row by row; bigger
/// changes are reported as "all rows may have changed".
const MAX_ROW_CHANGES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Set {
    /// Exactly these ids.
    Only(HashSet<NodeId>),
    /// Every row of the list except these ids.
    AllExcept(HashSet<NodeId>),
}

/// Rows whose selected state changed.
#[derive(Debug, PartialEq, Eq)]
pub enum Changed {
    None,
    /// These rows, in ascending order.
    Rows(Vec<usize>),
    /// Too many rows to list; refresh them all.
    All,
}

/// The selection over a list of node ids, plus the anchor of Shift ranges.
#[derive(Clone, Debug)]
pub struct Selection {
    set: Set,
    anchor: Option<NodeId>,
    /// Where the anchor was last seen, and the rows the selected ids took
    /// when they were one run (a click or a Shift range): hints that spare a
    /// search through a long list, checked before use since the list may
    /// have changed since.
    anchor_row: Option<usize>,
    run: Option<(usize, usize)>,
}

impl Default for Selection {
    fn default() -> Self {
        Self {
            set: Set::Only(HashSet::new()),
            anchor: None,
            anchor_row: None,
            run: None,
        }
    }
}

impl Selection {
    pub fn contains(&self, id: NodeId) -> bool {
        match &self.set {
            Set::Only(s) => s.contains(&id),
            Set::AllExcept(s) => !s.contains(&id),
        }
    }

    /// Number of selected rows of a list of `len` rows. Assumes [`retain`]
    /// was called for that list.
    ///
    /// [`retain`]: Selection::retain
    pub fn count(&self, len: usize) -> usize {
        match &self.set {
            Set::Only(s) => s.len(),
            Set::AllExcept(s) => len.saturating_sub(s.len()),
        }
    }

    /// Selected ids in list order.
    pub fn selected(&self, ids: &[NodeId]) -> Vec<NodeId> {
        match &self.set {
            Set::Only(s) if s.is_empty() => Vec::new(),
            Set::Only(s) if s.len() == 1 => s.iter().copied().collect(),
            _ => ids
                .iter()
                .copied()
                .filter(|&id| self.contains(id))
                .collect(),
        }
    }

    /// Selects only the row `row`.
    pub fn select_only(&mut self, ids: &[NodeId], row: usize) -> Changed {
        let Some(&id) = ids.get(row) else {
            return Changed::None;
        };
        self.anchor = Some(id);
        self.anchor_row = Some(row);
        self.replace(ids, Set::Only(HashSet::from([id])), Some((row, row)))
    }

    /// Adds the row to the selection or removes it.
    pub fn toggle(&mut self, ids: &[NodeId], row: usize) -> Changed {
        let Some(&id) = ids.get(row) else {
            return Changed::None;
        };
        self.anchor = Some(id);
        self.anchor_row = Some(row);
        self.run = None;
        let set = match &mut self.set {
            Set::Only(s) | Set::AllExcept(s) => s,
        };
        if !set.remove(&id) {
            set.insert(id);
        }
        Changed::Rows(vec![row])
    }

    /// Selects the rows from the anchor to `row`, both included. Without an
    /// anchor in the list, only `row` is selected and becomes the anchor.
    pub fn extend_to(&mut self, ids: &[NodeId], row: usize) -> Changed {
        if row >= ids.len() {
            return Changed::None;
        }
        let Some(start) = self.anchor.and_then(|a| {
            self.anchor_row
                .filter(|&r| ids.get(r) == Some(&a))
                .or_else(|| {
                    scanned(ids.len());
                    ids.iter().position(|&id| id == a)
                })
        }) else {
            return self.select_only(ids, row);
        };
        self.anchor_row = Some(start);
        let (lo, hi) = (start.min(row), start.max(row));
        let set = if lo == 0 && hi + 1 == ids.len() {
            Set::AllExcept(HashSet::new())
        } else {
            Set::Only(ids[lo..=hi].iter().copied().collect())
        };
        self.replace(ids, set, Some((lo, hi)))
    }

    /// Selects every row, without listing them.
    pub fn select_all(&mut self, ids: &[NodeId]) -> Changed {
        self.replace(ids, Set::AllExcept(HashSet::new()), None)
    }

    /// Selects the rows that were not selected and unselects the others,
    /// without listing them.
    pub fn invert(&mut self, ids: &[NodeId]) -> Changed {
        let new = match &self.set {
            Set::Only(s) => Set::AllExcept(s.clone()),
            Set::AllExcept(s) => Set::Only(s.clone()),
        };
        self.replace(ids, new, None)
    }

    pub fn clear(&mut self, ids: &[NodeId]) -> Changed {
        self.anchor = None;
        self.anchor_row = None;
        self.replace(ids, Set::Only(HashSet::new()), None)
    }

    /// Drops ids that are not in the new list `ids` (they were removed or no
    /// longer match). "All" stays all of the new list.
    pub fn retain(&mut self, ids: &[NodeId]) {
        let set = match &mut self.set {
            Set::Only(s) | Set::AllExcept(s) => s,
        };
        if !set.is_empty() {
            let present: HashSet<NodeId> =
                ids.iter().copied().filter(|id| set.contains(id)).collect();
            *set = present;
        }
        if let Some(a) = self.anchor {
            if !ids.contains(&a) {
                self.anchor = None;
            }
        }
    }

    /// Keeps the items selected in the list `old` that are in the new list
    /// `new` (the result of another query), and only those: "all" of the
    /// old list does not grow to the new one.
    pub fn carry_over(&mut self, old: &[NodeId], new: &[NodeId]) {
        let kept: HashSet<NodeId> = self.selected(old).into_iter().collect();
        let present: HashSet<NodeId> = if kept.is_empty() {
            HashSet::new()
        } else {
            new.iter().copied().filter(|id| kept.contains(id)).collect()
        };
        self.set = Set::Only(present);
        if let Some(a) = self.anchor {
            if !new.contains(&a) {
                self.anchor = None;
            }
        }
    }

    /// Forgets the selection and the anchor, e.g. when ids no longer mean the same nodes.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Replaces the set, whose ids are in the rows `run` when given, and
    /// reports which rows changed.
    fn replace(&mut self, ids: &[NodeId], new: Set, run: Option<(usize, usize)>) -> Changed {
        let changed = match (&self.set, &new) {
            (Set::Only(a), Set::Only(b)) if a == b => Changed::None,
            (Set::AllExcept(a), Set::AllExcept(b)) if a == b => Changed::None,
            (Set::Only(a), Set::Only(b)) if a.len() + b.len() <= MAX_ROW_CHANGES => {
                let mut diff: HashSet<NodeId> = a.symmetric_difference(b).copied().collect();
                // Rows of the old and the new run first; the list is
                // searched only for ids found in neither.
                let mut rows = Vec::new();
                for (lo, hi) in [self.run, run].into_iter().flatten() {
                    if hi - lo < MAX_ROW_CHANGES {
                        let run = ids.get(lo..=hi.min(ids.len().saturating_sub(1)));
                        for (r, id) in (lo..).zip(run.unwrap_or_default()) {
                            if diff.remove(id) {
                                rows.push(r);
                            }
                        }
                    }
                }
                rows.extend(rows_of(ids, &diff));
                rows.sort_unstable();
                Changed::Rows(rows)
            }
            _ => Changed::All,
        };
        self.set = new;
        self.run = run;
        changed
    }
}

#[cfg(test)]
thread_local! {
    /// Rows looked at by searches through the whole list, in tests.
    static SCANNED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Notes, in tests, that `rows` rows were looked at.
fn scanned(rows: usize) {
    #[cfg(test)]
    SCANNED.with(|s| s.set(s.get() + rows));
    #[cfg(not(test))]
    let _ = rows;
}

/// Rows holding the given ids.
fn rows_of(ids: &[NodeId], wanted: &HashSet<NodeId>) -> Vec<usize> {
    if wanted.is_empty() {
        return Vec::new();
    }
    scanned(ids.len());
    ids.iter()
        .enumerate()
        .filter(|(_, id)| wanted.contains(id))
        .map(|(row, _)| row)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(n: u32) -> Vec<NodeId> {
        // Not in id order, as after sorting.
        (0..n).rev().map(NodeId).collect()
    }

    fn rows(sel: &Selection, ids: &[NodeId]) -> Vec<usize> {
        (0..ids.len()).filter(|&r| sel.contains(ids[r])).collect()
    }

    #[test]
    fn invert_swaps_selected_and_unselected_rows() {
        let ids = list(6);
        let mut sel = Selection::default();
        assert_eq!(sel.invert(&ids), Changed::All);
        assert_eq!(rows(&sel, &ids), [0, 1, 2, 3, 4, 5]);
        sel.clear(&ids);
        sel.select_only(&ids, 1);
        sel.toggle(&ids, 4);
        sel.invert(&ids);
        assert_eq!(rows(&sel, &ids), [0, 2, 3, 5]);
        assert_eq!(sel.count(ids.len()), 4);
        sel.invert(&ids);
        assert_eq!(rows(&sel, &ids), [1, 4]);
        // After a refresh, inverting covers only the rows still listed.
        sel.invert(&ids);
        let fewer: Vec<NodeId> = ids[..3].to_vec();
        sel.retain(&fewer);
        sel.invert(&fewer);
        assert_eq!(rows(&sel, &fewer), [1]);
    }

    #[test]
    fn click_selects_one_row() {
        let ids = list(10);
        let mut s = Selection::default();
        assert_eq!(s.select_only(&ids, 3), Changed::Rows(vec![3]));
        assert_eq!(s.select_only(&ids, 5), Changed::Rows(vec![3, 5]));
        assert_eq!(rows(&s, &ids), [5]);
        assert_eq!(s.select_only(&ids, 5), Changed::None);
        assert_eq!(s.select_only(&ids, 99), Changed::None);
        assert_eq!(s.count(ids.len()), 1);
    }

    #[test]
    fn ctrl_click_toggles() {
        let ids = list(10);
        let mut s = Selection::default();
        s.select_only(&ids, 1);
        assert_eq!(s.toggle(&ids, 4), Changed::Rows(vec![4]));
        assert_eq!(rows(&s, &ids), [1, 4]);
        s.toggle(&ids, 1);
        assert_eq!(rows(&s, &ids), [4]);
    }

    #[test]
    fn shift_extends_from_the_anchor() {
        let ids = list(10);
        let mut s = Selection::default();
        s.select_only(&ids, 4);
        s.extend_to(&ids, 6);
        assert_eq!(rows(&s, &ids), [4, 5, 6]);
        // The anchor stays, so going back shrinks and flips the range.
        assert_eq!(s.extend_to(&ids, 2), Changed::Rows(vec![2, 3, 5, 6]));
        assert_eq!(rows(&s, &ids), [2, 3, 4]);
        // Without an anchor the row becomes the anchor.
        let mut fresh = Selection::default();
        fresh.extend_to(&ids, 7);
        assert_eq!(rows(&fresh, &ids), [7]);
        // A toggle moves the anchor.
        s.toggle(&ids, 8);
        s.extend_to(&ids, 9);
        assert_eq!(rows(&s, &ids), [8, 9]);
    }

    #[test]
    fn full_range_and_select_all_are_not_materialised() {
        let ids = list(1000);
        let mut s = Selection::default();
        s.select_only(&ids, 0);
        assert_eq!(s.extend_to(&ids, 999), Changed::All);
        assert!(matches!(&s.set, Set::AllExcept(e) if e.is_empty()));
        let mut s = Selection::default();
        assert_eq!(s.select_all(&ids), Changed::All);
        assert_eq!(s.count(ids.len()), 1000);
        s.toggle(&ids, 10);
        assert_eq!(s.count(ids.len()), 999);
        assert!(!s.contains(ids[10]));
        assert_eq!(s.selected(&ids).len(), 999);
        assert_eq!(s.clear(&ids), Changed::All);
        assert_eq!(s.count(ids.len()), 0);
    }

    #[test]
    fn selection_follows_ids_across_resorting() {
        let ids = list(10);
        let mut s = Selection::default();
        s.select_only(&ids, 2);
        s.toggle(&ids, 7);
        let picked = s.selected(&ids);
        let mut resorted = ids.clone();
        resorted.reverse();
        s.retain(&resorted);
        assert_eq!(s.selected(&resorted).len(), 2);
        let mut again = s.selected(&resorted);
        again.sort();
        let mut picked = picked;
        picked.sort();
        assert_eq!(again, picked);
    }

    #[test]
    fn retain_drops_ids_that_left_the_list() {
        let ids = list(10);
        let mut s = Selection::default();
        s.select_only(&ids, 0);
        s.extend_to(&ids, 3);
        let shorter: Vec<NodeId> = ids[2..].to_vec();
        s.retain(&shorter);
        assert_eq!(s.count(shorter.len()), 2);
        // The anchor (row 0 of the old list) is gone, so Shift starts anew.
        s.extend_to(&shorter, 5);
        assert_eq!(rows(&s, &shorter), [5]);
        // "All" stays all of the new list.
        s.select_all(&ids);
        s.retain(&shorter);
        assert_eq!(s.count(shorter.len()), shorter.len());
    }

    #[test]
    fn carried_over_selection_keeps_only_the_items_still_listed() {
        let ids = list(10);
        let mut s = Selection::default();
        s.select_only(&ids, 1);
        s.toggle(&ids, 4);
        s.toggle(&ids, 8);
        // Another query lists rows 3..10 of the old list, and new ones.
        let mut new: Vec<NodeId> = ids[3..].to_vec();
        new.extend((100..105).map(NodeId));
        s.carry_over(&ids, &new);
        let mut kept = s.selected(&new);
        kept.sort();
        let mut want = vec![ids[4], ids[8]];
        want.sort();
        assert_eq!(kept, want);
        assert_eq!(s.count(new.len()), 2);
        // "All" keeps the old items only; the new ones are not selected.
        s.select_all(&ids);
        s.carry_over(&ids, &new);
        assert_eq!(s.count(new.len()), 7);
        assert!(!s.contains(new[new.len() - 1]));
        // Nothing selected stays nothing.
        s.clear(&new);
        s.carry_over(&new, &ids);
        assert_eq!(s.count(ids.len()), 0);
    }

    #[test]
    fn big_changes_are_reported_as_all() {
        let ids = list(2000);
        let mut s = Selection::default();
        s.select_only(&ids, 0);
        assert_eq!(s.extend_to(&ids, 500), Changed::All);
        assert_eq!(s.select_only(&ids, 3), Changed::All);
        assert_eq!(s.select_only(&ids, 4), Changed::Rows(vec![3, 4]));
    }

    #[test]
    fn clicks_on_a_long_list_do_not_search_it() {
        let ids = list(1_000_000);
        let mut s = Selection::default();
        let looked = || SCANNED.with(|c| c.replace(0));
        looked();
        assert_eq!(s.select_only(&ids, 10), Changed::Rows(vec![10]));
        assert_eq!(
            s.select_only(&ids, 999_990),
            Changed::Rows(vec![10, 999_990])
        );
        assert_eq!(
            s.extend_to(&ids, 999_995),
            Changed::Rows((999_991..=999_995).collect())
        );
        let mut rows = vec![7];
        rows.extend(999_990..=999_995);
        assert_eq!(s.select_only(&ids, 7), Changed::Rows(rows));
        assert_eq!(s.select_only(&ids, 8), Changed::Rows(vec![7, 8]));
        assert_eq!(looked(), 0);
        // After the list changed, the rows are still found.
        let mut moved = ids.clone();
        moved.swap(8, 500);
        s.retain(&moved);
        assert_eq!(s.select_only(&moved, 9), Changed::Rows(vec![9, 500]));
    }
}
