//! Portable practice UI and inherited-pipe child pump. Native facts remain adapter-owned.
use crate::fixture::*;
use clap::Parser;
use crosspane_installer_core::AttemptId;
use crosspane_types::id::WindowId;
use crosspane_ui_kit::theme;
use eframe::egui;
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Read, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
type Result<T> = std::result::Result<T, FixtureError>;

#[derive(Parser, Debug)]
#[command(
    name = "crosspane-tutorial",
    about = "Internal Crosspane practice child"
)]
pub struct TutorialOptions {
    #[arg(long)]
    pub controlled: bool,
    #[arg(long)]
    pub font: Option<PathBuf>,
}
impl TutorialOptions {
    pub fn validate(&self) -> Result<&std::path::Path> {
        self.font
            .as_deref()
            .filter(|p| self.controlled && p.is_absolute())
            .ok_or(FixtureError::BadCall)
    }
}
/// Data supplied only by an independently admitted own-window observer; never a winit id.
pub type WindowObservation = Result<(WindowId, OwnWindowFacts)>;
pub fn unavailable_window() -> WindowObservation {
    Err(FixtureError::Unavailable)
}

/// OS-owned operations, polled without waiting on a GUI frame. None means pending; repeated
/// polls of the same call/tone never restart work. Observations must be fresh for each call and
/// identify only this child's primary surface with the exact controlled title. Successful tone
/// replies confirm only the fixture stream, never remote audio activity or human hearing.
/// Stop disables output immediately, releases it within 50 ms, and Drop must also stop output.
pub trait TutorialNative {
    fn observe_window(&mut self, call_id: u64, title: &str) -> Option<WindowObservation>;
    fn play_tone(&mut self, tone: ToneId, output: &SpeakersSelection) -> Option<Result<()>>;
    fn stop_tone(&mut self, tone: ToneId) -> Option<Result<()>>;
    fn tone_state(&self) -> OwnToneState;
}
#[derive(Debug, Default)]
pub struct UnavailableTutorial;
impl TutorialNative for UnavailableTutorial {
    fn observe_window(&mut self, _: u64, _: &str) -> Option<WindowObservation> {
        Some(unavailable_window())
    }
    fn play_tone(&mut self, _: ToneId, _: &SpeakersSelection) -> Option<Result<()>> {
        Some(Err(FixtureError::Unavailable))
    }
    fn stop_tone(&mut self, _: ToneId) -> Option<Result<()>> {
        Some(Err(FixtureError::Unavailable))
    }
    fn tone_state(&self) -> OwnToneState {
        OwnToneState::Stopped
    }
}
struct Native(Box<dyn TutorialNative>);
impl Default for Native {
    fn default() -> Self {
        Self(Box::new(UnavailableTutorial))
    }
}

