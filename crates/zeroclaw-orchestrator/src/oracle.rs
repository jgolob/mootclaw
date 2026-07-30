//! Judging whether a contribution should be admitted.
//!
//! The oracle is the orchestrator's taste, and the point where "the ledger holds
//! only contributions someone meant" is enforced. It is deliberately swappable:
//! a rule set, a small classifier, or a full model are all legitimate, and the
//! round driver does not care which.
//!
//! It is also the most privileged reader in the system — it sees rejected content
//! that never enters the ledger — and it costs one call per retrieved draft, so a
//! round of N participants is N+1 model calls if the oracle is a model.

use async_trait::async_trait;
use zeroclaw_ledger::types::{Draft, OracleVerdict};

/// Judges one draft.
#[async_trait]
pub trait Oracle: Send + Sync {
    async fn judge(&self, draft: &Draft) -> OracleVerdict;
}

/// Admits everything.
///
/// The walking-skeleton oracle: it makes the round loop observable before any
/// judgement policy exists to confuse the picture.
#[derive(Debug, Default, Clone, Copy)]
pub struct AdmitAll;

#[async_trait]
impl Oracle for AdmitAll {
    async fn judge(&self, _draft: &Draft) -> OracleVerdict {
        OracleVerdict::Admit
    }
}

/// Cheap, deterministic admission rules.
///
/// No model call, so a conversation can be gated meaningfully in tests and in a
/// deployment that has not chosen a judge yet.
#[derive(Debug, Clone)]
pub struct RulesOracle {
    /// Longest admissible contribution. A participant that dumps a transcript
    /// into the ledger degrades every other participant's context window.
    pub max_chars: usize,
}

impl Default for RulesOracle {
    fn default() -> Self {
        Self { max_chars: 8_000 }
    }
}

#[async_trait]
impl Oracle for RulesOracle {
    async fn judge(&self, draft: &Draft) -> OracleVerdict {
        if draft.content.trim().is_empty() {
            return OracleVerdict::Reject {
                reason: "empty contribution".to_string(),
            };
        }
        if draft.content.chars().count() > self.max_chars {
            return OracleVerdict::Reject {
                reason: format!(
                    "contribution is {} characters; the limit is {}",
                    draft.content.chars().count(),
                    self.max_chars
                ),
            };
        }
        OracleVerdict::Admit
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;
    use zeroclaw_ledger::types::{
        ConversationId, DraftId, DraftState, DraftVersion, ParticipantId, Seq,
    };

    fn draft(content: &str) -> Draft {
        Draft {
            id: DraftId(Uuid::from_u128(1)),
            conversation_id: ConversationId(Uuid::from_u128(2)),
            participant_id: ParticipantId(Uuid::from_u128(3)),
            content: content.to_string(),
            composed_against_seq: Seq(1),
            version: DraftVersion(1),
            state: DraftState::Sealed,
        }
    }

    #[tokio::test]
    async fn admit_all_admits() {
        assert_eq!(
            AdmitAll.judge(&draft("anything")).await,
            OracleVerdict::Admit
        );
    }

    #[tokio::test]
    async fn the_rules_oracle_rejects_empty_contributions() {
        let oracle = RulesOracle::default();
        for content in ["", "   ", "\n\t"] {
            assert!(
                matches!(
                    oracle.judge(&draft(content)).await,
                    OracleVerdict::Reject { .. }
                ),
                "empty content must not reach the ledger: {content:?}"
            );
        }
    }

    #[tokio::test]
    async fn the_rules_oracle_rejects_oversized_contributions() {
        let oracle = RulesOracle { max_chars: 10 };
        assert!(matches!(
            oracle
                .judge(&draft("this is well over ten characters"))
                .await,
            OracleVerdict::Reject { .. }
        ));
        assert_eq!(oracle.judge(&draft("short")).await, OracleVerdict::Admit);
    }

    #[tokio::test]
    async fn the_limit_counts_characters_not_bytes() {
        // A multi-byte contribution must not be rejected for being in a language
        // that does not fit in ASCII.
        let oracle = RulesOracle { max_chars: 5 };
        assert_eq!(oracle.judge(&draft("héllo")).await, OracleVerdict::Admit);
    }
}
