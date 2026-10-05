//! Narrow Win32 identity and filesystem security adapter. No elevation or impersonation.

use std::cell::RefCell;
use std::ffi::{OsStr, c_void};
use std::io::{self, Read, Write};
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Component, Path, PathBuf, Prefix};
use std::ptr;

use anyhow::{Context, Result, ensure};
use tokio::net::windows::named_pipe::NamedPipeClient;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation, CONTAINER_INHERIT_ACE,
    DACL_SECURITY_INFORMATION, GetAce, GetAclInformation, GetSecurityDescriptorControl,
    GetTokenInformation, IsValidAcl, IsValidSecurityDescriptor, IsValidSid, OBJECT_INHERIT_ACE,
    OWNER_SECURITY_INFORMATION, PSID, SE_DACL_PRESENT, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES,
    SID_AND_ATTRIBUTES, TOKEN_GROUPS, TOKEN_INFORMATION_CLASS, TOKEN_QUERY, TOKEN_USER,
    TokenLogonSid, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateDirectoryW, CreateFileW, DELETE, FILE_ALL_ACCESS, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
    FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_READ_ATTRIBUTES,
    FILE_READ_DATA, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_DATA, FileAttributeTagInfo,
    FileDispositionInfo, GetFileInformationByHandleEx, GetVolumeInformationByHandleW,
    OPEN_EXISTING, READ_CONTROL, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, SYNCHRONIZE,
    SetFileInformationByHandle,
};
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows_sys::Win32::System::SystemServices::{ACCESS_ALLOWED_ACE_TYPE, FILE_PERSISTENT_ACLS};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};
use xxhash_rust::xxh3::xxh3_128;

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns exactly one valid, non-pseudo Win32 handle.
        unsafe { CloseHandle(self.0) };
    }
}

struct LocalAllocation(*mut c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        // SAFETY: Windows allocated this pointer with LocalAlloc; it is released exactly once.
        unsafe { LocalFree(self.0) };
    }
}

/// Canonical SID strings obtained from a primary process token, never from environment variables.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Identity {
    user: String,
    logon: String,
}

fn process_token(process: HANDLE) -> Result<Handle> {
    let mut token = ptr::null_mut();
    // SAFETY: process is a live process handle; token points to initialized writable storage.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error()).context("query process token");
    }
    Ok(Handle(token))
}

struct TokenInfo {
    // usize gives every token structure and embedded SID its required alignment.
    words: Vec<usize>,
    len: usize,
}

impl TokenInfo {
    fn query(token: &Handle, class: TOKEN_INFORMATION_CLASS) -> Result<Self> {
        let mut needed = 0;
        // SAFETY: zero length/null buffer is the documented size query; needed is writable.
        unsafe { GetTokenInformation(token.0, class, ptr::null_mut(), 0, &mut needed) };
        ensure!(
            needed > 0 && needed <= 65536,
            "invalid token information length"
        );
        let mut words = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
        let capacity = words.len() * size_of::<usize>();
        let mut returned = 0;
        // SAFETY: words is aligned, initialized and contains at least needed writable bytes.
        if unsafe {
            GetTokenInformation(
                token.0,
                class,
                words.as_mut_ptr().cast(),
                needed,
                &mut returned,
            )
        } == 0
        {
            return Err(io::Error::last_os_error()).context("read token information");
        }
        ensure!(
            returned > 0 && returned as usize <= capacity,
            "invalid token reply length"
        );
        Ok(Self {
            words,
            len: returned as usize,
        })
    }

    fn read<T: Copy>(&self, offset: usize) -> Result<T> {
        ensure!(
            offset
                .checked_add(size_of::<T>())
                .is_some_and(|end| end <= self.len),
            "truncated token information"
        );
        // SAFETY: the checked byte range is inside this initialized token buffer. Unaligned
        // reading avoids relying on offsets supplied by the native variable-length layout.
        Ok(unsafe { ptr::read_unaligned(self.words.as_ptr().cast::<u8>().add(offset).cast()) })
    }

    fn sid(&self, sid: PSID) -> Result<String> {
        let start = self.words.as_ptr() as usize;
        let offset = (sid as usize)
            .checked_sub(start)
            .context("SID is outside token buffer")?;
        ensure!(
            offset.checked_add(8).is_some_and(|end| end <= self.len),
            "truncated SID header"
        );
        let subauthorities: u8 = self.read(offset + 1)?;
        ensure!(subauthorities <= 15, "invalid SID subauthority count");
        ensure!(
            offset
                .checked_add(8 + 4 * usize::from(subauthorities))
                .is_some_and(|end| end <= self.len),
            "truncated token SID"
        );
        sid_string(sid)
    }
}

