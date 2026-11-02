//! The XDG desktop portal: choosing folders, choosing where to save a file,
//! choosing the application to open a file with, and following the
//! desktop's light or dark setting.
//!
//! The folder dialog comes from `org.freedesktop.portal.FileChooser`, so it
//! is the desktop's own file chooser and needs no toolkit of ours. "Open
//! with" is `org.freedesktop.portal.OpenURI` with `ask`, which shows the
//! desktop's own application chooser. The color scheme comes from
//! `org.freedesktop.portal.Settings`. All calls block; run them off the UI
//! thread.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::File;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use zbus::blocking::{Connection, Proxy};
use zbus::message::Message;
use zbus::zvariant::{Fd, OwnedObjectPath, OwnedValue, Value};

const DESTINATION: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const CHOOSER: &str = "org.freedesktop.portal.FileChooser";
const OPEN_URI: &str = "org.freedesktop.portal.OpenURI";
const REQUEST: &str = "org.freedesktop.portal.Request";
const SETTINGS: &str = "org.freedesktop.portal.Settings";
const APPEARANCE: &str = "org.freedesktop.appearance";
const COLOR_SCHEME: &str = "color-scheme";

/// The desktop's color scheme as the portal reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorScheme {
    /// No preference (GNOME's "Default" style, which is light).
    NoPreference,
    Dark,
    Light,
}

/// The color scheme in a portal value: `1` dark, `2` light, `0` and unknown
/// numbers no preference. The deprecated `Read` call wraps the number in one
/// more variant, which is unwrapped. `None` when there is no number.
pub fn color_scheme(value: &Value) -> Option<ColorScheme> {
    match value {
        Value::U32(1) => Some(ColorScheme::Dark),
        Value::U32(2) => Some(ColorScheme::Light),
        Value::U32(_) => Some(ColorScheme::NoPreference),
        Value::Value(inner) => color_scheme(inner),
        _ => None,
    }
}

/// Reports the desktop's color scheme to `changed`: once at the start and
/// again whenever it changes. Runs until the session bus goes away; `Err`
/// when the portal cannot be reached.
pub fn watch_color_scheme(changed: impl Fn(ColorScheme)) -> Result<(), String> {
    let conn = Connection::session().map_err(|e| e.to_string())?;
    let settings = Proxy::new(&conn, DESTINATION, PATH, SETTINGS).map_err(|e| e.to_string())?;
    // Listen before reading, so a change in between is not missed.
    let signals = settings
        .receive_signal("SettingChanged")
        .map_err(|e| e.to_string())?;
    let args = (APPEARANCE, COLOR_SCHEME);
    // Portals before version 2 only have the deprecated `Read`.
    let reply = settings
        .call_method("ReadOne", &args)
        .or_else(|_| settings.call_method("Read", &args))
        .map_err(|e| e.to_string())?;
    let value: OwnedValue = reply.body().deserialize().map_err(|e| e.to_string())?;
    if let Some(scheme) = color_scheme(&value) {
        changed(scheme);
    }
    for message in signals {
        let Ok((namespace, key, value)) =
            message.body().deserialize::<(String, String, OwnedValue)>()
        else {
            continue;
        };
        if namespace == APPEARANCE && key == COLOR_SCHEME {
            if let Some(scheme) = color_scheme(&value) {
                changed(scheme);
            }
        }
    }
    Ok(())
}

/// Asks the user for one or more folders. `Ok(None)` when the dialog was
/// cancelled; `Err` when the portal is not available.
pub fn choose_folders(title: &str, accept: &str) -> Result<Option<Vec<PathBuf>>, String> {
    let options = vec![
        ("accept_label", Value::from(accept)),
        ("modal", Value::from(true)),
        ("multiple", Value::from(true)),
        ("directory", Value::from(true)),
    ];
    let Some(results) = file_chooser("OpenFile", title, options)? else {
        return Ok(None);
    };
    Ok(Some(
        uris(&results).iter().filter_map(|u| file_path(u)).collect(),
    ))
}

/// Asks for one destination folder; cancelled requests create nothing.
pub fn choose_folder(title: &str, accept: &str) -> Result<Option<PathBuf>, String> {
    let options = vec![
        ("accept_label", Value::from(accept)),
        ("modal", Value::from(true)),
        ("multiple", Value::from(false)),
        ("directory", Value::from(true)),
    ];
    let Some(results) = file_chooser("OpenFile", title, options)? else {
        return Ok(None);
    };
    uris(&results)
        .first()
        .and_then(|u| file_path(u))
        .map(Some)
        .ok_or_else(|| "the file chooser gave no local folder".into())
}

