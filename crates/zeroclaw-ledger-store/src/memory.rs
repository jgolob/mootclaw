//! In-memory [`LedgerStore`], for tests and for running the orchestrator without
//! infrastructure.
//!
//! This is a first-class implementation, not a stub: the round driver, the
//! projection path, and the mailbox protocol are all exercised against it, so a
//! full multi-agent conversation can be tested with no database, no container,
//! and no model provider. It is held to the same trait invariants as Postgres
//! and runs the same conformance suite.

use crate::store::{
    ConversationMeta, LedgerStore, ProposalRecord, Result, StoreError, StoredAuditEvent,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;
use uuid::Uuid;
use zeroclaw_ledger::{
    AuditEvent, ConversationId, ConversationKind, DraftId, LedgerEntry, MemberRole, NewEntry,
    OracleVerdict, Participant, ParticipantId, Round, RoundDisposition, Seq,
};

/// One conversation's state.
///
/// `members` and `rounds` use [`BTreeMap`] rather than `HashMap` so iteration is
/// ordered by [`ParticipantId`] by construction. Deterministic membership order
/// is a trait invariant because it feeds projection, and getting it from the
/// data structure is more durable than remembering to sort at every read.
#[derive(Debug)]
struct Conversation {
    kind: ConversationKind,
    round_timeout: Duration,
    /// Sequence of the most recent admitted entry; [`Seq::EMPTY`] when none.
    head: Seq,
    visible_from: Seq,
    members: BTreeMap<ParticipantId, MemberRole>,
    /// Append-only. Tombstoning flips a flag; nothing is ever removed.
    entries: Vec<StoredEntry>,
    rounds: BTreeMap<Round, RoundRecord>,
}

/// A ledger entry with the wall time it landed.
///
/// The timestamp is storage metadata for logging and QC. It is deliberately not
/// part of [`LedgerEntry`], so it cannot reach a projection: ordering is `seq`
/// and causality is `round`, and a clock would only invent a false ordering
/// between contributions that were genuinely concurrent.
#[derive(Debug)]
struct StoredEntry {
    entry: LedgerEntry,
    recorded_at: DateTime<Utc>,
}

#[derive(Debug)]
struct RoundRecord {
    deadline: DateTime<Utc>,
    dispositions: BTreeMap<ParticipantId, RoundDisposition>,
}

#[derive(Debug, Default)]
struct State {
    conversations: HashMap<ConversationId, Conversation>,
    participants: HashMap<ParticipantId, Participant>,
    proposals: HashMap<DraftId, (ProposalRecord, Option<OracleVerdict>)>,
    audit: Vec<StoredAuditEvent>,
}

/// In-memory implementation of [`LedgerStore`].
#[derive(Debug, Default)]
pub struct InMemoryLedgerStore {
    state: Mutex<State>,
}

impl InMemoryLedgerStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Locks the state.
    ///
    /// A poisoned lock is reported as a backend failure rather than unwrapped:
    /// the orchestrator writes conversation state, and a panic there should
    /// surface as an error the caller can log and act on, not take the process
    /// down with it.
    fn state(&self) -> Result<MutexGuard<'_, State>> {
        self.state
            .lock()
            .map_err(|_| StoreError::Backend("in-memory ledger state lock poisoned".to_string()))
    }
}

/// Looks up a conversation, or reports it missing.
fn conversation(state: &mut State, id: ConversationId) -> Result<&mut Conversation> {
    state
        .conversations
        .get_mut(&id)
        .ok_or(StoreError::ConversationNotFound(id))
}

#[async_trait]
impl LedgerStore for InMemoryLedgerStore {
    async fn create_conversation(
        &self,
        kind: ConversationKind,
        round_timeout: Duration,
    ) -> Result<ConversationId> {
        let id = ConversationId(Uuid::new_v4());
        let mut state = self.state()?;
        state.conversations.insert(
            id,
            Conversation {
                kind,
                round_timeout,
                head: Seq::EMPTY,
                visible_from: Seq::EMPTY,
                members: BTreeMap::new(),
                entries: Vec::new(),
                rounds: BTreeMap::new(),
            },
        );
        Ok(id)
    }

    async fn upsert_participant(&self, participant: Participant) -> Result<()> {
        let mut state = self.state()?;
        state.participants.insert(participant.id, participant);
        Ok(())
    }

