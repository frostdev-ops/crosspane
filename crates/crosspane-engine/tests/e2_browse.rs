#![allow(clippy::unwrap_used, clippy::expect_used)]

use crosspane_engine::e2::E2;
use crosspane_engine::{
    Command, EngineConfig, InjectCmd, Input, Notice, Output, ProjectionKey, ProxyEvent,
};
use crosspane_input::journal::MemoryJournal;
use crosspane_platform::{
    CaptureTarget, LockState, Parked, ParkingKind as PlatformParking, SessionEvent, SessionState,
    StreamId, WindowEvent, WindowInfo, WindowRole, WindowState,
};
use crosspane_protocol::link::{LinkError, LinkEvent};
use crosspane_protocol::msg::{Capability, ControlMessage, Refusal};
use crosspane_protocol::projection::{
    BrowsableWindow, MAX_BROWSE_WINDOWS, ParkingKind, ProjectionEndReason as Reason,
    ProjectionMessage as Message, WindowSummary,
};
use crosspane_types::color::ColorSpace;
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::{
    DisplayGeometry, PixelRect, PixelSize, PointLogical, RectLogical, SizeLogical, SizeMm,
};
use crosspane_types::hid::HidUsage;
use crosspane_types::id::{DisplayId, NodeId, ProjectionId, WindowId};
use crosspane_types::time::MonoTime;

const A: NodeId = NodeId([1; 32]);
const B: NodeId = NodeId([2; 32]);
const C: NodeId = NodeId([3; 32]);
const DISPLAY: DisplayId = DisplayId(4);
const WINDOW: WindowId = WindowId(10);
const ID: ProjectionId = ProjectionId(1);
const OPEN: SessionState = SessionState {
    lock: LockState::Unlocked,
    active: Some(true),
};
const GRANTS: [Capability; 3] = [
    Capability::WindowBrowse,
    Capability::WindowShare,
    Capability::WindowPresent,
];

fn size() -> PixelSize {
    PixelSize::new(641, 481)
}

fn key() -> ProjectionKey {
    ProjectionKey {
        source: A,
        projection: ID,
    }
}

fn window(id: WindowId) -> WindowInfo {
    WindowInfo {
        id,
        title: "fixture".into(),
        app_id: "test".into(),
        pid: None,
        display: Some(DISPLAY),
        frame: RectLogical::new(PointLogical::zero(), SizeLogical::new(320.25, 240.25)),
        state: WindowState::Normal,
        role: WindowRole::Toplevel,
        parent: None,
    }
}

fn parked() -> Parked {
    Parked {
        fullscreen: false,
        window: WINDOW,
        kind: PlatformParking::Twin,
        display: DISPLAY,
        content: PixelRect::new(
            crosspane_types::geom::euclid::Point2D::new(0, 0),
            crosspane_types::geom::euclid::Point2D::new(641, 481),
        ),
    }
}

fn sent(peer: NodeId, msg: Message) -> Output {
    Output::SendControl {
        peer,
        msg: ControlMessage::Projection(msg),
    }
}

fn control(peer: NodeId, msg: Message) -> Input {
    Input::Link(LinkEvent::Control {
        peer,
        msg: ControlMessage::Projection(msg),
    })
}

struct Fixture {
    node: NodeId,
    e2: E2,
}

impl Fixture {
    fn ready(node: NodeId, peer: NodeId) -> Self {
        let (e2, out) = E2::new(
            &EngineConfig::new(node),
            Box::new(MemoryJournal::default()),
            MonoTime::ZERO,
        )
        .unwrap();
        assert!(out.is_empty());
        let mut f = Self { node, e2 };
        f.handle(Input::Session(SessionEvent::State(OPEN)));
        f.handle(Input::PeerUp { peer });
        f.grants(peer, &GRANTS);
        f.handle(Input::LocalDisplays(vec![DisplayInfo {
            id: DISPLAY,
            name: "test".into(),
            geometry: DisplayGeometry {
                physical_size: SizeMm::new(200.0, 100.0),
                pixel_size: size(),
                scale: 2.0,
                logical_origin: PointLogical::zero(),
            },
            refresh_millihz: 60_000,
            color_space: ColorSpace::Srgb,
            hdr: false,
        }]));
        f
    }

    fn handle(&mut self, input: Input) -> Vec<Output> {
        let mut out = Vec::new();
        self.e2.handle(&input, MonoTime::ZERO, &mut out);
        out
    }

    fn grants(&mut self, peer: NodeId, grants: &[Capability]) -> Vec<Output> {
        self.handle(Input::Grants(
            [(peer, grants.iter().copied().collect())].into(),
        ))
    }

