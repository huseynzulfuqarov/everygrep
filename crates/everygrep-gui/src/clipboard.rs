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
//! Paste in this window copies those paths: text alone cannot prove that
//! the original Cut offer is still owned by this window. Text from elsewhere is accepted when every line is a
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
    /// The items as they were when Cut or Copy was used.
    pub chosen: crate::actions::Chosen,
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

/// How many of this window's Cut and Copy payloads are remembered.
const RECENT: usize = 32;

/// The files this window put on the clipboard this session, newest last,
/// each with the identities recorded at Cut or Copy. Paste finds the
/// identities through the payload it reads, so a newer Cut or Copy made
/// while the clipboard answers never takes them away.
#[derive(Debug, Default)]
pub struct Recent(std::collections::VecDeque<Held>);

impl Recent {
    pub fn remember(&mut self, held: Held) {
        if self.0.len() == RECENT {
            self.0.pop_front();
        }
        self.0.push_back(held);
    }

    /// The newest payload put on the clipboard as `text`.
    fn by_text(&self, text: &str) -> Option<&Held> {
        self.0.iter().rev().find(|h| h.text == text)
    }
}

/// What Paste takes: the operation, the files, and the items as they were
/// chosen when this window put them on the clipboard (`None` for files
/// another program put there).
pub type ToPaste = (Operation, Vec<PathBuf>, Option<crate::actions::Chosen>);

/// What to paste. `read` is what the X11 clipboard gave: the files and,
/// when this window served them, the payload they were served from (its
/// identities go with them). Files served by another program are taken as
/// they are. Without a read, the text clipboard (`text`) is used: text this
/// window put there itself (any remembered payload, not only the latest)
/// is pasted as a copy of that payload's items as they were chosen. Text
/// equality can preserve lossless paths, but cannot prove clipboard
/// ownership or Cut intent: only file-format offers (read separately) or an
/// explicit cut payload can request a move.
pub fn to_paste_from(
    recent: &Recent,
    read: Option<crate::x11clip::Answer>,
    text: impl FnOnce() -> String,
) -> Option<ToPaste> {
    if let Some(((op, paths), served)) = read {
        return Some((op, paths, served.map(|h| h.chosen)));
    }
    let text = text();
    if let Some(h) = recent.by_text(&text).filter(|h| !h.paths.is_empty()) {
        return Some((Operation::Copy, h.paths.clone(), Some(h.chosen.clone())));
    }
    parse(&text).map(|(op, paths)| (op, paths, None))
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

    fn chosen(paths: &[PathBuf]) -> crate::actions::Chosen {
        crate::actions::Chosen::of(paths)
    }

    #[test]
    fn paste_keeps_the_identities_of_the_cut_it_reads_after_a_newer_copy() {
        let dir = std::env::temp_dir().join(format!("everygrep-paste-ids-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();
        let held = |op, p: &PathBuf| Held {
            op,
            paths: vec![p.clone()],
            text: crate::actions::paths_text(std::slice::from_ref(p), false),
            chosen: chosen(std::slice::from_ref(p)),
        };
        let (cut_a, copy_b) = (held(Operation::Cut, &a), held(Operation::Copy, &b));
        let mut recent = Recent::default();
        recent.remember(cut_a.clone());
        recent.remember(copy_b.clone());
        let no_text = || -> String { unreachable!() };
        // The X11 read returns the Cut this window served before Copy
        // replaced what it holds.
        let got = to_paste_from(
            &recent,
            Some(((Operation::Cut, vec![a.clone()]), Some(cut_a.clone()))),
            no_text,
        );
        assert_eq!(
            got,
            Some((Operation::Cut, vec![a.clone()], Some(cut_a.chosen.clone())))
        );
        // The text clipboard still holds the paths of the earlier payload.
        let got = to_paste_from(&recent, None, || cut_a.text.clone());
        assert_eq!(
            got,
            Some((Operation::Copy, vec![a.clone()], Some(cut_a.chosen.clone())))
        );
        // Files another program put on the clipboard: taken as they are.
        let got = to_paste_from(
            &recent,
            Some(((Operation::Copy, vec![a.clone()]), None)),
            no_text,
        );
        assert_eq!(got, Some((Operation::Copy, vec![a.clone()], None)));
        let got = to_paste_from(&recent, None, || "/elsewhere".into());
        assert_eq!(
            got,
            Some((Operation::Copy, vec!["/elsewhere".into()], None))
        );
        // The latest payload, read back as served.
        let got = to_paste_from(
            &recent,
            Some(((Operation::Copy, vec![b.clone()]), Some(copy_b.clone()))),
            no_text,
        );
        assert_eq!(got, Some((Operation::Copy, vec![b], Some(copy_b.chosen))));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_late_answer_never_replaces_a_newer_copy() {
        let held = |name: &str| Held {
            op: Operation::Copy,
            paths: vec![PathBuf::from(name)],
            text: name.into(),
            chosen: Default::default(),
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
    fn a_new_identical_path_text_cannot_repeat_an_old_cut() {
        let dir = std::env::temp_dir().join(format!("everygrep-stale-cut-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let src = dir.join("file");
        let dest = dir.join("dest");
        std::fs::write(&src, "selected bytes").unwrap();
        std::fs::create_dir(&dest).unwrap();
        let text = crate::actions::paths_text(std::slice::from_ref(&src), false);
        let old = Held {
            op: Operation::Cut,
            paths: vec![src.clone()],
            text: text.clone(),
            chosen: Default::default(),
        };
        let mut recent = Recent::default();
        recent.remember(old);
        let (op, paths, _) = to_paste_from(&recent, None, || text.clone()).unwrap();
        assert_eq!(op, Operation::Copy);
        let out = crate::transfer::paste(
            op,
            &paths,
            &dest,
            &everygrep_core::progress::Progress::new(),
        );
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        assert_eq!(std::fs::read(&src).unwrap(), b"selected bytes");
        assert_eq!(std::fs::read(dest.join("file")).unwrap(), b"selected bytes");
        // Explicit file payloads still carry Cut intent.
        let payload = String::from_utf8(gnome_copied_files(Operation::Cut, &[src])).unwrap();
        assert_eq!(parse(&payload).unwrap().0, Operation::Cut);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn remembered_text_paths_always_copy_without_verified_ownership() {
        let held = Held {
            op: Operation::Cut,
            paths: vec![PathBuf::from("/a")],
            text: "/a".into(),
            chosen: Default::default(),
        };
        let mut recent = Recent::default();
        recent.remember(held);
        let paste = |recent: &Recent, text: &str| {
            to_paste_from(recent, None, || text.into()).map(|(op, paths, _)| (op, paths))
        };
        assert_eq!(
            paste(&recent, "/a"),
            Some((Operation::Copy, vec![PathBuf::from("/a")]))
        );
        // Copied elsewhere since: the clipboard text wins, as a copy.
        assert_eq!(
            paste(&recent, "/b"),
            Some((Operation::Copy, vec![PathBuf::from("/b")]))
        );
        assert_eq!(paste(&recent, "some words"), None);
        assert_eq!(paste(&Recent::default(), ""), None);
    }
}
