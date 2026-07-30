//! The round loop, end to end, with no database, no container, and no model.
//!
//! This is the milestone the whole sequencing was arranged around: an in-memory
//! store and in-process participants are enough to prove that a multi-agent
//! conversation actually advances — rounds open and close, contributions are
//! judged and admitted in a deterministic order, stale work is refused, silence
//! is free, and a dead participant does not stop the room.

use std::collections::BTreeMap;
use std::time::Duration;
use zeroclaw_ledger::types::ConversationId;
use zeroclaw_ledger::types::{
    ConversationKind, Disposition, MemberRole, Participant, ParticipantId, Round, RoundDisposition,
    Seq,
};
use zeroclaw_ledger_store::fixtures::{agent, human};
use zeroclaw_ledger_store::{InMemoryLedgerStore, LedgerStore};
use zeroclaw_orchestrator::bot_client::BotEndpoint;
use zeroclaw_orchestrator::human_desk::HumanDesk;
use zeroclaw_orchestrator::mock::{MockBehaviour, MockBotClient};
use zeroclaw_orchestrator::oracle::{AdmitAll, RulesOracle};
use zeroclaw_orchestrator::round_driver::RoundDriver;

const FAST_POLL: Duration = Duration::from_millis(5);

/// A conversation with the given participants, all contributors.
async fn conversation_with(
    store: &InMemoryLedgerStore,
    participants: &[Participant],
    round_timeout: Duration,
) -> ConversationId {
    let conversation = store
        .create_conversation(ConversationKind::Group, round_timeout)
        .await
        .expect("create conversation");
    for participant in participants {
        store
            .upsert_participant(participant.clone())
            .await
            .expect("register participant");
        store
            .add_member(conversation, participant.id, MemberRole::Contributor)
            .await
            .expect("add member");
    }
    conversation
}

fn endpoints(participants: &[Participant]) -> BTreeMap<ParticipantId, BotEndpoint> {
    participants
        .iter()
        .map(|participant| {
            (
                participant.id,
                BotEndpoint {
                    participant: participant.id,
                    base_url: format!("http://{}", participant.display_name),
                },
            )
        })
        .collect()
}

#[tokio::test]
async fn two_rounds_advance_the_conversation() {
    let store = InMemoryLedgerStore::new();
    let reviewer = agent("reviewer");
    let critic = agent("critic");
    let members = [reviewer.clone(), critic.clone()];
    let conversation = conversation_with(&store, &members, Duration::from_secs(60)).await;

    let bots = MockBotClient::new()
        .with(
            reviewer.id,
            MockBehaviour::Contribute("reviewer view".to_string()),
        )
        .with(
            critic.id,
            MockBehaviour::Contribute("critic view".to_string()),
        );

    let humans = HumanDesk::new();

    let driver = RoundDriver {
        store: &store,
        bots: &bots,
        oracle: &AdmitAll,
        endpoints: endpoints(&members),
        humans: &humans,
        stale_window: 0,
        poll_interval: FAST_POLL,
    };

    let first = driver
        .run_round(conversation, Round(1))
        .await
        .expect("round 1");
    assert_eq!(first.admitted.len(), 2, "both contributions landed");
    assert_eq!(first.head_after, Seq(2));

    let second = driver
        .run_round(conversation, Round(2))
        .await
        .expect("round 2");
    assert_eq!(second.admitted.len(), 2);
    assert_eq!(second.head_after, Seq(4));

    let entries = store
        .entries_from(conversation, Seq::EMPTY)
        .await
        .expect("read ledger");
    assert_eq!(entries.len(), 4);
    for pair in entries.windows(2) {
        assert!(pair[1].seq > pair[0].seq, "the ledger is ordered");
    }

    // Round 2's participants saw round 1's contributions — that is what makes
    // the shared ledger the context window.
    let envelopes = bots.envelopes_for(critic.id);
    assert_eq!(envelopes.len(), 2, "one envelope per round");
    assert!(
        envelopes[0].context.is_empty(),
        "the first round opens on an empty ledger"
    );
    assert!(
        !envelopes[1].context.is_empty(),
        "the second round carries the first round's contributions"
    );
    assert_eq!(envelopes[1].head_seq, Seq(2));
}

