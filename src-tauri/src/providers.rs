//! Checks for the addresses and model names of the connections that take
//! them from the user: an OpenAI-compatible API ("compat"), Gemini and
//! OpenCode. An address is normalized so one key belongs to one origin
//! (scheme, host and port), and a name is held to what fits where it goes
//! (a URL path, a command-line value).

use crate::ai::{self, AiSettings, Request, ASK_TIMEOUT, CUT_OFF, EMPTY_ANSWER, MAX_TOKENS};
use serde_json::{json, Value};
use std::net::{Ipv4Addr, Ipv6Addr};
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
