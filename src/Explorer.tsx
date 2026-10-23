import { useCallback, useEffect, useId, useImperativeHandle, useLayoutEffect, useMemo, useRef, useState } from "react";
import {
  ai,
  aiEvents,
  api,
  JEV_SHOW,
  JUDGE_NAME,
  judgeModel,
  judgeOf,
  judgeRemote,
  judgeTarget,
  methodNote,
  ops,
  places,
  plans,
  v04,
  v05,
  v07,
  type AiPayloadPreview,
  type AiSettings as AiConfig,
  type DiffInfo,
  type DiffNode,
  type DriveInfo,
  type Entry,
  type FindProgress,
  type JevFound,
  type JevHit,
  type JevKind,
  type JevWhen,
  type JudgeEngine,
  type KeyStatus,
  type Listing,
  type ProgramRef,
  type Risk,
} from "./api";
import { AdminBar, adminCanRead, adminError, ensureAdmin } from "./AdminSession";
import { contentsNote, HelperPane, useHelper, type HelperTarget } from "./AiPanel";
import { AiSettings } from "./AiSettings";
import { Caret, FOLD_MAX, Icon, MenuButton, MenuRow, useFold } from "./CommandBar";
import { Dialog, Duplicates } from "./Duplicates";
import { FileTable, type ChangeCell, type Column, type RiskCell, type ScoreCell, type SortKey } from "./FileTable";
import { GridView, gridStep } from "./GridView";
import { HELP_TIPS } from "./Help";
import { MapView } from "./MapView";
import { PlansInbox, PlanView } from "./PlanView";
import { Preview } from "./Preview";
import { Programs, shown } from "./Programs";
import { RelocateDialog, Relocations } from "./Relocate";
import { dragOut, dragRefusal, screenPoint } from "./shell";
import { TreeView, type TreeRefresh } from "./TreeView";
import { driveKey, folderTitle, formatDate, formatSize, isSameOrInside, normalizePath, parentOf, pathKey, typeLabel } from "./format";
import "./v09.css";

export type View = "list" | "tree" | "grid" | "map";
export const VIEWS: View[] = ["list", "tree", "grid", "map"];
const VIEW_LABEL: Record<View, string> = { list: "목록", tree: "트리", grid: "아이콘", map: "지도" };
const VIEW_TIP: Record<View, string> = { list: "viewList", tree: "viewTree", grid: "viewGrid", map: "viewMap" };
const VIEW_ICON: Record<View, () => React.ReactNode> = { list: Icon.list, tree: Icon.tree, grid: Icon.grid, map: Icon.map };

/** Search results are always a list: the view menu waits. */
const SEARCH_VIEW = "검색 결과는 목록으로 보여요";

/**
 * What the buttons and menu items do, as their tooltips: the help's short texts (HELP_TIPS), and
 * a few for items the help has no entry of its own for. Each is also the item's accessible
 * description: App puts the texts in the page once (TipTexts) and the items point at them.
 */
export const TIPS: Record<string, string> = {
  ...HELP_TIPS,
  open: "파일은 연결된 프로그램으로 열고, 폴더는 이 탭에서 열어요 (Enter, 더블클릭)",
  openTab: "이 폴더를 새 탭에서 열어요. 폴더를 가운데 클릭하면 뒤에서 열려요",
  reveal: "Windows 탐색기를 열어 이 항목을 골라 둬요",
  goParent: "이 항목이 들어 있는 폴더를 이 탭에서 열어요",
  copyPath: "전체 경로를 글자로 클립보드에 복사해요",
  helperOpen: "옆 창의 정리 도우미가 이 폴더를 봐요. AI 설명과 정리 계획(규칙으로는 AI 없이)을 거기서 받아요",
  helperOpenFile: "옆 창의 정리 도우미가 이 파일을 봐요. 무엇인지, 지워도 되는지 AI 설명을 거기서 받아요 (AI는 켜야 쓰는 선택 기능)",
  planMenu: "규칙(AI 없이)이나 AI로 이 폴더의 정리 계획을 만들어요. 만들기만 하고, 계획 창에서 확인하고 실행해야 바뀌어요",
  more: "더 보기: 창이 좁아서 접어 둔 명령이에요 (정리 메뉴, 열 더 보기…)",
  help: "도움말 (F1): 기능 설명, 검색 문법, 문제 해결",
  drive: "이 드라이브를 열어요. 가운데 클릭하면 새 탭에서 열려요",
  scanMore: "다른 스캔 방식: 관리자 권한으로 전체 다시 스캔, 일반 탐색으로 다시 스캔",
  walkScan: "일반 탐색: UAC 창 없이 폴더를 처음부터 다시 훑어요. 내 계정으로 열 수 없는 폴더는 빠지고, 동기화도 그 뒤로는 그 폴더를 세지 않아요",
};

/** The id of the hidden text that describes a feature (TipTexts). */
export const tipId = (key: string) => `ff-tip-${key}`;

/** A feature's tooltip (or `title` instead, a reason it is off) and accessible description, for a button or menu item. */
export function tip(key: string, title?: string): { title: string; "aria-describedby": string } {
  return { title: title ?? TIPS[key] ?? "", "aria-describedby": tipId(key) };
}

/** The texts the items' aria-describedby point at: once in the page, for every tab (App). */
export function TipTexts() {
  return (
    <div hidden>
      {Object.entries(TIPS).map(([key, text]) => (
        <span key={key} id={tipId(key)}>
          {text}
        </span>
      ))}
    </div>
  );
}

/** A listing error that means the folder is not there (any more): not found, or no longer a folder. */
const isMissing = (e: unknown) => /\(os error (2|3|267)\)/.test(String(e));

/** A listing or preview error that means this account may not read it (access denied). */
const isDenied = (e: unknown) => /\(os error 5\)/.test(String(e));

/** What a folder read through the admin session refuses (it is read-only). */
const READ_ONLY = "관리자 권한으로 읽는 중이라 읽기만 할 수 있어요. 바꾸거나 옮기거나 지울 수 없어요";

/** Where the list of plans is, for the toasts: under 정리 ▾, which a narrow window folds into "⋯". */
const PLANS_AT = '툴바의 "정리 ▾ → 정리 계획 목록…"(창이 좁으면 "⋯" 안)';

/** The focus is in a box being typed in (a rename, the search box, the address): a dialog opened now would take it away. */
function typingNow(): boolean {
  const a = document.activeElement;
  if (a instanceof HTMLInputElement) return a.type !== "checkbox";
  return a instanceof HTMLTextAreaElement || (a instanceof HTMLElement && a.isContentEditable);
}

/** create_folder's answer when the name is taken (by a folder or a file). */
const NAME_TAKEN = "같은 이름이 이미 있어요";

/** The Explorer menu takes items of one folder only (shellmenu.rs). */
const MIXED_FOLDERS = "같은 폴더 안의 항목에만 Windows 메뉴를 띄울 수 있어요. 하나만 골라 주세요";

/** The "Windows 메뉴 더 보기…" item's tooltip: its shortcuts, and whose rules its commands follow. */
const SHELL_TITLE =
  "탐색기의 메뉴예요 (Shift+오른쪽 클릭, Shift+F10). 메뉴의 명령은 탐색기 규칙대로 동작해요 (삭제도 탐색기의 삭제예요)";

/** Files dropped on search results: they are no folder to put things in. */
const DROP_ON_SEARCH = "검색 결과에는 놓을 수 없어요. 폴더 위에 놓거나 검색을 닫고 놓아 주세요";

/**
 * How many children a comparison lists at most (lib.rs DIFF_LIMIT), the biggest changes first.
 * Past that, a row it leaves out changed by less than the smallest one listed, or not at all.
 */
const DIFF_LIMIT = 64;

// A search on screen runs again for live changes in its scope, once the previous run has ended
// and this long has passed since: one over every drive reads millions of names.
const RERUN_ALL_MS = 5000;
const RERUN_HERE_MS = 2000;

/** The side pane keeps at least this width, and the list beside it at least LIST_MIN. */
const PREVIEW_MIN = 220;
const LIST_MIN = 320;

const FOLDER_COLUMNS: Column[] = [
  { key: "name", label: "이름", width: "minmax(220px, 3fr)" },
  { key: "size", label: "크기", width: "minmax(150px, 1.4fr)", align: "right" },
  { key: "files", label: "파일 수", width: "90px", align: "right" },
  { key: "modified", label: "수정한 날짜", width: "140px" },
  { key: "type", label: "종류", width: "minmax(90px, 0.8fr)" },
];

/** With "크기 변화" on (보기 ▾): the change since the scan before, right after the size. */
const FOLDER_DIFF_COLUMNS: Column[] = [
  ...FOLDER_COLUMNS.slice(0, 2),
  {
    key: "change",
    label: "크기 변화",
    width: "minmax(104px, 0.9fr)",
    align: "right",
    title: '지난 스캔 뒤로 늘고 준 크기예요. 누르면 이 열로 정렬하고, 보기 ▾ → "크기 변화 (지난 스캔과 비교)"로 켜고 꺼요',
  },
  ...FOLDER_COLUMNS.slice(2),
];

/** With "위험도 (AI 판정)" on (보기 ▾; jev or a local model): its badge for each row, last. */
const RISK_COLUMN: Column = { key: "risk", label: "위험도", width: "64px" };
// A local model's badges say "로컬" too ("로컬 안전").
const RISK_COLUMN_LOCAL: Column = { ...RISK_COLUMN, width: "84px" };

const SEARCH_COLUMNS: Column[] = [
  { key: "name", label: "이름", width: "minmax(200px, 2fr)" },
  { key: "location", label: "위치", width: "minmax(200px, 2.4fr)" },
  { key: "size", label: "크기", width: "minmax(130px, 1fr)", align: "right" },
  { key: "modified", label: "수정한 날짜", width: "140px" },
];

/** JEV_SHOW as the app says it: "40%". */
const SHOW_PCT = `${Math.round(JEV_SHOW * 100)}%`;

/** An AI 검색's results: the judge's score right after the name, as it is what they are ordered by. */
const aiColumns = (title: string): Column[] => [
  SEARCH_COLUMNS[0],
  { key: "score", label: "일치도", width: "72px", title: `${title}\n${SHOW_PCT} 미만은 처음에 숨겨요. 누르면 이 열로 정렬해요` },
  ...SEARCH_COLUMNS.slice(1),
];
const AI_COLUMNS: Record<JudgeEngine, Column[]> = {
  jev: aiColumns(
    "일치도 (TypeSafe jev): 찾는 말에 얼마나 맞는지 jev가 이름·경로 같은 정보만 보고 매긴 값이에요 (파일 내용은 안 봐요, 참고용).",
  ),
  // A local model answers in three steps (맞아요 90%, 아마 60%, 아니에요 10%), not a fine score.
  ollama: aiColumns(
    "일치도 (로컬 모델): 찾는 말에 맞는지 로컬 모델(Ollama)이 이름·경로 같은 정보만 보고 고른 값이에요 (파일 내용은 안 봐요, 참고용).\n" +
      "로컬 모델은 맞아요(90%) · 아마(60%) · 아니에요(10%) 세 단계로만 답해요.",
  ),
};

// ---- v0.9: AI 검색 (v0.9 "뜻으로 찾기"; v0.10 jev or a local model) ----

/**
 * Candidates an AI 검색 asks the judge about at most (meaning.rs): files, and games·programs; for
 * what the asking dialogs say (a search's own cap comes with it: JevFound.cap). A local model is
 * asked one candidate at a time, so it is asked about fewer.
 */
const JEV_FILE_MAX = 240;
const JEV_APP_MAX = 400;
const LOCAL_FILE_MAX = 100;
const LOCAL_APP_MAX = 150;

/** The engine that answered a search. */
const engineOf = (f: JevFound): JudgeEngine => (f.engine === "ollama" ? "ollama" : "jev");

/** "jev가", "로컬 모델이": the judge as the subject of a sentence. */
const judgeSubject = (e: JudgeEngine) => (e === "ollama" ? "로컬 모델이" : "jev가");

const JEV_KIND: Record<JevKind, string> = {
  document: "문서",
  image: "사진·그림",
  video: "동영상",
  audio: "음악·소리",
  archive: "압축 파일",
  program: "프로그램·설치 파일",
};

const JEV_WHEN: Record<JevWhen, string> = {
  today: "오늘",
  yesterday: "어제",
  this_week: "이번 주",
  last_week: "지난주",
  this_month: "이번 달",
  last_month: "지난달",
  this_year: "올해",
  last_year: "작년",
  older: "작년보다 전",
};

/** What the list says with no row to show of an AI 검색. `hidden`: hits kept back under JEV_SHOW. */
function jevEmpty(f: JevFound, hidden: number): string {
  const who = judgeSubject(engineOf(f));
  if (hidden > 0) return `${who} 맞다고 본 항목이 없어요. 위의 "${SHOW_PCT} 미만도 보기"로 나머지 ${hidden.toLocaleString()}개를 볼 수 있어요`;
  // Stopped before anything was judged (even before the query was read: no candidates, "file"),
  // which says nothing about the candidates.
  if (stoppedEarly(f)) return "멈췄어요. 아직 확인한 항목이 없어요. Enter를 누르면 다시 찾아요";
  if (f.cancelled) return `멈췄어요. 확인한 ${f.asked.toLocaleString()}개 중에는 ${who} 맞다고 본 항목이 없어요`;
  if (f.candidates === 0) {
    return f.target === "app"
      ? "후보가 될 게임·프로그램을 찾지 못했어요. 설치한 드라이브를 스캔했는지 확인해 주세요"
      : "후보가 될 파일을 찾지 못했어요. 이름에 들어 있을 만한 낱말로 바꿔 보세요";
  }
  return `${who} 맞다고 본 항목이 없어요. 다른 말로 찾아 보세요`;
}

/** An AI 검색 stopped before the judge answered for any candidate. */
const stoppedEarly = (f: JevFound) => f.cancelled && f.asked === 0;
/** Stopped before the query was read (what to look for, how to gather): its target and kind mean nothing. */
const stoppedUnread = (f: JevFound) => f.cancelled && f.candidates === 0;

/**
 * What an AI 검색 left out. `capped`: more candidates than the cap, which only the first `cap`
 * of were asked about (files: those with the most words; games·programs: the biggest, as they are
 * all of them whatever the words). `unanswered`: under the cap, a request the judge did not
 * answer; a search stopped early (`cancelled`) leaves out the rest by that instead.
 */
function jevLeftOut(f: JevFound) {
  const cap = Math.min(f.candidates, f.cap);
  return { cap, capped: f.candidates > f.cap, unanswered: f.cancelled ? 0 : Math.max(0, cap - f.asked) };
}

/**
 * The status bar's line for an AI 검색: how many fit, what was asked, how long it took, what the
 * judge took it for, what was left out. "jev 요청 R번" for jev; "로컬 모델 · 요청 R번" for a local one.
 */
function jevLine(f: JevFound, matched: number): string {
  const app = f.target === "app";
  const engine = engineOf(f);
  const what = app
    ? "게임·프로그램"
    : [f.kind && `종류: ${JEV_KIND[f.kind] ?? f.kind}`, f.when && `기간: ${JEV_WHEN[f.when] ?? f.when}`]
        .filter(Boolean)
        .join(" · ") || "파일·폴더";
  const requests = `${engine === "ollama" ? "로컬 모델 · 요청" : "jev 요청"} ${f.requests.toLocaleString()}번`;
  const asked = f.requests === 0 ? `${requests} (저장해 둔 답)` : requests;
  if (stoppedUnread(f)) return `AI 검색: 멈췄어요 — 아직 확인한 항목이 없어요 · ${requests} · ${jevTime(f.elapsed_ms)}`;
  const out = jevLeftOut(f);
  const notes = [
    out.capped && (app ? `큰 것부터 ${out.cap.toLocaleString()}개만 확인` : `후보가 많아 ${out.cap.toLocaleString()}개만 확인`),
    out.unanswered > 0 && `${judgeSubject(engine)} 답하지 않은 ${out.unanswered.toLocaleString()}개 빠짐`,
    f.unplaced > 0 && `폴더를 찾지 못한 프로그램 ${f.unplaced.toLocaleString()}개 빠짐`,
  ].filter(Boolean);
  return (
    `AI 검색: ${f.cancelled ? `멈췄어요 — 확인한 ${f.asked.toLocaleString()}개만 보여요 · ` : ""}` +
    `맞는 항목 ${matched.toLocaleString()}개 · 후보 ${f.candidates.toLocaleString()}개 중 ` +
    `${f.asked.toLocaleString()}개 확인 · ${asked} · ${jevTime(f.elapsed_ms)} · ${what}` +
    notes.map((n) => ` · ${n}`).join("")
  );
}

/** What "제거 (제어판처럼)…" does for a program, or why it cannot (the item is then off). */
function uninstallNote(p: ProgramRef): string {
  if (!p.can_uninstall) {
    // Win32: no uninstaller registered, one that is not there (or not a full path), or removing
    // turned off (NoRemove). Which one is not known here, so none is named.
    return p.kind === "store"
      ? "Windows가 지우지 못하게 한 앱이에요"
      : "여기서는 제거할 수 없어요. Windows 설정 → 앱에서 확인해 주세요";
  }
  return p.kind === "store" ? `${shown(p.name)} · 이 사용자에서 제거해요` : `${shown(p.name)}의 제거 프로그램을 실행해요`;
}

/** "0.8초"; a search the cache answered is quicker than that. */
const jevTime = (ms: number) => (ms < 100 ? `${ms}ms` : `${(ms / 1000).toFixed(1)}초`);

/** A path after `from` was renamed to `to`: itself, or an item inside it; null when neither. */
function renamedPath(p: string, from: string, to: string): string | null {
  const k = pathKey(p);
  const f = pathKey(from);
  if (k === f) return to;
  return isSameOrInside(k, f) ? to + p.slice(from.length) : null;
}

/** Whether a listing answers the folder now shown (case and trailing separator aside). */
function sameListing(a: string, b: string) {
  return a.replace(/\\$/, "").toLowerCase() === b.replace(/\\$/, "").toLowerCase();
}

// One collator for every sort: localeCompare with an options object builds a new one per call.
const cmp = new Intl.Collator("ko", { numeric: true }).compare;

// "변화" sorts by the comparison and "일치도" by jev's scores, which only the tab has.
const SORTERS: Record<Exclude<SortKey, "change" | "score">, (a: Entry, b: Entry) => number> = {
  name: (a, b) => cmp(a.name, b.name),
  size: (a, b) => a.size - b.size,
  files: (a, b) => a.files - b.files,
  modified: (a, b) => a.modified - b.modified,
  type: (a, b) => cmp(typeLabel(a.name, a.is_dir), typeLabel(b.name, b.is_dir)),
  location: (a, b) => cmp(a.path, b.path),
};

/**
 * Whether a shortcut's letter key is `letter` (lower case). By the character first, so other
 * layouts (Dvorak) keep their letters; by the key's place when the character is no Latin letter
 * (the Korean IME in 한글 mode).
 */
export function isLetter(ev: KeyboardEvent, letter: string): boolean {
  const k = ev.key.toLowerCase();
  return /^[a-z]$/.test(k) ? k === letter : ev.code === `Key${letter.toUpperCase()}`;
}

/** The menu key, or Shift+F10: Windows' keys for a context menu. */
const isMenuKey = (ev: KeyboardEvent) =>
  (ev.key === "ContextMenu" && !ev.ctrlKey && !ev.altKey) ||
  (ev.key === "F10" && ev.shiftKey && !ev.ctrlKey && !ev.altKey && !ev.metaKey);

/** Whether the Explorer menu can take `items`: all in one folder (search results may mix them). */
function oneFolder(items: Entry[]): boolean {
  if (!items.length) return false;
  const up = pathKey(parentOf(items[0].path) ?? "");
  return items.every((e) => pathKey(parentOf(e.path) ?? "") === up);
}

/** "+35%", "−4.2%", or null without a size to compare with. */
function percentText(delta: number, prev: number): string | null {
  if (prev <= 0) return null;
  const r = (Math.abs(delta) / prev) * 100;
  const sign = delta < 0 ? "−" : "+";
  if (r >= 10) return `${sign}${r.toFixed(0)}%`;
  if (r >= 0.1) return `${sign}${r.toFixed(1)}%`;
  return r > 0 ? `${sign}0.1% 미만` : null;
}

/** "+1.2 GB", "−300 MB". */
const signedSize = (delta: number) => `${delta < 0 ? "−" : "+"}${formatSize(Math.abs(delta))}`;

/** The list's comparison of one folder with the scan before (its "변화" column). */
interface Compared {
  path: string;
  root: DiffNode | null;
  info: DiffInfo | null;
  /** Why there is none (no scan before this one, the folder is not in the scan…), as the backend says. */
  error: string | null;
}

/** Where files dropped on a tab (not on a folder in it) go, or why they may not. */
export type DropInto = { path: string } | { refuse: string };

/** Changes on disk, from live updates or made by this app, that a tab checks against what it shows. */
export interface Changes {
  /** Folders (pathKey) whose direct children changed. */
  dirs: Set<string>;
  /** Drives ("C:") where every folder counts as changed: too many to list. */
  full: Set<string>;
  /** Folders removed or renamed away (pathKey). */
  removed: Set<string>;
  /** A live payload came without `removed`: a changed folder above may be one that went away. */
  unsure: boolean;
}

/** Changes made by this app: the folders whose contents changed, and the items gone from them. */
export function changesOf(dirs: (string | null)[], removed: string[] = []): Changes {
  return {
    dirs: new Set(dirs.filter((d): d is string => !!d).map(pathKey)),
    full: new Set(),
    removed: new Set(removed.map(pathKey)),
    unsure: false,
  };
}

// One trash at a time, across tabs: a second Delete while a long one runs (here, or in another
// tab showing the same folder) would send the same paths again.
const trashLock = { busy: false };

interface Menu {
  x: number;
  y: number;
  /** A tree row: the menu acts on it alone, not on the list selection. */
  target?: Entry;
  /** The empty area around the rows: the menu acts on the folder shown. */
  background?: boolean;
}

/** AI settings and which keys are saved (App reads them once and after the settings close). */
export interface AiState {
  settings: AiConfig;
  keys: KeyStatus;
}

type AiPlanTask = "structure" | "rename";
type RuleKind = "cleanup" | "dedupe" | "shrink" | "relocate";

const RULE_ITEMS: { kind: RuleKind; label: string; note: string }[] = [
  { kind: "cleanup", label: "정리 후보", note: "캐시·임시 파일, 오래된 설치 파일 (휴지통으로)" },
  { kind: "dedupe", label: "중복 정리", note: "내용이 같은 파일은 하나만 남기기 (휴지통으로)" },
  { kind: "shrink", label: "용량 줄이기", note: "큰 파일을 Windows 압축으로 (언제든 풀 수 있어요)" },
  { kind: "relocate", label: "다른 드라이브로 옮기기", note: "큰 폴더를 옮기고 링크 남기기" },
];

