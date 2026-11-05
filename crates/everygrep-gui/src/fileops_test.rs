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
