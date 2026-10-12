#![allow(dead_code, unused_imports)] // Source-included seams used independently by the three binaries.

#[path = "../src/platform/windows/detect.rs"]
mod detect;
#[path = "../src/platform/windows/native_io.rs"]
mod native_io;
#[path = "../src/platform/windows/payload.rs"]
mod payload;
#[path = "../src/platform/windows/service.rs"]
mod service;
#[cfg(windows)]
#[path = "../src/platform/windows/transport.rs"]
mod transport;

use crosspane_installer::agent_contract;
use native_io::{NativeError, files::*, identity::*};

#[test]
fn unsupported_or_unknown_host_never_reaches_foundation_or_mutation() {
    use detect::{Fact, HostFacts, WindowsVersion};
    let supported = HostFacts {
        version: Fact::Known(WindowsVersion {
            major: 10,
            build: 26100,
            workstation: true,
        }),
        native_machine: Fact::Known(0x8664),
    };
    let foundations = std::cell::Cell::new(0);
    for (host, error) in [
        (
            HostFacts {
                version: Fact::Known(WindowsVersion {
                    major: 10,
                    build: 26099,
                    workstation: true,
                }),
                ..supported
            },
            NativeError::Unsupported,
        ),
        (
            HostFacts {
                version: Fact::Known(WindowsVersion {
                    major: 10,
                    build: 26100,
                    workstation: false,
                }),
                ..supported
            },
            NativeError::Unsupported,
        ),
        (
            HostFacts {
                native_machine: Fact::Known(0xaa64),
                ..supported
            },
            NativeError::Unsupported,
        ),
        (
            HostFacts {
                version: Fact::Unknown,
                ..supported
            },
            NativeError::Unavailable,
        ),
        (
            HostFacts {
                native_machine: Fact::Unknown,
                ..supported
            },
            NativeError::Unavailable,
        ),
        (HostFacts::unknown(), NativeError::Unavailable),
    ] {
        assert_eq!(
            native_io::with_supported_host(&host, || {
                foundations.set(foundations.get() + 1);
                Ok(())
            }),
            Err(error)
        );
        assert_eq!(foundations.get(), 0);
    }
    assert_eq!(
        native_io::with_supported_host(&supported, || {
            foundations.set(foundations.get() + 1);
            Ok(())
        }),
        Ok(())
    );
    assert_eq!(foundations.get(), 1);
}

#[cfg(windows)]
#[test]
fn native_host_observation_is_read_only_and_does_not_assume_supported_ci_os() {
    let host = detect::native::observe();
    let mut admitted = false;
    let result = native_io::with_supported_host(&host, || {
        admitted = true;
        Ok(())
    });
    match detect::classify_host(&host) {
        detect::Eligibility::Supported => {
            assert_eq!(result, Ok(()));
            assert!(admitted);
        }
        detect::Eligibility::Unsupported => {
            assert_eq!(result, Err(NativeError::Unsupported));
            assert!(!admitted);
        }
        detect::Eligibility::Unknown => {
            assert_eq!(result, Err(NativeError::Unavailable));
            assert!(!admitted);
        }
    }
}

#[allow(clippy::unwrap_used)] // Statically constructed fixture SIDs; never native observations.
fn token() -> TokenFacts {
    TokenFacts {
        user: Sid::from_bytes(vec![1, 1, 0, 0, 0, 0, 0, 5, 21, 0, 0, 0]).unwrap(),
        logon: Sid::from_bytes(vec![
            1, 3, 0, 0, 0, 0, 0, 5, 5, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0,
        ])
        .unwrap(),
        session: 1,
        elevated: false,
        integrity: 0x2000,
        authentication_id: 7,
        impersonating: false,
    }
}

#[test]
fn admits_actual_limited_identity_only() {
    assert!(LimitedIdentity::admit(token()).is_ok());
    for facts in [
        TokenFacts {
            elevated: true,
            ..token()
        },
        TokenFacts {
            session: 0,
            ..token()
        },
        TokenFacts {
            integrity: 0x3000,
            ..token()
        },
        TokenFacts {
            impersonating: true,
            ..token()
        },
    ] {
        assert_eq!(
            LimitedIdentity::admit(facts).err(),
            Some(NativeError::Unsupported)
        );
    }
}

#[test]
fn sid_parser_refuses_truncated_and_non_sid_bytes() {
    assert_eq!(
        Sid::from_bytes(vec![1, 8, 0, 0, 0, 0, 0, 5]).err(),
        Some(NativeError::Invalid)
    );
    assert_eq!(
        Sid::from_bytes(vec![2, 0, 0, 0, 0, 0, 0, 5]).err(),
        Some(NativeError::Invalid)
    );
}

fn component(name: &str, private: bool) -> ComponentFacts {
    ComponentFacts {
        requested: name.into(),
        actual: name.into(),
        identity: FileIdentity {
            volume: 11,
            file: [1; 16],
        },
        directory: true,
        reparse: false,
        case_sensitive: false,
        links: 1,
        acl: AclFacts {
            owner: Principal::User,
            protected: private,
            entries: vec![Ace {
                principal: Principal::User,
                mask: u32::MAX,
                inherit_only: false,
                supported: true,
            }],
        },
    }
}

