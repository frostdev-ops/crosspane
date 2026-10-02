//! Non-activating HUD panels. Only the command handle crosses threads; native objects stay in
//! the main thread's registry. Showing our own panels requires no TCC permission.
//!
//! `show` and `hide` never wait for the main queue (WP-2.42): the engine calls them from its own
//! loop, and during native window tracking or GPU load the main queue can stall for much longer
//! than the input path may wait.
//!
//! - Each call takes the next sequence number for its overlay id and queues its work for the main
//!   thread. When that work runs, a command a newer one for the same id has superseded is skipped,
//!   which coalesces bursts of show and hide.
//! - Events are published only for the newest command of an id, checked at the moment of
//!   publishing. A late event from an old command can't cancel a newer one.
//! - A show that fails on the main thread publishes `Unavailable`. A hide that fails publishes
//!   nothing: a panel that may still be visible is the safe direction for the visible-indicator
//!   invariant (04 §5).
//! - Each configuration of a panel (a show that reaches the main thread, or a new subscriber's
//!   replay) owes exactly one first outcome for its sequence number: `Visible` once the panel is
//!   on screen, `Unavailable` if it is not on screen, or `Unavailable` if it still isn't presented
//!   after `PRESENTATION_WAIT` (500 ms). AppKit reports occlusion asynchronously after a panel is ordered
//!   in, so a panel that is merely not yet reported visible gets that wait. Silence is never an
//!   answer: the engine keeps a capture running only while its HUD is up (04 §8 invariant 5).
//!   After the outcome, transitions publish as they happen.
//! - That bound holds while the main queue is stalled. Every configuration also arms a deadline on
//!   a global dispatch queue, which never touches AppKit; the main queue's own recheck at the same
//!   time can still publish `Visible`. A per-panel `pending` flag, cleared under the lock that
//!   publishes events, is the single gate: whoever clears it (a notification, the main-queue
//!   recheck or the off-main deadline) publishes the first outcome, and nobody else does. A panel
//!   that is presented after an off-main `Unavailable` publishes `Visible` as an ordinary
//!   transition.
//! - What a panel owes belongs to its current configuration, not to the command that made it. A
//!   show queued behind a stalled main queue can't configure anything or arm a deadline, so it must
//!   not silence the deadline of the configuration before it: that deadline's `Unavailable`
//!   speaks for the newest command issued for the panel, recorded when the engine's call is made.
//!   When the queued show finally runs, it starts a new configuration and owes its own outcome.
//!   A display that disappears goes through the same gate: it answers `Unavailable` only if the
//!   configuration hasn't answered yet or was last reported visible.
//! - Each configuration has a generation, bumped by every show and replay (and by retiring a
//!   panel). Both kinds of deadline carry the generation they were armed for and do nothing once
//!   it has moved on, so an old timer can't answer for a newer wait.
//! - A call returns an error only for synchronous preconditions (the handle is closed, the overlay
//!   is invalid, or `subscribe` ran twice). It then publishes no event: the error is the answer.
//!   `subscribe` doesn't wait for the main thread either; the replay of current state arrives
//!   through the sink.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fmt;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use block2::RcBlock;
use crosspane_platform::{
    EventSink, Overlay, OverlayAnchor, OverlayEvent, OverlayHost, OverlayId, PlatformError,
};
use crosspane_types::id::DisplayId;
use dispatch2::{DispatchQoS, DispatchQueue, DispatchTime, GlobalQueueIdentifier};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplicationDidChangeScreenParametersNotification, NSBackingStoreType, NSBox, NSBoxType,
    NSColor, NSFont, NSLineBreakMode, NSPanel, NSScreen, NSStatusWindowLevel, NSTextField,
    NSTitlePosition, NSView, NSWindowCollectionBehavior,
    NSWindowDidChangeOcclusionStateNotification, NSWindowOcclusionState, NSWindowStyleMask,
};
use objc2_foundation::{
    NSNotification, NSNotificationCenter, NSNotificationName, NSNumber, NSObjectProtocol,
    NSOperationQueue, NSPoint, NSRect, NSSize, NSString, ns_string,
};

use crate::main_thread::{on_main, spawn_on_main};

/// How long the notification observers' hop to the main thread may take. They already run there,
/// so it doesn't wait. `show`, `hide` and `subscribe` don't wait for the main thread at all.
const CALL_TIMEOUT: Duration = Duration::from_millis(50);
/// How long a configured panel may go without being reported visible before it is declared
/// `Unavailable`. Matches the budget the GUI test allows for a presentation.
const PRESENTATION_WAIT: Duration = Duration::from_millis(500);
const MARGIN: f64 = 24.0;

define_class!(
    // SAFETY: NSPanel has no additional subclassing requirements. No ivars or custom Drop;
    // the overrides have the exact AppKit BOOL-returning, no-argument signatures.
    #[unsafe(super(NSPanel))]
    #[thread_kind = MainThreadOnly]
    #[name = "CrosspaneOverlayPanel"]
    #[derive(Debug)]
    struct OverlayPanel;

    impl OverlayPanel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> bool { false }

        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main_window(&self) -> bool { false }
    }
);

/// Which kind of command a sequence number belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Show,
    Hide,
}

/// The newest command number issued for each overlay id. Pure bookkeeping: what the main thread
/// runs and what it may report are decided here, so tests can check them without AppKit.
#[derive(Debug, Default)]
struct Sequencer {
    newest: BTreeMap<OverlayId, u64>,
}

impl Sequencer {
    /// The next sequence number for `id`; it supersedes every earlier command for `id`.
    fn issue(&mut self, id: OverlayId) -> u64 {
        let slot = self.newest.entry(id).or_insert(0);
        *slot = slot.saturating_add(1);
        *slot
    }

    fn is_newest(&self, id: OverlayId, seq: u64) -> bool {
        self.newest.get(&id) == Some(&seq)
    }

    /// The newest command issued for `id`, which may still be queued for the main thread.
    fn newest(&self, id: OverlayId) -> Option<u64> {
        self.newest.get(&id).copied()
    }

    /// Whether the main thread should run command `seq` for `id`, or skip it because a later
    /// command for the same id has superseded it.
    fn should_run(&self, id: OverlayId, seq: u64) -> bool {
        self.is_newest(id, seq)
    }

    /// Whether `event`, caused by command `seq`, may be published: only the newest command of an
    /// id speaks for it.
    fn admits(&self, seq: u64, event: OverlayEvent) -> bool {
        self.is_newest(event_id(event), seq)
    }

    /// The event to publish when command `seq` of `kind` failed on the main thread. A failed show
    /// means nothing is on screen; a failed hide means the overlay may still be, which is the safe
    /// direction, so it publishes nothing.
    fn failure_event(&self, kind: Kind, id: OverlayId, seq: u64) -> Option<OverlayEvent> {
        let event = OverlayEvent::Unavailable(id);
        match kind {
            Kind::Show if self.admits(seq, event) => Some(event),
            Kind::Show | Kind::Hide => None,
        }
    }
}

fn event_id(event: OverlayEvent) -> OverlayId {
    match event {
        OverlayEvent::Visible(id) | OverlayEvent::Unavailable(id) => id,
    }
}

/// What the screen shows of a panel right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sight {
    /// Ordered in, and AppKit reports at least part of it visible.
    Visible,
    /// Ordered in, but AppKit doesn't (yet) report any of it visible.
    Occluded,
    /// Not ordered in.
    OffScreen,
}

/// An availability event without its overlay id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Visible,
    Unavailable,
}

impl Outcome {
    fn event(self, id: OverlayId) -> OverlayEvent {
        match self {
            Self::Visible => OverlayEvent::Visible(id),
            Self::Unavailable => OverlayEvent::Unavailable(id),
        }
    }
}

/// The availability acknowledgements one panel owes. Pure, so tests can run it without AppKit.
///
/// Every configuration owes exactly one first outcome (see the module docs): `Visible` once the
/// panel is on screen, `Unavailable` if it is not ordered in, and `Unavailable` when the
/// presentation wait runs out. Occluded is not an answer by itself, because AppKit reports
/// occlusion after the panel is ordered in; but nor may it stay silent, or a capture would run
/// without its HUD. Once the first outcome is out, later changes publish as transitions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Presence {
    /// Counts configurations (shows and replays) and retirements. A timer carries the number it
    /// was armed for and does nothing once it no longer matches.
    generation: u64,
    /// The command the current configuration answers for. Outcomes speak for it.
    seq: u64,
    visible: bool,
    /// The configuration's first outcome hasn't been published. The single gate: whichever path
    /// clears it (a notification, the main-queue recheck, the off-main deadline) publishes the
    /// first outcome, and no other path does.
    pending: bool,
}

impl Presence {
    /// The panel was (re)configured: an outcome is owed again. `seq` is the command it answers
    /// for; a replay keeps the current one. Returns the new generation.
    fn configure(&mut self, seq: Option<u64>) -> u64 {
        self.generation = self.generation.saturating_add(1);
        if let Some(seq) = seq {
            self.seq = seq;
        }
        self.visible = false;
        self.pending = true;
        self.generation
    }

    /// The panel is gone: no outcome is owed, and every timer armed so far is void.
    fn retire(&mut self) {
        self.generation = self.generation.saturating_add(1);
        self.visible = false;
        self.pending = false;
    }

    fn owes(&self, generation: u64) -> bool {
        self.pending && self.generation == generation
    }

    /// The screen was observed (a configuration, a notification, a replay).
    fn observe(&mut self, sight: Sight) -> Option<Outcome> {
        let visible = sight == Sight::Visible;
        let previous = std::mem::replace(&mut self.visible, visible);
        if self.pending {
            return match sight {
                Sight::Visible => {
                    self.pending = false;
                    Some(Outcome::Visible)
                }
                Sight::OffScreen => {
                    self.pending = false;
                    Some(Outcome::Unavailable)
                }
                // Not reported visible yet: wait for a notification, or a deadline.
                Sight::Occluded => None,
            };
        }
        match (visible, previous) {
            (true, false) => Some(Outcome::Visible),
            (false, true) => Some(Outcome::Unavailable),
            _ => None,
        }
    }

    /// The main queue's recheck at the end of the presentation wait armed for `generation`:
    /// report what the screen shows now, and if the panel still owes its outcome, it is not going
    /// to be presented.
    fn recheck(&mut self, generation: u64, sight: Sight) -> Option<Outcome> {
        if generation != self.generation {
            return None;
        }
        let seen = self.observe(sight);
        if seen.is_none() && self.pending {
            self.pending = false;
            return Some(Outcome::Unavailable);
        }
        seen
    }

    /// The off-main deadline of the presentation wait armed for `generation`. It sees no screen:
    /// if the first outcome is still owed, the panel is declared unavailable. A later sighting of
    /// the panel is an ordinary transition.
    fn expire(&mut self, generation: u64) -> Option<Outcome> {
        if !self.owes(generation) {
            return None;
        }
        self.pending = false;
        Some(Outcome::Unavailable)
    }
}

