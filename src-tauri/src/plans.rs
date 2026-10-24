//! Optimization plans: a list of steps (make folder, move, rename, send to the
//! Recycle Bin, compress, move to another drive) made by rules or by AI, that
//! the user reviews, edits and approves before anything runs. Every executed
//! step is journaled with its inverse, so a whole run can be undone.
//!
//! Plans live in `<data dir>\plans\<id>.json`; the MCP server (mcp.rs) may add
//! plans there for the user to review ("source": "mcp"), never run them.
//! Progress arrives as "plan-progress" events (PlanProgress).
//!
//! What holds for every plan, whoever made it:
//! - Nothing runs but by `plan_execute` (the user's approval), and only the
//!   steps left selected. A plan that comes in through `store::save` (AI, MCP)
//!   is untrusted data: its shape is checked, it may only make folders, move,
//!   rename and trash, never for a whole drive; what is not a valid step is
//!   dropped (the summary says why), at most MAX_STEPS stay, and every Recycle
//!   Bin step (and every step its maker calls dangerous) starts unselected.
//!   Names are names only; nothing in them is ever read as an instruction.
//! - Each step is checked when the plan is saved or edited, and again right
//!   before it runs (`check`): a full path on a drive letter inside the plan's
//!   folder (a relocation's destination drive excepted), never a drive root,
//!   no short (8.3) name and no junction or symbolic link on the way below
//!   that folder, nothing of Windows or of an installed program (`classify`,
//!   the folders ops never moves, the Windows known folders and any folder
//!   holding one), and a source that is there and is not a link or a cloud file.
//! - "trash" goes to the Recycle Bin only, with the checks `trash_paths` makes
//!   (lib.rs); a duplicate (dedupe) only while the copy that stays is there,
//!   no step of the plan takes it, and both still have the content the plan
//!   found (blake3). "move" and "rename" never write over anything (another
//!   drive: ops' move, which removes the source only once the copy is
//!   checked); "compress" is Windows' own transparent compression (WOF LZX,
//!   else NTFS LZNT1), undone by decompressing; "relocate" is ops' relocation.
//! - What each done step did goes into the plan file, with how to undo it
//!   (the journal), before the next step starts: while that file cannot be
//!   written the run waits, then stops, and keeps trying to write what ran.
//!   `plan_undo` works through it last first: renames and moves go back
//!   (never over anything), items come back from the Recycle Bin (never over
//!   anything either), files are decompressed, relocations undone.
//! - One plan runs (or is undone) at a time, among all the app's processes.
//!   Closing the app lets the step at hand finish and be written down, and
//!   starts no other (`stop_and_wait`, lib.rs).

use crate::dups::{self, DupGroup};
use crate::index::{Index, FLAG_DELETED, FLAG_DIR, FLAG_HARDLINK, FLAG_NOT_LINK, FLAG_REPARSE, ROOT};
use crate::{classify, ops, win};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};
use windows_sys::Win32::Foundation::{ERROR_DIR_NOT_EMPTY, ERROR_SHARING_VIOLATION};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_SYSTEM, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE,
};

/// Steps a plan from AI or MCP keeps at most (rules make no more either).
const MAX_STEPS: usize = 500;
/// Progress events of a run are at least this far apart (state changes
/// excepted); the tests hear every one.
const PROGRESS_GAP: Duration = if cfg!(test) { Duration::ZERO } else { Duration::from_millis(100) };
/// FILETIME ticks in a day.
const DAY: i64 = 864_000_000_000;
const MB: u64 = 1 << 20;
const GB: u64 = 1 << 30;
/// Rules: the files the duplicate search compares, as in its window.
const DEDUPE_MIN: u64 = MB;
/// Rules: installers and archives in Downloads older than this are offered...
const OLD_DOWNLOAD_DAYS: i64 = 30;
/// ...and so are files this big not changed for BIG_OLD_DAYS.
const BIG_OLD_SIZE: u64 = GB;
const BIG_OLD_DAYS: i64 = 365;
/// Rules: a temp item changed within this long may still be in use.
const RECENT: i64 = DAY;
/// Rules: files worth compressing are at least this big.
const SHRINK_MIN: u64 = 16 * MB;
/// Rules: folders worth moving to another drive are at least this big...
const RELOCATE_MIN: u64 = 5 * GB;
/// ...and this many at most go in one plan...
const RELOCATE_MOST: usize = 10;
/// ...leaving this much room on the drive they go to (ops keeps as much).
const RELOCATE_MARGIN: u64 = GB;
/// Files one "compress" step of a folder compresses at most: each goes into
/// the journal, which is written after every step.
const PACK_MOST: usize = 20_000;
/// A step that changed nothing on disk is written down at least this often
/// (one that did is written down before the next starts).
const QUIET_SAVE: Duration = Duration::from_secs(1);
/// A journal that cannot be written (a full disk, a scanner holding the file)
/// is tried again after this, then twice as long each time up to JOURNAL_GAP_MOST...
const JOURNAL_GAP: Duration = Duration::from_millis(100);
const JOURNAL_GAP_MOST: Duration = Duration::from_secs(5);
/// ...and the run waits this long for it before it stops, the steps left not run.
/// What ran is still tried until it is written down.
const JOURNAL_PATIENCE: Duration = if cfg!(test) { Duration::from_secs(1) } else { Duration::from_secs(120) };
/// Bytes read at a time to compare a duplicate with the copy that stays.
const HASH_BUF: usize = 1 << 20;

const NOT_FOUND: &str = "계획을 찾을 수 없어요";
const BAD_FILE: &str = "계획 파일을 읽을 수 없어요";
const BAD_ID: &str = "계획 이름이 올바르지 않아요";
const BAD_KIND: &str = "알 수 없는 종류의 계획이에요";
const BAD_SOURCE: &str = "알 수 없는 곳에서 만든 계획이에요";
const BAD_ROOT: &str = "계획의 폴더 경로가 올바르지 않아요";
const NOT_DRAFT: &str = "이미 실행했거나 실행 중인 계획은 바꿀 수 없어요";
const BUSY: &str = "다른 계획을 실행하거나 되돌리는 중이에요. 끝난 뒤에 다시 해 보세요";
const RUNNING: &str = "실행 중인 계획은 지울 수 없어요";
const NOTHING_SELECTED: &str = "실행할 단계를 하나 이상 골라 주세요";
const NOTHING_TO_UNDO: &str = "되돌릴 단계가 없어요";
const PANICKED: &str = "예기치 못한 문제로 이 단계를 멈췄어요";
const SCAN_FIRST: &str = "이 드라이브를 먼저 스캔해 주세요";
const NOT_INDEXED: &str = "이 폴더는 스캔 결과에 없어요. 다시 스캔해 주세요";
const ROOT_NOT_FOLDER: &str = "폴더를 골라 주세요";
const ROOT_LINK: &str = "링크(정션) 폴더라서 계획을 만들지 않았어요. 링크가 가리키는 폴더에서 만들어 주세요";

const UNKNOWN_ACTION: &str = "알 수 없는 동작이에요";
const BAD_PATH: &str = "경로가 올바르지 않아요 (드라이브 문자로 시작하는 전체 경로여야 해요)";
const NO_TARGET: &str = "옮길 곳이 없어요";
const OUTSIDE_ROOT: &str = "계획의 폴더 밖이라서 하지 않아요";
const DRIVE_ROOT: &str = "드라이브 전체는 건드리지 않아요";
const SYSTEM_ITEM: &str = "Windows가 쓰는 시스템 항목이라 건드리지 않아요";
const PROGRAM_ITEM: &str = "프로그램·게임·개발 도구·앱 데이터 폴더의 항목이라 건드리지 않아요";
const PROTECTED: &str = "Windows·프로그램·사용자 폴더처럼 그 자리에 그대로 있어야 하는 폴더라서 건드리지 않아요";
const KNOWN_FOLDER: &str = "바탕 화면·문서·다운로드 같은 Windows 기본 폴더 자체는 옮기거나 지우지 않아요 (안에 있는 항목은 돼요)";
const HOLDS_KNOWN: &str = "안에 바탕 화면·문서·다운로드 같은 Windows 기본 폴더(또는 원드라이브 폴더)가 있어서 옮기거나 지우지 않아요";
const MISSING: &str = "항목을 찾을 수 없어요";
const LINK_ITEM: &str = "정션·심볼릭 링크나 클라우드(온라인 전용) 항목은 건드리지 않아요";
const LINK_ON_PATH: &str = "경로 중간에 정션·심볼릭 링크가 있어서 하지 않아요";
const SYSTEM_ATTR: &str = "시스템 속성이 있는 항목이라 건드리지 않아요";
const NAME_TAKEN: &str = "같은 이름이 이미 있어요 (덮어쓰지 않아요)";
const SAME_PLACE: &str = "이미 그 자리에 있어요";
const INTO_ITSELF: &str = "폴더를 그 폴더 안으로 옮길 수 없어요";
const NO_PARENT: &str = "넣을 폴더가 없어요";
const NOT_SAME_FOLDER: &str = "이름 바꾸기는 같은 폴더 안에서만 해요";
const NOT_NTFS: &str = "NTFS 드라이브에서만 압축할 수 있어요";
const ONLY_FOLDERS: &str = "폴더만 다른 드라이브로 옮길 수 있어요";
const SAME_DRIVE: &str = "같은 드라이브로는 옮길 수 없어요";
const MOVED_LOST: &str = "옮긴 항목을 찾을 수 없어요";
const ALREADY_SMALL: &str = "이미 압축돼 있거나 압축해도 줄지 않아요";
const NOT_RECYCLED: &str = "휴지통으로 옮기지 못했어요";
const NO_RECYCLE_BIN: &str = "이 드라이브에는 휴지통이 없어서 지우지 않았어요 (지우면 되돌릴 수 없어요)";
const BIN_DISABLED: &str = "이 드라이브는 휴지통을 쓰지 않도록 설정돼 있어서 지우지 않았어요 (지우면 되돌릴 수 없어요)";
const BIN_TOO_SMALL: &str = "휴지통보다 커서 지우지 않았어요. 휴지통 속성에서 최대 크기를 늘리면 옮길 수 있어요";
const ODD_NAME: &str = "이름이 점(.)이나 공백으로 끝나는 항목은 휴지통으로 옮길 수 없어서 지우지 않았어요";
const SHORT_NAME: &str = "짧은 이름(8.3, 예: PROGRA~1)이 들어 있는 경로라서 하지 않아요. 전체 이름으로 된 경로여야 해요";
const DRIVE_ROOT_PLAN: &str = "드라이브 전체(루트)에는 AI나 MCP의 계획을 받지 않아요. 그 아래 폴더를 골라 주세요";
const UNTRUSTED_ACTION: &str = "AI나 MCP의 계획에서는 새 폴더, 옮기기, 이름 바꾸기, 휴지통만 쓸 수 있어요";
const RULES_ONLY: &str = "규칙으로 만든 계획은 이렇게 저장할 수 없어요";
const TWIN_GONE: &str = "남기기로 한 같은 내용의 파일이 없어져서 지우지 않았어요";
const TWIN_ODD: &str = "남기기로 한 파일이 폴더, 링크나 클라우드(온라인 전용) 파일이 되어서 지우지 않았어요";
const TWIN_CHANGED: &str = "이 파일이나 남기기로 한 파일의 내용이 바뀌어서 지우지 않았어요 (이제 같은 내용이 아니에요)";
const TWIN_SAME: &str = "남기기로 한 파일이 이 파일과 같은 항목이라서 지우지 않았어요";
const TWIN_TAKEN: &str = "남기기로 한 파일도 이 계획의 다른 단계가 옮기거나 지워서 이 파일은 지우지 않았어요";
const TWIN_UNREAD: &str = "남기기로 한 파일과 같은 내용인지 확인하지 못해서 지우지 않았어요";
const KEEP_UNKNOWN: &str = "남길 사본을 확인할 수 없어서 지우지 않았어요";
const JOURNAL_WAIT: &str = "되돌리기 기록을 저장하지 못해서 다음 단계를 기다리게 했어요. 다시 저장하는 중이에요";
const JOURNAL_UNSAVED: &str = "되돌리기 기록을 저장하지 못해서 이 단계는 실행하지 않았어요";
const UNDO_UNSAVED: &str = "되돌린 기록을 저장하지 못해서 이 단계는 되돌리지 않았어요. 다시 \"되돌리기\"를 누르면 이어서 해요";
const CLOSING: &str = "앱을 끄는 중이라 이 단계는 실행하지 않았어요";
const CLOSING_UNDO: &str = "앱을 끄는 중이라 되돌리지 않았어요. 다시 \"되돌리기\"를 누르면 이어서 해요";
const PACK_STOPPED: &str = "앱을 끄는 중이라 압축을 중간에 멈췄어요";

const NOT_IN_BIN: &str = "휴지통에서 찾을 수 없어요 (휴지통을 비웠거나 이미 되살렸을 수 있어요)";
const ORIGINAL_TAKEN: &str = "원래 자리에 같은 이름이 있어서 되돌리지 않았어요";
const NOT_RESTORED: &str = "휴지통에서 되살리지 못했어요";
const FOLDER_NOT_EMPTY: &str = "폴더에 다른 항목이 생겨서 지우지 않았어요";
const NOT_OUR_FOLDER: &str = "만들었던 폴더가 아닌 것이 그 자리에 있어서 지우지 않았어요";
const NO_RELOCATION_RECORD: &str = "옮긴 기록을 찾지 못해서 이 계획으로는 되돌릴 수 없어요. \"옮긴 폴더 목록\"에서 되돌려 주세요";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PlanStep {
    pub id: u32,
    /// "mkdir" | "move" | "rename" | "trash" | "compress" | "relocate"
    pub action: String,
    pub from: String,
    /// move/rename/relocate: the new path (relocate: the destination folder).
    pub to: Option<String>,
    /// Why, in Korean, for the user.
    pub reason: String,
    /// "safe" | "caution" | "danger"
    pub risk: String,
    pub size: u64,
    pub selected: bool,
    /// "pending" | "done" | "failed" | "skipped" | "undone"
    pub state: String,
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Plan {
    pub id: String,
    /// "structure" | "rename" | "dedupe" | "shrink" | "relocate" | "cleanup"
    pub kind: String,
    pub title: String,
    pub summary: String,
    /// "rules" | "claude" | "openai" | "ollama" | "claude_cli" | "codex_cli" | "mcp"
    pub source: String,
    /// The folder the plan is about.
    pub root: String,
    /// FILETIME
    pub created_at: i64,
    pub steps: Vec<PlanStep>,
    /// "draft" | "running" | "done" | "partly" | "undone" | "failed"
    pub state: String,
}

#[derive(Serialize, Clone, Debug)]
pub struct PlanProgress {
    pub id: String,
    /// Plan.state now.
    pub state: String,
    pub done: u32,
    pub total: u32,
    pub current: Option<String>,
}

/// What a plan file holds: the plan, and the journal of its run.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct PlanFile {
    #[serde(flatten)]
    plan: Plan,
    /// Every step the run did, in the order done, with how to undo it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    journal: Vec<Done>,
    /// Dedupe (rules only): for a Recycle Bin step, the copy of equal content
    /// that stays. Checked again before the step runs; never in a plan from
    /// outside (`store::save` writes none), nor shown to or taken from the UI.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    keeps: Vec<Keep>,
}

/// The copy that stays when a duplicate goes to the Recycle Bin.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Keep {
    /// PlanStep.id of the Recycle Bin step.
    step: u32,
    path: String,
    /// What both files held when the plan was made: bytes and blake3 (hex).
    size: u64,
    hash: String,
}

impl PlanFile {
    fn new(plan: Plan) -> PlanFile {
        PlanFile { plan, journal: Vec::new(), keeps: Vec::new() }
    }

    fn keep_of(&self, step: u32) -> Option<&Keep> {
        self.keeps.iter().find(|k| k.step == step)
    }
}

/// One step a run did.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct Done {
    /// PlanStep.id
    step: u32,
    undo: Inverse,
    #[serde(default)]
    undone: bool,
}

/// How to undo one done step.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Inverse {
    /// The step made this folder: it goes again, only while it is empty.
    Rmdir { path: String },
    /// The item at `from` went to `to` (a move or a rename): it goes back,
    /// never over anything.
    MoveBack { from: String, to: String },
    /// `path` (where it really was: `real`) went to the Recycle Bin between
    /// these times (Unix seconds): that item comes back.
    Restore { path: String, real: String, deleted_from: i64, deleted_to: i64 },
    /// These files were compressed: they are decompressed.
    Decompress { files: Vec<Packed> },
    /// The relocation `id` (ops) moved `original` to `target`: it is undone.
    Unrelocate { id: String, original: String, target: String },
}

/// A file a step compressed, and how.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Packed {
    path: String,
    /// WOF (LZX); else NTFS compression (LZNT1).
    wof: bool,
}

/// Saving and reading plan files; used by the commands below, ai.rs and mcp.rs.
pub mod store {
    use super::{Plan, Store};
    use std::io;

    /// Saves a draft plan made outside the rules (AI, MCP), as untrusted data:
    /// its shape is checked, invalid steps (and any but mkdir, move, rename
    /// and trash) are dropped (the summary says why) and the rest checked
    /// against the disk (a step that does not hold is "skipped", with why), at
    /// most MAX_STEPS stay, and no Recycle Bin step nor "danger" step stays
    /// selected. A drive root is no plan's folder here. `load` gives the plan
    /// as it was stored. A plan that has run (or runs) is never written over.
    /// The id is what `new_id` makes (ASCII letters, digits, `-` and `_`).
    pub fn save(plan: &Plan) -> io::Result<()> {
        Store::app().save(plan).map(|_| ())
    }

    pub fn load(id: &str) -> io::Result<Plan> {
        Store::app().load(id).map(|f| f.plan)
    }

    /// Newest first.
    pub fn list() -> Vec<Plan> {
        Store::app().list()
    }
}

/// A new plan id: `p<FILETIME in hex>-<random hex>`.
pub(crate) fn new_id() -> String {
    format!("p{:016x}-{:08x}", crate::index::now_filetime(), win::random_u64() as u32)
}

/// Every saved plan, newest first.
#[tauri::command(async)]
pub fn plans_list() -> Vec<Plan> {
    store::list()
}

#[tauri::command(async)]
pub fn plan_get(id: String) -> Result<Plan, String> {
    store::load(&id).map_err(|e| e.to_string())
}

/// A plan made by rules (no AI) for `path`: kind "cleanup" | "dedupe" | "shrink" | "relocate".
#[tauri::command]
pub async fn plan_rules(kind: String, path: String, state: tauri::State<'_, crate::AppState>) -> Result<Plan, String> {
    let root = norm(&path).ok_or(BAD_PATH)?;
    let drive = crate::drive_of(&root).ok_or(BAD_PATH)?;
    let shared = state.get(&drive).ok_or(SCAN_FIRST)?;
    crate::blocking(move || rules_plan(&Store::app(), &kind, &root, &shared)).await?
}

/// Saves the user's edits (which steps are selected, edited targets) after
/// checking them again. Only draft plans can change.
#[tauri::command(async)]
pub fn plan_update(plan: Plan) -> Result<Plan, String> {
    Store::app().update(&plan).map_err(|e| e.to_string())
}

/// Runs the selected steps of a draft plan in the background.
#[tauri::command]
pub async fn plan_execute(app: tauri::AppHandle, id: String) -> Result<(), String> {
    crate::blocking(move || launch(Store::app(), id, Work::Execute, app)).await?
}

/// Undoes every done step of a plan, last first. Not while the app runs
/// elevated (ops::ELEVATED_JOURNALS): the journal is in the plan file.
#[tauri::command]
pub async fn plan_undo(app: tauri::AppHandle, id: String) -> Result<(), String> {
    ops::journals_refused()?;
    crate::blocking(move || launch(Store::app(), id, Work::Undo, app)).await?
}

#[tauri::command(async)]
pub fn plan_delete(id: String) -> Result<(), String> {
    Store::app().remove(&id).map_err(|e| e.to_string())
}

// ---- Paths ------------------------------------------------------------------------

/// `path` as plans keep it: a full path on a drive letter (`C:\a\b`), `/`
/// taken for `\`, no `\` at the end but in `C:\`, the letter in upper case.
/// None for anything else: relative paths, `.` and `..` parts, network and
/// device paths, wildcards, control characters.
fn norm(path: &str) -> Option<String> {
    if path.len() > 4096 {
        return None;
    }
    let path = path.replace('/', "\\");
    let path = path.strip_prefix(r"\\?\").unwrap_or(&path);
    let b = path.as_bytes();
    if b.len() < 3 || !b[0].is_ascii_alphabetic() || b[1] != b':' || b[2] != b'\\' {
        return None;
    }
    let rest = path[3..].trim_end_matches('\\');
    let mut parts = Vec::new();
    if !rest.is_empty() {
        for part in rest.split('\\') {
            let odd = |c: char| c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*');
            if part.is_empty() || part == "." || part == ".." || part.chars().any(odd) {
                return None;
            }
            parts.push(part);
        }
    }
    Some(format!("{}:\\{}", b[0].to_ascii_uppercase() as char, parts.join("\\")))
}

/// A normalized path for comparing: Windows names ignore case.
fn key(path: &str) -> String {
    path.to_lowercase()
}

/// Whether the normalized `inner` is somewhere in the folder `outer` (not `outer` itself).
fn below(inner: &str, outer: &str) -> bool {
    let (inner, outer) = (key(inner), key(outer));
    let outer = if outer.ends_with('\\') { outer } else { format!("{outer}\\") };
    inner.len() > outer.len() && inner.starts_with(&outer)
}

/// The folder a normalized path is in (`C:\a` → `C:\`); None for a drive root.
fn parent_of(path: &str) -> Option<&str> {
    let i = path.rfind('\\')?;
    if path.len() <= 3 {
        return None;
    }
    Some(if i == 2 { &path[..3] } else { &path[..i] })
}

fn name_of(path: &str) -> &str {
    path.rsplit('\\').next().unwrap_or(path)
}

fn is_drive_root(path: &str) -> bool {
    path.len() == 3
}

fn ext_of(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext.to_lowercase(),
        _ => String::new(),
    }
}

fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(win::verbatim(path)).is_ok()
}

fn meta_of(path: &str) -> io::Result<Metadata> {
    std::fs::symlink_metadata(win::verbatim(Path::new(path)))
}

/// A junction or symbolic link (a name surrogate), by its reparse tag. A
/// cloud placeholder or a WOF-compressed file is none.
fn is_link_tag(tag: u32) -> bool {
    tag & 0x2000_0000 != 0
}

fn now_filetime() -> i64 {
    crate::index::now_filetime()
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

fn size_text(bytes: u64) -> String {
    let b = bytes as f64;
    if bytes >= GB {
        format!("{:.1} GB", b / GB as f64)
    } else if bytes >= MB {
        format!("{:.0} MB", b / MB as f64)
    } else if bytes >= 1024 {
        format!("{:.0} KB", b / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// Text from outside (AI, MCP) as the window may show it: no control
/// characters but line breaks, at most `max` characters.
fn clip(text: &str, max: usize) -> String {
    text.chars().filter(|&c| !c.is_control() || c == '\n').take(max).collect()
}

// ---- Checks -----------------------------------------------------------------------

/// What the checks look up once for many steps.
struct Ctx {
    guards: ops::Guards,
    /// The Windows known folders (Desktop, Documents, ...), as `key`s.
    known: HashSet<String>,
}

impl Ctx {
    fn now() -> Ctx {
        Ctx { guards: ops::Guards::now(), known: known_folders() }
    }
}

fn known_folders() -> HashSet<String> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::*;
    let ids = [
        &FOLDERID_Desktop,
        &FOLDERID_Documents,
        &FOLDERID_Downloads,
        &FOLDERID_Pictures,
        &FOLDERID_Music,
        &FOLDERID_Videos,
        &FOLDERID_Favorites,
        &FOLDERID_Contacts,
        &FOLDERID_Links,
        &FOLDERID_SavedGames,
        &FOLDERID_SkyDrive,
        &FOLDERID_Profile,
        &FOLDERID_UserProfiles,
        &FOLDERID_RoamingAppData,
        &FOLDERID_LocalAppData,
        &FOLDERID_LocalAppDataLow,
        &FOLDERID_StartMenu,
        &FOLDERID_Programs,
        &FOLDERID_Startup,
        &FOLDERID_Templates,
        &FOLDERID_SendTo,
        &FOLDERID_Recent,
        &FOLDERID_Public,
        &FOLDERID_PublicDesktop,
        &FOLDERID_PublicDocuments,
        &FOLDERID_PublicDownloads,
    ];
    let mut known = HashSet::new();
    for id in ids {
        unsafe {
            let mut p: windows_sys::core::PWSTR = std::ptr::null_mut();
            let hr = SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut p);
            if hr >= 0 && !p.is_null() {
                let len = (0..).take_while(|&i| *p.add(i) != 0).count();
                let path = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
                known.extend(norm(&path).map(|p| key(&p)));
            }
            if !p.is_null() {
                CoTaskMemFree(p.cast());
            }
        }
    }
    // OneDrive's own folders (work or school ones are no known folder).
    for var in ["OneDrive", "OneDriveCommercial", "OneDriveConsumer"] {
        if let Some(path) = std::env::var_os(var) {
            known.extend(norm(&path.to_string_lossy()).filter(|p| !is_drive_root(p)).map(|p| key(&p)));
        }
    }
    known
}

/// Whether a known folder is `path` or somewhere below it: moving, renaming
/// or trashing `path` would take that folder away from where Windows looks
/// for it (Documents redirected to `D:\Personal\Data\Documents`, say).
fn holds_known(ctx: &Ctx, path: &str) -> bool {
    let k = key(path);
    ctx.known.contains(&k) || ctx.known.iter().any(|f| below(f, &k))
}

/// Fails when a name on the way to `path` (as far as it is on disk) is a
/// short 8.3 name: `C:\PROGRA~3` is ProgramData, which the checks by name
/// would not see. When Windows cannot spell the names out (a folder on the
/// way that cannot be listed), a name that looks like a short one fails too.
fn long_names(path: &str) -> Result<(), String> {
    let mut at = Some(path);
    while let Some(p) = at {
        if exists(Path::new(p)) {
            let long = match win::try_long_path(p) {
                Some(long) => key(&long) == key(p),
                None => !win::looks_short(p),
            };
            return if long { Ok(()) } else { Err(SHORT_NAME.into()) };
        }
        at = parent_of(p);
    }
    Ok(())
}

/// Folders named like caches that hold what Windows, installers or Office
/// still need (`Package Cache` keeps what uninstalls and repairs use).
const KEPT_CACHES: &[&str] = &["package cache", "officefilecache", "installer", "winsxs"];

/// Why `action` may not touch the item `path` for what it is (`classify`),
/// if so. Windows' own items never; a program's (also a game's, a dev tool's,
/// app data) only to send a cache or temp item of it to the Recycle Bin,
/// which the program makes again, or to relocate a folder of it (a junction
/// stays, so it is found where it was) unless it is an installed program.
fn kind_refusal(action: &str, path: &str, is_dir: bool) -> Option<&'static str> {
    let name = name_of(path);
    let category = classify::category(path, name, is_dir);
    let program = classify::program_category(path, name, is_dir);
    if category == "system" || program == Some("system") {
        return Some(SYSTEM_ITEM);
    }
    let parts: Vec<String> = path.get(3..).unwrap_or_default().split('\\').map(str::to_lowercase).collect();
    if parts.first().is_some_and(|p| p.starts_with("program files")) || parts.iter().any(|p| KEPT_CACHES.contains(&p.as_str())) {
        return Some(PROGRAM_ITEM);
    }
    match action {
        "trash" => (program.is_some() && category != "cache").then_some(PROGRAM_ITEM),
        "relocate" => (category == "program").then_some(PROGRAM_ITEM),
        _ => program.is_some().then_some(PROGRAM_ITEM),
    }
}

/// Why something at `path` may not be touched by `action`, by where it is:
/// for what it is (`kind_refusal`), a folder that must stay as it is, a
/// known folder itself or a folder holding one (wherever it was redirected).
fn place_refusal(ctx: &Ctx, action: &str, path: &str, is_dir: bool) -> Result<(), String> {
    if let Some(why) = kind_refusal(action, path, is_dir) {
        return Err(why.into());
    }
    if ctx.guards.why(Path::new(path)).is_some() {
        return Err(PROTECTED.into());
    }
    if action != "compress" && holds_known(ctx, path) {
        return Err(if ctx.known.contains(&key(path)) { KNOWN_FOLDER } else { HOLDS_KNOWN }.into());
    }
    Ok(())
}

/// Fails when a folder between `root` and `path` (both excluded) is a
/// junction or symbolic link: the step would reach out of the plan's folder.
fn links_between(root: &str, path: &str) -> Result<(), String> {
    let mut at = parent_of(path);
    while let Some(dir) = at {
        if !below(dir, root) {
            break;
        }
        if win::reparse_tag(Path::new(dir)).is_some_and(is_link_tag) {
            return Err(LINK_ON_PATH.into());
        }
        at = parent_of(dir);
    }
    Ok(())
}

/// What the earlier steps of a draft leave behind, which counts when a later
/// step is checked: what they make (`key` → is a folder), and what they move
/// (from → to), so an item in a folder an earlier step moves is looked for
/// where it is on disk now (`B\x` after "move A → B" is `A\x`).
#[derive(Default)]
struct Made {
    made: HashMap<String, bool>,
    moved: Vec<(String, String)>,
}

impl Made {
    /// Whether an earlier step makes `path`, and whether as a folder.
    fn get(&self, path: &str) -> Option<bool> {
        self.made.get(&key(path)).copied()
    }

    /// Where on disk the item the earlier steps leave at `path` is now, when
    /// they move a folder above it; None when none does.
    fn on_disk(&self, path: &str) -> Option<String> {
        let mut at = path.to_string();
        let mut moved = false;
        for (from, to) in self.moved.iter().rev() {
            if below(&at, to) {
                let rest: Vec<&str> = at.split('\\').skip(to.split('\\').count()).collect();
                at = format!("{from}\\{}", rest.join("\\"));
                moved = true;
            }
        }
        moved.then_some(at)
    }
}

/// Checks one step: on a draft with `made` (what earlier steps make counts
/// as there), or right before it runs with None (the disk alone). Gives
/// what the step makes (path, is a folder), for the checks of later steps.
fn check(ctx: &Ctx, root: &str, step: &PlanStep, made: Option<&Made>) -> Result<Option<(String, bool)>, String> {
    let root = norm(root).ok_or(BAD_ROOT)?;
    let from = norm(&step.from).ok_or(BAD_PATH)?;
    if is_drive_root(&from) {
        return Err(DRIVE_ROOT.into());
    }
    if !below(&from, &root) {
        return Err(OUTSIDE_ROOT.into());
    }
    match step.action.as_str() {
        "mkdir" => {
            check_target(ctx, &root, None, &from, "mkdir", true, made)?;
            Ok(Some((from, true)))
        }
        "move" | "rename" => {
            let (is_dir, _) = check_source(ctx, &root, &from, &step.action, made)?;
            let to = step.to.as_deref().and_then(norm).ok_or(NO_TARGET)?;
            if step.action == "rename" && parent_of(&to).map(key) != parent_of(&from).map(key) {
                return Err(NOT_SAME_FOLDER.into());
            }
            check_target(ctx, &root, Some(&from), &to, &step.action, is_dir, made)?;
            Ok(Some((to, is_dir)))
        }
        "trash" => {
            let (_, at) = check_source(ctx, &root, &from, "trash", made)?;
            // What an earlier step of a draft makes is not there yet to ask about.
            if made.and_then(|m| m.get(&from)).is_none() {
                recycle_refusal(&at, None)?;
            }
            Ok(None)
        }
        "compress" => {
            check_source(ctx, &root, &from, "compress", made)?;
            if !on_ntfs(&from) {
                return Err(NOT_NTFS.into());
            }
            Ok(None)
        }
        "relocate" => {
            let (is_dir, at) = check_source(ctx, &root, &from, "relocate", made)?;
            if !is_dir {
                return Err(ONLY_FOLDERS.into());
            }
            let to = step.to.as_deref().and_then(norm).ok_or(NO_TARGET)?;
            if key(&to[..2]) == key(&from[..2]) {
                return Err(SAME_DRIVE.into());
            }
            long_names(&to)?;
            if let Some(why) = ops::relocation_refused(Path::new(&at), Path::new(&to)) {
                return Err(why);
            }
            let need = step.size.saturating_add(RELOCATE_MARGIN);
            match win::free_space(Path::new(&to)) {
                Some(free) if free >= need => Ok(None),
                free => Err(format!(
                    "{} 드라이브에 공간이 부족해요 ({} 필요, {} 남음)",
                    &to[..2],
                    size_text(need),
                    size_text(free.unwrap_or(0))
                )),
            }
        }
        _ => Err(UNKNOWN_ACTION.into()),
    }
}

/// Checks the item a step works on: where it is, that it is there (or made
/// by an earlier step of a draft, or in a folder one moves) and is no link,
/// cloud file or system file, and that no name on its way is a short (8.3)
/// one. Gives whether it is a folder, and where it is on disk now (`from`,
/// but for an item in a folder an earlier step of a draft moves).
fn check_source(ctx: &Ctx, root: &str, from: &str, action: &str, made: Option<&Made>) -> Result<(bool, String), String> {
    let (meta, at) = match meta_of(from) {
        Ok(m) => (Some(m), from.to_string()),
        Err(_) => match made {
            Some(m) if m.get(from).is_some() => (None, from.to_string()),
            Some(m) => match m.on_disk(from) {
                Some(old) => (Some(meta_of(&old).map_err(|_| MISSING)?), old),
                None => return Err(MISSING.into()),
            },
            None => return Err(MISSING.into()),
        },
    };
    let is_dir = meta.as_ref().map_or_else(|| made.and_then(|m| m.get(from)).unwrap_or(false), Metadata::is_dir);
    long_names(&at)?;
    // Where it is now counts as much as how the step names it.
    for p in [from, at.as_str()] {
        place_refusal(ctx, action, p, is_dir)?;
        links_between(root, p)?;
    }
    if let Some(m) = meta {
        let attrs = m.file_attributes();
        // A file WOF compressed is a reparse point too, but a plain file all the same.
        let reparse = attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0;
        let wof = || !is_dir && !m.file_type().is_symlink() && !crate::preview::is_online_only(attrs) && pack::is_wof(Path::new(&at));
        if crate::preview::is_online_only(attrs) || (reparse && !wof()) {
            return Err(LINK_ITEM.into());
        }
        if attrs & FILE_ATTRIBUTE_SYSTEM != 0 {
            return Err(SYSTEM_ATTR.into());
        }
    }
    Ok((is_dir, at))
}

/// Checks where a step puts something (`to`, from `from`, a folder if
/// `is_dir`): inside the plan's folder, in a folder that is there (or made
/// by an earlier step), a name that is free and Windows keeps as it is.
fn check_target(ctx: &Ctx, root: &str, from: Option<&str>, to: &str, action: &str, is_dir: bool, made: Option<&Made>) -> Result<(), String> {
    if is_drive_root(to) {
        return Err(DRIVE_ROOT.into());
    }
    if !below(to, root) {
        return Err(OUTSIDE_ROOT.into());
    }
    if let Some(from) = from {
        // A case-only rename (a → A) is a change; a move needs another place.
        if to == from || (action != "rename" && key(to) == key(from)) {
            return Err(SAME_PLACE.into());
        }
        if below(to, from) {
            return Err(INTO_ITSELF.into());
        }
    }
    let name = name_of(to);
    match ops::check_name(name) {
        Ok(kept) if kept == name => {}
        Ok(_) => return Err(BAD_PATH.into()),
        Err(why) => return Err(why.into()),
    }
    links_between(root, to)?;
    long_names(to)?;
    let parent = parent_of(to).ok_or(BAD_PATH)?;
    let plain_dir = |m: &Metadata| m.is_dir() && m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0;
    // A folder of its own; the plan's folder may be reached through a junction.
    let parent_there = match meta_of(parent) {
        Ok(_) if key(parent) == key(root) => std::fs::metadata(win::verbatim(Path::new(parent))).is_ok_and(|m| m.is_dir()),
        Ok(m) => plain_dir(&m),
        Err(_) => made.is_some_and(|m| match m.get(parent) {
            Some(is_dir) => is_dir,
            None => m.on_disk(parent).is_some_and(|old| meta_of(&old).is_ok_and(|m| plain_dir(&m))),
        }),
    };
    if !parent_there {
        return Err(NO_PARENT.into());
    }
    // Never over anything: the name must be free (a case-only rename is the same item).
    let same_item = from.is_some_and(|f| action == "rename" && key(f) == key(to));
    let taken = match meta_of(to) {
        Ok(_) => !from.is_some_and(|f| action == "rename" && win::same_entry(Path::new(f), Path::new(to))),
        Err(_) => made.is_some_and(|m| m.get(to).is_some() || (!same_item && m.on_disk(to).is_some_and(|old| exists(Path::new(&old))))),
    };
    if taken {
        return Err(NAME_TAKEN.into());
    }
    place_refusal(ctx, action, to, is_dir)?;
    if ctx.guards.why_into(Path::new(parent)).is_some() {
        return Err(PROTECTED.into());
    }
    Ok(())
}

/// Why a duplicate may not go to the Recycle Bin for what the copy that
/// stays is now, without reading either: that copy must be there, as a
/// plain file (no folder, link or online-only file), another one than this,
/// and both still the size they had. `same_content` reads them.
fn twin_refusal(from: &str, keep: &Keep) -> Result<(), String> {
    let kept = norm(&keep.path).filter(|p| !is_drive_root(p)).ok_or(TWIN_GONE)?;
    if key(&kept) == key(from) {
        return Err(TWIN_SAME.into());
    }
    let m = meta_of(&kept).map_err(|_| TWIN_GONE)?;
    let attrs = m.file_attributes();
    let wof = || !m.file_type().is_symlink() && pack::is_wof(Path::new(&kept));
    if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 || crate::preview::is_online_only(attrs) || (attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 && !wof()) {
        return Err(TWIN_ODD.into());
    }
    if m.len() != keep.size || meta_of(from).is_ok_and(|f| f.len() != keep.size) {
        return Err(TWIN_CHANGED.into());
    }
    // One file under two paths (a junction above one of them): it would go too.
    if win::same_entry(Path::new(from), Path::new(&kept)) {
        return Err(TWIN_SAME.into());
    }
    Ok(())
}

/// Whether every Recycle Bin step of `plan` is a duplicate whose copy that
/// stays is written down (`PlanFile.keeps`): a dedupe plan rules made. One
/// without that entry (a file from an earlier build, say) does not run.
fn needs_keeps(plan: &Plan) -> bool {
    plan.kind == "dedupe" && plan.source == "rules"
}

/// Whether another step of this plan takes the copy that stays (or a folder
/// above it) away: one that ran, or one selected to run.
fn twin_taken(steps: &[PlanStep], me: u32, keep: &Keep) -> bool {
    steps.iter().any(|o| {
        o.id != me
            && matches!(o.action.as_str(), "move" | "rename" | "trash" | "relocate")
            && (o.state == "done" || (o.selected && o.state == "pending"))
            && (key(&o.from) == key(&keep.path) || below(&keep.path, &o.from))
    })
}

/// Right before a duplicate goes to the Recycle Bin: it and the copy that
/// stays still hold what the plan found in both (blake3 of all of each).
/// `seen` keeps the hashes of copies that stay read in this run, by path
/// with their size and time, for their other duplicates.
fn same_content(from: &str, keep: &Keep, seen: &mut HashMap<String, (u64, u64, String)>) -> Result<(), String> {
    meta_of(from).map_err(|_| MISSING)?;
    twin_refusal(from, keep)?;
    let path = norm(&keep.path).ok_or(TWIN_GONE)?;
    let m = meta_of(&path).map_err(|_| TWIN_GONE)?;
    let stamp = (m.len(), m.last_write_time());
    let kept = match seen.get(&key(&path)) {
        Some((len, time, hash)) if (*len, *time) == stamp => hash.clone(),
        _ => {
            let hash = content_hash(&path)?;
            seen.insert(key(&path), (stamp.0, stamp.1, hash.clone()));
            hash
        }
    };
    if kept != keep.hash || content_hash(from)? != keep.hash {
        return Err(TWIN_CHANGED.into());
    }
    Ok(())
}

/// blake3 of all of a file, in hex (as the duplicate search gives it). It is
/// opened so that nothing writes to it meanwhile; an online-only file is
/// never read (that would fetch it).
fn content_hash(path: &str) -> Result<String, String> {
    use io::Read;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_SEQUENTIAL_SCAN;
    let p = win::verbatim(Path::new(path));
    let m = std::fs::symlink_metadata(&p).map_err(|_| TWIN_GONE)?;
    if crate::preview::is_online_only(m.file_attributes()) {
        return Err(TWIN_ODD.into());
    }
    let mut file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN)
        .open(&p)
        .map_err(|_| TWIN_UNREAD)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; HASH_BUF];
    loop {
        let n = file.read(&mut buf).map_err(|_| TWIN_UNREAD)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Why `path` may not go to the Recycle Bin, if so: the checks `trash_paths`
/// makes (lib.rs), on the path as given and where it really is (the shell
/// resolves the junctions above it). `size` (when known) must fit in the bin.
/// Gives where it really is.
fn recycle_refusal(path: &str, size: Option<u64>) -> Result<String, String> {
    let p = Path::new(path);
    if !win::shell_can_name(p) {
        return Err(ODD_NAME.into());
    }
    let real = p
        .parent()
        .and_then(|parent| std::fs::canonicalize(win::verbatim(parent)).ok())
        .zip(p.file_name())
        .map(|(parent, name)| win::long_path(&parent.join(name).to_string_lossy()))
        .ok_or(NOT_RECYCLED)?;
    let mut limit: Option<u64> = None;
    for at in [path, real.as_str()] {
        if !win::has_recycle_bin(at) {
            return Err(NO_RECYCLE_BIN.into());
        }
        // On a disk that has a Recycle Bin, Windows still deletes for good (after
        // its own prompt) when the bin is switched off or the item is bigger than the bin.
        let policy = win::recycle_policy(at);
        if policy.nuke {
            return Err(BIN_DISABLED.into());
        }
        if let Some(max) = policy.max_bytes {
            limit = Some(limit.map_or(max, |l| l.min(max)));
        }
    }
    if let (Some(size), Some(max)) = (size, limit)
        && size > max
    {
        return Err(format!("{BIN_TOO_SMALL} (휴지통 최대 {}MB)", max >> 20));
    }
    Ok(real)
}

/// Whether `path` is on an NTFS drive, which can compress files.
fn on_ntfs(path: &str) -> bool {
    win::drive(&path[..2]).is_some_and(|d| d.fs.eq_ignore_ascii_case("NTFS"))
}

/// Checks every step of a draft again (with the copies its duplicates leave,
/// `keeps`): one that holds is "pending", one that does not is "skipped"
/// with why, and cannot stay selected.
fn review(plan: &mut Plan, ctx: &Ctx, keeps: &[Keep]) {
    let mut made = Made::default();
    let twins = needs_keeps(plan);
    for s in &mut plan.steps {
        let checked = check(ctx, &plan.root, s, Some(&made)).and_then(|output| {
            match keeps.iter().find(|k| k.step == s.id) {
                Some(keep) if s.action == "trash" => twin_refusal(&s.from, keep).map(|()| output),
                // No copy that stays to look at: the duplicate stays too.
                None if twins && s.action == "trash" => Err(KEEP_UNKNOWN.into()),
                _ => Ok(output),
            }
        });
        match checked {
            Ok(output) => {
                s.state = "pending".into();
                s.error = None;
                if let Some((path, is_dir)) = output {
                    if matches!(s.action.as_str(), "move" | "rename")
                        && let Some(from) = norm(&s.from)
                    {
                        made.moved.push((from, path.clone()));
                    }
                    made.made.insert(key(&path), is_dir);
                }
            }
            Err(why) => {
                s.state = "skipped".into();
                s.error = Some(why);
                s.selected = false;
            }
        }
    }
}

const KINDS: &[&str] = &["structure", "rename", "dedupe", "shrink", "relocate", "cleanup"];
const SOURCES: &[&str] = &["rules", "claude", "openai", "ollama", "claude_cli", "codex_cli", "mcp"];
const ACTIONS: &[&str] = &["mkdir", "move", "rename", "trash", "compress", "relocate"];
const RISKS: &[&str] = &["safe", "caution", "danger"];

fn invalid(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, why.to_string())
}

