//! Custom tab strip backed by Rust state and one shared result view.
use crate::engine::Engine;
use crate::i18n::fill;
use crate::i18n::tr;
use crate::prefs::Prefs;
use crate::results::ResultsModel;
use crate::session::{self, Ask, Saver, Session};
use crate::settings::Columns;
use crate::tabs::{SearchState, Tab, Tabs, tab_options};
use crate::{AppWindow, Command, MenuEntry, TabEntry, TabsWindow};
use slint::{ComponentHandle, Model, ModelRc, Timer, TimerMode, VecModel};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct State {
    main: slint::Weak<AppWindow>,
    engine: Arc<Engine>,
    model: Rc<ResultsModel>,
    prefs: Rc<Prefs>,
    pause: Rc<Timer>,
    tabs: RefCell<Tabs>,
    picker: RefCell<Option<TabsWindow>>,
    poll: Timer,
    last_submitted: RefCell<SearchState>,
    scope: String,
    saver: Saver,
    started: Instant,
    session_error: RefCell<Option<String>>,
}
thread_local! { static STATE: RefCell<Option<Rc<State>>> = const { RefCell::new(None) }; }
fn with<R>(f: impl FnOnce(&Rc<State>) -> R) -> Option<R> {
    let state = STATE.with(|s| s.borrow().clone());
    state.as_ref().map(f)
}

pub fn connect(
    w: &AppWindow,
    engine: &Arc<Engine>,
    model: &Rc<ResultsModel>,
    prefs: &Rc<Prefs>,
    pause: &Rc<Timer>,
    start: session::Start,
) {
    let mut tabs = Tabs::default();
    tabs.open[0].search = read(w);
    let path = crate::settings::config_dir().map(|d| d.join("session"));
    let mut restored = false;
    let mut session_error = None;
    if start.restore {
        match path.as_deref().map(Session::load).transpose() {
            Ok(Some(Some(session))) => {
                if let Some(previous) = session.restore(&start.scope) {
                    tabs = previous;
                    restored = true;
                }
            }
            Ok(_) => {}
            Err(e) => {
                session_error = Some(fill(
                    tr("Cannot restore tabs: {}"),
                    &[&session_detail(path.as_deref(), &e)],
                ))
            }
        }
    }
    tabs.history_policy(prefs.history_limit(), false);
    let state = Rc::new(State {
        main: w.as_weak(),
        engine: engine.clone(),
        model: model.clone(),
        prefs: prefs.clone(),
        pause: pause.clone(),
        tabs: RefCell::new(tabs),
        picker: RefCell::new(None),
        poll: Timer::default(),
        last_submitted: RefCell::new(read(w)),
        scope: start.scope,
        saver: Saver::new(path),
        started: Instant::now(),
        session_error: RefCell::new(None),
    });
    STATE.with(|s| *s.borrow_mut() = Some(state.clone()));
    w.on_tab_command(|command| {
        with(|s| s.command(command));
    });
    w.on_tab_selected(|i| {
        if let Ok(i) = usize::try_from(i) {
            with(|s| s.change(|t| t.select(i)));
        }
    });
    w.on_tab_closed(|i| {
        if let Ok(i) = usize::try_from(i) {
            with(|s| s.change(|t| t.close(&[i])));
        }
    });
    w.on_closed_tab_selected(|i| {
        if let Ok(i) = usize::try_from(i) {
            with(|s| {
                s.change(|t| {
                    if let Some(i) = t.closed.len().checked_sub(i + 1) {
                        t.reopen(i);
                    }
                })
            });
        }
    });
    w.on_tab_preference_changed(|i, on| crate::prefs::with(|p| p.set_tab_preference(i, on)));
    let weak = Rc::downgrade(&state);
    // Observe programmatic edits too (bookmarks, history, column layout), without
    // copying result ids or selection and without installing more search callbacks.
    state
        .poll
        .start(TimerMode::Repeated, Duration::from_millis(250), move || {
            if let Some(s) = weak.upgrade() {
                s.observe();
            }
        });
    if restored {
        state.install(w);
    }
    if let Some(error) = session_error {
        crate::error(w, error);
    }
    state.fill();
}

pub fn history_policy(max: usize, clear: bool) {
    with(|s| {
        if clear || max == 0 {
            s.pause.stop();
            if let Some(w) = s.main.upgrade() {
                s.observe_search(&w);
            }
        }
        s.tabs.borrow_mut().history_policy(max, clear);
        s.fill();
        if clear || max == 0 {
            s.forget_session();
        }
    });
}