#[tokio::test]
async fn a_participant_contributes_at_most_once_per_round() {
    let store = InMemoryLedgerStore::new();
    let reviewer = agent("reviewer");
    let members = [reviewer.clone()];
    let conversation = conversation_with(&store, &members, Duration::from_secs(60)).await;

    let bots = MockBotClient::new().with(
        reviewer.id,
        MockBehaviour::Contribute("one point".to_string()),
    );
    let humans = HumanDesk::new();
    let driver = RoundDriver {
        store: &store,
        bots: &bots,
        oracle: &AdmitAll,
        endpoints: endpoints(&members),
        humans: &humans,
        stale_window: 0,
        poll_interval: FAST_POLL,
    };

    let report = driver
        .run_round(conversation, Round(1))
        .await
        .expect("round 1");
    assert_eq!(
        report.admitted.len(),
        1,
        "polling repeatedly must not admit the same draft twice"
    );
}

#[tokio::test]
async fn contributions_land_in_deterministic_order() {
    // Admission order must come from the participants, not from whichever mailbox
    // answered first — otherwise every reader's projection diverges and prompt
    // caching quietly dies.
    let store = InMemoryLedgerStore::new();
    let mut members: Vec<Participant> = (0..4).map(|n| agent(&format!("agent{n}"))).collect();
    members.sort_by_key(|participant| participant.id);
    let conversation = conversation_with(&store, &members, Duration::from_secs(60)).await;

    let mut bots = MockBotClient::new();
    for (n, participant) in members.iter().enumerate() {
        bots = bots.with(
            participant.id,
            MockBehaviour::Contribute(format!("from agent index {n}")),
        );
    }

    let humans = HumanDesk::new();

    let driver = RoundDriver {
        store: &store,
        bots: &bots,
        oracle: &AdmitAll,
        endpoints: endpoints(&members),
        humans: &humans,
        stale_window: 0,
        poll_interval: FAST_POLL,
    };
    driver
        .run_round(conversation, Round(1))
        .await
        .expect("round 1");

    let entries = store
        .entries_from(conversation, Seq::EMPTY)
        .await
        .expect("read ledger");
    let authors: Vec<ParticipantId> = entries.iter().map(|entry| entry.participant_id).collect();
    let mut expected = authors.clone();
    expected.sort();
    assert_eq!(
        authors, expected,
        "a round's contributions are ordered by author, not by retrieval"
    );
}

#[tokio::test]
async fn a_round_of_passes_admits_nothing_and_leaves_the_ledger_untouched() {
    let store = InMemoryLedgerStore::new();
    let quiet = agent("quiet");
    let members = [quiet.clone()];
    let conversation = conversation_with(&store, &members, Duration::from_secs(60)).await;

    let bots = MockBotClient::new().with(quiet.id, MockBehaviour::Pass);
    let humans = HumanDesk::new();
    let driver = RoundDriver {
        store: &store,
        bots: &bots,
        oracle: &AdmitAll,
        endpoints: endpoints(&members),
        humans: &humans,
        stale_window: 0,
        poll_interval: FAST_POLL,
    };

    let report = driver
        .run_round(conversation, Round(1))
        .await
        .expect("round 1");
    assert!(report.admitted.is_empty(), "nobody had anything to say");
    assert_eq!(report.head_after, Seq::EMPTY, "the ledger did not move");

    assert_eq!(
        store
            .round_state(conversation, Round(1))
            .await
            .expect("round state"),
        vec![(quiet.id, RoundDisposition::Passed)],
        "a pass is a real response, recorded as one"
    );
}

#[tokio::test]
async fn a_silent_participant_times_out_without_stalling_the_round() {
    // The barrier's whole purpose: one participant thinking forever must not stop
    // the conversation, and must not be mistaken for one that chose to pass.
    let store = InMemoryLedgerStore::new();
    let talker = agent("talker");
    let ghost = agent("ghost");
    let members = [talker.clone(), ghost.clone()];
    let conversation = conversation_with(&store, &members, Duration::from_millis(150)).await;

    let bots = MockBotClient::new()
        .with(talker.id, MockBehaviour::Contribute("present".to_string()))
        .with(ghost.id, MockBehaviour::Silent);

    let humans = HumanDesk::new();

    let driver = RoundDriver {
        store: &store,
        bots: &bots,
        oracle: &AdmitAll,
        endpoints: endpoints(&members),
        humans: &humans,
        stale_window: 0,
        poll_interval: FAST_POLL,
    };

    let report = driver
        .run_round(conversation, Round(1))
        .await
        .expect("round 1");
    assert_eq!(report.admitted.len(), 1, "the round completed regardless");

    let state: BTreeMap<ParticipantId, RoundDisposition> = store
        .round_state(conversation, Round(1))
        .await
        .expect("round state")
        .into_iter()
        .collect();
    assert_eq!(state.get(&talker.id), Some(&RoundDisposition::Sealed));
    assert_eq!(
        state.get(&ghost.id),
        Some(&RoundDisposition::TimedOut),
        "silence and non-response are different facts"
    );
}

