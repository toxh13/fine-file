import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { UnlistenFn } from "@tauri-apps/api/event";
import { ai, planEvents, plans, type Plan, type PlanProgress, type PlanStep, type Risk } from "./api";
import { Dialog } from "./Duplicates";
import { driveKey, folderTitle, formatDate, formatSize, parentOf } from "./format";
import { Markdown, MarkdownInline } from "./markdown";
import "./components.css";
import "./v04.css";
import "./v06.css";
import "./v071.css";

type Action = PlanStep["action"];
type Level = PlanStep["risk"];
type Notice = { text: string; error?: boolean };

const ACTION_LABEL: Record<Action, string> = {
  mkdir: "새 폴더",
  move: "옮기기",
  rename: "이름 바꾸기",
  trash: "휴지통으로",
  compress: "압축",
  relocate: "다른 드라이브로",
};

/** What undoing a done step of each action does. */
const UNDO_LABEL: Record<Action, string> = {
  mkdir: "만든 폴더",
  move: "원래 자리로 옮기기",
  rename: "원래 이름으로",
  trash: "휴지통에서 되살리기",
  compress: "압축 풀기",
  relocate: "원래 드라이브로 되돌리기",
};

/** The order counts are listed in: what removes things first. */
const ACTION_ORDER: Action[] = ["trash", "relocate", "move", "rename", "compress", "mkdir"];

const RISK_LABEL: Record<Level, string> = { safe: "안전", caution: "주의", danger: "위험" };

const STEP_LABEL: Record<PlanStep["state"], string> = {
  pending: "대기",
  done: "완료",
  failed: "실패",
  skipped: "건너뜀",
  undone: "되돌림",
};

const STATE_LABEL: Record<Plan["state"], string> = {
  draft: "초안",
  running: "실행 중",
  done: "완료",
  partly: "일부 완료",
  undone: "되돌림",
  failed: "실패",
};

const KIND_LABEL: Record<Plan["kind"], string> = {
  structure: "구조 정리",
  rename: "이름 정리",
  dedupe: "중복 정리",
  shrink: "용량 줄이기",
  relocate: "다른 드라이브로 옮기기",
  cleanup: "정리 후보",
};

const SOURCE_LABEL: Record<Plan["source"], { label: string; title: string }> = {
  rules: { label: "규칙", title: "AI 없이 규칙으로 만든 계획" },
  claude: { label: "Claude", title: "Claude API가 제안한 계획" },
  openai: { label: "OpenAI", title: "OpenAI API가 제안한 계획" },
  ollama: { label: "Ollama", title: "Ollama(내 PC)가 제안한 계획" },
  claude_cli: { label: "CLI", title: "Claude Code CLI가 제안한 계획" },
  codex_cli: { label: "CLI", title: "Codex CLI가 제안한 계획" },
  mcp: { label: "MCP", title: "Claude 앱이 MCP로 제안한 계획" },
};

/** Steps that leave room on the drive: what goes to the Recycle Bin (once it is emptied) and what moves to another drive. */
const FREES = new Set<Action>(["trash", "relocate"]);

/** Debounce of saving the selection. */
const SAVE_MS = 600;

/** Items of a plan jev is asked about (a plan holds no more steps than this). */
const JEV_MAX = 500;
/** Items per jev_risk call: it answers for no more than this many (the rest would get no badge). */
const JEV_CALL = 400;

function errText(err: unknown): string {
  if (typeof err === "string") return err;
  if (err instanceof Error) return err.message;
  try {
    return JSON.stringify(err) ?? String(err);
  } catch {
    return String(err);
  }
}

// Bidi controls in a name can make it read as something else ("exe.txt" shown as "txt.exe"). Names,
// reasons and summaries from AI or MCP are untrusted: they show as text, with such marks made visible.
const BIDI = /[\u202A-\u202E\u2066-\u2069\u200E\u200F\u061C]/g;
const shown = (s: string) => s.replace(BIDI, "\uFFFD");

function baseName(p: string): string {
  const t = p.replace(/[\\/]+$/, "");
  const i = t.lastIndexOf("\\");
  return i < 0 ? t : t.slice(i + 1);
}

function dirOf(p: string): string {
  return parentOf(p) ?? "";
}

const isAiSource = (s: Plan["source"]) => s !== "rules";

function SourceTag({ source }: { source: Plan["source"] }) {
  const s = SOURCE_LABEL[source] ?? { label: source, title: source };
  const kind = source === "rules" ? "rules" : source === "mcp" ? "mcp" : "ai";
  return (
    <span className={`v06-src src-${kind}`} title={s.title}>
      {s.label}
    </span>
  );
}

function StateTag({ state }: { state: Plan["state"] }) {
  return <span className={`v06-state st-${state}`}>{STATE_LABEL[state] ?? state}</span>;
}

function sumSize(steps: PlanStep[]): number {
  return steps.reduce((n, s) => n + (s.action === "mkdir" ? 0 : s.size), 0);
}

/** Selected steps still to run, by action, in ACTION_ORDER. */
function countByAction(steps: PlanStep[]): { action: Action; count: number; bytes: number }[] {
  const m = new Map<Action, { count: number; bytes: number }>();
  for (const s of steps) {
    const c = m.get(s.action) ?? { count: 0, bytes: 0 };
    c.count++;
    if (s.action !== "mkdir") c.bytes += s.size;
    m.set(s.action, c);
  }
  const known = ACTION_ORDER.filter((a) => m.has(a)).map((a) => ({ action: a, ...m.get(a)! }));
  const other = [...m].filter(([a]) => !ACTION_ORDER.includes(a)).map(([action, c]) => ({ action, ...c }));
  return [...known, ...other];
}

/** Where a step's item is now: the folder to go to for it. */
function stepFolder(s: PlanStep): string {
  const at = s.state === "done" && s.to && s.action !== "relocate" ? s.to : s.from;
  return parentOf(at) ?? at;
}

// ---- how a step reads: a sentence with names, its folder, the before/after trees ----

/** The cards over the steps, in this order. */
const CARD_ORDER: Action[] = ["mkdir", "move", "rename", "trash", "compress", "relocate"];
const CARD_LABEL: Record<Action, string> = {
  mkdir: "새 폴더",
  move: "옮기기",
  rename: "이름 바꾸기",
  trash: "휴지통",
  compress: "압축",
  relocate: "다른 드라이브로",
};

/** Steps that change where things are or what they are called: the plan has a before and an after to show. */
const SHAPES = new Set<Action>(["mkdir", "move", "rename"]);

const JEV_TIP =
  "TypeSafe jev가 이 계획의 항목마다 지워도 되는지(안전·주의·위험)와 프로그램이 다시 만드는 항목인지를 한 번 더 판정해요. " +
  "계획을 만든 규칙·AI와 따로 보는 빠르고 저렴한 2차 의견이에요.\n" +
  "· 보내는 것: 항목의 경로·이름·종류·크기·날짜·분류 (파일 내용은 보내지 않아요)\n" +
  "· 요금: 내 jev API 키 주인에게 jev의 조건대로 청구돼요\n" +
  '· 결과: 단계의 위험 배지 옆에 "jev …" 배지로 붙어요. 참고용이고, 단계를 대신 고르지 않아요';

/** The path's folders under the plan's root ([] for the root itself); null when it is not under it. */
function underRoot(root: string, p: string): string[] | null {
  const r = root.replace(/[\\/]+$/, "").toLowerCase();
  const t = p.replace(/[\\/]+$/, "");
  if (t.toLowerCase() === r) return [];
  if (!t.toLowerCase().startsWith(`${r}\\`)) return null;
  return t
    .slice(r.length + 1)
    .split(/[\\/]+/)
    .filter(Boolean);
}

/** A folder as the plan's steps name it: relative to the root, the root itself, or in full when it is elsewhere. */
function folderLabel(root: string, folder: string): string {
  const segs = underRoot(root, folder);
  if (!segs) return folder;
  return segs.length ? segs.join("\\") : `${folderTitle(root)} (맨 위 폴더)`;
}

/**
 * The particle after a name: 을/를, (으)로. Read off a last Hangul syllable or digit; other endings
 * (a file's extension, mostly) get both forms, as in "'보고서.pdf'을(를)".
 */
