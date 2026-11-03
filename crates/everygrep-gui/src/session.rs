//! Opt-in, last-window session storage beside settings and history.
use crate::saved_searches::{self, NamedFilter};
use crate::settings::{self, Columns};
use crate::tabs::{CLOSED_LIMIT, SearchState, Tab, Tabs, tab_options};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

const HEADER: &str = "EveryGrep session 1";
const MAX_BYTES: u64 = 8 * 1024 * 1024;
const MAX_TABS: usize = 1000;
const SAVE_DELAY: Duration = Duration::from_secs(15);

pub struct Start {
    pub scope: String,
    pub restore: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    pub scope: String,
    pub open: Vec<SearchState>,
    pub active: usize,
    pub closed: Vec<SearchState>,
}
impl Session {
    pub fn capture(scope: &str, tabs: &Tabs) -> Self {
        Self {
            scope: scope.into(),
            open: tabs
                .open
                .iter()
                .map(|t| {
                    let mut search = t.search.clone();
                    search.options = tab_options(search.options);
                    if tabs.forgotten.contains(&search.query) {
                        search.query.clear();
                        search.filter = None;
                        search.filter_snapshot = false;
                    }
                    search
                })
                .collect(),
            active: tabs.active,
            closed: tabs
                .closed
                .iter()
                .filter(|t| !tabs.forgotten.contains(&t.search.query))
                .map(|t| {
                    let mut s = t.search.clone();
                    s.options = tab_options(s.options);
                    s
                })
                .collect(),
        }
    }
    pub fn restore(self, scope: &str) -> Option<Tabs> {
        if self.scope != scope {
            return None;
        }
        Some(Tabs {
            open: self
                .open
                .into_iter()
                .map(|search| Tab {
                    search,
                    ..Default::default()
                })
                .collect(),
            active: self.active,
            closed: self
                .closed
                .into_iter()
                .map(|search| Tab {
                    search,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        })
    }
    pub fn load(path: &Path) -> io::Result<Option<Self>> {
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let mut text = String::new();
        file.take(MAX_BYTES + 1).read_to_string(&mut text)?;
        if text.len() as u64 > MAX_BYTES {
            return Err(invalid("session is too large"));
        }
        Self::parse(&text).map(Some)
    }
    fn parse(text: &str) -> io::Result<Self> {
        let mut lines = text.lines();
        if lines.next() != Some(HEADER) {
            return Err(invalid("invalid session header"));
        }
        let mut session = Self {
            scope: String::new(),
            open: Vec::new(),
            active: 0,
            closed: Vec::new(),
        };
        let mut current: Option<(bool, SearchState)> = None;
        for line in lines {
            let line = line.trim_ascii();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(invalid("invalid session line"));
            };
            let (key, value) = (key.trim_ascii(), value.trim_ascii());
            match key {
                "scope" => session.scope = decode(value)?,
                "active" => {
                    session.active = value.parse().map_err(|_| invalid("invalid active tab"))?
                }
                "tab" => {
                    if let Some((closed, search)) = current.take() {
                        session.push(closed, search)?;
                    }
                    current = Some((
                        match value {
                            "open" => false,
                            "closed" => true,
                            _ => return Err(invalid("invalid tab state")),
                        },
                        SearchState::default(),
                    ));
                }
                "query" | "options" | "match-options" | "filter" | "filter-snapshot" | "sort"
                | "columns" | "widths" => {
                    let search = &mut current
                        .as_mut()
                        .ok_or_else(|| invalid("tab field without tab"))?
                        .1;
                    match key {
                        "query" => search.query = decode(value)?,
                        "options" => search.options = tab_options(options(value)?), // Legacy policy is ignored.
                        "match-options" => {
                            let (filter, bits) = value
                                .split_once('\t')
                                .ok_or_else(|| invalid("invalid tab options"))?;
                            let bits: u16 = bits
                                .parse()
                                .ok()
                                .filter(|b| *b < 512)
                                .ok_or_else(|| invalid("invalid tab options"))?;
                            search.options = tab_options(saved_searches::decode_options(
                                filter,
                                &bits.to_string(),
                                "hide",
                            )?);
                        }
                        "filter-snapshot" => search.filter_snapshot = boolean(value)?,
                        "filter" => {
                            if value == "none" {
                                search.filter = None;
                                search.filter_snapshot = false;
                                continue;
                            }
                            let p: Vec<_> = value.split('\t').collect();
                            if p.len() != 6 {
                                return Err(invalid("invalid filter snapshot"));
                            }
                            search.filter = Some(NamedFilter {
                                id: p[0]
                                    .parse()
                                    .map_err(|_| invalid("invalid filter identity"))?,
                                name: decode(p[1])?,
                                search: decode(p[2])?,
                                options: saved_searches::decode_options(p[3], p[4], p[5])?,
                            });
                        }
                        "sort" => {
                            let (column, descending) = value
                                .split_once('\t')
                                .ok_or_else(|| invalid("invalid sort"))?;
                            search.sort_column = column
                                .parse::<i32>()
                                .ok()
                                .filter(|c| crate::is_sort_column(*c))
                                .ok_or_else(|| invalid("invalid sort column"))?;
                            search.descending = boolean(descending)?;
                        }
                        "columns" => shown_columns(value, &mut search.columns),
                        "widths" => column_widths(value, &mut search.columns)?,
                        _ => unreachable!(),
                    }
                }
                _ => {} // Future keys are ignored; duplicate known fields use the last value.
            }
        }
        if let Some((closed, search)) = current {
            session.push(closed, search)?;
        }
        if session.open.is_empty() || session.scope.is_empty() {
            return Err(invalid("empty session"));
        }
        session.active = session.active.min(session.open.len() - 1);
        let extra = session.closed.len().saturating_sub(CLOSED_LIMIT);
        session.closed.drain(..extra);
        Ok(session)
    }
    fn push(&mut self, closed: bool, search: SearchState) -> io::Result<()> {
        if self.open.len() + self.closed.len() >= MAX_TABS {
            return Err(invalid("too many session tabs"));
        }
        if closed {
            self.closed.push(search);
        } else {
            self.open.push(search);
        }
        Ok(())
    }
    pub fn text(&self) -> String {
        let mut text = format!(
            "{HEADER}\nscope = {}\nactive = {}\n",
            settings::escape(self.scope.as_bytes()),
            self.active
        );
        for (closed, search) in self
            .open
            .iter()
            .map(|s| (false, s))
            .chain(self.closed.iter().map(|s| (true, s)))
        {
            text.push_str(&format!(
                "tab = {}\nquery = {}\nmatch-options = {}\nsort = {}\t{}\n",
                if closed { "closed" } else { "open" },
                settings::escape(search.query.as_bytes()),
                saved_searches::encode_options(tab_options(search.options))
                    .rsplit_once('\t')
                    .unwrap()
                    .0,
                search.sort_column,
                u8::from(search.descending)
            ));
            text.push_str(&format!(
                "filter-snapshot = {}\n",
                u8::from(search.filter_snapshot)
            ));
            if let Some(f) = &search.filter {
                text.push_str(&format!(
                    "filter = {}\t{}\t{}\t{}\n",
                    f.id,
                    settings::escape(f.name.as_bytes()),
                    settings::escape(f.search.as_bytes()),
                    saved_searches::encode_options(f.options)
                ));
            } else {
                text.push_str("filter = none\n");
            }
            let c = &search.columns;
            let shown: Vec<&str> = column_flags(c)
                .into_iter()
                .filter_map(|(name, on)| on.then_some(name))
                .collect();
            text.push_str(&format!("columns = {}\nwidths =", shown.join(" ")));
            for (name, width) in column_widths_of(c) {
                if let Some(width) = width {
                    text.push_str(&format!(" {name}:{width}"));
                }
            }
            text.push('\n');
        }
        text
    }
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let text = self.text();
        // Never create a file our own reader would reject.
        if text.len() as u64 > MAX_BYTES {
            return Err(invalid("session is too large"));
        }
        Self::parse(&text)?;
        let _lock = settings::lock(path)?;
        // Preserve unreadable or malformed existing data for recovery.
        Self::load(path)?;
        let (mut file, tmp) = settings::create_temp(path)?;
        let result = (|| {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&tmp, path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(tmp);
        }
        result
    }
}
fn invalid(s: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, s)
}
fn decode(s: &str) -> io::Result<String> {
    settings::unescape(s)
        .and_then(|v| String::from_utf8(v).ok())
        .ok_or_else(|| invalid("invalid escaped text"))
}
fn boolean(s: &str) -> io::Result<bool> {
    match s {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(invalid("invalid boolean")),
    }
}
fn options(s: &str) -> io::Result<everygrep_core::query::QueryOptions> {
    let p: Vec<_> = s.split('\t').collect();
    if p.len() != 3 {
        return Err(invalid("invalid tab options"));
    }
    saved_searches::decode_options(p[0], p[1], p[2])
}
/// The optional columns a tab can show, by their name in the file.
fn column_flags(c: &Columns) -> [(&'static str, bool); 48] {
    [
        ("type", c.show_type),
        ("ext", c.show_ext),
        ("created", c.show_created),
        ("accessed", c.show_accessed),
        ("attrs", c.show_attrs),
        ("md5", c.show_md5),
        ("sha256", c.show_sha256),
        ("crc32", c.show_crc32),
        ("name-freq", c.show_name_freq),
        ("ext-freq", c.show_ext_freq),
        ("size-freq", c.show_size_freq),
        ("full-path", c.show_full_path),
        ("parent-path", c.show_parent_path),
        ("stem", c.show_stem),
        ("name-length", c.show_name_length),
        ("path-length", c.show_path_length),
        ("disk-size", c.show_disk_size),
        ("total-size", c.show_total_size),
        ("child-count", c.show_child_count),
        ("child-files", c.show_child_files),
        ("child-folders", c.show_child_folders),
        ("desc-count", c.show_desc_count),
        ("desc-files", c.show_desc_files),
        ("desc-folders", c.show_desc_folders),
        ("folder-depth", c.show_folder_depth),
        ("hard-links", c.show_hard_links),
        ("shortcut-target", c.show_shortcut_target),
        ("owner", c.show_owner),
        ("width", c.show_width),
        ("height", c.show_height),
        ("dimensions", c.show_dimensions),
        ("length", c.show_length),
        ("bit-rate", c.show_bit_rate),
        ("title", c.show_title),
        ("artist", c.show_artist),
        ("album", c.show_album),
        ("year", c.show_year),
        ("track", c.show_track),
        ("genre", c.show_genre),
        ("rating", c.show_rating),
        ("tags", c.show_tags),
        ("page-count", c.show_page_count),
        ("word-count", c.show_word_count),
        ("camera-maker", c.show_camera_maker),
        ("camera-model", c.show_camera_model),
        ("date-taken", c.show_date_taken),
        ("gps-lat", c.show_gps_lat),
        ("gps-long", c.show_gps_long),
    ]
}

fn column_flag<'a>(c: &'a mut Columns, name: &str) -> Option<&'a mut bool> {
    Some(match name {
        "type" => &mut c.show_type,
        "ext" => &mut c.show_ext,
        "created" => &mut c.show_created,
        "accessed" => &mut c.show_accessed,
        "attrs" => &mut c.show_attrs,
        "md5" => &mut c.show_md5,
        "sha256" => &mut c.show_sha256,
        "crc32" => &mut c.show_crc32,
        "name-freq" => &mut c.show_name_freq,
        "ext-freq" => &mut c.show_ext_freq,
        "size-freq" => &mut c.show_size_freq,
        "full-path" => &mut c.show_full_path,
        "parent-path" => &mut c.show_parent_path,
        "stem" => &mut c.show_stem,
        "name-length" => &mut c.show_name_length,
        "path-length" => &mut c.show_path_length,
        "disk-size" => &mut c.show_disk_size,
        "total-size" => &mut c.show_total_size,
        "child-count" => &mut c.show_child_count,
        "child-files" => &mut c.show_child_files,
        "child-folders" => &mut c.show_child_folders,
        "desc-count" => &mut c.show_desc_count,
        "desc-files" => &mut c.show_desc_files,
        "desc-folders" => &mut c.show_desc_folders,
        "folder-depth" => &mut c.show_folder_depth,
        "hard-links" => &mut c.show_hard_links,
        "shortcut-target" => &mut c.show_shortcut_target,
        "owner" => &mut c.show_owner,
        "width" => &mut c.show_width,
        "height" => &mut c.show_height,
        "dimensions" => &mut c.show_dimensions,
        "length" => &mut c.show_length,
        "bit-rate" => &mut c.show_bit_rate,
        "title" => &mut c.show_title,
        "artist" => &mut c.show_artist,
        "album" => &mut c.show_album,
        "year" => &mut c.show_year,
        "track" => &mut c.show_track,
        "genre" => &mut c.show_genre,
        "rating" => &mut c.show_rating,
        "tags" => &mut c.show_tags,
        "page-count" => &mut c.show_page_count,
        "word-count" => &mut c.show_word_count,
        "camera-maker" => &mut c.show_camera_maker,
        "camera-model" => &mut c.show_camera_model,
        "date-taken" => &mut c.show_date_taken,
        "gps-lat" => &mut c.show_gps_lat,
        "gps-long" => &mut c.show_gps_long,
        _ => return None,
    })
}

