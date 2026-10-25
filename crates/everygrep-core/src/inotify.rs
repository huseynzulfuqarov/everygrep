//! A small safe wrapper over the Linux inotify API.
//!
//! The bindings are written by hand against glibc; constant values come from
//! `<sys/inotify.h>`, `<bits/inotify.h>`, `<bits/poll.h>` and
//! `<linux/limits.h>`.

use std::ffi::{CString, OsString};
use std::io;
use std::ops::{BitOr, BitOrAssign};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::raw::{c_char, c_int, c_short, c_ulong, c_void};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;
use std::time::{Duration, Instant};

/// Size of the fixed part of a kernel `struct inotify_event` in bytes.
pub const EVENT_HEADER_SIZE: usize = 16;

/// Longest file name the kernel can report (`NAME_MAX`, `<linux/limits.h>`).
pub const NAME_MAX: usize = 255;

/// Smallest read buffer that is guaranteed to hold one event.
pub const MIN_BUFFER_SIZE: usize = EVENT_HEADER_SIZE + NAME_MAX + 1;

/// Read buffer size used when the caller's buffer is smaller.
pub const DEFAULT_BUFFER_SIZE: usize = 64 * 1024;

/// `IN_NONBLOCK` (`<bits/inotify.h>`, octal 00004000).
const IN_NONBLOCK: c_int = 0o4000;
/// `IN_CLOEXEC` (`<bits/inotify.h>`, octal 02000000).
const IN_CLOEXEC: c_int = 0o2000000;
/// `POLLIN` (`<bits/poll.h>`).
const POLLIN: c_short = 0x001;

/// Mirror of glibc `struct pollfd` (`<sys/poll.h>`).
#[repr(C)]
struct PollFd {
    fd: c_int,
    events: c_short,
    revents: c_short,
}

unsafe extern "C" {
    fn inotify_init1(flags: c_int) -> c_int;
    fn inotify_add_watch(fd: c_int, pathname: *const c_char, mask: u32) -> c_int;
    fn inotify_rm_watch(fd: c_int, wd: c_int) -> c_int;
    fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
    fn poll(fds: *mut PollFd, nfds: c_ulong, timeout: c_int) -> c_int;
}

/// A set of inotify event bits and watch flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Mask(pub u32);

impl Mask {
    /// File was modified (`IN_MODIFY`).
    pub const MODIFY: Mask = Mask(0x0000_0002);
    /// Metadata changed (`IN_ATTRIB`).
    pub const ATTRIB: Mask = Mask(0x0000_0004);
    /// File opened for writing was closed (`IN_CLOSE_WRITE`).
    pub const CLOSE_WRITE: Mask = Mask(0x0000_0008);
    /// Entry was moved out of the watched folder (`IN_MOVED_FROM`).
    pub const MOVED_FROM: Mask = Mask(0x0000_0040);
    /// Entry was moved into the watched folder (`IN_MOVED_TO`).
    pub const MOVED_TO: Mask = Mask(0x0000_0080);
    /// Entry was created in the watched folder (`IN_CREATE`).
    pub const CREATE: Mask = Mask(0x0000_0100);
    /// Entry was deleted from the watched folder (`IN_DELETE`).
    pub const DELETE: Mask = Mask(0x0000_0200);
    /// The watched object itself was deleted (`IN_DELETE_SELF`).
    pub const DELETE_SELF: Mask = Mask(0x0000_0400);
    /// The watched object itself was moved (`IN_MOVE_SELF`).
    pub const MOVE_SELF: Mask = Mask(0x0000_0800);
    /// The backing file system was unmounted (`IN_UNMOUNT`, output only).
    pub const UNMOUNT: Mask = Mask(0x0000_2000);
    /// The kernel event queue overflowed; events were lost (`IN_Q_OVERFLOW`, output only).
    pub const Q_OVERFLOW: Mask = Mask(0x0000_4000);
    /// The watch was removed (`IN_IGNORED`, output only).
    pub const IGNORED: Mask = Mask(0x0000_8000);
    /// Only watch the path if it is a folder (`IN_ONLYDIR`, watch flag).
    pub const ONLYDIR: Mask = Mask(0x0100_0000);
    /// Do not follow a final symbolic link (`IN_DONT_FOLLOW`, watch flag).
    pub const DONT_FOLLOW: Mask = Mask(0x0200_0000);
    /// No events for children after they are unlinked (`IN_EXCL_UNLINK`, watch flag).
    pub const EXCL_UNLINK: Mask = Mask(0x0400_0000);
    /// The subject of the event is a folder (`IN_ISDIR`, output only).
    pub const ISDIR: Mask = Mask(0x4000_0000);

