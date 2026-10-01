//! Four tabs driven by asynchronous control replies. Status remains the source of truth.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, FontId, Pos2, RichText, Sense, Stroke, StrokeKind};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::ctl::{Failure, Request, Target, Worker};
use crate::layout::{Editor, Fit};
use crate::model::{Display, Offer, PairStatus, Status, Window, short_id};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Tab {
    Machines,
    Layout,
    Pairing,
    Windows,
}

#[derive(Clone, Copy, Debug)]
struct CanvasDrag {
    fit: Fit,
    pointer_start: Pos2,
}

#[derive(Debug)]
pub struct Settings {
    worker: Worker,
    tab: Tab,
    reachable: bool,
    status: Status,
    pair: PairStatus,
    pending: HashSet<Target>,
    status_sent: Option<Instant>,
    pair_sent: Option<Instant>,
    pairing_started: bool,
    message: Option<String>,
    forget: Option<String>,
    editor: Editor,
    canvas_drag: Option<CanvasDrag>,
    place_message: Option<String>,
    place_accepted: bool,
    listen_allow_input: bool,
    join_allow_input: bool,
    join_addr: String,
    offers: Vec<Offer>,
    scan_message: Option<String>,
    show_all: bool,
    local_windows: Vec<Window>,
    local_windows_message: Option<String>,
    peer_windows: HashMap<String, Vec<Window>>,
    peer_windows_messages: HashMap<String, String>,
    windows_need_refresh: bool,
}

impl Settings {
    pub fn new(worker: Worker) -> Self {
        Self {
            worker,
            tab: Tab::Machines,
            reachable: false,
            status: Status::default(),
            pair: PairStatus::default(),
            pending: HashSet::new(),
            status_sent: None,
            pair_sent: None,
            pairing_started: false,
            message: None,
            forget: None,
            editor: Editor::default(),
            canvas_drag: None,
            place_message: None,
            place_accepted: false,
            listen_allow_input: false,
            join_allow_input: false,
            join_addr: String::new(),
            offers: Vec::new(),
            scan_message: None,
            show_all: false,
            local_windows: Vec::new(),
            local_windows_message: None,
            peer_windows: HashMap::new(),
            peer_windows_messages: HashMap::new(),
            windows_need_refresh: true,
        }
    }

    fn send(&mut self, target: Target, request: Request) {
        // Periodic requests never pile up behind a slow peer. Actions retain FIFO ordering.
        if !matches!(target, Target::Action | Target::PairAction) && self.pending.contains(&target)
        {
            return;
        }
        if self.worker.send(target.clone(), request) {
            self.pending.insert(target);
        } else {
            self.reachable = false;
        }
    }

    fn action(&mut self, request: Request) {
        let pairing = matches!(
            request,
            Request::PairListen { .. }
                | Request::PairJoin { .. }
                | Request::PairConfirm { .. }
                | Request::PairPick { .. }
        );
        if pairing {
            self.pairing_started = true;
        }
        self.message = Some("Sending…".into());
        self.send(
            if pairing {
                Target::PairAction
            } else {
                Target::Action
            },
            request,
        );
    }

    fn poll(&mut self) {
        while let Ok(reply) = self.worker.replies.try_recv() {
            self.pending.remove(&reply.target);
            match reply.result {
                Err(Failure::Unavailable) => {
                    // Retry only status until the socket answers again.
                    self.reachable = false;
                    self.editor.stop_drag();
                    self.canvas_drag = None;
                    self.status_sent = Some(Instant::now());
                    self.windows_need_refresh = true;
                }
                Err(Failure::Message(error)) => {
                    self.reachable = true;
                    self.show_error(reply.target, error);
                }
                Ok(value) => {
                    self.reachable = true;
                    if let Err(error) = self.receive(reply.target.clone(), value) {
                        self.show_error(reply.target, error);
                    }
                }
            }
        }
    }

