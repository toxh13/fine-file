import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import {
  ai,
  places,
  plans,
  type AiPayloadPreview,
  type DriveInfo,
  type Plan,
  type RulesResult,
  type AiSettings as Settings,
  type KeyStatus,
} from "./api";
import { tip } from "./Explorer";
import { formatSize, normalizePath, pathKey, shown } from "./format";
import { Markdown } from "./markdown";
import "./components.css";
import "./v04.css";
import "./v06.css";

// ---- 정리 도우미: a tab of the side pane (beside 상세 정보) that follows where you are ----

type AiTask = "explain" | "structure" | "rename";
type RuleKind = "cleanup" | "dedupe" | "shrink" | "relocate";

/** What the helper looks at: the one item picked, else the folder shown (or what was pinned). */
export interface HelperTarget {
  path: string;
  name: string;
  is_dir: boolean;
}

/**
 * Where the rule plans look ("정리할 곳"): what the helper looks at, the profile folder, one
 * drive (`drive:C:`), or every indexed local drive (one plan per drive).
 */
export type ScopeId = "here" | "home" | "all" | `drive:${string}`;

/** One drive's part of a "컴퓨터 전체" run: its plan, or why there is none. */
interface RuleRun {
  /** The drive ("C:"). */
  label: string;
  plan: Plan | null;
  text: string;
  error: boolean;
}

/**
 * The request out (one at a time in a tab) and whose it is (`key`: the item's pathKey, or
 * `scope:<id>` for a rule plan made for a 정리할 곳 other than "지금 보는 곳"): a rule plan, or an
 * AI task getting its preview ready, making it again on "보내기", or waiting for the answer.
 */
interface Busy {
  key: string;
  name: string;
  id: AiTask | RuleKind;
  stage: "preview" | "check" | "send";
  /** "컴퓨터 전체": the drive it is on now. */
  at?: string;
}

/** What would go ("보내기 전에 미리 보기"). `changed`: made again on "보내기", it was not what had been shown (shown again, nothing sent). */
interface SendPreview {
  task: AiTask;
  payload: AiPayloadPreview;
  changed?: boolean;
}

/** What the helper got for one item, kept until the tab closes: going back to it shows it again. */
interface Memo {
  answer: string | null;
  error: { where: "ai" | "rules"; text: string } | null;
  preview: SendPreview | null;
  /**
   * The last plan made for it, from the AI or the rules. It opens by itself only while the pane still
   * shows this item and nothing is being typed (see Explorer); else it waits here under "계획 열기".
   */
  made: { id: string; label: string; where: "ai" | "rules" } | null;
  /** "컴퓨터 전체": each local drive's result, in the drives' order (filled in as it goes). */
  runs: RuleRun[] | null;
  /** A rule plan found nothing (none was saved): why, one reason a line. */
  nothing: string | null;
  /** The rule `runs` and `nothing` are for. */
  ruleKind: RuleKind | null;
}

const NO_MEMO: Memo = { answer: null, error: null, preview: null, made: null, runs: null, nothing: null, ruleKind: null };

const AI_ACTIONS: { id: AiTask; label: string; desc: string }[] = [
  { id: "explain", label: "이 폴더 설명", desc: "무엇이 든 폴더인지, 어디가 큰지 설명해 줘요" },
  { id: "structure", label: "구조 정리", desc: "하위 폴더를 만들고 나눠 옮기는 계획을 제안받아요" },
  { id: "rename", label: "이름 정리", desc: "이름을 한 가지 규칙으로 맞추는 계획을 제안받아요" },
];

/** A file has no plans of its own (they are made for a folder): only the explanation. */
const FILE_ACTIONS: { id: AiTask; label: string; desc: string }[] = [
  { id: "explain", label: "이 파일 설명", desc: "무엇인지, 어느 프로그램 것인지, 지워도 되는지 설명해 줘요" },
];

const RULE_ACTIONS: { id: RuleKind; label: string; desc: string }[] = [
  { id: "cleanup", label: "정리 후보", desc: "캐시·임시 파일처럼 지워도 될 만한 항목을 찾아요. 휴지통으로만 보내요" },
  { id: "dedupe", label: "중복 정리", desc: "내용이 똑같은 파일을 하나만 남기고 휴지통으로 보내는 계획이에요" },
  { id: "shrink", label: "용량 줄이기", desc: "큰 파일을 Windows 압축으로 줄여요. 언제든 풀 수 있어요" },
  { id: "relocate", label: "다른 드라이브로 옮기기", desc: "큰 폴더를 다른 드라이브로 옮기고 원래 자리에 링크를 남겨요" },
];

const isRule = (id: Busy["id"]) => RULE_ACTIONS.some((a) => a.id === id);
const labelOf = (id: Busy["id"]) =>
  RULE_ACTIONS.find((a) => a.id === id)?.label ?? AI_ACTIONS.find((a) => a.id === id)?.label ?? "정리";

