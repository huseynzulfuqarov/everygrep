//! Starting EveryGrep when the user logs in, through an XDG autostart entry:
//! `$XDG_CONFIG_HOME/autostart/az.huseynz.EveryGrep.desktop` (by default
//! under `~/.config`). The entry exists exactly while the setting is on, so
//! the settings window reads the state from the file and stays true when
//! the desktop's own tools remove it.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use crate::settings::write_atomic;

/// File name of the entry; equals the app id, like the menu entry.
const FILE_NAME: &str = "az.huseynz.EveryGrep.desktop";

/// The flag the entry starts the program with (see `--background` in
/// main.rs).
pub const BACKGROUND_FLAG: &str = "--background";

/// The autostart folder for the given `XDG_CONFIG_HOME` and `HOME`. A
/// relative `XDG_CONFIG_HOME` is ignored, as the base directory
/// specification says.
fn dir_from(config_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let base = config_home
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("autostart"))
}

/// Location of the entry for this session.
pub fn path() -> Option<PathBuf> {
    dir_from(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
    .map(|d| d.join(FILE_NAME))
}

/// Whether the program starts on login, judged by the entry at `path`.
pub fn is_enabled(path: &Path) -> bool {
    path.is_file()
}

/// Writes the entry, starting `exe`, to `path`.
pub fn enable(path: &Path, exe: &Path) -> io::Result<()> {
    let exe = exe.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "the program's path is not valid UTF-8",
        )
    })?;
    write_atomic(path, entry(exe).as_bytes())
}

/// Removes the entry at `path`; a missing entry is no error.
pub fn disable(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// The running program's file. Linux adds " (deleted)" to it when the file
/// was replaced (by an update) while the program runs; the new file at the
/// same place is meant then.
pub fn current_exe() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    if !exe.exists() {
        if let Some(s) = exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
            return Ok(PathBuf::from(s));
        }
    }
    Ok(exe)
}

/// The text of the entry starting `exe` in the background.
fn entry(exe: &str) -> String {
    let exec = format!("{} {BACKGROUND_FLAG}", exec_arg(exe));
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=EveryGrep\n\
         Comment=Keeps the file index ready from login\n\
         Exec={}\n\
         Icon=az.huseynz.EveryGrep\n\
         Terminal=false\n\
         NoDisplay=true\n\
         X-GNOME-Autostart-enabled=true\n",
        escape_value(&exec)
    )
}

/// One argument of an `Exec` key, quoted as the Desktop Entry
/// specification asks: an argument with a reserved character is put in
/// double quotes, inside which `"`, `` ` ``, `$` and `\` take a backslash,
/// and `%` is doubled everywhere, since it starts a field code.
fn exec_arg(arg: &str) -> String {
    const RESERVED: &[char] = &[
        ' ', '\t', '\n', '"', '\'', '\\', '>', '<', '~', '|', '&', ';', '$', '*', '?', '#', '(',
        ')', '`',
    ];
    let quoted = if arg.is_empty() || arg.contains(RESERVED) {
        let mut out = String::from('"');
        for c in arg.chars() {
            if matches!(c, '"' | '`' | '$' | '\\') {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
        out
    } else {
        arg.to_owned()
    };
    quoted.replace('%', "%%")
}

/// A value of type string in a desktop entry: backslash, line break, tab
/// and carriage return written as escapes. Applied after the `Exec` quoting,
/// so a backslash in a quoted argument ends up as four.
fn escape_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::tests::Scratch;

    #[test]
    fn folder_follows_xdg_config_home() {
        let some = |s: &str| Some(OsString::from(s));
        assert_eq!(
            dir_from(some("/tmp/cfg"), some("/home/u")),
            Some(PathBuf::from("/tmp/cfg/autostart"))
        );
        assert_eq!(
            dir_from(some("relative"), some("/home/u")),
            Some(PathBuf::from("/home/u/.config/autostart"))
        );
        assert_eq!(
            dir_from(None, some("/home/u")),
            Some(PathBuf::from("/home/u/.config/autostart"))
        );
        assert_eq!(dir_from(None, None), None);
    }

    #[test]
    fn exec_arguments_are_quoted_as_the_spec_says() {
        assert_eq!(exec_arg("/usr/bin/everygrep-gui"), "/usr/bin/everygrep-gui");
        assert_eq!(exec_arg("/opt/my apps/eg"), "\"/opt/my apps/eg\"");
        assert_eq!(exec_arg("/a/\"q\"/b"), "\"/a/\\\"q\\\"/b\"");
        assert_eq!(exec_arg("/a/$HOME`x`"), "\"/a/\\$HOME\\`x\\`\"");
        assert_eq!(exec_arg("/a\\b"), "\"/a\\\\b\"");
        assert_eq!(exec_arg("/a/100%"), "/a/100%%");
        assert_eq!(exec_arg("/a b/100%"), "\"/a b/100%%\"");
        assert_eq!(exec_arg("/it's"), "\"/it's\"");
        assert_eq!(exec_arg(""), "\"\"");
        // In the file a backslash inside quotes is written four times.
        assert_eq!(escape_value(&exec_arg("/a\\b")), "\"/a\\\\\\\\b\"");
        assert_eq!(escape_value("a\nb\tc"), "a\\nb\\tc");
    }

    #[test]
    fn entry_starts_the_program_in_the_background() {
        let text = entry("/home/u/my apps/everygrep-gui");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "[Desktop Entry]");
        assert!(lines.contains(&"Type=Application"));
        assert!(lines.contains(&"Exec=\"/home/u/my apps/everygrep-gui\" --background"));
        assert!(lines.contains(&"X-GNOME-Autostart-enabled=true"));
        assert!(lines.contains(&"Icon=az.huseynz.EveryGrep"));
    }

    #[test]
    fn enabling_writes_and_disabling_removes_the_entry() {
        let scratch = Scratch::new("autostart");
        let config = scratch.0.join("config");
        let path = dir_from(Some(config.clone().into()), None)
            .unwrap()
            .join(FILE_NAME);
        assert_eq!(path, config.join("autostart").join(FILE_NAME));
        assert!(!is_enabled(&path));
        // The folder is created when missing.
        enable(&path, Path::new("/opt/eg/everygrep-gui")).unwrap();
        assert!(is_enabled(&path));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\nExec=/opt/eg/everygrep-gui --background\n"));
        // Writing again replaces the entry, with no temporary file left.
        enable(&path, Path::new("/opt/eg 2/everygrep-gui")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\nExec=\"/opt/eg 2/everygrep-gui\" --background\n"));
        let files = std::fs::read_dir(config.join("autostart")).unwrap().count();
        assert_eq!(files, 1);
        disable(&path).unwrap();
        assert!(!is_enabled(&path));
        disable(&path).unwrap();
        // A folder in the way is not taken for the entry.
        std::fs::create_dir_all(&path).unwrap();
        assert!(!is_enabled(&path));
    }

    #[cfg(unix)]
    #[test]
    fn a_path_that_is_not_utf8_is_refused() {
        use std::os::unix::ffi::OsStrExt;
        let scratch = Scratch::new("autostart-bytes");
        let path = scratch.0.join(FILE_NAME);
        let exe = Path::new(std::ffi::OsStr::from_bytes(b"/opt/\xff/eg"));
        assert!(enable(&path, exe).is_err());
        assert!(!path.exists());
    }
}
