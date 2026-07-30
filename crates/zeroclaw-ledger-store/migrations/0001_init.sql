-- Shared conversation ledger.
--
-- Two tables carry the design's central rule: `ledger_entries` holds only
-- contributions a participant meant to make, and `audit_events` holds everything
-- operational. Nothing joins them, and nothing on the projection path reads the
-- second.
--
-- There is no wall-clock column on the conversation record itself. Ordering is
-- `seq` and causality is `round`; `recorded_at` exists for logging and QC and is
-- never projected into anyone's context.

CREATE TABLE conversations (
    id               UUID PRIMARY KEY,
    kind             TEXT        NOT NULL CHECK (kind IN ('group', 'dm')),
    round_timeout_ms BIGINT      NOT NULL CHECK (round_timeout_ms > 0),
    -- Sequence of the most recent admitted entry; 0 when none. Incremented in
    -- the same transaction as the batch insert, which is what makes sequence
    -- assignment atomic under concurrent writers.
    head_seq         BIGINT      NOT NULL DEFAULT 0,
    -- Entries at or below this are withheld from projection. Raised by
    -- containment, never lowered.
    visible_from     BIGINT      NOT NULL DEFAULT 0,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE participants (
    id           UUID PRIMARY KEY,
    kind         TEXT NOT NULL CHECK (kind IN ('human', 'agent')),
    display_name TEXT NOT NULL,
    agent_alias  TEXT
);

-- Maps a projection surface (Telegram, web, TUI) onto a participant, so one
-- person reaching the conversation through several channels is one participant.
-- Attribution references participants, never raw platform identifiers.
CREATE TABLE participant_identities (
    participant_id UUID NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    projection     TEXT NOT NULL,
    external_id    TEXT NOT NULL,
    PRIMARY KEY (projection, external_id)
);

CREATE TABLE conversation_members (
    conversation_id UUID NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    participant_id  UUID NOT NULL REFERENCES participants(id),
    role            TEXT NOT NULL CHECK (role IN ('contributor', 'observer')),
    PRIMARY KEY (conversation_id, participant_id)
);

-- The conversation itself: the context window of every participant.
CREATE TABLE ledger_entries (
    conversation_id UUID        NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    seq             BIGINT      NOT NULL,
    round           BIGINT      NOT NULL,
    participant_id  UUID        NOT NULL REFERENCES participants(id),
    content         TEXT        NOT NULL,
    kind            TEXT        NOT NULL CHECK (kind IN ('contribution')),
    -- Withheld from projection, never deleted: the ledger is logically
    -- append-only, so containment tombstones rather than removes.
    tombstoned      BOOLEAN     NOT NULL DEFAULT FALSE,
    -- Operational metadata for logging and QC. Never projected.
    recorded_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (conversation_id, seq)
);

CREATE INDEX ledger_entries_visible ON ledger_entries (conversation_id, seq)
    WHERE NOT tombstoned;

CREATE TABLE rounds (
    conversation_id UUID        NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    round           BIGINT      NOT NULL,
    deadline        TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (conversation_id, round)
);

CREATE TABLE round_state (
    conversation_id UUID   NOT NULL,
    round           BIGINT NOT NULL,
    participant_id  UUID   NOT NULL REFERENCES participants(id),
    disposition     TEXT   NOT NULL
        CHECK (disposition IN ('pending', 'sealed', 'passed', 'timed_out')),
    PRIMARY KEY (conversation_id, round, participant_id),
    FOREIGN KEY (conversation_id, round) REFERENCES rounds(conversation_id, round)
        ON DELETE CASCADE
);

-- Every retrieved draft, admitted or not. Rejected content never enters the
-- ledger but is not lost: it is what the economic layer meters and what an
-- operator needs in order to answer "why did my contribution not appear?".
CREATE TABLE proposals (
    id                   UUID PRIMARY KEY,
    conversation_id      UUID        NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    round                BIGINT      NOT NULL,
    participant_id       UUID        NOT NULL REFERENCES participants(id),
    content              TEXT        NOT NULL,
    composed_against_seq BIGINT      NOT NULL,
    version              BIGINT      NOT NULL,
    -- Null until judged, which is distinguishable from an unknown proposal.
    verdict              JSONB,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX proposals_by_round ON proposals (conversation_id, round);

-- Operational events. Never delivered to a participant, never joined to the
-- ledger: an agent that reads "peer timed out" will comment on it, and that is
-- how runaway loops begin.
CREATE TABLE audit_events (
    id              BIGSERIAL PRIMARY KEY,
    conversation_id UUID,
    participant_id  UUID,
    round           BIGINT,
    kind            TEXT        NOT NULL,
    detail          TEXT        NOT NULL,
    occurred_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX audit_events_by_conversation ON audit_events (conversation_id, id);
