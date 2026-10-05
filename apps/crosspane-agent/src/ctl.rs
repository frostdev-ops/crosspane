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
use crosspane_protocol::msg::Capability;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::Event;

/// The capabilities `crosspanectl allow` can grant, by the name used on the command line, in the
/// settings app and in `status`.
const CAPABILITIES: [(&str, Capability); 8] = [
    ("input", Capability::InputAccept),
    ("share", Capability::WindowShare),
    ("browse", Capability::WindowBrowse),
    ("present", Capability::WindowPresent),
    ("speaker", Capability::AudioSpeaker),
    ("mic", Capability::AudioMic),
    ("clipboard.read", Capability::ClipboardRead),
    ("clipboard.write", Capability::ClipboardWrite),
];

/// The capability a `crosspanectl allow` name stands for.
pub fn capability_named(name: &str) -> Option<Capability> {
    CAPABILITIES
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, capability)| *capability)
}

/// The name `crosspanectl allow` and `status` use for `capability`.
pub fn capability_name(capability: Capability) -> Option<&'static str> {
    CAPABILITIES
        .iter()
        .find(|(_, known)| *known == capability)
        .map(|(name, _)| *name)
}

/// What to tell the user about `capability` being granted or withdrawn, beyond the grant itself.
/// Microphones are stored but never served in this version (AUDIO-v0 §1: speakers only).
pub fn capability_note(capability: Capability) -> Option<&'static str> {
    (capability == Capability::AudioMic).then_some(
        "microphones are not supported yet: the grant is stored, but no microphone is shared",
    )
}