#[test]
fn specific_capability_sid_classification_excludes_other_security_namespaces() {
    fn sid(authority: u8, rids: &[u32]) -> Sid {
        let mut bytes = vec![1, rids.len() as u8, 0, 0, 0, 0, 0, authority];
        for rid in rids {
            bytes.extend_from_slice(&rid.to_le_bytes());
        }
        Sid::from_bytes(bytes).unwrap()
    }
    for rids in [
        &[3, 1][..],
        &[3, 10, 20, 30, 40, 50, 60, 70][..],
        &[3, 1024, 10, 20, 30, 40, 50, 60, 70, 80][..],
    ] {
        assert!(sid(15, rids).is_specific_capability());
    }
    for (authority, rids) in [
        (15, &[3][..]),
        (15, &[2, 1][..]),
        (15, &[2, 2][..]),
        (15, &[2, 100, 200, 300, 400, 500, 600, 700][..]),
        (5, &[32, 10, 20, 30, 40, 50, 60, 70, 80][..]),
        (5, &[3, 1][..]),
    ] {
        assert!(!sid(authority, rids).is_specific_capability());
    }
    for bytes in [
        vec![1, 2, 0, 0, 0, 0, 0, 15, 3, 0, 0, 0],
        vec![2, 2, 0, 0, 0, 0, 0, 15, 3, 0, 0, 0, 1, 0, 0, 0],
    ] {
        assert_eq!(Sid::from_bytes(bytes).err(), Some(NativeError::Invalid));
    }
}

#[test]
fn capability_ancestor_grants_never_admit_a_capability_owner_or_private_grant() {
    let mut facts = component("AppData", true);
    facts.acl.entries.push(Ace {
        principal: Principal::Capability,
        mask: 0x1f01ff,
        inherit_only: false,
        supported: true,
    });
    assert_eq!(admit_component(&facts, Admission::Ancestor), Ok(()));
    assert_eq!(
        admit_component(&facts, Admission::PrivateDirectory),
        Err(NativeError::Foreign)
    );
    assert_eq!(
        admit_component(
            &ComponentFacts {
                directory: false,
                ..facts.clone()
            },
            Admission::PrivateFile
        ),
        Err(NativeError::Foreign)
    );
    facts.acl.owner = Principal::Capability;
    assert_eq!(
        admit_component(&facts, Admission::Ancestor),
        Err(NativeError::Foreign)
    );
}

#[test]
fn capability_grants_do_not_hide_ordinary_foreign_mutation_or_unsupported_aces() {
    let mut facts = component("AppData", false);
    facts.acl.entries.push(Ace {
        principal: Principal::Capability,
        mask: 0x1f01ff,
        inherit_only: false,
        supported: true,
    });
    facts.acl.entries.push(Ace {
        principal: Principal::Foreign,
        mask: 0x1200a9,
        inherit_only: false,
        supported: true,
    });
    assert_eq!(admit_component(&facts, Admission::Ancestor), Ok(()));
    for mutation in [
        DELETE, 0x40, 0x100, 0x40000, 0x80000, 0x10000000, 0x40000000,
    ] {
        let mut unsafe_facts = facts.clone();
        unsafe_facts.acl.entries[2].mask |= mutation;
        assert_eq!(
            admit_component(&unsafe_facts, Admission::Ancestor),
            Err(NativeError::Foreign)
        );
    }
    facts.acl.entries[1].supported = false;
    assert_eq!(
        admit_component(&facts, Admission::Ancestor),
        Err(NativeError::Foreign)
    );
}

#[test]
fn private_directory_requires_user_owner_protected_acl_and_no_foreign_write() {
    let good = component("Installer", true);
    assert!(admit_component(&good, Admission::PrivateDirectory).is_ok());
    let mut bad = good.clone();
    bad.acl.owner = Principal::Foreign;
    assert_eq!(
        admit_component(&bad, Admission::PrivateDirectory),
        Err(NativeError::Foreign)
    );
    bad = good.clone();
    bad.acl.protected = false;
    assert_eq!(
        admit_component(&bad, Admission::PrivateDirectory),
        Err(NativeError::Foreign)
    );
    bad = good.clone();
    bad.acl.entries.push(Ace {
        principal: Principal::Foreign,
        mask: DELETE,
        inherit_only: false,
        supported: true,
    });
    assert_eq!(
        admit_component(&bad, Admission::PrivateDirectory),
        Err(NativeError::Foreign)
    );
}

#[test]
fn refuses_reparse_ancestor_case_alias_unknown_acl_and_case_sensitive_directory() {
    let good = component("Installer", true);
    for bad in [
        ComponentFacts {
            reparse: true,
            ..good.clone()
        },
        ComponentFacts {
            actual: "INSTALLER".into(),
            ..good.clone()
        },
        ComponentFacts {
            case_sensitive: true,
            ..good.clone()
        },
    ] {
        assert_eq!(
            admit_component(&bad, Admission::PrivateDirectory),
            Err(NativeError::Foreign)
        );
    }
    let mut bad = good;
    bad.acl.entries[0].supported = false;
    assert_eq!(
        admit_component(&bad, Admission::PrivateDirectory),
        Err(NativeError::Foreign)
    );
}

#[test]
fn ancestor_chain_rejects_volume_or_identity_replacement() {
    let old = vec![component("Users", false), component("Installer", true)];
    assert!(admit_chain(&old, 11).is_ok());
    let mut new = old.clone();
    new[1].identity.volume = 12;
    assert_eq!(admit_chain(&new, 11), Err(NativeError::Foreign));
    new = old.clone();
    new[0].identity.file = [2; 16];
    assert_eq!(same_chain(&old, &new), Err(NativeError::Foreign));
}