    async fn add_member(
        &self,
        conversation_id: ConversationId,
        participant: ParticipantId,
        role: MemberRole,
    ) -> Result<()> {
        let mut state = self.state()?;
        if !state.participants.contains_key(&participant) {
            return Err(StoreError::ParticipantNotFound(participant));
        }
        conversation(&mut state, conversation_id)?
            .members
            .insert(participant, role);
        Ok(())
    }

    async fn members(&self, conversation_id: ConversationId) -> Result<Vec<Participant>> {
        let mut state = self.state()?;
        let ids: Vec<ParticipantId> = conversation(&mut state, conversation_id)?
            .members
            .keys()
            .copied()
            .collect();
        // BTreeMap iteration is already ParticipantId-ordered, satisfying the
        // deterministic-membership invariant without a sort.
        ids.into_iter()
            .map(|id| {
                state
                    .participants
                    .get(&id)
                    .cloned()
                    .ok_or(StoreError::ParticipantNotFound(id))
            })
            .collect()
    }

    async fn member_role(
        &self,
        conversation_id: ConversationId,
        participant: ParticipantId,
    ) -> Result<MemberRole> {
        let mut state = self.state()?;
        conversation(&mut state, conversation_id)?
            .members
            .get(&participant)
            .copied()
            .ok_or(StoreError::NotAMember {
                conversation: conversation_id,
                participant,
            })
    }

    async fn head_seq(&self, conversation_id: ConversationId) -> Result<Seq> {
        let mut state = self.state()?;
        Ok(conversation(&mut state, conversation_id)?.head)
    }

    async fn conversation_meta(&self, conversation_id: ConversationId) -> Result<ConversationMeta> {
        let mut state = self.state()?;
        let conversation = conversation(&mut state, conversation_id)?;
        Ok(ConversationMeta {
            kind: conversation.kind,
            round_timeout: conversation.round_timeout,
            head_seq: conversation.head,
            visible_from: conversation.visible_from,
        })
    }

    async fn append_batch(
        &self,
        conversation_id: ConversationId,
        round: Round,
        entries: Vec<NewEntry>,
    ) -> Result<Vec<Seq>> {
        let recorded_at = Utc::now();
        let mut state = self.state()?;
        let conversation = conversation(&mut state, conversation_id)?;

        // The whole batch is assigned under one lock, so concurrent callers can
        // never interleave within a round or reuse a sequence.
        let mut assigned = Vec::with_capacity(entries.len());
        for entry in entries {
            let seq = Seq(conversation.head.0 + 1);
            conversation.head = seq;
            conversation.entries.push(StoredEntry {
                entry: LedgerEntry {
                    conversation_id,
                    seq,
                    round,
                    participant_id: entry.participant_id,
                    content: entry.content,
                    kind: entry.kind,
                    tombstoned: false,
                },
                recorded_at,
            });
            assigned.push(seq);
        }
        Ok(assigned)
    }

    async fn entries_from(
        &self,
        conversation_id: ConversationId,
        from: Seq,
    ) -> Result<Vec<LedgerEntry>> {
        let mut state = self.state()?;
        let conversation = conversation(&mut state, conversation_id)?;
        let floor = conversation.visible_from;
        Ok(conversation
            .entries
            .iter()
            .map(|stored| &stored.entry)
            .filter(|entry| entry.seq > from && entry.seq > floor && !entry.tombstoned)
            .cloned()
            .collect())
    }

    async fn entry_timestamps(
        &self,
        conversation_id: ConversationId,
    ) -> Result<Vec<(Seq, DateTime<Utc>)>> {
        let mut state = self.state()?;
        let conversation = conversation(&mut state, conversation_id)?;
        Ok(conversation
            .entries
            .iter()
            .map(|stored| (stored.entry.seq, stored.recorded_at))
            .collect())
    }

    async fn open_round(
        &self,
        conversation_id: ConversationId,
        round: Round,
        deadline: DateTime<Utc>,
    ) -> Result<()> {
        let mut state = self.state()?;
        let conversation = conversation(&mut state, conversation_id)?;
        // Only contributors are enrolled. Observers receive context but must
        // never hold a round open.
        let dispositions = conversation
            .members
            .iter()
            .filter(|(_, role)| matches!(role, MemberRole::Contributor))
            .map(|(id, _)| (*id, RoundDisposition::Pending))
            .collect();
        conversation.rounds.insert(
            round,
            RoundRecord {
                deadline,
                dispositions,
            },
        );
        Ok(())
    }

