//! Admin session: after one UAC prompt an elevated helper stays up for a while
//! (it ends by itself when idle) and answers read-only requests — list a folder
//! the user cannot open, read the start of a protected file, read a drive's
//! MFT for a full scan (as the scan helper does, scan/mod.rs, so a scan while
//! the session is on needs no UAC prompt of its own). It never writes.
//!
//! The helper is this program started as `fine-file.exe --admin-helper <pipe>`
//! (same pipe rules as the scan helper in scan/mod.rs and win.rs, with a pipe
//! that goes both ways). Requests and replies are length-prefixed JSON frames:
//! a little-endian u32 byte count, then that many bytes of JSON. The app sends
//! one request at a time and waits at most REQUEST_TIMEOUT for its reply. A
//! scan's reply is longer (`write_scan`): a JSON frame with the index's size,
//! the index (`Index::save_to` bytes) in frames of at most SCAN_CHUNK bytes,
//! and an empty frame; neither side holds more than one frame of it at a time
//! besides the index itself. A scan may take up to SCAN_TIMEOUT.
//!
//! The helper only opens what it is asked for with GENERIC_READ (or just
//! FILE_READ_ATTRIBUTES), never with write or delete access, and only paths on
//! a local drive letter (`X:\...`): no device, pipe or network paths, no SUBST
//! drives (they exist in the user's logon session only). Links on the way
//! could still lead elsewhere (a junction the user made, to an object
//! directory with a link to a pipe or a share), so:
//! - the app resolves the links on the way as the user before it asks
//!   (`for_helper`), and sends the path they lead to;
//! - the helper holds the folders on the way open, so none can be renamed or
//!   replaced meanwhile, and refuses a path with a link among them other than
//!   a volume mounted in a folder (`hold_the_way`); the item itself is opened
//!   without following a link where it can be;
//! - every open is anonymous (SECURITY_SQOS_PRESENT | SECURITY_ANONYMOUS: a
//!   pipe or server reached after all cannot act as the administrator);
//! - the helper does not follow junctions made by users who are not
//!   administrators (Redirection Guard, where Windows has it).

