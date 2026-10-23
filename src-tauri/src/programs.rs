//! Installed programs: what is installed (Uninstall registry keys and Store
//! apps), how big each is, when it was last used and on what evidence, and
//! folders left behind by programs that are gone. Removing a program always
//! runs its own uninstaller; leftover folders become a reviewable cleanup plan.
//!
//! - Installed: the Uninstall keys of HKLM (64-bit and 32-bit views) and of
//!   HKCU, without system components, updates and hotfixes (SystemComponent,
//!   ParentKeyName, ReleaseType) and entries without a name; Store apps from
//!   `Get-AppxPackage` (Windows' own PowerShell, hidden, with a time limit),
//!   without frameworks and Windows' own packages. When that fails, the list
//!   has no Store apps.
//! - Size: what the index measured for the program's own folder (its
//!   InstallLocation, else the folder of its icon or uninstaller) on a
//!   scanned drive, else its own estimate (EstimatedSize). A folder many
//!   programs share (a drive, Program Files itself, AppData) is no program's
//!   own. The disk is never walked here.
//! - Last used: Explorer's launch records (UserAssist: what was started from
//!   the Start menu, the taskbar, the desktop or Explorer, with a run count),
//!   Windows' Prefetch files (every start; read through the admin session,
//!   only when asked for and one is open, or directly when the app runs
//!   elevated), and only without either, when one of its executables was
//!   last read, if NTFS keeps that time at all.
//! - Uninstall: the program's UninstallString (a Windows Installer product:
//!   `msiexec /X{product code}`), started through the shell so that it asks
//!   for administrator rights itself; never its QuietUninstallString. Some
//!   UninstallStrings are silent themselves (Squirrel's `Update.exe
//!   --uninstall -s`, an NSIS `/S`), and the window says so. Store
//!   apps: Remove-AppxPackage for this user. The app deletes nothing itself.
//!   A folder that is exactly a program's own (`program_at`, and AI 검색's
//!   game and program hits) offers the same uninstaller ("제거 (제어판처럼)").
//! - Leftovers: folders of 50 MB or more straight in Program Files (both),
//!   ProgramData, AppData (Local, Roaming) and AppData\Local\Programs, from
//!   the index, that no installed program accounts for by its name,
//!   publisher, folder or executables, leaving out Windows' and Microsoft's
//!   folders, folders many programs share, tools' caches and driver makers'
//!   folders while a program of theirs is installed. Every Uninstall entry
//!   counts here, system components and updates too. A leftover plan is an
//!   ordinary rules-made cleanup plan (plans.rs), whose steps are checked as
//!   any step is: those checks keep plans out of Program Files, ProgramData
//!   and AppData, so such steps are saved "skipped" with why, and the plan
//!   says so. Nothing here loosens them.

use crate::index::{Index, FLAG_DELETED, FLAG_DIR, FLAG_NOT_LINK, FLAG_REPARSE};
use crate::plans::{self, Plan, PlanStep, TrashItem};
use crate::{classify, win, Shared};
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows_sys::core::{GUID, PWSTR};
use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_CANCELLED, ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS,
};
use windows_sys::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegEnumValueW, RegOpenKeyExW, RegQueryInfoKeyW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER,
    HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY, KEY_WOW64_64KEY, REG_DWORD, REG_EXPAND_SZ, REG_SAM_FLAGS, REG_SZ,
};
use windows_sys::Win32::UI::Shell::{
    SHGetKnownFolderPath, ShellExecuteExW, FOLDERID_LocalAppData, FOLDERID_Profile, FOLDERID_ProgramData,
    FOLDERID_ProgramFilesCommonX64, FOLDERID_ProgramFilesCommonX86, FOLDERID_ProgramFilesX64, FOLDERID_ProgramFilesX86,
    FOLDERID_RoamingAppData, FOLDERID_UserProgramFiles, FOLDERID_Windows, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall";
const USERASSIST_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\UserAssist";
const FILESYSTEM_KEY: &str = r"SYSTEM\CurrentControlSet\Control\FileSystem";

const MB: u64 = 1 << 20;
const GB: u64 = 1 << 30;
/// FILETIME ticks in a day.
const DAY: i64 = 864_000_000_000;
/// FILETIME of 1970-01-01 (UTC).
const FILETIME_UNIX: i64 = 116_444_736_000_000_000;
/// Leftover folders are at least this big...
const LEFTOVER_MIN: u64 = 50 * MB;
/// ...this many at most are listed...
const LEFTOVER_MOST: usize = 200;
/// ...and one with an executable or DLL changed within this many days may still be in use.
const RECENT_CODE_DAYS: i64 = 30;
/// Entries of the index one look inside a folder visits at most.
const WALK_MOST: usize = 500_000;
/// Executables of one program's folder looked at (from the index), how deep,
/// and how many of them are asked when they were last read.
const EXES_MOST: usize = 64;
const EXE_DEPTH: usize = 4;
const STAT_MOST: usize = 8;
/// Prefetch keeps this much of an executable's name.
const PREFETCH_NAME: usize = 29;
/// How long the Store app list may take, how long a Store app's removal is
/// waited for (it goes on after that), and how long a list is reused: by the
/// program list, and by AI 검색 (which only needs the apps' folders, and
/// should not wait for PowerShell on every search).
const STORE_LIMIT: Duration = Duration::from_secs(30);
const REMOVE_LIMIT: Duration = Duration::from_secs(120);
const STORE_FRESH: Duration = Duration::from_secs(60);
const STORE_KEPT: Duration = Duration::from_secs(600);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Store apps, without frameworks and Windows' own packages, as JSON (UTF-8:
/// Windows PowerShell would write the console's code page otherwise).
const STORE_LIST: &str = "[Console]::OutputEncoding=[Text.Encoding]::UTF8; Get-AppxPackage | Where-Object {-not $_.IsFramework -and $_.SignatureKind -ne 'System'} | Select-Object Name,PackageFullName,InstallLocation,Version,Publisher,NonRemovable | ConvertTo-Json -Compress";

const NOT_FOUND: &str = "이 프로그램을 찾을 수 없어요. 이미 지웠을 수 있어요 — 목록을 새로 고쳐 주세요";
const BAD_ID: &str = "프로그램 이름이 올바르지 않아요";
const NO_UNINSTALLER: &str = "이 프로그램은 제거 프로그램을 알려 주지 않아서 여기서 지울 수 없어요. Windows 설정 → 앱에서 지워 주세요";
const UNINSTALLER_MISSING: &str = "제거 프로그램 파일이 없어요. 이미 지웠거나 옮겨졌을 수 있어요";
const UAC_CANCELLED: &str = "관리자 권한을 허락하지 않아서 제거 프로그램을 시작하지 않았어요";
const NOT_REMOVABLE: &str = "Windows가 지우지 못하게 한 앱이에요";
const NO_POWERSHELL: &str = "Windows PowerShell을 실행할 수 없어요";
const STORE_SLOW: &str = "Store 앱 목록을 받아 오는 데 너무 오래 걸려서 그만뒀어요";
const STORE_FAILED: &str = "Store 앱 목록을 받아 오지 못했어요";
const REMOVE_FAILED: &str = "앱을 지우지 못했어요";
const SCAN_FIRST: &str = "Program Files·AppData가 있는 드라이브를 먼저 스캔해 주세요. 남은 폴더는 스캔 결과에서 찾아요";
const NOT_LEFTOVER: &str = "남은 폴더 목록에 없는 항목이 있어요. 목록을 새로 고친 뒤 다시 골라 주세요";
const NOTHING_PICKED: &str = "정리할 폴더를 하나 이상 골라 주세요";
const ONE_DRIVE: &str = "한 계획에는 한 드라이브의 폴더만 넣을 수 있어요. 드라이브마다 따로 만들어 주세요";

#[derive(Serialize, Clone, Debug)]
pub struct Program {
    /// Stable id: "<hive>\\<key name>" for registry programs, the package full name for Store apps.
    pub id: String,
    pub name: String,
    pub publisher: Option<String>,
    pub version: Option<String>,
    /// FILETIME of the install date when known.
    pub installed_at: Option<i64>,
    pub install_location: Option<String>,
    pub size: Option<u64>,
    /// "index" (measured from the scan) | "registry" (the program's own estimate) | "unknown"
    pub size_source: String,
    /// FILETIME of the last use when known.
    pub last_used: Option<i64>,
    /// "userassist" | "prefetch" | "exe" | null
    pub last_used_source: Option<String>,
    pub run_count: Option<u32>,
    /// "win32" | "store"
    pub kind: String,
    /// It has an uninstaller the app can start.
    pub can_uninstall: bool,
    /// Machine-wide (HKLM) install: its uninstaller will ask for administrator rights.
    pub machine_wide: bool,
}

#[derive(Serialize, Clone, Debug)]
pub struct Leftover {
    pub path: String,
    pub size: u64,
    /// FILETIME of the newest change inside, from the index.
    pub modified: i64,
    /// Korean: why it looks left behind ("설치된 프로그램 중 이 이름이 없어요" ...).
    pub reason: String,
    /// "caution" | "danger"
    pub risk: String,
}

/// Installed programs, biggest first. `with_prefetch` also reads Windows'
/// Prefetch folder through the admin session (if one is open) for last-run times.
#[tauri::command]
pub async fn programs_list(with_prefetch: bool, state: tauri::State<'_, crate::AppState>) -> Result<Vec<Program>, String> {
    let all = state.all();
    let prefetch = if with_prefetch { prefetch_runs().await } else { Vec::new() };
    crate::blocking(move || {
        let evidence = Evidence { launches: launches(), prefetch: prefetch_map(&prefetch), access_kept: last_access_kept() };
        let mut programs: Vec<Program> = installed(&Indexes::of(all), &evidence, STORE_FRESH).into_iter().map(|f| f.program).collect();
        programs.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        programs
    })
    .await
}

/// What `programs_list` lists (Uninstall keys and Store apps, a Store list
/// up to `store_kept` old), measured against the indexes.
fn installed(indexes: &Indexes, evidence: &Evidence, store_kept: Duration) -> Vec<Found> {
    let places = Places::now();
    let mut found = registry_programs(&places, false);
    found.extend(store_apps(store_kept).unwrap_or_default().into_iter().map(|a| store_found(a, &places)));
    for f in &mut found {
        measure(f, indexes, evidence);
    }
    found
}

/// An installed program as a search hit or a folder names it ("제거
/// (제어판처럼)…"): what the confirm says, and the id `program_uninstall` takes.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct ProgramRef {
    pub id: String,
    pub name: String,
    pub publisher: Option<String>,
    pub size: Option<u64>,
    /// It has an uninstaller the app can start (else the window says why not).
    pub can_uninstall: bool,
    /// Machine-wide (HKLM) install: its uninstaller will ask for administrator rights.
    pub machine_wide: bool,
    /// "win32" | "store" (removed for this user)
    pub kind: String,
}

impl ProgramRef {
    fn of(p: &Program) -> ProgramRef {
        ProgramRef {
            id: p.id.clone(),
            name: p.name.clone(),
            publisher: p.publisher.clone(),
            size: p.size,
            can_uninstall: p.can_uninstall,
            machine_wide: p.machine_wide,
            kind: p.kind.clone(),
        }
    }
}

/// An installed program with its folders, for AI 검색 (meaning.rs) and `program_at`.
#[derive(Clone, Debug)]
pub(crate) struct Placed {
    pub(crate) program: ProgramRef,
    /// Where it says it is installed, if it says so.
    pub(crate) location: Option<String>,
    /// Its own folder as written: none when it names none, or one many programs share.
    pub(crate) home: Option<String>,
    /// `home` is its InstallLocation, not the folder of its icon or uninstaller.
    pub(crate) stated: bool,
    /// That folder, when a scanned drive has it as a live folder now.
    pub(crate) folder: Option<String>,
}

/// Which of the programs that share a folder it is first: the one whose
/// InstallLocation it is (a launcher's folder is the launcher's, not a
/// game's whose uninstaller the launcher is), then one that can be
/// uninstalled (a suite, not its language packs), the biggest, by name.
/// AI 검색's hits (meaning.rs) and `program_at` name the same one.
pub(crate) fn owner_first(a: &Placed, b: &Placed) -> std::cmp::Ordering {
    b.stated
        .cmp(&a.stated)
        .then(b.program.can_uninstall.cmp(&a.program.can_uninstall))
        .then(b.program.size.cmp(&a.program.size))
        .then_with(|| a.program.name.cmp(&b.program.name))
}

/// An installed program as the Uninstall keys or the Store list have it,
/// before the indexes are asked where it is.
#[derive(Clone)]
struct Listed {
    /// Its size not measured yet.
    program: ProgramRef,
    location: Option<String>,
    home: Option<String>,
    stated: bool,
    /// Its own size estimate (EstimatedSize), bytes.
    estimated: Option<u64>,
}

impl Listed {
    fn of(f: Found) -> Listed {
        Listed {
            program: ProgramRef::of(&f.program),
            estimated: f.estimated.filter(|&kb| kb > 0).map(|kb| kb as u64 * 1024),
            location: f.program.install_location,
            home: f.home,
            stated: f.stated,
        }
    }
}

/// How long the Uninstall keys' programs are reused: a search and the
/// menus after it ask again and again, and they change slowly (an
/// uninstall forgets them at once). Where each one's folder is, is asked of
/// the indexes every time: a drive scanned meanwhile counts at once.
const PLACED_FRESH: Duration = Duration::from_secs(60);
/// The Uninstall keys' programs, and when they were read. Locked while they
/// are read: who asks meanwhile waits for that reading, not one of its own.
static REGISTRY: Mutex<Option<(Instant, Vec<Listed>)>> = Mutex::new(None);
/// The Store list that `STORE` made at that time, as Listed (its apps' manifests read once).
static STORE_LISTED: Mutex<Option<(Instant, Vec<Listed>)>> = Mutex::new(None);

/// The programs of the Uninstall keys, read again after PLACED_FRESH.
fn registry_listed(places: &Places) -> Vec<Listed> {
    let mut kept = REGISTRY.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((at, list)) = &*kept
        && at.elapsed() < PLACED_FRESH
    {
        return list.clone();
    }
    let list: Vec<Listed> = registry_programs(places, false).into_iter().map(Listed::of).collect();
    *kept = Some((Instant::now(), list.clone()));
    list
}

/// The apps of the Store list made at `at`.
fn store_listed(at: Instant, apps: Vec<Appx>, places: &Places) -> Vec<Listed> {
    let mut kept = STORE_LISTED.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((made, list)) = &*kept
        && *made == at
    {
        return list.clone();
    }
    let list: Vec<Listed> = apps.into_iter().map(|a| Listed::of(store_found(a, places))).collect();
    *kept = Some((at, list.clone()));
    list
}

