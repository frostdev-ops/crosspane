//! Fixed system fonts, parsed headlessly before a viewport or controlled child is opened.
use eframe::egui;
use rustix::fs::{self as rfs, Mode, OFlags};
use std::{fs::File, io::Read, os::unix::fs::MetadataExt, path::Path};

pub const MAX_FONT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemFont {
    Sfns,
    Helvetica,
}
impl SystemFont {
    pub fn path(self) -> &'static Path {
        Path::new(match self {
            Self::Sfns => "/System/Library/Fonts/SFNS.ttf",
            Self::Helvetica => "/System/Library/Fonts/Helvetica.ttc",
        })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FontError {
    #[error("system font is missing")]
    Missing,
    #[error("system font could not be read")]
    ReadFailed,
    #[error("system font must be a regular file")]
    NotRegular,
    #[error("font is empty")]
    Empty,
    #[error("font exceeds 16 MiB")]
    Oversize,
    #[error("font could not be parsed")]
    Invalid,
}

/// Only fixed candidates reach the production reader; tests inject bytes, not arbitrary paths.
pub trait FontReader {
    fn read(&self, candidate: SystemFont) -> Result<Vec<u8>, FontError>;
}
#[derive(Debug)]
pub struct SystemFontReader;
impl FontReader for SystemFontReader {
    fn read(&self, candidate: SystemFont) -> Result<Vec<u8>, FontError> {
        read_fixed(candidate, &FixedFiles)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FontSnapshot {
    pub regular: bool,
    pub length: u64,
    pub identity: (u64, u64, i64, i64, i64, i64),
}
pub(crate) trait FontFile: Read {
    fn snapshot(&self) -> Result<FontSnapshot, FontError>;
}
pub(crate) trait FontFiles {
    fn open(&self, candidate: SystemFont) -> Result<Box<dyn FontFile>, FontError>;
}
struct FixedFiles;
impl FontFiles for FixedFiles {
    fn open(&self, candidate: SystemFont) -> Result<Box<dyn FontFile>, FontError> {
        Ok(Box::new(open_font(candidate.path())?))
    }
}
pub(crate) fn open_font(path: &Path) -> Result<File, FontError> {
    let fd = rfs::open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|e| {
        if e == rustix::io::Errno::NOENT {
            FontError::Missing
        } else {
            FontError::ReadFailed
        }
    })?;
    Ok(File::from(fd))
}
impl FontFile for File {
    fn snapshot(&self) -> Result<FontSnapshot, FontError> {
        let m = self.metadata().map_err(|_| FontError::ReadFailed)?;
        Ok(FontSnapshot {
            regular: m.is_file(),
            length: m.len(),
            identity: (
                m.dev(),
                m.ino(),
                m.mtime(),
                m.mtime_nsec(),
                m.ctime(),
                m.ctime_nsec(),
            ),
        })
    }
}
pub(crate) fn read_fixed(
    candidate: SystemFont,
    files: &dyn FontFiles,
) -> Result<Vec<u8>, FontError> {
    let mut file = files.open(candidate)?;
    let before = file.snapshot()?;
    if !before.regular {
        return Err(FontError::NotRegular);
    }
    if before.length > MAX_FONT_BYTES as u64 {
        return Err(FontError::Oversize);
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_FONT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| FontError::ReadFailed)?;
    if bytes.len() > MAX_FONT_BYTES {
        return Err(FontError::Oversize);
    }
    let after = file.snapshot()?;
    if before != after {
        return Err(FontError::ReadFailed);
    }
    Ok(bytes)
}
pub struct ValidatedFont {
    pub candidate: SystemFont,
    pub definitions: egui::FontDefinitions,
}
impl std::fmt::Debug for ValidatedFont {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ValidatedFont")
            .field("candidate", &self.candidate)
            .finish()
    }
}
pub fn discover_system_font() -> Result<ValidatedFont, FontError> {
    discover_with(&SystemFontReader)
}
pub fn discover_with(reader: &dyn FontReader) -> Result<ValidatedFont, FontError> {
    let mut last = FontError::Missing;
    for candidate in [SystemFont::Sfns, SystemFont::Helvetica] {
        match reader.read(candidate).and_then(definitions) {
            Ok(definitions) => {
                return Ok(ValidatedFont {
                    candidate,
                    definitions,
                });
            }
            Err(error) => last = error,
        }
    }
    Err(last)
}
/// Face zero supplies both families. No bundled/default font is introduced.
pub fn definitions(bytes: Vec<u8>) -> Result<egui::FontDefinitions, FontError> {
    if bytes.is_empty() {
        return Err(FontError::Empty);
    }
    if bytes.len() > MAX_FONT_BYTES {
        return Err(FontError::Oversize);
    }
    crate::gui::font_definitions(bytes).map_err(|_| FontError::Invalid)
}
/// Explicit review input retains WP-4.3 validation; production discovery cannot use this path.
pub fn review_font(path: &Path) -> Result<egui::FontDefinitions, FontError> {
    crate::gui::load_review_font(path).map_err(|_| FontError::Invalid)
}
