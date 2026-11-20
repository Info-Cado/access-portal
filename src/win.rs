//! Small safe wrappers over the Windows calls the portal uses for accounts,
//! account settings (registry hives), sessions, processes and network
//! addresses. Each one takes and returns plain Rust values; the `unsafe` stays
//! in here. Before these, every step started PowerShell (about a second each).

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use anyhow::{bail, Context, Result};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, ERROR_FILE_NOT_FOUND, ERROR_LOGON_TYPE_NOT_GRANTED, ERROR_MEMBER_IN_ALIAS,
    ERROR_MORE_DATA, ERROR_NOT_ALL_ASSIGNED, ERROR_PASSWORD_EXPIRED, ERROR_PASSWORD_MUST_CHANGE, HANDLE,
    INVALID_HANDLE_VALUE, LUID,
};
use windows_sys::Win32::NetworkManagement::NetManagement as net;
use windows_sys::Win32::Security as sec;
use windows_sys::Win32::System::Registry as reg;

/// A NUL-terminated UTF-16 copy of `s`, for passing to Windows.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_path(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

/// A Windows string Windows handed us (may be null).
unsafe fn from_pwstr(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0;
    while *p.add(len) != 0 {
        len += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
}

/// A fixed-size UTF-16 buffer, up to its first NUL.
fn from_wide_buf(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Windows' own text for an error code ("Access is denied." ...).
pub fn error_text(code: u32) -> String {
    let text = std::io::Error::from_raw_os_error(code as i32).to_string();
    // "Access is denied. (os error 5)" reads better as "Access is denied (5)".
    match text.rsplit_once(" (os error") {
        Some((head, _)) => format!("{} ({code})", head.trim_end_matches('.')),
        None => text,
    }
}

// ---------------------------------------------------------------------------
// SIDs and the portal's own token
// ---------------------------------------------------------------------------

/// The text form ("S-1-5-21-...") of a binary SID.
unsafe fn sid_to_string(sid: sec::PSID) -> Option<String> {
    if sid.is_null() {
        return None;
    }
    let mut text: *mut u16 = null_mut();
    if sec::Authorization::ConvertSidToStringSidW(sid, &mut text) == 0 {
        return None;
    }
    let s = from_pwstr(text);
    LocalFree(text as *mut c_void);
    Some(s)
}

/// A binary SID made from its text form; freed on drop.
struct OwnedSid(sec::PSID);

impl OwnedSid {
    fn parse(text: &str) -> Result<Self> {
        let mut sid: sec::PSID = null_mut();
        let w = wide(text);
        if unsafe { sec::Authorization::ConvertStringSidToSidW(w.as_ptr(), &mut sid) } == 0 {
            bail!("bad SID {text}");
        }
        Ok(Self(sid))
    }
}

impl Drop for OwnedSid {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0) };
    }
}

/// BUILTIN\Administrators, found by SID so it works in every language.
pub const ADMINISTRATORS_SID: &str = "S-1-5-32-544";

/// The name of a SID's account or group on this computer ("Administrators",
/// "Administratoren", ...).
pub fn name_of_sid(text: &str) -> Result<String> {
    let sid = OwnedSid::parse(text)?;
    let mut name = [0u16; 256];
    let mut domain = [0u16; 256];
    let (mut n, mut d) = (name.len() as u32, domain.len() as u32);
    let mut usage = 0;
    let ok = unsafe { sec::LookupAccountSidW(null(), sid.0, name.as_mut_ptr(), &mut n, domain.as_mut_ptr(), &mut d, &mut usage) };
    if ok == 0 {
        bail!("looking up {text}: {}", error_text(unsafe { GetLastError() }));
    }
    Ok(from_wide_buf(&name))
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// The SID of the account the portal runs as.
pub fn my_sid() -> Result<String> {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), sec::TOKEN_QUERY, &mut token) == 0 {
            bail!("reading the portal's own account: {}", error_text(GetLastError()));
        }
        let token = OwnedHandle(token);
        let mut buf = vec![0u64; 64];
        let mut len = 0u32;
        if sec::GetTokenInformation(token.0, sec::TokenUser, buf.as_mut_ptr().cast(), (buf.len() * 8) as u32, &mut len) == 0 {
            bail!("reading the portal's own account: {}", error_text(GetLastError()));
        }
        let user = &*(buf.as_ptr() as *const sec::TOKEN_USER);
        sid_to_string(user.User.Sid).context("reading the portal's own account")
    }
}

/// Whether the portal runs with administrator rights (elevated).
pub fn is_elevated() -> bool {
    let Ok(admins) = OwnedSid::parse(ADMINISTRATORS_SID) else { return false };
    let mut member = 0;
    let ok = unsafe { sec::CheckTokenMembership(null_mut(), admins.0, &mut member) };
    ok != 0 && member != 0
}

// ---------------------------------------------------------------------------
// Local accounts and groups (netapi32)
// ---------------------------------------------------------------------------

