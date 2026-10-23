import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { v04, v07, type Leftover, type Plan, type Program } from "./api";
import { ensureAdmin } from "./AdminSession";
import { Dialog } from "./Duplicates";
import { filetimeToDate, formatSize, parentOf } from "./format";
import "./components.css";
import "./v04.css";
import "./v06.css";
import "./v07.css";

// ---- installed programs and what they left behind ---------------------------------

type Tab = "programs" | "leftovers";
type SortKey = "name" | "publisher" | "size" | "lastUsed" | "installed" | "kind";
type Sort = { key: SortKey; desc: boolean };
type Filter = "all" | "stale";
type Notice = { text: string; error?: boolean };

/** "오래 안 쓴 것": not used for this long, or no record of use and installed longer ago. */
const STALE_MS = 182 * 86_400_000;

/** The way each column sorts on its first click. */
const FIRST_DESC: Record<SortKey, boolean> = {
  name: false,
  publisher: false,
  size: true,
  lastUsed: false,
  installed: true,
  kind: false,
};

const SIZE_SOURCE: Record<Program["size_source"], { label: string; title: string }> = {
  index: { label: "스캔 기준", title: "설치 폴더를 스캔한 결과로 잰 크기예요" },
  registry: {
    label: "프로그램이 알려 준 값",
    title: "프로그램이 설치할 때 Windows에 적어 둔 크기예요. 그 뒤에 늘거나 줄었으면 실제와 달라요",
  },
  unknown: { label: "크기 모름", title: "설치 폴더를 모르고, 프로그램도 크기를 적어 두지 않았어요" },
};

/** `short`: under the date in the table; the tooltip has `label` and `title`. */
const USED_SOURCE: Record<NonNullable<Program["last_used_source"]>, { label: string; short: string; title: string }> = {
  userassist: {
    label: "시작 메뉴 실행 기록",
    short: "시작 메뉴",
    title:
      "시작 메뉴·바탕 화면·작업 표시줄·탐색기에서 실행할 때 Windows가 남기는 기록이에요. 다른 프로그램이 띄운 실행은 들어가지 않아서, 실제보다 오래돼 보일 수 있어요",
  },
  prefetch: {
    label: "Windows 미리 읽기 기록",
    short: "미리 읽기",
    title: "Windows가 프로그램을 빨리 띄우려고 남기는 실행 기록(Prefetch)이에요. 어떻게 실행했든 남아요",
  },
  exe: {
    label: "실행 파일에 남은 시각",
    short: "실행 파일",
    title: "프로그램 실행 파일에 Windows가 남긴 시각이에요. 실행하지 않아도 바뀔 수 있어서 참고만 해 주세요",
  },
};

const USED_UNKNOWN = {
  label: "알 수 없음",
  short: "",
  title: "Windows에 남은 실행 기록이 없어요. 시작 메뉴 밖에서만 실행했거나, 기록이 지워졌을 수 있어요",
};

const KIND_LABEL: Record<Program["kind"], string> = { win32: "데스크톱 프로그램", store: "Microsoft Store 앱" };

const RISK: Record<Leftover["risk"], { label: string; title: string }> = {
  caution: { label: "주의", title: "남은 것으로 보이지만, 지우기 전에 안을 한 번 확인해 주세요" },
  danger: {
    label: "위험",
    title: "다른 프로그램이나 Windows가 아직 쓰는 폴더일 수 있어요. 무엇인지 확실할 때만 고르세요",
  },
};

function errText(err: unknown): string {
  if (typeof err === "string") return err;
  if (err instanceof Error) return err.message;
  try {
    return JSON.stringify(err) ?? String(err);
  } catch {
    return String(err);
  }
}

// Names come from the registry and the disk, which any program can write: direction marks in them
// are made visible, so a name cannot read as something else.
const BIDI = /[\u202a-\u202e\u2066-\u2069\u200e\u200f\u061c]/g;
/** A name as shown: the marks that turn text around are replaced (Explorer's uninstall question too). */
export const shown = (s: string) => s.replace(BIDI, "\ufffd");

function baseName(p: string): string {
  const t = p.replace(/[\\/]+$/, "");
  const i = t.lastIndexOf("\\");
  return i < 0 ? t : t.slice(i + 1);
}

/** Milliseconds since 1970 of a FILETIME, or null when not known (or before 1980). */
function msOf(ft: number | null): number | null {
  const d = ft ? filetimeToDate(ft) : null;
  return d && d.getFullYear() >= 1980 ? d.getTime() : null;
}

function day(ft: number | null): string {
  const ms = msOf(ft);
  if (ms === null) return "";
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
}

