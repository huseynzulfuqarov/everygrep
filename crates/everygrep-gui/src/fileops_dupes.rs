//! Cleaning up duplicates: from each group of duplicate results, one rule
//! picks the copies to keep and the other copies go to the trash.
//!
//! The preview ([`plan`]) shows what is kept, what goes to the trash and
//! how much space that frees, before anything changes. Only regular files
//! take part; links, folders and items that are gone are left as they
//! are, and a group keeps at least one copy. Just before a file goes to
//! the trash ([`crate::fileops_undo::trash_duplicates`]) it is checked
//! again: it and the kept copy must be the files of the preview (device,
//! inode, size, modification time), not the same file under two names
//! (hard links), and they must hold the same bytes, compared side by side.
//! Nothing is deleted permanently; undo restores from the trash.

use std::cell::RefCell;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::engine::Engine;
use crate::i18n::{fill, tr, trn};
use crate::results::ResultsModel;
use crate::{AppWindow, DupeRow, format, portal};

/// Which copies of a group stay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Keep {
    /// The most recently modified copy.
    Newest,
    /// The least recently modified copy.
    Oldest,
    /// Every copy inside this folder (or its subfolders).
    InFolder(PathBuf),
}

/// A copy as the preview saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Copy {
    pub path: PathBuf,
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime: (i64, i64),
}

impl Copy {
    fn read(path: &Path) -> Option<Self> {
        let m = std::fs::symlink_metadata(path).ok()?;
        m.is_file().then(|| Self {
            path: path.to_owned(),
            dev: m.dev(),
            ino: m.ino(),
            size: m.len(),
            mtime: (m.mtime(), m.mtime_nsec()),
        })
    }

    /// Whether `path` is still this copy, unchanged.
    pub fn unchanged(&self) -> bool {
        Self::read(&self.path).is_some_and(|now| now == *self)
    }
}

/// One group in the preview.
#[derive(Clone, Debug, Default)]
pub struct Group {
    pub keep: Vec<Copy>,
    pub trash: Vec<Copy>,
    /// Paths left as they are, and why.
    pub left: Vec<(PathBuf, String)>,
    /// Why nothing of the group goes to the trash.
    pub skipped: Option<String>,
}

#[derive(Debug, Default)]
pub struct Plan {
    pub groups: Vec<Group>,
    pub files: usize,
    pub bytes: u64,
}

/// The preview of cleaning up `groups` (the paths of each group of
/// duplicate results) with the rule `keep`. Reads only metadata.
pub fn plan(groups: &[Vec<PathBuf>], keep: &Keep) -> Plan {
    let mut out = Plan::default();
    for paths in groups {
        let mut group = Group::default();
        let mut files = Vec::new();
        for path in paths {
            match Copy::read(path) {
                Some(copy) => files.push(copy),
                None => group.left.push((
                    path.clone(),
                    tr("Not a regular file (a folder, a link or gone): left as it is.").into(),
                )),
            }
        }
        if files.len() < 2 {
            group.skipped = Some(tr("Fewer than two files of this group are left.").into());
        } else if files.iter().any(|f| f.size != files[0].size) {
            group.skipped = Some(tr("The files are not the same size.").into());
        } else {
            let kept: Vec<usize> = match keep {
                Keep::Newest => vec![pick(&files, |a, b| a.mtime > b.mtime)],
                Keep::Oldest => vec![pick(&files, |a, b| a.mtime < b.mtime)],
                Keep::InFolder(folder) => (0..files.len())
                    .filter(|&i| files[i].path.starts_with(folder))
                    .collect(),
            };
            if kept.is_empty() {
                group.skipped = Some(tr("No copy of this group is in the chosen folder.").into());
            }
            let kept_ids: Vec<(u64, u64)> = if kept.is_empty() {
                Vec::new()
            } else {
                kept.iter().map(|&i| (files[i].dev, files[i].ino)).collect()
            };
            let mut trashed_ids = Vec::new();
            for (i, file) in files.into_iter().enumerate() {
                let id = (file.dev, file.ino);
                if kept.is_empty() || kept.contains(&i) {
                    group.keep.push(file);
                } else if kept_ids.contains(&id) || trashed_ids.contains(&id) {
                    // A hard link: the same file; removing a name frees nothing.
                    group.left.push((
                        file.path,
                        tr("The same file as another copy (a hard link): left as it is.").into(),
                    ));
                } else {
                    trashed_ids.push(id);
                    group.trash.push(file);
                }
            }
        }
        out.files += group.trash.len();
        out.bytes += group.trash.iter().map(|t| t.size).sum::<u64>();
        out.groups.push(group);
    }
    out
}

