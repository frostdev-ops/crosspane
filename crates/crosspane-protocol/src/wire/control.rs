//! Control-stream encoding (protobuf, schema in docs/wp/WP-1.2.md). Implemented in WP-1.2.

use crosspane_types::audio::{AudioKind, AudioStreamId};
use crosspane_types::color::ColorSpace;
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::{
    DisplayGeometry, PixelSize, PointDevice, PointLogical, PointMm, SizeMm,
};
use crosspane_types::id::{DisplayId, NodeId, ProjectionId, SessionId, WindowId};
use crosspane_types::input::LockKeys;
use prost::Message;

use super::{Frame, KIND_CONTROL, MAX_CONTROL_PAYLOAD, WIRE_VERSION, WireError};
use crate::msg::{
    Capability, ControlMessage, EndReason, Hello, Placement, Refusal, RevocationNotice,
};
use crate::projection::{
    BrowsableWindow, MAX_BROWSE_WINDOWS, ParkingKind, ProjectionEndReason, ProjectionMessage,
    WindowSummary,
};

const MAX_STRING: usize = 256;
const MAX_PROJECTION_STRING: usize = 1024;
const MAX_FEATURE: usize = 64;
const MAX_FEATURES: usize = 64;
const MAX_DISPLAYS: usize = 16;
const MAX_PLACEMENTS: usize = 64;
const MAX_CAPABILITIES: usize = 16;

/// Append one framed control message to `out`.
pub fn encode_control(msg: &ControlMessage, out: &mut Vec<u8>) -> Result<(), WireError> {
    let message = to_pb(msg)?;
    let len = message.encoded_len();
    if len > MAX_CONTROL_PAYLOAD {
        return Err(WireError::TooLarge {
            len,
            max: MAX_CONTROL_PAYLOAD,
        });
    }
    // The cap is below u32::MAX, so the header length always fits.
    let payload = message.encode_to_vec();
    out.extend_from_slice(&[WIRE_VERSION, KIND_CONTROL, 0, 0]);
    out.extend_from_slice(&(len as u32).to_le_bytes());
    out.extend_from_slice(&payload);
    Ok(())
}

/// Decode a control-stream frame.
pub fn decode_control(frame: &Frame) -> Result<ControlMessage, WireError> {
    if frame.kind != KIND_CONTROL {
        return Err(WireError::BadKind(frame.kind));
    }
    let message =
        pb::ControlMessage::decode(frame.payload.as_slice()).map_err(|_| WireError::BadControl)?;
    from_pb(message.body.ok_or(WireError::UnknownControl)?)
}

fn check_len(len: usize, max: usize, field: &'static str) -> Result<(), WireError> {
    if len > max {
        return Err(WireError::BadValue(field));
    }
    Ok(())
}

fn check_finite(values: &[f64]) -> Result<(), WireError> {
    if values.iter().any(|value| !value.is_finite()) {
        return Err(WireError::BadValue("non-finite coordinate"));
    }
    Ok(())
}

fn check_hello(name: &str, features: &[String]) -> Result<(), WireError> {
    check_len(name.len(), MAX_STRING, "hello name")?;
    check_len(features.len(), MAX_FEATURES, "feature count")?;
    for feature in features {
        check_len(feature.len(), MAX_FEATURE, "feature name")?;
    }
    Ok(())
}

fn check_display(display: &DisplayInfo) -> Result<(), WireError> {
    check_len(display.name.len(), MAX_STRING, "display name")?;
    if !display.geometry.is_valid() {
        return Err(WireError::BadValue("display geometry"));
    }
    Ok(())
}

fn check_signature(signature: &[u8]) -> Result<(), WireError> {
    if !(1..=80).contains(&signature.len()) {
        return Err(WireError::BadValue("revocation signature length"));
    }
    Ok(())
}

fn node_id(bytes: Vec<u8>) -> Result<NodeId, WireError> {
    bytes
        .try_into()
        .map(NodeId)
        .map_err(|_| WireError::BadValue("node ID length"))
}

fn displays_to_pb(displays: &[DisplayInfo]) -> Result<Vec<pb::Display>, WireError> {
    check_len(displays.len(), MAX_DISPLAYS, "display count")?;
    displays
        .iter()
        .map(|display| {
            check_display(display)?;
            let geometry = display.geometry;
            let color_space = match display.color_space {
                ColorSpace::Srgb => pb::ColorSpace::Srgb,
                ColorSpace::DisplayP3 => pb::ColorSpace::DisplayP3,
                ColorSpace::Bt709 => pb::ColorSpace::Bt709,
                _ => return Err(WireError::BadValue("color space")),
            };
            Ok(pb::Display {
                id: display.id.0,
                name: display.name.clone(),
                physical_w_mm: geometry.physical_size.width,
                physical_h_mm: geometry.physical_size.height,
                pixel_w: geometry.pixel_size.width,
                pixel_h: geometry.pixel_size.height,
                scale: geometry.scale,
                logical_x: geometry.logical_origin.x,
                logical_y: geometry.logical_origin.y,
                refresh_millihz: display.refresh_millihz,
                color_space: color_space as i32,
                hdr: display.hdr,
            })
        })
        .collect()
}