/// A plan id is what `new_id` makes: ASCII letters, digits, `-` and `_`, so
/// it is a file name of its own and never a path.
fn check_id(id: &str) -> io::Result<()> {
    let fine = !id.is_empty()
        && id.len() <= 64
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        && ops::check_name(&format!("{id}.json")).is_ok();
    if fine { Ok(()) } else { Err(invalid(BAD_ID)) }
}

/// What a plan from outside (AI, MCP) may do: what rules alone make
/// (compression, relocation to another drive) is never taken from it.
const UNTRUSTED_ACTIONS: &[&str] = &["mkdir", "move", "rename", "trash"];

/// A plan from outside (AI, MCP) brought into shape: kind, source, root (a
/// folder, never a whole drive) and every step's action and paths must be
/// valid; a step that is not is dropped (and the summary says why), and past
/// MAX_STEPS they are too. Every step starts pending; a Recycle Bin step, and
/// one its maker calls dangerous, unselected. Text is clipped.
fn admit(mut plan: Plan) -> io::Result<Plan> {
    check_id(&plan.id)?;
    if !KINDS.contains(&plan.kind.as_str()) {
        return Err(invalid(BAD_KIND));
    }
    if !SOURCES.contains(&plan.source.as_str()) {
        return Err(invalid(BAD_SOURCE));
    }
    // Rules write their plans themselves; one that says it is theirs is not believed.
    if plan.source == "rules" {
        return Err(invalid(RULES_ONLY));
    }
    plan.root = norm(&plan.root).ok_or_else(|| invalid(BAD_ROOT))?;
    if is_drive_root(&plan.root) {
        return Err(invalid(DRIVE_ROOT_PLAN));
    }
    plan.title = clip(&plan.title, 120);
    plan.summary = clip(&plan.summary, 2000);
    plan.state = "draft".into();
    if plan.created_at <= 0 {
        plan.created_at = now_filetime();
    }
    let (mut kept, mut dropped, mut over) = (Vec::new(), Vec::new(), 0usize);
    for mut s in std::mem::take(&mut plan.steps) {
        if kept.len() >= MAX_STEPS {
            over += 1;
            continue;
        }
        let shaped = shape(&mut s).and_then(|()| if UNTRUSTED_ACTIONS.contains(&s.action.as_str()) { Ok(()) } else { Err(UNTRUSTED_ACTION) });
        if let Err(why) = shaped {
            dropped.push(format!("{} ({why})", clip(&s.from, 120)));
            continue;
        }
        s.state = "pending".into();
        s.error = None;
        s.reason = clip(&s.reason, 500);
        if !RISKS.contains(&s.risk.as_str()) {
            s.risk = "caution".into();
        }
        // What goes to the Recycle Bin is the user's pick, never AI's or MCP's;
        // so is what they call dangerous themselves.
        if s.action == "trash" || s.risk == "danger" {
            s.selected = false;
        }
        kept.push(s);
    }
    let mut seen = HashSet::new();
    if kept.iter().any(|s| s.id == 0 || !seen.insert(s.id)) {
        for (s, id) in kept.iter_mut().zip(1..) {
            s.id = id;
        }
    }
    plan.steps = kept;
    if !dropped.is_empty() {
        let more = if dropped.len() > 5 { " 외" } else { "" };
        let list: Vec<&str> = dropped.iter().take(5).map(String::as_str).collect();
        plan.summary.push_str(&format!("\n\n올바르지 않아서 뺀 단계 {}개: {}{more}", dropped.len(), list.join("; ")));
    }
    if over > 0 {
        plan.summary.push_str(&format!("\n\n단계가 너무 많아서 앞의 {MAX_STEPS}개만 남겼어요 ({over}개 뺌)"));
    }
    Ok(plan)
}

/// A step's action and paths, normalized; why it is no valid step otherwise.
fn shape(s: &mut PlanStep) -> Result<(), &'static str> {
    if !ACTIONS.contains(&s.action.as_str()) {
        return Err(UNKNOWN_ACTION);
    }
    s.from = norm(&s.from).ok_or(BAD_PATH)?;
    if matches!(s.action.as_str(), "move" | "rename" | "relocate") {
        let to = s.to.as_deref().ok_or(NO_TARGET)?;
        s.to = Some(norm(to).ok_or(BAD_PATH)?);
    } else {
        s.to = None;
    }
    Ok(())
}

// ---- Store ------------------------------------------------------------------------

/// Where plans are kept: `<data dir>\plans` (tests keep their own).
#[derive(Clone)]
struct Store(PathBuf);

/// Changes to plan files take turns (within this process; the MCP server
/// only ever adds new plans).
static STORE_TURN: Mutex<()> = Mutex::new(());

fn store_turn() -> MutexGuard<'static, ()> {
    STORE_TURN.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Store {
    fn app() -> Store {
        Store(crate::scan::data_dir().join("plans"))
    }

    fn file(&self, id: &str) -> io::Result<PathBuf> {
        check_id(id)?;
        Ok(self.0.join(format!("{id}.json")))
    }

    fn lock_path(&self, id: &str) -> PathBuf {
        self.0.join(format!("{id}.lock"))
    }

    fn read(&self, id: &str) -> io::Result<PlanFile> {
        let bytes = std::fs::read(self.file(id)?).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => io::Error::new(io::ErrorKind::NotFound, NOT_FOUND),
            _ => io::Error::new(e.kind(), format!("{BAD_FILE}: {e}")),
        })?;
        let file: PlanFile = serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{BAD_FILE}: {e}")))?;
        // The name is the id: a file that says otherwise is not believed.
        if file.plan.id != id {
            return Err(io::Error::new(io::ErrorKind::InvalidData, BAD_FILE));
        }
        Ok(file)
    }

    /// Writes a plan file atomically (temp file, flush, rename).
    fn write(&self, file: &PlanFile) -> io::Result<()> {
        let path = self.file(&file.plan.id)?;
        let bytes = serde_json::to_vec_pretty(file).map_err(io::Error::other)?;
        crate::index::write_cache(&path, &bytes)
    }

    /// A plan left "running" by a process that is gone (a crash, a kill, a
    /// power cut) is shown as what it came to: partly done while some done
    /// step is not undone, undone when all are, failed when none was done.
    /// Its journal is in the file, so it can still be undone. True when changed.
    fn settle(&self, file: &mut PlanFile) -> bool {
        if file.plan.state != "running" || is_running(self, &file.plan.id) {
            return false;
        }
        file.plan.state = finished_state(file).into();
        true
    }

    fn load(&self, id: &str) -> io::Result<PlanFile> {
        let _turn = store_turn();
        let mut file = self.read(id)?;
        if self.settle(&mut file) {
            let _ = self.write(&file);
        }
        Ok(file)
    }

    fn list(&self) -> Vec<Plan> {
        let _turn = store_turn();
        let Ok(read) = std::fs::read_dir(&self.0) else { return Vec::new() };
        let mut plans: Vec<Plan> = read
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.strip_suffix(".json").map(str::to_string))
            .filter_map(|id| {
                let mut file = self.read(&id).ok()?;
                if self.settle(&mut file) {
                    let _ = self.write(&file);
                }
                Some(file.plan)
            })
            .collect();
        plans.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| b.id.cmp(&a.id)));
        plans
    }

    /// `store::save`: an untrusted draft, checked (`admit`, `review`), written
    /// in place of a draft of that id or as a new plan. Gives it as stored.
    fn save(&self, plan: &Plan) -> io::Result<Plan> {
        let mut plan = admit(plan.clone())?;
        review(&mut plan, &Ctx::now(), &[]);
        let _turn = store_turn();
        match self.read(&plan.id) {
            Ok(old) if old.plan.state != "draft" => return Err(io::Error::other(NOT_DRAFT)),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            // A file that cannot be read is not written over: it may hold a journal.
            Err(e) => return Err(e),
        }
        self.write(&PlanFile::new(plan.clone()))?;
        Ok(plan)
    }

    /// Changes a plan file and writes it back, in turn with other changes.
    fn change<T>(&self, id: &str, f: impl FnOnce(&mut PlanFile) -> io::Result<T>) -> io::Result<T> {
        let _turn = store_turn();
        let mut file = self.read(id)?;
        self.settle(&mut file);
        let out = f(&mut file)?;
        self.write(&file)?;
        Ok(out)
    }

    /// `plan_update`: only which steps are selected and where a move, rename
    /// or relocation goes are taken from `edited`; then every step is checked again.
    fn update(&self, edited: &Plan) -> io::Result<Plan> {
        let ctx = Ctx::now();
        self.change(&edited.id, |file| {
            if file.plan.state != "draft" {
                return Err(io::Error::other(NOT_DRAFT));
            }
            for s in &mut file.plan.steps {
                let Some(e) = edited.steps.iter().find(|e| e.id == s.id) else { continue };
                s.selected = e.selected;
                if matches!(s.action.as_str(), "move" | "rename" | "relocate")
                    && let Some(to) = &e.to
                {
                    // Kept as typed when it is no path, so the check says so.
                    s.to = Some(norm(to).unwrap_or_else(|| clip(to, 1024)));
                }
            }
            review(&mut file.plan, &ctx, &file.keeps);
            Ok(file.plan.clone())
        })
    }

    fn remove(&self, id: &str) -> io::Result<()> {
        let _turn = store_turn();
        self.read(id)?;
        // One left "running" by a process that is gone may go.
        if is_running(self, id) {
            return Err(io::Error::other(RUNNING));
        }
        std::fs::remove_file(self.file(id)?)?;
        let _ = std::fs::remove_file(self.lock_path(id));
        Ok(())
    }
}

/// The state a run (or an undo) ends in, by its journal.
fn finished_state(file: &PlanFile) -> &'static str {
    let left = file.journal.iter().filter(|d| !d.undone).count();
    if file.journal.is_empty() {
        "failed"
    } else if left == 0 {
        "undone"
    } else {
        "partly"
    }
}

// ---- One plan at a time -----------------------------------------------------------

/// The plan this process runs or undoes now...
static TURN: Mutex<Option<String>> = Mutex::new(None);
/// ...told when it is done with it.
static TURN_ENDED: Condvar = Condvar::new();
/// Set while the app closes: a run (or undo) starts no other step, and the
/// compression of a folder stops between files. Cleared only when the app
/// did not end after all (`reopen`: an update that was not installed).
static CLOSING_NOW: AtomicBool = AtomicBool::new(false);
/// `reopen` came while the plan told to stop still ran: the stop is lifted
/// once that run is over (`Turn`'s drop), unless the app is closed again
/// first (`stop_and_wait`). Changed only with `TURN` held.
static REOPEN_WHEN_DONE: AtomicBool = AtomicBool::new(false);

fn turn_now() -> MutexGuard<'static, Option<String>> {
    TURN.lock().unwrap_or_else(PoisonError::into_inner)
}

fn closing() -> bool {
    CLOSING_NOW.load(Ordering::SeqCst)
}

/// Whether this process runs or undoes a plan now (lib.rs asks before closing).
pub(crate) fn running() -> bool {
    turn_now().is_some()
}

/// The plan this process runs or undoes now, if one (lib.rs: which the user agreed to stop).
pub(crate) fn running_id() -> Option<String> {
    turn_now().clone()
}

/// Whether another of the app's processes runs or undoes a plan now (its
/// `run.lock`, `Turn`). Asked before an update, whose installer would end it.
pub(crate) fn running_elsewhere() -> bool {
    let turn = turn_now();
    turn.is_none() && held_elsewhere(&Store::app().0.join("run.lock"))
}

/// The app closes: the plan that runs (or is undone) finishes the step it
/// is at, writes down how to undo it and stops there. Waits at most `limit`
/// for that; true when no plan runs any more.
pub(crate) fn stop_and_wait(limit: Duration) -> bool {
    let turn = turn_now();
    CLOSING_NOW.store(true, Ordering::SeqCst);
    // A closing that comes after a `reopen` holds, also past the run at hand.
    REOPEN_WHEN_DONE.store(false, Ordering::SeqCst);
    let (turn, _) = TURN_ENDED.wait_timeout_while(turn, limit, |t| t.is_some()).unwrap_or_else(PoisonError::into_inner);
    turn.is_none()
}

/// The app did not close after all (an update whose installer did not
/// start, or that gave up waiting for a step, lib.rs): plans may run again.
/// Only once no plan runs, so a run that was told to stop never goes on to
/// its next step: while one still runs, the stop is lifted when it is over.
pub(crate) fn reopen() {
    let turn = turn_now();
    if turn.is_none() {
        CLOSING_NOW.store(false, Ordering::SeqCst);
    } else {
        REOPEN_WHEN_DONE.store(true, Ordering::SeqCst);
    }
}

/// The turn to run or undo one plan: here (`TURN`), and among the app's
/// processes by holding `run.lock` in the plans folder open for nobody else.
/// `<id>.lock`, held the same way, tells the others which plan runs; Windows
/// closes both when a process ends, by a crash too.
struct Turn {
    all: Option<File>,
    one: Option<File>,
    one_path: PathBuf,
}

/// Opens `path` for nobody else.
fn hold_exclusive(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).write(true).create(true).truncate(false).share_mode(0).open(path)
}

/// Whether another process holds the lock file `path` (`hold_exclusive`).
fn held_elsewhere(path: &Path) -> bool {
    let opened = OpenOptions::new().read(true).share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).open(path);
    matches!(opened, Err(e) if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32))
}

fn is_running(store: &Store, id: &str) -> bool {
    turn_now().as_deref() == Some(id) || held_elsewhere(&store.lock_path(id))
}

impl Turn {
    fn take(store: &Store, id: &str) -> Result<Turn, String> {
        let mut turn = turn_now();
        if turn.is_some() {
            return Err(BUSY.into());
        }
        let _ = std::fs::create_dir_all(&store.0);
        let all = match hold_exclusive(&store.0.join("run.lock")) {
            Ok(f) => Some(f),
            Err(e) if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32) => return Err(BUSY.into()),
            // No room for even an empty file: the turn in this process still holds.
            Err(_) => None,
        };
        let one_path = store.lock_path(id);
        let one = hold_exclusive(&one_path).ok();
        *turn = Some(id.to_string());
        Ok(Turn { all, one, one_path })
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        if self.one.take().is_some() {
            let _ = std::fs::remove_file(&self.one_path);
        }
        self.all.take();
        let mut turn = turn_now();
        *turn = None;
        if REOPEN_WHEN_DONE.swap(false, Ordering::SeqCst) {
            CLOSING_NOW.store(false, Ordering::SeqCst);
        }
        drop(turn);
        TURN_ENDED.notify_all();
    }
}

// ---- Running ----------------------------------------------------------------------

/// Where a run reports, and what of the app it uses: the window, or a test.
trait Events: Send + Sync + 'static {
    fn progress(&self, p: &PlanProgress);
    /// Where the jobs a step starts (ops) report: the window's jobs panel.
    fn jobs(&self) -> Box<dyn ops::Events>;
    /// The size of an item as the index has it (a folder: all that is in it).
    fn indexed_size(&self, path: &str) -> Option<u64>;
}

impl Events for AppHandle {
    fn progress(&self, p: &PlanProgress) {
        let _ = self.emit("plan-progress", p);
    }

    fn jobs(&self) -> Box<dyn ops::Events> {
        Box::new(self.clone())
    }