/// The index of the copy `better` prefers; the first one on a tie.
fn pick(files: &[Copy], better: impl Fn(&Copy, &Copy) -> bool) -> usize {
    let mut best = 0;
    for (i, f) in files.iter().enumerate().skip(1) {
        if better(f, &files[best]) {
            best = i;
        }
    }
    best
}

#[derive(Default)]
struct State {
    groups: Vec<Vec<PathBuf>>,
    folder: Option<PathBuf>,
    plan: Option<Plan>,
}

thread_local! {
    /// The open dialog's groups, chosen folder and preview (window thread).
    static STATE: RefCell<State> = RefCell::default();
}

/// Makes the preview again from the dialog's choices.
fn refresh(w: &AppWindow) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let plan = keep_of(w, s.folder.as_deref()).map(|keep| plan(&s.groups, &keep));
        show(w, plan.as_ref());
        s.plan = plan;
    })
}

fn keep_of(w: &AppWindow, folder: Option<&Path>) -> Option<Keep> {
    match w.get_dupes_keep() {
        0 => Some(Keep::Newest),
        1 => Some(Keep::Oldest),
        _ => folder.map(|f| Keep::InFolder(f.to_owned())),
    }
}

fn show(w: &AppWindow, plan: Option<&Plan>) {
    let rows = w.get_dupes_rows();
    let rows = rows
        .as_any()
        .downcast_ref::<VecModel<DupeRow>>()
        .expect("dupe rows model");
    let Some(plan) = plan else {
        rows.set_vec(Vec::new());
        w.set_dupes_summary(tr("Choose the folder whose copies are kept.").into());
        w.set_dupes_can_apply(false);
        return;
    };
    let mut out = Vec::new();
    let text = |p: &Path| format::visible_name(&p.to_string_lossy()).into_owned();
    for (n, g) in plan.groups.iter().enumerate() {
        out.push(DupeRow {
            text: fill(tr("Group {}"), &[&(n + 1)]).into(),
            note: g.skipped.clone().unwrap_or_default().into(),
            state: 0,
        });
        for k in &g.keep {
            out.push(DupeRow {
                text: text(&k.path).into(),
                note: tr("Keep").into(),
                state: 1,
            });
        }
        for t in &g.trash {
            out.push(DupeRow {
                text: text(&t.path).into(),
                note: fill(tr("To the trash ({})"), &[&format::size(t.size)]).into(),
                state: 2,
            });
        }
        for (p, why) in &g.left {
            out.push(DupeRow {
                text: text(p).into(),
                note: why.as_str().into(),
                state: 3,
            });
        }
    }
    rows.set_vec(out);
    let skipped = plan.groups.iter().filter(|g| g.skipped.is_some()).count();
    let mut parts = vec![fill(
        trn(
            "{0} file goes to the trash, freeing {1}.",
            "{0} files go to the trash, freeing {1}.",
            plan.files,
        ),
        &[&plan.files, &format::size(plan.bytes)],
    )];
    if skipped > 0 {
        parts.push(fill(
            trn(
                "{} group is left as it is.",
                "{} groups are left as they are.",
                skipped,
            ),
            &[&skipped],
        ));
    }
    w.set_dupes_summary(parts.join(" ").into());
    w.set_dupes_can_apply(plan.files > 0);
}

