//! The private resume hint: a tiny owner-only record of which setup step was last in flight.
//!
//! It is a hint and nothing else. It is read once at startup to say "an earlier setup stopped
//! here", never deserialized into flow state, never counted as evidence, and never grants
//! ownership of anything: every step is detected again from the real system before it is used.
//! A hint that is missing, oversize, linked, foreign, malformed or from an unknown schema is
//! ignored as if it did not exist.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::super::native_io::{LinuxNativeIo, SupportProof};

pub const HINT_FILE: &str = "resume-hint.json";
pub const MAX_HINT_BYTES: usize = 512;
const SCHEMA: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hint {
    pub schema: u32,
    pub step: String,
    pub phase: String,
}

const STEPS: [&str; 6] = [
    "support", "payload", "service", "restart", "agent", "network",
];
const PHASES: [&str; 3] = ["planned", "applied", "done"];

pub fn path(io: &LinuxNativeIo) -> PathBuf {
    io.target()
        .paths()
        .state_home
        .join("crosspane/installer")
        .join(HINT_FILE)
}

pub fn read(io: &LinuxNativeIo) -> Option<Hint> {
    let bytes = io.read(&path(io), MAX_HINT_BYTES, true).ok()?;
    let hint: Hint = serde_json::from_slice(&bytes).ok()?;
    (hint.schema == SCHEMA
        && STEPS.contains(&hint.step.as_str())
        && PHASES.contains(&hint.phase.as_str()))
    .then_some(hint)
}

/// Best effort: a hint that can't be written is simply not there next time.
pub fn write(io: &LinuxNativeIo, proof: &SupportProof, step: &str, phase: &str) {
    if !STEPS.contains(&step) || !PHASES.contains(&phase) {
        return;
    }
    let hint = Hint {
        schema: SCHEMA,
        step: step.into(),
        phase: phase.into(),
    };
    let Ok(bytes) = serde_json::to_vec(&hint) else {
        return;
    };
    let target = path(io);
    let Some(dir) = target.parent() else {
        return;
    };
    if io.create_private_dir(proof, dir).is_err() {
        return;
    }
    let _ = io.atomic_write(proof, &target, &bytes);
}

pub const LAN_RECEIPT_FILE: &str = "ufw-lan-receipt.json";
pub const MAX_RECEIPT_BYTES: usize = 512;

fn receipt_path(io: &LinuxNativeIo) -> PathBuf {
    io.target()
        .paths()
        .state_home
        .join("crosspane/installer")
        .join(LAN_RECEIPT_FILE)
}

/// The firewall module's own minimal receipt for the LAN rule this installer added, kept so a
/// later removal can name exactly that rule. It is not proof of anything by itself: the
/// firewall journal must admit it again before it can authorize a deletion.
pub fn read_receipt(io: &LinuxNativeIo) -> Option<Vec<u8>> {
    let bytes = io.read(&receipt_path(io), MAX_RECEIPT_BYTES, true).ok()?;
    serde_json::from_slice::<Value>(&bytes)
        .ok()?
        .is_object()
        .then_some(bytes)
}

pub fn write_receipt(io: &LinuxNativeIo, proof: &SupportProof, bytes: &[u8]) {
    if bytes.len() > MAX_RECEIPT_BYTES {
        return;
    }
    let target = receipt_path(io);
    let Some(dir) = target.parent() else {
        return;
    };
    if io.create_private_dir(proof, dir).is_ok() {
        let _ = io.atomic_write(proof, &target, bytes);
    }
}

/// The sentence shown on the welcome screen, or nothing when the earlier setup finished.
pub fn note(hint: &Hint) -> Option<String> {
    if hint.phase == "done" {
        return None;
    }
    let step = match hint.step.as_str() {
        "payload" => "while copying Crosspane's files",
        "service" => "while setting up startup",
        "restart" => "while restarting Crosspane",
        "agent" => "while waiting for Crosspane to start",
        "network" => "while opening network access",
        _ => "partway through",
    };
    Some(format!(
        "An earlier setup stopped {step}. Crosspane checks what is really in place first, then \
         carries on."
    ))
}
