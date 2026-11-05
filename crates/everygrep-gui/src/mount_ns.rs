//! Tests on real mounts (tests only). A test that needs a file system of
//! its own (a mount inside a folder, a case-insensitive folder) runs again
//! in a new user and mount namespace (`unshare -rm`, no root needed), after
//! a setup script mounted what it needs in a scratch folder. The mounts go
//! away with the namespace. Where user namespaces are not allowed, the test
//! is skipped with a note.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Set inside the namespace: the scratch folder the setup prepared.
const ENV: &str = "EVERYGREP_TEST_MOUNT_DIR";

/// Inside the namespace: the scratch folder; `None` outside it.
pub(crate) fn inside() -> Option<PathBuf> {
    std::env::var_os(ENV).map(PathBuf::from)
}

/// Runs `setup` (a `sh` script; `$1` is `dir`) and then the test `test`
/// (its full name, such as `actions::tests::x`) in a new namespace. The
/// setup exits with 77 when the mount is not available here: skipped.
/// Panics when the test fails in there.
pub(crate) fn run(test: &str, dir: &Path, setup: &str) {
    let script = format!("{setup}\nexec \"$2\" --exact {test} --test-threads=1");
    let run = Command::new("unshare")
        .args(["-rm", "sh", "-c", &script, "sh"])
        .arg(dir)
        .arg(std::env::current_exe().expect("test binary"))
        .env(ENV, dir)
        .stdin(Stdio::null())
        .output();
    match run {
        Ok(out) if out.status.success() => {}
        // No test output: unshare or the setup failed, not the test.
        Ok(out) if matches!(out.status.code(), Some(1 | 77)) && out.stdout.is_empty() => {
            eprintln!(
                "{test} skipped: no mount in a user namespace here: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(out) => panic!(
            "{test} in the namespace: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
        Err(e) => eprintln!("{test} skipped: no unshare: {e}"),
    }
}

/// A setup that mounts a tmpfs at `$1/cf` whose folder `d` does not tell
/// upper and lower case apart (`casefold`, `chattr +F`).
pub(crate) const CASEFOLD_SETUP: &str = "mkdir \"$1/cf\" || exit 1
    mount -t tmpfs -o casefold=utf8-12.1.0 tmpfs \"$1/cf\" || exit 77
    mkdir \"$1/cf/d\" && chattr +F \"$1/cf/d\" || exit 77";