/// The column widths of a tab, by their name in the file.
fn column_widths_of(c: &Columns) -> [(&'static str, Option<f32>); 52] {
    [
        ("name", c.name),
        ("folder", c.folder),
        ("size", c.size),
        ("date", c.date),
        ("type", c.type_),
        ("ext", c.ext),
        ("created", c.created),
        ("accessed", c.accessed),
        ("attrs", c.attrs),
        ("md5", c.md5),
        ("sha256", c.sha256),
        ("crc32", c.crc32),
        ("name-freq", c.name_freq),
        ("ext-freq", c.ext_freq),
        ("size-freq", c.size_freq),
        ("full-path", c.full_path),
        ("parent-path", c.parent_path),
        ("stem", c.stem),
        ("name-length", c.name_length),
        ("path-length", c.path_length),
        ("disk-size", c.disk_size),
        ("total-size", c.total_size),
        ("child-count", c.child_count),
        ("child-files", c.child_files),
        ("child-folders", c.child_folders),
        ("desc-count", c.desc_count),
        ("desc-files", c.desc_files),
        ("desc-folders", c.desc_folders),
        ("folder-depth", c.folder_depth),
        ("hard-links", c.hard_links),
        ("shortcut-target", c.shortcut_target),
        ("owner", c.owner),
        ("width", c.width),
        ("height", c.height),
        ("dimensions", c.dimensions),
        ("length", c.length),
        ("bit-rate", c.bit_rate),
        ("title", c.title),
        ("artist", c.artist),
        ("album", c.album),
        ("year", c.year),
        ("track", c.track),
        ("genre", c.genre),
        ("rating", c.rating),
        ("tags", c.tags),
        ("page-count", c.page_count),
        ("word-count", c.word_count),
        ("camera-maker", c.camera_maker),
        ("camera-model", c.camera_model),
        ("date-taken", c.date_taken),
        ("gps-lat", c.gps_lat),
        ("gps-long", c.gps_long),
    ]
}

fn column_width<'a>(c: &'a mut Columns, name: &str) -> Option<&'a mut Option<f32>> {
    Some(match name {
        "name" => &mut c.name,
        "folder" => &mut c.folder,
        "size" => &mut c.size,
        "date" => &mut c.date,
        "type" => &mut c.type_,
        "ext" => &mut c.ext,
        "created" => &mut c.created,
        "accessed" => &mut c.accessed,
        "attrs" => &mut c.attrs,
        "md5" => &mut c.md5,
        "sha256" => &mut c.sha256,
        "crc32" => &mut c.crc32,
        "name-freq" => &mut c.name_freq,
        "ext-freq" => &mut c.ext_freq,
        "size-freq" => &mut c.size_freq,
        "full-path" => &mut c.full_path,
        "parent-path" => &mut c.parent_path,
        "stem" => &mut c.stem,
        "name-length" => &mut c.name_length,
        "path-length" => &mut c.path_length,
        "disk-size" => &mut c.disk_size,
        "total-size" => &mut c.total_size,
        "child-count" => &mut c.child_count,
        "child-files" => &mut c.child_files,
        "child-folders" => &mut c.child_folders,
        "desc-count" => &mut c.desc_count,
        "desc-files" => &mut c.desc_files,
        "desc-folders" => &mut c.desc_folders,
        "folder-depth" => &mut c.folder_depth,
        "hard-links" => &mut c.hard_links,
        "shortcut-target" => &mut c.shortcut_target,
        "owner" => &mut c.owner,
        "width" => &mut c.width,
        "height" => &mut c.height,
        "dimensions" => &mut c.dimensions,
        "length" => &mut c.length,
        "bit-rate" => &mut c.bit_rate,
        "title" => &mut c.title,
        "artist" => &mut c.artist,
        "album" => &mut c.album,
        "year" => &mut c.year,
        "track" => &mut c.track,
        "genre" => &mut c.genre,
        "rating" => &mut c.rating,
        "tags" => &mut c.tags,
        "page-count" => &mut c.page_count,
        "word-count" => &mut c.word_count,
        "camera-maker" => &mut c.camera_maker,
        "camera-model" => &mut c.camera_model,
        "date-taken" => &mut c.date_taken,
        "gps-lat" => &mut c.gps_lat,
        "gps-long" => &mut c.gps_long,
        _ => return None,
    })
}

