//! Lenient read models for the control protocol. Unknown fields are intentionally ignored.

use serde::Deserialize;

pub fn short_id(node: &str) -> String {
    node.chars().take(16).collect()
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Status {
    pub node: String,
    pub name: String,
    pub gate_open: bool,
    pub session: String,
    pub permissions: Vec<Permission>,
    pub displays: Vec<Display>,
    pub peers: Vec<Peer>,
    pub layout: Vec<Placement>,
    pub notices: Vec<String>,
    pub projections: Vec<Projection>,
    /// Edge crossing is armed (false after a release or panic). Missing in older agents.
    pub armed: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Permission {
    pub permission: String,
    pub state: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Display {
    pub id: u32,
    pub name: String,
    pub pixels: [u32; 2],
    pub scale: f64,
    pub mm: [f64; 2],
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Peer {
    pub node: String,
    pub name: String,
    pub connected: bool,
    pub rtt_ms: Option<f64>,
    pub displays: Vec<Display>,
    pub grants: Vec<String>,
    /// The link class of the path to it (`Lan`, `Wifi`, `DirectUsb4Tb`, …), if known.
    pub link: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Placement {
    pub node: String,
    pub display: u32,
    pub origin_mm: [f64; 2],
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Projection {
    pub source: String,
    pub projection: u64,
    pub text: String,
    pub received: Option<Received>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Received {
    pub frames: u64,
    pub bytes: u64,
    pub last_ms_ago: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct PairStatus {
    pub phase: String,
    pub sas: Option<String>,
    pub candidates: Vec<String>,
    pub peer: Option<String>,
    pub error: Option<String>,
}

impl PairStatus {
    pub fn in_progress(&self) -> bool {
        matches!(
            self.phase.as_str(),
            "listening" | "connecting" | "waiting" | "pick" | "confirm"
        )
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Offer {
    pub name: String,
    pub addr: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Window {
    pub app: String,
    pub display: Option<u32>,
    pub id: u64,
    pub size: [f64; 2],
    pub title: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_status_sample() {
        let display = json!({"id":0,"name":"HDMI-A-1","pixels":[1080,1920],
            "scale":1.0,"mm":[340.0,600.0],"origin":[0.0,0.0]});
        let sample = json!({"node":"43bd67b4e81f7cbe000000000000000000000000000000000000000000000000",
            "name":"desktop","listening":"[::]:47811","gate_open":true,
            "session":"…","backends":"…",
            "permissions":[{"permission":"ScreenRecording","state":"Denied"}],
            "displays":[display.clone()],
            "peers":[{"node":"ab00000000000000000000000000000000000000000000000000000000000000",
                "name":"macbook","connected":true,"rtt_ms":6.4,"displays":[display],
                "features":["e1","h264","cursor"],"grants":["input","browse"]}],
            "layout":[{"node":"43bd67b4e81f7cbe","display":1,"origin_mm":[0.0,0.0],"version":0}],
            "notices":["…"],"projections":[{"source":"43bd67b4e81f7cbe","projection":1,
                "text":"…","received":{"frames":308,"bytes":11400000,"last_ms_ago":4}},
                {"projection":2,"received":null}],"uptime_s":123});
        let status: Status = serde_json::from_value(sample.clone()).expect("status");
        assert_eq!(status.name, "desktop");
        assert_eq!(status.peers[0].rtt_ms, Some(6.4));
        assert_eq!(status.displays[0].mm, [340.0, 600.0]);
        assert_eq!(status.layout[0].display, 1);
        assert_eq!(
            status.projections[0]
                .received
                .as_ref()
                .expect("stats")
                .frames,
            308
        );
        assert!(status.projections[1].received.is_none());
        let mut nullable = sample;
        nullable["peers"][0]["rtt_ms"] = json!(null);
        let nullable: Status = serde_json::from_value(nullable).expect("null RTT");
        assert_eq!(nullable.peers[0].rtt_ms, None);
    }

    #[test]
    fn parse_pair_status_sample() {
        let pair: PairStatus = serde_json::from_value(json!({"phase":"confirm","sas":"123 456",
            "candidates":["123 456"],"peer":"macbook","error":null}))
        .expect("pair status");
        assert_eq!(pair.sas.as_deref(), Some("123 456"));
        assert!(pair.in_progress());
        let pair: PairStatus = serde_json::from_value(json!({"phase":"failed","sas":null,
            "candidates":[],"peer":null,"error":"rejected"}))
        .expect("null pair fields");
        assert_eq!(pair.error.as_deref(), Some("rejected"));
        assert!(!pair.in_progress());
    }

    #[test]
    fn parse_pair_scan_sample() {
        let offers: Vec<Offer> = serde_json::from_value(json!([
            {"name":"macbook","addr":"192.168.4.20:47811"}
        ]))
        .expect("offers");
        assert_eq!(offers[0].addr, "192.168.4.20:47811");
    }

    #[test]
    fn parse_windows_sample() {
        let windows: Vec<Window> = serde_json::from_value(json!([
            {"app":"firefox","display":2,"id":402662695,"size":[1912.0,1046.0],"title":"…"}
        ]))
        .expect("windows");
        assert_eq!(windows[0].display, Some(2));
        assert_eq!(windows[0].size, [1912.0, 1046.0]);
    }

    #[test]
    fn parse_windows_from_sample() {
        let windows: Vec<Window> = serde_json::from_value(json!([
            {"app":"firefox","id":402662695,"size":[1912.0,1046.0],"title":""}
        ]))
        .expect("peer windows");
        assert_eq!(windows[0].display, None);
        assert!(windows[0].title.is_empty());
    }

    #[test]
    fn missing_optional_fields_default() {
        let status: Status = serde_json::from_value(json!({"peers":[{}],"displays":[{}],
            "layout":[{}],"permissions":[{}],"projections":[{}]}))
        .expect("defaults");
        assert!(status.name.is_empty());
        assert!(status.peers[0].grants.is_empty());
        assert_eq!(status.peers[0].rtt_ms, None);
        let pair: PairStatus = serde_json::from_value(json!({})).expect("pair defaults");
        assert!(!pair.in_progress());
        let offers: Vec<Offer> = serde_json::from_value(json!([{}])).expect("offer defaults");
        assert!(offers[0].name.is_empty());
        let windows: Vec<Window> =
            serde_json::from_value(json!([{"id":1}])).expect("window defaults");
        assert!(windows[0].title.is_empty());
        assert_eq!(windows[0].display, None);
    }
}
