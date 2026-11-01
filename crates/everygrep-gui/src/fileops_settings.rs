//! Preferences for file actions, separate from search and appearance settings.

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// Off keeps the existing in-place editor.
    pub rename_dialog: bool,
}

impl Settings {
    pub fn set(&mut self, key: &str, value: &str) -> bool {
        match (key, value) {
            ("rename-in-dialog", "true" | "yes" | "on" | "1") => self.rename_dialog = true,
            ("rename-in-dialog", "false" | "no" | "off" | "0") => self.rename_dialog = false,
            _ => return false,
        }
        true
    }

    pub fn write(&self, out: &mut String) {
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
}
