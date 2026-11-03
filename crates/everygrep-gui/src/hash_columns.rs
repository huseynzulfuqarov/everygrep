//! The MD5, SHA-256 and CRC-32 columns: hashes of the files on screen,
//! computed on a thread of their own and kept in the hash cache.
//!
//! Rows show these cells empty until their hashes come. The thread takes
//! the files asked for, the smallest first (a large file takes long, and
//! the many small ones on screen should not wait for it), reads each once
//! for every hash column shown, and hands the hashes over after each file
//! that took a while, or after a batch of quick ones. A new result list (or
//! another set of columns) cancels the file being read; requests for an
//! older list are dropped. Folders, links and unreadable files have no
//! hashes.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use everygrep_core::hash::{self, HashError, Wanted};
use everygrep_core::hashcache::Cached;
use everygrep_core::progress::Progress;
use everygrep_core::store::NodeId;

use crate::hashes::HashStore;

/// Which hash columns are shown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HashColumns {
    pub md5: bool,
    pub sha256: bool,
    pub crc32: bool,
}

impl HashColumns {
    pub fn any(self) -> bool {
        self.md5 || self.sha256 || self.crc32
    }

    fn wanted(self) -> Wanted {
        Wanted {
            md5: self.md5,
            sha256: self.sha256,
            crc32: self.crc32,
            full: false,
        }
    }

    /// Whether `hashes` has every hash these columns show.
    pub fn covered_by(self, hashes: &Cached) -> bool {
        (!self.md5 || hashes.md5.is_some())
            && (!self.sha256 || hashes.sha256.is_some())
            && (!self.crc32 || hashes.crc32.is_some())
    }
}

/// A file a row on screen needs hashes for.
pub struct HashRequest {
    pub list: u64,
    pub row: usize,
    pub id: NodeId,
    pub path: PathBuf,
    /// Size in the index, to take small files first.
    pub size: u64,
    pub columns: HashColumns,
}

/// Hashes the reader found, for [`crate::results::ResultsModel::take_hashes`].
pub struct HashBatch {
    pub found: Vec<(u64, usize, NodeId, Cached)>,
}

/// Hand over what was found after a file that took this long, or after
/// this many quick ones.
const DELIVER_AFTER: Duration = Duration::from_millis(100);
const DELIVER_EVERY: usize = 64;

/// The thread that hashes files for the hash columns.
pub struct HashReader {
    requests: mpsc::Sender<HashRequest>,
    /// Requests for another list than this one are skipped.
    current: Arc<AtomicU64>,
    /// Cancelled when the list changes, to stop the file being read.
    progress: Arc<Mutex<Arc<Progress>>>,
}

impl HashReader {
    /// Starts the thread; `deliver` gets each batch found, on that thread.
    pub fn start(store: Arc<HashStore>, deliver: impl Fn(HashBatch) + Send + 'static) -> Self {
        let (requests, asked) = mpsc::channel::<HashRequest>();
        let current = Arc::new(AtomicU64::new(0));
        let progress = Arc::new(Mutex::new(Arc::new(Progress::new())));
        let (list, cancel) = (Arc::clone(&current), Arc::clone(&progress));
        let _ = std::thread::Builder::new()
            .name("row-hashes".into())
            .spawn(move || {
                let mut queue: Vec<HashRequest> = Vec::new();
                while let Ok(first) = asked.recv() {
                    queue.push(first);
                    let mut found = Vec::new();
                    let mut since = Instant::now();
                    loop {
                        queue.extend(asked.try_iter());
                        let now = list.load(Ordering::SeqCst);
                        queue.retain(|r| r.list == now);
                        // The smallest last, to be popped first.
                        queue.sort_by_key(|r| std::cmp::Reverse(r.size));
                        let Some(r) = queue.pop() else { break };
                        let progress = Arc::clone(&cancel.lock().expect("progress"));
                        let started = Instant::now();
                        let hashes = hashes_of(&store, &r, &progress).unwrap_or_default();
                        if r.list == list.load(Ordering::SeqCst) {
                            found.push((r.list, r.row, r.id, hashes));
                        }
                        if !found.is_empty()
                            && (started.elapsed() >= DELIVER_AFTER
                                || since.elapsed() >= DELIVER_AFTER
                                || found.len() >= DELIVER_EVERY)
                        {
                            deliver(HashBatch {
                                found: std::mem::take(&mut found),
                            });
                            since = Instant::now();
                        }
                    }
                    if !found.is_empty() {
                        deliver(HashBatch { found });
                    }
                }
            });
        Self {
            requests,
            current,
            progress,
        }
    }

    /// The number of the current list, for requests.
    pub fn list(&self) -> u64 {
        self.current.load(Ordering::SeqCst)
    }

    /// Asks for the hashes of a file; false when the thread is gone.
    pub fn ask(&self, request: HashRequest) -> bool {
        self.requests.send(request).is_ok()
    }

