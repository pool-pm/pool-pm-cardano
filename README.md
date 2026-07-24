# pool-pm-cardano

A Cardano blockchain indexer and real-time event server. It follows a Cardano node over
**node-to-client (N2C)** for chain sync and the mempool, and fetches historical block
bodies over **node-to-node (N2N)** for feed replay. It maintains versioned in-memory state
and streams events to web clients over Server-Sent Events (SSE). Historical and aggregate
data is read from a `cardano-db-sync` PostgreSQL database. The repository also contains the
`pool.pm` web frontend.

## Overview

- **Source**: an Oura N2C source streams chain events (blocks / rollbacks) from a local node.
- **Sink**: maintains versioned state (UTXOs, stakes, rewards, pool/DRep delegations,
  per-address asset holdings, …) in persistent `imbl` structures with structural sharing,
  so each block is an O(1) snapshot and rollbacks are exact.
- **Mempool**: monitors pending transactions via the LocalTxMonitor mini-protocol.
- **SSE server**: an axum HTTP server streams per-subject feeds (address, stake, pool,
  DRep, policy, asset) at `/events` + a small `/api/*` surface. A new connection is first
  replayed its subject's recent blocks (bodies fetched over N2N) before going live.
- **Snapshot**: state is persisted to disk (msgpack) for fast restart. On a missing or
  incompatible snapshot the indexer rebuilds full state from db-sync.

See `CLAUDE.md` for architecture details.

## Layout

- `server/`: the Rust indexer + SSE server (Cargo workspace).
- `web/`: the Svelte 5 + TypeScript frontend (Vite, pnpm).

## Dependencies

### Build

