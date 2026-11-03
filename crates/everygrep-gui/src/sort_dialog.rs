//! The Advanced sort dialog: a primary, secondary and tertiary sort column,
//! each ascending or descending. Its choices live in the window's
//! `sort-then*` properties; [`window_sort`] turns them into the engine's
//! [`Sort`].

use crate::i18n::tr;
use crate::{AppWindow, is_sort_column, sort_for_column};
use everygrep_core::sort::{FolderSize, Sort};
use slint::{Model, ModelRc, SharedString, VecModel};

/// The order the window shows: its sort column and direction, then the
/// secondary and tertiary columns chosen in Advanced sort.
pub fn window_sort(w: &AppWindow, folders: FolderSize) -> Sort {
    let mut sort = sort_for_column(w.get_sort_column(), w.get_sort_descending(), folders);
    sort.then = then_keys(
        [
            (w.get_sort_then1_column(), w.get_sort_then1_descending()),
            (w.get_sort_then2_column(), w.get_sort_then2_descending()),
        ],
        folders,
    );
    sort
}

/// The keys of secondary and tertiary columns; -1 and columns that do not
/// sort are none.
fn then_keys(
    columns: [(i32, bool); 2],
    folders: FolderSize,
) -> [Option<(everygrep_core::sort::SortKey, bool)>; 2] {
    columns.map(|(column, descending)| {
        (column >= 0 && is_sort_column(column))
            .then(|| (sort_for_column(column, descending, folders).key, descending))
    })
}

/// Forgets the secondary and tertiary columns, for an order set by a
/// single column (a tab or bookmark restored).
pub fn clear_then(w: &AppWindow) {
    w.set_sort_then1_column(-1);
    w.set_sort_then2_column(-1);
}

/// Fills the dialog with the shown sortable columns and the current order.
pub fn opening(w: &AppWindow) {
    let items = w.get_sort_items();
    let shown: Vec<(SharedString, i32)> = items
        .iter()
        .filter(|item| item.enabled)
        .map(|item| (item.title, item.column))
        .collect();
    let columns: Vec<i32> = shown.iter().map(|(_, c)| *c).collect();
    let names: Vec<SharedString> = shown.iter().map(|(t, _)| t.clone()).collect();
    let then_names: Vec<SharedString> = std::iter::once(SharedString::from(tr("None")))
        .chain(names.iter().cloned())
        .collect();
    let chosen = chosen(
        &columns,
        [
            w.get_sort_column(),
            w.get_sort_then1_column(),
            w.get_sort_then2_column(),
        ],
    );
    w.set_advanced_sort_names(ModelRc::new(VecModel::from(names)));
    w.set_advanced_sort_then_names(ModelRc::new(VecModel::from(then_names)));
    w.set_advanced_sort_columns(ModelRc::new(VecModel::from(columns)));
    w.set_advanced_sort_chosen(ModelRc::new(VecModel::from(chosen.to_vec())));
    w.set_advanced_sort_descending(ModelRc::new(VecModel::from(vec![
        w.get_sort_descending(),
        w.get_sort_then1_descending(),
        w.get_sort_then2_descending(),
    ])));
}

/// The list positions of the primary column (the first one when it is not
/// listed) and of the secondary and tertiary ones (after None, which is 0).
fn chosen(columns: &[i32], current: [i32; 3]) -> [i32; 3] {
    let at = |c: i32| columns.iter().position(|&x| x == c).map(|i| i as i32);
    [
        at(current[0]).unwrap_or(0),
        at(current[1]).map_or(0, |i| i + 1),
        at(current[2]).map_or(0, |i| i + 1),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use everygrep_core::sort::SortKey;

    #[test]
    fn choices_map_to_list_positions_and_keys() {
        let columns = [0, 1, 2, 3, 5];
        assert_eq!(chosen(&columns, [2, 5, -1]), [2, 5, 0]);
        // A primary column no longer listed falls back to the first one.
        assert_eq!(chosen(&columns, [7, 9, 0]), [0, 0, 1]);
        assert_eq!(
            then_keys([(2, true), (-1, false)], FolderSize::Total),
            [Some((SortKey::Size, true)), None]
        );
        // Hash columns never sort.
        assert_eq!(
            then_keys([(9, false), (3, false)], FolderSize::Total),
            [None, Some((SortKey::Modified, false))]
        );
    }
}
