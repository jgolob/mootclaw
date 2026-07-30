//! Drives the mailbox endpoints in-process, with no network and no model.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use std::sync::Arc;
use tower::ServiceExt;
use zeroclaw_bot::compose::{AlwaysPass, Composer, Composition, FixedAnswer};
use zeroclaw_bot::mailbox_api::{BotContext, BotState, router};
use zeroclaw_ledger::mailbox::{BudgetStanding, DispositionNotice, InboxEnvelope, OutboxResponse};
use zeroclaw_ledger::projection::{ProjectedMessage, ProjectedRole};
use zeroclaw_ledger::types::{ConversationId, Disposition, DraftState, ParticipantId, Round, Seq};

fn conversation() -> ConversationId {
    ConversationId(uuid::Uuid::from_u128(0xC0FFEE))
}

fn identity() -> ParticipantId {
    ParticipantId(uuid::Uuid::from_u128(2))
}

fn envelope(round: i64, head: i64) -> InboxEnvelope {
    InboxEnvelope {
        conversation_id: conversation(),
        round: Round(round),
        head_seq: Seq(head),
        deadline: chrono::Utc::now() + chrono::TimeDelta::seconds(120),
        context: vec![ProjectedMessage {
            role: ProjectedRole::User,
            content: "[human:jonathan] what do you think?".to_string(),
        }],
        scratchpad: None,
        soul: None,
        budget: BudgetStanding::fresh(),
        dispositions: Vec::new(),
    }
}

fn context(composer: Arc<dyn Composer>) -> BotContext {
    BotContext {
        state: Arc::new(BotState::new(identity())),
        composer,
    }
}

async fn put_inbox(context: &BotContext, envelope: &InboxEnvelope) -> StatusCode {
    let request = Request::builder()
        .method("PUT")
        .uri("/inbox")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(envelope).expect("serialize envelope"),
        ))
        .expect("build request");
    router(context.clone())
        .oneshot(request)
        .await
        .expect("inbox request")
        .status()
}

async fn get_outbox(context: &BotContext) -> (StatusCode, OutboxResponse) {
    let request = Request::builder()
        .method("GET")
        .uri(format!("/outbox/{}", conversation().0))
        .body(Body::empty())
        .expect("build request");
    let response = router(context.clone())
        .oneshot(request)
        .await
        .expect("outbox request");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("read body")
        .to_bytes();
    let parsed = serde_json::from_slice(&bytes).expect("parse outbox response");
    (status, parsed)
}

async fn post_disposition(context: &BotContext, disposition: Disposition) -> StatusCode {
    let notice = DispositionNotice {
        conversation_id: conversation(),
        round: Round(1),
        disposition,
    };
    let request = Request::builder()
        .method("POST")
        .uri("/disposition")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&notice).expect("serialize notice"),
        ))
        .expect("build request");
    router(context.clone())
        .oneshot(request)
        .await
        .expect("disposition request")
        .status()
}

