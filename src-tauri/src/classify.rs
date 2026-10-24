//! What a folder or file is for, by rules on its path and name (no AI): the
//! colours of the size map and, later, hints for AI features.
//!
//! The rules go from the most to the least telling, and the first that holds
//! wins:
//!
//! 1. system: the Windows folder, `$Recycle.Bin`, `System Volume Information`,
//!    `Recovery` and the like at a drive's root, and pagefile/hiberfil/swapfile.
//! 2. dev: tool folders (`node_modules`, `.git`, `.venv`, `.cargo`, ...),
//!    build output (`target`, `build`, `dist`, `bin`, `obj`, `out`) inside a
//!    code folder (`GitHub`, `repos`, `src`, `projects`, `프로젝트`, ...), and
//!    project files (`.sln`, `.csproj`, `.pdb`, ...).
//! 3. cache: `Temp`, any folder with "cache" in its name, `CrashDumps`,
//!    `huggingface`, and temporary files (`.tmp`, `.crdownload`, ...).
//! 4. game: game stores and launchers, any folder with "games" in its name.
//! 5. program: `Program Files*`, `ProgramData\Microsoft`, `AppData\Local\Programs`.
//! 6. A file's extension: media, document, archive (installers only in Downloads).
//! 7. dev: anything else in a code folder (`GitHub`, `repos`, ...).
//! 8. The nearest well-known folder: Pictures/Videos/Music → media,
//!    Documents/Desktop → document (Korean names too: 사진, 문서, 바탕 화면, ...).
//! 9. personal: anything else under `Users`, or in a cloud drive folder.
//! 10. other.
//!
//! Rules 1 to 5 are about the folders an item is in (a folder counts itself),
//! so a picture inside a game or a cache is the game's or the cache's. Below
//! them a file goes by its extension first, so a PDF on the Desktop is a
//! document and a video there is media.

/// One of "system" | "program" | "dev" | "game" | "cache" | "media" |
/// "document" | "archive" | "personal" | "other".
///
/// `path` is the item's full path (`C:\Users\me\Pictures`); `name` is its last
/// part. Upper and lower case do not matter.
pub fn category(path: &str, name: &str, is_dir: bool) -> &'static str {
    let all = parts(path, name);
    let name = all.last().map(String::as_str).unwrap_or_default();
    // The folders the item is in, and the item itself when it is a folder.
    let dirs = if is_dir { &all[..] } else { &all[..all.len().saturating_sub(1)] };
    let ext = if is_dir { "" } else { extension(name) };

    if let Some(c) = system_or_temp(&all, dirs, name, is_dir) {
        return c;
    }
    if is_dev(dirs, ext) {
        return "dev";
    }
    if is_cache(dirs, ext) {
        return "cache";
    }
    if dirs.iter().any(|d| is_game_dir(d)) {
        return "game";
    }
    if is_program(dirs) {
        return "program";
    }
    if let Some(c) = by_extension(dirs, ext) {
        return c;
    }
    if dirs.iter().any(|d| DEV_ROOTS.contains(&d.as_str())) {
        return "dev";
    }
    for d in dirs.iter().rev() {
        if MEDIA_DIRS.contains(&d.as_str()) {
            return "media";
        }
        if DOCUMENT_DIRS.contains(&d.as_str()) {
            return "document";
        }
    }
    if all.first().is_some_and(|d| d == "users") || dirs.iter().any(|d| is_cloud_dir(d)) {
        return "personal";
    }
    "other"
}

/// What a program needs where it is, as far as the folders an item is in
/// tell (and the dev file types): "system", "dev", "game" or "program" by
/// rules 1, 2, 4 and 5 of `category`, without the cache rule in front of
/// them (a `cache` folder of a game or a program is still the game's or the
/// program's, and the Windows temp folder is Windows's), and app data
/// (`AppData`, `ProgramData`) counted as a program's. None otherwise. The
/// duplicate finder never picks such a file by itself.
pub fn program_category(path: &str, name: &str, is_dir: bool) -> Option<&'static str> {
    let all = parts(path, name);
    let name = all.last().map(String::as_str).unwrap_or_default();
    let dirs = if is_dir { &all[..] } else { &all[..all.len().saturating_sub(1)] };
    let ext = if is_dir { "" } else { extension(name) };
    if system_or_temp(&all, dirs, name, is_dir).is_some() {
        return Some("system");
    }
    if is_dev(dirs, ext) {
        return Some("dev");
    }
    if dirs.iter().any(|d| is_game_dir(d)) {
        return Some("game");
    }
    let app_data = dirs.first().is_some_and(|d| d == "programdata") || dirs.iter().any(|d| d == "appdata");
    (is_program(dirs) || app_data).then_some("program")
}

/// The parts of `path` below its drive (`C:`) or share (`\\server\share`), in
/// lower case, ending with the item's own name. `\\?\` paths work too.
fn parts(path: &str, name: &str) -> Vec<String> {
    let (rest, unc) = match path.strip_prefix(r"\\?\").or_else(|| path.strip_prefix(r"\\.\")) {
        Some(r) => match r.get(..4) {
            Some(head) if head.eq_ignore_ascii_case(r"UNC\") => (&r[4..], true),
            _ => (r, false),
        },
        None => match path.strip_prefix(r"\\") {
            Some(r) => (r, true),
            None => (path, false),
        },
    };
    let mut it = rest.split(['\\', '/']).filter(|p| !p.is_empty()).peekable();
    if unc {
        it.next();
        it.next();
    } else if it.peek().is_some_and(|p| p.ends_with(':')) {
        it.next();
    }
    let mut all: Vec<String> = it.map(str::to_lowercase).collect();
    // A caller may pass the folder an item is in, with the item's name apart.
    let name = name.to_lowercase();
    if !name.is_empty() && !name.ends_with(':') && all.last() != Some(&name) {
        all.push(name);
    }
    all
}

/// The part after the last dot, unless the dot starts the name (`.gitignore`).
fn extension(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext,
        _ => "",
    }
}

