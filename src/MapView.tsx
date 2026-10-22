import { useEffect, useId, useLayoutEffect, useMemo, useRef, useState } from "react";
import {
  hierarchy,
  partition,
  tree as tidyTree,
  treemap,
  treemapSquarify,
  type HierarchyRectangularNode,
} from "d3-hierarchy";
import { methodNote, v04, v05, type Category, type DiffInfo, type DiffNode, type MapNode } from "./api";
import { driveKey, folderTitle, formatDate, formatSize, parentOf, pathKey } from "./format";
import "./v04.css";

type Mode = "icicle" | "sunburst" | "treemap" | "mindmap";

const MODES: { id: Mode; label: string }[] = [
  { id: "icicle", label: "아이시클" },
  { id: "sunburst", label: "선버스트" },
  { id: "treemap", label: "트리맵" },
  { id: "mindmap", label: "가지 나무" },
];

const MODE_KEY = "fine-file.map-mode";
const DIFF_KEY = "fine-file.map-diff";
/** Levels under the folder shown. */
const DEPTH = 4;
/** The mind map's: three levels of eight already make a wide tree. */
const MIND_DEPTH = 3;

export const CATEGORY_LABEL: Record<Category, string> = {
  system: "시스템",
  program: "프로그램",
  dev: "개발",
  game: "게임",
  cache: "캐시",
  media: "미디어",
  document: "문서",
  archive: "압축·설치",
  personal: "개인",
  other: "기타",
};

const CATEGORIES = Object.keys(CATEGORY_LABEL) as Category[];

function savedMode(): Mode {
  try {
    const v = localStorage.getItem(MODE_KEY);
    return MODES.some((m) => m.id === v) ? (v as Mode) : "icicle";
  } catch {
    return "icicle";
  }
}

function storeMode(m: Mode) {
  try {
    localStorage.setItem(MODE_KEY, m);
  } catch {
    /* storage blocked: the choice lasts until the app closes */
  }
}

function savedDiff(): boolean {
  try {
    return localStorage.getItem(DIFF_KEY) === "1";
  } catch {
    return false;
  }
}

function storeDiff(on: boolean) {
  try {
    localStorage.setItem(DIFF_KEY, on ? "1" : "0");
  } catch {
    /* storage blocked: the choice lasts until the map closes */
  }
}

/** A block of the map: an item of the folder, or the small items folded together ("기타 N개"). */
interface Item {
  key: string;
  name: string;
  /** Null for the folded block, which nothing can be done with. */
  path: string | null;
  size: number;
  files: number;
  /** Items folded into the "기타" block; 0 for the others. */
  count: number;
  isDir: boolean;
  category: Category | null;
  kids: Item[];
}

/** What the listed children of `n` leave out, as one block. */
function restOf(n: MapNode): number {
  if (!n.is_dir) return 0;
  // Nothing listed: every child was too small to list (a folder at the depth limit has none folded).
  if (n.children.length === 0) return n.other_count > 0 ? Math.max(n.other_size, n.size) : 0;
  const listed = n.children.reduce((s, c) => s + c.size, 0);
  const rest = Math.max(n.other_size, n.size - listed, 0);
  // A sliver nobody folded (sizes counted a moment apart) is not worth a block.
  return n.other_count === 0 && rest < n.size * 0.001 ? 0 : rest;
}

function restItem(parentKey: string, size: number, count: number): Item {
  return {
    key: `${parentKey}\u0000기타`,
    name: count > 0 ? `기타 ${count.toLocaleString()}개` : "기타",
    path: null,
    size,
    files: 0,
    count,
    isDir: false,
    category: null,
    kids: [],
  };
}

function toItem(n: MapNode, root = false): Item {
  const kids = n.children.filter((c) => c.size > 0).map((c) => toItem(c));
  const rest = restOf(n);
  if (rest > 0) kids.push(restItem(n.path, rest, n.other_count));
  return {
    key: n.path,
    name: (root ? "" : n.name) || folderTitle(n.path),
    path: n.path,
    size: n.size,
    files: n.files,
    count: 0,
    isDir: n.is_dir,
    category: n.category,
    kids,
  };
}

/** Bytes per category, counted on the blocks with nothing under them; "rest" for the folded ones. */
function sharesOf(tree: Item): Map<Category | "rest", number> {
  const out = new Map<Category | "rest", number>();
  const walk = (it: Item) => {
    if (it.kids.length) return it.kids.forEach(walk);
    const k = it.category ?? "rest";
    out.set(k, (out.get(k) ?? 0) + it.size);
  };
  walk(tree);
  return out;
}

function ratioText(size: number, total: number): string {
  const r = total > 0 ? (size / total) * 100 : 0;
  if (r >= 10) return `${r.toFixed(0)}%`;
  if (r >= 0.1) return `${r.toFixed(1)}%`;
  return r > 0 ? "0.1% 미만" : "0%";
}

// ---- change since the scan before ------------------------------------------------

/** Growth from +200% and shrinking from −50% get the deepest colour. */
const GROW_FULL = 2;
const SHRINK_FULL = 0.5;

/** Every node of the comparison by pathKey(), to find a block's change with. */
function diffIndex(root: DiffNode): Map<string, DiffNode> {
  const out = new Map<string, DiffNode>();
  const walk = (n: DiffNode) => {
    out.set(pathKey(n.path), n);
    n.children.forEach(walk);
  };
  walk(root);
  return out;
}

/** Where between the light and the deep colour a change sits, 0 to 100. */
function changeDepth(d: DiffNode): number {
  if (d.prev_size <= 0) return 100;
  const r = Math.abs(d.size - d.prev_size) / d.prev_size;
  const full = d.status === "shrunk" ? SHRINK_FULL : GROW_FULL;
  return Math.round(Math.min(1, Math.log1p(r) / Math.log1p(full)) * 100);
}