pub fn connect(w: &AppWindow, model: &Rc<ResultsModel>, engine: &Arc<Engine>) {
    w.set_dupes_rows(ModelRc::new(VecModel::<DupeRow>::default()));
    w.on_fileops_clean_duplicates({
        let (weak, model) = (w.as_weak(), model.clone());
        move || {
            let Some(w) = weak.upgrade() else { return };
            let groups = match model.duplicate_groups() {
                Ok(g) => g,
                Err(crate::results::Offline) => {
                    crate::info(&w, crate::labels::offline_refused().into());
                    return;
                }
            };
            if groups.is_empty() {
                crate::info(
                    &w,
                    tr("Search for duplicates first, for example with duplicates:content.").into(),
                );
                return;
            }
            STATE.with(|s| {
                *s.borrow_mut() = State {
                    groups,
                    ..State::default()
                }
            });
            w.set_dupes_folder("".into());
            w.set_dupes_open(true);
            refresh(&w);
        }
    });
    w.on_dupes_changed({
        let weak = w.as_weak();
        move || {
            if let Some(w) = weak.upgrade() {
                refresh(&w);
            }
        }
    });
    w.on_dupes_choose_folder({
        let weak = w.as_weak();
        move || {
            let weak = weak.clone();
            // The chooser is the portal's; waiting for it blocks.
            std::thread::spawn(move || {
                let answer =
                    portal::choose_folder(tr("Keep the copies in this folder"), tr("Choose"));
                let _ = weak.upgrade_in_event_loop(move |w| match answer {
                    Ok(Some(folder)) => {
                        if !w.get_dupes_open() || w.get_dupes_busy() {
                            return;
                        }
                        w.set_dupes_folder(
                            format::visible_name(&folder.to_string_lossy())
                                .as_ref()
                                .into(),
                        );
                        STATE.with(|s| s.borrow_mut().folder = Some(folder));
                        refresh(&w);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        crate::error(&w, fill(tr("Cannot show the folder chooser: {}"), &[&e]))
                    }
                });
            });
        }
    });
    w.on_dupes_cancel({
        let weak = w.as_weak();
        move || {
            let Some(w) = weak.upgrade() else { return };
            if w.get_dupes_busy() {
                return;
            }
            STATE.with(|s| *s.borrow_mut() = State::default());
            w.set_dupes_open(false);
            w.invoke_focus_list();
        }
    });
    w.on_dupes_apply({
        let (weak, engine) = (w.as_weak(), engine.clone());
        move || {
            let Some(w) = weak.upgrade() else { return };
            if w.get_dupes_busy() {
                return;
            }
            let Some(plan) = STATE.with(|s| s.borrow_mut().plan.take()) else {
                return;
            };
            if plan.files == 0 {
                return;
            }
            w.set_dupes_busy(true);
            let (weak, engine) = (weak.clone(), engine.clone());
            std::thread::spawn(move || {
                crate::fileops_undo::queue_notice(weak.clone());
                let result = crate::fileops_undo::catch_worker(|| {
                    let job = engine.begin_job(crate::engine::TaskKind::ComparingFiles);
                    crate::fileops_undo::trash_duplicates(
                        &plan.groups,
                        &crate::actions::trash,
                        job.progress(),
                    )
                });
                let _ = weak.upgrade_in_event_loop(move |w| {
                    w.set_dupes_busy(false);
                    STATE.with(|s| *s.borrow_mut() = State::default());
                    w.set_dupes_open(false);
                    crate::fileops_undo_ui::refresh_open();
                    match result {
                        Err(e) => crate::error(&w, e),
                        Ok(out) => report(&w, &out),
                    }
                    w.invoke_focus_list();
                });
            });
        }
    });
}

