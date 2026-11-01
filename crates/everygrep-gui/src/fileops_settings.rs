//! Preferences for file actions, separate from search and appearance settings.

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// Off keeps the existing in-place editor.
    pub rename_dialog: bool,
    pub shortcut_kind: crate::fileops_shortcut::Kind,
    pub shortcut_collision: crate::fileops_shortcut::Collision,
}

impl Settings {
    pub fn fill_window(&self, w: &crate::SettingsWindow) {
        w.set_fileops_rename_dialog(self.rename_dialog);
        w.set_fileops_shortcut_kind(i32::from(
            self.shortcut_kind == crate::fileops_shortcut::Kind::Desktop,
        ));
        w.set_fileops_shortcut_collision(i32::from(
            self.shortcut_collision == crate::fileops_shortcut::Collision::Skip,
        ));
    }

    pub fn from_window(w: &crate::SettingsWindow) -> Self {
        Self {
            rename_dialog: w.get_fileops_rename_dialog(),
            shortcut_kind: if w.get_fileops_shortcut_kind() == 1 {
                crate::fileops_shortcut::Kind::Desktop
            } else {
                crate::fileops_shortcut::Kind::Symlink
            },
            shortcut_collision: if w.get_fileops_shortcut_collision() == 1 {
                crate::fileops_shortcut::Collision::Skip
            } else {
                crate::fileops_shortcut::Collision::Number
            },
        }
    }

    pub fn set(&mut self, key: &str, value: &str) -> bool {
        match (key, value) {
            ("rename-in-dialog", "true" | "yes" | "on" | "1") => self.rename_dialog = true,
            ("rename-in-dialog", "false" | "no" | "off" | "0") => self.rename_dialog = false,
            ("shortcut-kind", "symlink") => {
                self.shortcut_kind = crate::fileops_shortcut::Kind::Symlink
            }
            ("shortcut-kind", "desktop") => {
                self.shortcut_kind = crate::fileops_shortcut::Kind::Desktop
            }
            ("shortcut-collision", "number") => {
                self.shortcut_collision = crate::fileops_shortcut::Collision::Number
            }
            ("shortcut-collision", "skip") => {
                self.shortcut_collision = crate::fileops_shortcut::Collision::Skip
            }
            _ => return false,
        }
        true
    }

    pub fn write(&self, out: &mut String) {
        if self.shortcut_kind == crate::fileops_shortcut::Kind::Desktop {
            out.push_str("shortcut-kind = desktop\n");
        }
        if self.shortcut_collision == crate::fileops_shortcut::Collision::Skip {
            out.push_str("shortcut-collision = skip\n");
        }
        if self.rename_dialog {
            out.push_str("rename-in-dialog = true\n");
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::settings::{Page, Settings};

    #[test]
    fn rename_choice_roundtrips_and_resets_with_window_preferences() {
        let mut s = Settings::parse("rename-in-dialog = true\n");
        assert!(s.fileops.rename_dialog);
        assert!(Settings::parse(&s.to_text()).fileops.rename_dialog);
        s.reset(Page::Search);
        assert!(s.fileops.rename_dialog);
        s.reset(Page::Window);
        assert!(!s.fileops.rename_dialog);
        assert!(!s.to_text().contains("rename-in-dialog"));
        assert_eq!(
            Settings::parse("rename-in-dialog = invalid").problems.len(),
            1
        );
    }
    #[test]
    fn shortcut_choices_roundtrip_reset_and_report_invalid_values() {
        let mut s = Settings::parse("shortcut-kind = desktop\nshortcut-collision = skip\n");
        let back = Settings::parse(&s.to_text());
        assert_eq!(back.fileops, s.fileops);
        assert_eq!(
            s.fileops.shortcut_kind,
            crate::fileops_shortcut::Kind::Desktop
        );
        assert_eq!(
            s.fileops.shortcut_collision,
            crate::fileops_shortcut::Collision::Skip
        );
        s.reset(Page::Window);
        assert_eq!(s.fileops, super::Settings::default());
        assert_eq!(
            Settings::parse("shortcut-kind = invalid\nshortcut-collision = invalid")
                .problems
                .len(),
            2
        );
    }
}