const PROVIDER_LABEL: Record<Settings["provider"], string> = {
  off: "끔",
  claude: "Claude API",
  openai: "OpenAI API",
  ollama: "Ollama (내 PC)",
  claude_cli: "Claude Code CLI",
  codex_cli: "Codex CLI",
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

/** The text shown when a rule plan found nothing: the summary, then up to three reasons candidates were left out. */
export function rulesNothingText(r: RulesResult): string {
  return [r.summary, ...r.dropped.slice(0, 3).map((d) => `후보 ${d.count.toLocaleString()}개: ${d.why}`)].join("\n");
}

// ---- 정리할 곳: where the rule plans look ----

const SCOPE_KEY = "fine-file.helperScope";

const isScope = (v: unknown): v is ScopeId =>
  v === "here" || v === "home" || v === "all" || (typeof v === "string" && /^drive:[A-Z]:$/.test(v));

/** The 정리할 곳 chosen last on this PC; "지금 보는 곳" when none was, or it cannot be read. */
function savedScope(): ScopeId {
  try {
    const v = localStorage.getItem(SCOPE_KEY);
    return isScope(v) ? v : "here";
  } catch {
    return "here";
  }
}

/**
 * The drives a rule plan may cover: this PC's own disks. A SUBST drive is left out: it is a folder
 * of another drive (its files would be in two plans), and the shell never recycles on it.
 */
const localDrives = (drives: DriveInfo[]) =>
  drives.filter((d) => (d.kind === "fixed" || d.kind === "removable") && !d.subst);

/** What "컴퓨터 전체" covers: the fixed drives. A removable drive usually has no Recycle Bin; it keeps its own chip. */
const computerDrives = (drives: DriveInfo[]) => localDrives(drives).filter((d) => d.kind === "fixed");

/**
 * The 정리할 곳 used now: the one chosen, or "지금 보는 곳" while it cannot be used (the profile
 * folder not known yet, the drive gone or not indexed, no local drive indexed). The choice itself
 * is kept: it is used again once it can be.
 */
function liveScope(scope: ScopeId, drives: DriveInfo[], home: string | null): ScopeId {
  if (scope === "home") return home ? scope : "here";
  if (scope === "all") return computerDrives(drives).some((d) => d.indexed) ? scope : "here";
  if (scope.startsWith("drive:")) {
    const letter = scope.slice("drive:".length);
    return localDrives(drives).some((d) => d.letter.toUpperCase() === letter && d.indexed) ? scope : "here";
  }
  return "here";
}

/** Where a 정리할 곳's results are kept: under the item for "지금 보는 곳", else under the scope (they stay while you go elsewhere). */
const ruleKey = (scope: ScopeId, t: HelperTarget | null) =>
  scope === "here" ? (t ? pathKey(t.path) : "") : `scope:${scope}`;

/** A 정리할 곳 in a few words (as Busy.name: shown() when drawn). */
function scopeName(scope: ScopeId, t: HelperTarget | null): string {
  if (scope === "here") return t ? t.name : "지금 보는 곳";
  if (scope === "home") return "홈 폴더";
  if (scope === "all") return "컴퓨터 전체";
  return scope.slice("drive:".length);
}

const stepsSize = (plan: Plan) => plan.steps.reduce((sum, s) => sum + s.size, 0);

/**
 * What a preview says about file contents. The app sends none; Codex CLI is an agent with tools of
 * its own (commands in a read-only sandbox, whatever its own settings add) and can read files itself.
 */
export function contentsNote(provider: string): string {
  return provider === "codex_cli"
    ? "앱은 파일 내용을 보내지 않지만, Codex CLI는 스스로 도구를 써서 이 PC의 파일을 읽고 그 내용을 OpenAI로 보낼 수 있어요. AI 설정의 Codex 주의를 확인해 주세요."
    : "파일 내용은 보내지 않아요.";
}

/** Where a request would go, for the line over the AI buttons. */
function providerLine(s: Settings): string {
  const name = PROVIDER_LABEL[s.provider] ?? s.provider;
  switch (s.provider) {
    case "claude":
    case "claude_cli":
      return `${name} · ${s.claude_model || "claude-opus-5-5"}`;
    case "openai":
      return s.openai_model ? `${name} · ${s.openai_model}` : name;
    case "ollama":
      return s.ollama_model ? `${name} · ${s.ollama_model}` : name;
    case "codex_cli":
      return `${name} · ${s.codex_model || "gpt-5.6-luna"}`;
    default:
      return name;
  }
}

/** Why the AI buttons cannot be used now, in words the user can act on; null when they can. */
function aiProblem(s: Settings, k: KeyStatus | null, file: boolean): string | null {
  switch (s.provider) {
    case "off":
      return file
        ? "AI가 꺼져 있어요. AI 설정에서 쓸 AI를 고르면 쓸 수 있어요."
        : "AI가 꺼져 있어요. AI 설정에서 쓸 AI를 고르면 쓸 수 있어요. 아래의 규칙으로 만드는 계획은 AI 없이 동작해요.";
    case "claude":
      return k && !k.claude ? "Claude API 키가 없어요. AI 설정에서 키를 넣어 주세요." : null;
    case "openai":
      if (k && !k.openai) return "OpenAI API 키가 없어요. AI 설정에서 키를 넣어 주세요.";
      return s.openai_model.trim() ? null : "OpenAI 모델 이름이 비어 있어요. AI 설정에서 적어 주세요.";
    case "ollama":
      return s.ollama_model.trim() ? null : "Ollama 모델 이름이 비어 있어요. AI 설정에서 적어 주세요.";
    default:
      return null;
  }
}

export interface Helper {
  settings: Settings | null;
  keys: KeyStatus | null;
  loadError: string | null;
  load: () => void;
  busy: Busy | null;
  /** What it got for the item at `path` (nothing yet: all empty). */
  memo: (path: string) => Memo;
  /** What the rule plans got for the 정리할 곳 `scope`: for "지금 보는 곳", the item `t`'s memo. */
  rulesMemo: (scope: ScopeId, t: HelperTarget) => Memo;
  ask: (t: HelperTarget, task: AiTask) => void;
  confirmSend: (t: HelperTarget, shown: SendPreview) => void;
  dropPreview: (t: HelperTarget) => void;
  /** "정리할 곳", as chosen (remembered on this PC); see liveScope for the one used now. */
  scope: ScopeId;
  setScope: (id: ScopeId) => void;
  /** The profile folder ("홈 폴더"), once known_places answered. */
  home: string | null;
  /** known_places answered without the profile folder, or failed. */
  homeMissing: boolean;
  /** A rule plan for the 정리할 곳 chosen: `t` for "지금 보는 곳", `drives` for a drive or "컴퓨터 전체". */
  rule: (t: HelperTarget | null, kind: RuleKind, drives: DriveInfo[]) => void;
}

/**
 * One tab's 정리 도우미: what it got for each item (until the tab closes) and the one request it may
 * have out. It lives with the tab, not with the pane: the pane closed or the tab hidden, a request
 * goes on, and its answer is kept under its own item. `shown`: the pane shows the helper; the
 * settings are read then, and again when `stamp` changes (App read them again). `onPlan`: a plan
 * was made (rules, or an AI's) for `t` in `scope` ("here" for an AI's), maybe long after it was
 * asked for. `onSaved`: "컴퓨터 전체" saved one drive's plan (only the largest is given to `onPlan`,
 * at the end).
 */
export function useHelper(
  shown: boolean,
  stamp: unknown,
  onPlan: (planId: string, t: HelperTarget | null, scope: ScopeId) => void,
  onSaved?: () => void,
): Helper {
  const [settings, setSettings] = useState<Settings | null>(null);
  const [keys, setKeys] = useState<KeyStatus | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [busy, setBusy] = useState<Busy | null>(null);
  const [scope, setScopeNow] = useState<ScopeId>(savedScope);
  const [home, setHome] = useState<string | null>(null);
  const [homeMissing, setHomeMissing] = useState(false);
  // The requests read and write these as they go, past many renders; `tick` draws what they wrote.
  const busyRef = useRef<Busy | null>(null);
  const memos = useRef(new Map<string, Memo>());
  const [, setTick] = useState(0);
  const alive = useRef(true);
  const planRef = useRef(onPlan);
  const savedRef = useRef(onSaved);
  const homeAsked = useRef(false);
  useLayoutEffect(() => {
    planRef.current = onPlan;
    savedRef.current = onSaved;
  });

  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);

  const setScope = useCallback((id: ScopeId) => {
    setScopeNow(id);
    try {
      localStorage.setItem(SCOPE_KEY, id);
    } catch {
      /* storage blocked: the choice lasts until the tab closes */
    }
  }, []);

  const load = useCallback(() => {
    // The profile folder is asked for once (again after a failure).
    if (!homeAsked.current) {
      homeAsked.current = true;
      places.known().then(
        (list) => {
          if (!alive.current) return;
          const h = list.find((p) => p.id === "home");
          const found = h && typeof h.path === "string" ? normalizePath(h.path) : null;
          setHome(found);
          setHomeMissing(!found);
        },
        () => {
          homeAsked.current = false;
          if (alive.current) setHomeMissing(true);
        },
      );
    }
    ai.getSettings().then(
      (s) => {
        if (!alive.current) return;
        setSettings(s);
        setLoadError(null);
      },
      (err) => alive.current && setLoadError(errText(err)),
    );
    ai.keyStatus().then(
      (k) => alive.current && setKeys(k),
      () => {},
    );
  }, []);

  useEffect(() => {
    if (shown) load();
  }, [shown, stamp, load]);

  const memoOf = (key: string) => memos.current.get(key) ?? NO_MEMO;
  const patch = (key: string, m: Partial<Memo>) => {
    memos.current.set(key, { ...memoOf(key), ...m });
    if (alive.current) setTick((n) => n + 1);
  };
  const start = (b: Busy) => {
    busyRef.current = b;
    if (alive.current) setBusy(b);
  };
  const begin = (t: HelperTarget, id: Busy["id"], stage: Busy["stage"]) =>
    start({ key: pathKey(t.path), name: t.name, id, stage });
  const end = () => {
    busyRef.current = null;
    if (alive.current) setBusy(null);
  };

  /** Sends `task` about `t` (its preview, when one was shown, agreed to). The caller holds the turn and ends it. */
  const send = async (t: HelperTarget, task: AiTask) => {
    const key = pathKey(t.path);
    patch(key, { preview: null, error: null });
    begin(t, task, "send");
    try {
      if (task === "explain") {
        patch(key, { answer: await ai.explain(t.path) });
      } else {
        const plan = await ai.plan(task, t.path);
        patch(key, { made: { id: plan.id, label: labelOf(task), where: "ai" } });
        if (alive.current) planRef.current(plan.id, t, "here");
      }
    } catch (err) {
      patch(key, { error: { where: "ai", text: `${task === "explain" ? "설명을" : "계획을"} 받지 못했어요: ${errText(err)}` } });
    }
  };

  /**
   * With "보내기 전에 미리 보기" on, what would go is shown first and nothing is sent until the user says
   * so. Whether it is on is read from the settings as saved now: another window may have changed them
   * since the pane read them.
   */
  const ask = async (t: HelperTarget, task: AiTask) => {
    if (busyRef.current || !settings) return;
    const key = pathKey(t.path);
    patch(key, { error: null, preview: null, made: null, ...(task === "explain" ? { answer: null } : {}) });
    begin(t, task, "preview");
    try {
      let now: Settings;
      try {
        const [s, k] = await Promise.all([ai.getSettings(), ai.keyStatus()]);
        // The tab closed meanwhile: nothing goes (no answer would be seen, no plan opened).
        if (!alive.current) return;
        setSettings(s);
        setKeys(k);
        // The callout says what to do.
        if (aiProblem(s, k, !t.is_dir)) return;
        now = s;
      } catch (err) {
        patch(key, { error: { where: "ai", text: `AI 설정을 읽지 못해서 보내지 않았어요: ${errText(err)}` } });
        return;
      }
      if (!now.confirm_before_send) {
        await send(t, task);
        return;
      }
      try {
        patch(key, { preview: { task, payload: await ai.preview(task, t.path) } });
      } catch (err) {
        patch(key, { error: { where: "ai", text: `보낼 내용을 만들지 못했어요: ${errText(err)}` } });
      }
    } finally {
      end();
    }
  };

  /**
   * "보내기" in the preview. What goes is made again and compared with what was shown: when it is not
   * the same (files came or went, another AI was chosen meanwhile), the new one is shown instead and
   * nothing goes. "보내지 않기" while that runs, nothing goes either.
   */
  const confirmSend = async (t: HelperTarget, shownNow: SendPreview) => {
    if (busyRef.current) return;
    const key = pathKey(t.path);
    begin(t, shownNow.task, "check");
    try {
      let again: AiPayloadPreview;
      try {
        again = await ai.preview(shownNow.task, t.path);
      } catch (err) {
        if (memoOf(key).preview === shownNow) {
          patch(key, { preview: null, error: { where: "ai", text: `보낼 내용을 만들지 못해서 보내지 않았어요: ${errText(err)}` } });
        }
        return;
      }
      if (!alive.current || memoOf(key).preview !== shownNow) return;
      if (again.text !== shownNow.payload.text || again.provider !== shownNow.payload.provider) {
        patch(key, { preview: { task: shownNow.task, payload: again, changed: true } });
        return;
      }
      await send(t, shownNow.task);
    } finally {
      end();
    }
  };

  /**
   * A rule plan for the 정리할 곳 used now (liveScope). One folder: its plan opens, or why there is
   * none is said. "컴퓨터 전체": one plan per indexed fixed drive, one drive after another (each reads
   * that drive's index, some kinds its files), the largest of them opened at the end; the whole run
   * is one request.
   */
  const rule = async (t: HelperTarget | null, kind: RuleKind, drives: DriveInfo[]) => {
    if (busyRef.current) return;
    const s = liveScope(scope, drives, home);
    const key = ruleKey(s, t);
    const name = scopeName(s, t);
    if (s === "all") {
      // Moving to "another drive" means nothing for every drive at once (the buttons say so).
      if (kind === "relocate") return;
      const runs: RuleRun[] = [];
      patch(key, { error: null, made: null, nothing: null, runs: [], ruleKind: kind });
      try {
        for (const d of computerDrives(drives)) {
          if (!d.indexed) {
            runs.push({ label: d.letter, plan: null, text: "스캔 안 함 (먼저 스캔해 주세요)", error: false });
            patch(key, { runs: [...runs] });
            continue;
          }
          // The tab closed meanwhile: no other drive is started (no plan would be opened).
          if (!alive.current) break;
          start({ key, name, id: kind, stage: "send", at: d.letter });
          try {
            const r = await plans.rules(kind, `${d.letter}\\`);
            // Saved: the list of plans hears of it now (the largest opens at the end).
            if (r.plan && alive.current) savedRef.current?.();
            runs.push(
              r.plan
                ? {
                    label: d.letter,
                    plan: r.plan,
                    text: `${r.plan.steps.length.toLocaleString()}개 · ${formatSize(stepsSize(r.plan))}`,
                    error: false,
                  }
                : { label: d.letter, plan: null, text: rulesNothingText(r).split("\n")[0], error: false },
            );
          } catch (err) {
            runs.push({ label: d.letter, plan: null, text: `만들지 못했어요: ${errText(err)}`, error: true });
          }
          patch(key, { runs: [...runs] });
        }
        patch(key, { runs: [...runs] });
        let best: Plan | null = null;
        for (const r of runs) if (r.plan && (!best || stepsSize(r.plan) > stepsSize(best))) best = r.plan;
        if (best && alive.current) planRef.current(best.id, t, s);
      } finally {
        end();
      }
      return;
    }
    const at = s === "here" ? (t?.is_dir ? t.path : null) : s === "home" ? home : `${s.slice("drive:".length)}\\`;
    if (!at) return;
    patch(key, { error: null, made: null, nothing: null, runs: null, ruleKind: kind });
    start({ key, name, id: kind, stage: "send" });
    try {
      const r = await plans.rules(kind, at);
      if (!r.plan) {
        patch(key, { nothing: rulesNothingText(r) });
      } else {
        patch(key, { made: { id: r.plan.id, label: labelOf(kind), where: "rules" } });
        if (alive.current) planRef.current(r.plan.id, t, s);
      }
    } catch (err) {
      patch(key, { error: { where: "rules", text: `계획을 만들지 못했어요: ${errText(err)}` } });
    } finally {
      end();
    }
  };

  return {
    settings,
    keys,
    loadError,
    load,
    busy,
    memo: (path) => memoOf(pathKey(path)),
    rulesMemo: (s, t) => memoOf(ruleKey(s, t)),
    ask: (t, task) => void ask(t, task),
    confirmSend: (t, shownNow) => void confirmSend(t, shownNow),
    dropPreview: (t) => patch(pathKey(t.path), { preview: null }),
    scope,
    setScope,
    home,
    homeMissing,
    rule: (t, kind, drives) => void rule(t, kind, drives),
  };
}