    fn indexed_size(&self, path: &str) -> Option<u64> {
        let drive = crate::drive_of(path)?;
        let shared = self.try_state::<crate::AppState>()?.get(&drive)?;
        let ix = shared.read().unwrap_or_else(PoisonError::into_inner);
        ix.find(path).map(|n| ix.nodes[n as usize].size)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Work {
    /// The selected steps of a draft.
    Execute,
    /// Every done step, last first.
    Undo,
}

/// Takes the one-plan turn and marks the plan running, if it may run (or be undone).
fn begin(store: &Store, id: &str, work: Work) -> Result<Turn, String> {
    // A plan a gone process left "running" is settled first.
    store.load(id).map_err(|e| e.to_string())?;
    let turn = Turn::take(store, id)?;
    store
        .change(id, |file| {
            let may = match work {
                Work::Execute if file.plan.state != "draft" => Err(NOT_DRAFT),
                Work::Execute if !file.plan.steps.iter().any(|s| s.selected && s.state == "pending") => Err(NOTHING_SELECTED),
                Work::Undo if file.plan.state == "draft" || file.journal.iter().all(|d| d.undone) => Err(NOTHING_TO_UNDO),
                _ => Ok(()),
            };
            may.map_err(io::Error::other)?;
            file.plan.state = "running".into();
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    Ok(turn)
}

/// `begin`, then the run on a thread of its own.
fn launch(store: Store, id: String, work: Work, events: impl Events) -> Result<(), String> {
    let turn = begin(&store, &id, work)?;
    let (s, i) = (store.clone(), id.clone());
    let spawned = std::thread::Builder::new().name(format!("plan {id}")).spawn(move || {
        let _turn = turn;
        go(&s, &i, work, &events);
    });
    if let Err(e) = spawned {
        // The turn went with the closure: the plan is as it was.
        let _ = store.change(&id, |file| {
            file.plan.state = if work == Work::Execute { "draft".into() } else { finished_state(file).into() };
            Ok(())
        });
        return Err(format!("계획을 시작할 수 없어요: {e}"));
    }
    Ok(())
}

/// Runs (or undoes) a plan `begin` marked running, to its end.
fn go(store: &Store, id: &str, work: Work, events: &dyn Events) {
    let worked = catch_unwind(AssertUnwindSafe(|| match work {
        Work::Execute => execute(store, id, events),
        Work::Undo => undo(store, id, events),
    }));
    if worked.is_err() {
        // Whatever the journal has, the plan is not left running.
        let _ = store.change(id, |file| {
            file.plan.state = finished_state(file).into();
            Ok(())
        });
        events.progress(&PlanProgress { id: id.into(), state: "failed".into(), done: 0, total: 0, current: None });
    }
}

/// Progress of a run, sent at most every PROGRESS_GAP (state changes always).
struct Progress {
    id: String,
    done: u32,
    total: u32,
    current: Option<String>,
    last: Option<Instant>,
}

impl Progress {
    fn new(id: &str, total: u32) -> Progress {
        Progress { id: id.into(), done: 0, total, current: None, last: None }
    }

    fn send(&mut self, events: &dyn Events, state: &str, now: bool) {
        if !now && self.last.is_some_and(|t| t.elapsed() < PROGRESS_GAP) {
            return;
        }
        self.last = Some(Instant::now());
        events.progress(&PlanProgress {
            id: self.id.clone(),
            state: state.into(),
            done: self.done,
            total: self.total,
            current: self.current.clone(),
        });
    }

    fn at(&mut self, events: &dyn Events, path: &Path) {
        self.current = Some(path.to_string_lossy().into_owned());
        self.send(events, "running", false);
    }
}

/// What running a step came to.
enum Ran {
    /// Done; how to undo it, and a note when something needs a look.
    Done(Inverse, Option<String>),
    /// There was nothing to do, and why.
    Nothing(String),
}

/// Writes a run's plan file, trying again while it cannot be written (a full
/// disk, a scanner holding the file): after JOURNAL_GAP, then twice as long
/// each time up to JOURNAL_GAP_MOST, telling the window why meanwhile.
/// Gives up after `patience` (false then); with None it keeps trying for as
/// long as the process lives, since what it holds is how to undo what ran.
fn persist(store: &Store, file: &PlanFile, progress: &mut Progress, events: &dyn Events, patience: Option<Duration>) -> bool {
    let start = Instant::now();
    let mut gap = JOURNAL_GAP;
    loop {
        let e = match store.write(file) {
            Ok(()) => return true,
            Err(e) => e,
        };
        if patience.is_some_and(|p| start.elapsed() >= p) {
            return false;
        }
        progress.current = Some(format!("{JOURNAL_WAIT} ({e})"));
        progress.send(events, "running", true);
        std::thread::sleep(gap);
        gap = (gap * 2).min(JOURNAL_GAP_MOST);
    }
}

fn execute(store: &Store, id: &str, events: &dyn Events) {
    let Ok(mut file) = store.read(id) else {
        // Nothing ran; the file is settled as "failed" once the turn is let go.
        events.progress(&PlanProgress { id: id.into(), state: "failed".into(), done: 0, total: 0, current: None });
        return;
    };
    let ctx = Ctx::now();
    let total = file.plan.steps.iter().filter(|s| s.selected && s.state == "pending").count() as u32;
    let mut progress = Progress::new(id, total);
    progress.send(events, "running", true);
    let (mut done, mut failed, mut held) = (0u32, 0u32, 0u32);
    let mut saved = Instant::now();
    // Why the steps not run yet stay so: the app closes, or what ran could not be written down.
    let mut halt: Option<&'static str> = None;
    // What this run read of the copies duplicates leave.
    let mut seen = HashMap::new();
    for i in 0..file.plan.steps.len() {
        let step = file.plan.steps[i].clone();
        if step.state != "pending" {
            continue;
        }
        if !step.selected {
            file.plan.steps[i].state = "skipped".into();
            continue;
        }
        if halt.is_none() && closing() {
            halt = Some(CLOSING);
        }
        if let Some(why) = halt {
            let s = &mut file.plan.steps[i];
            s.state = "skipped".into();
            s.error = Some(why.into());
            held += 1;
            continue;
        }
        progress.at(events, Path::new(&step.from));
        let root = file.plan.root.clone();
        let keep = file.keep_of(step.id).filter(|_| step.action == "trash").cloned();
        let taken = keep.as_ref().is_some_and(|k| twin_taken(&file.plan.steps, step.id, k));
        let unknown = keep.is_none() && step.action == "trash" && needs_keeps(&file.plan);
        let ran = catch_unwind(AssertUnwindSafe(|| {
            // A duplicate goes only while the copy that stays is there with the same content.
            if unknown {
                return Err(KEEP_UNKNOWN.to_string());
            }
            if let Some(keep) = &keep {
                if taken {
                    return Ok(Ran::Nothing(TWIN_TAKEN.into()));
                }
                if let Err(why) = same_content(&step.from, keep, &mut seen) {
                    return Ok(Ran::Nothing(why));
                }
            }
            // Checked again right before it runs: the disk may have changed since the draft.
            check(&ctx, &root, &step, None)?;
            run_step(&step, events, &mut |p: &Path| progress.at(events, p))
        }));
        let s = &mut file.plan.steps[i];
        let did = matches!(ran, Ok(Ok(Ran::Done(..))));
        match ran {
            Ok(Ok(Ran::Done(undo, note))) => {
                s.state = "done".into();
                s.error = note;
                file.journal.push(Done { step: step.id, undo, undone: false });
                done += 1;
            }
            Ok(Ok(Ran::Nothing(why))) => {
                s.state = "skipped".into();
                s.error = Some(why);
            }
            Ok(Err(why)) => {
                s.state = "failed".into();
                s.error = Some(why);
                failed += 1;
            }
            Err(_) => {
                s.state = "failed".into();
                s.error = Some(PANICKED.into());
                failed += 1;
            }
        }
        progress.done += 1;
        // How to undo a step is on disk before the next step starts.
        if did {
            if !persist(store, &file, &mut progress, events, Some(JOURNAL_PATIENCE)) {
                halt = Some(JOURNAL_UNSAVED);
            }
            saved = Instant::now();
        } else if saved.elapsed() >= QUIET_SAVE {
            let _ = store.write(&file);
            saved = Instant::now();
        }
        progress.send(events, "running", false);
    }
    file.plan.state = match (done, failed + held) {
        (_, 0) => "done",
        (0, _) => "failed",
        _ => "partly",
    }
    .into();
    // What ran is written down, however long that takes.
    persist(store, &file, &mut progress, events, None);
    progress.current = None;
    progress.send(events, &file.plan.state, true);
}

fn run_step(step: &PlanStep, events: &dyn Events, at: &mut dyn FnMut(&Path)) -> Result<Ran, String> {
    let from = norm(&step.from).ok_or(BAD_PATH)?;
    let to = step.to.as_deref().and_then(norm);
    match (step.action.as_str(), to) {
        ("mkdir", _) => make_folder(&from),
        ("move" | "rename", Some(to)) => {
            let (now_at, note) = move_entry(events, &from, &to)?;
            Ok(Ran::Done(Inverse::MoveBack { from, to: now_at }, note))
        }
        ("trash", _) => recycle(events, &from),
        ("compress", _) => compress(&from, at),
        ("relocate", Some(to)) => relocate(events, &from, &to),
        ("move" | "rename" | "relocate", None) => Err(NO_TARGET.into()),
        _ => Err(UNKNOWN_ACTION.into()),
    }
}

fn make_folder(path: &str) -> Result<Ran, String> {
    std::fs::create_dir(win::verbatim(Path::new(path))).map_err(|e| match e.kind() {
        io::ErrorKind::AlreadyExists => NAME_TAKEN.to_string(),
        _ => format!("폴더를 만들 수 없어요: {e}"),
    })?;
    Ok(Ran::Done(Inverse::Rmdir { path: path.into() }, None))
}

/// Moves (or renames) `from` to `to`, never over anything. Within a volume
/// a rename; to another, ops' move (copy, check, then remove the source)
/// into `to`'s folder, then a rename to `to`'s name if it differs. Gives
/// where the item is now (`to`, or the moved item under its old name when
/// that last rename failed, which the note says).
fn move_entry(events: &dyn Events, from: &str, to: &str) -> Result<(String, Option<String>), String> {
    let (src, dst) = (Path::new(from), Path::new(to));
    let folder = dst.parent().ok_or(BAD_PATH)?;
    if win::same_volume(src, folder) {
        win::rename(src, dst).map_err(|e| match e.kind() {
            io::ErrorKind::AlreadyExists => NAME_TAKEN.to_string(),
            _ => format!("옮기지 못했어요: {e}"),
        })?;
        return Ok((to.into(), None));
    }
    let landed = folder.join(src.file_name().ok_or(BAD_PATH)?);
    if exists(&landed) {
        return Err(NAME_TAKEN.into());
    }
    let job = ops::move_and_wait(events.jobs(), src, folder)?;
    let landed_text = landed.to_string_lossy().into_owned();
    if exists(src) {
        // Not all of it went: what did is at the new place, the rest where it was.
        let why = job.errors.first().map_or_else(|| NAME_TAKEN.to_string(), |(_, why)| why.clone());
        return Err(if exists(&landed) { format!("{why} (일부는 {landed_text}에 옮겨졌고, 나머지는 원래 자리에 있어요)") } else { why });
    }
    if !exists(&landed) {
        return Err(MOVED_LOST.into());
    }
    if landed.file_name() != dst.file_name()
        && let Err(e) = win::rename(&landed, dst)
    {
        return Ok((landed_text.clone(), Some(format!("옮겼지만 이름을 바꾸지 못했어요 ({e}). 지금 이름: {landed_text}"))));
    }
    Ok((to.into(), None))
}

/// The bytes of the files in `path` (a file: its size), links not followed.
fn measure(path: &Path) -> Option<u64> {
    let meta = std::fs::symlink_metadata(win::verbatim(path)).ok()?;
    if !meta.is_dir() {
        return Some(meta.len());
    }
    let mut total = 0u64;
    let mut stack = vec![win::verbatim(path)];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else { continue };
        for entry in read.flatten() {
            let Ok(m) = entry.metadata() else { continue };
            if m.file_type().is_symlink() {
                continue;
            }
            if m.is_dir() {
                stack.push(entry.path());
            } else {
                total += m.len();
            }
        }
    }
    Some(total)
}

/// Sends `path` to the Recycle Bin, only after the checks of `trash_paths`.
fn recycle(events: &dyn Events, path: &str) -> Result<Ran, String> {
    let p = Path::new(path);
    let size = events.indexed_size(path).or_else(|| measure(p));
    let real = recycle_refusal(path, size)?;
    let deleted_from = unix_now();
    trash::delete(p).map_err(|e| format!("{NOT_RECYCLED}: {e}"))?;
    let deleted_to = unix_now();
    if exists(p) {
        return Err(NOT_RECYCLED.into());
    }
    Ok(Ran::Done(Inverse::Restore { path: path.into(), real, deleted_from, deleted_to }, None))
}

/// Names of files a folder's compression leaves alone: they are compressed
/// already, and WOF would read all of each only to find that out.
fn packed_already(ext: &str) -> bool {
    matches!(
        ext,
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "heic" | "avif" | "mp4" | "mkv" | "mov" | "avi" | "webm" | "mp3" | "aac"
            | "m4a" | "ogg" | "opus" | "flac" | "zip" | "7z" | "rar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "cab" | "jar"
            | "apk" | "docx" | "xlsx" | "pptx" | "pdf" | "msi" | "msix"
    )
}

fn compress(path: &str, at: &mut dyn FnMut(&Path)) -> Result<Ran, String> {
    let p = Path::new(path);
    let meta = std::fs::symlink_metadata(win::verbatim(p)).map_err(|_| MISSING)?;
    if !meta.is_dir() {
        return Ok(match pack::compress(p)? {
            Some(wof) => Ran::Done(Inverse::Decompress { files: vec![Packed { path: path.into(), wof }] }, None),
            None => Ran::Nothing(ALREADY_SMALL.into()),
        });
    }
    let (mut packed, mut failed, mut first) = (Vec::new(), 0u64, None::<String>);
    let mut stopped = false;
    let mut stack = vec![p.to_path_buf()];
    'walk: while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(win::verbatim(&dir)) else {
            failed += 1;
            first.get_or_insert_with(|| format!("폴더를 열 수 없어요: {}", dir.display()));
            continue;
        };
        for entry in read.flatten() {
            let Ok(m) = entry.metadata() else { continue };
            let attrs = m.file_attributes();
            // Links and cloud files are left alone; so are Windows' own files.
            if attrs & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_SYSTEM) != 0 || crate::preview::is_online_only(attrs) {
                continue;
            }
            // Not entry.path(): that has the `\\?\` of the listing.
            let file = dir.join(entry.file_name());
            if m.is_dir() {
                stack.push(file);
                continue;
            }
            if m.len() == 0 || packed_already(&ext_of(&entry.file_name().to_string_lossy())) {
                continue;
            }
            if packed.len() >= PACK_MOST {
                first.get_or_insert_with(|| format!("파일이 너무 많아서 앞의 {PACK_MOST}개만 압축했어요"));
                break 'walk;
            }
            // The app closes: what is compressed so far is written down (and can be undone).
            if closing() {
                stopped = true;
                first.get_or_insert_with(|| PACK_STOPPED.into());
                break 'walk;
            }
            at(&file);
            match pack::compress(&file) {
                Ok(Some(wof)) => packed.push(Packed { path: file.to_string_lossy().into_owned(), wof }),
                Ok(None) => {}
                Err(why) => {
                    failed += 1;
                    first.get_or_insert_with(|| format!("{}: {why}", file.display()));
                }
            }
        }
    }
    if packed.is_empty() {
        return match first {
            Some(why) if failed > 0 => Err(why),
            _ if stopped => Ok(Ran::Nothing(PACK_STOPPED.into())),
            _ => Ok(Ran::Nothing(ALREADY_SMALL.into())),
        };
    }
    let note = first.map(|why| if failed > 0 { format!("{failed}개는 압축하지 못했어요 ({why})") } else { why });
    Ok(Ran::Done(Inverse::Decompress { files: packed }, note))
}

fn relocate(events: &dyn Events, from: &str, to: &str) -> Result<Ran, String> {
    let (job, record) = ops::relocate_and_wait(events.jobs(), Path::new(from), Path::new(to))?;
    if job.state != "done" {
        let why = job.errors.first().map(|(_, why)| why.clone());
        return Err(why.unwrap_or_else(|| if job.state == "cancelled" { "취소했어요".into() } else { "옮기지 못했어요".into() }));
    }
    let note = job.errors.first().map(|(_, why)| format!("옮겼지만 확인이 필요해요: {why}"));
    Ok(match record {
        Some(r) => Ran::Done(Inverse::Unrelocate { id: r.id, original: r.original, target: r.target }, note),
        // Moved, but its record could not be found: the relocation list still has it.
        None => Ran::Done(Inverse::Unrelocate { id: String::new(), original: from.into(), target: String::new() }, Some(NO_RELOCATION_RECORD.into())),
    })
}

fn undo(store: &Store, id: &str, events: &dyn Events) {
    let Ok(mut file) = store.read(id) else {
        // Nothing was undone; the file is settled once the turn is let go.
        events.progress(&PlanProgress { id: id.into(), state: "failed".into(), done: 0, total: 0, current: None });
        return;
    };
    let todo: Vec<usize> = (0..file.journal.len()).filter(|&j| !file.journal[j].undone).rev().collect();
    let ctx = Ctx::now();
    let root = file.plan.root.clone();
    let mut progress = Progress::new(id, todo.len() as u32);
    progress.send(events, "running", true);
    // The Recycle Bin is listed once, when the first item is to come back.
    let mut bin: Option<Vec<trash::TrashItem>> = None;
    // Why the steps not undone yet stay so: the app closes, or what was undone could not be written down.
    let mut halt: Option<&'static str> = None;
    for j in todo {
        let mut entry = file.journal[j].clone();
        let step = file.plan.steps.iter().position(|s| s.id == entry.step);
        if halt.is_none() && closing() {
            halt = Some(CLOSING_UNDO);
        }
        if let Some(why) = halt {
            if let Some(i) = step {
                file.plan.steps[i].error = Some(why.into());
            }
            continue;
        }
        if let Some(i) = step {
            progress.at(events, Path::new(&file.plan.steps[i].from));
        }
        let action = step.map_or("", |i| file.plan.steps[i].action.as_str());
        let undone = catch_unwind(AssertUnwindSafe(|| {
            // Checked again right before it runs, as a step is.
            check_undo(&ctx, &root, action, &entry.undo)?;
            undo_one(&mut entry.undo, events, &mut bin)
        }));
        let result = undone.unwrap_or_else(|_| Err(PANICKED.into()));
        if result.is_ok() {
            entry.undone = true;
        }
        file.journal[j] = entry;
        if let Some(i) = step {
            let s = &mut file.plan.steps[i];
            match result {
                Ok(()) => {
                    s.state = "undone".into();
                    s.error = None;
                }
                Err(why) => s.error = Some(format!("되돌리지 못했어요: {why}")),
            }
        }
        progress.done += 1;
        if !persist(store, &file, &mut progress, events, Some(JOURNAL_PATIENCE)) {
            halt = Some(UNDO_UNSAVED);
        }
        progress.send(events, "running", false);
    }
    file.plan.state = finished_state(&file).into();
    // What was undone is written down, however long that takes.
    persist(store, &file, &mut progress, events, None);
    progress.current = None;
    progress.send(events, &file.plan.state, true);
}

/// Checks one undo right before it runs, as `check` does a step: the journal
/// is in the plan file, which any program of this account can write, and a
/// folder on the way may have become a junction since the step ran. What the
/// undo takes and where it puts it back must be in the plan's folder, with
/// no short (8.3) name and no junction or link on the way below it, and
/// nowhere that must stay as it is (`place_refusal` for `action`, the step's,
/// and `why_into` where something is put back).
fn check_undo(ctx: &Ctx, root: &str, action: &str, undo: &Inverse) -> Result<(), String> {
    let root = norm(root).ok_or(BAD_ROOT)?;
    let is_dir = |p: &str| meta_of(p).is_ok_and(|m| m.is_dir());
    match undo {
        Inverse::Rmdir { path } => undo_place(ctx, &root, action, path, true, false),
        Inverse::MoveBack { from, to } => {
            let dir = is_dir(to);
            undo_place(ctx, &root, action, to, dir, false)?;
            undo_place(ctx, &root, action, from, dir, true)
        }
        Inverse::Restore { path, real, .. } => {
            // In the bin nothing tells a folder from a file: it must pass as either.
            for dir in [false, true] {
                undo_place(ctx, &root, action, path, dir, true)?;
                // Where it really was may be outside the plan's folder (a junction above it): only what must stay counts there.
                if let Some(real) = norm(real) {
                    place_refusal(ctx, action, &real, dir)?;
                    if parent_of(&real).is_some_and(|p| ctx.guards.why_into(Path::new(p)).is_some()) {
                        return Err(PROTECTED.into());
                    }
                }
            }
            Ok(())
        }
        Inverse::Decompress { files } => files.iter().try_for_each(|f| undo_place(ctx, &root, action, &f.path, false, false)),
        Inverse::Unrelocate { original, .. } => undo_place(ctx, &root, action, original, true, false),
    }
}

/// `check`'s checks on one path an undo works on (`into`: one it puts
/// something back at): a full path in the plan's folder `root`, not a drive
/// root, with no short name and no link on the way below `root`, and
/// nothing that must stay where it is.
fn undo_place(ctx: &Ctx, root: &str, action: &str, path: &str, is_dir: bool, into: bool) -> Result<(), String> {
    let path = norm(path).ok_or(BAD_PATH)?;
    if is_drive_root(&path) {
        return Err(DRIVE_ROOT.into());
    }
    if !below(&path, root) {
        return Err(OUTSIDE_ROOT.into());
    }
    long_names(&path)?;
    links_between(root, &path)?;
    place_refusal(ctx, action, &path, is_dir)?;
    if into && parent_of(&path).is_some_and(|p| ctx.guards.why_into(Path::new(p)).is_some()) {
        return Err(PROTECTED.into());
    }
    Ok(())
}

/// Undoes one done step. When only part of it could be undone (some files
/// of a folder), `undo` is left with the rest, for another try.
fn undo_one(undo: &mut Inverse, events: &dyn Events, bin: &mut Option<Vec<trash::TrashItem>>) -> Result<(), String> {
    match undo {
        Inverse::Rmdir { path } => {
            let p = win::verbatim(Path::new(path.as_str()));
            match std::fs::symlink_metadata(&p) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                Ok(m) if m.is_dir() && m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 => {
                    // Only while empty: the folder was ours, what is in it now is not.
                    std::fs::remove_dir(&p).map_err(|e| match e.raw_os_error() {
                        Some(c) if c == ERROR_DIR_NOT_EMPTY as i32 => FOLDER_NOT_EMPTY.to_string(),
                        _ => format!("폴더를 지우지 못했어요: {e}"),
                    })
                }
                _ => Err(NOT_OUR_FOLDER.into()),
            }
        }
        Inverse::MoveBack { from, to } => {
            let (home, now_at) = (Path::new(from.as_str()), Path::new(to.as_str()));
            if !exists(now_at) {
                return Err(MOVED_LOST.into());
            }
            // Never over anything (a case-only rename is the same item).
            if exists(home) && !win::same_entry(home, now_at) {
                return Err(ORIGINAL_TAKEN.into());
            }
            let (back, note) = move_entry(events, to, from)?;
            match note {
                // Back on its drive under the name it had there: not quite where it was.
                Some(note) => {
                    *to = back;
                    Err(note)
                }
                None => Ok(()),
            }
        }
        Inverse::Restore { path, real, deleted_from, deleted_to } => {
            let (path, real, from, to) = (path.clone(), real.clone(), *deleted_from, *deleted_to);
            restore(&path, &real, from, to, bin).map_err(|(why, instead)| {
                // Out of the bin, but not under its old name yet: moved there next time.
                if let Some(instead) = instead {
                    *undo = instead;
                }
                why
            })
        }
        Inverse::Decompress { files } => {
            let mut left = Vec::new();
            let mut first = None;
            for f in files.drain(..) {
                if let Err(why) = pack::decompress(Path::new(&f.path), f.wof) {
                    first.get_or_insert_with(|| format!("{}: {why}", f.path));
                    left.push(f);
                }
            }
            let failed = left.len();
            *files = left;
            match first {
                None => Ok(()),
                Some(why) if failed == 1 => Err(why),
                Some(why) => Err(format!("{failed}개 파일의 압축을 풀지 못했어요 ({why})")),
            }
        }
        Inverse::Unrelocate { id, .. } => {
            if id.is_empty() {
                return Err(NO_RELOCATION_RECORD.into());
            }
            let job = ops::restore_and_wait(events.jobs(), id)?;
            if job.state == "done" {
                return Ok(());
            }
            Err(job.errors.first().map_or_else(|| "되돌리지 못했어요".to_string(), |(_, why)| why.clone()))
        }
    }
}

/// Brings `path` back from the Recycle Bin: the item that was there (as
/// given, or where it really was) and went to the bin in that time. It
/// comes out next to where it was under a name of its own (`unbin`), then
/// takes its old name by a rename that never replaces anything: the shell's
/// own restore answers "yes" for itself should something have taken that
/// name meanwhile. When that last rename fails, the error says where the
/// item is, and the Inverse it gives instead moves it there next time.
fn restore(path: &str, real: &str, from: i64, to: i64, bin: &mut Option<Vec<trash::TrashItem>>) -> Result<(), (String, Option<Inverse>)> {
    let plain = |why: &str| (why.to_string(), None);
    if bin.is_none() {
        *bin = Some(trash::os_limited::list().map_err(|e| plain(&format!("휴지통을 읽을 수 없어요: {e}")))?);
    }
    let items = bin.as_mut().ok_or_else(|| plain(NOT_IN_BIN))?;
    let want = [key(path), key(real)];
    // The bin keeps whole seconds; a little slack either way.
    let found = items
        .iter()
        .enumerate()
        .filter(|(_, i)| want.contains(&key(&i.original_path().to_string_lossy())) && (from - 2..=to + 2).contains(&i.time_deleted))
        .max_by_key(|(_, i)| i.time_deleted)
        .map(|(n, _)| n)
        .ok_or_else(|| plain(NOT_IN_BIN))?;
    let target = Path::new(path);
    let home = items[found].original_path();
    if exists(target) || exists(&home) {
        return Err(plain(ORIGINAL_TAKEN));
    }
    let item = items.remove(found);
    let landed = unbin(&item).map_err(|e| plain(&e))?;
    if let Err(e) = win::rename(&landed, &home) {
        let at = landed.to_string_lossy().into_owned();
        let why = match e.kind() {
            io::ErrorKind::AlreadyExists => format!("{ORIGINAL_TAKEN}. 휴지통에서 꺼낸 항목은 {at}에 있어요"),
            _ => format!("{NOT_RESTORED} ({e}). 휴지통에서 꺼낸 항목은 {at}에 있어요"),
        };
        return Err((why, Some(Inverse::MoveBack { from: home.to_string_lossy().into_owned(), to: at })));
    }
    if !exists(target) {
        return Err(plain(NOT_RESTORED));
    }
    Ok(())
}