/// A buffer netapi32 allocated; freed on drop.
struct NetBuf(*mut u8);

impl Drop for NetBuf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { net::NetApiBufferFree(self.0.cast()) };
        }
    }
}

/// One local account.
#[derive(Clone, Debug)]
pub struct UserInfo {
    /// The account name as Windows spells it.
    pub name: String,
    pub full_name: String,
    /// What Windows calls the description.
    pub comment: String,
    pub flags: u32,
    pub sid: String,
    /// Unix seconds; 0 when it never signed in.
    pub last_logon: u32,
}

impl UserInfo {
    pub fn enabled(&self) -> bool {
        self.flags & net::UF_ACCOUNTDISABLE == 0
    }
}

/// The names of every local account (normal accounts, as Get-LocalUser shows).
pub fn user_names() -> Result<Vec<String>> {
    let mut names = Vec::new();
    let mut resume = 0u32;
    loop {
        let mut buf: *mut u8 = null_mut();
        let (mut read, mut total) = (0u32, 0u32);
        let rc = unsafe {
            net::NetUserEnum(null(), 0, net::FILTER_NORMAL_ACCOUNT, &mut buf, net::MAX_PREFERRED_LENGTH, &mut read, &mut total, &mut resume)
        };
        let buf = NetBuf(buf);
        if rc != 0 && rc != ERROR_MORE_DATA {
            bail!("listing the Windows accounts: {}", error_text(rc));
        }
        if !buf.0.is_null() {
            let items = unsafe { std::slice::from_raw_parts(buf.0 as *const net::USER_INFO_0, read as usize) };
            names.extend(items.iter().map(|u| unsafe { from_pwstr(u.usri0_name) }));
        }
        if rc != ERROR_MORE_DATA {
            return Ok(names);
        }
    }
}

/// One account by name (any case); `None` when there is no such account.
pub fn user_info(name: &str) -> Result<Option<UserInfo>> {
    let w = wide(name);
    let mut buf: *mut u8 = null_mut();
    let rc = unsafe { net::NetUserGetInfo(null(), w.as_ptr(), 4, &mut buf) };
    let buf = NetBuf(buf);
    if rc == net::NERR_UserNotFound {
        return Ok(None);
    }
    if rc != 0 {
        bail!("reading the Windows account {name}: {}", error_text(rc));
    }
    let u = unsafe { &*(buf.0 as *const net::USER_INFO_4) };
    unsafe {
        Ok(Some(UserInfo {
            name: from_pwstr(u.usri4_name),
            full_name: from_pwstr(u.usri4_full_name),
            comment: from_pwstr(u.usri4_comment),
            flags: u.usri4_flags,
            sid: sid_to_string(u.usri4_user_sid).context("reading an account's SID")?,
            last_logon: u.usri4_last_logon,
        }))
    }
}

/// Whether the account signs in with a Microsoft account (an "internet
/// identity"), which is what Get-LocalUser calls PrincipalSource MicrosoftAccount.
pub fn is_microsoft(name: &str) -> bool {
    let w = wide(name);
    let mut buf: *mut u8 = null_mut();
    let rc = unsafe { net::NetUserGetInfo(null(), w.as_ptr(), 24, &mut buf) };
    let buf = NetBuf(buf);
    rc == 0 && !buf.0.is_null() && unsafe { (*(buf.0 as *const net::USER_INFO_24)).usri24_internet_identity } != 0
}

/// A group's members as (SID, name); `None` when the group doesn't exist.
pub fn group_members(group: &str) -> Result<Option<Vec<(String, String)>>> {
    const ERROR_NO_SUCH_ALIAS: u32 = 1376;
    let w = wide(group);
    let mut members = Vec::new();
    let mut resume = 0usize;
    loop {
        let mut buf: *mut u8 = null_mut();
        let (mut read, mut total) = (0u32, 0u32);
        let rc = unsafe {
            net::NetLocalGroupGetMembers(null(), w.as_ptr(), 1, &mut buf, net::MAX_PREFERRED_LENGTH, &mut read, &mut total, &mut resume)
        };
        let buf = NetBuf(buf);
        if rc == net::NERR_GroupNotFound || rc == ERROR_NO_SUCH_ALIAS {
            return Ok(None);
        }
        if rc != 0 && rc != ERROR_MORE_DATA {
            bail!("reading the members of {group}: {}", error_text(rc));
        }
        if !buf.0.is_null() {
            let items = unsafe { std::slice::from_raw_parts(buf.0 as *const net::LOCALGROUP_MEMBERS_INFO_1, read as usize) };
            for m in items {
                let sid = unsafe { sid_to_string(m.lgrmi1_sid) }.unwrap_or_default();
                members.push((sid, unsafe { from_pwstr(m.lgrmi1_name) }));
            }
        }
        if rc != ERROR_MORE_DATA {
            return Ok(Some(members));
        }
    }
}

