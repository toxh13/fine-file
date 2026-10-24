import { useEffect, useMemo, useRef, useState } from "react";
import type { UnlistenFn } from "@tauri-apps/api/event";
import {
  api,
  LINK_MAX,
  opEvents,
  ops,
  v04,
  v04Events,
  v082,
  type Category,
  type DriveInfo,
  type DupFile,
  type DupGroup,
  type DupLinkGroup,
  type DupProgress,
  type DupReport,
  type JobInfo,
} from "./api";
import { driveKey, formatDate, formatSize, normalizePath, shown } from "./format";
import "./components.css";
import "./v04.css";
import "./v0103.css";

// ---- dialog frame (shared by the v0.4 dialogs) ------------------------------------

const MODAL = '[aria-modal="true"]';

/**
 * A dialog over the window. The list's shortcuts listen on window (Delete, Backspace, F2, Ctrl+A):
 * none of them may act on the list behind while this is open, so keys stop here, as in the name
 * conflict dialog. Default actions (typing, Enter and Space on a button, arrows in a radio group)
 * still work; React's own key handlers inside do not run, so Esc and Tab are handled here. Another
 * dialog over this one (a confirm inside it, a name conflict from a job) keeps its keys and focus.
 */
export function Dialog({
  className,
  title,
  onClose,
  onBackdrop,
  children,
}: {
  className?: string;
  title: string;
  /** Esc. */
  onClose(): void;
  /** A click beside the dialog; left out, such a click does nothing. */
  onBackdrop?(): void;
  children: React.ReactNode;
}) {
  const box = useRef<HTMLDivElement>(null);
  const close = useRef(onClose);
  useEffect(() => {
    close.current = onClose;
  });

  useEffect(() => {
    const el = box.current;
    if (!el) return;
    const before = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const controls = () =>
      Array.from(
        el.querySelectorAll<HTMLElement>(
          "button:not(:disabled), input:not(:disabled), select:not(:disabled), textarea:not(:disabled), [tabindex]:not([tabindex='-1'])",
        ),
      ).filter((c) => c.closest(MODAL) === el);
    /** In another dialog, not one inside this. */
    const elsewhere = (t: EventTarget | null) => t instanceof Element && !el.contains(t) && t.closest(MODAL) !== null;
    if (!el.contains(document.activeElement)) (controls()[0] ?? el).focus();

    const onKey = (ev: KeyboardEvent) => {
      ev.stopPropagation();
      // A dialog inside this one, or over it, answers its own keys.
      if (elsewhere(ev.target) || el.querySelector(MODAL)) return;
      if (ev.key === "F5") ev.preventDefault();
      if (ev.key === "Escape") {
        ev.preventDefault();
        close.current();
        return;
      }
      if (ev.key !== "Tab") return;
      const list = controls();
      if (!list.length) return;
      ev.preventDefault();
      const at = list.indexOf(document.activeElement as HTMLElement);
      const next = at < 0 ? (ev.shiftKey ? list.length - 1 : 0) : (at + (ev.shiftKey ? -1 : 1) + list.length) % list.length;
      list[next].focus();
    };
    // The focus is not let out to the window behind, where Enter or Space would act.
    const keepIn = (ev: FocusEvent) => {
      const t = ev.target;
      if (!(t instanceof Node) || el.contains(t) || elsewhere(t)) return;
      (controls()[0] ?? el).focus();
    };
    window.addEventListener("keydown", onKey, true);
    document.addEventListener("focusin", keepIn, true);
    return () => {
      window.removeEventListener("keydown", onKey, true);
      document.removeEventListener("focusin", keepIn, true);
      const now = document.activeElement;
      if (before?.isConnected && (now === null || now === document.body || el.contains(now))) before.focus();
    };
  }, []);

  return (
    <div className="modal-bg" onClick={(e) => e.target === e.currentTarget && onBackdrop?.()}>
      <div ref={box} className={`modal${className ? ` ${className}` : ""}`} role="dialog" aria-modal="true" aria-label={title} tabIndex={-1}>
        {children}
      </div>
    </div>
  );
}

// ---- duplicate finder ---------------------------------------------------------

const MIN_SIZES: { label: string; bytes: number }[] = [
  { label: "100 KB", bytes: 100 << 10 },
  { label: "1 MB", bytes: 1 << 20 },
  { label: "10 MB", bytes: 10 << 20 },
  { label: "100 MB", bytes: 100 << 20 },
  { label: "1 GB", bytes: 1 << 30 },
];

const PHASE_LABEL: Record<DupProgress["phase"], string> = {
  size: "크기가 같은 파일을 찾는 중",
  partial: "파일 앞부분을 비교하는 중",
  full: "파일 전체를 비교하는 중",
  done: "다 찾았어요",
  cancelled: "취소했어요",
  failed: "찾지 못했어요",
};

const PHASE_STEP: Partial<Record<DupProgress["phase"], string>> = { size: "1/3", partial: "2/3", full: "3/3" };

const RANK: Record<DupProgress["phase"], number> = { size: 0, partial: 1, full: 2, done: 3, cancelled: 3, failed: 3 };
const isOver = (p: DupProgress["phase"]) => RANK[p] === 3;

/**
 * A search over has results to show: all of them once done, and the groups it had confirmed
 * (whole content compared) when it was cancelled or failed.
 */
const hasResults = (p: DupProgress) => p.phase === "done" || p.phase === "cancelled" || (p.phase === "failed" && p.groups > 0);

const NOT_IN_SCAN =
  "이 폴더는 스캔 결과에 없어요. 경로가 맞는지 확인해 주세요. 마지막 스캔 뒤에 생긴 폴더는 다시 스캔한 뒤에, 다른 곳을 가리키는 링크(정션) 폴더(다른 드라이브로 옮긴 폴더 등)는 링크가 가리키는 폴더에서 찾을 수 있어요";

// Files a program needs where they are: the backend's DupFile.category (classify.rs) says so for
// Windows, program, game and development-tool folders, also their cache folders, and app data
// (counted as "program"). Two equal model files of two programs are both in use: auto-select leaves
// such files alone unless the user turns that on, and the trash prompt asks once more for them.
const APP_CATEGORIES = new Set<Category>(["system", "program", "game", "dev"]);

/** Whether a file is in a Windows, program, game, development-tool or app-data folder. */
const inAppFolder = (f: DupFile): boolean => APP_CATEGORIES.has(f.category);

/** Windows' own folders: never picked by auto-select (even when program folders are), never linked. */
const inWindows = (f: DupFile): boolean => f.category === "system";

/**
 * In a drive's Recycle Bin: a file already deleted. The results leave such files out, so that a
 * group's "kept" copy is never one that emptying the bin (or Windows clearing old items) destroys.
 */
const inBin = (p: string): boolean => /^[a-z]:\\\$recycle\.bin(\\|$)/i.test(p.replace(/^\\\\\?\\/, ""));

const APP_NOTE =
  "프로그램·게임·개발 도구·앱 데이터 폴더 안의 파일이에요. 지우면 그 프로그램이 동작하지 않을 수 있어서, 따로 켜지 않으면 자동 선택에서 빠져요";

const WINDOWS_NOTE = "Windows 폴더 안의 파일이에요. 자동 선택에서 늘 빠지고, 하드 링크로 합치지도 않아요";

/** ops.rs's reason for a duplicate made into a link whose old copy did not go to the Recycle Bin. */
const LINK_LEFT = "합쳤지만";

// v0.10.3: what may become a hard link is the backend's rule (DupFile.link_block, the one
// start_dup_link enforces): files of games, modpacks, launchers, programs, backups, Windows,
// development trees and caches never are, nor settings and saves that programs rewrite in place.
// 0.8.2 linked any equal files, and a modpack linked with its backups would not start until its
// launcher downloaded the files again. The window only follows the rule (and says why per file).

/** The link prompt's opt-in: big models and archives in program folders (DupFile.link_optin). */
const OPTIN_LABEL = "프로그램 폴더 안의 큰 모델·압축 파일도 합치기";

/**
 * When merging is right, and when not: the link button's tooltip and the prompt. Pictures, video,
 * sound and PDFs the rule allows wait for the prompt's box (EDITABLE), so they are not offered here.
 */