fn displays_from_pb(displays: Vec<pb::Display>) -> Result<Vec<DisplayInfo>, WireError> {
    check_len(displays.len(), MAX_DISPLAYS, "display count")?;
    displays
        .into_iter()
        .map(|display| {
            let color_space = match pb::ColorSpace::try_from(display.color_space) {
                Ok(pb::ColorSpace::Srgb) => ColorSpace::Srgb,
                Ok(pb::ColorSpace::DisplayP3) => ColorSpace::DisplayP3,
                Ok(pb::ColorSpace::Bt709) => ColorSpace::Bt709,
                Err(_) => return Err(WireError::BadValue("color space")),
            };
            let display = DisplayInfo {
                id: DisplayId(display.id),
                name: display.name,
                geometry: DisplayGeometry {
                    physical_size: SizeMm::new(display.physical_w_mm, display.physical_h_mm),
                    pixel_size: PixelSize::new(display.pixel_w, display.pixel_h),
                    scale: display.scale,
                    logical_origin: PointLogical::new(display.logical_x, display.logical_y),
                },
                refresh_millihz: display.refresh_millihz,
                color_space,
                hdr: display.hdr,
            };
            check_display(&display)?;
            Ok(display)
        })
        .collect()
}

fn lock_to_pb(value: Option<bool>) -> i32 {
    (match value {
        None => pb::LockKey::Unknown,
        Some(false) => pb::LockKey::Off,
        Some(true) => pb::LockKey::On,
    }) as i32
}

fn lock_from_pb(value: i32) -> Result<Option<bool>, WireError> {
    match pb::LockKey::try_from(value) {
        Ok(pb::LockKey::Unknown) => Ok(None),
        Ok(pb::LockKey::Off) => Ok(Some(false)),
        Ok(pb::LockKey::On) => Ok(Some(true)),
        Err(_) => Err(WireError::BadValue("lock key")),
    }
}

fn to_pb(msg: &ControlMessage) -> Result<pb::ControlMessage, WireError> {
    use pb::control_message::Body;

    let body = match msg {
        ControlMessage::Hello(hello) => {
            check_hello(&hello.name, &hello.features)?;
            Body::Hello(pb::Hello {
                minor: hello.minor,
                name: hello.name.clone(),
                features: hello.features.clone(),
                displays: displays_to_pb(&hello.displays)?,
            })
        }
        ControlMessage::Displays(displays) => Body::Displays(pb::Displays {
            displays: displays_to_pb(displays)?,
        }),
        ControlMessage::Layout(placements) => {
            check_len(placements.len(), MAX_PLACEMENTS, "placement count")?;
            let placements = placements
                .iter()
                .map(|placement| {
                    check_finite(&[placement.origin.x, placement.origin.y])?;
                    Ok(pb::Placement {
                        node: placement.node.0.to_vec(),
                        display: placement.display.0,
                        origin_x_mm: placement.origin.x,
                        origin_y_mm: placement.origin.y,
                        version: placement.version,
                    })
                })
                .collect::<Result<_, WireError>>()?;
            Body::Layout(pb::Layout { placements })
        }
        ControlMessage::StartControl {
            session,
            entry_display,
            entry,
            lock_keys,
        } => {
            check_finite(&[entry.x, entry.y])?;
            Body::StartControl(pb::StartControl {
                session: session.0,
                entry_display: entry_display.0,
                entry_x: entry.x,
                entry_y: entry.y,
                lock_keys: Some(pb::LockKeys {
                    caps: lock_to_pb(lock_keys.caps_lock),
                    num: lock_to_pb(lock_keys.num_lock),
                    scroll: lock_to_pb(lock_keys.scroll_lock),
                }),
            })
        }
        ControlMessage::ControlStarted { session } => {
            Body::ControlStarted(pb::ControlStarted { session: session.0 })
        }
        ControlMessage::ControlRefused { session, reason } => {
            let reason = match reason {
                Refusal::Permission => pb::Refusal::Permission,
                Refusal::Locked => pb::Refusal::Locked,
                Refusal::SecureInput => pb::Refusal::SecureInput,
                Refusal::Busy => pb::Refusal::Busy,
                Refusal::InjectorFailed => pb::Refusal::InjectorFailed,
            };
            Body::ControlRefused(pb::ControlRefused {
                session: session.0,
                reason: reason as i32,
            })
        }
        ControlMessage::EndControl { session, reason } => {
            let reason = match reason {
                EndReason::Released => pb::EndReason::Released,
                EndReason::Panic => pb::EndReason::Panic,
                EndReason::TargetLocked => pb::EndReason::TargetLocked,
                EndReason::ControllerLocked => pb::EndReason::ControllerLocked,
                EndReason::LinkLost => pb::EndReason::LinkLost,
                EndReason::Revoked => pb::EndReason::Revoked,
            };
            Body::EndControl(pb::EndControl {
                session: session.0,
                reason: reason as i32,
            })
        }
        ControlMessage::Grants(capabilities) => {
            check_len(capabilities.len(), MAX_CAPABILITIES, "capability count")?;
            let capabilities = capabilities
                .iter()
                .map(|capability| {
                    (match capability {
                        Capability::InputAccept => pb::Capability::InputAccept,
                        Capability::WindowShare => pb::Capability::WindowShare,
                        Capability::WindowBrowse => pb::Capability::WindowBrowse,
                        Capability::WindowPresent => pb::Capability::WindowPresent,
                        Capability::AudioSpeaker => pb::Capability::AudioSpeaker,
                        Capability::AudioMic => pb::Capability::AudioMic,
                    }) as i32
                })
                .collect();
            Body::Grants(pb::Grants { capabilities })
        }
        ControlMessage::Revocation(notice) => {
            check_signature(&notice.signature)?;
            Body::Revocation(pb::Revocation {
                revoked: notice.revoked.0.to_vec(),
                issuer: notice.issuer.0.to_vec(),
                issued_at_ms: notice.issued_at_ms,
                signature: notice.signature.clone(),
            })
        }
        ControlMessage::Ping { t0 } => Body::Ping(pb::Ping { t0: *t0 }),
        ControlMessage::Pong { t0, t1, t2 } => Body::Pong(pb::Pong {
            t0: *t0,
            t1: *t1,
            t2: *t2,
        }),
        ControlMessage::AudioOpen {
            stream,
            kind,
            channels,
        } => {
            check_audio_stream(stream.0 as u32)?;
            if u16::from(*channels) != kind.format().channels {
                return Err(WireError::BadValue("audio channels"));
            }
            Body::AudioOpen(pb::AudioOpen {
                stream: u32::from(stream.0),
                kind: match kind {
                    AudioKind::Speaker => 1,
                    AudioKind::Microphone => 2,
                },
                channels: u32::from(*channels),
            })
        }
        ControlMessage::AudioOpened { stream } => Body::AudioOpened(pb::AudioStream {
            stream: check_audio_stream(u32::from(stream.0))?.0.into(),
        }),
        ControlMessage::AudioRefused { stream, reason } => Body::AudioRefused(pb::AudioRefused {
            stream: check_audio_stream(u32::from(stream.0))?.0.into(),
            reason: projection_refusal_to_pb(*reason),
        }),
        ControlMessage::AudioClose { stream } => Body::AudioClose(pb::AudioStream {
            stream: check_audio_stream(u32::from(stream.0))?.0.into(),
        }),
        ControlMessage::Projection(message) => Body::Projection(projection_to_pb(message)?),
        ControlMessage::Goodbye { message } => {
            check_len(message.len(), MAX_STRING, "goodbye message")?;
            Body::Goodbye(pb::Goodbye {
                message: message.clone(),
            })
        }
    };
    Ok(pb::ControlMessage { body: Some(body) })
}

