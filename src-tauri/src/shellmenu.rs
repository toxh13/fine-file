//! The Windows Explorer context menu (IContextMenu) for selected items, shown
//! at the cursor over the app window, with the items other programs add
//! (7-Zip, VS Code, "Send to", ...). The chosen command runs as in Explorer.
//!
//! The menu is built and shown on the window's UI thread (a single-threaded
//! COM apartment, as shell menus need), and while it is open the window's
//! messages go through the menu first, so submenus that fill themselves when
//! opened ("Send to", "Open with") and owner-drawn items (icons) work.
//!
//! Three of Explorer's own commands are taken out of the menu (`LEFT_OUT`).
//! 삭제 ("delete") is Explorer's delete, not this app's move to the Recycle
//! Bin (lib.rs `trash_paths`): where Windows cannot recycle (no bin, bin
//! switched off, item bigger than the bin) it deletes for good after its own
//! prompt, and whether Shift held while choosing it does the same is up to
//! shell32, which may read the keyboard itself whatever the invoke flags say.
//! 휴지통 비우기 ("empty", on an account's bin, which the app lists as a
//! folder in `C:\$Recycle.Bin`) deletes for good, which this app never does.
//! 이름 바꾸기 ("rename") edits the name in place in Explorer's view, which
//! this window does not have, so it would do nothing. Without CMF_CANRENAME
//! the shell does not offer it, and should a handler add it anyway it goes
//! too. The app's own menu has 휴지통으로 이동 and 이름 바꾸기. Shift and
//! Ctrl are still never passed on to a command (CMIC_MASK_SHIFT_DOWN /
//! CONTROL_DOWN). Other programs' commands (and the shell's other verbs) stay
//! as they are and follow their own rules, which may delete too.

use crate::win;
use std::collections::HashSet;
use std::ffi::c_void;
use std::marker::PhantomData;
use std::os::windows::ffi::OsStrExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Component, Path, PathBuf, Prefix};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::async_runtime::channel;
use windows::core::{Interface, PCSTR, PCWSTR, PSTR};
use windows::Win32::Foundation::{CO_E_NOTINITIALIZED, ERROR_CANCELLED, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::Com::{
    CoGetApartmentType, CoInitializeEx, APTTYPE, APTTYPEQUALIFIER, APTTYPEQUALIFIER_IMPLICIT_MTA, APTTYPE_MAINSTA,
    APTTYPE_MTA, APTTYPE_STA, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, GetKeyState, VK_SHIFT};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    DefSubclassProc, IContextMenu, IContextMenu2, IContextMenu3, ILFindLastID, ILFree, IShellFolder,
    RemoveWindowSubclass, SHBindToParent, SHParseDisplayName, SetWindowSubclass, CMF_EXPLORE, CMF_EXTENDEDVERBS,
    CMF_NORMAL, CMIC_MASK_PTINVOKE, CMINVOKECOMMANDINFO, CMINVOKECOMMANDINFOEX, GCS_VERBW, SEE_MASK_ASYNCOK,
    SEE_MASK_UNICODE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, DeleteMenu, DestroyMenu, GetMenuItemCount, GetMenuItemID, GetMenuItemInfoW, TrackPopupMenuEx,
    HMENU, MENUITEMINFOW, MFT_SEPARATOR, MF_BYPOSITION, MIIM_FTYPE, SW_SHOWNORMAL, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    WM_DRAWITEM, WM_INITMENUPOPUP, WM_MEASUREITEM, WM_MENUCHAR,
};

/// The menu's command ids: TrackPopupMenuEx returns 0 for "nothing chosen".
const ID_FIRST: u32 = 1;
const ID_LAST: u32 = 0x7FFF;
/// Names this module's window subclass among the others on the window (tao has its own).
const SUBCLASS_ID: usize = 0x6666_6d65;
/// Same values as in shobjidl.h, which defines them through SEE_MASK_*.
const CMIC_MASK_UNICODE: u32 = SEE_MASK_UNICODE;
const CMIC_MASK_ASYNCOK: u32 = SEE_MASK_ASYNCOK;

