import { useEffect, useState, useSyncExternalStore } from "react";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { api, ops, plans, v08, type UpdateOthers } from "./api";
import { Dialog } from "./Duplicates";
import { formatSize } from "./format";
import { Markdown } from "./markdown";
import "./components.css";
import "./v04.css";
import "./v06.css";
import "./v08.css";

// ---- v0.8: in-app updates ----------------------------------------------------------
//
// The updater plugin reads latest.json from the GitHub release (tauri.conf.json plugins.updater),
// downloads the installer and checks its signature against the public key built into the app; a
// file that does not match is never installed. Nothing is installed by itself: a new version is
// offered in a banner, and only "지금 업데이트" (after saying what will happen) installs it.
//
// On Windows the installer needs administrator rights (the app is installed for all users, in
// Program Files): one UAC prompt. Once it starts, the plugin ends this process and the installer
// (passive, /R) starts the new version. lib.rs update_prepare first does what closing the window
// does, since the process then ends without the app's exit handling.

const SKIP_KEY = "fine-file.update.skip";
const AUTO_KEY = "fine-file.update.auto";
/** The first automatic check, after the start (the app has other things to do first)... */
const FIRST_CHECK_MS = 15_000;
/** ...and then this often. */
const CHECK_EVERY_MS = 6 * 60 * 60 * 1000;
const CHECK_TIMEOUT_MS = 30_000;
/** The whole download at most (an installer of 15 MB at 30 KB/s takes about 8 minutes)... */
const DOWNLOAD_TIMEOUT_MS = 15 * 60_000;
/** ...and with nothing coming in, at most this long: a connection that stalled. */
const STALL_MS = 60_000;
/** How often the download's progress is drawn, at most. */
const PROGRESS_MS = 100;

/** Why a download was let go of: the user's "내려받기 취소", or nothing came for STALL_MS. */
const CANCELLED = Symbol("cancelled");
const STALLED = Symbol("stalled");

const SIGNATURE_BAD = "업데이트 파일의 서명이 맞지 않아 설치하지 않았어요";
const RELEASE_MISSING = "최신 버전 정보를 찾지 못했어요 (GitHub 릴리스에 latest.json이 없어요). 잠시 뒤에 다시 확인해 주세요";
const RELEASE_BAD =
  "GitHub 릴리스의 최신 버전 정보(latest.json)가 올바르지 않거나 이 컴퓨터(Windows x64)용 업데이트가 없어요. 다음 릴리스를 기다려 주세요";
const PLAN_ELSEWHERE =
  "다른 fine-file 창에서 정리 계획을 실행하거나 되돌리는 중이에요. 설치 프로그램이 그 창을 강제로 꺼서 하던 단계가 되돌리기 기록에 빠질 수 있으니, 끝난 뒤에 업데이트해 주세요";
const RELOCATION_ELSEWHERE = "다른 fine-file 창에서 폴더를 다른 드라이브로 옮기거나 되돌리는 중이에요. 끝난 뒤에 업데이트해 주세요";

