//! The XDG desktop portal: choosing folders and following the desktop's
//! light or dark setting.
//!
//! The folder dialog comes from `org.freedesktop.portal.FileChooser`, so it
//! is the desktop's own file chooser and needs no toolkit of ours. The color
//! scheme comes from `org.freedesktop.portal.Settings`. Both calls block;
//! run them off the UI thread.

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

const DESTINATION: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const CHOOSER: &str = "org.freedesktop.portal.FileChooser";
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

    let options: HashMap<&str, Value> = [
        ("handle_token", Value::from(token.as_str())),
        ("accept_label", Value::from(accept)),
        ("modal", Value::from(true)),
        ("multiple", Value::from(true)),
        ("directory", Value::from(true)),
    ]
    .into_iter()
    .collect();
    let reply = conn
        .call_method(
            Some(DESTINATION),
            PATH,
            Some(CHOOSER),
            "OpenFile",
            &("", title, options),
        )
        .map_err(|e| e.to_string())?;
    let handle: OwnedObjectPath = reply.body().deserialize().map_err(|e| e.to_string())?;
    // Older portals choose the path themselves; follow the one they name.
    if handle.as_str() != request_path {
        let request =
            Proxy::new(&conn, DESTINATION, handle.as_str(), REQUEST).map_err(|e| e.to_string())?;
        responses = request
            .receive_signal("Response")
            .map_err(|e| e.to_string())?;
    }
    let message = responses.next().ok_or("the file chooser did not answer")?;
    let (code, results): (u32, HashMap<String, OwnedValue>) =
        message.body().deserialize().map_err(|e| e.to_string())?;
    if code != 0 {
        return Ok(None);
    }
    let uris: Vec<String> = results
        .get("uris")
        .and_then(|v| Vec::<String>::try_from(v.try_clone().ok()?).ok())
        .unwrap_or_default();
    Ok(Some(uris.iter().filter_map(|u| file_path(u)).collect()))
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