export interface HelperPaneProps {
  helper: Helper;
  /** What it looks at; null when there is nothing here (see `why`). */
  target: HelperTarget | null;
  /** Why there is nothing to look at here: search results with no one item picked, a folder read with administrator rights. */
  why: string | null;
  /** "이 항목에 고정": the target stays while you go elsewhere. */
  pinned: boolean;
  onPin: (on: boolean) => void;
  /** "AI 설정…": the tab's AI settings dialog. */
  onSettings: () => void;
  /** "계획 열기": a plan made here, in its window. */
  onOpenPlan: (planId: string) => void;
  /** This PC's drives: the 정리할 곳 chips, and what "컴퓨터 전체" covers. */
  drives: DriveInfo[];
}

/** The 정리 도우미 tab of the side pane: AI explanations, plans from an AI or from rules, for its target. */
export function HelperPane(p: HelperPaneProps) {
  const h = p.helper;
  const t = p.target;
  if (!t) return <div className="preview-empty">{p.why}</div>;

  const key = pathKey(t.path);
  const m = h.memo(t.path);
  const file = !t.is_dir;
  // The 정리할 곳 used now, and what the rule plans got there (for "지금 보는 곳": this item's).
  const scope = liveScope(h.scope, p.drives, h.home);
  const rm = h.rulesMemo(scope, t);
  // A file has no plans of its own: "지금 보는 곳" makes none for it, another 정리할 곳 does.
  const rulesOff = file && scope === "here";
  // The request out is this item's AI request, a rule plan for the 정리할 곳 used now, or another
  // (whose answer is kept under it; a rule plan for this item counts as another while a different
  // 정리할 곳 is chosen, so the banner says what is running).
  const here = h.busy?.key === key && !isRule(h.busy.id) ? h.busy : null;
  const hereRule = h.busy?.key === ruleKey(scope, t) && isRule(h.busy.id) ? h.busy : null;
  const other = h.busy && !here && !hereRule ? h.busy : null;
  const waiting = h.busy !== null;
  const problem = h.settings ? aiProblem(h.settings, h.keys, file) : null;
  const actions = file ? FILE_ACTIONS : AI_ACTIONS;
  const aiOff = !h.settings || !!problem;
  const settingsLink = (
    <button className="v06-link" onClick={p.onSettings}>
      AI 설정 열기
    </button>
  );
  const made = (of: Memo, where: "ai" | "rules") =>
    of.made?.where === where && (
      <p className="helper-made">
        "{of.made.label}" 계획을 만들었어요. 계획 창에서 확인하고 실행해야 바뀌어요.{" "}
        <button className="v06-link" onClick={() => of.made && p.onOpenPlan(of.made.id)}>
          계획 열기
        </button>
      </p>
    );
  const canAll = liveScope("all", p.drives, h.home) === "all";

  return (
    <div className="details-scroll helper">
      <div className="helper-target">
        <div className="helper-target-row">
          <span className="helper-mark" aria-hidden="true">
            📍
          </span>
          <b title={shown(t.path)}>{shown(t.name)}</b>
          {/* Not "고정" alone: that is the sidebar's 즐겨찾기 (즐겨찾기에 고정). */}
          <button
            type="button"
            className={`btn small helper-pin${p.pinned ? " on" : ""}`}
            aria-pressed={p.pinned}
            aria-label="정리 도우미를 이 항목에 고정"
            onClick={() => p.onPin(!p.pinned)}
            {...tip("helperPin")}
          >
            이 항목에 고정
          </button>
        </div>
        <span className="muted helper-path">{shown(t.path)}</span>
        <span className="muted helper-follow">
          {p.pinned
            ? "이 항목에 고정했어요. 다른 곳으로 가도 이 항목을 봐요"
            : "항목을 하나 고르면 그 항목을, 아니면 지금 폴더를 봐요"}
        </span>
      </div>

      {other && (
        <div className="helper-busy" role="status">
          ⟳ {shown(other.name)}
          {isRule(other.id) ? "의 계획을 만드는 중…" : "에 대해 묻는 중…"}
          <span className="muted">
            {isRule(other.id)
              ? " 끝나면 여기서도 쓸 수 있어요. 만든 계획은 정리 계획 목록에 남아요."
              : other.id === "explain"
                ? ` 끝나면 여기서도 물을 수 있어요. 그 답은 ${shown(other.name)}에 남아요.`
                : " 끝나면 여기서도 물을 수 있어요. 받은 계획은 정리 계획 목록에 남아요."}
          </span>
        </div>
      )}

      <section className="v06-section">
        <div className="v06-section-head">
          <h4>AI에게 묻기</h4>
          {h.settings && h.settings.provider !== "off" && (
            <span className="muted" title={providerLine(h.settings)}>
              보낼 곳: {providerLine(h.settings)}
            </span>
          )}
        </div>
        {h.loadError && (
          <div className="v06-callout">
            <span>AI 설정을 불러오지 못했어요: {h.loadError}</span>
            <button className="btn small" onClick={h.load}>
              다시 읽기
            </button>
          </div>
        )}
        {problem && (
          <div className="v06-callout">
            <span>{problem}</span>
            <button className="btn small" onClick={p.onSettings}>
              AI 설정 열기
            </button>
          </div>
        )}
        <div className="ai-actions">
          {actions.map((a) => (
            <button
              key={a.id}
              className={`ai-action${here?.id === a.id ? " busy" : ""}`}
              disabled={aiOff || waiting}
              onClick={() => h.ask(t, a.id)}
            >
              <b>{a.label}</b>
              <span className="muted">
                {here?.id !== a.id
                  ? a.desc
                  : here.stage === "preview"
                    ? "보낼 내용을 만드는 중…"
                    : here.stage === "check"
                      ? "보낼 내용을 확인하는 중…"
                      : "AI에게 묻는 중…"}
              </span>
            </button>
          ))}
        </div>
        {here?.stage === "send" && !isRule(here.id) && (
          <div className="muted ai-wait">
            AI에게 묻는 중이에요. 몇십 초 걸릴 수 있어요. 다른 곳으로 가거나 옆 창을 닫아도 계속 묻고,{" "}
            {here.id === "explain"
              ? "답은 이 항목에 남아요."
              : '받은 계획은 여기 그대로 있으면 바로 열리고, 아니면 여기의 "계획 열기"와 "정리 계획 목록"에 남아요.'}
          </div>
        )}
        {m.error?.where === "ai" && (
          <div className="v04-error">
            {shown(m.error.text)} {settingsLink}
          </div>
        )}
        {m.preview && (
          <SendBox
            preview={m.preview}
            label={actions.find((a) => a.id === m.preview?.task)?.label ?? labelOf(m.preview.task)}
            checking={here?.stage === "check"}
            waiting={waiting}
            onDrop={() => h.dropPreview(t)}
            onSend={(shownNow) => h.confirmSend(t, shownNow)}
          />
        )}
        {made(m, "ai")}
        {m.answer !== null && (
          <>
            <Markdown className="ai-answer" text={m.answer} />
            <p className="v06-note">
              AI가 쓴 설명이라 틀릴 수 있어요. 앱은 {file ? "파일" : "폴더"} 정보(이름, 크기, 날짜, 분류)만 보냈어요.
            </p>
          </>
        )}
      </section>

      <section className="v06-section">
        <div className="v06-section-head">
          <h4>규칙으로 계획 만들기</h4>
          <span className="muted">AI 없이 이 컴퓨터에서 만들어요</span>
        </div>
        <ScopeChips
          target={t}
          scope={scope}
          home={h.home}
          homeMissing={h.homeMissing}
          drives={p.drives}
          onScope={h.setScope}
        />
        <div className="ai-actions">
          {RULE_ACTIONS.map((a) => (
            <button
              key={a.id}
              className={`ai-action${hereRule?.id === a.id ? " busy" : ""}`}
              disabled={waiting || rulesOff || (a.id === "relocate" && scope === "all")}
              onClick={() => h.rule(t, a.id, p.drives)}
            >
              <b>{a.label}</b>
              <span className="muted">{ruleDesc(a, scope, hereRule)}</span>
            </button>
          ))}
        </div>
        {rm.error?.where === "rules" && <div className="v04-error">{shown(rm.error.text)}</div>}
        {rm.nothing !== null && (
          <div className="helper-nothing" role="status">
            <p>{shown(rm.nothing)}</p>
            {scope !== "all" && rm.ruleKind !== "relocate" && (
              <button
                type="button"
                className="btn small"
                disabled={!canAll}
                title={canAll ? undefined : "먼저 드라이브를 스캔해 주세요"}
                onClick={() => h.setScope("all")}
              >
                범위를 컴퓨터 전체로
              </button>
            )}
          </div>
        )}
        {rm.runs && <RuleRuns runs={rm.runs} kind={rm.ruleKind} onOpenPlan={p.onOpenPlan} />}
        {made(rm, "rules")}
      </section>

      <p className="v06-note">
        {rulesOff
          ? '정리 계획은 폴더에서 만들어요. 이 파일이 든 폴더의 메뉴에서 "정리 계획 만들기"를 골라 주세요.'
          : '여기서는 계획을 만들기만 해요. 계획 창에서 단계를 확인하고 "실행"을 눌러야 파일이 바뀌고, 실행한 뒤에도 되돌릴 수 있어요.'}
      </p>
      <div className="helper-foot">
        <button className="btn" onClick={p.onSettings}>
          AI 설정…
        </button>
      </div>
    </div>
  );
}

