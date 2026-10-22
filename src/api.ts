import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export interface Entry {
  name: string;
  path: string;
  is_dir: boolean;
  size: number;
  alloc: number;
  /** FILETIME: 100 ns ticks since 1601-01-01 UTC */
  modified: number;
  files: number;
  hidden: boolean;
  system: boolean;
  reparse: boolean;
}

export interface DriveInfo {
  letter: string;
  label: string;
  fs: string;
  kind: string;
  total: number;
  free: number;
  indexed: boolean;
  method: string | null;
  scanned_at: number | null;
  scan_ms: number | null;
  entries: number | null;
  /**
   * A SUBST drive (a folder of another drive under a letter of its own): `kind` is that of the
   * disk it points into. Left out by backends that do not tell.
   */
  subst?: boolean;
}

export interface Listing {
  path: string;
  indexed: boolean;
  current: Entry | null;
  entries: Entry[];
}

export interface ScanResult {
  method: string;
  entries: number;
  scan_ms: number;
  note: string | null;
}

export interface SearchResult {
  total: number;
  items: Entry[];
  elapsed_ms: number;
  searched_drives: string[];
}

export interface TrashResult {
  moved: string[];
  failed: [string, string][];
}

/** How a drive's index keeps up with changes on disk. "syncing": a quick sync (v0.7) runs. */
export interface LiveStatus {
  drive: string;
  state: "watching" | "stale" | "syncing" | "off" | "error";
  detail: string | null;
}

/** "index-changed": a batch of changes is in the index. */
export interface IndexChanged {
  drive: string;
  /** Folders whose direct children changed. Empty with `full` when there were too many to list. */
  dirs: string[];
  full: boolean;
  /**
   * Folders removed or renamed away in this batch (their old paths). Optional: a backend that
   * leaves it out only says, through `dirs`, that a folder above changed.
   */
  removed?: string[];
}

/** "index-stale": live updates were lost; the index works but may be out of date until rescanned. */
export interface IndexStale {
  drive: string;
  reason: string;
}

export const api = {
  appInfo: () => invoke<{ version: string; elevated: boolean }>("app_info"),
  drives: () => invoke<DriveInfo[]>("get_drives"),
  loadCached: () => invoke<string[]>("load_cached"),
  /**
   * A full scan. "auto" and "mft": the MFT with administrator rights (no UAC prompt while an admin
   * session is on), else a folder walk; "mft" adds a note when the drive has no MFT. "walk": a
   * folder walk with normal rights.
   */
  scan: (drive: string, mode: "auto" | "mft" | "walk" = "auto") => invoke<ScanResult>("scan_drive", { drive, mode }),
  list: (path: string) => invoke<Listing>("list_dir", { path }),
  search: (query: string, under: string | null, limit = 2000) =>
    invoke<SearchResult>("search", { query, under, limit }),
  open: (path: string) => invoke<void>("open_path", { path }),
  reveal: (path: string) => invoke<void>("reveal_path", { path }),
  rename: (path: string, newName: string) => invoke<string>("rename_path", { path, newName }),
  trash: (paths: string[]) => invoke<TrashResult>("trash_paths", { paths }),
  liveStatus: () => invoke<LiveStatus[]>("live_status"),
};

export const events = {
  indexChanged: (f: (p: IndexChanged) => void): Promise<UnlistenFn> =>
    listen<IndexChanged>("index-changed", (e) => f(e.payload)),
  indexStale: (f: (p: IndexStale) => void): Promise<UnlistenFn> =>
    listen<IndexStale>("index-stale", (e) => f(e.payload)),
};

// ---- v0.3: file operations, clipboard, previews -------------------------------

export interface JobInfo {
  id: number;
  op: "copy" | "move" | "relocate" | "restore";
  state: "running" | "waiting" | "done" | "failed" | "cancelled";
  dest: string;
  total_bytes: number;
  done_bytes: number;
  total_items: number;
  done_items: number;
  current: string | null;
  speed_bps: number;
  errors: [string, string][];
  /**
   * How many items went wrong, when `errors` holds only some of them: the newest while the job
   * runs, the first thousand once it is over. Left out, `errors` is the whole list.
   */
  error_count?: number;
}