    fn show_error(&mut self, target: Target, error: String) {
        match target {
            Target::Place => {
                self.place_message = Some(error);
                self.place_accepted = false;
            }
            Target::Scan => self.scan_message = Some(error),
            Target::LocalWindows => self.local_windows_message = Some(error),
            Target::PeerWindows(peer) => {
                self.peer_windows.remove(&peer);
                self.peer_windows_messages.insert(peer, error);
            }
            Target::PairAction => {
                self.message = Some(error);
                self.pairing_started = false;
                self.pair_sent = None;
            }
            Target::Status | Target::PairStatus | Target::Action => self.message = Some(error),
        }
    }

    fn receive(&mut self, target: Target, value: Value) -> Result<(), String> {
        match target {
            Target::Status => {
                self.status = decode(value)?;
                if self.place_accepted {
                    self.editor.revert(&self.status);
                    self.place_accepted = false;
                } else {
                    self.editor.follow(&self.status);
                }
            }
            Target::PairStatus => {
                self.pair = decode(value)?;
                self.pairing_started = false;
            }
            Target::Scan => {
                self.offers = decode(value)?;
                self.scan_message = Some(if self.offers.is_empty() {
                    "No machine is pairing. Open a pairing window on the other machine.".into()
                } else {
                    format!("Found {} machine(s).", self.offers.len())
                });
            }
            Target::LocalWindows => {
                self.local_windows = decode(value)?;
                self.local_windows_message = None;
            }
            Target::PeerWindows(peer) => {
                self.peer_windows.insert(peer.clone(), decode(value)?);
                self.peer_windows_messages.remove(&peer);
            }
            Target::Place => {
                self.place_message = Some(reply_text(value));
                // Keep edits frozen until a status obtained after the successful place reply.
                self.place_accepted = true;
                self.status_sent = None;
            }
            Target::PairAction => {
                // A queued, older pair_status may have reported idle before the command ran.
                // Ensure a fresh query even if the user has already left the Pairing tab.
                self.pairing_started = true;
                self.message = Some(reply_text(value));
                self.status_sent = None;
                self.pair_sent = None;
            }
            Target::Action => {
                self.message = Some(reply_text(value));
                self.status_sent = None;
            }
        }
        Ok(())
    }

    fn schedule(&mut self) {
        let now = Instant::now();
        let status_interval = Duration::from_secs(if self.reachable { 1 } else { 2 });
        if self
            .status_sent
            .is_none_or(|sent| now.duration_since(sent) >= status_interval)
            && !self.pending.contains(&Target::Status)
        {
            self.send(Target::Status, Request::Status);
            self.status_sent = Some(now);
        }
        if !self.reachable {
            return;
        }
        if (self.tab == Tab::Pairing || self.pair.in_progress() || self.pairing_started)
            && self
                .pair_sent
                .is_none_or(|sent| now.duration_since(sent) >= Duration::from_millis(500))
            && !self.pending.contains(&Target::PairStatus)
        {
            self.send(Target::PairStatus, Request::PairStatus);
            self.pair_sent = Some(now);
        }
        if self.tab == Tab::Windows && self.windows_need_refresh {
            self.refresh_windows();
        }
    }

    fn refresh_windows(&mut self) {
        self.windows_need_refresh = false;
        self.local_windows_message = None;
        self.send(Target::LocalWindows, Request::Windows);
        let peers: Vec<_> = self
            .status
            .peers
            .iter()
            .filter(|peer| peer.connected)
            .map(|peer| short_id(&peer.node))
            .collect();
        for peer in peers {
            self.peer_windows_messages.remove(&peer);
            self.send(
                Target::PeerWindows(peer.clone()),
                Request::WindowsFrom { peer },
            );
        }
    }

