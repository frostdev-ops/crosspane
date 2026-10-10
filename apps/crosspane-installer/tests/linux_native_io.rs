#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crosspane_installer::{agent_contract::*, platform::linux::native_io::*};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

static ROOT_ID: AtomicU64 = AtomicU64::new(0);

#[test]
fn ufw_network_reads_admit_only_four_exact_commands_and_256k_output() {
    let fixture = Fixture::plain();
    let environment = ChildEnvironment::selected(fixture.io.target(), BTreeMap::new()).unwrap();
    let cases = [
        ("/usr/bin/systemctl", vec!["is-active", "ufw.service"]),
        ("/usr/bin/ip", vec!["-j", "-d", "addr", "show"]),
        ("/usr/bin/ip", vec!["-j", "route", "show", "default"]),
        ("/usr/bin/ip", vec!["-j", "-6", "route", "show", "default"]),
    ];
    for (path, args) in cases {
        let argv: Vec<String> = args.iter().map(|s| (*s).into()).collect();
        let command = CommandSpec::new(
            path.into(),
            argv.clone(),
            environment.clone(),
            MAX_FIREWALL_COMMAND_BYTES,
        )
        .unwrap();
        for bad in [
            [argv.clone(), vec!["--extra".into()]].concat(),
            [vec!["--user".into()], argv.clone()].concat(),
            [vec!["--system".into()], argv.clone()].concat(),
            argv[..argv.len() - 1].to_vec(),
        ] {
            assert!(CommandSpec::new(path.into(), bad, environment.clone(), 256).is_err());
        }
        for bad_path in [
            path.trim_start_matches('/').to_owned(),
            path.replace("/usr/bin/", "/bin/"),
        ] {
            assert!(
                CommandSpec::new(bad_path.into(), argv.clone(), environment.clone(), 256).is_err()
            );
        }
        assert!(
            CommandSpec::new(
                path.into(),
                argv.clone(),
                environment.clone(),
                MAX_FIREWALL_COMMAND_BYTES + 1
            )
            .is_err()
        );
        *fixture.runner.output.lock().unwrap() =
            Some((Some(0), vec![b'x'; MAX_FIREWALL_COMMAND_BYTES], vec![]));
        assert_eq!(
            fixture.io.run(&command, &deadline()).unwrap().stdout.len(),
            MAX_FIREWALL_COMMAND_BYTES
        );
        let calls = fixture.runner.calls.lock().unwrap();
        assert_eq!(calls.last().unwrap().argv(), argv);
        assert_eq!(
            calls.last().unwrap().executable(),
            std::path::Path::new(path)
        );
        drop(calls);
        *fixture.runner.output.lock().unwrap() =
            Some((Some(0), vec![0; MAX_FIREWALL_COMMAND_BYTES], vec![0]));
        assert_eq!(
            fixture.io.run(&command, &deadline()).unwrap_err(),
            NativeError::Oversize
        );
    }
    for args in [
        vec!["is-active", "foreign.service"],
        vec!["status", "ufw.service"],
        vec!["start", "ufw.service"],
        vec!["is-active", "ufw"],
    ] {
        assert!(
            CommandSpec::new(
                "/usr/bin/systemctl".into(),
                args.iter().map(|s| (*s).into()).collect(),
                environment.clone(),
                256
            )
            .is_err()
        );
    }
    for args in [
        vec!["-j", "addr", "show"],
        vec!["-j", "route", "show"],
        vec!["-j", "-4", "route", "show", "default"],
        vec!["-j", "route", "add", "default"],
    ] {
        assert!(
            CommandSpec::new(
                "/usr/bin/ip".into(),
                args.iter().map(|s| (*s).into()).collect(),
                environment.clone(),
                256
            )
            .is_err()
        );
    }
    assert!(
        CommandSpec::new(
            "/usr/bin/fc-match".into(),
            vec!["-f".into(), "%{file}".into(), "sans-serif".into()],
            environment.clone(),
            MAX_COMMAND_BYTES + 1
        )
        .is_err()
    );
    for path in [
        "/usr/bin/pkexec",
        "/usr/bin/sudo",
        "/usr/bin/setsid",
        "/usr/bin/ufw",
    ] {
        assert!(
            CommandSpec::new(
                path.into(),
                vec!["/usr/bin/ufw".into()],
                environment.clone(),
                256
            )
            .is_err()
        );
    }
}

#[test]
fn lan_cidr_canonical_network_validation_table() {
    for (input, expected) in [
        ("192.168.0.0/16", "192.168.0.0/16"),
        ("10.0.0.0/8", "10.0.0.0/8"),
        ("192.168.3.99/32", "192.168.3.99/32"),
        ("FD00:0000:0000:0000:0000:0000:0000:0000/16", "fd00::/16"),
        ("2001:db8::/32", "2001:db8::/32"),
        ("fd00::1/128", "fd00::1/128"),
    ] {
        assert_eq!(LanCidr::parse(input).unwrap().as_str(), expected);
    }
    for input in [
        "",
        "10.0.0.0",
        "10.0.0.1/24",
        "10.0.0.0/7",
        "10.0.0.0/33",
        "10.0.0.0/024",
        "10.0.0.0/-1",
        "10.0.0.0/24/extra",
        "10.0.0.0/24\n",
        "0.0.0.0/0",
        "0.0.0.0/8",
        "127.0.0.0/8",
        "224.0.0.0/8",
        "169.254.0.0/16",
        "169.254.1.1/32",
        "::/16",
        "::1/128",
        "ff00::/16",
        "fe80::/16",
        "febf::/16",
        "fd00::/15",
        "fd00::/129",
        "fd00::1/64",
        "fd00::/0",
        "fd00::%eth0/16",
        "10.0.0.0/24;evil",
    ] {
        assert!(LanCidr::parse(input).is_err(), "{input}");
    }
}

