import { Fragment, type ReactNode } from "react";
import { shown } from "./format";
import "./v071.css";

// ---- AI answers as formatted text ------------------------------------------------------
//
// A small Markdown reader for what an AI writes: headings, paragraphs, lists (one level of nesting,
// fenced code under an item), **bold**, *italic*, `code`, fenced code, quotes, rules and simple tables.
// It builds React elements only (no HTML is ever parsed or injected), so a tag in an answer shows as
// text. Links show their text and address as plain text: nothing in an answer can be clicked or loaded
// (no images either). Paths and file names keep their characters (see LITERAL).

/** ANSI colour and cursor codes a CLI may leave in its output. */
const ANSI = /\u001b\[[0-?]*[ -/]*[@-~]|\u001b\][^\u0007\u001b]*(?:\u0007|\u001b\\)|\u001b[@-Z\\-_]/g;
/**
 * The same colour codes with the ESC already taken out ("[1m", "[0m"). Names like "[30min] 회의.mp4"
 * or "강의 [5m30s].mp4" start the same way, so these go only from text that also holds a bare reset
 * (then it is CLI output), and never one a "]" closes in the same word.
 */
const ANSI_BARE = /\[\d{1,3}(?:;\d{1,3})*m(?![^\s[\]]*\])/g;
const BARE_RESET = /\[(?:0|22|23|24|27|39|49)?m(?![^\s[\]]*\])/;
const stripBare = (t: string) => (BARE_RESET.test(t) ? t.replace(ANSI_BARE, "") : t);
/** Other control characters (line breaks and tabs stay). */
const CONTROL = /[\u0000-\u0008\u000B\u000C\u000E-\u001F\u007F]/g;
/** What a CLI prints about its own use, not part of the answer ("tokens used: 1,234"). */
const NOISE_LINE = /^\s*(?:tokens? used|total tokens|토큰 사용량?)(?![A-Za-z])\s*[:：]?\s*[\d,.\s]*$/i;
const NUMBER_LINE = /^\s*[\d,.]+\s*$/;
/** An answer that is one fenced block, nothing around it. */
const ONE_FENCE = /^(```|~~~)[ \t]*([\w+-]*)[ \t]*\n([\s\S]*?)\n?\1[ \t]*$/;
/** Fields an answer given as a JSON object may hold its text in. */
const TEXT_FIELDS = ["answer", "text", "content", "result", "message", "explanation", "summary"];

/**
 * The answer without what is around it: CLI colour codes, "tokens used" lines, control characters,
 * and a fence wrapped around the whole answer (a JSON one holding the text is read for the text).
 */
export function cleanAiText(raw: string): string {
  // A byte order mark a CLI may put first goes. Marks that would make a name in it read as
  // something else show (shown), as everywhere else.
  if (raw.charCodeAt(0) === 0xfeff) raw = raw.slice(1);
  let t = shown(stripBare(raw.replace(/\r\n?/g, "\n").replace(ANSI, "")).replace(CONTROL, ""));
  const lines = t.split("\n");
  const kept: string[] = [];
  for (let i = 0; i < lines.length; i++) {
    if (NOISE_LINE.test(lines[i])) {
      // "tokens used" and the number on the line after it.
      if (i + 1 < lines.length && NUMBER_LINE.test(lines[i + 1])) i++;
      continue;
    }
    kept.push(lines[i]);
  }
  t = kept.join("\n").trim();
  const fence = ONE_FENCE.exec(t);
  if (fence && !fence[3].includes(fence[1])) {
    const inner = fence[3].trim();
    const lang = fence[2].toLowerCase();
    const plain = lang === "" || lang === "text" || lang === "txt";
    if (lang === "json" || plain) {
      const fromJson = /^[[{"]/.test(inner) ? jsonText(inner) : null;
      // Escapes in the JSON (\u202E) come out as the characters themselves: made visible again.
      if (fromJson !== null) return shown(fromJson.replace(/\r\n?/g, "\n").replace(CONTROL, ""));
      // JSON that is not one text stays a code block: as paragraphs it would read worse.
      if (lang === "json" || /^[[{]/.test(inner)) return t;
    }
    if (plain || lang === "md" || lang === "markdown") return inner;
  }
  return t;
}

/** A JSON value that says nothing: null, "", [] or {}. */
const emptyJson = (x: unknown) =>
  x === null ||
  x === undefined ||
  (typeof x === "string" && !x.trim()) ||
  (Array.isArray(x) && x.length === 0) ||
  (typeof x === "object" && !Array.isArray(x) && Object.keys(x as object).length === 0);

/**
 * The text in a JSON answer: a string, or an object's one text field when nothing else in it says
 * anything (an object with more, such as "safe_to_delete", stays JSON: none of it is dropped unseen);
 * null when it is something else.
 */
function jsonText(s: string): string | null {
  let v: unknown;
  try {
    v = JSON.parse(s);
  } catch {
    return null;
  }
  if (typeof v === "string") return v.trim();
  if (v && typeof v === "object" && !Array.isArray(v)) {
    const o = v as Record<string, unknown>;
    for (const f of TEXT_FIELDS) {
      const x = o[f];
      if (typeof x !== "string" || !x.trim()) continue;
      return Object.keys(o).every((k) => k === f || emptyJson(o[k])) ? x.trim() : null;
    }
  }
  return null;
}

// ---- blocks ----

type Align = "left" | "center" | "right" | null;

/** A run of an item's lines, or a fenced code block indented under it. */
type ItemPart = { kind: "text"; lines: string[] } | { kind: "code"; lang: string; text: string };

interface ListItem {
  /** Its text and the code blocks in it, in order. */
  parts: ItemPart[];
  children: ListBlock | null;
}

/** A line of an item's text, after what it already has. */
function addLine(item: ListItem, line: string) {
  const p = item.parts[item.parts.length - 1];
  if (p?.kind === "text") p.lines.push(line);
  else item.parts.push({ kind: "text", lines: [line] });
}

interface ListBlock {
  kind: "list";
  ordered: boolean;
  start: number;
  items: ListItem[];
}

type Block =
  | { kind: "heading"; level: 1 | 2 | 3; text: string }
  | { kind: "para"; lines: string[] }
  | { kind: "code"; lang: string; text: string }
  | { kind: "quote"; blocks: Block[] }
  | { kind: "rule" }
  | { kind: "table"; head: string[]; align: Align[]; rows: string[][] }
  | ListBlock;

const FENCE = /^\s{0,3}(```+|~~~+)\s*([\w+-]*)[^`]*$/;
/** A heading; closing #s go only after a space ("## About C#" keeps its #). */
const HEADING = /^\s{0,3}(#{1,6})\s+(.*?)(?:\s+#+)?\s*$/;
const RULE = /^\s{0,3}([-*_])(?:\s*\1){2,}\s*$/;
const QUOTE = /^\s{0,3}>\s?/;
const MARKER = /^(\s*)([-*+•]|\d{1,3}[.)])\s+(.*)$/;
const TABLE_SEP = /^\s*\|?\s*:?-{2,}:?\s*(?:\|\s*:?-{2,}:?\s*)*\|?\s*$/;

const indentOf = (s: string) => {
  let n = 0;
  for (const c of s) {
    if (c === " ") n++;
    else if (c === "\t") n += 4;
    else break;
  }
  return n;
};

const isTableStart = (lines: string[], i: number) =>
  lines[i].includes("|") && i + 1 < lines.length && TABLE_SEP.test(lines[i + 1]) && lines[i + 1].includes("-");

/** A line that starts a block of another kind (it ends a paragraph or a list). */
const startsBlock = (lines: string[], i: number) =>
  FENCE.test(lines[i]) || HEADING.test(lines[i]) || RULE.test(lines[i]) || QUOTE.test(lines[i]) || isTableStart(lines, i);

/** The line that ends a fenced block opened with `mark`. */
const closesFence = (line: string, mark: string) => {
  const t = line.trim();
  return t.startsWith(mark[0].repeat(3)) && t.replace(/[`~]/g, "") === "";
};

