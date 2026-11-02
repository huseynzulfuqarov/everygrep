//! Files on the clipboard: Cut, Copy and Paste of result items.
//!
//! File managers put files on the clipboard in two formats:
//! `x-special/gnome-copied-files` (`copy` or `cut`, then one `file://` URI
//! per line) and `text/uri-list` (RFC 2483: one URI per line, each ended by
//! CR LF). KDE programs such as Dolphin read only the URI list and tell a
//! cut from a copy by a separate `application/x-kde-cutselection` type,
//! `1` for a cut. Slint 1.18 can only put plain text on the clipboard, so
//! these are offered through the X11 clipboard of XWayland (see
//! [`crate::x11clip`]).
//! The window also remembers the files and the operation here. Without
//! XWayland, Cut and Copy put the full paths on the clipboard as text, and
//! Paste in this window uses what was remembered while the clipboard still
//! holds that text. Text from elsewhere is accepted when every line is a
//! file: an absolute path, a `file://` URI, or one of the two formats above
//! written as text.

use std::path::PathBuf;

use crate::open::file_uri;
use crate::portal::file_path;

/// What pasting does with the files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    /// Copies them; the originals stay.
    Copy,
    /// Moves them; the clipboard is emptied afterwards.
    Cut,
}

/// The `x-special/gnome-copied-files` payload: `copy` or `cut`, then one
/// URI per line, with no line end after the last.
pub fn gnome_copied_files(op: Operation, paths: &[PathBuf]) -> Vec<u8> {
    let mut out = String::from(match op {
        Operation::Copy => "copy",
        Operation::Cut => "cut",
    });
    for path in paths {
        out.push('\n');
        out.push_str(&file_uri(path));
    }
    out.into_bytes()
}

/// The `application/x-kde-cutselection` payload: `1` for a cut, `0` for a
/// copy. Without it KDE programs paste a cut as a copy.
pub fn kde_cut_selection(op: Operation) -> Vec<u8> {
    match op {
        Operation::Copy => b"0".to_vec(),
        Operation::Cut => b"1".to_vec(),
    }
}

/// The `text/uri-list` payload: one URI per line, each ended by CR LF.
pub fn uri_list(paths: &[PathBuf]) -> Vec<u8> {
    let mut out = String::new();
    for path in paths {
        out.push_str(&file_uri(path));
        out.push_str("\r\n");
    }
    out.into_bytes()
}

/// Files read from clipboard text, and what pasting them does. `None` when
/// the text is not a list of files: some line is neither an absolute path
/// nor a `file://` URI, or there is no file at all.
///
/// Understood: plain absolute paths, `text/uri-list` (lines starting with
/// `#` are comments), and `x-special/gnome-copied-files`, also behind the
/// `x-special/nautilus-clipboard` line older file managers put in text. A
/// leading `cut` line is the only way to get [`Operation::Cut`].
pub fn parse(text: &str) -> Option<(Operation, Vec<PathBuf>)> {
    let mut lines = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .filter(|l| !l.is_empty())
        .peekable();
    if lines.peek() == Some(&"x-special/nautilus-clipboard") {
        lines.next();
    }
    let op = match lines.peek() {
        Some(&"copy") => Some(Operation::Copy),
        Some(&"cut") => Some(Operation::Cut),
        _ => None,
    };
    if op.is_some() {
        lines.next();
    }
    let mut paths = Vec::new();
    for line in lines {
        if line.starts_with('#') {
            continue;
        }
        let path = if line.starts_with('/') {
            PathBuf::from(line)
        } else {
            file_path(line)?
        };
        paths.push(path);
    }
    (!paths.is_empty()).then(|| (op.unwrap_or(Operation::Copy), paths))
}

/// Files this window put on the clipboard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Held {
    pub op: Operation,
    pub paths: Vec<PathBuf>,
    /// The text put on the clipboard with them.
    pub text: String,
}

/// What to paste, given the files this window put on the clipboard and the
/// text the clipboard holds now. The remembered files count only while the
/// clipboard still holds their text; something copied since replaces them.
pub fn to_paste(held: Option<&Held>, clipboard_text: &str) -> Option<(Operation, Vec<PathBuf>)> {
    match held {
        Some(h) if h.text == clipboard_text && !h.paths.is_empty() => Some((h.op, h.paths.clone())),
        _ => parse(clipboard_text),
    }
}

/// Whether a cut on the clipboard was pasted here already: the same files
/// as the last cut moved (`moved`), and none of them is left. Such a cut
/// offers nothing to paste. A cut of the same paths made again after they
/// came back is pasted as usual.
pub fn moved_already(op: Operation, paths: &[PathBuf], moved: Option<&[PathBuf]>) -> bool {
    op == Operation::Cut
        && moved == Some(paths)
        && paths.iter().all(|p| std::fs::symlink_metadata(p).is_err())
}