/** "오늘", "3일 전", "5개월 전", "2년 전". */
function ago(ft: number | null): string {
  const ms = msOf(ft);
  if (ms === null) return "";
  const days = Math.floor((Date.now() - ms) / 86_400_000);
  if (days < 1) return "오늘";
  if (days < 2) return "어제";
  if (days < 30) return `${days}일 전`;
  if (days < 365) return `${Math.floor(days / 30)}개월 전`;
  return `${Math.floor(days / 365)}년 전`;
}

/** Not used for six months, or never seen used and installed more than six months ago. */
function isStale(p: Program, now = Date.now()): boolean {
  const used = msOf(p.last_used);
  if (used !== null) return now - used > STALE_MS;
  const installed = msOf(p.installed_at);
  return installed !== null && now - installed > STALE_MS;
}

const byText = (a: string | null, b: string | null) => {
  if (!a || !b) return a ? -1 : b ? 1 : 0;
  return a.localeCompare(b, "ko");
};

/** Values not known go last whichever way the column sorts. */
function compare(a: Program, b: Program, s: Sort): number {
  const num = (x: number | null, y: number | null) => {
    if (x === null || y === null) return x !== null ? -1 : y !== null ? 1 : 0;
    return s.desc ? y - x : x - y;
  };
  const txt = (x: string | null, y: string | null) => {
    if (!x || !y) return byText(x, y);
    return s.desc ? byText(y, x) : byText(x, y);
  };
  let r = 0;
  switch (s.key) {
    case "name":
      r = txt(a.name, b.name);
      break;
    case "publisher":
      r = txt(a.publisher, b.publisher);
      break;
    case "size":
      r = num(a.size, b.size);
      break;
    case "lastUsed":
      r = num(msOf(a.last_used), msOf(b.last_used));
      break;
    case "installed":
      r = num(msOf(a.installed_at), msOf(b.installed_at));
      break;
    case "kind":
      r = txt(KIND_LABEL[a.kind] ?? a.kind, KIND_LABEL[b.kind] ?? b.kind);
      break;
  }
  return r || byText(a.name, b.name);
}

