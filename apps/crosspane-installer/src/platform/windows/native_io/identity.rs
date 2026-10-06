use super::{NativeError, NativeResult};

#[derive(Clone, PartialEq, Eq)]
pub struct Sid(Vec<u8>);
impl std::fmt::Debug for Sid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Sid")
    }
}
impl Sid {
    pub fn from_bytes(value: Vec<u8>) -> NativeResult<Self> {
        if value.len() < 12
            || value[0] != 1
            || value[1] == 0
            || value[1] > 15
            || value.len() != 8 + usize::from(value[1]) * 4
        {
            return Err(NativeError::Invalid);
        }
        Ok(Self(value))
    }
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.0
    }
    pub(crate) fn sddl(&self) -> String {
        let authority = self.0[2..8]
            .iter()
            .fold(0u64, |v, b| (v << 8) | u64::from(*b));
        let mut value = format!("S-1-{authority}");
        for sub in self.0[8..].as_chunks::<4>().0 {
            value.push('-');
            value.push_str(&u32::from_le_bytes([sub[0], sub[1], sub[2], sub[3]]).to_string());
        }
        value
    }
    fn is_logon(&self) -> bool {
        self.0.len() == 20 && self.0[2..8] == [0, 0, 0, 0, 0, 5] && self.0[8..12] == [5, 0, 0, 0]
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct TokenFacts {
    pub user: Sid,
    pub logon: Sid,
    pub session: u32,
    pub elevated: bool,
    pub integrity: u32,
    pub authentication_id: u64,
    pub impersonating: bool,
}
pub struct LimitedIdentity(TokenFacts);
impl std::fmt::Debug for TokenFacts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TokenFacts")
    }
}
impl std::fmt::Debug for LimitedIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LimitedIdentity")
    }
}
impl LimitedIdentity {
    /// A checked observation, not a target capability. Only the native factory can seal it.
    pub fn admit(facts: TokenFacts) -> NativeResult<Self> {
        if facts.elevated
            || facts.impersonating
            || facts.session == 0
            || !(0x2000..0x3000).contains(&facts.integrity)
            || facts.authentication_id == 0
            || !facts.logon.is_logon()
            || facts.user == facts.logon
        {
            return Err(NativeError::Unsupported);
        }
        Ok(Self(facts))
    }
    pub fn facts(&self) -> &TokenFacts {
        &self.0
    }
}

#[cfg(windows)]
pub(crate) mod native {
    use super::*;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{Foundation::*, Security::*, System::Threading::*};

