//! The orchestrator: custodian of the shared conversation ledger.
//!
//! It owns the store and is the **sole writer**. Participants propose
//! contributions through their mailboxes; nothing they send reaches the ledger
//! except by the orchestrator's decision, and every operational event goes to an
//! audit log that no projection can read.
//!
//! A conversation advances in rounds. Each opens at the current ledger head,
//! every contributor gets the same window to seal a draft or pass, and the
//! admitted contributions land together — so participants within a round never
//! saw one another's drafts.

pub mod bot_client;
pub mod config;
pub mod human_desk;
pub mod mock;
pub mod oracle;
pub mod round_driver;
pub mod view_api;

pub use bot_client::{BotCallError, BotClient, BotEndpoint, HttpBotClient};
pub use human_desk::HumanDesk;
pub use mock::{MockBehaviour, MockBotClient};
pub use oracle::{AdmitAll, Oracle, RulesOracle};
pub use round_driver::{DriverError, RoundDriver, RoundReport};