    fn machines(&mut self, ui: &mut egui::Ui) {
        let status = self.status.clone();
        ui.heading(format!("{} ({})", status.name, short_id(&status.node)));
        ui.label(format!(
            "Input gate: {}   Session: {}",
            if status.gate_open { "open" } else { "closed" },
            status.session
        ));
        for display in &status.displays {
            display_line(ui, display);
        }
        for permission in &status.permissions {
            if permission.state != "Granted" {
                ui.colored_label(
                    Color32::YELLOW,
                    format!(
                        "{}: {} — grant in System Settings › Privacy & Security",
                        permission.permission, permission.state
                    ),
                );
            }
        }
        ui.horizontal(|ui| {
            if ui.button("Take input back").clicked() {
                self.action(Request::Release);
            }
            if ui.button("Panic").clicked() {
                self.action(Request::Panic);
            }
            // The frozen status has no armed/disarmed field.
            if ui.button("Rearm").clicked() {
                self.action(Request::Rearm);
            }
        });
        ui.separator();
        if status.peers.is_empty() {
            ui.label("No paired machines.");
        }
        for peer in &status.peers {
            let id = short_id(&peer.node);
            ui.push_id(&id, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(
                        if peer.connected {
                            Color32::GREEN
                        } else {
                            Color32::GRAY
                        },
                        "●",
                    );
                    ui.strong(format!("{} ({id})", peer.name));
                    ui.label(if peer.connected {
                        "connected"
                    } else {
                        "offline"
                    });
                    ui.label(
                        peer.rtt_ms
                            .map_or_else(|| "RTT: —".into(), |rtt| format!("RTT: {rtt:.1} ms")),
                    );
                });
                ui.horizontal(|ui| {
                    for capability in ["input", "share", "browse", "present"] {
                        // A toggle sends an intent; the next status is the source of truth.
                        let mut allow = peer.grants.iter().any(|grant| grant == capability);
                        if ui.checkbox(&mut allow, capability).changed() {
                            self.action(Request::Allow {
                                peer: id.clone(),
                                capability: capability.into(),
                                allow,
                            });
                        }
                    }
                    if ui.button("Forget…").clicked() {
                        self.forget = Some(id.clone());
                    }
                });
                if self.forget.as_deref() == Some(&id) {
                    ui.horizontal(|ui| {
                        ui.label(format!("Forget {} and end its connection?", peer.name));
                        if ui.button("Forget").clicked() {
                            self.action(Request::Forget { peer: id.clone() });
                            self.forget = None;
                        }
                        if ui.button("Cancel").clicked() {
                            self.forget = None;
                        }
                    });
                }
                for display in &peer.displays {
                    display_line(ui, display);
                }
                ui.separator();
            });
        }
        for notice in &status.notices {
            ui.label(notice);
        }
    }

    fn layout(&mut self, ui: &mut egui::Ui) {
        let busy = self.pending.contains(&Target::Place) || self.place_accepted;
        let overlaps = self.editor.overlaps();
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    self.editor.edited() && overlaps.is_empty() && !busy && !self.editor.dragging(),
                    egui::Button::new("Apply"),
                )
                .clicked()
            {
                self.place_message = Some("Applying…".into());
                self.send(Target::Place, self.editor.place_request());
            }
            if ui
                .add_enabled(self.editor.edited() && !busy, egui::Button::new("Revert"))
                .clicked()
            {
                self.editor.revert(&self.status);
                self.canvas_drag = None;
                self.place_message = None;
            }
            if !overlaps.is_empty() {
                ui.colored_label(Color32::RED, "Displays overlap.");
            }
        });
        if let Some(message) = &self.place_message {
            ui.label(message);
        }
        ui.label("The pointer crosses where displays of different machines touch.");
        let (canvas, _) = ui.allocate_exact_size(
            ui.available_size().max(egui::vec2(1.0, 1.0)),
            Sense::hover(),
        );
        let painter = ui.painter_at(canvas);
        painter.rect_filled(canvas, 4.0, ui.visuals().extreme_bg_color);
        if !self.editor.displays.iter().any(|display| display.visible()) {
            painter.text(
                canvas.center(),
                egui::Align2::CENTER_CENTER,
                "No placed displays with a known size.",
                FontId::proportional(16.0),
                ui.visuals().text_color(),
            );
            return;
        }
        let fit = self
            .canvas_drag
            .map_or_else(|| Fit::new(&self.editor.displays, canvas), |drag| drag.fit);
        let mut start = None;
        for display in self
            .editor
            .displays
            .iter()
            .filter(|display| display.visible())
        {
            let response = ui.interact(
                fit.rect(display),
                ui.id().with((&display.node, display.display)),
                if busy { Sense::hover() } else { Sense::drag() },
            );
            if response.drag_started() {
                start = ui
                    .input(|input| input.pointer.press_origin())
                    .map(|pointer_start| (display.node.clone(), CanvasDrag { fit, pointer_start }));
            }
            response.on_hover_text(format!(
                "{} / {}\n{:.0} × {:.0} mm at ({:.0}, {:.0}) mm",
                display.machine,
                display.name,
                display.size[0],
                display.size[1],
                display.origin[0],
                display.origin[1]
            ));
        }
        if let Some((node, drag)) = start {
            self.editor.start_drag(&node);
            self.canvas_drag = Some(drag);
        }
        if let Some(drag) = self.canvas_drag {
            if let Some(pointer) = ui.input(|input| input.pointer.interact_pos()) {
                let now = drag.fit.to_mm(pointer);
                let initial = drag.fit.to_mm(drag.pointer_start);
                // Snap threshold is eight physical screen pixels, accounting for HiDPI.
                self.editor.drag_by(
                    [now[0] - initial[0], now[1] - initial[1]],
                    drag.fit.scale * f64::from(ui.ctx().pixels_per_point()),
                );
            }
            if !ui.input(|input| input.pointer.primary_down()) {
                self.editor.stop_drag();
                self.canvas_drag = None;
            }
        }
        let overlaps = self.editor.overlaps();
        for (index, display) in self.editor.displays.iter().enumerate() {
            if !display.visible() {
                continue;
            }
            let rect = fit.rect(display);
            painter.rect_filled(rect, 3.0, machine_colour(&display.node));
            painter.rect_stroke(
                rect,
                3.0,
                Stroke::new(
                    if overlaps.contains(&index) { 3.0 } else { 1.0 },
                    if overlaps.contains(&index) {
                        Color32::RED
                    } else {
                        Color32::WHITE
                    },
                ),
                StrokeKind::Inside,
            );
            painter.with_clip_rect(rect.intersect(canvas)).text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                format!("{}\n{}", display.machine, display.name),
                FontId::proportional(14.0),
                Color32::WHITE,
            );
        }
    }

    fn pairing(&mut self, ui: &mut egui::Ui) {
        ui.heading("Pair a new machine");
        ui.columns(2, |columns| {
            let here = &mut columns[0];
            here.heading("Here (show a code)");
            here.checkbox(
                &mut self.listen_allow_input,
                "allow it to control this machine",
            );
            if here.button("Open pairing window").clicked() {
                self.action(Request::PairListen {
                    allow_input: self.listen_allow_input,
                });
            }
            match self.pair.phase.as_str() {
                "listening" => {
                    here.label("Waiting for the other machine…");
                }
                "confirm" => {
                    if let Some(sas) = &self.pair.sas {
                        here.label(RichText::new(sas).size(36.0));
                    }
                    here.label("Confirm only if the other screen shows the same code.");
                    here.horizontal(|ui| {
                        if ui.button("Confirm").clicked() {
                            self.action(Request::PairConfirm { accept: true });
                        }
                        if ui.button("Reject").clicked() {
                            self.action(Request::PairConfirm { accept: false });
                        }
                    });
                }
                _ => {}
            }
            let join = &mut columns[1];
            join.heading("Join another machine");
            join.checkbox(
                &mut self.join_allow_input,
                "allow it to control this machine",
            );
            if join
                .add_enabled(
                    !self.pending.contains(&Target::Scan),
                    egui::Button::new("Scan"),
                )
                .clicked()
            {
                self.scan_message = Some("Scanning…".into());
                self.send(Target::Scan, Request::PairScan);
            }
            if let Some(message) = &self.scan_message {
                join.label(message);
            }
            for offer in self.offers.clone() {
                join.horizontal(|ui| {
                    ui.label(format!("{} ({})", offer.name, offer.addr));
                    if ui.button("Join").clicked() {
                        self.action(Request::PairJoin {
                            addr: offer.addr,
                            allow_input: self.join_allow_input,
                        });
                    }
                });
            }
            join.add(egui::TextEdit::singleline(&mut self.join_addr).hint_text("host:port"));
            if join
                .add_enabled(
                    !self.join_addr.trim().is_empty(),
                    egui::Button::new("Join address"),
                )
                .clicked()
            {
                self.action(Request::PairJoin {
                    addr: self.join_addr.trim().into(),
                    allow_input: self.join_allow_input,
                });
            }
            match self.pair.phase.as_str() {
                "connecting" => {
                    join.label("Connecting…");
                }
                "waiting" => {
                    if let Some(sas) = &self.pair.sas {
                        join.label(RichText::new(sas).size(36.0));
                    }
                    join.label("Waiting for the other machine to confirm…");
                }
                "pick" => {
                    join.label("Choose the code shown on the other screen:");
                    for (index, candidate) in self.pair.candidates.clone().into_iter().enumerate() {
                        if join.button(RichText::new(candidate).size(30.0)).clicked() {
                            self.action(Request::PairPick { index });
                        }
                    }
                }
                _ => {}
            }
        });
        match self.pair.phase.as_str() {
            "paired" => {
                ui.label(format!(
                    "Paired with {}.",
                    self.pair.peer.as_deref().unwrap_or("the other machine")
                ));
            }
            "failed" => {
                ui.colored_label(Color32::RED, "Pairing failed.");
            }
            _ => {}
        }
        if let Some(error) = &self.pair.error {
            ui.colored_label(Color32::RED, error);
        }
    }

    fn windows(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Windows");
            if ui.button("Refresh").clicked() {
                self.refresh_windows();
            }
            ui.checkbox(&mut self.show_all, "show all");
        });
        let peers: Vec<_> = self
            .status
            .peers
            .iter()
            .filter(|peer| peer.connected)
            .cloned()
            .collect();
        ui.heading("This machine's windows");
        if let Some(error) = &self.local_windows_message {
            ui.colored_label(Color32::RED, error);
        }
        if self.pending.contains(&Target::LocalWindows) {
            ui.label("Loading…");
        }
        for window in self.local_windows.clone() {
            if window.title.is_empty() && !self.show_all {
                continue;
            }
            ui.push_id(("local", window.id), |ui| {
                ui.horizontal(|ui| {
                    ui.label(window_label(&window));
                    ui.menu_button("Show on…", |ui| {
                        if peers.is_empty() {
                            ui.label("No connected peers.");
                        }
                        for peer in &peers {
                            if ui.button(&peer.name).clicked() {
                                self.action(Request::Project {
                                    window: window.id,
                                    peer: short_id(&peer.node),
                                });
                                ui.close();
                            }
                        }
                    });
                });
            });
        }
        for peer in &peers {
            let id = short_id(&peer.node);
            ui.separator();
            ui.heading(format!("{}'s windows", peer.name));
            if let Some(error) = self.peer_windows_messages.get(&id) {
                ui.colored_label(Color32::RED, error);
            }
            if self.pending.contains(&Target::PeerWindows(id.clone())) {
                ui.label("Loading…");
            }
            for window in self.peer_windows.get(&id).cloned().unwrap_or_default() {
                ui.push_id((&id, window.id), |ui| {
                    ui.horizontal(|ui| {
                        ui.label(window_label(&window));
                        if ui.button("Show here").clicked() {
                            self.action(Request::Pull {
                                peer: id.clone(),
                                window: window.id,
                            });
                        }
                    });
                });
            }
        }
        ui.separator();
        ui.heading("Active projections");
        let own = short_id(&self.status.node);
        for projection in self.status.projections.clone() {
            let local = projection.source == own;
            ui.push_id((&projection.source, projection.projection), |ui| {
                ui.horizontal(|ui| {
                    ui.label(format!(
                        "{}:{} — {} — shown {}",
                        projection.source,
                        projection.projection,
                        projection.text,
                        if local { "from here" } else { "here" }
                    ));
                    if ui.button("Give back").clicked() {
                        self.action(Request::Return {
                            projection: projection.projection,
                            source: if local {
                                None
                            } else {
                                Some(projection.source.clone())
                            },
                        });
                    }
                });
                if let Some(received) = &projection.received {
                    ui.small(format!(
                        "{} frames, {:.1} MB, last frame {}",
                        received.frames,
                        received.bytes as f64 / 1_000_000.0,
                        received
                            .last_ms_ago
                            .map_or_else(|| "not received".into(), |ms| format!("{ms} ms ago"))
                    ));
                }
            });
        }
        if self.status.projections.is_empty() {
            ui.label("No active projections.");
        }
    }
}

