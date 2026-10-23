import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { ai, plans, type AiPayloadPreview, type AiSettings as Settings, type KeyStatus } from "./api";
import { tip } from "./Explorer";
import { formatSize, pathKey } from "./format";
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
 * The request out (one at a time in a tab) and whose it is (`key`: the item's pathKey): a rule
 * plan, or an AI task getting its preview ready, making it again on "보내기", or waiting for the answer.
 */
interface Busy {
  key: string;
  name: string;
  id: AiTask | RuleKind;
  stage: "preview" | "check" | "send";
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
}

const NO_MEMO: Memo = { answer: null, error: null, preview: null, made: null };

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
  ask: (t: HelperTarget, task: AiTask) => void;
  confirmSend: (t: HelperTarget, shown: SendPreview) => void;
  dropPreview: (t: HelperTarget) => void;
  rule: (t: HelperTarget, kind: RuleKind) => void;
}

/**
 * One tab's 정리 도우미: what it got for each item (until the tab closes) and the one request it may
 * have out. It lives with the tab, not with the pane: the pane closed or the tab hidden, a request
 * goes on, and its answer is kept under its own item. `shown`: the pane shows the helper; the
 * settings are read then, and again when `stamp` changes (App read them again). `onPlan`: a plan
 * was made (rules, or an AI's) for `t`, maybe long after it was asked for.
 */
export function useHelper(shown: boolean, stamp: unknown, onPlan: (planId: string, t: HelperTarget) => void): Helper {
  const [settings, setSettings] = useState<Settings | null>(null);
  const [keys, setKeys] = useState<KeyStatus | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [busy, setBusy] = useState<Busy | null>(null);
  // The requests read and write these as they go, past many renders; `tick` draws what they wrote.
  const busyRef = useRef<Busy | null>(null);
  const memos = useRef(new Map<string, Memo>());
  const [, setTick] = useState(0);
  const alive = useRef(true);
  const planRef = useRef(onPlan);
  useLayoutEffect(() => {
    planRef.current = onPlan;
  });

  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);

  const load = useCallback(() => {
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
  const begin = (t: HelperTarget, id: Busy["id"], stage: Busy["stage"]) => {
    const b = { key: pathKey(t.path), name: t.name, id, stage };
    busyRef.current = b;
    if (alive.current) setBusy(b);
  };
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
        if (alive.current) planRef.current(plan.id, t);
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

  const rule = async (t: HelperTarget, kind: RuleKind) => {
    if (busyRef.current) return;
    const key = pathKey(t.path);
    patch(key, { error: null, made: null });
    begin(t, kind, "send");
    try {
      const plan = await plans.rules(kind, t.path);
      patch(key, { made: { id: plan.id, label: labelOf(kind), where: "rules" } });
      if (alive.current) planRef.current(plan.id, t);
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
    ask: (t, task) => void ask(t, task),
    confirmSend: (t, shownNow) => void confirmSend(t, shownNow),
    dropPreview: (t) => patch(pathKey(t.path), { preview: null }),
    rule: (t, kind) => void rule(t, kind),
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
}

/** The 정리 도우미 tab of the side pane: AI explanations, plans from an AI or from rules, for its target. */
export function HelperPane(p: HelperPaneProps) {
  const h = p.helper;
  const t = p.target;
  if (!t) return <div className="preview-empty">{p.why}</div>;

  const key = pathKey(t.path);
  const m = h.memo(t.path);
  const file = !t.is_dir;
  // The request out is this item's, or another's (whose answer is kept under it).
  const here = h.busy?.key === key ? h.busy : null;
  const other = h.busy && !here ? h.busy : null;
  const waiting = h.busy !== null;
  const problem = h.settings ? aiProblem(h.settings, h.keys, file) : null;
  const actions = file ? FILE_ACTIONS : AI_ACTIONS;
  const aiOff = !h.settings || !!problem;
  const settingsLink = (
    <button className="v06-link" onClick={p.onSettings}>
      AI 설정 열기
    </button>
  );
  const made = (where: "ai" | "rules") =>
    m.made?.where === where && (
      <p className="helper-made">
        "{m.made.label}" 계획을 만들었어요. 계획 창에서 확인하고 실행해야 바뀌어요.{" "}
        <button className="v06-link" onClick={() => m.made && p.onOpenPlan(m.made.id)}>
          계획 열기
        </button>
      </p>
    );

  return (
    <div className="details-scroll helper">
      <div className="helper-target">
        <div className="helper-target-row">
          <span className="helper-mark" aria-hidden="true">
            📍
          </span>
          <b title={t.path}>{t.name}</b>
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
        <span className="muted helper-path">{t.path}</span>
        <span className="muted helper-follow">
          {p.pinned
            ? "이 항목에 고정했어요. 다른 곳으로 가도 이 항목을 봐요"
            : "항목을 하나 고르면 그 항목을, 아니면 지금 폴더를 봐요"}
        </span>
      </div>

      {other && (
        <div className="helper-busy" role="status">
          ⟳ {other.name}
          {isRule(other.id) ? "의 계획을 만드는 중…" : "에 대해 묻는 중…"}
          <span className="muted">
            {isRule(other.id)
              ? " 끝나면 여기서도 쓸 수 있어요. 만든 계획은 정리 계획 목록에 남아요."
              : other.id === "explain"
                ? ` 끝나면 여기서도 물을 수 있어요. 그 답은 ${other.name}에 남아요.`
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
            {m.error.text} {settingsLink}
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
        {made("ai")}
        {m.answer !== null && (
          <>
            <Markdown className="ai-answer" text={m.answer} />
            <p className="v06-note">
              AI가 쓴 설명이라 틀릴 수 있어요. 앱은 {file ? "파일" : "폴더"} 정보(이름, 크기, 날짜, 분류)만 보냈어요.
            </p>
          </>
        )}
      </section>

      {!file && (
        <section className="v06-section">
          <div className="v06-section-head">
            <h4>규칙으로 계획 만들기</h4>
            <span className="muted">AI 없이 이 컴퓨터에서 만들어요</span>
          </div>
          <div className="ai-actions">
            {RULE_ACTIONS.map((a) => (
              <button
                key={a.id}
                className={`ai-action${here?.id === a.id ? " busy" : ""}`}
                disabled={waiting}
                onClick={() => h.rule(t, a.id)}
              >
                <b>{a.label}</b>
                <span className="muted">{here?.id === a.id ? "만드는 중…" : a.desc}</span>
              </button>
            ))}
          </div>
          {m.error?.where === "rules" && <div className="v04-error">{m.error.text}</div>}
          {made("rules")}
        </section>
      )}

      <p className="v06-note">
        {file
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
      <pre className="v06-code ai-preview-text">{payload.text}</pre>
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
