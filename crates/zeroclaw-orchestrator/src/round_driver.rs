//! Driving one conversation, one round at a time.
//!
//! Each round:
//!
//! 1. Open at the current ledger head; enrol every contributor as pending.
//! 2. Project the conversation for each member and deliver it.
//! 3. Poll outboxes until every member has sealed or passed, or the window
//!    expires.
//! 4. Claim only *sealed* drafts, currency-check them, judge the survivors.
//! 5. Append the admitted contributions as one batch and report dispositions.
//!
//! The orchestrator is the sole writer. Participants propose; nothing they send
//! reaches the ledger except through [`LedgerStore::append_batch`], and every
//! operational event — a timeout, an unreachable container, a rejection — goes to
//! the audit log where no projection can ever read it.

use crate::bot_client::{BotClient, BotEndpoint};
use crate::human_desk::{HumanDesk, HumanResponse};
use crate::oracle::Oracle;
use chrono::{DateTime, TimeDelta, Utc};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::time::Duration;
use zeroclaw_ledger::mailbox::{BudgetStanding, DispositionNotice, InboxEnvelope, OutboxResponse};
use zeroclaw_ledger::projection::{ParticipantIndex, project_for_reader};
use zeroclaw_ledger::round::{
    RoundMachine, RoundStatus, admitted_dispositions, finalize_admission, plan_admission,
};
use zeroclaw_ledger::types::{
    AuditEvent, AuditKind, ConversationId, Disposition, Draft, DraftState, MemberRole,
    ParticipantId, ParticipantKind, Round, RoundDisposition, Seq,
};
use zeroclaw_ledger_store::store::{LedgerStore, ProposalRecord, StoreError};

/// How often outboxes are polled while a round is open.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Everything the driver needs that is not the store.
pub struct RoundDriver<'a, S: LedgerStore + ?Sized, C: BotClient + ?Sized, O: Oracle + ?Sized> {
    pub store: &'a S,
    pub bots: &'a C,
    pub oracle: &'a O,
    /// Where each participant's mailbox lives. Humans have no endpoint and are
    /// simply absent.
    pub endpoints: BTreeMap<ParticipantId, BotEndpoint>,
    /// Where human contributions wait. A human is an ordinary member — enrolled,
    /// able to pass, and timed out like anyone else if they say nothing.
    pub humans: &'a HumanDesk,
    pub stale_window: i64,
    pub poll_interval: Duration,
}

/// What one round did.
///
/// Deliberately not `Default`: there is no such thing as a round zero, and a
/// zero-valued report would be a lie about a round that never ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoundReport {
    pub round: Round,
    /// Sequences assigned to admitted contributions, in ledger order.
    pub admitted: Vec<Seq>,
    /// Set when the round was discarded for contamination.
    pub contamination: Option<Seq>,
    pub head_after: Seq,
}

#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("projection failed: {0}")]
    Projection(#[from] zeroclaw_ledger::projection::ProjectionError),
}