fn from_pb(body: pb::control_message::Body) -> Result<ControlMessage, WireError> {
    use pb::control_message::Body;

    Ok(match body {
        Body::Hello(hello) => {
            check_hello(&hello.name, &hello.features)?;
            ControlMessage::Hello(Hello {
                minor: hello.minor,
                name: hello.name,
                features: hello.features,
                displays: displays_from_pb(hello.displays)?,
            })
        }
        Body::Displays(displays) => ControlMessage::Displays(displays_from_pb(displays.displays)?),
        Body::Layout(layout) => {
            check_len(layout.placements.len(), MAX_PLACEMENTS, "placement count")?;
            let placements = layout
                .placements
                .into_iter()
                .map(|placement| {
                    check_finite(&[placement.origin_x_mm, placement.origin_y_mm])?;
                    Ok(Placement {
                        node: node_id(placement.node)?,
                        display: DisplayId(placement.display),
                        origin: PointMm::new(placement.origin_x_mm, placement.origin_y_mm),
                        version: placement.version,
                    })
                })
                .collect::<Result<_, WireError>>()?;
            ControlMessage::Layout(placements)
        }
        Body::StartControl(start) => {
            check_finite(&[start.entry_x, start.entry_y])?;
            let locks = start.lock_keys.unwrap_or_default();
            ControlMessage::StartControl {
                session: SessionId(start.session),
                entry_display: DisplayId(start.entry_display),
                entry: PointDevice::new(start.entry_x, start.entry_y),
                lock_keys: LockKeys {
                    caps_lock: lock_from_pb(locks.caps)?,
                    num_lock: lock_from_pb(locks.num)?,
                    scroll_lock: lock_from_pb(locks.scroll)?,
                },
            }
        }
        Body::ControlStarted(started) => ControlMessage::ControlStarted {
            session: SessionId(started.session),
        },
        Body::ControlRefused(refused) => {
            let reason = match pb::Refusal::try_from(refused.reason) {
                Ok(pb::Refusal::Permission) => Refusal::Permission,
                Ok(pb::Refusal::Locked) => Refusal::Locked,
                Ok(pb::Refusal::SecureInput) => Refusal::SecureInput,
                Ok(pb::Refusal::Busy) => Refusal::Busy,
                Ok(pb::Refusal::InjectorFailed) => Refusal::InjectorFailed,
                _ => return Err(WireError::BadValue("refusal")),
            };
            ControlMessage::ControlRefused {
                session: SessionId(refused.session),
                reason,
            }
        }
        Body::EndControl(end) => {
            let reason = match pb::EndReason::try_from(end.reason) {
                Ok(pb::EndReason::Released) => EndReason::Released,
                Ok(pb::EndReason::Panic) => EndReason::Panic,
                Ok(pb::EndReason::TargetLocked) => EndReason::TargetLocked,
                Ok(pb::EndReason::ControllerLocked) => EndReason::ControllerLocked,
                Ok(pb::EndReason::LinkLost) => EndReason::LinkLost,
                Ok(pb::EndReason::Revoked) => EndReason::Revoked,
                _ => return Err(WireError::BadValue("end reason")),
            };
            ControlMessage::EndControl {
                session: SessionId(end.session),
                reason,
            }
        }
        Body::Grants(grants) => {
            check_len(
                grants.capabilities.len(),
                MAX_CAPABILITIES,
                "capability count",
            )?;
            let capabilities = grants
                .capabilities
                .into_iter()
                .map(|capability| match pb::Capability::try_from(capability) {
                    Ok(pb::Capability::InputAccept) => Ok(Capability::InputAccept),
                    Ok(pb::Capability::WindowShare) => Ok(Capability::WindowShare),
                    Ok(pb::Capability::WindowBrowse) => Ok(Capability::WindowBrowse),
                    Ok(pb::Capability::WindowPresent) => Ok(Capability::WindowPresent),
                    Ok(pb::Capability::AudioSpeaker) => Ok(Capability::AudioSpeaker),
                    Ok(pb::Capability::AudioMic) => Ok(Capability::AudioMic),
                    _ => Err(WireError::BadValue("capability")),
                })
                .collect::<Result<_, _>>()?;
            ControlMessage::Grants(capabilities)
        }
        Body::Revocation(notice) => {
            check_signature(&notice.signature)?;
            ControlMessage::Revocation(RevocationNotice {
                revoked: node_id(notice.revoked)?,
                issuer: node_id(notice.issuer)?,
                issued_at_ms: notice.issued_at_ms,
                signature: notice.signature,
            })
        }
        Body::Ping(ping) => ControlMessage::Ping { t0: ping.t0 },
        Body::Pong(pong) => ControlMessage::Pong {
            t0: pong.t0,
            t1: pong.t1,
            t2: pong.t2,
        },
        Body::AudioOpen(open) => {
            let kind = match open.kind {
                1 => AudioKind::Speaker,
                2 => AudioKind::Microphone,
                _ => return Err(WireError::BadValue("audio kind")),
            };
            if open.channels != u32::from(kind.format().channels) {
                return Err(WireError::BadValue("audio channels"));
            }
            ControlMessage::AudioOpen {
                stream: check_audio_stream(open.stream)?,
                kind,
                channels: open.channels as u8,
            }
        }
        Body::AudioOpened(open) => ControlMessage::AudioOpened {
            stream: check_audio_stream(open.stream)?,
        },
        Body::AudioRefused(refused) => ControlMessage::AudioRefused {
            stream: check_audio_stream(refused.stream)?,
            reason: projection_refusal_from_pb(refused.reason)?,
        },
        Body::AudioClose(close) => ControlMessage::AudioClose {
            stream: check_audio_stream(close.stream)?,
        },
        Body::Projection(projection) => ControlMessage::Projection(projection_from_pb(projection)?),
        Body::Goodbye(goodbye) => {
            check_len(goodbye.message.len(), MAX_STRING, "goodbye message")?;
            ControlMessage::Goodbye {
                message: goodbye.message,
            }
        }
    })
}