/**
 * A block's fill by its change. Not in the comparison (it lists the biggest changes, so the rest
 * changed little or not at all) is drawn like an unchanged one.
 */
function changeFill(d: DiffNode | undefined): string {
  if (!d || d.status === "same" || d.status === "removed") return "var(--diff-same)";
  if (d.status === "new") return "var(--diff-new)";
  const tone = d.status === "grown" ? "grow" : "shrink";
  return `color-mix(in oklab, var(--diff-${tone}-hi) ${changeDepth(d)}%, var(--diff-${tone}-lo))`;
}

function percentText(delta: number, prev: number): string | null {
  if (prev <= 0) return null;
  const r = (Math.abs(delta) / prev) * 100;
  const sign = delta < 0 ? "−" : "+";
  if (r >= 10) return `${sign}${r.toFixed(0)}%`;
  if (r >= 0.1) return `${sign}${r.toFixed(1)}%`;
  return r > 0 ? `${sign}0.1% 미만` : "0%";
}

/** "+1.2 GB (+35%)", "새로 생김 (+300 MB)", "변화 없음". */
function changeText(d: DiffNode | undefined): string {
  if (!d) return "크게 바뀌지 않음";
  if (d.status === "same") return "변화 없음";
  if (d.status === "new") return `새로 생김 (+${formatSize(d.size)})`;
  if (d.status === "removed") return `없어짐 (−${formatSize(d.prev_size)})`;
  const delta = d.size - d.prev_size;
  const pct = percentText(delta, d.prev_size);
  return `${delta < 0 ? "−" : "+"}${formatSize(Math.abs(delta))}${pct ? ` (${pct})` : ""}`;
}

/** A scan time for the comparison line; "지난" when the backend did not tell. */
function scanTime(ft: number | null | undefined): string {
  return (ft && formatDate(ft)) || "지난";
}

/** The comparison of a folder, or why there is none (no scan before this one, and the like). */
interface Compared {
  path: string;
  depth: number;
  root: DiffNode | null;
  /** The backend's words. */
  error: string | null;
  info: DiffInfo | null;
}

// ---- labels -------------------------------------------------------------------

/** The font the map's labels use (v04.css), for measuring them. */
const FONT = '11px "Segoe UI Variable", "Segoe UI", "Malgun Gothic", system-ui, sans-serif';
let measure: CanvasRenderingContext2D | null | undefined;
const widths = new Map<string, number>();

function textWidth(s: string): number {
  const known = widths.get(s);
  if (known !== undefined) return known;
  if (measure === undefined) {
    measure = document.createElement("canvas").getContext("2d");
    if (measure) measure.font = FONT;
  }
  const w = measure ? measure.measureText(s).width : s.length * 7;
  if (widths.size > 5000) widths.clear();
  widths.set(s, w);
  return w;
}

/** `s`, cut short with "…" to fit `max` pixels; null when not even a couple of characters fit. */
function fit(s: string, max: number): string | null {
  if (max <= 0) return null;
  if (textWidth(s) <= max) return s;
  const chars = Array.from(s);
  let lo = 0;
  let hi = chars.length - 1;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (textWidth(`${chars.slice(0, mid).join("")}…`) <= max) lo = mid;
    else hi = mid - 1;
  }
  return lo >= 2 ? `${chars.slice(0, lo).join("")}…` : null;
}

interface Label {
  text: string;
  x: number;
  y: number;
  /** The size line, drawn lighter. */
  size?: boolean;
  /** Centred on (x, y), turned by `transform` (the sunburst's). */
  transform?: string;
}

type Shape =
  | { kind: "rect"; x: number; y: number; w: number; h: number }
  | { kind: "path"; d: string }
  /** At the origin, or at (cx, cy) in the mind map. */
  | { kind: "circle"; r: number; cx?: number; cy?: number };

interface Cell {
  item: Item;
  depth: number;
  /** Has blocks of its own under it. */
  inner: boolean;
  shape: Shape;
  labels: Label[];
  /** The mind map's fold switch, on a folder with items under it: open when they are shown. */
  toggle?: { x: number; y: number; open: boolean };
}

interface Bounds {
  x0: number;
  y0: number;
  x1: number;
  y1: number;
}

interface Layout {
  mode: Mode;
  cells: Cell[];
  /** Where (0, 0) of the cells is: the sunburst's centre. */
  origin: [number, number];
  /** The mind map's branches, as one path, and the space it all takes. */
  links?: string;
  bounds?: Bounds;
}

/** Name, and size under it when the block is tall enough. */
function rectLabels(item: Item, x: number, y: number, w: number, h: number): Label[] {
  if (h < 15 || w < 24) return [];
  const name = fit(item.name, w - 8);
  if (!name) return [];
  if (h < 32) return [{ text: name, x: x + 4, y: y + h / 2 + 4 }];
  const out: Label[] = [{ text: name, x: x + 4, y: y + 14 }];
  // A number cut short says nothing: whole or not at all.
  const size = formatSize(item.size);
  if (textWidth(size) <= w - 8) out.push({ text: size, x: x + 4, y: y + 28, size: true });
  return out;
}

function rootOf(tree: Item) {
  return (
    hierarchy(tree, (d) => d.kids)
      .sum((d) => (d.kids.length ? 0 : d.size))
      // Biggest first; the folded block last.
      .sort((a, b) => Number(a.data.path === null) - Number(b.data.path === null) || (b.value ?? 0) - (a.value ?? 0))
  );
}

