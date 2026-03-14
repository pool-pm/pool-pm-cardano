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

There are no tests in this project currently.

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
- **Sink** (`sink.rs`): Processes chain events, maintains versioned in-memory state using immutable data structures (`im` crate) with structural sharing. On blocks: updates UTXOs, sends `Event::Block`. On rollback: reverts state, sends `Event::Rollback`.
- **Mempool** (`mempool.rs`): Monitors the node mempool via LocalTxMonitor mini-protocol. Decodes transactions, resolves input addresses from UTXO state, computes CIP-14 asset fingerprints, and sends `Event::MempoolTx`.
- **Cursor**: Persists the current chain position to a JSON file on disk (kept for debugging, not used for resume).
- **Snapshot**: Serializes a `BlockSnapshot` to `{output}/snapshot.bin` using bitcode for fast resume on restart (see Snapshot Persistence below).
- **SSE Server** (`server.rs`): axum HTTP server with `GET /events` endpoint that streams events as JSON via Server-Sent Events.
- **Daemon** (`daemon.rs`): Orchestrates the full Gasket pipeline with retry policies, optional Prometheus metrics, and SSE server.

### Key Modules

- `args.rs` — CLI argument parsing via clap (socket, network, db connection, metrics endpoint, listen address, output dir)
- `chain.rs` — Cardano network configuration (mainnet/preprod/preview magic numbers via Oura GenesisValues)
- `dbsync.rs` — Async PostgreSQL queries via sqlx against cardano-db-sync schema (pools, delegations, UTXOs, stakes)
- `event.rs` — Shared event types: `MempoolTx`, `Block`, `Rollback` (serializable for SSE)
- `model.rs` — Data structures: `Pool`, `TxOutput`, CIP-14 asset fingerprint computation
- `state.rs` — Versioned state with `BlockSnapshot` history and structural sharing for O(1) rollbacks
- `mempool.rs` — Gasket worker stage for mempool monitoring via LocalTxMonitor
- `sink.rs` — Gasket worker stage that processes chain events into indexed state
- `server.rs` — axum SSE server for streaming events to web clients

### Key Patterns

- **Immutable data structures**: State is held in `im::HashMap`/`im::HashSet` for safe structural sharing and efficient rollbacks.
- **Versioned state**: `State` maintains a `Vec<BlockSnapshot>` history; each snapshot shares structure with the previous via `im` crate O(1) clone. Always store new per-block data in `BlockSnapshot` so rollbacks are handled automatically by history truncation — never maintain separate delta/rollback logic.
- **Event broadcasting**: `tokio::sync::broadcast` channel fans out events from pipeline stages to multiple SSE clients.
- **Gasket error handling**: Worker methods return `gasket::error::Error` with `or_panic()` / `or_retry()` combinators.
- **Async throughout**: tokio runtime with sqlx async database access.
- **Compile-time SQL checking**: sqlx `query_as!` macros validate SQL against the DB schema at compile time.

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

### Coding Guidelines

- **Formatting**: Never use tabs in source files. Always format before committing: `cargo fmt` for Rust, `pnpm prettier --write` for frontend (configured in `web/.prettierrc`).
- **Animations**: Prefer Svelte's built-in animation features (`svelte/animate`, `svelte/transition`) over pure CSS when they provide a better, smoother, or simpler solution. Use `animate:flip` for list reordering, transitions for enter/exit animations.
- **Package versions**: Use LTS or stable versions when possible, particularly for TypeScript, JavaScript runtimes, and Svelte.
- **Type safety**: Prefer specific types over `any`. Use `unknown` when the type is genuinely dynamic, narrowing it before use. `any` is acceptable when interfacing with untyped libraries or when proper typing would require disproportionately complex generics.
- **No macros**: Avoid Rust macros (`macro_rules!`, proc macros) for deduplication or abstraction. Prefer functions, generics, or a small amount of repetition. Ask for confirmation before introducing any macro.

