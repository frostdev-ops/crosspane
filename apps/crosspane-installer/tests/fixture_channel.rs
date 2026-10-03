#![allow(clippy::unwrap_used)] // In-memory fixture assertions; no production child/native calls.

use crosspane_installer::fixture::*;
use crosspane_installer_core::AttemptId;
use crosspane_types::id::{NodeId, WindowId};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const PID: u32 = 321;
const WINDOW: WindowId = WindowId(9001);
fn call(id: u64, command: FixtureCommand) -> FixtureCall {
    FixtureCall {
        id,
        attempt: AttemptId(7),
        command,
    }
}
fn open() -> FixtureCall {
    call(
        1,
        FixtureCommand::Open {
            machine_label: "Test Mac".into(),
        },
    )
}
fn event(
    id: Option<u64>,
    sequence: u64,
    result: Result<FixtureEvent, FixtureError>,
) -> FixtureEventPacket {
    FixtureEventPacket {
        schema_version: 1,
        message: FixtureMessage {
            call_id: id,
            attempt: AttemptId(7),
            sequence,
            result,
        },
    }
}
fn snapshot(phase: Option<PhaseId>, ticks: u64, clicks: u64) -> FixtureEvent {
    FixtureEvent::Snapshot {
        snapshot: FixtureSnapshot {
            fixture: FixtureId(1),
            window: WINDOW,
            phase,
            pattern_ticks: ticks,
            target_clicks: clicks,
            window_facts: OwnWindowFacts::Unknown,
            tone: OwnToneState::Stopped,
        },
    }
}
fn until(mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(2);
    while !f() {
        assert!(Instant::now() < end, "fake channel watchdog");
        thread::sleep(Duration::from_millis(1));
    }
}
#[derive(Default)]
struct Fake {
    incoming: VecDeque<u8>,
    writing: Vec<u8>,
    calls: Vec<FixtureCall>,
    writes: usize,
    reads: usize,
    frames: usize,
    eof: bool,
    block_write: bool,
    block_read: bool,
    write_error: bool,
    partial: usize,
    partial_time: Option<u64>,
    auto_open: bool,
    cleanup: bool,
    retired: usize,
    tone: bool,
    write_time: Option<u64>,
    receipt_time: Option<u64>,
}
struct Child {
    shared: Arc<Mutex<Fake>>,
    clock: Arc<AtomicU64>,
    pid: u32,
}
impl InheritedFixtureChild for Child {
    fn pid(&self) -> u32 {
        self.pid
    }
    fn read(&mut self, byte: &mut [u8]) -> io::Result<usize> {
        let mut f = self.shared.lock().unwrap();
        f.reads += 1;
        if f.block_read {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        if let Some(b) = f.incoming.pop_front() {
            byte[0] = b;
            if b == b'\n' {
                f.frames += 1;
                if let Some(at) = f.receipt_time {
                    self.clock.store(at, Ordering::SeqCst);
                }
            }
            Ok(1)
        } else if f.eof {
            Ok(0)
        } else {
            Err(io::ErrorKind::WouldBlock.into())
        }
    }
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut f = self.shared.lock().unwrap();
        if f.block_write {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        if f.write_error {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let n = if f.partial == 0 {
            bytes.len()
        } else {
            bytes.len().min(f.partial)
        };
        f.writes += 1;
        f.writing.extend_from_slice(&bytes[..n]);
        if let Some(at) = f.partial_time {
            self.clock.store(at, Ordering::SeqCst);
        }
        if f.writing.last() == Some(&b'\n') {
            let control = decode_control(&std::mem::take(&mut f.writing)).unwrap();
            if matches!(control.call.command, FixtureCommand::PlayTone { .. }) {
                f.tone = true;
            }
            if matches!(control.call.command, FixtureCommand::StopTone { .. }) {
                f.tone = false;
            }
            if let Some(at) = f.write_time {
                self.clock.store(at, Ordering::SeqCst);
            }
            if f.auto_open && matches!(control.call.command, FixtureCommand::Open { .. }) {
                let FixtureCommand::Open { machine_label } = &control.call.command else {
                    unreachable!()
                };
                let mut packet = event(
                    Some(control.call.id),
                    1,
                    Ok(FixtureEvent::Opened {
                        fixture: FixtureId(control.call.id),
                        pid: self.pid,
                        window: WINDOW,
                        label: machine_label.clone(),
                    }),
                );
                packet.message.attempt = control.call.attempt;
                f.incoming.extend(encode_event(&packet).unwrap());
            }
            f.calls.push(control.call);
        }
        Ok(n)
    }
    fn cleanup_confirmed(&mut self) -> Result<bool, FixtureError> {
        Ok(self.shared.lock().unwrap().cleanup)
    }
    fn retire(&mut self) {
        let mut f = self.shared.lock().unwrap();
        f.retired += 1;
        f.tone = false;
    }
}
struct Harness {
    port: PipeFixturePort,
    shared: Arc<Mutex<Fake>>,
    clock: Arc<AtomicU64>,
}
impl Harness {
    fn new(auto_open: bool) -> Self {
        let shared = Arc::new(Mutex::new(Fake {
            auto_open,
            ..Fake::default()
        }));
        let clock = Arc::new(AtomicU64::new(10));
        let now = clock.clone();
        let port = PipeFixturePort::new(
            Box::new(Child {
                shared: shared.clone(),
                clock: clock.clone(),
                pid: PID,
            }),
            Arc::new(move || now.load(Ordering::SeqCst)),
        )
        .unwrap();
        Self {
            port,
            shared,
            clock,
        }
    }
    fn submit(&mut self, call: FixtureCall) {
        let mut result = Err(FixtureError::Busy);
        until(|| {
            result = self.port.submit(call.clone());
            result != Err(FixtureError::Busy)
        });
        assert_eq!(result, Ok(()));
    }
    fn sent(&self, id: u64) {
        until(|| self.shared.lock().unwrap().calls.iter().any(|c| c.id == id));
    }
    fn send(&self, packet: FixtureEventPacket) {
        self.shared
            .lock()
            .unwrap()
            .incoming
            .extend(encode_event(&packet).unwrap());
    }
    fn raw(&self, bytes: Vec<u8>) {
        self.shared.lock().unwrap().incoming.extend(bytes);
    }
    fn receipts(&mut self, n: usize) -> Vec<FixtureReceipt> {
        let mut all = Vec::new();
        until(|| {
            all.extend(self.port.poll_receipts());
            all.len() >= n
        });
        assert_eq!(all.len(), n);
        all
    }
    fn opened(&mut self) {
        self.submit(open());
        assert_eq!(
            self.receipts(1)[0].message.result,
            Ok(FixtureEvent::Opened {
                fixture: FixtureId(1),
                pid: PID,
                window: WINDOW,
                label: "Test Mac".into()
            })
        );
    }
    fn retired(&self) {
        until(|| self.shared.lock().unwrap().retired == 1);
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.port.cancel();
        self.retired();
    }
}

#[test]
fn exact_control_bytes_all_commands_and_selected_speaker_key() {
    let peer = NodeId([0xab; 32]);
    let commands = [
        (
            FixtureCommand::Open {
                machine_label: "Test Mac".into(),
            },
            json!({"command":"open","machine_label":"Test Mac"}),
        ),
        (
            FixtureCommand::ArmTarget {
                fixture: FixtureId(1),
                phase: PhaseId(8),
            },
            json!({"command":"arm_target","fixture":1,"phase":8}),
        ),
        (
            FixtureCommand::ObserveWindow {
                fixture: FixtureId(1),
            },
            json!({"command":"observe_window","fixture":1}),
        ),
        (
            FixtureCommand::PlayTone {
                fixture: FixtureId(1),
                output: SpeakersSelection {
                    peer,
                    device_key: format!("crosspane.{peer}.speaker"),
                },
            },
            json!({"command":"play_tone","fixture":1,"output":{"peer":peer.to_string(),"device_key":format!("crosspane.{peer}.speaker")}}),
        ),
        (
            FixtureCommand::StopTone {
                fixture: FixtureId(1),
                tone: ToneId(2),
            },
            json!({"command":"stop_tone","fixture":1,"tone":2}),
        ),
        (
            FixtureCommand::Close {
                fixture: FixtureId(1),
            },
            json!({"command":"close","fixture":1}),
        ),
    ];
    for (command, expected) in commands {
        let packet = FixtureControlPacket {
            schema_version: 1,
            call: call(1, command),
        };
        let bytes = encode_control(&packet).unwrap();
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            json!({"schema_version":1,"call":{"id":1,"attempt":7,"command":expected}})
        );
        assert_eq!(decode_control(&bytes), Ok(packet));
    }
    let literal = b"{\"schema_version\":1,\"call\":{\"id\":1,\"attempt\":7,\"command\":{\"command\":\"open\",\"machine_label\":\"Test Mac\"}}}\n";
    assert_eq!(decode_control(literal).unwrap().call, open());
}

#[test]
fn exact_event_values_all_lifecycle_states_and_nullable_fields() {
    let events = [
        (
            FixtureEvent::Opened {
                fixture: FixtureId(1),
                pid: PID,
                window: WINDOW,
                label: "Test Mac".into(),
            },
            json!({"event":"opened","fixture":1,"pid":PID,"window":9001,"label":"Test Mac"}),
        ),
        (
            FixtureEvent::TargetArmed {
                fixture: FixtureId(1),
                phase: PhaseId(8),
            },
            json!({"event":"target_armed","fixture":1,"phase":8}),
        ),
        (
            snapshot(None, u64::MAX, u64::MAX),
            json!({"event":"snapshot","snapshot":{"fixture":1,"window":9001,"phase":null,"pattern_ticks":u64::MAX,"target_clicks":u64::MAX,"window_facts":{"state":"unknown"},"tone":{"state":"stopped"}}}),
        ),
        (
            FixtureEvent::ToneStarted {
                fixture: FixtureId(1),
                tone: ToneId(2),
            },
            json!({"event":"tone_started","fixture":1,"tone":2}),
        ),
        (
            FixtureEvent::ToneStopped {
                fixture: FixtureId(1),
                tone: ToneId(2),
            },
            json!({"event":"tone_stopped","fixture":1,"tone":2}),
        ),
        (
            FixtureEvent::CloseRequested {
                fixture: FixtureId(1),
            },
            json!({"event":"close_requested","fixture":1}),
        ),
        (
            FixtureEvent::Closed {
                fixture: FixtureId(1),
            },
            json!({"event":"closed","fixture":1}),
        ),
        (
            FixtureEvent::Lost {
                fixture: FixtureId(1),
                reason: FixtureError::CleanupFailed,
            },
            json!({"event":"lost","fixture":1,"reason":"cleanup_failed"}),
        ),
    ];
    for (value, expected) in events {
        let packet = event(None, 2, Ok(value));
        let bytes = encode_event(&packet).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            json!({"schema_version":1,"message":{"call_id":null,"attempt":7,"sequence":2,"result":{"Ok":expected}}})
        );
        assert_eq!(decode_event(&bytes), Ok(packet));
    }
    for visible in [None, Some(false), Some(true)] {
        for display in [None, Some(false), Some(true)] {
            for tone in [
                OwnToneState::Stopped,
                OwnToneState::Running { tone: ToneId(3) },
                OwnToneState::StopUnconfirmed { tone: ToneId(3) },
            ] {
                let mut s = if let FixtureEvent::Snapshot { snapshot: s } =
                    snapshot(Some(PhaseId(8)), 3, 4)
                {
                    s
                } else {
                    unreachable!()
                };
                s.window_facts = OwnWindowFacts::Present {
                    visible_on_user_workspace: visible,
                    on_initial_display: display,
                };
                s.tone = tone;
                let packet = event(Some(9), 8, Ok(FixtureEvent::Snapshot { snapshot: s }));
                assert_eq!(decode_event(&encode_event(&packet).unwrap()), Ok(packet));
            }
        }
    }
    let mut s = if let FixtureEvent::Snapshot { snapshot: s } = snapshot(None, 0, 0) {
        s
    } else {
        unreachable!()
    };
    s.window_facts = OwnWindowFacts::Missing;
    let p = event(None, 1, Ok(FixtureEvent::Snapshot { snapshot: s }));
    assert_eq!(decode_event(&encode_event(&p).unwrap()), Ok(p));
}

