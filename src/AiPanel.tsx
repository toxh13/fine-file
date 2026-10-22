import { useCallback, useEffect, useRef, useState } from "react";
import { ai, plans, type AiPayloadPreview, type AiSettings as Settings, type KeyStatus } from "./api";
import { AiSettings } from "./AiSettings";
import { Dialog } from "./Duplicates";
import { folderTitle, formatSize } from "./format";
import { Markdown } from "./markdown";
import "./components.css";
import "./v04.css";
import "./v06.css";

type AiTask = "explain" | "structure" | "rename";
type RuleKind = "cleanup" | "dedupe" | "shrink" | "relocate";
/** What is running: a rule plan, or an AI task getting its preview ready or waiting for the answer. */
type Busy = { id: AiTask | RuleKind; stage: "preview" | "send" };
type Preview = { task: AiTask; payload: AiPayloadPreview; changed?: boolean };

const AI_ACTIONS: { id: AiTask; label: string; desc: string }[] = [
  { id: "explain", label: "이 폴더 설명", desc: "무엇이 든 폴더인지, 어디가 큰지 설명해 줘요" },
  { id: "structure", label: "구조 정리", desc: "하위 폴더를 만들고 나눠 옮기는 계획을 제안받아요" },
  { id: "rename", label: "이름 정리", desc: "이름을 한 가지 규칙으로 맞추는 계획을 제안받아요" },
];

const RULE_ACTIONS: { id: RuleKind; label: string; desc: string }[] = [
  { id: "cleanup", label: "정리 후보", desc: "캐시·임시 파일처럼 지워도 될 만한 항목을 찾아요. 휴지통으로만 보내요" },
  { id: "dedupe", label: "중복 정리", desc: "내용이 똑같은 파일을 하나만 남기고 휴지통으로 보내는 계획이에요" },
  { id: "shrink", label: "용량 줄이기", desc: "큰 파일을 Windows 압축으로 줄여요. 언제든 풀 수 있어요" },
  { id: "relocate", label: "다른 드라이브로 옮기기", desc: "큰 폴더를 다른 드라이브로 옮기고 원래 자리에 링크를 남겨요" },
];

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

/** A file has no plans of its own (they are made for a folder): only the explanation. */
const FILE_ACTIONS: { id: AiTask; label: string; desc: string }[] = [
  { id: "explain", label: "이 파일 설명", desc: "무엇인지, 어느 프로그램 것인지, 지워도 되는지 설명해 줘요" },
];

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