/// The error for a name `crosspanectl allow` doesn't know.
pub fn unknown_capability(name: &str) -> String {
    let names: Vec<&str> = CAPABILITIES.iter().map(|(name, _)| *name).collect();
    format!("unknown capability {name}: use {}", names.join(", "))
}

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
    SettingsUpdate {
        expected_revision: String,
        mac_virtual_display: bool,
    },
    /// Unpair a peer (name or node-id prefix) and end its connection now.
    Forget {
        peer: String,
    },
    /// A lost or stolen device (04 §4): forget it, refuse it until it is paired again, and send
    /// a signed revocation notice to every other paired peer (now, or when it next connects).
    Revoke {
        peer: String,
    },
    /// List the windows `peer` lets this node pull (answered asynchronously).
    WindowsFrom {
        peer: String,
    },
    /// Ask `peer` to project its window `window` here.
    Pull {
        peer: String,
        window: u64,
    },
    /// Grant (`allow`) or withdraw a capability for a peer: `input`, `share`, `browse`,
    /// `present`, `speaker` (the peer may play sound on this machine's speakers) or `mic`
    /// (accepted and stored, but microphones are not supported yet).
    /// `clipboard.read` permits reading my clipboard when pasting there; `clipboard.write`
    /// permits the peer to offer its clipboard here. Both default off.
    Allow {
        peer: String,
        capability: String,
        allow: bool,
    },
    /// Put a peer (by name or node-id prefix) on a side of this node.
    Layout {
        peer: String,
        side: Side,
    },
    /// Place displays on the shared canvas (the settings app's layout editor). Each entry names a
    /// node (this one, or a peer by name or node-id prefix), one of its display ids, and the
    /// display's top-left corner in millimetres. Displays left out keep their place. Refused if
    /// any two displays of the resulting layout overlap.
    Place {
        placements: Vec<PlaceEntry>,
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
    /// Save the picture last shown for projection `projection` of `source` (a diagnostic: the
    /// decoded frame, before presentation) as a PPM file; answers its path.
    Snapshot {
        projection: u64,
        source: String,
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
    /// Machines on the network with a pairing window open.
    PairScan,
    /// Initiator: the codes match (or not).
    PairConfirm {
        accept: bool,
    },
    /// Joiner: the code the other screen shows is candidate `index` (0-based).
    PairPick {
        index: usize,
    },
    /// Ask for the first missing OS permission only (WP-4.33), in the order Accessibility, Input
    /// Monitoring, Screen Recording, Microphone. Answers like `AskPermission`.
    AskPermissions,
    /// Ask for one OS permission (`screen_recording`, `accessibility`, `input_monitoring` or
    /// `microphone`), from the agent's own process so the request names Crosspane. Answers what
    /// was shown: `{"permission", "shown": "nothing" | "prompt" | "pane" | "prompt_then_pane",
    /// "note"?}`. System Settings opens when the one-time prompt was already answered.
    AskPermission {
        permission: String,
    },
    /// Reset Crosspane's own entry for one OS permission (`tccutil reset <service>
    /// io.frostdev.crosspane.agent`), so its prompt can show again. Only ever sent on an explicit
    /// click; never another app's entry.
    ResetPermission {
        permission: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlaceEntry {
    pub node: String,
    pub display: u32,
    pub origin_mm: [f64; 2],
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_requests_use_their_wire_shapes() {
        for (text, cmd) in [
            (
                r#"{"cmd":"ask_permission","permission":"accessibility"}"#,
                "ask_permission",
            ),
            (
                r#"{"cmd":"reset_permission","permission":"screen_recording"}"#,
                "reset_permission",
            ),
            (r#"{"cmd":"ask_permissions"}"#, "ask_permissions"),
        ] {
            let request: Request = serde_json::from_str(text).unwrap_or_else(|e| panic!("{e}"));
            let value = serde_json::to_value(&request).unwrap_or_default();
            assert_eq!(value["cmd"], cmd);
        }
        assert!(serde_json::from_str::<Request>(r#"{"cmd":"ask_permission"}"#).is_err());
    }

    #[test]
    fn settings_update_uses_the_frozen_request_shape() {
        let request = Request::SettingsUpdate {
            expected_revision: "0123456789abcdef".into(),
            mac_virtual_display: true,
        };
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(
            value,
            serde_json::json!({ "cmd": "settings_update", "expected_revision": "0123456789abcdef", "mac_virtual_display": true })
        );
        assert!(matches!(
            serde_json::from_value::<Request>(value).unwrap(),
            Request::SettingsUpdate {
                mac_virtual_display: true,
                ..
            }
        ));
    }

    #[test]
    fn every_capability_has_one_name_that_maps_back() {
        for (name, capability) in CAPABILITIES {
            assert_eq!(capability_named(name), Some(capability));
            assert_eq!(capability_name(capability), Some(name));
        }
        let mut names: Vec<_> = CAPABILITIES.iter().map(|(name, _)| *name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), CAPABILITIES.len());
    }

    #[test]
    fn speaker_and_mic_map_to_the_audio_capabilities() {
        assert_eq!(capability_named("speaker"), Some(Capability::AudioSpeaker));
        assert_eq!(capability_named("mic"), Some(Capability::AudioMic));
        // The names are exact: no plural, no case folding.
        for wrong in ["speakers", "Speaker", "microphone", "audio", ""] {
            assert_eq!(capability_named(wrong), None, "{wrong:?}");
        }
    }

    #[test]
    fn clipboard_names_map_only_to_the_explicit_clipboard_capabilities() {
        for (name, capability) in [
            ("clipboard.read", Capability::ClipboardRead),
            ("clipboard.write", Capability::ClipboardWrite),
        ] {
            assert_eq!(capability_named(name), Some(capability));
            assert_eq!(capability_name(capability), Some(name));
        }
        for wrong in [
            "clipboard",
            "ClipboardRead",
            "clipboard.Read",
            "clipboard.write ",
        ] {
            assert_eq!(capability_named(wrong), None);
        }
    }

    #[test]
    fn only_the_microphone_grant_carries_a_note() {
        assert!(
            capability_note(Capability::AudioMic)
                .is_some_and(|note| note.contains("not supported"))
        );
        for (_, capability) in CAPABILITIES {
            if capability != Capability::AudioMic {
                assert_eq!(capability_note(capability), None);
            }
        }
    }

    #[test]
    fn the_unknown_name_error_lists_the_audio_names() {
        let error = unknown_capability("sound");
        assert!(error.contains("sound"));
        assert!(error.contains("speaker") && error.contains("mic"));
    }

    #[test]
    fn an_allow_request_for_the_speakers_parses_both_ways() {
        let on: Request = serde_json::from_str(
            r#"{"cmd":"allow","peer":"macbook","capability":"speaker","allow":true}"#,
        )
        .unwrap();
        let off: Request = serde_json::from_str(
            r#"{"cmd":"allow","peer":"macbook","capability":"speaker","allow":false}"#,
        )
        .unwrap();
        match (on, off) {
            (
                Request::Allow {
                    peer,
                    capability,
                    allow: true,
                },
                Request::Allow { allow: false, .. },
            ) => {
                assert_eq!(peer, "macbook");
                assert_eq!(
                    capability_named(&capability),
                    Some(Capability::AudioSpeaker)
                );
            }
            other => panic!("{other:?}"),
        }
    }
}
