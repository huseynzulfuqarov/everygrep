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

    /// The light and dark value of a color token in `ui/theme.slint`.
    fn token(name: &str) -> [[u8; 3]; 2] {
        let source = include_str!("../ui/theme.slint");
        let line = source
            .lines()
            .find(|l| l.contains(&format!("<color> {name}:")))
            .unwrap_or_else(|| panic!("no token {name}"));
        let colors: Vec<[u8; 3]> = line
            .split('#')
            .skip(1)
            .map(|h| {
                let byte = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).expect("hex color");
                [byte(0), byte(2), byte(4)]
            })
            .collect();
        match colors[..] {
            [one] => [one, one],
            [dark, light] => [light, dark],
            _ => panic!("{name}: {line}"),
        }
    }

    /// WCAG contrast ratio of two colors.
    fn contrast(a: [u8; 3], b: [u8; 3]) -> f64 {
        let luminance = |c: [u8; 3]| {
            let channel = |v: u8| {
                let v = f64::from(v) / 255.0;
                if v <= 0.039_28 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * channel(c[0]) + 0.7152 * channel(c[1]) + 0.0722 * channel(c[2])
        };
        let (x, y) = (luminance(a), luminance(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn floating_edges_stand_out_in_both_themes() {
        for theme in 0..2 {
            for bg in ["surface", "surface-alt", "popup-surface"] {
                let ratio = contrast(token("popup-edge")[theme], token(bg)[theme]);
                assert!(
                    ratio >= 3.0,
                    "popup edge on {bg} (theme {theme}): {ratio:.2}"
                );
            }
            assert!(contrast(token("text")[theme], token("popup-surface")[theme]) >= 7.0);
            assert!(contrast(token("text-muted")[theme], token("popup-surface")[theme]) >= 4.5);
        }
    }

    #[test]
    fn row_text_keeps_its_contrast_in_both_themes() {
        // Body text 7:1, muted text 4.5:1, offline entries 3.9:1, on the
        // backgrounds a row can have.
        let rules = [
            ("text", 7.0, &["surface", "row-alt", "hover"][..]),
            ("text-muted", 4.5, &["surface", "row-alt", "hover"][..]),
            ("text-offline", 3.9, &["surface", "row-alt"][..]),
            ("selection-text", 7.0, &["selection"][..]),
        ];
        for (fg, least, backgrounds) in rules {
            for bg in backgrounds {
                for theme in 0..2 {
                    let ratio = contrast(token(fg)[theme], token(bg)[theme]);
                    assert!(
                        ratio >= least,
                        "{fg} on {bg} ({}): {ratio:.2}",
                        ["light", "dark"][theme]
                    );
                }
            }
        }
    }
}
