//! Words the window shows for core concepts: filter names, file types and
//! matching modes. They are kept together here so they can be translated.

use everygrep_core::filter::Filter;
use everygrep_core::kind::FileKind;
use everygrep_core::query::QueryOptions;

/// Name of a filter in the filter selector.
pub fn filter(f: Filter) -> &'static str {
    match f {
        Filter::All => "All",
        Filter::Audio => "Audio",
        Filter::Archive => "Archives",
        Filter::Code => "Code",
        Filter::Document => "Documents",
        Filter::Executable => "Executables",
        Filter::Folder => "Folders",
        Filter::Picture => "Pictures",
        Filter::Video => "Videos",
    }
}

/// The noun for a kind: after an extension (`PNG image`) and on its own.
fn kind_nouns(kind: FileKind) -> (&'static str, &'static str) {
    match kind {
        FileKind::Folder => ("folder", "Folder"),
        FileKind::File => ("file", "File"),
        FileKind::Text => ("text", "Text"),
        FileKind::Document | FileKind::Pdf => ("document", "Document"),
        FileKind::Spreadsheet => ("spreadsheet", "Spreadsheet"),
        FileKind::Presentation => ("presentation", "Presentation"),
        FileKind::Image => ("image", "Image"),
        FileKind::Audio => ("audio", "Audio"),
        FileKind::Video => ("video", "Video"),
        FileKind::Archive => ("archive", "Archive"),
        FileKind::Code => ("source", "Source"),
        FileKind::Executable => ("program", "Program"),
        FileKind::Font => ("font", "Font"),
        FileKind::DiskImage => ("disk image", "Disk image"),
        FileKind::Link => ("link", "Link"),
    }
}

/// Text of the Type column: `Folder`, `PNG image`, `TAR.GZ archive` would be
/// `GZ archive` (the last extension), `Source` for a `Makefile`.
pub fn type_name(kind: FileKind, extension: &str) -> String {
    let (after, alone) = kind_nouns(kind);
    if kind == FileKind::Folder || extension.is_empty() {
        alone.to_owned()
    } else {
        format!("{} {after}", extension.to_uppercase())
    }
}

/// The extension of a name as written, without the dot; empty for folders and
/// for names without one. A dot at the very start (`.bashrc`) does not begin
/// an extension.
pub fn extension(name: &str, is_dir: bool) -> &str {
    match name.rfind('.') {
        Some(dot) if dot > 0 && !is_dir => &name[dot + 1..],
        _ => "",
    }
}

/// Short words for the matching modes that are on, for the status line.
pub fn mode_badges(o: &QueryOptions) -> Vec<&'static str> {
    [
        (o.case_sensitive, "Case"),
        (o.whole_words, "Whole words"),
        (o.match_path, "Path"),
        (o.match_diacritics, "Diacritics"),
    ]
    .into_iter()
    .filter_map(|(on, word)| on.then_some(word))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use everygrep_core::kind::classify;

    fn type_of(name: &str, is_dir: bool) -> String {
        type_name(classify(name.as_bytes(), is_dir), extension(name, is_dir))
    }

    #[test]
    fn type_names() {
        assert_eq!(type_of("photos.jpg", true), "Folder");
        assert_eq!(type_of("a.png", false), "PNG image");
        assert_eq!(type_of("Report.Pdf", false), "PDF document");
        assert_eq!(type_of("linux.tar.gz", false), "GZ archive");
        assert_eq!(type_of("Makefile", false), "Source");
        assert_eq!(type_of("notes", false), "File");
        assert_eq!(type_of("a.xyz", false), "XYZ file");
        assert_eq!(type_of(".bashrc", false), "Source");
    }

    #[test]
    fn extensions_keep_their_case() {
        assert_eq!(extension("IMG.JPG", false), "JPG");
        assert_eq!(extension("a.b.c", false), "c");
        assert_eq!(extension(".bashrc", false), "");
        assert_eq!(extension("trailing.", false), "");
        assert_eq!(extension("dir.d", true), "");
    }

    #[test]
    fn every_filter_has_a_distinct_name() {
        let mut names: Vec<&str> = Filter::ALL.into_iter().map(filter).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Filter::ALL.len());
    }

    #[test]
    fn badges_follow_the_options() {
        assert!(mode_badges(&QueryOptions::default()).is_empty());
        let o = QueryOptions {
            case_sensitive: true,
            match_diacritics: true,
            ..QueryOptions::default()
        };
        assert_eq!(mode_badges(&o), ["Case", "Diacritics"]);
    }
}
