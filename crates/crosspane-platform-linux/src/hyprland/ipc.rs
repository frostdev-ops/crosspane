//! Hyprland IPC (R19): one short-lived connection per request, with hard timeouts, so a stuck
//! client can never hold the compositor's synchronous request socket.
//!
//! Wire format (Hyprland 0.56, as `hyprctl` sends it): `<flags>/<command>` on
//! `$XDG_RUNTIME_DIR/hypr/<signature>/.socket.sock`, where `flags` is empty or `j` for JSON. The
//! reply is everything read until the compositor closes the connection. Events are `name>>data`
//! lines on `.socket2.sock`.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crosspane_platform::PlatformError;
use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};

/// The default bound on a whole request: connect, write and read.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(1);
/// Replies larger than this are an error (a full `clients` list is a few hundred KiB).
const MAX_REPLY: usize = 16 * 1024 * 1024;
/// How often the event thread checks whether it should stop.
const EVENT_POLL: Duration = Duration::from_millis(100);
const BACKOFF_MIN: Duration = Duration::from_millis(100);
const BACKOFF_MAX: Duration = Duration::from_secs(2);

/// A Hyprland instance's IPC endpoint. Cheap to clone; holds no connection.
#[derive(Clone, Debug)]
pub struct HyprIpc {
    dir: PathBuf,
    timeout: Duration,
}

/// Hyprland's version, from `version`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct HyprVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub commit: String,
}

/// What the event thread reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpcEvent<'a> {
    /// The event socket is (re)connected. Events may have been missed while it was down.
    Connected,
    /// One event line, split at the first `>>`.
    Event { name: &'a str, data: &'a str },
    /// The event socket dropped; the thread is reconnecting with backoff.
    Disconnected,
}

/// A running event subscription. Dropping it stops the thread within about 100 ms.
#[derive(Debug)]
pub struct EventStream {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            // The thread checks `stop` at least every EVENT_POLL; a panic in it is already logged.
            let _ = thread.join();
        }
    }
}

impl HyprIpc {
    /// The instance in `$HYPRLAND_INSTANCE_SIGNATURE` under `$XDG_RUNTIME_DIR`, with
    /// [`DEFAULT_TIMEOUT`].
    pub fn from_env() -> Result<HyprIpc, PlatformError> {
        let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
            .map_err(|_| PlatformError::Unsupported("not running under Hyprland"))?;
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .ok_or_else(|| PlatformError::Backend("XDG_RUNTIME_DIR is not set".into()))?;
        Ok(HyprIpc::new(&sig, Path::new(&runtime), DEFAULT_TIMEOUT))
    }

    pub fn new(instance_signature: &str, runtime_dir: &Path, timeout: Duration) -> HyprIpc {
        HyprIpc {
            dir: runtime_dir.join("hypr").join(instance_signature),
            timeout,
        }
    }

    /// One plain-text request (`/<command>`) on a fresh connection.
    pub fn request(&self, command: &str) -> Result<String, PlatformError> {
        self.raw(&format!("/{command}"))
    }

    /// One JSON request (`j/<command>`), parsed.
    pub fn json(&self, command: &str) -> Result<serde_json::Value, PlatformError> {
        let reply = self.raw(&format!("j/{command}"))?;
        serde_json::from_str(&reply)
            .map_err(|e| PlatformError::Backend(format!("hyprland `{command}`: bad JSON: {e}")))
    }

    /// Run a Lua chunk in the compositor (`eval`). `Ok` only if Hyprland replies `ok`.
    pub fn eval(&self, lua: &str) -> Result<(), PlatformError> {
        expect_ok("eval", &self.request(&format!("eval {lua}"))?)
    }

    /// Run a Lua dispatcher expression, e.g. `hl.dsp.focus({ window = "address:0x1" })`.
    pub fn dispatch(&self, lua: &str) -> Result<(), PlatformError> {
        expect_ok("dispatch", &self.request(&format!("dispatch {lua}"))?)
    }

