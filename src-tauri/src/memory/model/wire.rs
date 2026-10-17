use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::ingest::{Batch, ConvRef, ConversationKey, ObservedTurn, ProbeResult, Role, ServiceId, Snapshot, MAX_TURNS_PER_PROBE};

// ── Hy-Memory wire boundary ─────────────────────────────────────────────────

pub const ANYCHAT_USER_ID: &str = "anychat-local";

/// THE provider→scope mapping:
///   user_id = constant (AnyChat owns a private MEMORY_DATA_DIR, so no collision)
///   agent_id = service id (provenance and the server-side list filter)
///   session_id = conversation key (extraction history stays per conversation)
pub struct HyScope {
    pub user_id: &'static str,
    pub agent_id: String,
    pub session_id: String,
}

pub fn hy_scope(conv: &ConvRef) -> HyScope {
    HyScope {
        user_id: ANYCHAT_USER_ID,
        agent_id: conv.service.0.clone(),
        session_id: conv.conversation.0.clone(),
    }
}

pub fn add_request_body(batch: &Batch) -> String {
    let scope = hy_scope(&batch.conv);
    let messages: Vec<Value> = batch
        .turns
        .iter()
        .map(|t| json!({ "role": t.role.as_str(), "content": t.content }))
        .collect();
    let fps: Vec<String> = batch.fingerprints.iter().map(|fp| format!("{:016x}", fp.0)).collect();
    json!({
        "messages": messages,
        "user_id": scope.user_id,
        "agent_id": scope.agent_id,
        "session_id": scope.session_id,
        "memory_at": iso8601_utc(batch.observed_at_ms),
        "metadata": { "anychat_fps": fps },
    })
    .to_string()
}

/// `YYYY-MM-DDTHH:MM:SS+00:00`. Hy-Memory parses it with `datetime.fromisoformat`,
/// which rejects a trailing `Z` before Python 3.11.
pub fn iso8601_utc(epoch_ms: u64) -> String {
    let secs = epoch_ms / 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}+00:00",
        year,
        month,
        day,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's days-since-epoch to proleptic Gregorian date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// The three outcomes that matter for crash semantics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AddOutcome {
    /// 200 + `success: true`. Facts reconciled (possibly zero `memory_ids`).
    Stored { memory_ids: Vec<String> },
    /// 200 + `success: false` (Hy-Memory rolled back L1) or 4xx. Nothing committed.
    Rejected { reason: String },
    /// Timeout, connection reset, 5xx, or the sidecar exited mid-request.
    /// May have committed some facts; retrying makes delivery at-least-once.
    Unknown { reason: String },
}

