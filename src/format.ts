const UNITS = ["B", "KB", "MB", "GB", "TB"];

// Marks that turn text around ("계약서" + U+202E + "fdp.exe" reads as "계약서exe.pdf") or take no
// room (Unicode's default ignorables: U+200B, U+00AD, U+2065, U+3164, tags U+E0000–E007F…: two
// names that look the same). Names from the disk, AI or MCP are untrusted: wherever they are shown
// (not in a box they are edited in) such marks show as U+FFFD "�", one character for one. Joiners
// (U+200C, U+200D) and variation selectors (U+FE00–FE0F, U+E0100–E01EF) stay: emoji, some scripts
// and kanji variants need them. So do the tags of a flag (U+1F3F4, tags, U+E007F: 잉글랜드 깃발).
const HIDDEN_MARKS =
  /(\u{1F3F4}[\u{E0030}-\u{E0039}\u{E0061}-\u{E007A}]+\u{E007F})|(?![\u200C\u200D\uFE00-\uFE0F\u{E0100}-\u{E01EF}])\p{Default_Ignorable_Code_Point}/gu;

/** A name, path or label as shown: the marks that turn it around or hide in it made visible. */
export const shown = (s: string) => s.replace(HIDDEN_MARKS, (_, flag?: string) => flag ?? "\uFFFD");

export function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  let v = bytes;
  let u = 0;
  while (v >= 1024 && u < UNITS.length - 1) {
    v /= 1024;
    u++;
  }
  return `${v >= 100 ? v.toFixed(0) : v.toFixed(1)} ${UNITS[u]}`;
}

export function filetimeToDate(ft: number): Date | null {
  if (!ft) return null;
  return new Date(ft / 10000 - 11644473600000);
}