fn check_audio_stream(stream: u32) -> Result<AudioStreamId, WireError> {
    let value = u16::try_from(stream).map_err(|_| WireError::BadValue("audio stream"))?;
    if value == 0 {
        return Err(WireError::BadValue("audio stream"));
    }
    Ok(AudioStreamId(value))
}

fn check_projection_string(value: &str) -> Result<(), WireError> {
    check_len(value.len(), MAX_PROJECTION_STRING, "projection string")
}

fn check_projection_scale(scale: f64) -> Result<(), WireError> {
    if !scale.is_finite() || scale <= 0.0 {
        return Err(WireError::BadValue("projection scale"));
    }
    Ok(())
}

fn projection_refusal_to_pb(reason: Refusal) -> u32 {
    match reason {
        Refusal::Permission => 1,
        Refusal::Locked => 2,
        Refusal::SecureInput => 3,
        Refusal::Busy => 4,
        Refusal::InjectorFailed => 5,
    }
}

fn projection_refusal_from_pb(reason: u32) -> Result<Refusal, WireError> {
    match reason {
        1 => Ok(Refusal::Permission),
        2 => Ok(Refusal::Locked),
        3 => Ok(Refusal::SecureInput),
        4 => Ok(Refusal::Busy),
        5 => Ok(Refusal::InjectorFailed),
        _ => Err(WireError::BadValue("projection refusal")),
    }
}

