# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

pool-pm-cardano is a Cardano blockchain indexer written in Rust. It connects to Cardano peer nodes via the N2N protocol (using Oura/Pallas), processes chain events (blocks and rollbacks), and tracks stake pools, delegations, UTXOs, and stake information. Data is queried from a PostgreSQL database populated by cardano-db-sync.

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

The project is a Cargo workspace with a single package (`indexer/`).

### Stream Processing Pipeline

The indexer uses the **Gasket** framework to build a multi-stage stream processing pipeline:

```
Cardano Peers (N2N) → Source Stage (Oura) → Sink Stage → Cursor Stage (JSON file)
                                                ↕
                                        PostgreSQL (cardano-db-sync)
```

- **Source**: Oura N2N source connects to Cardano peer nodes and emits `ChainEvent`s (blocks or rollback points). Configured in `chain.rs`.
- **Sink** (`sink.rs`): The core processing stage. A Gasket `Worker` that receives chain events and maintains in-memory state using immutable data structures (`im` crate `HashMap`/`HashSet`). On each block it fetches pools from the DB, tracks delegations, and tracks UTXOs/stakes.
- **Cursor**: Persists the current chain position to a JSON file on disk for resumption after restart.
- **Daemon** (`daemon.rs`): Orchestrates the full Gasket pipeline with retry policies and optional Prometheus metrics.

### Key Modules

- `args.rs` — CLI argument parsing via clap (peers, network, db connection, metrics endpoint, output dir)
- `chain.rs` — Cardano network configuration (mainnet/preprod/preview magic numbers, known peers, genesis hashes)
- `dbsync.rs` — Async PostgreSQL queries via sqlx against cardano-db-sync schema (pools, delegations, UTXOs)
- `model.rs` — Data structures: `Pool`, `TxOutput`
- `sink.rs` — Gasket worker stage that processes chain events into indexed state

### Key Patterns

- **Immutable data structures**: State is held in `im::HashMap`/`im::HashSet` for safe structural sharing.
- **Gasket error handling**: Worker methods return `gasket::error::Error` with `or_panic()` / `or_retry()` combinators.
- **Async throughout**: tokio runtime with sqlx async database access.
- **Compile-time SQL checking**: sqlx `query_as!` macros validate SQL against the DB schema at compile time.

## Runtime Configuration

The indexer is configured via CLI args:
- `-n, --network` — mainnet (default), preprod, or preview
- `-d, --db` — PostgreSQL connection string (default: `postgresql:///NETWORK?host=/var/run/postgresql`)
- `-p, --peers` — Cardano node peer addresses (space-delimited)
- `-m, --metrics` — Prometheus metrics endpoint (`ADDR:PORT` or `default` for `127.0.0.1:9188`)
- `-o, --output` — Directory for cursor file (default: `/tmp/cardano`)
- `-v, --verbose` — Enable DEBUG-level logging

Requires a running cardano-db-sync PostgreSQL database for the target network.
