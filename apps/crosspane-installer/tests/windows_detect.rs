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
use detect::*;

#[test]
fn known_folder_redirection_cannot_change_the_fixed_install_root() {
    let facts = FolderFacts {
        local: "C:\\Users\\fixture\\AppData\\Local".into(),
        roaming: "C:\\Users\\fixture\\AppData\\Roaming".into(),
        programs: "C:\\Users\\fixture\\AppData\\Local\\Programs".into(),
        environment_local: "C:\\Users\\fixture\\AppData\\Local".into(),
        environment_roaming: "C:\\Users\\fixture\\AppData\\Roaming".into(),
    };
    let paths = FixedPaths::admit(facts.clone()).unwrap();
    assert_eq!(
        paths.install(),
        "C:\\Users\\fixture\\AppData\\Local\\Programs\\Crosspane"
    );
    assert_eq!(
        paths.installer(),
        "C:\\Users\\fixture\\AppData\\Local\\Crosspane\\Installer"
    );
    for bad in [
        FolderFacts {
            programs: "D:\\Programs".into(),
            ..facts.clone()
        },
        FolderFacts {
            environment_local: "C:\\other".into(),
            ..facts.clone()
        },
        FolderFacts {
            local: "\\\\server\\share".into(),
            ..facts
        },
    ] {
        assert!(FixedPaths::admit(bad).is_err());
    }
}

#[test]
fn support_never_turns_missing_or_failed_observations_into_readiness() {
    assert_eq!(
        classify(&SupportFacts {
            limited: Fact::Known(true),
            folders: Fact::Known(true),
            ntfs: Fact::Known(true),
            authority: Fact::Known(true)
        }),
        Eligibility::Supported
    );
    for facts in [
        SupportFacts {
            limited: Fact::Known(false),
            ..SupportFacts::unknown()
        },
        SupportFacts {
            ntfs: Fact::Known(false),
            ..SupportFacts::unknown()
        },
    ] {
        assert_eq!(classify(&facts), Eligibility::Unsupported);
    }
    assert_eq!(classify(&SupportFacts::unknown()), Eligibility::Unknown);
    assert_eq!(
        classify(&SupportFacts {
            limited: Fact::Known(true),
            folders: Fact::Known(true),
            ntfs: Fact::Known(true),
            authority: Fact::Unknown
        }),
        Eligibility::Unknown
    );
}

#[test]
fn native_failure_is_not_missing() {
    assert_eq!(absence_from_error(2), Ok(true));
    assert_eq!(absence_from_error(3), Ok(true));
    for error in [5, 32, 53, 87] {
        assert!(absence_from_error(error).is_err());
    }
}