#[test]
fn each_error_has_exact_string_form_and_no_object_enum_alias() {
    let cases = [
        (FixtureError::BadCall, "bad_call"),
        (FixtureError::Busy, "busy"),
        (FixtureError::NotOwned, "not_owned"),
        (FixtureError::Unavailable, "unavailable"),
        (FixtureError::UnknownWindow, "unknown_window"),
        (FixtureError::AmbiguousWindow, "ambiguous_window"),
        (FixtureError::OutputUnavailable, "output_unavailable"),
        (FixtureError::OutputChanged, "output_changed"),
        (FixtureError::UnsupportedFormat, "unsupported_format"),
        (FixtureError::TimedOut, "timed_out"),
        (FixtureError::ChildExited, "child_exited"),
        (FixtureError::CounterExhausted, "counter_exhausted"),
        (FixtureError::ChannelClosed, "channel_closed"),
        (FixtureError::InvalidMessage, "invalid_message"),
        (FixtureError::Refused, "refused"),
        (FixtureError::CleanupFailed, "cleanup_failed"),
    ];
    for (error, name) in cases {
        let p = event(Some(1), 1, Err(error));
        let mut v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["message"]["result"], json!({"Err":name}));
        assert_eq!(decode_event(&encode_event(&p).unwrap()), Ok(p));
        v["message"]["result"] = json!({"Err":{name:null}});
        assert_eq!(
            decode_event(&serde_json::to_vec(&v).unwrap()),
            Err(FixtureError::InvalidMessage)
        );
    }
}