static PKEXEC_TESTS: Mutex<()> = Mutex::new(());
#[derive(Default)]
struct PkexecState {
    calls: Mutex<Vec<(PkexecCommand, Deadline)>>,
    outcome: Mutex<Option<PkexecOutcome>>,
    entered: std::sync::atomic::AtomicBool,
    stall_spawn: std::sync::atomic::AtomicBool,
    reaped: std::sync::atomic::AtomicBool,
    dropped: std::sync::atomic::AtomicBool,
    terms: AtomicU64,
    requires_drain: std::sync::atomic::AtomicBool,
    out_remaining: AtomicU64,
    err_remaining: AtomicU64,
    hold_drop: std::sync::atomic::AtomicBool,
    setup_error: Mutex<Option<NativeError>>,
}
struct FakePkexec(Arc<PkexecState>);
impl PkexecRunner for FakePkexec {
    fn spawn(
        &self,
        command: &PkexecCommand,
        deadline: &Deadline,
    ) -> Result<Box<dyn PkexecChild>, NativeError> {
        self.0
            .calls
            .lock()
            .unwrap()
            .push((command.clone(), deadline.clone()));
        self.0.entered.store(true, Ordering::Release);
        while self.0.stall_spawn.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(1));
        }
        Ok(Box::new(FakePkexecChild(self.0.clone())))
    }
}
struct FakePkexecChild(Arc<PkexecState>);
impl PkexecChild for FakePkexecChild {
    fn poll(&mut self) -> Result<Option<PkexecOutcome>, NativeError> {
        if let Some(error) = self.0.setup_error.lock().unwrap().take() {
            return Err(error);
        }
        if self.0.requires_drain.load(Ordering::Acquire)
            && self.0.terms.load(Ordering::Acquire) != 0
        {
            // An EPERM-like terminate changes no child state. Both fake pipes must keep flowing.
            for pipe in [&self.0.out_remaining, &self.0.err_remaining] {
                let _ = pipe.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                    Some(n.saturating_sub(4096))
                });
            }
            if self.0.out_remaining.load(Ordering::Acquire) == 0
                && self.0.err_remaining.load(Ordering::Acquire) == 0
            {
                self.0.reaped.store(true, Ordering::Release);
            }
        }
        let outcome = self.0.outcome.lock().unwrap().take();
        if outcome.is_some() {
            self.0.reaped.store(true, Ordering::Release);
        }
        Ok(outcome)
    }
    fn terminate(&mut self) {
        self.0.terms.fetch_add(1, Ordering::AcqRel);
    }
    fn reaped(&mut self) -> bool {
        self.0.reaped.load(Ordering::Acquire)
    }
}
impl Drop for FakePkexecChild {
    fn drop(&mut self) {
        self.0.dropped.store(true, Ordering::Release);
        while self.0.hold_drop.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(1));
        }
    }
}
fn pkexec_fixture() -> (Fixture, Arc<PkexecState>) {
    let mut fixture = Fixture::plain();
    let state = Arc::new(PkexecState::default());
    fixture
        .io
        .set_scratch_pkexec_runner(Arc::new(FakePkexec(state.clone())))
        .unwrap();
    (fixture, state)
}
fn mutation(delete: bool, rule: UfwRule) -> UfwMutation {
    UfwMutation {
        delete,
        cidr: LanCidr::parse("192.168.7.0/24").unwrap(),
        rule,
    }
}
fn exited(stdout: Vec<u8>, stderr: Vec<u8>) -> PkexecOutcome {
    PkexecOutcome::Exited {
        code: 0,
        stdout,
        stderr,
        stdout_truncated: false,
        stderr_truncated: false,
    }
}
fn wait_fake(mut done: impl FnMut() -> bool) {
    let end = std::time::Instant::now() + Duration::from_secs(2);
    while !done() {
        assert!(std::time::Instant::now() < end, "fake worker stalled");
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn pkexec_ufw_all_four_argv_and_environment_are_exact_and_unclassified() {
    let _serial = PKEXEC_TESTS.lock().unwrap();
    for delete in [false, true] {
        for (rule, port, comment) in [
            (UfwRule::Lan, "47811:47812", "Crosspane (LAN)"),
            (UfwRule::Mdns, "5353", "Crosspane (mDNS)"),
        ] {
            let (fixture, state) = pkexec_fixture();
            *state.outcome.lock().unwrap() = Some(PkexecOutcome::Exited {
                code: 127,
                stdout: b"unclassified stdout".to_vec(),
                stderr: b"unclassified stderr".to_vec(),
                stdout_truncated: false,
                stderr_truncated: false,
            });
            let d = deadline();
            match fixture
                .io
                .pkexec_ufw(&fixture.proof(), mutation(delete, rule), &d)
                .unwrap()
            {
                PkexecOutcome::Exited {
                    code,
                    stdout,
                    stderr,
                    ..
                } => {
                    assert_eq!(code, 127);
                    assert_eq!(stdout, b"unclassified stdout");
                    assert_eq!(stderr, b"unclassified stderr");
                }
                other => panic!("{other:?}"),
            }
            wait_fake(|| state.dropped.load(Ordering::Acquire));
            let calls = state.calls.lock().unwrap();
            assert_eq!(calls.len(), 1);
            let (command, recorded_deadline) = &calls[0];
            let mut argv = vec!["--wait", "/usr/bin/pkexec", "/usr/bin/ufw"];
            if delete {
                argv.push("delete");
            }
            argv.extend([
                "allow",
                "from",
                "192.168.7.0/24",
                "to",
                "any",
                "port",
                port,
                "proto",
                "udp",
                "comment",
                comment,
            ]);
            assert_eq!(
                command.executable(),
                std::path::Path::new("/usr/bin/setsid")
            );
            assert_eq!(command.argv(), argv);
            assert_eq!(
                command.environment(),
                [("PATH", "/usr/bin:/bin"), ("LANG", "C"), ("LC_ALL", "C")]
            );
            assert!(command.stdin_null());
            assert!(command.new_session());
            assert_eq!(command.process_group(), None);
            assert_eq!(format!("{recorded_deadline:?}"), format!("{d:?}"));
            assert_eq!(state.terms.load(Ordering::Acquire), 0);
            assert!(fixture.runner.calls.lock().unwrap().is_empty());
        }
    }
}

#[test]
fn pkexec_ufw_requires_current_selected_proof_and_scratch_override() {
    let _serial = PKEXEC_TESTS.lock().unwrap();
    let (fixture, state) = pkexec_fixture();
    let other = Fixture::plain();
    assert!(matches!(
        other
            .io
            .pkexec_ufw(&other.proof(), mutation(false, UfwRule::Lan), &deadline()),
        Err(NativeError::Foreign)
    ));
    assert!(matches!(
        fixture
            .io
            .pkexec_ufw(&other.proof(), mutation(false, UfwRule::Lan), &deadline()),
        Err(NativeError::Unsupported)
    ));
    let proof = fixture.proof();
    let mut changed = facts(&fixture.io);
    changed.active = false;
    assert_eq!(
        proof.revalidate(&fixture.io, &changed),
        Err(NativeError::Unsupported)
    );
    assert!(matches!(
        fixture
            .io
            .pkexec_ufw(&proof, mutation(false, UfwRule::Lan), &deadline()),
        Err(NativeError::Unsupported)
    ));
    assert_eq!(
        Deadline::new(120_001, Cancellation::default()).unwrap_err(),
        NativeError::Invalid
    );
    let proof = fixture.proof();
    thread::sleep(SUPPORT_LIFETIME + Duration::from_millis(10));
    assert!(matches!(
        fixture
            .io
            .pkexec_ufw(&proof, mutation(false, UfwRule::Lan), &deadline()),
        Err(NativeError::Unsupported)
    ));
    let cancel = Cancellation::default();
    let d = Deadline::new(1000, cancel.clone()).unwrap();
    cancel.cancel();
    assert!(matches!(
        fixture
            .io
            .pkexec_ufw(&fixture.proof(), mutation(false, UfwRule::Lan), &d),
        Err(NativeError::Cancelled)
    ));
    assert!(state.calls.lock().unwrap().is_empty());
    assert!(fixture.runner.calls.lock().unwrap().is_empty());
}

#[test]
fn pkexec_timeout_terms_once_and_retains_process_wide_lease_until_reaped() {
    let _serial = PKEXEC_TESTS.lock().unwrap();
    let (fixture, state) = pkexec_fixture();
    let (other, other_state) = pkexec_fixture();
    let started = std::time::Instant::now();
    assert!(matches!(
        fixture
            .io
            .pkexec_ufw(
                &fixture.proof(),
                mutation(false, UfwRule::Lan),
                &Deadline::new(30, Cancellation::default()).unwrap()
            )
            .unwrap(),
        PkexecOutcome::TimedOut
    ));
    assert!(started.elapsed() < Duration::from_millis(500));
    wait_fake(|| state.terms.load(Ordering::Acquire) == 1);
    for _ in 0..3 {
        assert!(matches!(
            other
                .io
                .pkexec_ufw(&other.proof(), mutation(true, UfwRule::Mdns), &deadline()),
            Err(NativeError::Busy)
        ));
    }
    assert_eq!(state.calls.lock().unwrap().len(), 1);
    assert!(other_state.calls.lock().unwrap().is_empty());
    assert!(!state.dropped.load(Ordering::Acquire));
    state.reaped.store(true, Ordering::Release);
    wait_fake(|| state.dropped.load(Ordering::Acquire));
    *other_state.outcome.lock().unwrap() = Some(exited(vec![], vec![]));
    assert!(matches!(
        other
            .io
            .pkexec_ufw(&other.proof(), mutation(true, UfwRule::Mdns), &deadline())
            .unwrap(),
        PkexecOutcome::Exited { code: 0, .. }
    ));
    wait_fake(|| other_state.dropped.load(Ordering::Acquire));
    assert_eq!(state.terms.load(Ordering::Acquire), 1);
    assert_eq!(other_state.calls.lock().unwrap().len(), 1);
}

#[test]
fn pkexec_stalled_launch_is_bounded_cancelled_and_never_redispatched() {
    let _serial = PKEXEC_TESTS.lock().unwrap();
    let (fixture, state) = pkexec_fixture();
    state.stall_spawn.store(true, Ordering::Release);
    let cancel = Cancellation::default();
    let d = Deadline::new(1000, cancel.clone()).unwrap();
    thread::scope(|scope| {
        let operation = scope.spawn(|| {
            fixture
                .io
                .pkexec_ufw(&fixture.proof(), mutation(false, UfwRule::Lan), &d)
        });
        wait_fake(|| state.entered.load(Ordering::Acquire));
        cancel.cancel();
        assert_eq!(
            operation.join().unwrap().unwrap_err(),
            NativeError::Cancelled
        );
    });
    assert!(matches!(
        fixture
            .io
            .pkexec_ufw(&fixture.proof(), mutation(false, UfwRule::Lan), &deadline()),
        Err(NativeError::Busy)
    ));
    state.stall_spawn.store(false, Ordering::Release);
    wait_fake(|| state.terms.load(Ordering::Acquire) == 1);
    assert!(!state.dropped.load(Ordering::Acquire));
    state.reaped.store(true, Ordering::Release);
    wait_fake(|| state.dropped.load(Ordering::Acquire));
    assert_eq!(state.calls.lock().unwrap().len(), 1);
}

#[test]
fn pkexec_output_bounds_are_per_stream_flagged_and_signal_is_preserved() {
    let _serial = PKEXEC_TESTS.lock().unwrap();
    for (out, err) in [
        (0, 0),
        (MAX_COMMAND_BYTES, MAX_COMMAND_BYTES),
        (MAX_COMMAND_BYTES + 1, MAX_COMMAND_BYTES * 2),
    ] {
        let (fixture, state) = pkexec_fixture();
        *state.outcome.lock().unwrap() = Some(exited(vec![b'o'; out], vec![b'e'; err]));
        match fixture
            .io
            .pkexec_ufw(&fixture.proof(), mutation(false, UfwRule::Lan), &deadline())
            .unwrap()
        {
            PkexecOutcome::Exited {
                stdout,
                stderr,
                stdout_truncated,
                stderr_truncated,
                ..
            } => {
                assert_eq!(stdout.len(), out.min(MAX_COMMAND_BYTES));
                assert_eq!(stderr.len(), err.min(MAX_COMMAND_BYTES));
                assert_eq!(stdout_truncated, out > MAX_COMMAND_BYTES);
                assert_eq!(stderr_truncated, err > MAX_COMMAND_BYTES);
            }
            other => panic!("{other:?}"),
        }
        wait_fake(|| state.dropped.load(Ordering::Acquire));
    }
    let (fixture, state) = pkexec_fixture();
    *state.outcome.lock().unwrap() = Some(PkexecOutcome::Signalled);
    assert!(matches!(
        fixture
            .io
            .pkexec_ufw(&fixture.proof(), mutation(false, UfwRule::Lan), &deadline())
            .unwrap(),
        PkexecOutcome::Signalled
    ));
    wait_fake(|| state.dropped.load(Ordering::Acquire));
    assert_eq!(state.calls.lock().unwrap().len(), 1);
}

#[test]
fn firewall_reads_refuse_admitted_bus_and_manager_environments_before_any_runner() {
    use std::os::unix::net::UnixListener;
    let fixture = Fixture::plain();
    let runtime = fixture.io.target().paths().runtime_home.clone();
    fixture
        .io
        .create_private_dir(&fixture.proof(), &runtime.join("systemd"))
        .unwrap();
    let _manager = UnixListener::bind(runtime.join("systemd/private")).unwrap();
    let bus = runtime.join("bus");
    let _bus = UnixListener::bind(&bus).unwrap();
    let session = BTreeMap::from([(
        "DBUS_SESSION_BUS_ADDRESS".into(),
        format!("unix:path={}", bus.display()),
    )]);
    let environments = [
        fixture
            .io
            .manager_environment(BTreeMap::new(), &deadline())
            .unwrap(),
        ChildEnvironment::selected(fixture.io.target(), session).unwrap(),
    ];
    for environment in environments {
        for (program, args) in [
            ("/usr/bin/systemctl", vec!["is-active", "ufw.service"]),
            ("/usr/bin/ip", vec!["-j", "-d", "addr", "show"]),
            ("/usr/bin/ip", vec!["-j", "route", "show", "default"]),
            ("/usr/bin/ip", vec!["-j", "-6", "route", "show", "default"]),
        ] {
            assert!(
                matches!(
                    CommandSpec::new(
                        program.into(),
                        args.into_iter().map(str::to_owned).collect(),
                        environment.clone(),
                        256
                    ),
                    Err(NativeError::Invalid)
                ),
                "{program}"
            );
        }
        assert!(fixture.runner.calls.lock().unwrap().is_empty());
    }
}

#[test]
fn pkexec_cleanup_keeps_both_pipes_flowing_when_term_cannot_stop_child() {
    let _serial = PKEXEC_TESTS.lock().unwrap();
    let (fixture, state) = pkexec_fixture();
    state.requires_drain.store(true, Ordering::Release);
    state.out_remaining.store(64 * 1024, Ordering::Release);
    state.err_remaining.store(96 * 1024, Ordering::Release);
    assert!(matches!(
        fixture
            .io
            .pkexec_ufw(
                &fixture.proof(),
                mutation(false, UfwRule::Lan),
                &Deadline::new(30, Cancellation::default()).unwrap()
            )
            .unwrap(),
        PkexecOutcome::TimedOut
    ));
    wait_fake(|| state.terms.load(Ordering::Acquire) == 1);
    assert!(matches!(
        fixture
            .io
            .pkexec_ufw(&fixture.proof(), mutation(false, UfwRule::Lan), &deadline()),
        Err(NativeError::Busy)
    ));
    wait_fake(|| state.dropped.load(Ordering::Acquire));
    assert_eq!(state.out_remaining.load(Ordering::Acquire), 0);
    assert_eq!(state.err_remaining.load(Ordering::Acquire), 0);
    assert_eq!(state.terms.load(Ordering::Acquire), 1);
    assert_eq!(state.calls.lock().unwrap().len(), 1);
}

#[test]
fn pkexec_reaped_child_drop_observes_released_lease_on_repeated_dispatches() {
    let _serial = PKEXEC_TESTS.lock().unwrap();
    for _ in 0..20 {
        let (first, state) = pkexec_fixture();
        state.hold_drop.store(true, Ordering::Release);
        *state.outcome.lock().unwrap() = Some(exited(vec![], vec![]));
        assert!(matches!(
            first
                .io
                .pkexec_ufw(&first.proof(), mutation(false, UfwRule::Lan), &deadline())
                .unwrap(),
            PkexecOutcome::Exited { .. }
        ));
        wait_fake(|| state.dropped.load(Ordering::Acquire));
        let (next, next_state) = pkexec_fixture();
        *next_state.outcome.lock().unwrap() = Some(exited(vec![], vec![]));
        let result = next
            .io
            .pkexec_ufw(&next.proof(), mutation(false, UfwRule::Lan), &deadline());
        state.hold_drop.store(false, Ordering::Release);
        assert!(matches!(result.unwrap(), PkexecOutcome::Exited { .. }));
        wait_fake(|| next_state.dropped.load(Ordering::Acquire));
    }
}

#[test]
fn pkexec_setup_error_returns_promptly_and_common_reaper_retains_lease() {
    let _serial = PKEXEC_TESTS.lock().unwrap();
    let (fixture, state) = pkexec_fixture();
    *state.setup_error.lock().unwrap() = Some(NativeError::Unavailable);
    let start = std::time::Instant::now();
    assert_eq!(
        fixture
            .io
            .pkexec_ufw(
                &fixture.proof(),
                mutation(false, UfwRule::Lan),
                &Deadline::new(30, Cancellation::default()).unwrap()
            )
            .unwrap_err(),
        NativeError::Unavailable
    );
    assert!(start.elapsed() < Duration::from_millis(500));
    wait_fake(|| state.terms.load(Ordering::Acquire) == 1);
    assert!(matches!(
        fixture
            .io
            .pkexec_ufw(&fixture.proof(), mutation(false, UfwRule::Lan), &deadline()),
        Err(NativeError::Busy)
    ));
    assert!(!state.dropped.load(Ordering::Acquire));
    state.reaped.store(true, Ordering::Release);
    wait_fake(|| state.dropped.load(Ordering::Acquire));
    assert_eq!(state.terms.load(Ordering::Acquire), 1);
}

#[test]
fn fc_match_is_exactly_allowlisted_and_runs_only_the_injected_runner() {
    let fixture = Fixture::plain();
    let environment = ChildEnvironment::selected(fixture.io.target(), BTreeMap::new()).unwrap();
    let argv = vec!["-f".into(), "%{file}".into(), "sans-serif".into()];
    for (path, args) in [
        ("/bin/fc-match", argv.clone()),
        ("/usr/bin/fc-match", vec!["sans-serif".into()]),
        (
            "/usr/bin/fc-match",
            vec!["-f".into(), "%{family}".into(), "sans-serif".into()],
        ),
        (
            "/usr/bin/fc-match",
            vec!["-f".into(), "%{file}".into(), "serif".into()],
        ),
        (
            "/usr/bin/fc-match",
            vec![
                "-f".into(),
                "%{file}".into(),
                "sans-serif".into(),
                "--verbose".into(),
            ],
        ),
    ] {
        assert!(CommandSpec::new(path.into(), args, environment.clone(), 256).is_err());
    }
    let command =
        CommandSpec::new("/usr/bin/fc-match".into(), argv.clone(), environment, 256).unwrap();
    let answer = b"/usr/share/fonts/fixture.ttf".to_vec();
    *fixture.runner.output.lock().unwrap() = Some((Some(0), answer.clone(), Vec::new()));
    assert_eq!(
        fixture.io.run(&command, &deadline()).unwrap().stdout,
        answer
    );
    let calls = fixture.runner.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].argv(), argv);
    assert_eq!(
        calls[0].executable(),
        std::path::Path::new("/usr/bin/fc-match")
    );
    drop(calls);
    *fixture.runner.output.lock().unwrap() = Some((Some(0), vec![0; 257], Vec::new()));
    assert_eq!(
        fixture.io.run(&command, &deadline()).unwrap_err(),
        NativeError::Oversize
    );
}

