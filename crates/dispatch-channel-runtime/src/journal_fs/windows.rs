use super::unsafe_storage;
use std::{
    ffi::c_void,
    fs::{File, OpenOptions},
    io,
    mem::{offset_of, size_of},
    os::windows::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::Path,
    ptr::null_mut,
};
use windows_sys::Win32::{
    Foundation::{
        ERROR_INSUFFICIENT_BUFFER, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree,
    },
    Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT,
        },
        DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetLengthSid, GetTokenInformation, IsValidAcl,
        IsValidSid, IsWellKnownSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
        SECURITY_ATTRIBUTES, SID, TOKEN_QUERY, TOKEN_USER, TokenUser, WinBuiltinAdministratorsSid,
        WinLocalSystemSid,
    },
    Storage::FileSystem::{
        CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_NORMAL,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, GetVolumeInformationByHandleW,
        READ_CONTROL,
    },
    System::{
        SystemServices::{ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE, FILE_PERSISTENT_ACLS},
        Threading::{GetCurrentProcess, OpenProcessToken},
    },
};

/// Own memory returned by security APIs, including their embedded SID/DACL
/// pointers. No pointer outlives this allocation.
struct LocalAllocation(*mut c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}

struct Account {
    // Token information contains pointers into this aligned, stable allocation.
    information: Vec<usize>,
}

impl Account {
    fn current() -> io::Result<Self> {
        let mut token = null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let mut length = 0;
        let result = unsafe {
            GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut length)
        };
        if result != 0
            || io::Error::last_os_error().raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32)
            || length < size_of::<TOKEN_USER>() as u32
        {
            return Err(unsafe_storage("cannot determine journal account SID"));
        }
        let mut information = vec![0usize; (length as usize).div_ceil(size_of::<usize>())];
        if unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                information.as_mut_ptr().cast(),
                length,
                &mut length,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let account = Self { information };
        if unsafe { IsValidSid(account.sid()) } == 0 {
            return Err(unsafe_storage("invalid journal account SID"));
        }
        Ok(account)
    }

    fn sid(&self) -> PSID {
        unsafe { (*self.information.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }

    fn trusted(&self, sid: PSID) -> bool {
        unsafe {
            EqualSid(self.sid(), sid) != 0
                || IsWellKnownSid(sid, WinLocalSystemSid) != 0
                || IsWellKnownSid(sid, WinBuiltinAdministratorsSid) != 0
        }
    }

    fn descriptor(&self, directory: bool) -> io::Result<LocalAllocation> {
        let mut text = null_mut();
        if unsafe { ConvertSidToStringSidW(self.sid(), &mut text) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let allocation = LocalAllocation(text.cast());
        let mut length = 0;
        while unsafe { *text.add(length) } != 0 {
            length += 1;
        }
        let sid = String::from_utf16(unsafe { std::slice::from_raw_parts(text, length) })
            .map_err(|_| unsafe_storage("invalid journal account SID text"))?;
        drop(allocation);
        let inheritance = if directory { "OICI" } else { "" };
        let sddl = format!(
            "O:{sid}D:P(A;{inheritance};FA;;;{sid})(A;{inheritance};FA;;;SY)(A;{inheritance};FA;;;BA)"
        );
        descriptor_from_sddl(&sddl)
    }
}

fn descriptor_from_sddl(sddl: &str) -> io::Result<LocalAllocation> {
    let text: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            text.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(LocalAllocation(descriptor))
}

fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(unsafe_storage("journal path contains NUL"));
    }
    // Alternate streams are not independent journal artifacts.
    if path
        .file_name()
        .is_some_and(|name| name.encode_wide().any(|unit| unit == u16::from(b':')))
    {
        return Err(unsafe_storage(
            "journal path must not name an alternate stream",
        ));
    }
    Ok(wide.into_iter().chain(Some(0)).collect())
}

fn security_attributes(descriptor: &LocalAllocation) -> SECURITY_ATTRIBUTES {
    SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    }
}