const NO_ITEMS: &str = "고른 항목이 없어요";
const NOT_FULL: &str = "전체 경로가 아니에요";
const MIXED: &str = "같은 폴더 안의 항목에만 탐색기 메뉴를 띄울 수 있어요";
const ODD_NAME: &str =
    "이름이 점(.)이나 공백으로 끝나는 항목은 탐색기가 다른 항목으로 알아들을 수 있어서 메뉴를 띄우지 않았어요";
const NOT_FOUND: &str = "탐색기에서 이 항목을 찾지 못했어요";
const NO_COM: &str = "이 창에서는 탐색기 메뉴를 띄울 수 없어요 (COM을 준비하지 못했어요)";
const NO_MENU: &str = "탐색기 메뉴를 만들지 못했어요";
const NOT_RUN: &str = "메뉴의 명령을 실행하지 못했어요";
const NO_WINDOW: &str = "창을 찾지 못해서 탐색기 메뉴를 띄우지 못했어요";
const FAILED: &str = "탐색기 메뉴를 띄우지 못했어요";

/// Shows the shell menu for `paths` (all in the same folder) at screen point
/// (x, y) in physical pixels. Returns true when a command was chosen and run.
#[tauri::command]
pub async fn shell_menu(window: tauri::WebviewWindow, paths: Vec<String>, x: i32, y: i32) -> Result<bool, String> {
    let items = check_paths(&paths)?;
    // Here, not on the UI thread: Tauri gets the handle from there and waits for it.
    let owner = window.hwnd().map_err(|e| format!("{NO_WINDOW}: {e}"))?.0 as usize;
    let (tx, mut rx) = channel(1);
    window
        .run_on_main_thread(move || {
            // A panic must not unwind into the event loop.
            let shown = catch_unwind(AssertUnwindSafe(|| show(HWND(owner as *mut c_void), &items, x, y)));
            let _ = tx.try_send(shown.unwrap_or_else(|_| Err(FAILED.into())));
        })
        .map_err(|e| format!("{FAILED}: {e}"))?;
    // No answer: the event loop is gone (the app is closing).
    rx.recv().await.unwrap_or_else(|| Err(FAILED.into()))
}

/// The folder the shell holds an item in, as `check_paths` compares them.
#[derive(PartialEq)]
enum Folder {
    /// A drive root's: This PC.
    ThisPc,
    /// A network share's (`\\server\share`): the server, by its lowercase name.
    Server(String),
    /// Anything else's: its parent folder, lowercase.
    Path(String),
}

impl Folder {
    fn of(path: &Path) -> Folder {
        if let Some(parent) = path.parent() {
            return Folder::Path(parent.to_string_lossy().to_lowercase());
        }
        match path.components().next() {
            Some(Component::Prefix(p)) => match p.kind() {
                Prefix::UNC(server, _) => Folder::Server(server.to_string_lossy().to_lowercase()),
                _ => Folder::ThisPc,
            },
            _ => Folder::ThisPc,
        }
    }
}

/// The items as the shell should get them: full drive or UNC paths, `\` as
/// the separator, no `\` at the end, each item once (Windows names ignore
/// case). They must all be in one folder: drive roots count as one folder
/// (This PC), and network shares as one per server. `.` and `..` are refused
/// rather than resolved, and so are names ending in a dot or a space, which
/// the shell would take for another item.
fn check_paths(paths: &[String]) -> Result<Vec<PathBuf>, String> {
    if paths.is_empty() {
        return Err(NO_ITEMS.into());
    }
    let mut items = Vec::with_capacity(paths.len());
    let mut folder: Option<Folder> = None;
    let mut seen = HashSet::new();
    for p in paths {
        let path: PathBuf = Path::new(&p.replace('/', "\\")).components().collect();
        let mut parts = path.components();
        let drive_or_unc = matches!(
            parts.next(),
            Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::UNC(..))
        );
        let rooted = matches!(parts.next(), Some(Component::RootDir));
        if !drive_or_unc || !rooted || !parts.all(|c| matches!(c, Component::Normal(_))) {
            return Err(format!("{NOT_FULL}: {p}"));
        }
        if !win::shell_can_name(&path) {
            return Err(format!("{ODD_NAME}: {p}"));
        }
        // build() binds to the first item's folder and names every item in it.
        let parent = Folder::of(&path);
        match &folder {
            None => folder = Some(parent),
            Some(f) if *f == parent => {}
            Some(_) => return Err(MIXED.into()),
        }
        if seen.insert(path.to_string_lossy().to_lowercase()) {
            items.push(path);
        }
    }
    Ok(items)
}