    async fn round_state(
        &self,
        conversation_id: ConversationId,
        round: Round,
    ) -> Result<Vec<(ParticipantId, RoundDisposition)>> {
        let mut state = self.state()?;
        let conversation = conversation(&mut state, conversation_id)?;
        let record = conversation
            .rounds
            .get(&round)
            .ok_or(StoreError::RoundNotOpen {
                conversation: conversation_id,
                round,
            })?;
        Ok(record
            .dispositions
            .iter()
            .map(|(id, disposition)| (*id, *disposition))
            .collect())
    }

    async fn round_deadline(
        &self,
        conversation_id: ConversationId,
        round: Round,
    ) -> Result<DateTime<Utc>> {
        let mut state = self.state()?;
        let conversation = conversation(&mut state, conversation_id)?;
        conversation
            .rounds
            .get(&round)
            .map(|record| record.deadline)
            .ok_or(StoreError::RoundNotOpen {
                conversation: conversation_id,
                round,
            })
    }

    async fn record_round_disposition(
        &self,
        conversation_id: ConversationId,
        round: Round,
        participant: ParticipantId,
        disposition: RoundDisposition,
    ) -> Result<()> {
        let mut state = self.state()?;
        let conversation = conversation(&mut state, conversation_id)?;
        if !conversation.members.contains_key(&participant) {
            return Err(StoreError::NotAMember {
                conversation: conversation_id,
                participant,
            });
        }
        let record = conversation
            .rounds
            .get_mut(&round)
            .ok_or(StoreError::RoundNotOpen {
                conversation: conversation_id,
                round,
            })?;
        record.dispositions.insert(participant, disposition);
        Ok(())
    }

    async fn record_proposal(&self, proposal: ProposalRecord) -> Result<()> {
        let mut state = self.state()?;
        state.proposals.insert(proposal.id, (proposal, None));
        Ok(())
    }

    async fn set_proposal_verdict(&self, proposal: DraftId, verdict: OracleVerdict) -> Result<()> {
        let mut state = self.state()?;
        let entry = state
            .proposals
            .get_mut(&proposal)
            .ok_or(StoreError::ProposalNotFound(proposal))?;
        entry.1 = Some(verdict);
        Ok(())
    }

    async fn proposal_verdict(&self, proposal: DraftId) -> Result<Option<OracleVerdict>> {
        let state = self.state()?;
        state
            .proposals
            .get(&proposal)
            .map(|(_, verdict)| verdict.clone())
            .ok_or(StoreError::ProposalNotFound(proposal))
    }

    async fn append_audit(&self, event: AuditEvent) -> Result<()> {
        let occurred_at = Utc::now();
        let mut state = self.state()?;
        state.audit.push(StoredAuditEvent { event, occurred_at });
        Ok(())
    }

    async fn audit_events(&self, conversation_id: ConversationId) -> Result<Vec<StoredAuditEvent>> {
        let state = self.state()?;
        Ok(state
            .audit
            .iter()
            .filter(|stored| stored.event.conversation_id == Some(conversation_id))
            .cloned()
            .collect())
    }

    async fn tombstone_from(&self, conversation_id: ConversationId, from: Seq) -> Result<u64> {
        let mut state = self.state()?;
        let conversation = conversation(&mut state, conversation_id)?;
        let mut affected = 0u64;
        for stored in conversation.entries.iter_mut() {
            if stored.entry.seq >= from && !stored.entry.tombstoned {
                stored.entry.tombstoned = true;
                affected += 1;
            }
        }
        Ok(affected)
    }

    async fn set_visible_from(&self, conversation_id: ConversationId, seq: Seq) -> Result<()> {
        let mut state = self.state()?;
        let conversation = conversation(&mut state, conversation_id)?;
        // Monotonic: containment must not be reversible by a later caller.
        if seq > conversation.visible_from {
            conversation.visible_from = seq;
        }
        Ok(())
    }
}