fn report(w: &AppWindow, out: &crate::fileops_undo::DupesOutcome) {
    let mut text = fill(
        trn(
            "Moved {0} duplicate to the trash, freeing {1}.",
            "Moved {0} duplicates to the trash, freeing {1}.",
            out.trashed.len(),
        ),
        &[&out.trashed.len(), &format::size(out.bytes)],
    );
    if out.cancelled {
        text = format!("{text} {}", tr("Stopped before the end."));
    }
    if let Some(note) = &out.note {
        text = format!("{text} {note}");
    }
    match out.kept.first() {
        None => crate::info(w, text),
        Some((path, why)) => crate::error(
            w,
            fill(
                tr("{0} Left as they were: {1}. First: {2}: {3}"),
                &[
                    &text,
                    &out.kept.len(),
                    &format::visible_name(&path.to_string_lossy()),
                    why,
                ],
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use everygrep_core::progress::Progress;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime};

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "everygrep-dupes-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(dir.join("work")).unwrap();
            crate::trashbin::set_test_home(Some(dir.join("Trash")));
            Self(dir)
        }
        /// A file with `text`, modified `age` seconds before a fixed time.
        fn file(&self, name: &str, text: &str, age: u64) -> PathBuf {
            let p = self.0.join("work").join(name);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&p, text).unwrap();
            let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 - age);
            fs::File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(t)
                .unwrap();
            p
        }
        fn trashed(&self) -> usize {
            fs::read_dir(self.0.join("Trash/files")).map_or(0, |d| d.count())
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            crate::trashbin::set_test_home(None);
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn paths(g: &[Copy]) -> Vec<&Path> {
        g.iter().map(|c| c.path.as_path()).collect()
    }

    fn run(plan: &Plan) -> crate::fileops_undo::DupesOutcome {
        crate::fileops_undo::trash_duplicates(
            &plan.groups,
            &crate::trashbin::test_send,
            &Progress::new(),
        )
    }

    #[test]
    fn the_rule_picks_one_copy_to_keep_and_counts_what_is_freed() {
        let _lock = crate::fileops_undo::tests_lock();
        let t = Temp::new();
        let old = t.file("a/old.txt", "same", 300);
        let mid = t.file("b/mid.txt", "same", 200);
        let new = t.file("c/new.txt", "same", 100);
        let group = vec![vec![mid.clone(), old.clone(), new.clone()]];
        let p = plan(&group, &Keep::Newest);
        assert_eq!(paths(&p.groups[0].keep), [new.as_path()]);
        assert_eq!(paths(&p.groups[0].trash), [mid.as_path(), old.as_path()]);
        assert_eq!((p.files, p.bytes), (2, 8));
        let p = plan(&group, &Keep::Oldest);
        assert_eq!(paths(&p.groups[0].keep), [old.as_path()]);
        let p = plan(&group, &Keep::InFolder(t.0.join("work/b")));
        assert_eq!(paths(&p.groups[0].keep), [mid.as_path()]);
        assert_eq!(p.files, 2);
        // Nothing changed on disk by a preview.
        assert!(old.exists() && mid.exists() && new.exists());
    }

    #[test]
    fn groups_without_a_copy_to_keep_or_of_mixed_sizes_are_left_alone() {
        let _lock = crate::fileops_undo::tests_lock();
        let t = Temp::new();
        let a = t.file("a", "same", 0);
        let b = t.file("b", "same", 0);
        let p = plan(
            &[vec![a.clone(), b.clone()]],
            &Keep::InFolder(t.0.join("elsewhere")),
        );
        assert!(p.groups[0].skipped.is_some());
        assert!(p.groups[0].trash.is_empty());
        assert_eq!(p.groups[0].keep.len(), 2);
        let c = t.file("c", "longer", 0);
        let p = plan(&[vec![a.clone(), c]], &Keep::Newest);
        assert!(p.groups[0].skipped.is_some());
        assert_eq!(p.files, 0);
        // Every copy in the chosen folder: all stay.
        let p = plan(&[vec![a, b]], &Keep::InFolder(t.0.join("work")));
        assert_eq!(p.files, 0);
    }

    #[test]
    fn links_folders_hard_links_and_missing_items_never_go() {
        let _lock = crate::fileops_undo::tests_lock();
        let t = Temp::new();
        let a = t.file("a", "same", 100);
        let b = t.file("b", "same", 0);
        let link = t.0.join("work/link");
        std::os::unix::fs::symlink(&a, &link).unwrap();
        let hard = t.0.join("work/hard");
        fs::hard_link(&b, &hard).unwrap();
        let dir = t.0.join("work/dir");
        fs::create_dir(&dir).unwrap();
        let gone = t.0.join("work/gone");
        let p = plan(
            &[vec![
                a.clone(),
                link.clone(),
                hard.clone(),
                b.clone(),
                dir.clone(),
                gone,
            ]],
            &Keep::Newest,
        );
        let g = &p.groups[0];
        // b and its hard link are one file: the link stays with it.
        assert_eq!(paths(&g.trash), [a.as_path()]);
        let left: Vec<_> = g.left.iter().map(|(p, _)| p.as_path()).collect();
        assert!(left.contains(&link.as_path()) && left.contains(&dir.as_path()));
        assert!(g.keep.iter().any(|k| k.path == hard || k.path == b));
    }

    #[test]
    fn identical_copies_go_to_the_trash_and_undo_brings_them_back() {
        let _lock = crate::fileops_undo::tests_lock();
        crate::fileops_undo::with_undo_on(|| {
            let t = Temp::new();
            let keep = t.file("keep.jpg", "picture", 0);
            let copy1 = t.file("x/copy.jpg", "picture", 100);
            let copy2 = t.file("y/copy.jpg", "picture", 200);
            let p = plan(
                &[vec![copy1.clone(), keep.clone(), copy2.clone()]],
                &Keep::Newest,
            );
            let out = run(&p);
            assert_eq!(out.trashed, [copy1.clone(), copy2.clone()]);
            assert_eq!(out.bytes, 14);
            assert!(out.kept.is_empty());
            assert!(!copy1.exists() && !copy2.exists());
            assert_eq!(fs::read(&keep).unwrap(), b"picture");
            assert_eq!(t.trashed(), 2);
            let undone = crate::fileops_undo::step_for_tests(false);
            assert_eq!(undone, (2, 0));
            assert_eq!(fs::read(&copy1).unwrap(), b"picture");
            assert_eq!(fs::read(&copy2).unwrap(), b"picture");
            assert_eq!(t.trashed(), 0);
        });
    }

    #[test]
    fn copies_whose_bytes_differ_stay() {
        let _lock = crate::fileops_undo::tests_lock();
        let t = Temp::new();
        let keep = t.file("keep", "abcd", 0);
        let other = t.file("other", "abce", 100);
        let p = plan(&[vec![keep.clone(), other.clone()]], &Keep::Newest);
        assert_eq!(p.files, 1);
        let out = run(&p);
        assert!(out.trashed.is_empty());
        assert_eq!(out.kept.len(), 1);
        assert!(out.kept[0].1.contains("differ"));
        assert_eq!(fs::read(&other).unwrap(), b"abce");
        assert_eq!(t.trashed(), 0);
    }

    #[test]
    fn a_kept_copy_changed_or_gone_after_the_preview_stops_its_group() {
        let _lock = crate::fileops_undo::tests_lock();
        let t = Temp::new();
        let keep = t.file("keep", "data", 0);
        let copy = t.file("copy", "data", 100);
        let p = plan(&[vec![keep.clone(), copy.clone()]], &Keep::Newest);
        // Rewritten with the same bytes: another modification time.
        t.file("keep", "data", 50);
        let out = run(&p);
        assert!(out.trashed.is_empty());
        assert!(copy.exists());
        fs::remove_file(&keep).unwrap();
        let p2 = Plan {
            groups: p.groups.clone(),
            ..Plan::default()
        };
        let out = run(&p2);
        assert!(out.trashed.is_empty());
        assert_eq!(fs::read(&copy).unwrap(), b"data");
        assert_eq!(t.trashed(), 0);
    }

    #[test]
    fn a_copy_changed_or_replaced_by_a_link_after_the_preview_stays() {
        let _lock = crate::fileops_undo::tests_lock();
        let t = Temp::new();
        let keep = t.file("keep", "data", 0);
        let changed = t.file("changed", "data", 100);
        let linked = t.file("linked", "data", 100);
        let p = plan(
            &[vec![keep.clone(), changed.clone(), linked.clone()]],
            &Keep::Newest,
        );
        t.file("changed", "DATA", 100);
        fs::remove_file(&linked).unwrap();
        std::os::unix::fs::symlink(&keep, &linked).unwrap();
        let out = run(&p);
        assert!(out.trashed.is_empty());
        assert_eq!(out.kept.len(), 2);
        assert_eq!(fs::read(&changed).unwrap(), b"DATA");
        assert!(fs::symlink_metadata(&linked).unwrap().is_symlink());
        assert_eq!(fs::read(&keep).unwrap(), b"data");
    }

    #[test]
    fn a_failing_trash_leaves_the_copy_and_says_why() {
        let _lock = crate::fileops_undo::tests_lock();
        let t = Temp::new();
        let keep = t.file("keep", "data", 0);
        let copy = t.file("copy", "data", 100);
        let p = plan(&[vec![keep, copy.clone()]], &Keep::Newest);
        let out = crate::fileops_undo::trash_duplicates(
            &p.groups,
            &|_, _, _| Err("no trash here".to_owned()),
            &Progress::new(),
        );
        assert!(out.trashed.is_empty());
        assert_eq!(out.kept[0].1, "no trash here");
        assert_eq!(fs::read(&copy).unwrap(), b"data");
    }

    #[test]
    fn a_cancel_stops_before_the_next_copy() {
        let _lock = crate::fileops_undo::tests_lock();
        let t = Temp::new();
        let keep = t.file("keep", "data", 0);
        let copy = t.file("copy", "data", 100);
        let p = plan(&[vec![keep, copy.clone()]], &Keep::Newest);
        let progress = Progress::new();
        progress.cancel();
        let out = crate::fileops_undo::trash_duplicates(
            &p.groups,
            &crate::trashbin::test_send,
            &progress,
        );
        assert!(out.cancelled && out.trashed.is_empty());
        assert!(copy.exists());
    }

    #[test]
    fn a_group_never_loses_its_last_copy() {
        let _lock = crate::fileops_undo::tests_lock();
        let t = Temp::new();
        let a = t.file("a", "data", 0);
        // A preview edited to trash the only kept copy still keeps it:
        // the first kept copy is never in the trash list.
        let mut p = plan(&[vec![a.clone(), t.file("b", "data", 9)]], &Keep::Newest);
        let kept = p.groups[0].keep.remove(0);
        p.groups[0].trash.push(kept);
        let out = run(&p);
        assert!(out.trashed.is_empty());
        assert!(a.exists());
    }

    #[test]
    fn a_copy_stays_when_the_kept_copy_is_gone_just_before_the_trash() {
        let _lock = crate::fileops_undo::tests_lock();
        let t = Temp::new();
        let keep = t.file("keep", "same", 0);
        let copy = t.file("copy", "same", 100);
        let p = plan(&[vec![keep.clone(), copy.clone()]], &Keep::Newest);
        assert_eq!(p.files, 1);
        let k = keep.clone();
        crate::fileops_test::arm_path("trash-checked", &copy, move |_| fs::remove_file(k).unwrap());
        let out = run(&p);
        crate::fileops_test::fired();
        assert!(out.trashed.is_empty(), "{:?}", out.trashed);
        assert_eq!(out.kept.len(), 1);
        assert_eq!(out.kept[0].0, copy);
        assert_eq!(fs::read(&copy).unwrap(), b"same");
        assert_eq!(t.trashed(), 0);
    }

    #[test]
    fn a_copy_replaced_after_the_last_check_is_not_what_goes_to_the_trash() {
        let _lock = crate::fileops_undo::tests_lock();
        let t = Temp::new();
        let keep = t.file("keep", "same", 0);
        let copy = t.file("copy", "same", 100);
        let p = plan(&[vec![keep.clone(), copy.clone()]], &Keep::Newest);
        crate::fileops_test::arm_path("trash-checked", &copy, |path| {
            fs::write(path, "unique uncopied data").unwrap()
        });
        let out = run(&p);
        crate::fileops_test::fired();
        assert_eq!(out.trashed, std::slice::from_ref(&copy));
        assert_eq!(fs::read(&copy).unwrap(), b"unique uncopied data");
        assert_eq!(fs::read(t.0.join("Trash/files/copy")).unwrap(), b"same");
        assert_eq!(fs::read(&keep).unwrap(), b"same");
    }

    #[test]
    fn a_copy_stays_when_the_kept_copy_is_rewritten_just_before_the_trash() {
        let _lock = crate::fileops_undo::tests_lock();
        let t = Temp::new();
        let keep = t.file("keep", "same", 0);
        let copy = t.file("copy", "same", 100);
        let p = plan(&[vec![keep.clone(), copy.clone()]], &Keep::Newest);
        let k = keep.clone();
        crate::fileops_test::arm_path("trash-checked", &copy, move |_| {
            fs::write(k, "edited").unwrap()
        });
        let out = run(&p);
        crate::fileops_test::fired();
        assert!(out.trashed.is_empty(), "{:?}", out.trashed);
        assert_eq!(
            out.kept,
            [(
                copy.clone(),
                "The copy to keep changed or is gone, so this copy was kept.".to_owned()
            )]
        );
        assert_eq!(fs::read(&copy).unwrap(), b"same");
        assert_eq!(fs::read(&keep).unwrap(), b"edited");
        assert_eq!(t.trashed(), 0);
        assert_eq!(fs::read_dir(t.0.join("work")).unwrap().count(), 2);
    }

    #[test]
    fn every_later_copy_stays_once_the_kept_copy_is_gone() {
        let _lock = crate::fileops_undo::tests_lock();
        let t = Temp::new();
        let keep = t.file("keep", "same", 0);
        let first = t.file("first", "same", 100);
        let second = t.file("second", "same", 200);
        let other_keep = t.file("other/keep", "other", 0);
        let other_copy = t.file("other/copy", "other", 100);
        let p = plan(
            &[
                vec![keep.clone(), first.clone(), second.clone()],
                vec![other_keep.clone(), other_copy.clone()],
            ],
            &Keep::Newest,
        );
        assert_eq!(p.files, 3);
        let k = keep.clone();
        let doomed = p.groups[0].trash[0].path.clone();
        crate::fileops_test::arm_path("trash-checked", &doomed, move |_| {
            fs::remove_file(k).unwrap()
        });
        let out = run(&p);
        crate::fileops_test::fired();
        // The group that lost its kept copy keeps both copies; the other
        // group is cleaned up as usual.
        assert_eq!(out.trashed, std::slice::from_ref(&other_copy));
        assert_eq!(out.kept.len(), 2);
        assert_eq!(fs::read(&first).unwrap(), b"same");
        assert_eq!(fs::read(&second).unwrap(), b"same");
        assert_eq!(fs::read(&other_keep).unwrap(), b"other");
        assert!(!other_copy.exists());
        assert_eq!(t.trashed(), 1);
    }
}
