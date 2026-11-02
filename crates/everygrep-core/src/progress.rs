//! Progress and cancellation of long operations.
//!
//! A [`Progress`] is shared between an operation (a scan, a search, a sort)
//! and whoever shows it. The operation adds what it has done and checks
//! [`Progress::is_cancelled`] now and then; both are relaxed atomic
//! operations, done once per batch of entries, so they cost nothing
//! measurable. A cancelled operation returns [`Cancelled`] and leaves its
//! partial work for the caller to throw away.
//!
//! For a scan the total is not known in advance. [`estimate_entries`]
//! estimates it from the used inodes of the file systems below the roots;
//! file systems that do not report inodes (btrfs, FAT, most FUSE file
//! systems report zero) give no estimate, and the progress is then shown as
//! a count.

use std::collections::HashSet;
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_ulong};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::drives::Mount;
use crate::scan::VIRTUAL_FS_TYPES;

/// The operation was cancelled through its [`Progress`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cancelled;

/// Highest percentage shown while an operation runs: an estimate can be
/// too low, and 100 % is shown only by finishing.
pub const MAX_RUNNING_PERCENT: u8 = 99;

/// How far an operation is, and whether it was asked to stop.
#[derive(Debug, Default)]
pub struct Progress {
    done: AtomicU64,
    /// Expected amount of work; 0 when unknown.
    total: AtomicU64,
    cancelled: AtomicBool,
    /// Tests: cancel once this much work is done (0: never).
    #[cfg(test)]
    cancel_at: AtomicU64,
}

impl Progress {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records `n` more units of work done.
    pub fn add(&self, n: u64) {
        let _done = self.done.fetch_add(n, Ordering::Relaxed) + n;
        #[cfg(test)]
        {
            let at = self.cancel_at.load(Ordering::Relaxed);
            if at > 0 && _done >= at {
                self.cancel();
            }
        }
    }

    /// Tests: cancels the operation once `n` units of work are done, to
    /// interrupt it at a known point.
    #[cfg(test)]
    pub(crate) fn cancel_when_done(&self, n: u64) {
        self.cancel_at.store(n, Ordering::Relaxed);
    }

    /// Units of work done so far.
    pub fn done(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }

    /// Sets the expected amount of work; `None` or 0 for unknown.
    pub fn set_total(&self, total: Option<u64>) {
        self.total.store(total.unwrap_or(0), Ordering::Relaxed);
    }

    /// The expected amount of work, when known.
    pub fn total(&self) -> Option<u64> {
        Some(self.total.load(Ordering::Relaxed)).filter(|&t| t > 0)
    }

    /// Starts counting again from zero for the next step of an operation.
    pub fn restart(&self, total: Option<u64>) {
        self.done.store(0, Ordering::Relaxed);
        self.set_total(total);
    }

    /// Asks the operation to stop.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// `Err(Cancelled)` once the operation was asked to stop.
    pub fn check(&self) -> Result<(), Cancelled> {
        if self.is_cancelled() {
            Err(Cancelled)
        } else {
            Ok(())
        }
    }

    /// The running percentage, see [`percent`].
    pub fn percent(&self) -> Option<u8> {
        percent(self.done(), self.total())
    }
}

/// Percentage of `done` out of an expected `total`, at most
/// [`MAX_RUNNING_PERCENT`]. `None` when the total is unknown or zero.
pub fn percent(done: u64, total: Option<u64>) -> Option<u8> {
    let total = total.filter(|&t| t > 0)?;
    let p = u128::from(done) * 100 / u128::from(total);
    Some(u8::try_from(p).map_or(MAX_RUNNING_PERCENT, |p| p.min(MAX_RUNNING_PERCENT)))
}

/// Inode counts of a file system, as `statvfs` reports them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inodes {
    /// Total inodes (`f_files`).
    pub files: u64,
    /// Free inodes (`f_ffree`).
    pub free: u64,
}

impl Inodes {
    /// Inodes in use, or `None` when the file system does not count them
    /// (it reports no inodes at all, as btrfs and FAT do).
    pub fn used(self) -> Option<u64> {
        (self.files > 0).then(|| self.files.saturating_sub(self.free))
    }
}

