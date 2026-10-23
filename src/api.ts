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
  /** "link": duplicates made into hard links (v082.startDupLink). */
  op: "copy" | "move" | "relocate" | "restore" | "link";
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
  /**
   * "link": bytes of the merged copies now in the Recycle Bin, which come free once it is emptied
   * (`done_bytes` is what was read to compare). 0 for the other jobs; left out by older backends.
   */
  freed_bytes?: number;
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

/**
 * v0.10: what judges for the 위험도 badges, AI 검색 and the plan's second check: "jev" (TypeSafe
 * jev, my key, paid) or "ollama" (a local model, free, this PC).
 */
export type JudgeEngine = "jev" | "ollama";

export interface AiSettings {
  provider: AiProvider;
  claude_model: string;
  openai_model: string;
  ollama_url: string;
  ollama_model: string;
  codex_model: string;
  effort: "low" | "medium" | "high";
  /** The judge features' master switch ("AI 판정 쓰기"): 위험도 badges, AI 검색, the plan's second check. */
  jev_enabled: boolean;
  /** v0.10: the engine of those features. Old settings files have none: "jev". */
  judge: JudgeEngine;
  /** v0.10: the Ollama model that judges; empty, `ollama_model` does. The address is `ollama_url`. */
  judge_model: string;
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
  /**
   * How sure the judge is, 0..1 (jev: 0 when it does not say). v0.10: null for a local model,
   * which never says.
   */
  confidence: number | null;
  regenerable: number | null;
}

/** A Risk's confidence, or null when the judge gave none (a local model). */
export function riskConfidence(r: Risk): number | null {
  return r.confidence ?? null;
}

/** The judge's name as the app says it: "jev" or "로컬 모델". */
export const JUDGE_NAME: Record<JudgeEngine, string> = { jev: "jev", ollama: "로컬 모델" };

/** The engine the settings name (an old backend sends none: jev). */
export const judgeOf = (s: AiSettings): JudgeEngine => (s.judge === "ollama" ? "ollama" : "jev");

/** The Ollama model that judges: its own, else the AI's Ollama model; "" when neither is set. */
export const judgeModel = (s: AiSettings): string => (s.judge_model ?? "").trim() || (s.ollama_model ?? "").trim();

/**
 * An address on this computer (Ollama's): what goes there stays on this PC. Only "localhost",
 * 127.x.x.x and [::1]: another name (even "x.localhost") is looked up in DNS by the backend, which
 * may send it elsewhere, so it counts as another computer.
 */
export function isLocalUrl(url: string): boolean {
  try {
    // The URL parser writes IPv4 in its dotted form ("127.1" → "127.0.0.1") and IPv6 shortest, in brackets.
    const h = new URL(url.trim()).hostname.toLowerCase();
    return h === "localhost" || h === "[::1]" || /^127\.\d{1,3}\.\d{1,3}\.\d{1,3}$/.test(h);
  } catch {
    return false;
  }
}

/**
 * Where the judge sends what it is asked about, when that leaves this PC: "jev", or the Ollama
 * address of another computer. Null for an Ollama on this PC (nothing to ask about first).
 */
export function judgeTarget(s: AiSettings): string | null {
  if (judgeOf(s) === "jev") return "jev";
  const url = (s.ollama_url ?? "").trim();
  // Empty: the backend's own (this PC's) address.
  return !url || isLocalUrl(url) ? null : `ollama:${url}`;
}

/** The Ollama address the judge sends to when it is another computer's; null for jev and for this PC. */
export function judgeRemote(s: AiSettings): string | null {
  return judgeOf(s) === "ollama" && judgeTarget(s) !== null ? (s.ollama_url ?? "").trim() : null;
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
  /** 위험도 from the judge the settings name (jev, or a local model since v0.10). The name stays. */
  jevRisk: (paths: string[]) => invoke<Risk[]>("jev_risk", { paths }),
  /**
   * v0.9 AI 검색 ("뜻으로 찾기" before v0.10): the judge (jev, or a local model since v0.10)
   * reads what the query asks for (installed games·programs by genre or kind, or files by a
   * description, kind and period), the index gathers candidates, and the judge rates each by its
   * path, name, kind, size and date (never its contents). `under`: only below this folder. It runs
   * only when asked (Enter), never as one types (jev bills each request, a local model takes its
   * time); the same search again comes from the backend's cache (`requests` 0), each engine's
   * answers apart. Progress comes as "ai-find-progress" (aiEvents.findProgress); findCancel stops
   * it early, and it then resolves with what was answered (`cancelled`). Rejects (Korean) while the
   * judge is off or not ready (no jev key, Ollama not running, no model), for an empty or too long
   * query, or before any drive is scanned.
   */
  jevFind: (query: string, under: string | null) => invoke<JevFound>("jev_find", { query, under }),
  /**
   * v0.10: the AI 검색 that runs stops before its next request (the one out finishes); it then
   * resolves with what was answered so far, `cancelled` true. Nothing runs: nothing happens.
   */
  findCancel: () => invoke<void>("ai_find_cancel"),
  /**
   * v0.10: the models the Ollama at `url` (the settings' address when null) has, as `ollama list`
   * names them. Rejects (Korean) when it does not answer.
   */
  ollamaModels: (url: string | null) => invoke<string[]>("ai_ollama_models", { url }),
  /** v0.10: tries the judge as saved (jev: its key; a local model: Ollama has the model). A Korean status line. */
  judgeTest: () => invoke<string>("ai_judge_test"),
};

/** v0.10: "ai-find-progress": the AI 검색 that runs has judged `done` of its `total` batches. */
export interface FindProgress {
  done: number;
  total: number;
}

