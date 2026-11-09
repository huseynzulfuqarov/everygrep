//! Keeps our own windows above the window that opened them on Wayland.
//!
//! Slint has no parent for a second window, and winit 0.30 does not support
//! one on Wayland. The Wayland connection winit already has is used here
//! instead: [`keep_above`] sends `xdg_toplevel.set_parent` for one of our
//! windows.
//!
//! Without Wayland, or when anything fails, nothing is set: windows behave
//! as before. Failures go to the debug log once.
//!
//! All `unsafe` is in [`guest`] and [`proxy`]: winit's display and objects
//! are borrowed for the time of a `with_winit_window` call only.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::ptr::NonNull;

use i_slint_backend_winit::WinitWindowAccessor;
use i_slint_backend_winit::winit;
use slint::ComponentHandle;
use wayland_client::backend::{Backend, ObjectId};
use wayland_client::{Connection, Proxy};
use wayland_protocols::xdg::shell::client::xdg_toplevel::XdgToplevel;

/// The native Wayland objects of one winit window.
struct Native {
    display: NonNull<c_void>,
    toplevel: NonNull<c_void>,
}

/// What a window offers: nothing on another platform, its objects on
/// Wayland.
enum Platform {
    Other,
    Wayland(Native),
}

fn native(w: &winit::window::Window) -> Platform {
    use winit::platform::wayland::WindowExtWayland;
    use winit::raw_window_handle::{
        HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
    };
    let Ok(RawDisplayHandle::Wayland(display)) = w.display_handle().map(|d| d.as_raw()) else {
        return Platform::Other;
    };
    let Ok(RawWindowHandle::Wayland(_)) = w.window_handle().map(|h| h.as_raw()) else {
        return Platform::Other;
    };
    match w.xdg_toplevel() {
        Some(toplevel) => Platform::Wayland(Native {
            display: display.display,
            toplevel,
        }),
        None => Platform::Other,
    }
}

/// A connection object of our own on winit's display. Objects created on
/// it get their own event queue, so dispatching them never runs winit's
/// handlers, and dropping it closes nothing.
///
/// # Safety
///
/// `display` must be a live `wl_display`, and stay live until the returned
/// connection, its proxies and its event queues are dropped.
unsafe fn guest(display: NonNull<c_void>) -> Connection {
    // SAFETY: the caller guarantees the display outlives the connection.
    // A foreign display is never disconnected by the backend.
    let backend = unsafe { Backend::from_foreign_display(display.as_ptr().cast()) };
    Connection::from_backend(backend)
}

/// A proxy on `conn` for an object winit created. Requests can be sent on
/// it; it is not owned, so dropping it destroys nothing.
///
/// # Safety
///
/// `ptr` must be a live `wl_proxy` of interface `I`'s kind on the same
/// display as `conn`, and stay live while the proxy is used.
unsafe fn proxy<I: Proxy>(conn: &Connection, ptr: NonNull<c_void>) -> Option<I> {
    // SAFETY: the caller guarantees the pointer is a live proxy; the
    // interface name is checked against it, and a mismatch is an error.
    let id = unsafe { ObjectId::from_ptr(I::interface(), ptr.as_ptr().cast()) }.ok()?;
    I::from_id(conn, id).ok()
}

/// Logs `what` once per run, so a desktop without the protocol does not
/// fill the debug log.
fn note_once(flag: &'static std::thread::LocalKey<Cell<bool>>, what: &str) {
    if !flag.with(|f| f.replace(true)) {
        crate::debug_log::note!("{what}");
    }
}

thread_local! {
    static PARENT_FAILED: Cell<bool> = const { Cell::new(false) };
}

/// Sends `set_parent` for `child`. `Some(true)` when sent, `Some(false)`
/// when there is nothing to do (no Wayland), `None` when it failed or a
/// window has no native window yet.
fn set_parent(child: &slint::Window, parent: &slint::Window) -> Option<bool> {
    child
        .with_winit_window(|c| {
            let Platform::Wayland(c) = native(c) else {
                return Some(false);
            };
            parent
                .with_winit_window(|p| {
                    let Platform::Wayland(p) = native(p) else {
                        return Some(false);
                    };
                    if c.toplevel == p.toplevel {
                        return Some(false);
                    }
                    // SAFETY: both windows are borrowed for this call, so
                    // winit's display and their toplevels are live until the
                    // connection and proxies are dropped at its end. Both
                    // windows belong to the one display of the event loop.
                    let sent = unsafe {
                        let conn = guest(c.display);
                        let child: XdgToplevel = proxy(&conn, c.toplevel)?;
                        let parent: XdgToplevel = proxy(&conn, p.toplevel)?;
                        child.set_parent(Some(&parent));
                        conn.flush().ok()
                    };
                    sent.map(|()| true)
                })
                .flatten()
        })
        .flatten()
}

/// Keys windows by their address: stable while the window lives, and only
/// compared while both windows of a link are alive.
fn key(w: &slint::Window) -> usize {
    w as *const slint::Window as usize
}