#[test]
fn names_never_grant_path_or_stream_or_case_alias_authority() {
    assert!(PrivateName::new("receipt.json").is_ok());
    for value in [
        "../receipt.json",
        "x/y",
        "x\\y",
        "C:x",
        "x:stream",
        "x.",
        "x ",
        "CON",
        "aux.json",
        "Receipt.json",
        "",
    ] {
        assert_eq!(
            PrivateName::new(value).err(),
            Some(NativeError::Invalid),
            "{value}"
        );
    }
}

#[test]
fn private_regular_files_refuse_hardlink_and_wrong_type() {
    let good = ComponentFacts {
        directory: false,
        ..component("receipt.json", true)
    };
    assert!(admit_component(&good, Admission::PrivateFile).is_ok());
    assert_eq!(
        admit_component(
            &ComponentFacts {
                links: 2,
                ..good.clone()
            },
            Admission::PrivateFile
        ),
        Err(NativeError::Foreign)
    );
    assert_eq!(
        admit_component(
            &ComponentFacts {
                directory: true,
                ..good
            },
            Admission::PrivateFile
        ),
        Err(NativeError::Foreign)
    );
}

#[test]
fn bounded_read_rejects_growth_instead_of_truncating() {
    assert_eq!(check_read_size(12, 12), Ok(()));
    assert_eq!(check_read_size(13, 12), Err(NativeError::Oversize));
    assert_eq!(check_read_size(0, 0), Err(NativeError::Invalid));
}

#[test]
fn existing_ancestors_allow_normal_templates_and_trusted_admin_rights() {
    let mut facts = component("Users", false);
    facts.acl.owner = Principal::Administrators;
    facts.acl.entries = vec![
        Ace {
            principal: Principal::System,
            mask: u32::MAX,
            inherit_only: false,
            supported: true,
        },
        Ace {
            principal: Principal::Administrators,
            mask: u32::MAX,
            inherit_only: false,
            supported: true,
        },
        Ace {
            principal: Principal::Foreign,
            mask: 4 | 0x120089,
            inherit_only: false,
            supported: true,
        },
        Ace {
            principal: Principal::Foreign,
            mask: u32::MAX,
            inherit_only: true,
            supported: false,
        },
    ];
    assert_eq!(admit_component(&facts, Admission::Ancestor), Ok(()));
    facts.acl.entries.push(Ace {
        principal: Principal::Foreign,
        mask: 0x40,
        inherit_only: false,
        supported: true,
    });
    assert_eq!(
        admit_component(&facts, Admission::Ancestor),
        Err(NativeError::Foreign)
    );
}

#[test]
fn private_namespace_rejects_effective_admin_ace_but_not_inherit_only_template() {
    let mut facts = component("Installer", true);
    facts.acl.entries.push(Ace {
        principal: Principal::Administrators,
        mask: u32::MAX,
        inherit_only: false,
        supported: true,
    });
    assert_eq!(
        admit_component(&facts, Admission::PrivateDirectory),
        Err(NativeError::Foreign)
    );
    facts.acl.entries.last_mut().unwrap().inherit_only = true;
    assert_eq!(admit_component(&facts, Admission::PrivateDirectory), Ok(()));
}

#[test]
fn deadline_before_dispatch_never_runs_a_native_action() {
    use native_io::process::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let cancellation = Cancellation::default();
    cancellation.cancel();
    let budget = Deadline::new(500, Arc::new(MonotonicClock::default()), cancellation).unwrap();
    let called = Arc::new(AtomicBool::new(false));
    let marker = called.clone();
    let owner = Arc::new(CallOwner::default());
    assert_eq!(
        owner.run(Dispatch::Mutation, &budget, move || {
            marker.store(true, Ordering::Release);
            Ok(())
        }),
        Err(NativeError::Cancelled)
    );
    assert!(!called.load(Ordering::Acquire));
    assert!(owner.idle());
}

#[test]
fn timed_out_mutation_keeps_owner_busy_and_never_retries_after_release() {
    use native_io::process::*;
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    };
    let owner = Arc::new(CallOwner::default());
    struct FakeClock(AtomicU64);
    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.0.load(Ordering::Acquire)
        }
    }
    let clock = Arc::new(FakeClock(AtomicU64::new(0)));
    let budget = Deadline::new(5000, clock.clone(), Cancellation::default()).unwrap();
    let (entered, observed) = mpsc::sync_channel(1);
    let (release, blocked) = mpsc::sync_channel(1);
    let running = owner.clone();
    let caller = std::thread::spawn(move || {
        running.run(Dispatch::Mutation, &budget, move || {
            entered.send(()).unwrap();
            blocked.recv().unwrap();
            Ok(())
        })
    });
    observed
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    clock.0.store(5001, Ordering::Release);
    assert_eq!(caller.join().unwrap(), Err(NativeError::OutcomeUnknown));
    assert!(!owner.idle());
    let fresh = Deadline::new(500, clock, Cancellation::default()).unwrap();
    assert_eq!(
        owner.run(Dispatch::Observation, &fresh, || Ok(())),
        Err(NativeError::Busy)
    );
    release.send(()).unwrap();
    let end = std::time::Instant::now() + std::time::Duration::from_secs(1);
    while !owner.idle() && std::time::Instant::now() < end {
        std::thread::yield_now();
    }
    assert!(owner.idle());
    assert_eq!(
        owner.run(Dispatch::Mutation, &fresh, || Ok(())),
        Err(NativeError::OutcomeUnknown)
    );
    assert_eq!(owner.run(Dispatch::Observation, &fresh, || Ok(())), Ok(()));
}