/// Takes `item` out of the Recycle Bin into the folder it came from, under
/// a name nothing there has (`.name.ff-unbin-<hex>`), and gives where it is.
fn unbin(item: &trash::TrashItem) -> Result<PathBuf, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
    use windows::Win32::UI::Shell::{FileOperation, IFileOperation, IShellItem, SHCreateItemFromParsingName, FOFX_EARLYFAILURE, FOF_NO_UI};

    /// COM for this thread while the item comes out (the trash crate keeps its own).
    struct Com(bool);
    impl Drop for Com {
        fn drop(&mut self) {
            if self.0 {
                unsafe { CoUninitialize() };
            }
        }
    }

    let name = item.name.to_string_lossy();
    let tag = format!("ff-unbin-{:08x}", win::random_u64() as u32);
    // Names are at most 255 UTF-16 units.
    let temp = if name.encode_utf16().count() + tag.len() + 2 <= 255 { format!(".{name}.{tag}") } else { tag };
    if ops::check_name(&temp) != Ok(temp.as_str()) {
        return Err(NOT_RESTORED.into());
    }
    let landed = item.original_parent.join(&temp);
    if exists(&landed) {
        return Err(NOT_RESTORED.into());
    }
    let wide = |s: &std::ffi::OsStr| s.encode_wide().chain(Some(0)).collect::<Vec<u16>>();
    let (id, folder, new_name) = (wide(&item.id), wide(item.original_parent.as_os_str()), wide(std::ffi::OsStr::new(&temp)));
    let failed = |e: windows::core::Error| format!("{NOT_RESTORED} ({e})");
    let _com = Com(unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }.is_ok());
    unsafe {
        let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL).map_err(failed)?;
        // No dialog; a name taken would be answered "yes", but nothing has this one.
        op.SetOperationFlags(FOF_NO_UI | FOFX_EARLYFAILURE).map_err(failed)?;
        let from: IShellItem = SHCreateItemFromParsingName(PCWSTR(id.as_ptr()), None).map_err(failed)?;
        let into: IShellItem = SHCreateItemFromParsingName(PCWSTR(folder.as_ptr()), None).map_err(failed)?;
        op.MoveItem(&from, &into, PCWSTR(new_name.as_ptr()), None).map_err(failed)?;
        op.PerformOperations().map_err(failed)?;
        if op.GetAnyOperationsAborted().map_err(failed)?.as_bool() {
            return Err(NOT_RESTORED.into());
        }
    }
    if !exists(&landed) {
        return Err(NOT_RESTORED.into());
    }
    Ok(landed)
}

/// Windows' own transparent file compression. A compressed file opens and
/// reads as before; writing to a WOF-compressed one decompresses it.
mod pack {
    use super::{ALREADY_SMALL, LINK_ITEM};
    use crate::win;
    use std::fs::OpenOptions;
    use std::io;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_DISK_FULL, ERROR_HANDLE_DISK_FULL, ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION, FILETIME,
        HANDLE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, GetFileTime, SetFileTime, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_COMPRESSED,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    // From WinIoCtl.h, wofapi.h and WinError.h, as in ops.rs.
    const FSCTL_SET_COMPRESSION: u32 = 0x0009_C040;
    const FSCTL_SET_EXTERNAL_BACKING: u32 = 0x0009_030C;
    const FSCTL_GET_EXTERNAL_BACKING: u32 = 0x0009_0310;
    const FSCTL_DELETE_EXTERNAL_BACKING: u32 = 0x0009_0314;
    const COMPRESSION_FORMAT_NONE: u16 = 0;
    const COMPRESSION_FORMAT_DEFAULT: u16 = 1;
    const WOF_CURRENT_VERSION: u32 = 1;
    const WOF_PROVIDER_FILE: u32 = 2;
    const FILE_PROVIDER_CURRENT_VERSION: u32 = 1;
    const FILE_PROVIDER_COMPRESSION_LZX: u32 = 1;
    const ERROR_OBJECT_NOT_EXTERNALLY_BACKED: i32 = 342;
    const ERROR_COMPRESSION_NOT_BENEFICIAL: i32 = 344;

    const IN_USE: &str = "다른 프로그램이 쓰고 있어서 하지 않았어요";
    const READ_ONLY: &str = "읽기 전용 파일이라 압축하지 않았어요";
    const NO_ROOM: &str = "드라이브에 공간이 부족해요";
    const GONE: &str = "파일을 찾을 수 없어요";

    /// Compresses the file `path` in place: WOF with LZX (as `compact /exe:lzx`),
    /// or NTFS compression (LZNT1) where WOF cannot. Some(true) for WOF,
    /// Some(false) for NTFS; None when it is compressed already or would not
    /// get smaller. Its modified time stays as it was.
    pub(super) fn compress(path: &Path) -> Result<Option<bool>, String> {
        let meta = std::fs::symlink_metadata(win::verbatim(path)).map_err(|_| GONE)?;
        let attrs = meta.file_attributes();
        // Never opened to read when it is only in the cloud: that would fetch it.
        if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 || crate::preview::is_online_only(attrs) {
            return Err(LINK_ITEM.into());
        }
        if attrs & FILE_ATTRIBUTE_COMPRESSED != 0 || is_wof(path) {
            return Ok(None);
        }
        if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(LINK_ITEM.into());
        }
        if attrs & FILE_ATTRIBUTE_READONLY != 0 {
            return Err(READ_ONLY.into());
        }
        // Writers are kept out while it is compressed; never through a link.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(win::verbatim(path))
            .map_err(|e| reason(&e))?;
        let h = file.as_raw_handle() as HANDLE;
        // Again from the handle: a plain file still.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(h, &mut info) } == 0
            || info.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0
        {
            return Err(LINK_ITEM.into());
        }
        let written = modified(h);
        let wof: [u32; 5] =
            [WOF_CURRENT_VERSION, WOF_PROVIDER_FILE, FILE_PROVIDER_CURRENT_VERSION, FILE_PROVIDER_COMPRESSION_LZX, 0];
        let done = match control(h, FSCTL_SET_EXTERNAL_BACKING, wof.as_ptr().cast(), size_of_val(&wof)) {
            Ok(()) => Ok(Some(true)),
            Err(e) if e.raw_os_error() == Some(ERROR_COMPRESSION_NOT_BENEFICIAL) => Ok(None),
            Err(e) if stops(&e) => Err(reason(&e)),
            // No WOF on this volume or this Windows: NTFS compression.
            Err(_) => {
                let format = COMPRESSION_FORMAT_DEFAULT;
                control(h, FSCTL_SET_COMPRESSION, (&raw const format).cast(), size_of_val(&format)).map(|()| Some(false)).map_err(|e| reason(&e))
            }
        };
        keep_modified(h, written);
        done
    }

    /// Undoes `compress` for the file `path` (`wof`: how it was compressed).
    /// A file that is not compressed that way any more (written to since)
    /// has nothing to undo.
    pub(super) fn decompress(path: &Path, wof: bool) -> Result<(), String> {
        let meta = std::fs::symlink_metadata(win::verbatim(path)).map_err(|_| GONE)?;
        let attrs = meta.file_attributes();
        if wof {
            if !is_wof(path) {
                return Ok(());
            }
            // Opened through WOF, which takes the request (not the reparse point itself).
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .share_mode(FILE_SHARE_READ)
                .open(win::verbatim(path))
                .map_err(|e| reason(&e))?;
            let h = file.as_raw_handle() as HANDLE;
            let written = modified(h);
            let done = match control(h, FSCTL_DELETE_EXTERNAL_BACKING, null(), 0) {
                Err(e) if e.raw_os_error() != Some(ERROR_OBJECT_NOT_EXTERNALLY_BACKED) => Err(reason(&e)),
                _ => Ok(()),
            };
            keep_modified(h, written);
            return done;
        }
        if attrs & FILE_ATTRIBUTE_COMPRESSED == 0 {
            return Ok(());
        }
        if attrs & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
            return Err(LINK_ITEM.into());
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(win::verbatim(path))
            .map_err(|e| reason(&e))?;
        let h = file.as_raw_handle() as HANDLE;
        let written = modified(h);
        let format = COMPRESSION_FORMAT_NONE;
        let done = control(h, FSCTL_SET_COMPRESSION, (&raw const format).cast(), size_of_val(&format)).map_err(|e| reason(&e));
        keep_modified(h, written);
        done
    }

    /// Whether WOF compresses the file `path` (its own kind of backing, not a
    /// WIMBoot one). WOF hides its reparse point from listings, so their
    /// attributes do not tell: the file is asked.
    pub(super) fn is_wof(path: &Path) -> bool {
        let opened = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(win::verbatim(path));
        let Ok(file) = opened else { return false };
        // WOF_EXTERNAL_INFO, then the provider's own (FILE_PROVIDER_EXTERNAL_INFO_V1).
        let mut info = [0u32; 5];
        let mut returned = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                file.as_raw_handle() as HANDLE,
                FSCTL_GET_EXTERNAL_BACKING,
                null(),
                0,
                info.as_mut_ptr().cast(),
                size_of_val(&info) as u32,
                &mut returned,
                null_mut(),
            )
        };
        ok != 0 && info[1] == WOF_PROVIDER_FILE
    }

    fn control(h: HANDLE, code: u32, input: *const core::ffi::c_void, len: usize) -> io::Result<()> {
        let mut returned = 0u32;
        let ok = unsafe { DeviceIoControl(h, code, input, len as u32, null_mut(), 0, &mut returned, null_mut()) };
        if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    /// Errors that NTFS compression would meet just the same.
    fn stops(e: &io::Error) -> bool {
        matches!(
            e.raw_os_error().map(|c| c as u32),
            Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION | ERROR_ACCESS_DENIED | ERROR_DISK_FULL | ERROR_HANDLE_DISK_FULL)
        )
    }

    fn reason(e: &io::Error) -> String {
        match e.raw_os_error().map(|c| c as u32) {
            Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION) => IN_USE.into(),
            Some(ERROR_ACCESS_DENIED) => "권한이 없어서 하지 않았어요".into(),
            Some(ERROR_DISK_FULL | ERROR_HANDLE_DISK_FULL) => NO_ROOM.into(),
            Some(c) if c as i32 == ERROR_COMPRESSION_NOT_BENEFICIAL => ALREADY_SMALL.into(),
            _ => format!("압축을 바꾸지 못했어요: {e}"),
        }
    }

    fn modified(h: HANDLE) -> Option<FILETIME> {
        let mut t = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        (unsafe { GetFileTime(h, null_mut(), null_mut(), &mut t) } != 0).then_some(t)
    }

    /// Puts the modified time back should the change have touched it.
    fn keep_modified(h: HANDLE, before: Option<FILETIME>) {
        let (Some(before), Some(now)) = (before, modified(h)) else { return };
        if (before.dwLowDateTime, before.dwHighDateTime) != (now.dwLowDateTime, now.dwHighDateTime) {
            unsafe { SetFileTime(h, null(), null(), &before) };
        }
    }
}

// ---- Rules ------------------------------------------------------------------------

/// A plan made by rules for the folder `root` from the index, saved as a
/// draft. The index can be behind the disk: a step that does not hold on the
/// disk now is left out.
fn rules_plan(store: &Store, kind: &str, root: &str, shared: &crate::Shared) -> Result<Plan, String> {
    let now = now_filetime();
    // Found before the index is locked: asking every drive takes a moment.
    let target = if kind == "relocate" { relocation_target(root) } else { None };
    let mut groups = 0usize;
    // Each step with, for a duplicate, the copy that stays.
    let mut steps: Vec<(PlanStep, Option<Keep>)> = {
        let ix = shared.read().unwrap_or_else(PoisonError::into_inner);
        let top = ix.find(root).ok_or(NOT_INDEXED)?;
        let flags = ix.nodes[top as usize].flags;
        if flags & FLAG_DIR == 0 {
            return Err(ROOT_NOT_FOLDER.into());
        }
        if is_link_dir(flags) {
            return Err(ROOT_LINK.into());
        }
        let plain = |steps: Vec<PlanStep>| steps.into_iter().map(|s| (s, None)).collect();
        match kind {
            "cleanup" => plain(cleanup_steps(&ix, top, root, now)),
            "shrink" => plain(shrink_steps(&ix, top, root, now)),
            "relocate" => match &target {
                Some((drive, free)) => {
                    let ctx = Ctx::now();
                    // A folder holding a known folder is looked into too: the checks leave it where it is.
                    let whole = |p: &str| ctx.guards.why(Path::new(p)).is_some() || holds_known(&ctx, p);
                    plain(relocate_steps(&ix, top, root, drive, *free, &whole))
                }
                None => Vec::new(),
            },
            "dedupe" => {
                let candidates = dups::collect(&ix, top, DEDUPE_MIN);
                // The index is let go while the files are read.
                drop(ix);
                let report = dups::find_now(candidates)?;
                groups = report.groups.len();
                dedupe_steps(&report.groups).into_iter().map(|(s, k)| (s, Some(k))).collect()
            }
            _ => return Err(BAD_KIND.into()),
        }
    };
    let ctx = Ctx::now();
    steps.retain(|(s, keep)| check(&ctx, root, s, None).is_ok() && keep.as_ref().is_none_or(|k| twin_refusal(&s.from, k).is_ok()));
    steps.truncate(MAX_STEPS);
    let mut keeps = Vec::new();
    for ((s, keep), id) in steps.iter_mut().zip(1..) {
        s.id = id;
        if let Some(mut k) = keep.take() {
            k.step = id;
            keeps.push(k);
        }
    }
    let steps: Vec<PlanStep> = steps.into_iter().map(|(s, _)| s).collect();
    let (title, summary) = rules_text(kind, &steps, groups, target.as_ref().map(|(d, _)| d.as_str()));
    let plan = Plan {
        id: new_id(),
        kind: kind.into(),
        title,
        summary,
        source: "rules".into(),
        root: root.into(),
        created_at: now,
        steps,
        state: "draft".into(),
    };
    store.write(&PlanFile { keeps, ..PlanFile::new(plan.clone()) }).map_err(|e| e.to_string())?;
    Ok(plan)
}

/// A Recycle Bin step another part of the app found by its own rules.
pub(crate) struct TrashItem {
    pub path: String,
    pub size: u64,
    /// "safe" | "caution" | "danger"
    pub risk: String,
    pub reason: String,
}

/// Saves a draft "cleanup" plan of Recycle Bin steps that another part of
/// the app made by its own rules from the index (programs.rs: folders no
/// installed program accounts for), as `rules_plan` saves its own. The
/// steps get the checks every draft gets (`review`): one that does not hold
/// is "skipped" with why and cannot be selected. Only a "safe" step starts
/// selected; at most MAX_STEPS stay. `summary` is written from the steps as
/// checked. Gives the plan as stored.
pub(crate) fn save_rules_trash(
    root: &str,
    title: &str,
    items: Vec<TrashItem>,
    summary: impl FnOnce(&[PlanStep]) -> String,
) -> Result<Plan, String> {
    rules_trash(&Store::app(), &Ctx::now(), root, title, items, summary)
}

fn rules_trash(
    store: &Store,
    ctx: &Ctx,
    root: &str,
    title: &str,
    items: Vec<TrashItem>,
    summary: impl FnOnce(&[PlanStep]) -> String,
) -> Result<Plan, String> {
    let root = norm(root).ok_or(BAD_ROOT)?;
    let mut steps = Vec::new();
    for item in items.into_iter().take(MAX_STEPS) {
        let from = norm(&item.path).ok_or(BAD_PATH)?;
        let risk = if RISKS.contains(&item.risk.as_str()) { item.risk.as_str() } else { "caution" };
        steps.push(trash_step(from, item.size, risk, clip(&item.reason, 500)));
    }
    for (s, id) in steps.iter_mut().zip(1..) {
        s.id = id;
    }
    let mut plan = Plan {
        id: new_id(),
        kind: "cleanup".into(),
        title: clip(title, 120),
        summary: String::new(),
        source: "rules".into(),
        root,
        created_at: now_filetime(),
        steps,
        state: "draft".into(),
    };
    review(&mut plan, ctx, &[]);
    plan.summary = clip(&summary(&plan.steps), 2000);
    store.write(&PlanFile::new(plan.clone())).map_err(|e| e.to_string())?;
    Ok(plan)
}

fn rules_text(kind: &str, steps: &[PlanStep], groups: usize, target: Option<&str>) -> (String, String) {
    let total: u64 = steps.iter().map(|s| s.size).sum();
    let n = steps.len();
    let picked = steps.iter().filter(|s| s.selected).count();
    let (title, summary) = match kind {
        "cleanup" => (
            "캐시·임시 파일 정리",
            if n == 0 {
                "정리할 캐시·임시 파일을 찾지 못했어요".to_string()
            } else {
                format!("{n}개 항목, 모두 {} — 휴지통으로 보내요. 안전한 {picked}개는 골라 두었어요. 나머지는 확인하고 골라 주세요.", size_text(total))
            },
        ),
        "dedupe" => (
            "중복 파일 정리",
            if n == 0 {
                "지울 만한 중복 파일을 찾지 못했어요".to_string()
            } else {
                format!("{groups}묶음에서 {n}개 파일, 모두 {} — 묶음마다 가장 최근 파일 하나를 남기고 휴지통으로 보내요.", size_text(total))
            },
        ),
        "shrink" => {
            let saved: u64 = steps.iter().map(|s| (s.size as f64 * shrink_ratio(&ext_of(name_of(&s.from)))) as u64).sum();
            (
                "용량 줄이기 (투명 압축)",
                if n == 0 {
                    "압축해서 줄일 만한 큰 파일을 찾지 못했어요".to_string()
                } else {
                    format!("{n}개 파일, 모두 {} — 예상 절약 약 {}. 파일은 그대로 열 수 있고, 되돌리면 압축을 풀어요.", size_text(total), size_text(saved))
                },
            )
        }
        _ => (
            "다른 드라이브로 옮기기",
            match target {
                None => "옮겨 갈 다른 로컬 NTFS 드라이브가 없어요".to_string(),
                Some(_) if n == 0 => "옮길 만한 큰 폴더(5 GB 이상)를 찾지 못했어요".to_string(),
                Some(drive) => format!("{n}개 폴더, 모두 {} → {drive} — 원래 자리에 정션(링크)을 남겨서 프로그램은 예전 경로로 찾아가요.", size_text(total)),
            },
        ),
    };
    (title.to_string(), summary)
}

fn is_link_dir(flags: u32) -> bool {
    flags & (FLAG_DIR | FLAG_REPARSE | FLAG_NOT_LINK) == FLAG_DIR | FLAG_REPARSE
}

/// Every live entry below `top`, folders before what is in them: `visit`
/// gets each (its node, the path of its folder, its name) and says whether
/// to go into it. Folder links are never entered.
fn each_below(ix: &Index, top: u32, top_path: &str, mut visit: impl FnMut(u32, &str, &str) -> bool) {
    let mut stack = vec![(top, top_path.trim_end_matches('\\').to_string())];
    while let Some((dir, base)) = stack.pop() {
        for c in ix.children_of(dir) {
            let flags = ix.nodes[c as usize].flags;
            if flags & FLAG_DELETED != 0 {
                continue;
            }
            let name = ix.name(c);
            if visit(c, &base, name) && flags & FLAG_DIR != 0 && !is_link_dir(flags) {
                stack.push((c, format!("{base}\\{name}")));
            }
        }
    }
}

/// ops' working names (`.x.ff-partial`, `x.ff-old`, ...): a copy, a
/// relocation, a merge into hard links (`.x.ff-link`) or an unlink
/// (`.x.ff-unlink`) may be using them right now.
fn is_ops_name(name: &str) -> bool {
    let name = name.to_lowercase();
    [".ff-partial", ".ff-relocating", ".ff-restoring", ".ff-old", ".ff-link", ".ff-unlink"].iter().any(|m| name.contains(m))
}

fn trash_step(path: String, size: u64, risk: &str, reason: String) -> PlanStep {
    PlanStep {
        id: 0,
        action: "trash".into(),
        from: path,
        to: None,
        reason,
        risk: risk.into(),
        size,
        selected: risk == "safe",
        state: "pending".into(),
        error: None,
    }
}

fn is_temp_root(name: &str) -> bool {
    matches!(name.to_lowercase().as_str(), "temp" | "tmp" | "임시")
}

fn might_be_cache(name: &str) -> bool {
    let name = name.to_lowercase();
    ["cache", "캐시", "temp", "tmp", "임시", "crash", "huggingface", "temporary internet files"].iter().any(|w| name.contains(w))
}

fn is_temp_ext(ext: &str) -> bool {
    matches!(ext, "tmp" | "temp" | "crdownload" | "part" | "partial" | "download" | "dmp" | "mdmp")
}

fn in_downloads(folder: &str) -> bool {
    folder.split('\\').any(|d| matches!(d.to_lowercase().as_str(), "downloads" | "download" | "다운로드"))
}

