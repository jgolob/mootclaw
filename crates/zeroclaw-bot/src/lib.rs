//! A shared-ledger participant.
//!
//! A bot in this system is a long-lived service that holds **no authoritative
//! conversation state**. It receives the whole conversation in an envelope, works
//! at its own pace, and leaves a revisable draft in its outbox for the
//! orchestrator to retrieve.
//!
//! Everything it keeps is expendable working state: an in-progress draft, the
//! round it belongs to. Kill the process and the conversation is unharmed —
//! the next envelope reconstructs the full context from the ledger, which is the
//! single source of truth. That is what makes participants freely restartable and
//! reschedulable.

pub mod compose;
pub mod draft;
pub mod mailbox_api;
pub mod runtime_composer;

pub use compose::{AlwaysPass, Composer, Composition, FixedAnswer};
pub use draft::{DraftSlot, Outbox};
pub use mailbox_api::{BotContext, BotState, router};
pub use runtime_composer::{RuntimeComposer, to_chat_history, to_chat_message};