fn projection_to_pb(message: &ProjectionMessage) -> Result<pb::Projection, WireError> {
    use pb::projection::Body;

    let body = match message {
        ProjectionMessage::Start {
            projection,
            window,
            size,
        } => {
            check_projection_string(&window.title)?;
            check_projection_string(&window.app_id)?;
            Body::Start(pb::ProjectionStart {
                projection: projection.0,
                title: window.title.clone(),
                app_id: window.app_id.clone(),
                pixel_w: size.width,
                pixel_h: size.height,
            })
        }
        ProjectionMessage::Accepted {
            projection,
            size,
            scale,
        } => {
            check_projection_scale(*scale)?;
            Body::Accepted(pb::ProjectionAccepted {
                projection: projection.0,
                pixel_w: size.width,
                pixel_h: size.height,
                scale: *scale,
            })
        }
        ProjectionMessage::Refused { projection, reason } => Body::Refused(pb::ProjectionRefused {
            projection: projection.0,
            reason: projection_refusal_to_pb(*reason),
        }),
        ProjectionMessage::Resize {
            projection,
            request,
            size,
            scale,
        } => {
            check_projection_scale(*scale)?;
            Body::Resize(pb::ProjectionResize {
                projection: projection.0,
                pixel_w: size.width,
                pixel_h: size.height,
                scale: *scale,
                request: *request,
            })
        }
        ProjectionMessage::Geometry {
            projection,
            size,
            parking,
            answers,
        } => {
            let parking = match parking {
                ParkingKind::Twin => 1,
                ParkingKind::Mirror => 2,
            };
            Body::Geometry(pb::ProjectionGeometry {
                projection: projection.0,
                pixel_w: size.width,
                pixel_h: size.height,
                parking,
                answers: *answers,
            })
        }
        ProjectionMessage::Title { projection, title } => {
            check_projection_string(title)?;
            Body::Title(pb::ProjectionTitle {
                projection: projection.0,
                title: title.clone(),
            })
        }
        ProjectionMessage::Focus {
            projection,
            focused,
        } => Body::Focus(pb::ProjectionFocus {
            projection: projection.0,
            focused: *focused,
        }),
        ProjectionMessage::KeyFrameRequest { projection } => {
            Body::KeyFrameRequest(pb::ProjectionKeyFrameRequest {
                projection: projection.0,
            })
        }
        ProjectionMessage::End { projection, reason } => {
            let reason = match reason {
                ProjectionEndReason::Returned => 1,
                ProjectionEndReason::WindowClosed => 2,
                ProjectionEndReason::Revoked => 3,
                ProjectionEndReason::Locked => 4,
                ProjectionEndReason::LinkLost => 5,
                ProjectionEndReason::Failed => 6,
            };
            Body::End(pb::ProjectionEnd {
                projection: projection.0,
                reason,
            })
        }
        ProjectionMessage::Close { projection, reason } => {
            let reason = match reason {
                ProjectionEndReason::Returned => 1,
                ProjectionEndReason::WindowClosed => 2,
                ProjectionEndReason::Revoked => 3,
                ProjectionEndReason::Locked => 4,
                ProjectionEndReason::LinkLost => 5,
                ProjectionEndReason::Failed => 6,
            };
            Body::Close(pb::ProjectionClose {
                projection: projection.0,
                reason,
            })
        }
        ProjectionMessage::ListWindows { request } => {
            Body::ListWindows(pb::ProjectionListWindows { request: *request })
        }
        ProjectionMessage::WindowList { request, windows } => {
            check_len(windows.len(), MAX_BROWSE_WINDOWS, "browse window count")?;
            let windows = windows
                .iter()
                .map(|window| {
                    check_projection_string(&window.summary.title)?;
                    check_projection_string(&window.summary.app_id)?;
                    Ok(pb::BrowsableWindow {
                        window: window.window.0,
                        title: window.summary.title.clone(),
                        app_id: window.summary.app_id.clone(),
                        width: window.size.width,
                        height: window.size.height,
                    })
                })
                .collect::<Result<_, WireError>>()?;
            Body::WindowList(pb::ProjectionWindowList {
                request: *request,
                windows,
            })
        }
        ProjectionMessage::Pull { request, window } => Body::Pull(pb::ProjectionPull {
            request: *request,
            window: window.0,
        }),
        ProjectionMessage::BrowseRefused { request, reason } => {
            Body::BrowseRefused(pb::ProjectionBrowseRefused {
                request: *request,
                reason: projection_refusal_to_pb(*reason),
            })
        }
    };
    Ok(pb::Projection { body: Some(body) })
}