const AI_ITEMS: { task: AiPlanTask; label: string; note: string }[] = [
  { task: "structure", label: "구조 정리 (AI)", note: "하위 폴더로 나눠 옮기는 계획" },
  { task: "rename", label: "이름 정리 (AI)", note: "이름을 한 가지 규칙으로 맞추는 계획" },
];

const PROVIDER_NAME: Record<AiConfig["provider"], string> = {
  off: "꺼짐",
  claude: "Claude API",
  openai: "OpenAI API",
  ollama: "Ollama",
  claude_cli: "Claude Code CLI",
  codex_cli: "Codex CLI",
};

/** What AI-only buttons say while AI is off: where to turn it on. */
const AI_OFF_HELP =
  "AI가 꺼져 있어요. 툴바의 톱니바퀴(AI 설정)에서 쓸 AI(내 API 키, Ollama, 내가 로그인한 CLI)를 고르면 쓸 수 있어요. 규칙으로 만드는 계획은 AI 없이 동작해요";

/** Why AI requests cannot go now, in words the user can act on; null when they can. */
function aiBlocker(s: AiState | null): string | null {
  if (!s) return "AI 설정을 읽지 못했어요. 툴바의 톱니바퀴(AI 설정)을 열어 확인해 주세요";
  const { settings, keys } = s;
  if (settings.provider === "off") return AI_OFF_HELP;
  if (settings.provider === "claude" && !keys.claude) return "Claude API 키가 없어요. 툴바의 톱니바퀴(AI 설정)에서 키를 넣어 주세요";
  if (settings.provider === "openai" && !keys.openai) return "OpenAI API 키가 없어요. 툴바의 톱니바퀴(AI 설정)에서 키를 넣어 주세요";
  return null;
}

/**
 * Why the judge (위험도, AI 검색) cannot be asked now, in words the user can act on; "" when it
 * can: the master switch is on and the engine is ready (jev: its key saved; a local model: a model
 * named; whether Ollama runs is known only once asked).
 */
function judgeBlocker(s: AiState | null): string {
  if (!s) return "AI 설정을 읽지 못했어요. 잠시 뒤에 다시 해 주세요";
  if (!s.settings.jev_enabled) return 'AI 판정이 꺼져 있어요. AI 설정에서 "AI 판정 쓰기"를 켜 주세요';
  if (judgeOf(s.settings) === "jev") {
    return s.keys.jev ? "" : "jev 키가 없어요. AI 설정에서 jev 키를 넣거나, 무료 로컬 모델(Ollama)을 골라 주세요";
  }
  return judgeModel(s.settings) ? "" : "판정할 로컬 모델이 없어요. AI 설정에서 Ollama 모델 이름(예: qwen3:4b)을 넣어 주세요";
}

/** The judge can be asked (see judgeBlocker). */
const judgeReady = (s: AiState | null): s is AiState => judgeBlocker(s) === "";

/** judgeBlocker in a few words, under the menu item it keeps off (보기 ▾ → "위험도 (AI 판정)"). */
function judgeNote(s: AiState | null): string {
  if (!s) return "AI 설정을 읽지 못했어요. 누르면 다시 읽어요";
  if (!s.settings.jev_enabled) return "AI 설정에서 AI 판정을 켜면 써요";
  if (judgeOf(s.settings) === "jev") return "AI 설정에서 jev 키를 넣으면 써요";
  return "AI 설정에서 Ollama 모델 이름을 넣으면 써요";
}

/** Which judge answers, and with what (a local model's name, its address): answers of two never mix. */
const judgeKey = (s: AiConfig) =>
  judgeOf(s) === "jev" ? "jev" : `ollama:${judgeModel(s)}@${(s.ollama_url ?? "").trim()}`;

/**
 * Items asked about at once (what is in sight of the list; jev_risk takes at most 500). A local
 * model takes a while for each: fewer, so the badges come in bit by bit.
 */
const RISK_BATCH: Record<JudgeEngine, number> = { jev: 200, ollama: 40 };
/** An item the judge gave no answer for (or a request that failed) is asked about again after this long. */
const RISK_RETRY_MS = 5 * 60 * 1000;
/** After a failed request a tab waits this long before it asks again. */
const RISK_PAUSE_MS = 30 * 1000;
const RISK_CACHE_MAX = 20000;

/**
 * The judge's answers by judgeKey and pathKey, shared by every tab: an item is asked about once,
 * whichever tab shows it. `risk` null: asked, nothing to show (no answer, or the request failed).
 */
const riskCache = new Map<string, { risk: Risk | null; at: number }>();
function rememberRisk(key: string, risk: Risk | null) {
  riskCache.delete(key);
  riskCache.set(key, { risk, at: Date.now() });
  // The oldest go first.
  while (riskCache.size > RISK_CACHE_MAX) riskCache.delete(riskCache.keys().next().value!);
}
const riskKnown = (key: string) => {
  const c = riskCache.get(key);
  return !!c && (c.risk !== null || Date.now() - c.at < RISK_RETRY_MS);
};

/** A dialog of this tab (the trash question has its own state). */
type TabDialog =
  | { kind: "dups" }
  | { kind: "relocs" }
  | { kind: "relocate"; source: Entry }
  // v0.6: the AI settings, the plans, one plan (the AI panel is the side pane's 정리 도우미 now).
  // `thenAi`: opened from AI 검색's "AI 설정 열기": closed with a judge ready, the box turns to AI.
  | { kind: "aiSettings"; thenAi?: boolean }
  | { kind: "plans" }
  // `inbox`: opened from the list of plans, which comes back when it closes.
  | { kind: "plan"; id: string; inbox?: boolean }
  // What an AI plan asked for from the menu would send, before it goes ("보내기 전에 미리 보기").
  // `changed`: made again on "보내기", it was not what had been shown (shown again, nothing sent).
  | { kind: "aiSend"; task: AiPlanTask; path: string; payload: AiPayloadPreview; changed?: boolean }
  // Turning the list's 위험도 on, when the settings say to ask before sending and it would leave
  // this PC (jev, an Ollama elsewhere): `engine` and `url` say where to.
  | { kind: "riskAsk"; engine: JudgeEngine; url: string; target: string }
  // v0.9: the search box's AI 검색, likewise: the first time in this tab, and again on Enter
  // when the judge's destination (`target`, judgeTarget) changed since. `run`: agreed, it searches.
  // `changed`: another destination was said before (the warning says it changed).
  | { kind: "aiFindAsk"; engine: JudgeEngine; url: string; target: string; run?: boolean; changed?: boolean }
  // v0.10: AI 검색 with no judge ready: the two ways to get one (`why`: what is missing).
  | { kind: "aiHow"; why: string }
  // v0.10: "제거 (제어판처럼)…": a folder that is an installed program's own.
  | { kind: "uninstall"; prog: ProgramRef; path: string }
  // v0.7: installed programs and the folders removed ones left behind.
  | { kind: "programs" };

/**
 * v0.10: the AI 검색 out, one at a time for the whole window (the backend's ai_find_cancel stops
 * whichever runs, and its progress events name none): every tab asking the same waits for it
 * (sent once, billed once), one asking something else waits for it to end. `waiters`: the tabs
 * that wait for its answer; once none does, it is stopped (findCancel). `watch`: the tabs that
 * wait, told when the waiters change (their stop button says what it does: stop, or stop waiting).
 */
const aiRun: {
  current: { key: string; done: Promise<JevFound>; waiters: Set<number> } | null;
  watch: Set<() => void>;
} = { current: null, watch: new Set() };

const aiRunChanged = () => {
  for (const f of aiRun.watch) f();
};

/** Tab `id` no longer waits for the AI 검색 out: stopped once no tab does. */
function leaveAiRun(id: number) {
  const run = aiRun.current;
  if (!run || !run.waiters.delete(id)) return;
  aiRunChanged();
  if (run.waiters.size) return;
  ai.findCancel().catch(() => {});
}

/** A folder this account cannot open, as the admin session read it. */
interface AdminListing {
  path: string;
  entries: Entry[];
}

interface Results {
  /** Query and scope these items answer; a refresh of the same search keeps the selection. */
  key: string;
  items: Entry[];
  total: number;
  ms: number;
  /** v0.9: the answer of an AI 검색: `items` are its hits' entries, `hits` them by path. */
  jev?: JevResults;
}

interface JevResults {
  /** The query asked (trimmed) and where ("이 폴더만": that folder; null: every drive), which the box may no longer hold. */
  query: string;
  scope: string | null;
  found: JevFound;
  hits: Map<string, JevHit>;
}

/** Results of an AI 검색 whose hits are `hits` (the answer, or it patched after a rename or trash). */
function jevResults(key: string, query: string, scope: string | null, found: JevFound, hits: JevHit[]): Results {
  return {
    key,
    items: hits.map((h) => h.entry),
    total: hits.length,
    ms: found.elapsed_ms,
    jev: { query, scope, found: { ...found, items: hits }, hits: new Map(hits.map((h) => [h.entry.path, h])) },
  };
}

// An AI 검색 is not asked again after a change (jev bills it, a local model takes its time): its
// results follow the change instead, an item renamed here under its new name, one gone (trash,
// another program, an uninstaller) left out.
function jevRenamed(r: Results, j: JevResults, from: string, to: string): Results {
  const name = to.slice(to.lastIndexOf("\\") + 1);
  const hits = j.found.items.map((h) => {
    const at = renamedPath(h.entry.path, from, to);
    return at === null ? h : { ...h, entry: { ...h.entry, path: at, name: at === to ? name : h.entry.name } };
  });
  return jevResults(r.key, j.query, j.scope, j.found, hits);
}
function jevWithout(r: Results, j: JevResults, gone: string[]): Results {
  const keys = gone.map(pathKey);
  const hits = j.found.items.filter((h) => {
    const k = pathKey(h.entry.path);
    return !keys.some((g) => isSameOrInside(k, g));
  });
  return hits.length === j.found.items.length ? r : jevResults(r.key, j.query, j.scope, j.found, hits);
}

/** The side pane's tabs: 상세 정보, and the 정리 도우미. */
export type PaneTab = "details" | "helper";

/** The side pane, one for every tab (App's): open, the tab it shows, its width. */
export interface PaneState {
  open: boolean;
  tab: PaneTab;
  width: number;
}

export interface ExplorerHandle {
  /** Opens a folder in this tab (the sidebar). */
  navigate: (path: string) => void;
  /** Changes on disk: the tab shown acts on them now, a hidden one once it shows again. */
  apply: (c: Changes) => void;
  /** After a scan: everything shown lists again (now, or once the tab shows again). */
  resync: () => void;
  /** A dialog of this tab is open, so the tab shortcuts wait. */
  busy: () => boolean;
  /** Files dropped on this tab, not on a folder row in it: where they go; null to leave them (a dialog is open). */
  dropTarget: () => DropInto | null;
  /** The sidebar's "프로그램 정리": opens it over this tab (not while a dialog of the tab is open). */
  openPrograms: () => void;
}

export interface ExplorerProps {
  ref?: React.Ref<ExplorerHandle>;
  id: number;
  /** The tab shown. Only it owns the keyboard and lists changes at once. */
  active: boolean;
  initialPath: string;
  initialView: View;
  drives: DriveInfo[];
  elevated: boolean;
  scanning: string | null;
  onScan: (letter: string) => void;
  refreshDrives: () => Promise<unknown>;
  /** Banners shared by every tab (live updates lost), shown under the toolbar. */
  banners: React.ReactNode;
  /** The side pane: only the tab shown draws it. */
  pane: PaneState;
  /** "상세 정보" (Alt+P), "정리 도우미 (옆 창)" (Alt+H): the pane on that tab, or closed when that tab shows. */
  onTogglePane: (tab: PaneTab) => void;
  /** The pane on `tab` (its tabs, a menu's "정리 도우미로 보기"), or closed (null). */
  onShowPane: (tab: PaneTab | null) => void;
  onPaneWidth: (width: number) => void;
  toast: (text: string, kind?: "info" | "error") => void;
  /** The folder and view shown, for the tab's title and the tabs saved for next time. */
  onState: (id: number, path: string, view: View) => void;
  /** `show`: switch to the new tab; otherwise it opens behind this one. */
  onOpenTab: (path: string, show: boolean) => void;
  /** Ctrl+V: the clipboard's files go into `dest` (App runs the job). */
  onPaste: (dest: string) => void;
  /** A folder's move to another drive started (job `id`): App tells the tabs once it ends. */
  onRelocateStarted: (id: number, source: string) => void;
  /** This tab changed something on disk: the other tabs check what they show. */
  onChanged: (from: number, c: Changes) => void;
  /** AI settings and saved keys; null until read (or without a backend). */
  ai: AiState | null;
  /** The AI settings may have changed (a settings dialog closed): App reads them again. */
  onAiChanged: () => void;
  /** Plans proposed over MCP that wait as drafts: the badge on "정리 ▾". */
  planDrafts: number;
  /** A plan was made, changed or removed here: App counts the drafts again. */
  onPlansChanged: () => void;
  /** The toolbar's "?": the help (App's, as F1 opens it), at `topic` when given. */
  onHelp: (topic?: string) => void;
  /** v0.8.1: whether a folder is in the sidebar's 즐겨찾기 (pinned, or a known place shown). */
  pinned: (path: string) => boolean;
  /** "즐겨찾기에 고정" / "즐겨찾기에서 고정 해제" of a folder's menu. */
  onPin: (path: string, pin: boolean) => void;
}

