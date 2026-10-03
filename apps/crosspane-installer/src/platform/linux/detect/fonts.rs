//! Bounded system-font selection; no bundled fonts, viewport, or explicit-review override.
use crate::agent_contract::ObservationSource;
use crate::platform::linux::native_io::{
    ChildEnvironment, CommandSpec, Deadline, LinuxNativeIo, MAX_FONT_BYTES, NativeError, SystemRead,
};
use eframe::egui;
use std::path::{Component, Path, PathBuf};

pub const CANDIDATES: [&str; 3] = [
    "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
];
pub const MAX_MATCH_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FontError {
    #[error("system font observation failed: {0}")]
    Native(#[from] NativeError),
    #[error("system font is empty")]
    Empty,
    #[error("system font exceeds 16 MiB")]
    Oversize,
    #[error("system font could not be parsed")]
    Invalid,
}
pub trait FontReader {
    fn source(&self) -> ObservationSource;
    fn fc_match(&self, deadline: &Deadline) -> Result<Vec<u8>, NativeError>;
    fn read(&self, path: &Path, deadline: &Deadline) -> Result<Vec<u8>, NativeError>;
}
pub struct ValidatedFont {
    pub path: PathBuf,
    pub source: ObservationSource,
    pub definitions: egui::FontDefinitions,
}
impl std::fmt::Debug for ValidatedFont {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ValidatedFont")
            .field("path", &self.path)
            .field("source", &self.source)
            .finish()
    }
}
struct NativeFonts<'a> {
    io: &'a LinuxNativeIo,
    environment: &'a ChildEnvironment,
}
impl FontReader for NativeFonts<'_> {
    fn source(&self) -> ObservationSource {
        self.io.target().source()
    }
    fn fc_match(&self, deadline: &Deadline) -> Result<Vec<u8>, NativeError> {
        let command = CommandSpec::new(
            "/usr/bin/fc-match".into(),
            ["-f", "%{file}", "sans-serif"].map(String::from).into(),
            self.environment.clone(),
            MAX_MATCH_BYTES,
        )?;
        let output = self.io.run(&command, deadline)?;
        if output.code != Some(0) || !output.stderr.is_empty() {
            return Err(NativeError::Unavailable);
        }
        Ok(output.stdout)
    }
    fn read(&self, path: &Path, deadline: &Deadline) -> Result<Vec<u8>, NativeError> {
        Ok(self
            .io
            .read_system(SystemRead::Font(path.into()), deadline)?
            .bytes)
    }
}
fn admitted(path: &Path) -> bool {
    path.starts_with("/usr/share/fonts")
        && path != Path::new("/usr/share/fonts")
        && path.to_str().is_some_and(|s| {
            s.len() <= MAX_MATCH_BYTES
                && !s.chars().any(char::is_control)
                && s.split('/')
                    .skip(1)
                    .all(|c| !c.is_empty() && c != "." && c != "..")
        })
        && path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
}
pub fn matched_path(bytes: &[u8]) -> Result<PathBuf, NativeError> {
    if bytes.len() > MAX_MATCH_BYTES {
        return Err(NativeError::Oversize);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| NativeError::Invalid)?;
    let path = PathBuf::from(text.strip_suffix('\n').unwrap_or(text));
    if !admitted(&path) {
        return Err(NativeError::Foreign);
    }
    Ok(path)
}
pub fn definitions(bytes: Vec<u8>) -> Result<egui::FontDefinitions, FontError> {
    if bytes.is_empty() {
        return Err(FontError::Empty);
    }
    if bytes.len() > MAX_FONT_BYTES {
        return Err(FontError::Oversize);
    }
    // Existing headless epaint parser, both families, face zero; no native lookup or default font.
    crate::gui::font_definitions(bytes).map_err(|_| FontError::Invalid)
}
pub fn discover(
    io: &LinuxNativeIo,
    environment: &ChildEnvironment,
    deadline: &Deadline,
) -> Result<ValidatedFont, FontError> {
    discover_with(&NativeFonts { io, environment }, deadline)
}
pub fn discover_with(
    reader: &dyn FontReader,
    deadline: &Deadline,
) -> Result<ValidatedFont, FontError> {
    deadline.check()?;
    let mut paths = Vec::new();
    if let Ok(path) = reader
        .fc_match(deadline)
        .and_then(|bytes| matched_path(&bytes))
    {
        paths.push(path);
    }
    for candidate in CANDIDATES {
        let path = PathBuf::from(candidate);
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    let mut last = FontError::Native(NativeError::Unavailable);
    for path in paths {
        deadline.check()?;
        let result = reader
            .read(&path, deadline)
            .map_err(FontError::from)
            .and_then(definitions);
        deadline.check()?;
        match result {
            Ok(definitions) => {
                return Ok(ValidatedFont {
                    path,
                    source: reader.source(),
                    definitions,
                });
            }
            Err(error) => last = error,
        }
    }
    Err(last)
}