export interface Conflict {
  job: number;
  source: string;
  target: string;
  /** The source's kind (a folder link included); the target's is `target_is_dir`. */
  is_dir: boolean;
  /** The existing item's kind. Left out by backends that do not send it: the dialog looks it up. */
  target_is_dir?: boolean;
  /**
   * Either side is a junction or symbolic link. A link and a real file or folder are of different
   * kinds, and only items of the same kind are overwritten. Left out by older backends.
   */
  is_link?: boolean;
  target_is_link?: boolean;
  /** Files only; 0 for a folder. */
  source_size: number;
  target_size: number;
  source_modified: number;
  target_modified: number;
}

export type ConflictChoice = "skip" | "overwrite" | "rename" | "cancel";

export interface ClipboardFiles {
  paths: string[];
  cut: boolean;
}

export interface PreviewInfo {
  kind: "image" | "video" | "audio" | "pdf" | "text" | "none";
  text: string | null;
  truncated: boolean;
  encoding: string | null;
  size: number;
  mime: string | null;
  /**
   * An online-only cloud file: opening it would download all of it, so the page loads none of
   * it (the shell thumbnail stands in). Left out by backends that do not tell.
   */
  online_only?: boolean;
}

export const ops = {
  createFolder: (parent: string, name: string) => invoke<string>("create_folder", { parent, name }),
  startTransfer: (op: "copy" | "move", sources: string[], dest: string) =>
    invoke<number>("start_transfer", { op, sources, dest }),
  resolveConflict: (job: number, choice: ConflictChoice, applyAll: boolean) =>
    invoke<void>("resolve_conflict", { job, choice, applyAll }),
  cancelJob: (job: number) => invoke<void>("cancel_job", { job }),
  jobs: () => invoke<JobInfo[]>("jobs"),
  /**
   * Questions asked before this page loaded (a reload while a job waits). Not every backend has
   * it: callers take a rejection for "none".
   */
  pendingConflicts: () => invoke<Conflict[]>("pending_conflicts"),
  clipboardRead: () => invoke<ClipboardFiles>("clipboard_read"),
  clipboardWrite: (paths: string[], cut: boolean) => invoke<void>("clipboard_write", { paths, cut }),
  /** PNG data URL of the shell thumbnail, or null. */
  thumbnail: (path: string, size: number) => invoke<string | null>("thumbnail", { path, size }),
  previewInfo: (path: string) => invoke<PreviewInfo>("preview_info", { path }),
};

export const opEvents = {
  jobProgress: (f: (j: JobInfo) => void): Promise<UnlistenFn> => listen<JobInfo>("job-progress", (e) => f(e.payload)),
  jobConflict: (f: (c: Conflict) => void): Promise<UnlistenFn> => listen<Conflict>("job-conflict", (e) => f(e.payload)),
};

// ---- v0.4: size map, duplicates, relocation, admin session -----------------------

export type Category =
  | "system" | "program" | "dev" | "game" | "cache" | "media" | "document" | "archive" | "personal" | "other";

export interface MapNode {
  name: string;
  path: string;
  size: number;
  files: number;
  is_dir: boolean;
  category: Category;
  children: MapNode[];
  other_size: number;
  other_count: number;
}

export interface DupProgress {
  id: number;
  phase: "size" | "partial" | "full" | "done" | "cancelled" | "failed";
  done: number;
  total: number;
  groups: number;
  wasted: number;
  detail: string | null;
}

export interface DupFile {
  path: string;
  modified: number;
  /** classify.rs's category of the file, so automatic picks keep away from what programs need. */
  category: Category;
}

export interface DupGroup {
  size: number;
  hash: string;
  /** Newest first. */
  files: DupFile[];
}

