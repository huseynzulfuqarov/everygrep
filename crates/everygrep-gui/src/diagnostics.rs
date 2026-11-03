//! Help → Copy troubleshooting information: a short report about the
//! program and the system for a bug report.
//!
//! The report is in English, like the messages on standard error, so that
//! anyone helping can read it. It holds numbers and names of the system
//! only: no paths, no folder names, no search text, so it can be pasted
//! into a public issue as it is.

use std::fmt::Write as _;

use crate::engine::IndexStats;

/// What the report says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    pub version: String,
    /// `PRETTY_NAME` of `/etc/os-release`.
    pub os: Option<String>,
    /// `/proc/sys/kernel/osrelease`; the report keeps only its leading
    /// version number (see [`kernel_version`]).
    pub kernel: Option<String>,
    /// `XDG_CURRENT_DESKTOP`; the report keeps only known desktop names
    /// (see [`desktop_names`]).
    pub desktop: Option<String>,
    /// `XDG_SESSION_TYPE`; the report keeps only the known types (see
    /// [`session_type`]).
    pub session: Option<String>,
    pub wayland: bool,
    /// `DISPLAY` is set: XWayland, which the file clipboard uses.
    pub x11: bool,
    /// The language code of the window.
    pub language: &'static str,
    pub files: u64,
    pub folders: u64,
    pub roots: usize,
    pub drives: usize,
    pub drives_online: usize,
    pub memory: usize,
    /// The index is saved to a file; its size when it exists.
    pub saved: bool,
    pub index_file_bytes: Option<u64>,
    pub file_details: bool,
    pub unwatched: usize,
    pub watch_limit: bool,
    /// `/proc/sys/fs/inotify/max_user_watches`.
    pub max_watches: Option<u64>,
}

impl Facts {
    /// Takes the index numbers from `stats` and the rest from the system.
    /// Reads a few small files, so it is called off the window's thread.
    pub fn gather(stats: &IndexStats, language: &'static str) -> Self {
        let read = |p: &str| std::fs::read_to_string(p).ok();
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Self {
            version: everygrep_core::VERSION.to_owned(),
            os: read("/etc/os-release").and_then(|t| os_name(&t)),
            kernel: read("/proc/sys/kernel/osrelease").map(|k| k.trim().to_owned()),
            desktop: var("XDG_CURRENT_DESKTOP"),
            session: var("XDG_SESSION_TYPE"),
            wayland: var("WAYLAND_DISPLAY").is_some(),
            x11: var("DISPLAY").is_some(),
            language,
            files: stats.files,
            folders: stats.folders,
            roots: stats.roots,
            drives: stats.drives,
            drives_online: stats.drives_online,
            memory: stats.memory,
            saved: stats.index_file.is_some(),
            index_file_bytes: stats.index_file_bytes,
            file_details: stats.file_details,
            unwatched: stats.unwatched.folders,
            watch_limit: stats.unwatched.limit_reached,
            max_watches: read("/proc/sys/fs/inotify/max_user_watches")
                .and_then(|t| t.trim().parse().ok()),
        }
    }
}

/// `PRETTY_NAME` (or `NAME`) of an os-release file, without quotes.
pub fn os_name(os_release: &str) -> Option<String> {
    let value = |key: &str| {
        os_release.lines().find_map(|l| {
            let v = l.strip_prefix(key)?.strip_prefix('=')?.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(v);
            (!v.is_empty()).then(|| v.to_owned())
        })
    };
    value("PRETTY_NAME").or_else(|| value("NAME"))
}

/// The leading version number of a kernel release (`7.3.1` of
/// `7.3.1-1-cachyos`). The rest is left out: a self-built kernel can carry
/// any text there, a user or machine name too.
pub fn kernel_version(release: &str) -> Option<String> {
    let release = release.trim();
    let end = release
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(release.len());
    let version = release[..end].trim_end_matches('.');
    version
        .starts_with(|c: char| c.is_ascii_digit())
        .then(|| version.to_owned())
}

/// Desktop names of the freedesktop list and common compositors, in their
/// usual spelling.
const DESKTOPS: &[&str] = &[
    "GNOME",
    "GNOME-Classic",
    "GNOME-Flashback",
    "KDE",
    "XFCE",
    "LXDE",
    "LXQt",
    "MATE",
    "Cinnamon",
    "X-Cinnamon",
    "Unity",
    "ubuntu",
    "pop",
    "Budgie",
    "Pantheon",
    "COSMIC",
    "Deepin",
    "DDE",
    "UKUI",
    "Endless",
    "EDE",
    "TDE",
    "Lumina",
    "Enlightenment",
    "sway",
    "Hyprland",
    "niri",
    "river",
    "labwc",
    "Wayfire",
    "wlroots",
    "i3",
];

/// The parts of an `XDG_CURRENT_DESKTOP` value (`ubuntu:GNOME`) that are
/// known desktop names; any other part, which could be a path or a name,
/// becomes `other`.
pub fn desktop_names(value: &str) -> String {
    let parts: Vec<&str> = value
        .split(':')
        .map(|part| {
            let part = part.trim();
            DESKTOPS
                .iter()
                .find(|d| d.eq_ignore_ascii_case(part))
                .copied()
                .unwrap_or("other")
        })
        .collect();
    parts.join(":")
}

