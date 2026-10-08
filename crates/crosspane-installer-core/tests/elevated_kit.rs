#![allow(clippy::unwrap_used)] // Test fixtures may assert successful setup.
use crosspane_installer_core::elevated::driver::{DRIVER_BINARY, DRIVER_CATALOG, DRIVER_INF};
use crosspane_installer_core::elevated::kit::{
    KIT_SCHEMA, KitDocument, KitFile, KitManifest, KitPin, MAX_KIT_FILE_BYTES,
    MAX_KIT_INVENTORY_BYTES, parse_sha256,
};
use crosspane_installer_core::elevated::{DRIVER_DIRECTORY, ElevatedError, HELPER_IMAGE};
use serde_json::json;

/// The four kit files in `KitFile::ALL` order: the inventory name, a size and the byte that fills
/// the digest (see `digest_hex`).
const FILES: [(&str, u64, u8); 4] = [
    ("helper", 4_194_304, 0x11),
    ("driver-inf", 3_072, 0x22),
    ("driver-catalog", 12_288, 0x33),
    ("driver-binary", 2_097_152, 0x44),
];

/// The frozen inventory shape with four valid entries in `KitFile::ALL` order.
const FROZEN: &str = concat!(
    r#"{"schema_version":1,"files":["#,
    r#"{"file":"helper","size":4194304,"sha256":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"},"#,
    r#"{"file":"driver-inf","size":3072,"sha256":"fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210"},"#,
    r#"{"file":"driver-catalog","size":12288,"sha256":"a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5"},"#,
    r#"{"file":"driver-binary","size":2097152,"sha256":"ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"}"#,
    r#"]}"#,
);

/// The digest text of `byte` repeated 32 times: 64 lowercase hex digits.
fn digest_hex(byte: u8) -> String {
    format!("{byte:02x}").repeat(32)
}

/// One inventory entry in the frozen shape.
fn entry(file: &str, size: u64, sha256: &str) -> String {
    format!(r#"{{"file":"{file}","size":{size},"sha256":"{sha256}"}}"#)
}

/// The inventory text with `entries` in the frozen shape.
fn inventory(schema_version: u32, entries: &[String]) -> String {
    format!(
        r#"{{"schema_version":{schema_version},"files":[{}]}}"#,
        entries.join(",")
    )
}

/// The four valid entries, in `KitFile::ALL` order.
fn valid_entries() -> Vec<String> {
    FILES
        .iter()
        .map(|&(file, size, byte)| entry(file, size, &digest_hex(byte)))
        .collect()
}

fn document(text: &str) -> KitDocument {
    serde_json::from_str(text).unwrap()
}

fn admit_text(text: &str) -> Result<KitManifest, ElevatedError> {
    KitManifest::admit(document(text))
}

/// Admits `entries` under schema 1.
fn admit(entries: &[String]) -> Result<KitManifest, ElevatedError> {
    admit_text(&inventory(1, entries))
}

/// The eight bytes repeated four times.
fn pattern(bytes: &[u8; 8]) -> [u8; 32] {
    std::array::from_fn(|index| bytes[index % 8])
}

/// The pins that `FROZEN` admits, in `KitFile::ALL` order.
fn frozen_pins() -> [KitPin; 4] {
    [
        KitPin {
            file: KitFile::Helper,
            size: 4_194_304,
            sha256: pattern(&[0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]),
        },
        KitPin {
            file: KitFile::DriverInf,
            size: 3_072,
            sha256: pattern(&[0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10]),
        },
        KitPin {
            file: KitFile::DriverCatalog,
            size: 12_288,
            sha256: [0xa5; 32],
        },
        KitPin {
            file: KitFile::DriverBinary,
            size: 2_097_152,
            sha256: [0xff; 32],
        },
    ]
}

/// `KitManifest` stores its pins in `KitFile::ALL` order. The derived `Debug` prints them in
/// storage order, so the positions of the file names must increase.
fn assert_pins_in_order(manifest: &KitManifest) {
    let debug = format!("{manifest:?}");
    let positions: Vec<usize> = KitFile::ALL
        .iter()
        .map(|file| debug.find(&format!("file: {file:?}")).unwrap())
        .collect();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "{debug}"
    );
    for file in KitFile::ALL {
        assert_eq!(manifest.pin(file).file, file);
    }
}

#[test]
fn the_frozen_inventory_is_admitted_and_pins_each_file() {
    assert!(FROZEN.len() <= MAX_KIT_INVENTORY_BYTES);
    let manifest = admit_text(FROZEN).unwrap();
    for pin in frozen_pins() {
        assert_eq!(manifest.pin(pin.file), &pin);
    }
}

#[test]
fn pins_are_in_kit_file_all_order_whatever_the_inventory_order() {
    let manifest = admit_text(FROZEN).unwrap();
    assert_pins_in_order(&manifest);

    // The inventory may list the files in any order: the admitted manifest is the same.
    let mut reversed = valid_entries();
    reversed.reverse();
    let reversed_manifest = admit(&reversed).unwrap();
    assert_eq!(reversed_manifest, admit(&valid_entries()).unwrap());
    assert_pins_in_order(&reversed_manifest);
}

