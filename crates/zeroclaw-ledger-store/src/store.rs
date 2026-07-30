//! The `LedgerStore` trait: the entire persistence surface of the shared ledger.
//!
//! One rule shapes this API: **the orchestrator is the sole writer of ledger
//! content, and the only way content enters the ledger is [`LedgerStore::append_batch`].**
//! Participants propose; they never reach a store. Operational events go through
//! [`LedgerStore::append_audit`], which cannot write to the ledger at all — the
//! two logs are separated by the type system rather than by discipline.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::time::Duration;
use zeroclaw_ledger::{
    AuditEvent, ConversationId, ConversationKind, DraftId, LedgerEntry, MemberRole, NewEntry,
    OracleVerdict, Participant, ParticipantId, Round, RoundDisposition, Seq,
};

/// Failures a store can report.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("conversation {0:?} not found")]
    ConversationNotFound(ConversationId),
    #[error("participant {0:?} not found")]
    ParticipantNotFound(ParticipantId),
    #[error("participant {participant:?} is not a member of conversation {conversation:?}")]
    NotAMember {
        conversation: ConversationId,
        participant: ParticipantId,
    },
    #[error("round {round:?} is not open for conversation {conversation:?}")]
    RoundNotOpen {
        conversation: ConversationId,
        round: Round,
    },
    #[error("proposal {0:?} not found")]
    ProposalNotFound(DraftId),
    /// The backing store failed. Carries a message rather than a typed cause so
    /// this enum does not have to name every backend's error type.
    #[error("ledger store backend failure: {0}")]
    Backend(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// A retrieved draft, recorded whether or not it was admitted.
///
/// Every proposal is persisted — rejections included — because admission
/// decisions are what the economic layer meters and what an operator needs to
/// audit. Rejected content never enters the ledger, but it is not lost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposalRecord {
    pub id: DraftId,
    pub conversation_id: ConversationId,
    pub round: Round,
    pub participant_id: ParticipantId,
    pub content: String,
    pub composed_against_seq: Seq,
    pub version: zeroclaw_ledger::DraftVersion,
}

/// An audit event as stored, with the arrival time the store assigned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAuditEvent {
    pub event: AuditEvent,
    pub occurred_at: DateTime<Utc>,
}

/// Standing state of a conversation.
///
/// Grouped into one read because the orchestrator needs all of it to open a
/// round: the timeout to compute a deadline, the head to stamp on drafts as the
/// sequence they must be current against, and the visibility floor to project
/// from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationMeta {
    pub kind: ConversationKind,
    pub round_timeout: Duration,
    pub head_seq: Seq,
    /// Entries at or below this sequence are no longer projected. Raised by
    /// containment; never lowered.
    pub visible_from: Seq,
}

/// Persistence for one shared ledger.
///
/// Implementations must uphold:
///
/// 1. **Monotonic sequences.** [`append_batch`](LedgerStore::append_batch) assigns strictly
///    increasing sequences within a conversation, never reusing one, even under
///    concurrent calls. Sequences come from the store, never from a clock or a
///    caller.
/// 2. **Input order preserved.** The sequences returned correspond positionally
///    to the entries passed in, so a caller that ordered a round deterministically
///    gets that order in the ledger.
/// 3. **Withheld content stays withheld.** [`entries_from`](LedgerStore::entries_from)
///    excludes tombstoned entries and anything below the conversation's
///    visibility floor, so containment cannot be undone by a caller that forgets
///    to filter.
/// 4. **Deterministic membership order.** [`members`](LedgerStore::members) returns
///    participants sorted by [`ParticipantId`], because projection must be
///    byte-stable and iteration order must never leak into it.
/// 5. **The logs stay separate.** No method may write an [`AuditEvent`] into
///    ledger content, and [`entries_from`](LedgerStore::entries_from) must never
///    return one.
#[async_trait]
pub trait LedgerStore: Send + Sync {
    async fn create_conversation(
        &self,
        kind: ConversationKind,
        round_timeout: Duration,
    ) -> Result<ConversationId>;