const LINK_FITS =
  "같은 압축·설치 파일, 디스크 이미지, 모델 파일이 여러 폴더에 있을 때 알맞아요. 사진·영상·음악·PDF는 확인 창에서 따로 켜야 합쳐요";
const LINK_NOT =
  "게임·모드팩·프로그램·백업 폴더 안 파일, 설정·저장 파일은 합치지 않아요. 프로그램이 그런 파일을 고치거나 검사해서 다시 받으면 다른 쪽까지 바뀌거나 깨져요";

/** Where merged files are made separate again. */
const UNLINK_AT = '정리 ▾ → "하드 링크 찾기·풀기…"';

const WINDOWS_REASON = "Windows 폴더 안 파일이라 합치지 않아요";
const OTHER_DRIVE = "남길 파일과 드라이브가 달라서 합치지 않아요";
/** An older backend that does not say what may be linked: nothing is. */
const NO_RULE = "합쳐도 되는지 알 수 없어서 합치지 않아요";

/** Why `f` is not made into a hard link now (null: it may be), with the opt-in on or off. */
function linkRefusal(f: DupFile, programs: boolean): string | null {
  if (inWindows(f)) return WINDOWS_REASON;
  if (f.link_block === undefined) return NO_RULE;
  if (f.link_block === null || (programs && f.link_optin)) return null;
  return f.link_block;
}

/** The badge's tooltip for a file that is not linked by default (the opt-in's reason names its box). */
const blockNote = (f: DupFile) =>
  f.link_optin ? `${shown(f.link_block ?? "")}. 확인 창에서 켤 수 있어요` : shown(f.link_block ?? NO_RULE);

/** Groups drawn at first; more on request (a big drive can have thousands). */
const GROUP_PAGE = 100;

/**
 * Paths per trash call: lib.rs trash_paths takes them one by one and says nothing until the end,
 * so a big selection goes in parts, with progress and a cancel between them.
 */
const TRASH_CHUNK = 200;

/**
 * Files often edited in place, which hard links would tie together: the link prompt leaves them
 * out unless ticked, also where the rule allows them.
 */
const EDITABLE = new Set<Category>(["document", "media"]);
const editable = (f: DupFile): boolean => EDITABLE.has(f.category);

/** The kinds the link prompt counts, by DupFile.category. */
const KIND_LABEL: Record<Category, string> = {
  system: "Windows 폴더",
  program: "프로그램·앱 데이터 폴더",
  game: "게임 폴더",
  dev: "개발 도구 폴더",
  cache: "캐시",
  media: "사진·영상·음악",
  document: "문서",
  archive: "압축·설치 파일",
  personal: "사용자 폴더의 다른 파일",
  other: "그 밖의 파일",
};

type Rule = "newest" | "oldest" | "shortest";

function baseName(p: string): string {
  const i = p.lastIndexOf("\\");
  return i < 0 ? p : p.slice(i + 1);
}

function dirOf(p: string): string {
  const i = p.lastIndexOf("\\");
  return i < 0 ? "" : p.slice(0, i);
}

/**
 * The copy a rule keeps; ties go to the one listed first. Never one in Windows' folders while the
 * group has another: auto-select picks none of those, so such a keeper would leave every live copy
 * chosen (dups.rs lists equal times by path, and `C:\$...` comes before `C:\Users\...`).
 */
function keeperOf(g: DupGroup, rule: Rule): string {
  const live = g.files.filter((f) => !inWindows(f));
  const pool = live.length ? live : g.files;
  let best = pool[0];
  for (const f of pool) {
    if (rule === "newest" ? f.modified > best.modified : rule === "oldest" ? f.modified < best.modified : f.path.length < best.path.length) {
      best = f;
    }
  }
  return best.path;
}

/**
 * The program a file in a program's folder belongs to, as far as its path tells: the folder under
 * AppData (a Store app's package without its publisher id), Program Files or ProgramData, a Steam
 * game's folder, Windows; else the folder the file is in. For the trash prompt.
 */
function ownerOf(p: string): string {
  const parts = p.split("\\");
  const low = parts.map((s) => s.toLowerCase());
  // A folder of the path, not the drive or the file itself.
  const folder = (i: number) => (i > 0 && i < parts.length - 1 ? parts[i] : null);
  const app = low.indexOf("appdata");
  if (app > 0 && ["local", "roaming", "locallow"].includes(low[app + 1] ?? "")) {
    const at = app + 2;
    if (low[at] === "packages" && folder(at + 1)) return parts[at + 1].replace(/_[a-z0-9]{13}$/i, "");
    if (folder(at)) return parts[at];
  }
  const steam = low.indexOf("steamapps");
  if (steam > 0 && low[steam + 1] === "common" && folder(steam + 2)) return parts[steam + 2];
  const prog = low.findIndex((s) => s === "program files" || s === "program files (x86)" || s === "programdata");
  if (prog > 0 && folder(prog + 1)) return parts[prog + 1];
  if (low[1] === "windows") return "Windows";
  return baseName(dirOf(p)) || dirOf(p);
}

/** "VideoTranslator, OpenAI.Codex 외 2곳": the programs the most files belong to first. */
function ownersText(paths: string[]): string {
  const count = new Map<string, number>();
  for (const p of paths) {
    const o = ownerOf(p);
    count.set(o, (count.get(o) ?? 0) + 1);
  }
  const names = [...count].sort((a, b) => b[1] - a[1]).map(([o]) => o);
  const first = shown(names.slice(0, 3).join(", "));
  return names.length > 3 ? `${first} 외 ${(names.length - 3).toLocaleString()}곳` : first;
}

/** Items of a job that went wrong; `errors` may hold only some of them (JobInfo.error_count). */
const jobErrorCount = (j: JobInfo) => Math.max(j.error_count ?? 0, j.errors.length);

const jobActive = (s: JobInfo["state"]) => s === "running" || s === "waiting";

interface Notice {
  text: string;
  error?: boolean;
  /** Files that stayed, by reason. */
  failed?: [string, string[]][];
  /** What is said of the files in `failed`; "옮기지 못했어요" when left out. */
  failVerb?: string;
  /** Duplicates made into links whose old copy (`.<name>.ff-link`) is still there, hidden. */
  left?: string[];
}

/** What the trash button would send, and what the prompt says about it. */
interface Picked {
  paths: string[];
  bytes: number;
  /** Groups it takes files from. */
  groups: number;
  /** Of `paths`, those in a program's, game's, development tool's or app-data folder (not Windows'). */
  app: string[];
  /** Of `paths`, those in Windows' own folders (inWindows), only ever ticked by hand. */
  windows: string[];
  /** Of the groups, those not drawn now (below "묶음 더 보기"). */
  unseen: number;
}

/** What goes into a hard-link plan of the files chosen, as the link prompt's boxes say. */
interface LinkOptions {
  /** The rule of the auto-select the selection still is (its keeper stays), else null. */
  rule: Rule | null;
  /** The opt-in (OPTIN_LABEL): big models and archives in program folders too (DupFile.link_optin). */
  programs: boolean;
  /** Also documents and media (EDITABLE), which are often edited in place. */
  editable: boolean;
}

/** What "하드 링크로 합치기" would do with the selection. */
interface LinkPlan {
  groups: DupLinkGroup[];
  files: number;
  /** What the Recycle Bin gets (the old copies), so what emptying it frees. */
  bytes: number;
  /** Of the groups, those not drawn now (below "묶음 더 보기"). */
  unseen: number;
  /** Chosen files left out by the rule (linkRefusal), on another drive, or with no file to stay: by reason, most first. */
  left: [string, number][];
  leftFiles: number;
  /** Of `files`, those only the opt-in lets in (it or the file it is linked to). */
  programs: number;
  /** Documents and media chosen that could be merged (in `files` with `editable`). */
  editable: number;
  /** Of `files`, how many of each kind (KIND_LABEL), most first. */
  kinds: [string, number][];
}

/**
 * The file a group keeps when merged: the rule's keeper while the selection is as auto-select
 * made it; else the newest file not chosen (of all, when every one is). Only a file the rule lets
 * be linked (linkRefusal), so never one in Windows' folders: the copies are linked to it, so it is
 * bound as much as they are. None when no such file could stay.
 */
