//! The places in the left pane, as in Windows Explorer's navigation pane: the
//! user's own folders (홈, 바탕 화면, 다운로드, ... and OneDrive), the folders
//! pinned in Explorer's Quick Access (to bring over once), and the user's own
//! shortcuts, kept in `places.json`.
//!
//! Known folders come from Windows itself (SHGetKnownFolderPath), so a folder
//! moved elsewhere or backed up by OneDrive is found where it is now, and
//! their names are the ones Explorer shows (the shell's display name).

use crate::ai::{known_folder, normalize_abs};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use windows::core::{w, Interface, PCWSTR, PWSTR};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
use windows::Win32::UI::Shell::PropertiesSystem::PSGetPropertyKeyFromName;
use windows::Win32::UI::Shell::{
    BHID_EnumItems, IEnumShellItems, IShellItem, IShellItem2, SHCreateItemFromParsingName, SIGDN, SIGDN_FILESYSPATH,
    SIGDN_NORMALDISPLAY,
};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};
use windows_sys::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Music, FOLDERID_Pictures, FOLDERID_Profile,
    FOLDERID_Videos,
};

/// Shortcuts a user can make, at most.
const MAX_PINS: usize = 100;
/// A shortcut's own name, at most (in characters).
const MAX_LABEL: usize = 120;
/// Hidden places and folded sections kept, at most.
const MAX_IDS: usize = 64;
/// Items read from Explorer's Quick Access, at most.
const MAX_SHELL_ITEMS: usize = 1000;

/// Quick Access's folders (pinned and frequent ones): the folder part of
/// Explorer's Home, much quicker to read than Home, which also lists recent
/// files. Home itself is the fallback.
const FREQUENT_PLACES: &str = "shell:::{3936E9E4-D92C-4EEE-A85A-BC16D5EA0819}";
const HOME: &str = "shell:::{679f85cb-0220-4080-b29b-5540cc05aab6}";

const ONEDRIVE_ACCOUNTS: &str = r"Software\Microsoft\OneDrive\Accounts";
/// Work or school accounts are `Business1`, `Business2`, ... in the registry.
const MAX_BUSINESS_ACCOUNTS: u32 = 9;

/// The user's own folders, in Explorer's order, after Home and the OneDrive
/// accounts. The name is used when the shell gives none.
const KNOWN: [(&str, windows_sys::core::GUID, &str); 6] = [
    ("desktop", FOLDERID_Desktop, "바탕 화면"),
    ("downloads", FOLDERID_Downloads, "다운로드"),
    ("documents", FOLDERID_Documents, "문서"),
    ("pictures", FOLDERID_Pictures, "사진"),
    ("music", FOLDERID_Music, "음악"),
    ("videos", FOLDERID_Videos, "동영상"),
];

/// A folder Windows knows about: the user's profile, Desktop, ..., OneDrive.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Place {
    /// What the settings call it (`hidden`): the kind, but for the second and
    /// later work or school OneDrive accounts (`onedrive_business_2`, ...).
    pub id: String,
    pub label: String,
    pub path: String,
    /// "home" | "desktop" | "downloads" | "documents" | "pictures" | "music" |
    /// "videos" | "onedrive" | "onedrive_business"
    pub kind: &'static str,
}

/// A shortcut the user made. Without a label it shows the folder's name.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Pin {
    pub path: String,
    pub label: Option<String>,
}

/// What `places.json` holds.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct PlacesConfig {
    pub pinned: Vec<Pin>,
    /// Ids of the known places the user hid.
    pub hidden: Vec<String>,
    /// Ids of the folded sections ("quick", "drives", "tools").
    pub collapsed: Vec<String>,
    /// Whether Explorer's pinned folders were brought over already (once).
    pub imported_explorer: bool,
}

