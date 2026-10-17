//! Page side of memory: the extractor registry (data), one stateless read-only
//! probe script, and native evaluate-with-result.

use serde::Serialize;
use std::sync::mpsc::{sync_channel, RecvTimeoutError};
use std::time::Duration;

use super::model::{parse_probe_payload, ProbeResult, ServiceId, EVAL_TIMEOUT, MAX_TURNS_PER_PROBE};

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum RoleRule {
    /// e.g. ChatGPT `data-message-author-role="user|assistant"`.
    Attribute { name: &'static str, user: &'static str, assistant: &'static str },
    /// Node matches `user` selector ⇒ User; matches `assistant` ⇒ Assistant.
    Selector { user: &'static str, assistant: &'static str },
    /// Custom elements, e.g. Gemini `user-query` / `model-response`.
    TagName { user: &'static str, assistant: &'static str },
}

/// Serialized as the `spec` argument of `PROBE_JS`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtractorSpec {
    pub id: &'static str,
    /// Host suffixes, matched against the live page URL, so custom services on
    /// the same host are covered too.
    #[serde(skip)]
    pub hosts: &'static [&'static str],
    /// JS RegExp source applied to `location.pathname`; group 1 = conversation key.
    pub conversation_path: &'static str,
    pub turn_selector: &'static str,
    pub role: RoleRule,
    /// Optional child selector for the text body (else the turn node itself).
    pub text_selector: Option<&'static str>,
    /// Present ⇒ `Snapshot.streaming` is `Some(..)`.
    pub streaming_selector: Option<&'static str>,
}

/// UNVERIFIED against live signed-in pages. Each entry is exercised against a
/// saved-markup fixture in a real engine; selector drift shows up in the UI as
/// `unrecognized-page`.
pub static EXTRACTORS: &[ExtractorSpec] = &[
    ExtractorSpec {
        id: "chatgpt",
        hosts: &["chatgpt.com", "chat.openai.com"],
        conversation_path: r"/c/([0-9a-f-]{16,})",
        turn_selector: "[data-message-author-role]",
        role: RoleRule::Attribute { name: "data-message-author-role", user: "user", assistant: "assistant" },
        text_selector: None,
        streaming_selector: Some("[data-testid='stop-button']"),
    },
    ExtractorSpec {
        id: "claude",
        hosts: &["claude.ai"],
        conversation_path: r"^/chat/([0-9a-f-]{16,})",
        turn_selector: "[data-testid='user-message'], [data-is-streaming]",
        role: RoleRule::Selector { user: "[data-testid='user-message']", assistant: "[data-is-streaming]" },
        text_selector: None,
        streaming_selector: Some("[data-is-streaming='true']"),
    },
    ExtractorSpec {
        id: "gemini",
        hosts: &["gemini.google.com"],
        conversation_path: r"^/app/([0-9a-f]{8,})",
        turn_selector: "user-query, model-response",
        role: RoleRule::TagName { user: "user-query", assistant: "model-response" },
        text_selector: None,
        streaming_selector: None,
    },
];

pub fn supported_hosts() -> Vec<&'static str> {
    EXTRACTORS.iter().flat_map(|spec| spec.hosts.iter().copied()).collect()
}

pub fn spec_for_url(url: &tauri::Url) -> Option<&'static ExtractorSpec> {
    let host = url.host_str()?.to_ascii_lowercase();
    EXTRACTORS.iter().find(|spec| {
        spec.hosts
            .iter()
            .any(|h| host == *h || host.strip_suffix(h).is_some_and(|rest| rest.ends_with('.')))
    })
}