fn sid_string(sid: PSID) -> Result<String> {
    ensure!(!sid.is_null(), "invalid security SID");
    // SAFETY: callers supply a complete SID in a live token/security-descriptor allocation.
    let valid = unsafe { IsValidSid(sid) };
    ensure!(valid != 0, "invalid security SID");
    let mut text = ptr::null_mut();
    // SAFETY: sid is validated; text is an output pointer owned by LocalAllocation on success.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error()).context("format security SID");
    }
    let allocation = LocalAllocation(text.cast());
    let mut len = 0;
    // A valid SID has at most 15 u32 subauthorities, so its canonical text is below 256 UTF-16
    // units. The native conversion returns a terminated allocation for exactly that text.
    while len < 256 {
        // SAFETY: the native conversion returned a terminated SID string; the loop stops at NUL.
        if unsafe { *text.add(len) } == 0 {
            break;
        }
        len += 1;
    }
    ensure!(len < 256, "invalid SID string length");
    // SAFETY: len units precede the validated terminator in the live LocalAlloc allocation.
    let result = String::from_utf16(unsafe { std::slice::from_raw_parts(text, len) })
        .context("invalid SID string")?;
    drop(allocation);
    Ok(result)
}

fn identity(process: HANDLE) -> Result<Identity> {
    let token = process_token(process)?;
    let user = TokenInfo::query(&token, TokenUser)?;
    let user_entry: TOKEN_USER = user.read(0)?;
    let logon = TokenInfo::query(&token, TokenLogonSid)?;
    let count: u32 = logon.read(0)?;
    ensure!(count == 1, "process token has no unique logon SID");
    let entry: SID_AND_ATTRIBUTES = logon.read(offset_of!(TOKEN_GROUPS, Groups))?;
    Ok(Identity {
        user: user.sid(user_entry.User.Sid)?,
        logon: logon.sid(entry.Sid)?,
    })
}

pub(super) fn current_identity() -> Result<Identity> {
    // SAFETY: GetCurrentProcess returns the always-valid current-process pseudo handle.
    identity(unsafe { GetCurrentProcess() })
}

fn verify_server(pipe: &NamedPipeClient, expected: &Identity) -> Result<()> {
    let mut pid = 0;
    // SAFETY: pipe owns a connected client handle; pid is a writable output value.
    if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle().cast(), &mut pid) } == 0 {
        return Err(io::Error::last_os_error()).context("identify control server");
    }
    ensure!(pid != 0, "control server has no process identity");
    // SAFETY: query-only, non-inheritable open of the PID reported by the connected pipe.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(io::Error::last_os_error()).context("query control server identity");
    }
    let process = Handle(process);
    ensure!(
        identity(process.0)? == *expected,
        "control server identity does not match this user and logon"
    );
    Ok(())
}

struct Descriptor(LocalAllocation);

impl Descriptor {
    fn from_sddl(sddl: &str) -> Result<Self> {
        let text = wide(OsStr::new(sddl))?;
        let mut descriptor = ptr::null_mut();
        // SAFETY: text is NUL-terminated; output descriptor is immediately given RAII ownership.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error()).context("create private security descriptor");
        }
        Ok(Self(LocalAllocation(descriptor)))
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0.0,
            bInheritHandle: 0,
        }
    }
}

/// Open only the generated local endpoint, with individual write rights. GENERIC_WRITE also
/// requests FILE_CREATE_PIPE_INSTANCE, which a control client neither needs nor should request.
pub(super) fn open_client(path: &Path, expected: &Identity) -> Result<NamedPipeClient> {
    let name = wide(path.as_os_str())?;
    // SAFETY: name is terminated; the pipe is opened asynchronously, non-inheritable and with
    // identification-only SQOS (the server cannot impersonate this client's capabilities).
    let raw = unsafe {
        CreateFileW(
            name.as_ptr(),
            FILE_GENERIC_READ | FILE_WRITE_DATA | SYNCHRONIZE,
            0,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error()).context("open control pipe");
    }
    // SAFETY: the handle is valid, overlapped and exclusively transferred to Tokio. Tokio owns
    // and closes it on registration failure as well as on ordinary drop.
    let pipe =
        unsafe { NamedPipeClient::from_raw_handle(raw.cast()) }.context("register control pipe")?;
    verify_server(&pipe, expected)?;
    Ok(pipe)
}

fn wide(text: &OsStr) -> Result<Vec<u16>> {
    let mut units: Vec<u16> = text.encode_wide().collect();
    ensure!(!units.contains(&0), "path contains a NUL");
    units.push(0);
    Ok(units)
}

