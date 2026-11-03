//! Questions cross into the event loop; only the transfer worker waits.
use std::cell::RefCell;
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use crate::i18n::{fill, tr};
use crate::{
    AppWindow,
    transfer::{Decision, Resolver},
};
use slint::ComponentHandle;

thread_local! {
    static ANSWER: RefCell<Option<Sender<Decision>>> = const { RefCell::new(None) };
}

pub fn connect(w: &AppWindow) {
    w.on_fileops_conflict_answer({
        let weak = w.as_weak();
        move |choice| {
            let decision = match choice {
                0 => Decision::Overwrite,
                1 => Decision::Skip,
                2 => Decision::SkipAll,
                3 => Decision::KeepBoth,
                4 => Decision::Retry,
                _ => Decision::Cancel,
            };
            ANSWER.with(|a| {
                if let Some(a) = a.borrow_mut().take() {
                    let _ = a.send(decision);
                }
            });
            if let Some(w) = weak.upgrade().filter(|w| w.get_fileops_conflict_open()) {
                w.set_fileops_conflict_open(false);
                w.invoke_focus_list();
            }
        }
    });
}

pub fn resolver(weak: slint::Weak<AppWindow>) -> Resolver {
    Box::new(move |question, progress| {
        let (tx, rx) = mpsc::channel();
        if weak
            .upgrade_in_event_loop(move |w| {
                let text = match &question.error {
                    Some(error) => fill(
                        tr("Cannot paste {0}: {1}"),
                        &[&question.source.display(), error],
                    ),
                    None => {
                        let safety = if question.can_overwrite {
                            tr("The replaced file will be sent to the trash. If that fails, nothing will be replaced.")
                        } else if !question.trash_available {
                            tr("Trash is unavailable on this filesystem. Overwrite is disabled.")
                        } else {
                            ""
                        };
                        fill(tr("A name already exists.\nFrom: {0}\n{1}\nTo: {2}\n{3}\n{4}"),
                            &[&question.source.display(), &question.source_details,
                              &question.target.display(), &question.target_details, &safety])
                    },
                };
                ANSWER.with(|a| *a.borrow_mut() = Some(tx));
                w.set_fileops_conflict_text(text.into());
                w.set_fileops_conflict_overwrite(question.can_overwrite);
                w.set_fileops_conflict_error(question.error.is_some());
                w.set_fileops_conflict_open(true);
            })
            .is_err()
        {
            return Decision::Cancel;
        }
        wait_answer(rx, progress)
    })
}

fn wait_answer(
    rx: mpsc::Receiver<Decision>,
    progress: &everygrep_core::progress::Progress,
) -> Decision {
    loop {
        if progress.is_cancelled() {
            return Decision::Cancel;
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(choice) => return choice,
            Err(mpsc::RecvTimeoutError::Disconnected) => return Decision::Cancel,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use everygrep_core::progress::Progress;

    #[test]
    fn worker_waits_for_an_answer_and_cancellation_wakes_it() {
        let progress = std::sync::Arc::new(Progress::new());
        let (tx, rx) = mpsc::channel();
        let p = progress.clone();
        let worker = std::thread::spawn(move || wait_answer(rx, &p));
        tx.send(Decision::Skip).unwrap();
        assert_eq!(worker.join().unwrap(), Decision::Skip);
        let (_tx, rx) = mpsc::channel();
        let p = progress.clone();
        let worker = std::thread::spawn(move || wait_answer(rx, &p));
        progress.cancel();
        assert_eq!(worker.join().unwrap(), Decision::Cancel);
        let (tx, rx) = mpsc::channel();
        drop(tx);
        assert_eq!(wait_answer(rx, &Progress::new()), Decision::Cancel);
    }
}
