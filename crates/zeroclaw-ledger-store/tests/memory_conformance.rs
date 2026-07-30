//! Runs the shared [`LedgerStore`] contract against the in-memory implementation.
//!
//! The Postgres suite runs this same function, so the two implementations cannot
//! drift apart. That matters because the orchestrator's own tests use the
//! in-memory store: if it were allowed to behave differently, every test above
//! this layer would be verifying a fiction.

use zeroclaw_ledger_store::InMemoryLedgerStore;
use zeroclaw_ledger_store::conformance::run_conformance_suite;

#[tokio::test]
async fn in_memory_store_satisfies_the_contract() {
    let store = InMemoryLedgerStore::new();
    run_conformance_suite(&store).await;
}
