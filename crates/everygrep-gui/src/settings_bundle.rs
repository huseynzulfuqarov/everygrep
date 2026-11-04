//! Optional saved searches in a settings export; history and tabs stay separate.
use crate::{
    saved_searches::Library,
    settings::{self, Settings},
};
use std::{io, path::Path};
const PREFIX: &str = "# saved-searches-v1 = ";
pub fn export(settings: &Settings, library_path: &Path) -> io::Result<String> {
    let mut text = settings.to_text();
    if settings.export_filters || settings.export_bookmarks {
        let library = Library::load(library_path)?
            .portable(settings.export_filters, settings.export_bookmarks);
        text.push_str(PREFIX);
        text.push_str(&settings::escape(library.text().as_bytes()));
        text.push('\n');
    }
    Ok(text)
}
pub fn attachment(text: &str) -> io::Result<Option<Library>> {
    let mut found = None;
    for encoded in text.lines().filter_map(|l| l.strip_prefix(PREFIX)) {
        if found.is_some() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let bytes = settings::unescape(encoded).ok_or(io::ErrorKind::InvalidData)?;
        let decoded = String::from_utf8(bytes).map_err(|_| io::ErrorKind::InvalidData)?;
        found = Some(Library::parse(&decoded)?);
    }
    Ok(found)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn optional_library_bundle_is_portable_idempotent_and_excludes_private_history() {
        let mut library = Library::default();
        library
            .put_filter(
                None,
                "Images",
                "ext:png",
                Default::default(),
                Default::default(),
            )
            .unwrap();
        let portable = library.portable(true, false);
        let text = format!(
            "theme = dark\n{PREFIX}{}\n",
            settings::escape(portable.text().as_bytes())
        );
        assert_eq!(
            Settings::parse(&text).theme,
            crate::settings::ThemeChoice::Dark
        );
        let read = attachment(&text).unwrap().unwrap();
        assert!(read.bookmarks.is_empty() && read.shortcuts.is_empty());
        let mut destination = Library::default();
        destination.merge_import(&read).unwrap();
        let once = destination.text();
        destination.merge_import(&read).unwrap();
        assert_eq!(once, destination.text());
        assert!(attachment("theme = dark").unwrap().is_none());
        assert!(attachment(&format!("{PREFIX}invalid")).is_err());
        assert!(!text.contains("search-history") && !text.contains("session"));
        let mut invalid = read.clone();
        invalid.filters[0].options.regex = true;
        invalid.filters[0].search = "[".into();
        assert!(destination.merge_import(&invalid).is_err());
        assert_eq!(destination.text(), once);
        let folder =
            std::env::temp_dir().join(format!("everygrep-settings-bundle-{}", std::process::id()));
        std::fs::create_dir(&folder).unwrap();
        let library_path = folder.join("saved-searches");
        std::fs::write(&library_path, library.text()).unwrap();
        let selected = Settings::parse("export-filters = true");
        let exported = export(&selected, &library_path).unwrap();
        assert_eq!(
            attachment(&exported).unwrap().unwrap().filters,
            library.filters
        );
        std::fs::remove_dir_all(folder).unwrap();
        let disabled = Settings::default();
        assert!(
            !export(
                &disabled,
                &std::env::temp_dir().join("everygrep-unused-library")
            )
            .unwrap()
            .contains(PREFIX)
        );
    }
}

#[cfg(test)]
mod bookmark_tests {
    use super::*;
    #[test]
    fn bookmark_only_exports_keep_frozen_filters_and_settings_choices_roundtrip() {
        let s = Settings::parse("export-filters = true\nexport-bookmarks = true");
        assert!(s.export_filters && s.export_bookmarks);
        assert_eq!(Settings::parse(&s.to_text()), s);
        let mut library = Library::default();
        let filter = library.filters[0].clone();
        library
            .put_bookmark(
                None,
                crate::saved_searches::Bookmark {
                    id: 0,
                    name: "Work".into(),
                    search: "ext:rs".into(),
                    options: Default::default(),
                    filter: filter.clone(),
                    sort_column: 0,
                    descending: false,
                    columns: None,
                    kind: Default::default(),
                    folder: 0,
                },
            )
            .unwrap();
        let portable = library.portable(false, true);
        assert!(portable.filters.is_empty());
        let read = Library::parse(&portable.text()).unwrap();
        let mut dest = Library::default();
        dest.merge_import(&read).unwrap();
        assert_eq!(dest.bookmarks[0].filter, filter);
        let once = dest.text();
        dest.merge_import(&read).unwrap();
        assert_eq!(dest.text(), once);
        assert!(attachment(&format!("{PREFIX}bad\n{PREFIX}bad\n")).is_err());
    }
}
