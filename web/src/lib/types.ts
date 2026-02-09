export interface Config {
	nftcdn: string;
}

export interface TxInput {
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

export interface BlockTx {
	hash: string;
	fee: string;
	size: number;
	inputs: TxInput[];
	outputs: TxOutputInfo[];
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

export type Event = MempoolTxEvent | BlockEvent | RollbackEvent;

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
