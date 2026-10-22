//! Thumbnails (Windows shell thumbnails, so photos, videos and documents all
//! work) and the preview pane's content.
//!
//! Thumbnails are made on a small pool of COM threads and kept in an in-memory
//! LRU cache. A thumbnail provider that hangs only holds its own thread: after
//! RUN_LIMIT its caller gets an error ("try later", unlike None for "there is
//! none"), the file is not tried again until it changes or the provider is
//! done after all, and a new thread takes its place (up to MAX_THREADS in all).
//!
//! Online-only cloud files get only a thumbnail Windows already has cached:
//! making one would read the file, which downloads all of it.

use crate::win;
use base64::Engine;
use encoding_rs::{Encoding, EUC_KR, UTF_16BE, UTF_8};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::{self, File};
use std::hash::Hash;
use std::io::Read;
use std::os::windows::fs::MetadataExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::{Duration, Instant};
use tauri::async_runtime::{channel, Receiver, Sender};
use tauri::{AppHandle, Manager};
use windows::core::PCWSTR;
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
    HBITMAP,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF, SIIGBF_ICONONLY, SIIGBF_INCACHEONLY, SIIGBF_RESIZETOFIT,
    SIIGBF_THUMBNAILONLY,
};
use windows_sys::Win32::Foundation::{HANDLE, WAIT_FAILED, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{CreateSemaphoreW, ReleaseSemaphore};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MsgWaitForMultipleObjectsEx, PeekMessageW, TranslateMessage, MSG, MWMO_INPUTAVAILABLE, PM_REMOVE,
    QS_ALLINPUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN,
};

const THUMB_MIN: u32 = 16;
const THUMB_MAX: u32 = 512;
/// Up to this size a file without a thumbnail gets its icon instead (list
/// rows); bigger requests (grid, preview pane) get None and show their own.
const ICON_FALLBACK_MAX: u32 = 48;
/// Total size of the cached data URLs.
const CACHE_BYTES: usize = 64 << 20;
/// Per cache entry, on top of the path and the data URL.
const ENTRY_OVERHEAD: usize = 128;
const WORKERS: usize = 4;
/// Threads stuck in a thumbnail provider are replaced, up to this many in all.
const MAX_THREADS: usize = 8;
/// A provider still busy after this is given up on (the caller gets None).
const RUN_LIMIT: Duration = Duration::from_secs(10);
/// An idle thumbnail thread looks at the queue at least this often, should a wake-up ever go missing.
const IDLE_CHECK: Duration = Duration::from_secs(5);
/// What a caller hears when the provider is too slow or every thread is stuck.
const THUMB_LATER: &str = "썸네일을 아직 만들지 못했어요. 나중에 다시 시도해 주세요";
/// Text previews show at most this much of the file.
pub(crate) const TEXT_LIMIT: usize = 256 * 1024;
/// A file of unknown type is text if this much of its start looks like text.
pub(crate) const SNIFF_LEN: usize = 8 * 1024;

#[derive(Serialize)]
pub struct PreviewInfo {
    /// "image" | "video" | "audio" | "pdf" | "text" | "none"
    pub kind: String,
    /// Text kinds: the start of the file, decoded.
    pub text: Option<String>,
    /// The text was cut at the preview limit.
    pub truncated: bool,
    /// Text kinds: the encoding used to decode it (e.g. "UTF-8", "EUC-KR").
    pub encoding: Option<String>,
    pub size: u64,
    pub mime: Option<String>,
    /// An online-only cloud file (OneDrive and the like): reading any of it
    /// would download all of it, so kind is "none" and nothing was read.
    pub online_only: bool,
}

/// A PNG data URL (`data:image/png;base64,...`) of the item's shell thumbnail
/// at about `size` pixels, or None when Windows has none for it. An error
/// means "not now": its provider is too slow, or busy with others.
#[tauri::command]
pub async fn thumbnail(path: String, size: u32) -> Result<Option<String>, String> {
    let size = size.clamp(THUMB_MIN, THUMB_MAX);
    if path.contains('\0') || !Path::new(&path).is_absolute() {
        return Ok(None);
    }
    // The file's size and time are part of the cache key; reading them can
    // block (network drives), so not on the async runtime.
    let lookup = tauri::async_runtime::spawn_blocking(move || {
        let key = ThumbKey::of(&path, size)?;
        let hit = cache().lock().unwrap().get(&key);
        Some((key, hit))
    })
    .await
    .map_err(|e| e.to_string())?;
    let Some((key, hit)) = lookup else { return Ok(None) };
    let made = match hit {
        Some(cached) => cached,
        None => {
            let Some(mut rx) = submit(key) else { return Err(THUMB_LATER.into()) };
            rx.recv().await.unwrap_or(Cached::Late)
        }
    };
    match made {
        Cached::Made(thumb) => Ok(thumb.map(|t| t.to_string())),
        Cached::Late => Err(THUMB_LATER.into()),
    }
}

/// What the preview pane can show for `path`. For image, video, audio and pdf
/// kinds the file is added to the asset protocol scope, so the page can load
/// it with `convertFileSrc(path)`; nothing else is ever exposed.
#[tauri::command]
pub async fn preview_info(app: AppHandle, path: String) -> Result<PreviewInfo, String> {
    if path.contains('\0') || !Path::new(&path).is_absolute() {
        return Err("경로가 올바르지 않아요".into());
    }
    let p = path.clone();
    let info = tauri::async_runtime::spawn_blocking(move || inspect(Path::new(&p)))
        .await
        .map_err(|e| e.to_string())??;
    if is_media(&info.kind) {
        // Only this one file; the scope starts empty (tauri.conf.json).
        app.asset_protocol_scope()
            .allow_file(&path)
            .map_err(|e| format!("미리 보기를 열 수 없어요: {e}"))?;
    }
    Ok(info)
}

