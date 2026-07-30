//! Builders for constructing participants and contributions in tests.
//!
//! Shipped rather than test-gated because every crate above this one needs them:
//! the conformance suite, the projection tests, the round driver's in-memory
//! integration tests, and the Postgres suite all build the same shapes.

use uuid::Uuid;
use zeroclaw_ledger::{EntryKind, NewEntry, Participant, ParticipantId, ParticipantKind};

/// An agent participant with a fresh identity, aliased to `name`.
pub fn agent(name: &str) -> Participant {
    Participant {
        id: ParticipantId(Uuid::new_v4()),
        kind: ParticipantKind::Agent,
        display_name: name.to_string(),
        agent_alias: Some(name.to_string()),
    }
}

/// A human participant with a fresh identity.
pub fn human(name: &str) -> Participant {
    Participant {
        id: ParticipantId(Uuid::new_v4()),
        kind: ParticipantKind::Human,
        display_name: name.to_string(),
        agent_alias: None,
    }
}

/// A contribution authored by `participant`.
pub fn contribution(participant: ParticipantId, content: &str) -> NewEntry {
    NewEntry {
        participant_id: participant,
        content: content.to_string(),
        kind: EntryKind::Contribution,
    }
}