/** A rule button's second line: what it does here, or how far its request got. */
function ruleDesc(a: (typeof RULE_ACTIONS)[number], scope: ScopeId, busy: Busy | null): string {
  if (busy?.id === a.id) return busy.at ? `${busy.at} ${a.label} 계획을 만드는 중…` : "만드는 중…";
  if (a.id === "relocate" && scope === "all") return "컴퓨터 전체에서는 쓸 수 없어요. 드라이브를 골라 주세요";
  // Files of the same size are read to compare: on a whole drive that takes a while, and it cannot be stopped.
  if (a.id === "dedupe" && (scope === "all" || scope.startsWith("drive:"))) return `${a.desc}. 드라이브 전체는 몇 분 걸릴 수 있어요.`;
  return a.desc;
}

/**
 * "정리할 곳": the chips over the rule buttons (what the helper looks at, the profile folder, each
 * local drive, the whole computer), and the folders the one used now covers. A drive not indexed
 * cannot be chosen; nor can "컴퓨터 전체" while no local drive is.
 */
function ScopeChips(p: {
  target: HelperTarget;
  /** The one used now (liveScope). */
  scope: ScopeId;
  home: string | null;
  homeMissing: boolean;
  drives: DriveInfo[];
  onScope: (id: ScopeId) => void;
}) {
  const local = localDrives(p.drives);
  const computer = computerDrives(p.drives);
  const ready = computer.filter((d) => d.indexed).map((d) => d.letter);
  const unscanned = computer.filter((d) => !d.indexed).map((d) => d.letter);
  const chips: { id: ScopeId; label: string; title?: string; off: boolean }[] = [
    { id: "here", label: `지금 보는 곳 · ${shown(p.target.name)}`, title: shown(p.target.path), off: false },
    {
      id: "home",
      label: "홈 폴더",
      title: p.home ? shown(p.home) : p.homeMissing ? "홈 폴더를 찾지 못했어요" : undefined,
      off: !p.home,
    },
    ...local.map((d) => ({
      id: `drive:${d.letter.toUpperCase()}` as const,
      label: d.letter,
      title: !d.indexed ? "먼저 이 드라이브를 스캔해 주세요" : d.label ? `${shown(d.label)} (${d.letter})` : undefined,
      off: !d.indexed,
    })),
    { id: "all", label: "컴퓨터 전체", title: ready.length ? "고정 드라이브만 봐요. 이동식 드라이브는 따로 골라 주세요" : "먼저 드라이브를 스캔해 주세요", off: !ready.length },
  ];
  const where =
    p.scope === "here"
      ? shown(p.target.path)
      : p.scope === "home"
        ? shown(p.home ?? "")
        : p.scope === "all"
          ? `${ready.join(", ")} 드라이브${unscanned.length ? ` (${unscanned.join(", ")} 스캔 안 함)` : ""}`
          : `${p.scope.slice("drive:".length)}\\`;
  return (
    <>
      <div className="helper-scope" role="radiogroup" aria-label="정리할 곳">
        <span className="muted">정리할 곳:</span>
        {chips.map((c) => (
          <button
            key={c.id}
            type="button"
            role="radio"
            aria-checked={p.scope === c.id}
            className={`btn small helper-chip${p.scope === c.id ? " on" : ""}`}
            disabled={c.off}
            title={c.title}
            onClick={() => p.onScope(c.id)}
          >
            {c.label}
          </button>
        ))}
      </div>
      <span className="muted helper-scope-where">{where}</span>
    </>
  );
}

