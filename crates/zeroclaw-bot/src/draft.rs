//! A participant's outbox: at most one revisable draft per conversation.
//!
//! The draft is the participant's side of the mailbox protocol. It may be revised
//! freely while the participant thinks — as new contributions arrive, an
//! in-progress answer often stops being the right one — and it becomes eligible
//! for retrieval only when *sealed*.
//!
//! Sealing is the participant's declaration of intent. It is what stops the
//! orchestrator from retrieving half a thought: without an explicit state, a
//! retrieval racing a rewrite would have to be resolved by timing, and the
//! conversation would occasionally record something nobody meant to say.
//!
//! The bot side is not trusted to enforce this. The orchestrator claims only
//! sealed drafts of its own accord; this type keeps the participant honest with
//! itself, not the protocol safe from a hostile participant.

use chrono::{DateTime, Utc};
use zeroclaw_ledger::types::{
    ConversationId, Draft, DraftId, DraftState, DraftVersion, ParticipantId, Round, Seq,
};

/// The participant's working draft for one conversation, with the round it
/// belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftSlot {
    draft: Draft,
    /// Which round this draft answers. A draft never outlives its round's purpose.
    round: Round,
    updated_at: DateTime<Utc>,
}

impl DraftSlot {
    pub fn draft(&self) -> &Draft {
        &self.draft
    }

    pub fn round(&self) -> Round {
        self.round
    }

    pub fn updated_at(&self) -> DateTime<Utc> {
        self.updated_at
    }

    pub fn state(&self) -> DraftState {
        self.draft.state
    }

    pub fn is_sealed(&self) -> bool {
        matches!(self.draft.state, DraftState::Sealed)
    }
}

/// Builds and revises the draft for one conversation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outbox {
    slot: Option<DraftSlot>,
}

