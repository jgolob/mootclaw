//! Talking to participants.
//!
//! Every call is orchestrator-initiated, and **every call may fail without that
//! being an error**. A participant that is unreachable, slow, or broken is just a
//! participant that has not responded; the round barrier already knows what to do
//! with that — wait, then time it out.
//!
//! So these methods return a [`BotCallError`] the caller is expected to *record*
//! rather than propagate. If a failed delivery aborted the round, one crashed
//! container could stop a conversation everyone else is still having. And the
//! failure must go to the audit log, never into anyone's context: an agent that
//! reads "peer unreachable" will comment on it, which is how runaway loops start.

use async_trait::async_trait;
use std::time::Duration;
use zeroclaw_ledger::mailbox::{DispositionNotice, InboxEnvelope, OutboxResponse};
use zeroclaw_ledger::types::{AuditEvent, AuditKind, ConversationId, ParticipantId, Round};

/// Default per-call timeout.
///
/// Deliberately short relative to a round: these are mailbox operations, not
/// thinking. A participant accepts an envelope immediately and composes in the
/// background, so a slow *call* means a sick participant, not a thoughtful one.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a mailbox call did not succeed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BotCallError {
    #[error("participant is unreachable: {0}")]
    Unreachable(String),
    #[error("participant did not respond within the call timeout")]
    Timeout,
    #[error("participant answered with HTTP {0}")]
    Status(u16),
    #[error("participant's response could not be understood: {0}")]
    Malformed(String),
}

impl BotCallError {
    /// Renders this failure as an audit event.
    ///
    /// The detail is operational text and must never reach a participant's
    /// context — [`crate::round_driver`] writes it to the audit log only.
    pub fn to_audit(
        &self,
        conversation: ConversationId,
        participant: ParticipantId,
        round: Round,
    ) -> AuditEvent {
        let kind = match self {
            BotCallError::Timeout => AuditKind::ParticipantTimedOut,
            _ => AuditKind::ParticipantUnreachable,
        };
        AuditEvent {
            conversation_id: Some(conversation),
            participant_id: Some(participant),
            round: Some(round),
            kind,
            detail: self.to_string(),
        }
    }
}

pub type BotResult<T> = Result<T, BotCallError>;

/// Where a participant's mailbox lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BotEndpoint {
    pub participant: ParticipantId,
    /// Base URL, e.g. `http://reviewer:8080`.
    pub base_url: String,
}

/// The orchestrator's side of the mailbox protocol.
#[async_trait]
pub trait BotClient: Send + Sync {
    /// Hands a participant its context for a round.
    async fn deliver_inbox(
        &self,
        endpoint: &BotEndpoint,
        envelope: &InboxEnvelope,
    ) -> BotResult<()>;

    /// Reads a participant's current draft.
    async fn fetch_outbox(
        &self,
        endpoint: &BotEndpoint,
        conversation: ConversationId,
    ) -> BotResult<OutboxResponse>;

    /// Tells a participant what became of its draft.
    async fn notify_disposition(
        &self,
        endpoint: &BotEndpoint,
        notice: &DispositionNotice,
    ) -> BotResult<()>;
}

/// Talks to participants over HTTP.
#[derive(Debug, Clone)]
pub struct HttpBotClient {
    client: reqwest::Client,
}

impl HttpBotClient {
    /// Builds a client with the default call timeout.
    pub fn new() -> BotResult<Self> {
        Self::with_timeout(DEFAULT_CALL_TIMEOUT)
    }

    pub fn with_timeout(timeout: Duration) -> BotResult<Self> {
        reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map(|client| Self { client })
            .map_err(|error| BotCallError::Unreachable(error.to_string()))
    }
}

/// Classifies a transport failure.
fn classify(error: reqwest::Error) -> BotCallError {
    if error.is_timeout() {
        BotCallError::Timeout
    } else if error.is_decode() || error.is_body() {
        BotCallError::Malformed(error.to_string())
    } else {
        BotCallError::Unreachable(error.to_string())
    }
}

/// Turns a non-success status into an error.
fn check_status(response: reqwest::Response) -> BotResult<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        Ok(response)
    } else {
        Err(BotCallError::Status(status.as_u16()))
    }
}

#[async_trait]
impl BotClient for HttpBotClient {
    async fn deliver_inbox(
        &self,
        endpoint: &BotEndpoint,
        envelope: &InboxEnvelope,
    ) -> BotResult<()> {
        let response = self
            .client
            .put(format!("{}/inbox", endpoint.base_url.trim_end_matches('/')))
            .json(envelope)
            .send()
            .await
            .map_err(classify)?;
        check_status(response).map(|_| ())
    }

    async fn fetch_outbox(
        &self,
        endpoint: &BotEndpoint,
        conversation: ConversationId,
    ) -> BotResult<OutboxResponse> {
        let response = self
            .client
            .get(format!(
                "{}/outbox/{}",
                endpoint.base_url.trim_end_matches('/'),
                conversation.0
            ))
            .send()
            .await
            .map_err(classify)?;
        check_status(response)?
            .json()
            .await
            .map_err(|error| BotCallError::Malformed(error.to_string()))
    }

    async fn notify_disposition(
        &self,
        endpoint: &BotEndpoint,
        notice: &DispositionNotice,
    ) -> BotResult<()> {
        let response = self
            .client
            .post(format!(
                "{}/disposition",
                endpoint.base_url.trim_end_matches('/')
            ))
            .json(notice)
            .send()
            .await
            .map_err(classify)?;
        check_status(response).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn participant() -> ParticipantId {
        ParticipantId(Uuid::from_u128(2))
    }

    fn conversation() -> ConversationId {
        ConversationId(Uuid::from_u128(0xC0))
    }

    #[test]
    fn a_timeout_audits_as_a_timeout_not_as_unreachable() {
        // The distinction matters: a timeout is a participant that is up but
        // slow, and only unreachable means the container is gone.
        let event = BotCallError::Timeout.to_audit(conversation(), participant(), Round(1));
        assert_eq!(event.kind, AuditKind::ParticipantTimedOut);
        assert_eq!(event.participant_id, Some(participant()));
        assert_eq!(event.round, Some(Round(1)));
    }

    #[test]
    fn other_failures_audit_as_unreachable() {
        for error in [
            BotCallError::Unreachable("connection refused".to_string()),
            BotCallError::Status(500),
            BotCallError::Malformed("expected object".to_string()),
        ] {
            let event = error.to_audit(conversation(), participant(), Round(1));
            assert_eq!(event.kind, AuditKind::ParticipantUnreachable);
            assert!(
                !event.detail.is_empty(),
                "an operator needs to know what went wrong"
            );
        }
    }

    #[test]
    fn a_client_can_be_built() {
        assert!(HttpBotClient::new().is_ok());
    }
}
