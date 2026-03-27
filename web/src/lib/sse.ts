import { sections, newSection, config, pool } from './stores';
import type { BlockEvent, BlockTx, Config, Event, MempoolTxEvent, PoolInfo, Section } from './types';

let source: EventSource | null = null;
let pendingPrune = new Set<string>();

/** Resolve unresolved input addresses from other txs' outputs in the same set */
function resolveInputs(txs: BlockTx[]): void {
  const outputsByHash = new Map(txs.map((tx) => [tx.hash, tx.outputs]));
  for (const tx of txs) {
    for (const input of tx.inputs) {
      if (!input.address) {
        const output = outputsByHash.get(input.tx_hash)?.[input.index];
        if (output) {
          input.address = output.address;
          input.lovelace = output.lovelace;
        }
      }
    }
  }
}

function handleSnapshot(events: Event[]): void {
  const now = Date.now();
  const blockEvents: BlockEvent[] = [];
  const mempoolTxEvents: MempoolTxEvent[] = [];

  for (const event of events) {
    if (event.type === 'Block') blockEvents.push(event);
    else if (event.type === 'MempoolTx') mempoolTxEvents.push(event);
  }

  // Build block sections (snapshot arrives oldest-first, sections are newest-first)
  const blockSections: Section[] = blockEvents
    .map((event) => {
      const section = newSection();
      section.block = {
        slot: event.slot,
        hash: event.hash,
        number: event.number,
        timestamp: event.timestamp,
        pool_id: event.pool_id,
        pool_ticker: event.pool_ticker,
      };
      section.txs = event.txs.map((tx) => ({ ...tx, receivedAt: now }));
      return section;
    })
    .reverse();

  // Build mempool
  const mempool = newSection();
  mempool.txs = mempoolTxEvents.map((tx) => ({ ...tx, receivedAt: now }));
  resolveInputs(mempool.txs);

  sections.set([mempool, ...blockSections]);
}

function handleEvent(event: Event): void {
  switch (event.type) {
    case 'MempoolTx': {
      const now = Date.now();
      sections.update((s) => {
        const mempool = s[0];
        if (mempool.txs.some((t) => t.hash === event.hash)) return s;
        mempool.txs = [{ ...event, receivedAt: now }, ...mempool.txs];
        resolveInputs(mempool.txs);
        return [...s];
      });
      break;
    }

    case 'Block': {
      const now = Date.now();
      sections.update((s) => {
        // Deduplicate: skip if this block is already in sections (from history replay)
        if (s.some((sec, i) => i > 0 && sec.block?.slot === event.slot)) return s;
        const mempool = s[0];
        const mempoolByHash = new Map(mempool.txs.map((tx) => [tx.hash, tx]));
        const blockByHash = new Map(event.txs.map((tx) => [tx.hash, tx]));

        const excluded = mempool.txs.filter((tx) => !blockByHash.has(tx.hash));

        // Mempool-order first (keep mempool version for visual continuity),
        // then block-only txs appended
        const inBlock = [
          ...mempool.txs.filter((tx) => blockByHash.has(tx.hash)),
          ...event.txs.filter((tx) => !mempoolByHash.has(tx.hash)).map((tx) => ({ ...tx, receivedAt: now })),
        ];

        // Finalize current mempool as a block
        mempool.block = {
          slot: event.slot,
          hash: event.hash,
          number: event.number,
          timestamp: event.timestamp,
          pool_id: event.pool_id,
          pool_ticker: event.pool_ticker,
        };
        mempool.txs = inBlock;

        // New mempool with excluded txs, minus any pruned
        const next = newSection();
        next.txs = excluded.filter((tx) => !pendingPrune.has(tx.hash));
        pendingPrune.clear();

        return [next, ...s];
      });
      break;
    }

    case 'MempoolPrune': {
      for (const h of event.removed) pendingPrune.add(h);
      break;
    }

    case 'Rollback':
      sections.update((s) => {
        return s.filter((section, i) => i === 0 || !section.block || section.block.slot <= event.slot);
      });
      break;
  }
}

export function connectSSE(url: string): void {
  if (source) source.close();

  // Reset stores immediately so stale data from previous pool doesn't linger
  sections.set([newSection()]);
  config.set(null);
  pool.set(null);
  pendingPrune.clear();

  source = new EventSource(url);

  source.onmessage = (e: MessageEvent) => {
    try {
      const data = JSON.parse(e.data);

      if (data.type === 'Config') {
        config.set(data as Config);
      } else if (data.type === 'Pool') {
        pool.set(data as PoolInfo);
      } else if (Array.isArray(data)) {
        handleSnapshot(data as Event[]);
      } else {
        handleEvent(data as Event);
      }
    } catch (err) {
      console.error('SSE message error:', err);
    }
  };

  source.onerror = () => {
    source?.close();
    source = null;
    setTimeout(() => connectSSE(url), 3000);
  };
}

export function disconnectSSE(): void {
  source?.close();
  source = null;
}
