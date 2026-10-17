//! Imperative shell for memory. Owns all runtime state, runs the effects that
//! `model::step` asks for, talks HTTP to the sidecar, and persists the ledger.

#[cfg(test)]
mod contract_tests;
mod dto;
#[cfg(all(test, unix))]
mod live_tests;
mod model;
mod probe;
mod sidecar;

use reqwest::header::CONTENT_TYPE;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::{Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::Manager;
use tokio::sync::Notify;

use dto::*;
use model::*;
use sidecar::SidecarProcess;

/// Idle stop, backoff, and crash detection run on this cadence; ingest every `SWEEP_INTERVAL`.
const TICK_INTERVAL: Duration = Duration::from_secs(5);
const HEALTH_POLL: Duration = Duration::from_millis(250);
const HEALTHZ_TIMEOUT: Duration = Duration::from_secs(2);
const DEEP_CHECK_TIMEOUT: Duration = Duration::from_secs(90);
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
const WAITER_RECHECK: Duration = Duration::from_millis(500);
const LEDGER_FILE: &str = "ledger.json";
const PID_FILE: &str = "sidecar.pid";

// ── engine ──────────────────────────────────────────────────────────────────

pub enum IngestTrigger {
    /// The 记忆 tab or 立即同步. A call during a run asks it to loop once more.
    Command,
    /// The background sweep. The next sweep is never far off, so it never asks for a rerun.
    Sweep,
}

/// Single-flight. Probes and adds never overlap and at most one add is in flight.
enum IngestWorker {
    Idle,
    Running { rerun: bool },
}

struct PassContext {
    fp: RuntimeFingerprint,
    store: StoreId,
    gen: GenerationIdentity,
    targets: Vec<ServiceId>,
}

#[derive(Default)]
struct RunTotals {
    conversations_probed: u32,
    batches_stored: u32,
    batches_rejected: u32,
    batches_unknown: u32,
    turns_deferred: u32,
    memories_produced: u32,
}

impl RunTotals {
    fn summary(&self, at: u64) -> IngestSummaryDto {
        IngestSummaryDto::Ran {
            at,
            conversations_probed: self.conversations_probed,
            batches_stored: self.batches_stored,
            batches_rejected: self.batches_rejected,
            batches_unknown: self.batches_unknown,
            turns_deferred: self.turns_deferred,
            memories_produced: self.memories_produced,
        }
    }
}

struct EngineState {
    desired: Desired,
    runtime_fp: Option<RuntimeFingerprint>,
    sidecar: SidecarPhase,
    process: Option<SidecarProcess>,
    /// Bumped per spawn; a spawn that finishes after a newer one started is discarded.
    spawn_seq: u64,
    /// The next spawn waits for this so two servers never hold the same store's lock.
    pending_kill: Option<JoinHandle<()>>,
    /// The deep check already passed for this fingerprint in this session.
    verified_fp: Option<RuntimeFingerprint>,
    ingest: IngestWorker,
    /// Settle-rule memory. Runtime only.
    tails: HashMap<ConvRef, TailObservation>,
    /// Per process lifetime, keyed by a batch's first fingerprint. Persisted only
    /// once it crosses `MAX_REJECTIONS`, as an abandon mark.
    rejections: HashMap<Fingerprint, u8>,
    probes: HashMap<ServiceId, ProbeResult>,
    /// Loaded lazily for the active store; written back after every change.
    ledger: Option<(StoreId, Ledger)>,
    last_ingest: Option<IngestSummaryDto>,
}

impl EngineState {
    fn new() -> Self {
        Self {
            desired: Desired::Off,
            runtime_fp: None,
            sidecar: SidecarPhase::Off,
            process: None,
            spawn_seq: 0,
            pending_kill: None,
            verified_fp: None,
            ingest: IngestWorker::Idle,
            tails: HashMap::new(),
            rejections: HashMap::new(),
            probes: HashMap::new(),
            ledger: None,
            last_ingest: None,
        }
    }

    fn begin_ingest(&mut self, trigger: IngestTrigger) -> Result<(), IngestSummaryDto> {
        let skipped = |reason| Err(IngestSummaryDto::Skipped { reason });
        match &self.desired {
            Desired::Off => return skipped(SkipReason::MemoryOff),
            Desired::NeedsConfig(_) => return skipped(SkipReason::NeedsConfig),
            Desired::On { targets, .. } if targets.is_empty() => {
                return skipped(SkipReason::NoTargets)
            }
            Desired::On { .. } => {}
        }
        match (&mut self.ingest, trigger) {
            (IngestWorker::Running { rerun }, IngestTrigger::Command) => {
                *rerun = true;
                Err(IngestSummaryDto::Coalesced)
            }
            (IngestWorker::Running { .. }, IngestTrigger::Sweep) => {
                Err(IngestSummaryDto::Coalesced)
            }
            (IngestWorker::Idle, _) => {
                self.ingest = IngestWorker::Running { rerun: false };
                Ok(())
            }
        }
    }

    fn take_rerun(&mut self) -> bool {
        match &mut self.ingest {
            IngestWorker::Running { rerun } => std::mem::take(rerun),
            IngestWorker::Idle => false,
        }
    }

    fn pass_context(&self) -> Option<PassContext> {
        match &self.desired {
            Desired::On { config, targets } if !targets.is_empty() => Some(PassContext {
                fp: runtime_fingerprint(config),
                store: store_id(&config.embedding),
                gen: generation_identity(config),
                targets: targets.clone(),
            }),
            _ => None,
        }
    }

    fn ledger_for(&mut self, store: &StoreId) -> Option<&mut Ledger> {
        self.ledger
            .as_mut()
            .filter(|(id, _)| id == store)
            .map(|(_, ledger)| ledger)
    }

    fn status(&self) -> MemoryStatusDto {
        let (targets, gen): (&[ServiceId], _) = match &self.desired {
            Desired::On { config, targets } => (targets, Some(generation_identity(config))),
            _ => (&[], None),
        };
        let store = self.pass_context().map(|ctx| ctx.store);
        let abandoned_turns = match (&self.ledger, gen) {
            (Some((id, ledger)), Some(gen)) if Some(id) == store.as_ref() => ledger
                .conversations
                .values()
                .map(|conv| conv.abandoned.iter().filter(|(_, g)| *g == gen).count() as u32)
                .sum(),
            _ => 0,
        };
        MemoryStatusDto {
            runtime: runtime_dto(&self.desired, &self.sidecar),
            ingest: IngestActivityDto {
                state: match self.ingest {
                    IngestWorker::Idle => IngestStateDto::Idle,
                    IngestWorker::Running { .. } => IngestStateDto::Running,
                },
                last_run: self.last_ingest.clone(),
                abandoned_turns,
            },
            probes: targets
                .iter()
                .filter_map(|id| self.probes.get(id).map(|r| (id.0.clone(), r.into())))
                .collect(),
            supported_hosts: probe::supported_hosts(),
        }
    }
}

/// Tauri-managed. One mutex with short critical sections; no lock is held across
/// `.await`, a probe, file IO, or a process wait.
pub struct MemoryEngine {
    state: Mutex<EngineState>,
    waiters: Notify,
    http: reqwest::Client,
}

impl MemoryEngine {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(EngineState::new()),
            waiters: Notify::new(),
            // Loopback only; a user's HTTP(S)_PROXY must not capture sidecar traffic.
            http: reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap_or_default(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, EngineState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn status(&self) -> MemoryStatusDto {
        self.lock().status()
    }

    fn sync(&self, app: &tauri::AppHandle, raw: &RawMemorySettings, capture_targets: &[String]) {
        let desired = desired_from(raw, capture_targets);
        let event = {
            let mut st = self.lock();
            let (on, valid) = match &desired {
                Desired::Off => (false, false),
                Desired::NeedsConfig(_) => (true, false),
                Desired::On { .. } => (true, true),
            };
            let mut restart = false;
            if let Desired::On { config, targets } = &desired {
                let fp = runtime_fingerprint(config);
                restart = st.runtime_fp.is_some_and(|old| old != fp);
                st.runtime_fp = Some(fp);
                let store = store_id(&config.embedding);
                if st.ledger.as_ref().is_some_and(|(id, _)| *id != store) {
                    st.ledger = None;
                }
                st.probes.retain(|id, _| targets.contains(id));
            }
            st.desired = desired;
            SidecarEvent::Desired { on, valid, restart }
        };
        self.feed(app, event);
    }

    /// THE ingest. The sweep calls it too.
    pub async fn ingest(&self, app: &tauri::AppHandle, trigger: IngestTrigger) -> IngestSummaryDto {
        let records_skips = matches!(trigger, IngestTrigger::Command);
        if let Err(summary) = self.lock().begin_ingest(trigger) {
            if records_skips && matches!(summary, IngestSummaryDto::Skipped { .. }) {
                self.lock().last_ingest = Some(summary.clone());
            }
            return summary;
        }
        let summary = self.run_passes(app).await;
        let mut st = self.lock();
        st.ingest = IngestWorker::Idle;
        st.last_ingest = Some(summary.clone());
        summary
    }

    async fn run_passes(&self, app: &tauri::AppHandle) -> IngestSummaryDto {
        let mut totals = RunTotals::default();
        loop {
            let Some(ctx) = self.lock().pass_context() else {
                break;
            };
            self.ensure_ledger(app, &ctx.store);

            let mut probes = Vec::with_capacity(ctx.targets.len());
            for target in &ctx.targets {
                probes.push((
                    target.clone(),
                    probe::probe_service(app, target, now_ms()).await,
                ));
            }
            let (batches, baseline_acked) = self.plan(&ctx, probes, &mut totals);
            if baseline_acked {
                self.flush_ledger(app, &ctx.store);
            }

            if !batches.is_empty() {
                let Ok(port) = self.ensure_runtime(app).await else {
                    return IngestSummaryDto::Skipped {
                        reason: SkipReason::RuntimeFailed,
                    };
                };
                self.send(app, &ctx, port, batches, &mut totals).await;
            }
            if !self.lock().take_rerun() {
                break;
            }
        }
        totals.summary(now_ms())
    }

    /// Records probe health and tails, applies first-sight baseline acks, and
    /// returns the batches to send.
    fn plan(
        &self,
        ctx: &PassContext,
        probes: Vec<(ServiceId, ProbeResult)>,
        totals: &mut RunTotals,
    ) -> (Vec<Batch>, bool) {
        let mut st = self.lock();
        let snaps: Vec<Snapshot> = probes
            .iter()
            .filter_map(|(_, r)| match r {
                ProbeResult::Page(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        st.probes.extend(probes);
        totals.conversations_probed = snaps.len() as u32;
        totals.turns_deferred = 0;

        let EngineState { ledger, tails, .. } = &mut *st;
        let Some(ledger) = ledger
            .as_mut()
            .filter(|(id, _)| *id == ctx.store)
            .map(|(_, l)| l)
        else {
            return (Vec::new(), false);
        };
        let now = now_ms();
        let mut batches = Vec::new();
        let mut baseline_acked = false;
        for snap in &snaps {
            let plan = plan_conversation(
                ledger.get(&snap.conv),
                tails.get(&snap.conv),
                snap,
                ctx.gen,
                &DEFAULT_LIMITS,
            );
            if let Some(tail) = plan.next_tail {
                tails.insert(snap.conv.clone(), tail);
            }
            if !plan.baseline.is_empty() {
                apply_change(ledger, &snap.conv, &LedgerChange::Ack(plan.baseline), now);
                baseline_acked = true;
            }
            totals.turns_deferred += plan.deferred_turns as u32;
            batches.extend(plan.batches);
        }
        (batches, baseline_acked)
    }

    async fn send(
        &self,
        app: &tauri::AppHandle,
        ctx: &PassContext,
        port: u16,
        batches: Vec<Batch>,
        totals: &mut RunTotals,
    ) {
        for batch in batches {
            if self.lock().runtime_fp != Some(ctx.fp) {
                return;
            }
            let outcome = self.post_add(port, &batch).await;
            self.feed(app, SidecarEvent::Used);
            match &outcome {
                AddOutcome::Stored { memory_ids } => {
                    totals.batches_stored += 1;
                    totals.memories_produced += memory_ids.len() as u32;
                }
                AddOutcome::Rejected { .. } => totals.batches_rejected += 1,
                AddOutcome::Unknown { .. } => totals.batches_unknown += 1,
            }

            let changed = {
                let mut st = self.lock();
                let key = batch.fingerprints[0];
                let prior = st.rejections.get(&key).copied().unwrap_or(0);
                let change = after_add(&outcome, prior, ctx.gen, &batch);
                match (&outcome, &change) {
                    (AddOutcome::Rejected { .. }, LedgerChange::RetryLater) => {
                        st.rejections.insert(key, prior + 1);
                    }
                    (_, LedgerChange::Ack(_) | LedgerChange::Abandon(..)) => {
                        st.rejections.remove(&key);
                    }
                    _ => {}
                }
                match st.ledger_for(&ctx.store) {
                    Some(ledger) if change != LedgerChange::RetryLater => {
                        apply_change(ledger, &batch.conv, &change, now_ms());
                        true
                    }
                    _ => false,
                }
            };
            if changed {
                self.flush_ledger(app, &ctx.store);
            }
            if matches!(outcome, AddOutcome::Unknown { .. }) {
                self.reap_exited(app);
                if !matches!(self.lock().sidecar, SidecarPhase::Ready { .. }) {
                    return;
                }
            }
        }
    }

    async fn post_add(&self, port: u16, batch: &Batch) -> AddOutcome {
        let response = self
            .http
            .post(sidecar_url(port, "/api/v1/add"))
            .header(CONTENT_TYPE, "application/json")
            .body(add_request_body(batch))
            .timeout(ADD_TIMEOUT)
            .send()
            .await;
        match response {
            Ok(response) => {
                let status = response.status().as_u16();
                match response.text().await {
                    Ok(body) => parse_add_response(status, &body),
                    Err(e) => AddOutcome::Unknown {
                        reason: e.to_string(),
                    },
                }
            }
            Err(e) => AddOutcome::Unknown {
                reason: e.to_string(),
            },
        }
    }

    /// THE list. Starts the sidecar on demand; nothing is cached on either side.
    pub async fn list(
        &self,
        app: &tauri::AppHandle,
        query: ListQuery,
    ) -> Result<ListResultDto, String> {
        if !matches!(self.lock().desired, Desired::On { .. }) {
            return Ok(ListResultDto::Unavailable {
                runtime: self.status().runtime,
            });
        }
        let Ok(port) = self.ensure_runtime(app).await else {
            return Ok(ListResultDto::Unavailable {
                runtime: self.status().runtime,
            });
        };
        let response = self
            .http
            .post(sidecar_url(port, "/api/v1/list"))
            .header(CONTENT_TYPE, "application/json")
            .body(list_request_body(&query))
            .timeout(LIST_TIMEOUT)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        self.feed(app, SidecarEvent::Used);
        let body = response.text().await.map_err(|e| e.to_string())?;
        let page = parse_list_response(&body, &query).map_err(|e| e.0)?;
        Ok(ListResultDto::Page {
            items: page.items,
            total: page.total,
            next_offset: page.next_offset,
            dropped: page.dropped,
        })
    }

    /// Drives `step` until the sidecar is verified ready or reaches a phase that
    /// waiting cannot change.
    async fn ensure_runtime(&self, app: &tauri::AppHandle) -> Result<u16, SidecarPhase> {
        self.feed(app, SidecarEvent::Demand);
        loop {
            let mut notified = pin!(self.waiters.notified());
            notified.as_mut().enable();
            let phase = self.lock().sidecar.clone();
            match phase {
                SidecarPhase::Ready {
                    port,
                    verified: true,
                    ..
                } => return Ok(port),
                SidecarPhase::Starting { .. }
                | SidecarPhase::Ready {
                    verified: false, ..
                } => {}
                other => return Err(other),
            }
            let _ = tokio::time::timeout(WAITER_RECHECK, notified).await;
            self.reap_exited(app);
            self.feed(app, SidecarEvent::Tick);
        }
    }

    fn feed(&self, app: &tauri::AppHandle, event: SidecarEvent) {
        self.feed_if(app, event, |_| true);
    }

    /// `guard` runs under the same lock as the transition, so a late result from
    /// a previous process or configuration is dropped instead of applied.
    fn feed_if(
        &self,
        app: &tauri::AppHandle,
        event: SidecarEvent,
        guard: impl FnOnce(&mut EngineState) -> bool,
    ) {
        let effects = {
            let mut st = self.lock();
            if !guard(&mut st) {
                return;
            }
            let (next, effects) = step(&st.sidecar, event, now_ms());
            st.sidecar = next;
            effects
        };
        self.execute(app, effects);
    }

    fn execute(&self, app: &tauri::AppHandle, effects: Vec<SidecarEffect>) {
        for effect in effects {
            match effect {
                SidecarEffect::Spawn { .. } => self.start_spawn(app),
                SidecarEffect::PollHealth => self.start_health_poll(app),
                SidecarEffect::DeepCheck => self.start_deep_check(app),
                SidecarEffect::Kill => self.kill(),
                SidecarEffect::NotifyWaiters => self.waiters.notify_waiters(),
            }
        }
    }

    fn start_spawn(&self, app: &tauri::AppHandle) {
        let (config, seq, pending_kill) = {
            let mut st = self.lock();
            let Desired::On { config, .. } = &st.desired else {
                return;
            };
            let config = config.clone();
            st.spawn_seq += 1;
            (config, st.spawn_seq, st.pending_kill.take())
        };
        let Some(root) = memory_root(app) else {
            let failure = SidecarFailure::ExitedOnStart {
                code: None,
                stderr_tail: "app data directory unavailable".to_string(),
            };
            self.feed(app, SidecarEvent::SpawnFailed(failure));
            return;
        };
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            let store_dir = root.join(&store_id(&config.embedding).0);
            let pid_file = root.join(PID_FILE);
            let spawned = tauri::async_runtime::spawn_blocking(move || {
                if let Some(kill) = pending_kill {
                    let _ = kill.join();
                }
                sidecar::spawn(&config, &store_dir, &pid_file)
            })
            .await
            .unwrap_or_else(|e| {
                Err(SidecarFailure::ExitedOnStart {
                    code: None,
                    stderr_tail: e.to_string(),
                })
            });
            app.state::<MemoryEngine>().finish_spawn(&app, seq, spawned);
        });
    }

    fn finish_spawn(
        &self,
        app: &tauri::AppHandle,
        seq: u64,
        spawned: Result<(SidecarProcess, u16), SidecarFailure>,
    ) {
        let event = {
            let mut st = self.lock();
            if st.spawn_seq != seq {
                if let Ok((stale, _)) = spawned {
                    std::thread::spawn(move || stale.terminate());
                }
                return;
            }
            match spawned {
                Ok((process, port)) => {
                    st.process = Some(process);
                    SidecarEvent::Spawned { port }
                }
                Err(failure) => SidecarEvent::SpawnFailed(failure),
            }
        };
        self.feed(app, event);
    }

    fn starting_port(&self) -> Option<u16> {
        match self.lock().sidecar {
            SidecarPhase::Starting { port, .. } if port != 0 => Some(port),
            _ => None,
        }
    }

    fn start_health_poll(&self, app: &tauri::AppHandle) {
        let Some(port) = self.starting_port() else {
            return;
        };
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            let engine = app.state::<MemoryEngine>();
            while engine.starting_port() == Some(port) {
                let healthy = engine
                    .http
                    .get(sidecar_url(port, "/healthz"))
                    .timeout(HEALTHZ_TIMEOUT)
                    .send()
                    .await
                    .is_ok_and(|r| r.status().is_success());
                if healthy {
                    engine.feed_if(&app, SidecarEvent::Healthy, |st| {
                        matches!(st.sidecar, SidecarPhase::Starting { port: p, .. } if p == port)
                    });
                    return;
                }
                engine.reap_exited(&app);
                engine.feed(&app, SidecarEvent::Tick);
                tokio::time::sleep(HEALTH_POLL).await;
            }
        });
    }

    fn start_deep_check(&self, app: &tauri::AppHandle) {
        let (port, fp, already_verified) = {
            let st = self.lock();
            let (
                SidecarPhase::Ready {
                    port,
                    verified: false,
                    ..
                },
                Some(fp),
            ) = (&st.sidecar, st.runtime_fp)
            else {
                return;
            };
            (*port, fp, st.verified_fp == Some(fp))
        };
        if already_verified {
            self.feed(app, SidecarEvent::DeepCheckPassed);
            return;
        }
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            let engine = app.state::<MemoryEngine>();
            let result = match engine
                .http
                .get(sidecar_url(port, "/api/v1/status"))
                .timeout(DEEP_CHECK_TIMEOUT)
                .send()
                .await
            {
                Ok(response) => {
                    let status = response.status().as_u16();
                    parse_deep_check(status, &response.text().await.unwrap_or_default())
                }
                Err(e) if e.is_timeout() => Err("状态检查超时".to_string()),
                Err(e) => Err(e.to_string()),
            };
            let current = move |st: &EngineState| {
                st.runtime_fp == Some(fp)
                    && matches!(st.sidecar, SidecarPhase::Ready { port: p, verified: false, .. } if p == port)
            };
            match result {
                Ok(()) => engine.feed_if(&app, SidecarEvent::DeepCheckPassed, |st| {
                    let current = current(st);
                    if current {
                        st.verified_fp = Some(fp);
                    }
                    current
                }),
                Err(message) => {
                    engine.feed_if(&app, SidecarEvent::DeepCheckFailed { message }, |st| {
                        current(st)
                    })
                }
            }
        });
    }

    fn kill(&self) {
        let mut st = self.lock();
        let Some(process) = st.process.take() else {
            return;
        };
        let previous = st.pending_kill.take();
        st.pending_kill = Some(std::thread::spawn(move || {
            if let Some(previous) = previous {
                let _ = previous.join();
            }
            process.terminate();
        }));
    }

    fn reap_exited(&self, app: &tauri::AppHandle) {
        let (code, process) = {
            let mut st = self.lock();
            let Some(status) = st.process.as_mut().and_then(SidecarProcess::try_exited) else {
                return;
            };
            (status.code(), st.process.take())
        };
        let stderr_tail = process
            .map(SidecarProcess::into_exit_report)
            .unwrap_or_default();
        self.feed(app, SidecarEvent::Exited { code, stderr_tail });
    }

    fn tick(&self, app: &tauri::AppHandle) {
        self.reap_exited(app);
        self.feed(app, SidecarEvent::Tick);
    }

    fn ensure_ledger(&self, app: &tauri::AppHandle, store: &StoreId) {
        if self.lock().ledger_for(store).is_some() {
            return;
        }
        let loaded = memory_root(app)
            .map(|root| load_ledger(&root.join(&store.0)))
            .unwrap_or_default();
        let mut st = self.lock();
        if st.ledger_for(store).is_none() {
            st.ledger = Some((store.clone(), loaded));
        }
    }

    fn flush_ledger(&self, app: &tauri::AppHandle, store: &StoreId) {
        let Some(ledger) = self.lock().ledger_for(store).map(|l| l.clone()) else {
            return;
        };
        let Some(root) = memory_root(app) else {
            return;
        };
        if let Err(e) = write_ledger_atomic(&root.join(&store.0), &ledger) {
            eprintln!("[AnyChat] memory ledger write failed: {e}");
        }
    }
}

