//! One debounced worker; only the current selection may publish an answer.
use crate::{AppWindow, engine::Engine, filetype::KindIcon};
use everygrep_core::{content::CodePage, preview as data, progress::Progress, store::NodeId};
use slint::{ComponentHandle, Rgba8Pixel, SharedPixelBuffer, Timer, TimerMode};
use std::cell::{Cell, RefCell};
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
use std::rc::Rc;
use std::sync::{Arc, mpsc};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    pub cache: bool,
    pub images: bool,
    pub thumbnailers: bool,
    pub sandbox: bool,
    pub text: bool,
    /// Let the glycin helper (HEIC, JXL, SVG …) make its own nested sandbox.
    pub nested: bool,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            cache: true,
            images: true,
            thumbnailers: true,
            sandbox: true,
            text: true,
            nested: true,
        }
    }
}
impl Options {
    pub fn allows_thumbnailers(self) -> bool {
        self.thumbnailers && self.sandbox
    }
    pub fn set(&mut self, key: &str, value: &str) -> bool {
        let field = match key {
            "preview-cache" => &mut self.cache,
            "preview-images" => &mut self.images,
            "preview-thumbnailers" => &mut self.thumbnailers,
            "preview-sandbox" => &mut self.sandbox,
            "preview-text" => &mut self.text,
            "preview-nested-sandbox" => &mut self.nested,
            _ => return false,
        };
        *field = match value {
            "true" | "yes" | "on" | "1" => true,
            "false" | "no" | "off" | "0" => false,
            _ => return false,
        };
        true
    }
    pub fn write(self, out: &mut String, all: bool) {
        for (key, value) in [
            ("preview-cache", self.cache),
            ("preview-images", self.images),
            ("preview-thumbnailers", self.thumbnailers),
            ("preview-sandbox", self.sandbox),
            ("preview-text", self.text),
            ("preview-nested-sandbox", self.nested),
        ] {
            if all || !value {
                out.push_str(&format!("{key} = {value}\n"));
            }
        }
    }
    pub fn fill(self, w: &crate::SettingsWindow) {
        w.set_preview_cache(self.cache);
        w.set_preview_images(self.images);
        w.set_preview_thumbnailers(self.thumbnailers);
        w.set_preview_sandbox(self.sandbox);
        w.set_preview_text(self.text);
        w.set_preview_nested(self.nested);
    }
    pub fn edited(w: &crate::SettingsWindow) -> Self {
        Self {
            cache: w.get_preview_cache(),
            images: w.get_preview_images(),
            thumbnailers: w.get_preview_thumbnailers(),
            sandbox: w.get_preview_sandbox(),
            text: w.get_preview_text(),
            nested: w.get_preview_nested(),
        }
    }
}
struct Ask {
    serial: u64,
    id: NodeId,
    epoch: u64,
    progress: Arc<Progress>,
    options: Options,
    page: CodePage,
}
struct Answer {
    serial: u64,
    epoch: u64,
    image: Option<SharedPixelBuffer<Rgba8Pixel>>,
    text: String,
    info: String,
    kind: i32,
}
fn selection(id: Option<NodeId>, list_epoch: u64, current_epoch: u64) -> Option<NodeId> {
    id.filter(|_| list_epoch == current_epoch)
}
fn debounced<T>(receive: &mpsc::Receiver<T>, mut latest: T, quiet: Duration) -> T {
    while let Ok(newer) = receive.recv_timeout(quiet) {
        latest = newer;
    }
    latest
}

