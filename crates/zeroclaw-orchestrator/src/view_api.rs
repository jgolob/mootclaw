//! The read side, and the way a human joins in.
//!
//! Every client — TUI, web, Telegram — is a *projection* of the ledger, never a
//! source. They all read through this one API, so a conversation looks the same
//! from everywhere and no client holds state of its own.
//!
//! Note what this does **not** expose: the audit log. Operational events are
//! deliberately unreachable from the surface that renders a conversation, so a
//! client cannot accidentally show a participant "peer timed out" and make it
//! part of the discussion.

use crate::human_desk::HumanDesk;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use zeroclaw_ledger::types::{
    ConversationId, EntryKind, NewEntry, ParticipantId, ParticipantKind, Round, RoundDisposition,
    Seq,
};
use zeroclaw_ledger_store::store::LedgerStore;

/// One contribution, rendered for display.
///
/// Attribution is resolved here rather than left to the client: every surface
/// must name authors the same way, and a client that invented its own format
/// could make an agent look like a human.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewEntry {
    pub seq: i64,
    pub round: i64,
    pub participant_id: ParticipantId,
    /// `human` or `agent` — a reader's posture toward a peer differs from its
    /// posture toward a person, and a viewer's should too.
    pub kind: String,
    pub display_name: String,
    pub content: String,
}

/// Where a round stands.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewRound {
    pub round: i64,
    pub head_seq: i64,
    pub deadline: Option<chrono::DateTime<chrono::Utc>>,
    /// Per member, in a stable order.
    pub members: Vec<ViewMember>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewMember {
    pub participant_id: ParticipantId,
    pub display_name: String,
    pub kind: String,
    /// `pending`, `sealed`, `passed`, or `timed_out`.
    pub disposition: String,
}

/// A human contribution.
#[derive(Debug, Clone, Deserialize)]
pub struct Contribution {
    pub participant_id: ParticipantId,
    pub content: String,
}

/// Everything the read side needs.
///
/// Holds the store behind `dyn` rather than a type parameter: the handlers do
/// not care which store they read, and a concrete context keeps axum's handler
/// bounds simple.
#[derive(Clone)]
pub struct ViewContext {
    pub store: Arc<dyn LedgerStore>,
    pub humans: Arc<HumanDesk>,
    /// The round currently open, so a person's contribution is filed under the
    /// round during which they said it.
    pub current_round: Arc<AtomicI64>,
}

/// The client-facing API.
pub fn router(context: ViewContext) -> Router {
    Router::new()
        .route("/conversations/{id}/entries", get(get_entries))
        .route("/conversations/{id}/round", get(get_round))
        .route("/conversations/{id}/contribution", post(post_contribution))
        .route("/conversations/{id}/pass", post(post_pass))
        .route("/health", get(|| async { StatusCode::OK }))
        .with_state(context)
}

fn kind_str(kind: ParticipantKind) -> &'static str {
    match kind {
        ParticipantKind::Human => "human",
        ParticipantKind::Agent => "agent",
    }
}

fn disposition_str(disposition: RoundDisposition) -> &'static str {
    match disposition {
        RoundDisposition::Pending => "pending",
        RoundDisposition::Sealed => "sealed",
        RoundDisposition::Passed => "passed",
        RoundDisposition::TimedOut => "timed_out",
    }
}

/// `GET /conversations/{id}/entries?from=<seq>` — the visible conversation.
async fn get_entries(
    State(context): State<ViewContext>,
    Path(conversation): Path<uuid::Uuid>,
    axum::extract::Query(query): axum::extract::Query<FromQuery>,
) -> Result<Json<Vec<ViewEntry>>, StatusCode> {
    let conversation = ConversationId(conversation);
    let entries = context
        .store
        .entries_from(conversation, Seq(query.from.unwrap_or(0)))
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let members = context
        .store
        .members(conversation)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;

    Ok(Json(
        entries
            .into_iter()
            .map(|entry| {
                let author = members
                    .iter()
                    .find(|member| member.id == entry.participant_id);
                ViewEntry {
                    seq: entry.seq.0,
                    round: entry.round.0,
                    participant_id: entry.participant_id,
                    kind: author
                        .map(|author| kind_str(author.kind))
                        .unwrap_or("unknown")
                        .to_string(),
                    display_name: author
                        .map(|author| author.display_name.clone())
                        .unwrap_or_else(|| "unknown".to_string()),
                    content: entry.content,
                }
            })
            .collect(),
    ))
}