/** Columns left to right: the folder, then each level under it. */
function icicle(tree: Item, w: number, h: number): Layout {
  const root = partition<Item>().size([h, w])(rootOf(tree));
  const cells: Cell[] = [];
  for (const n of root.descendants()) {
    const x = n.y0;
    const y = n.x0;
    const cw = n.y1 - n.y0;
    const ch = n.x1 - n.x0;
    if (ch < 0.5) continue;
    cells.push({
      item: n.data,
      depth: n.depth,
      inner: !!n.children,
      shape: { kind: "rect", x, y, w: cw, h: ch },
      labels: rectLabels(n.data, x, y, cw, ch),
    });
  }
  return { mode: "icicle", cells, origin: [0, 0] };
}

const HEADER = 17;

/** Nested boxes; a folder big enough has a header line with its name. */
function treemapOf(tree: Item, w: number, h: number): Layout {
  const headed = new Set<HierarchyRectangularNode<Item>>();
  const side = (n: HierarchyRectangularNode<Item>) => (n.depth === 0 ? 0 : 2);
  const root = treemap<Item>()
    .tile(treemapSquarify)
    .size([w, h])
    .round(true)
    .paddingInner(2)
    // Asked once the folder's own box is placed, so its size decides.
    .paddingTop((n) => {
      if (n.depth > 0 && n.y1 - n.y0 > 40 && n.x1 - n.x0 > 48) {
        headed.add(n);
        return HEADER;
      }
      return side(n);
    })
    .paddingRight(side)
    .paddingBottom(side)
    .paddingLeft(side)(rootOf(tree));
  const cells: Cell[] = [];
  for (const n of root.descendants()) {
    if (n.depth === 0) continue;
    const cw = n.x1 - n.x0;
    const ch = n.y1 - n.y0;
    if (cw < 1 || ch < 1) continue;
    let labels: Label[] = [];
    if (headed.has(n)) {
      const name = fit(n.data.name, cw - 8);
      if (name) {
        labels = [{ text: name, x: n.x0 + 4, y: n.y0 + 12 }];
        const size = formatSize(n.data.size);
        const after = n.x0 + 4 + textWidth(name) + 6;
        if (name === n.data.name && after + textWidth(size) <= n.x1 - 4) {
          labels.push({ text: size, x: after, y: n.y0 + 12, size: true });
        }
      }
    } else if (!n.children) {
      labels = rectLabels(n.data, n.x0, n.y0, cw, ch);
    }
    cells.push({ item: n.data, depth: n.depth, inner: !!n.children, shape: { kind: "rect", x: n.x0, y: n.y0, w: cw, h: ch }, labels });
  }
  return { mode: "treemap", cells, origin: [0, 0] };
}

/** A ring sector from angle a0 to a1 (radians, clockwise from 12 o'clock), radius r0 to r1. */
function arcPath(a0: number, a1: number, r0: number, r1: number): string {
  const pt = (r: number, a: number) => `${(r * Math.sin(a)).toFixed(2)},${(-r * Math.cos(a)).toFixed(2)}`;
  if (a1 - a0 > 2 * Math.PI - 1e-6) {
    // A whole ring: two half circles (an arc cannot end where it starts), the hole the other way round.
    const ring = (r: number, sweep: 0 | 1) =>
      `M${pt(r, 0)}A${r},${r} 0 1 ${sweep} ${pt(r, Math.PI)}A${r},${r} 0 1 ${sweep} ${pt(r, 0)}Z`;
    return ring(r1, 1) + (r0 > 0 ? ring(r0, 0) : "");
  }
  const large = a1 - a0 > Math.PI ? 1 : 0;
  return (
    `M${pt(r1, a0)}A${r1},${r1} 0 ${large} 1 ${pt(r1, a1)}L${pt(r0, a1)}` +
    (r0 > 0 ? `A${r0},${r0} 0 ${large} 0 ${pt(r0, a0)}` : "") +
    "Z"
  );
}

/** Rings out from the folder in the middle. */
function sunburst(tree: Item, w: number, h: number): Layout {
  const r = Math.max(0, Math.min(w, h) / 2 - 4);
  const root = partition<Item>().size([2 * Math.PI, r])(rootOf(tree));
  const cells: Cell[] = [];
  for (const n of root.descendants()) {
    if (n.depth === 0) {
      const labels: Label[] = [];
      const name = fit(n.data.name, n.y1 * 1.6);
      if (name && n.y1 > 24) {
        labels.push({ text: name, x: 0, y: -2 });
        const size = formatSize(n.data.size);
        if (n.y1 > 34 && textWidth(size) <= n.y1 * 1.6) labels.push({ text: size, x: 0, y: 12, size: true });
      }
      cells.push({ item: n.data, depth: 0, inner: true, shape: { kind: "circle", r: n.y1 }, labels });
      continue;
    }
    const span = n.x1 - n.x0;
    if (span * n.y1 < 0.5) continue;
    const labels: Label[] = [];
    const mid = (n.x0 + n.x1) / 2;
    const rMid = (n.y0 + n.y1) / 2;
    // Along the radius, where the ring is wide enough for a line of text.
    if (span * rMid >= 13) {
      const name = fit(n.data.name, n.y1 - n.y0 - 8);
      if (name) {
        const deg = (mid * 180) / Math.PI;
        labels.push({
          text: name,
          x: 0,
          y: 0,
          transform: `rotate(${(deg - 90).toFixed(2)}) translate(${rMid.toFixed(2)},0) rotate(${deg < 180 ? 0 : 180})`,
        });
      }
    }
    cells.push({ item: n.data, depth: n.depth, inner: !!n.children, shape: { kind: "path", d: arcPath(n.x0, n.x1, n.y0, n.y1) }, labels });
  }
  return { mode: "sunburst", cells, origin: [w / 2, h / 2] };
}

