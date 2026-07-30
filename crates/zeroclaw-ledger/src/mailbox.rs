//! The mailbox protocol: what the orchestrator and a participant exchange.
//!
//! The relationship is asynchronous within a round and synchronous between
//! rounds. A participant receives an [`InboxEnvelope`] whenever contributions are
//! admitted, works at its own pace, and leaves a revisable [`Draft`] in its
//! outbox. The orchestrator retrieves only *sealed* drafts, judges them, and
//! reports back a [`DispositionNotice`].
//!
//! These types live with the domain rather than with either transport so the two
//! sides cannot drift: a participant and the orchestrator compile against the
//! same definitions.
//!
//! # The envelope carries no operational text
//!
//! [`InboxEnvelope`] is the *only* thing that becomes a participant's context, and
//! it deliberately has no field that could carry a timeout, a retry, a container
//! failure, or another participant's rejection reason. An agent that reads "peer
//! timed out" will comment on it, and a conversation about the machinery is how
//! runaway loops begin. `inbox_envelope_carries_no_operational_fields` pins the
//! exact field set so that adding one fails loudly rather than quietly.

use crate::projection::ProjectedMessage;
use crate::types::{ConversationId, Disposition, Draft, Round, Seq};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// How a participant stands against its spending budget.
///
/// Amounts are integer micro-USD rather than floats: money must compare and
/// accumulate exactly, and a float would make the same spend render differently
/// on different paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetStanding {
    /// Contributions admitted so far, across the participant's whole history.
    pub accepted_count: i64,
    /// Spend accumulated since the last admission — including tool use, code
    /// execution, and drafts that were retrieved but not admitted.
    pub spend_since_accepted_micros: i64,
    /// Rolling cost per admitted contribution. `None` before the first admission.
    pub cost_per_accepted_micros: Option<i64>,
    pub throttle: ThrottleState,
}

/// How constrained a participant currently is.
///
/// The right to spend is earned by contributing. A participant whose work lands
/// keeps its latitude; one that burns tokens without contributing loses it, with
/// no rule needed that defines "spinning".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ThrottleState {
    /// Unconstrained.
    Normal,
    /// Still admitted, but behind others when a round is ordered by priority.
    ReducedPriority,
    /// Capped on tool calls per round.
    ToolCapped { max_tool_calls: u32 },
    /// Not enrolled in rounds until spend is reconciled.
    Suspended,
}

impl BudgetStanding {
    /// Standing for a participant that has spent nothing and contributed nothing.
    pub fn fresh() -> Self {
        Self {
            accepted_count: 0,
            spend_since_accepted_micros: 0,
            cost_per_accepted_micros: None,
            throttle: ThrottleState::Normal,
        }
    }
}

/// Everything a participant needs to compose one round's contribution.
///
/// Delivery is idempotent on `(conversation_id, round)`: redelivery is always
/// safe, so the orchestrator may retry without coordinating.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxEnvelope {
    pub conversation_id: ConversationId,
    pub round: Round,
    /// The ledger head this round opened at. A contribution must be composed
    /// against it to be admitted.
    pub head_seq: Seq,
    /// When this round stops accepting responses.
    pub deadline: DateTime<Utc>,
    /// The conversation, projected for this reader.
    pub context: Vec<ProjectedMessage>,
    /// This participant's private working memory for this conversation.
    pub scratchpad: Option<String>,
    /// This participant's self-authored rules of engagement.
    pub soul: Option<String>,
    pub budget: BudgetStanding,
    /// What became of this participant's own recent drafts.
    ///
    /// Transient and private: never persisted into the ledger and never visible
    /// to anyone else. Without it, a participant whose draft was rejected would
    /// re-propose the same point indefinitely.
    pub dispositions: Vec<Disposition>,
}

/// A participant's outbox as retrieved.
///
/// `None` and a draft in [`DraftState::Pass`](crate::types::DraftState::Pass) both
/// mean "nothing this round", but `Pass` is preferred: it is an explicit
/// response, and it lets the round close as soon as everyone has answered rather
/// than waiting out the clock.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OutboxResponse {
    pub draft: Option<Draft>,
}

