//! A single behavioural contract every [`LedgerStore`] implementation must satisfy.
//!
//! The in-memory store and Postgres are held to *the same* suite, run verbatim
//! against both. That matters more than it might appear: the in-memory store is
//! what the orchestrator's tests run against, so if the two implementations can
//! drift, every test above this layer is testing a fiction. Anything asserted
//! here is a promise the orchestrator is allowed to rely on.
//!
//! This module ships ungated rather than behind a `testing` feature, because a
//! Cargo feature cannot be enabled for a crate's own integration tests without a
//! self-referential dev-dependency. These crates are `publish = false`, so the
//! cost is a little dead weight in a binary that is never shipped, against a
//! suite that is guaranteed to be reachable from both implementations' tests.

use crate::fixtures::{agent, contribution, human};
use crate::store::{LedgerStore, ProposalRecord, StoreError};
use chrono::{TimeDelta, Utc};
use std::time::Duration;
use uuid::Uuid;
use zeroclaw_ledger::{
    AuditEvent, AuditKind, ConversationId, ConversationKind, DraftId, DraftVersion, MemberRole,
    OracleVerdict, ParticipantId, Round, RoundDisposition, Seq,
};

/// Runs the full contract. Panics with a descriptive message on any violation.
pub async fn run_conformance_suite<S>(store: &S)
where
    S: LedgerStore + ?Sized,
{
    sequences_are_monotonic_and_gapless(store).await;
    batch_preserves_input_order(store).await;
    tombstones_are_withheld_but_not_deleted(store).await;
    visibility_floor_withholds_and_only_rises(store).await;
    members_are_ordered_and_roles_readable(store).await;
    rounds_persist_dispositions_and_deadlines(store).await;
    proposals_persist_with_their_verdicts(store).await;
    audit_never_reaches_ledger_content(store).await;
    missing_things_are_reported_not_panicked(store).await;
}

/// Creates a conversation with `count` agent contributors, returning their ids.
async fn conversation_with_contributors<S>(
    store: &S,
    count: usize,
) -> (ConversationId, Vec<ParticipantId>)
where
    S: LedgerStore + ?Sized,
{
    let conversation = store
        .create_conversation(ConversationKind::Group, Duration::from_secs(60))
        .await
        .expect("create conversation");

    let mut ids = Vec::with_capacity(count);
    for n in 0..count {
        let participant = agent(&format!("agent-{n}"));
        ids.push(participant.id);
        store
            .upsert_participant(participant.clone())
            .await
            .expect("upsert participant");
        store
            .add_member(conversation, participant.id, MemberRole::Contributor)
            .await
            .expect("add member");
    }
    (conversation, ids)
}

async fn sequences_are_monotonic_and_gapless<S>(store: &S)
where
    S: LedgerStore + ?Sized,
{
    let (conversation, participants) = conversation_with_contributors(store, 1).await;
    let author = participants[0];

    assert_eq!(
        store.head_seq(conversation).await.expect("head"),
        Seq::EMPTY,
        "a fresh conversation starts at the empty head"
    );

    let first = store
        .append_batch(
            conversation,
            Round(1),
            vec![contribution(author, "one"), contribution(author, "two")],
        )
        .await
        .expect("append first batch");
    assert_eq!(first, vec![Seq(1), Seq(2)], "sequences are 1-based");

    let second = store
        .append_batch(conversation, Round(2), vec![contribution(author, "three")])
        .await
        .expect("append second batch");
    assert_eq!(second, vec![Seq(3)], "sequences continue across batches");

    assert_eq!(
        store.head_seq(conversation).await.expect("head"),
        Seq(3),
        "head tracks the most recent entry"
    );

    // An empty batch must not disturb the head: a round in which everyone
    // passed admits nothing and must leave the ledger untouched.
    let empty = store
        .append_batch(conversation, Round(3), Vec::new())
        .await
        .expect("append empty batch");
    assert!(empty.is_empty(), "an empty batch assigns no sequences");
    assert_eq!(
        store.head_seq(conversation).await.expect("head"),
        Seq(3),
        "an all-pass round leaves the head unchanged"
    );
}

