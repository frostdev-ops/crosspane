#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crosspane_installer::{
    agent_contract::ObservationSource,
    platform::linux::{detect::fonts::*, native_io::*},
};
use eframe::egui::{self, FontFamily};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

fn fixture() -> Vec<u8> {
    include_bytes!("fixtures/fonts/LiberationSans-Regular.ttf").to_vec()
}
fn deadline() -> Deadline {
    Deadline::new(2000, Cancellation::default()).unwrap()
}
struct Reader {
    matched: Result<Vec<u8>, NativeError>,
    fonts: BTreeMap<PathBuf, Result<Vec<u8>, NativeError>>,
    calls: RefCell<Vec<Option<PathBuf>>>,
    cancel: Option<Cancellation>,
    delay: Duration,
}
impl FontReader for Reader {
    fn source(&self) -> ObservationSource {
        ObservationSource::Demo
    }
    fn fc_match(&self, deadline: &Deadline) -> Result<Vec<u8>, NativeError> {
        deadline.check()?;
        self.calls.borrow_mut().push(None);
        self.matched.clone()
    }
    fn read(&self, path: &Path, deadline: &Deadline) -> Result<Vec<u8>, NativeError> {
        deadline.check()?;
        self.calls.borrow_mut().push(Some(path.into()));
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        self.fonts
            .get(path)
            .cloned()
            .unwrap_or(Err(NativeError::Unavailable))
    }
}
fn reader(matched: Result<Vec<u8>, NativeError>) -> Reader {
    Reader {
        matched,
        fonts: BTreeMap::new(),
        calls: RefCell::new(Vec::new()),
        cancel: None,
        delay: Duration::ZERO,
    }
}
fn families(font: &ValidatedFont) {
    assert_eq!(font.source, ObservationSource::Demo);
    assert_eq!(font.definitions.font_data.len(), 1);
    assert_eq!(font.definitions.font_data["review-system-font"].index, 0);
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        assert_eq!(font.definitions.families[&family], ["review-system-font"]);
    }
}
#[test]
fn fc_match_first_then_exact_fixed_candidates_and_no_duplicate_reads() {
    assert_eq!(
        CANDIDATES,
        [
            "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
            "/usr/share/fonts/noto/NotoSans-Regular.ttf"
        ]
    );
    let path = PathBuf::from("/usr/share/fonts/injected.ttf");
    let mut matched = reader(Ok(path.to_string_lossy().as_bytes().to_vec()));
    matched.fonts.insert(path.clone(), Ok(fixture()));
    let font = discover_with(&matched, &deadline()).unwrap();
    assert_eq!(font.path, path);
    families(&font);
    assert_eq!(*matched.calls.borrow(), [None, Some(path)]);
    let mut fallback = reader(Ok(CANDIDATES[0].as_bytes().to_vec()));
    fallback
        .fonts
        .insert(CANDIDATES[0].into(), Ok(b"OTTO".to_vec()));
    fallback.fonts.insert(CANDIDATES[1].into(), Ok(fixture()));
    let font = discover_with(&fallback, &deadline()).unwrap();
    assert_eq!(font.path, Path::new(CANDIDATES[1]));
    families(&font);
    assert_eq!(
        *fallback.calls.borrow(),
        [None, Some(CANDIDATES[0].into()), Some(CANDIDATES[1].into())]
    );
}
#[test]
fn malformed_oversized_foreign_and_failed_fc_match_never_select_untrusted_paths() {
    for matched in [
        Ok(Vec::new()),
        Ok(vec![0xff]),
        Ok(b"relative.ttf".to_vec()),
        Ok(b"/home/owner/private.ttf".to_vec()),
        Ok(b"/usr/share/fonts/../escape.ttf".to_vec()),
        Ok(b"/usr/share/fonts/a\nsecond".to_vec()),
        Ok(vec![b'x'; MAX_MATCH_BYTES + 1]),
        Err(NativeError::Timeout),
        Err(NativeError::Oversize),
        Err(NativeError::Unavailable),
    ] {
        let mut reader = reader(matched);
        reader.fonts.insert(CANDIDATES[0].into(), Ok(fixture()));
        assert_eq!(
            discover_with(&reader, &deadline()).unwrap().path,
            Path::new(CANDIDATES[0])
        );
        assert_eq!(*reader.calls.borrow(), [None, Some(CANDIDATES[0].into())]);
    }
    assert_eq!(
        matched_path(b"/usr/share/fonts/valid.ttf\n").unwrap(),
        Path::new("/usr/share/fonts/valid.ttf")
    );
    for path in [
        "/usr/share/fonts",
        "/usr/share/fonts-other/a",
        "/usr/share/fonts//a",
        "/usr/share/fonts/./a",
        "/usr/share/fonts/a\0",
    ] {
        assert!(matched_path(path.as_bytes()).is_err());
    }
}
#[test]
fn missing_empty_invalid_oversize_and_fallback_usability_are_explicit() {
    assert!(matches!(definitions(Vec::new()), Err(FontError::Empty)));
    assert!(matches!(
        definitions(vec![0; MAX_FONT_BYTES + 1]),
        Err(FontError::Oversize)
    ));
    for bytes in [
        b"not a font".to_vec(),
        b"OTTO".to_vec(),
        b"ttcf".to_vec(),
        vec![0, 1, 0, 0],
    ] {
        assert!(matches!(definitions(bytes), Err(FontError::Invalid)));
    }
    let mut all_missing = reader(Err(NativeError::Unavailable));
    assert!(matches!(
        discover_with(&all_missing, &deadline()),
        Err(FontError::Native(NativeError::Unavailable))
    ));
    assert_eq!(all_missing.calls.borrow().len(), 4);
    all_missing
        .fonts
        .insert(CANDIDATES[0].into(), Ok(Vec::new()));
    all_missing
        .fonts
        .insert(CANDIDATES[1].into(), Ok(b"invalid".to_vec()));
    all_missing
        .fonts
        .insert(CANDIDATES[2].into(), Ok(fixture()));
    let font = discover_with(&all_missing, &deadline()).unwrap();
    assert_eq!(font.path, Path::new(CANDIDATES[2]));
    families(&font);
}
#[test]
fn exact_sixteen_mib_and_unicode_labels_parse_headlessly_without_native_fonts() {
    let mut bytes = fixture();
    bytes.resize(MAX_FONT_BYTES, 0);
    let definitions = definitions(bytes).unwrap();
    let context = egui::Context::default();
    context.set_fonts(definitions);
    let mut output = context.run_ui(egui::RawInput::default(), |ui| {
        ui.label("Crosspane — Παράθυρα • Пара · Settings");
    });
    assert!(!output.shapes.is_empty());
    output.textures_delta.clear(); // Headless CPU test deliberately has no texture consumer.
}
#[test]
fn cancellation_and_stalled_injected_read_cannot_publish_a_font() {
    let stop = Cancellation::default();
    let d = Deadline::new(1000, stop.clone()).unwrap();
    stop.cancel();
    let reader = reader(Err(NativeError::Unavailable));
    assert!(matches!(
        discover_with(&reader, &d),
        Err(FontError::Native(NativeError::Cancelled))
    ));
    assert!(reader.calls.borrow().is_empty());
    let stop = Cancellation::default();
    let d = Deadline::new(1000, stop.clone()).unwrap();
    let mut late = self::reader(Err(NativeError::Unavailable));
    late.fonts.insert(CANDIDATES[0].into(), Ok(fixture()));
    late.cancel = Some(stop);
    assert!(matches!(
        discover_with(&late, &d),
        Err(FontError::Native(NativeError::Cancelled))
    ));
    assert_eq!(late.calls.borrow().len(), 2);
    late.cancel = None;
    late.delay = Duration::from_millis(20);
    assert!(matches!(
        discover_with(&late, &Deadline::new(5, Cancellation::default()).unwrap()),
        Err(FontError::Native(NativeError::Timeout))
    ));
}