use crate::index::{EntryInfo, Index};
use crate::preview::{self, PreviewInfo};
use crate::scan::{self, HelperError};
use crate::win;
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{GetLastError, ERROR_NO_MORE_FILES, GENERIC_READ, HANDLE};
use windows_sys::Win32::Storage::FileSystem::{
    FileAttributeTagInfo, FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo, FileStandardInfo, GetFileInformationByHandleEx,
    GetFileType, GetFinalPathNameByHandleW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_SEQUENTIAL_SCAN, FILE_ID_BOTH_DIR_INFO,
    FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO,
    FILE_TYPE_DISK, SECURITY_ANONYMOUS, VOLUME_NAME_DOS,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Threading::{ProcessRedirectionTrustPolicy, SetProcessMitigationPolicy};

/// How long the app waits for one reply before it gives the session up.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// A session ends after this long without a request (what expires_in_s counts down).
const SESSION_IDLE: Duration = Duration::from_secs(10 * 60);
/// The helper waits a little longer than the app before it ends on its own,
/// so the app never sends a request to a helper that has just left.
const HELPER_GRACE: Duration = Duration::from_secs(10);
/// How long the helper tries to hand over one reply.
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);
/// Requests are small: an op and one path.
const MAX_REQUEST: usize = 64 << 10;
/// A listing of a very large folder still fits; anything bigger is refused.
const MAX_REPLY: usize = 64 << 20;
/// The helper reads at most this much of a file (the preview limit).
const READ_LIMIT: usize = preview::TEXT_LIMIT;
/// Index bytes in one frame of a scan's reply at most.
const SCAN_CHUNK: usize = 1 << 20;
/// How long a scan through the session may take, the MFT read and the
/// handing over of the index together.
const SCAN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Longest path taken, in UTF-16 units (`\\?\` goes in front of it).
const MAX_PATH_UNITS: usize = 32_000;

const NO_SESSION: &str = "관리자 모드가 꺼져 있어요. 다시 켜 주세요";
const BAD_PATH: &str = "경로가 올바르지 않아요";
const ODD_REPLY: &str = "관리자 도우미가 알 수 없는 응답을 보냈어요";
const NOT_INSTALLED: &str = "관리자 모드는 \"모든 사용자용\"(Program Files)으로 설치했을 때만 쓸 수 있어요.";
const CANCELLED: &str = "관리자 권한을 허락하지 않았어요";
const NOT_LOCAL: &str = "이 컴퓨터의 드라이브에 있는 항목만 열 수 있어요";
const DRIVE_NOT_LOCAL: &str =
    "이 컴퓨터의 드라이브에 있는 항목만 열 수 있어요 (네트워크 드라이브와 SUBST 드라이브는 관리자 권한으로 볼 수 없어요)";
const NO_BACKUP: &str = "백업 권한을 켜지 못해서 일부 항목은 열지 못할 수 있어요";
const LINK_ON_THE_WAY: &str =
    "경로에 정션·심볼릭 링크가 있어서 관리자 권한으로 열지 않았어요. 링크가 가리키는 폴더에서 다시 열어 보세요";
const UNTRUSTED_LINK: &str =
    "관리자 모드는 일반 사용자 권한으로 만든 정션(링크)을 따라가지 않아요. 링크가 가리키는 폴더에서 다시 열어 보세요";
const CHANGED: &str = "여는 사이에 항목이 바뀌어서 열지 않았어요. 다시 해 보세요";
/// What Redirection Guard gives for a junction it does not follow.
const ERROR_UNTRUSTED_MOUNT_POINT: i32 = 448;
const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
/// Reparse tags with this bit lead elsewhere (junctions, symbolic links, ...).
const NAME_SURROGATE: u32 = 0x2000_0000;
const FSCTL_GET_REPARSE_POINT: u32 = 0x0009_00A8;
const MAXIMUM_REPARSE_DATA_BUFFER_SIZE: usize = 16 * 1024;

#[derive(Serialize, Clone)]
pub struct AdminStatus {
    pub active: bool,
    /// Seconds until the session ends on its own (0 when not active).
    pub expires_in_s: u64,
    /// Why there is no session (cancelled UAC, not installed in Program Files, ...).
    pub detail: Option<String>,
}

// ---- protocol -------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Op {
    List,
    Read,
    /// The MFT of the drive in `path` (`C:`), for a full scan.
    Scan,
    Ping,
    Quit,
}

#[derive(Serialize, Deserialize, Debug)]
struct Request {
    op: Op,
    #[serde(default)]
    path: String,
}

impl Request {
    fn new(op: Op, path: &str) -> Request {
        Request { op, path: path.into() }
    }
}

/// One folder entry as the helper reads it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
struct Item {
    name: String,
    is_dir: bool,
    size: u64,
    /// FILETIME of the last write, like EntryInfo.modified.
    modified: i64,
    attributes: u32,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "reply", rename_all = "lowercase")]
enum Reply {
    /// `backup`: SeBackupPrivilege is on in the helper.
    Pong { backup: bool },
    Entries { entries: Vec<Item> },
    /// `data` (base64) is the start of the file, at most READ_LIMIT bytes;
    /// None when nothing was read: a folder, a link, an online-only file.
    File { size: u64, online_only: bool, data: Option<String> },
    /// A scan's index follows: `bytes` of it in frames (`write_scan`).
    Index { bytes: u64 },
    Bye,
    Error { message: String },
}

fn error(message: impl Into<String>) -> Reply {
    Reply::Error { message: message.into() }
}

/// Header and body go out in one write.
fn write_frame(w: &mut impl Write, body: &[u8]) -> io::Result<()> {
    let len = u32::try_from(body.len()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(body);
    w.write_all(&frame)
}

/// One frame's body. A length over `max` is refused before anything is
/// allocated for it; a stream that ends mid-frame is an error.
fn read_frame(r: &mut impl Read, max: usize) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("frame of {len} bytes is too large")));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(body)
}

fn send<T: Serialize>(w: &mut impl Write, msg: &T) -> io::Result<()> {
    write_frame(w, &serde_json::to_vec(msg).map_err(io::Error::other)?)
}

/// The app's half of one request: send it, read the reply.
fn exchange<C: Read + Write>(conn: &mut C, req: &Request) -> io::Result<Reply> {
    send(conn, req)?;
    let body = read_frame(conn, MAX_REPLY)?;
    serde_json::from_slice(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Writes into `w` as frames of at most SCAN_CHUNK bytes. A write of
/// nothing writes nothing: an empty frame ends the stream.
struct FramesOut<'a, W: Write>(&'a mut W);

impl<W: Write> Write for FramesOut<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let n = buf.len().min(SCAN_CHUNK);
        write_frame(self.0, &buf[..n])?;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// Reads what `FramesOut` wrote, `left` bytes in all, up to the empty frame
/// that ends it. More than announced, a frame over SCAN_CHUNK, or an end
/// before all of it came, is an error.
struct FramesIn<'a, R: Read> {
    r: &'a mut R,
    frame: Vec<u8>,
    at: usize,
    left: u64,
    ended: bool,
}

impl<'a, R: Read> FramesIn<'a, R> {
    fn new(r: &'a mut R, bytes: u64) -> Self {
        FramesIn { r, frame: Vec::new(), at: 0, left: bytes, ended: false }
    }
}

impl<R: Read> Read for FramesIn<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.at == self.frame.len() {
            if self.ended {
                return Ok(0);
            }
            let frame = read_frame(self.r, SCAN_CHUNK)?;
            if frame.is_empty() {
                if self.left != 0 {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the index ended early"));
                }
                self.ended = true;
                return Ok(0);
            }
            if frame.len() as u64 > self.left {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "more of the index than announced"));
            }
            self.left -= frame.len() as u64;
            self.frame = frame;
            self.at = 0;
        }
        let n = buf.len().min(self.frame.len() - self.at);
        buf[..n].copy_from_slice(&self.frame[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// The helper's reply to a scan: the index (see the module doc), or why the
/// MFT could not be read.
fn write_scan(w: &mut impl Write, result: &Result<Index, String>) -> io::Result<()> {
    match result {
        Ok(ix) => {
            send(w, &Reply::Index { bytes: ix.cache_len() as u64 })?;
            ix.save_to(&mut FramesOut(w))?;
            write_frame(w, &[])
        }
        Err(message) => send(w, &error(message.as_str())),
    }
}

/// The app's half of a scan's reply (`write_scan`): the MFT index of `drive`.
/// `Scan` when the helper could not read it (the stream is then in step for
/// the next request), `Helper` when the reply itself was wrong.
fn read_scan(r: &mut impl Read, drive: &str) -> Result<Index, HelperError> {
    let broken = |e: io::Error| HelperError::Helper(format!("관리자 도우미에게서 스캔 결과를 받지 못했어요: {e}"));
    let body = read_frame(r, MAX_REPLY).map_err(broken)?;
    match serde_json::from_slice::<Reply>(&body) {
        Ok(Reply::Index { bytes }) => {
            let mut frames = FramesIn::new(r, bytes);
            let ix = Index::load_from(&mut frames).map_err(broken)?;
            // Up to the end mark. What `load_from` read ahead is gone from
            // view, so the index's own length tells whether more came.
            let rest = io::copy(&mut frames, &mut io::sink()).map_err(broken)?;
            if !frames.ended || rest != 0 || ix.cache_len() as u64 != bytes {
                return Err(HelperError::Helper(ODD_REPLY.into()));
            }
            if ix.drive != drive || ix.method != "mft" {
                return Err(HelperError::Helper("다른 드라이브의 결과가 왔어요".into()));
            }
            Ok(ix)
        }
        Ok(Reply::Error { message }) => Err(HelperError::Scan(message)),
        _ => Err(HelperError::Helper(ODD_REPLY.into())),
    }
}

/// A path the helper may be asked about: a drive letter, `:\`, then plain
/// names (no `.` or `..`, no empty parts, no wildcards, stream names or
/// other characters Windows does not allow in names). Returned without a
/// trailing `\` except for the root (`C:\`). Anything else is None: device
/// paths (`\\.\`, `\\?\`), network paths, relative paths.
fn check_path(path: &str) -> Option<String> {
    if path.encode_utf16().count() > MAX_PATH_UNITS {
        return None;
    }
    let b = path.as_bytes();
    if b.len() < 2 || !b[0].is_ascii_alphabetic() || b[1] != b':' {
        return None;
    }
    let mut out = path[..2].to_string();
    let rest = &path[2..];
    if rest.is_empty() || rest == "\\" {
        out.push('\\');
        return Some(out);
    }
    let rest = rest.strip_prefix('\\')?;
    let rest = rest.strip_suffix('\\').unwrap_or(rest);
    for part in rest.split('\\') {
        let bad_char = |c: char| c < ' ' || matches!(c, '<' | '>' | ':' | '"' | '/' | '|' | '?' | '*');
        if part.is_empty() || part == "." || part == ".." || part.chars().any(bad_char) {
            return None;
        }
        out.push('\\');
        out.push_str(part);
    }
    Some(out)
}

fn child_path(dir: &str, name: &str) -> String {
    if dir.ends_with('\\') { format!("{dir}{name}") } else { format!("{dir}\\{name}") }
}

/// `hexdump -C` style: offset, 16 bytes in two groups of 8, then the bytes
/// as ASCII (`.` for anything else).
fn hex_dump(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(16) * 79);
    for (i, line) in bytes.chunks(16).enumerate() {
        let _ = write!(out, "{:08x}  ", i * 16);
        for j in 0..16 {
            match line.get(j) {
                Some(b) => {
                    let _ = write!(out, "{b:02x} ");
                }
                None => out.push_str("   "),
            }
            if j == 7 {
                out.push(' ');
            }
        }
        out.push_str(" |");
        out.extend(line.iter().map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '.' }));
        out.push_str("|\n");
    }
    out
}

/// What the preview pane shows for the bytes the helper read: text decoded
/// like preview_info (BOM, UTF-8, EUC-KR), else a hex dump (encoding "HEX").
fn preview_of(path: &str, size: u64, online_only: bool, data: Option<Vec<u8>>) -> PreviewInfo {
    let mut info = PreviewInfo {
        kind: "none".into(),
        text: None,
        truncated: false,
        encoding: None,
        size,
        mime: None,
        online_only,
    };
    let Some(buf) = data else { return info };
    let truncated = size > buf.len() as u64;
    let ext = Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    let head = &buf[..buf.len().min(preview::SNIFF_LEN)];
    let text = if preview::looks_like_text(head, head.len() < buf.len() || truncated) {
        Some(preview::decode(&buf, truncated))
    } else if preview::is_text_ext(&ext) {
        Some(preview::decode(&buf, truncated)).filter(|d| d.clean)
    } else {
        None
    };
    info.kind = "text".into();
    info.truncated = truncated;
    match text {
        Some(d) => {
            info.text = Some(d.text);
            info.encoding = Some(d.encoding.into());
        }
        None => {
            info.text = Some(hex_dump(&buf));
            info.encoding = Some("HEX".into());
        }
    }
    info
}

// ---- app side: the session ------------------------------------------------------

struct Session {
    /// Held for the whole of a request: one at a time.
    helper: Mutex<win::Elevated>,
    /// When the session ends unless a request comes first.
    expires: Mutex<Instant>,
    backup: bool,
}

static SESSION: Mutex<Option<Arc<Session>>> = Mutex::new(None);
/// One start at a time, so two clicks never bring up two UAC prompts.
static STARTING: Mutex<()> = Mutex::new(());

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The running session, or None. An expired one, or one whose helper is
/// gone, is ended here.
fn current() -> Option<Arc<Session>> {
    let mut slot = lock(&SESSION);
    let s = slot.clone()?;
    let expired = Instant::now() >= *lock(&s.expires);
    // A helper busy with a request is taken to be running.
    let gone = s.helper.try_lock().is_ok_and(|h| !h.running());
    if !(expired || gone) {
        return Some(s);
    }
    *slot = None;
    drop(slot);
    end(s);
    None
}

/// Asks the helper to quit (without waiting for its answer) and lets the
/// session go. If a request is still running, the connection closes when
/// that request is done, and the helper ends because its pipe broke.
fn end(s: Arc<Session>) {
    if let Ok(mut h) = s.helper.try_lock() {
        h.set_deadline(Some(Instant::now() + Duration::from_secs(1)));
        let _ = send(&mut *h, &Request::new(Op::Quit, ""));
    }
}

/// Removes `s` if it is still the session (another may have replaced it).
fn forget(s: &Arc<Session>) {
    let mut slot = lock(&SESSION);
    if slot.as_ref().is_some_and(|c| Arc::ptr_eq(c, s)) {
        *slot = None;
    }
}

fn status() -> AdminStatus {
    match current() {
        Some(s) => {
            let left = lock(&s.expires).saturating_duration_since(Instant::now());
            AdminStatus {
                active: true,
                expires_in_s: left.as_millis().div_ceil(1000) as u64,
                detail: (!s.backup).then(|| NO_BACKUP.into()),
            }
        }
        None => inactive(None),
    }
}

fn inactive(detail: Option<String>) -> AdminStatus {
    AdminStatus { active: false, expires_in_s: 0, detail }
}

fn start() -> AdminStatus {
    let _one = lock(&STARTING);
    if current().is_some() {
        return status();
    }
    // Elevating a binary the user can overwrite would hand administrator rights to
    // whatever replaced it. Development builds (target\debug) are exempt, as for scans.
    if !win::is_elevated() && !win::exe_in_program_files() && !cfg!(debug_assertions) {
        return inactive(Some(NOT_INSTALLED.into()));
    }
    let mut helper = match win::run_elevated_duplex("--admin-helper") {
        Ok(h) => h,
        Err(win::ElevateError::Cancelled) => return inactive(Some(CANCELLED.into())),
        Err(win::ElevateError::Failed(e)) => return inactive(Some(format!("관리자 도우미를 시작하지 못했어요: {e}"))),
    };
    helper.set_deadline(Some(Instant::now() + REQUEST_TIMEOUT));
    let backup = match exchange(&mut helper, &Request::new(Op::Ping, "")) {
        Ok(Reply::Pong { backup }) => backup,
        Ok(_) => return inactive(Some(ODD_REPLY.into())),
        Err(e) => return inactive(Some(format!("관리자 도우미가 응답하지 않아요: {e}"))),
    };
    helper.set_deadline(None);
    let s = Session { helper: Mutex::new(helper), expires: Mutex::new(Instant::now() + SESSION_IDLE), backup };
    *lock(&SESSION) = Some(Arc::new(s));
    status()
}

/// One request to the helper; its error replies come back as Err. A helper
/// that does not answer in time, or whose pipe broke, ends the session.
fn ask_blocking(req: &Request) -> Result<Reply, String> {
    let s = current().ok_or(NO_SESSION)?;
    let mut helper = lock(&s.helper);
    // Stopped or replaced while this waited its turn.
    if !lock(&SESSION).as_ref().is_some_and(|c| Arc::ptr_eq(c, &s)) {
        return Err(NO_SESSION.into());
    }
    helper.set_deadline(Some(Instant::now() + REQUEST_TIMEOUT));
    let result = exchange(&mut *helper, req);
    helper.set_deadline(None);
    match result {
        Ok(reply) => {
            // Every request restarts the helper's idle wait.
            *lock(&s.expires) = Instant::now() + SESSION_IDLE;
            match reply {
                Reply::Error { message } => Err(message),
                r => Ok(r),
            }
        }
        Err(e) => {
            drop(helper);
            forget(&s);
            Err(if e.kind() == io::ErrorKind::TimedOut {
                "관리자 도우미가 응답하지 않아 관리자 모드를 껐어요".into()
            } else {
                format!("관리자 도우미와 연결이 끊겼어요. 다시 켜 주세요 ({e})")
            })
        }
    }
}

/// A full scan of `drive` (`C:`) through the admin session, when one is on:
/// the helper reads the MFT, so there is no UAC prompt. None when there is
/// no session (the caller asks for administrator rights as it would without
/// one). A reply that went wrong ends the session (`Helper`); the helper
/// saying it could not read the MFT does not (`Scan`). Blocks for as long as
/// the scan takes; other requests to the session wait meanwhile.
pub fn scan_mft(drive: &str) -> Option<Result<Index, HelperError>> {
    let s = current()?;
    let mut helper = lock(&s.helper);
    // Stopped or replaced while this waited its turn.
    if !lock(&SESSION).as_ref().is_some_and(|c| Arc::ptr_eq(c, &s)) {
        return None;
    }
    helper.set_deadline(Some(Instant::now() + SCAN_TIMEOUT));
    let result = send(&mut *helper, &Request::new(Op::Scan, drive))
        .map_err(|e| HelperError::Helper(format!("관리자 도우미와 연결이 끊겼어요 ({e})")))
        .and_then(|()| read_scan(&mut *helper, drive));
    helper.set_deadline(None);
    if matches!(result, Ok(_) | Err(HelperError::Scan(_))) {
        // Every request restarts the helper's idle wait.
        *lock(&s.expires) = Instant::now() + SESSION_IDLE;
    } else {
        // Whatever is left of the reply would be taken for the next one.
        drop(helper);
        forget(&s);
    }
    Some(result)
}

/// `op` on `path` (from `local_path`), resolved for the helper first
/// (`for_helper`): a folder to list where its own link leads too, a file to
/// read up to the folder it is in.
async fn ask(op: Op, path: String) -> Result<Reply, String> {
    let run = move || ask_blocking(&Request::new(op, &for_helper(&path, op == Op::List)?));
    tauri::async_runtime::spawn_blocking(run).await.map_err(|e| e.to_string())?
}

fn entry_info(dir: &str, item: Item) -> EntryInfo {
    EntryInfo {
        path: child_path(dir, &item.name),
        name: item.name,
        is_dir: item.is_dir,
        size: if item.is_dir { 0 } else { item.size },
        alloc: 0,
        modified: item.modified,
        files: 0,
        hidden: item.attributes & 2 != 0,
        system: item.attributes & 4 != 0,
        reparse: item.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0,
    }
}

/// Asks for administrator rights once (UAC) and starts the helper.
#[tauri::command]
pub async fn admin_session_start() -> Result<AdminStatus, String> {
    tauri::async_runtime::spawn_blocking(start).await.map_err(|e| e.to_string())
}

/// Off the main thread, like the commands below: ending an expired session
/// writes to the helper's pipe, which may take up to a second.
#[tauri::command]
pub async fn admin_session_status() -> Result<AdminStatus, String> {
    tauri::async_runtime::spawn_blocking(status).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn admin_session_stop() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(stop).await.map_err(|e| e.to_string())
}

fn stop() {
    let s = lock(&SESSION).take();
    if let Some(s) = s {
        end(s);
    }
}

/// A path the helper may be asked about (`check_path`) on a drive of this
/// computer as the app sees it (`local_drive`).
fn local_path(path: &str) -> Result<String, String> {
    let path = check_path(path).ok_or(BAD_PATH)?;
    if !local_drive(&path[..2]) {
        return Err(DRIVE_NOT_LOCAL.into());
    }
    Ok(path)
}

/// Whether the elevated helper has the drive `letter` (`X:`) too. It runs in
/// another logon session, which has none of the user's mapped network drives
/// and SUBST drives; a drive letter that is not a volume's (one a program
/// made for this session, say) may not be there either.
fn local_drive(letter: &str) -> bool {
    !matches!(win::drive_kind(letter), "network" | "unknown") && !win::is_subst(letter) && win::volume_id(letter).is_some()
}

/// `path` (from `local_path`) as the helper is to open it: with the links on
/// the way resolved here, as the user, so the helper meets none. It follows
/// no junction a user made (Redirection Guard) and refuses links on the way
/// (`hold_the_way`), and a folder moved to another drive by fine-file is
/// such a junction. The deepest part of the path the user can open is
/// resolved (`std::fs::canonicalize`, which follows every link); the rest,
/// which only an administrator can open, goes after it as it is. Where the
/// links lead must be a local drive too. With `last` false the item itself
/// is left as it is (a file link is read as a link, which has nothing to show).
fn for_helper(path: &str, last: bool) -> Result<String, String> {
    let mut base = std::path::PathBuf::from(path);
    let mut rest: Vec<String> = Vec::new();
    if !last && let (Some(name), Some(parent)) = (base.file_name(), base.parent()) {
        rest.push(name.to_string_lossy().into_owned());
        base = parent.to_path_buf();
    }
    let real = loop {
        if let Ok(real) = std::fs::canonicalize(win::verbatim(&base)) {
            break real;
        }
        let (Some(name), Some(parent)) = (base.file_name(), base.parent()) else {
            // Not even the drive's root: the helper says why.
            return Ok(path.to_string());
        };
        rest.push(name.to_string_lossy().into_owned());
        base = parent.to_path_buf();
    };
    let real = real.to_str().and_then(|r| r.strip_prefix(r"\\?\")).filter(|r| r.as_bytes().get(1) == Some(&b':')).ok_or(NOT_LOCAL)?;
    let mut out = real.to_string();
    for name in rest.iter().rev() {
        out = child_path(&out, name);
    }
    local_path(&out)
}

/// A folder's entries, read by the helper (sizes of sub-folders unknown: 0).
/// Their paths are under `path` as asked, not where its links lead.
#[tauri::command]
pub async fn admin_list(path: String) -> Result<Vec<EntryInfo>, String> {
    let dir = local_path(&path)?;
    match ask(Op::List, dir.clone()).await? {
        Reply::Entries { entries } => Ok(entries.into_iter().map(|i| entry_info(&dir, i)).collect()),
        _ => Err(ODD_REPLY.into()),
    }
}

/// The start of a file, read by the helper: text (decoded like preview_info)
/// or, for binary files, a hex dump in `text` with encoding "HEX".
#[tauri::command]
pub async fn admin_read(path: String) -> Result<PreviewInfo, String> {
    let file = local_path(&path)?;
    match ask(Op::Read, file.clone()).await? {
        Reply::File { size, online_only, data } => {
            let data = match data {
                Some(d) => Some(base64::engine::general_purpose::STANDARD.decode(d).map_err(|_| ODD_REPLY)?),
                None => None,
            };
            if data.as_ref().is_some_and(|d| d.len() > READ_LIMIT) {
                return Err(ODD_REPLY.into());
            }
            Ok(preview_of(&file, size, online_only, data))
        }
        _ => Err(ODD_REPLY.into()),
    }
}

// ---- helper side ----------------------------------------------------------------

/// Entry point of `fine-file.exe --admin-helper \\.\pipe\fine-file-<pid>-<16 hex>`.
/// Like the scan helper it takes nothing but the pipe's name, checks who
/// serves it, and writes nothing but replies into it.
pub fn run_helper(args: &[String]) -> i32 {
    // Not in tests: they run the helper in their own process, and make junctions.
    if !cfg!(test) {
        distrust_user_junctions();
    }
    let [pipe] = args else { return 2 };
    let Some(server) = win::helper_pipe_server(pipe) else { return 2 };
    let Ok(mut conn) = win::open_duplex_pipe_client(pipe, server) else { return 3 };
    let backup = win::enable_backup_privilege();
    serve(&mut conn, SESSION_IDLE + HELPER_GRACE, backup)
}

/// Redirection Guard (PROCESS_MITIGATION_REDIRECTION_TRUST_POLICY with
/// EnforceRedirectionTrust): from now on this process does not follow a
/// junction made by a user who is not an administrator, so such a junction
/// on the way to a path it is asked about cannot send it anywhere else.
/// Windows versions without it refuse the call; then the checks of each open
/// (`open`, `on_local_drive`) are what there is.
fn distrust_user_junctions() {
    let enforce: u32 = 1;
    unsafe { SetProcessMitigationPolicy(ProcessRedirectionTrustPolicy, (&raw const enforce).cast(), size_of::<u32>()) };
}

/// Answers requests until "quit", until none came for `idle`, or until the
/// app is gone. 0 for those, 3 when the stream itself went wrong.
fn serve(conn: &mut win::PipeConn, idle: Duration, backup: bool) -> i32 {
    loop {
        conn.set_deadline(Some(Instant::now() + idle));
        let body = match read_frame(conn, MAX_REQUEST) {
            Ok(b) => b,
            Err(e) if matches!(e.kind(), io::ErrorKind::TimedOut | io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe) => {
                return 0;
            }
            Err(_) => return 3,
        };
        let (reply, quit) = match serde_json::from_slice::<Request>(&body) {
            Ok(req) if req.op == Op::Scan => {
                let result = scan_drive(&req.path);
                conn.set_deadline(Some(Instant::now() + SCAN_TIMEOUT));
                match write_scan(conn, &result) {
                    Ok(()) => continue,
                    Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return 0,
                    Err(_) => return 3,
                }
            }
            Ok(req) => (answer(&req, backup), req.op == Op::Quit),
            Err(_) => (error("알 수 없는 요청이에요"), false),
        };
        conn.set_deadline(Some(Instant::now() + WRITE_TIMEOUT));
        let mut out = serde_json::to_vec(&reply).unwrap_or_default();
        if out.len() > MAX_REPLY {
            out = serde_json::to_vec(&error("항목이 너무 많아서 보여 줄 수 없어요")).unwrap_or_default();
        }
        match write_frame(conn, &out) {
            Ok(()) if quit => return 0,
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return 0,
            Err(_) => return 3,
        }
    }
}

/// The MFT of the drive `path` names (`C:`; nothing else is taken), read as
/// the scan helper reads it: the volume is only opened for reading.
fn scan_drive(path: &str) -> Result<Index, String> {
    let drive = scan::normalize_drive(path).filter(|_| path.len() <= 3).ok_or(BAD_PATH)?;
    scan::scan_mft_guarded(&drive).map_err(|e| e.to_string())
}

fn answer(req: &Request, backup: bool) -> Reply {
    match req.op {
        Op::Ping => Reply::Pong { backup },
        Op::Quit => Reply::Bye,
        // Answered by `serve` itself: the reply is a stream.
        Op::Scan => error("알 수 없는 요청이에요"),
        Op::List | Op::Read => {
            let Some(path) = check_path(&req.path) else { return error(BAD_PATH) };
            let run = || if req.op == Op::List { list_dir(&path) } else { read_file(&path) };
            match catch_unwind(AssertUnwindSafe(run)) {
                Ok(Ok(reply)) => reply,
                Ok(Err(message)) => error(message),
                Err(_) => error("관리자 도우미에서 오류가 났어요"),
            }
        }
    }
}

/// Opens `path` exactly as written (`\\?\`), with backup semantics so the
/// backup privilege applies and folders open too. `access` is GENERIC_READ
/// or FILE_READ_ATTRIBUTES: never anything that could change the item.
/// Anonymous: should a link on the way lead to a pipe or a server after all
/// (`on_local_drive` can only tell once it is open), that side cannot act as
/// the administrator, as with the helpers' own pipe.
fn open(path: &str, access: u32, flags: u32) -> io::Result<File> {
    open_sharing(path, access, flags, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
}

/// `open`, letting others open it meanwhile only as `share` says.
fn open_sharing(path: &str, access: u32, flags: u32, share: u32) -> io::Result<File> {
    debug_assert!(access == GENERIC_READ || access == FILE_READ_ATTRIBUTES);
    std::fs::OpenOptions::new()
        .access_mode(access)
        .share_mode(share)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | flags)
        .security_qos_flags(SECURITY_ANONYMOUS)
        .open(win::verbatim(Path::new(path)))
}

/// Why `what` ("폴더를", "파일을") could not be opened, as the user reads it.
fn open_failed(what: &str, e: &io::Error) -> String {
    if e.raw_os_error() == Some(ERROR_UNTRUSTED_MOUNT_POINT) {
        return UNTRUSTED_LINK.into();
    }
    format!("{what} 열 수 없어요: {e}")
}

fn handle(f: &File) -> HANDLE {
    f.as_raw_handle() as HANDLE
}

/// The attributes and reparse tag of what `f` is (the link itself, when it
/// was opened with FILE_FLAG_OPEN_REPARSE_POINT).
fn tag_info(f: &File) -> io::Result<FILE_ATTRIBUTE_TAG_INFO> {
    let mut tag: FILE_ATTRIBUTE_TAG_INFO = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32;
    if unsafe { GetFileInformationByHandleEx(handle(f), FileAttributeTagInfo, (&raw mut tag).cast(), size) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(tag)
}

/// What an item opened as itself (FILE_FLAG_OPEN_REPARSE_POINT) leads to.
#[derive(Debug, PartialEq)]
enum Leads {
    /// Nowhere else: no reparse point, or one that only stands in for its own
    /// data (a cloud placeholder, a compressed or deduplicated file).
    Here,
    /// The root of a volume mounted in a folder (`\??\Volume{...}\`), which
    /// only administrators can mount: nothing on its way can be swapped.
    Volume,
    /// Anywhere else: a junction, a symbolic link, another kind of link.
    Elsewhere,
}

fn leads(f: &File) -> io::Result<Leads> {
    let tag = tag_info(f)?;
    if tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 || tag.ReparseTag & NAME_SURROGATE == 0 {
        return Ok(Leads::Here);
    }
    if tag.ReparseTag != IO_REPARSE_TAG_MOUNT_POINT {
        return Ok(Leads::Elsewhere);
    }
    let mut data = vec![0u8; MAXIMUM_REPARSE_DATA_BUFFER_SIZE];
    let mut len = 0u32;
    let got = unsafe {
        DeviceIoControl(handle(f), FSCTL_GET_REPARSE_POINT, null(), 0, data.as_mut_ptr().cast(), data.len() as u32, &mut len, null_mut())
    };
    if got == 0 {
        return Err(io::Error::last_os_error());
    }
    data.truncate(len as usize);
    Ok(if mount_target(&data).is_some_and(|t| is_volume_root(&t)) { Leads::Volume } else { Leads::Elsewhere })
}

/// The substitute name of a mount point's reparse data (`\??\...`).
fn mount_target(data: &[u8]) -> Option<String> {
    let half = |i: usize| data.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize);
    if u32::from_le_bytes(data.get(..4)?.try_into().ok()?) != IO_REPARSE_TAG_MOUNT_POINT {
        return None;
    }
    // After the 8-byte header: the substitute name's offset and length, from the names' start at 16.
    let (offset, len) = (half(8)?, half(10)?);
    let units: Vec<u16> = data.get(16 + offset..16 + offset + len)?.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    Some(String::from_utf16_lossy(&units))
}

/// `\??\Volume{xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx}\`: a volume's root and nothing more.
fn is_volume_root(target: &str) -> bool {
    let Some(guid) = target.strip_prefix(r"\??\Volume{").and_then(|t| t.strip_suffix(r"}\")) else { return false };
    guid.len() == 36
        && guid.char_indices().all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { c == '-' } else { c.is_ascii_hexdigit() })
}

/// Holds the folders on the way to `path` open (each below its drive's root,
/// without `path` itself), so that while they are held none of them can be
/// renamed, removed or replaced by a link, and refuses the way when one of
/// them leads elsewhere (`Leads::Elsewhere`): a junction a user made, also
/// one to `\RPC Control` with a link there to a pipe or a share, must not
/// lead the helper there, also where Windows has no Redirection Guard. The
/// app resolves the links on the way as the user (`for_helper`), so a link
/// here was made since, or is where only an administrator can look. A folder
/// that cannot be held (not even the backup privilege lets it be listed,
/// another program has it open to rename it) is still looked at.
fn hold_the_way(path: &str) -> Result<Vec<File>, String> {
    // `X:\` and the names after it, as `check_path` gives them.
    let (Some(root), Some(rest)) = (path.get(..3), path.get(3..)) else { return Err(BAD_PATH.into()) };
    let parts: Vec<&str> = rest.split('\\').filter(|p| !p.is_empty()).collect();
    let mut at = root.to_string();
    let mut held = Vec::new();
    for part in parts.iter().take(parts.len().saturating_sub(1)) {
        at = child_path(&at, part);
        let flags = FILE_FLAG_OPEN_REPARSE_POINT;
        let dir = open_sharing(&at, GENERIC_READ, flags, FILE_SHARE_READ | FILE_SHARE_WRITE)
            .or_else(|_| open(&at, FILE_READ_ATTRIBUTES, flags))
            .map_err(|e| open_failed("폴더를", &e))?;
        match leads(&dir) {
            Ok(Leads::Here | Leads::Volume) => held.push(dir),
            Ok(Leads::Elsewhere) => return Err(LINK_ON_THE_WAY.into()),
            Err(e) => return Err(open_failed("폴더를", &e)),
        }
    }
    Ok(held)
}

/// Whether what was opened is a file or folder on a local drive letter, as
/// the request said: following a link must not end up anywhere else.
fn on_local_drive(f: &File) -> bool {
    if unsafe { GetFileType(handle(f)) } != FILE_TYPE_DISK {
        return false;
    }
    let mut buf = vec![0u16; MAX_PATH_UNITS + 64];
    let n = unsafe {
        GetFinalPathNameByHandleW(handle(f), buf.as_mut_ptr(), buf.len() as u32, FILE_NAME_NORMALIZED | VOLUME_NAME_DOS)
    } as usize;
    if n == 0 || n >= buf.len() {
        return false;
    }
    let path = String::from_utf16_lossy(&buf[..n]);
    path.strip_prefix(r"\\?\").is_some_and(|p| {
        let b = p.as_bytes();
        b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\'
    })
}

fn standard_info(f: &File) -> io::Result<FILE_STANDARD_INFO> {
    let mut info: FILE_STANDARD_INFO = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<FILE_STANDARD_INFO>() as u32;
    if unsafe { GetFileInformationByHandleEx(handle(f), FileStandardInfo, (&raw mut info).cast(), size) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info)
}

fn list_dir(path: &str) -> Result<Reply, String> {
    let _way = hold_the_way(path)?;
    // The folder itself, not where a link there leads; a cloud placeholder
    // folder is then listed as it is here, without asking the sync app.
    let mut dir = open(path, GENERIC_READ, FILE_FLAG_OPEN_REPARSE_POINT).map_err(|e| open_failed("폴더를", &e))?;
    match leads(&dir).map_err(|e| open_failed("폴더를", &e))? {
        Leads::Here => {}
        // A volume mounted here: what to list is its root.
        Leads::Volume => dir = open(path, GENERIC_READ, 0).map_err(|e| open_failed("폴더를", &e))?,
        Leads::Elsewhere => return Err(LINK_ON_THE_WAY.into()),
    }
    if !on_local_drive(&dir) {
        return Err(NOT_LOCAL.into());
    }
    if standard_info(&dir).map_err(|e| format!("폴더를 열 수 없어요: {e}"))?.Directory == 0 {
        return Err("폴더가 아니에요".into());
    }
    // u64s keep the records 8-byte aligned; the slack past what the API gets
    // keeps a header read near the end in bounds.
    const BUF_BYTES: usize = 64 << 10;
    let mut buf = vec![0u64; BUF_BYTES / 8 + 16];
    let header = std::mem::size_of::<FILE_ID_BOTH_DIR_INFO>();
    let name_at = std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
    let mut entries = Vec::new();
    let mut class = FileIdBothDirectoryRestartInfo;
    loop {
        let ok = unsafe { GetFileInformationByHandleEx(handle(&dir), class, buf.as_mut_ptr().cast(), BUF_BYTES as u32) };
        if ok == 0 {
            let e = unsafe { GetLastError() };
            if e == ERROR_NO_MORE_FILES {
                break;
            }
            return Err(format!("폴더를 읽을 수 없어요: {}", io::Error::from_raw_os_error(e as i32)));
        }
        class = FileIdBothDirectoryInfo;
        let bytes: &[u8] = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast(), buf.len() * 8) };
        let mut at = 0usize;
        while at + header <= bytes.len() {
            let rec: FILE_ID_BOTH_DIR_INFO = unsafe { std::ptr::read_unaligned(bytes[at..].as_ptr().cast()) };
            let name_len = rec.FileNameLength as usize;
            let Some(raw) = bytes.get(at + name_at..at + name_at + name_len) else { break };
            let units: Vec<u16> = raw.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            let name = String::from_utf16_lossy(&units);
            if name != "." && name != ".." {
                let is_dir = rec.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
                entries.push(Item {
                    name,
                    is_dir,
                    size: if is_dir { 0 } else { rec.EndOfFile.max(0) as u64 },
                    modified: rec.LastWriteTime,
                    attributes: rec.FileAttributes,
                });
            }
            if rec.NextEntryOffset == 0 {
                break;
            }
            at += rec.NextEntryOffset as usize;
        }
    }
    Ok(Reply::Entries { entries })
}

fn read_file(path: &str) -> Result<Reply, String> {
    let _way = hold_the_way(path)?;
    // First a look at the item itself, without following a link and without
    // reading any data (which would download an online-only cloud file).
    let probe = open(path, FILE_READ_ATTRIBUTES, FILE_FLAG_OPEN_REPARSE_POINT).map_err(|e| open_failed("파일을", &e))?;
    if !on_local_drive(&probe) {
        return Err(NOT_LOCAL.into());
    }
    let tag = tag_info(&probe).map_err(|e| open_failed("파일을", &e))?;
    let size = standard_info(&probe).map(|s| s.EndOfFile.max(0) as u64).unwrap_or(0);
    drop(probe);
    // Folders, junctions and symbolic links have nothing to show (as in preview_info).
    let link = tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 && tag.ReparseTag & NAME_SURROGATE != 0;
    if tag.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 || link {
        return Ok(Reply::File { size, online_only: false, data: None });
    }
    if preview::is_online_only(tag.FileAttributes) {
        return Ok(Reply::File { size, online_only: true, data: None });
    }
    // A plain file is opened as itself again, so a link put in its place
    // since the look is not followed. One with a reparse point of its own
    // (compressed, deduplicated, a cloud file kept here) needs its filter
    // to read: that one is opened as usual.
    let plain = tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0;
    let flags = if plain { FILE_FLAG_SEQUENTIAL_SCAN | FILE_FLAG_OPEN_REPARSE_POINT } else { FILE_FLAG_SEQUENTIAL_SCAN };
    let file = open(path, GENERIC_READ, flags).map_err(|e| open_failed("파일을", &e))?;
    if plain && !tag_info(&file).is_ok_and(|t| t.FileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY) == 0) {
        return Err(CHANGED.into());
    }
    if !on_local_drive(&file) {
        return Err(NOT_LOCAL.into());
    }
    let size = standard_info(&file).map(|s| s.EndOfFile.max(0) as u64).unwrap_or(size);
    let mut data = Vec::new();
    (&file).take(READ_LIMIT as u64).read_to_end(&mut data).map_err(|e| format!("파일을 읽을 수 없어요: {e}"))?;
    let size = size.max(data.len() as u64);
    Ok(Reply::File { size, online_only: false, data: Some(base64::engine::general_purpose::STANDARD.encode(&data)) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ff-admin-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(win::verbatim(&dir));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn file_reply(reply: Reply) -> (u64, bool, Option<Vec<u8>>) {
        match reply {
            Reply::File { size, online_only, data } => {
                (size, online_only, data.map(|d| base64::engine::general_purpose::STANDARD.decode(d).unwrap()))
            }
            other => panic!("expected a file reply, got {other:?}"),
        }
    }

    #[test]
    fn frames_round_trip_and_bad_ones_are_refused() {
        let mut wire = Vec::new();
        send(&mut wire, &Request::new(Op::List, r"C:\Windows")).unwrap();
        send(&mut wire, &Reply::Pong { backup: true }).unwrap();
        let first = serde_json::to_vec(&Request::new(Op::List, r"C:\Windows")).unwrap();
        assert_eq!(wire[..4], (first.len() as u32).to_le_bytes());
        assert_eq!(wire[4..4 + first.len()], first[..]);
        let mut r = wire.as_slice();
        let req: Request = serde_json::from_slice(&read_frame(&mut r, MAX_REQUEST).unwrap()).unwrap();
        assert_eq!((req.op, req.path.as_str()), (Op::List, r"C:\Windows"));
        let reply: Reply = serde_json::from_slice(&read_frame(&mut r, MAX_REPLY).unwrap()).unwrap();
        assert_eq!(reply, Reply::Pong { backup: true });
        assert!(r.is_empty());
        // Cut short anywhere: an error, never a partial frame.
        for n in [0, 2, 4, 10] {
            assert!(read_frame(&mut &wire[..n], MAX_REQUEST).is_err(), "{n}");
        }
        // Too large: refused before reading it.
        let mut big = Vec::new();
        write_frame(&mut big, &vec![b' '; MAX_REQUEST + 1]).unwrap();
        assert_eq!(read_frame(&mut big.as_slice(), MAX_REQUEST).unwrap_err().kind(), io::ErrorKind::InvalidData);
        let mut huge = (u32::MAX).to_le_bytes().to_vec();
        huge.extend_from_slice(b"{}");
        assert_eq!(read_frame(&mut huge.as_slice(), MAX_REPLY).unwrap_err().kind(), io::ErrorKind::InvalidData);
        // The wire format of a request, as written by hand.
        let req: Request = serde_json::from_str(r#"{"op":"ping"}"#).unwrap();
        assert_eq!(req.op, Op::Ping);
        // Only these ops exist.
        let req: Request = serde_json::from_str(r#"{"op":"scan","path":"C:"}"#).unwrap();
        assert_eq!(req.op, Op::Scan);
        assert!(serde_json::from_str::<Request>(r#"{"op":"write","path":"C:\\x"}"#).is_err());
        assert!(serde_json::from_str::<Request>(r#"{"op":"delete","path":"C:\\x"}"#).is_err());
    }

    fn mft_index(drive: &str, files: usize) -> Index {
        use crate::index::{RawEntry, FLAG_DIR};
        let mut raw = vec![RawEntry { id: 5, parent: 5, name: ".".into(), flags: FLAG_DIR, size: 0, alloc: 0, modified: 0 }];
        // Long names, so the index takes several frames.
        raw.extend((0..files as u64).map(|i| RawEntry {
            id: 100 + i,
            parent: 5,
            name: format!("{i:06}-{}", "n".repeat(200)),
            flags: 0,
            size: i,
            alloc: i,
            modified: 0,
        }));
        Index::build(drive, "mft", 3, raw, 5)
    }

    fn scan_reply(result: &Result<Index, String>) -> Vec<u8> {
        let mut wire = Vec::new();
        write_scan(&mut wire, result).unwrap();
        wire
    }

    #[test]
    fn a_scan_comes_back_in_frames() {
        let ix = mft_index("C:", 12_000);
        assert!(ix.cache_len() > 2 * SCAN_CHUNK);
        let wire = scan_reply(&Ok(ix));
        // The size first, then frames of at most SCAN_CHUNK, then the end mark.
        let mut r = wire.as_slice();
        let Ok(Reply::Index { bytes }) = serde_json::from_slice(&read_frame(&mut r, MAX_REPLY).unwrap()) else { panic!("no size") };
        let mut total = 0;
        let mut frames = 0;
        loop {
            let f = read_frame(&mut r, SCAN_CHUNK).unwrap();
            if f.is_empty() {
                break;
            }
            total += f.len() as u64;
            frames += 1;
        }
        assert!(r.is_empty());
        assert_eq!(total, bytes);
        assert!(frames >= 3, "{frames}");
        // Read back whole, and the stream is in step for what follows.
        let mut two = wire.clone();
        send(&mut two, &Reply::Pong { backup: true }).unwrap();
        let mut r = two.as_slice();
        let back = read_scan(&mut r, "C:").ok().unwrap();
        assert_eq!(back.nodes.len(), 12_001);
        assert_eq!(back.nodes[crate::index::ROOT as usize].size, (0..12_000u64).sum::<u64>());
        assert!(back.find(&format!(r"C:\000042-{}", "n".repeat(200))).is_some());
        let next: Reply = serde_json::from_slice(&read_frame(&mut r, MAX_REPLY).unwrap()).unwrap();
        assert_eq!(next, Reply::Pong { backup: true });
        // Another drive's index, or a walk's, is not taken.
        assert!(matches!(read_scan(&mut wire.as_slice(), "D:"), Err(HelperError::Helper(_))));
        let mut walk = mft_index("C:", 3);
        walk.method = "walk".into();
        assert!(matches!(read_scan(&mut scan_reply(&Ok(walk)).as_slice(), "C:"), Err(HelperError::Helper(_))));
        // Cut off anywhere, before or after the index itself.
        for n in [0, 3, 30, wire.len() / 2, wire.len() - 4, wire.len() - 1] {
            assert!(matches!(read_scan(&mut &wire[..n], "C:"), Err(HelperError::Helper(_))), "{n}");
        }
    }

    #[test]
    fn a_scan_reply_that_does_not_add_up_is_refused() {
        let ix = mft_index("C:", 50);
        let mut body = Vec::new();
        ix.save_to(&mut body).unwrap();
        let reply = |announced: u64, chunks: &[&[u8]], end: bool| {
            let mut wire = Vec::new();
            send(&mut wire, &Reply::Index { bytes: announced }).unwrap();
            for c in chunks {
                write_frame(&mut wire, c).unwrap();
            }
            if end {
                write_frame(&mut wire, &[]).unwrap();
            }
            wire
        };
        let len = body.len() as u64;
        let (a, b) = body.split_at(body.len() / 2);
        assert!(read_scan(&mut reply(len, &[a, b], true).as_slice(), "C:").is_ok());
        // Less announced than sent, more announced than sent, bytes after the index, no end mark.
        assert!(matches!(read_scan(&mut reply(len - 1, &[a, b], true).as_slice(), "C:"), Err(HelperError::Helper(_))));
        assert!(matches!(read_scan(&mut reply(len + 1, &[a, b], true).as_slice(), "C:"), Err(HelperError::Helper(_))));
        assert!(matches!(read_scan(&mut reply(len + 3, &[a, b, b"xyz"], true).as_slice(), "C:"), Err(HelperError::Helper(_))));
        assert!(matches!(read_scan(&mut reply(len, &[a, b], false).as_slice(), "C:"), Err(HelperError::Helper(_))));
        // A frame over the limit is refused before it is read.
        let mut big = Vec::new();
        send(&mut big, &Reply::Index { bytes: u64::MAX }).unwrap();
        big.extend(((SCAN_CHUNK + 1) as u32).to_le_bytes());
        assert!(matches!(read_scan(&mut big.as_slice(), "C:"), Err(HelperError::Helper(_))));
        // The helper's own error is the scan's, and not a broken session.
        let wire = scan_reply(&Err("볼륨을 열 수 없어요".into()));
        assert!(matches!(read_scan(&mut wire.as_slice(), "C:"), Err(HelperError::Scan(m)) if m == "볼륨을 열 수 없어요"));
        // Anything else is not a scan's reply.
        let mut odd = Vec::new();
        send(&mut odd, &Reply::Bye).unwrap();
        assert!(matches!(read_scan(&mut odd.as_slice(), "C:"), Err(HelperError::Helper(_))));
        // The helper takes a drive and nothing else.
        for bad in ["", r"C:\Windows", r"\\.\C:", "CC:", "1:"] {
            assert_eq!(scan_drive(bad).err().as_deref(), Some(BAD_PATH), "{bad:?}");
        }
    }

    #[test]
    fn hex_dump_lines() {
        let mut bytes = b"Hello World\n".to_vec();
        bytes.extend([0u8, 1, 2, 3, 0x7f, 0xff, b'A']);
        let dump = hex_dump(&bytes);
        let lines: Vec<&str> = dump.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "00000000  48 65 6c 6c 6f 20 57 6f  72 6c 64 0a 00 01 02 03  |Hello World.....|");
        assert_eq!(lines[1], "00000010  7f ff 41                                          |..A|");
        // The ASCII column starts at the same place on a short line.
        assert_eq!(lines[0].find('|'), lines[1].find('|'));
        assert_eq!(hex_dump(&[]), "");
        assert_eq!(hex_dump(&[0u8; 32]).lines().nth(1).unwrap().get(..8), Some("00000010"));
    }

    #[test]
    fn only_local_drive_paths_are_taken() {
        assert_eq!(check_path(r"C:\Windows").as_deref(), Some(r"C:\Windows"));
        assert_eq!(check_path(r"C:\Windows\").as_deref(), Some(r"C:\Windows"));
        assert_eq!(check_path(r"C:\").as_deref(), Some(r"C:\"));
        assert_eq!(check_path("C:").as_deref(), Some(r"C:\"));
        assert_eq!(check_path(r"d:\사진\2024 여름").as_deref(), Some(r"d:\사진\2024 여름"));
        // Names Windows keeps as they are through `\\?\`.
        assert_eq!(check_path(r"C:\a\abc.").as_deref(), Some(r"C:\a\abc."));
        assert_eq!(check_path(r"C:\a\CON").as_deref(), Some(r"C:\a\CON"));
        for bad in [
            "",
            "C",
            r"C:Windows",
            r"\Windows",
            r"Windows\System32",
            r"C:\a\..\b",
            r"C:\a\.\b",
            r"C:\a\\b",
            r"C:\a\\",
            r"C:/Windows",
            r"C:\a/b",
            r"C:\a\*.txt",
            r"C:\a\b?",
            r"C:\a\file.txt:stream",
            "C:\\a\\b\0c",
            "C:\\a\\b\nc",
            r"\\.\PhysicalDrive0",
            r"\\.\C:",
            r"\\?\C:\Windows",
            r"\\?\GLOBALROOT\Device\HarddiskVolume1",
            r"\\server\share\x",
            r"\\.\pipe\fine-file-1-0123456789abcdef",
            r"1:\x",
        ] {
            assert_eq!(check_path(bad), None, "{bad:?}");
        }
        let long = format!(r"C:\{}", "a\\".repeat(MAX_PATH_UNITS));
        assert_eq!(check_path(&long), None);
        assert_eq!(child_path(r"C:\", "x"), r"C:\x");
        assert_eq!(child_path(r"C:\a", "x"), r"C:\a\x");
    }

    #[test]
    fn lists_and_reads_exact_names() {
        let dir = temp_dir("list");
        std::fs::write(dir.join("a.txt"), "hello").unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::fs::write(win::verbatim(&dir.join("abc.")), "dot").unwrap();
        std::fs::write(dir.join("abc"), "neighbour").unwrap();
        let p = dir.to_string_lossy().into_owned();
        let Ok(Reply::Entries { mut entries }) = list_dir(&p) else { panic!("no listing") };
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["a.txt", "abc", "abc.", "sub"]);
        assert_eq!((entries[0].size, entries[0].is_dir), (5, false));
        assert!(entries[0].modified > 0);
        assert!(entries[3].is_dir && entries[3].attributes & FILE_ATTRIBUTE_DIRECTORY != 0);
        // `abc.` is that file, not its neighbour.
        let (size, _, data) = file_reply(read_file(&format!(r"{p}\abc.")).unwrap());
        assert_eq!((size, data.as_deref()), (3, Some(&b"dot"[..])));
        // A folder has nothing to read; a missing item and a file listed as a folder are errors.
        assert_eq!(file_reply(read_file(&format!(r"{p}\sub")).unwrap()), (0, false, None));
        assert!(read_file(&format!(r"{p}\missing")).is_err());
        assert!(list_dir(&format!(r"{p}\a.txt")).is_err());
        assert!(list_dir(&format!(r"{p}\missing")).is_err());
        // An empty folder.
        let Ok(Reply::Entries { entries }) = list_dir(&format!(r"{p}\sub")) else { panic!("no listing") };
        assert!(entries.is_empty());
        // More entries than one buffer holds.
        let many = dir.join("many");
        std::fs::create_dir(&many).unwrap();
        for i in 0..1500 {
            std::fs::write(many.join(format!("file-with-a-longer-name-{i:04}.dat")), "").unwrap();
        }
        let Ok(Reply::Entries { entries }) = list_dir(&many.to_string_lossy()) else { panic!("no listing") };
        assert_eq!(entries.len(), 1500);
        std::fs::remove_dir_all(win::verbatim(&dir)).unwrap();
    }

    #[test]
    fn reads_at_most_the_limit() {
        let dir = temp_dir("read");
        let big: Vec<u8> = (0..READ_LIMIT + 1000).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.join("big.bin"), &big).unwrap();
        let path = dir.join("big.bin").to_string_lossy().into_owned();
        let (size, online_only, data) = file_reply(read_file(&path).unwrap());
        let data = data.unwrap();
        assert_eq!((size, online_only, data.len()), (big.len() as u64, false, READ_LIMIT));
        assert!(data[..] == big[..READ_LIMIT]);
        // Shown as a hex dump, cut at the limit.
        let info = preview_of(&path, size, false, Some(data));
        assert_eq!((info.kind.as_str(), info.encoding.as_deref(), info.truncated), ("text", Some("HEX"), true));
        assert_eq!(info.text.unwrap().lines().count(), READ_LIMIT / 16);
        std::fs::remove_dir_all(win::verbatim(&dir)).unwrap();
    }

    #[test]
    fn text_is_decoded_like_the_preview() {
        let info = preview_of(r"C:\a\notes.txt", 5, false, Some(b"hello".to_vec()));
        assert_eq!((info.kind.as_str(), info.text.as_deref(), info.encoding.as_deref()), ("text", Some("hello"), Some("UTF-8")));
        assert!(!info.truncated);
        // EUC-KR (CP949) for a file of unknown type too.
        let (euc, _, _) = encoding_rs::EUC_KR.encode("안녕하세요");
        let info = preview_of(r"C:\a\readme", euc.len() as u64, false, Some(euc.into_owned()));
        assert_eq!((info.text.as_deref(), info.encoding.as_deref()), (Some("안녕하세요"), Some("EUC-KR")));
        // UTF-16 by its BOM.
        let mut utf16 = vec![0xFF, 0xFE];
        utf16.extend("가a".encode_utf16().flat_map(|u| u.to_le_bytes()));
        let info = preview_of(r"C:\a\x.log", utf16.len() as u64, false, Some(utf16));
        assert_eq!((info.text.as_deref(), info.encoding.as_deref()), (Some("가a"), Some("UTF-16LE")));
        // Binary, even with a text extension.
        let info = preview_of(r"C:\a\x.txt", 4, false, Some(vec![0, 159, 146, 150]));
        assert_eq!(info.encoding.as_deref(), Some("HEX"));
        assert!(info.text.unwrap().starts_with("00000000  00 9f 92 96"));
        // Nothing read: nothing to show.
        let info = preview_of(r"C:\a\cloud.docx", 9000, true, None);
        assert_eq!((info.kind.as_str(), info.online_only, info.size), ("none", true, 9000));
        assert!(info.text.is_none());
    }

    #[test]
    fn helper_refuses_anything_but_its_pipe() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(run_helper(&args(&[])), 2);
        assert_eq!(run_helper(&args(&[r"C:\Users\me\file.txt"])), 2);
        assert_eq!(run_helper(&args(&[r"\\.\pipe\fine-file-1-0123456789abcdeg"])), 2);
        assert_eq!(run_helper(&args(&[r"\\.\pipe\fine-file-1-0123456789abcdef", r"C:\x"])), 2);
        // Well-formed, but nobody is listening.
        assert_eq!(run_helper(&args(&[r"\\.\pipe\fine-file-1-0123456789abcdef"])), 3);
    }

    #[test]
    fn helper_round_trip_through_the_pipe() {
        let dir = temp_dir("pipe");
        std::fs::write(dir.join("a.txt"), "hello").unwrap();
        std::fs::write(dir.join("b.bin"), [0u8, 1, 2, 255]).unwrap();
        let p = dir.to_string_lossy().into_owned();
        let pipe = win::tests::OwnPipe::duplex();
        let args = vec![pipe.name.clone()];
        let helper = std::thread::spawn(move || run_helper(&args));
        let mut conn = pipe.connection();
        conn.set_deadline(Some(Instant::now() + REQUEST_TIMEOUT));
        // Unelevated, the backup privilege cannot be had.
        assert_eq!(exchange(&mut conn, &Request::new(Op::Ping, "")).unwrap(), Reply::Pong { backup: win::is_elevated() });
        let Reply::Entries { entries } = exchange(&mut conn, &Request::new(Op::List, &p)).unwrap() else { panic!("no listing") };
        let mut infos: Vec<EntryInfo> = entries.into_iter().map(|i| entry_info(&p, i)).collect();
        infos.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(infos.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["a.txt", "b.bin"]);
        assert_eq!(infos[0].path, format!(r"{p}\a.txt"));
        assert_eq!(infos[0].size, 5);
        let text = format!(r"{p}\a.txt");
        let (size, online, data) = file_reply(exchange(&mut conn, &Request::new(Op::Read, &text)).unwrap());
        let info = preview_of(&text, size, online, data);
        assert_eq!((info.text.as_deref(), info.encoding.as_deref()), (Some("hello"), Some("UTF-8")));
        let bin = format!(r"{p}\b.bin");
        let (size, online, data) = file_reply(exchange(&mut conn, &Request::new(Op::Read, &bin)).unwrap());
        assert_eq!(preview_of(&bin, size, online, data).encoding.as_deref(), Some("HEX"));
        // Refused requests get an error and the helper keeps serving.
        for path in [r"\\.\PhysicalDrive0", r"C:\a\..\b", ""] {
            assert!(matches!(exchange(&mut conn, &Request::new(Op::Read, path)).unwrap(), Reply::Error { .. }), "{path}");
        }
        assert!(matches!(exchange(&mut conn, &Request::new(Op::List, &format!(r"{p}\missing"))).unwrap(), Reply::Error { .. }));
        write_frame(&mut conn, br#"{"op":"write","path":"C:\\x"}"#).unwrap();
        assert!(matches!(serde_json::from_slice::<Reply>(&read_frame(&mut conn, MAX_REPLY).unwrap()).unwrap(), Reply::Error { .. }));
        write_frame(&mut conn, b"not json").unwrap();
        assert!(matches!(serde_json::from_slice::<Reply>(&read_frame(&mut conn, MAX_REPLY).unwrap()).unwrap(), Reply::Error { .. }));
        // A scan: unelevated, the volume cannot be opened; the helper says so and serves on.
        if !win::is_elevated() {
            send(&mut conn, &Request::new(Op::Scan, "C:")).unwrap();
            assert!(matches!(read_scan(&mut conn, "C:"), Err(HelperError::Scan(m)) if !m.is_empty()));
        }
        send(&mut conn, &Request::new(Op::Scan, r"C:\Windows")).unwrap();
        assert!(matches!(read_scan(&mut conn, "C:"), Err(HelperError::Scan(m)) if m == BAD_PATH));
        assert_eq!(exchange(&mut conn, &Request::new(Op::Ping, "")).unwrap(), Reply::Pong { backup: win::is_elevated() });
        // Nothing in the folder changed.
        assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"hello");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        assert_eq!(exchange(&mut conn, &Request::new(Op::Quit, "")).unwrap(), Reply::Bye);
        assert_eq!(helper.join().unwrap(), 0);
        std::fs::remove_dir_all(win::verbatim(&dir)).unwrap();
    }

    #[test]
    fn helper_ends_when_idle_or_when_the_app_is_gone() {
        // Idle.
        let pipe = win::tests::OwnPipe::duplex();
        let name = pipe.name.clone();
        let helper = std::thread::spawn(move || {
            let mut c = win::open_duplex_pipe_client(&name, std::process::id()).unwrap();
            serve(&mut c, Duration::from_millis(200), false)
        });
        let mut conn = pipe.connection();
        let t = Instant::now();
        assert_eq!(helper.join().unwrap(), 0);
        assert!(t.elapsed() < Duration::from_secs(5));
        conn.set_deadline(Some(Instant::now() + Duration::from_secs(5)));
        assert!(exchange(&mut conn, &Request::new(Op::Ping, "")).is_err());
        // The app closes its end.
        let pipe = win::tests::OwnPipe::duplex();
        let name = pipe.name.clone();
        let helper = std::thread::spawn(move || {
            let mut c = win::open_duplex_pipe_client(&name, std::process::id()).unwrap();
            serve(&mut c, Duration::from_secs(60), false)
        });
        drop(pipe.connection());
        assert_eq!(helper.join().unwrap(), 0);
        // A frame too large to be a request breaks the stream.
        let pipe = win::tests::OwnPipe::duplex();
        let name = pipe.name.clone();
        let helper = std::thread::spawn(move || {
            let mut c = win::open_duplex_pipe_client(&name, std::process::id()).unwrap();
            serve(&mut c, Duration::from_secs(60), false)
        });
        let mut conn = pipe.connection();
        conn.write_all(&((MAX_REQUEST + 1) as u32).to_le_bytes()).unwrap();
        assert_eq!(helper.join().unwrap(), 3);
    }

    #[test]
    fn no_session_means_no_requests() {
        // Nothing was started in tests: requests are refused without a prompt.
        assert!(!status().active);
        assert_eq!(ask_blocking(&Request::new(Op::Ping, "")).unwrap_err(), NO_SESSION);
        // A scan without a session is left to the caller (which asks as before).
        assert!(scan_mft("C:").is_none());
        stop();
        assert!(!status().active);
        // The app turns away a drive the helper would not have, before asking it.
        assert_eq!(local_path(r"C:\a\..\b").unwrap_err(), BAD_PATH);
        if let Some(l) = ('D'..='Z').find(|l| win::drive_kind(&format!("{l}:")) == "unknown") {
            assert_eq!(local_path(&format!(r"{l}:\x")).unwrap_err(), DRIVE_NOT_LOCAL);
        }
        // A SUBST drive, which the helper's logon session does not have.
        if let Some(l) = win::drive_letters().into_iter().find(|l| win::is_subst(l)) {
            assert_eq!(local_path(&format!(r"{l}\x")).unwrap_err(), DRIVE_NOT_LOCAL);
        }
        assert!(local_path(&std::env::temp_dir().to_string_lossy()).is_ok());
    }

    #[test]
    fn links_are_resolved_by_the_app_and_refused_by_the_helper() {
        let dir = temp_dir("links");
        std::fs::create_dir(dir.join("real")).unwrap();
        std::fs::write(dir.join("real").join("a.txt"), "hello").unwrap();
        crate::ops::create_junction(&dir.join("link"), &dir.join("real")).unwrap();
        let p = dir.to_string_lossy().into_owned();
        let real = std::fs::canonicalize(&dir).unwrap().to_string_lossy().trim_start_matches(r"\\?\").to_string();
        // The app sends where the junction leads, also for a part that does not exist.
        assert_eq!(for_helper(&format!(r"{p}\link\a.txt"), false).unwrap(), format!(r"{real}\real\a.txt"));
        assert_eq!(for_helper(&format!(r"{p}\link\missing\x"), true).unwrap(), format!(r"{real}\real\missing\x"));
        assert_eq!(for_helper(&format!(r"{p}\link"), true).unwrap(), format!(r"{real}\real"));
        // A file to read stays itself: a link is read as a link.
        assert_eq!(for_helper(&format!(r"{p}\link"), false).unwrap(), format!(r"{real}\link"));
        // The helper itself never goes through it.
        assert_eq!(read_file(&format!(r"{p}\link\a.txt")).unwrap_err(), LINK_ON_THE_WAY);
        assert_eq!(list_dir(&format!(r"{p}\link")).unwrap_err(), LINK_ON_THE_WAY);
        assert_eq!(list_dir(&format!(r"{p}\link\sub")).unwrap_err(), LINK_ON_THE_WAY);
        // As itself, a link has nothing to read.
        assert_eq!(file_reply(read_file(&format!(r"{p}\link")).unwrap()), (0, false, None));
        let (_, _, data) = file_reply(read_file(&for_helper(&format!(r"{p}\link\a.txt"), false).unwrap()).unwrap());
        assert_eq!(data.as_deref(), Some(&b"hello"[..]));
        let Ok(Reply::Entries { entries }) = list_dir(&for_helper(&format!(r"{p}\link"), true).unwrap()) else { panic!("no listing") };
        assert_eq!(entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["a.txt"]);
        // Redirection Guard's refusal, in words.
        assert_eq!(open_failed("폴더를", &io::Error::from_raw_os_error(ERROR_UNTRUSTED_MOUNT_POINT)), UNTRUSTED_LINK);
        std::fs::remove_dir(dir.join("link")).unwrap();
        std::fs::remove_dir_all(win::verbatim(&dir)).unwrap();
    }

    #[test]
    fn only_a_volume_root_counts_as_a_mounted_volume() {
        assert!(is_volume_root(r"\??\Volume{0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0}\"));
        for other in [
            r"\??\Volume{0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0}\Games",
            r"\??\Volume{0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0}",
            r"\??\Volume{0f1e2d3c_4b5a-6978-8796-a5b4c3d2e1f0}\",
            r"\??\D:\",
            r"\??\D:\Games",
            r"\??\UNC\server\share\",
            r"\RPC Control\x",
        ] {
            assert!(!is_volume_root(other), "{other}");
        }
        // A junction's reparse data, as `mklink /J` writes it.
        let target = r"\??\C:\x";
        let units: Vec<u8> = target.encode_utf16().chain([0]).chain("C:\\x".encode_utf16()).chain([0]).flat_map(u16::to_le_bytes).collect();
        let s = (target.len() * 2) as u16;
        let mut data = IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes().to_vec();
        for half in [8 + units.len() as u16, 0, 0, s, s + 2, 8] {
            data.extend(half.to_le_bytes());
        }
        data.extend(units);
        assert_eq!(mount_target(&data).as_deref(), Some(target));
        assert_eq!(mount_target(&data[..10]), None);
    }
}
