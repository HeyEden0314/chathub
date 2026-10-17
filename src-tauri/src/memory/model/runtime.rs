use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::ingest::{fnv1a64, fnv_fields, GenerationIdentity, ServiceId};

// ── settings boundary ───────────────────────────────────────────────────────

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RawEndpoint {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum RawEmbeddingSource {
    #[default]
    ReuseGeneration,
    #[serde(rename_all = "camelCase")]
    Separate {
        #[serde(default)]
        base_url: String,
        #[serde(default)]
        api_key: String,
    },
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RawEmbedding {
    pub source: RawEmbeddingSource,
    pub model: String,
    pub dims: u32,
}

/// serde mirror of TS `MemorySettings`. Untrusted shape.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RawMemorySettings {
    pub enabled: bool,
    pub capture_service_ids: Vec<String>,
    pub generation: RawEndpoint,
    pub embedding: RawEmbedding,
    pub python_command: String,
}

/// Debug prints `***`. Only `sidecar_env` reads the inner value.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Secret(String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Endpoint {
    /// Validated http(s) URL, or `None` for the OpenAI default host.
    pub base_url: Option<String>,
    pub api_key: Secret,
    pub model: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EmbeddingConfig {
    /// `ReuseGeneration` is expanded to a copy of the generation endpoint.
    pub base_url: Option<String>,
    pub api_key: Secret,
    pub model: String,
    pub dims: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PythonCommand {
    Auto,
    Explicit(String),
}

/// Only constructible through `desired_from` (`Secret` is opaque outside this module).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RuntimeConfig {
    pub generation: Endpoint,
    pub embedding: EmbeddingConfig,
    pub python: PythonCommand,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ConfigProblem {
    /// One of `CONFIG_FIELDS`, matching TS `ConfigField`.
    pub field: &'static str,
    pub message: String,
}

#[cfg(test)]
pub const CONFIG_FIELDS: &[&str] = &[
    "generation.baseUrl",
    "generation.apiKey",
    "generation.model",
    "embedding.baseUrl",
    "embedding.apiKey",
    "embedding.model",
    "embedding.dims",
    "pythonCommand",
    "captureServiceIds",
];

#[derive(Clone, Debug, PartialEq)]
pub enum Desired {
    Off,
    NeedsConfig(Vec<ConfigProblem>),
    On {
        config: RuntimeConfig,
        targets: Vec<ServiceId>,
    },
}

pub const MAX_EMBEDDING_DIMS: u32 = 16_384;

/// The single validator. TS does not validate a second time.
pub fn desired_from(raw: &RawMemorySettings, capture_targets: &[String]) -> Desired {
    if !raw.enabled {
        return Desired::Off;
    }
    let mut problems = Vec::new();
    let mut problem = |field: &'static str, message: &str| {
        problems.push(ConfigProblem {
            field,
            message: message.to_string(),
        })
    };

    let gen_base = parse_base_url(&raw.generation.base_url);
    if gen_base.is_err() {
        problem(
            "generation.baseUrl",
            "接口地址需以 http:// 或 https:// 开头",
        );
    }
    let gen_key = raw.generation.api_key.trim();
    if gen_key.is_empty() {
        problem("generation.apiKey", "请填写 API Key（本地模型可填任意值）");
    }
    let gen_model = raw.generation.model.trim();
    if gen_model.is_empty() {
        problem("generation.model", "请填写生成模型名称");
    }

    let (emb_base, emb_key) = match &raw.embedding.source {
        RawEmbeddingSource::ReuseGeneration => (gen_base.clone(), gen_key.to_string()),
        RawEmbeddingSource::Separate { base_url, api_key } => {
            let base = parse_base_url(base_url);
            if base.is_err() {
                problem("embedding.baseUrl", "接口地址需以 http:// 或 https:// 开头");
            }
            if api_key.trim().is_empty() {
                problem("embedding.apiKey", "请填写嵌入接口的 API Key");
            }
            (base, api_key.trim().to_string())
        }
    };
    let emb_model = raw.embedding.model.trim();
    if emb_model.is_empty() {
        problem("embedding.model", "请填写嵌入模型名称");
    }
    if raw.embedding.dims == 0 || raw.embedding.dims > MAX_EMBEDDING_DIMS {
        problem("embedding.dims", "向量维度需为 1 到 16384 之间的整数");
    }
    let python = raw.python_command.trim();
    if python.contains(['\n', '\r', '\0']) {
        problem("pythonCommand", "Python 命令不能包含换行");
    }

    let (Ok(gen_base), Ok(emb_base)) = (gen_base, emb_base) else {
        return Desired::NeedsConfig(problems);
    };
    if !problems.is_empty() {
        return Desired::NeedsConfig(problems);
    }

    let mut targets: Vec<ServiceId> = capture_targets
        .iter()
        .map(|id| id.trim())
        .filter(|id| !id.is_empty())
        .map(|id| ServiceId(id.to_string()))
        .collect();
    targets.sort();
    targets.dedup();

    Desired::On {
        config: RuntimeConfig {
            generation: Endpoint {
                base_url: gen_base,
                api_key: Secret(gen_key.to_string()),
                model: gen_model.to_string(),
            },
            embedding: EmbeddingConfig {
                base_url: emb_base,
                api_key: Secret(emb_key),
                model: emb_model.to_string(),
                dims: raw.embedding.dims,
            },
            python: if python.is_empty() {
                PythonCommand::Auto
            } else {
                PythonCommand::Explicit(python.to_string())
            },
        },
        targets,
    }
}

/// `''` ⇒ `None` (OpenAI default host). Otherwise an http(s) URL with a host.
fn parse_base_url(raw: &str) -> Result<Option<String>, ()> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Ok(None);
    }
    let lower = trimmed.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err(());
    }
    if trimmed.chars().any(char::is_whitespace) || url_host(trimmed).is_none() {
        return Err(());
    }
    Ok(Some(trimmed.to_string()))
}