#[test]
fn matches_is_true_only_for_the_exact_size_and_digest() {
    let manifest = admit_text(FROZEN).unwrap();
    for pin in frozen_pins() {
        assert!(
            manifest.matches(pin.file, pin.size, &pin.sha256),
            "{:?}",
            pin.file
        );
        assert!(!manifest.matches(pin.file, pin.size + 1, &pin.sha256));
        assert!(!manifest.matches(pin.file, pin.size - 1, &pin.sha256));
        let mut other = pin.sha256;
        other[31] ^= 1;
        assert!(!manifest.matches(pin.file, pin.size, &other));
        assert!(!manifest.matches(pin.file, pin.size, &[0; 32]));
    }
    // Another file's size and digest do not match this file.
    let helper = *manifest.pin(KitFile::Helper);
    assert!(!manifest.matches(KitFile::DriverInf, helper.size, &helper.sha256));
}

#[test]
fn size_must_be_from_one_byte_to_the_kit_maximum() {
    for (index, &(file, _, byte)) in FILES.iter().enumerate() {
        for (size, admitted) in [
            (0, false),
            (1, true),
            (MAX_KIT_FILE_BYTES, true),
            (MAX_KIT_FILE_BYTES + 1, false),
        ] {
            let mut entries = valid_entries();
            entries[index] = entry(file, size, &digest_hex(byte));
            let result = admit(&entries);
            if admitted {
                assert_eq!(
                    result.unwrap().pin(KitFile::ALL[index]).size,
                    size,
                    "{file} size {size}"
                );
            } else {
                assert_eq!(result, Err(ElevatedError::Kit), "{file} size {size}");
            }
        }
    }
}

#[test]
fn schema_version_must_be_one() {
    for schema_version in [0, 2] {
        assert_eq!(
            admit_text(&inventory(schema_version, &valid_entries())),
            Err(ElevatedError::Kit),
            "schema {schema_version}"
        );
    }
    assert!(admit(&valid_entries()).is_ok());
}

#[test]
fn a_file_listed_twice_is_refused_even_among_five_entries() {
    let entries = valid_entries();

    let mut five = entries.clone();
    five.push(entry("driver-inf", 7, &digest_hex(0x99)));
    assert_eq!(admit(&five), Err(ElevatedError::Kit));

    let mut five_again = entries.clone();
    five_again.push(entries[0].clone());
    assert_eq!(admit(&five_again), Err(ElevatedError::Kit));

    // Four entries that repeat one file and leave another out are refused too.
    let repeated = vec![
        entries[0].clone(),
        entries[0].clone(),
        entries[2].clone(),
        entries[3].clone(),
    ];
    assert_eq!(admit(&repeated), Err(ElevatedError::Kit));
}

#[test]
fn a_missing_file_is_refused() {
    let entries = valid_entries();
    assert_eq!(admit(&entries[..3]), Err(ElevatedError::Kit));
    for skipped in 0..entries.len() {
        let without: Vec<String> = entries
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != skipped)
            .map(|(_, entry)| entry.clone())
            .collect();
        assert_eq!(
            admit(&without),
            Err(ElevatedError::Kit),
            "without {skipped}"
        );
    }
    assert_eq!(admit(&[]), Err(ElevatedError::Kit));
}

#[test]
fn unknown_fields_and_file_names_are_refused_by_serde() {
    let entries = valid_entries();
    assert!(serde_json::from_str::<KitDocument>(&inventory(1, &entries)).is_ok());

    let top = format!(
        r#"{{"schema_version":1,"files":[{}],"extra":1}}"#,
        entries.join(",")
    );
    assert!(serde_json::from_str::<KitDocument>(&top).is_err());

    let mut item = entries.clone();
    item[0] = format!(
        r#"{{"file":"helper","size":4194304,"sha256":"{}","extra":true}}"#,
        digest_hex(0x11)
    );
    assert!(serde_json::from_str::<KitDocument>(&inventory(1, &item)).is_err());

    let mut installer = entries.clone();
    installer[0] = entry("installer", 1, &digest_hex(0x11));
    assert!(serde_json::from_str::<KitDocument>(&inventory(1, &installer)).is_err());
}

#[test]
fn serde_names_are_exactly_the_inventory_file_values() {
    for (name, file) in [
        ("helper", KitFile::Helper),
        ("driver-inf", KitFile::DriverInf),
        ("driver-catalog", KitFile::DriverCatalog),
        ("driver-binary", KitFile::DriverBinary),
    ] {
        assert_eq!(
            serde_json::from_value::<KitFile>(json!(name)).unwrap(),
            file
        );
        assert_eq!(serde_json::to_value(file).unwrap(), json!(name));
    }
    for text in ["installer", "driver_inf", "DriverInf", "Helper", ""] {
        assert!(
            serde_json::from_value::<KitFile>(json!(text)).is_err(),
            "{text:?}"
        );
    }
}