fn projection_from_pb(projection: pb::Projection) -> Result<ProjectionMessage, WireError> {
    use pb::projection::Body;

    Ok(match projection.body.ok_or(WireError::UnknownControl)? {
        Body::Start(start) => {
            check_projection_string(&start.title)?;
            check_projection_string(&start.app_id)?;
            ProjectionMessage::Start {
                projection: ProjectionId(start.projection),
                window: WindowSummary {
                    title: start.title,
                    app_id: start.app_id,
                },
                size: PixelSize::new(start.pixel_w, start.pixel_h),
            }
        }
        Body::Accepted(accepted) => {
            check_projection_scale(accepted.scale)?;
            ProjectionMessage::Accepted {
                projection: ProjectionId(accepted.projection),
                size: PixelSize::new(accepted.pixel_w, accepted.pixel_h),
                scale: accepted.scale,
            }
        }
        Body::Refused(refused) => ProjectionMessage::Refused {
            projection: ProjectionId(refused.projection),
            reason: projection_refusal_from_pb(refused.reason)?,
        },
        Body::Resize(resize) => {
            check_projection_scale(resize.scale)?;
            ProjectionMessage::Resize {
                projection: ProjectionId(resize.projection),
                request: resize.request,
                size: PixelSize::new(resize.pixel_w, resize.pixel_h),
                scale: resize.scale,
            }
        }
        Body::Geometry(geometry) => {
            let parking = match geometry.parking {
                1 => ParkingKind::Twin,
                2 => ParkingKind::Mirror,
                _ => return Err(WireError::BadValue("projection parking")),
            };
            ProjectionMessage::Geometry {
                projection: ProjectionId(geometry.projection),
                size: PixelSize::new(geometry.pixel_w, geometry.pixel_h),
                parking,
                answers: geometry.answers,
            }
        }
        Body::Title(title) => {
            check_projection_string(&title.title)?;
            ProjectionMessage::Title {
                projection: ProjectionId(title.projection),
                title: title.title,
            }
        }
        Body::Focus(focus) => ProjectionMessage::Focus {
            projection: ProjectionId(focus.projection),
            focused: focus.focused,
        },
        Body::KeyFrameRequest(request) => ProjectionMessage::KeyFrameRequest {
            projection: ProjectionId(request.projection),
        },
        Body::End(end) => {
            let reason = match end.reason {
                1 => ProjectionEndReason::Returned,
                2 => ProjectionEndReason::WindowClosed,
                3 => ProjectionEndReason::Revoked,
                4 => ProjectionEndReason::Locked,
                5 => ProjectionEndReason::LinkLost,
                6 => ProjectionEndReason::Failed,
                _ => return Err(WireError::BadValue("projection end reason")),
            };
            ProjectionMessage::End {
                projection: ProjectionId(end.projection),
                reason,
            }
        }
        Body::Close(close) => {
            let reason = match close.reason {
                1 => ProjectionEndReason::Returned,
                2 => ProjectionEndReason::WindowClosed,
                3 => ProjectionEndReason::Revoked,
                4 => ProjectionEndReason::Locked,
                5 => ProjectionEndReason::LinkLost,
                6 => ProjectionEndReason::Failed,
                _ => return Err(WireError::BadValue("projection end reason")),
            };
            ProjectionMessage::Close {
                projection: ProjectionId(close.projection),
                reason,
            }
        }
        Body::ListWindows(list) => ProjectionMessage::ListWindows {
            request: list.request,
        },
        Body::WindowList(list) => {
            check_len(
                list.windows.len(),
                MAX_BROWSE_WINDOWS,
                "browse window count",
            )?;
            let windows = list
                .windows
                .into_iter()
                .map(|window| {
                    check_projection_string(&window.title)?;
                    check_projection_string(&window.app_id)?;
                    Ok(BrowsableWindow {
                        window: WindowId(window.window),
                        summary: WindowSummary {
                            title: window.title,
                            app_id: window.app_id,
                        },
                        size: PixelSize::new(window.width, window.height),
                    })
                })
                .collect::<Result<_, WireError>>()?;
            ProjectionMessage::WindowList {
                request: list.request,
                windows,
            }
        }
        Body::Pull(pull) => ProjectionMessage::Pull {
            request: pull.request,
            window: WindowId(pull.window),
        },
        Body::BrowseRefused(refused) => ProjectionMessage::BrowseRefused {
            request: refused.request,
            reason: projection_refusal_from_pb(refused.reason)?,
        },
    })
}