fn local_path(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "private path must be absolute");
    ensure!(
        matches!(path.components().next(), Some(Component::Prefix(prefix))
        if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))),
        "private path must be on a local drive"
    );
    ensure!(!path.components().any(|part| matches!(part, Component::Normal(name) if name.encode_wide().any(|unit| unit == b':' as u16))),
        "private path must not name an alternate data stream");
    ensure!(
        !path
            .components()
            .any(|part| matches!(part, Component::ParentDir)),
        "private path must not contain parent components"
    );
    Ok(())
}

fn open_path(path: &Path) -> Result<Handle> {
    let text = wide(path.as_os_str())?;
    // Omit FILE_SHARE_DELETE: while these handles are held, checked ancestors/leaf cannot be
    // renamed or replaced. READ_DATA/LIST_DIRECTORY participates in sharing checks; attributes
    // alone do not. OPEN_REPARSE_POINT prevents following a reparse leaf.
    // SAFETY: text is terminated; no security/template pointers are used; the returned handle
    // is immediately placed in RAII ownership on success.
    let raw = unsafe {
        CreateFileW(
            text.as_ptr(),
            READ_CONTROL | FILE_READ_ATTRIBUTES | FILE_READ_DATA,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error())
            .context("open private path without following links");
    }
    let handle = Handle(raw);
    let attributes = file_attributes(&handle)?;
    ensure!(
        attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0,
        "private path contains a reparse point"
    );
    Ok(handle)
}

fn file_attributes(handle: &Handle) -> Result<u32> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: handle is live; info is correctly sized initialized writable output storage.
    if unsafe {
        GetFileInformationByHandleEx(
            handle.0,
            FileAttributeTagInfo,
            (&mut info as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error()).context("check private path attributes");
    }
    Ok(info.FileAttributes)
}

fn pin_path(path: &Path) -> Result<Vec<Handle>> {
    local_path(path)?;
    let mut pins = Vec::new();
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component.as_os_str());
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        pins.push(open_path(&prefix)?);
    }
    Ok(pins)
}

fn check_owner(handle: &Handle) -> Result<()> {
    let identity = current_identity()?;
    let mut owner = ptr::null_mut();
    let mut descriptor = ptr::null_mut();
    // SAFETY: handle is held without delete sharing; the native allocation contains the owner
    // SID and remains alive until the RAII descriptor is dropped.
    let status = unsafe {
        GetSecurityInfo(
            handle.0,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32)).context("read metadata owner");
    }
    let _allocation = LocalAllocation(descriptor);
    ensure!(
        sid_string(owner)? == identity.user,
        "metadata is owned by another user"
    );
    Ok(())
}

/// Read the known metadata leaf through a checked, pinned, no-follow handle.
pub(super) fn read_metadata(path: &Path) -> Result<String> {
    let pins = pin_path(path)?;
    let leaf = pins.last().context("metadata has no leaf")?;
    ensure!(
        file_attributes(leaf)? & FILE_ATTRIBUTE_DIRECTORY == 0,
        "metadata is a directory"
    );
    persistent_acls(leaf.0)?;
    check_owner(leaf)?;
    let text = wide(path.as_os_str())?;
    // SAFETY: every path component is pinned; the data handle opens only this no-follow leaf.
    let raw = unsafe {
        CreateFileW(
            text.as_ptr(),
            FILE_READ_DATA,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error()).context("read metadata");
    }
    // SAFETY: raw is a new, valid file handle exclusively transferred to std::fs::File.
    let file = unsafe { std::fs::File::from_raw_handle(raw.cast()) };
    let mut output = String::new();
    file.take(1024 * 1024 + 1)
        .read_to_string(&mut output)
        .context("read metadata text")?;
    ensure!(
        output.len() <= 1024 * 1024,
        "diagnostics metadata is too large"
    );
    Ok(output)
}

fn persistent_acls(handle: HANDLE) -> Result<()> {
    let mut flags = 0;
    // SAFETY: handle is a live local file/directory handle. Only the filesystem flags output
    // is requested; all optional name/serial outputs are null with zero buffer lengths.
    if unsafe {
        GetVolumeInformationByHandleW(
            handle,
            ptr::null_mut(),
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut flags,
            ptr::null_mut(),
            0,
        )
    } == 0
    {
        return Err(io::Error::last_os_error()).context("query diagnostics volume ACL support");
    }
    ensure!(
        flags & FILE_PERSISTENT_ACLS != 0,
        "diagnostics require a volume that preserves and enforces access-control lists"
    );
    Ok(())
}