/// Asks for one file to read; `Ok(None)` when the dialog was cancelled.
pub fn choose_file(title: &str, accept: &str) -> Result<Option<PathBuf>, String> {
    open_file(title, accept, &[])
}

/// A file type offered by the save dialog: its name and a glob pattern.
pub struct SaveFilter<'a> {
    pub name: &'a str,
    pub pattern: &'a str,
}

/// Chooses one local file for importing data. Cancellation changes nothing.
pub fn open_file(
    title: &str,
    accept: &str,
    filters: &[SaveFilter],
) -> Result<Option<PathBuf>, String> {
    let list: Vec<(&str, Vec<(u32, &str)>)> = filters
        .iter()
        .map(|f| (f.name, vec![(0u32, f.pattern)]))
        .collect();
    let mut options = vec![
        ("accept_label", Value::from(accept)),
        ("modal", Value::from(true)),
        ("multiple", Value::from(false)),
        ("directory", Value::from(false)),
    ];
    if !list.is_empty() {
        options.push(("filters", Value::from(list)));
    }
    let Some(results) = file_chooser("OpenFile", title, options)? else {
        return Ok(None);
    };
    uris(&results)
        .first()
        .and_then(|u| file_path(u))
        .map(Some)
        .ok_or_else(|| "the file chooser gave no local file".into())
}

/// Asks the user where to save a file, suggesting `current_name` and
/// offering `filters` (the first is chosen at the start). Returns the path
/// and the index of the filter that was chosen when the dialog reports it.
/// `Ok(None)` when the dialog was cancelled; `Err` when the portal is not
/// available. The dialog asks before replacing an existing file.
pub fn save_file(
    title: &str,
    accept: &str,
    current_name: &str,
    filters: &[SaveFilter],
) -> Result<Option<(PathBuf, Option<usize>)>, String> {
    // a(sa(us)): name, then (0 = glob pattern, pattern).
    let list: Vec<(&str, Vec<(u32, &str)>)> = filters
        .iter()
        .map(|f| (f.name, vec![(0u32, f.pattern)]))
        .collect();
    let mut options = vec![
        ("accept_label", Value::from(accept)),
        ("modal", Value::from(true)),
        ("current_name", Value::from(current_name)),
        ("filters", Value::from(list.clone())),
    ];
    if let Some(first) = list.first() {
        options.push(("current_filter", Value::from(first.clone())));
    }
    let Some(results) = file_chooser("SaveFile", title, options)? else {
        return Ok(None);
    };
    let Some(path) = uris(&results).first().and_then(|u| file_path(u)) else {
        return Err("the file chooser gave no local file".into());
    };
    let chosen = results
        .get("current_filter")
        .and_then(|v| <(String, Vec<(u32, String)>)>::try_from(v.try_clone().ok()?).ok())
        .and_then(|(name, _)| filters.iter().position(|f| f.name == name));
    Ok(Some((path, chosen)))
}

/// The `uris` of a file chooser response.
fn uris(results: &HashMap<String, OwnedValue>) -> Vec<String> {
    results
        .get("uris")
        .and_then(|v| Vec::<String>::try_from(v.try_clone().ok()?).ok())
        .unwrap_or_default()
}

/// Calls `method` of the file chooser portal with `options` and waits for
/// the dialog. Returns its results, or `None` when it was cancelled.
fn file_chooser(
    method: &str,
    title: &str,
    options: Vec<(&str, Value)>,
) -> Result<Option<HashMap<String, OwnedValue>>, String> {
    request(options, |conn, options| {
        conn.call_method(
            Some(DESTINATION),
            PATH,
            Some(CHOOSER),
            method,
            &("", title, options),
        )
    })
}

/// Shows the desktop's application chooser for `path` and opens it with
/// the application picked there. Returns `Ok(false)` when the chooser was
/// cancelled; `Err` when the portal is not available or the item cannot
/// be opened. Only files and folders can be opened this way.
pub fn open_with(path: &Path) -> Result<bool, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() && !meta.is_dir() {
        // Opening a pipe or a device to pass it could block or have effects.
        return Err("not a file or folder".into());
    }
    let file = File::open(path).map_err(|e| e.to_string())?;
    let options = vec![("ask", Value::from(true))];
    let results = request(options, |conn, options| {
        conn.call_method(
            Some(DESTINATION),
            PATH,
            Some(OPEN_URI),
            "OpenFile",
            &("", Fd::from(&file), options),
        )
    })?;
    Ok(results.is_some())
}