#[test]
fn strict_codec_rejects_every_missing_field_arrays_duplicates_and_trailing_packets() {
    let p = FixtureControlPacket {
        schema_version: 1,
        call: open(),
    };
    let original = serde_json::to_value(p).unwrap();
    for path in [
        vec!["schema_version"],
        vec!["call"],
        vec!["call", "id"],
        vec!["call", "attempt"],
        vec!["call", "command"],
        vec!["call", "command", "command"],
        vec!["call", "command", "machine_label"],
    ] {
        let mut v = original.clone();
        let mut object = &mut v;
        for key in &path[..path.len() - 1] {
            object = &mut object[*key];
        }
        object
            .as_object_mut()
            .unwrap()
            .remove(path.last().unwrap().to_owned());
        assert!(decode_control(&serde_json::to_vec(&v).unwrap()).is_err());
    }
    for literal in [
        r#"[1,{"id":1,"attempt":7,"command":{"command":"open","machine_label":"a"}}]"#,
        r#"{"schema_version":1,"call":[1,7,{"command":"open","machine_label":"a"}]}"#,
        r#"{"schema_version":1,"call":{"id":1,"attempt":7,"command":{"command":"open","machine_label":"a","machine_label":"b"}}}"#,
        r#"{"schema_version":1,"schema_version":1,"call":{"id":1,"attempt":7,"command":{"command":"open","machine_label":"a"}}}"#,
        r#"{"schema_version":1,"call":{"id":1,"attempt":7,"command":{"command":"unknown","machine_label":"a"}}}"#,
        r#"{"schema_version":1,"call":{"id":1,"attempt":7,"command":{"command":"open","machine_label":"a","text":"TEST_TEXT_SENTINEL"}}}"#,
    ] {
        assert!(decode_control(literal.as_bytes()).is_err(), "{literal}");
    }
    let good = encode_control(&FixtureControlPacket {
        schema_version: 1,
        call: open(),
    })
    .unwrap();
    let mut trailing = good[..good.len() - 1].to_vec();
    trailing.extend_from_slice(&good);
    assert!(decode_control(&trailing).is_err());
    let mut two_lines = good.clone();
    two_lines.extend(good);
    assert!(decode_control(&two_lines).is_err());
    assert!(decode_control(b"{\n\"schema_version\":1}").is_err());
}

#[test]
fn nullable_event_fields_required_and_recursive_unknowns_fail_closed() {
    let p = event(Some(2), 2, Ok(snapshot(None, 0, 0)));
    let v = serde_json::to_value(&p).unwrap();
    for path in [
        vec!["message", "call_id"],
        vec!["message", "result", "Ok", "snapshot", "phase"],
    ] {
        let mut altered = v.clone();
        let mut o = &mut altered;
        for key in &path[..path.len() - 1] {
            o = &mut o[*key];
        }
        o.as_object_mut().unwrap().remove(*path.last().unwrap());
        assert!(decode_event(&serde_json::to_vec(&altered).unwrap()).is_err());
    }
    let mut v = v;
    v["message"]["result"]["Ok"]["snapshot"]["window_facts"] =
        json!({"state":"present","visible_on_user_workspace":null,"on_initial_display":null});
    assert!(decode_event(&serde_json::to_vec(&v).unwrap()).is_ok());
    for missing in ["visible_on_user_workspace", "on_initial_display"] {
        let mut a = v.clone();
        a["message"]["result"]["Ok"]["snapshot"]["window_facts"]
            .as_object_mut()
            .unwrap()
            .remove(missing);
        assert!(decode_event(&serde_json::to_vec(&a).unwrap()).is_err());
    }
    for nested in [
        json!([]),
        json!({"x":[1]}),
        json!({"x":{"x":{"x":{"x":{"x":{"x":{"x":{"x":{"x":{"x":{"x":{"x":{"x":{"x":{"x":{"x":{"x":null}}}}}}}}}}}}}}}}}),
    ] {
        let mut a = v.clone();
        a["message"]["ignored"] = nested;
        assert!(decode_event(&serde_json::to_vec(&a).unwrap()).is_err());
    }
}

#[test]
fn exact_line_string_scalar_id_and_speaker_bounds() {
    let base = serde_json::to_value(FixtureControlPacket {
        schema_version: 1,
        call: open(),
    })
    .unwrap();
    for bad in [
        json!(0),
        json!(-1),
        json!(1.5),
        json!(true),
        json!("1"),
        json!(null),
        json!(18446744073709551616_f64),
    ] {
        for field in ["id", "attempt"] {
            let mut v = base.clone();
            v["call"][field] = bad.clone();
            assert!(decode_control(&serde_json::to_vec(&v).unwrap()).is_err());
        }
    }
    for schema in [0, 2, u32::MAX as u64, u64::MAX] {
        let mut v = base.clone();
        v["schema_version"] = schema.into();
        assert!(decode_control(&serde_json::to_vec(&v).unwrap()).is_err());
    }
    for text in ["é".repeat(32), "x".repeat(64), "".into()] {
        let p = FixtureControlPacket {
            schema_version: 1,
            call: call(
                u64::MAX,
                FixtureCommand::Open {
                    machine_label: text,
                },
            ),
        };
        assert!(encode_control(&p).is_ok());
    }
    for text in [
        "é".repeat(33),
        "x".repeat(65),
        "a\nb".into(),
        "a\0b".into(),
        "a\tb".into(),
    ] {
        let p = FixtureControlPacket {
            schema_version: 1,
            call: call(
                1,
                FixtureCommand::Open {
                    machine_label: text,
                },
            ),
        };
        assert!(encode_control(&p).is_err());
    }
    let good = encode_control(&FixtureControlPacket {
        schema_version: 1,
        call: open(),
    })
    .unwrap();
    let mut exact = vec![b' '; MAX_LINE_BYTES - good.len()];
    exact.extend(&good);
    assert!(decode_control(&exact).is_ok());
    exact.insert(0, b' ');
    assert!(decode_control(&exact).is_err());
    let peer = NodeId([1; 32]);
    for key in [
        format!("crosspane.{}.speaker", NodeId([2; 32])),
        "default".into(),
        "x".repeat(161),
        format!("crosspane.{peer}.speaker\n"),
    ] {
        let p = FixtureControlPacket {
            schema_version: 1,
            call: call(
                2,
                FixtureCommand::PlayTone {
                    fixture: FixtureId(1),
                    output: SpeakersSelection {
                        peer,
                        device_key: key,
                    },
                },
            ),
        };
        assert!(encode_control(&p).is_err());
    }
    assert_eq!(
        practice_title("Test Mac", AttemptId(7), FixtureId(1)).unwrap(),
        "Crosspane practice | Test Mac | attempt 7 | fixture 1"
    );
}

#[test]
fn partial_inherited_pipes_keep_exact_call_and_complete_receipt_time_before_poll() {
    let mut h = Harness::new(true);
    {
        let mut f = h.shared.lock().unwrap();
        f.partial = 3;
        f.write_time = Some(20);
        f.receipt_time = Some(100);
    }
    h.submit(open());
    h.sent(1);
    until(|| h.shared.lock().unwrap().frames == 1);
    thread::sleep(Duration::from_millis(10));
    h.clock.store(500, Ordering::SeqCst);
    let r = h.receipts(1);
    assert_eq!(r[0].received_at_ms, 100);
    assert_eq!(h.shared.lock().unwrap().calls, vec![open()]);
    assert!(h.shared.lock().unwrap().writes > 1);
    assert_eq!(h.port.poll(), Vec::<FixtureMessage>::new());
}