/// Polls the outbox until a draft appears, or gives up.
///
/// Mirrors what the orchestrator does: thinking happens in the background, so a
/// draft is not expected to be there the instant the envelope is accepted.
async fn await_draft(context: &BotContext) -> OutboxResponse {
    for _ in 0..100 {
        let (status, response) = get_outbox(context).await;
        assert_eq!(status, StatusCode::OK);
        if response.draft.is_some() {
            return response;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("no draft appeared in the outbox");
}

#[tokio::test]
async fn an_empty_outbox_is_served_for_an_unknown_conversation() {
    let context = context(Arc::new(AlwaysPass));
    let (status, response) = get_outbox(&context).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response.draft, None, "silence is a valid answer");
}

#[tokio::test]
async fn an_envelope_is_accepted_immediately() {
    // The orchestrator must not be held for the length of a participant's
    // thinking; the deadline in the envelope bounds the work, not the request.
    let context = context(Arc::new(AlwaysPass));
    assert_eq!(
        put_inbox(&context, &envelope(1, 10)).await,
        StatusCode::ACCEPTED
    );
}

#[tokio::test]
async fn a_contributing_participant_seals_a_draft() {
    let context = context(Arc::new(FixedAnswer("a considered point".to_string())));
    put_inbox(&context, &envelope(1, 10)).await;

    let draft = await_draft(&context).await.draft.expect("draft present");
    assert_eq!(draft.state, DraftState::Sealed, "ready for retrieval");
    assert_eq!(draft.content, "a considered point");
    assert_eq!(draft.participant_id, identity());
    assert_eq!(
        draft.composed_against_seq,
        Seq(10),
        "the draft records the head it was composed against"
    );
}

#[tokio::test]
async fn a_participant_with_nothing_to_add_passes() {
    let context = context(Arc::new(AlwaysPass));
    put_inbox(&context, &envelope(1, 10)).await;

    let draft = await_draft(&context).await.draft.expect("pass recorded");
    assert_eq!(
        draft.state,
        DraftState::Pass,
        "an explicit pass, not silence — it lets the round close early"
    );
    assert!(draft.content.is_empty());
}

#[tokio::test]
async fn a_blank_answer_becomes_a_pass_rather_than_an_empty_contribution() {
    let context = context(Arc::new(FixedAnswer("   \n  ".to_string())));
    put_inbox(&context, &envelope(1, 10)).await;

    let draft = await_draft(&context).await.draft.expect("draft present");
    assert_eq!(
        draft.state,
        DraftState::Pass,
        "whitespace must not become an empty turn in the ledger"
    );
}

#[tokio::test]
async fn redelivering_an_envelope_is_idempotent() {
    // The orchestrator retries freely, so redelivery must be safe.
    let context = context(Arc::new(FixedAnswer("stable answer".to_string())));
    let envelope = envelope(1, 10);

    put_inbox(&context, &envelope).await;
    let first = await_draft(&context).await.draft.expect("draft present");

    put_inbox(&context, &envelope).await;
    let second = await_draft(&context).await.draft.expect("draft present");

    assert_eq!(
        first.id, second.id,
        "the same round keeps one draft identity"
    );
    assert_eq!(second.content, "stable answer");
    assert_eq!(second.state, DraftState::Sealed);
}

#[tokio::test]
async fn a_superseded_envelope_does_not_drag_the_participant_backwards() {
    // A delayed round-1 delivery arriving after round 2 has opened must not
    // replace the current answer with one to a question the room has moved past.
    let context = context(Arc::new(FixedAnswer("current".to_string())));

    context
        .state
        .think(envelope(2, 20), context.composer.as_ref())
        .await
        .expect("think for round 2");

    assert_eq!(
        put_inbox(&context, &envelope(1, 10)).await,
        StatusCode::ACCEPTED,
        "the delivery succeeded; there is simply nothing to do about it"
    );

    let draft = await_draft(&context).await.draft.expect("draft present");
    assert_eq!(
        draft.composed_against_seq,
        Seq(20),
        "the round-2 draft is untouched"
    );
}

#[tokio::test]
async fn a_new_round_replaces_the_previous_answer() {
    let context = context(Arc::new(FixedAnswer("answer".to_string())));

    context
        .state
        .think(envelope(1, 10), context.composer.as_ref())
        .await
        .expect("think for round 1");
    let first = await_draft(&context).await.draft.expect("draft present");

    context
        .state
        .think(envelope(2, 11), context.composer.as_ref())
        .await
        .expect("think for round 2");
    let second = await_draft(&context).await.draft.expect("draft present");

    assert_ne!(first.id, second.id, "a new round is a new draft");
    assert_eq!(second.composed_against_seq, Seq(11));
}

#[tokio::test]
async fn dispositions_are_acknowledged() {
    let context = context(Arc::new(AlwaysPass));
    for disposition in [
        Disposition::Admitted { seq: Seq(11) },
        Disposition::Rejected {
            reason: "adds nothing".to_string(),
        },
        Disposition::Stale {
            current_seq: Seq(12),
        },
        Disposition::Expunged,
    ] {
        assert_eq!(
            post_disposition(&context, disposition.clone()).await,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            context
                .state
                .last_disposition(conversation())
                .expect("read disposition"),
            Some(disposition),
            "a running participant can be asked why its contribution did not appear"
        );
    }
}

#[tokio::test]
async fn the_participant_holds_no_authoritative_conversation_state() {
    // Restarting a participant loses only in-progress thinking. The next envelope
    // carries the whole conversation, so the answer is the same.
    let composer: Arc<dyn Composer> = Arc::new(FixedAnswer("consistent".to_string()));

    let before = context(Arc::clone(&composer));
    before
        .state
        .think(envelope(1, 10), before.composer.as_ref())
        .await
        .expect("think");
    let original = await_draft(&before).await.draft.expect("draft present");

    // A brand-new process, with nothing carried over.
    let after = context(composer);
    after
        .state
        .think(envelope(1, 10), after.composer.as_ref())
        .await
        .expect("think");
    let restarted = await_draft(&after).await.draft.expect("draft present");

    assert_eq!(original.content, restarted.content);
    assert_eq!(
        original.composed_against_seq,
        restarted.composed_against_seq
    );
    assert_eq!(original.state, restarted.state);
}

#[tokio::test]
async fn composition_treats_blank_answers_as_passes() {
    assert_eq!(Composition::from_answer("\t\n"), Composition::Pass);
}
