//! Checks for the addresses and model names of the connections that take
//! them from the user: an OpenAI-compatible API ("compat"), Gemini and
//! OpenCode. An address is normalized so one key belongs to one origin
//! (scheme, host and port), and a name is held to what fits where it goes
//! (a URL path, a command-line value).

use crate::ai::{self, AiSettings, Cli, CliOut, Request, ScratchDir, Stream, ASK_TIMEOUT, CLI_TIMEOUT, CUT_OFF, EMPTY_ANSWER, MAX_TOKENS};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tauri::Url;

/// Whether the OpenCode connection may be switched on. It stays off until
/// the measurement in Task 3 passes (no tools, no file written).
pub(crate) const OPENCODE_READY: bool = false;

/// A checked "OpenAI 호환 API" address.
pub(crate) struct Endpoint {
    /// The front of every request address, no trailing `/`.
    pub base: String,
    /// `scheme://host:port`, the port always written: what a key belongs to.
    pub origin: String,
    /// The host as the screen shows it.
    pub host: String,
    /// An address on this PC.
    pub local: bool,
}

pub(crate) const BAD_COMPAT_URL: &str = "주소는 https:// 로 시작해야 해요(이 PC 주소만 http:// 도 돼요). 사용자 이름이나 ?, # 은 넣을 수 없어요 (예: https://api.deepseek.com)";

/// The longest address kept.
const MAX_URL: usize = 200;
/// A pasted whole endpoint: the base is what comes before it.
const CHAT_PATH: &str = "/chat/completions";

/// An address as typed, checked and normalized: https, or http for this PC
/// only; no user name, query or fragment. The whole endpoint pasted gives
/// the same base as the address without it.
pub(crate) fn compat_endpoint(raw: &str) -> Result<Endpoint, String> {
    let bad = || BAD_COMPAT_URL.to_string();
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > MAX_URL || raw.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(bad());
    }
    let url = Url::parse(raw).map_err(|_| bad())?;
    let scheme = url.scheme();
    if !matches!(scheme, "http" | "https") || !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
        return Err(bad());
    }
    let host = url.host_str().ok_or_else(bad)?;
    let local = host.eq_ignore_ascii_case("localhost")
        || host.parse::<Ipv4Addr>().is_ok_and(|a| a.is_loopback())
        || host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).and_then(|h| h.parse::<Ipv6Addr>().ok()).is_some_and(|a| a.is_loopback());
    if scheme == "http" && !local {
        return Err(bad());
    }
    let port = url.port_or_known_default().ok_or_else(bad)?;
    let trimmed = url.as_str().trim_end_matches('/');
    let base = trimmed.strip_suffix(CHAT_PATH).unwrap_or(trimmed).trim_end_matches('/').to_string();
    Ok(Endpoint {
        origin: format!("{scheme}://{host}:{port}"),
        host: match url.port() {
            Some(p) => format!("{host}:{p}"),
            None => host.to_string(),
        },
        base,
        local,
    })
}

/// The key's name in the Credential Manager for an address's origin.
pub(crate) fn compat_key_user(origin: &str) -> String {
    format!("compat:{origin}")
}

