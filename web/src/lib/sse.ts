import { sections, newSection, config, pool, drep, stake, address, cardano, blockCount } from './stores';
import type {
  AddressInfo,
  BlockEvent,
  BlockTx,
  CardanoInfo,
  Config,
  DRepInfo,
  AssetDelta,
  Event,
  MempoolTxEvent,
  PoolInfo,
  RewardEvent,
  Section,
  StakeInfo,
} from './types';

/** A section's position on the slot-ordered timeline: a block's slot or a reward
 * capsule's epoch-change slot. */
function sectionSlot(sec: Section): number {
  return sec.block?.slot ?? sec.reward?.slot ?? 0;
}

let source: EventSource | null = null;
let pendingPrune = new Set<string>();

// The assets grid registers a single handler here to receive live asset deltas (it
// loads its initial page over HTTP and isn't a store consumer).
let assetLiveHandler: ((e: AssetDelta) => void) | null = null;
export function onAssetLive(fn: (e: AssetDelta) => void): () => void {
  assetLiveHandler = fn;
  return () => {
    if (assetLiveHandler === fn) assetLiveHandler = null;
  };
}

// --- Infinite scroll (older history) pagination ---
// Stake/address feeds page by `slot` (+ walk anchor); pool/DRep feeds page by the
// per-source keyset ids. A cursor with all fields undefined is the pool/DRep seed
// (paginate from the tip).
type FeedCursor = {
  slot?: number;
  epoch?: number;
  stake?: string;
  block_id?: number;
  vote_id?: number;
  deleg_id?: number;
};
let feedCursor: FeedCursor | null = null; // null once seeded-empty or first tx reached
let feedCursorSeed: FeedCursor | null = null; // the connect-time cursor, restored on return-to-top
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
  while (idx < s.length && sectionSlot(s[idx]) > event.slot) idx++;
  const result = [...s];
  result.splice(idx, 0, section);
  return result;
}

/// Insert a per-epoch REWARDS capsule as a section, ordered by its epoch-change slot
/// (same newest→oldest scheme as blocks). Deduplicates by epoch.
function insertReward(s: Section[], event: RewardEvent): Section[] {
  if (s.some((sec, i) => i > 0 && sec.reward?.epoch === event.epoch)) return s;
  const section = newSection();
  section.id = `r-${event.epoch}`;
  section.reward = {
    epoch: event.epoch,
    slot: event.slot,
    timestamp: event.timestamp,
    rows: event.rows,
  };
  let idx = 1;
  while (idx < s.length && sectionSlot(s[idx]) > event.slot) idx++;
  const result = [...s];
  result.splice(idx, 0, section);
  return result;
}

/// Re-seed pagination to the connect-time cursor. Called when the user scrolls back to
/// the newest edge (where the feed is trimmed to the live window): a later scroll into
/// history then refetches from the seed and refills contiguously — pool/DRep re-page from
/// the tip and dedup the overlap; stake/address resume from the connect anchor. No-op
/// before the seed has arrived.
export function resetOlder(): void {
  if (feedCursorSeed !== null) {
    feedCursor = { ...feedCursorSeed };
    feedDone = false;
  }
}

/// Fetch the next page of older blocks for the current feed and append them. Guarded
/// against concurrent calls and feed switches; stops once the cursor is null (the
/// address's first transaction). Called from the feed's near-oldest-edge scroll.
export async function loadOlder(): Promise<void> {
  if (loadingOlder || feedDone || !feedCursor) return;
  loadingOlder = true;
  const gen = feedGen;
  try {
    // Fetch pages until one appends something new or history is exhausted. A pool/DRep
    // feed's first page pages from the tip and overlaps the connect replay, so it can
    // fully dedup — advance past it. The bound guards against a pathological loop.
    for (let guard = 0; guard < 20; guard++) {
      const cur = feedCursor;
      if (!cur) break;
      const params = new URLSearchParams({ dpr: feedDpr });
      if (cur.slot != null) params.set('before', String(cur.slot));
      if (cur.stake != null) params.set('stake', cur.stake);
      if (cur.epoch != null) params.set('epoch', String(cur.epoch));
      if (cur.block_id != null) params.set('block_id', String(cur.block_id));
      if (cur.vote_id != null) params.set('vote_id', String(cur.vote_id));
      if (cur.deleg_id != null) params.set('deleg_id', String(cur.deleg_id));
      const res = await fetch(`${olderBase}/api/feed/${feedId}/older?${params}`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      // `blocks` may carry both Block and Reward items (rewards have no block/tx).
      const data = (await res.json()) as {
        blocks: (BlockEvent | RewardEvent)[];
        cursor?: FeedCursor;
      };
      if (gen !== feedGen) return; // feed switched mid-fetch — drop this response
      const now = Date.now();
      let added = 0;
      sections.update((s) => {
        let next = s;
        for (const item of data.blocks) {
          const before = next;
          next = item.type === 'Reward' ? insertReward(next, item) : insertOlderBlock(next, item, now);
          if (next !== before) added++;
        }
        return next;
      });
      feedCursor = data.cursor ?? null;
      feedDone = feedCursor === null;
      if (added > 0 || feedDone) break;
    }
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

    case 'Reward':
      sections.update((s) => insertReward(s, event));
      break;

    case 'Rollback':
      // Keep the mempool (i === 0) and any block/reward section at/under the rollback slot.
      sections.update((s) => s.filter((section, i) => i === 0 || sectionSlot(section) <= event.slot));
      // The assets grid needs no special rollback handling: the server emits a corrective
      // AssetDelta (diffed against the reverted snapshot), delivered like any other delta.
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
  cardano.set(null);
  pendingPrune.clear();

  // Reset pagination state for the new feed.
  setFeedContext(url);
  feedCursor = null;
  feedCursorSeed = null;
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
      } else if (data.type === 'Cardano') {
        cardano.set(data as CardanoInfo);
      } else if (data.type === 'ReplayCursor') {
        // Seed pagination and remember the seed so return-to-top can restore it.
        feedCursorSeed = { slot: data.slot, epoch: data.epoch, stake: data.stake };
        feedCursor = { ...feedCursorSeed };
      } else if (data.type === 'AssetDelta') {
        assetLiveHandler?.({
          slot: data.slot,
          added: data.added ?? [],
          removed: data.removed ?? [],
        });
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