/// Everything the lock that publishes events protects: command numbering and what each panel owes.
#[derive(Debug, Default)]
struct Ledger {
    sequencer: Sequencer,
    panels: BTreeMap<OverlayId, Presence>,
}

impl Ledger {
    /// An outcome of `id` for command `seq`, if that command is still the newest.
    fn admit(&self, seq: u64, id: OverlayId, outcome: Outcome) -> Option<OverlayEvent> {
        let event = outcome.event(id);
        self.sequencer.admits(seq, event).then_some(event)
    }

    /// Advance the presence of `id` and publish its outcome for the command it answers for.
    fn step(
        &mut self,
        id: OverlayId,
        advance: impl FnOnce(&mut Presence) -> Option<Outcome>,
    ) -> Option<OverlayEvent> {
        let presence = self.panels.entry(id).or_default();
        let outcome = advance(presence)?;
        let seq = presence.seq;
        self.admit(seq, id, outcome)
    }

    fn configure(&mut self, id: OverlayId, seq: Option<u64>) -> u64 {
        self.panels.entry(id).or_default().configure(seq)
    }

    fn retire(&mut self, id: OverlayId) {
        self.panels.entry(id).or_default().retire();
    }

    fn owes(&self, id: OverlayId, generation: u64) -> bool {
        self.panels
            .get(&id)
            .is_some_and(|presence| presence.owes(generation))
    }

    /// Like [`Ledger::step`], for a deadline. The outcome owed belongs to the panel's current
    /// configuration, not to the command that made it: if the deadline finds that configuration
    /// still unanswered and declares it unavailable, the event speaks for the newest command
    /// issued for the panel, which a show queued behind a stalled main queue may already be. That
    /// command can't configure or arm a deadline until the main queue runs, so the safety
    /// deadline must not be suppressed by it. Anything else (a `Visible`, a transition) speaks
    /// for the configuration's own command, as in [`Ledger::step`].
    fn step_deadline(
        &mut self,
        id: OverlayId,
        advance: impl FnOnce(&mut Presence) -> Option<Outcome>,
    ) -> Option<OverlayEvent> {
        let newest = self.sequencer.newest(id);
        let presence = self.panels.entry(id).or_default();
        let owed = presence.pending;
        let outcome = advance(presence)?;
        let seq = match outcome {
            Outcome::Unavailable if owed => newest.unwrap_or(presence.seq),
            Outcome::Unavailable | Outcome::Visible => presence.seq,
        };
        self.admit(seq, id, outcome)
    }

    fn observe(&mut self, id: OverlayId, sight: Sight) -> Option<OverlayEvent> {
        self.step(id, |presence| presence.observe(sight))
    }

    fn recheck(&mut self, id: OverlayId, generation: u64, sight: Sight) -> Option<OverlayEvent> {
        self.step_deadline(id, |presence| presence.recheck(generation, sight))
    }

    fn expire(&mut self, id: OverlayId, generation: u64) -> Option<OverlayEvent> {
        self.step_deadline(id, |presence| presence.expire(generation))
    }

    /// The panel's display is gone. Whatever it still owes is answered, through the same gate as
    /// every other outcome: `Unavailable` only if the current configuration hasn't answered yet,
    /// or was last reported visible. A panel that already answered `Unavailable` (an expired
    /// deadline, say) adds no second one. Then the panel is retired, whatever was published, so
    /// its timers are void.
    fn gone(&mut self, id: OverlayId) -> Option<OverlayEvent> {
        let presence = self.panels.entry(id).or_default();
        let reportable = presence.pending || presence.visible;
        let seq = presence.seq;
        presence.retire();
        if !reportable {
            return None;
        }
        self.admit(seq, id, Outcome::Unavailable)
    }
}

/// Runs work later on a thread of its own, never the main thread, so it keeps its time while the
/// main queue is stalled.
trait Scheduler: Send + Sync {
    /// Run `work` once, about `delay` from now. Never waits.
    fn after(&self, delay: Duration, work: Box<dyn FnOnce() + Send>);
}

/// Production: a global dispatch queue. Its worker threads don't depend on the main queue.
struct GlobalQueueTimers;

impl Scheduler for GlobalQueueTimers {
    fn after(&self, delay: Duration, work: Box<dyn FnOnce() + Send>) {
        let queue = DispatchQueue::global_queue(GlobalQueueIdentifier::QualityOfService(
            DispatchQoS::UserInitiated,
        ));
        match DispatchTime::try_from(delay) {
            Ok(when) => {
                if queue.after(when, work).is_err() {
                    tracing::warn!("scheduling an overlay deadline failed");
                }
            }
            // Unreachable for a delay this short; if it ever fails, answer now rather than never.
            Err(()) => queue.exec_async(work),
        }
    }
}

struct Shared {
    alive: AtomicBool,
    // Serialises events from notifications, deadlines and failed commands. Locked before
    // `ledger`; the sink only queues, so holding both across `send` is cheap, and it makes "newest
    // at the time of publishing" and "first outcome claimed" exact.
    sink: Mutex<Option<Arc<dyn EventSink<OverlayEvent>>>>,
    ledger: Mutex<Ledger>,
    timers: Arc<dyn Scheduler>,
}

impl Shared {
    fn new(timers: Arc<dyn Scheduler>) -> Self {
        Self {
            alive: AtomicBool::new(true),
            sink: Mutex::new(None),
            ledger: Mutex::new(Ledger::default()),
            timers,
        }
    }

    fn ledger(&self) -> std::sync::MutexGuard<'_, Ledger> {
        self.ledger.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn issue(&self, id: OverlayId) -> u64 {
        self.ledger().sequencer.issue(id)
    }

    fn should_run(&self, id: OverlayId, seq: u64) -> bool {
        self.ledger().sequencer.should_run(id, seq)
    }

    /// Run `decide` on the ledger and publish the event it returns. `decide` is where a path
    /// claims a first outcome; it runs even with no subscriber, so state stays honest.
    fn publish(&self, decide: impl FnOnce(&mut Ledger) -> Option<OverlayEvent>) {
        let sink = self.sink.lock().unwrap_or_else(|e| e.into_inner());
        let mut ledger = self.ledger();
        let event = decide(&mut ledger);
        if self.alive.load(Ordering::Acquire)
            && let Some(sink) = &*sink
            && let Some(event) = event
        {
            sink.send(event);
        }
    }

    /// Publish what a failure of command `seq` calls for, if anything.
    fn command_failed(&self, kind: Kind, id: OverlayId, seq: u64) {
        self.publish(|ledger| ledger.sequencer.failure_event(kind, id, seq));
    }

    /// The panel of `id` was configured (a show reached the main thread, or a replay): it owes an
    /// outcome for command `seq` (a replay keeps the current one). Arms the off-main deadline and
    /// returns the generation, which the main-queue recheck must carry too.
    fn configure(self: &Arc<Self>, id: OverlayId, seq: Option<u64>) -> u64 {
        let generation = self.ledger().configure(id, seq);
        let shared = self.clone();
        self.timers.after(
            PRESENTATION_WAIT,
            Box::new(move || shared.expire(id, generation)),
        );
        generation
    }

    /// The screen was observed for the panel of `id`.
    fn observe(&self, id: OverlayId, sight: Sight) {
        self.publish(|ledger| ledger.observe(id, sight));
    }

    /// The main queue's recheck of generation `generation` of the panel of `id`.
    fn recheck(&self, id: OverlayId, generation: u64, sight: Sight) {
        self.publish(|ledger| ledger.recheck(id, generation, sight));
    }

    /// The off-main deadline of generation `generation` of the panel of `id`. Touches no AppKit
    /// object, only ids and this state.
    fn expire(&self, id: OverlayId, generation: u64) {
        self.publish(|ledger| ledger.expire(id, generation));
    }

    /// The display of the panel of `id` is gone.
    fn panel_gone(&self, id: OverlayId) {
        self.publish(|ledger| ledger.gone(id));
    }

    /// The panel of `id` was taken down: nothing is owed, and its timers are void.
    fn retire(&self, id: OverlayId) {
        self.ledger().retire(id);
    }

    fn owes(&self, id: OverlayId, generation: u64) -> bool {
        self.ledger().owes(id, generation)
    }
}

enum Action {
    Show(Overlay),
    Hide,
}

/// One command, queued for the main thread.
struct Command {
    id: OverlayId,
    seq: u64,
    action: Action,
}

/// What the command handle needs from the main thread. AppKit is the only implementation in
/// production; unit tests substitute one whose "main thread" they can stall.
trait Backend: Send + Sync {
    /// Queue `work` to run later on the main thread. Never waits for it.
    fn spawn(&self, work: Box<dyn FnOnce() + Send>);
    /// Present or update the panel for `id` as command `seq`. Called only from queued work.
    fn show_panel(&self, id: OverlayId, overlay: &Overlay, seq: u64) -> Result<(), PlatformError>;
    /// Take the panel for `id` off screen. Hiding one that isn't shown succeeds. Called only from
    /// queued work.
    fn hide_panel(&self, id: OverlayId) -> Result<(), PlatformError>;
    /// Redeliver the current state of every panel to a new subscriber, through the sink. Called
    /// only from queued work.
    fn replay(&self);
    /// Release every panel of this handle. Called when the handle drops.
    fn close(&self);
}

/// What the main thread does for one queued command.
fn run_command(shared: &Shared, backend: &dyn Backend, command: Command) {
    let Command { id, seq, action } = command;
    if !shared.alive.load(Ordering::Acquire) {
        return;
    }
    if !shared.should_run(id, seq) {
        tracing::trace!(overlay = id.0, seq, "overlay command superseded");
        return;
    }
    match action {
        Action::Show(overlay) => {
            if let Err(error) = backend.show_panel(id, &overlay, seq) {
                tracing::warn!(overlay = id.0, %error, "showing an overlay failed");
                // Drop whatever the failed show left behind.
                if let Err(error) = backend.hide_panel(id) {
                    tracing::debug!(overlay = id.0, %error, "cleaning up a failed overlay failed");
                }
                shared.command_failed(Kind::Show, id, seq);
            }
        }
        Action::Hide => {
            if let Err(error) = backend.hide_panel(id) {
                tracing::debug!(
                    overlay = id.0,
                    %error,
                    "hiding an overlay failed; it may still be visible"
                );
                shared.command_failed(Kind::Hide, id, seq);
            }
        }
    }
}

/// Synchronous precondition for `show`: the overlay must name a display that can exist.
/// `DisplayId(0)` is `kCGNullDirectDisplay`.
fn validate(overlay: &Overlay) -> Result<(), PlatformError> {
    if overlay.display.0 == 0 {
        return Err(PlatformError::NotFound);
    }
    Ok(())
}

/// A Send command handle for panels owned by the AppKit main thread.
pub struct MacOverlay {
    key: u64,
    shared: Arc<Shared>,
    backend: Arc<dyn Backend>,
}