/** One tab: its own history, folder, view, search, selection and tree. */
export function Explorer({ ref, ...p }: ExplorerProps) {
  const { id, active, toast, onState, onChanged, onAiChanged } = p;

  // History and position live in one state so a move never reads a stale position.
  const [nav, setNav] = useState<{ list: string[]; pos: number }>(() => ({ list: [p.initialPath], pos: 0 }));
  const history = nav.list;
  const pos = nav.pos;
  const path = history[pos] ?? "";
  const [address, setAddress] = useState("");
  const [listing, setListing] = useState<Listing | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);

  // The search box: its two modes (v0.10), each with its own query and results. "name": the name
  // search, as one types (`query`, `results`); "ai": AI 검색, on Enter (`aiQuery`, `aiRes`).
  const [mode, setMode] = useState<"name" | "ai">("name");
  const [query, setQuery] = useState("");
  const [aiQuery, setAiQuery] = useState("");
  const [scopeHere, setScopeHere] = useState(false);
  const [results, setResults] = useState<Results | null>(null);
  const [aiRes, setAiRes] = useState<Results | null>(null);
  // As of the last render, for callbacks made once (navigate) and for live changes.
  const aiResRef = useRef<Results | null>(null);
  const modeRef = useRef(mode);
  const [searchNonce, setSearchNonce] = useState(0);
  const shownSearch = useRef("");
  const [editingAddress, setEditingAddress] = useState(false);

  const [sort, setSort] = useState<{ key: SortKey; desc: boolean }>({ key: "size", desc: true });
  const [selected, setSelected] = useState<Set<string>>(new Set());
  // Focus and range anchor are paths, so they stay on the same item when rows re-sort or reload.
  const [focusPath, setFocusPath] = useState<string | null>(null);
  const anchor = useRef<string | null>(null);
  const [renaming, setRenaming] = useState<string | null>(null);
  // A folder just made here (pathKey): once the listing has it, it is selected and named.
  const [made, setMade] = useState<string | null>(null);
  const making = useRef(false);
  const [menu, setMenu] = useState<Menu | null>(null);
  const menuBox = useRef<HTMLDivElement>(null);
  // Whether the clipboard holds files, asked when a menu opens; null while unknown.
  const [canPaste, setCanPaste] = useState<boolean | null>(null);
  const clipAsk = useRef(0);
  const [confirmTrash, setConfirmTrash] = useState<Entry[] | null>(null);
  const [dialog, setDialog] = useState<TabDialog | null>(null);
  // A folder this account may not open, read through the admin session (read-only).
  const [admin, setAdmin] = useState<AdminListing | null>(null);
  const [adminAsking, setAdminAsking] = useState(false);
  // A file to select once its folder lists ("위치 보기" in the duplicate finder): pathKeys.
  const [reveal, setReveal] = useState<{ dir: string; file: string } | null>(null);
  // Bumped by F5 and the refresh button: the size map draws again.
  const [mapNonce, setMapNonce] = useState(0);
  // The toolbar's second row (the command bar), and the address's breadcrumbs.
  const cmdBar = useRef<HTMLDivElement>(null);
  const crumbsBox = useRef<HTMLDivElement>(null);
  const searchBox = useRef<HTMLInputElement>(null);
  const root = useRef<HTMLDivElement>(null);

  const [view, setView] = useState<View>(p.initialView);
  const [treeRefresh, setTreeRefresh] = useState<TreeRefresh>({ n: 0, dirs: null, gone: [] });
  // The tree's focused row, which the list selection does not show: the preview and Ctrl+C use it.
  const [treeFocus, setTreeFocus] = useState<{ e: Entry; sized: boolean } | null>(null);
  // Tiles per row in the icon view, so Up and Down move by a row.
  const [columns, setColumns] = useState(1);
  // The list's "변화" column, this tab's own switch, and the comparison it shows.
  const [diffOn, setDiffOn] = useState(false);
  const [diff, setDiff] = useState<Compared | null>(null);
  // Where files dropped on this tab go (dropTarget), as of the last render.
  const dropRef = useRef<DropInto | null>(null);
  // The list's "위험도" column (the judge: jev or a local model), this tab's own switch; the rows
  // in sight it asks about; a bump when answers come in.
  const [riskOn, setRiskOn] = useState(false);
  const [range, setRange] = useState<[number, number]>([0, 0]);
  const [riskTick, setRiskTick] = useState(0);
  // The 정리 도우미's target while "고정" is on; null: it follows (one item picked, else the folder shown).
  const [helperPin, setHelperPin] = useState<HelperTarget | null>(null);
  // A plan being made from the menu (rules, or asking an AI), for the status bar; one at a time.
  const [working, setWorking] = useState<string | null>(null);
  const planning = useRef(false);
  // "보내기" in an AI preview: what goes is being made again to compare with what was shown.
  const [sendCheck, setSendCheck] = useState(false);
  // v0.9: AI 검색, the search box's "AI" mode. Typing searches nothing (jev bills each run, a
  // local model takes its time): Enter asks (runAi). `aiAgreed`: where what goes was said to go,
  // once in this tab ("보내기 전에 미리 보기"; judgeTarget). The run itself is aiRun (one for the
  // window). `jevBusy`: the tab waits for the run that `jevReq` names (bumped, its answer is
  // dropped); `jevQueued`: for the one before it to end; `aiWait`: that run's key. `aiStopping`:
  // the stop button (or Esc) asked it to stop; what was answered comes once the request out ends.
  // `aiProgress`: its requests answered (a local model's: candidates). `aiEngine`: the judge it asked. `jevLow`: the hits under
  // JEV_SHOW show too.
  const aiAgreed = useRef<string | null>(null);
  const jevReq = useRef(0);
  const aiWait = useRef<string | null>(null);
  const [jevBusy, setJevBusy] = useState(false);
  const [jevQueued, setJevQueued] = useState(false);
  const [aiStopping, setAiStopping] = useState(false);
  const [aiProgress, setAiProgress] = useState<FindProgress | null>(null);
  const [aiEngine, setAiEngine] = useState<JudgeEngine>("jev");
  const [jevLow, setJevLow] = useState(false);
  // v0.10: the installed program a folder of the menu is ("제거 (제어판처럼)…"), as program_at
  // answered; an uninstaller being started.
  const [menuProg, setMenuProg] = useState<{ path: string; prog: ProgramRef | null } | null>(null);
  const progReq = useRef(0);
  const [uninstalling, setUninstalling] = useState(false);

  // Opening a folder leaves the name search; AI 검색's results give way to the folder too (they
  // are kept: "AI" shows them again), while with none on screen the box stays in AI mode.
  const navigate = useCallback((to: string) => {
    const target = normalizePath(to);
    setQuery("");
    if (modeRef.current === "ai" && aiResRef.current) setMode("name");
    if (!target) return;
    // Opening the folder already shown adds no history entry (Back would seem to do nothing).
    setNav((n) =>
      (n.list[n.pos] ?? "").toLowerCase() === target.toLowerCase()
        ? n
        : { list: [...n.list.slice(0, n.pos + 1), target], pos: n.pos + 1 },
    );
  }, []);

  // Back and Forward leave search mode, like opening a folder does.
  const go = useCallback((step: number) => {
    setNav((n) => {
      const p = n.pos + step;
      return p >= 0 && p < n.list.length ? { ...n, pos: p } : n;
    });
    setQuery("");
    if (modeRef.current === "ai" && aiResRef.current) setMode("name");
  }, []);

  // Reads a folder this account may not open through the admin session. `ask`: the user asked
  // (the banner's button), so a session may start (one UAC prompt); otherwise only one already
  // running is used. The newest read for the folder still shown wins.
  const adminReq = useRef(0);
  const drivesRef = useRef(p.drives);
  useLayoutEffect(() => {
    drivesRef.current = p.drives;
  }, [p.drives]);
  const readAsAdmin = useCallback(
    async (here: string, ask: boolean) => {
      const req = ++adminReq.current;
      const current = () => req === adminReq.current && pathRef.current === here;
      // A network place: the helper could not read it (no UAC prompt for nothing, no error
      // toast each time such a folder opens while a session runs).
      if (!adminCanRead(here, drivesRef.current)) {
        if (!ask) setAdmin(null);
        return;
      }
      if (ask) setAdminAsking(true);
      try {
        if (ask) {
          if (!(await ensureAdmin())) {
            if (current()) toast("관리자 권한을 받지 못했어요", "error");
            return;
          }
        } else if (!(await v04.adminStatus()).active) {
          // The session is over (10 minutes unused, or 끝내기): what it read may be out of date.
          if (current()) setAdmin(null);
          return;
        }
        const entries = await v04.adminList(here);
        if (current()) setAdmin({ path: here, entries });
      } catch (err) {
        if (!current()) return;
        if (!ask) setAdmin(null);
        toast(adminError(err), "error");
      } finally {
        if (ask) setAdminAsking(false);
      }
    },
    [toast],
  );

  // Only the newest listing request may update the view: an older one that returns late
  // (a slow live read, or a reload started before the user moved on) is dropped.
  const listReq = useRef(0);
  const reload = useCallback(
    async (live = false): Promise<"missing" | void> => {
      if (!path) return;
      const req = ++listReq.current;
      try {
        const l = await api.list(path);
        if (req !== listReq.current) return;
        setListing(l);
        setLoadError(null);
        setAdmin(null);
      } catch (e) {
        if (req !== listReq.current) return;
        // A live reload of a folder that went away leaves the view as it is: climb() replaces it.
        if (live && isMissing(e)) return "missing";
        setListing(null);
        setLoadError(String(e));
        // A folder this account may not open is read again through an admin session already
        // running (F5, opening the folder). Not for live changes: every read keeps the session
        // from ending, and a busy folder would keep it up for good.
        if (!isDenied(e)) setAdmin(null);
        else if (!live) await readAsAdmin(path, false);
      }
    },
    [path, readAsAdmin],
  );
  // Long tasks (scan, trash, rename) reload through this ref, so they list the folder shown when
  // they finish, not the one shown when they started.
  const reloadRef = useRef(reload);
  useLayoutEffect(() => {
    reloadRef.current = reload;
  }, [reload]);

  const under = scopeHere ? path : null;

  // Live index updates and search replies read the folder, query and scope shown when they
  // arrive, not when they subscribed or started.
  const pathRef = useRef(path);
  const queryRef = useRef(query);
  const underRef = useRef(under);
  const searchKey = useRef("");
  const activeRef = useRef(active);
  const busyRef = useRef(false);
  const dialogRef = useRef(dialog);
  // An AI 검색's answer, shown or kept for the "AI" mode, for live changes (apply).
  const jevShown = useRef<JevResults | null>(null);
  useLayoutEffect(() => {
    pathRef.current = path;
    queryRef.current = query;
    underRef.current = under;
    searchKey.current = `${query.trim()}\n${under ?? ""}`;
    activeRef.current = active;
    busyRef.current = confirmTrash !== null || dialog !== null;
    dialogRef.current = dialog;
    // In AI mode name searches neither run again for changes nor land.
    modeRef.current = mode;
    aiResRef.current = aiRes;
    jevShown.current = aiRes?.jev ?? null;
  });

  // Search runs. Typing (or a scan, rename, trash) starts one at once. A live re-run waits for
  // the run in flight and comes a while after it ended, so a slow search over every drive is
  // never cut short by the next one. The newest run that answers the query and scope shown wins.
  const searchRuns = useRef({ seq: 0, shown: 0, busy: 0, ended: 0, wanted: false, timer: null as number | null });
  const pumpRef = useRef(() => {});
  const runSearch = useCallback(
    async (q: string, scope: string | null, live: boolean) => {
      const s = searchRuns.current;
      const seq = ++s.seq;
      const key = `${q}\n${scope ?? ""}`;
      // Every change so far is in this run's answer.
      s.wanted = false;
      s.busy++;
      try {
        const r = await api.search(q, scope);
        // An older run's answer, or one for a query or scope no longer shown, or the box turned to AI.
        if (seq < s.shown || key !== searchKey.current || modeRef.current === "ai") return;
        s.shown = seq;
        setResults({ key, items: r.items, total: r.total, ms: r.elapsed_ms });
        if (key !== shownSearch.current) {
          shownSearch.current = key;
          setSelected(new Set());
          setFocusPath(null);
          anchor.current = null;
        }
      } catch (e) {
        // A live re-run fails quietly; the next change tries again.
        if (!live && key === searchKey.current && modeRef.current === "name") toast(String(e), "error");
      } finally {
        s.busy--;
        s.ended = Date.now();
        pumpRef.current();
      }
    },
    [toast],
  );
  // Starts the live re-run that is wanted, once nothing runs and the pause after the last run is
  // over. Not while the window is hidden (minimized) or another tab is shown: it runs when this
  // one shows again.
  const pumpSearch = useCallback(() => {
    const s = searchRuns.current;
    if (!s.wanted || s.busy || s.timer !== null || document.hidden || !activeRef.current) return;
    const q = queryRef.current.trim();
    // In AI mode the name search waits: it runs again as the box turns back to 이름. (An AI 검색 is
    // never run again by itself.)
    if (!q || modeRef.current === "ai") {
      s.wanted = false;
      return;
    }
    const scope = underRef.current;
    const wait = s.ended + (scope === null ? RERUN_ALL_MS : RERUN_HERE_MS) - Date.now();
    if (wait > 0) {
      s.timer = window.setTimeout(() => {
        s.timer = null;
        pumpRef.current();
      }, wait);
      return;
    }
    runSearch(q, scope, true);
  }, [runSearch]);
  useLayoutEffect(() => {
    pumpRef.current = pumpSearch;
  }, [pumpSearch]);

  // The name search as you type, in 이름 mode (again as the box turns back to it); AI 검색 only on
  // Enter (runAi).
  useEffect(() => {
    if (mode === "ai") return;
    const q = query.trim();
    if (!q) {
      setResults(null);
      shownSearch.current = "";
      return;
    }
    const t = setTimeout(() => runSearch(q, under, false), 120);
    return () => clearTimeout(t);
  }, [query, under, searchNonce, runSearch, mode]);

  // The AI box emptied (Esc, cleared by hand): its results go and the run out is not waited for.
  // It is stopped once no tab waits for it (what it already sent to jev is billed).
  useEffect(() => {
    if (aiQuery.trim()) return;
    setAiRes(null);
    jevReq.current++;
    aiWait.current = null;
    leaveAiRun(id);
    setJevBusy(false);
    setJevQueued(false);
    setAiStopping(false);
    setAiProgress(null);
  }, [aiQuery, id]);

  // A tab closed: the run it waited for is stopped when no other tab waits for it.
  useEffect(() => () => leaveAiRun(id), [id]);

  // The folder shown is gone (removed, or renamed by another program): the nearest folder above
  // it that is still there takes its place in the history, so Back does not lead to it again.
  // True when the view moved.
  const climb = useCallback(
    async (from: string) => {
      let up = parentOf(from);
      while (up) {
        try {
          await api.list(up);
          break;
        } catch (e) {
          // There but not listable (access denied): it shows its own error.
          if (!isMissing(e)) break;
          up = parentOf(up);
        }
      }
      // The user moved on meanwhile.
      if (pathRef.current !== from) return false;
      if (!up) {
        // Nothing above is there either (the drive went away): show why.
        await reloadRef.current();
        return false;
      }
      const to = up;
      toast(`${from} 폴더가 지워졌거나 이름이 바뀌었어요. 위 폴더를 보여 드려요.`);
      setNav((n) => {
        if (n.list[n.pos] !== from) return n;
        const list = [...n.list];
        if (n.pos > 0 && pathKey(list[n.pos - 1]) === pathKey(to)) {
          list.splice(n.pos, 1);
          return { list, pos: n.pos - 1 };
        }
        list[n.pos] = to;
        return { list, pos: n.pos };
      });
      return true;
    },
    [toast],
  );

  // The folder the tab opened with. When it is gone (a tab restored from the last session, a
  // folder removed since), the nearest one above it opens instead of an error.
  const opened = useRef<string | null>(p.initialPath);
  useEffect(() => {
    setAddress(path);
    setSelected(new Set());
    setFocusPath(null);
    anchor.current = null;
    setRenaming(null);
    setMade(null);
    setAdmin(null);
    // "위치 보기" opened this folder for the file it is waiting to select.
    setReveal((r) => (r && r.dir === pathKey(path) ? r : null));
    if (path === opened.current) {
      reload(true).then((r) => r === "missing" && climb(path));
      return;
    }
    opened.current = null;
    reload();
  }, [path, reload, climb]);

  // Live reloads of the folder shown, one at a time. Changes that come while one runs get one
  // more after it, so a slow listing (100k entries) lands instead of being superseded by the
  // next 500 ms window before it does.
  const liveList = useRef({ busy: false, again: false });
  const reloadLive = useCallback(async () => {
    const s = liveList.current;
    if (s.busy) {
      s.again = true;
      return;
    }
    s.busy = true;
    try {
      do {
        s.again = false;
        const here = pathRef.current;
        // Moved up: the new folder lists on its own, with every change so far.
        if ((await reloadRef.current(true)) === "missing" && (await climb(here))) break;
      } while (s.again);
    } finally {
      s.busy = false;
    }
  }, [climb]);

  const refreshTree = useCallback(
    (dirs: string[] | null = null, gone: string[] = []) => setTreeRefresh((t) => ({ n: t.n + 1, dirs, gone })),
    [],
  );

  // What a hidden tab owes once it shows again: a listing, folders of the tree, a search run.
  // Live re-runs of a search wait on their own (pumpSearch).
  const owed = useRef<{ list: boolean; tree: { dirs: Set<string> | null; gone: Set<string> } | null; search: boolean }>({
    list: false,
    tree: null,
    search: false,
  });

  // An AI 검색's hits in folders that changed (another program, Windows' own menu, a move, an
  // uninstaller): looked for on disk, a batch at a time, and left out once gone. The judge is not
  // asked again.
  const jevGone = useRef({ busy: false, paths: new Set<string>() });
  const checkJevGone = useCallback(async () => {
    const s = jevGone.current;
    if (s.busy) return;
    s.busy = true;
    try {
      while (s.paths.size) {
        const paths = [...s.paths];
        s.paths.clear();
        // Not readable is not gone.
        const kinds = await Promise.all(paths.map((p) => places.pathKind(p).catch(() => "file" as const)));
        const gone = paths.filter((_, i) => kinds[i] === "gone");
        if (gone.length) setAiRes((r) => (r?.jev ? jevWithout(r, r.jev, gone) : r));
      }
    } finally {
      s.busy = false;
    }
  }, []);

  const apply = useCallback(
    (c: Changes) => {
      const shown = activeRef.current;
      const later = owed.current;
      const dirs = [...c.dirs];
      const removed = [...c.removed];
      const jev = jevShown.current;
      if (jev) {
        // Live changes name the folders removed, not the files: a hit in a folder that changed,
        // or in one removed (a folder renamed only in case is still there), is looked for.
        // Without a removed list, any changed folder above it may be where it went.
        const wanted = jevGone.current.paths;
        for (const h of jev.found.items) {
          const k = pathKey(h.entry.path);
          const up = parentOf(h.entry.path);
          if (
            c.full.has(driveKey(k)) ||
            (up !== null && c.dirs.has(pathKey(up))) ||
            removed.some((r) => isSameOrInside(k, r)) ||
            (c.unsure && dirs.some((d) => isSameOrInside(k, d)))
          )
            wanted.add(h.entry.path);
        }
        if (wanted.size) void checkJevGone();
      }
      const here = pathRef.current;
      if (here) {
        const key = pathKey(here);
        const all = c.full.has(driveKey(here));
        // A change anywhere below the folder shown changes the size of one of its rows.
        const inside = all ? [] : dirs.filter((d) => isSameOrInside(d, key));
        const goneHere = removed.filter((r) => isSameOrInside(r, key));
        // The folder shown, or one above it, went away: the reload finds it gone and moves up.
        // Without a removed list, a changed folder above it (not the drive root, which every
        // change touches) may be that, and the reload tells.
        const gone =
          removed.some((r) => isSameOrInside(key, r)) ||
          (c.unsure && dirs.some((d) => d !== key && d.length > 2 && isSameOrInside(key, d)));
        if (all || inside.length || gone) {
          if (shown) reloadLive();
          else later.list = true;
        }
        if (all || inside.length || goneHere.length) {
          if (shown) refreshTree(all ? null : inside, goneHere);
          else {
            const t = (later.tree ??= { dirs: new Set(), gone: new Set() });
            if (all) t.dirs = null;
            else if (t.dirs) for (const d of inside) t.dirs.add(d);
            for (const g of goneHere) t.gone.add(g);
          }
        }
      }
      // An AI 검색 is not run again for changes; the name search not while the box is in AI mode
      // (it runs again as the box turns back to 이름).
      if (queryRef.current.trim() && modeRef.current === "name") {
        // A search of one folder only cares about changes in it, or to it.
        const scope = underRef.current === null ? null : pathKey(underRef.current);
        const touched =
          scope === null ||
          c.full.has(driveKey(scope)) ||
          dirs.some((d) => isSameOrInside(d, scope)) ||
          removed.some((r) => isSameOrInside(scope, r));
        if (touched) {
          searchRuns.current.wanted = true;
          pumpRef.current();
        }
      }
    },
    [reloadLive, refreshTree, checkJevGone],
  );

  const resync = useCallback(() => {
    if (activeRef.current) {
      setSearchNonce((n) => n + 1);
      refreshTree();
      reloadRef.current();
    } else owed.current = { list: true, tree: { dirs: null, gone: new Set() }, search: true };
  }, [refreshTree]);

  // Shown again: catch up on what changed meanwhile.
  useEffect(() => {
    if (!active) return;
    const later = owed.current;
    owed.current = { list: false, tree: null, search: false };
    if (later.list) reloadLive();
    if (later.tree) refreshTree(later.tree.dirs && [...later.tree.dirs], [...later.tree.gone]);
    if (later.search) setSearchNonce((n) => n + 1);
    pumpRef.current();
  }, [active, reloadLive, refreshTree]);

  // Hidden: menus close, and keys typed next belong to the tab shown, not to a box left focused
  // here (a name being edited is saved, as a click elsewhere would save it).
  useEffect(() => {
    if (active) return;
    setMenu(null);
    const a = document.activeElement;
    if (a instanceof HTMLElement && root.current?.contains(a)) a.blur();
  }, [active]);

  useEffect(() => {
    const runs = searchRuns.current;
    // A live re-run held back while the window was hidden.
    const onShow = () => pumpRef.current();
    document.addEventListener("visibilitychange", onShow);
    return () => {
      document.removeEventListener("visibilitychange", onShow);
      if (runs.timer !== null) {
        clearTimeout(runs.timer);
        runs.timer = null;
      }
    };
  }, []);

  // The sidebar's "프로그램 정리". A dialog already open (the trash question too) is answered first.
  const openPrograms = useCallback(() => {
    if (busyRef.current) return;
    setMenu(null);
    setDialog({ kind: "programs" });
  }, []);

  useImperativeHandle(
    ref,
    () => ({ navigate, apply, resync, busy: () => busyRef.current, dropTarget: () => dropRef.current, openPrograms }),
    [navigate, apply, resync, openPrograms],
  );

  useEffect(() => onState(id, path, view), [onState, id, path, view]);

  // The results the box's mode shows: the name search's, or AI 검색's (kept while 이름 shows).
  const aiMode = mode === "ai";
  const shownRes = aiMode ? aiRes : results;
  const searching = shownRes !== null;
  // The listing still on screen may be the previous folder's while the new one loads
  // (a slow network drive can take a minute): show nothing of it rather than act on it.
  const fresh = listing !== null && sameListing(listing.path, path) ? listing : null;
  // A folder this account may not open, read through the admin session: read-only, and shown as
  // a list or icons (the tree and the map read folders the normal way).
  const adminEntries = !searching && fresh === null && admin !== null && sameListing(admin.path, path) ? admin.entries : null;
  const readOnly = adminEntries !== null;
  const treeShown = !searching && !readOnly && view === "tree";
  const gridShown = !searching && view === "grid";
  const mapShown = !searching && !readOnly && view === "map";

  // The list's "변화" column: this folder compared with the scan before, one level deep.
  const diffWanted = diffOn && view === "list" && !searching && !readOnly && !!path;
  const compared = diffWanted && diff !== null && sameListing(diff.path, path) ? diff : null;
  const changes = useMemo(() => {
    const root = compared?.root;
    if (!root) return null;
    // By the exact path first: a case-sensitive folder can hold "Build" and "build".
    const exact = new Map<string, DiffNode>();
    const folded = new Map<string, DiffNode>();
    for (const c of root.children) {
      exact.set(c.path, c);
      if (!folded.has(pathKey(c.path))) folded.set(pathKey(c.path), c);
    }
    // A full list may leave out changes smaller than its smallest one.
    const smallest =
      root.children.length >= DIFF_LIMIT ? Math.min(...root.children.map((c) => Math.abs(c.size - c.prev_size))) : 0;
    return {
      root,
      find: (e: Entry) => exact.get(e.path) ?? folded.get(pathKey(e.path)),
      smallest,
      removed: root.children.filter((c) => c.status === "removed"),
    };
  }, [compared]);
  // An AI 검색's answer on screen (v0.9), and who judged it.
  const jevRes = shownRes?.jev ?? null;
  const jevEngine = jevRes ? engineOf(jevRes.found) : "jev";
  // Sorting by the change needs the column, and by 일치도 an AI 검색's results; without them the
  // list goes by size.
  const sortKey = (sort.key === "change" && !diffWanted) || (sort.key === "score" && !jevRes) ? "size" : sort.key;
  const changeSort = useMemo(
    () =>
      sortKey !== "change"
        ? null
        : changes
          ? (a: Entry, b: Entry) => {
              const da = changes.find(a);
              const db = changes.find(b);
              return (da ? a.size - da.prev_size : 0) - (db ? b.size - db.prev_size : 0);
            }
          : // While the comparison loads.
            SORTERS.size,
    [sortKey, changes],
  );
  const scoreSort = useMemo(
    () =>
      jevRes
        ? (a: Entry, b: Entry) => (jevRes.hits.get(a.path)?.score ?? 0) - (jevRes.hits.get(b.path)?.score ?? 0)
        : SORTERS.size,
    [jevRes],
  );
  const rows = useMemo(() => {
    let list = shownRes ? shownRes.items : (fresh?.entries ?? adminEntries ?? []);
    // An AI 검색 shows the hits scored under JEV_SHOW only when asked to ("40% 미만도 보기").
    if (jevRes && !jevLow) list = list.filter((e) => (jevRes.hits.get(e.path)?.score ?? 0) >= JEV_SHOW);
    const dir = sort.desc ? -1 : 1;
    const by = sortKey === "change" ? changeSort! : sortKey === "score" ? scoreSort : SORTERS[sortKey];
    return [...list].sort((a, b) => {
      // Folders first in name/type order, like Explorer.
      if ((sortKey === "name" || sortKey === "type") && a.is_dir !== b.is_dir) return a.is_dir ? -1 : 1;
      return by(a, b) * dir || cmp(a.name, b.name);
    });
  }, [shownRes, fresh, adminEntries, sort.desc, sortKey, changeSort, jevRes, jevLow, scoreSort]);
  // Hits an AI 검색 shows at first, and those it keeps back.
  const jevMatched = jevRes ? jevRes.found.items.filter((h) => h.score >= JEV_SHOW).length : 0;
  const jevHidden = jevRes ? jevRes.found.items.length - jevMatched : 0;

  // A folder compared for the first time is asked at once; again after live changes to its
  // listing, once they settle. The newest answer for the folder shown wins.
  const diffReq = useRef(0);
  const diffAsked = useRef<string | null>(null);
  useEffect(() => {
    if (!diffWanted) {
      diffAsked.current = null;
      return;
    }
    const here = path;
    const run = () => {
      const req = ++diffReq.current;
      diffAsked.current = here;
      Promise.allSettled([v05.scanDiff(here, 1), v05.diffInfo(driveKey(here))]).then(([d, info]) => {
        if (req !== diffReq.current) return;
        setDiff({
          path: here,
          root: d.status === "fulfilled" ? d.value : null,
          info: info.status === "fulfilled" ? info.value : null,
          error: d.status === "rejected" ? String(d.reason) : null,
        });
      });
    };
    const t = setTimeout(run, diffAsked.current === here ? 1000 : 0);
    return () => clearTimeout(t);
  }, [diffWanted, path, fresh]);

  const toggleDiff = () => {
    const on = !diffOn;
    setDiffOn(on);
    if (!on && sort.key === "change") setSort({ key: "size", desc: true });
  };

  // A row's "변화" cell. The listing's size (live) against the scan before.
  const changeCell = (e: Entry): ChangeCell | undefined => {
    if (!changes) return undefined;
    const d = changes.find(e);
    if (!d) {
      return changes.smallest > 0
        ? {
            text: "작음",
            tone: "unknown",
            title: `${formatSize(changes.smallest)}보다 적게 바뀌었거나 그대로예요 (변화가 큰 ${DIFF_LIMIT}개만 비교해요)`,
          }
        : { text: "—", tone: "same", title: "지난 스캔과 같아요" };
    }
    if (d.status === "new") return { text: `새 ${signedSize(e.size)}`, tone: "new", title: "지난 스캔 뒤에 생겼어요" };
    const delta = e.size - d.prev_size;
    if (delta === 0) {
      const files = d.files !== d.prev_files ? ` (파일 수 ${d.prev_files.toLocaleString()} → ${d.files.toLocaleString()})` : "";
      return { text: "—", tone: "same", title: `크기가 지난 스캔과 같아요${files}` };
    }
    const pct = percentText(delta, d.prev_size);
    return {
      text: signedSize(delta),
      tone: delta > 0 ? "grown" : "shrunk",
      title: `지난 스캔 ${formatSize(d.prev_size)} → 지금 ${formatSize(e.size)}${pct ? ` (${pct})` : ""}`,
    };
  };

  // The list's "위험도" column: the judge's answer (jev, or a local model) for the rows in sight of
  // a folder's listing (not search results, nor a folder read with administrator rights), asked
  // for in the background a moment after they come into sight. The list never waits for it, and a
  // failed request shows nothing.
  const judgeOk = judgeReady(p.ai);
  const judge: JudgeEngine = p.ai ? judgeOf(p.ai.settings) : "jev";
  // The local model's Ollama address when it is another computer's (what is judged goes there);
  // null for jev and for this PC. What the app says about "이 PC" depends on it.
  const judgeAway = p.ai ? judgeRemote(p.ai.settings) : null;
  // Answers are kept by the judge that gave them: another engine, model or address asks again.
  const riskJudge = p.ai && judgeOk ? judgeKey(p.ai.settings) : "";
  const riskShown = riskOn && judgeOk && view === "list" && !searching && !readOnly;
  const riskWanted = riskShown && active && fresh !== null;
  const riskAsking = useRef(new Set<string>());
  const riskBusy = useRef(false);
  const riskPausedUntil = useRef(0);
  // v0.10: why the last request failed, said once (a local model's "Ollama가 꺼져 있어요").
  const riskError = useRef<string | null>(null);
  // Where the column's requests were said to go when it was turned on (judgeTarget; null: this PC).
  const riskAgreed = useRef<string | null>(null);
  const onRange = useCallback(
    (first: number, last: number) => setRange((r) => (r[0] === first && r[1] === last ? r : [first, last])),
    [],
  );
  useEffect(() => {
    if (!riskWanted) return;
    // After a failed request, not before its pause is over.
    const pause = riskPausedUntil.current - Date.now();
    const t = setTimeout(() => {
      if (riskBusy.current) return;
      if (Date.now() < riskPausedUntil.current) {
        setRiskTick((n) => n + 1);
        return;
      }
      const keyOf = (at: string) => `${riskJudge}\n${pathKey(at)}`;
      const want: string[] = [];
      for (const e of rows.slice(range[0], range[1])) {
        const k = keyOf(e.path);
        if (riskKnown(k) || riskAsking.current.has(k)) continue;
        want.push(e.path);
        if (want.length >= RISK_BATCH[judge]) break;
      }
      if (!want.length) return;
      const keys = want.map(keyOf);
      riskBusy.current = true;
      // The backend asks the judge the settings name when the request goes, and they may have
      // changed since this tab read them (AI 설정 saves with its window open, another window): read
      // again first. Another judge (engine, model, address), or a destination not said when the
      // column was turned on: nothing goes, App reads them again, and the column follows (asks the
      // new judge, or turns off and says why).
      const agreed = riskAgreed.current;
      const send = async () => {
        let s: AiConfig;
        try {
          s = await ai.getSettings();
        } catch {
          // Not known now: asked again after the pause, as after a failed request.
          riskPausedUntil.current = Date.now() + RISK_PAUSE_MS;
          return;
        }
        const target = judgeTarget(s);
        if (judgeKey(s) !== riskJudge || (s.confirm_before_send && target !== null && target !== agreed)) {
          onAiChanged();
          return "moved";
        }
        for (const k of keys) riskAsking.current.add(k);
        setRiskTick((n) => n + 1);
        try {
          const list = await ai.jevRisk(want);
          riskError.current = null;
          const got = new Map(list.map((r) => [keyOf(r.path), r]));
          for (const k of keys) rememberRisk(k, got.get(k) ?? null);
        } catch (err) {
          for (const k of keys) rememberRisk(k, null);
          riskPausedUntil.current = Date.now() + RISK_PAUSE_MS;
          // The list shows nothing for it; the reason is said once, not on every try again.
          const why = String(err);
          if (why !== riskError.current) {
            riskError.current = why;
            toast(`위험도를 받지 못했어요: ${why}`, "error");
          }
        } finally {
          for (const k of keys) riskAsking.current.delete(k);
        }
      };
      void send().then((r) => {
        riskBusy.current = false;
        // Shown, and the next rows in sight (scrolled meanwhile) asked about. Moved to another
        // judge: once App has read the settings again (riskJudge, or the column turned off).
        if (r !== "moved") setRiskTick((n) => n + 1);
      });
    }, Math.max(400, pause + 50));
    return () => clearTimeout(t);
  }, [riskWanted, rows, range, riskTick, riskJudge, judge, toast, onAiChanged]);
  const riskCell = (e: Entry): RiskCell => {
    const k = `${riskJudge}\n${pathKey(e.path)}`;
    if (riskAsking.current.has(k)) return "asking";
    return riskCache.get(k)?.risk ?? null;
  };
  // The judge turned off in the settings, or not ready any more (its key removed, no model): the
  // column goes, and comes back off; the box turns back to 이름 (AI 검색's results are kept for
  // when a judge is ready again), the run it waited for is not waited for, and the question about
  // turning either on, if open, closes ("켜기" there would turn on what cannot run).
  useEffect(() => {
    if (judgeOk) return;
    setRiskOn(false);
    jevReq.current++;
    aiWait.current = null;
    leaveAiRun(id);
    setJevBusy(false);
    setJevQueued(false);
    setAiStopping(false);
    setAiProgress(null);
    if (modeRef.current === "ai") setMode("name");
    setDialog((d) => (d?.kind === "riskAsk" || d?.kind === "aiFindAsk" ? null : d));
  }, [judgeOk, id]);
  // Another judge chosen while the column is on (jev, a local model, another address): when what it
  // sends would now go where it was not said to, the column comes back off (turned on again, it says).
  useEffect(() => {
    if (!riskOn || !p.ai) return;
    const target = judgeTarget(p.ai.settings);
    if (!p.ai.settings.confirm_before_send || target === null || target === riskAgreed.current) return;
    setRiskOn(false);
    // Every tab with the column on turns it off; only the one shown says so (one toast, not one a tab).
    if (activeRef.current) toast("판정 엔진이 바뀌어서 위험도를 껐어요. 다시 켜면 어디로 보내는지 먼저 물어요");
  }, [riskOn, p.ai, toast]);

  const columnsShown = useMemo(() => {
    if (searching) return jevRes ? AI_COLUMNS[jevEngine] : SEARCH_COLUMNS;
    const base = diffWanted ? FOLDER_DIFF_COLUMNS : FOLDER_COLUMNS;
    return riskShown ? [...base, judge === "ollama" ? RISK_COLUMN_LOCAL : RISK_COLUMN] : base;
  }, [searching, jevRes, jevEngine, diffWanted, riskShown, judge]);

  // A row's 일치도, and the program or game the judge found in that folder.
  const scoreCell = (e: Entry): ScoreCell | undefined => {
    const h = jevRes?.hits.get(e.path);
    return h && { score: h.score, label: h.label, by: JUDGE_NAME[jevEngine] };
  };
  const aliasOf = (e: Entry) => jevRes?.hits.get(e.path)?.label;
  // The size bar of an AI 검색's rows (by score, not size): the biggest of them.
  const jevBar = useMemo(() => (jevRes ? rows.reduce((m, e) => Math.max(m, e.size), 0) : 0), [jevRes, rows]);

  /**
   * The AI settings and keys as saved now, not as this window last read them (another window may
   * have turned "보내기 전에 미리 보기" on meanwhile); App's copy is read again too. Null when not known.
   */
  const freshAi = async (): Promise<AiState | null> => {
    try {
      const [settings, keys] = await Promise.all([ai.getSettings(), ai.keyStatus()]);
      return { settings, keys };
    } catch {
      return null;
    } finally {
      p.onAiChanged();
    }
  };

  const toggleRisk = async () => {
    if (riskOn) {
      setRiskOn(false);
      return;
    }
    riskError.current = null;
    const now = await freshAi();
    if (!judgeReady(now)) {
      // Not ready (the menu item shows it off): what is missing, and the settings where it is set.
      toast(judgeBlocker(now));
      if (activeRef.current && !busyRef.current) openDialog({ kind: "aiSettings" });
      return;
    }
    if (!activeRef.current || busyRef.current) return;
    // What is sent, and to whom, is said first when the settings ask for it and it leaves this PC
    // (a model on this PC sends nothing out).
    const target = judgeTarget(now.settings);
    if (now.settings.confirm_before_send && target !== null) {
      openDialog({ kind: "riskAsk", engine: judgeOf(now.settings), url: (now.settings.ollama_url ?? "").trim(), target });
    } else {
      riskAgreed.current = target;
      setRiskOn(true);
    }
  };

  // ---- v0.9: AI 검색, the box's "AI" mode (v0.9 "뜻으로 찾기"; v0.10 jev or a local model) ----

  // New AI results came while the box showed 이름: they are ordered by 일치도 once AI shows them.
  const aiSortDue = useRef(false);
  /** New AI results on screen: ordered by 일치도, the selection starts over. */
  const primeAiView = (key: string) => {
    shownSearch.current = key;
    setSelected(new Set());
    setFocusPath(null);
    anchor.current = null;
    setSort({ key: "score", desc: true });
    aiSortDue.current = false;
  };

  // switchMode asks for the box's focus (below).
  const boxFocus = useRef(false);
  /**
   * The box turns to `to`: its text and results are that mode's own (이름's search runs again as it
   * comes back, as live changes may have come meanwhile). The selection starts over.
   */
  const switchMode = (to: "name" | "ai") => {
    setMode(to);
    setSelected(new Set());
    setFocusPath(null);
    anchor.current = null;
    // AI's as of now (an answer may have come while the settings were read).
    const kept = aiResRef.current;
    shownSearch.current = (to === "ai" ? kept?.key : results?.key) ?? "";
    if (to === "ai" && aiSortDue.current && kept) primeAiView(kept.key);
    boxFocus.current = true;
  };
  // The box takes the keyboard once the mode shows: after a dialog that closed meanwhile gave the
  // focus back to where it was.
  useEffect(() => {
    if (!boxFocus.current) return;
    boxFocus.current = false;
    searchBox.current?.focus();
  });

  /** The question to ask before AI 검색 sends to `s`'s judge (jev, an Ollama elsewhere), or null when none is due. */
  const findAsk = (s: AiState, run: boolean): TabDialog | null => {
    const target = judgeTarget(s.settings);
    if (!s.settings.confirm_before_send || target === null || aiAgreed.current === target) return null;
    // "바뀌었어요" only when another destination was said before (not when none was asked about:
    // "미리 보기" was off, or the model was on this PC).
    const changed = run && aiAgreed.current !== null;
    return { kind: "aiFindAsk", engine: judgeOf(s.settings), url: (s.settings.ollama_url ?? "").trim(), target, run, changed };
  };

  /**
   * The box's [이름 | AI] (Ctrl+F, Ctrl+Shift+F). AI with no judge ready says the two ways to get
   * one; the first time in this tab it says what goes where, when the settings ask for it (not for
   * a model on this PC: nothing leaves it).
   */
  const selectMode = async (to: "name" | "ai") => {
    if (to === modeRef.current) {
      searchBox.current?.focus();
      return;
    }
    if (to === "name") {
      switchMode("name");
      return;
    }
    const now = await freshAi();
    if (!activeRef.current || busyRef.current) return;
    if (!judgeReady(now)) {
      openDialog({ kind: "aiHow", why: judgeBlocker(now) });
      return;
    }
    const ask = findAsk(now, false);
    if (ask) openDialog(ask);
    else switchMode("ai");
  };

  /** This tab alone waits for the run out (the one it asked for): stopping it stops the run. */
  const aiAlone = () => {
    const run = aiRun.current;
    return !!run && run.key === aiWait.current && run.waiters.has(id) && run.waiters.size === 1;
  };
  // Another tab came to wait for the same run, or left it: the stop button says again what it does.
  const [, setAiRunSeen] = useState(0);
  useEffect(() => {
    if (!jevBusy) return;
    const seen = () => setAiRunSeen((n) => n + 1);
    aiRun.watch.add(seen);
    return () => void aiRun.watch.delete(seen);
  }, [jevBusy]);

  /** Stops waiting for the run out (its answer is dropped); it is stopped once no tab waits for it. */
  const quitAiWait = () => {
    jevReq.current++;
    aiWait.current = null;
    leaveAiRun(id);
    setJevBusy(false);
    setJevQueued(false);
    setAiStopping(false);
    setAiProgress(null);
  };

  /**
   * The stop button, and Esc while asking: the run this tab waits for stops before its next request
   * (the one out finishes), and what was answered comes in, marked "멈췄어요". When another tab waits
   * for it too, or it was asked to stop already, or another run is ahead of it, this tab only stops
   * waiting.
   */
  const stopAi = () => {
    if (jevBusy && !jevQueued && !aiStopping && aiAlone()) {
      setAiStopping(true);
      ai.findCancel().catch((err) => toast(String(err), "error"));
      return;
    }
    quitAiWait();
  };

  // The run's requests as they are answered ("ai-find-progress": jev's batches, a local model's
  // candidates one by one), for the run this tab waits for.
  useEffect(() => {
    if (!jevBusy || jevQueued) return;
    let dead = false;
    let off: (() => void) | undefined;
    aiEvents
      .findProgress((pr) => {
        if (aiRun.current && aiRun.current.key === aiWait.current) setAiProgress(pr);
      })
      .then(
        (u) => (dead ? u() : (off = u)),
        () => {},
      );
    return () => {
      dead = true;
      off?.();
    };
  }, [jevBusy, jevQueued]);

  /**
   * Enter in AI mode: asks the judge about what the box holds, within "이 폴더만" when it is on.
   * The best hits come first. One run at a time for the window (aiRun): the same search out is
   * waited for (sent once, billed once), another one waits for it to end. A new Enter stops waiting
   * for this tab's run before; emptying the box (Esc) too.
   */
  const runAi = async () => {
    const q = aiQuery.trim();
    if (!q) return;
    const s = p.ai;
    if (!judgeReady(s)) {
      openDialog({ kind: "aiHow", why: judgeBlocker(s) });
      return;
    }
    // The judge's destination changed since it was said (the settings, another window): said again.
    const ask = findAsk(s, true);
    if (ask) {
      openDialog(ask);
      return;
    }
    let engine = judgeOf(s.settings);
    const scope = under;
    const keyOf = (c: AiConfig) => `ai:${judgeKey(c)}:${q}\n${scope ?? ""}`;
    let key = keyOf(s.settings);
    if (aiWait.current !== null && aiWait.current !== key) leaveAiRun(id);
    const req = ++jevReq.current;
    aiWait.current = key;
    setJevBusy(true);
    setAiStopping(false);
    setAiProgress(null);
    setAiEngine(engine);
    try {
      let run = aiRun.current;
      for (;;) {
        // Another search still out: this one goes once it ended, never alongside it.
        while (aiRun.current && aiRun.current.key !== key) {
          setJevQueued(true);
          await aiRun.current.done.catch(() => {});
          if (req !== jevReq.current) return;
        }
        setJevQueued(false);
        // The same search still out: its answer, not a second run.
        run = aiRun.current;
        if (run) break;
        // About to send. The backend asks the judge the settings name then, and they may have
        // changed since Enter (above all while it waited: AI 설정 saved, another window): read
        // again, and nothing goes to a destination not said. Another judge said or not asked
        // about (another model, a model on this PC): this search is asked of it.
        const now = await freshAi();
        if (req !== jevReq.current) return;
        if (!judgeReady(now)) {
          toast(judgeBlocker(now));
          return;
        }
        const again = findAsk(now, true);
        if (again) {
          // The question is this tab's: asked when it is shown with no other dialog, else said.
          if (activeRef.current && !busyRef.current) openDialog(again);
          else toast("판정 엔진이나 보낼 곳이 바뀌어서 AI 검색을 보내지 않았어요. 다시 Enter를 눌러 주세요");
          return;
        }
        engine = judgeOf(now.settings);
        key = keyOf(now.settings);
        aiWait.current = key;
        setAiEngine(engine);
        // Another tab may have sent one while the settings were read: looked at again.
        if (aiRun.current) continue;
        const mine = { key, done: ai.jevFind(q, scope), waiters: new Set<number>() };
        aiRun.current = run = mine;
        void mine.done
          .finally(() => {
            if (aiRun.current === mine) aiRun.current = null;
          })
          .catch(() => {});
        break;
      }
      run.waiters.add(id);
      aiRunChanged();
      const found = await run.done;
      if (req !== jevReq.current) return;
      const isNew = key !== aiResRef.current?.key;
      setAiRes(jevResults(key, q, scope, found, found.items));
      if (isNew) {
        setJevLow(false);
        if (modeRef.current === "ai") primeAiView(key);
        else aiSortDue.current = true;
      }
    } catch (err) {
      if (req === jevReq.current) toast(String(err), "error");
    } finally {
      if (req === jevReq.current) {
        aiWait.current = null;
        setJevBusy(false);
        setJevQueued(false);
        setAiStopping(false);
        setAiProgress(null);
      }
    }
  };

  const focus = useMemo(
    () => (focusPath === null ? -1 : rows.findIndex((r) => r.path === focusPath)),
    [rows, focusPath],
  );
  const selectedEntries = useMemo(() => rows.filter((r) => selected.has(r.path)), [rows, selected]);

  // Files dropped on this tab, not on a folder in it, go into the folder shown (App's drop).
  // Search results are no folder, and a folder read with administrator rights is read-only.
  const dialogOpen = confirmTrash !== null || dialog !== null;
  useLayoutEffect(() => {
    dropRef.current =
      dialogOpen || !path ? null : searching ? { refuse: DROP_ON_SEARCH } : readOnly ? { refuse: READ_ONLY } : { path };
  });

  // Rows, tiles and tree rows dragged out of the window (to Explorer, another program, or a
  // folder here): the selection when the row is in it, else that row alone. Read through a ref:
  // the press that starts the drag selected the row a moment ago.
  const dragFrom = useRef({ rows, selected, readOnly });
  useLayoutEffect(() => {
    dragFrom.current = { rows, selected, readOnly };
  });
  const dragItems = (e: Entry, tree: boolean) => {
    const s = dragFrom.current;
    // A folder read with administrator rights: this account could not read what it would drag.
    if (s.readOnly) return;
    const items = !tree && s.selected.has(e.path) ? s.rows.filter((r) => s.selected.has(r.path)) : [e];
    if (!items.length) return;
    const paths = items.map((r) => r.path);
    // Paths Windows cannot take from a drag (see dragRefusal): said here, never handed over.
    const refused = dragRefusal(paths, p.drives);
    if (refused) {
      toast(refused);
      return;
    }
    dragOut(paths, e).catch((err) => toast(`끌어 놓기를 시작하지 못했어요: ${err}`, "error"));
  };

  // The folder just made is in the listing: select it and open its name for editing.
  useEffect(() => {
    if (made === null) return;
    const e = rows.find((r) => pathKey(r.path) === made);
    if (!e) return;
    setMade(null);
    setSelected(new Set([e.path]));
    setFocusPath(e.path);
    anchor.current = e.path;
    setRenaming(e.path);
  }, [rows, made]);

  // The file "위치 보기" asked for is in the listing: select it and bring it into sight.
  const listed = !searching && (fresh !== null || readOnly);
  useEffect(() => {
    if (reveal === null || !listed || pathKey(path) !== reveal.dir) return;
    setReveal(null);
    const e = rows.find((r) => pathKey(r.path) === reveal.file);
    if (!e) {
      toast("그 파일이 이 폴더에 없어요. 지워졌거나 옮겨졌을 수 있어요");
      return;
    }
    setSelected(new Set([e.path]));
    setFocusPath(e.path);
    anchor.current = e.path;
  }, [rows, reveal, listed, path, toast]);

  const select = (i: number, mode: "single" | "toggle" | "range") => {
    setMenu(null);
    const e = rows[i];
    if (!e) return;
    const a = mode === "range" && anchor.current !== null ? rows.findIndex((r) => r.path === anchor.current) : -1;
    if (a >= 0) {
      const [lo, hi] = [Math.min(a, i), Math.max(a, i)];
      setSelected(new Set(rows.slice(lo, hi + 1).map((r) => r.path)));
    } else if (mode === "toggle") {
      setSelected((s) => {
        const n = new Set(s);
        if (n.has(e.path)) n.delete(e.path);
        else n.add(e.path);
        return n;
      });
      anchor.current = e.path;
    } else {
      setSelected(new Set([e.path]));
      anchor.current = e.path;
    }
    setFocusPath(e.path);
  };

  const activate = (e: Entry) => {
    if (e.is_dir) navigate(e.path);
    else api.open(e.path).catch((err) => toast(String(err), "error"));
  };

  const goUp = () => {
    const up = parentOf(path);
    if (up) navigate(up);
  };

  // F5 and the refresh button: the open folders in the tree list again too, and the map draws again.
  const refreshView = () => {
    reload();
    refreshTree();
    if (mapShown) setMapNonce((n) => n + 1);
  };

  // "위치 보기" in the duplicate finder: the file's folder opens here with the file selected, in a
  // view that shows the selection.
  const revealFile = (file: string) => {
    const dir = parentOf(file);
    setDialog(null);
    if (!dir) return;
    if (view === "tree" || view === "map") setView("list");
    setReveal({ dir: pathKey(dir), file: pathKey(file) });
    navigate(dir);
  };

  const openFile = (file: string) => api.open(file).catch((err) => toast(String(err), "error"));

  const changeView = (v: View) => {
    setView(v);
    setMenu(null);
  };

  const finishRename = async (e: Entry, name: string | null) => {
    setRenaming(null);
    if (name === null || name.trim() === "" || name === e.name) return;
    try {
      const to = await api.rename(e.path, name);
      // Keep the renamed item selected and focused under its new path.
      setSelected((s) => {
        if (!s.has(e.path)) return s;
        const n = new Set(s);
        n.delete(e.path);
        n.add(to);
        return n;
      });
      setFocusPath((f) => (f === e.path ? to : f));
      if (anchor.current === e.path) anchor.current = to;
      setAiRes((r) => (r?.jev ? jevRenamed(r, r.jev, e.path, to) : r));
      onChanged(id, changesOf([parentOf(e.path)], [e.path]));
      setSearchNonce((n) => n + 1);
      refreshTree();
      await reloadRef.current();
    } catch (err) {
      toast(String(err), "error");
    }
  };

  const doTrash = async (items: Entry[]) => {
    setConfirmTrash(null);
    // One trash at a time: a second Delete while a long one runs would send the same paths again.
    if (trashLock.busy) return;
    trashLock.busy = true;
    try {
      // Where the keyboard focus goes once the items are gone: the next row that stays,
      // else the one before them (so ArrowDown carries on from here, not from the top).
      const gone = new Set(items.map((i) => i.path));
      const idx = rows.map((r, i) => (gone.has(r.path) ? i : -1)).filter((i) => i >= 0);
      const next = idx.length
        ? (rows.slice(idx[idx.length - 1] + 1).find((r) => !gone.has(r.path)) ??
          rows.slice(0, idx[0]).reverse().find((r) => !gone.has(r.path)))
        : undefined;
      const r = await api.trash(items.map((i) => i.path));
      if (r.moved.length) {
        toast(`휴지통으로 ${r.moved.length.toLocaleString()}개 옮겼어요`);
        const moved = new Set(r.moved);
        setSelected((s) => new Set([...s].filter((p) => !moved.has(p))));
        setAiRes((res) => (res?.jev ? jevWithout(res, res.jev, r.moved) : res));
        // Read focus as it is now: the user may have moved on while the trash ran.
        if (next) {
          setFocusPath((f) => (f !== null && moved.has(f) ? next.path : f));
          if (anchor.current !== null && moved.has(anchor.current)) anchor.current = next.path;
        }
        onChanged(id, changesOf(r.moved.map(parentOf), r.moved));
      }
      // One toast per reason, not one per file (hundreds on a USB stick).
      const byReason = new Map<string, string[]>();
      for (const [p, why] of r.failed) {
        const list = byReason.get(why);
        if (list) list.push(p);
        else byReason.set(why, [p]);
      }
      for (const [why, ps] of byReason) {
        const name = ps[0].slice(ps[0].lastIndexOf("\\") + 1);
        toast(ps.length === 1 ? `${name}: ${why}` : `${name} 외 ${ps.length - 1}개: ${why}`, "error");
      }
    } catch (err) {
      toast(String(err), "error");
    } finally {
      trashLock.busy = false;
    }
    setSearchNonce((n) => n + 1);
    refreshTree();
    await reloadRef.current();
    await p.refreshDrives().catch(() => {});
  };

  const copyPaths = (items: Entry[]) => {
    navigator.clipboard.writeText(items.map((i) => i.path).join("\r\n"));
    toast("경로를 복사했어요");
  };

  // Copy and cut put the files on the Windows clipboard, so Explorer (and other programs) can
  // paste them too.
  const toClipboard = (items: Entry[], cut: boolean) => {
    if (!items.length) return;
    // A copy job reads with this account's rights, which this folder does not let in.
    if (readOnly) {
      toast(READ_ONLY);
      return;
    }
    const what = items.length === 1 ? items[0].name : `${items.length.toLocaleString()}개 항목`;
    ops.clipboardWrite(
      items.map((i) => i.path),
      cut,
    ).then(
      () => {
        // The command bar's "붙여넣기" may have known an empty clipboard.
        clipAsk.current++;
        setCanPaste(true);
        toast(cut ? `잘라냈어요: ${what}. 옮길 폴더에서 붙여넣으세요 (Ctrl+V)` : `복사했어요: ${what}`);
      },
      (err) => toast(String(err), "error"),
    );
  };

  const paste = (dest: string) => {
    // Search results are not a folder to paste into.
    if (searching && dest === path) {
      toast("검색 결과에는 붙여넣을 수 없어요. 검색을 닫고 붙여넣으세요");
      return;
    }
    if (readOnly) {
      toast(READ_ONLY);
      return;
    }
    p.onPaste(dest);
  };

  // Explorer's names: "새 폴더", then "새 폴더 (2)", "(3)"… for the first one this folder lacks.
  const folderName = (n: number) => (n === 1 ? "새 폴더" : `새 폴더 (${n})`);
  const makeFolder = async (here: string): Promise<string> => {
    const taken = new Set((fresh?.entries ?? []).map((e) => e.name.toLowerCase()));
    let n = 1;
    while (taken.has(folderName(n).toLowerCase())) n++;
    // The listing can be behind (another program made one meanwhile): the next number is tried.
    for (let tries = 0; ; tries++, n++) {
      try {
        return await ops.createFolder(here, folderName(n));
      } catch (err) {
        if (String(err) !== NAME_TAKEN || tries >= 20) throw err;
      }
    }
  };
  const newFolder = async () => {
    const here = path;
    if (!here || making.current) return;
    if (searching) {
      toast("새 폴더는 검색을 닫고 만들 수 있어요");
      return;
    }
    if (readOnly) {
      toast(READ_ONLY);
      return;
    }
    if (mapShown) {
      toast("새 폴더는 목록이나 아이콘 보기에서 만들 수 있어요");
      return;
    }
    making.current = true;
    try {
      const to = await makeFolder(here);
      onChanged(id, changesOf([here]));
      if (pathRef.current !== here) return;
      // Selected with its name open for editing once the listing has it.
      setMade(pathKey(to));
      await reloadRef.current();
    } catch (err) {
      toast(String(err), "error");
    } finally {
      making.current = false;
    }
  };

  // Whether the clipboard holds files: asked as a menu opens, and as the pointer (or the focus)
  // comes to the command bar, whose "붙여넣기" is off while it is known to hold none.
  const askClipboard = () => {
    const ask = ++clipAsk.current;
    ops.clipboardRead().then(
      (c) => ask === clipAsk.current && setCanPaste(c.paths.length > 0),
      () => {},
    );
  };
  const openMenu = (m: Menu) => {
    setMenu(m);
    setCanPaste(null);
    askClipboard();
  };

  // The Explorer context menu ("Windows 메뉴"), with what other programs add to it, at a point of
  // the page. One at a time: it holds the window's UI thread while open. Not for a folder read with
  // administrator rights: its commands would change what this app only reads there.
  const canShell = (items: Entry[]) => !readOnly && oneFolder(items);
  const shellOpen = useRef(false);
  const shellMenu = async (items: Entry[], clientX: number, clientY: number) => {
    setMenu(null);
    if (!items.length || shellOpen.current) return;
    if (readOnly) {
      toast(READ_ONLY);
      return;
    }
    if (!oneFolder(items)) {
      toast(MIXED_FOLDERS);
      return;
    }
    const here = path;
    shellOpen.current = true;
    try {
      const at = await screenPoint(clientX, clientY);
      if (!(await v05.shellMenu(items.map((e) => e.path), at.x, at.y))) return;
      // The command may have changed anything (renamed, removed, unpacked an archive here): the
      // folder lists again now, and once more in a moment for a command that goes on by itself.
      onChanged(id, changesOf([parentOf(items[0].path)]));
      if (pathRef.current !== here) return;
      reloadRef.current();
      refreshTree();
      if (queryRef.current.trim()) setSearchNonce((n) => n + 1);
      p.refreshDrives().catch(() => {});
      setTimeout(() => pathRef.current === here && reloadLive(), 1500);
    } catch (err) {
      toast(String(err), "error");
    } finally {
      shellOpen.current = false;
    }
  };

  // Shift+F10 and the menu key: the Windows menu of the selection (the tree's row), under the
  // focused row as Explorer puts it, or at the pane's corner when that row is out of sight.
  const keyShellMenu = () => {
    const items = treeShown ? (treeFocus ? [treeFocus.e] : []) : mapShown ? [] : selectedEntries;
    if (!items.length) return;
    const pane = root.current?.querySelector(".pane");
    if (!pane) return;
    const box = pane.getBoundingClientRect();
    // The tree stays laid out (hidden) under search results: the focused row drawn on screen.
    const row = Array.from(pane.querySelectorAll(".tr.focus, .tile.focus"), (el) => el.getBoundingClientRect())
      .find((r) => r.height > 0 && r.bottom > box.top && r.top < box.bottom);
    if (row) shellMenu(items, row.left + Math.min(48, row.width / 2), Math.min(row.bottom, box.bottom));
    else shellMenu(items, box.left + 24, box.top + 24);
  };

  const rowMenu = (e: React.MouseEvent, i: number) => {
    e.preventDefault();
    e.stopPropagation();
    const row = rows[i];
    if (!row) return;
    const items = selected.has(row.path) ? selectedEntries : [row];
    if (!selected.has(row.path)) select(i, "single");
    // Shift+right-click: the Windows menu at once, as Explorer's "더 많은 옵션 표시".
    if (e.shiftKey && canShell(items)) shellMenu(items, e.clientX, e.clientY);
    else openMenu({ x: e.clientX, y: e.clientY });
  };

  // Keyboard shortcuts, for the tab shown only.
  useEffect(() => {
    if (!active) return;
    const onKey = (ev: KeyboardEvent) => {
      const t = ev.target as HTMLElement;
      const typing = t instanceof HTMLInputElement ? t.type !== "checkbox" : t.isContentEditable;
      // Ctrl+F: the box in 이름 mode; Ctrl+Shift+F: in AI mode (v0.10).
      if (ev.ctrlKey && !ev.altKey && isLetter(ev, "f")) {
        ev.preventDefault();
        if (ev.repeat) return;
        void selectMode(ev.shiftKey ? "ai" : "name");
        return;
      }
      if (ev.altKey && ev.key === "ArrowLeft") {
        if (pos > 0) go(-1);
        return;
      }
      if (ev.altKey && ev.key === "ArrowRight") {
        if (pos < history.length - 1) go(1);
        return;
      }
      if (ev.key === "F5") {
        ev.preventDefault();
        refreshView();
        return;
      }
      if (typing || confirmTrash || dialog) return;
      // Kept from the page: the browser would open its own menu (Back, Refresh, Print…).
      if (isMenuKey(ev)) {
        ev.preventDefault();
        keyShellMenu();
        return;
      }
      const ctrl = ev.ctrlKey && !ev.altKey && !ev.metaKey;
      // Ctrl+Shift+1~4: 목록, 트리, 아이콘, 지도 (Explorer's Ctrl+Shift+number). By the key's
      // place, as Shift turns "1" into "!". Search results stay a list.
      const viewKey = ctrl && ev.shiftKey ? /^Digit([1-4])$/.exec(ev.code) : null;
      if (viewKey) {
        ev.preventDefault();
        if (!searching) changeView(VIEWS[Number(viewKey[1]) - 1]);
        return;
      }
      if (ctrl && ev.shiftKey && isLetter(ev, "n")) {
        ev.preventDefault();
        newFolder();
        return;
      }
      if (ctrl && !ev.shiftKey && isLetter(ev, "v")) {
        ev.preventDefault();
        paste(path);
        return;
      }
      if (ctrl && !ev.shiftKey && (isLetter(ev, "c") || isLetter(ev, "x"))) {
        // Text selected on the page (the preview's text, a path) is copied as text.
        if (window.getSelection()?.toString()) return;
        // The map shows no selection to copy.
        const items = treeShown ? (treeFocus ? [treeFocus.e] : []) : mapShown ? [] : selectedEntries;
        if (items.length) {
          ev.preventDefault();
          toClipboard(items, isLetter(ev, "x"));
        }
        return;
      }
      // The tree moves through and opens its rows itself; rename, trash and select-all act on
      // the list selection, which the tree (and the map) does not show.
      if (treeShown || mapShown) {
        if (ev.key === "Backspace") goUp();
        else if (ev.key === "Escape" || ev.key.startsWith("Arrow")) setMenu(null);
        return;
      }
      // Only Tab can leave a button focused (mouse clicks blur it), so Enter there is the
      // button's own; moving through the rows takes the keyboard back.
      const onButton = t instanceof HTMLButtonElement;
      // Icons: all four arrows, Up and Down by a row.
      const step = gridShown ? gridStep(ev.key, focus, rows.length, columns) : null;
      if (step !== null) {
        ev.preventDefault();
        if (onButton) t.blur();
        select(step, ev.shiftKey ? "range" : "single");
      } else if (ev.key === "ArrowDown" || ev.key === "ArrowUp") {
        ev.preventDefault();
        if (onButton) t.blur();
        const next = Math.max(0, Math.min(rows.length - 1, focus + (ev.key === "ArrowDown" ? 1 : -1)));
        select(next, ev.shiftKey ? "range" : "single");
      } else if (ev.key === "Enter") {
        if (onButton) return;
        const e = rows[focus];
        if (e) {
          ev.preventDefault();
          activate(e);
        }
      } else if (ev.key === "Backspace") {
        goUp();
      } else if ((ev.key === "F2" || ev.key === "Delete") && readOnly && selectedEntries.length) {
        toast(READ_ONLY);
      } else if (ev.key === "F2" && selectedEntries.length === 1) {
        setRenaming(selectedEntries[0].path);
      } else if (ev.key === "Delete" && selectedEntries.length && !trashLock.busy) {
        setConfirmTrash(selectedEntries);
      } else if (ev.ctrlKey && isLetter(ev, "a")) {
        ev.preventDefault();
        setSelected(new Set(rows.map((r) => r.path)));
      } else if (ev.key === "Escape") {
        setSelected(new Set());
        setMenu(null);
      }
    };
    // On Windows the browser opens its menu when the menu key comes up, not when it goes down.
    const onKeyUp = (ev: KeyboardEvent) => {
      const t = ev.target as HTMLElement;
      const typing = t instanceof HTMLInputElement ? t.type !== "checkbox" : t.isContentEditable;
      if (ev.key === "ContextMenu" && !typing && !confirmTrash && !dialog) ev.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    window.addEventListener("keyup", onKeyUp);
    return () => {
      window.removeEventListener("keydown", onKey);
      window.removeEventListener("keyup", onKeyUp);
    };
  });

  // A click anywhere else (the sidebar and tabs too), or leaving the window, closes the menu.
  // The menu's own clicks stop at it.
  useEffect(() => {
    if (!menu) return;
    const close = () => setMenu(null);
    window.addEventListener("click", close);
    window.addEventListener("blur", close);
    return () => {
      window.removeEventListener("click", close);
      window.removeEventListener("blur", close);
    };
  }, [menu]);

  // Keep the context menu inside the window: open it above or left of the cursor when it
  // would not fit below or right of it. Again when "제거 (제어판처럼)…" comes in (menuProg), a
  // moment after it opened.
  const menuItems = selectedEntries.length;
  useLayoutEffect(() => {
    const el = menuBox.current;
    if (!menu || !el) return;
    el.style.left = "0px";
    el.style.top = "0px";
    const { width, height } = el.getBoundingClientRect();
    const fit = (at: number, size: number, room: number) =>
      at + size <= room - 4 ? at : Math.max(4, at - size);
    el.style.left = `${fit(menu.x, width, window.innerWidth)}px`;
    el.style.top = `${fit(menu.y, height, window.innerHeight)}px`;
  }, [menu, menuItems, searching, menuProg]);

  // The divider between the list and the side pane: dragged, it sets the pane's width.
  const startResize = (ev: React.PointerEvent<HTMLDivElement>) => {
    if (ev.button !== 0) return;
    ev.preventDefault();
    const bar = ev.currentTarget;
    const room = bar.parentElement?.clientWidth ?? 0;
    const x0 = ev.clientX;
    const w0 = p.pane.width;
    const onWidth = p.onPaneWidth;
    const width = (x: number) => Math.round(Math.max(PREVIEW_MIN, Math.min(room - LIST_MIN, w0 + x0 - x)));
    bar.setPointerCapture(ev.pointerId);
    bar.classList.add("dragging");
    const move = (e: PointerEvent) => onWidth(width(e.clientX));
    const up = () => {
      bar.removeEventListener("pointermove", move);
      bar.removeEventListener("pointerup", up);
      bar.removeEventListener("pointercancel", up);
      bar.classList.remove("dragging");
    };
    bar.addEventListener("pointermove", move);
    bar.addEventListener("pointerup", up);
    bar.addEventListener("pointercancel", up);
  };

  const drive = p.drives.find((d) => path.toUpperCase().startsWith(d.letter));
  const current = fresh?.current;
  const total = current?.size ?? 0;
  const selSize = selectedEntries.reduce((s, e) => s + e.size, 0);
  const crumbs = path ? path.replace(/\\$/, "").split("\\") : [];
  const target = menu?.target ?? null;
  // A file deep in the tree can open the folder it is in; one right under the root already is.
  const targetUp = target && !target.is_dir ? parentOf(target.path) : null;
  const targetParent = targetUp && pathKey(targetUp) !== pathKey(path) ? targetUp : null;
  const one = selectedEntries.length === 1 ? selectedEntries[0] : null;

  // v0.10: "제거 (제어판처럼)…" for one folder of the list's menu (a folder's listing or search
  // results; not a folder read with administrator rights) that is an installed program's own: an
  // AI 검색 hit says which, else program_at is asked as the menu opens (it keeps its list a minute).
  const menuFolder = menu && !menu.target && !menu.background && one?.is_dir && !readOnly ? one.path : null;
  const hitProg = menuFolder ? (jevRes?.hits.get(menuFolder)?.program ?? null) : null;
  useEffect(() => {
    if (!menuFolder || hitProg) return;
    const req = ++progReq.current;
    v07.programAt(menuFolder).then(
      (prog) => req === progReq.current && setMenuProg({ path: menuFolder, prog }),
      () => req === progReq.current && setMenuProg({ path: menuFolder, prog: null }),
    );
  }, [menuFolder, hitProg]);
  const menuProgram = hitProg ?? (menuFolder && menuProg?.path === menuFolder ? menuProg.prog : null);

  /** Starts the program's own uninstaller (programs.rs, as 프로그램 정리 does); the folder is never deleted here. */
  const runUninstall = async (d: Extract<TabDialog, { kind: "uninstall" }>) => {
    setDialog(null);
    if (uninstalling) return;
    setUninstalling(true);
    const name = shown(d.prog.name);
    const store = d.prog.kind === "store";
    try {
      await v07.uninstall(d.prog.id);
      // What the menu knew of the program is not kept: its uninstaller may leave the folder (files
      // it keeps, or stopped halfway), and the menu then asks program_at again (the backend forgets
      // its list of installed programs once an uninstaller starts), not the AI 검색 hit's copy.
      const gone = (h: JevHit) => h.program?.id === d.prog.id;
      setAiRes((r) => {
        const j = r?.jev;
        if (!r || !j || !j.found.items.some(gone)) return r;
        const hits = j.found.items.map((h) => (gone(h) ? { ...h, program: null } : h));
        return jevResults(r.key, j.query, j.scope, j.found, hits);
      });
      setMenuProg((m) => (m?.prog?.id === d.prog.id ? null : m));
      // Gone from the disk, the folder leaves the listing and the results by itself (live changes).
      toast(
        store
          ? `"${name}"을(를) 이 사용자에서 제거했어요. 다시 쓰려면 Microsoft Store에서 설치하면 돼요`
          : `"${name}"의 제거 프로그램을 실행했어요. 뜨는 창의 안내를 따라 주세요. 제거가 끝나 폴더가 없어지면 목록에서도 빠져요`,
      );
    } catch (err) {
      const why = String(err);
      toast(store ? (why.startsWith("앱을 지우지 못했어요") ? why : `앱을 지우지 못했어요: ${why}`) : `제거 프로그램을 실행하지 못했어요: ${why}`, "error");
    } finally {
      setUninstalling(false);
    }
  };

  const previewEntry = treeShown ? (treeFocus?.e ?? null) : mapShown ? null : (rows[focus] ?? null);
  const previewSized = treeShown ? !!treeFocus?.sized : searching || !!fresh?.indexed;
  // The details pane: several items selected in the list or icons show as one summary.
  const detailsMany = !treeShown && !mapShown && selectedEntries.length > 1;
  // The size of the folder shown, for the share of it an item (or the selection) takes: known in
  // a scanned folder; in the tree, for its top rows only.
  const detailsParent =
    searching || !fresh?.indexed || !current
      ? undefined
      : !treeShown
        ? total
        : previewEntry && treeFocus?.sized && pathKey(parentOf(previewEntry.path) ?? "") === pathKey(path)
          ? total
          : undefined;
  // The 정리 도우미 (the side pane's other tab) follows the one item picked (the tree's focused row;
  // the map has none), else the folder shown; "고정" keeps one while you go elsewhere. Search
  // results are no folder to look at, and a folder read with administrator rights is read-only.
  const helperPick = treeShown ? (treeFocus?.e ?? null) : mapShown ? null : one;
  const helperWhy = readOnly
    ? "관리자 권한으로 읽는 폴더에서는 정리 도우미를 쓸 수 없어요. 읽기만 할 수 있어서 정리 계획도 실행할 수 없어요"
    : searching && !helperPick
      ? "검색 결과에서는 항목을 하나 고르면 정리 도우미가 그 항목을 봐요"
      : !path
        ? "볼 폴더가 없어요"
        : null;
  const helperAuto: HelperTarget | null = helperWhy
    ? null
    : helperPick
      ? { path: helperPick.path, name: helperPick.name, is_dir: helperPick.is_dir }
      : { path, name: folderTitle(path), is_dir: true };
  const helperTarget = helperPin ?? helperAuto;
  const helperOn = p.pane.open && p.pane.tab === "helper";
  const detailsOn = p.pane.open && p.pane.tab === "details";
  // What the command bar's clipboard buttons act on: as Ctrl+C / Ctrl+X do (the tree's row; the
  // map has no selection). Rename and trash stay with the list and icons, as their keys do.
  const clipItems = treeShown ? (treeFocus ? [treeFocus.e] : []) : mapShown ? [] : selectedEntries;
  const listKeys = !treeShown && !mapShown;
  const noClip = readOnly
    ? READ_ONLY
    : mapShown
      ? "지도에는 고른 항목이 없어요. 목록이나 아이콘 보기에서 골라 주세요"
      : clipItems.length
        ? undefined
        : "먼저 항목을 골라 주세요";
  const noPaste = searching
    ? "검색 결과에는 붙여넣을 수 없어요. 검색을 닫고 붙여넣으세요"
    : readOnly
      ? READ_ONLY
      : canPaste === false
        ? "클립보드에 붙여넣을 파일이 없어요"
        : !path
          ? "붙여넣을 폴더가 없어요"
          : undefined;
  const noRename = readOnly
    ? READ_ONLY
    : !listKeys
      ? "이름 바꾸기는 목록이나 아이콘 보기에서 해요"
      : one
        ? undefined
        : "이름을 바꿀 항목을 하나만 골라 주세요";
  const noTrash = readOnly
    ? READ_ONLY
    : !listKeys
      ? "휴지통으로 보내기는 목록이나 아이콘 보기에서 해요"
      : selectedEntries.length
        ? undefined
        : "먼저 항목을 골라 주세요";
  // The command bar folds into "⋯" in a narrow window; measured again when its buttons change.
  const fold = useFold(cmdBar, `${p.planDrafts > 0 ? (p.planDrafts > 99 ? 3 : String(p.planDrafts).length) : 0}`);

  // The breadcrumbs show their end (the folder shown) when the path is longer than the box.
  useLayoutEffect(() => {
    const el = crumbsBox.current;
    if (!el) return;
    const end = () => {
      el.scrollLeft = el.scrollWidth;
    };
    end();
    const ro = new ResizeObserver(end);
    ro.observe(el);
    return () => ro.disconnect();
  }, [path, editingAddress]);
  const pasteTitle = canPaste === false ? "클립보드에 붙여넣을 파일이 없어요" : undefined;
  const emptyText = jevRes
    ? jevEmpty(jevRes.found, jevLow ? 0 : jevHidden)
    : searching
      ? "검색 결과가 없어요"
      : fresh || loadError
        ? "빈 폴더예요"
        : "불러오는 중…";
  const denied = !searching && !readOnly && loadError !== null && isDenied(loadError);
  // Refused, and somewhere the admin session can read (not a network place).
  const elevatable = denied && adminCanRead(path, p.drives);

  const openDialog = (d: TabDialog) => {
    setMenu(null);
    setDialog(d);
  };

  /**
   * v0.8.1: a folder's "즐겨찾기에 고정", or "고정 해제" when the sidebar's 즐겨찾기 has it.
   * `what`: the menu of the empty area says which folder ("이 폴더를 ").
   */
  const pinItem = (folder: string, what = "") => {
    const on = p.pinned(folder);
    return (
      <button onClick={() => (setMenu(null), p.onPin(folder, !on))} {...tip(on ? "unpin" : "pin")}>
        {what}
        {on ? "즐겨찾기에서 고정 해제" : "즐겨찾기에 고정"}
      </button>
    );
  };

  // ---- v0.6: AI and plans ----

  // A plan made in the background opens once it is there, unless the user moved on meanwhile
  // (another tab, another dialog; `open` false: see the helper's below): it then waits in the list
  // of plans, and the toast says where that is (in "⋯" when the bar is folded). `waits`: the toast's
  // text then.
  const showPlan = (planId: string, open = true, waits = `정리 계획을 만들었어요. ${PLANS_AT}에서 열 수 있어요`) => {
    p.onPlansChanged();
    if (open && activeRef.current && !busyRef.current) setDialog({ kind: "plan", id: planId });
    else toast(waits);
  };

  // The 정리 도우미: what it got for each item and its request out live with the tab, so they
  // outlast the pane (closed, or showing 상세 정보). Its settings are read while it shows. Its
  // answers may come long after they were asked for: a plan opens (as a dialog that takes the focus)
  // only while the pane still shows the helper on that item and nothing is being typed (a rename
  // box, the search box, the address); else it waits under the item's "계획 열기" and in the list.
  const helper = useHelper(active && helperOn, p.ai, (planId, t) => {
    p.onAiChanged();
    const there = helperOn && helperTarget !== null && pathKey(helperTarget.path) === pathKey(t.path);
    showPlan(
      planId,
      there && renaming === null && !typingNow(),
      `"${t.name}"의 정리 계획을 만들었어요. 정리 도우미의 "계획 열기"나 ${PLANS_AT}에서 열 수 있어요`,
    );
  });

  /**
   * A menu's "정리 도우미로 보기": the pane shows the helper on `e`. It is pinned there when the
   * helper would not follow it (another item picked, or a pin on something else).
   */
  const openHelper = (e: HelperTarget) => {
    setMenu(null);
    if (helperPin || pathKey(e.path) !== pathKey(helperAuto?.path ?? "")) setHelperPin(e);
    p.onShowPane("helper");
  };
  const helperOf = (e: Entry): HelperTarget => ({ path: e.path, name: e.name, is_dir: e.is_dir });

  /** "정리 계획 만들기 ▸" → a rule: made here from the scan, no AI. Opens the plan, which runs only once approved there. */
  const rulePlan = async (kind: RuleKind, at: string) => {
    setMenu(null);
    if (planning.current) {
      toast("다른 계획을 만드는 중이에요. 끝난 뒤에 다시 해 주세요");
      return;
    }
    const label = RULE_ITEMS.find((r) => r.kind === kind)?.label ?? "정리";
    planning.current = true;
    setWorking(`${label} 계획을 만드는 중…`);
    try {
      showPlan((await plans.rules(kind, at)).id);
    } catch (err) {
      toast(`계획을 만들지 못했어요: ${err}`, "error");
    } finally {
      planning.current = false;
      setWorking(null);
    }
  };

  /** Asks the AI chosen in the settings for a plan; nothing runs until the plan is approved. The caller holds `planning`. */
  const requestAiPlan = async (task: AiPlanTask, at: string) => {
    const label = AI_ITEMS.find((a) => a.task === task)?.label ?? "AI";
    setWorking(`AI에게 묻는 중… (${label})`);
    toast("AI에게 계획을 묻는 중이에요. 몇십 초 걸릴 수 있어요. 받은 계획은 실행하기 전에 확인할 수 있어요");
    try {
      showPlan((await ai.plan(task, at)).id);
    } catch (err) {
      toast(`계획을 받지 못했어요: ${err}`, "error");
    }
  };

  /** "정리 계획 만들기 ▸" → an AI plan. AI off (or no key): says how to turn it on and opens the settings. */
  const aiPlan = async (task: AiPlanTask, at: string) => {
    setMenu(null);
    const why = aiBlocker(p.ai);
    if (why) {
      toast(why);
      setDialog({ kind: "aiSettings" });
      return;
    }
    if (planning.current) {
      toast("다른 계획을 만드는 중이에요. 끝난 뒤에 다시 해 주세요");
      return;
    }
    planning.current = true;
    setWorking("AI 설정을 확인하는 중…");
    try {
      // Whether to show what goes first is decided by the settings as saved now.
      const now = await freshAi();
      const blocked = aiBlocker(now);
      if (blocked || !now) {
        toast(blocked ?? AI_OFF_HELP);
        if (activeRef.current && !busyRef.current) setDialog({ kind: "aiSettings" });
        return;
      }
      if (!now.settings.confirm_before_send) {
        await requestAiPlan(task, at);
        return;
      }
      // "보내기 전에 미리 보기": exactly what would go, before anything does.
      setWorking("보낼 내용을 만드는 중…");
      let payload: AiPayloadPreview;
      try {
        payload = await ai.preview(task, at);
      } catch (err) {
        toast(`보낼 내용을 만들지 못했어요: ${err}`, "error");
        return;
      }
      if (activeRef.current && !busyRef.current) setDialog({ kind: "aiSend", task, path: at, payload });
      // Moved on meanwhile (another tab, another dialog): nothing went, and it says so.
      else toast("보낼 내용을 보여 드리지 못해서 아무것도 보내지 않았어요 (그사이 다른 탭이나 창을 열었어요). 다시 골라 주세요");
    } finally {
      planning.current = false;
      setWorking(null);
    }
  };

  /**
   * "보내기" in the preview. What goes is made again and compared with what was shown: when it is not
   * the same (files came or went, another AI was chosen meanwhile), the new one is shown instead and
   * nothing goes. Closed while that runs, nothing goes either.
   */
  const confirmAiSend = async (d: Extract<TabDialog, { kind: "aiSend" }>) => {
    if (planning.current) return;
    planning.current = true;
    setSendCheck(true);
    setWorking("보낼 내용을 확인하는 중…");
    try {
      let again: AiPayloadPreview;
      try {
        again = await ai.preview(d.task, d.path);
      } catch (err) {
        if (dialogRef.current === d) setDialog(null);
        toast(`보낼 내용을 만들지 못해서 보내지 않았어요: ${err}`, "error");
        return;
      }
      if (dialogRef.current !== d) return;
      if (again.text !== d.payload.text || again.provider !== d.payload.provider) {
        setDialog({ ...d, payload: again, changed: true });
        return;
      }
      setDialog(null);
      await requestAiPlan(d.task, d.path);
    } finally {
      planning.current = false;
      setSendCheck(false);
      setWorking(null);
    }
  };

  // The plan's folder, or the folder an item of a step is in: shown in this tab, the plan closed
  // (it stays in the list of plans, and a run goes on).
  const planNavigate = (to: string) => {
    setDialog(null);
    p.onPlansChanged();
    navigate(to);
  };
  const isIndexed = (at: string) => !!p.drives.find((d) => d.letter === driveKey(at))?.indexed;

  /** "정리 계획 만들기 ▸": rules (no AI, from the scan) and, with an AI chosen, plans it proposes. */
  const planMenu = (at: string) => {
    const scanned = isIndexed(at);
    const why = aiBlocker(p.ai);
    return (
      <PlanFlyout>
        <div className="menu-label">규칙으로 (AI 없이)</div>
        {RULE_ITEMS.map((r) => (
          <button
            key={r.kind}
            disabled={!scanned}
            title={scanned ? r.note : "스캔한 드라이브에서만 만들 수 있어요. 먼저 드라이브를 스캔해 주세요"}
            onClick={() => rulePlan(r.kind, at)}
          >
            {r.label}
            <span className="menu-note">{r.note}</span>
          </button>
        ))}
        <hr />
        <div className="menu-label">
          AI로 {p.ai && p.ai.settings.provider !== "off" ? `(${PROVIDER_NAME[p.ai.settings.provider]})` : "(꺼짐)"}
        </div>
        {AI_ITEMS.map((a) => (
          <button key={a.task} className={why ? "off" : undefined} title={why ?? a.note} onClick={() => aiPlan(a.task, at)}>
            {a.label}…<span className="menu-note">{why ? "AI 설정에서 켜면 쓸 수 있어요" : a.note}</span>
          </button>
        ))}
      </PlanFlyout>
    );
  };

  // ---- the toolbar: the address and the search, and under them the command bar ----

  const diffTitle = searching
    ? "검색 결과에는 크기 변화 열이 없어요"
    : readOnly
      ? "관리자 권한으로 읽는 폴더는 스캔 결과와 비교할 수 없어요"
      : undefined;
  // The stop button of an AI 검색 (stopAi): it stops the run only when this tab alone waits for it.
  // `aiShared`: another tab waits for the same run.
  const aiQuitOnly = jevBusy && (jevQueued || aiStopping || !aiAlone());
  const aiShared =
    jevBusy && !jevQueued && !aiStopping && aiRun.current?.key === aiWait.current && (aiRun.current?.waiters.size ?? 0) > 1;
  // Off with no judge ready, saying what is missing (judgeBlocker).
  const riskTitle = searching
    ? "검색 결과에는 위험도 열이 없어요"
    : readOnly
      ? "관리자 권한으로 읽는 폴더는 위험도를 묻지 않아요"
      : !judgeOk
        ? `${TIPS.risk} · ${judgeBlocker(p.ai)}`
        : judge === "ollama"
          ? judgeAway
            ? `보이는 항목을 지워도 되는지 Ollama 주소 ${judgeAway}(다른 컴퓨터)의 로컬 모델에 물어 보여 주는 열이에요 (참고용, 요금 없음, 경로·크기 같은 정보를 그 컴퓨터로 보내요)`
            : "보이는 항목을 지워도 되는지 이 PC의 로컬 모델(Ollama)이 판정해 보여 주는 열이에요 (참고용, 요금 없음, 경로·크기 같은 정보만 줘요)"
          : "보이는 항목을 지워도 되는지 TypeSafe jev에 물어 보여 주는 열이에요 (참고용, 경로·크기 같은 정보만 보내요)";
  const plansTitle =
    p.planDrafts > 0
      ? `${TIPS.plans}. Claude 앱이 MCP로 제안한 계획 ${p.planDrafts.toLocaleString()}개가 확인을 기다려요`
      : undefined;
  const badge = p.planDrafts > 0 && <span className="tool-badge">{p.planDrafts > 99 ? "99+" : p.planDrafts}</span>;

  // 보기 ▾'s "열 더 보기": the list's extra columns, this tab's own switches (in "⋯" too, when folded).
  const riskOff = !judgeOk && !searching && !readOnly;
  const columnItems = view === "list" && (
    <>
      <hr />
      <div className="menu-label">열 더 보기</div>
      <button
        role="menuitemcheckbox"
        aria-checked={diffOn}
        disabled={searching || readOnly}
        onClick={toggleDiff}
        {...tip("diff", diffTitle)}
      >
        <MenuRow check={diffOn} icon={<Icon.diff />} label="크기 변화 (지난 스캔과 비교)" />
      </button>
      {/* Ready only with the judge on in the AI settings (jev's key, or a local model named). Not
          ready, it is shown off (as the plan menu's AI items) with what is missing under it, and
          stays in reach: a click says it again and opens the AI settings (toggleRisk). */}
      <button
        role="menuitemcheckbox"
        aria-checked={riskOn && judgeOk}
        className={riskOff ? "off" : undefined}
        disabled={searching || readOnly}
        onClick={toggleRisk}
        {...tip("risk", riskTitle)}
      >
        <MenuRow
          check={riskOn && judgeOk}
          icon={<Icon.risk />}
          label={
            <>
              위험도 (AI 판정)
              {riskOff && <span className="menu-note">{judgeNote(p.ai)}</span>}
            </>
          }
        />
      </button>
    </>
  );

  // 정리 ▾'s items ("⋯" has them instead, when folded): the helper, the plans, and the tools that tidy.
  const tidyItems = (
    <>
      <button role="menuitemcheckbox" aria-checked={helperOn} onClick={() => p.onTogglePane("helper")} {...tip("helper")}>
        <MenuRow check={helperOn} icon={<Icon.details />} label="정리 도우미 (옆 창)" keys="Alt+H" />
      </button>
      <button role="menuitem" onClick={() => openDialog({ kind: "plans" })} {...tip("plans", plansTitle)}>
        <MenuRow
          check={false}
          icon={null}
          label={
            <>
              정리 계획 목록…
              {badge}
              {p.planDrafts > 0 && <span className="menu-note">확인을 기다리는 계획 {p.planDrafts.toLocaleString()}개</span>}
            </>
          }
        />
      </button>
      <hr />
      <button role="menuitem" disabled={!path} onClick={() => openDialog({ kind: "dups" })} {...tip("dups")}>
        <MenuRow check={false} icon={null} label="중복 파일 찾기…" />
      </button>
      <button role="menuitem" onClick={() => openDialog({ kind: "programs" })} {...tip("programs")}>
        <MenuRow check={false} icon={null} label="프로그램 정리…" />
      </button>
      <button role="menuitem" onClick={() => openDialog({ kind: "relocs" })} {...tip("relocations")}>
        <MenuRow check={false} icon={null} label="옮긴 폴더 목록…" />
      </button>
    </>
  );
  const newFolderOff = readOnly
    ? READ_ONLY
    : searching
      ? "새 폴더는 검색을 닫고 만들 수 있어요"
      : mapShown
        ? "새 폴더는 목록이나 아이콘 보기에서 만들 수 있어요"
        : undefined;
  // The clipboard and file commands: icons in the bar, items of "⋯" when the bar folds that far.
  // Each does what its key does, and is off (saying why) where the key does nothing.
  const fileCmds: { key: string; label: string; keys: string; icon: () => React.ReactNode; off?: string; run: () => void }[] = [
    { key: "cut", label: "잘라내기", keys: "Ctrl+X", icon: Icon.cut, off: noClip, run: () => toClipboard(clipItems, true) },
    { key: "copy", label: "복사", keys: "Ctrl+C", icon: Icon.copy, off: noClip, run: () => toClipboard(clipItems, false) },
    { key: "paste", label: "붙여넣기", keys: "Ctrl+V", icon: Icon.paste, off: noPaste, run: () => paste(path) },
    { key: "rename", label: "이름 바꾸기", keys: "F2", icon: Icon.rename, off: noRename, run: () => one && setRenaming(one.path) },
    {
      key: "trash",
      label: "휴지통으로 이동",
      keys: "Delete",
      icon: Icon.trash,
      off: noTrash ?? (trashLock.busy ? "휴지통으로 옮기는 중이에요. 끝난 뒤에 다시 해 주세요" : undefined),
      run: () => !trashLock.busy && setConfirmTrash(selectedEntries),
    },
  ];

  return (
    // data-tab: App's drop finds the tab a drop lands on (see dropTarget).
    <div ref={root} className={`explorer ${active ? "" : "hidden"}`} data-tab={id}>
      <div className="toolbar">
        {/* First row, as in Explorer: where you are, and the search. */}
        <div className="toolbar-top">
          <button className="nav" aria-label="뒤로" title="뒤로 (Alt+←)" disabled={pos <= 0} onClick={() => go(-1)}>
            ←
          </button>
          <button
            className="nav"
            aria-label="앞으로"
            title="앞으로 (Alt+→)"
            disabled={pos >= history.length - 1}
            onClick={() => go(1)}
          >
            →
          </button>
          <button className="nav" aria-label="위로" title="위로 (Backspace)" disabled={!parentOf(path)} onClick={goUp}>
            ↑
          </button>
          <button className="nav" aria-label="새로 고침" title="새로 고침 (F5)" onClick={refreshView}>
            ⟳
          </button>
          <form
            className="address"
            onSubmit={(e) => {
              e.preventDefault();
              setEditingAddress(false);
              // An empty address goes nowhere; the current folder comes back.
              setAddress(path);
              if (normalizePath(address)) navigate(address);
            }}
          >
            {editingAddress ? (
              <input
                autoFocus
                aria-label="주소"
                value={address}
                onChange={(e) => setAddress(e.target.value)}
                onFocus={(e) => e.currentTarget.select()}
                onBlur={() => (setEditingAddress(false), setAddress(path))}
                onKeyDown={(e) => e.key === "Escape" && (setEditingAddress(false), setAddress(path))}
                spellCheck={false}
              />
            ) : (
              <div ref={crumbsBox} className="crumbs" title={path} onClick={() => setEditingAddress(true)}>
                {crumbs.map((c, i) => (
                  <span key={i} className="crumb-wrap">
                    {i > 0 && <span className="sep">›</span>}
                    <button
                      type="button"
                      className="crumb"
                      onClick={(e) => {
                        e.stopPropagation();
                        navigate(crumbs.slice(0, i + 1).join("\\") + (i === 0 ? "\\" : ""));
                      }}
                    >
                      {c}
                    </button>
                  </span>
                ))}
              </div>
            )}
          </form>
          {/* v0.10: one box, two modes: [이름 | AI], each with its own text and results. */}
          <div className={`search modes${aiMode ? " meaning" : ""}${aiMode && jevBusy ? " busy" : ""}`}>
            <div className="search-mode" role="group" aria-label="검색 방식">
              <button
                type="button"
                className={aiMode ? undefined : "on"}
                aria-pressed={!aiMode}
                onClick={() => void selectMode("name")}
                {...tip("searchName")}
              >
                이름
              </button>
              <button
                type="button"
                className={`${aiMode ? "on" : ""}${judgeOk ? "" : " not-ready"}`}
                aria-pressed={aiMode}
                onClick={() => void selectMode("ai")}
                {...tip("searchAi", judgeOk ? undefined : `${TIPS.searchAi} · 판정할 AI가 아직 없어요. 누르면 쓰는 방법을 알려 줘요`)}
              >
                AI
              </button>
            </div>
            <span className="search-icon">
              <Icon.search />
            </span>
            <input
              ref={searchBox}
              aria-label={
                aiMode
                  ? scopeHere
                    ? "이 폴더에서 AI 검색 (Enter)"
                    : "모든 드라이브에서 AI 검색 (Enter)"
                  : scopeHere
                    ? "이 폴더에서 이름으로 검색"
                    : "모든 드라이브에서 이름으로 검색"
              }
              aria-busy={(aiMode && jevBusy) || undefined}
              placeholder={aiMode ? "AI 검색: RPG 게임, 작년에 받은 전세 계약서 (Enter)" : "이름으로 검색 (Ctrl+F)"}
              value={aiMode ? aiQuery : query}
              onChange={(e) => (aiMode ? setAiQuery : setQuery)(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Escape") {
                  // AI: Esc stops a search out first (what was answered comes in), then empties the box.
                  if (!aiMode) setQuery("");
                  else if (jevBusy && !aiStopping) stopAi();
                  else setAiQuery("");
                }
                // AI: Enter asks; typing alone never does, nor a held Enter, nor the Enter that ends a
                // Hangul syllable being composed.
                else if (e.key === "Enter" && aiMode) {
                  e.preventDefault();
                  if (!e.repeat && !e.nativeEvent.isComposing) void runAi();
                }
              }}
              spellCheck={false}
              {...tip(aiMode ? "meaning" : "search")}
            />
            <label className={`scope${scopeHere ? " on" : ""}`} {...tip("searchHere")}>
              <input type="checkbox" checked={scopeHere} onChange={(e) => setScopeHere(e.target.checked)} />
              이 폴더만
            </label>
          </div>
        </div>

        {/* Second row, the command bar (Windows 11 Explorer's). In a narrow window the less used
            commands fold into "⋯" (useFold). */}
        <div
          ref={cmdBar}
          className={`cmdbar fold-${fold}`}
          onPointerEnter={askClipboard}
          onFocus={(e) => !e.currentTarget.contains(e.relatedTarget as Node | null) && askClipboard()}
        >
          <button
            className="cmd"
            aria-label="새 폴더"
            disabled={searching || !fresh || mapShown}
            onClick={newFolder}
            {...tip("newFolder", newFolderOff)}
          >
            <Icon.newFolder />
            <span className="cmd-label">새 폴더</span>
          </button>
          {fold < FOLD_MAX && (
            <>
              <span className="cmd-sep" aria-hidden="true" />
              {fileCmds.map((c) => (
                <button key={c.key} className="cmd icon-btn" aria-label={c.label} disabled={!!c.off} onClick={c.run} {...tip(c.key, c.off)}>
                  {c.icon()}
                </button>
              ))}
            </>
          )}
          <span className="cmd-sep" aria-hidden="true" />
          <MenuButton
            name="보기 방식"
            active={active}
            disabled={searching}
            label={
              <>
                {VIEW_ICON[view]()}
                <span className="cmd-label">보기</span>
                <Caret />
              </>
            }
            attrs={{ "aria-label": `보기: ${VIEW_LABEL[view]}`, ...tip("view", searching ? SEARCH_VIEW : undefined) }}
          >
            {VIEWS.map((v, i) => (
              <button key={v} role="menuitemradio" aria-checked={view === v} onClick={() => changeView(v)} {...tip(VIEW_TIP[v])}>
                <MenuRow check={view === v} icon={VIEW_ICON[v]()} label={VIEW_LABEL[v]} keys={`Ctrl+Shift+${i + 1}`} />
              </button>
            ))}
            {columnItems}
          </MenuButton>
          <button
            className={`cmd ${detailsOn ? "on" : ""}`}
            aria-label="상세 정보"
            aria-pressed={detailsOn}
            onClick={() => p.onTogglePane("details")}
            {...tip("details")}
          >
            <Icon.details />
            <span className="cmd-label">상세 정보</span>
          </button>
          {fold >= 1 && (
            <MenuButton
              className="cmd icon-btn more"
              name="더 보기"
              active={active}
              label={
                <>
                  <Icon.more />
                  {p.planDrafts > 0 && <span className="cmd-dot" />}
                </>
              }
              attrs={{
                "aria-label":
                  p.planDrafts > 0 ? `더 보기 · 확인을 기다리는 계획 ${p.planDrafts.toLocaleString()}개` : "더 보기",
                ...tip("more"),
              }}
            >
              {fold >= FOLD_MAX && (
                <>
                  {fileCmds.map((c) => (
                    <button key={c.key} role="menuitem" disabled={!!c.off} onClick={c.run} {...tip(c.key, c.off)}>
                      <MenuRow check={false} icon={c.icon()} label={c.label} keys={c.keys} />
                    </button>
                  ))}
                  <hr />
                </>
              )}
              <div className="menu-label">정리</div>
              {tidyItems}
              {columnItems}
            </MenuButton>
          )}

          <span className="cmd-end">
            {fold < 1 && (
              <MenuButton
                name="정리"
                active={active}
                label={
                  <>
                    <Icon.tidy />
                    <span className="cmd-label">정리</span>
                    {badge}
                    <Caret />
                  </>
                }
                attrs={{
                  "aria-label": p.planDrafts > 0 ? `정리 · 확인을 기다리는 계획 ${p.planDrafts.toLocaleString()}개` : "정리",
                  ...tip("tidy"),
                }}
              >
                {tidyItems}
              </MenuButton>
            )}
            <button className="cmd icon-btn" aria-label="AI 설정" onClick={() => openDialog({ kind: "aiSettings" })} {...tip("aiSettings")}>
              <GearIcon />
            </button>
            <button className="cmd icon-btn help-btn" aria-label="도움말" onClick={() => p.onHelp()} {...tip("help")}>
              ?
            </button>
          </span>
        </div>
      </div>

      {p.banners}

      {elevatable && (
        <div className="banner error">
          <span className="grow">
            이 폴더는 지금 권한으로 열 수 없어요. 관리자 권한으로 안의 항목을 볼 수 있어요 (읽기 전용).
            <span className="banner-detail">{loadError}</span>
          </span>
          <button disabled={adminAsking} onClick={() => readAsAdmin(path, true)} {...tip("admin")}>
            {adminAsking ? "권한을 받는 중…" : "관리자 권한으로 보기"}
          </button>
        </div>
      )}
      {readOnly && (
        <div className="banner admin-read" role="status">
          <span className="admin-read-dot" aria-hidden="true" />
          <span className="grow">
            <b>관리자 권한으로 읽는 중 (읽기 전용)</b> · 이 폴더에서는 이름 바꾸기, 휴지통, 붙여넣기를 할 수 없어요. 폴더
            크기는 보이지 않아요.
          </span>
        </div>
      )}

      {!searching && fresh && !fresh.indexed && drive && !drive.indexed && (
        <div className="banner">
          {drive.letter} 드라이브를 아직 스캔하지 않아서 폴더 크기가 보이지 않아요.
          <button disabled={p.scanning !== null} onClick={() => p.onScan(drive.letter)} {...tip("scan")}>
            {p.scanning === drive.letter ? "스캔 중…" : "지금 스캔"}
          </button>
        </div>
      )}
      {!searching && fresh && !fresh.indexed && drive?.indexed && (
        <div className="banner">
          이 폴더는 크기를 알 수 없어요. 마지막 스캔 뒤에 생겼거나, 다른 곳을 가리키는 링크(정션) 폴더예요.
        </div>
      )}
      {loadError && !searching && !readOnly && !elevatable && (
        <div className="banner error">
          {denied ? (
            <span className="grow">
              이 폴더는 지금 권한으로 열 수 없어요. 관리자 권한으로 보기는 이 컴퓨터의 드라이브에서만 할 수 있어요
              (네트워크 위치는 안 돼요).
              <span className="banner-detail">{loadError}</span>
            </span>
          ) : (
            loadError
          )}
        </div>
      )}

      {diffWanted && (
        <div className={`diff-bar ${compared?.error ? "note" : ""}`} role="status">
          {!compared ? (
            <span className="muted">지난 스캔과 비교하는 중…</span>
          ) : compared.error || !changes ? (
            <span>{compared.error}</span>
          ) : (
            <DiffSummary
              root={changes.root}
              size={current?.size ?? changes.root.size}
              info={compared.info}
              removed={changes.removed}
              partial={changes.smallest > 0}
            />
          )}
          {/* The toolbar has no button of its own for it any more: it is turned off here too. */}
          <button
            className="v06-link diff-off"
            title='크기 변화 열과 이 줄을 숨겨요. 다시 보려면 보기 ▾ → "크기 변화 (지난 스캔과 비교)"를 켜요'
            onClick={toggleDiff}
          >
            크기 변화 끄기
          </button>
        </div>
      )}

      {/* v0.9: AI mode: what the box does now, the run out (with its progress and the stop button),
          or what the answer shown took the query for. */}
      {aiMode && (jevBusy || jevRes || aiQuery.trim()) && (
        <div className="diff-bar jev-bar" role="status">
          {jevBusy ? (
            <>
              <b className="jev-asking">
                {aiStopping ? "멈추는 중…" : aiEngine === "ollama" ? "로컬 모델이 판정하는 중…" : "jev에 묻는 중…"}
                {!jevQueued && aiProgress && aiProgress.total > 0 && (
                  <span className="jev-progress">
                    {" "}
                    {Math.min(aiProgress.done, aiProgress.total)}/{aiProgress.total}
                  </span>
                )}
              </b>
              {/* As stopAi does: the run stops only when this tab alone waits for it; else this
                  tab only stops waiting (its results dropped, the run goes on for the others). */}
              <button
                type="button"
                className="btn small jev-stop"
                title={
                  aiQuitOnly
                    ? "이 검색을 그만 기다리고 결과를 버려요 (Esc)"
                    : "남은 후보는 묻지 않고, 지금까지 확인한 것만 보여요 (Esc)"
                }
                onClick={stopAi}
              >
                {aiQuitOnly ? "그만 기다리기" : "멈추기"}
              </button>
              <span className="muted">
                {jevQueued && "앞에서 보낸 검색이 끝나면 이어서 물어요. "}
                {aiStopping
                  ? `지금 판정 중인 ${aiEngine === "ollama" ? "항목" : "묶음"}이 끝나면 확인한 것만 보여요`
                  : aiShared
                    ? "다른 탭도 이 검색을 기다려요. 그만 기다려도 그 탭의 검색은 계속돼요"
                    : aiEngine === "ollama"
                      ? `${judgeAway ? "다른 컴퓨터의 로컬 모델이" : "이 PC에서"} 한 항목씩 판정해서 수십 초 걸릴 수 있어요. 멈추면(Esc) 확인한 것만 보여요`
                      : "멈추면(Esc) 확인한 것만 보여요. 이미 보낸 요청은 끝까지 가고 요금이 나가요"}
              </span>
            </>
          ) : jevRes ? (
            <JevSummary
              res={jevRes}
              typed={aiQuery.trim() !== jevRes.query}
              moved={under !== jevRes.scope}
              hidden={jevHidden}
              showLow={jevLow}
              onShowLow={setJevLow}
            />
          ) : (
            <span className="muted">
              {judge === "ollama"
                ? judgeAway
                  ? `Enter를 누르면 Ollama 주소 ${judgeAway}(다른 컴퓨터)의 로컬 모델에 뜻으로 물어봐요 (요금 없음). 글자를 치는 동안에는 보내지 않아요`
                  : "Enter를 누르면 로컬 모델이 뜻으로 판정해요 (이 PC에서, 요금 없음). 글자를 치는 동안에는 묻지 않아요"
                : "Enter를 누르면 jev에 뜻으로 물어봐요. 글자를 치는 동안에는 보내지 않아요"}
            </span>
          )}
        </div>
      )}

      <div className="content" style={{ "--preview-w": `${p.pane.width}px` } as React.CSSProperties}>
        <div
          className="pane"
          onMouseDownCapture={(ev) => {
            // A click in the list ends a text selection elsewhere (the preview), so Ctrl+C copies
            // files again.
            if (ev.target instanceof HTMLInputElement) return;
            const s = window.getSelection();
            if (s && !s.isCollapsed) s.removeAllRanges();
          }}
          onMouseDown={(ev) => {
            if (ev.button !== 1 || !gridShown) return;
            // GridView has no middle click of its own; a tile's tooltip is its path.
            const at = (ev.target as HTMLElement).closest(".tile")?.getAttribute("title");
            const e = at ? rows.find((r) => r.path === at) : undefined;
            if (e?.is_dir) {
              ev.preventDefault();
              p.onOpenTab(e.path, false);
            }
          }}
          onContextMenu={(ev) => {
            // Rows open their own menu and stop the event: this is the empty area around them.
            ev.preventDefault();
            // Nothing to paste or make in a folder read with administrator rights.
            if (searching || readOnly || !(ev.target as HTMLElement).closest(".tbody, .grid-view")) return;
            if (!treeShown) {
              setSelected(new Set());
              anchor.current = null;
            }
            openMenu({ x: ev.clientX, y: ev.clientY, background: true });
          }}
        >
          {/* Kept while search results cover it, so the open folders are still there afterwards. */}
          {view === "tree" && (
            <div className={`tree-wrap ${treeShown ? "" : "hidden"}`}>
              <TreeView
                key={path}
                root={path}
                listing={fresh}
                error={loadError}
                refresh={treeRefresh}
                active={treeShown && active}
                onNavigate={navigate}
                onOpen={activate}
                onContextMenu={(ev, e) => {
                  ev.preventDefault();
                  ev.stopPropagation();
                  // Shift+right-click: the Windows menu at once.
                  if (ev.shiftKey) shellMenu([e], ev.clientX, ev.clientY);
                  else openMenu({ x: ev.clientX, y: ev.clientY, target: e });
                }}
                onMiddleClick={(e) => p.onOpenTab(e.path, false)}
                onFocusEntry={(e, sized) => setTreeFocus(e && { e, sized })}
                renaming={renaming}
                onRenameDone={finishRename}
                onDragOut={(e) => dragItems(e, true)}
              />
            </div>
          )}

          {mapShown && <MapView key={mapNonce} path={path} onNavigate={navigate} onOpen={openFile} />}

          {gridShown && (
            <GridView
              entries={rows}
              resetKey={readOnly ? `admin:${path}` : `dir:${fresh?.path ?? ""}`}
              selected={selected}
              focus={focus}
              onSelect={select}
              onActivate={activate}
              onContextMenu={rowMenu}
              renaming={renaming}
              onRenameDone={finishRename}
              sized={!!fresh?.indexed}
              emptyText={emptyText}
              onColumns={setColumns}
              onDragOut={readOnly ? undefined : (e) => dragItems(e, false)}
              dropFolders={!readOnly}
            />
          )}

          {!treeShown && !gridShown && !mapShown && (
            <FileTable
              entries={rows}
              resetKey={shownRes ? `search:${shownRes.key}` : readOnly ? `admin:${path}` : `dir:${fresh?.path ?? ""}`}
              columns={columnsShown}
              sort={{ key: sortKey, desc: sort.desc }}
              onSort={(key) =>
                setSort((s) => ({
                  key,
                  desc:
                    s.key === key
                      ? !s.desc
                      : key === "size" || key === "files" || key === "modified" || key === "change" || key === "score",
                }))
              }
              selected={selected}
              focus={focus}
              onSelect={select}
              onActivate={activate}
              onContextMenu={rowMenu}
              renaming={renaming}
              onRenameDone={finishRename}
              barTotal={jevRes ? jevBar : shownRes ? (shownRes.items[0]?.size ?? 0) : total}
              sized={searching || !!fresh?.indexed}
              emptyText={emptyText}
              onMiddleClick={(e) => p.onOpenTab(e.path, false)}
              onDragOut={readOnly ? undefined : (e) => dragItems(e, false)}
              dropFolders={!readOnly}
              change={diffWanted ? changeCell : undefined}
              risk={riskShown ? riskCell : undefined}
              riskEngine={judge}
              riskRemote={judgeAway}
              onRange={riskShown ? onRange : undefined}
              score={jevRes ? scoreCell : undefined}
              alias={jevRes ? aliasOf : undefined}
            />
          )}
        </div>

        {/* The side pane: 상세 정보, or the 정리 도우미. Only the tab shown asks for previews. */}
        {active && p.pane.open && (
          <>
            <div className="divider" title="끌어서 옆 창 너비 바꾸기" onPointerDown={startResize} />
            <SidePane tab={p.pane.tab} onTab={p.onShowPane} onClose={() => p.onShowPane(null)}>
              {p.pane.tab === "details" ? (
                <Preview
                  entry={detailsMany ? null : previewEntry}
                  selection={detailsMany ? selectedEntries : undefined}
                  parentSize={detailsParent}
                  onCopyPaths={copyPaths}
                  sized={previewSized}
                  adminFolder={readOnly}
                  drives={p.drives}
                />
              ) : (
                <HelperPane
                  helper={helper}
                  target={helperTarget}
                  why={helperWhy}
                  pinned={helperPin !== null}
                  onPin={(on) => setHelperPin(on ? helperTarget : null)}
                  onSettings={() => openDialog({ kind: "aiSettings" })}
                  onOpenPlan={(planId) => openDialog({ kind: "plan", id: planId })}
                />
              )}
            </SidePane>
          </>
        )}
      </div>

      <footer className="status">
        <span>
          {jevRes
            ? jevLine(jevRes.found, jevMatched)
            : searching
              ? `검색 결과 ${shownRes!.total.toLocaleString()}개${shownRes!.total > shownRes!.items.length ? ` (큰 순서로 ${shownRes!.items.length.toLocaleString()}개 표시)` : ""} · ${shownRes!.ms}ms`
              : fresh
                ? `항목 ${rows.length.toLocaleString()}개${fresh.indexed && current ? ` · 전체 ${formatSize(total)}` : ""}`
                : readOnly
                  ? `항목 ${rows.length.toLocaleString()}개 · 관리자 권한으로 읽는 중 (읽기 전용)`
                  : loadError
                    ? "열 수 없는 폴더예요"
                    : "불러오는 중…"}
        </span>
        {!treeShown && !mapShown && selectedEntries.length > 0 && (
          <span>
            선택 {selectedEntries.length.toLocaleString()}개 · {formatSize(selSize)}
          </span>
        )}
        <span className="spacer" />
        {working && (
          <span className="working" role="status">
            {working}
          </span>
        )}
        {/* The admin session (one UAC prompt, read-only, ends by itself): only the tab shown asks after it. */}
        {active && <AdminBar />}
        <span className={p.elevated ? "admin" : "muted perm"}>{p.elevated ? "관리자 모드" : "일반 권한"}</span>
      </footer>

      {/* The tree's menu acts on its row alone. Rename and trash stay with the list, where the
          selection they act on shows. */}
      {target && (
        <div ref={menuBox} className="menu" onClick={(e) => e.stopPropagation()}>
          {target.is_dir ? (
            <button onClick={() => (setMenu(null), navigate(target.path))} {...tip("open")}>
              이 폴더로 이동
            </button>
          ) : (
            <button onClick={() => (setMenu(null), activate(target))} {...tip("open")}>
              열기
            </button>
          )}
          {target.is_dir && (
            <button onClick={() => (setMenu(null), p.onOpenTab(target.path, true))} {...tip("openTab")}>
              새 탭에서 열기
            </button>
          )}
          {target.is_dir && pinItem(target.path)}
          <button
            onClick={() => (setMenu(null), api.reveal(target.path).catch((err) => toast(String(err), "error")))}
            {...tip("reveal")}
          >
            탐색기에서 보기
          </button>
          {targetParent && (
            <button onClick={() => (setMenu(null), navigate(targetParent))} {...tip("goParent")}>
              들어 있는 폴더로 이동
            </button>
          )}
          <hr />
          <button onClick={() => (setMenu(null), toClipboard([target], true))} {...tip("cut")}>
            잘라내기 (Ctrl+X)
          </button>
          <button onClick={() => (setMenu(null), toClipboard([target], false))} {...tip("copy")}>
            복사 (Ctrl+C)
          </button>
          {target.is_dir && (
            <button
              disabled={canPaste === false}
              {...tip("paste", pasteTitle ?? "이 폴더에 붙여넣기. 이름이 같은 항목이 있으면 먼저 물어요")}
              onClick={() => (setMenu(null), paste(target.path))}
            >
              붙여넣기
            </button>
          )}
          <button onClick={() => (setMenu(null), copyPaths([target]))} {...tip("copyPath")}>
            경로 복사
          </button>
          {target.is_dir && (
            <>
              <hr />
              <button onClick={() => openDialog({ kind: "relocate", source: target })} {...tip("relocate")}>
                다른 드라이브로 옮기기…
              </button>
            </>
          )}
          <hr />
          <button onClick={() => openHelper(helperOf(target))} {...tip(target.is_dir ? "helperOpen" : "helperOpenFile")}>
            정리 도우미로 보기
          </button>
          {target.is_dir && planMenu(target.path)}
          <hr />
          <button {...tip("shellMenu", SHELL_TITLE)} onClick={() => shellMenu([target], menu?.x ?? 0, menu?.y ?? 0)}>
            Windows 메뉴 더 보기…
          </button>
        </div>
      )}

      {menu?.background && (
        <div ref={menuBox} className="menu" onClick={(e) => e.stopPropagation()}>
          <button
            disabled={canPaste === false}
            {...tip("paste", pasteTitle)}
            onClick={() => (setMenu(null), paste(path))}
          >
            붙여넣기 (Ctrl+V)
          </button>
          <button disabled={!fresh} onClick={() => (setMenu(null), newFolder())} {...tip("newFolder")}>
            새 폴더 (Ctrl+Shift+N)
          </button>
          {pinItem(path, "이 폴더를 ")}
          <hr />
          <button onClick={() => openHelper({ path, name: folderTitle(path), is_dir: true })} {...tip("helperOpen")}>
            이 폴더를 정리 도우미로 보기
          </button>
          {planMenu(path)}
        </div>
      )}

      {menu && !menu.target && !menu.background && !treeShown && !mapShown && selectedEntries.length > 0 && (
        <div ref={menuBox} className="menu" onClick={(e) => e.stopPropagation()}>
          {one && (
            <button onClick={() => (setMenu(null), activate(one))} {...tip("open")}>
              열기
            </button>
          )}
          {one?.is_dir && (
            <button onClick={() => (setMenu(null), p.onOpenTab(one.path, true))} {...tip("openTab")}>
              새 탭에서 열기
            </button>
          )}
          {one?.is_dir && pinItem(one.path)}
          {one && (
            <button
              onClick={() => (setMenu(null), api.reveal(one.path).catch((err) => toast(String(err), "error")))}
              {...tip("reveal")}
            >
              탐색기에서 보기
            </button>
          )}
          {one && searching && (
            <button onClick={() => (setMenu(null), navigate(parentOf(one.path) ?? path))} {...tip("goParent")}>
              들어 있는 폴더로 이동
            </button>
          )}
          <hr />
          {/* A folder read with administrator rights is read-only: nothing here changes it. */}
          {readOnly ? (
            <button onClick={() => (setMenu(null), copyPaths(selectedEntries))} {...tip("copyPath")}>
              경로 복사
            </button>
          ) : (
            <>
              <button onClick={() => (setMenu(null), toClipboard(selectedEntries, true))} {...tip("cut")}>
                잘라내기 (Ctrl+X)
              </button>
              <button onClick={() => (setMenu(null), toClipboard(selectedEntries, false))} {...tip("copy")}>
                복사 (Ctrl+C)
              </button>
              {one?.is_dir && (
                <button
                  disabled={canPaste === false}
                  {...tip("paste", pasteTitle ?? "이 폴더에 붙여넣기. 이름이 같은 항목이 있으면 먼저 물어요")}
                  onClick={() => (setMenu(null), paste(one.path))}
                >
                  붙여넣기
                </button>
              )}
              {one && (
                <button onClick={() => (setMenu(null), setRenaming(one.path))} {...tip("rename")}>
                  이름 바꾸기 (F2)
                </button>
              )}
              <button onClick={() => (setMenu(null), copyPaths(selectedEntries))} {...tip("copyPath")}>
                경로 복사
              </button>
              {one?.is_dir && (
                <button onClick={() => openDialog({ kind: "relocate", source: one })} {...tip("relocate")}>
                  다른 드라이브로 옮기기…
                </button>
              )}
              {one && (
                <>
                  <hr />
                  <button onClick={() => openHelper(helperOf(one))} {...tip(one.is_dir ? "helperOpen" : "helperOpenFile")}>
                    정리 도우미로 보기
                  </button>
                  {one.is_dir && planMenu(one.path)}
                </>
              )}
              <hr />
              {/* v0.10: the folder is an installed program's own: its uninstaller, as in 제어판. */}
              {one && menuProgram && (
                <button
                  disabled={!menuProgram.can_uninstall || uninstalling}
                  onClick={() => openDialog({ kind: "uninstall", prog: menuProgram, path: one.path })}
                  {...tip("uninstallHere", uninstallNote(menuProgram))}
                >
                  제거 (제어판처럼)…
                  <span className="menu-note">
                    {uninstalling ? "제거 프로그램을 실행하는 중…" : uninstallNote(menuProgram)}
                  </span>
                </button>
              )}
              <button
                className="danger"
                disabled={trashLock.busy}
                onClick={() => (setMenu(null), setConfirmTrash(selectedEntries))}
                {...tip("trash")}
              >
                휴지통으로 이동 (Delete)
              </button>
              {/* Search results from several folders: only one item at a time. */}
              {canShell(selectedEntries) && (
                <>
                  <hr />
                  <button {...tip("shellMenu", SHELL_TITLE)} onClick={() => shellMenu(selectedEntries, menu.x, menu.y)}>
                    Windows 메뉴 더 보기…
                  </button>
                </>
              )}
            </>
          )}
        </div>
      )}

      {confirmTrash && (
        <div className="modal-bg" onClick={() => setConfirmTrash(null)}>
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <h3>휴지통으로 옮길까요?</h3>
            <p>
              {confirmTrash.length === 1 ? confirmTrash[0].name : `${confirmTrash.length.toLocaleString()}개 항목`} ·{" "}
              {formatSize(confirmTrash.reduce((s, e) => s + e.size, 0))}
            </p>
            <p className="muted">
              휴지통에서 언제든 되돌릴 수 있어요. 휴지통에 넣을 수 없는 항목(USB·네트워크 드라이브, 휴지통보다 큰
              항목)은 지우지 않고 알려 드려요.
            </p>
            <div className="actions">
              <button onClick={() => setConfirmTrash(null)}>취소</button>
              <button className="primary danger" autoFocus onClick={() => doTrash(confirmTrash)}>
                휴지통으로 이동
              </button>
            </div>
          </div>
        </div>
      )}

      {/* The search runs over the folder shown when it opened; closing it cancels a search that runs. */}
      {dialog?.kind === "dups" && (
        <Duplicates scope={path} drives={p.drives} onClose={() => setDialog(null)} onReveal={revealFile} />
      )}
      {dialog?.kind === "relocs" && <Relocations onClose={() => setDialog(null)} />}
      {dialog?.kind === "relocate" && (
        <RelocateDialog
          source={dialog.source}
          drives={p.drives}
          onClose={() => setDialog(null)}
          onStarted={(job) => p.onRelocateStarted(job, dialog.source.path)}
        />
      )}

      {/* v0.6. A plan only runs from its own window, once approved there. */}
      {dialog?.kind === "aiSettings" && (
        <AiSettings
          // Saved with the window still open: App reads them now, not once it closes (위험도 that is
          // on follows at once: another judge asked, or the column off when it would go elsewhere).
          onSaved={p.onAiChanged}
          onClose={() => {
            const thenAi = dialog.thenAi;
            setDialog(null);
            p.onAiChanged();
            // Opened from AI 검색's "AI 설정 열기": with a judge ready now, the box turns to AI.
            if (thenAi) {
              void freshAi().then((now) => {
                if (judgeReady(now)) void selectMode("ai");
              });
            }
          }}
        />
      )}
      {dialog?.kind === "plans" && (
        <PlansInbox
          onOpen={(planId) => setDialog({ kind: "plan", id: planId, inbox: true })}
          onClose={() => {
            setDialog(null);
            p.onPlansChanged();
          }}
        />
      )}
      {dialog?.kind === "plan" && (
        <PlanView
          planId={dialog.id}
          onClose={() => {
            setDialog(dialog.inbox ? { kind: "plans" } : null);
            p.onPlansChanged();
          }}
          onNavigate={planNavigate}
        />
      )}
      {dialog?.kind === "aiSend" && (
        <Dialog className="ai-preview" title="이 내용을 보낼까요?" onClose={() => setDialog(null)} onBackdrop={() => setDialog(null)}>
          <h3>이 내용을 보낼까요?</h3>
          {dialog.changed && (
            <div className="v06-warn">
              그사이 보낼 내용이나 보낼 곳이 바뀌어서 보내지 않았어요. 바뀐 내용을 다시 확인해 주세요.
            </div>
          )}
          <p className="muted">
            {AI_ITEMS.find((a) => a.task === dialog.task)?.label} · 보낼 곳:{" "}
            <b>{PROVIDER_NAME[dialog.payload.provider as AiConfig["provider"]] ?? dialog.payload.provider}</b> ·{" "}
            {formatSize(dialog.payload.bytes)}. 아래 내용이 그대로 가요. {contentsNote(dialog.payload.provider)}
          </p>
          <pre className="v06-code ai-preview-text">{dialog.payload.text}</pre>
          <p className="v06-note">
            "보내기"를 누르면 보낼 내용을 다시 만들어 이것과 같을 때만 보내요. 받은 계획은 바로 실행하지 않아요. 계획 창에서 단계를
            확인하고 실행해야 바뀌어요. AI 설정에서 "보내기 전에 미리 보기"를 끄면 묻지 않고 보내요.
          </p>
          <div className="actions">
            <button onClick={() => setDialog(null)}>보내지 않기</button>
            <button className="primary" autoFocus disabled={sendCheck} onClick={() => void confirmAiSend(dialog)}>
              {sendCheck ? "확인하는 중…" : "보내기"}
            </button>
          </div>
        </Dialog>
      )}
      {dialog?.kind === "riskAsk" && (
        <Dialog
          className="ai-preview"
          title={dialog.engine === "jev" ? "jev 위험도를 볼까요?" : "위험도를 볼까요?"}
          onClose={() => setDialog(null)}
          onBackdrop={() => setDialog(null)}
        >
          {dialog.engine === "jev" ? (
            <>
              <h3>jev 위험도를 볼까요?</h3>
              <p>
                켜 두는 동안 이 탭의 목록에 보이는 항목(한 번에 {RISK_BATCH.jev}개까지)의 경로, 이름, 종류, 크기, 날짜, 분류를
                TypeSafe jev로 보내 위험도를 받아요. 다른 폴더로 가면 그 폴더에서 보이는 항목도 보내요.
              </p>
              <p className="muted">
                파일 내용은 보내지 않아요. 내 jev API 키로 보내고, 받은 위험도는 참고용이에요. 한 번 받은 항목은 앱을 끌 때까지
                다시 묻지 않아요. 끄려면 보기 ▾ → "위험도 (AI 판정)"를 한 번 더 누르세요.
              </p>
            </>
          ) : (
            <>
              <h3>위험도를 볼까요?</h3>
              <p>
                켜 두는 동안 이 탭의 목록에 보이는 항목(한 번에 {RISK_BATCH.ollama}개까지)의 경로, 이름, 종류, 크기, 날짜, 분류를
                Ollama 주소 <b>{dialog.url}</b>로 보내 로컬 모델의 위험도를 받아요. 다른 폴더로 가면 그 폴더에서 보이는 항목도 보내요.
              </p>
              <div className="v06-warn">이 주소는 이 컴퓨터가 아니에요. 보낸 정보는 그 컴퓨터로 가요.</div>
              <p className="muted">
                파일 내용은 보내지 않아요. 요금은 없고, 받은 위험도는 참고용이에요. 한 번 받은 항목은 앱을 끌 때까지 다시 묻지
                않아요. 끄려면 보기 ▾ → "위험도 (AI 판정)"를 한 번 더 누르세요.
              </p>
            </>
          )}
          <div className="actions">
            <button onClick={() => setDialog(null)}>보내지 않기</button>
            <button
              className="primary"
              autoFocus
              onClick={() => {
                setDialog(null);
                if (!judgeOk) return;
                riskAgreed.current = dialog.target;
                setRiskOn(true);
              }}
            >
              켜기
            </button>
          </div>
        </Dialog>
      )}
      {dialog?.kind === "aiFindAsk" && (
        <Dialog
          className="ai-preview"
          title={dialog.run ? "AI 검색으로 보낼까요?" : "AI 검색을 켤까요?"}
          onClose={() => setDialog(null)}
          onBackdrop={() => setDialog(null)}
        >
          <h3>{dialog.run ? "AI 검색으로 보낼까요?" : "AI 검색을 켤까요?"}</h3>
          {dialog.changed && (
            <div className="v06-warn">판정 엔진이나 보낼 곳이 바뀌었어요. 바뀐 곳으로 보내기 전에 한 번 더 물어요.</div>
          )}
          {dialog.engine === "jev" ? (
            <>
              <p>
                AI 검색에서 Enter를 누를 때마다 찾는 말과 오늘 날짜, 그리고 후보 항목을 TypeSafe jev로 보내 찾는 말에 맞는지 물어요.
                파일은 최대 {JEV_FILE_MAX}개의 경로, 이름, 분류, 크기, 수정한 날짜를, 게임·프로그램은 최대 {JEV_APP_MAX}개의 이름,
                설치 폴더, 게시자를 보내요. 글자를 치는 동안에는 보내지 않아요.
              </p>
              <p className="muted">
                파일 내용은 보내지 않아요(Steam 게임 이름만 Steam의 설치 정보 파일에서 읽어 보내요). 내 jev API 키로 보내서 요금은
                jev의 조건대로 키 주인에게 청구되고, 한 번 찾을 때 보통 2~6번 요청해요. 묻는 중에 멈추면(Esc) 남은 후보는 보내지
                않지만, 이미 보낸 요청은 끝까지 가고 요금이 나가요. 같은 검색을 다시 하면 저장해 둔 답을 써요(앱을 끌 때까지). 받은
                결과는 참고용이에요. 이름 검색으로 돌아가려면 검색창 앞의 "이름"을 누르세요.
              </p>
            </>
          ) : (
            <>
              <p>
                AI 검색에서 Enter를 누를 때마다 찾는 말과 후보 항목을 Ollama 주소 <b>{dialog.url}</b>로 보내 로컬 모델에게 한
                항목씩 물어요. 파일은 최대 {LOCAL_FILE_MAX}개의 경로와 수정한 날짜를(오늘 날짜와 함께), 게임·프로그램은 최대{" "}
                {LOCAL_APP_MAX}개의 이름과 설치 폴더를 보내요. 글자를 치는 동안에는 보내지 않아요.
              </p>
              <div className="v06-warn">이 주소는 이 컴퓨터가 아니에요. 보낸 정보는 그 컴퓨터로 가요.</div>
              <p className="muted">
                파일 내용은 보내지 않아요(Steam 게임 이름만 Steam의 설치 정보 파일에서 읽어 보내요). 요금은 없어요. 같은 검색을 다시
                하면 저장해 둔 답을 써요(앱을 끌 때까지). 받은 결과는 참고용이에요. 이름 검색으로 돌아가려면 검색창 앞의 "이름"을
                누르세요.
              </p>
            </>
          )}
          <div className="actions">
            <button onClick={() => setDialog(null)}>{dialog.run ? "보내지 않기" : "켜지 않기"}</button>
            <button
              className="primary"
              autoFocus
              onClick={() => {
                setDialog(null);
                // The judge went off meanwhile (another window's AI settings): nothing to turn on.
                if (!judgeOk) return;
                aiAgreed.current = dialog.target;
                if (!aiMode) switchMode("ai");
                if (dialog.run) void runAi();
              }}
            >
              {dialog.run ? "보내기" : "켜기"}
            </button>
          </div>
        </Dialog>
      )}
      {dialog?.kind === "aiHow" && (
        <AiHow
          why={dialog.why}
          onClose={() => setDialog(null)}
          onSettings={() => setDialog({ kind: "aiSettings", thenAi: true })}
        />
      )}
      {dialog?.kind === "uninstall" && (
        <UninstallAsk
          prog={dialog.prog}
          path={dialog.path}
          onClose={() => setDialog(null)}
          onRun={() => void runUninstall(dialog)}
        />
      )}

      {/* v0.7. Uninstalling runs the program's own uninstaller; leftovers become a plan, which runs
          only once approved in its own window. */}
      {dialog?.kind === "programs" && (
        <Programs
          onClose={() => setDialog(null)}
          // "폴더 보기", "위치 보기": the folder opens in this tab, the dialog closed.
          onReveal={(folder) => {
            setDialog(null);
            navigate(folder);
          }}
          onOpenPlan={(planId) => {
            p.onPlansChanged();
            setDialog({ kind: "plan", id: planId });
          }}
        />
      )}
    </div>
  );
}

