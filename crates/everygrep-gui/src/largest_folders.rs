//! A separate snapshot of indexed folder totals, without changing the search.
use crate::engine::Engine;
use crate::i18n::{fill, tr};
use crate::{AppWindow, LargestFoldersWindow};
use everygrep_core::index::Index;
use everygrep_core::progress::{Cancelled, Progress};
use everygrep_core::query::{HiddenFiles, Query, QueryOptions};
use everygrep_core::sort::{FolderSize, Sort, SortKey};
use slint::{ComponentHandle, ModelRc, StandardListViewItem, VecModel};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, TryLockError};

#[derive(Clone, Copy, Debug)]
struct Request {
    minimum: u64,
    /// Zero means all rows; otherwise only paths for the first rows are built.
    limit: usize,
    include_roots: bool,
    hidden: HiddenFiles,
    hide_offline: bool,
}

impl Request {
    fn parse(
        minimum: &str,
        unit: i32,
        limit: &str,
        include_roots: bool,
        hidden: HiddenFiles,
        hide_offline: bool,
    ) -> Result<Self, String> {
        let minimum = if minimum.trim().is_empty() {
            "0"
        } else {
            minimum.trim()
        };
        let minimum = crate::advanced_query::size(minimum, unit)
            .ok_or_else(|| tr("Enter a valid minimum size.").to_owned())?;
        let limit = limit.trim();
        if limit.is_empty() || !limit.bytes().all(|b| b.is_ascii_digit()) {
            return Err(tr("Maximum rows must be a non-negative whole number.").into());
        }
        let limit = limit
            .parse()
            .map_err(|_| tr("Maximum rows must be a non-negative whole number.").to_owned())?;
        Ok(Self {
            minimum,
            limit,
            include_roots,
            hidden,
            hide_offline,
        })
    }
}

#[derive(Debug)]
struct Row {
    path: PathBuf,
    size: u64,
    offline: bool,
}

#[derive(Debug)]
struct Snapshot {
    rows: Vec<Row>,
    total: usize,
}

fn collect(index: &Index, request: Request, progress: &Progress) -> Result<Snapshot, Cancelled> {
    let query = Query::parse(
        &format!("folder: size:>={}", request.minimum),
        QueryOptions {
            hidden_files: request.hidden,
            ..QueryOptions::default()
        },
    )
    .expect("an integer size produces a valid query");
    let found = everygrep_core::search::search_with_progress(index, &query, 1, progress)?;
    let mut ids = Vec::with_capacity(found.len());
    for id in found {
        progress.check()?;
        if (request.include_roots || index.store().parent(id).is_some())
            && (!request.hide_offline || !index.is_offline(id))
        {
            ids.push(id);
        }
    }
    let sort = Sort {
        key: SortKey::Size,
        descending: true,
        folders: FolderSize::Total,
        then: [Some((SortKey::Name, false)), None],
    };
    progress.restart(Some(ids.len() as u64));
    everygrep_core::sort::sort_with_progress(index, &mut ids, sort, 1, progress)?;
    let total = ids.len();
    let limit = if request.limit == 0 {
        total
    } else {
        request.limit.min(total)
    };
    let mut rows = Vec::with_capacity(limit);
    for id in ids.into_iter().take(limit) {
        progress.check()?;
        rows.push(Row {
            path: index.store().path(id),
            size: index.store().meta(id).size,
            offline: index.is_offline(id),
        });
    }
    progress.check()?;
    Ok(Snapshot { rows, total })
}

/// Only the current, uncancelled request may publish rows. Invalidation
/// also covers a result already queued for the UI when Cancel is clicked.
#[derive(Default)]
struct Jobs {
    generation: Cell<u64>,
    progress: RefCell<Option<Arc<Progress>>>,
}

impl Jobs {
    fn cancel(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        if let Some(progress) = self.progress.borrow_mut().take() {
            progress.cancel();
        }
    }

    fn begin(&self) -> (u64, Arc<Progress>) {
        self.cancel();
        let progress = Arc::new(Progress::new());
        *self.progress.borrow_mut() = Some(progress.clone());
        (self.generation.get(), progress)
    }

    fn finish(&self, generation: u64) -> bool {
        self.generation.get() == generation && self.progress.borrow_mut().take().is_some()
    }
}

struct State {
    main: slint::Weak<AppWindow>,
    engine: Arc<Engine>,
    window: RefCell<Option<LargestFoldersWindow>>,
    jobs: Jobs,
}

thread_local! { static STATE: RefCell<Option<Rc<State>>> = const { RefCell::new(None) }; }

fn with(f: impl FnOnce(&State)) {
    let state = STATE.with(|state| state.borrow().clone());
    if let Some(state) = state {
        f(&state);
    }
}

