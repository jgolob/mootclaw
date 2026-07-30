//! Invariant 4, as an executable test: **operational text never becomes context.**
//!
//! The ledger holds contributions someone meant to make. Everything about the
//! machinery — timeouts, unreachable containers, rejected drafts, retries — goes
//! to the audit log. The separation is not tidiness. An agent that reads "peer
//! timed out" will helpfully remark on it, the next agent will respond to the
//! remark, and a conversation about the plumbing is the most reliable way to
//! burn a budget on nothing.
//!
//! So this drives a round that goes wrong in every way at once and then checks
//! every envelope any participant received, for every round, against everything
//! the audit log recorded.

use std::collections::BTreeMap;
use std::time::Duration;
use zeroclaw_ledger::types::{
    ConversationId, ConversationKind, MemberRole, Participant, ParticipantId, Round, Seq,
};
use zeroclaw_ledger_store::fixtures::agent;
use zeroclaw_ledger_store::{InMemoryLedgerStore, LedgerStore};
use zeroclaw_orchestrator::bot_client::BotEndpoint;
use zeroclaw_orchestrator::mock::{MockBehaviour, MockBotClient};
use zeroclaw_orchestrator::oracle::RulesOracle;
use zeroclaw_orchestrator::round_driver::RoundDriver;

const FAST_POLL: Duration = Duration::from_millis(5);

/// Words that only ever appear in operational text.
///
/// A cheap second net: even if an audit payload were reworded, a projection that
/// started narrating the machinery would almost certainly use one of these.
const OPERATIONAL_VOCABULARY: &[&str] = &[
    "unreachable",
    "timed out",
    "timeout",
    "deadline",
    "HTTP",
    "retry",
    "retries",
    "container",
    "mock is down",
    "no response",
];

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

/// Builds a conversation guaranteed to produce a messy audit log.
async fn troubled_conversation(
    store: &InMemoryLedgerStore,
) -> (ConversationId, Vec<Participant>, MockBotClient) {
    let good = agent("good");
    let verbose = agent("verbose");
    let ghost = agent("ghost");
    let dead = agent("dead");
    let members = vec![good.clone(), verbose.clone(), ghost.clone(), dead.clone()];

    let conversation = store
        .create_conversation(ConversationKind::Group, Duration::from_millis(120))
        .await
        .expect("create conversation");
    for participant in &members {
        store
            .upsert_participant(participant.clone())
            .await
            .expect("register participant");
        store
            .add_member(conversation, participant.id, MemberRole::Contributor)
            .await
            .expect("add member");
    }

    let bots = MockBotClient::new()
        .with(
            good.id,
            MockBehaviour::Contribute("a genuine contribution".to_string()),
        )
        // Rejected by the oracle for length.
        .with(verbose.id, MockBehaviour::Contribute("x".repeat(500)))
        // Never answers: times out.
        .with(ghost.id, MockBehaviour::Silent)
        // Every call fails: unreachable.
        .with(dead.id, MockBehaviour::Unreachable);

    (conversation, members, bots)
}

#[tokio::test]
async fn no_audit_text_ever_reaches_a_participants_context() {
    let store = InMemoryLedgerStore::new();
    let (conversation, members, bots) = troubled_conversation(&store).await;

    let driver = RoundDriver {
        store: &store,
        bots: &bots,
        oracle: &RulesOracle { max_chars: 50 },
        endpoints: endpoints(&members),
        stale_window: 0,
        poll_interval: FAST_POLL,
    };

    // Three rounds, so later envelopes carry the accumulated ledger — the point
    // at which leaked operational text would become permanent context.
    for round in 1..=3 {
        driver
            .run_round(conversation, Round(round))
            .await
            .expect("a troubled round still completes");
    }

    let audit = store
        .audit_events(conversation)
        .await
        .expect("read audit log");
    assert!(
        audit.len() >= 6,
        "the scenario must actually produce operational events, got {}",
        audit.len()
    );

    // Every distinctive audit detail must be absent from every context.
    let details: Vec<String> = audit
        .iter()
        .map(|stored| stored.event.detail.clone())
        .filter(|detail| detail.len() >= 8)
        .collect();
    assert!(
        !details.is_empty(),
        "expected substantive audit details to check against"
    );

    let mut envelopes_checked = 0usize;
    for participant in &members {
        for envelope in bots.envelopes_for(participant.id) {
            envelopes_checked += 1;
            for message in &envelope.context {
                for detail in &details {
                    assert!(
                        !message.content.contains(detail.as_str()),
                        "audit detail {detail:?} reached {}'s context: {}",
                        participant.display_name,
                        message.content
                    );
                }
                for word in OPERATIONAL_VOCABULARY {
                    assert!(
                        !message
                            .content
                            .to_lowercase()
                            .contains(&word.to_lowercase()),
                        "operational vocabulary {word:?} reached {}'s context: {}",
                        participant.display_name,
                        message.content
                    );
                }
            }
        }
    }
    assert!(
        envelopes_checked >= 6,
        "expected to have inspected several envelopes, saw {envelopes_checked}"
    );
}

#[tokio::test]
async fn the_ledger_holds_only_genuine_contributions() {
    let store = InMemoryLedgerStore::new();
    let (conversation, members, bots) = troubled_conversation(&store).await;

    let driver = RoundDriver {
        store: &store,
        bots: &bots,
        oracle: &RulesOracle { max_chars: 50 },
        endpoints: endpoints(&members),
        stale_window: 0,
        poll_interval: FAST_POLL,
    };
    for round in 1..=3 {
        driver
            .run_round(conversation, Round(round))
            .await
            .expect("round completes");
    }

    let entries = store
        .entries_from(conversation, Seq::EMPTY)
        .await
        .expect("read ledger");

    assert_eq!(
        entries.len(),
        3,
        "one admitted contribution per round: the rejected, silent, and \
         unreachable participants contributed nothing"
    );
    for entry in &entries {
        assert_eq!(
            entry.content, "a genuine contribution",
            "only content a participant meant to say is in the ledger"
        );
    }
}

#[tokio::test]
async fn a_rejection_reason_is_told_to_its_author_and_to_nobody_else() {
    // Rejection feedback is legitimate — without it a participant re-proposes the
    // same point forever — but it is *about* the ledger, not *in* it, and it is
    // nobody else's business.
    let store = InMemoryLedgerStore::new();
    let (conversation, members, bots) = troubled_conversation(&store).await;

    let driver = RoundDriver {
        store: &store,
        bots: &bots,
        oracle: &RulesOracle { max_chars: 50 },
        endpoints: endpoints(&members),
        stale_window: 0,
        poll_interval: FAST_POLL,
    };
    driver
        .run_round(conversation, Round(1))
        .await
        .expect("round completes");

    let verbose = members
        .iter()
        .find(|participant| participant.display_name == "verbose")
        .expect("the verbose participant");

    assert!(
        !bots.dispositions_for(verbose.id).is_empty(),
        "the author learns its draft was refused"
    );

    for participant in &members {
        if participant.id == verbose.id {
            continue;
        }
        for disposition in bots.dispositions_for(participant.id) {
            let rendered = format!("{disposition:?}");
            assert!(
                !rendered.contains("characters"),
                "{}'s dispositions leaked another participant's rejection: {rendered}",
                participant.display_name
            );
        }
    }
}
