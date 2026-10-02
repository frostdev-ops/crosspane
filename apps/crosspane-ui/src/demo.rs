//! Isolated review model. No worker, socket, DNS or agent calls are created in review mode.
use crate::ctl::Request;
use serde_json::{Value, json};

#[derive(Debug)]
pub struct Demo {
    pub status: Value,
    pub pair: Value,
    pub local_windows: Value,
    pub peer_windows: Value,
}

impl Default for Demo {
    fn default() -> Self {
        Self {
            status: json!({
                "node":"desktop000000000", "name":"desktop", "gate_open":true,
                "armed":true, "session":"active",
                "displays":[
                    {"id":0,"name":"Ultrawide","pixels":[3440,1440],"scale":1.0,"mm":[800.0,335.0]},
                    {"id":1,"name":"Left display","pixels":[1920,1080],"scale":1.0,"mm":[530.0,300.0]},
                    {"id":2,"name":"Right display","pixels":[1920,1080],"scale":1.0,"mm":[530.0,300.0]}
                ],
                "peers":[{"node":"macbook000000000","name":"macbook","connected":true,
                    "rtt_ms":1.8,"link":"DirectEthernet","grants":["input","share","browse"],
                    "displays":[{"id":0,"name":"Built-in Retina","pixels":[3024,1964],"scale":2.0,"mm":[345.0,224.0]}]}],
                "layout":[
                    {"node":"desktop000000000","display":1,"origin_mm":[-530.0,35.0]},
                    {"node":"desktop000000000","display":0,"origin_mm":[0.0,0.0]},
                    {"node":"desktop000000000","display":2,"origin_mm":[800.0,35.0]},
                    {"node":"macbook000000000","display":0,"origin_mm":[1330.0,80.0]}
                ],
                "projections":[{"source":"macbook000000000","projection":7,"text":"Safari — Design references",
                    "received":{"frames":1260,"bytes":21000000,"last_ms_ago":8}}]
            }),
            pair: json!({"phase":"confirm","sas":"482 719","peer":"studio"}),
            local_windows: json!([
                {"id":11,"app":"Terminal","title":"Crosspane workspace","size":[1280.0,800.0],"display":0},
                {"id":12,"app":"Firefox","title":"Arctic field notes","size":[1440.0,900.0],"display":1}
            ]),
            peer_windows: json!([
                {"id":21,"app":"Safari","title":"Design references","size":[1512.0,982.0]},
                {"id":22,"app":"Notes","title":"Ideas for the desk","size":[960.0,700.0]}
            ]),
        }
    }
}

impl Demo {
    pub fn reply(&mut self, request: Request) -> Value {
        match request {
            Request::Status => self.status.clone(),
            Request::PairStatus => self.pair.clone(),
            Request::Windows => self.local_windows.clone(),
            Request::WindowsFrom { .. } => self.peer_windows.clone(),
            Request::PairScan => json!([{"name":"studio","addr":"192.0.2.20:47811"}]),
            Request::Allow {
                peer,
                capability,
                allow,
            } => {
                if let Some(peers) = self.status["peers"].as_array_mut() {
                    for machine in peers.iter_mut().filter(|machine| machine["node"] == peer) {
                        if let Some(grants) = machine["grants"].as_array_mut() {
                            grants.retain(|grant| grant != &capability);
                            if allow {
                                grants.push(json!(capability));
                            }
                        }
                    }
                }
                json!("Permission updated.")
            }
            Request::Forget { peer } => {
                if let Some(peers) = self.status["peers"].as_array_mut() {
                    peers.retain(|machine| machine["node"] != peer);
                }
                if let Some(layout) = self.status["layout"].as_array_mut() {
                    layout.retain(|display| display["node"] != peer);
                }
                json!("Machine forgotten.")
            }
            Request::Place { placements } => {
                if let Some(layout) = self.status["layout"].as_array_mut() {
                    for placement in placements {
                        for display in layout.iter_mut().filter(|display| {
                            display["node"] == placement.node
                                && display["display"] == placement.display
                        }) {
                            display["origin_mm"] = json!(placement.origin_mm);
                        }
                    }
                }
                json!("Desk arrangement applied.")
            }
            Request::PairListen { .. } => {
                self.pair = json!({"phase":"confirm","sas":"482 719","peer":"studio"});
                json!("Pairing window open.")
            }
            Request::PairJoin { .. } => {
                self.pair = json!({"phase":"pick","candidates":["482 719","615 203","928 146"]});
                json!("Choose the matching code.")
            }
            Request::PairConfirm { accept } => {
                self.pair = json!({"phase":if accept {"paired"} else {"failed"},"peer":"studio"});
                json!("Pairing response recorded.")
            }
            Request::PairPick { .. } => {
                self.pair = json!({"phase":"paired","peer":"studio"});
                json!("Paired.")
            }
            Request::Release | Request::Panic => {
                self.status["armed"] = json!(false);
                json!("Input returned to this machine.")
            }
            Request::Rearm => {
                self.status["armed"] = json!(true);
                json!("Edge crossing rearmed.")
            }
            Request::Return { projection, .. } => {
                if let Some(projections) = self.status["projections"].as_array_mut() {
                    projections.retain(|item| item["projection"] != projection);
                }
                json!("Window given back.")
            }
            Request::Project { window, .. } => self.project(window, "desktop000000000"),
            Request::Pull { window, .. } => self.project(window, "macbook000000000"),
        }
    }

    fn project(&mut self, window: u64, source: &str) -> Value {
        if let Some(projections) = self.status["projections"].as_array_mut() {
            projections
                .push(json!({"source": source, "projection": window, "text": "Demo window"}));
        }
        json!("Window projected.")
    }
}
