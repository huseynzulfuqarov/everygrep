//! File lists in the engine: the entries of a disk saved to a list file
//! (see [`everygrep_core::filelist`]) are grafted into the index as an
//! offline root, so they are found, drawn as offline and refused for file
//! actions like a removable drive that is not connected.
//!
//! The live index always wins. A list whose folder is, holds or lies in a
//! folder to index or another root of the index (a drive, connected or
//! kept offline) is not shown ("covered"); a drive that comes online where
//! a list is shown takes its place at once (see [`Engine::drop_lists_at`]),
//! and the list comes back when the drive is gone and not kept. Lists are
//! never saved with the index; they are read from their files again after
//! a new scan, and again when their file changed.

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::SystemTime;

use everygrep_core::filelist;
use everygrep_core::index::Index;
use everygrep_core::progress::Progress;
use everygrep_core::store::NodeId;

use super::{Engine, TaskKind, overlaps, path_of, root_id};

/// A file list as the settings keep it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileListSetting {
    pub path: PathBuf,
    pub enabled: bool,
}

/// What became of a file list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ListState {
    /// Turned off in the settings.
    Off,
    /// Not looked at yet.
    #[default]
    Pending,
    /// Its entries are in the index, shown as offline.
    Shown { entries: u64 },
    /// Not shown: the index has this folder from a live source.
    Covered { by: PathBuf },
    /// The file could not be read.
    Failed(String),
}

/// A file list, as the settings window lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileListStatus {
    pub path: PathBuf,
    pub enabled: bool,
    /// The folder the list is of, once its file was read.
    pub root: Option<PathBuf>,
    pub state: ListState,
}

#[derive(Clone, Debug)]
struct Slot {
    path: PathBuf,
    enabled: bool,
    /// The list's folder, from its file.
    root: Option<PathBuf>,
    /// Its entries are in the index under `root`.
    grafted: bool,
    /// The modification time of the file when it was read.
    read_at: Option<SystemTime>,
    state: ListState,
}

/// The engine's file lists.
#[derive(Debug, Default)]
pub(super) struct Lists {
    slots: Vec<Slot>,
    /// Roots of lists taken out of the settings while shown, to remove.
    dropped: Vec<PathBuf>,
    revision: u64,
}

impl Lists {
    /// The roots shown in the index.
    fn shown(&self) -> Vec<PathBuf> {
        self.slots
            .iter()
            .filter(|s| s.grafted)
            .filter_map(|s| s.root.clone())
            .collect()
    }
}

/// The node at `path` in `index`: a root, or a folder or file below one.
pub fn node_at(index: &Index, path: &Path) -> Option<NodeId> {
    let bytes = path.as_os_str().as_bytes();
    let store = index.store();
    for root in index.roots() {
        let name = store.name(root);
        if bytes == name {
            return Some(root);
        }
        let Some(rest) = bytes
            .strip_prefix(name)
            .and_then(|r| r.strip_prefix(b"/").or(name.ends_with(b"/").then_some(r)))
        else {
            continue;
        };
        let mut at = root;
        for part in rest.split(|&b| b == b'/').filter(|p| !p.is_empty()) {
            at = index.find_child(Some(at), part)?;
        }
        return Some(at);
    }
    None
}

impl Engine {
    /// Uses these file lists from now on, in this order; the index thread
    /// reads, shows or removes them soon.
    pub fn set_file_lists(&self, wanted: &[FileListSetting]) {
        let mut lists = self.file_lists.lock().expect("lock");
        lists.revision = lists.revision.wrapping_add(1);
        let mut old = std::mem::take(&mut lists.slots);
        for w in wanted {
            if lists.slots.iter().any(|s| s.path == w.path) {
                continue;
            }
            let slot = match old.iter().position(|s| s.path == w.path) {
                Some(i) => {
                    let mut s = old.swap_remove(i);
                    s.enabled = w.enabled;
                    s
                }
                None => Slot {
                    path: w.path.clone(),
                    enabled: w.enabled,
                    root: None,
                    grafted: false,
                    read_at: None,
                    state: ListState::Pending,
                },
            };
            lists.slots.push(slot);
        }
        let gone: Vec<PathBuf> = old
            .into_iter()
            .filter(|s| s.grafted)
            .filter_map(|s| s.root)
            .collect();
        lists.dropped.extend(gone);
        drop(lists);
        self.mounts_changed.store(true, Ordering::SeqCst);
    }