/// Whether `dirs` has `seq` as consecutive parts.
fn has_run(dirs: &[String], seq: &[&str]) -> bool {
    dirs.windows(seq.len()).any(|w| w.iter().zip(seq).all(|(a, b)| a == b))
}

// ---- system -------------------------------------------------------------------

/// Folders at a drive's root that belong to Windows.
const SYSTEM_TOP: &[&str] = &[
    "windows",
    "windows.old",
    "$recycle.bin",
    "system volume information",
    "recovery",
    "boot",
    "config.msi",
    "msocache",
    "perflogs",
    "$windows.~bt",
    "$windows.~ws",
    "$winreagent",
    "$sysreset",
    "$getcurrent",
];

/// Files at a drive's root that belong to Windows (besides the page files).
const SYSTEM_ROOT_FILES: &[&str] = &["bootmgr", "bootnxt", "bootsect.bak", "dumpstack.log", "dumpstack.log.tmp"];

/// "system", or "cache" for the Windows temp folder, which is not worth keeping.
fn system_or_temp(all: &[String], dirs: &[String], name: &str, is_dir: bool) -> Option<&'static str> {
    if !is_dir {
        if matches!(name, "pagefile.sys" | "hiberfil.sys" | "swapfile.sys") {
            return Some("system");
        }
        if all.len() == 1 && SYSTEM_ROOT_FILES.contains(&name) {
            return Some("system");
        }
    }
    let top = dirs.first()?;
    if top == "windows" && dirs.get(1).is_some_and(|d| d == "temp") {
        return Some("cache");
    }
    SYSTEM_TOP.contains(&top.as_str()).then_some("system")
}

// ---- dev ----------------------------------------------------------------------

/// Folders that only development tools make.
const DEV_DIRS: &[&str] = &[
    "node_modules",
    "bower_components",
    ".git",
    ".svn",
    ".hg",
    ".venv",
    "venv",
    "__pycache__",
    "site-packages",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".conda",
    "anaconda3",
    "miniconda3",
    ".gradle",
    ".m2",
    ".ivy2",
    ".cargo",
    ".rustup",
    ".npm",
    ".pnpm-store",
    ".yarn",
    ".nuget",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".turbo",
    ".angular",
    ".parcel-cache",
    ".dart_tool",
    ".pub-cache",
    ".expo",
    ".terraform",
    ".android",
    ".vs",
    ".idea",
    ".vscode",
    "docker",
];

/// Build output folders: dev only inside a code folder (`DEV_CONTEXT`), since
/// `build` or `bin` alone can be anything.
const DEV_OUTPUT: &[&str] = &["target", "build", "dist", "out", "bin", "obj"];

/// Folders where code is kept; everything in them is dev (rule 7).
const DEV_ROOTS: &[&str] = &["github", "gitlab", "bitbucket", "repos", "repositories", "androidstudioprojects"];

/// Folders that make the build output folders below them dev.
const DEV_CONTEXT: &[&str] = &[
    "source",
    "src",
    "src-tauri",
    "projects",
    "project",
    "workspace",
    "workspaces",
    "dev",
    "develop",
    "development",
    "code",
    "프로젝트",
    "개발",
    "소스",
    "코드",
];

/// Files that only development tools make: solutions, projects, symbols, objects.
fn is_dev_ext(ext: &str) -> bool {
    matches!(
        ext,
        "sln" | "csproj" | "vcxproj" | "vbproj" | "fsproj" | "pdb" | "ilk" | "pch" | "ipch" | "idb" | "rlib" | "rmeta"
            | "pyc" | "nupkg" | "whl" | "vsix"
    )
}

fn is_dev(dirs: &[String], ext: &str) -> bool {
    if dirs.iter().any(|d| DEV_DIRS.contains(&d.as_str())) || has_run(dirs, &["android", "sdk"]) || is_dev_ext(ext) {
        return true;
    }
    dirs.iter().enumerate().any(|(i, d)| {
        DEV_OUTPUT.contains(&d.as_str())
            && dirs[..i].iter().any(|a| DEV_ROOTS.contains(&a.as_str()) || DEV_CONTEXT.contains(&a.as_str()))
    })
}

// ---- cache --------------------------------------------------------------------

/// Temporary and half-downloaded files, crash dumps, and the app's own
/// working copies (ops.rs: a copy being written, a merge's and an unlink's
/// hidden `.name.ff-link` and `.name.ff-unlink`, which a crash can leave).
fn is_temp_ext(ext: &str) -> bool {
    matches!(
        ext,
        "tmp" | "temp" | "crdownload" | "part" | "partial" | "download" | "ff-partial" | "ff-link" | "ff-unlink" | "dmp" | "mdmp"
    )
}

fn is_cache(dirs: &[String], ext: &str) -> bool {
    is_temp_ext(ext) || dirs.iter().any(|d| is_cache_dir(d))
}

fn is_cache_dir(d: &str) -> bool {
    matches!(d, "temp" | "tmp" | "임시" | "temporary internet files" | "crashdumps" | "crashpad" | "huggingface")
        || d.contains("cache")
        || d.contains("캐시")
}

// ---- game ---------------------------------------------------------------------

/// Game stores, launchers and publishers' folders.
pub(crate) const GAME_DIRS: &[&str] = &[
    "steam",
    "steamlibrary",
    "steamapps",
    "epic games",
    "riot games",
    "nexon",
    "넥슨",
    "battle.net",
    "blizzard entertainment",
    "ubisoft",
    "ubisoft game launcher",
    "gog galaxy",
    ".minecraft",
    "ncsoft",
    "plaync",
    "smilegate",
    "netmarble",
    "pearl abyss",
    "krafton",
    "hoyoplay",
    // Minecraft launchers, with the mod packs, instances and backups they keep.
    "curseforge",
    "modrinth",
    "modrinthapp",
    "com.modrinth.theseus",
    "prismlauncher",
    "polymc",
    "multimc",
    "atlauncher",
    "gdlauncher",
    "gdlauncher_carbon",
    "ftba",
    ".technic",
    "tlauncher",
    ".tlauncher",
    "lunarclient",
    ".lunarclient",
    "minecraft launcher",
    "minecraftlauncher",
];