/// Lowercased host of an `scheme://[user@]host[:port]/…` URL. IPv6 keeps its brackets.
fn url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if host_port.starts_with('[') {
        host_port.split_inclusive(']').next()?
    } else {
        host_port.split(':').next()?
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

pub const OPENAI_DEFAULT_HOST: &str = "api.openai.com";

pub fn endpoint_host(base_url: Option<&str>) -> String {
    base_url
        .and_then(url_host)
        .unwrap_or_else(|| OPENAI_DEFAULT_HOST.to_string())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeFingerprint(pub u64);

/// Covers generation + embedding + python. Excludes targets (they never restart).
pub fn runtime_fingerprint(cfg: &RuntimeConfig) -> RuntimeFingerprint {
    let python = match &cfg.python {
        PythonCommand::Auto => String::new(),
        PythonCommand::Explicit(cmd) => cmd.clone(),
    };
    RuntimeFingerprint(fnv_fields(&[
        cfg.generation.base_url.as_deref().unwrap_or("").as_bytes(),
        cfg.generation.api_key.0.as_bytes(),
        cfg.generation.model.as_bytes(),
        cfg.embedding.base_url.as_deref().unwrap_or("").as_bytes(),
        cfg.embedding.api_key.0.as_bytes(),
        cfg.embedding.model.as_bytes(),
        &cfg.embedding.dims.to_le_bytes(),
        python.as_bytes(),
    ]))
}

pub fn generation_identity(cfg: &RuntimeConfig) -> GenerationIdentity {
    GenerationIdentity(fnv_fields(&[
        cfg.generation.model.as_bytes(),
        endpoint_host(cfg.generation.base_url.as_deref()).as_bytes(),
    ]))
}

/// `"{sanitized model}-{dims}-{fnv8(host)}"`. Selects `<app_data>/memory/<store_id>/`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StoreId(pub String);

const STORE_MODEL_SLUG_MAX: usize = 48;

pub fn store_id(emb: &EmbeddingConfig) -> StoreId {
    let mut slug = String::new();
    for c in emb.model.to_ascii_lowercase().chars() {
        let keep = c.is_ascii_alphanumeric() || c == '.' || c == '_';
        if keep {
            slug.push(c);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug: String = slug
        .trim_matches('-')
        .chars()
        .take(STORE_MODEL_SLUG_MAX)
        .collect();
    let slug = if slug.is_empty() {
        "model".to_string()
    } else {
        slug
    };
    let host_hash = fnv1a64(endpoint_host(emb.base_url.as_deref()).as_bytes());
    StoreId(format!("{}-{}-{:08x}", slug, emb.dims, host_hash >> 32))
}

/// Inherited variables that Hy-Memory would otherwise read as fallbacks.
pub const INHERITED_ENV_TO_STRIP: &[&str] =
    &["MEMORY_", "LLM_", "OPENAI_", "EMBEDDING_", "HY_MEMORY_"];

#[derive(Debug)]
pub struct EnvPlan {
    pub set: Vec<(String, String)>,
    pub remove_prefixes: &'static [&'static str],
    /// = store dir, so the sidecar never runs inside the user's cwd.
    pub cwd: PathBuf,
}

/// Base URLs are always set, empty for the OpenAI default, so a `.env` next to the
/// installed package cannot fill them in.
pub fn sidecar_env(cfg: &RuntimeConfig, port: u16, store_dir: &Path) -> EnvPlan {
    let pair = |k: &str, v: &str| (k.to_string(), v.to_string());
    EnvPlan {
        set: vec![
            pair("MEMORY_MODE", "pro"),
            pair("MEMORY_LLM_MODEL", &cfg.generation.model),
            pair("MEMORY_LLM_API_KEY", &cfg.generation.api_key.0),
            pair(
                "MEMORY_LLM_BASE_URL",
                cfg.generation.base_url.as_deref().unwrap_or(""),
            ),
            pair("MEMORY_EMBEDDER_MODEL", &cfg.embedding.model),
            pair("MEMORY_EMBEDDER_API_KEY", &cfg.embedding.api_key.0),
            pair(
                "MEMORY_EMBEDDER_BASE_URL",
                cfg.embedding.base_url.as_deref().unwrap_or(""),
            ),
            pair("MEMORY_EMBEDDING_DIMS", &cfg.embedding.dims.to_string()),
            pair("MEMORY_DATA_DIR", &store_dir.join("hy").to_string_lossy()),
            pair("HY_MEMORY_SERVER_HOST", "127.0.0.1"),
            pair("HY_MEMORY_SERVER_PORT", &port.to_string()),
        ],
        remove_prefixes: INHERITED_ENV_TO_STRIP,
        cwd: store_dir.to_path_buf(),
    }
}

/// Interpreters to try, in order. An explicit command is one program path, not a
/// shell line, so paths with spaces work.
pub fn python_candidates(cmd: &PythonCommand, windows: bool) -> Vec<Vec<String>> {
    let argv = |parts: &[&str]| parts.iter().map(|p| p.to_string()).collect::<Vec<_>>();
    match cmd {
        PythonCommand::Explicit(program) => vec![vec![program.clone()]],
        PythonCommand::Auto if windows => {
            vec![argv(&["py", "-3"]), argv(&["python"]), argv(&["python3"])]
        }
        PythonCommand::Auto => vec![argv(&["python3"]), argv(&["python"])],
    }
}

pub const HY_MEMORY_PIN: &str = "hy-memory==2.0.3";

pub fn install_hint(python: &str) -> String {
    format!("{python} -m pip install {HY_MEMORY_PIN}")
}

// ── sidecar supervisor: pure transitions, effects run by the shell ──────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SidecarFailure {
    PythonNotFound {
        tried: Vec<String>,
    },
    ModuleMissing {
        python: String,
    },
    ExitedOnStart {
        code: Option<i32>,
        stderr_tail: String,
    },
    HealthTimeout {
        after_ms: u64,
    },
    ModelCheckFailed {
        message: String,
    },
    CrashLoop {
        restarts: u8,
        stderr_tail: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SidecarPhase {
    Off,
    NeedsConfig,
    /// Configured, process stopped. Lazy start on demand; idle stop returns here.
    Idle,
    /// `port == 0` until the shell reports `Spawned`.
    Starting {
        port: u16,
        attempt: u8,
        since_ms: u64,
    },
    /// `verified`: deep `/api/v1/status` passed for this runtime fingerprint.
    Ready {
        port: u16,
        last_used_ms: u64,
        verified: bool,
    },
    Backoff {
        attempt: u8,
        until_ms: u64,
        last_exit: String,
    },
    Failed(SidecarFailure),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SidecarEvent {
    /// From `memory_sync_runtime`. `restart` = runtime fingerprint changed.
    Desired {
        on: bool,
        valid: bool,
        restart: bool,
    },
    /// Ingest has a batch, or list was called.
    Demand,
    Spawned {
        port: u16,
    },
    SpawnFailed(SidecarFailure),
    Healthy,
    DeepCheckPassed,
    DeepCheckFailed {
        message: String,
    },
    Exited {
        code: Option<i32>,
        stderr_tail: String,
    },
    Used,
    Tick,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SidecarEffect {
    /// Preflight `<python> -c "import hy_memory"`, pick a free port, spawn.
    Spawn {
        attempt: u8,
    },
    PollHealth,
    /// The shell may answer `DeepCheckPassed` without a request when this runtime
    /// fingerprint already passed in this session.
    DeepCheck,
    Kill,
    /// Wake `ensure_runtime` waiters with Ready(port) or the terminal phase.
    NotifyWaiters,
}

pub const IDLE_STOP: Duration = Duration::from_secs(10 * 60);
pub const HEALTH_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_RESTARTS: u8 = 3;
pub const BACKOFF_STEPS: [Duration; 3] = [
    Duration::from_secs(5),
    Duration::from_secs(30),
    Duration::from_secs(120),
];

pub fn describe_exit(code: Option<i32>, stderr_tail: &str) -> String {
    let code = code.map_or_else(|| "signal".to_string(), |c| format!("code {c}"));
    match stderr_tail.trim() {
        "" => code,
        tail => format!("{code}: {tail}"),
    }
}

fn backoff_after_exit(
    attempt: u8,
    now_ms: u64,
    last_exit: String,
) -> (SidecarPhase, Vec<SidecarEffect>) {
    let step = BACKOFF_STEPS[usize::from(attempt.clamp(1, 3)) - 1];
    (
        SidecarPhase::Backoff {
            attempt,
            until_ms: now_ms + step.as_millis() as u64,
            last_exit,
        },
        vec![SidecarEffect::NotifyWaiters],
    )
}

/// Total over (phase, event). Only this function changes `SidecarPhase`.
pub fn step(
    phase: &SidecarPhase,
    event: SidecarEvent,
    now_ms: u64,
) -> (SidecarPhase, Vec<SidecarEffect>) {
    use SidecarEffect as E;
    use SidecarPhase as P;

    let stay = || (phase.clone(), Vec::new());
    let stop_to = |next: P| (next, vec![E::Kill, E::NotifyWaiters]);

    match (phase, event) {
        (_, SidecarEvent::Desired { on: false, .. }) => stop_to(P::Off),
        (_, SidecarEvent::Desired { valid: false, .. }) => stop_to(P::NeedsConfig),
        (_, SidecarEvent::Desired { restart: true, .. }) => stop_to(P::Idle),
        (P::Off | P::NeedsConfig, SidecarEvent::Desired { .. }) => (P::Idle, Vec::new()),
        (_, SidecarEvent::Desired { .. }) => stay(),

        (P::Idle, SidecarEvent::Demand) => (
            P::Starting {
                port: 0,
                attempt: 1,
                since_ms: now_ms,
            },
            vec![E::Spawn { attempt: 1 }],
        ),
        (
            P::Ready {
                port,
                verified: true,
                ..
            },
            SidecarEvent::Demand,
        ) => (
            P::Ready {
                port: *port,
                last_used_ms: now_ms,
                verified: true,
            },
            vec![E::NotifyWaiters],
        ),
        (
            P::Starting { .. }
            | P::Ready {
                verified: false, ..
            },
            SidecarEvent::Demand,
        ) => stay(),
        (_, SidecarEvent::Demand) => (phase.clone(), vec![E::NotifyWaiters]),

        (
            P::Starting {
                attempt, since_ms, ..
            },
            SidecarEvent::Spawned { port },
        ) => (
            P::Starting {
                port,
                attempt: *attempt,
                since_ms: *since_ms,
            },
            vec![E::PollHealth],
        ),
        (_, SidecarEvent::Spawned { .. }) => (phase.clone(), vec![E::Kill]),

        (P::Starting { .. }, SidecarEvent::SpawnFailed(failure)) => {
            (P::Failed(failure), vec![E::NotifyWaiters])
        }
        (_, SidecarEvent::SpawnFailed(_)) => stay(),

        (P::Starting { port, .. }, SidecarEvent::Healthy) if *port != 0 => (
            P::Ready {
                port: *port,
                last_used_ms: now_ms,
                verified: false,
            },
            vec![E::DeepCheck],
        ),
        (_, SidecarEvent::Healthy) => stay(),

        (
            P::Ready {
                port,
                verified: false,
                ..
            },
            SidecarEvent::DeepCheckPassed,
        ) => (
            P::Ready {
                port: *port,
                last_used_ms: now_ms,
                verified: true,
            },
            vec![E::NotifyWaiters],
        ),
        (_, SidecarEvent::DeepCheckPassed) => stay(),

        (P::Ready { .. }, SidecarEvent::DeepCheckFailed { message }) => {
            stop_to(P::Failed(SidecarFailure::ModelCheckFailed { message }))
        }
        (_, SidecarEvent::DeepCheckFailed { .. }) => stay(),

        (P::Starting { attempt: 1, .. }, SidecarEvent::Exited { code, stderr_tail }) => (
            P::Failed(SidecarFailure::ExitedOnStart { code, stderr_tail }),
            vec![E::NotifyWaiters],
        ),
        (P::Starting { attempt, .. }, SidecarEvent::Exited { code, stderr_tail }) => {
            backoff_after_exit(*attempt, now_ms, describe_exit(code, &stderr_tail))
        }
        (P::Ready { .. }, SidecarEvent::Exited { code, stderr_tail }) => {
            backoff_after_exit(1, now_ms, describe_exit(code, &stderr_tail))
        }
        (_, SidecarEvent::Exited { .. }) => stay(),

        (P::Ready { port, verified, .. }, SidecarEvent::Used) => (
            P::Ready {
                port: *port,
                last_used_ms: now_ms,
                verified: *verified,
            },
            Vec::new(),
        ),
        (_, SidecarEvent::Used) => stay(),

        (P::Starting { port, since_ms, .. }, SidecarEvent::Tick)
            if *port != 0
                && now_ms.saturating_sub(*since_ms) >= HEALTH_TIMEOUT.as_millis() as u64 =>
        {
            stop_to(P::Failed(SidecarFailure::HealthTimeout {
                after_ms: now_ms.saturating_sub(*since_ms),
            }))
        }
        (P::Ready { last_used_ms, .. }, SidecarEvent::Tick)
            if now_ms.saturating_sub(*last_used_ms) >= IDLE_STOP.as_millis() as u64 =>
        {
            (P::Idle, vec![E::Kill])
        }
        (
            P::Backoff {
                attempt,
                until_ms,
                last_exit,
            },
            SidecarEvent::Tick,
        ) if now_ms >= *until_ms => {
            if *attempt >= MAX_RESTARTS {
                (
                    P::Failed(SidecarFailure::CrashLoop {
                        restarts: *attempt,
                        stderr_tail: last_exit.clone(),
                    }),
                    vec![E::NotifyWaiters],
                )
            } else {
                let next = attempt + 1;
                (
                    P::Starting {
                        port: 0,
                        attempt: next,
                        since_ms: now_ms,
                    },
                    vec![E::Spawn { attempt: next }],
                )
            }
        }
        (_, SidecarEvent::Tick) => stay(),
    }
}

pub const SWEEP_INTERVAL: Duration = Duration::from_secs(30);
pub const EVAL_TIMEOUT: Duration = Duration::from_secs(3);
pub const ADD_TIMEOUT: Duration = Duration::from_secs(300);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn raw_settings() -> RawMemorySettings {
        RawMemorySettings {
            enabled: true,
            capture_service_ids: vec!["chatgpt".into()],
            generation: RawEndpoint {
                base_url: "https://api.example.com/v1/".into(),
                api_key: "sk-test".into(),
                model: "gpt-4.1-mini".into(),
            },
            embedding: RawEmbedding {
                source: RawEmbeddingSource::ReuseGeneration,
                model: "text-embedding-3-small".into(),
                dims: 1536,
            },
            python_command: String::new(),
        }
    }

    fn config_of(raw: &RawMemorySettings) -> RuntimeConfig {
        match desired_from(raw, &["chatgpt".into()]) {
            Desired::On { config, .. } => config,
            other => panic!("expected On, got {other:?}"),
        }
    }

    #[test]
    fn desired_from_off_when_disabled() {
        let raw = RawMemorySettings {
            enabled: false,
            ..raw_settings()
        };
        assert_eq!(desired_from(&raw, &[]), Desired::Off);
    }

    #[test]
    fn desired_from_validates_fields() {
        let mut raw = raw_settings();
        raw.generation.base_url = "ftp://x".into();
        raw.generation.api_key = " ".into();
        raw.generation.model = String::new();
        raw.embedding.source = RawEmbeddingSource::Separate {
            base_url: "not a url".into(),
            api_key: String::new(),
        };
        raw.embedding.model = String::new();
        raw.embedding.dims = 0;
        raw.python_command = "py\nthon".into();
        let Desired::NeedsConfig(problems) = desired_from(&raw, &[]) else {
            panic!("expected NeedsConfig");
        };
        let fields: Vec<&str> = problems.iter().map(|p| p.field).collect();
        assert_eq!(
            fields,
            vec![
                "generation.baseUrl",
                "generation.apiKey",
                "generation.model",
                "embedding.baseUrl",
                "embedding.apiKey",
                "embedding.model",
                "embedding.dims",
                "pythonCommand",
            ]
        );
        assert!(fields.iter().all(|f| CONFIG_FIELDS.contains(f)));
    }

    #[test]
    fn desired_from_resolves_reuse_and_sorts_targets() {
        let raw = raw_settings();
        let Desired::On { config, targets } =
            desired_from(&raw, &["gemini".into(), "chatgpt".into(), "gemini".into()])
        else {
            panic!("expected On");
        };
        assert_eq!(
            targets,
            vec![ServiceId("chatgpt".into()), ServiceId("gemini".into())]
        );
        assert_eq!(
            config.generation.base_url.as_deref(),
            Some("https://api.example.com/v1")
        );
        assert_eq!(config.embedding.base_url, config.generation.base_url);
        assert_eq!(config.embedding.api_key, config.generation.api_key);
        assert_eq!(config.python, PythonCommand::Auto);
        assert!(!format!("{config:?}").contains("sk-test"));
    }

    #[test]
    fn raw_settings_deserialize_from_the_ts_shape() {
        let raw: RawMemorySettings = serde_json::from_value(json!({
            "enabled": true,
            "captureServiceIds": ["chatgpt"],
            "generation": {"baseUrl": "", "apiKey": "k", "model": "m"},
            "embedding": {"source": {"kind": "separate", "baseUrl": "http://localhost:11434/v1", "apiKey": "e"}, "model": "nomic", "dims": 768},
            "pythonCommand": "/usr/bin/python3"
        }))
        .unwrap();
        let cfg = config_of(&raw);
        assert_eq!(cfg.generation.base_url, None);
        assert_eq!(
            cfg.embedding.base_url.as_deref(),
            Some("http://localhost:11434/v1")
        );
        assert_eq!(
            cfg.python,
            PythonCommand::Explicit("/usr/bin/python3".into())
        );

        let reuse: RawMemorySettings =
            serde_json::from_value(json!({"embedding": {"source": {"kind": "reuse-generation"}}}))
                .unwrap();
        assert!(matches!(
            reuse.embedding.source,
            RawEmbeddingSource::ReuseGeneration
        ));
    }

    #[test]
    fn runtime_fingerprint_ignores_targets_and_tracks_runtime_fields() {
        let raw = raw_settings();
        let a = match desired_from(&raw, &["chatgpt".into()]) {
            Desired::On { config, .. } => runtime_fingerprint(&config),
            _ => unreachable!(),
        };
        let b = match desired_from(&raw, &["claude".into()]) {
            Desired::On { config, .. } => runtime_fingerprint(&config),
            _ => unreachable!(),
        };
        assert_eq!(a, b);
        let mut changed = raw_settings();
        changed.generation.api_key = "sk-other".into();
        assert_ne!(a, runtime_fingerprint(&config_of(&changed)));
        let mut python = raw_settings();
        python.python_command = "python3.12".into();
        assert_ne!(a, runtime_fingerprint(&config_of(&python)));
    }

    #[test]
    fn generation_identity_ignores_the_key() {
        let a = generation_identity(&config_of(&raw_settings()));
        let mut other_key = raw_settings();
        other_key.generation.api_key = "sk-other".into();
        assert_eq!(a, generation_identity(&config_of(&other_key)));
        let mut other_model = raw_settings();
        other_model.generation.model = "gpt-4.1".into();
        assert_ne!(a, generation_identity(&config_of(&other_model)));
    }

    #[test]
    fn store_id_is_stable_and_follows_embedding_identity() {
        let cfg = config_of(&raw_settings());
        let id = store_id(&cfg.embedding);
        assert_eq!(id, store_id(&cfg.embedding));
        assert!(id.0.starts_with("text-embedding-3-small-1536-"), "{}", id.0);
        assert_eq!(id.0.len(), "text-embedding-3-small-1536-".len() + 8);

        let mut gen_changed = raw_settings();
        gen_changed.generation.model = "deepseek-chat".into();
        assert_eq!(store_id(&config_of(&gen_changed).embedding), id);

        let mut dims_changed = raw_settings();
        dims_changed.embedding.dims = 512;
        assert_ne!(store_id(&config_of(&dims_changed).embedding), id);

        let mut weird = raw_settings();
        weird.embedding.model = "BAAI/bge-m3:latest".into();
        assert!(store_id(&config_of(&weird).embedding)
            .0
            .starts_with("baai-bge-m3-latest-1536-"));
    }

    #[test]
    fn endpoint_host_parses_urls() {
        assert_eq!(endpoint_host(None), "api.openai.com");
        assert_eq!(
            endpoint_host(Some("https://User@API.Example.com:8443/v1?x=1")),
            "api.example.com"
        );
        assert_eq!(endpoint_host(Some("http://[::1]:11434/v1")), "[::1]");
        assert!(parse_base_url("https://").is_err());
    }

    #[test]
    fn sidecar_env_strips_inherited_vars_and_sets_cwd() {
        let cfg = config_of(&raw_settings());
        let dir = Path::new("/data/memory/store");
        let plan = sidecar_env(&cfg, 41234, dir);
        let get = |k: &str| {
            plan.set
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(plan.cwd, dir);
        assert_eq!(plan.remove_prefixes, INHERITED_ENV_TO_STRIP);
        for prefix in ["MEMORY_", "LLM_", "OPENAI_", "EMBEDDING_", "HY_MEMORY_"] {
            assert!(plan.remove_prefixes.contains(&prefix));
        }
        assert_eq!(get("MEMORY_MODE"), Some("pro"));
        assert_eq!(get("MEMORY_LLM_MODEL"), Some("gpt-4.1-mini"));
        assert_eq!(get("MEMORY_LLM_API_KEY"), Some("sk-test"));
        assert_eq!(
            get("MEMORY_LLM_BASE_URL"),
            Some("https://api.example.com/v1")
        );
        assert_eq!(get("MEMORY_EMBEDDING_DIMS"), Some("1536"));
        assert_eq!(get("HY_MEMORY_SERVER_HOST"), Some("127.0.0.1"));
        assert_eq!(get("HY_MEMORY_SERVER_PORT"), Some("41234"));
        assert_eq!(
            get("MEMORY_DATA_DIR").map(PathBuf::from),
            Some(dir.join("hy"))
        );

        let mut default_host = raw_settings();
        default_host.generation.base_url = String::new();
        let plan = sidecar_env(&config_of(&default_host), 1, dir);
        assert!(plan
            .set
            .contains(&("MEMORY_LLM_BASE_URL".to_string(), String::new())));
    }

    #[test]
    fn python_candidates_per_platform() {
        assert_eq!(
            python_candidates(&PythonCommand::Auto, false),
            vec![vec!["python3"], vec!["python"]]
        );
        assert_eq!(
            python_candidates(&PythonCommand::Auto, true)[0],
            vec!["py", "-3"]
        );
        assert_eq!(
            python_candidates(
                &PythonCommand::Explicit("C:\\Program Files\\Python\\python.exe".into()),
                true
            ),
            vec![vec!["C:\\Program Files\\Python\\python.exe"]]
        );
    }

    // ── step transition table ──

    fn ready(verified: bool) -> SidecarPhase {
        SidecarPhase::Ready {
            port: 4000,
            last_used_ms: 0,
            verified,
        }
    }

    #[test]
    fn step_lazy_start_to_verified_ready() {
        use SidecarEffect as E;
        let (p, fx) = step(&SidecarPhase::Idle, SidecarEvent::Demand, 10);
        assert_eq!(
            p,
            SidecarPhase::Starting {
                port: 0,
                attempt: 1,
                since_ms: 10
            }
        );
        assert_eq!(fx, vec![E::Spawn { attempt: 1 }]);

        let (p, fx) = step(&p, SidecarEvent::Spawned { port: 4000 }, 20);
        assert_eq!(
            p,
            SidecarPhase::Starting {
                port: 4000,
                attempt: 1,
                since_ms: 10
            }
        );
        assert_eq!(fx, vec![E::PollHealth]);

        let (p, fx) = step(&p, SidecarEvent::Healthy, 30);
        assert_eq!(
            p,
            SidecarPhase::Ready {
                port: 4000,
                last_used_ms: 30,
                verified: false
            }
        );
        assert_eq!(fx, vec![E::DeepCheck]);

        let (p, fx) = step(&p, SidecarEvent::Demand, 35);
        assert_eq!(fx, vec![], "waiters keep waiting during the deep check");

        let (p, fx) = step(&p, SidecarEvent::DeepCheckPassed, 40);
        assert_eq!(
            p,
            SidecarPhase::Ready {
                port: 4000,
                last_used_ms: 40,
                verified: true
            }
        );
        assert_eq!(fx, vec![E::NotifyWaiters]);

        let (p, fx) = step(&p, SidecarEvent::Demand, 50);
        assert_eq!(
            p,
            SidecarPhase::Ready {
                port: 4000,
                last_used_ms: 50,
                verified: true
            }
        );
        assert_eq!(fx, vec![E::NotifyWaiters]);
    }

    #[test]
    fn step_deep_check_failure_is_terminal() {
        let (p, fx) = step(
            &ready(false),
            SidecarEvent::DeepCheckFailed {
                message: "401 Unauthorized".into(),
            },
            1,
        );
        assert_eq!(
            p,
            SidecarPhase::Failed(SidecarFailure::ModelCheckFailed {
                message: "401 Unauthorized".into()
            })
        );
        assert_eq!(fx, vec![SidecarEffect::Kill, SidecarEffect::NotifyWaiters]);
        let (p2, fx) = step(&p, SidecarEvent::Demand, 2);
        assert_eq!((p2, fx), (p, vec![SidecarEffect::NotifyWaiters]));
    }

    #[test]
    fn step_spawn_failures_and_early_exit() {
        let starting = SidecarPhase::Starting {
            port: 0,
            attempt: 1,
            since_ms: 0,
        };
        let failure = SidecarFailure::ModuleMissing {
            python: "python3".into(),
        };
        assert_eq!(
            step(&starting, SidecarEvent::SpawnFailed(failure.clone()), 1),
            (
                SidecarPhase::Failed(failure),
                vec![SidecarEffect::NotifyWaiters]
            )
        );
        let spawned = SidecarPhase::Starting {
            port: 4000,
            attempt: 1,
            since_ms: 0,
        };
        let (p, _) = step(
            &spawned,
            SidecarEvent::Exited {
                code: Some(1),
                stderr_tail: "Traceback".into(),
            },
            1,
        );
        assert_eq!(
            p,
            SidecarPhase::Failed(SidecarFailure::ExitedOnStart {
                code: Some(1),
                stderr_tail: "Traceback".into()
            })
        );
    }

    #[test]
    fn step_health_timeout() {
        let spawned = SidecarPhase::Starting {
            port: 4000,
            attempt: 1,
            since_ms: 0,
        };
        assert_eq!(step(&spawned, SidecarEvent::Tick, 1_000).0, spawned);
        let timeout = HEALTH_TIMEOUT.as_millis() as u64;
        let (p, fx) = step(&spawned, SidecarEvent::Tick, timeout);
        assert_eq!(
            p,
            SidecarPhase::Failed(SidecarFailure::HealthTimeout { after_ms: timeout })
        );
        assert_eq!(fx, vec![SidecarEffect::Kill, SidecarEffect::NotifyWaiters]);
    }

    #[test]
    fn step_crash_backoff_then_crash_loop() {
        let (p, fx) = step(
            &ready(true),
            SidecarEvent::Exited {
                code: None,
                stderr_tail: String::new(),
            },
            100,
        );
        assert_eq!(
            p,
            SidecarPhase::Backoff {
                attempt: 1,
                until_ms: 5_100,
                last_exit: "signal".into()
            }
        );
        assert_eq!(fx, vec![SidecarEffect::NotifyWaiters]);
        assert_eq!(step(&p, SidecarEvent::Tick, 5_000).0, p);

        let (p, fx) = step(&p, SidecarEvent::Tick, 5_100);
        assert_eq!(
            p,
            SidecarPhase::Starting {
                port: 0,
                attempt: 2,
                since_ms: 5_100
            }
        );
        assert_eq!(fx, vec![SidecarEffect::Spawn { attempt: 2 }]);
        let (p, _) = step(&p, SidecarEvent::Spawned { port: 4001 }, 5_200);
        let (p, _) = step(
            &p,
            SidecarEvent::Exited {
                code: Some(1),
                stderr_tail: "oom".into(),
            },
            6_000,
        );
        assert_eq!(
            p,
            SidecarPhase::Backoff {
                attempt: 2,
                until_ms: 36_000,
                last_exit: "code 1: oom".into()
            }
        );

        let (p, _) = step(&p, SidecarEvent::Tick, 36_000);
        let (p, _) = step(&p, SidecarEvent::Spawned { port: 4002 }, 36_100);
        let (p, _) = step(
            &p,
            SidecarEvent::Exited {
                code: Some(1),
                stderr_tail: "oom".into(),
            },
            37_000,
        );
        assert_eq!(
            p,
            SidecarPhase::Backoff {
                attempt: 3,
                until_ms: 157_000,
                last_exit: "code 1: oom".into()
            }
        );
        let (p, fx) = step(&p, SidecarEvent::Tick, 157_000);
        assert_eq!(
            p,
            SidecarPhase::Failed(SidecarFailure::CrashLoop {
                restarts: MAX_RESTARTS,
                stderr_tail: "code 1: oom".into()
            })
        );
        assert_eq!(fx, vec![SidecarEffect::NotifyWaiters]);
    }

    #[test]
    fn step_idle_stop_and_use_refresh() {
        let idle_ms = IDLE_STOP.as_millis() as u64;
        let (p, _) = step(&ready(true), SidecarEvent::Used, idle_ms - 1);
        assert_eq!(step(&p, SidecarEvent::Tick, idle_ms).0, p);
        assert_eq!(
            step(&ready(true), SidecarEvent::Tick, idle_ms),
            (SidecarPhase::Idle, vec![SidecarEffect::Kill])
        );
    }

    #[test]
    fn step_desired_changes() {
        use SidecarEffect as E;
        let stop = vec![E::Kill, E::NotifyWaiters];
        let off = SidecarEvent::Desired {
            on: false,
            valid: true,
            restart: false,
        };
        let invalid = SidecarEvent::Desired {
            on: true,
            valid: false,
            restart: false,
        };
        let restart = SidecarEvent::Desired {
            on: true,
            valid: true,
            restart: true,
        };
        let same = SidecarEvent::Desired {
            on: true,
            valid: true,
            restart: false,
        };

        assert_eq!(
            step(&ready(true), off.clone(), 0),
            (SidecarPhase::Off, stop.clone())
        );
        assert_eq!(
            step(&ready(true), invalid.clone(), 0),
            (SidecarPhase::NeedsConfig, stop.clone())
        );
        assert_eq!(
            step(&ready(true), restart.clone(), 0),
            (SidecarPhase::Idle, stop.clone())
        );
        assert_eq!(step(&ready(true), same.clone(), 0), (ready(true), vec![]));
        assert_eq!(
            step(&SidecarPhase::Off, same.clone(), 0),
            (SidecarPhase::Idle, vec![])
        );
        assert_eq!(
            step(&SidecarPhase::NeedsConfig, same.clone(), 0),
            (SidecarPhase::Idle, vec![])
        );
        let failed = SidecarPhase::Failed(SidecarFailure::PythonNotFound {
            tried: vec!["python3".into()],
        });
        assert_eq!(step(&failed, same, 0).0, failed);
        assert_eq!(step(&failed, restart, 0).0, SidecarPhase::Idle);
        assert_eq!(step(&failed, off, 0).0, SidecarPhase::Off);
    }

    #[test]
    fn step_ignores_stale_process_events() {
        assert_eq!(
            step(&SidecarPhase::Idle, SidecarEvent::Spawned { port: 1 }, 0),
            (SidecarPhase::Idle, vec![SidecarEffect::Kill])
        );
        for phase in [
            SidecarPhase::Off,
            SidecarPhase::Idle,
            SidecarPhase::NeedsConfig,
        ] {
            for event in [
                SidecarEvent::Healthy,
                SidecarEvent::DeepCheckPassed,
                SidecarEvent::DeepCheckFailed {
                    message: "x".into(),
                },
                SidecarEvent::Exited {
                    code: Some(0),
                    stderr_tail: String::new(),
                },
                SidecarEvent::Used,
                SidecarEvent::Tick,
                SidecarEvent::SpawnFailed(SidecarFailure::HealthTimeout { after_ms: 1 }),
            ] {
                assert_eq!(step(&phase, event, 0), (phase.clone(), vec![]));
            }
        }
        let not_spawned = SidecarPhase::Starting {
            port: 0,
            attempt: 1,
            since_ms: 0,
        };
        assert_eq!(step(&not_spawned, SidecarEvent::Healthy, 1).0, not_spawned);
        assert_eq!(
            step(&not_spawned, SidecarEvent::Tick, u64::MAX).0,
            not_spawned
        );
    }
}