pub fn parse_add_response(http_status: u16, body: &str) -> AddOutcome {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let field = |key: &str| -> Option<String> {
        parsed
            .as_ref()
            .and_then(|v| v.get(key))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    match http_status {
        200..=299 => match parsed.as_ref().and_then(|v| v.get("success")).and_then(Value::as_bool) {
            Some(true) => AddOutcome::Stored {
                memory_ids: parsed
                    .as_ref()
                    .and_then(|v| v.get("memory_ids"))
                    .and_then(Value::as_array)
                    .map(|ids| ids.iter().filter_map(|id| id.as_str().map(str::to_string)).collect())
                    .unwrap_or_default(),
            },
            Some(false) => AddOutcome::Rejected {
                reason: field("agent_error")
                    .or_else(|| field("error_message"))
                    .unwrap_or_else(|| "success=false".to_string()),
            },
            None => AddOutcome::Unknown {
                reason: format!("HTTP {http_status} without a success flag"),
            },
        },
        400..=499 => AddOutcome::Rejected {
            reason: field("error").unwrap_or_else(|| format!("HTTP {http_status}")),
        },
        _ => AddOutcome::Unknown {
            reason: field("error").unwrap_or_else(|| format!("HTTP {http_status}")),
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryKind {
    Profile,
    Fact,
    Intention,
    Schema,
    Summary,
    Other,
}

fn memory_kind(layer: &str) -> MemoryKind {
    match layer {
        "l0_basic_info" => MemoryKind::Profile,
        "l2_fact" => MemoryKind::Fact,
        "l7_intention" => MemoryKind::Intention,
        "l6_schema" => MemoryKind::Schema,
        l if l.starts_with("l3_") => MemoryKind::Summary,
        _ => MemoryKind::Other,
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryItem {
    pub id: String,
    pub text: String,
    pub kind: MemoryKind,
    pub service_id: Option<String>,
    pub conversation_key: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MemoryPage {
    pub items: Vec<MemoryItem>,
    pub total: u32,
    pub next_offset: Option<u32>,
    /// Rows without id/content or with unknown shape; counted, not shown.
    pub dropped: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ListQuery {
    pub service_id: Option<ServiceId>,
    pub offset: u32,
    /// Clamped to 1..=100.
    pub limit: u32,
}

impl ListQuery {
    fn clamped_limit(&self) -> u32 {
        self.limit.clamp(1, 100)
    }
}

pub fn list_request_body(q: &ListQuery) -> String {
    let mut body = json!({
        "user_id": ANYCHAT_USER_ID,
        "limit": q.clamped_limit(),
        "offset": q.offset,
        "order": "desc",
    });
    if let Some(service) = &q.service_id {
        body["agent_id"] = json!(service.0);
    }
    body.to_string()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WireError(pub String);

/// Reads `vdb.memories[]` and `vdb.total`; ignores `graph`. L0 rows are user-level,
/// so they get `service_id: None`.
pub fn parse_list_response(body: &str, q: &ListQuery) -> Result<MemoryPage, WireError> {
    let root: Value =
        serde_json::from_str(body).map_err(|e| WireError(format!("invalid list JSON: {e}")))?;
    let Some(vdb) = root.get("vdb") else {
        let reason = root.get("error").and_then(Value::as_str).unwrap_or("missing vdb bucket");
        return Err(WireError(reason.to_string()));
    };
    let rows = vdb
        .get("memories")
        .and_then(Value::as_array)
        .ok_or_else(|| WireError("missing vdb.memories".to_string()))?;

    let text_of = |row: &Value, key: &str| -> Option<String> {
        row.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let mut items = Vec::with_capacity(rows.len());
    let mut dropped = 0u32;
    for row in rows {
        let (Some(id), Some(text)) = (text_of(row, "memory_id"), text_of(row, "content")) else {
            dropped += 1;
            continue;
        };
        let kind = memory_kind(row.get("layer").and_then(Value::as_str).unwrap_or(""));
        items.push(MemoryItem {
            id,
            text,
            kind,
            service_id: if kind == MemoryKind::Profile { None } else { text_of(row, "agent_id") },
            conversation_key: text_of(row, "session_id"),
            created_at_ms: row
                .get("gmt_created")
                .and_then(Value::as_f64)
                .filter(|s| *s >= 0.0)
                .map(|s| (s * 1000.0) as u64),
        });
    }

    let consumed = q.offset.saturating_add(rows.len() as u32);
    let total = vdb
        .get("total")
        .and_then(Value::as_u64)
        .map_or(consumed, |t| u32::try_from(t).unwrap_or(u32::MAX));
    Ok(MemoryPage {
        items,
        total,
        next_offset: (!rows.is_empty() && consumed < total).then_some(consumed),
        dropped,
    })
}

#[derive(Deserialize)]
struct ProbeTurnPayload {
    role: Option<String>,
    text: Option<String>,
}

#[derive(Deserialize)]
struct ProbePayload {
    ok: bool,
    error: Option<String>,
    conversation: Option<String>,
    context: Option<ProbeTurnPayload>,
    #[serde(default)]
    turns: Vec<ProbeTurnPayload>,
    streaming: Option<bool>,
}

fn observed_turn(raw: &ProbeTurnPayload) -> Option<ObservedTurn> {
    let role = match raw.role.as_deref() {
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        _ => return None,
    };
    let text = raw.text.as_deref()?.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then_some(ObservedTurn { role, text })
}

/// Probe JSON contract: `{ok:true, host, conversation: string|null, context: Turn|null,
/// turns: Turn[], streaming: bool|null}` or `{ok:false, error}`.
pub fn parse_probe_payload(service: &ServiceId, json: &str, observed_at_ms: u64) -> ProbeResult {
    let payload: ProbePayload = match serde_json::from_str(json) {
        Ok(p) => p,
        Err(e) => {
            return ProbeResult::Failed {
                message: format!("invalid probe payload: {e}"),
            }
        }
    };
    if !payload.ok {
        return ProbeResult::Failed {
            message: payload.error.unwrap_or_else(|| "probe failed".to_string()),
        };
    }
    let Some(conversation) = payload.conversation.filter(|c| !c.trim().is_empty()) else {
        return ProbeResult::NoConversation;
    };
    let conversation = ConversationKey(conversation);

    let mut context_turn = payload.context.as_ref().and_then(observed_turn);
    let mut turns: Vec<ObservedTurn> = payload.turns.iter().filter_map(observed_turn).collect();
    if turns.is_empty() {
        return ProbeResult::UnrecognizedPage { conversation };
    }
    if turns.len() > MAX_TURNS_PER_PROBE {
        let excess = turns.len() - MAX_TURNS_PER_PROBE;
        context_turn = Some(turns[excess - 1].clone());
        turns.drain(..excess);
    }
    ProbeResult::Page(Snapshot {
        conv: ConvRef {
            service: service.clone(),
            conversation,
        },
        context_turn,
        turns,
        streaming: payload.streaming,
        observed_at_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::model::test_support::*;

    #[test]
    fn add_request_body_carries_scope_and_fingerprints() {
        let s = snap(dialogue(2), Some(false), 1_760_710_560_000);
        let batch = plan(Some(&empty_ledger()), None, &s).batches.remove(0);
        let body: Value = serde_json::from_str(&add_request_body(&batch)).unwrap();
        assert_eq!(body["user_id"], ANYCHAT_USER_ID);
        assert_eq!(body["agent_id"], "chatgpt");
        assert_eq!(body["session_id"], "c1");
        assert_eq!(body["memory_at"], "2025-10-17T14:16:00+00:00");
        assert_eq!(body["messages"][0], json!({"role": "user", "content": "u0"}));
        assert_eq!(body["messages"][1]["role"], "assistant");
        assert_eq!(body["metadata"]["anychat_fps"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn iso8601_handles_epoch_and_leap_day() {
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00+00:00");
        assert_eq!(iso8601_utc(951_782_400_000), "2000-02-29T00:00:00+00:00");
        assert_eq!(iso8601_utc(1_709_251_199_999), "2024-02-29T23:59:59+00:00");
    }

    #[test]
    fn parse_add_response_covers_three_outcomes() {
        assert_eq!(
            parse_add_response(200, r#"{"success":true,"memory_id":"raw","memory_ids":["f1","f2"]}"#),
            AddOutcome::Stored { memory_ids: vec!["f1".into(), "f2".into()] }
        );
        assert_eq!(
            parse_add_response(200, r#"{"success":true,"memory_id":"raw"}"#),
            AddOutcome::Stored { memory_ids: vec![] }
        );
        assert_eq!(
            parse_add_response(200, r#"{"success":false,"agent_error":"extract failed","error_message":null}"#),
            AddOutcome::Rejected { reason: "extract failed".into() }
        );
        assert_eq!(
            parse_add_response(400, r#"{"error":"data, text, or messages is required"}"#),
            AddOutcome::Rejected { reason: "data, text, or messages is required".into() }
        );
        assert!(matches!(parse_add_response(500, r#"{"error":"boom"}"#), AddOutcome::Unknown { .. }));
        assert!(matches!(parse_add_response(200, "not json"), AddOutcome::Unknown { .. }));
        assert!(matches!(parse_add_response(502, ""), AddOutcome::Unknown { .. }));
    }

    #[test]
    fn list_request_body_clamps_limit_and_filters_by_service() {
        let q = ListQuery { service_id: Some(ServiceId("claude".into())), offset: 20, limit: 500 };
        let body: Value = serde_json::from_str(&list_request_body(&q)).unwrap();
        assert_eq!(body["limit"], 100);
        assert_eq!(body["offset"], 20);
        assert_eq!(body["agent_id"], "claude");
        assert_eq!(body["order"], "desc");
        let all: Value =
            serde_json::from_str(&list_request_body(&ListQuery { service_id: None, offset: 0, limit: 0 })).unwrap();
        assert!(all.get("agent_id").is_none());
        assert_eq!(all["limit"], 1);
    }

    #[test]
    fn parse_list_response_is_tolerant() {
        let body = r#"{
          "vdb": {
            "memories": [
              {"memory_id":"m1","content":"用户偏好用 Rust 写 CLI 工具","layer":"l2_fact","agent_id":"chatgpt","session_id":"c1","gmt_created":1760710560},
              {"memory_id":"m2","content":"name: Ada","layer":"l0_basic_info","agent_id":"chatgpt","gmt_created":null},
              {"memory_id":"m3","content":"wants to learn Zig","layer":"l7_intention","agent_id":"claude","session_id":""},
              {"memory_id":"m4","content":"summary","layer":"l3_summary"},
              {"memory_id":"m5","content":"x","layer":"l9_future"},
              {"memory_id":"","content":"no id"},
              {"memory_id":"m6"},
              "garbage"
            ],
            "total": 30, "limit": 8, "offset": 0
          },
          "graph": {"nodes": []}
        }"#;
        let q = ListQuery { service_id: None, offset: 0, limit: 8 };
        let page = parse_list_response(body, &q).unwrap();
        assert_eq!(page.dropped, 3);
        assert_eq!(page.total, 30);
        assert_eq!(page.next_offset, Some(8));
        let kinds: Vec<MemoryKind> = page.items.iter().map(|i| i.kind).collect();
        assert_eq!(
            kinds,
            vec![MemoryKind::Fact, MemoryKind::Profile, MemoryKind::Intention, MemoryKind::Summary, MemoryKind::Other]
        );
        assert_eq!(page.items[0].created_at_ms, Some(1_760_710_560_000));
        assert_eq!(page.items[0].service_id.as_deref(), Some("chatgpt"));
        assert_eq!(page.items[0].conversation_key.as_deref(), Some("c1"));
        assert_eq!(page.items[1].service_id, None);
        assert_eq!(page.items[1].created_at_ms, None);
        assert_eq!(page.items[2].conversation_key, None);
    }

    #[test]
    fn parse_list_response_ends_pagination_and_reports_errors() {
        let q = ListQuery { service_id: None, offset: 28, limit: 8 };
        let page = parse_list_response(r#"{"vdb":{"memories":[{"memory_id":"a","content":"b","layer":"l2_fact"}],"total":29}}"#, &q).unwrap();
        assert_eq!(page.next_offset, None);
        assert!(parse_list_response(r#"{"error":"boom"}"#, &q).is_err());
        assert!(parse_list_response("nope", &q).is_err());
    }

    #[test]
    fn memory_item_serializes_to_the_ts_shape() {
        let item = MemoryItem {
            id: "m1".into(),
            text: "t".into(),
            kind: MemoryKind::Fact,
            service_id: Some("chatgpt".into()),
            conversation_key: None,
            created_at_ms: Some(5),
        };
        assert_eq!(
            serde_json::to_value(&item).unwrap(),
            json!({"id":"m1","text":"t","kind":"fact","serviceId":"chatgpt","conversationKey":null,"createdAt":5})
        );
    }

    fn probe(json: &str) -> ProbeResult {
        parse_probe_payload(&ServiceId("chatgpt".into()), json, 9)
    }

    #[test]
    fn parse_probe_payload_maps_every_case() {
        assert!(matches!(probe("{"), ProbeResult::Failed { .. }));
        assert_eq!(
            probe(r#"{"ok":false,"error":"TypeError: x"}"#),
            ProbeResult::Failed { message: "TypeError: x".into() }
        );
        assert_eq!(
            probe(r#"{"ok":true,"host":"chatgpt.com","conversation":null,"context":null,"turns":[],"streaming":null}"#),
            ProbeResult::NoConversation
        );
        assert_eq!(
            probe(r#"{"ok":true,"conversation":"abc","context":null,"turns":[{"role":"system","text":"x"},{"role":"user","text":"   "}],"streaming":null}"#),
            ProbeResult::UnrecognizedPage { conversation: ConversationKey("abc".into()) }
        );
        let ProbeResult::Page(s) = probe(
            r#"{"ok":true,"conversation":"abc","context":{"role":"assistant","text":"prev"},
               "turns":[{"role":"user","text":"  hello \n\n  world "},{"role":"assistant","text":"hi"}],"streaming":false}"#,
        ) else {
            panic!("expected a page");
        };
        assert_eq!(s.conv, conv("chatgpt", "abc"));
        assert_eq!(s.context_turn, Some(assistant("prev")));
        assert_eq!(s.turns, vec![user("hello world"), assistant("hi")]);
        assert_eq!(s.streaming, Some(false));
        assert_eq!(s.observed_at_ms, 9);
    }

    #[test]
    fn parse_probe_payload_trims_oversized_windows_into_context() {
        let turns: Vec<Value> = dialogue(MAX_TURNS_PER_PROBE + 2)
            .iter()
            .map(|t| json!({"role": t.role.as_str(), "text": t.text}))
            .collect();
        let payload = json!({"ok": true, "conversation": "abc", "context": null, "turns": turns, "streaming": null});
        let ProbeResult::Page(s) = probe(&payload.to_string()) else {
            panic!("expected a page");
        };
        assert_eq!(s.turns.len(), MAX_TURNS_PER_PROBE);
        assert_eq!(s.context_turn, Some(assistant("a1")));
        assert_eq!(s.turns[0], user("u2"));
    }
}
