//! A cancellable, read-only snapshot of indexed, live and cached file details.
use crate::{AppWindow, engine::Engine, hashes::HashStore, i18n::tr};
use everygrep_core::{
    details::Details,
    hash::FileKey,
    index::Index,
    localtime::Zone,
    progress::Progress,
    property::{self, Property},
    propertycache::Store,
    store::NodeId,
};
use slint::{ComponentHandle, Model};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
    mpsc,
};

const PERMISSIONS: i32 = 52;
const FINGERPRINT: i32 = 53;
const SAMPLE: i32 = 54;
const CHANGED: i32 = 55;
struct Snapshot {
    path: PathBuf,
    offline: bool,
    dir: bool,
    unavailable: bool,
    values: BTreeMap<i32, String>,
}
fn indexed(index: &Index, id: NodeId, progress: &Progress) -> Option<Snapshot> {
    let store = index.store();
    if id.index() >= store.len() || !store.is_live(id) || progress.is_cancelled() {
        return None;
    }
    let path = store.path(id);
    let dir = store.is_dir(id);
    let meta = store.meta(id);
    let extension = if dir {
        String::new()
    } else {
        path.extension()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    };
    let name = String::from_utf8_lossy(store.name(id)).into_owned();
    let kind = crate::filetype::classify(name.as_bytes(), dir);
    let mut values = BTreeMap::from([
        (0, name),
        (1, path.parent().unwrap_or(&path).display().to_string()),
        (2, exact_size(meta.size)),
        (3, crate::format::timestamp(meta.mtime_ns, &Zone::local())),
        (4, crate::labels::type_name(kind, &extension)),
        (5, extension),
    ]);
    for p in Property::ALL
        .into_iter()
        .filter(|p| !p.external() && !p.is_media())
    {
        values.insert(
            p.column(),
            p.display(&property::read(index, id, p, progress).ok()?),
        );
    }
    Some(Snapshot {
        path,
        dir,
        offline: index.is_offline(id),
        unavailable: false,
        values,
    })
}
fn exact_size(size: u64) -> String {
    format!("{} B ({})", size, crate::format::size(size))
}
fn live(s: &mut Snapshot, properties: &Store, hashes: &HashStore, progress: &Progress) {
    if s.offline || progress.is_cancelled() {
        return;
    }
    let Ok(meta) = std::fs::symlink_metadata(&s.path) else {
        s.unavailable = true;
        return;
    };
    let zone = Zone::local();
    let ns = |seconds: i64, nanos: i64| seconds.saturating_mul(1_000_000_000).saturating_add(nanos);
    s.values.insert(
        3,
        crate::format::timestamp(ns(meta.mtime(), meta.mtime_nsec()), &zone),
    );
    s.values.insert(
        CHANGED,
        crate::format::timestamp(ns(meta.ctime(), meta.ctime_nsec()), &zone),
    );
    if !s.dir {
        s.values.insert(2, exact_size(meta.len()));
    }
    if meta.is_symlink() {
        s.values.insert(
            4,
            crate::labels::type_name(crate::filetype::FileKind::Link, "").to_string(),
        );
    }
    let details = Details::from_metadata(s.path.file_name().unwrap_or_default().as_bytes(), &meta);
    for (column, time) in [(6, details.created_ns), (7, details.accessed_ns)] {
        if let Some(time) = time {
            s.values
                .insert(column, crate::format::timestamp(time, &zone));
        }
    }
    s.values
        .insert(8, details.attributes.unwrap_or_default().letters());
    s.values
        .insert(PERMISSIONS, format!("{:04o}", meta.mode() & 0o7777));
    for p in Property::ALL.into_iter().filter(|p| p.external()) {
        s.values.insert(
            p.column(),
            p.display(&property::read_path(&s.path, p, progress)),
        );
    }
    if meta.is_file() {
        let wanted = Property::ALL
            .into_iter()
            .filter(|p| p.is_media())
            .collect::<Vec<_>>();
        for (p, value) in properties.read(&s.path, &wanted, progress) {
            s.values.insert(p.column(), p.display(&value));
        }
        if progress.is_cancelled() {
            return;
        }
        let key = FileKey {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.len(),
            mtime_ns: ns(meta.mtime(), meta.mtime_nsec()),
        };
        if let Some(cached) = hashes.cache().lock().expect("hash cache").get(&key) {
            if let Some(v) = cached.md5 {
                s.values.insert(9, everygrep_core::hash::hex(&v));
            }
            if let Some(v) = cached.sha256 {
                s.values.insert(10, everygrep_core::hash::hex(&v));
            }
            if let Some(v) = cached.crc32 {
                s.values.insert(11, format!("{v:08x}"));
            }
            if let Some(v) = cached.full {
                s.values.insert(FINGERPRINT, format!("{v:032x}"));
            }
            if let Some(v) = cached.ends {
                s.values.insert(SAMPLE, format!("{v:032x}"));
            }
        }
    }
}
fn render(s: &Snapshot, titles: &[String]) -> String {
    let mut text = String::new();
    if s.offline {
        text.push_str(tr("Offline"));
        text.push_str("\n\n");
    }
    if s.unavailable {
        text.push_str(tr("File details are unavailable."));
        text.push_str("\n\n");
    }
    for (column, value) in &s.values {
        if value.is_empty() {
            continue;
        }
        let Some(title) = titles.get(*column as usize) else {
            continue;
        };
        text.push_str(title);
        text.push_str(": ");
        text.push_str(value);
        text.push('\n');
    }
    text
}
struct Ask {
    id: NodeId,
    epoch: u64,
    serial: u64,
    progress: Arc<Progress>,
    titles: Vec<String>,
}
pub fn connect(main: &AppWindow, engine: &Arc<Engine>) {
    let (send, receive) = mpsc::channel::<Ask>();
    let current = Arc::new(AtomicU64::new(0));
    let progress = Rc::new(RefCell::new(Arc::new(Progress::new())));
    let weak = main.as_weak();
    let worker = engine.clone();
    let serial = current.clone();
    std::thread::spawn(move || {
        while let Ok(mut ask) = receive.recv() {
            for newer in receive.try_iter() {
                ask = newer;
            }
            if ask.progress.is_cancelled() {
                continue;
            }
            let snapshot = worker.index.read().ok().and_then(|index| {
                (worker.epoch() == ask.epoch)
                    .then(|| indexed(&index, ask.id, &ask.progress))
                    .flatten()
            });
            let text = if let Some(mut s) = snapshot {
                live(&mut s, &worker.properties, &worker.hashes, &ask.progress);
                render(&s, &ask.titles)
            } else {
                tr("File details are unavailable.").to_string()
            };
            if ask.progress.is_cancelled() {
                continue;
            }
            let serial = serial.clone();
            let engine = worker.clone();
            let _ = weak.upgrade_in_event_loop(move |w| {
                if w.get_properties_open() && serial.load(Ordering::SeqCst) == ask.serial {
                    w.set_properties_text(if engine.epoch() == ask.epoch {
                        text.into()
                    } else {
                        tr("File details are unavailable.").into()
                    });
                }
            });
        }
    });
    main.on_show_properties({
        let weak = main.as_weak();
        let current = current.clone();
        let progress = progress.clone();
        move |row| {
            let Some(w) = weak.upgrade() else {
                return;
            };
            let mut selected = None;
            crate::with_model(&w, |m| {
                if let Ok(row) = usize::try_from(row)
                    && let Some(id) = m.id(row)
                {
                    selected = Some((id, m.snapshot().1));
                }
            });
            let Some((id, epoch)) = selected else {
                return;
            };
            progress.borrow().cancel();
            let p = Arc::new(Progress::new());
            *progress.borrow_mut() = p.clone();
            let serial = current.fetch_add(1, Ordering::SeqCst) + 1;
            let mut titles = w
                .get_column_choices()
                .iter()
                .map(|c| c.title.to_string())
                .collect::<Vec<_>>();
            titles.extend(
                [
                    tr("Permissions"),
                    tr("Content fingerprint"),
                    tr("Sample fingerprint"),
                    tr("Metadata changed"),
                ]
                .into_iter()
                .map(String::from),
            );
            w.set_properties_text(tr("Reading file details…").into());
            w.set_properties_open(true);
            let _ = send.send(Ask {
                id,
                epoch,
                serial,
                progress: p,
                titles,
            });
        }
    });
    main.on_properties_closed(move || {
        current.fetch_add(1, Ordering::SeqCst);
        progress.borrow().cancel();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use everygrep_core::{
        hashcache::Cached,
        store::{Meta, NodeStore},
    };
    #[test]
    fn offline_properties_use_only_the_index_and_cancel_cleanly() {
        let mut store = NodeStore::new();
        let root = store.add(None, b"/not-mounted/files", true, Meta::default());
        let id = store.add(
            Some(root),
            b"book.pdf",
            false,
            Meta {
                size: 123,
                mtime_ns: 0,
            },
        );
        let mut index = Index::new(store);
        index.set_offline(root, true);
        let p = Progress::new();
        let mut s = indexed(&index, id, &p).unwrap();
        live(&mut s, &Store::default(), &HashStore::new(None, 1024), &p);
        assert!(s.offline);
        assert!(s.values[&2].starts_with("123 B"));
        assert!(!s.values.contains_key(&PERMISSIONS));
        p.cancel();
        assert!(indexed(&index, id, &p).is_none());
    }
    #[test]
    fn live_properties_reuse_media_and_only_existing_hashes() {
        let dir = std::env::temp_dir().join(format!("everygrep-properties-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("a.png");
        std::fs::write(
            &path,
            everygrep_core::preview::png(1, 1, &[0, 0, 0, 255]).unwrap(),
        )
        .unwrap();
        let mut nodes = NodeStore::new();
        let id = nodes.add(None, path.as_os_str().as_bytes(), false, Meta::default());
        let index = Index::new(nodes);
        let p = Progress::new();
        let hashes = HashStore::new(None, 4096);
        let media = Store::default();
        let mut s = indexed(&index, id, &p).unwrap();
        live(&mut s, &media, &hashes, &p);
        assert_eq!(s.values[&Property::Width.column()], "1");
        assert!(!s.values.contains_key(&9));
        assert!(hashes.cache().lock().unwrap().is_empty());
        let key = everygrep_core::hash::open(&path).unwrap().key;
        hashes.cache().lock().unwrap().put(
            key,
            &Cached {
                md5: Some([1; 16]),
                ..Cached::default()
            },
        );
        live(&mut s, &media, &hashes, &p);
        assert_eq!(s.values[&9], "01".repeat(16));
        let link = dir.join("link.png");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let id = nodes_for_link(&link);
        let mut linked = indexed(&id.0, id.1, &p).unwrap();
        live(&mut linked, &media, &hashes, &p);
        assert!(linked.values[&8].contains('L'));
        assert!(!linked.values.contains_key(&Property::Width.column()));
        assert!(!linked.values.contains_key(&9));
        std::fs::remove_dir_all(dir).unwrap();
    }
    fn nodes_for_link(path: &std::path::Path) -> (Index, NodeId) {
        let mut nodes = NodeStore::new();
        let id = nodes.add(None, path.as_os_str().as_bytes(), false, Meta::default());
        (Index::new(nodes), id)
    }
}