    /// Registers a participant, or updates its display name and alias.
    async fn upsert_participant(&self, participant: Participant) -> Result<()>;

    async fn add_member(
        &self,
        conversation: ConversationId,
        participant: ParticipantId,
        role: MemberRole,
    ) -> Result<()>;

    /// Members of a conversation, **sorted by [`ParticipantId`]** (invariant 4).
    async fn members(&self, conversation: ConversationId) -> Result<Vec<Participant>>;

    /// Role of one member.
    async fn member_role(
        &self,
        conversation: ConversationId,
        participant: ParticipantId,
    ) -> Result<MemberRole>;

    /// Current ledger head. [`Seq::EMPTY`] when nothing has been admitted.
    async fn head_seq(&self, conversation: ConversationId) -> Result<Seq>;

    async fn conversation_meta(&self, conversation: ConversationId) -> Result<ConversationMeta>;

    /// Appends a round's admitted contributions as one batch.
    ///
    /// The only write path for ledger content. Entries sharing a round were
    /// revealed simultaneously and did not see one another, so the caller is
    /// responsible for having ordered them deterministically before calling.
    /// Returns the assigned sequences, positionally matching `entries`.
    async fn append_batch(
        &self,
        conversation: ConversationId,
        round: Round,
        entries: Vec<NewEntry>,
    ) -> Result<Vec<Seq>>;

    /// Visible, non-tombstoned entries with `seq > from`, ascending.
    ///
    /// Pass [`Seq::EMPTY`] for the whole conversation.
    async fn entries_from(
        &self,
        conversation: ConversationId,
        from: Seq,
    ) -> Result<Vec<LedgerEntry>>;

    /// Opens a round, marking every contributor [`RoundDisposition::Pending`].
    ///
    /// Observers are not enrolled: they receive context but never hold up a round.
    async fn open_round(
        &self,
        conversation: ConversationId,
        round: Round,
        deadline: DateTime<Utc>,
    ) -> Result<()>;

    /// Per-member standing in a round, sorted by [`ParticipantId`].
    async fn round_state(
        &self,
        conversation: ConversationId,
        round: Round,
    ) -> Result<Vec<(ParticipantId, RoundDisposition)>>;

    /// Deadline of an open round.
    ///
    /// Persisted so a round survives an orchestrator restart rather than
    /// silently restarting its clock.
    async fn round_deadline(
        &self,
        conversation: ConversationId,
        round: Round,
    ) -> Result<DateTime<Utc>>;

    async fn record_round_disposition(
        &self,
        conversation: ConversationId,
        round: Round,
        participant: ParticipantId,
        disposition: RoundDisposition,
    ) -> Result<()>;

    /// Records a retrieved draft, before it has been judged.
    async fn record_proposal(&self, proposal: ProposalRecord) -> Result<()>;

    async fn set_proposal_verdict(&self, proposal: DraftId, verdict: OracleVerdict) -> Result<()>;

    async fn proposal_verdict(&self, proposal: DraftId) -> Result<Option<OracleVerdict>>;

    /// Records an operational event. Cannot reach ledger content.
    async fn append_audit(&self, event: AuditEvent) -> Result<()>;

    /// Operational events for a conversation, oldest first.
    ///
    /// Exists so tests can prove audit content never reaches a participant's
    /// context, and so operators can investigate. Nothing on the projection path
    /// may call this.
    async fn audit_events(&self, conversation: ConversationId) -> Result<Vec<StoredAuditEvent>>;

    /// Withdraws every entry with `seq >= from` from projection.
    ///
    /// Tombstones; never deletes. Returns how many entries were affected.
    async fn tombstone_from(&self, conversation: ConversationId, from: Seq) -> Result<u64>;

    /// Raises the visibility floor: entries at or below `seq` stop being projected.
    ///
    /// Monotonic — a call that would lower the floor is a no-op, so containment
    /// cannot be reversed by a later caller.
    async fn set_visible_from(&self, conversation: ConversationId, seq: Seq) -> Result<()>;
}
