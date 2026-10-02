//! Four tabs driven by asynchronous control replies. Status remains the source of truth.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crosspane_ui_kit::art::Art;
use crosspane_ui_kit::layout::{DisplayRect, LayoutAction, LayoutView, LayoutWidget};
use crosspane_ui_kit::theme;
use eframe::egui::{self, Color32, FontId, RichText, Sense};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::ctl::{Failure, Request, Target, Worker};
use crate::demo::Demo;
use crate::layout::{from_status, place_request};
use crate::model::{Display, Offer, PairStatus, Status, Window, short_id};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Tab {
    Machines,
    Layout,
    Pairing,
    Windows,
}

pub struct Settings {
    worker: Option<Worker>,
    demo: Option<Demo>,
    art: Art,
    screenshot: Option<std::path::PathBuf>,
    screenshot_frames: u32,
    screenshot_started: Option<Instant>,
    screenshot_requested: Option<Instant>,
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
    desk: LayoutWidget,
    /// The layout in the latest status, which the desk follows and Revert restores.
    confirmed: Vec<DisplayRect>,
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
    pub fn new(worker: Option<Worker>, art: Art, screenshot: Option<std::path::PathBuf>) -> Self {
        let review = worker.is_none();
        let tab = if review {
            match std::env::var("CROSSPANE_UI_TAB").as_deref() {
                Ok("layout") => Tab::Layout,
                Ok("pairing") => Tab::Pairing,
                Ok("windows") => Tab::Windows,
                _ => Tab::Machines,
            }
        } else {
            Tab::Machines
        };
        let mut settings = Self {
            worker,
            demo: review.then(Demo::default),
            art,
            screenshot,
            screenshot_frames: 0,
            screenshot_started: None,
            screenshot_requested: None,
            tab,
            reachable: false,
            status: Status::default(),
            pair: PairStatus::default(),
            pending: HashSet::new(),
            status_sent: None,
            pair_sent: None,
            pairing_started: false,
            message: None,
            forget: None,
            desk: LayoutWidget::default(),
            confirmed: Vec::new(),
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
        };
        if review {
            settings.send(Target::Status, Request::Status);
            settings.send(Target::PairStatus, Request::PairStatus);
            settings.refresh_windows();
        }
        settings
    }

