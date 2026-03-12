export interface GenesisConfig {
  shelley_known_slot: number;
  shelley_known_time: number;
  shelley_slot_length: number;
  byron_epoch_length: number;
  shelley_epoch_length: number;
}

export interface Config {
  nftcdn: string;
  genesis: GenesisConfig;
}

export interface TxInput {
  tx_hash: string;
  index: number;
  address: string | null;
  lovelace: string;
}

export interface TxOutputInfo {
  address: string;
  lovelace: string;
  assets: AssetInfo[];
}

export interface AssetInfo {
  fingerprint: string;
  name?: string;
  quantity: string;
  tk?: string;
}

export interface DelegationInfo {
  stake_address: string;
  from_pool_id?: string;
  from_ticker?: string;
  to_pool_id?: string;
  to_ticker?: string;
}

export interface BlockTx {
  hash: string;
  fee: string;
  size: number;
  inputs: TxInput[];
  outputs: TxOutputInfo[];
  expiry?: number;
  delegations?: DelegationInfo[];
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