export interface DupReport {
  id: number;
  state: DupProgress["phase"];
  groups: DupGroup[];
  wasted: number;
  scanned_files: number;
  /**
   * The last event's detail once the search is over (the files left out and why; why it failed),
   * so it stays with the results. Null while it runs.
   */
  detail: string | null;
}

export interface Relocation {
  id: string;
  original: string;
  target: string;
  size: number;
  created_at: number;
  state: "active" | "restored" | "broken";
}

export interface AdminStatus {
  active: boolean;
  expires_in_s: number;
  detail: string | null;
}

export const v04 = {
  treeMap: (path: string, depth = 4, minRatio = 0.005) => invoke<MapNode>("tree_map", { path, depth, minRatio }),
  findDuplicates: (roots: string[], minSize = 1 << 20) => invoke<number>("find_duplicates", { roots, minSize }),
  cancelDup: (id: number) => invoke<void>("cancel_dup", { id }),
  dupResults: (id: number) => invoke<DupReport>("dup_results", { id }),
  startRelocate: (source: string, destParent: string) => invoke<number>("start_relocate", { source, destParent }),
  relocations: () => invoke<Relocation[]>("relocations"),
  undoRelocate: (id: string) => invoke<number>("undo_relocate", { id }),
  adminStart: () => invoke<AdminStatus>("admin_session_start"),
  adminStatus: () => invoke<AdminStatus>("admin_session_status"),
  adminStop: () => invoke<void>("admin_session_stop"),
  adminList: (path: string) => invoke<Entry[]>("admin_list", { path }),
  adminRead: (path: string) => invoke<PreviewInfo>("admin_read", { path }),
};

export const v04Events = {
  dupProgress: (f: (p: DupProgress) => void): Promise<UnlistenFn> =>
    listen<DupProgress>("dup-progress", (e) => f(e.payload)),
};

// ---- v0.5: scan comparison, Explorer context menu -------------------------------

export interface DiffNode {
  name: string;
  path: string;
  is_dir: boolean;
  size: number;
  prev_size: number;
  files: number;
  prev_files: number;
  status: "grown" | "shrunk" | "new" | "removed" | "same";
  children: DiffNode[];
}

export interface DiffInfo {
  drive: string;
  has_prev: boolean;
  scanned_at: number | null;
  prev_scanned_at: number | null;
  /** How each scan was made ("mft", "walk"), when known. */
  method: string | null;
  prev_method: string | null;
}

/**
 * A warning when the two scans compared were made differently. The MFT counts what is in the
 * folders this account cannot open, a plain walk leaves those out, so such folders look as if
 * they grew (or shrank) by all they hold.
 */
export function methodNote(info: DiffInfo | null): string | null {
  if (!info?.method || !info.prev_method || info.method === info.prev_method) return null;
  const label = (m: string) => (m === "mft" ? "MFT 고속 스캔" : m === "walk" ? "일반 탐색" : m);
  const walkNow = info.method === "walk";
  return (
    `지난 스캔은 ${label(info.prev_method)}, 지금 스캔은 ${label(info.method)}이에요. ` +
    `일반 탐색은 열 수 없는(보호된) 폴더를 빼고 세서, 그런 폴더는 ${walkNow ? "줄거나 없어진" : "늘거나 새로 생긴"} 것처럼 보일 수 있어요`
  );
}

export const v05 = {
  scanDiff: (path: string, depth = 3) => invoke<DiffNode>("scan_diff", { path, depth }),
  diffInfo: (drive: string) => invoke<DiffInfo>("diff_info", { drive }),
  /** Native Explorer context menu at screen point (physical px). True when a command ran. */
  shellMenu: (paths: string[], x: number, y: number) => invoke<boolean>("shell_menu", { paths, x, y }),
  /** Ctrl and Shift as held at this moment (Windows' key state, not the page's last key event). */
  modifierKeys: () => invoke<{ ctrl: boolean; shift: boolean }>("modifier_keys"),
};

// ---- v0.6: AI (optional) and optimization plans ----------------------------------

export type AiProvider = "off" | "claude" | "openai" | "ollama" | "claude_cli" | "codex_cli";

