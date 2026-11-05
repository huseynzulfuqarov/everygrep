//! Renaming several items at once, with a preview.
//!
//! The dialog shows each item's old and new name before anything changes
//! (see [`plan`]). A batch is applied only when no row has a problem: a
//! name that is not allowed, two items that would get the same name, or a
//! name that is taken in the folder (also by another item of the batch:
//! chains and swaps are refused, so every rename is a single step that
//! never replaces anything). Rows whose name does not change are left
//! out. The renames are one undo step.

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use everygrep_core::bulkname::{DateFormat, Item, Mode, Rule, RuleError};
use everygrep_core::localtime::Zone;
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::i18n::{fill, tr, trn};
use crate::{AppWindow, BulkRow, actions, format};

/// Items one batch may hold; more are refused.
pub const MAX_ITEMS: usize = 10_000;

/// Why a row is not renamed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    /// The new name is the old one: left out, no problem.
    Unchanged,
    /// The old name is not valid text, so no rule can be applied safely:
    /// left out, no problem.
    NotText,
    /// Blocks the batch.
    Blocking(String),
}

#[derive(Clone, Debug)]
pub struct Row {
    pub from: PathBuf,
    /// The old name as shown.
    pub old: String,
    pub new: String,
    pub problem: Option<Problem>,
}

#[derive(Debug, Default)]
pub struct Plan {
    pub rows: Vec<Row>,
    pub ready: usize,
    pub unchanged: usize,
    pub blocking: usize,
}

impl Plan {
    pub fn can_apply(&self) -> bool {
        self.ready > 0 && self.blocking == 0
    }

    /// The (path, new name) pairs to rename.
    pub fn renames(&self) -> Vec<(PathBuf, String)> {
        self.rows
            .iter()
            .filter(|r| r.problem.is_none())
            .map(|r| (r.from.clone(), r.new.clone()))
            .collect()
    }
}

/// The new names of `paths` under `rule`, checked against each other and
/// against the disk as it is now.
pub fn plan(paths: &[PathBuf], rule: &Rule, zone: Zone) -> Result<Plan, RuleError> {
    let rule = rule.compile(zone)?;
    let mut rows = Vec::with_capacity(paths.len());
    for (position, from) in paths.iter().enumerate() {
        let name = from.file_name().unwrap_or(from.as_os_str());
        let old = format::visible_name(&name.to_string_lossy()).into_owned();
        let row = |new: String, problem| Row {
            from: from.clone(),
            old: old.clone(),
            new,
            problem,
        };
        let Some(text) = name.to_str() else {
            rows.push(row(old.clone(), Some(Problem::NotText)));
            continue;
        };
        let meta = match std::fs::symlink_metadata(from) {
            Ok(m) => m,
            Err(e) => {
                rows.push(row(old.clone(), Some(Problem::Blocking(e.to_string()))));
                continue;
            }
        };
        let new = rule.apply(Item {
            name: text,
            is_dir: meta.is_dir(),
            modified: meta.mtime(),
            position: position as u64,
        });
        let problem = if new == text {
            Some(Problem::Unchanged)
        } else if let Err(e) = actions::validate_name(&new) {
            Some(Problem::Blocking(e.to_string()))
        } else {
            let target = from.with_file_name(&new);
            (std::fs::symlink_metadata(&target).is_ok() && !actions::case_only(from, &target))
                .then(|| Problem::Blocking(actions::RenameError::Exists(new.clone()).to_string()))
        };
        rows.push(row(new, problem));
    }
    // Two items of one folder that would get the same name; in a folder
    // that does not tell case apart, also names that differ only in case.
    let mut sensitive: HashMap<PathBuf, bool> = HashMap::new();
    for row in rows.iter().filter(|r| r.problem.is_none()) {
        let folder = row.from.parent().map(Path::to_owned).unwrap_or_default();
        if let std::collections::hash_map::Entry::Vacant(entry) = sensitive.entry(folder) {
            let answer = rows
                .iter()
                .filter(|r| r.problem.is_none() && r.from.parent() == Some(entry.key().as_path()))
                .find_map(|r| case_sensitive(&r.from))
                .unwrap_or(false);
            entry.insert(answer);
        }
    }
    let key = |row: &Row| {
        let folder = row.from.parent().map(Path::to_owned).unwrap_or_default();
        let name = if sensitive[&folder] {
            row.new.clone()
        } else {
            row.new.to_lowercase()
        };
        (folder, name)
    };
    let mut seen: HashMap<(PathBuf, String), Vec<String>> = HashMap::new();
    for row in rows.iter().filter(|r| r.problem.is_none()) {
        seen.entry(key(row)).or_default().push(row.new.clone());
    }
    for row in rows.iter_mut().filter(|r| r.problem.is_none()) {
        let same = &seen[&key(row)];
        if same.len() > 1 {
            row.problem = Some(Problem::Blocking(if same.iter().all(|n| *n == row.new) {
                tr("Another item of the list gets this name.").into()
            } else {
                tr("Another item of the list gets this name in other letter case, and this folder does not tell upper and lower case apart.").into()
            }));
        }
    }
    let mut out = Plan {
        rows,
        ..Plan::default()
    };
    for row in &out.rows {
        match row.problem {
            None => out.ready += 1,
            Some(Problem::Blocking(_)) => out.blocking += 1,
            Some(_) => out.unchanged += 1,
        }
    }
    Ok(out)
}