/// The installed programs `programs_list` lists, each with its own folder,
/// whether a scanned drive has it now and its size there (else its own
/// estimate). Only the folders matter here: no launch records, Prefetch or
/// file times are read. The Store list of the last ten minutes will do,
/// made now when there is none (PowerShell takes seconds).
pub(crate) fn placed(all: Vec<Shared>) -> Vec<Placed> {
    placed_with(all, store_list(STORE_KEPT).ok())
}

fn placed_with(all: Vec<Shared>, store: Option<(Instant, Vec<Appx>)>) -> Vec<Placed> {
    let places = Places::now();
    let mut list = registry_listed(&places);
    if let Some((at, apps)) = store {
        list.extend(store_listed(at, apps, &places));
    }
    in_indexes(list, &Indexes::of(all))
}

/// Each program with its folder, if the indexes have it as they are now.
fn in_indexes(list: Vec<Listed>, indexes: &Indexes) -> Vec<Placed> {
    list.into_iter()
        .map(|l| {
            let size = l.home.as_deref().and_then(|h| indexed_size(indexes, h));
            Placed {
                program: ProgramRef { size: size.or(l.estimated), ..l.program },
                location: l.location,
                folder: size.and(l.home.clone()),
                home: l.home,
                stated: l.stated,
            }
        })
        .collect()
}

/// The size of a program's own folder, when an index has it as a live
/// folder (not a link), as `measure` measures it.
fn indexed_size(indexes: &Indexes, home: &str) -> Option<u64> {
    let ix = indexes.of_path(home)?.read().unwrap_or_else(PoisonError::into_inner);
    let n = ix.find(home).filter(|&n| is_plain_dir(ix.nodes[n as usize].flags))?;
    Some(ix.nodes[n as usize].size)
}

/// The installed program whose own folder `path` is, exactly (not a folder
/// in it, nor one above), for "제거 (제어판처럼)…" on a folder or a hit.
/// None for anything else, for a place Windows needs and for the user's
/// own folders. A menu waits for no PowerShell: the Store apps are those of
/// the last Store list, whatever its age (an old one is made anew meanwhile).
#[tauri::command]
pub async fn program_at(path: String, state: tauri::State<'_, crate::AppState>) -> Result<Option<ProgramRef>, String> {
    let Some(path) = crate::ai::normalize_abs(&path) else { return Ok(None) };
    let all = state.all();
    crate::blocking(move || program_in(&placed_with(all, store_list_now()), &path, &still_installed)).await
}

/// The program of `list` whose own folder is `path` (any case, a `\` at the
/// end or not): of several (`owner_first`), the first that is `still`
/// installed. Never a drive, a place Windows needs, nor one of the user's
/// own folders or one above it (Downloads, Documents, the profile): whatever
/// an Uninstall key says, those are no program's.
fn program_in(list: &[Placed], path: &str, still: &dyn Fn(&str) -> bool) -> Option<ProgramRef> {
    let key = path_key(path);
    if key.len() <= 3 || matches!(crate::ai::step_guard(path, true), Some("system" | "special")) {
        return None;
    }
    let mut owners: Vec<&Placed> = list.iter().filter(|p| p.home.as_deref().is_some_and(|h| path_key(h) == key)).collect();
    owners.sort_by(|a, b| owner_first(a, b));
    owners.into_iter().find(|p| still(&p.program.id)).map(|p| p.program.clone())
}

