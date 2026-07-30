//! Domain types for the shared conversation ledger.
//!
//! Pure data only: no I/O, no clock reads, no async. Every timestamp that
//! appears here was assigned by the orchestrator and is carried as data.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Identifies one conversation. A conversation is a first-class row that
/// participants join, not a function of a platform room.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConversationId(pub Uuid);

/// Identifies one participant — human or agent. Attribution always references
/// this, never a raw platform identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ParticipantId(pub Uuid);

/// Identifies one draft in a participant's outbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DraftId(pub Uuid);

/// Position in a conversation's ledger. Assigned by the orchestrator in a
/// strictly increasing sequence; never derived from a clock or supplied by a
/// participant.
///
/// A conversation with no entries has head [`Seq::EMPTY`]; the first admitted
/// entry is `Seq(1)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Seq(pub i64);

impl Seq {
    /// Head sequence of a conversation with no admitted entries.
    pub const EMPTY: Seq = Seq(0);
}

/// Round number within a conversation. Rounds advance only when the previous
/// round closes, so this is monotonic per conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Round(pub i64);

/// Revision counter for a draft. Bumped on every revise so a retrieval that
/// lost a race against a concurrent reseal is detectable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DraftVersion(pub i64);

/// Whether a participant is a person or an agent.
///
/// This distinction is load-bearing in projection: an agent's trust posture
/// toward a peer agent differs from its posture toward a human principal, so
/// rendered attribution must keep the two tellable apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticipantKind {
    Human,
    Agent,
}

/// A member of one or more conversations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Participant {
    pub id: ParticipantId,
    pub kind: ParticipantKind,
    pub display_name: String,
    /// Set for agents, identifying the configured ZeroClaw agent alias.
    pub agent_alias: Option<String>,
}

/// What a ledger row is.
///
/// One variant, deliberately. The ledger holds only contributions a participant
/// meant to make; operational events (timeouts, container exits, retries,
/// rejections, spend) belong in the audit log and must never be projected into
/// anyone's context. Keeping this an enum means adding an operational variant
/// here is a visible, reviewable act rather than an accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Contribution,
}

/// One admitted contribution. This is the unit the ledger is made of and, in
/// aggregate, the context window of every participant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub conversation_id: ConversationId,
    pub seq: Seq,
    /// The round that admitted this entry. Entries sharing a round were
    /// revealed simultaneously and did not see one another.
    pub round: Round,
    pub participant_id: ParticipantId,
    pub content: String,
    pub kind: EntryKind,
    pub created_at: DateTime<Utc>,
    /// Tombstoned entries are excluded from projection but never deleted —
    /// the ledger is logically append-only.
    pub tombstoned: bool,
}

/// A contribution the orchestrator is about to admit. Carries no sequence: the
/// orchestrator assigns that at append time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewEntry {
    pub participant_id: ParticipantId,
    pub content: String,
    pub kind: EntryKind,
}

/// Where a draft is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftState {
    /// Still being revised. Never claimed by the orchestrator.
    Draft,
    /// The participant declares this ready. Only sealed drafts are claimed.
    Sealed,
    /// Nothing to add this round. Free, normal, and what lets a round close
    /// early rather than waiting out the timeout.
    Pass,
}

/// A participant's pending contribution for one conversation. At most one
/// exists per participant per conversation, revisable until the round closes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Draft {
    pub id: DraftId,
    pub conversation_id: ConversationId,
    pub participant_id: ParticipantId,
    pub content: String,
    /// The ledger head this draft was written against. A draft must be current
    /// to be admitted, so that every participant has actually contemplated the
    /// most recent contribution.
    pub composed_against_seq: Seq,
    pub version: DraftVersion,
    pub state: DraftState,
}

/// What became of a retrieved draft. Delivered back to its author out of band —
/// dispositions are *about* the ledger, never *in* it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Disposition {
    Admitted { seq: Seq },
    Rejected { reason: String },
    Stale { current_seq: Seq },
    Expunged,
}

/// The admission judgement for one draft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OracleVerdict {
    Admit,
    Reject {
        reason: String,
    },
    Stale {
        current_seq: Seq,
    },
    /// The draft is judged to be gaming the conversation (injection or
    /// similar). Containment truncates the ledger from `expunge_from`; the
    /// penalty is assessed individually against the proposer.
    Contaminated {
        expunge_from: Seq,
    },
}

