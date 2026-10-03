#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // AGENTS.md permits assertions in test fixtures.
use crosspane_installer::gui;
#[path = "../src/platform/macos/fonts.rs"]
#[allow(dead_code)]
mod subject;
use eframe::egui::{FontDefinitions, FontFamily};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};
use subject::*;

struct Reader {
    result: [Result<Vec<u8>, FontError>; 2],
    calls: Mutex<Vec<SystemFont>>,
}
impl FontReader for Reader {
    fn read(&self, candidate: SystemFont) -> Result<Vec<u8>, FontError> {
        self.calls.lock().unwrap().push(candidate);
        self.result[match candidate {
            SystemFont::Sfns => 0,
            SystemFont::Helvetica => 1,
        }]
        .clone()
    }
}
fn reader(first: Result<Vec<u8>, FontError>, second: Result<Vec<u8>, FontError>) -> Reader {
    Reader {
        result: [first, second],
        calls: Mutex::default(),
    }
}
fn fixture() -> Vec<u8> {
    std::fs::read("/System/Library/Fonts/SFNS.ttf")
        .expect("lead-approved read-only SFNS.ttf fixture")
}
fn families(definitions: &FontDefinitions) {
    assert_eq!(definitions.font_data.len(), 1);
    assert_eq!(definitions.font_data["review-system-font"].index, 0);
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        assert_eq!(definitions.families[&family], ["review-system-font"]);
    }
}
#[test]
fn production_discovery_reads_only_fixed_lead_approved_system_font() {
    assert_eq!(
        SystemFont::Sfns.path(),
        Path::new("/System/Library/Fonts/SFNS.ttf")
    );
    assert_eq!(
        SystemFont::Helvetica.path(),
        Path::new("/System/Library/Fonts/Helvetica.ttc")
    );
    let validated = discover_system_font().unwrap();
    assert_eq!(validated.candidate, SystemFont::Sfns);
    families(&validated.definitions);
}
#[test]
fn fixed_order_stops_on_first_parsed_font_and_falls_back_to_face_zero() {
    let first = reader(Ok(fixture()), Err(FontError::ReadFailed));
    assert_eq!(discover_with(&first).unwrap().candidate, SystemFont::Sfns);
    assert_eq!(*first.calls.lock().unwrap(), [SystemFont::Sfns]);
    for failure in [
        FontError::Missing,
        FontError::ReadFailed,
        FontError::NotRegular,
    ] {
        let second = reader(Err(failure), Ok(fixture()));
        let font = discover_with(&second).unwrap();
        assert_eq!(font.candidate, SystemFont::Helvetica);
        families(&font.definitions);
        assert_eq!(
            *second.calls.lock().unwrap(),
            [SystemFont::Sfns, SystemFont::Helvetica]
        );
    }
    families(&definitions(SystemFontReader.read(SystemFont::Helvetica).unwrap()).unwrap());
}
#[test]
fn missing_read_failed_nonregular_empty_oversize_and_invalid_are_explicit() {
    for error in [
        FontError::Missing,
        FontError::ReadFailed,
        FontError::NotRegular,
    ] {
        assert!(matches!(discover_with(&reader(Err(error), Err(error))), Err(e) if e == error));
    }
    assert!(matches!(definitions(vec![]), Err(FontError::Empty)));
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
    let invalid = reader(Ok(b"OTTO".to_vec()), Ok(Vec::new()));
    assert!(matches!(discover_with(&invalid), Err(FontError::Empty)));
    assert_eq!(
        *invalid.calls.lock().unwrap(),
        [SystemFont::Sfns, SystemFont::Helvetica]
    );
}
#[test]
fn exact_sixteen_mib_is_allowed_and_parsed_before_use() {
    let mut bytes = fixture();
    assert!(bytes.len() < MAX_FONT_BYTES);
    bytes.resize(MAX_FONT_BYTES, 0);
    families(&definitions(bytes).unwrap());
}
#[test]
fn explicit_review_fixture_retains_wp_4_3_behavior() {
    assert!(review_font(Path::new("relative.ttf")).is_err());
    let configured = std::env::var_os("CROSSPANE_TEST_FONT").map(PathBuf::from);
    let candidates = [
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        "/System/Library/Fonts/Supplemental/Verdana.ttf",
    ];
    if let Some(path) = &configured {
        assert!(
            approved_fixture(path),
            "CROSSPANE_TEST_FONT must name an approved read-only system font: {candidates:?}"
        );
    }
    let path = configured
        .or_else(|| candidates.iter().map(PathBuf::from).find(|p| p.is_file()))
        .unwrap_or_else(|| {
            panic!("set CROSSPANE_TEST_FONT; approved review candidates: {candidates:?}")
        });
    assert!(path.is_absolute());
    families(&review_font(&path).unwrap());
}