/// Whether a program of the list is still installed: a registry one has
/// its Uninstall key still (its uninstaller may have run since the list was
/// read: the list is read again then). A Store app's list is made anew
/// after each removal.
fn still_installed(id: &str) -> bool {
    let Some((tag, key_name)) = id.split_once('\\') else { return true };
    let Some(hive) = Hive::of_tag(tag) else { return false };
    let there = Key::open(hive.root(), &format!(r"{UNINSTALL_KEY}\{key_name}"), hive.access()).is_some();
    if !there {
        *REGISTRY.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
    there
}

/// Starts the program's own uninstaller (it shows its own windows and asks
/// for rights itself). Returns once it has started.
#[tauri::command]
pub async fn program_uninstall(id: String) -> Result<(), String> {
    let started = crate::blocking(move || uninstall(&id)).await?;
    // What is installed is about to change: the next search or menu reads it
    // again (and a menu checks the program it names is still there, `still_installed`).
    *REGISTRY.lock().unwrap_or_else(PoisonError::into_inner) = None;
    started
}

/// Folders in Program Files, ProgramData and AppData that no installed
/// program accounts for, biggest first.
#[tauri::command]
pub async fn program_leftovers(state: tauri::State<'_, crate::AppState>) -> Result<Vec<Leftover>, String> {
    let all = state.all();
    crate::blocking(move || find_leftovers(&Indexes::of(all))).await?
}

/// A draft cleanup plan (Recycle Bin steps, unselected) for chosen leftovers.
#[tauri::command]
pub async fn program_leftovers_plan(paths: Vec<String>, state: tauri::State<'_, crate::AppState>) -> Result<Plan, String> {
    let all = state.all();
    crate::blocking(move || leftovers_plan(&Indexes::of(all), &paths)).await?
}

// ---- Paths and names ----------------------------------------------------------------

/// A path for comparing: lower case, `\` only, no `\` at the end.
fn path_key(path: &str) -> String {
    path.replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

/// Whether `inner` (a `path_key`) is somewhere in the folder `outer` (a `path_key`).
fn below(inner: &str, outer: &str) -> bool {
    inner.len() > outer.len() + 1 && inner.starts_with(outer) && inner.as_bytes()[outer.len()] == b'\\'
}

fn name_of(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

fn parent_of(path: &str) -> Option<&str> {
    let i = path.rfind('\\')?;
    (i >= 2).then(|| &path[..i])
}

/// A folder path as the registry may give it (quoted, `/`, a `\` at the
/// end): a full path on a drive letter, below the drive's root. None else.
fn clean_location(raw: &str) -> Option<String> {
    let s = raw.trim().trim_matches('"').trim().replace('/', "\\");
    let s = s.trim_end_matches('\\');
    let b = s.as_bytes();
    if b.len() < 4 || !b[0].is_ascii_alphabetic() || b[1] != b':' || b[2] != b'\\' || s.contains(['"', '*', '?', '<', '>', '|']) {
        return None;
    }
    Some(format!("{}{}", (b[0] as char).to_ascii_uppercase(), &s[1..]))
}

fn is_absolute(path: &str) -> bool {
    let b = path.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\'
}

/// Lower case, letters and digits only (`Notepad++ (64-bit)` → `notepad64bit`).
fn squash(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// Lower-case words (`7-Zip 23.01 (x64)` → `7`, `zip`, `23`, `01`, `x64`).
fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect()
}

/// `{xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx}`
fn is_guid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 38
        && b[0] == b'{'
        && b[37] == b'}'
        && b[1..37].iter().enumerate().all(|(i, &c)| if matches!(i, 8 | 13 | 18 | 23) { c == b'-' } else { c.is_ascii_hexdigit() })
}

fn guid_of(s: &str) -> Option<GUID> {
    if !is_guid(s) {
        return None;
    }
    let hex: String = s[1..37].chars().filter(|&c| c != '-').collect();
    u128::from_str_radix(&hex, 16).ok().map(GUID::from_u128)
}

fn known_folder(id: &GUID) -> Option<String> {
    unsafe {
        let mut p: PWSTR = null_mut();
        let hr = SHGetKnownFolderPath(id, 0, null_mut(), &mut p);
        let path = (hr >= 0 && !p.is_null()).then(|| {
            let len = (0..).take_while(|&i| *p.add(i) != 0).count();
            String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
        });
        if !p.is_null() {
            CoTaskMemFree(p.cast());
        }
        path.map(|p| p.trim_end_matches('\\').to_string()).filter(|p| p.len() >= 2)
    }
}

/// `%NAME%` replaced by the environment variable, where there is one.
fn expand_env(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after.find('%').filter(|&j| j > 0).and_then(|j| std::env::var(&after[..j]).ok().map(|v| (j, v))) {
            Some((j, value)) => {
                out.push_str(&value);
                rest = &after[j + 1..];
            }
            None => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn filetime_of(t: SystemTime) -> Option<i64> {
    let d = t.duration_since(UNIX_EPOCH).ok()?;
    Some((d.as_nanos() / 100) as i64 + FILETIME_UNIX)
}

fn size_text(bytes: u64) -> String {
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else {
        format!("{:.0} MB", bytes as f64 / MB as f64)
    }
}

/// Where Windows keeps what many programs share.
struct Places {
    /// The Windows folder (`path_key`).
    windows: Option<String>,
    /// Its System32 folder, as written.
    system32: Option<String>,
    /// Folders whose direct children programs leave behind, and what they are.
    bases: Vec<(String, Area)>,
    /// Folders many programs share (`path_key`s): never one program's own.
    broad: HashSet<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Area {
    /// Where programs are installed (Program Files, AppData\Local\Programs).
    ProgramFiles,
    ProgramData,
    AppData,
}

impl Places {
    fn now() -> Places {
        let windows = known_folder(&FOLDERID_Windows);
        let mut bases: Vec<(String, Area)> = Vec::new();
        for (id, area) in [
            (&FOLDERID_ProgramFilesX64, Area::ProgramFiles),
            (&FOLDERID_ProgramFilesX86, Area::ProgramFiles),
            (&FOLDERID_ProgramData, Area::ProgramData),
            (&FOLDERID_LocalAppData, Area::AppData),
            (&FOLDERID_RoamingAppData, Area::AppData),
            (&FOLDERID_UserProgramFiles, Area::ProgramFiles),
        ] {
            if let Some(p) = known_folder(id).and_then(|p| clean_location(&p))
                && !bases.iter().any(|(b, _)| path_key(b) == path_key(&p))
            {
                bases.push((p, area));
            }
        }
        let mut broad: HashSet<String> = bases.iter().map(|(b, _)| path_key(b)).collect();
        let profile = known_folder(&FOLDERID_Profile);
        let shared = [known_folder(&FOLDERID_ProgramFilesCommonX64), known_folder(&FOLDERID_ProgramFilesCommonX86)];
        broad.extend(shared.into_iter().flatten().map(|p| path_key(&p)));
        if let Some(w) = &windows {
            for sub in ["", r"\System32", r"\SysWOW64", r"\Installer", r"\Temp"] {
                broad.insert(path_key(&format!("{w}{sub}")));
            }
        }
        if let Some(p) = &profile {
            broad.insert(path_key(p));
            broad.extend(parent_of(p).map(path_key));
        }
        // The user's own folders (Desktop, Documents, Downloads, ..., OneDrive, every
        // account's profile): an installer's icon left in Downloads makes it no program's.
        broad.extend(crate::ai::special_folders().iter().map(|p| path_key(p)));
        for (b, area) in &bases {
            let subs: &[&str] = match area {
                Area::ProgramFiles => &[r"\WindowsApps"],
                Area::ProgramData => &[r"\Microsoft", r"\Package Cache", r"\Packages"],
                Area::AppData => &[r"\Microsoft", r"\Packages", r"\Temp", r"\Programs"],
            };
            broad.extend(subs.iter().map(|s| path_key(&format!("{b}{s}"))));
        }
        let system32 = windows.as_ref().map(|w| format!(r"{w}\System32"));
        Places { windows: windows.map(|w| path_key(&w)), system32, bases, broad }
    }

    /// Whether `key` (a `path_key`) is a folder many programs share, or one above such a folder.
    fn is_broad(&self, key: &str) -> bool {
        key.len() <= 3 || self.broad.contains(key) || self.broad.iter().any(|b| below(b, key))
    }

    fn in_windows(&self, key: &str) -> bool {
        self.windows.as_deref().is_some_and(|w| below(key, w))
    }
}

// ---- Registry -----------------------------------------------------------------------

/// An open registry key, closed when dropped.
struct Key(HKEY);

impl Key {
    fn open(root: HKEY, path: &str, access: REG_SAM_FLAGS) -> Option<Key> {
        let path = win::wide(path);
        let mut h: HKEY = null_mut();
        (unsafe { RegOpenKeyExW(root, path.as_ptr(), 0, access, &mut h) } == ERROR_SUCCESS).then_some(Key(h))
    }

    fn subkeys(&self) -> Vec<String> {
        let mut out = Vec::new();
        // Key names have at most 255 characters.
        let mut buf = vec![0u16; 512];
        for i in 0..1_000_000u32 {
            let mut len = buf.len() as u32;
            match unsafe { RegEnumKeyExW(self.0, i, buf.as_mut_ptr(), &mut len, null(), null_mut(), null_mut(), null_mut()) } {
                ERROR_SUCCESS => out.push(String::from_utf16_lossy(&buf[..len as usize])),
                ERROR_MORE_DATA => continue,
                _ => break,
            }
        }
        out
    }

    /// A value's type and data.
    fn value(&self, name: &str) -> Option<(u32, Vec<u8>)> {
        let name = win::wide(name);
        // A value can grow between the two calls: then it is asked again.
        for _ in 0..3 {
            let (mut ty, mut size) = (0u32, 0u32);
            if unsafe { RegQueryValueExW(self.0, name.as_ptr(), null(), &mut ty, null_mut(), &mut size) } != ERROR_SUCCESS {
                return None;
            }
            let mut data = vec![0u8; size as usize];
            match unsafe { RegQueryValueExW(self.0, name.as_ptr(), null(), &mut ty, data.as_mut_ptr(), &mut size) } {
                ERROR_SUCCESS => {
                    data.truncate(size as usize);
                    return Some((ty, data));
                }
                ERROR_MORE_DATA => continue,
                _ => return None,
            }
        }
        None
    }

    /// A string value (REG_EXPAND_SZ expanded), trimmed; None when empty.
    fn text(&self, name: &str) -> Option<String> {
        let (ty, data) = self.value(name)?;
        let s = match ty {
            REG_SZ => utf16_text(&data),
            REG_EXPAND_SZ => expand_env(&utf16_text(&data)),
            _ => return None,
        };
        let s = s.trim();
        (!s.is_empty()).then(|| s.to_string())
    }

    fn dword(&self, name: &str) -> Option<u32> {
        match self.value(name)? {
            (REG_DWORD, data) if data.len() >= 4 => Some(u32::from_le_bytes([data[0], data[1], data[2], data[3]])),
            _ => None,
        }
    }

    /// Every value's name and data.
    fn values(&self) -> Vec<(String, Vec<u8>)> {
        let (mut name_max, mut data_max) = (0u32, 0u32);
        let info = unsafe {
            RegQueryInfoKeyW(
                self.0,
                null_mut(),
                null_mut(),
                null(),
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
                &mut name_max,
                &mut data_max,
                null_mut(),
                null_mut(),
            )
        };
        if info != ERROR_SUCCESS {
            return Vec::new();
        }
        let mut name = vec![0u16; name_max as usize + 1];
        let mut data = vec![0u8; data_max as usize];
        let mut out = Vec::new();
        for i in 0..1_000_000u32 {
            let (mut name_len, mut data_len, mut ty) = (name.len() as u32, data.len() as u32, 0u32);
            let err = unsafe {
                RegEnumValueW(self.0, i, name.as_mut_ptr(), &mut name_len, null(), &mut ty, data.as_mut_ptr(), &mut data_len)
            };
            match err {
                ERROR_SUCCESS => {
                    out.push((String::from_utf16_lossy(&name[..name_len as usize]), data[..data_len as usize].to_vec()))
                }
                // Grew since it was measured: skipped.
                ERROR_MORE_DATA => continue,
                _ => break,
            }
        }
        out
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}

/// UTF-16 (little-endian) bytes up to the first NUL.
fn utf16_text(data: &[u8]) -> String {
    let units: Vec<u16> = data.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
    String::from_utf16_lossy(&units)
}

/// Where an Uninstall key is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hive {
    Machine64,
    Machine32,
    User,
}

impl Hive {
    const ALL: [Hive; 3] = [Hive::Machine64, Hive::Machine32, Hive::User];

    fn tag(self) -> &'static str {
        match self {
            Hive::Machine64 => "HKLM64",
            Hive::Machine32 => "HKLM32",
            Hive::User => "HKCU",
        }
    }

    fn of_tag(tag: &str) -> Option<Hive> {
        Hive::ALL.into_iter().find(|h| h.tag() == tag)
    }

    fn root(self) -> HKEY {
        if self == Hive::User { HKEY_CURRENT_USER } else { HKEY_LOCAL_MACHINE }
    }

    fn access(self) -> REG_SAM_FLAGS {
        KEY_READ
            | match self {
                Hive::Machine64 => KEY_WOW64_64KEY,
                Hive::Machine32 => KEY_WOW64_32KEY,
                // HKCU\Software is one for 32-bit and 64-bit programs.
                Hive::User => 0,
            }
    }
}

// ---- What is installed --------------------------------------------------------------

/// How a program is removed.
#[derive(Clone, Debug, PartialEq)]
enum Uninstall {
    /// A program and its arguments, as the registry has them.
    Run { exe: String, args: String },
    /// A Windows Installer product, by its product code.
    Msi(String),
    /// A Store package, by its full name.
    Store(String),
}

/// One installed program as found, with what the app needs besides what it shows.
struct Found {
    program: Program,
    /// Its own folder, as written: none when it names none, or one many programs share.
    home: Option<String>,
    /// `home` is its InstallLocation (not the folder of its icon or uninstaller).
    stated: bool,
    /// Executables the registry names (its icon, its uninstaller): full paths.
    exes: Vec<String>,
    /// What it is called: its name, its key's name, its publisher.
    names: Vec<String>,
    /// Store apps: the package family name (lower case), which Explorer's launch records use.
    family: Option<String>,
    /// Its own size estimate, KB.
    estimated: Option<u32>,
    uninstall: Option<Uninstall>,
}

/// Every program in the Uninstall keys, but the ones `skipped` leaves out
/// (with `every`, those too, and keys without a name: they tell whose a
/// folder is); the same program in two places once.
fn registry_programs(places: &Places, every: bool) -> Vec<Found> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for hive in Hive::ALL {
        let Some(list) = Key::open(hive.root(), UNINSTALL_KEY, hive.access()) else { continue };
        for name in list.subkeys() {
            let Some(key) = Key::open(list.0, &name, hive.access()) else { continue };
            let Some(found) = entry(hive, &name, &key, places, every) else { continue };
            let same = (found.program.name.to_lowercase(), found.program.version.clone(), found.home.as_deref().map(path_key));
            if seen.insert(same) {
                out.push(found);
            }
        }
    }
    out
}

/// One program's Uninstall key, looked up again by its id's parts.
fn read_entry(hive: Hive, key_name: &str, places: &Places) -> Option<Found> {
    let key = Key::open(hive.root(), &format!(r"{UNINSTALL_KEY}\{key_name}"), hive.access())?;
    entry(hive, key_name, &key, places, false)
}

/// System components, updates, hotfixes and service packs: not programs to list.
fn skipped(system_component: Option<u32>, parent_key: Option<&str>, release_type: Option<&str>) -> bool {
    system_component == Some(1)
        || parent_key.is_some()
        || release_type.is_some_and(|r| {
            let r = r.to_lowercase();
            ["update", "hotfix", "service pack", "security"].iter().any(|w| r.contains(w))
        })
}

fn entry(hive: Hive, key_name: &str, key: &Key, places: &Places, every: bool) -> Option<Found> {
    let name = match key.text("DisplayName") {
        Some(name) => name,
        None if every => key_name.to_string(),
        None => return None,
    };
    if !every && skipped(key.dword("SystemComponent"), key.text("ParentKeyName").as_deref(), key.text("ReleaseType").as_deref()) {
        return None;
    }
    let publisher = key.text("Publisher");
    let location = key.text("InstallLocation").and_then(|l| clean_location(&l));
    let windows_installer = key.dword("WindowsInstaller") == Some(1);
    let uninstall = uninstall_command(key_name, windows_installer, key.text("UninstallString").as_deref());
    let mut exes: Vec<String> = key.text("DisplayIcon").and_then(|i| icon_exe(&i)).into_iter().collect();
    if let Some(Uninstall::Run { exe, .. }) = &uninstall
        && is_absolute(exe)
    {
        exes.push(exe.clone());
    }
    exes.retain(|e| !places.in_windows(&path_key(e)));
    exes.dedup_by(|a, b| path_key(a) == path_key(b));
    let stated = location.clone().filter(|l| !places.is_broad(&path_key(l)));
    let home = stated.clone().or_else(|| infer_home(&exes, places));
    let mut names = vec![name.clone()];
    let bare = key_name.strip_suffix("_is1").unwrap_or(key_name);
    if !is_guid(bare) && !bare.is_empty() {
        names.push(bare.to_string());
    }
    names.extend(publisher.clone());
    let can_uninstall = uninstall.as_ref().is_some_and(|u| startable(u, places)) && key.dword("NoRemove") != Some(1);
    let program = Program {
        id: format!("{}\\{key_name}", hive.tag()),
        name,
        publisher,
        version: key.text("DisplayVersion"),
        installed_at: key.text("InstallDate").and_then(|d| install_date(&d)),
        install_location: location.or_else(|| home.clone()),
        size: None,
        size_source: "unknown".into(),
        last_used: None,
        last_used_source: None,
        run_count: None,
        kind: "win32".into(),
        can_uninstall,
        machine_wide: hive != Hive::User,
    };
    Some(Found { program, stated: stated.is_some(), home, exes, names, family: None, estimated: key.dword("EstimatedSize"), uninstall })
}

/// The executable a DisplayIcon names (`"C:\x\app.exe",0` → `C:\x\app.exe`).
fn icon_exe(icon: &str) -> Option<String> {
    let icon = icon.trim();
    let icon = match icon.rsplit_once(',') {
        Some((path, index)) if index.trim().parse::<i32>().is_ok() => path,
        _ => icon,
    };
    let path = icon.trim().trim_matches('"').trim();
    (is_absolute(path) && path.to_lowercase().ends_with(".exe")).then(|| path.to_string())
}

/// Folders an executable is in that are not the program's own folder yet.
const EXE_SUBFOLDERS: &[&str] = &["bin", "uninstall", "uninst", "_uninst", "_uninstall", "uninstaller"];

/// For a program that names no folder: the folder of its icon or its
/// uninstaller, when that is a folder of its own (not one many programs
/// share, nor an installer cache).
fn infer_home(exes: &[String], places: &Places) -> Option<String> {
    exes.iter().find_map(|exe| {
        let mut dir = parent_of(exe)?;
        if EXE_SUBFOLDERS.contains(&name_of(dir).to_lowercase().as_str()) {
            dir = parent_of(dir)?;
        }
        let key = path_key(dir);
        let cache = key.split('\\').any(|p| {
            matches!(p, "package cache" | "installer" | "installshield installation information" | "common files") || p.starts_with('{')
        });
        (!cache && !places.is_broad(&key) && !places.in_windows(&key)).then(|| clean_location(dir)).flatten()
    })
}

/// How to remove a program, from its key: a Windows Installer product by
/// its product code (the key's name, or the code `msiexec /I` or `/X` names
/// in its UninstallString; anything else there, such as `/qn`, is dropped),
/// else its UninstallString.
fn uninstall_command(key_name: &str, windows_installer: bool, uninstall: Option<&str>) -> Option<Uninstall> {
    if windows_installer && is_guid(key_name) {
        return Some(Uninstall::Msi(key_name.to_uppercase()));
    }
    let (exe, args) = split_command(uninstall?)?;
    if is_msiexec(&exe)
        && let Some(code) = msi_code(&args)
    {
        return Some(Uninstall::Msi(code));
    }
    Some(Uninstall::Run { exe, args })
}

fn is_msiexec(exe: &str) -> bool {
    matches!(name_of(exe).to_lowercase().as_str(), "msiexec" | "msiexec.exe")
}

/// The product code of `msiexec` arguments that install (/I) or remove (/X) one.
fn msi_code(args: &str) -> Option<String> {
    let lower = args.to_lowercase();
    if !lower.contains("/x") && !lower.contains("/i") && !lower.contains("-x") && !lower.contains("-i") {
        return None;
    }
    let start = args.find('{')?;
    let code = args.get(start..start + 38)?;
    is_guid(code).then(|| code.to_uppercase())
}

/// A command line split into the program and its arguments: a quoted
/// program, else the shortest start that ends in `.exe` before a space or
/// the end (an unquoted path with spaces), else the first word.
fn split_command(cmd: &str) -> Option<(String, String)> {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return None;
    }
    if let Some(rest) = cmd.strip_prefix('"') {
        let end = rest.find('"')?;
        let exe = rest[..end].trim();
        return (!exe.is_empty()).then(|| (exe.to_string(), rest[end + 1..].trim().to_string()));
    }
    // ASCII lower case keeps every byte where it was.
    let lower = cmd.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(".exe") {
        let end = from + i + 4;
        if cmd[end..].chars().next().is_none_or(char::is_whitespace) {
            return Some((cmd[..end].to_string(), cmd[end..].trim().to_string()));
        }
        from = end;
    }
    match cmd.split_once(char::is_whitespace) {
        Some((exe, args)) => Some((exe.to_string(), args.trim().to_string())),
        None => Some((cmd.to_string(), String::new())),
    }
}

/// An InstallDate (`YYYYMMDD`, also with `-`, `/` or `.` between) as a
/// FILETIME at noon UTC, which is that day in every time zone from UTC−12
/// to UTC+11.
fn install_date(text: &str) -> Option<i64> {
    let digits: String = text.trim().chars().filter(|c| !matches!(c, '-' | '/' | '.')).collect();
    if digits.len() != 8 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (y, m, d): (i64, i64, i64) = (digits[..4].parse().ok()?, digits[4..6].parse().ok()?, digits[6..].parse().ok()?);
    if !(1980..=2100).contains(&y) || !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
        return None;
    }
    Some((days_from_civil(y, m, d) * 86_400 + 43_200) * 10_000_000 + FILETIME_UNIX)
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 (proleptic Gregorian).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// ---- Store apps ---------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
struct Appx {
    name: String,
    full_name: String,
    location: Option<String>,
    version: Option<String>,
    /// The signer's distinguished name (`CN=..., O=..., C=...`).
    publisher: Option<String>,
    non_removable: bool,
}

/// The last list, and when it was made.
static STORE: Mutex<Option<(Instant, Vec<Appx>)>> = Mutex::new(None);
/// Held while a list is made: who asks meanwhile waits for that list (one
/// PowerShell at a time, not one for each).
static STORE_LISTING: Mutex<()> = Mutex::new(());
/// A list is being made for `store_list_now`.
static STORE_REFRESHING: AtomicBool = AtomicBool::new(false);

/// The Store apps of this user: the last list if it is younger than `kept`
/// (never with `Duration::ZERO`), else a new one.
fn store_apps(kept: Duration) -> Result<Vec<Appx>, String> {
    store_list(kept).map(|(_, apps)| apps)
}

/// `store_apps`, with when the list was made.
fn store_list(kept: Duration) -> Result<(Instant, Vec<Appx>), String> {
    let young = || STORE.lock().unwrap_or_else(PoisonError::into_inner).as_ref().filter(|(at, _)| at.elapsed() < kept).cloned();
    if let Some(list) = young() {
        return Ok(list);
    }
    let _one = STORE_LISTING.lock().unwrap_or_else(PoisonError::into_inner);
    // Made while this one waited.
    if let Some(list) = young() {
        return Ok(list);
    }
    let apps = list_store()?;
    let at = Instant::now();
    *STORE.lock().unwrap_or_else(PoisonError::into_inner) = Some((at, apps.clone()));
    Ok((at, apps))
}

/// The last Store list whatever its age, waiting for no PowerShell (for a
/// menu): one older than STORE_KEPT, or none yet, is made anew in the
/// background, for the next time.
fn store_list_now() -> Option<(Instant, Vec<Appx>)> {
    let last = STORE.lock().unwrap_or_else(PoisonError::into_inner).clone();
    if last.as_ref().is_none_or(|(at, _)| at.elapsed() >= STORE_KEPT) && !STORE_REFRESHING.swap(true, Ordering::SeqCst) {
        std::thread::spawn(|| {
            let _ = store_list(STORE_KEPT);
            STORE_REFRESHING.store(false, Ordering::SeqCst);
        });
    }
    last
}

fn list_store() -> Result<Vec<Appx>, String> {
    let ps = powershell().ok_or(NO_POWERSHELL)?;
    let ran = run_hidden(&ps, &["-NoProfile", "-NonInteractive", "-Command", STORE_LIST], STORE_LIMIT, true)?;
    if !ran.done {
        return Err(STORE_SLOW.into());
    }
    if ran.code != Some(0) {
        return Err(format!("{STORE_FAILED} (종료 코드 {})", ran.code.map_or("-".into(), |c| c.to_string())));
    }
    Ok(parse_appx(&ran.out))
}

/// Windows' own PowerShell, never one found on PATH.
fn powershell() -> Option<PathBuf> {
    let windows = known_folder(&FOLDERID_Windows)?;
    let ps = PathBuf::from(windows).join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    ps.is_file().then_some(ps)
}

/// A package full name: `Name_Version_Arch_ResourceId_PublisherId`, of
/// letters, digits, `.`, `-`, `_` and `~` only (so it can go into a command
/// between single quotes).
fn is_package_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && s.contains('_')
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'~'))
}