    pub fn version(&self) -> Result<HyprVersion, PlatformError> {
        parse_version(&self.json("version")?)
    }

    /// The version, or `Unsupported` before 0.56 (whose Lua config and dispatchers this crate uses).
    pub fn require_supported(&self) -> Result<HyprVersion, PlatformError> {
        let v = self.version()?;
        if (v.major, v.minor) < (0, 56) {
            return Err(PlatformError::Unsupported("Hyprland older than 0.56"));
        }
        Ok(v)
    }

    /// `(name, id)` for every monitor, e.g. `("DP-3", 2)`. Backends map a `wl_output` (by its v4
    /// `name`) to the `DisplayId` the rest of Crosspane uses, which is Hyprland's monitor id.
    pub fn monitor_ids(&self) -> Result<Vec<(String, u32)>, PlatformError> {
        let monitors = self.json("monitors")?;
        let list = monitors
            .as_array()
            .ok_or_else(|| PlatformError::Backend("hyprland monitors: not a list".into()))?;
        Ok(list
            .iter()
            .filter_map(|m| {
                let name = m.get("name")?.as_str()?.to_owned();
                let id = u32::try_from(m.get("id")?.as_u64()?).ok()?;
                Some((name, id))
            })
            .collect())
    }

    /// Read `.socket2.sock` on a background thread, reconnecting with backoff (100 ms → 2 s).
    pub fn events(
        &self,
        mut on_event: Box<dyn FnMut(IpcEvent<'_>) + Send>,
    ) -> Result<EventStream, PlatformError> {
        let path = self.dir.join(".socket2.sock");
        let timeout = self.timeout;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("hypr-events".into())
            .spawn(move || event_loop(&path, timeout, &thread_stop, &mut *on_event))
            .map_err(|e| PlatformError::Backend(format!("spawn event thread: {e}")))?;
        Ok(EventStream {
            stop,
            thread: Some(thread),
        })
    }

    fn raw(&self, request: &str) -> Result<String, PlatformError> {
        let deadline = Instant::now() + self.timeout;
        let mut stream = connect(&self.dir.join(".socket.sock"), deadline)?;
        let remaining = || {
            deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(1))
        };
        stream
            .set_write_timeout(Some(remaining()))
            .map_err(io_err)?;
        stream.write_all(request.as_bytes()).map_err(io_err)?;
        let mut reply = Vec::new();
        let mut buf = [0u8; 16 * 1024];
        loop {
            if Instant::now() >= deadline {
                return Err(PlatformError::Timeout);
            }
            stream.set_read_timeout(Some(remaining())).map_err(io_err)?;
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    reply.extend_from_slice(buf.get(..n).unwrap_or_default());
                    if reply.len() > MAX_REPLY {
                        return Err(PlatformError::Backend("hyprland reply too large".into()));
                    }
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(io_err(e)),
            }
        }
        String::from_utf8(reply)
            .map_err(|_| PlatformError::Backend("hyprland reply is not UTF-8".into()))
    }
}