// ---- mind map ("가지 나무") ---------------------------------------------------------

/** Children drawn per folder, biggest first; the others go into one "기타" node. */
const MIND_KIDS = 8;
/** About how many nodes the tree opens with: the biggest folders first. */
const MIND_BUDGET = 40;
/** From one level to the next, and the least room between two nodes one above the other. */
const MIND_COL = 210;
const MIND_ROW = 22;
/** Node radii: the folder itself is the biggest; the area follows the size. */
const R_MIN = 3;
const R_MAX = 16;
const TOGGLE_R = 6;
/** Zoom limits; fitting the whole tree does not go below FIT_MIN, so the labels stay readable. */
const ZOOM_MIN = 0.2;
const ZOOM_MAX = 3;
const FIT_MIN = 0.55;

/** The top MIND_KIDS children of every folder; the rest, and what the map folded, as one node. */
function mindTreeOf(it: Item): Item {
  if (!it.kids.length) return it;
  const real = it.kids.filter((k) => k.path !== null).sort((a, b) => b.size - a.size);
  const folded = it.kids.filter((k) => k.path === null);
  const kids = real.slice(0, MIND_KIDS).map(mindTreeOf);
  const left = real.slice(MIND_KIDS);
  const restSize = left.reduce((s, k) => s + k.size, 0) + folded.reduce((s, k) => s + k.size, 0);
  const restCount = left.length + folded.reduce((s, k) => s + k.count, 0);
  if (restSize > 0) kids.push(restItem(it.key, restSize, restCount));
  return { ...it, kids };
}

/** The folders open at first: the biggest, as long as about MIND_BUDGET nodes are shown. */
function defaultOpen(tree: Item): Set<string> {
  const open = new Set<string>();
  let shown = 1;
  const queue: Item[] = [tree];
  while (queue.length) {
    queue.sort((a, b) => b.size - a.size);
    const it = queue.shift() as Item;
    if (it !== tree && shown + it.kids.length > MIND_BUDGET) continue;
    open.add(it.key);
    shown += it.kids.length;
    queue.push(...it.kids.filter((k) => k.kids.length > 0));
  }
  return open;
}

/** A tidy tree on its side: the folder at the left, each level a column further right. */
function mindmap(tree: Item, open: Set<string>): Layout {
  const rOf = (s: number) => R_MIN + (R_MAX - R_MIN) * Math.sqrt(tree.size > 0 ? Math.min(1, s / tree.size) : 0);
  const root = tidyTree<Item>()
    .nodeSize([1, MIND_COL])
    // In pixels (nodeSize's 1): big circles one above the other need more room; families a gap.
    .separation((a, b) => Math.max(MIND_ROW, rOf(a.data.size) + rOf(b.data.size) + 6) + (a.parent === b.parent ? 0 : 10))(
      hierarchy(tree, (d) => (open.has(d.key) ? d.kids : null)),
    );
  const cells: Cell[] = [];
  let links = "";
  const b: Bounds = { x0: -R_MAX - 4, y0: 0, x1: 0, y1: 0 };
  const p = (n: number) => n.toFixed(1);
  for (const n of root.descendants()) {
    const x = n.y;
    const y = n.x;
    const r = rOf(n.data.size);
    if (n.parent) {
      const sx = n.parent.y + rOf(n.parent.data.size);
      const sy = n.parent.x;
      const tx = x - r;
      const mx = (sx + tx) / 2;
      links += `M${p(sx)},${p(sy)}C${p(mx)},${p(sy)} ${p(mx)},${p(y)} ${p(tx)},${p(y)}`;
    }
    const shown = !!n.children;
    const foldable = n.depth > 0 && n.data.kids.length > 0;
    // Before the next column's circles when this node's children are drawn; free room otherwise.
    const room = (shown ? MIND_COL - r - 30 : 260) - (foldable ? TOGGLE_R * 2 + 6 : 0);
    const labels: Label[] = [];
    const lx = x + r + 5;
    let end = x + r;
    const size = formatSize(n.data.size);
    const name = fit(n.data.name, room - textWidth(size) - 6);
    if (name) {
      const after = lx + textWidth(name) + 6;
      labels.push({ text: name, x: lx, y: y + 4 }, { text: size, x: after, y: y + 4, size: true });
      end = after + textWidth(size);
    } else {
      const only = fit(n.data.name, room);
      if (only) {
        labels.push({ text: only, x: lx, y: y + 4 });
        end = lx + textWidth(only);
      }
    }
    const toggle = foldable ? { x: end + 6 + TOGGLE_R, y, open: shown } : undefined;
    if (toggle) end = toggle.x + TOGGLE_R;
    b.x1 = Math.max(b.x1, end);
    b.y0 = Math.min(b.y0, y - Math.max(r, 9));
    b.y1 = Math.max(b.y1, y + Math.max(r, 9));
    cells.push({ item: n.data, depth: n.depth, inner: shown, shape: { kind: "circle", r, cx: x, cy: y }, labels, toggle });
  }
  return { mode: "mindmap", cells, origin: [0, 0], links, bounds: b };
}

interface View {
  k: number;
  x: number;
  y: number;
}

/** The whole tree in the view when it fits; otherwise the folder at the middle of the left edge. */
function fitView(b: Bounds, w: number, h: number): View {
  const m = 24;
  const k = Math.min(1, (w - 2 * m) / (b.x1 - b.x0), (h - 2 * m) / (b.y1 - b.y0));
  if (k >= FIT_MIN) return { k, x: m - b.x0 * k, y: h / 2 - ((b.y0 + b.y1) / 2) * k };
  return { k: FIT_MIN, x: m - b.x0 * FIT_MIN, y: h / 2 };
}