#[test]
fn response_before_complete_control_write_is_not_admitted() {
    let mut h = Harness::new(false);
    h.shared.lock().unwrap().block_write = true;
    h.submit(open());
    h.send(event(
        Some(1),
        1,
        Ok(FixtureEvent::Opened {
            fixture: FixtureId(1),
            pid: PID,
            window: WINDOW,
            label: "Test Mac".into(),
        }),
    ));
    assert_eq!(
        h.receipts(1)[0].message.result,
        Err(FixtureError::InvalidMessage)
    );
    h.retired();
    assert_eq!(h.shared.lock().unwrap().writes, 0);
}

#[test]
fn calls_attempt_and_fixture_ownership_reject_before_pipe_io() {
    let mut h = Harness::new(true);
    h.opened();
    let before = h.shared.lock().unwrap().calls.len();
    for bad in [
        open(),
        call(
            0,
            FixtureCommand::Close {
                fixture: FixtureId(1),
            },
        ),
        call(
            2,
            FixtureCommand::ObserveWindow {
                fixture: FixtureId(99),
            },
        ),
        FixtureCall {
            id: 2,
            attempt: AttemptId(8),
            command: FixtureCommand::ObserveWindow {
                fixture: FixtureId(1),
            },
        },
        call(
            2,
            FixtureCommand::Open {
                machine_label: "new child needed".into(),
            },
        ),
    ] {
        assert!(h.port.submit(bad).is_err());
    }
    assert_eq!(h.shared.lock().unwrap().calls.len(), before);
    h.submit(call(
        2,
        FixtureCommand::ObserveWindow {
            fixture: FixtureId(1),
        },
    ));
    h.sent(2);
    h.send(event(Some(2), 2, Ok(snapshot(None, 1, 0))));
    assert_eq!(h.receipts(1)[0].message.result, Ok(snapshot(None, 1, 0)));
}

#[test]
fn all_response_identity_and_correlation_mismatches_retire_without_native_success() {
    for mutation in 0..7 {
        let mut h = Harness::new(true);
        h.opened();
        h.submit(call(
            2,
            FixtureCommand::ObserveWindow {
                fixture: FixtureId(1),
            },
        ));
        h.sent(2);
        let mut p = event(Some(2), 2, Ok(snapshot(None, 1, 0)));
        match mutation {
            0 => p.message.attempt = AttemptId(8),
            1 => p.message.sequence = 1,
            2 => p.message.call_id = Some(99),
            3 => {
                if let Ok(FixtureEvent::Snapshot { snapshot: s }) = &mut p.message.result {
                    s.fixture = FixtureId(99);
                }
            }
            4 => {
                if let Ok(FixtureEvent::Snapshot { snapshot: s }) = &mut p.message.result {
                    s.window = WindowId(44);
                }
            }
            5 => {
                if let Ok(FixtureEvent::Snapshot { snapshot: s }) = &mut p.message.result {
                    s.phase = Some(PhaseId(4));
                }
            }
            6 => {
                p.message.result = Ok(FixtureEvent::ToneStarted {
                    fixture: FixtureId(1),
                    tone: ToneId(2),
                })
            }
            _ => unreachable!(),
        }
        h.send(p);
        assert_eq!(
            h.receipts(1)[0].message.result,
            Err(FixtureError::InvalidMessage)
        );
        h.retired();
    }
}

#[test]
fn opened_pid_id_and_machine_label_are_bound_to_this_child_call() {
    for mutation in 0..3 {
        let mut h = Harness::new(false);
        h.submit(open());
        h.sent(1);
        h.send(event(
            Some(1),
            1,
            Ok(FixtureEvent::Opened {
                fixture: FixtureId(if mutation == 0 { 8 } else { 1 }),
                pid: if mutation == 1 { PID + 1 } else { PID },
                window: WINDOW,
                label: if mutation == 2 {
                    "Other".into()
                } else {
                    "Test Mac".into()
                },
            }),
        ));
        assert_eq!(
            h.receipts(1)[0].message.result,
            Err(FixtureError::InvalidMessage)
        );
        h.retired();
    }
}

#[test]
fn phases_strictly_increase_and_cumulative_counters_do_not_reset() {
    let mut h = Harness::new(true);
    h.opened();
    h.submit(call(
        2,
        FixtureCommand::ArmTarget {
            fixture: FixtureId(1),
            phase: PhaseId(10),
        },
    ));
    h.sent(2);
    h.send(event(
        Some(2),
        2,
        Ok(FixtureEvent::TargetArmed {
            fixture: FixtureId(1),
            phase: PhaseId(10),
        }),
    ));
    h.receipts(1);
    h.send(event(None, 3, Ok(snapshot(Some(PhaseId(10)), 4, 2))));
    h.receipts(1);
    assert_eq!(
        h.port.submit(call(
            3,
            FixtureCommand::ArmTarget {
                fixture: FixtureId(1),
                phase: PhaseId(10)
            }
        )),
        Err(FixtureError::NotOwned)
    );
    h.submit(call(
        3,
        FixtureCommand::ArmTarget {
            fixture: FixtureId(1),
            phase: PhaseId(11),
        },
    ));
    h.sent(3);
    h.send(event(
        Some(3),
        4,
        Ok(FixtureEvent::TargetArmed {
            fixture: FixtureId(1),
            phase: PhaseId(11),
        }),
    ));
    h.receipts(1);
    h.send(event(None, 5, Ok(snapshot(Some(PhaseId(11)), 5, 2))));
    assert_eq!(
        h.receipts(1)[0].message.result,
        Ok(snapshot(Some(PhaseId(11)), 5, 2))
    );
    h.send(event(None, 6, Ok(snapshot(Some(PhaseId(10)), 6, 3))));
    assert_eq!(
        h.receipts(1)[0].message.result,
        Err(FixtureError::InvalidMessage)
    );
}

#[test]
fn decreasing_each_counter_is_rejected_independently_with_unknown_native_facts() {
    for clicks in [false, true] {
        let mut h = Harness::new(true);
        h.opened();
        h.send(event(None, 2, Ok(snapshot(None, 8, 9))));
        h.receipts(1);
        h.send(event(
            None,
            3,
            Ok(snapshot(
                None,
                if clicks { 8 } else { 7 },
                if clicks { 8 } else { 9 },
            )),
        ));
        assert_eq!(
            h.receipts(1)[0].message.result,
            Err(FixtureError::InvalidMessage)
        );
        h.retired();
    }
}

#[test]
fn queue32_rejects_next_call_without_io_or_consuming_id() {
    let mut h = Harness::new(true);
    h.opened();
    for id in 2..=33 {
        h.submit(call(
            id,
            FixtureCommand::ObserveWindow {
                fixture: FixtureId(1),
            },
        ));
    }
    h.sent(33);
    let before = h.shared.lock().unwrap().calls.len();
    assert_eq!(
        h.port.submit(call(
            34,
            FixtureCommand::ObserveWindow {
                fixture: FixtureId(1)
            }
        )),
        Err(FixtureError::Busy)
    );
    thread::sleep(Duration::from_millis(5));
    assert_eq!(h.shared.lock().unwrap().calls.len(), before);
    for id in 2..=33 {
        h.send(event(Some(id), id, Ok(snapshot(None, id, 0))));
    }
    let messages = h.receipts(MAX_QUEUE);
    assert!(messages.iter().all(|r| r.message.result.is_ok()));
    h.submit(call(
        34,
        FixtureCommand::ObserveWindow {
            fixture: FixtureId(1),
        },
    ));
    h.sent(34);
    h.send(event(Some(34), 34, Ok(snapshot(None, 34, 0))));
    h.receipts(1);
}

