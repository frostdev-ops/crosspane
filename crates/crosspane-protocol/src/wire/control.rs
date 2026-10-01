//! Control-stream encoding (protobuf, schema in docs/wp/WP-1.2.md). Implemented in WP-1.2.

use crosspane_types::color::ColorSpace;
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::{
    DisplayGeometry, PixelSize, PointDevice, PointLogical, PointMm, SizeMm,
};
use crosspane_types::id::{DisplayId, NodeId, SessionId};
use crosspane_types::input::LockKeys;
use prost::Message;

use super::{Frame, KIND_CONTROL, MAX_CONTROL_PAYLOAD, WIRE_VERSION, WireError};
use crate::msg::{
    Capability, ControlMessage, EndReason, Hello, Placement, Refusal, RevocationNotice,
};

const MAX_STRING: usize = 256;
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
        // Encoded by WP-2.3; until then a projection message can't be sent.
        ControlMessage::Projection(_) => return Err(WireError::UnknownControl),
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
        Body::Goodbye(goodbye) => {
            check_len(goodbye.message.len(), MAX_STRING, "goodbye message")?;
            ControlMessage::Goodbye {
                message: goodbye.message,
            }
        }
    })
}

// Handwritten prost definitions matching proto/control_v1.proto; no protoc/build script.
mod pb {
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ControlMessage {
        #[prost(
            oneof = "control_message::Body",
            tags = "1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12"
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