/** "컴퓨터 전체": each local drive's line (its plan's size, or why there is none) and its plan to open. */
function RuleRuns(p: { runs: RuleRun[]; kind: RuleKind | null; onOpenPlan: (planId: string) => void }) {
  if (!p.runs.length) return null;
  return (
    <>
      {p.runs.some((r) => r.plan) && (
        <p className="helper-made">
          {p.kind ? `"${labelOf(p.kind)}" ` : ""}계획을 드라이브마다 따로 만들었어요. 계획 창에서 확인하고 실행해야 바뀌어요.
        </p>
      )}
      <ul className="helper-runs">
        {p.runs.map((r) => (
          <li key={r.label} className={r.error ? "error" : undefined}>
            <b>{r.label}</b>
            <span>{shown(r.text)}</span>
            {r.plan && (
              <button className="v06-link" onClick={() => r.plan && p.onOpenPlan(r.plan.id)}>
                계획 열기
              </button>
            )}
          </li>
        ))}
      </ul>
    </>
  );
}

/**
 * "이 내용을 보낼까요?", in the pane: exactly what would go, before anything does. It comes into
 * sight when it shows, but takes no focus: Enter in the list must not send it.
 */
function SendBox(p: {
  preview: SendPreview;
  /** The action it was asked for ("이 폴더 설명", "구조 정리"). */
  label: string;
  /** "보내기" makes what goes again to compare. */
  checking: boolean;
  /** A request is out (this one, or another item's). */
  waiting: boolean;
  onDrop: () => void;
  onSend: (shownNow: SendPreview) => void;
}) {
  const box = useRef<HTMLDivElement>(null);
  useEffect(() => {
    box.current?.scrollIntoView({ block: "nearest" });
  }, [p.preview]);
  const { payload, changed } = p.preview;
  return (
    <div ref={box} className="helper-send" role="group" aria-label="이 내용을 보낼까요?">
      <h4>이 내용을 보낼까요?</h4>
      {changed && (
        <div className="v06-warn">그사이 보낼 내용이나 보낼 곳이 바뀌어서 보내지 않았어요. 바뀐 내용을 다시 확인해 주세요.</div>
      )}
      <p className="muted">
        {p.label} · 보낼 곳: <b>{PROVIDER_LABEL[payload.provider as Settings["provider"]] ?? payload.provider}</b> ·{" "}
        {formatSize(payload.bytes)}. 아래 내용이 그대로 가요. {contentsNote(payload.provider)}
      </p>
      <pre className="v06-code ai-preview-text">{shown(payload.text)}</pre>
      <p className="v06-note">
        "보내기"를 누르면 보낼 내용을 다시 만들어 이것과 같을 때만 보내요. AI 설정에서 "보내기 전에 미리 보기"를 끄면 묻지 않고
        보내요.
      </p>
      <div className="actions">
        <button onClick={p.onDrop}>보내지 않기</button>
        <button className="primary" disabled={p.waiting} onClick={() => p.onSend(p.preview)}>
          {p.checking ? "확인하는 중…" : "보내기"}
        </button>
      </div>
    </div>
  );
}
