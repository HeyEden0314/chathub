//! Read models serialized to the TS types in `src/types/memory.ts`, and their
//! conversions from the model. Never carries an API key.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::model::*;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppliedRuntimeDto {
    pub(super) model: String,
    pub(super) endpoint_host: String,
    pub(super) embedding_model: String,
    pub(super) store_id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum SidecarFailureDto {
    PythonNotFound {
        tried: Vec<String>,
    },
    #[serde(rename_all = "camelCase")]
    ModuleMissing {
        python: String,
        install_hint: String,
    },
    #[serde(rename_all = "camelCase")]
    ExitedOnStart {
        code: Option<i32>,
        stderr_tail: String,
    },
    #[serde(rename_all = "camelCase")]
    HealthTimeout {
        after_ms: u64,
    },
    ModelCheckFailed {
        message: String,
    },
    #[serde(rename_all = "camelCase")]
    CrashLoop {
        restarts: u8,
        stderr_tail: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "phase", rename_all = "kebab-case")]
pub enum RuntimePhaseDto {
    Off,
    NeedsConfig {
        problems: Vec<ConfigProblem>,
    },
    Idle {
        applied: AppliedRuntimeDto,
    },
    Starting {
        applied: AppliedRuntimeDto,
    },
    Ready {
        applied: AppliedRuntimeDto,
    },
    #[serde(rename_all = "camelCase")]
    Backoff {
        applied: AppliedRuntimeDto,
        attempt: u8,
        retry_at: u64,
        last_exit: String,
    },
    Failed {
        applied: Option<AppliedRuntimeDto>,
        failure: SidecarFailureDto,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ProbeHealthDto {
    NotOpen,
    UnsupportedSite {
        host: String,
    },
    NoConversation,
    #[serde(rename_all = "camelCase")]
    UnrecognizedPage {
        conversation_key: String,
    },
    Ok {
        turns: usize,
        at: u64,
    },
    Error {
        message: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkipReason {
    MemoryOff,
    NeedsConfig,
    RuntimeFailed,
    NoTargets,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum IngestSummaryDto {
    Skipped {
        reason: SkipReason,
    },
    Coalesced,
    #[serde(rename_all = "camelCase")]
    Ran {
        at: u64,
        conversations_probed: u32,
        batches_stored: u32,
        batches_rejected: u32,
        batches_unknown: u32,
        turns_deferred: u32,
        memories_produced: u32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum IngestStateDto {
    Idle,
    Running,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestActivityDto {
    pub(super) state: IngestStateDto,
    pub(super) last_run: Option<IngestSummaryDto>,
    pub(super) abandoned_turns: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryStatusDto {
    pub(super) runtime: RuntimePhaseDto,
    pub(super) ingest: IngestActivityDto,
    pub(super) probes: BTreeMap<String, ProbeHealthDto>,
    pub(super) supported_hosts: Vec<&'static str>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ListResultDto {
    #[serde(rename_all = "camelCase")]
    Page {
        items: Vec<MemoryItem>,
        total: u32,
        next_offset: Option<u32>,
        dropped: u32,
    },
    Unavailable {
        runtime: RuntimePhaseDto,
    },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListQueryDto {
    #[serde(default)]
    pub(super) service_id: Option<String>,
    #[serde(default)]
    pub(super) offset: u32,
    #[serde(default = "default_list_limit")]
    pub(super) limit: u32,
}

fn default_list_limit() -> u32 {
    50
}

impl From<ListQueryDto> for ListQuery {
    fn from(q: ListQueryDto) -> Self {
        ListQuery {
            service_id: q
                .service_id
                .filter(|id| !id.trim().is_empty())
                .map(ServiceId),
            offset: q.offset,
            limit: q.limit,
        }
    }
}

fn applied_of(cfg: &RuntimeConfig) -> AppliedRuntimeDto {
    AppliedRuntimeDto {
        model: cfg.generation.model.clone(),
        endpoint_host: endpoint_host(cfg.generation.base_url.as_deref()),
        embedding_model: cfg.embedding.model.clone(),
        store_id: store_id(&cfg.embedding).0,
    }
}

impl From<&SidecarFailure> for SidecarFailureDto {
    fn from(failure: &SidecarFailure) -> Self {
        match failure.clone() {
            SidecarFailure::PythonNotFound { tried } => Self::PythonNotFound { tried },
            SidecarFailure::ModuleMissing { python } => Self::ModuleMissing {
                install_hint: install_hint(&python),
                python,
            },
            SidecarFailure::ExitedOnStart { code, stderr_tail } => {
                Self::ExitedOnStart { code, stderr_tail }
            }
            SidecarFailure::HealthTimeout { after_ms } => Self::HealthTimeout { after_ms },
            SidecarFailure::ModelCheckFailed { message } => Self::ModelCheckFailed { message },
            SidecarFailure::CrashLoop {
                restarts,
                stderr_tail,
            } => Self::CrashLoop {
                restarts,
                stderr_tail,
            },
        }
    }
}

/// `Ready` before the deep check passes is still `starting` to the user.
pub(super) fn runtime_dto(desired: &Desired, phase: &SidecarPhase) -> RuntimePhaseDto {
    let config = match desired {
        Desired::Off => return RuntimePhaseDto::Off,
        Desired::NeedsConfig(problems) => {
            return RuntimePhaseDto::NeedsConfig {
                problems: problems.clone(),
            }
        }
        Desired::On { config, .. } => config,
    };
    let applied = applied_of(config);
    match phase {
        SidecarPhase::Off | SidecarPhase::NeedsConfig | SidecarPhase::Idle => {
            RuntimePhaseDto::Idle { applied }
        }
        SidecarPhase::Starting { .. }
        | SidecarPhase::Ready {
            verified: false, ..
        } => RuntimePhaseDto::Starting { applied },
        SidecarPhase::Ready { verified: true, .. } => RuntimePhaseDto::Ready { applied },
        SidecarPhase::Backoff {
            attempt,
            until_ms,
            last_exit,
        } => RuntimePhaseDto::Backoff {
            applied,
            attempt: *attempt,
            retry_at: *until_ms,
            last_exit: last_exit.clone(),
        },
        SidecarPhase::Failed(failure) => RuntimePhaseDto::Failed {
            applied: Some(applied),
            failure: failure.into(),
        },
    }
}

impl From<&ProbeResult> for ProbeHealthDto {
    fn from(result: &ProbeResult) -> Self {
        match result {
            ProbeResult::NotOpen => Self::NotOpen,
            ProbeResult::UnsupportedSite { host } => Self::UnsupportedSite { host: host.clone() },
            ProbeResult::NoConversation => Self::NoConversation,
            ProbeResult::UnrecognizedPage { conversation } => Self::UnrecognizedPage {
                conversation_key: conversation.0.clone(),
            },
            ProbeResult::Page(s) => Self::Ok {
                turns: s.turns.len(),
                at: s.observed_at_ms,
            },
            ProbeResult::Failed { message } => Self::Error {
                message: message.clone(),
            },
        }
    }
}
