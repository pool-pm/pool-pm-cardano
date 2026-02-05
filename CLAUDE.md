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
- **Cursor**: Persists the current chain position to a JSON file on disk for resumption after restart.
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
- **Versioned state**: `State` maintains a `Vec<BlockSnapshot>` history; each snapshot shares structure with the previous via `im` crate O(1) clone.
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
- `-o, --output` — Directory for cursor file (default: `/tmp/cardano`)
- `-v, --verbose` — Enable DEBUG-level logging

Requires a running cardano-db-sync PostgreSQL database for the target network.

## Frontend (web/)

The frontend is a Svelte 5 + TypeScript app built with Vite.

### Coding Guidelines

- **Animations**: Prefer Svelte's built-in animation features (`svelte/animate`, `svelte/transition`) over pure CSS when they provide a better, smoother, or simpler solution. Use `animate:flip` for list reordering, transitions for enter/exit animations.

## Pending Specs

### Feed Limits

To prevent unbounded memory growth and performance degradation, the feed should enforce limits:

- **Time-based cleanup**: Remove items older than 10 minutes (already implemented)
- **Block cap**: Max 30 blocks (~10 minutes at normal Cardano block rate of ~1 block/20s), drop oldest first
