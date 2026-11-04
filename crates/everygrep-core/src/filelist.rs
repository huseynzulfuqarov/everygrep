//! File lists: the entries of one folder (normally a disk) saved to a text
//! file, so they can be searched while that folder is not there.
//!
//! The format is plain UTF-8 text with one entry per line (` → ` stands
//! for a tab here):
//!
//! ```text
//! EveryGrep file list 1
//! root /run/media/me/Backup
//! d →  → 1730000000000000000 →  → /run/media/me/Backup
//! f → 1024 → 1730000000123456789 → 1720000000000000000 → /run/media/me/Backup/a\tb.txt
//! ```
//!
//! - The first line names the format and its version. A reader refuses a
//!   newer version rather than guess at it.
//! - `root <path>` comes once, before the entries: the folder the list is
//!   of. Every entry is that folder or lies below it.
//! - An entry has five fields separated by tabs: the kind (`d` for a
//!   folder, `f` for anything else), the size in bytes (empty for folders,
//!   whose size is the total of the files below), the modification time and
//!   the creation time in nanoseconds since 1970-01-01 UTC (empty when not
//!   known), and the absolute path.
//! - Paths are kept byte for byte: a backslash, tab, line feed and carriage
//!   return are written `\\`, `\t`, `\n` and `\r`, and any other control
//!   byte, DEL, or byte that is not part of valid UTF-8 as `\xHH`.
//! - Empty lines and lines starting with `#` are skipped.
//! - Folders above an entry that have no line of their own are added as
//!   folders with unknown times. A path listed twice keeps its last line.

use std::collections::HashMap;
use std::fmt;
use std::io::{self, BufRead, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::index::Index;
use crate::progress::Progress;
use crate::store::{Meta, NodeId, NodeStore};

/// The first words of the first line.
pub const MAGIC: &str = "EveryGrep file list";

/// The version written, and the newest one read.
pub const VERSION: u32 = 1;

/// The file name extension of file lists.
pub const EXTENSION: &str = "eglist";

/// The longest line read: a path of many escaped bytes fits easily.
pub const MAX_LINE: usize = 1 << 20;

/// The largest file list read, so a wrong or damaged file cannot take all
/// memory.
pub const MAX_FILE: u64 = 64 << 20;

/// Bounds include implicit parent folders and both name pools.
pub const MAX_ENTRIES: usize = 250_000;
pub const MAX_NODES: usize = 500_000;
pub const MAX_DEPTH: usize = 256;
pub const MAX_PATH: usize = u16::MAX as usize;
const MAX_POOL: usize = 64 << 20;

/// How far [`read_root`] reads for the version and root lines.
const MAX_HEAD: u64 = 16 * MAX_LINE as u64;

/// One line of a file list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    /// The absolute path, raw bytes.
    pub path: Vec<u8>,
    pub is_dir: bool,
    /// Size in bytes; 0 for folders.
    pub size: u64,
    /// Nanoseconds since 1970, when known.
    pub modified_ns: Option<i64>,
    pub created_ns: Option<i64>,
}

/// A whole file list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileList {
    /// The folder the list is of, raw bytes.
    pub root: Vec<u8>,
    pub entries: Vec<Entry>,
}

/// Why a file list could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// The line, counted from 1; 0 for the file as a whole.
    pub line: usize,
    pub what: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            f.write_str(&self.what)
        } else {
            write!(f, "line {}: {}", self.line, self.what)
        }
    }
}

impl std::error::Error for Error {}

impl From<Error> for io::Error {
    fn from(e: Error) -> Self {
        let kind = if e.what == "cancelled" {
            io::ErrorKind::Interrupted
        } else {
            io::ErrorKind::InvalidData
        };
        io::Error::new(kind, e)
    }
}

fn error(line: usize, what: impl Into<String>) -> Error {
    Error {
        line,
        what: what.into(),
    }
}

/// Appends `bytes` to `out` with the escapes of the format.
pub fn escape_into(bytes: &[u8], out: &mut Vec<u8>) {
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '\\' => out.extend_from_slice(b"\\\\"),
                '\t' => out.extend_from_slice(b"\\t"),
                '\n' => out.extend_from_slice(b"\\n"),
                '\r' => out.extend_from_slice(b"\\r"),
                c if (c as u32) < 0x20 || c == '\u{7f}' => {
                    out.extend_from_slice(format!("\\x{:02X}", c as u32).as_bytes());
                }
                c => {
                    let mut buf = [0; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
            }
        }
        for &b in chunk.invalid() {
            out.extend_from_slice(format!("\\x{b:02X}").as_bytes());
        }
    }
}

