//! One window per index. The first window owns a name on the session bus; a
//! later launch for the same roots and index file hands its command line to
//! that window and exits instead of opening a second one.
//!
//! The bus connection runs on zbus's own thread. The handler given to
//! [`start`] is called there and must pass the request on to the UI thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
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
/// Our own key: `true` asks the window to hide when it is in front (see
/// [`Launch::Toggle`]). A window of an older version ignores it and shows
/// itself, as for a plain launch.
const KEY_TOGGLE: &str = "toggle";

/// A running window that does not answer within this time is taken to
/// hang (see [`Stall::NoAnswer`]).
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// How this launch was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Launch {
    /// Show the window: the running one, or a new one.
    Show,
    /// Like `Show`, but a running window that is in front hides (or quits,
    /// when closing quits) instead.
    Toggle,
    /// Open another window over the same index, even when one runs; it
    /// takes the name only when none does.
    NewWindow,
    /// Started on login: a running window is left alone.
    Background,
}

/// What a later launch asks the running window to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The later launch's arguments, without the program name.
    pub args: Vec<String>,
    /// Wayland activation token (or X11 startup id) the later launch was
    /// given by the shell, if any.
    pub activation_token: Option<String>,
    /// Whether the launch was a [`Launch::Toggle`].
    pub toggle: bool,
    /// Answers the later launch when the request is dropped.
    taken: Taken,
}

impl Request {
    /// Tells the later launch that this window is closing and did not run
    /// the request; it waits for the window to end and starts its own.
    pub fn refuse(mut self) {
        if let Some(tx) = self.taken.0.take() {
            let _ = tx.send(false);
        }
    }
}

/// Held by a [`Request`] until the window has taken it (dropped it): only
/// then does `Present` answer the later launch, so a window whose UI hangs
/// is reported as not answering ([`Stall::NoAnswer`]) instead of having
/// taken the request. A copy does not hold the answer.
#[derive(Debug, Default)]
struct Taken(Option<mpsc::Sender<bool>>);

impl Drop for Taken {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(true);
        }
    }
}

impl Clone for Taken {
    fn clone(&self) -> Self {
        Self(None)
    }
}

impl PartialEq for Taken {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for Taken {}

/// Called on the bus thread for every request.
pub type Handler = Box<dyn Fn(Request) + Send + Sync>;

/// The outcome of [`start`].
pub enum Role {
    /// This process owns the name. Keep the connection alive as long as the
    /// window is open; dropping it releases the name.
    Primary(Connection),
    /// The running window took over, or has nothing to do; this process
    /// should exit.
    HandedOver,
    /// No session bus, or the bus refused the name or the call: run on our
    /// own, without a name.
    Alone,
    /// The running window is closing or hangs; ask the user what to do.
    Stalled(Stalled),
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

/// Whether received platform data asks for a toggle.
fn toggle(data: &HashMap<String, OwnedValue>) -> bool {
    data.get(KEY_TOGGLE)
        .is_some_and(|v| matches!(&**v, Value::Bool(true)))
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

/// Set once the window is closing: requests are refused from then on, so a
/// later launch knows to wait instead of handing over to a window that will
/// never show.
static CLOSING: AtomicBool = AtomicBool::new(false);

/// Marks this process as closing (see [`CLOSING`]). The name stays taken
/// until the process ends, while the index is saved.
pub fn set_closing() {
    CLOSING.store(true, Ordering::SeqCst);
}

/// Errors `Present` replies with.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "az.huseynz.EveryGrep.Error")]
enum WindowError {
    #[zbus(error)]
    ZBus(zbus::Error),
    /// The window is closing and saving the index.
    ShuttingDown(String),
}

/// D-Bus name of [`WindowError::ShuttingDown`].
const ERROR_SHUTTING_DOWN: &str = "az.huseynz.EveryGrep.Error.ShuttingDown";

/// The object a later launch calls.
struct Window {
    handler: Handler,
}

#[zbus::interface(name = "az.huseynz.EveryGrep.Window1")]
impl Window {
    /// Raises the window and applies `args` (query words, filter and
    /// matching modes) as if they had been given at its start. Answers once
    /// the window took the request; a window that does not take it in time
    /// is not answered within [`CALL_TIMEOUT`], and one that closed without
    /// running it says so ([`Request::refuse`]).
    fn present(
        &self,
        args: Vec<String>,
        platform_data: HashMap<String, OwnedValue>,
    ) -> Result<(), WindowError> {
        if CLOSING.load(Ordering::SeqCst) {
            return Err(WindowError::ShuttingDown("the window is closing".into()));
        }
        let (tx, rx) = mpsc::channel();
        (self.handler)(Request {
            args,
            activation_token: token(&platform_data),
            toggle: toggle(&platform_data),
            taken: Taken(Some(tx)),
        });
        // Longer than the caller waits: a window that hangs gives it no
        // answer at all.
        match rx.recv_timeout(CALL_TIMEOUT + Duration::from_secs(1)) {
            Ok(false) => Err(WindowError::ShuttingDown("the window is closing".into())),
            _ => Ok(()),
        }
    }
}

/// What came of calling `Present` on the window that holds the name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// It took the request.
    Done,
    /// It is closing (a window of this version says so).
    ShuttingDown,
    /// The name had no owner any more when the call arrived.
    Gone,
    /// No reply within [`CALL_TIMEOUT`]: the window hangs.
    Silent,
    /// Any other failure.
    Failed,
}

/// Why a launch cannot reach the running window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stall {
    /// It is closing and saving the index.
    ShuttingDown,
    /// It does not answer.
    NoAnswer,
}

/// What a launch does after its call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    HandedOver,
    /// Request the name again: its owner just went away.
    ClaimAgain,
    /// Wait for the window to go, and let the user decide meanwhile.
    Wait(Stall),
    /// Run without a name.
    Alone,
}