    /// Events and watch flags a file name index needs on each folder.
    pub const INDEX_EVENTS: Mask = Mask(
        Self::CREATE.0
            | Self::DELETE.0
            | Self::MOVED_FROM.0
            | Self::MOVED_TO.0
            | Self::ATTRIB.0
            | Self::CLOSE_WRITE.0
            | Self::DELETE_SELF.0
            | Self::MOVE_SELF.0
            | Self::ONLYDIR.0
            | Self::DONT_FOLLOW.0
            | Self::EXCL_UNLINK.0,
    );

    /// Raw bit value.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// True when every bit of `other` is set in `self`.
    pub const fn contains(self, other: Mask) -> bool {
        self.0 & other.0 == other.0
    }

    /// True when at least one bit of `other` is set in `self`.
    pub const fn intersects(self, other: Mask) -> bool {
        self.0 & other.0 != 0
    }
}

impl BitOr for Mask {
    type Output = Mask;

    fn bitor(self, rhs: Mask) -> Mask {
        Mask(self.0 | rhs.0)
    }
}

impl BitOrAssign for Mask {
    fn bitor_assign(&mut self, rhs: Mask) {
        self.0 |= rhs.0;
    }
}

/// Identifier of one watch, as returned by [`Inotify::add_watch`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WatchId(pub i32);

/// One decoded inotify event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    /// Watch the event belongs to (`-1` for [`Mask::Q_OVERFLOW`]).
    pub wd: WatchId,
    /// Event bits.
    pub mask: Mask,
    /// Links the [`Mask::MOVED_FROM`] and [`Mask::MOVED_TO`] halves of a rename.
    pub cookie: u32,
    /// Name of the entry inside the watched folder, without NUL padding.
    /// `None` when the event is about the watched object itself.
    pub name: Option<OsString>,
}

/// An inotify instance. The descriptor is non-blocking and close-on-exec,
/// and is closed when the value is dropped.
#[derive(Debug)]
pub struct Inotify {
    fd: OwnedFd,
}