function linkKeeper(g: DupGroup, selected: Set<string>, rule: Rule | null, programs: boolean): DupFile | undefined {
  const ok = (f: DupFile) => linkRefusal(f, programs) === null;
  if (rule) {
    const k = keeperOf(g, rule);
    const f = g.files.find((x) => x.path === k);
    if (f && !selected.has(f.path) && ok(f)) return f;
  }
  const free = g.files.filter((f) => !selected.has(f.path));
  const pool = (free.length ? free : g.files).filter(ok);
  return pool.length ? pool.reduce((b, f) => (f.modified > b.modified ? f : b)) : undefined;
}

/**
 * The selection as hard links: each group keeps one file (linkKeeper) and each chosen one the
 * rule lets be linked becomes a link to it; the others stay as they are, counted by reason.
 * Documents and media only with `editable`. Files not chosen are never merged.
 */
function linkPlanOf(groups: DupGroup[], selected: Set<string>, drawn: number, o: LinkOptions): LinkPlan {
  const plan: LinkPlan = {
    groups: [],
    files: 0,
    bytes: 0,
    unseen: 0,
    left: [],
    leftFiles: 0,
    programs: 0,
    editable: 0,
    kinds: [],
  };
  const kinds = new Map<string, number>();
  const left = new Map<string, number>();
  const leave = (why: string, n = 1) => {
    left.set(why, (left.get(why) ?? 0) + n);
    plan.leftFiles += n;
  };
  // Only the opt-in lets it be linked.
  const optin = (f: DupFile) => linkRefusal(f, false) !== null;
  groups.forEach((g, i) => {
    const sel = g.files.filter((f) => selected.has(f.path));
    if (!sel.length) return;
    const keep = linkKeeper(g, selected, o.rule, o.programs);
    if (!keep) {
      // Nothing may stay to be linked to: what would have, says why (every copy chosen: one stays).
      const stays = g.files.find((f) => !selected.has(f.path)) ?? g.files[0];
      const why = linkRefusal(stays, o.programs) ?? NO_RULE;
      leave(`남길 파일을 합칠 수 없어요: ${why}`, sel.length >= g.files.length ? sel.length - 1 : sel.length);
      return;
    }
    const others: string[] = [];
    for (const f of sel) {
      if (f === keep) continue;
      const why = linkRefusal(f, o.programs);
      if (why !== null) {
        leave(why);
        continue;
      }
      if (driveKey(f.path) !== driveKey(keep.path)) {
        leave(OTHER_DRIVE);
        continue;
      }
      if (editable(f)) {
        plan.editable++;
        if (!o.editable) continue;
      }
      others.push(f.path);
      if (optin(f) || optin(keep)) plan.programs++;
      const kind = KIND_LABEL[f.category] ?? KIND_LABEL.other;
      kinds.set(kind, (kinds.get(kind) ?? 0) + 1);
    }
    if (!others.length) return;
    plan.groups.push({ keep: keep.path, others });
    plan.files += others.length;
    plan.bytes += others.length * g.size;
    if (i >= drawn) plan.unseen++;
  });
  plan.kinds = [...kinds].sort((a, b) => b[1] - a[1]);
  plan.left = [...left].sort((a, b) => b[1] - a[1]);
  return plan;
}

/** A "하드 링크로 합치기" job this window shows: what was sent, to bring the list up to date at its end. */
interface LinkRun {
  /** The search whose results it changes. */
  search: number | null;
  groups: DupLinkGroup[];
  /** Started before this window opened (an earlier one, another tab's): only its progress is shown. */
  adopted?: boolean;
}

/** The link prompt as it opens: the rule of the selection. Its boxes start off. */
interface LinkAsk {
  rule: Rule | null;
}