pub fn remove_history(query: &str) {
    with(|s| {
        s.pause.stop();
        if let Some(w) = s.main.upgrade() {
            s.observe_search(&w);
        }
        s.tabs.borrow_mut().remove_history(query);
        s.fill();
        s.forget_session();
    });
}

pub fn session_policy() {
    with(|s| {
        if !s.prefs.restore_session() {
            s.forget_session();
        }
    });
}

pub fn close_picker() {
    with(|s| s.close_picker());
}

pub fn submitted_sort(column: i32, descending: bool) {
    with(|s| {
        let mut search = s.last_submitted.borrow_mut();
        search.sort_column = column;
        search.descending = descending;
    });
}

pub fn submitted(w: &AppWindow) {
    with(|s| *s.last_submitted.borrow_mut() = read(w));
}

pub fn flush() {
    with(|s| {
        let Some(w) = s.main.upgrade() else { return };
        s.observe_search(&w);
        let snapshot = Session::capture(&s.scope, &s.tabs.borrow());
        // Saved before the program ends: `main` waits for the saving thread.
        s.save(Ask::Finish {
            enabled: s.prefs.restore_session(),
            snapshot,
        });
    });
}

pub fn refresh() {
    with(|s| s.fill());
}

pub fn open_search(
    query: &str,
    options: everygrep_core::query::QueryOptions,
    keep_filter: bool,
    columns: &Columns,
) {
    with(|s| {
        let Some(w) = s.main.upgrade() else { return };
        let search = read(&w).launched(query, options, keep_filter, columns);
        let activate = s.prefs.switch_to_new_tab() && !blocked(&w);
        if activate {
            s.store(&w);
        }
        s.tabs.borrow_mut().add(
            Tab {
                search,
                ..Default::default()
            },
            activate,
        );
        if activate {
            s.install(&w);
        }
        s.fill();
    });
}

/// Opens `searches` in new tabs after the others, in their order; the first
/// is shown when new tabs are shown at once (Settings).
pub fn open_tabs(searches: Vec<SearchState>) {
    with(|s| {
        let Some(w) = s.main.upgrade() else { return };
        if searches.is_empty() {
            return;
        }
        let activate = s.prefs.switch_to_new_tab() && !blocked(&w);
        if activate {
            s.store(&w);
        }
        {
            let mut tabs = s.tabs.borrow_mut();
            let first = tabs.open.len();
            for search in searches {
                tabs.add(
                    Tab {
                        search,
                        ..Default::default()
                    },
                    false,
                );
            }
            if activate {
                tabs.select(first);
            }
        }
        if activate {
            s.install(&w);
        }
        s.fill();
    });
}

/// The search shown now, as a new tab would start from.
pub fn shown_search(w: &AppWindow) -> SearchState {
    read(w)
}

fn session_detail(path: Option<&std::path::Path>, error: &std::io::Error) -> String {
    let path = path.map(|p| p.display().to_string()).unwrap_or_default();
    format!(
        "{path}: {error}\n{}",
        tr("To reset saved tabs, turn session restore off and on.")
    )
}

fn blocked(w: &AppWindow) -> bool {
    w.get_confirm_open() || w.get_fileops_rename_open()
}

fn read(w: &AppWindow) -> SearchState {
    SearchState {
        query: w.get_query().into(),
        options: tab_options(crate::options(w)),
        filter: crate::saved_ui::selected(w),
        filter_snapshot: crate::saved_ui::using_snapshot(w),
        sort_column: w.get_sort_column(),
        descending: w.get_sort_descending(),
        columns: read_columns(w),
        within: crate::scope_ui::current(),
    }
}

/// The window's shown optional columns, by name.
pub(crate) fn shown_columns(w: &AppWindow) -> Vec<String> {
    crate::session::shown_column_names(&read_columns(w))
}

/// Shows exactly the named optional columns (a filter's or a bookmark's).
pub(crate) fn show_columns(w: &AppWindow, names: &[String]) {
    let columns = crate::session::with_shown_columns(&read_columns(w), names);
    install_columns(w, &columns);
    crate::apply_detail_columns(w);
}

