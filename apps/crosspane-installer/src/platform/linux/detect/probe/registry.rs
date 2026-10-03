use super::*;
use std::collections::BTreeMap;
use wayland_client::{Connection, Dispatch, QueueHandle, protocol::wl_registry};

// Literal minimums consumed by the current injection, capture interaction and HUD adapters.
pub const REQUIRED_PROTOCOLS: &[(&str, u32)] = &[
    ("wl_compositor", 4),
    ("wl_shm", 1),
    ("wl_seat", 5),
    ("wl_output", 4),
    ("zwlr_layer_shell_v1", 3),
    ("wp_cursor_shape_manager_v1", 1),
    ("zwp_pointer_constraints_v1", 1),
    ("zwp_relative_pointer_manager_v1", 1),
    ("zwp_keyboard_shortcuts_inhibit_manager_v1", 1),
    ("zwlr_virtual_pointer_manager_v1", 2),
    ("zwp_virtual_keyboard_manager_v1", 1),
    ("ext_image_copy_capture_manager_v1", 1),
    ("ext_output_image_capture_source_manager_v1", 1),
];
fn global(interface: &str, version: u32) -> Result<(), ProbeIssue> {
    logind::bounded_text(interface, 128)?;
    if interface.is_empty() || version == 0 {
        return Err(ProbeIssue::Malformed);
    }
    Ok(())
}
/// Availability only: globals are never bound and no input/capture/output object is created.
pub fn protocols_satisfy(globals: &[(String, u32)]) -> Result<bool, ProbeIssue> {
    if globals.len() > 256 {
        return Err(ProbeIssue::Oversize);
    }
    for (interface, version) in globals {
        global(interface, *version)?;
    }
    Ok(REQUIRED_PROTOCOLS.iter().all(|(name, minimum)| {
        globals
            .iter()
            .any(|(interface, version)| interface == name && version >= minimum)
    }))
}
struct Registry {
    globals: BTreeMap<u32, (String, u32)>,
    bytes: usize,
    error: Option<ProbeIssue>,
    stop: UnixStream,
}
impl Dispatch<wl_registry::WlRegistry, ()> for Registry {
    fn event(
        state: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let result = (|| {
            match event {
                wl_registry::Event::Global {
                    name,
                    interface,
                    version,
                } => {
                    global(&interface, version)?;
                    if name == 0 || state.globals.contains_key(&name) {
                        return Err(ProbeIssue::Malformed);
                    }
                    state.bytes += interface.len() + 8;
                    if state.globals.len() >= 256 || state.bytes > MAX_PROBE_BYTES {
                        return Err(ProbeIssue::Oversize);
                    }
                    state.globals.insert(name, (interface, version));
                }
                wl_registry::Event::GlobalRemove { name } => {
                    state.globals.remove(&name).ok_or(ProbeIssue::Malformed)?;
                }
                _ => return Err(ProbeIssue::Malformed),
            }
            Ok(())
        })();
        if let Err(error) = result {
            state.error.get_or_insert(error);
            let _ = state.stop.shutdown(Shutdown::Both);
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct RegistryFacts {
    pub globals: Vec<(String, u32)>,
    pub protocols: Fact<bool>,
    pub pid: u32,
}
pub(crate) fn read(stream: UnixStream, clock: CallerClock) -> Result<RegistryFacts, ProbeIssue> {
    let pid = hyprland::peer_pid(&stream)?;
    let stop = stream.try_clone().map_err(|_| ProbeIssue::Unavailable)?;
    let connection = Connection::from_socket(stream).map_err(|_| ProbeIssue::Unavailable)?;
    let mut queue = connection.new_event_queue::<Registry>();
    let mut state = Registry {
        globals: BTreeMap::new(),
        bytes: 0,
        error: None,
        stop,
    };
    let _registry = connection.display().get_registry(&queue.handle(), ());
    let result = queue.roundtrip(&mut state);
    let receipt = clock();
    if let Some(error) = state.error {
        return Err(error);
    }
    result.map_err(|_| ProbeIssue::Unavailable)?;
    let globals: Vec<_> = state.globals.into_values().collect();
    Ok(RegistryFacts {
        protocols: Fact {
            value: protocols_satisfy(&globals),
            source: ObservationSource::Demo,
            observed_at_ms: receipt,
        },
        globals,
        pid,
    })
}
/// Explicit owned streams always produce Demo; only get_registry and sync requests are sent.
pub fn registry_from_stream(
    stream: UnixStream,
    deadline: &Deadline,
    clock: CallerClock,
) -> Fact<RegistryFacts> {
    let receipt = clock.clone();
    bounded(
        stream,
        deadline,
        clock,
        ObservationSource::Demo,
        move |stream| read(stream, receipt),
    )
}
