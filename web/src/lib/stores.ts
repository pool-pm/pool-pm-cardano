import { writable } from 'svelte/store';
import type { AddressInfo, CardanoInfo, Config, DRepInfo, PoolInfo, Section, StakeInfo } from './types';

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
export const drep = writable<DRepInfo | null>(null);
export const stake = writable<StakeInfo | null>(null);
export const address = writable<AddressInfo | null>(null);
/** Homepage network stats; set only on the global feed. */
export const cardano = writable<CardanoInfo | null>(null);
/** Bumped each time a block is received, so Feed can run cleanup reactively. */
export const blockCount = writable(0);
