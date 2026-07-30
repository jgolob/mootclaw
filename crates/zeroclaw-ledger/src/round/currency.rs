//! Whether a draft was composed against a current view of the conversation.
//!
//! A participant works asynchronously: it receives the ledger, thinks, runs
//! tools, and eventually seals a draft. Meanwhile the conversation may have
//! moved. A draft written against sequence 40 and retrieved at 43 may already be
//! redundant, answered, or a non-sequitur.
//!
//! The default is strict — a draft must be composed against the current head. It
//! is better to slow down and have every participant actually contemplate the
//! most recent contribution than to let fast participants pile on stale points.

use crate::types::Seq;

/// Default staleness tolerance: none.
///
/// Every contribution must have been composed with the newest entry in view.
pub const DEFAULT_STALE_WINDOW: i64 = 0;

/// Verdict on a draft's currency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CurrencyOutcome {
    /// Composed recently enough to stand.
    Current,
    /// The ledger moved on. The author should reconsider against `current_seq`.
    Stale { current_seq: Seq },
}

/// Judges whether a draft is current enough to be admitted.
///
/// `stale_window` is how many sequences the ledger may have advanced since the
/// draft was composed. Zero — [`DEFAULT_STALE_WINDOW`] — means the draft must be
/// composed against the head exactly. A negative window is treated as zero
/// rather than rejecting everything, since "stricter than exact" has no meaning.
///
/// A draft claiming to be composed against a sequence *ahead* of the head is
/// impossible: sequences come from the store, and a participant cannot know a
/// future one. It is reported [`CurrencyOutcome::Stale`] rather than accepted or
/// panicked on, because the honest response to an impossible claim is to refuse
/// it and let the author recompose against a sequence that exists.
pub fn validate_currency(draft_seq: Seq, head_seq: Seq, stale_window: i64) -> CurrencyOutcome {
    let window = stale_window.max(0);

    if draft_seq > head_seq {
        return CurrencyOutcome::Stale {
            current_seq: head_seq,
        };
    }

    // Saturating: a ledger near i64::MAX must not wrap into looking current.
    let drift = head_seq.0.saturating_sub(draft_seq.0);
    if drift <= window {
        CurrencyOutcome::Current
    } else {
        CurrencyOutcome::Stale {
            current_seq: head_seq,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stale(seq: i64) -> CurrencyOutcome {
        CurrencyOutcome::Stale {
            current_seq: Seq(seq),
        }
    }

    #[test]
    fn strict_by_default_requires_the_exact_head() {
        let cases = [
            (Seq(10), Seq(10), CurrencyOutcome::Current),
            (Seq(9), Seq(10), stale(10)),
            (Seq(0), Seq(10), stale(10)),
        ];
        for (draft, head, expected) in cases {
            assert_eq!(
                validate_currency(draft, head, DEFAULT_STALE_WINDOW),
                expected,
                "draft {draft:?} against head {head:?}"
            );
        }
    }

    #[test]
    fn a_window_tolerates_exactly_that_much_drift() {
        // Window of 2: heads 10, 11, and 12 are acceptable for a draft at 10.
        assert_eq!(
            validate_currency(Seq(10), Seq(12), 2),
            CurrencyOutcome::Current,
            "drift equal to the window is inside it"
        );
        assert_eq!(
            validate_currency(Seq(10), Seq(13), 2),
            stale(13),
            "drift beyond the window is stale"
        );
    }

    #[test]
    fn an_empty_ledger_accepts_a_draft_composed_against_nothing() {
        // The first round of a conversation: the head is EMPTY and every
        // participant composed against it.
        assert_eq!(
            validate_currency(Seq::EMPTY, Seq::EMPTY, DEFAULT_STALE_WINDOW),
            CurrencyOutcome::Current
        );
    }

    #[test]
    fn a_draft_ahead_of_the_head_is_refused() {
        // Impossible: sequences are assigned by the store, so no participant can
        // have seen a future one. Refuse rather than trust it.
        assert_eq!(validate_currency(Seq(11), Seq(10), 0), stale(10));
        assert_eq!(
            validate_currency(Seq(999), Seq(10), 1000),
            stale(10),
            "a generous window does not legitimise an impossible sequence"
        );
    }

    #[test]
    fn a_negative_window_behaves_as_strict() {
        assert_eq!(
            validate_currency(Seq(10), Seq(10), -5),
            CurrencyOutcome::Current
        );
        assert_eq!(validate_currency(Seq(9), Seq(10), -5), stale(10));
    }

    #[test]
    fn extreme_sequences_do_not_wrap() {
        assert_eq!(
            validate_currency(Seq(i64::MIN), Seq(i64::MAX), 0),
            stale(i64::MAX),
            "a saturating subtraction must not wrap into looking current"
        );
        assert_eq!(
            validate_currency(Seq(i64::MAX), Seq(i64::MAX), 0),
            CurrencyOutcome::Current
        );
    }
}