impl Inotify {
    /// Creates a new inotify instance.
    pub fn new() -> io::Result<Self> {
        // SAFETY: inotify_init1 takes only an integer and touches no memory of ours.
        let fd = unsafe { inotify_init1(IN_NONBLOCK | IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a fresh, valid descriptor that nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(Self { fd })
    }

    /// Starts watching `path` for the events in `mask`.
    ///
    /// Adding a path that is already watched returns the same [`WatchId`] and
    /// replaces its mask.
    pub fn add_watch(&self, path: &Path, mask: Mask) -> io::Result<WatchId> {
        let c_path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
        // SAFETY: the descriptor is valid for the lifetime of `self`, and
        // `c_path` is a NUL-terminated string that outlives the call.
        let wd = unsafe { inotify_add_watch(self.fd.as_raw_fd(), c_path.as_ptr(), mask.0) };
        if wd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(WatchId(wd))
    }

    /// Stops a watch. The kernel then queues one [`Mask::IGNORED`] event for it.
    pub fn remove_watch(&self, wd: WatchId) -> io::Result<()> {
        // SAFETY: both arguments are plain integers; the descriptor is valid.
        let rc = unsafe { inotify_rm_watch(self.fd.as_raw_fd(), wd.0) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Waits up to `timeout` (`None` = no limit) for events, then reads and
    /// decodes all events that are queued.
    ///
    /// `buf` is scratch space reused between calls. It is grown to
    /// [`DEFAULT_BUFFER_SIZE`] when shorter than [`MIN_BUFFER_SIZE`] (one
    /// header plus `NAME_MAX + 1` name bytes). Returns an empty vector when
    /// the timeout passes without events.
    pub fn read_events(
        &self,
        buf: &mut Vec<u8>,
        timeout: Option<Duration>,
    ) -> io::Result<Vec<Event>> {
        if buf.len() < MIN_BUFFER_SIZE {
            buf.resize(DEFAULT_BUFFER_SIZE, 0);
        }
        if !self.wait_readable(timeout)? {
            return Ok(Vec::new());
        }
        let mut events = Vec::new();
        loop {
            match self.read_once(buf) {
                Ok(0) => break,
                Ok(n) => events.extend(parse_events(&buf[..n])),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(events)
    }

    /// Polls the descriptor; returns `true` when it is readable. Restarts
    /// after a signal with the remaining time.
    fn wait_readable(&self, timeout: Option<Duration>) -> io::Result<bool> {
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            let ms = match deadline {
                None => -1,
                Some(d) => duration_to_poll_ms(d.saturating_duration_since(Instant::now())),
            };
            let mut pfd = PollFd {
                fd: self.fd.as_raw_fd(),
                events: POLLIN,
                revents: 0,
            };
            // SAFETY: `pfd` is a valid, writable pollfd and nfds is 1.
            let rc = unsafe { poll(&mut pfd, 1, ms) };
            if rc < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            return Ok(rc > 0);
        }
    }

    /// One `read` call into `buf`; returns the number of bytes read.
    fn read_once(&self, buf: &mut [u8]) -> io::Result<usize> {
        // SAFETY: `buf` is valid for writes of `buf.len()` bytes and the
        // descriptor is valid for the lifetime of `self`.
        let n = unsafe { read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(n as usize)
    }
}

impl AsFd for Inotify {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl AsRawFd for Inotify {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// Converts a wait time to whole milliseconds for `poll`, rounding up so a
/// short positive timeout never becomes a busy zero-timeout poll.
fn duration_to_poll_ms(d: Duration) -> c_int {
    let ms = d.as_nanos().div_ceil(1_000_000);
    ms.min(c_int::MAX as u128) as c_int
}

/// Decodes a buffer filled by `read` on an inotify descriptor.
///
/// Each record is a 16-byte header (`wd: i32`, `mask: u32`, `cookie: u32`,
/// `len: u32`, native byte order) followed by `len` name bytes padded with
/// NULs. Decoding stops at a truncated record instead of panicking.
pub fn parse_events(buf: &[u8]) -> Vec<Event> {
    let mut events = Vec::new();
    let mut rest = buf;
    while rest.len() >= EVENT_HEADER_SIZE {
        let field = |i: usize| -> [u8; 4] { rest[i * 4..i * 4 + 4].try_into().unwrap() };
        let wd = i32::from_ne_bytes(field(0));
        let mask = u32::from_ne_bytes(field(1));
        let cookie = u32::from_ne_bytes(field(2));
        let len = u32::from_ne_bytes(field(3)) as usize;
        let Some(name_bytes) = rest.get(EVENT_HEADER_SIZE..EVENT_HEADER_SIZE + len) else {
            break;
        };
        events.push(Event {
            wd: WatchId(wd),
            mask: Mask(mask),
            cookie,
            name: decode_name(name_bytes),
        });
        rest = &rest[EVENT_HEADER_SIZE + len..];
    }
    events
}

/// Strips NUL padding from a name field; `None` when nothing is left.
fn decode_name(bytes: &[u8]) -> Option<OsString> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    if end == 0 {
        return None;
    }
    Some(OsString::from_vec(bytes[..end].to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::fs;
    use std::path::PathBuf;

    fn record(wd: i32, mask: u32, cookie: u32, name: &[u8], len: u32) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&wd.to_ne_bytes());
        out.extend_from_slice(&mask.to_ne_bytes());
        out.extend_from_slice(&cookie.to_ne_bytes());
        out.extend_from_slice(&len.to_ne_bytes());
        let mut field = name.to_vec();
        field.resize(len as usize, 0);
        out.extend_from_slice(&field);
        out
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("everygrep-inotify-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Reads until `done` returns true for the collected events or 5 s pass.
    fn collect_until(ino: &Inotify, done: impl Fn(&[Event]) -> bool) -> Vec<Event> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut buf = Vec::new();
        let mut all = Vec::new();
        while !done(&all) && Instant::now() < deadline {
            let batch = ino
                .read_events(&mut buf, Some(Duration::from_millis(200)))
                .unwrap();
            all.extend(batch);
        }
        all
    }

    fn named<'a>(events: &'a [Event], name: &str, mask: Mask) -> Option<&'a Event> {
        events
            .iter()
            .find(|e| e.name.as_deref() == Some(OsStr::new(name)) && e.mask.contains(mask))
    }

    #[test]
    fn parses_several_events_and_strips_padding() {
        let mut buf = record(1, Mask::CREATE.0, 0, b"a.txt", 16);
        buf.extend(record(1, Mask::MOVED_FROM.0, 7, b"old", 16));
        buf.extend(record(2, (Mask::CREATE | Mask::ISDIR).0, 0, b"dir", 4));
        let events = parse_events(&buf);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].wd, WatchId(1));
        assert_eq!(events[0].name.as_deref(), Some(OsStr::new("a.txt")));
        assert_eq!(events[1].cookie, 7);
        assert_eq!(events[1].name.as_deref(), Some(OsStr::new("old")));
        assert!(events[2].mask.contains(Mask::CREATE | Mask::ISDIR));
        assert_eq!(events[2].name.as_deref(), Some(OsStr::new("dir")));
    }

    #[test]
    fn zero_length_name_is_none() {
        let buf = record(3, (Mask::DELETE_SELF).0, 0, b"", 0);
        let events = parse_events(&buf);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, None);
        assert_eq!(events[0].mask, Mask::DELETE_SELF);
    }

    #[test]
    fn truncated_tail_is_dropped() {
        let mut buf = record(1, Mask::CREATE.0, 0, b"keep", 16);
        let second = record(1, Mask::DELETE.0, 0, b"lost", 16);
        // Header complete but name cut short.
        buf.extend_from_slice(&second[..20]);
        assert_eq!(parse_events(&buf).len(), 1);
        // Header itself cut short.
        let cut = record(1, Mask::CREATE.0, 0, b"keep", 16).len() + 10;
        assert_eq!(parse_events(&buf[..cut]).len(), 1);
        assert!(parse_events(&[0u8; 5]).is_empty());
        assert!(parse_events(&[]).is_empty());
    }

    #[test]
    fn index_mask_and_bit_ops() {
        assert!(Mask::INDEX_EVENTS.contains(Mask::CREATE | Mask::MOVED_TO | Mask::ONLYDIR));
        assert!(!Mask::INDEX_EVENTS.contains(Mask::MODIFY));
        let mut m = Mask::CREATE;
        m |= Mask::ISDIR;
        assert!(m.intersects(Mask::ISDIR));
        assert_eq!(m.bits(), 0x4000_0100);
        assert_eq!(duration_to_poll_ms(Duration::from_micros(1)), 1);
        assert_eq!(duration_to_poll_ms(Duration::ZERO), 0);
        assert_eq!(duration_to_poll_ms(Duration::MAX), c_int::MAX);
    }

    #[test]
    fn reports_changes_in_watched_folder() {
        let dir = temp_dir("events");
        let ino = Inotify::new().unwrap();
        let wd = ino.add_watch(&dir, Mask::INDEX_EVENTS).unwrap();

        fs::write(dir.join("file.txt"), b"x").unwrap();
        fs::create_dir(dir.join("sub")).unwrap();
        fs::rename(dir.join("file.txt"), dir.join("renamed.txt")).unwrap();
        fs::remove_file(dir.join("renamed.txt")).unwrap();
        fs::remove_dir(dir.join("sub")).unwrap();
        fs::remove_dir(&dir).unwrap();

        let events = collect_until(&ino, |all| {
            all.iter().any(|e| e.mask.contains(Mask::IGNORED))
        });
        let _ = fs::remove_dir_all(&dir);

        assert!(events.iter().all(|e| e.wd == wd));
        let created = named(&events, "file.txt", Mask::CREATE).expect("CREATE file");
        assert!(!created.mask.contains(Mask::ISDIR));
        assert!(named(&events, "sub", Mask::CREATE | Mask::ISDIR).is_some());
        let from = named(&events, "file.txt", Mask::MOVED_FROM).expect("MOVED_FROM");
        let to = named(&events, "renamed.txt", Mask::MOVED_TO).expect("MOVED_TO");
        assert_ne!(from.cookie, 0);
        assert_eq!(from.cookie, to.cookie);
        assert!(named(&events, "renamed.txt", Mask::DELETE).is_some());
        assert!(named(&events, "sub", Mask::DELETE | Mask::ISDIR).is_some());
        assert!(
            events
                .iter()
                .any(|e| e.name.is_none() && e.mask.contains(Mask::DELETE_SELF))
        );
        assert!(
            events
                .iter()
                .any(|e| e.name.is_none() && e.mask.contains(Mask::IGNORED))
        );
    }

    #[test]
    fn remove_watch_queues_ignored() {
        let dir = temp_dir("remove");
        let ino = Inotify::new().unwrap();
        let wd = ino.add_watch(&dir, Mask::INDEX_EVENTS).unwrap();
        ino.remove_watch(wd).unwrap();
        let events = collect_until(&ino, |all| !all.is_empty());
        fs::remove_dir_all(&dir).unwrap();
        assert!(
            events
                .iter()
                .any(|e| e.wd == wd && e.mask.contains(Mask::IGNORED))
        );
    }

    #[test]
    fn timeout_without_events_returns_empty() {
        let ino = Inotify::new().unwrap();
        let mut buf = Vec::new();
        let events = ino
            .read_events(&mut buf, Some(Duration::from_millis(10)))
            .unwrap();
        assert!(events.is_empty());
        assert!(ino.as_raw_fd() >= 0);
    }

    #[test]
    fn watching_missing_path_fails() {
        let ino = Inotify::new().unwrap();
        let missing =
            std::env::temp_dir().join(format!("everygrep-inotify-missing-{}", std::process::id()));
        let _ = fs::remove_dir_all(&missing);
        let err = ino.add_watch(&missing, Mask::INDEX_EVENTS).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
