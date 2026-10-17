//! Real `hy_memory.server` against a local OpenAI-compatible stub. Needs an
//! interpreter that can import `hy_memory`:
//! `ANYCHAT_HY_PYTHON=/path/to/python cargo test --lib memory::live_tests -- --ignored`

use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use reqwest::header::CONTENT_TYPE;

use super::model::*;
use super::sidecar;
use super::{sidecar_url, MemoryEngine};

/// Every chat completion gets the same extractor-shaped answer; embeddings are
/// deterministic unit vectors.
const MODEL_STUB: &str = r#"
import hashlib, json, sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
DIMS = 8
REPLY = json.dumps({"memory": [{"content": "用户住在杭州，喜欢喝乌龙茶。"}]}, ensure_ascii=False)
def vec(text):
    h = hashlib.sha256(text.encode()).digest()
    v = [h[i] / 255.0 - 0.5 for i in range(DIMS)]
    n = sum(x * x for x in v) ** 0.5 or 1.0
    return [x / n for x in v]
class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))) or b"{}")
        if self.path.endswith("/embeddings"):
            items = body["input"] if isinstance(body["input"], list) else [body["input"]]
            out = {"object": "list", "data": [{"object": "embedding", "index": i, "embedding": vec(str(t))} for i, t in enumerate(items)],
                   "model": body.get("model"), "usage": {"prompt_tokens": 1, "total_tokens": 1}}
        else:
            out = {"id": "stub", "object": "chat.completion", "created": 0, "model": body.get("model"),
                   "choices": [{"index": 0, "finish_reason": "stop", "message": {"role": "assistant", "content": REPLY}}],
                   "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}}
        data = json.dumps(out).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)
ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
"#;

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn wait_until(what: &str, limit: Duration, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn config(python: &str, stub_port: u16) -> RuntimeConfig {
    let raw: RawMemorySettings = serde_json::from_value(serde_json::json!({
        "enabled": true,
        "generation": { "baseUrl": format!("http://127.0.0.1:{stub_port}/v1"), "apiKey": "sk-stub", "model": "stub" },
        "embedding": { "source": { "kind": "reuse-generation" }, "model": "stub-embed", "dims": 8 },
        "pythonCommand": python,
    }))
    .unwrap();
    match desired_from(&raw, &[]) {
        Desired::On { config, .. } => config,
        other => panic!("expected a valid config, got {other:?}"),
    }
}

#[test]
#[ignore = "needs ANYCHAT_HY_PYTHON with hy-memory installed"]
fn add_then_list_round_trips_through_a_real_sidecar() {
    let python = std::env::var("ANYCHAT_HY_PYTHON").expect("set ANYCHAT_HY_PYTHON");
    let dir: PathBuf = std::env::temp_dir().join(format!("anychat-live-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    let stub_port = free_port();
    let _stub = KillOnDrop(
        Command::new(&python)
            .args(["-c", MODEL_STUB, &stub_port.to_string()])
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_until("model stub", Duration::from_secs(10), || TcpStream::connect(("127.0.0.1", stub_port)).is_ok());

    let pid_file = dir.join("sidecar.pid");
    let (process, port) = sidecar::spawn(&config(&python, stub_port), &dir.join("store"), &pid_file)
        .unwrap_or_else(|f| panic!("spawn failed: {f:?}"));
    let engine = MemoryEngine::new();
    let conv = ConvRef { service: ServiceId("chatgpt".into()), conversation: ConversationKey("c-live".into()) };
    let batch = Batch {
        conv: conv.clone(),
        turns: vec![
            BatchTurn { role: Role::User, content: "我住在杭州，喜欢喝乌龙茶。".into() },
            BatchTurn { role: Role::Assistant, content: "好的，记住了。".into() },
        ],
        fingerprints: vec![Fingerprint(1), Fingerprint(2)],
        observed_at_ms: 1_792_231_500_000,
    };
    let query = ListQuery { service_id: Some(conv.service.clone()), offset: 0, limit: 50 };

    let (deep_check, outcome, page) = tauri::async_runtime::block_on(async {
        let healthz = Instant::now() + Duration::from_secs(120);
        loop {
            let ok = engine
                .http
                .get(sidecar_url(port, "/healthz"))
                .timeout(Duration::from_secs(2))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success());
            if ok {
                break;
            }
            assert!(Instant::now() < healthz, "sidecar never became healthy");
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let status = engine.http.get(sidecar_url(port, "/api/v1/status")).send().await.unwrap();
        let code = status.status().as_u16();
        let deep_check = parse_deep_check(code, &status.text().await.unwrap());
        let outcome = engine.post_add(port, &batch).await;
        let listed = engine
            .http
            .post(sidecar_url(port, "/api/v1/list"))
            .header(CONTENT_TYPE, "application/json")
            .body(list_request_body(&query))
            .send()
            .await
            .unwrap();
        let page = parse_list_response(&listed.text().await.unwrap(), &query);
        (deep_check, outcome, page)
    });

    let started = Instant::now();
    process.terminate();
    let stop_took = started.elapsed();

    assert_eq!(deep_check, Ok(()));
    let AddOutcome::Stored { memory_ids } = outcome else { panic!("expected Stored, got {outcome:?}") };
    assert_eq!(memory_ids.len(), 1);
    let page = page.expect("list parses");
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].id, memory_ids[0]);
    assert_eq!(page.items[0].text, "用户住在杭州，喜欢喝乌龙茶。");
    assert_eq!(page.items[0].kind, MemoryKind::Fact);
    assert_eq!(page.items[0].service_id.as_deref(), Some("chatgpt"));
    assert_eq!(page.items[0].conversation_key.as_deref(), Some("c-live"));

    assert!(stop_took < Duration::from_secs(6), "terminate took {stop_took:?}");
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err(), "sidecar still listening after terminate");
    assert!(!pid_file.exists());
    eprintln!("terminate took {stop_took:?}");
    let _ = fs::remove_dir_all(&dir);
}
