//! One window per index. The first window owns a name on the session bus; a
//! later launch for the same roots and index file hands its command line to
//! that window and exits instead of opening a second one.
//!
//! The bus connection runs on zbus's own thread. The handler given to
//! [`start`] is called there and must pass the request on to the UI thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use zbus::blocking::Connection;
use zbus::blocking::connection::Builder;
use zbus::fdo::{RequestNameFlags, RequestNameReply};
use zbus::zvariant::{OwnedValue, Value};

/// Bus name of the window for the default roots and index file; equals the
/// app id.
pub const BASE_NAME: &str = "az.huseynz.EveryGrep";
const OBJECT_PATH: &str = "/az/huseynz/EveryGrep";
const INTERFACE: &str = "az.huseynz.EveryGrep.Window1";
const METHOD: &str = "Present";

/// Keys of the platform data, as `org.freedesktop.Application` names them.
const KEY_ACTIVATION_TOKEN: &str = "activation-token";
const KEY_STARTUP_ID: &str = "desktop-startup-id";

/// A running window that does not answer within this time is treated as
/// gone, and the new launch opens its own window.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// What a later launch asks the running window to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The later launch's arguments, without the program name.
    pub args: Vec<String>,
    /// Wayland activation token (or X11 startup id) the later launch was
    /// given by the shell, if any.
    pub activation_token: Option<String>,
}

/// Called on the bus thread for every request.
pub type Handler = Box<dyn Fn(Request) + Send + Sync>;

/// The outcome of [`start`].
pub enum Role {
    /// This process owns the name. Keep the connection alive as long as the
    /// window is open; dropping it releases the name.
    Primary(Connection),
    /// The running window took over; this process should exit.
    HandedOver,
    /// No session bus, or the running window did not answer: run on our own,
    /// without a name.
    Alone,
}

/// The bus name for a window over `roots` with index file `index_file`.
///
/// Both are compared after normalising (see [`identity`]). The default
/// roots with the default index file get [`BASE_NAME`]; any other
/// combination gets a name of its own, so windows over different indexes
/// never take each other's requests.
pub fn bus_name(
    roots: &[PathBuf],
    index_file: Option<&Path>,
    default_index: Option<&Path>,
) -> String {
    if roots.is_empty() && index_file == default_index {
        return BASE_NAME.to_owned();
    }
    let mut key: Vec<u8> = Vec::new();
    for root in roots {
        key.extend_from_slice(root.as_os_str().as_encoded_bytes());
        key.push(0);
    }
    key.push(1);
    if let Some(file) = index_file {
        key.extend_from_slice(file.as_os_str().as_encoded_bytes());
    }
    // A bus name element must not start with a digit.
    format!("{BASE_NAME}.Index.x{:016x}", fnv1a(&key))
}

/// Normalised roots and index file of a command line: roots as the index
/// keeps them, the index file absolute, `None` standing for the default
/// file.
pub fn identity(roots: &[PathBuf], index_file: Option<&Path>) -> (Vec<PathBuf>, Option<PathBuf>) {
    let roots = everygrep_core::scan::normalize_roots(roots);
    let file = index_file
        .map(Path::to_path_buf)
        .or_else(everygrep_core::persist::default_path)
        .map(|f| absolute(&f));
    (roots, file)
}

/// The default index file, normalised like [`identity`] does.
pub fn default_index() -> Option<PathBuf> {
    everygrep_core::persist::default_path().map(|f| absolute(&f))
}

/// `path` made absolute; symlinks in its folder are resolved when the folder
/// exists, so two spellings of one file give one name.
fn absolute(path: &Path) -> PathBuf {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    match (path.parent(), path.file_name()) {
        (Some(dir), Some(name)) => std::fs::canonicalize(dir)
            .map(|d| d.join(name))
            .unwrap_or(path),
        _ => path,
    }
}