/// Verify the kernel object's actual permissions, rather than assuming SECURITY_ATTRIBUTES
/// was honored. Only one full-access, explicit current-user ACE is accepted.
fn verify_private(handle: HANDLE, directory: bool, expected: &Identity) -> Result<()> {
    persistent_acls(handle)?;
    let mut owner = ptr::null_mut();
    let mut dacl = ptr::null_mut();
    let mut descriptor = ptr::null_mut();
    // SAFETY: handle is live; every requested output points to initialized storage. GetSecurityInfo
    // returns an owned native descriptor, and owner/dacl borrow its allocation below.
    let status = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32))
            .context("read actual diagnostics permissions");
    }
    let _allocation = LocalAllocation(descriptor);
    ensure!(
        !descriptor.is_null(),
        "invalid diagnostics security descriptor"
    );
    // SAFETY: a successful GetSecurityInfo supplies this live complete descriptor allocation.
    let valid = unsafe { IsValidSecurityDescriptor(descriptor) };
    ensure!(valid != 0, "invalid diagnostics security descriptor");
    let mut controls = 0;
    let mut revision = 0;
    // SAFETY: descriptor is validated and live; both output pointers are writable and correctly sized.
    if unsafe { GetSecurityDescriptorControl(descriptor, &mut controls, &mut revision) } == 0 {
        return Err(io::Error::last_os_error()).context("query diagnostics DACL protection");
    }
    ensure!(
        controls & (SE_DACL_PRESENT | SE_DACL_PROTECTED) == (SE_DACL_PRESENT | SE_DACL_PROTECTED),
        "diagnostics DACL must be present and protected from inheritance"
    );
    ensure!(
        sid_string(owner)? == expected.user,
        "diagnostics owner does not match the current user"
    );
    ensure!(!dacl.is_null(), "invalid diagnostics DACL");
    // SAFETY: GetSecurityInfo returned dacl within the live descriptor allocation.
    let valid = unsafe { IsValidAcl(dacl) };
    ensure!(valid != 0, "invalid diagnostics DACL");
    let mut size = ACL_SIZE_INFORMATION::default();
    // SAFETY: dacl is validated; size is a correctly sized, initialized output structure.
    if unsafe {
        GetAclInformation(
            dacl,
            (&mut size as *mut ACL_SIZE_INFORMATION).cast(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    } == 0
    {
        return Err(io::Error::last_os_error()).context("query diagnostics DACL entries");
    }
    ensure!(
        size.AceCount == 1,
        "diagnostics DACL must contain exactly one user allow entry"
    );
    let mut ace = ptr::null_mut();
    // SAFETY: the validated ACL contains exactly one ACE; ace is writable output storage.
    if unsafe { GetAce(dacl, 0, &mut ace) } == 0 {
        return Err(io::Error::last_os_error()).context("read diagnostics allow entry");
    }
    // SAFETY: IsValidAcl validated the complete ACL header in the live descriptor allocation.
    let acl = unsafe { ptr::read_unaligned(dacl.cast::<ACL>()) };
    let offset = (ace as usize)
        .checked_sub(dacl as usize)
        .context("diagnostics ACE is outside DACL")?;
    ensure!(
        offset >= size_of::<ACL>()
            && offset
                .checked_add(size_of::<ACCESS_ALLOWED_ACE>())
                .is_some_and(|end| end <= usize::from(acl.AclSize)),
        "truncated diagnostics allow entry"
    );
    // SAFETY: the whole fixed ACE prefix lies within the validated live ACL allocation.
    let entry = unsafe { ptr::read_unaligned(ace.cast::<ACCESS_ALLOWED_ACE>()) };
    let flags = if directory {
        (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) as u8
    } else {
        0
    };
    ensure!(
        u32::from(entry.Header.AceType) == ACCESS_ALLOWED_ACE_TYPE
            && entry.Header.AceFlags == flags,
        "diagnostics permissions contain an unexpected or inherited ACE"
    );
    ensure!(
        entry.Mask == FILE_ALL_ACCESS,
        "diagnostics user does not have the exact private access mask"
    );
    let sid_offset = offset_of!(ACCESS_ALLOWED_ACE, SidStart);
    let ace_len = usize::from(entry.Header.AceSize);
    ensure!(
        offset
            .checked_add(ace_len)
            .is_some_and(|end| end <= usize::from(acl.AclSize))
            && ace_len >= sid_offset + 8,
        "truncated diagnostics user SID"
    );
    // SAFETY: the checked ACE contains the complete SID header. Its subauthority count is at
    // byte one; validate the full variable-size SID range before calling native SID routines.
    let count = unsafe { *ace.cast::<u8>().add(sid_offset + 1) };
    ensure!(
        count <= 15 && ace_len == sid_offset + 8 + 4 * usize::from(count),
        "invalid diagnostics user SID length"
    );
    // SAFETY: the preceding bounds checks establish the whole SID within the live ACE allocation.
    let sid = unsafe { ace.cast::<u8>().add(sid_offset).cast() };
    ensure!(
        sid_string(sid)? == expected.user,
        "diagnostics allow entry grants access to another identity"
    );
    Ok(())
}

fn with_cleanup<T>(error: anyhow::Error, cleanup: Result<()>) -> Result<T> {
    match cleanup {
        Ok(()) => Err(error),
        Err(cleanup) => Err(error.context(format!("diagnostics cleanup also failed: {cleanup:#}"))),
    }
}

fn mark_deleted(handle: HANDLE) -> Result<()> {
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: handle is the retained, owned file/directory handle opened with DELETE access.
    // The correctly sized initialized disposition structure applies only to that exact kernel
    // object. The handle remains open until the disposition is accepted; no path is reopened.
    if unsafe {
        SetFileInformationByHandle(
            handle,
            FileDispositionInfo,
            (&info as *const FILE_DISPOSITION_INFO).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error())
            .context("mark owned diagnostics object for deletion");
    }
    Ok(())
}

/// Call only below already-held, admitted private parent pins. Unlike an ancestry pin, this
/// leaf requests DELETE, so its exact object can later be removed without any pathname race.
fn open_delete_leaf(path: &Path) -> Result<Handle> {
    let text = wide(path.as_os_str())?;
    // SAFETY: text is terminated; the parent path is already pinned. The new non-inheritable
    // handle has DELETE and excludes delete sharing. OPEN_REPARSE_POINT opens a link itself.
    let raw = unsafe {
        CreateFileW(
            text.as_ptr(),
            READ_CONTROL | FILE_READ_ATTRIBUTES | DELETE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error()).context("open owned diagnostics deletion handle");
    }
    Ok(Handle(raw))
}

/// A newly created staging directory, pinned with all its ancestors until handle-bound cleanup.
pub(super) struct PrivateDirectory {
    path: PathBuf,
    leaf: Option<Handle>,
    _parents: Vec<Handle>,
    identity: Identity,
    payloads: RefCell<Vec<PathBuf>>,
    cleaned: bool,
}

impl PrivateDirectory {
    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    /// Give tar only known created payloads, relative to the admitted runtime parent. Avoid
    /// recursive traversal/reopening of the DELETE-bearing staging directory itself.
    pub(super) fn tar_members(&self) -> Result<Vec<PathBuf>> {
        let directory = self
            .path
            .file_name()
            .context("staging directory has no name")?;
        self.payloads
            .try_borrow()
            .context("diagnostics payload list is in use")?
            .iter()
            .map(|path| {
                Ok(PathBuf::from(directory)
                    .join(path.file_name().context("payload has no filename")?))
            })
            .collect()
    }

    /// Never repin this DELETE-bearing leaf: a second no-delete-share open would conflict with
    /// it. This guard already holds the admitted private directory and every ancestor.
    pub(super) fn write(&self, name: &str, bytes: &[u8]) -> Result<()> {
        ensure!(
            matches!(
                Path::new(name).components().next(),
                Some(Component::Normal(_))
            ) && Path::new(name).components().count() == 1,
            "diagnostics filename must be one leaf"
        );
        let path = self.path.join(name);
        local_path(&path)?;
        let mut file = create_owned_file(&path, self.identity.clone(), Vec::new())?;
        // Track only leaves successfully created by this invocation, including a leaf whose
        // subsequent write/flush fails. Unknown objects in the directory are never traversed.
        match self.payloads.try_borrow_mut() {
            Ok(mut payloads) => payloads.push(path),
            Err(error) => return with_cleanup(error.into(), file.abort()),
        }
        if let Err(error) = file
            .file_mut()?
            .write_all(bytes)
            .context("write private diagnostics payload")
        {
            return with_cleanup(error, file.abort());
        }
        // Payload handles close before tar reads them. Their exact-user private parent remains
        // pinned; it prevents foreign DELETE_CHILD substitution during the read/cleanup gap.
        file.commit()
    }

    fn cleanup(&mut self) -> Result<()> {
        let leaf = self
            .leaf
            .as_ref()
            .context("staging has no retained created-object handle; refuse path cleanup")?;
        verify_private(leaf.0, true, &self.identity)?;
        ensure!(
            file_attributes(leaf)? & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT)
                == FILE_ATTRIBUTE_DIRECTORY,
            "staging deletion target is not the admitted directory"
        );
        while let Some(path) = self.payloads.get_mut().pop() {
            let removed = self.remove_payload(&path);
            if let Err(error) = removed {
                self.payloads.get_mut().push(path);
                return Err(error);
            }
        }
        // The staging leaf stays pinned while all owned payloads are removed. An unexpected
        // child makes this empty-directory disposition fail; no recursive/path fallback exists.
        let leaf = self
            .leaf
            .as_ref()
            .context("staging deletion handle is closed")?;
        mark_deleted(leaf.0)?;
        self.cleaned = true;
        drop(self.leaf.take());
        Ok(())
    }

    fn remove_payload(&self, path: &Path) -> Result<()> {
        let handle = match open_delete_leaf(path) {
            Ok(handle) => handle,
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        ensure!(
            file_attributes(&handle)? & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT)
                == 0,
            "owned diagnostics payload was replaced by a directory or reparse point"
        );
        verify_private(handle.0, false, &self.identity)?;
        mark_deleted(handle.0)?;
        // Close only after native disposition binds deletion to the checked child object.
        drop(handle);
        Ok(())
    }

    pub(super) fn finish(mut self) -> Result<()> {
        self.cleanup()
    }
}

