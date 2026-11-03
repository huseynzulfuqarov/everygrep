//! One bounded queue for optional column reads, cancelled by viewport or query.
use crate::engine::Engine;
use everygrep_core::progress::Progress;
use everygrep_core::property::{self, Property};
use everygrep_core::store::NodeId;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

pub struct Batch {
    pub serial: u64,
    pub row: usize,
    pub id: NodeId,
    pub values: Vec<String>,
}
struct Request {
    serial: u64,
    row: usize,
    id: NodeId,
    epoch: u64,
    columns: Vec<bool>,
}
pub struct Reader {
    sender: mpsc::SyncSender<Request>,
    current: Arc<AtomicU64>,
    progress: Arc<Mutex<Arc<Progress>>>,
}
impl Reader {
    pub fn start(engine: Arc<Engine>, deliver: impl Fn(Batch) + Send + 'static) -> Self {
        let (sender, receiver) = mpsc::sync_channel::<Request>(256);
        let current = Arc::new(AtomicU64::new(0));
        let progress = Arc::new(Mutex::new(Arc::new(Progress::new())));
        let (serial, task) = (current.clone(), progress.clone());
        let _ = std::thread::Builder::new()
            .name("row-properties".into())
            .spawn(move || {
                while let Ok(r) = receiver.recv() {
                    if r.serial != serial.load(Ordering::SeqCst) {
                        continue;
                    }
                    let p = task.lock().expect("property progress").clone();
                    let Ok(index) = engine.index.read() else {
                        continue;
                    };
                    if r.epoch != engine.epoch() {
                        continue;
                    }
                    let mut values = vec![String::new(); Property::ALL.len()];
                    let path = index.store().path(r.id);
                    let online = !index.is_offline(r.id);
                    let online_file = online && !index.store().is_dir(r.id);
                    let mut media = Vec::new();
                    let mut external = Vec::new();
                    for (i, property) in Property::ALL.iter().enumerate() {
                        if r.columns.get(i) == Some(&true) {
                            if property.is_media() {
                                media.push(*property);
                            } else if property.external() {
                                external.push(*property);
                            } else {
                                values[i] = property::read(&index, r.id, *property, &p)
                                    .unwrap_or_default()
                                    .to_string();
                            }
                        }
                    }
                    drop(index);
                    // Filesystem reads run after the index lock is released.
                    for property in external.into_iter().filter(|_| online) {
                        values[property as usize] =
                            property.display(&property::read_path(&path, property, &p));
                    }
                    if online_file && !media.is_empty() {
                        for (property, value) in engine.properties.read(&path, &media, &p) {
                            values[property as usize] = property.display(&value);
                        }
                    }
                    if r.serial == serial.load(Ordering::SeqCst) {
                        deliver(Batch {
                            serial: r.serial,
                            row: r.row,
                            id: r.id,
                            values,
                        });
                    }
                }
            });
        Self {
            sender,
            current,
            progress,
        }
    }
    pub fn serial(&self) -> u64 {
        self.current.load(Ordering::SeqCst)
    }
    pub fn cancel(&self) {
        self.current.fetch_add(1, Ordering::SeqCst);
        let mut p = self.progress.lock().expect("property progress");
        p.cancel();
        *p = Arc::new(Progress::new());
    }
    /// Queues a read; false when the queue is full, so the row asks again
    /// the next time it is shown.
    pub fn request(&self, row: usize, id: NodeId, epoch: u64, columns: Vec<bool>) -> bool {
        self.sender
            .try_send(Request {
                serial: self.serial(),
                row,
                id,
                epoch,
                columns,
            })
            .is_ok()
    }
}

/// Shortest time between two automatic passes of the property indexer.
const REWALK_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

/// Opt-in warming uses the same cache as display and sorting. No UI thread
/// waits for this worker, and unchanged index versions do not rescan.
pub fn start_indexer(engine: Arc<Engine>) {
    let _ = std::thread::Builder::new()
        .name("property-index".into())
        .spawn(move || {
            let mut previous: Option<(u64, u64, u64)> = None;
            let mut last_pass = std::time::Instant::now() - REWALK_EVERY;
            while !engine.property_stopping() {
                std::thread::sleep(std::time::Duration::from_millis(500));
                let options = engine
                    .property_options
                    .lock()
                    .expect("property options")
                    .clone();
                if options.cache_mb == 0 || !options.rules.iter().any(|r| r.enabled) {
                    continue;
                }
                let revision = engine.property_revision.load(Ordering::SeqCst);
                let Ok(index) = engine.index.try_read() else {
                    continue;
                };
                let signature = (index.stamp(), index.version(), revision);
                // A watched index changes often: an index change starts a new
                // pass at most every REWALK_EVERY; Settings requests start at once.
                let requested = previous.is_none_or(|p| p.2 != revision);
                if previous == Some(signature) || (!requested && last_pass.elapsed() < REWALK_EVERY)
                {
                    continue;
                }
                previous = Some(signature);
                last_pass = std::time::Instant::now();
                let ids: Vec<_> = index.order().iter().collect();
                let epoch = engine.epoch();
                drop(index);
                let rules: Vec<_> = crate::property_settings::indexable()
                    .zip(&options.rules)
                    .filter(|(_, r)| r.enabled)
                    .filter_map(|(p, r)| r.filters().ok().map(|filters| (p, r.clone(), filters)))
                    .collect();
                let progress = Arc::new(Progress::new());
                progress.set_total(Some(ids.len() as u64));
                *engine.property_progress.lock().expect("property progress") = progress.clone();
                engine.property_running.store(true, Ordering::SeqCst);
                for id in ids {
                    if progress.is_cancelled()
                        || engine.property_stopping()
                        || engine.property_revision.load(Ordering::SeqCst) != revision
                    {
                        break;
                    }
                    let Ok(index) = engine.index.read() else {
                        break;
                    };
                    let store = index.store();
                    if engine.epoch() != epoch || index.stamp() != signature.0 {
                        break;
                    }
                    if id.index() >= store.len() || !store.is_live(id) || index.is_offline(id) {
                        progress.add(1);
                        continue;
                    }
                    let path = store.path(id);
                    let dir = store.is_dir(id);
                    drop(index);
                    if dir {
                        progress.add(1);
                        continue;
                    }
                    let size = std::fs::symlink_metadata(&path).ok().map(|m| m.len());
                    let media: Vec<Property> = rules
                        .iter()
                        .filter(|(_, rule, filters)| {
                            (rule.max_mb == 0
                                || size.is_some_and(|size| size <= rule.max_mb * 1024 * 1024))
                                && filters.accepts_path(&path, false)
                        })
                        .map(|(property, _, _)| *property)
                        .collect();
                    if !media.is_empty() {
                        let _ = engine.properties.read(&path, &media, &progress);
                    }
                    progress.add(1);
                }
                engine.property_running.store(false, Ordering::SeqCst);
                let _ = engine.properties.save();
            }
        });
}
