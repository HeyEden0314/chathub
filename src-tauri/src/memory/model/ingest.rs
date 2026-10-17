use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use super::wire::AddOutcome;

// ── identities ──────────────────────────────────────────────────────────────

/// AnyChat service id (`chatgpt`, `claude`, `custom-…`). Also the Hy-Memory `agent_id`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ServiceId(pub String);

/// Provider conversation id parsed from the page URL. Also the Hy-Memory `session_id`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ConversationKey(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ConvRef {
    pub service: ServiceId,
    pub conversation: ConversationKey,
}

impl ConvRef {
    pub fn ledger_key(&self) -> String {
        format!("{}::{}", self.service.0, self.conversation.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

// ── probe output ────────────────────────────────────────────────────────────

/// Normalized at the probe boundary: whitespace runs collapsed, non-empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedTurn {
    pub role: Role,
    pub text: String,
}

/// One read of one page: the tail window plus one turn of fingerprint context.
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub conv: ConvRef,
    /// The turn immediately before `turns[0]`; fingerprint context only, never sent.
    pub context_turn: Option<ObservedTurn>,
    /// Oldest first, at most `MAX_TURNS_PER_PROBE`.
    pub turns: Vec<ObservedTurn>,
    /// `Some(..)` only when the extractor has a reliable streaming signal.
    pub streaming: Option<bool>,
    pub observed_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ProbeResult {
    NotOpen,
    UnsupportedSite {
        host: String,
    },
    NoConversation,
    /// Conversation URL matched but zero turns matched: selector drift or still loading.
    UnrecognizedPage {
        conversation: ConversationKey,
    },
    Page(Snapshot),
    Failed {
        message: String,
    },
}

pub const MAX_TURNS_PER_PROBE: usize = 24;
/// On first sight of a conversation only this many trailing settled turns are sent;
/// older ones are acked without sending. Older history is AmberKeeper's domain.
pub const FIRST_SIGHT_SEND_TURNS: usize = 8;

#[derive(Clone, Copy, Debug)]
pub struct BatchLimits {
    pub max_turns: usize,
    pub max_chars: usize,
    pub user_turn_chars: usize,
    pub assistant_turn_chars: usize,
}

pub const DEFAULT_LIMITS: BatchLimits = BatchLimits {
    max_turns: 8,
    max_chars: 12_000,
    user_turn_chars: 4_000,
    assistant_turn_chars: 2_000,
};

const TRUNCATION_MARK: &str = "…[截断]";

// ── fingerprints and ledger ─────────────────────────────────────────────────

/// FNV-1a 64 over `service ‖ conversation ‖ role ‖ hash(prev.text) ‖ text`, full text.
/// `DefaultHasher` is not stable across Rust versions and the ledger is persisted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Fingerprint(pub u64);

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET;
    fnv_extend(&mut hash, bytes);
    hash
}

fn fnv_extend(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(FNV_PRIME);
    }
}

/// Length-prefixed fields so that `("ab", "c")` and `("a", "bc")` hash differently.
pub(super) fn fnv_fields(fields: &[&[u8]]) -> u64 {
    let mut hash = FNV_OFFSET;
    for field in fields {
        fnv_extend(&mut hash, &(field.len() as u64).to_le_bytes());
        fnv_extend(&mut hash, field);
    }
    hash
}

pub fn fingerprint(
    conv: &ConvRef,
    prev: Option<&ObservedTurn>,
    turn: &ObservedTurn,
) -> Fingerprint {
    let prev_hash = match prev {
        Some(p) => {
            let mut bytes = vec![1u8];
            bytes.extend_from_slice(&fnv1a64(p.text.as_bytes()).to_le_bytes());
            bytes
        }
        None => vec![0u8],
    };
    Fingerprint(fnv_fields(&[
        conv.service.0.as_bytes(),
        conv.conversation.0.as_bytes(),
        turn.role.as_str().as_bytes(),
        &prev_hash,
        turn.text.as_bytes(),
    ]))
}

/// Hash of (generation model, endpoint host). No key material. Scopes abandoned
/// marks so that changing the model retries previously failed turns.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GenerationIdentity(pub u64);