// ---- preview pane ---------------------------------------------------------------

fn is_media(kind: &str) -> bool {
    matches!(kind, "image" | "video" | "audio" | "pdf")
}

/// (kind, mime) of a media extension (lower case). SVG is meant for <img>,
/// where its scripts do not run.
fn media_kind(ext: &str) -> Option<(&'static str, &'static str)> {
    Some(match ext {
        "png" => ("image", "image/png"),
        "jpg" | "jpeg" => ("image", "image/jpeg"),
        "gif" => ("image", "image/gif"),
        "webp" => ("image", "image/webp"),
        "bmp" => ("image", "image/bmp"),
        "ico" => ("image", "image/x-icon"),
        "avif" => ("image", "image/avif"),
        "svg" => ("image", "image/svg+xml"),
        "mp4" => ("video", "video/mp4"),
        "webm" => ("video", "video/webm"),
        "mkv" => ("video", "video/x-matroska"),
        "mov" => ("video", "video/quicktime"),
        "m4v" => ("video", "video/x-m4v"),
        "mp3" => ("audio", "audio/mpeg"),
        "wav" => ("audio", "audio/wav"),
        "flac" => ("audio", "audio/flac"),
        "ogg" => ("audio", "audio/ogg"),
        "m4a" => ("audio", "audio/mp4"),
        "aac" => ("audio", "audio/aac"),
        "pdf" => ("pdf", "application/pdf"),
        _ => return None,
    })
}

pub(crate) fn is_text_ext(ext: &str) -> bool {
    matches!(
        ext,
        "txt" | "md" | "log" | "csv" | "json" | "xml" | "yml" | "yaml" | "toml" | "ini" | "cfg" | "rs" | "ts" | "tsx"
            | "js" | "jsx" | "py" | "c" | "h" | "cpp" | "java" | "kt" | "go" | "html" | "css" | "sql" | "bat" | "ps1"
            | "sh"
    )
}