async fn batch_preserves_input_order<S>(store: &S)
where
    S: LedgerStore + ?Sized,
{
    let (conversation, participants) = conversation_with_contributors(store, 3).await;

    // The caller has already ordered this round deterministically; the store
    // must not reorder it. Submitted deliberately against ascending participant
    // order so a store that sorted internally would still look correct — the
    // content check below is what actually pins it.
    let contents = ["alpha", "beta", "gamma"];
    let entries: Vec<_> = participants
        .iter()
        .zip(contents)
        .map(|(id, content)| contribution(*id, content))
        .collect();

    let assigned = store
        .append_batch(conversation, Round(1), entries)
        .await
        .expect("append batch");
    assert_eq!(assigned.len(), 3);

    let stored = store
        .entries_from(conversation, Seq::EMPTY)
        .await
        .expect("read entries");
    let stored_contents: Vec<&str> = stored.iter().map(|e| e.content.as_str()).collect();
    assert_eq!(
        stored_contents,
        contents.to_vec(),
        "entries must land in submitted order"
    );

    // Wall time is recorded for QC, and only there: it is absent from the entries
    // themselves so no projection can carry it into a participant's context.
    let timestamps = store
        .entry_timestamps(conversation)
        .await
        .expect("read entry timestamps");
    assert_eq!(
        timestamps.len(),
        3,
        "the store records when each entry landed, for logging and QC"
    );
    let stamped_sequences: Vec<Seq> = timestamps.iter().map(|(seq, _)| *seq).collect();
    assert_eq!(stamped_sequences, assigned, "timestamps track the entries");

    for (entry, seq) in stored.iter().zip(&assigned) {
        assert_eq!(
            entry.seq, *seq,
            "returned sequences correspond positionally to submitted entries"
        );
        assert_eq!(entry.round, Round(1), "entries carry their admitting round");
        assert!(!entry.tombstoned, "fresh entries are not tombstoned");
        assert_eq!(entry.conversation_id, conversation);
    }
}

async fn tombstones_are_withheld_but_not_deleted<S>(store: &S)
where
    S: LedgerStore + ?Sized,
{
    let (conversation, participants) = conversation_with_contributors(store, 1).await;
    let author = participants[0];
    store
        .append_batch(
            conversation,
            Round(1),
            vec![
                contribution(author, "kept"),
                contribution(author, "poisoned"),
                contribution(author, "after the poison"),
            ],
        )
        .await
        .expect("append batch");

    let affected = store
        .tombstone_from(conversation, Seq(2))
        .await
        .expect("tombstone");
    assert_eq!(affected, 2, "tombstoning is forward: seq 2 and 3");

    let visible = store
        .entries_from(conversation, Seq::EMPTY)
        .await
        .expect("read entries");
    let contents: Vec<&str> = visible.iter().map(|e| e.content.as_str()).collect();
    assert_eq!(
        contents,
        vec!["kept"],
        "tombstoned entries are withheld from readers"
    );

    // The head does not rewind. Sequences are never reused, so a later
    // contribution cannot silently occupy a withdrawn entry's position.
    assert_eq!(
        store.head_seq(conversation).await.expect("head"),
        Seq(3),
        "tombstoning withdraws content without rewinding the head"
    );
    let next = store
        .append_batch(conversation, Round(2), vec![contribution(author, "fresh")])
        .await
        .expect("append after tombstone");
    assert_eq!(
        next,
        vec![Seq(4)],
        "sequences continue past tombstoned rows"
    );

    // Re-tombstoning is idempotent: already-withdrawn rows are not recounted.
    let again = store
        .tombstone_from(conversation, Seq(2))
        .await
        .expect("tombstone again");
    assert_eq!(
        again, 1,
        "only the newly withdrawn entry counts on a second pass"
    );
}

async fn visibility_floor_withholds_and_only_rises<S>(store: &S)
where
    S: LedgerStore + ?Sized,
{
    let (conversation, participants) = conversation_with_contributors(store, 1).await;
    let author = participants[0];
    store
        .append_batch(
            conversation,
            Round(1),
            vec![
                contribution(author, "old"),
                contribution(author, "newer"),
                contribution(author, "newest"),
            ],
        )
        .await
        .expect("append batch");

    store
        .set_visible_from(conversation, Seq(1))
        .await
        .expect("raise floor");
    let visible = store
        .entries_from(conversation, Seq::EMPTY)
        .await
        .expect("read entries");
    let contents: Vec<&str> = visible.iter().map(|e| e.content.as_str()).collect();
    assert_eq!(
        contents,
        vec!["newer", "newest"],
        "entries at or below the floor are withheld"
    );

    // Containment must not be reversible by a later caller.
    store
        .set_visible_from(conversation, Seq::EMPTY)
        .await
        .expect("attempt to lower floor");
    let meta = store
        .conversation_meta(conversation)
        .await
        .expect("conversation meta");
    assert_eq!(
        meta.visible_from,
        Seq(1),
        "the visibility floor rises and never falls"
    );
}