pub const LEDGER_FPS_PER_CONV: usize = 64;
pub const LEDGER_MAX_CONVS: usize = 1_000;
pub const LEDGER_VERSION: u32 = 1;

/// Fingerprints only. There is deliberately no text field.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationLedger {
    pub acked: VecDeque<Fingerprint>,
    pub abandoned: Vec<(Fingerprint, GenerationIdentity)>,
    pub last_seen_ms: u64,
}

impl ConversationLedger {
    /// Acked, or abandoned under the *current* generation identity.
    pub fn is_done(&self, fp: Fingerprint, gen: GenerationIdentity) -> bool {
        self.acked.contains(&fp) || self.abandoned.iter().any(|(f, g)| *f == fp && *g == gen)
    }

    fn ack(&mut self, fps: &[Fingerprint]) {
        for fp in fps {
            if !self.acked.contains(fp) {
                self.acked.push_back(*fp);
            }
        }
        while self.acked.len() > LEDGER_FPS_PER_CONV {
            self.acked.pop_front();
        }
        self.abandoned.retain(|(f, _)| !fps.contains(f));
    }

    fn abandon(&mut self, fps: &[Fingerprint], gen: GenerationIdentity) {
        self.abandoned
            .retain(|(f, g)| *g == gen && !fps.contains(f));
        self.abandoned.extend(fps.iter().map(|fp| (*fp, gen)));
        let overflow = self.abandoned.len().saturating_sub(LEDGER_FPS_PER_CONV);
        self.abandoned.drain(..overflow);
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Ledger {
    pub version: u32,
    pub conversations: HashMap<String, ConversationLedger>,
}

impl Default for Ledger {
    fn default() -> Self {
        Self {
            version: LEDGER_VERSION,
            conversations: HashMap::new(),
        }
    }
}

impl Ledger {
    pub fn get(&self, conv: &ConvRef) -> Option<&ConversationLedger> {
        self.conversations.get(&conv.ledger_key())
    }

    /// Evict least-recently-seen conversations beyond `LEDGER_MAX_CONVS`.
    pub fn compact(&mut self) {
        let overflow = self.conversations.len().saturating_sub(LEDGER_MAX_CONVS);
        if overflow == 0 {
            return;
        }
        let mut by_age: Vec<(u64, String)> = self
            .conversations
            .iter()
            .map(|(key, conv)| (conv.last_seen_ms, key.clone()))
            .collect();
        by_age.sort();
        for (_, key) in by_age.into_iter().take(overflow) {
            self.conversations.remove(&key);
        }
    }
}

/// Runtime only: the last turn of the previous probe and when it was first seen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TailObservation {
    pub fp: Fingerprint,
    pub at_ms: u64,
}

// ── planning ────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchTurn {
    pub role: Role,
    /// Truncated per `BatchLimits` with a trailing "…[截断]".
    pub content: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Batch {
    pub conv: ConvRef,
    pub turns: Vec<BatchTurn>,
    /// Parallel to `turns`; acked together on `AddOutcome::Stored`.
    pub fingerprints: Vec<Fingerprint>,
    pub observed_at_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConversationPlan {
    /// First sight only: settled turns older than the send window, acked without sending.
    pub baseline: Vec<Fingerprint>,
    /// Oldest first. Sent sequentially; never two adds in flight.
    pub batches: Vec<Batch>,
    pub deferred_turns: usize,
    pub next_tail: Option<TailObservation>,
}

/// Rule (c) needs the tail to stay unchanged for this long. Shorter than one sweep so
/// two consecutive sweeps always qualify, long enough that an immediate re-probe does not.
pub const SETTLE_QUIET: Duration = Duration::from_secs(20);

/// Settle rule. A turn is settled when ANY holds:
///   (a) it is followed by another turn in the snapshot;
///   (b) it is the last turn, an assistant turn, and `streaming == Some(false)`;
///   (c) it is the last turn, an assistant turn, unchanged for `SETTLE_QUIET`, and the
///       extractor does not report `streaming == Some(true)`.
/// Batches end at the last settled assistant turn; a trailing user turn waits for its reply.
pub fn plan_conversation(
    ledger: Option<&ConversationLedger>,
    prev_tail: Option<&TailObservation>,
    snap: &Snapshot,
    gen: GenerationIdentity,
    limits: &BatchLimits,
) -> ConversationPlan {
    let turns = &snap.turns;
    let mut plan = ConversationPlan::default();
    let Some(last) = turns.len().checked_sub(1) else {
        return plan;
    };

    let fps: Vec<Fingerprint> = (0..turns.len())
        .map(|i| {
            let prev = if i == 0 {
                snap.context_turn.as_ref()
            } else {
                Some(&turns[i - 1])
            };
            fingerprint(&snap.conv, prev, &turns[i])
        })
        .collect();

    let unchanged_tail = prev_tail.filter(|t| t.fp == fps[last]);
    plan.next_tail = Some(unchanged_tail.copied().unwrap_or(TailObservation {
        fp: fps[last],
        at_ms: snap.observed_at_ms,
    }));

    let last_settled = turns[last].role == Role::Assistant
        && (snap.streaming == Some(false)
            || (snap.streaming != Some(true)
                && unchanged_tail.is_some_and(|t| {
                    snap.observed_at_ms.saturating_sub(t.at_ms) >= SETTLE_QUIET.as_millis() as u64
                })));

    let end = if last_settled {
        turns.len()
    } else {
        (0..last)
            .rev()
            .find(|&i| turns[i].role == Role::Assistant)
            .map_or(0, |i| i + 1)
    };
    plan.deferred_turns = turns.len() - end;

    let start = if ledger.is_none() {
        first_sight_cut(turns, end)
    } else {
        0
    };
    plan.baseline = fps[..start].to_vec();

    let candidates: Vec<usize> = (start..end)
        .filter(|&i| ledger.map_or(true, |l| !l.is_done(fps[i], gen)))
        .collect();
    plan.batches = cut_batches(snap, &fps, &candidates, limits);
    plan
}

/// Start of the first-sight send window, moved forward to an exchange boundary.
fn first_sight_cut(turns: &[ObservedTurn], end: usize) -> usize {
    let mut cut = end.saturating_sub(FIRST_SIGHT_SEND_TURNS);
    while cut > 0 && cut < end && turns[cut].role != Role::User {
        cut += 1;
    }
    cut
}

fn cut_batches(
    snap: &Snapshot,
    fps: &[Fingerprint],
    candidates: &[usize],
    limits: &BatchLimits,
) -> Vec<Batch> {
    let turns = &snap.turns;
    let mut exchanges: Vec<Vec<usize>> = Vec::new();
    for &i in candidates {
        let continues = exchanges.last().is_some_and(|ex| {
            let prev = ex[ex.len() - 1];
            prev + 1 == i && !(turns[prev].role == Role::Assistant && turns[i].role == Role::User)
        });
        match exchanges.last_mut() {
            Some(ex) if continues => ex.push(i),
            _ => exchanges.push(vec![i]),
        }
    }

    let mut batches = Vec::new();
    let mut current: Vec<(BatchTurn, Fingerprint)> = Vec::new();
    let mut current_chars = 0usize;
    let flush = |current: &mut Vec<(BatchTurn, Fingerprint)>, batches: &mut Vec<Batch>| {
        if current.is_empty() {
            return;
        }
        let (turns, fingerprints) = current.drain(..).unzip();
        batches.push(Batch {
            conv: snap.conv.clone(),
            turns,
            fingerprints,
            observed_at_ms: snap.observed_at_ms,
        });
    };

    for exchange in exchanges {
        let items: Vec<(BatchTurn, Fingerprint)> = exchange
            .iter()
            .map(|&i| {
                let limit = match turns[i].role {
                    Role::User => limits.user_turn_chars,
                    Role::Assistant => limits.assistant_turn_chars,
                };
                let content = truncate_chars(&turns[i].text, limit);
                (
                    BatchTurn {
                        role: turns[i].role,
                        content,
                    },
                    fps[i],
                )
            })
            .collect();
        for chunk in items.chunks(limits.max_turns.max(1)) {
            let chunk_chars: usize = chunk.iter().map(|(t, _)| t.content.chars().count()).sum();
            if !current.is_empty()
                && (current.len() + chunk.len() > limits.max_turns
                    || current_chars + chunk_chars > limits.max_chars)
            {
                flush(&mut current, &mut batches);
                current_chars = 0;
            }
            current.extend_from_slice(chunk);
            current_chars += chunk_chars;
        }
    }
    flush(&mut current, &mut batches);
    batches
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push_str(TRUNCATION_MARK);
    out
}

pub const MAX_REJECTIONS: u8 = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LedgerChange {
    Ack(Vec<Fingerprint>),
    Abandon(Vec<Fingerprint>, GenerationIdentity),
    RetryLater,
}

/// Stored ⇒ Ack. Rejected ⇒ RetryLater until `MAX_REJECTIONS`, then Abandon.
/// Unknown ⇒ RetryLater and never counts toward abandonment.
pub fn after_add(
    outcome: &AddOutcome,
    prior_rejections: u8,
    gen: GenerationIdentity,
    batch: &Batch,
) -> LedgerChange {
    match outcome {
        AddOutcome::Stored { .. } => LedgerChange::Ack(batch.fingerprints.clone()),
        AddOutcome::Rejected { .. } if prior_rejections.saturating_add(1) >= MAX_REJECTIONS => {
            LedgerChange::Abandon(batch.fingerprints.clone(), gen)
        }
        AddOutcome::Rejected { .. } | AddOutcome::Unknown { .. } => LedgerChange::RetryLater,
    }
}

pub fn apply_change(ledger: &mut Ledger, conv: &ConvRef, change: &LedgerChange, now_ms: u64) {
    let entry = match change {
        LedgerChange::RetryLater => return,
        _ => ledger.conversations.entry(conv.ledger_key()).or_default(),
    };
    match change {
        LedgerChange::Ack(fps) => entry.ack(fps),
        LedgerChange::Abandon(fps, gen) => entry.abandon(fps, *gen),
        LedgerChange::RetryLater => {}
    }
    entry.last_seen_ms = entry.last_seen_ms.max(now_ms);
    ledger.compact();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::model::test_support::*;

    #[test]
    fn fnv1a64_matches_reference_vectors() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn fingerprint_depends_on_every_component() {
        let c = conv("chatgpt", "c1");
        let base = fingerprint(&c, Some(&user("hi")), &assistant("继续"));
        assert_ne!(
            base,
            fingerprint(&conv("claude", "c1"), Some(&user("hi")), &assistant("继续"))
        );
        assert_ne!(
            base,
            fingerprint(
                &conv("chatgpt", "c2"),
                Some(&user("hi")),
                &assistant("继续")
            )
        );
        assert_ne!(base, fingerprint(&c, Some(&user("hi")), &user("继续")));
        assert_ne!(
            base,
            fingerprint(&c, Some(&user("hello")), &assistant("继续"))
        );
        assert_ne!(base, fingerprint(&c, None, &assistant("继续")));
        assert_eq!(
            base,
            fingerprint(&c, Some(&assistant("hi")), &assistant("继续"))
        );
    }

    #[test]
    fn fingerprint_is_stable_across_builds() {
        let fp = fingerprint(&conv("chatgpt", "c1"), None, &user("hello"));
        assert_eq!(fp, Fingerprint(0xcbbc_5f92_d539_31d6));
    }

    #[test]
    fn settle_rule_a_followed_turns_are_settled() {
        let s = snap(vec![user("q1"), assistant("a1"), user("q2")], None, 1_000);
        let p = plan(Some(&empty_ledger()), None, &s);
        assert_eq!(sent_texts(&p), vec!["q1", "a1"]);
        assert_eq!(p.deferred_turns, 1);
    }

    #[test]
    fn settle_rule_b_streaming_false_settles_last_assistant_turn() {
        let s = snap(vec![user("q1"), assistant("a1")], Some(false), 1_000);
        let p = plan(Some(&empty_ledger()), None, &s);
        assert_eq!(sent_texts(&p), vec!["q1", "a1"]);
        assert_eq!(p.deferred_turns, 0);
    }

    #[test]
    fn last_assistant_turn_waits_without_a_settle_signal() {
        let s = snap(vec![user("q1"), assistant("a1")], None, 1_000);
        let p = plan(Some(&empty_ledger()), None, &s);
        assert!(p.batches.is_empty());
        assert_eq!(p.deferred_turns, 2);
        assert_eq!(p.next_tail.map(|t| t.at_ms), Some(1_000));
    }

    #[test]
    fn settle_rule_c_unchanged_tail_across_quiet_period_settles() {
        let s1 = snap(vec![user("q1"), assistant("a1")], None, 1_000);
        let p1 = plan(Some(&empty_ledger()), None, &s1);
        let tail = p1.next_tail.unwrap();

        let too_soon = snap(vec![user("q1"), assistant("a1")], None, 5_000);
        let p2 = plan(Some(&empty_ledger()), Some(&tail), &too_soon);
        assert!(p2.batches.is_empty());
        assert_eq!(
            p2.next_tail,
            Some(tail),
            "first-seen time is kept while unchanged"
        );

        let later = snap(vec![user("q1"), assistant("a1")], None, 31_000);
        let p3 = plan(Some(&empty_ledger()), Some(&tail), &later);
        assert_eq!(sent_texts(&p3), vec!["q1", "a1"]);
    }

    #[test]
    fn settle_rule_c_does_not_override_a_streaming_signal() {
        let tail = TailObservation {
            fp: fingerprint(&conv("chatgpt", "c1"), Some(&user("q1")), &assistant("a1")),
            at_ms: 0,
        };
        let s = snap(vec![user("q1"), assistant("a1")], Some(true), 60_000);
        assert!(plan(Some(&empty_ledger()), Some(&tail), &s)
            .batches
            .is_empty());
    }

    #[test]
    fn changed_tail_resets_observation_time() {
        let s1 = snap(vec![user("q1"), assistant("a1 partial")], None, 1_000);
        let tail = plan(Some(&empty_ledger()), None, &s1).next_tail.unwrap();
        let s2 = snap(
            vec![user("q1"), assistant("a1 partial and more")],
            None,
            40_000,
        );
        let p2 = plan(Some(&empty_ledger()), Some(&tail), &s2);
        assert!(p2.batches.is_empty());
        assert_eq!(p2.next_tail.map(|t| t.at_ms), Some(40_000));
    }

    #[test]
    fn sliding_window_keeps_fingerprints_stable() {
        let all = dialogue(30);
        let c = conv("chatgpt", "c1");
        let window = |start: usize| Snapshot {
            conv: c.clone(),
            context_turn: start.checked_sub(1).map(|i| all[i].clone()),
            turns: all[start..start + MAX_TURNS_PER_PROBE].to_vec(),
            streaming: Some(false),
            observed_at_ms: 0,
        };
        let fps_of = |s: &Snapshot| -> Vec<Fingerprint> {
            plan_conversation(
                None,
                None,
                s,
                GEN,
                &BatchLimits {
                    max_turns: 100,
                    max_chars: usize::MAX,
                    ..DEFAULT_LIMITS
                },
            )
            .baseline
            .into_iter()
            .chain(
                plan(None, None, s)
                    .batches
                    .into_iter()
                    .flat_map(|b| b.fingerprints),
            )
            .collect()
        };
        let first = fps_of(&window(4));
        let second = fps_of(&window(5));
        assert_eq!(first.len(), MAX_TURNS_PER_PROBE);
        assert_eq!(first[1..], second[..MAX_TURNS_PER_PROBE - 1]);
    }

    #[test]
    fn first_sight_baselines_older_turns_and_sends_the_tail() {
        let s = snap(dialogue(20), Some(false), 0);
        let p = plan(None, None, &s);
        assert_eq!(p.baseline.len(), 12);
        assert_eq!(
            sent_texts(&p),
            vec!["u12", "a13", "u14", "a15", "u16", "a17", "u18", "a19"]
        );
    }

    #[test]
    fn first_sight_window_starts_on_a_user_turn() {
        let turns = vec![
            user("u0"),
            assistant("a1"),
            user("u2"),
            assistant("a3"),
            user("u4"),
            assistant("a5"),
            assistant("a6"),
            user("u7"),
            assistant("a8"),
            user("u9"),
            assistant("a10"),
            user("u11"),
            assistant("a12"),
        ];
        let p = plan(None, None, &snap(turns, Some(false), 0));
        assert_eq!(p.baseline.len(), 7, "cut at a5 moves past a6 to u7");
        assert_eq!(sent_texts(&p), vec!["u7", "a8", "u9", "a10", "u11", "a12"]);
    }

    #[test]
    fn trailing_user_turn_is_deferred_on_first_sight() {
        let p = plan(None, None, &snap(dialogue(19), None, 0));
        assert_eq!(p.deferred_turns, 1);
        assert_eq!(p.baseline.len(), 10);
        assert_eq!(sent_texts(&p).first().map(String::as_str), Some("u10"));
    }

    #[test]
    fn short_first_sight_conversation_has_no_baseline() {
        let p = plan(None, None, &snap(dialogue(4), Some(false), 0));
        assert!(p.baseline.is_empty());
        assert_eq!(sent_texts(&p).len(), 4);
    }

    #[test]
    fn acked_turns_are_not_sent_again() {
        let s = snap(dialogue(4), Some(false), 0);
        let first = plan(Some(&empty_ledger()), None, &s);
        let mut ledger = Ledger::default();
        for b in &first.batches {
            apply_change(
                &mut ledger,
                &b.conv,
                &LedgerChange::Ack(b.fingerprints.clone()),
                10,
            );
        }
        let again = plan(ledger.get(&s.conv), None, &s);
        assert!(again.batches.is_empty());
    }

    #[test]
    fn branch_edit_yields_new_fingerprints() {
        let s1 = snap(
            vec![user("q1"), assistant("a1"), user("q2"), assistant("a2")],
            Some(false),
            0,
        );
        let mut ledger = Ledger::default();
        for b in plan(Some(&empty_ledger()), None, &s1).batches {
            apply_change(&mut ledger, &b.conv, &LedgerChange::Ack(b.fingerprints), 1);
        }
        let s2 = snap(
            vec![
                user("q1"),
                assistant("a1"),
                user("q2 edited"),
                assistant("a2'"),
            ],
            Some(false),
            2,
        );
        let p = plan(ledger.get(&s2.conv), None, &s2);
        assert_eq!(sent_texts(&p), vec!["q2 edited", "a2'"]);
    }

    #[test]
    fn truncation_changes_content_but_not_fingerprint() {
        let long = "字".repeat(DEFAULT_LIMITS.assistant_turn_chars + 50);
        let s = snap(vec![user("q"), assistant(&long)], Some(false), 0);
        let p = plan(Some(&empty_ledger()), None, &s);
        let batch = &p.batches[0];
        assert!(batch.turns[1].content.ends_with(TRUNCATION_MARK));
        assert_eq!(
            batch.turns[1].content.chars().count(),
            DEFAULT_LIMITS.assistant_turn_chars + TRUNCATION_MARK.chars().count()
        );
        assert_eq!(
            batch.fingerprints[1],
            fingerprint(&s.conv, Some(&s.turns[0]), &s.turns[1])
        );
    }

    #[test]
    fn batches_split_at_exchange_boundaries_by_turn_limit() {
        let limits = BatchLimits {
            max_turns: 4,
            ..DEFAULT_LIMITS
        };
        let s = snap(dialogue(10), Some(false), 0);
        let p = plan_conversation(Some(&empty_ledger()), None, &s, GEN, &limits);
        let sizes: Vec<usize> = p.batches.iter().map(|b| b.turns.len()).collect();
        assert_eq!(sizes, vec![4, 4, 2]);
        assert!(p.batches.iter().all(|b| b.turns[0].role == Role::User));
        assert!(p
            .batches
            .iter()
            .all(|b| b.turns.len() == b.fingerprints.len()));
    }

    #[test]
    fn batches_split_by_char_limit() {
        let limits = BatchLimits {
            max_chars: 10,
            ..DEFAULT_LIMITS
        };
        let s = snap(
            vec![
                user("aaaa"),
                assistant("bbbb"),
                user("cccc"),
                assistant("dddd"),
            ],
            Some(false),
            0,
        );
        let p = plan_conversation(Some(&empty_ledger()), None, &s, GEN, &limits);
        assert_eq!(p.batches.len(), 2);
    }

    #[test]
    fn after_add_maps_each_outcome() {
        let s = snap(dialogue(2), Some(false), 0);
        let batch = plan(Some(&empty_ledger()), None, &s).batches.remove(0);
        let stored = AddOutcome::Stored { memory_ids: vec![] };
        let rejected = AddOutcome::Rejected { reason: "x".into() };
        let unknown = AddOutcome::Unknown { reason: "x".into() };

        assert_eq!(
            after_add(&stored, 2, GEN, &batch),
            LedgerChange::Ack(batch.fingerprints.clone())
        );
        assert_eq!(
            after_add(&rejected, 0, GEN, &batch),
            LedgerChange::RetryLater
        );
        assert_eq!(
            after_add(&rejected, 1, GEN, &batch),
            LedgerChange::RetryLater
        );
        assert_eq!(
            after_add(&rejected, MAX_REJECTIONS - 1, GEN, &batch),
            LedgerChange::Abandon(batch.fingerprints.clone(), GEN)
        );
        assert_eq!(
            after_add(&unknown, 200, GEN, &batch),
            LedgerChange::RetryLater
        );
    }

    #[test]
    fn abandoned_marks_clear_on_a_new_generation_identity() {
        let c = conv("chatgpt", "c1");
        let fp = Fingerprint(42);
        let mut ledger = Ledger::default();
        apply_change(&mut ledger, &c, &LedgerChange::Abandon(vec![fp], GEN), 5);
        let entry = ledger.get(&c).unwrap();
        assert!(entry.is_done(fp, GEN));
        let new_gen = GenerationIdentity(8);
        assert!(!entry.is_done(fp, new_gen));

        apply_change(
            &mut ledger,
            &c,
            &LedgerChange::Abandon(vec![Fingerprint(43)], new_gen),
            6,
        );
        assert_eq!(
            ledger.get(&c).unwrap().abandoned,
            vec![(Fingerprint(43), new_gen)]
        );
    }

    #[test]
    fn retry_later_leaves_the_ledger_untouched() {
        let mut ledger = Ledger::default();
        apply_change(
            &mut ledger,
            &conv("chatgpt", "c1"),
            &LedgerChange::RetryLater,
            5,
        );
        assert!(ledger.conversations.is_empty());
    }

    #[test]
    fn ledger_caps_fingerprints_and_conversations() {
        let c = conv("chatgpt", "c1");
        let mut ledger = Ledger::default();
        let fps: Vec<Fingerprint> = (0..100).map(Fingerprint).collect();
        apply_change(&mut ledger, &c, &LedgerChange::Ack(fps), 1);
        let acked = &ledger.get(&c).unwrap().acked;
        assert_eq!(acked.len(), LEDGER_FPS_PER_CONV);
        assert_eq!(acked.front(), Some(&Fingerprint(36)));

        for i in 0..(LEDGER_MAX_CONVS as u64 + 5) {
            apply_change(
                &mut ledger,
                &conv("chatgpt", &format!("k{i}")),
                &LedgerChange::Ack(vec![]),
                10 + i,
            );
        }
        assert_eq!(ledger.conversations.len(), LEDGER_MAX_CONVS);
        assert!(
            ledger.get(&c).is_none(),
            "least recently seen is evicted first"
        );
    }

    #[test]
    fn ledger_round_trips_without_text() {
        let s = snap(
            vec![user("secret question"), assistant("secret answer")],
            Some(false),
            0,
        );
        let mut ledger = Ledger::default();
        for b in plan(Some(&empty_ledger()), None, &s).batches {
            apply_change(&mut ledger, &b.conv, &LedgerChange::Ack(b.fingerprints), 1);
        }
        let json = serde_json::to_string(&ledger).unwrap();
        assert!(!json.contains("secret"));
        assert_eq!(serde_json::from_str::<Ledger>(&json).unwrap(), ledger);
    }
}
