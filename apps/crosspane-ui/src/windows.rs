//! Safe Windows renderer choices and a disposable, explicitly opted-in live UI fixture.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use eframe::egui_wgpu::WgpuSetup;
use eframe::wgpu::{Backend, Backends, DeviceType, Dx12Compiler};
use serde_json::Value;

pub struct Acceptance {
    pub tab: &'static str,
    pub started: Instant,
    expected_name: String,
    expected_pid: u32,
}

impl Acceptance {
    pub fn from_env(screenshot: Option<&Path>) -> Result<Option<Self>> {
        let Some(value) = std::env::var_os("CROSSPANE_UI_ACCEPTANCE_LIVE") else {
            return Ok(None);
        };
        ensure!(value == "1", "invalid live UI acceptance switch");
        ensure!(
            std::env::var("CROSSPANE_UI_DEMO").as_deref() != Ok("1"),
            "live UI acceptance cannot use demo data"
        );
        let appdata = env_path("APPDATA")?;
        let local = env_path("LOCALAPPDATA")?;
        let runtime = env_path("CROSSPANE_RUNTIME_DIR")?;
        let root = appdata.parent().context("scratch UI root is absent")?;
        let suffix = root
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(scratch_suffix)
            .context("live UI acceptance requires a unique E1 scratch root")?;
        ensure!(
            root.parent().map(std::fs::canonicalize).transpose()?
                == Some(std::fs::canonicalize(std::env::temp_dir())?),
            "live UI acceptance must use the temporary directory"
        );
        ensure!(
            appdata == root.join("roaming")
                && local == root.join("local")
                && runtime == root.join("runtime")
                && std::env::var("CROSSPANE_ACCEPTANCE_E1_ONLY").as_deref() == Ok("1")
                && ["CROSSPANE_DISCOVERY", "CROSSPANE_AUDIO", "CROSSPANE_GPU"]
                    .into_iter()
                    .all(|name| std::env::var(name).as_deref() == Ok("0")),
            "live UI acceptance requires the isolated E1 scratch environment"
        );
        let tab = match std::env::var("CROSSPANE_UI_TAB").as_deref() {
            Ok("machines") => "machines",
            Ok("layout") => "layout",
            Ok("pairing") => "pairing",
            Ok("windows") => "windows",
            _ => anyhow::bail!("live UI acceptance requires an explicit valid tab"),
        };
        let screenshot =
            screenshot.context("live UI acceptance requires an own-renderer output")?;
        let output = root.join("screenshots");
        ensure!(
            screenshot == output.join(format!("{tab}.png"))
                && std::fs::canonicalize(&output)?
                    == std::fs::canonicalize(root)?.join("screenshots"),
            "live UI output must be the selected tab inside its scratch screenshot directory"
        );
        let expected_pid = std::env::var("CROSSPANE_UI_ACCEPTANCE_AGENT_PID")
            .context("live UI acceptance requires the owned agent PID")?
            .parse::<u32>()
            .context("invalid owned agent PID")?;
        ensure!(expected_pid != 0, "invalid owned agent PID");
        Ok(Some(Self {
            tab,
            started: Instant::now(),
            expected_name: format!("wp-w1-5b-{suffix}"),
            expected_pid,
        }))
    }

    /// Only successful, real Status for this exact scratch agent can admit a live capture.
    pub fn check_status(&self, status: &Value) -> Result<()> {
        let installer = &status["installer"];
        ensure!(
            status["name"].as_str() == Some(&self.expected_name)
                && status["peers"].as_array().is_some_and(Vec::is_empty)
                && installer["instance"]["pid"].as_u64() == Some(u64::from(self.expected_pid))
                && installer["instance"].get("uid").is_some_and(Value::is_null)
                && installer["keystore"].as_str() == Some("file"),
            "live UI Status does not identify the isolated file-keystore agent"
        );
        Ok(())
    }
}

fn env_path(name: &str) -> Result<PathBuf> {
    let path = PathBuf::from(std::env::var_os(name).with_context(|| format!("{name} is absent"))?);
    ensure!(path.is_absolute(), "{name} must be absolute");
    Ok(path)
}

fn scratch_suffix(name: &str) -> Option<&str> {
    name.strip_prefix("crosspane-WP-W1.5b-")
        .filter(|suffix| suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

pub fn native_options(
    mut options: eframe::NativeOptions,
    acceptance: bool,
) -> Result<eframe::NativeOptions> {
    options.renderer = eframe::Renderer::Wgpu;
    let WgpuSetup::CreateNew(setup) = &mut options.wgpu_options.wgpu_setup else {
        anyhow::bail!("unexpected settings renderer configuration");
    };
    setup.instance_descriptor.backends = Backends::DX12;
    // The proxy uses this same OS-provided shader compiler; no ambient dxcompiler DLL needed.
    setup
        .instance_descriptor
        .backend_options
        .dx12
        .shader_compiler = Dx12Compiler::Fxc;
    if acceptance {
        // This is reached only after the scratch guard. Production retains normal selection.
        setup.native_adapter_selector = Some(Arc::new(|adapters, surface| {
            adapters
                .iter()
                .find(|adapter| {
                    let info = adapter.get_info();
                    info.backend == Backend::Dx12
                        && info.device_type == DeviceType::Cpu
                        && surface.is_some_and(|surface| adapter.is_surface_supported(surface))
                })
                .cloned()
                .ok_or_else(|| "compatible DX12 software adapter is unavailable".into())
        }));
    }
    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_status_requires_the_exact_owned_file_agent() {
        let mode = Acceptance {
            tab: "machines",
            started: Instant::now(),
            expected_name: "wp-w1-5b-fixture".into(),
            expected_pid: 123,
        };
        let good = serde_json::json!({"name":"wp-w1-5b-fixture","peers":[],
            "installer":{"instance":{"pid":123,"uid":null},"keystore":"file"}});
        assert!(mode.check_status(&good).is_ok());
        for (path, replacement) in [
            ("/name", serde_json::json!("another-agent")),
            ("/peers", serde_json::json!([{}])),
            ("/installer/instance/pid", serde_json::json!(124)),
            ("/installer/instance/uid", serde_json::json!(42)),
            ("/installer/keystore", serde_json::json!("os")),
        ] {
            let mut other = good.clone();
            *other.pointer_mut(path).unwrap() = replacement;
            assert!(mode.check_status(&other).is_err());
        }
        assert!(mode.check_status(&serde_json::json!({})).is_err());
    }

    #[test]
    fn scratch_names_require_the_existing_unique_e1_contract() {
        assert_eq!(
            scratch_suffix("crosspane-WP-W1.5b-0123456789abcdef0123456789abcdef"),
            Some("0123456789abcdef0123456789abcdef")
        );
        for name in [
            "owner",
            "crosspane-WP-W1.5b-fixture",
            "crosspane-WP-W1.5b-../../owner",
        ] {
            assert!(scratch_suffix(name).is_none());
        }
    }
}
