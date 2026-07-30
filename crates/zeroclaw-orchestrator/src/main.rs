//! Runs the orchestrator: owns the ledger, drives rounds, serves the view API.
//!
//! ```text
//! zeroclaw-orchestrator path/to/orchestrator.toml
//! ```
//!
//! Participants are addressed by mailbox; humans arrive through the view API.
//! Neither ever receives a database connection string — the orchestrator is the
//! sole writer, and that is enforced by who holds the credential as much as by
//! the code.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use zeroclaw_ledger::types::{
    ConversationKind, MemberRole, Participant, ParticipantId, ParticipantKind, Round,
};
use zeroclaw_ledger_store::PgLedgerStore;
use zeroclaw_ledger_store::store::LedgerStore;
use zeroclaw_orchestrator::bot_client::{BotEndpoint, HttpBotClient};
use zeroclaw_orchestrator::config::OrchestratorConfig;
use zeroclaw_orchestrator::human_desk::HumanDesk;
use zeroclaw_orchestrator::oracle::RulesOracle;
use zeroclaw_orchestrator::round_driver::RoundDriver;
use zeroclaw_orchestrator::view_api::{ViewContext, router};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: zeroclaw-orchestrator <config.toml>")?;
    let config = OrchestratorConfig::load(std::path::Path::new(&path))?;

    let store = Arc::new(PgLedgerStore::connect(&config.database_url).await?);
    let humans = Arc::new(HumanDesk::new());

    // One conversation for now. Participant identities are derived from their
    // configured names so a restart rejoins the same conversation rather than
    // starting a new one with new strangers in it.
    let conversation = store
        .create_conversation(
            ConversationKind::Group,
            Duration::from_secs(config.round_timeout_secs),
        )
        .await?;

    let mut endpoints: BTreeMap<ParticipantId, BotEndpoint> = BTreeMap::new();
    for entry in &config.participants {
        let kind = match entry.kind.as_str() {
            "agent" => ParticipantKind::Agent,
            "human" => ParticipantKind::Human,
            other => return Err(format!("unknown participant kind {other:?}").into()),
        };
        // A stable id from a stable name: the same participant across restarts.
        let id = ParticipantId(uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_URL,
            entry.name.as_bytes(),
        ));
        store
            .upsert_participant(Participant {
                id,
                kind,
                display_name: entry.name.clone(),
                agent_alias: matches!(kind, ParticipantKind::Agent).then(|| entry.name.clone()),
            })
            .await?;

        let role = match entry.role.as_deref() {
            Some("observer") => MemberRole::Observer,
            _ => MemberRole::Contributor,
        };
        store.add_member(conversation, id, role).await?;

        if let Some(url) = &entry.url {
            endpoints.insert(
                id,
                BotEndpoint {
                    participant: id,
                    base_url: url.clone(),
                },
            );
        }
    }

    println!("conversation {}", conversation.0);
    println!("view api     http://{}", config.bind);
    println!("participants {}", config.participants.len());

    // The round loop and the view API run side by side: a client can watch and
    // contribute while the panel is mid-round.
    let rounds = {
        let store = Arc::clone(&store);
        let humans = Arc::clone(&humans);
        let config = config.clone();
        zeroclaw_spawn::spawn!(async move {
            let bots = match HttpBotClient::new() {
                Ok(client) => client,
                Err(error) => {
                    eprintln!("cannot build mailbox client: {error}");
                    return;
                }
            };
            let oracle = RulesOracle {
                max_chars: config.max_contribution_chars,
            };
            let driver = RoundDriver {
                store: store.as_ref(),
                bots: &bots,
                oracle: &oracle,
                endpoints,
                humans: humans.as_ref(),
                stale_window: config.stale_window,
                poll_interval: Duration::from_secs_f64(config.poll_secs),
            };

            for round in 1.. {
                match driver.run_round(conversation, Round(round)).await {
                    Ok(report) => println!(
                        "round {round}: admitted {}, head {}",
                        report.admitted.len(),
                        report.head_after.0
                    ),
                    Err(error) => {
                        // A failed round is operational. Report it and carry on:
                        // stopping the conversation because one round went wrong
                        // is worse than the round going wrong.
                        eprintln!("round {round} failed: {error}");
                    }
                }
            }
        })
    };

    let context = ViewContext {
        store: Arc::clone(&store) as Arc<dyn LedgerStore>,
        humans: Arc::clone(&humans),
    };
    let listener = tokio::net::TcpListener::bind(&config.bind).await?;
    axum::serve(listener, router(context))
        .with_graceful_shutdown(async {
            if let Err(error) = tokio::signal::ctrl_c().await {
                eprintln!("failed to listen for shutdown signal: {error}");
            }
        })
        .await?;

    rounds.abort();
    Ok(())
}
