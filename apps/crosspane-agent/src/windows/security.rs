//! Narrow Win32 identity and filesystem security adapter. No elevation or impersonation.

use std::ffi::{OsStr, c_void};
use std::io;
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Component, Path, PathBuf, Prefix};
use std::ptr;

use anyhow::{Context, Result, ensure};
use tokio::net::windows::named_pipe::{NamedPipeServer, PipeMode, ServerOptions};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_FILE_OBJECT, SetSecurityInfo,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, DACL_SECURITY_INFORMATION, GetAce, GetSecurityDescriptorControl,
    GetSecurityDescriptorDacl, GetTokenInformation, IsValidAcl, IsValidSecurityDescriptor,
    IsValidSid, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSID,
    SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, SID_AND_ATTRIBUTES, TOKEN_ELEVATION, TOKEN_GROUPS,
    TOKEN_INFORMATION_CLASS, TOKEN_QUERY, TOKEN_USER, TokenElevation, TokenLogonSid, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_WRITE,
    FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FileAttributeTagInfo, GetFileInformationByHandleEx, GetVolumeInformationByHandleW,
    OPEN_EXISTING, READ_CONTROL,
};
use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;
use windows_sys::Win32::System::SystemServices::{
    ACCESS_ALLOWED_ACE_TYPE, FILE_PERSISTENT_ACLS, MAXIMUM_ALLOWED,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};
use xxhash_rust::xxh3::xxh3_128;

#[derive(Debug)]
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

pub fn is_elevated() -> Result<bool> {
    // SAFETY: GetCurrentProcess returns the always-valid current-process pseudo handle.
    let token = process_token(unsafe { GetCurrentProcess() })?;
    let info = TokenInfo::query(&token, TokenElevation)?;
    let elevation: TOKEN_ELEVATION = info.read(0)?;
    Ok(elevation.TokenIsElevated != 0)
}

