//! How a person's response reaches the round.
//!
//! A person's *contribution* does not wait here — it is appended to the ledger
//! the moment they send it. What waits here is only the fact that they have
//! responded, so the round barrier knows not to hold the conversation open for
//! someone who has already spoken.
//!
//! That split is the point. An agent proposes and is judged; a person writes to
//! the ledger. Making a person's words queue behind a round would mean the record
//! of a conversation lagged the conversation itself — the wrong way round for the
//! participant the whole thing exists to serve.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use zeroclaw_ledger::types::{ConversationId, ParticipantId};

/// How a person answered this round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HumanResponse {
    /// They said something. It is already in the ledger.
    Contributed,
    /// They had nothing to add. Recorded so the round can close promptly rather
    /// than waiting out the clock.
    Passed,
}

/// Which people have responded, and how.
#[derive(Debug, Default)]
pub struct HumanDesk {
    responses: Mutex<HashMap<(ConversationId, ParticipantId), HumanResponse>>,
}

impl HumanDesk {
    pub fn new() -> Self {
        Self::default()
    }

    fn responses(
        &self,
    ) -> Option<MutexGuard<'_, HashMap<(ConversationId, ParticipantId), HumanResponse>>> {
        // A poisoned lock costs this round's bookkeeping, not the conversation:
        // the barrier treats an unrecorded member as silent and times them out.
        self.responses.lock().ok()
    }

    /// Records that a person has contributed. Their words are already in the
    /// ledger; this only releases the round from waiting on them.
    pub fn contributed(&self, conversation: ConversationId, participant: ParticipantId) {
        if let Some(mut responses) = self.responses() {
            responses.insert((conversation, participant), HumanResponse::Contributed);
        }
    }

    /// Records that a person has nothing to add this round.
    pub fn passed(&self, conversation: ConversationId, participant: ParticipantId) {
        if let Some(mut responses) = self.responses() {
            responses.insert((conversation, participant), HumanResponse::Passed);
        }
    }

    /// What a person has said this round, without consuming it.
    pub fn peek(
        &self,
        conversation: ConversationId,
        participant: ParticipantId,
    ) -> Option<HumanResponse> {
        self.responses()?.get(&(conversation, participant)).copied()
    }

    /// Consumes a person's response, once the round has accounted for it.
    pub fn take(
        &self,
        conversation: ConversationId,
        participant: ParticipantId,
    ) -> Option<HumanResponse> {
        self.responses()?.remove(&(conversation, participant))
    }
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
    fn a_contribution_is_recorded_as_a_response() {
        let desk = HumanDesk::new();
        desk.contributed(conversation(), person());
        assert_eq!(
            desk.peek(conversation(), person()),
            Some(HumanResponse::Contributed)
        );
        assert_eq!(
            desk.take(conversation(), person()),
            Some(HumanResponse::Contributed)
        );
        assert!(
            desk.take(conversation(), person()).is_none(),
            "taking consumes it"
        );
    }

    #[test]
    fn a_pass_is_distinct_from_a_contribution() {
        // Both release the round, but only one means the person spoke — and
        // someone watching the round pane should be able to tell which.
        let desk = HumanDesk::new();
        desk.passed(conversation(), person());
        assert_eq!(
            desk.peek(conversation(), person()),
            Some(HumanResponse::Passed)
        );
    }

    #[test]
    fn speaking_after_passing_supersedes_the_pass() {
        let desk = HumanDesk::new();
        desk.passed(conversation(), person());
        desk.contributed(conversation(), person());
        assert_eq!(
            desk.peek(conversation(), person()),
            Some(HumanResponse::Contributed),
            "a person who changes their mind has spoken"
        );
    }

    #[test]
    fn silence_is_the_default() {
        // Someone not looking at the screen has not responded, and must not be
        // mistaken for someone who chose to pass.
        assert!(HumanDesk::new().peek(conversation(), person()).is_none());
    }
}