export function Duplicates({
  scope,
  drives,
  onClose,
  onReveal,
}: {
  scope: string;
  drives: DriveInfo[];
  onClose(): void;
  onReveal(path: string): void;
}) {
  const [root, setRoot] = useState(scope);
  /** The folder of the results shown (the box may have been changed since). */
  const [searched, setSearched] = useState<string | null>(null);
  const [minSize, setMinSize] = useState(1 << 20);
  const [running, setRunning] = useState(false);
  const [cancelling, setCancelling] = useState(false);
  const [progress, setProgress] = useState<DupProgress | null>(null);
  const [report, setReport] = useState<DupReport | null>(null);
  /** The report's groups, less the files sent to the trash or made into links since. */
  const [groups, setGroups] = useState<DupGroup[]>([]);
  const [selected, setSelected] = useState<Set<string>>(() => new Set());
  const [limit, setLimit] = useState(GROUP_PAGE);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<Notice | null>(null);
  /** What the last auto-select left out (files in a program's or Windows' folder). */
  const [autoNote, setAutoNote] = useState<string | null>(null);
  /** The rule of the selection, while it is as auto-select made it (no box ticked by hand since). */
  const [autoRule, setAutoRule] = useState<Rule | null>(null);
  /** Auto-select picks files in program and app-data folders too (never Windows' own). */
  const [withApp, setWithApp] = useState(false);
  const [confirm, setConfirm] = useState<Picked | null>(null);
  const [linkConfirm, setLinkConfirm] = useState<LinkAsk | null>(null);
  const [trashing, setTrashing] = useState(false);
  /** How far the trash has got (files sent of all), while it goes, and whether it was asked to stop. */
  const [trashDone, setTrashDone] = useState<{ done: number; total: number } | null>(null);
  const [trashStopping, setTrashStopping] = useState(false);
  /** Files of the results left out for being in a Recycle Bin already (inBin). */
  const [binned, setBinned] = useState(0);
  /** A "하드 링크로 합치기" job runs; its last progress event once one came. */
  const [linking, setLinking] = useState(false);
  const [linkJob, setLinkJob] = useState<JobInfo | null>(null);
  const [linkCancelling, setLinkCancelling] = useState(false);

  /** The search shown; null until its id comes back. */
  const job = useRef<number | null>(null);
  const last = useRef<DupProgress | null>(null);
  /** The end shown was assumed at a cancel; the search's own last event has not come yet. */
  const assumed = useRef(false);
  /** Files of this search sent to the trash or made into links, kept out of results loaded again. */
  const trashed = useRef(new Set<string>());
  /** Events that came before the id did (a small search can be over by then). */
  const starting = useRef(false);
  const early = useRef(new Map<number, DupProgress>());
  const listening = useRef<Promise<void>>(Promise.resolve());
  const alive = useRef(true);
  /** The link job: what it was sent, and its id once that comes back (events before it wait in linkSeen). */
  const linkRun = useRef<LinkRun | null>(null);
  const linkId = useRef<number | null>(null);
  /** The newest event of each link job since this opened, its end over anything else. */
  const linkSeen = useRef(new Map<number, JobInfo>());
  const jobListening = useRef<Promise<void>>(Promise.resolve());
  /** The trash under way was asked to stop after the part it is at. */
  const trashStop = useRef(false);

  const loadResults = (id: number) => {
    v04.dupResults(id).then(
      (r) => {
        if (!alive.current || job.current !== id) return;
        setReport(r);
        let bin = 0;
        setGroups(
          r.groups
            .map((g) => ({
              ...g,
              files: g.files.filter((f) => {
                if (!inBin(f.path)) return !trashed.current.has(f.path);
                bin++;
                return false;
              }),
            }))
            .filter((g) => g.files.length >= 2),
        );
        setBinned(bin);
      },
      (err) => {
        if (alive.current && job.current === id) setError(String(err));
      },
    );
  };

  /** `guess`: the end a cancel assumes, before the search says how it ended. */
  const apply = (p: DupProgress, guess = false) => {
    if (p.id !== job.current) return;
    const cur = last.current;
    // The search's own end after one assumed at a cancel: it tells how it really ended (it may
    // have finished first) and brings what it confirmed in the meantime.
    const real = !guess && assumed.current && cur?.id === p.id && isOver(p.phase);
    // Events may come out of order: never back to an earlier step, nothing after the end.
    if (
      !real &&
      cur &&
      cur.id === p.id &&
      (isOver(cur.phase) || RANK[p.phase] < RANK[cur.phase] || (p.phase === cur.phase && p.done < cur.done))
    ) {
      return;
    }
    assumed.current = guess;
    last.current = p;
    setProgress(p);
    if (!isOver(p.phase)) return;
    setRunning(false);
    setCancelling(false);
    if (p.phase === "failed") setError(p.detail ?? "중복 파일을 찾지 못했어요");
    if (hasResults(p)) loadResults(p.id);
  };
  const applyRef = useRef(apply);
  useEffect(() => {
    applyRef.current = apply;
  });

  useEffect(() => {
    alive.current = true;
    let dead = false;
    let off: UnlistenFn | null = null;
    listening.current = v04Events
      .dupProgress((p) => {
        if (dead) return;
        if (job.current === null) {
          if (starting.current) early.current.set(p.id, p);
          return;
        }
        applyRef.current(p);
      })
      .then(
        (f) => (dead ? f() : void (off = f)),
        () => {},
      );
    return () => {
      dead = true;
      alive.current = false;
      off?.();
      // A search nobody will see the end of.
      const id = job.current;
      if (id !== null && last.current && !isOver(last.current.phase)) v04.cancelDup(id).catch(() => {});
    };
  }, []);

  // The link job's progress (ops.rs "job-progress", op "link"). Closing the window leaves it
  // running: the job list shows it and says how it ended.
  useEffect(() => {
    let dead = false;
    let off: UnlistenFn | null = null;
    jobListening.current = opEvents
      .jobProgress((j) => {
        if (!dead) onJobRef.current(j);
      })
      .then(
        (f) => (dead ? f() : void (off = f)),
        () => {},
      );
    // A merge started before this opened (by this window closed since, or another tab's) still
    // runs: it is shown as this window's own, so nothing here searches, trashes or merges the
    // same files under it until it ends. Asked once the events are heard, so its end is not missed.
    jobListening.current
      .then(() => ops.jobs())
      .then((list) => {
        if (dead || linkRun.current) return;
        const j = list.filter((x) => x.op === "link" && jobActive(x.state)).sort((a, b) => b.id - a.id)[0];
        if (!j) return;
        // An event that came meanwhile is newer than the list.
        const now = linkSeen.current.get(j.id) ?? j;
        if (!jobActive(now.state)) return;
        linkRun.current = { search: null, groups: [], adopted: true };
        linkId.current = j.id;
        setLinking(true);
        setLinkJob(now);
        setLinkCancelling(false);
      })
      .catch(() => {});
    return () => {
      dead = true;
      off?.();
    };
  }, []);

  const start = async () => {
    const target = normalizePath(root);
    if (!target || running || linking || trashing) return;
    setError(null);
    setNotice(null);
    setAutoNote(null);
    setAutoRule(null);
    setReport(null);
    setGroups([]);
    setBinned(0);
    setSelected(new Set());
    setLimit(GROUP_PAGE);
    setSearched(target);
    setRunning(true);
    job.current = null;
    last.current = null;
    assumed.current = false;
    trashed.current = new Set();
    early.current.clear();
    starting.current = true;
    const first = (id: number): DupProgress => ({ id, phase: "size", done: 0, total: 0, groups: 0, wasted: 0, detail: null });
    setProgress(first(-1));
    try {
      await listening.current;
      // The search sees only what the scan saw. A folder the index does not have (a typo, one
      // made since, a link to elsewhere such as a folder moved to another drive) would come back
      // as "no duplicates" after looking at nothing. A drive not scanned at all gets
      // find_duplicates' own answer.
      const drive = drives.find((d) => driveKey(d.letter) === driveKey(target));
      if (drive?.indexed && !(await api.list(target).then((l) => l.indexed, () => false))) {
        if (!alive.current) return;
        setError(NOT_IN_SCAN);
        setRunning(false);
        setProgress(null);
        return;
      }
      const id = await v04.findDuplicates([target], minSize);
      if (!alive.current) {
        v04.cancelDup(id).catch(() => {});
        return;
      }
      job.current = id;
      last.current = first(id);
      setProgress(last.current);
      const ev = early.current.get(id);
      if (ev) apply(ev);
    } catch (err) {
      setError(String(err));
      setRunning(false);
      setProgress(null);
    } finally {
      starting.current = false;
      early.current.clear();
    }
  };

  const cancel = () => {
    const id = job.current;
    if (id === null || cancelling) return;
    setCancelling(true);
    v04.cancelDup(id).then(
      // The search stops at once for this page, whether or not its last event comes. What it
      // confirmed so far shows; its own last event, if it comes, brings the rest.
      () => last.current && apply({ ...last.current, phase: "cancelled" }, true),
      (err) => {
        setCancelling(false);
        setError(String(err));
      },
    );
  };

  // By hand, any file may be ticked (one in a program's folder too: the trash prompt asks again).
  const toggle = (g: DupGroup, path: string) => {
    setAutoRule(null);
    setSelected((s) => {
      const n = new Set(s);
      if (n.has(path)) {
        n.delete(path);
      } else {
        // Every group keeps at least one copy.
        if (!g.files.some((f) => f.path !== path && !n.has(f.path))) return s;
        n.add(path);
      }
      return n;
    });
  };

  const listed = groups.slice(0, limit);

  /** Files in a program's folder (inAppFolder). */
  const appFiles = useMemo(() => {
    const s = new Set<string>();
    for (const g of groups) for (const f of g.files) if (inAppFolder(f)) s.add(f.path);
    return s;
  }, [groups]);

  // Over all the groups found, also those not drawn yet ("묶음 더 보기" only pages the list; the
  // footer counts the whole selection, the prompts say how much of it is not drawn). Never a
  // file in Windows' folders, and a file in a program's folder only when `app` says so.
  const autoSelect = (rule: Rule, app = withApp) => {
    const n = new Set<string>();
    let spared = 0;
    let windows = 0;
    let appPicked = 0;
    for (const g of groups) {
      const keep = keeperOf(g, rule);
      for (const f of g.files) {
        if (f.path === keep) continue;
        if (inWindows(f)) windows++;
        else if (!inAppFolder(f)) n.add(f.path);
        else if (!app) spared++;
        else {
          n.add(f.path);
          appPicked++;
        }
      }
    }
    setSelected(n);
    setAutoRule(rule);
    const notes: string[] = [];
    if (spared > 0) {
      notes.push(
        `프로그램·게임·개발 도구·앱 데이터 폴더 안의 파일 ${spared.toLocaleString()}개는 지우면 그 프로그램이 동작하지 않을 수 있어서 고르지 않았어요. 휴지통으로 같이 보내려면 위의 "프로그램·앱 데이터 폴더 안 파일도 자동으로 고르기"를 켜세요.`,
      );
    }
    if (appPicked > 0) {
      notes.push(
        `프로그램·앱 데이터 폴더 안의 파일 ${appPicked.toLocaleString()}개도 골랐어요. 휴지통으로 옮기기 전에 한 번 더 물어요.`,
      );
    }
    if (windows > 0) {
      notes.push(`Windows 폴더 안의 파일 ${windows.toLocaleString()}개는 고르지 않았어요. 필요하면 직접 고르세요.`);
    }
    setAutoNote(notes.length ? notes.join(" ") : null);
  };

  const clearSelection = () => {
    setSelected(new Set());
    setAutoNote(null);
    setAutoRule(null);
  };

  const sizes = useMemo(() => {
    const m = new Map<string, number>();
    for (const g of groups) for (const f of g.files) m.set(f.path, g.size);
    return m;
  }, [groups]);

  /** What the trash would get: never every copy of a group, whatever the selection says. */
  const picked = useMemo((): Picked => {
    const paths: string[] = [];
    const app: string[] = [];
    const windows: string[] = [];
    let bytes = 0;
    let touched = 0;
    let unseen = 0;
    groups.forEach((g, i) => {
      const sel = g.files.filter((f) => selected.has(f.path));
      if (!sel.length || sel.length >= g.files.length) return;
      touched++;
      if (i >= limit) unseen++;
      for (const f of sel) {
        paths.push(f.path);
        if (inWindows(f)) windows.push(f.path);
        else if (inAppFolder(f)) app.push(f.path);
      }
      bytes += sel.length * g.size;
    });
    return { paths, bytes, groups: touched, app, windows, unseen };
  }, [groups, selected, limit]);

  // "하드 링크로 합치기" takes the files chosen only (v0.10.3: not those auto-select spared in
  // program folders), so it waits for a choice; its prompt says what of it may be merged and why
  // the rest may not.
  const linkAny = picked.paths.length > 0;

  const wasted = groups.reduce((s, g) => s + g.size * (g.files.length - 1), 0);
  const fileCount = groups.reduce((s, g) => s + g.files.length, 0);

  // Hard links need NTFS: a drive known to be something else (exFAT, FAT32) gets no link button.
  const fs = drives.find((d) => driveKey(d.letter) === driveKey(searched ?? root))?.fs ?? "";
  const linkable = !fs || fs.toUpperCase() === "NTFS";

  /** Takes files out of the list (gone to the trash, or links now), and out of the selection. */
  const dropFiles = (gone: Set<string>) => {
    for (const p of gone) trashed.current.add(p);
    // A group down to one file is no longer a duplicate.
    setGroups((gs) =>
      gs.map((g) => ({ ...g, files: g.files.filter((f) => !gone.has(f.path)) })).filter((g) => g.files.length >= 2),
    );
    setSelected((s) => new Set([...s].filter((p) => !gone.has(p))));
    setAutoRule(null);
  };

  // In parts of TRASH_CHUNK, each taken out of the list as it is done, with a cancel between them.
  // Closing the window stops it after the part it is at too: nobody would see how it ended.
  const doTrash = async (c: Picked) => {
    setConfirm(null);
    if (trashing || linking) return;
    setTrashing(true);
    setTrashStopping(false);
    trashStop.current = false;
    setTrashDone({ done: 0, total: c.paths.length });
    setNotice(null);
    const moved: string[] = [];
    const failed: [string, string][] = [];
    let sent = 0;
    let error: string | null = null;
    try {
      while (sent < c.paths.length && !trashStop.current && alive.current) {
        const part = c.paths.slice(sent, sent + TRASH_CHUNK);
        const r = await api.trash(part);
        sent += part.length;
        moved.push(...r.moved);
        failed.push(...r.failed);
        if (!alive.current) return;
        dropFiles(new Set(r.moved));
        setTrashDone({ done: sent, total: c.paths.length });
      }
    } catch (err) {
      error = String(err);
    }
    if (!alive.current) return;
    setTrashing(false);
    setTrashDone(null);
    setTrashStopping(false);
    const bytes = moved.reduce((s, p) => s + (sizes.get(p) ?? 0), 0);
    const byReason = new Map<string, string[]>();
    for (const [p, why] of failed) {
      const list = byReason.get(why);
      if (list) list.push(p);
      else byReason.set(why, [p]);
    }
    const parts = [
      moved.length
        ? `휴지통으로 ${moved.length.toLocaleString()}개 옮겼어요 · ${formatSize(bytes)}`
        : "휴지통으로 옮긴 파일이 없어요",
    ];
    const rest = c.paths.length - sent;
    if (error) parts.push(`나머지 ${(c.paths.length - moved.length - failed.length).toLocaleString()}개는 옮기지 못했어요: ${error}`);
    else if (rest > 0) parts.push(`취소해서 나머지 ${rest.toLocaleString()}개는 그대로 두었어요(아직 골라 둔 채예요)`);
    setNotice({ text: parts.join(". "), error: failed.length > 0 || error !== null, failed: [...byReason] });
  };

  const stopTrash = () => {
    trashStop.current = true;
    setTrashStopping(true);
  };

  /** `programs`: the prompt's opt-in (OPTIN_LABEL), which start_dup_link is told too. */
  const doLink = async (plan: LinkPlan, programs: boolean) => {
    setLinkConfirm(null);
    if (trashing || linkRun.current || !plan.groups.length) return;
    linkRun.current = { search: job.current, groups: plan.groups };
    linkId.current = null;
    setLinking(true);
    setLinkJob(null);
    setLinkCancelling(false);
    setNotice(null);
    try {
      await jobListening.current;
      const id = await v082.startDupLink(plan.groups, job.current, programs);
      if (!alive.current) return;
      linkId.current = id;
      // Events that came before the id did.
      const ev = linkSeen.current.get(id);
      if (ev) onJob(ev);
    } catch (err) {
      linkRun.current = null;
      if (!alive.current) return;
      setLinking(false);
      setNotice({ text: `하드 링크로 합치지 못했어요: ${err}`, error: true });
    }
  };

  const cancelLink = () => {
    const id = linkId.current;
    if (id === null || linkCancelling) return;
    setLinkCancelling(true);
    ops.cancelJob(id).catch((err) => {
      setLinkCancelling(false);
      setNotice({ text: String(err), error: true });
    });
  };

  /** The link job ended: the files it made into links leave the list, and the notice says how it went. */
  const endLink = (j: JobInfo) => {
    const run = linkRun.current;
    linkRun.current = null;
    linkId.current = null;
    setLinking(false);
    setLinkJob(null);
    setLinkCancelling(false);
    if (!run) return;
    const left = j.errors.filter(([, why]) => why.startsWith(LINK_LEFT)).map(([p]) => p);
    const failed = j.errors.filter(([, why]) => !why.startsWith(LINK_LEFT));
    const bad = new Set(failed.map(([p]) => p));
    // Which files are links now is known when every problem is listed: a copy it could not link
    // is named in `errors` (each copy of a group whose kept file could not be used, too), one
    // merged whose old copy stayed is named by that copy (`.name.ff-link`). Done, that is every
    // copy sent less those. Failed, ops.rs merged none (it fails a job only when every copy was
    // turned down), unless it stopped on a fault of its own, named at no copy: so known when
    // every copy sent is among the refusals. Cancelled, some may be links and the list stays as
    // it was: linking a file again does nothing, and sending one of two links to the trash still
    // loses nothing.
    const listed = jobErrorCount(j) <= j.errors.length;
    const refusedAll = run.groups.every((g) => g.others.every((o) => bad.has(o)));
    const known = !run.adopted && listed && (j.state === "done" || (j.state === "failed" && refusedAll));
    const merged = new Set<string>();
    if (known) {
      for (const g of run.groups) {
        if (bad.has(g.keep)) continue;
        for (const o of g.others) if (!bad.has(o)) merged.add(o);
      }
      if (merged.size && run.search === job.current) dropFiles(merged);
    }
    // What the Recycle Bin got (not the copies left in place, nor files that were one already).
    const bytes = j.freed_bytes ?? [...merged].reduce((s, p) => s + (sizes.get(p) ?? 0), 0);
    const freed = bytes > 0 ? ` · 휴지통을 비우면 ${formatSize(bytes)} 비어요` : "";
    const byReason = new Map<string, string[]>();
    for (const [p, why] of failed) {
      const list = byReason.get(why);
      if (list) list.push(p);
      else byReason.set(why, [p]);
    }
    const again = '목록을 맞추려면 "다시 찾기"를 눌러 주세요.';
    let text: string;
    if (run.adopted) {
      // Started before this opened: its files are not those of the list here (searched after it).
      const how = j.state === "cancelled" ? "를 취소했어요" : j.state === "failed" ? "가 실패했어요" : "가 끝났어요";
      text = `전에 시작한 하드 링크로 합치기${how}${freed}. 자세한 내용은 작업 목록에 있어요.`;
      if (report) text += ` ${again}`;
    } else if (j.state === "cancelled") {
      text = `하드 링크로 합치기를 취소했어요. 그때까지 합친 파일은 합친 채로 두었어요(두 위치 모두 그대로 열려요)${freed}. ${again}`;
    } else if (j.state === "failed" && known) {
      text = "하드 링크로 합친 파일이 없어요. 고른 파일을 모두 아래 이유로 합치지 못했어요(파일은 그대로예요)";
    } else if (j.state === "failed") {
      text = `하드 링크로 다 합치지 못했어요. 그때까지 합친 파일은 합친 채로 두었어요${freed}. ${again}`;
    } else if (!known) {
      text = `하드 링크로 합쳤어요${freed}. 합치지 못한 파일이 많아서 다 보여 드릴 수 없어요. ${again}`;
    } else if (merged.size) {
      text = `하드 링크로 ${merged.size.toLocaleString()}개를 합쳤어요${freed}. 다시 따로 만들려면 ${UNLINK_AT}의 "fine-file이 합친 파일"에서 풀면 돼요`;
    } else {
      text = "하드 링크로 합친 파일이 없어요";
    }
    setNotice({
      text,
      // A cancel is the user's own doing.
      error: j.state === "failed" || j.errors.length > 0,
      failed: [...byReason],
      failVerb: "합치지 못했어요",
      left,
    });
  };

  const onJob = (j: JobInfo) => {
    if (j.op !== "link") return;
    const had = linkSeen.current.get(j.id);
    if (!had || jobActive(had.state)) linkSeen.current.set(j.id, j);
    // Before the id came back, the event waits in linkSeen.
    if (!linkRun.current || linkId.current === null || j.id !== linkId.current) return;
    if (jobActive(j.state)) setLinkJob(j);
    else endLink(j);
  };
  const onJobRef = useRef(onJob);
  useEffect(() => {
    onJobRef.current = onJob;
  });

  const frac = progress && progress.total > 0 ? Math.min(1, progress.done / progress.total) : null;
  const linkFrac = !linkJob
    ? null
    : linkJob.total_bytes > 0
      ? Math.min(1, linkJob.done_bytes / linkJob.total_bytes)
      : linkJob.total_items > 0
        ? Math.min(1, linkJob.done_items / linkJob.total_items)
        : null;
  // How the search ended, once the results are in: stopped before the end, and the files it did
  // not compare (changed since the scan, unreadable, online-only), which the report leaves out.
  const partial = !!report && !!progress && progress.phase !== "done";
  // The report's own note once the search is over there too; before that (results loaded at a
  // cancel, ahead of the search's last event), the last event's.
  const skipped =
    !report || !progress || progress.phase === "failed"
      ? null
      : report.state === "done" || report.state === "cancelled"
        ? report.detail
        : progress.detail;
  const busy = trashing || linking;

  const linkBox = linking && (
    <div className="dup-notice dup-linking">
      <div className="dup-linking-head">
        <b>하드 링크로 합치는 중…</b>
        {linkJob && linkJob.total_items > 0 && (
          <span className="muted">
            {linkJob.done_items.toLocaleString()} / {linkJob.total_items.toLocaleString()}
          </span>
        )}
        <span className="spacer" />
        <button className="btn small" disabled={!linkJob || linkCancelling} onClick={cancelLink}>
          {linkCancelling ? "취소하는 중…" : "취소"}
        </button>
      </div>
      <div className={`job-bar${linkFrac === null ? " unknown" : ""}`}>
        <span style={linkFrac === null ? undefined : { width: `${linkFrac * 100}%` }} />
      </div>
      {linkJob?.current && (
        <div className="job-current" title={shown(linkJob.current)}>
          {shown(baseName(linkJob.current))}
        </div>
      )}
      <div className="muted">
        {linkRun.current?.adopted
          ? "이 창을 열기 전에 시작한 작업이에요. 끝날 때까지는 찾거나 휴지통으로 옮기거나 합칠 수 없어요."
          : "창을 닫아도 계속돼요. 진행 상황은 작업 목록에도 나와요."}
      </div>
    </div>
  );

  const trashBox = trashDone && (
    <div className="dup-notice dup-linking">
      <div className="dup-linking-head">
        <b>휴지통으로 옮기는 중…</b>
        <span className="muted">
          {trashDone.done.toLocaleString()} / {trashDone.total.toLocaleString()}
        </span>
        <span className="spacer" />
        <button className="btn small" disabled={trashStopping} onClick={stopTrash}>
          {trashStopping ? "멈추는 중…" : "취소"}
        </button>
      </div>
      <div className="job-bar">
        <span style={{ width: `${trashDone.total ? Math.min(1, trashDone.done / trashDone.total) * 100 : 0}%` }} />
      </div>
      <div className="muted">
        {TRASH_CHUNK.toLocaleString()}개씩 옮겨요. 취소하거나 창을 닫으면 지금 옮기는 {TRASH_CHUNK.toLocaleString()}개까지만
        옮기고 멈춰요.
      </div>
    </div>
  );

  const noticeBox = notice && (
    <div className={`dup-notice${notice.error ? " error" : ""}`}>
      <div>{shown(notice.text)}</div>
      {notice.failed?.map(([why, paths]) => (
        <div key={why} className="dup-notice-why">
          {paths.length.toLocaleString()}개는 {notice.failVerb ?? "옮기지 못했어요"}: {shown(why)}
        </div>
      ))}
      {notice.left && notice.left.length > 0 && (
        <div className="dup-notice-why">
          합쳤지만 남은 사본(.이름.ff-link) {notice.left.length.toLocaleString()}개를 휴지통에 넣지 못했어요. 숨김 파일로 남아
          있으니 직접 지워 주세요:
          <ul className="dup-notice-paths">
            {notice.left.slice(0, 5).map((p) => (
              <li key={p}>{shown(p)}</li>
            ))}
            {notice.left.length > 5 && (
              <li className="muted">외 {(notice.left.length - 5).toLocaleString()}개 (작업 목록에 있어요)</li>
            )}
          </ul>
        </div>
      )}
    </div>
  );

  return (
    <Dialog className="v04-dialog dups" title="중복 파일 찾기" onClose={onClose}>
      <div className="dlg-head">
        <h3>중복 파일 찾기</h3>
        <button className="dlg-close" aria-label="닫기" onClick={onClose}>
          ✕
        </button>
      </div>

      <form
        className="dup-options"
        onSubmit={(e) => {
          e.preventDefault();
          start();
        }}
      >
        <label className="field grow">
          <span>찾을 폴더</span>
          <input value={root} spellCheck={false} disabled={running} onChange={(e) => setRoot(e.target.value)} />
        </label>
        <label className="field">
          <span>최소 크기</span>
          <select value={minSize} disabled={running} onChange={(e) => setMinSize(Number(e.target.value))}>
            {MIN_SIZES.map((m) => (
              <option key={m.bytes} value={m.bytes}>
                {m.label} 이상
              </option>
            ))}
          </select>
        </label>
        {/* One button, so the focus stays on it as it turns from 찾기 to 취소 and back. */}
        <button
          type={running ? "button" : "submit"}
          className={`btn${running ? "" : " primary"}`}
          autoFocus
          title={
            linking
              ? "하드 링크로 합치는 동안에는 찾지 않아요"
              : trashing
                ? "휴지통으로 옮기는 동안에는 찾지 않아요"
                : undefined
          }
          disabled={running ? cancelling || job.current === null : !normalizePath(root) || linking || trashing}
          onClick={running ? cancel : undefined}
        >
          {running ? (cancelling ? "취소하는 중…" : "취소") : report || progress ? "다시 찾기" : "찾기"}
        </button>
      </form>

      <div className="dlg-body">
        {error && <div className="v04-error">{shown(error)}</div>}
        {/* Before any results: a merge started before this opened, and how it ended. */}
        {!report && linkBox}
        {!report && noticeBox}

        {!progress && !error && (
          <>
            <p className="muted v04-intro">
              크기가 같은 파일을 먼저 고르고, 앞부분과 전체 내용을 차례로 비교해서 내용이 똑같은 파일만 보여 줘요. 스캔한
              드라이브에서만 찾을 수 있어요.
            </p>
            <p className="muted v04-intro">
              하드 링크로 합치기는 {LINK_FITS}. {LINK_NOT}. 그런 파일은 목록에 "링크 안 됨"으로 보여요.
            </p>
          </>
        )}

        {progress && !report && (
          <div className="dup-progress">
            <div className="dup-phase">
              <b>{PHASE_LABEL[progress.phase]}</b>
              {PHASE_STEP[progress.phase] && <span className="muted">{PHASE_STEP[progress.phase]} 단계</span>}
            </div>
            {running && (
              <div className={`job-bar${frac === null ? " unknown" : ""}`}>
                <span style={frac === null ? undefined : { width: `${frac * 100}%` }} />
              </div>
            )}
            <div className="job-meta">
              {progress.total > 0 && `${progress.done.toLocaleString()} / ${progress.total.toLocaleString()} · `}
              중복 {progress.groups.toLocaleString()}묶음 · {formatSize(progress.wasted)} 낭비
            </div>
            {progress.detail && progress.phase !== "failed" && <div className="job-current">{progress.detail}</div>}
            {isOver(progress.phase) && hasResults(progress) && <div className="muted">결과를 불러오는 중…</div>}
          </div>
        )}

        {report && (
          <>
            <div className="dup-summary">
              {groups.length ? (
                <>
                  <b>중복 {groups.length.toLocaleString()}묶음</b> · 파일 {fileCount.toLocaleString()}개 · 하나씩만 남기면{" "}
                  <b>{formatSize(wasted)}</b> 비어요
                </>
              ) : (
                <b>{partial ? "멈추기 전까지 찾은 중복 파일이 없어요" : "중복 파일이 없어요"}</b>
              )}
              <span className="muted"> · 살펴본 파일 {report.scanned_files.toLocaleString()}개</span>
            </div>
            {partial && (
              <div className="dup-caveat">
                {progress?.phase === "cancelled" ? "취소해서" : "중간에 멈춰서"} 끝까지 찾지 않았어요. 그때까지 내용이 똑같다고
                확인한 묶음만 보여요. 더 있을 수 있어요.
              </div>
            )}
            {skipped && <div className="dup-caveat">{skipped}. 이 파일들은 비교하지 않아서 결과에 들어 있지 않아요.</div>}
            {binned > 0 && (
              <div className="muted dup-binned">
                휴지통 안에 있는 파일 {binned.toLocaleString()}개는 이미 지운 파일이라 결과에서 뺐어요.
              </div>
            )}

            {linkBox}
            {trashBox}
            {noticeBox}

            {groups.length > 0 && (
              <div className="dup-tools">
                <span className="muted">자동 선택 (모든 묶음)</span>
                <button className="btn" disabled={trashing} onClick={() => autoSelect("newest")}>
                  최신 하나만 남기기
                </button>
                <button className="btn" disabled={trashing} onClick={() => autoSelect("oldest")}>
                  가장 오래된 하나만 남기기
                </button>
                <button className="btn" disabled={trashing} onClick={() => autoSelect("shortest")}>
                  경로가 가장 짧은 것만 남기기
                </button>
                <button className="btn" disabled={!selected.size || trashing} onClick={clearSelection}>
                  선택 해제
                </button>
                <label className="dup-check dup-with-app" title={APP_NOTE}>
                  <input
                    type="checkbox"
                    checked={withApp}
                    disabled={trashing}
                    onChange={(e) => {
                      const on = e.target.checked;
                      setWithApp(on);
                      // A selection auto-select made follows the box at once.
                      if (autoRule) autoSelect(autoRule, on);
                    }}
                  />
                  프로그램·앱 데이터 폴더 안 파일도 자동으로 고르기 <span className="warn">(주의)</span>
                </label>
              </div>
            )}
            {autoNote && groups.length > 0 && <div className="dup-caveat">{autoNote}</div>}

            {listed.map((g) => {
              const anySel = g.files.some((f) => selected.has(f.path));
              return (
                <section key={`${g.size}|${g.hash}`} className="dup-group">
                  <header className="dup-group-head">
                    <span>
                      {formatSize(g.size)} × {g.files.length.toLocaleString()}개
                    </span>
                    <span className="muted">낭비 {formatSize(g.size * (g.files.length - 1))}</span>
                  </header>
                  {g.files.map((f) => {
                    const on = selected.has(f.path);
                    // The last copy not chosen stays: its box cannot be ticked. While the trash goes, the
                    // selection is what it sends: no box changes.
                    const lastOne = !on && g.files.every((x) => x.path === f.path || selected.has(x.path));
                    return (
                      <div key={f.path} className={`dup-file${on ? " sel" : ""}`}>
                        <label className="dup-file-pick" title={lastOne ? "묶음마다 하나는 남겨요" : shown(f.path)}>
                          <input
                            type="checkbox"
                            checked={on}
                            disabled={lastOne || trashing}
                            onChange={() => toggle(g, f.path)}
                          />
                          <span className="dup-file-name">
                            <span className="dup-file-base">{shown(baseName(f.path))}</span>
                            <span className="dup-file-dir">{shown(dirOf(f.path))}</span>
                          </span>
                        </label>
                        {appFiles.has(f.path) && (
                          <span className="v04-tag app" title={inWindows(f) ? WINDOWS_NOTE : APP_NOTE}>
                            {inWindows(f) ? "Windows 폴더" : "프로그램 폴더"}
                          </span>
                        )}
                        {/* The backend's reason it is not linked (with the opt-in: the prompt's box). */}
                        {f.link_block !== null && (
                          <span className={`v04-tag nolink${f.link_optin ? " optin" : ""}`} title={blockNote(f)}>
                            링크 안 됨
                          </span>
                        )}
                        {on ? (
                          <span className="v04-tag del">고른 파일</span>
                        ) : (
                          anySel && <span className="v04-tag keep">남김</span>
                        )}
                        <span className="dup-file-date">{formatDate(f.modified) || "날짜 모름"}</span>
                        <button className="btn small" onClick={() => onReveal(f.path)}>
                          위치 보기
                        </button>
                      </div>
                    );
                  })}
                </section>
              );
            })}
            {groups.length > limit && (
              <button className="btn dup-more" onClick={() => setLimit((l) => l + GROUP_PAGE)}>
                묶음 더 보기 ({(groups.length - limit).toLocaleString()}개 남음)
              </button>
            )}
          </>
        )}
      </div>

      <div className="dlg-foot">
        {report && groups.length > 0 && (
          <span className="muted">
            {picked.paths.length.toLocaleString()}개 선택 · {formatSize(picked.bytes)}
            {picked.groups > 0 && ` · 묶음 ${picked.groups.toLocaleString()}개`}
            {picked.unseen > 0 && ` (아래쪽 안 보이는 ${picked.unseen.toLocaleString()}묶음 포함)`}
          </span>
        )}
        <span className="spacer" />
        {report && groups.length > 0 && (
          <button
            className="btn"
            disabled={!linkAny || busy || !linkable}
            title={
              !linkable
                ? `하드 링크는 NTFS 드라이브에서만 만들 수 있어요 (이 드라이브: ${fs})`
                : !linkAny
                  ? "합칠 파일을 먼저 고르세요"
                  : `고른 파일 중 합쳐도 되는 파일만 남기는 파일의 하드 링크로 바꿔요. 두 위치 모두 그대로 쓸 수 있고 데이터는 한 번만 저장돼요. ${LINK_FITS}`
            }
            onClick={() => linkAny && setLinkConfirm({ rule: autoRule })}
          >
            {linking ? "합치는 중…" : "하드 링크로 합치기"}
          </button>
        )}
        {report && groups.length > 0 && (
          <button
            className="btn danger"
            disabled={!picked.paths.length || busy}
            onClick={() => picked.paths.length && setConfirm(picked)}
          >
            {trashing ? "옮기는 중…" : "선택 항목 휴지통으로"}
          </button>
        )}
        <button className="btn" onClick={onClose}>
          닫기
        </button>
      </div>

      {confirm && <TrashConfirm c={confirm} onCancel={() => setConfirm(null)} onConfirm={() => doTrash(confirm)} />}
      {linkConfirm && (
        <LinkConfirm
          groups={groups}
          selected={selected}
          drawn={limit}
          ask={linkConfirm}
          onCancel={() => setLinkConfirm(null)}
          onConfirm={(plan, programs) => doLink(plan, programs)}
        />
      )}
    </Dialog>
  );
}

