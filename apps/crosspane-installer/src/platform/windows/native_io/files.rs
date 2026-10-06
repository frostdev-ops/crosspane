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
    pub(crate) fn error(code: u32) -> NativeError {
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
            if !request.dont_reparse || !request.open_reparse_point {
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
                Attributes: OBJ_DONT_REPARSE,
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
            };
            // SAFETY: retained admitted parent handle; exactly one checked counted component;
            // public SDK structures/binding, strict open/create and no reparse following. No
            // case-insensitive, inherit, privilege-bypass, overwrite or full-path fallback.
            let status = unsafe {
                NtCreateFile(
                    &mut raw,
                    self.access | READ_CONTROL_ACCESS | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
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
                .map_err(|_| NativeError::Unavailable)?;
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