pub fn connect(main: &AppWindow, engine: &Arc<Engine>) {
    STATE.with(|state| {
        *state.borrow_mut() = Some(Rc::new(State {
            main: main.as_weak(),
            engine: engine.clone(),
            window: RefCell::new(None),
            jobs: Jobs::default(),
        }))
    });
    main.on_largest_folders(|| {
        with(|state| {
            if state
                .main
                .upgrade()
                .is_some_and(|main| main.get_largest_folders_visible())
            {
                state.close();
            } else {
                state.open();
            }
        })
    });
}

pub fn apply_scheme(scheme: i32, style: i32, light_style: i32, accent: crate::AccentColors) {
    with(|state| {
        if let Some(window) = state.window.borrow().as_ref() {
            window.invoke_set_color_scheme(scheme, style, light_style, accent);
        }
    });
}

impl State {
    fn open(&self) {
        let Some(main) = self.main.upgrade() else {
            return;
        };
        if self.window.borrow().is_none() {
            let window = match LargestFoldersWindow::new() {
                Ok(window) => window,
                Err(e) => {
                    crate::error(&main, e.to_string());
                    return;
                }
            };
            window.on_refresh(|| with(State::refresh));
            window.on_cancel(|| with(State::cancel));
            window.on_close_window(|| with(State::close));
            window.window().on_close_requested(|| {
                with(State::close);
                slint::CloseRequestResponse::HideWindow
            });
            *self.window.borrow_mut() = Some(window);
        }
        crate::prefs::with(|p| {
            apply_scheme(
                p.color_scheme(),
                p.dark_style(),
                p.light_style(),
                p.accent_colors(),
            )
        });
        main.set_largest_folders_visible(true);
        if let Some(window) = self.window.borrow().as_ref() {
            let _ = window.show();
            crate::wayland_parent::keep_above(window, &main);
        }
        self.refresh();
    }

    fn close(&self) {
        self.jobs.cancel();
        if let Some(window) = self.window.borrow().as_ref() {
            window.set_busy(false);
            let _ = window.hide();
        }
        if let Some(main) = self.main.upgrade() {
            main.set_largest_folders_visible(false);
        }
    }

    fn cancel(&self) {
        self.jobs.cancel();
        if let Some(window) = self.window.borrow().as_ref() {
            window.set_busy(false);
            window.set_notice_error(false);
            window.set_notice(tr("Cancelled. Previous folder results were kept.").into());
        }
    }

