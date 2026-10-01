//! The CLI's socket path and newline JSON exchange, run on a single background worker.

use std::io::{Read, Write};
use std::net::ToSocketAddrs;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct PlaceEntry {
    pub node: String,
    pub display: u32,
    pub origin_mm: [f64; 2],
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    Allow {
        peer: String,
        capability: String,
        allow: bool,
    },
    Forget {
        peer: String,
    },
    Place {
        placements: Vec<PlaceEntry>,
    },
    PairListen {
        allow_input: bool,
    },
    PairJoin {
        addr: String,
        allow_input: bool,
    },
    PairStatus,
    PairScan,
    PairConfirm {
        accept: bool,
    },
    PairPick {
        index: usize,
    },
    Windows,
    WindowsFrom {
        peer: String,
    },
    Project {
        window: u64,
        peer: String,
    },
    Pull {
        peer: String,
        window: u64,
    },
    Return {
        projection: u64,
        source: Option<String>,
    },
    Release,
    Panic,
    Rearm,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum Target {
    Status,
    PairStatus,
    Scan,
    LocalWindows,
    PeerWindows(String),
    Place,
    PairAction,
    Action,
}

#[derive(Debug)]
pub enum Failure {
    Unavailable,
    Message(String),
}

#[derive(Debug)]
pub struct Reply {
    pub target: Target,
    pub result: std::result::Result<Value, Failure>,
}

#[derive(Debug)]
struct Job {
    target: Target,
    request: Request,
}

#[derive(Debug)]
pub struct Worker {
    pub path: String,
    tx: Sender<Job>,
    pub replies: Receiver<Reply>,
}

impl Worker {
    pub fn start() -> Result<Self> {
        let path = socket_path();
        let path_text = match &path {
            Ok(path) => path.display().to_string(),
            Err(error) => error.to_string(),
        };
        let (tx, requests) = mpsc::channel::<Job>();
        let (results, replies) = mpsc::channel();
        std::thread::Builder::new()
            .name("settings-ctl".into())
            .spawn(move || {
                for job in requests {
                    let result = match &path {
                        Ok(path) => call(path, job.request),
                        Err(_) => Err(Failure::Unavailable),
                    };
                    if results
                        .send(Reply {
                            target: job.target,
                            result,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })?;
        Ok(Self {
            path: path_text,
            tx,
            replies,
        })
    }

    pub fn send(&self, target: Target, request: Request) -> bool {
        self.tx.send(Job { target, request }).is_ok()
    }
}

fn socket_path() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CROSSPANE_RUNTIME_DIR") {
        return Ok(PathBuf::from(dir).join("agent.sock"));
    }
    if cfg!(target_os = "macos") {
        let tmp = std::env::var_os("TMPDIR").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
        Ok(tmp.join("crosspane/agent.sock"))
    } else {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?;
        Ok(PathBuf::from(runtime).join("crosspane/agent.sock"))
    }
}

fn call(path: &Path, mut request: Request) -> std::result::Result<Value, Failure> {
    // The agent's frozen request parser expects SocketAddr. Resolve hostnames here, as the
    // CLI does, so neither DNS nor socket work happens in an egui frame.
    if let Request::PairJoin { addr, .. } = &mut request {
        let resolved = addr
            .to_socket_addrs()
            .map_err(|error| Failure::Message(format!("resolve {addr}: {error}")))?
            .next()
            .ok_or_else(|| Failure::Message("no address for host:port".into()))?;
        *addr = resolved.to_string();
    }
    let text = serde_json::to_vec(&request).map_err(|error| Failure::Message(error.to_string()))?;
    let line = exchange(path, text).map_err(|_| Failure::Unavailable)?;
    #[derive(Deserialize)]
    struct Response {
        ok: bool,
        #[serde(default)]
        result: Value,
        #[serde(default)]
        error: Option<String>,
    }
    let response: Response = serde_json::from_slice(&line)
        .map_err(|error| Failure::Message(format!("bad response from agent: {error}")))?;
    if response.ok {
        Ok(response.result)
    } else {
        Err(Failure::Message(
            response.error.unwrap_or_else(|| "request failed".into()),
        ))
    }
}

fn exchange(path: &Path, mut text: Vec<u8>) -> std::io::Result<Vec<u8>> {
    let deadline = Instant::now() + TIMEOUT;
    let mut stream = UnixStream::connect(path)?;
    stream.set_nonblocking(true)?;
    text.push(b'\n');
    let mut sent = 0;
    let mut line = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "agent call timed out",
            ));
        }
        let result = if sent < text.len() {
            stream.write(&text[sent..]).inspect(|&count| {
                sent += count;
            })
        } else {
            stream.read(&mut buffer).inspect(|&count| {
                line.extend_from_slice(&buffer[..count]);
            })
        };
        match result {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "agent closed socket",
                ));
            }
            Ok(_) => {
                if let Some(end) = line.iter().position(|byte| *byte == b'\n') {
                    line.truncate(end);
                    return Ok(line);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_request_matches_protocol() {
        let peer = "macbook".to_owned();
        let cases = vec![
            (Request::Status, json!({"cmd":"status"})),
            (
                Request::Forget { peer: peer.clone() },
                json!({"cmd":"forget","peer":"macbook"}),
            ),
            (
                Request::Place {
                    placements: vec![PlaceEntry {
                        node: "desktop".into(),
                        display: 2,
                        origin_mm: [-340.0, 12.0],
                    }],
                },
                json!({"cmd":"place","placements":[
                {"node":"desktop","display":2,"origin_mm":[-340.0,12.0]}]}),
            ),
            (
                Request::PairListen { allow_input: true },
                json!({"cmd":"pair_listen","allow_input":true}),
            ),
            (
                Request::PairJoin {
                    addr: "host:47811".into(),
                    allow_input: false,
                },
                json!({"cmd":"pair_join","addr":"host:47811","allow_input":false}),
            ),
            (Request::PairStatus, json!({"cmd":"pair_status"})),
            (Request::PairScan, json!({"cmd":"pair_scan"})),
            (
                Request::PairConfirm { accept: true },
                json!({"cmd":"pair_confirm","accept":true}),
            ),
            (
                Request::PairConfirm { accept: false },
                json!({"cmd":"pair_confirm","accept":false}),
            ),
            (
                Request::PairPick { index: 0 },
                json!({"cmd":"pair_pick","index":0}),
            ),
            (Request::Windows, json!({"cmd":"windows"})),
            (
                Request::WindowsFrom { peer: peer.clone() },
                json!({"cmd":"windows_from","peer":"macbook"}),
            ),
            (
                Request::Project {
                    window: 42,
                    peer: peer.clone(),
                },
                json!({"cmd":"project","window":42,"peer":"macbook"}),
            ),
            (
                Request::Pull {
                    peer: peer.clone(),
                    window: 42,
                },
                json!({"cmd":"pull","peer":"macbook","window":42}),
            ),
            (
                Request::Return {
                    projection: 42,
                    source: None,
                },
                json!({"cmd":"return","projection":42,"source":null}),
            ),
            (
                Request::Return {
                    projection: 42,
                    source: Some("43bd67b4e81f7cbe".into()),
                },
                json!({"cmd":"return","projection":42,"source":"43bd67b4e81f7cbe"}),
            ),
            (Request::Release, json!({"cmd":"release"})),
            (Request::Panic, json!({"cmd":"panic"})),
            (Request::Rearm, json!({"cmd":"rearm"})),
        ];
        for (request, expected) in cases {
            assert_eq!(
                serde_json::to_value(request).expect("request JSON"),
                expected
            );
        }
        for capability in ["input", "share", "browse", "present"] {
            for allow in [true, false] {
                assert_eq!(
                    serde_json::to_value(Request::Allow {
                        peer: peer.clone(),
                        capability: capability.into(),
                        allow
                    })
                    .expect("grant JSON"),
                    json!({"cmd":"allow","peer":"macbook","capability":capability,"allow":allow})
                );
            }
        }
    }
}