export function Programs({
  onClose,
  onReveal,
  onOpenPlan,
}: {
  onClose(): void;
  onReveal(path: string): void;
  /** A cleanup plan was made from the leftovers chosen: open it to review (nothing has changed yet). */
  onOpenPlan?(planId: string): void;
}) {
  const [tab, setTab] = useState<Tab>("programs");
  const alive = useRef(true);
  const openPlan = useRef(onOpenPlan);
  useEffect(() => {
    openPlan.current = onOpenPlan;
  });
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);

  // ---- installed programs ----
  const [list, setList] = useState<Program[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [listError, setListError] = useState<string | null>(null);
  /** The list shown has Windows' Prefetch record in it (read through the admin session). */
  const [withPrefetch, setWithPrefetch] = useState(false);
  const [prefetchBusy, setPrefetchBusy] = useState(false);
  const [prefetchNote, setPrefetchNote] = useState<Notice | null>(null);
  const [filter, setFilter] = useState<Filter>("all");
  const [sort, setSort] = useState<Sort>({ key: "size", desc: true });
  const [query, setQuery] = useState("");
  const [notice, setNotice] = useState<Notice | null>(null);
  const [confirm, setConfirm] = useState<Program | null>(null);
  const [starting, setStarting] = useState<string | null>(null);
  const loadSeq = useRef(0);

  const load = useCallback(async (prefetch: boolean) => {
    const seq = ++loadSeq.current;
    setLoading(true);
    setListError(null);
    try {
      const l = await v07.programs(prefetch);
      if (!alive.current || seq !== loadSeq.current) return;
      setList(l);
      setWithPrefetch(prefetch);
    } catch (err) {
      if (alive.current && seq === loadSeq.current) setListError(errText(err));
    } finally {
      if (alive.current && seq === loadSeq.current) setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load(false);
  }, [load]);

  /** Reads the list again; the Prefetch record stays in only while the admin session is still up (no new UAC prompt). */
  const reload = async () => {
    const admin = withPrefetch && (await v04.adminStatus().then((s) => s.active, () => false));
    if (!alive.current) return;
    if (withPrefetch && !admin) {
      setPrefetchNote({ text: "관리자 모드가 끝나서 이번에는 Windows 미리 읽기 기록 없이 읽었어요." });
    }
    await load(admin);
  };

  const readPrefetch = async () => {
    if (prefetchBusy) return;
    setPrefetchBusy(true);
    setPrefetchNote(null);
    try {
      let admin: boolean;
      try {
        admin = await ensureAdmin();
      } catch (err) {
        // The UAC prompt was cancelled, or the app is not installed in Program Files: the reason says.
        if (alive.current) setPrefetchNote({ text: `관리자 모드를 켜지 못했어요: ${errText(err)}`, error: true });
        return;
      }
      if (!admin) {
        if (alive.current) setPrefetchNote({ text: "관리자 모드를 켜지 못했어요.", error: true });
        return;
      }
      const l = await v07.programs(true);
      if (!alive.current) return;
      ++loadSeq.current;
      setList(l);
      setListError(null);
      setLoading(false);
      setWithPrefetch(true);
      setPrefetchNote({ text: "Windows 미리 읽기 기록까지 반영했어요. 어떻게 실행했든 마지막으로 쓴 때가 더 정확해요." });
    } catch (err) {
      if (alive.current) setPrefetchNote({ text: `미리 읽기 기록을 읽지 못했어요: ${errText(err)}`, error: true });
    } finally {
      if (alive.current) setPrefetchBusy(false);
    }
  };

  const uninstall = async (p: Program) => {
    setConfirm(null);
    if (starting) return;
    setStarting(p.id);
    setNotice(null);
    const store = p.kind === "store";
    try {
      await v07.uninstall(p.id);
      if (!alive.current) return;
      setNotice({
        // A Store app is gone when this resolves (Remove-AppxPackage, no window of its own); past
        // its time limit Windows finishes it by itself. A desktop program's uninstaller has started.
        text: store
          ? `"${shown(p.name)}"을(를) 이 계정에서 제거했어요(오래 걸리면 Windows가 뒤에서 마저 지워요). "새로 고침"을 누르면 목록에서 빠져요. 다시 쓰려면 Microsoft Store에서 설치하면 돼요.`
          : `"${shown(p.name)}"의 제거 프로그램을 실행했어요. 창이 떴으면 그 창의 안내를 따라 주세요(창 없이 바로 지우는 프로그램도 있어요). 제거가 끝나면 "새로 고침"을 눌러 목록을 다시 읽고, "남은 찌꺼기"에서 남은 폴더를 찾아볼 수 있어요.`,
      });
    } catch (err) {
      if (alive.current) {
        const why = errText(err);
        // programs.rs REMOVE_FAILED already says it for a Store app.
        const text = !store
          ? `제거 프로그램을 실행하지 못했어요: ${why}`
          : why.startsWith("앱을 지우지 못했어요")
            ? why
            : `앱을 지우지 못했어요: ${why}`;
        setNotice({ text, error: true });
      }
    } finally {
      if (alive.current) setStarting(null);
    }
  };

  const now = Date.now();
  const rows = useMemo(() => {
    if (!list) return [];
    const at = Date.now();
    const terms = query.trim().toLowerCase().split(/\s+/).filter(Boolean);
    return list
      .filter((p) => filter === "all" || isStale(p, at))
      .filter((p) => {
        if (!terms.length) return true;
        const hay = `${p.name} ${p.publisher ?? ""}`.toLowerCase();
        return terms.every((t) => hay.includes(t));
      })
      .sort((a, b) => compare(a, b, sort));
  }, [list, filter, sort, query]);

  const maxSize = rows.reduce((m, p) => Math.max(m, p.size ?? 0), 0);
  const total = rows.reduce((s, p) => s + (p.size ?? 0), 0);
  const staleCount = useMemo(() => (list ? list.filter((p) => isStale(p)).length : 0), [list]);

  const sortBy = (key: SortKey) =>
    setSort((s) => (s.key === key ? { key, desc: !s.desc } : { key, desc: FIRST_DESC[key] }));

  const head = (key: SortKey, label: string, title?: string) => (
    <th aria-sort={sort.key === key ? (sort.desc ? "descending" : "ascending") : "none"}>
      <button className={`v07-sort${sort.key === key ? " on" : ""}`} title={title} onClick={() => sortBy(key)}>
        {label}
        <span className="v07-sort-mark" aria-hidden="true">
          {sort.key === key ? (sort.desc ? "▼" : "▲") : ""}
        </span>
      </button>
    </th>
  );

  // ---- leftovers ----
  const [left, setLeft] = useState<Leftover[] | null>(null);
  const [leftLoading, setLeftLoading] = useState(false);
  const [leftError, setLeftError] = useState<string | null>(null);
  const [picked, setPicked] = useState<Set<string>>(() => new Set());
  const [planBusy, setPlanBusy] = useState(false);
  const [made, setMade] = useState<Plan | null>(null);
  const [planError, setPlanError] = useState<string | null>(null);
  const leftAsked = useRef(false);

  const loadLeft = useCallback(async () => {
    setLeftLoading(true);
    setLeftError(null);
    try {
      const l = await v07.leftovers();
      if (!alive.current) return;
      setLeft(l);
      const still = new Set(l.map((x) => x.path));
      setPicked((s) => new Set([...s].filter((p) => still.has(p))));
    } catch (err) {
      if (alive.current) setLeftError(errText(err));
    } finally {
      if (alive.current) setLeftLoading(false);
    }
  }, []);

  // Looked for the first time the tab is opened: it goes over the scanned drives.
  useEffect(() => {
    if (tab !== "leftovers" || leftAsked.current) return;
    leftAsked.current = true;
    void loadLeft();
  }, [tab, loadLeft]);

  const togglePick = (path: string) =>
    setPicked((s) => {
      const n = new Set(s);
      if (n.has(path)) n.delete(path);
      else n.add(path);
      return n;
    });

  const chosen = useMemo(() => (left ?? []).filter((x) => picked.has(x.path)), [left, picked]);
  const chosenBytes = chosen.reduce((s, x) => s + x.size, 0);
  const chosenDanger = chosen.filter((x) => x.risk === "danger").length;
  const leftTotal = (left ?? []).reduce((s, x) => s + x.size, 0);
  /** Steps of the plan made that its checks refused (saved "skipped": they never run). */
  const madeSkipped = made ? made.steps.filter((s) => s.state === "skipped").length : 0;
  const madeRunnable = made ? made.steps.length - madeSkipped : 0;

  const makePlan = async () => {
    if (planBusy || !chosen.length) return;
    setPlanBusy(true);
    setPlanError(null);
    setMade(null);
    try {
      const plan = await v07.leftoversPlan(chosen.map((x) => x.path));
      if (!alive.current) return;
      setMade(plan);
      setPicked(new Set());
      // The plan checks keep plans out of Program Files, ProgramData and AppData (plans.rs), so most
      // leftover steps are saved "skipped" and can never run. A plan with nothing it could run
      // stays a list: this window stays open, where "위치 보기" leads on.
      if (plan.steps.some((s) => s.state !== "skipped")) openPlan.current?.(plan.id);
    } catch (err) {
      if (alive.current) setPlanError(`계획을 만들지 못했어요: ${errText(err)}`);
    } finally {
      if (alive.current) setPlanBusy(false);
    }
  };

  // ---- view ----
  return (
    <Dialog className="v04-dialog v07-programs" title="프로그램 정리" onClose={onClose}>
      <div className="dlg-head">
        <h3>프로그램 정리</h3>
        <button className="dlg-close" aria-label="닫기" onClick={onClose}>
          ✕
        </button>
      </div>

      <div className="v07-tabs" role="tablist" aria-label="프로그램 정리">
        <button
          role="tab"
          id="v07-tab-programs"
          aria-controls="v07-panel-programs"
          aria-selected={tab === "programs"}
          className={`v07-tab${tab === "programs" ? " on" : ""}`}
          onClick={() => setTab("programs")}
        >
          설치된 프로그램
          {list && <span className="v07-count">{list.length.toLocaleString()}</span>}
        </button>
        <button
          role="tab"
          id="v07-tab-leftovers"
          aria-controls="v07-panel-leftovers"
          aria-selected={tab === "leftovers"}
          className={`v07-tab${tab === "leftovers" ? " on" : ""}`}
          onClick={() => setTab("leftovers")}
        >
          남은 찌꺼기
          {left && <span className="v07-count">{left.length.toLocaleString()}</span>}
        </button>
      </div>

      {tab === "programs" ? (
        <>
          <div className="dlg-body v07-body" role="tabpanel" id="v07-panel-programs" aria-labelledby="v07-tab-programs">
            <div className="v07-explain">
              <p>
                이 컴퓨터에 설치된 프로그램의 <b>크기</b>와 <b>마지막으로 쓴 때</b>를 보여 줘요. 오래 안 쓰면서 자리만 차지하는
                프로그램을 찾는 데 써요.
              </p>
              <ul>
                <li>
                  <b>제거</b>: 데스크톱 프로그램은 그 프로그램이 Windows에 등록한 제거 명령을 그대로 실행해요. 대부분은 제거
                  창이 떠서 그 안내를 따르면 되지만, 일부(Slack 같은 앱, 조용히 지우도록 등록한 프로그램)는 창 없이 바로 지워져요. Microsoft
                  Store 앱은 한 번 물은 뒤 이 계정에서 바로 제거해요. 제거 창이 따로 뜨지 않고, 앱의 데이터도 함께 지워져요.
                  fine-file이 프로그램 폴더를 직접 지우지는 않아요.
                </li>
                <li>
                  <b>크기</b>: "스캔 기준"은 설치 폴더를 스캔해 잰 값, "프로그램이 알려 준 값"은 설치할 때 적어 둔 값이라 실제와
                  다를 수 있어요.
                </li>
                <li>
                  <b>마지막 사용</b>은 Windows가 남긴 실행 기록으로 짐작한 값이에요. 칸에 마우스를 올리면 근거가 보여요.
                </li>
              </ul>
            </div>

            <div className="v07-tools">
              <div className="view-toggle" role="group" aria-label="보기">
                <button
                  className={filter === "all" && sort.key === "size" && sort.desc ? "on" : ""}
                  aria-pressed={filter === "all" && sort.key === "size" && sort.desc}
                  title="모든 프로그램을 크기가 큰 것부터"
                  onClick={() => {
                    setFilter("all");
                    setSort({ key: "size", desc: true });
                  }}
                >
                  큰 것부터
                </button>
                <button
                  className={filter === "stale" ? "on" : ""}
                  aria-pressed={filter === "stale"}
                  title="6개월 넘게 쓰지 않았거나, 쓴 기록 없이 6개월보다 전에 설치한 프로그램"
                  onClick={() => {
                    setFilter("stale");
                    setSort({ key: "lastUsed", desc: false });
                  }}
                >
                  오래 안 쓴 것{list ? ` ${staleCount.toLocaleString()}` : ""}
                </button>
              </div>
              {filter === "stale" && (
                <button className="btn small" onClick={() => setFilter("all")}>
                  모두 보기
                </button>
              )}
              <label className="field grow v07-search">
                <input
                  type="search"
                  value={query}
                  placeholder="이름이나 게시자로 찾기"
                  aria-label="프로그램 찾기"
                  spellCheck={false}
                  onChange={(e) => setQuery(e.target.value)}
                />
              </label>
              <button className="btn" disabled={loading || prefetchBusy} onClick={() => void reload()}>
                {loading ? "읽는 중…" : "새로 고침"}
              </button>
            </div>

            <div className="v07-prefetch">
              <button
                className="btn"
                disabled={prefetchBusy || loading}
                title="관리자 모드(UAC 창 한 번)로 Windows 미리 읽기(Prefetch) 기록을 읽어요. 읽기만 해요"
                onClick={() => void readPrefetch()}
              >
                {prefetchBusy ? "기록을 읽는 중…" : withPrefetch ? "사용 기록 다시 읽기 (관리자)" : "사용 기록 더 정확히 보기 (관리자)"}
              </button>
              <span className="v06-note">
                시작 메뉴 실행 기록에는 다른 방법으로 실행한 것이 빠져요. Windows 미리 읽기 기록(<code>C:\Windows\Prefetch</code>)은
                관리자만 읽을 수 있어서, 누르면 <b>관리자 모드</b>를 켜고(UAC 창이 한 번 떠요) 그 기록을 읽기만 해요. 아무것도
                바꾸지 않고, 관리자 모드는 10분 동안 쓰지 않으면 저절로 꺼져요.
              </span>
            </div>
            {prefetchNote && <div className={prefetchNote.error ? "v04-error" : "v04-ok"}>{prefetchNote.text}</div>}
            {notice && <div className={notice.error ? "v04-error" : "v04-ok"}>{notice.text}</div>}
            {listError && (
              <div className="v04-error">
                설치된 프로그램을 읽지 못했어요: {listError}{" "}
                <button className="v06-link" onClick={() => void reload()}>
                  다시 읽기
                </button>
              </div>
            )}
            {list === null && !listError && <p className="muted v04-intro">설치된 프로그램을 읽는 중…</p>}
            {list?.length === 0 && <p className="muted v04-intro">설치된 프로그램을 찾지 못했어요.</p>}
            {!!list?.length && rows.length === 0 && (
              <p className="muted v04-intro">
                {filter === "stale" && !query.trim() ? "오래 안 쓴 프로그램이 없어요." : "조건에 맞는 프로그램이 없어요."}
              </p>
            )}

            {rows.length > 0 && (
              <div className="v07-table-wrap">
                <table className="v07-table v07-prog-table">
                  <colgroup>
                    <col className="c-name" />
                    <col className="c-pub" />
                    <col className="c-size" />
                    <col className="c-used" />
                    <col className="c-inst" />
                    <col className="c-kind" />
                    <col className="c-act" />
                  </colgroup>
                  <thead>
                    <tr>
                      {head("name", "이름")}
                      {head("publisher", "게시자")}
                      {head("size", "크기")}
                      {head("lastUsed", "마지막 사용", "Windows가 남긴 실행 기록으로 짐작한 값이에요")}
                      {head("installed", "설치일")}
                      {head("kind", "종류")}
                      <th aria-label="할 일" />
                    </tr>
                  </thead>
                  <tbody>
                    {rows.map((p) => {
                      const src = SIZE_SOURCE[p.size_source] ?? SIZE_SOURCE.unknown;
                      const used = p.last_used_source ? (USED_SOURCE[p.last_used_source] ?? USED_UNKNOWN) : USED_UNKNOWN;
                      const usedDay = day(p.last_used);
                      const usedTitle =
                        `근거: ${usedDay ? used.label : USED_UNKNOWN.label}. ${usedDay ? used.title : USED_UNKNOWN.title}` +
                        (p.run_count ? ` · ${p.run_count.toLocaleString()}번 실행` : "");
                      const stale = isStale(p, now);
                      const pct = maxSize > 0 && p.size ? Math.max(1, (p.size / maxSize) * 100) : 0;
                      return (
                        <tr key={p.id}>
                          <td>
                            <div className="v07-name" title={shown(p.name)}>
                              {shown(p.name)}
                            </div>
                            {p.version && <div className="v07-sub">버전 {shown(p.version)}</div>}
                          </td>
                          <td>
                            <div className="v07-clip" title={p.publisher ? shown(p.publisher) : undefined}>
                              {p.publisher ? shown(p.publisher) : <span className="muted">모름</span>}
                            </div>
                          </td>
                          <td>
                            <div className="v07-size" title={src.title}>
                              <span className="v07-bar" style={{ width: `${pct}%` }} />
                              <span className="v07-num">{p.size !== null ? formatSize(p.size) : "—"}</span>
                            </div>
                            <div className="v07-sub" title={src.title}>
                              {src.label}
                            </div>
                          </td>
                          <td title={usedTitle}>
                            {usedDay ? (
                              <>
                                <div className={stale ? "v07-old" : undefined}>{ago(p.last_used)}</div>
                                <div className="v07-sub">
                                  {usedDay}
                                  {used.short ? ` · ${used.short}` : ""}
                                </div>
                              </>
                            ) : (
                              <>
                                <div className="muted">알 수 없음</div>
                                {stale && <div className="v07-sub v07-old">설치 뒤 쓴 기록 없음</div>}
                              </>
                            )}
                          </td>
                          <td>{day(p.installed_at) || <span className="muted">모름</span>}</td>
                          <td>
                            <div className="v07-clip">{KIND_LABEL[p.kind] ?? p.kind}</div>
                            {p.kind === "win32" && <div className="v07-sub">{p.machine_wide ? "모든 사용자" : "내 계정"}</div>}
                          </td>
                          <td className="v07-row-act">
                            <button
                              className="btn small"
                              disabled={!p.can_uninstall || starting !== null}
                              title={
                                !p.can_uninstall
                                  ? p.kind === "store"
                                    ? "Windows가 지우지 못하게 한 앱이에요"
                                    : // No uninstaller registered, one that is not there, or removing turned off (NoRemove).
                                      "여기서는 제거할 수 없어요. Windows 설정 → 앱에서 확인해 주세요"
                                  : p.kind === "store"
                                    ? "한 번 물은 뒤 이 계정에서 바로 제거해요. 제거 창이 따로 뜨지 않고, 앱의 데이터도 함께 지워져요"
                                    : "이 프로그램의 공식 제거 프로그램을 실행해요"
                              }
                              onClick={() => setConfirm(p)}
                            >
                              {starting === p.id ? (p.kind === "store" ? "제거하는 중…" : "실행하는 중…") : "제거"}
                            </button>
                            <button
                              className="btn small"
                              disabled={!p.install_location}
                              title={p.install_location ? shown(p.install_location) : "설치 폴더를 Windows에 적어 두지 않았어요"}
                              onClick={() => p.install_location && onReveal(p.install_location)}
                            >
                              폴더 보기
                            </button>
                          </td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
              </div>
            )}
          </div>
          <div className="dlg-foot">
            {list && (
              <span className="muted">
                {filter === "stale" ? "오래 안 쓴 프로그램" : "프로그램"} {rows.length.toLocaleString()}개 · 아는 크기만 합쳐{" "}
                {formatSize(total)}
                {withPrefetch ? " · 미리 읽기 기록 반영" : ""}
              </span>
            )}
            <span className="spacer" />
            <button className="btn" onClick={onClose}>
              닫기
            </button>
          </div>
        </>
      ) : (
        <>
          <div className="dlg-body v07-body" role="tabpanel" id="v07-panel-leftovers" aria-labelledby="v07-tab-leftovers">
            <div className="v07-explain">
              <p>
                지운 프로그램이 <b>남기고 간 것으로 보이는 폴더</b>예요. Program Files, ProgramData, AppData 안의 폴더 중에 지금
                설치된 프로그램과 이어지지 않는 것을 찾아요.
              </p>
              <ul>
                <li>
                  이름이 비슷한 다른 프로그램이 쓰는 폴더일 수도 있어요. "위치 보기"로 안을 확인한 뒤에 지우세요. "위험"은 특히
                  조심해 주세요.
                </li>
                <li>
                  <b>지우는 방법</b>: "위치 보기"로 폴더를 열어 안을 확인하고, 위로(Backspace) 가서 그 폴더를 고른 뒤 Delete를
                  눌러요(휴지통으로 가요). Program Files 안처럼 관리자만 바꿀 수 있는 폴더는 이 앱의 Delete로 지워지지 않고
                  이유를 알려 줘요. 그럴 때는 오른쪽 클릭 → "탐색기에서 보기"로 열어 탐색기에서 지워 주세요.
                </li>
                <li>
                  <b>정리 계획 만들기</b>는 고른 폴더를 계획(목록)으로 남겨 두기만 해요. 정리 계획은 Program Files, ProgramData,
                  AppData 안의 항목을 건드리지 않는 규칙이라, 이런 폴더의 단계는 이유와 함께 "건너뜀"으로 저장되고 실행되지
                  않아요.
                </li>
                <li>스캔한 드라이브에서만 찾아요. 크기와 날짜는 스캔(과 실시간 반영) 기준이에요.</li>
              </ul>
            </div>

            <div className="v07-tools">
              <button
                className="btn"
                disabled={!left?.some((x) => x.risk === "caution")}
                title='"위험" 항목은 직접 확인하고 하나씩 골라 주세요'
                onClick={() => setPicked(new Set((left ?? []).filter((x) => x.risk === "caution").map((x) => x.path)))}
              >
                "주의" 항목 모두 고르기
              </button>
              <button className="btn" disabled={!picked.size} onClick={() => setPicked(new Set())}>
                선택 해제
              </button>
              <span className="spacer" />
              <button className="btn" disabled={leftLoading} onClick={() => void loadLeft()}>
                {leftLoading ? "찾는 중…" : "다시 찾기"}
              </button>
            </div>

            {leftError && (
              <div className="v04-error">
                남은 폴더를 찾지 못했어요: {leftError}{" "}
                <button className="v06-link" onClick={() => void loadLeft()}>
                  다시 찾기
                </button>
              </div>
            )}
            {made && (
              <div className="v04-ok v07-made">
                <span>
                  정리 계획을 만들었어요 ({made.steps.length.toLocaleString()}단계). <b>아무것도 지우지 않았어요.</b>{" "}
                  {madeSkipped > 0 &&
                    `${madeRunnable > 0 ? `${madeSkipped.toLocaleString()}단계는` : "모든 단계가"} 정리 계획이 건드리지 않는 곳(대부분 Program Files·ProgramData·AppData 안)이라 "건너뜀"으로 저장했고, 실행되지 않아요(단계마다 이유가 있어요). 그런 폴더는 "위치 보기"로 열어 확인한 뒤 직접 Delete(휴지통)로 지워 주세요. `}
                  {madeRunnable > 0 &&
                    `${madeSkipped > 0 ? `나머지 ${madeRunnable.toLocaleString()}단계는` : "단계는"} 처음에 골라 두지 않았어요. ${
                      onOpenPlan ? "계획 창에서" : '툴바의 "계획"에서 이 계획을 열어'
                    } 확인하고 골라 "실행"을 눌러야 휴지통으로 가요.`}
                </span>
                {onOpenPlan && (
                  <button className="btn small" onClick={() => openPlan.current?.(made.id)}>
                    계획 열기
                  </button>
                )}
              </div>
            )}
            {planError && <div className="v04-error">{planError}</div>}
            {left === null && !leftError && <p className="muted v04-intro">남은 폴더를 찾는 중…</p>}
            {left?.length === 0 && (
              <p className="muted v04-intro">남은 것으로 보이는 폴더가 없어요. 스캔하지 않은 드라이브는 살펴보지 않았어요.</p>
            )}

            {!!left?.length && (
              <div className="v07-table-wrap">
                <table className="v07-table v07-left-table">
                  <colgroup>
                    <col className="c-path" />
                    <col className="c-size" />
                    <col className="c-date" />
                    <col className="c-why" />
                    <col className="c-risk" />
                    <col className="c-act" />
                  </colgroup>
                  <thead>
                    <tr>
                      <th>폴더</th>
                      <th className="num">크기</th>
                      <th>마지막 변경</th>
                      <th>이유</th>
                      <th>위험도</th>
                      <th aria-label="할 일" />
                    </tr>
                  </thead>
                  <tbody>
                    {left.map((x) => {
                      const on = picked.has(x.path);
                      const risk = RISK[x.risk] ?? RISK.danger;
                      return (
                        <tr key={x.path} className={on ? "sel" : undefined}>
                          <td>
                            <label className="v07-pick" title={shown(x.path)}>
                              <input type="checkbox" checked={on} onChange={() => togglePick(x.path)} />
                              <span className="v07-pick-main">
                                <span className="v07-name">{shown(baseName(x.path))}</span>
                                <span className="v07-sub">{shown(parentOf(x.path) ?? "")}</span>
                              </span>
                            </label>
                          </td>
                          <td className="num">{formatSize(x.size)}</td>
                          <td>
                            {day(x.modified) ? (
                              <>
                                <div>{ago(x.modified)}</div>
                                <div className="v07-sub">{day(x.modified)}</div>
                              </>
                            ) : (
                              <span className="muted">모름</span>
                            )}
                          </td>
                          <td>
                            <div className="v07-why">{shown(x.reason)}</div>
                          </td>
                          <td>
                            <span className={`v06-risk risk-${x.risk}`} title={risk.title}>
                              {risk.label}
                            </span>
                          </td>
                          <td className="v07-row-act">
                            <button className="btn small" onClick={() => onReveal(x.path)}>
                              위치 보기
                            </button>
                          </td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
              </div>
            )}
          </div>
          <div className="dlg-foot">
            {left && (
              <span className="muted">
                {chosen.length
                  ? `${chosen.length.toLocaleString()}개 선택 · ${formatSize(chosenBytes)}`
                  : `${left.length.toLocaleString()}개 · 모두 ${formatSize(leftTotal)}`}
              </span>
            )}
            {chosenDanger > 0 && (
              <span className="v07-foot-warn" title={RISK.danger.title}>
                "위험" {chosenDanger.toLocaleString()}개 포함 · "위치 보기"로 안을 꼭 확인해 주세요
              </span>
            )}
            <span className="spacer" />
            <button
              className="btn"
              disabled={!chosen.length || planBusy}
              title='고른 폴더를 정리 계획(목록)으로 남겨요. 아무것도 지우지 않아요. Program Files·ProgramData·AppData 안의 폴더는 정리 계획이 건드리지 않아서 "건너뜀"으로 저장되고 실행되지 않아요. 지우려면 "위치 보기"로 열어 Delete(휴지통)를 써 주세요'
              onClick={() => void makePlan()}
            >
              {planBusy ? "만드는 중…" : "정리 계획 만들기"}
            </button>
            <button className="btn" onClick={onClose}>
              닫기
            </button>
          </div>
        </>
      )}

      {confirm && (
        <Dialog
          className="v07-confirm"
          title={`${shown(confirm.name)} 제거`}
          onClose={() => setConfirm(null)}
          onBackdrop={() => setConfirm(null)}
        >
          <h3>{shown(confirm.name)}을(를) 제거할까요?</h3>
          {confirm.kind === "store" ? (
            // Remove-AppxPackage (programs.rs): no window of its own and nothing more is asked.
            <>
              <p className="v06-warn">
                Microsoft Store 앱은 <b>"제거"를 누르면 바로 제거돼요.</b> 제거 프로그램 창이 뜨지 않고, 더 묻지도 않아요. 이
                계정에서 앱과 함께 <b>앱이 저장해 둔 데이터</b>(설정, 앱 안에 둔 파일)도 지워지고, 휴지통으로 가지 않아요.
              </p>
              <p className="muted">
                {confirm.size !== null ? `${formatSize(confirm.size)} · ` : ""}
                다른 계정의 앱은 그대로예요. 다시 쓰려면 Microsoft Store에서 설치하면 되지만, 지워진 데이터는 돌아오지 않아요.
              </p>
            </>
          ) : (
            <>
              <p>
                이 프로그램이 Windows에 등록한 제거 명령을 그대로 실행해요. 제거 창이 뜨면 그 창의 안내를 따라 주세요.{" "}
                <b>일부 프로그램(Slack 같은 앱, 조용히 지우도록 등록한 프로그램)은 창 없이 바로 지워져요.</b>
              </p>
              {confirm.machine_wide && (
                <p className="v06-warn">
                  모든 사용자용으로 설치된 프로그램이라, 제거 프로그램이 관리자 권한(UAC 창)을 물을 수 있어요. 이 컴퓨터의 다른
                  계정에서도 없어져요.
                </p>
              )}
              <p className="muted">
                {confirm.size !== null ? `${formatSize(confirm.size)} · ` : ""}
                제거 프로그램이 묻는 것(설정 남기기, 함께 설치된 구성 요소 등)은 그 프로그램이 정해요. fine-file은 프로그램 파일을
                직접 지우지 않아요. 제거가 끝나면 "남은 찌꺼기"에서 남은 폴더를 찾아볼 수 있어요.
              </p>
            </>
          )}
          {/* 취소 has the focus: a stray Enter must not remove anything. */}
          <div className="actions">
            <button autoFocus onClick={() => setConfirm(null)}>
              취소
            </button>
            {confirm.kind === "store" ? (
              <button className="primary danger" onClick={() => void uninstall(confirm)}>
                제거
              </button>
            ) : (
              <button className="primary" onClick={() => void uninstall(confirm)}>
                제거 프로그램 실행
              </button>
            )}
          </div>
        </Dialog>
      )}
    </Dialog>
  );
}