const PANE_TABS: { id: PaneTab; label: string; keys: string }[] = [
  { id: "details", label: "상세 정보", keys: "Alt+P" },
  { id: "helper", label: "정리 도우미", keys: "Alt+H" },
];

/**
 * The side pane: its head ([상세 정보 | 정리 도우미], one button that closes the pane) and, under it,
 * the tab shown as their tab panel. As in a tab list, only the tab shown is in the Tab order, and
 * ← → (Home End) go to the other tab and show it.
 */
function SidePane({
  tab,
  onTab,
  onClose,
  children,
}: {
  tab: PaneTab;
  onTab: (tab: PaneTab) => void;
  onClose: () => void;
  children: React.ReactNode;
}) {
  const base = useId();
  const tabId = (t: PaneTab) => `${base}-tab-${t}`;
  const panelId = (t: PaneTab) => `${base}-panel-${t}`;
  const onKey = (ev: React.KeyboardEvent) => {
    const at = PANE_TABS.findIndex((t) => t.id === tab);
    const n = PANE_TABS.length;
    const to =
      ev.key === "ArrowRight" ? (at + 1) % n : ev.key === "ArrowLeft" ? (at + n - 1) % n : ev.key === "Home" ? 0 : ev.key === "End" ? n - 1 : -1;
    if (to < 0 || ev.altKey || ev.ctrlKey || ev.metaKey) return;
    // Kept from the window's keys behind: the list, the icons and the tree move with these too.
    ev.preventDefault();
    ev.stopPropagation();
    const next = PANE_TABS[to].id;
    onTab(next);
    document.getElementById(tabId(next))?.focus();
  };
  return (
    <aside className="preview" aria-label={tab === "helper" ? "정리 도우미" : "상세 정보"}>
      <div className="preview-head pane-head">
        <div className="pane-tabs" role="tablist" aria-label="옆 창" onKeyDown={onKey}>
          {PANE_TABS.map((t) => (
            <button
              key={t.id}
              type="button"
              role="tab"
              id={tabId(t.id)}
              aria-controls={panelId(t.id)}
              aria-selected={tab === t.id}
              tabIndex={tab === t.id ? 0 : -1}
              className={`pane-tab${tab === t.id ? " on" : ""}`}
              onClick={() => onTab(t.id)}
              {...tip(t.id)}
            >
              {t.label}
            </button>
          ))}
        </div>
        <button
          className="preview-close"
          aria-label="옆 창 닫기"
          title={`옆 창 닫기 (${PANE_TABS.find((t) => t.id === tab)?.keys})`}
          onClick={onClose}
        >
          ✕
        </button>
      </div>
      <div className="pane-panel" role="tabpanel" id={panelId(tab)} aria-labelledby={tabId(tab)}>
        {children}
      </div>
    </aside>
  );
}