/// Which windows are kept above which: one parent per child, so a child
/// shown again replaces its old link.
struct Links<V> {
    links: Vec<(usize, usize, V)>,
}

impl<V> Links<V> {
    const fn new() -> Self {
        Links { links: Vec::new() }
    }

    fn set(&mut self, child: usize, parent: usize, value: V) {
        self.links.retain(|(c, _, _)| *c != child);
        self.links.push((child, parent, value));
    }

    /// What is kept above `parent`.
    fn children(&self, parent: usize) -> impl Iterator<Item = &V> {
        self.links
            .iter()
            .filter(move |(_, p, _)| *p == parent)
            .map(|(_, _, v)| v)
    }

    /// Keeps only the links for which `keep` returns true.
    fn retain(&mut self, mut keep: impl FnMut(usize, usize, &V) -> bool) {
        self.links.retain(|(c, p, v)| keep(*c, *p, v));
    }
}

/// Sends the link again; `false` once either window is gone.
type Reapply = Box<dyn Fn() -> bool>;

thread_local! {
    static LINKS: RefCell<Links<std::rc::Rc<Reapply>>> = const { RefCell::new(Links::new()) };
}

/// Sends the link once both windows have their native window. Slint makes
/// the native window of a window just shown a moment later, in the event
/// loop, so right after `show()` there is nothing to link yet. Each wait
/// holds the window it waits for and ends when that window is made; the
/// native window it yields is dropped at once (holding it would keep a
/// hidden window on the screen).
fn link_when_made<C, P>(child: &C, parent: &P)
where
    C: ComponentHandle + 'static,
    P: ComponentHandle + 'static,
{
    let (c, p) = (child.as_weak(), parent.as_weak());
    let key_of_child = key(child.window());
    let started = slint::spawn_local(async move {
        if let Some(c) = c.upgrade() {
            let _ = c.window().winit_window().await;
        }
        if let Some(p) = p.upgrade() {
            let _ = p.window().winit_window().await;
        }
        let (Some(c), Some(p)) = (c.upgrade(), p.upgrade()) else {
            return;
        };
        if !c.window().is_visible() {
            return;
        }
        if set_parent(c.window(), p.window()).is_none() {
            note_once(&PARENT_FAILED, "cannot keep a window above its parent");
        }
        // Windows kept above the child wait for its new surface too.
        reattach(key_of_child);
    });
    if started.is_err() {
        note_once(&PARENT_FAILED, "cannot keep a window above its parent");
    }
}

/// Keeps `child` above `parent`, the window that opened it. Call it after
/// every `show()` of `child`: on Wayland Slint shows a hidden window with a
/// new surface, and the protocol drops the parent of a surface shown again.
/// Windows kept above `child` are attached to its new surface too.
pub fn keep_above<C, P>(child: &C, parent: &P)
where
    C: ComponentHandle + 'static,
    P: ComponentHandle + 'static,
{
    let (c, p) = (child.as_weak(), parent.as_weak());
    let reapply: Reapply = Box::new(move || {
        let (Some(c), Some(p)) = (c.upgrade(), p.upgrade()) else {
            return false;
        };
        if c.window().is_visible() && set_parent(c.window(), p.window()).is_none() {
            link_when_made(&c, &p);
        }
        true
    });
    let reapply = std::rc::Rc::new(reapply);
    let (child, parent) = (key(child.window()), key(parent.window()));
    LINKS.with(|l| l.borrow_mut().set(child, parent, reapply.clone()));
    reapply();
    reattach(child);
}

/// Attaches the windows kept above `parent` again, after `parent` was
/// shown again (with a new surface).
pub fn reattach_children<P: ComponentHandle>(parent: &P) {
    reattach(key(parent.window()));
}

fn reattach(parent: usize) {
    let due: Vec<_> = LINKS.with(|l| l.borrow().children(parent).cloned().collect());
    // Run outside the borrow; links of windows that are gone are dropped.
    let gone: Vec<_> = due
        .into_iter()
        .filter(|v| !v())
        .map(|v| std::rc::Rc::as_ptr(&v))
        .collect();
    if !gone.is_empty() {
        LINKS.with(|l| {
            l.borrow_mut()
                .retain(|_, _, v| !gone.contains(&std::rc::Rc::as_ptr(v)))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_has_one_parent_and_reattaching_finds_its_children() {
        let mut links = Links::new();
        links.set(1, 10, "a");
        links.set(2, 10, "b");
        links.set(3, 2, "c");
        // Shown again from another window: the old link is replaced.
        links.set(1, 20, "a2");
        let of_10: Vec<_> = links.children(10).copied().collect();
        assert_eq!(of_10, ["b"]);
        assert_eq!(links.links.len(), 3);
        links.retain(|c, _, _| c != 2);
        assert_eq!(links.links.iter().map(|l| l.0).collect::<Vec<_>>(), [3, 1]);
    }
}