impl PlacesConfig {
    /// The config as it can be kept: shortcuts with an absolute path (a
    /// drive letter), each folder once (case does not matter), at most
    /// MAX_PINS; names trimmed; ids well formed and each once. What does not
    /// fit is dropped, so a file edited by hand never stops the app.
    fn tidy(self) -> Self {
        let mut seen = HashSet::new();
        let pinned = self
            .pinned
            .into_iter()
            .filter_map(|p| Some(Pin { path: normalize_abs(p.path.trim())?, label: clean_label(p.label) }))
            .filter(|p| seen.insert(p.path.to_lowercase()))
            .take(MAX_PINS)
            .collect();
        PlacesConfig { pinned, hidden: clean_ids(self.hidden), collapsed: clean_ids(self.collapsed), ..self }
    }

    /// The config the window sent, tidied; refused when a shortcut's path
    /// is not absolute or there are more than MAX_PINS of them.
    fn checked(self) -> Result<Self, String> {
        if let Some(bad) = self.pinned.iter().find(|p| normalize_abs(p.path.trim()).is_none()) {
            return Err(format!("바로 가기 경로가 올바르지 않아요. 드라이브 문자부터 쓴 전체 경로여야 해요 ({})", bad.path));
        }
        let distinct = self.pinned.iter().filter_map(|p| normalize_abs(p.path.trim())).map(|p| p.to_lowercase()).collect::<HashSet<_>>();
        if distinct.len() > MAX_PINS {
            return Err(format!("바로 가기는 {MAX_PINS}개까지 만들 수 있어요"));
        }
        Ok(self.tidy())
    }
}

fn clean_label(label: Option<String>) -> Option<String> {
    let label = label?;
    let s: String = label.trim().chars().filter(|c| !c.is_control()).take(MAX_LABEL).collect();
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// `onedrive_business_2`, `quick`: lower-case letters, digits and `_`.
fn id_ok(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn clean_ids(ids: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    ids.into_iter().filter(|id| id_ok(id) && seen.insert(id.clone())).take(MAX_IDS).collect()
}

// ---- places.json -----------------------------------------------------------------

fn config_path() -> PathBuf {
    crate::scan::data_dir().join("places.json")
}

/// The saved config; the defaults when there is none. A file that cannot be
/// read as one is kept aside (`places.json.bad`) and the defaults used.
fn load_from(path: &Path) -> Result<PlacesConfig, String> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(PlacesConfig::default()),
        Err(e) => return Err(format!("바로 가기 설정을 읽지 못했어요: {e}")),
    };
    // Notepad may have saved it with a BOM.
    let json = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    match serde_json::from_slice::<PlacesConfig>(json) {
        Ok(c) => Ok(c.tidy()),
        Err(_) => {
            let _ = fs::rename(path, path.with_extension("json.bad"));
            Ok(PlacesConfig::default())
        }
    }
}

/// Written aside first and then put in place, so a crash midway never
/// leaves half a file. The temp name is this process's own, so two windows
/// saving at once do not write into each other's.
fn save_to(path: &Path, config: &PlacesConfig) -> io::Result<()> {
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("json.{}-{seq}.tmp", std::process::id()));
    let written = (|| {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(config)?)?;
        f.sync_all()
    })();
    written.and_then(|()| fs::rename(&tmp, path)).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

// ---- COM and the shell -----------------------------------------------------------

/// COM for the calling thread, as a single-threaded apartment (what the
/// shell expects). Uninitialized when dropped, only if this initialized it
/// (a thread already in the multithreaded apartment is left as it is).
struct Com(bool);