export const aiEvents = {
  findProgress: (f: (p: FindProgress) => void): Promise<UnlistenFn> =>
    listen<FindProgress>("ai-find-progress", (e) => f(e.payload)),
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

/**
 * v0.10: an installed program whose own folder is a given one (program_at, JevHit.program): what
 * "제거 (제어판처럼)…" shows and the id it hands to v07.uninstall.
 */
export interface ProgramRef {
  id: string;
  name: string;
  publisher: string | null;
  size: number | null;
  can_uninstall: boolean;
  machine_wide: boolean;
  kind: "win32" | "store";
}

export const v07 = {
  syncDrive: (drive: string) => invoke<ScanResult>("sync_drive", { drive }),
  programs: (withPrefetch = false) => invoke<Program[]>("programs_list", { withPrefetch }),
  /** Starts the program's own uninstaller (a Store app: removed for this account). It never deletes folders itself. */
  uninstall: (id: string) => invoke<void>("program_uninstall", { id }),
  /**
   * v0.10: the installed program whose own folder (or InstallLocation) is exactly `path` (case
   * aside, no trailing "\"); null when none is, or for Windows' own components.
   */
  programAt: (path: string) => invoke<ProgramRef | null>("program_at", { path }),
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

// ---- v0.8.2: duplicates made into hard links (Duplicates.tsx "하드 링크로 합치기") ----------

/** Copies one start_dup_link takes at most (ops.rs MAX_LINKS). */
export const LINK_MAX = 10_000;

/** One duplicate group's part: the file that stays, and the copies that become hard links to it. */
export interface DupLinkGroup {
  keep: string;
  others: string[];
}

export const v082 = {
  /**
   * Replaces each of `others` by an NTFS hard link to its group's `keep`, so every path still
   * opens the same content stored once. A job of ops.rs (op "link"): its progress comes as
   * "job-progress" and the job list shows it; `done_bytes` counts what was read to compare,
   * `freed_bytes` what the Recycle Bin got. Each pair is checked right before (both there, plain
   * files on the same NTFS volume outside Windows' folders, not one file already, unchanged since
   * the search, the whole content compared again); the old copy goes to the Recycle Bin.
   * `dup`: the search the groups come from (DupReport.id); null, the newest that has them. Every
   * path must be in that search's result, each group's in one of its groups, at most
   * LINK_MAX copies: else it rejects (Korean) and nothing starts. Resolves with the job id.
   */
  startDupLink: (groups: DupLinkGroup[], dup: number | null) => invoke<number>("start_dup_link", { groups, dup }),
};

// ---- v0.9: AI 검색 — search by meaning (Explorer.tsx's search box, ai.jevFind) ----------
// (v0.9 "뜻으로 찾기" by jev; v0.10 by jev or a local model, as the AI settings say.)

/** The backend's name for an Entry (index.rs EntryInfo): what search returns. */
export type EntryInfo = Entry;

/** A file kind jev read from the query (meaning.rs); null in JevFound: any kind. */
export type JevKind = "document" | "image" | "video" | "audio" | "archive" | "program";

/** A period jev read from the query, in local calendar terms (a week starts on Monday). */
export type JevWhen =
  | "today"
  | "yesterday"
  | "this_week"
  | "last_week"
  | "this_month"
  | "last_month"
  | "this_year"
  | "last_year"
  | "older";

/**
 * v0.10: what an AI 검색 shows at first: the hits the judge scored this high or more (the rest wait
 * behind "40% 미만도 보기"), and the 일치도 badge's "mid" from here. Not 0.5 (v0.9): jev's scores
 * move a little from request to request and with the order of the items, and true matches came as
 * low as 0.49 while wrong ones stayed at 0.4 or under. A local model answers 0.9, 0.6 or 0.1 only,
 * which this does not change.
 */
export const JEV_SHOW = 0.4;

/** One candidate the judge rated. */
export interface JevHit {
  /** The file or folder (for a game or program, its install folder). */
  entry: EntryInfo;
  /** How well it answers the query, 0..1, as the judge sees it. The list hides those under JEV_SHOW at first. */
  score: number;
  /** A program's display name, or Steam's name for a game (null: none known, the folder's name stands). */
  label: string | null;
  /**
   * v0.10: the installed program this folder is (its uninstaller is what "제거 (제어판처럼)…"
   * runs); null for files, and for game folders Windows has no program for.
   */
  program: ProgramRef | null;
}

export interface JevFound {
  /** v0.10: who judged: "jev" or "ollama" (a local model). */
  engine: JudgeEngine;
  /** v0.10: stopped early (findCancel): `items` hold what was answered before it. */
  cancelled: boolean;
  /** "app": installed games and programs (a genre or kind: "RPG 게임"); "file": files and folders (a description). */
  target: "app" | "file";
  /** Files only; null: any kind. */
  kind: JevKind | null;
  /** Files only; null: any time. */
  when: JevWhen | null;
  /** The words file candidates were gathered by (empty for "app"). */
  terms: string[];
  /** Every candidate judged, the best first. */
  items: JevHit[];
  /** Candidates gathered before the cap. */
  candidates: number;
  /**
   * v0.10: the cap, the most candidates judged for this engine and target (meaning.rs): jev 240
   * files, 400 games·programs; a local model, asked one candidate at a time, 100 and 150.
   */
  cap: number;
  /** Candidates judged: at most the cap, fewer when a request went unanswered. */
  asked: number;
  /** Programs whose folder is not in the index (Store apps and the like): left out. */
  unplaced: number;
  /** Requests sent to the judge for this search; 0 when every answer came from the cache. */
  requests: number;
  elapsed_ms: number;
}