impl Outbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// The current draft, whatever its state.
    ///
    /// Returns unsealed drafts too. Filtering to sealed ones is the
    /// orchestrator's job — a participant that hid its own drafts from itself
    /// would gain nothing, and the honest answer to "what is in your outbox" is
    /// what is actually there.
    pub fn peek(&self) -> Option<&DraftSlot> {
        self.slot.as_ref()
    }

    /// Writes or rewrites the draft, leaving it unsealed.
    ///
    /// Each revision bumps the version, so a retrieval that lost a race against a
    /// concurrent rewrite is detectable after the fact rather than silent.
    pub fn revise(
        &mut self,
        conversation: ConversationId,
        participant: ParticipantId,
        round: Round,
        composed_against_seq: Seq,
        content: impl Into<String>,
        now: DateTime<Utc>,
    ) -> &DraftSlot {
        let content = content.into();
        let next_version = self
            .slot
            .as_ref()
            .filter(|slot| slot.round == round)
            .map(|slot| DraftVersion(slot.draft.version.0 + 1))
            .unwrap_or(DraftVersion(1));

        let id = self
            .slot
            .as_ref()
            .filter(|slot| slot.round == round)
            .map(|slot| slot.draft.id)
            .unwrap_or_else(|| DraftId(uuid::Uuid::new_v4()));

        // `Option::insert` hands back the reference directly, so there is no
        // "assign then unwrap" panic path to justify.
        &*self.slot.insert(DraftSlot {
            draft: Draft {
                id,
                conversation_id: conversation,
                participant_id: participant,
                content,
                composed_against_seq,
                version: next_version,
                state: DraftState::Draft,
            },
            round,
            updated_at: now,
        })
    }

    /// Declares the current draft ready for retrieval.
    ///
    /// Returns `false` when there is nothing to seal.
    pub fn seal(&mut self, now: DateTime<Utc>) -> bool {
        match self.slot.as_mut() {
            Some(slot) => {
                slot.draft.state = DraftState::Sealed;
                slot.updated_at = now;
                true
            }
            None => false,
        }
    }

    /// Declares that this participant has nothing to add this round.
    ///
    /// A pass is a real response, not an absence: it lets the round close as soon
    /// as everyone has answered instead of waiting out the clock. Cheaper for the
    /// conversation than silence, and free for the participant.
    pub fn pass(
        &mut self,
        conversation: ConversationId,
        participant: ParticipantId,
        round: Round,
        composed_against_seq: Seq,
        now: DateTime<Utc>,
    ) -> &DraftSlot {
        &*self.slot.insert(DraftSlot {
            draft: Draft {
                id: DraftId(uuid::Uuid::new_v4()),
                conversation_id: conversation,
                participant_id: participant,
                content: String::new(),
                composed_against_seq,
                version: DraftVersion(1),
                state: DraftState::Pass,
            },
            round,
            updated_at: now,
        })
    }

    /// Discards the draft outright.
    pub fn clear(&mut self) {
        self.slot = None;
    }

    /// Prepares the outbox for `round`.
    ///
    /// An unsealed draft for an earlier round is discarded: it answers a question
    /// the conversation has moved past, and keeping it risks sealing a stale
    /// thought by accident.
    ///
    /// A *sealed* draft is kept, because retrieval may still be in flight and
    /// discarding it could lose a contribution the participant did commit to. If
    /// it turns out to be stale, the orchestrator's currency check refuses it and
    /// tells the author to reconsider — a safer outcome than silently dropping
    /// work here.
    pub fn open_round(&mut self, round: Round) {
        let discard = self
            .slot
            .as_ref()
            .is_some_and(|slot| slot.round != round && !slot.is_sealed());
        if discard {
            self.slot = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn conversation() -> ConversationId {
        ConversationId(Uuid::from_u128(0xC0))
    }

    fn participant() -> ParticipantId {
        ParticipantId(Uuid::from_u128(2))
    }

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).expect("valid fixed timestamp")
    }

    fn revise(outbox: &mut Outbox, round: i64, content: &str) {
        outbox.revise(
            conversation(),
            participant(),
            Round(round),
            Seq(10),
            content,
            now(),
        );
    }

    #[test]
    fn a_fresh_outbox_is_empty() {
        assert!(Outbox::new().peek().is_none());
    }

    #[test]
    fn revising_bumps_the_version_and_keeps_the_draft_unsealed() {
        let mut outbox = Outbox::new();
        revise(&mut outbox, 1, "first thought");
        let slot = outbox.peek().expect("draft exists");
        assert_eq!(slot.draft().version, DraftVersion(1));
        assert_eq!(slot.state(), DraftState::Draft);
        let id = slot.draft().id;

        revise(&mut outbox, 1, "better thought");
        let slot = outbox.peek().expect("draft exists");
        assert_eq!(slot.draft().version, DraftVersion(2));
        assert_eq!(slot.draft().content, "better thought");
        assert_eq!(
            slot.draft().id,
            id,
            "revising keeps the same draft identity within a round"
        );
    }

    #[test]
    fn sealing_marks_the_draft_retrievable() {
        let mut outbox = Outbox::new();
        revise(&mut outbox, 1, "considered");
        assert!(outbox.seal(now()));

        let slot = outbox.peek().expect("draft exists");
        assert!(slot.is_sealed());
        assert_eq!(slot.draft().state, DraftState::Sealed);
    }

    #[test]
    fn sealing_an_empty_outbox_reports_failure() {
        let mut outbox = Outbox::new();
        assert!(!outbox.seal(now()), "there is nothing to declare ready");
        assert!(outbox.peek().is_none());
    }

    #[test]
    fn resealing_after_a_revision_bumps_the_version_again() {
        // The race the version exists for: sealed at v1, rewritten and resealed at
        // v2 while a retrieval was in flight. The orchestrator can tell afterwards
        // which one it got.
        let mut outbox = Outbox::new();
        revise(&mut outbox, 1, "v1");
        outbox.seal(now());

        revise(&mut outbox, 1, "v2");
        assert_eq!(
            outbox.peek().expect("draft").state(),
            DraftState::Draft,
            "revising un-seals: the participant is thinking again"
        );
        outbox.seal(now());

        let slot = outbox.peek().expect("draft");
        assert_eq!(slot.draft().version, DraftVersion(2));
        assert!(slot.is_sealed());
    }

    #[test]
    fn passing_records_an_explicit_empty_response() {
        let mut outbox = Outbox::new();
        outbox.pass(conversation(), participant(), Round(1), Seq(10), now());

        let slot = outbox.peek().expect("pass is recorded");
        assert_eq!(slot.state(), DraftState::Pass);
        assert!(slot.draft().content.is_empty());
        assert!(
            !slot.is_sealed(),
            "a pass is not a contribution to retrieve"
        );
    }

    #[test]
    fn a_new_round_discards_an_unsealed_draft() {
        let mut outbox = Outbox::new();
        revise(&mut outbox, 1, "answering the old question");
        outbox.open_round(Round(2));
        assert!(
            outbox.peek().is_none(),
            "an unsealed draft answers a question the room has moved past"
        );
    }

    #[test]
    fn a_new_round_keeps_a_sealed_draft_for_retrieval() {
        // Retrieval may still be in flight. Dropping it here could lose work the
        // participant committed to; if it is stale, the orchestrator refuses it
        // and says so, which is recoverable.
        let mut outbox = Outbox::new();
        revise(&mut outbox, 1, "committed");
        outbox.seal(now());

        outbox.open_round(Round(2));
        let slot = outbox.peek().expect("sealed draft survives");
        assert!(slot.is_sealed());
        assert_eq!(slot.round(), Round(1));
    }

    #[test]
    fn opening_the_same_round_again_is_harmless() {
        // Inbox delivery is idempotent, so this happens on any retry.
        let mut outbox = Outbox::new();
        revise(&mut outbox, 1, "in progress");
        outbox.open_round(Round(1));
        assert_eq!(
            outbox.peek().expect("draft survives").draft().content,
            "in progress",
            "a redelivered inbox must not destroy work in progress"
        );
    }

    #[test]
    fn a_draft_for_a_new_round_starts_at_version_one() {
        let mut outbox = Outbox::new();
        revise(&mut outbox, 1, "round one");
        revise(&mut outbox, 2, "round two");
        assert_eq!(
            outbox.peek().expect("draft").draft().version,
            DraftVersion(1),
            "versions count revisions within a round, not across the conversation"
        );
    }

    #[test]
    fn clearing_empties_the_outbox() {
        let mut outbox = Outbox::new();
        revise(&mut outbox, 1, "something");
        outbox.clear();
        assert!(outbox.peek().is_none());
    }
}