/// Every file call here goes through `win::verbatim`, so `abc.` is that file
/// and not its neighbour `abc` (which plain Win32 parsing would open).
fn inspect(path: &Path) -> Result<PreviewInfo, String> {
    // Not following links: a link is not previewed as what it points to.
    let meta = fs::symlink_metadata(win::verbatim(path)).map_err(|e| format!("파일을 열 수 없어요: {e}"))?;
    let mut info = PreviewInfo {
        kind: "none".into(),
        text: None,
        truncated: false,
        encoding: None,
        size: meta.len(),
        mime: None,
        online_only: false,
    };
    // Folders, junctions and symbolic links have nothing to show.
    if !meta.is_file() {
        return Ok(info);
    }
    // Reading any of an online-only cloud file downloads all of it, and the
    // page loading it as media would too. The shell thumbnail stands in.
    if is_online_only(meta.file_attributes()) {
        info.online_only = true;
        return Ok(info);
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    // The page loads media through the asset protocol, which parses the path the Win32 way:
    // under a folder named `x.` it would get the file of the same name under `x`.
    if let Some((kind, mime)) = media_kind(&ext)
        && win::shell_can_name(path)
        && (kind != "pdf" || starts_as_pdf(path))
    {
        info.kind = kind.into();
        info.mime = Some(mime.into());
        return Ok(info);
    }
    let sniff = !is_text_ext(&ext);
    match read_text(path, sniff) {
        Ok(Some((buf, truncated))) => {
            let d = decode(&buf, truncated);
            info.kind = "text".into();
            info.text = Some(d.text);
            info.truncated = truncated;
            info.encoding = Some(d.encoding.into());
        }
        Ok(None) => {}
        // A file of unknown type that cannot be read just has no preview.
        Err(_) if sniff => {}
        Err(e) => return Err(format!("파일을 읽을 수 없어요: {e}")),
    }
    Ok(info)
}

/// Not downloaded yet from a cloud-sync provider. Folders that are not
/// listed yet carry RECALL_ON_OPEN instead.
pub(crate) fn is_online_only(attributes: u32) -> bool {
    attributes & (FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS | FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_OFFLINE) != 0
}

/// Whether the file begins the way a PDF does. The pdf kind is shown in a
/// frame, and the asset protocol serves a file by what its content looks
/// like, else by its extension, where an unknown one means HTML: a web page
/// saved as `invoice.pdf` would run its scripts in the app's window.
/// `%PDF-` at the very start makes it serve application/pdf.
fn starts_as_pdf(path: &Path) -> bool {
    let mut head = [0u8; 5];
    File::open(win::verbatim(path)).and_then(|mut f| f.read_exact(&mut head)).is_ok() && &head == b"%PDF-"
}

/// The start of the file, at most TEXT_LIMIT bytes, and whether there is more.
/// With `sniff`, None unless its first SNIFF_LEN bytes look like text.
fn read_text(path: &Path, sniff: bool) -> std::io::Result<Option<(Vec<u8>, bool)>> {
    let mut file = File::open(win::verbatim(path))?;
    let mut buf = Vec::new();
    if sniff {
        file.by_ref().take(SNIFF_LEN as u64).read_to_end(&mut buf)?;
        if !looks_like_text(&buf, buf.len() == SNIFF_LEN) {
            return Ok(None);
        }
    }
    // One byte past the limit tells whether the file goes on.
    file.by_ref().take((TEXT_LIMIT + 1 - buf.len()) as u64).read_to_end(&mut buf)?;
    let truncated = buf.len() > TEXT_LIMIT;
    buf.truncate(TEXT_LIMIT);
    Ok(Some((buf, truncated)))
}

/// Whether the start of a file of unknown type is text: it decodes cleanly
/// (BOM, UTF-8 or EUC-KR) and has no NUL or other binary control characters.
pub(crate) fn looks_like_text(head: &[u8], cut: bool) -> bool {
    let d = decode(head, cut);
    d.clean && !d.text.chars().any(|c| c < ' ' && !matches!(c, '\t' | '\n' | '\r' | '\x0c' | '\x1a' | '\x1b'))
}

pub(crate) struct Decoded {
    pub(crate) text: String,
    pub(crate) encoding: &'static str,
    /// Decoded without replacement characters.
    pub(crate) clean: bool,
}

/// Decodes the start of a text file: by its BOM (UTF-8, UTF-16LE/BE), else as
/// UTF-8, else as EUC-KR (CP949), else as UTF-8 with replacement characters.
/// With `cut` (the buffer stops before the file does) a character split at
/// the end is dropped instead of counting as an error.
pub(crate) fn decode(buf: &[u8], cut: bool) -> Decoded {
    if let Some((enc, bom_len)) = Encoding::for_bom(buf) {
        let body = &buf[bom_len..];
        if enc == UTF_8 {
            return match utf8_prefix(body, cut) {
                Some(s) => Decoded { text: s.into(), encoding: "UTF-8", clean: true },
                None => Decoded { text: String::from_utf8_lossy(body).into_owned(), encoding: "UTF-8", clean: false },
            };
        }
        let body = if cut { utf16_whole(body, enc == UTF_16BE) } else { body };
        let (text, had_errors) = enc.decode_without_bom_handling(body);
        return Decoded { text: text.into_owned(), encoding: enc.name(), clean: !had_errors };
    }
    if let Some(s) = utf8_prefix(buf, cut) {
        return Decoded { text: s.into(), encoding: "UTF-8", clean: true };
    }
    if let Some(s) = euc_kr(buf, cut) {
        return Decoded { text: s, encoding: "EUC-KR", clean: true };
    }
    Decoded { text: String::from_utf8_lossy(buf).into_owned(), encoding: "UTF-8", clean: false }
}

fn utf8_prefix(b: &[u8], cut: bool) -> Option<&str> {
    match std::str::from_utf8(b) {
        Ok(s) => Some(s),
        // Only the last character is incomplete: the limit split it.
        Err(e) if cut && e.error_len().is_none() => std::str::from_utf8(&b[..e.valid_up_to()]).ok(),
        Err(_) => None,
    }
}

/// UTF-16 bytes without a half code unit or a lone high surrogate at the end.
fn utf16_whole(b: &[u8], big_endian: bool) -> &[u8] {
    let mut end = b.len() & !1;
    if end >= 2 {
        let pair = [b[end - 2], b[end - 1]];
        let last = if big_endian { u16::from_be_bytes(pair) } else { u16::from_le_bytes(pair) };
        if (0xD800..0xDC00).contains(&last) {
            end -= 2;
        }
    }
    &b[..end]
}

/// EUC-KR as Windows reads it (CP949, encoding_rs's EUC-KR), or None if the
/// bytes are not valid in it.
fn euc_kr(b: &[u8], cut: bool) -> Option<String> {
    if let Some(s) = EUC_KR.decode_without_bom_handling_and_without_replacement(b) {
        return Some(s.into_owned());
    }
    // The limit may have split the last two-byte character.
    if cut && b.len() > 1 {
        return EUC_KR.decode_without_bom_handling_and_without_replacement(&b[..b.len() - 1]).map(|s| s.into_owned());
    }
    None
}

// ---- thumbnail cache ------------------------------------------------------------

/// A thumbnail, or None for "Windows has none" (cached too, so a folder full
/// of files without thumbnails is asked once).
type Thumb = Option<Arc<str>>;

/// What the cache knows of a thumbnail, and what a thumbnail thread answers.
#[derive(Clone, PartialEq, Debug)]
enum Cached {
    Made(Thumb),
    /// Its provider was still busy after RUN_LIMIT. Not asked again until
    /// the file changes, or the provider is done after all and the result
    /// takes this one's place.
    Late,
}

/// A changed file (time or size) gets a new key, so stale thumbnails are
/// never served; old ones just age out.
#[derive(Clone, PartialEq, Eq, Hash)]
struct ThumbKey {
    path: String,
    size: u32,
    /// FILETIME of the last write.
    modified: u64,
    len: u64,
    /// Online-only cloud file: only a thumbnail Windows has cached. Once the
    /// file is downloaded it gets a key of its own and a full try.
    online_only: bool,
}

impl ThumbKey {
    fn of(path: &str, size: u32) -> Option<Self> {
        let meta = fs::metadata(path).ok()?;
        Some(Self {
            path: path.to_string(),
            size,
            modified: meta.last_write_time(),
            len: meta.len(),
            online_only: is_online_only(meta.file_attributes()),
        })
    }

    fn weight(&self, cached: &Cached) -> usize {
        let thumb = match cached {
            Cached::Made(Some(t)) => t.len(),
            _ => 0,
        };
        ENTRY_OVERHEAD + self.path.len() + thumb
    }
}

fn cache() -> &'static Mutex<Lru<ThumbKey, Cached>> {
    static CACHE: OnceLock<Mutex<Lru<ThumbKey, Cached>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Lru::new(CACHE_BYTES)))
}

fn cache_put(key: &ThumbKey, cached: Cached) {
    let weight = key.weight(&cached);
    cache().lock().unwrap().insert(key.clone(), cached, weight);
}

/// A least-recently-used map capped by the total weight (bytes) of its entries.
struct Lru<K, V> {
    cap: usize,
    used: usize,
    tick: u64,
    /// key → (value, weight, last use)
    map: HashMap<K, (V, usize, u64)>,
    /// last use → key, oldest first
    order: BTreeMap<u64, K>,
}

impl<K: Clone + Eq + Hash, V: Clone> Lru<K, V> {
    fn new(cap: usize) -> Self {
        Self { cap, used: 0, tick: 0, map: HashMap::new(), order: BTreeMap::new() }
    }

