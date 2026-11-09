//! Row icons for file kinds.
//!
//! The kinds and the classifier live in the core
//! ([`everygrep_core::kind`]); this module maps each kind to its icon.

pub use everygrep_core::kind::{FileKind, classify};

/// The target's icon for a known symlink. Called only by the row-details
/// worker; unavailable targets keep a generic file icon.
pub fn link_icon(path: &std::path::Path) -> FileKind {
    use std::os::unix::ffi::OsStrExt;
    let Ok(target) = std::fs::canonicalize(path) else {
        return FileKind::File;
    };
    match std::fs::metadata(&target) {
        Ok(meta) => classify(
            target.file_name().unwrap_or_default().as_bytes(),
            meta.is_dir(),
        ),
        Err(_) => FileKind::File,
    }
}

/// Icon of a file kind.
pub trait KindIcon {
    /// Index of the kind's icon in the window's icon list.
    fn icon_index(self) -> i32;
    /// Icon file name (without `.svg`) in `ui/icons/types/`.
    #[allow(dead_code)] // Used by tests and documentation.
    fn icon_name(self) -> &'static str;
}

impl KindIcon for FileKind {
    /// The window keeps its icons in kind order, so this is the discriminant
    /// (a test checks the list in `ui/app.slint`).
    fn icon_index(self) -> i32 {
        self as i32
    }

    fn icon_name(self) -> &'static str {
        match self {
            FileKind::Folder => "folder",
            FileKind::File => "file",
            FileKind::Text => "text",
            FileKind::Document => "document",
            FileKind::Pdf => "pdf",
            FileKind::Spreadsheet => "spreadsheet",
            FileKind::Presentation => "presentation",
            FileKind::Image => "image",
            FileKind::Audio => "audio",
            FileKind::Video => "video",
            FileKind::Archive => "archive",
            FileKind::Code => "code",
            FileKind::Executable => "executable",
            FileKind::Font => "font",
            FileKind::DiskImage => "disk-image",
            FileKind::Link => "link",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_has_an_icon_file() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/ui/icons/types");
        for kind in FileKind::ALL {
            let path = format!("{dir}/{}.svg", kind.icon_name());
            assert!(std::path::Path::new(&path).is_file(), "{path}");
        }
    }

    #[test]
    fn window_icon_list_follows_the_kind_order() {
        let ui = include_str!("../ui/app.slint");
        let start = ui.find("out property <[image]> types").expect("icon list");
        let list = &ui[start..start + ui[start..].find("];").expect("list end")];
        let names: Vec<&str> = list
            .split("@image-url(\"icons/types/")
            .skip(1)
            .map(|s| &s[..s.find(".svg").expect("svg")])
            .collect();
        let expected: Vec<&str> = FileKind::ALL.iter().map(|k| k.icon_name()).collect();
        assert_eq!(names, expected);
        for (i, kind) in FileKind::ALL.iter().enumerate() {
            assert_eq!(kind.icon_index(), i as i32);
        }
    }
}
