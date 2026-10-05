//! The network side: the QUIC transport (WP-1.5) on a small tokio runtime, and dialers that keep
//! configured peers connected.

use std::collections::HashSet;
use std::net::SocketAddr;
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
        bind: SocketAddr,
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
                        // Production uses wildcard; admitted acceptance supplies exact loopback.
                        bind,
                        identity,
                        pins,
                        hello,
                    },
                    Arc::new(move |event| {
                        let _ = sink_events.send(Event::Link(event));
                    }),
                )
            })
            .with_context(|| format!("listen on UDP {bind}"))?;
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

// The route-class probe never sends a packet and stays blocking, as before. Quinn alone changes
// its transport sockets to nonblocking. Keep the same explicit dual-stack setting on every OS.
pub(super) fn udp_socket(bind: SocketAddr) -> std::io::Result<std::net::UdpSocket> {
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(bind),
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    if bind.is_ipv6() {
        socket.set_only_v6(false)?;
    }
    socket.bind(&bind.into())?;
    Ok(socket.into())
}

#[cfg(all(test, windows))]
mod udp_factory_tests {
    use super::*;
    use std::net::Ipv6Addr;

    #[test]
    #[ignore = "Limited owned loopback socket factory only; explicit W1.8 opt-in"]
    fn owned_loopback_route_probe_dual_stack_factory() -> Result<()> {
        anyhow::ensure!(
            std::env::var("CROSSPANE_W18_LOOPBACK").as_deref() == Ok("1"),
            "explicit loopback opt-in required"
        );
        anyhow::ensure!(
            !crate::windows::security::is_elevated()?,
            "acceptance must be non-elevated"
        );
        let mapped = std::net::Ipv4Addr::LOCALHOST.to_ipv6_mapped();
        for ip in [mapped, Ipv6Addr::LOCALHOST] {
            let socket = udp_socket(SocketAddr::from((ip, 0)))?;
            anyhow::ensure!(
                !socket2::SockRef::from(&socket).only_v6()?,
                "agent route-probe socket is IPv6-only after bind"
            );
            // The production route probe uses connect/local_addr only: no packet is sent.
            socket.connect(SocketAddr::from((ip, 9)))?;
            anyhow::ensure!(
                socket.local_addr()?.ip().is_loopback() || socket.local_addr()?.ip() == mapped,
                "route escaped loopback"
            );
        }
        println!("owned_agent_probe_factory=true; after_bind_only_v6_false=2; sent_packets=0");
        Ok(())
    }
}