/// The bytes `escape_into` wrote `text` for.
pub fn unescape(text: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let b = text[i];
        if b != b'\\' {
            out.push(b);
            i += 1;
            continue;
        }
        match text.get(i + 1) {
            Some(b'\\') => out.push(b'\\'),
            Some(b't') => out.push(b'\t'),
            Some(b'n') => out.push(b'\n'),
            Some(b'r') => out.push(b'\r'),
            Some(b'x') => {
                let hex = text
                    .get(i + 2..i + 4)
                    .and_then(|h| std::str::from_utf8(h).ok())
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                    .ok_or("\\x needs two hexadecimal digits")?;
                out.push(hex);
                i += 4;
                continue;
            }
            _ => return Err("unknown escape after a backslash".into()),
        }
        i += 2;
    }
    Ok(out)
}

/// Writes the first two lines of a list of the folder `root`.
pub fn write_header(out: &mut impl Write, root: &[u8]) -> io::Result<()> {
    let mut line = format!("{MAGIC} {VERSION}\nroot ").into_bytes();
    escape_into(root, &mut line);
    line.push(b'\n');
    out.write_all(&line)
}

/// Writes the line of one entry.
pub fn write_entry(out: &mut impl Write, entry: &Entry) -> io::Result<()> {
    let mut line = Vec::with_capacity(entry.path.len() + 48);
    line.push(if entry.is_dir { b'd' } else { b'f' });
    line.push(b'\t');
    if !entry.is_dir {
        line.extend_from_slice(entry.size.to_string().as_bytes());
    }
    for time in [entry.modified_ns, entry.created_ns] {
        line.push(b'\t');
        if let Some(t) = time {
            line.extend_from_slice(t.to_string().as_bytes());
        }
    }
    line.push(b'\t');
    escape_into(&entry.path, &mut line);
    line.push(b'\n');
    out.write_all(&line)
}

/// Writes a whole list.
pub fn write_list(out: &mut impl Write, list: &FileList) -> io::Result<()> {
    write_header(out, &list.root)?;
    for entry in &list.entries {
        write_entry(out, entry)?;
    }
    Ok(())
}

/// Writes the list of the folder `dir` of `index` and all entries below it,
/// folders before their contents. `created` gives the creation time of a
/// path (`None` when unknown). Returns the number of entries written.
pub fn write_index(
    out: &mut impl Write,
    index: &Index,
    dir: NodeId,
    created: &mut dyn FnMut(&Path) -> Option<i64>,
    progress: &Progress,
) -> io::Result<u64> {
    let store = index.store();
    let root = store.path(dir);
    write_header(out, root.as_os_str().as_bytes())?;
    let mut count = 0;
    let mut stack = vec![(dir, root)];
    while let Some((id, path)) = stack.pop() {
        if progress.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let meta = store.meta(id);
        let is_dir = store.is_dir(id);
        write_entry(
            out,
            &Entry {
                path: path.as_os_str().as_bytes().to_vec(),
                is_dir,
                size: if is_dir { 0 } else { meta.size },
                modified_ns: (meta.mtime_ns != 0).then_some(meta.mtime_ns),
                created_ns: created(&path),
            },
        )?;
        count += 1;
        progress.add(1);
        // Pushed in reverse, so children come out in the store's order.
        let children: Vec<NodeId> = store.children(id).collect();
        for child in children.into_iter().rev() {
            let name = std::ffi::OsStr::from_bytes(store.name(child));
            stack.push((child, path.join(name)));
        }
    }
    Ok(count)
}

/// Checks that `path` is absolute, not `/`, without empty, `.` or `..`
/// components, a trailing slash or a NUL byte.
fn check_path(path: &[u8]) -> Result<(), &'static str> {
    if path.len() > MAX_PATH {
        return Err("the path is too long");
    }
    if path.iter().filter(|&&b| b == b'/').count() > MAX_DEPTH {
        return Err("the path is too deep");
    }
    if path.first() != Some(&b'/') {
        return Err("the path is not absolute");
    }
    if path == b"/" {
        return Err("the path cannot be /");
    }
    if path.contains(&0) {
        return Err("the path holds a NUL byte");
    }
    if path[1..]
        .split(|&b| b == b'/')
        .any(|c| c.is_empty() || c == b"." || c == b"..")
    {
        return Err("the path has an empty, . or .. component");
    }
    for name in path[1..].split(|&b| b == b'/') {
        check_name(name)?;
    }
    Ok(())
}

