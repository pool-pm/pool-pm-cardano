<script lang="ts">
  import { flip } from 'svelte/animate';
  import { slide } from 'svelte/transition';
  import { sections } from '../stores';
  import { TX_WIDTH, TX_GAP, FLIP_DURATION, squareWidth } from '../layout';
  import BinPackGrid from './BinPackGrid.svelte';
  import Transaction from './Transaction.svelte';

  const MAX_BLOCKS = 30;
  const MAX_MEMPOOL_AGE_MS = 600_000;
  const PX_PER_SECOND = 2;
  const BLOCK_PADDING = 10;
  const BLOCK_BORDER = 2;
  const BLOCK_INSET = (BLOCK_PADDING + BLOCK_BORDER) * 2;

  let now = $state(Date.now());

  // Update current time every second for timeAgo display
  $effect(() => {
    const interval = setInterval(() => {
      now = Date.now();
    }, 1000);
    return () => clearInterval(interval);
  });

  // Clean up old sections periodically
  $effect(() => {
    const interval = setInterval(() => {
      const cutoff = Date.now() - MAX_MEMPOOL_AGE_MS;
      sections.update((s) => {
        s[0].txs = s[0].txs.filter((tx) => tx.receivedAt >= cutoff);
        return s.slice(0, MAX_BLOCKS + 1);
      });
    }, 10_000);
    return () => clearInterval(interval);
  });

  // Hash pool_id (minus "pool1" prefix) to a hue using a Fibonacci hashing variant
  // (multiply by golden ratio constant 0x9e3779b9) for uniform distribution across 0-359°
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

  function formatTime(timestamp: number): string {
    return new Date(timestamp * 1000).toLocaleTimeString();
  }

  // Detect landscape orientation for horizontal layout
  let horizontal = $state(false);

  $effect(() => {
    const mql = window.matchMedia('(orientation: landscape)');
    horizontal = mql.matches;
    const handler = (e: MediaQueryListEvent) => {
      horizontal = e.matches;
    };
    mql.addEventListener('change', handler);
    return () => mql.removeEventListener('change', handler);
  });
</script>

<div
  class="feed"
  style:--block-padding="{BLOCK_PADDING}px"
  style:--block-border="{BLOCK_BORDER}px"
  style:--flip-duration="{FLIP_DURATION}ms"
>
  {#each $sections as section, i (section.id)}
    {@const isMempool = !section.block}
    {@const color = section.block ? blockColor(section.block.pool_id) : '#444'}
    {@const maxWidth = squareWidth(section.txs.length) + BLOCK_INSET}
    {@const prevTimestamp = i > 0 ? ($sections[i - 1].block?.timestamp ?? now / 1000) : undefined}
    {@const gap =
      prevTimestamp && section.block ? Math.max(0, (prevTimestamp - section.block.timestamp) * PX_PER_SECOND) : 0}
    {@const spacing = gap}
    <div
      class="section"
      class:mempool={isMempool}
      class:has-line={i > 0 && gap > 0}
      style:border-color={color}
      style:background-color={color}
      style:--section-color={color}
      style:--section-width="{maxWidth}px"
      style:--spacing="{spacing}px"
      animate:flip={{ duration: FLIP_DURATION }}
      out:slide={{ duration: FLIP_DURATION, axis: horizontal ? 'x' : 'y' }}
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
        <BinPackGrid items={section.txs} key={(tx) => tx.hash} itemWidth={TX_WIDTH} gap={TX_GAP}>
          {#snippet children(tx)}
            <Transaction {tx} />
          {/snippet}
        </BinPackGrid>
      {/if}

      {#if section.block}
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

<style>
  .feed {
    flex: 1;
    overflow-y: auto;
    scrollbar-gutter: stable;
    padding: 16px 20px;
    display: flex;
    flex-direction: column;
    align-items: center;
  }

  .section {
    width: 100%;
    min-width: min-content;
    max-width: var(--section-width);
    margin-top: var(--spacing);
    position: relative;
    border: var(--block-border) solid;
    border-radius: 8px;
    padding: var(--block-padding);
    display: flex;
    flex-direction: column;
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

  /* Landscape: horizontal right-to-left flow */
  @media (orientation: landscape) {
    .feed {
      direction: rtl;
      flex-direction: row;
      overflow-x: auto;
      overflow-y: hidden;
      align-items: center;
      scrollbar-gutter: auto;
    }

    .section {
      direction: ltr;
      width: var(--section-width);
      max-width: none;
      flex-shrink: 0;
      margin-top: 0;
      margin-right: var(--spacing);
    }

    .section.has-line::before {
      bottom: auto;
      left: calc(100% + var(--block-border));
      top: 50%;
      width: var(--spacing);
      height: 1px;
    }
  }
</style>