/// Mirror of glibc `struct statvfs` (`<bits/statvfs.h>`). Only the fields up
/// to `f_ffree` are read; the tail is padded generously, since its layout
/// differs between 32- and 64-bit targets.
#[repr(C)]
struct StatVfs {
    f_bsize: c_ulong,
    f_frsize: c_ulong,
    f_blocks: c_ulong,
    f_bfree: c_ulong,
    f_bavail: c_ulong,
    f_files: c_ulong,
    f_ffree: c_ulong,
    tail: [c_ulong; 16],
}

unsafe extern "C" {
    fn statvfs(path: *const c_char, buf: *mut StatVfs) -> c_int;
}

/// The inode counts of the file system holding `path`.
pub fn inodes(path: &Path) -> Option<Inodes> {
    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: an all-zero `StatVfs` is valid (plain integers).
    let mut buf: StatVfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is a valid C string and `buf` is at least as large as
    // glibc's `struct statvfs`; both outlive the call.
    let rc = unsafe { statvfs(c_path.as_ptr(), &mut buf) };
    // `c_ulong` is 32 bits wide on 32-bit targets.
    #[allow(clippy::useless_conversion)]
    (rc == 0).then(|| Inodes {
        files: u64::from(buf.f_files),
        free: u64::from(buf.f_ffree),
    })
}

/// Estimates how many entries a scan of `roots` finds: the used inodes of
/// every file system it reads, from the mount table `mounts`. The scan reads
/// the file system holding each root and every file system mounted below a
/// root, except virtual ones and those inside `skip` (folders that are not
/// entered). A file system mounted twice counts once.
///
/// Returns `None` when any of them does not count inodes: leaving it out
/// would make the estimate too low, so no percentage is better than a wrong
/// one. The estimate is too high when a root is only part of its file
/// system, so the percentage then moves slower than the scan.
pub fn estimate_entries(
    roots: &[PathBuf],
    mounts: &[Mount],
    skip: &HashSet<PathBuf>,
    inodes_of: impl Fn(&Path) -> Option<Inodes>,
) -> Option<u64> {
    let mut seen = HashSet::new();
    let mut total = 0u64;
    for root in roots {
        // The mount holding the root: the last mount at the longest prefix.
        let holder = mounts
            .iter()
            .enumerate()
            .filter(|(_, m)| root.starts_with(&m.mount_point))
            .max_by_key(|(i, m)| (m.mount_point.as_os_str().len(), *i))
            .map(|(_, m)| m);
        let below = mounts.iter().filter(|m| {
            m.mount_point != *root
                && m.mount_point.starts_with(root)
                && !VIRTUAL_FS_TYPES.contains(&m.fs_type.as_str())
                && !skip.iter().any(|s| m.mount_point.starts_with(s))
        });
        for m in holder.into_iter().chain(below) {
            if !seen.insert(m.dev) {
                continue;
            }
            let path = if m.mount_point.starts_with(root) {
                &m.mount_point
            } else {
                root
            };
            total = total.saturating_add(inodes_of(path)?.used()?);
        }
    }
    Some(total)
}

