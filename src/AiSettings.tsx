import { useCallback, useEffect, useId, useRef, useState } from "react";
import { ai, isLocalUrl, judgeModel, judgeOf, type AiProvider, type AiSettings as Settings, type JudgeEngine, type KeyStatus } from "./api";
import { Dialog } from "./Duplicates";
import { shown } from "./format";
import "./components.css";
import "./v04.css";
import "./v06.css";

type KeyName = "claude" | "openai" | "jev";
type Notice = { text: string; error?: boolean };

const DEFAULT_CLAUDE = "claude-opus-5-5";
const DEFAULT_CODEX = "gpt-5.6-luna";
const DEFAULT_OLLAMA = "http://localhost:11434";

const PROVIDERS: { id: AiProvider; label: string; note: string }[] = [
  { id: "off", label: "끄기", note: "AI를 쓰지 않아요" },
  { id: "claude", label: "Claude API", note: "내 Anthropic API 키" },
  { id: "openai", label: "OpenAI API", note: "내 OpenAI API 키" },
  { id: "ollama", label: "Ollama (내 PC)", note: "이 컴퓨터에서 실행" },
  { id: "claude_cli", label: "Claude Code CLI", note: "내가 설치·로그인한 claude" },
  { id: "codex_cli", label: "Codex CLI", note: "내가 설치·로그인한 codex" },
];

/** v0.10: who judges for the 위험도 badges, AI 검색 and the plan's second check. */
const JUDGES: { id: JudgeEngine; label: string; note: string }[] = [
  { id: "jev", label: "jev", note: "유료, 빠름, 키 필요" },
  { id: "ollama", label: "로컬 모델 · Ollama", note: "무료, 이 PC에서, 처음엔 설치 필요" },
];

const CLAUDE_MODELS = [
  { id: "claude-opus-5-5", label: "claude-opus-5-5 (기본)" },
  { id: "claude-sonnet-5-5", label: "claude-sonnet-5-5" },
  { id: "claude-haiku-5-5", label: "claude-haiku-5-5" },
];

const EFFORTS: { id: Settings["effort"]; label: string }[] = [
  { id: "low", label: "낮음 (빠르고 적게 써요)" },
  { id: "medium", label: "보통" },
  { id: "high", label: "높음 (느리고 많이 써요)" },
];

const EXE = "C:\\Program Files\\fine-file\\fine-file.exe";
const MCP_JSON = JSON.stringify({ mcpServers: { "fine-file": { command: EXE, args: ["--mcp"] } } }, null, 2);
const MCP_CMD = `claude mcp add fine-file -- "${EXE}" --mcp`;

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
 * Puts text on the clipboard. The fallback's textarea goes inside `host` (the dialog): one outside
 * would lose the focus to the dialog, which keeps it in.
 */
async function copyText(text: string, host: HTMLElement): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    // below
  }
  const before = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  const ta = document.createElement("textarea");
  ta.value = text;
  ta.readOnly = true;
  ta.style.position = "fixed";
  ta.style.opacity = "0";
  ta.style.pointerEvents = "none";
  host.appendChild(ta);
  try {
    ta.select();
    return document.execCommand("copy");
  } catch {
    return false;
  } finally {
    ta.remove();
    before?.focus();
  }
}

function CopyBlock({ text, label }: { text: string; label: string }) {
  const [state, setState] = useState<"idle" | "done" | "fail">("idle");
  const box = useRef<HTMLDivElement>(null);
  const timer = useRef<number | null>(null);
  useEffect(
    () => () => {
      if (timer.current !== null) clearTimeout(timer.current);
    },
    [],
  );

  const copy = async () => {
    if (!box.current) return;
    const ok = await copyText(text, box.current);
    setState(ok ? "done" : "fail");
    if (timer.current !== null) clearTimeout(timer.current);
    timer.current = window.setTimeout(() => {
      timer.current = null;
      setState("idle");
    }, 1800);
  };

  return (
    <div ref={box} className="v06-code-wrap">
      <pre className="v06-code" aria-label={label}>
        {text}
      </pre>
      <button type="button" className="btn small v06-copy" onClick={copy}>
        {state === "done" ? "복사했어요" : state === "fail" ? "복사하지 못했어요" : "복사"}
      </button>
    </div>
  );
}