fn decode(bytes: &[u8], oriented: bool) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    data::dimensions(bytes, data::PIXEL_CAP)?;
    let image = slint::Image::load_from_data(bytes, None).ok()?.to_rgba8()?;
    Some(scale(
        &image,
        if oriented {
            data::orientation(bytes)
        } else {
            1
        },
        1024,
    ))
}
fn scale(
    source: &SharedPixelBuffer<Rgba8Pixel>,
    orientation: u32,
    cap: u32,
) -> SharedPixelBuffer<Rgba8Pixel> {
    let (w, h) = (source.width(), source.height());
    let (ow, oh) = if orientation >= 5 { (h, w) } else { (w, h) };
    let ratio = (cap as f64 / ow.max(oh) as f64).min(1.);
    let (tw, th) = (
        (ow as f64 * ratio).round().max(1.) as u32,
        (oh as f64 * ratio).round().max(1.) as u32,
    );
    let mut result = SharedPixelBuffer::<Rgba8Pixel>::new(tw, th);
    for y in 0..th {
        for x in 0..tw {
            let (a, b) = (x * ow / tw, y * oh / th);
            let (sx, sy) = match orientation {
                2 => (w - 1 - a, b),
                3 => (w - 1 - a, h - 1 - b),
                4 => (a, h - 1 - b),
                5 => (b, a),
                6 => (b, h - 1 - a),
                7 => (w - 1 - b, h - 1 - a),
                8 => (w - 1 - b, a),
                _ => (a, b),
            };
            result.make_mut_slice()[(y * tw + x) as usize] =
                source.as_slice()[(sy * w + sx) as usize];
        }
    }
    result
}
fn load(engine: &Engine, ask: &Ask) -> Option<Answer> {
    let index = engine.index.read().ok()?;
    let store = index.store();
    if engine.epoch() != ask.epoch || ask.id.index() >= store.len() || !store.is_live(ask.id) {
        return None;
    }
    let path = store.path(ask.id);
    let dir = store.is_dir(ask.id);
    let offline = index.is_offline(ask.id);
    let kind = crate::filetype::classify(path.file_name().unwrap_or_default().as_bytes(), dir);
    let meta = store.meta(ask.id);
    let info = format!(
        "{}\n{}\n{}",
        path.display(),
        crate::format::size(meta.size),
        crate::format::timestamp(meta.mtime_ns, &everygrep_core::localtime::Zone::local())
    );
    drop(index);
    let mut answer = Answer {
        serial: ask.serial,
        epoch: ask.epoch,
        image: None,
        text: String::new(),
        info,
        kind: kind.icon_index(),
    };
    if offline || dir || ask.progress.is_cancelled() {
        return Some(answer);
    }
    let Ok(meta) = std::fs::symlink_metadata(&path) else {
        return Some(answer);
    };
    if !meta.is_file() {
        return Some(answer);
    }
    let uri = crate::open::file_uri(&path);
    let cache = data::cache_root();
    if ask.options.cache
        && let Some(root) = &cache
        && let Some(bytes) = data::lookup(root, &uri, meta.mtime(), meta.len(), &ask.progress)
    {
        answer.image = decode(&bytes, false);
        if answer.image.is_some() {
            return Some(answer);
        }
    }
    if ask.options.images
        && matches!(kind, crate::filetype::FileKind::Image)
        && meta.len() <= data::INPUT_CAP as u64
        && let Some(bytes) = data::read(&path, data::INPUT_CAP, &ask.progress)
    {
        answer.image = decode(&bytes, true);
    }
    if ask.progress.is_cancelled() {
        return None;
    }
    if answer.image.is_none()
        && ask.options.text
        && matches!(
            kind,
            crate::filetype::FileKind::Text
                | crate::filetype::FileKind::Code
                | crate::filetype::FileKind::File
        )
        && let Some(bytes) = data::read(&path, data::TEXT_CAP, &ask.progress)
    {
        answer.text = data::text(&bytes, ask.page).unwrap_or_default();
    }
    let mut failed = false;
    if answer.image.is_none() && answer.text.is_empty() && ask.options.allows_thumbnailers() {
        // Failure records are private to this application and expire with the
        // source mtime/size; never poison another application's failure cache.
        let cached_failure = cache
            .as_ref()
            .and_then(|r| {
                data::read(
                    &r.join("fail/everygrep").join(data::name(&uri)),
                    data::OUTPUT_CAP,
                    &ask.progress,
                )
            })
            .is_some_and(|b| data::valid(&b, &uri, meta.mtime(), meta.len()));
        if let Some(why) = crate::thumbnailer::unavailable() {
            // The sandbox itself cannot run: say so once, and record no
            // failure for this file, so it is tried again once it works.
            answer.info = format!(
                "{}\n{}",
                answer.info,
                crate::labels::thumbnails_unavailable(why)
            );
        } else if !cached_failure {
            let mut attempted = false;
            answer.image = crate::thumbnailer::generate(
                &path,
                &ask.progress,
                &mut attempted,
                ask.options.nested,
            )
            .and_then(|b| decode(&b, false));
            failed = attempted && answer.image.is_none();
        }
    }
    if ask.progress.is_cancelled() {
        return None;
    }
    // A replacement during decoding must not create a thumbnail for stale data.
    let unchanged = std::fs::symlink_metadata(&path).is_ok_and(|now| {
        now.ino() == meta.ino()
            && now.dev() == meta.dev()
            && now.len() == meta.len()
            && now.mtime() == meta.mtime()
            && now.mtime_nsec() == meta.mtime_nsec()
    });
    if !unchanged {
        return None;
    }
    let store = cache.as_ref().filter(|r| data::cacheable(r, &path));
    if failed
        && let Some(root) = store
        && let Some(bytes) = data::png(1, 1, &[0, 0, 0, 0])
    {
        let _ = data::save(
            root,
            "fail/everygrep",
            &bytes,
            &uri,
            meta.mtime(),
            meta.len(),
        );
    }
    if let (Some(root), Some(image)) = (store, &answer.image) {
        for (tier, cap) in [("large", 256), ("normal", 128)] {
            let thumb = scale(image, 1, cap);
            if let Some(png) = data::png(thumb.width(), thumb.height(), thumb.as_bytes()) {
                let _ = data::save(root, tier, &png, &uri, meta.mtime(), meta.len());
            }
        }
    }
    Some(answer)
}