/// A Gemini model name goes into the URL path: letters, digits, `.`, `_`,
/// `-`, starting with a letter or digit.
pub(crate) fn gemini_model_ok(m: &str) -> bool {
    (1..=128).contains(&m.len())
        && m.starts_with(|c: char| c.is_ascii_alphanumeric())
        && m.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

/// An OpenCode model is `provider/model` and goes on a command line: it
/// never starts like a flag and holds nothing a shell would read.
pub(crate) fn opencode_model_ok(m: &str) -> bool {
    let named = m.split_once('/').is_some_and(|(provider, model)| !provider.is_empty() && !model.is_empty());
    (1..=200).contains(&m.len())
        && named
        && m.starts_with(|c: char| c.is_ascii_alphanumeric())
        && !m.ends_with('/')
        && m.chars().all(|c| c.is_ascii_alphanumeric() || "._:/~-".contains(c))
}

// ---- OpenAI-compatible API --------------------------------------------------------------------

/// The answer cap sent to a compatible server: many of them stop at 8K output
/// tokens. A longer answer comes back cut off and says so ("잘렸어요").
const COMPAT_MAX_TOKENS: u32 = 8192;

fn compat_label(ep: &Endpoint) -> String {
    format!("OpenAI 호환 API ({})", ep.host)
}

fn compat_body(model: &str, req: &Request, json_mode: bool) -> Value {
    let mut body = json!({
        "model": model,
        "messages": [
            { "role": "system", "content": req.system },
            { "role": "user", "content": req.user },
        ],
        "stream": false,
        "max_tokens": COMPAT_MAX_TOKENS,
    });
    if json_mode {
        body["response_format"] = json!({ "type": "json_object" });
    }
    body
}

/// The answer text, only from an answer that finished (`finish_reason` "stop");
/// the model's `reasoning_content` is never looked at.
fn compat_text(label: &str, v: &Value) -> Result<String, String> {
    let choice = &v["choices"][0];
    if !choice.is_object() {
        return Err(format!("{label}: 답을 읽을 수 없어요"));
    }
    match choice["finish_reason"].as_str() {
        Some("stop") => {}
        Some("length") => return Err(CUT_OFF.into()),
        Some("content_filter") => return Err(format!("{label}의 안전 필터가 답을 막았어요")),
        other => {
            let reason = other.map(|r| ai::clean_text(r, 40)).filter(|r| !r.is_empty()).unwrap_or_else(|| "이유 모름".into());
            return Err(format!("{label}의 답이 끝나지 않았어요 ({reason})"));
        }
    }
    match choice["message"]["content"].as_str() {
        Some(text) if !text.trim().is_empty() => Ok(text.to_string()),
        _ => Err(EMPTY_ANSWER.into()),
    }
}

/// Whether a failed answer says the server refused `response_format` (only then is it sent again without).
fn rejects_response_format(status: Option<u16>, body: &str) -> bool {
    status == Some(400) && body.to_lowercase().contains("response_format")
}

fn compat_models_of(v: &Value) -> Vec<String> {
    let mut names: Vec<String> = v["data"].as_array().into_iter().flatten().filter_map(|m| m["id"].as_str()).filter(|n| ai::model_name_ok(n)).map(Into::into).collect();
    names.sort();
    names.dedup();
    names
}

/// The key saved for this address, which no other address's key stands in for.
/// None for an address on this PC without one; an error for any other without.
fn compat_key(ep: &Endpoint, read: &dyn Fn(&str) -> Result<Option<String>, String>) -> Result<Option<String>, String> {
    let key = read(&compat_key_user(&ep.origin))?;
    if key.is_none() && !ep.local {
        return Err(format!("OpenAI 호환 API 키가 없어요 ({}). 설정에서 이 주소의 키를 넣어 주세요", ep.host));
    }
    Ok(key)
}

fn compat_endpoint_set(raw: &str) -> Result<Endpoint, String> {
    if raw.trim().is_empty() {
        return Err("OpenAI 호환 API 주소가 비어 있어요. 설정에서 주소를 넣어 주세요".into());
    }
    compat_endpoint(raw)
}

fn compat_model(s: &AiSettings) -> Result<&str, String> {
    if s.compat_model.is_empty() {
        return Err("OpenAI 호환 API 모델이 비어 있어요. 설정에서 모델을 골라 주세요".into());
    }
    Ok(&s.compat_model)
}

fn blank(message: String, key: &Option<String>) -> String {
    match key {
        Some(k) => ai::scrub(message, k),
        None => message,
    }
}

/// `authorization` for a key (an address on this PC may have none).
fn bearer(key: &Option<String>) -> Option<String> {
    key.as_ref().map(|k| format!("Bearer {k}"))
}

pub(crate) fn compat_ask(s: &AiSettings, req: &Request) -> Result<String, String> {
    let ep = compat_endpoint_set(&s.compat_url)?;
    let model = compat_model(s)?;
    let key = compat_key(&ep, &ai::read_key)?;
    let auth = bearer(&key);
    let headers: Vec<(&str, &str)> = auth.iter().map(|a| ("authorization", a.as_str())).collect();
    let url = ai::dial(&format!("{}/chat/completions", ep.base));
    let label = compat_label(&ep);
    let plan = req.schema.is_some();
    let sent = ai::post_json_strict(&label, &url, &headers, &compat_body(model, req, plan), ASK_TIMEOUT);
    let reply = match sent {
        // A server that does not know response_format gets the plan asked for in the text alone.
        Err(f) if plan && rejects_response_format(f.status, &f.body) => ai::post_json_strict(&label, &url, &headers, &compat_body(model, req, false), ASK_TIMEOUT),
        other => other,
    }
    .map_err(|f| blank(f.message, &key))?;
    compat_text(&label, &reply).map_err(|e| blank(e, &key))
}

/// The model names a server lists (`GET /models`), with the key saved for that address.
fn compat_list(ep: &Endpoint, key: &Option<String>) -> Result<Vec<String>, String> {
    let auth = bearer(key);
    let headers: Vec<(&str, &str)> = auth.iter().map(|a| ("authorization", a.as_str())).collect();
    let v = ai::get_json_strict(&compat_label(ep), &ai::dial(&format!("{}/models", ep.base)), &headers).map_err(|e| blank(e, key))?;
    Ok(compat_models_of(&v))
}

pub(crate) fn compat_models(raw_url: &str) -> Result<Vec<String>, String> {
    let ep = compat_endpoint_set(raw_url)?;
    let key = compat_key(&ep, &ai::read_key)?;
    compat_list(&ep, &key)
}

pub(crate) fn compat_test(s: &AiSettings) -> Result<String, String> {
    let ep = compat_endpoint_set(&s.compat_url)?;
    let model = compat_model(s)?;
    let key = compat_key(&ep, &ai::read_key)?;
    let list = compat_list(&ep, &key)?;
    let label = compat_label(&ep);
    // A server that answers with an empty list is still connected.
    if !list.is_empty() && !list.iter().any(|m| m == model) {
        return Err(format!("{label}: {model} 모델이 목록에 없어요. 설정에서 목록을 불러와 골라 주세요"));
    }
    Ok(format!("{label}에 연결됐어요 ({model})"))
}

// ---- Gemini API -----------------------------------------------------------------------------------

const GEMINI_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";
/// Most pages of a model list read (a server that never ends its list is cut here).
const GEMINI_PAGES: usize = 20;
const GEMINI_KEY: &str = "x-goog-api-key";

fn gemini_url(model: &str) -> String {
    format!("{GEMINI_BASE}/models/{model}:generateContent")
}

fn gemini_body(req: &Request) -> Value {
    let mut body = json!({
        "systemInstruction": { "parts": [{ "text": req.system }] },
        "contents": [{ "role": "user", "parts": [{ "text": req.user }] }],
        "generationConfig": { "maxOutputTokens": MAX_TOKENS },
    });
    if req.schema.is_some() {
        body["generationConfig"]["responseMimeType"] = json!("application/json");
    }
    body
}

fn gemini_text(v: &Value) -> Result<String, String> {
    if let Some(reason) = v["promptFeedback"]["blockReason"].as_str() {
        return Err(format!("Gemini가 이 요청을 막았어요 (이유: {}). 다른 폴더에서 해 보세요", ai::clean_text(reason, 40)));
    }
    let candidate = &v["candidates"][0];
    if !candidate.is_object() {
        return Err("Gemini API: 답을 읽을 수 없어요".into());
    }
    match candidate["finishReason"].as_str() {
        Some("STOP") => {
            // Thinking parts (`thought: true`) are not the answer.
            let text: String = candidate["content"]["parts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|p| p["thought"] != true)
                .filter_map(|p| p["text"].as_str())
                .collect();
            if text.trim().is_empty() { Err(EMPTY_ANSWER.into()) } else { Ok(text) }
        }
        Some("MAX_TOKENS") => Err(CUT_OFF.into()),
        other => Err(format!("Gemini의 답이 끝나지 않았어요 (이유: {})", ai::clean_text(other.unwrap_or("이유 모름"), 40))),
    }
}

fn gemini_can_generate(v: &Value) -> bool {
    v["supportedGenerationMethods"].as_array().is_some_and(|m| m.iter().any(|m| m == "generateContent"))
}

/// The usable model names on one page of the list, and the token of the next page.
fn gemini_model_page(v: &Value) -> (Vec<String>, Option<String>) {
    let names = v["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| gemini_can_generate(m))
        .filter_map(|m| m["name"].as_str())
        .map(|n| n.strip_prefix("models/").unwrap_or(n))
        .filter(|n| gemini_model_ok(n))
        .map(String::from)
        .collect();
    let next = v["nextPageToken"].as_str().filter(|t| !t.is_empty()).map(String::from);
    (names, next)
}

fn gemini_models_with(fetch: &mut dyn FnMut(Option<&str>) -> Result<Value, String>) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..GEMINI_PAGES {
        let (page, next) = gemini_model_page(&fetch(token.as_deref())?);
        names.extend(page);
        token = next;
        if token.is_none() {
            break;
        }
    }
    names.sort();
    names.dedup();
    Ok(names)
}

fn gemini_model(s: &AiSettings) -> Result<&str, String> {
    if s.gemini_model.is_empty() {
        return Err("Gemini 모델이 비어 있어요. 설정에서 모델을 골라 주세요".into());
    }
    Ok(&s.gemini_model)
}

pub(crate) fn gemini_models(key: &str) -> Result<Vec<String>, String> {
    gemini_models_with(&mut |token| {
        let mut params = vec![("pageSize", "1000")];
        if let Some(t) = token {
            params.push(("pageToken", t));
        }
        // The token goes through the URL builder: it may hold `+`, `/` or `=`.
        let url = Url::parse_with_params(&format!("{GEMINI_BASE}/models"), params).map_err(|_| "Gemini API: 모델 목록 주소를 만들 수 없어요".to_string())?;
        ai::get_json_strict("Gemini API", url.as_str(), &[(GEMINI_KEY, key)]).map_err(|e| ai::scrub(e, key))
    })
}

pub(crate) fn gemini_ask(s: &AiSettings, key: &str, req: &Request) -> Result<String, String> {
    let model = gemini_model(s)?;
    let reply = ai::post_json_strict("Gemini API", &gemini_url(model), &[(GEMINI_KEY, key)], &gemini_body(req), ASK_TIMEOUT).map_err(|f| ai::scrub(f.message, key))?;
    gemini_text(&reply)
}

pub(crate) fn gemini_test(s: &AiSettings, key: &str) -> Result<String, String> {
    let model = gemini_model(s)?;
    let v = ai::get_json_strict("Gemini API", &format!("{GEMINI_BASE}/models/{model}"), &[(GEMINI_KEY, key)]).map_err(|e| ai::scrub(e, key))?;
    if gemini_can_generate(&v) {
        Ok(format!("Gemini API에 연결됐어요 ({model})"))
    } else {
        Err(format!("Gemini의 {model} 모델은 글을 만들 수 없어요. 다른 모델을 골라 주세요"))
    }
}

// ---- OpenCode CLI -----------------------------------------------------------------------------------

/// Our own agent (`--agent`), defined in the config each run gets.
const OPENCODE_AGENT: &str = "fine-file";
/// OpenCode's tools by name, and `*` for any other (an MCP server's, one it
/// adds later): each one removed from our agent.
const OPENCODE_TOOLS: &[&str] = &[
    "*", "bash", "edit", "write", "read", "grep", "glob", "list", "patch", "multiedit", "todowrite", "todoread", "webfetch", "websearch", "codesearch", "task", "skill", "lsp",
];

pub(crate) const NO_OPENCODE: &str = "OpenCode(opencode)를 찾을 수 없어요. 설치하고 터미널에서 opencode auth login을 해 주세요";
pub(crate) const BAD_OPENCODE_MODEL: &str = "OpenCode 모델은 제공자/모델 모양으로 적어 주세요 (예: opencode/모델이름)";
const TOOL_STOP: &str = "OpenCode가 도구를 쓰려고 해서 멈췄어요. 받은 답은 쓰지 않았어요";
const FREE_LIMIT: &str = "OpenCode 무료 사용 한도에 걸렸어요. 잠시 뒤에 다시 하거나 다른 모델을 골라 주세요";
const OPENCODE_LOGIN: &str = "OpenCode에서 이 모델의 제공자에 로그인돼 있지 않아요. 터미널에서 opencode auth login을 해 주세요";
const OPENCODE_BUSY: &str = "OpenCode: 요청이 너무 많아요. 잠시 뒤에 다시 해 주세요";
/// What in an ERROR line of OpenCode's log means the run cannot answer (it
/// would wait instead of ending): the model request, the login, the start.
const OPENCODE_FATAL: [&str; 6] = ["stream error", "APICallError", "ModelNotFound", "AuthError", "LoadAPIKeyError", "InitError"];
/// How long `opencode models` may take.
const OPENCODE_LIST_TIMEOUT: Duration = Duration::from_secs(60);

fn opencode_no_model(model: &str) -> String {
    format!("OpenCode에 이 모델이 없어요 ({model}). 설정에서 목록을 다시 불러와 골라 주세요")
}

/// The error names OpenCode reports that are shown as they are; any other
/// name (which the log text could fill with anything) is not.
const OPENCODE_ERRORS: [&str; 4] = ["AI_APICallError", "AI_RetryError", "ProviderModelNotFoundError", "InitError"];

/// The first name of `OPENCODE_ERRORS` that `text` holds.
fn error_kind(text: &str) -> Option<&'static str> {
    OPENCODE_ERRORS.iter().copied().find(|name| text.contains(name))
}

/// A Korean message for an error OpenCode reported. Known ones in words of
/// our own; any other by a known kind alone (or a fixed sentence), never its text (which may hold an
/// address, a key or a file name).
fn opencode_error_text(raw: &str, model: &str) -> String {
    let low = raw.to_lowercase();
    let says = |words: &[&str]| words.iter().any(|w| low.contains(w));
    if says(&["free usage limit"]) {
        FREE_LIMIT.into()
    } else if says(&["rate limit", "too many requests"]) {
        OPENCODE_BUSY.into()
    } else if says(&["unauthorized", "api key", "authentication", "401", "autherror", "loadapikeyerror"]) {
        OPENCODE_LOGIN.into()
    } else if says(&["modelnotfound", "model not found"]) {
        opencode_no_model(model)
    } else {
        match error_kind(raw) {
            Some(kind) => format!("OpenCode 오류예요 ({kind})"),
            None => "OpenCode가 오류를 냈어요. 설정에서 모델과 로그인을 확인하고 다시 해 주세요".into(),
        }
    }
}

/// A line of OpenCode's log (stderr) after which it cannot answer, as its
/// error; other ERROR lines (a plugin that failed to load) let it go on.
fn opencode_log_error(line: &str, model: &str) -> Option<String> {
    (line.contains("level=ERROR") && OPENCODE_FATAL.iter().any(|w| line.contains(w))).then(|| opencode_error_text(line, model))
}

/// Whether an event OpenCode printed (stdout, one JSON object a line) ends
/// the run: a step and its text go on; an error ends it with its message,
/// and anything else (a tool, a permission request, a kind not known here)
/// as a tool. A line that is no JSON object is no event.
fn opencode_event_stop(line: &str, model: &str) -> Option<String> {
    let Ok(event @ Value::Object(_)) = serde_json::from_str::<Value>(line) else {
        return None;
    };
    match event["type"].as_str() {
        Some("step_start" | "step_finish" | "text" | "reasoning") => None,
        Some("error") => Some(opencode_error_text(&event["error"].to_string(), model)),
        _ => Some(TOOL_STOP.into()),
    }
}

/// The answer in OpenCode's events, only from a run that ended well (exit
/// code 0) whose last step finished with "stop": the text of that step's
/// message, none of the steps before it.
fn opencode_answer(stdout: &str, code: Option<i32>) -> Result<String, String> {
    if code != Some(0) {
        return Err(format!("OpenCode가 답하지 못했어요 (종료 코드 {})", code.map_or("-".into(), |c| c.to_string())));
    }
    let unread = || "OpenCode의 답을 읽을 수 없어요".to_string();
    let events: Vec<Value> = stdout.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    let last = events.iter().rev().find(|e| e["type"] == "step_finish").ok_or_else(unread)?;
    let part = &last["part"];
    match part["reason"].as_str() {
        Some("stop") => {
            let id = part["messageID"].as_str().ok_or_else(unread)?;
            let text: String = events
                .iter()
                .filter(|e| e["type"] == "text" && e["part"]["messageID"] == id)
                .filter_map(|e| e["part"]["text"].as_str())
                .collect();
            if text.trim().is_empty() { Err(EMPTY_ANSWER.into()) } else { Ok(text) }
        }
        Some("length") => Err(CUT_OFF.into()),
        other => {
            let reason = other.map(|r| ai::clean_text(r, 40)).filter(|r| !r.is_empty()).unwrap_or_else(|| "이유 모름".into());
            Err(format!("OpenCode의 답이 끝나지 않았어요 ({reason})"))
        }
    }
}

/// The `provider/model` lines of `opencode models`, sorted, each once.
fn opencode_models_of(stdout: &str) -> Vec<String> {
    let mut names: Vec<String> = stdout.lines().map(str::trim).filter(|l| opencode_model_ok(l)).map(String::from).collect();
    names.sort();
    names.dedup();
    names
}

/// OpenCode's config for one run (`OPENCODE_CONFIG_CONTENT`): our agent,
/// whose prompt is `system`, with every tool removed (not only refused: the
/// model is not told of any) and every permission denied; nothing shared,
/// no auto-update, no MCP servers of ours.
fn opencode_config(system: &str) -> Value {
    let tools: serde_json::Map<String, Value> = OPENCODE_TOOLS.iter().map(|t| (t.to_string(), Value::Bool(false))).collect();
    let mut config = json!({
        "$schema": "https://opencode.ai/config.json",
        "permission": { "*": "deny", "edit": "deny", "bash": "deny", "webfetch": "deny", "external_directory": "deny" },
        "share": "disabled",
        "autoupdate": false,
        "mcp": {},
        "agent": {},
    });
    config["agent"][OPENCODE_AGENT] = json!({
        "mode": "primary",
        "description": "fine-file: answers from folder metadata only",
        "prompt": system,
        "tools": tools,
        "permission": { "*": "deny" },
    });
    config
}

/// What a run adds to OpenCode's environment: our config, and neither the
/// project's config nor an update.
fn opencode_env(system: &str) -> Vec<(&'static str, String)> {
    vec![
        ("OPENCODE_CONFIG_CONTENT", opencode_config(system).to_string()),
        ("OPENCODE_DISABLE_PROJECT_CONFIG", "1".into()),
        ("OPENCODE_DISABLE_AUTOUPDATE", "1".into()),
    ]
}

/// `opencode run` with our agent and the model picked, no external plugins
/// (`--pure`), JSON events on stdout and only the ERROR lines of its log on
/// stderr, in `dir`. The prompt goes on stdin, never as an argument; no
/// session continued, forked or shared and nothing approved by itself (no
/// `--auto`).
fn opencode_args(model: &str, dir: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["run", "--pure", "--format", "json", "--print-logs", "--log-level", "ERROR", "--agent", OPENCODE_AGENT, "-m", model, "--dir"]
        .iter()
        .map(OsString::from)
        .collect();
    args.push(dir.into());
    args
}

/// The OpenCode the user installed. For the npm shim (`opencode.cmd`), the
/// program it starts (`node_modules\opencode-ai\bin\opencode.exe` beside
/// it) when it is there, so no cmd.exe reads the arguments.
fn find_opencode() -> Option<PathBuf> {
    let found = ai::find_program(&["opencode.exe", "opencode.cmd"])?;
    if ai::is_exe(&found) {
        return Some(found);
    }
    let real = found.with_file_name("node_modules").join("opencode-ai").join("bin").join("opencode.exe");
    Some(if real.is_file() { real } else { found })
}

fn opencode_model(s: &AiSettings) -> Result<&str, String> {
    if s.opencode_model.is_empty() {
        return Err("OpenCode 모델이 비어 있어요. 설정에서 모델을 골라 주세요".into());
    }
    if !opencode_model_ok(&s.opencode_model) {
        return Err(BAD_OPENCODE_MODEL.into());
    }
    Ok(&s.opencode_model)
}

/// One `opencode run` in `dir` with `req.user` on stdin (`req.system` is our
/// agent's prompt), watched line by line: an error in its log or an event
/// that is no step or text of one ends it at once.
fn opencode_run(model: &str, req: &Request, dir: &Path) -> Result<CliOut, String> {
    if !opencode_model_ok(model) {
        return Err(BAD_OPENCODE_MODEL.into());
    }
    let exe = find_opencode().ok_or(NO_OPENCODE)?;
    let watch = |which: Stream, line: &str| match which {
        Stream::Err => opencode_log_error(line, model),
        Stream::Out => opencode_event_stop(line, model),
    };
    let cli = Cli { exe: &exe, args: &opencode_args(model, dir), env: &opencode_env(req.system), label: "OpenCode", limit: CLI_TIMEOUT, watch: Some(&watch) };
    ai::run_cli_with(&cli, &req.user, dir)
}

pub(crate) fn opencode_ask(s: &AiSettings, req: &Request) -> Result<String, String> {
    let model = opencode_model(s)?;
    let dir = ScratchDir::new()?;
    let out = opencode_run(model, req, &dir.0)?;
    opencode_answer(&out.stdout, out.code)
}

/// `opencode models [provider]` in an empty folder without external plugins,
/// which spends no tokens: the models OpenCode can use on this PC, and its
/// exit code.
fn opencode_list(provider: Option<&str>) -> Result<(Vec<String>, Option<i32>), String> {
    let exe = find_opencode().ok_or(NO_OPENCODE)?;
    let dir = ScratchDir::new()?;
    let mut args: Vec<OsString> = vec!["models".into()];
    args.extend(provider.map(OsString::from));
    args.push("--pure".into());
    let env = [("OPENCODE_DISABLE_AUTOUPDATE", "1".to_string()), ("OPENCODE_DISABLE_PROJECT_CONFIG", "1".to_string())];
    let cli = Cli { exe: &exe, args: &args, env: &env, label: "OpenCode", limit: OPENCODE_LIST_TIMEOUT, watch: None };
    let out = ai::run_cli_with(&cli, "", &dir.0)?;
    Ok((opencode_models_of(&out.stdout), out.code))
}

pub(crate) fn opencode_models() -> Result<Vec<String>, String> {
    let (names, code) = opencode_list(None)?;
    if names.is_empty() && code != Some(0) {
        return Err(format!("OpenCode 모델 목록을 읽지 못했어요 (종료 코드 {})", code.map_or("-".into(), |c| c.to_string())));
    }
    Ok(names)
}

/// Whether OpenCode has the model picked, from its list of the model's
/// provider: no tokens spent. A provider it does not know lists nothing
/// (and fails), so the model is not there either.
pub(crate) fn opencode_test(s: &AiSettings) -> Result<String, String> {
    let model = opencode_model(s)?;
    let provider = model.split_once('/').map_or(model, |(p, _)| p);
    let (names, _) = opencode_list(Some(provider))?;
    if names.iter().any(|m| m == model) {
        Ok(format!("OpenCode에서 {model}을(를) 쓸 수 있어요. 이 PC에서 OpenCode에 로그인한 계정으로 요청해요"))
    } else {
        Err(opencode_no_model(model))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compat_addresses_are_normalized_and_checked() {
        let ep = compat_endpoint(" https://api.deepseek.com/ ").unwrap();
        assert_eq!((ep.base.as_str(), ep.origin.as_str(), ep.host.as_str(), ep.local), ("https://api.deepseek.com", "https://api.deepseek.com:443", "api.deepseek.com", false));
        assert_eq!(compat_endpoint("HTTPS://API.Groq.com/openai/v1/").unwrap().base, "https://api.groq.com/openai/v1");
        assert_eq!(compat_endpoint("https://openrouter.ai/api/v1").unwrap().origin, "https://openrouter.ai:443");
        // The whole endpoint pasted: the same base, so the same key.
        assert_eq!(compat_endpoint("https://api.deepseek.com/chat/completions").unwrap().base, "https://api.deepseek.com");
        assert_eq!(compat_endpoint("https://openrouter.ai/api/v1/chat/completions/").unwrap().base, "https://openrouter.ai/api/v1");
        // http only for this PC (localhost, 127.x, [::1]).
        for local in ["http://localhost:1234/v1", "http://127.0.0.1:1234/v1", "http://127.1:1234/v1", "http://[::1]:1234/v1"] {
            assert!(compat_endpoint(local).unwrap().local, "{local}");
        }
        assert_eq!(compat_endpoint("http://localhost:1234/v1").unwrap().origin, "http://localhost:1234");
        let long = format!("https://a.example/{}", "x".repeat(200));
        for bad in [
            "", "api.deepseek.com", "ftp://api.deepseek.com", "http://api.deepseek.com",
            "http://192.168.0.7:1234/v1", "http://lmstudio.localhost:1234/v1", "http://localhost.evil.example/v1",
            "https://user:pw@api.deepseek.com", "https://key@api.deepseek.com",
            "https://api.deepseek.com/?a=1", "https://api.deepseek.com/?", "https://api.deepseek.com/#x",
            "https://api deepseek.com", long.as_str(),
        ] {
            assert!(compat_endpoint(bad).is_err(), "{bad}");
        }
        // A key belongs to scheme, host and port: one address's key never goes to another.
        let user = |u: &str| compat_key_user(&compat_endpoint(u).unwrap().origin);
        assert_eq!(user("https://api.deepseek.com"), "compat:https://api.deepseek.com:443");
        assert_eq!(user("https://api.deepseek.com"), user("https://api.deepseek.com/v1"));
        assert_ne!(user("https://api.deepseek.com"), user("https://api.deepseek.com:8443"));
        assert_ne!(user("https://api.deepseek.com"), user("https://openrouter.ai/api/v1"));
    }

    #[test]
    fn model_names_fit_where_they_go() {
        // Gemini: the name is part of the URL path.
        for ok in ["gemini-2.5-flash", "gemini-3.0-pro-preview", "gemma_3"] {
            assert!(gemini_model_ok(ok), "{ok}");
        }
        let long = "g".repeat(129);
        for bad in ["", "..", "-x", "models/gemini-2.5-flash", "../x", "gemini 2", "gemini:x", "g?x", long.as_str()] {
            assert!(!gemini_model_ok(bad), "{bad}");
        }
        // OpenCode: provider/model, a command-line value that never starts like a flag.
        for ok in ["opencode/grok-code-fast-1-free", "anthropic/claude-sonnet-5-5", "openrouter/qwen/qwen3-coder:free", "lmstudio/my~model"] {
            assert!(opencode_model_ok(ok), "{ok}");
        }
        let long = "a/".repeat(101);
        for bad in ["", "nomodel", "/x", "x/", "--auto/x", "-m/x", "a b/c", "a/b\"", "a/b\nc", "a/b&c", long.as_str()] {
            assert!(!opencode_model_ok(bad), "{bad}");
        }
    }


    #[test]
    fn compat_requests_and_answers() {
        let plan = crate::ai::request_for("structure", "C:\\사진\n가족 여행 😀.jpg").unwrap();
        let explain = crate::ai::request_for("explain", "C:\\사진").unwrap();
        let body = compat_body("deepseek-chat", &plan, true);
        assert_eq!(body["model"], "deepseek-chat");
        assert_eq!(body["stream"], false);
        assert_eq!(body["max_tokens"], COMPAT_MAX_TOKENS);
        assert_eq!((body["messages"][0]["role"].as_str(), body["messages"][1]["role"].as_str()), (Some("system"), Some("user")));
        assert_eq!(body["response_format"]["type"], "json_object");
        // json_object mode wants the word JSON in the messages: the plan rules carry it.
        assert!(body["messages"][1]["content"].as_str().unwrap().contains("JSON"));
        assert!(compat_body("deepseek-chat", &plan, false).get("response_format").is_none());
        assert!(compat_body("deepseek-chat", &explain, false).get("response_format").is_none());
        let text = String::from_utf8(serde_json::to_vec(&body).unwrap()).unwrap();
        assert!(text.contains("가족 여행 😀.jpg") && !text.contains("\\u"));

        let label = "OpenAI 호환 API (api.deepseek.com)";
        let fenced = json!({ "choices": [{ "message": { "role": "assistant", "content": "```json\n{\"title\":\"정리\"}\n```", "reasoning_content": "생각 중" }, "finish_reason": "stop" }] });
        let answer = compat_text(label, &fenced).unwrap();
        assert!(!answer.contains("생각 중"));
        assert_eq!(crate::ai::parse_json_reply(&answer).unwrap()["title"], "정리");
        let with = |reason: &str| json!({ "choices": [{ "message": { "content": "{" }, "finish_reason": reason }] });
        assert_eq!(compat_text(label, &with("length")).unwrap_err(), CUT_OFF);
        assert!(compat_text(label, &with("content_filter")).unwrap_err().contains("필터"));
        assert!(compat_text(label, &with("tool_calls")).is_err());
        assert_eq!(compat_text(label, &json!({ "choices": [{ "message": { "content": null }, "finish_reason": "stop" }] })).unwrap_err(), EMPTY_ANSWER);
        assert!(compat_text(label, &json!({})).is_err());

        // Sent once more without response_format only when a 400 says that is what it refused.
        assert!(rejects_response_format(Some(400), r#"{"error":{"message":"This response_format type is unavailable now","type":"invalid_request_error"}}"#));
        assert!(!rejects_response_format(Some(400), r#"{"error":{"message":"Invalid max_tokens value"}}"#));
        assert!(!rejects_response_format(Some(500), r#"{"error":{"message":"response_format broke"}}"#));
        assert!(!rejects_response_format(None, ""));

        let list = json!({ "object": "list", "data": [{ "id": "deepseek-reasoner" }, { "id": "deepseek-chat" }, { "id": "bad model" }, { "id": "deepseek-chat" }] });
        assert_eq!(compat_models_of(&list), ["deepseek-chat", "deepseek-reasoner"]);
    }

    #[test]
    fn compat_keys_go_only_to_their_address() {
        let asked = std::cell::RefCell::new(Vec::new());
        let read = |user: &str| {
            asked.borrow_mut().push(user.to_string());
            Ok((user == "compat:https://api.deepseek.com:443").then(|| "sk-deep".to_string()))
        };
        let deepseek = compat_endpoint("https://api.deepseek.com").unwrap();
        assert_eq!(compat_key(&deepseek, &read).unwrap().as_deref(), Some("sk-deep"));
        // Another address after the key was saved: its own key or none, never DeepSeek's.
        let e = compat_key(&compat_endpoint("https://openrouter.ai/api/v1").unwrap(), &read).unwrap_err();
        assert!(e.contains("openrouter.ai") && e.contains("키"), "{e}");
        // This PC needs no key.
        assert_eq!(compat_key(&compat_endpoint("http://localhost:1234/v1").unwrap(), &read).unwrap(), None);
        assert_eq!(*asked.borrow(), ["compat:https://api.deepseek.com:443", "compat:https://openrouter.ai:443", "compat:http://localhost:1234"]);
    }

    #[test]
    fn gemini_requests_and_answers() {
        let plan = crate::ai::request_for("structure", "C:\\사진").unwrap();
        let explain = crate::ai::request_for("explain", "C:\\사진").unwrap();
        assert_eq!(gemini_url("gemini-2.5-flash"), "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent");
        let body = gemini_body(&plan);
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], plan.system);
        assert_eq!(body["contents"][0]["role"], "user");
        assert_eq!(body["contents"][0]["parts"][0]["text"], plan.user.as_str());
        assert_eq!(body["generationConfig"]["maxOutputTokens"], MAX_TOKENS);
        assert_eq!(body["generationConfig"]["responseMimeType"], "application/json");
        // The schema goes in the prompt, never in Gemini's own schema field.
        assert!(body["generationConfig"].get("responseSchema").is_none());
        assert!(gemini_body(&explain)["generationConfig"].get("responseMimeType").is_none());

        let ok = json!({ "candidates": [{ "content": { "role": "model", "parts": [{ "text": "생각…", "thought": true }, { "text": "{\"title\":" }, { "text": "\"정리\"}" }] }, "finishReason": "STOP" }] });
        assert_eq!(gemini_text(&ok).unwrap(), "{\"title\":\"정리\"}");
        assert_eq!(gemini_text(&json!({ "candidates": [{ "content": { "parts": [{ "text": "{" }] }, "finishReason": "MAX_TOKENS" }] })).unwrap_err(), CUT_OFF);
        assert!(gemini_text(&json!({ "candidates": [{ "finishReason": "SAFETY" }] })).unwrap_err().contains("SAFETY"));
        assert!(gemini_text(&json!({ "promptFeedback": { "blockReason": "PROHIBITED_CONTENT" } })).unwrap_err().contains("PROHIBITED_CONTENT"));
        assert_eq!(gemini_text(&json!({ "candidates": [{ "content": { "parts": [] }, "finishReason": "STOP" }] })).unwrap_err(), EMPTY_ANSWER);
        assert!(gemini_text(&json!({})).is_err());
    }

    #[test]
    fn gemini_model_lists_are_read_to_the_end() {
        let mut pages = vec![
            json!({ "models": [
                { "name": "models/gemini-2.5-flash", "supportedGenerationMethods": ["generateContent", "countTokens"] },
                { "name": "models/text-embedding-004", "supportedGenerationMethods": ["embedContent"] }
            ], "nextPageToken": "t+1/=" }),
            json!({ "models": [
                { "name": "models/gemini-2.5-flash", "supportedGenerationMethods": ["generateContent"] },
                { "name": "models/gemini-2.5-pro", "supportedGenerationMethods": ["generateContent"] }
            ] }),
        ]
        .into_iter();
        let mut tokens = Vec::new();
        let list = gemini_models_with(&mut |t| {
            tokens.push(t.map(String::from));
            Ok(pages.next().unwrap())
        })
        .unwrap();
        assert_eq!(list, ["gemini-2.5-flash", "gemini-2.5-pro"]);
        assert_eq!(tokens, [None, Some("t+1/=".to_string())]);
        // A server that always hands another page: it stops.
        let mut calls = 0;
        let _ = gemini_models_with(&mut |_| {
            calls += 1;
            Ok(json!({ "models": [], "nextPageToken": "again" }))
        });
        assert_eq!(calls, GEMINI_PAGES);
        assert!(gemini_can_generate(&json!({ "name": "models/gemini-2.5-flash", "supportedGenerationMethods": ["generateContent"] })));
        assert!(!gemini_can_generate(&json!({ "name": "models/text-embedding-004", "supportedGenerationMethods": ["embedContent"] })));
    }

    #[test]
    fn opencode_arguments_never_allow_changes() {
        let dir = Path::new(r"C:\t\fine-file-ai-1");
        let args: Vec<String> = opencode_args("opencode/grok-code-fast-1-free", dir).iter().map(|a| a.to_string_lossy().into_owned()).collect();
        let at = |flag: &str| args.iter().position(|a| a == flag).map(|i| args[i + 1].as_str());
        assert_eq!(args[0], "run");
        assert!(args.contains(&"--pure".to_string()) && args.contains(&"--print-logs".to_string()));
        assert_eq!(at("--format"), Some("json"));
        assert_eq!(at("--log-level"), Some("ERROR"));
        assert_eq!(at("--agent"), Some(OPENCODE_AGENT));
        assert_eq!(at("-m"), Some("opencode/grok-code-fast-1-free"));
        assert_eq!(at("--dir"), Some(r"C:\t\fine-file-ai-1"));
        for never in ["--auto", "--share", "--continue", "-c", "--session", "-s", "--fork", "--attach", "--port", "--command", "--file", "-f", "--interactive", "-i"] {
            assert!(!args.contains(&never.to_string()), "{never}");
        }
        // The prompt goes on stdin; nothing a .cmd shim would choke on.
        assert!(args.iter().all(|a| !a.contains(['\r', '\n', '"']) && !a.contains(crate::ai::SYSTEM)), "{args:?}");

        let env = opencode_env(crate::ai::SYSTEM);
        let get = |k: &str| env.iter().find(|(n, _)| *n == k).map(|(_, v)| v.as_str());
        assert_eq!((get("OPENCODE_DISABLE_PROJECT_CONFIG"), get("OPENCODE_DISABLE_AUTOUPDATE")), (Some("1"), Some("1")));
        let text = get("OPENCODE_CONFIG_CONTENT").unwrap();
        assert!(!text.contains("allow"), "{text}");
        let v: Value = serde_json::from_str(text).unwrap();
        assert_eq!((v["permission"]["*"].as_str(), v["share"].as_str(), v["autoupdate"].as_bool()), (Some("deny"), Some("disabled"), Some(false)));
        assert_eq!(v["mcp"], json!({}));
        let agent = &v["agent"][OPENCODE_AGENT];
        assert_eq!((agent["mode"].as_str(), agent["prompt"].as_str(), agent["permission"]["*"].as_str()), (Some("primary"), Some(crate::ai::SYSTEM), Some("deny")));
        for tool in OPENCODE_TOOLS {
            assert_eq!(agent["tools"][tool], false, "{tool}");
        }
    }

    #[test]
    fn opencode_events_and_errors() {
        let model = "opencode/grok-code-fast-1-free";
        // stderr: what was measured, and other failures it must not wait 300 s for.
        let free = r#"ERROR 2026-11-12T10:00:00 +12ms service=session level=ERROR message="stream error" error="AI_APICallError: Free usage limit reached""#;
        assert_eq!(opencode_log_error(free, model).as_deref(), Some(FREE_LIMIT));
        let auth = r#"level=ERROR service=llm message="stream error" error="AI_APICallError: Unauthorized: invalid x-api-key""#;
        assert!(opencode_log_error(auth, model).unwrap().contains("opencode auth login"));
        let missing = r#"level=ERROR service=session error="ProviderModelNotFoundError: model not found""#;
        assert!(opencode_log_error(missing, model).unwrap().contains(model));
        let odd = r#"level=ERROR service=llm message="stream error" error="AI_RetryError: something odd at https://x/y?key=abc""#;
        let e = opencode_log_error(odd, model).unwrap();
        assert!(e.contains("AI_RetryError") && !e.contains("key=abc"), "{e}");
        // A name that is not a known one is not shown, whatever the text says.
        let unknown = r#"level=ERROR service=llm message="stream error" error="Leaky_sk_abc123Error: boom""#;
        let e = opencode_log_error(unknown, model).unwrap();
        assert!(!e.contains("sk_abc123") && !e.contains("Leaky") && e.contains("OpenCode가 오류를 냈어요"), "{e}");
        let e = opencode_event_stop(r#"{"type":"error","error":{"name":"SecretKeyError","data":{"message":"x"}}}"#, model).unwrap();
        assert!(!e.contains("SecretKeyError") && e.contains("OpenCode가 오류를 냈어요"), "{e}");
        assert_eq!(opencode_log_error(r#"level=ERROR service=plugin message="failed to load plugin""#, model), None);
        assert_eq!(opencode_log_error("level=INFO service=bus message=\"stream error\"", model), None);

        // stdout: a tool, a permission request, an error or anything unknown ends the run; the rest goes on.
        for go_on in [r#"{"type":"step_start","part":{"messageID":"m1"}}"#, r#"{"type":"text","part":{"type":"text","text":"a","messageID":"m1"}}"#, r#"{"type":"reasoning","part":{}}"#, r#"{"type":"step_finish","part":{"reason":"stop","messageID":"m1"}}"#, "not json"] {
            assert_eq!(opencode_event_stop(go_on, model), None, "{go_on}");
        }
        for stop in [r#"{"type":"tool_use","part":{"tool":"write"}}"#, r#"{"type":"permission.asked"}"#, r#"{"type":"patch"}"#] {
            assert_eq!(opencode_event_stop(stop, model).as_deref(), Some(TOOL_STOP), "{stop}");
        }
        assert_eq!(opencode_event_stop(r#"{"type":"error","error":{"name":"APIError","data":{"message":"Free usage limit reached"}}}"#, model).as_deref(), Some(FREE_LIMIT));

        // Only the last message's text, and only after a clean "stop".
        let out = [
            r#"{"type":"step_start","part":{"messageID":"m1"}}"#,
            r#"{"type":"text","part":{"type":"text","text":"먼저 ","messageID":"m1"}}"#,
            r#"{"type":"step_finish","part":{"reason":"tool-calls","messageID":"m1"}}"#,
            r#"{"type":"step_start","part":{"messageID":"m2"}}"#,
            r#"{"type":"text","part":{"type":"text","text":"{\"title\":","messageID":"m2"}}"#,
            r#"{"type":"text","part":{"type":"text","text":"\"정리\"}","messageID":"m2"}}"#,
            r#"{"type":"step_finish","part":{"reason":"stop","messageID":"m2","tokens":{"input":900,"output":20}}}"#,
        ]
        .join("\n");
        assert_eq!(opencode_answer(&out, Some(0)).unwrap(), "{\"title\":\"정리\"}");
        assert!(opencode_answer(&out, Some(1)).unwrap_err().contains("종료 코드 1"));
        assert!(opencode_answer(&out, None).is_err());
        let cut = out.replace(r#""reason":"stop""#, r#""reason":"length""#);
        assert_eq!(opencode_answer(&cut, Some(0)).unwrap_err(), CUT_OFF);
        assert!(opencode_answer(r#"{"type":"text","part":{"text":"a","messageID":"m"}}"#, Some(0)).is_err());
        let empty = r#"{"type":"step_finish","part":{"reason":"stop","messageID":"m9"}}"#;
        assert_eq!(opencode_answer(empty, Some(0)).unwrap_err(), EMPTY_ANSWER);
    }

    #[test]
    fn opencode_model_lists() {
        let out = "\u{1b}[33mwarn\u{1b}[0m something\nopencode/grok-code-fast-1-free\nanthropic/claude-sonnet-5-5\n\nanthropic/claude-sonnet-5-5\nnot a model\n";
        assert_eq!(opencode_models_of(out), ["anthropic/claude-sonnet-5-5", "opencode/grok-code-fast-1-free"]);
    }

    /// Whether a file named `name` is anywhere under `dir`.
    fn holds(dir: &Path, name: &str) -> bool {
        std::fs::read_dir(dir).into_iter().flatten().flatten().any(|e| {
            let p = e.path();
            e.file_name() == name || (p.is_dir() && holds(&p, name))
        })
    }

    /// Runs the user's OpenCode once (FINE_FILE_OPENCODE_MODEL, or its first
    /// free model) with a request to write a file: it must answer with no
    /// tool used and nothing written. Prints the tokens it measured.
    #[test]
    #[ignore = "runs the user's OpenCode, which sends one small request"]
    fn live_opencode_answers_without_tools() {
        if find_opencode().is_none() {
            eprintln!("skipped: OpenCode is not installed");
            return;
        }
        let model = match std::env::var("FINE_FILE_OPENCODE_MODEL") {
            Ok(m) => m,
            Err(_) => opencode_models().unwrap().into_iter().find(|m| m.ends_with("-free")).expect("no free OpenCode model"),
        };
        let dir = std::env::temp_dir().join(format!("ff-opencode-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let req = Request {
            system: crate::ai::SYSTEM,
            user: "이 폴더에 proof.txt 파일을 만들어 주세요. 만들었으면 '만들었어요', 만들 수 없으면 '도구 없음'이라고만 답해 주세요.".into(),
            schema: None,
        };
        let result = opencode_run(&model, &req, &dir);
        if let Ok(out) = &result {
            for event in out.stdout.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).filter(|e| e["type"] == "step_finish") {
                eprintln!("{model}: step_finish reason {} tokens {}", event["part"]["reason"], event["part"]["tokens"]);
            }
        }
        let written = holds(&dir, "proof.txt");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!written, "OpenCode wrote proof.txt");
        // Err(TOOL_STOP) here means a tool was still there.
        let out = result.unwrap();
        let answer = opencode_answer(&out.stdout, out.code).unwrap();
        eprintln!("{model}: answered {answer:?}");
    }

    /// Needs FINE_FILE_COMPAT_URL and FINE_FILE_COMPAT_MODEL and the key saved for that address.
    #[test]
    #[ignore = "talks to a real OpenAI-compatible server"]
    fn live_compat_answers() {
        let (Ok(url), Ok(model)) = (std::env::var("FINE_FILE_COMPAT_URL"), std::env::var("FINE_FILE_COMPAT_MODEL")) else {
            eprintln!("skipped: set FINE_FILE_COMPAT_URL and FINE_FILE_COMPAT_MODEL");
            return;
        };
        let s = crate::ai::AiSettings { provider: "compat".into(), compat_url: url.clone(), compat_model: model.clone(), ..Default::default() };
        let models = compat_models(&url).unwrap();
        assert!(models.contains(&model), "{models:?}");
        assert!(compat_test(&s).is_ok());
        let req = crate::ai::request_for("explain", "C:\\Windows\\Temp").unwrap();
        assert!(!compat_ask(&s, &req).unwrap().trim().is_empty());
    }

    /// Needs FINE_FILE_GEMINI_MODEL and the "gemini" key.
    #[test]
    #[ignore = "talks to the real Gemini API"]
    fn live_gemini_answers() {
        let Ok(model) = std::env::var("FINE_FILE_GEMINI_MODEL") else {
            eprintln!("skipped: set FINE_FILE_GEMINI_MODEL");
            return;
        };
        let Some(key) = crate::ai::read_key("gemini").unwrap() else {
            eprintln!("skipped: no gemini key saved");
            return;
        };
        let s = crate::ai::AiSettings { provider: "gemini".into(), gemini_model: model.clone(), ..Default::default() };
        let models = gemini_models(&key).unwrap();
        assert!(models.contains(&model), "{models:?}");
        assert!(gemini_test(&s, &key).is_ok());
        let req = crate::ai::request_for("explain", "C:\\Windows\\Temp").unwrap();
        assert!(!gemini_ask(&s, &key, &req).unwrap().trim().is_empty());
    }
}