export interface AiSettings {
  provider: AiProvider;
  claude_model: string;
  openai_model: string;
  ollama_url: string;
  ollama_model: string;
  codex_model: string;
  effort: "low" | "medium" | "high";
  jev_enabled: boolean;
  confirm_before_send: boolean;
}

export interface KeyStatus {
  claude: boolean;
  openai: boolean;
  jev: boolean;
}

export interface AiPayloadPreview {
  provider: string;
  text: string;
  bytes: number;
}

export interface Risk {
  path: string;
  level: "safe" | "caution" | "danger";
  confidence: number;
  regenerable: number | null;
}

export type PlanKind = "structure" | "rename" | "dedupe" | "shrink" | "relocate" | "cleanup";

export interface PlanStep {
  id: number;
  action: "mkdir" | "move" | "rename" | "trash" | "compress" | "relocate";
  from: string;
  to: string | null;
  reason: string;
  risk: "safe" | "caution" | "danger";
  size: number;
  selected: boolean;
  state: "pending" | "done" | "failed" | "skipped" | "undone";
  error: string | null;
}

export interface Plan {
  id: string;
  kind: PlanKind;
  title: string;
  summary: string;
  source: "rules" | "claude" | "openai" | "ollama" | "claude_cli" | "codex_cli" | "mcp";
  root: string;
  created_at: number;
  steps: PlanStep[];
  state: "draft" | "running" | "done" | "partly" | "undone" | "failed";
}

export interface PlanProgress {
  id: string;
  state: Plan["state"];
  done: number;
  total: number;
  current: string | null;
}

export const ai = {
  getSettings: () => invoke<AiSettings>("ai_get_settings"),
  setSettings: (settings: AiSettings) => invoke<void>("ai_set_settings", { settings }),
  keyStatus: () => invoke<KeyStatus>("ai_key_status"),
  /** provider: "claude" | "openai" | "jev"; null removes the key. */
  setKey: (provider: "claude" | "openai" | "jev", key: string | null) => invoke<void>("ai_set_key", { provider, key }),
  test: (provider: string) => invoke<string>("ai_test", { provider }),
  preview: (task: "explain" | "structure" | "rename", path: string) =>
    invoke<AiPayloadPreview>("ai_preview", { task, path }),
  explain: (path: string) => invoke<string>("ai_explain", { path }),
  plan: (kind: "structure" | "rename", path: string) => invoke<Plan>("ai_plan", { kind, path }),
  jevRisk: (paths: string[]) => invoke<Risk[]>("jev_risk", { paths }),
};

export const plans = {
  list: () => invoke<Plan[]>("plans_list"),
  get: (id: string) => invoke<Plan>("plan_get", { id }),
  rules: (kind: "cleanup" | "dedupe" | "shrink" | "relocate", path: string) =>
    invoke<Plan>("plan_rules", { kind, path }),
  update: (plan: Plan) => invoke<Plan>("plan_update", { plan }),
  execute: (id: string) => invoke<void>("plan_execute", { id }),
  undo: (id: string) => invoke<void>("plan_undo", { id }),
  remove: (id: string) => invoke<void>("plan_delete", { id }),
};

export const planEvents = {
  progress: (f: (p: PlanProgress) => void): Promise<UnlistenFn> =>
    listen<PlanProgress>("plan-progress", (e) => f(e.payload)),
};

// ---- v0.7: quick sync, installed programs -----------------------------------------

/** "sync-progress": a quick re-sync (no administrator rights) of a drive. */
export interface SyncProgress {
  drive: string;
  state: "running" | "done" | "failed";
  /** Korean, user-facing. */
  detail: string | null;
  /** Why it ran: "startup" | "stale" | "user". */
  reason: string;
}

export interface Program {
  id: string;
  name: string;
  publisher: string | null;
  version: string | null;
  installed_at: number | null;
  install_location: string | null;
  size: number | null;
  size_source: "index" | "registry" | "unknown";
  last_used: number | null;
  last_used_source: "userassist" | "prefetch" | "exe" | null;
  run_count: number | null;
  kind: "win32" | "store";
  can_uninstall: boolean;
  machine_wide: boolean;
}