/// The shown columns and their widths in the window, for a tab.
fn read_columns(w: &AppWindow) -> Columns {
    Columns {
        show_type: w.get_show_type(),
        show_ext: w.get_show_ext(),
        show_created: w.get_show_created(),
        show_accessed: w.get_show_accessed(),
        show_attrs: w.get_show_attrs(),
        show_md5: w.get_show_md5(),
        show_sha256: w.get_show_sha256(),
        show_crc32: w.get_show_crc32(),
        show_name_freq: w.get_show_name_freq(),
        show_ext_freq: w.get_show_ext_freq(),
        show_size_freq: w.get_show_size_freq(),
        show_full_path: w.get_show_full_path(),
        full_path: Some(w.get_full_path_want()),
        show_parent_path: w.get_show_parent_path(),
        parent_path: Some(w.get_parent_path_want()),
        show_stem: w.get_show_stem(),
        stem: Some(w.get_stem_want()),
        show_name_length: w.get_show_name_length(),
        name_length: Some(w.get_name_length_want()),
        show_path_length: w.get_show_path_length(),
        path_length: Some(w.get_path_length_want()),
        show_disk_size: w.get_show_disk_size(),
        disk_size: Some(w.get_disk_size_want()),
        show_total_size: w.get_show_total_size(),
        total_size: Some(w.get_total_size_want()),
        show_child_count: w.get_show_child_count(),
        child_count: Some(w.get_child_count_want()),
        show_child_files: w.get_show_child_files(),
        child_files: Some(w.get_child_files_want()),
        show_child_folders: w.get_show_child_folders(),
        child_folders: Some(w.get_child_folders_want()),
        show_desc_count: w.get_show_desc_count(),
        desc_count: Some(w.get_desc_count_want()),
        show_desc_files: w.get_show_desc_files(),
        desc_files: Some(w.get_desc_files_want()),
        show_desc_folders: w.get_show_desc_folders(),
        desc_folders: Some(w.get_desc_folders_want()),
        show_folder_depth: w.get_show_folder_depth(),
        folder_depth: Some(w.get_folder_depth_want()),
        show_hard_links: w.get_show_hard_links(),
        hard_links: Some(w.get_hard_links_want()),
        show_shortcut_target: w.get_show_shortcut_target(),
        shortcut_target: Some(w.get_shortcut_target_want()),
        show_owner: w.get_show_owner(),
        owner: Some(w.get_owner_want()),
        show_width: w.get_show_width(),
        width: Some(w.get_width_want()),
        show_height: w.get_show_height(),
        height: Some(w.get_height_want()),
        show_dimensions: w.get_show_dimensions(),
        dimensions: Some(w.get_dimensions_want()),
        show_length: w.get_show_length(),
        length: Some(w.get_length_want()),
        show_bit_rate: w.get_show_bit_rate(),
        bit_rate: Some(w.get_bit_rate_want()),
        show_title: w.get_show_title(),
        title: Some(w.get_title_want()),
        show_artist: w.get_show_artist(),
        artist: Some(w.get_artist_want()),
        show_album: w.get_show_album(),
        album: Some(w.get_album_want()),
        show_year: w.get_show_year(),
        year: Some(w.get_year_want()),
        show_track: w.get_show_track(),
        track: Some(w.get_track_want()),
        show_genre: w.get_show_genre(),
        genre: Some(w.get_genre_want()),
        show_rating: w.get_show_rating(),
        rating: Some(w.get_rating_want()),
        show_tags: w.get_show_tags(),
        tags: Some(w.get_tags_want()),
        show_page_count: w.get_show_page_count(),
        page_count: Some(w.get_page_count_want()),
        show_word_count: w.get_show_word_count(),
        word_count: Some(w.get_word_count_want()),
        show_camera_maker: w.get_show_camera_maker(),
        camera_maker: Some(w.get_camera_maker_want()),
        show_camera_model: w.get_show_camera_model(),
        camera_model: Some(w.get_camera_model_want()),
        show_date_taken: w.get_show_date_taken(),
        date_taken: Some(w.get_date_taken_want()),
        show_gps_lat: w.get_show_gps_lat(),
        gps_lat: Some(w.get_gps_lat_want()),
        show_gps_long: w.get_show_gps_long(),
        show_sha1: w.get_show_sha1(),
        show_sha384: w.get_show_sha384(),
        show_sha512: w.get_show_sha512(),
        show_xxhash: w.get_show_xxhash(),
        show_dupe_group: w.get_show_dupe_group(),
        gps_long: Some(w.get_gps_long_want()),
        sha1: Some(w.get_sha1_want()),
        sha384: Some(w.get_sha384_want()),
        sha512: Some(w.get_sha512_want()),
        xxhash: Some(w.get_xxhash_want()),
        dupe_group: Some(w.get_dupe_group_want()),
        name: w.get_name_set().then(|| w.get_name_want()),
        // Folder has a width of its own only while the columns are wider
        // than the window; 0 means it takes the room the others leave.
        folder: (w.get_folder_want() >= 0.5).then(|| w.get_folder_want()),
        size: Some(w.get_size_want()),
        date: Some(w.get_date_want()),
        type_: Some(w.get_type_want()),
        ext: Some(w.get_ext_want()),
        created: Some(w.get_created_want()),
        accessed: Some(w.get_accessed_want()),
        attrs: Some(w.get_attr_want()),
        md5: Some(w.get_md5_want()),
        sha256: Some(w.get_sha256_want()),
        crc32: Some(w.get_crc32_want()),
        name_freq: Some(w.get_name_freq_want()),
        ext_freq: Some(w.get_ext_freq_want()),
        size_freq: Some(w.get_size_freq_want()),
    }
}