/// Makes a portal request: `call` sends it with `options`, to which a
/// handle token is added, and this waits for its `Response` signal. Returns
/// the results, `None` when the user cancelled, or an error when the portal
/// ended the request otherwise. The connection stays
/// open until the answer, since the portal closes the dialog of a caller
/// that leaves the bus.
fn request(
    mut options: Vec<(&str, Value)>,
    call: impl FnOnce(&Connection, HashMap<&str, Value>) -> zbus::Result<Message>,
) -> Result<Option<HashMap<String, OwnedValue>>, String> {
    let conn = Connection::session().map_err(|e| e.to_string())?;
    let sender = conn
        .unique_name()
        .ok_or("no name on the session bus")?
        .trim_start_matches(':')
        .replace('.', "_");
    // The reply comes as a signal on a request object whose path follows
    // from our name and this token; listen before calling so it is not
    // missed.
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let token = format!("everygrep{}_{n}", std::process::id());
    let request_path = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");
    let request = Proxy::new(&conn, DESTINATION, request_path.as_str(), REQUEST)
        .map_err(|e| e.to_string())?;
    let mut responses = request
        .receive_signal("Response")
        .map_err(|e| e.to_string())?;

    options.push(("handle_token", Value::from(token.clone())));
    let options: HashMap<&str, Value> = options.into_iter().collect();
    let reply = call(&conn, options).map_err(|e| e.to_string())?;
    let handle: OwnedObjectPath = reply.body().deserialize().map_err(|e| e.to_string())?;
    // Older portals choose the path themselves; follow the one they name.
    if handle.as_str() != request_path {
        let request =
            Proxy::new(&conn, DESTINATION, handle.as_str(), REQUEST).map_err(|e| e.to_string())?;
        responses = request
            .receive_signal("Response")
            .map_err(|e| e.to_string())?;
    }
    let message = responses.next().ok_or("the portal did not answer")?;
    let (code, results): (u32, HashMap<String, OwnedValue>) =
        message.body().deserialize().map_err(|e| e.to_string())?;
    response(code, results)
}

/// What a `Response` with `code` means for the caller: 0 the results, 1
/// (cancelled by the user) `None`, anything else (2: the dialog ended in
/// another way, such as a failure in the portal) an error.
fn response<T>(code: u32, results: T) -> Result<Option<T>, String> {
    match code {
        0 => Ok(Some(results)),
        1 => Ok(None),
        code => Err(format!("the portal ended the request (code {code})")),
    }
}

/// The local path of a `file://` URI, percent escapes decoded; `None` for
/// other URIs.
pub fn file_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let mut bytes = Vec::with_capacity(rest.len());
    let mut it = rest.bytes();
    while let Some(b) = it.next() {
        if b == b'%' {
            let hex = [it.next()?, it.next()?];
            bytes.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            bytes.push(b);
        }
    }
    Some(PathBuf::from(OsString::from_vec(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_scheme_values() {
        assert_eq!(color_scheme(&Value::U32(1)), Some(ColorScheme::Dark));
        assert_eq!(color_scheme(&Value::U32(2)), Some(ColorScheme::Light));
        assert_eq!(
            color_scheme(&Value::U32(0)),
            Some(ColorScheme::NoPreference)
        );
        assert_eq!(
            color_scheme(&Value::U32(7)),
            Some(ColorScheme::NoPreference)
        );
        // The deprecated Read wraps the value once more.
        let wrapped = Value::Value(Box::new(Value::U32(1)));
        assert_eq!(color_scheme(&wrapped), Some(ColorScheme::Dark));
        assert_eq!(color_scheme(&Value::from("dark")), None);
        assert_eq!(color_scheme(&Value::I32(1)), None);
    }

    #[test]
    fn only_a_cancel_is_quiet_and_a_failure_is_an_error() {
        assert_eq!(response(0, 5), Ok(Some(5)));
        assert_eq!(response(1, 5), Ok(None), "cancelled by the user");
        assert!(response(2, 5).is_err(), "ended in another way");
        assert!(response(7, 5).is_err());
    }

    #[test]
    fn file_uris_become_paths() {
        assert_eq!(
            file_path("file:///home/u/My%20Files"),
            Some(PathBuf::from("/home/u/My Files"))
        );
        assert_eq!(
            file_path("file:///home/u/%C5%9F%C9%99kil"),
            Some(PathBuf::from("/home/u/şəkil"))
        );
        assert_eq!(
            file_path("file://localhost/srv"),
            Some(PathBuf::from("/srv"))
        );
        let raw = file_path("file:///tmp/a%FFb").unwrap();
        assert_eq!(raw.into_os_string().into_vec(), b"/tmp/a\xffb");
        for bad in [
            "https://example.org/x",
            "file://server/share",
            "file:///bad%2",
            "file:///bad%zz",
            "/plain/path",
        ] {
            assert_eq!(file_path(bad), None, "{bad}");
        }
    }
}
