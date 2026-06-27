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
  media: AssetMedia[];
}

export interface PolicyAsset {
  fingerprint: string;
  name?: string;
  /** Ready-signed nftcdn preview URL (128px rung); use as the img fallback src. */
  src: string;
  /** Multi-rung srcset ("url 1x, url 2x, url 4x"); empty when only one rung. */
  srcset: string;
}

/** Response for both `/api/policy/{id}` and `/api/assets/{bech32}` — same
 * pagination scheme; the subject (policy id or address) is implicit in the URL. */
export interface AssetsResponse {
  assets: PolicyAsset[];
  /** Last asset id of this page; pass back as `?cursor=` for the next page. */
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
  slot: number;
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

/** One `/api/search` hit — a pool ticker or DRep name match. */
export interface SearchResult {
  /** bech32 pool/drep id; used for color and navigation (`/{id}`). */
  id: string;
  label: string;
  kind: 'pool' | 'drep';
  delegators: number;
  /** Live stake in lovelace (string — exceeds Number.MAX_SAFE_INTEGER). */
  live_stake: string;
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
}

export interface StakeInfo {
  stake_address: string;
  balance?: string;
  rewards?: string;
  pool_id?: string;
  pool_ticker?: string | null;
  drep_id?: string;
  drep_name?: string | null;
  /** Distinct multi-assets across every payment address sharing this stake;
   * updated live per block. Plain number — counts won't approach 2^53. */
  assets_count?: number;
}

export interface AddressInfo {
  address: string;
  balance?: string;
  stake_address?: string;
  handle?: string;
  /** Distinct multi-assets currently held; updated live per block. */
  assets_count?: number;
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