/// Puts a tab's columns and widths into the window.
fn install_columns(w: &AppWindow, c: &Columns) {
    w.set_show_type(c.show_type);
    w.set_show_ext(c.show_ext);
    w.set_show_created(c.show_created);
    w.set_show_accessed(c.show_accessed);
    w.set_show_attrs(c.show_attrs);
    w.set_show_md5(c.show_md5);
    w.set_show_sha256(c.show_sha256);
    w.set_show_crc32(c.show_crc32);
    w.set_show_name_freq(c.show_name_freq);
    w.set_show_ext_freq(c.show_ext_freq);
    w.set_show_size_freq(c.show_size_freq);
    w.set_show_full_path(c.show_full_path);
    c.full_path
        .into_iter()
        .for_each(|v| w.set_full_path_want(v));
    w.set_show_parent_path(c.show_parent_path);
    c.parent_path
        .into_iter()
        .for_each(|v| w.set_parent_path_want(v));
    w.set_show_stem(c.show_stem);
    c.stem.into_iter().for_each(|v| w.set_stem_want(v));
    w.set_show_name_length(c.show_name_length);
    c.name_length
        .into_iter()
        .for_each(|v| w.set_name_length_want(v));
    w.set_show_path_length(c.show_path_length);
    c.path_length
        .into_iter()
        .for_each(|v| w.set_path_length_want(v));
    w.set_show_disk_size(c.show_disk_size);
    c.disk_size
        .into_iter()
        .for_each(|v| w.set_disk_size_want(v));
    w.set_show_total_size(c.show_total_size);
    c.total_size
        .into_iter()
        .for_each(|v| w.set_total_size_want(v));
    w.set_show_child_count(c.show_child_count);
    c.child_count
        .into_iter()
        .for_each(|v| w.set_child_count_want(v));
    w.set_show_child_files(c.show_child_files);
    c.child_files
        .into_iter()
        .for_each(|v| w.set_child_files_want(v));
    w.set_show_child_folders(c.show_child_folders);
    c.child_folders
        .into_iter()
        .for_each(|v| w.set_child_folders_want(v));
    w.set_show_desc_count(c.show_desc_count);
    c.desc_count
        .into_iter()
        .for_each(|v| w.set_desc_count_want(v));
    w.set_show_desc_files(c.show_desc_files);
    c.desc_files
        .into_iter()
        .for_each(|v| w.set_desc_files_want(v));
    w.set_show_desc_folders(c.show_desc_folders);
    c.desc_folders
        .into_iter()
        .for_each(|v| w.set_desc_folders_want(v));
    w.set_show_folder_depth(c.show_folder_depth);
    c.folder_depth
        .into_iter()
        .for_each(|v| w.set_folder_depth_want(v));
    w.set_show_hard_links(c.show_hard_links);
    c.hard_links
        .into_iter()
        .for_each(|v| w.set_hard_links_want(v));
    w.set_show_shortcut_target(c.show_shortcut_target);
    c.shortcut_target
        .into_iter()
        .for_each(|v| w.set_shortcut_target_want(v));
    w.set_show_owner(c.show_owner);
    c.owner.into_iter().for_each(|v| w.set_owner_want(v));
    w.set_show_width(c.show_width);
    c.width.into_iter().for_each(|v| w.set_width_want(v));
    w.set_show_height(c.show_height);
    c.height.into_iter().for_each(|v| w.set_height_want(v));
    w.set_show_dimensions(c.show_dimensions);
    c.dimensions
        .into_iter()
        .for_each(|v| w.set_dimensions_want(v));
    w.set_show_length(c.show_length);
    c.length.into_iter().for_each(|v| w.set_length_want(v));
    w.set_show_bit_rate(c.show_bit_rate);
    c.bit_rate.into_iter().for_each(|v| w.set_bit_rate_want(v));
    w.set_show_title(c.show_title);
    c.title.into_iter().for_each(|v| w.set_title_want(v));
    w.set_show_artist(c.show_artist);
    c.artist.into_iter().for_each(|v| w.set_artist_want(v));
    w.set_show_album(c.show_album);
    c.album.into_iter().for_each(|v| w.set_album_want(v));
    w.set_show_year(c.show_year);
    c.year.into_iter().for_each(|v| w.set_year_want(v));
    w.set_show_track(c.show_track);
    c.track.into_iter().for_each(|v| w.set_track_want(v));
    w.set_show_genre(c.show_genre);
    c.genre.into_iter().for_each(|v| w.set_genre_want(v));
    w.set_show_rating(c.show_rating);
    c.rating.into_iter().for_each(|v| w.set_rating_want(v));
    w.set_show_tags(c.show_tags);
    c.tags.into_iter().for_each(|v| w.set_tags_want(v));
    w.set_show_page_count(c.show_page_count);
    c.page_count
        .into_iter()
        .for_each(|v| w.set_page_count_want(v));
    w.set_show_word_count(c.show_word_count);
    c.word_count
        .into_iter()
        .for_each(|v| w.set_word_count_want(v));
    w.set_show_camera_maker(c.show_camera_maker);
    c.camera_maker
        .into_iter()
        .for_each(|v| w.set_camera_maker_want(v));
    w.set_show_camera_model(c.show_camera_model);
    c.camera_model
        .into_iter()
        .for_each(|v| w.set_camera_model_want(v));
    w.set_show_date_taken(c.show_date_taken);
    c.date_taken
        .into_iter()
        .for_each(|v| w.set_date_taken_want(v));
    w.set_show_gps_lat(c.show_gps_lat);
    c.gps_lat.into_iter().for_each(|v| w.set_gps_lat_want(v));
    w.set_show_gps_long(c.show_gps_long);
    w.set_show_sha1(c.show_sha1);
    w.set_show_sha384(c.show_sha384);
    w.set_show_sha512(c.show_sha512);
    w.set_show_xxhash(c.show_xxhash);
    w.set_show_dupe_group(c.show_dupe_group);
    c.gps_long.into_iter().for_each(|v| w.set_gps_long_want(v));
    c.sha1.into_iter().for_each(|v| w.set_sha1_want(v));
    c.sha384.into_iter().for_each(|v| w.set_sha384_want(v));
    c.sha512.into_iter().for_each(|v| w.set_sha512_want(v));
    c.xxhash.into_iter().for_each(|v| w.set_xxhash_want(v));
    c.dupe_group
        .into_iter()
        .for_each(|v| w.set_dupe_group_want(v));
    w.set_name_set(c.name.is_some());
    if let Some(v) = c.name {
        w.set_name_want(v);
    }
    w.set_folder_want(c.folder.unwrap_or(0.0));
    c.size.into_iter().for_each(|v| w.set_size_want(v));
    c.date.into_iter().for_each(|v| w.set_date_want(v));
    c.type_.into_iter().for_each(|v| w.set_type_want(v));
    c.ext.into_iter().for_each(|v| w.set_ext_want(v));
    c.created.into_iter().for_each(|v| w.set_created_want(v));
    c.accessed.into_iter().for_each(|v| w.set_accessed_want(v));
    c.attrs.into_iter().for_each(|v| w.set_attr_want(v));
    c.md5.into_iter().for_each(|v| w.set_md5_want(v));
    c.sha256.into_iter().for_each(|v| w.set_sha256_want(v));
    c.crc32.into_iter().for_each(|v| w.set_crc32_want(v));
    c.name_freq
        .into_iter()
        .for_each(|v| w.set_name_freq_want(v));
    c.ext_freq.into_iter().for_each(|v| w.set_ext_freq_want(v));
    c.size_freq
        .into_iter()
        .for_each(|v| w.set_size_freq_want(v));
}

