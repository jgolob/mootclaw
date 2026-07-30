//! Deciding what, if anything, to contribute to a round.
//!
//! This is the seam between the mailbox protocol and whatever actually thinks.
//! The protocol does not care whether the answer came from a model, a rule, or a
//! fixed string, which is what lets a full multi-agent conversation be tested
//! end to end with no model provider in sight.

use async_trait::async_trait;
use zeroclaw_ledger::mailbox::InboxEnvelope;

/// What a participant decided to do with a round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Composition {
    /// Something worth saying. Will be sealed for retrieval.
    Contribute(String),
    /// Nothing to add.
    ///
    /// Free and normal, and better than silence: an explicit pass lets the round
    /// close as soon as everyone has answered instead of waiting out the clock.
    Pass,
}

impl Composition {
    /// Treats blank content as a pass.
    ///
    /// A model asked whether it has anything to add will often answer with
    /// whitespace or an empty string rather than declining outright. Admitting
    /// that as a contribution would put an empty turn into the ledger.
    pub fn from_answer(answer: impl Into<String>) -> Self {
        let answer = answer.into();
        if answer.trim().is_empty() {
            Composition::Pass
        } else {
            Composition::Contribute(answer)
        }
    }
}

/// Produces a participant's answer to one round.
#[async_trait]
pub trait Composer: Send + Sync + std::fmt::Debug {
    async fn compose(&self, envelope: &InboxEnvelope) -> Composition;
}

/// A participant that never has anything to add.
///
/// Useful on its own: a conversation of these exercises the whole round loop —
/// envelopes, passes, round closure — with no model calls at all.
#[derive(Debug, Default, Clone, Copy)]
pub struct AlwaysPass;

#[async_trait]
impl Composer for AlwaysPass {
    async fn compose(&self, _envelope: &InboxEnvelope) -> Composition {
        Composition::Pass
    }
}

/// A participant that always says the same thing.
#[derive(Debug, Clone)]
pub struct FixedAnswer(pub String);

#[async_trait]
impl Composer for FixedAnswer {
    async fn compose(&self, _envelope: &InboxEnvelope) -> Composition {
        Composition::from_answer(self.0.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_answers_become_passes() {
        assert_eq!(Composition::from_answer(""), Composition::Pass);
        assert_eq!(Composition::from_answer("   \n\t "), Composition::Pass);
        assert_eq!(
            Composition::from_answer("a point"),
            Composition::Contribute("a point".to_string())
        );
    }

    #[test]
    fn content_is_preserved_verbatim_including_its_whitespace() {
        // Only the emptiness check trims. Trimming the contribution itself would
        // change what a participant said on its way into the ledger.
        assert_eq!(
            Composition::from_answer("  indented\n\nparagraphs  "),
            Composition::Contribute("  indented\n\nparagraphs  ".to_string())
        );
    }
}