fn check_name(name: &[u8]) -> Result<(), &'static str> {
    if name.len() > usize::from(u16::MAX) || crate::fold::fold(name).len() > usize::from(u16::MAX) {
        return Err("the raw or folded name is too long");
    }
    Ok(())
}

fn number<T: std::str::FromStr>(field: &[u8]) -> Result<Option<T>, ()> {
    if field.is_empty() {
        return Ok(None);
    }
    std::str::from_utf8(field)
        .ok()
        .and_then(|t| t.parse().ok())
        .map(Some)
        .ok_or(())
}

/// Reads the version line; `Ok` with the version when it can be read.
fn header(line: &[u8]) -> Result<u32, Error> {
    let text = std::str::from_utf8(line)
        .ok()
        .and_then(|l| l.strip_prefix(MAGIC))
        .and_then(|l| l.strip_prefix(' '))
        .ok_or_else(|| error(1, "this is not an EveryGrep file list"))?;
    let version: u32 = text
        .trim_end()
        .parse()
        .map_err(|_| error(1, "the version is not a number"))?;
    if version > VERSION || version == 0 {
        return Err(error(
            1,
            format!("version {version} is not known to this program"),
        ));
    }
    Ok(version)
}

fn root_line(line: &[u8], n: usize) -> Result<Vec<u8>, Error> {
    let raw = line
        .strip_prefix(b"root ")
        .ok_or_else(|| error(n, "the root line must come before the entries"))?;
    let root = unescape(raw).map_err(|e| error(n, e))?;
    check_path(&root).map_err(|e| error(n, e))?;
    check_name(&root).map_err(|e| error(n, e))?;
    Ok(root)
}

/// A bounded line reader, including comments and blank lines in the budget.
fn next_line(reader: &mut impl BufRead, total: &mut u64, n: usize) -> Result<Vec<u8>, Error> {
    let mut line = Vec::new();
    loop {
        let buf = reader.fill_buf().map_err(|e| error(n, e.to_string()))?;
        if buf.is_empty() {
            break;
        }
        let len = buf
            .iter()
            .position(|&b| b == b'\n')
            .map_or(buf.len(), |i| i + 1);
        if line.len() + len > MAX_LINE || total.saturating_add(len as u64) > MAX_FILE {
            return Err(error(
                n,
                "the line is too long or the file list is too large",
            ));
        }
        let ended = buf[len - 1] == b'\n';
        line.extend_from_slice(&buf[..len]);
        reader.consume(len);
        *total += len as u64;
        if ended {
            break;
        }
    }
    Ok(line)
}

fn trim_line(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

fn read_head(
    reader: &mut impl BufRead,
    total: &mut u64,
    cancelled: &impl Fn() -> bool,
) -> Result<(Vec<u8>, usize), Error> {
    header(trim_line(&next_line(reader, total, 1)?))?;
    let mut n = 1;
    loop {
        n += 1;
        if cancelled() {
            return Err(error(n, "cancelled"));
        }
        let line = next_line(reader, total, n)?;
        if line.is_empty() {
            return Err(error(n, "the root line is missing"));
        }
        let line = trim_line(&line);
        if !line.is_empty() && !line.starts_with(b"#") {
            return Ok((root_line(line, n)?, n));
        }
    }
}

/// Reads a whole file list within the byte, entry and path budgets.
pub fn parse(bytes: &[u8]) -> Result<FileList, Error> {
    if bytes.len() as u64 > MAX_FILE {
        return Err(error(0, "the file list is too large"));
    }
    parse_reader(&mut io::Cursor::new(bytes), &|| false)
}

fn parse_reader(
    reader: &mut impl BufRead,
    cancelled: &impl Fn() -> bool,
) -> Result<FileList, Error> {
    let mut total = 0;
    let (root, mut n) = read_head(reader, &mut total, cancelled)?;
    let mut entries = Vec::new();
    loop {
        n += 1;
        if cancelled() {
            return Err(error(n, "cancelled"));
        }
        let line = next_line(reader, &mut total, n)?;
        if line.is_empty() {
            break;
        }
        let line = trim_line(&line);
        if line.is_empty() || line.starts_with(b"#") {
            continue;
        }
        if entries.len() == MAX_ENTRIES {
            return Err(error(n, "too many entries"));
        }
        let fields: Vec<&[u8]> = line.splitn(5, |&b| b == b'\t').collect();
        let [kind, size, modified, created, path] = fields[..] else {
            return Err(error(n, "an entry needs five fields separated by tabs"));
        };
        let is_dir = match kind {
            b"d" => true,
            b"f" => false,
            _ => return Err(error(n, "the kind must be d or f")),
        };
        let size = number::<u64>(size).map_err(|_| error(n, "the size is not a number"))?;
        let modified = number::<i64>(modified)
            .map_err(|_| error(n, "the modification time is not a number"))?;
        let created =
            number::<i64>(created).map_err(|_| error(n, "the creation time is not a number"))?;
        let path = unescape(path).map_err(|e| error(n, e))?;
        validate_entry(&root, &path, n)?;
        entries.push(Entry {
            path,
            is_dir,
            size: if is_dir { 0 } else { size.unwrap_or(0) },
            modified_ns: modified,
            created_ns: created,
        });
    }
    Ok(FileList { root, entries })
}

fn validate_entry(root: &[u8], path: &[u8], n: usize) -> Result<(), Error> {
    check_path(path).map_err(|e| error(n, e))?;
    if path != root && !(path.starts_with(root) && path.get(root.len()) == Some(&b'/')) {
        return Err(error(n, "the entry is not below the root"));
    }
    Ok(())
}

fn open_regular(path: &Path) -> io::Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags, open};
    // NONBLOCK prevents opening a named pipe from waiting for a writer.
    let file = std::fs::File::from(open(
        path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > MAX_FILE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a regular file within the size limit",
        ));
    }
    Ok(file)
}