/// `columns = type md5 …`: the optional columns shown, every other one
/// hidden. Names a later version may add are skipped.
fn shown_columns(s: &str, c: &mut Columns) {
    for (name, _) in column_flags(c) {
        *column_flag(c, name).expect("listed") = false;
    }
    for name in s.split_ascii_whitespace() {
        if let Some(flag) = column_flag(c, name) {
            *flag = true;
        }
    }
}

/// `widths = name:350 size:96 …`: the widths set, every other one unset.
/// Names a later version may add are skipped; a bad width is refused.
fn column_widths(s: &str, c: &mut Columns) -> io::Result<()> {
    for (name, _) in column_widths_of(c) {
        *column_width(c, name).expect("listed") = None;
    }
    for item in s.split_ascii_whitespace() {
        let (name, value) = item
            .split_once(':')
            .ok_or_else(|| invalid("invalid column width"))?;
        let width = value
            .parse::<f32>()
            .ok()
            .filter(|n| n.is_finite() && (1.0..=10_000.0).contains(n))
            .ok_or_else(|| invalid("invalid column width"))?;
        if let Some(slot) = column_width(c, name) {
            *slot = Some(width);
        }
    }
    Ok(())
}

/// Leading write, followed by at most one trailing write in a fixed window.
#[derive(Default)]
struct SaveWindow {
    deadline: Option<Duration>,
    dirty: bool,
}
impl SaveWindow {
    fn changed(&mut self, now: Duration) -> bool {
        if self.deadline.is_some() {
            self.dirty = true;
            false
        } else {
            self.deadline = Some(now + SAVE_DELAY);
            self.dirty = false;
            true
        }
    }
    fn due(&mut self, now: Duration) -> bool {
        if self.deadline.is_some_and(|d| now >= d) {
            self.deadline = None;
            std::mem::take(&mut self.dirty)
        } else {
            false
        }
    }
}

