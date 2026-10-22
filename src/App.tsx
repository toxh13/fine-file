import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import {
  ai,
  api,
  events,
  ops,
  opEvents,
  planEvents,
  plans,
  v07,
  v07Events,
  type ClipboardFiles,
  type DriveInfo,
  type JobInfo,
  type LiveStatus,
  type Plan,
  type SyncProgress,
} from "./api";
import {
  Explorer,
  TipTexts,
  VIEWS,
  changesOf,
  isLetter,
  tip,
  type AiState,
  type Changes,
  type ExplorerHandle,
  type View,
} from "./Explorer";
import { FirstRunGuide, HELP_TIPS, Help, firstRunDone } from "./Help";
import { dragOverWindow, keysNow, pagePoint, takeOwnDrop, trackModifiers } from "./shell";
import { FavoritesSection, SidebarSection, pinDropAt, sideNavKeys, usePlaces } from "./Sidebar";
import { Transfers } from "./Transfers";
import { UpdateBanner, UpdateSettingsDialog, useUpdateOffered } from "./Updater";
import { driveKey, folderTitle, formatSize, isSameOrInside, normalizePath, parentOf, pathKey, timeAgo } from "./format";
import "./App.css";

/** v0.2 had one view for the window: the first tab gets it when no tabs were saved. */
const VIEW_KEY = "fine-file.view";
const TABS_KEY = "fine-file.tabs";
/** The details pane (상세 정보; "미리 보기" up to v0.8): open, and its width. */
const PREVIEW_KEY = "fine-file.preview";

interface Tab {
  id: number;
  /** The folder shown, as the tab last reported it. */
  path: string;
  view: View;
}

const isView = (v: unknown): v is View => VIEWS.includes(v as View);

function savedView(): View {
  try {
    const v = localStorage.getItem(VIEW_KEY);
    return isView(v) ? v : "list";
  } catch {
    return "list";
  }
}

/** The tabs open when the app last closed, and which one was shown. */
function savedTabs(): { tabs: Omit<Tab, "id">[]; active: number } | null {
  try {
    const v = JSON.parse(localStorage.getItem(TABS_KEY) ?? "null");
    const tabs: Omit<Tab, "id">[] = [];
    for (const t of Array.isArray(v?.tabs) ? v.tabs : []) {
      const path = typeof t?.path === "string" ? normalizePath(t.path) : "";
      if (path) tabs.push({ path, view: isView(t.view) ? t.view : "list" });
    }
    if (!tabs.length) return null;
    const active = Number.isInteger(v.active) ? Math.min(Math.max(v.active, 0), tabs.length - 1) : 0;
    return { tabs, active };
  } catch {
    return null;
  }
}

function savedPreview(): { open: boolean; width: number } {
  try {
    const v = JSON.parse(localStorage.getItem(PREVIEW_KEY) ?? "null");
    return {
      open: v?.open === true,
      width: typeof v?.width === "number" && v.width >= 200 && v.width <= 2000 ? v.width : 320,
    };
  } catch {
    return { open: false, width: 320 };
  }
}

function store(key: string, value: string) {
  try {
    localStorage.setItem(key, value);
  } catch {
    /* storage blocked: the choice lasts until the app closes */
  }
}

const LIVE_LABEL: Record<LiveStatus["state"], string> = {
  watching: "실시간 반영 중",
  // Missed changes are synced again by themselves (or, after a removed hard link, need a full scan).
  stale: "놓친 변경 있음",
  syncing: "동기화 중",
  off: "실시간 반영 꺼짐",
  error: "실시간 반영 오류",
};

/** The sidebar's "<how> · <when>" under a drive, by the index's method ("sync": a quick sync of an MFT scan). */
const METHOD_SHORT: Record<string, string> = { mft: "MFT 스캔", sync: "동기화", walk: "일반 스캔" };
/** A scan's toast, by the method it ended up with. */
const METHOD_DONE: Record<string, string> = { mft: "MFT 고속 스캔", walk: "일반 탐색" };

/** Why a sync runs, beside "동기화 중" (a sync the user asked for needs no word). */
const SYNC_REASON: Record<string, string> = { startup: "켤 때 자동으로", stale: "놓친 변경을 자동으로" };

/**
 * watch.rs's stale reason after a file with several hard links was removed: a sync keeps the
 * counted links of what it does not see change, so only a full scan puts the sizes right (and no
 * sync runs by itself for it).
 */
const HARD_LINKS = /하드 링크/;
/** The sidebar's word for a drive in that state, instead of "놓친 변경 있음". */
const NEEDS_FULL_LABEL = "전체 스캔 필요";

/** A disk of this computer, which live updates watch and syncs may run for by themselves. */
const localDisk = (d: DriveInfo | undefined) => d?.kind === "fixed" || d?.kind === "removable";

/** Where the scan helper can read the MFT (lib.rs scan_drive): NTFS on a disk of this computer. */
const mftReadable = (d: DriveInfo) => localDisk(d) && d.fs.toUpperCase() === "NTFS";

/** Live-update statuses by drive ("C:"). */
type LiveMap = Record<string, LiveStatus>;

function without<T>(m: Record<string, T>, key: string): Record<string, T> {
  const n = { ...m };
  delete n[key];
  return n;
}

interface Toast {
  id: number;
  text: string;
  kind: "info" | "error";
}

/** A clipboard's file list in one comparable form. */
const clipKey = (paths: string[]) => paths.map(pathKey).sort().join("\n");

/** A paste, a drop (or a move to another drive) started here, until its job ends. */
interface Paste {
  sources: string[];
  /** Where it goes; "" for the job's own `dest` (a move to another drive). */
  dest: string;
  /** clipKey of the items being moved (a cut, or a drop that moves); null for a copy. */
  cut: string | null;
  /** The move pastes the clipboard's cut, which is emptied once it is done. */
  clip?: boolean;
  /** When the tabs were last told what came in (ms), for drives without live updates. */
  told: number;
}

/** The marked drop target while files are dragged over the window (see the drop below). */
const DROP_MARK = "drop-target";

/** On the page's root while files are dragged over the window: 즐겨찾기 makes room to pin them. */
const FILE_DRAG = "file-drag";

/** What a drop at a point does: into a folder, pinned in 즐겨찾기 at an index, refused, or nothing. */
type DropAt =
  | { dest: string; mark: Element }
  | { pin: number; mark: Element; cls: string }
  | { refuse: string }
  | null;

const isOver = (s: JobInfo["state"]) => s === "done" || s === "failed" || s === "cancelled";

/**
 * The folders a job changes: where it puts things, and for a move where they came from. A move to
 * another drive (and its undo) leaves a link, or the folder again, where the folder was.
 */
function jobDirs(op: JobInfo["op"], dest: string, sources: string[]): (string | null)[] {
  return op === "copy" ? [dest] : [dest, ...sources.map(parentOf)];
}

const movesDrives = (op: JobInfo["op"]) => op === "relocate" || op === "restore";

/** Plans proposed over MCP (by the Claude app, from another process) waiting to be looked at. */
const mcpDrafts = (list: Plan[]) => list.filter((p) => p.source === "mcp" && p.state === "draft").length;

/**
 * What a plan's run (or undo) changed, for the tabs: the folders its steps touched, and the items
 * gone from where they were (moved, renamed or sent to the Recycle Bin; moved back, for an undo).
 */
