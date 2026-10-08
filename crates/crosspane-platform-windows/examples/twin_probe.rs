//! Native rows probe for the twin display client (WP-W3.1b, rows R1–R9). Run it as a Limited user,
//! never elevated. Modes:
//! - `rows`: R1 open, R2 startup, R3 add 720p, R4 resize 1080p, R5 remove, R6 lease expiry.
//! - `crash` (R7): add a 720p twin, then end the process with code 137 and no destructors.
//! - `recover` (R8): run after `crash`; startup recovery must find the record it left behind.
//! - `absent` (R9): run without the driver; `open` must give `Absent` quickly.
//!
//! Each check prints `TWIN_ROW name=<row> ok=<bool> ms=<n> <facts>`. The facts describe our own
//! twins only: key, monitor id, mode, our GDI name, rect and DPI. No other display is named.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(windows)]
mod probe {
    use crosspane_platform_windows::{
        model::journal::{JOURNAL_NAME, JournalFile, TwinPhase, TwinRecord},
        twin::{
            Refusal, TwinClient, TwinConfig, TwinDisplay, TwinError, TwinMode, TwinStartup,
            recover_startup,
        },
    };
    use std::{
        fmt::Debug,
        fs,
        io::Write,
        mem::size_of,
        path::Path,
        process::exit,
        ptr::null_mut,
        thread,
        time::{Duration, Instant},
    };
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation},
        System::Threading::{GetCurrentProcess, OpenProcessToken, TerminateProcess},
    };

    /// The whole probe finishes well inside this bound; the watchdog exits with 124 past it.
    const WATCHDOG: Duration = Duration::from_secs(60);
    /// The R3 and R6 twin size, and the R4 resize target.
    const SIZE_720: (u32, u32) = (1280, 720);
    const SIZE_1080: (u32, u32) = (1920, 1080);
    /// R5: our paths must be gone this soon after the remove.
    const REMOVE_BOUND_MS: u128 = 1_500;
    /// R6: with heartbeats off, the lease (5 s) must expire inside this window, measured from just
    /// before `open_with`, where the driver's lease starts. The poll gives up after `LEASE_WAIT`.
    const LEASE_MIN_MS: u128 = 4_500;
    const LEASE_MAX_MS: u128 = 7_000;
    const LEASE_WAIT: Duration = Duration::from_secs(10);
    const LEASE_POLL: Duration = Duration::from_millis(50);
    /// R9: `open` must give `Absent` inside this bound.
    const ABSENT_BOUND_MS: u128 = 100;
    /// R7: the exit code `crash` ends with.
    const CRASH_CODE: u32 = 137;

    /// Entry point: the watchdog, the Limited check, then one mode. Exits 0 when every row passed.
    pub fn run() {
        thread::spawn(|| {
            thread::sleep(WATCHDOG);
            eprintln!("twin_probe: watchdog expired");
            exit(124);
        });
        refuse_if_elevated();
        let mode = std::env::args().nth(1).unwrap_or_default();
        if !matches!(mode.as_str(), "rows" | "crash" | "recover" | "absent") {
            eprintln!("usage: twin_probe rows|crash|recover|absent");
            exit(2);
        }
        let dir = std::env::temp_dir().join("crosspane-twin-probe");
        let ledger = dir.join(JOURNAL_NAME);
        if let Err(error) = fs::create_dir_all(&dir) {
            emit("setup", Instant::now(), Err(debug(error)));
            exit(1);
        }
        let ok = match mode.as_str() {
            "rows" => rows(&ledger),
            "crash" => crash(&ledger),
            "recover" => recover(&ledger),
            _ => absent(&ledger),
        };
        // `crash` keeps its ledger: `recover` must find the record that `crash` leaves behind.
        if mode != "crash" {
            remove_scratch(&dir, &ledger);
        }
        exit(if ok { 0 } else { 1 });
    }

    /// R1 to R6, in order. The first failed row stops the run; its TWIN_ROW line says why.
    fn rows(ledger: &Path) -> bool {
        let mut journal = match load(ledger) {
            Ok(journal) => journal,
            Err(error) => return emit("setup", Instant::now(), Err(error)),
        };

        // R1 open: the interface, hardware ID and our adapter are found, and we own no paths yet.
        let started = Instant::now();
        let mut client = match TwinClient::open() {
            Ok(client) => client,
            Err(error) => return emit("open", started, Err(debug(error))),
        };
        if !check("open", started, || {
            let paths = client.own_path_count().map_err(debug)?;
            Ok((paths == 0, format!("own_paths={paths}")))
        }) {
            return false;
        }

        // R2 startup: on the empty ledger the driver is present and a fresh LIST finds nothing.
        let started = Instant::now();
        if !check("startup", started, || {
            let startup = recover_startup(&mut journal).map_err(debug)?;
            let ok = startup.driver_present && startup.journaled == 0;
            Ok((ok, startup_facts(&startup)))
        }) {
            return false;
        }

        // R3 add 720p: the rect is 1280x720, the id and DPI are nonzero, and the disk says `Up`.
        let mut current: Option<TwinDisplay> = None;
        let started = Instant::now();
        if !check("add720", started, || {
            let twin = client.add(&mut journal, mode(SIZE_720)?).map_err(debug)?;
            let ok = rect_size(twin.rect) == SIZE_720
                && twin.monitor_id != 0
                && twin.dpi > 0
                && disk_up(ledger, &twin)?;
            let facts = twin_facts(&twin);
            current = Some(twin);
            Ok((ok, facts))
        }) {
            return false;
        }

        // R4 resize 1080p: REMOVE then ADD, so the monitor id changes and the disk says `Up(new)`.
        let started = Instant::now();
        if !check("resize1080", started, || {
            let before = current.take().ok_or_else(|| "no add720 twin".to_owned())?;
            let twin = client
                .resize(&mut journal, before.key, mode(SIZE_1080)?)
                .map_err(debug)?;
            let ok = twin.monitor_id != before.monitor_id
                && rect_size(twin.rect) == SIZE_1080
                && (twin.mode.width, twin.mode.height) == SIZE_1080
                && twin.dpi > 0
                && disk_up(ledger, &twin)?;
            let previous = before.monitor_id;
            let facts = format!("{} previous_monitor_id={previous}", twin_facts(&twin));
            current = Some(twin);
            Ok((ok, facts))
        }) {
            return false;
        }

        // R5 remove: our paths are gone within the bound, and the ledger holds no twin.
        let started = Instant::now();
        if !check("remove", started, || {
            let twin = current
                .take()
                .ok_or_else(|| "no twin to remove".to_owned())?;
            let removing = Instant::now();
            client.remove(&mut journal, twin.key).map_err(debug)?;
            let paths = client.own_path_count().map_err(debug)?;
            let gone_ms = removing.elapsed().as_millis();
            let ok = paths == 0 && gone_ms <= REMOVE_BOUND_MS && disk_records(ledger)?.is_empty();
            let facts = twin_facts(&twin);
            Ok((ok, format!("{facts} own_paths={paths} gone_ms={gone_ms}")))
        }) {
            return false;
        }
        // Heartbeats stop with this client. R6 opens its own handle with them off.
        drop(client);

        // R6 lease: with heartbeats off, our paths vanish 4.5 to 7 s after the lane opens. A later
        // REMOVE is refused as expired, closed or invalid, and the record is forgotten either way.
        let started = Instant::now();
        if !check("lease", started, || {
            let opening = Instant::now();
            let mut lease =
                TwinClient::open_with(TwinConfig { heartbeat: false }).map_err(debug)?;
            let twin = lease.add(&mut journal, mode(SIZE_720)?).map_err(debug)?;
            let added = Instant::now();
            let mut paths = lease.own_path_count().map_err(debug)?;
            while paths != 0 && opening.elapsed() < LEASE_WAIT {
                thread::sleep(LEASE_POLL);
                paths = lease.own_path_count().map_err(debug)?;
            }
            // The bound counts from before `open_with`. `lease_from_add_ms` is a fact only:
            // the same end point, measured from the return of ADD.
            let lease_ms = opening.elapsed().as_millis();
            let lease_from_add_ms = added.elapsed().as_millis();
            let expired = paths == 0 && (LEASE_MIN_MS..=LEASE_MAX_MS).contains(&lease_ms);
            let refusal = lease.remove(&mut journal, twin.key);
            let refused = matches!(
                refusal,
                Err(TwinError::Refused(
                    Refusal::Expired | Refusal::Closed | Refusal::Invalid
                ))
            );
            let remove = match &refusal {
                Ok(()) => "ok".to_owned(),
                Err(error) => debug(error),
            };
            let ok = expired && refused && disk_records(ledger)?.is_empty();
            let facts = twin_facts(&twin);
            Ok((
                ok,
                format!(
                    "{facts} lease_ms={lease_ms} lease_from_add_ms={lease_from_add_ms} own_paths={paths} remove={remove}"
                ),
            ))
        }) {
            return false;
        }
        true
    }

    /// R7: add a 720p twin and check that the disk holds it, then end the process with no
    /// destructor. The OS closes the lane's handle, so the driver retires the twin, and the ledger
    /// keeps its record for `recover`. Returns only when the row fails.
    fn crash(ledger: &Path) -> bool {
        let started = Instant::now();
        let mut journal = match load(ledger) {
            Ok(journal) => journal,
            Err(error) => return emit("crash", started, Err(error)),
        };
        let mut client = match TwinClient::open() {
            Ok(client) => client,
            Err(error) => return emit("crash", started, Err(debug(error))),
        };
        let added = check("crash", started, || {
            let twin = client.add(&mut journal, mode(SIZE_720)?).map_err(debug)?;
            let ok = disk_up(ledger, &twin)?;
            Ok((ok, twin_facts(&twin)))
        });
        if !added {
            return false;
        }
        // SAFETY: ends this process through its own pseudo-handle. On success the call never
        // returns, and no destructor runs, so the lane is closed by the OS and not by Drop.
        unsafe { TerminateProcess(GetCurrentProcess(), CRASH_CODE) };
        // Reached only when TerminateProcess failed.
        eprintln!("twin_probe: TerminateProcess failed ({})", last_error());
        false
    }

    /// R8: the ledger holds the twin that `crash` left. Startup recovery waits for it to vanish,
    /// checks that a fresh handle lists none, and clears the ledger. A second pass over the cleared
    /// file must find nothing, which repeats the fresh LIST = 0 check on its own.
    fn recover(ledger: &Path) -> bool {
        let started = Instant::now();
        let mut journal = match load(ledger) {
            Ok(journal) => journal,
            Err(error) => return emit("recover", started, Err(error)),
        };
        check("recover", started, || {
            let startup = recover_startup(&mut journal).map_err(debug)?;
            let cleared = disk_records(ledger)?.is_empty();
            let mut again = load(ledger)?;
            let second = recover_startup(&mut again).map_err(debug)?;
            let relisted = second.journaled;
            let ok = startup.driver_present && startup.journaled == 1 && cleared && relisted == 0;
            let facts = startup_facts(&startup);
            Ok((ok, format!("{facts} cleared={cleared} relisted={relisted}")))
        })
    }

    /// R9: without the driver, `open` must give `Absent` inside `ABSENT_BOUND_MS`, and startup
    /// recovery must report `driver_present = false`. If the driver is present, the row fails.
    fn absent(ledger: &Path) -> bool {
        let started = Instant::now();
        check("absent", started, || {
            let mut journal = load(ledger)?;
            let opening = Instant::now();
            let opened = TwinClient::open();
            let open_ms = opening.elapsed().as_millis();
            let label = match &opened {
                Ok(_) => "present".to_owned(),
                Err(error) => debug(error),
            };
            if opened.is_ok() {
                return Ok((false, format!("open={label} open_ms={open_ms}")));
            }
            let absent = matches!(opened, Err(TwinError::Absent)) && open_ms < ABSENT_BOUND_MS;
            let startup = recover_startup(&mut journal).map_err(debug)?;
            let ok = absent && !startup.driver_present;
            let facts = startup_facts(&startup);
            Ok((ok, format!("open={label} open_ms={open_ms} {facts}")))
        })
    }

    /// Refuses to run elevated. A token that cannot be read is a refusal too.
    fn refuse_if_elevated() {
        let started = Instant::now();
        match elevated() {
            Ok(false) => {
                emit("limited", started, Ok((true, "elevated=false".to_owned())));
            }
            Ok(true) => {
                emit("limited", started, Ok((false, "elevated=true".to_owned())));
                exit(3);
            }
            Err(code) => {
                emit("limited", started, Err(format!("token_error={code}")));
                exit(3);
            }
        }
    }

    /// Whether this process token is elevated. A token that cannot be read is an error.
    fn elevated() -> Result<bool, u32> {
        let mut token: HANDLE = null_mut();
        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned = 0u32;
        // SAFETY: opens this process's own token with query access only. `token` is closed below.
        let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
        if opened == 0 {
            return Err(last_error());
        }
        let size = u32::try_from(size_of::<TOKEN_ELEVATION>()).unwrap_or(0);
        // SAFETY: `token` is the handle opened above. The buffer is one TOKEN_ELEVATION, and `size`
        // is its exact length.
        let read = unsafe {
            GetTokenInformation(
                token,
                TokenElevation,
                (&raw mut elevation).cast(),
                size,
                &mut returned,
            )
        };
        let failure = (read == 0).then(last_error);
        // SAFETY: closes the token handle opened above, exactly once.
        unsafe { CloseHandle(token) };
        match failure {
            Some(code) => Err(code),
            None => Ok(elevation.TokenIsElevated != 0),
        }
    }

    fn last_error() -> u32 {
        std::io::Error::last_os_error()
            .raw_os_error()
            .and_then(|code| u32::try_from(code).ok())
            .unwrap_or(0)
    }

    /// Prints one row and reports whether it passed. A failed call is a failed row with its error.
    fn emit(name: &str, started: Instant, result: Result<(bool, String), String>) -> bool {
        let ms = started.elapsed().as_millis();
        let (ok, facts) = match result {
            Ok(outcome) => outcome,
            Err(error) => (false, format!("error={error}")),
        };
        println!("TWIN_ROW name={name} ok={ok} ms={ms} {facts}");
        let _ = std::io::stdout().flush();
        ok
    }

    /// Runs one row's body under its timer.
    fn check(
        name: &str,
        started: Instant,
        body: impl FnOnce() -> Result<(bool, String), String>,
    ) -> bool {
        emit(name, started, body())
    }

    /// Error text for a row. The error types here print static text, numbers and codes only.
    fn debug(error: impl Debug) -> String {
        format!("{error:?}")
    }

    fn load(ledger: &Path) -> Result<JournalFile, String> {
        JournalFile::open(ledger).map_err(debug)
    }

    /// The ledger records on disk, read through a fresh open.
    fn disk_records(ledger: &Path) -> Result<Vec<TwinRecord>, String> {
        Ok(load(ledger)?.twins().cloned().collect())
    }

    /// Whether the file on disk holds exactly one record: this twin, `Up`, with its monitor id.
    fn disk_up(ledger: &Path, twin: &TwinDisplay) -> Result<bool, String> {
        let records = disk_records(ledger)?;
        Ok(matches!(
            records.as_slice(),
            [record] if record.key == twin.key.0
                && record.phase == TwinPhase::Up
                && record.monitor_id == Some(twin.monitor_id)
        ))
    }

    /// An explicit driver mode for `size` at 96 DPI. The client accepts any valid CPD mode;
    /// M2 parking itself always asks for 1920x1080 (`twin_mode`).
    fn mode(size: (u32, u32)) -> Result<TwinMode, String> {
        let mm = |px: u32| (f64::from(px) * 25.4 / 96.0).round() as u32;
        let mode = TwinMode {
            width: size.0,
            height: size.1,
            width_mm: mm(size.0),
            height_mm: mm(size.1),
        };
        if crosspane_platform_windows::model::cpd::valid_mode(mode) {
            Ok(mode)
        } else {
            Err(format!("invalid probe mode {}x{}", size.0, size.1))
        }
    }

    /// Width and height of a physical rect, or zero for an inverted one.
    fn rect_size(rect: [i32; 4]) -> (u32, u32) {
        let width = rect[2]
            .checked_sub(rect[0])
            .and_then(|w| u32::try_from(w).ok());
        let height = rect[3]
            .checked_sub(rect[1])
            .and_then(|h| u32::try_from(h).ok());
        (width.unwrap_or(0), height.unwrap_or(0))
    }

    /// Our own twin's facts only: key, monitor id, mode, our GDI name, rect and DPI.
    fn twin_facts(twin: &TwinDisplay) -> String {
        let key = twin.key.0;
        let id = twin.monitor_id;
        let (width, height) = (twin.mode.width, twin.mode.height);
        let gdi = &twin.gdi_name;
        let [left, top, right, bottom] = twin.rect;
        let dpi = twin.dpi;
        format!(
            "key={key} monitor_id={id} mode={width}x{height} gdi={gdi} rect={left},{top},{right},{bottom} dpi={dpi}"
        )
    }

    fn startup_facts(startup: &TwinStartup) -> String {
        let driver = startup.driver_present;
        let journaled = startup.journaled;
        let stale = startup.stale_seen;
        let waited = startup.waited_ms;
        format!(
            "driver_present={driver} journaled={journaled} stale_seen={stale} waited_ms={waited}"
        )
    }

    /// Removes the scratch ledger, then its directory when that is empty.
    fn remove_scratch(dir: &Path, ledger: &Path) {
        let _ = fs::remove_file(ledger);
        let _ = fs::remove_dir(dir);
    }
}

#[cfg(windows)]
fn main() {
    probe::run();
}

#[cfg(not(windows))]
fn main() {}
