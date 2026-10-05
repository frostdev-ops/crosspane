//! JSON-line control transport over a local, authenticated Windows named pipe.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::NamedPipeServer;
use tokio::sync::Semaphore;

use super::security;
use crate::agent::Event;
use crate::ctl::{Request, Response};

const MAX_REQUEST: usize = 64 * 1024;
const MAX_CONNECTIONS: usize = 16;

/// Start only after a FIRST_PIPE_INSTANCE listener has successfully bound. The background
/// runtime keeps a listener alive while it creates its successor, preserving pipe ownership.
pub fn serve(path: &Path, events: Sender<Event>) -> Result<()> {
    let path = path.to_owned();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create control runtime")?;
    let (started, ready) = mpsc::sync_channel::<Result<()>>(1);
    std::thread::Builder::new()
        .name("ctl".into())
        .spawn(move || {
            runtime.block_on(async move {
                let first = security::create_server(&path, true);
                let identity = security::current_identity();
                let (mut listener, identity) = match (first, identity) {
                    (Ok(listener), Ok(identity)) => (listener, identity),
                    (Err(error), _) | (_, Err(error)) => {
                        let _ = started.send(Err(error));
                        return;
                    }
                };
                if started.send(Ok(())).is_err() {
                    return;
                }
                let capacity = Arc::new(Semaphore::new(MAX_CONNECTIONS));
                loop {
                    if listener.connect().await.is_err() {
                        tracing::error!("control pipe listener failed; stopping agent");
                        let _ = events.send(Event::Shutdown);
                        return;
                    }
                    // The connected instance remains alive until its successor exists. A bind
                    // failure is fatal: never drop every instance and reclaim an unowned name.
                    let next = match security::create_server(&path, false) {
                        Ok(next) => next,
                        Err(_) => {
                            tracing::error!("control pipe successor failed; stopping agent");
                            let _ = events.send(Event::Shutdown);
                            return;
                        }
                    };
                    let connected = std::mem::replace(&mut listener, next);
                    if security::verify_client(&connected, &identity).is_err() {
                        continue;
                    }
                    let Ok(permit) = capacity.clone().try_acquire_owned() else {
                        continue;
                    };
                    let events = events.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        handle(connected, events).await;
                    });
                }
            });
        })
        .context("start control thread")?;
    ready
        .recv_timeout(Duration::from_secs(10))
        .context("control listener did not start")?
}

async fn line(reader: &mut BufReader<NamedPipeServer>) -> io::Result<Option<Vec<u8>>> {
    let mut text = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if text.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "unterminated control request",
                ))
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |index| index + 1);
        if consumed > MAX_REQUEST.saturating_sub(text.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "control request is too large",
            ));
        }
        text.extend_from_slice(&available[..consumed]);
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(Some(text));
        }
    }
}

async fn handle(pipe: NamedPipeServer, events: Sender<Event>) {
    let mut reader = BufReader::new(pipe);
    loop {
        let text = match tokio::time::timeout(Duration::from_secs(30), line(&mut reader)).await {
            Ok(Ok(Some(text))) => text,
            _ => return,
        };
        let response = match serde_json::from_slice::<Request>(&text) {
            Ok(request) => {
                let (tx, rx) = mpsc::channel();
                if events.send(Event::Ctl(request, tx)).is_err() {
                    Response::err("agent is shutting down")
                } else {
                    // At most MAX_CONNECTIONS waits may use the blocking pool. The engine's
                    // original five-second response deadline remains unchanged.
                    match tokio::task::spawn_blocking(move || {
                        rx.recv_timeout(Duration::from_secs(5))
                    })
                    .await
                    {
                        Ok(Ok(response)) => response,
                        _ => Response::err("agent did not answer"),
                    }
                }
            }
            Err(error) => Response::err(format!("bad request: {error}")),
        };
        let Ok(mut text) = serde_json::to_vec(&response) else {
            return;
        };
        text.push(b'\n');
        match tokio::time::timeout(Duration::from_secs(10), reader.get_mut().write_all(&text)).await
        {
            Ok(Ok(())) => {}
            _ => return,
        }
    }
}