/// Builds the menu, shows it at (x, y) and runs the chosen command. Runs on
/// the UI thread of `owner`.
fn show(owner: HWND, items: &[PathBuf], x: i32, y: i32) -> Result<bool, String> {
    // The menu loop below is modal, so a second menu cannot really start meanwhile; should one
    // ever try, it shows nothing rather than share the window subclass with the first.
    if MENU_OPEN.swap(true, Ordering::SeqCst) {
        return Ok(false);
    }
    let _open = MenuOpen;
    // Tauri's UI thread already is a single-threaded apartment (tao and WebView2 set it up).
    // If it ever is not, it becomes one here and stays one: a command may still be running
    // in it after the menu is gone (CMIC_MASK_ASYNCOK), and nothing else on this thread
    // asked for another kind.
    enter_sta()?;
    let mut flags = CMF_NORMAL | CMF_EXPLORE;
    if shift_down() {
        // Shift + right-click, as in Explorer: "Copy as path", "Open PowerShell window here", ...
        flags |= CMF_EXTENDEDVERBS;
    }
    let menu = build(owner, items, flags)?;
    let chosen = {
        let _hook = Subclass::install(owner, &menu);
        let flags = TPM_RETURNCMD | TPM_RIGHTBUTTON;
        unsafe { TrackPopupMenuEx(menu.popup, flags.0, x, y, owner, None) }.0 as u32
    };
    // 0 is "dismissed" (or the menu could not be shown); anything else outside the range is not ours.
    if !(ID_FIRST..=ID_LAST).contains(&chosen) {
        return Ok(false);
    }
    invoke(&menu, owner, chosen - ID_FIRST, items, x, y)
}

static MENU_OPEN: AtomicBool = AtomicBool::new(false);

struct MenuOpen;

impl Drop for MenuOpen {
    fn drop(&mut self) {
        MENU_OPEN.store(false, Ordering::SeqCst);
    }
}

/// COM on the calling thread as a single-threaded apartment, which shell
/// menus need. A thread that already is one is left as it is; one in no
/// apartment (or only in the process's implicit multithreaded one) is made
/// one. A thread that joined another kind is refused. Returns whether this
/// call initialized COM, so that only such a call is ever undone.
fn enter_sta() -> Result<bool, String> {
    let (mut kind, mut qualifier) = (APTTYPE(0), APTTYPEQUALIFIER(0));
    match unsafe { CoGetApartmentType(&mut kind, &mut qualifier) } {
        Ok(()) if kind == APTTYPE_STA || kind == APTTYPE_MAINSTA => return Ok(false),
        Ok(()) if kind == APTTYPE_MTA && qualifier == APTTYPEQUALIFIER_IMPLICIT_MTA => {}
        Err(e) if e.code() == CO_E_NOTINITIALIZED => {}
        _ => return Err(NO_COM.into()),
    }
    if unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }.is_ok() {
        Ok(true)
    } else {
        Err(NO_COM.into())
    }
}

/// Whether Shift is held: as of the right-click that asked for the menu, or now.
fn shift_down() -> bool {
    let key = VK_SHIFT.0 as i32;
    unsafe { GetKeyState(key) < 0 || GetAsyncKeyState(key) < 0 }
}

/// An item ID list from the shell, freed when dropped.
struct Pidl(*mut ITEMIDLIST);

impl Drop for Pidl {
    fn drop(&mut self) {
        unsafe { ILFree(Some(self.0.cast_const())) };
    }
}