#[test]
fn reported_uncertain_native_failure_also_retires_mutation_owner() {
    use native_io::process::*;
    use std::sync::Arc;
    let owner = Arc::new(CallOwner::default());
    let budget = Deadline::new(
        5000,
        Arc::new(MonotonicClock::default()),
        Cancellation::default(),
    )
    .unwrap();
    assert_eq!(
        owner.run(Dispatch::Mutation, &budget, || Err::<(), _>(
            NativeError::OutcomeUnknown
        )),
        Err(NativeError::OutcomeUnknown)
    );
    assert_eq!(
        owner.run(Dispatch::Mutation, &budget, || Ok(())),
        Err(NativeError::OutcomeUnknown)
    );
}

#[test]
fn relative_dispatch_uses_pinned_parent_when_its_old_path_changes() {
    use native_io::files::{
        ComponentName, ComponentRequest, ObjectKind, RelativeIo, relative_component,
    };
    struct Fake {
        path_now_points_to: u64,
    }
    impl RelativeIo for Fake {
        type Parent = u64;
        type Object = u64;
        fn submit(&self, parent: Option<&u64>, _: &ComponentRequest) -> Result<u64, NativeError> {
            Ok(parent.copied().unwrap_or(self.path_now_points_to))
        }
    }
    let deadline = native_io::Deadline::new(
        5000,
        std::sync::Arc::new(native_io::MonotonicClock::default()),
        native_io::Cancellation::default(),
    )
    .unwrap();
    for create in [false, true] {
        assert_eq!(
            relative_component(
                &Fake {
                    path_now_points_to: 99
                },
                &7,
                &ComponentName::new("record.json").unwrap(),
                ObjectKind::File,
                create,
                &deadline
            )
            .unwrap(),
            Some(7)
        );
    }
}

#[test]
fn relative_dispatch_refuses_reparse_without_a_path_fallback() {
    use native_io::files::{
        ComponentName, ComponentRequest, ObjectKind, RelativeIo, relative_component,
    };
    struct Fake(std::cell::Cell<usize>);
    impl RelativeIo for Fake {
        type Parent = u64;
        type Object = u64;
        fn submit(
            &self,
            parent: Option<&u64>,
            request: &ComponentRequest,
        ) -> Result<u64, NativeError> {
            self.0.set(self.0.get() + 1);
            if parent == Some(&7) && request.dont_reparse && request.open_reparse_point {
                Err(NativeError::Foreign)
            } else {
                Ok(99)
            }
        }
    }
    let fake = Fake(std::cell::Cell::new(0));
    let deadline = native_io::Deadline::new(
        5000,
        std::sync::Arc::new(native_io::MonotonicClock::default()),
        native_io::Cancellation::default(),
    )
    .unwrap();
    assert_eq!(
        relative_component(
            &fake,
            &7,
            &ComponentName::new("Installer").unwrap(),
            ObjectKind::Directory,
            true,
            &deadline
        ),
        Err(NativeError::Foreign)
    );
    assert_eq!(fake.0.get(), 1);
}

#[test]
fn relative_names_never_encode_a_path_and_only_open_maps_missing_to_none() {
    use native_io::files::{
        ComponentName, ComponentRequest, ObjectKind, RelativeIo, relative_component,
    };
    for name in [
        "", ".", "..", "a\\b", "a/b", "a:stream", "NUL", "COM1.txt", "tail.", "tail ", "a\0b",
    ] {
        assert!(ComponentName::new(name).is_err());
    }
    assert!(ComponentName::new(&"a".repeat(256)).is_err());
    assert!(ComponentName::new("Known Folder").is_ok());
    struct Missing;
    impl RelativeIo for Missing {
        type Parent = u64;
        type Object = u64;
        fn submit(&self, _: Option<&u64>, request: &ComponentRequest) -> Result<u64, NativeError> {
            assert_eq!(request.name.as_str(), "record.json");
            assert!(request.kind == ObjectKind::File);
            Err(NativeError::Missing)
        }
    }
    let deadline = native_io::Deadline::new(
        5000,
        std::sync::Arc::new(native_io::MonotonicClock::default()),
        native_io::Cancellation::default(),
    )
    .unwrap();
    let name = ComponentName::new("record.json").unwrap();
    assert_eq!(
        relative_component(&Missing, &7, &name, ObjectKind::File, false, &deadline),
        Ok(None)
    );
    assert_eq!(
        relative_component(&Missing, &7, &name, ObjectKind::File, true, &deadline),
        Err(NativeError::Missing)
    );
}

