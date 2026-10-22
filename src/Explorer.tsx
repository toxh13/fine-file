import { useCallback, useEffect, useImperativeHandle, useLayoutEffect, useMemo, useRef, useState } from "react";
import {
  ai,
  api,
  methodNote,
  ops,
  plans,
  v04,
  v05,
  type AiPayloadPreview,
  type AiSettings as AiConfig,
  type DiffInfo,
  type DiffNode,
  type DriveInfo,
  type Entry,
  type KeyStatus,
  type Listing,
  type Risk,
} from "./api";
import { AdminBar, adminCanRead, adminError, ensureAdmin } from "./AdminSession";
import { AiPanel, contentsNote } from "./AiPanel";
import { AiSettings } from "./AiSettings";
import { Dialog, Duplicates } from "./Duplicates";
import { FileTable, type ChangeCell, type Column, type RiskCell, type SortKey } from "./FileTable";
import { GridView, gridStep } from "./GridView";
import { HELP_TIPS } from "./Help";
import { MapView } from "./MapView";
import { PlansInbox, PlanView } from "./PlanView";
import { Preview } from "./Preview";
import { Programs } from "./Programs";
import { RelocateDialog, Relocations } from "./Relocate";
import { dragOut, dragRefusal, screenPoint } from "./shell";
import { TreeView, type TreeRefresh } from "./TreeView";
import { driveKey, formatDate, formatSize, isSameOrInside, normalizePath, parentOf, pathKey, typeLabel } from "./format";

export type View = "list" | "tree" | "grid" | "map";
export const VIEWS: View[] = ["list", "tree", "grid", "map"];
const VIEW_LABEL: Record<View, string> = { list: "목록", tree: "트리", grid: "아이콘", map: "지도" };
const VIEW_TIP: Record<View, string> = { list: "viewList", tree: "viewTree", grid: "viewGrid", map: "viewMap" };

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
  explain: "이 항목이 무엇인지, 어느 프로그램 것인지, 지워도 되는지 AI에게 설명을 받아요 (AI는 켜야 쓰는 선택 기능)",
  planMenu: "규칙(AI 없이)이나 AI로 이 폴더의 정리 계획을 만들어요. 만들기만 하고, 계획 창에서 확인하고 실행해야 바뀌어요",
  tools: "중복 파일 찾기, 프로그램 정리, 옮긴 폴더 목록, 크기 지도",
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

/** The preview keeps at least this width, and the list beside it at least LIST_MIN. */
const PREVIEW_MIN = 220;
const LIST_MIN = 320;

const FOLDER_COLUMNS: Column[] = [
  { key: "name", label: "이름", width: "minmax(220px, 3fr)" },
  { key: "size", label: "크기", width: "minmax(150px, 1.4fr)", align: "right" },
  { key: "files", label: "파일 수", width: "90px", align: "right" },
  { key: "modified", label: "수정한 날짜", width: "140px" },
  { key: "type", label: "종류", width: "minmax(90px, 0.8fr)" },
];

/** With the list's "변화" switch on: the change since the scan before, right after the size. */
const FOLDER_DIFF_COLUMNS: Column[] = [
  ...FOLDER_COLUMNS.slice(0, 2),
  { key: "change", label: "변화", width: "minmax(104px, 0.9fr)", align: "right" },
  ...FOLDER_COLUMNS.slice(2),
];

/** With the toolbar's "위험도" on (jev): its badge for each row, last. */
const RISK_COLUMN: Column = { key: "risk", label: "위험도", width: "64px" };

const SEARCH_COLUMNS: Column[] = [
  { key: "name", label: "이름", width: "minmax(200px, 2fr)" },
  { key: "location", label: "위치", width: "minmax(200px, 2.4fr)" },
  { key: "size", label: "크기", width: "minmax(130px, 1fr)", align: "right" },
  { key: "modified", label: "수정한 날짜", width: "140px" },
];