    /// Reads the file lists again (their files may have changed).
    pub fn reload_file_lists(&self) {
        let mut lists = self.file_lists.lock().expect("lock");
        lists.revision = lists.revision.wrapping_add(1);
        for slot in &mut lists.slots {
            slot.read_at = None;
        }
        drop(lists);
        self.mounts_changed.store(true, Ordering::SeqCst);
    }

    /// The file lists and what became of them, in the settings' order.
    pub fn file_lists(&self) -> Vec<FileListStatus> {
        self.file_lists
            .lock()
            .expect("lock")
            .slots
            .iter()
            .map(|s| FileListStatus {
                path: s.path.clone(),
                enabled: s.enabled,
                root: s.root.clone(),
                state: s.state.clone(),
            })
            .collect()
    }

    /// The roots of file lists in `index`, which are not saved with it.
    pub(super) fn list_roots(&self, index: &Index) -> Vec<NodeId> {
        let lists = self.file_lists.lock().expect("lock");
        let shown = lists
            .shown()
            .into_iter()
            .chain(lists.dropped.iter().cloned());
        shown.filter_map(|r| root_id(index, &r)).collect()
    }

    /// A live root comes at `at` (a drive mounted there): the file lists
    /// shown in, at or above it leave the index first, so it takes their
    /// place. Called with the index locked for writing.
    pub(super) fn drop_lists_at(&self, index: &mut Index, at: &Path) {
        let mut lists = self.file_lists.lock().expect("lock");
        for slot in lists.slots.iter_mut().filter(|s| s.grafted) {
            let Some(root) = slot.root.clone() else {
                continue;
            };
            if !overlaps(std::slice::from_ref(&root), at) {
                continue;
            }
            if let Some(id) = root_id(index, &root).filter(|&id| index.is_offline(id)) {
                index.remove(id);
                index.take_removed();
            }
            slot.grafted = false;
            slot.state = ListState::Covered {
                by: at.to_path_buf(),
            };
            crate::debug_log::note!(
                "file list {} is covered by {}",
                slot.path.display(),
                at.display()
            );
        }
    }

    /// Brings the file lists in the index in line with the settings, their
    /// files and the other roots. Returns whether the index changed.
    pub(super) fn sync_file_lists(&self) -> bool {
        let mut task = None;
        self.sync_file_lists_with(|path, revision| {
            let task = task.get_or_insert_with(|| self.tasks.begin(TaskKind::Updating, true));
            filelist::load_cancellable(path, &|| {
                task.progress().is_cancelled()
                    || self.stop.load(Ordering::SeqCst)
                    || self.file_lists.lock().expect("lock").revision != revision
            })
        })
    }

