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

/// Accent colors resolved without a desktop dependency. System colors retain
/// their hue while moving toward black/white to meet both schemes' contrast.
pub fn accents(
    choice: crate::settings::AccentChoice,
    desktop: Option<[u8; 3]>,
) -> crate::AccentColors {
    use crate::settings::AccentChoice;
    let (light, dark) = match choice {
        AccentChoice::System => match desktop {
            Some(rgb) => (adjust(rgb, false), adjust(rgb, true)),
            None => ([31, 95, 214], [112, 166, 255]),
        },
        AccentChoice::Blue => ([31, 95, 214], [112, 166, 255]),
        AccentChoice::Green => ([23, 107, 58], [114, 206, 152]),
        AccentChoice::Teal => ([8, 115, 110], [102, 204, 197]),
        AccentChoice::Purple => ([117, 69, 181], [192, 162, 247]),
        AccentChoice::Orange => ([169, 75, 11], [245, 178, 121]),
        AccentChoice::Pink => ([179, 47, 88], [250, 155, 184]),
    };
    let color = |[r, g, b]: [u8; 3]| slint::Color::from_rgb_u8(r, g, b);
    crate::AccentColors {
        light: color(light),
        dark: color(dark),
        light_selection: color(blend(light, [255; 3], 0.82)),
        dark_selection: color(blend(dark, [0; 3], 0.65)),
        custom_selection: choice != AccentChoice::Blue
            && (choice != AccentChoice::System || desktop.is_some()),
    }
}

fn blend(a: [u8; 3], b: [u8; 3], amount: f64) -> [u8; 3] {
    std::array::from_fn(|i| {
        (f64::from(a[i]) * (1.0 - amount) + f64::from(b[i]) * amount).round() as u8
    })
}

fn luminance(c: [u8; 3]) -> f64 {
    let channel = |v: u8| {
        let v = f64::from(v) / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(c[0]) + 0.7152 * channel(c[1]) + 0.0722 * channel(c[2])
}

fn adjust(rgb: [u8; 3], dark: bool) -> [u8; 3] {
    for step in 0..=100 {
        let c = blend(
            rgb,
            if dark { [255; 3] } else { [0; 3] },
            f64::from(step) / 100.0,
        );
        let l = luminance(c);
        if (dark && l >= 0.38) || (!dark && l <= 0.14) {
            return c;
        }
    }
    unreachable!("black and white meet the luminance bounds")
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
        styled_token(name, 3)
    }

    fn styled_token(name: &str, style: usize) -> [[u8; 3]; 2] {
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
            [layered, graphite, classic, slate, light] => {
                [light, [slate, layered, graphite, classic][style]]
            }
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
    fn fixed_and_system_accents_meet_contrast_in_every_style() {
        use crate::settings::AccentChoice;
        let rgb = |c: slint::Color| [c.red(), c.green(), c.blue()];
        let check = |a: crate::AccentColors| {
            assert!(contrast(rgb(a.light), [255; 3]) >= 4.5);
            assert!(contrast(rgb(a.dark), [8, 18, 28]) >= 4.5);
            assert!(contrast(rgb(a.light_selection), [11, 27, 61]) >= 4.5);
            assert!(contrast(rgb(a.dark_selection), [255; 3]) >= 4.5);
            for style in 0..4 {
                let selection = styled_token("selection", style);
                let light_selection = if a.custom_selection {
                    rgb(a.light_selection)
                } else {
                    selection[0]
                };
                let dark_selection = if a.custom_selection {
                    rgb(a.dark_selection)
                } else {
                    selection[1]
                };
                assert!(contrast(rgb(a.light), light_selection) >= 3.0);
                assert!(contrast(rgb(a.dark), dark_selection) >= 3.0);
                for bg in [
                    "surface",
                    "surface-alt",
                    "field",
                    "hover",
                    "row-alt",
                    "popup-surface",
                ] {
                    let backgrounds = styled_token(bg, style);
                    assert!(contrast(rgb(a.light), backgrounds[0]) >= 3.0, "light {bg}");
                    assert!(
                        contrast(rgb(a.dark), backgrounds[1]) >= 3.0,
                        "style {style}: {bg}"
                    );
                }
            }
        };
        for choice in AccentChoice::ALL {
            check(accents(choice, None));
        }
        for r in [0, 51, 102, 153, 204, 255] {
            for g in [0, 51, 102, 153, 204, 255] {
                for b in [0, 51, 102, 153, 204, 255] {
                    check(accents(AccentChoice::System, Some([r, g, b])));
                }
            }
        }
        let blue = accents(AccentChoice::Blue, None);
        assert_eq!(accents(AccentChoice::System, None), blue);
        assert_eq!(accents(AccentChoice::Blue, Some([255, 0, 0])), blue);
        assert_ne!(accents(AccentChoice::System, Some([255, 0, 0])), blue);
    }

    #[test]
    fn every_dark_style_keeps_text_and_popup_edges_readable() {
        for style in 0..4 {
            let t = |name| styled_token(name, style)[1];
            for bg in [
                "surface",
                "surface-alt",
                "popup-surface",
                "hover",
                "row-alt",
            ] {
                assert!(contrast(t("text"), t(bg)) >= 7.0, "style {style}: {bg}");
                assert!(
                    contrast(t("text-muted"), t(bg)) >= 4.5,
                    "style {style}: {bg}"
                );
            }
            for bg in ["surface", "surface-alt", "popup-surface"] {
                assert!(
                    contrast(t("popup-edge"), t(bg)) >= 3.0,
                    "style {style}: {bg}"
                );
            }
            assert!(contrast(t("selection-text"), t("selection")) >= 7.0);
        }
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