pub(super) fn verify_client(pipe: &NamedPipeServer, expected: &Identity) -> Result<()> {
    let mut pid = 0;
    // SAFETY: pipe owns a connected server handle; pid is a writable output value.
    if unsafe { GetNamedPipeClientProcessId(pipe.as_raw_handle().cast(), &mut pid) } == 0 {
        return Err(io::Error::last_os_error()).context("identify control client");
    }
    ensure!(pid != 0, "control client has no process identity");
    // SAFETY: query-only, non-inheritable open of the PID reported by the connected pipe.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(io::Error::last_os_error()).context("query control client identity");
    }
    let process = Handle(process);
    ensure!(
        identity(process.0)? == *expected,
        "control client identity does not match this user and logon"
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

pub(super) fn create_server(path: &Path, first: bool) -> Result<NamedPipeServer> {
    let identity = current_identity()?;
    // One logon ACE, NOT a union of user and logon ACEs. The owner is the exact current user;
    // both connected processes must additionally pass the exact-user+logon token check.
    // Full pipe access is needed by subsequent server instances, including CREATE_PIPE_INSTANCE.
    let descriptor = Descriptor::from_sddl(&format!(
        "O:{}D:P(A;;FA;;;{})",
        identity.user, identity.logon
    ))?;
    let mut attributes = descriptor.attributes();
    let mut options = ServerOptions::new();
    options
        .pipe_mode(PipeMode::Byte)
        .reject_remote_clients(true)
        .first_pipe_instance(first);
    // SAFETY: attributes and its backing descriptor remain live until CreateNamedPipe returns.
    // Tokio copies the descriptor into the new kernel object and owns the resulting handle.
    unsafe {
        options.create_with_security_attributes_raw(
            path,
            (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
        )
    }
    .context("create private control pipe")
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

fn open_path(path: &Path, modify: bool) -> Result<Handle> {
    let text = wide(path.as_os_str())?;
    // Omit FILE_SHARE_DELETE: while these handles are held, checked ancestors/leaf cannot be
    // renamed or replaced. READ_DATA/LIST_DIRECTORY participates in sharing checks; attributes
    // alone do not. OPEN_REPARSE_POINT prevents following a reparse leaf.
    // SAFETY: text is terminated; no security/template pointers are used; the returned handle
    // is immediately placed in RAII ownership on success.
    let raw = unsafe {
        CreateFileW(
            text.as_ptr(),
            if modify {
                MAXIMUM_ALLOWED | FILE_READ_DATA
            } else {
                READ_CONTROL | FILE_READ_ATTRIBUTES | FILE_READ_DATA
            },
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

fn pin_path(path: &Path, modify_leaf: bool) -> Result<Vec<Handle>> {
    local_path(path)?;
    let mut pins: Vec<Handle> = Vec::new();
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component.as_os_str());
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        pins.push(open_path(&prefix, modify_leaf && prefix == path)?);
    }
    Ok(pins)
}

/// Keeps already-admitted ancestry bound to its checked filesystem objects.
#[derive(Debug)]
pub(crate) struct ParentPins {
    _pins: Vec<Handle>,
}

/// Admission for an operation requires the final parent to exist. No ancestor ACLs change.
pub(crate) fn pin_parent(path: &Path) -> Result<ParentPins> {
    let parent = path.parent().context("private path has no parent")?;
    let pins = pin_path(parent, false)?;
    let leaf = pins.last().context("private file has no parent")?;
    ensure!(
        file_attributes(leaf)? & FILE_ATTRIBUTE_DIRECTORY != 0,
        "private file parent is not a directory"
    );
    persistent_acls(leaf.0)?;
    private_dacl(leaf.0, &current_identity()?.user, true)?;
    Ok(ParentPins { _pins: pins })
}

fn persistent_acls(handle: HANDLE) -> Result<()> {
    let mut flags = 0;
    // SAFETY: the live filesystem handle identifies the volume; only the flags output is requested.
    let status = unsafe {
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
    };
    ensure!(
        status != 0,
        "query private volume: {}",
        io::Error::last_os_error()
    );
    ensure!(
        flags & FILE_PERSISTENT_ACLS != 0,
        "private files require a filesystem with persistent ACLs"
    );
    Ok(())
}

fn private_dacl(handle: HANDLE, user: &str, directory: bool) -> Result<()> {
    ensure!(
        has_private_dacl(handle, user, directory)?,
        "private DACL does not grant exactly this user"
    );
    Ok(())
}

/// Query/open errors and a foreign owner are errors. A current-owned legacy ACL can be secured.
fn has_private_dacl(handle: HANDLE, user: &str, directory: bool) -> Result<bool> {
    let mut owner = ptr::null_mut();
    let mut dacl = ptr::null_mut();
    let mut descriptor = ptr::null_mut();
    // SAFETY: live checked handle; writable output pointers are backed by the returned allocation.
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
    ensure!(
        status == 0,
        "verify private DACL: {}",
        io::Error::from_raw_os_error(status as i32)
    );
    let _descriptor = LocalAllocation(descriptor);
    ensure!(
        !descriptor.is_null(),
        "private security descriptor is absent"
    );
    // SAFETY: GetSecurityInfo returned this complete descriptor in the held native allocation.
    let valid = unsafe { IsValidSecurityDescriptor(descriptor) };
    ensure!(valid != 0, "private security descriptor is invalid");
    ensure!(
        sid_string(owner)? == user,
        "private owner does not match this user"
    );
    if dacl.is_null() {
        return Ok(false);
    }
    // SAFETY: the validated descriptor owns this complete native ACL allocation.
    let valid = unsafe { IsValidAcl(dacl) };
    ensure!(valid != 0, "private ACL is invalid");
    let mut control = 0;
    let mut revision = 0;
    // SAFETY: GetSecurityInfo returned a valid native descriptor held until the end of this function.
    let status = unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) };
    ensure!(status != 0, "read private descriptor control");
    if control & SE_DACL_PROTECTED == 0 {
        return Ok(false);
    }
    // SAFETY: the native descriptor owns a kernel-validated ACL; its header is live and aligned.
    if unsafe { (*dacl).AceCount } != 1 {
        return Ok(false);
    }
    let mut ace = ptr::null_mut();
    // SAFETY: this live ACL has one entry; GetAce returns its pointer within the held descriptor.
    let status = unsafe { GetAce(dacl, 0, &mut ace) };
    ensure!(status != 0 && !ace.is_null(), "read private DACL entry");
    // SAFETY: a kernel-validated entry has an ACE_HEADER; check its type before reading allow fields.
    let header = unsafe { &*ace.cast::<windows_sys::Win32::Security::ACE_HEADER>() };
    if u32::from(header.AceType) != ACCESS_ALLOWED_ACE_TYPE
        || usize::from(header.AceSize) < size_of::<ACCESS_ALLOWED_ACE>()
    {
        return Ok(false);
    }
    // SAFETY: checked allow-ACE type/size; SidStart points to its kernel-validated embedded SID.
    let allow = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
    Ok(header.AceFlags == if directory { 3 } else { 0 }
        && allow.Mask == FILE_ALL_ACCESS
        && sid_string(ptr::addr_of!(allow.SidStart).cast_mut().cast())? == user)
}

fn secure_path(path: &Path, directory: bool) -> Result<Vec<Handle>> {
    let pins = pin_path(path, true)?;
    let leaf = pins.last().context("private path has no leaf")?;
    secure_handle(leaf, directory)?;
    Ok(pins)
}

fn secure_handle(leaf: &Handle, directory: bool) -> Result<()> {
    ensure!(
        (file_attributes(leaf)? & FILE_ATTRIBUTE_DIRECTORY != 0) == directory,
        "private path has the wrong file type"
    );
    persistent_acls(leaf.0)?;
    let identity = current_identity()?;
    let mut owner = ptr::null_mut();
    let mut descriptor = ptr::null_mut();
    // SAFETY: leaf is held without delete sharing; native descriptor and owner output pointers
    // remain owned/live until the owner has been inspected and the RAII allocation is dropped.
    let status = unsafe {
        GetSecurityInfo(
            leaf.0,
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
        return Err(io::Error::from_raw_os_error(status as i32)).context("read private path owner");
    }
    let original = LocalAllocation(descriptor);
    ensure!(
        sid_string(owner)? == identity.user,
        "refuse a private path owned by another user"
    );
    drop(original);
    let inheritance = if directory { "OICI" } else { "" };
    let replacement = Descriptor::from_sddl(&format!(
        "O:{}D:P(A;{inheritance};FA;;;{})",
        identity.user, identity.user
    ))?;
    let mut present = 0;
    let mut defaulted = 0;
    let mut dacl = ptr::null_mut();
    // SAFETY: replacement is a live native security descriptor; every output pointer is valid.
    if unsafe {
        GetSecurityDescriptorDacl(replacement.0.0, &mut present, &mut dacl, &mut defaulted)
    } == 0
    {
        return Err(io::Error::last_os_error()).context("read replacement DACL");
    }
    ensure!(present != 0 && !dacl.is_null(), "private DACL is absent");
    // SAFETY: the checked leaf handle and backing DACL are live; only this held leaf's DACL
    // changes. MAXIMUM_ALLOWED on this handle suppresses propagation to existing children
    // (documented SetSecurityInfo behavior). New children inherit the private ACE, but no
    // existing child, parent permission or ownership is changed.
    let status = unsafe {
        SetSecurityInfo(
            leaf.0,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            dacl,
            ptr::null(),
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32))
            .context("protect private path DACL");
    }
    private_dacl(leaf.0, &identity.user, directory)?;
    Ok(())
}

/// Create missing components individually with private ACLs and pin each before descending.
/// Existing ancestors are only checked/pinned; only the final owned leaf's ACL is changed.
pub(crate) fn create_private_directory(path: &Path) -> Result<()> {
    local_path(path)?;
    let identity = current_identity()?;
    let descriptor = Descriptor::from_sddl(&format!(
        "O:{}D:P(A;OICI;FA;;;{})",
        identity.user, identity.user
    ))?;
    let attributes = descriptor.attributes();
    let mut pins: Vec<Handle> = Vec::new();
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component.as_os_str());
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        let final_leaf = prefix == path;
        let handle = match open_path(&prefix, final_leaf) {
            Ok(handle) => handle,
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) =>
            {
                let parent = pins.last().context("private directory has no parent")?;
                persistent_acls(parent.0)?;
                let text = wide(prefix.as_os_str())?;
                // SAFETY: all existing ancestors are pinned without delete sharing. The live
                // descriptor protects this one new component before it can contain any bytes.
                let created = unsafe { CreateDirectoryW(text.as_ptr(), &attributes) };
                if created == 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::AlreadyExists {
                        return Err(error).context("create private directory component");
                    }
                }
                // Pin and admit this component before creating another below it. If another
                // creator won the race, accept only the same protected current-user object.
                let handle = open_path(&prefix, final_leaf)?;
                persistent_acls(handle.0)?;
                private_dacl(handle.0, &identity.user, true)?;
                handle
            }
            Err(error) => return Err(error),
        };
        ensure!(
            file_attributes(&handle)? & FILE_ATTRIBUTE_DIRECTORY != 0,
            "private directory ancestry contains a non-directory"
        );
        if final_leaf {
            secure_handle(&handle, true)?;
        }
        pins.push(handle);
    }
    ensure!(!pins.is_empty(), "private directory has no leaf");
    Ok(())
}

