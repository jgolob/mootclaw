//! The round state machine.
//!
//! A conversation advances in rounds, not per contribution. A round opens at the
//! current ledger head, every contributor gets the same window to seal a draft or
//! pass, and only when all have responded — or the window expires — do the
//! admitted contributions land together.
//!
//! The pacing is deliberate: no zerg, Civ. Without the barrier, strict currency
//! would starve slow participants, since a fast one could keep advancing the head
//! and invalidating everyone else's work in progress. That would select for
//! exactly the reflexive, low-quality contributions the design is trying to
//! avoid.
//!
//! This type is pure. Time enters only as an explicit `now` argument, never from
//! a clock, so every scheduling behaviour is table-testable without waiting.

use crate::types::{ParticipantId, Round, RoundDisposition};
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;
use std::time::Duration;

/// Default window a participant gets to respond in a round.
///
/// Two minutes: long enough for a considered contribution including tool use,
/// short enough that one unresponsive participant does not stall a conversation.
/// A participant with nothing to add should pass immediately rather than run the
/// clock down — passing is free and closes the round sooner.
pub const DEFAULT_ROUND_TIMEOUT: Duration = Duration::from_secs(120);

/// Why a round stopped accepting responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundCloseReason {
    /// Every contributor sealed or passed. The common case, and the fast one.
    AllResponded,
    /// The window expired with contributors still thinking.
    TimedOut,
}

/// Whether a round is still accepting responses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoundStatus {
    Open {
        /// Contributors who have not yet responded, in [`ParticipantId`] order.
        pending: Vec<ParticipantId>,
    },
    Closed {
        reason: RoundCloseReason,
    },
}

/// Why a response was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RoundError {
    /// The responder is not enrolled in this round. Observers are never enrolled,
    /// so this also covers an observer attempting to contribute.
    #[error("participant is not a contributor in this round")]
    NotEnrolled,
    /// The round already closed. The response arrived too late to count.
    #[error("the round has already closed")]
    Closed,
}

/// Tracks one round's responses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoundMachine {
    round: Round,
    deadline: DateTime<Utc>,
    /// `BTreeMap` so pending lists and final dispositions come out in
    /// [`ParticipantId`] order without anyone remembering to sort.
    dispositions: BTreeMap<ParticipantId, RoundDisposition>,
}

impl RoundMachine {
    /// Opens a round enrolling `contributors`, every one of them pending.
    ///
    /// Only contributors are enrolled. An observer receives context but must
    /// never hold a round open, so it is simply absent here.
    pub fn open(round: Round, contributors: &[ParticipantId], deadline: DateTime<Utc>) -> Self {
        Self {
            round,
            deadline,
            dispositions: contributors
                .iter()
                .map(|id| (*id, RoundDisposition::Pending))
                .collect(),
        }
    }

    pub fn round(&self) -> Round {
        self.round
    }

    pub fn deadline(&self) -> DateTime<Utc> {
        self.deadline
    }

    /// Records that `participant` has a contribution ready for retrieval.
    ///
    /// Idempotent and revisable: a participant may seal, revise, and reseal
    /// freely until the round closes, and may seal after passing if it changes
    /// its mind. The last response before close is the one that counts.
    pub fn record_seal(
        &mut self,
        participant: ParticipantId,
        now: DateTime<Utc>,
    ) -> Result<(), RoundError> {
        self.record(participant, RoundDisposition::Sealed, now)
    }

    /// Records that `participant` has nothing to add this round.
    pub fn record_pass(
        &mut self,
        participant: ParticipantId,
        now: DateTime<Utc>,
    ) -> Result<(), RoundError> {
        self.record(participant, RoundDisposition::Passed, now)
    }

    fn record(
        &mut self,
        participant: ParticipantId,
        disposition: RoundDisposition,
        now: DateTime<Utc>,
    ) -> Result<(), RoundError> {
        if !self.dispositions.contains_key(&participant) {
            return Err(RoundError::NotEnrolled);
        }
        if matches!(self.status(now), RoundStatus::Closed { .. }) {
            return Err(RoundError::Closed);
        }
        self.dispositions.insert(participant, disposition);
        Ok(())
    }