#[test]
fn uncertain_worker_result_retires_owner_before_delivery_not_after_caller_wakes() {
    use native_io::process::{CallOwner, Dispatch};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    };
    let owner = Arc::new(CallOwner::default());
    let (entered, ready) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let wait = Mutex::new(wait);
    let first = AtomicBool::new(true);
    owner
        .before_delivery(Arc::new(move || {
            if first.swap(false, Ordering::AcqRel) {
                entered.send(()).unwrap();
                wait.lock().unwrap().recv().unwrap();
            }
        }))
        .unwrap();
    let budget = || {
        native_io::Deadline::new(
            5000,
            Arc::new(native_io::MonotonicClock::default()),
            native_io::Cancellation::default(),
        )
        .unwrap()
    };
    let worker_owner = owner.clone();
    let thread = std::thread::spawn(move || {
        worker_owner.run::<()>(
            Dispatch::Mutation,
            &native_io::Deadline::new(
                5000,
                Arc::new(native_io::MonotonicClock::default()),
                native_io::Cancellation::default(),
            )
            .unwrap(),
            || Err(NativeError::OutcomeUnknown),
        )
    });
    ready
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    assert!(owner.idle());
    let calls = Arc::new(AtomicUsize::new(0));
    let action_calls = calls.clone();
    let result = owner.run(Dispatch::Mutation, &budget(), move || {
        action_calls.fetch_add(1, Ordering::Relaxed);
        Ok(())
    });
    release.send(()).unwrap();
    assert_eq!(thread.join().unwrap(), Err(NativeError::OutcomeUnknown));
    assert_eq!(result, Err(NativeError::OutcomeUnknown));
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

#[test]
fn worker_unwind_retires_mutations_before_busy_release_and_delivery() {
    use native_io::process::{CallOwner, Dispatch};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::Duration;
    let owner = Arc::new(CallOwner::default());
    let (entered, ready) = mpsc::sync_channel(1);
    let (release, wait) = mpsc::sync_channel(1);
    let wait = Mutex::new(wait);
    owner
        .before_delivery(Arc::new(move || {
            entered.send(()).unwrap();
            wait.lock().unwrap().recv().unwrap();
        }))
        .unwrap();
    let budget = || {
        native_io::Deadline::new(
            5000,
            Arc::new(native_io::MonotonicClock::default()),
            native_io::Cancellation::default(),
        )
        .unwrap()
    };
    let worker_owner = owner.clone();
    let thread = std::thread::spawn(move || {
        worker_owner.run::<()>(
            Dispatch::Mutation,
            &native_io::Deadline::new(
                5000,
                Arc::new(native_io::MonotonicClock::default()),
                native_io::Cancellation::default(),
            )
            .unwrap(),
            || panic!("owned fake worker unwind"),
        )
    });
    if ready.recv_timeout(Duration::from_secs(2)).is_err() {
        let _ = release.send(());
        let _ = thread.join();
        panic!("worker unwind did not reach the ambiguity delivery barrier");
    }
    assert!(owner.idle());
    let result = owner.run(Dispatch::Mutation, &budget(), || {
        panic!("second mutation must never enter after worker unwind")
    });
    release.send(()).unwrap();
    assert_eq!(thread.join().unwrap(), Err(NativeError::OutcomeUnknown));
    assert_eq!(result, Err::<(), _>(NativeError::OutcomeUnknown));
}

