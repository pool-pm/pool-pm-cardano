export interface GenesisConfig {
  shelley_known_slot: number;
  shelley_known_time: number;
  shelley_slot_length: number;
  byron_epoch_length: number;
  byron_slot_length: number;
  shelley_epoch_length: number;
}

export interface Config {
  nftcdn: string;
  magic: number;
  genesis: GenesisConfig;
}

export interface TxInput {
  tx_hash: string;
  index: number;
  address: string | null;
  lovelace: string;
  assets?: AssetInfo[];
  handle?: string;
}

export interface TxOutputInfo {
  address: string;
  lovelace: string;
  assets: AssetInfo[];
  handle?: string;
}

export interface AssetInfo {
  fingerprint: string;
  name?: string;
  quantity: string;
  tk?: string;
  /** Server-negotiated image size (nftcdn power-of-2 rung for this client's DPR). */
  size: number;
}

export interface AssetMedia {
  src: string;
  type?: string;
  name: string;
}

export interface AssetMediaResponse {
  fingerprint: string;
  name?: string;
  /** Policy id (hex); links to the policy page. */
  policy?: string;
  /** Minted supply (string: can exceed JS safe-int). */
  quantity?: string;
  /** First / last mint times (unix seconds); a range when minted across several txs. */
  first_mint?: number;
  last_mint?: number;
  /** Current owner (NFTs only): `owner` is the `/…/assets` link subject (stake1…/addr1…),
   *  `owner_handle` its ADA Handle name (without the leading `$`) if any. */
  owner?: string;
  owner_handle?: string;
  /** Raw on-chain CIP-25/68 metadata object, for the page to format. */
  metadata?: Record<string, unknown>;
  media: AssetMedia[];
}

export interface PolicyAsset {
  fingerprint: string;
  name?: string;
  /** Policy id (hex) — lets the owned-assets grid group/route tiles by policy. */
  policy: string;
  /** Owned quantity, decimals-formatted; present only when it isn't 1 (owned-assets
   * tiles only — absent on the policy-browse grid). */
  quantity?: string;
  /** Ready-signed nftcdn preview URL (128px rung); use as the img fallback src. */
  src: string;
  /** Multi-rung srcset ("url 1x, url 2x, url 4x"); empty when only one rung. */
  srcset: string;
}

/** One policy's tile on the owned-assets grid: its held-asset `count` and up to a few
 * sample tiles for the stacked-card thumbnail. `count === 1` renders as a plain tile. */
export interface AssetGroup {
  policy: string;
  count: number;
  samples: PolicyAsset[];
}

/** A removed tile: `fingerprint` says which tile to drop; `policy` says which group to
 * decrement (a fingerprint can't be mapped back to a policy client-side). */
export interface AssetRef {
  policy: string;
  fingerprint: string;
}

/** Live `AssetDelta` SSE message on an assets feed: this connection's holdings change
 * for one block, derived server-side by diffing the subject's holdings against the
 * previous snapshot. `added` are ready-to-render tiles; `removed` are fingerprints. A
 * rollback arrives as an ordinary corrective delta (against the reverted snapshot), so
 * the grid applies it the same way. */
export interface AssetDeltaEvent {
  type: 'AssetDelta';
  slot: number;
  added?: PolicyAsset[];
  removed?: AssetRef[];
}

/** What `onAssetLive` delivers to the assets grid: one corrective delta to apply. */
export type AssetDelta = { slot: number; added: PolicyAsset[]; removed: AssetRef[] };

/** Response for both `/api/policy/{id}` and `/api/assets/{bech32}` — same
 * pagination scheme; the subject (policy id or address) is implicit in the URL. */
export interface AssetsResponse {
  assets: PolicyAsset[];
  /** Last asset id of this page; pass back as `?cursor=` for the next page. */
  cursor?: number;
  has_more: boolean;
}

/** `/api/assets/{subject}` — the owned-assets grid grouped by policy, paginated by policy. */
export interface GroupsResponse {
  groups: AssetGroup[];
  cursor?: number;
  has_more: boolean;
}

export interface DelegationInfo {
  stake_address: string;
  from_pool_id?: string;
  from_ticker?: string;
  to_pool_id?: string;
  to_ticker?: string;
  from_drep_id?: string;
  from_drep_name?: string;
  to_drep_id?: string;
  to_drep_name?: string;
  live_stake: string;
}

export interface VoteInfo {
  voter_role: string;
  voter_id: string;
  voter_name?: string;
  vote: string;
  action_tx_hash: string;
  action_index: number;
  action_title?: string;
}

export interface CatalystInfo {
  stake_address: string;
  live_stake?: string;
}

export interface OracleInfo {
  source: string;
  feed?: string;
  value?: string;
  /** POSIX milliseconds. */
  valid_from?: number;
  valid_until?: number;
}

/** Protocol-specific description of a tx, discriminated by `kind`. Add a protocol by
 * adding a member here and rendering it in Transaction.svelte — no new BlockTx field. */
export type TxAnnotation = { kind: 'oracle' } & OracleInfo;

export interface BlockTx {
  hash: string;
  fee: string;
  size: number;
  inputs: TxInput[];
  outputs: TxOutputInfo[];
  expiry?: number;
  delegations?: DelegationInfo[];
  votes?: VoteInfo[];
  message?: string[];
  stake_change?: string;
  /** Pool/DRep stake-change txs: the feed's delegator stake address(es) this tx moved (the
   * relevant account(s) among possibly many). The folded view shows these, not raw addresses. */
  stake_addresses?: string[];
  catalyst?: CatalystInfo;
  annotations?: TxAnnotation[];
}