/// Reads a whole list file: a regular file (not a pipe or device) within
/// the size limit.
pub fn read_list(path: &Path) -> io::Result<FileList> {
    let mut reader = io::BufReader::new(open_regular(path)?);
    Ok(parse_reader(&mut reader, &|| false)?)
}

/// The root of a regular list file, read within the header budget.
pub fn read_root(path: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let mut reader = io::BufReader::new(open_regular(path)?.take(MAX_HEAD));
    Ok(read_head(&mut reader, &mut 0, &|| false)?.0)
}

/// An index of the entries of `list`, one root named after the list's root.
/// Folders are found by (parent, name), not by whole path, so memory and
/// time grow with the size of the list, however deep its paths.
pub fn to_index(list: &FileList) -> Result<Index, Error> {
    to_index_cancel(list, &|| false, MAX_NODES, MAX_POOL)
}

fn to_index_cancel(
    list: &FileList,
    cancelled: &impl Fn() -> bool,
    nodes: usize,
    pool: usize,
) -> Result<Index, Error> {
    check_path(&list.root).map_err(|e| error(2, e))?;
    check_name(&list.root).map_err(|e| error(2, e))?;
    if list.entries.len() > MAX_ENTRIES {
        return Err(error(0, "too many entries"));
    }
    let mut path_bytes = list.root.len();
    let mut name_bytes = list.root.len() + crate::fold::fold(&list.root).len();
    let mut store = NodeStore::new();
    let meta = |e: &Entry| Meta {
        size: e.size,
        mtime_ns: e.modified_ns.unwrap_or(0),
    };
    let root = store.add(None, &list.root, true, Meta::default());
    let mut ids: HashMap<(NodeId, Vec<u8>), NodeId> = HashMap::new();
    for (i, entry) in list.entries.iter().enumerate() {
        // Line numbers are not kept; the entry's place is close enough.
        let at = i + 3;
        if cancelled() {
            return Err(error(at, "cancelled"));
        }
        validate_entry(&list.root, &entry.path, at)?;
        path_bytes = path_bytes.saturating_add(entry.path.len());
        if path_bytes as u64 > MAX_FILE {
            return Err(error(at, "the paths exceed the memory budget"));
        }
        let both = || error(at, "the path is listed as a folder and as a file");
        // `parse` checked that the path is the root or below it.
        let below = entry.path.get(list.root.len() + 1..).unwrap_or_default();
        if below.is_empty() {
            if !entry.is_dir {
                return Err(both());
            }
            store.set_meta(root, meta(entry));
            continue;
        }
        let mut names = below.split(|&b| b == b'/').peekable();
        let mut parent = root;
        while let Some(name) = names.next() {
            let last = names.peek().is_none();
            let key = (parent, name.to_vec());
            if let Some(&id) = ids.get(&key) {
                if last {
                    if store.is_dir(id) != entry.is_dir {
                        return Err(both());
                    }
                    store.set_meta(id, meta(entry));
                } else if !store.is_dir(id) {
                    return Err(error(at, "a folder above the entry is listed as a file"));
                }
                parent = id;
                continue;
            }
            name_bytes = name_bytes.saturating_add(name.len() + crate::fold::fold(name).len());
            if store.len() >= nodes || name_bytes > pool {
                return Err(error(
                    at,
                    "the nodes or name pools exceed the memory budget",
                ));
            }
            let id = if last {
                store.add(Some(parent), name, entry.is_dir, meta(entry))
            } else {
                store.add(Some(parent), name, true, Meta::default())
            };
            ids.insert(key, id);
            parent = id;
        }
    }
    Ok(Index::new(store))
}