/// `ConvertTo-Json` of one package (an object) or many (an array).
fn parse_appx(json: &str) -> Vec<Appx> {
    let json = json.trim_start_matches('\u{feff}').trim();
    let Ok(value) = serde_json::from_str::<Value>(json) else { return Vec::new() };
    let items = match value {
        Value::Array(items) => items,
        one @ Value::Object(_) => vec![one],
        _ => Vec::new(),
    };
    items
        .iter()
        .filter_map(|o| {
            let full_name = text_field(o, "PackageFullName").filter(|n| is_package_name(n))?;
            Some(Appx {
                name: text_field(o, "Name").unwrap_or_else(|| full_name.split('_').next().unwrap_or_default().to_string()),
                location: text_field(o, "InstallLocation"),
                version: text_field(o, "Version"),
                publisher: text_field(o, "Publisher"),
                non_removable: o.get("NonRemovable").and_then(Value::as_bool).unwrap_or(false),
                full_name,
            })
        })
        .collect()
}

/// A field as text: a string, a number, or a version object ({Major, Minor, ...}).
fn text_field(o: &Value, name: &str) -> Option<String> {
    let text = match o.get(name)? {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        Value::Object(v) if v.contains_key("Major") => ["Major", "Minor", "Build", "Revision"]
            .iter()
            .filter_map(|k| v.get(*k).and_then(Value::as_i64).filter(|&n| n >= 0))
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("."),
        _ => return None,
    };
    (!text.is_empty()).then_some(text)
}

/// The organization (else the common name) of a signer's distinguished
/// name; None for a Store-assigned id.
fn publisher_name(dn: &str) -> Option<String> {
    let mut fields: Vec<(String, String)> = Vec::new();
    let (mut part, mut quoted) = (String::new(), false);
    for c in dn.chars().chain(std::iter::once(',')) {
        match c {
            '"' => quoted = !quoted,
            ',' if !quoted => {
                if let Some((k, v)) = part.split_once('=') {
                    fields.push((k.trim().to_ascii_uppercase(), v.trim().to_string()));
                }
                part.clear();
            }
            c => part.push(c),
        }
    }
    let pick = |k: &str| fields.iter().find(|(f, v)| f == k && !v.is_empty()).map(|(_, v)| v.clone());
    pick("O").or_else(|| pick("CN")).filter(|v| !is_guid(&format!("{{{v}}}")))
}

/// `Name_PublisherId` of a package (from its full name).
fn family_of(name: &str, full_name: &str) -> Option<String> {
    let publisher_id = full_name.rsplit('_').next().filter(|p| !p.is_empty() && *p != full_name)?;
    Some(format!("{name}_{publisher_id}").to_lowercase())
}

/// The display name a package's manifest gives, unless it is a resource reference.
fn manifest_name(xml: &str) -> Option<String> {
    let props = &xml[xml.find("<Properties>")?..];
    let start = props.find("<DisplayName>")? + "<DisplayName>".len();
    let end = props[start..].find("</DisplayName>")?;
    let name = unescape_xml(props[start..start + end].trim());
    (!name.is_empty() && !name.to_lowercase().starts_with("ms-resource:")).then_some(name)
}

fn unescape_xml(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

fn store_found(app: Appx, places: &Places) -> Found {
    let location = app.location.as_deref().and_then(clean_location);
    let display = location
        .as_deref()
        .and_then(|l| {
            use std::io::Read;
            let mut xml = String::new();
            let file = std::fs::File::open(Path::new(l).join("AppxManifest.xml")).ok()?;
            file.take(1 << 20).read_to_string(&mut xml).ok()?;
            manifest_name(&xml)
        })
        .unwrap_or_else(|| app.name.clone());
    let home = location.clone().filter(|l| !places.is_broad(&path_key(l)));
    let program = Program {
        id: app.full_name.clone(),
        name: display.clone(),
        publisher: app.publisher.as_deref().and_then(publisher_name),
        version: app.version.clone(),
        installed_at: None,
        install_location: location,
        size: None,
        size_source: "unknown".into(),
        last_used: None,
        last_used_source: None,
        run_count: None,
        kind: "store".into(),
        can_uninstall: !app.non_removable,
        machine_wide: false,
    };
    Found {
        program,
        stated: home.is_some(),
        home,
        exes: Vec::new(),
        names: vec![display, app.name.clone()],
        family: family_of(&app.name, &app.full_name),
        estimated: None,
        uninstall: Some(Uninstall::Store(app.full_name)),
    }
}

/// What a hidden run printed and how it ended.
struct Ran {
    code: Option<i32>,
    out: String,
    err: String,
    /// It ended within the time it had.
    done: bool,
}

/// Runs `exe` with no window. After `limit` it is ended when `kill_late`,
/// else left to go on (and `done` is false).
fn run_hidden(exe: &Path, args: &[&str], limit: Duration, kill_late: bool) -> Result<Ran, String> {
    use std::io::Read;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    let mut child = Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("{NO_POWERSHELL}: {e}"))?;
    let read_all = |r: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut b = Vec::new();
            if let Some(r) = r {
                let _ = r.take(16 << 20).read_to_end(&mut b);
            }
            String::from_utf8_lossy(&b).into_owned()
        })
    };
    let out = read_all(child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>));
    let err = read_all(child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>));
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let (out, err) = (out.join().unwrap_or_default(), err.join().unwrap_or_default());
                return Ok(Ran { code: status.code(), out, err, done: true });
            }
            Ok(None) if start.elapsed() > limit => {
                if kill_late {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                return Ok(Ran { code: None, out: String::new(), err: String::new(), done: false });
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(format!("{NO_POWERSHELL}: {e}")),
        }
    }
}

// ---- Last use -----------------------------------------------------------------------

/// One of Explorer's launch records.
#[derive(Clone, Debug, PartialEq)]
struct Launch {
    /// What was started, in lower case: a path (known folders spelled out),
    /// a shortcut's path, or an app id (`family!app`).
    target: String,
    count: u32,
    /// FILETIME
    last: i64,
}

/// What tells when programs were last used.
struct Evidence {
    launches: Vec<Launch>,
    /// Prefetch: executable name (lower case, as Prefetch keeps it) → last run.
    prefetch: HashMap<String, i64>,
    /// NTFS keeps when files were last read.
    access_kept: bool,
}

fn rot13(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' => (((c as u8 - b'a' + 13) % 26) + b'a') as char,
            'A'..='Z' => (((c as u8 - b'A' + 13) % 26) + b'A') as char,
            c => c,
        })
        .collect()
}

/// A UserAssist value: its name ROT13 of what was started, a known folder's
/// id in front of a path standing for that folder; its data (Windows 7 and
/// later, 72 bytes) the run count at 4 and the last start (FILETIME) at 60.
fn launch_of(name: &str, data: &[u8], resolve: &mut dyn FnMut(&str) -> Option<String>) -> Option<Launch> {
    if data.len() < 68 {
        return None;
    }
    let count = u32::from_le_bytes(data[4..8].try_into().ok()?);
    let last = i64::from_le_bytes(data[60..68].try_into().ok()?);
    let decoded = rot13(name);
    // Counters (UEME_CTLSESSION ...) are no launch.
    if (count == 0 && last <= 0) || !(decoded.contains('\\') || decoded.contains('!')) {
        return None;
    }
    let target = match decoded.get(..38).filter(|g| is_guid(g)).and_then(&mut *resolve) {
        Some(base) => format!("{}{}", base.trim_end_matches('\\'), &decoded[38..]),
        None => decoded,
    };
    Some(Launch { target: target.to_lowercase(), count, last: last.max(0) })
}

/// Explorer's launch records of this user (UserAssist).
fn launches() -> Vec<Launch> {
    let Some(root) = Key::open(HKEY_CURRENT_USER, USERASSIST_KEY, KEY_READ) else { return Vec::new() };
    let mut folders: HashMap<String, Option<String>> = HashMap::new();
    let mut resolve = |g: &str| folders.entry(g.to_uppercase()).or_insert_with(|| guid_of(g).and_then(|id| known_folder(&id))).clone();
    let mut out = Vec::new();
    for sub in root.subkeys() {
        let Some(count) = Key::open(root.0, &format!(r"{sub}\Count"), KEY_READ) else { continue };
        out.extend(count.values().iter().filter_map(|(name, data)| launch_of(name, data, &mut resolve)));
    }
    out
}

/// Executables whose starts say little about the use of the program they
/// came with: uninstallers, installers, updaters, crash reporters, helpers.
fn is_generic_exe(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("unins")
        || ["setup", "install", "update", "crash", "elevat", "vc_redist", "maintenanceservice", "createdump", "squirrel"]
            .iter()
            .any(|p| n.starts_with(p))
        || matches!(n.as_str(), "helper.exe" | "launcher.exe" | "notification_helper.exe" | "dxsetup.exe")
}

/// Whether a shortcut's name names this program: its words are the first
/// word of the program's name (one word, of three letters or more), or a
/// run of words in it.
fn shortcut_matches(stem: &str, display: &str) -> bool {
    let (s, d) = (words(stem), words(display));
    match s.len() {
        0 => false,
        1 => s[0].chars().count() >= 3 && d.first() == Some(&s[0]),
        n => d.windows(n).any(|w| w == s.as_slice()),
    }
}

/// Folders shortcuts are kept in, whose names say nothing about a program.
const SHORTCUT_FOLDERS: &[&str] = &["programs", "taskbar", "start menu", "desktop", "quick launch", "implicitappshortcuts", "user pinned"];

/// Whether a launch record is of this program: an executable in its folder
/// (or one its key names), a shortcut named for it (or in a Start menu
/// folder named for it), or its Store app id.
fn launch_is_of(l: &Launch, f: &Found, home: Option<&str>, exes: &[String]) -> bool {
    let t = l.target.as_str();
    if t.ends_with(".exe") {
        return !is_generic_exe(name_of(t)) && (home.is_some_and(|h| below(t, h)) || exes.iter().any(|e| e == t));
    }
    if let Some(path) = t.strip_suffix(".lnk") {
        let stem = name_of(path);
        if stem.contains("uninstall") || stem.contains("제거") {
            return false;
        }
        let folder = parent_of(path).map(name_of).filter(|n| !SHORTCUT_FOLDERS.contains(n));
        return shortcut_matches(stem, &f.program.name) || folder.is_some_and(|n| shortcut_matches(n, &f.program.name));
    }
    f.family.as_deref().is_some_and(|fam| t.strip_prefix(fam).is_some_and(|rest| rest.starts_with('!')))
}

/// Prefetch file names (`NOTEPAD++.EXE-1A2B3C4D.pf`): the executable's name, lower case.
fn prefetch_exe(file: &str) -> Option<String> {
    let lower = file.to_lowercase();
    let stem = lower.strip_suffix(".pf")?;
    let (exe, hash) = stem.rsplit_once('-')?;
    (hash.len() == 8 && hash.bytes().all(|b| b.is_ascii_hexdigit()) && !exe.is_empty()).then(|| exe.to_string())
}

fn prefetch_map(runs: &[(String, i64)]) -> HashMap<String, i64> {
    let mut map: HashMap<String, i64> = HashMap::new();
    for (exe, at) in runs {
        let t = map.entry(exe.clone()).or_insert(*at);
        *t = (*t).max(*at);
    }
    map
}