function josa(word: string, pair: "을" | "로"): string {
  const last = word.replace(/[)\]}>'"’”\s]+$/, "").slice(-1);
  const c = last.charCodeAt(0);
  // 0: no final consonant, 8: ㄹ, anything else: another one.
  let fin: number | null = null;
  if (c >= 0xac00 && c <= 0xd7a3) fin = (c - 0xac00) % 28;
  else if (/^[0-9]$/.test(last)) fin = [1, 8, 0, 1, 0, 0, 1, 8, 8, 0][Number(last)];
  if (pair === "을") return fin === null ? "을(를)" : fin ? "을" : "를";
  return fin === null ? "(으)로" : fin && fin !== 8 ? "으로" : "로";
}

function ActIcon({ action }: { action: Action }) {
  const d: Record<Action, string> = {
    mkdir: "M1.5 4.5v8a1 1 0 0 0 1 1h11a1 1 0 0 0 1-1V6a1 1 0 0 0-1-1H8L6.5 3.5h-4a1 1 0 0 0-1 1zM8 7.5v4M6 9.5h4",
    move: "M2 8h8.5M7.5 4.5 11 8l-3.5 3.5M14 3v10",
    rename: "M10.5 2.5l3 3L6 13H3v-3zM9 4l3 3",
    trash: "M2.5 4.5h11M6.5 4.5V3h3v1.5M4 4.5l.7 8.6a1 1 0 0 0 1 .9h4.6a1 1 0 0 0 1-.9l.7-8.6",
    compress: "M8 1.5v5M5.5 4 8 6.5 10.5 4M8 14.5v-5M5.5 12 8 9.5 10.5 12M2.5 8h11",
    relocate: "M2 9.5h12v3a1 1 0 0 1-1 1H3a1 1 0 0 1-1-1zM2 9.5 4 3.5h8l2 6M11 11.5h.01",
  };
  return (
    <svg className="plan-ico" width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d={d[action] ?? "M3 8h10"} />
    </svg>
  );
}

function ActBadge({ action }: { action: Action }) {
  return (
    <span className={`v06-act act-${action} plan-act`}>
      <ActIcon action={action} />
      {ACTION_LABEL[action] ?? action}
    </span>
  );
}

/** A name in a sentence: in quotes, and a way to the folder its item is in. */
function NameRef({ s, name, onNavigate }: { s: PlanStep; name: string; onNavigate(path: string): void }) {
  return (
    <>
      &lsquo;
      <button className="plan-name" title={`${s.from}\n이 항목이 있는 폴더로 가기`} onClick={() => onNavigate(stepFolder(s))}>
        {shown(name)}
      </button>
      &rsquo;
    </>
  );
}

const Quoted = ({ text }: { text: string }) => <b className="plan-q">&lsquo;{shown(text)}&rsquo;</b>;

/** What a step does, as a sentence with names instead of full paths. */
function StepSentence({ s, root, onNavigate }: { s: PlanStep; root: string; onNavigate(path: string): void }) {
  const name = baseName(s.from);
  const ref = <NameRef s={s} name={name} onNavigate={onNavigate} />;
  switch (s.action) {
    case "mkdir":
      return (
        <>
          새 폴더 {ref} 만들기
        </>
      );
    case "trash":
      return (
        <>
          {ref}
          {josa(name, "을")} 휴지통으로 보내기
        </>
      );
    case "compress":
      return (
        <>
          {ref}
          {josa(name, "을")} 그 자리에서 압축하기 <span className="muted">(풀면 그대로예요)</span>
        </>
      );
    case "relocate":
      return (
        <>
          {ref} 폴더를{" "}
          {s.to ? (
            <>
              <b className="plan-q">{driveKey(s.to)}</b> 드라이브로
            </>
          ) : (
            "다른 드라이브로"
          )}{" "}
          옮기고 원래 자리에 링크 남기기
        </>
      );
    case "rename":
    case "move": {
      if (!s.to) return <>{ref}</>;
      const newName = baseName(s.to);
      const sameDir = dirOf(s.to).toLowerCase() === dirOf(s.from).toLowerCase();
      if (sameDir) {
        return (
          <>
            {ref}
            {josa(name, "을")} <Quoted text={newName} />
            {josa(newName, "로")} 이름 바꾸기
          </>
        );
      }
      const dest = dirOf(s.to);
      const segs = underRoot(root, dest);
      const where = segs && segs.length === 0 ? <>맨 위 폴더로</> : <><Quoted text={segs ? segs.join("\\") : dest} /> 폴더로</>;
      const renamed = newName !== name;
      return (
        <>
          {ref}
          {josa(name, "을")} {where} 옮기기
          {renamed && (
            <>
              {" "}
              (이름은 <Quoted text={newName} />
              {josa(newName, "로")})
            </>
          )}
        </>
      );
    }
    default:
      return <>{ref}</>;
  }
}

/** The full paths of a step, for its tooltip. */
function pathsTip(s: PlanStep): string {
  if (s.action === "mkdir") return `새로 만들 폴더: ${s.from}`;
  if (s.action === "trash") return `${s.from}\n→ 휴지통`;
  return s.to ? `${s.from}\n→ ${s.to}` : s.from;
}

interface StepGroup {
  key: string;
  folder: string;
  steps: PlanStep[];
}

/** Steps by the folder their item is in, the folders in the order the plan first names them. */
function groupSteps(steps: PlanStep[]): StepGroup[] {
  const m = new Map<string, StepGroup>();
  for (const s of steps) {
    const folder = parentOf(s.from) ?? s.from;
    const key = folder.toLowerCase();
    let g = m.get(key);
    if (!g) {
      g = { key, folder, steps: [] };
      m.set(key, g);
    }
    g.steps.push(s);
  }
  return [...m.values()];
}

/** A step a run could not do, or one an undo could not put back: its error in the step list says why. */
const troubled = (s: PlanStep) => s.state === "failed" || (s.state === "done" && !!s.error?.startsWith("되돌리지 못했어요"));

/** Where the app's own lines start in an AI or MCP plan's summary (plans.rs adds them after the AI's text). */
const APP_LINES = /(?:^|\n\n)(?=올바르지 않아서 뺀 단계 \d+개: |단계가 너무 많아서 앞의 \d+개만 남겼어요)/;

/** An AI or MCP plan's summary: what the AI wrote (Markdown), and what the app added (plain text, with paths). */
function splitSummary(summary: string): [string, string] {
  const m = APP_LINES.exec(summary);
  return m ? [summary.slice(0, m.index), summary.slice(m.index + m[0].length)] : [summary, ""];
}

// The before/after trees: the items the steps name and the folders above them, with the chosen steps
// applied to the after one. Nothing is read from the disk: what the plan does not name is not shown.

type Mark = "new" | "moved" | "renamed" | "trash" | "compress" | "relocate" | "out";

interface TreeNode {
  name: string;
  /** null: not known (an item no step says more of). */
  dir: boolean | null;
  mark?: Mark;
  /** Before: what a chosen step will do to it. */
  change?: Action;
  note?: string;
  kids: Map<string, TreeNode>;
}

interface Entry {
  segs: string[];
  dir: boolean | null;
  mark?: Mark;
  change?: Action;
  note?: string;
}

/** An item of the after tree, where the chosen steps before leave it. */
interface Live extends Entry {
  /** The same item in the before tree; none for a folder a step makes. */
  origin?: Entry;
  /** A step of its own put it in another folder (not only one that moved the folder it is in). */
  moved?: boolean;
  /** Not where it was any more: in the Recycle Bin, or moved out of the plan's folder. */
  gone?: boolean;
}

const segKey = (segs: string[]) => segs.join("\\").toLowerCase();
const node = (name: string, dir: boolean | null): TreeNode => ({ name, dir, kids: new Map() });
/** Whether `segs` is inside the folder `dir` (not the folder itself). */
const inDir = (segs: string[], dir: string[]) => segs.length > dir.length && segKey(segs.slice(0, dir.length)) === segKey(dir);

function treeOf(rootName: string, entries: Entry[]): TreeNode {
  const top = node(rootName, true);
  for (const e of entries) {
    let n = top;
    e.segs.forEach((seg, i) => {
      n.dir = true;
      const k = seg.toLowerCase();
      let c = n.kids.get(k);
      if (!c) {
        c = node(seg, null);
        n.kids.set(k, c);
      }
      if (i === e.segs.length - 1) {
        c.name = seg;
        if (e.dir !== null) c.dir = e.dir;
        c.mark = e.mark;
        c.change = e.change;
        c.note = e.note;
      }
      n = c;
    });
  }
  return top;
}

/** A chosen step that is (or was) done: what the after tree shows. */
const applied = (s: PlanStep) => s.selected && (s.state === "pending" || s.state === "done");

