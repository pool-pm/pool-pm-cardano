import { writable } from 'svelte/store';
import type { Config, Section } from './types';

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