/// [`estimate_entries`] with the live mount table and `statvfs`.
pub fn estimate_scan(roots: &[PathBuf], skip: &HashSet<PathBuf>) -> Option<u64> {
    let table = std::fs::read("/proc/self/mountinfo").ok()?;
    let mounts = crate::drives::parse_mountinfo(&table);
    estimate_entries(roots, &mounts, skip, inodes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_is_clamped_below_one_hundred_while_running() {
        assert_eq!(percent(0, Some(200)), Some(0));
        assert_eq!(percent(1, Some(200)), Some(0));
        assert_eq!(percent(100, Some(200)), Some(50));
        assert_eq!(percent(199, Some(200)), Some(99));
        // Reaching or passing the estimate never shows 100 %.
        assert_eq!(percent(200, Some(200)), Some(MAX_RUNNING_PERCENT));
        assert_eq!(percent(5_000, Some(200)), Some(MAX_RUNNING_PERCENT));
        assert_eq!(percent(u64::MAX, Some(1)), Some(MAX_RUNNING_PERCENT));
        assert_eq!(percent(u64::MAX / 2, Some(u64::MAX)), Some(49));
    }

    #[test]
    fn unknown_or_zero_total_gives_no_percent() {
        assert_eq!(percent(10, None), None);
        assert_eq!(percent(10, Some(0)), None);
        let p = Progress::new();
        p.add(7);
        assert_eq!(p.total(), None);
        assert_eq!(p.percent(), None);
        p.set_total(Some(0));
        assert_eq!(p.total(), None);
        p.set_total(Some(14));
        assert_eq!(p.percent(), Some(50));
        p.restart(Some(10));
        assert_eq!((p.done(), p.percent()), (0, Some(0)));
    }

    #[test]
    fn cancel_is_seen_by_check() {
        let p = Progress::new();
        assert_eq!(p.check(), Ok(()));
        p.cancel();
        assert!(p.is_cancelled());
        assert_eq!(p.check(), Err(Cancelled));
    }

    #[test]
    fn file_systems_without_inode_counts_give_no_estimate() {
        // btrfs and FAT report f_files == 0.
        assert_eq!(Inodes { files: 0, free: 0 }.used(), None);
        assert_eq!(
            Inodes {
                files: 1000,
                free: 600
            }
            .used(),
            Some(400)
        );
        assert_eq!(Inodes { files: 5, free: 9 }.used(), Some(0));
    }

    fn mount(dev: u32, at: &str, fs: &str) -> Mount {
        Mount {
            id: dev,
            parent: 1,
            dev: (0, dev),
            mount_point: PathBuf::from(at),
            fs_type: fs.to_owned(),
            source: PathBuf::from("none"),
        }
    }

    /// Used inodes by mount point: 1000 * the device number, except that
    /// device 9 does not count inodes.
    fn fake_inodes(mounts: &[Mount]) -> impl Fn(&Path) -> Option<Inodes> + '_ {
        move |path| {
            let m = mounts
                .iter()
                .filter(|m| path.starts_with(&m.mount_point))
                .max_by_key(|m| m.mount_point.as_os_str().len())?;
            let used = u64::from(m.dev.1) * 1000;
            Some(if m.dev.1 == 9 {
                Inodes { files: 0, free: 0 }
            } else {
                Inodes {
                    files: used + 5,
                    free: 5,
                }
            })
        }
    }

    #[test]
    fn estimate_adds_the_file_systems_below_the_roots() {
        let mounts = vec![
            mount(1, "/", "ext4"),
            mount(2, "/home", "ext4"),
            mount(3, "/home/u/usb", "vfat-like"),
            mount(4, "/proc", "proc"),
            mount(2, "/srv/bind", "ext4"),
            mount(5, "/home/u/skip/inner", "ext4"),
            mount(6, "/other", "ext4"),
        ];
        let skip: HashSet<PathBuf> = [PathBuf::from("/home/u/skip")].into();
        let f = fake_inodes(&mounts);
        // /home/u lies on /home (2) and holds the drive (3); the excluded
        // folder's mount (5) and unrelated mounts are not read.
        assert_eq!(
            estimate_entries(&[PathBuf::from("/home/u")], &mounts, &skip, &f),
            Some(5000)
        );
        // "/" reads 1, 2, 3, 5 (not skipped here) and 6; proc is virtual and
        // the bind mount of 2 counts once.
        assert_eq!(
            estimate_entries(&[PathBuf::from("/")], &mounts, &HashSet::new(), &f),
            Some(17_000)
        );
        // Two roots on the same file system count it once.
        assert_eq!(
            estimate_entries(
                &[PathBuf::from("/home/a"), PathBuf::from("/srv/bind/x")],
                &mounts,
                &HashSet::new(),
                &f
            ),
            Some(2000)
        );
    }

    #[test]
    fn one_file_system_without_inode_counts_spoils_the_estimate() {
        let mounts = vec![mount(1, "/", "ext4"), mount(9, "/home", "btrfs")];
        let f = fake_inodes(&mounts);
        let none = HashSet::new();
        assert_eq!(
            estimate_entries(&[PathBuf::from("/")], &mounts, &none, &f),
            None
        );
        assert_eq!(
            estimate_entries(&[PathBuf::from("/home/u")], &mounts, &none, &f),
            None
        );
        assert_eq!(
            estimate_entries(&[PathBuf::from("/etc")], &mounts, &none, &f),
            Some(1000)
        );
        // No mount table at all: nothing to go by.
        assert_eq!(
            estimate_entries(&[PathBuf::from("/etc")], &[], &none, &f),
            Some(0)
        );
    }

    #[test]
    fn statvfs_reads_the_root_file_system() {
        let i = inodes(Path::new("/")).expect("statvfs of /");
        assert!(i.free <= i.files, "{i:?}");
        assert!(inodes(Path::new("/no/such/path/here")).is_none());
    }
}
