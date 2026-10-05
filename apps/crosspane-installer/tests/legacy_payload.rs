#![allow(clippy::unwrap_used)]
// Portable tests of metadata classification. No signer/native authority is fabricated.
#[path = "../src/legacy_payload.rs"]
mod legacy_payload;
use serde_json::{Value, json};

fn current() -> Value {
    json!({"product_version":"2", "features":["video"], "files":[
        {"path":"Crosspane.app/Contents/MacOS/Crosspane", "mode":493,
         "size":10,"sha256":vec![1;32], "signing":{"role":"Agent","identifier":"fixture",
          "designated_requirement":"deliberately fake fixture rule","entitlements":{}}},
        {"path":"crosspanectl", "mode":493,"size":11,"sha256":vec![2;32],
         "signing":{"role":"Ctl","identifier":"fixture.ctl",
          "designated_requirement":"deliberately fake fixture ctl rule","entitlements":{}}}
    ]})
}
fn prior() -> Value {
    let mut inventory = current();
    inventory["product_version"] = json!("1");
    inventory["files"].as_array_mut().unwrap().push(json!({
        "path":legacy_payload::TUTORIAL,"mode":493,"size":12,"sha256":vec![3;32],
        "signing":{"role":"Tutorial","identifier":"unverifiable",
         "designated_requirement":"never trusted","entitlements":{}}}));
    json!({"inventory":inventory})
}
fn classify(value: &Value) -> Result<Option<legacy_payload::LegacyReceipt>, ()> {
    legacy_payload::classify(
        &serde_json::to_vec(value).unwrap(),
        &serde_json::to_vec(&current()).unwrap(),
    )
}
#[test]
fn legacy_mac_tutorial_metadata_is_classified_without_signer_authority() {
    let receipt = classify(&prior()).unwrap().unwrap();
    assert_eq!(receipt.product_version, "1");
    let mut changed = prior();
    changed["inventory"]["files"][2]["signing"]["designated_requirement"] =
        json!("also never trusted");
    let changed = classify(&changed).unwrap().unwrap();
    assert_ne!(receipt.manifest, changed.manifest);
    assert_eq!(receipt.payload, changed.payload);
}
#[test]
fn legacy_mac_wrong_path_extra_role_and_non_tutorial_rule_are_refused() {
    for change in 0..4 {
        let mut value = prior();
        match change {
            0 => {
                value["inventory"]["files"][2]["path"] = json!("Crosspane.app/Contents/MacOS/other")
            }
            1 => {
                value["inventory"]["files"][0]["signing"]["designated_requirement"] =
                    json!("changed")
            }
            2 => {
                let extra = value["inventory"]["files"][2].clone();
                value["inventory"]["files"]
                    .as_array_mut()
                    .unwrap()
                    .push(extra);
            }
            _ => value["inventory"]["files"][2]["mode"] = json!(420),
        }
        assert!(classify(&value).is_err(), "case {change}");
    }
}
#[test]
fn current_mac_inventory_has_no_legacy_classification() {
    assert_eq!(classify(&json!({"inventory":current()})).unwrap(), None);
}
