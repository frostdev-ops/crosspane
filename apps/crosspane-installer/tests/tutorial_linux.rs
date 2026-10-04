#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)] // Scratch and fake assertions only; production retains the deny.
use crosspane_installer::platform::linux::{native_io, tutorial};
use crosspane_installer::{fixture, tutorial_window};
use crosspane_installer_core::AttemptId;
use crosspane_types::id::WindowId;
use fixture::*;
use native_io::tutorial::TutorialIpc;
use native_io::*;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    collections::VecDeque,
    fs,
    io::{Read, Write},
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
const TITLE: &str = "Crosspane practice | TEST_MACHINE | attempt 1 | fixture 1";
static SERIAL: AtomicUsize = AtomicUsize::new(0);
struct Scratch(PathBuf, Arc<AtomicBool>);
impl Scratch {
    fn new() -> Self {
        let path = Self::path();
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self::owned(path)
    }
    fn owned(path: PathBuf) -> Self {
        Self(path, Arc::new(AtomicBool::new(false)))
    }
    fn path() -> PathBuf {
        PathBuf::from(format!(
            // Leave room for the compositor signature in Linux's bounded Unix socket path.
            "/tmp/cb{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::SeqCst)
        ))
    }
}

#[test]
fn b2_duplicate_stable_id_is_refused_even_for_an_unrelated_client() {
    let mut foreign = client();
    foreign["pid"] = json!(78);
    foreign["title"] = json!("UNRELATED_SYNTHETIC_TITLE");
    assert_eq!(
        observe(
            &mut tutorial::WindowObserver::default(),
            json!([client(), foreign]),
            monitors()
        ),
        Err(FixtureError::AmbiguousWindow)
    );
}

#[test]
fn b2_admitted_executable_replacement_is_refused_before_process_probe() {
    let a = Admission::new();
    let command = a.admit().unwrap();
    fs::rename(&a.path, a.root.0.join("held-executable")).unwrap();
    fs::write(&a.path, b"SUBSTITUTE_INERT_BYTES").unwrap();
    fs::set_permissions(&a.path, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(
        a.io.tutorial_identity(
            &command,
            42,
            &Deadline::new(2000, Cancellation::default()).unwrap()
        ),
        Err(NativeError::Foreign)
    ));
    assert!(a.probe.facts.lock().unwrap().is_empty());
}

#[test]
fn b2_digest_only_timing_on_same_bounded_artifact() {
    if std::env::var("CROSSPANE_DIGEST_TIMING").as_deref() != Ok("1") {
        return;
    }
    let scratch = Scratch::new();
    let path = scratch.0.join("crosspane-tutorial");
    stage_artifact(
        Path::new(env!("CARGO_BIN_EXE_crosspane-tutorial")),
        &path,
        MAX_FILE_BYTES as u64,
    )
    .unwrap();
    let mut file = fs::File::from(
        rustix::fs::open(
            &path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .unwrap(),
    );
    let stat = rustix::fs::fstat(&file).unwrap();
    assert_eq!(stat.st_size, 79_095_328);
    assert!(stat.st_size as u64 <= MAX_FILE_BYTES as u64);
    assert_eq!(stat.st_uid, rustix::process::geteuid().as_raw());
    assert_eq!(stat.st_nlink, 1);
    assert_eq!(stat.st_mode & 0o177022, 0o100000);
    let expected = [
        0x7c, 0x17, 0x83, 0x55, 0x4b, 0x25, 0x8f, 0x8d, 0x53, 0x95, 0xbd, 0x9c, 0x13, 0x36, 0x03,
        0x14, 0x86, 0x38, 0x43, 0x3e, 0x49, 0xac, 0x83, 0xed, 0x7e, 0x55, 0x62, 0xac, 0xa8, 0xdf,
        0x69, 0xb5,
    ];
    // Precisely mirror native_io/tutorial.rs's digest step, not font/ownership preparation,
    // IPC, spawn, Hello, or observation. The artifact, 64KiB reads and 2s bound are unchanged.
    let deadline = Deadline::new(2000, Cancellation::default()).unwrap();
    let started = Instant::now();
    let mut size = 0u64;
    let result = (|| -> std::result::Result<(), NativeError> {
        let mut digest = aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA256);
        let mut buffer = [0; 64 * 1024];
        loop {
            deadline.check()?;
            let n = file
                .read(&mut buffer)
                .map_err(|_| NativeError::Unavailable)?;
            if n == 0 {
                break;
            }
            size = size.checked_add(n as u64).ok_or(NativeError::Oversize)?;
            if size > MAX_FILE_BYTES as u64 {
                return Err(NativeError::Oversize);
            }
            digest.update(&buffer[..n]);
        }
        if size != stat.st_size as u64 || digest.finish().as_ref() != expected {
            return Err(NativeError::Foreign);
        }
        Ok(())
    })();
    let elapsed = started.elapsed();
    eprintln!(
        "digest-only AWS-LC Context/SHA256: elapsed={elapsed:?}, bytes={size}, result={result:?}, expected_sha256=7c1783554b258f8d5395bd9c133603148638433e49ac83ed7e5562aca8df69b5"
    );
    result.unwrap();
}