    fn receive(&mut self, peer: NodeId, outputs: &[Output]) -> Vec<Output> {
        let mut out = Vec::new();
        for output in outputs {
            match output {
                Output::SendControl { peer: to, msg } => {
                    assert_eq!(*to, self.node);
                    out.extend(self.handle(Input::Link(LinkEvent::Control {
                        peer,
                        msg: msg.clone(),
                    })));
                }
                Output::SendInput { peer: to, msg } => {
                    assert_eq!(*to, self.node);
                    out.extend(self.handle(Input::Link(LinkEvent::Input {
                        peer,
                        msg: msg.clone(),
                    })));
                }
                _ => {}
            }
        }
        out
    }
}

#[test]
fn browse_round_trip_filters_sorts_caps_and_trims_windows() {
    let mut source = Fixture::ready(A, B);
    let mut destination = Fixture::ready(B, A);
    for id in (1..=300).rev() {
        let mut info = window(WindowId(id));
        info.role = match id {
            1 => WindowRole::Popup,
            2 => WindowRole::Other,
            5 => WindowRole::Dialog,
            _ => WindowRole::Toplevel,
        };
        if id == 5 {
            info.title = "é".repeat(600);
            info.app_id = format!("{}🪟", "a".repeat(1023));
        }
        source.handle(Input::Windows(WindowEvent::Added(info)));
    }
    source.handle(Input::PeerUp { peer: C });
    source.handle(Input::Grants(
        [(B, GRANTS.into()), (C, GRANTS.into())].into(),
    ));
    // A projection to any peer makes the window unavailable to this requester.
    source.handle(Input::Command(Command::Project {
        window: WindowId(3),
        to: C,
        place: None,
    }));
    source.handle(Input::Command(Command::Project {
        window: WindowId(4),
        to: B,
        place: None,
    }));
    source.handle(control(
        B,
        Message::Accepted {
            projection: ProjectionId(2),
            size: size(),
            scale: 2.0,
        },
    ));
    source.handle(Input::Command(Command::Return(ProjectionKey {
        source: A,
        projection: ProjectionId(2),
    })));
    // Window 4 now has only a pending parking cleanup, with no source entry.
    let request = 77;
    let query = destination.handle(Input::Command(Command::Browse { peer: A, request }));
    assert_eq!(query, vec![sent(A, Message::ListWindows { request })]);
    let expected: Vec<_> = (5..=260)
        .map(|id| BrowsableWindow {
            window: WindowId(id),
            summary: if id == 5 {
                WindowSummary {
                    title: "é".repeat(512),
                    app_id: "a".repeat(1023),
                }
            } else {
                WindowSummary {
                    title: "fixture".into(),
                    app_id: "test".into(),
                }
            },
            size: size(),
        })
        .collect();
    assert_eq!(expected.len(), MAX_BROWSE_WINDOWS);
    let answer = source.receive(B, &query);
    assert_eq!(
        answer,
        vec![sent(
            B,
            Message::WindowList {
                request,
                windows: expected.clone()
            }
        )]
    );
    assert_eq!(
        destination.receive(A, &answer),
        vec![Output::BrowseResult {
            peer: A,
            request,
            result: Ok(expected)
        }]
    );
    assert_eq!(destination.e2.next_deadline(), None);
}

#[test]
fn browse_and_pull_require_both_grants() {
    for grants in [
        vec![],
        vec![Capability::WindowShare],
        vec![Capability::WindowBrowse],
    ] {
        let mut source = Fixture::ready(A, B);
        let mut destination = Fixture::ready(B, A);
        source.handle(Input::Windows(WindowEvent::Added(window(WINDOW))));
        source.grants(B, &grants);
        for msg in [
            Message::ListWindows { request: 3 },
            Message::Pull {
                request: 3,
                window: WINDOW,
            },
        ] {
            let answer = source.handle(control(B, msg));
            assert_eq!(
                answer,
                vec![sent(
                    B,
                    Message::BrowseRefused {
                        request: 3,
                        reason: Refusal::Permission
                    }
                )]
            );
            assert_eq!(
                destination.receive(A, &answer),
                vec![Output::BrowseResult {
                    peer: A,
                    request: 3,
                    result: Err(Refusal::Permission)
                }]
            );
            assert_eq!(source.e2.next_deadline(), None);
        }
    }
}

#[test]
fn browse_and_pull_refuse_blocked_sessions() {
    for blocked in [
        Input::Session(SessionEvent::State(SessionState {
            lock: LockState::Locked,
            ..OPEN
        })),
        Input::Session(SessionEvent::WillSleep),
        Input::Session(SessionEvent::Woke),
        Input::Session(SessionEvent::State(SessionState {
            active: None,
            ..OPEN
        })),
        Input::Command(Command::Panic),
    ] {
        let mut source = Fixture::ready(A, B);
        source.handle(Input::Windows(WindowEvent::Added(window(WINDOW))));
        source.handle(blocked);
        for msg in [
            Message::ListWindows { request: 4 },
            Message::Pull {
                request: 4,
                window: WINDOW,
            },
        ] {
            assert_eq!(
                source.handle(control(B, msg)),
                vec![sent(
                    B,
                    Message::BrowseRefused {
                        request: 4,
                        reason: Refusal::Locked
                    }
                )]
            );
            assert_eq!(source.e2.next_deadline(), None);
        }
    }
}