/// Whether a conversation is a group discussion or a direct exchange.
///
/// A DM is not a different mechanism — it is a conversation with two members,
/// living in the same ledger under the same orchestrator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationKind {
    Group,
    Dm,
}

/// What a member may do in a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberRole {
    /// Receives context and may contribute. Counted by the round barrier.
    Contributor,
    /// Receives context but never contributes, and never holds up a round.
    Observer,
}

/// Where a member stands in the current round.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoundDisposition {
    /// Still thinking; the round cannot close on this member.
    Pending,
    /// Declared a contribution ready for retrieval.
    Sealed,
    /// Explicitly had nothing to add.
    Passed,
    /// Ran out the round's clock without responding. Treated as a pass, but
    /// recorded distinctly so a persistently silent participant is visible.
    TimedOut,
}

/// Kind of operational event.
///
/// These describe the machinery, not the discussion. They are written to the
/// audit log and **never** projected into a participant's context: an agent that
/// reads "peer timed out" will comment on it, and that is how runaway loops
/// begin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditKind {
    RoundOpened,
    RoundClosed,
    /// A member's round clock expired without a response.
    ParticipantTimedOut,
    /// A member could not be reached at all.
    ParticipantUnreachable,
    /// A draft was retrieved but not admitted.
    DraftRejected,
    /// A draft was composed against a superseded ledger head.
    DraftStale,
    /// A draft was judged to be gaming the conversation.
    Contaminated,
    /// Ledger content was withdrawn from projection.
    Expunged,
}