    const MAX_TOKEN_BYTES: usize = 64 * 1024;
    struct TokenBuffer {
        words: Vec<usize>,
        length: usize,
    }
    impl TokenBuffer {
        fn query(token: &OwnedHandle, class: TOKEN_INFORMATION_CLASS) -> NativeResult<Self> {
            let mut needed = 0;
            // SAFETY: query-only retained token handle; zero-size query writes only needed.
            let first = unsafe {
                GetTokenInformation(
                    token.as_raw_handle(),
                    class,
                    std::ptr::null_mut(),
                    0,
                    &mut needed,
                )
            };
            if first != 0 || needed == 0 || needed as usize > MAX_TOKEN_BYTES {
                return Err(NativeError::Unavailable);
            }
            let mut value = Self {
                words: vec![0; (needed as usize).div_ceil(std::mem::size_of::<usize>())],
                length: needed as usize,
            };
            let allocated = needed;
            // SAFETY: pointer-aligned owned allocation has at least allocated writable bytes.
            if unsafe {
                GetTokenInformation(
                    token.as_raw_handle(),
                    class,
                    value.words.as_mut_ptr().cast(),
                    allocated,
                    &mut needed,
                )
            } == 0
                || needed > allocated
            {
                return Err(NativeError::Unavailable);
            }
            value.length = needed as usize;
            Ok(value)
        }
        fn header<T: Copy>(&self) -> NativeResult<T> {
            if self.length < std::mem::size_of::<T>() {
                return Err(NativeError::Unavailable);
            }
            // SAFETY: checked complete header range in owned buffer; unaligned read supports SDK layouts.
            Ok(unsafe { self.words.as_ptr().cast::<T>().read_unaligned() })
        }
        fn sid(&self, pointer: PSID) -> NativeResult<Sid> {
            let start = pointer as usize;
            let base = self.words.as_ptr() as usize;
            let offset = start.checked_sub(base).ok_or(NativeError::Unavailable)?;
            if offset.checked_add(8).is_none_or(|end| end > self.length) {
                return Err(NativeError::Unavailable);
            }
            // SAFETY: SID header was range checked against the live token allocation.
            let count = unsafe { pointer.cast::<u8>().add(1).read() } as usize;
            let length = 8usize
                .checked_add(count.checked_mul(4).ok_or(NativeError::Unavailable)?)
                .ok_or(NativeError::Unavailable)?;
            if offset
                .checked_add(length)
                .is_none_or(|end| end > self.length)
            {
                return Err(NativeError::Unavailable);
            }
            // SAFETY: entire counted SID lies inside this retained native token buffer.
            Sid::from_bytes(unsafe { std::slice::from_raw_parts(pointer.cast(), length) }.to_vec())
        }
    }
    /// Check the calling thread BEFORE dispatch as well as on the native worker. A newly
    /// created worker does not inherit caller impersonation, so worker-only checks are insufficient.
    pub(crate) fn refuse_impersonation() -> NativeResult<()> {
        let mut impersonated = std::ptr::null_mut();
        // SAFETY: only the calling thread's token is queried; no token mutation or impersonation.
        if unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut impersonated) } != 0 {
            // SAFETY: successful OpenThreadToken transferred one real handle to this owner.
            let _handle = unsafe { OwnedHandle::from_raw_handle(impersonated) };
            return Err(NativeError::Unsupported);
        }
        // SAFETY: immediately observes only the failed OpenThreadToken call.
        if unsafe { GetLastError() } != ERROR_NO_TOKEN {
            return Err(NativeError::Unavailable);
        }
        Ok(())
    }
    pub(crate) fn observe() -> NativeResult<TokenFacts> {
        refuse_impersonation()?;
        // SAFETY: query-only current-process pseudo handle, never transferred or closed.
        token_facts(unsafe { GetCurrentProcess() })
    }
    /// Query only the already-retained selected process; no caller-selected PID or token.
    pub(crate) fn observe_process(process: &OwnedHandle) -> NativeResult<TokenFacts> {
        refuse_impersonation()?;
        token_facts(process.as_raw_handle())
    }
    fn token_facts(process: HANDLE) -> NativeResult<TokenFacts> {
        let mut raw = std::ptr::null_mut();
        // SAFETY: query-only retained/pseudo process handle; valid writable token output.
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut raw) } == 0 {
            return Err(NativeError::Unavailable);
        }
        // SAFETY: successful OpenProcessToken transferred exactly one owned handle.
        let token = unsafe { OwnedHandle::from_raw_handle(raw) };
        let user_buffer = TokenBuffer::query(&token, TokenUser)?;
        let user = user_buffer.sid(user_buffer.header::<TOKEN_USER>()?.User.Sid)?;
        let groups = TokenBuffer::query(&token, TokenGroups)?;
        let count = groups.header::<TOKEN_GROUPS>()?.GroupCount as usize;
        let offset = std::mem::offset_of!(TOKEN_GROUPS, Groups);
        let extent = count
            .checked_mul(std::mem::size_of::<SID_AND_ATTRIBUTES>())
            .and_then(|n| n.checked_add(offset))
            .ok_or(NativeError::Unavailable)?;
        if count > 1024 || extent > groups.length {
            return Err(NativeError::Unavailable);
        }
        let mut logon = None;
        for index in 0..count {
            // SAFETY: checked counted array range; this is a copied SDK entry in the live buffer.
            let entry = unsafe {
                groups
                    .words
                    .as_ptr()
                    .cast::<u8>()
                    .add(offset + index * std::mem::size_of::<SID_AND_ATTRIBUTES>())
                    .cast::<SID_AND_ATTRIBUTES>()
                    .read_unaligned()
            };
            // SE_GROUP_LOGON_ID and SE_GROUP_ENABLED are public TOKEN_GROUPS flags.
            if entry.Attributes & 0xc0000000 == 0xc0000000
                && entry.Attributes & 4 != 0
                && logon.replace(groups.sid(entry.Sid)?).is_some()
            {
                return Err(NativeError::Unsupported);
            }
        }
        let integrity = TokenBuffer::query(&token, TokenIntegrityLevel)?;
        let integrity_sid =
            integrity.sid(integrity.header::<TOKEN_MANDATORY_LABEL>()?.Label.Sid)?;
        let bytes = integrity_sid.bytes();
        let last = bytes
            .get(bytes.len().saturating_sub(4)..)
            .ok_or(NativeError::Unavailable)?;
        let level = u32::from_le_bytes([last[0], last[1], last[2], last[3]]);
        let elevated = TokenBuffer::query(&token, TokenElevation)?
            .header::<TOKEN_ELEVATION>()?
            .TokenIsElevated
            != 0;
        if TokenBuffer::query(&token, TokenUIAccess)?.header::<u32>()? != 0
            || TokenBuffer::query(&token, TokenIsAppContainer)?.header::<u32>()? != 0
        {
            return Err(NativeError::Unsupported);
        }
        let stats = TokenBuffer::query(&token, TokenStatistics)?.header::<TOKEN_STATISTICS>()?;
        let authentication_id = (u64::from(stats.AuthenticationId.HighPart as u32) << 32)
            | u64::from(stats.AuthenticationId.LowPart);
        Ok(TokenFacts {
            user,
            logon: logon.ok_or(NativeError::Unsupported)?,
            session: TokenBuffer::query(&token, TokenSessionId)?.header::<u32>()?,
            elevated,
            integrity: level,
            authentication_id,
            impersonating: false,
        })
    }
    pub(crate) fn current() -> NativeResult<LimitedIdentity> {
        LimitedIdentity::admit(observe()?)
    }

    pub(crate) fn trusted_installer() -> NativeResult<Sid> {
        let name: Vec<u16> = "NT SERVICE\\TrustedInstaller"
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut sid_length = 0;
        let mut domain_length = 0;
        let mut kind = 0;
        // SAFETY: fixed local service identity, read-only bounded size query; no account mutation.
        unsafe {
            LookupAccountNameW(
                std::ptr::null(),
                name.as_ptr(),
                std::ptr::null_mut(),
                &mut sid_length,
                std::ptr::null_mut(),
                &mut domain_length,
                &mut kind,
            );
        }
        if sid_length == 0 || sid_length > 68 || domain_length == 0 || domain_length > 128 {
            return Err(NativeError::Unavailable);
        }
        let mut sid = vec![0u8; sid_length as usize];
        let mut domain = vec![0u16; domain_length as usize];
        let capacity = sid_length;
        // SAFETY: both writable buffers have their reported capacities; exact fixed lookup only.
        if unsafe {
            LookupAccountNameW(
                std::ptr::null(),
                name.as_ptr(),
                sid.as_mut_ptr().cast(),
                &mut sid_length,
                domain.as_mut_ptr(),
                &mut domain_length,
                &mut kind,
            )
        } == 0
            || sid_length > capacity
        {
            return Err(NativeError::Unavailable);
        }
        sid.truncate(sid_length as usize);
        let end = domain.iter().position(|v| *v == 0).unwrap_or(domain.len());
        if !String::from_utf16(&domain[..end])
            .map_err(|_| NativeError::Unavailable)?
            .eq_ignore_ascii_case("NT SERVICE")
        {
            return Err(NativeError::Foreign);
        }
        Sid::from_bytes(sid)
    }
}