/**
 * A step the trees show: not one the plan dropped with why (its item is missing, the name is taken…),
 * which does nothing. One skipped only because it was not chosen when the plan ran still shows.
 */
const inTrees = (s: PlanStep) => !(s.state === "skipped" && s.error);

/** In the after tree, what an item a step moved says of where it was and what it was called. */
function liveNote(e: Live): string | undefined {
  const was = e.origin?.segs;
  if (!was) return undefined;
  const notes: string[] = [];
  const dir = was.slice(0, -1);
  if (e.moved && segKey(dir) !== segKey(e.segs.slice(0, -1))) {
    notes.push(dir.length ? `'${dir.join("\\")}'에서 옮겨 옴` : "맨 위 폴더에서 옮겨 옴");
  }
  const name = was[was.length - 1];
  if (name !== e.segs[e.segs.length - 1]) notes.push(`원래 이름 '${name}'`);
  return notes.join(" · ") || undefined;
}

/**
 * The trees, read off the steps in their order, as the backend checks them (plans.rs `Made`): a
 * step's item is one an earlier step makes or moves there, or else one on disk now. Only the ones on
 * disk are in the before tree: one an earlier step makes (a folder, a moved or renamed item) is not
 * there yet, and one in a folder an earlier step moves is where that folder is now ("B\x" after
 * "A → B" is "A\x"). The after tree applies the chosen steps to those, one after another.
 */
function beforeAfter(plan: Plan): { before: TreeNode; after: TreeNode } {
  const rootName = folderTitle(plan.root);
  const steps = plan.steps.filter(inTrees);

  const now: Entry[] = [];
  const onDisk = new Map<string, Entry>();
  /** What the earlier steps make (folders, where they move items to), and their moves. */
  const made = new Set<string>();
  const moves: [string[], string[]][] = [];
  for (const s of steps) {
    const from = underRoot(plan.root, s.from);
    if (!from?.length) continue;
    const to = s.to ? underRoot(plan.root, s.to) : null;
    if (s.action !== "mkdir" && !made.has(segKey(from))) {
      let at = from;
      for (let i = moves.length - 1; i >= 0; i--) {
        const [a, b] = moves[i];
        if (inDir(at, b)) at = [...a, ...at.slice(b.length)];
      }
      const k = segKey(at);
      // Nothing is on disk yet in a folder a step makes.
      const inMade = at.some((_, i) => made.has(segKey(at.slice(0, i + 1))));
      if (!inMade && !onDisk.has(k)) {
        const e: Entry = { segs: at, dir: s.action === "relocate" ? true : null };
        onDisk.set(k, e);
        now.push(e);
      }
    }
    if (s.action === "mkdir") made.add(segKey(from));
    else if ((s.action === "move" || s.action === "rename") && to?.length) {
      made.add(segKey(to));
      moves.push([from, to]);
    }
  }

  const live: Live[] = now.map((e) => ({ segs: [...e.segs], dir: e.dir, origin: e }));
  const find = (segs: string[]) => {
    const k = segKey(segs);
    return live.find((e) => !e.gone && segKey(e.segs) === k);
  };
  const inside = (segs: string[]) => live.filter((x) => !x.gone && inDir(x.segs, segs));
  for (const s of steps) {
    if (!applied(s)) continue;
    const from = underRoot(plan.root, s.from);
    if (!from?.length) continue;
    // Not there by then (an earlier step that would bring it is not chosen): the step does nothing.
    const e = find(from);
    if (e?.origin && !e.origin.change && s.action !== "mkdir") e.origin.change = s.action;
    switch (s.action) {
      case "mkdir":
        if (e) e.dir = true;
        else live.push({ segs: from, dir: true, mark: "new" });
        break;
      case "move":
      case "rename": {
        if (!e) break;
        const to = s.to ? underRoot(plan.root, s.to) : null;
        const kids = inside(from);
        if (!to?.length) {
          e.mark = "out";
          e.note = s.to ? `→ ${s.to}` : undefined;
          e.gone = true;
          for (const x of kids) x.gone = true;
          break;
        }
        for (const x of kids) x.segs = [...to, ...x.segs.slice(from.length)];
        if (segKey(from.slice(0, -1)) !== segKey(to.slice(0, -1))) e.moved = true;
        e.segs = to;
        if (e.mark !== "new") {
          e.mark = e.moved ? "moved" : "renamed";
          e.note = liveNote(e);
        }
        break;
      }
      case "trash":
        if (e) {
          e.mark = "trash";
          e.gone = true;
          for (const x of inside(from)) x.gone = true;
        }
        break;
      case "compress":
        if (e) e.mark = "compress";
        break;
      case "relocate":
        if (e) {
          e.mark = "relocate";
          e.note = s.to ? `${driveKey(s.to)} 드라이브로 (원래 자리에 링크)` : undefined;
        }
        break;
    }
  }
  return { before: treeOf(rootName, now), after: treeOf(rootName, live) };
}

const MARK_LABEL: Record<Mark, string> = {
  new: "새 폴더",
  moved: "옮겨 옴",
  renamed: "이름 바뀜",
  trash: "휴지통",
  compress: "압축",
  relocate: "다른 드라이브로",
  out: "밖으로 옮김",
};

const CHANGE_LABEL: Record<Action, string> = {
  mkdir: "",
  move: "옮길 항목",
  rename: "이름을 바꿀 항목",
  trash: "휴지통으로 보낼 항목",
  compress: "압축할 항목",
  relocate: "다른 드라이브로 옮길 폴더",
};

/** A folder or file in a tree: one with items under it is a folder; one no step says more of goes by its name. */
const isDir = (n: TreeNode) => n.dir ?? (n.kids.size > 0 || !/\.[A-Za-z0-9]{1,6}$/.test(n.name));

function TreeRows({ n, side }: { n: TreeNode; side: "before" | "after" }) {
  const kids = [...n.kids.values()].sort(
    (a, b) => Number(isDir(b)) - Number(isDir(a)) || a.name.localeCompare(b.name, "ko", { numeric: true }),
  );
  if (!kids.length) return null;
  return (
    <ul className="plan-tree">
      {kids.map((k) => (
        <li key={k.name.toLowerCase()} className={k.mark ? `mk-${k.mark}` : k.change && side === "before" ? "chg" : undefined}>
          <div className="plan-tree-row" title={k.note ? `${k.name}\n${k.note}` : k.change && side === "before" ? CHANGE_LABEL[k.change] : k.name}>
            <span className={`icon ${isDir(k) ? "dir" : "file"}`} aria-hidden="true" />
            <span className="plan-tree-name">{shown(k.name)}</span>
            {side === "after" && k.mark && <span className={`plan-mark mk-${k.mark}`}>{MARK_LABEL[k.mark]}</span>}
            {side === "before" && k.change && <span className="plan-chg-dot" aria-label={CHANGE_LABEL[k.change]} />}
            {side === "after" && k.note && <span className="plan-tree-note">{shown(k.note)}</span>}
          </div>
          <TreeRows n={k} side={side} />
        </li>
      ))}
    </ul>
  );
}

function BeforeAfter({ plan }: { plan: Plan }) {
  const { before, after } = useMemo(() => beforeAfter(plan), [plan]);
  return (
    <>
      <div className="plan-ba">
        <section className="plan-ba-col">
          <h5>정리 전</h5>
          <div className="plan-ba-tree">
            <div className="plan-tree-row plan-tree-top" title={plan.root}>
              <span className="icon dir" aria-hidden="true" />
              <span className="plan-tree-name">{shown(before.name)}</span>
            </div>
            <TreeRows n={before} side="before" />
          </div>
        </section>
        <section className="plan-ba-col">
          <h5>정리 후</h5>
          <div className="plan-ba-tree">
            <div className="plan-tree-row plan-tree-top" title={plan.root}>
              <span className="icon dir" aria-hidden="true" />
              <span className="plan-tree-name">{shown(after.name)}</span>
            </div>
            <TreeRows n={after} side="after" />
          </div>
        </section>
      </div>
      <p className="plan-foot-note">
        계획이 다루는 항목과 그 위 폴더만 보여요(다른 항목은 그대로예요). 단계는 차례대로 반영해서, 앞 단계가 만들거나 옮겨 놓을
        항목은 정리 전에 없어요. 건너뛴 단계(항목이 없거나 이름이 이미 있는 단계 등)는 넣지 않았어요. 정리 후에는 고른 단계만
        반영하고, 휴지통으로 갈 항목은 줄을 그어 보여 줘요. 정리 전의 <span className="plan-chg-dot" aria-hidden="true" /> 표시는
        고른 단계가 바꿀 항목이에요. 실제로 실행할 때는 단계마다 다시 확인해서, 그사이 바뀐 단계는 건너뛰어요.
      </p>
    </>
  );
}

