<script lang="ts">
  import { onMount, tick, untrack } from 'svelte';
  import { slide } from 'svelte/transition';
  import { sections, config, pool } from '../stores';
  import type { GenesisConfig, Section } from '../types';
  import { TX_WIDTH, TX_GAP, FLIP_DURATION } from '../layout';
  import BinPackGrid from './BinPackGrid.svelte';
  import Transaction from './Transaction.svelte';

  const MAX_BLOCKS = 30;
  const MAX_MEMPOOL_AGE_MS = 600_000;
  const PX_PER_SECOND = 2;
  const BLOCK_PADDING = 10;
  const BLOCK_BORDER = 2;
  const BLOCK_INSET = (BLOCK_PADDING + BLOCK_BORDER) * 2;

  let feedEl: HTMLDivElement;
  let feedWidth = $state(0);
  let actualGridWidths = $state<Record<string, number>>({});

  // Section positioning: absolute layout with smooth CSS transitions
  let sectionRefs = new Map<string, HTMLElement>();
  let sectionPositions = $state<Map<string, { y: number; spacing: number }>>(new Map());
  let canvasHeight = $state(0);
  let animated = $state(false);
  let sectionObserver: ResizeObserver | undefined;
  let measurePending = false;

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
    const positions = new Map<string, { y: number; spacing: number }>();
    let y = 0;
    for (let i = 0; i < sects.length; i++) {
      const section = sects[i];
      let spacing = 0;
      if (i > 0) {
        const prev = sects[i - 1].block?.timestamp ?? now / 1000;
        const delta = section.block ? Math.max(0, prev - section.block.timestamp) : 0;
        spacing = PX_PER_SECOND * 120 * Math.log(1 + delta / 120);
        y += spacing;
      }
      positions.set(section.id, { y: Math.round(y), spacing: Math.round(spacing) });
      y += sectionRefs.get(section.id)?.offsetHeight ?? 0;
    }
    sectionPositions = positions;
    canvasHeight = y;
    if (!animated)
      tick().then(() => {
        animated = true;
      });
  }

  onMount(() => {
    feedWidth = feedEl.offsetWidth;
    const feedObserver = new ResizeObserver((entries) => {
      feedWidth = entries[0]?.contentRect.width ?? 0;
    });
    feedObserver.observe(feedEl);

    sectionObserver = new ResizeObserver(scheduleMeasure);
    for (const el of sectionRefs.values()) sectionObserver.observe(el);

    return () => {
      feedObserver.disconnect();
      sectionObserver?.disconnect();
    };
  });

  function sectionMaxWidth(section: Section): string {
    const gw = actualGridWidths[section.id];
    if (gw) return `${gw + BLOCK_INSET}px`;
    if (section.txs.length === 0) return 'min-content';
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

  // Re-measure positions when sections change or time advances (spacing depends on now)
  $effect(() => {
    $sections;
    now;
    untrack(scheduleMeasure);
  });

  function blockColor(poolId?: string): string {
    const key = poolId?.slice(5) ?? '';
    let h = 0;
    for (let i = 0; i < key.length; i++) {
      h = Math.imul(h ^ key.charCodeAt(i), 0x9e3779b9);
    }
    const hue = (h >>> 0) % 360;
    return `oklch(0.7 0.25 ${hue})`;
  }

  function timeAgo(timestamp: number): string {
    const sec = Math.floor((now - timestamp * 1000) / 1000);
    if (sec < 60) return `${sec}s ago`;
    if (sec < 3600) return `${Math.floor(sec / 60)}m ago`;
    return `${Math.floor(sec / 3600)}h ago`;
  }

  function formatTicker(ticker: string): string {
    return ticker
      .toUpperCase()
      .replace(/[^A-Z0-9]/g, '')
      .slice(0, 5);
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
    return '0.' + frac + ' ADA';
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
    const shelleyStartEpoch = Math.floor(genesis.shelley_known_slot / genesis.byron_epoch_length);
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
  bind:this={feedEl}
  style:--block-padding="{BLOCK_PADDING}px"
  style:--block-border="{BLOCK_BORDER}px"
  style:--flip-duration="{FLIP_DURATION}ms"
>
  {#if $pool}
    <div class="pool-header" style:border-color={blockColor($pool.pool_id)}>
      <span class="pool-ticker">{$pool.ticker ?? $pool.pool_id.slice(5, 10)}</span>
      <span class="pool-stat">{formatAda($pool.pledge)} pledge</span>
      <span class="pool-stat">{formatMargin($pool.margin)} margin</span>
      <span class="pool-stat">{formatAda($pool.fixed_cost)} cost</span>
    </div>
  {/if}
  <div class="canvas" style="height: {canvasHeight}px">
    {#each $sections as section, i (section.id)}
      {@const isMempool = !section.block}
      {@const color = section.block ? blockColor(section.block.pool_id) : '#444'}
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
        style:transform="translateY({layout?.y ?? 0}px)"
        ongridwidth={(e: CustomEvent<number>) => {
          actualGridWidths[section.id] = e.detail;
        }}
        use:trackSection={section.id}
        out:slide={{ duration: FLIP_DURATION, axis: 'y' }}
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

  .pool-header {
    display: flex;
    align-items: baseline;
    gap: 12px;
    padding: 8px 12px;
    margin-bottom: 16px;
    border-left: 3px solid;
    white-space: nowrap;
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

  .section {
    position: absolute;
    left: 0;
    right: 0;
    margin: 0 auto;
    max-width: var(--section-width);
    min-width: min-content;
    border: var(--block-border) solid;
    border-radius: 8px;
    padding: var(--block-padding);
    display: flex;
    flex-direction: column;
  }

  .section.animated {
    transition: transform var(--flip-duration) ease;
    will-change: transform;
  }

  .section.has-line::before {
    content: '';
    position: absolute;
    bottom: calc(100% + var(--block-border));
    left: 50%;
    width: 1px;
    height: var(--spacing);
    background: var(--border);
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
</style>