- **Rust** stable, edition 2021 (built with 1.92).
- Key crates: [`oura`](https://github.com/txpipe/oura) (N2C source) and
  [`pallas`](https://github.com/txpipe/pallas) (Cardano primitives) from TxPipe, plus `gasket`
  (stream pipeline), `sqlx` (PostgreSQL), `axum` + `tokio` (server), and `imbl` (persistent maps).
- `sqlx` checks SQL **at compile time**. The repo ships a `.sqlx/` offline query cache, so a
  normal build needs **no database**. Set `DATABASE_URL` (to a db-sync instance) only to
  check queries against a live schema, or to regenerate the cache after changing a query
  (see [Build](#build-1)).
- Frontend: **Node.js** + **pnpm**, Vite, Svelte 5.

### Runtime

- A synced **cardano-node**: provides the N2C socket and the N2N block-fetch address.
- **cardano-db-sync** and its **PostgreSQL** database for the target network.
- **NFTCDN** ([nftcdn.io](https://nftcdn.io)): third-party CDN that serves and signs asset
  media (thumbnails / metadata). Mainnet requires an account (a subdomain + an HMAC signing
  key). See [Environment](#environment).

## Environment

`dotenvy` loads a `.env` file at startup, so these may be set there or in the process
environment.

| Variable | When | Description |
| --- | --- | --- |
| `NFTCDN_KEY` | runtime (mainnet) | Base64 HMAC key from your NFTCDN account, used to sign asset-media URLs. **Required on mainnet** (the server panics on start without it). Preprod uses NFTCDN's public test key, and preview serves unsigned URLs. The mainnet subdomain is hardcoded to `poolpm.nftcdn.io` in `server/src/nftcdn.rs`. A different deployer must set their own subdomain there alongside their key. |
| `DATABASE_URL` | build (optional) | db-sync connection for `sqlx`'s compile-time query checking. Optional: builds use the committed `.sqlx/` cache when unset. Set it to verify against a live schema or to run `cargo sqlx prepare`. Build-only, the runtime database is selected with `--db`. |

## Hardware

The indexer keeps full per-address state in memory. On **mainnet** the steady-state
resident set is roughly **~10 GB** (the ~15M-entry asset-holdings map dominates; its
`(cred, addr)` keys are interned and its leaf is a packed 128-bit value to keep it compact),
so budget **≥ 16 GB RAM** for the indexer alone. A complete node + db-sync stack additionally
needs a large PostgreSQL (≈ 1 TB NVMe for mainnet), so a single-host deployment realistically
wants **64 GB+ RAM** and fast SSD/NVMe storage.

## cardano-db-sync configuration

The indexer is written against a specific db-sync schema variant, so db-sync must run with
these non-default `insert_options` (in its config YAML):

```yaml
insert_options:
  tx_out:
    value: consumed          # keep `tx_out.consumed_by_tx_id`, required for all the
                             # unspent/consumed UTXO queries (balances, holdings, feeds)
    use_address_table: true  # tx_out references `address(id)` via `address_id`, and the
                             # indexer joins the `address` table and reads `address.raw`
  offchain_pool_data: enable # `off_chain_pool_data`: pool tickers
  offchain_vote_data: enable # `off_chain_vote_data`: DRep names + governance metadata
```

Governance and multi-asset inserts must also be on. Both are db-sync defaults, so just
don't disable them (the indexer reads `delegation_vote`, `drep_*`, `reward_rest`,
`multi_asset`, `ma_tx_out`, `ma_tx_mint`). The two `tx_out` settings change the schema, so
toggling them on an existing database requires a full db-sync resync.

The indexer doesn't read the following, so they can be left off to keep the database
smaller (these are the space-saving choices, not requirements):

```yaml
insert_options:
  tx_out:
    force_tx_in: false   # `tx_in` is unused (with value: consumed it's redundant)
  pool_stat: disable     # per-epoch pool stats (unused)
  tx_cbor: disable       # raw transaction CBOR (unused)
```

## PostgreSQL indexes

The indexer needs custom indexes beyond db-sync's defaults for acceptable query latency.
Create them as the **owner of the db-sync tables** (the role db-sync runs as, e.g.
`cardano`). `CREATE INDEX CONCURRENTLY` must run **outside a transaction**. Execute the
statements one at a time, not inside a `BEGIN`/`COMMIT` block.

```sql
-- Address & stake feeds + owned-assets: unspent partials and ordered composites on tx_out.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_tx_out_address_id_unspent ON tx_out (address_id) WHERE consumed_by_tx_id IS NULL;
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_tx_out_address_tx       ON tx_out (address_id, tx_id);
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_tx_out_address_consumed ON tx_out (address_id, consumed_by_tx_id) WHERE consumed_by_tx_id IS NOT NULL;
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_tx_out_stake_unspent    ON tx_out (stake_address_id) WHERE consumed_by_tx_id IS NULL;
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_tx_out_stake_tx         ON tx_out (stake_address_id, tx_id);
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_tx_out_stake_consumed   ON tx_out (stake_address_id, consumed_by_tx_id) WHERE consumed_by_tx_id IS NOT NULL;

-- Withdrawals in stake / pool / DRep feeds.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_withdrawal_addr_tx ON withdrawal (addr_id, tx_id);

-- Multi-asset lookups: policy page, asset page, holdings, CIP-68 decimals.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_multi_asset_fingerprint ON multi_asset (fingerprint);
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_ma_tx_mint_ident        ON ma_tx_mint (ident);
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_ma_tx_out_ident         ON ma_tx_out (ident);
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_multi_asset_cip68_ft    ON multi_asset (policy, name) WHERE substring(name FROM 1 FOR 4) IN ('\x0014df10', '\x001bc280');

-- Governance / rewards / DRep.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_delegation_vote_addr_id        ON delegation_vote (addr_id);
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_reward_rest_addr_id            ON reward_rest (addr_id);
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_drep_registration_drep_hash_id_id ON drep_registration (drep_hash_id, id);
```

## Build

```bash
# Server: builds offline from the committed .sqlx/ query cache (no database needed).
cargo build --release          # -> target/release/server

# Frontend
cd web && pnpm install && pnpm build   # -> web/dist
```

After changing or adding a SQL query, regenerate the `.sqlx/` cache against a live db-sync
and commit it:

```bash
cargo install sqlx-cli --no-default-features --features rustls,postgres
DATABASE_URL='postgresql:///mainnet?host=/var/run/postgresql' cargo sqlx prepare --workspace
```

## Run

```bash
./target/release/server \
  --socket  /run/cardano-node/node.socket \
  --network mainnet \
  --db      'postgresql:///mainnet?host=/var/run/postgresql' \
  --n2n     127.0.0.1:3001 \
  --listen  0.0.0.0:3000
```

| Flag | Default | Description |
| --- | --- | --- |
| `-s, --socket <PATH>` | _(required)_ | Cardano node N2C socket |
| `-n, --network <NET>` | `mainnet` | `mainnet`, `preprod`, or `preview` |
| `-d, --db <URL>` | `postgresql:///NETWORK?host=/var/run/postgresql` | db-sync connection (`NETWORK` is replaced by the network name) |
| `--n2n <ADDR:PORT>` | `127.0.0.1:3001` | node-to-node address for block-fetch (feed replay) |
| `-l, --listen <ADDR:PORT>` | _(off)_ | SSE server address (omit to disable the server) |
| `-m, --metrics <ADDR:PORT｜default>` | _(off)_ | Prometheus metrics endpoint |
| `-o, --output <DIR>` | `/tmp/cardano` | snapshot / cursor files |
| `--snapshot-depth <N>` | `8` | blocks behind tip for the persisted snapshot |
| `-v, --verbose` | _(off)_ | DEBUG logging |

First start (or a missing/incompatible snapshot) triggers a full rebuild from db-sync, an
expensive cold start that runs the heavy queries the indexes above support. Later restarts
resume from the snapshot in `--output`.

Serve `web/dist` with any static web server / reverse proxy. The frontend calls the
server's `/events` and `/api/*` endpoints.

## Development

```bash
cargo test                 # server unit tests (needs a reachable db-sync DB)
cargo clippy
cd web && pnpm test        # frontend unit tests (Vitest)
cd web && pnpm dev         # frontend dev server (Vite)
```

## Deployment

- Run the indexer as a long-lived service (systemd or similar), on or near the node +
  db-sync host so the N2C socket is local and PostgreSQL latency is low.
- Front it with a reverse proxy (nginx, Caddy, …) to terminate TLS, serve `web/dist`, and
  proxy `/events` and `/api/*` to `--listen`. For the SSE location, **disable response
  buffering** and use a long read timeout.
- **Social cards** (Open Graph / Twitter for link unfurls on X, Telegram, Discord, …): the
  server answers any *unmatched* path with a server-rendered card — the SPA can't, since
  crawlers don't run JS. So the proxy must send **link-unfurl crawler User-Agents** to
  `--listen` for page (HTML) requests, while serving the static SPA to everyone else. Static
  files — including `web/dist/logo.png` / `logo_square.png` (the non-asset card image) — must be
  served directly even to crawlers, so `og:image` fetches return the image, not a card. Forward
  the original `Host` so the card's absolute `og:url` / `og:image` use the real domain. Any web
  server works; a minimal nginx form:

  ```nginx
  map $http_user_agent $og_crawler {
      default 0;
      "~*(Twitterbot|facebookexternalhit|TelegramBot|Discordbot|Slackbot|WhatsApp|LinkedInBot|redditbot|Applebot)" 1;
  }
  server {
      root /path/to/web/dist;
      location /events { proxy_pass http://127.0.0.1:3000; proxy_buffering off; proxy_read_timeout 24h; }
      location /api/   { proxy_pass http://127.0.0.1:3000; }
      # static files first, so a crawler's og:image (/logo.png) is served, not a card
      location ~* \.(js|css|png|jpe?g|svg|webp|ico|woff2?|txt|xml|webmanifest|map)$ { try_files $uri =404; }
      location / {
          proxy_set_header Host $host;
          if ($og_crawler) { proxy_pass http://127.0.0.1:3000; }
          try_files $uri /index.html;
      }
  }
  ```

  Verify with `curl -A Twitterbot https://<domain>/asset1…` (and `/pool1…`, `/`).
- Persist `--output` on durable storage so restarts resume from the snapshot instead of a
  full rebuild.
- Create the indexes and provision the RAM for the cold-start rebuild before first run.

## License

[MIT](LICENSE)

Compatible with the project's dependency licenses: Apache-2.0 (oura, pallas, gasket), MIT or
Apache-2.0 (tokio, sqlx, axum, …), and MPL-2.0 (imbl).