/// The members of BUILTIN\Administrators (whatever its name in this language).
pub fn administrators() -> Result<Vec<(String, String)>> {
    let group = name_of_sid(ADMINISTRATORS_SID)?;
    Ok(group_members(&group)?.unwrap_or_default())
}

/// What a failed account change says, in plain words.
fn net_error(what: &str, rc: u32) -> anyhow::Error {
    match rc {
        net::NERR_PasswordTooShort => anyhow::anyhow!(
            "Windows refused the password: it doesn't meet this computer's password rules (length, complexity or history)."
        ),
        5 => anyhow::anyhow!("Windows refused to {what}: access is denied (the portal needs administrator rights)."),
        _ => anyhow::anyhow!("Windows could not {what}: {}", error_text(rc)),
    }
}

fn set_user_info(name: &str, level: u32, data: *const u8, what: &str) -> Result<()> {
    let w = wide(name);
    let mut parm = 0u32;
    let rc = unsafe { net::NetUserSetInfo(null(), w.as_ptr(), level, data, &mut parm) };
    if rc != 0 {
        return Err(net_error(what, rc));
    }
    Ok(())
}

pub fn set_full_name(name: &str, full_name: &str) -> Result<()> {
    let v = wide(full_name);
    let info = net::USER_INFO_1011 { usri1011_full_name: v.as_ptr() as *mut u16 };
    set_user_info(name, 1011, (&info as *const net::USER_INFO_1011).cast(), &format!("change the full name of {name}"))
}

pub fn set_comment(name: &str, comment: &str) -> Result<()> {
    let v = wide(comment);
    let info = net::USER_INFO_1007 { usri1007_comment: v.as_ptr() as *mut u16 };
    set_user_info(name, 1007, (&info as *const net::USER_INFO_1007).cast(), &format!("change the description of {name}"))
}

pub fn set_password(name: &str, password: &str) -> Result<()> {
    let v = wide(password);
    let info = net::USER_INFO_1003 { usri1003_password: v.as_ptr() as *mut u16 };
    set_user_info(name, 1003, (&info as *const net::USER_INFO_1003).cast(), &format!("change the password of {name}"))
}

pub fn set_enabled(name: &str, enabled: bool) -> Result<()> {
    let Some(user) = user_info(name)? else { bail!("There is no Windows account named {name} on this computer.") };
    let flags = if enabled { user.flags & !net::UF_ACCOUNTDISABLE } else { user.flags | net::UF_ACCOUNTDISABLE };
    // UF_SCRIPT must always be set when flags are written.
    let info = net::USER_INFO_1008 { usri1008_flags: flags | net::UF_SCRIPT };
    let what = if enabled { format!("turn {name} on") } else { format!("turn {name} off") };
    set_user_info(name, 1008, (&info as *const net::USER_INFO_1008).cast(), &what)
}

/// A new standard account: password never expires, the user can't change it,
/// the account never expires.
pub fn add_user(name: &str, password: &str, comment: &str) -> Result<()> {
    let (n, p, c) = (wide(name), wide(password), wide(comment));
    let info = net::USER_INFO_1 {
        usri1_name: n.as_ptr() as *mut u16,
        usri1_password: p.as_ptr() as *mut u16,
        usri1_password_age: 0,
        usri1_priv: net::USER_PRIV_USER,
        usri1_home_dir: null_mut(),
        usri1_comment: c.as_ptr() as *mut u16,
        usri1_flags: net::UF_SCRIPT | net::UF_NORMAL_ACCOUNT | net::UF_DONT_EXPIRE_PASSWD | net::UF_PASSWD_CANT_CHANGE,
        usri1_script_path: null_mut(),
    };
    let mut parm = 0u32;
    let rc = unsafe { net::NetUserAdd(null(), 1, (&info as *const net::USER_INFO_1).cast(), &mut parm) };
    match rc {
        0 => Ok(()),
        net::NERR_UserExists => bail!("A Windows account named {name} already exists on this computer."),
        _ => Err(net_error(&format!("create the account {name}"), rc)),
    }
}

pub fn add_to_group(group: &str, name: &str) -> Result<()> {
    let (g, n) = (wide(group), wide(name));
    let info = net::LOCALGROUP_MEMBERS_INFO_3 { lgrmi3_domainandname: n.as_ptr() as *mut u16 };
    let rc = unsafe { net::NetLocalGroupAddMembers(null(), g.as_ptr(), 3, (&info as *const net::LOCALGROUP_MEMBERS_INFO_3).cast(), 1) };
    if rc == 0 || rc == ERROR_MEMBER_IN_ALIAS {
        Ok(())
    } else {
        Err(net_error(&format!("add {name} to the group '{group}'"), rc))
    }
}

pub fn delete_user(name: &str) -> Result<()> {
    let w = wide(name);
    let rc = unsafe { net::NetUserDel(null(), w.as_ptr()) };
    if rc == 0 {
        Ok(())
    } else {
        Err(net_error(&format!("delete the account {name}"), rc))
    }
}

