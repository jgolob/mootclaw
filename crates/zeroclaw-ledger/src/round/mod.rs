//! The round engine: pacing, currency, and admission.
//!
//! A conversation advances in rounds. Each opens at the current ledger head, every
//! contributor gets the same window to seal a draft or pass, and the admitted
//! contributions land together as a batch — so participants within a round never
//! saw one another's drafts. Simultaneous reveal, as in a real panel round.
//!
//! Every decision here is a pure function of values handed in. Time arrives as an
//! explicit `now`, never from a clock; the oracle's asynchronous judgement happens
//! in the caller, between [`plan_admission`] and [`finalize_admission`]. That is
//! what lets the whole engine be tested exhaustively with no database, no
//! container, and no model provider.

pub mod admission;
pub mod currency;
pub mod machine;

pub use admission::{
    AdmissionOutcome, AdmissionPlan, RefusedDraft, admitted_dispositions, finalize_admission,
    plan_admission,
};
pub use currency::{CurrencyOutcome, DEFAULT_STALE_WINDOW, validate_currency};
pub use machine::{DEFAULT_ROUND_TIMEOUT, RoundCloseReason, RoundError, RoundMachine, RoundStatus};
