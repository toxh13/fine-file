import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { ops, type Entry } from "./api";
import { pressItem } from "./FileTable";
import { formatSize, shown } from "./format";
import { focusTakenByConflict } from "./Transfers";
import "./components.css";

const TILE_W = 112;
const TILE_H = 128;
/** Space between the scroller's edge and the tiles. */
const PAD = 8;
/** Rows drawn above and below the visible ones, so a short scroll shows no blank tiles. */
const OVERSCAN = 2;
/** Shell thumbnail size asked for. */
const THUMB = 96;

export interface GridViewProps {
  entries: Entry[];
  /** Changes only when another folder or search is shown; the grid then scrolls back to the top. */
  resetKey: string;
  selected: Set<string>;
  focus: number;
  onSelect: (index: number, mode: "single" | "toggle" | "range") => void;
  onActivate: (entry: Entry) => void;
  onContextMenu: (e: React.MouseEvent, index: number) => void;
  renaming: string | null;
  onRenameDone: (entry: Entry, name: string | null) => void;
  /** Folder sizes are known (an indexed folder, or search results). */
  sized: boolean;
  emptyText: string;
  /** Tiles per row, called whenever it changes, so the arrow keys can move by rows (gridStep). */
  onColumns?: (columns: number) => void;
  /** The pointer moved with the button held on a tile: drag the items out (see Explorer). */
  onDragOut?: (entry: Entry) => void;
  /** Folder tiles take dropped files (they carry data-path, which App's drop looks for). */
  dropFolders?: boolean;
}

/**
 * Where an arrow key moves the focus in a grid `columns` tiles wide: Left and Right by one
 * tile, Up and Down by a row. Down onto a shorter last row lands on its last tile, like
 * Explorer. Null for any other key.
 */
export function gridStep(key: string, focus: number, count: number, columns: number): number | null {
  const step =
    key === "ArrowLeft" ? -1 : key === "ArrowRight" ? 1 : key === "ArrowUp" ? -columns : key === "ArrowDown" ? columns : 0;
  if (step === 0 || count === 0) return null;
  if (focus < 0 || focus >= count) return 0;
  const next = focus + step;
  if (next < 0) return focus;
  if (next >= count) return Math.floor((count - 1) / columns) > Math.floor(focus / columns) ? count - 1 : focus;
  return next;
}

// ---- thumbnails ---------------------------------------------------------------
// Shared by every grid of this session. Keyed by path and modified time, so a file that
// changes gets a new thumbnail. Shell thumbnails can be slow (videos, big photos, network
// drives), so only a few are asked for at once, and a tile scrolled away before its turn
// drops out of the queue.

/** Data URL, or null when the shell had none; `at` is when the answer came (Date.now()). */
const thumbs = new Map<string, { url: string | null; at: number }>();
/** About 20 KB each. */
const MAX_THUMBS = 1500;
const PARALLEL = 4;
/**
 * A null is not always for good: the backend also answers null for a provider that took too
 * long, while all its threads are busy, or when the file could not be read, and may have the
 * thumbnail later. After this long it is asked again: when the tile shows again, and once
 * while it stays on screen. A real "none" comes back from the backend's cache at once.
 */
const NULL_TTL = 45_000;

type Listener = (url: string | null) => void;
const pending = new Map<string, { path: string; listeners: Set<Listener>; started: boolean }>();
/** Keys of `pending` not started yet, in the order asked. */
const queue: string[] = [];
let running = 0;

/** Undefined when unknown, a null that is due again (NULL_TTL) included. */
function cachedThumb(key: string): string | null | undefined {
  const t = thumbs.get(key);
  if (t === undefined) return undefined;
  thumbs.delete(key);
  if (t.url === null && Date.now() - t.at >= NULL_TTL) return undefined;
  // Most recently used last, so the oldest ones are dropped first.
  thumbs.set(key, t);
  return t.url;
}

function remember(key: string, url: string | null) {
  thumbs.set(key, { url, at: Date.now() });
  while (thumbs.size > MAX_THUMBS) thumbs.delete(thumbs.keys().next().value!);
}

/** Asks for a thumbnail; returns a function that withdraws the request. */
function requestThumb(key: string, path: string, f: Listener): () => void {
  let p = pending.get(key);
  if (!p) {
    p = { path, listeners: new Set(), started: false };
    pending.set(key, p);
    queue.push(key);
  }
  p.listeners.add(f);
  pump();
  const req = p;
  return () => {
    req.listeners.delete(f);
    if (!req.started && req.listeners.size === 0 && pending.get(key) === req) pending.delete(key);
  };
}