#[test]
fn unsolicited_snapshots_coalesce_but_lifecycle_failure_survives_and_invalidates() {
    let mut h = Harness::new(true);
    h.opened();
    for seq in 2..=40 {
        h.send(event(None, seq, Ok(snapshot(None, seq, 0))));
    }
    h.send(event(
        None,
        41,
        Ok(FixtureEvent::Lost {
            fixture: FixtureId(1),
            reason: FixtureError::OutputChanged,
        }),
    ));
    h.retired();
    let all = h.port.poll();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].result, Ok(snapshot(None, 40, 0)));
    assert_eq!(
        all[1].result,
        Ok(FixtureEvent::Lost {
            fixture: FixtureId(1),
            reason: FixtureError::OutputChanged
        })
    );
}

#[test]
fn full_reply_queue_backpressures_without_losing_lifecycle_error() {
    let mut h = Harness::new(true);
    h.opened();
    for id in 2..=33 {
        h.submit(call(
            id,
            FixtureCommand::ObserveWindow {
                fixture: FixtureId(1),
            },
        ));
    }
    h.sent(33);
    for id in 2..=33 {
        h.send(event(Some(id), id, Ok(snapshot(None, id, 0))));
    }
    h.send(event(
        None,
        34,
        Ok(FixtureEvent::Lost {
            fixture: FixtureId(1),
            reason: FixtureError::CleanupFailed,
        }),
    ));
    until(|| h.shared.lock().unwrap().frames == 34);
    h.retired(); // A full queue must never delay owned-output shutdown after a native loss.
    assert_eq!(h.port.poll().len(), 32);
    assert_eq!(
        h.receipts(1)[0].message.result,
        Ok(FixtureEvent::Lost {
            fixture: FixtureId(1),
            reason: FixtureError::CleanupFailed
        })
    );
}

#[test]
fn shared_response_deadline_cancel_eof_corruption_and_late_packets_retire_owned_child() {
    for failure in [
        FixtureError::TimedOut,
        FixtureError::ChannelClosed,
        FixtureError::ChildExited,
        FixtureError::InvalidMessage,
    ] {
        let mut h = Harness::new(false);
        h.submit(open());
        h.sent(1);
        match failure {
            FixtureError::TimedOut => h.clock.store(2010, Ordering::SeqCst),
            FixtureError::ChannelClosed => h.port.cancel(),
            FixtureError::ChildExited => h.shared.lock().unwrap().eof = true,
            FixtureError::InvalidMessage => h.raw(b"not json\n".to_vec()),
            _ => unreachable!(),
        }
        assert_eq!(h.receipts(1)[0].message.result, Err(failure));
        h.retired();
        let reads = h.shared.lock().unwrap().reads;
        h.send(event(
            Some(1),
            1,
            Ok(FixtureEvent::Opened {
                fixture: FixtureId(1),
                pid: PID,
                window: WINDOW,
                label: "Test Mac".into(),
            }),
        ));
        thread::sleep(Duration::from_millis(5));
        assert!(h.port.poll().is_empty());
        assert_eq!(h.shared.lock().unwrap().reads, reads);
        assert_eq!(
            h.port.submit(call(
                2,
                FixtureCommand::Open {
                    machine_label: "new".into()
                }
            )),
            Err(FixtureError::ChannelClosed)
        );
    }
}

#[test]
fn partial_write_timeout_never_resends_and_oversized_or_partial_eof_lines_fail() {
    let mut prefix = Harness::new(false);
    {
        let mut f = prefix.shared.lock().unwrap();
        f.partial = 3;
        f.partial_time = Some(2010);
    }
    prefix.submit(open());
    assert_eq!(
        prefix.receipts(1)[0].message.result,
        Err(FixtureError::TimedOut)
    );
    prefix.retired();
    assert_eq!(prefix.shared.lock().unwrap().writing.len(), 3);
    assert_eq!(prefix.shared.lock().unwrap().writes, 1);
    assert!(prefix.shared.lock().unwrap().calls.is_empty());
    let mut h = Harness::new(false);
    {
        let mut f = h.shared.lock().unwrap();
        f.partial = 3;
        f.write_time = Some(10);
    }
    h.submit(open());
    h.sent(1);
    h.clock.store(2010, Ordering::SeqCst);
    assert_eq!(h.receipts(1)[0].message.result, Err(FixtureError::TimedOut));
    h.retired();
    assert_eq!(h.shared.lock().unwrap().calls, vec![open()]);
    for eof in [false, true] {
        let mut h = Harness::new(false);
        h.submit(open());
        h.sent(1);
        h.raw(if eof {
            b"{\"schema_version\":".to_vec()
        } else {
            vec![b'x'; MAX_LINE_BYTES + 1]
        });
        if eof {
            h.shared.lock().unwrap().eof = true;
        }
        assert_eq!(
            h.receipts(1)[0].message.result,
            Err(if eof {
                FixtureError::ChildExited
            } else {
                FixtureError::InvalidMessage
            })
        );
        h.retired();
    }
}

#[test]
fn failed_open_and_child_counter_exhaustion_retire_without_inventing_native_events() {
    let mut h = Harness::new(false);
    h.submit(open());
    h.sent(1);
    h.send(event(Some(1), 1, Err(FixtureError::Unavailable)));
    assert_eq!(
        h.receipts(1)[0].message.result,
        Err(FixtureError::Unavailable)
    );
    h.retired();
    let mut h = Harness::new(true);
    h.opened();
    h.submit(call(
        2,
        FixtureCommand::ObserveWindow {
            fixture: FixtureId(1),
        },
    ));
    h.sent(2);
    h.send(event(Some(2), 2, Err(FixtureError::CounterExhausted)));
    assert_eq!(
        h.receipts(1)[0].message.result,
        Err(FixtureError::CounterExhausted)
    );
    h.retired();
}

#[test]
fn native_close_request_is_unsolicited_and_idempotent_close_does_not_settle_early() {
    let mut h = Harness::new(true);
    h.opened();
    h.send(event(
        None,
        2,
        Ok(FixtureEvent::CloseRequested {
            fixture: FixtureId(1),
        }),
    ));
    assert_eq!(h.receipts(1)[0].message.call_id, None);
    assert_eq!(h.shared.lock().unwrap().retired, 0);
    for id in [2, 3] {
        h.submit(call(
            id,
            FixtureCommand::Close {
                fixture: FixtureId(1),
            },
        ));
        h.sent(id);
    }
    for id in [2, 3] {
        h.send(event(
            Some(id),
            id + 1,
            Ok(FixtureEvent::CloseRequested {
                fixture: FixtureId(1),
            }),
        ));
    }
    assert!(
        h.receipts(2)
            .iter()
            .all(|r| matches!(r.message.result, Ok(FixtureEvent::CloseRequested { .. })))
    );
    h.shared.lock().unwrap().cleanup = true;
    h.send(event(
        Some(3),
        5,
        Ok(FixtureEvent::Closed {
            fixture: FixtureId(1),
        }),
    ));
    h.retired();
    let receipts = h.port.poll();
    assert_eq!(
        receipts[0].result,
        Ok(FixtureEvent::Closed {
            fixture: FixtureId(1)
        })
    );
    assert_eq!(receipts[1].result, Err(FixtureError::ChildExited));
    assert_eq!(
        h.port.submit(call(
            4,
            FixtureCommand::Close {
                fixture: FixtureId(1)
            }
        )),
        Err(FixtureError::ChannelClosed)
    );
}

