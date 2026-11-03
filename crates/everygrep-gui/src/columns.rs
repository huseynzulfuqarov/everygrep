//! Widths of the result list's columns when a header border is dragged.
//!
//! The columns are numbered as the window numbers them (0 Name, 1 Folder,
//! 2 Size, 3 Modified, 4 Type, 5 Extension, 6 Created, 7 Accessed,
//! 8 Attributes, 9 MD5, 10 SHA-256, 11 CRC-32, 12 Same name,
//! 13 Same extension, 14 Same size) and sit side by side; a hidden column has width 0. Folder
//! takes the room the other columns leave in the window, but never less than
//! its minimum, nor less than its own width once it has one. So the columns
//! together are as wide as the window or, when they do not fit, wider: the
//! list then scrolls sideways.
//!
//! A drag moves the border by the pointer's movement, so the border stays
//! under the pointer as long as no column has to go below its minimum:
//!
//! - to the right, the column on the left grows; the column on the right
//!   gives up its room down to its minimum, and the rest makes the columns
//!   wider than the window;
//! - to the left, the column on the left shrinks (down to its minimum); the
//!   columns first stop being wider than the window, and only then does the
//!   column on the right get the room.
//!
//! The border after the last column has no column on its right: dragged to
//! the right it makes the columns wider than the window, and to the left it
//! only takes that back.
//!
//! Folder is given a width of its own only while the columns are wider than
//! the window and Folder is wider than its minimum; otherwise it follows the
//! window again.

/// Number of columns of the result list.
pub const COLUMNS: usize = 32;
/// Folder, the column that takes the spare room.
const FOLDER: usize = 1;

/// The columns as the window shows them.
#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    /// Shown widths, 0 for a hidden column.
    pub widths: [f32; COLUMNS],
    /// Minimum widths.
    pub mins: [f32; COLUMNS],
    /// Width of the window (the room the columns fill).
    pub window: f32,
    /// Folder's own width, 0 for none.
    pub folder_want: f32,
}

impl Layout {
    /// Width of all columns together.
    pub fn total(&self) -> f32 {
        self.widths.iter().sum()
    }

    /// Folder's width as the window computes it from the other columns.
    pub fn folder_width(&self) -> f32 {
        let others = self.total() - self.widths[FOLDER];
        (self.window - others)
            .max(self.mins[FOLDER])
            .max(self.folder_want)
    }

    /// Gives Folder the width the window gives it.
    fn settle(&mut self) {
        self.widths[FOLDER] = self.folder_width();
    }

    /// Right edge of column `c`.
    #[cfg(test)]
    fn right_edge(&self, c: usize) -> f32 {
        self.widths[..=c].iter().sum()
    }
}

/// The columns after the border right of `left` was dragged by `delta`
/// (positive to the right). `right` is the shown column right of the border,
/// or `None` for the border after the last column.
pub fn drag(layout: &Layout, left: usize, right: Option<usize>, delta: f32) -> Layout {
    let mut n = layout.clone();
    n.settle();
    if !delta.is_finite() || left >= COLUMNS || right.is_some_and(|r| r >= COLUMNS || r == left) {
        return n;
    }
    let over = (n.total() - n.window).max(0.0);
    if delta > 0.0 {
        let give = right.map_or(0.0, |r| (n.widths[r] - n.mins[r]).clamp(0.0, delta));
        if let Some(r) = right {
            n.widths[r] -= give;
        }
        n.widths[left] += delta;
    } else {
        let mut take = (-delta).min((n.widths[left] - n.mins[left]).max(0.0));
        if right.is_none() {
            take = take.min(over);
        }
        // The columns stop being wider than the window first.
        let pass = take - take.min(over);
        n.widths[left] -= take;
        if let Some(r) = right {
            n.widths[r] += pass;
        }
    }
    // Folder needs a width of its own only where the window would give it
    // less than it has now.
    let folder = n.widths[FOLDER];
    let room = n.window - (n.total() - folder);
    n.folder_want = if folder > room.max(n.mins[FOLDER]) + 0.01 {
        folder
    } else {
        0.0
    };
    n.settle();
    n
}

/// [`drag`] for the window's `drag-columns` callback: `right` is -1 for the
/// border after the last column.
pub fn drag_in_window(
    layout: crate::ColumnLayout,
    left: i32,
    right: i32,
    delta: f32,
) -> crate::ColumnDrag {
    use slint::Model;
    let array = |m: &slint::ModelRc<f32>| {
        let mut a = [0.0; COLUMNS];
        for (slot, v) in a.iter_mut().zip(m.iter()) {
            *slot = v;
        }
        a
    };
    let now = Layout {
        widths: array(&layout.widths),
        mins: array(&layout.mins),
        window: layout.window,
        folder_want: layout.folder_want,
    };
    let left = usize::try_from(left).unwrap_or(COLUMNS);
    let right = usize::try_from(right).ok();
    let next = drag(&now, left, right, delta);
    crate::ColumnDrag {
        left: next.widths.get(left).copied().unwrap_or_default(),
        right: right
            .and_then(|r| next.widths.get(r).copied())
            .unwrap_or_default(),
        folder_want: next.folder_want,
    }
}