function pump() {
  while (running < PARALLEL && queue.length > 0) {
    const key = queue.shift()!;
    const p = pending.get(key);
    // Withdrawn (scrolled away) before its turn.
    if (!p || p.started) continue;
    p.started = true;
    running++;
    ops
      .thumbnail(p.path, THUMB)
      .then(
        (url) => {
          remember(key, url);
          return url;
        },
        // A failed call tells nothing about the file: not remembered, so it is asked again.
        () => null,
      )
      .then((url) => {
        running--;
        if (pending.get(key) === p) pending.delete(key);
        p.listeners.forEach((f) => f(url));
        pump();
      });
  }
}

/** The tile's thumbnail: undefined while unknown, null when there is none. */
function useThumb(e: Entry, visible: boolean): string | null | undefined {
  const key = `${e.path}|${e.modified}`;
  const [thumb, setThumb] = useState<{ key: string; url: string | null } | null>(() => {
    const t = thumbs.get(key);
    return t === undefined ? null : { key, url: t.url };
  });
  // The key whose null was asked about once more while on screen (NULL_TTL).
  const [retried, setRetried] = useState<string | null>(null);
  useEffect(() => {
    const hit = cachedThumb(key);
    if (hit !== undefined) {
      setThumb((t) => (t?.key === key && t.url === hit ? t : { key, url: hit }));
      return;
    }
    if (!visible) return;
    // A tile that only flies past while scrolling does not ask at all.
    let withdraw = () => {};
    const t = setTimeout(() => {
      withdraw = requestThumb(key, e.path, (url) => setThumb({ key, url }));
    }, 60);
    return () => {
      clearTimeout(t);
      withdraw();
    };
  }, [key, visible, retried === key]);
  const url = thumb?.key === key ? thumb.url : undefined;
  useEffect(() => {
    if (url !== null || !visible || retried === key) return;
    const t = setTimeout(() => setRetried(key), NULL_TTL);
    return () => clearTimeout(t);
  }, [url, visible, key, retried]);
  return url;
}

// ---- grid ---------------------------------------------------------------------