#[test]
fn parent_drop_stops_own_tone_and_defers_failures_without_losing_full_queue() {
    let mut h = Harness::new(true);
    h.opened();
    for id in 2..=33 {
        h.submit(call(
            id,
            FixtureCommand::ObserveWindow {
                fixture: FixtureId(1),
            },
        ));
    }
    h.sent(33);
    for id in 2..=32 {
        h.send(event(Some(id), id, Ok(snapshot(None, id, 0))));
    }
    until(|| h.shared.lock().unwrap().frames == 32);
    h.port.cancel();
    h.retired();
    let r = h.receipts(32);
    assert_eq!(r.last().unwrap().message.call_id, Some(33));
    assert_eq!(
        r.last().unwrap().message.result,
        Err(FixtureError::ChannelClosed)
    );
    let shared = h.shared.clone();
    drop(h);
    assert_eq!(shared.lock().unwrap().retired, 1);
    let mut tone = Harness::new(true);
    tone.opened();
    let peer = NodeId([4; 32]);
    tone.submit(call(
        2,
        FixtureCommand::PlayTone {
            fixture: FixtureId(1),
            output: SpeakersSelection {
                peer,
                device_key: format!("crosspane.{peer}.speaker"),
            },
        },
    ));
    tone.sent(2);
    assert!(tone.shared.lock().unwrap().tone);
    let shared = tone.shared.clone();
    drop(tone);
    assert!(!shared.lock().unwrap().tone);
}

#[test]
fn complete_line_at_exact_deadline_is_timeout_and_no_success_is_restamped() {
    let mut h = Harness::new(true);
    h.shared.lock().unwrap().receipt_time = Some(2010);
    h.submit(open());
    assert_eq!(h.receipts(1)[0].message.result, Err(FixtureError::TimedOut));
    h.retired();
}

#[test]
fn close_requested_is_not_closed_until_native_exit_and_window_absence_are_confirmed() {
    let mut h = Harness::new(true);
    h.opened();
    h.submit(call(
        2,
        FixtureCommand::Close {
            fixture: FixtureId(1),
        },
    ));
    h.sent(2);
    h.send(event(
        Some(2),
        2,
        Ok(FixtureEvent::CloseRequested {
            fixture: FixtureId(1),
        }),
    ));
    assert_eq!(
        h.receipts(1)[0].message.result,
        Ok(FixtureEvent::CloseRequested {
            fixture: FixtureId(1)
        })
    );
    h.send(event(
        Some(2),
        3,
        Ok(FixtureEvent::Closed {
            fixture: FixtureId(1),
        }),
    ));
    until(|| h.shared.lock().unwrap().frames == 3);
    thread::sleep(Duration::from_millis(5));
    assert!(h.port.poll().is_empty());
    assert_eq!(h.shared.lock().unwrap().retired, 0);
    h.shared.lock().unwrap().cleanup = true;
    assert_eq!(
        h.receipts(1)[0].message.result,
        Ok(FixtureEvent::Closed {
            fixture: FixtureId(1)
        })
    );
    h.retired();
}

#[test]
fn close_cleanup_uncertainty_times_out_and_never_claims_projection_return() {
    let mut h = Harness::new(true);
    h.opened();
    h.submit(call(
        2,
        FixtureCommand::Close {
            fixture: FixtureId(1),
        },
    ));
    h.sent(2);
    h.send(event(
        Some(2),
        2,
        Ok(FixtureEvent::Closed {
            fixture: FixtureId(1),
        }),
    ));
    until(|| h.shared.lock().unwrap().frames == 2);
    h.clock.store(2010, Ordering::SeqCst);
    assert_eq!(h.receipts(1)[0].message.result, Err(FixtureError::TimedOut));
    h.retired();
}

#[test]
fn one_tone_owned_ids_idempotent_stop_and_cancelled_late_tone_are_bounded() {
    let mut h = Harness::new(true);
    h.opened();
    let peer = NodeId([3; 32]);
    let play = |id| {
        call(
            id,
            FixtureCommand::PlayTone {
                fixture: FixtureId(1),
                output: SpeakersSelection {
                    peer,
                    device_key: format!("crosspane.{peer}.speaker"),
                },
            },
        )
    };
    h.submit(play(2));
    h.sent(2);
    assert!(h.port.submit(play(3)).is_err());
    h.send(event(
        Some(2),
        2,
        Ok(FixtureEvent::ToneStarted {
            fixture: FixtureId(1),
            tone: ToneId(2),
        }),
    ));
    h.receipts(1);
    assert!(h.port.submit(play(3)).is_err());
    assert_eq!(
        h.port.submit(call(
            3,
            FixtureCommand::StopTone {
                fixture: FixtureId(1),
                tone: ToneId(99)
            }
        )),
        Err(FixtureError::NotOwned)
    );
    for id in [3, 4] {
        h.submit(call(
            id,
            FixtureCommand::StopTone {
                fixture: FixtureId(1),
                tone: ToneId(2),
            },
        ));
        h.sent(id);
        h.send(event(
            Some(id),
            id,
            Ok(FixtureEvent::ToneStopped {
                fixture: FixtureId(1),
                tone: ToneId(2),
            }),
        ));
        h.receipts(1);
    }
    h.submit(play(5));
    h.sent(5);
    h.port.cancel();
    assert_eq!(
        h.receipts(1)[0].message.result,
        Err(FixtureError::ChannelClosed)
    );
    h.retired();
    assert!(!h.shared.lock().unwrap().tone);
    h.send(event(
        Some(5),
        5,
        Ok(FixtureEvent::ToneStarted {
            fixture: FixtureId(1),
            tone: ToneId(5),
        }),
    ));
    assert!(h.port.poll().is_empty());
}

#[test]
fn ids_and_sequence_never_wrap_and_time_overflow_invalidates() {
    let mut h = Harness::new(true);
    h.submit(call(
        u64::MAX,
        FixtureCommand::Open {
            machine_label: "Test Mac".into(),
        },
    ));
    assert!(h.receipts(1)[0].message.result.is_ok());
    assert_eq!(
        h.port.submit(call(
            u64::MAX,
            FixtureCommand::Close {
                fixture: FixtureId(u64::MAX)
            }
        )),
        Err(FixtureError::CounterExhausted)
    );
    assert_eq!(
        h.receipts(1)[0].message.result,
        Err(FixtureError::CounterExhausted)
    );
    h.retired();
    let mut h = Harness::new(true);
    h.opened();
    h.send(event(None, u64::MAX, Ok(snapshot(None, 0, 0))));
    assert_eq!(
        h.receipts(1)[0].message.result,
        Err(FixtureError::CounterExhausted)
    );
    h.retired();
    let mut h = Harness::new(false);
    h.clock.store(u64::MAX - 10, Ordering::SeqCst);
    assert_eq!(h.port.submit(open()), Err(FixtureError::CounterExhausted));
    h.retired();
    assert!(h.shared.lock().unwrap().calls.is_empty());
}