/// Whether the folder of the existing item `sample` tells names apart by
/// case: the item is looked up under its name with the case of every
/// letter swapped. `None` when that tells nothing (no letters with case,
/// or the item cannot be read).
fn case_sensitive(sample: &Path) -> Option<bool> {
    let name = sample.file_name()?.to_str()?;
    let swapped: String = name
        .chars()
        .flat_map(|c| -> Vec<char> {
            if c.is_uppercase() {
                c.to_lowercase().collect()
            } else {
                c.to_uppercase().collect()
            }
        })
        .collect();
    if swapped == name {
        return None;
    }
    let other = sample.with_file_name(&swapped);
    let own = std::fs::symlink_metadata(sample).ok()?;
    match std::fs::symlink_metadata(&other) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(true),
        Err(_) => None,
        // Found under the other spelling: the item itself, unless the
        // folder lists that spelling as an entry of its own.
        Ok(m) => Some(m.dev() != own.dev() || m.ino() != own.ino() || actions::has_entry(&other)),
    }
}

fn rule_of(w: &AppWindow) -> Rule {
    let mode = match w.get_bulk_mode() {
        1 => Mode::Suffix,
        2 => Mode::Replace,
        3 => Mode::Regex,
        4 => Mode::Date,
        5 => Mode::Number,
        _ => Mode::Prefix,
    };
    let date_format = match w.get_bulk_date_format() {
        1 => DateFormat::Compact,
        2 => DateFormat::IsoTime,
        _ => DateFormat::Iso,
    };
    Rule {
        mode,
        text: w.get_bulk_text().into(),
        with: w.get_bulk_with().into(),
        match_case: w.get_bulk_match_case(),
        keep_extension: w.get_bulk_keep_extension(),
        at_end: w.get_bulk_at_end(),
        date_format,
        start: u64::try_from(w.get_bulk_start()).unwrap_or(0),
        step: u64::try_from(w.get_bulk_step()).unwrap_or(1),
        digits: usize::try_from(w.get_bulk_digits())
            .unwrap_or(1)
            .clamp(1, 12),
    }
}

fn rule_error(e: &RuleError) -> String {
    match e {
        RuleError::NothingToFind => tr("Type the text to find.").into(),
        RuleError::BadPattern(why) => fill(tr("The regular expression is not valid: {}"), &[why]),
    }
}

/// Shows `plan` (or why there is none) in the dialog.
fn show(w: &AppWindow, plan: Result<&Plan, String>) {
    let rows = w.get_bulk_rows();
    let rows = rows
        .as_any()
        .downcast_ref::<VecModel<BulkRow>>()
        .expect("bulk rows model");
    match plan {
        Err(why) => {
            rows.set_vec(Vec::new());
            w.set_bulk_summary(why.into());
            w.set_bulk_can_apply(false);
        }
        Ok(plan) => {
            rows.set_vec(
                plan.rows
                    .iter()
                    .map(|r| {
                        let (state, note) = match &r.problem {
                            None => (0, String::new()),
                            Some(Problem::Unchanged) => (1, tr("No change").to_owned()),
                            Some(Problem::NotText) => (
                                1,
                                tr("The name is not valid text; it is left as it is.").to_owned(),
                            ),
                            Some(Problem::Blocking(why)) => (2, why.clone()),
                        };
                        BulkRow {
                            old: r.old.as_str().into(),
                            new: format::visible_name(&r.new).as_ref().into(),
                            note: note.into(),
                            state,
                        }
                    })
                    .collect::<Vec<_>>(),
            );
            let mut parts = vec![fill(
                trn(
                    "{} item will be renamed.",
                    "{} items will be renamed.",
                    plan.ready,
                ),
                &[&plan.ready],
            )];
            if plan.unchanged > 0 {
                parts.push(fill(
                    trn("{} stays as it is.", "{} stay as they are.", plan.unchanged),
                    &[&plan.unchanged],
                ));
            }
            if plan.blocking > 0 {
                parts.push(fill(
                    trn(
                        "{} has a problem: nothing is renamed until it is solved.",
                        "{} have problems: nothing is renamed until they are solved.",
                        plan.blocking,
                    ),
                    &[&plan.blocking],
                ));
            }
            w.set_bulk_summary(parts.join(" ").into());
            w.set_bulk_can_apply(plan.can_apply());
        }
    }
}

