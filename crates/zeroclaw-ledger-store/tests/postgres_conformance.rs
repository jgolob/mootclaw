//! Runs the shared [`LedgerStore`] contract against Postgres.
//!
//! This is the same `run_conformance_suite` the in-memory store passes, run
//! verbatim. That equivalence is what lets the orchestrator's fast in-memory
//! tests stand for real behaviour: if the two implementations could drift, every
//! test above this layer would be verifying a fiction.
//!
//! Requires a database. Bring one up with:
//!
//! ```text
//! docker compose -f deploy/ledger/docker-compose.yml up -d ledger-db
//! export LEDGER_TEST_DATABASE_URL=postgres://zeroclaw:zeroclaw@localhost:5433/zeroclaw_ledger
//! ```
//!
//! Without `LEDGER_TEST_DATABASE_URL` these tests skip with an explanation
//! rather than failing: a developer without Docker running should not see a red
//! suite for infrastructure they were never asked to start.

use std::sync::Arc;
use std::time::Duration;
use zeroclaw_ledger::types::{ConversationKind, MemberRole, Round, Seq};
use zeroclaw_ledger_store::conformance::run_conformance_suite;
use zeroclaw_ledger_store::fixtures::{agent, contribution};
use zeroclaw_ledger_store::{LedgerStore, PgLedgerStore};

/// Connects, or explains why the test is being skipped.
async fn store() -> Option<PgLedgerStore> {
    let Ok(url) = std::env::var("LEDGER_TEST_DATABASE_URL") else {
        eprintln!(
            "skipping: LEDGER_TEST_DATABASE_URL is unset. Start one with \
             `docker compose -f deploy/ledger/docker-compose.yml up -d ledger-db`"
        );
        return None;
    };
    match PgLedgerStore::connect(&url).await {
        Ok(store) => Some(store),
        Err(error) => panic!(
            "LEDGER_TEST_DATABASE_URL is set but unusable: {error}. \
             Is the database up and healthy?"
        ),
    }
}

#[tokio::test]
async fn postgres_satisfies_the_contract() {
    let Some(store) = store().await else {
        return;
    };
    run_conformance_suite(&store).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writers_never_reuse_a_sequence() {
    // The property the whole design rests on, tested against the real thing.
    // In-memory it is a mutex; here it is a transaction bumping `head_seq` in
    // the same statement that reads it. Only a live database can show that the
    // second one actually holds.
    let Some(store) = store().await else {
        return;
    };
    let store = Arc::new(store);

    let conversation = store
        .create_conversation(ConversationKind::Group, Duration::from_secs(60))
        .await
        .expect("create conversation");
    let author = agent("writer");
    store
        .upsert_participant(author.clone())
        .await
        .expect("register participant");
    store
        .add_member(conversation, author.id, MemberRole::Contributor)
        .await
        .expect("add member");

    const WRITERS: usize = 8;
    const BATCH: usize = 4;

    let mut handles = Vec::with_capacity(WRITERS);
    for writer in 0..WRITERS {
        let store = Arc::clone(&store);
        let author_id = author.id;
        handles.push(zeroclaw_spawn::spawn!(async move {
            let entries = (0..BATCH)
                .map(|n| contribution(author_id, &format!("writer {writer} entry {n}")))
                .collect();
            store
                .append_batch(conversation, Round(writer as i64), entries)
                .await
                .expect("append batch")
        }));
    }

    let mut all = Vec::new();
    for handle in handles {
        let assigned = handle.await.expect("writer completed");
        assert_eq!(assigned.len(), BATCH);
        // Contiguous within a batch: a round's entries are never interleaved
        // with another writer's, which is what the transaction guarantees.
        for pair in assigned.windows(2) {
            assert_eq!(
                pair[1].0,
                pair[0].0 + 1,
                "batch sequences must be contiguous, got {assigned:?}"
            );
        }
        all.extend(assigned);
    }

    let total = WRITERS * BATCH;
    let unique: std::collections::BTreeSet<Seq> = all.iter().copied().collect();
    assert_eq!(unique.len(), total, "every sequence must be unique");
    let expected: std::collections::BTreeSet<Seq> = (1..=total as i64).map(Seq).collect();
    assert_eq!(unique, expected, "sequences must be gapless and 1-based");

    assert_eq!(
        store.head_seq(conversation).await.expect("head"),
        Seq(total as i64)
    );
}

#[tokio::test]
async fn the_schema_applies_only_once() {
    // Migration is idempotent, so several orchestrators starting at once against
    // one database is a normal event rather than a race.
    let Some(_first) = store().await else {
        return;
    };
    let Some(_second) = store().await else {
        return;
    };
    // Reaching here means the second connect did not fail applying the schema.
}
