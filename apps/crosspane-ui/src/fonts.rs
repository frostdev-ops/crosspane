//! System font selection; no bundled/default egui fonts.

use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use eframe::egui::{FontData, FontDefinitions, FontFamily};

#[derive(Clone, Copy, Debug)]
#[cfg(any(unix, test))]
enum FontOs {
    Linux,
    Mac,
}

#[cfg(any(unix, test))]
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

#[cfg(unix)]
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
    #[cfg(unix)]
    let os = if cfg!(target_os = "macos") {
        FontOs::Mac
    } else {
        FontOs::Linux
    };
    #[cfg(unix)]
    let matched = match os {
        FontOs::Linux => fc_match(),
        FontOs::Mac => None,
    };
    #[cfg(unix)]
    let paths = candidates(os, matched, Path::is_file);
    #[cfg(windows)]
    let paths = {
        let root = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or_else(|| anyhow::anyhow!("SystemRoot is not an absolute directory"))?;
        windows_candidates(&root, Path::is_file)
    };
    for path in paths {
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

#[cfg(any(windows, test))]
fn windows_candidates(root: &Path, exists: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    ["segoeui.ttf", "tahoma.ttf"]
        .into_iter()
        .map(|name| root.join("Fonts").join(name))
        .filter(|path| exists(path))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_fonts_use_installed_segoe_then_tahoma() {
        let root = Path::new("owned-system-root");
        let paths = windows_candidates(root, |_| true);
        assert_eq!(
            paths,
            [
                root.join("Fonts/segoeui.ttf"),
                root.join("Fonts/tahoma.ttf")
            ]
        );
        assert_eq!(
            windows_candidates(root, |path| path.ends_with("tahoma.ttf")),
            [root.join("Fonts/tahoma.ttf")]
        );
        assert!(windows_candidates(root, |_| false).is_empty());
    }

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
