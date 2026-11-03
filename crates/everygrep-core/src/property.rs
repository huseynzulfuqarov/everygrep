//! Optional properties. Index-derived values never walk the live filesystem.
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::index::Index;
use crate::progress::{Cancelled, Progress};
use crate::store::NodeId;

/// Stable property ids, independent of translated column titles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Property {
    FullPath,
    ParentPath,
    Stem,
    NameLength,
    PathLength,
    DiskSize,
    TotalSize,
    ChildCount,
    ChildFiles,
    ChildFolders,
    DescCount,
    DescFiles,
    DescFolders,
    FolderDepth,
    HardLinks,
    ShortcutTarget,
    Owner,
}

impl Property {
    pub const ALL: [Self; 17] = [
        Self::FullPath,
        Self::ParentPath,
        Self::Stem,
        Self::NameLength,
        Self::PathLength,
        Self::DiskSize,
        Self::TotalSize,
        Self::ChildCount,
        Self::ChildFiles,
        Self::ChildFolders,
        Self::DescCount,
        Self::DescFiles,
        Self::DescFolders,
        Self::FolderDepth,
        Self::HardLinks,
        Self::ShortcutTarget,
        Self::Owner,
    ];
    pub fn from_column(column: i32) -> Option<Self> {
        usize::try_from(column - 15)
            .ok()
            .and_then(|i| Self::ALL.get(i).copied())
    }
    pub fn column(self) -> i32 {
        15 + self as i32
    }
}

/// Empty values sort first. Numbers sort numerically, text naturally.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Value {
    #[default]
    Empty,
    Number(u64),
    Text(String),
}
impl Ord for Value {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use Value::*;
        match (self, other) {
            (Number(a), Number(b)) => a.cmp(b),
            (Text(a), Text(b)) => {
                crate::collate::collate(a.as_bytes(), b.as_bytes()).then_with(|| a.cmp(b))
            }
            (Empty, Empty) => std::cmp::Ordering::Equal,
            (Empty, _) => std::cmp::Ordering::Less,
            (_, Empty) => std::cmp::Ordering::Greater,
            (Number(_), Text(_)) => std::cmp::Ordering::Less,
            (Text(_), Number(_)) => std::cmp::Ordering::Greater,
        }
    }
}
impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => Ok(()),
            Self::Number(n) => n.fmt(f),
            Self::Text(s) => s.fmt(f),
        }
    }
}
fn text(p: &Path) -> Value {
    Value::Text(p.to_string_lossy().into_owned())
}

/// Read on a worker; links are never followed, offline entries are never opened.
pub fn read(
    index: &Index,
    id: NodeId,
    property: Property,
    progress: &Progress,
) -> Result<Value, Cancelled> {
    progress.check()?;
    let store = index.store();
    if id.index() >= store.len() || !store.is_live(id) {
        return Ok(Value::Empty);
    }
    let path = store.path(id);
    let number = Value::Number;
    use Property::*;
    Ok(match property {
        FullPath => text(&path),
        ParentPath => path.parent().map(text).unwrap_or_default(),
        Stem => Value::Text(if store.is_dir(id) {
            String::from_utf8_lossy(store.name(id)).into_owned()
        } else {
            path.file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        }),
        NameLength => number(String::from_utf8_lossy(store.name(id)).chars().count() as u64),
        PathLength => number(path.to_string_lossy().chars().count() as u64),
        TotalSize => number(store.meta(id).size),
        ChildCount | ChildFiles | ChildFolders | DescCount | DescFiles | DescFolders => {
            if !store.is_dir(id) {
                Value::Empty
            } else {
                let recursive = matches!(property, DescCount | DescFiles | DescFolders);
                let files = matches!(property, ChildFiles | DescFiles);
                let folders = matches!(property, ChildFolders | DescFolders);
                let mut pending = vec![id];
                let mut count = 0;
                while let Some(parent) = pending.pop() {
                    progress.check()?;
                    for child in store.children(parent) {
                        let dir = store.is_dir(child);
                        if (!files && !folders) || (files && !dir) || (folders && dir) {
                            count += 1;
                        }
                        if recursive && dir {
                            pending.push(child);
                        }
                    }
                }
                number(count)
            }
        }
        FolderDepth => number(
            path.components()
                .filter(|c| matches!(c, std::path::Component::Normal(_)))
                .count()
                .saturating_sub(usize::from(!store.is_dir(id))) as u64,
        ),
        DiskSize | HardLinks | ShortcutTarget | Owner => {
            if index.is_offline(id) {
                Value::Empty
            } else if let Ok(meta) = std::fs::symlink_metadata(&path) {
                match property {
                    DiskSize => number(meta.blocks().saturating_mul(512)),
                    HardLinks => number(meta.nlink()),
                    ShortcutTarget if meta.is_symlink() => std::fs::read_link(&path)
                        .ok()
                        .as_deref()
                        .map(text)
                        .unwrap_or_default(),
                    Owner => Value::Text(owner(meta.uid())),
                    _ => Value::Empty,
                }
            } else {
                Value::Empty
            }
        }
    })
}

