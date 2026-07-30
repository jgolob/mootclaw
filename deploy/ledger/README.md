# Shared-ledger stack: running it yourself

A conversation between several agents and you, where one Postgres ledger is the
single source of truth and *is* the context window of every participant. The
orchestrator owns the database and is its only writer; participants hold no
conversation state and never see a connection string.

Nothing here needs an API key. The bots answer with fixed text, which is enough
to watch the machinery work — rounds opening and closing, contributions being
admitted in a deterministic order, your own messages landing.

## 1. The database

```bash
podman machine start   # first time only, if it is not already running
podman run -d --name zeroclaw-ledger-db \
  -e POSTGRES_USER=zeroclaw -e POSTGRES_PASSWORD=zeroclaw -e POSTGRES_DB=zeroclaw_ledger \
  -p 5433:5432 docker.io/library/postgres:17-alpine
```

Port 5433, so it cannot collide with a Postgres already on 5432. The schema is
applied by the orchestrator on first connect.

## 2. Build

```bash
cargo +1.96.1 build -p zeroclaw-bot -p zeroclaw-orchestrator -p zeroclaw-tui
```

## 3. Two participants

Each is a separate process with its own mailbox. The participant id is derived
from the configured name, so it is stable across restarts — a participant that
crashes rejoins the same conversation rather than arriving as a stranger.

```bash
ZEROCLAW_BOT_PARTICIPANT_ID=2e92fb01-9537-50a9-9b75-07b51ee5c062 \
ZEROCLAW_BOT_BIND=127.0.0.1:8181 \
ZEROCLAW_BOT_ANSWER="The round barrier bounds cost predictably." \
  ./target/debug/zeroclaw-bot &
```

```bash
ZEROCLAW_BOT_PARTICIPANT_ID=e7495a61-51e3-58b1-8b2f-c2425c8b3f81 \
ZEROCLAW_BOT_BIND=127.0.0.1:8182 \
ZEROCLAW_BOT_ANSWER="But it slows the conversation; two minutes is too long." \
  ./target/debug/zeroclaw-bot &
```

## 4. The orchestrator

Copy `orchestrator.example.toml` and run it. It prints the conversation id.

```bash
./target/debug/zeroclaw-orchestrator deploy/ledger/orchestrator.example.toml
```

## 5. Watch, and join in

```bash
./target/debug/zeroclaw-tui http://127.0.0.1:8088 <conversation-id> --as 7c9008e9-bc0b-50f7-a69e-9cc9c5975e32
```

Type to compose, `Enter` to send, `Tab` to pass the round, `Esc` to quit. Drop
`--as` to watch without contributing.

## What to look for

- **Rounds, not turns.** Both agents contribute to the same round and are
  admitted together. They did not see each other's contribution — that is what
  the round heading means, and why a peer's message is not a reply to yours.
- **Your message is in the ledger the moment you send it.** Not queued, not
  judged, not refused — appended, and visible in the next refresh. People write
  to the ledger; agents propose and are assessed. It does not disturb the round
  in progress either: agents are currency-checked against the head their round
  opened at, so your typing never discards their in-flight work. They see what
  you said in the next round, which is what a shared round means.
- **Passing closes a round early.** `Tab` rather than waiting out the clock.
- **Kill a participant.** `pkill -f zeroclaw-bot` — the conversation continues,
  and the round pane shows the absent member timing out. Restart it and it
  rejoins mid-conversation with full context, because the context was never
  theirs to lose.
- **Nothing operational appears in the ledger.** Timeouts and unreachable
  participants go to the audit log, which the view API does not expose. An agent
  that read "peer timed out" would comment on it, and a conversation about the
  plumbing is how a budget gets burned on nothing.

## Known rough edges

- The orchestrator creates a **new** conversation on every start. Participant
  ids are stable across restarts, but the conversation is not yet resumable.
- One conversation per orchestrator process.
- Rounds run continuously, so a panel of fixed-answer bots will happily talk to
  itself forever. Real participants pass when they have nothing to add, which is
  what makes a conversation quiesce.

## Cleaning up

```bash
pkill -f zeroclaw-bot; pkill -f zeroclaw-orchestrator
podman rm -f zeroclaw-ledger-db
```
