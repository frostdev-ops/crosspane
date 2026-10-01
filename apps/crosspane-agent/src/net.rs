//! The network side: the QUIC transport (WP-1.5) on a small tokio runtime, and dialers that keep
//! configured peers connected.

use std::collections::HashSet;
use std::net::{Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use crosspane_protocol::link::PeerLink;
use crosspane_protocol::msg::Hello;
use crosspane_security::identity::DeviceIdentity;
use crosspane_transport::{PinStore, Transport, TransportConfig, TransportError};
use crosspane_types::id::NodeId;

use crate::agent::Event;

pub struct Net {
    runtime: tokio::runtime::Runtime,
    transport: Arc<Transport>,
    dialing: Arc<Mutex<HashSet<SocketAddr>>>,
}

impl std::fmt::Debug for Net {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Net")
            .field("transport", &self.transport)
            .finish_non_exhaustive()
    }
}

impl Net {
    pub fn start(
        port: u16,
        identity: Arc<DeviceIdentity>,
        pins: Arc<dyn PinStore>,
        hello: Hello,
        events: std::sync::mpsc::Sender<Event>,
    ) -> Result<Net> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("net")
            .enable_all()
            .build()
            .context("start tokio")?;
        let sink_events = events.clone();
        let transport = runtime
            .block_on(async move {
                Transport::bind(
                    TransportConfig {
                        // Dual-stack: IPv6 any also accepts IPv4 on Linux and macOS.
                        bind: SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)),
                        identity,
                        pins,
                        hello,
                    },
                    Arc::new(move |event| {
                        let _ = sink_events.send(Event::Link(event));
                    }),
                )
            })
            .with_context(|| format!("listen on UDP port {port}"))?;
        tracing::info!(addr = %transport.local_addr(), "listening");
        Ok(Net {
            runtime,
            transport: Arc::new(transport),
            dialing: Arc::default(),
        })
    }

    /// The transport, for the media encoder thread (`send_media` is synchronous).
    pub fn transport(&self) -> Arc<Transport> {
        self.transport.clone()
    }

    pub fn runtime(&self) -> tokio::runtime::Handle {
        self.runtime.handle().clone()
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.transport.local_addr()
    }

    pub fn link(&self, peer: NodeId) -> Option<Box<dyn PeerLink>> {
        self.transport.link(peer)
    }

    /// Keep `addr` connected: dial now, and again whenever the connection drops.
    /// One connection attempt to `addr` (a discovered candidate), no retries. Paired peers are
    /// recognised by their keys in the handshake; anything else simply fails.
    pub fn dial_once(&self, addr: SocketAddr) {
        let transport = self.transport.clone();
        self.runtime.spawn(async move {
            match transport.connect(addr).await {
                Ok(node) => tracing::debug!(%addr, peer = %node.short(), "connected (discovered)"),
                Err(e) => tracing::trace!(%addr, error = %e, "discovered candidate not reachable"),
            }
        });
    }

    pub fn dial(&self, addr: SocketAddr) {
        let Ok(mut dialing) = self.dialing.lock() else {
            return;
        };
        if !dialing.insert(addr) {
            return;
        }
        let transport = self.transport.clone();
        self.runtime.spawn(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                match transport.connect(addr).await {
                    Ok(node) => {
                        tracing::debug!(%addr, peer = %node.short(), "connected");
                        backoff = Duration::from_secs(1);
                        while transport.peers().contains(&node) {
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                    }
                    Err(TransportError::Untrusted) => {
                        tracing::warn!(%addr, "peer key not trusted; pair first");
                        tokio::time::sleep(Duration::from_secs(10)).await;
                    }
                    Err(e) => {
                        tracing::debug!(%addr, error = %e, "dial failed");
                        tokio::time::sleep(backoff).await;
                        // Short: a projection waiting out its grace period (WP-2.15) needs the
                        // link back within 20 s of the outage ending.
                        backoff = (backoff * 2).min(Duration::from_secs(3));
                    }
                }
            }
        });
    }

    pub fn shutdown(&self) {
        let transport = self.transport.clone();
        self.runtime
            .block_on(async move { transport.shutdown("agent stopping").await });
    }
}