impl fmt::Debug for MacOverlay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MacOverlay")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl MacOverlay {
    pub fn new() -> Result<Self, PlatformError> {
        static NEXT_KEY: AtomicU64 = AtomicU64::new(1);
        let key = NEXT_KEY.fetch_add(1, Ordering::Relaxed);
        let shared = Arc::new(Shared::new(Arc::new(GlobalQueueTimers)));
        let backend = Arc::new(AppKitBackend {
            key,
            shared: shared.clone(),
        });
        Ok(Self {
            key,
            shared,
            backend,
        })
    }

    /// Number a command and queue it for the main thread. Returns without waiting for it.
    fn command(&self, id: OverlayId, action: Action) -> Result<(), PlatformError> {
        if !self.shared.alive.load(Ordering::Acquire) {
            return Err(PlatformError::Backend("overlay host closed".into()));
        }
        if let Action::Show(overlay) = &action {
            validate(overlay)?;
        }
        let command = Command {
            id,
            seq: self.shared.issue(id),
            action,
        };
        let shared = self.shared.clone();
        let backend = self.backend.clone();
        self.backend.spawn(Box::new(move || {
            run_command(&shared, backend.as_ref(), command);
        }));
        Ok(())
    }
}

impl OverlayHost for MacOverlay {
    fn subscribe(&mut self, sink: Arc<dyn EventSink<OverlayEvent>>) -> Result<(), PlatformError> {
        let mut slot = self.shared.sink.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return Err(PlatformError::Backend(
                "OverlayHost::subscribe called twice".into(),
            ));
        }
        *slot = Some(sink);
        drop(slot);
        // The replay of what is already on screen runs on the main thread, later; its events
        // reach the sink like any other. Nothing here waits for the main queue.
        let backend = self.backend.clone();
        self.backend.spawn(Box::new(move || backend.replay()));
        Ok(())
    }

    fn show(&mut self, id: OverlayId, overlay: &Overlay) -> Result<(), PlatformError> {
        self.command(id, Action::Show(overlay.clone()))
    }

    fn hide(&mut self, id: OverlayId) -> Result<(), PlatformError> {
        self.command(id, Action::Hide)
    }
}

impl Drop for MacOverlay {
    fn drop(&mut self) {
        self.shared.alive.store(false, Ordering::Release);
        self.backend.close();
    }
}

/// The AppKit implementation: panels live in this thread-local registry on the main thread.
struct AppKitBackend {
    key: u64,
    shared: Arc<Shared>,
}

impl AppKitBackend {
    fn host(&self) -> Option<Rc<Host>> {
        HOSTS.with(|hosts| hosts.borrow().get(&self.key).cloned())
    }
}

impl Backend for AppKitBackend {
    fn spawn(&self, work: Box<dyn FnOnce() + Send>) {
        spawn_on_main(move |_| work());
    }

    fn show_panel(&self, id: OverlayId, overlay: &Overlay, seq: u64) -> Result<(), PlatformError> {
        let mtm = MainThreadMarker::new()
            .ok_or_else(|| PlatformError::Backend("overlay work ran off the main thread".into()))?;
        let host = match self.host() {
            Some(host) => host,
            None => {
                let host = Rc::new(Host::new(self.key, self.shared.clone()));
                HOSTS.with(|hosts| hosts.borrow_mut().insert(self.key, host.clone()));
                host
            }
        };
        host.show(id, overlay, seq, mtm)
    }

    fn hide_panel(&self, id: OverlayId) -> Result<(), PlatformError> {
        match self.host() {
            Some(host) => host.hide(id),
            None => Ok(()),
        }
    }

    fn replay(&self) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        if let Some(host) = self.host() {
            host.replay(mtm);
        }
    }

    fn close(&self) {
        let key = self.key;
        spawn_on_main(move |_| {
            // Drop outside the registry borrow: ordering panels out can deliver notifications.
            let host = HOSTS.with(|hosts| hosts.borrow_mut().remove(&key));
            drop(host);
        });
    }
}

thread_local! {
    static HOSTS: RefCell<BTreeMap<u64, Rc<Host>>> = const { RefCell::new(BTreeMap::new()) };
}

struct Host {
    key: u64,
    shared: Arc<Shared>,
    panels: RefCell<BTreeMap<OverlayId, Rc<Panel>>>,
    observers: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

impl Host {
    fn new(key: u64, shared: Arc<Shared>) -> Self {
        let center = NSNotificationCenter::defaultCenter();
        let queue = NSOperationQueue::mainQueue();
        // SAFETY: immutable notification-name constants exported by AppKit.
        let names = unsafe {
            [
                NSWindowDidChangeOcclusionStateNotification,
                NSApplicationDidChangeScreenParametersNotification,
            ]
        };
        let observers = names
            .into_iter()
            .map(|name: &NSNotificationName| {
                let block = RcBlock::new(move |_: NonNull<NSNotification>| {
                    let _ = on_main(CALL_TIMEOUT, move |mtm| {
                        if let Some(host) = HOSTS.with(|hosts| hosts.borrow().get(&key).cloned()) {
                            host.check(mtm);
                        }
                    });
                });
                // SAFETY: block captures only a Send integer, runs on the main queue, and ignores
                // the notification pointer. The observer is retained and removed in Host::drop.
                unsafe {
                    center.addObserverForName_object_queue_usingBlock(
                        Some(name),
                        None,
                        Some(&queue),
                        &block,
                    )
                }
            })
            .collect();
        Self {
            key,
            shared,
            panels: RefCell::new(BTreeMap::new()),
            observers,
        }
    }

    fn entries(&self) -> Vec<Rc<Panel>> {
        self.panels.borrow().values().cloned().collect()
    }

    /// Take the panel for `id` off screen. Fails only if the registry is mid-update, which leaves
    /// the panel where it is.
    fn hide(&self, id: OverlayId) -> Result<(), PlatformError> {
        let panel = self
            .panels
            .try_borrow_mut()
            .map_err(|_| PlatformError::Backend("overlay registry busy".into()))?
            .remove(&id);
        if let Some(panel) = panel {
            panel.window.orderOut(None);
            // Nothing is owed for a panel that is down, and its timers are void.
            self.shared.retire(id);
        }
        Ok(())
    }

    fn remove(&self, id: OverlayId) {
        if let Err(error) = self.hide(id) {
            tracing::debug!(overlay = id.0, %error, "removing an overlay panel failed");
        }
    }

    fn check(&self, mtm: MainThreadMarker) {
        for entry in self.entries() {
            if screen(entry.display.get(), mtm).is_none() {
                // Nothing can be presented any more: answer for the panel's command and retire it
                // in one step, then take it down.
                self.shared.panel_gone(entry.id);
                self.remove(entry.id);
            } else {
                entry.check(&self.shared);
            }
        }
    }

    fn show(
        &self,
        id: OverlayId,
        overlay: &Overlay,
        seq: u64,
        mtm: MainThreadMarker,
    ) -> Result<(), PlatformError> {
        let screen = screen(overlay.display, mtm).ok_or(PlatformError::NotFound)?;
        let panel = self.panels.borrow().get(&id).cloned();
        let panel = match panel {
            Some(panel) => panel,
            None => {
                let panel = Rc::new(Panel::new(id, overlay.display, mtm));
                self.panels.borrow_mut().insert(id, panel.clone());
                panel
            }
        };
        let content = content(overlay, mtm)?;
        // setContentView resizes its view to the panel's old frame (initially zero).
        let size = content.frame().size;
        panel.display.set(overlay.display);
        // Suppress checks during reconfiguration, then owe a fresh outcome for this command.
        panel.updating.set(true);
        panel.window.setContentView(Some(&content));
        panel.window.setFrame_display(
            anchor_frame(screen.visibleFrame(), size, overlay.anchor),
            true,
        );
        panel.window.orderFrontRegardless();
        panel.updating.set(false);
        self.acknowledge(&panel, Some(seq));
        Ok(())
    }

    /// Owe `panel` one outcome for command `seq` (a replay keeps the current command): publish it
    /// now if the screen already answers, otherwise let the main queue recheck at the end of the
    /// wait. The off-main deadline was armed by `configure`.
    fn acknowledge(&self, panel: &Panel, seq: Option<u64>) {
        let generation = self.shared.configure(panel.id, seq);
        panel.check(&self.shared);
        if self.shared.owes(panel.id, generation) {
            wait_for_presentation(self.key, panel.id, generation);
        }
    }

    /// The main queue's recheck at the end of the wait of generation `generation` of `id`.
    fn deadline(&self, id: OverlayId, generation: u64) {
        let panel = self.panels.borrow().get(&id).cloned();
        if let Some(panel) = panel {
            panel.deadline(&self.shared, generation);
        }
    }

    /// A new subscriber needs the current state of every panel: each owes an outcome again, with a
    /// new generation, so the timers of earlier configurations stay void.
    fn replay(&self, mtm: MainThreadMarker) {
        let configured: Vec<(OverlayId, u64)> = self
            .entries()
            .into_iter()
            .map(|entry| (entry.id, self.shared.configure(entry.id, None)))
            .collect();
        // Publishes `Unavailable` for panels whose display is gone and `Visible` or `Unavailable`
        // for those the screen already answers for.
        self.check(mtm);
        for (id, generation) in configured {
            if self.shared.owes(id, generation) {
                wait_for_presentation(self.key, id, generation);
            }
        }
    }
}

/// Let the main queue recheck a configured panel that isn't reported visible yet at the end of
/// [`PRESENTATION_WAIT`]: it publishes `Visible` if the panel got on screen, and `Unavailable`
/// otherwise. Runs on the main queue, so it can be late; the off-main deadline is what bounds the
/// wait. Never blocks.
fn wait_for_presentation(key: u64, id: OverlayId, generation: u64) {
    let deadline = move || {
        // The main queue only ever runs this on the main thread, where the registry lives.
        if let Some(host) = HOSTS.with(|hosts| hosts.borrow().get(&key).cloned()) {
            host.deadline(id, generation);
        }
    };
    match DispatchTime::try_from(PRESENTATION_WAIT) {
        Ok(when) => {
            if DispatchQueue::main().after(when, deadline).is_err() {
                tracing::warn!(
                    overlay = id.0,
                    "scheduling the overlay presentation wait failed"
                );
            }
        }
        // Unreachable for a 500 ms delay; if it ever fails, answer now rather than never.
        Err(()) => spawn_on_main(move |_| deadline()),
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let center = NSNotificationCenter::defaultCenter();
        for observer in &self.observers {
            // SAFETY: these tokens came from this center's block-based registration.
            unsafe { center.removeObserver((**observer).as_ref()) };
        }
        for panel in self.panels.get_mut().values() {
            panel.window.orderOut(None);
        }
    }
}

struct Panel {
    id: OverlayId,
    window: Retained<NSPanel>,
    display: Cell<DisplayId>,
    /// True while the panel is being reconfigured: notifications are ignored. What the panel owes
    /// (its generation, command and outcome) lives in the shared ledger, where the off-main
    /// deadline can reach it without touching AppKit.
    updating: Cell<bool>,
}

impl Panel {
    fn new(id: OverlayId, display: DisplayId, mtm: MainThreadMarker) -> Self {
        // SAFETY: standard NSPanel initializer on the main thread, using a subclass with no
        // ivars. Its inherited initializer returns a retained NSPanel; auto-release on close
        // is disabled immediately below, as required for Rust-owned windows.
        let window: Retained<OverlayPanel> = unsafe {
            msg_send![
                OverlayPanel::alloc(mtm),
                initWithContentRect: NSRect::ZERO,
                styleMask: NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
                backing: NSBackingStoreType::Buffered,
                defer: false
            ]
        };
        let window = window.into_super();
        // SAFETY: Rust's Retained owns the panel, so closing must not release it independently.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setIgnoresMouseEvents(true);
        window.setLevel(NSStatusWindowLevel);
        window.setBackgroundColor(Some(&NSColor::clearColor()));
        window.setOpaque(false);
        window.setHasShadow(true);
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        window.setHidesOnDeactivate(false);
        Self {
            id,
            window,
            display: Cell::new(display),
            updating: Cell::new(false),
        }
    }

