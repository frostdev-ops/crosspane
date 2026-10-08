//! Limited-token Windows foundation and an unavailable wizard port. The shell port owns no
//! install, agent, task or payload capability.
pub mod detect;
#[cfg(any(windows, test))]
#[cfg_attr(test, allow(dead_code))]
pub(crate) mod first_install;
#[cfg(windows)]
#[cfg_attr(test, allow(dead_code, unused_imports))]
pub(crate) mod integration;
pub mod native_io;
pub(crate) mod payload;
// Native removal consumers are excluded from the library unit-test graph; integration fakes include this new graph.
#[cfg_attr(test, allow(dead_code))]
pub(crate) mod removal;
// Native repair adapters are excluded from the library unit-test graph; focused fakes include the model.
#[cfg(any(windows, test))]
#[cfg_attr(test, allow(dead_code))]
pub(crate) mod repair;
pub mod service;
pub mod transport;

/// Fixed-argument binary entry only; it grants no root, process or operation authority.
#[doc(hidden)]
pub fn replace_helper_mode(arguments: &[std::ffi::OsString]) -> native_io::NativeResult<bool> {
    payload::helper::replace_helper_mode(arguments)
}

/// Run the admitted fixed helper without initializing the installer GUI.
#[cfg(windows)]
#[doc(hidden)]
pub fn replace_helper_entry() -> native_io::NativeResult<()> {
    match payload::helper::replace_helper_entry()? {
        payload::helper::HelperExit::HandoffCompleted => Ok(()),
        payload::helper::HelperExit::RecoveryRetained => {
            Err(native_io::NativeError::OutcomeUnknown)
        }
    }
}

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
    integration::open(None)
}