/// An operational event awaiting persistence.
///
/// Deliberately carries no timestamp: this crate never reads a clock. The store
/// stamps the arrival time, which is also the honest one for an operational log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEvent {
    pub conversation_id: Option<ConversationId>,
    pub participant_id: Option<ParticipantId>,
    pub round: Option<Round>,
    pub kind: AuditKind,
    /// Human-readable operational detail. Never rendered into any context.
    pub detail: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trips a value through JSON and asserts it survives unchanged.
    fn roundtrip<T>(value: T)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(&value).expect("serialize");
        let back: T = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(value, back, "round-trip changed the value (json: {json})");
    }

    fn participant_id() -> ParticipantId {
        ParticipantId(Uuid::from_u128(0x11))
    }

    fn conversation_id() -> ConversationId {
        ConversationId(Uuid::from_u128(0x22))
    }

    fn timestamp() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).expect("valid fixed timestamp")
    }

    #[test]
    fn newtypes_round_trip() {
        roundtrip(conversation_id());
        roundtrip(participant_id());
        roundtrip(DraftId(Uuid::from_u128(0x33)));
        roundtrip(Seq(42));
        roundtrip(Round(7));
        roundtrip(DraftVersion(3));
    }

    #[test]
    fn newtypes_serialize_transparently() {
        // Transparent representation keeps the wire format legible and stable:
        // a Seq is a number, not `{"0": n}`.
        assert_eq!(serde_json::to_string(&Seq(42)).expect("serialize"), "42");
        assert_eq!(serde_json::to_string(&Round(7)).expect("serialize"), "7");
        assert_eq!(
            serde_json::to_string(&conversation_id()).expect("serialize"),
            "\"00000000-0000-0000-0000-000000000022\""
        );
    }

    #[test]
    fn empty_head_is_zero_and_first_entry_is_one() {
        assert_eq!(Seq::EMPTY, Seq(0));
        assert!(Seq(1) > Seq::EMPTY, "first admitted entry follows the head");
    }

    #[test]
    fn participant_kinds_round_trip() {
        roundtrip(ParticipantKind::Human);
        roundtrip(ParticipantKind::Agent);
        roundtrip(Participant {
            id: participant_id(),
            kind: ParticipantKind::Agent,
            display_name: "reviewer".to_string(),
            agent_alias: Some("reviewer".to_string()),
        });
        roundtrip(Participant {
            id: participant_id(),
            kind: ParticipantKind::Human,
            display_name: "jonathan".to_string(),
            agent_alias: None,
        });
    }

    #[test]
    fn entries_round_trip() {
        roundtrip(EntryKind::Contribution);
        roundtrip(LedgerEntry {
            conversation_id: conversation_id(),
            seq: Seq(9),
            round: Round(3),
            participant_id: participant_id(),
            content: "the point I wish to make".to_string(),
            kind: EntryKind::Contribution,
            created_at: timestamp(),
            tombstoned: false,
        });
        roundtrip(NewEntry {
            participant_id: participant_id(),
            content: "pending contribution".to_string(),
            kind: EntryKind::Contribution,
        });
    }

    #[test]
    fn drafts_round_trip() {
        roundtrip(DraftState::Draft);
        roundtrip(DraftState::Sealed);
        roundtrip(DraftState::Pass);
        roundtrip(Draft {
            id: DraftId(Uuid::from_u128(0x44)),
            conversation_id: conversation_id(),
            participant_id: participant_id(),
            content: "a considered response".to_string(),
            composed_against_seq: Seq(9),
            version: DraftVersion(2),
            state: DraftState::Sealed,
        });
    }

    #[test]
    fn dispositions_round_trip() {
        roundtrip(Disposition::Admitted { seq: Seq(10) });
        roundtrip(Disposition::Rejected {
            reason: "empty content".to_string(),
        });
        roundtrip(Disposition::Stale {
            current_seq: Seq(12),
        });
        roundtrip(Disposition::Expunged);
    }

    #[test]
    fn oracle_verdicts_round_trip() {
        roundtrip(OracleVerdict::Admit);
        roundtrip(OracleVerdict::Reject {
            reason: "low quality".to_string(),
        });
        roundtrip(OracleVerdict::Stale {
            current_seq: Seq(12),
        });
        roundtrip(OracleVerdict::Contaminated {
            expunge_from: Seq(8),
        });
    }

    #[test]
    fn tagged_enums_are_self_describing_on_the_wire() {
        // Internally tagged so a disposition stays readable in audit output and
        // so adding a variant cannot silently alias an existing one.
        let json = serde_json::to_string(&Disposition::Admitted { seq: Seq(10) })
            .expect("serialize disposition");
        assert_eq!(json, r#"{"kind":"admitted","seq":10}"#);
    }

    #[test]
    fn conversation_and_membership_types_round_trip() {
        roundtrip(ConversationKind::Group);
        roundtrip(ConversationKind::Dm);
        roundtrip(MemberRole::Contributor);
        roundtrip(MemberRole::Observer);
        roundtrip(RoundDisposition::Pending);
        roundtrip(RoundDisposition::Sealed);
        roundtrip(RoundDisposition::Passed);
        roundtrip(RoundDisposition::TimedOut);
    }

    #[test]
    fn audit_events_round_trip() {
        roundtrip(AuditKind::RoundOpened);
        roundtrip(AuditKind::ParticipantTimedOut);
        roundtrip(AuditEvent {
            conversation_id: Some(conversation_id()),
            participant_id: Some(participant_id()),
            round: Some(Round(4)),
            kind: AuditKind::DraftRejected,
            detail: "empty content".to_string(),
        });
        roundtrip(AuditEvent {
            conversation_id: None,
            participant_id: None,
            round: None,
            kind: AuditKind::Expunged,
            detail: "operator action".to_string(),
        });
    }

    #[test]
    fn audit_events_carry_no_timestamp() {
        // The crate reads no clocks; the store stamps arrival time. If a
        // timestamp field is ever added here, a pure function could start
        // depending on wall time and quietly break byte-stable projection.
        let json = serde_json::to_string(&AuditEvent {
            conversation_id: None,
            participant_id: None,
            round: None,
            kind: AuditKind::RoundClosed,
            detail: String::new(),
        })
        .expect("serialize audit event");
        assert!(
            !json.contains("_at") && !json.contains("time"),
            "AuditEvent gained a time field: {json}"
        );
    }

    #[test]
    fn participant_ids_order_deterministically() {
        // Intra-round batch ordering sorts by participant, so a total order on
        // ParticipantId is what makes projection byte-stable.
        let mut ids = vec![
            ParticipantId(Uuid::from_u128(3)),
            ParticipantId(Uuid::from_u128(1)),
            ParticipantId(Uuid::from_u128(2)),
        ];
        ids.sort();
        assert_eq!(
            ids,
            vec![
                ParticipantId(Uuid::from_u128(1)),
                ParticipantId(Uuid::from_u128(2)),
                ParticipantId(Uuid::from_u128(3)),
            ]
        );
    }
}