export interface Leftover {
  path: string;
  size: number;
  modified: number;
  reason: string;
  risk: "caution" | "danger";
}

export const v07 = {
  syncDrive: (drive: string) => invoke<ScanResult>("sync_drive", { drive }),
  programs: (withPrefetch = false) => invoke<Program[]>("programs_list", { withPrefetch }),
  uninstall: (id: string) => invoke<void>("program_uninstall", { id }),
  leftovers: () => invoke<Leftover[]>("program_leftovers"),
  leftoversPlan: (paths: string[]) => invoke<Plan>("program_leftovers_plan", { paths }),
};

export const v07Events = {
  syncProgress: (f: (p: SyncProgress) => void): Promise<UnlistenFn> =>
    listen<SyncProgress>("sync-progress", (e) => f(e.payload)),
};

// ---- v0.8: in-app updates (Updater.tsx; the updater plugin does the checking and downloading) ----

/** What the update installer would close by force besides this window (lib.rs update_others). */
export interface UpdateOthers {
  /** Other processes of this program file: other windows, an MCP server a Claude app started. */
  processes: number;
  /** One of them runs or undoes a plan now. */
  plan: boolean;
  /** One of them moves a folder to another drive, or back, now. */
  relocation: boolean;
}

export const v08 = {
  updateOthers: () => invoke<UpdateOthers>("update_others"),
  /**
   * Readies the app for the installer: stops this window's work the user agreed to stop (the jobs
   * `stopJobs`, the plan `stopPlan`; as closing the window does) and writes the caches. False, with
   * nothing stopped, when other work of this window is going (started during the download, say):
   * the user is asked again. Rejects (Korean) when work elsewhere would be cut short.
   */
  updatePrepare: (stopJobs: number[], stopPlan: string | null) =>
    invoke<boolean>("update_prepare", { stopJobs, stopPlan }),
  /** The installer did not start: the windows the updater hid come back. */
  updateInstallFailed: () => invoke<void>("update_install_failed"),
};

// ---- v0.8.1: the sidebar's 즐겨찾기 (Sidebar.tsx) ------------------------------------

/** A folder Windows knows by its role (SHGetKnownFolderPath), or a OneDrive account's folder. */
export type PlaceKind =
  | "home"
  | "desktop"
  | "downloads"
  | "documents"
  | "pictures"
  | "music"
  | "videos"
  | "onedrive"
  | "onedrive_business";

export interface Place {
  /** Stable, for PlacesConfig.hidden ("desktop", "onedrive_business"…). */
  id: string;
  /** The shell's display name ("바탕 화면", "OneDrive - Personal"). */
  label: string;
  path: string;
  kind: PlaceKind;
}

/** A folder the user pinned; `label` null shows the folder's own name. */
export interface PinnedPlace {
  path: string;
  label: string | null;
}

/** What the sidebar keeps (places.json in the app's data folder). */
export interface PlacesConfig {
  /** In the order shown, at most 100. */
  pinned: PinnedPlace[];
  /** Known places (Place.id) the user hid. */
  hidden: string[];
  /** Folded sidebar sections: "quick", "drives", "tools". */
  collapsed: string[];
  /** Explorer's pinned folders were offered (or brought in) once. */
  imported_explorer: boolean;
}

export const places = {
  /** The known folders and OneDrive folders that exist, with the shell's names. */
  known: () => invoke<Place[]>("known_places"),
  /** Folders pinned in Explorer's Quick Access (홈 → 즐겨찾기), that exist. Rejects (Korean). */
  explorerPins: () => invoke<string[]>("explorer_pins"),
  get: () => invoke<PlacesConfig>("places_get"),
  set: (config: PlacesConfig) => invoke<void>("places_set", { config }),
  /** What a path is, from the item itself (no folder listing). Links count as what they are. */
  pathKind: (path: string) => invoke<"dir" | "file" | "gone">("path_kind", { path }),
};