/// The shell's context menu for some items of one folder, filled into a
/// popup menu. Dropping it destroys the popup, then releases the shell's
/// interfaces, then frees the item IDs (fields drop in this order).
struct ShellMenu {
    popup: HMENU,
    cm: IContextMenu,
    /// What the menu's messages go to while it is open (the newest the handler has).
    cm2: Option<IContextMenu2>,
    cm3: Option<IContextMenu3>,
    _items: Vec<Pidl>,
}

impl Drop for ShellMenu {
    fn drop(&mut self) {
        let _ = unsafe { DestroyMenu(self.popup) };
    }
}

/// Builds the shell menu for `items` (from check_paths: one folder, at least
/// one item) with QueryContextMenu `flags`, without showing it, and takes
/// out the commands this app leaves out.
fn build(owner: HWND, items: &[PathBuf], flags: u32) -> Result<ShellMenu, String> {
    let menu = fill(owner, items, flags)?;
    leave_out(&menu);
    Ok(menu)
}

/// The shell menu as its handlers fill it.
fn fill(owner: HWND, items: &[PathBuf], flags: u32) -> Result<ShellMenu, String> {
    let mut pidls = Vec::with_capacity(items.len());
    for item in items {
        let name: Vec<u16> = item.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut pidl = null_mut();
        unsafe { SHParseDisplayName(PCWSTR(name.as_ptr()), None, &mut pidl, 0, None) }
            .map_err(|e| format!("{NOT_FOUND}: {} ({e})", item.display()))?;
        pidls.push(Pidl(pidl));
    }
    // All the items are in the first one's folder, so each one's last ID is its name in there.
    let folder: IShellFolder =
        unsafe { SHBindToParent(pidls[0].0, None) }.map_err(|e| format!("{NO_MENU}: {} ({e})", items[0].display()))?;
    let children: Vec<*const ITEMIDLIST> = pidls.iter().map(|p| unsafe { ILFindLastID(p.0) }.cast_const()).collect();
    let cm: IContextMenu =
        unsafe { folder.GetUIObjectOf(owner, &children, None) }.map_err(|e| format!("{NO_MENU} ({e})"))?;
    let popup = unsafe { CreatePopupMenu() }.map_err(|e| format!("{NO_MENU} ({e})"))?;
    let menu = ShellMenu { popup, cm2: cm.cast().ok(), cm3: cm.cast().ok(), cm, _items: pidls };
    unsafe { menu.cm.QueryContextMenu(menu.popup, 0, ID_FIRST, ID_LAST, flags) }
        .ok()
        .map_err(|e| format!("{NO_MENU} ({e})"))?;
    Ok(menu)
}

/// Explorer's commands taken out of the menu, by verb (see the top of this file).
const LEFT_OUT: [&str; 3] = ["delete", "empty", "rename"];

/// The verb of the menu command `offset` places after ID_FIRST ("open",
/// "delete", ...), when its handler names one.
fn verb(cm: &IContextMenu, offset: u32) -> Option<String> {
    let mut name = [0u16; 64];
    // GCS_VERBW writes UTF-16 to what the call types as a byte string, `name.len()` characters at most.
    let wide = PSTR(name.as_mut_ptr().cast());
    unsafe { cm.GetCommandString(offset as usize, GCS_VERBW, None, wide, name.len() as u32) }.ok()?;
    let len = name.iter().position(|&c| c == 0)?;
    Some(String::from_utf16_lossy(&name[..len]))
}

fn left_out(cm: &IContextMenu, id: u32) -> bool {
    (ID_FIRST..=ID_LAST).contains(&id) && verb(cm, id - ID_FIRST).is_some_and(|v| LEFT_OUT.iter().any(|l| v.eq_ignore_ascii_case(l)))
}

/// Takes the LEFT_OUT commands off the top level of the menu, where Explorer
/// puts its own, then any separator that leaves at an end or next to another.
fn leave_out(menu: &ShellMenu) {
    let popup = menu.popup;
    let mut removed = false;
    for pos in (0..unsafe { GetMenuItemCount(Some(popup)) }).rev() {
        // A submenu has no id of its own (u32::MAX), a separator 0: neither is ours to take.
        if left_out(&menu.cm, unsafe { GetMenuItemID(popup, pos) }) {
            removed |= unsafe { DeleteMenu(popup, pos as u32, MF_BYPOSITION) }.is_ok();
        }
    }
    if removed {
        tidy_separators(popup);
    }
}