// ── commands ────────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn memory_sync_runtime(
    app: tauri::AppHandle,
    engine: tauri::State<'_, MemoryEngine>,
    settings: RawMemorySettings,
    capture_targets: Vec<String>,
) -> Result<MemoryStatusDto, String> {
    engine.sync(&app, &settings, &capture_targets);
    Ok(engine.status())
}

#[tauri::command]
pub async fn memory_ingest(
    app: tauri::AppHandle,
    engine: tauri::State<'_, MemoryEngine>,
) -> Result<IngestSummaryDto, String> {
    Ok(engine.ingest(&app, IngestTrigger::Command).await)
}

#[tauri::command]
pub async fn memory_list(
    app: tauri::AppHandle,
    engine: tauri::State<'_, MemoryEngine>,
    query: ListQueryDto,
) -> Result<ListResultDto, String> {
    engine.list(&app, query.into()).await
}

#[tauri::command]
pub fn memory_status(engine: tauri::State<'_, MemoryEngine>) -> MemoryStatusDto {
    engine.status()
}

// ── lifecycle ───────────────────────────────────────────────────────────────

/// Ticks every `TICK_INTERVAL` and ingests every `SWEEP_INTERVAL`. When memory is
/// off a tick is one mutex lock.
pub fn start_sweep(app: tauri::AppHandle) {
    let ticks_per_sweep = (SWEEP_INTERVAL.as_secs() / TICK_INTERVAL.as_secs()).max(1);
    let spawned = std::thread::Builder::new()
        .name("anychat-memory-sweep".into())
        .spawn(move || {
            let mut ticks = 0u64;
            loop {
                std::thread::sleep(TICK_INTERVAL);
                ticks += 1;
                let engine = app.state::<MemoryEngine>();
                if ticks % ticks_per_sweep == 0 {
                    tauri::async_runtime::block_on(engine.ingest(&app, IngestTrigger::Sweep));
                }
                engine.tick(&app);
            }
        });
    if let Err(e) = spawned {
        eprintln!("[AnyChat] memory sweep failed to start: {e}");
    }
}