/** The toolbar's AI settings button: drawn, as fonts do not all have a gear. */
function GearIcon() {
  return (
    <svg width="15" height="15" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeLinecap="round" aria-hidden="true">
      <circle cx="8" cy="8" r="4.4" strokeWidth="1.5" />
      <circle cx="8" cy="8" r="1.6" strokeWidth="1.3" />
      <path
        strokeWidth="2"
        d="M8 1.2v1.6M8 13.2v1.6M1.2 8h1.6M13.2 8h1.6M3.2 3.2l1.1 1.1M11.7 11.7l1.1 1.1M3.2 12.8l1.1-1.1M11.7 4.3l1.1-1.1"
      />
    </svg>
  );
}

/**
 * The context menu's "정리 계획 만들기 ▸": its items open beside the menu while the pointer is on
 * it, or after a click. A short pause lets the pointer cross the gap between the two.
 */
function PlanFlyout({ children }: { children: React.ReactNode }) {
  const [open, setOpen] = useState(false);
  const item = useRef<HTMLButtonElement>(null);
  const box = useRef<HTMLDivElement>(null);
  const timer = useRef<number | null>(null);
  const keep = () => {
    if (timer.current !== null) clearTimeout(timer.current);
    timer.current = null;
  };
  const show = () => {
    keep();
    setOpen(true);
  };
  const hide = () => {
    keep();
    timer.current = window.setTimeout(() => setOpen(false), 250);
  };
  useEffect(() => keep, []);
  // Right of the item, or left of the menu where it would not fit; kept inside the window.
  useLayoutEffect(() => {
    const el = box.current;
    const at = item.current?.closest(".menu")?.getBoundingClientRect();
    const row = item.current?.getBoundingClientRect();
    if (!open || !el || !at || !row) return;
    const { width, height } = el.getBoundingClientRect();
    const left = at.right + 2 + width <= window.innerWidth - 4 ? at.right + 2 : Math.max(4, at.left - width - 2);
    el.style.left = `${left}px`;
    el.style.top = `${Math.max(4, Math.min(row.top - 5, window.innerHeight - height - 4))}px`;
  }, [open]);
  return (
    <div className="menu-sub" onMouseEnter={show} onMouseLeave={hide}>
      <button
        ref={item}
        className={`has-sub ${open ? "on" : ""}`}
        aria-haspopup="menu"
        aria-expanded={open}
        {...tip("planMenu")}
        onClick={() => {
          keep();
          setOpen((o) => !o);
        }}
      >
        정리 계획 만들기
        <span className="sub-arrow" aria-hidden="true">
          ▸
        </span>
      </button>
      {open && (
        <div ref={box} className="menu flyout" role="menu">
          {children}
        </div>
      )}
    </div>
  );
}

