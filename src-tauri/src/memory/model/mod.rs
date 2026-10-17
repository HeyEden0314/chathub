//! Pure core of the memory feature: domain types, settings validation, sidecar
//! environment, fingerprints, ledger operations, the ingest planner, the sidecar
//! state machine, and Hy-Memory / probe JSON parsing. No tauri, no reqwest, no IO.

mod ingest;
mod runtime;
mod wire;

pub use ingest::*;
pub use runtime::*;
pub use wire::*;

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub fn conv(service: &str, id: &str) -> ConvRef {
        ConvRef {
            service: ServiceId(service.to_string()),
            conversation: ConversationKey(id.to_string()),
        }
    }

    pub fn user(text: &str) -> ObservedTurn {
        ObservedTurn {
            role: Role::User,
            text: text.to_string(),
        }
    }

    pub fn assistant(text: &str) -> ObservedTurn {
        ObservedTurn {
            role: Role::Assistant,
            text: text.to_string(),
        }
    }

    /// `u0 a1 u2 a3 …` alternating, `n` turns.
    pub fn dialogue(n: usize) -> Vec<ObservedTurn> {
        (0..n)
            .map(|i| {
                if i % 2 == 0 {
                    user(&format!("u{i}"))
                } else {
                    assistant(&format!("a{i}"))
                }
            })
            .collect()
    }

    pub fn snap(turns: Vec<ObservedTurn>, streaming: Option<bool>, at: u64) -> Snapshot {
        Snapshot {
            conv: conv("chatgpt", "c1"),
            context_turn: None,
            turns,
            streaming,
            observed_at_ms: at,
        }
    }

    pub const GEN: GenerationIdentity = GenerationIdentity(7);

    pub fn plan(
        ledger: Option<&ConversationLedger>,
        tail: Option<&TailObservation>,
        s: &Snapshot,
    ) -> ConversationPlan {
        plan_conversation(ledger, tail, s, GEN, &DEFAULT_LIMITS)
    }

    pub fn sent_texts(p: &ConversationPlan) -> Vec<String> {
        p.batches
            .iter()
            .flat_map(|b| b.turns.iter().map(|t| t.content.clone()))
            .collect()
    }

    pub fn empty_ledger() -> ConversationLedger {
        ConversationLedger::default()
    }
}
