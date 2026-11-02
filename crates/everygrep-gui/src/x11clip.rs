//! Files on the clipboard for other programs, through the X11 `CLIPBOARD`
//! selection of the XWayland display.
//!
//! Slint puts only text on the clipboard. GNOME has no Wayland protocol
//! that lets a program offer other types without a focused surface of its
//! own, but it passes the X11 clipboard of XWayland on to Wayland programs
//! with every type it offers. So Cut and Copy make a hidden X11 window own
//! `CLIPBOARD` and offer the files as `x-special/gnome-copied-files`,
//! `text/uri-list` with `application/x-kde-cutselection` (how KDE programs
//! tell a cut), and text (the full paths). Paste asks another owner for
//! its `TARGETS`, then for the first offered of those file types; files
//! from a URI list are a cut when the owner also offers
//! `application/x-kde-cutselection` as `1`, as Dolphin does.
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
/// [`MAX_READ`] in the 32-bit units a `GetProperty` length is given in, so
/// one reply never holds more.
const READ_UNITS: u32 = (MAX_READ / 4) as u32;
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

    /// Types asked of another program for files, most telling first: the
    /// first says itself whether the files were cut; for the URI list
    /// `KdeCutSelection` is asked as well when offered.
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
/// lists are read as a copy; only `x-special/gnome-copied-files` says `cut`
/// itself (a URI list's cut mark is a type of its own, see [`cut_mark`]).
pub fn read_payload(target: Target, bytes: &[u8]) -> Option<(Operation, Vec<PathBuf>)> {
    let text = std::str::from_utf8(bytes).ok()?;
    let (op, paths) = clipboard::parse(text)?;
    match target {
        Target::GnomeCopiedFiles => Some((op, paths)),
        _ => Some((Operation::Copy, paths)),
    }
}

/// What `application/x-kde-cutselection` content says: `1` is a cut,
/// anything else a copy.
pub fn cut_mark(bytes: &[u8]) -> Operation {
    if bytes.first() == Some(&b'1') {
        Operation::Cut
    } else {
        Operation::Copy
    }
}

/// The file types to ask for, in [`Target::READ_ORDER`]: those `offered`,
/// or all of them when the owner gave no `TARGETS` list.
fn types_to_ask(offered: Option<&[Target]>) -> VecDeque<Target> {
    Target::READ_ORDER
        .into_iter()
        .filter(|t| offered.is_none_or(|o| o.contains(t)))
        .collect()
}

/// Whether files read as `from` need the cut mark asked for: only a URI
/// list, and only when the owner listed the mark (no round trip without).
fn asks_cut_mark(from: Target, offered: Option<&[Target]>) -> bool {
    from == Target::UriList && offered.is_some_and(|o| o.contains(&Target::KdeCutSelection))
}

/// Atoms in a `TARGETS` answer (32-bit values in the connection's byte
/// order, which x11rb makes native).
fn atoms_in(bytes: &[u8]) -> impl Iterator<Item = Atom> + '_ {
    bytes
        .chunks_exact(4)
        .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
}

/// Requests to the clipboard thread.
enum Cmd {
    Offer(Held, Sender<bool>),
    /// Gives the clipboard up if this program holds it.
    Clear,
    /// Reads the clipboard, then takes it with nothing on it if it holds
    /// these files and this program does not hold it.
    EmptyIf(Found),
    Read(Sender<Option<Found>>),
    Quit,
}

/// Files read from the clipboard, and what pasting them does.
type Found = (Operation, Vec<PathBuf>);

/// What ends a read.
enum Then {
    /// The files go to the caller of [`X11Clipboard::read`].
    Reply(Sender<Option<Found>>),
    /// The clipboard is emptied if it held these files.
    EmptyIf(Found),
}

/// Whether content read so far (`have` bytes) plus a reply of `value`
/// bytes, with `bytes_after` more bytes left in the property, stays within
/// [`MAX_READ`].
fn fits(have: usize, value: usize, bytes_after: u32) -> bool {
    bytes_after == 0 && have.checked_add(value).is_some_and(|n| n <= MAX_READ)
}