#[test]
fn pull_refusals_preserve_projection_notices() {
    let mut source = Fixture::ready(A, B);
    let pull = control(
        B,
        Message::Pull {
            request: 5,
            window: WINDOW,
        },
    );
    let expected = vec![
        Output::Notice(Notice::ProjectionRefused {
            peer: B,
            reason: Refusal::Busy,
        }),
        sent(
            B,
            Message::BrowseRefused {
                request: 5,
                reason: Refusal::Busy,
            },
        ),
    ];
    assert_eq!(source.handle(pull.clone()), expected); // unknown window
    source.handle(Input::Windows(WindowEvent::Added(window(WINDOW))));
    source.handle(Input::Command(Command::Project {
        window: WINDOW,
        to: B,
        place: None,
    }));
    assert_eq!(source.handle(pull.clone()), expected); // already projected
    source.handle(control(
        B,
        Message::Accepted {
            projection: ID,
            size: size(),
            scale: 2.0,
        },
    ));
    source.handle(Input::Command(Command::Return(key())));
    assert_eq!(source.handle(pull), expected); // parking still pending
}

#[test]
fn browse_and_pull_check_connections_and_forward_uncorrelated_answers() {
    let mut destination = Fixture::ready(B, A);
    for peer in [C, A] {
        if peer == A {
            destination.handle(Input::Link(LinkEvent::Closed {
                peer,
                error: LinkError::Closed,
            }));
        }
        for command in [
            Command::Browse { peer, request: 6 },
            Command::Pull {
                peer,
                window: WINDOW,
                request: 6,
            },
        ] {
            assert_eq!(
                destination.handle(Input::Command(command)),
                vec![Output::BrowseResult {
                    peer,
                    request: 6,
                    result: Err(Refusal::Busy)
                }]
            );
        }
    }
    destination.handle(Input::PeerUp { peer: A });
    destination.handle(Input::Command(Command::Panic));
    destination.grants(A, &[]);
    // Requesting needs only a connection; the source enforces browse grants and session state.
    assert_eq!(
        destination.handle(Input::Command(Command::Browse {
            peer: A,
            request: 7
        })),
        vec![sent(A, Message::ListWindows { request: 7 })]
    );
    assert_eq!(
        destination.handle(Input::Command(Command::Pull {
            peer: A,
            window: WINDOW,
            request: 8
        })),
        vec![sent(
            A,
            Message::Pull {
                request: 8,
                window: WINDOW
            }
        )]
    );
    let windows = vec![BrowsableWindow {
        window: WINDOW,
        summary: WindowSummary {
            title: "remote".into(),
            app_id: "app".into(),
        },
        size: size(),
    }];
    assert_eq!(
        destination.handle(control(
            C,
            Message::WindowList {
                request: u32::MAX,
                windows: windows.clone()
            }
        )),
        vec![Output::BrowseResult {
            peer: C,
            request: u32::MAX,
            result: Ok(windows)
        }]
    );
    assert_eq!(
        destination.handle(control(
            A,
            Message::BrowseRefused {
                request: 0,
                reason: Refusal::InjectorFailed
            }
        )),
        vec![Output::BrowseResult {
            peer: A,
            request: 0,
            result: Err(Refusal::InjectorFailed)
        }]
    );
    assert_eq!(destination.e2.next_deadline(), None);
}