/**
 * The bar over an AI 검색's results: the query they answer, who judged, how the candidates were
 * gathered, what was left out and why, and the switch for the hits under JEV_SHOW. `typed`: the
 * box holds another query now, which Enter would ask; `moved`: "이 폴더만" (or the folder it
 * means) changed.
 */
function JevSummary(p: {
  res: JevResults;
  typed: boolean;
  moved: boolean;
  hidden: number;
  showLow: boolean;
  onShowLow: (on: boolean) => void;
}) {
  const f = p.res.found;
  const app = f.target === "app";
  const engine = engineOf(f);
  const out = jevLeftOut(f);
  const where = p.res.scope === null ? "모든 드라이브에서" : `"${folderTitle(p.res.scope)}" 폴더에서`;
  return (
    <>
      <b>AI 검색</b>
      {/* Not "이 PC의": the answer does not say which Ollama gave it. */}
      <span
        className={`jev-engine ${engine}`}
        title={engine === "ollama" ? "로컬 모델(Ollama)이 판정했어요" : "TypeSafe jev가 판정했어요"}
      >
        {JUDGE_NAME[engine]}
      </span>
      <span className="jev-query" title={p.res.query}>
        “{p.res.query}”
      </span>
      {/* Stopped early: what was judged before is all there is (Enter asks the rest; the answers
          so far come from the cache). */}
      {f.cancelled && (
        <span className="diff-method">
          {stoppedEarly(f) ? "멈췄어요 — 아직 확인한 항목이 없어요" : `멈췄어요 — 확인한 ${f.asked.toLocaleString()}개만 보여요`}
          {!p.typed && !p.moved ? `. Enter를 누르면 ${stoppedEarly(f) ? "다시 찾아요" : "나머지도 확인해요"}` : ""}
        </span>
      )}
      {/* Stopped before the query was read: how candidates would be gathered is not known. */}
      {!stoppedUnread(f) && (
        <span>
          {app
            ? "설치된 게임·프로그램 중에서 골랐어요"
            : f.terms.length
              ? `이름과 폴더 이름에 든 낱말로 후보를 모았어요: ${f.terms.join(", ")}`
              : "종류와 기간으로 후보를 모았어요"}
        </span>
      )}
      {/* Games·programs are all of them whatever the words: only a folder narrows them. */}
      {out.capped &&
        (app ? (
          <span className="diff-method">
            게임·프로그램이 많아 큰 것부터 {out.cap.toLocaleString()}개만 확인했어요 —{" "}
            {p.res.scope === null ? '"이 폴더만"으로 좁혀 보세요' : "더 안쪽 폴더에서 찾아 보세요"}
          </span>
        ) : (
          <span className="diff-method">후보가 많아 {out.cap.toLocaleString()}개만 확인했어요 — 낱말을 더 넣어 보세요</span>
        ))}
      {out.unanswered > 0 && (
        <span className="diff-method">
          {judgeSubject(engine)} 답하지 않은 후보 {out.unanswered.toLocaleString()}개는 빠졌어요
        </span>
      )}
      {f.unplaced > 0 && (
        <span className="muted" title="설치 폴더가 스캔 결과에 없는 프로그램이에요 (Microsoft Store 앱, 스캔하지 않은 드라이브의 프로그램)">
          폴더를 찾지 못한 프로그램 {f.unplaced.toLocaleString()}개는 빠졌어요
        </span>
      )}
      {p.typed ? (
        <span className="muted">Enter를 누르면 지금 쓴 말로 새로 찾아요</span>
      ) : (
        p.moved && (
          <span className="diff-method" title={p.res.scope ?? undefined}>
            범위가 바뀌었어요. 이 결과는 {where} 찾았어요. Enter를 누르면 새로 찾아요
          </span>
        )
      )}
      {p.hidden > 0 && (
        <label className={`scope jev-low${p.showLow ? " on" : ""}`} title={`${judgeSubject(engine)} ${SHOW_PCT} 미만으로 본 항목도 목록에 보여요`}>
          <input type="checkbox" checked={p.showLow} onChange={(e) => p.onShowLow(e.target.checked)} />
          {SHOW_PCT} 미만도 보기 ({p.hidden.toLocaleString()}개)
        </label>
      )}
      <span className="muted">
        {app
          ? engine === "ollama"
            ? "이름·설치 폴더만 보고 고른 참고용이에요"
            : "이름·설치 폴더·게시자만 보고 고른 참고용이에요"
          : "이름·경로·날짜 같은 정보만 보고 고른 참고용이에요"}
      </span>
    </>
  );
}

