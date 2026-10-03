use super::*;
#[derive(Clone, Debug, PartialEq)]
pub struct OsFacts {
    pub family: Fact<OsFamily>,
    /// Supplying path, including a successfully read but malformed file.
    pub path: Option<PathBuf>,
}
/// Fallback follows any read failure, never a parse failure. Unavailable does not prove absence.
pub fn os_from_reader(
    mut read: impl FnMut(SystemRead, &Deadline) -> Result<SystemBytes, NativeError>,
    deadline: &Deadline,
    source: ObservationSource,
    clock: &dyn Fn() -> u64,
) -> OsFacts {
    let result = deadline
        .check()
        .and_then(|_| read(SystemRead::OsRelease, deadline));
    let result = result.or_else(|_| {
        deadline.check()?;
        read(SystemRead::OsReleaseFallback, deadline)
    });
    let observed_at_ms = clock();
    let (value, path) = match result {
        Ok(bytes) => (parse_os_release(&bytes.bytes), Some(bytes.path)),
        Err(_) => (Err(ProbeIssue::Unverified), None),
    };
    OsFacts {
        family: Fact {
            value,
            source,
            observed_at_ms,
        },
        path,
    }
}