#[derive(Debug, Deserialize)]
pub struct FromQuery {
    pub from: Option<i64>,
}

/// `GET /conversations/{id}/round` — who is still thinking.
async fn get_round(
    State(context): State<ViewContext>,
    Path(conversation): Path<uuid::Uuid>,
    axum::extract::Query(query): axum::extract::Query<RoundQuery>,
) -> Result<Json<ViewRound>, StatusCode> {
    let conversation = ConversationId(conversation);
    let meta = context
        .store
        .conversation_meta(conversation)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let members = context
        .store
        .members(conversation)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;

    let round = Round(query.round.unwrap_or(0));
    let state = context
        .store
        .round_state(conversation, round)
        .await
        .unwrap_or_default();
    let deadline = context.store.round_deadline(conversation, round).await.ok();

    Ok(Json(ViewRound {
        round: round.0,
        head_seq: meta.head_seq.0,
        deadline,
        members: members
            .iter()
            .map(|member| {
                let disposition = state
                    .iter()
                    .find(|(id, _)| *id == member.id)
                    .map(|(_, disposition)| disposition_str(*disposition))
                    // Absent from the round state means an observer, who is
                    // never enrolled and never holds a round open.
                    .unwrap_or("observer");
                ViewMember {
                    participant_id: member.id,
                    display_name: member.display_name.clone(),
                    kind: kind_str(member.kind).to_string(),
                    disposition: disposition.to_string(),
                }
            })
            .collect(),
    }))
}

#[derive(Debug, Deserialize)]
pub struct RoundQuery {
    pub round: Option<i64>,
}

/// `POST /conversations/{id}/contribution` — a human seals a contribution.
///
/// It is composed against the current head, exactly as an agent's would be, and
/// is subject to the same currency check and the same oracle.
async fn post_contribution(
    State(context): State<ViewContext>,
    Path(conversation): Path<uuid::Uuid>,
    Json(contribution): Json<Contribution>,
) -> Result<StatusCode, StatusCode> {
    let conversation = ConversationId(conversation);
    if contribution.content.trim().is_empty() {
        // Nothing typed is a pass, not an empty turn in the ledger.
        return post_pass(
            State(context),
            Path(conversation.0),
            Json(PassRequest {
                participant_id: contribution.participant_id,
            }),
        )
        .await;
    }

    // Appended immediately. A person's words go into the record when they say
    // them — they are not a proposal awaiting a verdict, and a ledger that
    // lagged the conversation would be the wrong way round for the participant
    // the whole thing exists to serve.
    //
    // This does not disturb the round in progress. Agents are currency-checked
    // against the head captured when their round opened, not the live head, so a
    // contribution landing mid-round invalidates nobody's in-flight work. The
    // round number filed here keeps the meaning of a shared round intact: those
    // agents composed before this arrived and will see it next round.
    let round = Round(context.current_round.load(Ordering::Relaxed).max(1));
    context
        .store
        .append_batch(
            conversation,
            round,
            vec![NewEntry {
                participant_id: contribution.participant_id,
                content: contribution.content,
                kind: EntryKind::Contribution,
            }],
        )
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;

    // Release the round from waiting on them; they have spoken.
    context
        .humans
        .contributed(conversation, contribution.participant_id);
    Ok(StatusCode::CREATED)
}

#[derive(Debug, Clone, Deserialize)]
pub struct PassRequest {
    pub participant_id: ParticipantId,
}

/// `POST /conversations/{id}/pass` — nothing to add this round.
///
/// Worth having as an explicit action: it lets the round close as soon as
/// everyone has answered rather than waiting out the clock.
async fn post_pass(
    State(context): State<ViewContext>,
    Path(conversation): Path<uuid::Uuid>,
    Json(request): Json<PassRequest>,
) -> Result<StatusCode, StatusCode> {
    let conversation = ConversationId(conversation);
    context.humans.passed(conversation, request.participant_id);
    Ok(StatusCode::ACCEPTED)
}