pub(super) fn create_directory(path: &Path) -> io::Result<()> {
    let descriptor = Account::current()?.descriptor(true)?;
    let attributes = security_attributes(&descriptor);
    if unsafe { CreateDirectoryW(wide_path(path)?.as_ptr(), &attributes) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn create_file(path: &Path) -> io::Result<File> {
    let descriptor = Account::current()?.descriptor(false)?;
    let attributes = security_attributes(&descriptor);
    let handle = unsafe {
        CreateFileW(
            wide_path(path)?.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_handle(handle) })
}

pub(super) fn open_directory_chain(path: &Path) -> io::Result<Vec<File>> {
    let mut ancestors: Vec<_> = path.ancestors().collect();
    ancestors.reverse();
    let mut handles = Vec::new();
    for ancestor in ancestors {
        let file = OpenOptions::new()
            .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(ancestor)?;
        let metadata = file.metadata()?;
        if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(unsafe_storage(
                "event journal path must not traverse a reparse point",
            ));
        }
        handles.push(file);
    }
    let directory = handles
        .last()
        .ok_or_else(|| unsafe_storage("journal directory handle is absent"))?;
    let mut flags = 0;
    if unsafe {
        GetVolumeInformationByHandleW(
            directory.as_raw_handle(),
            null_mut(),
            0,
            null_mut(),
            null_mut(),
            &mut flags,
            null_mut(),
            0,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if flags & FILE_PERSISTENT_ACLS == 0 {
        return Err(unsafe_storage(
            "event journal volume must support persistent ACLs",
        ));
    }
    validate_private(directory)?;
    Ok(handles)
}

pub(super) fn validate_private(file: &File) -> io::Result<()> {
    if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(unsafe_storage(
            "event journal artifact must not be a reparse point",
        ));
    }
    let account = Account::current()?;
    let mut owner = null_mut();
    let mut dacl: *mut ACL = null_mut();
    let mut descriptor = null_mut();
    let error = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    let _descriptor = LocalAllocation(descriptor);
    if owner.is_null()
        || unsafe { IsValidSid(owner) } == 0
        || unsafe { EqualSid(account.sid(), owner) } == 0
        || dacl.is_null()
        || unsafe { IsValidAcl(dacl) } == 0
    {
        return Err(unsafe_storage(
            "event journal must have an account-owned private DACL; provision a private directory",
        ));
    }
    for index in 0..unsafe { (*dacl).AceCount } {
        let mut pointer = null_mut();
        if unsafe { GetAce(dacl, u32::from(index), &mut pointer) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // IsValidAcl bounds every ACE inside the DACL; copies avoid relying on
        // ACE alignment.
        let header = unsafe { pointer.cast::<ACE_HEADER>().read_unaligned() };
        match u32::from(header.AceType) {
            ACCESS_DENIED_ACE_TYPE => {}
            ACCESS_ALLOWED_ACE_TYPE => {
                // IsValidAcl keeps every ACE inside the DACL; the SID bound is ours.
                let sid = unsafe { allowed_ace_sid(pointer, header.AceSize) }
                    .ok_or_else(|| unsafe_storage("invalid event journal ACL entry"))?;
                // The SID bound already proves AceSize covers the fixed ACE fields.
                let ace = unsafe { pointer.cast::<ACCESS_ALLOWED_ACE>().read_unaligned() };
                if ace.Mask != 0 && !account.trusted(sid) {
                    return Err(unsafe_storage(
                        "event journal ACL grants access outside the account, SYSTEM, or Administrators; provision a private directory",
                    ));
                }
            }
            _ => return Err(unsafe_storage("unsupported event journal ACL entry")),
        }
    }
    Ok(())
}

/// SID of an access-allowed ACE, or `None` when the SID does not fit the ACE.
///
/// The fixed SID header (revision, count, authority) must lie inside the ACE
/// before `IsValidSid` reads it, and the full length reported by
/// `GetLengthSid` must lie inside it before any caller compares the SID.
///
/// # Safety
///
/// `ace` must point to at least `ace_size` readable bytes.
unsafe fn allowed_ace_sid(ace: *const c_void, ace_size: u16) -> Option<PSID> {
    let start = offset_of!(ACCESS_ALLOWED_ACE, SidStart);
    if usize::from(ace_size) < start + offset_of!(SID, SubAuthority) {
        return None;
    }
    let sid: PSID = unsafe { ace.cast::<u8>().add(start).cast_mut().cast() };
    if unsafe { IsValidSid(sid) } == 0
        || start + unsafe { GetLengthSid(sid) } as usize > usize::from(ace_size)
    {
        return None;
    }
    Some(sid)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use windows_sys::Win32::Security::Authorization::SetSecurityInfo;

    #[test]
    fn allowed_ace_sid_must_fit_inside_its_ace() {
        // ACCESS_ALLOWED_ACE header and mask, then S-1-5-18 (LocalSystem):
        // an 8-byte SID header followed by one sub-authority.
        let mut bytes = [0u8; 20];
        bytes[0] = ACCESS_ALLOWED_ACE_TYPE as u8;
        bytes[2..4].copy_from_slice(&20u16.to_ne_bytes());
        bytes[4..8].copy_from_slice(&u32::MAX.to_ne_bytes());
        bytes[8..16].copy_from_slice(&[1, 1, 0, 0, 0, 0, 0, 5]);
        bytes[16..20].copy_from_slice(&18u32.to_ne_bytes());
        let words: [u32; 5] = std::array::from_fn(|index| {
            u32::from_ne_bytes(bytes[index * 4..index * 4 + 4].try_into().unwrap())
        });
        let ace = words.as_ptr().cast::<c_void>();
        let sid = unsafe { allowed_ace_sid(ace, 20) }.expect("complete SID");
        assert_ne!(unsafe { IsWellKnownSid(sid, WinLocalSystemSid) }, 0);
        // Only part of the fixed SID header inside the ACE.
        for size in [12, 15] {
            assert!(unsafe { allowed_ace_sid(ace, size) }.is_none(), "{size}");
        }
        // The header fits but the declared sub-authority does not.
        assert!(unsafe { allowed_ace_sid(ace, 16) }.is_none());
        // An invalid revision is refused before its length is trusted.
        let mut invalid = words;
        invalid[2] = u32::from_ne_bytes([9, 1, 0, 0]);
        assert!(unsafe { allowed_ace_sid(invalid.as_ptr().cast(), 20) }.is_none());
    }

    pub(crate) fn grant_everyone(path: &Path) {
        use windows_sys::Win32::Storage::FileSystem::WRITE_DAC;
        let file = OpenOptions::new()
            .access_mode(READ_CONTROL | WRITE_DAC)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .unwrap();
        let descriptor = descriptor_from_sddl("D:P(A;;FA;;;WD)").unwrap();
        let mut present = 0;
        let mut defaulted = 0;
        let mut dacl = null_mut();
        assert_ne!(
            unsafe {
                windows_sys::Win32::Security::GetSecurityDescriptorDacl(
                    descriptor.0,
                    &mut present,
                    &mut dacl,
                    &mut defaulted,
                )
            },
            0
        );
        assert_eq!(
            unsafe {
                SetSecurityInfo(
                    file.as_raw_handle(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    dacl,
                    null_mut(),
                )
            },
            0
        );
    }

    pub(crate) fn remove_dacl(path: &Path) {
        use windows_sys::Win32::Storage::FileSystem::WRITE_DAC;
        let file = OpenOptions::new()
            .access_mode(READ_CONTROL | WRITE_DAC)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .unwrap();
        assert_eq!(
            unsafe {
                SetSecurityInfo(
                    file.as_raw_handle(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    null_mut(),
                )
            },
            0
        );
    }
}
