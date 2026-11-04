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
    sort_from_columns(
        &crate::ColumnSort {
            column: w.get_sort_column(),
            descending: w.get_sort_descending(),
            then1: w.get_sort_then1_column(),
            then1_descending: w.get_sort_then1_descending(),
            then2: w.get_sort_then2_column(),
            then2_descending: w.get_sort_then2_descending(),
        },
        folders,
    )
}

pub(crate) fn sort_from_columns(columns: &crate::ColumnSort, folders: FolderSize) -> Sort {
    let mut sort = sort_for_column(columns.column, columns.descending, folders);
    sort.then = then_keys(
        [
            (columns.then1, columns.then1_descending),
            (columns.then2, columns.then2_descending),
        ],
        folders,
    );
    sort
}

/// Reset hides every optional column; its keys cannot remain in the order.
pub fn reset_columns_sort(mut columns: crate::ColumnSort) -> crate::ColumnSort {
    if columns.column > 3 {
        columns.column = 0;
        columns.descending = false;
        columns.then1 = -1;
        columns.then2 = -1;
        return columns;
    }
    let mut visible = [
        (columns.then1, columns.then1_descending),
        (columns.then2, columns.then2_descending),
    ]
    .into_iter()
    .filter(|(column, _)| (0..=3).contains(column));
    (columns.then1, columns.then1_descending) = visible.next().unwrap_or((-1, false));
    (columns.then2, columns.then2_descending) = visible.next().unwrap_or((-1, false));
    columns
}

/// A plain column selection toggles the current primary, or starts a new one.
pub fn single_column_sort(
    mut columns: crate::ColumnSort,
    column: i32,
    first_descending: bool,
) -> crate::ColumnSort {
    if columns.column == column {
        columns.descending = !columns.descending;
    } else {
        columns.column = column;
        columns.descending = first_descending;
    }
    columns.then1 = -1;
    columns.then2 = -1;
    columns
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
    fn reset_columns_preserves_visible_keys_and_their_directions() {
        let width = everygrep_core::property::Property::Width.column();
        let before = crate::ColumnSort {
            column: 2,
            descending: true,
            then1: width,
            then1_descending: false,
            then2: 3,
            then2_descending: true,
        };
        let after = reset_columns_sort(before.clone());
        assert_eq!((after.column, after.descending), (2, true));
        assert_eq!(
            (after.then1, after.then1_descending, after.then2),
            (3, true, -1)
        );
        let after = reset_columns_sort(crate::ColumnSort {
            then1: 3,
            then1_descending: true,
            then2: width,
            ..before.clone()
        });
        assert_eq!(
            (after.then1, after.then1_descending, after.then2),
            (3, true, -1)
        );
        let after = reset_columns_sort(crate::ColumnSort {
            column: width,
            ..before
        });
        assert_eq!(
            (after.column, after.descending, after.then1, after.then2),
            (0, false, -1, -1)
        );
    }

    #[test]
    fn a_plain_column_selection_clears_tie_breakers_even_for_the_current_primary() {
        for descending in [false, true] {
            let before = crate::ColumnSort {
                column: 2,
                descending,
                then1: 3,
                then1_descending: true,
                then2: everygrep_core::property::Property::Width.column(),
                then2_descending: false,
            };
            let after = single_column_sort(before.clone(), 2, true);
            assert_eq!((after.column, after.descending), (2, !descending));
            assert_eq!((after.then1, after.then2), (-1, -1));
            assert!(
                sort_from_columns(&after, FolderSize::default())
                    .media_properties()
                    .is_empty()
            );
            let other = single_column_sort(before, 3, true);
            assert_eq!(
                (other.column, other.descending, other.then1, other.then2),
                (3, true, -1, -1)
            );
        }
    }

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
        // Hash columns can be secondary keys too.
        assert_eq!(
            then_keys([(9, false), (3, false)], FolderSize::Total),
            [
                Some((SortKey::Hash(everygrep_core::hash::Algorithm::Md5), false)),
                Some((SortKey::Modified, false))
            ]
        );
    }
}