    /// A new list: what was asked for before is dropped, and the file being
    /// read is given up.
    pub fn new_list(&self) {
        self.current.fetch_add(1, Ordering::SeqCst);
        let mut progress = self.progress.lock().expect("progress");
        progress.cancel();
        *progress = Arc::new(Progress::new());
    }
}

/// The hashes of the file of `r`: from the cache when it has all of them,
/// otherwise read (and kept in the cache). Nothing for what is not a
/// readable regular file, or when cancelled.
fn hashes_of(store: &HashStore, r: &HashRequest, progress: &Progress) -> Option<Cached> {
    #[cfg(test)]
    tests::slow_down(&r.path);
    let mut file = hash::open(&r.path).ok()?;
    let key = file.key;
    let known = store.cache().lock().expect("hash cache").get(&key);
    if let Some(known) = known.filter(|k| r.columns.covered_by(k)) {
        return Some(known);
    }
    let read = match hash::hash_file(&mut file, r.columns.wanted(), progress) {
        Ok(read) => read,
        Err(HashError::Cancelled | HashError::NotAFile | HashError::Unreadable) => return None,
    };
    let known = known.unwrap_or_default();
    let hashes = Cached {
        md5: read.md5.or(known.md5),
        sha256: read.sha256.or(known.sha256),
        crc32: read.crc32.or(known.crc32),
        ..known
    };
    let mut cache = store.cache().lock().expect("hash cache");
    cache.put(key, &hashes);
    Some(hashes)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// Paths that take a second to hash, to test cancelling.
    pub static SLOW: StdMutex<Vec<PathBuf>> = StdMutex::new(Vec::new());

    pub fn slow_down(path: &std::path::Path) {
        if SLOW.lock().unwrap().iter().any(|p| p == path) {
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    fn request(
        list: u64,
        row: usize,
        path: PathBuf,
        size: u64,
        columns: HashColumns,
    ) -> HashRequest {
        HashRequest {
            list,
            row,
            id: NodeId(row as u32),
            path,
            size,
            columns,
        }
    }

    #[test]
    fn hash_columns_return_computed_values_when_the_cache_is_disabled() {
        let dir = std::env::temp_dir().join(format!(
            "everygrep-hashcols-disabled-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("abc");
        std::fs::write(&path, b"abc").unwrap();
        let store = HashStore::new(None, 0);
        let columns = HashColumns {
            md5: true,
            sha256: true,
            crc32: true,
        };
        let found = hashes_of(&store, &request(0, 0, path, 3, columns), &Progress::new())
            .expect("computed values");
        assert_eq!(
            hash::hex(&found.md5.unwrap()),
            "900150983cd24fb0d6963f7d28e17f72"
        );
        assert_eq!(
            hash::hex(&found.sha256.unwrap()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(found.crc32, Some(0x3524_41C2));
        assert!(store.cache().lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn files_are_hashed_smallest_first_cached_and_dropped_for_an_old_list() {
        let dir = std::env::temp_dir().join(format!("everygrep-hashcols-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let small = dir.join("small");
        let big = dir.join("big");
        std::fs::write(&small, b"abc").unwrap();
        std::fs::write(&big, vec![1u8; 300_000]).unwrap();
        let store = Arc::new(HashStore::default());
        let (tx, rx) = mpsc::channel();
        let reader = HashReader::start(Arc::clone(&store), move |b| tx.send(b).unwrap());
        let cols = HashColumns {
            md5: true,
            crc32: true,
            ..HashColumns::default()
        };
        // Hold the thread on a slow file so both requests queue up.
        let slow = dir.join("slow");
        std::fs::write(&slow, b"x").unwrap();
        SLOW.lock().unwrap().push(slow.clone());
        assert!(reader.ask(request(0, 9, slow, 1, cols)));
        std::thread::sleep(Duration::from_millis(50));
        assert!(reader.ask(request(0, 0, big.clone(), 300_000, cols)));
        assert!(reader.ask(request(0, 1, small.clone(), 3, cols)));
        let mut rows = Vec::new();
        while rows.len() < 3 {
            let batch = rx.recv_timeout(Duration::from_secs(10)).unwrap();
            rows.extend(batch.found.into_iter().map(|(_, row, _, h)| (row, h)));
        }
        let order: Vec<usize> = rows.iter().map(|r| r.0).collect();
        assert_eq!(order, [9, 1, 0]);
        let abc = rows[1].1;
        assert_eq!(
            hash::hex(&abc.md5.unwrap()),
            "900150983cd24fb0d6963f7d28e17f72"
        );
        assert_eq!(abc.crc32, Some(0x3524_41C2));
        assert_eq!(abc.sha256, None);
        assert_eq!(store.cache().lock().unwrap().len(), 3);
        // A folder has none; an old list's request is dropped.
        assert!(reader.ask(request(0, 2, dir.clone(), 0, cols)));
        let batch = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(batch.found[0].3, Cached::default());
        reader.new_list();
        assert!(reader.ask(request(0, 3, small.clone(), 3, cols)));
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
