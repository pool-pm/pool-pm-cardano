export interface TxInput {
	address: string | null;
	lovelace: number;
}

export interface TxOutputInfo {
	address: string;
	lovelace: number;
	assets: AssetInfo[];
}

export interface AssetInfo {
	fingerprint: string;
	quantity: number;
}

export interface MempoolTxEvent {
	type: 'MempoolTx';
	hash: string;
	fee: number;
	size: number;
	inputs: TxInput[];
	outputs: TxOutputInfo[];
}

export interface BlockEvent {
	type: 'Block';
	slot: number;
	hash: string;
	number: number;
	timestamp: number;
	tx_hashes: string[];
}

export interface RollbackEvent {
	type: 'Rollback';
	slot: number;
}

export type Event = MempoolTxEvent | BlockEvent | RollbackEvent;

export interface FeedTx extends MempoolTxEvent {
	receivedAt: number;
}

export interface FeedBlock extends BlockEvent {
	receivedAt: number;
	txs: FeedTx[];
}