/// Stateless, read-only, no globals, no network. Returns a JSON *string* so every
/// engine hands back the same type. Exceptions are caught in-script because
/// WebView2 drops them. The window is filled from the end so `context` is always
/// the real predecessor of `turns[0]`, and `innerText` (which forces layout) is
/// read for at most `maxTurns + 1` nodes.
pub const PROBE_JS: &str = r#"(function (spec, maxTurns) {
  try {
    var m = new RegExp(spec.conversationPath).exec(location.pathname);
    if (!m) return JSON.stringify({ ok: true, host: location.hostname, conversation: null, context: null, turns: [], streaming: null });
    var rule = spec.role;
    var roleOf = function (n) {
      if (rule.kind === 'attribute') {
        var v = n.getAttribute(rule.name);
        return v === rule.user ? 'user' : v === rule.assistant ? 'assistant' : null;
      }
      if (rule.kind === 'selector') {
        return n.matches(rule.user) ? 'user' : n.matches(rule.assistant) ? 'assistant' : null;
      }
      if (rule.kind === 'tagName') {
        var tag = n.tagName.toLowerCase();
        return tag === rule.user ? 'user' : tag === rule.assistant ? 'assistant' : null;
      }
      return null;
    };
    var textOf = function (n) {
      var body = spec.textSelector ? n.querySelector(spec.textSelector) : n;
      return body ? String(body.innerText || body.textContent || '') : '';
    };
    var nodes = document.querySelectorAll(spec.turnSelector);
    var picked = [];
    for (var i = nodes.length - 1; i >= 0 && picked.length <= maxTurns; i--) {
      var role = roleOf(nodes[i]);
      var text = role ? textOf(nodes[i]) : '';
      if (role && text.trim()) picked.unshift({ role: role, text: text });
    }
    var hasContext = picked.length > maxTurns;
    var streaming = spec.streamingSelector ? !!document.querySelector(spec.streamingSelector) : null;
    return JSON.stringify({ ok: true, host: location.hostname, conversation: m[1],
      context: hasContext ? picked[0] : null, turns: hasContext ? picked.slice(1) : picked, streaming: streaming });
  } catch (e) { return JSON.stringify({ ok: false, error: String(e) }); }
})"#;

pub fn probe_script(spec: &ExtractorSpec) -> String {
    let spec_json = serde_json::to_string(spec).expect("extractor specs are static strings");
    format!("{PROBE_JS}({spec_json},{MAX_TURNS_PER_PROBE})")
}