function layoutOf(tree: Item, mode: Mode, w: number, h: number): Layout {
  if (mode === "sunburst") return sunburst(tree, w, h);
  if (mode === "treemap") return treemapOf(tree, w, h);
  return icicle(tree, w, h);
}

function ShapeEl({ shape, className, style }: { shape: Shape; className?: string; style?: React.CSSProperties }) {
  if (shape.kind === "rect") {
    return <rect className={className} style={style} x={shape.x} y={shape.y} width={shape.w} height={shape.h} />;
  }
  if (shape.kind === "circle") return <circle className={className} style={style} cx={shape.cx} cy={shape.cy} r={shape.r} />;
  return <path className={className} style={style} d={shape.d} />;
}

/** A block's outline drawn inside it, so the blocks next to it do not cover half of it. */
function inset(shape: Shape): Shape {
  return shape.kind === "rect" && shape.w > 3 && shape.h > 3
    ? { kind: "rect", x: shape.x + 1, y: shape.y + 1, w: shape.w - 2, h: shape.h - 2 }
    : shape;
}

/** "드라이브를 먼저 스캔해 주세요" and the like, in words for this view. */
function errorText(err: string): string {
  if (err.includes("먼저 스캔")) {
    return "이 드라이브는 아직 스캔하지 않았어요. 사이드바에서 드라이브를 먼저 스캔하면 크기 지도를 볼 수 있어요.";
  }
  return err;
}

interface Loaded {
  path: string;
  depth: number;
  root: MapNode | null;
  error: string | null;
}