    fn get(&mut self, key: &K) -> Option<V> {
        self.tick += 1;
        let (value, _, used) = self.map.get_mut(key)?;
        self.order.remove(used);
        *used = self.tick;
        self.order.insert(self.tick, key.clone());
        Some(value.clone())
    }

    /// An entry heavier than the whole cache is not kept.
    fn insert(&mut self, key: K, value: V, weight: usize) {
        if let Some((_, w, used)) = self.map.remove(&key) {
            self.used -= w;
            self.order.remove(&used);
        }
        if weight > self.cap {
            return;
        }
        while self.used + weight > self.cap {
            let Some((_, old)) = self.order.pop_first() else { break };
            if let Some((_, w, _)) = self.map.remove(&old) {
                self.used -= w;
            }
        }
        self.tick += 1;
        self.order.insert(self.tick, key.clone());
        self.map.insert(key, (value, weight, self.tick));
        self.used += weight;
    }
}

// ---- thumbnail threads ----------------------------------------------------------

struct Job {
    key: ThumbKey,
    reply: Sender<Cached>,
}

struct Running {
    id: u64,
    key: ThumbKey,
    since: Instant,
    reply: Sender<Cached>,
}

struct PoolState {
    queue: VecDeque<Job>,
    running: Vec<Running>,
    next_id: u64,
    threads: usize,
    /// Threads the watchdog gave up on that are still inside a provider.
    stuck: usize,
}

struct Pool {
    state: Mutex<PoolState>,
    /// A semaphore released once per queued job. Idle threads wait on it
    /// while handling their window messages (see `wait_pumping`), which a
    /// Condvar would not let them do.
    wake: Semaphore,
}

/// A Windows semaphore handle, made once and never closed.
struct Semaphore(HANDLE);

// SAFETY: a kernel handle may be used from any thread.
unsafe impl Send for Semaphore {}
unsafe impl Sync for Semaphore {}

fn pool() -> &'static Pool {
    static PLACE: OnceLock<Pool> = OnceLock::new();
    static START: Once = Once::new();
    let pool = PLACE.get_or_init(|| Pool {
        state: Mutex::new(PoolState { queue: VecDeque::new(), running: Vec::new(), next_id: 0, threads: 0, stuck: 0 }),
        wake: Semaphore(unsafe { CreateSemaphoreW(null(), 0, i32::MAX, null()) }),
    });
    START.call_once(|| {
        let mut st = pool.state.lock().unwrap();
        for _ in 0..WORKERS {
            spawn_worker(&mut st);
        }
        let _ = std::thread::Builder::new().name("ff-thumb-watch".into()).spawn(watchdog);
    });
    pool
}

fn spawn_worker(st: &mut PoolState) {
    st.threads += 1;
    if std::thread::Builder::new().name("ff-thumb".into()).spawn(worker).is_err() {
        st.threads -= 1;
    }
}

/// Queues a thumbnail; the receiver gets it (or Late). None at once when every
/// thread is stuck in a provider.
fn submit(key: ThumbKey) -> Option<Receiver<Cached>> {
    let pool = pool();
    let (tx, rx) = channel(1);
    let mut st = pool.state.lock().unwrap();
    if st.threads == st.stuck {
        return None;
    }
    st.queue.push_back(Job { key, reply: tx });
    drop(st);
    unsafe { ReleaseSemaphore(pool.wake.0, 1, null_mut()) };
    Some(rx)
}

/// COM for the calling thread, as a single-threaded apartment (what shell
/// thumbnail providers expect). Uninitialized when dropped.
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

fn worker() {
    let _com = Com::enter();
    let pool = pool();
    loop {
        let (id, key) = next_job(pool);
        // An earlier request for the same file may have made it (or hung on it) meanwhile.
        let hit = cache().lock().unwrap().get(&key);
        let made = hit.is_none();
        let answer =
            hit.unwrap_or_else(|| Cached::Made(catch_unwind(AssertUnwindSafe(|| render(&key))).unwrap_or(None)));
        let mut st = pool.state.lock().unwrap();
        // Under the pool lock, so a late result replaces the watchdog's Late
        // rather than the other way round.
        if made {
            cache_put(&key, answer.clone());
        }
        if let Some(i) = st.running.iter().position(|r| r.id == id) {
            let _ = st.running.swap_remove(i).reply.try_send(answer);
        } else {
            // The watchdog gave up on this one and started another thread.
            st.stuck -= 1;
            if st.threads - st.stuck > WORKERS {
                st.threads -= 1;
                return;
            }
        }
    }
}

fn next_job(pool: &Pool) -> (u64, ThumbKey) {
    loop {
        let mut st = pool.state.lock().unwrap();
        while let Some(job) = st.queue.pop_front() {
            // The caller went away.
            if job.reply.is_closed() {
                continue;
            }
            let id = st.next_id;
            st.next_id += 1;
            st.running.push(Running { id, key: job.key.clone(), since: Instant::now(), reply: job.reply });
            return (id, job.key);
        }
        drop(st);
        // A release for a job another thread took on its way leaves the
        // count high: this one then wakes, finds nothing and waits again.
        wait_pumping(pool.wake.0, IDLE_CHECK);
    }
}