impl Drop for PrivateDirectory {
    fn drop(&mut self) {
        if !self.cleaned {
            let _ = self.cleanup();
        }
    }
}

/// Require an already-existing, protected exact-user private parent before native directory
/// creation. This prevents foreign DELETE_CHILD substitution before the first leaf open.
/// Neither AppData nor an arbitrary user-supplied parent ever has its ACL changed here.
pub(super) fn create_private_directory(path: &Path) -> Result<PrivateDirectory> {
    local_path(path)?;
    let parent = path.parent().context("private directory has no parent")?;
    let pins = pin_path(parent)?;
    let identity = current_identity()?;
    let parent_handle = pins
        .last()
        .context("private directory has no parent handle")?;
    ensure!(
        file_attributes(parent_handle)? & FILE_ATTRIBUTE_DIRECTORY != 0,
        "diagnostics staging parent is not a directory"
    );
    verify_private(parent_handle.0, true, &identity)
        .context("diagnostics staging requires an admitted private runtime")?;
    let descriptor = Descriptor::from_sddl(&format!(
        "O:{}D:P(A;OICI;FA;;;{})",
        identity.user, identity.user
    ))?;
    let text = wide(path.as_os_str())?;
    let attributes = descriptor.attributes();
    // SAFETY: text/attributes/descriptor remain live. The admitted protected parent stays pinned
    // across creation and first open, and CreateDirectory refuses an existing leaf.
    if unsafe { CreateDirectoryW(text.as_ptr(), &attributes) } == 0 {
        return Err(io::Error::last_os_error()).context("create private diagnostics directory");
    }
    let leaf = open_delete_leaf(path).context(
        "could not retain the new empty staging directory; leaving its path untouched rather than deleting by name")?;
    let mut directory = PrivateDirectory {
        path: path.to_owned(),
        leaf: Some(leaf),
        _parents: pins,
        identity,
        payloads: RefCell::new(Vec::new()),
        cleaned: false,
    };
    let verification = directory
        .leaf
        .as_ref()
        .context("new staging handle is absent")
        .and_then(|leaf| {
            ensure!(
                file_attributes(leaf)? & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT)
                    == FILE_ATTRIBUTE_DIRECTORY,
                "new staging object is not a plain directory"
            );
            verify_private(leaf.0, true, &directory.identity)
        });
    if let Err(error) = verification {
        // No payload bytes or child objects exist yet. The admitted private parent established
        // this is our newly created object; delete only via the retained first-open handle.
        let cleanup = directory
            .leaf
            .as_ref()
            .context("new staging handle is absent")
            .and_then(|leaf| mark_deleted(leaf.0));
        if cleanup.is_ok() {
            directory.cleaned = true;
            drop(directory.leaf.take());
        }
        return with_cleanup(error, cleanup);
    }
    Ok(directory)
}