/**
 * The trash prompt; files in Windows', a program's or app data's folder (only ever ticked by hand,
 * or by auto-select once its box is on) need one more tick before the button works.
 */
function TrashConfirm({ c, onCancel, onConfirm }: { c: Picked; onCancel(): void; onConfirm(): void }) {
  const [riskOk, setRiskOk] = useState(false);
  const app = c.app.length;
  const win = c.windows.length;
  const risky = app + win > 0;
  const which = [
    win > 0 && `Windows 폴더 안 파일 ${win.toLocaleString()}개`,
    app > 0 && `프로그램 폴더 안 파일 ${app.toLocaleString()}개`,
  ]
    .filter(Boolean)
    .join("와 ");
  return (
    <Dialog className="dup-confirm" title="휴지통으로 옮길까요?" onClose={onCancel} onBackdrop={onCancel}>
      <h3>휴지통으로 옮길까요?</h3>
      <p>
        묶음 {c.groups.toLocaleString()}개에서 파일 {c.paths.length.toLocaleString()}개 · {formatSize(c.bytes)}
      </p>
      {c.unseen > 0 && (
        <p className="muted">
          이 중 묶음 {c.unseen.toLocaleString()}개는 목록 아래쪽에 있어서 아직 보이지 않아요. "묶음 더 보기"로 확인할 수
          있어요.
        </p>
      )}
      {risky && (
        <div className="v04-error dup-confirm-app">
          {win > 0 && (
            <p>
              이 중 <b>{win.toLocaleString()}개</b>는 <b>Windows 폴더</b> 안에 있어요. 지우면 Windows가 제대로 동작하지 않거나
              켜지지 않을 수 있어요. 무엇인지 모르는 파일이면 고르지 마세요.
            </p>
          )}
          {app > 0 && (
            <>
              <p>
                이 중 <b>{app.toLocaleString()}개</b>는 프로그램·앱 데이터 폴더 안에 있어요 ({ownersText(c.app)}).
              </p>
              <p>
                지우면 그 프로그램이 실행되지 않거나, 설정·저장한 데이터·내려받은 모델 같은 파일을 다시 받아야 할 수 있어요.
                무엇인지 모르는 파일이면 고르지 마세요.
              </p>
            </>
          )}
          <label className="dup-check">
            <input type="checkbox" checked={riskOk} onChange={(e) => setRiskOk(e.target.checked)} />
            알고 있어요. {which}도 휴지통으로 옮길게요
          </label>
        </div>
      )}
      <p className="muted">
        묶음마다 적어도 하나는 남겨요. 휴지통에서 언제든 되돌릴 수 있어요. 휴지통에 넣을 수 없는 파일(USB·네트워크 드라이브,
        휴지통보다 큰 파일)은 지우지 않고 알려 드려요.
      </p>
      <div className="actions">
        <button onClick={onCancel}>취소</button>
        <button className="primary danger" autoFocus={!risky} disabled={risky && !riskOk} onClick={onConfirm}>
          휴지통으로 이동
        </button>
      </div>
    </Dialog>
  );
}

