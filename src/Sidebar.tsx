import { useCallback, useEffect, useId, useLayoutEffect, useMemo, useRef, useState } from "react";
import { desktopDir, documentDir, downloadDir, homeDir } from "@tauri-apps/api/path";
import { api, ops, places as placesApi, type PinnedPlace, type Place, type PlaceKind, type PlacesConfig } from "./api";
import { Icon, MenuButton, MenuRow } from "./CommandBar";
import { tip } from "./Explorer";
import { folderTitle, normalizePath, pathKey, shown } from "./format";
import { watchPress } from "./shell";
import "./sidebar.css";

// ---- v0.8.1: the sidebar's sections and its 즐겨찾기, as Explorer's navigation pane has them ----
// Known folders (바탕 화면, 다운로드, 문서…) and OneDrive come from Windows (places.known); the
// folders the user pins, the places hidden and the sections folded are kept in places.json
// (places.get / places.set). Pinning: a folder's menu, a drop of folders between the items (App's
// drop asks pinDropAt), Explorer's own pins brought in. A drop on an item's middle still copies
// into that folder, as before.

/** places_set keeps at most this many pinned folders. */
export const MAX_PINS = 100;

/** Changes are written once they rest this long (a section folded and opened again, several moves). */
const SAVE_MS = 300;

/** All pinned folders are looked for again (gone? back?) at most this often, as the window comes back. */
const RECHECK_MS = 60_000;

/** A pin at the end of the pinned folders. */
const END = Number.MAX_SAFE_INTEGER;

/** The sidebar's foldable sections (PlacesConfig.collapsed). */
export type SectionId = "quick" | "drives" | "tools";

const EMPTY: PlacesConfig = { pinned: [], hidden: [], collapsed: [], imported_explorer: false };

/**
 * Known places in Explorer's order, whatever order they came in: 홈 and OneDrive (the top group,
 * a line under it), then the user's folders, which the pinned folders follow.
 */
const KIND_ORDER: PlaceKind[] = [
  "home",
  "onedrive",
  "onedrive_business",
  "desktop",
  "downloads",
  "documents",
  "pictures",
  "music",
  "videos",
];

/** Above the line in 즐겨찾기, as Explorer draws 홈 and OneDrive. */
const TOP_KINDS = new Set<PlaceKind>(["home", "onedrive", "onedrive_business"]);

/** A listing or preview error that means the item is not there (any more). */
const GONE = /\(os error (2|3)\)/;

/** places_set's limit on a pinned folder's own name (places.rs MAX_LABEL). */
const MAX_LABEL = 120;

/**
 * A path places_set takes (places.rs, normalize_abs): on a drive letter, no "..", no character
 * Windows names cannot hold. A network place (\\server\share) is not one.
 */