async fn members_are_ordered_and_roles_readable<S>(store: &S)
where
    S: LedgerStore + ?Sized,
{
    let conversation = store
        .create_conversation(ConversationKind::Dm, Duration::from_secs(90))
        .await
        .expect("create conversation");

    let watcher = human("watcher");
    let speaker = agent("speaker");
    for participant in [&watcher, &speaker] {
        store
            .upsert_participant(participant.clone())
            .await
            .expect("upsert participant");
    }
    store
        .add_member(conversation, watcher.id, MemberRole::Observer)
        .await
        .expect("add observer");
    store
        .add_member(conversation, speaker.id, MemberRole::Contributor)
        .await
        .expect("add contributor");

    let members = store.members(conversation).await.expect("members");
    let ids: Vec<ParticipantId> = members.iter().map(|m| m.id).collect();
    let mut expected = ids.clone();
    expected.sort();
    assert_eq!(
        ids, expected,
        "members must be returned in ParticipantId order, since projection must be byte-stable"
    );

    assert_eq!(
        store
            .member_role(conversation, watcher.id)
            .await
            .expect("observer role"),
        MemberRole::Observer
    );
    assert_eq!(
        store
            .member_role(conversation, speaker.id)
            .await
            .expect("contributor role"),
        MemberRole::Contributor
    );

    // Re-adding a member updates the role rather than duplicating the member.
    store
        .add_member(conversation, watcher.id, MemberRole::Contributor)
        .await
        .expect("promote observer");
    assert_eq!(
        store.members(conversation).await.expect("members").len(),
        2,
        "membership is a set, not an append log"
    );
    assert_eq!(
        store
            .member_role(conversation, watcher.id)
            .await
            .expect("promoted role"),
        MemberRole::Contributor
    );
}

async fn rounds_persist_dispositions_and_deadlines<S>(store: &S)
where
    S: LedgerStore + ?Sized,
{
    let conversation = store
        .create_conversation(ConversationKind::Group, Duration::from_secs(60))
        .await
        .expect("create conversation");

    let contributor = agent("contributor");
    let observer = human("observer");
    for participant in [&contributor, &observer] {
        store
            .upsert_participant(participant.clone())
            .await
            .expect("upsert participant");
    }
    store
        .add_member(conversation, contributor.id, MemberRole::Contributor)
        .await
        .expect("add contributor");
    store
        .add_member(conversation, observer.id, MemberRole::Observer)
        .await
        .expect("add observer");

    let deadline = Utc::now() + TimeDelta::seconds(60);
    store
        .open_round(conversation, Round(1), deadline)
        .await
        .expect("open round");

    let state = store
        .round_state(conversation, Round(1))
        .await
        .expect("round state");
    assert_eq!(
        state,
        vec![(contributor.id, RoundDisposition::Pending)],
        "only contributors are enrolled — an observer must never hold a round open"
    );

    assert_eq!(
        store
            .round_deadline(conversation, Round(1))
            .await
            .expect("round deadline")
            .timestamp(),
        deadline.timestamp(),
        "the deadline is persisted so a round survives an orchestrator restart"
    );

    store
        .record_round_disposition(
            conversation,
            Round(1),
            contributor.id,
            RoundDisposition::Sealed,
        )
        .await
        .expect("record seal");
    assert_eq!(
        store
            .round_state(conversation, Round(1))
            .await
            .expect("round state"),
        vec![(contributor.id, RoundDisposition::Sealed)]
    );

    // Revising is normal: a participant may reseal until the round closes.
    store
        .record_round_disposition(
            conversation,
            Round(1),
            contributor.id,
            RoundDisposition::Passed,
        )
        .await
        .expect("record pass");
    assert_eq!(
        store
            .round_state(conversation, Round(1))
            .await
            .expect("round state"),
        vec![(contributor.id, RoundDisposition::Passed)],
        "a later disposition replaces the earlier one"
    );

    // Rounds are independent: opening round 2 does not disturb round 1.
    store
        .open_round(conversation, Round(2), Utc::now() + TimeDelta::seconds(60))
        .await
        .expect("open second round");
    assert_eq!(
        store
            .round_state(conversation, Round(1))
            .await
            .expect("round 1 state"),
        vec![(contributor.id, RoundDisposition::Passed)],
        "an earlier round keeps its recorded dispositions"
    );
    assert_eq!(
        store
            .round_state(conversation, Round(2))
            .await
            .expect("round 2 state"),
        vec![(contributor.id, RoundDisposition::Pending)],
        "a new round starts every contributor pending"
    );
}