/// Never call on the main thread: `with_webview` dispatches to it and this waits.
pub async fn probe_service(app: &tauri::AppHandle, service: &ServiceId, now_ms: u64) -> ProbeResult {
    let Some(webview) = crate::live_service_webview(app, &service.0) else {
        return ProbeResult::NotOpen;
    };
    let url = match webview.url() {
        Ok(url) => url,
        Err(e) => return ProbeResult::Failed { message: e.to_string() },
    };
    let Some(spec) = spec_for_url(&url) else {
        return ProbeResult::UnsupportedSite { host: url.host_str().unwrap_or_default().to_string() };
    };
    let script = probe_script(spec);
    let evaluated =
        tauri::async_runtime::spawn_blocking(move || eval_with_result(&webview, script, EVAL_TIMEOUT)).await;
    match evaluated {
        Ok(Ok(json)) => parse_probe_payload(service, &json, now_ms),
        Ok(Err(e)) => ProbeResult::Failed { message: e.message() },
        Err(e) => ProbeResult::Failed { message: e.to_string() },
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EvalError {
    /// No native evaluate-with-result binding on this OS yet. There is no fallback channel.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    UnsupportedPlatform,
    WebviewGone(String),
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Engine(String),
    Timeout,
}

impl EvalError {
    pub fn message(&self) -> String {
        match self {
            EvalError::UnsupportedPlatform => "unsupported-platform".to_string(),
            EvalError::WebviewGone(e) => format!("webview unavailable: {e}"),
            EvalError::Engine(e) => format!("evaluate failed: {e}"),
            EvalError::Timeout => "probe timeout".to_string(),
        }
    }
}

/// Tauri 2.9 exposes only fire-and-forget `eval`, so reach the engine through
/// `Webview::with_webview`, which runs the closure on the main thread.
fn eval_with_result(webview: &tauri::Webview, js: String, timeout: Duration) -> Result<String, EvalError> {
    let (tx, rx) = sync_channel(1);
    webview
        .with_webview(move |pw| platform::evaluate(pw, &js, move |r| drop(tx.send(r))))
        .map_err(|e| EvalError::WebviewGone(e.to_string()))?;
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => Err(EvalError::Timeout),
        Err(RecvTimeoutError::Disconnected) => Err(EvalError::WebviewGone("closed during probe".to_string())),
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::EvalError;
    use javascriptcore::ValueExt;
    use webkit2gtk::WebViewExt;

    /// Named world: shares the DOM, invisible to and untamperable by page JS.
    const WORLD: &str = "anychat-probe";

    pub fn evaluate(
        pw: tauri::webview::PlatformWebview,
        js: &str,
        done: impl FnOnce(Result<String, EvalError>) + Send + 'static,
    ) {
        evaluate_in(&pw.inner(), js, done);
    }

    pub(super) fn evaluate_in(
        webview: &webkit2gtk::WebView,
        js: &str,
        done: impl FnOnce(Result<String, EvalError>) + 'static,
    ) {
        webview.evaluate_javascript(js, Some(WORLD), None, None::<&webkit2gtk::gio::Cancellable>, move |result| {
            done(match result {
                Ok(value) if value.is_string() => Ok(value.to_str().to_string()),
                Ok(_) => Err(EvalError::Engine("probe returned a non-string".to_string())),
                Err(e) => Err(EvalError::Engine(e.to_string())),
            })
        });
    }
}

#[cfg(not(target_os = "linux"))]
mod platform {
    use super::EvalError;

    pub fn evaluate(
        _pw: tauri::webview::PlatformWebview,
        _js: &str,
        done: impl FnOnce(Result<String, EvalError>) + Send + 'static,
    ) {
        done(Err(EvalError::UnsupportedPlatform));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;

    const BANNED: &[&str] =
        &["fetch(", "XMLHttpRequest", "WebSocket", "__TAURI__", "anychat://", "sendBeacon", "new Image"];

    fn squeeze(s: &str) -> String {
        s.chars().filter(|c| !c.is_whitespace()).collect()
    }

    fn banned_tokens(script: &str) -> Vec<&'static str> {
        let squeezed = squeeze(script);
        BANNED
            .iter()
            .copied()
            .filter(|token| script.contains(token) || squeezed.contains(&squeeze(token)))
            .collect()
    }

    fn spec(id: &str) -> &'static ExtractorSpec {
        EXTRACTORS.iter().find(|s| s.id == id).unwrap()
    }

    #[test]
    fn probe_script_opens_no_channel_out_of_the_page() {
        for spec in EXTRACTORS {
            assert_eq!(banned_tokens(&probe_script(spec)), Vec::<&str>::new(), "{} probe", spec.id);
        }
    }

    #[test]
    fn ban_check_catches_every_token_and_spacing_variants() {
        for token in BANNED {
            assert_eq!(banned_tokens(&format!("var x = 1; {token}")), vec![*token]);
        }
        assert_eq!(banned_tokens("fetch ('http://127.0.0.1')"), vec!["fetch("]);
        assert_eq!(banned_tokens("var i = new  Image();"), vec!["new Image"]);
    }

    #[test]
    fn probe_script_is_one_call_with_the_spec_as_json() {
        let script = probe_script(spec("chatgpt"));
        assert!(script.starts_with("(function (spec, maxTurns)"));
        assert!(script.ends_with(&format!(",{MAX_TURNS_PER_PROBE})")));
        let spec_json: serde_json::Value = serde_json::to_value(spec("chatgpt")).unwrap();
        assert_eq!(spec_json["conversationPath"], r"/c/([0-9a-f-]{16,})");
        assert_eq!(spec_json["role"]["kind"], "attribute");
        assert_eq!(spec_json["streamingSelector"], "[data-testid='stop-button']");
        assert!(spec_json.get("hosts").is_none());
        assert_eq!(serde_json::to_value(spec("gemini")).unwrap()["role"]["kind"], "tagName");
        assert_eq!(serde_json::to_value(spec("claude")).unwrap()["role"]["kind"], "selector");
    }

    #[test]
    fn spec_for_url_matches_host_and_subdomains_only() {
        let id = |url: &str| spec_for_url(&url.parse().unwrap()).map(|s| s.id);
        assert_eq!(id("https://chatgpt.com/c/abc"), Some("chatgpt"));
        assert_eq!(id("https://chat.openai.com/"), Some("chatgpt"));
        assert_eq!(id("https://CLAUDE.ai/chat/x"), Some("claude"));
        assert_eq!(id("https://www.claude.ai/"), Some("claude"));
        assert_eq!(id("https://gemini.google.com/app/1234abcd"), Some("gemini"));
        assert_eq!(id("https://notclaude.ai/"), None);
        assert_eq!(id("https://google.com/"), None);
        assert_eq!(id("http://127.0.0.1:5173/"), None);
        assert_eq!(id("about:blank"), None);
    }

    #[test]
    fn supported_hosts_lists_every_extractor_host() {
        assert_eq!(supported_hosts(), vec!["chatgpt.com", "chat.openai.com", "claude.ai", "gemini.google.com"]);
    }

    #[test]
    fn conversation_paths_capture_the_key() {
        for (id, path, key) in [
            ("chatgpt", "/c/6801b2b4-1c2c-8000-9d3e-0a1b2c3d4e5f", "6801b2b4-1c2c-8000-9d3e-0a1b2c3d4e5f"),
            ("chatgpt", "/g/g-abc/c/6801b2b4-1c2c-8000", "6801b2b4-1c2c-8000"),
            ("claude", "/chat/0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0", "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0"),
            ("gemini", "/app/1a2b3c4d5e6f7a8b", "1a2b3c4d5e6f7a8b"),
        ] {
            let re = Regex::new(spec(id).conversation_path).unwrap();
            assert_eq!(re.captures(path).map(|c| c[1].to_string()), Some(key.to_string()), "{id} {path}");
        }
        assert!(!Regex::new(spec("claude").conversation_path).unwrap().is_match("/new"));
        assert!(!Regex::new(spec("gemini").conversation_path).unwrap().is_match("/app"));
    }

    #[test]
    fn eval_errors_have_stable_messages() {
        assert_eq!(EvalError::UnsupportedPlatform.message(), "unsupported-platform");
        assert_eq!(EvalError::Timeout.message(), "probe timeout");
    }
}

/// Gate G1a on Linux: a real WebKitGTK engine, pages served with a strict CSP and
/// Trusted Types. Needs a display, so it is ignored by default:
/// `xvfb-run -a cargo test --lib memory::probe -- --ignored`.
#[cfg(all(test, target_os = "linux"))]
mod linux_fixture {
    use super::*;
    use crate::memory::model::{ObservedTurn, Role, Snapshot};
    use gtk::prelude::*;
    use javascriptcore::ValueExt;
    use std::cell::RefCell;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::rc::Rc;
    use std::time::Instant;
    use webkit2gtk::{LoadEvent, WebViewExt};

    const STRICT_CSP: &str = "default-src 'none'; script-src 'none'; connect-src 'none'; img-src 'none'; \
                              frame-src 'none'; require-trusted-types-for 'script'";

    fn page(body: &str) -> String {
        format!(
            "<!doctype html><html><head><meta charset=\"utf-8\"><title>fixture</title></head><body>\
             <script>document.title = 'page-script-ran';</script>{body}</body></html>"
        )
    }

    fn chatgpt_page(turns: usize) -> String {
        let rows: String = (0..turns)
            .map(|i| {
                let role = if i % 2 == 0 { "user" } else { "assistant" };
                format!("<article><div data-message-author-role=\"{role}\"><div><p>t{i}</p><p>第{i}段</p></div></div></article>")
            })
            .collect();
        page(&rows)
    }

    fn claude_page() -> String {
        page(
            "<div data-testid=\"user-message\"><p>hello claude</p></div>\
             <div data-is-streaming=\"false\"><div><p>hi there</p></div></div>\
             <div data-testid=\"user-message\"><p>second</p></div>\
             <div data-is-streaming=\"true\"><div><p>partial</p></div></div>",
        )
    }

    fn gemini_page() -> String {
        page(
            "<user-query><div>gemini question</div></user-query>\
             <model-response><div>gemini answer</div></model-response>",
        )
    }

    fn serve() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut request = Vec::new();
                let mut buf = [0u8; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let request = String::from_utf8_lossy(&request);
                let path = request.split_whitespace().nth(1).unwrap_or("/");
                let body = if path.starts_with("/c/") {
                    chatgpt_page(MAX_TURNS_PER_PROBE + 2)
                } else if path.starts_with("/chat/") {
                    claude_page()
                } else if path.starts_with("/app/") {
                    gemini_page()
                } else {
                    page("")
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Security-Policy: {STRICT_CSP}\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        port
    }

    fn pump_until(what: &str, mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !ready() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            gtk::main_iteration_do(false);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn load(webview: &webkit2gtk::WebView, url: &str) {
        let finished = Rc::new(RefCell::new(false));
        let flag = finished.clone();
        let handler = webview.connect_load_changed(move |_, event| {
            if event == LoadEvent::Finished {
                *flag.borrow_mut() = true;
            }
        });
        webview.load_uri(url);
        pump_until(url, || *finished.borrow());
        webview.disconnect(handler);
    }

    fn evaluate(webview: &webkit2gtk::WebView, js: &str) -> Result<String, EvalError> {
        let slot = Rc::new(RefCell::new(None));
        let sink = slot.clone();
        platform::evaluate_in(webview, js, move |r| *sink.borrow_mut() = Some(r));
        pump_until("evaluate", || slot.borrow().is_some());
        let result = slot.borrow_mut().take().unwrap();
        result
    }

    fn page_snapshot(webview: &webkit2gtk::WebView, base: &str, path: &str, id: &str) -> Snapshot {
        load(webview, &format!("{base}{path}"));
        assert_eq!(evaluate(webview, "document.title").unwrap(), "fixture", "page script must be blocked by CSP");
        let json = evaluate(webview, &probe_script(spec(id))).unwrap();
        match parse_probe_payload(&ServiceId(id.to_string()), &json, 1) {
            ProbeResult::Page(s) => s,
            other => panic!("{id}: expected a page, got {other:?} from {json}"),
        }
    }

    fn spec(id: &str) -> &'static ExtractorSpec {
        EXTRACTORS.iter().find(|s| s.id == id).unwrap()
    }

    fn turn(role: Role, text: &str) -> ObservedTurn {
        ObservedTurn { role, text: text.to_string() }
    }

    #[test]
    #[ignore = "needs a display and WebKitGTK; run under xvfb-run with --ignored"]
    fn probe_round_trips_under_strict_csp() {
        let base = format!("http://127.0.0.1:{}", serve());
        gtk::init().unwrap();
        let window = gtk::Window::new(gtk::WindowType::Toplevel);
        let webview = webkit2gtk::WebView::new();
        window.add(&webview);
        window.show_all();

        let s = page_snapshot(&webview, &base, "/c/0123456789abcdef0123", "chatgpt");
        assert_eq!(s.conv.conversation.0, "0123456789abcdef0123");
        assert_eq!(s.turns.len(), MAX_TURNS_PER_PROBE);
        assert_eq!(s.context_turn, Some(turn(Role::Assistant, "t1 第1段")));
        assert_eq!(s.turns[0], turn(Role::User, "t2 第2段"));
        assert_eq!(s.turns.last(), Some(&turn(Role::Assistant, "t25 第25段")));
        assert_eq!(s.streaming, Some(false));

        let s = page_snapshot(&webview, &base, "/chat/0f1e2d3c-4b5a-6978-8796", "claude");
        assert_eq!(
            s.turns,
            vec![
                turn(Role::User, "hello claude"),
                turn(Role::Assistant, "hi there"),
                turn(Role::User, "second"),
                turn(Role::Assistant, "partial"),
            ]
        );
        assert_eq!(s.context_turn, None);
        assert_eq!(s.streaming, Some(true));

        let s = page_snapshot(&webview, &base, "/app/1a2b3c4d5e6f", "gemini");
        assert_eq!(s.turns, vec![turn(Role::User, "gemini question"), turn(Role::Assistant, "gemini answer")]);
        assert_eq!(s.streaming, None);

        let json = evaluate(&webview, &probe_script(spec("chatgpt"))).unwrap();
        assert_eq!(parse_probe_payload(&ServiceId("chatgpt".into()), &json, 1), ProbeResult::NoConversation);

        assert_eq!(
            evaluate(&webview, "1 + 1"),
            Err(EvalError::Engine("probe returned a non-string".to_string()))
        );

        let tamper = "window.__pageGlobal = 'page'; JSON.stringify = function () { return 'tampered'; }; 'set'";
        assert_eq!(evaluate_in_main_world(&webview, tamper), "set");
        assert_eq!(evaluate_in_main_world(&webview, "JSON.stringify({})"), "tampered");
        assert_eq!(evaluate(&webview, "typeof window.__pageGlobal").unwrap(), "undefined");
        let json = evaluate(&webview, &probe_script(spec("gemini"))).unwrap();
        assert!(matches!(
            parse_probe_payload(&ServiceId("gemini".into()), &json, 1),
            ProbeResult::Page(s) if s.turns.len() == 2
        ));
    }

    fn evaluate_in_main_world(webview: &webkit2gtk::WebView, js: &str) -> String {
        let slot = Rc::new(RefCell::new(None));
        let sink = slot.clone();
        webview.evaluate_javascript(js, None, None, None::<&webkit2gtk::gio::Cancellable>, move |r| {
            *sink.borrow_mut() = Some(r.map(|v| v.to_str().to_string()).map_err(|e| e.to_string()));
        });
        pump_until("main-world evaluate", || slot.borrow().is_some());
        let result = slot.borrow_mut().take().unwrap();
        result.unwrap()
    }
}