pub struct Writer {
    path: Option<PathBuf>,
    latest: Option<Session>,
    window: SaveWindow,
    enabled: bool,
    disabled_clean: bool,
}
impl Writer {
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            latest: None,
            window: SaveWindow::default(),
            enabled: false,
            disabled_clean: false,
        }
    }
    pub fn observe(&mut self, enabled: bool, snapshot: Session, now: Duration) -> io::Result<()> {
        if !enabled {
            if !self.disabled_clean {
                self.forget()?;
            }
            return Ok(());
        }
        self.disabled_clean = false;
        if !self.enabled {
            self.enabled = true;
            self.latest = None;
        }
        let due = self.window.due(now);
        let changed = self.latest.as_ref() != Some(&snapshot);
        self.latest = Some(snapshot);
        let immediate = changed && self.window.changed(now);
        if due || immediate {
            self.flush()?;
        }
        Ok(())
    }
    pub fn finish(&mut self, enabled: bool, snapshot: Session) -> io::Result<()> {
        if !enabled {
            return self.forget();
        }
        self.disabled_clean = false;
        self.enabled = enabled;
        self.latest = Some(snapshot);
        self.flush()
    }
    pub fn forget(&mut self) -> io::Result<()> {
        self.enabled = false;
        self.latest = None;
        self.window = SaveWindow::default();
        let Some(path) = self.path.as_deref() else {
            return Ok(());
        };
        // Do not create directories or lock files while persistence is disabled.
        match std::fs::symlink_metadata(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.disabled_clean = true;
                return Ok(());
            }
            Err(e) => return Err(e),
            Ok(_) => {}
        }
        let _lock = settings::lock(path)?;
        let result = match std::fs::remove_file(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        };
        self.disabled_clean = result.is_ok();
        result
    }

    pub fn flush(&self) -> io::Result<()> {
        if self.enabled {
            let path = self.path.as_deref().ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "no configuration folder")
            })?;
            if let Some(snapshot) = &self.latest {
                snapshot.save(path)?;
            }
        }
        Ok(())
    }
}

