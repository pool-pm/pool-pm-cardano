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

export type Event = MempoolTxEvent | BlockEvent | RollbackEvent | MempoolPruneEvent;

export interface DRepInfo {
  drep_id: string;
  given_name: string | null;
  live_stake?: string;
  delegators?: number;
}

export interface PoolInfo {
  pool_id: string;
  ticker: string | null;
  pledge: string;
  margin: number;
  fixed_cost: string;
  live_stake?: string;
  delegators?: number;
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
  receivedAt: number;
}