export function GridView(p: GridViewProps) {
  const scroller = useRef<HTMLDivElement>(null);
  const [scrollTop, setScrollTop] = useState(0);
  const [size, setSize] = useState({ width: 800, height: 600 });
  const renameAt = useMemo(
    () => (p.renaming === null ? -1 : p.entries.findIndex((e) => e.path === p.renaming)),
    [p.entries, p.renaming],
  );

  useLayoutEffect(() => {
    const el = scroller.current;
    if (!el) return;
    const measure = () => setSize({ width: el.clientWidth, height: el.clientHeight });
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const columns = Math.max(1, Math.floor((size.width - PAD * 2) / TILE_W));
  const rows = Math.ceil(p.entries.length / columns);

  const onColumns = p.onColumns;
  useEffect(() => onColumns?.(columns), [columns, onColumns]);

  const reveal = (i: number) => {
    const el = scroller.current;
    if (!el || i < 0) return;
    const top = PAD + Math.floor(i / columns) * TILE_H;
    if (top < el.scrollTop) el.scrollTop = Math.max(0, top - PAD);
    else if (top + TILE_H > el.scrollTop + el.clientHeight) el.scrollTop = top + TILE_H + PAD - el.clientHeight;
  };

  // The path last scrolled to. Another folder or search starts over.
  const revealed = useRef<string | null>(null);

  // Back to the top only for another folder or search, not on every reload or re-sort.
  useEffect(() => {
    revealed.current = null;
    if (scroller.current) scroller.current.scrollTop = 0;
  }, [p.resetKey]);

  // Keep the focused tile visible when the focus moves to another item, not when a live
  // reload only moves the focused item to another place (as in FileTable).
  const focusPath = p.entries[p.focus]?.path ?? null;
  useEffect(() => {
    if (focusPath === null || focusPath === revealed.current) return;
    revealed.current = focusPath;
    reveal(p.focus);
  }, [focusPath]);

  // F2 on a selected tile that is scrolled away: bring it into view.
  useEffect(() => reveal(renameAt), [p.renaming]);

  const firstSeen = Math.max(0, Math.floor((scrollTop - PAD) / TILE_H));
  const lastSeen = Math.min(rows - 1, Math.floor((scrollTop + size.height - PAD) / TILE_H));
  const first = Math.max(0, firstSeen - OVERSCAN) * columns;
  const last = Math.min(p.entries.length, (lastSeen + 1 + OVERSCAN) * columns);
  const drawn: number[] = [];
  for (let i = first; i < last; i++) drawn.push(i);
  // The tile being renamed stays mounted while scrolled away, so the typed name is not lost.
  if (renameAt >= 0 && (renameAt < first || renameAt >= last)) drawn.push(renameAt);

  return (
    <div className="grid-view" ref={scroller} onScroll={(e) => setScrollTop(e.currentTarget.scrollTop)}>
      {p.entries.length === 0 && <div className="empty">{p.emptyText}</div>}
      <div style={{ height: rows > 0 ? rows * TILE_H + PAD * 2 : 0, position: "relative" }}>
        {drawn.map((i) => {
          const row = Math.floor(i / columns);
          return (
            <Tile
              key={p.entries[i].path}
              index={i}
              e={p.entries[i]}
              x={PAD + (i % columns) * TILE_W}
              y={PAD + row * TILE_H}
              visible={row >= firstSeen && row <= lastSeen}
              sel={p.selected.has(p.entries[i].path)}
              focused={i === p.focus}
              grid={p}
            />
          );
        })}
      </div>
    </div>
  );
}

interface TileProps {
  index: number;
  e: Entry;
  x: number;
  y: number;
  /** In the viewport, not just in the overscan: only these ask for a thumbnail. */
  visible: boolean;
  sel: boolean;
  focused: boolean;
  grid: GridViewProps;
}

function Tile({ index, e, x, y, visible, sel, focused, grid }: TileProps) {
  const thumb = useThumb(e, visible);
  const sizeText = e.is_dir ? (grid.sized ? formatSize(e.size) : "") : formatSize(e.size);
  return (
    <div
      className={`tile ${sel ? "sel" : ""} ${focused ? "focus" : ""} ${e.hidden || e.system ? "dim" : ""}`}
      style={{ left: x, top: y, width: TILE_W - 4, height: TILE_H - 4 }}
      title={shown(e.path)}
      // The path as it is (the tooltip has hidden marks made visible): Explorer's middle click
      // finds the tile's entry by it.
      data-tile-path={e.path}
      data-path={grid.dropFolders && e.is_dir ? e.path : undefined}
      onMouseDown={(ev) => {
        if (ev.button !== 0) return;
        const drag = grid.onDragOut;
        pressItem(ev, index, sel, grid.selected.size, grid.onSelect, drag && (() => drag(e)));
      }}
      onDoubleClick={() => grid.onActivate(e)}
      onContextMenu={(ev) => grid.onContextMenu(ev, index)}
    >
      <div className={`tile-thumb ${e.reparse ? "link" : ""}`}>
        {thumb ? <img src={thumb} alt="" draggable={false} /> : <Glyph e={e} />}
      </div>
      {grid.renaming === e.path ? (
        <TileRename entry={e} onDone={grid.onRenameDone} />
      ) : (
        <div className="tile-name">{shown(e.name)}</div>
      )}
      <div className="tile-size">{sizeText}</div>
    </div>
  );
}

/** Folder or page shape drawn in CSS, for items without a thumbnail (yet). */
export function Glyph({ e }: { e: Entry }) {
  if (e.is_dir) return <span className="glyph dir" />;
  const dot = e.name.lastIndexOf(".");
  const ext = dot > 0 ? e.name.slice(dot + 1, dot + 5).toUpperCase() : "";
  return (
    <span className="glyph file">
      <span className="glyph-ext">{shown(ext)}</span>
    </span>
  );
}

function TileRename({ entry, onDone }: { entry: Entry; onDone: (e: Entry, name: string | null) => void }) {
  const ref = useRef<HTMLInputElement>(null);
  // Enter or Escape closes the box, and the blur that follows must not submit a second time.
  const done = useRef(false);
  const finish = (name: string | null) => {
    if (done.current) return;
    done.current = true;
    onDone(entry, name);
  };
  // Only when the box opens: a reload hands in a fresh Entry, and re-selecting then would
  // overwrite what the user is typing.
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.focus();
    // Select the name without the extension, like Explorer.
    const dot = entry.is_dir ? -1 : entry.name.lastIndexOf(".");
    el.setSelectionRange(0, dot > 0 ? dot : entry.name.length);
  }, [entry.path]);
  return (
    <input
      ref={ref}
      className="tile-rename"
      defaultValue={entry.name}
      // Mouse use inside the box is text editing; it must not reach the tile (select, open, menu).
      onMouseDown={(ev) => ev.stopPropagation()}
      onDoubleClick={(ev) => ev.stopPropagation()}
      onContextMenu={(ev) => ev.stopPropagation()}
      onKeyDown={(ev) => {
        ev.stopPropagation();
        if (ev.key === "Enter") finish(ev.currentTarget.value);
        if (ev.key === "Escape") finish(null);
      }}
      // The name-conflict dialog takes the focus for a while and gives it back: still typing.
      onBlur={(ev) => focusTakenByConflict(ev) || finish(ev.currentTarget.value)}
    />
  );
}