/// Whether `password` is the account's password. A right password for an
/// account that may not sign in this way (or must change it) still counts.
pub fn check_password(name: &str, password: &str) -> bool {
    let (n, d, p) = (wide(name), wide("."), wide(password));
    let mut token: HANDLE = null_mut();
    let ok = unsafe {
        sec::LogonUserW(n.as_ptr(), d.as_ptr(), p.as_ptr(), sec::LOGON32_LOGON_NETWORK, sec::LOGON32_PROVIDER_DEFAULT, &mut token)
    };
    if ok != 0 {
        drop(OwnedHandle(token));
        return true;
    }
    matches!(unsafe { GetLastError() }, ERROR_LOGON_TYPE_NOT_GRANTED | ERROR_PASSWORD_MUST_CHANGE | ERROR_PASSWORD_EXPIRED)
}

// ---------------------------------------------------------------------------
// Profiles (userenv)
// ---------------------------------------------------------------------------

const PROFILE_LIST: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList";

/// Whether Windows has a profile entry for this SID (it signed in, or one was made).
pub fn has_profile(sid: &str) -> bool {
    matches!(Key::open(reg::HKEY_LOCAL_MACHINE, &format!(r"{PROFILE_LIST}\{sid}"), false), Ok(Some(_)))
}

/// The account's profile folder (C:\Users\name), from its profile entry.
pub fn profile_path(sid: &str) -> Option<PathBuf> {
    let key = Key::open(reg::HKEY_LOCAL_MACHINE, &format!(r"{PROFILE_LIST}\{sid}"), false).ok()??;
    let raw = key.get_string("ProfileImagePath")?;
    Some(PathBuf::from(expand_env(&raw)))
}

/// Make the account's profile (folder and settings file) before its first
/// sign-in, so its settings can be prepared. Returns the folder.
pub fn create_profile(sid: &str, name: &str) -> Result<PathBuf> {
    let (s, n) = (wide(sid), wide(name));
    let mut path = [0u16; 260];
    let hr = unsafe { windows_sys::Win32::UI::Shell::CreateProfile(s.as_ptr(), n.as_ptr(), path.as_mut_ptr(), path.len() as u32) };
    if hr != 0 {
        bail!("Could not create the Windows profile for {name} (0x{:08X}).", hr as u32);
    }
    Ok(PathBuf::from(from_wide_buf(&path)))
}

/// Remove the account's profile: its folder and its profile entry.
pub fn delete_profile(sid: &str) -> std::result::Result<(), u32> {
    let s = wide(sid);
    if unsafe { windows_sys::Win32::UI::Shell::DeleteProfileW(s.as_ptr(), null(), null()) } != 0 {
        Ok(())
    } else {
        Err(unsafe { GetLastError() })
    }
}

