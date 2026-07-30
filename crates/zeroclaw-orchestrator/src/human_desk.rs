//! Where human contributions wait to be collected.
//!
//! An agent participant runs a mailbox the orchestrator polls. A human does not:
//! they type into a client, which posts to the view API. The desk is the
//! equivalent of an outbox for them — somewhere a sealed contribution sits until
//! the round driver claims it.
//!
//! Humans are otherwise ordinary members. They are enrolled in rounds, they can
//! pass, and if they say nothing before the deadline they time out like anyone
//! else. A conversation should not stall because someone stepped away, and a
//! person should not have to be present for the panel to think.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use zeroclaw_ledger::types::{
    ConversationId, Draft, DraftId, DraftState, DraftVersion, ParticipantId, Seq,
};

/// Sealed human contributions awaiting collection.
#[derive(Debug, Default)]
pub struct HumanDesk {
    pending: Mutex<HashMap<(ConversationId, ParticipantId), Draft>>,
}

impl HumanDesk {
    pub fn new() -> Self {
        Self::default()
    }

    fn pending(&self) -> Option<MutexGuard<'_, HashMap<(ConversationId, ParticipantId), Draft>>> {
        // A poisoned lock costs this round's human contributions, not the
        // conversation: the round barrier treats a missing draft as silence.
        self.pending.lock().ok()
    }

    /// Leaves a sealed contribution for collection.
    ///
    /// Replaces anything already waiting: a person who types twice before the
    /// round closes meant the second one.
    pub fn seal(
        &self,
        conversation: ConversationId,
        participant: ParticipantId,
        composed_against_seq: Seq,
        content: impl Into<String>,
    ) -> Option<Draft> {
        let draft = Draft {
            id: DraftId(uuid::Uuid::new_v4()),
            conversation_id: conversation,
            participant_id: participant,
            content: content.into(),
            composed_against_seq,
            version: DraftVersion(1),
            state: DraftState::Sealed,
        };
        let mut pending = self.pending()?;
        pending.insert((conversation, participant), draft.clone());
        Some(draft)
    }

    /// Records an explicit pass, so the round can close without waiting out the
    /// clock.
    pub fn pass(
        &self,
        conversation: ConversationId,
        participant: ParticipantId,
        composed_against_seq: Seq,
    ) -> Option<Draft> {
        let draft = Draft {
            id: DraftId(uuid::Uuid::new_v4()),
            conversation_id: conversation,
            participant_id: participant,
            content: String::new(),
            composed_against_seq,
            version: DraftVersion(1),
            state: DraftState::Pass,
        };
        let mut pending = self.pending()?;
        pending.insert((conversation, participant), draft.clone());
        Some(draft)
    }

    /// What is waiting, without claiming it.
    pub fn peek(&self, conversation: ConversationId, participant: ParticipantId) -> Option<Draft> {
        self.pending()?.get(&(conversation, participant)).cloned()
    }

    /// Claims what is waiting, if anything.
    pub fn take(&self, conversation: ConversationId, participant: ParticipantId) -> Option<Draft> {
        self.pending()?.remove(&(conversation, participant))
    }

    // Deliberately no `clear` for a new round. A contribution left here is not
    // stale work to discard — it is something a person typed, and it waits until
    // it is claimed. Clearing at round open destroyed anything posted in the
    // moments before a round started, which is exactly when someone watching is
    // most likely to type.
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn conversation() -> ConversationId {
        ConversationId(Uuid::from_u128(0xC0))
    }

    fn person() -> ParticipantId {
        ParticipantId(Uuid::from_u128(1))
    }

    #[test]
    fn a_sealed_contribution_waits_to_be_claimed() {
        let desk = HumanDesk::new();
        desk.seal(conversation(), person(), Seq(4), "a considered point")
            .expect("seal");

        let peeked = desk.peek(conversation(), person()).expect("still waiting");
        assert_eq!(peeked.state, DraftState::Sealed);
        assert_eq!(peeked.content, "a considered point");
        assert_eq!(peeked.composed_against_seq, Seq(4));

        let taken = desk.take(conversation(), person()).expect("claimed");
        assert_eq!(taken.content, "a considered point");
        assert!(
            desk.take(conversation(), person()).is_none(),
            "claiming consumes it"
        );
    }

    #[test]
    fn typing_twice_before_the_round_closes_keeps_the_second() {
        let desk = HumanDesk::new();
        desk.seal(conversation(), person(), Seq(4), "first thought")
            .expect("seal");
        desk.seal(conversation(), person(), Seq(4), "better thought")
            .expect("reseal");
        assert_eq!(
            desk.take(conversation(), person())
                .expect("claimed")
                .content,
            "better thought"
        );
    }

    #[test]
    fn a_pass_is_recorded_as_a_real_response() {
        let desk = HumanDesk::new();
        desk.pass(conversation(), person(), Seq(4)).expect("pass");
        let taken = desk.take(conversation(), person()).expect("claimed");
        assert_eq!(taken.state, DraftState::Pass);
        assert!(taken.content.is_empty());
    }

    #[test]
    fn nothing_waits_by_default() {
        // Silence is the normal state of a person who is not looking at the
        // screen, and must not be mistaken for a contribution.
        assert!(HumanDesk::new().take(conversation(), person()).is_none());
    }

    #[test]
    fn a_contribution_waits_across_rounds_until_it_is_claimed() {
        // Someone who types just as a round is closing must not lose their words
        // because the next round happened to open first.
        let desk = HumanDesk::new();
        desk.seal(
            conversation(),
            person(),
            Seq(4),
            "typed at an awkward moment",
        )
        .expect("seal");
        assert_eq!(
            desk.take(conversation(), person())
                .expect("still there")
                .content,
            "typed at an awkward moment"
        );
    }
}
