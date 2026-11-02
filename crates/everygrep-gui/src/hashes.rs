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
    /// `None`: kept in memory only.
    path: Option<PathBuf>,
    cache: Mutex<HashCache>,
    loaded: Once,
    /// Whether the file was read (or there is none).
    ready: AtomicBool,
    max_bytes: AtomicU64,
}

impl HashStore {
    /// A cache kept in the file `path`, at most `max_bytes` large.
    pub fn new(path: Option<PathBuf>, max_bytes: u64) -> Self {
        Self {
            path,
            cache: Mutex::new(HashCache::new(max_bytes)),
            loaded: Once::new(),
            ready: AtomicBool::new(false),
            max_bytes: AtomicU64::new(max_bytes),
        }
    }

    /// The cache, read from its file first if this is the first use. A file
    /// that cannot be read is ignored (and replaced at the next save).
    pub fn cache(&self) -> &Mutex<HashCache> {
        self.loaded.call_once(|| {
            let max = self.max_bytes.load(Ordering::SeqCst);
            if let Some(path) = &self.path {
                match HashCache::load(path, max) {
                    Ok(loaded) => {
                        let mut cache = self.cache.lock().expect("hash cache");
                        // Entries added before the load (none normally) stay.
                        if cache.is_empty() {
                            *cache = loaded;
                        }
                    }
                    Err(e) => eprintln!("hash cache {}: {e}", path.display()),
                }
            }
            self.ready.store(true, Ordering::SeqCst);
        });
        &self.cache
    }

    /// Changes the size limit (0 keeps nothing), dropping entries now when
    /// the cache was read already.
    pub fn set_max_bytes(&self, max_bytes: u64) {
        self.max_bytes.store(max_bytes, Ordering::SeqCst);
        self.cache
            .lock()
            .expect("hash cache")
            .set_max_bytes(max_bytes);
    }

    /// Forgets every hash and removes the file.
    pub fn clear(&self) -> io::Result<()> {
        let mut cache = self.cache().lock().expect("hash cache");
        cache.clear();
        match &self.path {
            Some(path) => cache.save(path),
            None => Ok(()),
        }
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
        self.path
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok())
            .map_or(0, |m| m.len())
    }

    /// Writes the file when the cache changed since it was read or saved.
    /// The bytes are taken under the lock and written without it.
    pub fn save(&self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if !self.ready.load(Ordering::SeqCst) {
            return Ok(());
        }
        let bytes = {
            let mut cache = self.cache.lock().expect("hash cache");
            if !cache.is_changed() {
                return Ok(());
            }
            cache.set_changed(false);
            (!cache.is_empty()).then(|| cache.to_bytes())
        };
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
        // A limit of 0 keeps nothing.
        next.set_max_bytes(0);
        next.cache().lock().unwrap().put(key, &hashes);
        assert!(next.cache().lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
