//! Deterministic filesystem interference in temp-directory tests.
use std::cell::RefCell;
use std::path::{Path, PathBuf};
type Callback = Box<dyn FnOnce(&Path)>;
struct Hook {
    event: &'static str,
    path: Option<PathBuf>,
    callback: Callback,
}
thread_local! { static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) }; }
fn set(event: &'static str, path: Option<PathBuf>, callback: impl FnOnce(&Path) + 'static) {
    HOOK.with(|h| {
        assert!(
            h.borrow().is_none(),
            "previous filesystem hook was not reached"
        );
        *h.borrow_mut() = Some(Hook {
            event,
            path,
            callback: Box::new(callback),
        });
    });
}
pub(crate) fn arm(event: &'static str, callback: impl FnOnce(&Path) + 'static) {
    set(event, None, callback);
}
pub(crate) fn arm_path(event: &'static str, path: &Path, callback: impl FnOnce(&Path) + 'static) {
    set(event, Some(path.into()), callback);
}
pub(crate) fn at(event: &'static str, path: &Path) {
    let callback = HOOK.with(|h| {
        let mut h = h.borrow_mut();
        if h.as_ref()
            .is_some_and(|h| h.event == event && h.path.as_deref().is_none_or(|p| p == path))
        {
            h.take().map(|h| h.callback)
        } else {
            None
        }
    });
    if let Some(callback) = callback {
        callback(path);
    }
}
pub(crate) fn fired() {
    HOOK.with(|h| assert!(h.borrow().is_none(), "filesystem hook was not reached"));
}

thread_local! { static LOG: RefCell<Option<Vec<(&'static str, PathBuf)>>> = const { RefCell::new(None) }; }
/// Starts recording the [`note`]d steps of this thread.
pub(crate) fn start_log() {
    LOG.with(|l| *l.borrow_mut() = Some(Vec::new()));
}
/// Stops recording and returns the steps noted since [`start_log`].
pub(crate) fn take_log() -> Vec<(&'static str, PathBuf)> {
    LOG.with(|l| l.borrow_mut().take().unwrap_or_default())
}
/// Notes a step (an fsync, a removal) for a test that checks their order.
pub(crate) fn note(event: &'static str, path: &Path) {
    LOG.with(|l| {
        if let Some(log) = l.borrow_mut().as_mut() {
            log.push((event, path.to_owned()));
        }
    });
}
