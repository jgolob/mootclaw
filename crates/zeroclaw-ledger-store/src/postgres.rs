//! Postgres [`LedgerStore`].
//!
//! The production store. It is held to the same conformance suite as the
//! in-memory one, run verbatim, so the orchestrator's in-memory tests cannot be
//! testing a fiction.
//!
//! The load-bearing detail is sequence assignment: `head_seq` is bumped and the
//! rows inserted in **one transaction**, so concurrent writers cannot interleave
//! within a round or reuse a sequence. Sequences come from that counter, never
//! from a clock and never from a caller.
//!
//! Uses `tokio-postgres` rather than `sqlx`. sqlx resolves `sqlx-sqlite` into the
//! lockfile even when the feature is off, and its `libsqlite3-sys` conflicts with
//! the `rusqlite` this workspace already links — Cargo rejects two packages
//! linking `sqlite3` at resolution time, whatever ends up being built.

use crate::store::{
    ConversationMeta, LedgerStore, ProposalRecord, Result, StoreError, StoredAuditEvent,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use deadpool_postgres::{Config as PoolConfig, Pool, Runtime};
use std::time::Duration;
use tokio_postgres::NoTls;
use uuid::Uuid;
use zeroclaw_ledger::types::{
    AuditEvent, AuditKind, ConversationId, ConversationKind, DraftId, EntryKind, LedgerEntry,
    MemberRole, NewEntry, OracleVerdict, Participant, ParticipantId, ParticipantKind, Round,
    RoundDisposition, Seq,
};

/// The schema, applied once.
const SCHEMA: &str = include_str!("../migrations/0001_init.sql");
const SCHEMA_VERSION: i64 = 1;

fn backend<E: std::fmt::Display>(error: E) -> StoreError {
    StoreError::Backend(error.to_string())
}

fn kind_str(kind: ConversationKind) -> &'static str {
    match kind {
        ConversationKind::Group => "group",
        ConversationKind::Dm => "dm",
    }
}

fn parse_conversation_kind(value: &str) -> Result<ConversationKind> {
    match value {
        "group" => Ok(ConversationKind::Group),
        "dm" => Ok(ConversationKind::Dm),
        other => Err(StoreError::Backend(format!(
            "unknown conversation kind {other:?}"
        ))),
    }
}

fn role_str(role: MemberRole) -> &'static str {
    match role {
        MemberRole::Contributor => "contributor",
        MemberRole::Observer => "observer",
    }
}

fn parse_role(value: &str) -> Result<MemberRole> {
    match value {
        "contributor" => Ok(MemberRole::Contributor),
        "observer" => Ok(MemberRole::Observer),
        other => Err(StoreError::Backend(format!(
            "unknown member role {other:?}"
        ))),
    }
}

fn participant_kind_str(kind: ParticipantKind) -> &'static str {
    match kind {
        ParticipantKind::Human => "human",
        ParticipantKind::Agent => "agent",
    }
}

fn parse_participant_kind(value: &str) -> Result<ParticipantKind> {
    match value {
        "human" => Ok(ParticipantKind::Human),
        "agent" => Ok(ParticipantKind::Agent),
        other => Err(StoreError::Backend(format!(
            "unknown participant kind {other:?}"
        ))),
    }
}

fn disposition_str(disposition: RoundDisposition) -> &'static str {
    match disposition {
        RoundDisposition::Pending => "pending",
        RoundDisposition::Sealed => "sealed",
        RoundDisposition::Passed => "passed",
        RoundDisposition::TimedOut => "timed_out",
    }
}

fn parse_disposition(value: &str) -> Result<RoundDisposition> {
    match value {
        "pending" => Ok(RoundDisposition::Pending),
        "sealed" => Ok(RoundDisposition::Sealed),
        "passed" => Ok(RoundDisposition::Passed),
        "timed_out" => Ok(RoundDisposition::TimedOut),
        other => Err(StoreError::Backend(format!(
            "unknown round disposition {other:?}"
        ))),
    }
}

