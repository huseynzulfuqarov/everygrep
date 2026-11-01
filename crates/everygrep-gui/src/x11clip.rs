//! Files on the clipboard for other programs, through the X11 `CLIPBOARD`
//! selection of the XWayland display.
//!
//! Slint puts only text on the clipboard. GNOME has no Wayland protocol
//! that lets a program offer other types without a focused surface of its
//! own, but it passes the X11 clipboard of XWayland on to Wayland programs
//! with every type it offers. So Cut and Copy make a hidden X11 window own
//! `CLIPBOARD` and offer the files as `x-special/gnome-copied-files`,
//! `text/uri-list` with `application/x-kde-cutselection` (how KDE programs
//! tell a cut), and text (the full paths). Paste reads another owner's
//! files in that order.
//!
//! One thread owns the connection and answers the X server; the window
//! talks to it through a channel and wakes it with a client message to its
//! own window. Without `DISPLAY`, or when the connection fails, there is no
//! [`X11Clipboard`] and the window keeps using Slint's text clipboard.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
    Property, SELECTION_NOTIFY_EVENT, SelectionNotifyEvent, SelectionRequestEvent, Window,
    WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, CURRENT_TIME, NONE};

use crate::clipboard::{self, Held, Operation};

/// How long the window waits for the clipboard thread to take the
/// clipboard.
const OFFER_WAIT: Duration = Duration::from_millis(500);
/// How long the window waits for another program's clipboard.
const READ_WAIT: Duration = Duration::from_secs(1);
/// Largest clipboard content read from another program.
const MAX_READ: usize = 32 << 20;
/// Room for the request header when content is written in one request.
const REQUEST_HEADER: usize = 64;

/// A clipboard type this program offers or reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// The list of the other types (ICCCM).
    Targets,
    GnomeCopiedFiles,
    UriList,
    /// Whether the URI list is a cut, for KDE programs.
    KdeCutSelection,
    Utf8String,
    TextPlainUtf8,
}

impl Target {
    /// Every type offered, in the order of the `TARGETS` answer.
    pub const ALL: [Target; 6] = [
        Target::Targets,
        Target::GnomeCopiedFiles,
        Target::UriList,
        Target::KdeCutSelection,
        Target::Utf8String,
        Target::TextPlainUtf8,
    ];

    /// Types asked of another program, most telling first: only the first
    /// says whether the files were cut.
    pub const READ_ORDER: [Target; 3] = [
        Target::GnomeCopiedFiles,
        Target::UriList,
        Target::Utf8String,
    ];

    /// The X11 atom name.
    pub fn name(self) -> &'static str {
        match self {
            Target::Targets => "TARGETS",
            Target::GnomeCopiedFiles => "x-special/gnome-copied-files",
            Target::UriList => "text/uri-list",
            Target::KdeCutSelection => "application/x-kde-cutselection",
            Target::Utf8String => "UTF8_STRING",
            Target::TextPlainUtf8 => "text/plain;charset=utf-8",
        }
    }

    /// The type an atom name stands for (the thread maps atoms directly).
    #[cfg(test)]
    fn by_name(name: &str) -> Option<Target> {
        Target::ALL.into_iter().find(|t| t.name() == name)
    }
}

/// The bytes served for `target`; `None` for `TARGETS`, which is a list of
/// atoms.
pub fn payload(target: Target, held: &Held) -> Option<Vec<u8>> {
    match target {
        Target::Targets => None,
        Target::GnomeCopiedFiles => Some(clipboard::gnome_copied_files(held.op, &held.paths)),
        Target::UriList => Some(clipboard::uri_list(&held.paths)),
        Target::KdeCutSelection => Some(clipboard::kde_cut_selection(held.op)),
        Target::Utf8String | Target::TextPlainUtf8 => Some(held.text.as_bytes().to_vec()),
    }
}

/// Files in content read from another program as `target`. Text and URI
/// lists are always a copy; only `x-special/gnome-copied-files` can say
/// `cut`.
pub fn read_payload(target: Target, bytes: &[u8]) -> Option<(Operation, Vec<PathBuf>)> {
    let text = std::str::from_utf8(bytes).ok()?;
    let (op, paths) = clipboard::parse(text)?;
    match target {
        Target::GnomeCopiedFiles => Some((op, paths)),
        _ => Some((Operation::Copy, paths)),
    }
}

