import { writable } from 'svelte/store';
import type { FeedTx, FeedBlock } from './types';

export const mempoolTxs = writable(new Map<string, FeedTx>());
export const blocks = writable(new Map<string, FeedBlock>());
