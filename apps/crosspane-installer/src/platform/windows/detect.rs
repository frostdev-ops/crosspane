use super::native_io::{NativeError, NativeResult};

#[cfg(windows)]
#[path = "detect/native.rs"]
pub(crate) mod native;

#[derive(Clone)]
pub struct FolderFacts {
    pub local: String,
    pub roaming: String,
    pub programs: String,
    pub environment_local: String,
    pub environment_roaming: String,
}
impl std::fmt::Debug for FolderFacts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FolderFacts")
    }
}
#[derive(Clone, PartialEq, Eq)]
pub struct FixedPaths {
    local: String,
    roaming: String,
    programs: String,
    install: String,
    installer: String,
}
impl std::fmt::Debug for FixedPaths {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FixedPaths")
    }
}
impl FixedPaths {
    pub fn admit(facts: FolderFacts) -> NativeResult<Self> {
        fn local_absolute(value: &str) -> bool {
            let bytes = value.as_bytes();
            bytes.len() > 3
                && bytes.len() < 32760
                && bytes[0].is_ascii_uppercase()
                && bytes[1..3] == *b":\\"
                && value[3..].split('\\').all(|c| {
                    !c.is_empty()
                        && c != "."
                        && c != ".."
                        && !c.ends_with(['.', ' '])
                        && !c.contains(['/', ':', '\0'])
                })
        }
        if !local_absolute(&facts.local)
            || !local_absolute(&facts.roaming)
            || facts.programs != format!("{}\\Programs", facts.local)
            || facts.local != facts.environment_local
            || facts.roaming != facts.environment_roaming
        {
            return Err(NativeError::Unsupported);
        }
        Ok(Self {
            install: format!("{}\\Crosspane", facts.programs),
            installer: format!("{}\\Crosspane\\Installer", facts.local),
            local: facts.local,
            roaming: facts.roaming,
            programs: facts.programs,
        })
    }
    pub fn install(&self) -> &str {
        &self.install
    }
    pub fn installer(&self) -> &str {
        &self.installer
    }
    pub fn local(&self) -> &str {
        &self.local
    }
    pub fn roaming(&self) -> &str {
        &self.roaming
    }
    pub fn programs(&self) -> &str {
        &self.programs
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fact<T> {
    Known(T),
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowsVersion {
    pub major: u32,
    pub build: u32,
    pub workstation: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostFacts {
    pub version: Fact<WindowsVersion>,
    /// IMAGE_FILE_MACHINE_* for the actual host, never the emulated process architecture.
    pub native_machine: Fact<u16>,
}
impl HostFacts {
    pub fn unknown() -> Self {
        Self {
            version: Fact::Unknown,
            native_machine: Fact::Unknown,
        }
    }
}
#[derive(Debug)]
pub struct SupportFacts {
    pub host: HostFacts,
    pub limited: Fact<bool>,
    pub folders: Fact<bool>,
    pub ntfs: Fact<bool>,
    pub authority: Fact<bool>,
}
impl SupportFacts {
    pub fn unknown() -> Self {
        Self {
            host: HostFacts::unknown(),
            limited: Fact::Unknown,
            folders: Fact::Unknown,
            ntfs: Fact::Unknown,
            authority: Fact::Unknown,
        }
    }
}
#[derive(Debug, PartialEq, Eq)]
pub enum Eligibility {
    Supported,
    Unsupported,
    Unknown,
}
pub fn classify_host(facts: &HostFacts) -> Eligibility {
    let version = match facts.version {
        Fact::Known(version) => {
            Fact::Known(version.workstation && version.major >= 10 && version.build >= 26100)
        }
        Fact::Unknown => Fact::Unknown,
    };
    let architecture = match facts.native_machine {
        Fact::Known(machine) => Fact::Known(machine == 0x8664), // IMAGE_FILE_MACHINE_AMD64
        Fact::Unknown => Fact::Unknown,
    };
    classify_values(&[&version, &architecture])
}
pub fn classify(facts: &SupportFacts) -> Eligibility {
    let host = match classify_host(&facts.host) {
        Eligibility::Supported => Fact::Known(true),
        Eligibility::Unsupported => Fact::Known(false),
        Eligibility::Unknown => Fact::Unknown,
    };
    let values = [
        &host,
        &facts.limited,
        &facts.folders,
        &facts.ntfs,
        &facts.authority,
    ];
    classify_values(&values)
}
fn classify_values(values: &[&Fact<bool>]) -> Eligibility {
    if values.iter().any(|v| matches!(v, Fact::Known(false))) {
        Eligibility::Unsupported
    } else if values.iter().all(|v| matches!(v, Fact::Known(true))) {
        Eligibility::Supported
    } else {
        Eligibility::Unknown
    }
}
pub fn absence_from_error(error: u32) -> NativeResult<bool> {
    match error {
        2 | 3 => Ok(true),
        5 => Err(NativeError::Foreign),
        32 => Err(NativeError::Busy),
        _ => Err(NativeError::Unavailable),
    }
}

/// Foundation eligibility is not payload, agent, task or GUI health verification.
pub struct SupportReport {
    pub eligibility: Eligibility,
    pub facts: SupportFacts,
    pub identity: Option<super::native_io::identity::TokenFacts>,
    pub native_issue: Option<NativeError>,
}
impl std::fmt::Debug for SupportReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupportReport")
            .field("eligibility", &self.eligibility)
            .field("host", &self.facts.host)
            .field("native_issue", &self.native_issue)
            .finish_non_exhaustive()
    }
}

#[cfg(windows)]
pub fn support_report(
    io: &super::native_io::WindowsNativeIo,
    deadline: &super::native_io::Deadline,
) -> SupportReport {
    match io.admit_support(deadline) {
        Ok(_) => SupportReport {
            eligibility: Eligibility::Supported,
            facts: SupportFacts {
                host: *io.target().host(),
                limited: Fact::Known(true),
                folders: Fact::Known(true),
                ntfs: Fact::Known(true),
                authority: Fact::Known(true),
            },
            identity: Some(io.target().identity().clone()),
            native_issue: None,
        },
        Err(error) => SupportReport {
            eligibility: if matches!(error, NativeError::Foreign | NativeError::Unsupported) {
                Eligibility::Unsupported
            } else {
                Eligibility::Unknown
            },
            facts: SupportFacts::unknown(),
            identity: None,
            native_issue: Some(error),
        },
    }
}

#[cfg(windows)]
pub fn current_report(
    clock: std::sync::Arc<dyn super::native_io::Clock>,
    deadline: &super::native_io::Deadline,
) -> SupportReport {
    use super::native_io::{
        WindowsNativeIo, identity,
        process::{CallOwner, Dispatch},
    };
    let owner = std::sync::Arc::new(CallOwner::default());
    let host = match owner.run(Dispatch::Observation, deadline, || Ok(native::observe())) {
        Ok(host) => host,
        Err(error) => {
            return SupportReport {
                eligibility: Eligibility::Unknown,
                facts: SupportFacts::unknown(),
                identity: None,
                native_issue: Some(error),
            };
        }
    };
    let eligibility = classify_host(&host);
    if eligibility != Eligibility::Supported {
        return SupportReport {
            native_issue: Some(if eligibility == Eligibility::Unsupported {
                NativeError::Unsupported
            } else {
                NativeError::Unavailable
            }),
            eligibility,
            facts: SupportFacts {
                host,
                ..SupportFacts::unknown()
            },
            identity: None,
        };
    }
    let observed = match owner.run(Dispatch::Observation, deadline, identity::native::observe) {
        Ok(identity) => identity,
        Err(error) => {
            return SupportReport {
                eligibility: Eligibility::Unknown,
                facts: SupportFacts {
                    host,
                    ..SupportFacts::unknown()
                },
                identity: None,
                native_issue: Some(error),
            };
        }
    };
    if identity::LimitedIdentity::admit(observed.clone()).is_err() {
        return SupportReport {
            eligibility: Eligibility::Unsupported,
            facts: SupportFacts {
                host,
                limited: Fact::Known(false),
                ..SupportFacts::unknown()
            },
            identity: Some(observed),
            native_issue: Some(NativeError::Unsupported),
        };
    }
    match WindowsNativeIo::current(clock, deadline) {
        Ok(io) => support_report(&io, deadline),
        Err(error) => SupportReport {
            eligibility: if matches!(error, NativeError::Foreign | NativeError::Unsupported) {
                Eligibility::Unsupported
            } else {
                Eligibility::Unknown
            },
            facts: SupportFacts {
                host,
                limited: Fact::Known(true),
                ..SupportFacts::unknown()
            },
            identity: Some(observed),
            native_issue: Some(error),
        },
    }
}