#[tokio::test]
async fn an_unreachable_participant_is_audited_not_fatal() {
    let store = InMemoryLedgerStore::new();
    let alive = agent("alive");
    let dead = agent("dead");
    let members = [alive.clone(), dead.clone()];
    let conversation = conversation_with(&store, &members, Duration::from_millis(150)).await;

    let bots = MockBotClient::new()
        .with(
            alive.id,
            MockBehaviour::Contribute("still here".to_string()),
        )
        .with(dead.id, MockBehaviour::Unreachable);

    let humans = HumanDesk::new();

    let driver = RoundDriver {
        store: &store,
        bots: &bots,
        oracle: &AdmitAll,
        endpoints: endpoints(&members),
        humans: &humans,
        stale_window: 0,
        poll_interval: FAST_POLL,
    };

    let report = driver
        .run_round(conversation, Round(1))
        .await
        .expect("a dead participant must not abort the round");
    assert_eq!(report.admitted.len(), 1);

    let audit = store
        .audit_events(conversation)
        .await
        .expect("read audit log");
    assert!(
        audit
            .iter()
            .any(|event| event.event.participant_id == Some(dead.id)),
        "the failure is recorded for the operator"
    );

    // And none of it reached the conversation.
    let entries = store
        .entries_from(conversation, Seq::EMPTY)
        .await
        .expect("read ledger");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].content, "still here");
}

#[tokio::test]
async fn a_stale_draft_is_refused_and_its_author_told_to_reconsider() {
    let store = InMemoryLedgerStore::new();
    let current = agent("current");
    let behind = agent("behind");
    let members = [current.clone(), behind.clone()];
    let conversation = conversation_with(&store, &members, Duration::from_secs(60)).await;

    let bots = MockBotClient::new()
        .with(
            current.id,
            MockBehaviour::Contribute("up to date".to_string()),
        )
        .with(
            behind.id,
            MockBehaviour::Stale {
                content: "answering an old question".to_string(),
                composed_against_seq: Seq(0),
            },
        );

    let humans = HumanDesk::new();

    let driver = RoundDriver {
        store: &store,
        bots: &bots,
        oracle: &AdmitAll,
        endpoints: endpoints(&members),
        humans: &humans,
        stale_window: 0,
        poll_interval: FAST_POLL,
    };

    // Round 1 opens on an empty ledger, so Seq(0) is current for everyone.
    driver
        .run_round(conversation, Round(1))
        .await
        .expect("round 1");
    // Round 2 opens at a moved head, so the fixed Seq(0) draft is now stale.
    let report = driver
        .run_round(conversation, Round(2))
        .await
        .expect("round 2");

    assert_eq!(
        report.admitted.len(),
        1,
        "only the current contribution was admitted"
    );
    assert!(
        bots.dispositions_for(behind.id)
            .iter()
            .any(|disposition| matches!(disposition, Disposition::Stale { .. })),
        "the author is told to reconsider against the newest entry"
    );
}

#[tokio::test]
async fn admitted_authors_learn_their_sequence() {
    let store = InMemoryLedgerStore::new();
    let speaker = agent("speaker");
    let members = [speaker.clone()];
    let conversation = conversation_with(&store, &members, Duration::from_secs(60)).await;

    let bots = MockBotClient::new().with(
        speaker.id,
        MockBehaviour::Contribute("recorded".to_string()),
    );
    let humans = HumanDesk::new();
    let driver = RoundDriver {
        store: &store,
        bots: &bots,
        oracle: &AdmitAll,
        endpoints: endpoints(&members),
        humans: &humans,
        stale_window: 0,
        poll_interval: FAST_POLL,
    };

    driver
        .run_round(conversation, Round(1))
        .await
        .expect("round 1");

    assert_eq!(
        bots.dispositions_for(speaker.id),
        vec![Disposition::Admitted { seq: Seq(1) }],
        "an author learns where its contribution landed"
    );
}