/// Whether a folder name (lower case) is a game store's, a launcher's or a
/// publisher's, or one that holds games (`Games`, `XboxGames`, `게임`).
pub(crate) fn is_game_dir(d: &str) -> bool {
    GAME_DIRS.contains(&d) || d.contains("games") || d.contains("게임")
}

// ---- program ------------------------------------------------------------------

fn is_program(dirs: &[String]) -> bool {
    let top = dirs.first().map(String::as_str).unwrap_or_default();
    top.starts_with("program files")
        || (top == "programdata" && dirs.get(1).is_some_and(|d| d == "microsoft"))
        || has_run(dirs, &["appdata", "local", "programs"])
        || has_run(dirs, &["appdata", "local", "microsoft", "windowsapps"])
}

// ---- by extension -------------------------------------------------------------

fn is_image_ext(ext: &str) -> bool {
    matches!(
        ext,
        "jpg" | "jpeg" | "jpe" | "png" | "gif" | "bmp" | "tif" | "tiff" | "webp" | "heic" | "heif" | "avif" | "jxl"
            | "psd" | "raw" | "dng" | "cr2" | "cr3" | "nef" | "arw" | "orf" | "rw2" | "raf" | "srw"
    )
}

fn is_video_ext(ext: &str) -> bool {
    matches!(
        ext,
        "mp4" | "m4v" | "mkv" | "avi" | "mov" | "wmv" | "flv" | "webm" | "mpg" | "mpeg" | "m2ts" | "mts" | "3gp" | "vob"
            | "ogv" | "rmvb" | "asf"
    )
}

fn is_audio_ext(ext: &str) -> bool {
    matches!(
        ext,
        "mp3" | "flac" | "wav" | "m4a" | "aac" | "ogg" | "oga" | "opus" | "wma" | "aiff" | "aif" | "ape" | "mid" | "midi"
            | "amr"
    )
}

/// Pictures, video and sound.
fn is_media_ext(ext: &str) -> bool {
    is_image_ext(ext) || is_video_ext(ext) || is_audio_ext(ext)
}

fn is_document_ext(ext: &str) -> bool {
    ["doc", "xls", "ppt", "hwp"].iter().any(|p| ext.starts_with(p))
        || matches!(
            ext,
            "pdf" | "txt" | "md" | "markdown" | "rtf" | "odt" | "ods" | "odp" | "epub" | "cell" | "show"
        )
}

fn is_archive_ext(ext: &str) -> bool {
    matches!(
        ext,
        "zip" | "7z" | "rar" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "cab" | "alz" | "egg" | "iso" | "img"
            | "dmg" | "wim"
    )
}

/// Setup files: archives when they sit in Downloads, programs anywhere else.
fn is_installer_ext(ext: &str) -> bool {
    matches!(ext, "exe" | "msi" | "msix" | "msixbundle" | "appx" | "appxbundle")
}

/// A `.ts` file this big is a video (an MPEG transport stream); a smaller
/// one is TypeScript code, wherever it is kept.
const TS_VIDEO: u64 = 1 << 20;

/// What kind of file a name (lower case) of `size` bytes is by its
/// extension, for 뜻으로 찾기 (meaning.rs): "document" | "image" | "video" |
/// "audio" | "archive" | "program" (setup files and programs). None for
/// anything else. Wider than the categories in places: a `.csv` is a
/// document, and a `.ts` of a megabyte or more a video.
pub(crate) fn file_kind(name: &str, size: u64) -> Option<&'static str> {
    let ext = extension(name);
    if ext.is_empty() {
        None
    } else if is_document_ext(ext) || ext == "csv" {
        Some("document")
    } else if is_image_ext(ext) {
        Some("image")
    } else if is_video_ext(ext) || (ext == "ts" && size >= TS_VIDEO) {
        Some("video")
    } else if is_audio_ext(ext) {
        Some("audio")
    } else if is_archive_ext(ext) {
        Some("archive")
    } else if is_installer_ext(ext) {
        Some("program")
    } else {
        None
    }
}

fn by_extension(dirs: &[String], ext: &str) -> Option<&'static str> {
    if ext.is_empty() {
        return None;
    }
    if is_media_ext(ext) {
        return Some("media");
    }
    if is_document_ext(ext) {
        return Some("document");
    }
    let in_downloads = || dirs.iter().any(|d| matches!(d.as_str(), "downloads" | "download" | "다운로드"));
    (is_archive_ext(ext) || (is_installer_ext(ext) && in_downloads())).then_some("archive")
}

// ---- user folders -------------------------------------------------------------

const MEDIA_DIRS: &[&str] = &[
    "pictures",
    "photos",
    "videos",
    "movies",
    "music",
    "camera roll",
    "saved pictures",
    "screenshots",
    "dcim",
    "사진",
    "그림",
    "동영상",
    "비디오",
    "영상",
    "음악",
];

const DOCUMENT_DIRS: &[&str] = &["documents", "my documents", "desktop", "문서", "내 문서", "바탕 화면", "바탕화면"];

fn is_cloud_dir(d: &str) -> bool {
    d.starts_with("onedrive") || matches!(d, "dropbox" | "google drive" | "icloud drive" | "icloudrive" | "내 드라이브")
}

// ---- hard links ---------------------------------------------------------------
//
// Which duplicates may be merged into one file under two names (links.rs has
// the rule; dups.rs shows it, ops.rs's merge keeps to it). A hard link is one
// file: a program that rewrites it in place, changes its attributes or
// permissions, or checks and repairs its own folder changes it under every
// name. So only the user's own pictures, videos, music, archives, setup files,
// PDFs and model files merge by themselves; a big model or archive in a
// program's folder only when the user asks; anything a game, launcher, mod
// pack, backup, code project, cache or Windows keeps, never.