    /// Prepare without locks; publish the desired roots and their ownership
    /// together, always taking the index before the settings lock.
    pub(super) fn sync_file_lists_with(
        &self,
        mut load: impl FnMut(&Path, u64) -> std::io::Result<Index>,
    ) -> bool {
        let (slots, revision) = {
            let lists = self.file_lists.lock().expect("lock");
            (lists.slots.clone(), lists.revision)
        };
        let mut prepared = Vec::new();
        for slot in &slots {
            let modified = std::fs::metadata(&slot.path)
                .and_then(|m| m.modified())
                .ok();
            let current = slot.grafted
                && modified.is_some()
                && modified == slot.read_at
                && slot
                    .root
                    .as_ref()
                    .is_some_and(|r| root_id(&self.index.read().expect("index lock"), r).is_some());
            let result = if !slot.enabled {
                Prepared::Off
            } else if current {
                Prepared::Keep(slot.root.clone().expect("shown root"))
            } else {
                match load(&slot.path, revision) {
                    Ok(mut index) => {
                        let root = index.roots()[0];
                        index.set_offline(root, true);
                        Prepared::Loaded(Box::new(index))
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => return false,
                    Err(e) => {
                        crate::debug_log::note!("file list {}: {e}", slot.path.display());
                        Prepared::Failed(e.to_string())
                    }
                }
            };
            prepared.push((result, modified));
        }
        let configured = self.roots();
        let mut index = self.index.write().expect("index lock");
        let mut lists = self.file_lists.lock().expect("lock");
        if lists.revision != revision || self.stop.load(Ordering::SeqCst) {
            self.mounts_changed.store(true, Ordering::SeqCst);
            return false;
        }
        let mut old = lists.shown();
        old.extend(lists.dropped.iter().cloned());
        let mut taken: Vec<PathBuf> = index
            .roots()
            .into_iter()
            .map(|r| path_of(index.store().name(r)))
            .filter(|r| !old.contains(r))
            .chain(configured)
            .collect();
        // Decide all winners before removing or adding any root. In
        // particular, a newly enabled earlier list displaces a later one.
        let mut keep = Vec::new();
        for (slot, (result, modified)) in lists.slots.iter_mut().zip(&mut prepared) {
            let root = match result {
                Prepared::Keep(root) => Some(root.clone()),
                Prepared::Loaded(list) => Some(path_of(list.store().name(list.roots()[0]))),
                _ => None,
            };
            slot.grafted = false;
            slot.read_at = *modified;
            slot.state = match result {
                Prepared::Off => ListState::Off,
                Prepared::Failed(why) => ListState::Failed(why.clone()),
                _ => {
                    let root = root.expect("prepared root");
                    slot.root = Some(root.clone());
                    if let Some(by) = taken
                        .iter()
                        .find(|r| overlaps(std::slice::from_ref(r), &root))
                    {
                        ListState::Covered { by: by.clone() }
                    } else {
                        taken.push(root.clone());
                        slot.grafted = true;
                        let entries = match result {
                            Prepared::Keep(_) => {
                                keep.push(root.clone());
                                match slot.state {
                                    ListState::Shown { entries } => entries,
                                    _ => 0,
                                }
                            }
                            Prepared::Loaded(list) => list.len() as u64,
                            _ => unreachable!(),
                        };
                        ListState::Shown { entries }
                    }
                }
            };
        }
        let mut changed = false;
        for root in old.iter().filter(|r| !keep.contains(r)) {
            changed |= remove_offline_root(&mut index, root);
        }
        lists.dropped.clear();
        let mut notes = Vec::new();
        for (slot, (result, _)) in lists.slots.iter().zip(&prepared) {
            if slot.grafted
                && let Prepared::Loaded(list) = result
            {
                index.graft(list);
                notes.push(format!(
                    "file list {} shows {} entries of {}",
                    slot.path.display(),
                    list.len(),
                    slot.root.as_ref().expect("shown root").display()
                ));
                changed = true;
            }
        }
        drop(lists);
        drop(index);
        for note in notes {
            crate::debug_log::write(&note);
        }
        if changed {
            self.request_search();
            self.report_again();
        }
        changed
    }

    /// Writes the list of the folder `folder` of the index to `to`, with
    /// the creation times read from disk while the folder is connected.
    /// The entries are copied under the index lock, then written without
    /// it. `replace` is only for a destination confirmed by the save dialog.
    /// Returns the number of entries.
    pub fn save_file_list(
        &self,
        folder: &Path,
        to: &Path,
        progress: &Progress,
        replace: bool,
    ) -> std::io::Result<u64> {
        check_cancel(progress)?;
        let not_indexed = || {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{} is not in the index", folder.display()),
            )
        };
        let (list, online) = {
            let index = self.index.read().expect("index lock");
            let id = node_at(&index, folder).ok_or_else(not_indexed)?;
            if !index.store().is_dir(id) {
                return Err(not_indexed());
            }
            (snapshot(&index, id, progress)?, !index.is_offline(id))
        };
        check_cancel(progress)?;
        progress.restart(Some(list.entries.len() as u64));
        let (file, tmp) = crate::settings::create_temp(to)?;
        let result = (|| {
            let mut out = std::io::BufWriter::new(file);
            filelist::write_header(&mut out, &list.root)?;
            for mut entry in list.entries {
                if progress.is_cancelled() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "cancelled",
                    ));
                }
                if online {
                    entry.created_ns = created(Path::new(std::ffi::OsStr::from_bytes(&entry.path)));
                }
                filelist::write_entry(&mut out, &entry)?;
                progress.add(1);
            }
            let file = out.into_inner().map_err(|e| e.into_error())?;
            file.sync_all()?;
            check_cancel(progress)?;
            if replace {
                std::fs::rename(&tmp, to)?;
            } else {
                crate::transfer::rename_strict(&tmp, to)?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                let count = progress.done();
                crate::debug_log::note!(
                    "file list of {} saved to {} ({count} entries)",
                    folder.display(),
                    to.display()
                );
                Ok(count)
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        }
    }
}