#[tokio::test]
async fn a_rejected_contribution_never_enters_the_ledger() {
    let store = InMemoryLedgerStore::new();
    let verbose = agent("verbose");
    let members = [verbose.clone()];
    let conversation = conversation_with(&store, &members, Duration::from_secs(60)).await;

    let bots = MockBotClient::new().with(verbose.id, MockBehaviour::Contribute("x".repeat(200)));
    let humans = HumanDesk::new();
    let driver = RoundDriver {
        store: &store,
        bots: &bots,
        oracle: &RulesOracle { max_chars: 10 },
        endpoints: endpoints(&members),
        humans: &humans,
        stale_window: 0,
        poll_interval: FAST_POLL,
    };

    let report = driver
        .run_round(conversation, Round(1))
        .await
        .expect("round 1");
    assert!(report.admitted.is_empty());
    assert!(
        store
            .entries_from(conversation, Seq::EMPTY)
            .await
            .expect("read ledger")
            .is_empty(),
        "rejected content must not reach the ledger"
    );
    assert!(
        bots.dispositions_for(verbose.id)
            .iter()
            .any(|disposition| matches!(disposition, Disposition::Rejected { .. })),
        "the author is told why"
    );
}

#[tokio::test]
async fn a_human_member_without_a_mailbox_does_not_stall_the_round() {
    // Humans contribute through the view API, not a mailbox. Until that exists, a
    // human simply has no endpoint — and must still not hold a round open.
    let store = InMemoryLedgerStore::new();
    let bot = agent("bot");
    let person = human("jonathan");
    let members = [bot.clone(), person.clone()];
    let conversation = conversation_with(&store, &members, Duration::from_millis(150)).await;

    let bots = MockBotClient::new().with(bot.id, MockBehaviour::Contribute("hello".to_string()));

    // Only the bot has an endpoint.
    let humans = HumanDesk::new();
    let driver = RoundDriver {
        store: &store,
        bots: &bots,
        oracle: &AdmitAll,
        endpoints: endpoints(std::slice::from_ref(&bot)),
        humans: &humans,
        stale_window: 0,
        poll_interval: FAST_POLL,
    };

    let report = driver
        .run_round(conversation, Round(1))
        .await
        .expect("round 1");
    assert_eq!(report.admitted.len(), 1);
}

#[tokio::test]
async fn a_human_contribution_is_already_in_the_ledger() {
    // People write to the ledger; agents propose and are judged. A person's words
    // are appended when they say them, so by the time the round closes there is
    // nothing of theirs left to admit — and nothing that could have been refused.
    let store = InMemoryLedgerStore::new();
    let bot = agent("bot");
    let person = human("jonathan");
    let members = [bot.clone(), person.clone()];
    let conversation = conversation_with(&store, &members, Duration::from_millis(200)).await;

    // What the view API does on receiving a contribution: append, then record
    // that this person has responded.
    store
        .append_batch(
            conversation,
            Round(1),
            vec![zeroclaw_ledger::types::NewEntry {
                participant_id: person.id,
                content: "x".repeat(5_000),
                kind: zeroclaw_ledger::types::EntryKind::Contribution,
            }],
        )
        .await
        .expect("a person's words go straight in");

    let humans = HumanDesk::new();
    humans.contributed(conversation, person.id);

    let bots = MockBotClient::new().with(bot.id, MockBehaviour::Contribute("brief".to_string()));
    let driver = RoundDriver {
        store: &store,
        bots: &bots,
        // A limit the person's contribution violates by two orders of magnitude,
        // to show it was never subject to one.
        oracle: &RulesOracle { max_chars: 10 },
        endpoints: endpoints(std::slice::from_ref(&bot)),
        humans: &humans,
        stale_window: 0,
        poll_interval: FAST_POLL,
    };

    driver
        .run_round(conversation, Round(1))
        .await
        .expect("round 1");

    let entries = store
        .entries_from(conversation, Seq::EMPTY)
        .await
        .expect("read ledger");
    assert!(
        entries
            .iter()
            .any(|entry| entry.participant_id == person.id && entry.content.len() == 5_000),
        "the person's words are in the ledger, oracle limit notwithstanding"
    );
    assert!(
        entries.iter().any(|entry| entry.content == "brief"),
        "the agent's contribution landed too"
    );

    let state: BTreeMap<ParticipantId, RoundDisposition> = store
        .round_state(conversation, Round(1))
        .await
        .expect("round state")
        .into_iter()
        .collect();
    assert_eq!(
        state.get(&person.id),
        Some(&RoundDisposition::Sealed),
        "someone who spoke is shown as having spoken, not as having passed"
    );
}