#[derive(Default)]
pub struct Dialog {
    paths: RefCell<Vec<PathBuf>>,
}

type BulkDone = Box<dyn FnOnce(&AppWindow, Result<crate::fileops_undo::BatchOutcome, String>)>;
thread_local! { static DONE: RefCell<Option<BulkDone>> = RefCell::new(None); }

impl Dialog {
    pub fn connect(w: &AppWindow) -> Rc<Self> {
        let dialog = Rc::new(Self::default());
        w.set_bulk_rows(ModelRc::new(VecModel::<BulkRow>::default()));
        w.on_bulk_changed({
            let (dialog, weak) = (dialog.clone(), w.as_weak());
            move || {
                if let Some(w) = weak.upgrade() {
                    dialog.refresh(&w);
                }
            }
        });
        w.on_bulk_apply({
            let (dialog, weak) = (dialog.clone(), w.as_weak());
            move || {
                let Some(w) = weak.upgrade() else { return };
                if w.get_bulk_busy() {
                    return;
                }
                // Checked again against the disk as it is now.
                let Some(plan) = dialog.refresh(&w) else {
                    return;
                };
                if !plan.can_apply() {
                    return;
                }
                let renames = plan.renames();
                w.set_bulk_busy(true);
                let (weak, dialog) = (w.as_weak(), dialog.clone());
                DONE.with(|d| {
                    *d.borrow_mut() = Some(Box::new(move |w, result| {
                        w.set_bulk_busy(false);
                        crate::fileops_undo_ui::refresh_open();
                        match result {
                            Err(e) => w.set_bulk_summary(e.into()),
                            Ok(out) => {
                                dialog.close(w);
                                report(w, &out);
                            }
                        }
                    }))
                });
                std::thread::spawn(move || {
                    let result = crate::fileops_undo::catch_worker(|| {
                        crate::fileops_undo::rename_batch(&renames)
                    })
                    .unwrap_or_else(Err);
                    let _ = weak.upgrade_in_event_loop(move |w| {
                        DONE.with(|d| {
                            if let Some(done) = d.borrow_mut().take() {
                                done(&w, result);
                            }
                        })
                    });
                });
            }
        });
        w.on_bulk_cancel({
            let (dialog, weak) = (dialog.clone(), w.as_weak());
            move || {
                if let Some(w) = weak.upgrade() {
                    dialog.close(&w);
                }
            }
        });
        dialog
    }

    /// Opens the dialog for `paths`.
    pub fn start(&self, w: &AppWindow, paths: Vec<PathBuf>) {
        if paths.len() > MAX_ITEMS {
            crate::info(
                w,
                fill(
                    tr("Select at most {} items to rename them together."),
                    &[&format::count(MAX_ITEMS)],
                ),
            );
            return;
        }
        *self.paths.borrow_mut() = paths;
        w.set_bulk_open(true);
        self.refresh(w);
    }

    fn refresh(&self, w: &AppWindow) -> Option<Plan> {
        let paths = self.paths.borrow();
        match plan(&paths, &rule_of(w), Zone::local()) {
            Ok(plan) => {
                show(w, Ok(&plan));
                Some(plan)
            }
            Err(e) => {
                show(w, Err(rule_error(&e)));
                None
            }
        }
    }

    fn close(&self, w: &AppWindow) {
        if w.get_bulk_busy() {
            return;
        }
        self.paths.borrow_mut().clear();
        w.set_bulk_open(false);
        w.invoke_focus_list();
    }
}