export function MapView({
  path,
  onNavigate,
  onOpen,
}: {
  path: string;
  onNavigate(path: string): void;
  onOpen(path: string): void;
}) {
  const [mode, setMode] = useState<Mode>(savedMode);
  const [diffOn, setDiffOn] = useState(savedDiff);
  const [loaded, setLoaded] = useState<Loaded | null>(null);
  const [loading, setLoading] = useState(false);
  const [compared, setCompared] = useState<Compared | null>(null);
  const [comparing, setComparing] = useState(false);
  const [reload, setReload] = useState(0);
  const [size, setSize] = useState({ w: 0, h: 0 });
  const [hover, setHover] = useState<number | null>(null);
  // The mind map's folds and view, for the folder they were made on.
  const [opened, setOpened] = useState<{ key: string; open: Set<string> } | null>(null);
  const [moved, setMoved] = useState<({ key: string } & View) | null>(null);
  const [dragging, setDragging] = useState(false);
  const hoverRef = useRef<number | null>(null);
  const box = useRef<HTMLDivElement>(null);
  const tip = useRef<HTMLDivElement>(null);
  const pointer = useRef<[number, number] | null>(null);
  const drag = useRef<{ id: number; x: number; y: number; from: View; moving: boolean } | null>(null);
  // The click that ends a drag is not a click on a node.
  const eatClick = useRef(false);
  const zoom = useRef<{ key: string; view: View } | null>(null);
  const hatch = `map-hatch-${useId().replace(/[^A-Za-z0-9_-]/g, "")}`;
  const depth = mode === "mindmap" ? MIND_DEPTH : DEPTH;

  // An answer for a folder no longer shown (the user zoomed on meanwhile) is dropped.
  useEffect(() => {
    let stale = false;
    setLoading(true);
    v04.treeMap(path, depth).then(
      (root) => {
        if (stale) return;
        setLoaded({ path, depth, root, error: null });
        setLoading(false);
      },
      (err) => {
        if (stale) return;
        setLoaded({ path, depth, root: null, error: String(err) });
        setLoading(false);
      },
    );
    return () => {
      stale = true;
    };
  }, [path, depth, reload]);

  // The comparison with the scan before, the same way.
  useEffect(() => {
    if (!diffOn) return;
    let stale = false;
    setComparing(true);
    Promise.allSettled([v05.scanDiff(path, depth), v05.diffInfo(driveKey(path))]).then(([d, info]) => {
      if (stale) return;
      setCompared({
        path,
        depth,
        root: d.status === "fulfilled" ? d.value : null,
        error: d.status === "rejected" ? String(d.reason) : null,
        info: info.status === "fulfilled" ? info.value : null,
      });
      setComparing(false);
    });
    return () => {
      stale = true;
    };
  }, [diffOn, path, depth, reload]);

  useLayoutEffect(() => {
    const el = box.current;
    if (!el) return;
    const ro = new ResizeObserver(([e]) => {
      const w = Math.floor(e.contentRect.width);
      const h = Math.floor(e.contentRect.height);
      setSize((s) => (s.w === w && s.h === h ? s : { w, h }));
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const tree = useMemo(() => (loaded?.root ? toItem(loaded.root, true) : null), [loaded]);
  const mindTree = useMemo(() => (mode === "mindmap" && tree ? mindTreeOf(tree) : null), [tree, mode]);
  const mindKey = mindTree && loaded ? loaded.path : null;
  const firstOpen = useMemo(() => (mindTree ? defaultOpen(mindTree) : null), [mindTree]);
  const open = opened && opened.key === mindKey ? opened.open : firstOpen;
  const layout = useMemo(() => {
    if (!tree || tree.size <= 0 || size.w <= 0 || size.h <= 0) return null;
    if (mode === "mindmap") return mindTree && open ? mindmap(mindTree, open) : null;
    return layoutOf(tree, mode, size.w, size.h);
  }, [tree, mindTree, open, mode, size]);
  // Where the user left the mind map, or the whole tree fitted in.
  const pinned = moved !== null && moved.key === mindKey;
  const view = layout?.bounds ? (pinned ? moved : fitView(layout.bounds, size.w, size.h)) : null;
  const shares = useMemo(() => (tree ? sharesOf(tree) : null), [tree]);
  const total = tree?.size ?? 0;
  // The comparison of this folder at this depth; null while it loads or with the switch off.
  const comparedHere = diffOn && !comparing && compared?.path === path && compared.depth === depth ? compared : null;
  const diffs = useMemo(() => (comparedHere?.root ? diffIndex(comparedHere.root) : null), [comparedHere]);
  // The map drawn is of another folder (or out of date, or waiting for its comparison) until the answers come.
  const stale = loading || loaded?.path !== path || loaded.depth !== depth || (diffOn && !comparedHere);
  const up = parentOf(path);

  useEffect(() => {
    hoverRef.current = null;
    setHover(null);
  }, [layout, stale]);

  // Read by the wheel handler, which is bound once.
  useLayoutEffect(() => {
    zoom.current = mindKey && view && !stale ? { key: mindKey, view } : null;
  });

  // Wheel zoom on the mind map, about the pointer. Bound by hand: React's wheel listener is passive.
  useEffect(() => {
    const el = box.current;
    if (!el) return;
    const onWheel = (e: WheelEvent) => {
      const z = zoom.current;
      if (!z) return;
      e.preventDefault();
      const b = el.getBoundingClientRect();
      const px = e.clientX - b.left;
      const py = e.clientY - b.top;
      const dy = e.deltaMode === 1 ? e.deltaY * 16 : e.deltaMode === 2 ? e.deltaY * 400 : e.deltaY;
      const k = Math.min(ZOOM_MAX, Math.max(ZOOM_MIN, z.view.k * Math.exp(-dy * 0.0015)));
      if (k === z.view.k) return;
      const f = k / z.view.k;
      const next = { k, x: px - (px - z.view.x) * f, y: py - (py - z.view.y) * f };
      // Several wheel events can come before the next render.
      z.view = next;
      setMoved({ key: z.key, ...next });
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => el.removeEventListener("wheel", onWheel);
  }, []);

  const moveTip = (cx: number, cy: number) => {
    const el = tip.current;
    const host = box.current;
    if (!el || !host) return;
    const b = host.getBoundingClientRect();
    let x = cx - b.left + 14;
    let y = cy - b.top + 16;
    if (x + el.offsetWidth > b.width - 4) x = Math.max(4, cx - b.left - el.offsetWidth - 14);
    if (y + el.offsetHeight > b.height - 4) y = Math.max(4, cy - b.top - el.offsetHeight - 12);
    el.style.transform = `translate(${x}px, ${y}px)`;
  };

  // A new block under the pointer: the tooltip changed size, so place it again.
  useLayoutEffect(() => {
    if (hover !== null && pointer.current) moveTip(...pointer.current);
  }, [hover]);

  const cellAt = (e: React.SyntheticEvent): number | null => {
    const el = e.target instanceof Element ? e.target.closest("[data-i]") : null;
    return el ? Number(el.getAttribute("data-i")) : null;
  };

  const toggleAt = (e: React.SyntheticEvent): string | null => {
    const el = e.target instanceof Element ? e.target.closest("[data-toggle]") : null;
    return el ? el.getAttribute("data-toggle") : null;
  };

  const toggleOpen = (key: string) => {
    if (!mindKey || !open) return;
    const next = new Set(open);
    if (!next.delete(key)) next.add(key);
    setOpened({ key: mindKey, open: next });
    // The tree grows or shrinks about the node: the view stays where it is rather than fitting again.
    if (view && !pinned) setMoved({ key: mindKey, ...view });
  };

  const onDown = (e: React.PointerEvent<SVGSVGElement>) => {
    eatClick.current = false;
    if (!view || e.button !== 0) return;
    drag.current = { id: e.pointerId, x: e.clientX, y: e.clientY, from: view, moving: false };
  };

  const onUp = (e: React.PointerEvent<SVGSVGElement>) => {
    const g = drag.current;
    if (!g || g.id !== e.pointerId) return;
    drag.current = null;
    if (g.moving) {
      eatClick.current = true;
      setDragging(false);
    }
  };

  const onMove = (e: React.PointerEvent<SVGSVGElement>) => {
    pointer.current = [e.clientX, e.clientY];
    const g = drag.current;
    if (g && g.id === e.pointerId && mindKey) {
      if (!(e.buttons & 1)) {
        // Let go outside the window.
        drag.current = null;
        if (g.moving) setDragging(false);
      } else {
        const dx = e.clientX - g.x;
        const dy = e.clientY - g.y;
        if (!g.moving && Math.hypot(dx, dy) > 4) {
          // Captured only now: a plain click keeps its target, the node under it.
          g.moving = true;
          e.currentTarget.setPointerCapture(e.pointerId);
          setDragging(true);
          hoverRef.current = null;
          setHover(null);
        }
        if (g.moving) {
          setMoved({ key: mindKey, k: g.from.k, x: g.from.x + dx, y: g.from.y + dy });
          return;
        }
      }
    }
    const i = cellAt(e);
    if (i !== hoverRef.current) {
      hoverRef.current = i;
      setHover(i);
    }
    moveTip(e.clientX, e.clientY);
  };

  const onClick = (e: React.MouseEvent) => {
    if (eatClick.current) {
      eatClick.current = false;
      return;
    }
    // The second click of a double click is not a second zoom.
    if (!layout || stale || e.detail > 1) return;
    const t = toggleAt(e);
    if (t !== null) return toggleOpen(t);
    const i = cellAt(e);
    if (i === null) return;
    const c = layout.cells[i];
    if (c.depth === 0) {
      if (up) onNavigate(up);
    } else if (c.item.isDir && c.item.path) {
      onNavigate(c.item.path);
    }
  };

  const onDoubleClick = (e: React.MouseEvent) => {
    if (toggleAt(e) !== null) return;
    const i = cellAt(e);
    if (i === null || !layout || stale) return;
    const c = layout.cells[i];
    if (!c.item.isDir && c.item.path) onOpen(c.item.path);
  };

  // Drawn once per layout: hovering only moves the outline and the tooltip.
  const drawn = useMemo(() => {
    if (!layout) return null;
    const centred = layout.mode === "sunburst";
    // New items get a frame drawn over everything, so no neighbour hides it.
    const outlines: React.ReactNode[] = [];
    const cells = layout.cells.map((c, i) => {
      const d = diffs && c.item.path !== null ? diffs.get(pathKey(c.item.path)) : undefined;
      if (d?.status === "new") outlines.push(<ShapeEl key={c.item.key} shape={inset(c.shape)} className="map-diff-new" />);
      const fill = c.item.category === null ? `url(#${hatch})` : diffs ? changeFill(d) : undefined;
      const cls = [
        "map-cell",
        c.item.category === null ? "map-rest" : diffs ? "map-diff" : `cat-${c.item.category}`,
        c.inner ? "inner" : "",
        c.item.isDir ? "dir" : "",
        c.depth === 0 ? "root" : "",
      ]
        .filter(Boolean)
        .join(" ");
      return (
        <g key={c.item.key} data-i={i} className={cls}>
          <ShapeEl shape={c.shape} style={fill ? { fill } : undefined} />
          {c.labels.map((l, j) => (
            <text
              key={j}
              className={l.size ? "map-size" : undefined}
              x={l.x}
              y={l.y}
              transform={l.transform}
              textAnchor={centred ? "middle" : undefined}
              dy={l.transform ? "0.35em" : undefined}
            >
              {l.text}
            </text>
          ))}
          {c.toggle && (
            <g className="map-toggle" data-toggle={c.item.key} transform={`translate(${c.toggle.x.toFixed(1)},${c.toggle.y.toFixed(1)})`}>
              <circle r={TOGGLE_R} />
              <path d={c.toggle.open ? "M-3,0H3" : "M-3,0H3M0,-3V3"} />
            </g>
          )}
        </g>
      );
    });
    return { cells, outlines };
  }, [layout, hatch, diffs]);

  const hovered = hover !== null && layout ? layout.cells[hover] : null;
  const hint = hovered
    ? hovered.depth === 0
      ? up
        ? "누르면 위 폴더로 가요"
        : null
      : hovered.item.path === null
        ? null
        : hovered.item.isDir
          ? "누르면 이 폴더로 들어가요"
          : "두 번 누르면 열어요"
    : null;
  // With the comparison shown, every block but the folded one has a change line (perhaps "크게 바뀌지 않음").
  const hoverChange = !!(hovered && diffs && hovered.item.path !== null);
  const hoveredDiff =
    hovered && diffs && hovered.item.path !== null ? diffs.get(pathKey(hovered.item.path)) : undefined;
  const removed = hoveredDiff ? hoveredDiff.children.filter((c) => c.status === "removed") : [];

  let message: string | null = null;
  if (!loaded || (loaded.error && stale)) message = "크기 지도를 그리는 중…";
  else if (loaded.error) message = errorText(loaded.error);
  else if (total === 0 && !stale) message = "빈 폴더예요";

  const busy = loading || (diffOn && comparing);

  return (
    <div className="map-view">
      <div className="map-head">
        <button className="tool" disabled={!up} title={up ? `위로: ${up}` : undefined} onClick={() => up && onNavigate(up)}>
          ↑ 위로
        </button>
        <div className="map-title" title={path}>
          <b>{folderTitle(path)}</b>
          {tree && loaded?.path === path && (
            <span className="muted">
              {formatSize(total)} · 파일 {tree.files.toLocaleString()}개
            </span>
          )}
          {busy && <span className="muted">불러오는 중…</span>}
        </div>
        <div className="view-toggle" role="group" aria-label="지도 모양">
          {MODES.map((m) => (
            <button
              key={m.id}
              className={mode === m.id ? "on" : ""}
              aria-pressed={mode === m.id}
              onClick={() => {
                setMode(m.id);
                storeMode(m.id);
              }}
            >
              {m.label}
            </button>
          ))}
        </div>
        <button
          className={`tool${diffOn ? " on" : ""}`}
          aria-pressed={diffOn}
          title="지난 스캔과 비교해서 커지거나 작아진 곳을 색으로 보여요"
          onClick={() => {
            setDiffOn(!diffOn);
            storeDiff(!diffOn);
          }}
        >
          변화
        </button>
        {mode === "mindmap" && (
          <button className="tool" disabled={!pinned} title="가지 나무가 한눈에 들어오게 맞춰요" onClick={() => setMoved(null)}>
            맞추기
          </button>
        )}
        <button className="tool" disabled={busy} onClick={() => setReload((n) => n + 1)}>
          새로 고침
        </button>
      </div>

      {diffOn && (
        <div className={`map-diff-bar${comparedHere?.error ? " note" : ""}`} role="status">
          {!comparedHere ? (
            <span className="muted">지난 스캔과 비교하는 중…</span>
          ) : comparedHere.error ? (
            <span>{comparedHere.error}</span>
          ) : (
            comparedHere.root && (
              <>
                <b>{scanTime(comparedHere.info?.prev_scanned_at)} 스캔과 비교</b>
                <span>이 폴더 {changeText(comparedHere.root)}</span>
                {comparedHere.info?.scanned_at ? (
                  <span className="muted">지금 스캔 {formatDate(comparedHere.info.scanned_at)}</span>
                ) : null}
                {methodNote(comparedHere.info) && <span className="diff-method">{methodNote(comparedHere.info)}</span>}
              </>
            )
          )}
        </div>
      )}

      <div ref={box} className={`map-canvas${stale ? " stale" : ""}`}>
        {layout && !message && (
          <svg
            className={`map-svg map-${layout.mode}${dragging ? " dragging" : ""}`}
            width={size.w}
            height={size.h}
            role="img"
            aria-label={`${folderTitle(path)} 크기 지도`}
            onPointerDown={onDown}
            onPointerMove={onMove}
            onPointerUp={onUp}
            onPointerCancel={onUp}
            onLostPointerCapture={onUp}
            onPointerLeave={() => {
              hoverRef.current = null;
              setHover(null);
            }}
            onClick={onClick}
            onDoubleClick={onDoubleClick}
          >
            <defs>
              <pattern id={hatch} width="6" height="6" patternUnits="userSpaceOnUse" patternTransform="rotate(45)">
                <rect className="map-hatch-bg" width="6" height="6" />
                <line className="map-hatch-line" x1="0" y1="0" x2="0" y2="6" />
              </pattern>
            </defs>
            <g
              transform={
                view
                  ? `translate(${view.x.toFixed(1)},${view.y.toFixed(1)}) scale(${view.k.toFixed(4)})`
                  : `translate(${layout.origin[0]},${layout.origin[1]})`
              }
            >
              {layout.links && <path className="map-links" d={layout.links} />}
              {drawn?.cells}
              {drawn?.outlines}
              {hovered && <ShapeEl shape={hovered.shape} className="map-hover" />}
            </g>
          </svg>
        )}
        {layout?.mode === "mindmap" && !message && (
          <div className="map-mind-help">끌어서 옮기고 휠로 확대·축소해요. ＋/− 를 누르면 가지를 펼치거나 접어요.</div>
        )}
        {message && <div className={`map-message${loaded?.error ? " error" : ""}`}>{message}</div>}
        <div ref={tip} className="map-tip" hidden={!hovered}>
          {hovered && (
            <>
              <div className="map-tip-name">{hovered.item.name}</div>
              <dl>
                <dt>크기</dt>
                <dd>{formatSize(hovered.item.size)}</dd>
                {hoverChange && (
                  <>
                    <dt>변화</dt>
                    <dd className="map-change">{changeText(hoveredDiff)}</dd>
                    {hoveredDiff && (hoveredDiff.status === "grown" || hoveredDiff.status === "shrunk") && (
                      <>
                        <dt>지난 크기</dt>
                        <dd>{formatSize(hoveredDiff.prev_size)}</dd>
                      </>
                    )}
                  </>
                )}
                <dt>비율</dt>
                <dd>{ratioText(hovered.item.size, total)}</dd>
                {hovered.item.path === null ? (
                  hovered.item.count > 0 && (
                    <>
                      <dt>항목 수</dt>
                      <dd>{hovered.item.count.toLocaleString()}개</dd>
                    </>
                  )
                ) : (
                  hovered.item.isDir && (
                    <>
                      <dt>파일 수</dt>
                      <dd>{hovered.item.files.toLocaleString()}개</dd>
                    </>
                  )
                )}
                <dt>분류</dt>
                <dd>{hovered.item.category ? CATEGORY_LABEL[hovered.item.category] : "작은 항목 모음"}</dd>
              </dl>
              {removed.length > 0 && (
                <div className="map-tip-removed">
                  <div className="map-tip-sub">없어진 항목</div>
                  <ul>
                    {removed.slice(0, 5).map((r) => (
                      <li key={r.path}>
                        <span>{r.name}</span>
                        <span className="muted">−{formatSize(r.prev_size)}</span>
                      </li>
                    ))}
                  </ul>
                  {removed.length > 5 && <div className="map-tip-sub">외 {(removed.length - 5).toLocaleString()}개</div>}
                </div>
              )}
              {hint && <div className="map-tip-hint">{hint}</div>}
            </>
          )}
        </div>
      </div>

      {diffs ? (
        <div className="map-legend" aria-label="변화">
          <span className="map-key">
            <i className="map-swatch diff-grow" />
            커짐
            <span className="muted">진할수록 많이</span>
          </span>
          <span className="map-key">
            <i className="map-swatch diff-shrink" />
            작아짐
            <span className="muted">진할수록 많이</span>
          </span>
          <span className="map-key">
            <i className="map-swatch diff-new" />
            새로 생김
          </span>
          <span className="map-key">
            <i className="map-swatch diff-same" />
            크게 바뀌지 않음
          </span>
          <span className="map-key muted">없어진 항목은 폴더에 마우스를 올리면 보여요</span>
        </div>
      ) : (
        <div className="map-legend" aria-label="분류">
          {CATEGORIES.map((c) => {
            const bytes = shares?.get(c) ?? 0;
            return (
              <span key={c} className={`map-key${bytes > 0 ? "" : " off"}`} title={bytes > 0 ? formatSize(bytes) : undefined}>
                <i className={`map-swatch cat-${c}`} />
                {CATEGORY_LABEL[c]}
                {bytes > 0 && <span className="muted">{ratioText(bytes, total)}</span>}
              </span>
            );
          })}
          {(shares?.get("rest") ?? 0) > 0 && (
            <span className="map-key" title={formatSize(shares?.get("rest") ?? 0)}>
              <i className="map-swatch rest" />
              작은 항목 모음
              <span className="muted">{ratioText(shares?.get("rest") ?? 0, total)}</span>
            </span>
          )}
        </div>
      )}
    </div>
  );
}
