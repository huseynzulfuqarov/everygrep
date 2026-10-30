//! The window's colors, resolved from the theme setting and the desktop's
//! light or dark setting.

use crate::portal::ColorScheme;
use crate::settings::ThemeChoice;

/// Colors for the windows; the number is what their `set-color-scheme`
/// function takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    /// Leave it to the toolkit, which reads the same desktop setting. Used
    /// only while the desktop's answer is not known.
    Toolkit = 0,
    Light = 1,
    Dark = 2,
}

/// The colors for a theme choice. `desktop` is the desktop's setting, `None`
/// until the portal answered (or when there is no portal). "No preference"
/// is GNOME's default style, which is light.
pub fn resolve(choice: ThemeChoice, desktop: Option<ColorScheme>) -> Scheme {
    match (choice, desktop) {
        (ThemeChoice::Light, _) => Scheme::Light,
        (ThemeChoice::Dark, _) => Scheme::Dark,
        (ThemeChoice::System, Some(ColorScheme::Dark)) => Scheme::Dark,
        (ThemeChoice::System, Some(ColorScheme::Light | ColorScheme::NoPreference)) => {
            Scheme::Light
        }
        (ThemeChoice::System, None) => Scheme::Toolkit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_choices_ignore_the_desktop() {
        for desktop in [
            None,
            Some(ColorScheme::Dark),
            Some(ColorScheme::Light),
            Some(ColorScheme::NoPreference),
        ] {
            assert_eq!(resolve(ThemeChoice::Light, desktop), Scheme::Light);
            assert_eq!(resolve(ThemeChoice::Dark, desktop), Scheme::Dark);
        }
    }

    #[test]
    fn system_follows_the_desktop() {
        let system = |d| resolve(ThemeChoice::System, d);
        assert_eq!(system(Some(ColorScheme::Dark)), Scheme::Dark);
        assert_eq!(system(Some(ColorScheme::Light)), Scheme::Light);
        assert_eq!(system(Some(ColorScheme::NoPreference)), Scheme::Light);
        assert_eq!(system(None), Scheme::Toolkit);
    }
}
