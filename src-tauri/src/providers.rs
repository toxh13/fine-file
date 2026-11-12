//! Checks for the addresses and model names of the connections that take
//! them from the user: an OpenAI-compatible API ("compat"), Gemini and
//! OpenCode. An address is normalized so one key belongs to one origin
//! (scheme, host and port), and a name is held to what fits where it goes
//! (a URL path, a command-line value).

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
    #[allow(dead_code)] // read by compat_key (Task 2)
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
}
