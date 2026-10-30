//! EveryGrep core: an in-memory file name index with instant substring search.

pub mod drives;
pub mod filter;
pub mod fold;
pub mod index;
pub mod inotify;
pub mod kind;
pub mod localtime;
pub mod order;
pub mod persist;
pub mod query;
pub mod scan;
pub mod search;
pub mod sort;
pub mod store;
pub mod sync;
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