/** The line without up to `n` of its leading spaces. */
const dedent = (line: string, n: number) => {
  let k = 0;
  while (k < n && line[k] === " ") k++;
  return line.slice(k);
};

function cells(line: string): string[] {
  let t = line.trim();
  if (t.startsWith("|")) t = t.slice(1);
  if (t.endsWith("|")) t = t.slice(0, -1);
  return t.split("|").map((c) => c.trim());
}

function parseBlocks(lines: string[], depth = 0): Block[] {
  const out: Block[] = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    if (!line.trim()) {
      i++;
      continue;
    }
    const fence = FENCE.exec(line);
    if (fence) {
      const mark = fence[1];
      const body: string[] = [];
      i++;
      while (i < lines.length && !closesFence(lines[i], mark)) {
        body.push(lines[i]);
        i++;
      }
      i++; // the closing fence (or the end)
      out.push({ kind: "code", lang: fence[2], text: body.join("\n") });
      continue;
    }
    const h = HEADING.exec(line);
    if (h) {
      out.push({ kind: "heading", level: Math.min(3, h[1].length) as 1 | 2 | 3, text: h[2] });
      i++;
      continue;
    }
    if (RULE.test(line)) {
      out.push({ kind: "rule" });
      i++;
      continue;
    }
    if (QUOTE.test(line)) {
      const inner: string[] = [];
      while (i < lines.length && lines[i].trim() && QUOTE.test(lines[i])) {
        inner.push(lines[i].replace(QUOTE, ""));
        i++;
      }
      out.push(depth < 3 ? { kind: "quote", blocks: parseBlocks(inner, depth + 1) } : { kind: "para", lines: inner });
      continue;
    }
    if (isTableStart(lines, i)) {
      const head = cells(line);
      const align: Align[] = cells(lines[i + 1]).map((c) =>
        c.startsWith(":") && c.endsWith(":") ? "center" : c.endsWith(":") ? "right" : c.startsWith(":") ? "left" : null,
      );
      const rows: string[][] = [];
      i += 2;
      while (i < lines.length && lines[i].trim() && lines[i].includes("|")) {
        rows.push(cells(lines[i]));
        i++;
      }
      out.push({ kind: "table", head, align, rows });
      continue;
    }
    if (MARKER.test(line)) {
      const [list, next] = parseList(lines, i);
      out.push(list);
      i = next;
      continue;
    }
    const para: string[] = [];
    while (i < lines.length && lines[i].trim() && (para.length === 0 || (!startsBlock(lines, i) && !MARKER.test(lines[i])))) {
      para.push(lines[i].trim());
      i++;
    }
    out.push({ kind: "para", lines: para });
  }
  return out;
}