/// Reads the file list at `path` into an index of its own.
pub fn load(path: &Path) -> io::Result<Index> {
    load_cancellable(path, &|| false)
}

/// Reads once from a nonblocking, regular descriptor, checking cancellation
/// while reading lines and while building the store.
pub fn load_cancellable(path: &Path, cancelled: &impl Fn() -> bool) -> io::Result<Index> {
    if cancelled() {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
    }
    use std::os::unix::fs::MetadataExt;
    let file = open_regular(path)?;
    let stamp = |meta: std::fs::Metadata| {
        (
            meta.len(),
            meta.mtime(),
            meta.mtime_nsec(),
            meta.ctime(),
            meta.ctime_nsec(),
        )
    };
    let before = stamp(file.metadata()?);
    let mut reader = io::BufReader::new(file);
    let list = parse_reader(&mut reader, cancelled)?;
    let index = to_index_cancel(&list, cancelled, MAX_NODES, MAX_POOL)?;
    if before != stamp(reader.get_ref().metadata()?) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the list changed while being read",
        ));
    }
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::NodeStore;

    #[test]
    fn deep_input_is_rejected_in_a_separate_process() {
        const CHILD: &str = "EVERYGREP_DEEP_LIST_TEST";
        if std::env::var_os(CHILD).is_some() {
            let text = format!(
                "{MAGIC} {VERSION}\nroot /r\nf\t1\t\t\t/r{}\n",
                "/a".repeat(100_000)
            );
            assert!(parse(text.as_bytes()).is_err());
            return;
        }
        assert!(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "filelist::tests::deep_input_is_rejected_in_a_separate_process"
                ])
                .env(CHILD, "1")
                .status()
                .unwrap()
                .success()
        );
        let text = format!(
            "{MAGIC} {VERSION}\nroot /r\nf\t1\t\t\t/r{}\n",
            "/a".repeat(MAX_DEPTH - 1)
        );
        assert_eq!(
            to_index(&parse(text.as_bytes()).unwrap()).unwrap().len(),
            MAX_DEPTH
        );
    }

    #[test]
    fn raw_and_folded_names_and_public_list_values_are_checked() {
        for name in ["a".repeat(65536), "İ".repeat(22000)] {
            let text = format!("{MAGIC} {VERSION}\nroot /d\nf\t1\t\t\t/d/{name}\n");
            assert_eq!(parse(text.as_bytes()).unwrap_err().line, 3);
            let root = format!("{MAGIC} {VERSION}\nroot /{name}\n");
            assert_eq!(parse(root.as_bytes()).unwrap_err().line, 2);
            let list = FileList {
                root: b"/d".to_vec(),
                entries: vec![Entry {
                    path: format!("/d/{name}").into_bytes(),
                    ..Entry::default()
                }],
            };
            assert!(to_index(&list).is_err());
        }
        let text = format!(
            "{MAGIC} {VERSION}\nroot /d\nf\t1\t\t\t/d/{}\n",
            "a".repeat(MAX_PATH - 3)
        );
        assert_eq!(to_index(&parse(text.as_bytes()).unwrap()).unwrap().len(), 2);
        for byte in 1..=255 {
            if byte == b'/' || byte == b'.' {
                continue;
            }
            let list = FileList {
                root: b"/d".to_vec(),
                entries: vec![Entry {
                    path: vec![b'/', b'd', b'/', byte],
                    ..Entry::default()
                }],
            };
            let mut bytes = Vec::new();
            write_list(&mut bytes, &list).unwrap();
            assert!(to_index(&parse(&bytes).unwrap()).is_ok());
        }
    }

    #[test]
    fn file_line_entry_and_store_budgets_are_enforced() {
        let text = format!(
            "{MAGIC} {VERSION}\nroot /r\n{}",
            "f\t1\t\t\t/r/a\n".repeat(MAX_ENTRIES + 1)
        );
        assert!(
            parse(text.as_bytes())
                .unwrap_err()
                .what
                .contains("too many")
        );
        let text = format!("{MAGIC} {VERSION}\nroot /r\nf\t1\t\t\t/r/a/b/c\n");
        let list = parse(text.as_bytes()).unwrap();
        assert!(to_index_cancel(&list, &|| false, 3, MAX_POOL).is_err());
        assert!(to_index_cancel(&list, &|| false, MAX_NODES, 5).is_err());
        assert!(to_index_cancel(&list, &|| true, MAX_NODES, MAX_POOL).is_err());
        let dir = std::env::temp_dir().join(format!("eg-list-limits-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("large.eglist");
        std::fs::File::create(&file)
            .unwrap()
            .set_len(MAX_FILE + 1)
            .unwrap();
        assert!(
            load(&file)
                .err()
                .unwrap()
                .to_string()
                .contains("size limit")
        );
        assert!(
            read_root(&file)
                .err()
                .unwrap()
                .to_string()
                .contains("size limit")
        );
        std::fs::write(&file, vec![b'x'; MAX_LINE + 1]).unwrap();
        assert!(load(&file).is_err());
        assert!(read_root(&file).is_err());
        std::fs::write(
            &file,
            format!("{MAGIC} {VERSION}\nroot /r\n#{}", "x".repeat(MAX_LINE)),
        )
        .unwrap();
        assert!(load(&file).is_err());
        assert!(load_cancellable(&file, &|| true).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn special_files_are_refused_without_waiting() {
        const CHILD: &str = "EVERYGREP_SPECIAL_LIST_TEST";
        if std::env::var_os(CHILD).is_some() {
            let dir = std::env::temp_dir().join(format!("eg-list-special-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let fifo = dir.join("pipe.eglist");
            rustix::fs::mkfifoat(
                rustix::fs::CWD,
                &fifo,
                rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            )
            .unwrap();
            let zero = dir.join("zero.eglist");
            std::os::unix::fs::symlink("/dev/zero", &zero).unwrap();
            for path in [&fifo, &zero] {
                assert!(load(path).is_err());
                assert!(read_root(path).is_err());
            }
            std::fs::remove_dir_all(dir).unwrap();
            return;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "filelist::tests::special_files_are_refused_without_waiting",
            ])
            .env(CHILD, "1")
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("a special file blocked the reader");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn odd_entries() -> FileList {
        let root = b"/run/media/u/Disk \\ 1".to_vec();
        let under = |name: &[u8]| {
            let mut p = root.clone();
            p.push(b'/');
            p.extend_from_slice(name);
            p
        };
        FileList {
            root: root.clone(),
            entries: vec![
                Entry {
                    path: root.clone(),
                    is_dir: true,
                    size: 0,
                    modified_ns: Some(1_730_000_000_000_000_000),
                    created_ns: None,
                },
                Entry {
                    path: under(b"tab\there\nnew\rline"),
                    is_dir: true,
                    size: 0,
                    modified_ns: Some(-5),
                    created_ns: Some(7),
                },
                Entry {
                    path: under(b"tab\there\nnew\rline/\xff\xfe latin1 caf\xe9.txt"),
                    is_dir: false,
                    size: 1024,
                    modified_ns: None,
                    created_ns: Some(1_720_000_000_000_000_000),
                },
                Entry {
                    path: under("bell\u{7}del\u{7f} əöü #x \\x41.txt".as_bytes()),
                    is_dir: false,
                    size: 0,
                    modified_ns: Some(0),
                    created_ns: None,
                },
            ],
        }
    }

    #[test]
    fn odd_names_round_trip_byte_for_byte() {
        let list = odd_entries();
        let mut text = Vec::new();
        write_list(&mut text, &list).unwrap();
        // One line per entry plus the two header lines: no raw line breaks
        // or tabs inside paths.
        assert_eq!(text.iter().filter(|&&b| b == b'\n').count(), 6);
        assert!(std::str::from_utf8(&text).is_ok(), "the file is UTF-8");
        let text_str = String::from_utf8(text.clone()).unwrap();
        assert!(text_str.starts_with("EveryGrep file list 1\nroot /run/media/u/Disk \\\\ 1\n"));
        assert!(text_str.contains("\\xFF\\xFE latin1 caf\\xE9.txt"));
        assert!(text_str.contains("bell\\x07del\\x7F əöü #x \\\\x41.txt"));
        assert_eq!(parse(&text).unwrap(), list);
    }

    #[test]
    fn arbitrary_input_and_numeric_overflow_are_errors_without_panics() {
        let mut seed = 17_u32;
        for len in 0..512 {
            let mut bytes = Vec::new();
            for _ in 0..len {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                bytes.push((seed >> 24) as u8);
            }
            let _ = parse(&bytes);
            let mut prefixed = b"EveryGrep file list 1\nroot /r\n".to_vec();
            prefixed.extend_from_slice(&bytes);
            if let Ok(list) = parse(&prefixed) {
                let _ = to_index(&list);
            }
        }
        for fields in [
            "18446744073709551616\t\t",
            "1\t9223372036854775808\t",
            "1\t\t-9223372036854775809",
        ] {
            let text = format!("{MAGIC} {VERSION}\nroot /r\nf\t{fields}\t/r/a\n");
            assert_eq!(parse(text.as_bytes()).unwrap_err().line, 3);
        }
        let root = format!("{MAGIC} {VERSION}\nroot /{}\n", "a".repeat(MAX_PATH - 1));
        assert_eq!(to_index(&parse(root.as_bytes()).unwrap()).unwrap().len(), 1);
        let folded = format!(
            "{MAGIC} {VERSION}\nroot /r\nf\t1\t\t\t/r/{}\n",
            "İ".repeat(21845)
        );
        assert_eq!(
            to_index(&parse(folded.as_bytes()).unwrap()).unwrap().len(),
            2
        );
    }

    #[test]
    fn in_place_changes_and_cancellation_during_read_are_detected() {
        let dir = std::env::temp_dir().join(format!("eg-list-changing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("list.eglist");
        std::fs::write(&path, b"EveryGrep file list 1\nroot /a\nf\t1\t\t\t/a/old\n").unwrap();
        let calls = std::cell::Cell::new(0);
        let result = load_cancellable(&path, &|| {
            calls.set(calls.get() + 1);
            if calls.get() == 3 {
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap()
                    .write_all(b"# changed\n")
                    .unwrap();
            }
            false
        });
        assert!(result.err().unwrap().to_string().contains("changed while"));
        let calls = std::cell::Cell::new(0);
        assert!(
            load_cancellable(&path, &|| {
                calls.set(calls.get() + 1);
                calls.get() >= 3
            })
            .is_err()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn replacing_the_path_does_not_change_the_opened_list() {
        let dir = std::env::temp_dir().join(format!("eg-list-open-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("list.eglist");
        let replacement = dir.join("replacement.eglist");
        std::fs::write(&path, b"EveryGrep file list 1\nroot /a\nf\t1\t\t\t/a/old\n").unwrap();
        std::fs::write(
            &replacement,
            b"EveryGrep file list 1\nroot /b\nf\t1\t\t\t/b/new\n",
        )
        .unwrap();
        let file = open_regular(&path).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let list = parse_reader(&mut io::BufReader::new(file), &|| false).unwrap();
        assert_eq!(list.root, b"/a");
        assert_eq!(list.entries[0].path, b"/a/old");
        assert_eq!(load(&path).unwrap().store().name(NodeId(0)), b"/b");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn comments_blank_lines_and_crlf_are_read() {
        let text =
            b"EveryGrep file list 1\r\n# made by hand\n\nroot /d\nf\t3\t\t\t/d/a\r\n\n# end\n";
        let list = parse(text).unwrap();
        assert_eq!(list.root, b"/d");
        assert_eq!(list.entries.len(), 1);
        assert_eq!(list.entries[0].size, 3);
        let dir = std::env::temp_dir().join(format!("eg-filelist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("l.eglist");
        std::fs::write(&file, text).unwrap();
        assert_eq!(read_root(&file).unwrap(), b"/d");
        let index = load(&file).unwrap();
        assert_eq!(index.len(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bad_lists_say_which_line_is_wrong() {
        let cases: [(&[u8], usize, &str); 10] = [
            (b"", 1, "not an EveryGrep file list"),
            (b"EveryGrep file list 2\nroot /d\n", 1, "version 2"),
            (b"EveryGrep file list 1\n", 2, "root line is missing"),
            (b"EveryGrep file list 1\nroot d\n", 2, "not absolute"),
            (b"EveryGrep file list 1\nroot /\n", 2, "cannot be /"),
            (
                b"EveryGrep file list 1\nroot /d\nf\t1\t\t/d/a\n",
                3,
                "five fields",
            ),
            (
                b"EveryGrep file list 1\nroot /d\nx\t1\t\t\t/d/a\n",
                3,
                "d or f",
            ),
            (
                b"EveryGrep file list 1\nroot /d\nf\tz\t\t\t/d/a\n",
                3,
                "size",
            ),
            (
                b"EveryGrep file list 1\nroot /d\nf\t1\t\t\t/e/a\n",
                3,
                "below the root",
            ),
            (
                b"EveryGrep file list 1\nroot /d\nf\t1\t\t\t/d/a\\q\n",
                3,
                "escape",
            ),
        ];
        for (text, line, what) in cases {
            let e = parse(text).unwrap_err();
            assert_eq!(e.line, line, "{e}");
            assert!(e.what.contains(what), "{e}");
        }
        assert!(parse(b"EveryGrep file list 1\nroot /d\nf\t1\t\t\t/d/../x\n").is_err());
        assert!(parse(b"EveryGrep file list 1\nroot /d\nf\t1\t\t\t/dx\n").is_err());
    }

    #[test]
    fn the_index_of_a_list_adds_missing_folders_and_keeps_the_last_duplicate() {
        let text = b"EveryGrep file list 1\nroot /d\n\
            f\t5\t9\t\t/d/x/y/a.txt\n\
            f\t7\t\t\t/d/b\n\
            f\t8\t\t\t/d/b\n";
        let index = to_index(&parse(text).unwrap()).unwrap();
        let store = index.store();
        assert_eq!(index.roots().len(), 1);
        let root = index.roots()[0];
        assert_eq!(store.name(root), b"/d");
        assert_eq!(store.meta(root).size, 13, "folder totals");
        let x = index.find_child(Some(root), b"x").unwrap();
        assert!(store.is_dir(x));
        let y = index.find_child(Some(x), b"y").unwrap();
        let a = index.find_child(Some(y), b"a.txt").unwrap();
        assert_eq!(
            store.meta(a),
            Meta {
                size: 5,
                mtime_ns: 9
            }
        );
        assert_eq!(
            store.meta(index.find_child(Some(root), b"b").unwrap()).size,
            8
        );
        assert_eq!(index.len(), 5);
        let bad = b"EveryGrep file list 1\nroot /d\nf\t1\t\t\t/d/b\nf\t1\t\t\t/d/b/c\n";
        assert!(to_index(&parse(bad).unwrap()).is_err());
        let both = b"EveryGrep file list 1\nroot /d\nf\t1\t\t\t/d/b\nd\t\t\t\t/d/b\n";
        assert!(to_index(&parse(both).unwrap()).is_err());
    }

    #[test]
    fn an_index_folder_is_written_and_read_back() {
        let mut store = NodeStore::new();
        let root = store.add(
            None,
            b"/m/disk",
            true,
            Meta {
                size: 0,
                mtime_ns: 11,
            },
        );
        let sub = store.add(Some(root), b"s\nub", true, Meta::default());
        store.add(
            Some(sub),
            b"f\xff",
            false,
            Meta {
                size: 3,
                mtime_ns: 12,
            },
        );
        store.add(
            Some(root),
            b"g",
            false,
            Meta {
                size: 4,
                mtime_ns: 0,
            },
        );
        let index = Index::new(store);
        let mut out = Vec::new();
        let mut asked = Vec::new();
        let written = write_index(
            &mut out,
            &index,
            index.roots()[0],
            &mut |p| {
                asked.push(p.to_path_buf());
                Some(42)
            },
            &Progress::new(),
        )
        .unwrap();
        assert_eq!(written, 4);
        assert_eq!(asked.len(), 4);
        let list = parse(&out).unwrap();
        assert_eq!(list.root, b"/m/disk");
        let paths: Vec<&[u8]> = list.entries.iter().map(|e| &e.path[..]).collect();
        let at = |p: &[u8]| paths.iter().position(|q| *q == p).unwrap();
        assert_eq!(at(b"/m/disk"), 0);
        assert!(
            at(b"/m/disk/s\nub") < at(b"/m/disk/s\nub/f\xff"),
            "folders first"
        );
        let g = &list.entries[at(b"/m/disk/g")];
        assert_eq!((g.size, g.modified_ns), (4, None), "0 is unknown");
        assert!(list.entries.iter().all(|e| e.created_ns == Some(42)));
        let again = to_index(&list).unwrap();
        assert_eq!(again.len(), 4);
        assert_eq!(again.store().meta(again.roots()[0]).size, 7);
        // A cancel stops the writing.
        let progress = Progress::new();
        progress.cancel();
        let e = write_index(
            &mut Vec::new(),
            &index,
            index.roots()[0],
            &mut |_| None,
            &progress,
        )
        .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Interrupted);
    }
}