/// An `XDG_SESSION_TYPE` value if it is one of the defined types, else
/// `other`.
pub fn session_type(value: &str) -> &'static str {
    ["wayland", "x11", "tty", "mir", "unspecified"]
        .into_iter()
        .find(|t| t.eq_ignore_ascii_case(value.trim()))
        .unwrap_or("other")
}

/// The report, one fact a line. Values read from the system that are free
/// text (kernel release, desktop and session names) are reduced to known
/// forms, so no name or path the user chose gets into it.
pub fn report(f: &Facts) -> String {
    let unknown = |v: &Option<String>| v.clone().unwrap_or_else(|| "unknown".into());
    let yes = |b: bool| if b { "yes" } else { "no" };
    let mut out = String::new();
    let _ = writeln!(out, "EveryGrep {}", f.version);
    let _ = writeln!(out, "System: {}", unknown(&f.os));
    let kernel = f.kernel.as_deref().and_then(kernel_version);
    let _ = writeln!(out, "Kernel: {}", unknown(&kernel));
    let desktop = f.desktop.as_deref().map(desktop_names);
    let session = f.session.as_deref().map(|s| session_type(s).to_owned());
    let _ = writeln!(
        out,
        "Desktop: {} ({} session; Wayland: {}, X11 display: {})",
        unknown(&desktop),
        unknown(&session),
        yes(f.wayland),
        yes(f.x11)
    );
    let _ = writeln!(out, "Language: {}", f.language);
    let _ = writeln!(
        out,
        "Index: {} files, {} folders, {} folders to index, {} drives ({} connected)",
        f.files, f.folders, f.roots, f.drives, f.drives_online
    );
    let _ = writeln!(
        out,
        "Index memory: {} MiB; saved to disk: {}{}",
        f.memory >> 20,
        yes(f.saved),
        f.index_file_bytes
            .map_or(String::new(), |b| format!(" ({} MiB)", b >> 20))
    );
    let _ = writeln!(out, "File details indexed: {}", yes(f.file_details));
    let _ = writeln!(
        out,
        "Folders not watched: {}{}; watch limit: {}",
        f.unwatched,
        if f.watch_limit {
            " (limit reached)"
        } else {
            ""
        },
        f.max_watches
            .map_or_else(|| "unknown".to_owned(), |n| n.to_string())
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_system_name_comes_from_os_release() {
        let text = "NAME=\"Arch Linux\"\nPRETTY_NAME=\"CachyOS Linux\"\nID=cachyos\n";
        assert_eq!(os_name(text).as_deref(), Some("CachyOS Linux"));
        assert_eq!(os_name("NAME=Fedora\n").as_deref(), Some("Fedora"));
        assert_eq!(os_name("PRETTY_NAME=\"\"\nNAME=x\n").as_deref(), Some("x"));
        assert_eq!(os_name("ID=x\n"), None);
        // A key that only starts the same is another key.
        assert_eq!(os_name("PRETTY_NAMES=y\n"), None);
    }

    #[test]
    fn the_report_has_one_fact_a_line_and_no_paths() {
        let stats = IndexStats {
            files: 1200,
            folders: 30,
            roots: 2,
            drives: 1,
            memory: 5 << 20,
            index_file: Some("/home/someone/.cache/everygrep/index.bin".into()),
            index_file_bytes: Some(3 << 20),
            file_details: true,
            ..IndexStats::default()
        };
        let mut facts = Facts::gather(&stats, "az");
        facts.version = "0.1.0".into();
        facts.os = Some("Test OS".into());
        facts.kernel = None;
        facts.desktop = Some("GNOME".into());
        facts.session = Some("wayland".into());
        facts.wayland = true;
        facts.x11 = false;
        facts.max_watches = Some(65536);
        assert_eq!(
            report(&facts),
            "EveryGrep 0.1.0\n\
             System: Test OS\n\
             Kernel: unknown\n\
             Desktop: GNOME (wayland session; Wayland: yes, X11 display: no)\n\
             Language: az\n\
             Index: 1200 files, 30 folders, 2 folders to index, 1 drives (0 connected)\n\
             Index memory: 5 MiB; saved to disk: yes (3 MiB)\n\
             File details indexed: yes\n\
             Folders not watched: 0; watch limit: 65536\n"
        );
        assert!(!report(&facts).contains("/home"));
    }

    #[test]
    fn identifying_system_strings_are_not_copied() {
        let mut facts = Facts {
            version: "0.1.0".into(),
            kernel: Some("6.12.1-alice-laptop".into()),
            desktop: Some("/home/alice/custom-session".into()),
            session: Some("alice".into()),
            ..Facts::default()
        };
        let text = report(&facts);
        assert!(!text.contains("alice"), "{text}");
        assert!(text.contains("Kernel: 6.12.1\n"), "{text}");
        assert!(text.contains("Desktop: other (other session;"), "{text}");
        // Known names stay, in their usual spelling.
        facts.kernel = Some("7.3.1-1-cachyos\n".into());
        facts.desktop = Some("ubuntu:gnome:Alice".into());
        facts.session = Some("Wayland".into());
        let text = report(&facts);
        assert!(text.contains("Kernel: 7.3.1\n"), "{text}");
        assert!(
            text.contains("Desktop: ubuntu:GNOME:other (wayland session;"),
            "{text}"
        );
        facts.kernel = Some("alice".into());
        assert!(report(&facts).contains("Kernel: unknown\n"));
    }
}