function readStore(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function writeStore(key: string, value: string | null) {
  try {
    if (value === null) localStorage.removeItem(key);
    else localStorage.setItem(key, value);
  } catch {
    /* storage blocked: the choice lasts until the app closes */
  }
}

/** "시작할 때 업데이트 확인": on unless turned off. */
export function autoCheckOn(): boolean {
  return readStore(AUTO_KEY) !== "0";
}

/** The particle after a version: "0.8.1이", "0.8.2가" (by how its last digit is read). */
function iGa(version: string): string {
  const last = version.slice(-1);
  if (!/[0-9]/.test(last)) return "(이)가";
  return "2459".includes(last) ? "가" : "이";
}

/** "0.8.1로", "0.8.0으로": 영, 삼, 육 end in a consonant other than ㄹ. */
function euRo(version: string): string {
  const last = version.slice(-1);
  if (!/[0-9]/.test(last)) return "(으)로";
  return "036".includes(last) ? "으로" : "로";
}

/** The updater's English errors in a sentence for the user. */
function updateError(err: unknown, stage: "check" | "download" | "install"): string {
  if (err === STALLED) return "업데이트 파일을 1분 동안 하나도 받지 못해서 멈췄어요. 인터넷 연결을 확인하고 다시 해 주세요";
  const s = String(err);
  // The signature's own errors, and those of decoding it (base64) when it was tampered with.
  if (/signature|minisign|signed for version|was signed|public key|different key|base64|Invalid encoding|Invalid symbol|Invalid padding|Invalid input length|Invalid last symbol/i.test(s)) {
    return SIGNATURE_BAD;
  }
  if (stage === "install" && /os error (5|1223)\b|access is denied|액세스가 거부/i.test(s)) {
    return "UAC 창에서 허락하지 않아서 업데이트를 설치하지 않았어요. 앱은 그대로 쓸 수 있어요";
  }
  // GitHub answered, but not with a release the app can read: no latest.json (a 404, say: the
  // newest release has none), or one that is broken or has no installer for this computer.
  if (/fetch a valid release/i.test(s)) return RELEASE_MISSING;
  if (/was not found in the response|fallback platforms|at line \d+ column \d+|decoding response body/i.test(s)) return RELEASE_BAD;
  const status = /failed with status:?\s*(\d{3}[^\n]*)/i.exec(s);
  if (status) return `업데이트 파일을 내려받지 못했어요 (HTTP ${status[1].trim()}). 잠시 뒤에 다시 해 주세요`;
  if (/request|connect|dns|resolve|timed? ?out|network|tls|certificate|socket|os error 100\d\d/i.test(s)) {
    return stage === "check"
      ? "업데이트를 확인하지 못했어요. 인터넷 연결을 확인해 주세요"
      : "업데이트 파일을 내려받지 못했어요. 인터넷 연결을 확인하고 다시 해 주세요";
  }
  if (stage === "check") return `업데이트를 확인하지 못했어요: ${s}`;
  return `업데이트하지 못했어요: ${s}`;
}

// ---- the window's one updater state (the banner and the settings both read it) ----------

interface UpdaterState {
  /** The release the last check found (newer than this one), until a later check finds none. */
  update: Update | null;
  /** The banner offers it: false after "나중에" or "이 버전 건너뛰기", until a check shows it again. */
  shown: boolean;
  /** It is the version "이 버전 건너뛰기" set aside. */
  skipped: boolean;
  checking: boolean;
  /** What the last check came to, for the settings. */
  last: { text: string; error?: boolean } | null;
  /** The install under way: the download, making the app ready (lib.rs), starting the installer. */
  stage: "download" | "prepare" | "install" | null;
  progress: { done: number; total: number | null } | null;
  /** Its bytes are in (another try does not download again). */
  downloaded: boolean;
  /** Why the last download or install did not go through. */
  error: string | null;
  /** The last try failed past the download: the plugin may have let go of the update, so check again. */
  stale: boolean;
}

let state: UpdaterState = {
  update: null,
  shown: false,
  skipped: false,
  checking: false,
  last: null,
  stage: null,
  progress: null,
  downloaded: false,
  error: null,
  stale: false,
};
const listeners = new Set<() => void>();

function set(p: Partial<UpdaterState>) {
  state = { ...state, ...p };
  for (const l of listeners) l();
}

function subscribe(l: () => void) {
  listeners.add(l);
  return () => {
    listeners.delete(l);
  };
}

export function useUpdater(): UpdaterState {
  return useSyncExternalStore(subscribe, () => state);
}

/**
 * Whether a new version is there that the user has not set aside ("이 버전 건너뛰기"; the
 * sidebar's "새 버전"). Only this, so the download's progress does not draw the whole window again.
 */
export function useUpdateOffered(): boolean {
  return useSyncExternalStore(subscribe, () => state.update !== null && (state.shown || !state.skipped));
}

/** "나중에": that version is not offered again by an automatic check before `until`... */
let snoozed: { version: string; until: number } | null = null;
/** ...and is offered again then (`remind`). */
let remindTimer: number | undefined;

function remind() {
  const u = state.update;
  if (snoozed && u && u.version === snoozed.version && !state.shown && !state.skipped && !state.stage && autoCheckOn()) {
    set({ shown: true });
  }
  snoozed = null;
}

/**
 * Asks the release for a newer version (one check at a time; none while one installs). `manual`
 * (the settings' "업데이트 확인") shows it even if that version was skipped or put off; an
 * automatic check says nothing when it fails.
 */
export async function checkForUpdate(manual: boolean): Promise<void> {
  if (state.checking || state.stage) return;
  set({ checking: true });
  try {
    const found = await check({ timeout: CHECK_TIMEOUT_MS });
    const old = state.update;
    if (old && old !== found) void old.close().catch(() => {});
    if (!found) {
      set({ update: null, shown: false, skipped: false, downloaded: false, error: null, stale: false, last: { text: "지금 버전이 최신이에요" } });
      return;
    }
    const skipped = readStore(SKIP_KEY) === found.version;
    if (manual) snoozed = null;
    const putOff = snoozed !== null && snoozed.version === found.version && Date.now() < snoozed.until;
    // A banner the user sees now stays (an automatic check after "업데이트 확인" does not hide it).
    const seen = state.shown && old?.version === found.version;
    set({
      update: found,
      shown: manual || seen || (!skipped && !putOff),
      skipped,
      downloaded: false,
      error: null,
      stale: false,
      last: {
        text: `새 버전 v${found.version}${iGa(found.version)} 있어요${skipped ? " (건너뛰기로 한 버전이에요)" : ""}. 창 위쪽의 알림에서 업데이트할 수 있어요`,
      },
    });
  } catch (err) {
    set({ last: { text: updateError(err, "check"), error: true } });
  } finally {
    set({ checking: false });
  }
}

/** What is going on that the update would cut short, here and in the app's other windows. */
interface Busy {
  /** This window's copy and move jobs not over yet. */
  transfers: number;
  /** This window's relocations (and undos) not over yet. */
  relocations: number;
  /** This window's merges of duplicates into hard links (Duplicates.tsx) not over yet. */
  links: number;
  /** The ids of those jobs: the ones the user is told about (lib.rs update_prepare stops only these). */
  jobIds: number[];
  /** A plan is run or undone (by this window, unless others.plan)... */
  plan: boolean;
  /** ...which one. */
  planId: string | null;
  others: UpdateOthers;
}

async function busyNow(): Promise<Busy> {
  const [jobs, list, others] = await Promise.all([
    ops.jobs().catch(() => []),
    plans.list().catch(() => []),
    v08.updateOthers().catch((): UpdateOthers => ({ processes: 0, plan: false, relocation: false })),
  ]);
  const going = jobs.filter((j) => j.state === "running" || j.state === "waiting");
  const running = list.find((p) => p.state === "running");
  return {
    transfers: going.filter((j) => j.op === "copy" || j.op === "move").length,
    relocations: going.filter((j) => j.op === "relocate" || j.op === "restore").length,
    links: going.filter((j) => j.op === "link").length,
    jobIds: going.map((j) => j.id),
    plan: running !== undefined,
    planId: running?.id ?? null,
    others,
  };
}

/** Work of this window the update stops (as closing the window does) once the user agrees. */
const stopsWork = (b: Busy) => b.transfers > 0 || b.relocations > 0 || b.links > 0 || (b.plan && !b.others.plan);

/** Why the update cannot be installed now: work in another window the installer would cut short. */
const blockedBy = (b: Busy) => (b.others.plan ? PLAN_ELSEWHERE : b.others.relocation ? RELOCATION_ELSEWHERE : null);

/** Lets go of the download under way, if one (`download`). */
let dropDownload: ((why: symbol) => void) | null = null;

/** "내려받기 취소": the download is no longer waited for, and nothing is installed. */
function cancelDownload() {
  dropDownload?.(CANCELLED);
}

/**
 * The update's download, with its progress drawn. It fails when nothing comes in for STALL_MS,
 * and can be let go of (`cancelDownload`). The plugin cannot stop a download, so one let go of
 * runs out in the background (DOWNLOAD_TIMEOUT_MS at most) and its end is ignored.
 */
function download(u: Update): Promise<{ done: number; total: number | null }> {
  return new Promise((resolve, reject) => {
    let done = 0;
    let total: number | null = null;
    let drawn = 0;
    let heard = Date.now();
    let over = false;
    const end = (then: () => void) => {
      if (over) return;
      over = true;
      window.clearInterval(watch);
      dropDownload = null;
      then();
    };
    const watch = window.setInterval(() => {
      if (Date.now() - heard >= STALL_MS) end(() => reject(STALLED));
    }, 5_000);
    dropDownload = (why) => end(() => reject(why));
    // A download let go of that ends after a newer check: its update is no one's any more.
    const after = () => {
      if (over && state.update !== u) void u.close().catch(() => {});
    };
    u.download(
      (e) => {
        if (over) return;
        heard = Date.now();
        if (e.event === "Started") total = e.data.contentLength ?? null;
        else if (e.event === "Progress") done += e.data.chunkLength;
        const now = Date.now();
        if (e.event !== "Progress" || now - drawn >= PROGRESS_MS) {
          drawn = now;
          set({ progress: { done, total } });
        }
      },
      { timeout: DOWNLOAD_TIMEOUT_MS },
    ).then(
      () => {
        after();
        end(() => resolve({ done, total }));
      },
      (err) => {
        after();
        end(() => reject(err));
      },
    );
  });
}

/**
 * Downloads (unless done), readies the app and starts the installer; on Windows the app ends there.
 * `agreed` is the work the user was told about and agreed to stop. "busy" when other work started
 * meanwhile (during the download, say): they are asked again, and nothing was stopped.
 */
async function install(agreed: Busy): Promise<"busy" | void> {
  const u = state.update;
  if (!u || state.stage) return;
  let stage: "download" | "install" = "download";
  try {
    if (!state.downloaded) {
      set({ stage: "download", progress: { done: 0, total: null }, error: null });
      const { done, total } = await download(u);
      set({ downloaded: true, progress: { done, total: total ?? done } });
    }
    stage = "install";
    set({ stage: "prepare", error: null });
    // lib.rs's own refusals (work elsewhere, work that would not stop) are Korean already.
    let ready: boolean;
    try {
      ready = await v08.updatePrepare(agreed.jobIds, agreed.others.plan ? null : agreed.planId);
    } catch (err) {
      set({ stage: null, error: String(err) });
      return;
    }
    if (!ready) {
      set({ stage: null });
      return "busy";
    }
    set({ stage: "install" });
    try {
      // Windows: starts the installer (the UAC prompt first) and ends this process.
      await u.install();
    } catch (err) {
      // The plugin hides the windows before it starts the installer: they come back.
      await v08.updateInstallFailed().catch(() => {});
      throw err;
    }
    // Elsewhere the new version is in place once install() returns; the app starts it.
    await relaunch();
  } catch (err) {
    if (err === CANCELLED) {
      set({ stage: null, progress: null });
      return;
    }
    set({ stage: null, error: updateError(err, stage), downloaded: stage === "install" && state.downloaded, stale: stage === "install" });
  }
}

// ---- the banner ---------------------------------------------------------------------

/**
 * The offer of a new version, over the tabs, and the automatic checks (15 s after the start, then
 * every 6 hours, while the window is shown; "시작할 때 업데이트 확인"). Mounted once (App).
 */
export function UpdateBanner() {
  const st = useUpdater();
  // The question before installing: what will happen, and the work it would stop.
  const [ask, setAsk] = useState<Busy | null>(null);
  const [asking, setAsking] = useState(false);
  const [version, setVersion] = useState("");

  useEffect(() => {
    api.appInfo().then(
      (i) => setVersion(i.version),
      () => {},
    );
    // A check that falls due while the window is hidden waits until it is shown again.
    let due = false;
    const tick = () => {
      if (!autoCheckOn()) return;
      if (document.visibilityState === "hidden") {
        due = true;
        return;
      }
      due = false;
      void checkForUpdate(false);
    };
    const first = window.setTimeout(tick, FIRST_CHECK_MS);
    const every = window.setInterval(tick, CHECK_EVERY_MS);
    const onVisible = () => {
      if (due && document.visibilityState === "visible") tick();
    };
    document.addEventListener("visibilitychange", onVisible);
    return () => {
      window.clearTimeout(first);
      window.clearInterval(every);
      document.removeEventListener("visibilitychange", onVisible);
    };
  }, []);

  const u = st.update;
  const p = st.progress;
  const pct = p && p.total ? Math.min(100, Math.round((p.done / p.total) * 100)) : null;
  const headline = u ? `새 버전 v${u.version}${iGa(u.version)} 나왔어요` : "";
  // What a screen reader says: the offer, what the install is at, and the progress only by tens of
  // percent (the bar itself changes up to every PROGRESS_MS). Kept mounted, so changes are heard.
  const spoken = !u || !st.shown
    ? ""
    : st.error
      ? st.error
      : st.stage === "download"
        ? `업데이트 파일을 내려받는 중이에요${pct === null ? "" : ` (${Math.floor(pct / 10) * 10}%)`}`
        : st.stage !== null
          ? "업데이트를 설치하는 중이에요"
          : headline;
  const live = (
    <span className="update-live" role="status">
      {spoken}
    </span>
  );
  if (!u || !st.shown) return live;

  const begin = async () => {
    if (asking) return;
    setAsking(true);
    try {
      if (st.stale) {
        // The last try may have cost the update its handle in the plugin: a fresh check.
        await checkForUpdate(true);
        if (state.last?.error) set({ error: state.last.text });
        if (!state.update || state.stale) return;
      }
      setAsk(await busyNow());
    } finally {
      setAsking(false);
    }
  };

  const go = async (busy: Busy) => {
    setAsk(null);
    if ((await install(busy)) === "busy") setAsk(await busyNow());
  };

  const later = () => {
    snoozed = { version: u.version, until: Date.now() + CHECK_EVERY_MS };
    window.clearTimeout(remindTimer);
    remindTimer = window.setTimeout(remind, CHECK_EVERY_MS);
    set({ shown: false });
  };
  const skip = () => {
    writeStore(SKIP_KEY, u.version);
    set({ shown: false, skipped: true });
  };

  const working = st.stage !== null;
  const notes = u.body?.trim();

  // The dialogs are beside the banner, not in it: the banner's own look (.banner button) is not theirs.
  return (
    <>
      <div className="banner info update-banner">
        <span className="grow">
          <b>{headline}</b>
          {version && <span className="muted"> (지금 v{version})</span>}
          {notes && (
            <details className="update-notes">
              <summary>바뀐 점 보기</summary>
              <Markdown text={notes} className="update-notes-body" />
            </details>
          )}
          {st.stage === "download" && p && (
            <span className="update-progress">
              <span className={`job-bar ${pct === null ? "unknown" : ""}`}>
                <span style={pct === null ? undefined : { width: `${pct}%` }} />
              </span>
              <span className="banner-detail">
                내려받는 중… {formatSize(p.done)}
                {p.total ? ` / ${formatSize(p.total)} (${pct}%)` : ""}
              </span>
            </span>
          )}
          {st.error && <span className="banner-detail update-error">{st.error}</span>}
        </span>
        {!working && (
          <>
            <button disabled={asking} onClick={() => void begin()}>
              {st.error ? "다시 시도" : "지금 업데이트"}
            </button>
            <button className="link" onClick={later} title="지금은 닫고, 6시간쯤 뒤(또는 다음에 켤 때) 다시 알려 줘요">
              나중에
            </button>
            <button className="link" onClick={skip} title="이 버전은 다시 알리지 않아요. 더 새 버전이 나오면 알려 줘요">
              이 버전 건너뛰기
            </button>
          </>
        )}
        {st.stage === "download" && (
          <button className="link" onClick={cancelDownload} title="내려받기를 그만둬요. 아무것도 설치하지 않고, 알림은 남아서 나중에 다시 할 수 있어요">
            내려받기 취소
          </button>
        )}
      </div>
      {live}

      {ask && <InstallConfirm version={u.version} busy={ask} onCancel={() => setAsk(null)} onGo={() => void go(ask)} />}
      {(st.stage === "prepare" || st.stage === "install") && (
        <Dialog className="update-confirm" title="업데이트 설치" onClose={() => {}}>
          <h3>업데이트를 설치하는 중이에요</h3>
          <p>
            {st.stage === "prepare"
              ? "하던 작업을 정리하고 스캔 결과를 저장하는 중이에요…"
              : "설치 프로그램을 시작해요. UAC 창이 뜨면 \"예\"를 눌러 주세요. 앱이 꺼지고, 짧은 설치 창이 보인 뒤 새 버전이 다시 켜져요."}
          </p>
        </Dialog>
      )}
    </>
  );
}

/** What installing does, said before it starts, with the work it would stop or is held up by. */
function InstallConfirm({ version, busy, onCancel, onGo }: { version: string; busy: Busy; onCancel(): void; onGo(): void }) {
  const blocked = blockedBy(busy);
  const warns: string[] = [];
  if (busy.transfers > 0) {
    warns.push(
      `복사·이동 작업 ${busy.transfers}개가 아직 끝나지 않았어요. 업데이트하면 취소해요. 쓰다 만 복사본은 지우고, 다 옮기지 못한 항목의 원본은 그대로 남아요.`,
    );
  }
  if (busy.relocations > 0) {
    warns.push(
      "폴더를 다른 드라이브로 옮기거나 되돌리는 중이에요. 업데이트하면 취소해요(원본은 그대로 남아요). 이미 마지막 단계면 그 단계가 끝날 때까지 기다려요.",
    );
  }
  if (busy.links > 0) {
    warns.push(
      `중복 파일을 하드 링크로 합치는 중이에요${busy.links > 1 ? ` (${busy.links}개)` : ""}. 업데이트하면 취소해요. 이미 합친 파일은 합친 채로 남고, 합치던 파일은 마저 합치거나 손대지 않은 채로 멈춰요.`,
    );
  }
  if (busy.plan && !busy.others.plan) {
    warns.push("정리 계획을 실행하거나 되돌리는 중이에요. 업데이트하면 지금 하던 단계까지만 마치고 되돌리기 기록을 저장한 뒤 멈춰요. 남은 단계는 하지 않아요.");
  }
  if (busy.others.processes > 0) {
    warns.push(
      `fine-file이 ${busy.others.processes}개 더 실행 중이에요(다른 창, 또는 Claude 앱이 띄운 MCP 서버). 설치 프로그램이 이것들도 강제로 꺼요. 다른 창에서 복사·이동 중이면 끝난 뒤에 하거나 그 창을 먼저 닫아 주세요(쓰다 만 복사본이 남을 수 있어요).`,
    );
  }
  const stops = stopsWork(busy);
  return (
    <Dialog className="update-confirm" title="업데이트를 설치할까요?" onClose={onCancel} onBackdrop={onCancel}>
      <h3>{`v${version}${euRo(version)} 업데이트할까요?`}</h3>
      <p className="muted">업데이트 파일을 내려받아 서명을 확인한 뒤 설치 프로그램을 실행해요. 서명이 맞지 않으면 설치하지 않아요.</p>
      <ul className="update-steps">
        <li>
          Windows의 <b>UAC 창</b>(사용자 계정 컨트롤)이 한 번 떠요. Program Files에 설치된 앱이라 관리자 권한이 필요해서예요.
          "예"를 눌러 주세요.
        </li>
        <li>그러면 이 앱이 꺼지고, 짧은 설치 창이 보인 뒤 새 버전이 저절로 다시 켜져요. 스캔 결과와 열어 둔 탭은 그대로예요.</li>
        <li>
          {stops
            ? "\"아니요\"를 누르면 설치하지 않고 앱이 다시 보여요. 다만 아래 작업은 UAC 창이 뜨기 전에 멈추고, \"아니요\"를 눌러도 다시 이어지지 않아요."
            : "\"아니요\"를 누르면 설치하지 않고, 앱이 그대로 다시 보여요."}
        </li>
      </ul>
      {blocked && <div className="v04-error">{blocked}</div>}
      {!blocked && warns.length > 0 && (
        <div className="v06-warn">
          <ul className="update-warns">
            {warns.map((w) => (
              <li key={w}>{w}</li>
            ))}
          </ul>
        </div>
      )}
      <div className="actions">
        <button autoFocus={warns.length > 0 || !!blocked} onClick={onCancel}>
          {blocked ? "닫기" : "취소"}
        </button>
        {!blocked && (
          <button className="primary" autoFocus={warns.length === 0} onClick={onGo}>
            {stops ? "작업을 멈추고 업데이트" : "업데이트"}
          </button>
        )}
      </div>
    </Dialog>
  );
}

// ---- settings -----------------------------------------------------------------------

/** The update part of the settings: this version, a check now, and the automatic checks. */
export function UpdateSettings() {
  const st = useUpdater();
  const [version, setVersion] = useState("");
  const [auto, setAuto] = useState(autoCheckOn);

  useEffect(() => {
    api.appInfo().then(
      (i) => setVersion(i.version),
      () => {},
    );
  }, []);

  const toggle = (on: boolean) => {
    writeStore(AUTO_KEY, on ? "1" : "0");
    setAuto(on);
  };

  return (
    <section className="v06-section first">
      <h4>업데이트</h4>
      <p className="update-version">
        지금 버전: <b>{version ? `v${version}` : "…"}</b>
      </p>
      <div className="ai-test">
        <button className="btn" disabled={st.checking || st.stage !== null} onClick={() => void checkForUpdate(true)}>
          {st.checking ? "확인하는 중…" : "업데이트 확인"}
        </button>
        {st.stage !== null && <span className="muted">업데이트를 설치하는 중이에요</span>}
        {!st.checking && st.stage === null && st.last && (
          <span className={st.last.error ? "ai-test-bad" : "ai-test-ok"}>{st.last.text}</span>
        )}
      </div>
      <label className="v06-check update-auto">
        <input type="checkbox" checked={auto} onChange={(e) => toggle(e.target.checked)} />
        <span className="v06-check-main">
          <span>시작할 때 업데이트 확인</span>
          <span className="muted">
            켜 두면 앱을 켜고 15초쯤 뒤와, 켜 둔 동안 6시간마다 새 버전이 있는지 확인해요. 새 버전은 알려 주기만 하고, "지금
            업데이트"를 눌러야 설치해요.
          </span>
        </span>
      </label>
      <p className="v06-note">
        확인할 때는 GitHub의 fine-file 릴리스에서 최신 버전 정보(<code>latest.json</code>)만 받아 와요. 파일·폴더 정보는 보내지
        않아요. 내려받은 업데이트 파일은 앱에 들어 있는 공개 키로 서명을 확인한 뒤에만 설치해요. 설치할 때 UAC 창이 한 번 뜨고,
        앱이 꺼졌다가 새 버전으로 다시 켜져요.
      </p>
    </section>
  );
}

/** The settings' update part in a small window of its own (the sidebar's "업데이트"). */
export function UpdateSettingsDialog({ onClose }: { onClose(): void }) {
  return (
    <Dialog className="v04-dialog update-settings" title="업데이트" onClose={onClose} onBackdrop={onClose}>
      <div className="dlg-head">
        <h3>업데이트</h3>
        <button className="dlg-close" aria-label="닫기" onClick={onClose}>
          ✕
        </button>
      </div>
      <div className="dlg-body">
        <UpdateSettings />
      </div>
      <div className="dlg-foot">
        <span className="spacer" />
        <button className="btn" onClick={onClose}>
          닫기
        </button>
      </div>
    </Dialog>
  );
}
