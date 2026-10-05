//! Deliberate deferrals. Neither the file nor a skipped row supplies proof.

#[cfg(any(test, target_os = "linux", target_os = "macos"))]
use std::collections::BTreeSet;
#[cfg(any(test, target_os = "linux", target_os = "macos"))]
use std::io::{Read, Write};
#[cfg(any(test, target_os = "linux", target_os = "macos"))]
use std::path::PathBuf;
#[cfg(any(test, target_os = "linux", target_os = "macos"))]
use std::sync::atomic::{AtomicU64, Ordering};

use crosspane_installer_core::{FlowEvent, StepId, StepState};

use super::controller::{LiveController, OwnCall};
use super::graph::steps;
use crate::gui::ShellEffect;
use crate::view::ScreenId;
use crate::view::{RowState, RowView};

pub(super) const LATER: &str = "Do this later from the Crosspane menu > Settings";

pub(super) fn optional(id: StepId) -> bool {
    matches!(id.0, 60..=62)
}

/// A bounded preference file at a selected installer's own path. Tests pass only private roots.
#[cfg(any(test, target_os = "linux", target_os = "macos"))]
pub(crate) struct SkippedStore(pub PathBuf);

#[cfg(any(test, target_os = "linux", target_os = "macos"))]
impl SkippedStore {
    pub fn load(&self) -> BTreeSet<StepId> {
        let read = || -> Option<BTreeSet<StepId>> {
            if !std::fs::symlink_metadata(&self.0).ok()?.is_file() {
                return None;
            }
            let mut bytes = Vec::new();
            std::fs::File::open(&self.0)
                .ok()?
                .take(4097)
                .read_to_end(&mut bytes)
                .ok()?;
            if bytes.len() > 4096 {
                return None;
            }
            let ids: Vec<StepId> = serde_json::from_slice(&bytes).ok()?;
            if ids.len() > 12 || ids.iter().any(|id| !matches!(id.0, 60..=62 | 70..=78)) {
                return None;
            }
            // Old Practice preferences are accepted as data, then ignored.
            Some(ids.into_iter().filter(|id| optional(*id)).collect())
        };
        read().unwrap_or_default()
    }

    pub fn save(&self, steps: &BTreeSet<StepId>) {
        let write = || -> std::io::Result<()> {
            if steps.is_empty() {
                return match std::fs::remove_file(&self.0) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    result => result,
                };
            }
            if steps.iter().any(|id| !optional(*id)) {
                return Ok(());
            }
            let Some(parent) = self.0.parent() else {
                return Ok(());
            };
            std::fs::create_dir_all(parent)?;
            static NEXT: AtomicU64 = AtomicU64::new(1);
            let temp = parent.join(format!(
                ".skipped-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temp)?;
            let mut publish = || -> std::io::Result<()> {
                file.write_all(&serde_json::to_vec(steps)?)?;
                file.sync_all()?;
                std::fs::rename(&temp, &self.0)
            };
            let result = publish();
            if result.is_err() {
                let _ = std::fs::remove_file(&temp);
            }
            result
        };
        // Preference I/O never grants authority or changes installation readiness.
        let _ = write();
    }
}

impl LiveController {
    pub(super) fn summary_rows(&self) -> Vec<RowView> {
        let mut rows: Vec<_> = self
            .rows()
            .into_iter()
            .filter(|r| r.state != RowState::Skipped)
            .collect();
        for (id, screen, label) in [
            (940, ScreenId::Connect, "Connect another computer"),
            (941, ScreenId::Layout, "Arrange your screens"),
        ] {
            if self.skipped_on(screen)
                || (screen == ScreenId::Layout && self.skipped_on(ScreenId::Grants))
            {
                rows.push(RowView {
                    id,
                    label: label.into(),
                    detail: LATER.into(),
                    state: RowState::Skipped,
                    human_confirmed: false,
                });
            }
        }
        rows
    }
    pub(super) fn settled(&self, id: StepId) -> bool {
        matches!(
            self.step_state(id),
            StepState::Satisfied | StepState::Skipped
        )
    }

    pub(super) fn skipped_on(&self, screen: ScreenId) -> bool {
        self.graph
            .on_screen(screen)
            .any(|m| self.step_state(m.id) == StepState::Skipped)
    }

