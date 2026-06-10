import { sections, newSection, config, pool, drep, stake, address, blockCount } from './stores';
import type {
  AddressInfo,
  BlockEvent,
  BlockTx,
  Config,
  DRepInfo,
  Event,
  MempoolTxEvent,
  PoolInfo,
  Section,
  StakeInfo,
} from './types';

let source: EventSource | null = null;
let pendingPrune = new Set<string>();

// --- Infinite scroll (older history) pagination ---
type FeedCursor = { slot: number; epoch?: number; stake?: string };
let feedCursor: FeedCursor | null = null; // null once seeded-empty or first tx reached
let feedDone = false;
let loadingOlder = false;
let olderBase = ''; // origin + path before "/events/"
let feedId = '';
let feedDpr = '1';
let feedGen = 0; // bumped on (re)connect; guards against late responses after a feed switch

function setFeedContext(url: string): void {
  const i = url.indexOf('/events/');
  olderBase = i >= 0 ? url.slice(0, i) : '';
  const rest = i >= 0 ? url.slice(i + '/events/'.length) : '';
  feedId = rest.split('?')[0];
  feedDpr = url.match(/[?&]dpr=([^&]+)/)?.[1] ?? '1';
}

/// Insert a historical (older-than-newest) block as a section, ordered by slot
/// (newest→oldest after sections[0] = mempool). Deduplicates by slot. Shared by the
/// live Block handler's older branch and the `/older` pagination fetch.
function insertOlderBlock(s: Section[], event: BlockEvent, now: number): Section[] {
  if (s.some((sec, i) => i > 0 && sec.block?.slot === event.slot)) return s;
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
  let idx = 1;
  while (idx < s.length && (s[idx].block?.slot ?? 0) > event.slot) idx++;
  const result = [...s];
  result.splice(idx, 0, section);
  return result;
}

/// Fetch the next page of older blocks for the current feed and append them. Guarded
/// against concurrent calls and feed switches; stops once the cursor is null (the
/// address's first transaction). Called from the feed's near-oldest-edge scroll.
export async function loadOlder(): Promise<void> {
  if (loadingOlder || feedDone || !feedCursor) return;
  loadingOlder = true;
  const gen = feedGen;
  const cur = feedCursor;
  try {
    const params = new URLSearchParams({ before: String(cur.slot), dpr: feedDpr });
    if (cur.stake != null) params.set('stake', cur.stake);
    if (cur.epoch != null) params.set('epoch', String(cur.epoch));
    const res = await fetch(`${olderBase}/api/feed/${feedId}/older?${params}`);
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    const data = (await res.json()) as { blocks: BlockEvent[]; cursor?: FeedCursor };
    if (gen !== feedGen) return; // feed switched mid-fetch — drop this response
    const now = Date.now();
    sections.update((s) => {
      let next = s;
      for (const block of data.blocks) next = insertOlderBlock(next, block, now);
      return next;
    });
    feedCursor = data.cursor ?? null;
    feedDone = feedCursor === null;
  } catch (err) {
    console.error('loadOlder error:', err); // keep the cursor so the next scroll retries
  } finally {
    if (gen === feedGen) loadingOlder = false;
  }
}

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
          input.assets = output.assets;
        }
      }
    }
  }
}

function handleSnapshot(events: Event[]): void {
  const now = Date.now();
  const blockEvents: Event[] = [];
  const mempoolTxs: MempoolTxEvent[] = [];

  for (const event of events) {
    if (event.type === 'Block') blockEvents.push(event);
    else if (event.type === 'MempoolTx') mempoolTxs.push(event);
  }

  const blocks: Section[] = blockEvents
    .map((event) => {
      if (event.type !== 'Block') return null;
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
    .filter((s): s is Section => s !== null)
    .reverse();

  const mempool = newSection();
  mempool.txs = mempoolTxs.map((tx) => ({ ...tx, receivedAt: now }));
  resolveInputs(mempool.txs);

  sections.set([mempool, ...blocks]);
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
        if (s.some((sec, i) => i > 0 && sec.block?.slot === event.slot)) return s;

        const newestBlockSlot = s[1]?.block?.slot ?? 0;

        if (event.slot >= newestBlockSlot) {
          const mempool = s[0];
          const mempoolByHash = new Map(mempool.txs.map((tx) => [tx.hash, tx]));
          const blockByHash = new Map(event.txs.map((tx) => [tx.hash, tx]));

          const excluded = mempool.txs.filter((tx) => !blockByHash.has(tx.hash));
          const inBlock = [
            ...mempool.txs.filter((tx) => blockByHash.has(tx.hash)),
            ...event.txs.filter((tx) => !mempoolByHash.has(tx.hash)).map((tx) => ({ ...tx, receivedAt: now })),
          ];

          mempool.block = {
            slot: event.slot,
            hash: event.hash,
            number: event.number,
            timestamp: event.timestamp,
            pool_id: event.pool_id,
            pool_ticker: event.pool_ticker,
          };
          mempool.txs = inBlock;

          const next = newSection();
          next.txs = excluded.filter((tx) => !pendingPrune.has(tx.hash));
          pendingPrune.clear();

          return [next, ...s];
        } else {
          return insertOlderBlock(s, event, now);
        }
      });
      blockCount.update((n) => n + 1);
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

  sections.set([newSection()]);
  config.set(null);
  pool.set(null);
  drep.set(null);
  stake.set(null);
  address.set(null);
  pendingPrune.clear();

  // Reset pagination state for the new feed.
  setFeedContext(url);
  feedCursor = null;
  feedDone = false;
  loadingOlder = false;
  feedGen++;

  source = new EventSource(url);

  source.onmessage = (e: MessageEvent) => {
    try {
      const data = JSON.parse(e.data);

      if (data.type === 'Config') {
        config.set(data as Config);
      } else if (data.type === 'Pool') {
        pool.set(data as PoolInfo);
      } else if (data.type === 'DRep') {
        drep.set(data as DRepInfo);
      } else if (data.type === 'Stake') {
        stake.set(data as StakeInfo);
      } else if (data.type === 'Address') {
        address.set(data as AddressInfo);
      } else if (data.type === 'ReplayCursor') {
        feedCursor = { slot: data.slot, epoch: data.epoch, stake: data.stake };
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
