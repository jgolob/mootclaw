//! Persistence for the shared conversation ledger.
//!
//! [`LedgerStore`] is the whole storage surface. Two properties of it are worth
//! stating up front, because the rest of the system leans on them:
//!
//! - **One write path for content.** [`LedgerStore::append_batch`] is the only
//!   way a contribution enters a ledger, and only the orchestrator calls it.
//!   Participants propose; they never touch a store.
//! - **The two logs cannot cross.** Operational events go through
//!   [`LedgerStore::append_audit`], which has no way to reach ledger content, and
//!   [`LedgerStore::entries_from`] never returns one. An agent that reads "peer
//!   timed out" will comment on it, so the separation is structural rather than a
//!   convention to remember.
//!
//! The pure domain and decision logic lives in `zeroclaw-ledger`, which forbids
//! itself async runtimes and database clients. I/O belongs here.

pub mod conformance;
pub mod fixtures;
pub mod memory;
pub mod postgres;
pub mod store;

pub use memory::InMemoryLedgerStore;
pub use postgres::PgLedgerStore;
pub use store::{
    ConversationMeta, LedgerStore, ProposalRecord, Result, StoreError, StoredAuditEvent,
};