#[test]
fn parse_sha256_reads_64_lowercase_hex_digits_in_byte_order() {
    let ascending = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    assert_eq!(ascending.len(), 64);
    assert_eq!(
        parse_sha256(ascending),
        Some(std::array::from_fn(|index| index as u8))
    );
    assert_eq!(
        parse_sha256(&"fedcba9876543210".repeat(4)),
        Some(pattern(&[0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10]))
    );
    assert_eq!(parse_sha256(&"ff".repeat(32)), Some([0xff; 32]));

    // The last digit is the low nibble of the last byte.
    let mut last = "0".repeat(63);
    last.push('1');
    assert_eq!(parse_sha256(&last).map(|digest| digest[31]), Some(1));
}

#[test]
fn parse_sha256_refuses_anything_but_64_lowercase_nonzero_hex_digits() {
    let refused = vec![
        "AB".repeat(32),
        format!("{}F", "a".repeat(63)),
        "a".repeat(63),
        "a".repeat(65),
        String::new(),
        "g".repeat(64),
        format!("{} ", "a".repeat(63)),
        "0".repeat(64),
        "00".repeat(32),
        "é".repeat(32),
    ];
    for text in &refused {
        assert_eq!(parse_sha256(text), None, "{text:?}");
    }
    // Sixty-four bytes of two-byte characters: the length gate passes and the digits refuse.
    assert_eq!("é".repeat(32).len(), 64);
}

#[test]
fn components_are_the_helper_image_or_the_driver_directory_and_leaf() {
    assert_eq!(KitFile::Helper.components(), &[HELPER_IMAGE]);
    assert_eq!(
        KitFile::DriverInf.components(),
        &[DRIVER_DIRECTORY, DRIVER_INF]
    );
    assert_eq!(
        KitFile::DriverCatalog.components(),
        &[DRIVER_DIRECTORY, DRIVER_CATALOG]
    );
    assert_eq!(
        KitFile::DriverBinary.components(),
        &[DRIVER_DIRECTORY, DRIVER_BINARY]
    );

    // The literal leaves that the kit installs.
    assert_eq!(HELPER_IMAGE, "crosspane-elevated-setup.exe");
    assert_eq!(DRIVER_DIRECTORY, "driver");
    assert_eq!(DRIVER_INF, "CrosspaneIdd.inf");
    assert_eq!(DRIVER_CATALOG, "CrosspaneIdd.cat");
    assert_eq!(DRIVER_BINARY, "CrosspaneIdd.dll");
    assert_eq!(
        KitFile::DriverInf.components(),
        &["driver", "CrosspaneIdd.inf"]
    );
}

#[test]
fn kit_file_order_and_inventory_constants() {
    assert_eq!(
        KitFile::ALL,
        [
            KitFile::Helper,
            KitFile::DriverInf,
            KitFile::DriverCatalog,
            KitFile::DriverBinary,
        ]
    );
    assert_eq!(MAX_KIT_INVENTORY_BYTES, 16 * 1024);
    assert_eq!(KIT_SCHEMA, 1);
}

/// Digest texts that `parse_sha256` refuses, each with a label for the failure message. Every one
/// must be refused by `KitManifest::admit` wherever it appears in the inventory.
fn malformed_digests() -> Vec<(&'static str, String)> {
    vec![
        ("64 uppercase hex digits", "ABCDEF0123456789".repeat(4)),
        ("64 zeros", "0".repeat(64)),
        ("63 digits", "a".repeat(63)),
        ("65 digits", "a".repeat(65)),
        ("64 bytes of two-byte characters", "é".repeat(32)),
        ("empty", String::new()),
    ]
}

#[test]
fn admit_refuses_each_malformed_sha256_in_each_entry() {
    let valid = valid_entries();
    assert!(admit(&valid).is_ok());
    // The two-byte case is 64 bytes: it passes the length gate and is refused on its digits.
    assert_eq!("é".repeat(32).len(), 64);

    for (index, &(file, size, _)) in FILES.iter().enumerate() {
        for (label, digest) in malformed_digests() {
            let mut entries = valid.clone();
            entries[index] = entry(file, size, &digest);
            assert_eq!(
                admit(&entries),
                Err(ElevatedError::Kit),
                "{file} sha256: {label}"
            );
        }
    }
}

#[test]
fn admit_refuses_uppercase_digits_and_admits_the_same_digits_in_lowercase() {
    let lower = "abcdef0123456789".repeat(4);
    let upper = lower.to_ascii_uppercase();
    assert_ne!(lower, upper);

    let mut entries = valid_entries();
    entries[0] = entry("helper", 4_194_304, &lower);
    assert!(admit(&entries).is_ok());
    entries[0] = entry("helper", 4_194_304, &upper);
    assert_eq!(admit(&entries), Err(ElevatedError::Kit));
}