    /// Whether the round is still open, as of `now`.
    ///
    /// Computed rather than stored, so a round cannot be "open" in memory while
    /// its deadline has passed. All-responded is checked before the deadline, so
    /// a round that filled up exactly as time ran out closes as
    /// [`RoundCloseReason::AllResponded`] — the participants did respond.
    pub fn status(&self, now: DateTime<Utc>) -> RoundStatus {
        let pending: Vec<ParticipantId> = self
            .dispositions
            .iter()
            .filter(|(_, disposition)| matches!(disposition, RoundDisposition::Pending))
            .map(|(id, _)| *id)
            .collect();

        if pending.is_empty() {
            return RoundStatus::Closed {
                reason: RoundCloseReason::AllResponded,
            };
        }
        if now >= self.deadline {
            return RoundStatus::Closed {
                reason: RoundCloseReason::TimedOut,
            };
        }
        RoundStatus::Open { pending }
    }

    /// Contributors who have sealed a contribution, in [`ParticipantId`] order.
    ///
    /// Whose outboxes the orchestrator should claim.
    pub fn sealed(&self) -> Vec<ParticipantId> {
        self.dispositions
            .iter()
            .filter(|(_, disposition)| matches!(disposition, RoundDisposition::Sealed))
            .map(|(id, _)| *id)
            .collect()
    }

