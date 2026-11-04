//! Zoom of the result list: the size of its text, icons and rows, in
//! percent. Zoom in and out step through [`STEPS`]; a value set in the
//! settings between two steps moves to the next one.

pub const DEFAULT: u32 = 100;
pub const MIN: u32 = 50;
pub const MAX: u32 = 300;

/// The sizes Zoom in and Zoom out step through.
pub const STEPS: [u32; 14] = [
    50, 67, 75, 80, 90, 100, 110, 125, 150, 175, 200, 225, 250, 300,
];

/// Whether `percent` can be set.
pub fn valid(percent: u32) -> bool {
    (MIN..=MAX).contains(&percent)
}

/// The next size after `current` in the direction of `step`: above 0 in,
/// below 0 out, 0 back to the default. Stays at the ends.
pub fn next(current: u32, step: i32) -> u32 {
    match step.signum() {
        0 => DEFAULT,
        1 => STEPS.iter().copied().find(|&s| s > current).unwrap_or(MAX),
        _ => STEPS
            .iter()
            .rev()
            .copied()
            .find(|&s| s < current)
            .unwrap_or(MIN),
    }
}

/// The zoom as the status line shows it, empty at the default size.
pub fn status_text(percent: u32) -> String {
    if percent == DEFAULT {
        String::new()
    } else {
        format!("{percent}%")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_steps_in_and_out_and_stays_at_the_ends() {
        assert_eq!(next(100, 1), 110);
        assert_eq!(next(100, -1), 90);
        assert_eq!(next(125, 0), 100);
        assert_eq!(next(300, 1), 300);
        assert_eq!(next(50, -1), 50);
        // A value between two steps goes to the next one.
        assert_eq!(next(120, 1), 125);
        assert_eq!(next(120, -1), 110);
        assert!(STEPS.iter().all(|&s| valid(s)));
        assert!(STEPS.windows(2).all(|w| w[0] < w[1]));
        assert!(!valid(49) && !valid(301));
    }

    #[test]
    fn the_status_line_shows_only_a_changed_zoom() {
        assert_eq!(status_text(100), "");
        assert_eq!(status_text(125), "125%");
    }
}