/// Waits until `handle` is signalled or `timeout` has passed, handling this
/// thread's window messages meanwhile. A single-threaded apartment must:
/// COM calls into it and callbacks a provider posts to it arrive as
/// messages, and would otherwise wait until the provider gives up.
fn wait_pumping(handle: HANDLE, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now()).as_millis().min(u32::MAX as u128) as u32;
        let woke = unsafe { MsgWaitForMultipleObjectsEx(1, &handle, left, QS_ALLINPUT, MWMO_INPUTAVAILABLE) };
        if woke == WAIT_FAILED {
            // No semaphore after all: look at the queue now and then instead of spinning.
            std::thread::sleep(Duration::from_millis(50));
            return;
        }
        if woke != WAIT_OBJECT_0 + 1 {
            // Signalled or timed out: the caller looks at the queue either way.
            return;
        }
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        while unsafe { PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) } != 0 {
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

/// Answers Late for thumbnails that take longer than RUN_LIMIT and replaces
/// their threads.
fn watchdog() {
    let pool = pool();
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let mut st = pool.state.lock().unwrap();
        let now = Instant::now();
        while let Some(i) = st.running.iter().position(|r| now.duration_since(r.since) >= RUN_LIMIT) {
            let late = st.running.swap_remove(i);
            let _ = late.reply.try_send(Cached::Late);
            // Not tried again until the file changes (or its late result lands).
            cache_put(&late.key, Cached::Late);
            st.stuck += 1;
            if st.threads < MAX_THREADS {
                spawn_worker(&mut st);
            }
        }
        if st.threads == st.stuck {
            for job in std::mem::take(&mut st.queue) {
                let _ = job.reply.try_send(Cached::Late);
            }
        }
    }
}

// ---- shell thumbnails -----------------------------------------------------------

fn render(key: &ThumbKey) -> Thumb {
    #[cfg(test)]
    if let Some(thumb) = tests::fake_render(key) {
        return thumb;
    }
    let (w, h, rgba) = shell_image(&key.path, key.size, key.online_only)?;
    let png = encode_png(w, h, &rgba)?;
    Some(format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png)).into())
}

/// The item's shell thumbnail fitted in size×size, as top-down RGBA. Small
/// sizes fall back to the item's icon. With `cached_only` only what Windows
/// already has: an online-only cloud file is not read (which would download
/// it) to make one, nor to find its icon (programs and shortcuts keep theirs
/// inside). Needs COM on this thread.
fn shell_image(path: &str, size: u32, cached_only: bool) -> Option<(u32, u32, Vec<u8>)> {
    let wpath = crate::win::wide(path);
    let factory: IShellItemImageFactory = unsafe { SHCreateItemFromParsingName(PCWSTR(wpath.as_ptr()), None) }.ok()?;
    let want = SIZE { cx: size as i32, cy: size as i32 };
    let only = if cached_only { SIIGBF_INCACHEONLY } else { SIIGBF::default() };
    let hbm = match unsafe { factory.GetImage(want, SIIGBF_THUMBNAILONLY | SIIGBF_RESIZETOFIT | only) } {
        Ok(hbm) => hbm,
        Err(_) if size <= ICON_FALLBACK_MAX => {
            unsafe { factory.GetImage(want, SIIGBF_ICONONLY | SIIGBF_RESIZETOFIT | only) }.ok()?
        }
        Err(_) => return None,
    };
    if hbm.is_invalid() {
        return None;
    }
    let bitmap = Bitmap(hbm);
    bitmap.rgba()
}

/// Deleted when dropped.
struct Bitmap(HBITMAP);

impl Drop for Bitmap {
    fn drop(&mut self) {
        let _ = unsafe { DeleteObject(self.0.into()) };
    }
}

