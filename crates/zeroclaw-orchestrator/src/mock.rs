//! An in-process participant, for testing the orchestrator without a network.
//!
//! Real participants are containers behind HTTP. This one is a `Mutex` and a
//! rule, which lets the whole round loop — envelopes, sealing, passing,
//! admission, timeouts, unreachability — be tested deterministically and in
//! milliseconds.

use crate::bot_client::{BotCallError, BotClient, BotEndpoint, BotResult};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use zeroclaw_ledger::mailbox::{DispositionNotice, InboxEnvelope, OutboxResponse};
use zeroclaw_ledger::types::{
    ConversationId, Disposition, Draft, DraftId, DraftState, DraftVersion, ParticipantId,
};

/// How a mock participant answers a round.
#[derive(Debug, Clone)]
pub enum MockBehaviour {
    /// Seals this text every round.
    Contribute(String),
    /// Explicitly has nothing to add.
    Pass,
    /// Accepts envelopes but never produces a draft — a participant that is
    /// thinking forever, which the round barrier must resolve by timing out.
    Silent,
    /// Fails every call, as a stopped container would.
    Unreachable,
    /// Seals a draft claiming to be composed against a fixed, outdated sequence.
    Stale {
        content: String,
        composed_against_seq: zeroclaw_ledger::types::Seq,
    },
}

#[derive(Debug, Default)]
struct MockRecord {
    envelopes: Vec<InboxEnvelope>,
    dispositions: Vec<DispositionNotice>,
    draft: Option<Draft>,
}

/// A set of in-process participants addressed the same way real ones are.
#[derive(Debug, Default)]
pub struct MockBotClient {
    behaviours: Mutex<HashMap<ParticipantId, MockBehaviour>>,
    records: Mutex<HashMap<ParticipantId, MockRecord>>,
}

impl MockBotClient {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers how `participant` will answer.
    pub fn with(self, participant: ParticipantId, behaviour: MockBehaviour) -> Self {
        if let Ok(mut behaviours) = self.behaviours.lock() {
            behaviours.insert(participant, behaviour);
        }
        self
    }

    fn behaviours(&self) -> BotResult<MutexGuard<'_, HashMap<ParticipantId, MockBehaviour>>> {
        self.behaviours
            .lock()
            .map_err(|_| BotCallError::Unreachable("mock behaviour lock poisoned".to_string()))
    }

    fn records(&self) -> BotResult<MutexGuard<'_, HashMap<ParticipantId, MockRecord>>> {
        self.records
            .lock()
            .map_err(|_| BotCallError::Unreachable("mock record lock poisoned".to_string()))
    }

    /// Envelopes this participant received, in order.
    pub fn envelopes_for(&self, participant: ParticipantId) -> Vec<InboxEnvelope> {
        self.records()
            .ok()
            .and_then(|records| records.get(&participant).map(|r| r.envelopes.clone()))
            .unwrap_or_default()
    }

    /// Dispositions this participant was told about, in order.
    pub fn dispositions_for(&self, participant: ParticipantId) -> Vec<Disposition> {
        self.records()
            .ok()
            .and_then(|records| {
                records.get(&participant).map(|record| {
                    record
                        .dispositions
                        .iter()
                        .map(|notice| notice.disposition.clone())
                        .collect()
                })
            })
            .unwrap_or_default()
    }
}

#[async_trait]
impl BotClient for MockBotClient {
    async fn deliver_inbox(
        &self,
        endpoint: &BotEndpoint,
        envelope: &InboxEnvelope,
    ) -> BotResult<()> {
        let behaviour = self
            .behaviours()?
            .get(&endpoint.participant)
            .cloned()
            .unwrap_or(MockBehaviour::Pass);

        if matches!(behaviour, MockBehaviour::Unreachable) {
            return Err(BotCallError::Unreachable("mock is down".to_string()));
        }

        let mut records = self.records()?;
        let record = records.entry(endpoint.participant).or_default();
        record.envelopes.push(envelope.clone());

        // Composing is instantaneous here, so the draft is ready by the time the
        // orchestrator first polls. Real participants take longer; the round
        // barrier is what accommodates that, not this.
        record.draft = match behaviour {
            MockBehaviour::Contribute(content) => Some(Draft {
                id: DraftId(uuid::Uuid::new_v4()),
                conversation_id: envelope.conversation_id,
                participant_id: endpoint.participant,
                content,
                composed_against_seq: envelope.head_seq,
                version: DraftVersion(1),
                state: DraftState::Sealed,
            }),
            MockBehaviour::Stale {
                content,
                composed_against_seq,
            } => Some(Draft {
                id: DraftId(uuid::Uuid::new_v4()),
                conversation_id: envelope.conversation_id,
                participant_id: endpoint.participant,
                content,
                composed_against_seq,
                version: DraftVersion(1),
                state: DraftState::Sealed,
            }),
            MockBehaviour::Pass => Some(Draft {
                id: DraftId(uuid::Uuid::new_v4()),
                conversation_id: envelope.conversation_id,
                participant_id: endpoint.participant,
                content: String::new(),
                composed_against_seq: envelope.head_seq,
                version: DraftVersion(1),
                state: DraftState::Pass,
            }),
            MockBehaviour::Silent => None,
            MockBehaviour::Unreachable => None,
        };
        Ok(())
    }

    async fn fetch_outbox(
        &self,
        endpoint: &BotEndpoint,
        _conversation: ConversationId,
    ) -> BotResult<OutboxResponse> {
        let behaviour = self
            .behaviours()?
            .get(&endpoint.participant)
            .cloned()
            .unwrap_or(MockBehaviour::Pass);
        if matches!(behaviour, MockBehaviour::Unreachable) {
            return Err(BotCallError::Unreachable("mock is down".to_string()));
        }

        Ok(OutboxResponse {
            draft: self
                .records()?
                .get(&endpoint.participant)
                .and_then(|record| record.draft.clone()),
        })
    }

    async fn notify_disposition(
        &self,
        endpoint: &BotEndpoint,
        notice: &DispositionNotice,
    ) -> BotResult<()> {
        let behaviour = self
            .behaviours()?
            .get(&endpoint.participant)
            .cloned()
            .unwrap_or(MockBehaviour::Pass);
        if matches!(behaviour, MockBehaviour::Unreachable) {
            return Err(BotCallError::Unreachable("mock is down".to_string()));
        }
        self.records()?
            .entry(endpoint.participant)
            .or_default()
            .dispositions
            .push(notice.clone());
        Ok(())
    }
}