export function AiPanel({
  path,
  file = false,
  onClose,
  onOpenPlan,
}: {
  path: string;
  /** `path` is a file: it is explained, and plans (made for folders) are not offered. */
  file?: boolean;
  onClose(): void;
  onOpenPlan(planId: string): void;
}) {
  const [settings, setSettings] = useState<Settings | null>(null);
  const [keys, setKeys] = useState<KeyStatus | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [busy, setBusy] = useState<Busy | null>(null);
  const [answer, setAnswer] = useState<string | null>(null);
  const [error, setError] = useState<{ where: "ai" | "rules"; text: string } | null>(null);
  /** `changed`: made again on "보내기", it was not what had been shown (shown again, nothing sent). */
  const [preview, setPreview] = useState<Preview | null>(null);
  /** "보내기" in the preview: what goes is being made again to compare with what was shown. */
  const [checking, setChecking] = useState(false);
  const [showSettings, setShowSettings] = useState(false);
  const alive = useRef(true);
  const openPlan = useRef(onOpenPlan);
  const previewRef = useRef(preview);
  useEffect(() => {
    openPlan.current = onOpenPlan;
    previewRef.current = preview;
  });

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
    alive.current = true;
    load();
    return () => {
      alive.current = false;
    };
  }, [load]);

  // What was said about another folder does not stay.
  useEffect(() => {
    setAnswer(null);
    setError(null);
    setPreview(null);
  }, [path]);

  const send = async (task: AiTask) => {
    setPreview(null);
    setBusy({ id: task, stage: "send" });
    setError(null);
    try {
      if (task === "explain") {
        const text = await ai.explain(path);
        if (alive.current) setAnswer(text);
      } else {
        const plan = await ai.plan(task, path);
        if (alive.current) openPlan.current(plan.id);
      }
    } catch (err) {
      if (alive.current) {
        setError({ where: "ai", text: `${task === "explain" ? "설명을" : "계획을"} 받지 못했어요: ${errText(err)}` });
      }
    } finally {
      if (alive.current) setBusy(null);
    }
  };

  /**
   * With "보내기 전에 미리 보기" on, what would go is shown first and nothing is sent until the user says
   * so. Whether it is on is read from the settings as saved now: another window may have changed them
   * since this panel read them.
   */
  const ask = async (task: AiTask) => {
    if (busy || !settings) return;
    setError(null);
    if (task === "explain") setAnswer(null);
    setBusy({ id: task, stage: "preview" });
    let now: Settings;
    try {
      const [s, k] = await Promise.all([ai.getSettings(), ai.keyStatus()]);
      if (!alive.current) return;
      setSettings(s);
      setKeys(k);
      if (aiProblem(s, k, file)) {
        // The callout says what to do.
        setBusy(null);
        return;
      }
      now = s;
    } catch (err) {
      if (alive.current) {
        setError({ where: "ai", text: `AI 설정을 읽지 못해서 보내지 않았어요: ${errText(err)}` });
        setBusy(null);
      }
      return;
    }
    if (!now.confirm_before_send) {
      await send(task);
      return;
    }
    try {
      const payload = await ai.preview(task, path);
      if (alive.current) setPreview({ task, payload });
    } catch (err) {
      if (alive.current) setError({ where: "ai", text: `보낼 내용을 만들지 못했어요: ${errText(err)}` });
    } finally {
      if (alive.current) setBusy(null);
    }
  };

  /**
   * "보내기" in the preview. What goes is made again and compared with what was shown: when it is not
   * the same (files came or went, another AI was chosen meanwhile), the new one is shown instead and
   * nothing goes. Closed while that runs, nothing goes either.
   */
  const confirmSend = async (shownNow: Preview) => {
    if (checking || busy) return;
    setChecking(true);
    let again: AiPayloadPreview;
    try {
      again = await ai.preview(shownNow.task, path);
    } catch (err) {
      if (alive.current && previewRef.current === shownNow) {
        setPreview(null);
        setError({ where: "ai", text: `보낼 내용을 만들지 못해서 보내지 않았어요: ${errText(err)}` });
      }
      return;
    } finally {
      if (alive.current) setChecking(false);
    }
    if (!alive.current || previewRef.current !== shownNow) return;
    if (again.text !== shownNow.payload.text || again.provider !== shownNow.payload.provider) {
      setPreview({ task: shownNow.task, payload: again, changed: true });
      return;
    }
    await send(shownNow.task);
  };

  const rule = async (kind: RuleKind) => {
    if (busy) return;
    setBusy({ id: kind, stage: "send" });
    setError(null);
    try {
      const plan = await plans.rules(kind, path);
      if (alive.current) openPlan.current(plan.id);
    } catch (err) {
      if (alive.current) setError({ where: "rules", text: `계획을 만들지 못했어요: ${errText(err)}` });
    } finally {
      if (alive.current) setBusy(null);
    }
  };

  const problem = settings ? aiProblem(settings, keys, file) : null;
  const actions = file ? FILE_ACTIONS : AI_ACTIONS;
  const aiOff = !settings || !!problem;
  const settingsLink = (
    <button className="v06-link" onClick={() => setShowSettings(true)}>
      AI 설정 열기
    </button>
  );

  return (
    <Dialog className="v04-dialog ai-panel" title="정리 도우미" onClose={onClose}>
      <div className="dlg-head">
        <h3>정리 도우미</h3>
        <button className="dlg-close" aria-label="닫기" onClick={onClose}>
          ✕
        </button>
      </div>
      <div className="dlg-body">
        <div className="reloc-source">
          <b title={path}>{folderTitle(path)}</b>
          <span className="muted">{path}</span>
        </div>

        <section className="v06-section">
          <div className="v06-section-head">
            <h4>AI에게 묻기</h4>
            {settings && settings.provider !== "off" && (
              <span className="muted" title={providerLine(settings)}>
                보낼 곳: {providerLine(settings)}
              </span>
            )}
          </div>
          {loadError && (
            <div className="v06-callout">
              <span>AI 설정을 불러오지 못했어요: {loadError}</span>
              <button className="btn small" onClick={load}>
                다시 읽기
              </button>
            </div>
          )}
          {problem && (
            <div className="v06-callout">
              <span>{problem}</span>
              <button className="btn small" onClick={() => setShowSettings(true)}>
                AI 설정 열기
              </button>
            </div>
          )}
          <div className="ai-actions">
            {actions.map((a) => (
              <button
                key={a.id}
                className={`ai-action${busy?.id === a.id ? " busy" : ""}`}
                disabled={aiOff || busy !== null}
                onClick={() => ask(a.id)}
              >
                <b>{a.label}</b>
                <span className="muted">
                  {busy?.id !== a.id ? a.desc : busy.stage === "preview" ? "보낼 내용을 만드는 중…" : "AI에게 묻는 중…"}
                </span>
              </button>
            ))}
          </div>
          {busy?.stage === "send" && actions.some((a) => a.id === busy.id) && (
            <div className="muted ai-wait">
              AI에게 묻는 중이에요. 몇십 초 걸릴 수 있어요.{file ? "" : ' 창을 닫아도 만든 계획은 "정리 계획"에 남아요.'}
            </div>
          )}
          {error?.where === "ai" && (
            <div className="v04-error">
              {error.text} {settingsLink}
            </div>
          )}
          {answer !== null && (
            <>
              <Markdown className="ai-answer" text={answer} />
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
                  className={`ai-action${busy?.id === a.id ? " busy" : ""}`}
                  disabled={busy !== null}
                  onClick={() => rule(a.id)}
                >
                  <b>{a.label}</b>
                  <span className="muted">{busy?.id === a.id ? "만드는 중…" : a.desc}</span>
                </button>
              ))}
            </div>
            {error?.where === "rules" && <div className="v04-error">{error.text}</div>}
          </section>
        )}

        <p className="v06-note">
          {file
            ? '정리 계획은 폴더에서 만들어요. 이 파일이 든 폴더의 메뉴에서 "정리 계획 만들기"를 골라 주세요.'
            : '여기서는 계획을 만들기만 해요. 계획 창에서 단계를 확인하고 "실행"을 눌러야 파일이 바뀌고, 실행한 뒤에도 되돌릴 수 있어요.'}
        </p>
      </div>
      <div className="dlg-foot">
        <button className="btn" onClick={() => setShowSettings(true)}>
          AI 설정…
        </button>
        <span className="spacer" />
        <button className="btn" onClick={onClose}>
          닫기
        </button>
      </div>

      {preview && (
        <Dialog className="ai-preview" title="이 내용을 보낼까요?" onClose={() => setPreview(null)} onBackdrop={() => setPreview(null)}>
          <h3>이 내용을 보낼까요?</h3>
          {preview.changed && (
            <div className="v06-warn">
              그사이 보낼 내용이나 보낼 곳이 바뀌어서 보내지 않았어요. 바뀐 내용을 다시 확인해 주세요.
            </div>
          )}
          <p className="muted">
            보낼 곳: <b>{PROVIDER_LABEL[preview.payload.provider as Settings["provider"]] ?? preview.payload.provider}</b> ·{" "}
            {formatSize(preview.payload.bytes)}. 아래 내용이 그대로 가요. {contentsNote(preview.payload.provider)}
          </p>
          <pre className="v06-code ai-preview-text">{preview.payload.text}</pre>
          <p className="v06-note">
            "보내기"를 누르면 보낼 내용을 다시 만들어 이것과 같을 때만 보내요. AI 설정에서 "보내기 전에 미리 보기"를 끄면 묻지 않고
            보내요.
          </p>
          <div className="actions">
            <button onClick={() => setPreview(null)}>보내지 않기</button>
            <button className="primary" autoFocus disabled={checking} onClick={() => void confirmSend(preview)}>
              {checking ? "확인하는 중…" : "보내기"}
            </button>
          </div>
        </Dialog>
      )}

      {showSettings && (
        <AiSettings
          onClose={() => {
            setShowSettings(false);
            load();
          }}
        />
      )}
    </Dialog>
  );
}