/// A CREATE_NEW output file; its original DELETE-bearing handle is retained through streaming
/// and cleanup. No pathname reopen/removal can delete a replacement supplied by DELETE_CHILD.
pub(super) struct PrivateFile {
    file: Option<std::fs::File>,
    _parents: Vec<Handle>,
    identity: Identity,
    preserve: bool,
}

impl PrivateFile {
    fn file_mut(&mut self) -> Result<&mut std::fs::File> {
        self.file
            .as_mut()
            .context("private diagnostics file is closed")
    }

    /// Stream tar stdout into the already-created/verified object; tar never reopens its path.
    pub(super) fn copy_from(&mut self, source: &mut impl Read) -> Result<u64> {
        io::copy(source, self.file_mut()?).context("stream archive into private diagnostics output")
    }

    fn cleanup(&mut self) -> Result<()> {
        let file = self
            .file
            .as_ref()
            .context("private diagnostics deletion handle is closed")?;
        mark_deleted(file.as_raw_handle().cast())?;
        self.preserve = true;
        drop(self.file.take());
        Ok(())
    }

    pub(super) fn commit(mut self) -> Result<()> {
        let file = self
            .file
            .as_ref()
            .context("private diagnostics file is closed")?;
        if let Err(error) = verify_private(file.as_raw_handle().cast(), false, &self.identity)
            .and_then(|()| file.sync_all().context("flush private diagnostics output"))
        {
            let cleanup = self.cleanup();
            return with_cleanup(error, cleanup);
        }
        self.preserve = true;
        Ok(())
    }

