use super::{NativeError, NativeResult};

pub const DELETE: u32 = 0x10000;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileIdentity {
    pub volume: u64,
    pub file: [u8; 16],
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Principal {
    User,
    System,
    Administrators,
    TrustedInstaller,
    Foreign,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ace {
    pub principal: Principal,
    pub mask: u32,
    pub inherit_only: bool,
    pub supported: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AclFacts {
    pub owner: Principal,
    pub protected: bool,
    pub entries: Vec<Ace>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComponentFacts {
    pub requested: String,
    pub actual: String,
    pub identity: FileIdentity,
    pub directory: bool,
    pub reparse: bool,
    pub case_sensitive: bool,
    pub links: u32,
    pub acl: AclFacts,
}
#[derive(Clone, Copy, Debug)]
pub enum Admission {
    Ancestor,
    PrivateDirectory,
    PrivateFile,
}
pub fn admit_component(facts: &ComponentFacts, admission: Admission) -> NativeResult<()> {
    let private = !matches!(admission, Admission::Ancestor);
    let directory = !matches!(admission, Admission::PrivateFile);
    if facts.reparse
        || facts.case_sensitive
        || facts.directory != directory
        || facts.requested != facts.actual
        || facts.identity.volume == 0
        || facts.identity.file == [0; 16]
        || (!directory && facts.links != 1)
        || (private && (facts.acl.owner != Principal::User || !facts.acl.protected))
        || facts.acl.owner == Principal::Foreign
    {
        return Err(NativeError::Foreign);
    }
    // Trusted machine administrators remain part of Windows' security boundary. An existing
    // ancestor may allow foreign creation of a sibling, but never changes/deletion of our child.
    const ANCESTOR_MUTATION: u32 =
        DELETE | 0x40 | 0x100 | 0x40000 | 0x80000 | 0x10000000 | 0x40000000;
    for ace in &facts.acl.entries {
        if ace.inherit_only {
            continue;
        }
        if !ace.supported {
            return Err(NativeError::Foreign);
        }
        let unsafe_access = if private {
            !matches!(ace.principal, Principal::User | Principal::System) && ace.mask != 0
        } else {
            ace.principal == Principal::Foreign && ace.mask & ANCESTOR_MUTATION != 0
        };
        if unsafe_access {
            return Err(NativeError::Foreign);
        }
    }
    Ok(())
}
pub fn admit_chain(chain: &[ComponentFacts], volume: u64) -> NativeResult<()> {
    if chain.is_empty() || chain.len() > 64 || volume == 0 {
        return Err(NativeError::Foreign);
    }
    for component in chain {
        admit_component(component, Admission::Ancestor)?;
        if component.identity.volume != volume {
            return Err(NativeError::Foreign);
        }
    }
    Ok(())
}
pub fn same_chain(old: &[ComponentFacts], new: &[ComponentFacts]) -> NativeResult<()> {
    if old.len() != new.len()
        || old
            .iter()
            .zip(new)
            .any(|(a, b)| a.identity != b.identity || a.requested != b.requested)
    {
        Err(NativeError::Foreign)
    } else {
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrivateName(String);
impl PrivateName {
    pub fn new(name: &str) -> NativeResult<Self> {
        let stem = name.split('.').next().ok_or(NativeError::Invalid)?;
        let device = matches!(stem, "con" | "prn" | "aux" | "nul" | "clock$")
            || (stem.len() == 4
                && (stem.starts_with("com") || stem.starts_with("lpt"))
                && stem.as_bytes()[3].is_ascii_digit());
        if name.is_empty()
            || name.len() > 128
            || name.starts_with('.')
            || name.ends_with('.')
            || device
            || !name.bytes().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'-' | b'_')
            })
        {
            return Err(NativeError::Invalid);
        }
        Ok(Self(name.into()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
pub const MAX_RECORD_BYTES: usize = 1024 * 1024;
pub fn check_read_size(length: usize, cap: usize) -> NativeResult<()> {
    if cap == 0 || cap > MAX_RECORD_BYTES {
        Err(NativeError::Invalid)
    } else if length > cap {
        Err(NativeError::Oversize)
    } else {
        Ok(())
    }
}

/// Exactly one filesystem component; this is not an arbitrary path capability.
#[derive(Clone)]
pub(crate) struct ComponentName(String);
impl ComponentName {
    pub(crate) fn new(name: &str) -> NativeResult<Self> {
        let stem = name
            .split('.')
            .next()
            .ok_or(NativeError::Invalid)?
            .to_ascii_lowercase();
        let device = matches!(stem.as_str(), "con" | "prn" | "aux" | "nul" | "clock$")
            || (stem.len() == 4
                && (stem.starts_with("com") || stem.starts_with("lpt"))
                && stem.as_bytes()[3].is_ascii_digit());
        if name.is_empty()
            || name.encode_utf16().count() > 255
            || name == "."
            || name == ".."
            || name.contains(['\\', '/', ':', '\0'])
            || name.ends_with(['.', ' '])
            || device
            || name.chars().any(char::is_control)
        {
            return Err(NativeError::Invalid);
        }
        Ok(Self(name.into()))
    }
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObjectKind {
    Directory,
    File,
    Opaque,
}
pub(crate) struct ComponentRequest {
    pub name: ComponentName,
    pub kind: ObjectKind,
    pub create: bool,
    pub dont_reparse: bool,
    pub open_reparse_point: bool,
}
/// Shared dispatch seam for the SDK binding and deterministic parent-path race fakes.
/// A parent is always a retained admitted object, never reconstructed from a record/path.
pub(crate) trait RelativeIo {
    type Parent;
    type Object;
    fn submit(
        &self,
        parent: Option<&Self::Parent>,
        request: &ComponentRequest,
    ) -> NativeResult<Self::Object>;
}
pub(crate) fn relative_component<I: RelativeIo>(
    io: &I,
    parent: &I::Parent,
    name: &ComponentName,
    kind: ObjectKind,
    create: bool,
    deadline: &super::Deadline,
) -> NativeResult<Option<I::Object>> {
    deadline.check()?;
    let request = ComponentRequest {
        name: name.clone(),
        kind,
        create,
        dont_reparse: true,
        open_reparse_point: true,
    };
    let result = io.submit(Some(parent), &request);
    deadline.check()?;
    match result {
        Err(NativeError::Missing) if !create => Ok(None),
        value => value.map(Some),
    }
}

#[cfg(windows)]
pub(crate) mod native {
    use super::super::{Deadline, identity::Sid};
    use super::*;
    use std::{
        fs::File,
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
        sync::Arc,
    };
    use windows_sys::Wdk::{
        Foundation::OBJECT_ATTRIBUTES,
        Storage::FileSystem::{
            FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
            FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
        },
    };
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
    use windows_sys::Win32::{
        Foundation::*, Security::Authorization::*, Security::*, Storage::FileSystem::*,
    };

    const READ_CONTROL_ACCESS: u32 = 0x20000;
    const MAX_PATH_UNITS: usize = 32768;
    pub(crate) struct Security {
        pub user: Sid,
        pub trusted_installer: Option<Sid>,
    }
    // A6 diagnostic scope is read-only, thread-local and never changes old error semantics.
    // SDK and std::io errors are captured before the existing deliberately coarse mapping.
    #[cfg(not(test))]
    std::thread_local! {
        static REPAIR_DENIAL: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    }
    #[cfg(not(test))]
    pub(crate) enum RepairRead<T> {
        Present(T),
        Missing,
        AccessDenied,
        Unsafe,
        Unavailable,
        Unknown,
    }
    #[cfg(not(test))]
    struct RepairDiagnosticScope(Option<bool>);
    #[cfg(not(test))]
    impl Drop for RepairDiagnosticScope {
        fn drop(&mut self) {
            REPAIR_DENIAL.with(|scope| scope.set(self.0));
        }
    }
    #[cfg(not(test))]
    pub(crate) fn repair_capture_error(code: u32) {
        if code == ERROR_ACCESS_DENIED {
            REPAIR_DENIAL.with(|scope| {
                if scope.get().is_some() {
                    scope.set(Some(true));
                }
            });
        }
    }
    #[cfg(not(test))]
    pub(crate) fn repair_readonly_diagnostic<T>(
        read: impl FnOnce() -> NativeResult<Option<T>>,
    ) -> RepairRead<T> {
        let prior = REPAIR_DENIAL.with(|scope| scope.replace(Some(false)));
        let _scope = RepairDiagnosticScope(prior);
        // Nested scopes are not an authority or absence observation. RAII still restores the
        // exact previous scope, including unwind/early-return paths.
        if prior.is_some() {
            return RepairRead::Unknown;
        }
        let result = read();
        if REPAIR_DENIAL.with(|scope| scope.get()) == Some(true) {
            return RepairRead::AccessDenied;
        }
        match result {
            Ok(Some(value)) => RepairRead::Present(value),
            Ok(None) | Err(NativeError::Missing) => RepairRead::Missing,
            Err(NativeError::Invalid | NativeError::Foreign | NativeError::Oversize) => {
                RepairRead::Unsafe
            }
            Err(NativeError::Unavailable | NativeError::Unsupported) => RepairRead::Unavailable,
            Err(_) => RepairRead::Unknown,
        }
    }
    fn repair_read_error(error: std::io::Error) -> NativeError {
        #[cfg(not(test))]
        if let Some(code) = error.raw_os_error() {
            repair_capture_error(code as u32);
        }
        #[cfg(test)]
        let _ = error;
        // Existing callers retain exactly their old Unavailable mapping; no diagnostic scope
        // is installed except by the newly admitted read-only repair probe.
        NativeError::Unavailable
    }
    pub(crate) fn error(code: u32) -> NativeError {
        #[cfg(not(test))]
        repair_capture_error(code);
        match code {
            ERROR_FILE_NOT_FOUND => NativeError::Missing,
            ERROR_ACCESS_DENIED => NativeError::Foreign,
            ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION => NativeError::Busy,
            _ => NativeError::Unavailable,
        }
    }
    pub(crate) fn last_error() -> NativeError {
        // SAFETY: immediately captures the calling thread's last native error; no mutation.
        error(unsafe { GetLastError() })
    }
    pub(crate) fn wide(value: &str) -> NativeResult<Vec<u16>> {
        let mut units: Vec<u16> = value.encode_utf16().collect();
        if units.is_empty() || units.len() >= MAX_PATH_UNITS || units.contains(&0) {
            return Err(NativeError::Invalid);
        }
        units.push(0);
        Ok(units)
    }
    pub(crate) struct Descriptor(PSECURITY_DESCRIPTOR);
    impl Drop for Descriptor {
        fn drop(&mut self) {
            // SAFETY: exactly the allocation transferred by GetSecurityInfo/SDDL conversion.
            unsafe {
                LocalFree(self.0);
            }
        }
    }
    impl Descriptor {
        /// Agent windows/security.rs requires exactly one protected user ACE for this leaf.
        fn agent_lock(user: &Sid) -> NativeResult<Self> {
            let sddl = wide(&format!("O:{}D:P(A;;FA;;;{})", user.sddl(), user.sddl()))?;
            let mut descriptor = std::ptr::null_mut();
            // SAFETY: fixed user-only SDDL uses an admitted SID; the returned allocation is owned.
            if unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    1,
                    &mut descriptor,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                return Err(last_error());
            }
            if descriptor.is_null() {
                return Err(NativeError::Unavailable);
            }
            Ok(Self(descriptor))
        }
        fn private(user: &Sid) -> NativeResult<Self> {
            let sddl = wide(&format!(
                "O:{}D:P(A;OICI;FA;;;{})(A;OICI;FA;;;SY)",
                user.sddl(),
                user.sddl()
            ))?;
            let mut descriptor = std::ptr::null_mut();
            // SAFETY: fixed bounded SDDL containing only a validated SID; output is LocalFree-owned.
            if unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    1,
                    &mut descriptor,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                return Err(last_error());
            }
            if descriptor.is_null() {
                return Err(NativeError::Unavailable);
            }
            Ok(Self(descriptor))
        }
    }
    fn principal(pointer: PSID, security: &Security) -> NativeResult<Principal> {
        // SAFETY: pointer is a SID owned by the retained GetSecurityInfo descriptor or ACE.
        if pointer.is_null() || unsafe { IsValidSid(pointer) } == 0 {
            return Err(NativeError::Unavailable);
        }
        // SAFETY: native validity check above precedes the bounded length query.
        let length = unsafe { GetLengthSid(pointer) } as usize;
        if !(12..=68).contains(&length) {
            return Err(NativeError::Unavailable);
        }
        // SAFETY: valid native SID has exactly length readable bytes in the retained descriptor.
        let sid = Sid::from_bytes(
            unsafe { std::slice::from_raw_parts(pointer.cast(), length) }.to_vec(),
        )?;
        if sid == security.user {
            return Ok(Principal::User);
        }
        if security.trusted_installer.as_ref() == Some(&sid) {
            return Ok(Principal::TrustedInstaller);
        }
        match sid.sddl().as_str() {
            "S-1-5-18" => Ok(Principal::System),
            "S-1-5-32-544" => Ok(Principal::Administrators),
            _ => Ok(Principal::Foreign),
        }
    }
    fn acl(file: &File, security: &Security) -> NativeResult<AclFacts> {
        let mut owner = std::ptr::null_mut();
        let mut dacl = std::ptr::null_mut();
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: query-only retained file handle with READ_CONTROL; output allocation retained below.
        let code = unsafe {
            GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        if code != ERROR_SUCCESS {
            return Err(error(code));
        }
        let descriptor = Descriptor(descriptor);
        if descriptor.0.is_null() || dacl.is_null() {
            return Err(NativeError::Foreign);
        }
        let mut control = 0;
        let mut revision = 0;
        // SAFETY: successful GetSecurityInfo supplied a live complete security descriptor.
        if unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) } == 0 {
            return Err(last_error());
        }
        // SAFETY: DACL header is retained by descriptor; SDK owns its complete allocation.
        let count = unsafe { (*dacl).AceCount };
        if count > 1024 {
            return Err(NativeError::Foreign);
        }
        let mut entries = Vec::with_capacity(count as usize);
        for index in 0..u32::from(count) {
            let mut pointer = std::ptr::null_mut();
            // SAFETY: index is bounded by the native ACL's own count; no ACL mutation.
            if unsafe { GetAce(dacl, index, &mut pointer) } == 0 || pointer.is_null() {
                return Err(last_error());
            }
            // SAFETY: GetAce returned the complete SDK ACE; read its fixed header first.
            let header = unsafe { pointer.cast::<ACE_HEADER>().read_unaligned() };
            if header.AceSize < 8 {
                return Err(NativeError::Foreign);
            }
            if header.AceFlags & 8 != 0 {
                entries.push(Ace {
                    principal: Principal::Foreign,
                    mask: 0,
                    inherit_only: true,
                    supported: false,
                });
                continue;
            }
            // ACCESS_ALLOWED_ACE_TYPE=0; ACCESS_DENIED_ACE_TYPE=1. Other layouts are refused.
            if header.AceType > 1 {
                return Err(NativeError::Foreign);
            }
            if header.AceSize < 16 {
                return Err(NativeError::Foreign);
            }
            // SAFETY: complete 8-byte SID header fits after the checked standard ACE header.
            let sub_authorities = unsafe { pointer.cast::<u8>().add(9).read() } as usize;
            if sub_authorities > 15 || 16 + sub_authorities * 4 > usize::from(header.AceSize) {
                return Err(NativeError::Foreign);
            }
            // SAFETY: standard allowed/denied ACE has a 4-byte mask followed by its valid SID.
            let mask = unsafe { pointer.cast::<u8>().add(4).cast::<u32>().read_unaligned() };
            // SAFETY: allowed/denied ACE SID begins at SDK SidStart offset, retained by descriptor.
            let ace_principal = principal(unsafe { pointer.cast::<u8>().add(8) }.cast(), security)?;
            entries.push(Ace {
                principal: ace_principal,
                mask: if header.AceType == 0 { mask } else { 0 },
                inherit_only: header.AceFlags & 8 != 0,
                supported: true,
            });
        }
        Ok(AclFacts {
            owner: principal(owner, security)?,
            protected: control & SE_DACL_PROTECTED != 0,
            entries,
        })
    }
    fn info<T: Default>(file: &File, class: FILE_INFO_BY_HANDLE_CLASS) -> NativeResult<T> {
        let mut value = T::default();
        // SAFETY: caller pairs the public information class with its exact SDK structure.
        if unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                class,
                (&mut value as *mut T).cast(),
                std::mem::size_of::<T>() as u32,
            )
        } == 0
        {
            return Err(last_error());
        }
        Ok(value)
    }
    fn name(file: &File) -> NativeResult<String> {
        let mut buffer = vec![0u64; (4 + MAX_PATH_UNITS * 2).div_ceil(8)];
        // SAFETY: aligned owned buffer covers the maximum bounded FILE_NAME_INFO variable length.
        if unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileNameInfo,
                buffer.as_mut_ptr().cast(),
                (buffer.len() * 8) as u32,
            )
        } == 0
        {
            return Err(last_error());
        }
        // SAFETY: successful FILE_NAME_INFO begins with the fixed length field.
        let length = unsafe { buffer.as_ptr().cast::<u32>().read() } as usize;
        if !length.is_multiple_of(2) || length > MAX_PATH_UNITS * 2 {
            return Err(NativeError::Unavailable);
        }
        // SAFETY: checked byte length fits inside the live aligned output buffer after its header.
        let units = unsafe {
            std::slice::from_raw_parts(
                buffer.as_ptr().cast::<u8>().add(4).cast::<u16>(),
                length / 2,
            )
        };
        let full = String::from_utf16(units).map_err(|_| NativeError::Unavailable)?;
        Ok(full
            .rsplit('\\')
            .next()
            .ok_or(NativeError::Unavailable)?
            .into())
    }
    pub(crate) fn observe(
        file: &File,
        requested: &str,
        security: &Security,
    ) -> NativeResult<ComponentFacts> {
        // SAFETY: queries the retained file object type only; pipes/devices are never admitted.
        if unsafe { GetFileType(file.as_raw_handle()) } != FILE_TYPE_DISK {
            return Err(NativeError::Foreign);
        }
        let identity: FILE_ID_INFO = info(file, FileIdInfo)?;
        let tag: FILE_ATTRIBUTE_TAG_INFO = info(file, FileAttributeTagInfo)?;
        let standard: FILE_STANDARD_INFO = info(file, FileStandardInfo)?;
        let directory = standard.Directory;
        let case_sensitive = if directory {
            info::<FILE_CASE_SENSITIVE_INFO>(file, FileCaseSensitiveInfo)?.Flags != 0
        } else {
            false
        };
        Ok(ComponentFacts {
            requested: requested.into(),
            actual: name(file)?,
            identity: FileIdentity {
                volume: identity.VolumeSerialNumber,
                file: identity.FileId.Identifier,
            },
            directory,
            reparse: tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0,
            case_sensitive,
            links: standard.NumberOfLinks,
            acl: acl(file, security)?,
        })
    }
    fn open_volume_root(path: &str) -> NativeResult<File> {
        if path.len() != 3
            || path.as_bytes()[1..] != *b":\\"
            || !path.as_bytes()[0].is_ascii_uppercase()
        {
            return Err(NativeError::Invalid);
        }
        let value = wide(path)?;
        // SAFETY: read-only bootstrap of exactly a drive root, never a descendant or mutation.
        // Every following component is opened relative to this retained native file object.
        let raw = unsafe {
            CreateFileW(
                value.as_ptr(),
                READ_CONTROL_ACCESS | FILE_READ_ATTRIBUTES | FILE_LIST_DIRECTORY | FILE_TRAVERSE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Err(last_error());
        }
        // SAFETY: successful CreateFileW transferred one owned handle, closed exactly once by File.
        Ok(File::from(unsafe { OwnedHandle::from_raw_handle(raw) }))
    }
    struct NativeComponentIo<'a> {
        access: u32,
        sharing: u32,
        descriptor: Option<&'a Descriptor>,
    }
    impl RelativeIo for NativeComponentIo<'_> {
        type Parent = File;
        type Object = File;
        fn submit(&self, parent: Option<&File>, request: &ComponentRequest) -> NativeResult<File> {
            let parent = parent.ok_or(NativeError::Foreign)?;
            if !request.open_reparse_point
                || (!request.dont_reparse && (request.kind != ObjectKind::Opaque || request.create))
            {
                return Err(NativeError::Foreign);
            }
            let units: Vec<u16> = request.name.as_str().encode_utf16().collect();
            let length = u16::try_from(units.len().checked_mul(2).ok_or(NativeError::Invalid)?)
                .map_err(|_| NativeError::Invalid)?;
            let name = UNICODE_STRING {
                Length: length,
                MaximumLength: length,
                Buffer: units.as_ptr().cast_mut(),
            };
            let attributes = OBJECT_ATTRIBUTES {
                Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
                RootDirectory: parent.as_raw_handle(),
                ObjectName: &name,
                Attributes: if request.dont_reparse {
                    OBJ_DONT_REPARSE
                } else {
                    OBJ_CASE_INSENSITIVE
                },
                SecurityDescriptor: self
                    .descriptor
                    .map_or(std::ptr::null(), |value| value.0.cast()),
                SecurityQualityOfService: std::ptr::null(),
            };
            let mut raw = std::ptr::null_mut();
            let mut status_block = IO_STATUS_BLOCK::default();
            let kind = match request.kind {
                ObjectKind::Directory => FILE_DIRECTORY_FILE,
                ObjectKind::File => FILE_NON_DIRECTORY_FILE,
                ObjectKind::Opaque => 0,
            };
            // SAFETY: retained admitted parent handle; exactly one checked counted component;
            // public SDK structures/binding; strict ancestors/approved leaves remain exact-case.
            // Only an existing opaque final entry uses case-insensitive lookup plus OPEN_REPARSE_POINT;
            // it never follows its target. No inherit, privilege-bypass, overwrite or path fallback.
            let status = unsafe {
                NtCreateFile(
                    &mut raw,
                    self.access
                        | FILE_READ_ATTRIBUTES
                        | SYNCHRONIZE
                        | if request.kind == ObjectKind::Opaque {
                            0
                        } else {
                            READ_CONTROL_ACCESS
                        },
                    &attributes,
                    &mut status_block,
                    std::ptr::null(),
                    FILE_ATTRIBUTE_NORMAL,
                    self.sharing,
                    if request.create {
                        FILE_CREATE
                    } else {
                        FILE_OPEN
                    },
                    kind | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
                    std::ptr::null(),
                    0,
                )
            };
            if status != 0 {
                if status == STATUS_REPARSE_POINT_ENCOUNTERED {
                    return Err(NativeError::Foreign);
                }
                // SAFETY: NTSTATUS conversion is a public stateless SDK call, not a retry.
                return Err(error(unsafe { RtlNtStatusToDosError(status) }));
            }
            if raw.is_null() || raw == INVALID_HANDLE_VALUE {
                return Err(NativeError::OutcomeUnknown);
            }
            // SAFETY: successful NtCreateFile transferred exactly one owned file handle.
            let file = File::from(unsafe { OwnedHandle::from_raw_handle(raw) });
            // SAFETY: synchronous successful SDK call initialized the status union.
            if unsafe { status_block.Anonymous.Status } != 0
                || status_block.Information != if request.create { 2 } else { 1 }
            {
                return Err(NativeError::OutcomeUnknown);
            }
            Ok(file)
        }
    }
    fn open_component(
        parent: &File,
        name: &ComponentName,
        kind: ObjectKind,
        access: u32,
        sharing: u32,
        deadline: &Deadline,
    ) -> NativeResult<Option<File>> {
        relative_component(
            &NativeComponentIo {
                access,
                sharing,
                descriptor: None,
            },
            parent,
            name,
            kind,
            false,
            deadline,
        )
    }
    fn create_component(
        parent: &File,
        name: &ComponentName,
        kind: ObjectKind,
        descriptor: &Descriptor,
        deadline: &Deadline,
    ) -> NativeResult<File> {
        let access = match kind {
            ObjectKind::Directory => FILE_LIST_DIRECTORY | FILE_TRAVERSE,
            ObjectKind::File => GENERIC_READ | GENERIC_WRITE | super::DELETE,
            ObjectKind::Opaque => return Err(NativeError::Invalid),
        };
        relative_component(
            &NativeComponentIo {
                access,
                sharing: FILE_SHARE_READ,
                descriptor: Some(descriptor),
            },
            parent,
            name,
            kind,
            true,
            deadline,
        )?
        .ok_or(NativeError::OutcomeUnknown)
    }
    fn volume_path(file: &File) -> NativeResult<String> {
        let mut buffer = vec![0u16; MAX_PATH_UNITS];
        // SAFETY: live retained handle and complete bounded writable output; requests actual volume GUID.
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                VOLUME_NAME_GUID,
            )
        } as usize;
        if length == 0 || length >= buffer.len() {
            return Err(last_error());
        }
        let path = String::from_utf16(&buffer[..length]).map_err(|_| NativeError::Unavailable)?;
        if !path.starts_with("\\\\?\\Volume{") {
            return Err(NativeError::Foreign);
        }
        Ok(path)
    }
    fn require_ntfs(root: &File) -> NativeResult<()> {
        let mut filesystem = [0u16; 32];
        // SAFETY: query-only retained root directory; no volume settings change.
        if unsafe {
            GetVolumeInformationByHandleW(
                root.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                filesystem.as_mut_ptr(),
                filesystem.len() as u32,
            )
        } == 0
        {
            return Err(last_error());
        }
        if filesystem[..5] != [78, 84, 70, 83, 0] {
            return Err(NativeError::Unsupported);
        }
        Ok(())
    }
    #[derive(Clone)]
    struct Pin {
        file: Arc<File>,
        facts: ComponentFacts,
    }
    /// Non-deleting shares retain every ancestor through dispatch; drive aliases never become roots.
    #[derive(Clone)]
    pub(crate) struct Anchor {
        path: String,
        pins: Vec<Pin>,
    }
    impl Anchor {
        pub(crate) fn open(
            path: &str,
            security: &Security,
            private: bool,
            deadline: &Deadline,
        ) -> NativeResult<Option<Self>> {
            let bytes = path.as_bytes();
            if bytes.len() < 3 || bytes[1..3] != *b":\\" || !bytes[0].is_ascii_uppercase() {
                return Err(NativeError::Invalid);
            }
            let components: Vec<&str> = path[3..].split('\\').filter(|n| !n.is_empty()).collect();
            if components.len() > 64
                || components.iter().any(|n| {
                    *n == "."
                        || *n == ".."
                        || n.contains([':', '/', '\0'])
                        || n.ends_with(['.', ' '])
                })
            {
                return Err(NativeError::Foreign);
            }
            deadline.check()?;
            let root = open_volume_root(&path[..3])?;
            require_ntfs(&root)?;
            let root_facts = observe(&root, "", security)?;
            admit_component(&root_facts, Admission::Ancestor)?;
            let volume = root_facts.identity.volume;
            volume_path(&root)?;
            let mut pins = vec![Pin {
                file: Arc::new(root),
                facts: root_facts,
            }];
            for (index, component) in components.iter().enumerate() {
                deadline.check()?;
                let parent = pins.last().ok_or(NativeError::Foreign)?;
                let file = match open_component(
                    &parent.file,
                    &ComponentName::new(component)?,
                    ObjectKind::Directory,
                    FILE_LIST_DIRECTORY | FILE_TRAVERSE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    deadline,
                )? {
                    Some(file) => file,
                    None => return Ok(None),
                };
                let facts = observe(&file, component, security)?;
                admit_component(
                    &facts,
                    if private && index + 1 == components.len() {
                        Admission::PrivateDirectory
                    } else {
                        Admission::Ancestor
                    },
                )?;
                if facts.identity.volume != volume {
                    return Err(NativeError::Foreign);
                }
                pins.push(Pin {
                    file: Arc::new(file),
                    facts,
                });
            }
            Ok(Some(Self {
                path: path.into(),
                pins,
            }))
        }
        pub(crate) fn revalidate(
            &self,
            security: &Security,
            private: bool,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            let fresh =
                Self::open(&self.path, security, private, deadline)?.ok_or(NativeError::Foreign)?;
            if fresh.pins.len() != self.pins.len() {
                return Err(NativeError::Foreign);
            }
            for (old, new) in self.pins.iter().zip(&fresh.pins) {
                if old.facts.identity != new.facts.identity {
                    return Err(NativeError::Foreign);
                }
            }
            Ok(())
        }
        pub(crate) fn file(&self) -> NativeResult<&File> {
            self.pins
                .last()
                .map(|p| p.file.as_ref())
                .ok_or(NativeError::Foreign)
        }
        pub(crate) fn identity(&self) -> NativeResult<FileIdentity> {
            self.pins
                .last()
                .map(|p| p.facts.identity)
                .ok_or(NativeError::Foreign)
        }
        /// DOS canonical spelling from the retained admitted directory, never a JSON path.
        pub(crate) fn canonical_dos_path(
            &self,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<std::ffi::OsString> {
            use std::os::windows::ffi::OsStringExt;
            self.revalidate(security, false, deadline)?;
            let mut value = vec![0u16; MAX_PATH_UNITS];
            // SAFETY: retained directory handle and complete bounded writable UTF-16 output.
            let length = unsafe {
                GetFinalPathNameByHandleW(
                    self.file()?.as_raw_handle(),
                    value.as_mut_ptr(),
                    value.len() as u32,
                    VOLUME_NAME_DOS,
                )
            } as usize;
            if length == 0 || length >= value.len() {
                return Err(NativeError::Unavailable);
            }
            deadline.check()?;
            Ok(std::ffi::OsString::from_wide(&value[..length]))
        }
        /// Metadata-only pinned fixed leaf. No executable bytes, write sharing or delete sharing.
        pub(crate) fn open_file_metadata(
            &self,
            name: &PrivateName,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<(File, FileIdentity)> {
            self.revalidate(security, false, deadline)?;
            let file = open_component(
                self.file()?,
                &ComponentName::new(name.as_str())?,
                ObjectKind::File,
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ,
                deadline,
            )?
            .ok_or(NativeError::Missing)?;
            let facts = observe(&file, name.as_str(), security)?;
            admit_component(&facts, Admission::PrivateFile)?;
            if facts.identity.volume != self.identity()?.volume {
                return Err(NativeError::Foreign);
            }
            deadline.check()?;
            Ok((file, facts.identity))
        }
        #[cfg(test)]
        pub(crate) fn fixture_path(&self) -> &str {
            &self.path
        }
        pub(crate) fn create_child_directory(
            &self,
            name: &str,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<Self> {
            deadline.check()?;
            self.revalidate(security, false, deadline)?;
            if !matches!(name, "Crosspane" | "Installer" | "Programs") {
                PrivateName::new(name)?;
            }
            let requested = self
                .pins
                .last()
                .map(|pin| pin.facts.requested.as_str())
                .ok_or(NativeError::Foreign)?;
            if observe(self.file()?, requested, security)?.acl.owner != Principal::User {
                return Err(NativeError::Foreign);
            }
            let descriptor = Descriptor::private(&security.user)?;
            deadline.check()?;
            let file = create_component(
                self.file()?,
                &ComponentName::new(name)?,
                ObjectKind::Directory,
                &descriptor,
                deadline,
            )?;
            let facts = observe(&file, name, security)?;
            admit_component(&facts, Admission::PrivateDirectory)?;
            if facts.identity.volume != self.identity()?.volume {
                return Err(NativeError::OutcomeUnknown);
            }
            let mut child = self.clone();
            child.path = format!("{}\\{name}", self.path.trim_end_matches('\\'));
            child.pins.push(Pin {
                file: Arc::new(file),
                facts,
            });
            Ok(child)
        }
        pub(crate) fn create_private(
            &self,
            name: &PrivateName,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<File> {
            self.revalidate(security, true, deadline)?;
            deadline.check()?;
            let descriptor = Descriptor::private(&security.user)?;
            deadline.check()?;
            let file = create_component(
                self.file()?,
                &ComponentName::new(name.as_str())?,
                ObjectKind::File,
                &descriptor,
                deadline,
            )?;
            let facts = observe(&file, name.as_str(), security)?;
            admit_component(&facts, Admission::PrivateFile)?;
            if facts.identity.volume != self.identity()?.volume {
                return Err(NativeError::OutcomeUnknown);
            }
            Ok(file)
        }
        /// Literal state-root agent.lock only. Its writable std File is locked with the same
        /// File::try_lock used by the agent; no serialized owner/PID contents are read.
        #[cfg_attr(test, allow(dead_code))]
        pub(crate) fn first_install_agent_lock(
            &self,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<File> {
            self.revalidate(security, true, deadline)?;
            let name = ComponentName::new("agent.lock")?;
            let existing = open_component(
                self.file()?,
                &name,
                ObjectKind::File,
                GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                deadline,
            )?;
            let file = match existing {
                Some(file) => file,
                None => {
                    let descriptor = Descriptor::agent_lock(&security.user)?;
                    relative_component(
                        &NativeComponentIo {
                            access: GENERIC_WRITE,
                            sharing: FILE_SHARE_READ | FILE_SHARE_WRITE,
                            descriptor: Some(&descriptor),
                        },
                        self.file()?,
                        &name,
                        ObjectKind::File,
                        true,
                        deadline,
                    )?
                    .ok_or(NativeError::OutcomeUnknown)?
                }
            };
            let facts = observe(&file, "agent.lock", security)?;
            admit_component(&facts, Admission::PrivateFile)?;
            // Do not repair an existing DACL. Match the agent's one-user full-access contract.
            if facts.identity.volume != self.identity()?.volume
                || facts.acl.entries.len() != 1
                || facts.acl.entries[0].principal != Principal::User
                || facts.acl.entries[0].mask != 0x001f01ff
                || facts.acl.entries[0].inherit_only
            {
                return Err(NativeError::Foreign);
            }
            let mut dacl = std::ptr::null_mut();
            let mut descriptor = std::ptr::null_mut();
            // SAFETY: query-only retained admitted agent.lock with READ_CONTROL. The owned
            // descriptor below retains the DACL and ACE for this exact file, never a path reopen.
            let code = unsafe {
                GetSecurityInfo(
                    file.as_raw_handle(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut dacl,
                    std::ptr::null_mut(),
                    &mut descriptor,
                )
            };
            if code != ERROR_SUCCESS {
                return Err(error(code));
            }
            let descriptor = Descriptor(descriptor);
            if descriptor.0.is_null() || dacl.is_null() {
                return Err(NativeError::Foreign);
            }
            // SAFETY: successful GetSecurityInfo owns the live ACL header through descriptor.
            if unsafe { (*dacl).AceCount } != 1 {
                return Err(NativeError::Foreign);
            }
            let mut pointer = std::ptr::null_mut();
            // SAFETY: the same retained DACL has exactly one ACE; this queries index zero only.
            if unsafe { GetAce(dacl, 0, &mut pointer) } == 0 || pointer.is_null() {
                return Err(last_error());
            }
            // SAFETY: successful GetAce yields a complete ACE header retained by descriptor.
            let header = unsafe { pointer.cast::<ACE_HEADER>().read_unaligned() };
            if header.AceSize < std::mem::size_of::<ACE_HEADER>() as u16 || header.AceFlags != 0 {
                return Err(NativeError::Foreign);
            }
            deadline.check()?;
            Ok(file)
        }
        pub(crate) fn read_private(
            &self,
            name: &PrivateName,
            security: &Security,
            cap: usize,
            deadline: &Deadline,
        ) -> NativeResult<Option<(FileIdentity, Vec<u8>)>> {
            use std::io::Read;
            self.revalidate(security, true, deadline)?;
            check_read_size(0, cap)?;
            let mut file = match open_component(
                self.file()?,
                &ComponentName::new(name.as_str())?,
                ObjectKind::File,
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_DELETE,
                deadline,
            )? {
                Some(file) => file,
                None => return Ok(None),
            };
            let facts = observe(&file, name.as_str(), security)?;
            admit_component(&facts, Admission::PrivateFile)?;
            if facts.identity.volume != self.identity()?.volume {
                return Err(NativeError::Foreign);
            }
            let standard: FILE_STANDARD_INFO = info(&file, FileStandardInfo)?;
            let length = usize::try_from(standard.EndOfFile).map_err(|_| NativeError::Oversize)?;
            check_read_size(length, cap)?;
            deadline.check()?;
            let mut bytes = Vec::with_capacity(length.min(cap));
            file.by_ref()
                .take((cap + 1) as u64)
                .read_to_end(&mut bytes)
                .map_err(repair_read_error)?;
            check_read_size(bytes.len(), cap)?;
            deadline.check()?;
            let after = observe(&file, name.as_str(), security)?;
            admit_component(&after, Admission::PrivateFile)?;
            if after.identity != facts.identity {
                return Err(NativeError::Foreign);
            }
            Ok(Some((facts.identity, bytes)))
        }
        pub(crate) fn open_lock(
            &self,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<Option<File>> {
            self.revalidate(security, true, deadline)?;
            let file = open_component(
                self.file()?,
                &ComponentName::new("install.lock")?,
                ObjectKind::File,
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                deadline,
            )?;
            if let Some(file) = &file {
                let facts = observe(file, "install.lock", security)?;
                admit_component(&facts, Admission::PrivateFile)?;
                if facts.identity.volume != self.identity()?.volume {
                    return Err(NativeError::Foreign);
                }
            }
            Ok(file)
        }
        pub(crate) fn create_lock(
            &self,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<File> {
            self.revalidate(security, true, deadline)?;
            let descriptor = Descriptor::private(&security.user)?;
            let name = ComponentName::new("install.lock")?;
            let file = relative_component(
                &NativeComponentIo {
                    access: GENERIC_READ | GENERIC_WRITE,
                    sharing: FILE_SHARE_READ | FILE_SHARE_WRITE,
                    descriptor: Some(&descriptor),
                },
                self.file()?,
                &name,
                ObjectKind::File,
                true,
                deadline,
            )?
            .ok_or(NativeError::OutcomeUnknown)?;
            let facts = observe(&file, "install.lock", security)?;
            admit_component(&facts, Admission::PrivateFile)?;
            if facts.identity.volume != self.identity()?.volume {
                return Err(NativeError::OutcomeUnknown);
            }
            Ok(file)
        }
        pub(crate) fn publish_private(
            &self,
            source: &File,
            name: &PrivateName,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.revalidate(security, true, deadline)?;
            let identity: FILE_ID_INFO = info(source, FileIdInfo)?;
            if identity.VolumeSerialNumber != self.identity()?.volume {
                return Err(NativeError::Foreign);
            }
            let units: Vec<u16> = name.as_str().encode_utf16().collect();
            let offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
            let length = offset
                .checked_add(units.len().checked_mul(2).ok_or(NativeError::Invalid)?)
                .ok_or(NativeError::Invalid)?;
            let mut buffer = vec![
                0u64;
                length
                    .max(std::mem::size_of::<FILE_RENAME_INFO>())
                    .div_ceil(8)
            ];
            let pointer = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
            // SAFETY: aligned allocation covers the SDK fixed header and complete variable name;
            // relative single component is bound to our retained private directory handle.
            unsafe {
                (*pointer).Anonymous.ReplaceIfExists = true;
                (*pointer).RootDirectory = self.file()?.as_raw_handle();
                (*pointer).FileNameLength = (units.len() * 2) as u32;
                std::ptr::copy_nonoverlapping(
                    units.as_ptr(),
                    buffer.as_mut_ptr().cast::<u8>().add(offset).cast::<u16>(),
                    units.len(),
                );
            }
            deadline.check()?;
            // SAFETY: retained own source has DELETE access; one same-volume rename with no copy
            // fallback, target paths/streams excluded. Errors are inspected rather than retried.
            if unsafe {
                SetFileInformationByHandle(
                    source.as_raw_handle(),
                    FileRenameInfo,
                    pointer.cast(),
                    length as u32,
                )
            } == 0
            {
                return Err(last_error());
            }
            deadline.check()?;
            Ok(())
        }
    }

    /// Raw observed PE facts are not approval. The caller supplies the embedded expected pin.
    pub(crate) struct ImageData {
        pub file: Arc<File>,
        pub identity: FileIdentity,
        pub canonical: String,
        pub facts: super::super::super::payload::inventory::PeFacts,
    }
    pub(crate) struct OpaqueData {
        file: File,
        pub identity: FileIdentity,
        name: String,
    }
    fn raw_identity(file: &File) -> NativeResult<FileIdentity> {
        // SAFETY: retained native leaf handle; query only, no device/pipe accepted.
        if unsafe { GetFileType(file.as_raw_handle()) } != FILE_TYPE_DISK {
            return Err(NativeError::Foreign);
        }
        let id: FILE_ID_INFO = info(file, FileIdInfo)?;
        let value = FileIdentity {
            volume: id.VolumeSerialNumber,
            file: id.FileId.Identifier,
        };
        if value.volume == 0 || value.file == [0; 16] {
            return Err(NativeError::Foreign);
        }
        Ok(value)
    }
    fn canonical_file(file: &File, deadline: &Deadline) -> NativeResult<String> {
        let mut units = vec![0u16; MAX_PATH_UNITS];
        // SAFETY: query of exactly the retained file, complete bounded UTF-16 output.
        let len = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                units.as_mut_ptr(),
                units.len() as u32,
                VOLUME_NAME_DOS,
            )
        } as usize;
        if len == 0 || len >= units.len() {
            return Err(last_error());
        }
        deadline.check()?;
        String::from_utf16(&units[..len]).map_err(|_| NativeError::Unavailable)
    }
    pub(crate) fn measure_image(
        file: Arc<File>,
        version: &str,
        deadline: &Deadline,
    ) -> NativeResult<ImageData> {
        use super::super::super::payload::inventory::{MAX_IMAGE_BYTES, PeFacts, pe_header};
        use aws_lc_rs::digest::{Context, SHA256};
        use std::os::windows::fs::FileExt;
        let before = raw_identity(&file)?;
        let standard: FILE_STANDARD_INFO = info(&file, FileStandardInfo)?;
        let size = u64::try_from(standard.EndOfFile).map_err(|_| NativeError::Oversize)?;
        if standard.Directory || !(128..=MAX_IMAGE_BYTES).contains(&size) {
            return Err(NativeError::Unsupported);
        }
        let mut digest = Context::new(&SHA256);
        let mut prefix = Vec::new();
        let mut block = vec![0; 64 * 1024];
        let mut offset = 0;
        while offset < size {
            deadline.check()?;
            let requested = usize::try_from((size - offset).min(block.len() as u64))
                .map_err(|_| NativeError::Oversize)?;
            let count = file
                .seek_read(&mut block[..requested], offset)
                .map_err(repair_read_error)?;
            if count == 0 {
                return Err(NativeError::Unavailable);
            }
            digest.update(&block[..count]);
            let kept = count.min((1024 * 1024usize).saturating_sub(prefix.len()));
            prefix.extend_from_slice(&block[..kept]);
            offset += count as u64;
        }
        let after: FILE_STANDARD_INFO = info(&file, FileStandardInfo)?;
        if raw_identity(&file)? != before || after.EndOfFile != standard.EndOfFile {
            return Err(NativeError::Foreign);
        }
        let (machine, subsystem) = pe_header(&prefix, size)?;
        let mut sha256 = [0; 32];
        sha256.copy_from_slice(digest.finish().as_ref());
        let facts = PeFacts {
            size,
            sha256,
            machine,
            subsystem,
            version: version.into(),
        };
        deadline.check()?;
        Ok(ImageData {
            canonical: canonical_file(&file, deadline)?,
            file,
            identity: before,
            facts,
        })
    }
    impl Anchor {
        pub(crate) fn child(
            &self,
            child: &str,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<Option<Self>> {
            self.revalidate(security, false, deadline)?;
            let name = ComponentName::new(child)?;
            let Some(file) = open_component(
                self.file()?,
                &name,
                ObjectKind::Directory,
                FILE_LIST_DIRECTORY | FILE_TRAVERSE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                deadline,
            )?
            else {
                return Ok(None);
            };
            let facts = observe(&file, child, security)?;
            admit_component(&facts, Admission::PrivateDirectory)?;
            if facts.identity.volume != self.identity()?.volume {
                return Err(NativeError::Foreign);
            }
            let mut value = self.clone();
            value.path = format!("{}\\{child}", self.path.trim_end_matches('\\'));
            value.pins.push(Pin {
                file: Arc::new(file),
                facts,
            });
            Ok(Some(value))
        }
        pub(crate) fn open_image(
            &self,
            leaf: &str,
            private: bool,
            version: &str,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<ImageData> {
            self.open_image_access(leaf, private, false, version, security, deadline)
        }
        pub(crate) fn open_staged_image(
            &self,
            leaf: &str,
            version: &str,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<ImageData> {
            self.open_image_access(leaf, true, true, version, security, deadline)
        }
        fn open_image_access(
            &self,
            leaf: &str,
            private: bool,
            delete: bool,
            version: &str,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<ImageData> {
            self.revalidate(security, false, deadline)?;
            let file = open_component(
                self.file()?,
                &ComponentName::new(leaf)?,
                ObjectKind::File,
                GENERIC_READ
                    | if delete {
                        super::DELETE | GENERIC_WRITE
                    } else {
                        0
                    },
                FILE_SHARE_READ,
                deadline,
            )?
            .ok_or(NativeError::Missing)?;
            let facts = observe(&file, leaf, security)?;
            if private {
                admit_component(&facts, Admission::PrivateFile)?;
            } else if facts.reparse || facts.directory || facts.actual != leaf || facts.links != 1 {
                return Err(NativeError::Foreign);
            }
            if facts.identity.volume != self.identity()?.volume {
                return Err(NativeError::Foreign);
            }
            measure_image(Arc::new(file), version, deadline)
        }
        /// Opens only the opaque entry below an admitted parent; OPEN_REPARSE_POINT bypasses
        /// processing of THIS leaf. No directory traversal or approval of its ACL/contents occurs.
        pub(crate) fn opaque(
            &self,
            leaf: &str,
            exclusive: bool,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<Option<OpaqueData>> {
            self.revalidate(security, false, deadline)?;
            let request = ComponentRequest {
                name: ComponentName::new(leaf)?,
                kind: ObjectKind::Opaque,
                create: false,
                dont_reparse: false,
                open_reparse_point: true,
            };
            deadline.check()?;
            let file = match (NativeComponentIo {
                access: if exclusive { super::DELETE } else { 0 },
                sharing: if exclusive {
                    0
                } else {
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
                },
                descriptor: None,
            })
            .submit(Some(self.file()?), &request)
            {
                Ok(file) => file,
                Err(NativeError::Missing) => return Ok(None),
                Err(error) => return Err(error),
            };
            // NT's opaque-only single-component lookup chose this entry. Preserve its actual
            // spelling; differently cased stale leaves at a case-insensitive fixed path are backups.
            let actual = name(&file)?;
            ComponentName::new(&actual)?;
            let identity = raw_identity(&file)?;
            if identity.volume != self.identity()?.volume {
                return Err(NativeError::Foreign);
            }
            deadline.check()?;
            Ok(Some(OpaqueData {
                file,
                identity,
                name: actual,
            }))
        }
        pub(crate) fn stage_image(
            &self,
            leaf: &str,
            mut input: Box<dyn std::io::Read + Send>,
            expected: &super::super::super::payload::inventory::ApprovedPe,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<ImageData> {
            use std::io::Write;
            let mut file = self.create_private(&PrivateName::new(leaf)?, security, deadline)?;
            let mut block = vec![0; 64 * 1024];
            let mut count = 0u64;
            loop {
                deadline.check()?;
                let remaining = expected.size().saturating_sub(count);
                // Probe one extra byte at the exact limit without allocating the entire executable.
                let limit = usize::try_from(remaining.min(block.len() as u64).max(1))
                    .map_err(|_| NativeError::Oversize)?;
                let read = input
                    .read(&mut block[..limit])
                    .map_err(|_| NativeError::OutcomeUnknown)?;
                if read == 0 {
                    break;
                }
                count = count
                    .checked_add(read as u64)
                    .ok_or(NativeError::Oversize)?;
                if count > expected.size() {
                    return Err(NativeError::Oversize);
                }
                deadline.check()?;
                file.write_all(&block[..read])
                    .map_err(|_| NativeError::OutcomeUnknown)?;
            }
            if count != expected.size() {
                return Err(NativeError::Unsupported);
            }
            deadline.check()?;
            // SAFETY: retained own CREATE_NEW writable stage; no replace/POSIX bypass flags.
            if unsafe { FlushFileBuffers(file.as_raw_handle()) } == 0 {
                return Err(last_error());
            }
            let image = measure_image(Arc::new(file), expected.version(), deadline)?;
            if image.facts != *expected.facts() {
                return Err(NativeError::Unsupported);
            }
            Ok(image)
        }
        pub(crate) fn move_opaque(
            &self,
            mut source: OpaqueData,
            destination: &Anchor,
            leaf: &str,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<FileIdentity> {
            self.revalidate(security, false, deadline)?;
            destination.revalidate(security, true, deadline)?;
            let expected = source.identity;
            // Settle the observation pin before acquiring positive exclusive DELETE admission.
            let old_name = source.name.clone();
            drop(source.file);
            source = self
                .opaque(&old_name, true, security, deadline)?
                .ok_or(NativeError::Foreign)?;
            if source.identity != expected {
                return Err(NativeError::Foreign);
            }
            rename_no_replace(&source.file, destination, leaf, deadline)?;
            if raw_identity(&source.file)? != expected || name(&source.file)? != leaf {
                return Err(NativeError::OutcomeUnknown);
            }
            Ok(expected)
        }
        pub(crate) fn publish_image(
            &self,
            image: &ImageData,
            destination: &Anchor,
            leaf: &str,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.revalidate(security, true, deadline)?;
            destination.revalidate(security, true, deadline)?;
            if raw_identity(&image.file)? != image.identity {
                return Err(NativeError::Foreign);
            }
            rename_no_replace(&image.file, destination, leaf, deadline)?;
            if name(&image.file)? != leaf || raw_identity(&image.file)? != image.identity {
                return Err(NativeError::OutcomeUnknown);
            }
            Ok(())
        }
        pub(crate) fn entry_names(
            &self,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<Vec<String>> {
            self.revalidate(security, true, deadline)?;
            entries(self.file()?, deadline)
        }
        /// Delete ONLY the selected fixed supervisor archive record, on its freshly admitted
        /// exact DELETE handle. The caller already persisted ArchiveIntent under the real lock.
        // Only the new native epoch adapter consumes this; production entry is cfg(not(test)).
        #[cfg_attr(test, allow(dead_code, unused_imports))]
        pub(crate) fn delete_private_record(
            &self,
            name: &PrivateName,
            expected: FileIdentity,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            if !matches!(
                name.as_str(),
                "supervisor-epoch-0.json" | "supervisor-epoch-1.json" | "supervisor-epoch-2.json"
            ) {
                return Err(NativeError::Foreign);
            }
            self.revalidate(security, true, deadline)?;
            // Strict File admission adds READ_CONTROL_ACCESS for the owner/DACL query,
            // unlike the deliberately opaque old-leaf adapter. Generic opaque behavior stays.
            let source = open_component(
                self.file()?,
                &ComponentName::new(name.as_str())?,
                ObjectKind::File,
                super::DELETE,
                0,
                deadline,
            )?
            .ok_or(NativeError::Foreign)?;
            let facts = observe(&source, name.as_str(), security)?;
            super::admit_component(&facts, super::Admission::PrivateFile)?;
            if facts.identity != expected {
                return Err(NativeError::Foreign);
            }
            let mut disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
            deadline.check()?;
            // SAFETY: this is the exact private non-reparse record with expected FileId, opened
            // relative with DELETE/no-follow. No name reopening or target-following for the effect.
            if unsafe {
                SetFileInformationByHandle(
                    source.as_raw_handle(),
                    FileDispositionInfo,
                    (&mut disposition as *mut FILE_DISPOSITION_INFO).cast(),
                    std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
                )
            } == 0
            {
                return Err(last_error());
            }
            // DeletePending is NOT completion: settle this exact handle before positive absence.
            drop(source);
            deadline.check()?;
            match self.opaque(name.as_str(), false, security, deadline)? {
                None => Ok(()),
                Some(_) => Err(NativeError::OutcomeUnknown),
            }
        }
        /// A4e literal Outer pointer retirement after timely OuterRetireIntent under the
        /// actual installer lock. Exact file identity and same-handle DELETE only; no arbitrary leaf.
        #[cfg(not(test))]
        pub(crate) fn delete_recovered_outer_record(
            &self,
            expected: FileIdentity,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            const LEAF: &str = "outer-upgrade.json";
            self.revalidate(security, true, deadline)?;
            let source = open_component(
                self.file()?,
                &ComponentName::new(LEAF)?,
                ObjectKind::File,
                super::DELETE,
                0,
                deadline,
            )?
            .ok_or(NativeError::Foreign)?;
            let facts = observe(&source, LEAF, security)?;
            super::admit_component(&facts, super::Admission::PrivateFile)?;
            if facts.identity != expected {
                return Err(NativeError::Foreign);
            }
            let mut disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
            deadline.check()?;
            // SAFETY: exact admitted private non-reparse single-link record with expected FileId;
            // opened relative with DELETE/no-follow. The effect never reopens a claimed name.
            if unsafe {
                SetFileInformationByHandle(
                    source.as_raw_handle(),
                    FileDispositionInfo,
                    (&mut disposition as *mut FILE_DISPOSITION_INFO).cast(),
                    std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
                )
            } == 0
            {
                return Err(last_error());
            }
            // DeletePending is not completion; close the actual source before observing absence.
            drop(source);
            deadline.check()?;
            match self.opaque(LEAF, false, security, deadline)? {
                None => Ok(()),
                Some(_) => Err(NativeError::OutcomeUnknown),
            }
        }
        /// A4d exact-identity fixed lifecycle copy under the actual installer lock and admitted
        /// durable cleanup intent. Caller renews its live exclusive namespace plus strict terminal
        /// selection, optionally also its positive retained-peer exit; FS-only cleanup grants no
        /// old tree/process completion or start authority.
        #[cfg(not(test))]
        pub(crate) fn delete_keeper_copy(
            &self,
            expected: FileIdentity,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            const LEAF: &str = "keeper-copy.exe";
            self.revalidate(security, true, deadline)?;
            let source = open_component(
                self.file()?,
                &ComponentName::new(LEAF)?,
                ObjectKind::File,
                super::DELETE,
                0,
                deadline,
            )?
            .ok_or(NativeError::Foreign)?;
            let facts = observe(&source, LEAF, security)?;
            super::admit_component(&facts, super::Admission::PrivateFile)?;
            if facts.identity != expected {
                return Err(NativeError::Foreign);
            }
            let mut disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
            deadline.check()?;
            // SAFETY: strict private/no-follow fixed leaf; exact FileId checked on THIS exclusive
            // DELETE handle. No POSIX flags, overwrite, path reopen or loaded-image bypass.
            if unsafe {
                SetFileInformationByHandle(
                    source.as_raw_handle(),
                    FileDispositionInfo,
                    (&mut disposition as *mut FILE_DISPOSITION_INFO).cast(),
                    std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
                )
            } == 0
            {
                return Err(last_error());
            }
            drop(source);
            deadline.check()?;
            match self.opaque(LEAF, false, security, deadline)? {
                None => Ok(()),
                Some(_) => Err(NativeError::OutcomeUnknown),
            }
        }
        /// Bounded no-follow deletion, only below a completed fixed backup-generation handle.
        pub(crate) fn prune_tree(
            &self,
            leaf: &str,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.revalidate(security, true, deadline)?;
            let request = ComponentRequest {
                name: ComponentName::new(leaf)?,
                kind: ObjectKind::Opaque,
                create: false,
                dont_reparse: false,
                open_reparse_point: true,
            };
            let file = match (NativeComponentIo {
                access: super::DELETE | FILE_LIST_DIRECTORY,
                sharing: 0,
                descriptor: None,
            })
            .submit(Some(self.file()?), &request)
            {
                Ok(file) => file,
                Err(NativeError::Missing) => return Ok(()),
                Err(error) => return Err(error),
            };
            let identity = raw_identity(&file)?;
            if identity.volume != self.identity()?.volume {
                return Err(NativeError::Foreign);
            }
            let root = OpaqueData {
                file,
                identity,
                name: leaf.into(),
            };
            let mut budget = 1024usize;
            prune_leaf(root, deadline, 0, &mut budget)?;
            match self.opaque(leaf, false, security, deadline)? {
                None => Ok(()),
                Some(_) => Err(NativeError::OutcomeUnknown),
            }
        }
    }
    // A6: fixed metadata-only effects. The caller supplies a genuine original-context lock
    // and durable protocol intent; these helpers cannot select another leaf or executable.
    #[cfg(not(test))]
    impl Anchor {
        pub(crate) fn write_repair_publication_intent(
            &self,
            previous: Option<(FileIdentity, &[u8])>,
            bytes: &[u8],
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<FileIdentity> {
            use std::io::Write;
            let name = PrivateName::new("repair-publication-intent.json")?;
            self.revalidate(security, true, deadline)?;
            check_read_size(bytes.len(), MAX_RECORD_BYTES)?;
            let mut file = match previous {
                Some((id, old)) => {
                    let mut file = open_component(
                        self.file()?,
                        &ComponentName::new(name.as_str())?,
                        ObjectKind::File,
                        GENERIC_READ | GENERIC_WRITE,
                        0,
                        deadline,
                    )?
                    .ok_or(NativeError::Foreign)?;
                    let facts = observe(&file, name.as_str(), security)?;
                    admit_component(&facts, Admission::PrivateFile)?;
                    if facts.identity != id || read_repair_bytes(&mut file, deadline)? != old {
                        return Err(NativeError::Foreign);
                    }
                    file
                }
                None => self.create_private(&name, security, deadline)?,
            };
            let facts = observe(&file, name.as_str(), security)?;
            admit_component(&facts, Admission::PrivateFile)?;
            deadline.check()?;
            // Fixed intent is deliberately not published via a random temporary. A torn write
            // leaves a strictly malformed/Unknown fixed intent and can never be salvaged as permission.
            file.set_len(0).map_err(|_| NativeError::OutcomeUnknown)?;
            std::io::Seek::rewind(&mut file).map_err(|_| NativeError::OutcomeUnknown)?;
            file.write_all(bytes)
                .map_err(|_| NativeError::OutcomeUnknown)?;
            deadline.check()?;
            // SAFETY: exact retained private writable intent; ordinary flush, no path fallback.
            if unsafe { FlushFileBuffers(file.as_raw_handle()) } == 0 {
                return Err(last_error());
            }
            let id = facts.identity;
            drop(file);
            let (actual, observed) = self
                .read_private(&name, security, MAX_RECORD_BYTES, deadline)?
                .ok_or(NativeError::OutcomeUnknown)?;
            if actual != id || observed != bytes {
                return Err(NativeError::OutcomeUnknown);
            }
            deadline.check()?;
            Ok(id)
        }
        pub(crate) fn create_repair_pending(
            &self,
            bytes: &[u8],
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<FileIdentity> {
            use std::io::Write;
            check_read_size(bytes.len(), MAX_RECORD_BYTES)?;
            let name = PrivateName::new("repair-pending.json")?;
            // This raw fixed pending file contains the selected target envelope, NOT an
            // independently trusted RepairPending body. Only the intent's exact target/stamp admits it.
            let mut file = self.create_private(&name, security, deadline)?;
            deadline.check()?;
            file.write_all(bytes)
                .map_err(|_| NativeError::OutcomeUnknown)?;
            deadline.check()?;
            // SAFETY: our one CREATE_NEW complete bounded pending record, retained through flush.
            if unsafe { FlushFileBuffers(file.as_raw_handle()) } == 0 {
                return Err(last_error());
            }
            let facts = observe(&file, name.as_str(), security)?;
            admit_component(&facts, Admission::PrivateFile)?;
            let id = facts.identity;
            drop(file);
            let (actual, observed) = self
                .read_private(&name, security, MAX_RECORD_BYTES, deadline)?
                .ok_or(NativeError::OutcomeUnknown)?;
            if actual != id || observed != bytes {
                return Err(NativeError::OutcomeUnknown);
            }
            Ok(id)
        }
        pub(crate) fn publish_repair_pending(
            &self,
            target: &PrivateName,
            pending_id: FileIdentity,
            bytes: &[u8],
            previous: Option<(FileIdentity, &[u8])>,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<FileIdentity> {
            if !matches!(
                target.as_str(),
                "repair.json" | "repair-evidence-index.json"
            ) {
                return Err(NativeError::Foreign);
            }
            self.revalidate(security, true, deadline)?;
            let pending = PrivateName::new("repair-pending.json")?;
            let mut source = open_component(
                self.file()?,
                &ComponentName::new(pending.as_str())?,
                ObjectKind::File,
                GENERIC_READ | super::DELETE,
                0,
                deadline,
            )?
            .ok_or(NativeError::Foreign)?;
            let facts = observe(&source, pending.as_str(), security)?;
            admit_component(&facts, Admission::PrivateFile)?;
            if facts.identity != pending_id || read_repair_bytes(&mut source, deadline)? != bytes {
                return Err(NativeError::Foreign);
            }
            let actual = self.read_private(target, security, MAX_RECORD_BYTES, deadline)?;
            match (previous, actual) {
                (None, None) => {}
                (Some((id, old)), Some((actual, observed))) if id == actual && old == observed => {}
                _ => return Err(NativeError::Foreign),
            }
            self.publish_private(&source, target, security, deadline)?;
            drop(source);
            let (actual, observed) = self
                .read_private(target, security, MAX_RECORD_BYTES, deadline)?
                .ok_or(NativeError::OutcomeUnknown)?;
            if actual != pending_id
                || observed != bytes
                || self
                    .read_private(&pending, security, MAX_RECORD_BYTES, deadline)?
                    .is_some()
            {
                return Err(NativeError::OutcomeUnknown);
            }
            Ok(actual)
        }
        // A6b: same-handle protocol for the separate, closed payload-repair leaves.
        pub(crate) fn write_payload_repair_publication_intent(
            &self,
            previous: Option<(FileIdentity, &[u8])>,
            bytes: &[u8],
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<FileIdentity> {
            use std::io::Write;
            let name = PrivateName::new("repair-payload-publication-intent.json")?;
            self.revalidate(security, true, deadline)?;
            check_read_size(bytes.len(), MAX_RECORD_BYTES)?;
            let mut file = match previous {
                Some((id, old)) => {
                    let mut file = open_component(
                        self.file()?,
                        &ComponentName::new(name.as_str())?,
                        ObjectKind::File,
                        GENERIC_READ | GENERIC_WRITE,
                        0,
                        deadline,
                    )?
                    .ok_or(NativeError::Foreign)?;
                    let facts = observe(&file, name.as_str(), security)?;
                    admit_component(&facts, Admission::PrivateFile)?;
                    if facts.identity != id || read_repair_bytes(&mut file, deadline)? != old {
                        return Err(NativeError::Foreign);
                    }
                    file
                }
                None => self.create_private(&name, security, deadline)?,
            };
            let facts = observe(&file, name.as_str(), security)?;
            admit_component(&facts, Admission::PrivateFile)?;
            deadline.check()?;
            // Fixed intent is deliberately not published via a random temporary. A torn write
            // leaves a strictly malformed/Unknown fixed intent and can never be salvaged as permission.
            file.set_len(0).map_err(|_| NativeError::OutcomeUnknown)?;
            std::io::Seek::rewind(&mut file).map_err(|_| NativeError::OutcomeUnknown)?;
            file.write_all(bytes)
                .map_err(|_| NativeError::OutcomeUnknown)?;
            deadline.check()?;
            // SAFETY: exact retained private writable intent; ordinary flush, no path fallback.
            if unsafe { FlushFileBuffers(file.as_raw_handle()) } == 0 {
                return Err(last_error());
            }
            let id = facts.identity;
            drop(file);
            let (actual, observed) = self
                .read_private(&name, security, MAX_RECORD_BYTES, deadline)?
                .ok_or(NativeError::OutcomeUnknown)?;
            if actual != id || observed != bytes {
                return Err(NativeError::OutcomeUnknown);
            }
            deadline.check()?;
            Ok(id)
        }
        pub(crate) fn create_payload_repair_pending(
            &self,
            bytes: &[u8],
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<FileIdentity> {
            use std::io::Write;
            check_read_size(bytes.len(), MAX_RECORD_BYTES)?;
            let name = PrivateName::new("repair-payload-pending.json")?;
            // This raw fixed pending file contains the selected target envelope, NOT an
            // independently trusted RepairPayloadPending body. Only the intent's exact target/stamp admits it.
            let mut file = self.create_private(&name, security, deadline)?;
            deadline.check()?;
            file.write_all(bytes)
                .map_err(|_| NativeError::OutcomeUnknown)?;
            deadline.check()?;
            // SAFETY: our one CREATE_NEW complete bounded pending record, retained through flush.
            if unsafe { FlushFileBuffers(file.as_raw_handle()) } == 0 {
                return Err(last_error());
            }
            let facts = observe(&file, name.as_str(), security)?;
            admit_component(&facts, Admission::PrivateFile)?;
            let id = facts.identity;
            drop(file);
            let (actual, observed) = self
                .read_private(&name, security, MAX_RECORD_BYTES, deadline)?
                .ok_or(NativeError::OutcomeUnknown)?;
            if actual != id || observed != bytes {
                return Err(NativeError::OutcomeUnknown);
            }
            Ok(id)
        }
        pub(crate) fn publish_payload_repair_pending(
            &self,
            target: &PrivateName,
            pending_id: FileIdentity,
            bytes: &[u8],
            previous: Option<(FileIdentity, &[u8])>,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<FileIdentity> {
            if !matches!(
                target.as_str(),
                "repair-payload.json" | "repair-payload-catalog.json"
            ) {
                return Err(NativeError::Foreign);
            }
            self.revalidate(security, true, deadline)?;
            let pending = PrivateName::new("repair-payload-pending.json")?;
            let mut source = open_component(
                self.file()?,
                &ComponentName::new(pending.as_str())?,
                ObjectKind::File,
                GENERIC_READ | super::DELETE,
                0,
                deadline,
            )?
            .ok_or(NativeError::Foreign)?;
            let facts = observe(&source, pending.as_str(), security)?;
            admit_component(&facts, Admission::PrivateFile)?;
            if facts.identity != pending_id || read_repair_bytes(&mut source, deadline)? != bytes {
                return Err(NativeError::Foreign);
            }
            let actual = self.read_private(target, security, MAX_RECORD_BYTES, deadline)?;
            match (previous, actual) {
                (None, None) => {}
                (Some((id, old)), Some((actual, observed))) if id == actual && old == observed => {}
                _ => return Err(NativeError::Foreign),
            }
            self.publish_private(&source, target, security, deadline)?;
            drop(source);
            let (actual, observed) = self
                .read_private(target, security, MAX_RECORD_BYTES, deadline)?
                .ok_or(NativeError::OutcomeUnknown)?;
            if actual != pending_id
                || observed != bytes
                || self
                    .read_private(&pending, security, MAX_RECORD_BYTES, deadline)?
                    .is_some()
            {
                return Err(NativeError::OutcomeUnknown);
            }
            Ok(actual)
        }
        pub(crate) fn repair_evidence_slot(
            &self,
            slot: u8,
            create: bool,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<Option<Anchor>> {
            let leaf = match slot {
                0 => "slot-0",
                1 => "slot-1",
                2 => "slot-2",
                _ => return Err(NativeError::Invalid),
            };
            self.revalidate(security, true, deadline)?;
            let evidence = match self.child("repair-evidence", security, deadline)? {
                Some(root) => root,
                None if create => {
                    self.create_child_directory("repair-evidence", security, deadline)?
                }
                None => return Ok(None),
            };
            match evidence.child(leaf, security, deadline)? {
                Some(root) => Ok(Some(root)),
                None if create => evidence
                    .create_child_directory(leaf, security, deadline)
                    .map(Some),
                None => Ok(None),
            }
        }
        pub(crate) fn archive_repair_metadata(
            &self,
            leaf: &str,
            expected: FileIdentity,
            bytes: &[u8],
            destination: &Anchor,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            if !matches!(
                leaf,
                "outer-upgrade.json" | "file-recovery.json" | "removal.json"
            ) {
                return Err(NativeError::Foreign);
            }
            self.revalidate(security, true, deadline)?;
            destination.revalidate(security, true, deadline)?;
            if self.identity()?.volume != destination.identity()?.volume {
                return Err(NativeError::Foreign);
            }
            let name = PrivateName::new(leaf)?;
            if destination
                .read_private(&name, security, MAX_RECORD_BYTES, deadline)?
                .is_some()
            {
                return Err(NativeError::Foreign);
            }
            // Strict no-follow File adds READ_CONTROL for actual owner/DACL, unlike opaque opens.
            let mut source = open_component(
                self.file()?,
                &ComponentName::new(leaf)?,
                ObjectKind::File,
                GENERIC_READ | super::DELETE,
                0,
                deadline,
            )?
            .ok_or(NativeError::Foreign)?;
            let facts = observe(&source, leaf, security)?;
            admit_component(&facts, Admission::PrivateFile)?;
            if facts.identity != expected || read_repair_bytes(&mut source, deadline)? != bytes {
                return Err(NativeError::Foreign);
            }
            rename_no_replace(&source, destination, leaf, deadline)?;
            if raw_identity(&source)? != expected || name_of_repair_file(&source)? != leaf {
                return Err(NativeError::OutcomeUnknown);
            }
            drop(source);
            // Neither DeletePending nor rename return is settlement. Reobserve both exact names.
            if self
                .read_private(&name, security, MAX_RECORD_BYTES, deadline)?
                .is_some()
            {
                return Err(NativeError::OutcomeUnknown);
            }
            let (id, archived) = destination
                .read_private(&name, security, MAX_RECORD_BYTES, deadline)?
                .ok_or(NativeError::OutcomeUnknown)?;
            if id != expected || archived != bytes {
                return Err(NativeError::OutcomeUnknown);
            }
            deadline.check()
        }
        /// Exact owned first-recovery leaf only. Normal exclusive DELETE can never force a
        /// mapped image; a reparse entry is deleted as its link, never followed.
        pub(crate) fn delete_first_recovery_leaf(
            &self,
            leaf: &str,
            expected: FileIdentity,
            security: &Security,
            deadline: &Deadline,
            effect: &dyn Fn(),
        ) -> NativeResult<bool> {
            let allowed = super::super::super::payload::inventory::PayloadRole::ALL
                .iter()
                .any(|r| r.leaf() == leaf)
                || matches!(
                    leaf,
                    "first-install-stage" | "first-install-backups" | "Crosspane"
                )
                || leaf.len() == 32
                    && leaf
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
            if !allowed {
                return Err(NativeError::Foreign);
            }
            self.revalidate(security, false, deadline)?;
            let object = match removal_object(
                self.file()?,
                leaf,
                super::DELETE | FILE_LIST_DIRECTORY,
                0,
                self.identity()?.volume,
                deadline,
            ) {
                Ok(None) => return Ok(true),
                Ok(Some(object)) => object,
                Err(NativeError::Unavailable) => return Ok(false), // Positive no-dispatch; retain.
                Err(error) => return Err(error),
            };
            if object.identity != expected {
                return Err(NativeError::Foreign);
            }
            let attrs: FILE_ATTRIBUTE_TAG_INFO = info(&object.file, FileAttributeTagInfo)?;
            if attrs.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0
                && attrs.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0
                && !entries(&object.file, deadline)?.is_empty()
            {
                return Ok(false);
            }
            deadline.check()?;
            effect();
            let mut disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
            // SAFETY: ordinary disposition on the retained no-follow, exclusive DELETE handle
            // with the exact admitted FileId; directories were positively empty on this handle.
            if unsafe {
                SetFileInformationByHandle(
                    object.file.as_raw_handle(),
                    FileDispositionInfo,
                    (&mut disposition as *mut FILE_DISPOSITION_INFO).cast(),
                    std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
                )
            } == 0
            {
                return Err(last_error());
            }
            drop(object);
            if self.opaque(leaf, false, security, deadline)?.is_some() {
                return Err(NativeError::OutcomeUnknown);
            }
            deadline.check()?;
            Ok(true)
        }
        pub(crate) fn first_history_slot(
            &self,
            slot: u8,
            create: bool,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<Option<Anchor>> {
            let leaf = match slot {
                0 => "slot-0",
                1 => "slot-1",
                2 => "slot-2",
                _ => return Err(NativeError::Invalid),
            };
            self.revalidate(security, true, deadline)?;
            let evidence = match self.child("first-install-history", security, deadline)? {
                Some(root) => root,
                None if create => {
                    self.create_child_directory("first-install-history", security, deadline)?
                }
                None => return Ok(None),
            };
            match evidence.child(leaf, security, deadline)? {
                Some(root) => Ok(Some(root)),
                None if create => evidence
                    .create_child_directory(leaf, security, deadline)
                    .map(Some),
                None => Ok(None),
            }
        }
        pub(crate) fn archive_first_history_metadata(
            &self,
            leaf: &str,
            expected: FileIdentity,
            bytes: &[u8],
            destination: &Anchor,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            if !super::super::super::first_install::record::history_leaf_allowed(leaf) {
                return Err(NativeError::Foreign);
            }
            self.revalidate(security, true, deadline)?;
            destination.revalidate(security, true, deadline)?;
            if self.identity()?.volume != destination.identity()?.volume {
                return Err(NativeError::Foreign);
            }
            let name = PrivateName::new(leaf)?;
            if destination
                .read_private(&name, security, MAX_RECORD_BYTES, deadline)?
                .is_some()
            {
                return Err(NativeError::Foreign);
            }
            // Strict no-follow File adds READ_CONTROL for actual owner/DACL, unlike opaque opens.
            let mut source = open_component(
                self.file()?,
                &ComponentName::new(leaf)?,
                ObjectKind::File,
                GENERIC_READ | super::DELETE,
                0,
                deadline,
            )?
            .ok_or(NativeError::Foreign)?;
            let facts = observe(&source, leaf, security)?;
            admit_component(&facts, Admission::PrivateFile)?;
            if facts.identity != expected || read_repair_bytes(&mut source, deadline)? != bytes {
                return Err(NativeError::Foreign);
            }
            rename_no_replace(&source, destination, leaf, deadline)?;
            if raw_identity(&source)? != expected || name_of_repair_file(&source)? != leaf {
                return Err(NativeError::OutcomeUnknown);
            }
            drop(source);
            // Neither DeletePending nor rename return is settlement. Reobserve both exact names.
            if self
                .read_private(&name, security, MAX_RECORD_BYTES, deadline)?
                .is_some()
            {
                return Err(NativeError::OutcomeUnknown);
            }
            let (id, archived) = destination
                .read_private(&name, security, MAX_RECORD_BYTES, deadline)?
                .ok_or(NativeError::OutcomeUnknown)?;
            if id != expected || archived != bytes {
                return Err(NativeError::OutcomeUnknown);
            }
            deadline.check()
        }
    }
    #[cfg(not(test))]
    fn name_of_repair_file(file: &File) -> NativeResult<String> {
        name(file)
    }
    #[cfg(not(test))]
    fn read_repair_bytes(file: &mut File, deadline: &Deadline) -> NativeResult<Vec<u8>> {
        use std::io::{Read, Seek};
        let standard: FILE_STANDARD_INFO = info(file, FileStandardInfo)?;
        let length = usize::try_from(standard.EndOfFile).map_err(|_| NativeError::Oversize)?;
        check_read_size(length, MAX_RECORD_BYTES)?;
        deadline.check()?;
        file.rewind().map_err(|_| NativeError::Unavailable)?;
        let mut bytes = Vec::with_capacity(length);
        file.take((MAX_RECORD_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| NativeError::Unavailable)?;
        check_read_size(bytes.len(), MAX_RECORD_BYTES)?;
        if bytes.len() != length {
            return Err(NativeError::Foreign);
        }
        deadline.check()?;
        Ok(bytes)
    }

    fn rename_no_replace(
        source: &File,
        destination: &Anchor,
        leaf: &str,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        ComponentName::new(leaf)?;
        let units: Vec<u16> = leaf.encode_utf16().collect();
        let offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
        let length = offset
            .checked_add(units.len() * 2)
            .ok_or(NativeError::Invalid)?;
        let mut buffer = vec![
            0u64;
            length
                .max(std::mem::size_of::<FILE_RENAME_INFO>())
                .div_ceil(8)
        ];
        let pointer = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        // SAFETY: aligned complete variable buffer; single checked component under retained root.
        // ReplaceIfExists=false preserves backup collisions and loaded-image sharing refusal.
        unsafe {
            (*pointer).Anonymous.ReplaceIfExists = false;
            (*pointer).RootDirectory = destination.file()?.as_raw_handle();
            (*pointer).FileNameLength = (units.len() * 2) as u32;
            std::ptr::copy_nonoverlapping(
                units.as_ptr(),
                buffer.as_mut_ptr().cast::<u8>().add(offset).cast::<u16>(),
                units.len(),
            );
        }
        deadline.check()?;
        // SAFETY: retained source with DELETE, same-volume root, no following/copy/overwrite fallback.
        if unsafe {
            SetFileInformationByHandle(
                source.as_raw_handle(),
                FileRenameInfo,
                pointer.cast(),
                length as u32,
            )
        } == 0
        {
            return Err(last_error());
        }
        deadline.check()
    }
    fn entries(file: &File, deadline: &Deadline) -> NativeResult<Vec<String>> {
        let mut output = Vec::new();
        let mut restart = true;
        loop {
            deadline.check()?;
            let mut buffer = vec![0u64; 8192];
            // SAFETY: retained non-reparse directory, aligned bounded enumeration output. Names
            // are observations only; each child is separately opened relative with no traversal.
            if unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    if restart {
                        FileIdBothDirectoryRestartInfo
                    } else {
                        FileIdBothDirectoryInfo
                    },
                    buffer.as_mut_ptr().cast(),
                    (buffer.len() * 8) as u32,
                )
            } == 0
            {
                // SAFETY: reads this thread's last completed query error.
                let code = unsafe { GetLastError() };
                if code == ERROR_NO_MORE_FILES {
                    break;
                }
                return Err(error(code));
            }
            restart = false;
            let mut at = 0usize;
            loop {
                let header = std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
                if at + header > buffer.len() * 8 {
                    return Err(NativeError::Unavailable);
                }
                // SAFETY: current fixed header lies inside complete successful aligned output.
                let row = unsafe { buffer.as_ptr().cast::<u8>().add(at) };
                // SAFETY: both fixed u32 fields lie within the checked header; no flexible-array reference.
                let bytes = unsafe {
                    row.add(std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileNameLength))
                        .cast::<u32>()
                        .read_unaligned()
                } as usize;
                if !bytes.is_multiple_of(2) || at + header + bytes > buffer.len() * 8 {
                    return Err(NativeError::Unavailable);
                }
                // SAFETY: exact variable UTF-16 name range checked in the live output allocation.
                let units = unsafe {
                    std::slice::from_raw_parts(
                        buffer.as_ptr().cast::<u8>().add(at + header).cast::<u16>(),
                        bytes / 2,
                    )
                };
                let name = String::from_utf16(units).map_err(|_| NativeError::Unavailable)?;
                if name != "." && name != ".." {
                    ComponentName::new(&name)?;
                    output.push(name);
                    if output.len() > 1024 {
                        return Err(NativeError::Oversize);
                    }
                }
                // SAFETY: first u32 field lies within the previously checked fixed header.
                let next = unsafe { row.cast::<u32>().read_unaligned() } as usize;
                if next == 0 {
                    break;
                }
                if next < header
                    || !next.is_multiple_of(8)
                    || at.checked_add(next).is_none_or(|value| value <= at)
                {
                    return Err(NativeError::Unavailable);
                }
                at += next;
            }
        }
        Ok(output)
    }
    fn prune_leaf(
        leaf: OpaqueData,
        deadline: &Deadline,
        depth: usize,
        budget: &mut usize,
    ) -> NativeResult<()> {
        deadline.check()?;
        if depth > 32 || *budget == 0 {
            return Err(NativeError::Oversize);
        }
        *budget -= 1;
        let standard: FILE_STANDARD_INFO = info(&leaf.file, FileStandardInfo)?;
        let tag: FILE_ATTRIBUTE_TAG_INFO = info(&leaf.file, FileAttributeTagInfo)?;
        if standard.Directory && tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
            for name in entries(&leaf.file, deadline)? {
                let request = ComponentRequest {
                    name: ComponentName::new(&name)?,
                    kind: ObjectKind::Opaque,
                    create: false,
                    dont_reparse: false,
                    open_reparse_point: true,
                };
                let file = (NativeComponentIo {
                    access: super::DELETE | FILE_LIST_DIRECTORY,
                    sharing: 0,
                    descriptor: None,
                })
                .submit(Some(&leaf.file), &request)?;
                let identity = raw_identity(&file)?;
                if identity.volume != leaf.identity.volume {
                    return Err(NativeError::Foreign);
                }
                if self::name(&file)? != name {
                    return Err(NativeError::Foreign);
                }
                prune_leaf(
                    OpaqueData {
                        file,
                        identity,
                        name,
                    },
                    deadline,
                    depth + 1,
                    budget,
                )?;
            }
        }
        let mut disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        deadline.check()?;
        // SAFETY: exact exclusively opened entry itself; reparse targets never enumerated or deleted.
        if unsafe {
            SetFileInformationByHandle(
                leaf.file.as_raw_handle(),
                FileDispositionInfo,
                (&mut disposition as *mut FILE_DISPOSITION_INFO).cast(),
                std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        } == 0
        {
            return Err(last_error());
        }
        deadline.check()
    }

    /// Fresh rooted observations only. Neither the relative names nor FileIds are authority;
    /// the wiring requires the actual removal journal permit and original completion separately.
    #[cfg(not(test))]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum RemovalObjectKind {
        File,
        Directory,
        ReparseLink,
    }
    #[cfg(not(test))]
    pub(crate) struct RemovalEntryObservation {
        pub(crate) components: Vec<String>,
        pub(crate) parent: FileIdentity,
        pub(crate) identity: FileIdentity,
        pub(crate) kind: RemovalObjectKind,
    }
    #[cfg(not(test))]
    pub(crate) struct RemovalTreeObservation {
        pub(crate) root: Option<FileIdentity>,
        pub(crate) nodes: Vec<RemovalEntryObservation>,
    }
    #[cfg(not(test))]
    fn removal_object(
        parent: &File,
        component: &str,
        access: u32,
        sharing: u32,
        volume: u64,
        deadline: &Deadline,
    ) -> NativeResult<Option<OpaqueData>> {
        let request = ComponentRequest {
            name: ComponentName::new(component)?,
            kind: ObjectKind::Opaque,
            create: false,
            dont_reparse: false,
            open_reparse_point: true,
        };
        deadline.check()?;
        let file = match (NativeComponentIo {
            access,
            sharing,
            descriptor: None,
        })
        .submit(Some(parent), &request)
        {
            Ok(file) => file,
            Err(NativeError::Missing) => return Ok(None),
            Err(error) => return Err(error),
        };
        let identity = raw_identity(&file)?;
        if identity.volume != volume || name(&file)? != component {
            return Err(NativeError::Foreign);
        }
        // SAFETY: read-only type query of the retained actual disk object; no device/pipe admitted.
        if unsafe { GetFileType(file.as_raw_handle()) } != FILE_TYPE_DISK {
            return Err(NativeError::Foreign);
        }
        deadline.check()?;
        Ok(Some(OpaqueData {
            file,
            identity,
            name: component.into(),
        }))
    }
    #[cfg(not(test))]
    fn removal_kind(file: &File) -> NativeResult<RemovalObjectKind> {
        let standard: FILE_STANDARD_INFO = info(file, FileStandardInfo)?;
        let tag: FILE_ATTRIBUTE_TAG_INFO = info(file, FileAttributeTagInfo)?;
        Ok(if tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            RemovalObjectKind::ReparseLink
        } else if standard.Directory {
            RemovalObjectKind::Directory
        } else {
            RemovalObjectKind::File
        })
    }
    #[cfg(not(test))]
    fn removal_directory(
        parent: &File,
        leaf: &str,
        expected: FileIdentity,
        deadline: &Deadline,
    ) -> NativeResult<OpaqueData> {
        let object = removal_object(
            parent,
            leaf,
            FILE_LIST_DIRECTORY | FILE_TRAVERSE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            expected.volume,
            deadline,
        )?
        .ok_or(NativeError::Missing)?;
        if object.identity != expected
            || removal_kind(&object.file)? != RemovalObjectKind::Directory
        {
            return Err(NativeError::Foreign);
        }
        // A genuine READ/LIST pin with no delete sharing binds this same directory while descending.
        Ok(object)
    }
    #[cfg(not(test))]
    fn removal_snapshot(
        directory: &File,
        parent_id: FileIdentity,
        path: &mut Vec<String>,
        output: &mut Vec<RemovalEntryObservation>,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        if path.len() > 32 {
            return Err(NativeError::Oversize);
        }
        for component in entries(directory, deadline)? {
            if output.len() >= 1024 {
                return Err(NativeError::Oversize);
            }
            let object = removal_object(
                directory,
                &component,
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                parent_id.volume,
                deadline,
            )?
            .ok_or(NativeError::Foreign)?;
            let identity = object.identity;
            let kind = removal_kind(&object.file)?;
            path.push(component.clone());
            if path.len() > 32 {
                return Err(NativeError::Oversize);
            }
            if kind == RemovalObjectKind::Directory {
                // Never descend a reparse entry. Reopen this exact ordinary directory with real
                // LIST access/no delete sharing, then revalidate its FileId before recursion.
                let pin = removal_directory(directory, &component, identity, deadline)?;
                removal_snapshot(&pin.file, identity, path, output, deadline)?;
            }
            if output.len() >= 1024 {
                return Err(NativeError::Oversize);
            }
            output.push(RemovalEntryObservation {
                components: path.clone(),
                parent: parent_id,
                identity,
                kind,
            });
            path.pop();
        }
        deadline.check()
    }
    #[cfg(not(test))]
    impl Anchor {
        /// Caller supplies only the genuine Shell-admitted Programs anchor. The root leaf is
        /// literal; observations keep no install-root pin after returning, enabling final removal.
        pub(crate) fn observe_removal_install(
            &self,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<RemovalTreeObservation> {
            self.revalidate(security, false, deadline)?;
            let root = removal_object(
                self.file()?,
                "Crosspane",
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                self.identity()?.volume,
                deadline,
            )?;
            let Some(root) = root else {
                return Ok(RemovalTreeObservation {
                    root: None,
                    nodes: Vec::new(),
                });
            };
            if removal_kind(&root.file)? != RemovalObjectKind::Directory {
                return Err(NativeError::Foreign);
            }
            let pin = removal_directory(self.file()?, "Crosspane", root.identity, deadline)?;
            let mut nodes = Vec::new();
            removal_snapshot(
                &pin.file,
                pin.identity,
                &mut Vec::new(),
                &mut nodes,
                deadline,
            )?;
            self.revalidate(security, false, deadline)?;
            Ok(RemovalTreeObservation {
                root: Some(pin.identity),
                nodes,
            })
        }
        // Independently explicit immutable root/parent/leaf identities and type are all plan facts;
        // the native wiring supplies the distinct genuine publication/completion capabilities.
        #[allow(clippy::too_many_arguments)]
        pub(crate) fn delete_removal_node(
            &self,
            root: FileIdentity,
            components: &[String],
            ancestors: &[FileIdentity],
            expected_parent: FileIdentity,
            expected: FileIdentity,
            kind: RemovalObjectKind,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.revalidate(security, false, deadline)?;
            if components.is_empty()
                || components.len() > 32
                || ancestors.len() != components.len()
                || ancestors.first() != Some(&root)
                || root.volume != self.identity()?.volume
                || expected.volume != root.volume
                || expected_parent.volume != root.volume
            {
                return Err(NativeError::Foreign);
            }
            let mut pins = vec![removal_directory(
                self.file()?,
                "Crosspane",
                root,
                deadline,
            )?];
            for (index, component) in components[..components.len() - 1].iter().enumerate() {
                let parent = pins.last().ok_or(NativeError::Foreign)?;
                let child = removal_object(
                    &parent.file,
                    component,
                    FILE_LIST_DIRECTORY | FILE_TRAVERSE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    root.volume,
                    deadline,
                )?
                .ok_or(NativeError::Missing)?;
                if child.identity != ancestors[index + 1]
                    || removal_kind(&child.file)? != RemovalObjectKind::Directory
                {
                    return Err(NativeError::Foreign);
                }
                pins.push(child);
            }
            let parent = pins.last().ok_or(NativeError::Foreign)?;
            if parent.identity != expected_parent {
                return Err(NativeError::Foreign);
            }
            let leaf = components.last().ok_or(NativeError::Invalid)?;
            let access = super::DELETE
                | if kind == RemovalObjectKind::Directory {
                    FILE_LIST_DIRECTORY
                } else {
                    0
                };
            let object = removal_object(&parent.file, leaf, access, 0, root.volume, deadline)?;
            let Some(object) = object else { return Ok(()) }; // Only wiring's exact durable delete intent admits absence.
            if object.identity != expected || removal_kind(&object.file)? != kind {
                return Err(NativeError::Foreign);
            }
            if kind == RemovalObjectKind::Directory && !entries(&object.file, deadline)?.is_empty()
            {
                return Err(NativeError::Foreign);
            }
            let mut disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
            deadline.check()?;
            // SAFETY: the exclusively opened same planned FileId is marked by normal disposition.
            // Reparse entries are the link itself and are never enumerated or followed; no force flags.
            if unsafe {
                SetFileInformationByHandle(
                    object.file.as_raw_handle(),
                    FileDispositionInfo,
                    (&mut disposition as *mut FILE_DISPOSITION_INFO).cast(),
                    std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
                )
            } == 0
            {
                return Err(last_error());
            }
            drop(object);
            deadline.check()?;
            if removal_object(
                &parent.file,
                leaf,
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                root.volume,
                deadline,
            )?
            .is_some()
            {
                return Err(NativeError::OutcomeUnknown);
            }
            self.revalidate(security, false, deadline)
        }
        /// Metadata only, through the same original parent chain. An absent original ancestor
        /// implies the selected descendant is absent; a replacement identity never does.
        pub(crate) fn observe_removal_node(
            &self,
            root: FileIdentity,
            components: &[String],
            ancestors: &[FileIdentity],
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<Option<(FileIdentity, RemovalObjectKind)>> {
            self.revalidate(security, false, deadline)?;
            if components.is_empty()
                || components.len() > 32
                || ancestors.len() != components.len()
                || ancestors.first() != Some(&root)
            {
                return Err(NativeError::Invalid);
            }
            let original = removal_object(
                self.file()?,
                "Crosspane",
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                self.identity()?.volume,
                deadline,
            )?;
            let Some(original) = original else {
                return Ok(None);
            };
            if original.identity != root
                || removal_kind(&original.file)? != RemovalObjectKind::Directory
            {
                return Err(NativeError::Foreign);
            }
            let mut pins = vec![removal_directory(
                self.file()?,
                "Crosspane",
                root,
                deadline,
            )?];
            for (index, component) in components[..components.len() - 1].iter().enumerate() {
                let parent = pins.last().ok_or(NativeError::Foreign)?;
                let child = removal_object(
                    &parent.file,
                    component,
                    FILE_LIST_DIRECTORY | FILE_TRAVERSE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    root.volume,
                    deadline,
                )?;
                let Some(child) = child else { return Ok(None) };
                if child.identity != ancestors[index + 1]
                    || removal_kind(&child.file)? != RemovalObjectKind::Directory
                {
                    return Err(NativeError::Foreign);
                }
                pins.push(child);
            }
            let parent = pins.last().ok_or(NativeError::Foreign)?;
            removal_object(
                &parent.file,
                components.last().ok_or(NativeError::Invalid)?,
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                root.volume,
                deadline,
            )?
            .map(|object| Ok((object.identity, removal_kind(&object.file)?)))
            .transpose()
        }
        pub(crate) fn removal_install_identity(
            &self,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<Option<FileIdentity>> {
            self.revalidate(security, false, deadline)?;
            let root = removal_object(
                self.file()?,
                "Crosspane",
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                self.identity()?.volume,
                deadline,
            )?;
            root.map(|object| {
                if removal_kind(&object.file)? != RemovalObjectKind::Directory {
                    return Err(NativeError::Foreign);
                }
                Ok(object.identity)
            })
            .transpose()
        }
        /// Exact fixed regular copy metadata; READ_CONTROL is explicit for same-handle ACL facts.
        pub(crate) fn removal_copy_identity(
            &self,
            leaf: &str,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<Option<FileIdentity>> {
            if !matches!(leaf, "helper-copy.exe" | "keeper-copy.exe") {
                return Err(NativeError::Invalid);
            }
            self.revalidate(security, true, deadline)?;
            let object = removal_object(
                self.file()?,
                leaf,
                READ_CONTROL_ACCESS,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                self.identity()?.volume,
                deadline,
            )?;
            object
                .map(|object| {
                    let facts = observe(&object.file, leaf, security)?;
                    admit_component(&facts, Admission::PrivateFile)?;
                    deadline.check()?;
                    Ok(object.identity)
                })
                .transpose()
        }
        /// The caller separately proves original process exit or genuine exclusive cold namespace
        /// absence. This primitive only deletes the literal selected regular copy FileId.
        pub(crate) fn delete_removal_copy(
            &self,
            leaf: &str,
            expected: FileIdentity,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            if !matches!(leaf, "helper-copy.exe" | "keeper-copy.exe") {
                return Err(NativeError::Invalid);
            }
            self.revalidate(security, true, deadline)?;
            // Opaque opens intentionally add no implicit READ_CONTROL. This exact copy needs
            // owner/DACL admission on the SAME handle before normal disposition.
            let object = removal_object(
                self.file()?,
                leaf,
                super::DELETE | READ_CONTROL_ACCESS,
                0,
                self.identity()?.volume,
                deadline,
            )?;
            let Some(object) = object else { return Ok(()) };
            let facts = observe(&object.file, leaf, security)?;
            admit_component(&facts, Admission::PrivateFile)?;
            if object.identity != expected {
                return Err(NativeError::Foreign);
            }
            let mut disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
            deadline.check()?;
            // SAFETY: normal disposition on the exact exclusively opened regular copy handle;
            // the independent caller seal prohibits executing-image cleanup and grants no bypass.
            if unsafe {
                SetFileInformationByHandle(
                    object.file.as_raw_handle(),
                    FileDispositionInfo,
                    (&mut disposition as *mut FILE_DISPOSITION_INFO).cast(),
                    std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
                )
            } == 0
            {
                return Err(last_error());
            }
            drop(object);
            deadline.check()?;
            if removal_object(
                self.file()?,
                leaf,
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                self.identity()?.volume,
                deadline,
            )?
            .is_some()
            {
                return Err(NativeError::OutcomeUnknown);
            }
            self.revalidate(security, true, deadline)
        }
        /// Requires every planned child result first. This method cannot prune the root and owns
        /// no install-root aliases itself; it deletes only an actually empty exact root object.
        pub(crate) fn delete_removal_install_root(
            &self,
            expected: FileIdentity,
            security: &Security,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.revalidate(security, false, deadline)?;
            let root = removal_object(
                self.file()?,
                "Crosspane",
                super::DELETE | FILE_LIST_DIRECTORY,
                0,
                self.identity()?.volume,
                deadline,
            )?;
            let Some(root) = root else { return Ok(()) };
            if root.identity != expected
                || removal_kind(&root.file)? != RemovalObjectKind::Directory
                || !entries(&root.file, deadline)?.is_empty()
            {
                return Err(NativeError::Foreign);
            }
            let mut disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
            deadline.check()?;
            // SAFETY: exact empty original install-root handle, exclusive DELETE; no recursive,
            // ADS, POSIX, reboot-pending or executing-image workaround is used.
            if unsafe {
                SetFileInformationByHandle(
                    root.file.as_raw_handle(),
                    FileDispositionInfo,
                    (&mut disposition as *mut FILE_DISPOSITION_INFO).cast(),
                    std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
                )
            } == 0
            {
                return Err(last_error());
            }
            drop(root);
            deadline.check()?;
            if removal_object(
                self.file()?,
                "Crosspane",
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                self.identity()?.volume,
                deadline,
            )?
            .is_some()
            {
                return Err(NativeError::OutcomeUnknown);
            }
            self.revalidate(security, false, deadline)
        }
    }

    /// Fixture-only creator ledger, not reconstructed from disk or deserialized bytes.
    #[cfg(test)]
    #[derive(Clone)]
    pub(crate) struct CreatedLeaf {
        pub name: PrivateName,
        pub identity: FileIdentity,
        pub directory: bool,
    }

    #[cfg(test)]
    pub(crate) fn cleanup_fixture(
        parent: &Anchor,
        root_name: &PrivateName,
        expected_root: FileIdentity,
        leaves: &[CreatedLeaf],
        security: &Security,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        parent.revalidate(security, false, deadline)?;
        // All owned backend/lock/child pins must already have been closed. Reopen the known
        // created root with DELETE, binding it to this original retained Temp parent object.
        let root = open_component(
            parent.file()?,
            &ComponentName::new(root_name.as_str())?,
            ObjectKind::Directory,
            super::DELETE | FILE_LIST_DIRECTORY | FILE_TRAVERSE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            deadline,
        )?
        .ok_or(NativeError::Foreign)?;
        let facts = observe(&root, root_name.as_str(), security)?;
        admit_component(&facts, Admission::PrivateDirectory)?;
        if facts.identity != expected_root {
            return Err(NativeError::Foreign);
        }
        for leaf in leaves.iter().rev() {
            deadline.check()?;
            let kind = if leaf.directory {
                ObjectKind::Directory
            } else {
                ObjectKind::File
            };
            let file = open_component(
                &root,
                &ComponentName::new(leaf.name.as_str())?,
                kind,
                super::DELETE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                deadline,
            )?
            .ok_or(NativeError::Foreign)?;
            let found = observe(&file, leaf.name.as_str(), security)?;
            // The original creator identity authorizes only this exact leaf, even for an
            // intentionally malformed ACL fixture. Never follow reparse points or hardlinks.
            if found.identity != leaf.identity
                || found.reparse
                || found.directory != leaf.directory
                || found.actual != leaf.name.as_str()
                || (!found.directory && found.links != 1)
            {
                return Err(NativeError::Foreign);
            }
            set_disposition(&file)?;
        }
        deadline.check()?;
        // A directory containing any unrecorded leaf refuses deletion. There is no recursion
        // or adoption of unexpected names; a failure leaves all remaining material intact.
        set_disposition(&root)?;
        drop(root);
        match open_component(
            parent.file()?,
            &ComponentName::new(root_name.as_str())?,
            ObjectKind::Directory,
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            deadline,
        )? {
            None => Ok(()),
            Some(_) => Err(NativeError::OutcomeUnknown),
        }
    }
    #[cfg(test)]
    fn set_disposition(file: &File) -> NativeResult<()> {
        let mut disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        // SAFETY: only a freshly rechecked sealed fixture creator handle with DELETE access.
        if unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FileDispositionInfo,
                (&mut disposition as *mut FILE_DISPOSITION_INFO).cast(),
                std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        } == 0
        {
            return Err(last_error());
        }
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn fixture_foreign_acl(
        parent: &Anchor,
        name: &PrivateName,
        expected: FileIdentity,
        directory: bool,
        security: &Security,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        let file = open_component(
            parent.file()?,
            &ComponentName::new(name.as_str())?,
            if directory {
                ObjectKind::Directory
            } else {
                ObjectKind::File
            },
            WRITE_DAC,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            deadline,
        )?
        .ok_or(NativeError::Foreign)?;
        let facts = observe(&file, name.as_str(), security)?;
        admit_component(
            &facts,
            if directory {
                Admission::PrivateDirectory
            } else {
                Admission::PrivateFile
            },
        )?;
        if facts.identity != expected {
            return Err(NativeError::Foreign);
        }
        let sddl = wide(&format!(
            "O:{}D:P(A;;FA;;;{})(A;;FA;;;SY)(A;;GW;;;BU)",
            security.user.sddl(),
            security.user.sddl()
        ))?;
        let mut raw = std::ptr::null_mut();
        // SAFETY: fixture's own created leaf; bounded fixed SDDL, no parent/profile ACL mutation.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut raw,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(last_error());
        }
        let descriptor = Descriptor(raw);
        let mut present = 0;
        let mut dacl = std::ptr::null_mut();
        let mut defaulted = 0;
        // SAFETY: retained successful conversion descriptor; query its own DACL only.
        if unsafe {
            GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut dacl, &mut defaulted)
        } == 0
            || present == 0
        {
            return Err(NativeError::Unavailable);
        }
        // SAFETY: exact owned fixture file, WRITE_DAC access, no owner/parent/other object changes.
        let code = unsafe {
            SetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null_mut(),
            )
        };
        if code != ERROR_SUCCESS {
            return Err(error(code));
        }
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn fixture_case_alias(
        parent: &Anchor,
        name: &PrivateName,
        expected: FileIdentity,
        security: &Security,
        deadline: &Deadline,
    ) -> NativeResult<bool> {
        let upper = name.as_str().to_ascii_uppercase();
        let file = open_component(
            parent.file()?,
            &ComponentName::new(&upper)?,
            ObjectKind::File,
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_DELETE,
            deadline,
        )?;
        match file {
            None => Ok(true),
            Some(file) => {
                let facts = observe(&file, &upper, security)?;
                if facts.identity != expected {
                    return Err(NativeError::Foreign);
                }
                Ok(matches!(
                    admit_component(&facts, Admission::PrivateFile),
                    Err(NativeError::Foreign)
                ))
            }
        }
    }
    #[cfg(test)]
    pub(crate) fn fixture_reparse_refusal(
        parent: &Anchor,
        created: &CreatedLeaf,
        security: &Security,
        deadline: &Deadline,
    ) -> NativeResult<bool> {
        use windows_sys::{
            Wdk::Storage::FileSystem::REPARSE_DATA_BUFFER,
            Win32::System::{
                IO::DeviceIoControl,
                Ioctl::{FSCTL_DELETE_REPARSE_POINT, FSCTL_SET_REPARSE_POINT},
                SystemServices::IO_REPARSE_TAG_MOUNT_POINT,
            },
        };
        if !created.directory {
            return Err(NativeError::Foreign);
        }
        parent.revalidate(security, true, deadline)?;
        let file = open_component(
            parent.file()?,
            &ComponentName::new(created.name.as_str())?,
            ObjectKind::Directory,
            FILE_WRITE_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            deadline,
        )?
        .ok_or(NativeError::Foreign)?;
        let facts = observe(&file, created.name.as_str(), security)?;
        admit_component(&facts, Admission::PrivateDirectory)?;
        if facts.identity != created.identity {
            return Err(NativeError::Foreign);
        }
        // STATIC own fixture only: the tag target is this same owned scratch root, not a
        // foreign object. No concurrent changes or operation races are attempted.
        let target: Vec<u16> = format!("\\??\\{}", parent.path).encode_utf16().collect();
        let name_bytes = target.len().checked_mul(2).ok_or(NativeError::Oversize)?;
        let data_length = 12usize
            .checked_add(name_bytes)
            .ok_or(NativeError::Oversize)?;
        let input_length = 8 + data_length;
        if input_length > 16384 {
            return Err(NativeError::Oversize);
        }
        let mut buffer = vec![
            0u64;
            input_length
                .max(std::mem::size_of::<REPARSE_DATA_BUFFER>())
                .div_ceil(8)
        ];
        let pointer = buffer.as_mut_ptr().cast::<REPARSE_DATA_BUFFER>();
        // SAFETY: aligned owned buffer covers the SDK header/union and both counted names;
        // variable bytes contain only the sealed fixture's own path, with trailing NULs.
        unsafe {
            (*pointer).ReparseTag = IO_REPARSE_TAG_MOUNT_POINT;
            (*pointer).ReparseDataLength = data_length as u16;
            (*pointer)
                .Anonymous
                .MountPointReparseBuffer
                .SubstituteNameLength = name_bytes as u16;
            (*pointer).Anonymous.MountPointReparseBuffer.PrintNameOffset = (name_bytes + 2) as u16;
            std::ptr::copy_nonoverlapping(
                target.as_ptr(),
                buffer.as_mut_ptr().cast::<u8>().add(16).cast::<u16>(),
                target.len(),
            );
        }
        deadline.check()?;
        let mut returned = 0;
        // SAFETY: exact creator handle/ID, own empty scratch directory; public SDK buffer.
        // No privilege change, profile/parent mutation, path-based create or race reproduction.
        let set = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                FSCTL_SET_REPARSE_POINT,
                buffer.as_ptr().cast(),
                input_length as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        let set_error = if set == 0 { Some(last_error()) } else { None };
        let refusal = if set != 0 {
            match Anchor::open(
                &format!("{}\\{}", parent.path, created.name.as_str()),
                security,
                true,
                deadline,
            ) {
                Err(NativeError::Foreign) => Ok(true),
                Ok(Some(_)) => Ok(false),
                Ok(None) => Err(NativeError::OutcomeUnknown),
                Err(error) => Err(error),
            }
        } else {
            Ok(false)
        };
        let found = observe(&file, created.name.as_str(), security)?;
        if found.identity != created.identity {
            return Err(NativeError::OutcomeUnknown);
        }
        if found.reparse {
            let mut clear = [0u64; 1];
            // SAFETY: the documented delete-reparse input is only the SDK's 8-byte fixed header.
            unsafe {
                clear
                    .as_mut_ptr()
                    .cast::<u32>()
                    .write(IO_REPARSE_TAG_MOUNT_POINT);
            }
            // SAFETY: SAME retained creator handle/ID, clearing only our own static fixture tag;
            // attempted once even if the observation budget expired. Caller timeout stays unknown.
            if unsafe {
                DeviceIoControl(
                    file.as_raw_handle(),
                    FSCTL_DELETE_REPARSE_POINT,
                    clear.as_ptr().cast(),
                    8,
                    std::ptr::null_mut(),
                    0,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                return Err(NativeError::OutcomeUnknown);
            }
            if observe(&file, created.name.as_str(), security)?.reparse {
                return Err(NativeError::OutcomeUnknown);
            }
        }
        deadline.check()?;
        if let Some(error) = set_error {
            return Err(error);
        }
        refusal
    }
}
