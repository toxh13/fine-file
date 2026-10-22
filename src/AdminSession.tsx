import { useEffect, useState, useSyncExternalStore } from "react";
import { v04, type AdminStatus, type DriveInfo } from "./api";
import { driveKey } from "./format";
import "./v04.css";

/**
 * Whether the admin session can read `path`: one on a drive letter of this computer. The helper
 * takes no network path, and an elevated process does not see the drive letters this account
 * mapped to network shares or made with SUBST, so offering it there would only end in an error
 * after the UAC prompt. A drive not listed (yet) is not offered either.
 */
export function adminCanRead(path: string, drives: DriveInfo[]): boolean {
  if (!/^[A-Za-z]:\\/.test(path)) return false;
  const d = drives.find((x) => driveKey(x.letter) === driveKey(path));
  return (
    d !== undefined &&
    !d.subst &&
    (d.kind === "fixed" || d.kind === "removable" || d.kind === "cdrom" || d.kind === "ram")
  );
}

/** How often the bar asks how long the session has left, while there is one. */
const POLL_MS = 5000;

// The session as last heard, shared by the bar and ensureAdmin().
let status: AdminStatus = { active: false, expires_in_s: 0, detail: null };
const listeners = new Set<() => void>();

function publish(s: AdminStatus) {
  status = s;
  listeners.forEach((f) => f());
}

function subscribe(f: () => void) {
  listeners.add(f);
  return () => {
    listeners.delete(f);
  };
}

const snapshot = () => status;

const asError = (err: unknown) => (err instanceof Error ? err : new Error(String(err)));

/**
 * An admin read's error as the user sees it. A refusal (access denied) while the session runs
 * without the backup privilege says so: that is the likely reason.
 */
export function adminError(err: unknown): string {
  const text = err instanceof Error ? err.message : String(err);
  return status.active && status.detail && /\(os error 5\)/.test(text) ? `${text} · ${status.detail}` : text;
}

/** One UAC prompt at a time: callers that come while it is up wait for the same answer. */
let starting: Promise<AdminStatus> | null = null;

/**
 * An admin session to use: the one running, or a new one (one UAC prompt). True once there is
 * one; when there is not, the reason (UAC cancelled, not installed in Program Files…) is the
 * thrown Error's message.
 */
export async function ensureAdmin(): Promise<boolean> {
  try {
    const now = await v04.adminStatus();
    publish(now);
    if (now.active) return true;
  } catch {
    /* asked below */
  }
  starting ??= v04.adminStart().finally(() => {
    starting = null;
  });
  let s: AdminStatus;
  try {
    s = await starting;
  } catch (err) {
    throw asError(err);
  }
  publish(s);
  if (s.active) return true;
  if (s.detail) throw new Error(s.detail);
  return false;
}

/** "관리자 모드 · N분 남음" while an admin session is up; nothing otherwise. */
export function AdminBar() {
  const s = useSyncExternalStore(subscribe, snapshot);
  const [stopping, setStopping] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // A session started before this mounted (another window part, a reload).
  useEffect(() => {
    v04.adminStatus().then(publish, () => {});
  }, []);

  useEffect(() => {
    if (!s.active) return;
    const t = setInterval(() => v04.adminStatus().then(publish, () => {}), POLL_MS);
    return () => clearInterval(t);
  }, [s.active]);

  if (!s.active) return null;

  const stop = async () => {
    setStopping(true);
    setError(null);
    try {
      await v04.adminStop();
      publish({ active: false, expires_in_s: 0, detail: null });
      v04.adminStatus().then(publish, () => {});
    } catch (err) {
      setError(String(err));
    } finally {
      setStopping(false);
    }
  };

  const minutes = Math.max(1, Math.ceil(s.expires_in_s / 60));
  return (
    <div className="admin-bar" role="status">
      <span className="admin-bar-dot" aria-hidden="true" />
      <span className="admin-bar-text" title="관리자 권한으로 보호된 폴더를 읽을 수 있어요. 파일은 바꾸거나 지우지 않아요">
        관리자 모드 · {minutes.toLocaleString()}분 남음
      </span>
      {/* Up, but with less than it should have (no backup privilege): some items stay closed. */}
      {s.detail && !error && (
        <span className="admin-bar-warn" title={s.detail}>
          {s.detail}
        </span>
      )}
      {error && <span className="admin-bar-error">{error}</span>}
      <button className="admin-bar-stop" disabled={stopping} onClick={stop}>
        {stopping ? "끝내는 중…" : "끝내기"}
      </button>
    </div>
  );
}