/// Retains checked ancestors and a private regular leaf across a std read/open. Keeping the
/// no-delete leaf alive prevents replacement with a reparse point during that operation.
#[derive(Debug)]
pub(crate) struct PrivateFile {
    _pins: Vec<Handle>,
}

pub(crate) fn private_file(path: &Path) -> Result<Option<PrivateFile>> {
    let until = std::time::Instant::now() + std::time::Duration::from_millis(500);
    loop {
        match private_file_once(path) {
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.raw_os_error() == Some(32))
                    && std::time::Instant::now() < until =>
            {
                // A second process may still hold its legacy-hardening MAXIMUM_ALLOWED
                // handle. Retry only sharing violation; all security/query failures stop.
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            result => return result,
        }
    }
}

fn private_file_once(path: &Path) -> Result<Option<PrivateFile>> {
    let _parent = pin_parent(path)?;
    let pins = match pin_path(path, false) {
        Ok(pins) => pins,
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let leaf = pins.last().context("private file has no leaf")?;
    ensure!(
        file_attributes(leaf)? & FILE_ATTRIBUTE_DIRECTORY == 0,
        "private path has the wrong file type"
    );
    persistent_acls(leaf.0)?;
    if has_private_dacl(leaf.0, &current_identity()?.user, false)? {
        // Existing exact ACLs need no DELETE-bearing MAXIMUM_ALLOWED rewrite. Query pins
        // coexist across processes and remain held while the caller opens/reads this leaf.
        return Ok(Some(PrivateFile { _pins: pins }));
    }
    // Release the leaf before a modifying open; the admitted private parent remains pinned.
    drop(pins);
    secure_path(path, false).map(|pins| Some(PrivateFile { _pins: pins }))
}

/// A new lock gets an explicit TokenUser owner; an existing leaf is admitted before opening.
pub(crate) fn open_private_lock(path: &Path) -> Result<std::fs::File> {
    let _parents = pin_parent(path)?;
    loop {
        if let Some(_leaf) = private_file(path)? {
            return std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .context("open admitted private lock");
        }
        match create_private_file(path) {
            Ok(file) => return Ok(file),
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::AlreadyExists) => {}
            Err(error) => return Err(error),
        }
    }
}