fn audit_kind_str(kind: AuditKind) -> &'static str {
    match kind {
        AuditKind::RoundOpened => "round_opened",
        AuditKind::RoundClosed => "round_closed",
        AuditKind::ParticipantTimedOut => "participant_timed_out",
        AuditKind::ParticipantUnreachable => "participant_unreachable",
        AuditKind::DraftRejected => "draft_rejected",
        AuditKind::DraftStale => "draft_stale",
        AuditKind::Contaminated => "contaminated",
        AuditKind::Expunged => "expunged",
    }
}

fn parse_audit_kind(value: &str) -> Result<AuditKind> {
    match value {
        "round_opened" => Ok(AuditKind::RoundOpened),
        "round_closed" => Ok(AuditKind::RoundClosed),
        "participant_timed_out" => Ok(AuditKind::ParticipantTimedOut),
        "participant_unreachable" => Ok(AuditKind::ParticipantUnreachable),
        "draft_rejected" => Ok(AuditKind::DraftRejected),
        "draft_stale" => Ok(AuditKind::DraftStale),
        "contaminated" => Ok(AuditKind::Contaminated),
        "expunged" => Ok(AuditKind::Expunged),
        other => Err(StoreError::Backend(format!("unknown audit kind {other:?}"))),
    }
}

/// Postgres-backed ledger.
#[derive(Clone)]
pub struct PgLedgerStore {
    pool: Pool,
}

impl std::fmt::Debug for PgLedgerStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PgLedgerStore")
            .finish_non_exhaustive()
    }
}

impl PgLedgerStore {
    /// Connects to `database_url` and applies the schema.
    pub async fn connect(database_url: &str) -> Result<Self> {
        let mut config = PoolConfig::new();
        config.url = Some(database_url.to_string());
        let pool = config
            .create_pool(Some(Runtime::Tokio1), NoTls)
            .map_err(backend)?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// Applies the schema if this database has not seen it.
    ///
    /// Deliberately small: one versioned schema, applied in a transaction, with
    /// an advisory lock so concurrently starting orchestrators cannot race each
    /// other into a half-built schema.
    async fn migrate(&self) -> Result<()> {
        let mut client = self.pool.get().await.map_err(backend)?;
        let transaction = client.transaction().await.map_err(backend)?;

        transaction
            .execute("SELECT pg_advisory_xact_lock($1)", &[&SCHEMA_VERSION])
            .await
            .map_err(backend)?;
        transaction
            .batch_execute(
                "CREATE TABLE IF NOT EXISTS schema_versions (\
                   version    BIGINT PRIMARY KEY, \
                   applied_at TIMESTAMPTZ NOT NULL DEFAULT now())",
            )
            .await
            .map_err(backend)?;

        let applied = transaction
            .query_opt(
                "SELECT version FROM schema_versions WHERE version = $1",
                &[&SCHEMA_VERSION],
            )
            .await
            .map_err(backend)?;

        if applied.is_none() {
            transaction.batch_execute(SCHEMA).await.map_err(backend)?;
            transaction
                .execute(
                    "INSERT INTO schema_versions (version) VALUES ($1)",
                    &[&SCHEMA_VERSION],
                )
                .await
                .map_err(backend)?;
        }

        transaction.commit().await.map_err(backend)?;
        Ok(())
    }
}

#[async_trait]
impl LedgerStore for PgLedgerStore {
    async fn create_conversation(
        &self,
        kind: ConversationKind,
        round_timeout: Duration,
    ) -> Result<ConversationId> {
        let id = ConversationId(Uuid::new_v4());
        let timeout_ms = round_timeout.as_millis().max(1) as i64;
        let client = self.pool.get().await.map_err(backend)?;
        client
            .execute(
                "INSERT INTO conversations (id, kind, round_timeout_ms) VALUES ($1, $2, $3)",
                &[&id.0, &kind_str(kind), &timeout_ms],
            )
            .await
            .map_err(backend)?;
        Ok(id)
    }

