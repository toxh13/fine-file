//! Thin wrappers over the Windows APIs fine-file needs.

use serde::Serialize;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, IntoRawHandle};
use std::os::windows::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, ERROR_BROKEN_PIPE, ERROR_CANCELLED, ERROR_IO_PENDING, ERROR_NOT_ALL_ASSIGNED,
    ERROR_OPERATION_ABORTED, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
    SE_BACKUP_NAME, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows_sys::Win32::Storage::FileSystem::{
    FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, GetDiskFreeSpaceExW, GetDiskFreeSpaceW, GetDriveTypeW,
    GetFileType, GetLogicalDrives, GetVolumeInformationW, GetLongPathNameW, GetVolumeNameForVolumeMountPointW, GetVolumePathNameW,
    MoveFileExW, QueryDosDeviceW, ReadFile, WriteFile, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_FIRST_PIPE_INSTANCE,
    FILE_FLAG_OVERLAPPED, FILE_FLAGS_AND_ATTRIBUTES, FILE_READ_ATTRIBUTES, FILE_TYPE_PIPE, PIPE_ACCESS_DUPLEX,
    PIPE_ACCESS_INBOUND, SECURITY_ANONYMOUS, WIN32_FIND_DATAW,
};
use windows_sys::core::{GUID, PWSTR};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, GetNamedPipeServerProcessId,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, GetExitCodeProcess, GetProcessId, OpenProcessToken, WaitForMultipleObjects,
    WaitForSingleObject, INFINITE,
};
use windows_sys::Win32::UI::Shell::{
    FOLDERID_ProgramFiles, FOLDERID_ProgramFilesX64, FOLDERID_ProgramFilesX86, IsUserAnAdmin, SHGetKnownFolderPath,
    ShellExecuteExW, ShellExecuteW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOWNORMAL};

const DRIVE_FIXED: u32 = 3;
/// A drive that has not answered by then is left out of this listing.
const DRIVE_QUERY_TIMEOUT: Duration = Duration::from_secs(3);
/// Who may open the helper pipe: elevated processes (they carry the
/// Administrators group, whichever admin account approved the UAC prompt).
/// Nothing running unelevated, the app itself included, can connect.
const HELPER_PIPE_SDDL: &str = "D:P(A;;GA;;;BA)";

pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_os(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

fn os_error(code: u32) -> io::Error {
    io::Error::from_raw_os_error(code as i32)
}

pub fn is_elevated() -> bool {
    unsafe { IsUserAnAdmin() != 0 }
}

#[derive(Serialize, Clone)]
pub struct Drive {
    pub letter: String,
    pub label: String,
    pub fs: String,
    pub kind: String,
    pub total: u64,
    pub free: u64,
    /// A SUBST drive (`is_subst`): its kind is that of the disk it points into.
    pub subst: bool,
}

/// Drive letters in use, such as `C:`. Touches no drive, so it never stalls.
pub fn drive_letters() -> Vec<String> {
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8).filter(|i| mask & (1 << i) != 0).map(|i| format!("{}:", (b'A' + i) as char)).collect()
}

/// Every drive. Drives are asked in parallel, so one unreachable network
/// drive or a disc that is spinning up cannot hold up the whole list; one that
/// has not answered within a few seconds is listed with kind "unresponsive"
/// and no size, so it stays visible (and scannable) instead of vanishing.
pub fn drives() -> Vec<Drive> {
    let letters = drive_letters();
    let (tx, rx) = std::sync::mpsc::channel();
    for (i, letter) in letters.iter().cloned().enumerate() {
        let tx = tx.clone();
        // A query that misses the deadline finishes on its own; nobody waits for it.
        std::thread::spawn(move || {
            let _ = tx.send((i, drive(&letter)));
        });
    }
    drop(tx);
    let deadline = Instant::now() + DRIVE_QUERY_TIMEOUT;
    let mut answered: Vec<Option<Option<Drive>>> = vec![None; letters.len()];
    while let Ok((i, d)) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        answered[i] = Some(d);
    }
    answered
        .into_iter()
        .zip(letters)
        .filter_map(|(a, letter)| match a {
            // Answered: a drive, or nothing to show (an empty card reader).
            Some(d) => d,
            None => Some(Drive {
                letter,
                label: String::new(),
                fs: String::new(),
                kind: "unresponsive".into(),
                total: 0,
                free: 0,
                subst: false,
            }),
        })
        .collect()
}

/// What kind of drive a letter is ("fixed", "removable", "network", "cdrom",
/// "ram" or "unknown"), from the drive type alone.
pub fn drive_kind(letter: &str) -> &'static str {
    kind_of(&wide(&format!("{letter}\\")))
}

fn kind_of(root: &[u16]) -> &'static str {
    match unsafe { GetDriveTypeW(root.as_ptr()) } {
        2 => "removable",
        3 => "fixed",
        4 => "network",
        5 => "cdrom",
        6 => "ram",
        _ => "unknown",
    }
}

/// Bytes per cluster on drive `letter`, the unit disk space is handed out in.
pub fn cluster_size(letter: &str) -> Option<u64> {
    let root = wide(&format!("{letter}\\"));
    let (mut per_cluster, mut per_sector, mut free, mut total) = (0u32, 0u32, 0u32, 0u32);
    let ok = unsafe { GetDiskFreeSpaceW(root.as_ptr(), &mut per_cluster, &mut per_sector, &mut free, &mut total) };
    (ok != 0 && per_cluster > 0 && per_sector > 0).then(|| per_cluster as u64 * per_sector as u64)
}

/// One drive (`C:`), or None when it has no readable volume.
pub fn drive(letter: &str) -> Option<Drive> {
    let root = wide(&format!("{letter}\\"));
    let kind = kind_of(&root);
    let mut label = [0u16; 261];
    let mut fs = [0u16; 261];
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            null_mut(),
            null_mut(),
            null_mut(),
            fs.as_mut_ptr(),
            fs.len() as u32,
        )
    };
    if ok == 0 {
        // Empty card readers and disc drives: nothing to show.
        return None;
    }
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    unsafe { GetDiskFreeSpaceExW(root.as_ptr(), &mut avail, &mut total, &mut free) };
    Some(Drive {
        letter: letter.to_string(),
        label: from_wide(&label),
        fs: from_wide(&fs),
        kind: kind.into(),
        total,
        free,
        subst: is_subst(letter),
    })
}

/// Whether Ctrl and Shift are down at this moment, whichever window has the
/// keyboard (not as of the last message this thread read).
pub fn ctrl_shift_down() -> (bool, bool) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_SHIFT};
    let down = |vk: u16| unsafe { GetAsyncKeyState(i32::from(vk)) } < 0;
    (down(VK_CONTROL.0), down(VK_SHIFT.0))
}

/// Bytes the user may still write on the volume that holds `path` (or the
/// nearest folder above it that exists), or None when that cannot be told.
pub fn free_space(path: &Path) -> Option<u64> {
    path.ancestors().filter(|p| !p.as_os_str().is_empty()).find_map(|p| {
        let dir = wide_os(p.as_os_str());
        let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
        let ok = unsafe { GetDiskFreeSpaceExW(dir.as_ptr(), &mut avail, &mut total, &mut free) };
        (ok != 0).then_some(avail)
    })
}

/// The reparse tag of the entry at `path`, 0 when it is not a reparse point,
/// as its folder's listing has it: the entry itself is not opened, so a
/// cloud-files placeholder is not fetched. None when it cannot be listed.
pub fn reparse_tag(path: &Path) -> Option<u32> {
    // `\\?\` (also `\\?\UNC\`) lifts the 260-character limit and keeps names as
    // they are: `abc.` is not its neighbour `abc`.
    let name = wide_path(path);
    let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    let find = unsafe {
        FindFirstFileExW(name.as_ptr(), FindExInfoBasic, (&raw mut data).cast(), FindExSearchNameMatch, null(), 0)
    };
    if find == INVALID_HANDLE_VALUE {
        return None;
    }
    unsafe { FindClose(find) };
    Some(if data.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 { data.dwReserved0 } else { 0 })
}

