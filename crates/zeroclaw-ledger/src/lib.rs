//! Shared conversation ledger for multi-agent discussion.
//!
//! One ledger is the single source of truth for a conversation and *is* the
//! context window of every participant — human and agent alike. A single
//! orchestrator owns the storage and is the only writer; participants propose
//! contributions and the orchestrator admits them.
//!
//! # Invariants this crate exists to protect
//!
//! - **Sole writer.** Participants propose; only the orchestrator appends.
//! - **Two logs.** The ledger holds only contributions a participant meant to
//!   make. Operational events — timeouts, container exits, retries, rejections,
//!   spend — live in the audit log and are never projected into anyone's
//!   context. An agent that reads "peer timed out" will helpfully comment on it,
//!   which is how runaway loops start.
//! - **Hard attribution.** Every entry names its author. There are no anonymous
//!   rows, agent-to-agent included.
//! - **Byte-stable projection.** The same ledger prefix rendered for the same
//!   reader must produce byte-identical output across processes and restarts.
//!   Provider-side prompt caching depends on it, so drift here does not fail
//!   loudly — it just multiplies cost.
//! - **Logically append-only.** Sequences come from the orchestrator, never from
//!   a clock or a participant. Containment tombstones; it never deletes.
//!
//! This crate is deliberately pure: domain types plus decision logic that takes
//! values in and returns values out. Storage, HTTP, and the agent runtime live
//! in the crates above it.

pub mod projection;
pub mod types;

pub use projection::{
    ParticipantIndex, ProjectedMessage, ProjectedRole, ProjectionError, order_round_batch,
    project_for_reader,
};

pub use types::{
    AuditEvent, AuditKind, ConversationId, ConversationKind, Disposition, Draft, DraftId,
    DraftState, DraftVersion, EntryKind, LedgerEntry, MemberRole, NewEntry, OracleVerdict,
    Participant, ParticipantId, ParticipantKind, Round, RoundDisposition, Seq,
};