    /// The round's dispositions as they should be recorded, given `now`.
    ///
    /// Anyone still pending at a timed-out close is reported
    /// [`RoundDisposition::TimedOut`] rather than [`RoundDisposition::Passed`]:
    /// both let the round proceed, but silence and non-response are different
    /// facts, and only one of them says something about a participant's health.
    pub fn final_dispositions(&self, now: DateTime<Utc>) -> Vec<(ParticipantId, RoundDisposition)> {
        let timed_out = matches!(
            self.status(now),
            RoundStatus::Closed {
                reason: RoundCloseReason::TimedOut
            }
        );
        self.dispositions
            .iter()
            .map(|(id, disposition)| {
                let resolved = match disposition {
                    RoundDisposition::Pending if timed_out => RoundDisposition::TimedOut,
                    other => *other,
                };
                (*id, resolved)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;
    use uuid::Uuid;

    fn id(n: u128) -> ParticipantId {
        ParticipantId(Uuid::from_u128(n))
    }

    fn start() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).expect("valid fixed timestamp")
    }

    fn deadline() -> DateTime<Utc> {
        start() + TimeDelta::seconds(120)
    }

    fn machine(contributors: &[ParticipantId]) -> RoundMachine {
        RoundMachine::open(Round(1), contributors, deadline())
    }

    #[test]
    fn the_default_window_is_two_minutes() {
        assert_eq!(DEFAULT_ROUND_TIMEOUT, Duration::from_secs(120));
    }

    #[test]
    fn a_fresh_round_is_open_with_everyone_pending() {
        let round = machine(&[id(2), id(1), id(3)]);
        assert_eq!(
            round.status(start()),
            RoundStatus::Open {
                pending: vec![id(1), id(2), id(3)],
            },
            "pending is reported in ParticipantId order"
        );
        assert!(round.sealed().is_empty());
    }

    #[test]
    fn a_round_closes_when_everyone_has_responded() {
        let mut round = machine(&[id(1), id(2)]);
        round.record_seal(id(1), start()).expect("seal");
        assert!(matches!(round.status(start()), RoundStatus::Open { .. }));

        round.record_pass(id(2), start()).expect("pass");
        assert_eq!(
            round.status(start()),
            RoundStatus::Closed {
                reason: RoundCloseReason::AllResponded
            },
            "the round closes early rather than waiting out the clock"
        );
    }

    #[test]
    fn a_round_of_all_passes_closes_cleanly() {
        // Everyone having nothing to say is a normal outcome, not a fault.
        let mut round = machine(&[id(1), id(2)]);
        round.record_pass(id(1), start()).expect("pass");
        round.record_pass(id(2), start()).expect("pass");
        assert_eq!(
            round.status(start()),
            RoundStatus::Closed {
                reason: RoundCloseReason::AllResponded
            }
        );
        assert!(round.sealed().is_empty(), "nothing to retrieve");
    }

    #[test]
    fn a_round_with_no_contributors_is_closed_immediately() {
        let round = machine(&[]);
        assert_eq!(
            round.status(start()),
            RoundStatus::Closed {
                reason: RoundCloseReason::AllResponded
            },
            "a conversation of observers cannot hold a round open"
        );
    }

    #[test]
    fn the_deadline_closes_a_round_with_members_still_thinking() {
        let mut round = machine(&[id(1), id(2)]);
        round.record_seal(id(1), start()).expect("seal");

        assert!(matches!(
            round.status(deadline() - TimeDelta::seconds(1)),
            RoundStatus::Open { .. }
        ));
        assert_eq!(
            round.status(deadline()),
            RoundStatus::Closed {
                reason: RoundCloseReason::TimedOut
            },
            "the deadline is inclusive"
        );
    }

    #[test]
    fn responding_after_close_is_refused_not_silently_dropped() {
        let mut round = machine(&[id(1), id(2)]);
        assert_eq!(
            round.record_seal(id(1), deadline()),
            Err(RoundError::Closed),
            "a late seal must not sneak into a closed round"
        );
        assert_eq!(
            round.record_pass(id(2), deadline()),
            Err(RoundError::Closed)
        );

        // And the late responder is still counted as having timed out.
        assert_eq!(
            round.final_dispositions(deadline()),
            vec![
                (id(1), RoundDisposition::TimedOut),
                (id(2), RoundDisposition::TimedOut),
            ]
        );
    }

    #[test]
    fn a_non_contributor_cannot_respond() {
        let mut round = machine(&[id(1)]);
        assert_eq!(
            round.record_seal(id(99), start()),
            Err(RoundError::NotEnrolled),
            "an observer or stranger cannot contribute"
        );
        assert_eq!(
            round.record_pass(id(99), start()),
            Err(RoundError::NotEnrolled)
        );
    }

    #[test]
    fn sealing_is_revisable_until_the_round_closes() {
        let mut round = machine(&[id(1), id(2)]);
        round.record_seal(id(1), start()).expect("seal");
        round.record_seal(id(1), start()).expect("reseal");
        assert_eq!(round.sealed(), vec![id(1)], "resealing is idempotent");

        round.record_pass(id(1), start()).expect("change to a pass");
        assert!(
            round.sealed().is_empty(),
            "a later response replaces the earlier one"
        );

        round.record_seal(id(1), start()).expect("change back");
        assert_eq!(round.sealed(), vec![id(1)]);
    }

    #[test]
    fn sealed_reports_only_sealers_in_order() {
        let mut round = machine(&[id(1), id(2), id(3)]);
        round.record_seal(id(3), start()).expect("seal");
        round.record_pass(id(2), start()).expect("pass");
        round.record_seal(id(1), start()).expect("seal");
        assert_eq!(round.sealed(), vec![id(1), id(3)]);
    }

    #[test]
    fn timed_out_and_passed_are_recorded_distinctly() {
        // Both let the round proceed, but only one says a participant may be
        // unhealthy — collapsing them would hide that.
        let mut round = machine(&[id(1), id(2), id(3)]);
        round.record_seal(id(1), start()).expect("seal");
        round.record_pass(id(2), start()).expect("pass");

        assert_eq!(
            round.final_dispositions(deadline()),
            vec![
                (id(1), RoundDisposition::Sealed),
                (id(2), RoundDisposition::Passed),
                (id(3), RoundDisposition::TimedOut),
            ]
        );
    }

    #[test]
    fn dispositions_before_the_deadline_still_read_as_pending() {
        let round = machine(&[id(1)]);
        assert_eq!(
            round.final_dispositions(start()),
            vec![(id(1), RoundDisposition::Pending)],
            "a member still on the clock has not timed out"
        );
    }

    #[test]
    fn filling_up_exactly_at_the_deadline_counts_as_responded() {
        let mut round = machine(&[id(1)]);
        round
            .record_seal(id(1), deadline() - TimeDelta::milliseconds(1))
            .expect("seal just in time");
        assert_eq!(
            round.status(deadline()),
            RoundStatus::Closed {
                reason: RoundCloseReason::AllResponded
            },
            "everyone did respond, so the close is not a timeout"
        );
    }

    #[test]
    fn the_machine_reads_no_clock() {
        // Status is a pure function of recorded state and the `now` handed in, so
        // the same machine can be interrogated at any point in time, repeatedly,
        // with identical results.
        let mut round = machine(&[id(1), id(2)]);
        round.record_seal(id(1), start()).expect("seal");
        for _ in 0..50 {
            assert!(matches!(round.status(start()), RoundStatus::Open { .. }));
            assert!(matches!(
                round.status(deadline()),
                RoundStatus::Closed { .. }
            ));
        }
    }
}