/// Takes out every separator at an end of the menu or right after another.
fn tidy_separators(popup: HMENU) {
    // From the end: a separator goes when the end or another separator comes after it.
    let mut next_is_separator = true;
    for pos in (0..unsafe { GetMenuItemCount(Some(popup)) }).rev() {
        let separator = is_separator(popup, pos);
        if separator && next_is_separator && unsafe { DeleteMenu(popup, pos as u32, MF_BYPOSITION) }.is_ok() {
            continue;
        }
        next_is_separator = separator;
    }
    if unsafe { GetMenuItemCount(Some(popup)) } > 0 && is_separator(popup, 0) {
        let _ = unsafe { DeleteMenu(popup, 0, MF_BYPOSITION) };
    }
}

fn is_separator(popup: HMENU, pos: i32) -> bool {
    let mut info = MENUITEMINFOW { cbSize: size_of::<MENUITEMINFOW>() as u32, fMask: MIIM_FTYPE, ..Default::default() };
    unsafe { GetMenuItemInfoW(popup, pos as u32, true, &mut info) }.is_ok() && info.fType.0 & MFT_SEPARATOR.0 != 0
}

/// Sends the owner window's menu messages to the shell menu first, for as
/// long as it lives (the menu is open). Without it the submenus that fill
/// themselves when opened stay empty and owner-drawn items stay blank.
struct Subclass<'a>(HWND, PhantomData<&'a ShellMenu>);

impl<'a> Subclass<'a> {
    fn install(owner: HWND, menu: &'a ShellMenu) -> Option<Self> {
        if menu.cm2.is_none() && menu.cm3.is_none() {
            return None;
        }
        // Should this fail the menu still works, only without what those messages bring.
        let data = menu as *const ShellMenu as usize;
        if unsafe { SetWindowSubclass(owner, Some(menu_proc), SUBCLASS_ID, data) }.as_bool() {
            Some(Subclass(owner, PhantomData))
        } else {
            None
        }
    }
}

impl Drop for Subclass<'_> {
    fn drop(&mut self) {
        let _ = unsafe { RemoveWindowSubclass(self.0, Some(menu_proc), SUBCLASS_ID) };
    }
}

