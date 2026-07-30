//! Sequence assignment under concurrency.
//!
//! Monotonic, never-reused sequences are the foundation everything else rests
//! on: they order the conversation, they are what a draft must be current
//! against, and they are what makes each reader's projection a stable growing
//! prefix (and therefore cacheable). A duplicated or skipped sequence would not
//! announce itself — it would show up as a subtly scrambled conversation.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;
use zeroclaw_ledger::{ConversationKind, MemberRole, Round, Seq};
use zeroclaw_ledger_store::fixtures::{agent, contribution};
use zeroclaw_ledger_store::{InMemoryLedgerStore, LedgerStore};

const CONCURRENT_WRITERS: usize = 8;
const BATCH_SIZE: usize = 4;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_batches_never_reuse_a_sequence() {
    let store = Arc::new(InMemoryLedgerStore::new());
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

    // Eight writers race, each appending a batch of four.
    let mut handles = Vec::with_capacity(CONCURRENT_WRITERS);
    for writer in 0..CONCURRENT_WRITERS {
        let store = Arc::clone(&store);
        let author_id = author.id;
        handles.push(zeroclaw_spawn::spawn!(async move {
            let entries = (0..BATCH_SIZE)
                .map(|n| contribution(author_id, &format!("writer {writer} entry {n}")))
                .collect();
            store
                .append_batch(conversation, Round(writer as i64), entries)
                .await
                .expect("append batch")
        }));
    }

    let mut all_assigned = Vec::new();
    for handle in handles {
        let assigned = handle.await.expect("writer task completed");
        assert_eq!(
            assigned.len(),
            BATCH_SIZE,
            "one sequence returned per submitted entry"
        );
        // Within a batch, sequences must be contiguous and ascending: a round's
        // entries are never interleaved with another writer's.
        for pair in assigned.windows(2) {
            assert_eq!(
                pair[1].0,
                pair[0].0 + 1,
                "batch sequences must be contiguous, got {assigned:?}"
            );
        }
        all_assigned.extend(assigned);
    }

    let expected_total = CONCURRENT_WRITERS * BATCH_SIZE;
    let unique: BTreeSet<Seq> = all_assigned.iter().copied().collect();
    assert_eq!(
        unique.len(),
        expected_total,
        "every assigned sequence must be unique; got {} unique of {} assigned",
        unique.len(),
        all_assigned.len()
    );

    // No gaps: the assigned set is exactly 1..=total.
    let expected: BTreeSet<Seq> = (1..=expected_total as i64).map(Seq).collect();
    assert_eq!(unique, expected, "sequences must be gapless and 1-based");

    let head = store.head_seq(conversation).await.expect("head");
    assert_eq!(
        head,
        Seq(expected_total as i64),
        "head tracks the last entry"
    );

    let entries = store
        .entries_from(conversation, Seq::EMPTY)
        .await
        .expect("read entries");
    assert_eq!(entries.len(), expected_total, "every entry is readable");
    for pair in entries.windows(2) {
        assert!(
            pair[1].seq > pair[0].seq,
            "entries_from must return ascending sequences"
        );
    }
}

#[tokio::test]
async fn empty_conversation_starts_at_the_empty_head() {
    let store = InMemoryLedgerStore::new();
    let conversation = store
        .create_conversation(ConversationKind::Dm, Duration::from_secs(30))
        .await
        .expect("create conversation");

    assert_eq!(
        store.head_seq(conversation).await.expect("head"),
        Seq::EMPTY
    );
    assert!(
        store
            .entries_from(conversation, Seq::EMPTY)
            .await
            .expect("read entries")
            .is_empty()
    );

    let meta = store
        .conversation_meta(conversation)
        .await
        .expect("conversation meta");
    assert_eq!(meta.kind, ConversationKind::Dm);
    assert_eq!(meta.round_timeout, Duration::from_secs(30));
    assert_eq!(meta.head_seq, Seq::EMPTY);
    assert_eq!(meta.visible_from, Seq::EMPTY);
}