/// The window's title: the search of the shown tab (none for an empty one),
/// the program's name, and how many other tabs are open.
fn window_title(search: &str, other_tabs: usize) -> String {
    let mut out = if search.trim().is_empty() {
        "EveryGrep".to_owned()
    } else {
        // A title is one line.
        let line: String = search
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        format!("{line} - EveryGrep")
    };
    if other_tabs > 0 {
        out.push(' ');
        out.push_str(&fill(
            crate::i18n::trn("(and {} other tab)", "(and {} other tabs)", other_tabs),
            &[&other_tabs],
        ));
    }
    out
}

fn title(tab: &Tab) -> String {
    if tab.search.query.is_empty() {
        tr("New tab").into()
    } else {
        crate::fileops_search::readable(&tab.search.query)
    }
}

impl State {
    fn forget_session(&self) {
        self.save(Ask::Forget);
    }

    /// Hands `ask` to the saving thread; a failure is shown when it comes.
    fn save(&self, ask: Ask) {
        if let Some(job) = self.saver.ask(ask) {
            self.prefs.save_in_order(move || {
                let result = job();
                if result.is_err() {
                    let _ = slint::invoke_from_event_loop(move || {
                        with(|s| s.report_write(result));
                    });
                }
            });
        }
    }

    fn observe(&self) {
        let Some(w) = self.main.upgrade() else { return };
        self.observe_search(&w);
        let snapshot = Session::capture(&self.scope, &self.tabs.borrow());
        self.save(Ask::Observe {
            enabled: self.prefs.restore_session(),
            snapshot,
            now: self.started.elapsed(),
        });
    }

