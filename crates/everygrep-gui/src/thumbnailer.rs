//! System thumbnailers run only inside a fresh, bounded namespace sandbox.
use everygrep_core::{preview, progress::Progress};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

#[derive(Debug)]
struct Definition {
    exec: String,
    mimes: Vec<String>,
}
fn definition(text: &str) -> Option<Definition> {
    let mut entry = false;
    let mut exec = None;
    let mut mimes = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            entry = line == "[Thumbnailer Entry]";
        }
        if !entry {
            continue;
        }
        if let Some(v) = line.strip_prefix("Exec=") {
            exec = Some(v.to_owned());
        }
        if let Some(v) = line.strip_prefix("MimeType=") {
            mimes = v
                .split(';')
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect();
        }
        if let Some(v) = line.strip_prefix("TryExec=") {
            executable(v)?;
        }
    }
    Some(Definition { exec: exec?, mimes })
}
fn executable(name: &str) -> Option<PathBuf> {
    let p = if name.contains('/') {
        PathBuf::from(name)
    } else {
        Path::new("/usr/bin").join(name)
    };
    // Component-wise starts_with accepts "/usr/../home"; allow only plain names.
    let plain = p
        .components()
        .skip(1)
        .all(|c| matches!(c, std::path::Component::Normal(_)));
    (plain && p.starts_with("/usr/") && p.is_file()).then_some(p)
}
/// Desktop Exec quoting, without shell interpretation or command substitution.
fn words(line: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut quote = false;
    let mut escape = false;
    for c in line.chars() {
        if escape {
            word.push(c);
            escape = false;
        } else if c == '\\' {
            escape = true;
        } else if c == '"' {
            quote = !quote;
        } else if c.is_whitespace() && !quote {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
        } else {
            word.push(c);
        }
    }
    if quote || escape {
        return None;
    }
    if !word.is_empty() {
        out.push(word);
    }
    Some(out)
}
fn expand(exec: &str, input: &str) -> Option<Vec<OsString>> {
    let mut words = words(exec)?;
    let program = executable(words.first()?)?;
    let mut output = vec![program.into_os_string()];
    for word in words.drain(1..) {
        let mut arg = String::new();
        let mut chars = word.chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                arg.push(c);
                continue;
            }
            match chars.next()? {
                '%' => arg.push('%'),
                'i' => arg.push_str(input),
                'u' => arg.push_str(&crate::open::file_uri(Path::new(input))),
                'o' => arg.push_str("/tmp/thumbnail.png"),
                's' => arg.push_str("256"),
                _ => return None,
            }
        }
        output.push(arg.into());
    }
    Some(output)
}
fn mime(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?.to_lowercase();
    let globs = fs::read_to_string("/usr/share/mime/globs2").ok()?;
    globs
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            let mut fields = l.split(':');
            let weight = fields.next()?.parse::<u32>().ok()?;
            let mime = fields.next()?;
            let pattern = fields.next()?;
            let suffix = pattern.strip_prefix('*')?;
            (!suffix.contains(['*', '?', '[']) && name.ends_with(suffix))
                .then_some(((weight, suffix.len()), mime))
        })
        .max_by_key(|(weight, _)| *weight)
        .map(|(_, mime)| mime.to_string())
}

/// The top-level program and library folders, mirrored from the host.
const TOP_LEVEL: [&str; 6] = ["bin", "sbin", "lib", "lib64", "lib32", "libx32"];

/// Recreates the host's top-level program and library folders (below `root`,
/// which is `/` outside tests): a symlink gets the same target (merged /usr
/// systems, where the loader may be in `/usr/lib`, `/usr/lib64` or
/// `/usr/lib/<triplet>`), a real folder is bound read-only (split systems),
/// and a missing one is left out.
fn host_layout(root: &Path) -> Vec<OsString> {
    let mut out = Vec::new();
    for name in TOP_LEVEL {
        let host = root.join(name);
        let inside = OsString::from(format!("/{name}"));
        let Ok(meta) = fs::symlink_metadata(&host) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            if let Ok(target) = fs::read_link(&host) {
                out.extend(["--symlink".into(), target.into_os_string(), inside]);
            }
        } else if meta.is_dir() {
            out.extend(["--ro-bind".into(), host.into_os_string(), inside]);
        }
    }
    out
}