/// What the window asks of its session file (see [`Saver::ask`]).
pub enum Ask {
    /// [`Writer::observe`]: saves now or later, or deletes when disabled.
    Observe {
        enabled: bool,
        snapshot: Session,
        now: Duration,
    },
    /// [`Writer::finish`]: the last save of the window.
    Finish { enabled: bool, snapshot: Session },
    /// [`Writer::forget`]: deletes the file (history cleared, restore off).
    Forget,
}

/// Asks not run yet: a deletion, then the newest other ask.
#[derive(Default)]
struct Queued {
    forget: bool,
    latest: Option<Ask>,
    /// A job that takes them is waiting on the saving thread.
    scheduled: bool,
}

/// Keeps the session's disk work (lock wait, read of the old file, fsync,
/// rename, delete) off the window's thread: the [`Writer`] runs in jobs on
/// the settings saving thread, in order with the settings and history
/// saves. While a job waits, a newer snapshot replaces the one it would
/// save; a deletion asked meanwhile is never dropped, and drops the
/// snapshots asked before it.
pub struct Saver {
    path: Option<PathBuf>,
    writer: std::sync::Arc<std::sync::Mutex<Writer>>,
    queued: std::sync::Arc<std::sync::Mutex<Queued>>,
}

impl Saver {
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            writer: std::sync::Arc::new(std::sync::Mutex::new(Writer::new(path.clone()))),
            path,
            queued: std::sync::Arc::default(),
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Notes `ask`. Returns the job to run on the saving thread when none is
    /// waiting already (the waiting one takes `ask` too); the job returns
    /// how the disk work went.
    pub fn ask(&self, ask: Ask) -> Option<impl FnOnce() -> io::Result<()> + Send + 'static> {
        let mut q = self.queued.lock().unwrap_or_else(|e| e.into_inner());
        match ask {
            Ask::Forget => {
                q.forget = true;
                q.latest = None;
            }
            ask => q.latest = Some(ask),
        }
        if std::mem::replace(&mut q.scheduled, true) {
            return None;
        }
        let (writer, queued) = (self.writer.clone(), self.queued.clone());
        Some(move || {
            let (forget, latest) = {
                let mut q = queued.lock().unwrap_or_else(|e| e.into_inner());
                q.scheduled = false;
                (std::mem::take(&mut q.forget), q.latest.take())
            };
            let mut writer = writer.lock().unwrap_or_else(|e| e.into_inner());
            let forgot = if forget { writer.forget() } else { Ok(()) };
            let saved = match latest {
                Some(Ask::Observe {
                    enabled,
                    snapshot,
                    now,
                }) => writer.observe(enabled, snapshot, now),
                Some(Ask::Finish { enabled, snapshot }) => writer.finish(enabled, snapshot),
                Some(Ask::Forget) | None => Ok(()),
            };
            forgot.and(saved)
        })
    }
}

pub fn can_persist(restore: bool, recording: bool, on_disk: bool) -> bool {
    restore && recording && on_disk
}