    fn observe_search(&self, w: &AppWindow) {
        let search = read(w);
        let changed = {
            let mut tabs = self.tabs.borrow_mut();
            let active = tabs.active;
            let changed = tabs.open[active].search != search;
            tabs.open[active].search = search;
            changed
        };
        if changed {
            self.fill();
        }
    }

    fn report_write(&self, result: std::io::Result<()>) {
        if let Err(e) = result {
            let message = fill(
                tr("Cannot save tabs: {}"),
                &[&session_detail(self.saver.path(), &e)],
            );
            if self.session_error.borrow().as_ref() != Some(&message) {
                crate::debug_log::note!("cannot save tabs: {e}");
                if let Some(w) = self.main.upgrade() {
                    crate::error(&w, message.clone());
                }
                *self.session_error.borrow_mut() = Some(message);
            }
        }
    }

    fn store(&self, w: &AppWindow) {
        // End a rename and flush the old tab's delayed history record before leaving.
        self.pause.stop();
        w.invoke_history_record();
        crate::stop_renaming(w, &self.model);
        let mut tabs = self.tabs.borrow_mut();
        let active = tabs.active;
        let tab = &mut tabs.open[active];
        tab.search = read(w);
        tab.folders = crate::folders::freeze();
        let result = self.model.freeze();
        tab.settled = result.settled.clone();
        tab.result = Some(result);
        tab.history = self.prefs.history.borrow().clone();
        tab.current = w.get_current();
        tab.scroll_y = w.get_results_scroll_y();
        tab.scroll_x = w.get_results_scroll_x();
        tab.stopped = self.engine.stopped_search();
        tab.pending = w.get_search_pending();
        tab.submitted = Some(self.last_submitted.borrow().clone());
    }