    fn refresh(&self) {
        let Some(main) = self.main.upgrade() else {
            return;
        };
        let window = self.window.borrow();
        let Some(window) = window.as_ref() else {
            return;
        };
        let (generation, progress) = self.jobs.begin();
        window.set_notice_error(false);
        let request = match Request::parse(
            &window.get_minimum(),
            window.get_size_unit(),
            &window.get_limit_text(),
            window.get_include_roots(),
            crate::options(&main).hidden_files,
            main.get_hide_offline(),
        ) {
            Ok(request) => request,
            Err(e) => {
                self.jobs.cancel();
                window.set_busy(false);
                window.set_notice_error(true);
                window.set_notice(e.into());
                return;
            }
        };
        window.set_busy(true);
        window.set_notice(tr("Updating folder sizes…").into());
        let weak = window.as_weak();
        let engine = self.engine.clone();
        std::thread::spawn(move || {
            // Do not wait on a writer without observing cancellation.
            let result = loop {
                if progress.is_cancelled() {
                    break Ok(None);
                }
                match engine.index.try_read() {
                    Ok(index) => break Ok(collect(&index, request, &progress).ok()),
                    Err(TryLockError::WouldBlock) => {
                        std::thread::sleep(std::time::Duration::from_millis(10))
                    }
                    Err(TryLockError::Poisoned(_)) => {
                        break Err(tr("The index is unavailable.").to_owned());
                    }
                }
            };
            let _ = weak.upgrade_in_event_loop(move |window| {
                with(|state| {
                    // A hidden window or a newer refresh owns the current rows.
                    if !state.jobs.finish(generation) {
                        return;
                    }
                    window.set_busy(false);
                    match result {
                        Ok(Some(snapshot)) => {
                            let rows: Vec<ModelRc<StandardListViewItem>> = snapshot
                                .rows
                                .iter()
                                .map(|row| {
                                    ModelRc::new(VecModel::from(vec![
                                        StandardListViewItem::from(
                                            crate::format::visible_name(
                                                &row.path.to_string_lossy(),
                                            )
                                            .as_ref(),
                                        ),
                                        StandardListViewItem::from(
                                            crate::format::size(row.size).as_str(),
                                        ),
                                        StandardListViewItem::from(if row.offline {
                                            tr("Offline")
                                        } else {
                                            ""
                                        }),
                                    ]))
                                })
                                .collect();
                            window.set_rows(ModelRc::new(VecModel::from(rows)));
                            window.set_current(-1);
                            window.set_notice(
                                fill(
                                    tr("Showing {0} of {1} folders."),
                                    &[
                                        &crate::format::count(snapshot.rows.len()),
                                        &crate::format::count(snapshot.total),
                                    ],
                                )
                                .into(),
                            );
                        }
                        Ok(None) => window
                            .set_notice(tr("Cancelled. Previous folder results were kept.").into()),
                        Err(e) => {
                            window.set_notice_error(true);
                            window.set_notice(e.into());
                        }
                    }
                })
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use everygrep_core::store::{Meta, NodeStore};

    fn request(minimum: u64, limit: usize) -> Request {
        Request {
            minimum,
            limit,
            include_roots: true,
            hidden: HiddenFiles::Show,
            hide_offline: false,
        }
    }

    #[test]
    fn largest_folders_use_recursive_index_totals_and_inclusive_thresholds() {
        let mut s = NodeStore::new();
        let root = s.add(None, b"/missing", true, Meta::default());
        let small = s.add(Some(root), b"small", true, Meta::default());
        let big = s.add(Some(root), b"big", true, Meta::default());
        s.add(Some(root), b"empty", true, Meta::default());
        s.add(
            Some(big),
            b"nested.bin",
            false,
            Meta {
                size: 2048,
                mtime_ns: 0,
            },
        );
        s.add(
            Some(small),
            b"file.bin",
            false,
            Meta {
                size: 1024,
                mtime_ns: 0,
            },
        );
        let mut index = Index::new(s);
        let snapshot = collect(&index, request(1024, 0), &Progress::new()).unwrap();
        assert_eq!(
            snapshot.rows.iter().map(|row| row.size).collect::<Vec<_>>(),
            [3072, 2048, 1024]
        );
        assert_eq!(snapshot.rows[1].path, PathBuf::from("/missing/big"));
        let snapshot = collect(&index, request(2048, 1), &Progress::new()).unwrap();
        assert_eq!(snapshot.total, 2);
        assert_eq!(snapshot.rows.len(), 1);
        assert_eq!(snapshot.rows[0].size, 3072);
        let no_roots = Request {
            include_roots: false,
            ..request(0, 0)
        };
        assert_eq!(
            collect(&index, no_roots, &Progress::new()).unwrap().rows[0].size,
            2048
        );
        index.remove(big);
        let snapshot = collect(&index, request(0, 0), &Progress::new()).unwrap();
        assert_eq!(snapshot.rows[0].size, 1024);
        assert_eq!(snapshot.total, 3);
    }

    #[test]
    fn visibility_offline_and_cancellation_follow_index_state() {
        let mut s = NodeStore::new();
        let root = s.add(None, b"/missing", true, Meta::default());
        let hidden = s.add(Some(root), b".hidden", true, Meta::default());
        s.add(Some(hidden), b"nested", true, Meta::default());
        let mut index = Index::new(s);
        let hidden = Request {
            hidden: HiddenFiles::Hide,
            ..request(0, 0)
        };
        assert_eq!(collect(&index, hidden, &Progress::new()).unwrap().total, 1);
        index.set_offline(root, true);
        assert!(
            collect(&index, request(0, 0), &Progress::new())
                .unwrap()
                .rows
                .iter()
                .all(|row| row.offline)
        );
        assert_eq!(
            collect(
                &index,
                Request {
                    hide_offline: true,
                    ..request(0, 0)
                },
                &Progress::new()
            )
            .unwrap()
            .total,
            0
        );
        let progress = Progress::new();
        progress.cancel();
        assert_eq!(
            collect(&index, request(0, 0), &progress).unwrap_err(),
            Cancelled
        );
    }

    #[test]
    fn view_bounds_allow_all_rows_and_reject_invalid_or_overflowing_numbers() {
        let parse = |minimum, unit, limit| {
            Request::parse(minimum, unit, limit, true, HiddenFiles::Show, false)
        };
        assert_eq!(parse("1.5", 1, "1000").unwrap().minimum, 1536);
        assert_eq!(parse("", 2, "0").unwrap().limit, 0);
        for (minimum, unit, limit) in [
            ("-1", 0, "0"),
            ("1e9", 0, "0"),
            ("0", 4, "0"),
            ("0", 0, "-1"),
            ("0", 0, ""),
            ("0", 0, "18446744073709551616"),
            ("18446744073709551616", 0, "1"),
        ] {
            assert!(parse(minimum, unit, limit).is_err());
        }
    }
    #[test]
    fn late_results_cannot_replace_rows_after_cancel_close_or_a_newer_refresh() {
        let jobs = Jobs::default();
        let (a, progress_a) = jobs.begin();
        let (b, progress_b) = jobs.begin();
        assert!(progress_a.is_cancelled());
        assert!(!jobs.finish(a));
        assert!(!progress_b.is_cancelled());
        // Even a completed worker waiting to deliver its result is invalidated.
        jobs.cancel();
        assert!(progress_b.is_cancelled());
        assert!(!jobs.finish(b));
        let (c, progress_c) = jobs.begin();
        assert!(!jobs.finish(b));
        assert!(!progress_c.is_cancelled());
        assert!(jobs.finish(c));
        assert!(!jobs.finish(c));
        let (d, progress_d) = jobs.begin();
        jobs.cancel();
        assert!(progress_d.is_cancelled());
        assert!(!jobs.finish(d));
    }
}