fn approved_fixture(path: &Path) -> bool {
    [
        "/System/Library/Fonts/SFNS.ttf",
        "/System/Library/Fonts/Helvetica.ttc",
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        "/System/Library/Fonts/Supplemental/Verdana.ttf",
    ]
    .iter()
    .any(|approved| path.as_os_str() == Path::new(approved).as_os_str())
}
#[test]
fn review_override_refuses_owner_relative_alias_and_traversal_before_open() {
    for path in [
        "/Users/owner/Library/Keychains/login.keychain-db",
        "relative.ttf",
        "/tmp/font.ttf",
        "/System/Library/Fonts/../Fonts/SFNS.ttf",
        "/System/Library/Fonts//SFNS.ttf",
    ] {
        assert!(!approved_fixture(Path::new(path)));
    }
    assert!(approved_fixture(Path::new(
        "/System/Library/Fonts/SFNS.ttf"
    )));
    assert!(approved_fixture(Path::new(
        "/System/Library/Fonts/Supplemental/Arial.ttf"
    )));
}
struct Files(std::cell::RefCell<Option<Box<dyn FontFile>>>);
impl FontFiles for Files {
    fn open(&self, candidate: SystemFont) -> Result<Box<dyn FontFile>, FontError> {
        assert_eq!(candidate, SystemFont::Sfns);
        Ok(self.0.borrow_mut().take().unwrap())
    }
}
struct ScriptedFile {
    remaining: usize,
    read: std::rc::Rc<std::cell::Cell<usize>>,
    before: FontSnapshot,
    changed: bool,
    read_error: bool,
    snapshot_error: bool,
    snapshots: std::cell::Cell<usize>,
}
impl std::io::Read for ScriptedFile {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        if self.read_error {
            return Err(std::io::ErrorKind::PermissionDenied.into());
        }
        let n = self.remaining.min(bytes.len());
        bytes[..n].fill(0);
        self.remaining -= n;
        self.read.set(self.read.get() + n);
        Ok(n)
    }
}
impl FontFile for ScriptedFile {
    fn snapshot(&self) -> Result<FontSnapshot, FontError> {
        let n = self.snapshots.get();
        self.snapshots.set(n + 1);
        if self.snapshot_error && n != 0 {
            return Err(FontError::ReadFailed);
        }
        let mut snapshot = self.before.clone();
        if self.changed && n != 0 {
            snapshot.identity.1 += 1;
        }
        Ok(snapshot)
    }
}
#[test]
fn actual_fixed_reader_bounds_growth_and_rejects_nonregular_read_and_metadata_failures() {
    for fault in [
        "nonregular",
        "oversize",
        "growth",
        "read",
        "metadata",
        "changed",
    ] {
        let count = std::rc::Rc::new(std::cell::Cell::new(0));
        let file = ScriptedFile {
            remaining: if fault == "growth" {
                MAX_FONT_BYTES + 100
            } else {
                1
            },
            read: count.clone(),
            before: FontSnapshot {
                regular: fault != "nonregular",
                length: if fault == "oversize" {
                    MAX_FONT_BYTES as u64 + 1
                } else {
                    1
                },
                identity: (1, 2, 3, 4, 5, 6),
            },
            changed: fault == "changed",
            read_error: fault == "read",
            snapshot_error: fault == "metadata",
            snapshots: std::cell::Cell::new(0),
        };
        let files = Files(std::cell::RefCell::new(Some(Box::new(file))));
        let expected = match fault {
            "nonregular" => FontError::NotRegular,
            "oversize" | "growth" => FontError::Oversize,
            _ => FontError::ReadFailed,
        };
        assert_eq!(read_fixed(SystemFont::Sfns, &files), Err(expected));
        assert!(count.get() <= MAX_FONT_BYTES + 1);
        if ["nonregular", "oversize"].contains(&fault) {
            assert_eq!(count.get(), 0);
        }
        if fault == "growth" {
            assert_eq!(count.get(), MAX_FONT_BYTES + 1);
        }
    }
}
#[test]
fn actual_font_opener_refuses_scratch_symlink_missing_directory_and_oversize() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let root = PathBuf::from(format!("/private/tmp/cp-font-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(
        open_font(&root.join("missing.ttf")),
        Err(FontError::Missing)
    ));
    symlink(root.join("missing.ttf"), root.join("link.ttf")).unwrap();
    assert!(matches!(
        open_font(&root.join("link.ttf")),
        Err(FontError::ReadFailed)
    ));
    let files = Files(std::cell::RefCell::new(Some(Box::new(
        open_font(&root).unwrap(),
    ))));
    assert_eq!(
        read_fixed(SystemFont::Sfns, &files),
        Err(FontError::NotRegular)
    );
    let path = root.join("oversize.ttf");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(MAX_FONT_BYTES as u64 + 1).unwrap();
    let files = Files(std::cell::RefCell::new(Some(Box::new(
        open_font(&path).unwrap(),
    ))));
    assert_eq!(
        read_fixed(SystemFont::Sfns, &files),
        Err(FontError::Oversize)
    );
    std::fs::remove_dir_all(&root).unwrap();
}