impl<S, C, O> RoundDriver<'_, S, C, O>
where
    S: LedgerStore + ?Sized,
    C: BotClient + ?Sized,
    O: Oracle + ?Sized,
{
    /// Runs one complete round.
    pub async fn run_round(
        &self,
        conversation: ConversationId,
        round: Round,
    ) -> Result<RoundReport, DriverError> {
        let meta = self.store.conversation_meta(conversation).await?;
        let head = meta.head_seq;
        let deadline =
            Utc::now() + TimeDelta::from_std(meta.round_timeout).unwrap_or(TimeDelta::seconds(120));

        let members = self.store.members(conversation).await?;
        let mut contributors = Vec::new();
        // People are never refused: not for staleness, not by the oracle.
        let mut humans: BTreeSet<ParticipantId> = BTreeSet::new();
        for member in &members {
            if matches!(member.kind, ParticipantKind::Human) {
                humans.insert(member.id);
            }
            if matches!(
                self.store.member_role(conversation, member.id).await?,
                MemberRole::Contributor
            ) {
                contributors.push(member.id);
            }
        }

        self.store.open_round(conversation, round, deadline).await?;
        self.store
            .append_audit(AuditEvent {
                conversation_id: Some(conversation),
                participant_id: None,
                round: Some(round),
                kind: AuditKind::RoundOpened,
                detail: format!("head {head:?}, {} contributors", contributors.len()),
            })
            .await?;

        let mut machine = RoundMachine::open(round, &contributors, deadline);
        let index: ParticipantIndex = members.iter().cloned().collect();
        let entries = self.store.entries_from(conversation, Seq::EMPTY).await?;

        // Deliver context to every contributor with a mailbox.
        for participant in &contributors {
            let Some(endpoint) = self.endpoints.get(participant) else {
                // A human contributor: contributes through the view API, not a
                // mailbox. Nothing to deliver.
                continue;
            };
            let context =
                project_for_reader(&entries, &index, *participant, None, meta.visible_from)?;
            let envelope = InboxEnvelope {
                conversation_id: conversation,
                round,
                head_seq: head,
                deadline,
                context,
                scratchpad: None,
                soul: None,
                budget: BudgetStanding::fresh(),
                dispositions: Vec::new(),
            };
            if let Err(error) = self.bots.deliver_inbox(endpoint, &envelope).await {
                // A participant that cannot be reached has not responded. That is
                // the barrier's business, not an error — and the reason belongs
                // in the audit log, never in anyone's context.
                self.store
                    .append_audit(error.to_audit(conversation, *participant, round))
                    .await?;
            }
        }

        let sealed = self
            .collect_responses(conversation, round, &mut machine)
            .await?;
        self.close_round(conversation, round, &machine).await?;

        self.admit(conversation, round, head, sealed, &humans).await
    }

    /// Polls outboxes until the round closes, returning the sealed drafts.
    async fn collect_responses(
        &self,
        conversation: ConversationId,
        round: Round,
        machine: &mut RoundMachine,
    ) -> Result<Vec<Draft>, DriverError> {
        let mut sealed: BTreeMap<ParticipantId, Draft> = BTreeMap::new();

        loop {
            let now = Utc::now();
            if matches!(machine.status(now), RoundStatus::Closed { .. }) {
                break;
            }

            let RoundStatus::Open { pending } = machine.status(now) else {
                break;
            };

            for participant in pending {
                let Some(endpoint) = self.endpoints.get(&participant) else {
                    // No mailbox: a person. Their words, if any, are already in
                    // the ledger — the desk only says whether they have
                    // responded, so the round need not keep waiting on them.
                    match self.humans.peek(conversation, participant) {
                        Some(HumanResponse::Contributed) => {
                            if machine.record_seal(participant, Utc::now()).is_ok() {
                                self.humans.take(conversation, participant);
                            }
                        }
                        Some(HumanResponse::Passed)
                            if machine.record_pass(participant, Utc::now()).is_ok() =>
                        {
                            self.humans.take(conversation, participant);
                        }
                        _ => {}
                    }
                    continue;
                };
                let response = match self.bots.fetch_outbox(endpoint, conversation).await {
                    Ok(response) => response,
                    Err(error) => {
                        self.store
                            .append_audit(error.to_audit(conversation, participant, round))
                            .await?;
                        OutboxResponse { draft: None }
                    }
                };

                let Some(draft) = response.draft else {
                    continue;
                };

                // Only sealed drafts are claimed. An unsealed one is a thought in
                // progress, and retrieving it would record something the
                // participant never committed to.
                match draft.state {
                    DraftState::Sealed => {
                        if machine.record_seal(participant, Utc::now()).is_ok() {
                            sealed.insert(participant, draft);
                        }
                    }
                    DraftState::Pass => {
                        let _ = machine.record_pass(participant, Utc::now());
                    }
                    DraftState::Draft => {}
                }
            }

            if matches!(machine.status(Utc::now()), RoundStatus::Closed { .. }) {
                break;
            }
            tokio::time::sleep(self.poll_interval).await;
        }

        Ok(sealed.into_values().collect())
    }

    /// Records how the round ended.
    async fn close_round(
        &self,
        conversation: ConversationId,
        round: Round,
        machine: &RoundMachine,
    ) -> Result<(), DriverError> {
        let now = Utc::now();
        for (participant, disposition) in machine.final_dispositions(now) {
            self.store
                .record_round_disposition(conversation, round, participant, disposition)
                .await?;
            if matches!(disposition, RoundDisposition::TimedOut) {
                self.store
                    .append_audit(AuditEvent {
                        conversation_id: Some(conversation),
                        participant_id: Some(participant),
                        round: Some(round),
                        kind: AuditKind::ParticipantTimedOut,
                        detail: "no response before the round deadline".to_string(),
                    })
                    .await?;
            }
        }

        let reason = match machine.status(now) {
            RoundStatus::Closed { reason } => format!("{reason:?}"),
            RoundStatus::Open { .. } => "open".to_string(),
        };
        self.store
            .append_audit(AuditEvent {
                conversation_id: Some(conversation),
                participant_id: None,
                round: Some(round),
                kind: AuditKind::RoundClosed,
                detail: reason,
            })
            .await?;
        Ok(())
    }

    /// Judges the sealed drafts and appends what survives.
    async fn admit(
        &self,
        conversation: ConversationId,
        round: Round,
        head: Seq,
        sealed: Vec<Draft>,
        humans: &BTreeSet<ParticipantId>,
    ) -> Result<RoundReport, DriverError> {
        for draft in &sealed {
            self.store
                .record_proposal(ProposalRecord {
                    id: draft.id,
                    conversation_id: conversation,
                    round,
                    participant_id: draft.participant_id,
                    content: draft.content.clone(),
                    composed_against_seq: draft.composed_against_seq,
                    version: draft.version,
                })
                .await?;
        }

        let plan = plan_admission(sealed, head, self.stale_window, conversation, round, humans);

        // Only current drafts reach the oracle — stale ones never enter
        // `to_judge`, so there is no path that spends a judgement on one.
        let mut judged = Vec::with_capacity(plan.to_judge.len());
        for draft in plan.to_judge.clone() {
            let verdict = self.oracle.judge(&draft).await;
            self.store
                .set_proposal_verdict(draft.id, verdict.clone())
                .await?;
            judged.push((draft, verdict));
        }

        let outcome = finalize_admission(
            plan.refused,
            judged,
            plan.exempt.clone(),
            conversation,
            round,
        );

        for event in &outcome.audit {
            self.store.append_audit(event.clone()).await?;
        }

        let mut dispositions = outcome.dispositions.clone();
        let mut admitted = Vec::new();

        if outcome.contamination.is_none() && !outcome.to_append.is_empty() {
            let sequences = self
                .store
                .append_batch(conversation, round, outcome.to_append.clone())
                .await?;
            dispositions.extend(admitted_dispositions(&outcome.to_append, &sequences));
            admitted = sequences;
        }

        self.report_dispositions(conversation, round, &dispositions)
            .await?;

        Ok(RoundReport {
            round,
            admitted,
            contamination: outcome.contamination,
            head_after: self.store.head_seq(conversation).await?,
        })
    }

    /// Tells each participant what became of its draft.
    async fn report_dispositions(
        &self,
        conversation: ConversationId,
        round: Round,
        dispositions: &[(ParticipantId, Disposition)],
    ) -> Result<(), DriverError> {
        for (participant, disposition) in dispositions {
            let Some(endpoint) = self.endpoints.get(participant) else {
                continue;
            };
            let notice = DispositionNotice {
                conversation_id: conversation,
                round,
                disposition: disposition.clone(),
            };
            if let Err(error) = self.bots.notify_disposition(endpoint, &notice).await {
                self.store
                    .append_audit(error.to_audit(conversation, *participant, round))
                    .await?;
            }
        }
        Ok(())
    }
}

/// Convenience: the wall-clock deadline `timeout` from `now`.
pub fn deadline_from(now: DateTime<Utc>, timeout: Duration) -> DateTime<Utc> {
    now + TimeDelta::from_std(timeout).unwrap_or(TimeDelta::seconds(120))
}