#[test]
fn show_environment_is_read_only_pinned_and_drift_discards_fake_output() {
    use std::os::unix::net::UnixListener;
    let fixture = Fixture::plain();
    let root = fixture.io.target().paths().runtime_home.join("systemd");
    fixture
        .io
        .create_private_dir(&fixture.proof(), &root)
        .unwrap();
    let path = root.join("private");
    let _listener = UnixListener::bind(&path).unwrap();
    let environment = fixture
        .io
        .manager_environment(BTreeMap::new(), &deadline())
        .unwrap();
    let args = vec!["--user".into(), "show-environment".into()];
    for args in [
        vec!["show-environment".into()],
        vec!["--system".into(), "show-environment".into()],
        vec![
            "--user".into(),
            "show-environment".into(),
            "foreign.service".into(),
        ],
        vec![
            "--user".into(),
            "show-environment".into(),
            "--host=foreign".into(),
        ],
    ] {
        assert!(
            CommandSpec::new("/usr/bin/systemctl".into(), args, environment.clone(), 256).is_err()
        );
    }
    let command = CommandSpec::new("/usr/bin/systemctl".into(), args, environment, 256).unwrap();
    *fixture.runner.output.lock().unwrap() = Some((
        Some(0),
        b"WAYLAND_DISPLAY=wayland-fixture\n".to_vec(),
        Vec::new(),
    ));
    assert!(fixture.io.run(&command, &deadline()).is_ok());
    let hook_path = path.clone();
    *fixture.runner.hook.lock().unwrap() = Some((
        2,
        Box::new(move || {
            fs::rename(&hook_path, hook_path.with_extension("old")).unwrap();
            let listener = UnixListener::bind(&hook_path).unwrap();
            drop(listener);
        }),
    ));
    assert_eq!(
        fixture.io.run(&command, &deadline()).unwrap_err(),
        NativeError::Foreign
    );
    assert_eq!(fixture.runner.calls.lock().unwrap().len(), 2);
    assert_eq!(
        fixture.io.run(&command, &deadline()).unwrap_err(),
        NativeError::Foreign
    );
    assert_eq!(
        fixture.runner.calls.lock().unwrap().len(),
        2,
        "preflight drift must make zero runner calls"
    );
}
const START: &[u8] = b"Fri Oct  2 12:00:00 2026\n";
type FakeOutput = (Option<i32>, Vec<u8>, Vec<u8>);
type QueryHook = (usize, Box<dyn FnOnce() + Send>);
#[derive(Default)]
struct Runner {
    calls: Mutex<Vec<CommandSpec>>,
    name: Mutex<Option<Vec<u8>>>,
    start: Mutex<Option<Vec<u8>>>,
    oversized: bool,
    output: Mutex<Option<FakeOutput>>,
    query_outputs: Mutex<std::collections::VecDeque<FakeOutput>>,
    hook: Mutex<Option<QueryHook>>,
}
impl CommandRunner for Runner {
    fn run(
        &self,
        command: &CommandSpec,
        deadline: &Deadline,
    ) -> Result<CommandOutput, NativeError> {
        deadline.check()?;
        let count = {
            let mut calls = self.calls.lock().unwrap();
            calls.push(command.clone());
            calls.len()
        };
        let hook = { self.hook.lock().unwrap().take() };
        if let Some((at, hook)) = hook {
            if at == count {
                hook();
            } else {
                *self.hook.lock().unwrap() = Some((at, hook));
            }
        }
        let scripted = self.query_outputs.lock().unwrap().pop_front();
        if let Some((code, stdout, stderr)) =
            scripted.or_else(|| self.output.lock().unwrap().clone())
        {
            return Ok(CommandOutput {
                code,
                stdout,
                stderr,
            });
        }
        let stdout = if self.oversized {
            vec![0; command.output_limit() + 1]
        } else if command.argv()[1] == "lstart=" {
            self.start
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| START.to_vec())
        } else {
            self.name
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| b"crosspane-agent\n".to_vec())
        };
        Ok(CommandOutput {
            code: Some(0),
            stdout,
            stderr: Vec::new(),
        })
    }
}
struct Probe {
    root: PathBuf,
    calls: AtomicU64,
    reuse: bool,
    wrong_uid: bool,
    wrong_exe: bool,
    absent: bool,
    script: Mutex<std::collections::VecDeque<Result<ProcessFacts, NativeError>>>,
}
impl ProcessProbe for Probe {
    fn snapshot(&self, _: u32, deadline: &Deadline) -> Result<ProcessFacts, NativeError> {
        deadline.check()?;
        if let Some(value) = self.script.lock().unwrap().pop_front() {
            return value;
        }
        if self.absent {
            return Err(NativeError::Unavailable);
        }
        Ok(ProcessFacts {
            uid: rustix::process::geteuid().as_raw() + u32::from(self.wrong_uid),
            executable: self.root.join(if self.wrong_exe {
                ".local/bin/foreign"
            } else {
                ".local/bin/crosspane-agent"
            }),
            generation: if self.reuse {
                self.calls.fetch_add(1, Ordering::Relaxed)
            } else {
                77
            },
        })
    }
}
struct Fixture {
    io: LinuxNativeIo,
    runner: Arc<Runner>,
    root: PathBuf,
    probe: Arc<Probe>,
}
impl Fixture {
    fn new(reuse: bool, wrong_uid: bool, wrong_exe: bool, absent: bool, oversized: bool) -> Self {
        let root = PathBuf::from(format!(
            "/tmp/cp47n-{}-{}",
            std::process::id(),
            ROOT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let runner = Arc::new(Runner {
            oversized,
            ..Default::default()
        });
        let probe = Arc::new(Probe {
            root: root.clone(),
            calls: AtomicU64::new(0),
            reuse,
            wrong_uid,
            wrong_exe,
            absent,
            script: Mutex::default(),
        });
        let io = LinuxNativeIo::scratch(&root, runner.clone(), probe.clone()).unwrap();
        let fixture = Self {
            io,
            runner,
            root,
            probe,
        };
        let bin = fixture.root.join(".local/bin");
        fixture
            .io
            .create_private_dir(&fixture.proof(), &bin)
            .unwrap();
        fs::write(fixture.io.target().agent_path(), b"inert test fixture").unwrap();
        fs::set_permissions(
            fixture.io.target().agent_path(),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        fixture
    }
    fn plain() -> Self {
        Self::new(false, false, false, false, false)
    }
    fn proof(&self) -> SupportProof {
        self.io.scratch_support(facts(&self.io)).unwrap()
    }
    fn bootstrap(&self, offset: i64) {
        let proof = self.proof();
        self.io
            .create_private_dir(&proof, self.io.target().runtime_dir())
            .unwrap();
        let started = parse_ps_start(START)
            .unwrap()
            .checked_add_signed(offset)
            .unwrap();
        let bytes = serde_json::to_vec(
            &serde_json::json!({"schema_version":1,"instance_id":9,"pid":4242,
            "started_unix_ms":started,"phase":"waiting_for_keystore","phase_seq":2,"keystore":null,
            "reason":null,"runtime_dir":self.io.target().runtime_dir()}),
        )
        .unwrap();
        self.io
            .atomic_write(
                &proof,
                &self.io.target().runtime_dir().join("bootstrap.json"),
                &bytes,
            )
            .unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}
fn facts(io: &LinuxNativeIo) -> SupportObservations {
    SupportObservations {
        uid: io.target().paths().uid,
        desktop: crosspane_installer::platform::linux::detect::Desktop::Hyprland,
        architecture: std::env::consts::ARCH.into(),
        arch_based: true,
        compositor_version: [0, 56, 0],
        protocols_ready: true,
        runtime_libraries_ready: true,
        compositor_managed: true,
        graphical_target_active: true,
        graphical_sessions: 1,
        session_id: "test-session".into(),
        session_type: "wayland".into(),
        seat: "seat0".into(),
        active: true,
    }
}
fn deadline() -> Deadline {
    Deadline::new(1000, Cancellation::default()).unwrap()
}

#[test]
fn scratch_admission_cannot_be_reused_for_another_or_production_target() {
    let a = Fixture::plain();
    let b = Fixture::plain();
    let proof = a.proof();
    assert_eq!(proof.check(&b.io), Err(NativeError::Unsupported));
    assert_eq!(a.io.target().source(), ObservationSource::Demo);
    assert!(LinuxNativeIo::selected(a.io.target().paths().clone()).is_err());
    assert!(
        LinuxNativeIo::scratch(
            &a.root,
            a.runner.clone(),
            Arc::new(Probe {
                root: a.root.clone(),
                calls: AtomicU64::new(0),
                reuse: false,
                wrong_uid: false,
                wrong_exe: false,
                absent: false,
                script: Mutex::default()
            })
        )
        .is_err()
    );
    assert_eq!(proof.revalidate(&a.io, &facts(&a.io)), Ok(()));
    let mut changed = facts(&a.io);
    changed.session_id = "replaced-session".into();
    assert_eq!(
        proof.revalidate(&a.io, &changed),
        Err(NativeError::Unsupported)
    );
    assert_eq!(proof.check(&a.io), Err(NativeError::Unsupported));
}
#[test]
fn session_authority_fails_closed_without_gating_compatibility() {
    let fixture = Fixture::plain();
    for index in 0..13 {
        let mut f = facts(&fixture.io);
        match index {
            0 => f.arch_based = false,
            1 => f.compositor_version = [0, 55, 9],
            2 => f.protocols_ready = false,
            3 => f.runtime_libraries_ready = false,
            4 => f.compositor_managed = false,
            5 => f.graphical_target_active = false,
            6 => f.graphical_sessions = 0,
            7 => f.graphical_sessions = 2,
            8 => f.session_type = "x11".into(),
            9 => f.seat.clear(),
            10 => f.uid += 1,
            11 => f.active = false,
            _ => f.architecture = "unsupported".into(),
        }
        let admitted = fixture.io.scratch_support(f);
        if matches!(index, 0..=3 | 12) {
            assert!(admitted.is_ok(), "compatibility {index}");
        } else {
            assert!(
                matches!(admitted, Err(NativeError::Unsupported)),
                "authority {index}"
            );
        }
    }
    assert!(fixture.runner.calls.lock().unwrap().is_empty());
    assert!(!fixture.root.join("run").exists());
}
#[test]
fn private_atomic_files_and_lock_exclusion_are_bounded_and_owned() {
    let fixture = Fixture::plain();
    let proof = fixture.proof();
    let state = fixture.root.join(".local/state/crosspane/installer");
    fixture.io.create_private_dir(&proof, &state).unwrap();
    assert_eq!(
        fs::metadata(&state).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let path = state.join("intent.json");
    fixture.io.atomic_write(&proof, &path, b"intent").unwrap();
    assert_eq!(fixture.io.read(&path, 6, true).unwrap(), b"intent");
    assert_eq!(fixture.io.read(&path, 5, true), Err(NativeError::Oversize));
    assert_eq!(
        fixture.io.read(&path, MAX_FILE_BYTES + 1, true),
        Err(NativeError::Invalid)
    );
    let metadata = fixture.io.metadata(&path).unwrap().unwrap();
    assert_eq!(metadata.st_mode & 0o777, 0o600);
    assert!(
        fixture
            .io
            .metadata(&state.join("absent"))
            .unwrap()
            .is_none()
    );
    let lock_path = state.join("install.lock");
    let lock = fixture.io.lock(&proof, &lock_path).unwrap();
    assert!(matches!(
        fixture.io.lock(&proof, &lock_path),
        Err(NativeError::Busy)
    ));
    drop(lock);
    drop(fixture.io.lock(&proof, &lock_path).unwrap());
    fixture.io.atomic_write(&proof, &path, b"outcome").unwrap();
    assert_eq!(fixture.io.read(&path, 7, true).unwrap(), b"outcome");
    assert!(fs::read_dir(&state).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".crosspane-")
    }));
}
#[test]
fn symlinks_hardlinks_foreign_modes_and_traversal_never_authorize_writes() {
    let fixture = Fixture::plain();
    let proof = fixture.proof();
    let dir = fixture.root.join("private");
    fixture.io.create_private_dir(&proof, &dir).unwrap();
    let original = dir.join("original");
    fixture.io.atomic_write(&proof, &original, b"kept").unwrap();
    let link = dir.join("link");
    symlink(&original, &link).unwrap();
    assert_eq!(
        fixture.io.atomic_write(&proof, &link, b"bad"),
        Err(NativeError::Foreign)
    );
    assert!(fixture.io.read(&link, 100, true).is_err());
    let hard = dir.join("hard");
    fs::hard_link(&original, &hard).unwrap();
    assert_eq!(
        fixture.io.atomic_write(&proof, &hard, b"bad"),
        Err(NativeError::Foreign)
    );
    assert_eq!(
        fixture.io.read(&original, 100, true),
        Err(NativeError::Foreign)
    );
    fs::remove_file(hard).unwrap();
    fs::set_permissions(&original, fs::Permissions::from_mode(0o666)).unwrap();
    assert_eq!(
        fixture.io.atomic_write(&proof, &original, b"bad"),
        Err(NativeError::Foreign)
    );
    assert!(
        fixture
            .io
            .read(&dir.join("../private/original"), 100, false)
            .is_err()
    );
    assert!(
        fixture
            .io
            .atomic_write(
                &proof,
                &PathBuf::from("/nonexistent/crosspane-test/outside"),
                b"bad"
            )
            .is_err()
    );
    fs::set_permissions(&original, fs::Permissions::from_mode(0o600)).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(fixture.io.read(&original, 100, false).is_err());
    assert_eq!(fs::read(&original).unwrap(), b"kept");
}
#[test]
fn selected_paths_are_read_only_validated_and_private_runtime_modes_are_required() {
    let fixture = Fixture::plain();
    fixture.bootstrap(0);
    let proof = fixture.proof();
    let path = fixture.io.target().runtime_dir().join("bootstrap.json");
    fs::set_permissions(
        fixture.io.target().runtime_dir(),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert_eq!(
        fixture.io.read(&path, 4096, true),
        Err(NativeError::Foreign)
    );
    fs::set_permissions(
        fixture.io.target().runtime_dir(),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let foreign = fixture.root.join(".config");
    symlink(fixture.root.join("run"), &foreign).unwrap();
    assert_eq!(fixture.io.validate_target(), Err(NativeError::Foreign));
    assert_eq!(
        fixture.io.atomic_write(&proof, &path, b"bad"),
        Err(NativeError::Foreign)
    );
}
#[test]
fn child_commands_have_explicit_environment_bounds_and_no_shell() {
    let fixture = Fixture::plain();
    let other = Fixture::plain();
    let env = ChildEnvironment::selected(fixture.io.target(), BTreeMap::new()).unwrap();
    assert_eq!(env.values()["LC_ALL"], "C");
    assert_eq!(env.values()["TZ"], "UTC");
    for key in [
        "DISPLAY",
        "PULSE_SERVER",
        "PIPEWIRE_REMOTE",
        "CROSSPANE_AUDIO",
    ] {
        assert!(!env.values().contains_key(key));
    }
    assert!(
        ChildEnvironment::selected(
            fixture.io.target(),
            BTreeMap::from([("LD_PRELOAD".into(), "foreign".into())])
        )
        .is_err()
    );
    assert!(
        ChildEnvironment::selected(
            fixture.io.target(),
            BTreeMap::from([(
                "DBUS_SESSION_BUS_ADDRESS".into(),
                "unix:path=/nonexistent/foreign".into()
            )])
        )
        .is_err()
    );
    assert!(
        ChildEnvironment::selected(
            fixture.io.target(),
            BTreeMap::from([("WAYLAND_DISPLAY".into(), "../foreign".into())])
        )
        .is_err()
    );
    for (exe, args, size) in [
        ("/bin/sh", vec![], 1),
        ("relative", vec![], 1),
        ("/bin/ps", vec!["x".repeat(4097)], 1),
        ("/bin/ps", vec!["x".into(); 33], 1),
        ("/bin/ps", vec![], MAX_COMMAND_BYTES + 1),
    ] {
        assert!(CommandSpec::new(exe.into(), args, env.clone(), size).is_err());
    }
    let wrong = ChildEnvironment::selected(other.io.target(), BTreeMap::new()).unwrap();
    let spec = CommandSpec::new(
        "/bin/ps".into(),
        vec!["-o".into(), "comm=".into(), "-p".into(), "4242".into()],
        wrong,
        256,
    )
    .unwrap();
    assert!(matches!(
        fixture.io.run(&spec, &deadline()),
        Err(NativeError::Foreign)
    ));
    assert!(fixture.runner.calls.lock().unwrap().is_empty());
}
#[test]
fn ps_argv_environment_calendar_and_start_tolerance_are_exact() {
    let fixture = Fixture::plain();
    for offset in [-2000, 0, 2000] {
        fixture.bootstrap(offset);
        assert!(fixture.io.bootstrap(&deadline()).is_ok());
    }
    for offset in [-2001, 2001] {
        fixture.bootstrap(offset);
        assert!(fixture.io.bootstrap(&deadline()).is_err());
    }
    for call in fixture.runner.calls.lock().unwrap().iter() {
        assert_eq!(call.executable(), std::path::Path::new("/bin/ps"));
        assert!(
            call.argv() == ["-o", "lstart=", "-p", "4242"]
                || call.argv() == ["-o", "comm=", "-p", "4242"]
        );
        assert_eq!(call.environment().values()["LC_ALL"], "C");
        assert_eq!(call.environment().values()["TZ"], "UTC");
    }
    for bad in [
        b"Thu Oct 2 12:00:00 2026".as_slice(),
        b"Fri Oct 32 12:00:00 2026",
        b"Fri Oct 2 24:00:00 2026",
        b"Fri Oct 2 12:00:00 2026\n\n",
        b"Fri Oct +2 12:00:00 2026",
        b"Thu Feb 29 00:00:00 2025",
        b"",
    ] {
        assert!(parse_ps_start(bad).is_err());
    }
    assert_eq!(parse_ps_start(b"Thu Jan 1 00:00:00 1970\n").unwrap(), 0);
    assert!(parse_ps_start(b"Thu Feb 29 00:00:00 2024").is_ok());
}
#[test]
fn malformed_absent_foreign_processes_and_pid_reuse_fail_closed() {
    for flags in [
        (true, false, false, false),
        (false, true, false, false),
        (false, false, true, false),
        (false, false, false, true),
    ] {
        let fixture = Fixture::new(flags.0, flags.1, flags.2, flags.3, false);
        assert!(fixture.io.process_identity(4242, &deadline()).is_err());
    }
    let fixture = Fixture::plain();
    for name in [
        b"foreign\n".as_slice(),
        b"crosspane-agent\ncrosspane-agent\n",
        b"crosspane-agent\n\n",
        b"",
    ] {
        *fixture.runner.name.lock().unwrap() = Some(name.to_vec());
        assert!(fixture.io.process_identity(4242, &deadline()).is_err());
    }
    *fixture.runner.name.lock().unwrap() = None;
    *fixture.runner.start.lock().unwrap() = Some(b"malformed".to_vec());
    assert!(fixture.io.process_identity(4242, &deadline()).is_err());
    assert!(fixture.io.process_identity(0, &deadline()).is_err());
    assert!(fixture.io.bootstrap(&deadline()).is_err());
}
#[test]
fn cancellation_timeouts_oversize_outputs_and_expired_support_refuse_mutation() {
    let fixture = Fixture::new(false, false, false, false, true);
    assert_eq!(
        fixture.io.process_identity(4242, &deadline()),
        Err(NativeError::Oversize)
    );
    let fixture = Fixture::plain();
    let cancel = Cancellation::default();
    let d = Deadline::new(1000, cancel.clone()).unwrap();
    cancel.cancel();
    assert_eq!(
        fixture.io.process_identity(4242, &d),
        Err(NativeError::Cancelled)
    );
    let d = Deadline::new(1, Cancellation::default()).unwrap();
    thread::sleep(Duration::from_millis(3));
    assert_eq!(d.check(), Err(NativeError::Timeout));
    let proof = fixture.proof();
    thread::sleep(SUPPORT_LIFETIME + Duration::from_millis(10));
    let dir = fixture.root.join("never-created");
    assert_eq!(
        fixture.io.create_private_dir(&proof, &dir),
        Err(NativeError::Unsupported)
    );
    assert!(matches!(
        fixture.io.lock(&proof, &fixture.root.join("lock")),
        Err(NativeError::Unsupported)
    ));
    let spec = CommandSpec::new(
        "/bin/ps".into(),
        vec!["-o".into(), "comm=".into(), "-p".into(), "4242".into()],
        ChildEnvironment::selected(fixture.io.target(), BTreeMap::new()).unwrap(),
        256,
    )
    .unwrap();
    assert!(matches!(
        fixture.io.run_mutation(&proof, &spec, &deadline()),
        Err(NativeError::Unsupported)
    ));
    assert!(matches!(
        CommandSpec::new(
            "/usr/bin/systemctl".into(),
            vec![
                "--user".into(),
                "start".into(),
                "crosspane-agent.service".into(),
            ],
            ChildEnvironment::selected(fixture.io.target(), BTreeMap::new()).unwrap(),
            256,
        ),
        Err(NativeError::Invalid)
    ));
    assert!(!dir.exists());
    assert!(fixture.runner.calls.lock().unwrap().is_empty());
}

#[test]
fn scripted_exit_bootstrap_replacement_regression_and_ps_failure_are_refused() {
    let fixture = Fixture::plain();
    let stable = ProcessFacts {
        uid: fixture.io.target().paths().uid,
        executable: fixture.io.target().agent_path(),
        generation: 77,
    };
    *fixture.probe.script.lock().unwrap() =
        [Ok(stable.clone()), Err(NativeError::Unavailable)].into();
    assert_eq!(
        fixture.io.process_identity(4242, &deadline()),
        Err(NativeError::Unavailable)
    );
    assert!(fixture.io.process_identity(4242, &deadline()).is_ok());
    for query in 0..2 {
        for (code, stderr) in [
            (Some(1), Vec::new()),
            (None, Vec::new()),
            (Some(0), b"untrusted error".to_vec()),
        ] {
            let mut outputs = vec![
                (Some(0), START.to_vec(), Vec::new()),
                (Some(0), b"crosspane-agent\n".to_vec(), Vec::new()),
            ];
            outputs[query].0 = code;
            outputs[query].2 = stderr;
            *fixture.runner.query_outputs.lock().unwrap() = outputs.into();
            assert!(fixture.io.process_identity(4242, &deadline()).is_err());
            fixture.runner.query_outputs.lock().unwrap().clear();
        }
    }
    for field in ["instance_id", "phase_seq"] {
        fixture.bootstrap(0);
        let path = fixture.io.target().runtime_dir().join("bootstrap.json");
        let count = fixture.runner.calls.lock().unwrap().len() + 2;
        *fixture.runner.hook.lock().unwrap() = Some((
            count,
            Box::new(move || {
                let mut json: serde_json::Value =
                    serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                json[field] = serde_json::json!(if field == "phase_seq" { 1 } else { 10 });
                fs::write(path, serde_json::to_vec(&json).unwrap()).unwrap();
            }),
        ));
        assert_eq!(
            fixture.io.bootstrap(&deadline()).unwrap_err(),
            NativeError::Foreign
        );
    }
    fixture.bootstrap(0);
    let changed = ProcessFacts {
        generation: 78,
        ..stable.clone()
    };
    *fixture.probe.script.lock().unwrap() = [
        Ok(stable.clone()),
        Ok(stable),
        Ok(changed.clone()),
        Ok(changed),
    ]
    .into();
    assert_eq!(
        fixture.io.bootstrap(&deadline()).unwrap_err(),
        NativeError::Foreign
    );
}

#[test]
fn systemctl_is_never_admitted_and_fake_mutation_executor_requires_selected_proof() {
    let fixture = Fixture::plain();
    let other = Fixture::plain();
    let proof = fixture.proof();
    let environment = ChildEnvironment::selected(fixture.io.target(), BTreeMap::new()).unwrap();
    for operation in [
        "daemon-reload",
        "enable",
        "start",
        "stop",
        "is-enabled",
        "is-active",
        "show",
    ] {
        let mut argv = vec!["--user".into(), operation.into()];
        if operation != "daemon-reload" {
            argv.push("crosspane-agent.service".into());
        }
        if operation == "show" {
            argv.push("--property=FragmentPath,DropInPaths,ExecStart,User,ActiveState,UnitFileState,MainPID".into());
        }
        for executable in ["/usr/bin/systemctl", "/bin/systemctl"] {
            assert!(matches!(
                CommandSpec::new(executable.into(), argv.clone(), environment.clone(), 256),
                Err(NativeError::Invalid)
            ));
        }
        assert!(fixture.runner.calls.lock().unwrap().is_empty());
    }
    let command = CommandSpec::new(
        "/bin/ps".into(),
        vec!["-o".into(), "comm=".into(), "-p".into(), "4242".into()],
        environment,
        256,
    )
    .unwrap();
    assert!(matches!(
        fixture
            .io
            .run_mutation(&other.proof(), &command, &deadline()),
        Err(NativeError::Unsupported)
    ));
    assert!(fixture.runner.calls.lock().unwrap().is_empty());
    // Only the injected runner executes this generic proof-gated path.
    fixture
        .io
        .run_mutation(&proof, &command, &deadline())
        .unwrap();
    assert_eq!(fixture.runner.calls.lock().unwrap().len(), 1);
    fixture.runner.calls.lock().unwrap().clear();
    let mut changed = facts(&fixture.io);
    changed.active = false;
    assert_eq!(
        proof.revalidate(&fixture.io, &changed),
        Err(NativeError::Unsupported)
    );
    assert!(matches!(
        fixture.io.run_mutation(&proof, &command, &deadline()),
        Err(NativeError::Unsupported)
    ));
    let foreign = CommandSpec::new(
        "/bin/ps".into(),
        vec!["-o".into(), "comm=".into(), "-p".into(), "4242".into()],
        ChildEnvironment::selected(other.io.target(), BTreeMap::new()).unwrap(),
        256,
    )
    .unwrap();
    assert!(matches!(
        fixture.io.run(&foreign, &deadline()),
        Err(NativeError::Foreign)
    ));
    let fresh = fixture.proof();
    assert!(matches!(
        fixture.io.run_mutation(&fresh, &foreign, &deadline()),
        Err(NativeError::Foreign)
    ));
    assert!(fixture.runner.calls.lock().unwrap().is_empty());
    let observed = CommandSpec::new(
        "/bin/ps".into(),
        foreign.argv().to_vec(),
        ChildEnvironment::selected(fixture.io.target(), BTreeMap::new()).unwrap(),
        256,
    )
    .unwrap();
    fs::create_dir_all(&fixture.io.target().paths().prefix).unwrap();
    symlink(&other.root, &fixture.io.target().paths().config_home).unwrap();
    assert!(matches!(
        fixture.io.run(&observed, &deadline()),
        Err(NativeError::Foreign)
    ));
    assert!(matches!(
        fixture.io.run_mutation(&fresh, &command, &deadline()),
        Err(NativeError::Foreign)
    ));
    assert!(fixture.runner.calls.lock().unwrap().is_empty());
}

#[test]
fn every_mutation_entry_checks_revoked_other_target_proofs_and_lock_files() {
    let fixture = Fixture::plain();
    let other = Fixture::plain();
    let proof = fixture.proof();
    let mut changed = facts(&fixture.io);
    changed.active = false;
    assert_eq!(
        proof.revalidate(&fixture.io, &changed),
        Err(NativeError::Unsupported)
    );
    let spec = CommandSpec::new(
        "/bin/ps".into(),
        vec!["-o".into(), "comm=".into(), "-p".into(), "4242".into()],
        ChildEnvironment::selected(fixture.io.target(), BTreeMap::new()).unwrap(),
        256,
    )
    .unwrap();
    for proof in [proof, other.proof()] {
        assert!(matches!(
            fixture.io.run_mutation(&proof, &spec, &deadline()),
            Err(NativeError::Unsupported)
        ));
        assert!(matches!(
            fixture.io.lock(&proof, &fixture.root.join("lock")),
            Err(NativeError::Unsupported)
        ));
        assert_eq!(
            fixture
                .io
                .create_private_dir(&proof, &fixture.root.join("no-dir")),
            Err(NativeError::Unsupported)
        );
    }
    assert!(!fixture.root.join("lock").exists());
    assert!(!fixture.root.join("no-dir").exists());
    assert!(fixture.runner.calls.lock().unwrap().is_empty());
    let good = fixture.proof();
    let path = fixture.root.join("lock");
    let original = fixture.root.join("original");
    fs::write(&original, b"preserved").unwrap();
    fs::set_permissions(&original, fs::Permissions::from_mode(0o600)).unwrap();
    symlink(&original, &path).unwrap();
    assert!(fixture.io.lock(&good, &path).is_err());
    fs::remove_file(&path).unwrap();
    fs::hard_link(&original, &path).unwrap();
    assert_eq!(
        fixture.io.lock(&good, &path).unwrap_err(),
        NativeError::Foreign
    );
    fs::remove_file(&path).unwrap();
    fs::write(&path, b"foreign").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        fixture.io.lock(&good, &path).unwrap_err(),
        NativeError::Foreign
    );
    assert_eq!(fs::read(&original).unwrap(), b"preserved");
    assert_eq!(fs::read(&path).unwrap(), b"foreign");
}

#[test]
fn command_environment_aggregate_control_and_combined_output_bounds() {
    let fixture = Fixture::plain();
    let env = ChildEnvironment::selected(fixture.io.target(), BTreeMap::new()).unwrap();
    let args = vec!["-o".into(), "comm=".into(), "-p".into(), "4242".into()];
    for args in [
        vec!["x".repeat(4096); 9],
        vec!["nul\0".into()],
        vec!["line\n".into()],
    ] {
        assert!(CommandSpec::new("/bin/ps".into(), args, env.clone(), 256).is_err());
    }
    assert!(CommandSpec::new("/bin/ps".into(), args.clone(), env.clone(), 0).is_err());
    for value in ["x".repeat(4097), "bad\0".into(), "bad\n".into()] {
        assert!(
            ChildEnvironment::selected(
                fixture.io.target(),
                BTreeMap::from([("HYPRLAND_INSTANCE_SIGNATURE".into(), value)])
            )
            .is_err()
        );
    }
    assert!(
        ChildEnvironment::selected(
            fixture.io.target(),
            BTreeMap::from([("HYPRLAND_INSTANCE_SIGNATURE".into(), "x".repeat(4096))])
        )
        .is_ok()
    );
    let spec = CommandSpec::new("/bin/ps".into(), args, env, 256).unwrap();
    *fixture.runner.output.lock().unwrap() = Some((Some(0), vec![0; 128], vec![0; 128]));
    assert!(fixture.io.run(&spec, &deadline()).is_ok());
    *fixture.runner.output.lock().unwrap() = Some((Some(0), vec![0; 128], vec![0; 129]));
    assert_eq!(
        fixture.io.run(&spec, &deadline()).unwrap_err(),
        NativeError::Oversize
    );
}