/// "cleanup": cache and temp folders and files (`classify` "cache"),
/// installers and archives in Downloads older than OLD_DOWNLOAD_DAYS, and
/// big files not changed for BIG_OLD_DAYS. All to the Recycle Bin; the safe
/// ones (temp items of programs, left over a day, crash dumps, setup files)
/// are selected.
fn cleanup_steps(ix: &Index, top: u32, top_path: &str, now: i64) -> Vec<PlanStep> {
    let mut steps = Vec::new();
    let age = |modified: i64| (now - modified).max(0);
    // Each item of a temp folder (never the folder itself, which programs expect to be there).
    let offer_temp = |steps: &mut Vec<PlanStep>, folder: u32, folder_path: &str| {
        let by_programs = folder_path.to_lowercase().split('\\').any(|d| d == "appdata");
        for c in ix.children_of(folder) {
            let n = &ix.nodes[c as usize];
            let name = ix.name(c);
            if n.flags & (FLAG_DELETED | FLAG_REPARSE | FLAG_HARDLINK) != 0 || n.size == 0 || is_ops_name(name) {
                continue;
            }
            let recent = age(n.modified) < RECENT;
            let risk = if by_programs && !recent { "safe" } else { "caution" };
            let mut reason = if by_programs {
                "임시 폴더에 남은 항목이에요. 프로그램이 필요하면 다시 만들어요.".to_string()
            } else {
                "이름이 임시(temp, tmp) 폴더라서 골랐어요. 직접 만든 폴더라면 안을 확인해 주세요.".to_string()
            };
            if recent {
                reason.push_str(" 하루 안에 바뀌어서 지금 쓰는 중일 수 있어요.");
            }
            steps.push(trash_step(format!("{}\\{name}", folder_path.trim_end_matches('\\')), n.size, risk, reason));
        }
    };
    let top_name = name_of(top_path);
    if top != ROOT && classify::category(top_path, top_name, true) == "cache" {
        offer_temp(&mut steps, top, top_path);
        return steps;
    }
    each_below(ix, top, top_path, |c, base, name| {
        let n = &ix.nodes[c as usize];
        let is_dir = n.flags & FLAG_DIR != 0;
        // Links, cloud files and extra names of a file (which free nothing) are never offered.
        if n.flags & (FLAG_REPARSE | FLAG_HARDLINK) != 0 || is_ops_name(name) {
            return false;
        }
        let path = || format!("{base}\\{name}");
        if is_dir {
            // Only a drive's top folders can be Windows' own (`classify`): those are left out whole.
            if base.len() == 2 && classify::program_category(&path(), name, true) == Some("system") {
                return false;
            }
            if might_be_cache(name) && classify::category(&path(), name, true) == "cache" {
                let path = path();
                let lower = name.to_lowercase();
                if KEPT_CACHES.contains(&lower.as_str()) {
                    return false;
                }
                if is_temp_root(name) {
                    offer_temp(&mut steps, c, &path);
                } else if n.size > 0 {
                    let (risk, reason) = if lower == "crashdumps" {
                        ("safe", "프로그램에 오류가 났을 때 남는 덤프 파일 폴더예요.".to_string())
                    } else if lower.contains("huggingface") {
                        ("caution", "내려받은 AI 모델 캐시예요. 지우면 다음에 쓸 때 다시 내려받아야 해요.".to_string())
                    } else {
                        ("caution", "캐시 폴더예요. 지워도 프로그램이 다시 만들어요 (처음 열 때 조금 느릴 수 있어요).".to_string())
                    };
                    steps.push(trash_step(path, n.size, risk, reason));
                }
                return false;
            }
            return true;
        }
        if n.size == 0 {
            return false;
        }
        let ext = ext_of(name);
        let days = age(n.modified) / DAY;
        if is_temp_ext(&ext) {
            let path = path();
            if classify::category(&path, name, false) == "cache" {
                let dump = matches!(ext.as_str(), "dmp" | "mdmp");
                let recent = age(n.modified) < RECENT;
                let risk = if recent { "caution" } else { "safe" };
                let mut reason = if dump {
                    "프로그램에 오류가 났을 때 남는 덤프 파일이에요.".to_string()
                } else {
                    format!("임시 파일이에요 (.{ext}). 다 쓰고 남은 것으로 보여요.")
                };
                if recent {
                    reason.push_str(" 하루 안에 바뀌어서 지금 쓰는 중일 수 있어요.");
                }
                steps.push(trash_step(path, n.size, risk, reason));
            }
            return false;
        }
        let lower = name.to_lowercase();
        let installer = matches!(ext.as_str(), "msi" | "msix" | "msixbundle" | "appx" | "appxbundle")
            || (ext == "exe" && ["setup", "install", "설치"].iter().any(|w| lower.contains(w)));
        if days > OLD_DOWNLOAD_DAYS && in_downloads(base) {
            let path = path();
            if classify::category(&path, name, false) == "archive" {
                let (risk, reason) = if installer {
                    ("safe", format!("{days}일 전에 받은 설치 파일이에요. 이미 설치했다면 필요 없어요."))
                } else {
                    ("caution", format!("{days}일 전에 받은 파일이에요 (설치·압축·디스크 이미지). 다 썼다면 필요 없을 수 있어요."))
                };
                steps.push(trash_step(path, n.size, risk, reason));
                return false;
            }
        }
        if n.size >= BIG_OLD_SIZE && days > BIG_OLD_DAYS {
            let path = path();
            if classify::program_category(&path, name, false).is_none() && classify::category(&path, name, false) != "system" {
                let reason = format!("{}짜리 큰 파일인데 {}년 넘게 바뀌지 않았어요. 아직 필요한지 확인해 주세요.", size_text(n.size), days / 365);
                steps.push(trash_step(path, n.size, "caution", reason));
            }
        }
        false
    });
    steps.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.from.cmp(&b.from)));
    steps.truncate(MAX_STEPS);
    steps
}

/// "dedupe": in each group of files of equal content, the newest copy that
/// is no program's stays (the newest of all if every copy is), and every
/// other copy that is no program's goes to the Recycle Bin, selected. Each
/// step comes with the copy that stays (`Keep`, its step id still 0), which
/// is checked again before the step runs.
fn dedupe_steps(groups: &[DupGroup]) -> Vec<(PlanStep, Keep)> {
    let program = |c: &str| matches!(c, "system" | "program" | "game" | "dev");
    let mut steps = Vec::new();
    for g in groups {
        // Newest first already.
        let Some(kept) = g.files.iter().find(|f| !program(f.category)).or(g.files.first()) else { continue };
        for f in g.files.iter().filter(|f| f.path != kept.path && !program(f.category)) {
            let reason = format!("같은 내용의 파일이 남아요: {} (가장 최근 파일)", kept.path);
            let keep = Keep { step: 0, path: kept.path.clone(), size: g.size, hash: g.hash.clone() };
            steps.push((trash_step(f.path.clone(), g.size, "safe", reason), keep));
        }
    }
    steps.sort_by(|(a, _), (b, _)| b.size.cmp(&a.size).then_with(|| a.from.cmp(&b.from)));
    steps.truncate(MAX_STEPS);
    steps
}

/// How much of a file of this kind compression saves, about (0 when it is
/// not worth trying).
fn shrink_ratio(ext: &str) -> f64 {
    match ext {
        "log" | "txt" | "csv" | "tsv" | "json" | "jsonl" | "ndjson" | "xml" | "sql" | "md" => 0.7,
        "bmp" => 0.6,
        "tif" | "tiff" => 0.3,
        "exe" | "dll" => 0.35,
        _ => 0.0,
    }
}

/// "shrink": big files that compress well (logs and other text, bitmaps,
/// TIFF; programs and libraries in the user's own folders) and are not
/// compressed yet, each to compress, with the space it may free.
fn shrink_steps(ix: &Index, top: u32, top_path: &str, now: i64) -> Vec<PlanStep> {
    let mut steps: Vec<(u64, PlanStep)> = Vec::new();
    each_below(ix, top, top_path, |c, base, name| {
        let n = &ix.nodes[c as usize];
        if n.flags & (FLAG_REPARSE | FLAG_HARDLINK) != 0 || is_ops_name(name) {
            return false;
        }
        if n.flags & FLAG_DIR != 0 {
            // Only a drive's top folders can be Windows' own: those are left out whole.
            return base.len() != 2 || classify::program_category(&format!("{base}\\{name}"), name, true) != Some("system");
        }
        let ext = ext_of(name);
        let ratio = shrink_ratio(&ext);
        // Compressed already (or sparse): it takes less room than its size.
        if ratio == 0.0 || n.size < SHRINK_MIN || (n.alloc > 0 && n.alloc < n.size - n.size / 10) {
            return false;
        }
        let path = format!("{base}\\{name}");
        if classify::program_category(&path, name, false).is_some() || classify::category(&path, name, false) == "system" {
            return false;
        }
        let program = matches!(ext.as_str(), "exe" | "dll");
        // Programs only in the user's own folders (`C:\Users\<name>\...`).
        if program && !path.get(3..).is_some_and(|rest| rest.to_lowercase().starts_with("users\\") && rest.matches('\\').count() >= 2) {
            return false;
        }
        let saved = (n.size as f64 * ratio) as u64;
        let recent = now - n.modified < DAY;
        let mut reason = if program {
            format!("프로그램 파일이에요. 예상 절약 약 {}. 압축하면 처음 실행할 때 조금 느려질 수 있어요.", size_text(saved))
        } else {
            let what = if matches!(ext.as_str(), "bmp" | "tif" | "tiff") { "압축하지 않은 그림" } else { "텍스트" };
            format!("{what} 파일이라 잘 줄어요. 예상 절약 약 {} (파일은 그대로 열 수 있어요).", size_text(saved))
        };
        if recent {
            reason.push_str(" 하루 안에 바뀐 파일이라 곧 다시 쓰이면 압축이 풀려요.");
        }
        let risk = if program || recent { "caution" } else { "safe" };
        steps.push((
            saved,
            PlanStep {
                id: 0,
                action: "compress".into(),
                from: path,
                to: None,
                reason,
                risk: risk.into(),
                size: n.size,
                selected: false,
                state: "pending".into(),
                error: None,
            },
        ));
        false
    });
    steps.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.from.cmp(&b.1.from)));
    steps.into_iter().take(MAX_STEPS).map(|(_, s)| s).collect()
}

/// The other local fixed NTFS drive with the most room (`D:\`, bytes free),
/// for "relocate".
fn relocation_target(root: &str) -> Option<(String, u64)> {
    let here = root.get(..2)?.to_uppercase();
    win::drives()
        .into_iter()
        .filter(|d| d.kind == "fixed" && !d.subst && d.fs.eq_ignore_ascii_case("NTFS") && d.letter.to_uppercase() != here)
        .max_by_key(|d| d.free)
        .map(|d| (format!("{}\\", d.letter), d.free))
}