/// What the orchestrator did with a retrieved draft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispositionNotice {
    pub conversation_id: ConversationId,
    pub round: Round,
    pub disposition: Disposition,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::ProjectedRole;
    use crate::types::{DraftId, DraftState, DraftVersion, ParticipantId};
    use std::collections::BTreeSet;
    use uuid::Uuid;

    fn conversation() -> ConversationId {
        ConversationId(Uuid::from_u128(0xC0))
    }

    fn envelope() -> InboxEnvelope {
        InboxEnvelope {
            conversation_id: conversation(),
            round: Round(3),
            head_seq: Seq(12),
            deadline: DateTime::from_timestamp(1_700_000_120, 0).expect("valid fixed timestamp"),
            context: vec![ProjectedMessage {
                role: ProjectedRole::User,
                content: "[human:jonathan] what do you think?".to_string(),
            }],
            scratchpad: Some("still weighing the timeout default".to_string()),
            soul: Some("Prefer evidence over assertion.".to_string()),
            budget: BudgetStanding::fresh(),
            dispositions: vec![Disposition::Rejected {
                reason: "adds nothing".to_string(),
            }],
        }
    }

    fn roundtrip<T>(value: T)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(&value).expect("serialize");
        let back: T = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(value, back, "round-trip changed the value (json: {json})");
    }

    #[test]
    fn envelope_round_trips() {
        roundtrip(envelope());
    }

    #[test]
    fn outbox_and_disposition_round_trip() {
        roundtrip(OutboxResponse { draft: None });
        roundtrip(OutboxResponse {
            draft: Some(Draft {
                id: DraftId(Uuid::from_u128(7)),
                conversation_id: conversation(),
                participant_id: ParticipantId(Uuid::from_u128(2)),
                content: "a considered contribution".to_string(),
                composed_against_seq: Seq(12),
                version: DraftVersion(2),
                state: DraftState::Sealed,
            }),
        });
        roundtrip(DispositionNotice {
            conversation_id: conversation(),
            round: Round(3),
            disposition: Disposition::Admitted { seq: Seq(13) },
        });
    }

    #[test]
    fn budget_and_throttle_states_round_trip() {
        roundtrip(BudgetStanding::fresh());
        for throttle in [
            ThrottleState::Normal,
            ThrottleState::ReducedPriority,
            ThrottleState::ToolCapped { max_tool_calls: 3 },
            ThrottleState::Suspended,
        ] {
            roundtrip(BudgetStanding {
                accepted_count: 4,
                spend_since_accepted_micros: 12_500,
                cost_per_accepted_micros: Some(3_125),
                throttle,
            });
        }
    }

    #[test]
    fn inbox_envelope_carries_no_operational_fields() {
        // The envelope is the whole of a participant's context. Nothing about the
        // machinery — timeouts, retries, container failures, other participants'
        // rejection reasons — may ride along in it.
        //
        // This test pins the exact field set. Adding a field is allowed, but it
        // must be a deliberate act that updates this list, not something that
        // slips in because an envelope was a convenient place to put it.
        let json = serde_json::to_value(envelope()).expect("serialize envelope");
        let object = json.as_object().expect("envelope is a JSON object");
        let actual: BTreeSet<&str> = object.keys().map(String::as_str).collect();

        let expected: BTreeSet<&str> = [
            "conversation_id",
            "round",
            "head_seq",
            "deadline",
            "context",
            "scratchpad",
            "soul",
            "budget",
            "dispositions",
        ]
        .into_iter()
        .collect();

        assert_eq!(
            actual, expected,
            "InboxEnvelope's field set changed. If deliberate, update this list — and confirm the \
             new field cannot carry operational text into a participant's context."
        );
    }

    #[test]
    fn dispositions_in_the_envelope_are_only_the_readers_own() {
        // A rejection reason is feedback to its author, not news for the room. The
        // envelope carries a bare list precisely because it belongs to one
        // participant — there is no author field for it to be about someone else.
        let envelope = envelope();
        let json = serde_json::to_value(&envelope).expect("serialize");
        let dispositions = json
            .get("dispositions")
            .and_then(|value| value.as_array())
            .expect("dispositions is an array");
        for disposition in dispositions {
            let object = disposition.as_object().expect("disposition is an object");
            assert!(
                !object.contains_key("participant_id") && !object.contains_key("author"),
                "a disposition must not name a participant: {object:?}"
            );
        }
    }

    #[test]
    fn an_empty_outbox_is_representable() {
        // Silence must be as easy to express as speech, and cost nothing.
        assert_eq!(OutboxResponse::default(), OutboxResponse { draft: None });
    }
}
