import { writable } from 'svelte/store';
import type { Config, PoolInfo, Section } from './types';

let idCounter = 0;

export function newSection(): Section {
  return {
    id: `s-${idCounter++}`,
    txs: [],
    receivedAt: Date.now(),
  };
}

export const sections = writable<Section[]>([newSection()]);
export const config = writable<Config | null>(null);
export const pool = writable<PoolInfo | null>(null);