#[derive(Default)]
pub struct Practice {
    attempt: Option<AttemptId>,
    fixture: Option<FixtureId>,
    window: Option<WindowId>,
    title: String,
    phase: Option<PhaseId>,
    press_phase: Option<PhaseId>,
    click_frame: Option<u64>,
    pattern_ticks: u64,
    target_clicks: u64,
    next_pattern: u64,
    sequence: u64,
    last_call: u64,
    text: String,
    reduced_motion: bool,
    stopped: bool,
    close_reply: Option<u64>,
    editor_context: Option<egui::Context>,
    native: Native,
    pending: Option<ReceivedCall>,
    opening: Option<u64>,
    running_tone: Option<ToneId>,
    requested_tone: Option<ToneId>,
    tone_cancelled: bool,
    pending_event: Option<(FixtureMessage, u64)>,
    close_at: Option<u64>,
}
impl std::fmt::Debug for Practice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Practice(private volatile text)")
    }
}
impl Practice {
    pub fn with_native(native: Box<dyn TutorialNative>) -> Self {
        let mut practice = Self::default();
        practice.native = Native(native);
        practice
    }
    pub fn text_is_empty(&self) -> bool {
        self.text.is_empty()
    }
    fn clear_text(&mut self) {
        self.text.clear();
        if let Some(ctx) = &self.editor_context {
            ctx.data_mut(|d| {
                d.remove::<egui::text_edit::TextEditState>(egui::Id::new("practice-editor"))
            });
        }
    }
    fn message(&mut self, id: Option<u64>, result: Result<FixtureEvent>) -> Result<FixtureMessage> {
        self.sequence = self
            .sequence
            .checked_add(1)
            .filter(|n| *n < u64::MAX)
            .ok_or(FixtureError::CounterExhausted)?;
        Ok(FixtureMessage {
            call_id: id,
            attempt: self.attempt.ok_or(FixtureError::NotOwned)?,
            sequence: self.sequence,
            result,
        })
    }
    fn validate_call(&self, call: &FixtureCall) -> Result<()> {
        encode_control(&FixtureControlPacket {
            schema_version: 1,
            call: call.clone(),
        })?;
        if self.last_call == u64::MAX {
            return Err(FixtureError::CounterExhausted);
        }
        if self.stopped
            || call.id <= self.last_call
            || self.attempt.is_some_and(|a| a != call.attempt)
        {
            return Err(FixtureError::InvalidMessage);
        }
        let owned = match &call.command {
            FixtureCommand::Open { .. } => self.attempt.is_none(),
            FixtureCommand::ArmTarget { fixture, .. }
            | FixtureCommand::ObserveWindow { fixture }
            | FixtureCommand::PlayTone { fixture, .. }
            | FixtureCommand::StopTone { fixture, .. }
            | FixtureCommand::Close { fixture } => self.fixture == Some(*fixture),
        };
        if !owned {
            return Err(FixtureError::NotOwned);
        }
        Ok(())
    }
    pub fn handle(
        &mut self,
        call: FixtureCall,
        observation: WindowObservation,
    ) -> Result<FixtureMessage> {
        self.validate_call(&call)?;
        let fixture = self.fixture.unwrap_or(FixtureId(call.id));
        let result = match call.command {
            FixtureCommand::Open { machine_label } => {
                self.title = practice_title(&machine_label, call.attempt, fixture)?;
                self.attempt = Some(call.attempt);
                self.fixture = Some(fixture);
                observation.and_then(|(window, _)| {
                    if window.0 == 0 {
                        return Err(FixtureError::UnknownWindow);
                    }
                    self.window = Some(window);
                    Ok(FixtureEvent::Opened {
                        fixture,
                        pid: std::process::id(),
                        window,
                        label: machine_label,
                    })
                })
            }
            FixtureCommand::ArmTarget { phase, .. }
                if self.window.is_some() && self.phase.is_none_or(|p| phase > p) =>
            {
                self.phase = Some(phase);
                self.press_phase = None;
                Ok(FixtureEvent::TargetArmed { fixture, phase })
            }
            FixtureCommand::ObserveWindow { .. } => observation.and_then(|(window, facts)| {
                if self.window != Some(window) {
                    return Err(FixtureError::UnknownWindow);
                }
                Ok(FixtureEvent::Snapshot {
                    snapshot: FixtureSnapshot {
                        fixture,
                        window,
                        phase: self.phase,
                        pattern_ticks: self.pattern_ticks,
                        target_clicks: self.target_clicks,
                        window_facts: facts,
                        tone: self.native.0.tone_state(),
                    },
                })
            }),
            FixtureCommand::PlayTone { .. } if self.tone_cancelled => {
                Err(FixtureError::Unavailable)
            }
            FixtureCommand::PlayTone { output, .. } => {
                self.requested_tone = Some(ToneId(call.id));
                self.native
                    .0
                    .play_tone(ToneId(call.id), &output)
                    .ok_or(FixtureError::Busy)?
                    .map(|()| {
                        self.running_tone = Some(ToneId(call.id));
                        FixtureEvent::ToneStarted {
                            fixture,
                            tone: ToneId(call.id),
                        }
                    })
            }
            FixtureCommand::StopTone { tone, .. } => self
                .native
                .0
                .stop_tone(tone)
                .ok_or(FixtureError::Busy)?
                .map(|()| {
                    if self.running_tone == Some(tone) {
                        self.running_tone = None;
                    }
                    FixtureEvent::ToneStopped { fixture, tone }
                }),
            FixtureCommand::Close { .. } => {
                self.stop();
                Ok(FixtureEvent::CloseRequested { fixture })
            }
            _ => return Err(FixtureError::NotOwned),
        };
        self.last_call = call.id;
        self.message(Some(call.id), result)
    }
    pub fn close_requested(&mut self) -> Result<Option<FixtureMessage>> {
        self.tone_cancelled = true;
        self.clear_text();
        self.press_phase = None;
        self.stop_output();
        self.fixture
            .map(|fixture| self.message(None, Ok(FixtureEvent::CloseRequested { fixture })))
            .transpose()
    }
    pub fn stop(&mut self) {
        self.tone_cancelled = true;
        self.clear_text();
        self.press_phase = None;
        self.stop_output();
        self.stopped = true;
    }
    fn stop_output(&mut self) {
        let tone = match self.native.0.tone_state() {
            OwnToneState::Running { tone } | OwnToneState::StopUnconfirmed { tone } => Some(tone),
            OwnToneState::Stopped => self.requested_tone,
        };
        if let Some(tone) = tone {
            let _ = self.native.0.stop_tone(tone);
        }
    }
    fn send_lifecycle(
        &mut self,
        channel: &ChildChannel,
        message: FixtureMessage,
        at: u64,
    ) -> Result<bool> {
        match channel.send(message.clone(), at) {
            Ok(()) => Ok(true),
            Err(FixtureError::Busy) => {
                self.pending_event = Some((message, at));
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }
    /// Shared dispatch: `close` is a native close request; true permits viewport shutdown.
    pub fn process(&mut self, channel: &ChildChannel, now_ms: u64, close: bool) -> Result<bool> {
        let result = (|| {
            if let Some(error) = channel.failure() {
                return Err(error);
            }
            if close {
                self.clear_text();
                self.press_phase = None;
                self.tone_cancelled = true;
                self.close_at.get_or_insert(now_ms);
                self.stop_output();
            }
            if let Some((message, at)) = self.pending_event.take() {
                if now_ms >= at.saturating_add(RESPONSE_MS) {
                    return Err(FixtureError::TimedOut);
                }
                if !self.send_lifecycle(channel, message, at)? {
                    return Ok(false);
                }
            }
            for _ in 0..MAX_QUEUE {
                if close
                    && self
                        .pending
                        .as_ref()
                        .is_some_and(|p| matches!(p.call.command, FixtureCommand::PlayTone { .. }))
                {
                    break; // Deliver native CloseRequested before the pending play's refusal.
                }
                let Some(received) = self
                    .pending
                    .take()
                    .or_else(|| channel.calls.try_recv().ok())
                else {
                    break;
                };
                if now_ms >= received.received_at_ms.saturating_add(RESPONSE_MS) {
                    return Err(FixtureError::TimedOut);
                }
                self.validate_call(&received.call)?;
                let id = received.call.id;
                if let FixtureCommand::Open { machine_label } = &received.call.command
                    && self.opening != Some(id)
                {
                    self.title =
                        practice_title(machine_label, received.call.attempt, FixtureId(id))?;
                    self.opening = Some(id);
                    self.pending = Some(received);
                    break; // Yield so the viewport applies the title before observation.
                }
                let observation = match received.call.command {
                    FixtureCommand::Open { .. } | FixtureCommand::ObserveWindow { .. } => {
                        self.native.0.observe_window(id, &self.title)
                    }
                    _ => Some(unavailable_window()),
                };
                let Some(observation) = observation.filter(|o| {
                    !matches!(received.call.command, FixtureCommand::Open { .. })
                        || matches!(o, Ok((window, facts)) if window.0 != 0 && *facts != OwnWindowFacts::Missing)
                }) else {
                    self.pending = Some(received);
                    break;
                };
                let message = match self.handle(received.call.clone(), observation) {
                    Err(FixtureError::Busy) => {
                        self.pending = Some(received);
                        break;
                    }
                    result => result?,
                };
                channel.send(message, received.received_at_ms)?;
                if self.stopped {
                    self.close_reply = Some(id);
                }
            }
            let pending_stop = self.pending.as_ref().is_some_and(|p| {
                matches!(p.call.command, FixtureCommand::StopTone { tone, .. } if Some(tone) == self.running_tone)
            });
            if !pending_stop
                && self.running_tone.is_some()
                && self.native.0.tone_state() == OwnToneState::Stopped
            {
                let tone = self.running_tone.take().ok_or(FixtureError::NotOwned)?;
                let fixture = self.fixture.ok_or(FixtureError::NotOwned)?;
                let message =
                    self.message(None, Ok(FixtureEvent::ToneStopped { fixture, tone }))?;
                if !self.send_lifecycle(channel, message, now_ms)? {
                    return Ok(false);
                }
            }
            if !self.stopped
                && let Some(at) = self.close_at.take()
                && let Some(message) = self.close_requested()?
                && !self.send_lifecycle(channel, message, at)?
            {
                return Ok(false);
            }
            Ok(self.stopped && self.close_reply.is_none_or(|id| channel.written(id)))
        })();
        if result.is_err() {
            channel.abort();
            self.stop();
        }
        result
    }
    /// Emit the same viewport commands in native and headless frames.
    pub fn viewport(&mut self, ctx: &egui::Context, channel: &ChildChannel, now_ms: u64) {
        let close = ctx.input(|i| i.viewport().close_requested());
        let result = self.process(channel, now_ms, close);
        if self.pending.is_some() || self.pending_event.is_some() || self.close_at.is_some() {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
        if close && matches!(result, Ok(false)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(self.title.clone()));
        if matches!(result, Ok(true) | Err(_)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
    /// Counts submitted content updates, not elapsed intervals or native/presented frames.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        now_ms: u64,
    ) -> Result<(egui::Response, egui::Response)> {
        if self.stopped {
            return Err(FixtureError::ChannelClosed);
        }
        if self.fixture.is_some() && now_ms >= self.next_pattern {
            self.pattern_ticks = self
                .pattern_ticks
                .checked_add(1)
                .ok_or(FixtureError::CounterExhausted)?;
            self.next_pattern = now_ms
                .checked_add(500)
                .ok_or(FixtureError::CounterExhausted)?;
        }
        ui.heading(&self.title);
        ui.label("Synthetic sample: Crosspane practice 123 — type only test text below.");
        ui.checkbox(&mut self.reduced_motion, "Reduced motion");
        ui.style_mut().animation_time = if self.reduced_motion { 0.0 } else { 0.16 };
        let colors = [theme::FROST, theme::GLACIER];
        let color = colors[(self.pattern_ticks % 2) as usize];
        // Both motion modes use discrete colour/number changes; there is no spatial animation.
        ui.label(
            egui::RichText::new(format!("Pattern {}", self.pattern_ticks))
                .size(36.0)
                .color(color),
        );
        ui.label(format!(
            "Content updates: {} · Target clicks: {}",
            self.pattern_ticks, self.target_clicks
        ));
        let target = ui.add_enabled(
            self.phase.is_some(),
            egui::Button::new("Practice click target").min_size(egui::vec2(320.0, 100.0)),
        );
        let frame = ui.ctx().cumulative_frame_nr();
        for event in ui.input(|i| i.events.clone()) {
            if let egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                ..
            } = event
            {
                if pressed {
                    self.press_phase = self.phase.filter(|_| target.rect.contains(pos));
                } else {
                    if target.rect.contains(pos)
                        && target.clicked_by(egui::PointerButton::Primary)
                        && self.press_phase.is_some()
                        && self.press_phase == self.phase
                        && self.click_frame != Some(frame)
                    {
                        self.target_clicks = self
                            .target_clicks
                            .checked_add(1)
                            .ok_or(FixtureError::CounterExhausted)?;
                        self.click_frame = Some(frame);
                    }
                    self.press_phase = None;
                }
            }
        }
        self.editor_context = Some(ui.ctx().clone());
        let text = ui.add(
            egui::TextEdit::singleline(&mut self.text)
                .id(egui::Id::new("practice-editor"))
                .char_limit(128)
                .hint_text("Volatile test text"),
        );
        if ui.button("Clear test text").clicked() {
            self.clear_text();
        }
        ui.ctx().output_mut(|o| {
            o.commands
                .retain(|c| !matches!(c, egui::OutputCommand::CopyText(_)))
        });
        if self.fixture.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(
                self.next_pattern.saturating_sub(now_ms).max(1),
            ));
        }
        Ok((target, text))
    }
}
impl Drop for Practice {
    fn drop(&mut self) {
        self.stop_output();
    }
}
#[derive(Debug)]
pub struct ReceivedCall {
    pub call: FixtureCall,
    pub received_at_ms: u64,
}
/// Both injected streams must be nonblocking. No GUI method performs pipe I/O or waits for exit.
#[derive(Debug)]
pub struct ChildChannel {
    calls: mpsc::Receiver<ReceivedCall>,
    replies: mpsc::SyncSender<(Vec<u8>, u64, Option<u64>)>,
    stop: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<FixtureError>>>,
    completed: Arc<AtomicU64>,
}
impl ChildChannel {
    pub fn new(
        mut reader: impl Read + Send + 'static,
        mut writer: impl Write + Send + 'static,
        clock: FixtureClock,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self> {
        let (send, calls) = mpsc::sync_channel(MAX_QUEUE);
        let (replies, receive) = mpsc::sync_channel::<(Vec<u8>, u64, Option<u64>)>(MAX_QUEUE);
        let stop = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None));
        let halted = stop.clone();
        let error = failure.clone();
        let completed = Arc::new(AtomicU64::new(0));
        let written = completed.clone();
        thread::Builder::new()
            .name("practice-pipes".into())
            .spawn(move || {
                let mut input = Vec::new();
                let mut output: Option<(Vec<u8>, usize, u64, Option<u64>)> = None;
                let mut pending = BTreeMap::<u64, u64>::new();
                let result = (|| -> Result<()> {
                    while !halted.load(Ordering::Acquire) {
                        if pending.values().any(|until| clock() >= *until) {
                            return Err(FixtureError::TimedOut);
                        }
                        if output.is_none() {
                            output = receive
                                .try_recv()
                                .ok()
                                .map(|(b, until, id)| (b, 0, until, id));
                        }
                        if let Some((bytes, offset, until, id)) = &mut output {
                            if clock() >= *until {
                                return Err(FixtureError::TimedOut);
                            }
                            if halted.load(Ordering::Acquire) {
                                break;
                            }
                            if let Some(n) = progress(writer.write(&bytes[*offset..]))? {
                                if n > bytes.len() - *offset {
                                    return Err(FixtureError::InvalidMessage);
                                }
                                *offset += n;
                                if clock() >= *until {
                                    return Err(FixtureError::TimedOut);
                                }
                                if *offset == bytes.len() {
                                    if let Some(id) = id {
                                        pending.remove(id);
                                        written.store(*id, Ordering::Release);
                                    }
                                    output = None;
                                    wake();
                                }
                            }
                        }
                        let mut byte = [0];
                        if let Some(n) = progress(reader.read(&mut byte))? {
                            if n != 1 {
                                return Err(FixtureError::InvalidMessage);
                            }
                            input.push(byte[0]);
                            if input.len() > MAX_LINE_BYTES {
                                return Err(FixtureError::InvalidMessage);
                            }
                            if byte[0] == b'\n' {
                                let received_at_ms = clock();
                                let call = decode_control(&input)?.call;
                                if pending.len() == MAX_QUEUE {
                                    return Err(FixtureError::Busy);
                                }
                                let until = received_at_ms
                                    .checked_add(RESPONSE_MS)
                                    .ok_or(FixtureError::CounterExhausted)?;
                                if pending.insert(call.id, until).is_some() {
                                    return Err(FixtureError::InvalidMessage);
                                }
                                send.try_send(ReceivedCall {
                                    call,
                                    received_at_ms,
                                })
                                .map_err(|_| FixtureError::Busy)?;
                                input.clear();
                                wake();
                            }
                        } else {
                            thread::sleep(Duration::from_millis(1))
                        }
                    }
                    Ok(())
                })();
                if let Err(reason) = result {
                    if let Ok(mut error) = error.lock() {
                        *error = Some(reason);
                    }
                    halted.store(true, Ordering::Release);
                    wake();
                }
            })
            .map_err(|_| FixtureError::Unavailable)?;
        Ok(Self {
            calls,
            replies,
            stop,
            failure,
            completed,
        })
    }
    pub fn poll(&self) -> Vec<ReceivedCall> {
        self.calls.try_iter().take(MAX_QUEUE).collect()
    }
    pub fn failure(&self) -> Option<FixtureError> {
        self.failure.try_lock().ok().and_then(|e| *e)
    }
    pub fn send(&self, message: FixtureMessage, received_at_ms: u64) -> Result<()> {
        if self.stop.load(Ordering::Acquire) {
            return Err(FixtureError::ChannelClosed);
        }
        let until = received_at_ms
            .checked_add(RESPONSE_MS)
            .ok_or(FixtureError::CounterExhausted)?;
        let id = message.call_id;
        let bytes = encode_event(&FixtureEventPacket {
            schema_version: 1,
            message,
        })?;
        self.replies
            .try_send((bytes, until, id))
            .map_err(|_| FixtureError::Busy)
    }
    pub fn abort(&self) {
        self.stop.store(true, Ordering::Release);
    }
    pub fn written(&self, id: u64) -> bool {
        id != 0 && self.completed.load(Ordering::Acquire) == id
    }
}
impl Drop for ChildChannel {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}
fn progress(result: io::Result<usize>) -> Result<Option<usize>> {
    match result {
        Ok(0) => Err(FixtureError::ChannelClosed),
        Ok(n) => Ok(Some(n)),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(_) => Err(FixtureError::ChannelClosed),
    }
}
struct PracticeApp {
    practice: Practice,
    channel: ChildChannel,
    clock: FixtureClock,
}
impl eframe::App for PracticeApp {
    fn logic(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.practice.viewport(ctx, &self.channel, (self.clock)());
    }
    fn ui(&mut self, ui: &mut egui::Ui, _: &mut eframe::Frame) {
        if self.practice.stopped {
            return;
        }
        if self.practice.show(ui, (self.clock)()).is_err() {
            self.channel.abort();
            self.practice.stop();
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}
fn platform_native(font: &std::path::Path) -> Box<dyn TutorialNative> {
    #[cfg(target_os = "linux")]
    {
        let _ = font;
        Box::new(UnavailableTutorial)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = font;
        Box::new(UnavailableTutorial)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = font;
        Box::new(UnavailableTutorial)
    }
}
pub fn run(options: TutorialOptions) -> Result<()> {
    let path = options.validate()?;
    // The parent supplies its admitted system font. Bound and parse it before opening a viewport.
    let fonts = crate::gui::load_review_font(path).map_err(|_| FixtureError::Unavailable)?;
    // Owned duplicates bypass stdio buffering: every partial write is an actual pipe write.
    let input = File::from(rustix::io::dup(io::stdin()).map_err(|_| FixtureError::NotOwned)?);
    let output = File::from(rustix::io::dup(io::stdout()).map_err(|_| FixtureError::NotOwned)?);
    for fd in [&input, &output] {
        let stat = rustix::fs::fstat(fd).map_err(|_| FixtureError::NotOwned)?;
        if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::Fifo {
            return Err(FixtureError::NotOwned);
        }
        let flags = rustix::fs::fcntl_getfl(fd).map_err(|_| FixtureError::Unavailable)?;
        rustix::fs::fcntl_setfl(fd, flags | rustix::fs::OFlags::NONBLOCK)
            .map_err(|_| FixtureError::Unavailable)?;
    }
    let started = Instant::now();
    let clock: FixtureClock =
        Arc::new(move || started.elapsed().as_millis().min(u64::MAX as u128) as u64);
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([800.0, 600.0]),
        ..Default::default()
    };
    let tutorial_native = platform_native(path);
    eframe::run_native(
        "Crosspane practice",
        native,
        Box::new(move |cc| {
            cc.egui_ctx.set_fonts(fonts);
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            cc.egui_ctx.set_style_of(egui::Theme::Dark, theme::style());
            let ctx = cc.egui_ctx.clone();
            let channel = ChildChannel::new(
                input,
                output,
                clock.clone(),
                Arc::new(move || ctx.request_repaint()),
            )?;
            Ok(Box::new(PracticeApp {
                practice: Practice::with_native(tutorial_native),
                channel,
                clock,
            }))
        }),
    )
    .map_err(|_| FixtureError::Unavailable)
}
