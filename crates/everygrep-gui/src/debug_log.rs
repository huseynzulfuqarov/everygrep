//! The program's own log: short notes about what went wrong or what it did,
//! for finding the cause of a problem.
//!
//! Every note goes to the terminal (standard error) as before and into a
//! ring of the last [`CAPACITY`] lines in memory, which Help → Debug log
//! shows. Started with `--debug-log`, the program also appends the lines to
//! `$XDG_CACHE_HOME/everygrep/debug.log` (see [`file_path`]); without it
//! nothing is written to disk. A file over [`ROTATE_AT`] at the start is
//! renamed to `debug.log.old`; during a run it is truncated at that bound.
//!
//! Lines keep file paths: the log stays on this computer.

use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Lines kept in memory.
pub const CAPACITY: usize = 1000;

/// Maximum bytes in the running log; an older oversized file becomes `.old`.
pub const ROTATE_AT: u64 = 4 << 20;

/// The log of the running program.
static LOG: Mutex<Log> = Mutex::new(Log::new());

/// Notes the program as a line of the log; `format!` arguments.
macro_rules! note {
    ($($arg:tt)*) => {
        $crate::debug_log::write(&format!($($arg)*))
    };
}
pub(crate) use note;

/// The last lines in memory and, with `--debug-log`, the file they also go
/// to.
pub struct Log {
    lines: VecDeque<String>,
    capacity: usize,
    file: Option<File>,
}

impl Log {
    pub const fn new() -> Self {
        Self::with_capacity(CAPACITY)
    }

    pub const fn with_capacity(capacity: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            capacity,
            file: None,
        }
    }

    /// Adds a line, dropping the oldest when the ring is full. A message of
    /// several lines is kept as one entry with its line breaks.
    pub fn push(&mut self, line: String) {
        if let Some(file) = self.file.as_mut() {
            // A full disk only loses lines of the file, never the window.
            let result = (|| -> std::io::Result<()> {
                let mut end = line.len().min(ROTATE_AT as usize - 1);
                while !line.is_char_boundary(end) {
                    end -= 1;
                }
                if file.metadata()?.len().saturating_add(end as u64 + 1) > ROTATE_AT {
                    file.set_len(0)?;
                }
                writeln!(file, "{}", &line[..end])
            })();
            if result.is_err() {
                self.file = None;
            }
        }
        if self.capacity == 0 {
            return;
        }
        if self.lines.len() == self.capacity {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    /// The lines in memory, oldest first, each ended by a line break.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            out.push_str(line);
            out.push('\n');
        }
        out
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.lines.len()
    }
}

/// `[HH:MM:SS.mmm] message` in local time.
pub fn stamped(at: SystemTime, zone: &everygrep_core::localtime::Zone, message: &str) -> String {
    let since = at.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs() as i64;
    let local = zone.to_local(secs).rem_euclid(86_400);
    format!(
        "[{:02}:{:02}:{:02}.{:03}] {message}",
        local / 3600,
        local / 60 % 60,
        local % 60,
        since.subsec_millis()
    )
}

/// The local time zone, read once.
fn zone() -> &'static everygrep_core::localtime::Zone {
    static ZONE: std::sync::OnceLock<everygrep_core::localtime::Zone> = std::sync::OnceLock::new();
    ZONE.get_or_init(everygrep_core::localtime::Zone::local)
}

/// Writes `message` to standard error and to the log.
pub fn write(message: &str) {
    eprintln!("everygrep-gui: {message}");
    let line = stamped(SystemTime::now(), zone(), message);
    if let Ok(mut log) = LOG.lock() {
        log.push(line);
    }
}

/// The file the log is written to, with `--debug-log`.
static WRITING_TO: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Where the lines also go, when the program was started with
/// `--debug-log` and the file could be opened.
pub fn writing_to() -> Option<&'static Path> {
    WRITING_TO.get().map(PathBuf::as_path)
}

/// The lines in memory, oldest first.
pub fn text() -> String {
    LOG.lock().map(|log| log.text()).unwrap_or_default()
}