    pub(super) fn abort(mut self) -> Result<()> {
        self.cleanup()
    }
}

impl Drop for PrivateFile {
    fn drop(&mut self) {
        if !self.preserve {
            let _ = self.cleanup();
        }
    }
}

pub(super) fn create_private_file(path: &Path) -> Result<PrivateFile> {
    local_path(path)?;
    let parent = path.parent().context("private file has no parent")?;
    let pins = pin_path(parent)?;
    persistent_acls(pins.last().context("private file has no parent handle")?.0)?;
    create_owned_file(path, current_identity()?, pins)
}

/// Parent pins are supplied either by create_private_file or its live enclosing staging guard.
/// Only this helper transfers a freshly created output handle into PrivateFile cleanup authority.
fn create_owned_file(path: &Path, identity: Identity, pins: Vec<Handle>) -> Result<PrivateFile> {
    let descriptor = Descriptor::from_sddl(&format!(
        "O:{}D:P(A;;FA;;;{})",
        identity.user, identity.user
    ))?;
    let attributes = descriptor.attributes();
    let text = wide(path.as_os_str())?;
    // SAFETY: text/attributes/descriptor are live. CREATE_NEW gives direct ownership proof of
    // this new object; DELETE enables final removal through this same handle, without reopening
    // the path. No handle is inherited and delete sharing is withheld for the handle's lifetime.
    let raw = unsafe {
        CreateFileW(
            text.as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error()).context("create private diagnostics file");
    }
    // SAFETY: raw is a new valid file handle exclusively transferred to std::fs::File.
    let file = unsafe { std::fs::File::from_raw_handle(raw.cast()) };
    let output = PrivateFile {
        file: Some(file),
        _parents: pins,
        identity,
        preserve: false,
    };
    if let Err(error) = verify_private(raw, false, &output.identity) {
        return with_cleanup(error, output.abort());
    }
    Ok(output)
}

/// Keep the algorithm byte-for-byte equivalent in crosspanectl's independent adapter.
pub fn endpoint(runtime_dir: &Path) -> Result<PathBuf> {
    let pins = pin_path(runtime_dir)?;
    persistent_acls(
        pins.last()
            .context("control runtime has no directory handle")?
            .0,
    )?;
    let runtime =
        std::fs::canonicalize(runtime_dir).context("canonicalize control runtime directory")?;
    let identity = current_identity()?;
    Ok(endpoint_for(&identity, runtime.as_os_str()))
}