/// 64-bit FNV-1a: stable across builds and Rust versions, unlike the
/// standard library's hasher.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// The platform data a launch sends along: the activation token from the
/// environment the shell started it with.
pub fn platform_data(env: impl Fn(&str) -> Option<String>) -> HashMap<&'static str, String> {
    let mut data = HashMap::new();
    let token = env("XDG_ACTIVATION_TOKEN").filter(|t| !t.is_empty());
    let startup_id = env("DESKTOP_STARTUP_ID").filter(|t| !t.is_empty());
    if let Some(t) = token.clone().or_else(|| startup_id.clone()) {
        data.insert(KEY_ACTIVATION_TOKEN, t);
    }
    if let Some(id) = startup_id.or(token) {
        data.insert(KEY_STARTUP_ID, id);
    }
    data
}

/// The activation token in received platform data.
fn token(data: &HashMap<String, OwnedValue>) -> Option<String> {
    [KEY_ACTIVATION_TOKEN, KEY_STARTUP_ID]
        .iter()
        .filter_map(|k| data.get(*k))
        .find_map(|v| match &**v {
            Value::Str(s) if !s.is_empty() => Some(s.to_string()),
            _ => None,
        })
}

/// The object a later launch calls.
struct Window {
    handler: Handler,
}

#[zbus::interface(name = "az.huseynz.EveryGrep.Window1")]
impl Window {
    /// Raises the window and applies `args` (query words, filter and
    /// matching modes) as if they had been given at its start.
    fn present(&self, args: Vec<String>, platform_data: HashMap<String, OwnedValue>) {
        (self.handler)(Request {
            args,
            activation_token: token(&platform_data),
        });
    }
}

