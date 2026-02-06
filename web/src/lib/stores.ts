import { writable } from 'svelte/store';
import type { Section } from './types';

let idCounter = 0;

export function newSection(): Section {
	return {
		id: `s-${idCounter++}`,
		txs: [],
		receivedAt: Date.now(),
	};
}

export const sections = writable<Section[]>([newSection()]);