#[tokio::test]
async fn a_human_contribution_does_not_invalidate_in_flight_agent_work() {
    // The reason an immediate append is safe: agents are currency-checked
    // against the head their round opened at, not the live head. A person typing
    // mid-round must not silently discard the whole panel's work.
    let store = InMemoryLedgerStore::new();
    let bot = agent("bot");
    let person = human("jonathan");
    let members = [bot.clone(), person.clone()];
    let conversation = conversation_with(&store, &members, Duration::from_millis(200)).await;

    let bots = MockBotClient::new().with(bot.id, MockBehaviour::Contribute("composed".to_string()));
    let humans = HumanDesk::new();
    let driver = RoundDriver {
        store: &store,
        bots: &bots,
        oracle: &AdmitAll,
        endpoints: endpoints(std::slice::from_ref(&bot)),
        humans: &humans,
        stale_window: 0,
        poll_interval: FAST_POLL,
    };

    // Round 1 establishes a head the agent will compose against.
    driver
        .run_round(conversation, Round(1))
        .await
        .expect("round 1");
    let head_after_first = store.head_seq(conversation).await.expect("head");

    // A person speaks, advancing the head while round 2 is about to run.
    store
        .append_batch(
            conversation,
            Round(2),
            vec![zeroclaw_ledger::types::NewEntry {
                participant_id: person.id,
                content: "typed mid-round".to_string(),
                kind: zeroclaw_ledger::types::EntryKind::Contribution,
            }],
        )
        .await
        .expect("append");
    humans.contributed(conversation, person.id);
    assert!(
        store.head_seq(conversation).await.expect("head") > head_after_first,
        "the person's words moved the head"
    );

    let report = driver
        .run_round(conversation, Round(2))
        .await
        .expect("round 2");
    assert_eq!(
        report.admitted.len(),
        1,
        "the agent's contribution was admitted, not refused as stale"
    );
}

#[tokio::test]
async fn a_human_who_says_nothing_does_not_stall_the_round() {
    // The other half of the guarantee: a person is always able to contribute,
    // and never obliged to. Someone who stepped away must not hold the panel.
    let store = InMemoryLedgerStore::new();
    let bot = agent("bot");
    let person = human("jonathan");
    let members = [bot.clone(), person.clone()];
    let conversation = conversation_with(&store, &members, Duration::from_millis(120)).await;

    let bots = MockBotClient::new().with(bot.id, MockBehaviour::Contribute("thinking".to_string()));
    let humans = HumanDesk::new();
    let driver = RoundDriver {
        store: &store,
        bots: &bots,
        oracle: &AdmitAll,
        endpoints: endpoints(std::slice::from_ref(&bot)),
        humans: &humans,
        stale_window: 0,
        poll_interval: FAST_POLL,
    };

    let report = driver
        .run_round(conversation, Round(1))
        .await
        .expect("the round completes without the person");
    assert_eq!(report.admitted.len(), 1);

    let state: BTreeMap<ParticipantId, RoundDisposition> = store
        .round_state(conversation, Round(1))
        .await
        .expect("round state")
        .into_iter()
        .collect();
    assert_eq!(state.get(&person.id), Some(&RoundDisposition::TimedOut));
}

#[tokio::test]
async fn a_human_pass_closes_the_round_early() {
    let store = InMemoryLedgerStore::new();
    let bot = agent("bot");
    let person = human("jonathan");
    let members = [bot.clone(), person.clone()];
    // A long window: if the round waited it out, this test would be slow.
    let conversation = conversation_with(&store, &members, Duration::from_secs(30)).await;

    let bots = MockBotClient::new().with(bot.id, MockBehaviour::Contribute("said".to_string()));
    let humans = HumanDesk::new();
    humans.passed(conversation, person.id);

    let driver = RoundDriver {
        store: &store,
        bots: &bots,
        oracle: &AdmitAll,
        endpoints: endpoints(std::slice::from_ref(&bot)),
        humans: &humans,
        stale_window: 0,
        poll_interval: FAST_POLL,
    };

    let started = std::time::Instant::now();
    driver
        .run_round(conversation, Round(1))
        .await
        .expect("round 1");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "an explicit pass closes the round rather than running the clock down"
    );

    let state: BTreeMap<ParticipantId, RoundDisposition> = store
        .round_state(conversation, Round(1))
        .await
        .expect("round state")
        .into_iter()
        .collect();
    assert_eq!(state.get(&person.id), Some(&RoundDisposition::Passed));
}