    fn install(&self, w: &AppWindow) {
        self.tabs.borrow_mut().shown(self.prefs.tab_results_kept());
        let tabs = self.tabs.borrow();
        let tab = &tabs.open[tabs.active];
        crate::folders::restore(&tab.folders);
        let search = &tab.search;
        w.set_query(search.query.as_str().into());
        // The search below asks with the tab's scope.
        crate::scope_ui::set(w, search.within.clone(), false);
        let policy = self.prefs.search_policy();
        // Clear the previous tab's extra slot before selecting this tab's built-in.
        crate::saved_ui::tab_filter(w, None, false);
        crate::set_options(w, search.options);
        self.prefs.restore_search_policy(w);
        crate::show_badges(w);
        if search.filter.is_some() {
            crate::saved_ui::tab_filter(w, search.filter.as_ref(), search.filter_snapshot);
        }
        w.set_sort_column(search.sort_column);
        w.set_sort_descending(search.descending);
        crate::sort_dialog::clear_then(w);
        install_columns(w, &search.columns);
        let counted = self.model.frequency_columns().any();
        crate::apply_detail_columns(w);
        // A frequency column shown now needs a search to count the index.
        let counting = !counted && self.model.frequency_columns().any();
        self.model.restore(tab.result.as_ref());
        crate::folders::results_changed();
        w.set_current(if self.model.row_count() == 0 {
            -1
        } else {
            tab.current.clamp(0, self.model.row_count() as i32 - 1)
        });
        w.set_results_scroll_y(tab.scroll_y);
        // After the tab's columns, so the table has its width; kept within
        // the room the table has now (the window may have changed since).
        w.set_results_scroll_x(tab.scroll_x.clamp(-w.get_scroll_room(), 0.0));
        let mut history = tab.history.clone();
        history.set_max(self.prefs.history_limit());
        *self.prefs.history.borrow_mut() = history;
        // What the field shows still waits for Enter when it would wait
        // now (as a search that reads files does while typing).
        let pending = tab.pending && {
            let filter = search
                .filter
                .as_ref()
                .map(|f| crate::saved_ui::filter_for_tab(f, search.filter_snapshot));
            let options = search.effective_options(policy, filter.as_ref());
            let filter = filter.map(|f| (f.search, f.options));
            let reads = crate::typing::reads_files(&search.query, options, filter.as_ref());
            crate::typing::with(|t| t.waits(reads))
        };
        let submitted = if pending {
            tab.submitted.as_ref().unwrap_or(search)
        } else {
            search
        };
        let sort = crate::sort_for_column(
            submitted.sort_column,
            submitted.descending,
            self.model.folder_size(),
        );
        // Only the visible tab updates. Unsubmitted text keeps waiting for Enter.
        let filter = submitted
            .filter
            .as_ref()
            .map(|f| crate::saved_ui::filter_for_tab(f, submitted.filter_snapshot));
        let options = submitted.effective_options(policy, filter.as_ref());
        let filter = filter.map(|f| (f.search, f.options));
        // A content search the user stopped stays stopped with its matches
        // so far, and complete results stay while the index did not change
        // (a duplicate or content search is not run again for nothing),
        // while they are still for this index and nothing changed the
        // request; otherwise the tab searches again.
        let frozen = !pending && self.model.snapshot().1 == self.engine.epoch();
        let kept = frozen
            && (tab.stopped.as_ref().is_some_and(|stopped| {
                self.engine
                    .show_stopped(stopped, &submitted.query, options, filter.clone(), sort)
                    .is_some()
            }) || !counting
                && tab.settled.as_ref().is_some_and(|settled| {
                    self.engine
                        .show_settled(settled, &submitted.query, options, filter.clone(), sort)
                        .is_some()
                }));
        if !kept {
            self.engine.set_sort(sort);
            self.engine
                .set_filtered_query(&submitted.query, options, filter);
        }
        *self.last_submitted.borrow_mut() = submitted.clone();
        crate::typing::with(|t| {
            t.searched();
            if pending {
                t.edited(true);
            }
        });
        w.set_search_pending(pending);
        w.set_message("".into());
        if pending {
            crate::wait_for_enter(w);
        } else {
            crate::info(w, String::new());
        }
        crate::refresh_status(w);
        w.invoke_focus_search();
    }

    fn change(&self, change: impl Fn(&mut Tabs)) {
        let Some(w) = self.main.upgrade() else { return };
        if blocked(&w) {
            return;
        }
        let mut current = self.tabs.borrow().clone();
        current.open[current.active].search = read(&w);
        let mut preview = current.clone();
        change(&mut preview);
        self.close_picker();
        if current.same_layout(&preview) {
            return;
        }
        self.store(&w);
        change(&mut self.tabs.borrow_mut());
        self.install(&w);
        self.fill();
    }

    fn fill(&self) {
        let Some(w) = self.main.upgrade() else { return };
        let tabs = self.tabs.borrow();
        let rows: Vec<_> = tabs
            .open
            .iter()
            .enumerate()
            .map(|(i, t)| TabEntry {
                title: title(t).into(),
                checked: i == tabs.active,
            })
            .collect();
        let shown = crate::menus::window(rows.len(), Some(tabs.active), crate::menus::TABS);
        w.set_tab_items(ModelRc::new(VecModel::from(
            rows[shown.clone()]
                .iter()
                .zip(shown)
                .map(|(t, i)| MenuEntry {
                    title: t.title.clone(),
                    checked: t.checked,
                    index: i as i32,
                })
                .collect::<Vec<_>>(),
        )));
        let search = tabs
            .open
            .get(tabs.active)
            .map(|t| crate::fileops_search::readable(&t.search.query))
            .unwrap_or_default();
        w.set_window_title(window_title(&search, rows.len().saturating_sub(1)).into());
        w.set_tabs(ModelRc::new(VecModel::from(rows)));
        w.set_active_tab(tabs.active as i32);
        w.set_closed_tab_names(ModelRc::new(VecModel::from(
            tabs.closed
                .iter()
                .rev()
                .map(|t| title(t).into())
                .collect::<Vec<slint::SharedString>>(),
        )));
    }