const isAbsolute = (p: string) =>
  /^[A-Z]:\\/.test(p) && !p.slice(3).split("\\").some((s) => s === ".." || /[:*?"<>|\x00-\x1f\x7f-\x9f]/.test(s));

/** A network folder (\\server\share\...), as known_places gives a redirected Desktop or Documents. */
const isNetwork = (p: string) =>
  /^\\\\[^\\?.][^\\]*\\[^\\]+/.test(p) && !p.slice(2).split("\\").some((s) => s === ".." || /[*?"<>|\x00-\x1f\x7f-\x9f]/.test(s));

/** A name as places_set keeps it: no control characters, trimmed, at most MAX_LABEL characters. */
const cleanLabel = (s: string) => Array.from(s.replace(/[\x00-\x1f\x7f]/g, "").trim()).slice(0, MAX_LABEL).join("").trim();

const NETWORK_PIN = "네트워크 위치(\\\\서버\\…)의 폴더는 즐겨찾기에 고정할 수 없어요. 드라이브 문자로 연결한 뒤 고정해 주세요";

const strings = (v: unknown): string[] => (Array.isArray(v) ? v.filter((s): s is string => typeof s === "string") : []);

/** What places_get answered, safe to use: an older or hand-edited file may differ. */
function cleanConfig(v: Partial<PlacesConfig> | null | undefined): PlacesConfig {
  const seen = new Set<string>();
  const pinned: PinnedPlace[] = [];
  for (const p of Array.isArray(v?.pinned) ? v.pinned : []) {
    const path = typeof p?.path === "string" ? normalizePath(p.path) : "";
    const key = pathKey(path);
    if (!isAbsolute(path) || seen.has(key) || pinned.length >= MAX_PINS) continue;
    seen.add(key);
    const label = typeof p.label === "string" ? cleanLabel(p.label) : "";
    pinned.push({ path, label: label || null });
  }
  return {
    pinned,
    hidden: strings(v?.hidden),
    collapsed: strings(v?.collapsed),
    imported_explorer: v?.imported_explorer === true,
  };
}

const rank = (k: PlaceKind) => {
  const i = KIND_ORDER.indexOf(k);
  return i < 0 ? KIND_ORDER.length : i;
};

/**
 * known_places' answer in the sidebar's order, once per folder. The profile folder is "홈" as in
 * v0.8 (its shell name is the account's name).
 */
function sortKnown(list: Place[]): Place[] {
  const seen = new Set<string>();
  return list
    // Known places may be on a network share (folders redirected by a company's policy); only
    // their ids are saved, so places_set's drive-letter rule does not apply to them.
    .filter((p) => typeof p?.path === "string" && (isAbsolute(normalizePath(p.path)) || isNetwork(normalizePath(p.path))))
    .map((p, i) => ({
      p: { ...p, path: normalizePath(p.path), label: p.kind === "home" ? "홈" : p.label || folderTitle(p.path) },
      i,
    }))
    .sort((a, b) => rank(a.p.kind) - rank(b.p.kind) || a.i - b.i)
    .map((x) => x.p)
    .filter((p) => !seen.has(pathKey(p.path)) && !!seen.add(pathKey(p.path)));
}

/** v0.8's 즐겨찾기, when known_places cannot answer (an older backend, an error). */
async function fallbackPlaces(): Promise<Place[]> {
  const out: Place[] = [];
  for (const [kind, label, dir] of [
    ["home", "홈", homeDir],
    ["desktop", "바탕 화면", desktopDir],
    ["documents", "문서", documentDir],
    ["downloads", "다운로드", downloadDir],
  ] as const) {
    try {
      out.push({ id: kind, kind, label, path: normalizePath(await dir()) });
    } catch {
      /* folder not configured */
    }
  }
  return out;
}

/** Folders of `found` that are neither pinned nor known places, once each. */
function newPins(found: string[], c: PlacesConfig, known: Place[]): string[] {
  const have = new Set([...c.pinned.map((p) => pathKey(p.path)), ...known.map((k) => pathKey(k.path))]);
  const out: string[] = [];
  for (const raw of found) {
    const path = normalizePath(String(raw));
    const key = pathKey(path);
    if (!isAbsolute(path) || have.has(key)) continue;
    have.add(key);
    out.push(path);
  }
  return out;
}

/**
 * Whether a pinned folder is there: true, false (not found), or null when that cannot be told (a
 * network place that does not answer, access denied). preview_info reads only a folder's own
 * metadata (a listing would read every child), so it is a cheap look.
 */
async function folderThere(path: string): Promise<boolean | null> {
  try {
    await ops.previewInfo(path);
    return true;
  } catch (e) {
    return GONE.test(String(e)) ? false : null;
  }
}

/** What a dropped item is. A folder this account cannot open, or one that cannot be told, counts as a folder. */
async function kindOf(path: string): Promise<"dir" | "file" | "gone"> {
  try {
    // path_kind looks at the item itself; listing the folder (list_dir) could read thousands of
    // entries on a network or unscanned drive before the pin shows.
    return await placesApi.pathKind(path);
  } catch {
    return "dir";
  }
}

// ---- the places: loaded, pinned, saved -----------------------------------------------

export interface PlacesState {
  /** places_get has answered (or failed): the pinned folders are known. */
  ready: boolean;
  config: PlacesConfig;
  /** Known places shown, in order (those hidden left out). */
  shown: Place[];
  /** Known places the user hid. */
  hiddenPlaces: Place[];
  /** Pinned folders not found (pathKey). */
  missing: ReadonlySet<string>;
  /** Explorer's pinned folders offered on the first run, until brought in or turned down. */
  offer: string[] | null;
  /** Shown in 즐겨찾기: pinned, or a known place not hidden. */
  isPinned: (path: string) => boolean;
  /**
   * Pins folders at `at` (an index of the pinned folders; left out, the end). A folder pinned
   * already moves there. `check`: the paths may be files or gone (a drop); only folders are kept.
   * A hidden known place shows again instead.
   */
  pin: (paths: string[], at?: number, check?: boolean) => Promise<void>;
  /** A pinned folder is unpinned; a known place is hidden. */
  unpin: (path: string) => void;
  showHidden: () => void;
  /** The pinned folder at `from` goes before the one at `to` (the end: the pinned count). */
  move: (from: number, to: number) => void;
  /** A pinned folder's name in the sidebar; null for the folder's own. */
  rename: (path: string, label: string | null) => void;
  isCollapsed: (s: SectionId) => boolean;
  toggle: (s: SectionId) => void;
  /** "탐색기 즐겨찾기 가져오기": Explorer's pinned folders not here yet come in, at the end. */
  importExplorer: () => Promise<void>;
  takeOffer: () => void;
  dismissOffer: () => void;
  /** A folder not found, looked for again: true when it is there now (or cannot be told). */
  recheck: (path: string) => Promise<boolean>;
}

interface Added {
  /** Folders newly pinned, and hidden known places shown again. */
  added: number;
  /** Pinned or shown already. */
  already: number;
  /** Some did not fit (MAX_PINS). */
  full: boolean;
}

/** The sidebar's places: what Windows knows and what the user pinned, kept in places.json. */
export function usePlaces(toast: (text: string, kind?: "info" | "error") => void): PlacesState {
  const [config, setConfig] = useState<PlacesConfig>(EMPTY);
  const [known, setKnown] = useState<Place[]>([]);
  const [ready, setReady] = useState(false);
  const [missing, setMissing] = useState<ReadonlySet<string>>(() => new Set());
  const [offer, setOffer] = useState<string[] | null>(null);
  const configRef = useRef(config);
  const knownRef = useRef(known);
  // Changes made before places_get answers (a section folded at once) are made again on what it read.
  const loaded = useRef(false);
  const pending = useRef<((c: PlacesConfig) => PlacesConfig)[]>([]);
  // places.json could not be read: nothing is written (it would replace the pins it holds with
  // this session's), and the changes made meanwhile are replayed onto it once a read works.
  const blocked = useRef(false);
  const blockedOps = useRef<((c: PlacesConfig) => PlacesConfig)[]>([]);
  const readyWait = useRef<{ promise: Promise<void>; resolve: () => void } | null>(null);
  if (!readyWait.current) {
    let resolve = () => {};
    const promise = new Promise<void>((r) => (resolve = r));
    readyWait.current = { promise, resolve };
  }

  // ---- saving: debounced, one write at a time and in order, failures told once ----
  const saveTimer = useRef<number | null>(null);
  const saving = useRef<Promise<unknown>>(Promise.resolve());
  const warned = useRef(false);
  const flush = useCallback(() => {
    if (saveTimer.current !== null) {
      clearTimeout(saveTimer.current);
      saveTimer.current = null;
    }
    const snapshot = configRef.current;
    saving.current = saving.current
      .then(() => placesApi.set(snapshot))
      .catch((err) => {
        if (warned.current) return;
        warned.current = true;
        toast(`즐겨찾기를 저장하지 못했어요. 바꾼 것은 앱을 끌 때까지만 남아요: ${err}`, "error");
      });
  }, [toast]);

  const commit = useCallback(
    (next: PlacesConfig) => {
      if (next === configRef.current) return;
      configRef.current = next;
      setConfig(next);
      if (!loaded.current || blocked.current) return;
      if (saveTimer.current !== null) clearTimeout(saveTimer.current);
      saveTimer.current = window.setTimeout(flush, SAVE_MS);
    },
    [flush],
  );

  const update = useCallback(
    (fn: (c: PlacesConfig) => PlacesConfig) => {
      if (!loaded.current) pending.current.push(fn);
      else if (blocked.current) blockedOps.current.push(fn);
      commit(fn(configRef.current));
    },
    [commit],
  );

  // A change still waiting when the window goes is written then.
  useEffect(() => {
    const out = () => {
      if (saveTimer.current !== null) flush();
    };
    window.addEventListener("pagehide", out);
    return () => window.removeEventListener("pagehide", out);
  }, [flush]);

  // Startup: the known places (v0.8's four when Windows cannot be asked) and what was saved.
  useEffect(() => {
    let gone = false;
    (async () => {
      const [list, saved] = await Promise.all([
        placesApi.known().then(
          (l) => (l.length ? sortKnown(l) : fallbackPlaces()),
          () => fallbackPlaces(),
        ),
        placesApi.get().then(cleanConfig, () => null),
      ]);
      if (gone) return;
      knownRef.current = list;
      setKnown(list);
      let c = saved ?? EMPTY;
      for (const fn of pending.current) c = fn(c);
      const replayed = pending.current.length > 0;
      if (!saved) {
        // Not written until a read works (see `blocked`); what was done before is kept for then.
        blocked.current = true;
        blockedOps.current.push(...pending.current);
        toast("즐겨찾기 목록을 읽지 못했어요. 덮어쓰지 않도록 이번에 바꾼 것은 목록을 다시 읽을 수 있을 때 저장할게요.", "error");
      }
      pending.current = [];
      loaded.current = true;
      configRef.current = c;
      setConfig(c);
      if (replayed && !blocked.current) flush();
      setReady(true);
      readyWait.current?.resolve();
      // The first run offers Explorer's pinned folders once, when there are new ones (and only
      // when the answer can be kept: places.json could be read).
      if (!saved || saved.imported_explorer) return;
      const done = () => update((x) => (x.imported_explorer ? x : { ...x, imported_explorer: true }));
      placesApi.explorerPins().then(
        (found) => {
          if (gone) return;
          const fresh = newPins(found, configRef.current, knownRef.current);
          if (fresh.length) setOffer(fresh);
          else done();
        },
        () => !gone && done(),
      );
    })();
    return () => {
      gone = true;
    };
  }, [flush, update, toast]);

  // Read again when the window comes back with nothing waiting to be written: another fine-file
  // window may have changed the pins meanwhile (each window writes its whole list), and a list that
  // could not be read at startup may be readable now.
  useEffect(() => {
    let busy = false;
    const reread = async () => {
      if (busy || !loaded.current || saveTimer.current !== null) return;
      busy = true;
      try {
        await saving.current;
        if (saveTimer.current !== null) return;
        let saved: PlacesConfig;
        try {
          saved = cleanConfig(await placesApi.get());
        } catch {
          return;
        }
        if (saveTimer.current !== null) return;
        if (blocked.current) {
          let c = saved;
          for (const fn of blockedOps.current) c = fn(c);
          const changed = blockedOps.current.length > 0;
          blockedOps.current = [];
          blocked.current = false;
          configRef.current = c;
          setConfig(c);
          if (changed) flush();
          return;
        }
        if (JSON.stringify(saved) !== JSON.stringify(configRef.current)) {
          configRef.current = saved;
          setConfig(saved);
        }
      } finally {
        busy = false;
      }
    };
    const onFocus = () => void reread();
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, [flush]);

  // Pinned folders not found. Each pinned folder is looked for once, and all of them again when the
  // window comes back a while later (a folder deleted or a drive unplugged meanwhile).
  const looked = useRef(new Set<string>());
  const lookedAt = useRef(0);
  const noteThere = useCallback((path: string, there: boolean) => {
    const key = pathKey(path);
    setMissing((s) => {
      if (there === !s.has(key)) return s;
      const n = new Set(s);
      if (there) n.delete(key);
      else n.add(key);
      return n;
    });
  }, []);

  /** Puts folders into the pinned list at `at`. `move`: one pinned already goes there too. */
  const addPins = useCallback(
    (paths: string[], at: number, opts: { move: boolean; known: "show" | "skip" }): Added => {
      const c = configRef.current;
      const list = knownRef.current;
      let hidden = c.hidden;
      let pinned = c.pinned;
      let pos = Math.max(0, Math.min(at, pinned.length));
      const add: PinnedPlace[] = [];
      const taken = new Set<string>();
      let already = 0;
      let full = false;
      for (const raw of paths) {
        const path = normalizePath(raw);
        const key = pathKey(path);
        if (!isAbsolute(path) || taken.has(key)) continue;
        const place = list.find((k) => pathKey(k.path) === key);
        if (place) {
          if (opts.known === "show" && hidden.includes(place.id)) hidden = hidden.filter((h) => h !== place.id);
          else already++;
          continue;
        }
        const i = pinned.findIndex((p) => pathKey(p.path) === key);
        if (i >= 0) {
          if (!opts.move) {
            already++;
            continue;
          }
          // Dropped again: it goes where it was dropped, with its name.
          if (i < pos) pos--;
          add.push(pinned[i]);
          pinned = pinned.filter((_, j) => j !== i);
          taken.add(key);
          continue;
        }
        if (pinned.length + add.length >= MAX_PINS) {
          full = true;
          continue;
        }
        add.push({ path, label: null });
        taken.add(key);
      }
      const before = new Set(c.pinned.map((p) => pathKey(p.path)));
      const added = add.filter((a) => !before.has(pathKey(a.path))).length + (c.hidden.length - hidden.length);
      if (add.length || hidden !== c.hidden) {
        commit({ ...c, hidden, pinned: [...pinned.slice(0, pos), ...add, ...pinned.slice(pos)] });
      }
      return { added, already, full };
    },
    [commit],
  );

  const fullText = `즐겨찾기에는 폴더를 ${MAX_PINS}개까지 고정할 수 있어요. 안 쓰는 폴더를 고정 해제해 주세요`;

  const pin = useCallback(
    async (paths: string[], at = END, check = false) => {
      await readyWait.current?.promise;
      let folders = paths.slice(0, MAX_PINS).map(normalizePath);
      // places.json keeps drive-letter paths only.
      const network = folders.filter((p) => !isAbsolute(p)).length;
      folders = folders.filter(isAbsolute);
      if (network) toast(NETWORK_PIN);
      if (!folders.length) return;
      if (check) {
        const kinds: ("dir" | "file" | "gone")[] = [];
        kinds.push(...(await Promise.all(folders.map(kindOf))));
        const files = kinds.filter((k) => k === "file").length;
        folders = folders.filter((_, i) => kinds[i] === "dir");
        if (files) toast(folders.length ? `파일 ${files}개는 빼고 폴더만 고정했어요` : "즐겨찾기에는 폴더만 고정할 수 있어요");
        if (!folders.length) return;
      }
      const r = addPins(folders, at, { move: true, known: "show" });
      if (r.full) toast(fullText, "error");
      else if (!r.added && r.already && at === END) toast("이미 즐겨찾기에 있어요");
    },
    [addPins, toast, fullText],
  );

  const unpin = useCallback(
    (path: string) => {
      const key = pathKey(path);
      // Pinned again later, it is looked for again.
      looked.current.delete(key);
      noteThere(path, true);
      update((c) => {
        let n = c;
        if (c.pinned.some((p) => pathKey(p.path) === key)) n = { ...n, pinned: c.pinned.filter((p) => pathKey(p.path) !== key) };
        const place = knownRef.current.find((k) => pathKey(k.path) === key);
        if (place && !n.hidden.includes(place.id)) n = { ...n, hidden: [...n.hidden, place.id] };
        return n;
      });
    },
    [update, noteThere],
  );

  const showHidden = useCallback(() => update((c) => (c.hidden.length ? { ...c, hidden: [] } : c)), [update]);

  const move = useCallback(
    (from: number, to: number) =>
      update((c) => {
        if (from < 0 || from >= c.pinned.length) return c;
        const at = Math.max(0, Math.min(c.pinned.length, to > from ? to - 1 : to));
        if (at === from) return c;
        const list = [...c.pinned];
        const [it] = list.splice(from, 1);
        list.splice(at, 0, it);
        return { ...c, pinned: list };
      }),
    [update],
  );

  const rename = useCallback(
    (path: string, label: string | null) => {
      const key = pathKey(path);
      update((c) => {
        const i = c.pinned.findIndex((p) => pathKey(p.path) === key);
        if (i < 0 || c.pinned[i].label === label) return c;
        return { ...c, pinned: c.pinned.map((p, j) => (j === i ? { ...p, label } : p)) };
      });
    },
    [update],
  );

  const toggle = useCallback(
    (s: SectionId) =>
      update((c) => ({
        ...c,
        collapsed: c.collapsed.includes(s) ? c.collapsed.filter((x) => x !== s) : [...c.collapsed, s],
      })),
    [update],
  );

  const markImported = useCallback(
    () => update((c) => (c.imported_explorer ? c : { ...c, imported_explorer: true })),
    [update],
  );

  const importedText = (r: Added) =>
    r.added
      ? `탐색기 즐겨찾기에서 폴더 ${r.added}개를 가져왔어요`
      : "새로 가져올 폴더가 없어요. 탐색기에 고정한 폴더는 이미 다 여기 있어요";

  const importExplorer = useCallback(async () => {
    await readyWait.current?.promise;
    let found: string[];
    try {
      found = await placesApi.explorerPins();
    } catch (err) {
      toast(`탐색기 즐겨찾기를 읽지 못했어요: ${err}`, "error");
      return;
    }
    const r = addPins(found, END, { move: false, known: "skip" });
    setOffer(null);
    markImported();
    toast(r.full ? fullText : importedText(r), r.full ? "error" : "info");
  }, [addPins, markImported, toast, fullText]);

  const takeOffer = useCallback(() => {
    if (!offer) return;
    const r = addPins(offer, END, { move: false, known: "skip" });
    setOffer(null);
    markImported();
    toast(r.full ? fullText : importedText(r), r.full ? "error" : "info");
  }, [offer, addPins, markImported, toast, fullText]);

  const dismissOffer = useCallback(() => {
    setOffer(null);
    markImported();
  }, [markImported]);

  // ---- pinned folders that went away ----
  const look = useCallback(
    async (paths: string[]) => {
      const queue = [...paths];
      const worker = async () => {
        for (let p = queue.shift(); p !== undefined; p = queue.shift()) {
          const there = await folderThere(p);
          if (there !== null) noteThere(p, there);
        }
      };
      await Promise.all([worker(), worker(), worker(), worker()]);
    },
    [noteThere],
  );
  const pinKeys = config.pinned.map((p) => pathKey(p.path)).join("\n");
  useEffect(() => {
    if (!ready) return;
    const todo = configRef.current.pinned.filter((p) => !looked.current.has(pathKey(p.path)));
    for (const p of todo) looked.current.add(pathKey(p.path));
    if (!lookedAt.current) lookedAt.current = Date.now();
    if (todo.length) look(todo.map((p) => p.path));
  }, [ready, pinKeys, look]);
  useEffect(() => {
    const onFocus = () => {
      if (!loaded.current || Date.now() - lookedAt.current < RECHECK_MS) return;
      lookedAt.current = Date.now();
      look(configRef.current.pinned.map((p) => p.path));
    };
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, [look]);

  const recheck = useCallback(
    async (path: string) => {
      const there = await folderThere(path);
      if (there !== null) noteThere(path, there);
      return there !== false;
    },
    [noteThere],
  );

  const shown = useMemo(() => known.filter((k) => !config.hidden.includes(k.id)), [known, config.hidden]);
  const hiddenPlaces = useMemo(() => known.filter((k) => config.hidden.includes(k.id)), [known, config.hidden]);
  const keys = useMemo(
    () => new Set([...shown.map((k) => pathKey(k.path)), ...config.pinned.map((p) => pathKey(p.path))]),
    [shown, config.pinned],
  );
  const isPinned = useCallback((path: string) => keys.has(pathKey(path)), [keys]);
  const isCollapsed = useCallback((s: SectionId) => config.collapsed.includes(s), [config.collapsed]);

  return {
    ready,
    config,
    shown,
    hiddenPlaces,
    missing,
    offer,
    isPinned,
    pin,
    unpin,
    showHidden,
    move,
    rename,
    isCollapsed,
    toggle,
    importExplorer,
    takeOffer,
    dismissOffer,
    recheck,
  };
}

// ---- drops of files on 즐겨찾기 (App's drop) ------------------------------------------

/** Where a pin goes for a point at height `y` over the list: before the first pinned row below its middle. */
function insertionAt(list: Element, y: number): { at: number; row: Element | null; after: boolean } {
  const rows = Array.from(list.querySelectorAll<HTMLElement>("li[data-fav-index]"));
  for (const row of rows) {
    const r = row.getBoundingClientRect();
    if (y < r.top + r.height / 2) return { at: Number(row.dataset.favIndex), row, after: false };
  }
  const last = rows[rows.length - 1];
  return last ? { at: Number(last.dataset.favIndex) + 1, row: last, after: true } : { at: END, row: null, after: true };
}

export interface PinDrop {
  /** The pinned index the folders go at. */
  pin: number;
  /** The element marked while files are over it, with `cls`. */
  mark: Element;
  cls: "pin-before" | "pin-after" | "pin-here";
}

/**
 * Files dragged over `el` (at height `y`) in 즐겨찾기: where they would be pinned, or null when
 * they go into a folder there (the middle of a row: App's data-path drop) or this is not 즐겨찾기.
 * Between rows, at a row's top or bottom quarter, below the list and on the section's header they
 * are pinned. Known places keep their order, so their rows always take files in.
 */
export function pinDropAt(el: Element, y: number): PinDrop | null {
  const zone = el.closest("[data-fav-drop]");
  if (zone) return { pin: END, mark: zone, cls: "pin-here" };
  const list = el.closest("[data-fav-list]");
  if (!list) return null;
  const row = el.closest("li.fav-li");
  if (row?.querySelector("[data-path]")) {
    if (!row.hasAttribute("data-fav-index")) return null;
    const r = row.getBoundingClientRect();
    const rel = (y - r.top) / Math.max(1, r.height);
    if (rel > 0.25 && rel < 0.75) return null;
  }
  const ins = insertionAt(list, y);
  if (!ins.row) {
    const pad = list.parentElement?.querySelector("[data-fav-drop]");
    return { pin: END, mark: pad ?? list, cls: "pin-here" };
  }
  return { pin: ins.at, mark: ins.row, cls: ins.after ? "pin-after" : "pin-before" };
}

// ---- keyboard ---------------------------------------------------------------------------

/**
 * ↑ ↓ Home End move through the sidebar's section headers and items (data-side-nav), as in
 * Explorer's navigation pane. For the sidebar's onKeyDown; the keys stop there, so the list of the
 * tab does not move too.
 */
export function sideNavKeys(ev: React.KeyboardEvent<HTMLElement>) {
  if (ev.ctrlKey || ev.altKey || ev.metaKey || ev.shiftKey) return;
  // Plain Delete and F2 belong to the file list (trash, rename the selected file): never from
  // anywhere in the sidebar. Inputs (the pinned-name box) keep them.
  const typing = ev.target instanceof HTMLInputElement || ev.target instanceof HTMLTextAreaElement;
  if ((ev.key === "Delete" || ev.key === "F2") && !typing) {
    ev.stopPropagation();
    return;
  }
  const t = ev.target as HTMLElement;
  if (!t.hasAttribute("data-side-nav")) return;
  const step = ev.key === "ArrowDown" ? 1 : ev.key === "ArrowUp" ? -1 : 0;
  if (!step && ev.key !== "Home" && ev.key !== "End") return;
  const all = Array.from(ev.currentTarget.querySelectorAll<HTMLElement>("[data-side-nav]")).filter(
    (el) => el.getClientRects().length > 0 && !(el as HTMLButtonElement).disabled,
  );
  const i = all.indexOf(t);
  const to = ev.key === "Home" ? 0 : ev.key === "End" ? all.length - 1 : Math.max(0, Math.min(all.length - 1, i + step));
  ev.preventDefault();
  ev.stopPropagation();
  all[to]?.focus();
}

/** The menu key, or Shift+F10. */
const isMenuKey = (ev: React.KeyboardEvent) =>
  (ev.key === "ContextMenu" && !ev.ctrlKey && !ev.altKey) ||
  (ev.key === "F10" && ev.shiftKey && !ev.ctrlKey && !ev.altKey && !ev.metaKey);

// ---- icons ------------------------------------------------------------------------------

function Glyph({ children }: { children: React.ReactNode }) {
  return (
    <svg
      width="16"
      height="16"
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.3"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      focusable="false"
    >
      {children}
    </svg>
  );
}

const cloud = () => (
  <Glyph>
    <path d="M4.4 12.6h7.3a2.6 2.6 0 0 0 .4-5.2 3.8 3.8 0 0 0-7.3-1.3 3.3 3.3 0 0 0-.4 6.5z" />
  </Glyph>
);

/** Drawn while the shell's icon is not there (yet), or for a folder not found. */
const GLYPHS: Record<PlaceKind | "folder", () => React.ReactNode> = {
  home: () => (
    <Glyph>
      <path d="M2.4 7.4 8 2.7l5.6 4.7" />
      <path d="M4 6.2v7h3v-3.1h2v3.1h3v-7" />
    </Glyph>
  ),
  desktop: () => (
    <Glyph>
      <rect x="1.8" y="2.6" width="12.4" height="8.4" rx="1" />
      <path d="M5.8 13.6h4.4M8 11v2.6" />
    </Glyph>
  ),
  downloads: () => (
    <Glyph>
      <path d="M8 2.2v7.6M4.9 6.8 8 9.9l3.1-3.1" />
      <path d="M2.6 10.8v1.8a1 1 0 0 0 1 1h8.8a1 1 0 0 0 1-1v-1.8" />
    </Glyph>
  ),
  documents: () => (
    <Glyph>
      <path d="M4 1.8h5.1L12 4.7v9.5H4z" />
      <path d="M9 1.8v3h3M6 8h4M6 10.6h4" />
    </Glyph>
  ),
  pictures: () => (
    <Glyph>
      <rect x="1.8" y="2.6" width="12.4" height="10.8" rx="1.2" />
      <circle cx="5.6" cy="6.1" r="1.2" />
      <path d="m2.2 12 3.8-3.5 2.6 2.1 2.2-1.8 3 2.6" />
    </Glyph>
  ),
  music: () => (
    <Glyph>
      <path d="M6 12V3.4l7-1.4v8.6" />
      <circle cx="4.3" cy="12" r="1.7" />
      <circle cx="11.3" cy="10.6" r="1.7" />
    </Glyph>
  ),
  videos: () => (
    <Glyph>
      <rect x="1.8" y="3" width="12.4" height="10" rx="1.2" />
      <path d="m6.6 5.8 3.6 2.2-3.6 2.2z" />
    </Glyph>
  ),
  onedrive: cloud,
  onedrive_business: cloud,
  folder: () => (
    <Glyph>
      <path d="M1.8 12.4V3.6a1 1 0 0 1 1-1h3.1l1.4 1.6h5.9a1 1 0 0 1 1 1v7.2a1 1 0 0 1-1 1H2.8a1 1 0 0 1-1-1z" />
    </Glyph>
  ),
};

function PinGlyph() {
  return (
    <svg
      width="12"
      height="12"
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.4"
      strokeLinejoin="round"
      strokeLinecap="round"
      aria-hidden="true"
      focusable="false"
    >
      <path d="M9.7 1.8 14.2 6.3 12 7.2 9.5 9.7l.5 3-1.2 1.2-6.7-6.7 1.2-1.2 3 .5 2.5-2.5z" />
      <path d="M5.2 10.8 1.8 14.2" />
    </svg>
  );
}

function Chevron() {
  return (
    <svg
      width="10"
      height="10"
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      focusable="false"
    >
      <path d="m4 6 4 4 4-4" />
    </svg>
  );
}

/** Shell icons by pathKey, for every render (null: Windows gave none). */
const iconCache = new Map<string, string | null>();

/** The folder's Windows icon (ops.thumbnail's icon fallback at this size), once it is read. */
function useShellIcon(path: string | null): string | null {
  const key = path ? pathKey(path) : "";
  const [, bump] = useState(0);
  useEffect(() => {
    if (!path || iconCache.has(key)) return;
    let gone = false;
    const px = Math.max(16, Math.min(48, Math.round(18 * (window.devicePixelRatio || 1))));
    ops.thumbnail(path, px).then(
      (url) => {
        iconCache.set(key, url);
        if (!gone) bump((n) => n + 1);
      },
      // Busy ("나중에 다시"), or no backend: the glyph stays, and the next showing asks again.
      () => {},
    );
    return () => {
      gone = true;
    };
  }, [path, key]);
  return path ? (iconCache.get(key) ?? null) : null;
}

function FavIcon({ path, kind }: { path: string | null; kind: PlaceKind | "folder" }) {
  const url = useShellIcon(path);
  if (url) return <img className="fav-icon" src={url} alt="" draggable={false} />;
  return <span className={`fav-icon fav-glyph k-${kind}`}>{(GLYPHS[kind] ?? GLYPHS.folder)()}</span>;
}

// ---- a foldable section -----------------------------------------------------------------

export interface SidebarSectionProps {
  id: SectionId;
  title: string;
  places: PlacesState;
  /** Beside the title (the ⋯ menu). */
  actions?: React.ReactNode;
  /** Folders dropped on the header are pinned (즐겨찾기). */
  pinDrop?: boolean;
  children: React.ReactNode;
}

/** A sidebar section whose header folds it, as Explorer's navigation pane (←/→ on the header too). */
export function SidebarSection(p: SidebarSectionProps) {
  const folded = p.places.isCollapsed(p.id);
  const uid = useId();
  const head = `${uid}-head`;
  const body = `${uid}-body`;
  return (
    <div className={`side-sec${folded ? " folded" : ""}`} role="group" aria-labelledby={head}>
      <div className="side-head" data-fav-drop={p.pinDrop ? "end" : undefined}>
        <button
          type="button"
          id={head}
          className="side-toggle"
          data-side-nav=""
          aria-expanded={!folded}
          aria-controls={body}
          title={folded ? `${p.title} 펼치기` : `${p.title} 접기`}
          onClick={() => p.places.toggle(p.id)}
          onKeyDown={(ev) => {
            if (ev.ctrlKey || ev.altKey || ev.metaKey || ev.shiftKey) return;
            if (ev.key !== "ArrowLeft" && ev.key !== "ArrowRight") return;
            ev.preventDefault();
            ev.stopPropagation();
            if ((ev.key === "ArrowLeft") !== folded) p.places.toggle(p.id);
          }}
        >
          <span className="side-chev" aria-hidden="true">
            <Chevron />
          </span>
          {p.title}
        </button>
        {p.actions}
      </div>
      <div id={body} className="side-body" hidden={folded}>
        {p.children}
      </div>
    </div>
  );
}

// ---- 즐겨찾기 ------------------------------------------------------------------------------

interface Item {
  key: string;
  path: string;
  label: string;
  kind: PlaceKind | "folder";
  /** Its index in PlacesConfig.pinned; none for a known place. */
  index?: number;
  missing: boolean;
}

export interface FavoritesProps {
  places: PlacesState;
  /** The folder the tab shown is in. */
  path: string;
  onGo: (path: string) => void;
  onOpenTab: (path: string, show: boolean) => void;
  toast: (text: string, kind?: "info" | "error") => void;
  onHelp: (topic?: string) => void;
}

/** The sidebar's 즐겨찾기: known places, OneDrive, then the pinned folders. */
export function FavoritesSection({ places, path, onGo, onOpenTab, toast, onHelp }: FavoritesProps) {
  const here = pathKey(path);
  const knownKeys = new Set(places.shown.map((k) => pathKey(k.path)));
  const known: Item[] = places.shown.map((k) => ({
    key: pathKey(k.path),
    path: k.path,
    label: k.label,
    kind: k.kind,
    missing: false,
  }));
  // As Explorer draws them: 홈 and OneDrive, a line, then the user's folders and the pinned ones.
  const top = known.filter((k) => k.kind !== "folder" && TOP_KINDS.has(k.kind));
  const rest = known.filter((k) => k.kind === "folder" || !TOP_KINDS.has(k.kind));
  // A pinned folder that is a known place too shows once, as the known place.
  const pins: Item[] = places.config.pinned.flatMap((p, index) => {
    const key = pathKey(p.path);
    return knownKeys.has(key)
      ? []
      : [{ key, path: p.path, label: p.label ?? folderTitle(p.path), kind: "folder" as const, index, missing: places.missing.has(key) }];
  });

  const [menu, setMenu] = useState<{ item: Item; x: number; y: number; keyboard: boolean } | null>(null);
  const menuBox = useRef<HTMLDivElement>(null);
  const [renaming, setRenaming] = useState<string | null>(null);
  const renamingRef = useRef<string | null>(null);
  // A pinned folder dragged to another place in the list (from: its index; to: where, or null off the list).
  const [reorder, setReorder] = useState<{ from: number; to: number | null } | null>(null);
  const list = useRef<HTMLUListElement>(null);
  // The row (pathKey) the keyboard focus goes back to after it moved, a rename or a menu.
  const refocus = useRef<string | null>(null);
  // A press that dragged: the click that ends it opens nothing.
  const dragged = useRef(false);

  useLayoutEffect(() => {
    const key = refocus.current;
    if (key === null) return;
    refocus.current = null;
    list.current?.querySelector<HTMLElement>(`[data-fav-key="${CSS.escape(key)}"]`)?.focus();
  });

  const open = async (it: Item) => {
    if (dragged.current) return;
    if (!it.missing) return onGo(it.path);
    if (await places.recheck(it.path)) onGo(it.path);
    else toast(`폴더를 찾을 수 없어요: ${it.path}. 지워졌거나 옮겨졌다면 고정 해제해 주세요`, "error");
  };

  const copyPath = (p: string) =>
    navigator.clipboard.writeText(p).then(
      () => toast("경로를 복사했어요"),
      () => toast("경로를 복사하지 못했어요", "error"),
    );

  /** Moves a pinned folder one row up (-1) or down (1), keeping the keyboard on it. */
  const moveBy = (it: Item, d: -1 | 1) => {
    const at = pins.findIndex((x) => x.key === it.key);
    const next = pins[at + d];
    if (at < 0 || !next || it.index === undefined || next.index === undefined) return;
    places.move(it.index, d < 0 ? next.index : next.index + 1);
    refocus.current = it.key;
  };

  const startRename = (it: Item) => {
    renamingRef.current = it.key;
    setRenaming(it.key);
  };
  const finishRename = (it: Item, value: string | null) => {
    if (renamingRef.current !== it.key) return;
    renamingRef.current = null;
    setRenaming(null);
    refocus.current = it.key;
    if (value === null) return;
    const v = cleanLabel(value);
    places.rename(it.path, v && v !== folderTitle(it.path) ? v : null);
  };

  // ---- a pinned folder dragged to another place (pointer only: the drag out is Windows') ----
  const startReorder = (it: Item) => {
    if (it.index === undefined || renamingRef.current !== null) return;
    const from = it.index;
    let to: number | null = null;
    setReorder({ from, to });
    document.documentElement.classList.add("fav-reordering");
    const at = (x: number, y: number): number | null => {
      const ul = list.current;
      if (!ul) return null;
      const box = ul.getBoundingClientRect();
      // Off the list (to the side, or well above or below it): let go there, nothing moves.
      if (x < box.left - 16 || x > box.right + 16 || y < box.top - 24 || y > box.bottom + 24) return null;
      const ins = insertionAt(ul, y);
      return ins.row ? ins.at : null;
    };
    const move = (e: MouseEvent) => {
      if ((e.buttons & 1) === 0) return finish(false);
      const n = at(e.clientX, e.clientY);
      if (n !== to) {
        to = n;
        setReorder({ from, to });
      }
    };
    const up = (e: MouseEvent) => {
      if (e.button === 0) finish(true);
    };
    const key = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.preventDefault();
      e.stopPropagation();
      finish(false);
    };
    const blur = () => finish(false);
    function finish(apply: boolean) {
      window.removeEventListener("mousemove", move, true);
      window.removeEventListener("mouseup", up, true);
      window.removeEventListener("keydown", key, true);
      window.removeEventListener("blur", blur);
      document.documentElement.classList.remove("fav-reordering");
      setReorder(null);
      // The click that comes with this release is not an "open".
      dragged.current = true;
      setTimeout(() => (dragged.current = false), 0);
      if (apply && to !== null) places.move(from, to);
    }
    window.addEventListener("mousemove", move, true);
    window.addEventListener("mouseup", up, true);
    window.addEventListener("keydown", key, true);
    window.addEventListener("blur", blur);
  };

  // ---- the row's menu ----
  const openMenu = (it: Item, x: number, y: number, keyboard: boolean) => {
    // Other menus (a tab's, a drive's ▾) close as on any click elsewhere.
    window.dispatchEvent(new Event("click"));
    setMenu({ item: it, x, y, keyboard });
  };
  const closeMenu = (back: boolean) => {
    if (back && menu) refocus.current = menu.item.key;
    setMenu(null);
  };
  // Kept inside the window, the focus on its first item.
  useLayoutEffect(() => {
    const el = menuBox.current;
    if (!menu || !el) return;
    const { width, height } = el.getBoundingClientRect();
    el.style.left = `${Math.max(4, Math.min(menu.x, window.innerWidth - width - 4))}px`;
    el.style.top = `${menu.y + height <= window.innerHeight - 4 ? menu.y : Math.max(4, menu.y - height)}px`;
    el.querySelector<HTMLElement>('[role="menuitem"]:not(:disabled)')?.focus();
  }, [menu]);
  // A press anywhere else, or leaving the window, closes it.
  useEffect(() => {
    if (!menu) return;
    const down = (ev: PointerEvent) => {
      if (!menuBox.current?.contains(ev.target as Node)) setMenu(null);
    };
    const leave = () => setMenu(null);
    window.addEventListener("pointerdown", down, true);
    window.addEventListener("blur", leave);
    return () => {
      window.removeEventListener("pointerdown", down, true);
      window.removeEventListener("blur", leave);
    };
  }, [menu]);
  const onMenuKey = (ev: React.KeyboardEvent<HTMLDivElement>) => {
    const items = Array.from(ev.currentTarget.querySelectorAll<HTMLElement>('[role="menuitem"]:not(:disabled)'));
    const at = items.indexOf(document.activeElement as HTMLElement);
    const go = (i: number) => items.length && items[(i + items.length) % items.length].focus();
    switch (ev.key) {
      case "ArrowDown":
        go(at + 1);
        break;
      case "ArrowUp":
        go(at < 0 ? items.length - 1 : at - 1);
        break;
      case "Home":
        go(0);
        break;
      case "End":
        go(items.length - 1);
        break;
      case "Escape":
      case "Tab":
        closeMenu(true);
        break;
      default:
        // Shortcuts, F1 and F5 go on; Enter and Space click the item; the list's keys stop here.
        if (ev.ctrlKey || ev.altKey || ev.metaKey || ev.key === "F1" || ev.key === "F5") return;
        if (ev.key === "F10" && ev.shiftKey) ev.preventDefault();
        ev.stopPropagation();
        return;
    }
    ev.preventDefault();
    ev.stopPropagation();
  };
  /** A menu item's action: the menu closes first (the keyboard goes back to the row when it was used). */
  const act = (fn: () => void, back = true) => (ev: React.MouseEvent) => {
    closeMenu(back && ev.detail === 0);
    fn();
  };

  const onRowKey = (ev: React.KeyboardEvent<HTMLButtonElement>, it: Item) => {
    const plain = !ev.ctrlKey && !ev.altKey && !ev.metaKey;
    // Alt+↑ / Alt+↓: the pinned folder moves (the keyboard's drag).
    if (ev.altKey && !ev.ctrlKey && !ev.metaKey && !ev.shiftKey && (ev.key === "ArrowUp" || ev.key === "ArrowDown")) {
      ev.preventDefault();
      ev.stopPropagation();
      moveBy(it, ev.key === "ArrowUp" ? -1 : 1);
      return;
    }
    // The menu key and Shift+F10: this row's menu, not the Windows menu of the list's selection.
    if (isMenuKey(ev)) {
      ev.preventDefault();
      ev.stopPropagation();
      const r = ev.currentTarget.getBoundingClientRect();
      openMenu(it, r.left + 24, r.bottom, true);
      return;
    }
    if (plain && !ev.shiftKey && ev.key === "F2" && it.index !== undefined) {
      ev.preventDefault();
      ev.stopPropagation();
      startRename(it);
      return;
    }
    // Delete and F2 here are not for the list of the tab (its trash, its rename).
    if (plain && (ev.key === "Delete" || ev.key === "F2")) ev.stopPropagation();
  };

  // Where a dragged pinned folder would land (no line where it would not move).
  const line = reorder && reorder.to !== null && reorder.to !== reorder.from && reorder.to !== reorder.from + 1 ? reorder.to : null;
  const lastIndex = pins.length ? pins[pins.length - 1].index! : -1;

  const row = (it: Item) => {
    const active = !it.missing && it.key === here;
    const cls = [
      "fav-li",
      it.missing ? "missing" : "",
      reorder?.from === it.index && it.index !== undefined ? "dragged" : "",
      line !== null && it.index === line ? "ins-before" : "",
      line !== null && it.index === lastIndex && line > lastIndex ? "ins-after" : "",
    ]
      .filter(Boolean)
      .join(" ");
    const pinned = it.index !== undefined;
    return (
      <li key={`${pinned ? "p" : "k"}:${it.key}`} className={cls} data-fav-index={it.index}>
        {renaming === it.key ? (
          <input
            className="fav-rename"
            defaultValue={it.label}
            maxLength={MAX_LABEL}
            autoFocus
            spellCheck={false}
            aria-label={`${folderTitle(it.path)}의 즐겨찾기 이름 (비우면 폴더 이름)`}
            onFocus={(e) => e.currentTarget.select()}
            onKeyDown={(e) => {
              e.stopPropagation();
              // Enter that ends a 한글 composition only ends it.
              if ((e.key !== "Enter" && e.key !== "Escape") || e.nativeEvent.isComposing) return;
              // Kept from the row the focus goes back to: its keypress would open the folder too.
              e.preventDefault();
              finishRename(it, e.key === "Enter" ? e.currentTarget.value : null);
            }}
            onBlur={(e) => finishRename(it, e.currentTarget.value)}
          />
        ) : (
          <button
            type="button"
            className={`place fav-item${active ? " active" : ""}${it.missing ? " missing" : ""}`}
            data-side-nav=""
            data-fav-key={it.key}
            // Files dropped on the row's middle go into the folder (App's drop), as on tabs and drives.
            data-path={it.missing ? undefined : it.path}
            aria-current={active ? "page" : undefined}
            aria-label={it.missing ? `${it.label}, 폴더를 찾을 수 없어요` : it.label}
            title={
              it.missing
                ? `${shown(it.path)}\n폴더를 찾을 수 없어요. 지워졌거나 옮겨졌을 수 있어요`
                : `${shown(it.path)}\n가운데 클릭: 새 탭${pinned ? " · 끌거나 Alt+↑/↓로 순서 바꾸기" : ""}`
            }
            onClick={() => open(it)}
            onMouseDown={(ev) => {
              if (ev.button === 1) {
                // Not the browser's autoscroll.
                ev.preventDefault();
                if (!it.missing) onOpenTab(it.path, false);
              } else if (ev.button === 0 && pinned && !ev.shiftKey && !ev.ctrlKey) {
                watchPress(ev, () => startReorder(it));
              }
            }}
            onContextMenu={(ev) => {
              ev.preventDefault();
              ev.stopPropagation();
              openMenu(it, ev.clientX, ev.clientY, false);
            }}
            onKeyDown={(ev) => onRowKey(ev, it)}
          >
            <FavIcon path={it.missing ? null : it.path} kind={it.kind} />
            <span className="fav-text">
              <span className="fav-label">{shown(it.label)}</span>
              {it.missing && <span className="fav-note">폴더를 찾을 수 없어요</span>}
            </span>
            {pinned && !it.missing && (
              <span className="fav-pin" aria-hidden="true">
                <PinGlyph />
              </span>
            )}
          </button>
        )}
        {it.missing && renaming !== it.key && (
          <button
            type="button"
            className="fav-unpin"
            aria-label={`${it.label} 고정 해제`}
            onClick={() => places.unpin(it.path)}
            {...tip("unpin")}
          >
            고정 해제
          </button>
        )}
      </li>
    );
  };

  const m = menu?.item;
  const mAt = m ? pins.findIndex((x) => x.key === m.key) : -1;
  const hiddenCount = places.hiddenPlaces.length;

  return (
    <SidebarSection
      id="quick"
      title="즐겨찾기"
      places={places}
      pinDrop
      actions={
        <MenuButton
          label={Icon.more()}
          className="side-more"
          name="즐겨찾기 메뉴"
          active
          attrs={{ ...tip("places"), "aria-label": "즐겨찾기 메뉴" }}
        >
          <button role="menuitem" onClick={() => places.importExplorer()} {...tip("importPins")}>
            <MenuRow label="탐색기 즐겨찾기 가져오기" />
          </button>
          <button
            role="menuitem"
            disabled={!path || places.isPinned(path)}
            onClick={() => places.pin([path])}
            {...tip("pin", path && places.isPinned(path) ? "지금 폴더는 이미 즐겨찾기에 있어요" : "지금 탭의 폴더를 즐겨찾기에 고정해요")}
          >
            <MenuRow label="지금 폴더 고정" />
          </button>
          <button
            role="menuitem"
            disabled={!hiddenCount}
            onClick={places.showHidden}
            {...tip(
              "hidePlace",
              hiddenCount
                ? `숨긴 장소를 다시 보여요: ${places.hiddenPlaces.map((h) => h.label).join(", ")}`
                : "숨긴 장소가 없어요",
            )}
          >
            <MenuRow label={`숨긴 장소 다시 보이기${hiddenCount ? ` (${hiddenCount})` : ""}`} />
          </button>
          <hr />
          <button role="menuitem" onClick={() => onHelp("places")} {...tip("help", "즐겨찾기를 쓰는 법을 도움말에서 봐요")}>
            <MenuRow label="즐겨찾기 설명…" />
          </button>
        </MenuButton>
      }
    >
      <ul ref={list} className="fav-list" role="list" aria-label="즐겨찾기" data-fav-list="">
        {top.map(row)}
        {top.length > 0 && rest.length + pins.length > 0 && <li className="fav-sep" role="separator" aria-hidden="true" />}
        {rest.map(row)}
        {pins.map(row)}
      </ul>
      {/* Below the rows: folders dropped here are pinned at the end. */}
      <div className={`fav-pad${pins.length ? "" : " empty"}`} data-fav-drop="end">
        {places.ready && !pins.length && "폴더를 여기로 끌어 놓거나, 폴더 메뉴의 \"즐겨찾기에 고정\"을 고르면 여기에 고정돼요"}
      </div>
      {places.offer && (
        <div className="fav-offer" role="status" title={shown(places.offer.join("\n"))}>
          <span>탐색기 즐겨찾기에 고정한 폴더 {places.offer.length}개를 여기로 가져올까요?</span>
          <span className="fav-offer-actions">
            <button type="button" className="primary" onClick={places.takeOffer} {...tip("importPins")}>
              가져오기
            </button>
            <button type="button" onClick={places.dismissOffer} title="가져오지 않아요. ⋯ 메뉴에서 언제든 가져올 수 있어요">
              괜찮아요
            </button>
          </span>
        </div>
      )}

      {menu && m && (
        <div
          ref={menuBox}
          className="menu cmd-menu fav-menu"
          role="menu"
          aria-label={`${m.label} 메뉴`}
          style={{ left: menu.x, top: menu.y }}
          onKeyDown={onMenuKey}
          onContextMenu={(ev) => ev.preventDefault()}
        >
          {!m.missing && (
            <>
              <button role="menuitem" onClick={act(() => onOpenTab(m.path, true), false)} {...tip("openTab")}>
                새 탭에서 열기
              </button>
              <hr />
            </>
          )}
          {m.index !== undefined ? (
            <>
              <button role="menuitem" onClick={act(() => places.unpin(m.path), false)} {...tip("unpin")}>
                고정 해제
              </button>
              <button role="menuitem" onClick={act(() => startRename(m), false)} {...tip("renamePin")}>
                이름 바꾸기 (F2)
              </button>
              <button role="menuitem" disabled={mAt <= 0} onClick={act(() => moveBy(m, -1))} {...tip("movePin")}>
                위로 옮기기 (Alt+↑)
              </button>
              <button
                role="menuitem"
                disabled={mAt < 0 || mAt >= pins.length - 1}
                onClick={act(() => moveBy(m, 1))}
                {...tip("movePin")}
              >
                아래로 옮기기 (Alt+↓)
              </button>
            </>
          ) : (
            <button role="menuitem" onClick={act(() => places.unpin(m.path), false)} {...tip("hidePlace")}>
              숨기기
            </button>
          )}
          <hr />
          {!m.missing && (
            <button
              role="menuitem"
              onClick={act(() => api.reveal(m.path).catch((err) => toast(String(err), "error")), false)}
              {...tip("reveal")}
            >
              탐색기에서 보기
            </button>
          )}
          <button role="menuitem" onClick={act(() => copyPath(m.path))} {...tip("copyPath")}>
            경로 복사
          </button>
        </div>
      )}
    </SidebarSection>
  );
}
