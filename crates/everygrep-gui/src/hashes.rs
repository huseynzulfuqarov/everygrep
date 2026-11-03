//! The window's hash cache (see [`everygrep_core::hashcache`]): loaded from
//! its file the first time a search or a hash column needs it, on that
//! thread, so the window starts without reading it, and saved with the
//! index.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, Once};

use everygrep_core::hashcache::{DEFAULT_MAX_BYTES, HashCache};
use everygrep_core::persist;

pub struct HashStore {
    /// Serializes file publication with clears and destination changes.
    publication: Mutex<()>,
    /// `None`: kept in memory only.
    path: Mutex<Option<PathBuf>>,
    cache: Mutex<HashCache>,
    loaded: Once,
    /// Whether the file was read (or there is none).
    ready: AtomicBool,
    max_bytes: AtomicU64,
    #[cfg(test)]
    before_publish: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    before_clear: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl HashStore {
    /// A cache kept in the file `path`, at most `max_bytes` large.
    pub fn new(path: Option<PathBuf>, max_bytes: u64) -> Self {
        Self {
            publication: Mutex::new(()),
            path: Mutex::new(path),
            cache: Mutex::new(HashCache::new(max_bytes)),
            loaded: Once::new(),
            ready: AtomicBool::new(false),
            max_bytes: AtomicU64::new(max_bytes),
            #[cfg(test)]
            before_publish: Mutex::new(None),
            #[cfg(test)]
            before_clear: Mutex::new(None),
        }
    }

    /// The cache, read from its file first if this is the first use. A file
    /// that cannot be read is ignored (and replaced at the next save).
    pub fn cache(&self) -> &Mutex<HashCache> {
        self.loaded.call_once(|| {
            // Held while the file is read, so a clear meanwhile waits.
            let mut cache = self.cache.lock().expect("hash cache");
            let max = self.max_bytes.load(Ordering::SeqCst);
            if let Some(path) = self.path() {
                match HashCache::load(&path, max) {
                    Ok(loaded) => *cache = loaded,
                    Err(e) => eprintln!("hash cache {}: {e}", path.display()),
                }
            }
            cache.set_max_bytes(max);
            self.ready.store(true, Ordering::SeqCst);
        });
        &self.cache
    }

    /// Changes the size limit (0 keeps nothing). It applies when the cache
    /// is next saved or read, not at once: the settings change it on every
    /// value typed, and a value on the way (`5` before `512`) must not drop
    /// entries.
    pub fn set_max_bytes(&self, max_bytes: u64) {
        self.max_bytes.store(max_bytes, Ordering::SeqCst);
    }

    /// Forgets every hash and removes the file, without reading it first.
    pub fn clear(&self) -> io::Result<()> {
        let _publication = self.publication.lock().expect("hash publication");
        // Keep first-use loading out until the old file is removed too.
        let mut cache = self.cache.lock().expect("hash cache");
        cache.clear();
        cache.set_changed(false);
        #[cfg(test)]
        if let Some(hook) = self.before_clear.lock().unwrap().take() {
            hook();
        }
        match self.path() {
            Some(path) => match std::fs::remove_file(path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            },
            None => Ok(()),
        }
    }

    /// Clears on a helper thread of its own and reports the result and usage
    /// there. Not on the settings thread: a clear that waits for a slow
    /// cache read or save must not hold up the settings saves after it.
    pub fn clear_later(
        self: &std::sync::Arc<Self>,
        done: impl FnOnce(io::Result<()>, u64) + Send + 'static,
    ) {
        let store = std::sync::Arc::clone(self);
        std::thread::spawn(move || {
            let result = store.clear();
            done(result, store.bytes());
        });
    }

    /// Bytes the hashes take in the file: those in memory once it was read,
    /// otherwise the size of the file.
    pub fn bytes(&self) -> u64 {
        if self.ready.load(Ordering::SeqCst) {
            let cache = self.cache.lock().expect("hash cache");
            return if cache.is_empty() {
                0
            } else {
                cache.bytes() + 16
            };
        }
        self.path()
            .and_then(|p| std::fs::metadata(p).ok())
            .map_or(0, |m| m.len())
    }

    fn path(&self) -> Option<PathBuf> {
        self.path.lock().expect("hash cache path").clone()
    }

    /// Keeps the cache in the file `to` from now on. A file not read yet is
    /// moved there (left behind when that fails: it is only a cache);
    /// one read already is written there at the next save and the old one
    /// removed.
    pub fn move_to(&self, to: PathBuf) {
        let _publication = self.publication.lock().expect("hash publication");
        // Held, so a first use waits and then reads the new place.
        let mut cache = self.cache.lock().expect("hash cache");
        let Some(old) = self
            .path
            .lock()
            .expect("hash cache path")
            .replace(to.clone())
        else {
            return;
        };
        if old == to {
            return;
        }
        if self.ready.load(Ordering::SeqCst) {
            if !cache.is_empty() {
                cache.set_changed(true);
            }
            let _ = std::fs::remove_file(&old);
        } else if std::fs::rename(&old, &to).is_err() {
            let _ = std::fs::remove_file(&old);
        }
    }

    /// Writes the file when the cache changed since it was read or saved.
    /// The bytes are taken under the lock and written without it.
    pub fn save(&self) -> io::Result<()> {
        let _publication = self.publication.lock().expect("hash publication");
        let Some(path) = self.path() else {
            return Ok(());
        };
        let path = &path;
        if !self.ready.load(Ordering::SeqCst) {
            return Ok(());
        }
        let bytes = {
            let mut cache = self.cache.lock().expect("hash cache");
            let max = self.max_bytes.load(Ordering::SeqCst);
            if cache.max_bytes() != max {
                cache.set_max_bytes(max);
            }
            if !cache.is_changed() {
                return Ok(());
            }
            cache.set_changed(false);
            (!cache.is_empty()).then(|| cache.to_bytes())
        };
        #[cfg(test)]
        if let Some(hook) = self.before_publish.lock().unwrap().take() {
            hook();
        }
        let written = match bytes {
            Some(bytes) => persist::write_atomically(path, &bytes),
            None => match std::fs::remove_file(path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            },
        };
        if written.is_err() {
            self.cache.lock().expect("hash cache").set_changed(true);
        }
        written
    }
}

impl Default for HashStore {
    fn default() -> Self {
        Self::new(None, DEFAULT_MAX_BYTES)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use everygrep_core::hash::FileKey;
    use everygrep_core::hashcache::Cached;

    #[test]
    fn first_use_cannot_reload_the_file_while_clear_is_removing_it() {
        use std::sync::{Arc, mpsc};
        use std::time::Duration;
        let dir = std::env::temp_dir().join(format!("everygrep-clear-load-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hashes.bin");
        let seeded = HashStore::new(Some(path.clone()), DEFAULT_MAX_BYTES);
        seeded.cache().lock().unwrap().put(
            FileKey::default(),
            &Cached {
                crc32: Some(1),
                ..Cached::default()
            },
        );
        seeded.save().unwrap();
        let store = Arc::new(HashStore::new(Some(path.clone()), DEFAULT_MAX_BYTES));
        let (paused_tx, paused) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        *store.before_clear.lock().unwrap() = Some(Box::new(move || {
            paused_tx.send(()).unwrap();
            blocked.recv_timeout(Duration::from_secs(5)).unwrap();
        }));
        let clearing = Arc::clone(&store);
        let clear = std::thread::spawn(move || clearing.clear().unwrap());
        paused.recv_timeout(Duration::from_secs(5)).unwrap();
        let loading = Arc::clone(&store);
        let (done_tx, done) = mpsc::channel();
        let load = std::thread::spawn(move || {
            let empty = loading.cache().lock().unwrap().is_empty();
            done_tx.send(empty).unwrap();
        });
        let early = done.recv_timeout(Duration::from_millis(200)).ok();
        release.send(()).unwrap();
        clear.join().unwrap();
        load.join().unwrap();
        let empty = early.unwrap_or_else(|| done.recv_timeout(Duration::from_secs(5)).unwrap());
        let _ = std::fs::remove_dir_all(dir);
        assert!(empty, "first use reloaded hashes that clear was removing");
    }

    #[test]
    fn clearing_waits_for_an_older_publication_and_does_not_resurrect_it() {
        use std::sync::{Arc, mpsc};
        use std::time::Duration;
        let dir =
            std::env::temp_dir().join(format!("everygrep-hash-publication-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hashes.bin");
        let store = Arc::new(HashStore::new(Some(path.clone()), DEFAULT_MAX_BYTES));
        store.cache().lock().unwrap().put(
            FileKey {
                ino: 1,
                ..FileKey::default()
            },
            &Cached {
                crc32: Some(7),
                ..Cached::default()
            },
        );
        let (paused_tx, paused) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        *store.before_publish.lock().unwrap() = Some(Box::new(move || {
            paused_tx.send(()).unwrap();
            blocked.recv_timeout(Duration::from_secs(5)).unwrap();
        }));
        let saving = Arc::clone(&store);
        let save = std::thread::spawn(move || saving.save().unwrap());
        paused.recv_timeout(Duration::from_secs(5)).unwrap();
        let clearing = Arc::clone(&store);
        let (cleared_tx, cleared) = mpsc::channel();
        let clear = std::thread::spawn(move || {
            clearing.clear().unwrap();
            cleared_tx.send(()).unwrap();
        });
        // The older snapshot is held until clear has had time to run.
        let early_clear = cleared.recv_timeout(Duration::from_millis(200)).is_ok();
        release.send(()).unwrap();
        save.join().unwrap();
        clear.join().unwrap();
        let resurrected = path.exists();
        let empty = store.cache().lock().unwrap().is_empty();
        let _ = std::fs::remove_dir_all(dir);
        assert!(!resurrected, "an older save resurrected the cleared cache");
        assert!(empty);
        assert!(
            !early_clear,
            "clear completed while an older publication could still write"
        );
    }

    #[test]
    fn a_clear_holds_up_neither_the_caller_nor_later_settings_saves() {
        use std::sync::{Arc, mpsc};
        use std::time::{Duration, Instant};
        let store = Arc::new(HashStore::default());
        let held = Arc::clone(&store);
        let (ready_tx, ready) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let _cache = held.cache().lock().unwrap();
            ready_tx.send(()).unwrap();
            let _ = blocked.recv_timeout(Duration::from_secs(2));
        });
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        let writer = crate::settings::Writer::start();
        let (done_tx, done) = mpsc::channel();
        let start = Instant::now();
        store.clear_later(move |result, used| done_tx.send((result.is_ok(), used)).unwrap());
        let elapsed = start.elapsed();
        // A settings save asked after the clear is not held up by it.
        writer.run(|| {});
        let saves_done = writer.wait_done(Duration::from_millis(500));
        let waiting = done.try_recv().is_err();
        let _ = release.send(());
        reader.join().unwrap();
        assert!(
            elapsed < Duration::from_millis(300),
            "clear took {elapsed:?}"
        );
        assert!(saves_done, "a later settings save waited for the clear");
        assert!(
            waiting,
            "clear should still have been queued behind the read"
        );
        assert_eq!(
            done.recv_timeout(Duration::from_secs(5)).unwrap(),
            (true, 0)
        );
    }

    #[test]
    fn moving_takes_the_file_or_the_cache_along() {
        let dir =
            std::env::temp_dir().join(format!("everygrep-hashes-move-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("b")).unwrap();
        let (a, b, c) = (dir.join("a.bin"), dir.join("b/h.bin"), dir.join("c.bin"));
        let key = FileKey {
            dev: 1,
            ino: 2,
            size: 3,
            mtime_ns: 4,
        };
        let hashes = Cached {
            crc32: Some(5),
            ..Cached::default()
        };
        let first = HashStore::new(Some(a.clone()), DEFAULT_MAX_BYTES);
        first.cache().lock().unwrap().put(key, &hashes);
        first.save().unwrap();
        // Not read yet: the file moves.
        let store = HashStore::new(Some(a.clone()), DEFAULT_MAX_BYTES);
        store.move_to(b.clone());
        assert!(!a.exists() && b.exists());
        assert_eq!(store.cache().lock().unwrap().get(&key), Some(hashes));
        // Read already: written at the new place by the next save.
        store.move_to(c.clone());
        assert!(!b.exists());
        store.save().unwrap();
        let moved = HashStore::new(Some(c.clone()), DEFAULT_MAX_BYTES);
        assert_eq!(moved.cache().lock().unwrap().get(&key), Some(hashes));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_file_is_read_on_first_use_saved_when_changed_and_cleared() {
        let dir = std::env::temp_dir().join(format!("everygrep-hashes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("hashes.bin");
        let key = FileKey {
            dev: 1,
            ino: 2,
            size: 3,
            mtime_ns: 4,
        };
        let hashes = Cached {
            crc32: Some(5),
            ..Cached::default()
        };
        let store = HashStore::new(Some(path.clone()), DEFAULT_MAX_BYTES);
        assert_eq!(store.bytes(), 0);
        // Nothing read, nothing to save.
        store.save().unwrap();
        assert!(!path.exists());
        store.cache().lock().unwrap().put(key, &hashes);
        store.save().unwrap();
        let size = std::fs::metadata(&path).unwrap().len();
        assert_eq!(store.bytes(), size);
        // A new window reads it when it first needs it.
        let next = HashStore::new(Some(path.clone()), DEFAULT_MAX_BYTES);
        assert_eq!(next.bytes(), size);
        assert_eq!(next.cache().lock().unwrap().get(&key), Some(hashes));
        // Unchanged: not written again.
        std::fs::remove_file(&path).unwrap();
        next.save().unwrap();
        assert!(!path.exists());
        next.cache().lock().unwrap().put(key, &hashes);
        next.save().unwrap();
        assert!(path.exists());
        next.clear().unwrap();
        assert!(!path.exists());
        assert_eq!(next.bytes(), 0);
        // A new limit applies at the save: 0 keeps nothing.
        next.cache().lock().unwrap().put(key, &hashes);
        next.set_max_bytes(0);
        assert_eq!(next.cache().lock().unwrap().len(), 1);
        next.save().unwrap();
        assert!(next.cache().lock().unwrap().is_empty());
        assert!(!path.exists());
        // Clearing before the file was read removes it without reading it.
        let fresh = HashStore::new(Some(path.clone()), DEFAULT_MAX_BYTES);
        fresh.cache().lock().unwrap().put(key, &hashes);
        fresh.save().unwrap();
        let unread = HashStore::new(Some(path.clone()), DEFAULT_MAX_BYTES);
        unread.clear().unwrap();
        assert!(!path.exists());
        assert!(unread.cache().lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