/**
 * v0.10: "AI" with no judge ready: the two ways to get one (my jev key, or a free model on this
 * PC), and the AI settings, where either is set. `why`: what is missing now.
 */
function AiHow(p: { why: string; onClose: () => void; onSettings: () => void }) {
  return (
    <Dialog className="ai-preview ai-how" title="AI 검색을 쓰려면" onClose={p.onClose} onBackdrop={p.onClose}>
      <h3>AI 검색을 쓰려면</h3>
      <p>
        이름에 없는 말("RPG 게임", "작년에 받은 전세 계약서")로 찾으려면, 후보마다 맞는지 판정할 AI가 있어야 해요. 둘 중 하나를
        골라 주세요.
      </p>
      {p.why && <div className="v06-warn">{p.why}</div>}
      <div className="ai-how-ways">
        <section className="ai-how-way">
          <h4>
            내 jev 키 <span className="ai-how-tag">유료 · 빠름</span>
          </h4>
          <p className="muted">
            TypeSafe jev의 API 키를 AI 설정에 넣어요. 한 번 찾는 데 몇 초면 되고, 보낸 만큼 jev의 조건대로 키 주인에게 청구돼요.
            찾는 말과 후보의 이름·경로 같은 정보를 jev로 보내요(파일 내용은 보내지 않아요).
          </p>
        </section>
        <section className="ai-how-way">
          <h4>
            무료 로컬 모델 <span className="ai-how-tag free">무료 · 이 PC에서</span>
          </h4>
          <p className="muted">
            Ollama를 설치하고 모델을 받아요. 이 PC에서 돌아서 요금이 없고 아무것도 밖으로 나가지 않아요. 대신 후보를 한 항목씩
            물어서 느리고(한 번 찾는 데 수십 초) jev보다 덜 정확할 수 있어요.
          </p>
          <ol className="ai-how-steps">
            <li>
              <b>ollama.com</b>에서 Ollama를 설치해요
            </li>
            <li>
              터미널에서 <code>ollama pull qwen3:4b</code>로 모델을 받아요 (GPU 4GB 이상 추천, 더 정확하게는 <code>qwen3:8b</code>)
            </li>
            <li>AI 설정 → "AI 판정"에서 "로컬 모델"을 고르고 모델 이름을 넣어요</li>
          </ol>
        </section>
      </div>
      <p className="muted">어느 쪽이든 AI 설정에서 "AI 판정 쓰기"를 켜 주세요. 이름 검색은 AI 없이 그대로 써요.</p>
      <div className="actions">
        <button onClick={p.onClose}>닫기</button>
        <button className="primary" autoFocus onClick={p.onSettings}>
          AI 설정 열기
        </button>
      </div>
    </Dialog>
  );
}