/// Atomically create a private temp leaf. Its DACL is verified before the caller writes bytes.
pub(crate) fn create_private_file(path: &Path) -> Result<std::fs::File> {
    let _parents = pin_parent(path)?;
    let identity = current_identity()?;
    let descriptor = Descriptor::from_sddl(&format!(
        "O:{}D:P(A;;FA;;;{})",
        identity.user, identity.user
    ))?;
    let attributes = descriptor.attributes();
    let text = wide(path.as_os_str())?;
    // SAFETY: name/descriptor are live; CREATE_NEW never follows/replaces an existing leaf.
    // Delete sharing allows the caller's later atomic rename of this same open temp file.
    let raw = unsafe {
        CreateFileW(
            text.as_ptr(),
            FILE_GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error()).context("create private temp file");
    }
    // SAFETY: the new, valid native handle is transferred once to the owning std File.
    let file = unsafe { std::fs::File::from_raw_handle(raw.cast()) };
    if let Err(error) = persistent_acls(raw).and_then(|_| private_dacl(raw, &identity.user, false))
    {
        drop(file);
        std::fs::remove_file(path).context("remove rejected private temp file")?;
        return Err(error);
    }
    Ok(file)
}

/// Keep the algorithm byte-for-byte equivalent in crosspanectl's independent adapter.
pub fn endpoint(runtime_dir: &Path) -> Result<PathBuf> {
    let _pins = pin_path(runtime_dir, false)?;
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

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "crosspane-private-components-{}-{stamp}",
                std::process::id()
            ));
            create_private_directory(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn missing_components_are_private_and_pins_prevent_substitution() {
        let root = Scratch::new();
        let first = root.0.join("first");
        let second = first.join("second");
        let leaf = second.join("private");
        create_private_directory(&leaf).unwrap();
        let user = current_identity().unwrap().user;
        for path in [&first, &second, &leaf] {
            let pin = open_path(path, false).unwrap();
            private_dacl(pin.0, &user, true).unwrap();
        }
        let unrelated = root.0.join("unrelated");
        std::fs::write(&unrelated, b"fixture sibling").unwrap();
        let pins = pin_path(&leaf, false).unwrap();
        assert!(std::fs::rename(&first, root.0.join("substitute")).is_err());
        assert!(std::fs::rename(&leaf, second.join("substitute")).is_err());
        assert_eq!(std::fs::read(&unrelated).unwrap(), b"fixture sibling");
        drop(pins);
    }

    #[test]
    fn nonempty_journal_compaction_releases_only_the_leaf_pin() {
        use crosspane_input::{Held, journal::Journal};
        use crosspane_types::hid::HidUsage;
        use std::io::Write;
        let root = Scratch::new();
        let path = root.0.join("input.journal");
        let mut journal = crate::paths::open_journal(&path).unwrap();
        let held = Held::Key(HidUsage { page: 7, id: 4 });
        journal.record_down(held).unwrap();
        drop(journal);
        let record = std::fs::read(&path).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        for _ in 0..9000 {
            file.write_all(&record).unwrap();
        }
        drop(file);
        let before = std::fs::metadata(&path).unwrap().len();
        let pins = pin_parent(&path).unwrap();
        assert!(std::fs::rename(&root.0, root.0.with_extension("substitute")).is_err());
        let journal = crate::paths::open_journal(&path).unwrap();
        assert_eq!(journal.held().unwrap(), vec![held]);
        assert!(std::fs::metadata(&path).unwrap().len() < before);
        drop(journal);
        drop(pins);
    }

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
}
