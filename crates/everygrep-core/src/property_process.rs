//! Bounded helper processes; pipes cannot grow without a limit.
use crate::progress::Progress;
use std::ffi::OsStr;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub(crate) fn output(
    program: &str,
    args: &[&OsStr],
    timeout: Duration,
    progress: &Progress,
) -> Option<Vec<u8>> {
    let mut child = Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let started = Instant::now();
    // These helpers return a few KiB. A full pipe blocks the child; the deadline
    // then kills and reaps it, before reading at most 64 KiB.
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Err(_) => break None,
            _ if progress.is_cancelled() || started.elapsed() >= timeout => break None,
            _ => std::thread::sleep(Duration::from_millis(5)),
        }
    };
    if !status.is_some_and(|s| s.success()) {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    let mut bytes = Vec::new();
    child
        .stdout
        .take()?
        .take(65536)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(bytes)
}
