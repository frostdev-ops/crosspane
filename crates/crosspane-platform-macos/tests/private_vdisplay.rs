//! Also compiled as a standalone GUI driver by the two tests below: libtest owns the
//! process's main thread, while AppKit needs it running main_thread::run_app().
//! The display-only probe grows its mode to the descriptor maximum without moving windows.
#![cfg(all(target_os = "macos", feature = "private-vdisplay"))]
#![allow(unexpected_cfgs, clippy::unwrap_used, clippy::expect_used)]

#[cfg(not(crosspane_vdisplay_driver))]
fn run_driver(live: bool) {
    use std::process::Command;
    let deps = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/private_vdisplay.rs");
    let executable = deps.join(format!("private-vdisplay-gui-{}", std::process::id()));
    let mut compiler = Command::new("rustc");
    compiler
        .arg("--edition=2024")
        .arg("--cfg")
        .arg("test")
        .arg("--cfg")
        .arg("crosspane_vdisplay_driver")
        .arg("--cfg")
        .arg("feature=\"private-vdisplay\"")
        .arg("-L")
        .arg(format!("dependency={}", deps.display()));
    for name in [
        "crosspane_platform",
        "crosspane_types",
        "objc2",
        "objc2_foundation",
        "objc2_app_kit",
        "objc2_application_services",
        "objc2_core_foundation",
        "objc2_core_graphics",
        "dispatch2",
        "tracing",
    ] {
        let prefix = format!("lib{name}-");
        let native_version = if name == "objc2" {
            Some("objc2-0.6.4/".to_string())
        } else if name.starts_with("objc2_") {
            Some(format!("{}-0.3.2/", name.replace('_', "-")))
        } else {
            None
        };
        let library = std::fs::read_dir(&deps)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(&prefix)
                    && path
                        .extension()
                        .is_some_and(|extension| extension == "rlib")
            })
            // The workspace also builds objc2 0.5 / Foundation 0.2 for winit. Match the
            // allowed versions using rustc's dep-info rather than picking those by recency.
            .filter(|path| {
                native_version.as_ref().is_none_or(|version| {
                    let stem = path.file_stem().unwrap().to_string_lossy();
                    let info = deps.join(format!("{}.d", stem.trim_start_matches("lib")));
                    std::fs::read_to_string(info).is_ok_and(|info| info.contains(version))
                })
            })
            .max_by_key(|path| path.metadata().unwrap().modified().unwrap())
            .unwrap_or_else(|| panic!("missing compiled dependency {name}"));
        compiler
            .arg("--extern")
            .arg(format!("{name}={}", library.display()));
    }
    assert!(
        compiler
            .arg(source)
            .arg("-o")
            .arg(&executable)
            .status()
            .unwrap()
            .success()
    );
    let status = Command::new(&executable)
        .env("CROSSPANE_MAC_LIVE", if live { "1" } else { "0" })
        .env("CROSSPANE_MAC_GUI", "1")
        .status()
        .unwrap();
    std::fs::remove_file(executable).unwrap();
    assert!(status.success(), "GUI driver failed: {status}");
}

#[cfg(not(crosspane_vdisplay_driver))]
#[test]
fn gui_virtual_display_lifecycle() {
    if std::env::var("CROSSPANE_MAC_GUI").as_deref() != Ok("1") {
        eprintln!("skipped: display-only GUI test requires CROSSPANE_MAC_GUI=1 via run-in-gui.sh");
        return;
    }
    run_driver(false);
}

#[cfg(not(crosspane_vdisplay_driver))]
#[test]
fn live_textedit_park_and_restore() {
    if std::env::var("CROSSPANE_MAC_LIVE").as_deref() != Ok("1") {
        eprintln!(
            "skipped: TextEdit parking requires CROSSPANE_MAC_LIVE=1 in the lead's GUI session"
        );
        return;
    }
    run_driver(true);
}

// The driver compiles the implementation directly, so it tests the internal creation
// function without adding any production constructor or changing the lead-owned manifest.
#[cfg(crosspane_vdisplay_driver)]
#[path = "../src/clock.rs"]
mod clock;
#[cfg(crosspane_vdisplay_driver)]
#[path = "../src/main_thread.rs"]
mod main_thread;
#[cfg(crosspane_vdisplay_driver)]
#[allow(dead_code)]
#[path = "../src/permissions.rs"]
mod permissions;
#[cfg(crosspane_vdisplay_driver)]
#[allow(dead_code)]
#[path = "../src/private_vdisplay.rs"]
mod private_vdisplay;
#[cfg(crosspane_vdisplay_driver)]
#[allow(dead_code, unused_imports)]
#[path = "../src/windows.rs"]
mod windows;

#[cfg(crosspane_vdisplay_driver)]
fn main() {
    if std::env::var("CROSSPANE_MAC_GUI").as_deref() != Ok("1") {
        eprintln!("skipped: GUI driver requires CROSSPANE_MAC_GUI=1");
        return;
    }
    std::thread::spawn(|| {
        let result = std::panic::catch_unwind(|| {
            if std::env::var("CROSSPANE_MAC_LIVE").as_deref() == Ok("1") {
                private_vdisplay::tests::live_textedit();
            } else {
                private_vdisplay::tests::gui_lifecycle();
            }
        });
        // Drain RAII cleanup before exiting, including when an assertion unwinds.
        main_thread::on_main(std::time::Duration::from_secs(2), |_| {}).unwrap();
        std::process::exit(if result.is_ok() { 0 } else { 1 });
    });
    main_thread::run_app().expect("AppKit main loop");
}
