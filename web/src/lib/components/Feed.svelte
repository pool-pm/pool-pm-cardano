<script lang="ts">
  import { onMount, tick, untrack } from 'svelte';
  import { slide } from 'svelte/transition';
  import { flip } from 'svelte/animate';
  import { sections, config, pool } from '../stores';
  import type { GenesisConfig, Section } from '../types';
  import { TX_WIDTH, TX_GAP, FLIP_DURATION, poolColor, formatTicker } from '../layout';
  import BinPackGrid from './BinPackGrid.svelte';
  import Transaction from './Transaction.svelte';

  const MAX_BLOCKS = 30;
  const MAX_MEMPOOL_AGE_MS = 600_000;
  const DEFAULT_PX_PER_SECOND = 2;
  const MAX_TOTAL_GAP_PX = 400;
  const BLOCK_PADDING = 10;
  const BLOCK_BORDER = 2;
  const BLOCK_INSET = (BLOCK_PADDING + BLOCK_BORDER) * 2;

  let feedEl: HTMLDivElement;
  let poolHeaderEl: HTMLDivElement | undefined;
  let feedWidth = $state(0);
  let feedHeight = $state(0);
  let poolHeaderHeight = $state(0);
  let landscape = $state(false);
  let actualGridWidths = $state<Record<string, number>>({});

  // Section positioning: absolute layout with smooth CSS transitions
  let sectionRefs = new Map<string, HTMLElement>();
  let sectionPositions = $state<Map<string, { pos: number; spacing: number }>>(new Map());
  let canvasSize = $state(0);
  let animated = $state(false);
  let sectionObserver: ResizeObserver | undefined;
  let measurePending = false;

  // Dynamic spacing: shrink PX_PER_SECOND so total gaps fit on screen
  let pxPerSecond = $derived.by(() => {
    const sects = $sections;
    if (sects.length <= 2) return DEFAULT_PX_PER_SECOND;
    const oldest = sects[sects.length - 1].block?.timestamp;
    if (!oldest) return DEFAULT_PX_PER_SECOND;
    const totalTime = Date.now() / 1000 - oldest;
    if (totalTime <= 0) return DEFAULT_PX_PER_SECOND;
    return Math.min(DEFAULT_PX_PER_SECOND, MAX_TOTAL_GAP_PX / totalTime);
  });

  // Available height for tx columns in landscape mode
  let txAreaHeight = $derived(feedHeight - BLOCK_INSET - 40 - poolHeaderHeight);

  function colsNeeded(heights: number[], gap: number, maxH: number): number {
    let cols = 1, h = 0;
    for (const itemH of heights) {
      if (h > 0 && h + gap + itemH > maxH) {
        cols++;
        h = itemH;
      } else {
        h += (h > 0 ? gap : 0) + itemH;
      }
    }
    return cols;
  }

  function balanceColumns(node: HTMLElement, availableHeight: number) {
    let lastMaxH = '';
    const gap = TX_GAP;

    function rebalance() {
      const items = Array.from(node.children) as HTMLElement[];
      if (items.length === 0) {
        node.style.maxHeight = '';
        node.style.width = '';
        return;
      }

      const heights = items.map((el) => el.offsetHeight);
      const total = heights.reduce((s, h) => s + h, 0) + Math.max(0, items.length - 1) * gap;

      let maxH: number;
      if (total <= availableHeight) {
        maxH = availableHeight;
      } else {
        const targetCols = Math.ceil(total / availableHeight);
        let lo = Math.max(...heights), hi = availableHeight;
        while (hi - lo > 1) {
          const mid = Math.floor((lo + hi) / 2);
          if (colsNeeded(heights, gap, mid) <= targetCols) hi = mid;
          else lo = mid;
        }
        maxH = hi;
      }

      const cols = colsNeeded(heights, gap, maxH);

      const maxHStr = `${maxH}px`;
      if (lastMaxH !== maxHStr) {
        lastMaxH = maxHStr;
        node.style.maxHeight = maxHStr;
      }
      // Browsers don't auto-size the cross axis of a column-wrap flex container,
      // so set width explicitly to contain all wrapped columns.
      node.style.width = `${cols * TX_WIDTH + Math.max(0, cols - 1) * gap}px`;
    }

    const mutObs = new MutationObserver(() => requestAnimationFrame(rebalance));
    mutObs.observe(node, { childList: true });

    requestAnimationFrame(rebalance);

    return {
      update(newAvailableHeight: number) {
        availableHeight = newAvailableHeight;
        rebalance();
      },
      destroy() {
        mutObs.disconnect();
      },
    };
  }

  function trackSection(node: HTMLElement, id: string) {
    sectionRefs.set(id, node);
    sectionObserver?.observe(node);
    scheduleMeasure();
    return {
      destroy() {
        sectionObserver?.unobserve(node);
        sectionRefs.delete(id);
      },
    };
  }

  function scheduleMeasure() {
    if (!measurePending) {
      measurePending = true;
      tick().then(() => {
        measurePending = false;
        measureSections();
      });
    }
  }

  function measureSections() {
    const sects = $sections;
    const positions = new Map<string, { pos: number; spacing: number }>();
    let pos = 0;
    for (let i = 0; i < sects.length; i++) {
      const section = sects[i];
      let spacing = 0;
      if (i > 0) {
        const prev = sects[i - 1].block?.timestamp ?? now / 1000;
        const delta = section.block ? Math.max(0, prev - section.block.timestamp) : 0;
        spacing = Math.max(2, Math.round(pxPerSecond * delta));
        pos += spacing;
      }
      positions.set(section.id, { pos, spacing });
      const el = sectionRefs.get(section.id);
      pos += landscape ? (el?.offsetWidth ?? 0) : (el?.offsetHeight ?? 0);
    }
    sectionPositions = positions;
    canvasSize = pos;
    if (!animated)
      tick().then(() => {
        animated = true;
      });
  }

  function updateLandscape() {
    const was = landscape;
    landscape = window.innerWidth > window.innerHeight;
    if (was !== landscape) {
      animated = false;
    }
  }

  onMount(() => {
    updateLandscape();
    window.addEventListener('resize', updateLandscape);

    feedWidth = feedEl.offsetWidth;
    feedHeight = feedEl.offsetHeight;
    const feedObserver = new ResizeObserver((entries) => {
      feedWidth = entries[0]?.contentRect.width ?? 0;
      feedHeight = entries[0]?.contentRect.height ?? 0;
    });
    feedObserver.observe(feedEl);

    if (poolHeaderEl) {
      const style = getComputedStyle(poolHeaderEl);
      poolHeaderHeight = poolHeaderEl.offsetHeight + parseFloat(style.marginBottom);
    }

    sectionObserver = new ResizeObserver(scheduleMeasure);
    for (const el of sectionRefs.values()) sectionObserver.observe(el);

    return () => {
      window.removeEventListener('resize', updateLandscape);
      feedObserver.disconnect();
      sectionObserver?.disconnect();
    };
  });

  function sectionMaxWidth(section: Section): string {
    if (landscape) return 'none';
    const gw = actualGridWidths[section.id];
    if (gw) return `${gw + BLOCK_INSET}px`;
    if (section.txs.length === 0) return `${TX_WIDTH + BLOCK_INSET}px`;
    return 'none';
  }

  let now = $state(Date.now());

  $effect(() => {
    const interval = setInterval(() => {
      now = Date.now();
    }, 1000);
    return () => clearInterval(interval);
  });

  $effect(() => {
    const interval = setInterval(() => {
      const nowSec = Math.floor(Date.now() / 1000);
      const cutoff = Date.now() - MAX_MEMPOOL_AGE_MS;
      sections.update((s) => {
        s[0].txs = s[0].txs.filter((tx) => (tx.expiry ? tx.expiry > nowSec : tx.receivedAt >= cutoff));
        return s.slice(0, MAX_BLOCKS + 1);
      });
    }, 10_000);
    return () => clearInterval(interval);
  });

  // Re-measure positions when sections change, time advances, or orientation changes
  $effect(() => {
    $sections;
    now;
    landscape;
    pxPerSecond;
    untrack(scheduleMeasure);
  });

  // After feed resizes (e.g. orientation change), balanceColumns rewraps columns via its
  // update() method, which changes section widths. Schedule a deferred re-measure so
  // positions reflect the new widths after the browser has laid out the rewrapped columns.
  $effect(() => {
    feedHeight;
    feedWidth;
    if (!landscape) return;
    untrack(() => {
      requestAnimationFrame(() => scheduleMeasure());
    });
  });

  function timeAgo(timestamp: number): string {
    const sec = Math.floor((now - timestamp * 1000) / 1000);
    if (sec < 60) return `${sec}s ago`;
    if (sec < 3600) return `${Math.floor(sec / 60)}m ago`;
    return `${Math.floor(sec / 3600)}h ago`;
  }


  function formatAda(lovelace: string): string {
    const padded = lovelace.padStart(7, '0');
    const whole = padded.slice(0, -6) || '0';
    const frac = padded.slice(-6);
    const wholeNum = Number(whole);
    if (wholeNum >= 1000) return wholeNum.toLocaleString() + ' ADA';
    if (wholeNum >= 1) {
      const trimmed = frac.slice(0, 2).replace(/0+$/, '');
      return trimmed ? whole + '.' + trimmed + ' ADA' : whole + ' ADA';
    }
    const trimmed = frac.replace(/0+$/, '');
    return trimmed ? '0.' + trimmed + ' ADA' : '0 ADA';
  }


  function formatMargin(m: number): string {
    return (m * 100).toFixed(2).replace(/\.?0+$/, '') + '%';
  }

  function formatTime(timestamp: number): string {
    return new Date(timestamp * 1000).toLocaleTimeString();
  }

  function epochInfo(genesis: GenesisConfig): { epoch: number; epochEnd: number } {
    const nowSec = Math.floor(now / 1000);
    const slot =
      genesis.shelley_known_slot + Math.floor((nowSec - genesis.shelley_known_time) / genesis.shelley_slot_length);
    const shelleyStartEpoch = Math.floor(genesis.shelley_known_slot * genesis.byron_slot_length / genesis.byron_epoch_length);
    const epochsSince = Math.floor((slot - genesis.shelley_known_slot) / genesis.shelley_epoch_length);
    const epoch = shelleyStartEpoch + epochsSince;
    const epochEndSlot = genesis.shelley_known_slot + (epochsSince + 1) * genesis.shelley_epoch_length;
    const epochEnd =
      genesis.shelley_known_time + (epochEndSlot - genesis.shelley_known_slot) * genesis.shelley_slot_length;
    return { epoch, epochEnd };
  }

  function formatTimeLeft(epochEnd: number): string {
    const sec = Math.max(0, Math.floor(epochEnd - now / 1000));
    if (sec >= 172800) return `${Math.floor(sec / 86400)} days left`;
    if (sec >= 7200) return `${Math.floor(sec / 3600)} hours left`;
    if (sec >= 120) return `${Math.floor(sec / 60)} mins left`;
    return `${sec} secs left`;
  }
