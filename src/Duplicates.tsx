import { useEffect, useMemo, useRef, useState } from "react";
import type { UnlistenFn } from "@tauri-apps/api/event";
import {
  api,
  v04,
  v04Events,
  type Category,
  type DriveInfo,
  type DupFile,
  type DupGroup,
  type DupProgress,
  type DupReport,
} from "./api";
import { driveKey, formatDate, formatSize, normalizePath } from "./format";
import "./components.css";
import "./v04.css";

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
// (counted as "program"). Two equal DLLs in two games are both in use: auto-select leaves such
// files alone, and the trash prompt says when some were ticked by hand.
const APP_CATEGORIES = new Set<Category>(["system", "program", "game", "dev"]);

/** Whether a file is in a Windows, program, game, development-tool or app-data folder. */
const inAppFolder = (f: DupFile): boolean => APP_CATEGORIES.has(f.category);

const APP_NOTE =
  "Windows·프로그램·게임·개발 도구·앱 데이터 폴더 안의 파일이에요. 지우면 그 프로그램이 동작하지 않을 수 있어서 자동 선택에서 빠져요";

/** Groups drawn at first; more on request (a big drive can have thousands). */
const GROUP_PAGE = 100;

type Rule = "newest" | "oldest" | "shortest";

function baseName(p: string): string {
  const i = p.lastIndexOf("\\");
  return i < 0 ? p : p.slice(i + 1);
}

function dirOf(p: string): string {
  const i = p.lastIndexOf("\\");
  return i < 0 ? "" : p.slice(0, i);
}

/** The copy a rule keeps; ties go to the one listed first. */
function keeperOf(g: DupGroup, rule: Rule): string {
  let best = g.files[0];
  for (const f of g.files) {
    if (rule === "newest" ? f.modified > best.modified : rule === "oldest" ? f.modified < best.modified : f.path.length < best.path.length) {
      best = f;
    }
  }
  return best.path;
}

interface Notice {
  text: string;
  error?: boolean;
  /** Files that stayed, by reason. */
  failed?: [string, string[]][];
}

/** What the trash button would send, and what the prompt says about it. */
interface Picked {
  paths: string[];
  bytes: number;
  /** Groups it takes files from. */
  groups: number;
  /** Of `paths`, those in a program's folder (inAppFolder), all ticked by hand. */
  app: number;
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
  const [minSize, setMinSize] = useState(1 << 20);
  const [running, setRunning] = useState(false);
  const [cancelling, setCancelling] = useState(false);
  const [progress, setProgress] = useState<DupProgress | null>(null);
  const [report, setReport] = useState<DupReport | null>(null);
  /** The report's groups, less the files sent to the trash since. */
  const [groups, setGroups] = useState<DupGroup[]>([]);
  const [selected, setSelected] = useState<Set<string>>(() => new Set());
  const [limit, setLimit] = useState(GROUP_PAGE);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<Notice | null>(null);
  /** What the last auto-select left out (groups not drawn, files in a program's folder). */
  const [autoNote, setAutoNote] = useState<string | null>(null);
  const [confirm, setConfirm] = useState<Picked | null>(null);
  const [trashing, setTrashing] = useState(false);

  /** The search shown; null until its id comes back. */
  const job = useRef<number | null>(null);
  const last = useRef<DupProgress | null>(null);
  /** The end shown was assumed at a cancel; the search's own last event has not come yet. */
  const assumed = useRef(false);
  /** Files of this search sent to the trash, kept out of results loaded again. */
  const trashed = useRef(new Set<string>());
  /** Events that came before the id did (a small search can be over by then). */
  const starting = useRef(false);
  const early = useRef(new Map<number, DupProgress>());
  const listening = useRef<Promise<void>>(Promise.resolve());
  const alive = useRef(true);