fn endpoint_for(identity: &Identity, runtime: &OsStr) -> PathBuf {
    let mut bytes = Vec::new();
    for text in [
        OsStr::new(&identity.user),
        OsStr::new(&identity.logon),
        runtime,
    ] {
        for unit in text.encode_wide() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes.extend_from_slice(&0u16.to_le_bytes());
    }
    PathBuf::from(format!(r"\\.\pipe\Crosspane-{:032x}", xxh3_128(&bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_endpoint_separates_users_logons_and_runtime_directories() {
        let identity = Identity {
            user: "S-1-5-21-1".into(),
            logon: "S-1-5-5-1-2".into(),
        };
        let base = endpoint_for(&identity, OsStr::new(r"C:\scratch\runtime"));
        assert_eq!(
            base,
            endpoint_for(&identity, OsStr::new(r"C:\scratch\runtime"))
        );
        assert_ne!(
            base,
            endpoint_for(&identity, OsStr::new(r"C:\scratch\other"))
        );
        let mut other = identity.clone();
        other.logon.push('3');
        assert_ne!(
            base,
            endpoint_for(&other, OsStr::new(r"C:\scratch\runtime"))
        );
        other = identity.clone();
        other.user.push('3');
        assert_ne!(
            base,
            endpoint_for(&other, OsStr::new(r"C:\scratch\runtime"))
        );
    }

    #[test]
    fn private_paths_reject_relative_parent_and_remote_names() {
        for path in [
            r"relative",
            r"C:\scratch\..\other",
            r"\\server\share\runtime",
            r"\\?\UNC\server\share",
        ] {
            assert!(local_path(Path::new(path)).is_err());
        }
        assert!(local_path(Path::new(r"C:\scratch\runtime")).is_ok());
    }

    struct FixtureParent {
        path: PathBuf,
        pin: Option<Handle>,
        _parents: Vec<Handle>,
        id: (u64, [u8; 16]),
        identity: Identity,
    }

    fn fixture_id(handle: &Handle) -> Result<(u64, [u8; 16])> {
        use windows_sys::Win32::Storage::FileSystem::{FILE_ID_INFO, FileIdInfo};
        let mut info = FILE_ID_INFO::default();
        // SAFETY: handle is live; info is initialized, aligned, correctly sized native output.
        if unsafe {
            GetFileInformationByHandleEx(
                handle.0,
                FileIdInfo,
                (&mut info as *mut FILE_ID_INFO).cast(),
                size_of::<FILE_ID_INFO>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error()).context("query owned scratch fixture identity");
        }
        Ok((info.VolumeSerialNumber, info.FileId.Identifier))
    }

    impl FixtureParent {
        fn new() -> Result<Self> {
            let base = std::env::temp_dir();
            let parents = pin_path(&base)?;
            persistent_acls(parents.last().context("fixture has no parent")?.0)?;
            let identity = current_identity()?;
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos();
            static NEXT_FIXTURE: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(1);
            let serial = NEXT_FIXTURE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = base.join(format!(
                "crosspane-ctl-owned-test-{}-{stamp}-{serial}",
                std::process::id()
            ));
            let descriptor = Descriptor::from_sddl(&format!(
                "O:{}D:P(A;OICI;FA;;;{})",
                identity.user, identity.user
            ))?;
            let text = wide(path.as_os_str())?;
            let attributes = descriptor.attributes();
            // SAFETY: this creates one unique empty test-owned leaf, with the live exact-user
            // descriptor. It never changes the temp parent's permissions or opens owner UI.
            if unsafe { CreateDirectoryW(text.as_ptr(), &attributes) } == 0 {
                return Err(io::Error::last_os_error()).context("create owned scratch fixture");
            }
            let pin = open_path(&path)?;
            verify_private(pin.0, true, &identity)?;
            let id = fixture_id(&pin)?;
            Ok(Self {
                path,
                pin: Some(pin),
                _parents: parents,
                id,
                identity,
            })
        }
    }

    impl Drop for FixtureParent {
        fn drop(&mut self) {
            // The fixture parent is an admission/query pin, so nested production helpers can
            // reopen it. Cleanup may reopen only after dropping it, then must match the exact
            // recorded volume/file ID; a substituted sibling/object is never deleted.
            drop(self.pin.take());
            let cleanup = || -> Result<()> {
                let handle = open_delete_leaf(&self.path)?;
                ensure!(
                    fixture_id(&handle)? == self.id,
                    "scratch fixture was substituted"
                );
                verify_private(handle.0, true, &self.identity)?;
                mark_deleted(handle.0)?;
                Ok(())
            };
            let _ = cleanup();
        }
    }

    #[test]
    fn retained_native_handles_deny_staging_and_archive_substitution() -> Result<()> {
        let fixture = FixtureParent::new()?;
        let staging_path = fixture.path.join("staging");
        let staging = create_private_directory(&staging_path)?;
        staging.write("payload.txt", b"owned staging fixture")?;
        assert!(std::fs::rename(&staging_path, fixture.path.join("replaced-stage")).is_err());
        let archive_path = fixture.path.join("archive.tar.gz");
        let mut archive = create_private_file(&archive_path)?;
        archive.copy_from(&mut io::Cursor::new(b"owned archive fixture"))?;
        assert!(std::fs::rename(&archive_path, fixture.path.join("replaced-archive")).is_err());
        archive.abort()?;
        staging.finish()?;
        assert!(!archive_path.exists());
        assert!(!staging_path.exists());
        Ok(())
    }

    #[test]
    fn native_abort_and_finish_preserve_an_unrelated_owned_sibling() -> Result<()> {
        let fixture = FixtureParent::new()?;
        let sibling_path = fixture.path.join("unrelated.txt");
        let mut sibling = create_private_file(&sibling_path)?;
        sibling.copy_from(&mut io::Cursor::new(b"unrelated owned sibling fixture"))?;
        let staging_path = fixture.path.join("staging");
        let staging = create_private_directory(&staging_path)?;
        staging.write("payload.txt", b"owned staging fixture")?;
        let archive_path = fixture.path.join("archive.tar.gz");
        let mut archive = create_private_file(&archive_path)?;
        archive.copy_from(&mut io::Cursor::new(b"owned archive fixture"))?;
        archive.abort()?;
        assert_eq!(
            std::fs::read(&sibling_path)?,
            b"unrelated owned sibling fixture"
        );
        staging.finish()?;
        assert_eq!(
            std::fs::read(&sibling_path)?,
            b"unrelated owned sibling fixture"
        );
        assert!(!archive_path.exists());
        assert!(!staging_path.exists());
        sibling.abort()?;
        Ok(())
    }
}
