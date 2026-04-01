<script lang="ts">
  import { onMount, tick, untrack } from 'svelte';
  import { slide } from 'svelte/transition';
  import { sections, config, pool, drep, blockCount } from '../stores';
  import type { GenesisConfig, Section } from '../types';
  import { TX_WIDTH, TX_GAP, FLIP_DURATION, poolColor, formatTicker, layoutGrid } from '../layout';
  import Transaction from './Transaction.svelte';

  const MAX_BLOCKS = 30;
  /** Prune blocks older than 1h whose net stake change is below this fraction of live stake. */
  const STAKE_CHANGE_PRUNE_DIVISOR = 1_000n; // 0.1%
  const PX_PER_SECOND = 2;
  const BLOCK_PADDING = 10;
  const BLOCK_BORDER = 2;
  const BLOCK_INSET = (BLOCK_PADDING + BLOCK_BORDER) * 2;

  let feedEl: HTMLDivElement;
  let feedWidth = $state(0);
  let feedHeight = $state(0);
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
  let scrolledAway = false;
  let ignoreScroll = false;

  // Pool feeds: logarithmic spacing — 2px/sec for small gaps, ~100px/day
  function logGap(seconds: number): number {
    return 10 * Math.log1p(seconds / 5);
  }

  // Available height for tx columns in landscape mode.
  // Overhead = section border (4) + padding (20) + 3 flex gaps (30) + header (10) + ticker (13) + footer (10)
  const SECTION_OVERHEAD = BLOCK_BORDER * 2 + BLOCK_PADDING * 2 + BLOCK_PADDING * 3 + 33; // = 87
  let txAreaHeight = $derived(feedHeight - SECTION_OVERHEAD - LANDSCAPE_MARGIN);

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

  function handleScroll() {
    if (ignoreScroll) return;
    if (!feedEl) return;
    // row-reverse: scrollLeft ≈ 0 at right edge (can be slightly negative
    // due to padding/scrollbar gutter), goes more negative when scrolled left
    scrolledAway = landscape ? feedEl.scrollLeft < -30 : feedEl.scrollTop > 10;
  }

  function measureSections() {
    const sects = $sections;

    // Before remeasuring, record an anchor section's viewport position.
    // After DOM update we'll measure the actual shift and compensate scroll.
    let anchorEl: HTMLElement | undefined;
    let anchorBefore: number | undefined;
    if (animated && scrolledAway && canvasSize > 0) {
      for (let i = 1; i < sects.length; i++) {
        const el = sectionRefs.get(sects[i].id);
        if (el) {
          anchorEl = el;
          const rect = el.getBoundingClientRect();
          anchorBefore = landscape ? rect.left : rect.top;
          break;
        }
      }
    }

    const positions = new Map<string, { pos: number; spacing: number }>();
    let pos = 0;
    for (let i = 0; i < sects.length; i++) {
      const section = sects[i];
      let spacing = 0;
      if (i > 0) {
        const prev = sects[i - 1].block?.timestamp ?? now / 1000;
        const timeDelta = section.block ? Math.max(0, prev - section.block.timestamp) : 0;
        const maxSpacing = Math.round((landscape ? feedWidth : feedHeight) / 2);
        spacing = Math.min(
          maxSpacing,
          Math.max(2, Math.round($pool || $drep ? logGap(timeDelta) : PX_PER_SECOND * timeDelta)),
        );
        pos += spacing;
      }
      positions.set(section.id, { pos, spacing });
      const el = sectionRefs.get(section.id);
      pos += landscape ? (el?.offsetWidth ?? 0) : (el?.offsetHeight ?? 0);
    }

    if (anchorEl) animated = false;

    sectionPositions = positions;
    canvasSize = pos;

    if (anchorEl && anchorBefore !== undefined) {
      const anchor = anchorEl;
      const before = anchorBefore;
      tick().then(() => {
        const rect = anchor.getBoundingClientRect();
        const shift = (landscape ? rect.left : rect.top) - before;
        if (shift !== 0) {
          ignoreScroll = true;
          if (landscape) feedEl.scrollLeft += shift;
          else feedEl.scrollTop += shift;
          requestAnimationFrame(() => {
            ignoreScroll = false;
          });
        }
        requestAnimationFrame(() => {
          animated = true;
        });
      });
    } else if (!animated) {
      tick().then(() => {
        animated = true;
      });
    }
  }

  function updateLandscape() {
    const was = landscape;
    landscape = window.innerWidth > window.innerHeight;
    if (was !== landscape) {
      animated = false;
      scrolledAway = false;
    }
  }

  onMount(() => {
    updateLandscape();
    window.addEventListener('resize', updateLandscape);

    // Use content box (matching ResizeObserver's contentRect) — exclude padding
    const cs = getComputedStyle(feedEl);
    feedWidth = feedEl.clientWidth - parseFloat(cs.paddingLeft) - parseFloat(cs.paddingRight);
    feedHeight = feedEl.clientHeight - parseFloat(cs.paddingTop) - parseFloat(cs.paddingBottom);
    const feedObserver = new ResizeObserver((entries) => {
      feedWidth = entries[0]?.contentRect.width ?? 0;
      feedHeight = entries[0]?.contentRect.height ?? 0;
    });
    feedObserver.observe(feedEl);

    feedEl.addEventListener('scroll', handleScroll, { passive: true });

    function handleWheel(e: WheelEvent) {
      if (!landscape) return;
      if (e.deltaX !== 0) return; // native horizontal scroll, don't remap
      e.preventDefault();
      feedEl.scrollLeft -= e.deltaY;
    }

    function handleKeydown(e: KeyboardEvent) {
      if (!landscape) return;
      if (e.key === 'Home') {
        e.preventDefault();
        feedEl.scrollLeft = 0;
      }
    }

    feedEl.addEventListener('wheel', handleWheel, { passive: false });
    window.addEventListener('keydown', handleKeydown);

    sectionObserver = new ResizeObserver(scheduleMeasure);
    for (const el of sectionRefs.values()) sectionObserver.observe(el);

    return () => {
      feedEl.removeEventListener('scroll', handleScroll);
      feedEl.removeEventListener('wheel', handleWheel);
      window.removeEventListener('keydown', handleKeydown);
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

  // Set page title from pool ticker or DRep name
  $effect(() => {
    const p = $pool;
    const d = $drep;
    if (d) {
      document.title = `${d.given_name ?? d.drep_id.slice(5, 13)} - pool.pm`;
    } else if (p) {
      document.title = `${formatTicker(p.ticker ?? p.pool_id.slice(5, 10))} - pool.pm`;
    } else {
      document.title = 'pool.pm';
    }
  });

  $effect(() => {
    const interval = setInterval(() => {
      now = Date.now();
    }, 1000);
    return () => clearInterval(interval);
  });

  $effect(() => {
    $blockCount; // trigger on each new block
    const p = $pool;
    const d = $drep;
    const nowSec = Math.floor(Date.now() / 1000);
    sections.update((s) => {
      s[0].txs = s[0].txs.filter((tx) => !tx.expiry || tx.expiry > nowSec);
      let result = s.slice(0, MAX_BLOCKS + 1);
      // In pool/drep feeds, prune old small stake/delegation changes
      const liveStake = p?.live_stake ?? d?.live_stake;
      if (liveStake) {
        const threshold = BigInt(liveStake) / STAKE_CHANGE_PRUNE_DIVISOR;
        const oneHourAgo = nowSec - 3600;
        const feedPoolId = p?.pool_id;
        const feedDrepId = d?.drep_id;
        result = result.filter((section, i) => {
          if (i === 0 || !section.block) return true;
          if (feedPoolId && section.block.pool_id === feedPoolId) return true;
          if (section.block.timestamp > oneHourAgo) return true;
          if (
            section.txs.some((tx) =>
              tx.delegations?.some(
                (dl) =>
                  (feedPoolId && (dl.from_pool_id === feedPoolId || dl.to_pool_id === feedPoolId)) ||
                  (feedDrepId && (dl.from_drep_id === feedDrepId || dl.to_drep_id === feedDrepId)),
              ),
            )
          )
            return true;
          let net = 0n;
          for (const tx of section.txs) {
            if (tx.stake_change) net += BigInt(tx.stake_change);
          }
          if (net < 0n) net = -net;
          return net >= threshold;
        });
      }
      return result;
    });
  });

  // Re-measure positions when sections change, time advances, or orientation changes
  $effect(() => {
    $sections;
    landscape;
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

  const STAKE_POSITIVE = 'oklch(0.7 0.25 145)';
  const STAKE_NEGATIVE = 'oklch(0.7 0.25 25)';

  function sectionColors(section: Section): { bg: string; border: string; accent: string } {
    if (!section.block) return { bg: '#222', border: '#222', accent: 'rgb(255 255 255 / 0.4)' };
    if (!$pool && !$drep) {
      const c = poolColor(section.block.pool_id);
      return { bg: c, border: c, accent: c };
    }
    if ($pool && section.block.pool_id === $pool.pool_id) {
      const c = poolColor($pool.pool_id);
      return { bg: c, border: c, accent: c };
    }
    let net = 0n;
    for (const tx of section.txs) {
      if (tx.stake_change) net += BigInt(tx.stake_change);
    }
    if (net > 0n) return { bg: '#222', border: '#222', accent: STAKE_POSITIVE };
    if (net < 0n) return { bg: '#222', border: '#222', accent: STAKE_NEGATIVE };
    return { bg: '#555', border: '#555', accent: '#555' };
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
    if (wholeNum >= 1000) return '₳\u2009' + wholeNum.toLocaleString();
    if (wholeNum >= 1) {
      const trimmed = frac.slice(0, 2).replace(/0+$/, '');
      return trimmed ? '₳\u2009' + whole + '.' + trimmed : '₳\u2009' + whole;
    }
    const trimmed = frac.replace(/0+$/, '');
    return trimmed ? '₳\u20090.' + trimmed : '₳\u20090';
  }

  function formatMargin(m: number): string {
    return (m * 100).toFixed(2).replace(/\.?0+$/, '') + '%';
  }

  function formatDate(timestamp: number): string {
    const date = new Date(timestamp * 1000);
    const today = new Date(now);
    if (date.toDateString() === today.toDateString()) return 'Today';
    return date.toLocaleDateString(undefined, { month: 'short', day: 'numeric' });
  }

  function formatTime(timestamp: number): string {
    return new Date(timestamp * 1000).toLocaleTimeString();
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
    {@const color = poolColor($pool.pool_id)}
    <div class="pool-circle" style:border-color={color}>
      <span class="pool-name" style:color>{formatTicker($pool.ticker ?? $pool.pool_id.slice(5, 10))}</span>
      {#if $pool.delegators != null}
        <span class="pool-delegators">{$pool.delegators.toLocaleString()} delegators</span>
      {/if}
      {#if $pool.live_stake}
        <span class="pool-stake">{formatAda($pool.live_stake)}</span>
      {/if}
      <div class="pool-params">
        <div class="pool-param">
          <span class="pool-param-label">margin</span>
          <span class="pool-param-value">{formatMargin($pool.margin)}</span>
        </div>
        <div class="pool-param">
          <span class="pool-param-label">cost</span>
          <span class="pool-param-value">{formatAda($pool.fixed_cost)}</span>
        </div>
      </div>
      <div class="pool-param">
        <span class="pool-param-label">pledge</span>
        <span class="pool-param-value">{formatAda($pool.pledge)}</span>
      </div>
    </div>
  {:else if $drep}
    {@const color = poolColor($drep.drep_id)}
    <div class="pool-circle" style:border-color={color}>
      <span class="drep-name" style:color>{$drep.given_name ?? $drep.drep_id.slice(5, 13)}</span>
      {#if $drep.delegators != null}
        <span class="pool-delegators">{$drep.delegators.toLocaleString()} delegators</span>
      {/if}
      {#if $drep.live_stake}
        <span class="pool-stake">{formatAda($drep.live_stake)}</span>
      {/if}
    </div>
  {/if}
  <div class="canvas" style={landscape ? `width: ${canvasSize}px` : `height: ${canvasSize}px`}>
    {#each $sections as section, i (section.id)}
      {@const isMempool = !section.block}
      {@const colors = sectionColors(section)}
      {@const layout = sectionPositions.get(section.id)}
      <!-- svelte-ignore a11y_no_static_element_interactions -->
      <div
        class="section"
        class:mempool={isMempool}
        class:animated
        class:has-line={i > 0 && (layout?.spacing ?? 0) > 0}
        style:border-color={colors.border}
        style:background-color={colors.bg}
        style:--section-color={colors.accent}
        style:--meta-color={colors.bg.startsWith('#') ? 'rgb(255 255 255 / 0.4)' : ''}
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
          {#if isMempool && $config?.genesis}
            {@const ei = epochInfo($config.genesis)}
            <span class="block-meta">Epoch {ei.epoch}</span>
            <span class="block-meta">{formatTimeLeft(ei.epochEnd)}</span>
          {:else if section.block}
            <span class="block-meta">{formatDate(section.block.timestamp)}</span>
            <span class="block-meta">
              {#if i === 1}{timeAgo(section.block.timestamp)}{:else}{formatTime(section.block.timestamp)}{/if}
            </span>
          {/if}
        </div>
        {#if section.block && !$drep && (!$pool || section.block.pool_id === $pool.pool_id)}
          <a class="block-ticker" href="/{section.block.pool_id ?? ''}"
            >{formatTicker(section.block.pool_ticker ?? section.block.pool_id?.slice(5, 10) ?? '')}</a
          >
        {:else if !section.block}
          <span class="block-ticker">MEMPOOL</span>
        {/if}

        {#if section.txs.length > 0}
          <div
            class="tx-grid"
            use:layoutGrid={{ landscape, availableWidth: feedWidth - BLOCK_INSET, availableHeight: txAreaHeight }}
          >
            {#each section.txs as tx (tx.hash)}
              <div class="tx-grid-item">
                <Transaction {tx} compact={landscape && feedHeight < 500} />
              </div>
            {/each}
          </div>
        {/if}

        {#if section.block}
          <div class="block-footer">
            <span class="block-meta block-hash mono">{section.block.hash}</span>
            <span class="block-meta mono">#{section.block.number}</span>
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
    display: flex;
    flex-direction: row-reverse;
    align-items: center;
    overflow-y: hidden;
    overflow-x: auto;
  }

  .pool-circle {
    width: 220px;
    height: 220px;
    border-radius: 50%;
    border: 2px solid;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 6px;
    text-align: center;
    padding: 20px;
    box-sizing: border-box;
    margin: 0 auto 16px;
    flex-shrink: 0;
  }

  .landscape .pool-circle {
    margin: 0 16px;
    direction: ltr;
  }

  .pool-name {
    font-weight: 700;
    font-size: 24px;
    line-height: 1;
  }

  .drep-name {
    font-weight: 600;
    font-size: 14px;
    line-height: 1.2;
    text-align: center;
    max-width: 120px;
    overflow: hidden;
    text-overflow: ellipsis;
    display: -webkit-box;
    -webkit-line-clamp: 2;
    -webkit-box-orient: vertical;
  }

  .pool-stake {
    font-weight: 600;
    font-size: 24px;
    color: var(--text);
    line-height: 1;
  }

  .pool-delegators {
    font-size: 13px;
    color: var(--text-muted);
  }

  .pool-info {
    font-size: 10px;
    color: var(--text-muted);
    white-space: nowrap;
  }

  .pool-params {
    display: flex;
    gap: 0;
    width: 100%;
  }

  .pool-params .pool-param {
    flex: 1;
  }

  .pool-param {
    display: flex;
    flex-direction: column;
    align-items: center;
    padding: 0 6px;
  }

  .pool-params .pool-param + .pool-param {
    border-left: 1px solid rgb(255 255 255 / 0.15);
  }

  .pool-param-label {
    font-size: 8px;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    color: var(--text-muted);
  }

  .pool-param-value {
    font-size: 12px;
    font-weight: 600;
    color: var(--text);
    white-space: nowrap;
  }

  .canvas {
    position: relative;
    flex-shrink: 0;
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
    gap: var(--block-padding);
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

  .block-header {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    line-height: 1;
    white-space: nowrap;
    gap: 8px;
  }

  .block-footer {
    display: flex;
    justify-content: space-between;
    white-space: nowrap;
    gap: 8px;
  }

  .block-meta {
    color: var(--meta-color, rgb(0 0 0 / 0.5));
    font-size: 10px;
  }

  .block-hash {
    overflow: hidden;
    text-overflow: ellipsis;
    max-width: 8ch;
  }

  .block-ticker {
    display: block;
    text-align: center;
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
