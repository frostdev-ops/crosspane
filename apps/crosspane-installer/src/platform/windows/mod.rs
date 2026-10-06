//! Limited-token Windows foundation and an unavailable wizard port. The shell port owns no
//! install, agent, task or payload capability.
pub mod detect;
pub mod native_io;
pub mod service;
pub mod transport;

/// Construct the shell without any install, agent or native-resource capability.
pub fn unavailable(
    clock: crate::live::Clock,
) -> anyhow::Result<Box<dyn crate::gui::InstallerController>> {
    Ok(Box::new(crate::live::LiveController::new(
        Box::new(Unavailable {
            agent: UnavailableAgent,
        }),
        clock,
    )?))
}

const UNAVAILABLE: &str = "Windows install isn't available in this build yet";

struct UnavailableAgent;
impl crate::agent_contract::AgentPort for UnavailableAgent {
    fn submit(
        &mut self,
        _call: crate::agent_contract::AgentCall,
    ) -> Result<(), crate::agent_contract::CallFailure> {
        Err(crate::agent_contract::CallFailure::Unavailable)
    }
    fn poll(&mut self) -> Vec<crate::agent_contract::AgentReply> {
        Vec::new()
    }
}

struct Unavailable {
    agent: UnavailableAgent,
}
impl crate::live::Platform for Unavailable {
    fn describe(&self) -> crate::live::PlatformDescription {
        use crate::live::{NativeStep, PlatformDescription};
        use crate::view::{ProgressGroup, ScreenId};
        use crosspane_installer_core::StepId;
        PlatformDescription {
            platform: crate::agent_contract::AgentPlatform::Windows,
            machine_label: "Windows PC".into(),
            steps: vec![NativeStep {
                id: StepId(10),
                prerequisites: Vec::new(),
                required_for_installed: true,
                required_for_ready: true,
                screen: ScreenId::Compatibility,
                group: ProgressGroup::Install,
                label: "Check this computer".into(),
                action_label: "Try again".into(),
                uses_status: false,
                settles_with_peer: false,
                agent_apply: None,
            }],
            connect_after: vec![StepId(10)],
            ready_after: vec![StepId(10)],
            hiding_choice: false,
            resume_note: None,
        }
    }
    fn submit(&mut self, _job: crate::live::NativeJob) -> Result<(), crate::live::NativeRefusal> {
        Err(crate::live::NativeRefusal::Unavailable(UNAVAILABLE.into()))
    }
    fn poll(&mut self) -> Vec<crate::live::NativeReport> {
        Vec::new()
    }
    fn agent(&mut self) -> &mut dyn crate::agent_contract::AgentPort {
        &mut self.agent
    }
    fn shutdown(&mut self) {}
}

/// System-font read and the unavailable port only; no native install authority is opened.
#[cfg(windows)]
pub fn open() -> anyhow::Result<(
    eframe::egui::FontDefinitions,
    Box<dyn crate::gui::InstallerController>,
)> {
    use anyhow::Context;
    let root =
        std::env::var_os("SystemRoot").context("Windows system font location is unavailable")?;
    let font = std::path::PathBuf::from(root)
        .join("Fonts")
        .join("segoeui.ttf");
    let fonts = crate::gui::load_review_font(&font)?;
    let started = std::time::Instant::now();
    let clock =
        std::sync::Arc::new(move || started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64);
    Ok((fonts, unavailable(clock)?))
}