/// Launches give up claiming a name that keeps changing hands after this
/// many tries, and run on their own.
const MAX_CLAIMS: usize = 3;

/// A window that says it is closing usually releases the name within this
/// time; only after it does the launch ask the user.
const QUIET_WAIT: Duration = Duration::from_millis(1500);

/// What came of a `Present` call.
fn answer(result: &zbus::Result<zbus::Message>) -> Answer {
    match result {
        Ok(_) => Answer::Done,
        Err(zbus::Error::MethodError(name, _, _)) => match name.as_str() {
            ERROR_SHUTTING_DOWN => Answer::ShuttingDown,
            "org.freedesktop.DBus.Error.ServiceUnknown"
            | "org.freedesktop.DBus.Error.NameHasNoOwner" => Answer::Gone,
            "org.freedesktop.DBus.Error.NoReply" | "org.freedesktop.DBus.Error.Timeout" => {
                Answer::Silent
            }
            _ => Answer::Failed,
        },
        Err(zbus::Error::InputOutput(e)) if e.kind() == std::io::ErrorKind::TimedOut => {
            Answer::Silent
        }
        Err(_) => Answer::Failed,
    }
}

/// The step after answer `a`, at the `claims`-th claim of the name.
fn next_step(a: Answer, claims: usize) -> Step {
    match a {
        Answer::Done => Step::HandedOver,
        Answer::Gone if claims < MAX_CLAIMS => Step::ClaimAgain,
        Answer::Gone | Answer::Failed => Step::Alone,
        Answer::ShuttingDown => Step::Wait(Stall::ShuttingDown),
        Answer::Silent => Step::Wait(Stall::NoAnswer),
    }
}

/// A launch that found the name held by a window that cannot take its
/// request. The caller asks the user, then calls [`Stalled::retry`] once
/// the window is gone (see [`Stalled::owner_gone`], [`Stalled::terminate`]).
pub struct Stalled {
    connection: Connection,
    name: String,
    args: Vec<String>,
    launch: Launch,
    claims: usize,
    pub stall: Stall,
}

impl Stalled {
    /// Whether the name is free now.
    pub fn owner_gone(&self) -> bool {
        zbus::blocking::fdo::DBusProxy::new(&self.connection)
            .and_then(|bus| Ok(bus.name_has_owner(self.name.as_str().try_into()?)?))
            .is_ok_and(|has| !has)
    }