    async fn upsert_participant(&self, participant: Participant) -> Result<()> {
        let client = self.pool.get().await.map_err(backend)?;
        client
            .execute(
                "INSERT INTO participants (id, kind, display_name, agent_alias) \
                 VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (id) DO UPDATE \
                 SET kind = EXCLUDED.kind, \
                     display_name = EXCLUDED.display_name, \
                     agent_alias = EXCLUDED.agent_alias",
                &[
                    &participant.id.0,
                    &participant_kind_str(participant.kind),
                    &participant.display_name,
                    &participant.agent_alias,
                ],
            )
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn add_member(
        &self,
        conversation: ConversationId,
        participant: ParticipantId,
        role: MemberRole,
    ) -> Result<()> {
        let client = self.pool.get().await.map_err(backend)?;
        if client
            .query_opt(
                "SELECT id FROM participants WHERE id = $1",
                &[&participant.0],
            )
            .await
            .map_err(backend)?
            .is_none()
        {
            return Err(StoreError::ParticipantNotFound(participant));
        }
        if client
            .query_opt(
                "SELECT id FROM conversations WHERE id = $1",
                &[&conversation.0],
            )
            .await
            .map_err(backend)?
            .is_none()
        {
            return Err(StoreError::ConversationNotFound(conversation));
        }

        // Membership is a set: re-adding updates the role rather than
        // duplicating the member.
        client
            .execute(
                "INSERT INTO conversation_members (conversation_id, participant_id, role) \
                 VALUES ($1, $2, $3) \
                 ON CONFLICT (conversation_id, participant_id) DO UPDATE SET role = EXCLUDED.role",
                &[&conversation.0, &participant.0, &role_str(role)],
            )
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn members(&self, conversation: ConversationId) -> Result<Vec<Participant>> {
        let client = self.pool.get().await.map_err(backend)?;
        if client
            .query_opt(
                "SELECT id FROM conversations WHERE id = $1",
                &[&conversation.0],
            )
            .await
            .map_err(backend)?
            .is_none()
        {
            return Err(StoreError::ConversationNotFound(conversation));
        }

        // Ordered by participant id: projection must be byte-stable, so iteration
        // order can never be allowed to leak into it.
        let rows = client
            .query(
                "SELECT p.id, p.kind, p.display_name, p.agent_alias \
                 FROM conversation_members m \
                 JOIN participants p ON p.id = m.participant_id \
                 WHERE m.conversation_id = $1 \
                 ORDER BY p.id",
                &[&conversation.0],
            )
            .await
            .map_err(backend)?;

        rows.into_iter()
            .map(|row| {
                let kind: String = row.get("kind");
                Ok(Participant {
                    id: ParticipantId(row.get("id")),
                    kind: parse_participant_kind(&kind)?,
                    display_name: row.get("display_name"),
                    agent_alias: row.get("agent_alias"),
                })
            })
            .collect()
    }

    async fn member_role(
        &self,
        conversation: ConversationId,
        participant: ParticipantId,
    ) -> Result<MemberRole> {
        let client = self.pool.get().await.map_err(backend)?;
        let row = client
            .query_opt(
                "SELECT role FROM conversation_members \
                 WHERE conversation_id = $1 AND participant_id = $2",
                &[&conversation.0, &participant.0],
            )
            .await
            .map_err(backend)?;

        match row {
            Some(row) => {
                let role: String = row.get("role");
                parse_role(&role)
            }
            None => Err(StoreError::NotAMember {
                conversation,
                participant,
            }),
        }
    }

    async fn head_seq(&self, conversation: ConversationId) -> Result<Seq> {
        let client = self.pool.get().await.map_err(backend)?;
        let row = client
            .query_opt(
                "SELECT head_seq FROM conversations WHERE id = $1",
                &[&conversation.0],
            )
            .await
            .map_err(backend)?;
        row.map(|row| Seq(row.get::<_, i64>("head_seq")))
            .ok_or(StoreError::ConversationNotFound(conversation))
    }

    async fn conversation_meta(&self, conversation: ConversationId) -> Result<ConversationMeta> {
        let client = self.pool.get().await.map_err(backend)?;
        let row = client
            .query_opt(
                "SELECT kind, round_timeout_ms, head_seq, visible_from \
                 FROM conversations WHERE id = $1",
                &[&conversation.0],
            )
            .await
            .map_err(backend)?
            .ok_or(StoreError::ConversationNotFound(conversation))?;

        let kind: String = row.get("kind");
        let timeout_ms: i64 = row.get("round_timeout_ms");
        Ok(ConversationMeta {
            kind: parse_conversation_kind(&kind)?,
            round_timeout: Duration::from_millis(timeout_ms.max(0) as u64),
            head_seq: Seq(row.get("head_seq")),
            visible_from: Seq(row.get("visible_from")),
        })
    }

    async fn append_batch(
        &self,
        conversation: ConversationId,
        round: Round,
        entries: Vec<NewEntry>,
    ) -> Result<Vec<Seq>> {
        let mut client = self.pool.get().await.map_err(backend)?;
        let transaction = client.transaction().await.map_err(backend)?;

        if transaction
            .query_opt(
                "SELECT id FROM conversations WHERE id = $1 FOR UPDATE",
                &[&conversation.0],
            )
            .await
            .map_err(backend)?
            .is_none()
        {
            return Err(StoreError::ConversationNotFound(conversation));
        }

        if entries.is_empty() {
            // An all-pass round admits nothing and must leave the head alone.
            transaction.commit().await.map_err(backend)?;
            return Ok(Vec::new());
        }

        // Bump the counter and insert the rows in one transaction. This is what
        // makes sequences unique and gapless under concurrent writers: no other
        // writer can observe or claim these numbers before the commit.
        let count = entries.len() as i64;
        let row = transaction
            .query_one(
                "UPDATE conversations SET head_seq = head_seq + $2 \
                 WHERE id = $1 RETURNING head_seq",
                &[&conversation.0, &count],
            )
            .await
            .map_err(backend)?;
        let new_head: i64 = row.get("head_seq");
        let first = new_head - count + 1;

        let mut assigned = Vec::with_capacity(entries.len());
        for (offset, entry) in entries.into_iter().enumerate() {
            let seq = Seq(first + offset as i64);
            let kind = match entry.kind {
                EntryKind::Contribution => "contribution",
            };
            transaction
                .execute(
                    "INSERT INTO ledger_entries \
                     (conversation_id, seq, round, participant_id, content, kind) \
                     VALUES ($1, $2, $3, $4, $5, $6)",
                    &[
                        &conversation.0,
                        &seq.0,
                        &round.0,
                        &entry.participant_id.0,
                        &entry.content,
                        &kind,
                    ],
                )
                .await
                .map_err(backend)?;
            assigned.push(seq);
        }

        transaction.commit().await.map_err(backend)?;
        Ok(assigned)
    }

    async fn entries_from(
        &self,
        conversation: ConversationId,
        from: Seq,
    ) -> Result<Vec<LedgerEntry>> {
        let meta = self.conversation_meta(conversation).await?;
        let floor = from.0.max(meta.visible_from.0);

        let client = self.pool.get().await.map_err(backend)?;
        let rows = client
            .query(
                "SELECT seq, round, participant_id, content, kind, tombstoned \
                 FROM ledger_entries \
                 WHERE conversation_id = $1 AND seq > $2 AND NOT tombstoned \
                 ORDER BY seq",
                &[&conversation.0, &floor],
            )
            .await
            .map_err(backend)?;

        rows.into_iter()
            .map(|row| {
                let kind: String = row.get("kind");
                if kind != "contribution" {
                    return Err(StoreError::Backend(format!("unknown entry kind {kind:?}")));
                }
                Ok(LedgerEntry {
                    conversation_id: conversation,
                    seq: Seq(row.get("seq")),
                    round: Round(row.get("round")),
                    participant_id: ParticipantId(row.get("participant_id")),
                    content: row.get("content"),
                    kind: EntryKind::Contribution,
                    tombstoned: row.get("tombstoned"),
                })
            })
            .collect()
    }

    async fn entry_timestamps(
        &self,
        conversation: ConversationId,
    ) -> Result<Vec<(Seq, DateTime<Utc>)>> {
        let client = self.pool.get().await.map_err(backend)?;
        let rows = client
            .query(
                "SELECT seq, recorded_at FROM ledger_entries \
                 WHERE conversation_id = $1 ORDER BY seq",
                &[&conversation.0],
            )
            .await
            .map_err(backend)?;
        Ok(rows
            .into_iter()
            .map(|row| (Seq(row.get("seq")), row.get("recorded_at")))
            .collect())
    }

    async fn open_round(
        &self,
        conversation: ConversationId,
        round: Round,
        deadline: DateTime<Utc>,
    ) -> Result<()> {
        let mut client = self.pool.get().await.map_err(backend)?;
        let transaction = client.transaction().await.map_err(backend)?;

        if transaction
            .query_opt(
                "SELECT id FROM conversations WHERE id = $1 FOR UPDATE",
                &[&conversation.0],
            )
            .await
            .map_err(backend)?
            .is_none()
        {
            return Err(StoreError::ConversationNotFound(conversation));
        }

        transaction
            .execute(
                "INSERT INTO rounds (conversation_id, round, deadline) VALUES ($1, $2, $3) \
                 ON CONFLICT (conversation_id, round) DO UPDATE SET deadline = EXCLUDED.deadline",
                &[&conversation.0, &round.0, &deadline],
            )
            .await
            .map_err(backend)?;

        // Only contributors are enrolled: an observer receives context but must
        // never hold a round open.
        transaction
            .execute(
                "INSERT INTO round_state (conversation_id, round, participant_id, disposition) \
                 SELECT $1, $2, participant_id, 'pending' FROM conversation_members \
                 WHERE conversation_id = $1 AND role = 'contributor' \
                 ON CONFLICT (conversation_id, round, participant_id) \
                 DO UPDATE SET disposition = 'pending'",
                &[&conversation.0, &round.0],
            )
            .await
            .map_err(backend)?;

        transaction.commit().await.map_err(backend)?;
        Ok(())
    }

    async fn round_state(
        &self,
        conversation: ConversationId,
        round: Round,
    ) -> Result<Vec<(ParticipantId, RoundDisposition)>> {
        let client = self.pool.get().await.map_err(backend)?;
        if client
            .query_opt(
                "SELECT round FROM rounds WHERE conversation_id = $1 AND round = $2",
                &[&conversation.0, &round.0],
            )
            .await
            .map_err(backend)?
            .is_none()
        {
            return Err(StoreError::RoundNotOpen {
                conversation,
                round,
            });
        }

        let rows = client
            .query(
                "SELECT participant_id, disposition FROM round_state \
                 WHERE conversation_id = $1 AND round = $2 ORDER BY participant_id",
                &[&conversation.0, &round.0],
            )
            .await
            .map_err(backend)?;

        rows.into_iter()
            .map(|row| {
                let disposition: String = row.get("disposition");
                Ok((
                    ParticipantId(row.get("participant_id")),
                    parse_disposition(&disposition)?,
                ))
            })
            .collect()
    }

    async fn round_deadline(
        &self,
        conversation: ConversationId,
        round: Round,
    ) -> Result<DateTime<Utc>> {
        let client = self.pool.get().await.map_err(backend)?;
        client
            .query_opt(
                "SELECT deadline FROM rounds WHERE conversation_id = $1 AND round = $2",
                &[&conversation.0, &round.0],
            )
            .await
            .map_err(backend)?
            .map(|row| row.get("deadline"))
            .ok_or(StoreError::RoundNotOpen {
                conversation,
                round,
            })
    }

    async fn record_round_disposition(
        &self,
        conversation: ConversationId,
        round: Round,
        participant: ParticipantId,
        disposition: RoundDisposition,
    ) -> Result<()> {
        let client = self.pool.get().await.map_err(backend)?;
        if client
            .query_opt(
                "SELECT role FROM conversation_members \
                 WHERE conversation_id = $1 AND participant_id = $2",
                &[&conversation.0, &participant.0],
            )
            .await
            .map_err(backend)?
            .is_none()
        {
            return Err(StoreError::NotAMember {
                conversation,
                participant,
            });
        }
        if client
            .query_opt(
                "SELECT round FROM rounds WHERE conversation_id = $1 AND round = $2",
                &[&conversation.0, &round.0],
            )
            .await
            .map_err(backend)?
            .is_none()
        {
            return Err(StoreError::RoundNotOpen {
                conversation,
                round,
            });
        }

        client
            .execute(
                "INSERT INTO round_state (conversation_id, round, participant_id, disposition) \
                 VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (conversation_id, round, participant_id) \
                 DO UPDATE SET disposition = EXCLUDED.disposition",
                &[
                    &conversation.0,
                    &round.0,
                    &participant.0,
                    &disposition_str(disposition),
                ],
            )
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn record_proposal(&self, proposal: ProposalRecord) -> Result<()> {
        let client = self.pool.get().await.map_err(backend)?;
        client
            .execute(
                "INSERT INTO proposals \
                 (id, conversation_id, round, participant_id, content, \
                  composed_against_seq, version) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7) \
                 ON CONFLICT (id) DO UPDATE \
                 SET content = EXCLUDED.content, \
                     composed_against_seq = EXCLUDED.composed_against_seq, \
                     version = EXCLUDED.version",
                &[
                    &proposal.id.0,
                    &proposal.conversation_id.0,
                    &proposal.round.0,
                    &proposal.participant_id.0,
                    &proposal.content,
                    &proposal.composed_against_seq.0,
                    &proposal.version.0,
                ],
            )
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn set_proposal_verdict(&self, proposal: DraftId, verdict: OracleVerdict) -> Result<()> {
        let encoded = serde_json::to_value(&verdict).map_err(backend)?;
        let client = self.pool.get().await.map_err(backend)?;
        let affected = client
            .execute(
                "UPDATE proposals SET verdict = $2 WHERE id = $1",
                &[&proposal.0, &encoded],
            )
            .await
            .map_err(backend)?;
        if affected == 0 {
            return Err(StoreError::ProposalNotFound(proposal));
        }
        Ok(())
    }

    async fn proposal_verdict(&self, proposal: DraftId) -> Result<Option<OracleVerdict>> {
        let client = self.pool.get().await.map_err(backend)?;
        let row = client
            .query_opt(
                "SELECT verdict FROM proposals WHERE id = $1",
                &[&proposal.0],
            )
            .await
            .map_err(backend)?
            .ok_or(StoreError::ProposalNotFound(proposal))?;

        let encoded: Option<serde_json::Value> = row.get("verdict");
        match encoded {
            None => Ok(None),
            Some(value) => serde_json::from_value(value).map(Some).map_err(backend),
        }
    }

    async fn append_audit(&self, event: AuditEvent) -> Result<()> {
        let client = self.pool.get().await.map_err(backend)?;
        client
            .execute(
                "INSERT INTO audit_events \
                 (conversation_id, participant_id, round, kind, detail) \
                 VALUES ($1, $2, $3, $4, $5)",
                &[
                    &event.conversation_id.map(|id| id.0),
                    &event.participant_id.map(|id| id.0),
                    &event.round.map(|round| round.0),
                    &audit_kind_str(event.kind),
                    &event.detail,
                ],
            )
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn audit_events(&self, conversation: ConversationId) -> Result<Vec<StoredAuditEvent>> {
        let client = self.pool.get().await.map_err(backend)?;
        let rows = client
            .query(
                "SELECT participant_id, round, kind, detail, occurred_at \
                 FROM audit_events WHERE conversation_id = $1 ORDER BY id",
                &[&conversation.0],
            )
            .await
            .map_err(backend)?;

        rows.into_iter()
            .map(|row| {
                let participant: Option<Uuid> = row.get("participant_id");
                let round: Option<i64> = row.get("round");
                let kind: String = row.get("kind");
                Ok(StoredAuditEvent {
                    event: AuditEvent {
                        conversation_id: Some(conversation),
                        participant_id: participant.map(ParticipantId),
                        round: round.map(Round),
                        kind: parse_audit_kind(&kind)?,
                        detail: row.get("detail"),
                    },
                    occurred_at: row.get("occurred_at"),
                })
            })
            .collect()
    }

    async fn tombstone_from(&self, conversation: ConversationId, from: Seq) -> Result<u64> {
        let client = self.pool.get().await.map_err(backend)?;
        client
            .execute(
                "UPDATE ledger_entries SET tombstoned = TRUE \
                 WHERE conversation_id = $1 AND seq >= $2 AND NOT tombstoned",
                &[&conversation.0, &from.0],
            )
            .await
            .map_err(backend)
    }

    async fn set_visible_from(&self, conversation: ConversationId, seq: Seq) -> Result<()> {
        // `GREATEST` makes the floor monotonic in the database itself, so
        // containment cannot be reversed by a later caller or a racing one.
        let client = self.pool.get().await.map_err(backend)?;
        client
            .execute(
                "UPDATE conversations SET visible_from = GREATEST(visible_from, $2) WHERE id = $1",
                &[&conversation.0, &seq.0],
            )
            .await
            .map_err(backend)?;
        Ok(())
    }
}