const markerInfo = (m: RegExpExecArray) => {
  const ordered = /\d/.test(m[2]);
  return { ordered, start: ordered ? parseInt(m[2], 10) : 1 };
};

/** A list from line `from`, with one level of nesting (deeper items join that level). */
function parseList(lines: string[], from: number): [ListBlock, number] {
  const first = MARKER.exec(lines[from])!;
  const base = indentOf(first[1]);
  const list: ListBlock = { kind: "list", ...markerInfo(first), items: [] };
  /** Where text that goes on belongs: the last item, or the last item of its sub-list. */
  let last: ListItem | null = null;
  let blank = false;
  let i = from;
  for (; i < lines.length; i++) {
    const line = lines[i];
    if (!line.trim()) {
      blank = true;
      continue;
    }
    const m = MARKER.exec(line);
    const indent = indentOf(line);
    // A fenced block indented under an item is the item's own (a command to run, say).
    const fence = last && indent > base ? FENCE.exec(line.trim()) : null;
    if (last && fence) {
      const body: string[] = [];
      for (i++; i < lines.length && !closesFence(lines[i], fence[1]); i++) body.push(dedent(lines[i], indent));
      last.parts.push({ kind: "code", lang: fence[2], text: body.join("\n") });
      blank = false;
      // On the closing fence (the loop steps past it), or at the end of the text.
      if (i >= lines.length) break;
      continue;
    }
    if (m && !RULE.test(line)) {
      if (indent < base) break;
      const item: ListItem = { parts: [{ kind: "text", lines: [m[3]] }], children: null };
      if (indent <= base + 1 || list.items.length === 0) {
        // Bullets after numbers (or the other way round) are a list of their own.
        if (markerInfo(m).ordered !== list.ordered) break;
        list.items.push(item);
      } else {
        const parent = list.items[list.items.length - 1];
        if (!parent.children) parent.children = { kind: "list", ...markerInfo(m), items: [] };
        parent.children.items.push(item);
      }
      last = item;
      blank = false;
      continue;
    }
    // Text that goes on from an item: indented under it, or right after it (no blank line between).
    if (last && (indent > base || (!blank && !startsBlock(lines, i)))) {
      if (blank && last.parts[last.parts.length - 1]?.kind === "text") addLine(last, "");
      addLine(last, line.trim());
      blank = false;
      continue;
    }
    break;
  }
  // Blank lines the list ended on are not its own.
  while (i > from && !lines[i - 1]?.trim()) i--;
  return [list, i];
}

// ---- inline ----

