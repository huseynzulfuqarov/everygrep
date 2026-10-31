//! What a launch shows when the open window is closing or does not answer
//! (see [`instance::Role::Stalled`]): a small dialog that waits for the
//! window to go, offers to terminate it, and closes by itself once it is
//! gone, so the launch goes on and opens its window.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Timer, TimerMode};

use crate::instance::{self, Stall, Stalled};
use crate::{StalledDialog, i18n, labels};

/// How often the dialog looks whether the window is gone.
const POLL: Duration = Duration::from_millis(200);

/// How long a terminated window may take to end before the dialog says
/// it timed out.
const TERMINATE_WAIT: Duration = Duration::from_secs(5);

/// What the dialog does at one look.
#[derive(Debug, PartialEq, Eq)]
enum Tick {
    /// The name is free: close the dialog and claim it.
    Retry,
    /// Keep waiting.
    Wait,
    /// The terminated window has not ended in time.
    TimedOut,
}

/// The look at `now`: the name is free (`gone`), or the window was
/// terminated at `terminated` and had [`TERMINATE_WAIT`] to end.
fn tick(gone: bool, terminated: Option<Instant>, now: Instant) -> Tick {
    if gone {
        Tick::Retry
    } else if terminated.is_some_and(|t| now.duration_since(t) >= TERMINATE_WAIT) {
        Tick::TimedOut
    } else {
        Tick::Wait
    }
}

/// Shows the dialog until the window is gone (then the next role of the
/// launch is returned) or the user cancels (`None`: the launch ends).
pub fn resolve(stalled: Stalled) -> Result<Option<instance::Role>, slint::PlatformError> {
    let dialog = StalledDialog::new()?;
    // The language was read with the settings; the .slint texts follow it
    // once a window exists.
    i18n::apply_to_ui();
    let (heading, message) = labels::stalled(stalled.stall);
    dialog.set_heading(heading.into());
    dialog.set_message(message.into());

    let stalled = Rc::new(RefCell::new(Some(stalled)));
    let retry = Rc::new(Cell::new(false));
    let terminated: Rc<Cell<Option<Instant>>> = Rc::default();
    let timer = Timer::default();
    let weak = dialog.as_weak();
    timer.start(TimerMode::Repeated, POLL, {
        let (stalled, retry, terminated, weak) = (
            stalled.clone(),
            retry.clone(),
            terminated.clone(),
            weak.clone(),
        );
        move || {
            let gone = stalled.borrow().as_ref().is_some_and(Stalled::owner_gone);
            match tick(gone, terminated.get(), Instant::now()) {
                Tick::Retry => {
                    retry.set(true);
                    let _ = slint::quit_event_loop();
                }
                Tick::TimedOut => {
                    if let Some(d) = weak.upgrade() {
                        let (heading, message) = labels::stalled_timed_out();
                        d.set_heading(heading.into());
                        d.set_message(message.into());
                        d.set_can_terminate(false);
                    }
                    // Said once; the look goes on in case it ends later.
                    terminated.set(None);
                }
                Tick::Wait => {}
            }
        }
    });
    dialog.on_terminate({
        let (stalled, terminated, weak) = (stalled.clone(), terminated.clone(), weak.clone());
        move || {
            let Some(d) = weak.upgrade() else { return };
            let result = match stalled.borrow().as_ref() {
                Some(s) => s.terminate(),
                None => return,
            };
            d.set_can_terminate(false);
            match result {
                Ok(()) => {
                    terminated.set(Some(Instant::now()));
                    d.set_message(labels::stalled_terminating().into());
                }
                Err(e) => d.set_message(labels::cannot_terminate(&e).into()),
            }
        }
    });
    dialog.on_cancel(|| {
        let _ = slint::quit_event_loop();
    });
    dialog.window().on_close_requested(|| {
        let _ = slint::quit_event_loop();
        slint::CloseRequestResponse::HideWindow
    });
    dialog.show()?;
    slint::run_event_loop_until_quit()?;
    timer.stop();
    dialog.hide()?;
    let stalled = stalled.borrow_mut().take();
    Ok(match stalled {
        Some(s) if retry.get() => Some(s.retry()),
        _ => None,
    })
}

/// The stall a launch reports on the terminal, for launches without a
/// window system.
pub fn describe(stall: Stall) -> &'static str {
    match stall {
        Stall::ShuttingDown => "the open window is still shutting down",
        Stall::NoAnswer => "the open window does not answer",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dialog_retries_once_the_window_is_gone() {
        let t0 = Instant::now();
        assert_eq!(tick(true, None, t0), Tick::Retry);
        assert_eq!(tick(false, None, t0 + Duration::from_secs(60)), Tick::Wait);
        // After terminating, the window has a few seconds to end.
        assert_eq!(
            tick(false, Some(t0), t0 + Duration::from_secs(1)),
            Tick::Wait
        );
        assert_eq!(tick(false, Some(t0), t0 + TERMINATE_WAIT), Tick::TimedOut);
        assert_eq!(tick(true, Some(t0), t0 + TERMINATE_WAIT), Tick::Retry);
    }
}