pub fn expand_env(s: &str) -> String {
    use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;
    let src = wide(s);
    let mut buf = vec![0u16; 1024];
    loop {
        let n = unsafe { ExpandEnvironmentStringsW(src.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
        if n == 0 {
            return s.to_string();
        }
        if n <= buf.len() {
            return from_wide_buf(&buf);
        }
        buf.resize(n, 0);
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

pub use reg::{HKEY_LOCAL_MACHINE, HKEY_USERS};

/// An open registry key; closed on drop.
pub struct Key(reg::HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe { reg::RegCloseKey(self.0) };
    }
}

impl Key {
    /// Open an existing key (`None` when it doesn't exist), for reading or for
    /// changing (`write`).
    pub fn open(parent: reg::HKEY, path: &str, write: bool) -> Result<Option<Key>> {
        let w = wide(path);
        let mut key: reg::HKEY = null_mut();
        let access = if write { reg::KEY_ALL_ACCESS } else { reg::KEY_READ };
        let rc = unsafe { reg::RegOpenKeyExW(parent, w.as_ptr(), 0, access, &mut key) };
        match rc {
            0 => Ok(Some(Key(key))),
            ERROR_FILE_NOT_FOUND => Ok(None),
            _ => bail!("opening the registry key {path}: {}", error_text(rc)),
        }
    }

    /// Open a key, creating it (and its parents) when missing.
    pub fn create(parent: reg::HKEY, path: &str) -> Result<Key> {
        let w = wide(path);
        let mut key: reg::HKEY = null_mut();
        let rc = unsafe {
            reg::RegCreateKeyExW(parent, w.as_ptr(), 0, null(), reg::REG_OPTION_NON_VOLATILE, reg::KEY_ALL_ACCESS, null(), &mut key, null_mut())
        };
        if rc != 0 {
            bail!("creating the registry key {path}: {}", error_text(rc));
        }
        Ok(Key(key))
    }

    pub fn open_sub(&self, path: &str, write: bool) -> Result<Option<Key>> {
        Key::open(self.0, path, write)
    }

    pub fn create_sub(&self, path: &str) -> Result<Key> {
        Key::create(self.0, path)
    }

    fn query(&self, name: &str) -> Option<(u32, Vec<u8>)> {
        let w = wide(name);
        let mut kind = 0u32;
        let mut len = 0u32;
        let rc = unsafe { reg::RegQueryValueExW(self.0, w.as_ptr(), null(), &mut kind, null_mut(), &mut len) };
        if rc != 0 {
            return None;
        }
        let mut data = vec![0u8; len as usize + 2];
        let mut len2 = len + 2;
        let rc = unsafe { reg::RegQueryValueExW(self.0, w.as_ptr(), null(), &mut kind, data.as_mut_ptr(), &mut len2) };
        if rc != 0 {
            return None;
        }
        data.truncate(len2 as usize);
        Some((kind, data))
    }

    /// A REG_DWORD value.
    pub fn get_dword(&self, name: &str) -> Option<u32> {
        match self.query(name)? {
            (reg::REG_DWORD, d) if d.len() >= 4 => Some(u32::from_le_bytes([d[0], d[1], d[2], d[3]])),
            _ => None,
        }
    }

    /// A REG_SZ or REG_EXPAND_SZ value, as stored (not expanded).
    pub fn get_string(&self, name: &str) -> Option<String> {
        match self.query(name)? {
            (reg::REG_SZ | reg::REG_EXPAND_SZ, d) => {
                let units: Vec<u16> = d.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                Some(from_wide_buf(&units))
            }
            _ => None,
        }
    }

    pub fn set_dword(&self, name: &str, value: u32) -> Result<()> {
        let w = wide(name);
        let bytes = value.to_le_bytes();
        let rc = unsafe { reg::RegSetValueExW(self.0, w.as_ptr(), 0, reg::REG_DWORD, bytes.as_ptr(), 4) };
        if rc != 0 {
            bail!("writing the registry value {name}: {}", error_text(rc));
        }
        Ok(())
    }

    pub fn set_string(&self, name: &str, value: &str) -> Result<()> {
        let w = wide(name);
        let v = wide(value);
        let rc = unsafe { reg::RegSetValueExW(self.0, w.as_ptr(), 0, reg::REG_SZ, v.as_ptr().cast(), (v.len() * 2) as u32) };
        if rc != 0 {
            bail!("writing the registry value {name}: {}", error_text(rc));
        }
        Ok(())
    }

    /// Delete a value; one that isn't there is fine.
    pub fn delete_value(&self, name: &str) -> Result<()> {
        let w = wide(name);
        let rc = unsafe { reg::RegDeleteValueW(self.0, w.as_ptr()) };
        if rc != 0 && rc != ERROR_FILE_NOT_FOUND {
            bail!("deleting the registry value {name}: {}", error_text(rc));
        }
        Ok(())
    }

    /// Delete a subkey and everything under it; one that isn't there is fine.
    pub fn delete_tree(&self, sub: &str) -> Result<()> {
        let w = wide(sub);
        let rc = unsafe { reg::RegDeleteTreeW(self.0, w.as_ptr()) };
        if rc != 0 && rc != ERROR_FILE_NOT_FOUND {
            bail!("deleting the registry key {sub}: {}", error_text(rc));
        }
        if rc == 0 {
            // RegDeleteTree empties the key; remove the key itself too.
            let rc = unsafe { reg::RegDeleteKeyW(self.0, w.as_ptr()) };
            if rc != 0 && rc != ERROR_FILE_NOT_FOUND {
                bail!("deleting the registry key {sub}: {}", error_text(rc));
            }
        }
        Ok(())
    }
}

/// The current thread acting with SeBackupPrivilege and SeRestorePrivilege
/// switched on (needed to load and unload another account's settings file).
/// Only this thread gets them, and only until this is dropped.
struct BackupRestore;

impl BackupRestore {
    fn begin() -> Result<Self> {
        use windows_sys::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};
        unsafe {
            if sec::ImpersonateSelf(sec::SecurityImpersonation) == 0 {
                bail!("acting with backup rights: {}", error_text(GetLastError()));
            }
            let me = BackupRestore; // reverts on any early return
            let mut token: HANDLE = null_mut();
            if OpenThreadToken(GetCurrentThread(), sec::TOKEN_ADJUST_PRIVILEGES | sec::TOKEN_QUERY, 1, &mut token) == 0 {
                bail!("acting with backup rights: {}", error_text(GetLastError()));
            }
            let token = OwnedHandle(token);
            for privilege in [sec::SE_BACKUP_NAME, sec::SE_RESTORE_NAME] {
                let mut luid = LUID { LowPart: 0, HighPart: 0 };
                if sec::LookupPrivilegeValueW(null(), privilege, &mut luid) == 0 {
                    bail!("acting with backup rights: {}", error_text(GetLastError()));
                }
                let tp = sec::TOKEN_PRIVILEGES {
                    PrivilegeCount: 1,
                    Privileges: [sec::LUID_AND_ATTRIBUTES { Luid: luid, Attributes: sec::SE_PRIVILEGE_ENABLED }],
                };
                let ok = sec::AdjustTokenPrivileges(token.0, 0, &tp, 0, null_mut(), null_mut());
                let err = GetLastError();
                if ok == 0 || err == ERROR_NOT_ALL_ASSIGNED {
                    bail!("The portal lacks the backup and restore rights needed to open another account's settings.");
                }
            }
            Ok(me)
        }
    }
}

impl Drop for BackupRestore {
    fn drop(&mut self) {
        unsafe { sec::RevertToSelf() };
    }
}

/// An account's own settings (its HKEY_CURRENT_USER): already open under
/// HKEY_USERS\<SID> while it is signed in, otherwise its NTUSER.DAT is loaded
/// under a temporary name and unloaded again on drop.
pub struct Hive {
    root: Option<Key>,
    temp: Option<Vec<u16>>,
    _rights: Option<BackupRestore>,
}

impl Hive {
    /// `None` when the settings file can't be opened.
    pub fn open(sid: &str, profile_dir: &Path) -> Result<Option<Hive>> {
        if let Some(root) = Key::open(HKEY_USERS, sid, true)? {
            return Ok(Some(Hive { root: Some(root), temp: None, _rights: None }));
        }
        let rights = BackupRestore::begin()?;
        let temp = format!("ap_{sid}");
        // Left loaded by a run that stopped half way: use it, unload it after.
        if Key::open(HKEY_USERS, &temp, false)?.is_none() {
            let (t, f) = (wide(&temp), wide_path(&profile_dir.join("NTUSER.DAT")));
            let rc = unsafe { reg::RegLoadKeyW(HKEY_USERS, t.as_ptr(), f.as_ptr()) };
            if rc != 0 {
                tracing::warn!("loading {}: {}", profile_dir.join("NTUSER.DAT").display(), error_text(rc));
                return Ok(None);
            }
        }
        let mut hive = Hive { root: None, temp: Some(wide(&temp)), _rights: Some(rights) };
        hive.root = Key::open(HKEY_USERS, &temp, true)?;
        if hive.root.is_none() {
            return Ok(None);
        }
        Ok(Some(hive))
    }

    pub fn root(&self) -> &Key {
        self.root.as_ref().expect("hive is open")
    }
}

impl Drop for Hive {
    fn drop(&mut self) {
        // Every handle into the hive must be closed before it can be unloaded.
        self.root.take();
        if let Some(temp) = self.temp.take() {
            for attempt in 0..5 {
                let rc = unsafe { reg::RegUnLoadKeyW(HKEY_USERS, temp.as_ptr()) };
                if rc == 0 {
                    return;
                }
                if attempt == 4 {
                    tracing::warn!("could not unload {}: {}", String::from_utf16_lossy(&temp[..temp.len() - 1]), error_text(rc));
                }
                std::thread::sleep(std::time::Duration::from_millis(400));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Sessions (wtsapi32)
// ---------------------------------------------------------------------------

/// A signed-in Windows session.
#[derive(Clone, Debug)]
pub struct Session {
    pub id: u32,
    pub user: String,
    /// WTS_CONNECTSTATE_CLASS: 0 active, 4 disconnected, ...
    pub state: u32,
    pub disconnected_secs: u64,
}

/// Every session with a signed-in user.
pub fn sessions() -> Vec<Session> {
    use windows_sys::Win32::System::RemoteDesktop as wts;
    let mut list = Vec::new();
    unsafe {
        let mut p: *mut wts::WTS_SESSION_INFOW = null_mut();
        let mut count = 0u32;
        if wts::WTSEnumerateSessionsW(null_mut(), 0, 1, &mut p, &mut count) == 0 {
            tracing::warn!("listing Windows sessions: {}", error_text(GetLastError()));
            return list;
        }
        for s in std::slice::from_raw_parts(p, count as usize) {
            let mut buf: *mut u16 = null_mut();
            let mut len = 0u32;
            if wts::WTSQuerySessionInformationW(null_mut(), s.SessionId, wts::WTSSessionInfo, &mut buf, &mut len) == 0 || buf.is_null() {
                continue;
            }
            let info = std::ptr::read_unaligned(buf as *const wts::WTSINFOW);
            wts::WTSFreeMemory(buf.cast());
            let user = from_wide_buf(&info.UserName);
            if user.is_empty() {
                continue;
            }
            let state = s.State as u32;
            let disconnected_secs = if state == 4 && info.DisconnectTime > 0 {
                ((info.CurrentTime - info.DisconnectTime) / 10_000_000).max(0) as u64
            } else {
                0
            };
            list.push(Session { id: s.SessionId, user, state, disconnected_secs });
        }
        wts::WTSFreeMemory(p.cast());
    }
    list
}

/// Sign a session out; with `wait`, return once it has ended.
pub fn log_off(id: u32, wait: bool) -> bool {
    unsafe { windows_sys::Win32::System::RemoteDesktop::WTSLogoffSession(null_mut(), id, i32::from(wait)) != 0 }
}

// ---------------------------------------------------------------------------
// Processes
// ---------------------------------------------------------------------------

/// Every running process: (process id, program file name).
pub fn processes() -> Vec<(u32, String)> {
    use windows_sys::Win32::System::Diagnostics::ToolHelp as th;
    let mut list = Vec::new();
    unsafe {
        let snap = OwnedHandle(th::CreateToolhelp32Snapshot(th::TH32CS_SNAPPROCESS, 0));
        if snap.0 == INVALID_HANDLE_VALUE {
            return list;
        }
        let mut entry: th::PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<th::PROCESSENTRY32W>() as u32;
        let mut ok = th::Process32FirstW(snap.0, &mut entry);
        while ok != 0 {
            list.push((entry.th32ProcessID, from_wide_buf(&entry.szExeFile)));
            ok = th::Process32NextW(snap.0, &mut entry);
        }
    }
    list
}

/// The session a process runs in.
pub fn session_of(pid: u32) -> Option<u32> {
    let mut id = 0u32;
    (unsafe { windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId(pid, &mut id) } != 0).then_some(id)
}

/// A process's command line (`None` when it can't be read).
pub fn command_line(pid: u32) -> Option<String> {
    use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessCommandLineInformation};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    #[repr(C)]
    struct UnicodeString {
        length: u16,
        maximum_length: u16,
        buffer: *const u16,
    }
    unsafe {
        let process = OwnedHandle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid));
        if process.0.is_null() {
            return None;
        }
        let mut buf = vec![0u64; 512];
        let mut len = 0u32;
        let mut status = NtQueryInformationProcess(process.0, ProcessCommandLineInformation, buf.as_mut_ptr().cast(), (buf.len() * 8) as u32, &mut len);
        if status < 0 && len as usize > buf.len() * 8 {
            buf = vec![0u64; (len as usize).div_ceil(8)];
            status = NtQueryInformationProcess(process.0, ProcessCommandLineInformation, buf.as_mut_ptr().cast(), (buf.len() * 8) as u32, &mut len);
        }
        if status < 0 {
            return None;
        }
        let s = &*(buf.as_ptr() as *const UnicodeString);
        if s.buffer.is_null() {
            return Some(String::new());
        }
        Some(String::from_utf16_lossy(std::slice::from_raw_parts(s.buffer, s.length as usize / 2)))
    }
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

/// Whether a file or folder has an empty permission list (nobody may open it).
/// `None` when its permissions can't be read.
pub fn has_empty_dacl(path: &Path) -> Option<bool> {
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    let w = wide_path(path);
    let mut dacl: *mut sec::ACL = null_mut();
    let mut sd: sec::PSECURITY_DESCRIPTOR = null_mut();
    let rc = unsafe {
        GetNamedSecurityInfoW(w.as_ptr(), SE_FILE_OBJECT, sec::DACL_SECURITY_INFORMATION, null_mut(), null_mut(), &mut dacl, null_mut(), &mut sd)
    };
    if rc != 0 {
        return None;
    }
    let empty = !dacl.is_null() && unsafe { (*dacl).AceCount } == 0;
    unsafe { LocalFree(sd) };
    Some(empty)
}

/// A folder's own permission list, as far as the portal cares about it.
pub struct FolderAccess {
    /// It no longer takes permissions from its parent folder.
    pub protected: bool,
    /// SIDs given full control of the folder and everything inside it by an
    /// entry set on the folder itself (not inherited).
    pub full: Vec<String>,
}

/// Read [`FolderAccess`] for a folder; `None` when its permissions can't be read.
pub fn folder_access(path: &Path) -> Option<FolderAccess> {
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    const FILE_ALL_ACCESS: u32 = 0x001F_01FF;
    const GENERIC_ALL: u32 = 0x1000_0000;
    let w = wide_path(path);
    let mut dacl: *mut sec::ACL = null_mut();
    let mut sd: sec::PSECURITY_DESCRIPTOR = null_mut();
    let rc = unsafe {
        GetNamedSecurityInfoW(w.as_ptr(), SE_FILE_OBJECT, sec::DACL_SECURITY_INFORMATION, null_mut(), null_mut(), &mut dacl, null_mut(), &mut sd)
    };
    if rc != 0 {
        return None;
    }
    let mut control = 0u16;
    let mut revision = 0u32;
    let protected = unsafe { sec::GetSecurityDescriptorControl(sd, &mut control, &mut revision) } != 0
        && control & sec::SE_DACL_PROTECTED != 0;
    let mut full = Vec::new();
    let count = if dacl.is_null() { 0 } else { unsafe { (*dacl).AceCount } };
    let inherit = (sec::OBJECT_INHERIT_ACE | sec::CONTAINER_INHERIT_ACE) as u8;
    for i in 0..u32::from(count) {
        let mut ace: *mut c_void = null_mut();
        if unsafe { sec::GetAce(dacl, i, &mut ace) } == 0 || ace.is_null() {
            continue;
        }
        let header = unsafe { &*(ace as *const sec::ACE_HEADER) };
        let flags = header.AceFlags;
        if header.AceType != ACCESS_ALLOWED_ACE_TYPE
            || flags & sec::INHERITED_ACE as u8 != 0
            || flags & sec::INHERIT_ONLY_ACE as u8 != 0
            || flags & inherit != inherit
        {
            continue;
        }
        let allowed = unsafe { &*(ace as *const sec::ACCESS_ALLOWED_ACE) };
        if allowed.Mask & FILE_ALL_ACCESS != FILE_ALL_ACCESS && allowed.Mask & GENERIC_ALL == 0 {
            continue;
        }
        let sid = std::ptr::addr_of!(allowed.SidStart) as sec::PSID;
        if let Some(text) = unsafe { sid_to_string(sid) } {
            full.push(text);
        }
    }
    unsafe { LocalFree(sd) };
    Some(FolderAccess { protected, full })
}

/// What a shortcut (.lnk) opens: (program, arguments, working folder). Read
/// with the Windows shell, on a thread of its own (COM needs one set up for it).
pub fn resolve_shortcut(path: &Path) -> Result<(String, String, String)> {
    let path = path.to_path_buf();
    std::thread::spawn(move || -> Result<(String, String, String)> {
        use windows::core::{Interface, PCWSTR};
        use windows::Win32::System::Com::{
            CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, STGM_READ,
        };
        use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
        unsafe {
            let init = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let result = (|| -> windows::core::Result<(String, String, String)> {
                let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
                let file: IPersistFile = link.cast()?;
                let w = wide_path(&path);
                file.Load(PCWSTR(w.as_ptr()), STGM_READ)?;
                let mut target = vec![0u16; 1024];
                link.GetPath(&mut target, null_mut(), 0)?;
                let mut args = vec![0u16; 4096];
                link.GetArguments(&mut args)?;
                let mut dir = vec![0u16; 1024];
                link.GetWorkingDirectory(&mut dir)?;
                Ok((from_wide_buf(&target), from_wide_buf(&args), from_wide_buf(&dir)))
            })();
            if init.is_ok() {
                CoUninitialize();
            }
            result.map_err(|e| anyhow::anyhow!("Could not read the shortcut '{}': {}", path.display(), e.message()))
        }
    })
    .join()
    .map_err(|_| anyhow::anyhow!("reading the shortcut failed"))?
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

/// The IPv4 addresses of network adapters that are up, skipping adapters
/// whose name starts with one of `skip` (case does not matter).
pub fn ipv4_addresses(skip: &[&str]) -> Vec<std::net::Ipv4Addr> {
    use windows_sys::Win32::NetworkManagement::IpHelper as ip;
    use windows_sys::Win32::Networking::WinSock::{IpDadStatePreferred, AF_INET, SOCKADDR_IN};
    let mut found = Vec::new();
    let flags = ip::GAA_FLAG_SKIP_ANYCAST | ip::GAA_FLAG_SKIP_MULTICAST | ip::GAA_FLAG_SKIP_DNS_SERVER;
    let mut size = 16 * 1024u32;
    let mut buf: Vec<u64>;
    loop {
        buf = vec![0u64; (size as usize).div_ceil(8)];
        let rc = unsafe { ip::GetAdaptersAddresses(AF_INET as u32, flags, null(), buf.as_mut_ptr().cast(), &mut size) };
        match rc {
            0 => break,
            111 => continue, // ERROR_BUFFER_OVERFLOW: size now holds what is needed
            _ => return found,
        }
    }
    unsafe {
        let mut adapter = buf.as_ptr() as *const ip::IP_ADAPTER_ADDRESSES_LH;
        while !adapter.is_null() {
            let a = &*adapter;
            adapter = a.Next;
            if a.OperStatus != windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp {
                continue;
            }
            let name = from_pwstr(a.FriendlyName).to_lowercase();
            if skip.iter().any(|s| name.starts_with(&s.to_lowercase())) {
                continue;
            }
            let mut unicast = a.FirstUnicastAddress;
            while !unicast.is_null() {
                let u = &*unicast;
                unicast = u.Next;
                if u.DadState != IpDadStatePreferred || u.Address.lpSockaddr.is_null() {
                    continue;
                }
                if (*u.Address.lpSockaddr).sa_family != AF_INET {
                    continue;
                }
                let sin = &*(u.Address.lpSockaddr as *const SOCKADDR_IN);
                found.push(std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.S_un.S_addr)));
            }
        }
    }
    found
}