/// The text to put on the clipboard once the offer of the files `offered`
/// was answered: their paths, when the clipboard could not be `taken` with
/// the files, unless this window put something else on it meanwhile
/// (`current` is what it holds now).
pub fn fallback_text<'a>(
    current: Option<&Held>,
    offered: &'a Held,
    taken: bool,
) -> Option<&'a str> {
    (!taken && current == Some(offered)).then_some(offered.text.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    fn paths() -> Vec<PathBuf> {
        vec![
            PathBuf::from("/home/u/My Files/it's #1.txt"),
            PathBuf::from("/tmp/şəkil.png"),
            PathBuf::from(OsStr::from_bytes(b"/x/\xff%")),
        ]
    }

    #[test]
    fn a_late_answer_never_replaces_a_newer_copy() {
        let held = |name: &str| Held {
            op: Operation::Copy,
            paths: vec![PathBuf::from(name)],
            text: name.into(),
        };
        let (a, b) = (held("/a"), held("/b"));
        // Taken with the files: no text needed.
        assert_eq!(fallback_text(Some(&a), &a, true), None);
        // Not taken: the text, while the files are still the latest.
        assert_eq!(fallback_text(Some(&a), &a, false), Some("/a"));
        // Copied again meanwhile, or pasted and let go: left alone.
        assert_eq!(fallback_text(Some(&b), &a, false), None);
        assert_eq!(fallback_text(None, &a, false), None);
    }

    #[test]
    fn gnome_copied_files_bytes() {
        assert_eq!(
            gnome_copied_files(Operation::Copy, &paths()),
            b"copy\n\
              file:///home/u/My%20Files/it%27s%20%231.txt\n\
              file:///tmp/%C5%9F%C9%99kil.png\n\
              file:///x/%FF%25"
        );
        assert_eq!(
            gnome_copied_files(Operation::Cut, &paths()[1..2]),
            b"cut\nfile:///tmp/%C5%9F%C9%99kil.png"
        );
        assert_eq!(gnome_copied_files(Operation::Copy, &[]), b"copy");
    }

    #[test]
    fn kde_cut_selection_bytes() {
        assert_eq!(kde_cut_selection(Operation::Cut), b"1");
        assert_eq!(kde_cut_selection(Operation::Copy), b"0");
    }

    #[test]
    fn uri_list_bytes() {
        assert_eq!(
            uri_list(&paths()),
            b"file:///home/u/My%20Files/it%27s%20%231.txt\r\n\
              file:///tmp/%C5%9F%C9%99kil.png\r\n\
              file:///x/%FF%25\r\n"
        );
        assert_eq!(uri_list(&[]), b"");
    }

    #[test]
    fn payloads_read_back_as_the_same_files() {
        let p = paths();
        for (bytes, op) in [
            (gnome_copied_files(Operation::Cut, &p), Operation::Cut),
            (gnome_copied_files(Operation::Copy, &p), Operation::Copy),
            (uri_list(&p), Operation::Copy),
        ] {
            let text = String::from_utf8(bytes).unwrap();
            assert_eq!(parse(&text), Some((op, p.clone())), "{text}");
        }
    }

    #[test]
    fn text_forms_that_are_files() {
        assert_eq!(
            parse("/a/b c\n/d\n"),
            Some((
                Operation::Copy,
                vec![PathBuf::from("/a/b c"), PathBuf::from("/d")]
            ))
        );
        assert_eq!(
            parse("x-special/nautilus-clipboard\ncut\nfile:///a%20b\n"),
            Some((Operation::Cut, vec![PathBuf::from("/a b")]))
        );
        assert_eq!(
            parse("# a comment\r\nfile://localhost/srv\r\n"),
            Some((Operation::Copy, vec![PathBuf::from("/srv")]))
        );
    }

    #[test]
    fn text_that_is_not_files() {
        for text in [
            "",
            "\n\n",
            "copy",
            "cut\n",
            "hello",
            "/a\nrelative/b",
            "https://example.org/x",
            "file://server/share",
            "file:///bad%zz",
            "/a\nhttps://example.org/",
        ] {
            assert_eq!(parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn a_moved_cut_is_not_pasted_again() {
        let dir = std::env::temp_dir().join(format!("eg-moved-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let gone = vec![dir.join("gone-1"), dir.join("gone-2")];
        let back = vec![dir.join("back")];
        std::fs::write(&back[0], b"x").unwrap();
        // Moved and gone: nothing to paste.
        assert!(moved_already(Operation::Cut, &gone, Some(&gone)));
        // A copy, other files, or nothing moved yet: pasted as usual.
        assert!(!moved_already(Operation::Copy, &gone, Some(&gone)));
        assert!(!moved_already(Operation::Cut, &gone[..1], Some(&gone)));
        assert!(!moved_already(Operation::Cut, &gone, None));
        // The same path exists again: a new cut.
        assert!(!moved_already(Operation::Cut, &back, Some(&back)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn held_files_count_while_the_clipboard_has_their_text() {
        let held = Held {
            op: Operation::Cut,
            paths: vec![PathBuf::from("/a")],
            text: "/a".into(),
        };
        assert_eq!(
            to_paste(Some(&held), "/a"),
            Some((Operation::Cut, vec![PathBuf::from("/a")]))
        );
        // Copied elsewhere since: the clipboard text wins, as a copy.
        assert_eq!(
            to_paste(Some(&held), "/b"),
            Some((Operation::Copy, vec![PathBuf::from("/b")]))
        );
        assert_eq!(to_paste(Some(&held), "some words"), None);
        assert_eq!(to_paste(None, ""), None);
    }
}
