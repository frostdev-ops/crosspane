//! System font selection; no bundled/default egui fonts.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use eframe::egui::{FontData, FontDefinitions, FontFamily};

#[derive(Clone, Copy, Debug)]
enum FontOs {
    Linux,
    Mac,
}

fn candidates(
    os: FontOs,
    matched: Option<PathBuf>,
    exists: impl Fn(&Path) -> bool,
) -> Vec<PathBuf> {
    let paths = match os {
        FontOs::Linux => matched
            .into_iter()
            .chain([
                PathBuf::from("/usr/share/fonts/liberation/LiberationSans-Regular.ttf"),
                PathBuf::from("/usr/share/fonts/TTF/DejaVuSans.ttf"),
                PathBuf::from("/usr/share/fonts/noto/NotoSans-Regular.ttf"),
            ])
            .collect::<Vec<_>>(),
        FontOs::Mac => vec![
            PathBuf::from("/System/Library/Fonts/SFNS.ttf"),
            PathBuf::from("/System/Library/Fonts/Helvetica.ttc"),
        ],
    };
    paths.into_iter().filter(|path| exists(path)).collect()
}

fn fc_match() -> Option<PathBuf> {
    let mut child = Command::new("fc-match")
        .args(["-f", "%{file}", "sans-serif"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                let output = child.wait_with_output().ok()?;
                let text = String::from_utf8(output.stdout).ok()?;
                let path = text.trim();
                return (!path.is_empty()).then(|| PathBuf::from(path));
            }
            Ok(Some(_)) => return None,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                // Only the font probe we spawned; never a desktop or compositor process.
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

pub fn load() -> Result<FontDefinitions> {
    let os = if cfg!(target_os = "macos") {
        FontOs::Mac
    } else {
        FontOs::Linux
    };
    let matched = match os {
        FontOs::Linux => fc_match(),
        FontOs::Mac => None,
    };
    for path in candidates(os, matched, Path::is_file) {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        if bytes.is_empty() {
            continue;
        }
        let mut fonts = FontDefinitions::empty();
        // FontData::from_owned selects face 0, including Helvetica.ttc.
        fonts
            .font_data
            .insert("system".into(), FontData::from_owned(bytes).into());
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            fonts.families.insert(family, vec!["system".into()]);
        }
        return Ok(fonts);
    }
    bail!("could not load a system font for Crosspane Settings")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_candidate_order_with_injected_exists() {
        let paths = candidates(FontOs::Linux, Some("/matched.ttf".into()), |_| true);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/matched.ttf"),
                PathBuf::from("/usr/share/fonts/liberation/LiberationSans-Regular.ttf"),
                PathBuf::from("/usr/share/fonts/TTF/DejaVuSans.ttf"),
                PathBuf::from("/usr/share/fonts/noto/NotoSans-Regular.ttf")
            ]
        );
        let paths = candidates(FontOs::Linux, Some("/missing.ttf".into()), |path| {
            path == Path::new("/usr/share/fonts/TTF/DejaVuSans.ttf")
        });
        assert_eq!(
            paths,
            vec![PathBuf::from("/usr/share/fonts/TTF/DejaVuSans.ttf")]
        );
        assert!(candidates(FontOs::Linux, None, |_| false).is_empty());
    }

    #[test]
    fn mac_candidate_order_with_injected_exists() {
        let paths = candidates(FontOs::Mac, Some("/ignored.ttf".into()), |_| true);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/System/Library/Fonts/SFNS.ttf"),
                PathBuf::from("/System/Library/Fonts/Helvetica.ttc")
            ]
        );
        let paths = candidates(FontOs::Mac, None, |path| {
            path.extension().is_some_and(|ext| ext == "ttc")
        });
        assert_eq!(
            paths,
            vec![PathBuf::from("/System/Library/Fonts/Helvetica.ttc")]
        );
        assert!(candidates(FontOs::Mac, None, |_| false).is_empty());
    }
}
