//! EveryGrep core: an in-memory file name index with instant substring search.

pub mod collate;
pub mod content;
pub mod details;
pub mod drives;
pub mod dupe;
pub mod filter;
pub mod fold;
pub mod frequency;
pub mod hash;
pub mod hashcache;
pub mod index;
pub mod inotify;
pub mod kind;
mod listing;
pub mod localtime;
pub mod media;
pub mod order;
pub mod persist;
pub mod progress;
pub mod property;
mod property_process;
pub mod query;
pub mod rules;
pub mod scan;
pub mod search;
pub mod sort;
pub mod store;
pub mod sync;
pub mod video;
pub mod watch;

/// Version of the engine, taken from the crate manifest.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }
}
