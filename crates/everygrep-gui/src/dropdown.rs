//! Selection stepping shared by the two search dropdowns.

/// Arrow keys select immediately, stop at either end, and never select an
/// absent row. A missing selection starts at the first row on Down.
pub fn step_index(current: i32, count: i32, step: i32) -> i32 {
    if count <= 0 {
        -1
    } else {
        current.saturating_add(step).clamp(0, count - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrows_select_adjacent_rows_without_wrapping() {
        assert_eq!(step_index(2, 5, -1), 1);
        assert_eq!(step_index(2, 5, 1), 3);
        assert_eq!(step_index(0, 5, -1), 0);
        assert_eq!(step_index(4, 5, 1), 4);
        assert_eq!(step_index(0, 1, 1), 0);
        assert_eq!(step_index(0, 1, -1), 0);
    }

    #[test]
    fn absent_and_stale_selections_stay_within_the_model() {
        assert_eq!(step_index(-1, 5, 1), 0);
        assert_eq!(step_index(-1, 5, -1), 0);
        assert_eq!(step_index(8, 3, -1), 2);
        assert_eq!(step_index(i32::MAX, 3, 1), 2);
        assert_eq!(step_index(i32::MIN, 3, -1), 0);
    }

    #[test]
    fn empty_models_have_no_selection() {
        for current in [-1, 0, 1, i32::MAX] {
            for step in [-1, 1] {
                assert_eq!(step_index(current, 0, step), -1);
                assert_eq!(step_index(current, -1, step), -1);
            }
        }
    }
}
