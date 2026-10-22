import { useCallback, useEffect, useId, useMemo, useRef, useState } from "react";
import type { UnlistenFn } from "@tauri-apps/api/event";
import { opEvents, v04, type DriveInfo, type Entry, type JobInfo, type Relocation } from "./api";
import { Dialog } from "./Duplicates";
import { driveKey, formatDate, formatSize, normalizePath } from "./format";
import "./v04.css";

const FOLDER_NAME = "fine-file 옮긴 폴더";

const defaultFolder = (letter: string) => `${driveKey(letter)}\\${FOLDER_NAME}`;

function baseName(p: string): string {
  const t = p.replace(/[\\/]+$/, "");
  const i = t.lastIndexOf("\\");
  return i < 0 ? t : t.slice(i + 1);
}

/** Room for the folder on `d`; a folder whose size is not known is let through (the job checks). */
const fits = (d: DriveInfo, bytes: number) => bytes <= 0 || d.free > bytes;

export function RelocateDialog({
  source,
  drives,
  onClose,
  onStarted,
}: {
  source: Entry;
  drives: DriveInfo[];
  onClose(): void;
  onStarted(jobId: number): void;
}) {
  const from = driveKey(source.path);
  // Other drives of this computer: a junction cannot point at a network drive or a stick that may go.
  const choices = useMemo(
    () => drives.filter((d) => d.kind === "fixed" && d.total > 0 && driveKey(d.letter) !== from),
    [drives, from],
  );
  const [letter, setLetter] = useState<string | null>(() => {
    const ok = choices.filter((d) => fits(d, source.size));
    return [...(ok.length ? ok : choices)].sort((a, b) => b.free - a.free)[0]?.letter ?? null;
  });
  const [folder, setFolder] = useState(() => (letter ? defaultFolder(letter) : ""));
  const [busy, setBusy] = useState(false);
  const [started, setStarted] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const group = useId();

  const dest = normalizePath(folder);
  const destDrive = /^[A-Za-z]:\\/.test(dest) ? driveKey(dest) : null;
  const target = choices.find((d) => driveKey(d.letter) === destDrive) ?? null;

  let problem: string | null = null;
  if (!source.is_dir) problem = "폴더만 옮길 수 있어요";
  else if (source.reparse) problem = "이미 다른 곳을 가리키는 링크 폴더예요. 링크는 옮기지 않아요";
  else if (/^[A-Za-z]:\\?$/.test(source.path)) problem = "드라이브 전체는 옮길 수 없어요";
  else if (!choices.length) problem = "옮겨 둘 다른 로컬 드라이브가 없어요";
  else if (!dest) problem = "옮겨 둘 폴더를 적어 주세요";
  else if (!target) problem = "이 컴퓨터의 다른 로컬 드라이브에 있는 폴더를 적어 주세요";
  else if (!fits(target, source.size)) problem = `${target.letter} 드라이브에 빈 공간이 모자라요`;

  const pickDrive = (d: DriveInfo) => {
    setLetter(d.letter);
    setFolder((f) => {
      const cur = normalizePath(f);
      // The suggested folder follows the drive; one the user wrote keeps its own path there.
      if (!cur || (letter && cur === defaultFolder(letter))) return defaultFolder(d.letter);
      return /^[A-Za-z]:\\/.test(cur) ? `${driveKey(d.letter)}${cur.slice(2)}` : defaultFolder(d.letter);
    });
  };

  const typeFolder = (v: string) => {
    setFolder(v);
    const p = normalizePath(v);
    const d = /^[A-Za-z]:\\/.test(p) ? choices.find((c) => driveKey(c.letter) === driveKey(p)) : undefined;
    if (d) setLetter(d.letter);
  };

  const start = async () => {
    if (problem || busy) return;
    setBusy(true);
    setError(null);
    try {
      const id = await v04.startRelocate(source.path, dest);
      setStarted(true);
      onStarted(id);
    } catch (err) {
      setError(String(err));
      setBusy(false);
    }
  };

  return (
    <Dialog className="v04-dialog reloc" title="다른 드라이브로 옮기기" onClose={onClose}>
      <div className="dlg-head">
        <h3>다른 드라이브로 옮기기</h3>
        <button className="dlg-close" aria-label="닫기" onClick={onClose}>
          ✕
        </button>
      </div>
      <form
        className="dlg-body reloc-body"
        onSubmit={(e) => {
          e.preventDefault();
          start();
        }}
      >
        <div className="reloc-source">
          <b title={source.path}>{source.name}</b>
          <span className="muted">
            {source.size > 0 ? formatSize(source.size) : "크기 모름"} · {source.path}
          </span>
        </div>

        {choices.length > 0 && (
          <fieldset className="reloc-drives" disabled={busy}>
            <legend>옮겨 둘 드라이브</legend>
            {choices.map((d) => {
              const ok = fits(d, source.size);
              const used = d.total > 0 ? (d.total - d.free) / d.total : 0;
              const on = letter === d.letter;
              return (
                <label key={d.letter} className={`reloc-drive${on ? " on" : ""}${ok ? "" : " short"}`}>
                  <input type="radio" name={group} checked={on} disabled={!ok} onChange={() => pickDrive(d)} />
                  <span className="reloc-drive-main">
                    <span className="drive-name">
                      {d.label || "로컬 디스크"} ({d.letter})
                    </span>
                    <span className="usage">
                      <span className={used > 0.9 ? "full" : undefined} style={{ width: `${(used * 100).toFixed(1)}%` }} />
                    </span>
                    <span className="drive-meta">
                      {formatSize(d.free)} 남음 / {formatSize(d.total)}
                      {d.fs ? ` · ${d.fs}` : ""}
                      {ok ? "" : " · 공간 부족"}
                    </span>
                  </span>
                </label>
              );
            })}
          </fieldset>
        )}

        <label className="field">
          <span>옮겨 둘 폴더</span>
          <input value={folder} spellCheck={false} disabled={busy || !choices.length} onChange={(e) => typeFolder(e.target.value)} />
        </label>
        {dest && target && (
          <div className="reloc-final muted">
            새 위치: {dest.endsWith("\\") ? `${dest}${source.name}` : `${dest}\\${source.name}`}
          </div>
        )}

        <ul className="reloc-explain">
          <li>
            폴더를 {target ? `${target.letter} 드라이브` : "다른 드라이브"}로 옮기고, 원래 위치에는 링크가 남아서 프로그램은
            그대로 동작해요.
          </li>
          <li>원본은 복사·확인 후 지워져요.</li>
          <li>옮기는 동안에는 이 폴더를 쓰는 프로그램을 꺼 두세요. 쓰고 있는 파일은 옮기지 못해요.</li>
          <li>옮긴 폴더 목록에서 언제든 원래 자리로 되돌릴 수 있어요.</li>
        </ul>

        {problem && <div className="v04-error">{problem}</div>}
        {error && <div className="v04-error">{error}</div>}
        {started && <div className="v04-ok">옮기기를 시작했어요. 진행 상황은 작업 창에 보여요.</div>}

        <div className="actions">
          <button type="button" onClick={onClose}>
            {started ? "닫기" : "취소"}
          </button>
          <button type="submit" className="primary" autoFocus disabled={!!problem || busy}>
            옮기기
          </button>
        </div>
      </form>
    </Dialog>
  );
}