/**
 * `code`, **bold**, __bold__, ~~struck~~, [text](address), <address>, *italic*, _italic_. Underscores
 * count only at word edges (Hangul is a word too), never against a backslash, a slash or a dot with
 * a name after it, and a star before a dot is a wildcard (*.tmp), so names show as written
 * (my_file_name.txt, 회의_최종_본.hwp, _draft_.txt, /srv/_old_/x).
 */
const INLINE =
  /(`+)([\s\S]*?[^`])\1(?!`)|\*\*(?=\S)([\s\S]*?\S)\*\*|(?<![\p{L}\p{N}_\\/.])__(?=\S)([\s\S]*?\S)__(?![\p{L}\p{N}_\\/]|\.[\p{L}\p{N}])|~~(?=\S)([\s\S]*?\S)~~|(!?)\[([^\]\n]*)\]\(\s*<?([^)\s>]*)>?(?:\s+["'][^"'\n]*["'])?\s*\)|<((?:https?|mailto|file):[^>\s]+)>|(?<![\w*])\*(?=[^\s*.])([^*\n]*?[^\s*\\])\*(?![\w*])|(?<![\p{L}\p{N}_\\/.])_(?=[^\s_])([^_\n]*?[^\s_])_(?![\p{L}\p{N}_\\/]|\.[\p{L}\p{N}])/gu;

/**
 * What shows as written, its marks not read: a path with a backslash or a drive (C:\proj\__init__.py,
 * D:\사진\_정리_\IMG_0001.jpg, D:\문서\[최종](수정).hwp, C:/x), a Python name (__init__, __pycache__),
 * and a file name with marks in it ([최종](수정).hwp, ~~old~~.txt). Stars and backticks are no part
 * of one, so **C:\temp** is still bold and `a_b.txt` still code.
 */
const LITERAL =
  /[^\s*`"<>|]*\\[^\s*`"<>|]*|(?<![\p{L}\p{N}])[A-Za-z]:\/[^\s*`"<>|]*|(?<![A-Za-z0-9_])__[A-Za-z0-9]\w*__(?![A-Za-z0-9_])|[^\s*`"'<>|/:\\]*[_~[\]()][^\s*`"'<>|/:\\]*\.[A-Za-z0-9]{1,8}(?![A-Za-z0-9_)\]])/gu;

/** Private-use characters: what stands for a literal while the marks are read. */
const PUA_BASE = 0xe000;
const PUA_SIZE = 0x1900;
const PUA = /[\uE000-\uF8FF]/;
const PUA_ALL = /[\uE000-\uF8FF]/g;

/** A line's marks as elements, with names and paths in it kept as written (LITERAL). */
function inline(text: string, key: string): ReactNode[] {
  const kept: string[] = [];
  // Text that holds such characters itself (an icon font's) is read as it is.
  const t = PUA.test(text)
    ? text
    : text.replace(LITERAL, (s) => (kept.length < PUA_SIZE ? String.fromCharCode(PUA_BASE + kept.push(s) - 1) : s));
  const back = (s: string) => (kept.length ? s.replace(PUA_ALL, (c) => kept[c.charCodeAt(0) - PUA_BASE] ?? c) : s);
  return marks(t, key, 0, back);
}

function marks(text: string, key: string, depth: number, back: (s: string) => string): ReactNode[] {
  if (depth > 4) return [back(text)];
  const out: ReactNode[] = [];
  let at = 0;
  let n = 0;
  const re = new RegExp(INLINE.source, INLINE.flags);
  for (let m = re.exec(text); m; m = re.exec(text)) {
    if (m.index > at) out.push(back(text.slice(at, m.index)));
    const k = `${key}.${n++}`;
    if (m[1] !== undefined) {
      let code = back(m[2]).replace(/\n/g, " ");
      if (code.length > 2 && code.startsWith(" ") && code.endsWith(" ")) code = code.slice(1, -1);
      out.push(
        <code key={k} className="md-code">
          {code}
        </code>,
      );
    } else if (m[3] !== undefined || m[4] !== undefined) {
      out.push(<strong key={k}>{marks(m[3] ?? m[4], k, depth + 1, back)}</strong>);
    } else if (m[5] !== undefined) {
      out.push(<s key={k}>{marks(m[5], k, depth + 1, back)}</s>);
    } else if (m[7] !== undefined) {
      const label = m[7].trim();
      const url = back(m[8]);
      out.push(
        <span key={k} className="md-link">
          {m[6] ? "그림: " : ""}
          {label ? marks(label, k, depth + 1, back) : null}
          {url && url !== back(label) ? (
            <span className="md-url" title="AI가 적은 주소예요 (눌러도 열리지 않아요)">
              {label || m[6] ? ` (${url})` : url}
            </span>
          ) : null}
        </span>,
      );
    } else if (m[9] !== undefined) {
      out.push(
        <span key={k} className="md-url" title="AI가 적은 주소예요 (눌러도 열리지 않아요)">
          {back(m[9])}
        </span>,
      );
    } else if (m[10] !== undefined || m[11] !== undefined) {
      out.push(<em key={k}>{marks(m[10] ?? m[11], k, depth + 1, back)}</em>);
    }
    at = m.index + m[0].length;
  }
  if (at < text.length) out.push(back(text.slice(at)));
  return out;
}

/** Lines of one paragraph or item, their breaks kept (an AI's line breaks are usually meant). */
function lineRun(lines: string[], key: string): ReactNode[] {
  const out: ReactNode[] = [];
  lines.forEach((l, i) => {
    if (i > 0) out.push(<br key={`${key}.br${i}`} />);
    out.push(<Fragment key={`${key}.l${i}`}>{inline(l, `${key}.l${i}`)}</Fragment>);
  });
  return out;
}

function ListView({ list, k }: { list: ListBlock; k: string }) {
  const items = list.items.map((it, i) => (
    <li key={i}>
      {it.parts.map((p, j) =>
        p.kind === "text" ? (
          <Fragment key={j}>{lineRun(p.lines, `${k}.${i}.${j}`)}</Fragment>
        ) : (
          <pre key={j} className="md-pre" data-lang={p.lang || undefined}>
            {p.text}
          </pre>
        ),
      )}
      {it.children && <ListView list={it.children} k={`${k}.${i}c`} />}
    </li>
  ));
  return list.ordered ? (
    <ol className="md-list" start={list.start !== 1 ? list.start : undefined}>
      {items}
    </ol>
  ) : (
    <ul className="md-list">{items}</ul>
  );
}

function BlockView({ b, k }: { b: Block; k: string }) {
  switch (b.kind) {
    case "heading": {
      const Tag = b.level === 1 ? "h4" : b.level === 2 ? "h5" : "h6";
      return <Tag className={`md-h md-h${b.level}`}>{inline(b.text, k)}</Tag>;
    }
    case "para":
      return <p className="md-p">{lineRun(b.lines, k)}</p>;
    case "code":
      return (
        <pre className="md-pre" data-lang={b.lang || undefined}>
          {b.text}
        </pre>
      );
    case "quote":
      return (
        <blockquote className="md-quote">
          {b.blocks.map((x, i) => (
            <BlockView key={i} b={x} k={`${k}.${i}`} />
          ))}
        </blockquote>
      );
    case "rule":
      return <hr className="md-rule" />;
    case "table":
      return (
        <div className="md-table-wrap">
          <table className="md-table">
            <thead>
              <tr>
                {b.head.map((c, i) => (
                  <th key={i} style={b.align[i] ? { textAlign: b.align[i]! } : undefined}>
                    {inline(c, `${k}.h${i}`)}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {b.rows.map((r, i) => (
                <tr key={i}>
                  {b.head.map((_, j) => (
                    <td key={j} style={b.align[j] ? { textAlign: b.align[j]! } : undefined}>
                      {inline(r[j] ?? "", `${k}.${i}.${j}`)}
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      );
    case "list":
      return <ListView list={b} k={k} />;
  }
}

/** An AI's answer as formatted text (see the top of this file for what is read). */
export function Markdown({ text, className }: { text: string; className?: string }) {
  const blocks = parseBlocks(cleanAiText(text).split("\n"));
  return (
    <div className={`md${className ? ` ${className}` : ""}`}>
      {blocks.map((b, i) => (
        <BlockView key={i} b={b} k={`b${i}`} />
      ))}
    </div>
  );
}

/**
 * One line of AI or MCP text (a step's reason) with its inline marks, on one line. Not for what the
 * app writes itself (a rule's reason): that is plain text, its paths and names shown as they are.
 */
export function MarkdownInline({ text }: { text: string }) {
  const t = shown(stripBare(text.replace(ANSI, "")).replace(CONTROL, " ")).replace(/\s*\n\s*/g, " ");
  return <>{inline(t, "i")}</>;
}
