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
                    let online_file = !index.is_offline(r.id) && !index.store().is_dir(r.id);
                    let mut media = Vec::new();
                    for (i, property) in Property::ALL.iter().enumerate() {
                        if r.columns.get(i) == Some(&true) {
                            if property.is_media() {
                                media.push(*property);
                            } else {
                                values[i] = property::read(&index, r.id, *property, &p)
                                    .unwrap_or_default()
                                    .to_string();
                            }
                        }
                    }
                    drop(index);
                    if online_file && !media.is_empty() {
                        for (property, value) in everygrep_core::media::read(&path, &media, &p) {
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
