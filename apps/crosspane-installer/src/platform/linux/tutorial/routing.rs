//! Private exact-sink routing core. This module never produces tone or creates an input stream.
use crate::fixture::{FixtureError, SpeakersSelection};
use pipewire::{self as pw, spa};
use rustix::{event, fs as rfs, net, process};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    os::fd::{AsRawFd, OwnedFd},
    path::{Component, Path},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};
pub(super) type Result<T> = std::result::Result<T, FixtureError>;
fn unavailable(_: impl std::fmt::Display) -> FixtureError {
    FixtureError::OutputUnavailable
}
#[derive(Debug, Default)]
pub(super) struct Status {
    pub(super) disabled: AtomicBool,
    pub(super) removed: AtomicBool,
    pub(super) rendered: AtomicBool,
    pub(super) reply: AtomicU8,
}
impl Status {
    pub(super) fn fail(&self, code: u8) {
        self.disabled.store(true, Ordering::Release);
        let _ = self
            .reply
            .compare_exchange(0, code, Ordering::AcqRel, Ordering::Acquire);
    }
}
pub(super) fn remaining(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .ok_or(FixtureError::TimedOut)
}
pub(super) fn socket(runtime: &Path, deadline: Instant) -> Result<OwnedFd> {
    let uid = process::geteuid().as_raw();
    if !runtime.is_absolute()
        || runtime
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(FixtureError::Refused);
    }
    let flags =
        rfs::OFlags::RDONLY | rfs::OFlags::DIRECTORY | rfs::OFlags::NOFOLLOW | rfs::OFlags::CLOEXEC;
    let mut parent = rfs::open("/", flags, rfs::Mode::empty()).map_err(unavailable)?;
    for part in runtime.components() {
        remaining(deadline)?;
        let stat = rfs::fstat(&parent).map_err(unavailable)?;
        if ![0, uid].contains(&stat.st_uid)
            || stat.st_mode & 0o022 != 0 && !(stat.st_uid == 0 && stat.st_mode & 0o1000 != 0)
        {
            return Err(FixtureError::Refused);
        }
        if let Component::Normal(name) = part {
            parent = rfs::openat(&parent, name, flags, rfs::Mode::empty()).map_err(unavailable)?;
        }
    }
    let owner = rfs::fstat(&parent).map_err(unavailable)?;
    if owner.st_uid != uid || owner.st_mode & 0o777 != 0o700 {
        return Err(FixtureError::Refused);
    }
    let original =
        rfs::statat(&parent, "pipewire-0", rfs::AtFlags::SYMLINK_NOFOLLOW).map_err(unavailable)?;
    if original.st_uid != uid || original.st_mode & 0o170022 != 0o140000 {
        return Err(FixtureError::Refused);
    }
    let socket = net::socket_with(
        net::AddressFamily::UNIX,
        net::SocketType::STREAM,
        net::SocketFlags::NONBLOCK | net::SocketFlags::CLOEXEC,
        None,
    )
    .map_err(unavailable)?;
    let address =
        net::SocketAddrUnix::new(format!("/proc/self/fd/{}/pipewire-0", parent.as_raw_fd()))
            .map_err(unavailable)?;
    match net::connect(&socket, &address) {
        Ok(()) => (),
        Err(error)
            if error == rustix::io::Errno::INPROGRESS || error == rustix::io::Errno::AGAIN =>
        {
            let mut fds = [event::PollFd::new(&socket, event::PollFlags::OUT)];
            event::poll(
                &mut fds,
                Some(&remaining(deadline)?.try_into().map_err(unavailable)?),
            )
            .map_err(unavailable)?;
            net::sockopt::socket_error(&socket)
                .map_err(unavailable)?
                .map_err(unavailable)?;
        }
        Err(error) => return Err(unavailable(error)),
    }
    let peer = net::sockopt::socket_peercred(&socket).map_err(unavailable)?;
    let current =
        rfs::statat(&parent, "pipewire-0", rfs::AtFlags::SYMLINK_NOFOLLOW).map_err(unavailable)?;
    let named =
        rfs::statat(rfs::CWD, runtime, rfs::AtFlags::SYMLINK_NOFOLLOW).map_err(unavailable)?;
    if peer.uid.as_raw() != uid
        || (original.st_dev, original.st_ino) != (current.st_dev, current.st_ino)
        || (owner.st_dev, owner.st_ino) != (named.st_dev, named.st_ino)
    {
        return Err(FixtureError::Refused);
    }
    remaining(deadline)?;
    Ok(socket)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity {
    id: u32,
    serial: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Port {
    identity: Identity,
    port_id: Option<u32>,
    node: u32,
    output: Option<bool>,
    channel: Option<usize>,
    format: Option<String>,
    media_type: Option<String>,
}
impl Port {
    fn matches_header(&self, header: &Self) -> bool {
        self.identity == header.identity
            && header.port_id.is_none_or(|id| self.port_id == Some(id))
            && self.node == header.node
            && self.output == header.output
            && self.channel == header.channel
            && header
                .format
                .as_ref()
                .is_none_or(|v| self.format.as_ref() == Some(v))
            && header
                .media_type
                .as_ref()
                .is_none_or(|v| self.media_type.as_ref() == Some(v))
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Target {
    identity: Identity,
    ports: [Identity; 2],
}
fn number(props: &spa::utils::dict::DictRef, key: &str) -> Option<u64> {
    let value = props.get(key)?;
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok().filter(|v| *v > 0)
}
fn node(props: &spa::utils::dict::DictRef, key: &str, id: u32) -> Option<Identity> {
    (props.get("node.name") == Some(key)
        && props.get("media.class") == Some("Audio/Sink")
        && props.get("node.virtual") == Some("true"))
    .then_some(Identity {
        id,
        serial: number(props, "object.serial")?,
    })
}
fn port(props: &spa::utils::dict::DictRef, id: u32) -> Option<Port> {
    let value = |key| match props.get(key) {
        None => Some(None),
        Some(value) if value.len() <= 128 && value.is_ascii() => Some(Some(value.to_owned())),
        _ => None,
    };
    Some(Port {
        identity: Identity {
            id,
            serial: number(props, "object.serial")?,
        },
        port_id: match props.get("port.id") {
            None => None,
            Some(value) if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) => {
                Some(value.parse().ok()?)
            }
            _ => return None,
        },
        node: u32::try_from(number(props, "node.id")?).ok()?,
        output: match props.get("port.direction") {
            Some("out") => Some(true),
            Some("in") => Some(false),
            _ => None,
        },
        channel: match props.get("audio.channel") {
            _ if ["port.monitor", "port.physical"]
                .iter()
                .any(|key| props.get(key).is_some_and(|v| v != "false")) =>
            {
                None
            }
            Some("FL") => Some(0),
            Some("FR") => Some(1),
            _ => None,
        },
        format: value("format.dsp")?,
        media_type: value("media.type")?,
    })
}
struct Graph {
    key: String,
    nodes: BTreeMap<u32, Identity>,
    ports: BTreeMap<u32, Port>,
    links: BTreeMap<u32, (u32, u32)>,
    expected: Option<(Target, [Identity; 2])>,
    confirmed: Option<Identity>,
    confirmed_ports: BTreeMap<u32, Port>,
    stream: Option<u32>,
    status: Arc<Status>,
}
impl Graph {
    fn global(&mut self, global: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>) {
        if !matches!(
            global.type_,
            pw::types::ObjectType::Node | pw::types::ObjectType::Port | pw::types::ObjectType::Link
        ) {
            return;
        }
        let Some(props) = global.props else {
            return self.status.fail(2);
        };
        let Some(serial) = number(props, "object.serial") else {
            return self.status.fail(2);
        };
        let identity = Identity {
            id: global.id,
            serial,
        };
        if global.type_ == pw::types::ObjectType::Node && props.get("node.name") == Some(&self.key)
        {
            if props.get("media.class") != Some("Audio/Sink") || self.nodes.len() >= 4 {
                self.status.fail(2);
                return;
            }
            self.nodes.insert(global.id, identity);
            if self
                .expected
                .as_ref()
                .is_some_and(|(target, _)| target.identity != identity)
            {
                self.status.fail(3);
            }
        }
        if global.type_ == pw::types::ObjectType::Port {
            let Some(port) = port(props, global.id) else {
                return self.status.fail(2);
            };
            if self.ports.len() >= 4096 && !self.ports.contains_key(&global.id) {
                self.status.fail(2);
                return;
            }
            self.ports.insert(global.id, port);
        }
        if global.type_ == pw::types::ObjectType::Link {
            let pair = ["link.input.port", "link.output.port"]
                .map(|key| number(props, key).and_then(|id| u32::try_from(id).ok()));
            let [Some(input), Some(output)] = pair else {
                self.status.fail(3);
                return;
            };
            if self.links.len() >= 4096 && !self.links.contains_key(&global.id) {
                self.status.fail(3);
                return;
            }
            self.links.insert(global.id, (input, output));
        }
        self.check_links();
    }
    fn observe_node(&mut self, id: u32, props: &spa::utils::dict::DictRef) {
        let identity = node(props, &self.key, id);
        if identity != self.nodes.get(&id).copied()
            || self
                .confirmed
                .is_some_and(|baseline| Some(baseline) != identity)
        {
            self.status.fail(3);
        }
        self.confirmed = identity;
    }
    fn node_receipt(
        &mut self,
        id: u32,
        reported: u32,
        mask: pw::node::NodeChangeMask,
        props: Option<&spa::utils::dict::DictRef>,
    ) {
        if reported != id {
            self.status.fail(3);
            return;
        }
        if mask.contains(pw::node::NodeChangeMask::PROPS) {
            match props {
                Some(props) => self.observe_node(id, props),
                None => self.status.fail(3),
            }
        } else if self.confirmed.is_none() {
            // A partial STATE/PARAMS receipt cannot establish the first identity baseline.
            self.status.fail(3);
        }
    }
    fn observe_port(
        &mut self,
        id: u32,
        direction: spa::utils::Direction,
        props: &spa::utils::dict::DictRef,
    ) {
        let Some(observed) = port(props, id) else {
            return self.status.fail(3);
        };
        if self
            .ports
            .get(&id)
            .is_none_or(|header| !observed.matches_header(header))
            || self
                .confirmed_ports
                .get(&id)
                .is_some_and(|baseline| baseline != &observed)
            || observed.output != Some(direction == spa::utils::Direction::Output)
        {
            self.status.fail(3);
            return;
        }
        self.confirmed_ports.insert(id, observed);
    }
    fn port_receipt(
        &mut self,
        id: u32,
        reported: u32,
        direction: spa::utils::Direction,
        mask: pw::port::PortChangeMask,
        props: Option<&spa::utils::dict::DictRef>,
    ) {
        let changed = mask.contains(pw::port::PortChangeMask::PROPS);
        if reported != id
            || self.ports.get(&id).and_then(|p| p.output)
                != Some(direction == spa::utils::Direction::Output)
        {
            self.status.fail(3);
        } else if changed || !self.confirmed_ports.contains_key(&id) {
            // Native protocol omits properties on PARAMS-only receipts, exposing an empty dict.
            // Only a full PROPS receipt can establish or replace admitted property facts.
            match (changed, props) {
                (true, Some(props)) => self.observe_port(id, direction, props),
                _ => self.status.fail(3),
            }
        }
    }
    fn stream_port(&self, id: u32) -> bool {
        self.ports
            .get(&id)
            .is_some_and(|p| Some(p.node) == self.stream)
    }
    fn watch_stream(&mut self, node: u32) -> Result<()> {
        if self.stream.is_some_and(|old| old != node) {
            self.status.fail(3);
        }
        self.stream = Some(node);
        self.check_links();
        if self.status.disabled.load(Ordering::Acquire) {
            Err(FixtureError::OutputChanged)
        } else {
            Ok(())
        }
    }
    fn infos_confirmed(&self, target: &Target, own: &[Identity; 2]) -> bool {
        own.iter().chain(&target.ports).all(|id| {
            self.confirmed_ports
                .get(&id.id)
                .is_some_and(|p| p.identity == *id)
                && self
                    .confirmed_ports
                    .get(&id.id)
                    .zip(self.ports.get(&id.id))
                    .is_some_and(|(baseline, header)| baseline.matches_header(header))
        })
    }
    fn owned_count(&self) -> usize {
        self.expected.as_ref().map_or(0, |_| {
            self.links
                .values()
                .filter(|(to, from)| self.stream_port(*to) || self.stream_port(*from))
                .count()
        })
    }
    fn check_links(&self) {
        let mut channels = [false; 2];
        for port in self.ports.values().filter(|p| Some(p.node) == self.stream) {
            match (port.output, port.channel) {
                (Some(true), Some(channel)) if !channels[channel] => channels[channel] = true,
                (Some(false), _) => (),
                _ => return self.status.fail(3),
            }
        }
        let Some((target, own)) = &self.expected else {
            if self
                .links
                .values()
                .any(|(to, from)| self.stream_port(*to) || self.stream_port(*from))
            {
                self.status.fail(3);
            }
            return;
        };
        if own.iter().chain(&target.ports).any(|id| {
            self.confirmed_ports.get(&id.id).is_some_and(|baseline| {
                self.ports
                    .get(&id.id)
                    .is_none_or(|header| !baseline.matches_header(header))
            })
        }) {
            self.status.fail(3);
        }
        if self
            .stream
            .and_then(|id| self.channels(id, true).ok())
            .as_ref()
            != Some(own)
        {
            self.status.fail(3);
        }
        if self.links.values().any(|(to, from)| {
            (self.stream_port(*to) || self.stream_port(*from))
                && !(0..2).any(|c| own[c].id == *from && target.ports[c].id == *to)
        }) || (0..2).any(|c| {
            self.links
                .values()
                .filter(|(_, from)| own[c].id == *from)
                .count()
                > 1
        }) {
            self.status.fail(3);
        }
    }
    fn remove(&mut self, id: u32) {
        self.nodes.remove(&id);
        self.ports.remove(&id);
        self.confirmed_ports.remove(&id);
        let own_link = self.links.remove(&id).is_some_and(|(to, from)| {
            self.expected
                .as_ref()
                .is_some_and(|(_, own)| own.iter().any(|p| p.id == to || p.id == from))
        });
        if own_link
            || self.expected.as_ref().is_some_and(|(target, own)| {
                target.identity.id == id || target.ports.iter().chain(own).any(|p| p.id == id)
            })
        {
            self.status.fail(3);
        }
    }
    fn established(&self, active: u8, target: &Target) -> bool {
        active == 3
            && self.owned_count() == 2
            && !self.status.disabled.load(Ordering::Acquire)
            && self.target().as_ref() == Ok(target)
            && self
                .expected
                .as_ref()
                .is_some_and(|(_, own)| self.infos_confirmed(target, own))
    }
    fn pending_ports(&self, node: u32) -> Option<[Identity; 2]> {
        self.channels(node, true).ok()
    }
    fn prepare(&mut self, target: &Target, node: u32) -> Result<[Identity; 2]> {
        self.watch_stream(node)?;
        if self.target()? != *target {
            return Err(FixtureError::OutputChanged);
        }
        let own = self.channels(node, true)?;
        if self
            .expected
            .as_ref()
            .is_some_and(|old| old != &(target.clone(), own))
        {
            self.status.fail(3);
            return Err(FixtureError::OutputChanged);
        }
        self.expected = Some((target.clone(), own));
        self.check_links();
        if self.status.disabled.load(Ordering::Acquire) {
            return Err(FixtureError::OutputChanged);
        }
        Ok(own)
    }
    fn channels(&self, node: u32, output: bool) -> Result<[Identity; 2]> {
        let mut found = [None, None];
        for port in self
            .ports
            .values()
            .filter(|p| p.node == node && p.output == Some(output))
        {
            let channel = port.channel.ok_or(FixtureError::OutputUnavailable)?;
            if found[channel].replace(port.identity).is_some() {
                return Err(FixtureError::OutputUnavailable);
            }
        }
        Ok([
            found[0].ok_or(FixtureError::OutputUnavailable)?,
            found[1].ok_or(FixtureError::OutputUnavailable)?,
        ])
    }
    fn target(&self) -> Result<Target> {
        if self.nodes.len() != 1
            || self.confirmed != self.nodes.values().next().copied()
            || self.status.disabled.load(Ordering::Acquire)
        {
            return Err(FixtureError::OutputUnavailable);
        }
        let identity = *self
            .nodes
            .values()
            .next()
            .ok_or(FixtureError::OutputUnavailable)?;
        Ok(Target {
            identity,
            ports: self.channels(identity.id, false)?,
        })
    }
}
fn accept_link(
    active: &Cell<u8>,
    status: &Status,
    channel: usize,
    expected: [u32; 4],
    observed: [u32; 4],
    state: pw::link::LinkState<'_>,
) {
    let was_active = active.get() & (1 << channel) != 0;
    if observed != expected
        || was_active && !matches!(state, pw::link::LinkState::Active)
        || matches!(
            state,
            pw::link::LinkState::Error(_) | pw::link::LinkState::Unlinked
        )
    {
        status.fail(3);
    } else if matches!(state, pw::link::LinkState::Active) {
        active.set(active.get() | (1 << channel));
    }
}
/// Internal native facts only. Construction and attachment belong to the owned output worker.
pub(super) struct Route {
    _listener: pw::registry::Listener,
    _nodes: Rc<RefCell<Vec<(pw::node::NodeListener, pw::node::Node)>>>,
    _ports: Vec<(pw::port::PortListener, pw::port::Port)>,
    listeners: Vec<pw::link::LinkListener>,
    links: Vec<pw::link::Link>,
    graph: Rc<RefCell<Graph>>,
    _registry: pw::registry::RegistryRc,
    core: pw::core::CoreRc,
    target: Option<Target>,
    active: Rc<Cell<u8>>,
    attached: bool,
}
impl Route {
    pub(super) fn new(
        core: pw::core::CoreRc,
        output: &SpeakersSelection,
        status: Arc<Status>,
    ) -> Result<Self> {
        if output.device_key != format!("crosspane.{}.speaker", output.peer) {
            return Err(FixtureError::OutputUnavailable);
        }
        let registry = core.get_registry_rc().map_err(unavailable)?;
        let graph = Rc::new(RefCell::new(Graph {
            key: output.device_key.clone(),
            nodes: BTreeMap::new(),
            ports: BTreeMap::new(),
            links: BTreeMap::new(),
            expected: None,
            confirmed: None,
            confirmed_ports: BTreeMap::new(),
            stream: None,
            status,
        }));
        let add = graph.clone();
        let remove = graph.clone();
        let nodes = Rc::new(RefCell::new(Vec::with_capacity(4)));
        let watched = nodes.clone();
        let binding = registry.clone();
        let listener = registry
            .add_listener_local()
            .global(move |global| {
                add.borrow_mut().global(global);
                if global.type_ == pw::types::ObjectType::Node
                    && global.props.and_then(|p| p.get("node.name"))
                        == Some(add.borrow().key.as_str())
                {
                    if watched.borrow().len() >= 4 {
                        add.borrow().status.fail(2);
                        return;
                    }
                    let Ok(bound) = binding.bind::<pw::node::Node, _>(global) else {
                        add.borrow().status.fail(2);
                        return;
                    };
                    let observed = add.clone();
                    let id = global.id;
                    let watch = bound
                        .add_listener_local()
                        .info(move |info| {
                            observed.borrow_mut().node_receipt(
                                id,
                                info.id(),
                                info.change_mask(),
                                info.props(),
                            );
                        })
                        .register();
                    watched.borrow_mut().push((watch, bound));
                }
            })
            .global_remove(move |id| remove.borrow_mut().remove(id))
            .register();
        Ok(Self {
            core,
            graph,
            _registry: registry,
            _listener: listener,
            _nodes: nodes,
            _ports: Vec::with_capacity(4),
            target: None,
            active: Rc::new(Cell::new(0)),
            listeners: Vec::with_capacity(2),
            links: Vec::with_capacity(2),
            attached: false,
        })
    }
    /// Called after registry and bound-node info syncs, before creating the output stream.
    pub(super) fn pin_sink(&mut self) -> Result<()> {
        if self.target.is_some() {
            return Err(FixtureError::Refused);
        }
        self.target = Some(self.graph.borrow().target()?);
        Ok(())
    }
    /// None waits for this stream's two ports. Ok queues exact links; established is separate.
    pub(super) fn attach(&mut self, stream: &pw::stream::Stream) -> Option<Result<()>> {
        if self.attached {
            return Some(Err(FixtureError::Refused));
        }
        if stream.node_id() == pw::constants::ID_ANY {
            return None;
        }
        if let Err(error) = self.graph.borrow_mut().watch_stream(stream.node_id()) {
            return Some(Err(error));
        }
        let target = self.target.as_ref()?;
        let own = self.graph.borrow().pending_ports(stream.node_id())?;
        if let Err(error) = self.graph.borrow_mut().prepare(target, stream.node_id()) {
            return Some(Err(error));
        }
        if self._ports.is_empty() {
            for identity in own.iter().chain(&target.ports) {
                // The ID is from the retained registry header; bound info checks its incarnation.
                let header = pw::registry::GlobalObject::<pw::properties::PropertiesBox> {
                    id: identity.id,
                    permissions: pw::permissions::PermissionFlags::R,
                    type_: pw::types::ObjectType::Port,
                    version: pw::sys::PW_VERSION_PORT,
                    props: None,
                };
                let bound = match self._registry.bind::<pw::port::Port, _>(&header) {
                    Ok(port) => port,
                    Err(error) => {
                        self.graph.borrow().status.fail(2);
                        return Some(Err(unavailable(error)));
                    }
                };
                let graph = self.graph.clone();
                let id = identity.id;
                let listener = bound
                    .add_listener_local()
                    .info(move |info| {
                        graph.borrow_mut().port_receipt(
                            id,
                            info.id(),
                            info.direction(),
                            info.change_mask(),
                            info.props(),
                        );
                    })
                    .register();
                self._ports.push((listener, bound));
            }
        }
        if !self.graph.borrow().infos_confirmed(target, &own) {
            return None;
        }
        self.attached = true;
        Some((|| {
            self.graph.borrow_mut().prepare(target, stream.node_id())?;
            for (channel, port) in own.iter().enumerate() {
                if self.graph.borrow().target()? != *target {
                    return Err(FixtureError::OutputChanged);
                }
                let source_node = stream.node_id();
                let source = port.id;
                let destination = target.ports[channel].id;
                let target_node = target.identity.id;
                let link = self
                    .core
                    .create_object::<pw::link::Link>(
                        "link-factory",
                        &pw::properties::properties! {
                            "link.output.node" => source_node.to_string(),
                            "link.output.port" => source.to_string(),
                            "link.input.node" => target_node.to_string(),
                            "link.input.port" => destination.to_string(),
                            "link.passive" => "false",
                            "object.linger" => "false",
                        },
                    )
                    .map_err(unavailable)?;
                let active = self.active.clone();
                let status = self.graph.borrow().status.clone();
                let listener = link
                    .add_listener_local()
                    .info(move |info| {
                        accept_link(
                            &active,
                            &status,
                            channel,
                            [source_node, source, target_node, destination],
                            [
                                info.output_node_id(),
                                info.output_port_id(),
                                info.input_node_id(),
                                info.input_port_id(),
                            ],
                            info.state(),
                        )
                    })
                    .register();
                self.listeners.push(listener);
                self.links.push(link);
            }
            Ok(())
        })())
    }
    pub(super) fn established(&self) -> bool {
        self.target
            .as_ref()
            .is_some_and(|target| self.graph.borrow().established(self.active.get(), target))
    }
}

#[cfg(test)]
pub(super) mod test_support {
    use super::*;
    pub(crate) struct Model {
        graph: Graph,
        target: Option<Target>,
        active: Cell<u8>,
        own: Option<[Identity; 2]>,
        stream: u32,
    }
    impl Model {
        pub(crate) fn new(output: &SpeakersSelection) -> Self {
            // Typecheck the frozen worker boundary without constructing native objects.
            let _native = (
                Route::new,
                Route::pin_sink,
                Route::attach,
                Route::established,
                socket,
            );
            Self {
                graph: Graph {
                    key: output.device_key.clone(),
                    nodes: BTreeMap::new(),
                    ports: BTreeMap::new(),
                    links: BTreeMap::new(),
                    expected: None,
                    confirmed: None,
                    confirmed_ports: BTreeMap::new(),
                    stream: None,
                    status: Arc::new(Status::default()),
                },
                target: None,
                active: Cell::new(0),
                own: None,
                stream: 10,
            }
        }
        pub(crate) fn global(
            &mut self,
            kind: pw::types::ObjectType,
            id: u32,
            props: &[(&str, &str)],
        ) {
            let mut properties = pw::properties::PropertiesBox::new();
            for (key, value) in props {
                properties.insert(*key, *value);
            }
            self.graph.global(&pw::registry::GlobalObject {
                id,
                permissions: pw::permissions::PermissionFlags::R,
                type_: kind,
                version: 3,
                props: Some(<pw::properties::PropertiesBox as AsRef<
                    spa::utils::dict::DictRef,
                >>::as_ref(&properties)),
            });
        }
        pub(crate) fn pin(&mut self) -> Result<()> {
            self.target = Some(self.graph.target()?);
            Ok(())
        }
        pub(crate) fn attach(&mut self) -> Result<[(u32, u32); 2]> {
            let target = self
                .target
                .as_ref()
                .ok_or(FixtureError::OutputUnavailable)?;
            let own = self.graph.prepare(target, self.stream)?;
            if !self.graph.infos_confirmed(target, &own) {
                return Err(FixtureError::OutputUnavailable);
            }
            self.own = Some(own);
            Ok([
                (own[0].id, target.ports[0].id),
                (own[1].id, target.ports[1].id),
            ])
        }
        pub(crate) fn pending_attach(&mut self) -> Option<Result<[(u32, u32); 2]>> {
            if let Err(error) = self.graph.watch_stream(self.stream) {
                return Some(Err(error));
            }
            self.graph.pending_ports(self.stream)?;
            let target = self.target.as_ref()?;
            let own = match self.graph.prepare(target, self.stream) {
                Ok(own) => own,
                Err(error) => return Some(Err(error)),
            };
            if !self.graph.infos_confirmed(target, &own) {
                return None;
            }
            Some(self.attach())
        }
        pub(crate) fn requests(&self) -> usize {
            usize::from(self.own.is_some()) * 2
        }
        pub(crate) fn link_state(
            &self,
            channel: usize,
            from: u32,
            to: u32,
            state: pw::link::LinkState<'_>,
        ) {
            let target = self.target.as_ref().unwrap();
            let own = self.own.as_ref().unwrap();
            accept_link(
                &self.active,
                &self.graph.status,
                channel,
                [
                    self.stream,
                    own[channel].id,
                    target.identity.id,
                    target.ports[channel].id,
                ],
                [self.stream, from, target.identity.id, to],
                state,
            );
        }
        pub(crate) fn established(&self) -> bool {
            self.target
                .as_ref()
                .is_some_and(|t| self.graph.established(self.active.get(), t))
        }
        pub(crate) fn remove(&mut self, id: u32) {
            self.graph.remove(id);
        }
        pub(crate) fn disabled(&self) -> bool {
            self.graph.status.disabled.load(Ordering::Acquire)
        }
        pub(crate) fn node_info(&mut self, id: u32, props: &[(&str, &str)]) {
            self.node_receipt(id, id, pw::node::NodeChangeMask::PROPS, Some(props));
        }
        pub(crate) fn port_info(&mut self, id: u32, output: bool, props: &[(&str, &str)]) {
            self.port_receipt(id, id, output, pw::port::PortChangeMask::PROPS, Some(props));
        }
        pub(crate) fn port_property_change(
            &mut self,
            id: u32,
            output: bool,
            props: &[(&str, &str)],
        ) {
            self.port_receipt(id, id, output, pw::port::PortChangeMask::PROPS, Some(props));
        }
        pub(crate) fn port_receipt(
            &mut self,
            id: u32,
            reported: u32,
            output: bool,
            mask: pw::port::PortChangeMask,
            props: Option<&[(&str, &str)]>,
        ) {
            let properties = props.map(|props| {
                let mut properties = pw::properties::PropertiesBox::new();
                for (key, value) in props {
                    properties.insert(*key, *value);
                }
                properties
            });
            self.graph.port_receipt(
                id,
                reported,
                if output {
                    spa::utils::Direction::Output
                } else {
                    spa::utils::Direction::Input
                },
                mask,
                properties.as_ref().map(|properties| properties.as_ref()),
            );
        }
        pub(crate) fn node_receipt(
            &mut self,
            id: u32,
            reported: u32,
            mask: pw::node::NodeChangeMask,
            props: Option<&[(&str, &str)]>,
        ) {
            let properties = props.map(|props| {
                let mut properties = pw::properties::PropertiesBox::new();
                for (key, value) in props {
                    properties.insert(*key, *value);
                }
                properties
            });
            self.graph
                .node_receipt(id, reported, mask, properties.as_ref().map(|p| p.as_ref()));
        }
        pub(crate) fn node_property_change(&mut self, id: u32, props: &[(&str, &str)]) {
            self.node_receipt(id, id, pw::node::NodeChangeMask::PROPS, Some(props));
        }
    }

    #[test]
    fn b3a_registry_facts_and_link_requests_are_not_stream_admission() {
        let peer = crosspane_types::id::NodeId([0x31; 32]);
        let key = format!("crosspane.{peer}.speaker");
        let mut model = Model::new(&SpeakersSelection {
            peer,
            device_key: key.clone(),
        });
        model.global(
            pw::types::ObjectType::Node,
            20,
            &[
                ("object.serial", "200"),
                ("node.name", &key),
                ("media.class", "Audio/Sink"),
            ],
        );
        model.node_info(
            20,
            &[
                ("object.serial", "200"),
                ("node.name", &key),
                ("media.class", "Audio/Sink"),
                ("node.virtual", "true"),
            ],
        );
        for (id, serial, parent, direction, channel) in [
            (21, "201", "20", "in", "FL"),
            (22, "202", "20", "in", "FR"),
            (11, "101", "10", "out", "FL"),
            (12, "102", "10", "out", "FR"),
        ] {
            model.global(
                pw::types::ObjectType::Port,
                id,
                &[
                    ("object.serial", serial),
                    ("node.id", parent),
                    ("port.direction", direction),
                    ("audio.channel", channel),
                ],
            );
            model.port_info(
                id,
                direction == "out",
                &[
                    ("object.serial", serial),
                    ("node.id", parent),
                    ("port.direction", direction),
                    ("audio.channel", channel),
                ],
            );
        }
        model.pin().unwrap();
        assert_eq!(
            model.pending_attach().unwrap().unwrap(),
            [(11, 21), (12, 22)]
        );
        assert_eq!(model.requests(), 2);
        assert!(!model.established());
        assert!(!model.graph.status.rendered.load(Ordering::Acquire));
        assert!(!model.graph.status.removed.load(Ordering::Acquire));
        for (id, from, to, channel) in [(30, "11", "21", 0), (31, "12", "22", 1)] {
            model.global(
                pw::types::ObjectType::Link,
                id,
                &[
                    ("object.serial", "800"),
                    ("link.output.port", from),
                    ("link.input.port", to),
                ],
            );
            model.link_state(
                channel,
                from.parse().unwrap(),
                to.parse().unwrap(),
                pw::link::LinkState::Active,
            );
        }
        assert!(model.established());
        model.node_info(
            20,
            &[
                ("object.serial", "200"),
                ("node.name", &key),
                ("media.class", "Audio/Sink"),
                ("node.virtual", "true"),
            ],
        );
        assert!(!model.disabled());
        model.node_property_change(
            20,
            &[
                ("object.serial", "200"),
                ("node.name", &key),
                ("media.class", "Audio/Sink"),
                ("node.virtual", "true"),
            ],
        );
        assert!(
            model.established(),
            "unchanged admitted node properties remain established"
        );
        model.remove(30);
        model.port_property_change(
            11,
            true,
            &[
                ("object.serial", "101"),
                ("node.id", "10"),
                ("port.direction", "out"),
                ("audio.channel", "FL"),
            ],
        );
        assert!(model.disabled());
        assert!(!model.established());
    }
}
