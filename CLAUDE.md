# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

pool-pm-cardano is a Cardano blockchain indexer and real-time event server written in Rust. It connects to a Cardano node via N2C (node-to-client) protocol using Oura/Pallas, processes chain events (blocks and rollbacks), monitors the mempool for pending transactions, and streams events to web clients via SSE (Server-Sent Events). Data is queried from a PostgreSQL database populated by cardano-db-sync.

## Build Commands

```bash
cargo build                # Debug build
cargo build --release      # Optimized release build
cargo check                # Type-check without building (fastest feedback loop)
cargo run                  # Build and run
cargo clippy               # Lint
```

## Testing

```bash
cargo test                    # Rust unit tests (server/)
cd web && pnpm test           # Frontend unit tests (Vitest)
```

- **Rust**: unit tests live in a `#[cfg(test)] mod tests` block in the same file (e.g. `server/src/nftcdn.rs`). `cargo test` compiles the crate, so — like any build — it needs a reachable cardano-db-sync DB (the sqlx `query!` macros are validated at compile time). Test pure logic; queries that need the DB aren't unit-testable.
- **Frontend**: Vitest. Put a `*.test.ts` next to the module it covers (e.g. `web/src/lib/search.test.ts`) and `import { describe, it, expect } from 'vitest'`. Test **pure functions** — extract logic out of `.svelte` components into a plain `.ts` module so it's testable without a DOM (see `search.ts` + `search.test.ts`). Run a single file with `pnpm test <path>`.

## Architecture

The project is a Cargo workspace with a single package (`server/`).

### Stream Processing Pipeline

The server uses the **Gasket** framework to build a multi-stage stream processing pipeline:

```
Cardano Node (N2C) → Source Stage (Oura) → Sink Stage → Cursor Stage (JSON file)
                                               ↕
                                       PostgreSQL (cardano-db-sync)
                   → Mempool Monitor Stage
                                               ↓
                                    broadcast::Sender<Event>
                                               ↓
                                    axum SSE server (/events)
```

- **Source**: Oura N2C source connects to a Cardano node and emits `ChainEvent`s (blocks or rollback points).
- **Sink** (`sink.rs`): Processes chain events, maintains versioned in-memory state using immutable data structures (`imbl` crate) with structural sharing. On blocks: updates UTXOs, sends `Event::Block`. On rollback: reverts state, sends `Event::Rollback`.
- **Mempool** (`mempool.rs`): Monitors the node mempool via LocalTxMonitor mini-protocol. Decodes transactions, resolves input addresses from UTXO state, computes CIP-14 asset fingerprints, and sends `Event::MempoolTx`.
- **Cursor**: Persists the current chain position to a JSON file on disk (kept for debugging, not used for resume).
- **Snapshot**: Serializes a `BlockSnapshot` to `{output}/snapshot.bin` using MessagePack (`rmp-serde`) for fast resume on restart (see Snapshot Persistence below).
- **SSE Server** (`server.rs`): axum HTTP server with `GET /events` endpoint that streams events as JSON via Server-Sent Events.
- **Daemon** (`daemon.rs`): Orchestrates the full Gasket pipeline with retry policies, optional Prometheus metrics, and SSE server.

### Key Modules

- `args.rs` — CLI argument parsing via clap (socket, network, db connection, metrics endpoint, listen address, output dir)
- `chain.rs` — Cardano network configuration (mainnet/preprod/preview magic numbers via Oura GenesisValues)
- `state/dbsync.rs` — Async PostgreSQL queries via sqlx against cardano-db-sync schema (pools, delegations, UTXOs, stakes)
- `event.rs` — Shared event types: `MempoolTx`, `Block`, `Rollback` (serializable for SSE)
- `model.rs` — Data structures: `Pool`, `TxOutput`, CIP-14 asset fingerprint computation
- `state/mod.rs` — Versioned state with `BlockSnapshot` history and structural sharing for O(1) rollbacks; `state/feed_index.rs` holds the 5-day per-subject feed index
- `mempool.rs` — Gasket worker stage for mempool monitoring via LocalTxMonitor
- `sink.rs` — Gasket worker stage that processes chain events into indexed state
- `server.rs` — axum SSE server for streaming events to web clients

### Key Patterns