fn check_cancel(progress: &Progress) -> std::io::Result<()> {
    progress
        .check()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled"))
}

/// Copy entries directly, with no intermediate text or second parse. The
/// same progress covers preparation and writing; every allocation is bounded.
fn snapshot(
    index: &Index,
    dir: NodeId,
    progress: &Progress,
) -> std::io::Result<filelist::FileList> {
    let store = index.store();
    let root = store.path(dir);
    let mut list = filelist::FileList {
        root: root.as_os_str().as_bytes().to_vec(),
        entries: Vec::new(),
    };
    if everygrep_core::fold::fold(&list.root).len() > usize::from(u16::MAX) {
        return Err(std::io::Error::other("the folded root name is too long"));
    }
    let mut bytes = list.root.len() as u64 * 8 + 160;
    let mut stack = vec![(dir, root)];
    while let Some((id, path)) = stack.pop() {
        check_cancel(progress)?;
        let raw = path.as_os_str().as_bytes();
        if raw.len() > filelist::MAX_PATH
            || raw.iter().filter(|&&b| b == b'/').count() > filelist::MAX_DEPTH
        {
            return Err(std::io::Error::other(
                "the path exceeds the file list limits",
            ));
        }
        let meta = store.meta(id);
        let is_dir = store.is_dir(id);
        list.entries.push(filelist::Entry {
            path: raw.to_vec(),
            is_dir,
            size: if is_dir { 0 } else { meta.size },
            modified_ns: (meta.mtime_ns != 0).then_some(meta.mtime_ns),
            created_ns: None,
        });
        progress.add(1);
        for child in store.children(id) {
            check_cancel(progress)?;
            if list.entries.len() + stack.len() >= filelist::MAX_ENTRIES {
                return Err(std::io::Error::other("too many entries for a file list"));
            }
            let path = path.join(std::ffi::OsStr::from_bytes(store.name(child)));
            // Account for the worst-case escaped text as well as snapshots.
            bytes = bytes.saturating_add(path.as_os_str().len() as u64 * 4 + 80);
            if bytes > filelist::MAX_FILE {
                return Err(std::io::Error::other(
                    "the file list exceeds the memory budget",
                ));
            }
            stack.push((child, path));
        }
    }
    check_cancel(progress)?;
    Ok(list)
}

enum Prepared {
    Off,
    Keep(PathBuf),
    Loaded(Box<Index>),
    Failed(String),
}

/// The creation time of `path` in nanoseconds since 1970, when the file
/// system keeps one.
fn created(path: &Path) -> Option<i64> {
    let t = std::fs::symlink_metadata(path).ok()?.created().ok()?;
    let ns = match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).ok()?,
        Err(e) => -i64::try_from(e.duration().as_nanos()).ok()?,
    };
    Some(ns)
}

/// Removes the offline root named `root`; whether there was one.
fn remove_offline_root(index: &mut Index, root: &Path) -> bool {
    match root_id(index, root).filter(|&id| index.is_offline(id)) {
        Some(id) => {
            index.remove(id);
            index.take_removed();
            true
        }
        None => false,
    }
}