    fn send(&mut self, target: Target, request: Request) {
        // Periodic requests never pile up behind a slow peer. Actions retain FIFO ordering.
        if !matches!(target, Target::Action | Target::PairAction) && self.pending.contains(&target)
        {
            return;
        }
        if let Some(demo) = &mut self.demo {
            let value = demo.reply(request);
            self.reachable = true;
            if let Err(error) = self.receive(target.clone(), value) {
                self.show_error(target, error);
            }
            return;
        }
        if self
            .worker
            .as_ref()
            .is_some_and(|worker| worker.send(target.clone(), request))
        {
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
        while let Some(reply) = self
            .worker
            .as_ref()
            .and_then(|worker| worker.replies.try_recv().ok())
        {
            self.pending.remove(&reply.target);
            match reply.result {
                Err(Failure::Unavailable) => {
                    // Retry only status until the socket answers again.
                    self.reachable = false;
                    self.desk.cancel_drag();
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
                self.confirmed = from_status(&self.status);
                if self.place_accepted {
                    self.desk.revert(&self.confirmed);
                    self.place_accepted = false;
                } else {
                    self.desk.follow(&self.confirmed);
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
        machine_card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.heading(&status.name);
                theme::chip(ui, "●  This machine", theme::FROST);
                theme::chip(ui, "Online", theme::GLACIER);
            });
            ui.label(
                RichText::new(short_id(&status.node))
                    .small()
                    .color(theme::QUIET),
            );
            ui.label(format!(
                "Input gate: {}   ·   Session: {}",
                if status.gate_open { "open" } else { "closed" },
                status.session
            ));
            for display in &status.displays {
                display_line(ui, display);
            }
            for permission in &status.permissions {
                if permission.state != "Granted" {
                    ui.colored_label(
                        theme::WARNING,
                        format!(
                            "{}: {} — grant in System Settings › Privacy & Security",
                            permission.permission, permission.state
                        ),
                    );
                }
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if theme::primary(ui, "Take input back", true).clicked() {
                    self.action(Request::Release);
                }
                if theme::destructive(ui, "Panic").clicked() {
                    self.action(Request::Panic);
                }
                if status.armed != Some(true) && ui.button("Rearm").clicked() {
                    self.action(Request::Rearm);
                }
            });
        });
        ui.add_space(10.0);
        theme::section(ui, "PAIRED MACHINES");
        if status.peers.is_empty() {
            ui.label("Pair another computer to move the pointer between your machines.");
        }
        for peer in &status.peers {
            let id = short_id(&peer.node);
            ui.push_id(&id, |ui| {
                machine_card().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal_wrapped(|ui| {
                        ui.heading(&peer.name);
                        theme::chip(
                            ui,
                            if peer.connected {
                                "●  Online"
                            } else {
                                "●  Offline"
                            },
                            if peer.connected {
                                theme::GLACIER
                            } else {
                                theme::QUIET
                            },
                        );
                        if let Some(link) = &peer.link {
                            theme::chip(
                                ui,
                                match link.as_str() {
                                    "DirectUsb4Tb" => "direct cable · USB4 / Thunderbolt",
                                    "DirectEthernet" => "direct cable",
                                    "Lan" => "LAN",
                                    "Wifi" => "Wi-Fi",
                                    _ => "network",
                                },
                                theme::QUIET,
                            );
                        }
                        ui.label(
                            RichText::new(
                                peer.rtt_ms
                                    .map_or_else(|| "RTT: —".into(), |rtt| format!("{rtt:.1} ms")),
                            )
                            .small()
                            .color(theme::QUIET),
                        );
                    });
                    ui.label(RichText::new(&id).small().color(theme::QUIET));
                    for display in &peer.displays {
                        display_line(ui, display);
                    }
                    ui.add_space(6.0);
                    theme::section(ui, "WHAT THIS MACHINE MAY DO HERE");
                    ui.horizontal_wrapped(|ui| {
                        for capability in ["input", "share", "browse", "present"] {
                            // A toggle sends an intent; the next status remains the source of truth.
                            let mut allow = peer.grants.iter().any(|grant| grant == capability);
                            if theme::switch(ui, &mut allow, capability).changed() {
                                self.action(Request::Allow {
                                    peer: id.clone(),
                                    capability: capability.into(),
                                    allow,
                                });
                            }
                        }
                        if theme::destructive(ui, "Forget…").clicked() {
                            self.forget = Some(id.clone());
                        }
                    });
                    if self.forget.as_deref() == Some(&id) {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(format!("Forget {} and end its connection?", peer.name));
                            if theme::destructive(ui, "Forget").clicked() {
                                self.action(Request::Forget { peer: id.clone() });
                                self.forget = None;
                            }
                            if ui.button("Cancel").clicked() {
                                self.forget = None;
                            }
                        });
                    }
                });
            });
        }
        for notice in &status.notices {
            ui.label(notice);
        }
    }

    fn layout(&mut self, ui: &mut egui::Ui) {
        let busy = self.pending.contains(&Target::Place) || self.place_accepted;
        let own = short_id(&self.status.node);
        let peer_order: Vec<String> = self
            .status
            .peers
            .iter()
            .map(|peer| short_id(&peer.node))
            .collect();
        let action = self.desk.show(
            ui,
            LayoutView {
                confirmed: &self.confirmed,
                local_node: &own,
                peer_order: &peer_order,
                busy,
                feedback: self.place_message.as_deref(),
            },
        );
        match action {
            Some(LayoutAction::Apply(intents)) => {
                self.place_message = Some("Applying…".into());
                self.send(Target::Place, place_request(intents));
            }
            Some(LayoutAction::Revert) => self.place_message = None,
            None => {}
        }
    }

    fn pairing(&mut self, ui: &mut egui::Ui) {
        if ui.available_width() >= 660.0 {
            ui.columns(2, |columns| {
                self.pair_here(&mut columns[0]);
                self.pair_join(&mut columns[1]);
            });
        } else {
            self.pair_here(ui);
            self.pair_join(ui);
        }
        match self.pair.phase.as_str() {
            "paired" => {
                ui.label(format!(
                    "Paired with {}.",
                    self.pair.peer.as_deref().unwrap_or("the other machine")
                ));
            }
            "failed" => {
                ui.colored_label(theme::WARNING, "Pairing failed.");
            }
            _ => {}
        }
        if let Some(error) = &self.pair.error {
            ui.colored_label(theme::WARNING, error);
        }
    }

    fn pair_here(&mut self, ui: &mut egui::Ui) {
        machine_card().show(ui, |here| {
            here.set_width(here.available_width());
            theme::section(here, "01  ·  ON THIS COMPUTER");
            here.heading("Show a code");
            here.label("Open a pairing window, then join from your other computer.");
            theme::switch(
                here,
                &mut self.listen_allow_input,
                "allow it to control this machine",
            );
            if theme::primary(here, "Open pairing window", true).clicked() {
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
                        sas_card(here, sas);
                    }
                    here.label("Confirm only if the other screen shows the same code.");
                    here.horizontal(|ui| {
                        if theme::primary(ui, "Confirm", true).clicked() {
                            self.action(Request::PairConfirm { accept: true });
                        }
                        if theme::destructive(ui, "Reject").clicked() {
                            self.action(Request::PairConfirm { accept: false });
                        }
                    });
                }
                _ => {}
            }
        });
    }

    fn pair_join(&mut self, ui: &mut egui::Ui) {
        machine_card().show(ui, |join| {
            join.set_width(join.available_width());
            theme::section(join, "02  ·  FROM ANOTHER COMPUTER");
            join.heading("Join a machine");
            join.label("Find a computer with an open pairing window, or enter its address.");
            theme::switch(
                join,
                &mut self.join_allow_input,
                "allow it to control this machine",
            );
            if theme::primary(join, "Scan", !self.pending.contains(&Target::Scan)).clicked() {
                self.scan_message = Some("Scanning…".into());
                self.send(Target::Scan, Request::PairScan);
            }
            if let Some(message) = &self.scan_message {
                join.label(message);
            }
            for offer in self.offers.clone() {
                join.horizontal_wrapped(|ui| {
                    ui.label(format!("{} ({})", offer.name, offer.addr));
                    if theme::primary(ui, "Join", true).clicked() {
                        self.action(Request::PairJoin {
                            addr: offer.addr,
                            allow_input: self.join_allow_input,
                        });
                    }
                });
            }
            theme::text_edit(join, &mut self.join_addr, "host:port");
            if theme::primary(join, "Join address", !self.join_addr.trim().is_empty()).clicked() {
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
                        sas_card(join, sas);
                    }
                    join.label("Waiting for the other machine to confirm…");
                }
                "pick" => {
                    join.label("Choose the code shown on the other screen:");
                    for (index, candidate) in self.pair.candidates.clone().into_iter().enumerate() {
                        if join
                            .button(RichText::new(candidate).size(30.0).color(theme::GLACIER))
                            .clicked()
                        {
                            self.action(Request::PairPick { index });
                        }
                    }
                }
                _ => {}
            }
        });
    }

    fn windows(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("Refresh").clicked() {
                self.refresh_windows();
            }
            theme::switch(ui, &mut self.show_all, "show all");
        });
        let peers: Vec<_> = self
            .status
            .peers
            .iter()
            .filter(|peer| peer.connected)
            .cloned()
            .collect();
        theme::section(ui, "FROM THIS MACHINE");
        if let Some(error) = &self.local_windows_message {
            ui.colored_label(theme::WARNING, error);
        }
        if self.pending.contains(&Target::LocalWindows) {
            ui.label("Loading…");
        }
        let local_name = self.status.name.clone();
        let mut local_visible = false;
        for window in self.local_windows.clone() {
            if window.title.is_empty() && !self.show_all {
                continue;
            }
            local_visible = true;
            ui.push_id(("local", window.id), |ui| {
                window_card(ui, &window, &local_name, "Choose destination", |ui| {
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
        if !local_visible
            && !self.pending.contains(&Target::LocalWindows)
            && self.local_windows_message.is_none()
        {
            ui.label("No windows to show from this machine.");
        }
        for peer in &peers {
            let id = short_id(&peer.node);
            ui.add_space(8.0);
            theme::section(ui, &format!("FROM {}", peer.name.to_uppercase()));
            if let Some(error) = self.peer_windows_messages.get(&id) {
                ui.colored_label(theme::WARNING, error);
            }
            if self.pending.contains(&Target::PeerWindows(id.clone())) {
                ui.label("Loading…");
            }
            let windows = self.peer_windows.get(&id).cloned().unwrap_or_default();
            if windows.is_empty()
                && !self.pending.contains(&Target::PeerWindows(id.clone()))
                && !self.peer_windows_messages.contains_key(&id)
            {
                ui.label("No windows reported by this machine.");
            }
            for window in windows {
                ui.push_id((&id, window.id), |ui| {
                    window_card(ui, &window, &peer.name, "This machine", |ui| {
                        if theme::primary(ui, "Bring here", true).clicked() {
                            self.action(Request::Pull {
                                peer: id.clone(),
                                window: window.id,
                            });
                        }
                    });
                });
            }
        }
        ui.add_space(8.0);
        theme::section(ui, "ACTIVE PROJECTIONS");
        let own = short_id(&self.status.node);
        for projection in self.status.projections.clone() {
            let local = projection.source == own;
            let source_name = self
                .status
                .peers
                .iter()
                .find(|peer| short_id(&peer.node) == projection.source)
                .map_or_else(|| projection.source.clone(), |peer| peer.name.clone());
            ui.push_id((&projection.source, projection.projection), |ui| {
                machine_card().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal_wrapped(|ui| {
                        theme::chip(
                            ui,
                            if local { "From here" } else { &source_name },
                            theme::QUIET,
                        );
                        theme::chip(
                            ui,
                            if local {
                                "On another machine"
                            } else {
                                "Shown here"
                            },
                            theme::GLACIER,
                        );
                        ui.label(&projection.text);
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
                    ui.small(format!(
                        "Projection {} · {}",
                        projection.projection, projection.source
                    ));
                    if let Some(received) = &projection.received {
                        ui.label(
                            RichText::new(format!(
                                "{} frames, {:.1} MB, last frame {}",
                                received.frames,
                                received.bytes as f64 / 1_000_000.0,
                                received.last_ms_ago.map_or_else(
                                    || "not received".into(),
                                    |ms| format!("{ms} ms ago")
                                )
                            ))
                            .small()
                            .color(theme::QUIET),
                        );
                    }
                });
            });
        }
        if self.status.projections.is_empty() {
            ui.label("No active projections.");
        }
    }

    fn header(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            self.art.emblem(ui, egui::vec2(56.0, 56.0));
            ui.vertical(|ui| {
                let wordmark_size = egui::vec2(260.0, 260.0 * 96.0 / 689.0);
                self.art.wordmark(ui, wordmark_size);
                theme::section(ui, "BY FROSTDEV");
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let peers = self
                    .status
                    .peers
                    .iter()
                    .filter(|peer| peer.connected)
                    .count();
                let text = if self.reachable {
                    let suffix = if peers == 1 { "" } else { "s" };
                    format!("●  Agent reachable  ·  {peers} peer{suffix} online")
                } else {
                    "●  Agent unavailable".into()
                };
                let color = if self.reachable {
                    theme::GLACIER
                } else {
                    theme::QUIET
                };
                theme::chip(ui, &text, color);
            });
        });
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        theme::section(ui, "WORKSPACE");
        let tabs = [
            (Tab::Machines, "Machines"),
            (Tab::Layout, "Layout"),
            (Tab::Pairing, "Pairing"),
            (Tab::Windows, "Windows"),
        ];
        for (index, (tab, name)) in tabs.into_iter().enumerate() {
            if navigation(ui, index, name, self.tab == tab).clicked() && self.tab != tab {
                if self.tab == Tab::Layout {
                    self.desk.cancel_drag();
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
        let footer_height = if self.demo.is_some() { 80.0 } else { 50.0 };
        ui.add_space((ui.available_height() - footer_height).max(20.0));
        ui.label(
            RichText::new("Your computers.\nOne workspace.")
                .size(14.0)
                .color(theme::QUIET),
        );
        if self.demo.is_some() {
            theme::chip(ui, "Demo workspace", theme::QUIET);
        }
    }

    fn content(&mut self, ui: &mut egui::Ui) {
        let (title, subtitle) = match self.tab {
            Tab::Machines => (
                "Your machines",
                "Move the pointer. Take control back. Choose what each machine may do.",
            ),
            Tab::Layout => ("Arrange your desk", "Put every screen in its place."),
            Tab::Pairing => (
                "Connect another computer",
                "Make it part of your workspace, one matching code at a time.",
            ),
            Tab::Windows => (
                "Bring a window over",
                "Keep your work in view, wherever it is running.",
            ),
        };
        ui.heading(title);
        ui.label(RichText::new(subtitle).color(theme::QUIET));
        ui.add_space(12.0);
        if !self.reachable {
            ui.vertical_centered(|ui| {
                ui.add_space(36.0);
                self.art.emblem(ui, egui::vec2(150.0, 150.0));
                ui.label("Start the Crosspane agent to bring your computers into one workspace.");
                if let Some(worker) = &self.worker {
                    ui.label(RichText::new(&worker.path).small().color(theme::QUIET));
                }
            });
            return;
        }
        if let Some(message) = &self.message {
            egui::Frame::new()
                .fill(theme::alpha(theme::NAVY, 95))
                .corner_radius(8)
                .inner_margin(10)
                .show(ui, |ui| {
                    ui.label(message);
                });
        }
        if self.tab == Tab::Layout {
            self.layout(ui);
        } else {
            let scroll = egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    match self.tab {
                        Tab::Machines => self.machines(ui),
                        Tab::Pairing => self.pairing(ui),
                        Tab::Windows => self.windows(ui),
                        Tab::Layout => {}
                    }
                    if self.tab == Tab::Windows {
                        ui.add_space(24.0);
                    }
                });
            if self.tab == Tab::Windows
                && scroll.content_size.y - scroll.state.offset.y > scroll.inner_rect.height() + 1.0
            {
                let fade = egui::Rect::from_min_max(
                    scroll.inner_rect.left_bottom() - egui::vec2(0.0, 38.0),
                    scroll.inner_rect.right_bottom(),
                );
                theme::gradient(
                    &ui.painter().with_clip_rect(scroll.inner_rect),
                    fade,
                    0.0,
                    Color32::TRANSPARENT,
                    theme::alpha(theme::MIDNIGHT, 245),
                );
            }
        }
    }
}

impl eframe::App for Settings {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll();
        self.schedule();
        ctx.request_repaint_after(Duration::from_millis(500));
        if let Some(path) = &self.screenshot {
            let captured = ctx.input(|input| {
                input.events.iter().find_map(|event| {
                    if let egui::Event::Screenshot { image, .. } = event {
                        Some(image.clone())
                    } else {
                        None
                    }
                })
            });
            if let Some(image) = captured {
                if let Err(error) = crate::art::save_screenshot(path, &image) {
                    eprintln!("Crosspane screenshot: {error:#}");
                    std::process::exit(1);
                }
                self.screenshot = None;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else if self
                .screenshot_requested
                .is_some_and(|sent| sent.elapsed() > Duration::from_secs(15))
            {
                eprintln!(
                    "Crosspane screenshot: renderer did not return a capture within 15 seconds"
                );
                std::process::exit(1);
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.art.background(ui.painter(), ui.max_rect());
        egui::Frame::new().inner_margin(24).show(ui, |ui| {
            self.header(ui);
            ui.add_space(18.0);
            let body = ui.available_rect_before_wrap();
            // Fixed sibling rectangles keep panel borders inside the window. Frames inherit
            // layouts, so each child explicitly starts a vertical column instead of inheriting
            // a horizontal body layout and consuming its sibling's width.
            let sidebar = egui::Rect::from_min_size(body.min, egui::vec2(200.0, body.height()));
            let content =
                egui::Rect::from_min_max(egui::pos2(sidebar.right() + 16.0, body.top()), body.max);
            let mut sidebar_ui =
                glass_column(ui, "sidebar", sidebar, theme::glass().inner_margin(14));
            self.sidebar(&mut sidebar_ui);
            let mut content_ui = glass_column(ui, "content", content, theme::glass());
            self.content(&mut content_ui);
            ui.allocate_rect(body, Sense::hover());
        });
        if self.screenshot.is_some() && self.screenshot_requested.is_none() {
            let started = *self.screenshot_started.get_or_insert_with(Instant::now);
            self.screenshot_frames = self.screenshot_frames.saturating_add(1);
            // Allow layout passes, font/texture uploads and the 160 ms selection animation
            // to settle. Only screenshot mode needs this temporary faster repaint cadence.
            if self.screenshot_frames >= 12 && started.elapsed() >= Duration::from_millis(350) {
                self.screenshot_requested = Some(Instant::now());
                ui.ctx()
                    .send_viewport_cmd(
                        egui::ViewportCommand::Screenshot(egui::UserData::default()),
                    );
            } else {
                ui.ctx().request_repaint_after(Duration::from_millis(32));
            }
        }
    }
}

/// Paint a glass surface at its fixed bounds, then create a bounded vertical content UI.
/// Keeping the frame's bounds independent of child measurements preserves the outer margin.
fn glass_column(parent: &mut egui::Ui, id: &str, rect: egui::Rect, frame: egui::Frame) -> egui::Ui {
    let inner = rect - frame.total_margin();
    parent.painter().add(frame.paint(inner));
    let mut child = parent.new_child(
        egui::UiBuilder::new()
            .id_salt(id)
            .max_rect(inner)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    child.set_clip_rect(rect.intersect(parent.clip_rect()));
    child
}

fn navigation(ui: &mut egui::Ui, index: usize, label: &str, selected: bool) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 46.0), Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel,
            ui.is_enabled(),
            selected,
            label,
        )
    });
    let hover =
        ui.ctx()
            .animate_bool_with_time(response.id.with("hover"), response.hovered(), 0.16);
    let glow = ui
        .ctx()
        .animate_bool_with_time(response.id.with("selection"), selected, 0.16);
    if hover > 0.0 || glow > 0.0 {
        ui.painter().rect_filled(
            rect,
            9.0,
            theme::alpha(theme::NAVY, (45.0 * hover + 110.0 * glow) as u8),
        );
    }
    if glow > 0.0 {
        let bar = egui::Rect::from_min_size(
            rect.left_top() + egui::vec2(0.0, 10.0),
            egui::vec2(3.0, 26.0),
        );
        ui.painter().rect_filled(
            bar.expand(3.0),
            5.0,
            theme::alpha(theme::FROST, (18.0 * glow) as u8),
        );
        ui.painter()
            .rect_filled(bar, 2.0, theme::alpha(theme::FROST, (255.0 * glow) as u8));
    }
    theme::icon(
        ui.painter(),
        egui::Rect::from_min_size(rect.min + egui::vec2(12.0, 11.0), egui::vec2(24.0, 24.0)),
        index,
        if selected { theme::FROST } else { theme::QUIET },
    );
    ui.painter().text(
        rect.min + egui::vec2(46.0, 23.0),
        egui::Align2::LEFT_CENTER,
        label,
        FontId::proportional(14.0),
        if selected { theme::ICE } else { theme::QUIET },
    );
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn machine_card() -> egui::Frame {
    theme::glass()
        .fill(theme::alpha(theme::NAVY, 42))
        .inner_margin(16)
        .shadow(egui::epaint::Shadow::NONE)
}

fn sas_card(ui: &mut egui::Ui, sas: &str) {
    ui.add_space(12.0);
    theme::glass()
        .fill(theme::alpha(theme::NAVY, 100))
        .show(ui, |ui| {
            ui.set_width((ui.available_width()).max(1.0));
            theme::section(ui, "MATCH ON BOTH SCREENS");
            ui.label(RichText::new(sas).size(44.0).strong().color(theme::GLACIER));
        });
}

fn window_card(
    ui: &mut egui::Ui,
    window: &Window,
    source: &str,
    destination: &str,
    action: impl FnOnce(&mut egui::Ui),
) {
    machine_card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_wrapped(|ui| {
            ui.strong(&window.app);
            theme::chip(ui, &format!("From {source}"), theme::QUIET);
            theme::chip(ui, destination, theme::GLACIER);
        });
        ui.horizontal_wrapped(|ui| {
            ui.label(window_label(window));
            action(ui);
        });
    });
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
    ui.label(
        RichText::new(format!(
            "Display {}: {} — {} × {} px, scale {}, {:.0} × {:.0} mm",
            display.id,
            display.name,
            display.pixels[0],
            display.pixels[1],
            display.scale,
            display.mm[0],
            display.mm[1]
        ))
        .size(12.0)
        .color(theme::QUIET),
    );
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
        "{title} · {:.0} × {:.0}{display}",
        window.size[0], window.size[1]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::App;
    use eframe::egui::Pos2;

    // Inspect the real egui output without a native window, renderer, compositor or socket.
    // This catches inherited layouts and offscreen content that the model tests cannot detect.
    #[test]
    fn every_tab_is_visible_with_stacked_navigation_and_bounded_panels() {
        fn leaves<'a>(shape: &'a egui::Shape, out: &mut Vec<&'a egui::Shape>) {
            if let egui::Shape::Vec(shapes) = shape {
                for shape in shapes {
                    leaves(shape, out);
                }
            } else {
                out.push(shape);
            }
        }
        for size in [
            egui::vec2(1072.0, 937.0),
            egui::vec2(1100.0, 760.0),
            egui::vec2(800.0, 600.0),
        ] {
            for (tab, title, content) in [
                (Tab::Machines, "Your machines", "desktop"),
                (Tab::Layout, "Arrange your desk", "3440 × 1440"),
                (Tab::Pairing, "Connect another computer", "482 719"),
                (Tab::Windows, "Bring a window over", "Arctic field notes"),
            ] {
                let ctx = egui::Context::default();
                ctx.set_fonts(crate::fonts::load().expect("system font"));
                ctx.set_theme(egui::Theme::Dark);
                ctx.set_style_of(egui::Theme::Dark, theme::style());
                let mut app = Settings::new(None, crate::art::load(&ctx), None);
                app.tab = tab;
                let mut frame = eframe::Frame::_new_kittest();
                let mut input_center = None;
                for number in 0..6 {
                    let mut events = Vec::new();
                    if let Some(pos) =
                        input_center.filter(|_| tab == Tab::Pairing && (number == 1 || number == 2))
                    {
                        events.push(egui::Event::PointerMoved(pos));
                        events.push(egui::Event::PointerButton {
                            pos,
                            button: egui::PointerButton::Primary,
                            pressed: number == 1,
                            modifiers: egui::Modifiers::default(),
                        });
                    }
                    let mut output = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(Pos2::ZERO, size)),
                            time: Some(f64::from(number) * 0.1),
                            events,
                            ..Default::default()
                        },
                        |ui| app.ui(ui, &mut frame),
                    );
                    // A headless test deliberately has no renderer to apply texture deltas.
                    output.textures_delta.clear();
                    let mut texts = Vec::new();
                    let mut panels = Vec::new();
                    let mut input = None;
                    let mut colors = Vec::new();
                    for clipped in &output.shapes {
                        let mut shapes = Vec::new();
                        leaves(&clipped.shape, &mut shapes);
                        for shape in shapes {
                            match shape {
                                egui::Shape::Text(text) => {
                                    let rect = text.galley.rect.translate(text.pos.to_vec2());
                                    texts.push((text.galley.text(), rect, clipped.clip_rect));
                                    colors.push((
                                        text.galley.text(),
                                        text.galley.job.sections[0].format.color,
                                    ));
                                }
                                egui::Shape::Rect(rect) if rect.fill == theme::glass().fill => {
                                    panels.push(rect.rect)
                                }
                                egui::Shape::Rect(rect)
                                    if rect.fill == theme::alpha(theme::NAVY, 45) =>
                                {
                                    input = Some(rect);
                                }
                                _ => {}
                            }
                        }
                    }
                    if tab == Tab::Pairing && size.x >= 1000.0 {
                        let input = input.expect("glass text field");
                        assert!((34.0..=38.0).contains(&input.rect.height()));
                        assert_eq!(input.corner_radius, egui::CornerRadius::same(8));
                        input_center = Some(input.rect.center());
                        if number >= 3 {
                            assert_eq!(input.stroke.color, theme::FROST);
                        }
                        assert!(colors.contains(&("host:port", theme::QUIET)));
                        assert!(colors.contains(&("Join address", theme::QUIET)));
                    } else if tab == Tab::Layout {
                        assert!(colors.contains(&("Apply", theme::QUIET)));
                    }
                    let find = |needle: &str| {
                        *texts
                            .iter()
                            .find(|(text, _, _)| text.contains(needle))
                            .unwrap_or_else(|| {
                                panic!("missing {needle:?} on {tab:?} at {size:?}, frame {number}")
                            })
                    };
                    assert_eq!(panels.len(), 2, "sidebar and content panels");
                    let sidebar = panels[0];
                    let panel = panels[1];
                    assert!((sidebar.left() - 24.0).abs() < 0.1);
                    assert!((sidebar.width() - 200.0).abs() < 0.1);
                    assert!((size.x - panel.right() - 24.0).abs() < 0.1);
                    assert!((size.y - panel.bottom() - 24.0).abs() < 0.1);
                    assert!(sidebar.right() < panel.left());
                    let mut previous_bottom = sidebar.top();
                    for label in ["Machines", "Layout", "Pairing", "Windows"] {
                        let (_, rect, clip) = find(label);
                        assert!(rect.top() > previous_bottom, "nav rows must stack: {label}");
                        assert!(sidebar.contains_rect(rect) && clip.contains_rect(rect));
                        previous_bottom = rect.bottom();
                    }
                    let (_, tagline, tagline_clip) = find("Your computers.");
                    let (_, badge, badge_clip) = find("Demo workspace");
                    assert!(tagline.top() > previous_bottom);
                    assert!(badge.top() > tagline.bottom());
                    assert!(sidebar.contains_rect(tagline) && tagline_clip.contains_rect(tagline));
                    assert!(sidebar.contains_rect(badge) && badge_clip.contains_rect(badge));
                    for needle in [title, content] {
                        let (_, rect, clip) = find(needle);
                        assert!(
                            panel.contains_rect(rect),
                            "{needle:?} outside panel on {tab:?} at {size:?}: {rect:?}"
                        );
                        assert!(
                            clip.contains_rect(rect),
                            "{needle:?} clipped on {tab:?} at {size:?}: {rect:?}, {clip:?}"
                        );
                    }
                }
            }
        }
    }
}