- **Immutable data structures**: State is held in `imbl::OrdMap`/`imbl::HashMap`/`imbl::HashSet` for safe structural sharing and efficient rollbacks.
- **Versioned state**: `State` maintains a `Vec<BlockSnapshot>` history; each snapshot shares structure with the previous via `imbl` crate O(1) clone. Always store new per-block data in `BlockSnapshot` so rollbacks are handled automatically by history truncation — never maintain separate delta/rollback logic.
- **Rollback correctness is critical**: Every new feature that tracks or derives data from blocks must handle rollbacks correctly. A `Rollback { slot }` event removes all blocks with `slot > rollback_slot` from the event bus, state history, and frontend sections. If a feature maintains counters, caches, or derived state from block data, it must revert cleanly on rollback — do not add features that only increment/accumulate without a rollback path.
- **Event broadcasting**: `tokio::sync::broadcast` channel fans out events from pipeline stages to multiple SSE clients.
- **Gasket error handling**: Worker methods return `gasket::error::Error` with `or_panic()` / `or_retry()` combinators.
- **Async throughout**: tokio runtime with sqlx async database access.
- **Compile-time SQL checking**: sqlx `query_as!` macros validate SQL against the DB schema at compile time.

### Backward reconstruction of historical per-block state (feed replay)