const STATE_LABEL: Record<Relocation["state"], string> = {
  active: "옮겨 둠",
  restored: "되돌림",
  broken: "링크 끊김",
};

/** How an undo started here ended, for the line at the top of the list. */
function undoEnd(j: JobInfo): { text: string; error?: boolean } {
  if (j.state === "cancelled") return { text: "되돌리기를 취소했어요" };
  if (j.state === "failed") return { text: `되돌리지 못했어요: ${j.errors[0]?.[1] ?? "알 수 없는 오류"}`, error: true };
  if (j.errors.length || (j.error_count ?? 0) > 0) {
    return { text: "원래 자리로 되돌렸지만 마무리하지 못한 일이 있어요. 작업 창에서 확인해 주세요", error: true };
  }
  return { text: "원래 자리로 되돌렸어요" };
}

export function Relocations({ onClose }: { onClose(): void }) {
  const [list, setList] = useState<Relocation[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<{ text: string; error?: boolean } | null>(null);
  const [confirm, setConfirm] = useState<Relocation | null>(null);
  /** Relocation id → the job putting it back. */
  const [undoing, setUndoing] = useState<Map<string, number>>(() => new Map());
  /** Undo jobs started here and not over yet. */
  const ours = useRef(new Set<number>());
  /** Relocate and restore jobs that ended, by id: an undo that fails at once can end before its id comes back. */
  const ended = useRef(new Map<number, JobInfo>());
  const alive = useRef(true);

  const load = useCallback(() => {
    v04.relocations().then(
      (l) => {
        if (!alive.current) return;
        setList(l);
        setError(null);
      },
      (err) => alive.current && setError(String(err)),
    );
  }, []);

  useEffect(() => {
    alive.current = true;
    let dead = false;
    let off: UnlistenFn | null = null;
    load();
    // A move or a move back that ends changes the list.
    opEvents
      .jobProgress((j) => {
        if (dead || (j.op !== "relocate" && j.op !== "restore")) return;
        if (j.state === "running" || j.state === "waiting") return;
        ended.current.set(j.id, j);
        if (ended.current.size > 20) ended.current.delete(ended.current.keys().next().value!);
        if (ours.current.delete(j.id)) {
          setUndoing((m) => new Map([...m].filter(([, job]) => job !== j.id)));
          setNotice(undoEnd(j));
        }
        load();
      })
      .then(
        (f) => (dead ? f() : void (off = f)),
        () => {},
      );
    return () => {
      dead = true;
      alive.current = false;
      off?.();
    };
  }, [load]);

  const undo = async (r: Relocation) => {
    setConfirm(null);
    setNotice(null);
    try {
      const job = await v04.undoRelocate(r.id);
      if (!alive.current) return;
      // Over already (its last event came before the id did): nothing to wait for.
      const end = ended.current.get(job);
      if (end) {
        setNotice(undoEnd(end));
        load();
        return;
      }
      ours.current.add(job);
      setUndoing((m) => new Map(m).set(r.id, job));
      setNotice({ text: "되돌리기를 시작했어요. 진행 상황은 작업 창에 보여요." });
    } catch (err) {
      if (alive.current) setError(String(err));
    }
  };

  return (
    <Dialog className="v04-dialog relocs" title="옮긴 폴더" onClose={onClose}>
      <div className="dlg-head">
        <h3>옮긴 폴더</h3>
        <button className="dlg-close" aria-label="닫기" onClick={onClose}>
          ✕
        </button>
      </div>
      <div className="dlg-body">
        {error && <div className="v04-error">{error}</div>}
        {notice && <div className={notice.error ? "v04-error" : "v04-ok"}>{notice.text}</div>}
        {list === null && !error && <p className="muted v04-intro">불러오는 중…</p>}
        {list?.length === 0 && (
          <p className="muted v04-intro">아직 옮긴 폴더가 없어요. 다른 드라이브로 옮긴 폴더가 여기에 남아요.</p>
        )}
        {!!list?.length && (
          <ul className="reloc-list">
            {list.map((r) => {
              const busy = undoing.has(r.id);
              return (
                <li key={r.id} className={`reloc-item ${r.state}`}>
                  <div className="reloc-top">
                    <b className="reloc-name" title={r.original}>
                      {baseName(r.original)}
                    </b>
                    <span className={`v04-tag ${r.state}`}>{STATE_LABEL[r.state]}</span>
                    <span className="muted">{formatSize(r.size)}</span>
                    <span className="spacer" />
                    <button className="btn" disabled={r.state !== "active" || busy} onClick={() => setConfirm(r)}>
                      {busy ? "되돌리는 중…" : "되돌리기"}
                    </button>
                  </div>
                  <div className="reloc-paths">
                    <span title={r.original}>{r.original}</span>
                    <span className="reloc-arrow">→</span>
                    <span title={r.target}>{r.target}</span>
                  </div>
                  <div className="reloc-when muted">
                    {formatDate(r.created_at) ? `${formatDate(r.created_at)}에 옮김` : "옮긴 날짜 모름"}
                    {r.state === "broken" && " · 링크나 옮긴 폴더가 없어졌어요. 두 위치를 직접 확인해 주세요"}
                  </div>
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

      {confirm && (
        <Dialog title="원래 자리로 되돌릴까요?" onClose={() => setConfirm(null)} onBackdrop={() => setConfirm(null)}>
          <h3>원래 자리로 되돌릴까요?</h3>
          <p>
            <b>{baseName(confirm.original)}</b> · {formatSize(confirm.size)}
          </p>
          <p className="muted">
            {confirm.target}에 있는 폴더를 {confirm.original}(으)로 다시 옮기고, 남겨 둔 링크를 없애요. 원래 드라이브에 빈
            공간이 있어야 하고, 옮기는 동안에는 이 폴더를 쓰는 프로그램을 꺼 두세요.
          </p>
          <div className="actions">
            <button onClick={() => setConfirm(null)}>취소</button>
            <button className="primary" autoFocus onClick={() => undo(confirm)}>
              되돌리기
            </button>
          </div>
        </Dialog>
      )}
    </Dialog>
  );
}
