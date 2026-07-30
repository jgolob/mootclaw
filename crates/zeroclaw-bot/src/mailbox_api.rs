//! The HTTP surface a participant exposes to the orchestrator.
//!
//! Three endpoints, all orchestrator-initiated:
//!
//! - `PUT /inbox` — accept an envelope and begin thinking. Returns `202` at once.
//! - `GET /outbox/{conversation_id}` — hand back the current draft, if any.
//! - `POST /disposition` — learn what became of a retrieved draft.
//!
//! The orchestrator drives all three, so a participant needs neither the
//! orchestrator's address nor credentials to it. A participant that is down is
//! simply a participant that does not respond, which the round barrier already
//! handles as a timeout.
//!
//! # Why `PUT /inbox` returns before thinking finishes
//!
//! Composing a contribution can take minutes — a model call, tool use, running
//! code. Holding the request open for that would make one slow participant slow
//! the orchestrator's whole round loop, and would turn an HTTP timeout into a
//! lost contribution. So the envelope is accepted, work starts in the background,
//! and the draft appears in the outbox when it is ready. The deadline in the
//! envelope, not the HTTP request, is what bounds the work.

use crate::compose::Composer;
use crate::draft::Outbox;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::Utc;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use zeroclaw_ledger::mailbox::{DispositionNotice, InboxEnvelope, OutboxResponse};
use zeroclaw_ledger::types::{ConversationId, Disposition, ParticipantId, Round};

/// What this participant knows about one conversation.
///
/// Working state only. The conversation itself lives in the ledger, and every
/// envelope reconstructs the full context — so losing this costs nothing but
/// in-progress thinking.
#[derive(Debug, Default)]
struct ConversationState {
    outbox: Outbox,
    /// The round of the most recent envelope, so a late-arriving stale envelope
    /// cannot pull the participant backwards.
    current_round: Option<Round>,
    /// What the orchestrator last said about one of this participant's drafts.
    ///
    /// Kept for observability — it is the answer to "why did my contribution not
    /// appear?" when inspecting a running participant. Not authoritative: the
    /// next envelope carries the dispositions that actually inform composition.
    last_disposition: Option<Disposition>,
}

#[derive(Debug)]
struct BotStateInner {
    conversations: HashMap<ConversationId, ConversationState>,
}

/// A participant's in-memory state.
#[derive(Debug)]
pub struct BotState {
    identity: ParticipantId,
    inner: Mutex<BotStateInner>,
}

impl BotState {
    pub fn new(identity: ParticipantId) -> Self {
        Self {
            identity,
            inner: Mutex::new(BotStateInner {
                conversations: HashMap::new(),
            }),
        }
    }

    pub fn identity(&self) -> ParticipantId {
        self.identity
    }