/// The Prefetch files' names and times, when Windows lets the app see them:
/// this app runs elevated, or an admin session is open (it is never started
/// here, so no UAC prompt comes of it).
async fn prefetch_runs() -> Vec<(String, i64)> {
    let Some(windows) = known_folder(&FOLDERID_Windows) else { return Vec::new() };
    let dir = format!(r"{windows}\Prefetch");
    let files: Vec<(String, i64)> = if win::is_elevated() {
        crate::blocking(move || {
            let Ok(read) = std::fs::read_dir(&dir) else { return Vec::new() };
            read.flatten()
                .filter_map(|e| {
                    let t = e.metadata().ok()?.modified().ok().and_then(filetime_of)?;
                    Some((e.file_name().to_string_lossy().into_owned(), t))
                })
                .collect()
        })
        .await
        .unwrap_or_default()
    } else {
        match crate::admin::admin_session_status().await {
            Ok(s) if s.active => crate::admin::admin_list(dir)
                .await
                .map(|entries| entries.into_iter().filter(|e| !e.is_dir).map(|e| (e.name, e.modified)).collect())
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    };
    files.into_iter().filter_map(|(name, t)| prefetch_exe(&name).map(|exe| (exe, t))).collect()
}

/// Whether NTFS keeps last-access times, by NtfsDisableLastAccessUpdate:
/// its lowest bit switches the updates off (1, 0x80000001 user-managed,
/// 0x80000003 system-managed); 0, 0x80000000 and 0x80000002 leave them on.
/// No value: unknown, so not trusted.
fn access_updates_on(setting: Option<u32>) -> bool {
    setting.is_some_and(|v| v & 1 == 0)
}

fn last_access_kept() -> bool {
    let key = Key::open(HKEY_LOCAL_MACHINE, FILESYSTEM_KEY, KEY_READ | KEY_WOW64_64KEY);
    access_updates_on(key.and_then(|k| k.dword("NtfsDisableLastAccessUpdate")))
}

/// A folder link (junction, symbolic link), which the index does not go into.
fn is_link_dir(flags: u32) -> bool {
    flags & (FLAG_DIR | FLAG_REPARSE | FLAG_NOT_LINK) == FLAG_DIR | FLAG_REPARSE
}

fn is_plain_dir(flags: u32) -> bool {
    flags & (FLAG_DIR | FLAG_DELETED) == FLAG_DIR && !is_link_dir(flags)
}

/// The executables (not `is_generic_exe`) in the indexed folder `top` and
/// a few levels below it, nearest first.
fn exes_below(ix: &Index, top: u32, top_path: &str, out: &mut Vec<String>) {
    let mut queue = std::collections::VecDeque::from([(top, top_path.trim_end_matches('\\').to_string(), 0usize)]);
    let mut seen = 0usize;
    while let Some((dir, base, depth)) = queue.pop_front() {
        for c in ix.children_of(dir) {
            seen += 1;
            if seen > WALK_MOST / 10 || out.len() >= EXES_MOST {
                return;
            }
            let flags = ix.nodes[c as usize].flags;
            if flags & FLAG_DELETED != 0 {
                continue;
            }
            let name = ix.name(c);
            if flags & FLAG_DIR != 0 {
                if depth + 1 < EXE_DEPTH && !is_link_dir(flags) {
                    queue.push_back((c, format!("{base}\\{name}"), depth + 1));
                }
            } else if name.to_lowercase().ends_with(".exe") && !is_generic_exe(name) {
                out.push(format!("{base}\\{name}"));
            }
        }
    }
}

/// The indexes of the scanned drives, by drive (`C:`).
struct Indexes(HashMap<String, Shared>);

impl Indexes {
    fn of(all: Vec<Shared>) -> Indexes {
        Indexes(
            all.into_iter()
                .map(|s| {
                    let drive = s.read().unwrap_or_else(PoisonError::into_inner).drive.clone();
                    (drive, s)
                })
                .collect(),
        )
    }

    fn of_path(&self, path: &str) -> Option<&Shared> {
        self.0.get(&crate::drive_of(path)?)
    }
}

/// Fills in a program's size (from the index, else its own estimate) and
/// its last use, with what tells it.
fn measure(f: &mut Found, indexes: &Indexes, ev: &Evidence) {
    let mut exes = f.exes.clone();
    if let Some(home) = &f.home
        && let Some(shared) = indexes.of_path(home)
    {
        let ix = shared.read().unwrap_or_else(PoisonError::into_inner);
        if let Some(n) = ix.find(home).filter(|&n| is_plain_dir(ix.nodes[n as usize].flags)) {
            f.program.size = Some(ix.nodes[n as usize].size);
            f.program.size_source = "index".into();
            exes_below(&ix, n, home, &mut exes);
        }
    }
    if f.program.size.is_none()
        && let Some(kb) = f.estimated.filter(|&kb| kb > 0)
    {
        f.program.size = Some(kb as u64 * 1024);
        f.program.size_source = "registry".into();
    }
    let home = f.home.as_deref().map(path_key);
    let exe_keys: Vec<String> = exes.iter().map(|e| path_key(e)).collect();
    let mut best: Option<(i64, &str)> = None;
    let mut runs: Option<u32> = None;
    for l in ev.launches.iter().filter(|l| launch_is_of(l, f, home.as_deref(), &exe_keys)) {
        if l.last > 0 && best.is_none_or(|(t, _)| l.last > t) {
            best = Some((l.last, "userassist"));
        }
        if l.count > 0 {
            runs = runs.max(Some(l.count));
        }
    }
    for exe in &exe_keys {
        let name: String = name_of(exe).chars().take(PREFETCH_NAME).collect();
        if is_generic_exe(&name) {
            continue;
        }
        if let Some(&t) = ev.prefetch.get(&name)
            && best.is_none_or(|(b, _)| t > b)
        {
            best = Some((t, "prefetch"));
        }
    }
    if best.is_none() && ev.access_kept {
        let read = exes.iter().filter(|e| !is_generic_exe(name_of(e))).take(STAT_MOST);
        let newest = read.filter_map(|e| std::fs::metadata(e).ok()?.accessed().ok().and_then(filetime_of)).max();
        best = newest.map(|t| (t, "exe"));
    }
    f.program.last_used = best.map(|(t, _)| t);
    f.program.last_used_source = best.map(|(_, s)| s.to_string());
    f.program.run_count = runs;
}

// ---- Uninstall ----------------------------------------------------------------------

fn uninstall(id: &str) -> Result<(), String> {
    match id.split_once('\\') {
        Some((tag, key_name)) => {
            let hive = Hive::of_tag(tag).ok_or(BAD_ID)?;
            if key_name.is_empty() || key_name.len() > 255 || key_name.contains(['\\', '/']) {
                return Err(BAD_ID.into());
            }
            let places = Places::now();
            let found = read_entry(hive, key_name, &places).ok_or(NOT_FOUND)?;
            match found.uninstall {
                Some(command) if found.program.can_uninstall => start_uninstaller(&command, &places),
                _ => Err(NO_UNINSTALLER.into()),
            }
        }
        None => {
            if !is_package_name(id) {
                return Err(BAD_ID.into());
            }
            let app = store_apps(Duration::ZERO)?.into_iter().find(|a| a.full_name == id).ok_or(NOT_FOUND)?;
            if app.non_removable {
                return Err(NOT_REMOVABLE.into());
            }
            remove_package(&app.full_name)
        }
    }
}

/// The program an UninstallString names, as it is started: a full path as
/// it is, a bare program name (`MsiExec.exe`, `rundll32`) only as Windows'
/// own in System32. Anything else (a bare name System32 does not have, a
/// relative path) is not started: the shell would look for it in this app's
/// working folder (its install folder, with its own uninstall.exe) and on
/// PATH, and might start some other program.
fn uninstaller_exe(exe: &str, system32: Option<&str>) -> Result<String, &'static str> {
    let full = exe.replace('/', "\\");
    if is_absolute(&full) {
        return Ok(full);
    }
    if exe.contains(['\\', '/', ':']) {
        return Err(NO_UNINSTALLER);
    }
    let file = if exe.contains('.') { exe.to_string() } else { format!("{exe}.exe") };
    system32.map(|s| format!(r"{s}\{file}")).filter(|p| Path::new(p).is_file()).ok_or(NO_UNINSTALLER)
}

/// Whether `command` names something `start_uninstaller` would start (a
/// full path is checked only then).
fn startable(command: &Uninstall, places: &Places) -> bool {
    match command {
        Uninstall::Run { exe, .. } => uninstaller_exe(exe, places.system32.as_deref()).is_ok(),
        Uninstall::Msi(_) | Uninstall::Store(_) => true,
    }
}

/// Starts an uninstaller through the shell, which asks for administrator
/// rights when it needs them (`uninstaller_exe` says which program).
fn start_uninstaller(command: &Uninstall, places: &Places) -> Result<(), String> {
    let (exe, args) = match command {
        Uninstall::Msi(code) => (places.system32.as_ref().map_or("msiexec.exe".into(), |s| format!(r"{s}\msiexec.exe")), format!("/X{code}")),
        Uninstall::Run { exe, args } => {
            let exe = uninstaller_exe(exe, places.system32.as_deref())?;
            // An app execution alias (in `AppData\Local\Microsoft\WindowsApps`) is
            // a link no file API opens; the shell runs it all the same.
            if !std::fs::symlink_metadata(&exe).is_ok_and(|m| !m.is_dir()) {
                return Err(UNINSTALLER_MISSING.into());
            }
            (exe, args.clone())
        }
        Uninstall::Store(name) => return remove_package(name),
    };
    shell_start(&exe, &args)
}

/// COM for the shell on this thread, while it lives.
struct Com(bool);

impl Com {
    fn init() -> Com {
        Com(unsafe { CoInitializeEx(null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32) } >= 0)
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

/// ShellExecuteEx (the default verb) of `exe` with `args`, in its folder.
/// Returns once it has started (and its UAC prompt, if any, is answered).
fn shell_start(exe: &str, args: &str) -> Result<(), String> {
    let _com = Com::init();
    let file = win::wide(exe);
    let params = win::wide(args);
    let dir = parent_of(exe).filter(|d| is_absolute(d)).map(|d| win::wide(&format!("{d}\\")));
    let ok = unsafe {
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOASYNC;
        info.lpFile = file.as_ptr();
        info.lpParameters = if args.is_empty() { null() } else { params.as_ptr() };
        info.lpDirectory = dir.as_ref().map_or(null(), |d| d.as_ptr());
        info.nShow = SW_SHOWNORMAL;
        ShellExecuteExW(&mut info) != 0
    };
    if ok {
        return Ok(());
    }
    Err(match unsafe { GetLastError() } {
        ERROR_CANCELLED => UAC_CANCELLED.into(),
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => UNINSTALLER_MISSING.into(),
        e => format!("제거 프로그램을 시작하지 못했어요 (오류 {e})"),
    })
}

/// Remove-AppxPackage of one package, for this user. Waits REMOVE_LIMIT for
/// it to finish, to tell when it fails; after that it goes on by itself.
fn remove_package(full_name: &str) -> Result<(), String> {
    if !is_package_name(full_name) {
        return Err(BAD_ID.into());
    }
    let ps = powershell().ok_or(NO_POWERSHELL)?;
    let script = format!(
        "$ErrorActionPreference='Stop'; [Console]::OutputEncoding=[Text.Encoding]::UTF8; Remove-AppxPackage -Package '{full_name}'"
    );
    let ran = run_hidden(&ps, &["-NoProfile", "-NonInteractive", "-Command", &script], REMOVE_LIMIT, false);
    *STORE.lock().unwrap_or_else(PoisonError::into_inner) = None;
    let ran = ran?;
    if !ran.done || ran.code == Some(0) {
        return Ok(());
    }
    let said: String = ran.err.split_whitespace().collect::<Vec<_>>().join(" ").chars().filter(|c| !c.is_control()).take(300).collect();
    Err(if said.is_empty() { REMOVE_FAILED.into() } else { format!("{REMOVE_FAILED}: {said}") })
}

// ---- Leftovers ----------------------------------------------------------------------

/// Folders straight in Program Files, ProgramData and AppData that are
/// Windows', shared by many programs, or kept by drivers and tools that
/// leave no Uninstall entry (caches of package managers among them): never
/// called left behind. Names in lower case.
const KEPT: &[&str] = &[
    "common files",
    "package cache",
    "packages",
    "temp",
    "tmp",
    "programs",
    "internet explorer",
    "reference assemblies",
    "msbuild",
    "dotnet",
    "uninstall information",
    "packagemanagement",
    "modifiablewindowsapps",
    // Drivers (INF/DCH packages from Windows Update or the PC's maker) put
    // these next to themselves with no Uninstall entry: sound, touchpad,
    // network, fingerprint. Taking one away breaks the device's extras.
    "intel",
    "realtek",
    "waves",
    "maxxaudio",
    "dolby",
    "dts",
    "dts, inc",
    "nahimic",
    "a-volute",
    "sonic studio",
    "conexant",
    "synaptics",
    "elantech",
    "elan",
    "alps",
    "goodix",
    "rivet networks",
    "killer networking",
    "qualcomm",
    "qualcomm atheros",
    "broadcom",
    "mediatek",
    "cirrus logic",
    "idt",
    "hyper-v",
    "iis",
    "iis express",
    "difx",
    "rempl",
    "application data",
    "desktop",
    "documents",
    "start menu",
    "templates",
    "favorites",
    "history",
    "ssh",
    "usoprivate",
    "usoshared",
    "softwaredistribution",
    "comms",
    "connecteddevicesplatform",
    "d3dscache",
    "crashdumps",
    "placeholdertilelogofolder",
    "publishers",
    "virtualstore",
    "elevateddiagnostics",
    "lxss",
    "squirreltemp",
    "chocolatey",
    "pip",
    "pypa",
    "npm",
    "npm-cache",
    "node-gyp",
    "yarn",
    "pnpm",
    "nuget",
    "electron",
    "electron-builder",
    "ms-playwright",
    "go-build",
    "deno",
    "uv",
    "tauri",
    "cypress",
    // Windows Subsystem for Linux: its Linux disks live here.
    "wsl",
    // WebView2's data, of whichever program started it.
    "ebwebview",
    "fine-file",
    "com.toxh13.fine-file",
];

/// Names starting so are Windows' or Microsoft's, an installer's cache
/// (`{GUID}`), or a tool's downloads.
const KEPT_PREFIXES: &[&str] = &["microsoft", "windows", "{", "ms-playwright"];

/// Words in names that say nothing about whose a folder is.
const GENERIC_WORDS: &[&str] = &[
    "com", "net", "org", "app", "apps", "data", "desktop", "local", "user", "users", "updater", "update", "updates", "helper",
    "service", "services", "tool", "tools", "manager", "store", "launcher", "client", "software", "program", "programs",
    "files", "cache", "temp", "setup", "installer", "studio", "player", "center", "plus", "free", "lite", "beta",
    "edition", "version", "application", "common", "shared", "bin", "x64", "x86",
    // Makers' names: "NVIDIA Corporation" is not Microsoft Corporation's.
    "corporation", "corp", "inc", "llc", "ltd", "limited", "gmbh", "company", "technologies", "technology", "systems",
    "labs", "group", "foundation", "games", "entertainment", "interactive", "digital", "media", "international",
    "global", "solutions", "online", "network", "networks", "cloud", "electronics",
];

/// What else a folder's name may be called by: each telling word of it
/// (`com.github.githubapp` → `github`, `githubapp`), its first two words
/// together (`PC Manager Store` → `pcmanager`), and its letters without an
/// ending programs add (`DBeaverData` → `dbeaver`, `notion-updater` → `notion`).
fn folder_aliases(name: &str) -> Vec<String> {
    let s = squash(name);
    let all = words(name);
    let telling = all.iter().filter(|w| w.chars().count() >= 4 && !GENERIC_WORDS.contains(&w.as_str())).cloned();
    let first_two = all.get(..2).map(|w| w.concat()).filter(|w| w.chars().count() >= 5);
    let endings = ["updater", "update", "userdata", "data", "cache", "desktop", "launcher"];
    let shorter = endings.iter().filter_map(|e| s.strip_suffix(e)).filter(|stem| stem.chars().count() >= 4).map(String::from);
    let mut out: Vec<String> = Vec::new();
    for alias in telling.chain(first_two).chain(shorter) {
        if alias != s && !out.contains(&alias) {
            out.push(alias);
        }
    }
    out
}

/// Driver makers' folders, and the names of theirs that a program of theirs has.
const DRIVER_VENDORS: &[(&str, &[&str])] = &[
    ("nvidia corporation", &["nvidia"]),
    ("nvidia", &["nvidia"]),
    ("amd", &["amd", "ati", "advancedmicrodevices"]),
    ("ati technologies", &["amd", "ati", "advancedmicrodevices"]),
];

fn is_kept(name: &str) -> bool {
    let n = name.to_lowercase();
    KEPT.contains(&n.as_str()) || KEPT_PREFIXES.iter().any(|p| n.starts_with(p))
}

/// A program's name as it is compared.
struct Name {
    squash: String,
    words: Vec<String>,
}

impl Name {
    fn of(s: &str) -> Name {
        Name { squash: squash(s), words: words(s) }
    }
}

/// Whether a folder's name and the name `other` name the same thing: the
/// same letters and digits, one starting with the other (three or more),
/// one in the other (five or more), or the folder's name a word of `other`.
#[cfg(test)]
fn names_match(folder: &str, other: &str) -> bool {
    squashed_match(&squash(folder), &Name::of(other))
}

fn squashed_match(f: &str, other: &Name) -> bool {
    let n = other.squash.as_str();
    let (fl, nl) = (f.chars().count(), n.chars().count());
    if fl == 0 || nl == 0 {
        return false;
    }
    f == n
        || (fl >= 3 && n.starts_with(f))
        || (nl >= 3 && f.starts_with(n))
        || (fl >= 5 && n.contains(f))
        || (nl >= 5 && f.contains(n))
        || (fl >= 3 && other.words.iter().any(|w| w == f))
}

/// What installed programs say about where they are and what they are called.
struct Owners {
    /// Their folders and executables (`path_key`s).
    paths: Vec<String>,
    /// Their names, key names and publishers, and the names of the folders
    /// they are in below Program Files, ProgramData and AppData (a maker's
    /// folder there is often called so in AppData too: `Hnc`, `Naver`).
    names: Vec<Name>,
}

impl Owners {
    fn of(found: &[Found], places: &Places) -> Owners {
        let mut paths = Vec::new();
        let mut names: Vec<String> = Vec::new();
        for f in found {
            paths.extend(f.home.iter().chain(&f.exes).map(|p| path_key(p)));
            paths.extend(f.program.install_location.iter().map(|l| path_key(l)).filter(|l| !places.is_broad(l)));
            names.extend(f.names.iter().cloned());
        }
        paths.sort();
        paths.dedup();
        for p in &paths {
            for (base, _) in &places.bases {
                let base = path_key(base);
                if below(p, &base) {
                    let folders = p[base.len() + 1..].split('\\').take(2);
                    names.extend(folders.filter(|n| n.chars().count() >= 3 && !GENERIC_WORDS.contains(n) && !n.ends_with(".exe")).map(String::from));
                }
            }
        }
        names.sort();
        names.dedup();
        Owners { paths, names: names.iter().map(|n| Name::of(n)).collect() }
    }

    /// Whether a program accounts for the folder `key` (a `path_key`) named
    /// `name`: it is in that folder, or its name matches the folder's
    /// (`names_match`), a telling word or a shorter form of it
    /// (`folder_aliases`), or has the folder's first two words in a row.
    fn own(&self, key: &str, name: &str) -> bool {
        if self.paths.iter().any(|p| p == key || below(p, key)) {
            return true;
        }
        let mut forms = vec![squash(name)];
        forms.extend(folder_aliases(name));
        if self.names.iter().any(|n| forms.iter().any(|f| squashed_match(f, n))) {
            return true;
        }
        let w = words(name);
        w.len() >= 2 && self.names.iter().any(|n| n.words.windows(2).any(|pair| pair == &w[..2]))
    }

    /// For a driver maker's folder: whether a program of theirs is installed.
    fn has_vendor(&self, aliases: &[&str]) -> bool {
        self.names.iter().any(|n| aliases.iter().any(|a| n.words.iter().any(|x| x == a) || (a.len() >= 4 && n.squash.starts_with(a))))
    }
}

/// Why a folder looks left behind, in Korean.
fn leftover_reason(area: Area, vendor: bool, recent_code: bool) -> String {
    let mut why = String::from("설치된 프로그램 중 이 폴더의 이름·위치와 맞는 것이 없어요. ");
    why.push_str(match area {
        Area::ProgramFiles => "프로그램을 지운 뒤 남은 설치 폴더일 수 있어요",
        Area::ProgramData => "지운 프로그램이 모든 사용자용으로 남긴 데이터일 수 있어요",
        Area::AppData => "지운 프로그램의 설정·캐시·저장 파일일 수 있어요 (포터블 프로그램이나 게임 저장 파일일 수도 있어요)",
    });
    if vendor {
        why.push_str(" · 그래픽 드라이버(NVIDIA·AMD)가 쓰는 폴더일 수 있어요. 드라이버를 Windows 업데이트로 받았다면 지우지 마세요");
    }
    if recent_code {
        why.push_str(&format!(" · 최근 {RECENT_CODE_DAYS}일 안에 바뀐 실행 파일(.exe·.dll)이 있어서 아직 쓰는 프로그램일 수 있어요"));
    }
    why
}

fn is_code(name: &str) -> bool {
    let n = name.to_lowercase();
    n.ends_with(".exe") || n.ends_with(".dll")
}

/// The newest change in an indexed folder, and whether an executable or a
/// DLL in it changed at `since` or later.
fn look_inside(ix: &Index, top: u32, since: i64) -> (i64, bool) {
    let mut newest = ix.nodes[top as usize].modified;
    let mut code = false;
    let mut stack = vec![top];
    let mut seen = 0usize;
    while let Some(dir) = stack.pop() {
        for c in ix.children_of(dir) {
            seen += 1;
            if seen > WALK_MOST {
                return (newest, code);
            }
            let n = &ix.nodes[c as usize];
            if n.flags & FLAG_DELETED != 0 {
                continue;
            }
            newest = newest.max(n.modified);
            if n.flags & FLAG_DIR != 0 {
                if !is_link_dir(n.flags) {
                    stack.push(c);
                }
            } else if !code && n.modified >= since && is_code(ix.name(c)) {
                code = true;
            }
        }
    }
    (newest, code)
}

/// The folders straight in `base` (indexed) that look left behind.
fn leftovers_in(ix: &Index, base: &str, area: Area, owners: &Owners, own: &[String], now: i64, out: &mut Vec<Leftover>) {
    let Some(top) = ix.find(base) else { return };
    let base = base.trim_end_matches('\\');
    for c in ix.children_of(top) {
        let node = ix.nodes[c as usize];
        if !is_plain_dir(node.flags) || node.size < LEFTOVER_MIN {
            continue;
        }
        let name = ix.name(c);
        let path = format!("{base}\\{name}");
        let key = path_key(&path);
        if is_kept(name) || own.contains(&key) {
            continue;
        }
        let category = classify::category(&path, name, true);
        if matches!(category, "system" | "cache" | "dev") || classify::program_category(&path, name, true) == Some("system") {
            continue;
        }
        let vendor = DRIVER_VENDORS.iter().find(|(v, _)| *v == name.to_lowercase()).map(|(_, a)| *a);
        if owners.own(&key, name) || vendor.is_some_and(|a| owners.has_vendor(a)) {
            continue;
        }
        let (modified, recent_code) = look_inside(ix, c, now - RECENT_CODE_DAYS * DAY);
        let risk = if recent_code || vendor.is_some() { "danger" } else { "caution" };
        out.push(Leftover { path, size: node.size, modified, reason: leftover_reason(area, vendor.is_some(), recent_code), risk: risk.into() });
    }
}

/// This app's own folders: its data and where it is installed.
fn own_folders() -> Vec<String> {
    let mut own = vec![path_key(&crate::scan::data_dir().to_string_lossy())];
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| path_key(&p.to_string_lossy()))) {
        own.push(dir);
    }
    own
}