</script>

<div
  class="feed"
  class:landscape
  bind:this={feedEl}
  style:--block-padding="{BLOCK_PADDING}px"
  style:--block-border="{BLOCK_BORDER}px"
  style:--flip-duration="{FLIP_DURATION}ms"
>
  {#if $pool}
    <div class="pool-header" bind:this={poolHeaderEl} style:border-color={poolColor($pool.pool_id)}>
      <span class="pool-ticker">{formatTicker($pool.ticker ?? $pool.pool_id.slice(5, 10))}</span>
      {#if $pool.live_stake}
        <span class="pool-stat">{formatAda($pool.live_stake)} stake</span>
      {/if}
      {#if $pool.delegators != null}
        <span class="pool-stat">{$pool.delegators.toLocaleString()} delegators</span>
      {/if}
      <span class="pool-stat">{formatAda($pool.pledge)} pledge</span>
      <span class="pool-stat">{formatMargin($pool.margin)} margin</span>
      <span class="pool-stat">{formatAda($pool.fixed_cost)} fixed cost</span>
    </div>
  {/if}
  <div
    class="canvas"
    style={landscape ? `width: ${canvasSize}px` : `height: ${canvasSize}px`}
  >
    {#each $sections as section, i (section.id)}
      {@const isMempool = !section.block}
      {@const color = section.block ? poolColor(section.block.pool_id) : '#444'}
      {@const layout = sectionPositions.get(section.id)}
      <!-- svelte-ignore a11y_no_static_element_interactions -->
      <div
        class="section"
        class:mempool={isMempool}
        class:animated
        class:has-line={i > 0 && (layout?.spacing ?? 0) > 0}
        style:border-color={color}
        style:background-color={color}
        style:--section-color={color}
        style:--section-width={sectionMaxWidth(section)}
        style:--spacing="{layout?.spacing ?? 0}px"
        style:transform={landscape
          ? `translate(${-(layout?.pos ?? 0)}px, ${Math.round((feedHeight - (sectionRefs.get(section.id)?.offsetHeight ?? 0)) / 2)}px)`
          : `translateY(${layout?.pos ?? 0}px)`}
        ongridwidth={(e: CustomEvent<number>) => {
          actualGridWidths[section.id] = e.detail;
        }}
        use:trackSection={section.id}
        out:slide={{ duration: FLIP_DURATION, axis: landscape ? 'x' : 'y' }}
      >
        <div class="block-header">
          {#if section.block}
            <a class="block-ticker" href="/{section.block.pool_id ?? ''}"
              >{formatTicker(section.block.pool_ticker ?? section.block.pool_id?.slice(5, 10) ?? '')}</a
            >
            <span class="block-meta">#{section.block.number}</span>
          {:else}
            <span class="block-ticker">MEMPOOL</span>
          {/if}
        </div>

        {#if section.txs.length > 0}
          {#if landscape}
            <div class="column-grid" use:balanceColumns={txAreaHeight}>
              {#each section.txs as tx (tx.hash)}
                <div class="column-grid-item" animate:flip={{ duration: FLIP_DURATION }}>
                  <Transaction {tx} />
                </div>
              {/each}
            </div>
          {:else}
            <BinPackGrid
              items={section.txs}
              key={(tx) => tx.hash}
              itemWidth={TX_WIDTH}
              gap={TX_GAP}
              availableWidth={feedWidth - BLOCK_INSET}
            >
              {#snippet children(tx)}
                <Transaction {tx} />
              {/snippet}
            </BinPackGrid>
          {/if}
        {/if}

        {#if isMempool && $config?.genesis}
          {@const ei = epochInfo($config.genesis)}
          <div class="block-footer">
            <span class="block-meta">Epoch {ei.epoch}</span>
            <span class="block-meta">{formatTimeLeft(ei.epochEnd)}</span>
          </div>
        {:else if section.block}
          <div class="block-footer">
            <span class="block-meta">{section.block.hash.slice(0, 4)}…{section.block.hash.slice(-4)}</span>
            <span class="block-meta">
              {#if i === 1}{timeAgo(section.block.timestamp)}{:else}{formatTime(section.block.timestamp)}{/if}
            </span>
          </div>
        {/if}
      </div>
    {/each}
  </div>
</div>

<style>
  .feed {
    flex: 1;
    overflow-y: auto;
    scrollbar-gutter: stable;
    padding: 16px 20px;
  }

  .feed.landscape {
    overflow-y: hidden;
    overflow-x: auto;
    direction: rtl;
  }

  .pool-header {
    display: flex;
    align-items: baseline;
    gap: 12px;
    padding: 8px 12px;
    margin-bottom: 16px;
    border-left: 3px solid;
    white-space: nowrap;
  }

  .landscape .pool-header {
    direction: ltr;
    margin-bottom: 16px;
    position: sticky;
    right: 0;
  }

  .pool-ticker {
    font-weight: 700;
    font-size: 14px;
    color: var(--text);
  }

  .pool-stat {
    font-size: 11px;
    color: var(--text-muted);
  }

  .canvas {
    position: relative;
  }

  .landscape .canvas {
    height: 100%;
    direction: ltr;
  }

  .section {
    position: absolute;
    left: 0;
    right: 0;
    margin: 0 auto;
    max-width: var(--section-width);
    min-width: 132px;
    border: var(--block-border) solid;
    border-radius: 8px;
    padding: var(--block-padding);
    display: flex;
    flex-direction: column;
  }

  .landscape .section {
    left: auto;
    right: 0;
    top: 0;
    margin: 0;
  }

  .section.animated {
    transition: transform var(--flip-duration) ease;
    will-change: transform;
  }

  /* Portrait: vertical connecting line above the block */
  .section.has-line::before {
    content: '';
    position: absolute;
    bottom: calc(100% + var(--block-border));
    left: 50%;
    width: 1px;
    height: var(--spacing);
    background: #444;
  }

  /* Landscape: horizontal connecting line to the right of the block */
  .landscape .section.has-line::before {
    bottom: auto;
    left: calc(100% + var(--block-border));
    top: 50%;
    width: var(--spacing);
    height: 1px;
  }

  .landscape .section.mempool {
    max-height: calc(100vh - 72px);
    overflow: hidden;
  }

  .section.mempool {
    filter: grayscale(1);
  }

  .section.mempool .block-header {
    justify-content: center;
  }

  .block-header {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    margin-bottom: calc(var(--block-padding) + var(--block-border));
    line-height: 1;
    white-space: nowrap;
    gap: 8px;
  }

  .block-footer {
    display: flex;
    justify-content: space-between;
    margin-top: 8px;
    white-space: nowrap;
    gap: 8px;
  }

  .block-meta {
    color: rgb(0 0 0 / 0.5);
    font-size: 10px;
  }

  .block-ticker {
    color: white;
    font-size: 13px;
    font-weight: 700;
    line-height: 1;
    text-decoration: none;
  }

  .column-grid {
    display: flex;
    flex-direction: column;
    flex-wrap: wrap;
    gap: 6px;
    justify-content: flex-end;
    align-content: flex-start;
  }

  .column-grid-item {
    width: 108px;
    will-change: transform;
  }
</style>
