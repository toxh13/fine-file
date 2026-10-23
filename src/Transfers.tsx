import { memo, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { UnlistenFn } from "@tauri-apps/api/event";
import { api, ops, opEvents, type Conflict, type ConflictChoice, type JobInfo } from "./api";
import { formatDate, formatSize, parentOf, shown, typeLabel } from "./format";
import "./components.css";

type Toast = (text: string, kind?: "info" | "error") => void;

const OP_LABEL: Record<JobInfo["op"], string> = {
  copy: "복사",
  move: "이동",
  relocate: "다른 드라이브로 옮기기",
  restore: "되돌리기",
  link: "하드 링크로 합치기",
};

const STATE_LABEL: Record<JobInfo["state"], string> = {
  running: "진행 중",
  waiting: "답을 기다려요",
  done: "완료",
  failed: "실패",
  cancelled: "취소됨",
};

/** Finished jobs kept in the list; older ones drop off (jobs() keeps every job of the session). */
const KEEP_FINISHED = 10;

/**
 * A job still waiting this long with no question here lost it (asked before the page reloaded):
 * its row offers the answers instead. The question follows the "waiting" event at once.
 */
const LOST_AFTER = 1500;

const isActive = (s: JobInfo["state"]) => s === "running" || s === "waiting";

/** Items that went wrong; `errors` may hold only the newest of them while the job runs. */
const errorCount = (j: JobInfo) => Math.max(j.error_count ?? 0, j.errors.length);

/**
 * A blur that only moved the focus into the name-conflict dialog. The dialog can open at any
 * moment (a job in the background) and gives the focus back when it is answered, so a rename
 * box that loses it this way stays open instead of taking the half-typed name.
 */
export function focusTakenByConflict(ev: React.FocusEvent): boolean {
  const to = ev.relatedTarget;
  return to instanceof Element && to.closest(".modal.conflict") !== null;
}

function baseName(p: string): string {
  const t = p.replace(/[\\/]+$/, "");
  const i = t.lastIndexOf("\\");
  return i < 0 ? t : t.slice(i + 1);
}

function dirOf(p: string): string {
  const i = p.replace(/[\\/]+$/, "").lastIndexOf("\\");
  return i < 0 ? "" : p.slice(0, i);
}

/** Done fraction by bytes, by items when there are no bytes to count, null while unknown. */
function progress(j: JobInfo): number | null {
  if (j.total_bytes > 0) return Math.min(1, j.done_bytes / j.total_bytes);
  if (j.total_items > 0) return Math.min(1, j.done_items / j.total_items);
  return null;
}

function formatEta(s: number): string {
  if (s < 60) return `${Math.max(1, Math.round(s))}초`;
  if (s < 3600) return `${Math.round(s / 60)}분`;
  const h = Math.floor(s / 3600);
  const m = Math.round((s % 3600) / 60);
  return m > 0 ? `${h}시간 ${m}분` : `${h}시간`;
}

/** Seconds left at the current speed, or null when it cannot be told. */
function etaOf(j: JobInfo): number | null {
  if (j.state !== "running" || j.speed_bps <= 0 || j.total_bytes <= 0) return null;
  return Math.max(0, j.total_bytes - j.done_bytes) / j.speed_bps;
}

function upsert(list: JobInfo[], j: JobInfo): JobInfo[] {
  const i = list.findIndex((x) => x.id === j.id);
  if (i < 0) return [j, ...list].sort((a, b) => b.id - a.id);
  const next = list.slice();
  next[i] = j;
  return next;
}

// ops.rs's reasons for what a relocation or an undo could not tidy up once the folder was there.
const OLD_LEFT = "다 옮긴 뒤 남은 원래 폴더를 다 지우지 못했어요";
const NOT_RECORDED = "옮긴 기록을 저장하지 못했어요";

/**
 * A relocation or an undo that ended "done" with errors. The folder is where it was going (one
 * that does not get there undoes what it did and fails); only the steps after fell short: the
 * old folder (`<name>.ff-old`) not wholly removed, or the record not saved.
 */
const untidy = (j: JobInfo) => (j.op === "relocate" || j.op === "restore") && j.state === "done" && errorCount(j) > 0;

function untidyText(j: JobInfo): string {
  const there = j.op === "relocate";
  const parts = [there ? "다른 드라이브로 옮겼어요." : "원래 자리로 되돌렸어요."];
  const left = j.errors.find(([, why]) => why.startsWith(OLD_LEFT));
  if (left) {
    parts.push(`${there ? "남은 원래 폴더" : "옮겨 두었던 폴더"}(${baseName(left[0])})를 다 지우지 못했어요. 남은 것은 직접 지우면 돼요.`);
  }
  if (j.errors.some(([, why]) => why.startsWith(NOT_RECORDED))) {
    parts.push(there ? "옮긴 기록을 저장하지 못해서 옮긴 폴더 목록에서 되돌릴 수 없어요." : "옮긴 폴더 목록의 기록을 고치지 못했어요.");
  }
  if (parts.length === 1) parts.push("마무리하지 못한 일이 있어요.");
  parts.push("자세한 내용은 작업 목록에 있어요");
  return parts.join(" ");
}

// ops.rs's reason for a duplicate made into a hard link whose old copy (`.<name>.ff-link`, hidden)
// did not go to the Recycle Bin: the link is there, the copy is left for the user to remove.
const LINK_LEFT = "합쳤지만";

/** Old copies a hard-link job left in place (see LINK_LEFT), of those `errors` lists. */
const linkLeft = (j: JobInfo) => (j.op === "link" ? j.errors.filter(([, why]) => why.startsWith(LINK_LEFT)).length : 0);

/**
 * Copies a hard-link job merged: those it went through, less those that went wrong (a copy left
 * in place, LINK_LEFT, was merged all the same).
 */
const linkMerged = (j: JobInfo) => Math.max(0, j.done_items - (errorCount(j) - linkLeft(j)));

/** What a hard-link job frees: the old copies in the Recycle Bin (`done_bytes` is what it read). */
const linkFreed = (j: JobInfo) => `휴지통을 비우면 ${formatSize(j.freed_bytes ?? 0)} 비어요`;

/** A hard-link job's end. */
function announceLink(j: JobInfo, toast: Toast) {
  const errors = errorCount(j);
  const left = linkLeft(j);
  const freed = linkFreed(j);
  if (j.state === "done" && errors === 0) {
    toast(`하드 링크로 합쳤어요: 파일 ${linkMerged(j).toLocaleString()}개 · ${freed}`);
  } else if (j.state === "done") {
    const parts = [`하드 링크로 합쳤어요: 파일 ${linkMerged(j).toLocaleString()}개 · ${freed}.`];
    if (left > 0) parts.push(`남은 사본 ${left.toLocaleString()}개는 휴지통에 넣지 못해서 숨김 파일로 남아 있어요.`);
    if (errors > left) parts.push(`${(errors - left).toLocaleString()}개는 합치지 못했어요.`);
    parts.push("자세한 내용은 작업 목록에 있어요");
    toast(parts.join(" "), "error");
  } else if (j.state === "failed") {
    toast(`하드 링크로 합치기 실패: ${j.errors[0]?.[1] ?? "알 수 없는 오류"}`, "error");
  }
}

/** One toast when a job ends; a cancel is the user's own doing and needs none. */
function announce(j: JobInfo, toast: Toast) {
  if (j.op === "link") return announceLink(j, toast);
  const op = OP_LABEL[j.op];
  const errors = errorCount(j);
  if (j.state === "done" && errors === 0) {
    toast(`${op} 완료: ${j.done_items.toLocaleString()}개 항목 · ${formatSize(j.done_bytes)}`);
  } else if (untidy(j)) {
    toast(untidyText(j), "error");
  } else if (j.state === "done") {
    toast(`${op} 끝: ${errors.toLocaleString()}개 항목은 ${op}하지 못했어요. 이유는 작업 목록에 있어요`, "error");
  } else if (j.state === "failed") {
    toast(`${op} 실패: ${j.errors[0]?.[1] ?? "알 수 없는 오류"}`, "error");
  }
}

export function Transfers({ onToast }: { onToast: Toast }) {
  /** Newest first. */
  const [jobs, setJobs] = useState<JobInfo[]>([]);
  /** Waiting for an answer, oldest first; the first one is asked. */
  const [conflicts, setConflicts] = useState<Conflict[]>([]);
  const [open, setOpen] = useState(true);
  const [dismissed, setDismissed] = useState<Set<number>>(() => new Set());
  const [errorsOpen, setErrorsOpen] = useState<Set<number>>(() => new Set());
  const [cancelling, setCancelling] = useState<Set<number>>(() => new Set());
  const toast = useRef(onToast);
  useEffect(() => {
    toast.current = onToast;
  });
  /** The last state seen per job, so a job's end is announced once. */
  const seen = useRef(new Map<number, JobInfo["state"]>());

  useEffect(() => {
    let dead = false;
    const offs: UnlistenFn[] = [];
    const keep = (p: Promise<UnlistenFn>) =>
      p.then((off) => (dead ? off() : void offs.push(off))).catch(() => {});
    const enqueue = (c: Conflict) => {
      // A job known to be over asks nothing (a question from before the reload, answered meanwhile).
      const state = seen.current.get(c.job);
      if (dead || (state !== undefined && !isActive(state))) return;
      setConflicts((q) =>
        q.some((x) => x.job === c.job && x.source === c.source && x.target === c.target) ? q : [...q, c],
      );
    };

    keep(
      opEvents.jobProgress((j) => {
        if (dead) return;
        const before = seen.current.get(j.id);
        seen.current.set(j.id, j.state);
        // A new job opens the panel again.
        if (before === undefined) setOpen(true);
        if (!isActive(j.state) && (before === undefined || isActive(before))) announce(j, toast.current);
        setJobs((list) => upsert(list, j));
        // An ended job asks nothing more.
        if (!isActive(j.state)) setConflicts((q) => q.filter((c) => c.job !== j.id));
      }),
    );
    keep(opEvents.jobConflict(enqueue));

    // Jobs started before this mounted. An event that arrived first is newer, so it stays.
    ops
      .jobs()
      .then((list) => {
        if (dead) return;
        for (const j of list) if (!seen.current.has(j.id)) seen.current.set(j.id, j.state);
        setJobs((cur) => {
          const known = new Set(cur.map((j) => j.id));
          return [...cur, ...list.filter((j) => !known.has(j.id))].sort((a, b) => b.id - a.id);
        });
      })
      .catch(() => {})
      // Then the questions asked before this mounted, whose events are gone (after the jobs, so
      // one of a job that is over is left out). A backend without the command has none to give:
      // such a job's row offers the answers instead (`lost` below).
      .then(() => ops.pendingConflicts())
      .then((list) => list.forEach(enqueue))
      .catch(() => {});

    return () => {
      dead = true;
      offs.forEach((off) => off());
    };
  }, []);

  const shown = useMemo(() => {
    let finished = 0;
    return jobs.filter((j) => !dismissed.has(j.id) && (isActive(j.state) || finished++ < KEEP_FINISHED));
  }, [jobs, dismissed]);
  const active = shown.filter((j) => isActive(j.state));

  // Toasts sit in the same corner: lift them above the panel while it is there.
  const panel = useRef<HTMLDivElement>(null);
  const hasPanel = shown.length > 0;
  useLayoutEffect(() => {
    const el = panel.current;
    const root = document.documentElement.style;
    if (!el) return;
    const ro = new ResizeObserver(() => root.setProperty("--transfers-h", `${el.offsetHeight + 8}px`));
    ro.observe(el, { box: "border-box" });
    return () => {
      ro.disconnect();
      root.removeProperty("--transfers-h");
    };
  }, [hasPanel]);

  const toggle = (set: React.Dispatch<React.SetStateAction<Set<number>>>, id: number, on?: boolean) =>
    set((s) => {
      const n = new Set(s);
      if (on ?? !n.has(id)) n.add(id);
      else n.delete(id);
      return n;
    });

  const cancel = (id: number) => {
    toggle(setCancelling, id, true);
    ops.cancelJob(id).catch((err) => {
      toggle(setCancelling, id, false);
      toast.current(String(err), "error");
    });
  };

  const answer = (c: Conflict, choice: ConflictChoice, applyAll: boolean) => {
    // With "apply to the rest" or a cancel, the job asks nothing more.
    setConflicts((q) => q.filter((x) => x !== c && !(x.job === c.job && (applyAll || choice === "cancel"))));
    ops.resolveConflict(c.job, choice, applyAll).catch((err) => toast.current(String(err), "error"));
  };

  const clearFinished = () =>
    setDismissed((s) => {
      const n = new Set(s);
      for (const j of shown) if (!isActive(j.state)) n.add(j.id);
      return n;
    });

  // The whole of what is still running, for the header.
  const totalBytes = active.reduce((s, j) => s + j.total_bytes, 0);
  const doneBytes = active.reduce((s, j) => s + j.done_bytes, 0);
  const speed = active.reduce((s, j) => s + (j.state === "running" ? j.speed_bps : 0), 0);
  const overall = totalBytes > 0 ? doneBytes / totalBytes : null;
  const overallEta = speed > 0 && totalBytes > 0 ? (totalBytes - doneBytes) / speed : null;
  const title =
    active.length === 0
      ? "작업 끝"
      : active.length === 1
        ? `${OP_LABEL[active[0].op]} 중`
        : `작업 ${active.length.toLocaleString()}개 진행 중`;

  const ask = conflicts[0];

  // Waiting jobs with no question here, and those still so after LOST_AFTER.
  const unasked = jobs
    .filter((j) => j.state === "waiting" && !conflicts.some((c) => c.job === j.id))
    .map((j) => j.id)
    .join(" ");
  const [lost, setLost] = useState("");
  useEffect(() => {
    const t = setTimeout(() => setLost(unasked), unasked ? LOST_AFTER : 0);
    return () => clearTimeout(t);
  }, [unasked]);
  const lostIds = useMemo(() => {
    const now = new Set(unasked.split(" "));
    return new Set(lost.split(" ").filter((id) => id && now.has(id)).map(Number));
  }, [lost, unasked]);

  // The dialog takes the focus while it asks (ConflictDialog), and gives it back when the last
  // question is answered: to a rename box being typed in, say, which kept its text meanwhile
  // (focusTakenByConflict). A layout effect, so it sees the focus before the dialog moves it.
  const asking = ask !== undefined;
  useLayoutEffect(() => {
    if (!asking) return;
    const before = document.activeElement;
    return () => {
      if (before instanceof HTMLElement && before !== document.body && before.isConnected) {
        before.focus({ preventScroll: true });
      }
    };
  }, [asking]);

  return (
    <>
      {hasPanel && (
        <div ref={panel} className={`transfers ${open ? "open" : ""}`}>
          <div className="transfers-head">
            <button className="transfers-title" onClick={() => setOpen((o) => !o)} title={open ? "접기" : "펼치기"}>
              <span className="twisty">{open ? "▾" : "▸"}</span>
              {title}
              {active.length > 0 && overall !== null && (
                <span className="muted">
                  {" "}
                  {Math.floor(overall * 100)}%{overallEta !== null && ` · 약 ${formatEta(overallEta)} 남음`}
                </span>
              )}
            </button>
            {shown.length > active.length && (
              <button className="transfers-btn" onClick={clearFinished}>
                끝난 작업 지우기
              </button>
            )}
          </div>
          {!open && active.length > 0 && <Bar value={overall} state="running" />}
          {open && (
            <div className="transfers-list">
              {shown.map((j) => (
                <JobRow
                  key={j.id}
                  j={j}
                  cancelling={cancelling.has(j.id)}
                  errorsOpen={errorsOpen.has(j.id)}
                  lost={lostIds.has(j.id)}
                  onLostAnswer={(choice) =>
                    ops.resolveConflict(j.id, choice, false).catch((err) => toast.current(String(err), "error"))
                  }
                  onCancel={() => cancel(j.id)}
                  onDismiss={() => toggle(setDismissed, j.id, true)}
                  onToggleErrors={() => toggle(setErrorsOpen, j.id)}
                />
              ))}
            </div>
          )}
        </div>
      )}
      {ask && (
        <ConflictDialog
          key={`${ask.job}|${ask.source}`}
          c={ask}
          op={jobs.find((j) => j.id === ask.job)?.op}
          more={conflicts.length - 1}
          onAnswer={(choice, all) => answer(ask, choice, all)}
        />
      )}
    </>
  );
}

function Bar({ value, state }: { value: number | null; state: JobInfo["state"] }) {
  return (
    <div className={`job-bar ${state} ${value === null ? "unknown" : ""}`}>
      <span style={value === null ? undefined : { width: `${value * 100}%` }} />
    </div>
  );
}

interface JobRowProps {
  j: JobInfo;
  cancelling: boolean;
  errorsOpen: boolean;
  /** Waiting on a question this page never got (see LOST_AFTER). */
  lost: boolean;
  onLostAnswer: (choice: "skip" | "rename") => void;
  onCancel: () => void;
  onDismiss: () => void;
  onToggleErrors: () => void;
}

function JobRow({ j, cancelling, errorsOpen, lost, onLostAnswer, onCancel, onDismiss, onToggleErrors }: JobRowProps) {
  // The progress it was answered at: the next event (the job going on) lets it answer again.
  const [answered, setAnswered] = useState<JobInfo | null>(null);
  const errors = errorCount(j);
  const active = isActive(j.state);
  const items = `${j.done_items.toLocaleString()} / ${j.total_items.toLocaleString()} 항목`;
  let meta: string;
  if (active) {
    const eta = etaOf(j);
    meta = [
      items,
      j.total_bytes > 0 ? `${formatSize(j.done_bytes)} / ${formatSize(j.total_bytes)}` : null,
      j.state === "running" && j.speed_bps > 0 ? `${formatSize(j.speed_bps)}/s` : null,
      eta !== null ? `약 ${formatEta(eta)} 남음` : null,
    ]
      .filter(Boolean)
      .join(" · ");
  } else if (j.state === "done" && j.op === "link") {
    meta = `파일 ${linkMerged(j).toLocaleString()}개 합침 · ${linkFreed(j)}`;
  } else if (j.state === "done") {
    meta = `${j.done_items.toLocaleString()}개 항목 · ${formatSize(j.done_bytes)}`;
  } else if (j.op === "link" && (j.freed_bytes ?? 0) > 0) {
    // Cancelled (or stopped) part way: the copies merged until then are in the Recycle Bin already.
    meta = `${items}에서 멈췄어요 · ${linkFreed(j)}`;
  } else {
    meta = `${items}에서 멈췄어요`;
  }
  return (
    <div className={`job ${j.state}`}>
      <div className="job-top">
        <span className="job-title" title={j.dest ? shown(j.dest) : undefined}>
          {/* A hard-link job has no destination; its folder, when it names one, is where it works. */}
          {j.op === "link"
            ? j.dest
              ? `${OP_LABEL[j.op]} · ${shown(baseName(j.dest) || j.dest)}`
              : OP_LABEL[j.op]
            : `${OP_LABEL[j.op]} → ${shown(baseName(j.dest) || j.dest)}`}
        </span>
        <span className="job-state">
          {untidy(j) || (j.state === "done" && errors > 0 && linkLeft(j) === errors)
            ? "완료 · 확인 필요"
            : j.state === "done" && errors > 0
              ? "일부 실패"
              : STATE_LABEL[j.state]}
        </span>
        {active ? (
          <button className="transfers-btn" disabled={cancelling} onClick={onCancel}>
            {cancelling ? "취소 중…" : "취소"}
          </button>
        ) : (
          <button className="transfers-btn" title="목록에서 지우기" onClick={onDismiss}>
            ✕
          </button>
        )}
      </div>
      {active && (
        <div className="job-current" title={j.current ? shown(j.current) : undefined}>
          {j.state === "waiting" && lost
            ? `${shown(baseName(j.current ?? "")) || "항목"}: 이름이 같은 항목이 있는데, 묻는 창을 다시 띄울 수 없어요`
            : j.state === "waiting"
              ? "이름이 같은 항목이 있어서 답을 기다리고 있어요"
              : j.current
                ? shown(baseName(j.current))
                : "준비 중…"}
        </div>
      )}
      {j.state === "waiting" && lost && (
        // No overwrite here: without the question there is nothing to tell what would go to the
        // Recycle Bin. Both answers leave everything that is there as it is.
        <div className="job-lost">
          {(["skip", "rename"] as const).map((choice) => (
            <button
              key={choice}
              className="transfers-btn"
              disabled={answered === j}
              onClick={() => {
                setAnswered(j);
                onLostAnswer(choice);
              }}
            >
              {choice === "skip" ? "건너뛰기" : "이름 바꿔서 둘 다 남기기"}
            </button>
          ))}
          <span className="muted">덮어쓰려면 작업을 취소하고 다시 붙여넣으세요</span>
        </div>
      )}
      {active && <Bar value={progress(j)} state={j.state} />}
      <div className="job-meta">{meta}</div>
      {errors > 0 && (
        <>
          <button className="job-errors-toggle" onClick={onToggleErrors}>
            {errorsOpen ? "▾" : "▸"} 문제 {errors.toLocaleString()}개
          </button>
          {errorsOpen && <JobErrors errors={j.errors} hidden={errors - j.errors.length} active={active} />}
        </>
      )}
    </div>
  );
}

interface JobErrorsProps {
  errors: [string, string][];
  /** Problems not in `errors` (yet). */
  hidden: number;
  active: boolean;
}

const sameError = (a: [string, string] | undefined, b: [string, string] | undefined) =>
  a === b || (a !== undefined && b !== undefined && a[0] === b[0] && a[1] === b[1]);

/**
 * The problems of one job. Every progress event brings the whole list again (up to a thousand),
 * and the list only grows at its end: drawn again only when its length or last item changes.
 */
const JobErrors = memo(
  function JobErrors({ errors, hidden, active }: JobErrorsProps) {
    return (
      <ul className="job-errors">
        {errors.map(([path, why], i) => (
          <li key={i} title={shown(path)}>
            <span className="job-error-name">{shown(baseName(path) || path)}</span>
            <span className="muted">{shown(why)}</span>
          </li>
        ))}
        {hidden > 0 && (
          <li className="muted">
            {active
              ? `그 밖의 ${hidden.toLocaleString()}개는 작업이 끝나면 보여요`
              : `그 밖에 ${hidden.toLocaleString()}개 항목도 처리하지 못했어요`}
          </li>
        )}
      </ul>
    );
  },
  (a, b) =>
    a.hidden === b.hidden &&
    a.active === b.active &&
    a.errors.length === b.errors.length &&
    sameError(a.errors[a.errors.length - 1], b.errors[b.errors.length - 1]),
);

interface ConflictProps {
  c: Conflict;
  /** The job's op, once its first progress event is in. */
  op: JobInfo["op"] | undefined;
  /** Other conflicts queued behind this one. */
  more: number;
  onAnswer: (choice: ConflictChoice, applyAll: boolean) => void;
}

/** One side of a conflict as its folder lists it: the event tells only the source's kind. */
interface Found {
  isDir: boolean;
  /** A junction or symbolic link, or a cloud folder: the listing does not tell which. */
  reparse: boolean;
  /** A folder's size from the index; null when it is not known. */
  size: number | null;
}

async function lookUp(path: string): Promise<Found | null> {
  const dir = parentOf(path);
  if (!dir) return null;
  const listing = await api.list(dir);
  const name = baseName(path).toLowerCase();
  const e = listing.entries.find((x) => x.name.toLowerCase() === name);
  if (!e) return null;
  return { isDir: e.is_dir, reparse: e.reparse, size: e.is_dir && listing.indexed ? e.size : null };
}

const KIND_DIFFERS = "종류가 달라서 덮어쓰지 않아요";

/** One side's kind as far as it is known; `link` is null while (or when) it cannot be told. */
interface Kind {
  dir: boolean | null;
  link: boolean | null;
}

/**
 * Whether "overwrite" can be done to this pair: ops.rs overwrites only items of the same kind
 * (file, folder, link), and refuses the rest (KIND_DIFFERS) even when the answer was given for
 * all. False when the kinds differ, null while that is not known.
 */
function sameKind(src: Kind, tgt: Kind): boolean | null {
  if (src.dir === null || tgt.dir === null) return null;
  if (src.dir !== tgt.dir) return false;
  if (src.link === null || tgt.link === null) return null;
  return src.link === tgt.link;
}

/**
 * What "overwrite" does to this pair (ops.rs `transfer`), by the backend's rule: same kind only.
 * A file over a file sends the existing one to the Recycle Bin, a folder onto a folder merges the
 * contents, a link over a link sends only the existing link to the bin. Different kinds (a file
 * and a folder, a link and a real file or folder) are never overwritten.
 */
function overwriteHint(src: Kind, tgt: Kind, all: boolean): string {
  const same = sameKind(src, tgt);
  if (same === false) {
    const what = src.dir !== tgt.dir ? "파일과 폴더" : src.dir ? "링크와 실제 폴더" : "링크와 실제 파일";
    return `${KIND_DIFFERS} (${what})`;
  }
  if (src.dir === null || tgt.dir === null) return "같은 종류일 때만 덮어써요: 파일은 기존 파일을 휴지통으로, 폴더는 합쳐요";
  // Both of a kind, but whether they are links is not known (or known of one side only).
  if (same === null) {
    if (src.link || tgt.link) return "둘 다 링크면 기존 링크만 휴지통으로, 아니면 종류가 달라서 덮어쓰지 않아요";
    return src.dir
      ? "폴더를 합쳐요. 한쪽만 링크·정션이면 종류가 달라서 덮어쓰지 않아요"
      : "기존 파일은 휴지통으로. 한쪽만 링크면 종류가 달라서 덮어쓰지 않아요";
  }
  if (src.link) return "기존 링크만 휴지통으로 (링크가 가리키는 곳은 그대로)";
  if (!src.dir) return "기존 파일은 휴지통으로";
  // With "apply to the rest" the names inside get the same answer, but only those of the same kind.
  return all
    ? "폴더를 합쳐요. 안의 같은 이름 항목도 종류가 같으면 묻지 않고 덮어쓰고, 다르면 다시 물어요"
    : "폴더를 합쳐요. 안의 같은 이름 항목은 하나씩 물어요";
}

/** A side's link-ness: the event's word, else the listing's, which cannot tell a link from a cloud item. */
function linkOf(sent: boolean | undefined, listed: Found | null | undefined): boolean | null {
  if (sent !== undefined) return sent;
  return listed && !listed.reparse ? false : null;
}

function ConflictDialog({ c, op, more, onAnswer }: ConflictProps) {
  const [all, setAll] = useState(false);
  /** Both sides as listed: undefined while looking, null for a side that was not found. */
  const [found, setFound] = useState<{ src: Found | null; tgt: Found | null }>();
  const box = useRef<HTMLDivElement>(null);
  const first = useRef<HTMLButtonElement>(null);

  // What "overwrite" does depends on both kinds. The event tells them (an older backend only the
  // source's folder-ness); the listing gives folder sizes and stands in for what the event leaves
  // out. It is the index's, a moment behind the disk at worst; the event's own word wins.
  useEffect(() => {
    let stale = false;
    const look = (p: string) => lookUp(p).catch(() => null);
    Promise.all([look(c.source), look(c.target)]).then(([src, tgt]) => {
      if (!stale) setFound({ src, tgt });
    });
    return () => {
      stale = true;
    };
  }, []);

  // The list's shortcuts listen on window, and the job waits on this answer: Delete, Backspace,
  // F2 or the arrows must not act on the list behind. Enter and Space still work here, as they
  // are the browser's default actions, which stopping propagation leaves alone. Tab goes round
  // the dialog's own controls, and the focus is not let out to the window behind (a rename box
  // that opens there, say), where Enter or Space would act. Focused here, not with autoFocus:
  // Transfers notes where the focus was first, to give it back.
  useEffect(() => {
    first.current?.focus();
    // A disabled control (overwrite, for items of different kinds) cannot take the focus.
    const controls = () =>
      Array.from(box.current?.querySelectorAll<HTMLElement>("button:not(:disabled), input:not(:disabled)") ?? []);
    const block = (ev: KeyboardEvent) => {
      ev.stopPropagation();
      if (ev.key === "F5") ev.preventDefault();
      if (ev.key !== "Tab") return;
      const list = controls();
      if (!list.length) return;
      ev.preventDefault();
      const at = list.indexOf(document.activeElement as HTMLElement);
      const next = at < 0 ? (ev.shiftKey ? list.length - 1 : 0) : (at + (ev.shiftKey ? -1 : 1) + list.length) % list.length;
      list[next].focus();
    };
    const keepIn = (ev: FocusEvent) => {
      if (ev.target instanceof Node && box.current && !box.current.contains(ev.target)) first.current?.focus();
    };
    window.addEventListener("keydown", block, true);
    document.addEventListener("focusin", keepIn, true);
    return () => {
      window.removeEventListener("keydown", block, true);
      document.removeEventListener("focusin", keepIn, true);
    };
  }, []);

  const srcDir = c.is_dir;
  // Sizes count files only, so one above zero is a file's; otherwise the listing tells.
  const tgtDir = c.target_is_dir ?? (c.target_size > 0 ? false : found?.tgt ? found.tgt.isDir : null);
  const src: Kind = { dir: srcDir, link: linkOf(c.is_link, found?.src) };
  const tgt: Kind = { dir: tgtDir, link: linkOf(c.target_is_link, found?.tgt) };
  // The backend refuses to overwrite items of different kinds: not offered.
  const differs = sameKind(src, tgt) === false;
  const newer = c.source_modified > c.target_modified ? "source" : c.source_modified < c.target_modified ? "target" : null;
  const bigger =
    srcDir || tgtDir !== false || c.source_size === c.target_size ? null : c.source_size > c.target_size ? "source" : "target";
  const incoming = op === "move" ? "옮길 항목" : op === "copy" ? "복사할 항목" : "새 항목";
  const verb = op === "move" ? "옮기지" : op === "copy" ? "복사하지" : null;
  const skipHint = verb ? `${srcDir ? "이 폴더는 통째로 " : ""}${verb} 않고 넘어가요` : "이 항목은 그대로 두고 넘어가요";

  return (
    <div className="modal-bg">
      <div ref={box} className="modal conflict" role="dialog" aria-modal="true" aria-labelledby="conflict-title">
        <h3 id="conflict-title">같은 이름의 {tgtDir === true ? "폴더가" : tgtDir === false ? "파일이" : "항목이"} 있어요</h3>
        <p className="conflict-name" title={shown(c.target)}>
          {shown(baseName(c.target))}
        </p>
        <div className="conflict-cmp">
          <Side
            label={incoming}
            path={c.source}
            isDir={srcDir}
            link={src.link}
            differs={differs}
            looking={false}
            size={srcDir ? (found?.src?.size ?? null) : c.source_size}
            modified={c.source_modified}
            newer={newer === "source"}
            bigger={bigger === "source"}
          />
          <Side
            label="이미 있는 항목"
            path={c.target}
            isDir={tgtDir}
            link={tgt.link}
            differs={differs}
            looking={found === undefined}
            size={tgtDir === false ? c.target_size : tgtDir ? (found?.tgt?.size ?? null) : null}
            modified={c.target_modified}
            newer={newer === "target"}
            bigger={bigger === "target"}
          />
        </div>
        <div className="conflict-choices">
          <button ref={first} onClick={() => onAnswer("skip", all)}>
            <b>건너뛰기</b>
            <span className="muted">{skipHint}</span>
          </button>
          <button disabled={differs} onClick={() => onAnswer("overwrite", all)}>
            <b>덮어쓰기</b>
            <span className="muted">{overwriteHint(src, tgt, all)}</span>
          </button>
          <button onClick={() => onAnswer("rename", all)}>
            <b>이름 바꾸기</b>
            <span className="muted">둘 다 남기고, 새로 들어가는 쪽 이름을 바꿔요</span>
          </button>
        </div>
        <div className="conflict-foot">
          <label title="이 작업에서 이름이 같은 항목이 또 나오면 묻지 않고 같은 답을 써요. 덮어쓰기는 같은 종류끼리만 해요: 파일은 기존 파일을 휴지통으로 보내고, 폴더는 합쳐요. 종류(파일, 폴더, 링크)가 다른 항목은 휴지통으로 보내지 않고 다시 물어요">
            <input type="checkbox" checked={all} onChange={(e) => setAll(e.target.checked)} />
            나머지에도 적용
          </label>
          {more > 0 && <span className="muted">다른 작업에서 {more.toLocaleString()}개 더</span>}
          <span className="spacer" />
          <button className="conflict-cancel" onClick={() => onAnswer("cancel", false)}>
            작업 취소
          </button>
        </div>
      </div>
    </div>
  );
}

interface SideProps {
  label: string;
  path: string;
  /** Null while (or when) it is not known. */
  isDir: boolean | null;
  /** A junction or symbolic link; null when not known. */
  link: boolean | null;
  /** The two sides are known to be of different kinds (so overwrite is not offered). */
  differs: boolean;
  looking: boolean;
  /** Null when not known (a folder outside the index). */
  size: number | null;
  modified: number;
  newer: boolean;
  bigger: boolean;
}

function Side({ label, path, isDir, link, differs, looking, size, modified, newer, bigger }: SideProps) {
  const folder = dirOf(path);
  return (
    <div className="conflict-side">
      <div className="conflict-label">{label}</div>
      <div className="conflict-row">
        {isDir === null ? (looking ? "확인하는 중…" : "종류 모름") : shown(typeLabel(baseName(path), isDir))}
        {link && <span className="conflict-tag">링크</span>}
        {differs && <span className="conflict-tag">종류 다름</span>}
      </div>
      <div className="conflict-row">
        {size === null ? "크기 모름" : formatSize(size)}
        {bigger && <span className="conflict-tag">더 큼</span>}
      </div>
      <div className="conflict-row">
        {formatDate(modified) || "날짜 모름"}
        {newer && <span className="conflict-tag">더 최근</span>}
      </div>
      <div className="conflict-folder" title={shown(folder)}>
        {shown(folder)}
      </div>
    </div>
  );
}