/// Connect without ever blocking past `deadline`. A Unix-socket connect blocks when the listener's
/// backlog is full (a stuck compositor), so connect non-blocking and retry on `EAGAIN`.
fn connect(path: &Path, deadline: Instant) -> Result<UnixStream, PlatformError> {
    let addr = SocketAddrUnix::new(path)
        .map_err(|e| PlatformError::Backend(format!("socket path {}: {e}", path.display())))?;
    let fd = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
        None,
    )
    .map_err(|e| PlatformError::Backend(format!("socket: {e}")))?;
    loop {
        match rustix::net::connect(&fd, &addr) {
            Ok(()) => break,
            Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => {
                if Instant::now() >= deadline {
                    return Err(PlatformError::Timeout);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => {
                return Err(PlatformError::Backend(format!(
                    "connect {}: {e}",
                    path.display()
                )));
            }
        }
    }
    let stream = UnixStream::from(fd);
    stream.set_nonblocking(false).map_err(io_err)?;
    Ok(stream)
}

fn event_loop(
    path: &Path,
    timeout: Duration,
    stop: &AtomicBool,
    on_event: &mut (dyn FnMut(IpcEvent<'_>) + Send),
) {
    let mut backoff = BACKOFF_MIN;
    while !stop.load(Ordering::Acquire) {
        let stream = connect(path, Instant::now() + timeout).and_then(|s| {
            s.set_read_timeout(Some(EVENT_POLL)).map_err(io_err)?;
            Ok(s)
        });
        let stream = match stream {
            Ok(stream) => stream,
            Err(e) => {
                tracing::debug!(error = %e, "hyprland event socket unavailable");
                sleep_unless_stopped(backoff, stop);
                backoff = (backoff * 2).min(BACKOFF_MAX);
                continue;
            }
        };
        backoff = BACKOFF_MIN;
        on_event(IpcEvent::Connected);
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        loop {
            if stop.load(Ordering::Acquire) {
                return;
            }
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) if line.ends_with('\n') => {
                    let text = line.trim_end_matches('\n');
                    let (name, data) = text.split_once(">>").unwrap_or((text, ""));
                    on_event(IpcEvent::Event { name, data });
                    line.clear();
                }
                // A partial line before a timeout stays in `line` and is completed next read.
                Ok(_) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                    ) => {}
                Err(e) => {
                    tracing::debug!(error = %e, "hyprland event socket read failed");
                    break;
                }
            }
        }
        on_event(IpcEvent::Disconnected);
        sleep_unless_stopped(backoff, stop);
    }
}

fn sleep_unless_stopped(total: Duration, stop: &AtomicBool) {
    let end = Instant::now() + total;
    while !stop.load(Ordering::Acquire) {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        std::thread::sleep(left.min(EVENT_POLL));
    }
}

fn expect_ok(what: &str, reply: &str) -> Result<(), PlatformError> {
    if reply.trim() == "ok" {
        Ok(())
    } else {
        Err(PlatformError::Backend(format!(
            "hyprland {what}: {}",
            reply.trim()
        )))
    }
}

fn io_err(e: std::io::Error) -> PlatformError {
    if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) {
        PlatformError::Timeout
    } else {
        PlatformError::Backend(format!("hyprland IPC: {e}"))
    }
}