/// `RunEvent::Exit`: stop the sidecar. The ledger is already on disk.
pub fn shutdown(app: &tauri::AppHandle) {
    let engine = app.state::<MemoryEngine>();
    let (process, pending_kill) = {
        let mut st = engine.lock();
        st.spawn_seq += 1;
        (st.process.take(), st.pending_kill.take())
    };
    if let Some(pending_kill) = pending_kill {
        let _ = pending_kill.join();
    }
    if let Some(process) = process {
        process.terminate();
    }
}

fn memory_root(app: &tauri::AppHandle) -> Option<PathBuf> {
    app.path().app_data_dir().ok().map(|dir| dir.join("memory"))
}

fn sidecar_url(port: u16, path: &str) -> String {
    format!("http://127.0.0.1:{port}{path}")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Missing or unreadable ⇒ empty. Worst case is re-sending up to
/// `FIRST_SIGHT_SEND_TURNS` turns per conversation.
fn load_ledger(dir: &Path) -> Ledger {
    fs::read(dir.join(LEDGER_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Ledger>(&bytes).ok())
        .filter(|ledger| ledger.version == LEDGER_VERSION)
        .unwrap_or_default()
}

fn write_ledger_atomic(dir: &Path, ledger: &Ledger) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{LEDGER_FILE}.tmp"));
    fs::write(&tmp, serde_json::to_vec(ledger)?)?;
    fs::rename(&tmp, dir.join(LEDGER_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_dir(tag: &str) -> PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "anychat-memory-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn desired(targets: &[&str]) -> Desired {
        let raw: RawMemorySettings = serde_json::from_value(json!({
            "enabled": true,
            "generation": { "baseUrl": "https://api.example.com/v1", "apiKey": "sk-test", "model": "gpt-4.1-mini" },
            "embedding": { "source": { "kind": "reuse-generation" }, "model": "text-embedding-3-small", "dims": 1536 },
        }))
        .unwrap();
        let targets: Vec<String> = targets.iter().map(|t| t.to_string()).collect();
        desired_from(&raw, &targets)
    }

    fn state_with(desired: Desired) -> EngineState {
        let mut st = EngineState::new();
        st.desired = desired;
        st
    }

    fn skipped(reason: SkipReason) -> Result<(), IngestSummaryDto> {
        Err(IngestSummaryDto::Skipped { reason })
    }

    #[test]
    fn ingest_gate_skips_until_memory_has_targets() {
        assert_eq!(
            state_with(Desired::Off).begin_ingest(IngestTrigger::Command),
            skipped(SkipReason::MemoryOff)
        );
        assert_eq!(
            state_with(Desired::NeedsConfig(vec![])).begin_ingest(IngestTrigger::Sweep),
            skipped(SkipReason::NeedsConfig)
        );
        assert_eq!(
            state_with(desired(&[])).begin_ingest(IngestTrigger::Command),
            skipped(SkipReason::NoTargets)
        );
        assert_eq!(
            state_with(desired(&["chatgpt"])).begin_ingest(IngestTrigger::Command),
            Ok(())
        );
    }

    #[test]
    fn ingest_is_single_flight_and_only_commands_ask_for_a_rerun() {
        let mut st = state_with(desired(&["chatgpt"]));
        assert_eq!(st.begin_ingest(IngestTrigger::Sweep), Ok(()));
        assert_eq!(
            st.begin_ingest(IngestTrigger::Sweep),
            Err(IngestSummaryDto::Coalesced)
        );
        assert!(!st.take_rerun());
        assert_eq!(
            st.begin_ingest(IngestTrigger::Command),
            Err(IngestSummaryDto::Coalesced)
        );
        assert!(st.take_rerun());
        assert!(!st.take_rerun(), "a rerun request is consumed once");
    }

    #[test]
    fn runtime_dto_hides_unverified_ready_and_never_the_key() {
        let on = desired(&["chatgpt"]);
        let ready = |verified| SidecarPhase::Ready {
            port: 1,
            last_used_ms: 0,
            verified,
        };
        assert_eq!(
            runtime_dto(&Desired::Off, &ready(true)),
            RuntimePhaseDto::Off
        );
        assert!(matches!(
            runtime_dto(&on, &ready(false)),
            RuntimePhaseDto::Starting { .. }
        ));
        let RuntimePhaseDto::Ready { applied } = runtime_dto(&on, &ready(true)) else {
            panic!("expected ready");
        };
        assert_eq!(applied.model, "gpt-4.1-mini");
        assert_eq!(applied.endpoint_host, "api.example.com");
        assert!(applied.store_id.starts_with("text-embedding-3-small-1536-"));
        let json = serde_json::to_string(&runtime_dto(&on, &ready(true))).unwrap();
        assert!(!json.contains("sk-test"));
        assert!(matches!(
            runtime_dto(&on, &SidecarPhase::Idle),
            RuntimePhaseDto::Idle { .. }
        ));
    }

    #[test]
    fn failures_serialize_with_kebab_kinds_and_install_hint() {
        let dto = SidecarFailureDto::from(&SidecarFailure::ModuleMissing {
            python: "python3".into(),
        });
        assert_eq!(
            serde_json::to_value(dto).unwrap(),
            json!({"kind": "module-missing", "python": "python3", "installHint": "python3 -m pip install hy-memory==2.0.3"})
        );
        let dto = SidecarFailureDto::from(&SidecarFailure::ExitedOnStart {
            code: Some(1),
            stderr_tail: "x".into(),
        });
        assert_eq!(
            serde_json::to_value(dto).unwrap(),
            json!({"kind": "exited-on-start", "code": 1, "stderrTail": "x"})
        );
    }

    #[test]
    fn status_lists_probes_only_for_current_targets() {
        let mut st = state_with(desired(&["chatgpt"]));
        st.probes
            .insert(ServiceId("chatgpt".into()), ProbeResult::NoConversation);
        st.probes
            .insert(ServiceId("claude".into()), ProbeResult::NotOpen);
        let status = serde_json::to_value(st.status()).unwrap();
        assert_eq!(
            status["probes"],
            json!({"chatgpt": {"kind": "no-conversation"}})
        );
        assert_eq!(
            status["ingest"],
            json!({"state": "idle", "lastRun": null, "abandonedTurns": 0})
        );
        assert_eq!(status["runtime"]["phase"], "idle");
        assert!(status["supportedHosts"]
            .as_array()
            .unwrap()
            .contains(&Value::from("claude.ai")));
    }

    #[test]
    fn list_query_dto_maps_blank_service_to_all() {
        let q: ListQuery =
            serde_json::from_value::<ListQueryDto>(json!({"serviceId": " ", "offset": 50}))
                .unwrap()
                .into();
        assert_eq!(
            q,
            ListQuery {
                service_id: None,
                offset: 50,
                limit: 50
            }
        );
        let q: ListQuery = serde_json::from_value::<ListQueryDto>(
            json!({"serviceId": "claude", "offset": 0, "limit": 10}),
        )
        .unwrap()
        .into();
        assert_eq!(q.service_id, Some(ServiceId("claude".into())));
    }

    #[test]
    fn ledger_file_round_trips_and_tolerates_damage() {
        let dir = temp_dir("ledger");
        assert_eq!(load_ledger(&dir), Ledger::default());

        let mut ledger = Ledger::default();
        let conv = ConvRef {
            service: ServiceId("chatgpt".into()),
            conversation: ConversationKey("c1".into()),
        };
        apply_change(
            &mut ledger,
            &conv,
            &LedgerChange::Ack(vec![Fingerprint(7)]),
            1,
        );
        write_ledger_atomic(&dir, &ledger).unwrap();
        assert_eq!(load_ledger(&dir), ledger);
        assert!(!dir.join("ledger.json.tmp").exists());

        fs::write(dir.join(LEDGER_FILE), b"{not json").unwrap();
        assert_eq!(load_ledger(&dir), Ledger::default());
        fs::write(
            dir.join(LEDGER_FILE),
            br#"{"version": 99, "conversations": {}}"#,
        )
        .unwrap();
        assert_eq!(load_ledger(&dir), Ledger::default());
    }
}
