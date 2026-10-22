import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { api, type Entry, type Listing } from "./api";
import { RenameBox } from "./FileTable";
import { formatDate, formatSize, isSameOrInside, pathKey } from "./format";
import { watchPress } from "./shell";

const ROW = 28;
/** Children drawn per folder; the rest is summed up in one row. */
const CAP = 200;
const INDENT = 18;
const COLUMNS = "minmax(260px, 3fr) minmax(150px, 1.4fr) 90px 140px";
const FILES_KEY = "fine-file.tree-files";

const cmp = new Intl.Collator("ko", { numeric: true }).compare;
const bySize = (a: Entry, b: Entry) => b.size - a.size || cmp(a.name, b.name);

/** A folder's children, sorted once when they arrive (big folders hold 100k entries). */
interface Node {
  path: string;
  all: Entry[];
  dirs: Entry[];
  /** The folder is in the index, so its subfolder sizes are known. */
  sized: boolean;
  /** A listing has arrived; a reload keeps showing the old one until the new one lands. */
  loaded: boolean;
  error: string | null;
}

function makeNode(path: string, l: Listing): Node {
  const all = [...l.entries].sort(bySize);
  return { path, all, dirs: all.filter((e) => e.is_dir), sized: l.indexed, loaded: true, error: null };
}

type Row =
  | { kind: "entry"; key: string; parent: string; depth: number; e: Entry; sized: boolean; open: boolean }
  | { kind: "more"; key: string; depth: number; text: string }
  | { kind: "note"; key: string; depth: number; text: string; error: boolean };

export interface TreeRefresh {
  /** Bumped for every refresh, so the same folders can be refreshed twice in a row. */
  n: number;
  /** Folders (pathKey) whose children changed; null for every folder. */
  dirs: string[] | null;
  /** Folders (pathKey) removed or renamed away: what is cached below them is dropped too. */
  gone: string[];
}

/** Whether `p` is `folder` or inside it. Exact paths as listed, so "Build" and "build" differ. */
function within(p: string, folder: string): boolean {
  return p === folder || p.startsWith(folder.endsWith("\\") ? folder : `${folder}\\`);
}

interface Props {
  root: string;
  /** The root folder's listing, loaded and reloaded by App; null while it loads. */
  listing: Listing | null;
  error: string | null;
  refresh: TreeRefresh;
  /** False while search results cover the tree: it keeps its state but leaves the keyboard alone. */
  active: boolean;
  onNavigate: (path: string) => void;
  onOpen: (e: Entry) => void;
  onContextMenu: (ev: React.MouseEvent, e: Entry) => void;
  /** Middle click on a folder (open it in a new tab). */
  onMiddleClick?: (e: Entry) => void;
  /** The row the keyboard is on (null for none), for the preview and copy/cut. */
  onFocusEntry?: (e: Entry | null, sized: boolean) => void;
  /** A root row whose name is open for editing (a folder just made), and where the name goes. */
  renaming?: string | null;
  onRenameDone?: (e: Entry, name: string | null) => void;
  /** The pointer moved with the button held on a row: drag that row's item out (see Explorer). */
  onDragOut?: (e: Entry) => void;
}

function savedShowFiles(): boolean {
  try {
    return localStorage.getItem(FILES_KEY) === "1";
  } catch {
    return false;
  }
}