/// Where a file is, for merging duplicates into hard links. The first that
/// holds wins.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LinkPlace {
    /// Windows's own folders (rule 1 of `category`).
    System,
    /// A game's, a launcher's, a mod pack's or a server pack's folder, an
    /// instance's or a save's; `backup` when in a backup folder of one.
    Game { backup: bool },
    /// Rules 2 and 7.
    Dev,
    /// Rule 3, the Windows temp folder too, but not a Store app's own app
    /// data (`Packages\<app>\LocalCache`).
    Cache,
    /// A backup folder anywhere (`Backup`, `백업`, `WindowsImageBackup`, File History).
    Backup,
    /// Program files and app data (`program_category`'s "program"); `below`
    /// is how many folders the file is in below `AppData\Local` (`Roaming`,
    /// `LocalLow`), `Program Files*` or `ProgramData`.
    Program { below: usize },
    /// The user's own folders: Downloads, Documents, Desktop, Pictures,
    /// Videos, Music and cloud drives, in a user's folder (`Users\<name>`) or
    /// at a drive's root; `below` is how many folders the file is in below that one.
    User { below: usize },
    Elsewhere,
}

/// What a file is, by its name, for merging duplicates into hard links.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LinkKind {
    /// Settings, saves, worlds, databases, logs, scripts, virtual disks:
    /// programs rewrite them in place.
    Rewritten,
    /// Documents, which get edited.
    Document,
    /// A model's weights (a `.bin` of 100 MB or more among them) or a zip,
    /// 7z or iso archive: what two programs may each keep a copy of.
    Bulky,
    /// Pictures, video, sound, archives and disk images, setup files, PDFs:
    /// written once and then only read.
    Settled,
    Other,
}

/// A `.bin` this big is a model's weights; a smaller one can be anything.
const MODEL_BIN: u64 = 100 << 20;

/// Folders of a mod pack, a server pack or a game's instance, whichever
/// launcher made them (or none: one unpacked in Downloads).
fn is_pack_dir(d: &str) -> bool {
    matches!(
        d,
        "mods" | "modpacks" | "resourcepacks" | "shaderpacks" | "texturepacks" | "datapacks" | "saves" | "kubejs"
            | "defaultconfigs" | "global_packs" | "crash-reports" | "minecraft" | "instances"
    ) || ["modpack", "mod pack", "mod-pack", "server-pack", "serverpack", "server pack", "server_pack", "server-files", "serverfiles", "server files"]
        .iter()
        .any(|p| d.contains(p))
}

fn is_backup_dir(d: &str) -> bool {
    d.contains("backup") || d.contains("백업") || matches!(d, "filehistory" | "file history")
}

/// The user's own folders, as `LinkPlace::User` has them.
fn is_user_dir(d: &str) -> bool {
    matches!(d, "downloads" | "download" | "다운로드")
        || MEDIA_DIRS.contains(&d)
        || DOCUMENT_DIRS.contains(&d)
        || is_cloud_dir(d)
}

/// Where the file `path` (`name` its last part) is, for merging it with a
/// copy into a hard link.
pub(crate) fn link_place(path: &str, name: &str) -> LinkPlace {
    let all = parts(path, name);
    let name = all.last().map(String::as_str).unwrap_or_default();
    let dirs = &all[..all.len().saturating_sub(1)];
    let ext = extension(name);
    match system_or_temp(&all, dirs, name, false) {
        Some("system") => return LinkPlace::System,
        Some(_) => return LinkPlace::Cache,
        None => {}
    }
    if let Some(at) = dirs.iter().position(|d| is_game_dir(d) || is_pack_dir(d)) {
        return LinkPlace::Game { backup: dirs[at..].iter().any(|d| is_backup_dir(d)) };
    }
    if is_dev(dirs, ext) || dirs.iter().any(|d| DEV_ROOTS.contains(&d.as_str())) {
        return LinkPlace::Dev;
    }
    // `Packages\<app>\LocalCache` is where a Store app keeps its app data.
    let cache = |(i, d): (usize, &String)| is_cache_dir(d) && !(d == "localcache" && i >= 2 && dirs[i - 2] == "packages");
    if is_temp_ext(ext) || dirs.iter().enumerate().any(cache) {
        return LinkPlace::Cache;
    }
    if dirs.iter().any(|d| is_backup_dir(d)) {
        return LinkPlace::Backup;
    }
    // Folders below `dirs[root]`.
    let below = |root: usize| dirs.len().saturating_sub(root + 1);
    let top = dirs.first().map(String::as_str).unwrap_or_default();
    if top.starts_with("program files") || top == "programdata" {
        return LinkPlace::Program { below: below(0) };
    }
    if let Some(i) = dirs.iter().position(|d| d == "appdata") {
        let area = dirs.get(i + 1).is_some_and(|d| matches!(d.as_str(), "local" | "roaming" | "locallow"));
        return LinkPlace::Program { below: below(if area { i + 1 } else { i }) };
    }
    let at_top = |i: usize| i == 0 || (i == 2 && top == "users");
    match dirs.iter().enumerate().find(|&(i, d)| at_top(i) && is_user_dir(d)) {
        Some((i, _)) => LinkPlace::User { below: below(i) },
        None => LinkPlace::Elsewhere,
    }
}

