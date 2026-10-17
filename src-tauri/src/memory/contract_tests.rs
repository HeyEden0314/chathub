//! Renders one sample of every read-model variant, produced by the same code paths
//! the commands use, into `src/types/memory-contract.generated.ts`. `tsc` checks
//! that file with `satisfies MemoryContractFixture`, so a Rust/TS shape mismatch
//! fails the frontend build. Regenerate with
//! `UPDATE_MEMORY_CONTRACT=1 cargo test --lib memory::contract_tests`.

use std::fs;
use std::path::Path;

use serde::Serialize;

use super::model::test_support::{dialogue, snap};
use super::model::*;
use super::*;

const GENERATED: &str = "../src/types/memory-contract.generated.ts";
const AT: u64 = 1_792_231_500_000;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    statuses: Vec<MemoryStatusDto>,
    ingest_summaries: Vec<IngestSummaryDto>,
    list_results: Vec<ListResultDto>,
    config_fields: &'static [&'static str],
}

fn desired(settings: serde_json::Value, targets: &[&str]) -> Desired {
    let raw: RawMemorySettings = serde_json::from_value(settings).unwrap();
    let targets: Vec<String> = targets.iter().map(|t| t.to_string()).collect();
    desired_from(&raw, &targets)
}

fn configured(targets: &[&str]) -> Desired {
    desired(
        serde_json::json!({
            "enabled": true,
            "captureServiceIds": targets,
            "generation": { "baseUrl": "https://api.openai.com/v1", "apiKey": "sk-never-serialized", "model": "gpt-4.1-mini" },
            "embedding": { "source": { "kind": "reuse-generation" }, "model": "text-embedding-3-small", "dims": 1536 },
        }),
        targets,
    )
}

fn state(desired: Desired, sidecar: SidecarPhase) -> EngineState {
    let mut st = EngineState::new();
    st.desired = desired;
    st.sidecar = sidecar;
    st
}

fn statuses() -> Vec<MemoryStatusDto> {
    let mut out = vec![
        state(Desired::Off, SidecarPhase::Off).status(),
        state(
            desired(serde_json::json!({ "enabled": true }), &[]),
            SidecarPhase::NeedsConfig,
        )
        .status(),
        state(
            configured(&["chatgpt"]),
            SidecarPhase::Starting {
                port: 0,
                attempt: 1,
                since_ms: AT,
            },
        )
        .status(),
        state(
            configured(&["chatgpt"]),
            SidecarPhase::Ready {
                port: 41_000,
                last_used_ms: AT,
                verified: true,
            },
        )
        .status(),
        state(
            configured(&["chatgpt"]),
            SidecarPhase::Backoff {
                attempt: 2,
                until_ms: AT + 4_000,
                last_exit: "exit code 1".into(),
            },
        )
        .status(),
    ];

    let probed = ["chatgpt", "claude", "gemini", "deepseek", "grok", "poe"];
    let mut idle = state(configured(&probed), SidecarPhase::Idle);
    idle.ingest = IngestWorker::Running { rerun: false };
    idle.last_ingest = Some(
        RunTotals {
            conversations_probed: 2,
            batches_stored: 1,
            ..Default::default()
        }
        .summary(AT),
    );
    for (id, result) in probed.iter().zip([
        ProbeResult::Page(snap(dialogue(4), Some(false), AT)),
        ProbeResult::NotOpen,
        ProbeResult::NoConversation,
        ProbeResult::UnsupportedSite {
            host: "chat.deepseek.com".into(),
        },
        ProbeResult::UnrecognizedPage {
            conversation: ConversationKey("c-42".into()),
        },
        ProbeResult::Failed {
            message: "evaluate timed out".into(),
        },
    ]) {
        idle.probes.insert(ServiceId(id.to_string()), result);
    }
    out.push(idle.status());

    for failure in [
        SidecarFailure::PythonNotFound {
            tried: vec!["python3".into(), "python".into()],
        },
        SidecarFailure::ModuleMissing {
            python: "python3".into(),
        },
        SidecarFailure::ExitedOnStart {
            code: Some(1),
            stderr_tail: "Traceback: kuzu lock held".into(),
        },
        SidecarFailure::HealthTimeout { after_ms: 60_000 },
        SidecarFailure::ModelCheckFailed {
            message: "llm: error: 401 Unauthorized".into(),
        },
        SidecarFailure::CrashLoop {
            restarts: 3,
            stderr_tail: "Segmentation fault".into(),
        },
    ] {
        out.push(state(configured(&["chatgpt"]), SidecarPhase::Failed(failure)).status());
    }
    out
}