/// Waits on a helper thread at most `wait` for `answer`, then calls `done`
/// with it (`otherwise` when it did not come).
fn answer_then<T: Send + 'static>(
    answer: Receiver<T>,
    wait: Duration,
    otherwise: T,
    done: impl FnOnce(T) + Send + 'static,
) {
    std::thread::spawn(move || done(answer.recv_timeout(wait).unwrap_or(otherwise)));
}

/// Whether a read that ended with `found` empties the clipboard for
/// `then`.
fn empties(then: &Then, found: Option<&Found>) -> bool {
    matches!(then, Then::EmptyIf(want) if Some(want) == found)
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

    /// Takes the clipboard with `held` without waiting: `done` learns on a
    /// helper thread whether it was taken (`false` when not within
    /// [`OFFER_WAIT`]; the caller then puts the text on the clipboard).
    pub fn offer_then(&self, held: Held, done: impl FnOnce(bool) + Send + 'static) {
        let (reply, answer) = mpsc::channel();
        if !self.send(Cmd::Offer(held, reply)) {
            done(false);
            return;
        }
        answer_then(answer, OFFER_WAIT, false, done);
    }

    /// Gives the clipboard up if this program still holds it.
    pub fn clear(&self) {
        self.send(Cmd::Clear);
    }

    /// Takes the clipboard from its owner and offers nothing, so files
    /// moved away are not pasted again; only while it still holds `found`
    /// from another program. Does not wait: the clipboard thread reads it
    /// first, and a [`read`](Self::read) asked meanwhile replaces that
    /// check.
    pub fn empty_if_holding(&self, found: Found) {
        self.send(Cmd::EmptyIf(found));
    }

    /// Whether this program holds the clipboard.
    pub fn owns(&self) -> bool {
        self.owns.load(Ordering::SeqCst)
    }

    /// Asks for the files another program put on the clipboard without
    /// waiting: `done` gets them (or `None`) on a helper thread, at the
    /// latest after [`READ_WAIT`].
    pub fn read_then(&self, done: impl FnOnce(Option<Found>) + Send + 'static) {
        let (reply, answer) = mpsc::channel();
        if !self.send(Cmd::Read(reply)) {
            done(None);
            return;
        }
        answer_then(answer, READ_WAIT, None, done);
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
    then: Then,
    /// The owner's `TARGETS` that this program reads; `None` when it gave
    /// no list.
    offered: Option<Vec<Target>>,
    /// Types still to ask for.
    left: VecDeque<Target>,
    asked: Target,
    /// Files from the URI list while their cut mark is asked for.
    files: Option<Vec<PathBuf>>,
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
                Event::SelectionNotify(e)
                    if e.requestor == self.window
                        && self
                            .read
                            .as_ref()
                            .is_some_and(|r| self.atoms.atom(r.asked) == e.target) =>
                {
                    if e.property == NONE {
                        self.refused()?;
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
            Cmd::EmptyIf(found) => {
                if !self.owns.load(Ordering::SeqCst) {
                    self.start_read(Then::EmptyIf(found))?;
                }
            }
            Cmd::Read(reply) => self.start_read(Then::Reply(reply))?,
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
            // Emptied: the list of types holds only itself.
            (None, Some(Target::Targets)) if e.selection == self.atoms.clipboard => {
                self.conn.change_property32(
                    PropMode::REPLACE,
                    e.requestor,
                    property,
                    AtomEnum::ATOM,
                    &[self.atoms.atom(Target::Targets)],
                )?;
                true
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

    /// Starts a read of another program's clipboard; the list of types
    /// comes first. A newer read replaces an unanswered one, whose caller
    /// has stopped waiting.
    fn start_read(&mut self, then: Then) -> XResult<()> {
        self.read = Some(Read {
            then,
            offered: None,
            left: VecDeque::new(),
            asked: Target::Targets,
            files: None,
            incr: None,
        });
        self.ask(Target::Targets)
    }

    /// Takes the clipboard with nothing on it.
    fn empty(&mut self) -> XResult<()> {
        self.held = None;
        self.conn
            .set_selection_owner(self.window, self.atoms.clipboard, CURRENT_TIME)?;
        let owner = self
            .conn
            .get_selection_owner(self.atoms.clipboard)?
            .reply()?
            .owner;
        self.owns.store(owner == self.window, Ordering::SeqCst);
        Ok(())
    }

    /// Asks the clipboard's owner for `target`.
    fn ask(&mut self, target: Target) -> XResult<()> {
        let Some(read) = self.read.as_mut() else {
            return Ok(());
        };
        read.incr = None;
        read.asked = target;
        self.conn.convert_selection(
            self.window,
            self.atoms.clipboard,
            self.atoms.atom(target),
            self.atoms.paste,
            CURRENT_TIME,
        )?;
        self.conn.flush()?;
        Ok(())
    }

    /// Asks the clipboard's owner for the next file type, or ends the read.
    fn ask_next(&mut self) -> XResult<()> {
        let Some(read) = self.read.as_mut() else {
            return Ok(());
        };
        match read.left.pop_front() {
            Some(target) => self.ask(target),
            None => self.finish(None),
        }
    }

    /// The owner refused the type asked for.
    fn refused(&mut self) -> XResult<()> {
        let Some(read) = self.read.as_mut() else {
            return Ok(());
        };
        match read.asked {
            // No list: every file type is asked, without a cut mark.
            Target::Targets => {
                read.left = types_to_ask(None);
                self.ask_next()
            }
            Target::KdeCutSelection => {
                let files = read.files.take().unwrap_or_default();
                self.finish(Some((Operation::Copy, files)))
            }
            _ => self.ask_next(),
        }
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
                READ_UNITS,
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
        if !fits(0, reply.value.len(), reply.bytes_after) {
            return self.too_large();
        }
        self.parsed(&reply.value)
    }

    /// Ends a read whose content is over [`MAX_READ`]: too large to be a
    /// list of files worth pasting. The property is not deleted by a
    /// partial `GetProperty`, so it is deleted here.
    fn too_large(&mut self) -> XResult<()> {
        self.conn.delete_property(self.window, self.atoms.paste)?;
        self.conn.flush()?;
        self.finish(None)
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
                READ_UNITS,
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
        // Checked before the step is added, so the buffer never grows past
        // the limit.
        if !fits(buf.len(), reply.value.len(), reply.bytes_after) {
            return self.too_large();
        }
        buf.extend_from_slice(&reply.value);
        Ok(())
    }

    /// Goes on with the content of the type asked for: the list of types,
    /// the cut mark, or files. Ends the read with the files, or asks for
    /// the next type when there are none.
    fn parsed(&mut self, content: &[u8]) -> XResult<()> {
        let Some(read) = self.read.as_mut() else {
            return Ok(());
        };
        match read.asked {
            Target::Targets => {
                let offered: Vec<Target> = atoms_in(content)
                    .filter_map(|a| self.atoms.target(a))
                    .collect();
                read.left = types_to_ask(Some(&offered));
                read.offered = Some(offered);
                self.ask_next()
            }
            Target::KdeCutSelection => {
                let files = read.files.take().unwrap_or_default();
                self.finish(Some((cut_mark(content), files)))
            }
            asked => match read_payload(asked, content) {
                Some((_, paths)) if asks_cut_mark(asked, read.offered.as_deref()) => {
                    read.files = Some(paths);
                    self.ask(Target::KdeCutSelection)
                }
                Some(found) => self.finish(Some(found)),
                None => self.ask_next(),
            },
        }
    }

    /// Ends the read with `found`.
    fn finish(&mut self, found: Option<Found>) -> XResult<()> {
        let Some(read) = self.read.take() else {
            return Ok(());
        };
        if empties(&read.then, found.as_ref()) {
            return self.empty();
        }
        if let Then::Reply(reply) = read.then {
            let _ = reply.send(found);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(op: Operation) -> Held {
        let paths = vec![PathBuf::from("/a b/c.txt"), PathBuf::from("/d")];
        Held {
            op,
            text: crate::actions::paths_text(&paths, false),
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
        // Not asked for files: it only says whether the URI list was cut.
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

    /// What a paste reads from an owner offering `offers` (type and
    /// content), going through the types as the clipboard thread does.
    /// `offers` of `None` stands for an owner that refuses `TARGETS`.
    fn paste_from(offers: Option<&[(Target, &[u8])]>) -> Option<(Operation, Vec<PathBuf>)> {
        let offered: Option<Vec<Target>> = offers.map(|o| o.iter().map(|(t, _)| *t).collect());
        let content = |t: Target| {
            offers
                .unwrap_or_default()
                .iter()
                .find(|(o, _)| *o == t)
                .map(|(_, c)| *c)
        };
        for asked in types_to_ask(offered.as_deref()) {
            let Some((op, paths)) = content(asked).and_then(|c| read_payload(asked, c)) else {
                continue;
            };
            if asks_cut_mark(asked, offered.as_deref()) {
                let mark = content(Target::KdeCutSelection);
                return Some((mark.map_or(Operation::Copy, cut_mark), paths));
            }
            return Some((op, paths));
        }
        None
    }

    #[test]
    fn a_uri_list_is_cut_when_marked() {
        let files = vec![PathBuf::from("/a b"), PathBuf::from("/c")];
        let uris: &[u8] = b"file:///a%20b\r\nfile:///c\r\n";
        let kde = |mark: &'static [u8]| {
            vec![
                (Target::Targets, &b""[..]),
                (Target::UriList, uris),
                (Target::KdeCutSelection, mark),
                (Target::Utf8String, &b"/a b\n/c"[..]),
            ]
        };
        assert_eq!(
            paste_from(Some(&kde(b"1"))),
            Some((Operation::Cut, files.clone()))
        );
        for mark in [&b"0"[..], b"", b"true", b"01"] {
            assert_eq!(
                paste_from(Some(&kde(mark))),
                Some((Operation::Copy, files.clone())),
                "{mark:?}"
            );
        }
        // No mark offered: a copy, and the mark is not asked for.
        let unmarked = [(Target::UriList, uris)];
        assert!(!asks_cut_mark(Target::UriList, Some(&[Target::UriList])));
        assert_eq!(
            paste_from(Some(&unmarked)),
            Some((Operation::Copy, files.clone()))
        );
        // No TARGETS list: every type is tried, the mark never.
        assert_eq!(types_to_ask(None), Target::READ_ORDER);
        assert!(!asks_cut_mark(Target::UriList, None));
        // Only the URI list takes the mark.
        let text_marked = [
            (Target::Utf8String, &b"/a b\n/c"[..]),
            (Target::KdeCutSelection, b"1"),
        ];
        assert_eq!(
            paste_from(Some(&text_marked)),
            Some((Operation::Copy, files.clone()))
        );
        assert_eq!(cut_mark(b"1"), Operation::Cut);
        assert_eq!(cut_mark(b"1\n"), Operation::Cut);
        assert_eq!(cut_mark(b"0"), Operation::Copy);
        assert_eq!(cut_mark(b""), Operation::Copy);
    }

    #[test]
    fn gnome_copied_files_wins_over_the_cut_mark() {
        let files = vec![PathBuf::from("/a")];
        // Gnome says copy, the KDE mark says cut: gnome decides.
        let both = [
            (Target::UriList, &b"file:///a\r\n"[..]),
            (Target::KdeCutSelection, b"1"),
            (Target::GnomeCopiedFiles, b"copy\nfile:///a"),
        ];
        assert_eq!(
            paste_from(Some(&both)),
            Some((Operation::Copy, files.clone()))
        );
        assert!(!asks_cut_mark(
            Target::GnomeCopiedFiles,
            Some(&[Target::KdeCutSelection])
        ));
        let cut = [
            (Target::GnomeCopiedFiles, &b"cut\nfile:///a"[..]),
            (Target::UriList, b"file:///a\r\n"),
            (Target::KdeCutSelection, b"0"),
        ];
        assert_eq!(
            paste_from(Some(&cut)),
            Some((Operation::Cut, files.clone()))
        );
        // Unreadable gnome content falls back to the URI list and its mark.
        let broken = [
            (Target::GnomeCopiedFiles, &b"cut\n"[..]),
            (Target::UriList, b"file:///a\r\n"),
            (Target::KdeCutSelection, b"1"),
        ];
        assert_eq!(paste_from(Some(&broken)), Some((Operation::Cut, files)));
        // Only offered types are asked for.
        assert_eq!(
            types_to_ask(Some(&[Target::Utf8String, Target::UriList])),
            [Target::UriList, Target::Utf8String]
        );
        assert!(types_to_ask(Some(&[])).is_empty());
        assert_eq!(paste_from(Some(&[])), None);
    }

    #[test]
    fn a_moved_cut_empties_the_clipboard_only_while_it_holds_it() {
        let moved: Found = (Operation::Cut, vec![PathBuf::from("/a")]);
        let then = Then::EmptyIf(moved.clone());
        assert!(empties(&then, Some(&moved)));
        // Something else was put on the clipboard since, or nothing.
        assert!(!empties(&then, Some(&(Operation::Copy, moved.1.clone()))));
        assert!(!empties(
            &then,
            Some(&(Operation::Cut, vec![PathBuf::from("/b")]))
        ));
        assert!(!empties(&then, None));
        // A read for Paste never empties.
        let (reply, _answer) = mpsc::channel();
        assert!(!empties(&Then::Reply(reply), Some(&moved)));
    }

    #[test]
    fn a_read_answers_later_without_blocking_the_caller() {
        let (done_tx, done) = mpsc::channel();
        // No answer: the caller goes on at once, `done` gets None later.
        let (_reply, answer) = mpsc::channel::<Option<u32>>();
        let start = std::time::Instant::now();
        let tx = done_tx.clone();
        answer_then(answer, Duration::from_millis(300), None, move |got| {
            tx.send(got).unwrap();
        });
        assert!(start.elapsed() < Duration::from_millis(100));
        assert_eq!(done.recv_timeout(Duration::from_secs(5)).unwrap(), None);
        assert!(start.elapsed() >= Duration::from_millis(300));
        // An answer is passed on.
        let (reply, answer) = mpsc::channel();
        answer_then(answer, Duration::from_secs(5), None, move |got| {
            done_tx.send(got).unwrap();
        });
        reply.send(Some(7)).unwrap();
        assert_eq!(done.recv_timeout(Duration::from_secs(5)).unwrap(), Some(7));
    }

    #[test]
    fn reads_stay_within_the_limit() {
        // One reply never asks for more than the limit.
        assert_eq!(READ_UNITS as usize * 4, MAX_READ);
        assert!(fits(0, MAX_READ, 0));
        assert!(!fits(0, MAX_READ, 1), "more left in the property");
        assert!(fits(MAX_READ - 10, 10, 0));
        assert!(!fits(MAX_READ - 10, 11, 0), "a step past the limit");
        assert!(!fits(usize::MAX, 1, 0));
    }

    #[test]
    fn targets_answer_is_read_as_atoms() {
        let atoms: Vec<Atom> = vec![4, 0x1_0203, u32::MAX];
        let bytes: Vec<u8> = atoms.iter().flat_map(|a| a.to_ne_bytes()).collect();
        assert_eq!(atoms_in(&bytes).collect::<Vec<_>>(), atoms);
        // A cut-off last value is left out.
        assert_eq!(atoms_in(&bytes[..10]).count(), 2);
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