    fn close_picker(&self) {
        if let Some(picker) = self.picker.borrow_mut().take() {
            let _ = picker.hide();
        }
    }

    fn picker(self: &Rc<Self>, multiple: bool) {
        self.close_picker();
        let Ok(w) = TabsWindow::new() else { return };
        let rows = Rc::new(VecModel::from(
            self.tabs
                .borrow()
                .open
                .iter()
                .map(|t| TabEntry {
                    title: title(t).into(),
                    checked: false,
                })
                .collect::<Vec<_>>(),
        ));
        w.set_tabs(ModelRc::from(rows.clone()));
        w.set_multiple(multiple);
        let weak = Rc::downgrade(self);
        w.on_chosen(move |i| {
            if let (Some(s), Ok(i)) = (weak.upgrade(), usize::try_from(i)) {
                s.change(|t| t.select(i));
            }
        });
        let marked = rows.clone();
        w.on_marked(move |i, checked| {
            if let Ok(i) = usize::try_from(i) {
                if let Some(mut row) = marked.row_data(i) {
                    row.checked = checked;
                    marked.set_row_data(i, row);
                }
            }
        });
        let weak = Rc::downgrade(self);
        w.on_close_marked(move || {
            if let Some(s) = weak.upgrade() {
                let indexes: Vec<_> = (0..rows.row_count())
                    .filter(|i| rows.row_data(*i).is_some_and(|t| t.checked))
                    .collect();
                s.change(|t| t.close(&indexes));
            }
        });
        let _ = w.show();
        *self.picker.borrow_mut() = Some(w);
    }

    fn command(self: &Rc<Self>, command: Command) {
        if self.main.upgrade().is_none_or(|w| blocked(&w)) {
            return;
        }
        if command == Command::ListTabs {
            self.picker(false);
            return;
        }
        if command == Command::CloseMultipleTabs {
            self.picker(true);
            return;
        }
        self.change(|t| match command {
            Command::NewTab => {
                let mut search = t.open[t.active].search.clone();
                search.query.clear();
                t.add(
                    Tab {
                        search,
                        ..Default::default()
                    },
                    true,
                );
            }
            Command::DuplicateTab => t.duplicate(),
            Command::CloseTab => t.close(&[t.active]),
            Command::NextTab => t.step(true),
            Command::PreviousTab => t.step(false),
            Command::SelectTab1 => t.select(0),
            Command::SelectTab2 => t.select(1),
            Command::SelectTab3 => t.select(2),
            Command::SelectTab4 => t.select(3),
            Command::SelectTab5 => t.select(4),
            Command::SelectTab6 => t.select(5),
            Command::SelectTab7 => t.select(6),
            Command::SelectTab8 => t.select(7),
            Command::SelectTab9 => t.select(8),
            Command::SelectTab10 => t.select(9),
            Command::CloseLeftTabs => t.close(&(0..t.active).collect::<Vec<_>>()),
            Command::CloseRightTabs => t.close(&(t.active + 1..t.open.len()).collect::<Vec<_>>()),
            Command::CloseOtherTabs => t.close(
                &(0..t.open.len())
                    .filter(|i| *i != t.active)
                    .collect::<Vec<_>>(),
            ),
            Command::ReopenTab => {
                if let Some(i) = t.closed.len().checked_sub(1) {
                    t.reopen(i);
                }
            }
            Command::ReopenAllTabs => t.reopen_all(),
            _ => {}
        });
    }
}

#[cfg(test)]
mod tests {
    use super::window_title;

    #[test]
    fn the_window_title_names_the_search_and_the_other_tabs() {
        assert_eq!(window_title("", 0), "EveryGrep");
        assert_eq!(window_title("  ", 0), "EveryGrep");
        assert_eq!(window_title("report", 0), "report - EveryGrep");
        assert_eq!(
            window_title("report", 1),
            "report - EveryGrep (and 1 other tab)"
        );
        assert_eq!(window_title("", 3), "EveryGrep (and 3 other tabs)");
        // Line breaks and tabs of a content search stay on one line.
        assert_eq!(
            window_title("content:\"a\nb\"\tx", 0),
            "content:\"a b\" x - EveryGrep"
        );
    }
}