    pub(super) fn skip_current(&mut self) {
        let screens: &[ScreenId] = match self.screen {
            ScreenId::Connect => &[ScreenId::Connect, ScreenId::Grants, ScreenId::Layout],
            ScreenId::Layout => &[ScreenId::Layout],
            _ => return,
        };
        let ids: Vec<_> = self
            .graph
            .metas
            .iter()
            .filter(|m| screens.contains(&m.screen))
            .map(|m| m.id)
            .collect();
        self.reopened.retain(|screen| !screens.contains(screen));
        if self.screen == ScreenId::Connect {
            self.own_calls.retain(|_, kind| {
                !matches!(
                    kind,
                    OwnCall::PairScan
                        | OwnCall::PairListen
                        | OwnCall::PairJoin
                        | OwnCall::PairStatus
                        | OwnCall::PairConfirm
                        | OwnCall::PairPick
                        | OwnCall::Dial
                )
            });
            self.connect.leave();
            self.connect.polling = false;
            self.connect.pairing = None;
            self.connect.mode = None;
            self.connect.candidates.clear();
            self.peer = None;
        }
        if self.screen == ScreenId::Layout {
            self.effects.push(ShellEffect::CancelLayoutDrag);
            self.own_calls.retain(|_, kind| *kind != OwnCall::Place);
            self.connect.layout_busy = false;
            self.connect.place = None;
            self.connect.placed = None;
        }
        self.pending_skip = Some(ids);
        self.finish_skip();
    }

    pub(super) fn finish_skip(&mut self) {
        if self.pending_skip.is_none() {
            return;
        }
        let Some(ids) = self.pending_skip.take() else {
            return;
        };
        for id in ids {
            if self.reduce(FlowEvent::Skip(id)).is_ok() {
                self.saved_skipped.insert(id);
            }
        }
        self.platform.save_skipped(&self.saved_skipped);
        self.go(if self.screen == ScreenId::Connect {
            ScreenId::Summary
        } else {
            self.next_screen().unwrap_or(ScreenId::Summary)
        });
    }

    pub(super) fn reopen(&mut self, mut screen: ScreenId) {
        if screen != ScreenId::Connect && self.step_state(steps::PAIR) == StepState::Skipped {
            screen = ScreenId::Connect;
        }
        self.go(screen);
        self.auto_advance = false;
        if !self.reopened.contains(&screen) {
            self.reopened.push(screen);
        }
        let ids: Vec<_> = self.graph.on_screen(screen).map(|m| m.id).collect();
        for id in ids {
            if self.step_state(id) == StepState::Skipped {
                self.begin(id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_practice_skips_load_without_erasing_connect_and_arrange() {
        struct Root(PathBuf);
        impl Drop for Root {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let path = std::env::temp_dir().join(format!(
            "crosspane-wp436-skips-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        let root = Root(path);
        let store = SkippedStore(root.0.join("skipped.json"));
        std::fs::write(&store.0, b"[60,61,62,70,71,72,73,74,75,76,77,78]").unwrap();
        assert_eq!(
            store.load(),
            BTreeSet::from([StepId(60), StepId(61), StepId(62)])
        );
        std::fs::write(&store.0, b"[70,78]").unwrap();
        assert!(store.load().is_empty());
    }

    #[test]
    fn skipped_preferences_are_bounded_and_preserve_remaining_members() {
        struct Root(PathBuf);
        impl Drop for Root {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let path = std::env::temp_dir().join(format!(
            "crosspane-skipped-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        let root = Root(path);
        let store = SkippedStore(root.0.join("installer/skipped.json"));
        assert!(store.load().is_empty());
        store.save(&BTreeSet::from([StepId(60), StepId(62)]));
        assert_eq!(store.load(), BTreeSet::from([StepId(60), StepId(62)]));
        store.save(&BTreeSet::from([StepId(62)]));
        assert_eq!(store.load(), BTreeSet::from([StepId(62)]));
        for bytes in [b"not json".as_slice(), b"[10]", &vec![b' '; 4097]] {
            std::fs::write(&store.0, bytes).unwrap();
            assert!(store.load().is_empty());
        }
        store.save(&BTreeSet::new());
        assert!(!store.0.exists());
        #[cfg(unix)]
        {
            let other = root.0.join("other");
            std::fs::write(&other, "[60]").unwrap();
            std::os::unix::fs::symlink(&other, &store.0).unwrap();
            assert!(store.load().is_empty());
            store.save(&BTreeSet::new());
            assert_eq!(std::fs::read_to_string(other).unwrap(), "[60]");
        }
    }
}