/// The status line after a batch.
fn report(w: &AppWindow, out: &crate::fileops_undo::BatchOutcome) {
    let mut text = fill(
        trn("Renamed {} item.", "Renamed {} items.", out.done.len()),
        &[&out.done.len()],
    );
    if let Some(note) = &out.note {
        text = format!("{text} {note}");
    }
    match out.failed.first() {
        None => crate::info(w, text),
        Some((path, why)) => crate::error(
            w,
            fill(
                tr("{0} Not renamed: {1}. First: {2}: {3}"),
                &[
                    &text,
                    &out.failed.len(),
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
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "everygrep-bulk-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn files(&self, names: &[&str]) -> Vec<PathBuf> {
            names
                .iter()
                .map(|n| {
                    let p = self.0.join(n);
                    fs::write(&p, n).unwrap();
                    p
                })
                .collect()
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn prefix(text: &str) -> Rule {
        Rule {
            text: text.into(),
            ..Rule::default()
        }
    }

    fn blocking(plan: &Plan) -> Vec<&str> {
        plan.rows
            .iter()
            .filter(|r| matches!(r.problem, Some(Problem::Blocking(_))))
            .map(|r| r.old.as_str())
            .collect()
    }

    #[test]
    fn a_prefix_previews_every_item_and_touches_nothing() {
        let t = Temp::new();
        let paths = t.files(&["a.txt", "b.jpg"]);
        let plan = plan(&paths, &prefix("2024 "), Zone::utc()).unwrap();
        assert!(plan.can_apply());
        assert_eq!(plan.ready, 2);
        let new: Vec<_> = plan.rows.iter().map(|r| r.new.as_str()).collect();
        assert_eq!(new, ["2024 a.txt", "2024 b.jpg"]);
        assert!(paths.iter().all(|p| p.exists()));
        assert_eq!(fs::read_dir(&t.0).unwrap().count(), 2);
    }

    #[test]
    fn names_taken_in_the_folder_or_in_the_batch_block_it() {
        let t = Temp::new();
        let paths = t.files(&["a.txt", "new a.txt", "b.txt"]);
        let taken = plan(&paths[..1], &prefix("new "), Zone::utc()).unwrap();
        assert_eq!(blocking(&taken), ["a.txt"]);
        assert!(!taken.can_apply());
        // Both would become "x.txt".
        let rule = Rule {
            mode: Mode::Regex,
            text: "^[ab]".into(),
            with: "x".into(),
            ..Rule::default()
        };
        let same = plan(&[paths[0].clone(), paths[2].clone()], &rule, Zone::utc()).unwrap();
        assert_eq!(blocking(&same), ["a.txt", "b.txt"]);
    }

    /// Removes the digits: "Q1.txt" and "q2.txt" become "Q.txt" and "q.txt".
    fn no_digits() -> Rule {
        Rule {
            mode: Mode::Regex,
            text: "[0-9]".into(),
            with: String::new(),
            match_case: true,
            ..Rule::default()
        }
    }

    #[test]
    fn names_differing_only_in_case_block_where_the_folder_ignores_case() {
        if let Some(base) = crate::mount_ns::inside() {
            let d = base.join("cf/d");
            let paths: Vec<PathBuf> = ["Q1.txt", "q2.txt", "r3.txt"]
                .iter()
                .map(|n| {
                    let p = d.join(n);
                    fs::write(&p, n).unwrap();
                    p
                })
                .collect();
            assert_eq!(case_sensitive(&paths[0]), Some(false));
            let plan = plan(&paths, &no_digits(), Zone::utc()).unwrap();
            assert_eq!(blocking(&plan), ["Q1.txt", "q2.txt"]);
            assert!(!plan.can_apply());
            return;
        }
        let t = Temp::new();
        crate::mount_ns::run(
            "fileops_bulk::tests::names_differing_only_in_case_block_where_the_folder_ignores_case",
            &t.0,
            crate::mount_ns::CASEFOLD_SETUP,
        );
    }

    #[test]
    fn names_differing_only_in_case_are_fine_where_case_counts() {
        let t = Temp::new();
        let paths = t.files(&["Q1.txt", "q2.txt"]);
        assert_eq!(case_sensitive(&paths[0]), Some(true));
        assert_eq!(case_sensitive(&t.0.join("missing")), None);
        let plan = plan(&paths, &no_digits(), Zone::utc()).unwrap();
        assert!(plan.can_apply(), "{:?}", blocking(&plan));
        // A name without letters tells nothing.
        assert_eq!(case_sensitive(&t.files(&["123"])[0]), None);
    }

    #[test]
    fn chains_are_refused_instead_of_ordered() {
        let t = Temp::new();
        let paths = t.files(&["1.txt", "2.txt"]);
        // "1.txt" would take the name "2.txt" has now.
        let chain = Rule {
            mode: Mode::Replace,
            text: "1".into(),
            with: "2".into(),
            ..Rule::default()
        };
        let plan = plan(&paths, &chain, Zone::utc()).unwrap();
        assert_eq!(blocking(&plan), ["1.txt"]);
        assert_eq!(plan.unchanged, 1);
        assert!(!plan.can_apply());
        assert_eq!(fs::read(&paths[0]).unwrap(), b"1.txt");
        // A rule that gives the same name changes nothing.
        let case = Rule {
            mode: Mode::Regex,
            text: "^1".into(),
            with: "1".into(),
            ..Rule::default()
        };
        assert_eq!(plan_unchanged(&paths[..1], &case), 1);
    }

    fn plan_unchanged(paths: &[PathBuf], rule: &Rule) -> usize {
        plan(paths, rule, Zone::utc()).unwrap().unchanged
    }

    #[test]
    fn bad_names_and_vanished_items_block_and_unchanged_rows_are_left_out() {
        let t = Temp::new();
        let paths = t.files(&["a", "b.txt"]);
        let slash = Rule {
            mode: Mode::Replace,
            text: "a".into(),
            with: "x/y".into(),
            ..Rule::default()
        };
        let plan1 = plan(&paths, &slash, Zone::utc()).unwrap();
        assert_eq!(blocking(&plan1), ["a"]);
        assert_eq!(plan1.unchanged, 1);
        for with in ["", ".", ".."] {
            let rule = Rule {
                mode: Mode::Regex,
                text: "^a$".into(),
                with: with.into(),
                keep_extension: false,
                ..Rule::default()
            };
            let plan = plan(&paths[..1], &rule, Zone::utc()).unwrap();
            assert_eq!(blocking(&plan), ["a"], "{with:?}");
        }
        let long = prefix(&"x".repeat(255));
        assert_eq!(
            blocking(&plan(&paths[..1], &long, Zone::utc()).unwrap()),
            ["a"]
        );
        fs::remove_file(&paths[1]).unwrap();
        let gone = plan(&paths, &prefix("z"), Zone::utc()).unwrap();
        assert_eq!(blocking(&gone), ["b.txt"]);
        assert!(!gone.can_apply());
        let nothing = Rule {
            mode: Mode::Replace,
            text: "zzz".into(),
            ..Rule::default()
        };
        let plan = plan(&paths[..1], &nothing, Zone::utc()).unwrap();
        assert_eq!((plan.ready, plan.unchanged), (0, 1));
        assert!(!plan.can_apply());
    }

    #[test]
    fn names_that_are_not_text_are_left_as_they_are() {
        use std::os::unix::ffi::OsStrExt;
        let t = Temp::new();
        let odd = t.0.join(std::ffi::OsStr::from_bytes(b"caf\xe9.txt"));
        fs::write(&odd, "x").unwrap();
        let plain = t.files(&["b.txt"]);
        let plan = plan(&[odd.clone(), plain[0].clone()], &prefix("n "), Zone::utc()).unwrap();
        assert_eq!(plan.rows[0].problem, Some(Problem::NotText));
        assert!(plan.can_apply());
        assert_eq!(plan.renames(), [(plain[0].clone(), "n b.txt".to_owned())]);
    }

    #[test]
    fn a_link_is_renamed_as_a_link() {
        let _test = crate::fileops_undo::tests_lock();
        let t = Temp::new();
        let target = t.files(&["target.txt"]).remove(0);
        let link = t.0.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let plan = plan(std::slice::from_ref(&link), &prefix("x "), Zone::utc()).unwrap();
        let out = crate::fileops_undo::rename_batch(&plan.renames()).unwrap();
        assert_eq!(out.done.len(), 1);
        assert_eq!(fs::read_link(t.0.join("x link")).unwrap(), target);
        assert_eq!(fs::read(&target).unwrap(), b"target.txt");
    }

    #[test]
    fn rule_errors_read_plainly() {
        assert!(!rule_error(&RuleError::NothingToFind).is_empty());
        assert!(rule_error(&RuleError::BadPattern("unclosed group".into())).contains("unclosed"));
    }
}
