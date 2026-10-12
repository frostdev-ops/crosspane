#![cfg(windows)]
//! Memory-only Authz descriptors and synthetic identities; no file ACL, token or task mutation.

use crosspane_installer::platform::windows::native_io::{
    NativeError,
    files::{Ace, AclFacts, Admission, ComponentFacts, FileIdentity, Principal, admit_component},
};
use std::{ffi::c_void, ptr};
use windows_sys::Win32::{
    Foundation::{ERROR_ACCESS_DENIED, GetLastError, LUID, LocalFree},
    Security::{Authorization::*, SID_AND_ATTRIBUTES},
    System::SystemServices::SE_GROUP_ENABLED,
};

const OWNER: &str = "S-1-5-21-100-200-300-1001";
const FOREIGN: &str = "S-1-5-21-100-200-300-1002";
const CAPABILITY: &str = "S-1-15-3-10-20-30-40-50-60-70";
const PACKAGE: &str = "S-1-15-2-100-200-300-400-500-600-700";
const MUTATIONS: [u32; 6] = [0x10000, 0x40, 0x100, 0x40000, 0x80000, 0xd0140];

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}

fn succeeded(result: i32) {
    // SAFETY: the immediately preceding native call's thread-local error is read only.
    assert_ne!(result, 0, "native error {}", unsafe { GetLastError() });
}

#[derive(Debug)]
struct LocalMemory(*mut c_void);
impl LocalMemory {
    fn sid(value: &str) -> Self {
        let value = wide(value);
        let mut pointer = ptr::null_mut();
        // SAFETY: terminated fixture SID and initialized writable output; allocation is retained.
        succeeded(unsafe { ConvertStringSidToSidW(value.as_ptr(), &mut pointer) });
        Self(pointer)
    }
    fn descriptor(value: &str) -> Self {
        let value = wide(value);
        let mut pointer = ptr::null_mut();
        // SAFETY: terminated fixture SDDL and initialized output; no kernel object is modified.
        succeeded(unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                value.as_ptr(),
                SDDL_REVISION_1,
                &mut pointer,
                ptr::null_mut(),
            )
        });
        Self(pointer)
    }
}
impl Drop for LocalMemory {
    fn drop(&mut self) {
        // SAFETY: each wrapper owns exactly one allocation returned by an SDDL/SID conversion.
        unsafe { LocalFree(self.0) };
    }
}

#[derive(Debug)]
struct ResourceManager(AUTHZ_RESOURCE_MANAGER_HANDLE);
impl ResourceManager {
    fn new() -> Self {
        let mut handle = ptr::null_mut();
        // SAFETY: memory-only manager without auditing, callbacks or borrowed arguments.
        succeeded(unsafe {
            AuthzInitializeResourceManager(
                AUTHZ_RM_FLAG_NO_AUDIT,
                None,
                None,
                None,
                ptr::null(),
                &mut handle,
            )
        });
        Self(handle)
    }
}
impl Drop for ResourceManager {
    fn drop(&mut self) {
        // SAFETY: the owned manager outlives all its client contexts in each test.
        unsafe { AuthzFreeResourceManager(self.0) };
    }
}

#[derive(Debug)]
struct Context(AUTHZ_CLIENT_CONTEXT_HANDLE);
impl Context {
    fn new(manager: &ResourceManager, user: &LocalMemory) -> Self {
        let mut handle = ptr::null_mut();
        // SAFETY: retained fixture SID; SKIP_TOKEN_GROUPS prevents account or network lookups.
        succeeded(unsafe {
            AuthzInitializeContextFromSid(
                AUTHZ_SKIP_TOKEN_GROUPS,
                user.0,
                manager.0,
                ptr::null(),
                LUID::default(),
                ptr::null(),
                &mut handle,
            )
        });
        Self(handle)
    }
    fn appcontainer(&self, package: &LocalMemory, capability: Option<&LocalMemory>) {
        let entry = capability.map(|sid| SID_AND_ATTRIBUTES {
            Sid: sid.0,
            Attributes: SE_GROUP_ENABLED as u32,
        });
        // SAFETY: retained synthetic package/capability SIDs; only this memory context changes.
        succeeded(unsafe {
            AuthzSetAppContainerInformation(
                self.0,
                package.0,
                u32::from(entry.is_some()),
                entry.as_ref().map_or(ptr::null(), ptr::from_ref),
            )
        });
    }
    fn allowed(&self, descriptor: &LocalMemory, desired: u32) -> bool {
        let request = AUTHZ_ACCESS_REQUEST {
            DesiredAccess: desired,
            ..Default::default()
        };
        let mut granted = 0;
        let mut error = 0;
        let mut sacl = 0;
        let mut reply = AUTHZ_ACCESS_REPLY {
            ResultListLength: 1,
            GrantedAccessMask: &mut granted,
            SaclEvaluationResults: &mut sacl,
            Error: &mut error,
        };
        // SAFETY: live memory-only context/descriptor and initialized one-entry reply buffers.
        succeeded(unsafe {
            AuthzAccessCheck(
                0,
                self.0,
                &request,
                ptr::null_mut(),
                descriptor.0,
                ptr::null(),
                0,
                &mut reply,
                ptr::null_mut(),
            )
        });
        if error == 0 {
            assert_eq!(granted & desired, desired);
            true
        } else {
            assert_eq!(error, ERROR_ACCESS_DENIED);
            assert_eq!(granted, 0);
            false
        }
    }
}
impl Drop for Context {
    fn drop(&mut self) {
        // SAFETY: exactly one owned Authz memory context is released before its manager.
        unsafe { AuthzFreeContext(self.0) };
    }
}