export function formatDate(ft: number): string {
  const d = filetimeToDate(ft);
  if (!d || d.getFullYear() < 1980) return "";
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

export function timeAgo(ft: number): string {
  const d = filetimeToDate(ft);
  if (!d) return "";
  const s = (Date.now() - d.getTime()) / 1000;
  if (s < 60) return "방금";
  if (s < 3600) return `${Math.floor(s / 60)}분 전`;
  if (s < 86400) return `${Math.floor(s / 3600)}시간 전`;
  return `${Math.floor(s / 86400)}일 전`;
}

const TYPES: Record<string, string> = {
  exe: "응용 프로그램", msi: "설치 패키지", dll: "라이브러리", sys: "시스템 파일",
  zip: "압축 파일", "7z": "압축 파일", rar: "압축 파일",
  jpg: "이미지", jpeg: "이미지", png: "이미지", gif: "이미지", webp: "이미지", bmp: "이미지",
  mp4: "동영상", mkv: "동영상", avi: "동영상", mov: "동영상", webm: "동영상",
  mp3: "오디오", wav: "오디오", flac: "오디오",
  pdf: "PDF 문서", txt: "텍스트", md: "Markdown", doc: "Word 문서", docx: "Word 문서",
  xls: "Excel 문서", xlsx: "Excel 문서", ppt: "PowerPoint", pptx: "PowerPoint", hwp: "한글 문서", hwpx: "한글 문서",
  iso: "디스크 이미지", vhdx: "가상 디스크", log: "로그",
};

export function typeLabel(name: string, isDir: boolean): string {
  if (isDir) return "폴더";
  const dot = name.lastIndexOf(".");
  if (dot <= 0) return "파일";
  const ext = name.slice(dot + 1).toLowerCase();
  return TYPES[ext] ?? `${ext.toUpperCase()} 파일`;
}

export function parentOf(path: string): string | null {
  const trimmed = path.replace(/[\\/]+$/, "");
  const i = trimmed.lastIndexOf("\\");
  if (i < 0) return null;
  if (i <= 2) return trimmed.length > 3 ? trimmed.slice(0, i + 1) : null;
  return trimmed.slice(0, i);
}

/** Cleans a typed path. Returns "" for an empty one, so callers can ignore it. */
export function normalizePath(p: string): string {
  let s = p.trim().replace(/\//g, "\\");
  // Collapse doubled separators (C:\\x, C://x), keeping the leading \\ of a UNC path.
  s = (s.startsWith("\\\\") ? "\\" : "") + s.replace(/\\{2,}/g, "\\");
  s = s.replace(/^[a-z]:/, (d) => d.toUpperCase());
  // A bare drive is its root: "C:" alone means the current directory on C:.
  if (/^[A-Z]:\\?$/.test(s)) return `${s.slice(0, 2)}\\`;
  return s.replace(/\\+$/, "");
}

/** A path in one comparable form: cleaned, lower case, no trailing separator ("C:\" is "c:"). */
export function pathKey(p: string): string {
  return normalizePath(p).toLowerCase().replace(/\\$/, "");
}

/** Whether `p` is `folder` or somewhere inside it. Both are pathKey()s. */
export function isSameOrInside(p: string, folder: string): boolean {
  return p === folder || p.startsWith(`${folder}\\`);
}

/** "C:" from a drive name or path, in the case DriveInfo.letter uses. */
export function driveKey(p: string): string {
  return p.slice(0, 2).toUpperCase();
}

/** A tab's name: the folder's own name, or the drive ("C:") for a drive root. */
export function folderTitle(path: string): string {
  const t = path.replace(/\\+$/, "");
  if (/^[A-Za-z]:$/.test(t)) return t.toUpperCase();
  return t.slice(t.lastIndexOf("\\") + 1) || path;
}

// ---- sorting lists ----------------------------------------------------------------------

// One collator for every sort: localeCompare with an options object builds a new one per call.
export const cmp = new Intl.Collator("ko", { numeric: true }).compare;

/** Each distinct text's place in `cmp` order; texts the collator finds equal ("01", "1") share one. */
function collationRanks(texts: Iterable<string>): Map<string, number> {
  const all = [...new Set(texts)].sort(cmp);
  const ranks = new Map<string, number>();
  let r = 0;
  for (let i = 0; i < all.length; i++) {
    if (i > 0 && cmp(all[i - 1], all[i]) !== 0) r++;
    ranks.set(all[i], r);
  }
  return ranks;
}

/**
 * Names in collation order, kept between sorts of one list (another column, the other way, a live
 * reload of the same folder): those sorts then compare numbers, not strings. Made again, for the
 * list sorted then (one list's names at most), only when a sort needs many names compared and
 * holds one it does not know.
 */
export interface NameOrder {
  ranks: Map<string, number> | null;
}

/** What a list sorts by: a number per item, a text per item (in `cmp` order), or the name. */
export type SortBy<T> = "name" | ((item: T) => number) | { text: (item: T) => string };

/**
 * `items` sorted by `by`, the other way when `desc`, then by name (A→Z either way), items the
 * collator finds equal staying as they came: the order of
 * `[...items].sort((a, b) => (key(a) - key(b)) * dir || cmp(a.name, b.name))`. `first`: items that
 * go before the others whichever way (folders, in name and type order).
 *
 * The keys are made once per item, not twice per comparison (a 100k folder by 변화 or 종류 took
 * seconds), and names are compared only where the keys are equal, through `names` when it knows
 * them all.
 */
export function sortItems<T extends { name: string }>(
  items: readonly T[],
  by: SortBy<T>,
  desc: boolean,
  opts: { first?: (item: T) => boolean; names?: NameOrder } = {},
): T[] {
  const n = items.length;
  const dir = desc ? -1 : 1;
  const group = new Uint8Array(n);
  if (opts.first) for (let i = 0; i < n; i++) group[i] = opts.first(items[i]) ? 0 : 1;
  // The names' places when `names` holds every one of them (null: not known).
  const known = (): Float64Array | null => {
    const ranks = opts.names?.ranks;
    if (!ranks) return null;
    const out = new Float64Array(n);
    for (let i = 0; i < n; i++) {
      const r = ranks.get(items[i].name);
      if (r === undefined) return null;
      out[i] = r;
    }
    return out;
  };
  const learn = (): Float64Array => {
    const ranks = collationRanks(items.map((e) => e.name));
    if (opts.names) opts.names.ranks = ranks;
    return Float64Array.from(items, (e) => ranks.get(e.name)!);
  };

  let key: Float64Array;
  if (by === "name") {
    key = known() ?? learn();
  } else if (typeof by === "function") {
    key = Float64Array.from(items, by);
  } else {
    const texts = items.map(by.text);
    const ranks = collationRanks(texts);
    key = Float64Array.from(texts, (t) => ranks.get(t)!);
  }
  const same = (a: number, b: number) => group[a] === group[b] && key[a] === key[b];

  const order = Array.from({ length: n }, (_, i) => i);
  order.sort((a, b) => group[a] - group[b] || (key[a] - key[b]) * dir || a - b);
  // By name, equal keys are names the collator finds equal: they stay as they came.
  if (by === "name") return order.map((i) => items[i]);

  // Runs of equal keys go by name.
  const runs: [number, number][] = [];
  // About how many name comparisons sorting the runs takes.
  let work = 0;
  for (let s = 0; s < n; ) {
    let e = s + 1;
    while (e < n && same(order[s], order[e])) e++;
    if (e - s > 1) {
      runs.push([s, e]);
      work += (e - s) * Math.log2(e - s);
    }
    s = e;
  }
  if (!runs.length) return order.map((i) => items[i]);
  // Through the names' places when they are known. Else they are placed (and kept for the next
  // sort) when the runs alone cost at least half as much as placing every name: most items tie
  // (종류, 변화, 파일 수).
  const names = known() ?? (opts.names && work * 2 >= n * Math.log2(n) ? learn() : null);
  if (names) {
    for (const [s, e] of runs) {
      const run = order.slice(s, e).sort((a, b) => names[a] - names[b] || a - b);
      for (let j = 0; j < run.length; j++) order[s + j] = run[j];
    }
    return order.map((i) => items[i]);
  }
  // A run is in the order the items came: the (stable) sort keeps equal names so.
  const out = order.map((i) => items[i]);
  for (const [s, e] of runs) {
    const run = out.slice(s, e).sort((a, b) => cmp(a.name, b.name));
    for (let j = 0; j < run.length; j++) out[s + j] = run[j];
  }
  return out;
}