/// "relocate": the biggest folders (RELOCATE_MIN and more) that may move as
/// a whole, to the drive `target` with `free` bytes, as long as they fit
/// with RELOCATE_MARGIN to spare. Windows' and installed programs' folders
/// are left out; a folder that may not move as a whole (`whole` says so: a
/// profile, a folder holding the app's data) is looked into instead.
fn relocate_steps(ix: &Index, top: u32, top_path: &str, target: &str, free: u64, whole: &dyn Fn(&str) -> bool) -> Vec<PlanStep> {
    let mut found: Vec<(String, u64)> = Vec::new();
    each_below(ix, top, top_path, |c, base, name| {
        let n = &ix.nodes[c as usize];
        if n.flags & FLAG_DIR == 0 || n.flags & FLAG_REPARSE != 0 || n.size < RELOCATE_MIN || is_ops_name(name) {
            return false;
        }
        let path = format!("{base}\\{name}");
        let category = classify::category(&path, name, true);
        if category == "system" || category == "program" || classify::program_category(&path, name, true) == Some("system") {
            return false;
        }
        if whole(&path) {
            return true;
        }
        found.push((path, n.size));
        false
    });
    found.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let drive = target.trim_end_matches('\\');
    let mut room = free.saturating_sub(RELOCATE_MARGIN);
    let mut steps = Vec::new();
    for (path, size) in found {
        if steps.len() >= RELOCATE_MOST {
            break;
        }
        if size > room {
            continue;
        }
        room -= size;
        let reason = format!(
            "{}짜리 폴더예요. {drive}로 옮기고 원래 자리에 정션(링크)을 남겨서, 프로그램은 예전 경로로 그대로 찾아가요. 쓰는 중인 파일이 있으면 옮기지 않아요.",
            size_text(size)
        );
        steps.push(PlanStep {
            id: 0,
            action: "relocate".into(),
            from: path,
            to: Some(target.into()),
            reason,
            risk: "caution".into(),
            size,
            selected: false,
            state: "pending".into(),
            error: None,
        });
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dups::DupFile;
    use std::sync::Arc;
    use crate::index::RawEntry;
    use std::fs;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{GetCompressedFileSizeW, FILE_ATTRIBUTE_COMPRESSED};

    /// Runs that take the one-plan turn take turns too.
    fn one_at_a_time() -> MutexGuard<'static, ()> {
        static TESTS: Mutex<()> = Mutex::new(());
        TESTS.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A folder for one test, in the user's profile: under the temp folder
    /// (app data) the checks would rightly refuse to touch anything.
    struct Temp(PathBuf);

    impl Temp {
        fn new(tag: &str) -> Temp {
            let home = std::env::var("USERPROFILE").expect("a profile folder");
            let dir = PathBuf::from(home).join(format!("ff-plan-test-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Temp(dir)
        }

        fn path(&self, name: &str) -> String {
            self.0.join(name).to_string_lossy().into_owned()
        }

        fn file(&self, name: &str, bytes: &[u8]) -> String {
            let p = self.path(name);
            fs::write(&p, bytes).unwrap();
            p
        }

        fn store(&self) -> Store {
            Store(self.0.join("plans"))
        }

        fn root(&self) -> String {
            self.0.to_string_lossy().into_owned()
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(win::verbatim(&self.0));
        }
    }

    /// What a run told, for a test.
    #[derive(Default)]
    struct Sink(Mutex<Vec<PlanProgress>>);

    /// The jobs a test's steps start report nowhere.
    struct Quiet;

    impl ops::Events for Quiet {
        fn progress(&self, _: &ops::JobInfo) {}
        fn conflict(&self, _: &ops::Conflict) {}
    }

    impl Events for Sink {
        fn progress(&self, p: &PlanProgress) {
            self.0.lock().unwrap().push(p.clone());
        }

        fn jobs(&self) -> Box<dyn ops::Events> {
            Box::new(Quiet)
        }

        fn indexed_size(&self, _: &str) -> Option<u64> {
            None
        }
    }

    impl Sink {
        fn states(&self) -> Vec<String> {
            self.0.lock().unwrap().iter().map(|p| p.state.clone()).collect()
        }

        fn currents(&self) -> Vec<String> {
            self.0.lock().unwrap().iter().filter_map(|p| p.current.clone()).collect()
        }
    }

    /// A Sink that also does something as it hears each event.
    struct Hooked<F: Fn(&PlanProgress) + Send + Sync + 'static>(Sink, F);

    impl<F: Fn(&PlanProgress) + Send + Sync + 'static> Events for Hooked<F> {
        fn progress(&self, p: &PlanProgress) {
            self.0.progress(p);
            (self.1)(p);
        }

        fn jobs(&self) -> Box<dyn ops::Events> {
            Box::new(Quiet)
        }

        fn indexed_size(&self, _: &str) -> Option<u64> {
            None
        }
    }

    fn step(id: u32, action: &str, from: &str, to: Option<&str>) -> PlanStep {
        PlanStep {
            id,
            action: action.into(),
            from: from.into(),
            to: to.map(str::to_string),
            reason: "테스트".into(),
            risk: "safe".into(),
            size: 0,
            selected: true,
            state: "pending".into(),
            error: None,
        }
    }

    fn plan(id: &str, root: &str, source: &str, steps: Vec<PlanStep>) -> Plan {
        Plan {
            id: id.into(),
            kind: "structure".into(),
            title: "테스트".into(),
            summary: String::new(),
            source: source.into(),
            root: root.into(),
            created_at: 0,
            steps,
            state: "draft".into(),
        }
    }

    /// Runs `work` on a plan in the store to its end, on this thread.
    fn run(store: &Store, id: &str, work: Work) -> Sink {
        let sink = Sink::default();
        let turn = begin(store, id, work).unwrap();
        go(store, id, work, &sink);
        drop(turn);
        sink
    }

    fn compressed(path: &str) -> bool {
        let p = Path::new(path);
        let attrs = fs::symlink_metadata(p).unwrap().file_attributes();
        let wide: Vec<u16> = p.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut high = 0u32;
        let low = unsafe { GetCompressedFileSizeW(wide.as_ptr(), &mut high) };
        let on_disk = ((high as u64) << 32) | low as u64;
        attrs & FILE_ATTRIBUTE_COMPRESSED != 0 || pack::is_wof(p) || on_disk < fs::metadata(p).unwrap().len()
    }

    #[test]
    fn paths_are_normalized_or_refused() {
        assert_eq!(norm(r"c:\Users\me\").as_deref(), Some(r"C:\Users\me"));
        assert_eq!(norm("d:/a/b").as_deref(), Some(r"D:\a\b"));
        assert_eq!(norm(r"\\?\C:\x").as_deref(), Some(r"C:\x"));
        assert_eq!(norm(r"C:\").as_deref(), Some(r"C:\"));
        for bad in [r"a\b", r"\\server\share\x", r"C:\a\..\Windows", r"C:\a\.\b", r"C:\a\\b", r"C:\a*", "C:\\a\u{7}", r"C:x", ""] {
            assert_eq!(norm(bad), None, "{bad}");
        }
        assert!(below(r"C:\a\b", r"C:\A"));
        assert!(below(r"C:\a", r"C:\"));
        assert!(!below(r"C:\a", r"C:\a"));
        assert!(!below(r"C:\ab", r"C:\a"));
        assert_eq!(parent_of(r"C:\a\b"), Some(r"C:\a"));
        assert_eq!(parent_of(r"C:\a"), Some(r"C:\"));
        assert_eq!(parent_of(r"C:\"), None);
        for bad in ["", "../x", r"a\b", "con", "NUL", "x y", &"p".repeat(65)] {
            assert!(check_id(bad).is_err(), "{bad}");
        }
        let id = new_id();
        assert!(id.starts_with('p') && check_id(&id).is_ok(), "{id}");
        assert_ne!(new_id(), id);
    }

    #[test]
    fn plans_round_trip_through_the_store() {
        let _turn = one_at_a_time();
        let t = Temp::new("store");
        let store = t.store();
        let a = t.file("a.txt", b"a");
        let mut first = plan("p1-old", &t.root(), "claude", vec![step(1, "rename", &a, Some(&t.path("b.txt")))]);
        first.created_at = 100;
        let mut second = plan("p2-new", &t.root(), "mcp", vec![step(1, "trash", &a, None)]);
        second.created_at = 200;
        store.save(&first).unwrap();
        store.save(&second).unwrap();

        let back = store.load("p1-old").unwrap().plan;
        assert_eq!(back, first);
        let ids: Vec<String> = store.list().into_iter().map(|p| p.id).collect();
        assert_eq!(ids, ["p2-new", "p1-old"]);
        // Saved from outside: a Recycle Bin step never stays selected.
        assert!(!store.load("p2-new").unwrap().plan.steps[0].selected);

        // A draft can be saved again; a plan that ran cannot.
        store.save(&first).unwrap();
        store.change("p1-old", |f| {
            f.plan.state = "done".into();
            Ok(())
        })
        .unwrap();
        assert!(store.save(&first).is_err());

        for bad in [r"..\x", "con", ""] {
            assert!(store.load(bad).is_err(), "{bad}");
            assert!(store.save(&plan(bad, &t.root(), "mcp", Vec::new())).is_err(), "{bad}");
        }
        assert_eq!(store.load("p-missing").unwrap_err().kind(), io::ErrorKind::NotFound);

        // A running plan stays; once its process is gone it is settled and can go.
        let turn = Turn::take(&store, "p2-new").unwrap();
        store.change("p2-new", |f| {
            f.plan.state = "running".into();
            Ok(())
        })
        .unwrap();
        assert_eq!(store.load("p2-new").unwrap().plan.state, "running");
        assert_eq!(store.remove("p2-new").unwrap_err().to_string(), RUNNING);
        drop(turn);
        assert_eq!(store.load("p2-new").unwrap().plan.state, "failed");
        store.remove("p2-new").unwrap();
        assert_eq!(store.list().len(), 1);
    }

    #[test]
    fn steps_are_checked_where_they_go() {
        let t = Temp::new("check");
        let ctx = Ctx::now();
        let root = t.root();
        let a = t.file("a.txt", b"a");
        let b = t.file("b.txt", b"b");
        fs::create_dir(t.path("real")).unwrap();
        t.file(r"real\inner.txt", b"x");
        ops::create_junction(Path::new(&t.path("link")), Path::new(&t.path("real"))).unwrap();
        let why = |s: PlanStep, root: &str| check(&ctx, root, &s, None).err().unwrap_or_default();

        assert_eq!(why(step(1, "trash", r"C:\elsewhere.txt", None), &root), OUTSIDE_ROOT);
        assert_eq!(why(step(1, "trash", r"C:\", None), r"C:\"), DRIVE_ROOT);
        assert_eq!(why(step(1, "trash", r"C:\Windows\System32", None), r"C:\"), SYSTEM_ITEM);
        assert_eq!(why(step(1, "trash", r"C:\Program Files", None), r"C:\"), PROGRAM_ITEM);
        assert_eq!(why(step(1, "move", &a, Some(r"C:\Windows\a.txt")), r"C:\"), SYSTEM_ITEM);
        // Never over anything.
        assert_eq!(why(step(1, "move", &a, Some(&b)), &root), NAME_TAKEN);
        assert_eq!(why(step(1, "rename", &a, Some(&b)), &root), NAME_TAKEN);
        assert_eq!(why(step(1, "mkdir", &b, None), &root), NAME_TAKEN);
        // Links: the link itself, and anything reached through it.
        assert_eq!(why(step(1, "trash", &t.path("link"), None), &root), LINK_ITEM);
        assert_eq!(why(step(1, "trash", &t.path(r"link\inner.txt"), None), &root), LINK_ON_PATH);
        assert_eq!(why(step(1, "move", &a, Some(&t.path(r"link\a.txt"))), &root), LINK_ON_PATH);
        assert_eq!(why(step(1, "trash", &t.path("gone.txt"), None), &root), MISSING);
        assert_eq!(why(step(1, "rename", &a, Some(&t.path(r"real\a.txt"))), &root), NOT_SAME_FOLDER);
        assert_eq!(why(step(1, "rename", &a, Some(&t.path("nul"))), &root), ops::check_name("nul").unwrap_err());
        assert_eq!(why(step(1, "move", &t.path("real"), Some(&t.path(r"real\deeper"))), &root), INTO_ITSELF);
        assert_eq!(why(step(1, "move", &a, Some(&t.path(r"nowhere\a.txt"))), &root), NO_PARENT);
        assert_eq!(why(step(1, "move", &a, None), &root), NO_TARGET);
        assert_eq!(why(step(1, "explode", &a, None), &root), UNKNOWN_ACTION);
        // What holds.
        assert!(check(&ctx, &root, &step(1, "rename", &a, Some(&t.path("A.txt"))), None).is_ok(), "a case-only rename");
        assert!(check(&ctx, &root, &step(1, "move", &a, Some(&t.path(r"real\a.txt"))), None).is_ok());
        assert!(check(&ctx, &root, &step(1, "compress", &b, None), None).is_ok());
        if !win::recycle_policy(&b).nuke {
            assert!(check(&ctx, &root, &step(1, "trash", &b, None), None).is_ok());
        }

        // A draft counts what earlier steps make; a run looks at the disk alone.
        let mut draft = plan("p-draft", &root, "claude", vec![
            step(1, "mkdir", &t.path("new"), None),
            step(2, "move", &a, Some(&t.path(r"new\a.txt"))),
            step(3, "move", &b, Some(&t.path(r"new\a.txt"))),
        ]);
        review(&mut draft, &ctx, &[]);
        let states: Vec<&str> = draft.steps.iter().map(|s| s.state.as_str()).collect();
        assert_eq!(states, ["pending", "pending", "skipped"]);
        assert_eq!(draft.steps[2].error.as_deref(), Some(NAME_TAKEN));
        assert!(!draft.steps[2].selected);
        assert_eq!(check(&ctx, &root, &draft.steps[1], None).unwrap_err(), NO_PARENT);
    }

    #[test]
    fn untrusted_plans_are_brought_into_shape() {
        let root = r"C:\Users\me\Stuff";
        let mut steps = vec![
            step(7, "trash", r"C:\Users\me\Stuff\old.zip", None),
            step(7, "move", r"C:\Users\me\Stuff\a.txt", Some(r"C:\Users\me\Stuff\b\a.txt")),
            step(9, "format_disk", r"C:\Users\me\Stuff", None),
            step(10, "rename", r"..\..\Windows", Some("x")),
            step(11, "mkdir", r"C:\Users\me\Stuff\new", Some(r"C:\ignored")),
            // What only rules make is never taken from outside.
            step(12, "compress", r"C:\Users\me\Stuff\big.log", None),
            step(13, "relocate", r"C:\Users\me\Stuff\Games", Some(r"E:\")),
            step(14, "rename", r"C:\Users\me\Stuff\c.txt", Some(r"C:\Users\me\Stuff\d.txt")),
        ];
        steps[0].risk = "whatever".into();
        steps[1].reason = format!("이유\u{0}{}", "길다".repeat(400));
        steps[7].risk = "danger".into();
        let p = admit(plan("p-ai", root, "openai", steps)).unwrap();
        let actions: Vec<&str> = p.steps.iter().map(|s| s.action.as_str()).collect();
        assert_eq!(actions, ["trash", "move", "mkdir", "rename"]);
        // The user picks what goes to the Recycle Bin, and what its maker calls dangerous; other picks stay.
        assert!(!p.steps[0].selected && p.steps[1].selected && !p.steps[3].selected);
        assert_eq!(p.steps[0].risk, "caution");
        // Ids made unique again; text clipped, without control characters.
        let ids: Vec<u32> = p.steps.iter().map(|s| s.id).collect();
        assert_eq!(ids, [1, 2, 3, 4]);
        assert_eq!(p.steps[1].reason.chars().count(), 500);
        assert!(!p.steps[1].reason.contains('\u{0}'));
        assert_eq!(p.steps[2].to, None);
        assert!(p.summary.contains("올바르지 않아서 뺀 단계 4개"), "{}", p.summary);
        assert!(p.summary.contains(UNTRUSTED_ACTION), "{}", p.summary);

        let many: Vec<PlanStep> = (1..=MAX_STEPS as u32 + 20).map(|i| step(i, "mkdir", &format!(r"{root}\f{i}"), None)).collect();
        let p = admit(plan("p-many", root, "mcp", many)).unwrap();
        assert_eq!(p.steps.len(), MAX_STEPS);
        assert!(p.summary.contains("20개 뺌"), "{}", p.summary);

        assert!(admit(plan("p-bad", root, "someone", Vec::new())).is_err());
        assert!(admit(plan("p-bad", "relative", "mcp", Vec::new())).is_err());
        // Never a whole drive, and never a plan that says rules made it.
        assert_eq!(admit(plan("p-bad", r"C:\", "mcp", Vec::new())).unwrap_err().to_string(), DRIVE_ROOT_PLAN);
        assert_eq!(admit(plan("p-bad", root, "rules", Vec::new())).unwrap_err().to_string(), RULES_ONLY);
        let mut odd = plan("p-bad", root, "mcp", Vec::new());
        odd.kind = "everything".into();
        assert!(admit(odd).is_err());
    }

    #[test]
    fn a_plan_runs_and_comes_undone() {
        let _turn = one_at_a_time();
        let t = Temp::new("run");
        let store = t.store();
        let a = t.file("a.txt", b"apple");
        let b = t.file("b.txt", b"banana");
        let d = t.file("d.txt", b"date");
        let e = t.file("e.txt", b"elder");
        let log = t.file("big.log", &b"a line of a log that repeats and repeats\n".repeat(40_000));
        let when = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        File::options().write(true).open(&log).unwrap().set_modified(when).unwrap();
        let mut steps = vec![
            step(1, "mkdir", &t.path("new"), None),
            step(2, "move", &a, Some(&t.path(r"new\a.txt"))),
            step(3, "rename", &b, Some(&t.path("c.txt"))),
            step(4, "compress", &log, None),
            step(5, "move", &d, Some(&t.path("d2.txt"))),
            step(6, "rename", &e, Some(&t.path("f.txt"))),
        ];
        steps[5].selected = false;
        let mut file = PlanFile::new(plan("p-run", &t.root(), "rules", steps));
        review(&mut file.plan, &Ctx::now(), &[]);
        assert!(file.plan.steps.iter().all(|s| s.state == "pending"), "{:?}", file.plan.steps);
        store.write(&file).unwrap();

        let sink = Sink::default();
        let turn = begin(&store, "p-run", Work::Execute).unwrap();
        // Taken after the plan was approved: that step fails instead of writing over it.
        fs::write(t.path("d2.txt"), b"someone else's").unwrap();
        go(&store, "p-run", Work::Execute, &sink);
        drop(turn);
        let done = store.load("p-run").unwrap();
        let states: Vec<&str> = done.plan.steps.iter().map(|s| s.state.as_str()).collect();
        assert_eq!(states, ["done", "done", "done", "done", "failed", "skipped"], "{:?}", done.plan.steps);
        assert_eq!(done.plan.steps[4].error.as_deref(), Some(NAME_TAKEN));
        assert_eq!(done.plan.state, "partly");
        assert_eq!(fs::read(t.path("d2.txt")).unwrap(), b"someone else's");
        assert_eq!(fs::read(&d).unwrap(), b"date");
        assert_eq!(fs::read(t.path(r"new\a.txt")).unwrap(), b"apple");
        assert_eq!(fs::read(t.path("c.txt")).unwrap(), b"banana");
        assert!(!Path::new(&a).exists() && !Path::new(&b).exists() && Path::new(&e).exists());
        assert!(compressed(&log), "the log is compressed");
        assert_eq!(fs::metadata(&log).unwrap().modified().unwrap(), when);
        assert_eq!(done.journal.len(), 4);
        assert!(matches!(&done.journal[3].undo, Inverse::Decompress { files } if files.len() == 1 && files[0].path == log));
        // Compressed, it is a plain file to the checks still, and compressing it again has nothing to do.
        let ctx = Ctx::now();
        assert!(check(&ctx, &t.root(), &step(9, "compress", &log, None), None).is_ok());
        assert!(check(&ctx, &t.root(), &step(9, "rename", &log, Some(&t.path("big2.log"))), None).is_ok());
        assert_eq!(pack::compress(Path::new(&log)), Ok(None));
        let states = sink.states();
        assert_eq!((states.first().map(String::as_str), states.last().map(String::as_str)), (Some("running"), Some("partly")));
        // A plan that ran is not run again.
        assert_eq!(begin(&store, "p-run", Work::Execute).err().as_deref(), Some(NOT_DRAFT));

        run(&store, "p-run", Work::Undo);
        let undone = store.load("p-run").unwrap();
        assert_eq!(undone.plan.state, "undone", "{:?}", undone.plan.steps);
        let states: Vec<&str> = undone.plan.steps.iter().map(|s| s.state.as_str()).collect();
        assert_eq!(states, ["undone", "undone", "undone", "undone", "failed", "skipped"]);
        assert_eq!(fs::read(&a).unwrap(), b"apple");
        assert_eq!(fs::read(&b).unwrap(), b"banana");
        assert!(!Path::new(&t.path("c.txt")).exists() && !Path::new(&t.path("new")).exists());
        assert!(!compressed(&log), "the log is decompressed");
        assert_eq!(fs::read(&log).unwrap().len(), 40_000 * 41);
        assert_eq!(fs::metadata(&log).unwrap().modified().unwrap(), when);
        assert_eq!(begin(&store, "p-run", Work::Undo).err().as_deref(), Some(NOTHING_TO_UNDO));
    }

    #[test]
    fn a_folder_made_by_the_plan_goes_only_while_empty() {
        let _turn = one_at_a_time();
        let t = Temp::new("rmdir");
        let store = t.store();
        let a = t.file("a.txt", b"apple");
        let steps = vec![step(1, "mkdir", &t.path("box"), None), step(2, "move", &a, Some(&t.path(r"box\a.txt")))];
        store.write(&PlanFile::new(plan("p-box", &t.root(), "rules", steps))).unwrap();
        run(&store, "p-box", Work::Execute);
        assert_eq!(store.load("p-box").unwrap().plan.state, "done");
        // Something of the user's lands in the folder meanwhile: it stays, and so does the folder.
        fs::write(t.path(r"box\mine.txt"), b"mine").unwrap();
        run(&store, "p-box", Work::Undo);
        let after = store.load("p-box").unwrap();
        assert_eq!(after.plan.state, "partly");
        assert_eq!(after.plan.steps[0].state, "done");
        assert!(after.plan.steps[0].error.as_deref().is_some_and(|e| e.contains(FOLDER_NOT_EMPTY)));
        assert_eq!(after.plan.steps[1].state, "undone");
        assert_eq!(fs::read(&a).unwrap(), b"apple");
        assert_eq!(fs::read(t.path(r"box\mine.txt")).unwrap(), b"mine");
        // Once it is empty again, another undo finishes the job.
        fs::remove_file(t.path(r"box\mine.txt")).unwrap();
        run(&store, "p-box", Work::Undo);
        assert_eq!(store.load("p-box").unwrap().plan.state, "undone");
        assert!(!Path::new(&t.path("box")).exists());
    }

    #[test]
    fn trash_goes_to_the_recycle_bin_and_comes_back() {
        let _turn = one_at_a_time();
        let t = Temp::new("bin");
        if !win::has_recycle_bin(&t.root()) || win::recycle_policy(&t.root()).nuke {
            return;
        }
        let store = t.store();
        let name = format!("trash-me-{:08x}.txt", win::random_u64() as u32);
        let victim = t.file(&name, b"keep me safe");
        let steps = vec![step(1, "trash", &victim, None)];
        store.write(&PlanFile::new(plan("p-bin", &t.root(), "rules", steps))).unwrap();
        run(&store, "p-bin", Work::Execute);
        let done = store.load("p-bin").unwrap();
        assert_eq!(done.plan.state, "done", "{:?}", done.plan.steps);
        assert!(!Path::new(&victim).exists());
        if !bin_lists(&victim) {
            eprintln!("this account's Recycle Bin cannot be read back (a service account on CI); skipped");
            return;
        }
        assert!(matches!(&done.journal[0].undo, Inverse::Restore { path, .. } if *path == victim));
        run(&store, "p-bin", Work::Undo);
        let undone = store.load("p-bin").unwrap();
        assert_eq!(undone.plan.state, "undone", "{:?}", undone.plan.steps);
        assert_eq!(fs::read(&victim).unwrap(), b"keep me safe");
    }

    #[test]
    fn a_run_never_goes_on_while_its_journal_is_not_written() {
        let _turn = one_at_a_time();
        let t = Temp::new("journal");
        let store = t.store();
        let a = t.file("a.txt", b"apple");
        let b = t.file("b.txt", b"banana");
        let steps = vec![step(1, "rename", &a, Some(&t.path("a2.txt"))), step(2, "rename", &b, Some(&t.path("b2.txt")))];
        store.write(&PlanFile::new(plan("p-jam", &t.root(), "rules", steps))).unwrap();
        let plan_file = store.file("p-jam").unwrap();
        // Right before step 1 a scanner holds the plan file (for longer than the run waits for it).
        let held: Arc<Mutex<Option<File>>> = Arc::default();
        let hold = held.clone();
        let first = a.clone();
        let events = Hooked(Sink::default(), move |p: &PlanProgress| {
            let mut slot = hold.lock().unwrap();
            if p.current.as_deref() == Some(first.as_str()) && slot.is_none() {
                *slot = Some(OpenOptions::new().read(true).share_mode(0).open(&plan_file).unwrap());
                let hold = hold.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(JOURNAL_PATIENCE * 2);
                    hold.lock().unwrap().take();
                });
            }
        });
        let turn = begin(&store, "p-jam", Work::Execute).unwrap();
        go(&store, "p-jam", Work::Execute, &events);
        drop(turn);
        assert!(held.lock().unwrap().is_none(), "let go before the run ended");
        let after = store.load("p-jam").unwrap();
        let states: Vec<&str> = after.plan.steps.iter().map(|s| s.state.as_str()).collect();
        assert_eq!(states, ["done", "skipped"], "{:?}", after.plan.steps);
        assert_eq!(after.plan.steps[1].error.as_deref(), Some(JOURNAL_UNSAVED));
        assert_eq!(after.plan.state, "partly");
        // Step 2 never ran; step 1's undo reached the disk once it could.
        assert_eq!(fs::read(&b).unwrap(), b"banana");
        assert_eq!(after.journal.len(), 1);
        assert!(events.0.currents().iter().any(|c| c.starts_with(JOURNAL_WAIT)), "the window heard why it waits");
        run(&store, "p-jam", Work::Undo);
        assert_eq!(store.load("p-jam").unwrap().plan.state, "undone");
        assert_eq!(fs::read(&a).unwrap(), b"apple");
    }

    #[test]
    fn closing_the_app_lets_the_step_at_hand_finish_and_starts_no_other() {
        let _turn = one_at_a_time();
        let t = Temp::new("closing");
        let store = t.store();
        let a = t.file("a.txt", b"apple");
        let b = t.file("b.txt", b"banana");
        let steps = vec![step(1, "rename", &a, Some(&t.path("a2.txt"))), step(2, "rename", &b, Some(&t.path("b2.txt")))];
        store.write(&PlanFile::new(plan("p-close", &t.root(), "rules", steps))).unwrap();
        assert!(!running());
        // The app is closed while step 1 runs.
        let events = Hooked(Sink::default(), |p: &PlanProgress| {
            if p.state == "running" && p.done == 1 {
                CLOSING_NOW.store(true, Ordering::SeqCst);
            }
        });
        let turn = begin(&store, "p-close", Work::Execute).unwrap();
        assert!(running());
        go(&store, "p-close", Work::Execute, &events);
        drop(turn);
        assert!(stop_and_wait(Duration::ZERO), "no plan runs any more");
        CLOSING_NOW.store(false, Ordering::SeqCst);
        let after = store.load("p-close").unwrap();
        let states: Vec<&str> = after.plan.steps.iter().map(|s| s.state.as_str()).collect();
        assert_eq!(states, ["done", "skipped"]);
        assert_eq!(after.plan.steps[1].error.as_deref(), Some(CLOSING));
        assert_eq!(after.journal.len(), 1);
        assert_eq!(after.plan.state, "partly");
        assert_eq!(fs::read(&b).unwrap(), b"banana");
        assert_eq!(fs::read(t.path("a2.txt")).unwrap(), b"apple");
    }

    #[test]
    fn an_update_that_did_not_go_ahead_lets_plans_run_again_once_none_runs() {
        let _turn = one_at_a_time();
        let t = Temp::new("reopen");
        let store = t.store();
        let a = t.file("a.txt", b"apple");
        store.write(&PlanFile::new(plan("p-reopen", &t.root(), "rules", vec![step(1, "rename", &a, Some(&t.path("a2.txt")))]))).unwrap();
        // Told to stop for the update while it runs: the stop stays until the run is over.
        let turn = begin(&store, "p-reopen", Work::Execute).unwrap();
        assert!(!stop_and_wait(Duration::ZERO), "the run has not stopped yet");
        reopen();
        assert!(closing(), "a run that was told to stop never goes on");
        // The update gave up waiting (lib.rs update_prepare): once the run is over, plans may run again.
        drop(turn);
        assert!(!closing(), "the stop is lifted when the run it waited for is over");
        // Nothing runs: at once.
        CLOSING_NOW.store(true, Ordering::SeqCst);
        reopen();
        assert!(!closing());
    }

    #[test]
    fn a_closing_after_an_update_gave_up_holds_past_the_run_at_hand() {
        let _turn = one_at_a_time();
        let t = Temp::new("reclose");
        let store = t.store();
        let a = t.file("a.txt", b"apple");
        store.write(&PlanFile::new(plan("p-reclose", &t.root(), "rules", vec![step(1, "rename", &a, Some(&t.path("a2.txt")))]))).unwrap();
        let turn = begin(&store, "p-reclose", Work::Execute).unwrap();
        assert!(!stop_and_wait(Duration::ZERO));
        reopen();
        // Then the window is closed (or the update tried again) before the run is over.
        assert!(!stop_and_wait(Duration::ZERO));
        drop(turn);
        assert!(closing(), "a later closing is not undone by the reopen before it");
        CLOSING_NOW.store(false, Ordering::SeqCst);
    }

    #[test]
    fn a_duplicate_goes_only_while_the_copy_that_stays_holds() {
        let _turn = one_at_a_time();
        let t = Temp::new("twin");
        let store = t.store();
        let content = b"the same content in both copies";
        let keep_path = t.file("keep.bin", content);
        let dup = t.file("dup.bin", content);
        assert_eq!(content_hash(&keep_path).unwrap(), blake3::hash(content).to_hex().to_string());
        let keep = |step| Keep { step, path: keep_path.clone(), size: content.len() as u64, hash: blake3::hash(content).to_hex().to_string() };
        let dedupe = |id: &str, steps: Vec<PlanStep>, keeps: Vec<Keep>| {
            let mut p = plan(id, &t.root(), "rules", steps);
            p.kind = "dedupe".into();
            store.write(&PlanFile { keeps, ..PlanFile::new(p) }).unwrap();
            run(&store, id, Work::Execute);
            store.load(id).unwrap().plan
        };

        // Another step of the plan takes the copy that stays: the duplicate stays too.
        let p = dedupe("p-twin-taken", vec![step(1, "trash", &dup, None), step(2, "rename", &keep_path, Some(&t.path("kept.bin")))], vec![keep(1)]);
        assert_eq!((p.steps[0].state.as_str(), p.steps[0].error.as_deref()), ("skipped", Some(TWIN_TAKEN)));
        fs::rename(t.path("kept.bin"), &keep_path).unwrap();

        // The copy that stays was changed (same size, other bytes).
        fs::write(&keep_path, b"THE SAME CONTENT IN BOTH COPIES").unwrap();
        let p = dedupe("p-twin-changed", vec![step(1, "trash", &dup, None)], vec![keep(1)]);
        assert_eq!((p.steps[0].state.as_str(), p.steps[0].error.as_deref()), ("skipped", Some(TWIN_CHANGED)));
        fs::write(&keep_path, content).unwrap();

        // The duplicate itself was changed.
        fs::write(&dup, b"the same content in both copieS").unwrap();
        let p = dedupe("p-dup-changed", vec![step(1, "trash", &dup, None)], vec![keep(1)]);
        assert_eq!(p.steps[0].error.as_deref(), Some(TWIN_CHANGED));
        fs::write(&dup, content).unwrap();

        // The copy that stays is gone: also when the draft is looked at again.
        let bin = win::has_recycle_bin(&t.root()) && !win::recycle_policy(&t.root()).nuke;
        fs::remove_file(&keep_path).unwrap();
        let mut draft = plan("p-twin-gone", &t.root(), "rules", vec![step(1, "trash", &dup, None)]);
        review(&mut draft, &Ctx::now(), &[keep(1)]);
        if bin {
            assert_eq!((draft.steps[0].state.as_str(), draft.steps[0].error.as_deref(), draft.steps[0].selected), ("skipped", Some(TWIN_GONE), false));
        }
        let p = dedupe("p-twin-gone", vec![step(1, "trash", &dup, None)], vec![keep(1)]);
        assert_eq!(p.steps[0].error.as_deref(), Some(TWIN_GONE));
        assert_eq!(fs::read(&dup).unwrap(), content, "the only copy left stayed");

        // No copy that stays written down for it (a file from an earlier build): it does not go.
        fs::write(&keep_path, content).unwrap();
        let p = dedupe("p-twin-unknown", vec![step(1, "trash", &dup, None)], vec![]);
        assert_eq!((p.steps[0].state.as_str(), p.steps[0].error.as_deref()), ("failed", Some(KEEP_UNKNOWN)));
        assert_eq!(fs::read(&dup).unwrap(), content);
        let mut draft = plan("p-twin-unknown", &t.root(), "rules", vec![step(1, "trash", &dup, None)]);
        draft.kind = "dedupe".into();
        review(&mut draft, &Ctx::now(), &[]);
        if bin {
            assert_eq!((draft.steps[0].state.as_str(), draft.steps[0].error.as_deref(), draft.steps[0].selected), ("skipped", Some(KEEP_UNKNOWN), false));
        }

        // Both as they were: it goes to the Recycle Bin (where there is one).
        fs::write(&keep_path, content).unwrap();
        if bin {
            let p = dedupe("p-twin-ok", vec![step(1, "trash", &dup, None)], vec![keep(1)]);
            assert_eq!(p.steps[0].state, "done", "{:?}", p.steps);
            assert!(!Path::new(&dup).exists() && Path::new(&keep_path).exists());
            run(&store, "p-twin-ok", Work::Undo);
            assert_eq!(fs::read(&dup).unwrap(), content);
        }
    }

    #[test]
    fn steps_in_a_folder_an_earlier_step_moves_are_kept() {
        let _turn = one_at_a_time();
        let t = Temp::new("chain");
        let store = t.store();
        fs::create_dir(t.path("A")).unwrap();
        t.file(r"A\x.txt", b"x");
        t.file(r"A\z.txt", b"z");
        t.file("c.txt", b"c");
        let mut draft = plan("p-chain", &t.root(), "mcp", vec![
            step(1, "move", &t.path("A"), Some(&t.path("B"))),
            // Where step 1 puts it.
            step(2, "rename", &t.path(r"B\x.txt"), Some(&t.path(r"B\y.txt"))),
            // Taken by what step 1 brings along.
            step(3, "move", &t.path("c.txt"), Some(&t.path(r"B\z.txt"))),
            step(4, "trash", &t.path(r"B\nothing.txt"), None),
        ]);
        review(&mut draft, &Ctx::now(), &[]);
        let states: Vec<(&str, Option<&str>)> = draft.steps.iter().map(|s| (s.state.as_str(), s.error.as_deref())).collect();
        assert_eq!(states, [("pending", None), ("pending", None), ("skipped", Some(NAME_TAKEN)), ("skipped", Some(MISSING))]);
        draft.source = "rules".into();
        store.write(&PlanFile::new(draft)).unwrap();
        run(&store, "p-chain", Work::Execute);
        let after = store.load("p-chain").unwrap();
        assert_eq!(after.plan.state, "done", "{:?}", after.plan.steps);
        assert_eq!(fs::read(t.path(r"B\y.txt")).unwrap(), b"x");
        run(&store, "p-chain", Work::Undo);
        assert_eq!(store.load("p-chain").unwrap().plan.state, "undone");
        assert_eq!(fs::read(t.path(r"A\x.txt")).unwrap(), b"x");
    }

    #[test]
    fn folders_holding_known_folders_and_short_names_are_refused() {
        let t = Temp::new("known");
        let root = t.root();
        fs::create_dir_all(t.path(r"Personal\Data\Documents")).unwrap();
        t.file(r"Personal\Data\Documents\memo.txt", b"m");
        t.file(r"Personal\other.txt", b"o");
        // Documents redirected into the test folder.
        let ctx = Ctx { guards: ops::Guards::now(), known: [key(&t.path(r"Personal\Data\Documents"))].into_iter().collect() };
        let why = |s: PlanStep| check(&ctx, &root, &s, None).err().unwrap_or_default();
        assert_eq!(why(step(1, "move", &t.path(r"Personal\Data"), Some(&t.path("Data")))), HOLDS_KNOWN);
        assert_eq!(why(step(1, "trash", &t.path("Personal"), None)), HOLDS_KNOWN);
        assert_eq!(why(step(1, "rename", &t.path(r"Personal\Data\Documents"), Some(&t.path(r"Personal\Data\Docs")))), KNOWN_FOLDER);
        // What is in it, and what is next to it, may move.
        assert!(check(&ctx, &root, &step(1, "move", &t.path(r"Personal\Data\Documents\memo.txt"), Some(&t.path("memo.txt"))), None).is_ok());
        assert!(check(&ctx, &root, &step(1, "rename", &t.path(r"Personal\other.txt"), Some(&t.path(r"Personal\other2.txt"))), None).is_ok());

        // A short (8.3) name stands for a folder the checks by name would not see.
        let dir = t.path("Long Folder Name For Test");
        fs::create_dir(&dir).unwrap();
        let file = t.file(r"Long Folder Name For Test\in.txt", b"i");
        let wide: Vec<u16> = Path::new(&dir).as_os_str().encode_wide().chain(Some(0)).collect();
        let mut buf = vec![0u16; 1024];
        let n = unsafe { windows_sys::Win32::Storage::FileSystem::GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
        let short = name_of(&String::from_utf16_lossy(&buf[..n.min(buf.len())])).to_string();
        // Only where the volume makes short names.
        if n > 0 && n < buf.len() && key(&short) != key("Long Folder Name For Test") {
            let via = format!("{root}\\{short}");
            assert_eq!(why(step(1, "rename", &format!("{via}\\in.txt"), Some(&format!("{via}\\out.txt")))), SHORT_NAME);
            assert_eq!(why(step(1, "move", &t.path(r"Personal\other.txt"), Some(&format!("{via}\\other.txt")))), SHORT_NAME);
            assert!(long_names(&file).is_ok());
        }
    }

    /// Whether the Recycle Bin lists an item that was just sent there. On some accounts (a CI
    /// service account) the shell recycles but cannot list or restore what it holds.
    fn bin_lists(original: &str) -> bool {
        trash::os_limited::list()
            .is_ok_and(|items| items.iter().any(|i| i.original_path().to_string_lossy().eq_ignore_ascii_case(original)))
    }

    #[test]
    fn what_comes_back_from_the_bin_never_lands_on_something_else() {
        let _turn = one_at_a_time();
        let t = Temp::new("bin-taken");
        if !win::has_recycle_bin(&t.root()) || win::recycle_policy(&t.root()).nuke {
            return;
        }
        let store = t.store();
        let name = format!("trash-me-{:08x}.txt", win::random_u64() as u32);
        let victim = t.file(&name, b"keep me safe");
        store.write(&PlanFile::new(plan("p-bin2", &t.root(), "rules", vec![step(1, "trash", &victim, None)]))).unwrap();
        run(&store, "p-bin2", Work::Execute);
        assert!(!Path::new(&victim).exists());
        if !bin_lists(&victim) {
            eprintln!("this account's Recycle Bin cannot be read back (a service account on CI); skipped");
            return;
        }
        // Something else took the name meanwhile: it stays, and the item stays in the bin.
        fs::write(&victim, b"someone else's").unwrap();
        run(&store, "p-bin2", Work::Undo);
        let after = store.load("p-bin2").unwrap();
        assert_eq!(after.plan.state, "partly");
        assert!(after.plan.steps[0].error.as_deref().is_some_and(|e| e.contains(ORIGINAL_TAKEN)), "{:?}", after.plan.steps);
        assert_eq!(fs::read(&victim).unwrap(), b"someone else's");
        // Once the name is free again, it comes back; no working name is left behind.
        fs::remove_file(&victim).unwrap();
        run(&store, "p-bin2", Work::Undo);
        assert_eq!(store.load("p-bin2").unwrap().plan.state, "undone");
        assert_eq!(fs::read(&victim).unwrap(), b"keep me safe");
        let left: Vec<String> = fs::read_dir(&t.0).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert!(!left.iter().any(|n| n.contains("ff-unbin")), "{left:?}");
    }

    /// An index of drive C: from (path below C:, is a folder, size, alloc, days old).
    fn index_of(entries: &[(&str, bool, u64, u64, i64)], now: i64) -> Index {
        let mut raw = vec![RawEntry { id: 1, parent: 1, name: ".".into(), flags: FLAG_DIR, size: 0, alloc: 0, modified: now }];
        let mut ids: HashMap<String, u64> = HashMap::new();
        for (path, is_dir, size, alloc, days) in entries {
            let parts: Vec<&str> = path.split('\\').collect();
            let mut parent = 1;
            for (i, part) in parts.iter().enumerate() {
                let here = parts[..=i].join("\\").to_lowercase();
                let last = i == parts.len() - 1;
                let next = ids.len() as u64 + 2;
                let id = *ids.entry(here).or_insert_with(|| {
                    let (flags, size, alloc) = if last && !is_dir { (0, *size, *alloc) } else { (FLAG_DIR, 0, 0) };
                    raw.push(RawEntry { id: next, parent, name: part.to_string(), flags, size, alloc, modified: now - days * DAY });
                    next
                });
                parent = id;
            }
        }
        Index::build("C:", "test", 0, raw, 1)
    }

    fn froms(steps: &[PlanStep]) -> Vec<(&str, &str, bool)> {
        steps.iter().map(|s| (s.from.as_str(), s.risk.as_str(), s.selected)).collect()
    }

    #[test]
    fn cleanup_finds_what_can_go_and_picks_the_safe_part() {
        let now = 1_000_000 * DAY;
        let ix = index_of(
            &[
                (r"Users\me\AppData\Local\Temp\old.tmp", false, 5 * MB, 5 * MB, 3),
                (r"Users\me\AppData\Local\Temp\fresh\x.bin", false, 2 * MB, 2 * MB, 0),
                (r"Users\me\AppData\Local\Temp\.a.ff-partial", false, MB, MB, 3),
                // An unlink's and a merge's working copies, which the job may be using right now.
                (r"Users\me\Downloads\.model.gguf.ff-unlink", false, 5 * GB, 5 * GB, 3),
                (r"Users\me\Downloads\.clip.mkv.ff-link", false, GB, GB, 3),
                (r"Users\me\AppData\Local\Google\Chrome\User Data\Default\Cache\data_1", false, 30 * MB, 30 * MB, 2),
                (r"Users\me\AppData\Local\Package Cache\{guid}\setup.msi", false, 40 * MB, 40 * MB, 400),
                (r"Users\me\Downloads\tool-setup.exe", false, 80 * MB, 80 * MB, 60),
                (r"Users\me\Downloads\photos.zip", false, 300 * MB, 300 * MB, 90),
                (r"Users\me\Downloads\new.msi", false, 10 * MB, 10 * MB, 5),
                (r"Users\me\Downloads\x.zip.crdownload", false, 7 * MB, 7 * MB, 10),
                (r"Users\me\old-disk.vhdx", false, 3 * GB, 3 * GB, 800),
                (r"Users\me\Documents\tmp\notes.txt", false, MB, MB, 100),
                (r"Users\me\Documents\report.docx", false, 2 * GB, 2 * GB, 900),
                (r"Windows\Temp\w.tmp", false, MB, MB, 10),
            ],
            now,
        );
        let top = ix.find(r"C:\Users\me").unwrap();
        let steps = cleanup_steps(&ix, top, r"C:\Users\me", now);
        let mut got = froms(&steps);
        got.sort();
        let mut want = vec![
            (r"C:\Users\me\AppData\Local\Google\Chrome\User Data\Default\Cache", "caution", false),
            (r"C:\Users\me\AppData\Local\Temp\fresh", "caution", false),
            (r"C:\Users\me\AppData\Local\Temp\old.tmp", "safe", true),
            (r"C:\Users\me\Documents\report.docx", "caution", false),
            (r"C:\Users\me\Documents\tmp\notes.txt", "caution", false),
            (r"C:\Users\me\Downloads\photos.zip", "caution", false),
            (r"C:\Users\me\Downloads\tool-setup.exe", "safe", true),
            (r"C:\Users\me\Downloads\x.zip.crdownload", "safe", true),
            (r"C:\Users\me\old-disk.vhdx", "caution", false),
        ];
        want.sort();
        assert_eq!(got, want);
        assert!(steps.iter().all(|s| s.action == "trash" && s.state == "pending" && !s.reason.is_empty()));
        // Biggest first.
        assert_eq!(steps[0].from, r"C:\Users\me\old-disk.vhdx");
        // A plan for the temp folder itself offers what is in it.
        let temp = ix.find(r"C:\Users\me\AppData\Local\Temp").unwrap();
        let inside = cleanup_steps(&ix, temp, r"C:\Users\me\AppData\Local\Temp", now);
        assert_eq!(froms(&inside).len(), 2);
        // Windows' own temp folder is Windows': a plan for the drive leaves it out.
        let all = cleanup_steps(&ix, ROOT, r"C:\", now);
        assert!(all.iter().all(|s| !s.from.starts_with(r"C:\Windows")));
    }

    #[test]
    fn shrink_offers_big_files_that_compress() {
        let now = 1_000_000 * DAY;
        let ix = index_of(
            &[
                (r"Users\me\Documents\server.log", false, 400 * MB, 400 * MB, 3),
                (r"Users\me\Documents\packed.log", false, 400 * MB, 90 * MB, 3),
                (r"Users\me\Documents\small.txt", false, MB, MB, 3),
                (r"Users\me\Documents\movie.mp4", false, 900 * MB, 900 * MB, 3),
                (r"Users\me\Pictures\scan.bmp", false, 50 * MB, 50 * MB, 30),
                (r"Users\me\Desktop\tool\tool.exe", false, 60 * MB, 60 * MB, 30),
                (r"Users\me\AppData\Local\app\trace.log", false, 500 * MB, 500 * MB, 3),
                (r"Tools\other.exe", false, 60 * MB, 60 * MB, 30),
                (r"Users\me\Documents\live.json", false, 100 * MB, 100 * MB, 0),
            ],
            now,
        );
        let steps = shrink_steps(&ix, ROOT, r"C:\", now);
        let got = froms(&steps);
        assert_eq!(
            got,
            [
                (r"C:\Users\me\Documents\server.log", "safe", false),
                (r"C:\Users\me\Documents\live.json", "caution", false),
                (r"C:\Users\me\Pictures\scan.bmp", "safe", false),
                (r"C:\Users\me\Desktop\tool\tool.exe", "caution", false),
            ]
        );
        assert!(steps.iter().all(|s| s.action == "compress" && s.reason.contains("예상 절약")));
        assert!(steps[0].reason.contains("280 MB"), "{}", steps[0].reason);
    }

    #[test]
    fn relocate_offers_the_biggest_folders_that_fit() {
        let now = 1_000_000 * DAY;
        let ix = index_of(
            &[
                (r"Games\Steam\steamapps\common\Big\data.pak", false, 40 * GB, 40 * GB, 10),
                (r"Users\me\Videos\raw\a.mkv", false, 12 * GB, 12 * GB, 10),
                (r"Users\me\AppData\Local\JetBrains\caches.bin", false, 6 * GB, 6 * GB, 10),
                (r"Users\me\small\x.bin", false, GB, GB, 10),
                (r"Program Files\Huge\h.bin", false, 50 * GB, 50 * GB, 10),
                (r"Windows\System32\big.bin", false, 30 * GB, 30 * GB, 10),
            ],
            now,
        );
        // As at the app: a profile, `Users` and AppData may not move whole; Videos is a known folder.
        let whole = |p: &str| {
            let k = key(p);
            [r"c:\users", r"c:\users\me", r"c:\users\me\appdata", r"c:\users\me\appdata\local", r"c:\users\me\videos"].contains(&k.as_str())
        };
        let steps = relocate_steps(&ix, ROOT, r"C:\", r"D:\", 100 * GB, &whole);
        let got: Vec<(&str, u64)> = steps.iter().map(|s| (s.from.as_str(), s.size / GB)).collect();
        assert_eq!(got, [(r"C:\Games", 40), (r"C:\Users\me\Videos\raw", 12), (r"C:\Users\me\AppData\Local\JetBrains", 6)]);
        assert!(steps.iter().all(|s| s.to.as_deref() == Some(r"D:\") && !s.selected && s.risk == "caution"));
        // Only what fits, with room to spare.
        let tight = relocate_steps(&ix, ROOT, r"C:\", r"D:\", 18 * GB, &whole);
        let got: Vec<&str> = tight.iter().map(|s| s.from.as_str()).collect();
        assert_eq!(got, [r"C:\Users\me\Videos\raw"]);
    }

    #[test]
    fn dedupe_keeps_the_newest_copy_that_is_no_programs() {
        let file = |path: &str, modified: i64, category: &'static str| DupFile { path: path.into(), modified, category, link_block: None, link_optin: false };
        let groups = vec![
            DupGroup {
                size: 10 * MB,
                hash: "a".into(),
                files: vec![
                    file(r"C:\Users\me\AppData\Roaming\app\copy.bin", 30, "program"),
                    file(r"C:\Users\me\Documents\copy.bin", 20, "document"),
                    file(r"C:\Users\me\Downloads\copy.bin", 10, "personal"),
                ],
            },
            DupGroup {
                size: 2 * MB,
                hash: "b".into(),
                files: vec![file(r"D:\Photos\a.jpg", 5, "media"), file(r"D:\Photos\Copy of a.jpg", 4, "media")],
            },
            DupGroup {
                size: 3 * MB,
                hash: "c".into(),
                files: vec![file(r"C:\Program Files\x\a.dll", 5, "program"), file(r"C:\Program Files\y\a.dll", 4, "program")],
            },
        ];
        let found = dedupe_steps(&groups);
        let (steps, keeps): (Vec<PlanStep>, Vec<Keep>) = found.into_iter().unzip();
        let got = froms(&steps);
        assert_eq!(got, [(r"C:\Users\me\Downloads\copy.bin", "safe", true), (r"D:\Photos\Copy of a.jpg", "safe", true)]);
        assert!(steps[0].reason.contains(r"C:\Users\me\Documents\copy.bin"), "{}", steps[0].reason);
        assert_eq!(steps[0].size, 10 * MB);
        // The copy that stays goes with each step, to be checked again before it runs.
        assert_eq!(keeps[0], Keep { step: 0, path: r"C:\Users\me\Documents\copy.bin".into(), size: 10 * MB, hash: "a".into() });
        assert_eq!((keeps[1].path.as_str(), keeps[1].hash.as_str()), (r"D:\Photos\a.jpg", "b"));
        assert!(steps.iter().all(|s| s.to.is_none()));
    }

    #[test]
    fn only_safe_trash_steps_start_selected() {
        assert!(trash_step(r"C:\x".into(), 1, "safe", String::new()).selected);
        assert!(!trash_step(r"C:\x".into(), 1, "caution", String::new()).selected);
        assert!(!trash_step(r"C:\x".into(), 1, "danger", String::new()).selected);
        // From AI or MCP, none: the user picks.
        let mut s = step(1, "trash", r"C:\Users\me\x.tmp", None);
        s.risk = "safe".into();
        let p = admit(plan("p-sel", r"C:\Users\me", "claude_cli", vec![s])).unwrap();
        assert!(!p.steps[0].selected);
    }

    #[test]
    fn trash_plans_of_other_rules_get_every_check() {
        let t = Temp::new("other-rules");
        fs::create_dir_all(t.path("Old tool")).unwrap();
        let item = |path: String, risk: &str| TrashItem { path, size: 60 * MB, risk: risk.into(), reason: "남은 폴더".into() };
        let items = vec![
            item(t.path("Old tool"), "caution"),
            // Program Files is never a plan's to touch, whoever made the plan.
            item(r"C:\Program Files\Common Files".into(), "caution"),
            item(t.path("gone"), "odd"),
        ];
        // A rules plan may be about a whole drive; its steps never are.
        let p = rules_trash(&t.store(), &Ctx::now(), r"C:\", "남은 프로그램 폴더 정리", items, |steps| format!("{}개", steps.len())).unwrap();
        assert_eq!((p.kind.as_str(), p.source.as_str(), p.summary.as_str()), ("cleanup", "rules", "3개"));
        assert_eq!(p.steps.iter().map(|s| s.id).collect::<Vec<_>>(), [1, 2, 3]);
        assert_eq!((p.steps[0].state.as_str(), p.steps[0].selected), ("pending", false));
        assert_eq!((p.steps[1].state.as_str(), p.steps[1].selected), ("skipped", false));
        assert_eq!(p.steps[1].error.as_deref(), Some(PROGRAM_ITEM));
        assert_eq!((p.steps[2].state.as_str(), p.steps[2].risk.as_str()), ("skipped", "caution"));
        assert_eq!(p.steps[2].error.as_deref(), Some(MISSING));
        assert_eq!(t.store().load(&p.id).unwrap().plan, p);
        assert!(rules_trash(&t.store(), &Ctx::now(), "relative", "x", Vec::new(), |_| String::new()).is_err());
    }
}
