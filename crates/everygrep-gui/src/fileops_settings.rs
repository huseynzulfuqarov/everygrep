//! Preferences for file actions, separate from search and appearance settings.

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// Off keeps the existing in-place editor.
    pub rename_dialog: bool,
    pub conflict_ask: bool,
    pub search_new_window: bool,
    pub search_recursive: bool,
    pub search_exact_name: bool,
    pub shortcut_kind: crate::fileops_shortcut::Kind,
    pub shortcut_collision: crate::fileops_shortcut::Collision,
}

impl Settings {
    pub fn fill_window(&self, w: &crate::SettingsWindow) {
        w.set_fileops_rename_dialog(self.rename_dialog);
        w.set_fileops_conflict_ask(self.conflict_ask);
        w.set_fileops_search_new_window(self.search_new_window);
        w.set_fileops_search_recursive(self.search_recursive);
        w.set_fileops_search_exact_name(self.search_exact_name);
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
            conflict_ask: w.get_fileops_conflict_ask(),
            search_new_window: w.get_fileops_search_new_window(),
            search_recursive: w.get_fileops_search_recursive(),
            search_exact_name: w.get_fileops_search_exact_name(),
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
            (
                "context-search-new-window"
                | "context-search-subfolders"
                | "context-search-exact-name",
                value,
            ) => {
                let on = match value {
                    "true" | "yes" | "on" | "1" => true,
                    "false" | "no" | "off" | "0" => false,
                    _ => return false,
                };
                match key {
                    "context-search-new-window" => self.search_new_window = on,
                    "context-search-subfolders" => self.search_recursive = on,
                    _ => self.search_exact_name = on,
                }
            }
            ("file-conflicts-ask", "true" | "yes" | "on" | "1") => self.conflict_ask = true,
            ("file-conflicts-ask", "false" | "no" | "off" | "0") => self.conflict_ask = false,
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

    /// The lines of the settings that differ from their defaults; with
    /// `all`, of every setting.
    pub fn write(&self, out: &mut String, all: bool) {
        for (key, on) in [
            ("context-search-new-window", self.search_new_window),
            ("context-search-subfolders", self.search_recursive),
            ("context-search-exact-name", self.search_exact_name),
            ("rename-in-dialog", self.rename_dialog),
            ("file-conflicts-ask", self.conflict_ask),
        ] {
            if on || all {
                out.push_str(&format!("{key} = {on}\n"));
            }
        }
        let desktop = self.shortcut_kind == crate::fileops_shortcut::Kind::Desktop;
        if desktop || all {
            let kind = if desktop { "desktop" } else { "symlink" };
            out.push_str(&format!("shortcut-kind = {kind}\n"));
        }
        let skip = self.shortcut_collision == crate::fileops_shortcut::Collision::Skip;
        if skip || all {
            let collision = if skip { "skip" } else { "number" };
            out.push_str(&format!("shortcut-collision = {collision}\n"));
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::settings::{Page, Settings};

    #[test]
    fn conflict_choice_defaults_to_keep_both_and_roundtrips() {
        assert!(!Settings::default().fileops.conflict_ask);
        let mut s = Settings::parse("file-conflicts-ask = true");
        assert!(s.fileops.conflict_ask);
        assert_eq!(Settings::parse(&s.to_text()).fileops, s.fileops);
        s.reset(Page::Window);
        assert!(!s.fileops.conflict_ask);
        assert_eq!(
            Settings::parse("file-conflicts-ask = invalid")
                .problems
                .len(),
            1
        );
    }

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
    #[test]
    fn context_search_choices_are_persisted_and_reset_on_the_window_page() {
        let mut s = Settings::parse(
            "context-search-new-window = true\ncontext-search-subfolders = true\ncontext-search-exact-name = true",
        );
        assert!(
            s.fileops.search_new_window
                && s.fileops.search_recursive
                && s.fileops.search_exact_name
        );
        assert_eq!(Settings::parse(&s.to_text()).fileops, s.fileops);
        s.reset(Page::Search);
        assert!(s.fileops.search_new_window);
        s.reset(Page::Window);
        assert_eq!(s.fileops, super::Settings::default());
        assert_eq!(
            Settings::parse("context-search-subfolders = invalid")
                .problems
                .len(),
            1
        );
    }
}
