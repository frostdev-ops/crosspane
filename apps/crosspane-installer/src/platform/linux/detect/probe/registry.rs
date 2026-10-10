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
/// GNOME and KDE: what the portal-based backend binds itself. Input, capture and window control
/// go through portals and the compositor's own bridge, so no wlroots protocol is asked for.
/// `xdg_wm_base` hosts the projection windows; `zxdg_output_manager_v1` gives output geometry.
pub const REQUIRED_PORTAL_PROTOCOLS: &[(&str, u32)] = &[
    ("wl_compositor", 4),
    ("wl_shm", 1),
    ("wl_seat", 5),
    ("wl_output", 4),
    ("xdg_wm_base", 1),
    ("zxdg_output_manager_v1", 1),
];
/// The protocol list judged for `desktop`.
pub fn required_protocols(desktop: Desktop) -> &'static [(&'static str, u32)] {
    match desktop {
        Desktop::Hyprland => REQUIRED_PROTOCOLS,
        Desktop::Gnome | Desktop::Kde => REQUIRED_PORTAL_PROTOCOLS,
    }
}
pub const MAX_REGISTRY_GLOBALS: usize = 4096;
fn global(interface: &str, version: u32, required: &[(&str, u32)]) -> Result<(), ProbeIssue> {
    if !required.iter().any(|(name, _)| *name == interface) {
        return Ok(());
    }
    logind::bounded_text(interface, 128)?;
    if interface.is_empty() || version == 0 {
        return Err(ProbeIssue::Malformed);
    }
    Ok(())
}
/// Availability only: globals are never bound and no input/capture/output object is created.
pub fn protocols_satisfy(globals: &[(String, u32)]) -> Result<bool, ProbeIssue> {
    protocols_satisfy_for(globals, Desktop::Hyprland)
}
/// The same availability check against the list for `desktop`.
pub fn protocols_satisfy_for(
    globals: &[(String, u32)],
    desktop: Desktop,
) -> Result<bool, ProbeIssue> {
    let required = required_protocols(desktop);
    if globals.len() > MAX_REGISTRY_GLOBALS {
        return Err(ProbeIssue::Oversize);
    }
    for (interface, version) in globals {
        global(interface, *version, required)?;
    }
    Ok(required.iter().all(|(name, minimum)| {
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
    required: &'static [(&'static str, u32)],
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
                    global(&interface, version, state.required)?;
                    if name == 0 || state.globals.contains_key(&name) {
                        return Err(ProbeIssue::Malformed);
                    }
                    state.bytes += interface.len() + 8;
                    if state.globals.len() >= MAX_REGISTRY_GLOBALS || state.bytes > MAX_PROBE_BYTES
                    {
                        return Err(ProbeIssue::Oversize);
                    }
                    state.globals.insert(name, (interface, version));
                }
                wl_registry::Event::GlobalRemove { name } => {
                    state.globals.remove(&name);
                }
                _ => {}
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
pub(crate) fn read_for(
    stream: UnixStream,
    clock: CallerClock,
    desktop: Desktop,
) -> Result<RegistryFacts, ProbeIssue> {
    let pid = hyprland::peer_pid(&stream)?;
    let stop = stream.try_clone().map_err(|_| ProbeIssue::Unavailable)?;
    let connection = Connection::from_socket(stream).map_err(|_| ProbeIssue::Unavailable)?;
    let mut queue = connection.new_event_queue::<Registry>();
    let mut state = Registry {
        globals: BTreeMap::new(),
        bytes: 0,
        error: None,
        stop,
        required: required_protocols(desktop),
    };
    let _registry = connection.display().get_registry(&queue.handle(), ());
    let result = queue.roundtrip(&mut state);
    let receipt = clock();
    let completeness = state.error.map_or_else(
        || result.map(|_| ()).map_err(|_| ProbeIssue::Unavailable),
        Err,
    );
    let globals: Vec<_> = state.globals.into_values().collect();
    Ok(RegistryFacts {
        protocols: Fact {
            value: completeness.and_then(|()| protocols_satisfy_for(&globals, desktop)),
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
    registry_from_stream_for(stream, deadline, clock, Desktop::Hyprland)
}
/// The same, judged against the protocol list for `desktop`.
pub fn registry_from_stream_for(
    stream: UnixStream,
    deadline: &Deadline,
    clock: CallerClock,
    desktop: Desktop,
) -> Fact<RegistryFacts> {
    let receipt = clock.clone();
    bounded(
        stream,
        deadline,
        clock,
        ObservationSource::Demo,
        move |stream| read_for(stream, receipt, desktop),
    )
}