#[test]
fn owned_child_retired_once_on_drop_invalid_pid_and_write_failure() {
    let shared = Arc::new(Mutex::new(Fake::default()));
    let clock = Arc::new(AtomicU64::new(0));
    assert!(matches!(
        PipeFixturePort::new(
            Box::new(Child {
                shared: shared.clone(),
                clock: clock.clone(),
                pid: 0
            }),
            Arc::new(|| 0)
        ),
        Err(FixtureError::NotOwned)
    ));
    assert_eq!(shared.lock().unwrap().retired, 1);
    let mut h = Harness::new(false);
    h.shared.lock().unwrap().write_error = true;
    h.submit(open());
    assert_eq!(
        h.receipts(1)[0].message.result,
        Err(FixtureError::ChannelClosed)
    );
    h.retired();
    h.port.cancel();
    assert_eq!(h.shared.lock().unwrap().retired, 1);
}

#[test]
fn dropping_port_itself_without_cancel_stops_and_reaps_only_its_owned_child() {
    let shared = Arc::new(Mutex::new(Fake {
        auto_open: true,
        ..Fake::default()
    }));
    let clock = Arc::new(AtomicU64::new(10));
    let now = clock.clone();
    let mut port = PipeFixturePort::new(
        Box::new(Child {
            shared: shared.clone(),
            clock,
            pid: PID,
        }),
        Arc::new(move || now.load(Ordering::SeqCst)),
    )
    .unwrap();
    assert_eq!(port.submit(open()), Ok(()));
    until(|| !port.poll().is_empty());
    let peer = NodeId([5; 32]);
    let mut accepted = false;
    until(|| {
        accepted = port
            .submit(call(
                2,
                FixtureCommand::PlayTone {
                    fixture: FixtureId(1),
                    output: SpeakersSelection {
                        peer,
                        device_key: format!("crosspane.{peer}.speaker"),
                    },
                },
            ))
            .is_ok();
        accepted
    });
    assert!(accepted);
    until(|| shared.lock().unwrap().tone);
    drop(port); // No Harness::drop and no explicit cancel may mask this contract.
    until(|| shared.lock().unwrap().retired == 1);
    assert!(!shared.lock().unwrap().tone);
    assert_eq!(shared.lock().unwrap().calls.len(), 2);
}

#[test]
fn packet_and_debug_surface_has_no_text_input_coordinates_or_raw_messages() {
    let p = event(None, 1, Ok(snapshot(None, 1, 1)));
    for encoded in [
        String::from_utf8(encode_event(&p).unwrap()).unwrap(),
        format!("{p:?}"),
    ] {
        for sensitive in ["TEST_TEXT_SENTINEL", "key_code", "pointer", "pcm", "text"] {
            assert!(!encoded.contains(sensitive));
        }
    }
    let v = json!({"schema_version":1,"message":{"call_id":null,"attempt":7,"sequence":1,"result":{"Ok":{"event":"snapshot","snapshot":{"fixture":1,"window":9001,"phase":null,"pattern_ticks":1,"target_clicks":1,"window_facts":{"state":"unknown"},"tone":{"state":"stopped"},"text":"TEST_TEXT_SENTINEL"}}}}});
    assert!(decode_event(&serde_json::to_vec(&v).unwrap()).is_err());
}

fn play(id: u64) -> FixtureCall {
    let peer = NodeId([3; 32]);
    call(
        id,
        FixtureCommand::PlayTone {
            fixture: FixtureId(1),
            output: SpeakersSelection {
                peer,
                device_key: format!("crosspane.{peer}.speaker"),
            },
        },
    )
}

#[test]
fn stale_idempotent_stop_reply_cannot_clear_a_newer_running_tone() {
    let mut h = Harness::new(true);
    h.opened();
    h.submit(play(2));
    h.sent(2);
    h.send(event(
        Some(2),
        2,
        Ok(FixtureEvent::ToneStarted {
            fixture: FixtureId(1),
            tone: ToneId(2),
        }),
    ));
    h.receipts(1);
    h.submit(call(
        3,
        FixtureCommand::StopTone {
            fixture: FixtureId(1),
            tone: ToneId(2),
        },
    ));
    h.sent(3);
    h.send(event(
        Some(3),
        3,
        Ok(FixtureEvent::ToneStopped {
            fixture: FixtureId(1),
            tone: ToneId(2),
        }),
    ));
    h.receipts(1);
    h.submit(play(4));
    h.submit(call(
        5,
        FixtureCommand::StopTone {
            fixture: FixtureId(1),
            tone: ToneId(2),
        },
    ));
    h.sent(5);
    h.send(event(
        Some(4),
        4,
        Ok(FixtureEvent::ToneStarted {
            fixture: FixtureId(1),
            tone: ToneId(4),
        }),
    ));
    h.send(event(
        Some(5),
        5,
        Ok(FixtureEvent::ToneStopped {
            fixture: FixtureId(1),
            tone: ToneId(2),
        }),
    ));
    assert!(h.receipts(2).iter().all(|r| r.message.result.is_ok()));
    assert_eq!(h.port.submit(play(6)), Err(FixtureError::NotOwned));
    assert_eq!(h.shared.lock().unwrap().calls.len(), 5);
}

// Leave 32 ordinary replies unpolled and the lifecycle command still pending.
fn saturate(h: &mut Harness, first: u64, command: FixtureCommand) -> (u64, u64) {
    let lifecycle_call = first + 31;
    for id in first..lifecycle_call {
        h.submit(call(
            id,
            FixtureCommand::ObserveWindow {
                fixture: FixtureId(1),
            },
        ));
    }
    h.submit(call(lifecycle_call, command));
    h.sent(lifecycle_call);
    for id in first..lifecycle_call {
        h.send(event(Some(id), id, Ok(snapshot(None, id, 0))));
    }
    h.send(event(
        None,
        lifecycle_call,
        Ok(snapshot(None, lifecycle_call, 0)),
    ));
    until(|| h.shared.lock().unwrap().frames as u64 == lifecycle_call);
    (lifecycle_call, lifecycle_call + 1)
}