impl eframe::App for Settings {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll();
        self.schedule();
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Frame::central_panel(ui.style()).show(ui, |ui| {
            ui.horizontal(|ui| {
                for (tab, name) in [
                    (Tab::Machines, "Machines"),
                    (Tab::Layout, "Layout"),
                    (Tab::Pairing, "Pairing"),
                    (Tab::Windows, "Windows"),
                ] {
                    if ui.selectable_label(self.tab == tab, name).clicked() && self.tab != tab {
                        if self.tab == Tab::Layout {
                            self.editor.stop_drag();
                            self.canvas_drag = None;
                        }
                        self.tab = tab;
                        if tab == Tab::Windows {
                            self.windows_need_refresh = true;
                        }
                        if tab == Tab::Pairing {
                            self.pair_sent = None;
                        }
                    }
                }
            });
            ui.separator();
            if !self.reachable {
                ui.label(format!(
                    "crosspane-agent isn't running ({})",
                    self.worker.path
                ));
                return;
            }
            if let Some(message) = &self.message {
                ui.label(message);
            }
            if self.tab == Tab::Layout {
                self.layout(ui);
            } else {
                egui::ScrollArea::vertical().show(ui, |ui| match self.tab {
                    Tab::Machines => self.machines(ui),
                    Tab::Pairing => self.pairing(ui),
                    Tab::Windows => self.windows(ui),
                    Tab::Layout => {}
                });
            }
        });
    }
}