/**
 * The "하드 링크로 합치기" prompt: what it does, in plain words, what it frees, and what of the files
 * chosen it leaves out and why (the backend's rule). Its boxes add the big models and archives of
 * program folders (the opt-in, off at first) and the documents and media chosen (left out at
 * first: merged, editing one copy changes the other).
 */
function LinkConfirm({
  groups,
  selected,
  drawn,
  ask,
  onCancel,
  onConfirm,
}: {
  groups: DupGroup[];
  selected: Set<string>;
  drawn: number;
  ask: LinkAsk;
  onCancel(): void;
  onConfirm(plan: LinkPlan, programs: boolean): void;
}) {
  const [programs, setPrograms] = useState(false);
  const [edit, setEdit] = useState(false);
  const plan = useMemo(
    () => linkPlanOf(groups, selected, drawn, { rule: ask.rule, programs, editable: edit }),
    [groups, selected, drawn, ask.rule, programs, edit],
  );
  // What the opt-in would add: its box shows only when there is any.
  const withPrograms = useMemo(
    () => linkPlanOf(groups, selected, drawn, { rule: ask.rule, programs: true, editable: true }).programs,
    [groups, selected, drawn, ask.rule],
  );
  const tooMany = plan.files > LINK_MAX;
  const ok = plan.files > 0 && !tooMany;
  return (
    <Dialog className="dup-confirm" title="하드 링크로 합칠까요?" onClose={onCancel} onBackdrop={onCancel}>
      <h3>하드 링크로 합칠까요?</h3>
      {plan.files > 0 ? (
        <p>
          묶음 {plan.groups.length.toLocaleString()}개에서 파일 {plan.files.toLocaleString()}개를 남기는 파일과 합쳐요. 휴지통을
          비우면 최대 <b>{formatSize(plan.bytes)}</b> 비어요(합치지 못한 파일은 빼고).
        </p>
      ) : (
        <p>고른 파일 중 하드 링크로 합칠 수 있는 파일이 없어요.</p>
      )}
      {plan.kinds.length > 0 && (
        <p className="muted">합칠 파일: {plan.kinds.map(([k, n]) => `${k} ${n.toLocaleString()}개`).join(" · ")}</p>
      )}
      {plan.leftFiles > 0 && (
        <div className="dup-caveat dup-confirm-left">
          <p>
            고른 파일 중 <b>{plan.leftFiles.toLocaleString()}개</b>는 합치지 않아요. 그대로 두거나, 필요 없는 사본이면
            휴지통으로 보내세요.
          </p>
          <ul>
            {plan.left.slice(0, 6).map(([why, n]) => (
              <li key={why}>
                {n.toLocaleString()}개: {shown(why)}
              </li>
            ))}
            {plan.left.length > 6 && <li>그 밖의 이유로 {plan.left.slice(6).reduce((s, [, n]) => s + n, 0).toLocaleString()}개</li>}
          </ul>
        </div>
      )}
      {withPrograms > 0 && (
        <div className="v04-error dup-confirm-app">
          <p>
            고른 파일 중 <b>{withPrograms.toLocaleString()}개</b>는 프로그램·앱 데이터 폴더 안의 큰 모델·압축 파일이에요.{" "}
            {programs ? "같이 합쳐요." : "켜지 않으면 합치지 않아요."}
          </p>
          <p>
            합치면 두 프로그램이 한 파일을 같이 써요. 한 프로그램이 업데이트하면서 그 파일을 고치거나 바꾸면 다른 프로그램의
            파일도 같이 바뀌고, 그 프로그램이 파일을 검사해서 다시 받을 수도 있어요. 모델·압축 파일 말고 다른 프로그램 파일은
            켜도 합치지 않아요. 이상하면 {UNLINK_AT}로 다시 따로 만들 수 있어요.
          </p>
          <label className="dup-check">
            <input type="checkbox" checked={programs} onChange={(e) => setPrograms(e.target.checked)} />
            {OPTIN_LABEL} <span className="warn">(주의)</span>
          </label>
        </div>
      )}
      {plan.editable > 0 && (
        <div className="v04-error dup-confirm-app">
          <p>
            고른 파일 중 <b>문서·사진·영상·음악 {plan.editable.toLocaleString()}개</b>는 {edit ? "같이 합쳐요" : "빼 두었어요"}.
            합치면 한쪽을 고칠 때 다른 쪽도 같이 바뀌어요. 따로 고쳐 쓸 파일이면 휴지통으로 보내거나 그대로 두세요.
          </p>
          <label className="dup-check">
            <input type="checkbox" checked={edit} onChange={(e) => setEdit(e.target.checked)} />
            알고 있어요. 문서·사진·영상·음악 {plan.editable.toLocaleString()}개도 합칠게요
          </label>
        </div>
      )}
      <ul className="dup-confirm-list">
        <li>두 위치 모두 그대로 쓸 수 있고, 데이터는 한 번만 저장돼요.</li>
        <li>합치기 전에 두 파일의 내용을 처음부터 끝까지 다시 비교해요. 다르거나 찾은 뒤에 바뀐 파일은 건드리지 않아요.</li>
        <li>합친 파일 자리에 있던 사본은 휴지통으로 가요. 휴지통을 비우면 공간이 생겨요.</li>
        <li>같은 드라이브(NTFS)에 있고 권한·속성이 같은 파일끼리만 돼요.</li>
        <li>
          <b>합친 뒤에는 한쪽을 고치면 다른 쪽도 같이 바뀌어요.</b> {LINK_FITS}.
        </li>
        <li>{LINK_NOT}.</li>
        <li>합친 파일은 기록해 두어서, {UNLINK_AT}의 "fine-file이 합친 파일"에서 언제든 다시 따로 만들 수 있어요.</li>
      </ul>
      <p className="muted">
        묶음마다 고르지 않은 파일이 남고, 합칠 파일은 그 파일을 가리키는 하드 링크가 돼요. 합친 파일의 날짜·속성은 남기는
        파일의 것을 같이 써요.
      </p>
      {plan.unseen > 0 && (
        <p className="muted">
          이 중 묶음 {plan.unseen.toLocaleString()}개는 목록 아래쪽에 있어서 아직 보이지 않아요. "묶음 더 보기"로 확인할 수
          있어요.
        </p>
      )}
      {tooMany && (
        <p className="v04-error">
          한 번에 합칠 수 있는 파일은 {LINK_MAX.toLocaleString()}개까지예요. 고른 파일을 줄인 뒤에 다시 눌러 주세요.
        </p>
      )}
      <div className="actions">
        <button onClick={onCancel} autoFocus={!ok}>
          취소
        </button>
        <button className="primary" autoFocus={ok} disabled={!ok} onClick={() => onConfirm(plan, programs)}>
          합치기
        </button>
      </div>
    </Dialog>
  );
}