#[cfg(windows)]
#[test]
fn history_exact_retirement_helpers_preserve_unadmitted_entries_and_pins() {
    use native_io::{Cancellation, Deadline, MonotonicClock, files::native};
    use std::{
        io::Write, os::windows::fs::OpenOptionsExt, panic::AssertUnwindSafe, sync::Arc,
        time::SystemTime,
    };
    let security = native::Security {
        user: native_io::identity::native::observe().unwrap().user,
        trusted_installer: native_io::identity::native::trusted_installer().ok(),
    };
    let budget = || {
        Deadline::new(
            20000,
            Arc::new(MonotonicClock::default()),
            Cancellation::default(),
        )
        .unwrap()
    };
    let target = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .join("target");
    let parent = native::Anchor::open(target.to_str().unwrap(), &security, false, &budget())
        .unwrap()
        .unwrap();
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root_name = PrivateName::new(&format!(
        "history-retirement-{}-{nonce}",
        std::process::id()
    ))
    .unwrap();
    let root = parent
        .create_child_directory(root_name.as_str(), &security, &budget())
        .unwrap();
    let root_identity = root.identity().unwrap();
    let history_name = PrivateName::new("first-install-history").unwrap();
    let slot_name = PrivateName::new("slot-0").unwrap();
    let history = root
        .create_child_directory(history_name.as_str(), &security, &budget())
        .unwrap();
    let history_identity = history.identity().unwrap();
    let slot = history
        .create_child_directory(slot_name.as_str(), &security, &budget())
        .unwrap();
    let slot_identity = slot.identity().unwrap();
    let mut leaves = Vec::new();
    let trial = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let mut create = |name: &str, bytes: &[u8]| {
            let mut file = slot
                .create_private(&PrivateName::new(name).unwrap(), &security, &budget())
                .unwrap();
            file.write_all(bytes).unwrap();
            file.sync_all().unwrap();
            drop(file);
            let (_, identity) = slot
                .open_file_metadata(&PrivateName::new(name).unwrap(), &security, &budget())
                .unwrap();
            leaves.push(native::CreatedLeaf {
                name: PrivateName::new(name).unwrap(),
                identity,
                directory: false,
            });
            let mut digest = [0; 32];
            digest.copy_from_slice(
                aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes).as_ref(),
            );
            (identity, bytes.len() as u32, digest)
        };
        // No installed evidence: fixture bytes are authored and remain in the exact owned tree.
        let bytes = b"authored metadata fixture";
        let (identity, length, digest) = create("first-install.json", bytes);
        let (extra_identity, extra_length, extra_digest) =
            create("repair.json", b"unplanned fixture entry");
        let leaf = HistoryMetadata {
            name: "first-install.json",
            identity,
            length,
            digest,
        };
        let mut wrong_identity = identity;
        wrong_identity.file[0] ^= 1;
        let bad_id = HistoryMetadata {
            identity: wrong_identity,
            ..leaf
        };
        let bad_hash = HistoryMetadata {
            digest: [0x42; 32],
            ..leaf
        };
        let mut wrong_directory = slot_identity;
        wrong_directory.file[0] ^= 1;
        for bad in [&bad_id, &bad_hash] {
            assert!(
                root.delete_first_history_metadata(0, slot_identity, bad, &security, &budget())
                    .is_err()
            );
            assert_eq!(
                std::fs::read(std::path::Path::new(slot.fixture_path()).join(leaf.name)).unwrap(),
                bytes
            );
        }
        assert!(
            root.delete_first_history_metadata(0, wrong_directory, &leaf, &security, &budget())
                .is_err()
        );
        let path = std::path::Path::new(slot.fixture_path()).join(leaf.name);
        let pin = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ)
            .open(&path)
            .unwrap();
        assert!(
            root.delete_first_history_metadata(0, slot_identity, &leaf, &security, &budget())
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        drop(pin);
        // Both names are freshly created inside the retained private fixture directory.
        // Remove our exact extra link before asserting, so a refusal leaves a single-link file.
        let linked = std::path::Path::new(slot.fixture_path()).join("owned-link.json");
        std::fs::hard_link(&path, &linked).unwrap();
        let link_identity = slot
            .opaque("owned-link.json", false, &security, &budget())
            .unwrap()
            .unwrap()
            .identity;
        let hardlink_refusal =
            root.delete_first_history_metadata(0, slot_identity, &leaf, &security, &budget());
        std::fs::remove_file(&linked).unwrap();
        assert_eq!(link_identity, identity);
        assert!(hardlink_refusal.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        root.delete_first_history_metadata(0, slot_identity, &leaf, &security, &budget())
            .unwrap();
        assert!(
            slot.opaque(leaf.name, false, &security, &budget())
                .unwrap()
                .is_none()
        );
        let extra = HistoryMetadata {
            name: "repair.json",
            identity: extra_identity,
            length: extra_length,
            digest: extra_digest,
        };
        // Drop the slot observation pin before DELETE; the parent/root pins remain retained.
        drop(slot);
        assert!(
            root.delete_empty_first_history_slot(0, slot_identity, &security, &budget())
                .is_err()
        );
        root.delete_first_history_metadata(0, slot_identity, &extra, &security, &budget())
            .unwrap();
        assert!(
            root.delete_empty_first_history_slot(0, wrong_directory, &security, &budget())
                .is_err()
        );
        assert_eq!(
            history
                .child("slot-0", &security, &budget())
                .unwrap()
                .unwrap()
                .identity()
                .unwrap(),
            slot_identity
        );
        root.delete_empty_first_history_slot(0, slot_identity, &security, &budget())
            .unwrap();
        assert!(
            history
                .child("slot-0", &security, &budget())
                .unwrap()
                .is_none()
        );
    }));
    // On a failing assertion, remove only exact creator-ledger leaves still in the owned slot.
    if let Some(current) = history.child("slot-0", &security, &budget()).unwrap() {
        assert_eq!(current.identity().unwrap(), slot_identity);
        let mut remaining = Vec::new();
        for leaf in leaves {
            if let Some(observed) = current
                .opaque(leaf.name.as_str(), false, &security, &budget())
                .unwrap()
            {
                assert_eq!(observed.identity, leaf.identity);
                remaining.push(leaf);
            }
        }
        drop(current);
        native::cleanup_fixture(
            &history,
            &slot_name,
            slot_identity,
            &remaining,
            &security,
            &budget(),
        )
        .unwrap();
    }
    drop(history);
    native::cleanup_fixture(
        &root,
        &history_name,
        history_identity,
        &[],
        &security,
        &budget(),
    )
    .unwrap();
    drop(root);
    native::cleanup_fixture(
        &parent,
        &root_name,
        root_identity,
        &[],
        &security,
        &budget(),
    )
    .unwrap();
    if let Err(panic) = trial {
        std::panic::resume_unwind(panic);
    }
}