/** The counts and sizes over the steps. */
function PlanCards({ plan, draft, totals }: { plan: Plan; draft: boolean; totals: { sel: PlanStep[]; bytes: number; freed: number; danger: number; done: PlanStep[] } }) {
  const by = new Map<Action, { all: number; sel: number; done: number }>();
  for (const s of plan.steps) {
    const c = by.get(s.action) ?? { all: 0, sel: 0, done: 0 };
    c.all++;
    if (s.selected && s.state === "pending") c.sel++;
    if (s.state === "done") c.done++;
    by.set(s.action, c);
  }
  const doneBytes = sumSize(totals.done);
  const doneFreed = sumSize(totals.done.filter((s) => FREES.has(s.action)));
  const hasTrash = totals.sel.some((s) => s.action === "trash");
  const hasReloc = totals.sel.some((s) => s.action === "relocate");
  return (
    <div className="plan-cards">
      {CARD_ORDER.filter((a) => by.has(a)).map((a) => {
        const c = by.get(a)!;
        return (
          <div key={a} className={`plan-card act-${a}`}>
            <span className="plan-card-label">
              <ActIcon action={a} />
              {CARD_LABEL[a]}
            </span>
            <b className="plan-card-num">{(draft ? c.sel : c.done).toLocaleString()}</b>
            <span className="plan-card-sub">
              {draft ? `전체 ${c.all.toLocaleString()}개 중 고름` : `전체 ${c.all.toLocaleString()}개 중 완료`}
            </span>
          </div>
        );
      })}
      <div className="plan-card size">
        <span className="plan-card-label">{draft ? "선택한 항목 크기" : "실행한 항목 크기"}</span>
        <b className="plan-card-num">{formatSize(draft ? totals.bytes : doneBytes)}</b>
        <span className="plan-card-sub">
          {draft ? `고른 단계 ${totals.sel.length.toLocaleString()}개` : `완료한 단계 ${totals.done.length.toLocaleString()}개`}
        </span>
      </div>
      <div className="plan-card free">
        <span className="plan-card-label">{draft ? "예상 확보 용량" : "확보한 용량"}</span>
        <b className="plan-card-num">{formatSize(draft ? totals.freed : doneFreed)}</b>
        <span className="plan-card-sub">
          {!draft
            ? "휴지통으로 보낸 항목은 휴지통을 비워야 생겨요"
            : hasTrash
              ? "휴지통을 비우면 생겨요"
              : hasReloc
                ? "다른 드라이브로 옮긴 만큼"
                : "지우거나 옮기는 단계가 없어요"}
        </span>
      </div>
      {draft && totals.danger > 0 && (
        <div className="plan-card warn">
          <span className="plan-card-label">위험 표시</span>
          <b className="plan-card-num">{totals.danger.toLocaleString()}</b>
          <span className="plan-card-sub">고른 단계 중 "위험"이에요</span>
        </div>
      )}
    </div>
  );
}

type PlanViewProps = {
  planId: string;
  onClose(): void;
  /** A folder to show in the tab: the plan's folder, or the folder an item of a step is in. */
  onNavigate(path: string): void;
};

/** One plan: review, pick steps, run, undo. Another plan id starts over (nothing of the one before stays). */
export function PlanView(props: PlanViewProps) {
  return <OnePlan key={props.planId} {...props} />;
}