When a feed must show a value that depends on **pre-block state at an old block**
(e.g. the `live_stake` and previous delegation target at a delegation tx from a
year ago), three naive approaches all fail:
- **Resolve against current state** — *wrong*: shows the present value as if it were
  historical (e.g. the address's current DRep shown as the `from` of an old cert).
- **Point-in-time db query** — *too slow*: e.g. summing an address's unspent UTXOs
  as-of a block is a random-heap scan (~18s for a 177k-UTXO address; `value` isn't
  in the partial index).
- **Keep more history in memory** — the feed index is pruned to 5 days; older items
  fall out.

The technique that works (`build_subject_replay` + `SubjectReplay::pre_block_stake`
in `server.rs`, db queries in `state/dbsync.rs`): a feed only ever replays a bounded
window (the last `STAKE_REPLAY_BLOCKS` blocks touching the subject), and we know the
**exact current value** from the snapshot. So walk **backward** from the current
value, undoing each replayed block, to recover the exact pre-block value at every
displayed block — cheaply and without ever resolving against stale current state.

Concretely, for stake feeds (`live_stake(cred) = stakes[cred] + rewards[cred]`,
forward deltas defined in `state/mod.rs::apply_block`):
- Start `running` = snapshot `stakes[cred] + rewards[cred]`.
- Process replayed blocks **newest→oldest**; per block, undo (then assign the result
  as that block's pre-block value):
  1. **Epoch reward accruals** applied after this block — `spendable_epoch > block.epoch`
     (a pure epoch-number compare; no boundary slots needed, since `delta(E)` is
     already in any non-boundary block of epoch `E`). From `reward ∪ reward_rest`.
  2. **Off-window withdrawals** at `slot > block.slot` whose tx isn't in the replayed
     set (in-window withdrawals are already inside the block's net stake change).
  3. The block's **own net stake change** = `Σ tx.stake_change` over *all* decoded txs
     (before the `retain` filter, so dropped withdrawal-only txs still count).
- Delegation `from`/`to` come from the **full db history** (`LAG` over
  `delegation ∪ stake_deregistration`, per credential — deregs interleaved so `from`
  is correct across them), so both are exact at any age. The 5-day feed-index overlay
  still wins near the tip (db-sync lags the chain by seconds).

**Applicability criteria for reusing this on another feed** (evaluate before
assuming feasible *or* infeasible):
- The replayed window must contain **every** state change to the reconstructed
  quantity for that subject. ✔ Stake feeds: all the credential's UTXO-touching blocks
  are in the window. ✘ **Address feeds**: blocks/`stake_change` are scoped to one
  payment address, but a delegation's stake is credential-level (other addresses of
  the key, withdrawals the address branch doesn't net out) — so the walk would be
  wrong and is deliberately **not** applied there (they keep the feed-index overlay).
- Out-of-window contributions (here: epoch rewards, off-window withdrawals) must be
  recoverable from **cheap `addr_id`-indexed** db queries (ms-scale), not heap scans.
- All db lookups run **off the `chain_state` lock** (`db_handle()` + short
  await-free guard scopes only) — see the never-block-the-feeds rule.

The per-block arithmetic is isolated in `pre_block_stake` and unit-tested; keep new
variants equally testable (pure function over the event lists).

### Per-address asset holdings (flat composite-key map)

`asset_holdings` (`state/mod.rs`) tracks every UTXO-held token in a single flat
`imbl::OrdMap<HeldKey, Held>`, where `HeldKey = (Arc<AddrKey>, policy++name)` sorts as the
composite `(cred, addr, policy, name)`. A single large map keeps `imbl`'s fixed-capacity node
chunks densely packed. The ~15M-entry map is the dominant memory user, so the layout is
tuned hard (mainnet cold-reset RSS ~8.5 GB); any change here must preserve these properties:

- **Reads are prefix range scans** over the sorted composite key: an address's tokens are
  the contiguous `(cred, addr, …)` range (`addr_range`); a stake credential's are the
  `(Some(cred), …)` range (`cred_range`), deduping/summing the same asset across the
  credential's addresses. No per-address sub-map to index into.
- **Whale-safe mutation & diffs**: a block only moves a few tokens, so `bump_one` is an
  O(log n) point op (prune the entry at qty 0), and live grid deltas walk `prev.diff(curr)`
  (O(block changes), structural) filtered by the subject's key prefix — never an
  O(total holdings) scan per block.
- **Interned `(cred, addr)` key**: all of one address's tokens share a single `Arc<AddrKey>`,
  so the credential + payment-address bytes (and their allocations) are stored once, not once
  per held token (~11× dedup on mainnet — 1.3M distinct addresses over 15M entries; ~1.5 GB
  saved). `AddrKey`'s derived `Ord` reproduces the old `(cred, addr)` byte order, and
  `Arc<T>`'s by-value `Ord` delegates to it — so the sort the range scans depend on is
  unchanged. The interner (`AddrInterner` in `State`, **not** in `BlockSnapshot`) is rebuilt
  at reset, seeded during the streamed db-sync build, and re-interned as the snapshot streams
  in on load; it's rollback-safe because it only grows and its entries are content-addressed.
  Every mutation and every range/point lookup builds its key through `intern_addr` /
  `AddrKey::from_query` — compare by **value** (`k.as_ref() == &target`), never `Arc::ptr_eq`
  (prev and curr snapshots may hold distinct Arcs for the same address).
- **Packed 128-bit leaf**: `Held` is a `u128` packed as `(mint_slot:30 | qty:98)`, stored as
  two `u64` (align 8, 16 bytes — a bare `u128` is align 16 and would just re-pad the align-8
  key, erasing the saving; a `const` assert locks size/align). The low 98 bits are the exact
  quantity (a `u128`, far beyond any real supply < 2^64 — no clamping); the high 30 bits are
  the asset's first-mint **slot**, used only to sort the owned grid (monotonic with time; not
  displayed — the asset-info popup's mint dates come from a separate db query). Serde stays
  variable-length: `(mint_slot, Qty)` where `Qty` is 1 byte for small values, a `(lo, hi)`
  pair above `u64`.
- **Rollback is automatic** (the map lives in `BlockSnapshot` history). The map is
  `#[serde(skip)]` and (de)serialized manually in `write_snapshot` / `load_snapshot`, **grouped
  by address**: `AddrKey → [(AssetId, Held)]`, keys deref'd off their `Arc` (`HoldingsSer`).
  Because the map is sorted by `(cred, addr, …)` an address's tokens are contiguous, so its
  `(cred, addr)` is written once per address (1.3M) instead of once per token (14.8M) — that
  alone halved both the file (3.6 GB → 2.0 GB) and the holdings load (30.0 s → 16.5 s,
  measured back-to-back). Load interns each address **once** and inserts its tokens as they
  stream (`HoldingsSeed` + `TokensSeed`), so the un-shared full map is never materialized.
  Bump `SNAPSHOT_FORMAT` on any persisted-shape change so old snapshots rebuild from db-sync;
  when the change is cheap to read both ways, keep a one-release read-only compat path
  instead (`SNAPSHOT_FORMAT_LEGACY_UNGROUPED`) so a deploy resumes rather than cold-resetting.
  Two `#[ignore]`d benches in `state/mod.rs` time a real file end to end — see
  `SNAPSHOT_BENCH=… cargo test --release snapshot_load_timing -- --nocapture --ignored`.

`BlockSnapshot::log_memory` / `State::log_memory` (called after reset and warm resume) print
a per-field byte breakdown — content bytes, the `asset_holdings` inline/heap split, and the
`addr_interner` size — for reasoning about this layout; RSS is the ground truth.

## Runtime Configuration

The server is configured via CLI args:
- `-s, --socket` — Cardano node socket path (required)
- `-n, --network` — mainnet (default), preprod, or preview
- `-d, --db` — PostgreSQL connection string (default: `postgresql:///NETWORK?host=/var/run/postgresql`)
- `-l, --listen` — SSE server listen address (e.g., `0.0.0.0:3000`; omit to disable SSE)
- `-m, --metrics` — Prometheus metrics endpoint (`ADDR:PORT` or `default` for `127.0.0.1:9188`)
- `-o, --output` — Directory for snapshot and cursor files (default: `/tmp/cardano`)
- `--snapshot-depth` — How many blocks back from tip to persist the snapshot (default: `8`)
- `-v, --verbose` — Enable DEBUG-level logging

Requires a running cardano-db-sync PostgreSQL database for the target network.

### Snapshot Persistence

On startup, the server tries to load `{output}/snapshot.bin`. If found, it restores the in-memory state and requests the node to resume from the snapshot's slot/block hash (`IntersectConfig::Point`). If the snapshot is missing, corrupt, or the node rejects the intersection point (e.g., snapshot too old), the server falls back to starting from tip with a full reset from db-sync.

The snapshot file is a sequence of msgpack values that **lead with `(format, magic)`**, validated before the multi-GB `asset_holdings` map is read — so a stale (`SNAPSHOT_FORMAT` bumped) or foreign-network snapshot is rejected cheaply, without deserializing ~10 GB only to drop it (freed pages the allocator would retain).

Snapshots are saved:
- Immediately after a reset (so a restart doesn't repeat the expensive db-sync queries)
- Every 50 blocks during normal operation

The snapshot is written `--snapshot-depth` blocks behind the tip (default 8) to provide rollback safety — on mainnet, rollbacks rarely exceed 3 blocks. The write is atomic (temp file + rename) to prevent corruption from crashes.

The cursor file (`cursor.json`) is still written by the Gasket cursor stage but is no longer used for resume — it serves as a debug aid to see the current chain position.

## Frontend (web/)

The frontend is a Svelte 5 + TypeScript app built with Vite.

### Sections Pattern (mempool as unfinalized block)

The frontend uses a unified `sections` store (`Section[]`) instead of separate mempool/blocks stores. `sections[0]` is always the mempool — an unfinalized block without a border or header. When a block is confirmed, `sections[0]` is finalized (block metadata attached, border/header appear via CSS), excluded txs move to a new `sections[0]`, and the BinPackGrid instance survives the transition. This means transactions never change DOM containers, so `animate:flip` handles repositioning naturally with no cross-container animation needed.

### Large Integer Serialization

Values that can exceed `Number.MAX_SAFE_INTEGER` (`lovelace`, `fee`, `quantity`) are serialized as JSON strings from Rust (`#[serde(with = "string")]` in `event.rs`) and typed as `string` on the frontend. For display, use string slicing to insert the decimal point rather than float arithmetic. Convert to `BigInt` only when arithmetic is needed. New fields with potentially large values should follow this pattern.

### Event Ordering

SSE events from the server may arrive in any order (pool blocks, delegation changes, snapshot events). The **frontend is responsible for ordering** sections by slot. The server should send events as soon as they are available — never buffer or sort server-side. The frontend must insert blocks at the correct position by slot, not just prepend.

### Coding Guidelines

- **Formatting**: Never use tabs in source files. Always format before committing: `cargo fmt` for Rust, `pnpm prettier --write` for frontend (configured in `web/.prettierrc`).
- **Animations**: Prefer Svelte's built-in animation features (`svelte/animate`, `svelte/transition`) over pure CSS when they provide a better, smoother, or simpler solution. Use `animate:flip` for list reordering, transitions for enter/exit animations.
- **Package versions**: Use LTS or stable versions when possible, particularly for TypeScript, JavaScript runtimes, and Svelte.
- **Type safety**: Prefer specific types over `any`. Use `unknown` when the type is genuinely dynamic, narrowing it before use. `any` is acceptable when interfacing with untyped libraries or when proper typing would require disproportionately complex generics.
- **No macros**: Avoid Rust macros (`macro_rules!`, proc macros) for deduplication or abstraction. Prefer functions, generics, or a small amount of repetition. Ask for confirmation before introducing any macro.
- **Named constants**: Always use named constants instead of hardcoded magic numbers. The name documents the intent and factorizes the value so it can be changed in one place.