/// Requests to the clipboard thread.
enum Cmd {
    Offer(Held, Sender<bool>),
    /// Gives the clipboard up if this program holds it.
    Clear,
    Read(Sender<Option<(Operation, Vec<PathBuf>)>>),
    Quit,
}

/// This program's side of the X11 clipboard.
pub struct X11Clipboard {
    conn: Arc<RustConnection>,
    window: Window,
    wake: Atom,
    tx: Sender<Cmd>,
    owns: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl X11Clipboard {
    /// Connects to `DISPLAY`. `None` when it is not set or cannot be
    /// reached.
    pub fn connect() -> Option<Self> {
        let display = std::env::var("DISPLAY").ok().filter(|d| !d.is_empty())?;
        let (conn, screen) = RustConnection::connect(Some(&display)).ok()?;
        let conn = Arc::new(conn);
        let root = conn.setup().roots.get(screen)?.root;
        let window = conn.generate_id().ok()?;
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            COPY_FROM_PARENT,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .ok()?;
        let atoms = Atoms::intern(&conn).ok()?;
        conn.flush().ok()?;
        let wake = atoms.wake;
        let (tx, rx) = mpsc::channel();
        let owns = Arc::new(AtomicBool::new(false));
        let max_write = conn.maximum_request_bytes().saturating_sub(REQUEST_HEADER);
        let mut worker = Worker {
            conn: conn.clone(),
            window,
            atoms,
            rx,
            owns: owns.clone(),
            held: None,
            read: None,
            max_write,
        };
        let thread = std::thread::Builder::new()
            .name("x11-clipboard".into())
            .spawn(move || worker.run())
            .ok()?;
        Some(Self {
            conn,
            window,
            wake,
            tx,
            owns,
            thread: Some(thread),
        })
    }

    /// Takes the clipboard with `held`. `false` when it could not be taken
    /// in time; the caller then puts the text on the clipboard instead.
    pub fn offer(&self, held: Held) -> bool {
        let (reply, answer) = mpsc::channel();
        self.send(Cmd::Offer(held, reply)) && answer.recv_timeout(OFFER_WAIT).unwrap_or(false)
    }

    /// Gives the clipboard up if this program still holds it.
    pub fn clear(&self) {
        self.send(Cmd::Clear);
    }

    /// Whether this program holds the clipboard.
    pub fn owns(&self) -> bool {
        self.owns.load(Ordering::SeqCst)
    }

    /// The files another program put on the clipboard, if any; waits at
    /// most [`READ_WAIT`].
    pub fn read(&self) -> Option<(Operation, Vec<PathBuf>)> {
        let (reply, answer) = mpsc::channel();
        if !self.send(Cmd::Read(reply)) {
            return None;
        }
        answer.recv_timeout(READ_WAIT).ok().flatten()
    }

    /// Queues `cmd` and wakes the thread. `false` when the thread is gone.
    fn send(&self, cmd: Cmd) -> bool {
        if self.tx.send(cmd).is_err() {
            return false;
        }
        let event = ClientMessageEvent::new(32, self.window, self.wake, [0u32; 5]);
        self.conn
            .send_event(false, self.window, EventMask::NO_EVENT, event)
            .is_ok()
            && self.conn.flush().is_ok()
    }
}

impl Drop for X11Clipboard {
    fn drop(&mut self) {
        self.send(Cmd::Quit);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Atoms {
    clipboard: Atom,
    incr: Atom,
    /// The property of our window that receives another program's content.
    paste: Atom,
    /// Type of the client message that wakes the thread.
    wake: Atom,
    targets: Vec<(Target, Atom)>,
}

impl Atoms {
    fn intern(conn: &RustConnection) -> Result<Self, Box<dyn std::error::Error>> {
        let atom = |name: &str| -> Result<Atom, Box<dyn std::error::Error>> {
            Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
        };
        let mut targets = Vec::new();
        for t in Target::ALL {
            targets.push((t, atom(t.name())?));
        }
        Ok(Self {
            clipboard: atom("CLIPBOARD")?,
            incr: atom("INCR")?,
            paste: atom("EVERYGREP_PASTE")?,
            wake: atom("EVERYGREP_WAKE")?,
            targets,
        })
    }

    fn atom(&self, target: Target) -> Atom {
        self.targets
            .iter()
            .find(|(t, _)| *t == target)
            .map_or(NONE, |(_, a)| *a)
    }

    fn target(&self, atom: Atom) -> Option<Target> {
        self.targets
            .iter()
            .find(|(_, a)| *a == atom)
            .map(|(t, _)| *t)
    }
}

/// A read of another program's clipboard in progress.
struct Read {
    reply: Sender<Option<(Operation, Vec<PathBuf>)>>,
    /// Types still to ask for.
    left: VecDeque<Target>,
    asked: Target,
    /// Content received so far in `INCR` steps.
    incr: Option<Vec<u8>>,
}

struct Worker {
    conn: Arc<RustConnection>,
    window: Window,
    atoms: Atoms,
    rx: Receiver<Cmd>,
    owns: Arc<AtomicBool>,
    held: Option<Held>,
    read: Option<Read>,
    /// Largest content written in one request; larger content is refused.
    max_write: usize,
}

type XResult<T> = Result<T, Box<dyn std::error::Error>>;

impl Worker {
    fn run(&mut self) {
        // Errors from the server for requests about other programs' windows
        // (gone meanwhile) arrive as events and are ignored; a broken
        // connection ends the thread.
        let _ = self.serve();
        self.owns.store(false, Ordering::SeqCst);
        let _ = self.conn.destroy_window(self.window);
        let _ = self.conn.flush();
    }

    fn serve(&mut self) -> XResult<()> {
        loop {
            match self.conn.wait_for_event()? {
                Event::ClientMessage(e)
                    if e.window == self.window && e.type_ == self.atoms.wake =>
                {
                    while let Ok(cmd) = self.rx.try_recv() {
                        if !self.command(cmd)? {
                            return Ok(());
                        }
                    }
                }
                Event::SelectionRequest(e) => self.answer(&e)?,
                Event::SelectionClear(e) if e.selection == self.atoms.clipboard => {
                    // Another program took the clipboard.
                    self.held = None;
                    self.owns.store(false, Ordering::SeqCst);
                }
                Event::SelectionNotify(e) if e.requestor == self.window => {
                    if e.property == NONE {
                        self.ask_next()?;
                    } else {
                        self.receive()?;
                    }
                }
                Event::PropertyNotify(e)
                    if e.window == self.window
                        && e.atom == self.atoms.paste
                        && e.state == Property::NEW_VALUE
                        && self.read.as_ref().is_some_and(|r| r.incr.is_some()) =>
                {
                    self.receive_step()?;
                }
                _ => {}
            }
        }
    }

    /// Runs one request; `false` to stop.
    fn command(&mut self, cmd: Cmd) -> XResult<bool> {
        match cmd {
            Cmd::Offer(held, reply) => {
                self.held = Some(held);
                self.conn
                    .set_selection_owner(self.window, self.atoms.clipboard, CURRENT_TIME)?;
                let owner = self
                    .conn
                    .get_selection_owner(self.atoms.clipboard)?
                    .reply()?
                    .owner;
                let owns = owner == self.window;
                if !owns {
                    self.held = None;
                }
                self.owns.store(owns, Ordering::SeqCst);
                let _ = reply.send(owns);
            }
            Cmd::Clear => {
                if self.owns.swap(false, Ordering::SeqCst) {
                    self.conn
                        .set_selection_owner(NONE, self.atoms.clipboard, CURRENT_TIME)?;
                    self.conn.flush()?;
                }
                self.held = None;
            }
            Cmd::Read(reply) => {
                // A newer read replaces an unanswered one, whose caller
                // has stopped waiting.
                self.read = Some(Read {
                    reply,
                    left: Target::READ_ORDER.into_iter().collect(),
                    asked: Target::Targets,
                    incr: None,
                });
                self.ask_next()?;
            }
            Cmd::Quit => return Ok(false),
        }
        Ok(true)
    }

    /// Answers another program that asks for the clipboard.
    fn answer(&self, e: &SelectionRequestEvent) -> XResult<()> {
        // Obsolete clients give no property; the target is used instead.
        let property = if e.property == NONE {
            e.target
        } else {
            e.property
        };
        let ok = match (&self.held, self.atoms.target(e.target)) {
            (Some(held), Some(target)) if e.selection == self.atoms.clipboard => {
                self.write(e.requestor, property, e.target, target, held)?
            }
            _ => false,
        };
        let notify = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: e.time,
            requestor: e.requestor,
            selection: e.selection,
            target: e.target,
            property: if ok { property } else { NONE },
        };
        self.conn
            .send_event(false, e.requestor, EventMask::NO_EVENT, notify)?;
        self.conn.flush()?;
        Ok(())
    }

    /// Writes `target` to the requestor's property. `false` when the
    /// content is too large for one request; it is refused rather than sent
    /// in `INCR` steps (that is over a hundred thousand paths).
    fn write(
        &self,
        requestor: Window,
        property: Atom,
        type_atom: Atom,
        target: Target,
        held: &Held,
    ) -> XResult<bool> {
        match payload(target, held) {
            None => {
                let atoms: Vec<Atom> = Target::ALL.iter().map(|&t| self.atoms.atom(t)).collect();
                self.conn.change_property32(
                    PropMode::REPLACE,
                    requestor,
                    property,
                    AtomEnum::ATOM,
                    &atoms,
                )?;
            }
            Some(bytes) if bytes.len() > self.max_write => return Ok(false),
            Some(bytes) => {
                self.conn.change_property8(
                    PropMode::REPLACE,
                    requestor,
                    property,
                    type_atom,
                    &bytes,
                )?;
            }
        }
        Ok(true)
    }

    /// Asks the clipboard's owner for the next type, or ends the read.
    fn ask_next(&mut self) -> XResult<()> {
        let Some(read) = self.read.as_mut() else {
            return Ok(());
        };
        read.incr = None;
        match read.left.pop_front() {
            Some(target) => {
                read.asked = target;
                self.conn.convert_selection(
                    self.window,
                    self.atoms.clipboard,
                    self.atoms.atom(target),
                    self.atoms.paste,
                    CURRENT_TIME,
                )?;
                self.conn.flush()?;
            }
            None => self.finish(None),
        }
        Ok(())
    }

    /// Takes the content from our property after the owner answered.
    fn receive(&mut self) -> XResult<()> {
        let reply = self
            .conn
            .get_property(
                true,
                self.window,
                self.atoms.paste,
                AtomEnum::ANY,
                0,
                u32::MAX / 4,
            )?
            .reply()?;
        self.conn.flush()?;
        if reply.type_ == self.atoms.incr {
            // Deleting the property asked for the first step.
            if let Some(read) = self.read.as_mut() {
                read.incr = Some(Vec::new());
            }
            return Ok(());
        }
        self.parsed(&reply.value)
    }

    /// One `INCR` step; an empty one ends the content.
    fn receive_step(&mut self) -> XResult<()> {
        let reply = self
            .conn
            .get_property(
                true,
                self.window,
                self.atoms.paste,
                AtomEnum::ANY,
                0,
                u32::MAX / 4,
            )?
            .reply()?;
        self.conn.flush()?;
        let Some(read) = self.read.as_mut() else {
            return Ok(());
        };
        let Some(buf) = read.incr.as_mut() else {
            return Ok(());
        };
        if reply.value.is_empty() {
            let content = std::mem::take(buf);
            return self.parsed(&content);
        }
        buf.extend_from_slice(&reply.value);
        if buf.len() > MAX_READ {
            // Too large to be a list of files worth pasting.
            self.finish(None);
        }
        Ok(())
    }

    /// Ends the read with the files in `content`, or asks for the next
    /// type when there are none.
    fn parsed(&mut self, content: &[u8]) -> XResult<()> {
        let Some(read) = self.read.as_ref() else {
            return Ok(());
        };
        match read_payload(read.asked, content) {
            Some(found) => {
                self.finish(Some(found));
                Ok(())
            }
            None => self.ask_next(),
        }
    }

    fn finish(&mut self, found: Option<(Operation, Vec<PathBuf>)>) {
        if let Some(read) = self.read.take() {
            let _ = read.reply.send(found);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(op: Operation) -> Held {
        let paths = vec![PathBuf::from("/a b/c.txt"), PathBuf::from("/d")];
        Held {
            op,
            text: crate::actions::paths_text(&paths),
            paths,
        }
    }

    #[test]
    fn targets_are_found_by_their_atom_names() {
        for t in Target::ALL {
            assert_eq!(Target::by_name(t.name()), Some(t));
        }
        assert_eq!(
            Target::by_name("x-special/gnome-copied-files"),
            Some(Target::GnomeCopiedFiles)
        );
        assert_eq!(Target::by_name("text/uri-list"), Some(Target::UriList));
        assert_eq!(
            Target::by_name("application/x-kde-cutselection"),
            Some(Target::KdeCutSelection)
        );
        // Not offered: the answer is a refusal.
        for other in ["MULTIPLE", "TIMESTAMP", "STRING", "image/png", ""] {
            assert_eq!(Target::by_name(other), None, "{other}");
        }
        // TARGETS lists every type, itself first.
        assert_eq!(Target::ALL[0], Target::Targets);
        assert_eq!(Target::ALL.len(), 6);
        // Not read: it holds no files, only whether the URI list was cut.
        assert!(!Target::READ_ORDER.contains(&Target::KdeCutSelection));
    }

    #[test]
    fn each_target_serves_its_format() {
        let h = held(Operation::Cut);
        assert_eq!(payload(Target::Targets, &h), None);
        assert_eq!(
            payload(Target::GnomeCopiedFiles, &h).unwrap(),
            b"cut\nfile:///a%20b/c.txt\nfile:///d"
        );
        assert_eq!(
            payload(Target::UriList, &h).unwrap(),
            b"file:///a%20b/c.txt\r\nfile:///d\r\n"
        );
        assert_eq!(payload(Target::KdeCutSelection, &h).unwrap(), b"1");
        assert_eq!(payload(Target::Utf8String, &h).unwrap(), b"/a b/c.txt\n/d");
        assert_eq!(
            payload(Target::TextPlainUtf8, &h).unwrap(),
            b"/a b/c.txt\n/d"
        );
        let copy = held(Operation::Copy);
        assert!(
            payload(Target::GnomeCopiedFiles, &copy)
                .unwrap()
                .starts_with(b"copy\n")
        );
        assert_eq!(payload(Target::KdeCutSelection, &copy).unwrap(), b"0");
    }

    #[test]
    fn content_from_other_programs() {
        let files = vec![PathBuf::from("/a b"), PathBuf::from("/c")];
        assert_eq!(
            read_payload(Target::GnomeCopiedFiles, b"cut\nfile:///a%20b\nfile:///c"),
            Some((Operation::Cut, files.clone()))
        );
        assert_eq!(
            read_payload(
                Target::GnomeCopiedFiles,
                b"copy\nfile:///a%20b\nfile:///c\n"
            ),
            Some((Operation::Copy, files.clone()))
        );
        assert_eq!(
            read_payload(Target::UriList, b"file:///a%20b\r\nfile:///c\r\n"),
            Some((Operation::Copy, files.clone()))
        );
        // Only gnome-copied-files can say cut.
        assert_eq!(
            read_payload(Target::Utf8String, b"cut\n/a b\n/c"),
            Some((Operation::Copy, files.clone()))
        );
        assert_eq!(read_payload(Target::Utf8String, b"hello"), None);
        assert_eq!(read_payload(Target::UriList, b"\xff\xfe"), None);
        assert_eq!(read_payload(Target::GnomeCopiedFiles, b"cut\n"), None);
        // Gnome first: the only type that tells a cut from a copy.
        assert_eq!(Target::READ_ORDER[0], Target::GnomeCopiedFiles);
    }

    #[test]
    fn served_content_reads_back_the_same() {
        for op in [Operation::Copy, Operation::Cut] {
            let h = held(op);
            for t in Target::READ_ORDER {
                let want = if t == Target::GnomeCopiedFiles {
                    op
                } else {
                    Operation::Copy
                };
                assert_eq!(
                    read_payload(t, &payload(t, &h).unwrap()),
                    Some((want, h.paths.clone())),
                    "{t:?}"
                );
            }
        }
    }
}
