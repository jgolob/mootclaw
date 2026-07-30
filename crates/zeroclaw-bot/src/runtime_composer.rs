//! Composing with the ZeroClaw agent runtime.
//!
//! This is the seam where a shared-ledger participant becomes a real agent: it
//! gets tools, memory, risk profiles, MCP servers — everything a configured
//! ZeroClaw agent has — while still holding no authoritative conversation state.
//!
//! The context it thinks with is the projected ledger, handed to the runtime
//! verbatim through [`process_history`]. That path exists precisely so nothing is
//! added to it: the ordinary [`process_message`](zeroclaw_runtime::agent::process_message)
//! path stamps the local time onto the user turn, which would change the prompt
//! prefix on every turn and silently destroy provider-side prompt caching.

use crate::compose::{Composer, Composition};
use async_trait::async_trait;
use zeroclaw_api::ingress::TurnOrigin;
use zeroclaw_api::model_provider::ChatMessage;
use zeroclaw_config::schema::Config;
use zeroclaw_ledger::mailbox::InboxEnvelope;
use zeroclaw_ledger::projection::ProjectedMessage;

/// Converts a projected message into the runtime's wire type.
///
/// A plain function rather than a `From` impl: `ProjectedMessage` belongs to
/// `zeroclaw-ledger`, which cannot depend on `zeroclaw-api` without losing the
/// purity that keeps projection testable with no runtime — and `ChatMessage`
/// belongs to `zeroclaw-api`, so the orphan rule rules out a third crate
/// implementing the conversion.
pub fn to_chat_message(message: &ProjectedMessage) -> ChatMessage {
    ChatMessage {
        role: message.role.as_str().to_string(),
        content: message.content.clone(),
    }
}

/// Converts a whole projected context.
pub fn to_chat_history(context: &[ProjectedMessage]) -> Vec<ChatMessage> {
    context.iter().map(to_chat_message).collect()
}

/// Composes using a configured ZeroClaw agent.
#[derive(Debug, Clone)]
pub struct RuntimeComposer {
    config: Config,
    /// The configured agent this participant runs as.
    agent_alias: String,
}

impl RuntimeComposer {
    pub fn new(config: Config, agent_alias: impl Into<String>) -> Self {
        Self {
            config,
            agent_alias: agent_alias.into(),
        }
    }
}

#[async_trait]
impl Composer for RuntimeComposer {
    async fn compose(&self, envelope: &InboxEnvelope) -> Composition {
        let mut history = Vec::with_capacity(envelope.context.len() + 1);

        // The soul is already rendered into the projected context as a framed,
        // untrusted segment; nothing extra is prepended here.
        history.extend(to_chat_history(&envelope.context));

        // The scratchpad is this participant's private working memory. It rides
        // at the end, after the ledger: it changes between rounds, and volatile
        // content belongs behind the stable prefix so it costs one uncached tail
        // rather than the whole conversation.
        if let Some(scratchpad) = envelope.scratchpad.as_deref() {
            let scratchpad = scratchpad.trim();
            if !scratchpad.is_empty() {
                history.push(ChatMessage::user(format!(
                    "[scratchpad] Your private working notes for this conversation. \
                     Not visible to anyone else.\n\n{scratchpad}"
                )));
            }
        }

        // `TurnOrigin::AgentDirect` because a round is peer-driven: the trigger
        // is other participants' contributions, not an operator instruction.
        let outcome = zeroclaw_runtime::agent::process_history(
            self.config.clone(),
            &self.agent_alias,
            history,
            None,
            TurnOrigin::AgentDirect,
        )
        .await;

        match outcome {
            Ok(answer) => Composition::from_answer(answer),
            // A failed turn is a pass, not a contribution. The failure is
            // operational and belongs in the orchestrator's audit log — it must
            // never become a ledger entry, because an agent that reads "peer
            // errored" will comment on it, and that is how runaways start.
            Err(_) => Composition::Pass,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_ledger::projection::ProjectedRole;

    #[test]
    fn roles_survive_the_conversion() {
        for (role, expected) in [
            (ProjectedRole::System, "system"),
            (ProjectedRole::User, "user"),
            (ProjectedRole::Assistant, "assistant"),
        ] {
            let converted = to_chat_message(&ProjectedMessage {
                role,
                content: "text".to_string(),
            });
            assert_eq!(converted.role, expected);
            assert_eq!(converted.content, "text");
        }
    }

    #[test]
    fn conversion_preserves_order_and_content_verbatim() {
        // The projected context is the cached prompt prefix; converting it must
        // not reorder, trim, or annotate anything.
        let context = vec![
            ProjectedMessage {
                role: ProjectedRole::User,
                content: "  [human:jonathan] leading space kept  ".to_string(),
            },
            ProjectedMessage {
                role: ProjectedRole::Assistant,
                content: "mine".to_string(),
            },
        ];
        let converted = to_chat_history(&context);
        assert_eq!(converted.len(), 2);
        assert_eq!(
            converted[0].content,
            "  [human:jonathan] leading space kept  "
        );
        assert_eq!(converted[1].role, "assistant");
    }

    #[test]
    fn an_empty_context_converts_to_an_empty_history() {
        assert!(to_chat_history(&[]).is_empty());
    }
}