function OnePlan({ planId, onClose, onNavigate }: PlanViewProps) {
  const [plan, setPlan] = useState<Plan | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [progress, setProgress] = useState<PlanProgress | null>(null);
  const [save, setSave] = useState<{ state: "idle" | "saving" | "saved" | "error"; text?: string }>({ state: "idle" });
  const [confirm, setConfirm] = useState<"run" | "undo" | "delete" | "jev" | null>(null);
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<Notice | null>(null);
  /** jev is on with a key, and whether to ask before sending. */
  const [jev, setJev] = useState<{ ready: boolean; confirm: boolean }>({ ready: false, confirm: true });
  const [risks, setRisks] = useState<Map<string, Risk> | null>(null);
  const [jevBusy, setJevBusy] = useState(false);
  /** The steps by folder, or the before/after trees (a plan that makes folders, moves or renames). */
  const [view, setView] = useState<"steps" | "tree">("steps");
  /** Folders (their groups' keys) folded away, and steps whose full paths are open. */
  const [closed, setClosed] = useState<Set<string>>(() => new Set());
  const [opened, setOpened] = useState<Set<number>>(() => new Set());

  const alive = useRef(true);
  /** The plan as shown, for callbacks that outlive a render. */
  const planRef = useRef<Plan | null>(null);
  /** Edits made here, and the newest of them saved. */
  const version = useRef(0);
  const savedVersion = useRef(0);
  const timer = useRef<number | null>(null);
  /** Saves run one after another, in the order made. */
  const chain = useRef<Promise<boolean>>(Promise.resolve(true));
  /** Only the newest load is shown. */
  const loads = useRef(0);
  /** Progress events seen, so a "running" set here does not hide one that came first. */
  const events = useRef(0);
  const reloadTimer = useRef<number | null>(null);
  /** When a run or undo was started here: a plan read just after may not say "running" yet. */
  const startedAt = useRef(0);
  /** What was started here last: how it ended is told in its own words. */
  const lastWork = useRef<"run" | "undo" | null>(null);
  /** A run or undo ended (its last event came): the plan read next shows where it went wrong. */
  const reveal = useRef(false);

  const put = (p: Plan) => {
    planRef.current = p;
    setPlan(p);
  };

  const load = useCallback(() => {
    const n = ++loads.current;
    plans.get(planId).then(
      (p) => {
        if (!alive.current || n !== loads.current) return;
        // Edits not saved yet stay (only a draft has them).
        if (version.current !== savedVersion.current && p.state === "draft" && planRef.current?.state === "draft") return;
        planRef.current = p;
        setPlan(p);
        setLoadError(null);
        // The errors are in the step list (not the before/after trees): shown, their folders open.
        if (reveal.current && p.state !== "running" && p.state !== "draft") {
          reveal.current = false;
          const bad = new Set(groupSteps(p.steps.filter(troubled)).map((g) => g.key));
          if (bad.size) {
            setView("steps");
            setClosed((c) => ([...c].some((k) => bad.has(k)) ? new Set([...c].filter((k) => !bad.has(k))) : c));
          }
        }
        // Over, even if its last event was missed.
        if (p.state !== "running" && p.state !== "draft" && Date.now() - startedAt.current > 1500) {
          setProgress((pr) => (pr && pr.state === "running" ? { ...pr, state: p.state } : pr));
        }
      },
      (err) => {
        if (alive.current && n === loads.current) setLoadError(errText(err));
      },
    );
  }, [planId]);

  const saveNow = useCallback(async (): Promise<boolean> => {
    const p = planRef.current;
    const v = version.current;
    if (!p || v === savedVersion.current) return true;
    if (p.state !== "draft") return false;
    if (alive.current) setSave({ state: "saving" });
    try {
      const back = await plans.update(p);
      savedVersion.current = Math.max(savedVersion.current, v);
      if (alive.current && version.current === v) {
        // The backend checks the plan again and may change it (a step it no longer allows): it is what runs.
        planRef.current = back;
        setPlan(back);
        setSave({ state: "saved" });
      }
      return true;
    } catch (err) {
      if (alive.current) setSave({ state: "error", text: errText(err) });
      return false;
    }
  }, []);

  /** Saves edits not saved yet, now. True when the backend has them all. */
  const flush = useCallback((): Promise<boolean> => {
    if (timer.current !== null) {
      clearTimeout(timer.current);
      timer.current = null;
    }
    const next = chain.current.then(saveNow, saveNow);
    chain.current = next;
    return next;
  }, [saveNow]);

  useEffect(() => {
    alive.current = true;
    let dead = false;
    let off: UnlistenFn | null = null;
    load();
    planEvents
      .progress((p) => {
        if (dead || p.id !== planId) return;
        events.current++;
        setProgress(p);
        if (p.state !== "running") {
          if (reloadTimer.current !== null) clearTimeout(reloadTimer.current);
          reloadTimer.current = null;
          reveal.current = true;
          load();
        } else if (reloadTimer.current === null) {
          // Steps' states while it runs, a couple of times a second at most.
          reloadTimer.current = window.setTimeout(() => {
            reloadTimer.current = null;
            load();
          }, 500);
        }
      })
      .then(
        (f) => (dead ? f() : void (off = f)),
        () => {},
      );
    return () => {
      dead = true;
      alive.current = false;
      off?.();
      if (reloadTimer.current !== null) clearTimeout(reloadTimer.current);
      reloadTimer.current = null;
      // A selection changed just before closing is kept.
      if (timer.current !== null || version.current !== savedVersion.current) void flush();
    };
  }, [load, planId, flush]);

  useEffect(() => {
    Promise.all([ai.getSettings(), ai.keyStatus()]).then(
      ([s, k]) => alive.current && setJev({ ready: s.jev_enabled && k.jev, confirm: s.confirm_before_send }),
      () => {},
    );
  }, []);

  const draft = plan?.state === "draft";
  const running = plan?.state === "running" || progress?.state === "running";
  const editable = draft && !busy && !running;

  // Events tell how a run goes; reading the plan now and then also catches one that was missed.
  useEffect(() => {
    if (!running) return;
    const t = window.setInterval(load, 2000);
    return () => clearInterval(t);
  }, [running, load]);

  /** Sets `selected` of every pending step `f` answers for (null leaves it). */
  const edit = (f: (s: PlanStep) => boolean | null) => {
    const p = planRef.current;
    if (!p || p.state !== "draft" || busy) return;
    let changed = false;
    const steps = p.steps.map((s) => {
      const v = s.state === "pending" ? f(s) : null;
      if (v === null || v === s.selected) return s;
      changed = true;
      return { ...s, selected: v };
    });
    if (!changed) return;
    put({ ...p, steps });
    version.current++;
    setSave({ state: "idle" });
    if (timer.current !== null) clearTimeout(timer.current);
    timer.current = window.setTimeout(() => {
      timer.current = null;
      void flush();
    }, SAVE_MS);
  };

  const totals = useMemo(() => {
    const steps = plan?.steps ?? [];
    const sel = steps.filter((s) => s.selected && s.state === "pending");
    return {
      all: steps.length,
      pending: steps.filter((s) => s.state === "pending").length,
      sel,
      bytes: sumSize(sel),
      freed: sumSize(sel.filter((s) => FREES.has(s.action))),
      trash: sel.filter((s) => s.action === "trash"),
      compress: sumSize(sel.filter((s) => s.action === "compress")),
      danger: sel.filter((s) => s.risk === "danger").length,
      done: steps.filter((s) => s.state === "done"),
      failed: steps.filter((s) => s.state === "failed").length,
      skipped: steps.filter((s) => s.state === "skipped").length,
      troubled: steps.filter(troubled).length,
    };
  }, [plan]);

  const canUndo =
    !!plan &&
    !running &&
    totals.done.length > 0 &&
    (plan.state === "done" || plan.state === "partly" || plan.state === "failed");

  const jevPaths = useMemo(() => {
    const seen = new Set<string>();
    const out: string[] = [];
    for (const s of plan?.steps ?? []) {
      if (s.action === "mkdir" || seen.has(s.from.toLowerCase())) continue;
      seen.add(s.from.toLowerCase());
      out.push(s.from);
      if (out.length >= JEV_MAX) break;
    }
    return out;
  }, [plan]);

  const groups = useMemo(() => groupSteps(plan?.steps ?? []), [plan]);
  const hasShape = !!plan?.steps.some((s) => SHAPES.has(s.action) && inTrees(s));
  const toggleGroup = (key: string) =>
    setClosed((c) => {
      const n = new Set(c);
      if (!n.delete(key)) n.add(key);
      return n;
    });
  const toggleOpen = (id: number) =>
    setOpened((o) => {
      const n = new Set(o);
      if (!n.delete(id)) n.add(id);
      return n;
    });

  /** Before the confirm: the selection saved, so what it counts is what will run. */
  const openRun = async () => {
    if (busy || !totals.sel.length) return;
    setNotice(null);
    if (!(await flush())) {
      if (alive.current) setNotice({ text: "고른 단계를 저장하지 못해서 실행할 수 없어요. 잠시 뒤에 다시 해 주세요", error: true });
      return;
    }
    if (alive.current) setConfirm("run");
  };

  const run = async () => {
    setConfirm(null);
    if (busy) return;
    setBusy(true);
    setNotice(null);
    try {
      if (!(await flush())) {
        if (alive.current) setNotice({ text: "고른 단계를 저장하지 못해서 실행하지 않았어요", error: true });
        return;
      }
      const total = planRef.current?.steps.filter((s) => s.selected && s.state === "pending").length ?? 0;
      const seen = events.current;
      startedAt.current = Date.now();
      lastWork.current = "run";
      await plans.execute(planId);
      if (!alive.current) return;
      if (events.current === seen) setProgress({ id: planId, state: "running", done: 0, total, current: null });
      load();
    } catch (err) {
      if (alive.current) setNotice({ text: `실행하지 못했어요: ${errText(err)}`, error: true });
    } finally {
      if (alive.current) setBusy(false);
    }
  };

  const undo = async () => {
    setConfirm(null);
    if (busy) return;
    setBusy(true);
    setNotice(null);
    try {
      const total = totals.done.length;
      const seen = events.current;
      startedAt.current = Date.now();
      lastWork.current = "undo";
      await plans.undo(planId);
      if (!alive.current) return;
      if (events.current === seen) setProgress({ id: planId, state: "running", done: 0, total, current: null });
      load();
    } catch (err) {
      if (alive.current) setNotice({ text: `되돌리지 못했어요: ${errText(err)}`, error: true });
    } finally {
      if (alive.current) setBusy(false);
    }
  };

  const remove = async () => {
    setConfirm(null);
    if (busy) return;
    setBusy(true);
    // Nothing more to save for a plan that goes.
    if (timer.current !== null) clearTimeout(timer.current);
    timer.current = null;
    try {
      // A save on its way finishes first, so it cannot write the plan back after it is gone.
      await chain.current;
      await plans.remove(planId);
      savedVersion.current = version.current;
      if (alive.current) onClose();
    } catch (err) {
      if (alive.current) setNotice({ text: `계획을 지우지 못했어요: ${errText(err)}`, error: true });
    } finally {
      if (alive.current) setBusy(false);
    }
  };

  const fetchJev = async () => {
    setConfirm(null);
    if (jevBusy || !jevPaths.length) return;
    setJevBusy(true);
    try {
      const list: Risk[] = [];
      for (let i = 0; i < jevPaths.length; i += JEV_CALL) {
        list.push(...(await ai.jevRisk(jevPaths.slice(i, i + JEV_CALL))));
        if (!alive.current) return;
      }
      setRisks(new Map(list.map((r) => [r.path.toLowerCase(), r])));
    } catch (err) {
      if (alive.current) setNotice({ text: `jev 위험도를 받지 못했어요: ${errText(err)}`, error: true });
    } finally {
      if (alive.current) setJevBusy(false);
    }
  };

  /** Whether to say what goes first is read from the settings as saved now (another window may have changed them). */
  const askJev = async () => {
    if (jevBusy || !jevPaths.length) return;
    let ask = jev.confirm;
    try {
      const s = await ai.getSettings();
      if (!alive.current) return;
      setJev((j) => ({ ...j, confirm: s.confirm_before_send }));
      if (!s.jev_enabled) {
        setJev((j) => ({ ...j, ready: false }));
        setNotice({ text: "jev 위험도가 AI 설정에서 꺼졌어요. 다시 켜면 쓸 수 있어요", error: true });
        return;
      }
      ask = s.confirm_before_send;
    } catch {
      // Not known now: asked first.
      ask = true;
    }
    if (ask) setConfirm("jev");
    else void fetchJev();
  };

  /** How the last run (or undo) seen here ended. */
  const ended: Notice | null = (() => {
    if (!plan || !progress || progress.state === "running" || plan.state === "running") return null;
    // An undo started elsewhere (another tab) shows in the steps: undone ones, or ones it could not undo.
    const undoing =
      lastWork.current === "undo" ||
      (lastWork.current === null && plan.steps.some((s) => s.state === "undone" || !!s.error?.startsWith("되돌리지 못했어요")));
    switch (plan.state) {
      case "done":
        return { text: "고른 단계를 모두 실행했어요. 필요하면 되돌릴 수 있어요." };
      case "partly":
        return undoing
          ? {
              text: '일부 단계는 되돌리지 못했어요. 단계 목록에서 "실패"가 붙은 폴더를 펼치면 단계마다 이유가 있어요. 이유를 해결하고 다시 "되돌리기"를 누르면 남은 단계만 다시 해 봐요.',
              error: true,
            }
          : {
              text: '일부 단계는 실행하지 못했어요. 단계 목록에서 "실패"가 붙은 폴더를 펼쳐 단계마다 오류를 확인해 주세요. 실행한 단계는 되돌릴 수 있어요.',
              error: true,
            };
      case "failed":
        return { text: '실행하지 못했어요. 단계 목록에서 "실패"가 붙은 폴더를 펼쳐 단계마다 오류를 확인해 주세요.', error: true };
      case "undone":
        return { text: "되돌렸어요. 되돌리지 못한 단계가 있으면 단계 목록에 이유가 남아 있어요." };
      default:
        return null;
    }
  })();

  const allSel = totals.pending > 0 && totals.sel.length === totals.pending;
  const someSel = totals.sel.length > 0 && !allSel;
  const saveText = !draft
    ? ""
    : save.state === "saving"
      ? "저장하는 중…"
      : save.state === "saved"
        ? "고른 단계를 저장했어요"
        : save.state === "error"
          ? "저장하지 못했어요"
          : "";
  const fromAi = !!plan && isAiSource(plan.source);
  // What the rules write (and what the app adds to an AI's summary) holds paths as they are: plain text.
  const [aiSummary, appSummary] = !plan ? ["", ""] : fromAi ? splitSummary(plan.summary).map((x) => x.trim()) : ["", plan.summary.trim()];

  return (
    <Dialog className="v04-dialog plan-view" title={plan ? `정리 계획: ${plan.title}` : "정리 계획"} onClose={onClose}>
      <div className="dlg-head">
        <h3 title={plan?.title}>{plan ? shown(plan.title || KIND_LABEL[plan.kind] || "정리 계획") : "정리 계획"}</h3>
        {plan && <SourceTag source={plan.source} />}
        {plan && <StateTag state={progress?.state === "running" ? "running" : plan.state} />}
        <button className="dlg-close" aria-label="닫기" onClick={onClose}>
          ✕
        </button>
      </div>

      {plan && (
        <div className="plan-meta">
          <span>{KIND_LABEL[plan.kind] ?? plan.kind}</span>
          {formatDate(plan.created_at) && <span>· {formatDate(plan.created_at)}에 만듦</span>}
          <span>·</span>
          <button className="plan-root" title={`${plan.root}\n이 폴더로 가기`} onClick={() => onNavigate(plan.root)}>
            {shown(plan.root)}
          </button>
        </div>
      )}

      {plan && draft && (
        <div className="plan-tools">
          <span className="muted">고르기</span>
          <button className="btn small" disabled={!editable} onClick={() => edit(() => true)}>
            모두
          </button>
          <button className="btn small" disabled={!editable} onClick={() => edit(() => false)}>
            없음
          </button>
          <button className="btn small" disabled={!editable} onClick={() => edit((s) => s.risk === "safe")}>
            안전만
          </button>
          <button className="btn small" disabled={!editable} onClick={() => edit((s) => s.risk !== "danger")}>
            주의까지
          </button>
          <button className="btn small" disabled={!editable} onClick={() => edit((s) => (s.risk === "danger" ? false : null))}>
            위험 빼기
          </button>
          <span className="spacer" />
          {jev.ready && (
            <>
              <button className="btn small" disabled={jevBusy || !jevPaths.length} title={JEV_TIP} onClick={askJev}>
                {jevBusy ? "jev에 묻는 중…" : "jev로 위험도 다시 확인 (빠르고 저렴한 2차 판정)"}
              </button>
              <span className="plan-info" tabIndex={0} role="note" aria-label={JEV_TIP} title={JEV_TIP}>
                ?
              </span>
            </>
          )}
        </div>
      )}

      <div className="dlg-body">
        {loadError && <div className="v04-error">계획을 불러오지 못했어요: {loadError}</div>}
        {!plan && !loadError && <p className="muted v04-intro">불러오는 중…</p>}
        {plan && (
          <>
            {(aiSummary || appSummary) && (
              <div className="plan-summary">
                {aiSummary && <span className="v06-src src-ai plan-said">AI 설명</span>}
                <div className="plan-summary-text">
                  {aiSummary && <Markdown text={aiSummary} />}
                  {appSummary && <p className={`plan-plain${aiSummary ? " plan-summary-app" : ""}`}>{shown(appSummary)}</p>}
                </div>
              </div>
            )}
            {plan.steps.length > 0 && <PlanCards plan={plan} draft={draft} totals={totals} />}
            {fromAi && draft && (
              <div className="dup-caveat">
                {plan.source === "mcp" ? "Claude 앱이 MCP로 제안한" : "AI가 만든"} 계획이에요. 휴지통으로 보내는 단계는 처음에 고르지
                않은 상태예요. 직접 확인한 단계만 골라 주세요. 실행할 때 앱이 단계마다 다시 확인해서, 이 폴더 밖이나 시스템·프로그램
                폴더의 항목은 실행하지 않아요.
              </div>
            )}
            {notice && <div className={notice.error ? "v04-error" : "v04-ok"}>{notice.text}</div>}
            {ended && !notice && <div className={ended.error ? "v04-error" : "v04-ok"}>{ended.text}</div>}

            {running && (
              <div className="plan-progress">
                <div className={`job-bar${progress && progress.total > 0 ? "" : " unknown"}`}>
                  <span
                    style={
                      progress && progress.total > 0
                        ? { width: `${Math.min(100, (progress.done / progress.total) * 100).toFixed(1)}%` }
                        : undefined
                    }
                  />
                </div>
                <div className="job-meta">
                  {progress && progress.total > 0 ? `${progress.done.toLocaleString()} / ${progress.total.toLocaleString()} 단계` : "시작하는 중…"}
                </div>
                {progress?.current && (
                  <div className="job-current" title={progress.current}>
                    {shown(progress.current)}
                  </div>
                )}
                <div className="muted plan-foot-note">
                  실행 중에 앱을 끄려고 하면 먼저 물어요. 끄기로 하면 지금 하던 단계까지만 마치고 그 되돌리기 기록을 저장한 뒤에 꺼지고,
                  남은 단계는 하지 않아요. 이 창은 닫아도 실행은 계속돼요.
                </div>
              </div>
            )}

            {!draft && (
              <div className="muted plan-foot-note">
                완료 {totals.done.length.toLocaleString()}개 · 실패 {totals.failed.toLocaleString()}개 · 건너뜀{" "}
                {totals.skipped.toLocaleString()}개 / 전체 {totals.all.toLocaleString()}개
              </div>
            )}

            {plan.steps.length === 0 ? (
              <p className="muted v04-intro">할 일이 없어요. 이 폴더에서 바꿀 만한 것을 찾지 못했어요.</p>
            ) : (
              <>
                <div className="plan-list-head">
                  {hasShape && (
                    <div className="plan-views" role="tablist" aria-label="보는 방식">
                      <button role="tab" aria-selected={view === "steps"} className={view === "steps" ? "on" : undefined} onClick={() => setView("steps")}>
                        단계 목록
                      </button>
                      <button
                        role="tab"
                        aria-selected={view === "tree"}
                        className={view === "tree" ? "on" : undefined}
                        title="계획이 다루는 항목의 폴더 구조를 정리 전과 정리 후로 나란히 보여 줘요"
                        onClick={() => setView("tree")}
                      >
                        정리 전 · 후 비교
                      </button>
                    </div>
                  )}
                  {view === "steps" && (
                    <>
                      <label className="plan-all">
                        <input
                          type="checkbox"
                          aria-label="모두 고르기"
                          checked={allSel}
                          disabled={!editable || totals.pending === 0}
                          ref={(el) => {
                            if (el) el.indeterminate = someSel;
                          }}
                          onChange={(e) => {
                            const on = e.target.checked;
                            edit(() => on);
                          }}
                        />
                        모두 고르기
                      </label>
                      <span className="muted">
                        폴더 {groups.length.toLocaleString()}곳 · 단계 {totals.all.toLocaleString()}개
                      </span>
                      <span className="spacer" />
                      {groups.length > 1 && (
                        <>
                          <button className="btn small" disabled={closed.size === 0} onClick={() => setClosed(new Set())}>
                            모두 펼치기
                          </button>
                          <button
                            className="btn small"
                            disabled={closed.size === groups.length}
                            onClick={() => setClosed(new Set(groups.map((g) => g.key)))}
                          >
                            모두 접기
                          </button>
                        </>
                      )}
                    </>
                  )}
                </div>

                {view === "tree" && hasShape ? (
                  <>
                    {totals.troubled > 0 && (
                      <div className="v04-error plan-tree-trouble">
                        실패한 단계가 {totals.troubled.toLocaleString()}개 있어요. 이유는{" "}
                        <button className="v06-link" onClick={() => setView("steps")}>
                          단계 목록
                        </button>
                        에서 볼 수 있어요.
                      </div>
                    )}
                    <BeforeAfter plan={plan} />
                  </>
                ) : (
                  <div className="plan-groups">
                    {groups.map((g) => {
                      const pend = g.steps.filter((s) => s.state === "pending");
                      const sel = pend.filter((s) => s.selected);
                      const gAll = pend.length > 0 && sel.length === pend.length;
                      const gSome = sel.length > 0 && !gAll;
                      const shut = closed.has(g.key);
                      const label = folderLabel(plan.root, g.folder);
                      const ids = new Set(g.steps.map((s) => s.id));
                      const bytes = sumSize(draft ? sel : g.steps);
                      const bad = g.steps.filter(troubled).length;
                      return (
                        <section key={g.key} className={`plan-group${shut ? " shut" : ""}`}>
                          <div className="plan-group-head">
                            <input
                              type="checkbox"
                              aria-label={`${label} 폴더의 단계 모두 고르기`}
                              checked={gAll}
                              disabled={!editable || pend.length === 0}
                              ref={(el) => {
                                if (el) el.indeterminate = gSome;
                              }}
                              onChange={(e) => {
                                const on = e.target.checked;
                                edit((x) => (ids.has(x.id) ? on : null));
                              }}
                            />
                            <button className="plan-group-toggle" aria-expanded={!shut} title={g.folder} onClick={() => toggleGroup(g.key)}>
                              <span className="plan-chev" aria-hidden="true">
                                {shut ? "▸" : "▾"}
                              </span>
                              <span className="icon dir" aria-hidden="true" />
                              <span className="plan-group-name">{shown(label)}</span>
                              <span className="plan-group-meta">
                                단계 {g.steps.length.toLocaleString()}개
                                {draft ? ` · 고른 ${sel.length.toLocaleString()}개` : ""}
                                {bytes > 0 ? ` · ${formatSize(bytes)}` : ""}
                                {bad > 0 && <span className="plan-group-fail"> · 실패 {bad.toLocaleString()}개</span>}
                              </span>
                            </button>
                          </div>
                          {!shut && (
                            <ul className="plan-items">
                              {g.steps.map((s) => {
                                const r = risks?.get(s.from.toLowerCase());
                                const open = opened.has(s.id);
                                return (
                                  <li key={s.id} className={`plan-step st-${s.state}${s.selected && s.state === "pending" ? " sel" : ""}`}>
                                    <input
                                      type="checkbox"
                                      aria-label={`${ACTION_LABEL[s.action] ?? s.action}: ${baseName(s.from)}`}
                                      checked={s.selected}
                                      disabled={!editable || s.state !== "pending"}
                                      onChange={(e) => {
                                        const on = e.target.checked;
                                        edit((x) => (x.id === s.id ? on : null));
                                      }}
                                    />
                                    <ActBadge action={s.action} />
                                    <div className="plan-step-main">
                                      <div className="plan-sentence" title={pathsTip(s)}>
                                        <StepSentence s={s} root={plan.root} onNavigate={onNavigate} />
                                      </div>
                                      {s.reason && (
                                        <div className="plan-reason" title={s.reason}>
                                          {/* A rule's reason names paths as they are (which copy stays): no marks read in it. */}
                                          {fromAi ? <MarkdownInline text={s.reason} /> : shown(s.reason)}
                                        </div>
                                      )}
                                      {s.error && (
                                        <div className="plan-err" title={s.error}>
                                          {shown(s.error)}
                                        </div>
                                      )}
                                      {open && (
                                        <dl className="plan-detail">
                                          <dt>{s.action === "mkdir" ? "만들 폴더" : "지금 경로"}</dt>
                                          <dd>{shown(s.from)}</dd>
                                          {s.to && (
                                            <>
                                              <dt>새 경로</dt>
                                              <dd>{shown(s.to)}</dd>
                                            </>
                                          )}
                                          {s.action === "trash" && (
                                            <>
                                              <dt>가는 곳</dt>
                                              <dd>휴지통 (되돌리기로 되살릴 수 있어요)</dd>
                                            </>
                                          )}
                                          <dd className="plan-detail-go">
                                            <button className="v06-link" onClick={() => onNavigate(stepFolder(s))}>
                                              이 항목이 있는 폴더로 가기
                                            </button>
                                          </dd>
                                        </dl>
                                      )}
                                    </div>
                                    <div className="plan-step-risk">
                                      <span className={`v06-risk risk-${s.risk}`} title="계획이 매긴 위험도">
                                        {RISK_LABEL[s.risk] ?? s.risk}
                                      </span>
                                      {r && (
                                        <span
                                          className={`v06-jev risk-${r.level}`}
                                          title={
                                            `jev 2차 판정 (참고용) · 신뢰도 ${Math.round(r.confidence * 100)}%` +
                                            (r.regenerable !== null ? ` · 프로그램이 다시 만드는 항목일 확률 ${Math.round(r.regenerable * 100)}%` : "")
                                          }
                                        >
                                          jev {RISK_LABEL[r.level] ?? r.level}
                                        </span>
                                      )}
                                    </div>
                                    <div className="plan-step-size num">{s.size > 0 && s.action !== "mkdir" ? formatSize(s.size) : ""}</div>
                                    <div className="plan-step-state">
                                      <span className={`v06-step st-${s.state}`}>
                                        {s.state === "pending" && !s.selected ? "안 함" : STEP_LABEL[s.state] ?? s.state}
                                      </span>
                                    </div>
                                    <button
                                      className="plan-more"
                                      aria-expanded={open}
                                      title={open ? "전체 경로 닫기" : `전체 경로 보기\n${pathsTip(s)}`}
                                      onClick={() => toggleOpen(s.id)}
                                    >
                                      {open ? "▴" : "▾"}
                                      <span className="plan-more-text">경로</span>
                                    </button>
                                  </li>
                                );
                              })}
                            </ul>
                          )}
                        </section>
                      );
                    })}
                  </div>
                )}
              </>
            )}

            {draft && plan.steps.length > 0 && (
              <p className="plan-foot-note">
                확보 예상은 휴지통으로 보내는 크기와 다른 드라이브로 옮기는 크기를 더한 값이에요. 휴지통으로 보낸 항목은 휴지통을
                비워야 공간이 생겨요. 압축으로 줄어드는 양은 파일마다 달라서 넣지 않았어요
                {totals.compress > 0 ? ` (압축할 크기 ${formatSize(totals.compress)})` : ""}.
              </p>
            )}
          </>
        )}
      </div>

      <div className="dlg-foot">
        {plan && draft && (
          <span className="plan-totals">
            고른 단계 <b>{totals.sel.length.toLocaleString()}</b>/{totals.pending.toLocaleString()}개 · 다루는 크기{" "}
            <b>{formatSize(totals.bytes)}</b> · 확보 예상 <b>{formatSize(totals.freed)}</b>
          </span>
        )}
        <span className="spacer" />
        {saveText && (
          <span className={`plan-save ${save.state}`} title={save.text}>
            {saveText}
          </span>
        )}
        {plan && draft && (
          <button className="btn" disabled={busy} onClick={() => setConfirm("delete")}>
            삭제
          </button>
        )}
        {canUndo && (
          <button className="btn" disabled={busy} onClick={() => setConfirm("undo")}>
            되돌리기…
          </button>
        )}
        <button className="btn" onClick={onClose}>
          닫기
        </button>
        {plan && draft && (
          <button
            className={`btn ${totals.trash.length ? "danger" : "primary"}`}
            disabled={busy || running || !totals.sel.length}
            onClick={() => void openRun()}
          >
            실행…
          </button>
        )}
      </div>

      {confirm === "run" && plan && (
        <Dialog className="plan-confirm" title="계획을 실행할까요?" onClose={() => setConfirm(null)} onBackdrop={() => setConfirm(null)}>
          <h3>계획을 실행할까요?</h3>
          <p>
            고른 단계 <b>{totals.sel.length.toLocaleString()}개</b> · {formatSize(totals.bytes)}
          </p>
          <ul className="plan-counts">
            {countByAction(totals.sel).map((c) => (
              <li key={c.action} className={c.action === "trash" ? "trash" : undefined}>
                <span>
                  {ACTION_LABEL[c.action] ?? c.action} {c.count.toLocaleString()}개
                </span>
                <span>{c.bytes > 0 ? formatSize(c.bytes) : ""}</span>
              </li>
            ))}
          </ul>
          {totals.danger > 0 && (
            <p className="v04-error">위험으로 표시된 단계 {totals.danger.toLocaleString()}개가 들어 있어요. 한 번 더 확인해 주세요.</p>
          )}
          {fromAi && totals.trash.length > 0 && (
            <p className="v04-error">
              {plan.source === "mcp" ? "Claude 앱이 MCP로" : "AI가"} 제안한 휴지통 단계 {totals.trash.length.toLocaleString()}개를
              골랐어요. 직접 확인한 것만 실행해 주세요.
            </p>
          )}
          <p className="muted">
            실행할 때 단계마다 다시 확인해요. 그사이 바뀌었거나 없어진 항목, 시스템·프로그램 폴더 안의 항목은 실행하지 않고 이유를
            남겨요. 이미 있는 이름 위에는 쓰지 않아요. 지우는 것은 휴지통으로만 보내요. 끝나면 "되돌리기"로 거꾸로 되돌릴 수 있어요.
          </p>
          <div className="actions">
            <button onClick={() => setConfirm(null)}>취소</button>
            <button className={`primary${totals.trash.length ? " danger" : ""}`} autoFocus onClick={() => void run()}>
              실행
            </button>
          </div>
        </Dialog>
      )}

      {confirm === "undo" && plan && (
        <Dialog className="plan-confirm" title="계획을 되돌릴까요?" onClose={() => setConfirm(null)} onBackdrop={() => setConfirm(null)}>
          <h3>계획을 되돌릴까요?</h3>
          <p>
            실행한 단계 <b>{totals.done.length.toLocaleString()}개</b>를 마지막 것부터 거꾸로 되돌려요.
          </p>
          <ul className="plan-counts">
            {countByAction(totals.done).map((c) => (
              <li key={c.action}>
                <span>
                  {UNDO_LABEL[c.action] ?? c.action} {c.count.toLocaleString()}개
                </span>
                <span>{c.bytes > 0 ? formatSize(c.bytes) : ""}</span>
              </li>
            ))}
          </ul>
          <p className="muted">
            그사이 바뀐 항목은 되돌리지 않고 이유를 남겨요. 휴지통을 비웠거나 휴지통에서 지운 항목은 되살릴 수 없어요. 다른
            드라이브로 옮긴 폴더를 되돌리려면 원래 드라이브에 빈 공간이 있어야 해요.
          </p>
          <div className="actions">
            <button onClick={() => setConfirm(null)}>취소</button>
            <button className="primary" autoFocus onClick={() => void undo()}>
              되돌리기
            </button>
          </div>
        </Dialog>
      )}

      {confirm === "delete" && plan && (
        <Dialog className="plan-confirm" title="계획을 지울까요?" onClose={() => setConfirm(null)} onBackdrop={() => setConfirm(null)}>
          <h3>계획을 지울까요?</h3>
          <p>
            <b>{shown(plan.title || KIND_LABEL[plan.kind] || "정리 계획")}</b>
          </p>
          <p className="muted">계획만 지워요. 파일과 폴더는 건드리지 않아요.</p>
          <div className="actions">
            <button onClick={() => setConfirm(null)}>취소</button>
            <button className="primary danger" autoFocus onClick={() => void remove()}>
              삭제
            </button>
          </div>
        </Dialog>
      )}

      {confirm === "jev" && (
        <Dialog className="ai-preview" title="jev에 보낼까요?" onClose={() => setConfirm(null)} onBackdrop={() => setConfirm(null)}>
          <h3>jev에 보낼까요?</h3>
          <p className="muted">
            아래 항목 {jevPaths.length.toLocaleString()}개의 경로와 크기 같은 정보를 TypeSafe jev로 보내 위험도를 받아요. 파일 내용은
            보내지 않아요.
          </p>
          <pre className="v06-code plan-paths-sent">{jevPaths.map(shown).join("\n")}</pre>
          <div className="actions">
            <button onClick={() => setConfirm(null)}>보내지 않기</button>
            <button className="primary" autoFocus onClick={() => void fetchJev()}>
              보내기
            </button>
          </div>
        </Dialog>
      )}
    </Dialog>
  );
}

