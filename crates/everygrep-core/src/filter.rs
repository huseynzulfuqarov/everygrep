//! Result filters: a quick way to limit results to one broad kind of entry.
//!
//! A filter is combined with the query text (both must match). It uses the
//! name-based [`classify`](crate::kind::classify), so the lists of extensions
//! live in one place, [`crate::kind`].
//!
//! | Filter | Accepts kinds |
//! |---|---|
//! | `all` | every entry |
//! | `audio` | audio |
//! | `archive` | archives (zip, 7z, tar and its compressed forms, packages) |
//! | `code` | source code, markup, config and build files |
//! | `document` | text, documents, PDF, spreadsheets, presentations |
//! | `executable` | programs, scripts and launchers by extension (sh, AppImage, desktop, exe, …), and files of no known type with an execute bit |
//! | `folder` | folders |
//! | `image` | images, including camera raw and editor formats |
//! | `video` | video |
//!
//! The same filters can be written in the query as words, their identifiers
//! followed by a colon ([`Filter::from_query_word`]): `audio:`, `archive:`,
//! `code:`, `document:`, `executable:`, `image:` and `video:`.
//!
//! The index keeps no permission bits. For `executable`, a file whose name
//! gives no known type is looked up on disk while searching (`lstat`, links
//! not followed): it passes when it is a regular file with an execute bit.
//! Files with a known type (`notes.txt`) do not pass by their bits, so
//! drives that mark every file executable (FAT, NTFS) do not fill the
//! results. That check is in [`crate::query`], which has the paths.

use crate::kind::FileKind;

/// One of the built-in filters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Filter {
    #[default]
    All,
    Audio,
    Archive,
    Code,
    Document,
    Executable,
    Folder,
    Image,
    Video,
}

impl Filter {
    /// Every filter, in the order a selector shows them.
    pub const ALL: [Filter; 9] = [
        Filter::All,
        Filter::Audio,
        Filter::Archive,
        Filter::Code,
        Filter::Document,
        Filter::Executable,
        Filter::Folder,
        Filter::Image,
        Filter::Video,
    ];

    /// Stable lowercase identifier, used on the command line.
    pub fn id(self) -> &'static str {
        match self {
            Filter::All => "all",
            Filter::Audio => "audio",
            Filter::Archive => "archive",
            Filter::Code => "code",
            Filter::Document => "document",
            Filter::Executable => "executable",
            Filter::Folder => "folder",
            Filter::Image => "image",
            Filter::Video => "video",
        }
    }

    /// The filter with identifier `id` (ASCII case ignored).
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|f| f.id().eq_ignore_ascii_case(id))
    }

    /// The filter that the query word `word` (without the `:`, ASCII case
    /// ignored) stands for: its identifier.
    pub fn from_query_word(word: &str) -> Option<Self> {
        // `all:` and `folder:` are not filter words: `folder:` is a modifier.
        let found = Self::from_id(word)?;
        (!matches!(found, Filter::All | Filter::Folder)).then_some(found)
    }

    /// Whether an entry of `kind` passes the filter.
    pub fn accepts(self, kind: FileKind) -> bool {
        use FileKind as K;
        match self {
            Filter::All => true,
            Filter::Audio => kind == K::Audio,
            Filter::Archive => kind == K::Archive,
            Filter::Code => kind == K::Code,
            Filter::Document => matches!(
                kind,
                K::Text | K::Document | K::Pdf | K::Spreadsheet | K::Presentation
            ),
            Filter::Executable => kind == K::Executable,
            Filter::Folder => kind == K::Folder,
            Filter::Image => kind == K::Image,
            Filter::Video => kind == K::Video,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kind::classify;

    fn passes(filter: Filter, name: &str, is_dir: bool) -> bool {
        filter.accepts(classify(name.as_bytes(), is_dir))
    }

    #[test]
    fn ids_round_trip() {
        for f in Filter::ALL {
            assert_eq!(Filter::from_id(f.id()), Some(f));
        }
        assert_eq!(Filter::from_id("Image"), Some(Filter::Image));
        assert_eq!(Filter::from_id("images"), None);
    }

    #[test]
    fn query_words() {
        assert_eq!(Filter::from_query_word("Archive"), Some(Filter::Archive));
        assert_eq!(Filter::from_query_word("IMAGE"), Some(Filter::Image));
        assert_eq!(
            Filter::from_query_word("executable"),
            Some(Filter::Executable)
        );
        assert_eq!(Filter::from_query_word("document"), Some(Filter::Document));
        assert_eq!(Filter::from_query_word("audio"), Some(Filter::Audio));
        assert_eq!(Filter::from_query_word("code"), Some(Filter::Code));
        assert_eq!(Filter::from_query_word("video"), Some(Filter::Video));
        assert_eq!(Filter::from_query_word("all"), None);
        assert_eq!(Filter::from_query_word("folder"), None);
        assert_eq!(Filter::from_query_word("photos"), None);
    }

    #[test]
    fn each_filter_takes_its_kinds_only() {
        assert!(passes(Filter::All, "x.bin", false));
        assert!(passes(Filter::All, "dir", true));
        assert!(passes(Filter::Audio, "song.FLAC", false));
        assert!(!passes(Filter::Audio, "clip.mkv", false));
        assert!(passes(Filter::Archive, "linux.tar.zst", false));
        assert!(!passes(Filter::Archive, "disk.iso", false));
        assert!(passes(Filter::Code, "main.rs", false));
        assert!(passes(Filter::Code, "Makefile", false));
        for doc in ["a.txt", "a.odt", "a.pdf", "a.xlsx", "a.pptx", "README"] {
            assert!(passes(Filter::Document, doc, false), "{doc}");
        }
        assert!(!passes(Filter::Document, "a.rs", false));
        assert!(passes(Filter::Executable, "Tool.AppImage", false));
        assert!(passes(Filter::Executable, "install.sh", false));
        assert!(!passes(Filter::Executable, "notes", false));
        assert!(passes(Filter::Folder, "photos.jpg", true));
        assert!(!passes(Filter::Folder, "photos.jpg", false));
        assert!(passes(Filter::Image, "IMG_1.HEIC", false));
        assert!(!passes(Filter::Image, "pictures", true));
        assert!(passes(Filter::Video, "film.webm", false));
    }
}