/// Whether deleting `path` through the shell can go to the Recycle Bin.
/// Windows keeps one only on local fixed disks; on network shares, USB
/// sticks, memory cards, discs and SUBST drives it deletes for good.
pub fn has_recycle_bin(path: &str) -> bool {
    // UNC paths are network shares; `\\.\` paths are devices.
    if let Some(rest) = path.strip_prefix(r"\\") {
        match rest.strip_prefix(r"?\") {
            Some(local) if !local.get(..4).is_some_and(|p| p.eq_ignore_ascii_case(r"UNC\")) => {}
            _ => return false,
        }
    }
    let mut root = [0u16; 1024];
    if unsafe { GetVolumePathNameW(wide(path).as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
        return false;
    }
    if unsafe { GetDriveTypeW(root.as_ptr()) } != DRIVE_FIXED {
        return false;
    }
    // A SUBST drive reports the type of the disk it points into, but the
    // shell never recycles on it.
    let root = from_wide(&root);
    let root = root.strip_prefix(r"\\?\").unwrap_or(&root);
    !root.get(..2).filter(|l| l.ends_with(':')).is_some_and(is_subst)
}

/// Whether the drive letter `letter` (`X:`) is a SUBST drive: a folder of
/// another drive under a letter of its own, in this logon session only. It
/// reports the type of the disk it points into; its target is a `\??\` path
/// where a volume's is a `\Device\` one.
pub fn is_subst(letter: &str) -> bool {
    let mut target = [0u16; 1024];
    let n = unsafe { QueryDosDeviceW(wide(letter).as_ptr(), target.as_mut_ptr(), target.len() as u32) };
    n > 0 && from_wide(&target).starts_with(r"\??\")
}

/// The Recycle Bin settings of the volume holding `path` (Recycle Bin
/// properties in Explorer). Missing settings mean Windows defaults: recycle,
/// with a size limit we do not know.
pub struct RecyclePolicy {
    /// "Don't move files to the Recycle Bin. Remove files immediately."
    pub nuke: bool,
    /// Items larger than this cannot be recycled; Windows offers to delete them for good.
    pub max_bytes: Option<u64>,
}

pub fn recycle_policy(path: &str) -> RecyclePolicy {
    let mut policy = RecyclePolicy { nuke: false, max_bytes: None };
    let mut root = [0u16; 1024];
    if unsafe { GetVolumePathNameW(wide(path).as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
        return policy;
    }
    let mut volume = [0u16; 64];
    if unsafe { GetVolumeNameForVolumeMountPointW(root.as_ptr(), volume.as_mut_ptr(), volume.len() as u32) } == 0 {
        return policy;
    }
    // `\\?\Volume{guid}\` → `{guid}`
    let volume = from_wide(&volume);
    let Some(guid) = volume.find('{').and_then(|s| volume[s..].find('}').map(|e| &volume[s..=s + e])) else {
        return policy;
    };
    let key = wide(&format!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\BitBucket\Volume\{guid}"));
    let dword = |name: &str| -> Option<u32> {
        let mut value = 0u32;
        let mut size = 4u32;
        let err = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                wide(name).as_ptr(),
                RRF_RT_REG_DWORD,
                null_mut(),
                (&mut value as *mut u32).cast(),
                &mut size,
            )
        };
        (err == 0).then_some(value)
    };
    policy.nuke = dword("NukeOnDelete").is_some_and(|v| v != 0) || recycling_disabled_by_policy();
    // MaxCapacity is in MB.
    policy.max_bytes = dword("MaxCapacity").map(|mb| mb as u64 * 1024 * 1024);
    policy
}

/// The "Do not move deleted files to the Recycle Bin" group policy, which
/// overrides every volume's own setting.
fn recycling_disabled_by_policy() -> bool {
    let key = wide(r"Software\Microsoft\Windows\CurrentVersion\Policies\Explorer");
    let name = wide("NoRecycleFiles");
    [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE].into_iter().any(|root| {
        let mut value = 0u32;
        let mut size = 4u32;
        let err = unsafe {
            RegGetValueW(root, key.as_ptr(), name.as_ptr(), RRF_RT_REG_DWORD, null_mut(), (&mut value as *mut u32).cast(), &mut size)
        };
        err == 0 && value != 0
    })
}

/// The volume GUID path (`\\?\Volume{...}\`) of a drive letter, which tells
/// two disks apart that got the same letter at different times. None for
/// drives without one (network drives, SUBST).
pub fn volume_id(letter: &str) -> Option<String> {
    let root = wide(&format!("{letter}\\"));
    let mut volume = [0u16; 64];
    let ok = unsafe { GetVolumeNameForVolumeMountPointW(root.as_ptr(), volume.as_mut_ptr(), volume.len() as u32) };
    (ok != 0).then(|| from_wide(&volume)).filter(|v| !v.is_empty())
}

/// `C:\PROGRA~1\x` → `C:\Program Files\x`, and drops a `\\?\` prefix, so a
/// copy started through a short or extended path still matches. Junctions
/// are left alone on purpose: only the path as written is compared. The
/// path as given (without `\\?\`) when Windows cannot tell its long names;
/// a check against short names asks `try_long_path` instead.
pub(crate) fn long_path(path: &str) -> String {
    try_long_path(path).unwrap_or_else(|| strip_verbatim(path).to_string())
}

/// `long_path`, but None when Windows cannot tell the long names (a folder
/// on the way that cannot be listed, a path not there): then a short name
/// in it stays as it is, which a check against short names must not take.
pub(crate) fn try_long_path(path: &str) -> Option<String> {
    let path = strip_verbatim(path);
    let drive = path.as_bytes().get(1) == Some(&b':') && path.as_bytes().get(2) == Some(&b'\\') && !path.contains('/');
    // Past MAX_PATH a drive path is read whole only in its `\\?\` form; a
    // shorter one may still have a long form past it, so that form is tried too.
    if path.len() >= 260 && path.as_bytes().get(1) == Some(&b':') {
        return long_path_of(path, true);
    }
    long_path_of(path, false).or_else(|| if drive { long_path_of(path, true) } else { None })
}

fn strip_verbatim(path: &str) -> &str {
    path.strip_prefix(r"\\?\").filter(|p| !p.starts_with(r"UNC\")).unwrap_or(path)
}

fn long_path_of(path: &str, verbatim: bool) -> Option<String> {
    let short = if verbatim { wide(&format!(r"\\?\{path}")) } else { wide(path) };
    let mut buf = vec![0u16; 1024];
    let mut n = unsafe { GetLongPathNameW(short.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n >= buf.len() {
        // Too long for the buffer: n is the room it takes.
        buf.resize(n + 1, 0);
        n = unsafe { GetLongPathNameW(short.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
    }
    if n == 0 || n >= buf.len() {
        return None;
    }
    let long = from_wide(&buf[..n]);
    Some(match long.strip_prefix(r"\\?\") {
        Some(rest) if verbatim => rest.to_string(),
        _ => long,
    })
}

/// Whether a name in `path` may be a short 8.3 one (`PROGRA~1`,
/// `LONGFI~12.TXT`): it has a `~` with a digit after it.
pub(crate) fn looks_short(path: &str) -> bool {
    path.split(['\\', '/']).any(|name| name.as_bytes().windows(2).any(|w| w[0] == b'~' && w[1].is_ascii_digit()))
}

/// Whether this program runs from Program Files, which only administrators
/// can change. Elevating a copy that sits in a folder the user can write to
/// (a per-user install, or any folder picked in the installer) would let any
/// program running as the user swap the binary and get administrator rights
/// at the next UAC prompt.
pub fn exe_in_program_files() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let exe = long_path(&exe.to_string_lossy()).to_lowercase();
    [&FOLDERID_ProgramFiles, &FOLDERID_ProgramFilesX64, &FOLDERID_ProgramFilesX86]
        .into_iter()
        .filter_map(known_folder)
        .any(|dir| exe.starts_with(&format!("{}\\", dir.to_lowercase().trim_end_matches('\\'))))
}

/// How many other processes run this program file: other windows, an MCP
/// server (`--mcp`) a Claude app started, a helper another window started.
/// This process's own helpers are left out. The update installer closes all
/// of them by force (Restart Manager), so the window warns first. One whose
/// path cannot be read (elevated, say) counts when its name matches.
pub fn other_instances() -> u32 {
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let Ok(exe) = std::env::current_exe() else { return 0 };
    let exe = long_path(&exe.to_string_lossy());
    let Some(name) = Path::new(&exe).file_name().map(|n| n.to_string_lossy().into_owned()) else { return 0 };
    let me = std::process::id();
    let path_of = |pid: u32| unsafe {
        let process = Handle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid));
        if process.0.is_null() {
            return None;
        }
        let mut buf = vec![0u16; 32768];
        let mut len = buf.len() as u32;
        (QueryFullProcessImageNameW(process.0, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) != 0)
            .then(|| long_path(&String::from_utf16_lossy(&buf[..len as usize])))
    };
    unsafe {
        let snapshot = Handle(CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0));
        if snapshot.0 == INVALID_HANDLE_VALUE {
            return 0;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut count = 0;
        let mut more = Process32FirstW(snapshot.0, &mut entry) != 0;
        while more {
            let (pid, parent) = (entry.th32ProcessID, entry.th32ParentProcessID);
            if pid != me
                && parent != me
                && from_wide(&entry.szExeFile).eq_ignore_ascii_case(&name)
                && path_of(pid).is_none_or(|p| p.eq_ignore_ascii_case(&exe))
            {
                count += 1;
            }
            more = Process32NextW(snapshot.0, &mut entry) != 0;
        }
        count
    }
}

fn known_folder(id: &GUID) -> Option<String> {
    unsafe {
        let mut p: PWSTR = null_mut();
        let hr = SHGetKnownFolderPath(id, 0, null_mut(), &mut p);
        let result = if hr >= 0 && !p.is_null() {
            let len = (0..).take_while(|&i| *p.add(i) != 0).count();
            Some(String::from_utf16_lossy(std::slice::from_raw_parts(p, len)))
        } else {
            None
        };
        if !p.is_null() {
            CoTaskMemFree(p.cast());
        }
        result.filter(|s| !s.is_empty())
    }
}

/// Renames without ever replacing: if another item already has the new name
/// this fails with `AlreadyExists`, while a case-only change of the same item
/// (`émile` → `Émile` too) goes through. Both paths are taken exactly as
/// they are (`wide_path`): renaming `abc.` renames that item, never its
/// neighbour `abc`, so the new name must be one Windows keeps as it is.
pub fn rename(from: &Path, to: &Path) -> io::Result<()> {
    let (from, to) = (wide_path(from), wide_path(to));
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub enum ElevateError {
    Cancelled,
    Failed(String),
}

/// Closes the handle when dropped.
struct Handle(HANDLE);

// A kernel handle may be used and closed from any thread (an admin session
// lives in a static and is used from whichever worker runs the request).
unsafe impl Send for Handle {}

impl Handle {
    fn take(&mut self) -> Handle {
        std::mem::replace(self, Handle(null_mut()))
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// A connected helper: read what it writes into its pipe, as it arrives, and
/// (for `run_elevated_duplex`) write requests to it.
pub struct Elevated {
    pipe: PipeConn,
    process: Handle,
}

impl Read for Elevated {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.pipe.read(buf)
    }
}

impl Write for Elevated {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.pipe.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.pipe.flush()
    }
}

impl Elevated {
    /// The helper's exit code, waiting a few seconds for it to finish.
    pub fn exit_code(&self) -> Option<u32> {
        exit_code(&self.process, 5000)
    }

    /// Whether the helper process is still running.
    pub fn running(&self) -> bool {
        unsafe { WaitForSingleObject(self.process.0, 0) == WAIT_TIMEOUT }
    }

    /// Reads and writes on the pipe give up at `deadline` (see PipeConn).
    pub fn set_deadline(&mut self, deadline: Option<Instant>) {
        self.pipe.set_deadline(deadline);
    }
}

/// Starts this executable again with administrator rights (UAC prompt),
/// passing `args` and then the name of a new pipe, and returns that pipe once
/// the new process has connected. The child only ever writes into the pipe;
/// it is never given a file path. Only elevated processes can open the pipe,
/// and only the process started here is accepted. If it exits without
/// connecting, this returns instead of waiting forever.
pub fn run_elevated_piped(args: &str) -> Result<Elevated, ElevateError> {
    launch_elevated(args, PIPE_ACCESS_INBOUND)
}

/// Like `run_elevated_piped`, with a pipe that goes both ways: the app writes
/// requests and the helper answers them (the admin session, admin.rs). Same
/// name, permission and process checks; the helper still gets no file path
/// on its command line, only the pipe's name.
pub fn run_elevated_duplex(args: &str) -> Result<Elevated, ElevateError> {
    launch_elevated(args, PIPE_ACCESS_DUPLEX)
}

fn launch_elevated(args: &str, access: FILE_FLAGS_AND_ATTRIBUTES) -> Result<Elevated, ElevateError> {
    let failed = |e: io::Error| ElevateError::Failed(e.to_string());
    let name = pipe_name().map_err(failed)?;
    // Listening starts before the launch, so even a helper that fails at once
    // finds the pipe and can say why.
    let server = PipeServer::listen_with(&name, HELPER_PIPE_SDDL, access).map_err(failed)?;
    let process = run_as_admin(&format!("{args} {name}"))?;
    let child = unsafe { GetProcessId(process.0) };
    match server.accept(process.0, child) {
        Ok(pipe) => Ok(Elevated { pipe, process }),
        Err(e) => Err(match exit_code(&process, 5000) {
            Some(code) => ElevateError::Failed(format!("{e} (helper exit code {code})")),
            None => failed(e),
        }),
    }
}

/// For the admin helper: turns on SeBackupPrivilege in this (elevated)
/// process, so a file or folder opened with FILE_FLAG_BACKUP_SEMANTICS can be
/// read whatever its permissions say. Reading only: SeRestorePrivilege (the
/// writing half) is never asked for. False when the token does not hold it
/// (not elevated).
pub fn enable_backup_privilege() -> bool {
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let token = Handle(token);
        let mut privileges = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES { Luid: std::mem::zeroed(), Attributes: SE_PRIVILEGE_ENABLED }],
        };
        if LookupPrivilegeValueW(null(), SE_BACKUP_NAME, &mut privileges.Privileges[0].Luid) == 0 {
            return false;
        }
        // Succeeds even when the privilege is missing; the last error tells.
        AdjustTokenPrivileges(token.0, 0, &privileges, 0, null_mut(), null_mut()) != 0
            && GetLastError() != ERROR_NOT_ALL_ASSIGNED
    }
}

/// ShellExecuteEx with the `runas` verb. Returns once the UAC prompt is
/// answered and the process exists.
fn run_as_admin(params: &str) -> Result<Handle, ElevateError> {
    let exe = std::env::current_exe().map_err(|e| ElevateError::Failed(e.to_string()))?;
    let exe = wide_os(exe.as_os_str());
    let verb = wide("runas");
    let params = wide(params);
    unsafe {
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS;
        info.lpVerb = verb.as_ptr();
        info.lpFile = exe.as_ptr();
        info.lpParameters = params.as_ptr();
        info.nShow = SW_HIDE;
        if ShellExecuteExW(&mut info) == 0 {
            let err = GetLastError();
            return Err(if err == ERROR_CANCELLED {
                ElevateError::Cancelled
            } else {
                ElevateError::Failed(format!("ShellExecuteEx error {err}"))
            });
        }
        if info.hProcess.is_null() {
            return Err(ElevateError::Failed("no process handle".into()));
        }
        Ok(Handle(info.hProcess))
    }
}

fn exit_code(process: &Handle, wait_ms: u32) -> Option<u32> {
    unsafe {
        if WaitForSingleObject(process.0, wait_ms) != WAIT_OBJECT_0 {
            return None;
        }
        let mut code = 0u32;
        (GetExitCodeProcess(process.0, &mut code) != 0).then_some(code)
    }
}

/// Random bits from the system's generator, for names that must not repeat
/// (plan ids). Should that fail, the time and process id stand in.
pub(crate) fn random_u64() -> u64 {
    let mut r = [0u8; 8];
    let status = unsafe { BCryptGenRandom(null_mut(), r.as_mut_ptr(), r.len() as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    if status == 0 {
        return u64::from_le_bytes(r);
    }
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    nanos.rotate_left(17) ^ u64::from(std::process::id()).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// A fresh, unguessable pipe name: `\\.\pipe\fine-file-<pid>-<16 hex>`.
fn pipe_name() -> io::Result<String> {
    let mut r = [0u8; 8];
    let status = unsafe { BCryptGenRandom(null_mut(), r.as_mut_ptr(), r.len() as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    if status != 0 {
        return Err(io::Error::other(format!("BCryptGenRandom failed ({status:#x})")));
    }
    Ok(format!(r"\\.\pipe\fine-file-{}-{:016x}", std::process::id(), u64::from_le_bytes(r)))
}

/// For the helper: the process id inside a name made by `pipe_name`, or None
/// when `name` is anything else, so the helper can never be aimed at a file.
pub fn helper_pipe_server(name: &str) -> Option<u32> {
    let (pid, hex) = name.strip_prefix(r"\\.\pipe\fine-file-")?.split_once('-')?;
    let pid_ok = (1..=10).contains(&pid.len()) && pid.bytes().all(|b| b.is_ascii_digit());
    let hex_ok = hex.len() == 16 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if !(pid_ok && hex_ok) {
        return None;
    }
    pid.parse().ok()
}

/// For the helper: opens the app's pipe for writing. The handle must really
/// be a pipe served by process `server`, and the server is not allowed to
/// act with this (elevated) process's rights.
pub fn open_pipe_client(name: &str, server: u32) -> io::Result<File> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .access_mode(GENERIC_WRITE | FILE_READ_ATTRIBUTES)
        .security_qos_flags(SECURITY_ANONYMOUS)
        .open(name)?;
    check_pipe_server(&file, server)?;
    Ok(file)
}

/// For the admin helper: opens the app's two-way pipe, with the same checks
/// as `open_pipe_client` (a pipe, served by process `server`, which may not
/// act with this process's rights). Overlapped, so a wait for the next
/// request can end (the helper stops when idle).
pub fn open_duplex_pipe_client(name: &str, server: u32) -> io::Result<PipeConn> {
    let file = std::fs::OpenOptions::new()
        .access_mode(GENERIC_READ | GENERIC_WRITE)
        .custom_flags(FILE_FLAG_OVERLAPPED)
        .security_qos_flags(SECURITY_ANONYMOUS)
        .open(name)?;
    check_pipe_server(&file, server)?;
    PipeConn::new(Handle(file.into_raw_handle() as HANDLE))
}

fn check_pipe_server(file: &File, server: u32) -> io::Result<()> {
    let h = file.as_raw_handle() as HANDLE;
    let mut pid = 0u32;
    let is_pipe = unsafe { GetFileType(h) == FILE_TYPE_PIPE && GetNamedPipeServerProcessId(h, &mut pid) != 0 };
    if !is_pipe || pid != server {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "not the app's pipe"));
    }
    Ok(())
}

fn create_pipe(name: &str, sddl: &str, access: FILE_FLAGS_AND_ATTRIBUTES) -> io::Result<Handle> {
    unsafe {
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(wide(sddl).as_ptr(), SDDL_REVISION_1, &mut sd, null_mut()) == 0 {
            return Err(io::Error::last_os_error());
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd,
            bInheritHandle: 0,
        };
        // One instance, and creation fails if the name is already taken, so
        // no other process can sit in front of this pipe.
        let out_buffer = if access == PIPE_ACCESS_INBOUND { 0 } else { 64 << 10 };
        let pipe = CreateNamedPipeW(
            wide(name).as_ptr(),
            access | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            out_buffer,
            1 << 20,
            0,
            &sa,
        );
        let err = io::Error::last_os_error();
        LocalFree(sd);
        if pipe == INVALID_HANDLE_VALUE { Err(err) } else { Ok(Handle(pipe)) }
    }
}

/// The server end of a one-client pipe, already listening. The connect is
/// overlapped so the wait for a client can be given up.
struct PipeServer {
    pipe: Handle,
    event: Handle,
    /// Boxed: the kernel may write to it until the connect finishes or is cancelled.
    ov: Box<OVERLAPPED>,
    pending: bool,
}

impl PipeServer {
    /// A pipe the client only writes into (the scan helper).
    #[cfg(test)]
    fn listen(name: &str, sddl: &str) -> io::Result<PipeServer> {
        PipeServer::listen_with(name, sddl, PIPE_ACCESS_INBOUND)
    }

    /// `access`: PIPE_ACCESS_INBOUND (client writes only) or PIPE_ACCESS_DUPLEX.
    fn listen_with(name: &str, sddl: &str, access: FILE_FLAGS_AND_ATTRIBUTES) -> io::Result<PipeServer> {
        let pipe = create_pipe(name, sddl, access)?;
        let event = Handle(unsafe { CreateEventW(null(), 1, 0, null()) });
        if event.0.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut ov: Box<OVERLAPPED> = Box::new(unsafe { std::mem::zeroed() });
        ov.hEvent = event.0;
        let pending = unsafe {
            if ConnectNamedPipe(pipe.0, &mut *ov) != 0 {
                false
            } else {
                match GetLastError() {
                    ERROR_IO_PENDING => true,
                    ERROR_PIPE_CONNECTED => false,
                    e => return Err(os_error(e)),
                }
            }
        };
        Ok(PipeServer { pipe, event, ov, pending })
    }

    /// Waits until a client connects, or gives up once `give_up` (the helper
    /// process) is signalled first. Only process `client_pid` is accepted.
    fn accept(mut self, give_up: HANDLE, client_pid: u32) -> io::Result<PipeConn> {
        if self.pending {
            let handles = [self.event.0, give_up];
            let woke = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
            // Even after a give-up the client may have won the race; finish() tells.
            if !self.finish(woke != WAIT_OBJECT_0) {
                return Err(io::Error::new(io::ErrorKind::NotConnected, "helper exited before connecting"));
            }
        }
        let mut pid = 0u32;
        if unsafe { GetNamedPipeClientProcessId(self.pipe.0, &mut pid) } == 0 || pid != client_pid {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "unexpected process on the helper pipe"));
        }
        Ok(PipeConn { pipe: self.pipe.take(), event: self.event.take(), deadline: None })
    }

    /// Ends a pending connect, cancelling it first when asked. Returns whether
    /// a client is connected.
    fn finish(&mut self, cancel: bool) -> bool {
        self.pending = false;
        let mut n = 0u32;
        unsafe {
            if cancel {
                CancelIoEx(self.pipe.0, &*self.ov);
            }
            GetOverlappedResult(self.pipe.0, &*self.ov, &mut n, 1) != 0
        }
    }
}

impl Drop for PipeServer {
    fn drop(&mut self) {
        if self.pending {
            self.finish(true);
        }
    }
}

/// One end of a helper pipe (the app's server end, or the admin helper's
/// client end), opened for overlapped I/O. Each read or write waits for
/// itself only, until the deadline when one is set: then it is cancelled and
/// fails with `TimedOut`, so a helper (or an app) that stops answering cannot
/// hold the other side forever. The other side closing its end ends a read
/// (end of stream) and fails a write.
pub struct PipeConn {
    pipe: Handle,
    event: Handle,
    deadline: Option<Instant>,
}

impl PipeConn {
    fn new(pipe: Handle) -> io::Result<PipeConn> {
        let event = Handle(unsafe { CreateEventW(null(), 1, 0, null()) });
        if event.0.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(PipeConn { pipe, event, deadline: None })
    }

    /// None: wait as long as it takes.
    pub fn set_deadline(&mut self, deadline: Option<Instant>) {
        self.deadline = deadline;
    }

    /// One ReadFile (or WriteFile) of at most 1 MiB, waited for. `buf` must
    /// stay valid for `len` bytes: the kernel is done with it (and with the
    /// OVERLAPPED) before this returns, even when it gives up.
    fn transfer(&mut self, write: bool, buf: *mut u8, len: usize) -> io::Result<usize> {
        let len = len.min(1 << 20) as u32;
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = self.event.0;
        let mut n = 0u32;
        unsafe {
            let started = if write {
                WriteFile(self.pipe.0, buf.cast_const(), len, null_mut(), &mut ov)
            } else {
                ReadFile(self.pipe.0, buf, len, null_mut(), &mut ov)
            };
            if started == 0 {
                let e = GetLastError();
                if e != ERROR_IO_PENDING {
                    return Err(os_error(e));
                }
            }
            let wait = match self.deadline {
                None => INFINITE,
                Some(d) => d.saturating_duration_since(Instant::now()).as_millis().min(INFINITE as u128 - 1) as u32,
            };
            if WaitForSingleObject(self.event.0, wait) != WAIT_OBJECT_0 {
                CancelIoEx(self.pipe.0, &ov);
                // Waits until the cancel (or a completion that beat it) is through.
                if GetOverlappedResult(self.pipe.0, &ov, &mut n, 1) != 0 {
                    return Ok(n as usize);
                }
                let e = GetLastError();
                return Err(if e == ERROR_OPERATION_ABORTED {
                    io::Error::new(io::ErrorKind::TimedOut, "no answer in time on the helper pipe")
                } else {
                    os_error(e)
                });
            }
            if GetOverlappedResult(self.pipe.0, &ov, &mut n, 0) == 0 {
                return Err(os_error(GetLastError()));
            }
        }
        Ok(n as usize)
    }
}

impl Read for PipeConn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.transfer(false, buf.as_mut_ptr(), buf.len()) {
            // The other side closing its end is the end of the stream.
            Err(e) if e.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) => Ok(0),
            r => r,
        }
    }
}

impl Write for PipeConn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // WriteFile only reads from the buffer.
        self.transfer(true, buf.as_ptr().cast_mut(), buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // A write is complete once WriteFile is: nothing is buffered here.
        Ok(())
    }
}

/// Opens a file or folder with its default program, like double-clicking it.
pub fn open_path(path: &str) -> Result<(), String> {
    let verb = wide("open");
    let file = wide(path);
    let r = unsafe {
        ShellExecuteW(null_mut(), verb.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL)
    };
    // ShellExecute reports success with a value greater than 32.
    if r as isize > 32 { Ok(()) } else { Err(format!("열 수 없어요 (오류 {})", r as isize)) }
}

/// Opens Windows Explorer with the item selected.
pub fn reveal_path(path: &str) -> Result<(), String> {
    std::process::Command::new("explorer.exe")
        .raw_arg(format!("/select,\"{path}\""))
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// `path` as a NUL-terminated wide string for the file APIs. A full drive or
/// UNC path gets `\\?\` in front, so names past 260 characters work and are
/// taken exactly as they are. A path with `/`, `.`, `..` or an empty part
/// is passed as written, since `\\?\` would take those literally too.
pub(crate) fn wide_path(path: &Path) -> Vec<u16> {
    let w: Vec<u16> = path.as_os_str().encode_wide().collect();
    let (bs, dot) = (b'\\' as u16, b'.' as u16);
    let drive = w.len() >= 3 && w[0] < 128 && (w[0] as u8).is_ascii_alphabetic() && w[1] == b':' as u16 && w[2] == bs;
    let unc = w.len() > 2 && w[0] == bs && w[1] == bs && w[2] != b'?' as u16 && w[2] != dot;
    let body = if unc { &w[2..] } else { &w[..] };
    let mut parts = body.split(|&c| c == bs).peekable();
    let mut clean = !w.contains(&(b'/' as u16));
    while let Some(part) = parts.next() {
        // A trailing `\` leaves one empty part at the end, which is fine.
        if part == [dot] || part == [dot, dot] || (part.is_empty() && parts.peek().is_some()) {
            clean = false;
        }
    }
    let prefix = match (drive, unc, clean) {
        (true, _, true) => r"\\?\",
        (_, true, true) => r"\\?\UNC\",
        _ => return wide_os(path.as_os_str()),
    };
    prefix.encode_utf16().chain(body.iter().copied()).chain(std::iter::once(0)).collect()
}

/// `path` for std::fs calls, with `\\?\` in front whenever `wide_path` would
/// put it there. std passes such a path on as it is, so a name that ends in
/// a dot or a space (WSL and git can make those) means that very name, where
/// plain Win32 parsing would drop the dot and find another item; long paths
/// work too. Listing a folder through it gives its entries the prefix.
pub(crate) fn verbatim(path: &Path) -> PathBuf {
    let mut w = wide_path(path);
    w.pop();
    PathBuf::from(OsString::from_wide(&w))
}

/// Whether the shell (Recycle Bin, Explorer) can tell this path from others:
/// it parses paths the Win32 way, which drops a dot or space at the end of a
/// name, so `abc.` would mean `abc`.
pub(crate) fn shell_can_name(path: &Path) -> bool {
    path.components().all(|c| match c {
        Component::Normal(name) => !name.encode_wide().last().is_some_and(|c| c == b'.' as u16 || c == b' ' as u16),
        _ => true,
    })
}

/// The volume that holds `path`, as its GUID path when it has one (so a
/// folder mounted from another disk counts as that disk), else its root.
fn volume_of(path: &Path) -> Option<String> {
    let mut root = [0u16; 1024];
    // `\\?\` so a path past 260 characters is found too; the root then comes back with it.
    if unsafe { GetVolumePathNameW(wide_path(path).as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
        return None;
    }
    let root = from_wide(&root);
    let root = match root.strip_prefix(r"\\?\") {
        Some(unc) if unc.get(..4).is_some_and(|p| p.eq_ignore_ascii_case(r"UNC\")) => format!(r"\\{}", &unc[4..]),
        Some(local) if local.get(1..3) == Some(r":\") => local.to_string(),
        _ => root,
    };
    let mut volume = [0u16; 64];
    let ok = unsafe { GetVolumeNameForVolumeMountPointW(wide(&root).as_ptr(), volume.as_mut_ptr(), volume.len() as u32) };
    Some(if ok != 0 { from_wide(&volume) } else { root })
}

/// Whether two paths are on one volume, so moving between them is a rename.
/// When that cannot be told the answer is no (the caller then copies).
pub fn same_volume(a: &Path, b: &Path) -> bool {
    matches!((volume_of(a), volume_of(b)), (Some(x), Some(y)) if x.eq_ignore_ascii_case(&y))
}

/// Whether two paths name the same file or folder, say because one of them
/// goes through a junction. A link is compared as itself, not as what it
/// points to, and nothing is fetched for a cloud-files placeholder.
///
/// By the 128-bit file id (ReFS, Dev Drives: the old 64-bit index is not
/// unique there), or the 64-bit one where that is all there is (FAT). Some
/// network servers give every file id 0; without a usable id only the very
/// same path counts as the same entry.
pub fn same_entry(a: &Path, b: &Path) -> bool {
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandle, GetFileInformationByHandleEx, BY_HANDLE_FILE_INFORMATION,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO,
    };
    let id = |p: &Path| -> Option<(u64, [u8; 16])> {
        let f = std::fs::OpenOptions::new()
            .access_mode(0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(verbatim(p))
            .ok()?;
        let h = f.as_raw_handle() as HANDLE;
        let mut ex: FILE_ID_INFO = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<FILE_ID_INFO>() as u32;
        if unsafe { GetFileInformationByHandleEx(h, FileIdInfo, (&raw mut ex).cast(), size) } != 0 {
            return (ex.FileId.Identifier != [0; 16]).then_some((ex.VolumeSerialNumber, ex.FileId.Identifier));
        }
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(h, &mut info) } == 0 {
            return None;
        }
        let index = ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64;
        let mut long = [0u8; 16];
        long[..8].copy_from_slice(&index.to_le_bytes());
        // Marked, so an id of this kind never equals one of the kind above.
        (index != 0).then_some((info.dwVolumeSerialNumber as u64 | (1 << 63), long))
    };
    match (id(a), id(b)) {
        (Some(x), Some(y)) => x == y,
        _ => {
            let text = |p: &Path| p.to_string_lossy().trim_end_matches('\\').to_lowercase();
            text(a) == text(b)
        }
    }
}

/// Lowers the calling thread to background mode: low CPU, I/O and memory
/// priority, so work it does (a sync that runs by itself) makes way for
/// whatever the user is doing. For threads made for such work only.
pub fn background_thread() {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN};
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) };
}

/// The id tauri-build gives the program's icon in its resources.
const APP_ICON_ID: i32 = tauri::utils::platform::WINDOWS_APP_ICON_RESOURCE_ID as i32;

/// Icon handles (HICON), big and small.
#[derive(Clone, Copy, Debug)]
pub struct Icons {
    pub big: usize,
    pub small: usize,
}

/// The icon of the program file `exe`, `big` and `small` pixels square: the
/// one tauri-build puts in the program's resources, else the file's first.
/// None when the file has none. The caller owns the handles.
pub fn file_icons(exe: &Path, big: u32, small: u32) -> Option<Icons> {
    use windows_sys::Win32::UI::Shell::SHDefExtractIconW;
    use windows_sys::Win32::UI::WindowsAndMessaging::DestroyIcon;
    let path = wide_os(exe.as_os_str());
    let sizes = (small.min(0xFFFF) << 16) | big.min(0xFFFF);
    // A negative index is a resource id.
    for index in [-APP_ICON_ID, 0] {
        let (mut b, mut s) = (null_mut(), null_mut());
        let hr = unsafe { SHDefExtractIconW(path.as_ptr(), index, 0, &mut b, &mut s, sizes) };
        if hr == 0 && !b.is_null() && !s.is_null() {
            return Some(Icons { big: b as usize, small: s as usize });
        }
        for h in [b, s].into_iter().filter(|h| !h.is_null()) {
            unsafe { DestroyIcon(h) };
        }
    }
    None
}

/// Gives the window `hwnd` this program's own icon as both its big icon
/// (taskbar, Alt+Tab) and its small one (title bar). Tauri sets only the
/// small one, from the bundle's PNGs; without a big one Windows shows what
/// it cached for the program file, which after an update in place can be an
/// older version's icon. The icons are loaded once, at the sizes Windows
/// uses for this screen, and kept for as long as the process runs (a window
/// does not own the icons it is given). False when there was none to give.
pub fn set_window_icons(hwnd: usize) -> bool {
    use std::sync::OnceLock;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SendMessageW, ICON_BIG, ICON_SMALL, SM_CXICON, SM_CXSMICON, WM_SETICON,
    };
    static ICONS: OnceLock<Option<Icons>> = OnceLock::new();
    let icons = *ICONS.get_or_init(|| {
        let exe = std::env::current_exe().ok()?;
        let (big, small) = unsafe { (GetSystemMetrics(SM_CXICON), GetSystemMetrics(SM_CXSMICON)) };
        file_icons(&exe, big.max(16) as u32, small.max(16) as u32)
    });
    let Some(icons) = icons else { return false };
    unsafe {
        SendMessageW(hwnd as HANDLE, WM_SETICON, ICON_BIG as usize, icons.big as isize);
        SendMessageW(hwnd as HANDLE, WM_SETICON, ICON_SMALL as usize, icons.small as isize);
    }
    true
}

/// A Yes/No question in a Windows message box, "No" the default, over the
/// window `owner` (an HWND) when given. True for "Yes".
pub fn ask_yes_no(owner: Option<usize>, title: &str, text: &str) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, IDYES, MB_DEFBUTTON2, MB_ICONWARNING, MB_YESNO};
    let owner = owner.map_or(null_mut(), |h| h as HANDLE);
    let style = MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2;
    unsafe { MessageBoxW(owner, wide(text).as_ptr(), wide(title).as_ptr(), style) == IDYES }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn other_instances_leave_out_this_process_and_its_children() {
        // The test binary runs once; a copy it starts is its own child, as the helpers are.
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "win::tests::no_such_test", "--test-threads=1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let n = other_instances();
        let _ = child.wait();
        assert_eq!(n, 0);
    }

    #[test]
    fn volume_ids_and_program_files_check() {
        let c = volume_id("C:").expect("C: has a volume GUID");
        assert!(c.starts_with(r"\\?\Volume{") && c.ends_with("}\\"), "{c}");
        assert_eq!(volume_id("C:"), Some(c));
        // Test binaries run from target\, never from Program Files.
        assert!(!exe_in_program_files());
        let pf = known_folder(&FOLDERID_ProgramFiles).unwrap();
        assert!(long_path(&format!(r"\\?\{pf}")).eq_ignore_ascii_case(&pf));
        // What Windows cannot spell out is told apart from what has no short name.
        let gone = r"C:\no such folder ff\PROGRA~1\x";
        assert_eq!(try_long_path(gone), None);
        assert_eq!(long_path(&format!(r"\\?\{gone}")), gone);
        assert!(looks_short(gone) && looks_short(r"C:\LONGFI~12.TXT") && looks_short("C:/a/B~3"));
        assert!(!looks_short(r"C:\Users\me\notes~.txt") && !looks_short(r"C:\a~b\c") && !looks_short(r"C:\Program Files"));
        // Past MAX_PATH too, a short name is spelled out.
        let base = std::env::temp_dir().join(format!("ff-long-path-{}", std::process::id()));
        let named = base.join("Long Folder Name For Test");
        let deep = named.join("x".repeat(120)).join("y".repeat(120));
        std::fs::create_dir_all(verbatim(&deep)).unwrap();
        let mut buf = vec![0u16; 1024];
        let n = unsafe { windows_sys::Win32::Storage::FileSystem::GetShortPathNameW(wide_os(named.as_os_str()).as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
        let short_dir = from_wide(&buf[..n.min(buf.len())]);
        let short_name = short_dir.rsplit('\\').next().unwrap_or_default().to_string();
        if n > 0 && !short_name.eq_ignore_ascii_case("Long Folder Name For Test") {
            let mixed = base.join(&short_name).join("x".repeat(120)).join("y".repeat(120)).to_string_lossy().into_owned();
            assert!(mixed.len() >= 260, "{mixed}");
            assert!(long_path(&mixed).eq_ignore_ascii_case(&deep.to_string_lossy()), "{mixed}");
            assert!(long_path(&format!(r"\\?\{mixed}")).eq_ignore_ascii_case(&deep.to_string_lossy()), "{mixed}");
        }
        let _ = std::fs::remove_dir_all(verbatim(&base));
        // Reading the bin settings of C: must not fail or panic.
        let _ = recycle_policy(r"C:\Windows");
        // A folder that does not exist yet is measured on its volume.
        assert!(free_space(Path::new(r"C:\Windows")).is_some());
        assert!(free_space(Path::new(r"C:\Windows\no such folder\deeper")).is_some());
    }
    #[test]
    fn program_icons_come_from_the_file() {
        use windows::Win32::Graphics::Gdi::{DeleteObject, GetObjectW, BITMAP};
        use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};
        let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
        let explorer = Path::new(&windir).join("explorer.exe");
        let icons = file_icons(&explorer, 48, 20).expect("explorer.exe has an icon");
        // Each at the size asked for.
        for (h, px) in [(icons.big, 48), (icons.small, 20)] {
            let h = HICON(h as *mut _);
            let mut info = ICONINFO::default();
            unsafe { GetIconInfo(h, &mut info) }.unwrap();
            assert!(info.fIcon.as_bool());
            let mut bm = BITMAP::default();
            let got = unsafe { GetObjectW(info.hbmColor.into(), size_of::<BITMAP>() as i32, Some((&raw mut bm).cast())) };
            assert!(got > 0);
            assert_eq!((bm.bmWidth, bm.bmHeight), (px, px));
            unsafe {
                let _ = DeleteObject(info.hbmColor.into());
                let _ = DeleteObject(info.hbmMask.into());
                let _ = DestroyIcon(h);
            }
        }
        // A file without icons, and one that is not there.
        let dir = std::env::temp_dir().join(format!("ff-icon-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let text = dir.join("plain.txt");
        std::fs::write(&text, "no icon here").unwrap();
        assert!(file_icons(&text, 32, 16).is_none());
        assert!(file_icons(&dir.join("missing.exe"), 32, 16).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
        // Our resource id is tauri-build's.
        assert_eq!(APP_ICON_ID, 32512);
    }

    use std::io::Write;
    use windows_sys::Win32::System::Threading::SetEvent;

    /// Lets this unelevated test process be its own pipe client.
    const OWNER_ONLY: &str = "D:P(A;;GA;;;OW)";

    fn event() -> Handle {
        Handle(unsafe { CreateEventW(null(), 1, 0, null()) })
    }

    /// A helper pipe that this test process serves and may also connect to.
    pub(crate) struct OwnPipe {
        pub(crate) name: String,
        server: PipeServer,
    }

    impl OwnPipe {
        pub(crate) fn new() -> OwnPipe {
            let name = pipe_name().unwrap();
            OwnPipe { server: PipeServer::listen(&name, OWNER_ONLY).unwrap(), name }
        }

        /// A two-way one, like `run_elevated_duplex` makes.
        pub(crate) fn duplex() -> OwnPipe {
            let name = pipe_name().unwrap();
            OwnPipe { server: PipeServer::listen_with(&name, OWNER_ONLY, PIPE_ACCESS_DUPLEX).unwrap(), name }
        }

        pub(crate) fn accept(self) -> impl Read {
            self.connection()
        }

        pub(crate) fn connection(self) -> PipeConn {
            let never = event();
            self.server.accept(never.0, std::process::id()).unwrap()
        }
    }

    fn client(name: &str, data: Vec<u8>) -> std::thread::JoinHandle<()> {
        let name = name.to_string();
        std::thread::spawn(move || {
            let mut f = open_pipe_client(&name, std::process::id()).unwrap();
            f.write_all(&data).unwrap();
        })
    }

    #[test]
    fn pipe_names_are_checked_exactly() {
        let name = pipe_name().unwrap();
        assert_eq!(helper_pipe_server(&name), Some(std::process::id()));
        assert_ne!(pipe_name().unwrap(), name);
        for bad in [
            r"C:\Users\me\AppData\Local\x.idx",
            r"\\.\pipe\fine-file-12-0123456789abcdeg",
            r"\\.\pipe\fine-file-12-0123456789abcde",
            r"\\.\pipe\fine-file-12-0123456789abcdef0",
            r"\\.\pipe\fine-file-12-0123456789ABCDEF",
            r"\\.\pipe\fine-file--0123456789abcdef",
            r"\\.\pipe\fine-file-12345678901-0123456789abcdef",
            r"\\.\pipe\fine-file-12-0123456789abcdef\..\..\C:\x",
            r"\\?\pipe\fine-file-12-0123456789abcdef",
            r"\\.\pipe\other-12-0123456789abcdef",
        ] {
            assert_eq!(helper_pipe_server(bad), None, "{bad}");
        }
    }

    #[test]
    fn reply_survives_a_client_that_already_exited() {
        // A helper that fails fast writes its message and is gone before the app waits.
        let name = pipe_name().unwrap();
        let server = PipeServer::listen(&name, OWNER_ONLY).unwrap();
        client(&name, b"\x01no MFT".to_vec()).join().unwrap();
        let never = event();
        let mut got = Vec::new();
        server.accept(never.0, std::process::id()).unwrap().read_to_end(&mut got).unwrap();
        assert_eq!(got, b"\x01no MFT");
    }

    #[test]
    fn large_reply_streams_through() {
        let name = pipe_name().unwrap();
        let server = PipeServer::listen(&name, OWNER_ONLY).unwrap();
        let data: Vec<u8> = (0..5_000_000u32).map(|i| (i * 7 + i / 251) as u8).collect();
        let writer = client(&name, data.clone());
        let never = event();
        let mut got = Vec::new();
        server.accept(never.0, std::process::id()).unwrap().read_to_end(&mut got).unwrap();
        writer.join().unwrap();
        assert!(got == data);
    }

    #[test]
    fn gives_up_when_the_helper_is_gone() {
        let server = PipeServer::listen(&pipe_name().unwrap(), OWNER_ONLY).unwrap();
        let gone = event();
        unsafe { SetEvent(gone.0) };
        let t = Instant::now();
        assert!(server.accept(gone.0, std::process::id()).is_err());
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn only_the_started_process_and_the_named_server_count() {
        let name = pipe_name().unwrap();
        let server = PipeServer::listen(&name, OWNER_ONLY).unwrap();
        // A second server under the same name is refused.
        assert!(PipeServer::listen(&name, OWNER_ONLY).is_err());
        // The client refuses a server other than the one the name promises.
        assert_eq!(open_pipe_client(&name, std::process::id() + 1).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        // The app refuses a client other than the helper it started.
        let name = pipe_name().unwrap();
        let other = PipeServer::listen(&name, OWNER_ONLY).unwrap();
        client(&name, b"\x00fake".to_vec()).join().unwrap();
        let never = event();
        assert_eq!(other.accept(never.0, std::process::id() + 1).err().unwrap().kind(), io::ErrorKind::PermissionDenied);
        drop(server);
    }

    #[test]
    fn helper_pipe_is_closed_to_unelevated_processes() {
        if is_elevated() {
            return;
        }
        let name = pipe_name().unwrap();
        let _server = PipeServer::listen(&name, HELPER_PIPE_SDDL).unwrap();
        assert_eq!(open_pipe_client(&name, std::process::id()).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        let name = pipe_name().unwrap();
        let _server = PipeServer::listen_with(&name, HELPER_PIPE_SDDL, PIPE_ACCESS_DUPLEX).unwrap();
        assert_eq!(open_duplex_pipe_client(&name, std::process::id()).err().unwrap().kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn duplex_pipe_goes_both_ways_and_checks_its_server() {
        // Like the one-way pipe: a client refuses a server other than the one the name promises.
        let other = OwnPipe::duplex();
        assert_eq!(open_duplex_pipe_client(&other.name, std::process::id() + 1).err().unwrap().kind(), io::ErrorKind::PermissionDenied);
        let pipe = OwnPipe::duplex();
        let name = pipe.name.clone();
        let helper = std::thread::spawn(move || {
            let mut c = open_duplex_pipe_client(&name, std::process::id()).unwrap();
            let mut req = [0u8; 4];
            c.read_exact(&mut req).unwrap();
            assert_eq!(&req, b"ping");
            // Bigger than the pipe's buffers: the write waits for the reader.
            c.write_all(&vec![7u8; 3 << 20]).unwrap();
        });
        let mut conn = pipe.connection();
        conn.write_all(b"ping").unwrap();
        let mut got = Vec::new();
        conn.read_to_end(&mut got).unwrap();
        helper.join().unwrap();
        assert!(got.len() == 3 << 20 && got.iter().all(|&b| b == 7));
    }

    #[test]
    fn pipe_waits_end_at_the_deadline() {
        let pipe = OwnPipe::duplex();
        let name = pipe.name.clone();
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let helper = std::thread::spawn(move || {
            let mut c = open_duplex_pipe_client(&name, std::process::id()).unwrap();
            // The helper's idle wait: nothing comes, so it ends.
            c.set_deadline(Some(Instant::now() + Duration::from_millis(100)));
            let mut b = [0u8; 1];
            assert_eq!(c.read(&mut b).unwrap_err().kind(), io::ErrorKind::TimedOut);
            // Still connected after the cancelled read.
            rx.recv().unwrap();
            c.write_all(b"late").unwrap();
            c
        });
        let mut conn = pipe.connection();
        // A helper that does not answer: the app gives up.
        conn.set_deadline(Some(Instant::now() + Duration::from_millis(200)));
        let t = Instant::now();
        let mut b = [0u8; 4];
        assert_eq!(conn.read(&mut b).unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(t.elapsed() >= Duration::from_millis(150) && t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
        tx.send(()).unwrap();
        conn.set_deadline(Some(Instant::now() + Duration::from_secs(10)));
        conn.read_exact(&mut b).unwrap();
        assert_eq!(&b, b"late");
        // The other side closing its end ends the stream instead of waiting for the deadline.
        drop(helper.join().unwrap());
        assert_eq!(conn.read(&mut b).unwrap(), 0);
        assert_eq!(conn.write(b"x").unwrap_err().kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn backup_privilege_needs_elevation() {
        // Unelevated tokens do not hold it; elevated ones do.
        assert_eq!(enable_backup_privilege(), is_elevated());
    }

    #[test]
    fn rename_never_replaces_but_allows_case_changes() {
        let dir = std::env::temp_dir().join(format!("ff-rename-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("émile.txt"), "a").unwrap();
        std::fs::write(dir.join("other.txt"), "b").unwrap();
        rename(&dir.join("émile.txt"), &dir.join("Émile.txt")).unwrap();
        let names: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert!(names.contains(&"Émile.txt".to_string()), "{names:?}");
        let err = rename(&dir.join("Émile.txt"), &dir.join("other.txt")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(dir.join("other.txt")).unwrap(), "b");
        // `abc.` is renamed, not its neighbour `abc` that Win32 parsing would take it for.
        std::fs::write(verbatim(&dir.join("abc")), "neighbour").unwrap();
        std::fs::write(verbatim(&dir.join("abc.")), "dot").unwrap();
        rename(&dir.join("abc."), &dir.join("renamed")).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("renamed")).unwrap(), "dot");
        assert_eq!(std::fs::read_to_string(dir.join("abc")).unwrap(), "neighbour");
        assert!(std::fs::symlink_metadata(verbatim(&dir.join("abc."))).is_err());
        // Past 260 characters too.
        let mut deep = dir.join("deep");
        while deep.as_os_str().len() < 300 {
            deep.push("a-rather-long-folder-name");
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("f.txt"), "deep").unwrap();
        rename(&deep.join("f.txt"), &deep.join("g.txt")).unwrap();
        assert_eq!(std::fs::read_to_string(deep.join("g.txt")).unwrap(), "deep");
        std::fs::remove_dir_all(verbatim(&dir)).unwrap();
    }

    #[test]
    fn reparse_tags_are_read_for_the_exact_name() {
        let dir = std::env::temp_dir().join(format!("ff-reparse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(verbatim(&dir));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir(dir.join("target")).unwrap();
        std::fs::create_dir(dir.join("abc")).unwrap();
        // mklink cannot make `abc.` itself: made as `j`, then renamed.
        let junction = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(dir.join("j"))
            .arg(dir.join("target"))
            .output()
            .is_ok_and(|o| o.status.success())
            && rename(&dir.join("j"), &dir.join("abc.")).is_ok();
        assert_eq!(reparse_tag(&dir.join("abc")), Some(0));
        assert_eq!(reparse_tag(&dir.join("missing")), None);
        if junction {
            assert_eq!(reparse_tag(&dir.join("abc.")), Some(0xA000_0003));
            std::fs::remove_dir(verbatim(&dir.join("abc."))).unwrap();
        } else {
            eprintln!("mklink /J failed; the junction part was skipped");
        }
        // Past 260 characters too (UNC paths get `\\?\UNC\` the same way: wide_path).
        let mut deep = dir.clone();
        while deep.as_os_str().len() < 300 {
            deep.push("a-rather-long-folder-name");
        }
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(reparse_tag(&deep), Some(0));
        std::fs::remove_dir_all(verbatim(&dir)).unwrap();
    }

    #[test]
    fn verbatim_paths_and_names_the_shell_can_take() {
        let v = |p: &str| verbatim(Path::new(p)).to_string_lossy().into_owned();
        assert_eq!(v(r"C:\a\b."), r"\\?\C:\a\b.");
        assert_eq!(v(r"\\server\share\x "), r"\\?\UNC\server\share\x ");
        assert_eq!(v(r"a\b"), r"a\b");
        assert_eq!(v(r"C:\a\..\b"), r"C:\a\..\b");
        assert!(shell_can_name(Path::new(r"C:\a\b.txt")));
        assert!(shell_can_name(Path::new(r"C:\")));
        for odd in [r"C:\a\b.", r"C:\a.\b", r"C:\a\b ", r"\\server\share\x.\y"] {
            assert!(!shell_can_name(Path::new(odd)), "{odd}");
        }
    }

    #[test]
    fn same_entry_and_same_volume() {
        let dir = std::env::temp_dir().join(format!("ff-same-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a"), "a").unwrap();
        std::fs::write(dir.join("b"), "b").unwrap();
        std::fs::hard_link(dir.join("a"), dir.join("also a")).unwrap();
        std::fs::write(verbatim(&dir.join("a.")), "a dot").unwrap();
        assert!(same_entry(&dir.join("a"), &dir.join("also a")));
        assert!(same_entry(&dir.join("a"), &dir.join("A")));
        assert!(!same_entry(&dir.join("a"), &dir.join("b")));
        // Not the neighbour that Win32 parsing would turn it into.
        assert!(!same_entry(&dir.join("a"), &dir.join("a.")));
        // No id to compare: only the same path counts.
        assert!(same_entry(&dir.join("missing"), &dir.join("missing")));
        assert!(!same_entry(&dir.join("missing"), &dir.join("b")));
        // Past 260 characters the volume is still found, so a move there stays a rename.
        let mut deep = dir.clone();
        while deep.as_os_str().len() < 300 {
            deep.push("a-rather-long-folder-name");
        }
        std::fs::create_dir_all(&deep).unwrap();
        assert!(volume_of(&deep).is_some());
        assert_eq!(volume_of(&deep), volume_of(&dir));
        assert!(same_volume(&deep, &dir));
        assert!(volume_of(&dir).unwrap().starts_with(r"\\?\Volume{"), "{:?}", volume_of(&dir));
        std::fs::remove_dir_all(verbatim(&dir)).unwrap();
    }

    #[test]
    fn network_and_device_paths_have_no_recycle_bin() {
        assert!(!has_recycle_bin(r"\\server\share\file.txt"));
        assert!(!has_recycle_bin(r"\\?\UNC\server\share\file.txt"));
        assert!(!has_recycle_bin(r"\\.\pipe\x"));
    }
}