export function PlansInbox({ onOpen, onClose }: { onOpen(id: string): void; onClose(): void }) {
  const [list, setList] = useState<Plan[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const alive = useRef(true);
  const loads = useRef(0);

  const load = useCallback(() => {
    const n = ++loads.current;
    plans.list().then(
      (l) => {
        if (!alive.current || n !== loads.current) return;
        setList([...l].sort((a, b) => b.created_at - a.created_at));
        setError(null);
      },
      (err) => {
        if (alive.current && n === loads.current) setError(errText(err));
      },
    );
  }, []);

  useEffect(() => {
    alive.current = true;
    let dead = false;
    let off: UnlistenFn | null = null;
    load();
    // A run that ends changes a plan's state.
    planEvents
      .progress((p) => {
        if (!dead && p.state !== "running") load();
      })
      .then(
        (f) => (dead ? f() : void (off = f)),
        () => {},
      );
    // Plans proposed over MCP come from another process, without an event: back from the Claude app, look again.
    const onFocus = () => load();
    window.addEventListener("focus", onFocus);
    return () => {
      dead = true;
      alive.current = false;
      off?.();
      window.removeEventListener("focus", onFocus);
    };
  }, [load]);

  const mcpDrafts = list?.filter((p) => p.source === "mcp" && p.state === "draft").length ?? 0;

  return (
    <Dialog className="v04-dialog plans-inbox" title="정리 계획" onClose={onClose}>
      <div className="dlg-head">
        <h3>정리 계획</h3>
        <button className="dlg-close" aria-label="닫기" onClick={onClose}>
          ✕
        </button>
      </div>
      <div className="dlg-body">
        {error && <div className="v04-error">계획 목록을 읽지 못했어요: {error}</div>}
        {list === null && !error && <p className="muted v04-intro">불러오는 중…</p>}
        {list?.length === 0 && (
          <p className="muted v04-intro">
            아직 만든 계획이 없어요. 정리 도우미에서 계획을 만들거나 Claude 앱이 MCP로 제안하면 여기에 남아요. 계획은 열어서 확인하고
            실행해야만 파일이 바뀌어요.
          </p>
        )}
        {mcpDrafts > 0 && (
          <div className="inbox-note">
            Claude 앱이 MCP로 제안한 계획 <b>{mcpDrafts.toLocaleString()}개</b>가 확인을 기다려요. 열어서 단계를 고르고 실행하기
            전에는 아무것도 바뀌지 않아요.
          </div>
        )}
        {!!list?.length && (
          <ul className="inbox-list">
            {list.map((p) => {
              const mcpDraft = p.source === "mcp" && p.state === "draft";
              const sel = p.steps.filter((s) => s.selected && s.state === "pending");
              const title = shown(p.title || KIND_LABEL[p.kind] || "정리 계획");
              return (
                <li key={p.id}>
                  <button className={`inbox-item${mcpDraft ? " mcp-draft" : ""}`} title={title} onClick={() => onOpen(p.id)}>
                    <span className="inbox-top">
                      <span className="inbox-title">{title}</span>
                      <SourceTag source={p.source} />
                      <StateTag state={p.state} />
                      {mcpDraft && <span className="v06-mcp-flag">확인 필요</span>}
                    </span>
                    <span className="inbox-meta muted">
                      {KIND_LABEL[p.kind] ?? p.kind} · 단계 {p.steps.length.toLocaleString()}개
                      {p.state === "draft" ? ` · 고른 ${sel.length.toLocaleString()}개 · ${formatSize(sumSize(sel))}` : ""}
                      {formatDate(p.created_at) ? ` · ${formatDate(p.created_at)}` : ""}
                    </span>
                    <span className="inbox-root muted" title={p.root}>
                      {shown(p.root)}
                    </span>
                  </button>
                </li>
              );
            })}
          </ul>
        )}
      </div>
      <div className="dlg-foot">
        <button className="btn" onClick={load}>
          새로 고침
        </button>
        <span className="spacer" />
        <button className="btn" autoFocus onClick={onClose}>
          닫기
        </button>
      </div>
    </Dialog>
  );
}
