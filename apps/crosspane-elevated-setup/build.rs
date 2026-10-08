//! Build script of the elevated helper (WP-W4.1c, T21). On MSVC the helper's dependent DLLs load only
//! from System32, so a DLL planted beside the helper is never loaded.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_ENV").is_ok_and(|env| env == "msvc") {
        println!("cargo:rustc-link-arg-bin=crosspane-elevated-setup=/DEPENDENTLOADFLAG:0x800");
    }
}