fn parse_version(v: &serde_json::Value) -> Result<HyprVersion, PlatformError> {
    let text = v
        .get("version")
        .and_then(|t| t.as_str())
        .or_else(|| {
            v.get("tag")
                .and_then(|t| t.as_str())
                .map(|t| t.trim_start_matches('v'))
        })
        .ok_or_else(|| PlatformError::Backend("hyprland version: no `version` field".into()))?;
    let mut parts = text.split(['.', '-']).map(|p| p.parse::<u32>());
    let mut next = || -> Result<u32, PlatformError> {
        parts.next().and_then(Result::ok).ok_or_else(|| {
            PlatformError::Backend(format!("hyprland version: can't parse `{text}`"))
        })
    };
    let (major, minor, patch) = (next()?, next()?, next()?);
    let commit = v
        .get("commit")
        .and_then(|c| c.as_str())
        .unwrap_or_default()
        .to_owned();
    Ok(HyprVersion {
        major,
        minor,
        patch,
        commit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn parses_version() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"version":"0.56.2","commit":"efb5","tag":"v0.56.2"}"#)
                .unwrap();
        assert_eq!(
            parse_version(&v).unwrap(),
            HyprVersion {
                major: 0,
                minor: 56,
                patch: 2,
                commit: "efb5".into()
            }
        );
        let old: serde_json::Value = serde_json::from_str(r#"{"tag":"v0.41.0-12"}"#).unwrap();
        assert_eq!(parse_version(&old).unwrap().minor, 41);
        assert!(parse_version(&serde_json::json!({})).is_err());
    }

    fn fake_instance() -> (tempdir::Dir, HyprIpc) {
        let dir = tempdir::Dir::new();
        std::fs::create_dir_all(dir.0.join("hypr/sig")).unwrap();
        let ipc = HyprIpc::new("sig", &dir.0, Duration::from_millis(300));
        (dir, ipc)
    }

    #[test]
    fn request_round_trip_and_wire_format() {
        let (dir, ipc) = fake_instance();
        let listener = UnixListener::bind(dir.0.join("hypr/sig/.socket.sock")).unwrap();
        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for reply in ["ok", "{\"version\":\"0.56.2\"}", "error: nope"] {
                let (mut c, _) = listener.accept().unwrap();
                let mut buf = [0u8; 256];
                let n = c.read(&mut buf).unwrap();
                seen.push(String::from_utf8(buf[..n].to_vec()).unwrap());
                c.write_all(reply.as_bytes()).unwrap();
            }
            seen
        });
        ipc.eval("local x = 1").unwrap();
        assert_eq!(ipc.require_supported().unwrap().minor, 56);
        assert!(matches!(
            ipc.dispatch("hl.dsp.exit()"),
            Err(PlatformError::Backend(_))
        ));
        assert_eq!(
            server.join().unwrap(),
            ["/eval local x = 1", "j/version", "/dispatch hl.dsp.exit()"]
        );
    }

    #[test]
    fn silent_server_times_out() {
        let (dir, ipc) = fake_instance();
        let listener = UnixListener::bind(dir.0.join("hypr/sig/.socket.sock")).unwrap();
        let hold = std::thread::spawn(move || {
            let (c, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(800));
            drop(c);
        });
        let start = Instant::now();
        assert!(matches!(
            ipc.request("monitors"),
            Err(PlatformError::Timeout)
        ));
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "{:?}",
            start.elapsed()
        );
        hold.join().unwrap();
    }

    #[test]
    fn missing_socket_is_an_error() {
        let (_dir, ipc) = fake_instance();
        assert!(matches!(
            ipc.request("monitors"),
            Err(PlatformError::Backend(_))
        ));
    }

    #[test]
    fn events_split_reconnect_and_stop() {
        let (dir, ipc) = fake_instance();
        let path = dir.0.join("hypr/sig/.socket2.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let stream = ipc
            .events(Box::new(move |e| {
                let s = match e {
                    IpcEvent::Connected => "connected".to_owned(),
                    IpcEvent::Event { name, data } => format!("{name}|{data}"),
                    IpcEvent::Disconnected => "disconnected".to_owned(),
                };
                let _ = tx.send(s);
            }))
            .unwrap();
        for _ in 0..2 {
            let (mut c, _) = listener.accept().unwrap();
            c.write_all(b"openwindow>>abc,1,foot,title>>x\nmonitorad")
                .unwrap();
            std::thread::sleep(Duration::from_millis(150));
            c.write_all(b"ded>>DP-1\n").unwrap();
            drop(c);
        }
        let got: Vec<String> = (0..7)
            .map(|_| rx.recv_timeout(Duration::from_secs(2)).unwrap())
            .collect();
        assert_eq!(
            got,
            [
                "connected",
                "openwindow|abc,1,foot,title>>x",
                "monitoradded|DP-1",
                "disconnected",
                "connected",
                "openwindow|abc,1,foot,title>>x",
                "monitoradded|DP-1",
            ]
        );
        let start = Instant::now();
        drop(stream);
        assert!(start.elapsed() < Duration::from_millis(300));
    }

    /// A self-deleting temporary directory with a short path (Unix socket paths are limited to 108
    /// bytes).
    mod tempdir {
        use std::path::PathBuf;
        pub struct Dir(pub PathBuf);
        impl Dir {
            pub fn new() -> Dir {
                use std::sync::atomic::{AtomicU32, Ordering};
                static N: AtomicU32 = AtomicU32::new(0);
                let p = std::env::temp_dir().join(format!(
                    "cpipc-{}-{}",
                    std::process::id(),
                    N.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&p).unwrap();
                Dir(p)
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
