//! Persistent typed metadata cache. File identity includes change time and XMP.
use crate::progress::Progress;
use crate::property::{Property, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

pub const DEFAULT_MAX_BYTES: u64 = 256 * 1024 * 1024;
const MAGIC: &[u8; 8] = b"EGPRP001";
type Key = [u64; 12];
fn fingerprint(m: &fs::Metadata) -> [u64; 6] {
    [
        m.dev(),
        m.ino(),
        m.len(),
        m.mtime() as u64,
        m.mtime_nsec() as u64,
        (m.ctime() as u64)
            .wrapping_mul(1_000_000_000)
            .wrapping_add(m.ctime_nsec() as u64),
    ]
}
fn key(path: &Path) -> Option<Key> {
    let m = fs::symlink_metadata(path).ok()?;
    if !m.is_file() && !m.is_dir() && !m.is_symlink() {
        return None;
    }
    let mut k = [0; 12];
    k[..6].copy_from_slice(&fingerprint(&m));
    if let Ok(m) = fs::symlink_metadata(path.with_extension("xmp"))
        && m.is_file()
    {
        k[6..].copy_from_slice(&fingerprint(&m));
    }
    Some(k)
}
#[derive(Clone)]
struct Entry {
    values: BTreeMap<u8, Value>,
}
impl Entry {
    fn bytes(&self) -> u64 {
        128 + self
            .values
            .values()
            .map(|v| match v {
                Value::Text(t) => 16 + t.len() as u64,
                _ => 16,
            })
            .sum::<u64>()
    }
}
struct State {
    entries: BTreeMap<Key, Entry>,
    path: Option<PathBuf>,
    max: u64,
    loaded: bool,
    dirty: bool,
    bytes: u64,
    revision: u64,
}
/// All disk operations run on callers' workers. Publication, clears and loading
/// use the same mutex so a clear cannot be undone by an older save or load.
pub struct Store {
    state: Mutex<State>,
    max: AtomicU64,
}
impl Default for Store {
    fn default() -> Self {
        Self::new(None, DEFAULT_MAX_BYTES)
    }
}
impl Store {
    pub fn new(path: Option<PathBuf>, max: u64) -> Self {
        Self {
            state: Mutex::new(State {
                entries: BTreeMap::new(),
                path,
                max,
                loaded: false,
                dirty: false,
                bytes: 0,
                revision: 0,
            }),
            max: AtomicU64::new(max),
        }
    }
    pub fn move_to(&self, path: PathBuf) {
        let mut s = self.state.lock().expect("property cache");
        s.path = Some(path);
        s.dirty = true;
    }
    pub fn set_max_bytes(&self, max: u64) {
        self.max.store(max, Ordering::SeqCst);
    }
    pub fn clear(&self) -> io::Result<()> {
        let mut s = self.state.lock().expect("property cache");
        s.revision = s.revision.wrapping_add(1);
        s.entries.clear();
        s.bytes = 0;
        s.loaded = true;
        s.dirty = false;
        if let Some(path) = &s.path {
            match fs::remove_file(path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        Ok(())
    }
    pub fn read(
        &self,
        path: &Path,
        wanted: &[Property],
        progress: &Progress,
    ) -> Vec<(Property, Value)> {
        self.read_with(path, wanted, progress, |missing| {
            crate::media::read(path, missing, progress)
        })
    }
    pub fn read_with(
        &self,
        path: &Path,
        wanted: &[Property],
        progress: &Progress,
        load: impl FnOnce(&[Property]) -> Vec<(Property, Value)>,
    ) -> Vec<(Property, Value)> {
        let Some(before) = key(path) else {
            return Vec::new();
        };
        let mut found = Vec::new();
        let mut missing = Vec::new();
        let revision;
        {
            let mut s = self.state.lock().expect("property cache");
            s.max = self.max.load(Ordering::SeqCst);
            s.load();
            s.trim();
            revision = s.revision;
            let max = s.max;
            if let Some(entry) = s.entries.get_mut(&before).filter(|_| max > 0) {
                for &p in wanted {
                    if let Some(v) = entry.values.get(&(p as u8)) {
                        found.push((p, v.clone()));
                    } else {
                        missing.push(p);
                    }
                }
            } else {
                missing.extend(wanted);
            }
        }
        if missing.is_empty() {
            return found;
        }
        let fresh = load(&missing);
        if progress.is_cancelled() || key(path) != Some(before) {
            return Vec::new();
        }
        // A failed/over-budget property remains uncached, allowing an explicit
        // refresh to retry; available values are shared by display and sort.
        let mut s = self.state.lock().expect("property cache");
        s.max = self.max.load(Ordering::SeqCst);
        if s.revision == revision && s.max > 0 && !fresh.is_empty() {
            let old = s.entries.get(&before).map_or(0, Entry::bytes);
            let entry = s.entries.entry(before).or_insert_with(|| Entry {
                values: BTreeMap::new(),
            });
            for (p, v) in &fresh {
                entry.values.insert(*p as u8, v.clone());
            }
            let new = entry.bytes();
            s.bytes = s.bytes.saturating_sub(old) + new;
            s.dirty = true;
            s.trim();
        }
        found.extend(fresh);
        found
    }
    pub fn save(&self) -> io::Result<()> {
        let mut s = self.state.lock().expect("property cache");
        if !s.loaded {
            return Ok(());
        }
        s.max = self.max.load(Ordering::SeqCst);
        s.trim();
        if !s.dirty {
            return Ok(());
        }
        if s.entries.is_empty() {
            if let Some(path) = &s.path {
                match fs::remove_file(path) {
                    Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                    _ => {}
                }
            }
            s.dirty = false;
            return Ok(());
        }
        if let Some(path) = &s.path {
            let mut bytes = MAGIC.to_vec();
            bytes.extend((s.entries.len() as u64).to_le_bytes());
            for (k, e) in &s.entries {
                for n in k {
                    bytes.extend(n.to_le_bytes());
                }
                bytes.push(e.values.len() as u8);
                for (p, v) in &e.values {
                    bytes.push(*p);
                    match v {
                        Value::Empty => bytes.push(0),
                        Value::Number(n) => {
                            bytes.push(1);
                            bytes.extend(n.to_le_bytes());
                        }
                        Value::Text(t) => {
                            bytes.push(2);
                            bytes.extend((t.len() as u32).to_le_bytes());
                            bytes.extend(t.as_bytes());
                        }
                        Value::Signed(n) => {
                            bytes.push(3);
                            bytes.extend(n.to_le_bytes());
                        }
                        Value::Decimal(n) => {
                            bytes.push(4);
                            bytes.extend(n.to_le_bytes());
                        }
                    }
                }
            }
            if bytes.len() as u64 > s.max {
                return Err(io::Error::other("property cache limit"));
            }
            crate::persist::write_atomically(path, &bytes)?;
        }
        s.dirty = false;
        Ok(())
    }
}
impl State {
    fn trim(&mut self) {
        while self.bytes > self.max || self.entries.len() > 100_000 {
            let Some((_, e)) = self.entries.pop_first() else {
                // Nothing is left to count.
                self.bytes = 0;
                break;
            };
            self.bytes = self.bytes.saturating_sub(e.bytes());
            self.dirty = true;
        }
    }
    fn load(&mut self) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        let Some(path) = &self.path else {
            return;
        };
        let Ok(m) = fs::symlink_metadata(path) else {
            return;
        };
        if !m.is_file() || m.len() > self.max || m.len() < 16 {
            return;
        }
        let Ok(file) = fs::File::open(path) else {
            return;
        };
        let mut bytes = Vec::new();
        if file.take(self.max + 1).read_to_end(&mut bytes).is_err() || bytes.len() as u64 > self.max
        {
            return;
        }
        if let Some(entries) = decode(&bytes) {
            self.entries = entries;
            self.bytes = self.entries.values().map(Entry::bytes).sum();
            self.trim();
        }
    }
}
fn decode(mut b: &[u8]) -> Option<BTreeMap<Key, Entry>> {
    fn take<'a>(b: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
        let (a, rest) = b.split_at_checked(n)?;
        *b = rest;
        Some(a)
    }
    fn u64le(b: &mut &[u8]) -> Option<u64> {
        Some(u64::from_le_bytes(take(b, 8)?.try_into().ok()?))
    }
    if take(&mut b, 8)? != MAGIC {
        return None;
    }
    let count = u64le(&mut b)?;
    if count > 100_000 {
        return None;
    }
    let mut entries = BTreeMap::new();
    for _ in 0..count {
        let mut k = [0; 12];
        for n in &mut k {
            *n = u64le(&mut b)?;
        }
        let count = *take(&mut b, 1)?.first()? as usize;
        if count > Property::ALL.len() {
            return None;
        }
        let mut values = BTreeMap::new();
        for _ in 0..count {
            let p = take(&mut b, 1)?[0];
            if p as usize >= Property::ALL.len() {
                return None;
            }
            let v = match take(&mut b, 1)?[0] {
                0 => Value::Empty,
                1 => Value::Number(u64le(&mut b)?),
                2 => {
                    let len = u32::from_le_bytes(take(&mut b, 4)?.try_into().ok()?) as usize;
                    if len > 16384 {
                        return None;
                    }
                    Value::Text(std::str::from_utf8(take(&mut b, len)?).ok()?.to_owned())
                }
                3 => Value::Signed(u64le(&mut b)? as i64),
                4 => Value::Decimal(u64le(&mut b)? as i64),
                _ => return None,
            };
            if values.insert(p, v).is_some() {
                return None;
            }
        }
        if entries.insert(k, Entry { values }).is_some() {
            return None;
        }
    }
    b.is_empty().then_some(entries)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cache_round_trip_invalidation_limit_and_clear() {
        let dir =
            std::env::temp_dir().join(format!("everygrep-property-cache-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.txt");
        let cache = dir.join("properties.bin");
        fs::write(&path, b"one two").unwrap();
        let store = Store::new(Some(cache.clone()), 8192);
        let p = Progress::new();
        assert_eq!(
            store.read(&path, &[Property::WordCount], &p),
            vec![(Property::WordCount, Value::Number(2))]
        );
        store.save().unwrap();
        let loaded = Store::new(Some(cache.clone()), 8192);
        assert_eq!(
            loaded.read(&path, &[Property::WordCount], &p),
            vec![(Property::WordCount, Value::Number(2))]
        );
        fs::write(&path, b"one two three").unwrap();
        assert_eq!(
            loaded.read(&path, &[Property::WordCount], &p),
            vec![(Property::WordCount, Value::Number(3))]
        );
        loaded.set_max_bytes(0);
        loaded.save().unwrap();
        assert_eq!(loaded.state.lock().unwrap().bytes, 0);
        loaded.clear().unwrap();
        assert!(!cache.exists());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn a_clear_discards_an_inflight_insert_and_cache_hits_skip_the_reader() {
        use std::sync::{Arc, mpsc};
        let dir =
            std::env::temp_dir().join(format!("everygrep-property-flight-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.txt");
        fs::write(&path, b"one").unwrap();
        let store = Arc::new(Store::new(None, 8192));
        let (ready_tx, ready) = mpsc::channel();
        let (release_tx, release) = mpsc::channel();
        let worker = store.clone();
        let file = path.clone();
        let thread = std::thread::spawn(move || {
            worker.read_with(&file, &[Property::WordCount], &Progress::new(), |_| {
                ready_tx.send(()).unwrap();
                release
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap();
                vec![(Property::WordCount, Value::Number(1))]
            })
        });
        ready
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        store.clear().unwrap();
        release_tx.send(()).unwrap();
        thread.join().unwrap();
        assert!(store.state.lock().unwrap().entries.is_empty());
        let p = Progress::new();
        store.read(&path, &[Property::WordCount], &p);
        assert_eq!(
            store.read_with(&path, &[Property::WordCount], &p, |_| panic!(
                "cached value must not read"
            )),
            vec![(Property::WordCount, Value::Number(1))]
        );
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn trimming_an_empty_cache_ends() {
        let mut s = State {
            entries: BTreeMap::new(),
            path: None,
            max: 0,
            loaded: true,
            dirty: false,
            bytes: 10,
            revision: 0,
        };
        s.trim();
        assert_eq!(s.bytes, 0);
    }

    #[test]
    fn malformed_cache_is_rejected() {
        assert!(decode(b"broken").is_none());
        let mut b = MAGIC.to_vec();
        b.extend(u64::MAX.to_le_bytes());
        assert!(decode(&b).is_none());
    }
}