/// What a file named `name` (any case) of `size` bytes is, for merging it
/// with a copy into a hard link.
pub(crate) fn link_kind(name: &str, size: u64) -> LinkKind {
    let name = name.to_lowercase();
    let ext = extension(&name);
    let rewritten = matches!(
        ext,
        "json" | "json5" | "jsonc" | "toml" | "yaml" | "yml" | "ini" | "cfg" | "conf" | "config" | "txt" | "md" | "csv" | "xml"
            | "properties" | "prefs" | "plist" | "reg" | "dat" | "dat_old" | "db" | "db3" | "sqlite" | "sqlite3" | "db-wal"
            | "db-shm" | "db-journal" | "wal" | "ldb" | "mdb" | "accdb" | "log" | "nbt" | "snbt" | "mca" | "mcr" | "sav"
            | "save" | "bak" | "old" | "lock" | "bp" | "js" | "zs" | "lua" | "vdf" | "acf" | "pst" | "ost" | "vhd" | "vhdx"
            | "vmdk" | "vdi" | "qcow2"
    );
    let model = matches!(ext, "gguf" | "ggml" | "safetensors" | "onnx" | "pt" | "pth" | "ckpt") || (ext == "bin" && size >= MODEL_BIN);
    if rewritten {
        LinkKind::Rewritten
    } else if model || matches!(ext, "zip" | "7z" | "iso") {
        LinkKind::Bulky
    } else if is_media_ext(ext) || is_archive_ext(ext) || is_installer_ext(ext) || ext == "pdf" {
        LinkKind::Settled
    } else if is_document_ext(ext) {
        LinkKind::Document
    } else {
        LinkKind::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `category` with the name taken from the path.
    fn cat(path: &str, is_dir: bool) -> &'static str {
        let name = path.rsplit(['\\', '/']).next().unwrap_or_default();
        category(path, name, is_dir)
    }

    fn check(cases: &[(&str, bool)], want: &str) {
        for &(path, is_dir) in cases {
            assert_eq!(cat(path, is_dir), want, "{path} (dir: {is_dir})");
        }
    }

    const D: bool = true;
    const F: bool = false;

    #[test]
    fn system() {
        check(
            &[
                (r"C:\Windows", D),
                (r"C:\WINDOWS\System32", D),
                (r"C:\Windows\System32\kernel32.dll", F),
                // A picture inside Windows is still Windows's.
                (r"C:\Windows\Web\Wallpaper\Windows\img0.jpg", F),
                // "cache" in a name deep in Windows is not a cache.
                (r"C:\Windows\WinSxS\amd64_microsoft-windows-font-cache_31bf3856ad364e35", D),
                (r"C:\Windows.old", D),
                (r"C:\$Recycle.Bin", D),
                (r"C:\$RECYCLE.BIN\S-1-5-21-1000\$RABC123.mp4", F),
                (r"D:\$Recycle.Bin", D),
                (r"C:\System Volume Information", D),
                (r"C:\Recovery", D),
                (r"C:\pagefile.sys", F),
                (r"C:\hiberfil.sys", F),
                (r"C:\swapfile.sys", F),
                (r"D:\PAGEFILE.SYS", F),
                (r"C:\DumpStack.log.tmp", F),
                (r"\\?\C:\Windows\System32", D),
                ("C:/Windows/explorer.exe", F),
            ],
            "system",
        );
    }

    #[test]
    fn program() {
        check(
            &[
                (r"C:\Program Files", D),
                (r"C:\Program Files (x86)", D),
                (r"C:\Program Files\Mozilla Firefox\firefox.exe", F),
                (r"C:\Program Files (x86)\Microsoft\Edge\Application", D),
                (r"C:\Program Files\WindowsApps", D),
                // A program's own text and pictures are the program's.
                (r"D:\Program Files\Tool\readme.txt", F),
                (r"C:\Program Files\Tool\logo.png", F),
                (r"C:\ProgramData\Microsoft\Windows Defender", D),
                (r"C:\Users\me\AppData\Local\Programs\Microsoft VS Code", D),
                (r"C:\Users\me\AppData\Local\Programs\Python\Python312\python.exe", F),
                (r"C:\Users\me\AppData\Local\Microsoft\WindowsApps", D),
            ],
            "program",
        );
    }

    #[test]
    fn dev() {
        check(
            &[
                (r"D:\GitHub\fine-file\node_modules", D),
                (r"C:\Users\me\work\app\node_modules\react\README.md", F),
                (r"C:\Users\me\work\app\node_modules\.cache", D),
                (r"D:\GitHub\fine-file\.git", D),
                (r"C:\Users\me\work\app\.git\objects\pack\pack-1.pack", F),
                (r"D:\GitHub\fine-file\src-tauri\target", D),
                (r"D:\GitHub\fine-file\src-tauri\target\debug\fine-file.exe", F),
                (r"C:\Users\me\.cargo", D),
                (r"C:\Users\me\.cargo\registry\cache", D),
                (r"C:\Users\me\.rustup\toolchains", D),
                (r"C:\Users\me\.gradle\caches", D),
                (r"C:\Users\me\.m2\repository", D),
                (r"C:\Users\me\.nuget\packages", D),
                (r"C:\Users\me\.npm", D),
                (r"C:\Users\me\work\api\.venv", D),
                (r"D:\code\api\venv\Lib\site-packages", D),
                (r"D:\code\api\__pycache__", D),
                (r"D:\code\api\__pycache__\main.cpython-312.pyc", F),
                (r"C:\Users\me\source\repos\App\bin\Debug", D),
                (r"C:\Users\me\source\repos\App\obj", D),
                (r"C:\Users\me\source\repos\App\App.sln", F),
                (r"C:\Users\me\work\App\App.csproj", F),
                (r"C:\Users\me\work\App\.vs", D),
                (r"D:\GitHub\web\dist", D),
                (r"C:\Users\me\projects\site\build", D),
                (r"D:\dev\tool\out", D),
                (r"D:\프로젝트\앱\build", D),
                (r"D:\개발\서버\target\release", D),
                (r"C:\Users\me\AppData\Local\Android\Sdk", D),
                // Code folders and what is in them.
                (r"D:\GitHub", D),
                (r"D:\GitHub\fine-file", D),
                (r"D:\GitHub\fine-file\src\App.tsx", F),
                (r"C:\Users\me\source\repos", D),
                (r"C:\Users\me\Documents\GitHub\notes", D),
            ],
            "dev",
        );
    }

    #[test]
    fn build_output_needs_a_code_folder() {
        assert_eq!(cat(r"D:\target", D), "other");
        assert_eq!(cat(r"D:\Backup\build", D), "other");
        assert_eq!(cat(r"C:\Users\me\Downloads\build", D), "personal");
        assert_eq!(cat(r"C:\Users\me\Documents\dist", D), "document");
        assert_eq!(cat(r"D:\SteamLibrary\steamapps\common\Witcher\bin", D), "game");
    }

    #[test]
    fn cache() {
        check(
            &[
                (r"C:\Windows\Temp", D),
                (r"C:\Windows\Temp\setup.log", F),
                (r"C:\Users\me\AppData\Local\Temp", D),
                (r"C:\Users\me\AppData\Local\Temp\a1b2\photo.jpg", F),
                (r"C:\Users\me\AppData\Local\Microsoft\Windows\INetCache", D),
                (r"C:\Users\me\AppData\Local\Google\Chrome\User Data\Default\Cache\Cache_Data\f_000001", F),
                (r"C:\Users\me\AppData\Local\Google\Chrome\User Data\Default\Code Cache", D),
                (r"C:\Users\me\AppData\Roaming\discord\GPUCache", D),
                (r"C:\Users\me\AppData\Local\CrashDumps", D),
                (r"C:\Users\me\AppData\Local\npm-cache", D),
                (r"C:\Users\me\.cache", D),
                (r"C:\Users\me\.cache\huggingface\hub", D),
                (r"D:\models\huggingface", D),
                (r"C:\ProgramData\Package Cache", D),
                (r"C:\Users\me\Downloads\setup.tmp", F),
                (r"C:\Users\me\Downloads\movie.mp4.crdownload", F),
                (r"C:\Users\me\Downloads\.big.iso.ff-partial", F),
                (r"C:\Users\me\Downloads\.model.gguf.ff-link", F),
                (r"C:\Users\me\Videos\.clip.mkv.ff-unlink", F),
                // A game's shader cache can go; the game stays.
                (r"D:\SteamLibrary\steamapps\shadercache", D),
                (r"C:\Program Files (x86)\Steam\appcache", D),
                (r"D:\캐시", D),
                (r"D:\임시\자료", D),
                (r"D:\tmp", D),
            ],
            "cache",
        );
    }

    #[test]
    fn game() {
        check(
            &[
                (r"D:\SteamLibrary", D),
                (r"D:\SteamLibrary\steamapps\common\ELDEN RING", D),
                (r"C:\Program Files (x86)\Steam", D),
                (r"C:\Program Files (x86)\Steam\steamapps\common\Terraria\Terraria.exe", F),
                (r"C:\Program Files\Epic Games\Fortnite", D),
                (r"C:\Riot Games\League of Legends", D),
                (r"C:\Nexon\Maplestory", D),
                (r"C:\넥슨\메이플스토리", D),
                (r"C:\Program Files (x86)\Battle.net", D),
                (r"D:\Games\Witcher3\bin\x64\witcher3.exe", F),
                (r"C:\XboxGames", D),
                (r"C:\Users\me\Saved Games", D),
                (r"C:\Users\me\Documents\My Games\Skyrim\Saves", D),
                (r"C:\Users\me\AppData\Roaming\.minecraft", D),
                (r"D:\게임\스타크래프트", D),
                (r"D:\Program Files\Kakao Games\Odin", D),
                // A game's videos are the game's.
                (r"D:\SteamLibrary\steamapps\common\Game\movies\intro.mp4", F),
            ],
            "game",
        );
    }

    #[test]
    fn media() {
        check(
            &[
                (r"C:\Users\me\Pictures", D),
                (r"C:\Users\me\Pictures\2024\Jeju trip", D),
                (r"C:\Users\me\Pictures\2024\Jeju trip\notes.dat", F),
                (r"C:\Users\me\Videos", D),
                (r"C:\Users\me\Videos\clip.mkv", F),
                (r"C:\Users\me\Music", D),
                (r"D:\Backup\photo.HEIC", F),
                (r"D:\stuff\song.Mp3", F),
                // Extension first: a video on the Desktop is media.
                (r"C:\Users\me\Desktop\movie.MP4", F),
                (r"C:\Users\me\Documents\scan.jpg", F),
                (r"C:\Users\me\OneDrive\사진", D),
                (r"C:\Users\me\OneDrive\사진\가족", D),
                (r"D:\사진\제주도", D),
                (r"D:\동영상", D),
                (r"D:\영상\편집본", D),
                (r"D:\음악\a.flac", F),
                (r"D:\그림", D),
                // The nearest well-known folder decides.
                (r"C:\Users\me\Documents\Pictures", D),
                (r"\\nas\share\Movies\a.bin", F),
                ("C:/Users/me/Pictures", D),
            ],
            "media",
        );
    }

    #[test]
    fn document() {
        check(
            &[
                (r"C:\Users\me\Documents", D),
                (r"C:\Users\me\Desktop", D),
                (r"C:\Users\me\Documents\report.docx", F),
                (r"C:\Users\me\Documents\data.bin", F),
                (r"C:\Users\me\Documents\Pictures\list.pdf", F),
                // Extension first: a PDF among the pictures is a document.
                (r"C:\Users\me\Pictures\scan.pdf", F),
                (r"D:\work\budget.XLSX", F),
                (r"D:\work\old.xls", F),
                (r"D:\work\slides.pptx", F),
                (r"D:\work\공문.hwp", F),
                (r"D:\work\서식.HWPX", F),
                (r"D:\notes.txt", F),
                (r"D:\GitHub\fine-file\README.md", F),
                (r"D:\books\novel.epub", F),
                (r"C:\Users\me\OneDrive\문서", D),
                (r"C:\Users\me\OneDrive\문서\회의록", D),
                (r"C:\Users\me\바탕 화면", D),
                (r"D:\문서\계약서", D),
                (r"C:\Users\Public\Desktop", D),
                (r"C:\Users\me\Desktop\Pictures\list.txt", F),
            ],
            "document",
        );
    }

    #[test]
    fn archive() {
        check(
            &[
                (r"C:\Users\me\Downloads\setup.exe", F),
                (r"C:\Users\me\Downloads\tool.MSI", F),
                (r"C:\Users\me\Downloads\photos.zip", F),
                (r"C:\Users\me\Downloads\Win11_24H2_Korean_x64.iso", F),
                (r"C:\Users\me\Downloads\old\drivers.7z", F),
                (r"D:\다운로드\설치.exe", F),
                (r"D:\Download\app.msix", F),
                (r"D:\Backup\old.rar", F),
                (r"D:\Backup\site.tar.gz", F),
                (r"C:\Users\me\Documents\자료.alz", F),
            ],
            "archive",
        );
        // A setup file outside Downloads is not an archive.
        assert_eq!(cat(r"D:\Tools\app.exe", F), "other");
        assert_eq!(cat(r"C:\Users\me\tools\app.exe", F), "personal");
        // A folder named like an archive is still a folder.
        assert_eq!(cat(r"D:\Backup\old.zip", D), "other");
    }

    #[test]
    fn personal() {
        check(
            &[
                (r"C:\Users", D),
                (r"C:\Users\me", D),
                (r"C:\Users\홍길동", D),
                (r"C:\Users\me\AppData", D),
                (r"C:\Users\me\AppData\Roaming\Foo\settings.json", F),
                (r"C:\Users\me\Downloads", D),
                (r"C:\Users\me\random.dat", F),
                (r"C:\Users\me\.ssh", D),
                (r"D:\OneDrive", D),
                (r"D:\OneDrive - Contoso\Shared", D),
                (r"D:\Dropbox\stuff", D),
            ],
            "personal",
        );
    }

    #[test]
    fn other() {
        assert_eq!(category(r"C:\", "C:", true), "other");
        assert_eq!(category("D:", "D:", true), "other");
        check(
            &[
                (r"D:\Data", D),
                (r"D:\stuff\file.bin", F),
                (r"C:\ProgramData", D),
                (r"C:\ProgramData\Vendor\app.db", F),
                (r"E:\자료\보관", D),
                (r"D:\.gitignore", F),
                (r"\\nas\share\backup", D),
            ],
            "other",
        );
    }

    #[test]
    fn path_and_name_apart() {
        // The folder an item is in, with its name given separately.
        assert_eq!(category(r"C:\Users\me", "Pictures", true), "media");
        assert_eq!(category(r"D:\GitHub\app", "node_modules", true), "dev");
        assert_eq!(category(r"C:\", "Windows", true), "system");
        assert_eq!(category(r"C:\Users\me\Downloads", "a.zip", false), "archive");
    }

    #[test]
    fn what_programs_need_where_it_is() {
        let p = |path: &str| program_category(path, path.rsplit('\\').next().unwrap_or_default(), false);
        for (path, want) in [
            (r"C:\Windows\System32\kernel32.dll", Some("system")),
            // A cache to `category`, but Windows's, a game's or a program's here.
            (r"C:\Windows\Temp\setup.log", Some("system")),
            (r"D:\SteamLibrary\steamapps\common\Game\Cache\data.pak", Some("game")),
            (r"C:\Program Files\Tool\cache\index.bin", Some("program")),
            (r"D:\GitHub\app\node_modules\.cache\x.js", Some("dev")),
            (r"D:\GitHub\app\target\debug\app.pdb", Some("dev")),
            // App data, whatever `category` calls it.
            (r"C:\Users\me\AppData\Roaming\Vendor\settings.db", Some("program")),
            (r"C:\Users\me\AppData\Local\Vendor\Cache\blob", Some("program")),
            (r"C:\ProgramData\Vendor\app.db", Some("program")),
            // The user's own.
            (r"C:\Users\me\Pictures\a.jpg", None),
            (r"D:\자료\보관\report.pdf", None),
            (r"C:\Users\me\Downloads\cache\x.zip", None),
            (r"D:\GitHub\app\photo.jpg", None),
        ] {
            assert_eq!(p(path), want, "{path}");
        }
        // What is one by `category` is one here too.
        for path in [r"C:\Program Files\Mozilla Firefox\firefox.exe", r"D:\Games\x\y.dat", r"C:\$Recycle.Bin\S-1\$R1.mp4"] {
            let name = path.rsplit('\\').next().unwrap();
            assert_eq!(program_category(path, name, false), Some(category(path, name, false)), "{path}");
        }
    }

    #[test]
    fn file_kinds() {
        let kinds = [
            ("document", &["a.pdf", "b.hwp", "c.hwpx", "d.docx", "e.xlsx", "f.pptx", "g.txt", "h.md", "i.csv", "j.odt"][..]),
            ("image", &["a.jpg", "b.jpeg", "c.png", "d.heic", "e.psd", "f.cr2", "g.nef", "h.arw", "i.tiff"]),
            ("video", &["a.mp4", "b.mkv", "c.mov", "d.webm", "e.m4v", "f.ts"]),
            ("audio", &["a.mp3", "b.flac", "c.wav", "d.m4a", "e.wma"]),
            ("archive", &["a.zip", "b.7z", "c.rar", "d.tar", "e.gz", "f.iso", "g.alz", "h.egg"]),
            ("program", &["setup.exe", "tool.msi"]),
        ];
        for (kind, names) in kinds {
            for name in names {
                assert_eq!(file_kind(name, 50 << 20), Some(kind), "{name}");
            }
        }
        for name in ["readme", ".pdf", "a.dll", "a.sys", "a.json"] {
            assert_eq!(file_kind(name, 50 << 20), None, "{name}");
        }
        // A small `.ts` is TypeScript, wherever it is; a big one a recording.
        assert_eq!(file_kind("index.ts", 12_000), None);
        assert_eq!(file_kind("index.ts", TS_VIDEO - 1), None);
        assert_eq!(file_kind("녹화.ts", TS_VIDEO), Some("video"));
        assert_eq!(file_kind("a.pdf", 0), Some("document"));
    }

    #[test]
    fn minecraft_launchers_are_games() {
        check(
            &[
                (r"C:\Users\me\curseforge", D),
                (r"C:\Users\me\curseforge\minecraft\Instances\PS Homegrown - Sunlit Valley\mods\create.jar", F),
                (r"C:\Users\me\curseforge\minecraft\Backups\pack\config\a.toml", F),
                (r"C:\Users\me\AppData\Roaming\ModrinthApp\profiles\pack", D),
                (r"C:\Users\me\AppData\Roaming\PrismLauncher\instances\x\.minecraft\saves", D),
                (r"C:\Users\me\AppData\Roaming\.tlauncher", D),
                (r"C:\Program Files (x86)\Minecraft Launcher", D),
            ],
            "game",
        );
    }

    #[test]
    fn where_files_are_for_hard_links() {
        use LinkPlace::*;
        let place = |path: &str| link_place(path, path.rsplit('\\').next().unwrap_or_default());
        for (path, want) in [
            (r"C:\Windows\WinSxS\amd64_x\a.dll", System),
            (r"C:\Windows\Temp\a.zip", Cache),
            // The packs and backups of the bug that made the rule.
            (r"C:\Users\me\curseforge\minecraft\Instances\PS Homegrown - Sunlit Valley\config\portable_blueprints\a.bp", Game { backup: false }),
            (r"C:\Users\me\curseforge\minecraft\Instances\PS Homegrown - Sunlit Valley\saves\W\portable_blueprints\a.bp", Game { backup: false }),
            (r"C:\Users\me\curseforge\minecraft\Backups\Sunlit Valley\mods\jna-5.12.1.jar", Game { backup: true }),
            (r"C:\Users\me\Downloads\Abyssal-Ascent-server-pack\libraries\jna-5.12.1.jar", Game { backup: false }),
            (r"C:\Users\me\Downloads\Abyssal-Ascent-server-pack\server-icon.png", Game { backup: false }),
            (r"D:\minecraft\world.zip", Game { backup: false }),
            (r"D:\SteamLibrary\steamapps\common\Game\movie.mp4", Game { backup: false }),
            (r"D:\GitHub\app\assets\logo.png", Dev),
            (r"C:\Users\me\work\app\node_modules\x\a.png", Dev),
            (r"C:\Users\me\.cache\huggingface\hub\m\model.safetensors", Cache),
            (r"C:\Users\me\AppData\Local\Temp\x\a.zip", Cache),
            (r"D:\Backup\2024\photo.jpg", Backup),
            (r"C:\Users\me\Documents\백업\photo.jpg", Backup),
            (r"C:\Users\me\AppData\Local\VideoTranslator\models\Qwen3-8B-Q4_K_M.gguf", Program { below: 2 }),
            // A Store app's own app data is not a cache.
            (r"C:\Users\me\AppData\Local\Packages\OpenAI.Codex_2p2nqsd0c76g0\LocalCache\Local\VideoTranslator\models\Qwen3-8B-Q4_K_M.gguf", Program { below: 6 }),
            (r"C:\Users\me\AppData\Local\Programs\REA\ghidra_11.2\Ghidra\lib\jna-5.12.1.jar", Program { below: 5 }),
            (r"C:\Program Files\App\data.zip", Program { below: 1 }),
            (r"C:\ProgramData\Vendor\a.iso", Program { below: 1 }),
            (r"C:\Users\me\Downloads\photo.jpg", User { below: 0 }),
            (r"C:\Users\me\Pictures\2024\trip\a.jpg", User { below: 2 }),
            (r"C:\Users\me\OneDrive\사진\a.jpg", User { below: 1 }),
            (r"C:\Users\Public\Desktop\a.pdf", User { below: 0 }),
            (r"D:\사진\제주\a.jpg", User { below: 1 }),
            (r"D:\Downloads\a.iso", User { below: 0 }),
            // Not the user's own folders: a well-known name deep in another folder counts for nothing.
            (r"D:\Anime\a.mkv", Elsewhere),
            (r"D:\Tools\x\music\a.mp3", Elsewhere),
            (r"C:\Users\me\a.mp4", Elsewhere),
        ] {
            assert_eq!(place(path), want, "{path}");
        }
    }

    #[test]
    fn what_files_are_for_hard_links() {
        use LinkKind::*;
        for (name, size, want) in [
            ("options.TXT", 10, Rewritten),
            ("a.json", 10, Rewritten),
            ("server.properties", 10, Rewritten),
            ("level.dat", 10, Rewritten),
            ("r.0.0.mca", 10, Rewritten),
            ("x.bp", 10, Rewritten),
            ("disk.vhdx", 10, Rewritten),
            ("Qwen3-8B.gguf", 10, Bulky),
            ("model.safetensors", 10, Bulky),
            ("pytorch_model.bin", MODEL_BIN, Bulky),
            ("pack.zip", 10, Bulky),
            ("win.iso", 10, Bulky),
            ("photo.jpg", 10, Settled),
            ("clip.mkv", 10, Settled),
            ("song.flac", 10, Settled),
            ("old.rar", 10, Settled),
            ("setup.exe", 10, Settled),
            ("manual.pdf", 10, Settled),
            ("report.docx", 10, Document),
            ("공문.hwp", 10, Document),
            ("small.bin", MODEL_BIN - 1, Other),
            ("jna-5.12.1.jar", 10, Other),
            ("a.dll", 10, Other),
            ("README", 10, Other),
        ] {
            assert_eq!(link_kind(name, size), want, "{name}");
        }
    }

    #[test]
    fn helpers() {
        assert_eq!(extension("a.tar.gz"), "gz");
        assert_eq!(extension(".gitignore"), "");
        assert_eq!(extension("Makefile"), "");
        assert_eq!(parts(r"\\?\UNC\server\share\Windows", "Windows"), vec!["windows"]);
        assert_eq!(parts(r"\\server\share\a\B", "B"), vec!["a", "b"]);
        assert_eq!(parts(r"C:\A\b\", "b"), vec!["a", "b"]);
        assert!(parts(r"C:\", "C:").is_empty());
    }
}
