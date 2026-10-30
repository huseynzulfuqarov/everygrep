//! Choosing folders through the desktop's file chooser.
//!
//! The dialog comes from the XDG desktop portal
//! (`org.freedesktop.portal.FileChooser`), so it is the desktop's own file
//! chooser and needs no toolkit of ours. The call blocks until the user
//! closes the dialog; run it off the UI thread.

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