#[cfg(windows)]
#[test]
fn keeper_copy_seals_readonly_reverification_and_blocks_mutation() {
    use native_io::{Cancellation, Deadline, MonotonicClock, files::native};
    use payload::inventory::{ApprovedPe, PayloadRole, PeFacts, pe_header};
    use std::{
        fs::{File, OpenOptions},
        io::Cursor,
        os::windows::{fs::OpenOptionsExt, process::CommandExt},
        panic::AssertUnwindSafe,
        process::{Child, Command, Stdio},
        sync::Arc,
        time::{Duration, Instant, SystemTime},
    };

    struct OwnChild(Child);
    impl Drop for OwnChild {
        fn drop(&mut self) {
            if !matches!(self.0.try_wait(), Ok(Some(_))) {
                // Only the exact test child returned by our --list spawn, never a PID lookup.
                if self.0.kill().is_ok() {
                    let _ = self.0.wait();
                }
            }
        }
    }

    // A fresh creator-owned fixture under the build directory, with no profile, installed
    // payload, token, or task mutation. Token observation does not require a Limited host.
    let security = native::Security {
        user: native_io::identity::native::observe().unwrap().user,
        trusted_installer: native_io::identity::native::trusted_installer().ok(),
    };
    let budget = || {
        Deadline::new(
            20000,
            Arc::new(MonotonicClock::default()),
            Cancellation::default(),
        )
        .unwrap()
    };
    let target = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .join("target");
    let parent = native::Anchor::open(target.to_str().unwrap(), &security, false, &budget())
        .unwrap()
        .unwrap();
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root_name =
        PrivateName::new(&format!("keeper-copy-{}-{nonce}", std::process::id())).unwrap();
    let root = parent
        .create_child_directory(root_name.as_str(), &security, &budget())
        .unwrap();
    let root_identity = root.identity().unwrap();
    let mut leaves = Vec::new();
    let trial = std::panic::catch_unwind(AssertUnwindSafe(|| {
        // Minimal inert PE bytes: staged and inspected, never executed.
        let mut bytes = vec![0x5a; 512];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[60..64].copy_from_slice(&64u32.to_le_bytes());
        bytes[64..68].copy_from_slice(b"PE\0\0");
        bytes[68..70].copy_from_slice(&0x8664u16.to_le_bytes());
        bytes[84..86].copy_from_slice(&240u16.to_le_bytes());
        bytes[88..90].copy_from_slice(&0x20bu16.to_le_bytes());
        bytes[156..158].copy_from_slice(&3u16.to_le_bytes());
        let (machine, subsystem) = pe_header(&bytes, bytes.len() as u64).unwrap();
        let hash = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, &bytes);
        let mut sha256 = [0; 32];
        sha256.copy_from_slice(hash.as_ref());
        let expected = ApprovedPe::fixture(
            PayloadRole::Installer,
            PeFacts {
                size: bytes.len() as u64,
                sha256,
                machine,
                subsystem,
                version: "1.0.0".into(),
            },
        );
        let leaf = "keeper-copy.exe";
        let image = root
            .stage_readonly_image(
                leaf,
                Box::new(Cursor::new(bytes)),
                &expected,
                &security,
                &budget(),
            )
            .unwrap();
        leaves.push(native::CreatedLeaf {
            name: PrivateName::new(leaf).unwrap(),
            identity: image.identity,
            directory: false,
        });
        assert_eq!(image.facts, *expected.facts());
        // OpenedPe::reverify performs this same strict second read-only open while its
        // original copy pin remains retained. The old writable stage handle rejected it.
        let reverified = root
            .open_image(leaf, true, expected.version(), &security, &budget())
            .unwrap();
        assert_eq!(reverified.identity, image.identity);
        assert_eq!(reverified.facts, *expected.facts());
        // The sealing transition must not permit tampering or replacement of either pin.
        assert!(
            root.open_staged_image(leaf, expected.version(), &security, &budget())
                .is_err()
        );
        assert!(root.opaque(leaf, true, &security, &budget()).is_err());

        // A separately approved copy of this test binary verifies actual loader acceptance.
        // --list enumerates test names only; no test or product entry is executed in the child.
        let executable = std::env::current_exe().unwrap();
        let measured = native::measure_image(
            Arc::new(File::open(&executable).unwrap()),
            "1.0.0",
            &budget(),
        )
        .unwrap();
        let own_expected = ApprovedPe::fixture(PayloadRole::Installer, measured.facts.clone());
        drop(measured);
        let runnable = root
            .stage_readonly_image(
                "keeper-list.exe",
                Box::new(File::open(executable).unwrap()),
                &own_expected,
                &security,
                &budget(),
            )
            .unwrap();
        leaves.push(native::CreatedLeaf {
            name: PrivateName::new("keeper-list.exe").unwrap(),
            identity: runnable.identity,
            directory: false,
        });
        let reverified = root
            .open_image(
                "keeper-list.exe",
                true,
                own_expected.version(),
                &security,
                &budget(),
            )
            .unwrap();
        assert_eq!(reverified.identity, runnable.identity);
        assert_eq!(reverified.facts, *own_expected.facts());
        let mut child = OwnChild(
            Command::new(&runnable.canonical)
                .arg("--list")
                .current_dir(root.fixture_path())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
                .spawn()
                .unwrap(),
        );
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                assert!(status.success(), "own --list loader child failed: {status}");
                break;
            }
            assert!(Instant::now() < until, "own --list loader child timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            root.open_staged_image(
                "keeper-list.exe",
                own_expected.version(),
                &security,
                &budget(),
            )
            .is_err()
        );
        assert!(
            root.opaque("keeper-list.exe", true, &security, &budget())
                .is_err()
        );
        for name in [leaf, "keeper-list.exe"] {
            let path = std::path::Path::new(root.fixture_path()).join(name);
            let sharing = windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ
                | windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE
                | windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
            // Request each mutation right separately, with permissive new-handle sharing.
            // Thus refusal proves the retained read pin's share mask, not a conflicting
            // share mask or the other mutation right on the newly requested handle.
            let write_error = OpenOptions::new()
                .write(true)
                .share_mode(sharing)
                .open(&path)
                .unwrap_err();
            assert_eq!(
                write_error.raw_os_error(),
                Some(windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION as i32)
            );
            let delete_error = OpenOptions::new()
                .access_mode(DELETE)
                .share_mode(sharing)
                .open(path)
                .unwrap_err();
            assert_eq!(
                delete_error.raw_os_error(),
                Some(windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION as i32)
            );
        }
    }));
    drop(root);
    let cleanup = native::cleanup_fixture(
        &parent,
        &root_name,
        root_identity,
        &leaves,
        &security,
        &budget(),
    );
    assert_eq!(cleanup, Ok(()), "creator-ledger fixture cleanup failed");
    if let Err(panic) = trial {
        std::panic::resume_unwind(panic);
    }
}

