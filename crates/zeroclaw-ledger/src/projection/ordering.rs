//! Deterministic ordering of a round's admitted contributions.
//!
//! Everyone in a round composes against the same head and does not see the
//! others' drafts, so the order they land in is a choice the orchestrator makes
//! rather than something the conversation dictates. That choice must be a pure
//! function of the drafts themselves.
//!
//! If it were retrieval order, two orchestrators replaying the same round — or
//! the same orchestrator after a restart, or one whose HTTP calls happened to
//! return in a different sequence — would produce different ledgers from
//! identical inputs. Every reader's projection would diverge, and because
//! provider-side prompt caching keys on the prompt prefix, the failure would show
//! up as a quietly larger bill rather than as a broken test.

use crate::types::Draft;

/// Orders a round's drafts deterministically, by author.
///
/// Sorts on [`ParticipantId`](crate::types::ParticipantId) alone — never a clock,
/// retrieval order, content, or map iteration order. The sort is stable, so the
/// (invalid) case of two drafts from one author preserves their relative order
/// instead of choosing arbitrarily between them.
pub fn order_round_batch(mut drafts: Vec<Draft>) -> Vec<Draft> {
    drafts.sort_by_key(|draft| draft.participant_id);
    drafts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ConversationId, DraftId, DraftState, DraftVersion, ParticipantId, Seq};
    use uuid::Uuid;

    fn draft(participant: u128, content: &str) -> Draft {
        Draft {
            id: DraftId(Uuid::from_u128(participant + 1000)),
            conversation_id: ConversationId(Uuid::from_u128(1)),
            participant_id: ParticipantId(Uuid::from_u128(participant)),
            content: content.to_string(),
            composed_against_seq: Seq(7),
            version: DraftVersion(1),
            state: DraftState::Sealed,
        }
    }

    /// Every distinct permutation of a 4-element slice, by index.
    fn permutations() -> Vec<[usize; 4]> {
        let mut out = Vec::new();
        for a in 0..4 {
            for b in 0..4 {
                for c in 0..4 {
                    for d in 0..4 {
                        let candidate = [a, b, c, d];
                        let mut seen = [false; 4];
                        if candidate.iter().all(|i| {
                            let fresh = !seen[*i];
                            seen[*i] = true;
                            fresh
                        }) {
                            out.push(candidate);
                        }
                    }
                }
            }
        }
        out
    }

    #[test]
    fn every_input_order_produces_one_output_order() {
        let drafts = [
            draft(3, "gamma"),
            draft(1, "alpha"),
            draft(4, "delta"),
            draft(2, "beta"),
        ];

        let orders = permutations();
        assert_eq!(orders.len(), 24, "all permutations of four drafts");

        let expected = vec!["alpha", "beta", "gamma", "delta"];
        for order in orders {
            let shuffled: Vec<Draft> = order.iter().map(|i| drafts[*i].clone()).collect();
            let ordered = order_round_batch(shuffled);
            let contents: Vec<&str> = ordered.iter().map(|d| d.content.as_str()).collect();
            assert_eq!(
                contents, expected,
                "input order {order:?} must not affect output order"
            );
        }
    }

    #[test]
    fn ordering_is_idempotent() {
        let drafts = vec![draft(2, "beta"), draft(1, "alpha")];
        let once = order_round_batch(drafts);
        let twice = order_round_batch(once.clone());
        assert_eq!(once, twice, "re-ordering an ordered batch changes nothing");
    }

    #[test]
    fn same_author_keeps_relative_order() {
        // Not a state the round engine should produce — one draft per author per
        // round — but a stable sort means the outcome is defined rather than
        // arbitrary if it ever happens.
        let ordered = order_round_batch(vec![
            draft(1, "first"),
            draft(1, "second"),
            draft(0, "earlier author"),
        ]);
        let contents: Vec<&str> = ordered.iter().map(|d| d.content.as_str()).collect();
        assert_eq!(contents, vec!["earlier author", "first", "second"]);
    }

    #[test]
    fn empty_and_single_batches_are_handled() {
        assert!(order_round_batch(Vec::new()).is_empty());
        let single = order_round_batch(vec![draft(1, "only")]);
        assert_eq!(single.len(), 1);
    }
}