/** One API key: typed in a password field, kept by the backend, never shown again. */
function KeyField({
  name,
  label,
  placeholder,
  has,
  onChanged,
}: {
  name: KeyName;
  label: string;
  placeholder: string;
  /** Whether a key is saved; undefined while not known. */
  has: boolean | undefined;
  onChanged(): void;
}) {
  const [value, setValue] = useState("");
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<Notice | null>(null);
  const alive = useRef(true);
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);

  const store = async (key: string | null) => {
    if (busy) return;
    setBusy(true);
    setMsg(null);
    try {
      await ai.setKey(name, key);
      if (!alive.current) return;
      setValue("");
      setMsg({ text: key ? "키를 저장했어요" : "키를 지웠어요" });
      onChanged();
    } catch (err) {
      if (alive.current) setMsg({ text: `${key ? "저장하지" : "지우지"} 못했어요: ${errText(err)}`, error: true });
    } finally {
      if (alive.current) setBusy(false);
    }
  };

  return (
    <form
      className="ai-key"
      onSubmit={(e) => {
        e.preventDefault();
        const k = value.trim();
        if (k) store(k);
      }}
    >
      <label className="field">
        <span>
          {label}
          <span className={`ai-key-state${has ? " on" : ""}`}>{has === undefined ? "확인 중" : has ? "저장됨" : "없음"}</span>
        </span>
        <input
          type="password"
          value={value}
          autoComplete="off"
          spellCheck={false}
          placeholder={has ? "바꾸려면 새 키를 넣어 주세요" : placeholder}
          disabled={busy}
          onChange={(e) => setValue(e.target.value)}
        />
      </label>
      <button type="submit" className="btn" disabled={busy || !value.trim()}>
        저장
      </button>
      <button type="button" className="btn" disabled={busy || !has} onClick={() => store(null)}>
        지우기
      </button>
      {msg && <div className={`ai-key-msg${msg.error ? " error" : ""}`}>{msg.text}</div>}
    </form>
  );
}

/**
 * `onSaved`: settings or a key were saved (the window stays open): whoever keeps a copy of them
 * reads them again now, so what is sent next goes where they now say (위험도 that is on, a search).
 */