impl Com {
    fn enter() -> Self {
        Com(unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }.is_ok())
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

/// A string the shell allocated, freed here.
fn take_string(p: PWSTR) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let s = unsafe { p.to_string() }.ok();
    unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn shell_item(parsing_name: &str) -> Option<IShellItem> {
    let name = crate::win::wide(parsing_name);
    unsafe { SHCreateItemFromParsingName(PCWSTR(name.as_ptr()), None) }.ok()
}

fn item_name(item: &IShellItem, sigdn: SIGDN) -> Option<String> {
    take_string(unsafe { item.GetDisplayName(sigdn) }.ok()?)
}

/// The name Explorer shows for a folder ("바탕 화면" for Desktop, "OneDrive -
/// Personal", the user's name for the profile folder).
fn display_name(path: &str) -> Option<String> {
    item_name(&shell_item(path)?, SIGDN_NORMALDISPLAY)
}

/// A string value under HKEY_CURRENT_USER.
fn reg_string(subkey: &str, name: &str) -> Option<String> {
    let (key, value) = (crate::win::wide(subkey), crate::win::wide(name));
    let read = |buf: *mut u16, size: &mut u32| unsafe {
        RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr(), RRF_RT_REG_SZ, null_mut(), buf.cast(), size)
    };
    let mut size = 0u32;
    if read(null_mut(), &mut size) != 0 || size == 0 {
        return None;
    }
    let mut buf = vec![0u16; size as usize / 2 + 1];
    let mut size = (buf.len() * 2) as u32;
    if read(buf.as_mut_ptr(), &mut size) != 0 {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]).trim().to_string()).filter(|s| !s.is_empty())
}

fn env_path(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

// ---- known places ----------------------------------------------------------------

/// A work or school OneDrive account: its folder, its registry number
/// (`Business<n>`) and the organization's name, when the registry has them.
struct Business {
    path: String,
    number: u32,
    org: Option<String>,
}

fn onedrive_personal() -> Option<String> {
    env_path("OneDriveConsumer").or_else(|| reg_string(&format!(r"{ONEDRIVE_ACCOUNTS}\Personal"), "UserFolder"))
}

fn onedrive_business() -> Vec<Business> {
    let mut found: Vec<Business> = (1..=MAX_BUSINESS_ACCOUNTS)
        .filter_map(|n| {
            let key = format!(r"{ONEDRIVE_ACCOUNTS}\Business{n}");
            Some(Business { path: reg_string(&key, "UserFolder")?, number: n, org: reg_string(&key, "DisplayName") })
        })
        .collect();
    // The first account's folder, also when the registry does not say.
    if let Some(path) = env_path("OneDriveCommercial")
        && !found.iter().any(|b| b.path.eq_ignore_ascii_case(&path))
        && !found.iter().any(|b| b.number == 1)
    {
        found.insert(0, Business { path, number: 1, org: None });
    }
    found
}

fn folder_name(path: &str) -> Option<String> {
    Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).filter(|n| !n.is_empty())
}