/// Resolve through NSS on a worker. A numeric uid remains useful without getent.
fn owner(uid: u32) -> String {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static USERS: OnceLock<Mutex<HashMap<u32, String>>> = OnceLock::new();
    let users = USERS.get_or_init(Default::default);
    if let Some(name) = users.lock().expect("users").get(&uid) {
        return name.clone();
    }
    let name = crate::property_process::output(
        "getent",
        &[
            std::ffi::OsStr::new("passwd"),
            std::ffi::OsStr::new(&uid.to_string()),
        ],
        std::time::Duration::from_millis(200),
        &Progress::new(),
    )
    .and_then(|bytes| String::from_utf8(bytes).ok())
    .and_then(|s| {
        s.split(':')
            .next()
            .filter(|n| !n.is_empty())
            .map(str::to_owned)
    })
    .unwrap_or_else(|| uid.to_string());
    let mut cache = users.lock().expect("users");
    if cache.len() >= 1024 {
        cache.clear();
    }
    cache.insert(uid, name.clone());
    name
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Meta;
    #[test]
    fn indexed_values_and_links() {
        let path =
            std::env::temp_dir().join(format!("everygrep-properties-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        let file = path.join("hello.txt");
        std::fs::write(&file, b"hello").unwrap();
        std::os::unix::fs::symlink("hello.txt", path.join("link")).unwrap();
        let mut store = crate::store::NodeStore::new();
        let root = store.add(
            None,
            path.as_os_str().as_encoded_bytes(),
            true,
            Meta::default(),
        );
        let f = store.add(
            Some(root),
            b"hello.txt",
            false,
            Meta {
                size: 5,
                ..Meta::default()
            },
        );
        let link = store.add(Some(root), b"link", false, Meta::default());
        let index = Index::new(store);
        let p = Progress::new();
        assert_eq!(
            read(&index, f, Property::Stem, &p).unwrap().to_string(),
            "hello"
        );
        assert_eq!(
            read(&index, root, Property::ChildCount, &p).unwrap(),
            Value::Number(2)
        );
        assert_eq!(
            read(&index, root, Property::DescFiles, &p).unwrap(),
            Value::Number(2)
        );
        assert_eq!(
            read(&index, f, Property::NameLength, &p).unwrap(),
            Value::Number(9)
        );
        assert_eq!(
            read(&index, link, Property::ShortcutTarget, &p)
                .unwrap()
                .to_string(),
            "hello.txt"
        );
        assert!(matches!(
            read(&index, f, Property::HardLinks, &p).unwrap(),
            Value::Number(1)
        ));
        p.cancel();
        assert_eq!(read(&index, f, Property::Stem, &p), Err(Cancelled));
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn numeric_order_and_column_identity() {
        assert!(Value::Number(9) < Value::Number(10));
        assert!(Value::Text("file2".into()) < Value::Text("file10".into()));
        for p in Property::ALL {
            assert_eq!(Property::from_column(p.column()), Some(p));
        }
        assert_eq!(Property::from_column(14), None);
    }
}