export interface MempoolTxEvent extends BlockTx {
  type: 'MempoolTx';
}

export interface BlockEvent {
  type: 'Block';
  slot: number;
  hash: string;
  number: number;
  timestamp: number;
  /** Serialized block size in bytes (folded pool-own blocks show this as KB). */
  size: number;
  pool_id?: string;
  pool_ticker?: string;
  txs: BlockTx[];
}

export interface RollbackEvent {
  type: 'Rollback';
  slot: number;
}

export interface MempoolPruneEvent {
  type: 'MempoolPrune';
  removed: string[];
}

export interface ReplayCursorEvent {
  type: 'ReplayCursor';
  /** Stake/address slot-walk cursor. Absent on pool/DRep feeds (empty marker that
   *  just enables scrolling; pagination pages from the tip by keyset id). */
  slot?: number;
  epoch?: number;
  stake?: string;
}

/** One row of a per-epoch REWARDS capsule: the reward `type` as `label`, the
 * amount (lovelace string), and the source pool for pool rewards (member/leader). */
export interface RewardRow {
  label: string;
  amount: string;
  pool_id?: string;
  pool_ticker?: string;
}

export interface RewardEvent {
  type: 'Reward';
  epoch: number;
  slot: number;
  timestamp: number;
  rows: RewardRow[];
}

export type Event = MempoolTxEvent | BlockEvent | RollbackEvent | MempoolPruneEvent | ReplayCursorEvent | RewardEvent;

/** Homepage network stats (the global feed's "subject"). */
export interface CardanoInfo {
  /** Circulating supply in lovelace (string — exceeds Number.MAX_SAFE_INTEGER). */
  circulation: string;
  pool_count: number;
  drep_count: number;
  /** % of circulating ADA delegated to pools, one decimal. */
  staked_percent: number;
}

/** One `/api/search` hit — a pool ticker, DRep name, or ADA Handle match. */
export interface SearchResult {
  /** For pool/drep: the bech32 id. For a handle: the holder's payment address.
   * Used for color and navigation (`/{id}`). */
  id: string;
  /** Ticker / DRep name, or the handle (without the leading `$`). */
  label: string;
  kind: 'pool' | 'drep' | 'handle';
  /** Pool/drep only. */
  delegators?: number;
  /** Live stake in lovelace (string — exceeds Number.MAX_SAFE_INTEGER). Pool/drep only. */
  live_stake?: string;
}

export interface DRepInfo {
  drep_id: string;
  given_name: string | null;
  live_stake: string;
  delegators: number;
}

export interface PoolInfo {
  pool_id: string;
  ticker: string | null;
  pledge: string;
  margin: number;
  fixed_cost: string;
  live_stake: string;
  delegators: number;
  /** Lifetime blocks minted, updated live as the pool mints. */
  blocks: number;
  /** The epoch `epoch_blocks` is counted for (the chain's current epoch when sent). */
  epoch: number;
  /** Exact blocks minted by the pool in `epoch` (from the server's full feed index).
   * Shown while the displayed epoch matches `epoch`; reset to 0 once the epoch rolls over. */
  epoch_blocks: number;
}

export interface StakeInfo {
  stake_address: string;
  balance?: string;
  rewards?: string;
  pool_id?: string;
  pool_ticker?: string | null;
  drep_id?: string;
  drep_name?: string | null;
  /** Shortest ADA Handle owned across this stake credential's payment addresses, if any
   * (updated live per block). Shown on the stake assets page as "$handle's stake". */
  handle?: string;
  /** Distinct multi-assets across every payment address sharing this stake;
   * always present (read from the in-memory holdings map), updated live per
   * block. Plain number — counts won't approach 2^53. */
  assets_count: number;
}

export interface AddressInfo {
  address: string;
  balance?: string;
  stake_address?: string;
  /** Total live stake (balance + rewards) of this address's stake credential,
   * lovelace as a string; absent for enterprise/pointer addresses. */
  stake_value?: string;
  /** Distinct multi-assets across this address's whole stake credential; absent
   * for enterprise/pointer addresses. */
  stake_assets_count?: number;
  handle?: string;
  /** Pool + DRep this address's stake credential delegates to (same as the linked
   * stake feed); absent when not delegated / no stake part. */
  pool_id?: string;
  pool_ticker?: string | null;
  drep_id?: string;
  drep_name?: string | null;
  /** Distinct multi-assets currently held; always present (in-memory),
   * updated live per block. */
  assets_count: number;
}

export interface FeedTx extends BlockTx {
  receivedAt: number;
}

export interface Section {
  id: string;
  txs: FeedTx[];
  block?: {
    slot: number;
    hash: string;
    number: number;
    timestamp: number;
    size: number;
    pool_id?: string;
    pool_ticker?: string;
  };
  /** A per-epoch REWARDS capsule (positioned at the epoch-change `slot`/`timestamp`).
   * Mutually exclusive with `block`; has no `txs`. */
  reward?: {
    epoch: number;
    slot: number;
    timestamp: number;
    rows: RewardRow[];
  };
  receivedAt: number;
}