    /// Asks the process that holds the name to end (`SIGTERM`). Only a
    /// process of this same program is signalled.
    pub fn terminate(&self) -> std::io::Result<()> {
        let pid = zbus::blocking::fdo::DBusProxy::new(&self.connection)
            .and_then(|bus| Ok(bus.get_connection_unix_process_id(self.name.as_str().try_into()?)?))
            .map_err(std::io::Error::other)?;
        let comm = |pid: &str| std::fs::read_to_string(format!("/proc/{pid}/comm"));
        if comm(&pid.to_string())? != comm("self")? {
            return Err(std::io::Error::other("the name is held by another program"));
        }
        let pid = i32::try_from(pid).map_err(std::io::Error::other)?;
        if pid <= 1 || pid == std::process::id() as i32 {
            return Err(std::io::Error::other("refusing to signal this process"));
        }
        terminate_process(pid)
    }

    /// Claims the name again, or hands over to whoever holds it now.
    pub fn retry(self) -> Role {
        settle(
            self.connection,
            &self.name,
            &self.args,
            self.launch,
            self.claims + 1,
        )
    }
}

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

const SIGTERM: i32 = 15;

fn terminate_process(pid: i32) -> std::io::Result<()> {
    // SAFETY: kill takes plain integers and touches no memory of ours.
    let rc = unsafe { kill(pid, SIGTERM) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Claims `name`, or hands `args` to the window that already owns it. A
/// [`Launch::Background`] launch hands nothing over: the running window
/// stays as it is. A [`Launch::NewWindow`] launch that finds the name taken
/// runs on its own.
///
/// The name is requested without queueing, so of two launches at the same
/// moment the bus lets exactly one become the owner; the other hands over.
/// A window that is closing or hangs gives [`Role::Stalled`].
pub fn start(name: &str, args: &[String], launch: Launch, handler: Handler) -> Role {
    let connection = Builder::session()
        .map(|b| b.method_timeout(CALL_TIMEOUT))
        .and_then(|b| b.serve_at(OBJECT_PATH, Window { handler }))
        .and_then(Builder::build);
    let connection = match connection {
        Ok(c) => c,
        Err(e) => {
            crate::debug_log::note!("no session bus, opening a separate window: {e}");
            return Role::Alone;
        }
    };
    settle(connection, name, args, launch, 1)
}

/// One claim of `name`; `claims` counts them.
fn settle(
    connection: Connection,
    name: &str,
    args: &[String],
    launch: Launch,
    claims: usize,
) -> Role {
    match connection.request_name_with_flags(name, RequestNameFlags::DoNotQueue.into()) {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {
            Role::Primary(connection)
        }
        // zbus reports `Exists` as the `NameTaken` error.
        Ok(RequestNameReply::Exists | RequestNameReply::InQueue) | Err(zbus::Error::NameTaken) => {
            match launch {
                Launch::Background => return Role::HandedOver,
                Launch::NewWindow => return Role::Alone,
                Launch::Show | Launch::Toggle => {}
            }
            let mut data: HashMap<&str, Value> = platform_data(|k| std::env::var(k).ok())
                .into_iter()
                .map(|(k, v)| (k, Value::from(v)))
                .collect();
            if launch == Launch::Toggle {
                data.insert(KEY_TOGGLE, Value::from(true));
            }
            let result = connection.call_method(
                Some(name),
                OBJECT_PATH,
                Some(INTERFACE),
                METHOD,
                &(args, data),
            );
            match next_step(answer(&result), claims) {
                Step::HandedOver => Role::HandedOver,
                Step::ClaimAgain => settle(connection, name, args, launch, claims + 1),
                Step::Wait(stall) => {
                    let stalled = Stalled {
                        connection,
                        name: name.to_owned(),
                        args: args.to_vec(),
                        launch,
                        claims,
                        stall,
                    };
                    if stall == Stall::ShuttingDown && wait_until_gone(&stalled, QUIET_WAIT) {
                        return stalled.retry();
                    }
                    Role::Stalled(stalled)
                }
                Step::Alone => {
                    if let Err(e) = result {
                        crate::debug_log::note!(
                            "the open window did not answer, opening another: {e}"
                        );
                    }
                    Role::Alone
                }
            }
        }
        Err(e) => {
            crate::debug_log::note!("cannot claim {name}, opening a separate window: {e}");
            Role::Alone
        }
    }
}

/// Waits up to `limit` for the name to be released.
fn wait_until_gone(stalled: &Stalled, limit: Duration) -> bool {
    let deadline = std::time::Instant::now() + limit;
    loop {
        if stalled.owner_gone() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
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

    fn method_error(name: &str) -> zbus::Result<zbus::Message> {
        let reply = zbus::Message::method_call("/", "x")
            .unwrap()
            .build(&())
            .unwrap();
        Err(zbus::Error::MethodError(
            zbus::names::OwnedErrorName::try_from(name).unwrap(),
            None,
            reply,
        ))
    }

    #[test]
    fn answers_are_told_apart() {
        let timed_out = std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out");
        let cases = [
            (method_error(ERROR_SHUTTING_DOWN), Answer::ShuttingDown),
            (
                method_error("org.freedesktop.DBus.Error.ServiceUnknown"),
                Answer::Gone,
            ),
            (
                method_error("org.freedesktop.DBus.Error.NameHasNoOwner"),
                Answer::Gone,
            ),
            (
                method_error("org.freedesktop.DBus.Error.NoReply"),
                Answer::Silent,
            ),
            (Err(zbus::Error::from(timed_out)), Answer::Silent),
            (
                method_error("org.freedesktop.DBus.Error.UnknownMethod"),
                Answer::Failed,
            ),
            (Err(zbus::Error::InvalidReply), Answer::Failed),
        ];
        for (result, expected) in cases {
            assert_eq!(answer(&result), expected, "{result:?}");
        }
        let reply = zbus::Message::method_call("/", "x")
            .unwrap()
            .build(&())
            .unwrap();
        assert_eq!(answer(&Ok(reply)), Answer::Done);
    }

    #[test]
    fn a_closing_or_hanging_window_is_waited_for() {
        assert_eq!(next_step(Answer::Done, 1), Step::HandedOver);
        assert_eq!(
            next_step(Answer::ShuttingDown, 1),
            Step::Wait(Stall::ShuttingDown)
        );
        // Asked again after the user waited: still a wait, never a second
        // window next to one that is closing.
        assert_eq!(
            next_step(Answer::ShuttingDown, 9),
            Step::Wait(Stall::ShuttingDown)
        );
        assert_eq!(next_step(Answer::Silent, 1), Step::Wait(Stall::NoAnswer));
        // An owner that went away is claimed again, a few times at most.
        assert_eq!(next_step(Answer::Gone, 1), Step::ClaimAgain);
        assert_eq!(next_step(Answer::Gone, MAX_CLAIMS), Step::Alone);
        assert_eq!(next_step(Answer::Failed, 1), Step::Alone);
    }

    #[test]
    fn toggle_is_read_from_platform_data() {
        let mut data = HashMap::new();
        assert!(!toggle(&data));
        data.insert(KEY_TOGGLE.to_owned(), OwnedValue::from(true));
        assert!(toggle(&data));
        data.insert(KEY_TOGGLE.to_owned(), OwnedValue::from(false));
        assert!(!toggle(&data));
        // Another type is not taken for a toggle.
        data.insert(KEY_TOGGLE.to_owned(), OwnedValue::from(1u32));
        assert!(!toggle(&data));
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

    /// The bus tests share [`CLOSING`], so they run one at a time.
    static BUS_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A window that is closing refuses requests with its own error, which
    /// the later launch recognises. Runs only on a private bus, like the
    /// test below.
    #[test]
    #[ignore = "needs a private session bus, see the doc comment"]
    fn a_closing_window_refuses_requests() {
        if std::env::var_os("EVERYGREP_PRIVATE_BUS").is_none() {
            panic!("refusing to run on the real session bus; see the doc comment");
        }
        let _one = BUS_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let name = "az.huseynz.EveryGrep.TestClosing";
        let handler: Handler = Box::new(|_| {});
        let Role::Primary(owner) = start(name, &[], Launch::Show, handler) else {
            panic!("the first launch owns the name");
        };
        let caller = Builder::session().unwrap().build().unwrap();
        let call = || {
            caller.call_method(
                Some(name),
                OBJECT_PATH,
                Some(INTERFACE),
                METHOD,
                &(Vec::<String>::new(), HashMap::<&str, Value>::new()),
            )
        };
        assert_eq!(answer(&call()), Answer::Done);
        set_closing();
        assert_eq!(answer(&call()), Answer::ShuttingDown);
        // A launch waits quietly and takes over once the window has gone.
        let closer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(owner);
        });
        let Role::Primary(second) = start(name, &[], Launch::Show, Box::new(|_| {})) else {
            panic!("the launch takes over from the closed window");
        };
        closer.join().unwrap();
        // One that stays closing longer is reported, and retried once gone.
        let Role::Stalled(stalled) = start(name, &[], Launch::Show, Box::new(|_| {})) else {
            panic!("a window closing for long is reported");
        };
        assert_eq!(stalled.stall, Stall::ShuttingDown);
        assert!(!stalled.owner_gone());
        // The holder is this very process, which is never signalled.
        assert!(stalled.terminate().is_err());
        drop(second);
        assert!(wait_until_gone(&stalled, Duration::from_secs(2)));
        assert!(matches!(stalled.retry(), Role::Primary(_)));
        // Without an owner the bus answers for it.
        assert_eq!(answer(&call()), Answer::Gone);
        CLOSING.store(false, Ordering::SeqCst);
    }

    /// A window whose UI never takes the request (it hangs) is reported as
    /// not answering, not as having taken it. Runs only on a private bus.
    #[test]
    #[ignore = "needs a private session bus, see the doc comment"]
    fn a_window_that_never_takes_a_request_is_reported() {
        if std::env::var_os("EVERYGREP_PRIVATE_BUS").is_none() {
            panic!("refusing to run on the real session bus; see the doc comment");
        }
        let _one = BUS_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let name = "az.huseynz.EveryGrep.TestFrozen";
        // The bus thread queues requests for a UI thread that never runs.
        let queued: std::sync::Arc<std::sync::Mutex<Vec<Request>>> = Default::default();
        let handler: Handler = Box::new({
            let queued = std::sync::Arc::clone(&queued);
            move |r| queued.lock().unwrap().push(r)
        });
        let Role::Primary(_owner) = start(name, &[], Launch::Show, handler) else {
            panic!("the first launch owns the name");
        };
        let role = start(name, &["frozen".into()], Launch::Show, Box::new(|_| {}));
        let Role::Stalled(stalled) = role else {
            panic!("a window that never takes the request is reported");
        };
        assert_eq!(stalled.stall, Stall::NoAnswer);
        assert_eq!(queued.lock().unwrap()[0].args, ["frozen"]);
    }

    /// Runs only on a private bus:
    /// `EVERYGREP_PRIVATE_BUS=1 dbus-run-session -- cargo test -p everygrep-gui -- --ignored`
    #[test]
    #[ignore = "needs a private session bus, see the doc comment"]
    fn exactly_one_launch_owns_the_name_and_others_hand_over() {
        if std::env::var_os("EVERYGREP_PRIVATE_BUS").is_none() {
            panic!("refusing to run on the real session bus; see the doc comment");
        }
        let _one = BUS_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        use std::sync::mpsc;
        let (tx, rx) = mpsc::channel();
        let name = "az.huseynz.EveryGrep.Test";
        // Launches at the same moment: the bus picks one owner.
        let launches: Vec<_> = (0..8)
            .map(|i| {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    // Taken at once: the request is dropped here.
                    let handler: Handler = Box::new(move |r| tx.send(r.args.clone()).unwrap());
                    start(name, &[format!("w{i}")], Launch::Show, handler)
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
            .map(|_| rx.recv_timeout(CALL_TIMEOUT).unwrap().concat())
            .collect();
        got.sort();
        assert_eq!(got.len(), 7);
        assert!(got.iter().all(|a| a.starts_with('w')));
    }
}
