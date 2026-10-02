//! The settings app's side of the desk layout: converting agent status into the kit's display
//! rectangles, and the kit's placement intent into the agent's Place request. The geometry, the
//! editor and the widget live in `crosspane-ui-kit`.

use crosspane_ui_kit::layout::{DisplayRect, PlacementIntent};

use crate::ctl::{PlaceEntry, Request};
use crate::model::{Status, short_id};

/// The confirmed layout, from status. Nodes are the agent's short ids exactly as the agent
/// reports them in `layout`; they travel unchanged into the Place request.
pub fn from_status(status: &Status) -> Vec<DisplayRect> {
    status
        .layout
        .iter()
        .filter_map(|placement| {
            let own = short_id(&status.node);
            let (machine, displays) = if placement.node == own {
                (&status.name, &status.displays)
            } else {
                let peer = status
                    .peers
                    .iter()
                    .find(|peer| short_id(&peer.node) == placement.node)?;
                (&peer.name, &peer.displays)
            };
            let display = displays
                .iter()
                .find(|display| display.id == placement.display);
            if !placement.origin_mm.iter().all(|n| n.is_finite()) {
                return None;
            }
            // Keep placements without known sizes in the group and place request. They are
            // invisible until status supplies their geometry, but move with their machine.
            let size = display.map_or([0.0; 2], |display| display.mm);
            let size = if size.iter().all(|n| n.is_finite() && *n > 0.0) {
                size
            } else {
                [0.0; 2]
            };
            Some(DisplayRect {
                node: placement.node.clone(),
                machine: machine.clone(),
                display: placement.display,
                name: display.map_or_else(String::new, |display| display.name.clone()),
                origin: placement.origin_mm,
                size,
                pixels: display.map_or([0; 2], |display| display.pixels),
            })
        })
        .collect()
}

/// Exactly the Place request the settings app has always sent: one entry per intent, in order.
pub fn place_request(intents: Vec<PlacementIntent>) -> Request {
    Request::Place {
        placements: intents
            .into_iter()
            .map(|intent| PlaceEntry {
                node: intent.node,
                display: intent.display,
                origin_mm: intent.origin_mm,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crosspane_ui_kit::layout::Editor;

    const OWN: &str = "aaaaaaaaaaaaaaaa";
    const PEER: &str = "bbbbbbbbbbbbbbbb";

    fn status(value: serde_json::Value) -> Status {
        serde_json::from_value(value).expect("status")
    }

    #[test]
    fn intents_become_exactly_the_existing_place_request() {
        let status = status(serde_json::json!({
            "node":"aaaaaaaaaaaaaaaa000000000000000000000000000000000000000000000000",
            "name":"desktop",
            "displays":[{"id":0,"name":"left","pixels":[1920,1080],"mm":[100.0,100.0]},
                {"id":1,"name":"right","pixels":[1920,1080],"mm":[100.0,100.0]}],
            "peers":[{"node":"bbbbbbbbbbbbbbbb000000000000000000000000000000000000000000000000",
                "name":"macbook","displays":[{"id":0,"name":"retina","mm":[100.0,100.0]}]}],
            "layout":[{"node":OWN,"display":0,"origin_mm":[0.0,0.0]},
                {"node":OWN,"display":1,"origin_mm":[100.0,30.0]},
                {"node":PEER,"display":0,"origin_mm":[700.0,0.0]}]
        }));
        let mut editor = Editor::default();
        editor.revert(&from_status(&status));
        editor.start_drag(OWN);
        editor.drag_by([20.7, -9.2], 1.0);
        editor.stop_drag();
        let Request::Place { placements } = place_request(editor.place_intent()) else {
            panic!("place")
        };
        assert_eq!(
            placements,
            vec![
                PlaceEntry {
                    node: OWN.into(),
                    display: 0,
                    origin_mm: [21.0, -9.0]
                },
                PlaceEntry {
                    node: OWN.into(),
                    display: 1,
                    origin_mm: [121.0, 21.0]
                }
            ]
        );
        // The request serialises exactly as before, with the short ids untouched.
        assert_eq!(
            serde_json::to_value(place_request(editor.place_intent())).expect("json"),
            serde_json::json!({"cmd":"place","placements":[
                {"node":OWN,"display":0,"origin_mm":[21.0,-9.0]},
                {"node":OWN,"display":1,"origin_mm":[121.0,21.0]}]})
        );
    }

    #[test]
    fn status_conversion_keeps_unsized_placements_and_drops_unknown_or_non_finite_ones() {
        // A placed display whose geometry is absent still travels with its machine.
        let mut status = status(serde_json::json!({
            "node":"aaaaaaaaaaaaaaaa000000000000000000000000000000000000000000000000",
            "name":"desktop","displays":[{"id":0,"name":"main","pixels":[1920,1080],"mm":[100.0,100.0]}],
            "peers":[{"node":"bbbbbbbbbbbbbbbb000000000000000000000000000000000000000000000000",
                "name":"macbook","displays":[{"id":0,"name":"retina","pixels":[3024,1964],"mm":[345.0,224.0]}]}],
            "layout":[{"node":OWN,"display":0,"origin_mm":[0.0,0.0]},
                {"node":OWN,"display":1,"origin_mm":[100.0,30.0]},
                {"node":PEER,"display":0,"origin_mm":[345.0,0.0]},
                {"node":"cccccccccccccccc","display":0,"origin_mm":[9.0,9.0]}]
        }));
        // A placement with a non-finite origin is dropped as well.
        status.layout.push(crate::model::Placement {
            node: OWN.into(),
            display: 0,
            origin_mm: [f64::NAN, 0.0],
        });
        let displays = from_status(&status);
        // A placement for a machine status does not know is dropped.
        assert_eq!(displays.len(), 3);
        assert_eq!(displays[0].machine, "desktop");
        assert_eq!(displays[0].name, "main");
        assert_eq!(displays[0].pixels, [1920, 1080]);
        assert_eq!(displays[0].size, [100.0, 100.0]);
        assert_eq!(displays[1].size, [0.0, 0.0]);
        assert_eq!(displays[1].pixels, [0, 0]);
        assert_eq!(displays[2].machine, "macbook");
        assert_eq!(displays[2].pixels, [3024, 1964]);
        let mut editor = Editor::default();
        editor.revert(&displays);
        editor.start_drag(OWN);
        editor.drag_by([20.7, -9.2], 1.0);
        editor.stop_drag();
        let Request::Place { placements } = place_request(editor.place_intent()) else {
            panic!("place")
        };
        assert_eq!(placements.len(), 2);
        assert_eq!(placements[1].origin_mm, [121.0, 21.0]);
    }
}