fn facts(private: bool, foreign: bool) -> ComponentFacts {
    let mut entries = vec![
        Ace {
            principal: Principal::User,
            mask: 0x1f01ff,
            inherit_only: false,
            supported: true,
        },
        Ace {
            principal: Principal::System,
            mask: 0x1f01ff,
            inherit_only: false,
            supported: true,
        },
    ];
    if !private {
        entries.push(Ace {
            principal: Principal::Capability,
            mask: 0x1f01ff,
            inherit_only: false,
            supported: true,
        });
    }
    if foreign {
        entries.push(Ace {
            principal: Principal::Foreign,
            mask: 0x1f01ff,
            inherit_only: false,
            supported: true,
        });
    }
    ComponentFacts {
        requested: "fixture".into(),
        actual: "fixture".into(),
        identity: FileIdentity {
            volume: 1,
            file: [1; 16],
        },
        directory: true,
        reparse: false,
        case_sensitive: false,
        links: 1,
        acl: AclFacts {
            owner: Principal::User,
            protected: private,
            entries,
        },
    }
}

#[test]
fn capability_ancestor_access_requires_the_ordinary_user_pass() {
    let manager = ResourceManager::new();
    let owner = LocalMemory::sid(OWNER);
    let foreign = LocalMemory::sid(FOREIGN);
    let capability = LocalMemory::sid(CAPABILITY);
    let package = LocalMemory::sid(PACKAGE);
    let descriptor = LocalMemory::descriptor(&format!(
        "O:{OWNER}G:SYD:(A;;FA;;;{OWNER})(A;;FA;;;SY)(A;;FA;;;{CAPABILITY})"
    ));
    let own_app = Context::new(&manager, &owner);
    own_app.appcontainer(&package, Some(&capability));
    let foreign_app = Context::new(&manager, &foreign);
    foreign_app.appcontainer(&package, Some(&capability));
    let foreign_normal = Context::new(&manager, &foreign);
    let own_without_cap = Context::new(&manager, &owner);
    own_without_cap.appcontainer(&package, None);
    for desired in MUTATIONS {
        assert!(own_app.allowed(&descriptor, desired));
        assert!(!foreign_app.allowed(&descriptor, desired));
        assert!(!foreign_normal.allowed(&descriptor, desired));
        assert!(!own_without_cap.allowed(&descriptor, desired));
    }
    assert_eq!(
        admit_component(&facts(false, false), Admission::Ancestor),
        Ok(())
    );
}

#[test]
fn private_user_system_acl_still_excludes_capability_enabled_appcontainers() {
    let manager = ResourceManager::new();
    let owner = LocalMemory::sid(OWNER);
    let foreign = LocalMemory::sid(FOREIGN);
    let capability = LocalMemory::sid(CAPABILITY);
    let package = LocalMemory::sid(PACKAGE);
    let descriptor =
        LocalMemory::descriptor(&format!("O:{OWNER}G:SYD:P(A;;FA;;;{OWNER})(A;;FA;;;SY)"));
    let own_normal = Context::new(&manager, &owner);
    let own_app = Context::new(&manager, &owner);
    own_app.appcontainer(&package, Some(&capability));
    let foreign_app = Context::new(&manager, &foreign);
    foreign_app.appcontainer(&package, Some(&capability));
    for desired in MUTATIONS {
        assert!(own_normal.allowed(&descriptor, desired));
        assert!(!own_app.allowed(&descriptor, desired));
        assert!(!foreign_app.allowed(&descriptor, desired));
    }
    assert_eq!(
        admit_component(&facts(true, false), Admission::PrivateDirectory),
        Ok(())
    );
}

#[test]
fn ordinary_foreign_mutation_is_refused_even_when_a_capability_also_grants_access() {
    let manager = ResourceManager::new();
    let foreign = LocalMemory::sid(FOREIGN);
    let capability = LocalMemory::sid(CAPABILITY);
    let package = LocalMemory::sid(PACKAGE);
    let descriptor = LocalMemory::descriptor(&format!(
        "O:{OWNER}G:SYD:(A;;FA;;;{OWNER})(A;;FA;;;{FOREIGN})(A;;FA;;;{CAPABILITY})"
    ));
    let foreign_app = Context::new(&manager, &foreign);
    foreign_app.appcontainer(&package, Some(&capability));
    for desired in MUTATIONS {
        assert!(foreign_app.allowed(&descriptor, desired));
    }
    assert_eq!(
        admit_component(&facts(false, true), Admission::Ancestor),
        Err(NativeError::Foreign)
    );
}

#[test]
fn a_synthetic_ordinary_authz_group_is_not_an_appcontainer_capability_context() {
    let manager = ResourceManager::new();
    let foreign = LocalMemory::sid(FOREIGN);
    let capability = LocalMemory::sid(CAPABILITY);
    let descriptor = LocalMemory::descriptor(&format!(
        "O:{OWNER}G:SYD:(A;;FA;;;{OWNER})(A;;FA;;;{CAPABILITY})"
    ));
    let foreign_normal = Context::new(&manager, &foreign);
    let entry = SID_AND_ATTRIBUTES {
        Sid: capability.0,
        Attributes: SE_GROUP_ENABLED as u32,
    };
    let mut handle = ptr::null_mut();
    // SAFETY: only an artificial Authz context is created, never an OS access token or group.
    succeeded(unsafe {
        AuthzAddSidsToContext(foreign_normal.0, &entry, 1, ptr::null(), 0, &mut handle)
    });
    let fabricated_group = Context(handle);
    // Ordinary users cannot manufacture this group in an OS token; the real capability tests
    // above use AuthzSetAppContainerInformation to exercise Windows' separate access passes.
    assert!(fabricated_group.allowed(&descriptor, 0xd0140));
}