async fn proposals_persist_with_their_verdicts<S>(store: &S)
where
    S: LedgerStore + ?Sized,
{
    let (conversation, participants) = conversation_with_contributors(store, 1).await;
    let author = participants[0];

    let proposal = DraftId(Uuid::new_v4());
    store
        .record_proposal(ProposalRecord {
            id: proposal,
            conversation_id: conversation,
            round: Round(1),
            participant_id: author,
            content: "a rejected thought".to_string(),
            composed_against_seq: Seq::EMPTY,
            version: DraftVersion(1),
        })
        .await
        .expect("record proposal");

    assert_eq!(
        store
            .proposal_verdict(proposal)
            .await
            .expect("verdict lookup"),
        None,
        "a freshly recorded proposal has not been judged yet"
    );

    let verdict = OracleVerdict::Reject {
        reason: "low quality".to_string(),
    };
    store
        .set_proposal_verdict(proposal, verdict.clone())
        .await
        .expect("set verdict");
    assert_eq!(
        store
            .proposal_verdict(proposal)
            .await
            .expect("verdict lookup"),
        Some(verdict),
        "rejected proposals are retained with their reason — the economic layer meters them"
    );

    // A rejected proposal's content must not have leaked into the ledger.
    assert!(
        store
            .entries_from(conversation, Seq::EMPTY)
            .await
            .expect("read entries")
            .is_empty(),
        "recording a proposal must never append to the ledger"
    );
}

async fn audit_never_reaches_ledger_content<S>(store: &S)
where
    S: LedgerStore + ?Sized,
{
    let (conversation, participants) = conversation_with_contributors(store, 1).await;
    let author = participants[0];
    let other = store
        .create_conversation(ConversationKind::Group, Duration::from_secs(60))
        .await
        .expect("create second conversation");

    store
        .append_batch(
            conversation,
            Round(1),
            vec![contribution(author, "a real contribution")],
        )
        .await
        .expect("append batch");

    let marker = "PEER TIMED OUT AFTER 60s";
    store
        .append_audit(AuditEvent {
            conversation_id: Some(conversation),
            participant_id: Some(author),
            round: Some(Round(1)),
            kind: AuditKind::ParticipantTimedOut,
            detail: marker.to_string(),
        })
        .await
        .expect("append audit");

    // The whole point of the two-log split: an agent that reads "peer timed out"
    // will comment on it, and that is how runaway loops start.
    let entries = store
        .entries_from(conversation, Seq::EMPTY)
        .await
        .expect("read entries");
    assert_eq!(entries.len(), 1, "audit events are not ledger entries");
    for entry in &entries {
        assert!(
            !entry.content.contains(marker),
            "operational text reached ledger content: {}",
            entry.content
        );
    }

    let audited = store
        .audit_events(conversation)
        .await
        .expect("read audit events");
    assert_eq!(audited.len(), 1, "the audit log retains the event");
    assert_eq!(audited[0].event.detail, marker);
    assert!(
        audited[0].occurred_at <= Utc::now(),
        "the store stamps an arrival time"
    );

    assert!(
        store
            .audit_events(other)
            .await
            .expect("read audit events")
            .is_empty(),
        "audit events are scoped to their conversation"
    );
}

async fn missing_things_are_reported_not_panicked<S>(store: &S)
where
    S: LedgerStore + ?Sized,
{
    let absent_conversation = ConversationId(Uuid::new_v4());
    let absent_participant = ParticipantId(Uuid::new_v4());

    assert!(
        matches!(
            store.head_seq(absent_conversation).await,
            Err(StoreError::ConversationNotFound(_))
        ),
        "an unknown conversation is an error, not a panic or a default"
    );

    let (conversation, _) = conversation_with_contributors(store, 1).await;

    assert!(
        matches!(
            store
                .add_member(conversation, absent_participant, MemberRole::Contributor)
                .await,
            Err(StoreError::ParticipantNotFound(_))
        ),
        "membership requires a registered participant"
    );

    assert!(
        matches!(
            store.member_role(conversation, absent_participant).await,
            Err(StoreError::NotAMember { .. })
        ),
        "a non-member has no role"
    );

    assert!(
        matches!(
            store.round_state(conversation, Round(99)).await,
            Err(StoreError::RoundNotOpen { .. })
        ),
        "an unopened round is an error rather than an empty round"
    );

    assert!(
        matches!(
            store.proposal_verdict(DraftId(Uuid::new_v4())).await,
            Err(StoreError::ProposalNotFound(_))
        ),
        "an unknown proposal is distinguishable from an unjudged one"
    );
}