/// `$XDG_CACHE_HOME/everygrep/debug.log` (or `~/.cache/…`), next to the
/// default index file.
pub fn file_path() -> Option<PathBuf> {
    Some(everygrep_core::persist::default_path()?.with_file_name("debug.log"))
}

/// Opens `path` for appending, after renaming a file larger than
/// [`ROTATE_AT`] to `<path>.old`. The file is made readable by its owner
/// only, since lines hold file paths.
pub fn open_file(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    use std::os::unix::fs::PermissionsExt;
    // Linux O_NOFOLLOW | O_NONBLOCK: refuse symlinks and never wait on a FIFO.
    let open = || {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(0o400000 | 0o4000)
            .open(path)
    };
    let mut file = open()?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(std::io::Error::other("debug log is not a regular file"));
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    if meta.len() > ROTATE_AT {
        let mut old = path.as_os_str().to_owned();
        old.push(".old");
        std::fs::rename(path, old)?;
        file = open()?;
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::other("debug log is not a regular file"));
        }
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

/// `--debug-log`: from now on lines also go to [`file_path`]. Says where,
/// or why it cannot, in the log itself.
pub fn start_file() {
    let Some(path) = file_path() else {
        write("debug log: no cache folder (HOME is not set)");
        return;
    };
    match open_file(&path) {
        Ok(file) => {
            if let Ok(mut log) = LOG.lock() {
                log.file = Some(file);
            }
            let _ = WRITING_TO.set(path.clone());
            write(&format!(
                "debug log: everygrep-gui {} writing to {}",
                everygrep_core::VERSION,
                path.display()
            ));
        }
        Err(e) => write(&format!("debug log: cannot write {}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn the_ring_keeps_the_last_lines_in_order() {
        let mut log = Log::with_capacity(3);
        for i in 0..5 {
            log.push(format!("line {i}"));
        }
        assert_eq!(log.len(), 3);
        assert_eq!(log.text(), "line 2\nline 3\nline 4\n");
        assert_eq!(Log::new().capacity, 1000);
    }

    #[test]
    fn lines_go_to_the_file_too_when_one_is_open() {
        let dir = std::env::temp_dir().join(format!("eg-debuglog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("sub").join("debug.log");
        let mut log = Log::with_capacity(2);
        log.push("before".into());
        log.file = Some(open_file(&path).unwrap());
        log.push("a".into());
        log.push("b\nc".into());
        log.push("d".into());
        drop(log);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb\nc\nd\n");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // A large file is kept as .old and a new one begun.
        std::fs::write(&path, vec![b'x'; ROTATE_AT as usize + 1]).unwrap();
        let mut file = open_file(&path).unwrap();
        writeln!(file, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
        let old = dir.join("sub").join("debug.log.old");
        assert_eq!(std::fs::metadata(old).unwrap().len(), ROTATE_AT + 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn running_file_is_bounded_and_existing_permissions_are_private() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = std::env::temp_dir().join(format!("eg-debuglog-bound-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("debug.log");
        std::fs::write(&path, "old\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        let mut log = Log::with_capacity(1);
        log.file = Some(open_file(&path).unwrap());
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        for _ in 0..6 {
            log.push("x".repeat(ROTATE_AT as usize / 3));
            assert!(std::fs::metadata(&path).unwrap().len() <= ROTATE_AT);
        }
        log.push("ə".repeat(ROTATE_AT as usize));
        assert!(std::fs::metadata(&path).unwrap().len() <= ROTATE_AT);
        assert!(std::fs::read_to_string(&path).is_ok());
        let link = dir.join("link");
        symlink(&path, &link).unwrap();
        assert!(open_file(&link).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn lines_are_stamped_with_the_local_time_of_day() {
        let zone = everygrep_core::localtime::Zone::utc();
        let at = UNIX_EPOCH + Duration::from_millis(86_400_000 + 3_723_045);
        assert_eq!(stamped(at, &zone, "hi"), "[01:02:03.045] hi");
    }
}
