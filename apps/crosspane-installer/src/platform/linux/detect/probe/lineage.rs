//! /proc evidence for Hyprland ≥ 0.56 under uwsm: the service's MainPID is the
//! `start-hyprland` watchdog launcher and the compositor (the IPC/Wayland peer) is its direct
//! child. Read only for the selected native target; never a grandchild, name or cmdline match.
use super::*;
use std::{
    io::Read,
    path::{Path, PathBuf},
};

/// The only launcher accepted between uwsm's MainPID and the compositor.
pub const START_HYPRLAND: &str = "/usr/bin/start-hyprland";
const MAX_STAT_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessStat {
    pub ppid: u32,
    /// Field 22, start time in clock ticks since boot; detects PID reuse between reads.
    pub start_ticks: u64,
}
/// One consistent observation of the launcher and the compositor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompositorLineage {
    pub launcher_pid: u32,
    pub launcher_executable: PathBuf,
    pub compositor_pid: u32,
    pub compositor_parent: u32,
}
/// Read seam over `/proc/<pid>/stat` and `/proc/<pid>/exe`.
pub trait ProcessReader {
    fn stat(&self, pid: u32) -> Result<Vec<u8>, ProbeIssue>;
    fn executable(&self, pid: u32) -> Result<PathBuf, ProbeIssue>;
}
/// `pid (comm) state ppid … starttime …` (proc(5)); comm may hold spaces and parentheses.
pub fn parse_stat(bytes: &[u8], pid: u32) -> Result<ProcessStat, ProbeIssue> {
    if bytes.len() > MAX_STAT_BYTES {
        return Err(ProbeIssue::Oversize);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| ProbeIssue::Malformed)?;
    let (head, _) = text.split_once(" (").ok_or(ProbeIssue::Malformed)?;
    let (_, tail) = text.rsplit_once(") ").ok_or(ProbeIssue::Malformed)?;
    if head.parse::<u32>().ok() != Some(pid) {
        return Err(ProbeIssue::Foreign);
    }
    let fields: Vec<_> = tail.split_ascii_whitespace().collect();
    // tail[0] is field 3 (state): ppid is field 4, starttime field 22.
    let number = |index: usize| {
        fields
            .get(index)
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or(ProbeIssue::Malformed)
    };
    Ok(ProcessStat {
        ppid: u32::try_from(number(1)?).map_err(|_| ProbeIssue::Malformed)?,
        start_ticks: number(19)?,
    })
}
/// Compositor stat, launcher exe and stat, then the compositor stat again; any change or a
/// launcher that started after the compositor (a reused PID) is Foreign.
pub fn read_lineage(
    reader: &dyn ProcessReader,
    launcher: u32,
    compositor: u32,
) -> Result<CompositorLineage, ProbeIssue> {
    if launcher == 0 || compositor == 0 || launcher == compositor {
        return Err(ProbeIssue::Malformed);
    }
    let child = parse_stat(&reader.stat(compositor)?, compositor)?;
    let launcher_executable = reader.executable(launcher)?;
    let parent = parse_stat(&reader.stat(launcher)?, launcher)?;
    let again = parse_stat(&reader.stat(compositor)?, compositor)?;
    if again != child || parent.start_ticks > child.start_ticks {
        return Err(ProbeIssue::Foreign);
    }
    Ok(CompositorLineage {
        launcher_pid: launcher,
        launcher_executable,
        compositor_pid: compositor,
        compositor_parent: child.ppid,
    })
}
/// The compositor is the uwsm main process itself, or the direct child of a `start-hyprland`
/// main process. Nothing looser.
pub fn compositor_matches(
    main_pid: u32,
    compositor_pid: u32,
    lineage: &Result<CompositorLineage, ProbeIssue>,
) -> Result<(), ProbeIssue> {
    if main_pid == compositor_pid {
        return Ok(());
    }
    let lineage = lineage.as_ref().map_err(|error| *error)?;
    if lineage.launcher_pid == main_pid
        && lineage.compositor_pid == compositor_pid
        && lineage.compositor_parent == main_pid
        && lineage.launcher_executable == Path::new(START_HYPRLAND)
    {
        Ok(())
    } else {
        Err(ProbeIssue::Foreign)
    }
}
pub(crate) struct NativeProcesses;
impl ProcessReader for NativeProcesses {
    fn stat(&self, pid: u32) -> Result<Vec<u8>, ProbeIssue> {
        let file = std::fs::File::open(format!("/proc/{pid}/stat"))
            .map_err(|_| ProbeIssue::Unavailable)?;
        let mut bytes = Vec::new();
        file.take(MAX_STAT_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ProbeIssue::Unavailable)?;
        Ok(bytes)
    }
    fn executable(&self, pid: u32) -> Result<PathBuf, ProbeIssue> {
        std::fs::read_link(format!("/proc/{pid}/exe")).map_err(|_| ProbeIssue::Unavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    struct Fake {
        stats: BTreeMap<u32, Vec<Vec<u8>>>,
        exes: BTreeMap<u32, PathBuf>,
        reads: std::cell::Cell<usize>,
    }
    impl ProcessReader for Fake {
        fn stat(&self, pid: u32) -> Result<Vec<u8>, ProbeIssue> {
            let reads = self.reads.get();
            self.reads.set(reads + 1);
            let all = self.stats.get(&pid).ok_or(ProbeIssue::Unavailable)?;
            Ok(all[reads.min(all.len() - 1)].clone())
        }
        fn executable(&self, pid: u32) -> Result<PathBuf, ProbeIssue> {
            self.exes.get(&pid).cloned().ok_or(ProbeIssue::Unavailable)
        }
    }
    // Real shape from the owner's desktop: Hyprland 1848, child of start-hyprland 1843.
    fn stat(pid: u32, comm: &str, ppid: u32, start: u64) -> Vec<u8> {
        format!(
            "{pid} ({comm}) S {ppid} {ppid} {ppid} 0 -1 4194304 153354 2285 1675 0 330094 147149 1 0 20 0 8 0 {start} 1047470080 39995 18446744073709551615 1 1 0 0 0 0 0 4096 0 0 0 17 3 0 0 0 0 0\n"
        )
        .into_bytes()
    }
    fn fake(child_parent: u32, exe: &str, launcher_start: u64) -> Fake {
        Fake {
            stats: BTreeMap::from([
                (1848, vec![stat(1848, "Hyprland", child_parent, 2927)]),
                (
                    1843,
                    vec![stat(1843, "start-hyprland", 1700, launcher_start)],
                ),
                (1700, vec![stat(1700, "uwsm", 1, 2000)]),
            ]),
            exes: BTreeMap::from([(1843, PathBuf::from(exe)), (1700, PathBuf::from(exe))]),
            reads: std::cell::Cell::new(0),
        }
    }
    fn check(reader: &Fake, main: u32) -> Result<(), ProbeIssue> {
        compositor_matches(main, 1848, &read_lineage(reader, main, 1848))
    }

    #[test]
    fn direct_child_of_start_hyprland_is_the_compositor() {
        let reader = fake(1843, START_HYPRLAND, 2900);
        let lineage = read_lineage(&reader, 1843, 1848).unwrap();
        assert_eq!(lineage.compositor_parent, 1843);
        assert_eq!(check(&reader, 1843), Ok(()));
        // MainPID being the compositor itself needs no /proc evidence at all.
        assert_eq!(
            compositor_matches(1848, 1848, &Err(ProbeIssue::Unavailable)),
            Ok(())
        );
    }

    #[test]
    fn grandchildren_other_launchers_reuse_and_unreadable_proc_never_match() {
        // Grandchild: the compositor's parent is start-hyprland, but MainPID is its parent.
        assert_eq!(
            check(&fake(1843, START_HYPRLAND, 2900), 1700),
            Err(ProbeIssue::Foreign)
        );
        // Not a direct child of MainPID.
        assert_eq!(
            check(&fake(1700, START_HYPRLAND, 2900), 1843),
            Err(ProbeIssue::Foreign)
        );
        // Another executable, a deleted launcher, or a lookalike path.
        for exe in [
            "/usr/bin/uwsm",
            "/usr/bin/start-hyprland (deleted)",
            "/usr/local/bin/start-hyprland",
            "/tmp/start-hyprland",
        ] {
            assert_eq!(
                check(&fake(1843, exe, 2900), 1843),
                Err(ProbeIssue::Foreign),
                "{exe}"
            );
        }
        // A launcher PID that started after the compositor was reused.
        assert_eq!(
            check(&fake(1843, START_HYPRLAND, 3000), 1843),
            Err(ProbeIssue::Foreign)
        );
        // The compositor changed between the two reads.
        let mut changed = fake(1843, START_HYPRLAND, 2900);
        changed.stats.insert(
            1848,
            vec![
                stat(1848, "Hyprland", 1843, 2927),
                stat(1848, "Hyprland", 1, 2927),
            ],
        );
        assert_eq!(check(&changed, 1843), Err(ProbeIssue::Foreign));
        // Unreadable or malformed /proc stays pending, never a match.
        let mut gone = fake(1843, START_HYPRLAND, 2900);
        gone.exes.clear();
        assert_eq!(check(&gone, 1843), Err(ProbeIssue::Unavailable));
        let mut gone = fake(1843, START_HYPRLAND, 2900);
        gone.stats.remove(&1848);
        assert_eq!(check(&gone, 1843), Err(ProbeIssue::Unavailable));
        assert_eq!(
            read_lineage(&fake(1843, START_HYPRLAND, 2900), 1843, 1843),
            Err(ProbeIssue::Malformed)
        );
    }

    #[test]
    fn proc_stat_parsing_is_exact() {
        assert_eq!(
            parse_stat(&stat(1848, "Hyprland", 1843, 2927), 1848),
            Ok(ProcessStat {
                ppid: 1843,
                start_ticks: 2927
            })
        );
        // comm with spaces and parentheses cannot shift the fields.
        assert_eq!(
            parse_stat(&stat(9, "a) S 1 (b", 7, 11), 9),
            Ok(ProcessStat {
                ppid: 7,
                start_ticks: 11
            })
        );
        assert_eq!(
            parse_stat(&stat(9, "x", 7, 11), 10),
            Err(ProbeIssue::Foreign)
        );
        for bad in [
            b"".as_slice(),
            b"9 (x) S",
            b"9 x S 7",
            b"\xff (x) S 7",
            b"9 (x) S -1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 11",
        ] {
            assert!(parse_stat(bad, 9).is_err());
        }
        assert_eq!(parse_stat(&vec![b'9'; 4097], 9), Err(ProbeIssue::Oversize));
    }
}