  const loadResults = (id: number) => {
    v04.dupResults(id).then(
      (r) => {
        if (!alive.current || job.current !== id) return;
        setReport(r);
        setGroups(
          r.groups
            .map((g) => ({ ...g, files: g.files.filter((f) => !trashed.current.has(f.path)) }))
            .filter((g) => g.files.length >= 2),
        );
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

  const start = async () => {
    const target = normalizePath(root);
    if (!target || running) return;
    setError(null);
    setNotice(null);
    setAutoNote(null);
    setReport(null);
    setGroups([]);
    setSelected(new Set());
    setLimit(GROUP_PAGE);
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

  const toggle = (g: DupGroup, path: string) =>
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

  const shown = groups.slice(0, limit);

  /** Files in a program's folder (inAppFolder). */
  const appFiles = useMemo(() => {
    const s = new Set<string>();
    for (const g of groups) for (const f of g.files) if (inAppFolder(f)) s.add(f.path);
    return s;
  }, [groups]);

  // Only the groups drawn, so one click never sends to the trash what the user has not seen;
  // and never a file a program needs where it is.
  const autoSelect = (rule: Rule) => {
    const n = new Set<string>();
    let spared = 0;
    for (const g of shown) {
      const keep = keeperOf(g, rule);
      for (const f of g.files) {
        if (f.path === keep) continue;
        if (appFiles.has(f.path)) spared++;
        else n.add(f.path);
      }
    }
    setSelected(n);
    const notes: string[] = [];
    if (groups.length > shown.length) {
      notes.push(
        `보이는 ${shown.length.toLocaleString()}묶음에서만 골랐어요. 나머지 ${(groups.length - shown.length).toLocaleString()}묶음은 "묶음 더 보기"로 펼친 뒤에 다시 누르면 골라요.`,
      );
    }
    if (spared > 0) {
      notes.push(
        `Windows·프로그램·게임·개발 도구·앱 데이터 폴더 안의 파일 ${spared.toLocaleString()}개는 지우면 그 프로그램이 동작하지 않을 수 있어서 고르지 않았어요. 필요하면 직접 고르세요.`,
      );
    }
    setAutoNote(notes.length ? notes.join(" ") : null);
  };

  const sizes = useMemo(() => {
    const m = new Map<string, number>();
    for (const g of groups) for (const f of g.files) m.set(f.path, g.size);
    return m;
  }, [groups]);

  /** What the trash would get: never every copy of a group, whatever the selection says. */
  const picked = useMemo((): Picked => {
    const paths: string[] = [];
    let bytes = 0;
    let touched = 0;
    let app = 0;
    for (const g of groups) {
      const sel = g.files.filter((f) => selected.has(f.path));
      if (!sel.length || sel.length >= g.files.length) continue;
      touched++;
      for (const f of sel) {
        paths.push(f.path);
        if (appFiles.has(f.path)) app++;
      }
      bytes += sel.length * g.size;
    }
    return { paths, bytes, groups: touched, app };
  }, [groups, selected, appFiles]);

  const wasted = groups.reduce((s, g) => s + g.size * (g.files.length - 1), 0);
  const fileCount = groups.reduce((s, g) => s + g.files.length, 0);

  const doTrash = async (c: Picked) => {
    setConfirm(null);
    if (trashing) return;
    setTrashing(true);
    try {
      const r = await api.trash(c.paths);
      if (!alive.current) return;
      const moved = new Set(r.moved);
      for (const p of r.moved) trashed.current.add(p);
      // A group down to one file is no longer a duplicate.
      const left = groups
        .map((g) => ({ ...g, files: g.files.filter((f) => !moved.has(f.path)) }))
        .filter((g) => g.files.length >= 2);
      const still = new Set(left.flatMap((g) => g.files.map((f) => f.path)));
      setGroups(left);
      setSelected((s) => new Set([...s].filter((p) => still.has(p))));
      const bytes = r.moved.reduce((s, p) => s + (sizes.get(p) ?? 0), 0);
      const byReason = new Map<string, string[]>();
      for (const [p, why] of r.failed) {
        const list = byReason.get(why);
        if (list) list.push(p);
        else byReason.set(why, [p]);
      }
      setNotice({
        text: r.moved.length
          ? `휴지통으로 ${r.moved.length.toLocaleString()}개 옮겼어요 · ${formatSize(bytes)}`
          : "휴지통으로 옮긴 파일이 없어요",
        error: r.failed.length > 0,
        failed: [...byReason],
      });
    } catch (err) {
      if (alive.current) setNotice({ text: String(err), error: true });
    } finally {
      if (alive.current) setTrashing(false);
    }
  };

  const frac = progress && progress.total > 0 ? Math.min(1, progress.done / progress.total) : null;
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
          disabled={running ? cancelling || job.current === null : !normalizePath(root)}
          onClick={running ? cancel : undefined}
        >
          {running ? (cancelling ? "취소하는 중…" : "취소") : report || progress ? "다시 찾기" : "찾기"}
        </button>
      </form>

      <div className="dlg-body">
        {error && <div className="v04-error">{error}</div>}

        {!progress && !error && (
          <p className="muted v04-intro">
            크기가 같은 파일을 먼저 고르고, 앞부분과 전체 내용을 차례로 비교해서 내용이 똑같은 파일만 보여 줘요. 스캔한
            드라이브에서만 찾을 수 있어요.
          </p>
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

            {notice && (
              <div className={`dup-notice${notice.error ? " error" : ""}`}>
                <div>{notice.text}</div>
                {notice.failed?.map(([why, paths]) => (
                  <div key={why} className="dup-notice-why">
                    {paths.length.toLocaleString()}개는 옮기지 못했어요: {why}
                  </div>
                ))}
              </div>
            )}

            {groups.length > 0 && (
              <div className="dup-tools">
                <span className="muted">자동 선택</span>
                <button className="btn" onClick={() => autoSelect("newest")}>
                  최신 하나만 남기기
                </button>
                <button className="btn" onClick={() => autoSelect("oldest")}>
                  가장 오래된 하나만 남기기
                </button>
                <button className="btn" onClick={() => autoSelect("shortest")}>
                  경로가 가장 짧은 것만 남기기
                </button>
                <button
                  className="btn"
                  disabled={!selected.size}
                  onClick={() => {
                    setSelected(new Set());
                    setAutoNote(null);
                  }}
                >
                  선택 해제
                </button>
              </div>
            )}
            {autoNote && groups.length > 0 && <div className="dup-caveat">{autoNote}</div>}

            {shown.map((g) => {
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
                    // The last copy not chosen stays: its box cannot be ticked.
                    const lastOne = !on && g.files.every((x) => x.path === f.path || selected.has(x.path));
                    return (
                      <div key={f.path} className={`dup-file${on ? " sel" : ""}`}>
                        <label className="dup-file-pick" title={lastOne ? "묶음마다 하나는 남겨요" : f.path}>
                          <input type="checkbox" checked={on} disabled={lastOne} onChange={() => toggle(g, f.path)} />
                          <span className="dup-file-name">
                            <span className="dup-file-base">{baseName(f.path)}</span>
                            <span className="dup-file-dir">{dirOf(f.path)}</span>
                          </span>
                        </label>
                        {appFiles.has(f.path) && (
                          <span className="v04-tag app" title={APP_NOTE}>
                            프로그램 폴더
                          </span>
                        )}
                        {on ? (
                          <span className="v04-tag del">지울 파일</span>
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
          </span>
        )}
        <span className="spacer" />
        {report && groups.length > 0 && (
          <button
            className="btn danger"
            disabled={!picked.paths.length || trashing}
            onClick={() => picked.paths.length && setConfirm(picked)}
          >
            {trashing ? "옮기는 중…" : "선택 항목 휴지통으로"}
          </button>
        )}
        <button className="btn" onClick={onClose}>
          닫기
        </button>
      </div>

      {confirm && (
        <Dialog title="휴지통으로 옮길까요?" onClose={() => setConfirm(null)} onBackdrop={() => setConfirm(null)}>
          <h3>휴지통으로 옮길까요?</h3>
          <p>
            묶음 {confirm.groups.toLocaleString()}개에서 파일 {confirm.paths.length.toLocaleString()}개 ·{" "}
            {formatSize(confirm.bytes)}
          </p>
          {confirm.app > 0 && (
            <p className="v04-error">
              이 중 {confirm.app.toLocaleString()}개는 Windows·프로그램·게임·개발 도구·앱 데이터 폴더 안에 있어요. 지우면 그
              프로그램이 동작하지 않을 수 있어요.
            </p>
          )}
          <p className="muted">
            묶음마다 적어도 하나는 남겨요. 휴지통에서 언제든 되돌릴 수 있어요. 휴지통에 넣을 수 없는 파일(USB·네트워크
            드라이브, 휴지통보다 큰 파일)은 지우지 않고 알려 드려요.
          </p>
          <div className="actions">
            <button onClick={() => setConfirm(null)}>취소</button>
            <button className="primary danger" autoFocus onClick={() => doTrash(confirm)}>
              휴지통으로 이동
            </button>
          </div>
        </Dialog>
      )}
    </Dialog>
  );
}
