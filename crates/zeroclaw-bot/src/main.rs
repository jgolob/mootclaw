//! Runs one participant's mailbox service.
//!
//! Configuration is environment-driven so a container image can be parameterised
//! per participant without rebuilding:
//!
//! - `ZEROCLAW_BOT_PARTICIPANT_ID` — this participant's UUID, as registered with
//!   the orchestrator. Required: a participant that invented its own identity
//!   would have its contributions attributed to nobody.
//! - `ZEROCLAW_BOT_BIND` — listen address, default `0.0.0.0:8080`.
//! - `ZEROCLAW_BOT_ANSWER` — a fixed answer, for smoke-testing a deployment.
//!   Absent, the participant passes every round.

use std::net::SocketAddr;
use std::sync::Arc;
use zeroclaw_bot::compose::{AlwaysPass, Composer, FixedAnswer};
use zeroclaw_bot::mailbox_api::{BotContext, BotState, router};
use zeroclaw_ledger::types::ParticipantId;

const DEFAULT_BIND: &str = "0.0.0.0:8080";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let identity = std::env::var("ZEROCLAW_BOT_PARTICIPANT_ID").map_err(
        |_| "ZEROCLAW_BOT_PARTICIPANT_ID must be set to this participant's registered UUID",
    )?;
    let identity = ParticipantId(identity.parse()?);

    let bind: SocketAddr = std::env::var("ZEROCLAW_BOT_BIND")
        .unwrap_or_else(|_| DEFAULT_BIND.to_string())
        .parse()?;

    let composer: Arc<dyn Composer> = match std::env::var("ZEROCLAW_BOT_ANSWER") {
        Ok(answer) => Arc::new(FixedAnswer(answer)),
        Err(_) => Arc::new(AlwaysPass),
    };

    let context = BotContext {
        state: Arc::new(BotState::new(identity)),
        composer,
    };

    let listener = tokio::net::TcpListener::bind(bind).await?;
    println!("participant {identity:?} listening on {bind}");
    axum::serve(listener, router(context))
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

/// Waits for Ctrl-C.
///
/// A participant carries no state worth draining — the ledger holds the
/// conversation — so shutdown only needs to stop accepting new envelopes.
async fn shutdown() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        eprintln!("failed to listen for shutdown signal: {error}");
    }
}