    fn sight(&self) -> Sight {
        if !self.window.isVisible() {
            Sight::OffScreen
        } else if self
            .window
            .occlusionState()
            .contains(NSWindowOcclusionState::Visible)
        {
            Sight::Visible
        } else {
            Sight::Occluded
        }
    }

    /// A notification, a configuration or a replay: report what the screen shows.
    fn check(&self, shared: &Shared) {
        if self.updating.get() {
            return;
        }
        shared.observe(self.id, self.sight());
    }

    /// The main queue's recheck at the end of the wait of generation `generation`. The ledger
    /// ignores it if the panel was reconfigured or retired since: that configuration has its own.
    fn deadline(&self, shared: &Shared, generation: u64) {
        if self.updating.get() {
            return;
        }
        shared.recheck(self.id, generation, self.sight());
    }
}

fn screen(display: DisplayId, mtm: MainThreadMarker) -> Option<Retained<NSScreen>> {
    NSScreen::screens(mtm).into_iter().find(|screen| {
        screen
            .deviceDescription()
            .objectForKey(ns_string!("NSScreenNumber"))
            .and_then(|number| number.downcast::<NSNumber>().ok())
            .is_some_and(|number| number.unsignedIntValue() == display.0)
    })
}

fn anchor_frame(visible: NSRect, size: NSSize, anchor: OverlayAnchor) -> NSRect {
    let x = match anchor {
        OverlayAnchor::TopCenter | OverlayAnchor::Center => {
            visible.origin.x + (visible.size.width - size.width) / 2.0
        }
        OverlayAnchor::TopRight | OverlayAnchor::BottomRight => {
            visible.origin.x + visible.size.width - size.width - MARGIN
        }
    };
    let y = match anchor {
        OverlayAnchor::TopCenter | OverlayAnchor::TopRight => {
            visible.origin.y + visible.size.height - size.height - MARGIN
        }
        OverlayAnchor::BottomRight => visible.origin.y + MARGIN,
        OverlayAnchor::Center => visible.origin.y + (visible.size.height - size.height) / 2.0,
    };
    NSRect::new(NSPoint::new(x, y), size)
}

fn content(overlay: &Overlay, mtm: MainThreadMarker) -> Result<Retained<NSView>, PlatformError> {
    let label = NSTextField::labelWithString(&NSString::from_str(&overlay.text), mtm);
    label.setEditable(false);
    label.setSelectable(false);
    label.setTextColor(Some(&NSColor::whiteColor()));
    label.setFont(Some(&NSFont::systemFontOfSize(14.0)));
    label.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    label.setMaximumNumberOfLines(1);
    label.sizeToFit();
    let label_size = label.frame().size;
    let size = NSSize::new(label_size.width + 40.0, label_size.height + 20.0);
    let view = NSBox::initWithFrame(NSBox::alloc(mtm), NSRect::new(NSPoint::ZERO, size));
    view.setBoxType(NSBoxType::Custom);
    view.setTitlePosition(NSTitlePosition::NoTitle);
    view.setBorderWidth(0.0);
    view.setCornerRadius(10.0);
    view.setContentViewMargins(NSSize::ZERO);
    view.setFillColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
        31.0 / 255.0,
        41.0 / 255.0,
        55.0 / 255.0,
        0.9,
    ));
    view.setWantsLayer(true);
    if view.layer().is_none() {
        return Err(PlatformError::Backend(
            "overlay backing layer unavailable".into(),
        ));
    }
    let bar = NSBox::initWithFrame(
        NSBox::alloc(mtm),
        NSRect::new(NSPoint::new(8.0, 8.0), NSSize::new(4.0, size.height - 16.0)),
    );
    bar.setBoxType(NSBoxType::Custom);
    bar.setTitlePosition(NSTitlePosition::NoTitle);
    bar.setBorderWidth(0.0);
    bar.setCornerRadius(0.0);
    bar.setFillColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
        f64::from(overlay.accent.r) / 255.0,
        f64::from(overlay.accent.g) / 255.0,
        f64::from(overlay.accent.b) / 255.0,
        1.0,
    ));
    bar.setWantsLayer(true);
    label.setFrame(NSRect::new(NSPoint::new(24.0, 10.0), label_size));
    view.addSubview(&bar);
    view.addSubview(&label);
    Ok(view.into_super())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::{Barrier, mpsc};
    use std::time::Instant;

    use crosspane_platform::Rgb8;

    use super::*;

    #[test]
    fn anchors_use_visible_frame_and_24_point_margin() {
        for origin in [NSPoint::ZERO, NSPoint::new(-1200.0, 200.0)] {
            let visible = NSRect::new(origin, NSSize::new(1200.0, 800.0));
            let size = NSSize::new(200.0, 40.0);
            for (anchor, x, y) in [
                (OverlayAnchor::TopCenter, 500.0, 736.0),
                (OverlayAnchor::TopRight, 976.0, 736.0),
                (OverlayAnchor::BottomRight, 976.0, 24.0),
                (OverlayAnchor::Center, 500.0, 380.0),
            ] {
                assert_eq!(
                    anchor_frame(visible, size, anchor),
                    NSRect::new(NSPoint::new(origin.x + x, origin.y + y), size)
                );
            }
        }
    }

    // The sequencing rules, without any threads.

    const A: OverlayId = OverlayId(1);
    const B: OverlayId = OverlayId(2);

    /// What the main thread would publish if it ran commands `runs` (sequence numbers, in queue
    /// order) for `id`, each presenting successfully.
    fn visible_events(sequencer: &Sequencer, id: OverlayId, runs: &[u64]) -> Vec<OverlayEvent> {
        runs.iter()
            .filter(|&&seq| sequencer.should_run(id, seq))
            .map(|&seq| (seq, OverlayEvent::Visible(id)))
            .filter(|&(seq, event)| sequencer.admits(seq, event))
            .map(|(_, event)| event)
            .collect()
    }

    #[test]
    fn sequence_numbers_increase_per_id() {
        let mut sequencer = Sequencer::default();
        assert_eq!(sequencer.issue(A), 1);
        assert_eq!(sequencer.issue(A), 2);
        assert_eq!(sequencer.issue(B), 1);
        assert_eq!(sequencer.issue(A), 3);
    }

    #[test]
    fn show_hide_show_runs_only_the_last_show() {
        let mut sequencer = Sequencer::default();
        let show1 = sequencer.issue(A);
        let hide = sequencer.issue(A);
        let show2 = sequencer.issue(A);
        assert!(!sequencer.should_run(A, show1));
        assert!(!sequencer.should_run(A, hide));
        assert!(sequencer.should_run(A, show2));
        assert_eq!(
            visible_events(&sequencer, A, &[show1, hide, show2]),
            [OverlayEvent::Visible(A)]
        );
        // A Visible that an older command raised late is not published either.
        assert!(!sequencer.admits(show1, OverlayEvent::Visible(A)));
        assert!(sequencer.admits(show2, OverlayEvent::Visible(A)));
    }

    #[test]
    fn stale_show_failure_after_a_newer_show_publishes_nothing() {
        let mut sequencer = Sequencer::default();
        let stale = sequencer.issue(A);
        let newest = sequencer.issue(A);
        assert_eq!(sequencer.failure_event(Kind::Show, A, stale), None);
        assert_eq!(
            sequencer.failure_event(Kind::Show, A, newest),
            Some(OverlayEvent::Unavailable(A))
        );
    }

    #[test]
    fn hide_failure_publishes_nothing() {
        let mut sequencer = Sequencer::default();
        let hide = sequencer.issue(A);
        assert_eq!(sequencer.failure_event(Kind::Hide, A, hide), None);
    }

    #[test]
    fn interleaved_ids_are_independent() {
        let mut sequencer = Sequencer::default();
        let a1 = sequencer.issue(A);
        let b1 = sequencer.issue(B);
        let a2 = sequencer.issue(A);
        // B's only command is not superseded by A's second one, and vice versa.
        assert!(!sequencer.should_run(A, a1));
        assert!(sequencer.should_run(B, b1));
        assert!(sequencer.should_run(A, a2));
        assert_eq!(
            sequencer.failure_event(Kind::Show, B, b1),
            Some(OverlayEvent::Unavailable(B))
        );
        assert_eq!(sequencer.failure_event(Kind::Show, A, a1), None);
        // A number for one id never matches another id's newest command.
        assert!(!sequencer.admits(a2, OverlayEvent::Visible(B)));
    }

    #[test]
    fn unknown_ids_have_no_newest_command() {
        let sequencer = Sequencer::default();
        assert!(!sequencer.should_run(A, 0));
        assert!(!sequencer.admits(1, OverlayEvent::Visible(A)));
    }

    // The acknowledgement rule: one first outcome per configuration, then transitions.

    #[test]
    fn a_panel_visible_at_configuration_answers_visible_once() {
        let mut presence = Presence::default();
        let generation = presence.configure(Some(1));
        assert!(presence.owes(generation));
        assert_eq!(presence.observe(Sight::Visible), Some(Outcome::Visible));
        assert!(!presence.owes(generation));
        assert_eq!(presence.observe(Sight::Visible), None);
        assert_eq!(presence.recheck(generation, Sight::Visible), None);
        assert_eq!(presence.expire(generation), None);
    }

    #[test]
    fn a_panel_not_on_screen_answers_unavailable_at_once() {
        let mut presence = Presence::default();
        let generation = presence.configure(Some(1));
        assert_eq!(
            presence.observe(Sight::OffScreen),
            Some(Outcome::Unavailable)
        );
        assert!(!presence.owes(generation));
        assert_eq!(presence.recheck(generation, Sight::OffScreen), None);
        assert_eq!(presence.expire(generation), None);
    }

    #[test]
    fn an_occluded_panel_waits_then_answers_visible_when_presented() {
        let mut presence = Presence::default();
        let generation = presence.configure(Some(1));
        assert_eq!(presence.observe(Sight::Occluded), None);
        assert!(presence.owes(generation));
        assert_eq!(presence.observe(Sight::Visible), Some(Outcome::Visible));
        // Either deadline ending afterwards adds nothing.
        assert_eq!(presence.recheck(generation, Sight::Visible), None);
        assert_eq!(presence.expire(generation), None);
    }

    #[test]
    fn a_panel_never_presented_answers_unavailable_when_the_wait_runs_out() {
        let mut presence = Presence::default();
        let generation = presence.configure(Some(1));
        assert_eq!(presence.observe(Sight::Occluded), None);
        assert_eq!(
            presence.recheck(generation, Sight::Occluded),
            Some(Outcome::Unavailable)
        );
        assert!(!presence.owes(generation));
        // Exactly one outcome: later notifications while it stays hidden add nothing...
        assert_eq!(presence.observe(Sight::Occluded), None);
        assert_eq!(presence.recheck(generation, Sight::Occluded), None);
        assert_eq!(presence.expire(generation), None);
        // ...and a late presentation is a transition.
        assert_eq!(presence.observe(Sight::Visible), Some(Outcome::Visible));
    }

    #[test]
    fn the_main_queue_recheck_reports_what_the_screen_shows_by_then() {
        let mut presence = Presence::default();
        let generation = presence.configure(Some(1));
        assert_eq!(
            presence.recheck(generation, Sight::Visible),
            Some(Outcome::Visible)
        );
        let mut presence = Presence::default();
        let generation = presence.configure(Some(1));
        assert_eq!(
            presence.recheck(generation, Sight::OffScreen),
            Some(Outcome::Unavailable)
        );
    }

    #[test]
    fn the_off_main_deadline_declares_unavailable_without_seeing_the_screen() {
        let mut presence = Presence::default();
        let generation = presence.configure(Some(1));
        assert_eq!(presence.observe(Sight::Occluded), None);
        assert_eq!(presence.expire(generation), Some(Outcome::Unavailable));
        // One first outcome: neither the deadline again nor a main-queue recheck adds another.
        assert_eq!(presence.expire(generation), None);
        assert_eq!(presence.recheck(generation, Sight::Occluded), None);
        // A panel the main queue later finds presented is an ordinary transition.
        assert_eq!(presence.observe(Sight::Visible), Some(Outcome::Visible));
        assert_eq!(
            presence.observe(Sight::Occluded),
            Some(Outcome::Unavailable)
        );
    }

    #[test]
    fn whichever_path_claims_the_first_outcome_publishes_it_and_the_other_does_not() {
        // Off-main deadline first, then the main-queue recheck of a covered panel.
        let mut presence = Presence::default();
        let generation = presence.configure(Some(1));
        assert_eq!(presence.expire(generation), Some(Outcome::Unavailable));
        assert_eq!(presence.recheck(generation, Sight::Occluded), None);

        // Main-queue recheck first, then the off-main deadline.
        let mut presence = Presence::default();
        let generation = presence.configure(Some(1));
        assert_eq!(
            presence.recheck(generation, Sight::Occluded),
            Some(Outcome::Unavailable)
        );
        assert_eq!(presence.expire(generation), None);

        // The main queue got there first and saw the panel: Visible is the one outcome.
        let mut presence = Presence::default();
        let generation = presence.configure(Some(1));
        assert_eq!(
            presence.recheck(generation, Sight::Visible),
            Some(Outcome::Visible)
        );
        assert_eq!(presence.expire(generation), None);

        // The deadline got there first; the panel that the main queue then sees is a transition.
        let mut presence = Presence::default();
        let generation = presence.configure(Some(1));
        assert_eq!(presence.expire(generation), Some(Outcome::Unavailable));
        assert_eq!(
            presence.recheck(generation, Sight::Visible),
            Some(Outcome::Visible)
        );
    }

    #[test]
    fn after_the_outcome_changes_publish_as_transitions() {
        let mut presence = Presence::default();
        presence.configure(Some(1));
        assert_eq!(presence.observe(Sight::Visible), Some(Outcome::Visible));
        assert_eq!(
            presence.observe(Sight::Occluded),
            Some(Outcome::Unavailable)
        );
        assert_eq!(presence.observe(Sight::Occluded), None);
        assert_eq!(presence.observe(Sight::Visible), Some(Outcome::Visible));
        assert_eq!(
            presence.observe(Sight::OffScreen),
            Some(Outcome::Unavailable)
        );
    }

    #[test]
    fn every_configuration_owes_its_own_outcome() {
        let mut presence = Presence::default();
        presence.configure(Some(1));
        assert_eq!(presence.observe(Sight::Visible), Some(Outcome::Visible));
        // A second show of an already visible panel acknowledges again.
        presence.configure(Some(2));
        assert_eq!(presence.observe(Sight::Visible), Some(Outcome::Visible));
        // A configuration that finds the panel covered owes Unavailable, even though the panel
        // was visible before and no transition is seen.
        let generation = presence.configure(Some(3));
        assert_eq!(presence.observe(Sight::Occluded), None);
        assert_eq!(presence.expire(generation), Some(Outcome::Unavailable));
    }

    #[test]
    fn every_configuration_and_retirement_moves_the_generation_on() {
        let mut presence = Presence::default();
        let first = presence.configure(Some(1));
        let second = presence.configure(None);
        assert!(second > first);
        presence.retire();
        let third = presence.configure(None);
        assert!(third > second + 1);
        // A replay keeps the command the panel answers for.
        assert_eq!(presence.seq, 1);
    }

    #[test]
    fn a_timer_of_an_older_generation_does_nothing() {
        // Show at t=0, replay at t=450 ms: the show's timer fires at t=500 ms and must not answer
        // for the replay's fresh wait.
        let mut presence = Presence::default();
        let show = presence.configure(Some(1));
        assert_eq!(presence.observe(Sight::Occluded), None);
        let replay = presence.configure(None);
        assert_eq!(presence.observe(Sight::Occluded), None);
        assert_eq!(presence.expire(show), None);
        assert_eq!(presence.recheck(show, Sight::Occluded), None);
        // Not even a sight of the panel is taken from a stale timer.
        assert_eq!(presence.recheck(show, Sight::Visible), None);
        assert!(presence.owes(replay));
        assert_eq!(presence.expire(replay), Some(Outcome::Unavailable));
    }

    #[test]
    fn a_retired_panels_timers_are_void() {
        let mut presence = Presence::default();
        let generation = presence.configure(Some(1));
        presence.retire();
        assert!(!presence.owes(generation));
        assert_eq!(presence.expire(generation), None);
        assert_eq!(presence.recheck(generation, Sight::Occluded), None);
    }

    // What the ledger lets out. A configuration's `Visible` and its transitions speak for its own
    // command; a deadline's `Unavailable` speaks for the newest command issued.

    #[test]
    fn the_deadline_answers_for_the_newest_command_issued_meanwhile() {
        let mut ledger = Ledger::default();
        let first = ledger.sequencer.issue(A);
        let generation = ledger.configure(A, Some(first));
        // Shows queued behind a stalled main queue can't configure anything or arm a deadline.
        // They must not silence the one that is armed.
        ledger.sequencer.issue(A);
        ledger.sequencer.issue(A);
        assert_eq!(
            ledger.expire(A, generation),
            Some(OverlayEvent::Unavailable(A))
        );
        assert!(!ledger.owes(A, generation));
        // When the newest queued show finally runs, it owes a fresh outcome of its own.
        let newest = ledger.sequencer.newest(A).unwrap();
        let generation = ledger.configure(A, Some(newest));
        assert!(ledger.owes(A, generation));
        assert_eq!(
            ledger.observe(A, Sight::Visible),
            Some(OverlayEvent::Visible(A))
        );
    }

    #[test]
    fn the_main_queue_recheck_answers_for_the_newest_command_too() {
        let mut ledger = Ledger::default();
        let first = ledger.sequencer.issue(A);
        let generation = ledger.configure(A, Some(first));
        ledger.sequencer.issue(A);
        assert_eq!(
            ledger.recheck(A, generation, Sight::Occluded),
            Some(OverlayEvent::Unavailable(A))
        );
        // Once: the off-main deadline of the same configuration adds nothing.
        assert_eq!(ledger.expire(A, generation), None);
    }

    #[test]
    fn a_visible_or_a_transition_still_speaks_for_the_configurations_own_command() {
        let mut ledger = Ledger::default();
        let first = ledger.sequencer.issue(A);
        ledger.configure(A, Some(first));
        // A newer command is queued; the panel is presented for the configuration before it. The
        // queued show will configure and answer for itself, so this isn't published.
        let second = ledger.sequencer.issue(A);
        assert_eq!(ledger.observe(A, Sight::Visible), None);
        let generation = ledger.configure(A, Some(second));
        assert_eq!(
            ledger.observe(A, Sight::Visible),
            Some(OverlayEvent::Visible(A))
        );
        // The same for a transition, and for the recheck's Visible.
        ledger.sequencer.issue(A);
        assert_eq!(ledger.observe(A, Sight::Occluded), None);
        assert_eq!(ledger.recheck(A, generation, Sight::Visible), None);
    }

    #[test]
    fn a_deadline_whose_configuration_has_moved_on_answers_nothing() {
        let mut ledger = Ledger::default();
        let first = ledger.sequencer.issue(A);
        let stale = ledger.configure(A, Some(first));
        let second = ledger.sequencer.issue(A);
        ledger.configure(A, Some(second));
        // Even with a newer command to speak for, a deadline of an older generation is void.
        assert_eq!(ledger.expire(A, stale), None);
        assert_eq!(ledger.recheck(A, stale, Sight::Occluded), None);
    }

    #[test]
    fn a_replay_answers_for_the_command_the_panel_already_shows() {
        let mut ledger = Ledger::default();
        let seq = ledger.sequencer.issue(A);
        ledger.configure(A, Some(seq));
        let replay = ledger.configure(A, None);
        assert_eq!(ledger.expire(A, replay), Some(OverlayEvent::Unavailable(A)));
    }

    #[test]
    fn a_gone_display_answers_once_and_voids_the_timers() {
        // Removal first, then the deadline: one Unavailable, and the timer is void.
        let mut ledger = Ledger::default();
        let seq = ledger.sequencer.issue(A);
        let generation = ledger.configure(A, Some(seq));
        assert_eq!(ledger.gone(A), Some(OverlayEvent::Unavailable(A)));
        assert_eq!(ledger.expire(A, generation), None);
        assert_eq!(ledger.recheck(A, generation, Sight::Occluded), None);
    }

    #[test]
    fn a_display_removed_after_the_deadline_answered_adds_no_second_unavailable() {
        // The deadline first, then removal before the engine has processed the answer.
        let mut ledger = Ledger::default();
        let seq = ledger.sequencer.issue(A);
        let generation = ledger.configure(A, Some(seq));
        assert_eq!(
            ledger.expire(A, generation),
            Some(OverlayEvent::Unavailable(A))
        );
        assert_eq!(ledger.gone(A), None);
        // And the panel is retired all the same.
        assert!(!ledger.owes(A, generation));
        assert_eq!(ledger.gone(A), None);
    }

    #[test]
    fn a_display_removed_after_a_main_queue_unavailable_adds_no_second_one() {
        let mut ledger = Ledger::default();
        let seq = ledger.sequencer.issue(A);
        let generation = ledger.configure(A, Some(seq));
        assert_eq!(
            ledger.recheck(A, generation, Sight::OffScreen),
            Some(OverlayEvent::Unavailable(A))
        );
        assert_eq!(ledger.gone(A), None);
    }

    #[test]
    fn a_visible_panels_display_removal_answers_unavailable_once() {
        let mut ledger = Ledger::default();
        let seq = ledger.sequencer.issue(A);
        ledger.configure(A, Some(seq));
        assert_eq!(
            ledger.observe(A, Sight::Visible),
            Some(OverlayEvent::Visible(A))
        );
        assert_eq!(ledger.gone(A), Some(OverlayEvent::Unavailable(A)));
        assert_eq!(ledger.gone(A), None);
    }

    #[test]
    fn a_panel_that_already_went_unavailable_by_transition_adds_nothing_on_removal() {
        let mut ledger = Ledger::default();
        let seq = ledger.sequencer.issue(A);
        ledger.configure(A, Some(seq));
        ledger.observe(A, Sight::Visible);
        assert_eq!(
            ledger.observe(A, Sight::Occluded),
            Some(OverlayEvent::Unavailable(A))
        );
        assert_eq!(ledger.gone(A), None);
    }

    #[test]
    fn a_panel_presented_after_the_deadline_answers_unavailable_on_removal() {
        // The deadline answered Unavailable; the panel then got on screen (a transition Visible);
        // removing its display now is a real loss and is reported.
        let mut ledger = Ledger::default();
        let seq = ledger.sequencer.issue(A);
        let generation = ledger.configure(A, Some(seq));
        ledger.expire(A, generation);
        assert_eq!(
            ledger.observe(A, Sight::Visible),
            Some(OverlayEvent::Visible(A))
        );
        assert_eq!(ledger.gone(A), Some(OverlayEvent::Unavailable(A)));
    }

    #[test]
    fn a_never_configured_panel_reports_nothing_when_its_display_goes() {
        let mut ledger = Ledger::default();
        ledger.sequencer.issue(A);
        assert_eq!(ledger.gone(A), None);
    }

    #[test]
    fn ids_keep_their_own_generations_and_outcomes() {
        let mut ledger = Ledger::default();
        let a = ledger.sequencer.issue(A);
        let b = ledger.sequencer.issue(B);
        let a_generation = ledger.configure(A, Some(a));
        let b_generation = ledger.configure(B, Some(b));
        ledger.retire(A);
        assert_eq!(ledger.expire(A, a_generation), None);
        assert_eq!(
            ledger.expire(B, b_generation),
            Some(OverlayEvent::Unavailable(B))
        );
    }

    // The command handle against a main thread the tests control.

    type Work = Box<dyn FnOnce() + Send>;
    type ShowHook = Box<dyn FnMut(OverlayId, u64) -> Result<(), PlatformError> + Send>;

    /// Timers the tests fire by hand, in the order they were armed. A real wait is never slept
    /// through; a test that wants the clock uses [`GlobalQueueTimers`].
    #[derive(Default)]
    struct ManualTimers {
        armed: Mutex<Vec<(Duration, Option<Work>)>>,
    }

    impl Scheduler for ManualTimers {
        fn after(&self, delay: Duration, work: Work) {
            self.armed.lock().unwrap().push((delay, Some(work)));
        }
    }

    impl ManualTimers {
        fn armed(&self) -> usize {
            self.armed.lock().unwrap().len()
        }

        fn delay(&self, index: usize) -> Duration {
            self.armed.lock().unwrap()[index].0
        }

        /// Fire the `index`th timer armed (counting from 0), on the calling thread, once.
        fn fire(&self, index: usize) {
            let work = self.armed.lock().unwrap()[index]
                .1
                .take()
                .expect("a timer fires once");
            work();
        }
    }

    /// A stand-in for AppKit with its own "main thread" that runs queued work in order. Tests
    /// stall it with [`FakeBackend::block`], which is how the real main queue behaves during a
    /// window-resize drag. Panels follow the same acknowledgement rule as the real ones, because
    /// they use the same shared state (including its off-main deadline), over a screen the tests
    /// control with [`FakeBackend::set_sight`].
    struct FakeBackend {
        queue: mpsc::Sender<Work>,
        shared: Arc<Shared>,
        log: Mutex<Vec<String>>,
        show_hook: Mutex<Option<ShowHook>>,
        panels: Mutex<BTreeSet<OverlayId>>,
        sights: Mutex<BTreeMap<OverlayId, Sight>>,
        generations: Mutex<BTreeMap<OverlayId, Vec<u64>>>,
        fail_hide: AtomicBool,
        closed: AtomicBool,
    }

    impl FakeBackend {
        fn new(shared: Arc<Shared>) -> Arc<Self> {
            let (queue, work) = mpsc::channel::<Work>();
            std::thread::spawn(move || {
                for work in work {
                    work();
                }
            });
            Arc::new(Self {
                queue,
                shared,
                log: Mutex::new(Vec::new()),
                show_hook: Mutex::new(None),
                panels: Mutex::new(BTreeSet::new()),
                sights: Mutex::new(BTreeMap::new()),
                generations: Mutex::new(BTreeMap::new()),
                fail_hide: AtomicBool::new(false),
                closed: AtomicBool::new(false),
            })
        }

        fn sight(&self, id: OverlayId) -> Sight {
            *self
                .sights
                .lock()
                .unwrap()
                .get(&id)
                .unwrap_or(&Sight::Visible)
        }

        /// The screen changes under the panel of `id`, and AppKit notifies. Delivered on the
        /// calling thread, so a test can deliver one while the fake main thread is stalled; the
        /// panel then speaks for the command it was last configured by.
        fn set_sight(&self, id: OverlayId, sight: Sight) {
            self.sights.lock().unwrap().insert(id, sight);
            if self.panels.lock().unwrap().contains(&id) {
                self.shared.observe(id, sight);
            }
        }

        /// Every generation configured for `id` so far (shows and replays), oldest first.
        fn generations(&self, id: OverlayId) -> Vec<u64> {
            self.generations
                .lock()
                .unwrap()
                .get(&id)
                .cloned()
                .unwrap_or_default()
        }

        fn latest_generation(&self, id: OverlayId) -> u64 {
            *self.generations(id).last().expect("a configured panel")
        }

        fn configured(&self, id: OverlayId, generation: u64) {
            self.generations
                .lock()
                .unwrap()
                .entry(id)
                .or_default()
                .push(generation);
        }

        /// The main queue's recheck at the end of the wait armed for `generation` of `id`, run on
        /// the calling thread. A test passes an old generation to fire a stale timer.
        fn recheck(&self, id: OverlayId, generation: u64) {
            self.shared.recheck(id, generation, self.sight(id));
        }

        /// The same recheck, queued behind whatever the fake main thread is doing.
        fn recheck_on_main(self: &Arc<Self>, id: OverlayId, generation: u64) {
            let backend = self.clone();
            self.queue
                .send(Box::new(move || backend.recheck(id, generation)))
                .unwrap();
        }

        fn note(&self, line: String) {
            self.log.lock().unwrap().push(line);
        }

        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }

        /// Keep the main thread busy for `duration`, like a native tracking loop.
        fn block(&self, duration: Duration) {
            self.queue
                .send(Box::new(move || std::thread::sleep(duration)))
                .unwrap();
        }

        /// Wait until everything queued so far has run.
        fn drain(&self) {
            let (done, wait) = mpsc::channel();
            self.queue
                .send(Box::new(move || done.send(()).unwrap()))
                .unwrap();
            wait.recv_timeout(Duration::from_secs(5))
                .expect("fake main thread drained");
        }
    }

    impl Backend for FakeBackend {
        fn spawn(&self, work: Work) {
            self.queue.send(work).unwrap();
        }

        fn show_panel(
            &self,
            id: OverlayId,
            _overlay: &Overlay,
            seq: u64,
        ) -> Result<(), PlatformError> {
            self.note(format!("show {} #{seq}", id.0));
            if let Some(hook) = self.show_hook.lock().unwrap().as_mut() {
                hook(id, seq)?;
            }
            // Configure the panel and owe the outcome for this command, as the real one does:
            // this arms the off-main deadline too.
            let generation = self.shared.configure(id, Some(seq));
            self.configured(id, generation);
            self.panels.lock().unwrap().insert(id);
            self.shared.observe(id, self.sight(id));
            Ok(())
        }

        fn hide_panel(&self, id: OverlayId) -> Result<(), PlatformError> {
            self.note(format!("hide {}", id.0));
            if self.fail_hide.load(Ordering::Acquire) {
                return Err(PlatformError::Backend("fake hide failure".into()));
            }
            self.panels.lock().unwrap().remove(&id);
            self.shared.retire(id);
            Ok(())
        }

        fn replay(&self) {
            let ids: Vec<OverlayId> = self.panels.lock().unwrap().iter().copied().collect();
            for id in ids {
                let generation = self.shared.configure(id, None);
                self.configured(id, generation);
                self.shared.observe(id, self.sight(id));
            }
        }

        fn close(&self) {
            self.closed.store(true, Ordering::Release);
        }
    }

    struct Rig {
        overlay: MacOverlay,
        backend: Arc<FakeBackend>,
        shared: Arc<Shared>,
        events: mpsc::Receiver<OverlayEvent>,
        sink_tx: mpsc::Sender<OverlayEvent>,
        /// The off-main deadlines, when the rig runs on hand-fired ones; empty with a real clock.
        timers: Arc<ManualTimers>,
    }

    impl Rig {
        fn build(scheduler: Arc<dyn Scheduler>, timers: Arc<ManualTimers>) -> Self {
            let shared = Arc::new(Shared::new(scheduler));
            let (sink_tx, events) = mpsc::channel();
            let backend = FakeBackend::new(shared.clone());
            let overlay = MacOverlay {
                key: 0,
                shared: shared.clone(),
                backend: backend.clone(),
            };
            Self {
                overlay,
                backend,
                shared,
                events,
                sink_tx,
                timers,
            }
        }

        /// A handle nobody has subscribed to yet, whose deadlines the test fires by hand.
        fn bare() -> Self {
            let timers = Arc::new(ManualTimers::default());
            Self::build(timers.clone(), timers)
        }

        /// A handle nobody has subscribed to yet, on the production deadlines and the real clock.
        fn bare_on_the_clock() -> Self {
            Self::build(Arc::new(GlobalQueueTimers), Arc::default())
        }

        fn subscribe(&mut self) -> Result<(), PlatformError> {
            let tx = self.sink_tx.clone();
            self.overlay.subscribe(Arc::new(move |event| {
                let _ = tx.send(event);
            }))
        }

        fn new() -> Self {
            let mut rig = Self::bare();
            rig.subscribe().unwrap();
            rig
        }

        /// Let the fake main thread finish, then take the events it published.
        fn settle(&self) -> Vec<OverlayEvent> {
            self.backend.drain();
            self.events.try_iter().collect()
        }
    }

    fn overlay_on(display: u32) -> Overlay {
        Overlay {
            display: DisplayId(display),
            anchor: OverlayAnchor::TopCenter,
            text: "Controlled from test".into(),
            accent: Rgb8 {
                r: 59,
                g: 130,
                b: 246,
            },
        }
    }

    fn timed<R>(call: impl FnOnce() -> R) -> (R, Duration) {
        let start = Instant::now();
        let result = call();
        (result, start.elapsed())
    }

    #[test]
    fn show_and_hide_return_while_the_main_queue_is_blocked() {
        let mut rig = Rig::new();
        rig.backend.block(Duration::from_millis(200));
        let limit = Duration::from_millis(5);
        let (shown, show_time) = timed(|| rig.overlay.show(A, &overlay_on(1)));
        shown.unwrap();
        let (other, other_time) = timed(|| rig.overlay.show(B, &overlay_on(1)));
        other.unwrap();
        let (hidden, hide_time) = timed(|| rig.overlay.hide(A));
        hidden.unwrap();
        for (call, time) in [
            ("show", show_time),
            ("show of another id", other_time),
            ("hide", hide_time),
        ] {
            assert!(
                time < limit,
                "{call} took {time:?} with the main queue blocked"
            );
        }
        assert!(
            rig.backend.log().is_empty(),
            "the main queue was still blocked, so nothing has run"
        );
        // Once it runs, the burst coalesces: A's show is superseded by A's hide.
        assert_eq!(rig.settle(), [OverlayEvent::Visible(B)]);
        assert_eq!(rig.backend.log(), ["show 2 #1", "hide 1"]);
    }

    /// The real handle and the real main queue. A libtest process never services the main queue
    /// (nothing runs its run loop), which is the stalled-queue case in its strongest form: the
    /// queued work doesn't run at all, so no AppKit object is touched and no panel can appear.
    #[test]
    fn the_real_handle_never_waits_for_the_main_queue() {
        let mut overlay = MacOverlay::new().unwrap();
        let limit = Duration::from_millis(5);
        let (subscribed, subscribe_time) = timed(|| overlay.subscribe(Arc::new(|_| {})));
        subscribed.unwrap();
        let (shown, show_time) = timed(|| overlay.show(A, &overlay_on(1)));
        shown.unwrap();
        let (hidden, hide_time) = timed(|| overlay.hide(A));
        hidden.unwrap();
        assert!(subscribe_time < limit, "subscribe took {subscribe_time:?}");
        assert!(show_time < limit, "show took {show_time:?}");
        assert!(hide_time < limit, "hide took {hide_time:?}");
    }

    #[test]
    fn subscribe_returns_while_the_main_queue_is_blocked_and_replays_the_current_state() {
        let mut rig = Rig::bare();
        // A panel is already up when the subscriber arrives; nobody heard its Visible.
        rig.overlay.show(A, &overlay_on(1)).unwrap();
        rig.backend.drain();
        assert!(rig.events.try_recv().is_err());
        rig.backend.block(Duration::from_millis(200));
        let (subscribed, time) = timed(|| rig.subscribe());
        subscribed.unwrap();
        assert!(
            time < Duration::from_millis(5),
            "subscribe took {time:?} with the main queue blocked"
        );
        assert!(rig.events.try_recv().is_err(), "the replay waits its turn");
        assert_eq!(rig.settle(), [OverlayEvent::Visible(A)]);
        // Subscribing twice is still an error, answered at once.
        assert!(matches!(rig.subscribe(), Err(PlatformError::Backend(_))));
    }

    #[test]
    fn show_hide_show_publishes_only_the_last_visible() {
        let mut rig = Rig::new();
        rig.backend.block(Duration::from_millis(50));
        rig.overlay.show(A, &overlay_on(1)).unwrap();
        rig.overlay.hide(A).unwrap();
        rig.overlay.show(A, &overlay_on(1)).unwrap();
        assert_eq!(rig.settle(), [OverlayEvent::Visible(A)]);
        assert_eq!(rig.backend.log(), ["show 1 #3"]);
    }

    #[test]
    fn a_stale_show_failure_after_a_newer_show_does_not_cancel_it() {
        let mut rig = Rig::new();
        let (entered_tx, entered) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        *rig.backend.show_hook.lock().unwrap() = Some(Box::new(move |_, seq| {
            if seq == 1 {
                // The first show is mid-flight on the main thread when the next one arrives.
                entered_tx.send(()).unwrap();
                released.recv().unwrap();
                return Err(PlatformError::Backend("first show failed".into()));
            }
            Ok(())
        }));
        rig.overlay.show(A, &overlay_on(1)).unwrap();
        entered
            .recv_timeout(Duration::from_secs(5))
            .expect("first show started");
        rig.overlay.show(A, &overlay_on(1)).unwrap();
        release.send(()).unwrap();
        // No Unavailable from the stale failure; only the newer show's Visible.
        assert_eq!(rig.settle(), [OverlayEvent::Visible(A)]);
    }

    #[test]
    fn the_newest_show_failing_publishes_unavailable() {
        let mut rig = Rig::new();
        *rig.backend.show_hook.lock().unwrap() =
            Some(Box::new(|_, _| Err(PlatformError::NotFound)));
        rig.overlay.show(A, &overlay_on(7)).unwrap();
        assert_eq!(rig.settle(), [OverlayEvent::Unavailable(A)]);
        // The failed show's leftovers are removed.
        assert_eq!(rig.backend.log(), ["show 1 #1", "hide 1"]);
    }

    #[test]
    fn a_hide_failure_publishes_nothing() {
        let mut rig = Rig::new();
        rig.overlay.show(A, &overlay_on(1)).unwrap();
        assert_eq!(rig.settle(), [OverlayEvent::Visible(A)]);
        rig.backend.fail_hide.store(true, Ordering::Release);
        rig.overlay.hide(A).unwrap();
        assert!(rig.settle().is_empty(), "a failed hide publishes no event");
        assert_eq!(rig.backend.log(), ["show 1 #1", "hide 1"]);
    }

    #[test]
    fn interleaved_ids_do_not_coalesce_each_other() {
        let mut rig = Rig::new();
        rig.backend.block(Duration::from_millis(50));
        rig.overlay.show(A, &overlay_on(1)).unwrap();
        rig.overlay.show(B, &overlay_on(1)).unwrap();
        rig.overlay.show(A, &overlay_on(1)).unwrap();
        assert_eq!(
            rig.settle(),
            [OverlayEvent::Visible(B), OverlayEvent::Visible(A)]
        );
        assert_eq!(rig.backend.log(), ["show 2 #1", "show 1 #2"]);
    }

    #[test]
    fn late_events_of_a_superseded_command_are_dropped() {
        let rig = Rig::new();
        let first = rig.shared.issue(A);
        rig.shared.configure(A, Some(first));
        rig.shared.observe(A, Sight::Visible);
        let second = rig.shared.issue(A);
        // A notification for the first command's panel arrives after the second was issued.
        rig.shared.observe(A, Sight::Occluded);
        rig.shared.configure(A, Some(second));
        rig.shared.observe(A, Sight::Visible);
        assert_eq!(
            rig.settle(),
            [OverlayEvent::Visible(A), OverlayEvent::Visible(A)]
        );
    }

    #[test]
    fn synchronous_preconditions_fail_without_queueing_or_events() {
        let mut rig = Rig::new();
        assert!(matches!(
            rig.overlay.show(A, &overlay_on(0)),
            Err(PlatformError::NotFound)
        ));
        rig.shared.alive.store(false, Ordering::Release);
        assert!(matches!(
            rig.overlay.show(A, &overlay_on(1)),
            Err(PlatformError::Backend(_))
        ));
        assert!(matches!(
            rig.overlay.hide(A),
            Err(PlatformError::Backend(_))
        ));
        assert!(rig.settle().is_empty());
        assert!(rig.backend.log().is_empty());
    }

    /// An active capture's HUD is re-shown (a target switch). The panel gets covered after that
    /// command is issued and stays covered when it runs: the old command's notification must not
    /// speak, and the newest command must not stay silent.
    #[test]
    fn a_newest_show_whose_panel_stays_invisible_answers_unavailable() {
        let mut rig = Rig::new();
        rig.overlay.show(A, &overlay_on(1)).unwrap();
        assert_eq!(rig.settle(), [OverlayEvent::Visible(A)]);

        rig.backend.block(Duration::from_millis(100));
        rig.overlay.show(A, &overlay_on(1)).unwrap();
        // The panel is covered while the newest show waits for the main thread. That notification
        // belongs to the first command, which the newest one has superseded.
        rig.backend.set_sight(A, Sight::Occluded);
        assert!(
            rig.events.try_recv().is_err(),
            "the old command's Unavailable is suppressed"
        );

        // The main thread catches up: the newest show configures a panel that is still covered.
        assert!(
            rig.settle().is_empty(),
            "the panel may still be presented, so the wait is on"
        );
        assert_eq!(rig.timers.armed(), 2, "each configuration arms a deadline");
        assert_eq!(rig.timers.delay(1), PRESENTATION_WAIT);
        let generations = rig.backend.generations(A);
        // It never is: the wait runs out, and the newest command answers.
        rig.timers.fire(1);
        assert_eq!(rig.settle(), [OverlayEvent::Unavailable(A)]);
        // Exactly once: not again from the main queue's recheck, nor from the first show's
        // deadline or recheck, which belong to a configuration that is long gone.
        rig.backend.recheck(A, generations[1]);
        rig.timers.fire(0);
        rig.backend.recheck(A, generations[0]);
        assert!(rig.settle().is_empty());
    }

    /// The bound must hold when the main queue is busy: the off-main deadline answers while the
    /// main thread is still stuck, once, and the main queue's late recheck adds nothing.
    #[test]
    fn the_bound_holds_while_the_main_queue_is_stalled() {
        let mut rig = Rig::bare_on_the_clock();
        rig.subscribe().unwrap();
        rig.backend.set_sight(A, Sight::Occluded);
        rig.overlay.show(A, &overlay_on(1)).unwrap();
        assert!(rig.settle().is_empty(), "configured, and waiting");
        let generation = rig.backend.latest_generation(A);

        // A target switch's main queue stalls for 2 s from here.
        rig.backend.block(Duration::from_secs(2));
        let start = Instant::now();
        let event = rig
            .events
            .recv_timeout(Duration::from_millis(1500))
            .expect("Unavailable arrives while the main queue is stalled");
        let waited = start.elapsed();
        assert_eq!(event, OverlayEvent::Unavailable(A));
        assert!(
            waited >= Duration::from_millis(300),
            "the wait is {PRESENTATION_WAIT:?}, not immediate: {waited:?}"
        );
        assert!(
            waited < Duration::from_millis(1000),
            "Unavailable took {waited:?} with the main queue stalled for 2 s"
        );
        assert!(
            rig.events.recv_timeout(Duration::from_millis(200)).is_err(),
            "exactly one outcome"
        );

        // The main thread recovers and rechecks: the panel is still covered, which adds nothing.
        rig.backend.recheck_on_main(A, generation);
        assert!(rig.settle().is_empty());
    }

    /// A covered HUD configures at t=0, the main queue stalls for 2 s, and another target switch
    /// issues a show at t=100 ms. That show can't configure or arm anything until the main queue
    /// runs, and must not silence the deadline of the configuration before it.
    #[test]
    fn a_show_queued_behind_the_stall_does_not_silence_the_deadline() {
        let mut rig = Rig::bare_on_the_clock();
        rig.subscribe().unwrap();
        rig.backend.set_sight(A, Sight::Occluded);
        rig.overlay.show(A, &overlay_on(1)).unwrap();
        assert!(rig.settle().is_empty(), "configured, and waiting");
        let configured = Instant::now();

        rig.backend.block(Duration::from_secs(2));
        std::thread::sleep(Duration::from_millis(100));
        rig.overlay.show(A, &overlay_on(1)).unwrap();
        assert_eq!(
            rig.backend.generations(A).len(),
            1,
            "the newer show is queued behind the stall, not configured"
        );

        // The deadline answers at about 500 ms, for the newer command, with the main queue still
        // stuck.
        let event = rig
            .events
            .recv_timeout(Duration::from_millis(1500))
            .expect("Unavailable arrives while the main queue is stalled");
        let waited = configured.elapsed();
        assert_eq!(event, OverlayEvent::Unavailable(A));
        assert!(
            waited >= Duration::from_millis(350),
            "the wait is {PRESENTATION_WAIT:?}, not immediate: {waited:?}"
        );
        assert!(
            waited < Duration::from_millis(1000),
            "Unavailable took {waited:?} with the main queue stalled for 2 s"
        );
        assert!(
            rig.events.recv_timeout(Duration::from_millis(200)).is_err(),
            "exactly one outcome"
        );
        assert_eq!(
            rig.backend.generations(A).len(),
            1,
            "the main queue is still stalled"
        );

        // The panel gets presented while the queued show still waits. That belongs to the
        // configuration before it, which the queued show supersedes: not published.
        rig.backend.set_sight(A, Sight::Visible);
        assert!(rig.events.try_recv().is_err());

        // The main queue resumes: the queued show configures, and its own outcome follows.
        assert_eq!(rig.settle(), [OverlayEvent::Visible(A)]);
        assert_eq!(rig.backend.generations(A).len(), 2);
    }

    #[test]
    fn a_replay_voids_the_timers_of_the_show_before_it() {
        // Show at t=0 (covered), replay at t=450 ms.
        let mut rig = Rig::bare();
        rig.backend.set_sight(A, Sight::Occluded);
        rig.overlay.show(A, &overlay_on(1)).unwrap();
        rig.backend.drain();
        rig.subscribe().unwrap();
        assert!(rig.settle().is_empty());
        assert_eq!(rig.timers.armed(), 2);
        let generations = rig.backend.generations(A);
        assert_eq!(generations.len(), 2);

        // The show's deadline fires at t=500 ms, 50 ms into the replay's wait: nothing.
        rig.timers.fire(0);
        rig.backend.recheck(A, generations[0]);
        assert!(
            rig.settle().is_empty(),
            "a stale timer must not answer for the replay"
        );
        // The replay's own deadline answers, once.
        rig.timers.fire(1);
        assert_eq!(rig.settle(), [OverlayEvent::Unavailable(A)]);
        rig.backend.recheck(A, generations[1]);
        assert!(rig.settle().is_empty());
    }

    #[test]
    fn a_panel_presented_during_the_replays_wait_answers_visible_and_the_deadline_adds_nothing() {
        let mut rig = Rig::bare();
        rig.backend.set_sight(A, Sight::Occluded);
        rig.overlay.show(A, &overlay_on(1)).unwrap();
        rig.backend.drain();
        rig.subscribe().unwrap();
        assert!(rig.settle().is_empty());
        rig.timers.fire(0);
        assert!(rig.settle().is_empty());
        // Presented at t=600 ms: within the replay's budget, though past the show's.
        rig.backend.set_sight(A, Sight::Visible);
        assert_eq!(rig.settle(), [OverlayEvent::Visible(A)]);
        rig.timers.fire(1);
        rig.backend.recheck(A, rig.backend.latest_generation(A));
        assert!(rig.settle().is_empty());
    }

    /// The same, on the real clock: show at t=0, replay at t=350 ms. The show's own deadline
    /// (t=500 ms) is 150 ms into the replay and must stay silent; the replay's answers at
    /// t=850 ms.
    #[test]
    fn on_the_clock_a_replay_gets_its_own_full_wait() {
        let mut rig = Rig::bare_on_the_clock();
        rig.backend.set_sight(A, Sight::Occluded);
        rig.overlay.show(A, &overlay_on(1)).unwrap();
        rig.backend.drain();
        std::thread::sleep(Duration::from_millis(350));
        let replay = Instant::now();
        rig.subscribe().unwrap();
        assert!(
            rig.events.recv_timeout(Duration::from_millis(350)).is_err(),
            "the show's deadline answered for the replay"
        );
        let event = rig
            .events
            .recv_timeout(Duration::from_millis(900))
            .expect("the replay's own deadline answers");
        assert_eq!(event, OverlayEvent::Unavailable(A));
        assert!(
            replay.elapsed() >= Duration::from_millis(400),
            "answered {:?} into the replay's wait",
            replay.elapsed()
        );
    }

    #[test]
    fn a_newer_show_voids_the_timers_of_the_show_before_it() {
        let mut rig = Rig::new();
        rig.backend.set_sight(A, Sight::Occluded);
        rig.overlay.show(A, &overlay_on(1)).unwrap();
        assert!(rig.settle().is_empty());
        rig.overlay.show(A, &overlay_on(1)).unwrap();
        assert!(rig.settle().is_empty());
        let generations = rig.backend.generations(A);
        assert_eq!(generations.len(), 2);
        assert_ne!(generations[0], generations[1]);

        rig.timers.fire(0);
        rig.backend.recheck(A, generations[0]);
        assert!(
            rig.settle().is_empty(),
            "the older show's timers must not answer for the newer one"
        );
        rig.timers.fire(1);
        assert_eq!(rig.settle(), [OverlayEvent::Unavailable(A)]);
    }

    #[test]
    fn a_hidden_panels_timers_are_void() {
        let mut rig = Rig::new();
        rig.backend.set_sight(A, Sight::Occluded);
        rig.overlay.show(A, &overlay_on(1)).unwrap();
        assert!(rig.settle().is_empty());
        let generation = rig.backend.latest_generation(A);
        rig.overlay.hide(A).unwrap();
        assert!(rig.settle().is_empty());
        rig.timers.fire(0);
        rig.backend.recheck(A, generation);
        assert!(rig.settle().is_empty(), "a panel that is down owes nothing");
    }

    /// The two paths race for real: whichever clears the owed flag publishes the first outcome,
    /// and the other publishes nothing for it.
    #[test]
    fn racing_deadlines_publish_exactly_one_first_outcome() {
        let timers = Arc::new(ManualTimers::default());
        let shared = Arc::new(Shared::new(timers));
        let published = Arc::new(Mutex::new(Vec::new()));
        let sink = published.clone();
        *shared.sink.lock().unwrap() = Some(Arc::new(move |event| {
            sink.lock().unwrap().push(event);
        }));
        for sight in [Sight::Occluded, Sight::Visible] {
            for round in 0..300 {
                let seq = shared.issue(A);
                let generation = shared.configure(A, Some(seq));
                let start = Arc::new(Barrier::new(2));
                let off_main = {
                    let shared = shared.clone();
                    let start = start.clone();
                    std::thread::spawn(move || {
                        start.wait();
                        shared.expire(A, generation);
                    })
                };
                // This thread plays the main queue's recheck.
                start.wait();
                shared.recheck(A, generation, sight);
                off_main.join().unwrap();
                let events = std::mem::take(&mut *published.lock().unwrap());
                match sight {
                    // Only one of them can say Unavailable.
                    Sight::Occluded => assert_eq!(
                        events,
                        [OverlayEvent::Unavailable(A)],
                        "round {round}, covered panel"
                    ),
                    // The main queue sees the panel: Visible is the first outcome, or the
                    // deadline's Unavailable came first and Visible followed as a transition.
                    _ => assert!(
                        events == [OverlayEvent::Visible(A)]
                            || events == [OverlayEvent::Unavailable(A), OverlayEvent::Visible(A)],
                        "round {round}, visible panel: {events:?}"
                    ),
                }
            }
        }
    }

    #[test]
    fn an_occluded_panel_presented_within_the_wait_answers_visible_once() {
        let mut rig = Rig::new();
        rig.backend.set_sight(A, Sight::Occluded);
        rig.overlay.show(A, &overlay_on(1)).unwrap();
        assert!(rig.settle().is_empty());
        rig.backend.set_sight(A, Sight::Visible);
        assert_eq!(rig.settle(), [OverlayEvent::Visible(A)]);
        rig.timers.fire(0);
        rig.backend.recheck(A, rig.backend.latest_generation(A));
        assert!(rig.settle().is_empty());
    }

    #[test]
    fn a_panel_that_is_not_on_screen_answers_unavailable_without_waiting() {
        let mut rig = Rig::new();
        rig.backend.set_sight(A, Sight::OffScreen);
        rig.overlay.show(A, &overlay_on(1)).unwrap();
        assert_eq!(rig.settle(), [OverlayEvent::Unavailable(A)]);
        rig.timers.fire(0);
        rig.backend.recheck(A, rig.backend.latest_generation(A));
        assert!(rig.settle().is_empty());
    }

    #[test]
    fn a_second_show_acknowledges_again_and_transitions_follow_the_outcome() {
        let mut rig = Rig::new();
        rig.overlay.show(A, &overlay_on(1)).unwrap();
        assert_eq!(rig.settle(), [OverlayEvent::Visible(A)]);
        rig.overlay.show(A, &overlay_on(1)).unwrap();
        assert_eq!(rig.settle(), [OverlayEvent::Visible(A)]);
        rig.backend.set_sight(A, Sight::Occluded);
        assert_eq!(rig.settle(), [OverlayEvent::Unavailable(A)]);
        rig.backend.set_sight(A, Sight::Visible);
        assert_eq!(rig.settle(), [OverlayEvent::Visible(A)]);
    }

    #[test]
    fn a_subscriber_replay_of_a_covered_panel_is_bounded_too() {
        let mut rig = Rig::bare();
        rig.overlay.show(A, &overlay_on(1)).unwrap();
        rig.backend.drain();
        rig.backend.set_sight(A, Sight::Occluded);
        rig.subscribe().unwrap();
        assert!(rig.settle().is_empty());
        rig.timers.fire(1);
        assert_eq!(rig.settle(), [OverlayEvent::Unavailable(A)]);
    }

    #[test]
    fn work_queued_before_the_handle_closes_is_dropped() {
        let mut rig = Rig::new();
        rig.backend.block(Duration::from_millis(50));
        rig.overlay.show(A, &overlay_on(1)).unwrap();
        let backend = rig.backend.clone();
        let events = rig.events;
        drop(rig.overlay);
        assert!(backend.closed.load(Ordering::Acquire));
        backend.drain();
        assert!(backend.log().is_empty());
        assert_eq!(events.try_iter().count(), 0);
    }
}
