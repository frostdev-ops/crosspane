//! The control socket: one JSON request per line, one JSON response per line. `crosspanectl`
//! (and the tray, later) talk to the agent through it. The socket lives in a 0700 directory, so
//! only this user can connect.

use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use anyhow::{Context, Result};
use crosspane_input::arrange::Side;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::Event;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    /// Give input back to this node (same as the release chord).
    Release,
    /// End everything and disarm (04 §6).
    Panic,
    Rearm,
    /// Stop cleanly and start again in place (e.g. after granting OS permissions).
    Restart,
    /// Put a peer (by name or node-id prefix) on a side of this node.
    Layout {
        peer: String,
        side: Side,
    },
    /// Dial a peer address now.
    Dial {
        addr: SocketAddr,
    },
    /// List this node's windows (E2).
    Windows,
    /// Project this node's `window` to `peer` (name or node-id prefix).
    Project {
        window: u64,
        peer: String,
    },
    /// End projection `projection` of `source` (default: this node) and return the window.
    Return {
        projection: u64,
        source: Option<String>,
    },
    /// Open a 120 s pairing window (this node shows the code and confirms).
    PairListen {
        allow_input: bool,
    },
    /// Join another node's pairing window at `addr` (its normal address).
    PairJoin {
        addr: SocketAddr,
        allow_input: bool,
    },
    PairStatus,
    /// Initiator: the codes match (or not).
    PairConfirm {
        accept: bool,
    },
    /// Joiner: the code the other screen shows is candidate `index` (0-based).
    PairPick {
        index: usize,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub result: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok(result: Value) -> Response {
        Response {
            ok: true,
            result,
            error: None,
        }
    }

    pub fn err(error: impl Into<String>) -> Response {
        Response {
            ok: false,
            result: Value::Null,
            error: Some(error.into()),
        }
    }
}

/// Serve the control socket on a background thread.
pub fn serve(path: &Path, events: Sender<Event>) -> Result<()> {
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            anyhow::bail!(
                "another crosspane-agent is running ({} answers)",
                path.display()
            );
        }
        std::fs::remove_file(path).with_context(|| format!("remove stale {}", path.display()))?;
    }
    let listener = UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))?;
    std::thread::Builder::new()
        .name("ctl".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let events = events.clone();
                let _ = std::thread::Builder::new()
                    .name("ctl-conn".into())
                    .spawn(move || handle(stream, &events));
            }
        })?;
    Ok(())
}

fn handle(stream: UnixStream, events: &Sender<Event>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let Ok(line) = line else { return };
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => {
                let (tx, rx) = mpsc::channel();
                if events.send(Event::Ctl(request, tx)).is_err() {
                    Response::err("agent is shutting down")
                } else {
                    rx.recv_timeout(Duration::from_secs(5))
                        .unwrap_or_else(|_| Response::err("agent did not answer"))
                }
            }
            Err(e) => Response::err(format!("bad request: {e}")),
        };
        let Ok(text) = serde_json::to_string(&response) else {
            return;
        };
        if writeln!(writer, "{text}").is_err() {
            return;
        }
    }
}
