# Umbrachyx — concept

This is the working concept. It is expected to change. When a part of it is
settled, that part moves into a decision record and this page points to it.

## One line

A database that assumes the caller cannot be trusted. Every write can be
undone, every read is recorded, and both of those facts are backed by
machine-checked evidence. It runs at memory speed.

## Why another database

Most databases are built on four assumptions:

1. **A caller who has logged in can be trusted** with everything its role
   allows.
2. **A write is final.** Undoing one means a person restoring a backup.
3. **Privileges start broad** and a person narrows them by configuration.
4. **Correctness is shown by age**, by how many years the database has run.

AI agents break all four. They are fast and numerous, they make mistakes, and
a prompt injection can hijack them. So logging in is no longer a reason for
trust. Agents also write more than a person can review one change at a time,
and the people setting their privileges cannot narrow them all correctly. And
a new database has no years to point to.

## Prior art

Each of the ideas here already exists somewhere. What does not yet exist is
the combination at memory speed.

| System | What it keeps | What it lacks for this purpose |
|---|---|---|
| Dolt | Commits, branches, merges and cell-level diffs over MySQL | Not built for memory-speed access; history records the author only, not the reason or the approver |
| Datomic | Immutable facts per attribute (effectively per cell) | No branch or merge; not SQL |
| TerminusDB | History per fact, with branch and merge | Graph model; not aimed at hot operational data |
| XTDB | Bitemporal history per document | Per record, not per cell; no branch or merge |
| SQL:2011 system-versioned tables (MariaDB, SQL Server, Db2) | Past versions of each row | No branch, merge, diff or provenance |
| Postgres audit extensions (pgMemento, supa_audit) | A trigger-written log of row changes | Bolted on after the fact; costs write speed |
| Redis / Valkey | Memory speed | No history at all |
| Irmin | A git-like store as a library | A library, not a database server |

## Architecture sketch

```
write ─▶ (1) append-only log, synchronous, on disk   ← "committed" once written here
             │  carries who / why / approved-by
             ▼
         (2) in-memory persistent tree (current state) ← reads and writes served here
             │  asynchronous
             ▼
         (3) content-addressed store (git-like)        ← commits, branches, diffs, past states
```

- **(1) is the truth.** A write is acknowledged once it is in the log. If the
  process crashes, (2) and (3) are rebuilt from (1). This closes the gap that a
  purely asynchronous "memory first, persist later" design leaves open, where
  a crash loses recent writes and the history silently misses them.
- **(2) and (3) are the same structure.** The in-memory state is an immutable
  Merkle tree. A change creates new nodes only along the changed path and
  shares everything else with the previous version. Then:
  - a commit is one root hash,
  - a branch is one more pointer to a root, with no copying,
  - a diff walks two roots and descends only where the hashes differ, so its
    cost grows with the size of the change rather than the size of the data,
  - persisting to (3) means writing out the nodes that are not on disk yet.
- **Not everything has to fit in memory.** Hot branches stay in memory. Cold
  ones are read from (3) when needed. This tiering is designed in from the
  start, not added later.

## Granularity

```
commit (when / who / why / approved by)
 └ table
    └ record   ← a leaf of the tree is a record
       └ cell  ← inside the leaf: per-cell "last changed by / at / because"
```

The leaves are records, which keeps the tree, diffs and sync cheap. Inside
each leaf, every cell records its own last change, so "who changed this value,
and when?" is answered directly and diffs can mark changed cells.

**Retention is chosen per column.** For example:

| Column kind | Example | History |
|---|---|---|
| Must never be lost | Paid currency, items | Every change, kept |
| Changes often, matters in aggregate | Stamina | Compacted to one entry per interval |
| Ephemeral | Position | Current value only, no history |

The same record can mix all three. The record-level version list still runs
through every column.

## Sync and scale

Fast writes and "every node always sees the same value" pull against each
other. The speed of light sets a floor on how fast a remote node can confirm a
write. So the strength of consistency is chosen per kind of value:

1. **Strongly consistent (currency, items).** Each owner, such as a player,
   has a home node that commits their writes. Most writes therefore touch one
   node and stay fast, and double-spending is ruled out.
2. **Mergeable (leaderboards, counters).** These use CRDTs: each node applies
   writes locally, and the replicas converge without coordination.
3. **Anti-entropy.** Nodes compare root hashes, and when the roots differ they
   descend only into the subtrees that differ. This is the same Merkle
   structure that serves the history, so the structure chosen for history is
   also what makes sync cheap.

## Interface

- **The core is key/value.** A small core is easier to make fast and easier to
  verify. A SQL layer is built on top of it afterwards, following the
  layered approach FoundationDB showed works.
- **MCP from the start, with every tool scoped by capability.** Adding tools
  must not widen what a hijacked agent can reach. A capability states which
  data, until when and for what purpose, and it can only be narrowed when it
  is handed on. Agent writes always land on a branch.

## Deleting what must be deleted

Keeping every change forever makes two things worse: a leak exposes past
values as well as current ones, and a request to delete personal data cannot
be honoured by rewriting history.

The answer is **crypto-shredding**. Values are encrypted under a key per
subject, such as a player. To erase a subject, its key is destroyed. The
history keeps its shape (what changed, and when), and the content becomes
permanently unreadable. Fully homomorphic computation over the encrypted
values is not attempted. It is still too slow, so the line between what can be
computed over encrypted values and what cannot will be drawn explicitly.

## Non-goals

- Being the fastest database at everything. "Fast enough at memory speed",
  together with full history, is the claim.
- Competing with Postgres on general-purpose breadth.
- Being a cache. A committed write is durable.
- Claiming safety because an AI wrote it. The claim rests on the evidence.

## How "better" will be measured

| Compared with | Measure |
|---|---|
| Redis / Valkey | p99 read and write latency |
| Dolt | Speed of history, diff and merge |
| Postgres with audit triggers | Effort and write cost for the same history |
| All of them | **What a hijacked agent can damage or leak**, a measure existing databases are rarely tested against |

## Open questions

- Is the time model transaction time only, or bitemporal? The current plan is
  transaction time only, with room left to add valid time later.
- What does a capability token look like (macaroon-style attenuation,
  biscuit, or something of its own)?
- How do retention policies interact with branches? Is a compacted column
  still diffable across the compaction point?
- What is the merge conflict model? It can be richer than git's: constraint
  and foreign-key violations can be detected, not only overlapping edits.
- What is the first deterministic-simulation target: the log, the tree, or
  sync?
