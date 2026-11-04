//! Metadata preservation for moves copied across filesystems. A failure here
//! keeps the source; copying bytes alone is insufficient to complete a move.
use crate::i18n::tr;
use std::collections::BTreeMap;
use std::ffi::{CString, c_char, c_int, c_long, c_void};
use std::fs::{File, FileTimes, Metadata};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

unsafe extern "C" {
    fn flistxattr(fd: c_int, list: *mut c_char, size: usize) -> isize;
    fn fgetxattr(fd: c_int, name: *const c_char, value: *mut c_void, size: usize) -> isize;
    fn fsetxattr(
        fd: c_int,
        name: *const c_char,
        value: *const c_void,
        size: usize,
        flags: c_int,
    ) -> c_int;
    fn fremovexattr(fd: c_int, name: *const c_char) -> c_int;
    fn fchown(fd: c_int, owner: u32, group: u32) -> c_int;
    fn llistxattr(path: *const c_char, list: *mut c_char, size: usize) -> isize;
    fn lgetxattr(
        path: *const c_char,
        name: *const c_char,
        value: *mut c_void,
        size: usize,
    ) -> isize;
    fn lsetxattr(
        path: *const c_char,
        name: *const c_char,
        value: *const c_void,
        size: usize,
        flags: c_int,
    ) -> c_int;
    fn lremovexattr(path: *const c_char, name: *const c_char) -> c_int;
    fn lchown(path: *const c_char, owner: u32, group: u32) -> c_int;
    fn utimensat(dirfd: c_int, path: *const c_char, times: *const Timespec, flags: c_int) -> c_int;
}

#[repr(C)]
struct Timespec {
    sec: c_long,
    nsec: c_long,
}