/// Explicit launch searches and explicit extra windows take precedence.
pub fn should_restore(enabled: bool, has_search: bool, extra_window: bool) -> bool {
    enabled && !has_search && !extra_window
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::tests::Scratch;
    use everygrep_core::query::{HiddenFiles, QueryOptions};
    fn sample(n: usize) -> Session {
        let options = QueryOptions {
            regex: true,
            case_sensitive: true,
            whole_words: true,
            match_path: true,
            match_diacritics: true,
            prefix: true,
            suffix: true,
            ignore_punctuation: true,
            ignore_whitespace: true,
            and_before_or: true,
            literal_operators: true,
            wildcards_anywhere: true,
            hidden_files: HiddenFiles::Only,
            ..Default::default()
        };
        Session {
            scope: "local-index".into(),
            active: n.saturating_sub(1),
            closed: Vec::new(),
            open: (0..n)
                .map(|i| SearchState {
                    query: format!(" query {i}\\\t\nşəkil "),
                    options: tab_options(options),
                    filter: Some(NamedFilter {
                        id: 12,
                        name: "Şəkillər\tResimler".into(),
                        search: "ext:png;jpg".into(),
                        options: QueryOptions::default(),
                    }),
                    filter_snapshot: i % 2 == 0,
                    sort_column: 7,
                    descending: true,
                    columns: Columns {
                        show_accessed: true,
                        show_attrs: true,
                        name: Some(350.5),
                        size: Some(96.0),
                        ..Default::default()
                    },
                })
                .collect(),
        }
    }
    #[test]
    fn session_round_trip_restores_searches_options_filters_and_layout_only() {
        let dir = Scratch::new("session-roundtrip");
        let path = dir.0.join("session");
        assert!(Session::load(&path).unwrap().is_none());
        let mut s = sample(3);
        s.closed.push(s.open[0].clone());
        s.save(&path).unwrap();
        assert_eq!(Session::load(&path).unwrap(), Some(s.clone()));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(s.clone().restore("different-index").is_none());
        let tabs = s.restore("local-index").unwrap();
        assert_eq!(tabs.active, 2);
        assert_eq!(tabs.closed.len(), 1);
        assert!(tabs.open.iter().all(|t| t.result.is_none()));
        assert_eq!(
            tabs.open[1].search.options,
            tab_options(tabs.open[1].search.options)
        );
    }
    #[test]
    fn every_sort_column_and_every_column_survive_the_session() {
        let sortable: Vec<i32> = (-1..crate::columns::COLUMNS as i32 + 4)
            .filter(|c| crate::is_sort_column(*c))
            .collect();
        assert_eq!(
            sortable,
            [0, 1, 2, 3, 4, 5, 6, 7, 8, 12, 13, 14]
                .into_iter()
                .chain(everygrep_core::property::Property::ALL.map(|p| p.column()))
                .collect::<Vec<_>>()
        );
        for column in sortable {
            let mut s = sample(1);
            s.open[0].sort_column = column;
            s.open[0].descending = column % 2 == 0;
            assert_eq!(Session::parse(&s.text()).unwrap(), s, "sort {column}");
        }
        // Every optional column alone, then all with every width.
        for (name, _) in column_flags(&Columns::default()) {
            let mut s = sample(1);
            s.open[0].columns = Columns::default();
            *column_flag(&mut s.open[0].columns, name).unwrap() = true;
            assert_eq!(Session::parse(&s.text()).unwrap(), s, "{name}");
        }
        let mut s = sample(1);
        let c = &mut s.open[0].columns;
        for (name, _) in column_flags(&c.clone()) {
            *column_flag(c, name).unwrap() = true;
        }
        for (i, (name, _)) in column_widths_of(&c.clone()).into_iter().enumerate() {
            *column_width(c, name).unwrap() = Some(40.5 + i as f32);
        }
        assert!(c.show_md5 && c.show_size_freq && c.folder == Some(41.5));
        let text = s.text();
        assert_eq!(Session::parse(&text).unwrap(), s);
        // Columns a later version adds are skipped, not refused.
        let future = text
            .replace("columns = ", "columns = future ")
            .replace("widths = ", "widths = future:12 ");
        assert_eq!(Session::parse(&future).unwrap(), s);
    }
    #[test]
    fn legacy_tab_policies_are_ignored_and_new_files_omit_them() {
        let legacy = "EveryGrep session 1\nscope = test\ntab = open\noptions = all\t4095\tonly\n";
        let s = Session::parse(legacy).unwrap();
        assert!(s.open[0].options.case_sensitive && s.open[0].options.regex);
        assert_eq!(s.open[0].options, tab_options(s.open[0].options));
        let text = s.text();
        assert!(text.contains("match-options = all\t511\n"));
        assert!(!text.lines().any(|line| line.starts_with("options =")));
        assert_eq!(Session::parse(&text).unwrap(), s);
    }

    #[test]
    fn unicode_edge_spaces_survive_in_queries_and_filters() {
        let mut s = sample(1);
        s.open[0].query = "　foo ".into();
        let f = s.open[0].filter.as_mut().unwrap();
        f.name = " Docs　".into();
        f.search = "　ext:pdf ".into();
        assert_eq!(Session::parse(&s.text()).unwrap(), s);
    }

    #[test]
    fn clear_and_delete_redact_open_queries_and_never_resave_closed_queries() {
        let dir = Scratch::new("session-privacy");
        let path = dir.0.join("session");
        let mut tabs = sample(3).restore("local-index").unwrap();
        let private = tabs.open[0].search.query.clone();
        tabs.close(&[0]);
        let mut writer = Writer::new(Some(path.clone()));
        writer
            .observe(true, Session::capture("local-index", &tabs), Duration::ZERO)
            .unwrap();
        tabs.remove_history(&private);
        writer.forget().unwrap();
        assert!(!path.exists());
        writer
            .observe(true, Session::capture("local-index", &tabs), Duration::ZERO)
            .unwrap();
        assert!(
            !std::fs::read_to_string(&path)
                .unwrap()
                .contains(&settings::escape(private.as_bytes()))
        );
        tabs.open[0].search.query = private.clone();
        writer
            .finish(true, Session::capture("local-index", &tabs))
            .unwrap();
        assert!(
            Session::load(&path).unwrap().unwrap().open[0]
                .query
                .is_empty()
        );
        let shown = tabs.open[1].search.query.clone();
        tabs.history_policy(200, true);
        writer.forget().unwrap();
        writer
            .finish(true, Session::capture("local-index", &tabs))
            .unwrap();
        let restored = Session::load(&path).unwrap().unwrap();
        assert!(restored.closed.is_empty());
        assert!(restored.open.iter().all(|s| s.query.is_empty()));
        assert_eq!(tabs.open[1].search.query, shown); // Visible work remains usable.
    }

    #[test]
    fn privacy_policy_never_saves_without_recording_and_disk_permission() {
        let dir = Scratch::new("session-policy");
        let path = dir.0.join("session");
        let mut writer = Writer::new(Some(path.clone()));
        for restore in [false, true] {
            for recording in [false, true] {
                for disk in [false, true] {
                    let enabled = can_persist(restore, recording, disk);
                    assert_eq!(enabled, restore && recording && disk);
                    writer.observe(enabled, sample(1), Duration::ZERO).unwrap();
                    writer.finish(enabled, sample(2)).unwrap();
                    assert_eq!(path.exists(), enabled);
                }
            }
        }
        // Disabling also deletes invalid/newer files without parsing or backups.
        std::fs::write(&path, "EveryGrep session 99\nprivate").unwrap();
        writer.observe(false, sample(1), Duration::ZERO).unwrap();
        assert!(!path.exists());
        assert!(writer.latest.is_none());
    }

    #[test]
    fn last_window_replaces_the_whole_session_under_the_lock() {
        let dir = Scratch::new("session-last-window");
        let path = dir.0.join("session");
        sample(3).save(&path).unwrap();
        let last = sample(2);
        last.save(&path).unwrap();
        assert_eq!(Session::load(&path).unwrap(), Some(last));
        let files: Vec<_> = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(files.len(), 2); // Session plus its stable sibling lock.
    }
    #[test]
    fn concurrent_windows_write_complete_snapshots() {
        let dir = Scratch::new("session-concurrent");
        let path = dir.0.join("session");
        let handles: Vec<_> = (1..=4)
            .map(|n| {
                let path = path.clone();
                std::thread::spawn(move || sample(n).save(&path).unwrap())
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let loaded = Session::load(&path).unwrap().unwrap();
        assert_eq!(loaded, sample(loaded.open.len()));
    }
    #[test]
    fn unknown_keys_are_skipped_duplicates_use_last_and_active_is_clamped() {
        let text = sample(1).text() + "future-field = hello\nquery = last\nactive = 999\n";
        let session = Session::parse(&text).unwrap();
        assert_eq!(session.open[0].query, "last");
        assert_eq!(session.active, 0);
    }
    #[test]
    fn closed_tabs_are_pruned_to_ten_on_restore() {
        let mut s = sample(1);
        s.closed = sample(15).open;
        let loaded = Session::parse(&s.text()).unwrap();
        assert_eq!(loaded.closed, s.closed[5..]);
    }
    #[test]
    fn disabling_restore_recovers_from_a_malformed_or_newer_file() {
        let dir = Scratch::new("session-reset");
        let path = dir.0.join("session");
        for bad in [
            "EveryGrep session 99\n",
            "EveryGrep session 1\nscope = x\ntab = open\nquery = \\q\n",
        ] {
            std::fs::write(&path, bad).unwrap();
            let mut writer = Writer::new(Some(path.clone()));
            assert!(writer.observe(true, sample(1), Duration::ZERO).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), bad);
            writer.observe(false, sample(1), Duration::ZERO).unwrap();
            assert!(!path.exists());
            writer.observe(true, sample(2), Duration::ZERO).unwrap();
            assert_eq!(Session::load(&path).unwrap(), Some(sample(2)));
        }
    }

    #[test]
    fn malformed_or_newer_files_are_preserved() {
        let dir = Scratch::new("session-malformed");
        let path = dir.0.join("session");
        for text in [
            "EveryGrep session 2\n",
            "EveryGrep session 1\n",
            "EveryGrep session 1\nscope = a\ntab = open\noptions = invalid\n",
            "EveryGrep session 1\nscope = a\ntab = open\nsort = 9\t1\n",
            "EveryGrep session 1\nscope = a\ntab = open\nwidths = name:NaN\n",
            "EveryGrep session 1\nscope = a\ntab = open\nwidths = name\n",
            "EveryGrep session 1\nscope = a\ntab = open\nsort = 11\t0\n",
            "EveryGrep session 1\nscope = a\ntab = open\nquery = \\q\n",
        ] {
            std::fs::write(&path, text).unwrap();
            assert!(Session::load(&path).is_err());
            assert!(sample(1).save(&path).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        }
    }
    #[test]
    fn oversized_input_and_excessive_tab_counts_are_refused() {
        let dir = Scratch::new("session-limits");
        let path = dir.0.join("session");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_BYTES + 1).unwrap();
        assert!(Session::load(&path).is_err());
        assert!(Session::parse(&sample(MAX_TABS + 1).text()).is_err());
    }
    #[test]
    fn disabled_restore_never_writes_and_explicit_launches_win() {
        let dir = Scratch::new("session-disabled");
        let path = dir.0.join("session");
        let mut writer = Writer::new(Some(path.clone()));
        writer.observe(false, sample(1), Duration::ZERO).unwrap();
        writer.flush().unwrap();
        assert!(!path.exists());
        for enabled in [false, true] {
            for search in [false, true] {
                for extra in [false, true] {
                    assert_eq!(
                        should_restore(enabled, search, extra),
                        enabled && !search && !extra
                    );
                }
            }
        }
    }
    #[test]
    fn autosave_leads_and_trails_without_restarting_for_keystrokes() {
        let mut window = SaveWindow::default();
        assert!(window.changed(Duration::ZERO));
        for ms in 1..15_000 {
            assert!(!window.changed(Duration::from_millis(ms)));
            assert!(!window.due(Duration::from_millis(ms)));
        }
        assert!(window.due(SAVE_DELAY));
        assert!(!window.due(SAVE_DELAY));
        assert!(window.changed(Duration::from_secs(16)));
        assert!(!window.due(Duration::from_secs(31))); // No trailing write if unchanged.
    }
    #[test]
    fn saves_run_later_on_the_saving_thread_newest_first_and_keep_deletions() {
        let dir = Scratch::new("session-saver");
        let path = dir.0.join("session");
        let saver = Saver::new(Some(path.clone()));
        let observe = |n: usize, now: u64| Ask::Observe {
            enabled: true,
            snapshot: sample(n),
            now: Duration::from_secs(now),
        };
        // Nothing is written by asking; a newer snapshot replaces a waiting one.
        let job = saver.ask(observe(1, 0)).expect("a job");
        assert!(
            saver.ask(observe(2, 0)).is_none(),
            "taken by the waiting job"
        );
        assert!(!path.exists());
        let writer = settings::Writer::start();
        let (done, results) = std::sync::mpsc::channel();
        let caller = std::thread::current().id();
        let send = done.clone();
        writer.run(move || {
            assert_ne!(std::thread::current().id(), caller);
            send.send(job()).unwrap();
        });
        results
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        assert_eq!(Session::load(&path).unwrap(), Some(sample(2)));
        // A deletion asked after a snapshot drops it and still deletes.
        let job = saver.ask(observe(3, 1)).expect("a job");
        assert!(saver.ask(Ask::Forget).is_none());
        job().unwrap();
        assert!(!path.exists());
        // A snapshot asked after a deletion is saved after it.
        let job = saver.ask(Ask::Forget).expect("a job");
        assert!(saver.ask(observe(4, 2)).is_none());
        job().unwrap();
        assert_eq!(Session::load(&path).unwrap(), Some(sample(4)));
        // The last save of the window, and its error, come back from the job.
        std::fs::write(&path, "EveryGrep session 99\n").unwrap();
        let job = saver
            .ask(Ask::Finish {
                enabled: true,
                snapshot: sample(5),
            })
            .expect("a job");
        assert!(job().is_err(), "a newer file is kept");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "EveryGrep session 99\n"
        );
        assert!(writer.wait_done(Duration::from_secs(5)));
    }

    #[test]
    fn writer_coalesces_changes_and_flushes_latest_at_exit() {
        let dir = Scratch::new("session-writer");
        let path = dir.0.join("session");
        let mut writer = Writer::new(Some(path.clone()));
        writer.observe(true, sample(1), Duration::ZERO).unwrap();
        writer
            .observe(true, sample(2), Duration::from_secs(1))
            .unwrap();
        assert_eq!(Session::load(&path).unwrap(), Some(sample(1)));
        writer
            .observe(true, sample(3), Duration::from_secs(14))
            .unwrap();
        writer.observe(true, sample(3), SAVE_DELAY).unwrap();
        assert_eq!(Session::load(&path).unwrap(), Some(sample(3)));
        writer
            .observe(true, sample(4), Duration::from_secs(16))
            .unwrap();
        writer
            .observe(true, sample(5), Duration::from_secs(17))
            .unwrap();
        writer.flush().unwrap();
        assert_eq!(Session::load(&path).unwrap(), Some(sample(5)));
        writer
            .observe(false, sample(6), Duration::from_secs(18))
            .unwrap();
        writer.flush().unwrap();
        assert!(!path.exists());
    }
    #[test]
    fn finishing_a_window_saves_its_latest_tabs_once_more() {
        let dir = Scratch::new("session-finish");
        let path = dir.0.join("session");
        let mut first = Writer::new(Some(path.clone()));
        let mut second = Writer::new(Some(path.clone()));
        first.observe(true, sample(1), Duration::ZERO).unwrap();
        second.observe(true, sample(2), Duration::ZERO).unwrap();
        first.finish(true, sample(3)).unwrap();
        assert_eq!(Session::load(&path).unwrap(), Some(sample(3)));
        second.finish(true, sample(4)).unwrap();
        assert_eq!(Session::load(&path).unwrap(), Some(sample(4)));
        first.finish(false, sample(5)).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn measure_session_with_thirty_tabs() {
        let dir = Scratch::new("session-thirty");
        let path = dir.0.join("session");
        let session = sample(30);
        let bytes = session.text().len();
        let start = std::time::Instant::now();
        session.save(&path).unwrap();
        eprintln!(
            "30 tabs: session {bytes} bytes, atomic locked write {:?} (debug build, temp directory)",
            start.elapsed()
        );
        assert_eq!(Session::load(&path).unwrap(), Some(session));
        assert_eq!(std::fs::metadata(path).unwrap().len(), bytes as u64);
    }
}