#[cfg(windows)]
#[test]
#[ignore = "Limited win-gui only: explicitly admitted own scratch root, never cargo native execution"]
fn limited_owned_scratch_foundation_probe() {
    use native_io::{
        Cancellation, Clock, Deadline, MonotonicClock,
        records::{PublicationRecovery, RecordName, encode_record},
        scratch_current,
    };
    use std::{
        sync::{Arc, mpsc},
        time::Duration,
    };
    assert_eq!(
        std::env::var("CROSSPANE_WINDOWS_INSTALLER_SCRATCH").as_deref(),
        Ok("1"),
        "native fixture opt-in absent"
    );
    // Do not even construct a scratch fixture from an elevated or impersonated process.
    native_io::identity::native::current().expect("probe requires a genuine Limited token");
    let (finished, wait) = mpsc::channel();
    let watchdog = std::thread::spawn(move || {
        if matches!(
            wait.recv_timeout(Duration::from_secs(15)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ) {
            eprintln!("UNVERIFIED scratch cleanup: independent own-process watchdog expired");
            // SAFETY: this is the probe's own process pseudo-handle; no other process is touched.
            unsafe {
                windows_sys::Win32::System::Threading::TerminateProcess(
                    windows_sys::Win32::System::Threading::GetCurrentProcess(),
                    124,
                );
            }
        }
    });
    let clock: Arc<dyn Clock> = Arc::new(MonotonicClock::default());
    let budget = || Deadline::new(5000, clock.clone(), Cancellation::default()).unwrap();
    let fixture = scratch_current(clock.clone(), &budget());
    let fixture = match fixture {
        Ok(fixture) => fixture,
        Err(error) => {
            println!(
                "scratch fixture refused: {error}; native rows unrun; cleanup UNVERIFIED if create dispatched"
            );
            let _ = finished.send(());
            watchdog.join().unwrap();
            panic!("scratch admission refused");
        }
    };
    println!(
        "scratch before: exact random root absent witnessed by strict FILE_CREATE; realm=fixture; Limited=true"
    );
    let trial = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
        || -> Result<(), NativeError> {
            let lock = fixture.lock(&budget())?;
            assert_eq!(fixture.lock(&budget()).err(), Some(NativeError::Busy));
            let name = PrivateName::new("read-fixture.json")?;
            fixture.write(&lock, name.clone(), b"owned-by-this-fixture", &budget())?;
            assert_eq!(
                fixture.read(name.clone(), 64, &budget())?.as_deref(),
                Some(b"owned-by-this-fixture".as_slice())
            );
            assert_eq!(
                fixture.read(name.clone(), 4, &budget()).err(),
                Some(NativeError::Oversize)
            );
            assert!(fixture.case_alias_refusal(name, &budget())?);
            let acl_name = PrivateName::new("acl-fixture.json")?;
            fixture.write(&lock, acl_name.clone(), b"owned", &budget())?;
            assert!(fixture.foreign_acl_refusal(acl_name, &budget())?);
            assert!(fixture.ancestor_refusal(&budget())?);
            assert!(fixture.reparse_refusal(&budget())?);
            let old = encode_record(&RecordName::Receipt, serde_json::json!({"generation":1}))?;
            assert_eq!(
                fixture
                    .publish(&lock, RecordName::Receipt, &old, &budget())?
                    .state,
                PublicationRecovery::NewPublished
            );
            let new = encode_record(&RecordName::Receipt, serde_json::json!({"generation":2}))?;
            let interrupted =
                fixture.publish_interrupted(&lock, RecordName::Receipt, &new, &budget())?;
            assert_eq!(interrupted.state, PublicationRecovery::OldRetained);
            assert_eq!(interrupted.native_failure, Some(NativeError::Unavailable)); // Explicit fixture stop, not an OS failure.
            assert_eq!(
                fixture.read(PrivateName::new("receipt.json")?, 1024, &budget())?,
                Some(old)
            );
            drop(lock);
            println!(
                "scratch rows passed: lock exclusion, bounded read, case alias, foreign leaf/ancestor ACL, static reparse, whole publication, injected interrupted-publication observation"
            );
            println!("UNRUN: real cross-volume and hardware power-loss; reparse race is FAKE-only");
            Ok(())
        },
    ));
    let cleanup = fixture.cleanup(&budget());
    println!(
        "scratch after: exact creator-ledger cleanup={}; root absent={}; recursion=false",
        cleanup.is_ok(),
        cleanup.is_ok()
    );
    let _ = finished.send(());
    watchdog.join().unwrap();
    cleanup.expect("scratch cleanup must be positively verified");
    trial
        .expect("native trial panicked")
        .expect("native trial refused");
}