/// Claims `name`, or hands `args` to the window that already owns it.
///
/// The name is requested without queueing, so of two launches at the same
/// moment the bus lets exactly one become the owner; the other hands over.
pub fn start(name: &str, args: &[String], handler: Handler) -> Role {
    let connection = Builder::session()
        .map(|b| b.method_timeout(CALL_TIMEOUT))
        .and_then(|b| b.serve_at(OBJECT_PATH, Window { handler }))
        .and_then(Builder::build);
    let connection = match connection {
        Ok(c) => c,
        Err(e) => {
            eprintln!("everygrep-gui: no session bus, opening a separate window: {e}");
            return Role::Alone;
        }
    };
    match connection.request_name_with_flags(name, RequestNameFlags::DoNotQueue.into()) {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {
            Role::Primary(connection)
        }
        // zbus reports `Exists` as the `NameTaken` error.
        Ok(RequestNameReply::Exists | RequestNameReply::InQueue) | Err(zbus::Error::NameTaken) => {
            let data: HashMap<&str, Value> = platform_data(|k| std::env::var(k).ok())
                .into_iter()
                .map(|(k, v)| (k, Value::from(v)))
                .collect();
            match connection.call_method(
                Some(name),
                OBJECT_PATH,
                Some(INTERFACE),
                METHOD,
                &(args, data),
            ) {
                Ok(_) => Role::HandedOver,
                Err(e) => {
                    eprintln!(
                        "everygrep-gui: the open window did not answer, opening another: {e}"
                    );
                    Role::Alone
                }
            }
        }
        Err(e) => {
            eprintln!("everygrep-gui: cannot claim {name}, opening a separate window: {e}");
            Role::Alone
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn default_roots_and_index_use_the_app_id() {
        let default = p("/home/u/.cache/everygrep/index.bin");
        assert_eq!(bus_name(&[], Some(&default), Some(&default)), BASE_NAME);
        assert_eq!(bus_name(&[], None, None), BASE_NAME);
    }

    #[test]
    fn other_roots_or_index_get_their_own_stable_name() {
        let default = p("/home/u/.cache/everygrep/index.bin");
        let other = p("/tmp/other.bin");
        let a = bus_name(&[p("/tmp/demo")], Some(&other), Some(&default));
        assert_eq!(a, bus_name(&[p("/tmp/demo")], Some(&other), Some(&default)));
        assert!(a.starts_with("az.huseynz.EveryGrep.Index.x"));
        assert_eq!(a.len(), BASE_NAME.len() + ".Index.x".len() + 16);
        let names = [
            a,
            bus_name(&[], Some(&other), Some(&default)),
            bus_name(&[p("/tmp/demo")], Some(&default), Some(&default)),
            bus_name(&[p("/tmp/demo"), p("/srv")], Some(&other), Some(&default)),
            // The split between roots and file is part of the key.
            bus_name(&[p("/tmp/demo.bin")], None, Some(&default)),
            bus_name(&[], Some(&p("/tmp/demo.bin")), Some(&default)),
        ];
        for (i, x) in names.iter().enumerate() {
            assert!(
                zbus::names::WellKnownName::try_from(x.as_str()).is_ok(),
                "{x}"
            );
            for y in &names[i + 1..] {
                assert_ne!(x, y);
            }
        }
    }

    #[test]
    fn identity_normalises_paths() {
        let dir = std::env::temp_dir();
        let (roots, file) = identity(&[dir.join("."), dir.clone()], Some(&dir.join("x.bin")));
        let canonical = std::fs::canonicalize(&dir).unwrap();
        assert_eq!(roots, std::slice::from_ref(&canonical));
        assert_eq!(file, Some(canonical.join("x.bin")));
    }

    #[test]
    fn platform_data_carries_the_token() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        let wayland = platform_data(env(&[("XDG_ACTIVATION_TOKEN", "abc")]));
        assert_eq!(
            wayland.get(KEY_ACTIVATION_TOKEN).map(String::as_str),
            Some("abc")
        );
        let x11 = platform_data(env(&[("DESKTOP_STARTUP_ID", "id1")]));
        assert_eq!(
            x11.get(KEY_ACTIVATION_TOKEN).map(String::as_str),
            Some("id1")
        );
        assert!(platform_data(env(&[("XDG_ACTIVATION_TOKEN", "")])).is_empty());
        assert!(platform_data(env(&[])).is_empty());
    }

    #[test]
    fn token_is_read_back_from_platform_data() {
        let received = |data: HashMap<&'static str, String>| -> HashMap<String, OwnedValue> {
            data.into_iter()
                .map(|(k, v)| (k.to_owned(), OwnedValue::try_from(Value::from(v)).unwrap()))
                .collect()
        };
        let data = received(platform_data(|k| {
            (k == "XDG_ACTIVATION_TOKEN").then(|| "tok".to_owned())
        }));
        assert_eq!(token(&data), Some("tok".to_owned()));
        assert_eq!(token(&HashMap::new()), None);
        let mut other = HashMap::new();
        other.insert(KEY_ACTIVATION_TOKEN.to_owned(), OwnedValue::from(7u32));
        assert_eq!(token(&other), None);
    }

    /// Runs only on a private bus:
    /// `EVERYGREP_PRIVATE_BUS=1 dbus-run-session -- cargo test -p everygrep-gui -- --ignored`
    #[test]
    #[ignore = "needs a private session bus, see the doc comment"]
    fn exactly_one_launch_owns_the_name_and_others_hand_over() {
        if std::env::var_os("EVERYGREP_PRIVATE_BUS").is_none() {
            panic!("refusing to run on the real session bus; see the doc comment");
        }
        use std::sync::mpsc;
        let (tx, rx) = mpsc::channel();
        let name = "az.huseynz.EveryGrep.Test";
        // Launches at the same moment: the bus picks one owner.
        let launches: Vec<_> = (0..8)
            .map(|i| {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let handler: Handler = Box::new(move |r| tx.send(r).unwrap());
                    start(name, &[format!("w{i}")], handler)
                })
            })
            .collect();
        let roles: Vec<Role> = launches.into_iter().map(|t| t.join().unwrap()).collect();
        let owners = roles
            .iter()
            .filter(|r| matches!(r, Role::Primary(_)))
            .count();
        let handed = roles
            .iter()
            .filter(|r| matches!(r, Role::HandedOver))
            .count();
        assert_eq!((owners, handed), (1, 7));
        let mut got: Vec<String> = (0..7)
            .map(|_| rx.recv_timeout(CALL_TIMEOUT).unwrap().args.concat())
            .collect();
        got.sort();
        assert_eq!(got.len(), 7);
        assert!(got.iter().all(|a| a.starts_with('w')));
    }
}
