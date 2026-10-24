import { useEffect, useMemo, useRef, useState } from "react";
import type { UnlistenFn } from "@tauri-apps/api/event";
import { links, opEvents, ops, UNLINK_MAX, UNLINK_SPARE, type DriveInfo, type HardlinkGroup, type HardlinkScan, type JobInfo, type LinkRecord } from "./api";
import { Dialog } from "./Duplicates";
import { driveKey, formatDate, formatSize, normalizePath, pathKey, shown } from "./format";
import "./components.css";
import "./v04.css";
import "./v07.css";
import "./v0103.css";

// ---- v0.10.3: hard links found and undone ("정리 ▾ → 하드 링크 찾기·풀기…") ---------------
//
// A hard link is one file under several names: what one name changes, the others see. 0.8.2's
// "하드 링크로 합치기" linked equal files across apps, games and backups (a modpack with its own
// backups, a template with the copy in a save), and a launcher that rewrites, checks or repairs its
// files then broke. This finds such files on disk (any, not only fine-file's: merges before 0.10.3
// left no record) and makes the names chosen files of their own again (links.unlink): each gets a
// copy of the data, the other names keep the original. Nothing is deleted.

type Tab = "find" | "ledger";

/** Groups (and ledger rows) drawn at first; more on request. */
const PAGE = 100;

const PROTECTED_NOTE = "Windows가 쓰는 링크라 건드리지 않아요";
const KEEPS_NOTE = "이 이름은 원래 파일로 남아요";
const OUTSIDE_NOTE = "찾은 폴더 밖에 있는 이름이에요. 처음에는 고르지 않아서 원래 파일로 남아요";

function baseName(p: string): string {
  const i = p.lastIndexOf("\\");
  return i < 0 ? p : p.slice(i + 1);
}

function dirOf(p: string): string {
  const i = p.lastIndexOf("\\");
  return i < 0 ? "" : p.slice(0, i);
}

/** Items of a job that went wrong; `errors` may hold only some of them (JobInfo.error_count). */
const jobErrorCount = (j: JobInfo) => Math.max(j.error_count ?? 0, j.errors.length);

const jobActive = (s: JobInfo["state"]) => s === "running" || s === "waiting";

/**
 * The names of a group ticked at first: those inside the folder, while one name keeps the original.
 * A name outside the folder does (it is not ticked); with every name inside, the one fine-file kept
 * when it merged them (`kept`, pathKeys), else the first.
 */
function defaultPicks(g: HardlinkGroup, kept: Set<string>): string[] {
  if (g.protected) return [];
  const inside = g.paths.filter((_, i) => g.inside[i] === true);
  if (inside.length < g.paths.length) return inside;
  const stay = inside.find((p) => kept.has(pathKey(p))) ?? inside[0];
  return inside.filter((p) => p !== stay);
}

/** What "독립된 파일로 만들기" (or a ledger row's "링크 풀기") would send, for its prompt. */
interface UnlinkAsk {
  paths: string[];
  /** Files they are names of. */
  files: number;
  /** The copies' size: room they take. */
  bytes: number;
  /** Room needed per drive ("C:"). */
  need: [string, number][];
}

/** An unlink job this window shows. */
interface UnlinkRun {
  paths: string[];
  /** Started before this window opened: only its progress is shown. */
  adopted?: boolean;
}

interface Notice {
  text: string;
  error?: boolean;
  /** Names that stayed links, by reason. */
  failed?: [string, string[]][];
}