// Handwritten prost definitions matching proto/control_v1.proto; no protoc/build script.
mod pb {
    #[derive(Clone, Copy, PartialEq, prost::Message)]
    pub struct AudioOpen {
        #[prost(uint32, tag = "1")]
        pub stream: u32,
        #[prost(uint32, tag = "2")]
        pub kind: u32,
        #[prost(uint32, tag = "3")]
        pub channels: u32,
    }
    #[derive(Clone, Copy, PartialEq, prost::Message)]
    pub struct AudioStream {
        #[prost(uint32, tag = "1")]
        pub stream: u32,
    }
    #[derive(Clone, Copy, PartialEq, prost::Message)]
    pub struct AudioRefused {
        #[prost(uint32, tag = "1")]
        pub stream: u32,
        #[prost(uint32, tag = "2")]
        pub reason: u32,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Projection {
        #[prost(
            oneof = "projection::Body",
            tags = "1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14"
        )]
        pub body: Option<projection::Body>,
    }

    pub mod projection {
        #[derive(Clone, PartialEq, prost::Oneof)]
        pub enum Body {
            #[prost(message, tag = "1")]
            Start(super::ProjectionStart),
            #[prost(message, tag = "2")]
            Accepted(super::ProjectionAccepted),
            #[prost(message, tag = "3")]
            Refused(super::ProjectionRefused),
            #[prost(message, tag = "4")]
            Resize(super::ProjectionResize),
            #[prost(message, tag = "5")]
            Geometry(super::ProjectionGeometry),
            #[prost(message, tag = "6")]
            Title(super::ProjectionTitle),
            #[prost(message, tag = "7")]
            Focus(super::ProjectionFocus),
            #[prost(message, tag = "8")]
            KeyFrameRequest(super::ProjectionKeyFrameRequest),
            #[prost(message, tag = "9")]
            End(super::ProjectionEnd),
            #[prost(message, tag = "10")]
            Close(super::ProjectionClose),
            #[prost(message, tag = "11")]
            ListWindows(super::ProjectionListWindows),
            #[prost(message, tag = "12")]
            WindowList(super::ProjectionWindowList),
            #[prost(message, tag = "13")]
            Pull(super::ProjectionPull),
            #[prost(message, tag = "14")]
            BrowseRefused(super::ProjectionBrowseRefused),
        }
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ProjectionStart {
        #[prost(uint64, tag = "1")]
        pub projection: u64,
        #[prost(string, tag = "2")]
        pub title: String,
        #[prost(string, tag = "3")]
        pub app_id: String,
        #[prost(uint32, tag = "4")]
        pub pixel_w: u32,
        #[prost(uint32, tag = "5")]
        pub pixel_h: u32,
    }

    #[derive(Clone, Copy, PartialEq, prost::Message)]
    pub struct ProjectionAccepted {
        #[prost(uint64, tag = "1")]
        pub projection: u64,
        #[prost(uint32, tag = "2")]
        pub pixel_w: u32,
        #[prost(uint32, tag = "3")]
        pub pixel_h: u32,
        #[prost(double, tag = "4")]
        pub scale: f64,
    }

    #[derive(Clone, Copy, PartialEq, prost::Message)]
    pub struct ProjectionRefused {
        #[prost(uint64, tag = "1")]
        pub projection: u64,
        #[prost(uint32, tag = "2")]
        pub reason: u32,
    }

    #[derive(Clone, Copy, PartialEq, prost::Message)]
    pub struct ProjectionResize {
        #[prost(uint64, tag = "1")]
        pub projection: u64,
        #[prost(uint32, tag = "2")]
        pub pixel_w: u32,
        #[prost(uint32, tag = "3")]
        pub pixel_h: u32,
        #[prost(double, tag = "4")]
        pub scale: f64,
        #[prost(uint32, tag = "5")]
        pub request: u32,
    }

    #[derive(Clone, Copy, PartialEq, prost::Message)]
    pub struct ProjectionGeometry {
        #[prost(uint64, tag = "1")]
        pub projection: u64,
        #[prost(uint32, tag = "2")]
        pub pixel_w: u32,
        #[prost(uint32, tag = "3")]
        pub pixel_h: u32,
        #[prost(uint32, tag = "4")]
        pub parking: u32,
        #[prost(uint32, tag = "5")]
        pub answers: u32,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ProjectionTitle {
        #[prost(uint64, tag = "1")]
        pub projection: u64,
        #[prost(string, tag = "2")]
        pub title: String,
    }

    #[derive(Clone, Copy, PartialEq, prost::Message)]
    pub struct ProjectionFocus {
        #[prost(uint64, tag = "1")]
        pub projection: u64,
        #[prost(bool, tag = "2")]
        pub focused: bool,
    }

    #[derive(Clone, Copy, PartialEq, prost::Message)]
    pub struct ProjectionKeyFrameRequest {
        #[prost(uint64, tag = "1")]
        pub projection: u64,
    }

    #[derive(Clone, Copy, PartialEq, prost::Message)]
    pub struct ProjectionEnd {
        #[prost(uint64, tag = "1")]
        pub projection: u64,
        #[prost(uint32, tag = "2")]
        pub reason: u32,
    }

    #[derive(Clone, Copy, PartialEq, prost::Message)]
    pub struct ProjectionClose {
        #[prost(uint64, tag = "1")]
        pub projection: u64,
        #[prost(uint32, tag = "2")]
        pub reason: u32,
    }

    #[derive(Clone, Copy, PartialEq, prost::Message)]
    pub struct ProjectionListWindows {
        #[prost(uint32, tag = "1")]
        pub request: u32,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ProjectionWindowList {
        #[prost(uint32, tag = "1")]
        pub request: u32,
        #[prost(message, repeated, tag = "2")]
        pub windows: Vec<BrowsableWindow>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct BrowsableWindow {
        #[prost(uint64, tag = "1")]
        pub window: u64,
        #[prost(string, tag = "2")]
        pub title: String,
        #[prost(string, tag = "3")]
        pub app_id: String,
        #[prost(uint32, tag = "4")]
        pub width: u32,
        #[prost(uint32, tag = "5")]
        pub height: u32,
    }

    #[derive(Clone, Copy, PartialEq, prost::Message)]
    pub struct ProjectionPull {
        #[prost(uint32, tag = "1")]
        pub request: u32,
        #[prost(uint64, tag = "2")]
        pub window: u64,
    }

    #[derive(Clone, Copy, PartialEq, prost::Message)]
    pub struct ProjectionBrowseRefused {
        #[prost(uint32, tag = "1")]
        pub request: u32,
        #[prost(uint32, tag = "2")]
        pub reason: u32,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ControlMessage {
        #[prost(
            oneof = "control_message::Body",
            tags = "1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17"
        )]
        pub body: Option<control_message::Body>,
    }

    pub mod control_message {
        #[derive(Clone, PartialEq, prost::Oneof)]
        pub enum Body {
            #[prost(message, tag = "1")]
            Hello(super::Hello),
            #[prost(message, tag = "2")]
            Displays(super::Displays),
            #[prost(message, tag = "3")]
            Layout(super::Layout),
            #[prost(message, tag = "4")]
            StartControl(super::StartControl),
            #[prost(message, tag = "5")]
            ControlStarted(super::ControlStarted),
            #[prost(message, tag = "6")]
            ControlRefused(super::ControlRefused),
            #[prost(message, tag = "7")]
            EndControl(super::EndControl),
            #[prost(message, tag = "8")]
            Grants(super::Grants),
            #[prost(message, tag = "9")]
            Revocation(super::Revocation),
            #[prost(message, tag = "10")]
            Ping(super::Ping),
            #[prost(message, tag = "11")]
            Pong(super::Pong),
            #[prost(message, tag = "12")]
            Goodbye(super::Goodbye),
            #[prost(message, tag = "13")]
            Projection(super::Projection),
            #[prost(message, tag = "14")]
            AudioOpen(super::AudioOpen),
            #[prost(message, tag = "15")]
            AudioOpened(super::AudioStream),
            #[prost(message, tag = "16")]
            AudioRefused(super::AudioRefused),
            #[prost(message, tag = "17")]
            AudioClose(super::AudioStream),
        }
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Hello {
        #[prost(uint32, tag = "1")]
        pub minor: u32,
        #[prost(string, tag = "2")]
        pub name: String,
        #[prost(string, repeated, tag = "3")]
        pub features: Vec<String>,
        #[prost(message, repeated, tag = "4")]
        pub displays: Vec<Display>,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
    #[repr(i32)]
    pub enum ColorSpace {
        Srgb = 0,
        DisplayP3 = 1,
        Bt709 = 2,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Display {
        #[prost(uint32, tag = "1")]
        pub id: u32,
        #[prost(string, tag = "2")]
        pub name: String,
        #[prost(double, tag = "3")]
        pub physical_w_mm: f64,
        #[prost(double, tag = "4")]
        pub physical_h_mm: f64,
        #[prost(uint32, tag = "5")]
        pub pixel_w: u32,
        #[prost(uint32, tag = "6")]
        pub pixel_h: u32,
        #[prost(double, tag = "7")]
        pub scale: f64,
        #[prost(double, tag = "8")]
        pub logical_x: f64,
        #[prost(double, tag = "9")]
        pub logical_y: f64,
        #[prost(uint32, tag = "10")]
        pub refresh_millihz: u32,
        #[prost(enumeration = "ColorSpace", tag = "11")]
        pub color_space: i32,
        #[prost(bool, tag = "12")]
        pub hdr: bool,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Displays {
        #[prost(message, repeated, tag = "1")]
        pub displays: Vec<Display>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Placement {
        #[prost(bytes = "vec", tag = "1")]
        pub node: Vec<u8>,
        #[prost(uint32, tag = "2")]
        pub display: u32,
        #[prost(double, tag = "3")]
        pub origin_x_mm: f64,
        #[prost(double, tag = "4")]
        pub origin_y_mm: f64,
        #[prost(uint64, tag = "5")]
        pub version: u64,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Layout {
        #[prost(message, repeated, tag = "1")]
        pub placements: Vec<Placement>,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
    #[repr(i32)]
    pub enum LockKey {
        Unknown = 0,
        Off = 1,
        On = 2,
    }

    #[derive(Clone, Copy, PartialEq, prost::Message)]
    pub struct LockKeys {
        #[prost(enumeration = "LockKey", tag = "1")]
        pub caps: i32,
        #[prost(enumeration = "LockKey", tag = "2")]
        pub num: i32,
        #[prost(enumeration = "LockKey", tag = "3")]
        pub scroll: i32,
    }

    #[derive(Clone, Copy, PartialEq, prost::Message)]
    pub struct StartControl {
        #[prost(uint64, tag = "1")]
        pub session: u64,
        #[prost(uint32, tag = "2")]
        pub entry_display: u32,
        #[prost(double, tag = "3")]
        pub entry_x: f64,
        #[prost(double, tag = "4")]
        pub entry_y: f64,
        #[prost(message, optional, tag = "5")]
        pub lock_keys: Option<LockKeys>,
    }

    #[derive(Clone, Copy, PartialEq, prost::Message)]
    pub struct ControlStarted {
        #[prost(uint64, tag = "1")]
        pub session: u64,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
    #[repr(i32)]
    pub enum Refusal {
        Unspecified = 0,
        Permission = 1,
        Locked = 2,
        SecureInput = 3,
        Busy = 4,
        InjectorFailed = 5,
    }

    #[derive(Clone, Copy, PartialEq, prost::Message)]
    pub struct ControlRefused {
        #[prost(uint64, tag = "1")]
        pub session: u64,
        #[prost(enumeration = "Refusal", tag = "2")]
        pub reason: i32,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
    #[repr(i32)]
    pub enum EndReason {
        Unspecified = 0,
        Released = 1,
        Panic = 2,
        TargetLocked = 3,
        ControllerLocked = 4,
        LinkLost = 5,
        Revoked = 6,
    }

    #[derive(Clone, Copy, PartialEq, prost::Message)]
    pub struct EndControl {
        #[prost(uint64, tag = "1")]
        pub session: u64,
        #[prost(enumeration = "EndReason", tag = "2")]
        pub reason: i32,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
    #[repr(i32)]
    pub enum Capability {
        Unspecified = 0,
        InputAccept = 1,
        WindowShare = 2,
        WindowBrowse = 3,
        WindowPresent = 4,
        AudioSpeaker = 5,
        AudioMic = 6,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Grants {
        #[prost(enumeration = "Capability", repeated, tag = "1")]
        pub capabilities: Vec<i32>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Revocation {
        #[prost(bytes = "vec", tag = "1")]
        pub revoked: Vec<u8>,
        #[prost(bytes = "vec", tag = "2")]
        pub issuer: Vec<u8>,
        #[prost(uint64, tag = "3")]
        pub issued_at_ms: u64,
        #[prost(bytes = "vec", tag = "4")]
        pub signature: Vec<u8>,
    }

    #[derive(Clone, Copy, PartialEq, prost::Message)]
    pub struct Ping {
        #[prost(uint64, tag = "1")]
        pub t0: u64,
    }

    #[derive(Clone, Copy, PartialEq, prost::Message)]
    pub struct Pong {
        #[prost(uint64, tag = "1")]
        pub t0: u64,
        #[prost(uint64, tag = "2")]
        pub t1: u64,
        #[prost(uint64, tag = "3")]
        pub t2: u64,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Goodbye {
        #[prost(string, tag = "1")]
        pub message: String,
    }
}
