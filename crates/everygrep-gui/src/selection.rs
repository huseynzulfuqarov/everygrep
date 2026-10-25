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
}

impl Default for Selection {
    fn default() -> Self {
        Self {
            set: Set::Only(HashSet::new()),
            anchor: None,
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
    #[allow(dead_code)] // Used by the file actions.
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
        self.replace(ids, Set::Only(HashSet::from([id])))
    }

    /// Adds the row to the selection or removes it.
    pub fn toggle(&mut self, ids: &[NodeId], row: usize) -> Changed {
        let Some(&id) = ids.get(row) else {
            return Changed::None;
        };
        self.anchor = Some(id);
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
        let Some(start) = self.anchor.and_then(|a| ids.iter().position(|&id| id == a)) else {
            return self.select_only(ids, row);
        };
        let (lo, hi) = (start.min(row), start.max(row));
        let set = if lo == 0 && hi + 1 == ids.len() {
            Set::AllExcept(HashSet::new())
        } else {
            Set::Only(ids[lo..=hi].iter().copied().collect())
        };
        self.replace(ids, set)
    }

    /// Selects every row, without listing them.
    pub fn select_all(&mut self, ids: &[NodeId]) -> Changed {
        self.replace(ids, Set::AllExcept(HashSet::new()))
    }

    pub fn clear(&mut self, ids: &[NodeId]) -> Changed {
        self.anchor = None;
        self.replace(ids, Set::Only(HashSet::new()))
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

    /// Forgets the selection and the anchor, e.g. when ids no longer mean the same nodes.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Replaces the set and reports which rows changed.
    fn replace(&mut self, ids: &[NodeId], new: Set) -> Changed {
        let changed = match (&self.set, &new) {
            (Set::Only(a), Set::Only(b)) if a == b => Changed::None,
            (Set::AllExcept(a), Set::AllExcept(b)) if a == b => Changed::None,
            (Set::Only(a), Set::Only(b)) if a.len() + b.len() <= MAX_ROW_CHANGES => {
                let diff: HashSet<NodeId> = a.symmetric_difference(b).copied().collect();
                Changed::Rows(rows_of(ids, &diff))
            }
            _ => Changed::All,
        };
        self.set = new;
        changed
    }
}

/// Rows holding the given ids.
fn rows_of(ids: &[NodeId], wanted: &HashSet<NodeId>) -> Vec<usize> {
    if wanted.is_empty() {
        return Vec::new();
    }
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
    fn big_changes_are_reported_as_all() {
        let ids = list(2000);
        let mut s = Selection::default();
        s.select_only(&ids, 0);
        assert_eq!(s.extend_to(&ids, 500), Changed::All);
        assert_eq!(s.select_only(&ids, 3), Changed::All);
        assert_eq!(s.select_only(&ids, 4), Changed::Rows(vec![3, 4]));
    }
}