/// The folders Windows knows about that are there now, in Explorer's order:
/// the profile folder (Home), OneDrive (personal, then work or school), then
/// Desktop, Downloads, Documents, Pictures, Music and Videos. Each folder
/// once; only drive-letter paths. Call with COM entered.
fn known() -> Vec<Place> {
    let mut found: Vec<(String, &'static str, Option<String>, String)> = Vec::new();
    if let Some(p) = known_folder(&FOLDERID_Profile) {
        found.push(("home".into(), "home", Some(p), "홈".into()));
    }
    found.push(("onedrive".into(), "onedrive", onedrive_personal(), "OneDrive - Personal".into()));
    for b in onedrive_business() {
        let id = if b.number == 1 { "onedrive_business".to_string() } else { format!("onedrive_business_{}", b.number) };
        let fallback = match (&b.org, folder_name(&b.path)) {
            (Some(org), _) => format!("OneDrive - {org}"),
            (None, Some(name)) => name,
            (None, None) => "OneDrive - 회사 또는 학교".into(),
        };
        found.push((id, "onedrive_business", Some(b.path), fallback));
    }
    for (kind, folder, fallback) in KNOWN {
        found.push((kind.into(), kind, known_folder(&folder), fallback.into()));
    }
    let mut seen = HashSet::new();
    found
        .into_iter()
        .filter_map(|(id, kind, path, fallback)| {
            let path = known_path(path?.trim())?;
            if !Path::new(&path).is_dir() || !seen.insert(path.to_lowercase()) {
                return None;
            }
            let label = display_name(&path).unwrap_or(fallback);
            Some(Place { id, label, path, kind })
        })
        .collect()
}

// ---- Explorer's Quick Access -----------------------------------------------------

/// The key of `System.Home.IsPinned`, on Windows versions that have it.
fn pinned_key() -> Option<PROPERTYKEY> {
    let mut key = PROPERTYKEY::default();
    unsafe { PSGetPropertyKeyFromName(w!("System.Home.IsPinned"), &mut key) }.ok().map(|()| key)
}

/// The folders pinned in Explorer's Quick Access (Home), in Explorer's
/// order: those it marks pinned, or, on a Windows that does not say which
/// are, every folder there. Only folders on a drive letter that are there
/// now, each once. Call with COM entered.
fn explorer_pinned() -> Result<Vec<String>, String> {
    let folder = [FREQUENT_PLACES, HOME]
        .iter()
        .find_map(|ns| shell_item(ns))
        .ok_or("탐색기의 빠른 액세스를 열지 못했어요")?;
    let items: IEnumShellItems =
        unsafe { folder.BindToHandler(None, &BHID_EnumItems) }.map_err(|e| format!("탐색기의 빠른 액세스 목록을 읽지 못했어요: {e}"))?;
    let key = pinned_key();
    let mut listed: Vec<(String, Option<bool>)> = Vec::new();
    for _ in 0..MAX_SHELL_ITEMS {
        let mut one = [None];
        let mut fetched = 0u32;
        if unsafe { items.Next(&mut one, Some(&mut fetched)) }.is_err() || fetched == 0 {
            break;
        }
        let Some(item) = one[0].take() else { break };
        // Libraries, This PC and the like have no file-system path.
        let Some(path) = item_name(&item, SIGDN_FILESYSPATH) else { continue };
        let pinned = key.as_ref().and_then(|k| {
            let item = item.cast::<IShellItem2>().ok()?;
            unsafe { item.GetBool(k) }.ok().map(|b| b.as_bool())
        });
        listed.push((path, pinned));
    }
    let marked = listed.iter().any(|(_, p)| p.is_some());
    let mut seen = HashSet::new();
    Ok(listed
        .into_iter()
        .filter(|(_, pinned)| !marked || *pinned == Some(true))
        .filter_map(|(path, _)| normalize_abs(&path))
        .filter(|p| Path::new(p).is_dir() && seen.insert(p.to_lowercase()))
        .collect())
}

// ---- commands --------------------------------------------------------------------

/// The user's own folders for the left pane (see `known`).
#[tauri::command]
pub async fn known_places() -> Result<Vec<Place>, String> {
    crate::blocking(|| {
        let _com = Com::enter();
        known()
    })
    .await
}

/// The folders pinned in Explorer's Quick Access (see `explorer_pinned`).
#[tauri::command]
pub async fn explorer_pins() -> Result<Vec<String>, String> {
    crate::blocking(|| {
        let _com = Com::enter();
        explorer_pinned()
    })
    .await?
}

#[tauri::command(async)]
pub fn places_get() -> Result<PlacesConfig, String> {
    load_from(&config_path())
}

#[tauri::command(async)]
pub fn places_set(config: PlacesConfig) -> Result<(), String> {
    let config = config.checked()?;
    save_to(&config_path(), &config).map_err(|e| format!("바로 가기 설정을 저장하지 못했어요: {e}"))
}

/// What `path` is: "dir", "file" or "gone", from the item itself (a link counts as what it is,
/// never followed; a folder this account cannot open is still a folder). For dropped items: one
/// look, where listing a folder could read thousands of entries first.
#[tauri::command(async)]
pub fn path_kind(path: String) -> &'static str {
    kind_of(Path::new(&path))
}