/// Folded for the column search: lower case, without diacritics.
fn folded(text: &str) -> Vec<u8> {
    everygrep_core::fold::strip_diacritics(&everygrep_core::fold::fold(text.as_bytes()))
}

/// Whether a column titled `title` matches what was typed into the column
/// search of Organize columns: every word typed is part of the title,
/// ignoring case and diacritics. Nothing typed matches every column.
pub fn title_matches(title: &str, typed: &str) -> bool {
    let title = folded(title);
    typed.split_whitespace().all(|word| {
        let word = folded(word);
        title.windows(word.len()).any(|w| w == word.as_slice())
    })
}

/// The columns of Organize columns that match the column search.
pub fn filter_choices(
    items: slint::ModelRc<crate::ColumnChoice>,
    typed: slint::SharedString,
) -> slint::ModelRc<crate::ColumnChoice> {
    use slint::Model;
    let shown: Vec<_> = items
        .iter()
        .filter(|c| title_matches(&c.title, &typed))
        .collect();
    slint::ModelRc::new(slint::VecModel::from(shown))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_column_search_matches_parts_of_titles() {
        assert!(title_matches("Same extension", ""));
        assert!(title_matches("Same extension", "  "));
        assert!(title_matches("Same extension", "same"));
        assert!(title_matches("Same extension", "EXT"));
        assert!(title_matches("Same extension", "same ext"));
        assert!(!title_matches("Same extension", "size"));
        assert!(!title_matches("Same extension", "ext size"));
        // Case and diacritics are ignored, both ways.
        assert!(title_matches("Yaradılma tarixi", "TARİXİ"));
        assert!(title_matches("Uzantı sıklığı", "siklig"));
        assert!(title_matches("Size", "SİZE"));
        assert!(title_matches("SHA-256", "sha-2"));
    }

    /// `first` and zeros for the columns after them.
    fn pad(first: &[f32]) -> [f32; COLUMNS] {
        let mut a = [0.0; COLUMNS];
        a[..first.len()].copy_from_slice(first);
        a
    }

    fn mins() -> [f32; COLUMNS] {
        let mut a = [48.0; COLUMNS];
        a[..9].copy_from_slice(&[120.0, 100.0, 56.0, 80.0, 64.0, 48.0, 80.0, 80.0, 40.0]);
        a
    }

    /// The four default columns in a window of `window` pixels, Name at 300.
    fn four(window: f32) -> Layout {
        let mut l = Layout {
            widths: pad(&[300.0, 0.0, 96.0, 120.0]),
            mins: mins(),
            window,
            folder_want: 0.0,
        };
        l.settle();
        l
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.01
    }

    #[test]
    fn a_drag_with_room_keeps_the_columns_as_wide_as_the_window() {
        let l = four(1000.0);
        assert!(close(l.widths[1], 484.0));
        // Size|Modified to the right: Size grows, Modified shrinks.
        let n = drag(&l, 2, Some(3), 20.0);
        assert!(close(n.widths[2], 116.0) && close(n.widths[3], 100.0));
        assert!(close(n.total(), 1000.0));
        assert!(close(n.right_edge(2), l.right_edge(2) + 20.0));
        assert_eq!(n.folder_want, 0.0);
        // Name|Folder: Folder gives the room and still follows the window.
        let n = drag(&l, 0, Some(1), 50.0);
        assert!(close(n.widths[0], 350.0) && close(n.widths[1], 434.0));
        assert_eq!(n.folder_want, 0.0);
    }

    #[test]
    fn past_the_minimum_of_the_right_column_the_columns_get_wider_than_the_window() {
        let l = four(1000.0);
        // Modified can give 40 pixels; the other 60 widen the columns.
        let n = drag(&l, 2, Some(3), 100.0);
        assert!(close(n.widths[3], 80.0));
        assert!(close(n.widths[2], 196.0));
        assert!(close(n.total(), 1060.0), "{n:?}");
        // The border stayed under the pointer, and Folder kept its width.
        assert!(close(n.right_edge(2), l.right_edge(2) + 100.0));
        assert!(close(n.widths[1], l.widths[1]));
        assert!(close(n.folder_want, l.widths[1]));

        // Name past Folder's minimum.
        let n = drag(&l, 0, Some(1), 500.0);
        assert!(close(n.widths[1], 100.0));
        assert!(close(n.right_edge(0), 800.0));
        assert!(close(n.total(), 1116.0));
        assert_eq!(n.folder_want, 0.0, "Folder at its minimum needs no width");

        // Folder's own right border.
        let n = drag(&l, 1, Some(2), 200.0);
        assert!(close(n.widths[2], 56.0));
        assert!(close(n.right_edge(1), l.right_edge(1) + 200.0));
        assert!(close(n.folder_want, n.widths[1]));
    }

    #[test]
    fn dragging_back_first_takes_the_extra_width_back() {
        let l = four(1000.0);
        let wide = drag(&l, 2, Some(3), 100.0);
        // 30 back: the columns narrow, Modified stays at its minimum.
        let n = drag(&wide, 2, Some(3), -30.0);
        assert!(close(n.total(), 1030.0));
        assert!(close(n.widths[3], 80.0));
        assert!(close(n.right_edge(2), wide.right_edge(2) - 30.0));
        // All the way back: as at the start, Folder follows the window again.
        let n = drag(&n, 2, Some(3), -70.0);
        assert!(close(n.total(), 1000.0));
        assert_eq!(n.widths, l.widths);
        assert_eq!(n.folder_want, 0.0);
    }

    #[test]
    fn the_last_border_only_widens_or_takes_its_extra_width_back() {
        let l = four(1000.0);
        let n = drag(&l, 3, None, 40.0);
        assert!(close(n.widths[3], 160.0) && close(n.total(), 1040.0));
        assert!(close(n.right_edge(3), 1040.0));
        let back = drag(&n, 3, None, -100.0);
        assert!(close(back.total(), 1000.0) && close(back.widths[3], 120.0));
        assert_eq!(drag(&l, 3, None, -10.0), l, "nothing to take back");
    }

    #[test]
    fn no_column_goes_below_its_minimum() {
        let l = four(1000.0);
        let n = drag(&l, 2, Some(3), -500.0);
        assert!(close(n.widths[2], 56.0));
        assert!(close(n.widths[3], 160.0));
        let n = drag(&l, 0, Some(1), -1000.0);
        assert!(close(n.widths[0], 120.0));
        assert!(close(n.total(), 1000.0));
    }

    #[test]
    fn a_narrow_window_with_every_column_is_wider_than_the_window() {
        let mut l = four(560.0);
        l.widths = pad(&[300.0, 0.0, 96.0, 120.0, 130.0, 80.0, 120.0, 120.0, 64.0]);
        l.settle();
        assert!(close(l.widths[1], 100.0));
        assert!(l.total() > 560.0);
        // Narrowing a column narrows the columns, nothing else moves.
        let n = drag(&l, 4, Some(5), -30.0);
        assert!(close(n.total(), l.total() - 30.0));
        assert!(close(n.widths[5], 80.0));
    }

    #[test]
    fn borders_follow_the_pointer_through_any_series_of_drags() {
        // Hidden columns (Type, Extension) are skipped as the window does.
        let shown = [0, 1, 2, 3, 6, 7, 8];
        let mut l = four(900.0);
        l.widths = pad(&[250.0, 0.0, 96.0, 120.0, 0.0, 0.0, 120.0, 120.0, 64.0]);
        l.settle();
        let mut seed = 7_u32;
        for _ in 0..2000 {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let at = (seed >> 16) as usize % shown.len();
            let delta = ((seed >> 4) % 161) as f32 - 80.0;
            let left = shown[at];
            let right = shown.get(at + 1).copied();
            let n = drag(&l, left, right, delta);
            for c in 0..COLUMNS {
                if l.widths[c] > 0.0 {
                    assert!(n.widths[c] >= n.mins[c] - 0.01, "{c} below minimum: {n:?}");
                }
            }
            assert!(
                n.total() >= n.window - 0.01,
                "narrower than the window: {n:?}"
            );
            let moved = n.right_edge(left) - l.right_edge(left);
            // The border follows, or stops where a minimum (or the last
            // border's extra width) runs out, never going the wrong way.
            assert!(moved * delta >= -0.01, "{moved} for {delta}");
            assert!(moved.abs() <= delta.abs() + 0.01);
            if moved.abs() + 0.01 < delta.abs() {
                let stuck = n.widths[left] <= n.mins[left] + 0.01 || right.is_none();
                assert!(stuck, "stopped early: {l:?} {left} {delta} {n:?}");
            }
            // Folder only has a width of its own while it needs one.
            if n.folder_want > 0.0 {
                assert!(n.total() > n.window + 0.01 && n.widths[1] > n.mins[1]);
            }
            l = n;
        }
    }
}