/// The window subclass while the menu is open. Must not panic (it is called
/// from Windows).
unsafe extern "system" fn menu_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    data: usize,
) -> LRESULT {
    // WM_DRAWITEM and WM_MEASUREITEM come from a menu when wParam is 0, else from a control.
    let for_menu = match msg {
        WM_INITMENUPOPUP | WM_MENUCHAR => true,
        WM_DRAWITEM | WM_MEASUREITEM => wparam.0 == 0,
        _ => false,
    };
    if for_menu {
        // The ShellMenu the subclass was installed with; it outlives the subclass (Subclass<'a>).
        let menu = unsafe { &*(data as *const ShellMenu) };
        if let Some(cm3) = &menu.cm3 {
            let mut result = LRESULT(0);
            if unsafe { cm3.HandleMenuMsg2(msg, wparam, lparam, Some(&raw mut result)) }.is_ok() {
                return result;
            }
        } else if let Some(cm2) = &menu.cm2
            && unsafe { cm2.HandleMenuMsg(msg, wparam, lparam) }.is_ok()
        {
            return LRESULT(0);
        }
    }
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

/// Runs the menu command `offset` places after ID_FIRST. Ok(false) when its
/// handler says it was cancelled (it asked first and the answer was no). A
/// command that goes on in its own thread (CMIC_MASK_ASYNCOK) returns before
/// asking anything, so for such a one a no still comes back as Ok(true).
fn invoke(menu: &ShellMenu, owner: HWND, offset: u32, items: &[PathBuf], x: i32, y: i32) -> Result<bool, String> {
    // The items' folder, for commands that start a program there ("Open in Terminal").
    let dir: Option<Vec<u16>> = items[0].parent().map(|d| d.as_os_str().encode_wide().chain(Some(0)).collect());
    let info = CMINVOKECOMMANDINFOEX {
        cbSize: size_of::<CMINVOKECOMMANDINFOEX>() as u32,
        // ASYNCOK lets a slow command go on in its own thread; this one (the UI thread) stays.
        // No CMIC_MASK_SHIFT_DOWN / CONTROL_DOWN: see the top of this file.
        fMask: CMIC_MASK_UNICODE | CMIC_MASK_PTINVOKE | CMIC_MASK_ASYNCOK,
        hwnd: owner,
        // MAKEINTRESOURCE(offset): the command by its place in the menu instead of a verb name.
        lpVerb: PCSTR(std::ptr::without_provenance(offset as usize)),
        lpVerbW: PCWSTR(std::ptr::without_provenance(offset as usize)),
        lpDirectoryW: dir.as_ref().map_or(PCWSTR::null(), |d| PCWSTR(d.as_ptr())),
        nShow: SW_SHOWNORMAL.0,
        ptInvoke: POINT { x, y },
        ..Default::default()
    };
    let info = &info as *const CMINVOKECOMMANDINFOEX as *const CMINVOKECOMMANDINFO;
    match unsafe { menu.cm.InvokeCommand(info) } {
        Ok(()) => Ok(true),
        Err(e) if e.code() == ERROR_CANCELLED.to_hresult() => Ok(false),
        Err(e) => Err(format!("{NOT_RUN} ({e})")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Com::CoUninitialize;

    fn check(paths: &[&str]) -> Result<Vec<PathBuf>, String> {
        check_paths(&paths.iter().map(|p| p.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn items_are_full_paths_in_one_folder() {
        assert_eq!(
            check(&[r"C:\a\b.txt", r"c:/A/c/", r"C:\A\B.TXT"]).unwrap(),
            [PathBuf::from(r"C:\a\b.txt"), PathBuf::from(r"c:\A\c")]
        );
        assert_eq!(check(&[r"C:\", r"D:\"]).unwrap(), [PathBuf::from(r"C:\"), PathBuf::from(r"D:\")]);
        assert_eq!(check(&[r"\\server\share\a", r"\\server\share\b"]).unwrap().len(), 2);
        // Shares are items of their server's folder.
        assert_eq!(check(&[r"\\server\x\", r"\\SERVER\y"]).unwrap().len(), 2);

        assert_eq!(check(&[]).unwrap_err(), NO_ITEMS);
        for mixed in [
            &[r"C:\a\x", r"C:\b\y"][..],
            &[r"C:\a\x", r"D:\a\y"],
            &[r"C:\", r"C:\a"],
            &[r"C:\a\x", r"C:\a\x\y"],
            // A root has no parent, but the roots of drives and of shares are not all in one folder.
            &[r"C:\", r"\\srvA\share\"],
            &[r"\\srvA\share\", r"C:\"],
            &[r"\\a\x\", r"\\b\y\"],
            &[r"\\a\x\", r"\\a\x\y"],
        ] {
            assert_eq!(check(mixed).unwrap_err(), MIXED, "{mixed:?}");
        }
        for relative in [r"a\b", r"C:a\b", r"\a\b", r"C:\a\..\b", r"C:", r"\\?\C:\a", r"\\.\C:\a", ""] {
            assert!(check(&[relative]).unwrap_err().starts_with(NOT_FULL), "{relative}");
        }
        assert!(check(&[r"C:\a\b."]).unwrap_err().starts_with(ODD_NAME));
        assert!(check(&[r"C:\a \b"]).unwrap_err().starts_with(ODD_NAME));
    }

    #[test]
    fn builds_the_menu_for_a_file() {
        let Ok(initialized) = enter_sta() else {
            eprintln!("COM is not available here: skipped");
            return;
        };
        let dir = std::env::temp_dir().join(format!("ff-shellmenu-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("menu test.txt");
        std::fs::write(&file, b"fine-file").unwrap();
        let items = check(&[file.to_str().unwrap()]).unwrap();
        // Never shown: only filled. The interfaces are gone before COM is.
        let seen = fill(HWND::default(), &items, CMF_NORMAL | CMF_EXPLORE).map(|menu| {
            let before = verbs(&menu);
            leave_out(&menu);
            (before, verbs(&menu), unsafe { GetMenuItemCount(Some(menu.popup)) })
        });
        if initialized {
            unsafe { CoUninitialize() };
        }
        let _ = std::fs::remove_dir_all(&dir);
        let (before, after, entries) = seen.unwrap();
        eprintln!("verbs before: {before:?}\nverbs after: {after:?}");
        assert!(entries > 0);
        // Explorer's delete is in the menu as the shell fills it, and not once this app has it.
        assert!(before.iter().any(|v| v.eq_ignore_ascii_case("delete")), "{before:?}");
        assert!(after.iter().all(|v| !LEFT_OUT.iter().any(|l| v.eq_ignore_ascii_case(l))), "{after:?}");
        assert!(after.len() < before.len());
    }

    /// The verbs of the commands on the menu's top level, where the shell names them.
    fn verbs(menu: &ShellMenu) -> Vec<String> {
        (0..unsafe { GetMenuItemCount(Some(menu.popup)) })
            .map(|pos| unsafe { GetMenuItemID(menu.popup, pos) })
            .filter(|id| (ID_FIRST..=ID_LAST).contains(id))
            .filter_map(|id| verb(&menu.cm, id - ID_FIRST))
            .collect()
    }

    #[test]
    fn the_recycle_bin_is_not_emptied_from_the_menu() {
        let Ok(initialized) = enter_sta() else {
            eprintln!("COM is not available here: skipped");
            return;
        };
        let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        let bin = PathBuf::from(format!(r"{drive}\$Recycle.Bin"));
        // The folder, and each account's bin in it (the shell's Recycle Bin for the account's own).
        let mut folders = vec![bin.clone()];
        folders.extend(std::fs::read_dir(&bin).into_iter().flatten().flatten().map(|e| e.path()));
        let seen: Vec<_> = folders
            .iter()
            .filter_map(|f| {
                let items = check(&[f.to_str()?]).ok()?;
                let menu = fill(HWND::default(), &items, CMF_NORMAL | CMF_EXPLORE).ok()?;
                let before = verbs(&menu);
                leave_out(&menu);
                Some((f.clone(), before, verbs(&menu)))
            })
            .collect();
        if initialized {
            unsafe { CoUninitialize() };
        }
        // A folder with no menu (another account's bin, no bin on this drive) has nothing to take out.
        // The account's own bin offers "empty" as the shell fills it.
        for (folder, before, after) in &seen {
            eprintln!("{}\n  verbs before: {before:?}\n  verbs after: {after:?}", folder.display());
            assert!(after.iter().all(|v| !LEFT_OUT.iter().any(|l| v.eq_ignore_ascii_case(l))), "{after:?}");
        }
    }

    #[test]
    fn no_separator_is_left_at_an_end_or_doubled() {
        use windows::core::w;
        use windows::Win32::UI::WindowsAndMessaging::{AppendMenuW, MF_SEPARATOR, MF_STRING};
        let popup = unsafe { CreatePopupMenu() }.unwrap();
        // | a | | | b | |   (| a separator)
        for (sep, id) in [(true, 0), (false, 1), (true, 0), (true, 0), (false, 2), (true, 0)] {
            let flags = if sep { MF_SEPARATOR } else { MF_STRING };
            unsafe { AppendMenuW(popup, flags, id, if sep { PCWSTR::null() } else { w!("item") }) }.unwrap();
        }
        tidy_separators(popup);
        let shape: Vec<bool> = (0..unsafe { GetMenuItemCount(Some(popup)) }).map(|pos| is_separator(popup, pos)).collect();
        assert_eq!(shape, [false, true, false]);
        assert_eq!((unsafe { GetMenuItemID(popup, 0) }, unsafe { GetMenuItemID(popup, 2) }), (1, 2));
        unsafe { DestroyMenu(popup) }.unwrap();
    }
}