fn find_leftovers(indexes: &Indexes) -> Result<Vec<Leftover>, String> {
    let places = Places::now();
    // System components and updates say whose a folder is too (WSL, parts of drivers).
    let mut found = registry_programs(&places, true);
    // A Store app's name accounts for a folder too; without the list, a few more look left behind.
    found.extend(store_apps(STORE_FRESH).unwrap_or_default().into_iter().map(|a| store_found(a, &places)));
    let owners = Owners::of(&found, &places);
    let own = own_folders();
    let now = crate::index::now_filetime();
    let mut out = Vec::new();
    let mut scanned = false;
    for (base, area) in &places.bases {
        let Some(shared) = indexes.of_path(base) else { continue };
        scanned = true;
        let ix = shared.read().unwrap_or_else(PoisonError::into_inner);
        leftovers_in(&ix, base, *area, &owners, &own, now, &mut out);
    }
    if !scanned {
        return Err(SCAN_FIRST.into());
    }
    let mut seen = HashSet::new();
    out.retain(|l| seen.insert(path_key(&l.path)));
    out.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
    out.truncate(LEFTOVER_MOST);
    Ok(out)
}

/// The deepest folder that holds every one of `paths` (each below it).
fn common_folder(paths: &[&str]) -> Option<String> {
    let first: Vec<&str> = paths.first()?.split('\\').collect();
    let mut len = first.len().saturating_sub(1);
    for p in &paths[1..] {
        let parts: Vec<&str> = p.split('\\').collect();
        let same = first.iter().zip(&parts[..parts.len().saturating_sub(1)]).take_while(|(a, b)| a.eq_ignore_ascii_case(b)).count();
        len = len.min(same);
    }
    match len {
        0 => None,
        1 => Some(format!("{}\\", first[0])),
        n => Some(first[..n].join("\\")),
    }
}

fn leftovers_plan(indexes: &Indexes, paths: &[String]) -> Result<Plan, String> {
    if paths.is_empty() {
        return Err(NOTHING_PICKED.into());
    }
    let current = find_leftovers(indexes)?;
    let by_key: HashMap<String, &Leftover> = current.iter().map(|l| (path_key(&l.path), l)).collect();
    let mut picked: Vec<&Leftover> = Vec::new();
    let mut seen = HashSet::new();
    for p in paths {
        let key = path_key(p);
        let l = *by_key.get(&key).ok_or(NOT_LEFTOVER)?;
        if seen.insert(key) {
            picked.push(l);
        }
    }
    let root = common_folder(&picked.iter().map(|l| l.path.as_str()).collect::<Vec<_>>()).ok_or(ONE_DRIVE)?;
    let items = picked
        .iter()
        .map(|l| TrashItem { path: l.path.clone(), size: l.size, risk: l.risk.clone(), reason: l.reason.clone() })
        .collect();
    plans::save_rules_trash(&root, "남은 프로그램 폴더 정리", items, leftover_summary)
}

