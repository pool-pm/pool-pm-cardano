# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

pool-pm-cardano is a Cardano indexer and real-time event server (Rust, single-package Cargo
workspace in `server/`, Svelte 5 frontend in `web/`). It follows a Cardano node over N2C
(chain-sync + LocalTxMonitor via pallas), keeps the chain state it needs in memory, and streams
blocks, mempool txs and rollbacks to browsers over SSE. Anything historical comes from a
cardano-db-sync PostgreSQL database.

## Testing

- **Rust**: unit tests live in a `#[cfg(test)] mod tests` block in the same file. `cargo test`
  compiles the crate, so — like any build — it needs a reachable cardano-db-sync DB (the sqlx
  `query!` macros are validated against the schema at compile time). Test pure logic; queries
  that need the DB aren't unit-testable.
- **Frontend**: Vitest, `*.test.ts` next to the module it covers. Test **pure functions** —
  extract logic out of `.svelte` components into a plain `.ts` module so it's testable without
  a DOM (see `search.ts` + `search.test.ts`).

## Architecture

### Stream Processing Pipeline

Gasket stages, wired in `daemon.rs`:

```
Cardano Node ──N2C chain-sync──> source.rs ──> sink.rs ──> EventBus ──> axum SSE (server.rs)
             ──LocalTxMonitor──> mempool.rs ──────────────────^              ^
                                     PostgreSQL (cardano-db-sync) ───────────┘
```

- **source.rs** is our own N2C chain-sync driver, not oura's: identical behaviour plus a **read
  timeout**, because oura's blocks forever on a half-alive socket and silently freezes the whole
  pipeline (the 2026-07-28 production freeze). It publishes the node's tip and an `at_tip` flag
  so the sink knows when it has drained everything available.
- **sink.rs** applies blocks to the versioned in-memory state and emits `Event::Block` /
  `Event::Rollback`; **mempool.rs** decodes pending txs the same way.
- Feeds are served from that state plus db-sync; `state/feed_index.rs` holds a 5-day per-subject
  index of the blocks each pool/DRep appears in.

### Key Patterns

- **Versioned state**: `State` keeps a `Vec<BlockSnapshot>`; each snapshot shares structure with
  the previous through `imbl`'s O(1) clone. Always put new per-block data **in `BlockSnapshot`**
  so rollbacks come for free by truncating history — never maintain separate delta/undo logic.
- **Rollback correctness is critical**: a `Rollback { slot }` drops every block after `slot` from
  the event bus, the state history and the frontend sections. Any counter, cache or derived value
  fed by blocks must revert cleanly — don't add something that only accumulates forward.
- **Never block the feeds**: db work runs *off* the `chain_state` lock (`db_handle()` + short
  await-free guard scopes). Holding the guard across a slow await starves the sink's per-block
  write lock — the root cause of the 2026-07-28 freeze.
- **One db pool per tokio runtime**: this process runs several — gasket builds one per stage,
  the startup populates use a temporary one, axum has its own. sqlx binds a connection to the
  runtime that created it, so a pool shared across runtimes hands out connections nobody polls
  and the acquire stalls for the full `acquire_timeout` (30s by default). `State::db()` keys its
  pools by `Handle::current().id()`; never hoist one into a `static`/`OnceCell`.

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
  `(cred, addr)` goes down once per address (1.3M, not 14.8M) and the 28-byte policy once per
  (address, policy) run (6.3M, not 14.8M) — the wire is `address → [(policy, [(name, Held)])]`.
  Byte strings use `serde_bytes`: plain `Vec<u8>`/`Box<[u8]>` serialize as msgpack *arrays of
  integers* in rmp-serde (~1.5× the bytes, one visitor call per byte), worth remembering before
  adding a byte field. Together those took the file 3.6 GB → 1.57 GB and the holdings load
  30.0 s → 9.5 s, each step measured back-to-back. Load interns each address **once** and
  inserts its tokens as they stream, so the un-shared full map is never materialized and each
  entry costs exactly one allocation (`AssetIdSeed` writes `policy ++ name` straight into it).
  The other maps' `Vec<u8>` keys (balances, stakes, rewards, delegations, delegator sets) get
  the same treatment through `state/wire.rs` — `#[serde(with = …)]` on the field, so only the
  wire changes and every call site keeps its `Vec<u8>`. Their readers accept **both** encodings,
  which is why that change needed no format bump: 1.57 GB → 1.31 GB and the non-holdings half of
  the load 15.3 s → 9.0 s, measured back-to-back.
  Bump `SNAPSHOT_FORMAT` on any persisted-shape change so old snapshots rebuild from db-sync;
  when the change is cheap to read both ways, a one-release read-only compat path lets a deploy
  resume rather than cold-reset (the grouping change shipped one, then removed it).
  Two `#[ignore]`d benches in `state/mod.rs` time a real file end to end — see
  `SNAPSHOT_BENCH=… cargo test --release snapshot_load_timing -- --nocapture --ignored`.

`BlockSnapshot::log_memory` / `State::log_memory` (called after reset and warm resume) print
a per-field byte breakdown — content bytes, the `asset_holdings` inline/heap split, and the
`addr_interner` size — for reasoning about this layout; RSS is the ground truth.

## Runtime Configuration

`--help` documents the flags (`args.rs`). The server needs a Cardano node socket **and** a
cardano-db-sync PostgreSQL database for the same network.

### Snapshot Persistence

On startup the server loads `{output}/snapshot.bin` and asks the node to resume from its
slot/hash (`IntersectConfig::Point`). Missing, stale or rejected → full reset from db-sync.

- The file leads with `(format, magic)`, checked **before** the multi-GB holdings map is read,
  so a stale or foreign-network snapshot is rejected without deserializing gigabytes only to
  drop them (freed pages the allocator would keep).
- Written after a reset and every `SNAPSHOT_INTERVAL` (50) blocks, `--snapshot-depth` (8) blocks
  behind the tip for rollback safety — so the file on disk is 8–58 blocks old, which is what a
  restart has to replay. The write is atomic (temp file + rename).

## Frontend (web/)

The frontend is a Svelte 5 + TypeScript app built with Vite.

### Sections Pattern (mempool as unfinalized block)

The frontend uses a unified `sections` store (`Section[]`) instead of separate mempool/blocks stores. `sections[0]` is always the mempool — an unfinalized block without a border or header. When a block is confirmed, `sections[0]` is finalized (block metadata attached, border/header appear via CSS), excluded txs move to a new `sections[0]`, and the same grid instance survives the transition. Transactions therefore never change DOM containers, so `animate:flip` handles repositioning on its own — no cross-container animation.

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