fn kind_of(path: &Path) -> &'static str {
    use std::os::windows::fs::MetadataExt;
    match fs::symlink_metadata(path) {
        // From the attribute: std's is_dir() is false for junctions and folder symlinks.
        Ok(m) => if m.file_attributes() & 0x10 != 0 { "dir" } else { "file" },
        Err(e) if e.kind() == io::ErrorKind::NotFound => "gone",
        // Denied and the like: it is there; folders are what gets dropped here.
        Err(_) => "dir",
    }
}

/// A known folder's path: a drive-letter path as `normalize_abs` makes it, or a network folder
/// (`\\server\share\...`), where a company's policy may redirect Desktop or Documents. Only the
/// ids of known places are saved, so `places_set`'s drive-letter rule never sees these.
fn known_path(raw: &str) -> Option<String> {
    if let Some(p) = normalize_abs(raw) {
        return Some(p);
    }
    let rest = raw.strip_prefix(r"\\")?;
    // \\?\ and \\.\ are device and verbatim paths, not shares.
    if rest.starts_with(['?', '.']) {
        return None;
    }
    let parts: Vec<&str> = rest.split(['\\', '/']).filter(|s| !s.is_empty()).collect();
    let bad = |s: &&str| *s == "." || *s == ".." || s.chars().any(|c| c.is_control() || "*?\"<>|".contains(c));
    if parts.len() < 2 || parts.iter().any(bad) {
        return None;
    }
    Some(format!(r"\\{}", parts.join(r"\")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_known_folders_are_kept() {
        assert_eq!(known_path(r"\\fs01\home$\me\Desktop").as_deref(), Some(r"\\fs01\home$\me\Desktop"));
        assert_eq!(known_path(r"\\fs01\share\").as_deref(), Some(r"\\fs01\share"));
        assert_eq!(known_path(r"\\fs01"), None);
        assert_eq!(known_path(r"\\?\C:\Users"), None);
        assert_eq!(known_path(r"\\.\pipe\x"), None);
        assert_eq!(known_path(r"\\fs01\share\..\x"), None);
        assert!(known_path(r"C:\Users").is_some());
    }

    #[test]
    fn path_kinds() {
        let dir = temp_dir("kind");
        fs::write(dir.join("a.txt"), "x").unwrap();
        assert_eq!(kind_of(&dir), "dir");
        assert_eq!(kind_of(&dir.join("a.txt")), "file");
        assert_eq!(kind_of(&dir.join("missing")), "gone");
        fs::remove_dir_all(&dir).unwrap();
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ff-places-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn pin(path: &str, label: Option<&str>) -> Pin {
        Pin { path: path.into(), label: label.map(Into::into) }
    }

    #[test]
    fn a_config_is_tidied_and_comes_back_as_saved() {
        let dir = temp_dir("round");
        let path = dir.join("places.json");
        let config = PlacesConfig {
            pinned: vec![
                pin("d:/mbc_데이터분석과목/", Some("  수업 자료 ")),
                pin(r"D:\MBC_데이터분석과목", Some("again")),
                pin(r"C:\Users\me\Downloads", Some("   ")),
                pin(r"G:\", Some("드\u{7}라이브")),
                pin(r"relative\folder", None),
                pin(r"\\server\share", None),
            ],
            hidden: vec!["onedrive".into(), "onedrive".into(), "Bad Id!".into(), "onedrive_business_2".into()],
            collapsed: vec!["tools".into(), "".into()],
            imported_explorer: true,
        };
        let tidy = config.tidy();
        assert_eq!(
            tidy.pinned,
            vec![pin(r"D:\mbc_데이터분석과목", Some("수업 자료")), pin(r"C:\Users\me\Downloads", None), pin(r"G:\", Some("드라이브"))]
        );
        assert_eq!(tidy.hidden, vec!["onedrive", "onedrive_business_2"]);
        assert_eq!(tidy.collapsed, vec!["tools"]);
        save_to(&path, &tidy).unwrap();
        assert_eq!(load_from(&path).unwrap(), tidy);
        // Saved again over it, with no temp file left behind.
        let none = PlacesConfig::default();
        save_to(&path, &none).unwrap();
        assert_eq!(load_from(&path).unwrap(), none);
        let left: Vec<_> = fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left, vec![std::ffi::OsString::from("places.json")]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_window_cannot_save_relative_paths_or_too_many_shortcuts() {
        let one = |p: &str| PlacesConfig { pinned: vec![pin(p, None)], ..Default::default() };
        assert!(one(r"folder\inside").checked().is_err());
        assert!(one(r"\\server\share\x").checked().is_err());
        assert!(one("").checked().is_err());
        assert!(one(r"C:").checked().is_ok());
        let many = |n: usize| PlacesConfig { pinned: (0..n).map(|i| pin(&format!(r"C:\f{i}"), None)).collect(), ..Default::default() };
        assert_eq!(many(MAX_PINS).checked().unwrap().pinned.len(), MAX_PINS);
        assert!(many(MAX_PINS + 1).checked().is_err());
        // Duplicates do not count toward the limit; they are dropped.
        let mut dup = many(MAX_PINS);
        dup.pinned.push(pin(r"c:\F0\", None));
        assert_eq!(dup.checked().unwrap().pinned.len(), MAX_PINS);
    }

    #[test]
    fn what_the_window_sends_reads_with_nulls_and_missing_fields() {
        let c: PlacesConfig = serde_json::from_str(r#"{"pinned":[{"path":"D:\\x","label":null}]}"#).unwrap();
        assert_eq!(c, PlacesConfig { pinned: vec![pin(r"D:\x", None)], ..Default::default() });
        let back = serde_json::to_value(&c).unwrap();
        assert_eq!(back, serde_json::json!({"pinned":[{"path":"D:\\x","label":null}],"hidden":[],"collapsed":[],"imported_explorer":false}));
    }

    #[test]
    fn no_file_is_the_defaults_and_a_broken_one_is_kept_aside() {
        let dir = temp_dir("broken");
        let path = dir.join("places.json");
        assert_eq!(load_from(&path).unwrap(), PlacesConfig::default());
        fs::write(&path, b"{ not json").unwrap();
        assert_eq!(load_from(&path).unwrap(), PlacesConfig::default());
        assert!(!path.exists());
        assert_eq!(fs::read(dir.join("places.json.bad")).unwrap(), b"{ not json");
        // With a BOM, as Notepad may save it.
        fs::write(&path, b"\xEF\xBB\xBF{\"imported_explorer\":true}").unwrap();
        assert!(load_from(&path).unwrap().imported_explorer);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn known_places_are_folders_that_are_there() {
        let places = {
            let _com = Com::enter();
            known()
        };
        if places.is_empty() {
            eprintln!("no known folders here (a service account?): skipped");
            return;
        }
        let kinds = ["home", "desktop", "downloads", "documents", "pictures", "music", "videos", "onedrive", "onedrive_business"];
        let mut ids = HashSet::new();
        for p in &places {
            assert!(kinds.contains(&p.kind), "{p:?}");
            assert!(id_ok(&p.id) && ids.insert(p.id.clone()), "{p:?}");
            assert_eq!(normalize_abs(&p.path).as_deref(), Some(p.path.as_str()), "{p:?}");
            assert!(Path::new(&p.path).is_dir(), "{p:?}");
            assert!(!p.label.trim().is_empty(), "{p:?}");
        }
    }

    #[test]
    fn explorer_pins_are_read_without_panicking() {
        let _com = Com::enter();
        match explorer_pinned() {
            Ok(pins) => {
                let mut seen = HashSet::new();
                for p in &pins {
                    assert!(Path::new(p).is_dir() && seen.insert(p.to_lowercase()), "{p}");
                }
            }
            Err(e) => {
                eprintln!("Quick Access not readable here: {e}");
                assert!(!e.is_empty());
            }
        }
    }
}