#[derive(Clone, Copy)]
enum Object<'a> {
    File(&'a File),
    Link(&'a CString),
}
impl Object<'_> {
    fn list(self, buf: &mut [u8]) -> isize {
        // SAFETY: descriptors/C strings and the writable buffer live throughout.
        unsafe {
            match self {
                Self::File(f) => flistxattr(f.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()),
                Self::Link(p) => llistxattr(p.as_ptr(), buf.as_mut_ptr().cast(), buf.len()),
            }
        }
    }
    fn get(self, name: &CString, buf: &mut [u8]) -> isize {
        // SAFETY: valid descriptors/C strings and a writable buffer of this size.
        unsafe {
            match self {
                Self::File(f) => fgetxattr(
                    f.as_raw_fd(),
                    name.as_ptr(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                ),
                Self::Link(p) => lgetxattr(
                    p.as_ptr(),
                    name.as_ptr(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                ),
            }
        }
    }
    fn set(self, name: &CString, value: &[u8]) -> io::Result<()> {
        #[cfg(test)]
        if FAIL_SET.with(std::cell::Cell::get) {
            return Err(io::Error::from_raw_os_error(95));
        }
        // SAFETY: valid descriptors/C strings and a readable buffer of this size.
        checked(unsafe {
            match self {
                Self::File(f) => fsetxattr(
                    f.as_raw_fd(),
                    name.as_ptr(),
                    value.as_ptr().cast(),
                    value.len(),
                    0,
                ),
                Self::Link(p) => lsetxattr(
                    p.as_ptr(),
                    name.as_ptr(),
                    value.as_ptr().cast(),
                    value.len(),
                    0,
                ),
            }
        })
    }
    fn remove(self, name: &CString) -> io::Result<()> {
        // SAFETY: valid descriptor and C strings, no borrowed pointers retained.
        checked(unsafe {
            match self {
                Self::File(f) => fremovexattr(f.as_raw_fd(), name.as_ptr()),
                Self::Link(p) => lremovexattr(p.as_ptr(), name.as_ptr()),
            }
        })
    }
    fn attrs(self) -> io::Result<BTreeMap<Vec<u8>, Vec<u8>>> {
        // Linux caps the complete list and each value at 64 KiB. Fixed buffers
        // avoid a size/query race and preserve empty values as well.
        let mut names = vec![0; 65536];
        let n = self.list(&mut names);
        if n < 0 {
            let e = io::Error::last_os_error();
            return if e.raw_os_error() == Some(95) {
                Ok(BTreeMap::new())
            } else {
                Err(e)
            };
        }
        names.truncate(n as usize);
        let mut out = BTreeMap::new();
        for name in names.split(|b| *b == 0).filter(|s| !s.is_empty()) {
            let c = CString::new(name)?;
            let mut value = vec![0; 65536];
            let n = self.get(&c, &mut value);
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            value.truncate(n as usize);
            out.insert(name.to_vec(), value);
        }
        Ok(out)
    }
}
fn checked(rc: c_int) -> io::Result<()> {
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
fn attributes(src: Object<'_>, dst: Object<'_>) -> io::Result<()> {
    let expected = src.attrs()?;
    for name in dst
        .attrs()?
        .keys()
        .filter(|name| !expected.contains_key(*name))
    {
        dst.remove(&CString::new(name.as_slice())?)?;
    }
    for (name, value) in &expected {
        dst.set(&CString::new(name.as_slice())?, value)?;
    }
    if src.attrs()? != expected || dst.attrs()? != expected {
        return Err(io::Error::other(tr(
            "Extended attributes could not be preserved exactly.",
        )));
    }
    Ok(())
}

pub(super) fn file(src: &File, dst: &File, meta: &Metadata) -> io::Result<()> {
    let now = dst.metadata()?;
    if (now.uid(), now.gid()) != (meta.uid(), meta.gid()) {
        // SAFETY: a live descriptor; uid/gid come from the source metadata.
        checked(unsafe { fchown(dst.as_raw_fd(), meta.uid(), meta.gid()) })?;
    }
    dst.set_permissions(std::fs::Permissions::from_mode(meta.mode() & 0o7777))?;
    attributes(Object::File(src), Object::File(dst))?;
    dst.set_times(
        FileTimes::new()
            .set_accessed(meta.accessed()?)
            .set_modified(meta.modified()?),
    )?;
    verify(&dst.metadata()?, meta)
}

pub(super) fn link(src: &Path, dst: &Path, meta: &Metadata) -> io::Result<()> {
    let a = CString::new(src.as_os_str().as_bytes())?;
    let b = CString::new(dst.as_os_str().as_bytes())?;
    let now = std::fs::symlink_metadata(dst)?;
    if (now.uid(), now.gid()) != (meta.uid(), meta.gid()) {
        // SAFETY: valid C string; lchown never follows the final symlink.
        checked(unsafe { lchown(b.as_ptr(), meta.uid(), meta.gid()) })?;
    }
    attributes(Object::Link(&a), Object::Link(&b))?;
    let times = [
        Timespec {
            sec: meta.atime(),
            nsec: meta.atime_nsec(),
        },
        Timespec {
            sec: meta.mtime(),
            nsec: meta.mtime_nsec(),
        },
    ];
    // SAFETY: valid C string and two timespecs; AT_SYMLINK_NOFOLLOW keeps scope.
    checked(unsafe { utimensat(-100, b.as_ptr(), times.as_ptr(), 0x100) })?;
    verify(&std::fs::symlink_metadata(dst)?, meta)
}
fn verify(now: &Metadata, expected: &Metadata) -> io::Result<()> {
    if (
        now.uid(),
        now.gid(),
        now.mode() & 0o7777,
        now.atime(),
        now.atime_nsec(),
        now.mtime(),
        now.mtime_nsec(),
    ) != (
        expected.uid(),
        expected.gid(),
        expected.mode() & 0o7777,
        expected.atime(),
        expected.atime_nsec(),
        expected.mtime(),
        expected.mtime_nsec(),
    ) {
        return Err(io::Error::other(tr(
            "Ownership, permissions or timestamps could not be preserved exactly.",
        )));
    }
    Ok(())
}

#[cfg(test)]
thread_local! { pub(super) static FAIL_SET: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
#[cfg(test)]
pub(super) fn set_test_attr(path: &Path, name: &str, value: &[u8]) {
    let f = File::open(path).unwrap();
    Object::File(&f)
        .set(&CString::new(name).unwrap(), value)
        .unwrap();
}
#[cfg(test)]
pub(super) fn test_attrs(path: &Path) -> BTreeMap<Vec<u8>, Vec<u8>> {
    Object::File(&File::open(path).unwrap()).attrs().unwrap()
}

#[cfg(test)]
pub(super) fn age_test_link(path: &Path) {
    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let times = [
        Timespec {
            sec: 1234567890,
            nsec: 123456789,
        },
        Timespec {
            sec: 1234567891,
            nsec: 987654321,
        },
    ];
    // SAFETY: the path and two timespecs remain valid; final link not followed.
    checked(unsafe { utimensat(-100, path.as_ptr(), times.as_ptr(), 0x100) }).unwrap();
}
