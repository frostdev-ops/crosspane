//! Read-only recognition of the retired V1 Mac payload member. This never supplies an
//! approved signer or install/delete authority; every remaining signing rule stays current.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub(crate) const TUTORIAL: &str = "Crosspane.app/Contents/MacOS/crosspane-tutorial";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Inventory {
    product_version: String,
    features: Vec<String>,
    files: Vec<File>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    path: String,
    size: u64,
    sha256: [u8; 32],
    mode: u32,
    signing: Option<Rule>,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rule {
    role: String,
    identifier: String,
    designated_requirement: String,
    entitlements: BTreeMap<String, bool>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prior {
    inventory: Inventory,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LegacyReceipt {
    pub product_version: String,
    pub manifest: [u8; 32],
    pub payload: [u8; 32],
}
fn hash(bytes: &[u8]) -> [u8; 32] {
    let mut result = [0; 32];
    result.copy_from_slice(aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes).as_ref());
    result
}
pub(crate) fn classify(prior: &[u8], current: &[u8]) -> Result<Option<LegacyReceipt>, ()> {
    if prior.len() > 256 * 1024 {
        return Err(());
    }
    let Prior { mut inventory } = serde_json::from_slice(prior).map_err(|_| ())?;
    let mut current: Inventory = serde_json::from_slice(current).map_err(|_| ())?;
    let candidates: Vec<_> = inventory
        .files
        .iter()
        .filter(|f| f.path == TUTORIAL || f.signing.as_ref().is_some_and(|r| r.role == "Tutorial"))
        .collect();
    if candidates.is_empty() {
        return Ok(None);
    }
    if candidates.len() != 1
        || candidates[0].path != TUTORIAL
        || candidates[0].mode != 0o755
        || candidates[0].size == 0
        || candidates[0].size > 64 * 1024 * 1024
        || candidates[0]
            .signing
            .as_ref()
            .is_none_or(|r| r.role != "Tutorial")
        || inventory.files.len() != current.files.len() + 1
        || inventory.files.len() > 128
        || inventory.product_version.is_empty()
        || inventory.product_version.len() > 128
        || inventory.product_version.chars().any(char::is_control)
        || inventory.features.len() > 32
    {
        return Err(());
    }
    inventory.features.sort();
    current.features.sort();
    inventory.files.sort_by(|a, b| a.path.cmp(&b.path));
    current.files.sort_by(|a, b| a.path.cmp(&b.path));
    if inventory.features != current.features
        || inventory.features.windows(2).any(|w| w[0] == w[1])
        || inventory
            .files
            .iter()
            .filter(|f| f.path != TUTORIAL)
            .zip(&current.files)
            .any(|(old, new)| {
                old.path != new.path || old.mode != new.mode || old.signing != new.signing
            })
    {
        return Err(());
    }
    // V1 field order and sorted inventory are retained solely to correlate its advisory receipt.
    let manifest = hash(&serde_json::to_vec(&inventory).map_err(|_| ())?);
    let mut bytes = Vec::new();
    for f in &inventory.files {
        bytes.extend_from_slice(&(f.path.len() as u64).to_le_bytes());
        bytes.extend_from_slice(f.path.as_bytes());
        bytes.extend_from_slice(&f.size.to_le_bytes());
        bytes.extend_from_slice(&f.sha256);
    }
    Ok(Some(LegacyReceipt {
        product_version: inventory.product_version,
        manifest,
        payload: hash(&bytes),
    }))
}