/** Whether a listing answers the folder now shown (case and trailing separator aside). */
function sameListing(a: string, b: string) {
  return a.replace(/\\$/, "").toLowerCase() === b.replace(/\\$/, "").toLowerCase();
}

// One collator for every sort: localeCompare with an options object builds a new one per call.
const cmp = new Intl.Collator("ko", { numeric: true }).compare;

// "변화" sorts by the comparison, which only the tab has.
const SORTERS: Record<Exclude<SortKey, "change">, (a: Entry, b: Entry) => number> = {
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
  /** The toolbar's "도구" button. */
  tools?: boolean;
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

/** jev's 위험도 can be asked for: it is on in the settings and its key is saved. */
const jevReady = (s: AiState | null): s is AiState => !!s && s.settings.jev_enabled && s.keys.jev;

/** Items asked about at once (what is in sight of the list; jev_risk takes at most 400). */
const RISK_BATCH = 200;
/** An item jev gave no answer for (or a request that failed) is asked about again after this long. */
const RISK_RETRY_MS = 5 * 60 * 1000;
/** After a failed request a tab waits this long before it asks again. */
const RISK_PAUSE_MS = 30 * 1000;
const RISK_CACHE_MAX = 20000;

/**
 * jev's answers by pathKey, shared by every tab: an item is asked about once, whichever tab shows
 * it. `risk` null: asked, nothing to show (no answer, or the request failed).
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
  // v0.6: the AI panel for a folder or file, the AI settings, the plans, one plan.
  | { kind: "ai"; path: string; file: boolean }
  | { kind: "aiSettings" }
  | { kind: "plans" }
  // `inbox`: opened from the list of plans, which comes back when it closes.
  | { kind: "plan"; id: string; inbox?: boolean }
  // What an AI plan asked for from the menu would send, before it goes ("보내기 전에 미리 보기").
  // `changed`: made again on "보내기", it was not what had been shown (shown again, nothing sent).
  | { kind: "aiSend"; task: AiPlanTask; path: string; payload: AiPayloadPreview; changed?: boolean }
  // Turning the list's 위험도 on, when the settings say to ask before sending.
  | { kind: "jevAsk" }
  // v0.7: installed programs and the folders removed ones left behind.
  | { kind: "programs" };

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
  previewOpen: boolean;
  previewWidth: number;
  onTogglePreview: () => void;
  onPreviewWidth: (width: number) => void;
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
  /** Plans proposed over MCP that wait as drafts: the "계획" button's badge. */
  planDrafts: number;
  /** A plan was made, changed or removed here: App counts the drafts again. */
  onPlansChanged: () => void;
  /** The toolbar's "?": the help (App's, as F1 opens it), at `topic` when given. */
  onHelp: (topic?: string) => void;
}