#[test]
fn saturated_close_requested_and_tone_stopped_survive_cancellation_and_following_lost() {
    for tone in [false, true] {
        for lost in [false, true] {
            let mut h = Harness::new(true);
            h.opened();
            if tone {
                h.submit(play(2));
                h.sent(2);
                h.send(event(
                    Some(2),
                    2,
                    Ok(FixtureEvent::ToneStarted {
                        fixture: FixtureId(1),
                        tone: ToneId(2),
                    }),
                ));
                h.receipts(1);
            }
            let (id, seq) = saturate(
                &mut h,
                if tone { 3 } else { 2 },
                if tone {
                    FixtureCommand::StopTone {
                        fixture: FixtureId(1),
                        tone: ToneId(2),
                    }
                } else {
                    FixtureCommand::Close {
                        fixture: FixtureId(1),
                    }
                },
            );
            let lifecycle = if tone {
                FixtureEvent::ToneStopped {
                    fixture: FixtureId(1),
                    tone: ToneId(2),
                }
            } else {
                FixtureEvent::CloseRequested {
                    fixture: FixtureId(1),
                }
            };
            h.clock.store(23, Ordering::SeqCst);
            h.send(event(Some(id), seq, Ok(lifecycle.clone())));
            until(|| h.shared.lock().unwrap().frames as u64 == seq);
            if lost {
                h.send(event(
                    None,
                    seq + 1,
                    Ok(FixtureEvent::Lost {
                        fixture: FixtureId(1),
                        reason: FixtureError::OutputChanged,
                    }),
                ));
            } else {
                // Complete receipt must remain retained even if retirement wins the next loop.
                thread::sleep(Duration::from_millis(5));
                h.port.cancel();
            }
            h.retired(); // No poll may be needed to discover the following terminal packet.
            let expected = 34 + usize::from(lost && !tone);
            let all = h.receipts(expected);
            assert!(
                all.windows(2)
                    .all(|w| w[0].message.sequence < w[1].message.sequence)
            );
            let retained = all
                .iter()
                .find(|r| r.message.result == Ok(lifecycle.clone()))
                .unwrap();
            assert_eq!(retained.message.call_id, Some(id));
            assert_eq!(retained.received_at_ms, 23);
            assert_eq!(retained.message.sequence, seq);
            if lost {
                assert!(
                    all.iter()
                        .any(|r| matches!(r.message.result, Ok(FixtureEvent::Lost { .. })))
                );
            } else {
                assert_eq!(
                    all.last().unwrap().message.result,
                    Err(FixtureError::ChannelClosed)
                );
            }
            assert!(h.port.poll().is_empty());
        }
    }
}

#[test]
fn saturated_close_requested_keeps_its_deadline_and_retained_progress() {
    let mut h = Harness::new(true);
    h.opened();
    let (id, seq) = saturate(
        &mut h,
        2,
        FixtureCommand::Close {
            fixture: FixtureId(1),
        },
    );
    h.send(event(
        Some(id),
        seq,
        Ok(FixtureEvent::CloseRequested {
            fixture: FixtureId(1),
        }),
    ));
    until(|| h.shared.lock().unwrap().frames as u64 == seq);
    thread::sleep(Duration::from_millis(5));
    h.clock.store(2010, Ordering::SeqCst);
    h.retired(); // A queued progress acknowledgement never exempts Close from its two seconds.
    let all = h.receipts(34);
    assert_eq!(
        all[32].message.result,
        Ok(FixtureEvent::CloseRequested {
            fixture: FixtureId(1)
        })
    );
    assert_eq!(all[32].received_at_ms, 10);
    assert_eq!(all[33].message.call_id, Some(id));
    assert_eq!(all[33].message.result, Err(FixtureError::TimedOut));
    assert!(
        !all.iter()
            .any(|r| matches!(r.message.result, Ok(FixtureEvent::Closed { .. })))
    );
}

#[test]
fn one_complete_line_beyond_ready_capacity_keeps_reading_reserved_lifecycle_and_loss() {
    let mut h = Harness::new(true);
    h.opened();
    h.submit(play(2));
    h.sent(2);
    h.send(event(
        Some(2),
        2,
        Ok(FixtureEvent::ToneStarted {
            fixture: FixtureId(1),
            tone: ToneId(2),
        }),
    ));
    h.receipts(1);
    for id in 3..=34 {
        h.submit(call(
            id,
            FixtureCommand::ObserveWindow {
                fixture: FixtureId(1),
            },
        ));
    }
    h.sent(34);
    for id in 3..=34 {
        h.send(event(Some(id), id, Ok(snapshot(None, id, 0))));
    }
    // An additional unsolicited data line occupies the existing bounded in-flight allowance.
    h.send(event(None, 35, Ok(snapshot(None, 35, 0))));
    h.send(event(
        None,
        36,
        Ok(FixtureEvent::ToneStopped {
            fixture: FixtureId(1),
            tone: ToneId(2),
        }),
    ));
    h.send(event(
        None,
        37,
        Ok(FixtureEvent::CloseRequested {
            fixture: FixtureId(1),
        }),
    ));
    h.send(event(
        None,
        38,
        Ok(FixtureEvent::Lost {
            fixture: FixtureId(1),
            reason: FixtureError::CleanupFailed,
        }),
    ));
    h.retired(); // Reading and owned retirement must not depend on draining the data backlog.
    let first = h.port.poll_receipts();
    assert_eq!(first.len(), MAX_QUEUE);
    let rest = h.receipts(4);
    let mut all = first;
    all.extend(rest);
    assert_eq!(all.len(), 36);
    assert!(
        all.windows(2)
            .all(|w| w[0].message.sequence < w[1].message.sequence)
    );
    assert_eq!(all[32].message.result, Ok(snapshot(None, 35, 0)));
    assert!(matches!(
        all[33].message.result,
        Ok(FixtureEvent::ToneStopped { .. })
    ));
    assert!(matches!(
        all[34].message.result,
        Ok(FixtureEvent::CloseRequested { .. })
    ));
    assert!(matches!(
        all[35].message.result,
        Ok(FixtureEvent::Lost { .. })
    ));
    assert!(h.port.poll().is_empty());
}

#[test]
fn buffered_old_phase_snapshot_survives_requested_arm_before_write_and_acknowledgement() {
    let mut h = Harness::new(true);
    h.opened();
    h.submit(call(
        2,
        FixtureCommand::ArmTarget {
            fixture: FixtureId(1),
            phase: PhaseId(10),
        },
    ));
    h.sent(2);
    h.send(event(
        Some(2),
        2,
        Ok(FixtureEvent::TargetArmed {
            fixture: FixtureId(1),
            phase: PhaseId(10),
        }),
    ));
    h.receipts(1);
    {
        let mut f = h.shared.lock().unwrap();
        f.block_read = true;
        f.block_write = true;
    }
    let old = snapshot(Some(PhaseId(10)), 8, 3);
    h.send(event(None, 3, Ok(old.clone())));
    h.submit(call(
        3,
        FixtureCommand::ArmTarget {
            fixture: FixtureId(1),
            phase: PhaseId(11),
        },
    ));
    assert_eq!(
        h.port.submit(call(
            4,
            FixtureCommand::ArmTarget {
                fixture: FixtureId(1),
                phase: PhaseId(11),
            }
        )),
        Err(FixtureError::NotOwned)
    );
    h.shared.lock().unwrap().block_read = false;
    assert_eq!(h.receipts(1)[0].message.result, Ok(old));
    assert_eq!(h.shared.lock().unwrap().calls.len(), 2);
    assert_eq!(h.shared.lock().unwrap().retired, 0);
    h.shared.lock().unwrap().block_write = false;
    h.sent(3);
    h.send(event(
        Some(3),
        4,
        Ok(FixtureEvent::TargetArmed {
            fixture: FixtureId(1),
            phase: PhaseId(11),
        }),
    ));
    h.receipts(1);
    let new = snapshot(Some(PhaseId(11)), 9, 3);
    h.send(event(None, 5, Ok(new.clone())));
    assert_eq!(h.receipts(1)[0].message.result, Ok(new));
}