/// The sandbox without the input and output files: only runtime data,
/// never the host home or root tree. `layout` comes from [`host_layout`].
fn base_args(layout: &[OsString]) -> Vec<OsString> {
    let mut a: Vec<OsString> = [
        "--die-with-parent",
        "--new-session",
        "--unshare-all",
        "--cap-drop",
        "ALL",
        "--clearenv",
        "--setenv",
        "PATH",
        "/usr/bin",
        "--setenv",
        "HOME",
        "/tmp",
        "--setenv",
        "TMPDIR",
        "/tmp",
        "--setenv",
        "LC_ALL",
        "C.UTF-8",
        "--setenv",
        "GIO_USE_VFS",
        "local",
        "--ro-bind",
        "/usr",
        "/usr",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    a.extend(layout.iter().cloned());
    a.extend(
        [
            "--ro-bind-try",
            "/etc/ld.so.cache",
            "/etc/ld.so.cache",
            "--ro-bind-try",
            "/etc/alternatives",
            "/etc/alternatives",
            "--ro-bind-try",
            "/etc/fonts",
            "/etc/fonts",
            "--ro-bind-try",
            "/etc/localtime",
            "/etc/localtime",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--size",
            "16777216",
            "--tmpfs",
            "/tmp",
            "--dir",
            "/input",
            "--chdir",
            "/tmp",
        ]
        .into_iter()
        .map(OsString::from),
    );
    a
}

/// Bind only runtime data and this input, never the host home or root tree.
fn args(
    input: &Path,
    output: &Path,
    sandbox_input: &str,
    helper: &[OsString],
    layout: &[OsString],
) -> Vec<OsString> {
    let mut a = base_args(layout);
    a.extend([
        "--ro-bind".into(),
        input.as_os_str().into(),
        sandbox_input.into(),
        "--bind".into(),
        output.as_os_str().into(),
        "/tmp/thumbnail.png".into(),
    ]);
    a.extend(helper.iter().cloned());
    a
}

#[repr(C)]
struct Limit {
    soft: u64,
    hard: u64,
}
unsafe extern "C" {
    fn setrlimit(resource: i32, limit: *const Limit) -> i32;
    fn fcntl(fd: i32, cmd: i32, ...) -> i32;
}
fn limits() -> std::io::Result<()> {
    // Linux RLIMIT_CPU, FSIZE, AS. The children inherit these limits.
    for (resource, cap) in [
        (0, 5),
        (1, preview::OUTPUT_CAP as u64),
        (9, 2 * 1024 * 1024 * 1024),
    ] {
        if unsafe {
            setrlimit(
                resource,
                &Limit {
                    soft: cap,
                    hard: cap,
                },
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}
/// Native Linux classic BPF in the spirit of the desktop thumbnail sandbox:
/// validate the ABI, deny kernel administration, mount, tracing and user
/// namespace creation. Namespace isolation still applies without this filter.
/// The one helper allowed to make its own nested sandbox (HEIC, JXL, SVG
/// and other image formats): it runs each image decoder in a `bwrap` of
/// its own, which needs user namespaces and mounts inside ours.
const GLYCIN: &str = "/usr/bin/glycin-thumbnailer";

/// Syscalls a nested `bwrap` needs beyond the rest: pivot_root, chroot,
/// mount, umount2, unshare and setns (and clone with a new user namespace).
#[cfg(target_arch = "x86_64")]
const NESTED: &[u32] = &[155, 161, 165, 166, 272, 308];
#[cfg(target_arch = "aarch64")]
const NESTED: &[u32] = &[41, 51, 40, 39, 97, 268];
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const NESTED: &[u32] = &[];

/// Whether `helper` may open its own nested sandbox: only the exact glycin
/// path, a regular file owned by root that no one else can change (nor its
/// folder), and only when allowed in Settings.
fn nested_allowed(helper: &Path, allowed: bool) -> bool {
    use std::os::unix::fs::MetadataExt;
    let root_only = |p: &Path, dir: bool| {
        fs::symlink_metadata(p).is_ok_and(|m| {
            (if dir { m.is_dir() } else { m.is_file() }) && m.uid() == 0 && m.mode() & 0o022 == 0
        })
    };
    allowed
        && helper == Path::new(GLYCIN)
        && root_only(helper, false)
        && helper.parent().is_some_and(|d| root_only(d, true))
}

#[cfg(target_arch = "x86_64")]
const SECCOMP: Option<(u32, Blocked)> = Some((
    0xc000003e,
    Blocked {
        clone: 56,
        clone3: 435,
        eperm: &[
            101, 103, 134, 154, 155, 156, 161, 163, 165, 166, 167, 168, 169, 172, 173, 174, 175,
            176, 179, 180, 181, 182, 183, 184, 185, 212, 246, 248, 249, 250, 272, 279, 298, 300,
            303, 304, 308, 310, 311, 312, 313, 320, 321, 323, 425, 426, 427, 428, 429, 430, 431,
            432, 433, 442,
        ],
    },
));
#[cfg(target_arch = "aarch64")]
const SECCOMP: Option<(u32, Blocked)> = Some((
    0xc00000b7,
    Blocked {
        clone: 220,
        clone3: 435,
        eperm: &[
            39, 40, 41, 51, 60, 89, 97, 104, 105, 106, 116, 117, 142, 217, 218, 219, 224, 225, 239,
            241, 262, 264, 265, 268, 270, 271, 272, 273, 280, 282, 294, 425, 426, 427, 428, 429,
            430, 431, 432, 433, 442,
        ],
    },
));
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const SECCOMP: Option<(u32, Blocked)> = None;
struct Blocked {
    clone: u32,
    clone3: u32,
    eperm: &'static [u32],
}
const CLONE_NEWUSER: u32 = 0x1000_0000;
const RET_KILL: u32 = 0x8000_0000;
const RET_EPERM: u32 = 0x0005_0001;
// ENOSYS makes libc fall back to clone(), whose flags the filter can inspect.
const RET_ENOSYS: u32 = 0x0005_0026;
const RET_ALLOW: u32 = 0x7fff_0000;
/// The filter; `nested` lets a helper make its own sandbox (see
/// [`nested_allowed`]): user namespaces and the [`NESTED`] calls. All
/// other denials stay.
fn seccomp(nested: bool) -> Option<Vec<u8>> {
    let (arch, blocked) = SECCOMP?;
    let mut out = Vec::new();
    let mut push = |code: u16, jt: u8, jf: u8, k: u32| {
        out.extend(code.to_ne_bytes());
        out.extend([jt, jf]);
        out.extend(k.to_ne_bytes());
    };
    // ld arch; jeq; kill; ld nr
    push(0x20, 0, 0, 4);
    push(0x15, 1, 0, arch);
    push(0x06, 0, 0, RET_KILL);
    push(0x20, 0, 0, 0);
    // Reject x32 as well as alternate architectures.
    if cfg!(target_arch = "x86_64") {
        push(0x45, 0, 1, 0x4000_0000);
        push(0x06, 0, 0, RET_KILL);
    }
    for nr in blocked.eperm {
        if nested && NESTED.contains(nr) {
            continue;
        }
        push(0x15, 0, 1, *nr);
        push(0x06, 0, 0, RET_EPERM);
    }
    push(0x15, 0, 1, blocked.clone3);
    push(0x06, 0, 0, RET_ENOSYS);
    if !nested {
        // clone(flags, ...): the low word of the first argument holds the flags.
        push(0x15, 0, 3, blocked.clone);
        push(0x20, 0, 0, 16);
        push(0x45, 0, 1, CLONE_NEWUSER);
        push(0x06, 0, 0, RET_EPERM);
    }
    push(0x06, 0, 0, RET_ALLOW);
    Some(out)
}
struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// `None` when the helper was stopped for cancellation, time or output size.
fn wait(
    child: &mut std::process::Child,
    output: &Path,
    progress: &Progress,
    started: Instant,
) -> Option<bool> {
    loop {
        if !preview::active(progress, started)
            || fs::metadata(output).is_ok_and(|m| m.len() > preview::OUTPUT_CAP as u64)
        {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        match child.try_wait() {
            Ok(Some(status)) => return Some(status.success()),
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// `nested`: the Settings switch that lets the glycin helper (only) make
/// its own nested sandbox.
pub fn generate(
    path: &Path,
    progress: &Progress,
    attempted: &mut bool,
    nested: bool,
) -> Option<Vec<u8>> {
    if !Path::new("/usr/bin/bwrap").is_file() || progress.is_cancelled() {
        return None;
    }
    let source = preview::open(path)?;
    let source_fd = source.as_raw_fd();
    let input = PathBuf::from(format!("/proc/self/fd/{source_fd}"));
    let mime = mime(path)?;
    let mut definitions = fs::read_dir("/usr/share/thumbnailers")
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect::<Vec<_>>();
    definitions.sort();
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp_path = std::env::temp_dir().join(format!(
        "everygrep-thumbnail-{}-{serial}",
        std::process::id()
    ));
    fs::DirBuilder::new().mode(0o700).create(&temp_path).ok()?;
    let temp = Temp(temp_path);
    let output = temp.0.join("thumbnail.png");
    let sandbox_input = format!(
        "/input/file.{}",
        path.extension()
            .unwrap_or_default()
            .to_string_lossy()
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect::<String>()
    );
    let layout = host_layout(Path::new("/"));
    let started = Instant::now();
    for entry in definitions {
        if !preview::active(progress, started) {
            break;
        }
        let Some(d) = fs::read_to_string(entry).ok().and_then(|t| definition(&t)) else {
            continue;
        };
        if !d.mimes.contains(&mime) {
            continue;
        }
        let Some(helper) = expand(&d.exec, &sandbox_input) else {
            continue;
        };
        File::options()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&output)
            .ok()?;
        let mut command = Command::new("/usr/bin/bwrap");
        command
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut filter = None;
        let nested = nested_allowed(Path::new(&helper[0]), nested);
        if let Some(bytes) = seccomp(nested) {
            let file = temp.0.join("filter");
            let mut f = File::options()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&file)
                .ok()?;
            f.write_all(&bytes).ok()?;
            drop(f);
            let f = File::open(&file).ok()?;
            fs::remove_file(&file).ok()?;
            let fd = f.as_raw_fd();
            command.args(["--seccomp", &fd.to_string()]);
            filter = Some(f);
        }
        command.args(args(&input, &output, &sandbox_input, &helper, &layout));
        // Only async-signal-safe libc calls in the child before exec.
        unsafe {
            command.pre_exec(move || {
                limits()?;
                if fcntl(source_fd, 2, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if let Some(f) = &filter
                    && fcntl(f.as_raw_fd(), 2, 0) == -1
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let Ok(mut child) = command.spawn() else {
            continue;
        };
        // Only a helper that finished by itself may produce a failure record;
        // a timeout or cancellation is retried on the next selection.
        let Some(success) = wait(&mut child, &output, progress, started) else {
            continue;
        };
        *attempted = true;
        if success
            && let Some(bytes) = preview::read(&output, preview::OUTPUT_CAP + 1, progress)
            && bytes.len() <= preview::OUTPUT_CAP
            && preview::dimensions(&bytes, 256 * 256).is_some()
        {
            return Some(bytes);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Runs the classic BPF program on one seccomp_data record.
    fn verdict(program: &[u8], arch: u32, nr: u32, arg0: u64) -> u32 {
        let mut data = Vec::new();
        data.extend(nr.to_ne_bytes());
        data.extend(arch.to_ne_bytes());
        data.extend(0u64.to_ne_bytes());
        data.extend(arg0.to_ne_bytes());
        let (mut pc, mut acc) = (0usize, 0u32);
        loop {
            let i = &program[pc * 8..pc * 8 + 8];
            let code = u16::from_ne_bytes([i[0], i[1]]);
            let (jt, jf) = (i[2] as usize, i[3] as usize);
            let k = u32::from_ne_bytes(i[4..8].try_into().unwrap());
            pc += 1;
            match code {
                0x20 => {
                    acc = u32::from_ne_bytes(data[k as usize..k as usize + 4].try_into().unwrap())
                }
                0x15 => pc += if acc == k { jt } else { jf },
                0x45 => pc += if acc & k != 0 { jt } else { jf },
                0x06 => return k,
                _ => panic!("unexpected opcode {code:#x}"),
            }
        }
    }
    #[test]
    fn seccomp_denies_mount_tracing_and_user_namespaces_only() {
        let Some((arch, blocked)) = SECCOMP else {
            return;
        };
        let program = seccomp(false).unwrap();
        assert_eq!(program.len() % 8, 0);
        let run = |nr, arg0| verdict(&program, arch, nr, arg0);
        assert_eq!(verdict(&program, arch ^ 1, 0, 0), RET_KILL);
        let (mount, ptrace, read, utimensat, io_uring) = if cfg!(target_arch = "x86_64") {
            assert_eq!(run(0x4000_0000, 0), RET_KILL);
            (165, 101, 0, 280, 425)
        } else {
            (40, 117, 63, 88, 425)
        };
        for nr in [mount, ptrace, io_uring] {
            assert_eq!(run(nr, 0), RET_EPERM, "syscall {nr}");
        }
        for nr in [read, utimensat] {
            assert_eq!(run(nr, 0), RET_ALLOW, "syscall {nr}");
        }
        assert_eq!(run(blocked.clone3, 0), RET_ENOSYS);
        assert_eq!(run(blocked.clone, 0x11 | 0x0100_0000), RET_ALLOW);
        assert_eq!(
            run(blocked.clone, u64::from(CLONE_NEWUSER) | 0x11),
            RET_EPERM
        );
    }
    #[test]
    fn nested_filter_allows_only_namespace_and_mount_calls_more() {
        let Some((arch, blocked)) = SECCOMP else {
            return;
        };
        let strict = seccomp(false).unwrap();
        let nested = seccomp(true).unwrap();
        // Every syscall number the strict filter denies stays denied, except
        // the few a nested bwrap needs.
        for nr in 0..500 {
            let a = verdict(&strict, arch, nr, 0);
            let b = verdict(&nested, arch, nr, 0);
            if NESTED.contains(&nr) {
                assert_eq!((a, b), (RET_EPERM, RET_ALLOW), "syscall {nr}");
            } else {
                assert_eq!(a, b, "syscall {nr}");
            }
        }
        let newuser = u64::from(CLONE_NEWUSER) | 0x11;
        assert_eq!(verdict(&strict, arch, blocked.clone, newuser), RET_EPERM);
        assert_eq!(verdict(&nested, arch, blocked.clone, newuser), RET_ALLOW);
        assert_eq!(verdict(&nested, arch, blocked.clone3, 0), RET_ENOSYS);
        assert_eq!(verdict(&nested, arch ^ 1, 0, 0), RET_KILL);
        // ptrace, kexec, bpf and io_uring stay denied.
        let denied: &[u32] = if cfg!(target_arch = "x86_64") {
            &[101, 246, 321, 425]
        } else {
            &[117, 104, 280, 425]
        };
        for nr in denied {
            assert_eq!(verdict(&nested, arch, *nr, 0), RET_EPERM, "syscall {nr}");
        }
    }

    #[test]
    fn only_the_exact_root_owned_glycin_helper_may_nest() {
        assert!(!nested_allowed(Path::new(GLYCIN), false));
        assert!(!nested_allowed(Path::new("/usr/bin/sh"), true));
        assert!(!nested_allowed(
            Path::new("/usr/bin/../bin/glycin-thumbnailer"),
            true
        ));
        assert!(!nested_allowed(
            Path::new("/usr/local/bin/glycin-thumbnailer"),
            true
        ));
        let d = std::env::temp_dir().join(format!("eg-glycin-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        let fake = d.join("glycin-thumbnailer");
        fs::write(&fake, b"").unwrap();
        assert!(!nested_allowed(&fake, true));
        let _ = fs::remove_dir_all(&d);
        // On a system with the real helper, it is allowed when switched on.
        let real = Path::new(GLYCIN);
        if fs::symlink_metadata(real).is_ok_and(|m| {
            use std::os::unix::fs::MetadataExt;
            m.is_file() && m.uid() == 0 && m.mode() & 0o022 == 0
        }) {
            assert!(nested_allowed(real, true));
        }
    }

    #[test]
    fn stopped_helpers_are_not_failures() {
        let out = Path::new("/nonexistent/everygrep-output");
        let mut child = Command::new("/usr/bin/sleep").arg("5").spawn().unwrap();
        let progress = Progress::new();
        progress.cancel();
        assert_eq!(wait(&mut child, out, &progress, Instant::now()), None);
        assert!(child.try_wait().unwrap().is_some());
        let mut child = Command::new("/usr/bin/sleep").arg("5").spawn().unwrap();
        let late = Instant::now() - preview::TIME_CAP;
        assert_eq!(wait(&mut child, out, &Progress::new(), late), None);
        let mut child = Command::new("/usr/bin/false").spawn().unwrap();
        assert_eq!(
            wait(&mut child, out, &Progress::new(), Instant::now()),
            Some(false)
        );
    }
    #[test]
    fn sandbox_is_private_and_has_no_host_root_or_home() {
        let a = args(
            Path::new("/tmp/test input.pdf"),
            Path::new("/tmp/output"),
            "/input/file.pdf",
            &["/usr/bin/helper".into()],
            &host_layout(Path::new("/")),
        );
        let s = a.iter().map(|s| s.to_str().unwrap()).collect::<Vec<_>>();
        for flag in [
            "--unshare-all",
            "--die-with-parent",
            "--new-session",
            "--clearenv",
            "--cap-drop",
            "--tmpfs",
        ] {
            assert!(s.contains(&flag));
        }
        assert!(!s.windows(3).any(|s| s == ["--ro-bind", "/", "/"]));
        assert!(s.windows(3).any(|s| s == ["--ro-bind", "/usr", "/usr"]));
        assert!(
            s.windows(3)
                .any(|s| s == ["--ro-bind", "/tmp/test input.pdf", "/input/file.pdf"])
        );
        assert_eq!(
            words("helper \"a b\" $HOME ;").unwrap(),
            ["helper", "a b", "$HOME", ";"]
        );
        assert!(words("helper \"broken").is_none());
    }
    /// Builds a fake host root with `links` (name, target) and real `dirs`.
    fn fake_root(tag: &str, links: &[(&str, &str)], dirs: &[&str]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("eg-layout-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("usr/lib")).unwrap();
        for (name, target) in links {
            std::os::unix::fs::symlink(target, root.join(name)).unwrap();
        }
        for name in dirs {
            fs::create_dir(root.join(name)).unwrap();
        }
        root
    }
    fn triples(a: &[OsString]) -> Vec<[String; 3]> {
        a.chunks(3)
            .map(|c| c.iter().map(|s| s.to_string_lossy().into_owned()))
            .map(|mut c| [(); 3].map(|_| c.next().unwrap()))
            .collect()
    }
    #[test]
    fn sandbox_layout_mirrors_merged_usr_hosts() {
        // Arch and CachyOS: all of them point into usr/lib and usr/bin.
        let arch = fake_root(
            "arch",
            &[
                ("bin", "usr/bin"),
                ("sbin", "usr/bin"),
                ("lib", "usr/lib"),
                ("lib64", "usr/lib"),
            ],
            &[],
        );
        assert_eq!(
            triples(&host_layout(&arch)),
            [
                ["--symlink", "usr/bin", "/bin"],
                ["--symlink", "usr/bin", "/sbin"],
                ["--symlink", "usr/lib", "/lib"],
                ["--symlink", "usr/lib", "/lib64"],
            ]
        );
        // Fedora and openSUSE: the loader is in /usr/lib64; Debian and
        // Ubuntu also have lib32/libx32 links on some installs.
        let fedora = fake_root(
            "fedora",
            &[
                ("bin", "usr/bin"),
                ("sbin", "usr/sbin"),
                ("lib", "usr/lib"),
                ("lib64", "usr/lib64"),
                ("lib32", "usr/lib32"),
            ],
            &[],
        );
        let layout = host_layout(&fedora);
        assert_eq!(
            triples(&layout),
            [
                ["--symlink", "usr/bin", "/bin"],
                ["--symlink", "usr/sbin", "/sbin"],
                ["--symlink", "usr/lib", "/lib"],
                ["--symlink", "usr/lib64", "/lib64"],
                ["--symlink", "usr/lib32", "/lib32"],
            ]
        );
        // The layout comes right after /usr, so the links resolve in it.
        let a = args(
            Path::new("/tmp/in"),
            Path::new("/tmp/out"),
            "/input/file",
            &["/usr/bin/helper".into()],
            &layout,
        );
        let s = a.iter().map(|s| s.to_str().unwrap()).collect::<Vec<_>>();
        let usr = s
            .windows(3)
            .position(|w| w == ["--ro-bind", "/usr", "/usr"])
            .unwrap();
        assert_eq!(&s[usr + 3..usr + 6], ["--symlink", "usr/bin", "/bin"]);
        assert!(
            !s.windows(3)
                .any(|w| w == ["--symlink", "usr/lib", "/lib64"])
        );
        assert!(
            s.windows(3)
                .any(|w| w == ["--symlink", "usr/lib64", "/lib64"])
        );
        let _ = fs::remove_dir_all(&arch);
        let _ = fs::remove_dir_all(&fedora);
    }
    #[test]
    fn sandbox_layout_binds_real_top_level_folders_read_only() {
        // An older Debian without merged /usr: real folders, no lib32.
        let debian = fake_root("debian", &[], &["bin", "sbin", "lib", "lib64"]);
        // A stray file with a known name is neither linked nor bound.
        fs::write(debian.join("libx32"), b"").unwrap();
        let p = |n: &str| debian.join(n).to_string_lossy().into_owned();
        assert_eq!(
            triples(&host_layout(&debian)),
            [
                ["--ro-bind".into(), p("bin"), "/bin".into()],
                ["--ro-bind".into(), p("sbin"), "/sbin".into()],
                ["--ro-bind".into(), p("lib"), "/lib".into()],
                ["--ro-bind".into(), p("lib64"), "/lib64".into()],
            ]
        );
        let _ = fs::remove_dir_all(&debian);
        // An empty root gives no top-level entries at all.
        let empty = fake_root("empty", &[], &[]);
        assert!(host_layout(&empty).is_empty());
        let _ = fs::remove_dir_all(&empty);
    }
    #[test]
    fn desktop_placeholders_are_arguments_and_unknown_codes_are_refused() {
        let a = expand(
            "/usr/bin/true --input %i --uri %u --output %o --size %s %%",
            "/input/a b.pdf",
        )
        .unwrap();
        let a = a.iter().map(|s| s.to_str().unwrap()).collect::<Vec<_>>();
        assert!(a.contains(&"/input/a b.pdf"));
        assert!(a.contains(&"file:///input/a%20b.pdf"));
        assert!(a.contains(&"/tmp/thumbnail.png"));
        assert!(a.contains(&"256"));
        assert!(a.contains(&"%"));
        assert!(expand("/usr/bin/true %f", "/input/a").is_none());
        assert!(expand("/home/helper %i", "/input/a").is_none());
        assert!(expand("/usr/../usr/bin/true %i", "/input/a").is_none());
        let d=definition("[Other]\nExec=ignore\n[Thumbnailer Entry]\nExec=/usr/bin/true %i %o\nMimeType=application/pdf;video/mp4;\n").unwrap();
        assert_eq!(d.mimes, ["application/pdf", "video/mp4"]);
    }
}
