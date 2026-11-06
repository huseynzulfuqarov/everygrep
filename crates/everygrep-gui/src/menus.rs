//! Lists shown inside menus (open tabs, bookmarks, filters).
//!
//! Slint draws menus inside the window and cannot scroll them: a menu taller
//! than the room below the menu bar is pushed up over the bar and cut off.
//! So a menu shows only part of a long list, and an entry after it opens the
//! whole list (List all tabs, Manage bookmarks, Manage filters).

use std::ops::Range;

/// Open tabs shown in the Tabs menu. With the menu's other entries this keeps
/// it below the menu bar at the window's default height (660 px).
pub const TABS: usize = 8;
/// Bookmarks in the Bookmarks menu and filters in Search → Filter.
pub const SAVED: usize = 10;

/// Keep an unchanged model's identity: Slint regenerates the IDs of the
/// entire menu tree when a repeater's model changes. Open popups still
/// carry the old IDs, so neither activation nor submenu lookup can work.
pub fn stable_model<T: Clone + PartialEq + 'static>(
    old: slint::ModelRc<T>,
    rows: Vec<T>,
) -> slint::ModelRc<T> {
    use slint::Model;
    if old.iter().eq(rows.iter().cloned()) {
        old
    } else {
        slint::ModelRc::new(slint::VecModel::from(rows))
    }
}

/// The part of a list of `len` entries a menu shows: at most `limit`
/// entries, always including `current` (the checked one) when there is one.
pub fn window(len: usize, current: Option<usize>, limit: usize) -> Range<usize> {
    if len <= limit {
        return 0..len;
    }
    let start = match current {
        Some(i) if i < len => i.saturating_sub(limit / 2).min(len - limit),
        _ => 0,
    };
    start..start + limit
}

#[cfg(test)]
mod tests {
    use super::{stable_model, window};
    use slint::Model;

    #[test]
    fn unchanged_rows_keep_the_menu_model_identity() {
        let old = slint::ModelRc::new(slint::VecModel::from(vec![1, 2]));
        let same = stable_model(old.clone(), vec![1, 2]);
        assert_eq!(old, same);
        let changed = stable_model(same, vec![1, 3]);
        assert_ne!(old, changed);
        assert_eq!(changed.row_data(1), Some(3));
    }

    #[test]
    fn short_lists_are_shown_whole() {
        assert_eq!(window(0, None, 8), 0..0);
        assert_eq!(window(8, Some(7), 8), 0..8);
    }

    #[test]
    fn long_lists_keep_the_current_entry_in_view() {
        assert_eq!(window(20, None, 8), 0..8);
        assert_eq!(window(20, Some(0), 8), 0..8);
        assert_eq!(window(20, Some(3), 8), 0..8);
        assert_eq!(window(20, Some(10), 8), 6..14);
        assert_eq!(window(20, Some(19), 8), 12..20);
        // An index past the list (a filter snapshot after the live ones).
        assert_eq!(window(20, Some(25), 8), 0..8);
        for current in 0..20 {
            assert!(window(20, Some(current), 8).contains(&current));
        }
    }
}