fn ingest_summaries() -> Vec<IngestSummaryDto> {
    let skipped = |desired: Desired| {
        state(desired, SidecarPhase::Off)
            .begin_ingest(IngestTrigger::Command)
            .unwrap_err()
    };
    let mut busy = state(configured(&["chatgpt"]), SidecarPhase::Idle);
    busy.ingest = IngestWorker::Running { rerun: false };
    vec![
        skipped(Desired::Off),
        skipped(desired(serde_json::json!({ "enabled": true }), &[])),
        skipped(configured(&[])),
        IngestSummaryDto::Skipped {
            reason: SkipReason::RuntimeFailed,
        },
        busy.begin_ingest(IngestTrigger::Command).unwrap_err(),
        RunTotals {
            conversations_probed: 2,
            batches_stored: 2,
            batches_rejected: 1,
            batches_unknown: 1,
            turns_deferred: 1,
            memories_produced: 3,
        }
        .summary(AT),
    ]
}

/// Row shape captured from hy-memory 2.0.3 `POST /api/v1/list`.
const LIST_BODY: &str = r#"{"vdb": {"memories": [
  {"memory_id": "88f3c566", "content": "用户住在杭州，喜欢喝乌龙茶。", "layer": "l2_fact", "status": "active",
   "memory_at": 1792231500, "gmt_created": 1792249675, "user_id": "anychat-local", "agent_id": "chatgpt",
   "session_id": "c-124", "speculate": null, "tags": [], "custom": {}, "meta": []},
  {"memory_id": "0b7e21aa", "content": "用户名叫小林。", "layer": "l0_basic_info", "gmt_created": 1792249600,
   "user_id": "anychat-local", "agent_id": "chatgpt", "session_id": "c-124"},
  {"memory_id": "", "content": "", "layer": "l2_fact"}
], "total": 120, "limit": 50, "offset": 0}}"#;

fn list_results() -> Vec<ListResultDto> {
    let query = ListQuery {
        service_id: None,
        offset: 0,
        limit: 50,
    };
    let page = parse_list_response(LIST_BODY, &query).unwrap();
    vec![
        ListResultDto::Page {
            items: page.items,
            total: page.total,
            next_offset: page.next_offset,
            dropped: page.dropped,
        },
        ListResultDto::Unavailable {
            runtime: runtime_dto(&Desired::Off, &SidecarPhase::Off),
        },
    ]
}

fn render() -> String {
    let fixture = Fixture {
        statuses: statuses(),
        ingest_summaries: ingest_summaries(),
        list_results: list_results(),
        config_fields: CONFIG_FIELDS,
    };
    let json = serde_json::to_string_pretty(&fixture).unwrap();
    format!(
        "// Generated by `UPDATE_MEMORY_CONTRACT=1 cargo test --lib memory::contract_tests` in src-tauri. Do not edit.\n\
         import type {{ MemoryContractFixture }} from './memory';\n\n\
         export const MEMORY_CONTRACT = {json} satisfies MemoryContractFixture;\n"
    )
}

#[test]
fn generated_ts_contract_is_current() {
    let rendered = render();
    assert!(
        !rendered.contains("sk-never-serialized"),
        "an API key reached a read model"
    );
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(GENERATED);
    if std::env::var_os("UPDATE_MEMORY_CONTRACT").is_some() {
        fs::write(&path, &rendered).unwrap();
        return;
    }
    let committed = fs::read_to_string(&path).unwrap_or_default();
    assert!(
        committed == rendered,
        "{GENERATED} is stale; run `UPDATE_MEMORY_CONTRACT=1 cargo test --lib memory::contract_tests` in src-tauri"
    );
}