// The guarded cases below are window-only. No PipeWire server, input injector, agent,
// Secret Service, tray, or hardware audio endpoint is created or queried.
const DEAD_SESSION: &str = "unix:path=/nonexistent/crosspane-test-bus";
const DEAD_SYSTEM: &str = "unix:path=/nonexistent/crosspane-system-bus";
const DEAD_PIPEWIRE: &str = "/nonexistent/crosspane-audio";
const DEAD_PULSE: &str = "unix:/nonexistent/crosspane-audio";
#[derive(Debug, Clone, PartialEq, Eq)]
struct NestSelection {
    signature: String,
    display: String,
}
impl NestSelection {
    fn verify_fresh(
        &self,
        recorded: (u32, u64),
        current_start: Option<u64>,
        instances: &Value,
    ) -> std::result::Result<(), &'static str> {
        if recorded.0 == 0 || recorded.1 == 0 || current_start != Some(recorded.1) {
            return Err("owned PID/start record is stale");
        }
        let instances = instances
            .as_array()
            .filter(|a| a.len() <= 64)
            .ok_or("invalid current instances")?;
        let mut matches = 0;
        for instance in instances {
            if instance["pid"].as_u64() == Some(u64::from(recorded.0))
                || instance["instance"].as_str() == Some(&self.signature)
                || instance["wl_socket"].as_str() == Some(&self.display)
            {
                if instance["pid"].as_u64() != Some(u64::from(recorded.0))
                    || instance["instance"].as_str() != Some(&self.signature)
                    || instance["wl_socket"].as_str() != Some(&self.display)
                {
                    return Err("current instance/socket does not belong to the recorded child");
                }
                matches += 1;
            }
        }
        if matches != 1 {
            return Err("owned current instance must be unique");
        }
        Ok(())
    }
    fn parse(text: &str) -> std::result::Result<Self, &'static str> {
        let lines: Vec<_> = text.lines().collect();
        if lines.len() != 4
            || lines[0] != "unset WAYLAND_SOCKET"
            || lines[3] != "export CROSSPANE_NESTED_HYPR=1"
            || !text.ends_with('\n')
        {
            return Err("not the named harness environment");
        }
        let signature = lines[1]
            .strip_prefix("export HYPRLAND_INSTANCE_SIGNATURE=")
            .ok_or("missing signature")?;
        let display = lines[2]
            .strip_prefix("export WAYLAND_DISPLAY=")
            .ok_or("missing display")?;
        for value in [signature, display] {
            if value.is_empty()
                || value.len() > 160
                || value == "."
                || value == ".."
                || !value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            {
                return Err("invalid exact harness value");
            }
        }
        Ok(Self {
            signature: signature.into(),
            display: display.into(),
        })
    }
    fn verify(&self, values: &BTreeMap<String, String>) -> std::result::Result<(), &'static str> {
        for (key, expected) in [
            ("CROSSPANE_NESTED_HYPR", "1"),
            ("HYPRLAND_INSTANCE_SIGNATURE", self.signature.as_str()),
            ("WAYLAND_DISPLAY", self.display.as_str()),
            ("DBUS_SESSION_BUS_ADDRESS", DEAD_SESSION),
            ("DBUS_SYSTEM_BUS_ADDRESS", DEAD_SYSTEM),
            ("PIPEWIRE_REMOTE", DEAD_PIPEWIRE),
            ("PULSE_SERVER", DEAD_PULSE),
        ] {
            if values.get(key).map(String::as_str) != Some(expected) {
                return Err("inherited environment does not equal the owned harness");
            }
        }
        for key in [
            "WAYLAND_SOCKET",
            "DISPLAY",
            "CROSSPANE_AUDIO",
            "CROSSPANE_LIVE_TESTS",
            "CROSSPANE_REAL_STORE",
            "CROSSPANE_SECRET_SERVICE_LIVE",
            "CROSSPANE_LIVE_HYPR",
            "CROSSPANE_PRIVATE_PIPEWIRE",
            "CROSSPANE_INJECT_PRIVATE_NEST",
            "CROSSPANE_LIVE_SNI",
        ] {
            if values.contains_key(key) {
                return Err("unexpected live opt-in");
            }
        }
        Ok(())
    }
    fn connect_explicit(&self, runtime: &Path, home: &Path, retain: Arc<AtomicBool>) {
        // No connect_to_env, descriptor environment, substring guard, or owner fallback.
        let state = runtime.join("crosspane-hypr-wp415");
        let recorded = read_nest_record(&state).unwrap();
        assert_eq!(owned_start_ticks(recorded.0).unwrap(), Some(recorded.1));
        let mut command = private_command(runtime, home);
        command.args(["/usr/bin/hyprctl", "instances", "-j"]);
        let instances = serde_json::from_str(&run_bounded(command, retain)).unwrap();
        // Refresh start time after the bounded inventory command and immediately before connect.
        self.verify_fresh(recorded, owned_start_ticks(recorded.0).unwrap(), &instances)
            .unwrap();
        let address = rustix::net::SocketAddrUnix::new(runtime.join(&self.display)).unwrap();
        let fd = rustix::net::socket_with(
            rustix::net::AddressFamily::UNIX,
            rustix::net::SocketType::STREAM,
            rustix::net::SocketFlags::NONBLOCK | rustix::net::SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();
        connect_before(
            Instant::now() + Duration::from_millis(250),
            || match rustix::net::connect(&fd, &address) {
                Ok(()) | Err(rustix::io::Errno::ISCONN) => Ok(()),
                Err(
                    rustix::io::Errno::AGAIN
                    | rustix::io::Errno::INPROGRESS
                    | rustix::io::Errno::ALREADY,
                ) => Err(std::io::ErrorKind::WouldBlock.into()),
                Err(error) => Err(error.into()),
            },
        )
        .unwrap();
        let stream = UnixStream::from(fd);
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(250)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_millis(250)))
            .unwrap();
        let _connection = wayland_client::Connection::from_socket(stream).unwrap();
    }
}
fn read_nest_record(state: &Path) -> std::io::Result<(u32, u64)> {
    let meta = fs::symlink_metadata(state)?;
    let uid = rustix::process::geteuid().as_raw();
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o022 != 0 {
        return Err(std::io::ErrorKind::PermissionDenied.into());
    }
    let read = |name: &str| -> std::io::Result<u64> {
        let file = fs::File::from(rustix::fs::open(
            state.join(name),
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )?);
        let meta = file.metadata()?;
        if !meta.is_file() || meta.uid() != uid || meta.mode() & 0o022 != 0 || meta.len() > 128 {
            return Err(std::io::ErrorKind::PermissionDenied.into());
        }
        let mut text = String::new();
        file.take(129).read_to_string(&mut text)?;
        if text.len() > 128 {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        text.trim()
            .parse()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| std::io::ErrorKind::InvalidData.into())
    };
    let pid = u32::try_from(read("pid")?).map_err(|_| std::io::ErrorKind::InvalidData)?;
    Ok((pid, read("start")?))
}
fn isolated_values(selection: &NestSelection) -> BTreeMap<String, String> {
    [
        ("CROSSPANE_NESTED_HYPR", "1"),
        ("HYPRLAND_INSTANCE_SIGNATURE", selection.signature.as_str()),
        ("WAYLAND_DISPLAY", selection.display.as_str()),
        ("DBUS_SESSION_BUS_ADDRESS", DEAD_SESSION),
        ("DBUS_SYSTEM_BUS_ADDRESS", DEAD_SYSTEM),
        ("PIPEWIRE_REMOTE", DEAD_PIPEWIRE),
        ("PULSE_SERVER", DEAD_PULSE),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v.into()))
    .collect()
}
#[test]
fn b2_nested_guard_requires_exact_values_and_rejects_owner_handles_before_connect() {
    let selected = NestSelection::parse("unset WAYLAND_SOCKET\nexport HYPRLAND_INSTANCE_SIGNATURE=owned_exact\nexport WAYLAND_DISPLAY=wayland-9\nexport CROSSPANE_NESTED_HYPR=1\n").unwrap();
    let values = isolated_values(&selected);
    selected.verify(&values).unwrap();
    for (key, bad) in [
        ("HYPRLAND_INSTANCE_SIGNATURE", "owned_exact_neighbor"),
        ("HYPRLAND_INSTANCE_SIGNATURE", "prefix_owned_exact"),
        ("WAYLAND_DISPLAY", "wayland-90"),
        ("WAYLAND_DISPLAY", "prefix-wayland-9"),
        ("CROSSPANE_NESTED_HYPR", "01"),
        ("CROSSPANE_NESTED_HYPR", "1-extra"),
        ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus"),
        (
            "DBUS_SYSTEM_BUS_ADDRESS",
            "unix:path=/run/dbus/system_bus_socket",
        ),
        ("PIPEWIRE_REMOTE", "pipewire-0"),
        ("PULSE_SERVER", "unix:/owner/pulse"),
        ("WAYLAND_SOCKET", "3"),
        ("CROSSPANE_AUDIO", "1"),
        ("CROSSPANE_LIVE_SNI", "1"),
    ] {
        let mut changed = values.clone();
        changed.insert(key.into(), bad.into());
        assert!(
            selected.verify(&changed).is_err(),
            "guard must refuse {key}"
        );
    }
    for missing in values.keys() {
        let mut changed = values.clone();
        changed.remove(missing);
        assert!(selected.verify(&changed).is_err());
    }
    for malformed in [
        "unset WAYLAND_SOCKET\nexport HYPRLAND_INSTANCE_SIGNATURE=../foreign\nexport WAYLAND_DISPLAY=wayland-9\nexport CROSSPANE_NESTED_HYPR=1\n",
        "unset WAYLAND_SOCKET\nexport HYPRLAND_INSTANCE_SIGNATURE=owned_exact\nexport WAYLAND_DISPLAY=wayland-9\nexport CROSSPANE_NESTED_HYPR=1",
    ] {
        assert!(NestSelection::parse(malformed).is_err());
    }
}
fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .into()
}
fn private_command(runtime: &Path, home: &Path) -> Command {
    let mut command = Command::new(repository().join("scripts/lead/impl-env.sh"));
    command
        .env_clear()
        .envs([
            ("PATH", "/usr/bin:/bin"),
            ("LC_ALL", "C"),
            ("DBUS_SESSION_BUS_ADDRESS", DEAD_SESSION),
            ("DBUS_SYSTEM_BUS_ADDRESS", DEAD_SYSTEM),
            ("PIPEWIRE_REMOTE", DEAD_PIPEWIRE),
            ("PULSE_SERVER", DEAD_PULSE),
            ("CARGO_BUILD_JOBS", "2"),
            ("GALLIUM_DRIVER", "llvmpipe"),
        ])
        .env("XDG_RUNTIME_DIR", runtime)
        .env("HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}
fn reap_before(deadline: Instant, mut probe: impl FnMut() -> bool) -> bool {
    while Instant::now() < deadline {
        if probe() {
            return Instant::now() < deadline;
        }
        thread::sleep(Duration::from_millis(2));
    }
    false
}
fn connect_before(
    deadline: Instant,
    mut connect: impl FnMut() -> std::io::Result<()>,
) -> std::io::Result<()> {
    loop {
        if Instant::now() >= deadline {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        let result = connect();
        if Instant::now() >= deadline {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        match result {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2))
            }
            result => return result,
        }
    }
}
fn stage_bounded(
    mut source: impl Read,
    target: &Path,
    limit: u64,
) -> std::io::Result<([u8; 32], fs::File)> {
    let mut target = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(target)?;
    let mut digest = aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA256);
    let mut copied = 0u64;
    let mut buffer = [0; 64 * 1024];
    loop {
        let room = (limit - copied).min(buffer.len() as u64) as usize;
        if room == 0 {
            if source.read(&mut buffer[..1])? != 0 {
                return Err(std::io::ErrorKind::FileTooLarge.into());
            }
            break;
        }
        let n = source.read(&mut buffer[..room])?;
        if n == 0 {
            break;
        }
        target.write_all(&buffer[..n])?;
        digest.update(&buffer[..n]);
        copied += n as u64;
    }
    let digest = digest
        .finish()
        .as_ref()
        .try_into()
        .map_err(|_| std::io::ErrorKind::InvalidData)?;
    Ok((digest, target))
}
fn stage_artifact(source: &Path, target: &Path, limit: u64) -> std::io::Result<[u8; 32]> {
    let mut source = fs::File::from(rustix::fs::open(
        source,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?);
    let before = source.metadata()?;
    if !before.is_file()
        || before.uid() != rustix::process::geteuid().as_raw()
        // Cargo's source and deps executable are hard links. The private copy is single-link.
        || before.nlink() == 0
        || before.mode() & 0o7022 != 0
        || before.len() > limit
    {
        return Err(std::io::ErrorKind::PermissionDenied.into());
    }
    let (digest, staged) = stage_bounded(&mut source, target, limit)?;
    let after = source.metadata()?;
    let copied = staged.metadata()?;
    let named = fs::symlink_metadata(target)?;
    if before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.uid() != after.uid()
        || before.mode() != after.mode()
        || before.nlink() != after.nlink()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
        || !copied.is_file()
        || copied.uid() != before.uid()
        || copied.nlink() != 1
        || copied.mode() & 0o7777 != 0o700
        || copied.len() != after.len()
        || named.dev() != copied.dev()
        || named.ino() != copied.ino()
    {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    Ok(digest)
}
#[test]
fn b2_stale_nested_record_is_refused_before_connect() {
    let root = Scratch::new();
    let selected = NestSelection {
        signature: "owned_exact".into(),
        display: "wayland-9".into(),
    };
    let mapped = json!([{"pid":42,"instance":"owned_exact","wl_socket":"wayland-9"}]);
    for (start, instances) in [
        (None, mapped.clone()),
        (Some(101), mapped.clone()),
        (
            Some(100),
            json!([{"pid":43,"instance":"owned_exact","wl_socket":"wayland-9"}]),
        ),
        (
            Some(100),
            json!([{"pid":42,"instance":"owner_other","wl_socket":"wayland-9"}]),
        ),
        (
            Some(100),
            json!([{"pid":42,"instance":"owned_exact","wl_socket":"wayland-10"}]),
        ),
        (Some(100), json!([mapped[0].clone(), mapped[0].clone()])),
    ] {
        // Scratch records and injected current facts only: no proc probe or socket connection.
        fs::write(root.0.join("pid"), b"42\n").unwrap();
        fs::write(root.0.join("start"), b"100\n").unwrap();
        let recorded = read_nest_record(&root.0).unwrap();
        assert!(selected.verify_fresh(recorded, start, &instances).is_err());
    }
    selected
        .verify_fresh((42, 100), Some(100), &mapped)
        .unwrap();
}
#[test]
fn b2_nonblocking_connect_deadline_refuses_a_stalled_scratch_endpoint() {
    let root = Scratch::new();
    let _listener = UnixListener::bind(root.0.join("inert.sock")).unwrap();
    let deadline = Instant::now() + Duration::from_millis(1);
    let result = connect_before(deadline, || {
        thread::sleep(Duration::from_millis(3));
        Ok(())
    });
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
    let mut attempts = 0;
    let result = connect_before(Instant::now() + Duration::from_millis(10), || {
        attempts += 1;
        Err(std::io::ErrorKind::WouldBlock.into())
    });
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
    assert!(attempts > 1);
}
#[test]
fn b2_reap_deadline_preserves_owned_evidence_until_confirmation() {
    let root = Scratch::new();
    let record = root.0.join("owned-record");
    fs::write(&record, b"42 100\n").unwrap();
    let result = reap_before(Instant::now(), || true);
    assert!(!result, "a probe beyond the grace cannot confirm cleanup");
    assert!(record.exists());
    struct FakeChild {
        reaped: Arc<AtomicBool>,
        dropped: Arc<AtomicBool>,
        signalled: Arc<AtomicBool>,
    }
    impl ReapOwned for FakeChild {
        fn signal_owned(&mut self) {
            self.signalled.store(true, Ordering::Release);
        }
        fn reaped(&mut self) -> bool {
            self.reaped.load(Ordering::Acquire)
        }
    }
    impl Drop for FakeChild {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }
    let reaped = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let signalled = Arc::new(AtomicBool::new(false));
    let owner = OwnedProcess {
        pending: Some(Cleanup {
            child: FakeChild {
                reaped: reaped.clone(),
                dropped: dropped.clone(),
                signalled: signalled.clone(),
            },
            record: record.clone(),
            _slot: HarnessSlot::admit(),
        }),
        retain: root.1.clone(),
        grace: Duration::ZERO,
    };
    drop(owner);
    assert!(root.1.load(Ordering::Acquire));
    assert!(signalled.load(Ordering::Acquire));
    assert!(!dropped.load(Ordering::Acquire));
    assert!(record.exists());
    assert_eq!(HARNESS_CHILDREN.load(Ordering::Acquire), 1);
    reaped.store(true, Ordering::Release);
    until(|| dropped.load(Ordering::Acquire));
    assert!(!record.exists());
    until(|| HARNESS_CHILDREN.load(Ordering::Acquire) == 0);
    root.1.store(false, Ordering::Release);
}
#[test]
fn b2_held_artifact_growth_cannot_exceed_copy_limit() {
    let root = Scratch::new();
    let source = root.0.join("source");
    fs::write(&source, b"1234").unwrap();
    let held = fs::File::open(&source).unwrap();
    fs::write(&source, b"1234567890123456").unwrap();
    let target = root.0.join("staged");
    assert!(stage_bounded(held, &target, 8).is_err());
    assert!(target.metadata().map_or(true, |m| m.len() <= 8));
}
#[test]
fn b2_bounded_0755_cargo_artifact_stages_into_exact_private_prefix() {
    let a = Admission::new();
    fs::remove_file(&a.path).unwrap();
    let paths = a.io.target().paths();
    for path in [
        &paths.prefix,
        &paths.config_home,
        &paths.state_home,
        &paths.data_home,
        &paths.runtime_home,
        &paths.prefix.join("bin"),
        &paths.runtime_home.join("private"),
    ] {
        fs::create_dir_all(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    assert_eq!(a.path, a.root.0.join(".local/bin/crosspane-tutorial"));
    let build = a.root.0.join("build/deps");
    fs::create_dir_all(&build).unwrap();
    let source = a.root.0.join("build/crosspane-tutorial");
    // Use a bounded ELF prefix as synthetic data, independent of debug-profile size.
    // Neither this source nor its staged destination is ever executed by the regression.
    const SOURCE_BYTES: u64 = 8 * 1024 * 1024;
    let current = std::env::current_exe().unwrap();
    let held = fs::File::from(
        rustix::fs::open(
            current,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .unwrap(),
    );
    let (expected, _held_copy) =
        stage_bounded(held.take(SOURCE_BYTES), &source, SOURCE_BYTES).unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
    fs::hard_link(&source, build.join("crosspane_tutorial-test")).unwrap();
    let meta = source.metadata().unwrap();
    assert!(meta.len() > 0 && meta.len() <= SOURCE_BYTES);
    assert_eq!(meta.mode() & 0o7777, 0o755);
    assert_eq!(
        meta.nlink(),
        2,
        "Cargo's executable and deps paths share the source inode"
    );
    assert!(!source.starts_with(&paths.prefix));
    let mut magic = [0; 4];
    fs::File::open(&source)
        .unwrap()
        .read_exact(&mut magic)
        .unwrap();
    assert_eq!(magic, *b"\x7fELF");
    let digest = stage_artifact(&source, &a.path, MAX_FILE_BYTES as u64).unwrap();
    assert_eq!(digest, expected);
    let staged = a.path.metadata().unwrap();
    assert_eq!(staged.mode() & 0o7777, 0o700);
    assert_eq!(staged.nlink(), 1);
    assert_eq!(staged.uid(), paths.uid);
    assert_eq!(staged.len(), meta.len());
    a.io.admit_tutorial(
        &a.proof,
        &a.environment,
        digest,
        &a.font,
        &Deadline::new(2000, Cancellation::default()).unwrap(),
    )
    .unwrap();
    assert!(
        a.probe.facts.lock().unwrap().is_empty(),
        "no process probe or spawn"
    );
}
#[test]
fn b2_stable_id_rejects_signed_and_non_ascii_hex() {
    for id in [
        "+1800000c",
        "0x+1800000c",
        "-1800000c",
        " 1800000c",
        "1800000c ",
        "0x",
        "１８０００００c",
    ] {
        let mut c = client();
        c["stableId"] = json!(id);
        assert!(
            observe(
                &mut tutorial::WindowObserver::default(),
                json!([c]),
                monitors()
            )
            .is_err(),
            "{id}"
        );
    }
}
static HARNESS_CHILDREN: AtomicUsize = AtomicUsize::new(0);
struct HarnessSlot;
impl HarnessSlot {
    fn admit() -> Self {
        HARNESS_CHILDREN
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < 4).then_some(n + 1)
            })
            .unwrap();
        Self
    }
}
impl Drop for HarnessSlot {
    fn drop(&mut self) {
        HARNESS_CHILDREN.fetch_sub(1, Ordering::AcqRel);
    }
}
trait ReapOwned: Send + 'static {
    fn signal_owned(&mut self);
    fn reaped(&mut self) -> bool;
}
impl ReapOwned for Child {
    fn signal_owned(&mut self) {
        // An error (including lost wait ownership) cannot authorize a signal.
        if self.try_wait().is_ok_and(|status| status.is_none()) {
            let _ = self.kill();
        }
    }
    fn reaped(&mut self) -> bool {
        self.try_wait().is_ok_and(|s| s.is_some())
    }
}
struct Cleanup<C> {
    child: C,
    record: PathBuf,
    _slot: HarnessSlot,
}
struct OwnedProcess<C: ReapOwned = Child> {
    pending: Option<Cleanup<C>>,
    retain: Arc<AtomicBool>,
    grace: Duration,
}
impl OwnedProcess {
    fn spawn(mut command: Command, retain: Arc<AtomicBool>) -> Self {
        let slot = HarnessSlot::admit();
        let home = command
            .get_envs()
            .find(|(key, _)| *key == "HOME")
            .and_then(|(_, value)| value)
            .unwrap();
        let record = Path::new(home).join(format!(
            ".owned-command-{}",
            SERIAL.fetch_add(1, Ordering::SeqCst)
        ));
        let child = command.spawn().unwrap();
        let mut owned = Self {
            pending: Some(Cleanup {
                child,
                record: record.clone(),
                _slot: slot,
            }),
            retain,
            grace: Duration::from_secs(3),
        };
        let pid = owned.child().id();
        let start = owned_start_ticks(pid).ok().flatten();
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(record)
            .unwrap();
        writeln!(file, "pid={pid} start={start:?}").unwrap();
        owned
    }
    fn child(&mut self) -> &mut Child {
        &mut self.pending.as_mut().unwrap().child
    }
}
impl<C: ReapOwned> Drop for OwnedProcess<C> {
    fn drop(&mut self) {
        // Only a Child created by this fixture is signalled, never a PID from IPC/environment.
        let Some(mut pending) = self.pending.take() else {
            return;
        };
        if !pending.child.reaped() {
            pending.child.signal_owned();
        }
        if reap_before(Instant::now() + self.grace, || pending.child.reaped()) {
            let _ = fs::remove_file(&pending.record);
            return;
        }
        self.retain.store(true, Ordering::Release);
        eprintln!(
            "owned command reaping outstanding; retaining {}",
            pending.record.display()
        );
        // Retain both Child and its four-slot admission until reaped, without blocking Drop.
        let pending = Arc::new(Mutex::new(Some(pending)));
        let worker = pending.clone();
        if thread::Builder::new()
            .name("owned-harness-reap".into())
            .spawn(move || {
                loop {
                    let mut held = worker.lock().unwrap();
                    if held.as_mut().unwrap().child.reaped() {
                        let done = held.take().unwrap();
                        let _ = fs::remove_file(&done.record);
                        return;
                    }
                    drop(held);
                    thread::sleep(Duration::from_millis(10));
                }
            })
            .is_err()
        {
            // Bounded by HARNESS_CHILDREN; failed thread creation must not lose ownership.
            static RETAINED: OnceLock<Mutex<Vec<Box<dyn Send>>>> = OnceLock::new();
            RETAINED
                .get_or_init(Mutex::default)
                .lock()
                .unwrap()
                .push(Box::new(pending));
        }
    }
}
fn read_bounded(mut pipe: impl Read + std::os::fd::AsFd) -> String {
    let flags = rustix::fs::fcntl_getfl(&pipe).unwrap();
    rustix::fs::fcntl_setfl(&pipe, flags | rustix::fs::OFlags::NONBLOCK).unwrap();
    let deadline = Instant::now() + Duration::from_millis(250);
    let mut bytes = Vec::new();
    let mut buffer = [0; 1024];
    loop {
        assert!(Instant::now() < deadline, "owned command pipe deadline");
        match pipe.read(&mut buffer) {
            Ok(0) => return String::from_utf8(bytes).unwrap(),
            Ok(n) => {
                assert!(bytes.len() + n <= 4096, "owned command pipe bound");
                bytes.extend_from_slice(&buffer[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2))
            }
            Err(e) => panic!("owned command pipe: {e}"),
        }
    }
}
fn run_bounded(command: Command, retain: Arc<AtomicBool>) -> String {
    let mut child = OwnedProcess::spawn(command, retain);
    let deadline = Instant::now() + Duration::from_secs(35);
    let status = loop {
        if let Some(status) = child.child().try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "owned harness command deadline");
        thread::sleep(Duration::from_millis(10));
    };
    let out = read_bounded(child.child().stdout.take().unwrap());
    let err = read_bounded(child.child().stderr.take().unwrap());
    assert!(
        status.success(),
        "owned harness command failed: {err}; {out}"
    );
    out
}
struct InnerNest {
    runtime: PathBuf,
    home: PathBuf,
    selection: NestSelection,
    retain: Arc<AtomicBool>,
}
impl InnerNest {
    fn start(runtime: &Path, home: &Path, parent: &Path, retain: Arc<AtomicBool>) -> Self {
        let mut owned = Self {
            runtime: runtime.into(),
            home: home.into(),
            selection: NestSelection {
                signature: String::new(),
                display: String::new(),
            },
            retain,
        };
        let mut command = private_command(runtime, home);
        command
            .env("CROSSPANE_PARENT_WAYLAND_DISPLAY", parent)
            .args(["bash", "-c", "umask 077; exec \"$@\"", "--"])
            .arg(repository().join("scripts/hypr-nested.sh"))
            .args(["start", "--name", "wp415"]);
        run_bounded(command, owned.retain.clone());
        let mut command = private_command(runtime, home);
        command
            .arg(repository().join("scripts/hypr-nested.sh"))
            .args(["env", "--name", "wp415"]);
        owned.selection =
            NestSelection::parse(&run_bounded(command, owned.retain.clone())).unwrap();
        owned
            .selection
            .verify(&isolated_values(&owned.selection))
            .unwrap();
        owned
            .selection
            .connect_explicit(runtime, home, owned.retain.clone());
        owned
    }
}
impl Drop for InnerNest {
    fn drop(&mut self) {
        let state = self.runtime.join("crosspane-hypr-wp415");
        let recorded = fs::read_to_string(state.join("pid"))
            .ok()
            .and_then(|p| p.trim().parse::<u32>().ok())
            .zip(
                fs::read_to_string(state.join("start"))
                    .ok()
                    .and_then(|s| s.trim().parse::<u64>().ok()),
            );
        if thread::panicking() {
            let path = self.runtime.join("crosspane-hypr-wp415/stdout.log");
            if let Ok(file) = fs::File::open(path) {
                let mut log = String::new();
                let _ = file.take(8192).read_to_string(&mut log);
                eprintln!("owned private nest startup diagnostic: {log}");
            }
        }
        let mut command = private_command(&self.runtime, &self.home);
        command
            .arg(repository().join("scripts/hypr-nested.sh"))
            .args(["stop", "--name", "wp415"]);
        // Finally-style cleanup also runs during unwinding; avoid a second panic.
        let stopped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_bounded(command, self.retain.clone())
        }))
        .is_ok();
        let deadline = Instant::now() + Duration::from_secs(3);
        let gone = recorded.is_some_and(|(pid, start)| {
            loop {
                match owned_start_ticks(pid) {
                    Ok(None) => return true,
                    Ok(Some(now)) if now != start => return true,
                    _ if Instant::now() >= deadline => return false,
                    _ => thread::sleep(Duration::from_millis(10)),
                }
            }
        });
        if !stopped || !gone {
            self.retain.store(true, Ordering::Release);
            eprintln!(
                "owned nest stop/reaping not confirmed; preserving private records at {}; recorded={recorded:?}",
                state.display()
            );
        } else {
            eprintln!("owned private nest stop and PID/start disappearance confirmed");
        }
    }
}
fn owned_start_ticks(pid: u32) -> std::io::Result<Option<u64>> {
    let file = match fs::File::open(format!("/proc/{pid}/stat")) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut text = String::new();
    file.take(4097).read_to_string(&mut text)?;
    let ticks = (text.len() <= 4096)
        .then(|| {
            text.rsplit_once(") ")
                .and_then(|(_, fields)| fields.split_whitespace().nth(19))
                .and_then(|s| s.parse::<u64>().ok())
        })
        .flatten()
        .ok_or(std::io::ErrorKind::InvalidData)?;
    Ok(Some(ticks))
}
struct OwnedTutorialProbe {
    executable: PathBuf,
    uid: u32,
}
impl ProcessProbe for OwnedTutorialProbe {
    fn snapshot(
        &self,
        pid: u32,
        deadline: &Deadline,
    ) -> std::result::Result<ProcessFacts, NativeError> {
        // The only caller is spawn_tutorial, using its held unreaped Child's PID.
        deadline.check()?;
        let path = PathBuf::from(format!("/proc/{pid}"));
        let uid = fs::metadata(&path)
            .map_err(|_| NativeError::Unavailable)?
            .uid();
        let executable = fs::read_link(path.join("exe")).map_err(|_| NativeError::Unavailable)?;
        if uid != self.uid || executable != self.executable {
            return Err(NativeError::Foreign);
        }
        let mut stat = String::new();
        fs::File::open(path.join("stat"))
            .map_err(|_| NativeError::Unavailable)?
            .take(4097)
            .read_to_string(&mut stat)
            .map_err(|_| NativeError::Unavailable)?;
        if stat.len() > 4096 {
            return Err(NativeError::Oversize);
        }
        let generation = stat
            .rsplit_once(") ")
            .and_then(|(_, fields)| fields.split_whitespace().nth(19))
            .and_then(|s| s.parse().ok())
            .filter(|ticks| *ticks > 0)
            .ok_or(NativeError::Invalid)?;
        deadline.check()?;
        Ok(ProcessFacts {
            uid,
            executable,
            generation,
        })
    }
}
fn fixture_reply(port: &mut tutorial::LinuxFixturePort, call: FixtureCall) -> FixtureMessage {
    let id = call.id;
    port.submit(call).unwrap();
    let mut answer = None;
    until(|| {
        for receipt in port.poll_receipts() {
            let packet = receipt.message;
            if packet.call_id == Some(id) {
                answer = Some(packet);
            } else {
                assert!(packet.result.is_ok(), "unexpected fixture error");
            }
        }
        answer.is_some()
    });
    answer.unwrap()
}
#[test]
fn nested_fixture_owned_window_and_closed_evidence() {
    if std::env::var_os("CROSSPANE_NESTED_HYPR").is_none() {
        eprintln!("window-only runtime skipped: named nested guard is unset; no audio coverage");
        return;
    }
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
    let state = runtime.join("crosspane-hypr-wp415");
    let meta = fs::symlink_metadata(&state).unwrap();
    assert!(
        meta.is_dir()
            && meta.uid() == rustix::process::geteuid().as_raw()
            && meta.mode() & 0o022 == 0,
        "named harness state must be owned"
    );
    let text = fs::read_to_string(state.join("env")).unwrap();
    let outer = NestSelection::parse(&text).unwrap();
    outer.verify(&std::env::vars().collect()).unwrap();
    // Recheck current owned PID/start and exact instance/display immediately before connecting.
    outer.connect_explicit(&runtime, &state, Arc::new(AtomicBool::new(false)));
    if !Path::new("/usr/bin/dbus-daemon").is_file() {
        eprintln!(
            "window-only runtime skipped: private dbus-daemon is unavailable; no audio coverage"
        );
        return;
    }
    let root_path = Scratch::path();
    let executable = root_path.join(".local/bin/crosspane-tutorial");
    let io = Arc::new(
        LinuxNativeIo::scratch(
            &root_path,
            Arc::new(NoCommands),
            Arc::new(OwnedTutorialProbe {
                executable: executable.clone(),
                uid: rustix::process::geteuid().as_raw(),
            }),
        )
        .unwrap(),
    );
    let root = Scratch::owned(root_path);
    let paths = io.target().paths();
    for path in [
        &paths.prefix,
        &paths.config_home,
        &paths.state_home,
        &paths.data_home,
        &paths.runtime_home,
        &paths.prefix.join("bin"),
        &paths.runtime_home.join("private"),
    ] {
        fs::create_dir_all(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let inner = InnerNest::start(
        &paths.runtime_home,
        &root.0,
        &runtime.join(&outer.display),
        root.1.clone(),
    );
    let bus = paths.runtime_home.join("private/bus");
    let bus_address = format!("unix:path={}", bus.display());
    let mut command = private_command(&paths.runtime_home, &root.0);
    command
        .args([
            "bash",
            "-c",
            "umask 077; exec \"$@\"",
            "--",
            "/usr/bin/dbus-daemon",
            "--session",
            "--nofork",
            "--nopidfile",
            "--nosyslog",
        ])
        .arg(format!("--address={bus_address}"));
    let mut bus_child = OwnedProcess::spawn(command, root.1.clone());
    until(|| {
        assert!(bus_child.child().try_wait().unwrap().is_none());
        bus.exists()
    });
    let source = Path::new(env!("CARGO_BIN_EXE_crosspane-tutorial"));
    let digest = stage_artifact(source, &executable, MAX_FILE_BYTES as u64).unwrap();
    let environment = ChildEnvironment::selected(
        io.target(),
        [
            ("XDG_SESSION_ID".into(), "fixture".into()),
            ("XDG_SESSION_TYPE".into(), "wayland".into()),
            (
                "HYPRLAND_INSTANCE_SIGNATURE".into(),
                inner.selection.signature.clone(),
            ),
            ("WAYLAND_DISPLAY".into(), inner.selection.display.clone()),
            ("DBUS_SESSION_BUS_ADDRESS".into(), bus_address),
        ]
        .into(),
    )
    .unwrap();
    assert_eq!(
        environment.values()["XDG_RUNTIME_DIR"],
        paths.runtime_home.to_str().unwrap()
    );
    assert_eq!(environment.values()["DBUS_SYSTEM_BUS_ADDRESS"], DEAD_SYSTEM);
    for key in [
        "PIPEWIRE_REMOTE",
        "PULSE_SERVER",
        "CROSSPANE_AUDIO",
        "CROSSPANE_PARENT_WAYLAND_DISPLAY",
    ] {
        assert!(!environment.values().contains_key(key));
    }
    let font = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/fonts/LiberationSans-Regular.ttf");
    let start = Instant::now();
    let clock: FixtureClock = Arc::new(move || start.elapsed().as_millis() as u64);
    let mut preparing =
        tutorial::launch(io.clone(), proof(&io), environment, digest, font, clock).unwrap();
    let mut port = None;
    until(|| {
        if let Some(result) = preparing.poll() {
            port = Some(result.unwrap());
        }
        port.is_some()
    });
    let mut port = port.unwrap();
    let opened = fixture_reply(
        &mut port,
        FixtureCall {
            id: 1,
            attempt: AttemptId(1),
            command: FixtureCommand::Open {
                machine_label: "TEST_MACHINE".into(),
            },
        },
    );
    let (fixture, pid, window) = match opened.result.unwrap() {
        FixtureEvent::Opened {
            fixture,
            pid,
            window,
            ..
        } => (fixture, pid, window),
        other => panic!("expected opened, got {other:?}"),
    };
    let ipc = TutorialIpc::new(&inner.selection.signature, &paths.runtime_home).unwrap();
    let clients = ipc.json("clients").unwrap();
    let own: Vec<_> = clients
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["pid"].as_u64() == Some(u64::from(pid)) && c["title"].as_str() == Some(TITLE))
        .collect();
    assert_eq!(own.len(), 1);
    let stable = own[0]["stableId"].as_str().unwrap();
    assert_eq!(
        window,
        WindowId(u64::from_str_radix(stable.strip_prefix("0x").unwrap_or(stable), 16).unwrap())
    );
    let observed = fixture_reply(
        &mut port,
        FixtureCall {
            id: 2,
            attempt: AttemptId(1),
            command: FixtureCommand::ObserveWindow { fixture },
        },
    );
    assert!(matches!(
        observed.result,
        Ok(FixtureEvent::Snapshot {
            snapshot: FixtureSnapshot {
                window_facts: OwnWindowFacts::Present { .. },
                ..
            }
        })
    ));
    let armed = fixture_reply(
        &mut port,
        FixtureCall {
            id: 3,
            attempt: AttemptId(1),
            command: FixtureCommand::ArmTarget {
                fixture,
                phase: PhaseId(1),
            },
        },
    );
    assert!(matches!(
        armed.result,
        Ok(FixtureEvent::TargetArmed {
            phase: PhaseId(1),
            ..
        })
    ));
    // Close itself is not success. Accept Closed only from the owned cleanup path.
    port.submit(FixtureCall {
        id: 4,
        attempt: AttemptId(1),
        command: FixtureCommand::Close { fixture },
    })
    .unwrap();
    let mut terminal = None;
    until(|| {
        for receipt in port.poll_receipts() {
            if matches!(
                receipt.message.result,
                Ok(FixtureEvent::Closed { .. }) | Err(FixtureError::ChildExited)
            ) {
                assert!(terminal.is_none());
                terminal = Some(receipt.message.result);
            }
        }
        let _ = port.complete_closed(AttemptId(1), fixture);
        terminal.is_some()
    });
    if matches!(terminal, Some(Ok(FixtureEvent::Closed { .. }))) {
        assert!(
            ipc.json("clients")
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .all(|c| c["pid"].as_u64() != Some(u64::from(pid)))
        );
        eprintln!("window-only owned runtime: Closed confirmed by reaping and fresh absence");
    } else {
        eprintln!(
            "window-only owned runtime: EOF cleanup unconfirmed, honest ChildExited; no Closed claim"
        );
    }
    assert!(port.poll_receipts().is_empty());
    drop(port);
    drop(bus_child);
    drop(inner);
    assert!(
        !root.1.load(Ordering::Acquire),
        "owned inner nest cleanup must be proven before removing its records"
    );
    drop(root);
}
impl Drop for Scratch {
    fn drop(&mut self) {
        if !self.1.load(Ordering::Acquire) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
fn until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !condition() {
        assert!(Instant::now() < deadline, "bounded fake wait");
        thread::sleep(Duration::from_millis(1));
    }
}
fn client() -> Value {
    json!({"pid":77,"title":TITLE,"stableId":"1800000c","address":"0xdeadbeef","mapped":true,"hidden":false,"monitor":2,"workspace":{"id":1,"name":"1"}})
}
fn monitors() -> Value {
    json!([{"id":2,"activeWorkspace":{"id":1},"specialWorkspace":{"id":0}},{"id":3,"activeWorkspace":{"id":2},"specialWorkspace":{"id":0}}])
}
fn observe(
    observer: &mut tutorial::WindowObserver,
    clients: Value,
    monitors: Value,
) -> tutorial_window::WindowObservation {
    observer.observe(&clients, &monitors, 77, TITLE)
}

#[test]
fn b2_stable_id_mapping_uses_exact_pid_title_and_never_address() {
    let mut observer = tutorial::WindowObserver::default();
    let mut foreign = client();
    foreign["pid"] = json!(78);
    foreign["title"] = json!("UNRELATED_SYNTHETIC_TITLE");
    foreign["stableId"] = json!("1800000d");
    let (id, facts) = observe(&mut observer, json!([foreign, client()]), monitors()).unwrap();
    assert_eq!(id, WindowId(0x1800000c));
    assert_eq!(
        facts,
        OwnWindowFacts::Present {
            visible_on_user_workspace: Some(true),
            on_initial_display: Some(true)
        }
    );
    for stable in ["0x1800000c", "1800000C"] {
        let mut c = client();
        c["stableId"] = json!(stable);
        c["address"] = json!("INVALID_IGNORED_ADDRESS");
        assert_eq!(
            observe(&mut observer, json!([c]), monitors()).unwrap().0,
            id
        );
    }
}
#[test]
fn b2_missing_before_identity_and_ambiguity_do_not_guess_titles() {
    let mut observer = tutorial::WindowObserver::default();
    assert_eq!(
        observe(&mut observer, json!([]), monitors()),
        Err(FixtureError::UnknownWindow)
    );
    assert_eq!(
        observe(&mut observer, json!([client(), client()]), monitors()),
        Err(FixtureError::AmbiguousWindow)
    );
    let mut c = client();
    c["title"] = json!(format!("{TITLE} suffix"));
    assert_eq!(
        observe(&mut observer, json!([c]), monitors()),
        Err(FixtureError::UnknownWindow)
    );
    assert_eq!(
        observer.observe(&json!([client()]), &monitors(), 77, "uncontrolled"),
        Err(FixtureError::NotOwned)
    );
}
#[test]
fn b2_established_identity_never_rebinds_pid_stable_id_or_title() {
    for field in ["pid", "stableId", "title"] {
        let mut observer = tutorial::WindowObserver::default();
        observe(&mut observer, json!([client()]), monitors()).unwrap();
        let mut changed = client();
        changed[field] = match field {
            "pid" => json!(78),
            "stableId" => json!("ff"),
            _ => json!("OTHER_SYNTHETIC_TITLE"),
        };
        assert_eq!(
            observe(&mut observer, json!([changed]), monitors()),
            Err(FixtureError::UnknownWindow)
        );
        assert_eq!(
            observe(&mut observer, json!([]), monitors()),
            Ok((WindowId(0x1800000c), OwnWindowFacts::Missing))
        );
    }
}
#[test]
fn b2_visibility_parked_hidden_unmapped_and_original_display_are_facts() {
    let mut observer = tutorial::WindowObserver::default();
    observe(&mut observer, json!([client()]), monitors()).unwrap();
    for changed in [
        json!({"mapped":false}),
        json!({"hidden":true}),
        json!({"workspace":{"id":-7,"name":"special:crosspane-test"}}),
    ] {
        let mut c = client();
        for (key, value) in changed.as_object().unwrap() {
            c[key] = value.clone();
        }
        assert_eq!(
            observe(&mut observer, json!([c]), monitors()).unwrap().1,
            OwnWindowFacts::Present {
                visible_on_user_workspace: Some(false),
                on_initial_display: Some(true)
            }
        );
    }
    let mut moved = client();
    moved["monitor"] = json!(3);
    moved["workspace"]["id"] = json!(2);
    assert_eq!(
        observe(&mut observer, json!([moved]), monitors())
            .unwrap()
            .1,
        OwnWindowFacts::Present {
            visible_on_user_workspace: Some(true),
            on_initial_display: Some(false)
        }
    );
    assert_eq!(
        observe(&mut observer, json!([client()]), Value::Null)
            .unwrap()
            .1,
        OwnWindowFacts::Present {
            visible_on_user_workspace: None,
            on_initial_display: None
        }
    );
    let mut unknown = tutorial::WindowObserver::default();
    observe(&mut unknown, json!([client()]), Value::Null).unwrap();
    assert_eq!(
        observe(&mut unknown, json!([client()]), monitors())
            .unwrap()
            .1,
        OwnWindowFacts::Present {
            visible_on_user_workspace: Some(true),
            on_initial_display: None
        }
    );
}
#[test]
fn b2_malformed_and_bounded_observations_refuse_without_window_success() {
    for id in [
        json!(""),
        json!("0"),
        json!("not-hex"),
        json!(null),
        json!("fffffffffffffffff"),
    ] {
        let mut c = client();
        c["stableId"] = id;
        assert!(
            observe(
                &mut tutorial::WindowObserver::default(),
                json!([c]),
                monitors()
            )
            .is_err()
        );
    }
    assert!(
        observe(
            &mut tutorial::WindowObserver::default(),
            json!({}),
            monitors()
        )
        .is_err()
    );
    assert!(
        observe(
            &mut tutorial::WindowObserver::default(),
            json!(vec![client(); 4097]),
            monitors()
        )
        .is_err()
    );
}

struct Server {
    root: Scratch,
    path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    values: Arc<Mutex<(Value, Value)>>,
    calls: Arc<Mutex<Vec<String>>>,
    delay: Arc<AtomicU64>,
}
impl Server {
    fn new() -> Self {
        let root = Scratch::new();
        let parent = root.0.join("hypr/fixture");
        fs::create_dir_all(&parent).unwrap();
        fs::set_permissions(root.0.join("hypr"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        let path = parent.join(".socket.sock");
        let listener = UnixListener::bind(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let values = Arc::new(Mutex::new((json!([client()]), monitors())));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let delay = Arc::new(AtomicU64::new(0));
        let (s, v, c, d) = (stop.clone(), values.clone(), calls.clone(), delay.clone());
        let worker = thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                if s.load(Ordering::Acquire) {
                    return;
                }
                stream
                    .set_read_timeout(Some(Duration::from_millis(250)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_millis(250)))
                    .unwrap();
                let mut bytes = [0; 64];
                let n = stream.read(&mut bytes).unwrap();
                let command = String::from_utf8(bytes[..n].to_vec()).unwrap();
                c.lock().unwrap().push(command.clone());
                thread::sleep(Duration::from_millis(d.load(Ordering::Acquire)));
                let values = v.lock().unwrap();
                let value = match command.as_str() {
                    "j/clients" => &values.0,
                    "j/monitors" => &values.1,
                    _ => panic!("unexpected read-only query"),
                };
                let _ = stream.write_all(serde_json::to_string(value).unwrap().as_bytes());
            }
        });
        Self {
            root,
            path,
            stop,
            thread: Some(worker),
            values,
            calls,
            delay,
        }
    }
    fn ipc(&self) -> TutorialIpc {
        TutorialIpc::new("fixture", &self.root.0).unwrap()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = UnixStream::connect(&self.path);
        if let Some(worker) = self.thread.take() {
            worker.join().unwrap();
        }
    }
}
#[test]
fn b2_ipc_is_fresh_read_only_and_pins_socket_and_ancestors() {
    let server = Server::new();
    let ipc = server.ipc();
    assert_eq!(ipc.json("clients").unwrap(), json!([client()]));
    ipc.json("monitors").unwrap();
    server.values.lock().unwrap().0 = json!([]);
    assert_eq!(ipc.json("clients").unwrap(), json!([]));
    assert!(ipc.json("dispatch").is_err());
    assert_eq!(
        *server.calls.lock().unwrap(),
        ["j/clients", "j/monitors", "j/clients"]
    );
    let saved = server.path.with_extension("saved");
    fs::rename(&server.path, &saved).unwrap();
    let replacement = UnixListener::bind(&server.path).unwrap();
    fs::set_permissions(&server.path, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(ipc.json("clients").is_err());
    drop(replacement);
    fs::remove_file(&server.path).unwrap();
    fs::rename(saved, &server.path).unwrap();
    let old = server.root.0.join("hypr");
    let saved = server.root.0.join("saved");
    fs::rename(&old, &saved).unwrap();
    symlink(&saved, &old).unwrap();
    assert!(ipc.json("clients").is_err());
    fs::remove_file(&old).unwrap();
    fs::rename(saved, old).unwrap();
}
#[test]
fn b2_ipc_stall_is_bounded_and_signature_or_writable_socket_refused() {
    let server = Server::new();
    for signature in ["", ".", "..", "fixture/other", "fixture\n", "fixture."] {
        assert!(TutorialIpc::new(signature, &server.root.0).is_err());
    }
    fs::set_permissions(&server.path, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(TutorialIpc::new("fixture", &server.root.0).is_err());
    fs::set_permissions(&server.path, fs::Permissions::from_mode(0o700)).unwrap();
    server.delay.store(350, Ordering::Release);
    let started = Instant::now();
    assert!(server.ipc().json("clients").is_err());
    assert!(started.elapsed() < Duration::from_millis(650));
}
#[test]
fn b2_native_tone_stays_explicitly_unavailable_until_b3() {
    let mut native = tutorial::native(Path::new("/inert/font"));
    assert_eq!(
        native.play_tone(
            ToneId(1),
            &SpeakersSelection {
                peer: crosspane_types::id::NodeId([1; 32]),
                device_key: "inert".into()
            }
        ),
        Some(Err(FixtureError::Unavailable))
    );
    assert_eq!(
        native.stop_tone(ToneId(1)),
        Some(Err(FixtureError::Unavailable))
    );
    assert_eq!(native.tone_state(), OwnToneState::Stopped);
}

struct NoCommands;
impl CommandRunner for NoCommands {
    fn run(
        &self,
        _: &CommandSpec,
        _: &Deadline,
    ) -> std::result::Result<CommandOutput, NativeError> {
        panic!("native commands forbidden in ordinary test")
    }
}
struct Probe {
    facts: Mutex<VecDeque<ProcessFacts>>,
}
impl ProcessProbe for Probe {
    fn snapshot(&self, _: u32, _: &Deadline) -> std::result::Result<ProcessFacts, NativeError> {
        let mut facts = self.facts.lock().unwrap();
        if facts.len() > 1 {
            Ok(facts.pop_front().unwrap())
        } else {
            facts.front().cloned().ok_or(NativeError::Unavailable)
        }
    }
}
struct Admission {
    io: Arc<LinuxNativeIo>,
    root: Scratch,
    probe: Arc<Probe>,
    proof: SupportProof,
    environment: ChildEnvironment,
    font: PathBuf,
    path: PathBuf,
}
fn proof(io: &LinuxNativeIo) -> SupportProof {
    io.scratch_support(SupportObservations {
        uid: io.target().paths().uid,
        architecture: std::env::consts::ARCH.into(),
        arch_based: true,
        hyprland_version: [0, 56, 0],
        protocols_ready: true,
        runtime_libraries_ready: true,
        uwsm_managed: true,
        graphical_target_active: true,
        graphical_sessions: 1,
        session_id: "fixture".into(),
        session_type: "wayland".into(),
        seat: "seat0".into(),
        active: true,
    })
    .unwrap()
}
impl Admission {
    fn new() -> Self {
        let path = Scratch::path();
        let probe = Arc::new(Probe {
            facts: Mutex::new(VecDeque::new()),
        });
        let io =
            Arc::new(LinuxNativeIo::scratch(&path, Arc::new(NoCommands), probe.clone()).unwrap());
        let root = Scratch::owned(path);
        fs::create_dir_all(io.target().paths().prefix.join("bin")).unwrap();
        let path = io.target().paths().prefix.join("bin/crosspane-tutorial");
        fs::write(&path, b"INERT_TUTORIAL_BYTES_NEVER_EXECUTED").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let proof = proof(&io);
        let environment = ChildEnvironment::selected(
            io.target(),
            [
                ("XDG_SESSION_ID".into(), "fixture".into()),
                ("XDG_SESSION_TYPE".into(), "wayland".into()),
                ("HYPRLAND_INSTANCE_SIGNATURE".into(), "fixture".into()),
                ("WAYLAND_DISPLAY".into(), "wayland-fixture".into()),
            ]
            .into(),
        )
        .unwrap();
        let font = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/fonts/LiberationSans-Regular.ttf");
        Self {
            io,
            root,
            probe,
            proof,
            environment,
            font,
            path,
        }
    }
    fn admit(&self) -> std::result::Result<native_io::tutorial::TutorialCommand, NativeError> {
        self.io.admit_tutorial(
            &self.proof,
            &self.environment,
            crosspane_installer::platform::linux::payload::sha256(
                b"INERT_TUTORIAL_BYTES_NEVER_EXECUTED",
            ),
            &self.font,
            &Deadline::new(2000, Cancellation::default()).unwrap(),
        )
    }
}
#[test]
fn b2_native_admission_pins_digest_prefix_font_and_selected_environment() {
    let a = Admission::new();
    let command = a.admit().unwrap();
    assert!(!format!("{command:?}").contains("INERT_TUTORIAL_BYTES"));
    assert!(
        a.io.admit_tutorial(
            &a.proof,
            &a.environment,
            [0; 32],
            &a.font,
            &Deadline::new(2000, Cancellation::default()).unwrap()
        )
        .is_err()
    );
    for font in [
        a.root.0.join("font.ttf"),
        PathBuf::from("/usr/share/fonts/foreign.ttf"),
    ] {
        assert!(
            a.io.admit_tutorial(
                &a.proof,
                &a.environment,
                [0; 32],
                &font,
                &Deadline::new(2000, Cancellation::default()).unwrap()
            )
            .is_err()
        );
    }
    let other = Admission::new();
    assert!(
        a.io.admit_tutorial(
            &a.proof,
            &other.environment,
            [0; 32],
            &a.font,
            &Deadline::new(2000, Cancellation::default()).unwrap()
        )
        .is_err()
    );
    assert!(
        CommandSpec::new(
            a.path.clone(),
            vec![
                "--controlled".into(),
                "--font".into(),
                a.font.to_str().unwrap().into()
            ],
            a.environment.clone(),
            4096
        )
        .is_err()
    );
}
#[test]
fn b2_native_admission_refuses_links_nonexecutables_writes_and_oversize() {
    let a = Admission::new();
    for mode in [0o600, 0o777, 0o4700, 0o2700] {
        fs::set_permissions(&a.path, fs::Permissions::from_mode(mode)).unwrap();
        assert!(a.admit().is_err());
    }
    fs::set_permissions(&a.path, fs::Permissions::from_mode(0o700)).unwrap();
    let alias = a.root.0.join("alias");
    fs::hard_link(&a.path, &alias).unwrap();
    assert!(a.admit().is_err());
    fs::remove_file(alias).unwrap();
    let saved = a.root.0.join("saved");
    fs::rename(&a.path, &saved).unwrap();
    symlink(&saved, &a.path).unwrap();
    assert!(a.admit().is_err());
    fs::remove_file(&a.path).unwrap();
    fs::rename(saved, &a.path).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&a.path)
        .unwrap()
        .set_len(MAX_FILE_BYTES as u64 + 1)
        .unwrap();
    assert!(a.admit().is_err());
}
#[test]
fn b2_tutorial_process_verifier_has_its_own_uid_executable_and_start_ticks() {
    let a = Admission::new();
    let command = a.admit().unwrap();
    let good = ProcessFacts {
        uid: a.io.target().paths().uid,
        executable: a.path.clone(),
        generation: 123,
    };
    *a.probe.facts.lock().unwrap() = vec![good.clone()].into();
    let identity =
        a.io.tutorial_identity(
            &command,
            42,
            &Deadline::new(2000, Cancellation::default()).unwrap(),
        )
        .unwrap();
    assert_eq!(identity.start_ticks, 123);
    assert_eq!(identity.executable, a.path);
    for bad in [
        ProcessFacts {
            uid: good.uid + 1,
            ..good.clone()
        },
        ProcessFacts {
            executable: a.io.target().agent_path(),
            ..good.clone()
        },
        ProcessFacts {
            generation: 0,
            ..good.clone()
        },
    ] {
        *a.probe.facts.lock().unwrap() = vec![bad].into();
        assert!(
            a.io.tutorial_identity(
                &command,
                42,
                &Deadline::new(2000, Cancellation::default()).unwrap()
            )
            .is_err()
        );
    }
    *a.probe.facts.lock().unwrap() = vec![
        good.clone(),
        ProcessFacts {
            generation: 124,
            ..good
        },
    ]
    .into();
    assert!(
        a.io.tutorial_identity(
            &command,
            42,
            &Deadline::new(2000, Cancellation::default()).unwrap()
        )
        .is_err()
    );
}
#[test]
fn b2_cancelled_native_spawn_does_not_execute_inert_bytes_or_probe() {
    let a = Admission::new();
    let command = a.admit().unwrap();
    let cancel = Cancellation::default();
    let deadline = Deadline::new(2000, cancel.clone()).unwrap();
    cancel.cancel();
    assert!(matches!(
        a.io.spawn_tutorial(command, &deadline),
        Err(NativeError::Cancelled)
    ));
    assert!(a.probe.facts.lock().unwrap().is_empty());
}

#[derive(Default)]
struct PipeState {
    input: Vec<u8>,
    output: VecDeque<u8>,
    eof: bool,
    reaped: bool,
    absent: bool,
    checks: usize,
    retired: bool,
    cleanup_error: Option<FixtureError>,
}
struct MemoryChild(Arc<Mutex<PipeState>>);
impl InheritedFixtureChild for MemoryChild {
    fn pid(&self) -> u32 {
        77
    }
    fn read(&mut self, byte: &mut [u8]) -> std::io::Result<usize> {
        let mut s = self.0.lock().unwrap();
        if let Some(b) = s.output.pop_front() {
            byte[0] = b;
            Ok(1)
        } else if s.eof {
            Ok(0)
        } else {
            Err(std::io::ErrorKind::WouldBlock.into())
        }
    }
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().input.extend(bytes);
        Ok(bytes.len())
    }
    fn cleanup_confirmed(&mut self) -> std::result::Result<bool, FixtureError> {
        let mut s = self.0.lock().unwrap();
        s.checks += 1;
        if let Some(error) = s.cleanup_error {
            return Err(error);
        }
        Ok(s.reaped && s.absent)
    }
    fn retire(&mut self) {
        self.0.lock().unwrap().retired = true;
    }
}
fn message(sequence: u64, call_id: Option<u64>, event: FixtureEvent) -> Vec<u8> {
    encode_event(&FixtureEventPacket {
        schema_version: 1,
        message: FixtureMessage {
            call_id,
            attempt: AttemptId(1),
            sequence,
            result: Ok(event),
        },
    })
    .unwrap()
}
#[test]
fn b2_accepted_native_close_then_eof_before_parent_poll_reuses_cleanup_and_accept() {
    for (reaped, absent, closed, cleanup_error) in [
        (true, true, true, None),
        (true, false, false, None),
        (false, true, false, None),
        (true, true, false, Some(FixtureError::CleanupFailed)),
        (true, true, false, Some(FixtureError::TimedOut)),
    ] {
        let shared = Arc::new(Mutex::new(PipeState::default()));
        let mut port =
            PipeFixturePort::new(Box::new(MemoryChild(shared.clone())), Arc::new(|| 1)).unwrap();
        port.submit(FixtureCall {
            id: 1,
            attempt: AttemptId(1),
            command: FixtureCommand::Open {
                machine_label: "TEST_MACHINE".into(),
            },
        })
        .unwrap();
        until(|| shared.lock().unwrap().input.ends_with(b"\n"));
        let mut s = shared.lock().unwrap();
        s.output.extend(message(
            1,
            Some(1),
            FixtureEvent::Opened {
                fixture: FixtureId(1),
                pid: 77,
                window: WindowId(12),
                label: "TEST_MACHINE".into(),
            },
        ));
        s.output.extend(message(
            2,
            None,
            FixtureEvent::CloseRequested {
                fixture: FixtureId(1),
            },
        ));
        s.reaped = reaped;
        s.absent = absent;
        s.eof = true;
        s.cleanup_error = cleanup_error;
        drop(s);
        // No parent poll or complete_closed call occurs before the worker consumes EOF.
        until(|| shared.lock().unwrap().retired);
        let receipts = port.poll_receipts();
        assert!(
            receipts
                .iter()
                .any(|r| matches!(r.message.result, Ok(FixtureEvent::CloseRequested { .. })))
        );
        assert_eq!(
            receipts
                .iter()
                .filter(|r| matches!(r.message.result, Ok(FixtureEvent::Closed { .. })))
                .count(),
            usize::from(closed)
        );
        assert_eq!(
            receipts
                .iter()
                .any(|r| r.message.result == Err(FixtureError::ChildExited)),
            !closed
        );
        assert!(shared.lock().unwrap().checks > 0);
        assert!(port.poll_receipts().is_empty());
    }
}

#[test]
fn b2_written_close_then_eof_completes_once_without_parent_confirmation_queue() {
    let shared = Arc::new(Mutex::new(PipeState::default()));
    let mut port =
        PipeFixturePort::new(Box::new(MemoryChild(shared.clone())), Arc::new(|| 1)).unwrap();
    port.submit(FixtureCall {
        id: 1,
        attempt: AttemptId(1),
        command: FixtureCommand::Open {
            machine_label: "TEST_MACHINE".into(),
        },
    })
    .unwrap();
    until(|| shared.lock().unwrap().input.ends_with(b"\n"));
    shared.lock().unwrap().output.extend(message(
        1,
        Some(1),
        FixtureEvent::Opened {
            fixture: FixtureId(1),
            pid: 77,
            window: WindowId(12),
            label: "TEST_MACHINE".into(),
        },
    ));
    let mut opened = false;
    until(|| {
        opened |= port
            .poll_receipts()
            .iter()
            .any(|r| matches!(r.message.result, Ok(FixtureEvent::Opened { .. })));
        opened
    });
    port.submit(FixtureCall {
        id: 2,
        attempt: AttemptId(1),
        command: FixtureCommand::Close {
            fixture: FixtureId(1),
        },
    })
    .unwrap();
    until(|| {
        shared
            .lock()
            .unwrap()
            .input
            .split_inclusive(|b| *b == b'\n')
            .any(|line| {
                decode_control(line)
                    .is_ok_and(|p| matches!(p.call.command, FixtureCommand::Close { .. }))
            })
    });
    let mut state = shared.lock().unwrap();
    state.reaped = true;
    state.absent = true;
    state.eof = true;
    drop(state);
    // No complete_closed or parent poll occurs after the accepted Close write and before EOF.
    until(|| shared.lock().unwrap().retired);
    let receipts = port.poll_receipts();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].message.call_id, Some(2));
    assert_eq!(receipts[0].message.sequence, 2);
    assert_eq!(
        receipts[0].message.result,
        Ok(FixtureEvent::Closed {
            fixture: FixtureId(1)
        })
    );
    assert!(port.poll_receipts().is_empty());
}
