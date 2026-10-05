//! Pure contract tests; no Windows credential or DPAPI calls.

use crosspane_platform::PlatformError;
use crosspane_platform_windows::model::keystore::*;
use proptest::prelude::*;

#[test]
fn lowercase_short_names_are_accepted_without_normalization() {
    for name in ["device-key", "a", "key_1.0", "a-b_c.d", &"z".repeat(64)] {
        assert!(validate_name(name).is_ok());
    }
}

#[test]
fn invalid_names_are_refused_before_native_io() {
    for name in [
        "",
        ".hidden",
        "a..b",
        "..",
        "Upper",
        "KEY",
        "a/b",
        "a\\b",
        "a:b",
        "λ",
        "a b",
        "a\0b",
        "a\nb",
        &"z".repeat(65),
    ] {
        assert!(validate_name(name).is_err(), "invalid name was admitted");
    }
}

#[test]
fn protected_blob_capacity_is_exact_without_truncation_or_overhead_guessing() {
    for size in [0, 1, MAX_BLOB_SIZE - 1, MAX_BLOB_SIZE] {
        assert!(validate_blob_size(size).is_ok());
    }
    for size in [MAX_BLOB_SIZE + 1, usize::MAX] {
        assert!(matches!(
            validate_blob_size(size),
            Err(PlatformError::TooLarge)
        ));
    }
}

#[test]
fn only_credread_not_found_means_absence_and_missing_delete_succeeds() {
    assert!(read_error(ERROR_NOT_FOUND).is_ok());
    assert!(delete_error(ERROR_NOT_FOUND).is_ok());
    for code in [0, 5, 13, 87, 1312, ERROR_PASSWORD_RESTRICTION] {
        assert!(read_error(code).is_err());
        assert!(delete_error(code).is_err());
    }
    // The same code from DPAPI or CredWrite is still an error, never absence.
    assert!(matches!(
        native_error(ERROR_NOT_FOUND),
        PlatformError::Backend(_)
    ));
}

#[test]
fn ui_forbidden_password_restriction_is_interaction_required() {
    assert!(matches!(
        native_error(ERROR_PASSWORD_RESTRICTION),
        PlatformError::InteractionRequired
    ));
}

#[test]
fn error_messages_contain_only_operation_metadata_and_numeric_error() {
    assert_eq!(native_error(5).to_string(), "Windows key store error 5");
}

proptest! {
    #[test]
    fn name_admission_matches_the_frozen_lowercase_policy(name in ".{0,90}") {
        let expected = (1..=64).contains(&name.len())
            && !name.starts_with('.') && !name.contains("..")
            && name.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(&c));
        prop_assert_eq!(validate_name(&name).is_ok(), expected);
    }

    #[test]
    fn protected_size_admission_has_no_silent_truncation(size in any::<usize>()) {
        prop_assert_eq!(validate_blob_size(size).is_ok(), size <= MAX_BLOB_SIZE);
    }
}