fn leftover_summary(steps: &[PlanStep]) -> String {
    let total: u64 = steps.iter().map(|s| s.size).sum();
    let blocked = steps.iter().filter(|s| s.state == "skipped").count();
    let mut summary = format!("설치된 어느 프로그램의 것도 아닌 것 같은 폴더 {}개, 모두 {}.", steps.len(), size_text(total));
    if blocked < steps.len() {
        summary.push_str(&format!(
            " 휴지통으로 보낼 수 있는 {}개는 처음에 골라 두지 않았어요. 폴더를 열어 확인한 뒤 골라 주세요.",
            steps.len() - blocked
        ));
    }
    if blocked > 0 {
        summary.push_str(&format!(
            "\n\n{blocked}개는 Program Files·ProgramData·AppData처럼 정리 계획이 건드리지 않는 곳에 있어서 이 계획으로는 지울 수 없어요 (단계마다 이유가 있어요). 탐색기에서 직접 확인하고 지워 주세요."
        ));
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::RawEntry;

    fn places() -> Places {
        let bases = vec![
            (r"C:\Program Files".to_string(), Area::ProgramFiles),
            (r"C:\ProgramData".to_string(), Area::ProgramData),
            (r"C:\Users\me\AppData\Local".to_string(), Area::AppData),
        ];
        let mut broad: HashSet<String> = bases.iter().map(|(b, _)| path_key(b)).collect();
        broad.extend([r"c:\windows", r"c:\users\me", r"c:\program files\common files"].map(String::from));
        Places { windows: Some(r"c:\windows".into()), system32: Some(r"C:\Windows\System32".into()), bases, broad }
    }

    fn program(name: &str, publisher: Option<&str>, home: Option<&str>) -> Found {
        Found {
            program: Program {
                id: format!("HKLM64\\{name}"),
                name: name.into(),
                publisher: publisher.map(String::from),
                version: None,
                installed_at: None,
                install_location: home.map(String::from),
                size: None,
                size_source: "unknown".into(),
                last_used: None,
                last_used_source: None,
                run_count: None,
                kind: "win32".into(),
                can_uninstall: true,
                machine_wide: true,
            },
            home: home.map(String::from),
            stated: home.is_some(),
            exes: Vec::new(),
            names: [Some(name), publisher].into_iter().flatten().map(String::from).collect(),
            family: None,
            estimated: None,
            uninstall: None,
        }
    }

    /// A program as `placed` gives it: its own folder (`stated`: its
    /// InstallLocation, else the folder of its icon or uninstaller), live in an index.
    fn placed_at(name: &str, home: Option<&str>, stated: bool, can_uninstall: bool, size: u64) -> Placed {
        let f = program(name, Some("Maker"), home);
        Placed {
            program: ProgramRef { can_uninstall, size: Some(size), ..ProgramRef::of(&f.program) },
            location: f.program.install_location,
            folder: f.home.clone(),
            home: f.home,
            stated: stated && home.is_some(),
        }
    }

    fn installed_still(_: &str) -> bool {
        true
    }

    #[test]
    fn a_folder_is_a_programs_only_when_it_is_its_own() {
        let placed = |name: &str, home: Option<&str>, can_uninstall: bool, size: u64| placed_at(name, home, true, can_uninstall, size);
        let list = vec![
            placed("ELDEN RING", Some(r"D:\SteamLibrary\steamapps\common\ELDEN RING"), true, 50 << 30),
            placed("Office", Some(r"C:\Program Files\Microsoft Office"), true, 3 << 30),
            placed("Office ko-kr", Some(r"C:\Program Files\Microsoft Office\"), true, 1 << 30),
            placed("Office helper", Some(r"c:\program files\microsoft office"), false, 9 << 30),
            placed("No folder", None, true, 1),
        ];
        let at = |p: &str| program_in(&list, p, &installed_still).map(|r| r.name);
        // Exactly its folder, in any case, with or without a `\` at the end.
        assert_eq!(at(r"d:\steamlibrary\STEAMAPPS\common\elden ring\").as_deref(), Some("ELDEN RING"));
        // Not a folder in it, not one above it, not a drive.
        assert_eq!(at(r"D:\SteamLibrary\steamapps\common\ELDEN RING\Game"), None);
        assert_eq!(at(r"D:\SteamLibrary\steamapps\common"), None);
        assert_eq!(at(r"D:\"), None);
        // Shared by a suite and its parts: the one that can be removed, then the biggest.
        assert_eq!(at(r"C:\Program Files\Microsoft Office").as_deref(), Some("Office"));
        let r = program_in(&list, r"C:\Program Files\Microsoft Office", &installed_still).unwrap();
        assert_eq!((r.id.as_str(), r.publisher.as_deref(), r.kind.as_str(), r.machine_wide), (r"HKLM64\Office", Some("Maker"), "win32", true));
        // One that is gone since the list was read (its uninstaller ran): the next one, or none.
        let gone = |id: &str| id != r"HKLM64\Office";
        assert_eq!(program_in(&list, r"C:\Program Files\Microsoft Office", &gone).map(|r| r.name).as_deref(), Some("Office ko-kr"));
        assert!(program_in(&list, r"D:\SteamLibrary\steamapps\common\ELDEN RING", &|_| false).is_none());
        // A place Windows needs is never offered.
        let windows = vec![placed("Windows Help", Some(r"C:\Windows\Help"), true, 1)];
        assert!(program_in(&windows, r"C:\Windows\Help", &installed_still).is_none());
        // One that cannot be removed is still named (the window says why it cannot).
        let only = vec![placed("Locked", Some(r"C:\Tools\Locked"), false, 1)];
        assert_eq!(program_in(&only, r"C:\Tools\Locked", &installed_still).map(|r| r.can_uninstall), Some(false));
        assert!(program_in(&[], r"C:\Tools\Locked", &installed_still).is_none());

        // A launcher's folder is the launcher's (its InstallLocation), not that of a game
        // whose uninstaller is the launcher's exe (the folder only guessed from it).
        let launcher = r"C:\Program Files (x86)\Ubisoft\Ubisoft Game Launcher";
        let list = vec![placed_at("Assassin's Creed", Some(launcher), false, true, 1 << 30), placed_at("Ubisoft Connect", Some(launcher), true, true, 1 << 30)];
        assert_eq!(program_in(&list, launcher, &installed_still).map(|r| r.name).as_deref(), Some("Ubisoft Connect"));
        let mut both = list.clone();
        both.sort_by(owner_first);
        assert_eq!(both[0].program.name, "Ubisoft Connect");
    }

    #[test]
    fn the_users_own_folders_are_no_programs() {
        use windows_sys::Win32::UI::Shell::{FOLDERID_Documents, FOLDERID_Downloads};
        // An installer that left its icon in Downloads, or named it as where it is installed.
        for id in [FOLDERID_Downloads, FOLDERID_Documents, FOLDERID_Profile] {
            let Some(dir) = known_folder(&id) else { continue };
            let list = vec![placed_at("Foo", Some(&dir), true, true, 1)];
            assert!(program_in(&list, &dir, &installed_still).is_none(), "{dir}");
            let p = Places::now();
            assert!(p.is_broad(&path_key(&dir)), "{dir}");
            assert_eq!(infer_home(&[format!(r"{dir}\FooSetup.exe")], &p), None, "{dir}");
            // A folder of its own in there still is one.
            let own = format!(r"{dir}\Tools\Foo");
            assert_eq!(infer_home(&[format!(r"{own}\foo.exe")], &p).as_deref(), Some(own.as_str()));
            let list = vec![placed_at("Foo", Some(&own), false, true, 1)];
            assert_eq!(program_in(&list, &own, &installed_still).map(|r| r.name).as_deref(), Some("Foo"));
        }
    }

    #[test]
    fn where_programs_are_is_asked_of_the_indexes_as_they_are_now() {
        let raw = vec![
            RawEntry { id: 1, parent: 1, name: ".".into(), flags: FLAG_DIR, size: 0, alloc: 0, modified: 0 },
            RawEntry { id: 2, parent: 1, name: "Games".into(), flags: FLAG_DIR, size: 0, alloc: 0, modified: 0 },
            RawEntry { id: 3, parent: 2, name: "Foo".into(), flags: FLAG_DIR, size: 0, alloc: 0, modified: 0 },
            RawEntry { id: 4, parent: 3, name: "foo.exe".into(), flags: 0, size: 5 * GB, alloc: 5 * GB, modified: 0 },
        ];
        let d: Shared = std::sync::Arc::new(std::sync::RwLock::new(Index::build("D:", "test", 0, raw, 1)));
        let listed = |home: &str| Listed {
            program: ProgramRef::of(&program("Foo", None, Some(home)).program),
            location: Some(home.into()),
            home: Some(home.into()),
            stated: true,
            estimated: Some(MB),
        };
        let list = vec![listed(r"D:\Games\Foo"), listed(r"D:\Games\Gone")];
        // D: not scanned yet: its own estimate, no folder.
        let before = in_indexes(list.clone(), &Indexes(HashMap::new()));
        assert_eq!((before[0].folder.as_deref(), before[0].program.size), (None, Some(MB)));
        // Scanned since: the folder counts at once, measured.
        let after = in_indexes(list, &Indexes::of(vec![d]));
        assert_eq!((after[0].folder.as_deref(), after[0].program.size), (Some(r"D:\Games\Foo"), Some(5 * GB)));
        assert_eq!((after[1].folder.as_deref(), after[1].program.size), (None, Some(MB)));
        assert!(after[0].stated);
    }

    fn userassist_data(count: u32, last: i64) -> Vec<u8> {
        let mut data = vec![0u8; 72];
        data[4..8].copy_from_slice(&count.to_le_bytes());
        data[60..68].copy_from_slice(&last.to_le_bytes());
        data
    }

    #[test]
    fn userassist_names_are_rot13_with_known_folders_in_front() {
        assert_eq!(rot13("Hello, World! 123"), "Uryyb, Jbeyq! 123");
        assert_eq!(rot13(&rot13("{6D809377}\\7-Zip\\7zFM.exe")), "{6D809377}\\7-Zip\\7zFM.exe");
        // `{FOLDERID_ProgramFilesX64}\7-Zip\7zFM.exe`, as UserAssist keeps it.
        let name = r"{6Q809377-6NS0-444O-8957-N3773S02200R}\7-Mvc\7mSZ.rkr";
        let mut asked = Vec::new();
        let mut resolve = |g: &str| {
            asked.push(g.to_string());
            (g == "{6D809377-6AF0-444B-8957-A3773F02200E}").then(|| r"C:\Program Files".to_string())
        };
        let at = 133_500_000_000_000_000i64;
        let l = launch_of(name, &userassist_data(7, at), &mut resolve).unwrap();
        assert_eq!(l, Launch { target: r"c:\program files\7-zip\7zfm.exe".into(), count: 7, last: at });
        assert_eq!(asked, ["{6D809377-6AF0-444B-8957-A3773F02200E}"]);
        // An app id: no folder to spell out.
        let app = rot13("Microsoft.WindowsCalculator_8wekyb3d8bbwe!App");
        let l = launch_of(&app, &userassist_data(2, at), &mut |_| None).unwrap();
        assert_eq!(l.target, "microsoft.windowscalculator_8wekyb3d8bbwe!app");
        // An unknown folder id stays as it is.
        let other = rot13(r"{0139D44E-6AFE-49F2-8690-3DAFCAE6FFB8}\Notepad++.lnk");
        assert_eq!(launch_of(&other, &userassist_data(1, at), &mut |_| None).unwrap().target, r"{0139d44e-6afe-49f2-8690-3dafcae6ffb8}\notepad++.lnk");
        // Counters, short (Windows XP) data, nothing recorded.
        assert!(launch_of(&rot13("UEME_CTLSESSION"), &userassist_data(3, at), &mut |_| None).is_none());
        assert!(launch_of(name, &[0u8; 16], &mut |_| None).is_none());
        assert!(launch_of(name, &userassist_data(0, 0), &mut |_| None).is_none());
        assert_eq!(guid_of("{6D809377-6AF0-444B-8957-A3773F02200E}").map(|g| (g.data1, g.data2, g.data4)), Some((0x6D80_9377, 0x6AF0, [0x89, 0x57, 0xA3, 0x77, 0x3F, 0x02, 0x20, 0x0E])));
        assert!(guid_of("{6D809377-6AF0-444B-8957-A3773F02200}").is_none());
    }

    #[test]
    fn launches_are_matched_to_programs() {
        let at = 133_500_000_000_000_000i64;
        let mut zip = program("7-Zip 23.01 (x64)", Some("Igor Pavlov"), Some(r"C:\Program Files\7-Zip"));
        let mut steam = program("Steamworks Common Redistributables", Some("Valve"), None);
        let launch = |target: &str, count, last| Launch { target: target.into(), count, last };
        let ev = Evidence {
            launches: vec![
                launch(r"c:\program files\7-zip\7zfm.exe", 4, at),
                launch(r"c:\program files\7-zip\uninstall.exe", 1, at + DAY),
                launch(r"c:\programdata\microsoft\windows\start menu\programs\7-zip\7-zip file manager.lnk", 9, at - DAY),
                launch(r"c:\users\me\desktop\steam.lnk", 3, at),
            ],
            prefetch: HashMap::from([("7zfm.exe".to_string(), at + 2 * DAY), ("unins000.exe".to_string(), at + 9 * DAY)]),
            access_kept: false,
        };
        let indexes = Indexes(HashMap::new());
        measure(&mut zip, &indexes, &ev);
        // The uninstaller's start is no use; the shortcut in the 7-Zip folder is.
        assert_eq!(zip.program.run_count, Some(9));
        assert_eq!((zip.program.last_used, zip.program.last_used_source.as_deref()), (Some(at), Some("userassist")));
        // Prefetch knows of a later start, by an executable the index (here: the key) names.
        zip.exes.push(r"C:\Program Files\7-Zip\7zFM.exe".into());
        measure(&mut zip, &indexes, &ev);
        assert_eq!((zip.program.last_used, zip.program.last_used_source.as_deref()), (Some(at + 2 * DAY), Some("prefetch")));
        // "Steam" is not "Steamworks ...".
        measure(&mut steam, &indexes, &ev);
        assert_eq!((steam.program.last_used, steam.program.run_count), (None, None));
        assert!(shortcut_matches("Notepad++", "Notepad++ (64-bit x64)"));
        assert!(shortcut_matches("Visual Studio Code", "Microsoft Visual Studio Code (User)"));
        assert!(!shortcut_matches("Steam", "Steamworks Common Redistributables"));
        assert!(!shortcut_matches("Go", "Go Programming Language"));
        // Store apps by their app id.
        let mut calc = program("계산기", None, None);
        calc.family = family_of("Microsoft.WindowsCalculator", "Microsoft.WindowsCalculator_11.2307.4.0_x64__8wekyb3d8bbwe");
        let ev = Evidence { launches: vec![launch("microsoft.windowscalculator_8wekyb3d8bbwe!app", 5, at)], prefetch: HashMap::new(), access_kept: false };
        measure(&mut calc, &indexes, &ev);
        assert_eq!((calc.program.run_count, calc.program.last_used_source.as_deref()), (Some(5), Some("userassist")));
    }

    #[test]
    fn prefetch_names_and_last_access_settings() {
        assert_eq!(prefetch_exe("NOTEPAD++.EXE-1A2B3C4D.pf").as_deref(), Some("notepad++.exe"));
        assert_eq!(prefetch_exe("MY-TOOL.EXE-0000ABCD.pf").as_deref(), Some("my-tool.exe"));
        assert!(prefetch_exe("Layout.ini").is_none());
        assert!(prefetch_exe("AGGENTRY.DB").is_none());
        assert!(prefetch_exe("X.EXE-12.pf").is_none());
        let map = prefetch_map(&[("a.exe".into(), 5), ("a.exe".into(), 9), ("b.exe".into(), 1)]);
        assert_eq!((map["a.exe"], map["b.exe"]), (9, 1));
        for on in [0, 0x8000_0000, 0x8000_0002] {
            assert!(access_updates_on(Some(on)), "{on:#x}");
        }
        for off in [1, 0x8000_0001, 0x8000_0003] {
            assert!(!access_updates_on(Some(off)), "{off:#x}");
        }
        assert!(!access_updates_on(None));
        assert!(is_generic_exe("unins000.exe") && is_generic_exe("Uninstall.exe") && is_generic_exe("setup.exe"));
        assert!(!is_generic_exe("7zFM.exe") && !is_generic_exe("Code.exe"));
    }

    #[test]
    fn install_dates_become_filetimes() {
        // 2024-01-15 is day 19737 after 1970-01-01; noon UTC.
        let want = (19_737 * 86_400 + 43_200) * 10_000_000 + FILETIME_UNIX;
        assert_eq!(install_date("20240115"), Some(want));
        assert_eq!(install_date(" 2024-01-15 "), Some(want));
        assert_eq!(install_date("2024/01/15"), Some(want));
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert!(install_date("20240229").is_some());
        assert!(install_date("20230229").is_none());
        assert!(install_date("20241301").is_none());
        assert!(install_date("20240100").is_none());
        assert!(install_date("01152024").is_none());
        assert!(install_date("2024115").is_none());
        assert!(install_date("").is_none());
        assert!(install_date("abcdefgh").is_none());
    }

    #[test]
    fn uninstall_strings_split_into_program_and_arguments() {
        let split = split_command;
        let pair = |e: &str, a: &str| Some((e.to_string(), a.to_string()));
        assert_eq!(split(r#""C:\Program Files\Foo\unins000.exe""#), pair(r"C:\Program Files\Foo\unins000.exe", ""));
        assert_eq!(split(r#""C:\Program Files\Foo\uninstall.exe" /S"#), pair(r"C:\Program Files\Foo\uninstall.exe", "/S"));
        assert_eq!(split(r#""C:\x\Update.exe"--uninstall"#), pair(r"C:\x\Update.exe", "--uninstall"));
        assert_eq!(split(r"C:\Program Files\Foo Bar\uninstall.exe /x"), pair(r"C:\Program Files\Foo Bar\uninstall.exe", "/x"));
        assert_eq!(split(r"C:\Program Files (x86)\Foo\uninst.exe"), pair(r"C:\Program Files (x86)\Foo\uninst.exe", ""));
        assert_eq!(split(r"C:\Tools\my.exes\un.EXE -q  1"), pair(r"C:\Tools\my.exes\un.EXE", "-q  1"));
        assert_eq!(split("MsiExec.exe /X{12345678-ABCD-ABCD-ABCD-1234567890AB}"), pair("MsiExec.exe", "/X{12345678-ABCD-ABCD-ABCD-1234567890AB}"));
        assert_eq!(split("rundll32 dfshim.dll,ShArpMaintain app.application"), pair("rundll32", "dfshim.dll,ShArpMaintain app.application"));
        assert_eq!(split("  "), None);
        assert_eq!(split(r#""C:\unterminated.exe"#), None);
        assert_eq!(split(r#""" /x"#), None);

        let code = "{12345678-ABCD-ABCD-ABCD-1234567890AB}";
        // An MSI product by its key's name; /I (repair) and quiet switches become a plain /X.
        assert_eq!(uninstall_command(code, true, Some("MsiExec.exe /I{12345678-ABCD-ABCD-ABCD-1234567890AB}")), Some(Uninstall::Msi(code.into())));
        assert_eq!(uninstall_command("Foo", false, Some(r"C:\Windows\SysWOW64\msiexec.exe /x {12345678-abcd-abcd-abcd-1234567890ab} /qn")), Some(Uninstall::Msi(code.into())));
        assert_eq!(
            uninstall_command("Foo_is1", false, Some(r#""C:\Program Files\Foo\unins000.exe""#)),
            Some(Uninstall::Run { exe: r"C:\Program Files\Foo\unins000.exe".into(), args: String::new() })
        );
        assert_eq!(uninstall_command("Foo", false, Some("msiexec.exe /fa something")), Some(Uninstall::Run { exe: "msiexec.exe".into(), args: "/fa something".into() }));
        assert_eq!(uninstall_command("Foo", false, None), None);
        assert_eq!(icon_exe(r#""C:\Program Files\Foo\foo.exe",0"#).as_deref(), Some(r"C:\Program Files\Foo\foo.exe"));
        assert_eq!(icon_exe(r"C:\Program Files\Foo\foo.exe,-101").as_deref(), Some(r"C:\Program Files\Foo\foo.exe"));
        assert_eq!(icon_exe(r"C:\Program Files\Foo\foo.ico"), None);
        assert!(skipped(Some(1), None, None) && skipped(None, Some("Office16"), None) && skipped(None, None, Some("Security Update")));
        assert!(!skipped(Some(0), None, None) && !skipped(None, None, Some("")));
    }

    #[test]
    fn only_full_paths_and_windows_own_tools_are_started() {
        let sys = std::env::var("SystemRoot").map(|r| format!(r"{r}\System32")).unwrap();
        let s = Some(sys.as_str());
        // Windows' own, by a bare name, with or without `.exe`.
        assert_eq!(uninstaller_exe("MsiExec.exe", s), Ok(format!(r"{sys}\MsiExec.exe")));
        assert_eq!(uninstaller_exe("rundll32", s), Ok(format!(r"{sys}\rundll32.exe")));
        assert_eq!(uninstaller_exe(r"C:\Program Files\Foo\unins000.exe", s).as_deref(), Ok(r"C:\Program Files\Foo\unins000.exe"));
        assert_eq!(uninstaller_exe("C:/Program Files/Foo/unins000.exe", s).as_deref(), Ok(r"C:\Program Files\Foo\unins000.exe"));
        // Never what the shell would find in this app's folder or on PATH.
        for bad in ["uninstall.exe", "uninst", r".\uninstall.exe", r"Foo\uninst.exe", "../x.exe", "C:uninst.exe", r"\\server\share\un.exe"] {
            assert_eq!(uninstaller_exe(bad, s), Err(NO_UNINSTALLER), "{bad}");
        }
        assert_eq!(uninstaller_exe("msiexec.exe", None), Err(NO_UNINSTALLER));
        // So the list does not offer to remove such a program.
        let p = Places { system32: Some(sys.clone()), ..places() };
        let run = |exe: &str| Uninstall::Run { exe: exe.into(), args: String::new() };
        assert!(startable(&run(r"C:\Program Files\Foo\gone.exe"), &p) && startable(&run("rundll32.exe"), &p));
        assert!(!startable(&run("uninstall.exe"), &p) && !startable(&run(r".\uninstall.exe"), &p));
        assert!(startable(&Uninstall::Msi("{X}".into()), &p));
        let err = start_uninstaller(&run("ff-test-no-such-uninstaller.exe"), &p).unwrap_err();
        assert_eq!(err, NO_UNINSTALLER);
    }

    #[test]
    fn folders_of_programs_are_their_own_or_shared() {
        let p = places();
        assert!(p.is_broad(r"c:\program files") && p.is_broad(r"c:\") && p.is_broad(r"c:\users"));
        assert!(!p.is_broad(r"c:\program files\7-zip"));
        assert_eq!(clean_location(r#" "C:/Program Files/Foo/" "#).as_deref(), Some(r"C:\Program Files\Foo"));
        assert_eq!(clean_location(r"c:\Foo\").as_deref(), Some(r"C:\Foo"));
        assert_eq!(clean_location(r"C:\"), None);
        assert_eq!(clean_location(r"\\server\share\x"), None);
        assert_eq!(clean_location("Foo"), None);
        let exes = [r"C:\Program Files\Foo\uninstall\unins.exe".to_string()];
        assert_eq!(infer_home(&exes, &p).as_deref(), Some(r"C:\Program Files\Foo"));
        let cached = [r"C:\ProgramData\Package Cache\{1234}\setup.exe".to_string(), r"C:\Program Files\setup.exe".to_string()];
        assert_eq!(infer_home(&cached, &p), None);
        assert_eq!(common_folder(&[r"C:\Program Files\A"]).as_deref(), Some(r"C:\Program Files"));
        assert_eq!(common_folder(&[r"C:\Program Files\A", r"c:\program files\B"]).as_deref(), Some(r"C:\Program Files"));
        assert_eq!(common_folder(&[r"C:\Program Files\A", r"C:\ProgramData\B"]).as_deref(), Some(r"C:\"));
        assert_eq!(common_folder(&[r"C:\Program Files\A", r"D:\B"]), None);
    }

    #[test]
    fn leftover_folders_are_the_ones_no_program_accounts_for() {
        let now = 134_000_000_000_000_000i64;
        let old = now - 400 * DAY;
        let mut id = 10;
        let mut raw = vec![RawEntry { id: 1, parent: 1, name: ".".into(), flags: FLAG_DIR, size: 0, alloc: 0, modified: old }];
        let mut add = |parent: u64, name: &str, dir: bool, size: u64, modified: i64| -> u64 {
            id += 1;
            raw.push(RawEntry { id, parent, name: name.into(), flags: if dir { FLAG_DIR } else { 0 }, size, alloc: size, modified });
            id
        };
        let pf = add(1, "Program Files", true, 0, old);
        let pd = add(1, "ProgramData", true, 0, old);
        let users = add(1, "Users", true, 0, old);
        let me = add(users, "me", true, 0, old);
        let appdata = add(me, "AppData", true, 0, old);
        let local = add(appdata, "Local", true, 0, old);
        let big = 60 * MB;
        let folder = |add: &mut dyn FnMut(u64, &str, bool, u64, i64) -> u64, parent, name: &str, file: &str, modified| {
            let d = add(parent, name, true, 0, old);
            add(d, file, false, big, modified);
        };
        folder(&mut add, pf, "7-Zip", "7z.dll", old);
        folder(&mut add, pf, "Old App", "data.bin", old);
        folder(&mut add, pf, "Fresh App", "fresh.dll", now - DAY);
        folder(&mut add, pf, "Common Files", "x.bin", old);
        folder(&mut add, pf, "Microsoft Office", "x.bin", old);
        folder(&mut add, pf, "Windows Defender", "x.bin", old);
        folder(&mut add, pf, "NVIDIA Corporation", "nv.bin", old);
        // A sound driver's companion (MaxxAudio), which has no Uninstall entry.
        folder(&mut add, pf, "Waves", "MaxxAudio.dll", old);
        folder(&mut add, pd, "Dolby", "x.bin", old);
        folder(&mut add, pf, "VendorStuff", "x.bin", old);
        folder(&mut add, pd, "Package Cache", "x.bin", old);
        folder(&mut add, pd, "Gone Tool", "x.bin", old);
        folder(&mut add, local, "Code", "x.bin", old);
        folder(&mut add, local, "Google", "x.bin", old);
        folder(&mut add, local, "pip", "x.bin", old);
        folder(&mut add, local, "SomethingCache", "x.bin", old);
        folder(&mut add, local, "wsl", "ext4.vhdx", old);
        folder(&mut add, local, "HNC", "x.bin", old);
        folder(&mut add, local, "notion-updater", "x.bin", old);
        folder(&mut add, local, "com.github.githubapp", "x.bin", old);
        folder(&mut add, local, "PC Manager Store", "x.bin", old);
        folder(&mut add, local, "net.golbin.hop", "x.bin", old);
        let small = add(pf, "Small App", true, 0, old);
        add(small, "x.bin", false, 10 * MB, old);
        let ix = Index::build("C:", "test", 0, raw, 1);

        let p = places();
        let mut found = vec![
            program("7-Zip 23.01 (x64)", Some("Igor Pavlov"), Some(r"C:\Program Files\7-Zip")),
            program("Microsoft Visual Studio Code (User)", Some("Microsoft Corporation"), None),
            program("Google Chrome", Some("Google LLC"), None),
            program("Some Tool", Some("Acme"), None),
            // Its maker's folder in Program Files names its AppData folder too.
            program("한컴오피스 2024", Some("Hancom"), Some(r"C:\Program Files\Hnc\Office 2024")),
            program("Notion 4.2.0", Some("Notion Labs, Inc"), None),
            program("GitHub Desktop", Some("GitHub, Inc."), None),
            // A Store app known by its package name only.
            program("Microsoft.MicrosoftPCManager", None, None),
        ];
        // A program that names no folder but whose uninstaller is in one.
        found[3].exes.push(r"C:\Program Files\VendorStuff\bin\uninst.exe".into());
        let leftovers = |found: &[Found]| {
            let owners = Owners::of(found, &p);
            let mut out = Vec::new();
            for (base, area) in &p.bases {
                leftovers_in(&ix, base, *area, &owners, &[], now, &mut out);
            }
            out.sort_by(|a, b| a.path.cmp(&b.path));
            out
        };
        let out = leftovers(&found);
        let got: Vec<(&str, &str)> = out.iter().map(|l| (l.path.as_str(), l.risk.as_str())).collect();
        assert_eq!(
            got,
            [
                (r"C:\Program Files\Fresh App", "danger"),
                (r"C:\Program Files\NVIDIA Corporation", "danger"),
                (r"C:\Program Files\Old App", "caution"),
                (r"C:\ProgramData\Gone Tool", "caution"),
                (r"C:\Users\me\AppData\Local\net.golbin.hop", "caution"),
            ]
        );
        assert!(out[4].reason.contains("설정·캐시·저장 파일"), "{}", out[4].reason);
        assert_eq!(folder_aliases("com.github.githubapp"), ["github", "githubapp", "comgithub"]);
        assert_eq!(folder_aliases("PC Manager Store"), ["pcmanager"]);
        assert_eq!(folder_aliases("DBeaverData"), ["dbeaver"]);
        assert_eq!(folder_aliases("NVIDIA Corporation"), ["nvidia"]);
        assert_eq!(folder_aliases("notion-updater"), ["notion"]);
        assert!(folder_aliases("Old App").is_empty());
        assert_eq!((out[2].size, out[2].modified), (big, old));
        assert_eq!(out[0].modified, now - DAY);
        assert!(out[0].reason.contains("30일"), "{}", out[0].reason);
        assert!(out[1].reason.contains("드라이버"), "{}", out[1].reason);
        assert!(out[3].reason.contains("모든 사용자용"), "{}", out[3].reason);
        // Once a program of NVIDIA's is installed, its folder is theirs.
        found.push(program("NVIDIA 그래픽 드라이버 560.94", Some("NVIDIA Corporation"), None));
        assert!(leftovers(&found).iter().all(|l| !l.path.contains("NVIDIA")));
        // This app's own folders never.
        let owners = Owners::of(&found, &p);
        let mut out = Vec::new();
        leftovers_in(&ix, r"C:\Program Files", Area::ProgramFiles, &owners, &[r"c:\program files\old app".into()], now, &mut out);
        assert!(out.iter().all(|l| l.path != r"C:\Program Files\Old App"));

        assert!(names_match("Code", "Microsoft Visual Studio Code"));
        assert!(names_match("Google", "Google LLC"));
        assert!(names_match("Python312", "Python 3.12.1 (64-bit)"));
        assert!(names_match("EpicGamesLauncher", "Epic Games Launcher"));
        assert!(names_match("nodejs", "Node.js"));
        assert!(!names_match("Old App", "7-Zip 23.01 (x64)"));
        assert!(!names_match("R", "Rust"));
        assert!(is_kept("Waves") && is_kept("DTS, Inc") && is_kept("Rivet Networks") && is_kept("Synaptics"));
        assert!(!is_kept("Wavesurfer") && !is_kept("Elan Tool"));
    }

    #[test]
    fn store_lists_are_read_from_json() {
        let many = r#"[{"Name":"SpotifyAB.SpotifyMusic","PackageFullName":"SpotifyAB.SpotifyMusic_1.230.1135.0_x64__zpdnekdrzrea0","InstallLocation":"C:\\Program Files\\WindowsApps\\SpotifyAB.SpotifyMusic_1.230.1135.0_x64__zpdnekdrzrea0","Version":"1.230.1135.0","Publisher":"CN=5F5A26B8-1FF4-4B5D-A6D5-2DF8D3B7B6A3","NonRemovable":false},{"Name":"Microsoft.Bad","PackageFullName":"bad'; Remove-Item C:\\","InstallLocation":null}]"#;
        let apps = parse_appx(many);
        assert_eq!(apps.len(), 1);
        let a = &apps[0];
        assert_eq!((a.name.as_str(), a.version.as_deref(), a.non_removable), ("SpotifyAB.SpotifyMusic", Some("1.230.1135.0"), false));
        assert_eq!(a.location.as_deref(), Some(r"C:\Program Files\WindowsApps\SpotifyAB.SpotifyMusic_1.230.1135.0_x64__zpdnekdrzrea0"));
        assert_eq!(publisher_name(a.publisher.as_deref().unwrap()), None);
        assert_eq!(family_of(&a.name, &a.full_name).as_deref(), Some("spotifyab.spotifymusic_zpdnekdrzrea0"));
        // One package is an object, not an array; a BOM may come first; versions may be objects.
        let one = "\u{feff}{\"Name\":\"A.B\",\"PackageFullName\":\"A.B_1.0.0.0_neutral__abc\",\"Version\":{\"Major\":1,\"Minor\":2,\"Build\":3,\"Revision\":4},\"NonRemovable\":true}";
        let apps = parse_appx(one);
        assert_eq!((apps.len(), apps[0].version.as_deref(), apps[0].non_removable, apps[0].location.as_deref()), (1, Some("1.2.3.4"), true, None));
        assert!(parse_appx("").is_empty() && parse_appx("not json").is_empty() && parse_appx("42").is_empty());
        assert_eq!(publisher_name("CN=Microsoft Corporation, O=Microsoft Corporation, L=Redmond, S=Washington, C=US").as_deref(), Some("Microsoft Corporation"));
        assert_eq!(publisher_name(r#"CN=Foo, O="Foo, Inc.", C=US"#).as_deref(), Some("Foo, Inc."));
        assert_eq!(publisher_name("CN=Someone").as_deref(), Some("Someone"));
        assert!(is_package_name("Microsoft.WindowsCalculator_11.2307.4.0_x64__8wekyb3d8bbwe"));
        assert!(!is_package_name("a'b_c") && !is_package_name("nounderscore") && !is_package_name(""));
        let xml = "<Package><Properties><DisplayName>Tom &amp; Jerry</DisplayName><PublisherDisplayName>X</PublisherDisplayName></Properties></Package>";
        assert_eq!(manifest_name(xml).as_deref(), Some("Tom & Jerry"));
        assert_eq!(manifest_name("<Properties><DisplayName>ms-resource:AppName</DisplayName></Properties>"), None);
        assert_eq!(manifest_name("<DisplayName>Outside</DisplayName>"), None);
    }

    #[test]
    fn registry_ids_name_their_hive() {
        for h in Hive::ALL {
            assert_eq!(Hive::of_tag(h.tag()), Some(h));
        }
        assert_eq!(Hive::of_tag("HKLM"), None);
        assert!(uninstall(r"HKXX\Foo").is_err());
        assert_eq!(uninstall(r"HKLM64\a\b").unwrap_err(), BAD_ID);
        assert_eq!(uninstall(r"HKCU\ff-test-no-such-program-key").unwrap_err(), NOT_FOUND);
        assert_eq!(uninstall("bad name").unwrap_err(), BAD_ID);
        assert_eq!(expand_env("%FF_TEST_NO_SUCH_VAR%\\x"), "%FF_TEST_NO_SUCH_VAR%\\x");
        assert!(!expand_env(r"%SystemRoot%\System32").contains('%'));
        assert_eq!(utf16_text(&[0x41, 0, 0x42, 0, 0, 0, 0x43, 0]), "AB");
    }
}