function planChanges(plan: Plan): Changes {
  const dirs: (string | null)[] = [];
  const removed: string[] = [];
  for (const s of plan.steps) {
    if (s.state !== "done" && s.state !== "undone") continue;
    dirs.push(parentOf(s.from));
    if (s.to) dirs.push(parentOf(s.to));
    const gone = s.state === "done" ? (["move", "rename", "trash"].includes(s.action) ? s.from : null) : s.action === "mkdir" ? s.from : s.to;
    if (gone && s.action !== "relocate" && s.action !== "compress") removed.push(gone);
  }
  return changesOf(dirs, removed);
}

export default function App() {
  const [drives, setDrives] = useState<DriveInfo[]>([]);
  const [elevated, setElevated] = useState(false);
  const [version, setVersion] = useState("");
  const [scanning, setScanning] = useState<string | null>(null);
  const [toasts, setToasts] = useState<Toast[]>([]);
  // Per drive ("C:"): how its index follows the disk, and why live updates stopped.
  const [live, setLive] = useState<LiveMap>({});
  const [stale, setStale] = useState<Record<string, string>>({});
  // v0.7: quick syncs running (as their last "sync-progress" said, whatever started them), and the
  // ones asked for here that have not answered yet. The ref is the same map, current at once.
  const [syncs, setSyncs] = useState<Record<string, SyncProgress>>({});
  const syncsRef = useRef<Record<string, SyncProgress>>({});
  const [asked, setAsked] = useState<Record<string, true>>({});
  const askedRef = useRef(new Set<string>());
  // When a scan, and a sync, of each drive last started (ms): a sync a scan replaced says nothing.
  const scanAt = useRef<Record<string, number>>({});
  const syncAt = useRef<Record<string, number>>({});
  const syncOpen = useRef<Record<string, boolean>>({});
  // Drives whose last "sync-progress" was "done" or "failed" (none "running" since). The sync's
  // last event comes a moment before it lets go of its turn (lib.rs), so a live_status read in
  // between can still say "syncing": that one is not believed, and is asked again.
  const syncEnded = useRef<Record<string, boolean>>({});
  // Drives where a file with several hard links was removed (HARD_LINKS), by the reason given.
  // A sync keeps the old count of what it does not see change, and its new watcher reports
  // "watching", so this lasts until a full scan of the drive ends, through any number of syncs.
  const [needsFull, setNeedsFull] = useState<Record<string, string>>({});
  const needsFullRef = useRef<Record<string, string>>({});
  const markNeedsFull = useCallback((letter: string, reason: string | null) => {
    const now = needsFullRef.current;
    if (reason === null ? !(letter in now) : now[letter] === reason) return;
    needsFullRef.current = reason === null ? without(now, letter) : { ...now, [letter]: reason };
    setNeedsFull(needsFullRef.current);
  }, []);
  // The sidebar's "▾" under a drive: other ways to scan it.
  const [driveMenu, setDriveMenu] = useState<{ letter: string; x: number; y: number } | null>(null);
  const driveMenuBox = useRef<HTMLDivElement>(null);
  // The help (F1, "?"), and the guide shown once on the first start.
  const [help, setHelp] = useState<{ topic?: string } | null>(null);
  const [guide, setGuide] = useState(() => !firstRunDone());
  const openHelp = useCallback((topic?: string) => setHelp({ topic }), []);
  // v0.8: the update settings (sidebar "업데이트"); the offer itself is UpdateBanner's.
  const [updateOpen, setUpdateOpen] = useState(false);
  // Only whether a new version is offered: the download's progress does not draw the window again.
  const updateOffered = useUpdateOffered();

  const [tabs, setTabs] = useState<Tab[]>([]);
  const [activeId, setActiveId] = useState(0);
  const nextTab = useRef(1);
  // Each tab's handle, for what every tab hears about (live changes, scans) and the sidebar.
  const handles = useRef(new Map<number, ExplorerHandle>());
  const tabBar = useRef<HTMLDivElement>(null);
  const [preview, setPreview] = useState(savedPreview);

  const toast = useCallback((text: string, kind: Toast["kind"] = "info") => {
    const id = Date.now() + Math.random();
    setToasts((t) => [...t, { id, text, kind }]);
    setTimeout(() => setToasts((t) => t.filter((x) => x.id !== id)), kind === "error" ? 8000 : 5000);
  }, []);

  // v0.8.1: the sidebar's 즐겨찾기 (known folders, OneDrive, pinned folders) and folded sections.
  const places = usePlaces(toast);
  const placesRef = useRef(places);
  useLayoutEffect(() => {
    placesRef.current = places;
  });
  const setPinned = useCallback((to: string, pin: boolean) => {
    if (pin) placesRef.current.pin([to]);
    else placesRef.current.unpin(to);
  }, []);

  const refreshDrives = useCallback(() => api.drives().then(setDrives), []);
  // A backend without live updates has no live_status: the sidebar then shows nothing for it.
  // Resolves to the statuses by drive ("C:"), or undefined when they could not be read.
  // live_status can wait up to 3 s for watchers still starting, while live updates ask every
  // half second, so one call runs at a time: asked meanwhile, one more call follows it, and
  // whoever asked in between gets that one's answer (it started after they asked).
  const liveReq = useRef(0);
  const appliedReq = useRef(0);
  const liveCall = useRef<{ running: Promise<LiveMap | undefined> | null; next: Promise<LiveMap | undefined> | null }>({
    running: null,
    next: null,
  });
  const refreshLive = useCallback((): Promise<LiveMap | undefined> => {
    const call = liveCall.current;
    const start = () => {
      const req = ++liveReq.current;
      const run: Promise<LiveMap | undefined> = api
        .liveStatus()
        .then((list) => {
          const m: LiveMap = Object.fromEntries(list.map((s) => [driveKey(s.drive), s]));
          // An older answer that lands late must not undo a newer one.
          if (req > appliedReq.current) {
            appliedReq.current = req;
            setLive(m);
          }
          return m;
        })
        .catch(() => undefined)
        .finally(() => {
          if (call.running === run) call.running = null;
        });
      call.running = run;
      return run;
    };
    if (call.next) return call.next;
    if (!call.running) return start();
    call.next = call.running.then(() => {
      call.next = null;
      return start();
    });
    return call.next;
  }, []);

  // Every tab hears about changes on disk; `except` is the tab that made them (it has reloaded).
  const broadcast = useCallback((c: Changes, except?: number) => {
    for (const [id, h] of handles.current) if (id !== except) h.apply(c);
  }, []);
  const notifyOthers = useCallback((from: number, c: Changes) => broadcast(c, from), [broadcast]);

  // ---- v0.6: AI (optional) and plans ----

  // The AI settings and which keys are saved, for every tab's buttons and menus. Null while
  // unknown: AI-only actions then say so, and everything else works as without AI.
  const [aiState, setAiState] = useState<AiState | null>(null);
  const aiReq = useRef(0);
  const refreshAi = useCallback(() => {
    const req = ++aiReq.current;
    Promise.all([ai.getSettings(), ai.keyStatus()]).then(
      ([settings, keys]) => req === aiReq.current && setAiState({ settings, keys }),
      () => {},
    );
  }, []);

  // Drafts proposed over MCP: the "계획" button's badge. They come from another process without an
  // event, so the list is read again when the window comes back to the front, and now and then.
  const [planDrafts, setPlanDrafts] = useState(0);
  const plansReq = useRef(0);
  const refreshPlans = useCallback(() => {
    const req = ++plansReq.current;
    plans.list().then(
      (list) => req === plansReq.current && setPlanDrafts(mcpDrafts(list)),
      () => {},
    );
  }, []);

  useEffect(() => {
    refreshAi();
    refreshPlans();
    const onFocus = () => {
      refreshAi();
      refreshPlans();
    };
    window.addEventListener("focus", onFocus);
    const t = setInterval(refreshPlans, 60000);
    return () => {
      window.removeEventListener("focus", onFocus);
      clearInterval(t);
    };
  }, [refreshAi, refreshPlans]);

  // A drive that was slow to answer (spinning up, a far-away network share) is asked again
  // a few times, so it fills in without a restart.
  const stuck = drives
    .filter((d) => d.kind === "unresponsive")
    .map((d) => d.letter)
    .join(",");
  const retries = useRef(0);
  const [retryTick, setRetryTick] = useState(0);
  // Each new set of slow drives gets its own five tries.
  useEffect(() => {
    retries.current = 0;
  }, [stuck]);
  useEffect(() => {
    if (!stuck || retries.current >= 5) return;
    const t = setTimeout(() => {
      retries.current++;
      // A failed query changes nothing, so tick to try again.
      refreshDrives().catch(() => setRetryTick((n) => n + 1));
    }, 5000);
    return () => clearTimeout(t);
  }, [stuck, drives, retryTick, refreshDrives]);

  // Startup: saved indexes first (instant sizes), then drives, then the tabs open last time (or the
  // system drive). The sidebar's 즐겨찾기 reads its places by itself (usePlaces).
  useEffect(() => {
    (async () => {
      const info = await api.appInfo();
      setElevated(info.elevated);
      setVersion(info.version);
      await api.loadCached().catch(() => []);
      const ds = await api.drives();
      setDrives(ds);
      refreshLive();
      const saved = savedTabs();
      const start = ds.find((d) => d.letter === "C:") ?? ds[0];
      const list = saved?.tabs ?? (start ? [{ path: `${start.letter}\\`, view: savedView() }] : []);
      const made = list.map((t) => ({ ...t, id: nextTab.current++ }));
      setTabs(made);
      if (made.length) setActiveId(made[saved?.active ?? 0].id);
    })().catch((e) => toast(String(e), "error"));
  }, [toast, refreshLive]);

  // The open tabs come back next time.
  useEffect(() => {
    if (!tabs.length) return;
    const active = Math.max(0, tabs.findIndex((t) => t.id === activeId));
    store(TABS_KEY, JSON.stringify({ tabs: tabs.map(({ path, view }) => ({ path, view })), active }));
  }, [tabs, activeId]);

  // Dragging the details pane's edge changes its width many times a second: saved once it rests.
  useEffect(() => {
    const t = setTimeout(() => store(PREVIEW_KEY, JSON.stringify(preview)), 300);
    return () => clearTimeout(t);
  }, [preview]);

  // Watchers start and stop on their own (a drive is plugged in, a watch fails quietly), so the
  // sidebar asks again now and then; it is a small in-memory answer.
  useEffect(() => {
    const t = setInterval(refreshLive, 30000);
    return () => clearInterval(t);
  }, [refreshLive]);

  const liveRef = useRef(live);
  const activeRef = useRef(activeId);
  useLayoutEffect(() => {
    liveRef.current = live;
    activeRef.current = activeId;
  });

  useEffect(() => {
    // Batches arrive about every 300 ms while something keeps writing, so they are gathered for
    // 500 ms from the first one and handed to every tab together. A window, not a debounce that
    // restarts on every batch: a long copy would hold every update back until it ends. The tab
    // shown acts on them at once; the others when they show again.
    let pending: (Changes & { drives: Set<string> }) | null = null;
    let flushTimer: number | null = null;
    let liveTimer: number | null = null;

    const flush = () => {
      flushTimer = null;
      const batch = pending;
      pending = null;
      if (!batch) return;
      broadcast(batch);
      // A watcher in error that came back goes on as "stale" without an event of its own, and a
      // drive not in live_status yet (just scanned) gets its state. One watching or stale stays so;
      // one syncing hears how the sync ended ("sync-progress"), unless that end has come already.
      const settled = (d: string) => {
        const s = liveRef.current[d]?.state;
        return s === "watching" || s === "stale" || (s === "syncing" && !syncEnded.current[d]);
      };
      if (![...batch.drives].every(settled)) refreshLive();
    };

    const subs = [
      events.indexChanged((p) => {
        const batch = (pending ??= { dirs: new Set(), full: new Set(), removed: new Set(), drives: new Set(), unsure: false });
        const drive = driveKey(p.drive);
        batch.drives.add(drive);
        if (p.full) batch.full.add(drive);
        for (const d of p.dirs) {
          // Too many to compare one by one: the whole drive counts as changed.
          if (batch.dirs.size >= 4096) {
            batch.full.add(drive);
            break;
          }
          batch.dirs.add(pathKey(d));
        }
        if (!p.removed) batch.unsure = true;
        else
          for (const r of p.removed) {
            if (batch.removed.size >= 4096) {
              batch.full.add(drive);
              break;
            }
            batch.removed.add(pathKey(r));
          }
        flushTimer ??= window.setTimeout(flush, 500);
      }),
      events.indexStale((p) => {
        const drive = driveKey(p.drive);
        if (HARD_LINKS.test(p.reason)) markNeedsFull(drive, p.reason);
        setStale((s) => ({ ...s, [drive]: p.reason }));
        refreshLive();
        // A watcher that fails to start reports before live_status has it: ask again shortly.
        if (liveTimer !== null) clearTimeout(liveTimer);
        liveTimer = window.setTimeout(() => {
          liveTimer = null;
          refreshLive();
        }, 1000);
      }),
    ]
      // Outside Tauri (a plain browser preview) there is nothing to listen to.
      .map((s) => s.catch(() => null));

    return () => {
      if (flushTimer !== null) clearTimeout(flushTimer);
      if (liveTimer !== null) clearTimeout(liveTimer);
      for (const s of subs) s.then((unlisten) => unlisten?.());
    };
  }, [refreshLive, broadcast, markNeedsFull]);

  // ---- tabs ----

  // Tabs opened from the tab shown go after the ones it opened before, in order, as in a browser.
  const lastOpened = useRef<{ from: number; id: number } | null>(null);
  const openTab = useCallback((path: string, show: boolean) => {
    const to = normalizePath(path);
    if (!to) return;
    const from = activeRef.current;
    const prev = lastOpened.current?.from === from ? lastOpened.current.id : null;
    const id = nextTab.current++;
    setTabs((ts) => {
      const at = ts.findIndex((t) => t.id === from);
      const after = Math.max(at, prev === null ? -1 : ts.findIndex((t) => t.id === prev));
      const n = [...ts];
      n.splice(after + 1, 0, { id, path: to, view: ts[at]?.view ?? "list" });
      return n;
    });
    lastOpened.current = { from, id };
    if (show) setActiveId(id);
  }, []);

  // The last tab stays. Closing the one shown shows the one to its right (or left, at the end).
  const closeTab = (id: number) => {
    if (tabs.length <= 1) return;
    const i = tabs.findIndex((t) => t.id === id);
    if (i < 0) return;
    const rest = tabs.filter((t) => t.id !== id);
    setTabs((ts) => (ts.length > 1 ? ts.filter((t) => t.id !== id) : ts));
    if (id === activeId) setActiveId(rest[Math.min(i, rest.length - 1)].id);
  };

  const cycleTab = (step: number) => {
    const i = tabs.findIndex((t) => t.id === activeId);
    if (i < 0 || tabs.length < 2) return;
    setActiveId(tabs[(i + step + tabs.length) % tabs.length].id);
  };

  const onTabState = useCallback((id: number, path: string, view: View) => {
    setTabs((ts) =>
      ts.some((t) => t.id === id && (t.path !== path || t.view !== view))
        ? ts.map((t) => (t.id === id ? { ...t, path, view } : t))
        : ts,
    );
  }, []);

  // The tab shown stays in sight when there are more tabs than fit.
  useEffect(() => {
    tabBar.current?.querySelector(".tab.on")?.scrollIntoView({ block: "nearest", inline: "nearest" });
  }, [activeId, tabs.length]);

  const togglePreview = useCallback(() => setPreview((v) => ({ ...v, open: !v.open })), []);
  const setPreviewWidth = useCallback((width: number) => setPreview((v) => (v.width === width ? v : { ...v, width })), []);

  // ---- copy, move, paste ----

  // Pastes started here, by job id, until they end.
  const pastes = useRef(new Map<number, Paste>());
  // Cuts being moved (clipKey): Ctrl+V again meanwhile would move the same items a second time.
  const moving = useRef(new Set<string>());
  // Jobs that ended before start_transfer answered with their id (a tiny job can).
  const ended = useRef(new Map<number, JobInfo>());

  // After a cut is pasted the clipboard is emptied, as Explorer does, so it cannot be pasted as
  // a move again. Only if it still holds that cut: the user may have copied something else since.
  const clearCut = useCallback(async (key: string) => {
    const same = async () => {
      const c = await ops.clipboardRead();
      return c.cut && clipKey(c.paths) === key ? c : null;
    };
    try {
      if (!(await same())) return;
      // The page's own clipboard text takes the files' place.
      await navigator.clipboard.writeText("").catch(() => {});
      // Not allowed (the window was not focused, say): the same files as a copy instead, so a
      // second paste copies what was left behind (skipped items) and never moves anything.
      const c = await same();
      if (c) await ops.clipboardWrite(c.paths, false);
    } catch {
      /* the clipboard is busy: it stays as it is */
    }
  }, []);

  const finishPaste = useCallback(
    (j: JobInfo, job: Paste) => {
      pastes.current.delete(j.id);
      if (job.cut) moving.current.delete(job.cut);
      // Drives without live updates show the result only this way; the others hear it twice.
      // A folder moved to another drive is still there, as a link: nothing went away.
      broadcast(changesOf(jobDirs(j.op, job.dest || j.dest, job.sources), j.op === "move" ? job.sources : []));
      refreshDrives().catch(() => {});
      if (job.cut && job.clip && j.state === "done") clearCut(job.cut);
    },
    [broadcast, refreshDrives, clearCut],
  );

  useEffect(() => {
    const off = opEvents
      .jobProgress((j) => {
        const job = pastes.current.get(j.id);
        if (isOver(j.state)) {
          if (job) finishPaste(j, job);
          else {
            // Not known yet, or started before the window reloaded (an undo from the list of
            // moved folders, too): at least its folder lists again.
            ended.current.set(j.id, j);
            if (ended.current.size > 20) ended.current.delete(ended.current.keys().next().value!);
            broadcast(changesOf([j.dest]));
            // A move between drives changes how much room both have.
            if (movesDrives(j.op)) refreshDrives().catch(() => {});
          }
          return;
        }
        // On drives without live updates, what came in so far shows now and then.
        const now = Date.now();
        if (!job || now - job.told < 1500) return;
        const unwatched = jobDirs(j.op, job.dest || j.dest, job.sources).filter(
          (d) => d !== null && liveRef.current[driveKey(d)]?.state !== "watching",
        );
        if (!unwatched.length) return;
        job.told = now;
        broadcast(changesOf(unwatched));
      })
      .catch(() => null);
    return () => {
      off.then((unlisten) => unlisten?.());
    };
  }, [broadcast, finishPaste, refreshDrives]);

  // A plan's run or undo ended (in whichever tab it was started): the tabs list what it changed
  // again (drives without live updates show it only this way), the drives' free space too, and
  // the drafts are counted again.
  useEffect(() => {
    const off = planEvents
      .progress((pr) => {
        if (pr.state === "running") return;
        refreshPlans();
        plans.get(pr.id).then(
          (plan) => {
            broadcast(planChanges(plan));
            refreshDrives().catch(() => {});
          },
          () => {},
        );
      })
      .catch(() => null);
    return () => {
      off.then((unlisten) => unlisten?.());
    };
  }, [broadcast, refreshDrives, refreshPlans]);

  // A job started here: the tabs hear what it changed once it ends.
  const track = useCallback(
    (id: number, job: Paste) => {
      const end = ended.current.get(id);
      ended.current.delete(id);
      if (end) finishPaste(end, job);
      else pastes.current.set(id, job);
    },
    [finishPaste],
  );

  // "다른 드라이브로 옮기기": the job reports in the transfer panel like a paste.
  const relocateStarted = useCallback(
    (id: number, source: string) => track(id, { sources: [source], dest: "", cut: null, told: 0 }),
    [track],
  );

  // Ctrl+V: the files on the Windows clipboard (ours or Explorer's) go into `dest`, moved when
  // they were cut. The job reports in the transfer panel.
  const paste = useCallback(
    async (dest: string) => {
      let clip: ClipboardFiles;
      try {
        clip = await ops.clipboardRead();
      } catch (err) {
        toast(String(err), "error");
        return;
      }
      if (!clip.paths.length) {
        toast("붙여넣을 파일이 없어요. 먼저 복사하거나 잘라내 주세요");
        return;
      }
      const cut = clip.cut ? clipKey(clip.paths) : null;
      if (cut && moving.current.has(cut)) {
        toast("이 항목들은 지금 옮기는 중이에요");
        return;
      }
      if (cut) moving.current.add(cut);
      try {
        const id = await ops.startTransfer(clip.cut ? "move" : "copy", clip.paths, dest);
        track(id, { sources: clip.paths, dest, cut, clip: true, told: 0 });
      } catch (err) {
        if (cut) moving.current.delete(cut);
        toast(String(err), "error");
      }
    },
    [toast, track],
  );

  // ---- drag and drop ----

  // Files dropped on the window (from Explorer, another program, or dragged out of a tab here)
  // go into the folder under the pointer, as a job of the transfer panel like a paste: a folder
  // row, tile or tree row (data-path), a place or drive in the sidebar (data-path), a tab's header
  // (data-tab-drop: what that tab takes), or else the folder the tab shows. Folders dropped in
  // 즐겨찾기 between its rows (or on its header, or below the rows) are pinned there instead
  // (pinDropAt); on a row's middle they still go into that folder. Dragged out of a tab
  // here they are copied, as the cursor shows over this window (the drag offers Windows only a
  // copy, see shell.ts), and moved when Shift is held as they are let go (read from Windows then;
  // the keys of the drag's start when it cannot tell). From another program they are always
  // copied. Moves are this app's own (copied and checked before the original goes, across drives).
  useEffect(() => trackModifiers(), []);

  /**
   * The folder a drop at a page point goes into and the element marked as its target, or where in
   * 즐겨찾기 it pins folders, or why it may not; null where nothing takes files (the transfer
   * panel, a dialog).
   */
  const dropAt = useCallback((x: number, y: number): DropAt => {
    const el = document.elementFromPoint(x, y);
    if (!el) return null;
    // A tab's header takes what its tab takes (not search results, nor a read-only folder).
    const header = el.closest("[data-tab-drop]");
    if (header) {
      const into = handles.current.get(Number(header.getAttribute("data-tab-drop")))?.dropTarget();
      if (!into) return null;
      return "refuse" in into ? into : { dest: into.path, mark: header };
    }
    const pin = pinDropAt(el, y);
    if (pin) return pin;
    const folder = el.closest("[data-path]");
    if (folder) return { dest: folder.getAttribute("data-path")!, mark: folder };
    // A tab's own dialog lies inside it: the tab then says no (dropTarget).
    const tab = el.closest("[data-tab]");
    const into = tab ? handles.current.get(Number(tab.getAttribute("data-tab")))?.dropTarget() : null;
    if (!tab || !into) return null;
    if ("refuse" in into) return into;
    return { dest: into.path, mark: tab.querySelector(".pane") ?? tab };
  }, []);

  /**
   * `mine`: a drag of this window (its items as dragged), with the keys held as it was let go.
   * Those are a copy unless Shift is held (a move, Ctrl+Shift too is a copy), as the cursor says
   * copy over this window whatever is held; another program's are always a copy.
   */
  const dropFiles = useCallback(
    async (paths: string[], dest: string, mine: Promise<{ ctrl: boolean; shift: boolean }> | null) => {
      const sources = paths.filter((s) => s.length > 0);
      if (!sources.length) return;
      const key = pathKey(dest);
      // A drag of this window let go on one of its folders, or in the folder its items are in
      // (often a click that slid into a drag): nothing to do, as in Explorer. With Ctrl too: a
      // copy beside the original is Ctrl+C, Ctrl+V.
      const inPlace =
        sources.some((s) => pathKey(s) === key) || sources.every((s) => pathKey(parentOf(s) ?? "") === key);
      if (mine && inPlace) return;
      if (sources.some((s) => isSameOrInside(key, pathKey(s)))) {
        toast("폴더를 그 폴더 안으로 옮기거나 복사할 수 없어요", "error");
        return;
      }
      const keys = mine ? await mine : null;
      const op = keys?.shift && !keys.ctrl ? "move" : "copy";
      const cut = op === "move" ? clipKey(sources) : null;
      if (cut && moving.current.has(cut)) {
        toast("이 항목들은 지금 옮기는 중이에요");
        return;
      }
      if (cut) moving.current.add(cut);
      try {
        const id = await ops.startTransfer(op, sources, dest);
        track(id, { sources, dest, cut, told: 0 });
      } catch (err) {
        if (cut) moving.current.delete(cut);
        toast(`놓은 항목을 ${op === "move" ? "옮기지" : "복사하지"} 못했어요: ${err}`, "error");
      }
    },
    [toast, track],
  );

  // The target under the pointer is marked while files are dragged over the window (again on every
  // move: a row drawn anew loses the class). A place in 즐겨찾기 to pin at has a class of its own.
  const marked = useRef<{ el: Element; cls: string } | null>(null);
  useEffect(() => {
    const mark = (el: Element | null, cls = DROP_MARK) => {
      const m = marked.current;
      if (m && (m.el !== el || m.cls !== cls)) m.el.classList.remove(m.cls);
      el?.classList.add(cls);
      marked.current = el ? { el, cls } : null;
    };
    const dragging = (on: boolean) => {
      dragOverWindow(on);
      document.documentElement.classList.toggle(FILE_DRAG, on);
    };
    let unlisten: (() => void) | null = null;
    let gone = false;
    getCurrentWebview()
      .onDragDropEvent((ev) => {
        const e = ev.payload;
        if (e.type === "enter" || e.type === "over") dragging(true);
        if (e.type === "leave") {
          dragging(false);
          return mark(null);
        }
        const at = pagePoint(e.position);
        // Hit-tested while the drag layout is still in place: leaving it first shrinks the
        // sidebar's drop pad, the drive cards move up and the point would land on another target.
        const target = dropAt(at.x, at.y);
        if (e.type === "drop") dragging(false);
        if (e.type !== "drop") {
          if (!target || !("mark" in target)) return mark(null);
          return mark(target.mark, "pin" in target ? target.cls : DROP_MARK);
        }
        mark(null);
        // A drag of this window: its items as this app named them, not as the drop reports them
        // (resolved through junctions and SUBST drives, see shell.ts). Its keys are read at once,
        // while those of the drop are still held.
        const mine = takeOwnDrop(e.paths);
        const held = mine && keysNow(mine.keys);
        if (!target) return;
        // A drag of this window let go where it started (search results): no need to say so.
        if ("refuse" in target) {
          if (!mine) toast(target.refuse);
          return;
        }
        // Pinned, not copied: only folders are kept (a file dropped there is said so).
        if ("pin" in target) {
          placesRef.current.pin(mine?.paths ?? e.paths, target.pin, true);
          return;
        }
        dropFiles(mine?.paths ?? e.paths, target.dest, held);
      })
      .then((u) => (gone ? u() : (unlisten = u)))
      // Outside Tauri (a plain browser preview) there is nothing to listen to.
      .catch(() => {});
    return () => {
      gone = true;
      unlisten?.();
      mark(null);
      document.documentElement.classList.remove(FILE_DRAG);
    };
  }, [dropAt, dropFiles, toast]);

  // ---- scans and syncs ----

  // The watcher now in charge (a new one after a scan or sync, or the old one after a failed one)
  // decides the banner: whatever a replaced watcher reported is gone with it. Except a removed
  // hard link (needsFull): a sync does not put that right, so its banner stays until a full scan.
  const settleBanner = useCallback(
    async (letter: string) => {
      for (let tries = 0; ; tries++) {
        const st = (await refreshLive())?.[letter];
        // A sync whose end was heard may not have let go of its turn yet: asked again shortly.
        if (st?.state === "syncing" && syncEnded.current[letter] && tries < 5) {
          await new Promise((r) => setTimeout(r, 400));
          continue;
        }
        if (st?.state === "stale" && st.detail && HARD_LINKS.test(st.detail)) markNeedsFull(letter, st.detail);
        const full = needsFullRef.current[letter];
        setStale((s) =>
          st?.state === "error"
            ? { ...s, [letter]: st.detail ?? LIVE_LABEL.error }
            : full !== undefined
              ? { ...s, [letter]: full }
              : st?.state === "stale"
                ? { ...s, [letter]: st.detail ?? LIVE_LABEL.stale }
                : without(s, letter),
        );
        return;
      }
    },
    [refreshLive, markNeedsFull],
  );

  /** A full scan of the drive started since its sync did: the scan stopped it, and brings its own result. */
  const replacedByScan = (letter: string) => {
    const at = scanAt.current[letter];
    return at !== undefined && at >= (syncAt.current[letter] ?? 0);
  };

  /**
   * A full scan. "auto" (the button of a drive not scanned yet) and "mft": the MFT with
   * administrator rights, one UAC prompt unless the admin session is on; "walk": a folder walk with
   * normal rights. A sync of the drive under way stops.
   */
  const scan = useCallback(
    async (letter: string, mode: "auto" | "mft" | "walk" = "auto") => {
      setDriveMenu(null);
      setScanning(letter);
      scanAt.current[letter] = Date.now();
      // A fresh scan catches up on whatever live updates missed, so the banner goes now. One that
      // the new watcher sends (it could not start) comes during the scan, and the end decides.
      setStale((s) => without(s, letter));
      try {
        const r = await api.scan(letter, mode);
        // Counted from scratch: a removed hard link no longer leaves the sizes off.
        markNeedsFull(letter, null);
        toast(`${letter} ${METHOD_DONE[r.method] ?? "스캔"} 완료: ${r.entries.toLocaleString()}개, ${(r.scan_ms / 1000).toFixed(1)}초`);
        if (r.note) toast(r.note, "error");
        await refreshDrives();
        // Every tab lists again: the tab shown now, the others when they show.
        for (const h of handles.current.values()) h.resync();
      } catch (e) {
        toast(`스캔 실패: ${e}`, "error");
      } finally {
        setScanning(null);
      }
      settleBanner(letter);
    },
    [toast, refreshDrives, settleBanner, markNeedsFull],
  );

  /**
   * "동기화": a quick sync of a scanned drive, with normal rights (never a UAC prompt). It walks the
   * folders again and keeps what the last scan found in those this account cannot open. How it goes
   * shows in the sidebar ("sync-progress"); how it ended, here.
   */
  const sync = useCallback(
    async (letter: string) => {
      setDriveMenu(null);
      if (syncsRef.current[letter] || askedRef.current.has(letter)) return;
      askedRef.current.add(letter);
      setAsked((a) => ({ ...a, [letter]: true }));
      syncAt.current[letter] = Date.now();
      syncOpen.current[letter] = true;
      try {
        const r = await v07.syncDrive(letter);
        toast(`${letter} 동기화 완료 (UAC 창 없이): ${r.entries.toLocaleString()}개, ${(r.scan_ms / 1000).toFixed(1)}초`);
        if (r.note) toast(r.note);
        await refreshDrives();
        for (const h of handles.current.values()) h.resync();
      } catch (e) {
        // A sync that started by itself just before runs on: it is shown, and its end is what counts.
        if (!replacedByScan(letter)) toast(`${letter} 동기화하지 못했어요: ${e}`, syncsRef.current[letter] ? "info" : "error");
      } finally {
        askedRef.current.delete(letter);
        setAsked((a) => without(a, letter));
        if (!syncsRef.current[letter]) syncOpen.current[letter] = false;
      }
      settleBanner(letter);
    },
    [toast, refreshDrives, settleBanner],
  );

  // Every sync, the ones that run by themselves too (once per drive after startup, and a while after
  // live updates missed changes): the sidebar shows it while it runs. Once it is done the drive's
  // details are read again and its banner goes (the new watcher decides); its index-changed event
  // has the tabs list again. One that failed by itself says why, unless a scan replaced it.
  useEffect(() => {
    const off = v07Events
      .syncProgress((p) => {
        const letter = driveKey(p.drive);
        syncEnded.current[letter] = p.state !== "running";
        if (p.state === "running") {
          if (!syncOpen.current[letter]) {
            syncOpen.current[letter] = true;
            syncAt.current[letter] = Date.now();
          }
          syncsRef.current = { ...syncsRef.current, [letter]: p };
          setSyncs(syncsRef.current);
          return;
        }
        syncOpen.current[letter] = false;
        syncsRef.current = without(syncsRef.current, letter);
        setSyncs(syncsRef.current);
        // One asked for here tells how it went where it was asked for (sync).
        if (p.state === "failed" && p.reason !== "user" && !replacedByScan(letter)) {
          toast(`${letter} 자동 동기화를 하지 못했어요: ${p.detail ?? "알 수 없는 오류"}`);
        }
        if (p.state === "done") refreshDrives().catch(() => {});
        settleBanner(letter);
      })
      // Outside Tauri (a plain browser preview) there is nothing to listen to.
      .catch(() => null);
    return () => {
      off.then((unlisten) => unlisten?.());
    };
  }, [toast, refreshDrives, settleBanner]);

  /** The drive's live status, but not a "syncing" read after that sync's end was heard. */
  const liveOf = (letter: string): LiveStatus | undefined => {
    const st = live[letter];
    return st?.state === "syncing" && syncEnded.current[letter] ? undefined : st;
  };

  /** A sync of the drive runs, or was asked for here and has not answered yet. */
  const isSyncing = (letter: string) => !!syncs[letter] || !!asked[letter] || liveOf(letter)?.state === "syncing";

  // The "▾" menu: under its button, kept inside the window. A click elsewhere (its own button
  // toggles it), Esc, or leaving the window closes it.
  const openDriveMenu = (ev: React.MouseEvent<HTMLButtonElement>, letter: string) => {
    if (driveMenu?.letter === letter) {
      setDriveMenu(null);
      return;
    }
    const r = ev.currentTarget.getBoundingClientRect();
    setDriveMenu({ letter, x: r.left, y: r.bottom + 4 });
  };
  useEffect(() => {
    if (!driveMenu) return;
    const close = (ev: Event) => {
      if (ev.target instanceof Element && ev.target.closest(".drive-menu, .scan.more")) return;
      setDriveMenu(null);
    };
    const onKey = (ev: KeyboardEvent) => ev.key === "Escape" && setDriveMenu(null);
    const onBlur = () => setDriveMenu(null);
    window.addEventListener("click", close);
    window.addEventListener("keydown", onKey);
    window.addEventListener("blur", onBlur);
    return () => {
      window.removeEventListener("click", close);
      window.removeEventListener("keydown", onKey);
      window.removeEventListener("blur", onBlur);
    };
  }, [driveMenu]);
  useLayoutEffect(() => {
    const el = driveMenuBox.current;
    if (!driveMenu || !el) return;
    const { width, height } = el.getBoundingClientRect();
    el.style.left = `${Math.max(4, Math.min(driveMenu.x, window.innerWidth - width - 4))}px`;
    el.style.top = `${driveMenu.y + height <= window.innerHeight - 4 ? driveMenu.y : Math.max(4, driveMenu.y - height - 36)}px`;
  }, [driveMenu]);

  // A mouse click must not leave a button focused: the rows own the keyboard, and Enter on a
  // focused button would click it too (open a folder, then jump back to the drive root).
  useEffect(() => {
    const onClick = (ev: MouseEvent) => {
      const a = document.activeElement;
      if (ev.detail > 0 && a instanceof HTMLButtonElement) a.blur();
    };
    // Capture phase, because some buttons stop the click from bubbling.
    window.addEventListener("click", onClick, true);
    return () => window.removeEventListener("click", onClick, true);
  }, []);

  const activeTab = tabs.find((t) => t.id === activeId);
  const path = activeTab?.path ?? "";

  // Tab and window shortcuts; each tab handles its own list keys.
  useEffect(() => {
    const onKey = (ev: KeyboardEvent) => {
      // A dialog of the tab shown (the trash question) is answered first.
      if (handles.current.get(activeId)?.busy()) return;
      const ctrl = ev.ctrlKey && !ev.altKey && !ev.metaKey;
      if (ctrl && ev.key === "Tab") {
        ev.preventDefault();
        cycleTab(ev.shiftKey ? -1 : 1);
      } else if (ctrl && !ev.shiftKey && isLetter(ev, "t")) {
        ev.preventDefault();
        if (activeTab) openTab(activeTab.path, true);
      } else if (ctrl && !ev.shiftKey && isLetter(ev, "w")) {
        ev.preventDefault();
        closeTab(activeId);
      } else if (ev.altKey && !ev.ctrlKey && !ev.metaKey && isLetter(ev, "p")) {
        // The details pane (상세 정보): Alt+P, and Alt+Shift+P as in Explorer (Shift is not looked at).
        ev.preventDefault();
        togglePreview();
      } else if (ev.key === "F1" && !ev.ctrlKey && !ev.altKey && !ev.metaKey && !ev.shiftKey) {
        // The help. While a dialog is open (the help itself too) its keys stop there.
        ev.preventDefault();
        setDriveMenu(null);
        openHelp();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  const go = (to: string) => handles.current.get(activeId)?.navigate(to);
  // Middle click on a place or drive opens it in a new tab, behind the one shown.
  const middle = (to: string) => (ev: React.MouseEvent) => {
    if (ev.button !== 1) return;
    ev.preventDefault();
    openTab(to, false);
  };

  const drive = drives.find((d) => path.toUpperCase().startsWith(d.letter));
  // Live updates lost changes, or stopped. Missed changes are put right by a sync that runs by
  // itself a while later (local disks; no UAC prompt), which "동기화" runs at once; a watcher that
  // stopped starts again with a sync too. After a removed hard link only a full scan counts right.
  const banners = Object.entries(stale).map(([letter, reason]) => {
    const d = drives.find((x) => x.letter === letter);
    const st = liveOf(letter);
    const stopped = st?.state === "error" || st?.state === "off";
    // A removed hard link outlasts syncs and missed changes (a full scan puts both right); a
    // watcher that stopped is told first, as "동기화" starts it again.
    const linksReason = needsFull[letter] ?? (HARD_LINKS.test(reason) ? reason : undefined);
    const fullOnly = linksReason !== undefined && !stopped;
    const dismiss = (
      <button className="dismiss" title="닫기" aria-label="닫기" onClick={() => setStale((s) => without(s, letter))}>
        ✕
      </button>
    );
    const why = (
      <button className="link" onClick={() => openHelp("frequentScan")} {...tip("help", "왜 이런 일이 생기는지 도움말에서 봐요")}>
        자세히
      </button>
    );
    if (isSyncing(letter)) {
      return (
        <div key={letter} className="banner info" role="status">
          <span className="grow">
            {letter} 놓친 변경을 다시 맞추는 중이에요 (관리자 권한 없이, UAC 창 없이).
            <span className="banner-detail">{syncs[letter]?.detail ?? "폴더를 다시 훑는 중이에요"}</span>
          </span>
          {dismiss}
        </div>
      );
    }
    if (fullOnly) {
      return (
        <div key={letter} className="banner">
          <span className="grow">
            {letter} {linksReason}
            <span className="banner-detail">
              "동기화"로는 바로잡히지 않아서(바뀌지 않은 파일은 지난 셈을 그대로 써요), 동기화한 뒤에도 전체 스캔을 할 때까지 다시
              알려 줘요.
            </span>
          </span>
          {why}
          <button disabled={scanning !== null || !d || !mftReadable(d)} onClick={() => scan(letter, "mft")} {...tip("fullScan")}>
            {scanning === letter ? "스캔 중…" : "전체 다시 스캔"}
          </button>
          {dismiss}
        </div>
      );
    }
    const auto = !stopped && localDisk(d);
    return (
      <div key={letter} className={`banner ${auto ? "info" : ""}`}>
        <span className="grow" title={auto ? reason : undefined}>
          {auto ? (
            <>
              {letter} 실시간 반영이 변경을 일부 놓쳤을 수 있어요. 관리자 권한 없이(UAC 창 없이) 자동으로 다시 맞춰요. 처음에는
              30초쯤 뒤지만, 자주 놓치는 드라이브는 간격이 길어져요(10분에서 길게는 2시간 40분). 기다리지 않으려면 "동기화"를
              누르세요.
            </>
          ) : (
            <>
              {letter} {reason}
              <span className="banner-detail">"동기화"로도 다시 켤 수 있어요. UAC 창 없이 폴더를 다시 훑고 실시간 반영을 다시 시작해요.</span>
            </>
          )}
        </span>
        {why}
        <button disabled={scanning === letter || !d?.indexed} onClick={() => sync(letter)} {...tip("sync")}>
          {scanning === letter ? "스캔 중…" : "동기화"}
        </button>
        {dismiss}
      </div>
    );
  });

  const menuDrive = driveMenu ? drives.find((d) => d.letter === driveMenu.letter) : undefined;

  return (
    <div className="app">
      {/* The navigation pane: 즐겨찾기, drives, tools. ↑ ↓ move through its rows (sideNavKeys). */}
      <nav className="sidebar" aria-label="탐색 창" onKeyDown={sideNavKeys}>
        <div className="brand">
          fine-file <span className="ver">v{version}</span>
        </div>
        <FavoritesSection
          places={places}
          path={path}
          onGo={go}
          onOpenTab={openTab}
          toast={toast}
          onHelp={openHelp}
        />
        <SidebarSection id="drives" title="드라이브" places={places}>
          {drives.map((d) => {
            const used = d.total - d.free;
            const pct = d.total ? (used / d.total) * 100 : 0;
            // A drop reported by "index-stale" shows as "stale" while its banner is open, unless the
            // watcher reports something more telling: an error, or that it is off. "off" without a
            // detail only means live_status had no watcher for it yet (one that failed to start
            // reports before it is registered). A sync running shows over all of them. A removed hard
            // link (needsFull) stays until a full scan, whatever a newer watcher says, but an error
            // or an "off" with a reason is told first.
            const st = liveOf(d.letter);
            const banner = stale[d.letter];
            const full = needsFull[d.letter];
            const syncing = isSyncing(d.letter);
            const sp = syncs[d.letter];
            const bannerWins = !st || st.state === "watching" || (st.state === "off" && st.detail === null);
            const fullWins = full !== undefined && (bannerWins || st?.state === "stale");
            const ls: LiveStatus | undefined = !d.indexed
              ? undefined
              : syncing
                ? { drive: d.letter, state: "syncing", detail: sp?.detail ?? (st?.state === "syncing" ? st.detail : null) }
                : fullWins
                  ? { drive: d.letter, state: "stale", detail: full }
                  : banner !== undefined && bannerWins
                    ? { drive: d.letter, state: "stale", detail: banner }
                    : st;
            const why = sp ? SYNC_REASON[sp.reason] : undefined;
            const scanThis = scanning === d.letter;
            return (
              <div key={d.letter} className={`drive ${drive?.letter === d.letter ? "active" : ""}`}>
                <button
                  className="drive-main"
                  data-side-nav=""
                  data-path={`${d.letter}\\`}
                  onClick={() => go(`${d.letter}\\`)}
                  onMouseDown={middle(`${d.letter}\\`)}
                  {...tip("drive")}
                >
                  <div className="drive-name">
                    {d.label || (d.kind === "unresponsive" ? "응답 없음" : "로컬 디스크")} ({d.letter})
                  </div>
                  <div className="usage">
                    <span className={pct > 90 ? "full" : ""} style={{ width: `${pct}%` }} />
                  </div>
                  <div className="drive-meta">
                    {d.kind === "unresponsive"
                      ? "드라이브가 아직 응답하지 않아요"
                      : `${formatSize(d.free)} 남음 / ${formatSize(d.total)}`}
                  </div>
                  <div className="drive-meta">
                    {d.indexed
                      ? `${METHOD_SHORT[d.method ?? ""] ?? "일반 스캔"} · ${timeAgo(d.scanned_at ?? 0)}`
                      : "아직 스캔 안 함"}
                  </div>
                  {ls && (
                    <div className={`drive-meta live ${ls.state}`} title={ls.detail ?? undefined}>
                      {fullWins && !syncing ? NEEDS_FULL_LABEL : (LIVE_LABEL[ls.state] ?? ls.state)}
                      {ls.state === "syncing" && why ? ` · ${why}` : ""}
                    </div>
                  )}
                </button>
                {/* Scanned: "동기화" (normal rights, no UAC prompt). Not yet: "스캔" (the MFT, one UAC
                    prompt). "▾": a full rescan with administrator rights, or a plain walk. */}
                <div className="drive-actions">
                  {d.indexed ? (
                    <button
                      className="scan"
                      disabled={scanThis || syncing}
                      onClick={() => sync(d.letter)}
                      {...tip(
                        "sync",
                        full !== undefined
                          ? `${HELP_TIPS.sync}. 이 드라이브는 하드 링크가 여러 개인 파일이 지워져서 크기가 어긋났을 수 있는데, 그건 동기화로 바로잡히지 않아요: ▾ → "전체 다시 스캔"을 해 주세요`
                          : undefined,
                      )}
                    >
                      {syncing ? "동기화 중…" : scanThis ? "스캔 중…" : "동기화"}
                    </button>
                  ) : (
                    <button className="scan" disabled={scanning !== null} onClick={() => scan(d.letter)} {...tip("scan")}>
                      {scanThis ? "스캔 중…" : "스캔"}
                    </button>
                  )}
                  <button
                    className={`scan more ${driveMenu?.letter === d.letter ? "on" : ""}`}
                    aria-haspopup="menu"
                    aria-expanded={driveMenu?.letter === d.letter}
                    aria-label={`${d.letter} 다른 스캔 방식`}
                    onClick={(ev) => openDriveMenu(ev, d.letter)}
                    {...tip("scanMore")}
                  >
                    ▾
                  </button>
                </div>
              </div>
            );
          })}
        </SidebarSection>
        <SidebarSection id="tools" title="도구" places={places}>
          <button
            className="place"
            data-side-nav=""
            onClick={() => handles.current.get(activeId)?.openPrograms()}
            {...tip("programs")}
          >
            프로그램 정리
          </button>
          <button className="place" data-side-nav="" onClick={() => setUpdateOpen(true)} {...tip("update")}>
            업데이트{updateOffered ? " · 새 버전" : ""}
          </button>
          <button className="place" data-side-nav="" onClick={() => openHelp()} {...tip("help")}>
            도움말 (F1)
          </button>
        </SidebarSection>
      </nav>

      {/* Other ways to scan the drive than its button's. */}
      {driveMenu && menuDrive && (
        <div ref={driveMenuBox} className="menu drive-menu" role="menu" style={{ left: driveMenu.x, top: driveMenu.y }}>
          <button
            role="menuitem"
            disabled={scanning !== null || !mftReadable(menuDrive)}
            onClick={() => scan(menuDrive.letter, "mft")}
            {...tip(
              "fullScan",
              !mftReadable(menuDrive)
                ? "MFT 고속 스캔은 이 컴퓨터에 붙은 NTFS 드라이브에서만 할 수 있어요. 일반 탐색으로 스캔해 주세요"
                : scanning !== null
                  ? "다른 드라이브를 스캔하는 중이에요. 끝난 뒤에 해 주세요"
                  : undefined,
            )}
          >
            {menuDrive.indexed ? "전체 다시 스캔" : "전체 스캔"} (관리자, 가장 정확)
            <span className="menu-note">보호된 폴더까지 처음부터 다시 세요. UAC 창이 떠요 (관리자 모드 중에는 묻지 않아요)</span>
          </button>
          <button
            role="menuitem"
            disabled={scanning !== null}
            onClick={() => scan(menuDrive.letter, "walk")}
            {...tip("walkScan", scanning !== null ? "다른 드라이브를 스캔하는 중이에요. 끝난 뒤에 해 주세요" : undefined)}
          >
            {menuDrive.indexed ? "일반 탐색으로 다시 스캔" : "일반 탐색으로 스캔"}
            <span className="menu-note">UAC 창 없이 처음부터. 내 계정으로 열 수 없는 폴더는 빠져요</span>
          </button>
          <hr />
          <button
            role="menuitem"
            onClick={() => {
              setDriveMenu(null);
              openHelp("scan");
            }}
            {...tip("help", "동기화, 전체 스캔, 일반 탐색이 어떻게 다른지 도움말에서 봐요")}
          >
            스캔 방식 설명…
          </button>
        </div>
      )}

      <main className="main">
        {/* v0.8: a new version, offered (never installed without the user's say). */}
        <UpdateBanner />
        <div
          ref={tabBar}
          className="tabs"
          role="tablist"
          // A mouse wheel moves along the tabs when there are more than fit.
          onWheel={(ev) => {
            if (ev.deltaX === 0) ev.currentTarget.scrollLeft += ev.deltaY;
          }}
        >
          {tabs.map((t) => (
            <div
              key={t.id}
              role="tab"
              aria-selected={t.id === activeId}
              className={`tab ${t.id === activeId ? "on" : ""}`}
              title={t.path}
              // Files dropped here go where this tab takes them (see the drop above).
              data-tab-drop={t.id}
              onMouseDown={(ev) => {
                // Middle click closes, as in a browser (and is not the browser's autoscroll).
                if (ev.button === 1) {
                  ev.preventDefault();
                  closeTab(t.id);
                } else if (ev.button === 0) setActiveId(t.id);
              }}
            >
              <span className="tab-label">{folderTitle(t.path)}</span>
              {tabs.length > 1 && (
                <button
                  className="tab-close"
                  title="탭 닫기 (Ctrl+W)"
                  onMouseDown={(ev) => ev.stopPropagation()}
                  onClick={() => closeTab(t.id)}
                >
                  ✕
                </button>
              )}
            </div>
          ))}
          <button className="tab-new" title="새 탭 (Ctrl+T)" onClick={() => activeTab && openTab(activeTab.path, true)}>
            +
          </button>
        </div>

        <div className="explorers">
          {tabs.map((t) => (
            <Explorer
              key={t.id}
              ref={(h) => {
                if (h) handles.current.set(t.id, h);
                return () => {
                  handles.current.delete(t.id);
                };
              }}
              id={t.id}
              active={t.id === activeId}
              initialPath={t.path}
              initialView={t.view}
              drives={drives}
              elevated={elevated}
              scanning={scanning}
              onScan={scan}
              refreshDrives={refreshDrives}
              banners={t.id === activeId ? banners : null}
              previewOpen={preview.open}
              previewWidth={preview.width}
              onTogglePreview={togglePreview}
              onPreviewWidth={setPreviewWidth}
              toast={toast}
              onState={onTabState}
              onOpenTab={openTab}
              onPaste={paste}
              onRelocateStarted={relocateStarted}
              onChanged={notifyOthers}
              ai={aiState}
              onAiChanged={refreshAi}
              planDrafts={planDrafts}
              onPlansChanged={refreshPlans}
              onHelp={openHelp}
              pinned={places.isPinned}
              onPin={setPinned}
            />
          ))}
        </div>
      </main>

      {/* One panel for every tab's copies and moves; it asks about name conflicts too. */}
      <Transfers onToast={toast} />

      {/* v0.7: what each button and menu item does (their aria-describedby), the help, the guide. */}
      <TipTexts />
      {help && <Help topic={help.topic} onClose={() => setHelp(null)} />}
      {updateOpen && <UpdateSettingsDialog onClose={() => setUpdateOpen(false)} />}
      {guide && <FirstRunGuide onDone={() => setGuide(false)} />}

      <div className="toasts">
        {toasts.map((t) => (
          <div key={t.id} className={`toast ${t.kind}`}>
            {t.text}
          </div>
        ))}
      </div>
    </div>
  );
}