export function TreeView(p: Props) {
  const scroller = useRef<HTMLDivElement>(null);
  const [scrollTop, setScrollTop] = useState(0);
  const [height, setHeight] = useState(600);
  const [showFiles, setShowFiles] = useState(savedShowFiles);
  // Keyed by the exact path as listed, not pathKey: a case-sensitive folder (WSL, git) can hold
  // "Build" and "build" side by side. Only live changes are matched case-insensitively, as the
  // index matches names. A collapsed folder keeps its children and the open state of its
  // subfolders, so opening it again brings the same view back.
  const [nodes, setNodes] = useState<Map<string, Node>>(() => new Map());
  const [open, setOpen] = useState<Set<string>>(() => new Set());
  const [focus, setFocus] = useState<string | null>(null);

  // Newest request per folder: a listing that lands after a newer request (or after the folder
  // was dropped from the cache) is ignored.
  const reqs = useRef(new Map<string, number>());
  // Folders being listed, and whether another listing was asked for meanwhile. Live changes that
  // keep coming while a big folder lists get one more listing after it, instead of a new one
  // every 500 ms that supersedes each reply before it lands.
  const busy = useRef(new Map<string, boolean>());
  const alive = useRef(true);
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);

  useLayoutEffect(() => {
    const el = scroller.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setHeight(el.clientHeight));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  // Hidden under search results, the scroller may lose its position without a scroll event.
  useLayoutEffect(() => {
    if (p.active && scroller.current) setScrollTop(scroller.current.scrollTop);
  }, [p.active]);

  const load = useCallback((path: string) => {
    if (busy.current.has(path)) {
      busy.current.set(path, true);
      return;
    }
    busy.current.set(path, false);
    const req = (reqs.current.get(path) ?? 0) + 1;
    reqs.current.set(path, req);
    setNodes((m) =>
      m.has(path) ? m : new Map(m).set(path, { path, all: [], dirs: [], sized: false, loaded: false, error: null }),
    );
    const done = (node: Node) => {
      if (alive.current && reqs.current.get(path) === req) setNodes((m) => new Map(m).set(path, node));
      const again = busy.current.get(path);
      busy.current.delete(path);
      if (again && alive.current) load(path);
    };
    api.list(path).then(
      (l) => done(makeNode(path, l)),
      (err) => done({ path, all: [], dirs: [], sized: false, loaded: false, error: String(err) }),
    );
  }, []);

  const rootNode = useMemo(() => (p.listing ? makeNode(p.root, p.listing) : null), [p.root, p.listing]);
  const renaming = p.renaming ?? null;

  // The visible rows, flattened: open folders list their children right below themselves.
  const rows = useMemo(() => {
    const out: Row[] = [];
    const walk = (node: Node, parent: string, depth: number) => {
      const list = showFiles ? node.all : node.dirs;
      const n = Math.min(list.length, CAP);
      const add = (e: Entry) => {
        const key = e.path;
        const isOpen = e.is_dir && open.has(key);
        out.push({ kind: "entry", key, parent, depth, e, sized: node.sized, open: isOpen });
        if (!isOpen) return;
        const child = nodes.get(key);
        const note = (text: string, error = false) =>
          out.push({ kind: "note", key: `${key}\n…`, depth: depth + 1, text, error });
        if (child?.error) note(child.error, true);
        else if (!child?.loaded) note("불러오는 중…");
        else if ((showFiles ? child.all : child.dirs).length === 0)
          note(showFiles || !child.all.length ? "빈 폴더예요" : `하위 폴더 없음 · 파일 ${child.all.length.toLocaleString()}개`);
        else walk(child, key, depth + 1);
      };
      for (let i = 0; i < n; i++) add(list[i]);
      // A new folder being named sorts last (0 bytes), maybe past the cap: it shows anyway.
      const late = renaming === null ? -1 : list.findIndex((e, i) => i >= n && e.path === renaming);
      if (late >= 0) add(list[late]);
      if (list.length > n + (late >= 0 ? 1 : 0)) {
        const rest = list.filter((_, i) => i >= n && i !== late);
        // Folder sizes in an unindexed folder are unknown, so their sum would be too small.
        const sized = node.sized || rest.every((e) => !e.is_dir);
        const size = rest.reduce((s, e) => s + e.size, 0);
        const text = `그 외 ${rest.length.toLocaleString()}개${sized ? ` (${formatSize(size)})` : ""}`;
        out.push({ kind: "more", key: `${parent}\n+`, depth, text });
      }
    };
    if (rootNode) walk(rootNode, p.root, 0);
    return out;
  }, [rootNode, p.root, nodes, open, showFiles, renaming]);

  // Open folders whose children are not loaded yet (just opened, or dropped by a refresh while
  // hidden under a collapsed parent) load now.
  useEffect(() => {
    for (const r of rows) if (r.kind === "entry" && r.open && !nodes.has(r.key)) load(r.e.path);
  }, [rows, nodes, load]);

  // Live changes and F5: open folders on screen reload in place, cached ones out of sight are
  // dropped and load again when they come into view.
  const seen = useRef(p.refresh.n);
  useEffect(() => {
    if (p.refresh.n === seen.current) return;
    seen.current = p.refresh.n;
    const { dirs, gone } = p.refresh;
    // By pathKey, as the index matches names. A change anywhere below a folder changes the size
    // of the child it is in; a folder that went away takes what is cached below it along.
    const hit = (k: string) =>
      dirs === null || dirs.some((d) => isSameOrInside(d, k)) || gone.some((g) => isSameOrInside(k, g));
    const shown = new Set<string>();
    for (const r of rows) if (r.kind === "entry" && r.open) shown.add(r.key);
    const drop: string[] = [];
    for (const [key, node] of nodes) {
      if (!hit(pathKey(node.path))) continue;
      if (shown.has(key)) load(node.path);
      else {
        drop.push(key);
        reqs.current.set(key, (reqs.current.get(key) ?? 0) + 1);
      }
    }
    if (drop.length)
      setNodes((m) => {
        const n = new Map(m);
        for (const k of drop) n.delete(k);
        return n;
      });
  }, [p.refresh]);

  const focusAt = useMemo(
    () => (focus === null ? -1 : rows.findIndex((r) => r.kind === "entry" && r.key === focus)),
    [rows, focus],
  );
  const renameAt = useMemo(
    () => (renaming === null ? -1 : rows.findIndex((r) => r.kind === "entry" && r.key === renaming)),
    [rows, renaming],
  );

  // The focused entry goes up with every change, a fresh one after a reload included (sizes).
  // Read through a ref: a new callback each render must not report again.
  const onFocusEntry = useRef(p.onFocusEntry);
  useLayoutEffect(() => {
    onFocusEntry.current = p.onFocusEntry;
  });
  const focusRow = rows[focusAt];
  const focusEntry = focusRow?.kind === "entry" ? focusRow.e : null;
  const focusSized = focusRow?.kind === "entry" && focusRow.sized;
  useEffect(() => {
    onFocusEntry.current?.(focusEntry, focusSized);
    return () => onFocusEntry.current?.(null, false);
  }, [focusEntry, focusSized]);

  const toggle = (key: string, to?: boolean) => {
    const want = to ?? !open.has(key);
    if (want === open.has(key)) return;
    setOpen((s) => {
      const n = new Set(s);
      if (want) n.add(key);
      else n.delete(key);
      return n;
    });
    // Collapsing a folder that holds the focus moves the focus onto it, so it stays on screen.
    if (!want && focus !== null && focus !== key && within(focus, key)) setFocus(key);
  };

  const reveal = (i: number) => {
    const el = scroller.current;
    if (!el || i < 0) return;
    const top = i * ROW;
    if (top < el.scrollTop) el.scrollTop = top;
    else if (top + ROW > el.scrollTop + el.clientHeight) el.scrollTop = top + ROW - el.clientHeight;
  };

  // Only when the focus moves: a live update that adds rows above it must not scroll the view.
  useEffect(() => reveal(focusAt), [focus]);

  // A row whose name opens for editing takes the focus, which brings it into view.
  useEffect(() => {
    if (renaming !== null) setFocus(renaming);
  }, [renaming]);

  // The next entry row from `from` (notes and "그 외" rows are skipped).
  const step = (from: number, dir: 1 | -1) => {
    if (from < 0) return rows.findIndex((r) => r.kind === "entry");
    for (let i = from + dir; i >= 0 && i < rows.length; i += dir) if (rows[i].kind === "entry") return i;
    return from;
  };

  useEffect(() => {
    const onKey = (ev: KeyboardEvent) => {
      if (!p.active || ev.altKey || ev.ctrlKey || ev.metaKey) return;
      const t = ev.target as HTMLElement;
      const typing = t instanceof HTMLInputElement ? t.type !== "checkbox" : t.isContentEditable;
      if (typing) return;
      const onButton = t instanceof HTMLButtonElement;
      const r = rows[focusAt];
      const cur = r?.kind === "entry" ? r : undefined;
      const moveTo = (i: number) => {
        const row = rows[i];
        if (row?.kind === "entry") setFocus(row.key);
      };
      switch (ev.key) {
        case "ArrowDown":
        case "ArrowUp":
          ev.preventDefault();
          if (onButton) t.blur();
          moveTo(step(focusAt, ev.key === "ArrowDown" ? 1 : -1));
          break;
        case "Home":
          ev.preventDefault();
          moveTo(step(-1, 1));
          break;
        case "End":
          ev.preventDefault();
          moveTo(rows.length - 1 - [...rows].reverse().findIndex((x) => x.kind === "entry"));
          break;
        case "ArrowRight":
          if (!cur?.e.is_dir) return;
          ev.preventDefault();
          if (!cur.open) toggle(cur.key, true);
          // Already open: step onto its first child.
          else if (rows[focusAt + 1]?.kind === "entry" && rows[focusAt + 1].depth > cur.depth) moveTo(focusAt + 1);
          break;
        case "Enter":
          // Enter on a button is the button's own (see App).
          if (onButton || !cur) return;
          ev.preventDefault();
          if (!cur.e.is_dir) p.onOpen(cur.e);
          else if (!cur.open) toggle(cur.key, true);
          break;
        case "ArrowLeft":
          if (!cur) return;
          ev.preventDefault();
          if (cur.open) toggle(cur.key, false);
          else if (cur.depth > 0) setFocus(cur.parent);
          break;
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  const total = p.listing?.current?.size ?? 0;
  const first = Math.max(0, Math.floor(scrollTop / ROW) - 8);
  const last = Math.min(rows.length, Math.ceil((scrollTop + height) / ROW) + 8);
  const shown: number[] = [];
  for (let i = first; i < last; i++) shown.push(i);
  // The row being renamed stays mounted while scrolled away, so the typed name is not lost.
  if (renameAt >= 0 && (renameAt < first || renameAt >= last)) shown.push(renameAt);

  const emptyText = rootNode
    ? !showFiles && rootNode.all.length
      ? "하위 폴더가 없어요. '파일도 보기'를 켜면 파일이 보여요"
      : "빈 폴더예요"
    : p.error
      ? "폴더를 열 수 없어요"
      : "불러오는 중…";

  return (
    <div className="table tree">
      <div className="thead" style={{ gridTemplateColumns: COLUMNS }}>
        <div className="th">
          이름
          <label className="scope tree-files" title="폴더 안의 파일도 함께 보여요">
            <input
              type="checkbox"
              checked={showFiles}
              onChange={(e) => {
                const v = e.target.checked;
                setShowFiles(v);
                try {
                  localStorage.setItem(FILES_KEY, v ? "1" : "0");
                } catch {
                  /* storage blocked: the choice lasts until the app closes */
                }
              }}
            />
            파일도 보기
          </label>
        </div>
        <div className="th right">크기</div>
        <div className="th right">파일 수</div>
        <div className="th">수정한 날짜</div>
      </div>
      <div className="tbody" ref={scroller} onScroll={(e) => setScrollTop(e.currentTarget.scrollTop)}>
        {rows.length === 0 && <div className="empty">{emptyText}</div>}
        <div style={{ height: rows.length * ROW, position: "relative" }}>
          {shown.map((i) => {
            const r = rows[i];
            const style = { top: i * ROW, gridTemplateColumns: COLUMNS };
            const indent = { paddingLeft: r.depth * INDENT };
            if (r.kind !== "entry")
              return (
                <div key={r.key} className="tr note" style={style}>
                  <div className={`td name ${r.kind === "note" && r.error ? "danger" : "muted"}`} style={indent}>
                    <span className="twisty" />
                    <span className="label" title={r.text}>
                      {r.text}
                    </span>
                  </div>
                </div>
              );
            const e = r.e;
            const known = r.sized || !e.is_dir;
            const pct = total > 0 && known ? Math.min(100, (e.size / total) * 100) : 0;
            return (
              <div
                key={r.key}
                className={`tr ${i === focusAt ? "sel focus" : ""} ${e.hidden || e.system ? "dim" : ""}`}
                style={style}
                // Folder rows take dropped files (App's drop looks for data-path).
                data-path={e.is_dir ? e.path : undefined}
                onMouseDown={(ev) => {
                  if (ev.button === 1 && e.is_dir && p.onMiddleClick) {
                    // Not the browser's autoscroll.
                    ev.preventDefault();
                    p.onMiddleClick(e);
                  }
                  if (ev.button !== 0) return;
                  setFocus(r.key);
                  const drag = p.onDragOut;
                  if (drag) watchPress(ev, () => drag(e));
                }}
                onDoubleClick={() => (e.is_dir ? p.onNavigate(e.path) : p.onOpen(e))}
                onContextMenu={(ev) => {
                  setFocus(r.key);
                  p.onContextMenu(ev, e);
                }}
              >
                <div className="td name" title={e.path} style={indent}>
                  <span
                    className="twisty"
                    // Opening and closing is not a double-click on the row (that enters the folder).
                    onDoubleClick={(ev) => ev.stopPropagation()}
                    onClick={() => e.is_dir && toggle(r.key)}
                  >
                    {e.is_dir ? (r.open ? "▾" : "▸") : ""}
                  </span>
                  <span className={`icon ${e.is_dir ? "dir" : "file"} ${e.reparse ? "link" : ""}`} />
                  {renaming === e.path && p.onRenameDone ? (
                    <RenameBox entry={e} onDone={p.onRenameDone} />
                  ) : (
                    <span className="label">{e.name}</span>
                  )}
                </div>
                <div className="td size right">
                  <span className="bar" style={{ width: `${pct}%` }} />
                  <span className="num">{known ? formatSize(e.size) : "—"}</span>
                </div>
                <div className="td right muted">{e.is_dir && r.sized ? e.files.toLocaleString() : ""}</div>
                <div className="td muted">{formatDate(e.modified)}</div>
              </div>
            );
          })}
        </div>
      </div>
    </div>
  );
}