#[test]
fn pull_runs_the_same_projection_path_and_checks_window_present() {
    let mut source = Fixture::ready(A, B);
    let mut local = Fixture::ready(A, B);
    let mut destination = Fixture::ready(B, A);
    for f in [&mut source, &mut local] {
        f.handle(Input::Windows(WindowEvent::Added(window(WINDOW))));
    }
    let query = destination.handle(Input::Command(Command::Pull {
        peer: A,
        window: WINDOW,
        request: 9,
    }));
    assert_eq!(
        query,
        vec![sent(
            A,
            Message::Pull {
                request: 9,
                window: WINDOW
            }
        )]
    );
    let offer = source.receive(B, &query);
    assert_eq!(
        offer,
        local.handle(Input::Command(Command::Project {
            window: WINDOW,
            to: B,
            place: None,
        }))
    );
    assert_eq!(
        offer,
        vec![sent(
            B,
            Message::Start {
                projection: ID,
                window: WindowSummary {
                    title: "fixture".into(),
                    app_id: "test".into()
                },
                size: size()
            }
        )]
    );
    assert_eq!(
        destination.receive(A, &offer),
        vec![Output::OpenProxy {
            key: key(),
            title: "fixture".into(),
            app_id: "test".into(),
            size: size(),
            place: None,
        }]
    );
    let accepted = destination.handle(Input::ProxyOpened {
        key: key(),
        result: Ok((size(), 2.0)),
    });
    let parking = source.receive(B, &accepted);
    assert_eq!(parking, local.receive(B, &accepted));
    assert_eq!(
        parking,
        vec![Output::Park {
            window: WINDOW,
            size: size(),
            scale: 2.0
        }]
    );
    let geometry = source.handle(Input::Parked {
        window: WINDOW,
        result: Ok(parked()),
    });
    assert_eq!(
        geometry,
        local.handle(Input::Parked {
            window: WINDOW,
            result: Ok(parked())
        })
    );
    assert_eq!(
        geometry,
        vec![
            Output::StartCapture {
                projection: ID,
                peer: B,
                target: CaptureTarget::Display(DISPLAY),
                crop: Some(parked().content),
                max_fps: 60
            },
            sent(
                B,
                Message::Geometry {
                    fullscreen: Some(false),
                    projection: ID,
                    size: size(),
                    parking: ParkingKind::Twin,
                    answers: 0
                }
            ),
            Output::Notice(Notice::ProjectionStarted {
                key: key(),
                peer: B,
                parking: ParkingKind::Twin
            }),
        ]
    );
    assert_eq!(
        destination.receive(A, &geometry),
        vec![Output::ProxyGeometry {
            key: key(),
            size: size(),
            parking: ParkingKind::Twin
        }]
    );
    let captured = Input::CaptureStarted {
        projection: ID,
        result: Ok(StreamId(1)),
    };
    assert_eq!(source.handle(captured.clone()), local.handle(captured));
    source.handle(Input::Windows(WindowEvent::Focused(Some(WINDOW))));
    let press = destination.handle(Input::Proxy {
        key: key(),
        event: ProxyEvent::Key {
            usage: HidUsage::keyboard(4),
            down: true,
        },
    });
    let injected = source.receive(B, &press);
    assert!(matches!(
        injected.as_slice(),
        [Output::Inject {
            cmd: InjectCmd::Key { down: true, .. },
            ..
        }]
    ));
    for output in injected {
        if let Output::Inject { id, .. } = output {
            source.handle(Input::InjectDone { id, ok: true });
        }
    }
    let close = destination.handle(Input::Proxy {
        key: key(),
        event: ProxyEvent::CloseRequested,
    });
    let ended = source.receive(B, &close);
    assert!(ended.contains(&Output::StopCapture {
        stream: StreamId(1)
    }));
    assert!(ended.contains(&Output::Restore {
        window: WINDOW,
        place: None
    }));
    assert!(ended.contains(&Output::Notice(Notice::ProjectionEnded {
        key: key(),
        reason: Reason::Returned
    })));

    let mut blocked = Fixture::ready(B, A);
    blocked.grants(A, &[Capability::WindowBrowse, Capability::WindowShare]);
    assert_eq!(
        blocked.receive(A, &offer),
        vec![sent(
            A,
            Message::Refused {
                projection: ID,
                reason: Refusal::Permission
            }
        )]
    );
}

#[test]
fn revoking_window_share_ends_a_pulled_projection() {
    let mut source = Fixture::ready(A, B);
    source.handle(Input::Windows(WindowEvent::Added(window(WINDOW))));
    source.handle(control(
        B,
        Message::Pull {
            request: 10,
            window: WINDOW,
        },
    ));
    source.handle(control(
        B,
        Message::Accepted {
            projection: ID,
            size: size(),
            scale: 2.0,
        },
    ));
    source.handle(Input::Parked {
        window: WINDOW,
        result: Ok(parked()),
    });
    source.handle(Input::CaptureStarted {
        projection: ID,
        result: Ok(StreamId(1)),
    });
    // Only WindowShare is removed: WindowBrowse remains granted.
    assert_eq!(
        source.grants(B, &[Capability::WindowBrowse, Capability::WindowPresent]),
        vec![
            Output::StopCapture {
                stream: StreamId(1)
            },
            Output::Restore {
                window: WINDOW,
                place: None
            },
            sent(
                B,
                Message::End {
                    projection: ID,
                    reason: Reason::Revoked
                }
            ),
            Output::Notice(Notice::ProjectionEnded {
                key: key(),
                reason: Reason::Revoked
            }),
        ]
    );
    assert_eq!(source.e2.next_deadline(), None);
}
