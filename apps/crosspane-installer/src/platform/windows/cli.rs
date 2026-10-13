//! The headless command line (L14): argument routing and the CLI runner on the shared engine.

use std::{ffi::OsString, path::PathBuf};

/// One lifecycle command. `skip_admin` sets `ElevatedPolicy::Never` so a scripted install never
/// raises UAC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Install {
        payload: Option<PathBuf>,
        complete: bool,
        skip_admin: bool,
    },
    Repair,
    Uninstall {
        remove_settings: bool,
        teardown: bool,
    },
    Status,
}

/// What the process was started to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Launch {
    Supervisor,
    Cli(Command),
    Maintenance,
    Gui,
}

/// Strict: a lifecycle flag plus any unknown flag → `Err(usage)`. `--maintenance` →
/// `Maintenance`. `--payload P` is allowed with `--install` and with no lifecycle flag (`Gui`).
pub fn parse(arguments: &[OsString]) -> Result<Launch, String> {
    let _ = arguments;
    Err("not built".to_owned())
}

/// Attaches the parent console only if stdout/stderr are not inherited; refuses an unsupported
/// machine (`support::check`); progress → stderr, result → stdout (`Status`: one JSON object);
/// 0 ok, 1 failure, 2 usage. `Install`: `ElevatedPolicy::Never` if `skip_admin`, else `IfNeeded`.
/// `Uninstall`: `teardown` → `IfNeeded`, else `Never`.
pub fn run(command: Command) -> i32 {
    let _ = command;
    1
}