/** One tab: its own history, folder, view, search, selection and tree. */
export function Explorer({ ref, ...p }: ExplorerProps) {
  const { id, active, toast, onState, onChanged } = p;

  // History and position live in one state so a move never reads a stale position.
  const [nav, setNav] = useState<{ list: string[]; pos: number }>(() => ({ list: [p.initialPath], pos: 0 }));
  const history = nav.list;
  const pos = nav.pos;
  const path = history[pos] ?? "";
  const [address, setAddress] = useState("");
  const [listing, setListing] = useState<Listing | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);

  const [query, setQuery] = useState("");
  const [scopeHere, setScopeHere] = useState(false);
  const [results, setResults] = useState<Results | null>(null);
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
  const toolsButton = useRef<HTMLButtonElement>(null);
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
  // The list's "위험도" column (jev), this tab's own switch; the rows in sight it asks about; a
  // bump when answers come in.
  const [riskOn, setRiskOn] = useState(false);
  const [range, setRange] = useState<[number, number]>([0, 0]);
  const [riskTick, setRiskTick] = useState(0);
  // A plan being made from the menu (rules, or asking an AI), for the status bar; one at a time.
  const [working, setWorking] = useState<string | null>(null);
  const planning = useRef(false);
  // "보내기" in an AI preview: what goes is being made again to compare with what was shown.
  const [sendCheck, setSendCheck] = useState(false);

  const navigate = useCallback((to: string) => {
    const target = normalizePath(to);
    setQuery("");
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
  useLayoutEffect(() => {
    pathRef.current = path;
    queryRef.current = query;
    underRef.current = under;
    searchKey.current = `${query.trim()}\n${under ?? ""}`;
    activeRef.current = active;
    busyRef.current = confirmTrash !== null || dialog !== null;
    dialogRef.current = dialog;
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
        // An older run's answer, or one for a query or scope no longer shown.
        if (seq < s.shown || key !== searchKey.current) return;
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
        if (!live && key === searchKey.current) toast(String(e), "error");
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
    if (!q) {
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

  // Search as you type.
  useEffect(() => {
    const q = query.trim();
    if (!q) {
      setResults(null);
      shownSearch.current = "";
      return;
    }
    const t = setTimeout(() => runSearch(q, under, false), 120);
    return () => clearTimeout(t);
  }, [query, under, searchNonce, runSearch]);

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

  const apply = useCallback(
    (c: Changes) => {
      const shown = activeRef.current;
      const later = owed.current;
      const dirs = [...c.dirs];
      const removed = [...c.removed];
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
      if (queryRef.current.trim()) {
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
    [reloadLive, refreshTree],
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

  const searching = results !== null;
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
  // Sorting by the change needs the column; without it the list goes by size.
  const sortKey = sort.key === "change" && !diffWanted ? "size" : sort.key;
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
  const rows = useMemo(() => {
    const list = searching ? results!.items : (fresh?.entries ?? adminEntries ?? []);
    const dir = sort.desc ? -1 : 1;
    const by = sortKey === "change" ? changeSort! : SORTERS[sortKey];
    return [...list].sort((a, b) => {
      // Folders first in name/type order, like Explorer.
      if ((sortKey === "name" || sortKey === "type") && a.is_dir !== b.is_dir) return a.is_dir ? -1 : 1;
      return by(a, b) * dir || cmp(a.name, b.name);
    });
  }, [searching, results, fresh, adminEntries, sort.desc, sortKey, changeSort]);

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

  // The list's "위험도" column: jev's answer for the rows in sight of a folder's listing (not search
  // results, nor a folder read with administrator rights), asked for in the background a moment
  // after they come into sight. The list never waits for it, and a failed request shows nothing.
  const jevOk = jevReady(p.ai);
  const riskShown = riskOn && jevOk && view === "list" && !searching && !readOnly;
  const riskWanted = riskShown && active && fresh !== null;
  const riskAsking = useRef(new Set<string>());
  const riskBusy = useRef(false);
  const riskPausedUntil = useRef(0);
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
      const want: string[] = [];
      for (const e of rows.slice(range[0], range[1])) {
        const k = pathKey(e.path);
        if (riskKnown(k) || riskAsking.current.has(k)) continue;
        want.push(e.path);
        if (want.length >= RISK_BATCH) break;
      }
      if (!want.length) return;
      const keys = want.map(pathKey);
      riskBusy.current = true;
      for (const k of keys) riskAsking.current.add(k);
      setRiskTick((n) => n + 1);
      ai.jevRisk(want)
        .then(
          (list) => {
            const got = new Map(list.map((r) => [pathKey(r.path), r]));
            for (const k of keys) rememberRisk(k, got.get(k) ?? null);
          },
          () => {
            for (const k of keys) rememberRisk(k, null);
            riskPausedUntil.current = Date.now() + RISK_PAUSE_MS;
          },
        )
        .finally(() => {
          for (const k of keys) riskAsking.current.delete(k);
          riskBusy.current = false;
          // Shown, and the next rows in sight (scrolled meanwhile) asked about.
          setRiskTick((n) => n + 1);
        });
    }, Math.max(400, pause + 50));
    return () => clearTimeout(t);
  }, [riskWanted, rows, range, riskTick]);
  const riskCell = (e: Entry): RiskCell => {
    const k = pathKey(e.path);
    if (riskAsking.current.has(k)) return "asking";
    return riskCache.get(k)?.risk ?? null;
  };
  // jev turned off in the settings, or its key removed: the column goes, and comes back off.
  useEffect(() => {
    if (!jevOk) setRiskOn(false);
  }, [jevOk]);

  const columnsShown = useMemo(() => {
    if (searching) return SEARCH_COLUMNS;
    const base = diffWanted ? FOLDER_DIFF_COLUMNS : FOLDER_COLUMNS;
    return riskShown ? [...base, RISK_COLUMN] : base;
  }, [searching, diffWanted, riskShown]);

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
    const now = await freshAi();
    if (!jevReady(now)) {
      toast(now ? "jev 위험도가 꺼져 있거나 jev 키가 없어요. AI 설정에서 확인해 주세요" : "AI 설정을 읽지 못했어요. 잠시 뒤에 다시 해 주세요");
      return;
    }
    if (!activeRef.current || busyRef.current) return;
    // What is sent, and to whom, is said first when the settings ask for it.
    if (now.settings.confirm_before_send) openDialog({ kind: "jevAsk" });
    else setRiskOn(true);
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
      () => toast(cut ? `잘라냈어요: ${what}. 옮길 폴더에서 붙여넣으세요 (Ctrl+V)` : `복사했어요: ${what}`),
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

  const openMenu = (m: Menu) => {
    setMenu(m);
    setCanPaste(null);
    const ask = ++clipAsk.current;
    ops.clipboardRead().then(
      (c) => ask === clipAsk.current && setCanPaste(c.paths.length > 0),
      () => {},
    );
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
      if (ev.ctrlKey && isLetter(ev, "f")) {
        ev.preventDefault();
        searchBox.current?.focus();
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
  // would not fit below or right of it.
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
  }, [menu, menuItems, searching]);

  // The divider between the list and the preview: dragged, it sets the preview's width.
  const startResize = (ev: React.PointerEvent<HTMLDivElement>) => {
    if (ev.button !== 0) return;
    ev.preventDefault();
    const bar = ev.currentTarget;
    const room = bar.parentElement?.clientWidth ?? 0;
    const x0 = ev.clientX;
    const w0 = p.previewWidth;
    const onWidth = p.onPreviewWidth;
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
  const previewEntry = treeShown ? (treeFocus?.e ?? null) : mapShown ? null : (rows[focus] ?? null);
  const previewSized = treeShown ? !!treeFocus?.sized : searching || !!fresh?.indexed;
  const pasteTitle = canPaste === false ? "클립보드에 붙여넣을 파일이 없어요" : undefined;
  const emptyText = searching ? "검색 결과가 없어요" : fresh || loadError ? "빈 폴더예요" : "불러오는 중…";
  const denied = !searching && !readOnly && loadError !== null && isDenied(loadError);
  // Refused, and somewhere the admin session can read (not a network place).
  const elevatable = denied && adminCanRead(path, p.drives);

  const openTools = (ev: React.MouseEvent) => {
    // The click would reach the window and close the menu it opens.
    ev.stopPropagation();
    if (menu?.tools) {
      setMenu(null);
      return;
    }
    // Under the button, lined up with its left edge; at the window's right edge, with its right
    // edge (the menu's layout puts one that does not fit to the right of x on its left).
    const r = toolsButton.current?.getBoundingClientRect();
    const x = !r ? ev.clientX : r.left + 240 <= window.innerWidth ? r.left : r.right;
    setMenu({ x, y: (r?.bottom ?? ev.clientY) + 4, tools: true });
  };
  const openDialog = (d: TabDialog) => {
    setMenu(null);
    setDialog(d);
  };

  // ---- v0.6: AI and plans ----

  // A plan made in the background opens once it is there, unless the user moved on meanwhile
  // (another tab, another dialog): it then waits under "계획".
  const showPlan = (planId: string) => {
    p.onPlansChanged();
    if (activeRef.current && !busyRef.current) setDialog({ kind: "plan", id: planId });
    else toast('정리 계획을 만들었어요. 툴바의 "계획"에서 열 수 있어요');
  };

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

  /** "AI로 설명…" and the toolbar's AI button: the AI panel for a folder or file (it says what to do while AI is off). */
  const openAi = (e: { path: string; is_dir: boolean }) => openDialog({ kind: "ai", path: e.path, file: !e.is_dir });

  // The plan's folder, or the folder an item of a step is in: shown in this tab, the plan closed
  // (it stays under "계획", and a run goes on).
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

  return (
    // data-tab: App's drop finds the tab a drop lands on (see dropTarget).
    <div ref={root} className={`explorer ${active ? "" : "hidden"}`} data-tab={id}>
      <div className="toolbar">
        <button className="nav" title="뒤로 (Alt+←)" disabled={pos <= 0} onClick={() => go(-1)}>
          ←
        </button>
        <button className="nav" title="앞으로 (Alt+→)" disabled={pos >= history.length - 1} onClick={() => go(1)}>
          →
        </button>
        <button className="nav" title="위로 (Backspace)" disabled={!parentOf(path)} onClick={goUp}>
          ↑
        </button>
        <button className="nav" title="새로 고침 (F5)" onClick={refreshView}>
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
              value={address}
              onChange={(e) => setAddress(e.target.value)}
              onFocus={(e) => e.currentTarget.select()}
              onBlur={() => (setEditingAddress(false), setAddress(path))}
              onKeyDown={(e) => e.key === "Escape" && (setEditingAddress(false), setAddress(path))}
              spellCheck={false}
            />
          ) : (
            <div className="crumbs" onClick={() => setEditingAddress(true)}>
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
        <div className="search">
          <input
            ref={searchBox}
            placeholder={scopeHere ? "이 폴더에서 검색 (Ctrl+F)" : "모든 드라이브에서 검색 (Ctrl+F)"}
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={(e) => e.key === "Escape" && setQuery("")}
            spellCheck={false}
            {...tip("search")}
          />
          <label className="scope" {...tip("searchHere")}>
            <input type="checkbox" checked={scopeHere} onChange={(e) => setScopeHere(e.target.checked)} />
            이 폴더만
          </label>
        </div>
        <div className="view-toggle" role="group" aria-label="보기 방식" title={searching ? "검색 결과는 목록으로 보여요" : undefined}>
          {VIEWS.map((v) => (
            <button
              key={v}
              className={view === v ? "on" : ""}
              aria-pressed={view === v}
              disabled={searching}
              onClick={() => changeView(v)}
              {...tip(VIEW_TIP[v], searching ? "검색 결과는 목록으로 보여요" : undefined)}
            >
              {VIEW_LABEL[v]}
            </button>
          ))}
        </div>
        {view === "list" && (
          <button
            className={`tool ${diffOn ? "on" : ""}`}
            aria-pressed={diffOn}
            disabled={searching || readOnly}
            {...tip(
              "diff",
              searching
                ? "검색 결과에는 변화 열이 없어요"
                : readOnly
                  ? "관리자 권한으로 읽는 폴더는 스캔 결과와 비교할 수 없어요"
                  : "지난 스캔 뒤로 크기가 얼마나 늘고 줄었는지 보여 주는 열을 더해요",
            )}
            onClick={toggleDiff}
          >
            변화
          </button>
        )}
        {/* jev on in the AI settings, with its key: this tab's 위험도 column. */}
        {view === "list" && jevOk && (
          <button
            className={`tool ${riskOn ? "on" : ""}`}
            aria-pressed={riskOn}
            disabled={searching || readOnly}
            title={
              searching
                ? "검색 결과에는 위험도 열이 없어요"
                : readOnly
                  ? "관리자 권한으로 읽는 폴더는 jev에 묻지 않아요"
                  : "보이는 항목을 지워도 되는지 TypeSafe jev에 물어 보여 주는 열이에요 (참고용, 경로·크기 같은 정보만 보내요)"
            }
            onClick={toggleRisk}
          >
            위험도
          </button>
        )}
        <button
          className="tool"
          disabled={searching || !fresh || mapShown}
          onClick={newFolder}
          {...tip("newFolder", readOnly ? READ_ONLY : undefined)}
        >
          새 폴더
        </button>
        <button
          className={`tool ${p.previewOpen ? "on" : ""}`}
          aria-pressed={p.previewOpen}
          onClick={p.onTogglePreview}
          {...tip("preview")}
        >
          미리 보기
        </button>
        <button
          ref={toolsButton}
          className={`tool ${menu?.tools ? "on" : ""}`}
          aria-haspopup="menu"
          aria-expanded={!!menu?.tools}
          onClick={openTools}
          {...tip("tools")}
        >
          도구 ▾
        </button>
        <button
          className="tool"
          disabled={!path || readOnly}
          {...tip(
            "ai",
            readOnly
              ? "관리자 권한으로 읽는 폴더에서는 쓸 수 없어요"
              : aiBlocker(p.ai) === null
                ? undefined
                : "정리 도우미: 규칙으로 정리 계획을 만들어요 (AI는 꺼져 있어요. 톱니바퀴(AI 설정)에서 켤 수 있어요)",
          )}
          onClick={() => openAi({ path, is_dir: true })}
        >
          AI
        </button>
        <button
          className="tool"
          {...tip(
            "plans",
            p.planDrafts > 0
              ? `정리 계획 목록 · Claude 앱이 MCP로 제안한 계획 ${p.planDrafts.toLocaleString()}개가 확인을 기다려요`
              : undefined,
          )}
          onClick={() => openDialog({ kind: "plans" })}
        >
          계획
          {p.planDrafts > 0 && <span className="tool-badge">{p.planDrafts > 99 ? "99+" : p.planDrafts}</span>}
        </button>
        <button className="tool gear" aria-label="AI 설정" onClick={() => openDialog({ kind: "aiSettings" })} {...tip("aiSettings")}>
          <GearIcon />
        </button>
        <button className="tool help-btn" aria-label="도움말" onClick={() => p.onHelp()} {...tip("help")}>
          ?
        </button>
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
        </div>
      )}

      <div className="content" style={{ "--preview-w": `${p.previewWidth}px` } as React.CSSProperties}>
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
              resetKey={searching ? `search:${results!.key}` : readOnly ? `admin:${path}` : `dir:${fresh?.path ?? ""}`}
              columns={columnsShown}
              sort={{ key: sortKey, desc: sort.desc }}
              onSort={(key) =>
                setSort((s) => ({
                  key,
                  desc: s.key === key ? !s.desc : key === "size" || key === "files" || key === "modified" || key === "change",
                }))
              }
              selected={selected}
              focus={focus}
              onSelect={select}
              onActivate={activate}
              onContextMenu={rowMenu}
              renaming={renaming}
              onRenameDone={finishRename}
              barTotal={searching ? (results!.items[0]?.size ?? 0) : total}
              sized={searching || !!fresh?.indexed}
              emptyText={emptyText}
              onMiddleClick={(e) => p.onOpenTab(e.path, false)}
              onDragOut={readOnly ? undefined : (e) => dragItems(e, false)}
              dropFolders={!readOnly}
              change={diffWanted ? changeCell : undefined}
              risk={riskShown ? riskCell : undefined}
              onRange={riskShown ? onRange : undefined}
            />
          )}
        </div>

        {/* Only the tab shown asks for previews. */}
        {active && p.previewOpen && (
          <>
            <div className="divider" title="끌어서 미리 보기 너비 바꾸기" onPointerDown={startResize} />
            <Preview
              entry={previewEntry}
              onClose={p.onTogglePreview}
              sized={previewSized}
              adminFolder={readOnly}
              drives={p.drives}
            />
          </>
        )}
      </div>

      <footer className="status">
        <span>
          {searching
            ? `검색 결과 ${results!.total.toLocaleString()}개${results!.total > results!.items.length ? ` (큰 순서로 ${results!.items.length.toLocaleString()}개 표시)` : ""} · ${results!.ms}ms`
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
          <button onClick={() => openAi(target)} {...tip("explain")}>
            AI로 설명…
          </button>
          {target.is_dir && planMenu(target.path)}
          <hr />
          <button {...tip("shellMenu", SHELL_TITLE)} onClick={() => shellMenu([target], menu?.x ?? 0, menu?.y ?? 0)}>
            Windows 메뉴 더 보기…
          </button>
        </div>
      )}

      {menu?.tools && (
        <div ref={menuBox} className="menu" role="menu" onClick={(e) => e.stopPropagation()}>
          <button role="menuitem" disabled={!path} onClick={() => openDialog({ kind: "dups" })} {...tip("dups")}>
            중복 파일 찾기…
          </button>
          <button role="menuitem" onClick={() => openDialog({ kind: "programs" })} {...tip("programs")}>
            프로그램 정리…
          </button>
          <button role="menuitem" onClick={() => openDialog({ kind: "relocs" })} {...tip("relocations")}>
            옮긴 폴더 목록…
          </button>
          <hr />
          <button
            role="menuitem"
            disabled={searching}
            onClick={() => (setMenu(null), changeView("map"))}
            {...tip("viewMap", searching ? "검색 결과는 목록으로 보여요" : undefined)}
          >
            크기 지도로 보기
          </button>
          <hr />
          <button role="menuitem" onClick={() => (setMenu(null), p.onHelp())} {...tip("help")}>
            도움말 (F1)
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
          <hr />
          <button onClick={() => openAi({ path, is_dir: true })} {...tip("explain")}>
            이 폴더를 AI로 설명…
          </button>
          {planMenu(path)}
        </div>
      )}

      {menu && !menu.target && !menu.background && !menu.tools && !treeShown && !mapShown && selectedEntries.length > 0 && (
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
                  <button onClick={() => openAi(one)} {...tip("explain")}>
                    AI로 설명…
                  </button>
                  {one.is_dir && planMenu(one.path)}
                </>
              )}
              <hr />
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
      {dialog?.kind === "ai" && (
        <AiPanel
          path={dialog.path}
          file={dialog.file}
          onClose={() => {
            setDialog(null);
            p.onAiChanged();
          }}
          onOpenPlan={(planId) => {
            p.onAiChanged();
            p.onPlansChanged();
            setDialog({ kind: "plan", id: planId });
          }}
        />
      )}
      {dialog?.kind === "aiSettings" && (
        <AiSettings
          onClose={() => {
            setDialog(null);
            p.onAiChanged();
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
      {dialog?.kind === "jevAsk" && (
        <Dialog className="ai-preview" title="jev 위험도를 볼까요?" onClose={() => setDialog(null)} onBackdrop={() => setDialog(null)}>
          <h3>jev 위험도를 볼까요?</h3>
          <p>
            켜 두는 동안 이 탭의 목록에 보이는 항목(한 번에 {RISK_BATCH}개까지)의 경로, 이름, 종류, 크기, 날짜, 분류를 TypeSafe jev로
            보내 위험도를 받아요. 다른 폴더로 가면 그 폴더에서 보이는 항목도 보내요.
          </p>
          <p className="muted">
            파일 내용은 보내지 않아요. 내 jev API 키로 보내고, 받은 위험도는 참고용이에요. 한 번 받은 항목은 앱을 끌 때까지 다시
            묻지 않아요. 끄려면 툴바의 "위험도"를 한 번 더 누르세요.
          </p>
          <div className="actions">
            <button onClick={() => setDialog(null)}>보내지 않기</button>
            <button
              className="primary"
              autoFocus
              onClick={() => {
                setDialog(null);
                setRiskOn(true);
              }}
            >
              켜기
            </button>
          </div>
        </Dialog>
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