/**
 * v0.10: "제거 (제어판처럼)…": what the program is and what its uninstaller does, before it runs
 * (as 프로그램 정리's question). Nothing is deleted here: the program's own uninstaller is started,
 * or a Store app removed for this user.
 */
function UninstallAsk(p: { prog: ProgramRef; path: string; onClose: () => void; onRun: () => void }) {
  const g = p.prog;
  const name = shown(g.name);
  const store = g.kind === "store";
  const facts = [
    g.publisher && shown(g.publisher),
    g.size !== null && formatSize(g.size),
    store ? "Microsoft Store 앱" : g.machine_wide ? "모든 사용자용" : "내 계정용",
  ].filter(Boolean);
  return (
    <Dialog className="v07-confirm uninstall-ask" title={`${name} 제거`} onClose={p.onClose} onBackdrop={p.onClose}>
      <h3>{name}을(를) 제거할까요?</h3>
      <p className="muted">
        {facts.join(" · ")}
        <br />
        <span className="uninstall-path" title={p.path}>
          {shown(p.path)}
        </span>
      </p>
      {store ? (
        // Remove-AppxPackage (programs.rs): no window of its own and nothing more is asked.
        <div className="v06-warn">
          이 사용자에서 바로 제거해요. 제거 창이 뜨지 않고 더 묻지도 않아요. 앱이 저장해 둔 데이터(설정, 앱 안에 둔 파일)도 함께
          지워지고 휴지통으로 가지 않아요. 다른 계정의 앱은 그대로예요.
        </div>
      ) : (
        <>
          <p>
            이 프로그램의 제거 프로그램을 실행해요. 뜨는 창의 안내를 따라 주세요. <b>일부 프로그램은 창 없이 바로 지워져요.</b>
          </p>
          {g.machine_wide && (
            <div className="v06-warn">모든 사용자용으로 설치된 프로그램이라 관리자 권한을 물어요(UAC 창). 다른 계정에서도 없어져요.</div>
          )}
        </>
      )}
      <p className="muted">
        제어판(설정 → 앱)의 "제거"와 같아요. Steam 게임이면 Steam이 제거를 맡아요. fine-file은 폴더를 직접 지우지 않고, 제거가 끝나
        폴더가 없어지면 목록과 검색 결과에서도 빠져요. 남은 폴더는 정리 ▾(창이 좁으면 ⋯) → 프로그램 정리의 "남은 찌꺼기"에서 찾아볼 수 있어요.
      </p>
      {/* 취소 has the focus: a stray Enter must not remove anything. */}
      <div className="actions">
        <button autoFocus onClick={p.onClose}>
          취소
        </button>
        {store ? (
          <button className="primary danger" onClick={p.onRun}>
            제거
          </button>
        ) : (
          <button className="primary" onClick={p.onRun}>
            제거 프로그램 실행
          </button>
        )}
      </div>
    </Dialog>
  );
}

/**
 * The list's comparison bar: when the scan compared with was made, and how this folder changed
 * since (by its size now, which live updates keep current). `partial`: the comparison lists only
 * the biggest changes, so what it says went away may be more.
 */
function DiffSummary(p: { root: DiffNode; size: number; info: DiffInfo | null; removed: DiffNode[]; partial: boolean }) {
  const delta = p.size - p.root.prev_size;
  const pct = percentText(delta, p.root.prev_size);
  const gone = p.removed.reduce((s, c) => s + c.prev_size, 0);
  const shown = p.removed.slice(0, 10).map((c) => c.name);
  const names = shown.join(", ") + (p.removed.length > shown.length ? ` 외 ${p.removed.length - shown.length}개` : "");
  const here =
    p.root.status === "new"
      ? "지난 스캔 뒤에 생겼어요"
      : delta === 0
        ? "크기 변화 없음"
        : `${signedSize(delta)}${pct ? ` (${pct})` : ""}`;
  const note = methodNote(p.info);
  return (
    <>
      <b>{p.info?.prev_scanned_at ? formatDate(p.info.prev_scanned_at) : "지난"} 스캔과 비교</b>
      <span className={`change ${p.root.status === "new" ? "new" : delta > 0 ? "grown" : delta < 0 ? "shrunk" : "same"}`}>
        이 폴더 {here}
      </span>
      {p.removed.length > 0 && (
        <span title={names}>
          없어진 항목 {p.removed.length.toLocaleString()}개 (−{formatSize(gone)})
        </span>
      )}
      {p.partial && <span className="muted">변화가 큰 {DIFF_LIMIT}개만 비교했어요</span>}
      {p.info?.scanned_at ? <span className="muted">지금 스캔 {formatDate(p.info.scanned_at)}</span> : null}
      {note && <span className="diff-method">{note}</span>}
    </>
  );
}