struct Runner {
    calls: Mutex<Vec<CommandSpec>>,
    code: Option<i32>,
    stderr: Vec<u8>,
}
impl CommandRunner for Runner {
    fn run(
        &self,
        command: &CommandSpec,
        deadline: &Deadline,
    ) -> Result<CommandOutput, NativeError> {
        deadline.check()?;
        self.calls.lock().unwrap().push(command.clone());
        Ok(CommandOutput {
            code: self.code,
            stdout: b"/usr/share/fonts/injected.ttf".to_vec(),
            stderr: self.stderr.clone(),
        })
    }
}
struct NoProcesses;
impl ProcessProbe for NoProcesses {
    fn snapshot(&self, _: u32, _: &Deadline) -> Result<ProcessFacts, NativeError> {
        panic!("font lookup must never inspect a process");
    }
}
static ROOT_ID: AtomicU64 = AtomicU64::new(0);
#[test]
fn native_font_adapter_uses_exact_fake_argv_and_scratch_never_reads_host_fonts() {
    for (code, stderr) in [
        (Some(0), Vec::new()),
        (Some(1), Vec::new()),
        (None, Vec::new()),
        (Some(0), b"failure".to_vec()),
    ] {
        let root = PathBuf::from(format!(
            "/tmp/cp48-font-{}-{}",
            std::process::id(),
            ROOT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let runner = Arc::new(Runner {
            calls: Mutex::new(Vec::new()),
            code,
            stderr,
        });
        let io = LinuxNativeIo::scratch(&root, runner.clone(), Arc::new(NoProcesses)).unwrap();
        let environment = ChildEnvironment::selected(io.target(), BTreeMap::new()).unwrap();
        assert!(matches!(
            discover(&io, &environment, &deadline()),
            Err(FontError::Native(NativeError::Foreign))
        ));
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].executable(), Path::new("/usr/bin/fc-match"));
        assert_eq!(calls[0].argv(), ["-f", "%{file}", "sans-serif"]);
        assert_eq!(calls[0].output_limit(), MAX_MATCH_BYTES);
        drop(calls);
        std::fs::remove_dir_all(root).unwrap();
    }
}