impl Bitmap {
    /// The pixels as 32-bit top-down RGBA, straight (not premultiplied) alpha.
    fn rgba(&self) -> Option<(u32, u32, Vec<u8>)> {
        let mut bm = BITMAP::default();
        let got = unsafe { GetObjectW(self.0.into(), size_of::<BITMAP>() as i32, Some((&raw mut bm).cast())) };
        if got == 0 {
            return None;
        }
        let (w, h) = (bm.bmWidth, bm.bmHeight.abs());
        if !(1..=4096).contains(&w) || !(1..=4096).contains(&h) {
            return None;
        }
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                // Negative: rows top to bottom.
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut px = vec![0u8; w as usize * h as usize * 4];
        let dc = unsafe { GetDC(None) };
        if dc.is_invalid() {
            return None;
        }
        let lines =
            unsafe { GetDIBits(dc, self.0, 0, h as u32, Some(px.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS) };
        unsafe { ReleaseDC(None, dc) };
        if lines != h {
            return None;
        }
        bgra_to_rgba(&mut px);
        Some((w as u32, h as u32, px))
    }
}

/// GDI's BGRA to RGBA. Shell bitmaps with alpha are premultiplied, so the
/// color is divided back out; a bitmap whose alpha is all 0 has none (opaque).
/// If some color is brighter than its alpha the bitmap was not premultiplied
/// after all and is left as it is.
fn bgra_to_rgba(px: &mut [u8]) {
    for p in px.chunks_exact_mut(4) {
        p.swap(0, 2);
    }
    if px.chunks_exact(4).all(|p| p[3] == 0) {
        for p in px.chunks_exact_mut(4) {
            p[3] = 255;
        }
        return;
    }
    if !px.chunks_exact(4).all(|p| p[..3].iter().all(|&c| c <= p[3])) {
        return;
    }
    for p in px.chunks_exact_mut(4) {
        let a = p[3] as u32;
        if a != 0 && a != 255 {
            for c in &mut p[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}

fn encode_png(w: u32, h: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fast);
    let mut writer = enc.write_header().ok()?;
    writer.write_image_data(rgba).ok()?;
    writer.finish().ok()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ff-preview-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn decodes_utf8_with_and_without_bom() {
        let d = decode("안녕 hello\n".as_bytes(), false);
        assert_eq!((d.text.as_str(), d.encoding, d.clean), ("안녕 hello\n", "UTF-8", true));
        let d = decode(b"\xEF\xBB\xBFabc", false);
        assert_eq!((d.text.as_str(), d.encoding), ("abc", "UTF-8"));
        // "한" is 3 bytes; the limit cut it after 2. Dropped, not an error.
        let cut = &"a한".as_bytes()[..3];
        let d = decode(cut, true);
        assert_eq!((d.text.as_str(), d.encoding, d.clean), ("a", "UTF-8", true));
    }

    #[test]
    fn decodes_utf16_by_bom() {
        let text = "가나다 abc 😀";
        let mut le = vec![0xFF, 0xFE];
        let mut be = vec![0xFE, 0xFF];
        for u in text.encode_utf16() {
            le.extend(u.to_le_bytes());
            be.extend(u.to_be_bytes());
        }
        let d = decode(&le, false);
        assert_eq!((d.text.as_str(), d.encoding, d.clean), (text, "UTF-16LE", true));
        let d = decode(&be, false);
        assert_eq!((d.text.as_str(), d.encoding, d.clean), (text, "UTF-16BE", true));
        // Cut inside the emoji's surrogate pair, and again mid code unit.
        let d = decode(&le[..le.len() - 2], true);
        assert_eq!((d.text.as_str(), d.clean), ("가나다 abc ", true));
        let d = decode(&be[..be.len() - 3], true);
        assert_eq!((d.text.as_str(), d.clean), ("가나다 abc ", true));
        // UTF-16 text is full of NUL bytes but still sniffs as text.
        assert!(looks_like_text(&le, false));
    }

    #[test]
    fn decodes_euc_kr() {
        // "한글 텍스트" in EUC-KR / CP949.
        let bytes = [0xC7, 0xD1, 0xB1, 0xDB, 0x20, 0xC5, 0xD8, 0xBD, 0xBA, 0xC6, 0xAE];
        let d = decode(&bytes, false);
        assert_eq!((d.text.as_str(), d.encoding, d.clean), ("한글 텍스트", "EUC-KR", true));
        // Cut after a lead byte.
        let d = decode(&bytes[..3], true);
        assert_eq!((d.text.as_str(), d.encoding), ("한", "EUC-KR"));
        assert!(looks_like_text(&bytes, false));
        // Neither UTF-8 nor EUC-KR: shown with replacement characters.
        let d = decode(&[b'a', 0xFF, 0xFE, 0xFF], false);
        assert_eq!((d.encoding, d.clean), ("UTF-8", false));
    }

    #[test]
    fn sniffs_text_and_binary() {
        assert!(looks_like_text(b"plain text\r\n\twith tabs", false));
        assert!(looks_like_text(b"", false));
        assert!(!looks_like_text(b"MZ\x90\x00\x03\x00", false));
        assert!(!looks_like_text(b"abc\x01\x02def", false));
        // A PNG's signature alone passes as CP949 text; its first chunk does not.
        assert!(!looks_like_text(b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR", false));
    }

    #[test]
    fn kinds_from_extension_and_content() {
        let dir = temp_dir("kinds");
        let file = |name: &str, bytes: &[u8]| {
            let p = dir.join(name);
            fs::write(&p, bytes).unwrap();
            p
        };
        let info = inspect(&file("a.PNG", b"not really a png")).unwrap();
        assert_eq!((info.kind.as_str(), info.mime.as_deref(), info.size), ("image", Some("image/png"), 16));
        assert!(info.text.is_none());
        for (name, kind, mime) in [
            ("b.mkv", "video", "video/x-matroska"),
            ("c.m4a", "audio", "audio/mp4"),
            ("e.svg", "image", "image/svg+xml"),
        ] {
            let info = inspect(&file(name, b"x")).unwrap();
            assert_eq!((info.kind.as_str(), info.mime.as_deref()), (kind, Some(mime)), "{name}");
            assert!(!info.online_only);
        }
        // A PDF only when it starts as one: the frame would run a web page saved as .pdf.
        let info = inspect(&file("d.pdf", b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n")).unwrap();
        assert_eq!((info.kind.as_str(), info.mime.as_deref()), ("pdf", Some("application/pdf")));
        let page = b"<html><body><script>fetch('https://example.com')</script></body></html>";
        let info = inspect(&file("invoice.pdf", page)).unwrap();
        assert_eq!((info.kind.as_str(), info.mime.as_deref()), ("text", None));
        assert_eq!(info.text.as_deref().map(str::as_bytes), Some(&page[..]));
        let info = inspect(&file("x.pdf", b"\x00\x01%PDF-1.7")).unwrap();
        assert_eq!((info.kind.as_str(), info.mime.as_deref()), ("none", None));
        assert_eq!(inspect(&file("short.pdf", b"%PD")).unwrap().kind, "text");
        let info = inspect(&file("main.rs", b"fn main() {}\n")).unwrap();
        assert_eq!((info.kind.as_str(), info.text.as_deref(), info.encoding.as_deref()), ("text", Some("fn main() {}\n"), Some("UTF-8")));
        assert!(info.mime.is_none() && !info.truncated);
        // Unknown extensions: by content.
        let info = inspect(&file("README", "설명서".as_bytes())).unwrap();
        assert_eq!((info.kind.as_str(), info.text.as_deref()), ("text", Some("설명서")));
        let info = inspect(&file("data.bin", b"\x00\x01\x02\x03binary")).unwrap();
        assert_eq!((info.kind.as_str(), info.size), ("none", 10));
        assert!(info.text.is_none());
        // A known text type is shown even with odd bytes in it.
        let info = inspect(&file("odd.txt", b"a\x00b")).unwrap();
        assert_eq!(info.kind, "text");
        // Long text is cut at the limit.
        let big = "가".repeat(TEXT_LIMIT / 3 + 10);
        let info = inspect(&file("big.log", big.as_bytes())).unwrap();
        assert!(info.truncated);
        let text = info.text.unwrap();
        assert!(text.len() <= TEXT_LIMIT && text.len() > TEXT_LIMIT - 3 && text.chars().all(|c| c == '가'));
        assert_eq!(info.size, big.len() as u64);
        // Folders have nothing to show.
        let sub = dir.join("sub.txt");
        fs::create_dir_all(&sub).unwrap();
        let info = inspect(&sub).unwrap();
        assert_eq!((info.kind.as_str(), info.mime.as_deref()), ("none", None));
        assert!(inspect(&dir.join("missing.txt")).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_name_ending_in_a_dot_is_previewed_as_itself() {
        let dir = temp_dir("dots");
        fs::write(win::verbatim(&dir.join("abc")), "neighbour").unwrap();
        fs::write(win::verbatim(&dir.join("abc.")), "itself").unwrap();
        let info = inspect(&dir.join("abc.")).unwrap();
        assert_eq!((info.kind.as_str(), info.text.as_deref(), info.size), ("text", Some("itself"), 6));
        // Under a folder named `x.` the asset protocol would load `x\a.png`: no media.
        fs::create_dir(dir.join("x")).unwrap();
        fs::write(dir.join("x").join("a.png"), "neighbour").unwrap();
        fs::write(dir.join("x").join("b.pdf"), "%PDF-1.7 neighbour").unwrap();
        fs::create_dir(win::verbatim(&dir.join("x."))).unwrap();
        fs::write(win::verbatim(&dir.join("x.").join("a.png")), b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR").unwrap();
        fs::write(win::verbatim(&dir.join("x.").join("b.pdf")), "not a pdf").unwrap();
        let info = inspect(&dir.join("x.").join("a.png")).unwrap();
        assert_eq!((info.kind.as_str(), info.mime.as_deref(), info.size), ("none", None, 16));
        let info = inspect(&dir.join("x.").join("b.pdf")).unwrap();
        assert_eq!((info.kind.as_str(), info.text.as_deref()), ("text", Some("not a pdf")));
        let _ = fs::remove_dir_all(win::verbatim(&dir));
    }

    #[test]
    fn idle_threads_handle_their_messages() {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::GetCurrentThreadId;
        use windows_sys::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, PM_NOREMOVE, WM_USER};
        let sem = unsafe { CreateSemaphoreW(null(), 0, 10, null()) };
        assert!(!sem.is_null());
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        // Gives this thread a message queue, then something in it.
        unsafe { PeekMessageW(&mut msg, null_mut(), 0, 0, PM_NOREMOVE) };
        assert_ne!(unsafe { PostThreadMessageW(GetCurrentThreadId(), WM_USER, 0, 0) }, 0);
        let t = Instant::now();
        wait_pumping(sem, Duration::from_millis(200));
        assert!(t.elapsed() >= Duration::from_millis(150), "{:?}", t.elapsed());
        // Taken off the queue while waiting.
        assert_eq!(unsafe { PeekMessageW(&mut msg, null_mut(), 0, 0, PM_NOREMOVE) }, 0);
        // A release ends the wait at once.
        unsafe { ReleaseSemaphore(sem, 1, null_mut()) };
        let t = Instant::now();
        wait_pumping(sem, Duration::from_secs(5));
        assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
        unsafe { CloseHandle(sem) };
    }

    #[test]
    fn online_only_files_are_told_apart() {
        assert!(!is_online_only(0x20));
        assert!(is_online_only(FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS | 0x20));
        assert!(is_online_only(FILE_ATTRIBUTE_OFFLINE));
        assert!(is_online_only(FILE_ATTRIBUTE_RECALL_ON_OPEN | 0x10));
    }

    #[test]
    fn lru_evicts_least_recently_used() {
        let mut lru = Lru::new(100);
        lru.insert("a", 1, 40);
        lru.insert("b", 2, 40);
        assert_eq!(lru.get(&"a"), Some(1));
        // "b" is now the oldest and makes room for "c".
        lru.insert("c", 3, 40);
        assert_eq!((lru.get(&"a"), lru.get(&"b"), lru.get(&"c")), (Some(1), None, Some(3)));
        assert_eq!(lru.used, 80);
        // Replacing an entry replaces its weight.
        lru.insert("a", 4, 10);
        assert_eq!((lru.get(&"a"), lru.used), (Some(4), 50));
        // Too heavy for the whole cache: not kept, nothing else lost.
        lru.insert("huge", 5, 101);
        assert_eq!((lru.get(&"huge"), lru.used, lru.map.len()), (None, 50, 2));
        // Heavy enough to push out everything else.
        lru.insert("d", 6, 100);
        assert_eq!((lru.get(&"a"), lru.get(&"c"), lru.get(&"d")), (None, None, Some(6)));
        assert_eq!((lru.used, lru.map.len(), lru.order.len()), (100, 1, 1));
    }

    #[test]
    fn unpremultiplies_alpha() {
        // BGRA: half-transparent red (premultiplied 128,0,0 → 255,0,0) and opaque blue.
        let mut px = vec![0, 0, 128, 128, 255, 0, 0, 255];
        bgra_to_rgba(&mut px);
        assert_eq!(px, [255, 0, 0, 128, 0, 0, 255, 255]);
        // No alpha at all: opaque.
        let mut px = vec![10, 20, 30, 0];
        bgra_to_rgba(&mut px);
        assert_eq!(px, [30, 20, 10, 255]);
        // Color brighter than alpha: straight alpha, kept as it is.
        let mut px = vec![200, 200, 200, 100, 0, 0, 0, 0];
        bgra_to_rgba(&mut px);
        assert_eq!(px, [200, 200, 200, 100, 0, 0, 0, 0]);
    }

    /// Stand-in providers: "ff-test:quick" answers at once, "ff-test:hang"
    /// only once the watchdog has given up on it.
    pub(super) fn fake_render(key: &ThumbKey) -> Option<Thumb> {
        match key.path.as_str() {
            "ff-test:quick" => Some(Some("quick".into())),
            "ff-test:hang" => {
                while cache().lock().unwrap().get(key) != Some(Cached::Late) {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Some(Some("late".into()))
            }
            _ => None,
        }
    }

    #[test]
    fn hung_provider_is_given_up_and_replaced() {
        let key = |path: &str| ThumbKey { path: path.into(), size: 64, modified: 0, len: 0, online_only: false };
        let made = |s: &str| Some(Cached::Made(Some(s.into())));
        let started = Instant::now();
        let mut hang = submit(key("ff-test:hang")).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        // The other threads keep working meanwhile.
        let mut quick = submit(key("ff-test:quick")).unwrap();
        assert_eq!(quick.blocking_recv(), made("quick"));
        assert!(started.elapsed() < RUN_LIMIT);
        // The caller hears "later" after RUN_LIMIT, not when the provider is done.
        assert_eq!(hang.blocking_recv(), Some(Cached::Late));
        let waited = started.elapsed();
        assert!(waited >= RUN_LIMIT && waited < RUN_LIMIT + Duration::from_secs(3), "{waited:?}");
        // The late result still lands in the cache, and the extra thread leaves.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let late = cache().lock().unwrap().get(&key("ff-test:hang"));
            let (threads, stuck) = {
                let st = pool().state.lock().unwrap();
                (st.threads, st.stuck)
            };
            if late == made("late") && (threads, stuck) == (WORKERS, 0) {
                break;
            }
            assert!(Instant::now() < deadline, "threads {threads}, stuck {stuck}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// (width, height, RGBA pixels) of a thumbnail data URL.
    fn png_pixels(data_url: &str) -> (u32, u32, Vec<u8>) {
        let b64 = data_url.strip_prefix("data:image/png;base64,").expect("PNG data URL");
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
        let mut reader = png::Decoder::new(&bytes[..]).read_info().unwrap();
        let mut px = vec![0; reader.output_buffer_size()];
        let frame = reader.next_frame(&mut px).unwrap();
        assert_eq!((frame.color_type, frame.bit_depth), (png::ColorType::Rgba, png::BitDepth::Eight));
        px.truncate(frame.buffer_size());
        (frame.width, frame.height, px)
    }

    fn png_size(data_url: &str) -> (u32, u32) {
        let (w, h, _) = png_pixels(data_url);
        (w, h)
    }

    fn write_bmp(path: &Path, w: u32, h: u32) {
        let row = (w * 3).div_ceil(4) * 4;
        let data = row * h;
        let mut b = Vec::new();
        b.extend(b"BM");
        b.extend((54 + data).to_le_bytes());
        b.extend([0u8; 4]);
        b.extend(54u32.to_le_bytes());
        b.extend(40u32.to_le_bytes());
        b.extend((w as i32).to_le_bytes());
        b.extend((h as i32).to_le_bytes());
        b.extend(1u16.to_le_bytes());
        b.extend(24u16.to_le_bytes());
        b.extend([0u8; 24]);
        for y in 0..h {
            for x in 0..w {
                b.extend([(x * 255 / w) as u8, (y * 255 / h) as u8, 200]);
            }
            b.resize(b.len() + (row - w * 3) as usize, 0);
        }
        fs::write(path, b).unwrap();
    }

    #[test]
    fn shell_thumbnails_of_generated_images() {
        let dir = temp_dir("thumbs");
        let png_path = dir.join("pic.png");
        let mut rgba = Vec::new();
        for y in 0..60u32 {
            for x in 0..120u32 {
                rgba.extend([(x * 2) as u8, (y * 4) as u8, 90, 255]);
            }
        }
        fs::write(&png_path, encode_png(120, 60, &rgba).unwrap()).unwrap();
        let bmp_path = dir.join("pic.bmp");
        write_bmp(&bmp_path, 50, 100);
        let odd = dir.join("note.fftest-unknown");
        fs::write(&odd, b"no thumbnail provider for this").unwrap();
        let thumb = |p: &Path, size: u32| {
            tauri::async_runtime::block_on(thumbnail(p.to_string_lossy().into_owned(), size)).unwrap()
        };

        let Some(t) = thumb(&png_path, 64) else {
            eprintln!("shell gave no thumbnail here (no thumbnail cache / CI?); skipping");
            let _ = fs::remove_dir_all(&dir);
            return;
        };
        let (w, h, px) = png_pixels(&t);
        // 120×60 fitted in 64×64, aspect kept.
        assert!(w == 64 && (31..=33).contains(&h), "{w}x{h}");
        // Opaque, and in RGBA order: blue is 90 everywhere, red grows left to right.
        assert!(px.chunks_exact(4).all(|p| p[3] == 255 && p[2].abs_diff(90) <= 4), "{:?}", &px[..8]);
        let row = &px[(h as usize / 2) * w as usize * 4..][..w as usize * 4];
        assert!(row[0] < 40 && row[row.len() - 4] > 200, "{} .. {}", row[0], row[row.len() - 4]);
        // Same file, same size: from the cache.
        let started = Instant::now();
        assert_eq!(thumb(&png_path, 64).as_deref(), Some(t.as_str()));
        assert!(started.elapsed() < Duration::from_secs(1));
        // Sizes are clamped to 16..=512.
        if let Some(t) = thumb(&png_path, 1) {
            let (w, h) = png_size(&t);
            assert!(w <= THUMB_MIN && h <= THUMB_MIN, "{w}x{h}");
        }
        if let Some(t) = thumb(&bmp_path, 48) {
            let (w, h) = png_size(&t);
            assert!(w <= 48 && h <= 48 && w > 0 && h > 0, "{w}x{h}");
        } else {
            eprintln!("no BMP thumbnail");
        }
        // No thumbnail: the icon for list rows, nothing for big views.
        assert!(thumb(&odd, 256).is_none());
        if let Some(t) = thumb(&odd, 32) {
            let (w, h) = png_size(&t);
            assert!(w <= 32 && h <= 32, "{w}x{h}");
        }
        assert!(thumb(&dir.join("missing.png"), 64).is_none());
        assert!(thumb(Path::new("relative.png"), 64).is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}
