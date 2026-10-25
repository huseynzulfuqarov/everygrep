//! EveryGrep core: an in-memory file name index with instant substring search.

pub mod fold;
pub mod index;
pub mod matcher;
pub mod scan;
pub mod search;
pub mod store;

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