fn decode<T: DeserializeOwned>(value: Value) -> Result<T, String> {
    serde_json::from_value(value).map_err(|error| format!("bad response from agent: {error}"))
}

fn reply_text(value: Value) -> String {
    match value {
        Value::String(text) => text,
        Value::Null => "Done.".into(),
        other => other.to_string(),
    }
}

fn display_line(ui: &mut egui::Ui, display: &Display) {
    ui.label(format!(
        "Display {}: {} — {} × {} px, scale {}, {:.0} × {:.0} mm",
        display.id,
        display.name,
        display.pixels[0],
        display.pixels[1],
        display.scale,
        display.mm[0],
        display.mm[1]
    ));
}

fn window_label(window: &Window) -> String {
    let title = if window.title.is_empty() {
        "(untitled)"
    } else {
        &window.title
    };
    let display = window
        .display
        .map_or_else(String::new, |id| format!(" · display {id}"));
    format!(
        "{} — {title} · {:.0} × {:.0}{display}",
        window.app, window.size[0], window.size[1]
    )
}

fn machine_colour(node: &str) -> Color32 {
    let hash = node.bytes().fold(2_166_136_261_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(16_777_619)
    });
    Color32::from_rgb(
        45 + (hash & 63) as u8,
        65 + ((hash >> 8) & 63) as u8,
        85 + ((hash >> 16) & 63) as u8,
    )
}
