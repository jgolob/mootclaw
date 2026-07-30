//! Orchestrator configuration.
//!
//! Names the conversation and its members, and where each agent's mailbox lives.
//! Humans have no address — they arrive through the view API.

use serde::{Deserialize, Serialize};
use std::path::Path;
use zeroclaw_ledger::round::machine::DEFAULT_ROUND_TIMEOUT;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestratorConfig {
    /// Postgres connection string. The orchestrator is its only holder:
    /// participants never receive one.
    pub database_url: String,
    /// Where the view API listens.
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Seconds each participant gets to respond in a round.
    #[serde(default = "default_round_timeout_secs")]
    pub round_timeout_secs: u64,
    /// How far the ledger may have moved since a draft was composed. Zero means
    /// a contribution must have been written with the newest entry in view.
    #[serde(default)]
    pub stale_window: i64,
    /// Seconds between outbox polls while a round is open.
    #[serde(default = "default_poll_secs")]
    pub poll_secs: f64,
    /// Longest admissible contribution.
    #[serde(default = "default_max_chars")]
    pub max_contribution_chars: usize,
    pub participants: Vec<ParticipantConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParticipantConfig {
    /// Stable name. Used for attribution and, for agents, as the alias.
    pub name: String,
    /// `agent` or `human`.
    pub kind: String,
    /// Mailbox base URL. Required for agents, absent for humans.
    #[serde(default)]
    pub url: Option<String>,
    /// `contributor` (default) or `observer`.
    #[serde(default)]
    pub role: Option<String>,
}

fn default_bind() -> String {
    "127.0.0.1:8088".to_string()
}

fn default_round_timeout_secs() -> u64 {
    DEFAULT_ROUND_TIMEOUT.as_secs()
}

fn default_poll_secs() -> f64 {
    0.5
}

fn default_max_chars() -> usize {
    8_000
}

impl OrchestratorConfig {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        toml::from_str(&text).map_err(|error| format!("cannot parse {}: {error}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minimal_config_parses_with_sensible_defaults() {
        let config: OrchestratorConfig = toml::from_str(
            r#"
            database_url = "postgres://localhost/ledger"

            [[participants]]
            name = "reviewer"
            kind = "agent"
            url = "http://reviewer:8080"

            [[participants]]
            name = "jonathan"
            kind = "human"
            "#,
        )
        .expect("parse");

        assert_eq!(config.participants.len(), 2);
        assert_eq!(config.round_timeout_secs, 120, "two minutes by default");
        assert_eq!(
            config.stale_window, 0,
            "a contribution must be current by default"
        );
        assert!(
            config.participants[1].url.is_none(),
            "humans arrive through the view API, not a mailbox"
        );
    }
}