    /// Locks the state.
    ///
    /// A poisoned lock becomes a `503` rather than a panic: one conversation
    /// going wrong should not take the participant off the network for every
    /// other conversation it is in.
    fn lock(&self) -> Result<MutexGuard<'_, BotStateInner>, StatusCode> {
        self.inner
            .lock()
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)
    }

    /// The current draft for a conversation, whatever its state.
    pub fn outbox(&self, conversation: ConversationId) -> Result<OutboxResponse, StatusCode> {
        let state = self.lock()?;
        Ok(OutboxResponse {
            draft: state
                .conversations
                .get(&conversation)
                .and_then(|conversation| conversation.outbox.peek())
                .map(|slot| slot.draft().clone()),
        })
    }

    /// The most recent disposition reported for a conversation, if any.
    pub fn last_disposition(
        &self,
        conversation: ConversationId,
    ) -> Result<Option<Disposition>, StatusCode> {
        let state = self.lock()?;
        Ok(state
            .conversations
            .get(&conversation)
            .and_then(|conversation| conversation.last_disposition.clone()))
    }

    /// Records what the orchestrator reported about a draft.
    fn record_disposition(&self, notice: &DispositionNotice) -> Result<(), StatusCode> {
        let mut state = self.lock()?;
        state
            .conversations
            .entry(notice.conversation_id)
            .or_default()
            .last_disposition = Some(notice.disposition.clone());
        Ok(())
    }

    /// Records an envelope and prepares the outbox for its round.
    ///
    /// Returns `false` when the envelope is for a round already superseded, in
    /// which case it is ignored: a delayed delivery must not drag the participant
    /// back to a question the conversation has moved past.
    fn accept_envelope(&self, envelope: &InboxEnvelope) -> Result<bool, StatusCode> {
        let mut state = self.lock()?;
        let conversation = state
            .conversations
            .entry(envelope.conversation_id)
            .or_default();

        if conversation
            .current_round
            .is_some_and(|current| envelope.round < current)
        {
            return Ok(false);
        }

        conversation.current_round = Some(envelope.round);
        conversation.outbox.open_round(envelope.round);
        Ok(true)
    }

    /// Writes the composer's answer into the outbox.
    fn record_composition(
        &self,
        envelope: &InboxEnvelope,
        outcome: crate::compose::Composition,
    ) -> Result<(), StatusCode> {
        use crate::compose::Composition;

        let now = Utc::now();
        let mut state = self.lock()?;
        let conversation = state
            .conversations
            .entry(envelope.conversation_id)
            .or_default();

        // The round may have moved on while we were thinking. Discard rather than
        // seal an answer to a superseded question.
        if conversation
            .current_round
            .is_some_and(|current| current != envelope.round)
        {
            return Ok(());
        }

        match outcome {
            Composition::Contribute(content) => {
                conversation.outbox.revise(
                    envelope.conversation_id,
                    self.identity,
                    envelope.round,
                    envelope.head_seq,
                    content,
                    now,
                );
                conversation.outbox.seal(now);
            }
            Composition::Pass => {
                conversation.outbox.pass(
                    envelope.conversation_id,
                    self.identity,
                    envelope.round,
                    envelope.head_seq,
                    now,
                );
            }
        }
        Ok(())
    }

    /// Composes an answer to `envelope` and records it.
    ///
    /// Exposed so tests can drive a full think-and-seal cycle without racing the
    /// background task the handler spawns.
    pub async fn think(
        &self,
        envelope: InboxEnvelope,
        composer: &dyn Composer,
    ) -> Result<(), StatusCode> {
        if !self.accept_envelope(&envelope)? {
            return Ok(());
        }
        let outcome = composer.compose(&envelope).await;
        self.record_composition(&envelope, outcome)
    }
}

/// Everything the handlers need.
#[derive(Clone)]
pub struct BotContext {
    pub state: Arc<BotState>,
    pub composer: Arc<dyn Composer>,
}

/// The participant's mailbox router.
pub fn router(context: BotContext) -> Router {
    Router::new()
        .route("/inbox", put(put_inbox))
        .route("/outbox/{conversation_id}", get(get_outbox))
        .route("/disposition", post(post_disposition))
        .with_state(context)
}

/// `PUT /inbox` — accept an envelope; think in the background.
async fn put_inbox(
    State(context): State<BotContext>,
    Json(envelope): Json<InboxEnvelope>,
) -> Result<StatusCode, StatusCode> {
    if !context.state.accept_envelope(&envelope)? {
        // Already past this round. Accepting is still the honest answer: the
        // delivery succeeded, there is simply nothing left to do about it.
        return Ok(StatusCode::ACCEPTED);
    }

    let state = Arc::clone(&context.state);
    let composer = Arc::clone(&context.composer);
    zeroclaw_spawn::spawn!(async move {
        let outcome = composer.compose(&envelope).await;
        // A failure to record is logged by the caller's span rather than
        // propagated: there is no one to return it to, and the round barrier
        // already treats a missing draft as a timeout.
        let _ = state.record_composition(&envelope, outcome);
    });

    Ok(StatusCode::ACCEPTED)
}

/// `GET /outbox/{conversation_id}` — the current draft, if any.
async fn get_outbox(
    State(context): State<BotContext>,
    Path(conversation_id): Path<ConversationId>,
) -> Result<Json<OutboxResponse>, StatusCode> {
    context.state.outbox(conversation_id).map(Json)
}

/// `POST /disposition` — what became of a retrieved draft.
async fn post_disposition(
    State(context): State<BotContext>,
    Json(notice): Json<DispositionNotice>,
) -> Result<StatusCode, StatusCode> {
    // What actually informs the next composition is the disposition list carried
    // in the next envelope; this copy exists so a running participant can be
    // asked why its contribution did not appear.
    context.state.record_disposition(&notice)?;
    Ok(StatusCode::NO_CONTENT)
}
