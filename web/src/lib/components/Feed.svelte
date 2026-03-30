<script lang="ts">
  import { onMount, tick, untrack } from 'svelte';
  import { slide } from 'svelte/transition';
  import { sections, config, pool } from '../stores';
  import type { GenesisConfig, Section } from '../types';
  import { TX_WIDTH, TX_GAP, FLIP_DURATION, poolColor, formatTicker, layoutGrid } from '../layout';
  import Transaction from './Transaction.svelte';

  const MAX_BLOCKS = 30;
  const MAX_MEMPOOL_AGE_MS = 600_000;
  const DEFAULT_PX_PER_SECOND = 2;
  const MAX_TOTAL_GAP_PX = 400;
  const BLOCK_PADDING = 10;
  const BLOCK_BORDER = 2;
  const BLOCK_INSET = (BLOCK_PADDING + BLOCK_BORDER) * 2;

  let feedEl: HTMLDivElement;
  let poolHeaderEl = $state<HTMLDivElement | undefined>();
  let feedWidth = $state(0);
  let feedHeight = $state(0);
  let poolHeaderHeight = $state(0);
  const LANDSCAPE_MARGIN = 16; // vertical breathing room in landscape
  let landscape = $state(false);
  let actualGridWidths = $state<Record<string, number>>({});

  // Section positioning: absolute layout with smooth CSS transitions
  let sectionRefs = new Map<string, HTMLElement>();
  let sectionPositions = $state<Map<string, { pos: number; spacing: number }>>(new Map());
  let canvasSize = $state(0);
  let animated = $state(false);
  let sectionObserver: ResizeObserver | undefined;
  let measurePending = false;

  // Dynamic spacing: shrink PX_PER_SECOND so total gaps fit on screen.
  // Use the block timestamp range (not Date.now()) so that pxPerSecond only
  // changes when blocks are added/removed, not on every mempool tx arrival.
  let pxPerSecond = $derived.by(() => {
    const sects = $sections;
    if (sects.length <= 2) return DEFAULT_PX_PER_SECOND;
    const newest = sects[1]?.block?.timestamp;
    const oldest = sects[sects.length - 1].block?.timestamp;
    if (!newest || !oldest) return DEFAULT_PX_PER_SECOND;
    const totalTime = newest - oldest;
    if (totalTime <= 0) return DEFAULT_PX_PER_SECOND;
    return Math.min(DEFAULT_PX_PER_SECOND, MAX_TOTAL_GAP_PX / totalTime);
  });

  // Available height for tx columns in landscape mode
  let txAreaHeight = $derived(feedHeight - BLOCK_INSET - 40 - poolHeaderHeight - LANDSCAPE_MARGIN);

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

  // Set page title from pool ticker
  $effect(() => {
    const p = $pool;
    document.title = p ? `${formatTicker(p.ticker ?? p.pool_id.slice(5, 10))} - pool.pm` : 'pool.pm';
  });

  // Measure pool header height reactively (it appears after SSE sends Pool event)
  $effect(() => {
    const el = poolHeaderEl;
    if (!el) {
      poolHeaderHeight = 0;
      return;
    }
    const style = getComputedStyle(el);
    poolHeaderHeight = el.offsetHeight + parseFloat(style.marginBottom);
    const obs = new ResizeObserver(() => {
      const s = getComputedStyle(el);
      poolHeaderHeight = el.offsetHeight + parseFloat(s.marginBottom);
    });
    obs.observe(el);
    return () => obs.disconnect();
  });

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

  // After feed resizes (e.g. orientation change), layoutGrid rewraps and changes section
  // widths. Schedule a deferred re-measure so positions reflect the new widths.
  $effect(() => {
    feedHeight;
    feedWidth;
    untrack(() => {
      requestAnimationFrame(() => scheduleMeasure());
    });
  });

  const STAKE_POSITIVE = 'oklch(0.55 0.13 155)';
  const STAKE_NEGATIVE = 'oklch(0.55 0.13 25)';

  function sectionColor(section: Section): string {
    if (!section.block) return '#444';
    if (!$pool) return poolColor(section.block.pool_id);
    // Pool's own block: use pool color
    if (section.block.pool_id === $pool.pool_id) return poolColor($pool.pool_id);
    // Compute net stake change from txs
    let net = 0n;
    for (const tx of section.txs) {
      if (tx.stake_change) net += BigInt(tx.stake_change);
    }
    if (net > 0n) return STAKE_POSITIVE;
    if (net < 0n) return STAKE_NEGATIVE;
    return '#555';
  }

  function introScale(node: HTMLElement) {
    node.style.animation = `section-intro ${FLIP_DURATION}ms ease`;
    node.style.transition = 'none';
    const timer = setTimeout(() => {
      node.style.animation = '';
      node.style.transition = '';
    }, FLIP_DURATION);
    return {
      destroy() {
        clearTimeout(timer);
      },
    };
  }

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
    const date = new Date(timestamp * 1000);
    const today = new Date(now);
    if (date.toDateString() === today.toDateString()) {
      return date.toLocaleTimeString();
    }
    return date.toLocaleDateString(undefined, { month: 'short', day: 'numeric' }) + ' ' + date.toLocaleTimeString();
  }

  function epochInfo(genesis: GenesisConfig): { epoch: number; epochEnd: number } {
    const nowSec = Math.floor(now / 1000);
    const slot =
      genesis.shelley_known_slot + Math.floor((nowSec - genesis.shelley_known_time) / genesis.shelley_slot_length);
    const shelleyStartEpoch = Math.floor(
      (genesis.shelley_known_slot * genesis.byron_slot_length) / genesis.byron_epoch_length,
    );
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
  <div class="canvas" style={landscape ? `width: ${canvasSize}px` : `height: ${canvasSize}px`}>
    {#each $sections as section, i (section.id)}
      {@const isMempool = !section.block}
      {@const color = sectionColor(section)}
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
        use:introScale
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
          <div
            class="tx-grid"
            use:layoutGrid={{ landscape, availableWidth: feedWidth - BLOCK_INSET, availableHeight: txAreaHeight }}
          >
            {#each section.txs as tx (tx.hash)}
              <div class="tx-grid-item">
                <Transaction {tx} />
              </div>
            {/each}
          </div>
        {/if}

        {#if isMempool && $config?.genesis}
          {@const ei = epochInfo($config.genesis)}
          <div class="block-footer">
            <span class="block-meta">Epoch {ei.epoch}</span>
            <span class="block-meta">{formatTimeLeft(ei.epochEnd)}</span>
          </div>
        {:else if section.block}
          <div class="block-footer">
            <span class="block-meta block-hash mono">{section.block.hash}</span>
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

  @keyframes section-intro {
    from {
      scale: 0;
    }
    to {
      scale: 1;
    }
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

  .block-hash {
    overflow: hidden;
    text-overflow: ellipsis;
    max-width: 8ch;
  }

  .block-ticker {
    color: white;
    font-size: 13px;
    font-weight: 700;
    line-height: 1;
    text-decoration: none;
  }

  .tx-grid {
    position: relative;
    overflow: hidden;
  }

  .feed:not(.landscape) .tx-grid {
    width: 100%;
  }

  .tx-grid-item {
    position: absolute;
    width: 108px;
    transition: transform var(--flip-duration) ease;
    will-change: transform;
  }
</style>
