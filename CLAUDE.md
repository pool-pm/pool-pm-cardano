# CLAUDE.md

Rules and non-obvious facts for working in this repo. Everything else — architecture, what a
module does, which constants exist — read from the code.

## Rules

- **Put new per-block state in `BlockSnapshot`**, never in a separate delta/undo structure.
  Rollback works by truncating snapshot history, so anything that only accumulates forward
  breaks it. Every counter, cache and derived value must revert cleanly.
- **Never hold the `chain_state` guard across an await.** Use `db_handle()` and short await-free
  guard scopes; a slow await under the guard starves the sink's per-block write lock and freezes
  the pipeline.
- **Never buffer or sort events server-side** to fix ordering — send as soon as available; the
  frontend orders sections by slot.
- **Serialize potentially large integers as strings** (`#[serde(with = "string")]`, `string` on
  the frontend), display by string slicing, `BigInt` only for arithmetic.
- **Bump `SNAPSHOT_FORMAT`** on any persisted-shape change. If the new shape is cheap to read
  both ways, a read-only compat path lets a deploy resume instead of cold-resetting; remove it
  once the fleet has written the new form.
- **No macros** (`macro_rules!`, proc macros) — functions, generics, or a little repetition.
  Ask before introducing one.
- **Named constants**, never magic numbers.
- **No tabs.** Format before committing: `cargo fmt`, and `pnpm prettier --write` in `web/`.
- Prefer Svelte's own `svelte/animate` / `svelte/transition` over hand-rolled CSS. Don't
  reintroduce cross-container transaction animation — the mempool-as-`sections[0]` design exists
  so txs never change DOM container and `animate:flip` suffices.
- Prefer specific types over `any`; `unknown` + narrowing when genuinely dynamic.
- LTS/stable package versions.

## Testing

- Rust: `#[cfg(test)] mod tests` in the same file. **`cargo test` needs a reachable db-sync DB** —
  sqlx `query!` is validated against the schema at compile time. Test pure logic only.
- Frontend: Vitest, `*.test.ts` beside the module. Extract logic out of `.svelte` into a plain
  `.ts` module so it's testable without a DOM (`search.ts` + `search.test.ts`).

## Non-obvious facts

- **One db pool per tokio runtime.** sqlx binds a connection to the runtime that created it, and
  this process has several (one per gasket stage, axum's, a temporary one for startup populates).
  A shared pool hands out connections nobody polls and the acquire stalls for the full
  `acquire_timeout` (30s). `db_handle()` keys pools by `Handle::current().id()` and creates them
  lazily; never hoist one into a `static`/`OnceCell`.
- **rmp-serde encodes `Vec<u8>`/`Box<[u8]>` as an array of integers, not `bin`** — ~1.5× the bytes
  and one visitor call per byte. Route byte fields through `serde_bytes` or `state/wire.rs`.
- **`asset_holdings` keys compare by value, never `Arc::ptr_eq`** — prev and curr snapshots may
  hold distinct `Arc<AddrKey>` for the same address. Build every key through `intern_addr` /
  `AddrKey::from_query`.
- **`Held` is two `u64`, not a `u128`**, because align 16 would re-pad the align-8 key and erase
  the saving. A `const` assert locks size/align.
- **`AddrInterner` lives in `State`, not `BlockSnapshot`** — rollback-safe only because it grows
  monotonically and is content-addressed.
- **A point-in-time UTXO sum from db-sync is a heap scan** (~18s for a 177k-UTXO address; `value`
  isn't in the partial index). Historical per-block values are reconstructed by walking backward
  from the known current snapshot value instead (`build_subject_replay` / `pre_block_stake`).
- **That backward walk is deliberately not used for address feeds**: blocks and `stake_change` are
  per payment address, but stake is credential-level, so the walk would be wrong. Before reusing
  it elsewhere, verify the replay window contains *every* change to the quantity, and that
  out-of-window contributions come from cheap `addr_id`-indexed queries.
- Snapshot benches (`#[ignore]`d, real file end to end):
  `SNAPSHOT_BENCH=… cargo test --release snapshot_load_timing -- --nocapture --ignored`.
  `log_memory` is DEBUG-gated and skipped entirely when off (it costs ~1s to compute).
