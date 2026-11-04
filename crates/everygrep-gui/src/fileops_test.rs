//! Deterministic scheduling of filesystem interference in temp-directory tests.
use std::cell::RefCell;
use std::path::Path;

type Callback = Box<dyn FnOnce(&Path)>;
thread_local! {
    static HOOK: RefCell<Option<(&'static str, Callback)>> = const { RefCell::new(None) };
}
pub(crate) fn arm(event: &'static str, callback: impl FnOnce(&Path) + 'static) {
    HOOK.with(|h| {
        assert!(
            h.borrow().is_none(),
            "previous filesystem hook was not reached"
        );
        *h.borrow_mut() = Some((event, Box::new(callback)));
    });
}
pub(crate) fn at(event: &'static str, path: &Path) {
    let callback = HOOK.with(|h| {
        let mut h = h.borrow_mut();
        if h.as_ref().is_some_and(|(want, _)| *want == event) {
            h.take().map(|(_, callback)| callback)
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