export function AiSettings({ onClose, onSaved }: { onClose(): void; onSaved?: () => void }) {
  /** As last loaded or saved; `draft` is what the page shows. */
  const [saved, setSaved] = useState<Settings | null>(null);
  const [draft, setDraft] = useState<Settings | null>(null);
  const [keys, setKeys] = useState<KeyStatus | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [notice, setNotice] = useState<Notice | null>(null);
  const [testing, setTesting] = useState<string | null>(null);
  const [tested, setTested] = useState<{ provider: string; text: string; error?: boolean } | null>(null);
  const [askClose, setAskClose] = useState(false);
  // v0.10: the models the Ollama at the address shown has (null: not read, or it did not answer).
  const [models, setModels] = useState<string[] | null>(null);
  const [modelsBusy, setModelsBusy] = useState(false);
  const [modelsError, setModelsError] = useState<string | null>(null);
  const modelsReq = useRef(0);
  const alive = useRef(true);
  const group = useId();
  const judgeGroup = useId();

  const dirty = !!draft && !!saved && JSON.stringify(draft) !== JSON.stringify(saved);

  const loadKeys = useCallback(() => {
    ai.keyStatus().then(
      (k) => alive.current && setKeys(k),
      (err) => alive.current && setNotice({ text: `저장한 키를 확인하지 못했어요: ${errText(err)}`, error: true }),
    );
  }, []);
  // As of the last render: the callbacks below are made once.
  const savedHook = useRef(onSaved);
  useEffect(() => {
    savedHook.current = onSaved;
  });
  /** A key was saved or removed: read again here, and by whoever keeps a copy (onSaved). */
  const keyChanged = useCallback(() => {
    loadKeys();
    savedHook.current?.();
  }, [loadKeys]);

  useEffect(() => {
    alive.current = true;
    ai.getSettings().then(
      (s) => {
        if (!alive.current) return;
        setSaved(s);
        setDraft(s);
      },
      (err) => alive.current && setLoadError(errText(err)),
    );
    loadKeys();
    return () => {
      alive.current = false;
    };
  }, [loadKeys]);

  const set = <K extends keyof Settings>(k: K, v: Settings[K]) => setDraft((d) => (d ? { ...d, [k]: v } : d));

  const pick = (p: AiProvider) =>
    setDraft((d) => {
      if (!d) return d;
      const n = { ...d, provider: p };
      if ((p === "claude" || p === "claude_cli") && !n.claude_model.trim()) n.claude_model = DEFAULT_CLAUDE;
      if (p === "codex_cli" && !n.codex_model.trim()) n.codex_model = DEFAULT_CODEX;
      if (p === "ollama" && !n.ollama_url.trim()) n.ollama_url = DEFAULT_OLLAMA;
      return n;
    });

  /** The judge engine (v0.10). A local model needs an address: this PC's, unless one is there. */
  const pickJudge = (j: JudgeEngine) =>
    setDraft((d) => {
      if (!d) return d;
      const n = { ...d, judge: j };
      if (j === "ollama" && !n.ollama_url.trim()) n.ollama_url = DEFAULT_OLLAMA;
      return n;
    });

  /** The models the Ollama at `url` has (ollama list), for the 판정 모델 choice. The newest read wins. */
  const loadModels = useCallback((url: string) => {
    const req = ++modelsReq.current;
    setModelsBusy(true);
    ai.ollamaModels(url.trim() || null).then(
      (list) => {
        if (!alive.current || req !== modelsReq.current) return;
        setModels([...list].sort());
        setModelsError(null);
      },
      (err) => {
        if (!alive.current || req !== modelsReq.current) return;
        setModels(null);
        setModelsError(errText(err));
      },
    ).finally(() => alive.current && req === modelsReq.current && setModelsBusy(false));
  }, []);

  // Read as the local model is chosen, and again a moment after its address is changed.
  const judgeUrl = draft && judgeOf(draft) === "ollama" ? draft.ollama_url.trim() : null;
  useEffect(() => {
    if (judgeUrl === null) return;
    const t = setTimeout(() => loadModels(judgeUrl), 400);
    return () => clearTimeout(t);
  }, [judgeUrl, loadModels]);

  /** True when what the page shows is what is saved. */
  const save = async (): Promise<boolean> => {
    if (!draft || !saved) return false;
    if (!dirty) return true;
    const s: Settings = {
      ...draft,
      claude_model: draft.claude_model.trim(),
      openai_model: draft.openai_model.trim(),
      ollama_url: draft.ollama_url.trim(),
      ollama_model: draft.ollama_model.trim(),
      codex_model: draft.codex_model.trim(),
      judge: judgeOf(draft),
      judge_model: (draft.judge_model ?? "").trim(),
    };
    setSaving(true);
    setNotice(null);
    try {
      await ai.setSettings(s);
      savedHook.current?.();
      if (alive.current) {
        setSaved(s);
        setDraft((d) => (d === draft ? s : d));
        setNotice({ text: "설정을 저장했어요" });
      }
      return true;
    } catch (err) {
      if (alive.current) setNotice({ text: `설정을 저장하지 못했어요: ${errText(err)}`, error: true });
      return false;
    } finally {
      if (alive.current) setSaving(false);
    }
  };

  /**
   * Saves what changed first: the test tries the settings as saved. "judge": the judge engine as a
   * whole (v0.10; for a local model, Ollama answers and has the model).
   */
  const test = async (provider: string) => {
    if (testing || saving) return;
    setTested(null);
    if (!(await save())) return;
    setTesting(provider);
    try {
      const text = provider === "judge" ? await ai.judgeTest() : await ai.test(provider);
      if (alive.current) setTested({ provider, text: text || "연결했어요" });
    } catch (err) {
      if (alive.current) setTested({ provider, text: errText(err), error: true });
    } finally {
      if (alive.current) setTesting(null);
    }
  };

  const close = () => (dirty ? setAskClose(true) : onClose());

  const p = draft?.provider ?? "off";
  const judge: JudgeEngine = draft ? judgeOf(draft) : "jev";
  const testLine = (provider: string) =>
    tested?.provider === provider && <span className={tested.error ? "ai-test-bad" : "ai-test-ok"}>{tested.text}</span>;
  const claudeModels =
    draft && draft.claude_model && !CLAUDE_MODELS.some((m) => m.id === draft.claude_model)
      ? [...CLAUDE_MODELS, { id: draft.claude_model, label: draft.claude_model }]
      : CLAUDE_MODELS;

  return (
    <Dialog className="v04-dialog ai-settings" title="AI 설정" onClose={close}>
      <div className="dlg-head">
        <h3>AI 설정</h3>
        <button className="dlg-close" aria-label="닫기" onClick={close}>
          ✕
        </button>
      </div>
      <div className="dlg-body">
        {loadError && <div className="v04-error">AI 설정을 불러오지 못했어요: {loadError}</div>}
        {!draft && !loadError && <p className="muted v04-intro">불러오는 중…</p>}
        {draft && (
          <>
            <p className="muted v06-lead">
              AI는 고를 때만 쓰는 선택 기능이에요. 꺼 두어도 탐색, 검색, 크기 지도, 규칙으로 만드는 정리 계획까지 모든 기능이
              그대로 동작해요.
            </p>

            <section className="v06-section first">
              <h4>쓸 AI</h4>
              <div className="ai-providers" role="radiogroup" aria-label="쓸 AI">
                {PROVIDERS.map((o) => (
                  <label key={o.id} className={`ai-provider${p === o.id ? " on" : ""}`}>
                    <input type="radio" name={group} checked={p === o.id} onChange={() => pick(o.id)} />
                    <span className="ai-provider-main">
                      <span>{o.label}</span>
                      <span className="muted">{o.note}</span>
                    </span>
                  </label>
                ))}
              </div>

              {p === "off" && (
                <p className="v06-note">AI에 아무것도 보내지 않아요. 아래의 MCP 연결은 이 설정과 따로 동작해요.</p>
              )}

              {(p === "claude" || p === "claude_cli") && (
                <div className="ai-fields">
                  <label className="field">
                    <span>모델</span>
                    <select value={draft.claude_model || DEFAULT_CLAUDE} onChange={(e) => set("claude_model", e.target.value)}>
                      {claudeModels.map((m) => (
                        <option key={m.id} value={m.id}>
                          {m.label}
                        </option>
                      ))}
                    </select>
                  </label>
                  <label className="field">
                    <span>생각 깊이 (effort)</span>
                    <select value={draft.effort} onChange={(e) => set("effort", e.target.value as Settings["effort"])}>
                      {EFFORTS.map((o) => (
                        <option key={o.id} value={o.id}>
                          {o.label}
                        </option>
                      ))}
                    </select>
                  </label>
                </div>
              )}
              {p === "claude" && (
                <>
                  <KeyField name="claude" label="Claude API 키" placeholder="sk-ant-…" has={keys?.claude} onChanged={keyChanged} />
                  <p className="v06-note">
                    Anthropic Console에서 만든 API 키를 넣어 주세요. 쓴 만큼 Anthropic이 키 주인에게 청구해요.
                  </p>
                </>
              )}
              {p === "claude_cli" && (
                <p className="v06-note">
                  직접 설치하고 로그인한 Claude Code(<code>claude</code> 명령)를 불러요. 앱은 로그인 정보(<code>~/.claude</code>)를
                  읽거나 저장하지 않아요. 사용량은 그 CLI에 로그인한 계정의 한도와 요금을 따라요.
                </p>
              )}

              {p === "openai" && (
                <>
                  <div className="ai-fields">
                    <label className="field">
                      <span>모델</span>
                      <input
                        value={draft.openai_model}
                        spellCheck={false}
                        placeholder="OpenAI 모델 ID"
                        onChange={(e) => set("openai_model", e.target.value)}
                      />
                    </label>
                  </div>
                  <KeyField name="openai" label="OpenAI API 키" placeholder="sk-…" has={keys?.openai} onChanged={keyChanged} />
                  <p className="v06-note">
                    OpenAI Platform에서 만든 API 키를 넣어 주세요. ChatGPT 구독과는 따로, 쓴 만큼 OpenAI가 키 주인에게 청구해요.
                  </p>
                </>
              )}

              {p === "ollama" && (
                <>
                  <div className="ai-fields">
                    <label className="field">
                      <span>주소</span>
                      <input
                        value={draft.ollama_url}
                        spellCheck={false}
                        placeholder={DEFAULT_OLLAMA}
                        onChange={(e) => set("ollama_url", e.target.value)}
                      />
                    </label>
                    <label className="field">
                      <span>모델</span>
                      <input
                        value={draft.ollama_model}
                        spellCheck={false}
                        placeholder="ollama list에 보이는 이름"
                        onChange={(e) => set("ollama_model", e.target.value)}
                      />
                    </label>
                  </div>
                  {draft.ollama_url.trim() && !isLocalUrl(draft.ollama_url) && (
                    <div className="v06-warn">
                      이 주소는 이 컴퓨터가 아니에요. 폴더 정보가 그 컴퓨터로 가요. 내 PC에서만 쓰려면{" "}
                      <code>{DEFAULT_OLLAMA}</code>로 두세요.
                    </div>
                  )}
                  <p className="v06-note">
                    Ollama를 설치하고 모델을 받아 두세요(<code>ollama pull 모델이름</code>). 요금이 없고, 주소가 이 컴퓨터면 폴더
                    정보가 밖으로 나가지 않아요.
                  </p>
                </>
              )}

              {p === "codex_cli" && (
                <>
                  <div className="ai-fields">
                    <label className="field">
                      <span>모델</span>
                      <input
                        value={draft.codex_model}
                        spellCheck={false}
                        placeholder={DEFAULT_CODEX}
                        onChange={(e) => set("codex_model", e.target.value)}
                      />
                    </label>
                  </div>
                  <p className="v06-note">
                    직접 설치하고 로그인한 Codex CLI(<code>codex</code> 명령)를 불러요. 앱은 로그인 정보(<code>auth.json</code>)를
                    읽지 않아요. 사용량은 그 CLI에 로그인한 계정의 한도와 요금을 따라요.
                  </p>
                  <div className="v06-warn">
                    Codex는 스스로 도구를 쓰는 에이전트예요. 앱은 폴더 정보만 넘기고, Codex를 내{" "}
                    <code>~/.codex/config.toml</code> 없이 실행해요. 거기 넣어 둔 MCP 서버·플러그인·프로필·직접 넣은 모델 공급자는 쓰지
                    않아서, 직접 넣은 공급자로만 쓰고 있었다면 실패하거나 기본 공급자(OpenAI)로 갈 수 있어요. 셸 명령·웹 검색 같은 도구
                    기능도 끄고 남는 것은 읽기 전용 샌드박스에서 돌게 하지만, Codex가 가진 도구를 앱이 모두 끈다고 보장할 수 없어요.
                    Codex가 그 샌드박스 안에서 이 PC의 파일을 읽고 그 내용을 OpenAI로 보낼 수 있다고 생각하고 써 주세요.
                  </div>
                </>
              )}

              {p !== "off" && (
                <div className="ai-test">
                  <button className="btn" disabled={!!testing || saving} onClick={() => test(p)}>
                    {testing === p ? "시험하는 중…" : "연결 시험"}
                  </button>
                  {dirty && !testing && <span className="muted">바꾼 설정을 먼저 저장해요</span>}
                  {testLine(p)}
                </div>
              )}
            </section>

            <section className="v06-section">
              <h4>보내는 정보</h4>
              <label className="v06-check">
                <input
                  type="checkbox"
                  checked={draft.confirm_before_send}
                  onChange={(e) => set("confirm_before_send", e.target.checked)}
                />
                <span className="v06-check-main">
                  <span>보내기 전에 미리 보기</span>
                  <span className="muted">AI에 보내기 전에 보낼 내용을 그대로 보여 주고 물어봐요.</span>
                </span>
              </label>
              <p className="v06-note">
                앱은 파일 내용을 보내지 않아요. 경로, 이름, 크기, 날짜, 분류만 보내요{p === "codex_cli" ? " (Codex CLI는 위의 주의처럼 스스로 파일을 읽을 수 있어요)" : ""}.
                미리 보기에서 "보내기"를 누르면 보낼 내용을 다시 만들어, 보여 준 것과 같을 때만 보내요. AI가 낸 계획은 바로 실행하지
                않아요. 직접 확인하고 실행을 눌러야 하고, 실행할 때 앱이 단계마다 다시 확인해요.
              </p>
            </section>

            <section className="v06-section">
              <h4>AI 판정 (위험도 배지, AI 검색)</h4>
              <p className="v06-note">
                <b>AI 판정은 무엇인가요?</b> 항목마다 지워도 되는지(안전·주의·위험)를 판정하고, 찾는 말에 맞는 항목인지도 판정해요.
                위에서 고른 AI와는 따로라서, AI를 꺼 둔 채로 판정만 쓸 수도 있어요. 판정하는 쪽(엔진)은 둘 중에서 골라요:{" "}
                <b>jev</b>(TypeSafe의 유료 서비스, 빠름, 내 키)와 <b>로컬 모델</b>(Ollama, 무료, 이 PC에서). 이 앱에서는 세 곳에 써요.
              </p>
              <ul className="v06-list">
                <li>
                  <b>목록의 "위험도" 열</b>: 목록 보기에서 보기 ▾ → "위험도 (AI 판정)"를 켜면, 그 탭에 보이는 항목마다 판정이 배지로 붙어요.
                </li>
                <li>
                  <b>정리 계획의 2차 확인</b>: 계획 창의 "위험도 다시 확인"을 누르면, 계획(규칙이나 AI)이 매긴 위험 옆에 판정이 "jev
                  안전"이나 "로컬 안전"처럼 한 번 더 붙어요. 계획을 만든 쪽과 따로 보는 2차 의견이에요.
                </li>
                <li>
                  <b>AI 검색</b>: 검색창 앞의 [이름 | AI]에서 "AI"를 고르고(<code>Ctrl+Shift+F</code>) Enter를 누르면 이름에 없는 말로
                  찾아요. "RPG 게임"이면 설치된 게임 중에서, "작년에 받은 전세 계약서"면 파일 중에서 맞는 것을 골라 일치도와 함께 보여 줘요.
                </li>
                <li>
                  <b>보내는 것</b>: 메타데이터만 써요. 위험도는 항목의 경로·이름·종류·크기·날짜·분류를 줘요. AI 검색은 찾는 말과
                  후보를 주는데, jev에는 오늘 날짜와 함께 파일이면 경로·이름·분류·크기·수정한 날짜, 게임·프로그램이면 이름·설치
                  폴더·게시자를 주고, 로컬 모델에는 파일이면 경로와 수정한 날짜(오늘 날짜와 함께), 게임·프로그램이면 이름과 설치
                  폴더만 줘요. 파일 내용은 주지 않아요(Steam 게임 이름만 Steam의 설치 정보 파일에서 읽어요).
                </li>
              </ul>
              <label className="v06-check">
                <input type="checkbox" checked={draft.jev_enabled} onChange={(e) => set("jev_enabled", e.target.checked)} />
                <span className="v06-check-main">
                  <span>AI 판정 쓰기 (위험도 배지, AI 검색)</span>
                  <span className="muted">
                    켜면 계획 창의 "위험도 다시 확인"과 목록 보기의 보기 ▾ → "위험도 (AI 판정)"를 쓸 수 있고, 검색창의 "AI"로 찾을 수
                    있어요. 위험도는 그것을 켜야, AI 검색은 Enter를 눌러야 판정해요(언제 무엇을 보내는지는 아래).
                  </span>
                </span>
              </label>

              <div className="ai-providers judge-engines" role="radiogroup" aria-label="판정 엔진">
                {JUDGES.map((o) => (
                  <label key={o.id} className={`ai-provider${judge === o.id ? " on" : ""}`}>
                    <input type="radio" name={judgeGroup} checked={judge === o.id} onChange={() => pickJudge(o.id)} />
                    <span className="ai-provider-main">
                      <span>{o.label}</span>
                      <span className="muted">{o.note}</span>
                    </span>
                  </label>
                ))}
              </div>

              {judge === "jev" ? (
                <>
                  <KeyField name="jev" label="jev API 키" placeholder="jev 콘솔에서 받은 키" has={keys?.jev} onChanged={keyChanged} />
                  {draft.jev_enabled && keys && !keys.jev && (
                    <div className="v06-warn">
                      jev API 키가 없으면 위험도 배지도, AI 검색도 쓸 수 없어요. 키가 없으면 위의 무료 로컬 모델을 골라도 돼요.
                    </div>
                  )}
                  <div className="ai-test">
                    <button className="btn" disabled={!!testing || saving || !keys?.jev} onClick={() => test("jev")}>
                      {testing === "jev" ? "시험하는 중…" : "jev 연결 시험"}
                    </button>
                    {testLine("jev")}
                  </div>
                  <p className="v06-note">
                    jev는 얼리 액세스 서비스예요. 내 jev API 키로만 동작하고, 보낸 만큼 jev 키 주인에게 jev의 조건대로 청구돼요(이
                    앱은 요금을 받지 않아요). 계획 창에서는 누를 때 그 계획의 항목(500개까지)을 보내요. 목록의 "위험도"는 탭마다 따로
                    켜는데, 켜 두는 동안 그 탭의 목록에 보이는 항목(한 번에 200개까지)을 보내고, 스크롤하거나 다른 폴더로 갈 때마다 새로
                    보이는 항목도 보내요. AI 검색은 Enter를 누를 때만 보내요. 파일은 240개까지, 게임·프로그램은 400개까지 80개씩
                    묶어 보내고(동시에 4개까지), 한 번 찾을 때 보통 2~6번 요청해요. 묻는 중에 "멈추기"(Esc)를 누르면 남은 후보는 보내지 않지만, 이미 보낸 요청은 요금이
                    나가요. 같은 검색을 다시 하면 앱을 끌 때까지 저장해 둔 답을 써요.
                  </p>
                </>
              ) : (
                <>
                  <div className="ai-fields">
                    <label className="field">
                      <span>Ollama 주소</span>
                      <input
                        value={draft.ollama_url}
                        spellCheck={false}
                        placeholder={DEFAULT_OLLAMA}
                        onChange={(e) => set("ollama_url", e.target.value)}
                      />
                    </label>
                    <label className="field">
                      <span>판정 모델</span>
                      <input
                        value={draft.judge_model ?? ""}
                        spellCheck={false}
                        placeholder={draft.ollama_model.trim() || "qwen3:4b"}
                        onChange={(e) => set("judge_model", e.target.value)}
                      />
                    </label>
                  </div>
                  {/* The models this Ollama has (ollama list), when it answers. */}
                  <div className="judge-models">
                    {models && models.length > 0 ? (
                      <label className="field">
                        <span>설치된 모델</span>
                        <select
                          value={models.includes((draft.judge_model ?? "").trim()) ? (draft.judge_model ?? "").trim() : ""}
                          onChange={(e) => e.target.value && set("judge_model", e.target.value)}
                        >
                          <option value="">설치된 모델에서 고르기…</option>
                          {models.map((m) => (
                            // The value is the name as Ollama gave it (what is saved); the text shows its marks.
                            <option key={m} value={m}>
                              {shown(m)}
                            </option>
                          ))}
                        </select>
                      </label>
                    ) : (
                      <span className="muted">
                        {modelsBusy
                          ? "설치된 모델을 읽는 중…"
                          : models
                            ? "Ollama에 받은 모델이 없어요. 아래 명령으로 받아 주세요"
                            : modelsError
                              ? `설치된 모델을 읽지 못했어요: ${shown(modelsError)}`
                              : ""}
                      </span>
                    )}
                    <button type="button" className="btn small" disabled={modelsBusy} onClick={() => loadModels(draft.ollama_url)}>
                      목록 새로 읽기
                    </button>
                  </div>
                  {draft.ollama_url.trim() && !isLocalUrl(draft.ollama_url) && (
                    <div className="v06-warn">
                      이 주소는 이 컴퓨터가 아니에요. 판정할 항목의 정보가 그 컴퓨터로 가요. 내 PC에서만 쓰려면{" "}
                      <code>{DEFAULT_OLLAMA}</code>로 두세요.
                    </div>
                  )}
                  {draft.jev_enabled && !judgeModel(draft) && (
                    <div className="v06-warn">판정할 모델 이름을 넣어 주세요 (예: qwen3:4b). 모델이 없으면 위험도 배지도, AI 검색도 쓸 수 없어요.</div>
                  )}
                  <p className="v06-note">
                    <b>추천</b>: <code>qwen3:4b</code> (한국어, GPU 4GB 이상) · 더 정확하게: <code>qwen3:8b</code>. 비워 두면 위 "쓸
                    AI"의 Ollama 모델을 써요. 주소는 위 "쓸 AI"의 Ollama와 같은 칸이에요.
                  </p>
                  <p className="v06-note">
                    <b>처음 쓸 때</b>: ① <b>ollama.com</b>에서 Ollama를 설치해요. ② 터미널에서 아래 명령으로 모델을 받아요. ③ 위에 모델
                    이름을 넣고 "로컬 모델 시험"을 눌러요.
                  </p>
                  {/* The model that judges (its own, else 쓸 AI's Ollama model), as the test tries it. */}
                  <CopyBlock text={`ollama pull ${judgeModel(draft) || "qwen3:4b"}`} label="모델을 받는 명령" />
                  <div className="ai-test">
                    <button className="btn" disabled={!!testing || saving || !judgeModel(draft)} onClick={() => test("judge")}>
                      {testing === "judge" ? "시험하는 중…" : "로컬 모델 시험"}
                    </button>
                    {dirty && !testing && <span className="muted">바꾼 설정을 먼저 저장해요</span>}
                    {testLine("judge")}
                  </div>
                  <p className="v06-note">
                    로컬 모델은 이 PC에서 돌아서 요금이 없고, 주소가 이 컴퓨터면 아무것도 밖으로 나가지 않아요. 대신 jev보다 느리고 덜
                    정확할 수 있어요. 위험도에는 신뢰도를 매기지 않고("로컬" 배지), AI 검색의 일치도는 맞아요·아마·아니에요 세
                    단계로만 답해요. 판정 중에는 "멈추기"(Esc)로 멈추고 확인한 것만 볼 수 있어요.
                  </p>
                  <p className="v06-note">
                    <b>AI 검색의 속도</b>: 로컬 모델에는 후보를 한 항목씩 차례로 물어요. 작은 모델은 여러 개를 한 번에 물으면 다 같은
                    답을 내서예요. 한 항목에 0.3초쯤 걸려서(qwen3:4b, 노트북 GPU 기준) 파일은 100개, 게임·프로그램은 150개까지만
                    확인하고, 한 번 찾는 데 수십 초 걸려요. 첫 검색은 모델을 올리느라 더 걸릴 수 있어요.
                  </p>
                </>
              )}
              <p className="v06-note">
                "보내기 전에 미리 보기"가 켜져 있으면 계획 창에서는 보내기 전에, 목록의 "위험도"와 AI 검색은 켤 때 무엇을 어디로
                보내는지 먼저 물어요(이 PC의 로컬 모델은 밖으로 나가는 것이 없어서 묻지 않아요). 꺼 두면 묻지 않고 보내요. 배지와
                일치도는 참고용이고, 실행할 단계는 직접 골라요.
              </p>
            </section>

            <section className="v06-section">
              <h4>MCP로 Claude 앱에 연결</h4>
              <p className="v06-note">
                Claude 앱(Claude Desktop, Claude Code)에 이 앱을 MCP 서버로 붙이면, 내 Claude 구독으로 스캔 결과(드라이브, 폴더
                크기, 검색, 크기 지도)를 <b>읽기 전용</b>으로 보고 정리 계획을 제안할 수 있어요. API 키가 필요 없어요. 제안은
                "정리 계획"에 초안으로 들어오고, 여기서 확인하고 실행해야만 파일이 바뀌어요.
              </p>
              <p className="v06-note">
                Claude Desktop: <code>%APPDATA%\Claude\claude_desktop_config.json</code>에 넣고 Claude Desktop을 다시 켜요.
              </p>
              <CopyBlock text={MCP_JSON} label="claude_desktop_config.json에 넣을 내용" />
              <p className="v06-note">Claude Code: 터미널에서 실행해요.</p>
              <CopyBlock text={MCP_CMD} label="Claude Code에 추가하는 명령" />
              <p className="v06-note">다른 폴더에 설치했다면 경로를 그 폴더의 fine-file.exe로 바꿔 주세요.</p>
            </section>

            <section className="v06-section">
              <h4>요금과 로그인</h4>
              <ul className="v06-list muted">
                <li>API 키(Claude·OpenAI)로 보낸 요청은 쓴 만큼 그 회사가 키 주인에게 청구해요. 이 앱은 요금을 받지 않아요.</li>
                <li>Ollama는 내 PC에서 돌아서 요금이 없어요.</li>
                <li>
                  Claude Pro·Max나 ChatGPT 같은 구독 로그인은 이 앱 안에서 쓸 수 없어요. 구독으로 쓰려면 위의 MCP로 Claude 앱에
                  연결하거나, 직접 설치하고 로그인한 CLI(Claude Code, Codex)를 고르세요.
                </li>
                <li>키는 Windows 자격 증명 관리자에 저장하고, 파일이나 로그에는 남기지 않아요. 저장한 키는 다시 보여 주지 않아요.</li>
              </ul>
            </section>
          </>
        )}
      </div>
      <div className="dlg-foot">
        {notice && <span className={`v06-foot-note${notice.error ? " error" : ""}`} title={notice.text}>{notice.text}</span>}
        <span className="spacer" />
        <button className="btn" onClick={close}>
          닫기
        </button>
        <button className="btn primary" disabled={!dirty || saving} onClick={() => void save()}>
          {saving ? "저장하는 중…" : "저장"}
        </button>
      </div>

      {askClose && (
        <Dialog title="바꾼 설정을 저장할까요?" onClose={() => setAskClose(false)} onBackdrop={() => setAskClose(false)}>
          <h3>바꾼 설정을 저장할까요?</h3>
          <p className="muted">저장하지 않고 닫으면 바꾼 내용은 없어져요. API 키는 키 옆의 "저장"으로 따로 저장해요.</p>
          <div className="actions">
            <button onClick={onClose}>저장하지 않고 닫기</button>
            <button
              className="primary"
              autoFocus
              onClick={async () => {
                setAskClose(false);
                if (await save()) onClose();
              }}
            >
              저장하고 닫기
            </button>
          </div>
        </Dialog>
      )}
    </Dialog>
  );
}