export function Hardlinks({
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
  const [tab, setTab] = useState<Tab>("find");
  const [root, setRoot] = useState(scope);
  const [finding, setFinding] = useState(false);
  const [scan, setScan] = useState<HardlinkScan | null>(null);
  /** Names to make files of their own. */
  const [picked, setPicked] = useState<Set<string>>(() => new Set());
  const [limit, setLimit] = useState(PAGE);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<Notice | null>(null);
  const [ledger, setLedger] = useState<LinkRecord[] | null>(null);
  const [ledgerError, setLedgerError] = useState<string | null>(null);
  const [ledgerLimit, setLedgerLimit] = useState(PAGE);
  const [ask, setAsk] = useState<UnlinkAsk | null>(null);
  /** An unlink job runs; its last progress event once one came. */
  const [unlinking, setUnlinking] = useState(false);
  const [unlinkJob, setUnlinkJob] = useState<JobInfo | null>(null);
  const [cancelling, setCancelling] = useState(false);

  const alive = useRef(true);
  /** The find whose answer is shown: an older one answering late is dropped. */
  const findSeq = useRef(0);
  /** The unlink job: what it was sent, and its id once that comes back (events before it wait in `seen`). */
  const run = useRef<UnlinkRun | null>(null);
  const runId = useRef<number | null>(null);
  /** The newest event of each unlink job since this opened, its end over anything else. */
  const seen = useRef(new Map<number, JobInfo>());
  const listening = useRef<Promise<void>>(Promise.resolve());

  /** The names fine-file kept when it merged (still linked): with every name inside, that one stays. */
  const kept = useMemo(
    () => new Set((ledger ?? []).filter((r) => r.still_linked).map((r) => pathKey(r.kept))),
    [ledger],
  );
  const keptRef = useRef(kept);
  useEffect(() => {
    keptRef.current = kept;
  });

  const loadLedger = () => {
    links.ledger().then(
      (l) => {
        if (!alive.current) return;
        setLedger([...l].sort((a, b) => b.at - a.at));
        setLedgerError(null);
      },
      (err) => alive.current && setLedgerError(String(err)),
    );
  };

  const find = async (target: string) => {
    if (!target) return;
    const n = ++findSeq.current;
    setFinding(true);
    setError(null);
    try {
      const r = await links.find(target);
      if (!alive.current || n !== findSeq.current) return;
      setScan(r);
      setPicked(new Set(r.groups.flatMap((g) => defaultPicks(g, keptRef.current))));
      setLimit(PAGE);
    } catch (err) {
      if (alive.current && n === findSeq.current) setError(String(err));
    } finally {
      if (alive.current && n === findSeq.current) setFinding(false);
    }
  };

  useEffect(() => {
    alive.current = true;
    loadLedger();
    let dead = false;
    let off: UnlistenFn | null = null;
    listening.current = opEvents
      .jobProgress((j) => {
        if (!dead) onJobRef.current(j);
      })
      .then(
        (f) => (dead ? f() : void (off = f)),
        () => {},
      );
    // An undo started before this opened still runs: it is shown as this window's own, so nothing
    // here starts another over the same files until it ends. Asked once the events are heard.
    listening.current
      .then(() => ops.jobs())
      .then((list) => {
        if (dead || run.current) return;
        const j = list.filter((x) => x.op === "unlink" && jobActive(x.state)).sort((a, b) => b.id - a.id)[0];
        if (!j) return;
        const now = seen.current.get(j.id) ?? j;
        if (!jobActive(now.state)) return;
        run.current = { paths: [], adopted: true };
        runId.current = j.id;
        setUnlinking(true);
        setUnlinkJob(now);
      })
      .catch(() => {});
    return () => {
      dead = true;
      alive.current = false;
      off?.();
    };
  }, []);

  const onJob = (j: JobInfo) => {
    if (j.op !== "unlink") return;
    const had = seen.current.get(j.id);
    if (!had || jobActive(had.state)) seen.current.set(j.id, j);
    if (!run.current || runId.current === null || j.id !== runId.current) return;
    if (jobActive(j.state)) setUnlinkJob(j);
    else endRun(j);
  };
  const onJobRef = useRef(onJob);
  useEffect(() => {
    onJobRef.current = onJob;
  });

  /** The undo ended: says how it went, and both lists are read again (what is still a link). */
  const endRun = (j: JobInfo) => {
    const r = run.current;
    run.current = null;
    runId.current = null;
    setUnlinking(false);
    setUnlinkJob(null);
    setCancelling(false);
    if (!r) return;
    const byReason = new Map<string, string[]>();
    for (const [p, why] of j.errors) {
      const list = byReason.get(why);
      if (list) list.push(p);
      else byReason.set(why, [p]);
    }
    const errors = jobErrorCount(j);
    const done = Math.max(0, j.done_items - errors);
    let text: string;
    if (r.adopted) {
      const how = j.state === "cancelled" ? "를 취소했어요" : j.state === "failed" ? "가 실패했어요" : "가 끝났어요";
      text = `전에 시작한 하드 링크 풀기${how}. 자세한 내용은 작업 목록에 있어요.`;
    } else if (j.state === "cancelled") {
      text = "하드 링크 풀기를 취소했어요. 그때까지 푼 이름은 독립된 파일로 남고, 나머지는 링크인 채로 그대로예요.";
    } else if (j.state === "failed") {
      text = "하드 링크를 풀지 못했어요. 파일은 그대로예요.";
    } else if (done > 0) {
      text = `${done.toLocaleString()}개를 독립된 파일로 만들었어요. 이제 한쪽을 고쳐도 다른 쪽은 바뀌지 않아요.`;
    } else {
      text = "독립된 파일로 만든 이름이 없어요.";
    }
    if (errors > j.errors.length) text += ` 풀지 못한 이름이 많아서 일부만 보여요(작업 목록에 있어요).`;
    setNotice({ text, error: j.state === "failed" || j.errors.length > 0, failed: [...byReason] });
    if (scan) void find(scan.root);
    loadLedger();
  };

  const start = async (paths: string[]) => {
    setAsk(null);
    if (run.current || !paths.length) return;
    run.current = { paths };
    runId.current = null;
    setUnlinking(true);
    setUnlinkJob(null);
    setCancelling(false);
    setNotice(null);
    try {
      await listening.current;
      const id = await links.unlink(paths);
      if (!alive.current) return;
      runId.current = id;
      // Events that came before the id did.
      const ev = seen.current.get(id);
      if (ev) onJob(ev);
    } catch (err) {
      run.current = null;
      if (!alive.current) return;
      setUnlinking(false);
      setNotice({ text: `하드 링크를 풀지 못했어요: ${err}`, error: true });
    }
  };

  const cancel = () => {
    const id = runId.current;
    if (id === null || cancelling) return;
    setCancelling(true);
    ops.cancelJob(id).catch((err) => {
      setCancelling(false);
      setNotice({ text: String(err), error: true });
    });
  };

  // Any name may be ticked, but not the last one of its file not ticked: it keeps the original.
  const toggle = (g: HardlinkGroup, path: string) => {
    setPicked((s) => {
      const n = new Set(s);
      if (n.has(path)) {
        n.delete(path);
      } else {
        if (!g.paths.some((p) => p !== path && !n.has(p))) return s;
        n.add(path);
      }
      return n;
    });
  };

  const groups = scan?.groups ?? [];
  const listed = groups.slice(0, limit);
  const shielded = groups.filter((g) => g.protected).length;

  /** What would be sent: never a protected file's name, never every name of a file. */
  const chosen = useMemo((): UnlinkAsk => {
    const paths: string[] = [];
    const need = new Map<string, number>();
    let files = 0;
    let bytes = 0;
    for (const g of groups) {
      if (g.protected) continue;
      const sel = g.paths.filter((p) => picked.has(p));
      if (!sel.length || sel.length >= g.paths.length) continue;
      files++;
      paths.push(...sel);
      bytes += sel.length * g.size;
      for (const p of sel) need.set(driveKey(p), (need.get(driveKey(p)) ?? 0) + g.size);
    }
    return { paths, files, bytes, need: [...need] };
  }, [groups, picked]);

  const askLedger = (r: LinkRecord) =>
    setAsk({ paths: [r.linked], files: 1, bytes: r.size, need: [[driveKey(r.linked), r.size]] });

  const ledgerLinked = ledger?.filter((r) => r.still_linked).length ?? 0;
  const busy = unlinking;
  const target = normalizePath(root);

  const frac = !unlinkJob
    ? null
    : unlinkJob.total_bytes > 0
      ? Math.min(1, unlinkJob.done_bytes / unlinkJob.total_bytes)
      : unlinkJob.total_items > 0
        ? Math.min(1, unlinkJob.done_items / unlinkJob.total_items)
        : null;

  const runBox = unlinking && (
    <div className="dup-notice dup-linking">
      <div className="dup-linking-head">
        <b>하드 링크를 푸는 중…</b>
        {unlinkJob && unlinkJob.total_items > 0 && (
          <span className="muted">
            {unlinkJob.done_items.toLocaleString()} / {unlinkJob.total_items.toLocaleString()}
          </span>
        )}
        <span className="spacer" />
        <button className="btn small" disabled={!unlinkJob || cancelling} onClick={cancel}>
          {cancelling ? "취소하는 중…" : "취소"}
        </button>
      </div>
      <div className={`job-bar${frac === null ? " unknown" : ""}`}>
        <span style={frac === null ? undefined : { width: `${frac * 100}%` }} />
      </div>
      {unlinkJob?.current && (
        <div className="job-current" title={shown(unlinkJob.current)}>
          {shown(baseName(unlinkJob.current))}
        </div>
      )}
      <div className="muted">
        {run.current?.adopted
          ? "이 창을 열기 전에 시작한 작업이에요. 끝날 때까지는 새로 풀 수 없어요."
          : "창을 닫아도 계속돼요. 진행 상황은 작업 목록에도 나와요. 끝나면 목록을 다시 읽어요."}
      </div>
    </div>
  );

  const noticeBox = notice && (
    <div className={`dup-notice${notice.error ? " error" : ""}`}>
      <div>{shown(notice.text)}</div>
      {notice.failed?.map(([why, paths]) => (
        <div key={why} className="dup-notice-why">
          {paths.length.toLocaleString()}개는 풀지 못했어요: {shown(why)}
        </div>
      ))}
    </div>
  );

  return (
    <Dialog className="v04-dialog hardlinks" title="하드 링크 찾기·풀기" onClose={onClose}>
      <div className="dlg-head">
        <h3>하드 링크 찾기·풀기</h3>
        <button className="dlg-close" aria-label="닫기" onClick={onClose}>
          ✕
        </button>
      </div>

      <div className="v07-tabs" role="tablist" aria-label="하드 링크 찾기·풀기">
        <button
          role="tab"
          id="hl-tab-find"
          aria-controls="hl-panel-find"
          aria-selected={tab === "find"}
          className={`v07-tab${tab === "find" ? " on" : ""}`}
          onClick={() => setTab("find")}
        >
          폴더에서 찾기
          {scan && <span className="v07-count">{groups.length.toLocaleString()}</span>}
        </button>
        <button
          role="tab"
          id="hl-tab-ledger"
          aria-controls="hl-panel-ledger"
          aria-selected={tab === "ledger"}
          className={`v07-tab${tab === "ledger" ? " on" : ""}`}
          onClick={() => setTab("ledger")}
        >
          fine-file이 합친 파일
          {ledger && <span className="v07-count">{ledgerLinked.toLocaleString()}</span>}
        </button>
      </div>

      {tab === "find" ? (
        <>
          <form
            className="dup-options"
            onSubmit={(e) => {
              e.preventDefault();
              if (!busy && !finding) void find(target);
            }}
          >
            <label className="field grow">
              <span>찾을 폴더</span>
              <input value={root} spellCheck={false} disabled={finding} onChange={(e) => setRoot(e.target.value)} />
            </label>
            <button
              type="submit"
              className="btn primary"
              autoFocus
              disabled={!target || finding || busy}
              title={busy ? "하드 링크를 푸는 동안에는 찾지 않아요" : undefined}
            >
              {finding ? "찾는 중…" : scan ? "다시 찾기" : "찾기"}
            </button>
          </form>

          <div className="dlg-body" role="tabpanel" id="hl-panel-find" aria-labelledby="hl-tab-find">
            {error && <div className="v04-error">{shown(error)}</div>}
            {runBox}
            {noticeBox}

            {!scan && !finding && !error && (
              <>
                <p className="muted v04-intro">
                  하드 링크는 한 파일에 이름이 여러 개 있는 거예요. 한 이름으로 파일을 고치면 다른 이름도 같이 바뀌어요. 이
                  폴더 안에서 다른 이름이 있는 파일을 디스크에서 직접 찾아서(스캔하지 않은 드라이브도 돼요), 고른 이름을
                  독립된 파일로 만들어요. 고르지 않은 이름은 원래 파일을 그대로 써요. 지우는 파일은 없어요.
                </p>
                <p className="muted v04-intro">
                  모드팩이나 게임이 하드 링크로 합친 뒤에 켜지지 않으면, 그 폴더(예: 모드팩 폴더)를 찾아서 풀고 실행기의
                  검사·복구를 한 번 돌리세요.
                </p>
              </>
            )}

            {finding && <div className="dup-progress muted">폴더 안의 파일을 하나씩 살펴보는 중…</div>}

            {scan && !finding && (
              <>
                <div className="dup-summary">
                  {groups.length ? (
                    <b>하드 링크로 이어진 파일 {groups.length.toLocaleString()}개</b>
                  ) : (
                    <b>이 폴더에는 하드 링크로 이어진 파일이 없어요</b>
                  )}
                  <span className="muted">
                    {" "}
                    · {shown(scan.root)} · 살펴본 파일 {scan.scanned.toLocaleString()}개
                  </span>
                </div>
                {scan.truncated && (
                  <div className="dup-caveat">
                    파일이 많아서 {scan.scanned.toLocaleString()}개까지만 살펴봤어요. 더 있을 수 있어요. 더 작은 폴더를 골라 다시
                    찾아 주세요.
                  </div>
                )}
                {shielded > 0 && (
                  <div className="muted dup-binned">
                    Windows가 쓰는 링크 {shielded.toLocaleString()}개는 흐리게 보여요. 건드리지 않아요.
                  </div>
                )}
                {groups.length > 0 && (
                  <div className="dup-tools">
                    <span className="muted">
                      처음에는 폴더 안의 이름을 골라 두고, 파일마다 이름 하나는 원래 파일로 남겨요. "폴더 밖"은 찾은 폴더 밖의
                      이름이에요.
                    </span>
                    <span className="spacer" />
                    <button
                      className="btn"
                      disabled={busy}
                      onClick={() => setPicked(new Set(groups.flatMap((g) => defaultPicks(g, kept))))}
                    >
                      처음처럼 고르기
                    </button>
                    <button className="btn" disabled={busy || !picked.size} onClick={() => setPicked(new Set())}>
                      선택 해제
                    </button>
                  </div>
                )}

                {listed.map((g) => {
                  const any = !g.protected && g.paths.some((p) => picked.has(p));
                  return (
                    <section key={g.id} className={`dup-group hl-group${g.protected ? " protected" : ""}`}>
                      <header className="dup-group-head">
                        <span className="hl-group-tags">
                          <span>
                            {formatSize(g.size)} · 이름 {g.paths.length.toLocaleString()}개
                          </span>
                          {g.by_fine_file && <span className="v04-tag keep">fine-file이 합친 것</span>}
                          {g.protected && <span className="hl-note">{PROTECTED_NOTE}</span>}
                        </span>
                        <span className="muted">{formatDate(g.modified) || "날짜 모름"}</span>
                      </header>
                      {g.paths.map((p, i) => {
                        const on = !g.protected && picked.has(p);
                        // The last name not ticked keeps the original: its box cannot be ticked.
                        const lastOne = !on && g.paths.every((x) => x === p || picked.has(x));
                        return (
                          <div key={p} className={`dup-file${on ? " sel" : ""}`}>
                            <label
                              className="dup-file-pick"
                              title={g.protected ? PROTECTED_NOTE : lastOne ? KEEPS_NOTE : shown(p)}
                            >
                              <input
                                type="checkbox"
                                checked={on}
                                disabled={g.protected || lastOne || busy}
                                onChange={() => toggle(g, p)}
                              />
                              <span className="dup-file-name">
                                <span className="dup-file-base">{shown(baseName(p))}</span>
                                <span className="dup-file-dir">{shown(dirOf(p))}</span>
                              </span>
                            </label>
                            {g.inside[i] !== true && (
                              <span className="v04-tag outside" title={OUTSIDE_NOTE}>
                                폴더 밖
                              </span>
                            )}
                            {on ? (
                              <span className="v04-tag">독립된 파일로</span>
                            ) : (
                              any && (
                                <span className="v04-tag keep" title={KEEPS_NOTE}>
                                  원래 파일로 남음
                                </span>
                              )
                            )}
                            <button className="btn small" onClick={() => onReveal(p)}>
                              위치 보기
                            </button>
                          </div>
                        );
                      })}
                    </section>
                  );
                })}
                {groups.length > limit && (
                  <button className="btn dup-more" onClick={() => setLimit((l) => l + PAGE)}>
                    더 보기 ({(groups.length - limit).toLocaleString()}개 남음)
                  </button>
                )}
              </>
            )}
          </div>

          <div className="dlg-foot">
            {scan && groups.length > 0 && (
              <span className="muted">
                이름 {chosen.paths.length.toLocaleString()}개 고름
                {chosen.files > 0 && ` · 파일 ${chosen.files.toLocaleString()}개 · 빈 공간 ${formatSize(chosen.bytes)} 필요`}
              </span>
            )}
            <span className="spacer" />
            {scan && groups.length > 0 && (
              <button
                className="btn primary"
                disabled={!chosen.paths.length || busy || finding}
                title="고른 이름마다 내용을 복사해서 독립된 파일로 만들어요. 고르지 않은 이름은 원래 파일을 그대로 써요"
                onClick={() => chosen.paths.length && setAsk(chosen)}
              >
                {unlinking ? "푸는 중…" : "독립된 파일로 만들기"}
              </button>
            )}
            <button className="btn" onClick={onClose}>
              닫기
            </button>
          </div>
        </>
      ) : (
        <>
          <div className="dlg-body" role="tabpanel" id="hl-panel-ledger" aria-labelledby="hl-tab-ledger">
            {runBox}
            {noticeBox}
            <p className="muted v04-intro">
              중복 파일 찾기의 "하드 링크로 합치기"로 합친 파일이에요. "링크 풀기"를 누르면 합친 이름이 다시 독립된 파일이
              되고, 남긴 파일은 그대로예요. 0.10.3 전에 합친 파일은 기록이 없어요. 그런 파일은 "폴더에서 찾기"에서 그 폴더를
              골라 찾아 주세요.
            </p>
            {ledgerError && <div className="v04-error">{shown(ledgerError)}</div>}
            {!ledger && !ledgerError && <div className="muted">불러오는 중…</div>}
            {ledger && ledger.length === 0 && <div className="muted">0.10.3부터 합친 파일이 아직 없어요.</div>}
            {ledger && ledger.length > 0 && (
              <div className="hl-list">
                {ledger.slice(0, ledgerLimit).map((r) => (
                  <div key={`${r.linked}|${r.at}`} className="hl-rec">
                    <span className="dup-file-name" title={shown(r.linked)}>
                      <span className="dup-file-base">{shown(baseName(r.linked))}</span>
                      <span className="dup-file-dir">{shown(dirOf(r.linked))}</span>
                      <span className="dup-file-dir" title={shown(r.kept)}>
                        남긴 파일: {shown(r.kept)}
                      </span>
                    </span>
                    <span className="hl-rec-meta">
                      {formatSize(r.size)}
                      <br />
                      {formatDate(r.at) ? `${formatDate(r.at)}에 합침` : "합친 날짜 모름"}
                    </span>
                    {r.still_linked ? (
                      <button className="btn small" disabled={busy} onClick={() => askLedger(r)}>
                        링크 풀기
                      </button>
                    ) : (
                      <span className="v04-tag restored" title="두 이름이 이제 같은 파일이 아니에요">
                        이미 따로예요
                      </span>
                    )}
                    <button className="btn small" onClick={() => onReveal(r.linked)}>
                      위치 보기
                    </button>
                  </div>
                ))}
              </div>
            )}
            {ledger && ledger.length > ledgerLimit && (
              <button className="btn dup-more" onClick={() => setLedgerLimit((l) => l + PAGE)}>
                더 보기 ({(ledger.length - ledgerLimit).toLocaleString()}개 남음)
              </button>
            )}
          </div>
          <div className="dlg-foot">
            <span className="spacer" />
            <button className="btn" onClick={loadLedger}>
              다시 읽기
            </button>
            <button className="btn" onClick={onClose}>
              닫기
            </button>
          </div>
        </>
      )}

      {ask && <UnlinkConfirm ask={ask} drives={drives} onCancel={() => setAsk(null)} onConfirm={() => void start(ask.paths)} />}
    </Dialog>
  );
}

/** The prompt before an undo: what it does, the room it takes, and a drive short of it. */
function UnlinkConfirm({
  ask,
  drives,
  onCancel,
  onConfirm,
}: {
  ask: UnlinkAsk;
  drives: DriveInfo[];
  onCancel(): void;
  onConfirm(): void;
}) {
  // Free space as last read (the job checks again before each name): the copies, and UNLINK_SPARE
  // left over after the last of them, as the job wants it.
  const spare = formatSize(UNLINK_SPARE);
  const short = ask.need
    .map(([d, bytes]) => ({ d, need: bytes + UNLINK_SPARE, free: drives.find((x) => driveKey(x.letter) === d) }))
    .filter((x) => x.free && x.free.total > 0 && x.need > x.free.free)
    .map(
      (x) =>
        `${x.d} 드라이브에 빈 공간이 모자라요 (필요 ${formatSize(x.need)}, 여유 ${spare} 포함, 남음 ${formatSize(x.free!.free)})`,
    );
  const tooMany = ask.paths.length > UNLINK_MAX;
  const ok = short.length === 0 && !tooMany;
  return (
    <Dialog className="dup-confirm" title="독립된 파일로 만들까요?" onClose={onCancel} onBackdrop={onCancel}>
      <h3>독립된 파일로 만들까요?</h3>
      <p>
        {ask.files > 1 && `파일 ${ask.files.toLocaleString()}개에서 `}이름 {ask.paths.length.toLocaleString()}개를 독립된
        파일로 만들어요. 빈 공간이 <b>{formatSize(ask.bytes)}</b> 더 필요하고, 드라이브마다 {spare}는 남아 있어야 해요.
      </p>
      {short.map((t) => (
        <p key={t} className="v04-error">
          {t}
        </p>
      ))}
      {tooMany && (
        <p className="v04-error">
          한 번에 풀 수 있는 이름은 {UNLINK_MAX.toLocaleString()}개까지예요 (고른 이름 {ask.paths.length.toLocaleString()}개).
          고른 이름을 줄인 뒤에 다시 눌러 주세요.
        </p>
      )}
      <ul className="dup-confirm-list">
        <li>
          고른 이름마다 내용을 같은 폴더에 복사하고, 원래 파일과 똑같은지 확인한 뒤에 그 이름 자리에 놓아요. 날짜와 속성도
          그대로예요.
        </li>
        <li>고르지 않은 이름은 원래 파일을 그대로 써요. 지우거나 휴지통으로 보내는 파일은 없어요.</li>
        <li>그 뒤로는 한쪽을 고쳐도 다른 쪽은 바뀌지 않아요.</li>
        <li>그 파일을 쓰는 게임·실행기·프로그램은 먼저 꺼 주세요. 쓰는 중인 파일은 풀지 못할 수 있어요.</li>
        <li>모드팩이나 게임이면 다 푼 뒤에 실행기의 검사·복구를 한 번 돌리세요.</li>
      </ul>
      <div className="actions">
        <button onClick={onCancel} autoFocus={!ok}>
          취소
        </button>
        <button className="primary" autoFocus={ok} disabled={!ok} onClick={onConfirm}>
          독립된 파일로 만들기
        </button>
      </div>
    </Dialog>
  );
}