pub fn connect(w: &AppWindow, engine: &Arc<Engine>) {
    let (send, receive) = mpsc::channel::<Ask>();
    let (done, answers) = mpsc::channel::<Answer>();
    let worker = engine.clone();
    std::thread::spawn(move || {
        while let Ok(ask) = receive.recv() {
            // Every newer selection restarts the quiet period.
            let ask = debounced(&receive, ask, Duration::from_millis(150));
            if !ask.progress.is_cancelled()
                && let Ok(Some(answer)) =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| load(&worker, &ask)))
            {
                let _ = done.send(answer);
            }
        }
    });
    let serial = Rc::new(Cell::new(0u64));
    let progress = Rc::new(RefCell::new(Arc::new(Progress::new())));
    w.on_preview_changed({
        let weak = w.as_weak();
        move || {
            if let Some(w) = weak.upgrade() {
                crate::prefs::with(|p| {
                    p.set_preview(w.get_preview_visible(), w.get_preview_width())
                });
            }
        }
    });
    // Current-row changes invalidate immediately, before the next polling tick.
    w.on_preview_selection_changed({
        let progress = progress.clone();
        let serial = serial.clone();
        move || {
            progress.borrow().cancel();
            serial.set(serial.get().wrapping_add(1));
        }
    });
    let weak = w.as_weak();
    let engine = engine.clone();
    let timer = Rc::new(Timer::default());
    let keep = timer.clone();
    let mut previous = None;
    timer.start(TimerMode::Repeated, Duration::from_millis(50), move || {
        let _ = &keep;
        let Some(w) = weak.upgrade() else {
            progress.borrow().cancel();
            return;
        };
        let mut id = None;
        let mut epoch = engine.epoch();
        if w.get_preview_visible() {
            crate::with_model(&w, |m| {
                if let Ok(row) = usize::try_from(w.get_current())
                    && m.is_selected(row)
                {
                    epoch = m.snapshot().1;
                    id = selection(m.id(row), epoch, engine.epoch());
                }
            });
        }
        let (mut options, mut page) = (Options::default(), CodePage::default());
        crate::prefs::with(|p| {
            (options, page) = p.preview_options();
        });
        let key = (id, epoch, serial.get(), options, page);
        if previous != Some(key) {
            progress.borrow().cancel();
            serial.set(serial.get().wrapping_add(1));
            previous = Some((id, key.1, serial.get(), options, page));
            w.set_preview_image(Default::default());
            w.set_preview_text("".into());
            w.set_preview_info("".into());
            w.set_preview_kind(1);
            let current = Arc::new(Progress::new());
            *progress.borrow_mut() = current.clone();
            if let Some(id) = id {
                let _ = send.send(Ask {
                    serial: serial.get(),
                    id,
                    epoch: key.1,
                    progress: current,
                    options,
                    page,
                });
            }
        }
        for answer in answers.try_iter() {
            if answer.serial == serial.get()
                && answer.epoch == engine.epoch()
                && w.get_preview_visible()
            {
                w.set_preview_image(
                    answer
                        .image
                        .map(slint::Image::from_rgba8)
                        .unwrap_or_default(),
                );
                w.set_preview_text(answer.text.into());
                w.set_preview_info(answer.info.into());
                w.set_preview_kind(answer.kind);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preview_policy_round_trips_and_rejects_invalid_flags() {
        let mut s = crate::settings::Settings::parse(
            "preview-visible = true\npreview-width = 450\npreview-cache = false\npreview-images = false\npreview-thumbnailers = false\npreview-sandbox = false\npreview-text = false\npreview-nested-sandbox = false\ncontent-code-page = windows-1254\n",
        );
        assert!(s.problems.is_empty());
        let round = crate::settings::Settings::parse(&s.to_text());
        assert_eq!(round.preview, s.preview);
        assert!(round.preview_visible);
        assert_eq!(round.preview_width, 450.);
        assert_eq!(round.content.code_page, CodePage::Windows1254);
        assert_eq!(
            s.preview,
            Options {
                cache: false,
                images: false,
                thumbnailers: false,
                sandbox: false,
                text: false,
                nested: false,
            }
        );
        let invalid = crate::settings::Settings::parse(
            "preview-cache = maybe\npreview-sandbox = later\npreview-width = NaN\n",
        );
        assert_eq!(invalid.problems.len(), 3);
        assert_eq!(invalid.preview, Options::default());
        assert!(!crate::settings::Settings::default().preview_visible);
        s.reset(crate::settings::Page::Window);
        assert_eq!(s.preview, Options::default());
        assert!(!s.preview_visible);
        assert_eq!(s.content.code_page, CodePage::Windows1254);
    }
    #[test]
    fn disabling_sandbox_always_disables_system_thumbnailers() {
        assert!(Options::default().allows_thumbnailers());
        let mut options = Options::default();
        assert!(options.set("preview-sandbox", "off"));
        assert!(options.thumbnailers);
        assert!(!options.allows_thumbnailers());
        options.set("preview-sandbox", "on");
        options.set("preview-thumbnailers", "off");
        assert!(!options.allows_thumbnailers());
    }
    #[test]
    fn selection_changes_cancel_work_and_debounce_to_the_last_request() {
        assert_eq!(selection(Some(NodeId(1)), 7, 8), None);
        assert_eq!(selection(Some(NodeId(1)), 8, 8), Some(NodeId(1)));
        let (send, receive) = mpsc::channel();
        let old = Arc::new(Progress::new());
        send.send((1, old.clone())).unwrap();
        let first = receive.recv().unwrap();
        old.cancel();
        let current = Arc::new(Progress::new());
        send.send((2, current.clone())).unwrap();
        send.send((3, current.clone())).unwrap();
        drop(send);
        let latest = debounced(&receive, first, Duration::from_millis(1));
        assert_eq!(latest.0, 3);
        assert!(old.is_cancelled());
        assert!(!latest.1.is_cancelled());
        current.cancel();
        assert!(latest.1.is_cancelled());
    }
    #[test]
    fn every_exif_orientation_and_thumbnail_bounds() {
        let mut source = SharedPixelBuffer::<Rgba8Pixel>::new(2, 3);
        for (i, p) in source.make_mut_slice().iter_mut().enumerate() {
            p.r = i as u8;
            p.a = 255;
        }
        for (o, expected) in [
            (1, vec![0, 1, 2, 3, 4, 5]),
            (2, vec![1, 0, 3, 2, 5, 4]),
            (3, vec![5, 4, 3, 2, 1, 0]),
            (4, vec![4, 5, 2, 3, 0, 1]),
            (5, vec![0, 2, 4, 1, 3, 5]),
            (6, vec![4, 2, 0, 5, 3, 1]),
            (7, vec![5, 3, 1, 4, 2, 0]),
            (8, vec![1, 3, 5, 0, 2, 4]),
        ] {
            let out = scale(&source, o, 256);
            assert_eq!(
                out.as_slice().iter().map(|p| p.r).collect::<Vec<_>>(),
                expected
            );
        }
        let out = scale(&SharedPixelBuffer::<Rgba8Pixel>::new(1000, 500), 1, 256);
        assert_eq!((out.width(), out.height()), (256, 128));
        let png = data::png(2, 3, source.as_bytes()).unwrap();
        assert_eq!(decode(&png, false).unwrap().as_bytes(), source.as_bytes());
    }
}
