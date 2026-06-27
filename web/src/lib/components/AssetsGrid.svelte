<script lang="ts">
  import { onMount } from 'svelte';
  import { SvelteSet } from 'svelte/reactivity';
  import type { PolicyAsset, AssetsResponse } from '../types';
  import { stake, address } from '../stores';
  import SubjectCard from './SubjectCard.svelte';

  // `endpoint` is the paginated API URL (cursor is appended as `?cursor=`); `title`
  // sets document.title. `mode` controls cell rendering: 'hide-broken' (policy
  // pages: tiles whose image 404s drop out of the grid) or 'text-fallback' (owned-
  // assets: the cell keeps showing, with the decoded asset name or fingerprint as
  // a backdrop the image covers when it loads — so non-image tokens stay visible).
  let {
    endpoint,
    title,
    mode = 'hide-broken',
  }: { endpoint: string; title: string; mode?: 'hide-broken' | 'text-fallback' } = $props();

  // Uniform-grid geometry (px). The fixed cell size is what makes windowing
  // trivial: a row's top is exactly its index * ROW, so no measurement is needed.
  const CELL = 128;
  const GAP = 8;
  const ROW = CELL + GAP;
  const BUFFER_ROWS = 4; // extra rows rendered above/below the viewport
  const PREFETCH_ROWS = 6; // fetch the next page once the buffer gets this close to the end
  const VPAD = 16; // breathing room above the first row / below the last
  const FAST_SCROLL_PX_PER_MS = 3; // above this fling speed, defer image loads
  const SCROLL_SETTLE_MS = 120; // load the settled view this long after scrolling stops

  let assets = $state<PolicyAsset[]>([]);
  let cursor = $state<number | undefined>(undefined);
  let hasMore = $state(true);
  let loading = $state(false);
  let error = $state<string | null>(null);

  // Measured from the scroll container; drive layout + windowing reactively.
  let containerW = $state(0);
  let viewportH = $state(0);
  let scrollTop = $state(0);

  // While flinging fast, defer loading thumbnails for rows we haven't seen yet, so
  // we don't queue a burst of requests for rows we only pass through. Rows whose
  // image already loaded keep showing (served from cache), so the grid never
  // blanks out what was on screen — only brand-new rows wait for the scroll to
  // settle.
  let suppressImages = $state(false);
  const loaded = new SvelteSet<string>();
  // Fingerprints whose thumbnail 404'd. In 'hide-broken' mode they drop out of
  // `visible` so the grid reflows; in 'text-fallback' mode the cell stays
  // (showing its text backdrop) and `broken` only suppresses the `<img>` so a
  // recycled cell doesn't re-request the same 404.
  const broken = new SvelteSet<string>();
  let lastScrollTop = 0;
  let lastScrollTime = 0;
  let settleTimer: ReturnType<typeof setTimeout> | undefined;

  // 'hide-broken' filters 404s out so the grid reflows and later assets fill the
  // gap; 'text-fallback' keeps every cell so name/fingerprint stays readable.
  const visible = $derived(mode === 'hide-broken' ? assets.filter((a) => !broken.has(a.fingerprint)) : assets);

  const cols = $derived(Math.max(1, Math.floor((containerW + GAP) / ROW)));
  // Exact width of one full row of `cols` cells. Pinning the flex container to
  // this (rather than the full container width) makes it wrap at exactly `cols`
  // per row — matching the slice math deterministically, instead of letting
  // sub-pixel rounding drift to cols-1 and unmount on-screen rows (black gaps).
  const rowWidth = $derived(cols * CELL + (cols - 1) * GAP);
  const loadedRows = $derived(Math.ceil(visible.length / cols));
  // Content height: rows are ROW apart, the last row adds only its cell height;
  // VPAD is reserved above the first row and below the last.
  const totalHeight = $derived(loadedRows > 0 ? (loadedRows - 1) * ROW + CELL : 0);
  const spacerHeight = $derived(totalHeight + VPAD * 2);

  // Rows live at y = VPAD + row*ROW, so the scroll math offsets by VPAD too.
  const firstRow = $derived(Math.floor(Math.max(0, scrollTop - VPAD) / ROW));
  const renderFrom = $derived(Math.max(0, firstRow - BUFFER_ROWS));
  const renderTo = $derived(firstRow + Math.ceil(viewportH / ROW) + BUFFER_ROWS);
  const startIndex = $derived(renderFrom * cols);
  const endIndex = $derived(Math.min(visible.length, renderTo * cols));
  const slice = $derived(visible.slice(startIndex, endIndex));
  const offsetY = $derived(VPAD + renderFrom * ROW);

  async function loadMore() {
    if (loading || !hasMore) return;
    loading = true;
    try {
      const url = cursor === undefined ? endpoint : `${endpoint}?cursor=${cursor}`;
      const res = await fetch(url);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const data: AssetsResponse = await res.json();
      assets = [...assets, ...data.assets];
      cursor = data.cursor;
      hasMore = data.has_more;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
      hasMore = false;
    } finally {
      loading = false;
    }
  }

  // Keep the buffer ahead: refetch whenever the rendered window (grown by scroll,
  // resize, or measurement) comes within PREFETCH_ROWS of the loaded data. Also
  // fires the very first load on mount and tops up tall viewports that one page
  // can't fill. Terminates: each page grows loadedRows or clears hasMore.
  $effect(() => {
    if (hasMore && !loading && renderTo + PREFETCH_ROWS >= loadedRows) {
      loadMore();
    }
  });

  onMount(() => {
    document.title = title;
    return () => clearTimeout(settleTimer);
  });

  function onScroll(e: Event) {
    const el = e.currentTarget as HTMLElement;
    const now = performance.now();
    const dt = now - lastScrollTime;
    const velocity = dt > 0 ? Math.abs(el.scrollTop - lastScrollTop) / dt : 0;
    lastScrollTop = el.scrollTop;
    lastScrollTime = now;
    scrollTop = el.scrollTop;

    if (velocity > FAST_SCROLL_PX_PER_MS) suppressImages = true;
    clearTimeout(settleTimer);
    settleTimer = setTimeout(() => (suppressImages = false), SCROLL_SETTLE_MS);
  }
</script>

<div class="page">
  <!-- Populated only on the owned-assets page (address/stake), where App connects
       the SSE feed; on policy pages both stores stay null so nothing renders. -->
  <SubjectCard stake={$stake} address={$address} />
  <div class="scroll" bind:clientHeight={viewportH} onscroll={onScroll}>
    {#if error && assets.length === 0}
      <div class="status">Could not load: {error}</div>
    {:else if !loading && assets.length === 0}
      <div class="status">No assets.</div>
    {:else}
      <!-- clientWidth is bound here (not on .scroll): .scroll carries the horizontal
           padding, so the spacer's content-box width is what the column math needs. -->
      <div class="spacer" bind:clientWidth={containerW} style="height:{spacerHeight}px">
        <div class="window" style="transform:translateY({offsetY}px); width:{rowWidth}px">
          {#each slice as a (a.fingerprint)}
            {@const label = a.name ?? a.fingerprint}
            <a class="cell" href={'/' + a.fingerprint} aria-label={label} title={label}>
              <!-- text-fallback mode: the label sits at z=0 and the image covers
                 it when (and only when) it loads. For broken/404 images we
                 keep the cell in the grid (in 'hide-broken' mode the cell
                 would have dropped out of `visible` already). -->
              {#if mode === 'text-fallback'}
                <span class="cell-text">{label}</span>
              {/if}
              <!-- No loading="lazy": windowing already keeps only near-viewport
                 rows mounted, so lazy just delays cached images from repainting
                 when a row is scrolled back into view. New rows are deferred while
                 flinging (suppressImages); rows already loaded keep showing so the
                 grid never blanks out what was on screen. alt="" keeps Firefox from
                 painting the name over a still-loading tile (the link is labelled
                 via aria-label instead). -->
              {#if (!suppressImages || loaded.has(a.fingerprint)) && !broken.has(a.fingerprint)}
                <img
                  class="thumb"
                  src={a.src}
                  srcset={a.srcset}
                  decoding="async"
                  alt=""
                  onload={() => loaded.add(a.fingerprint)}
                  onerror={() => broken.add(a.fingerprint)}
                />
              {/if}
            </a>
          {/each}
        </div>
      </div>
    {/if}
  </div>
</div>

<style>
  .page {
    display: flex;
    flex-direction: column;
    height: 100dvh;
    /* Top breathing room for the header card, matching the feed's 16px top padding.
       The card centers itself (margin-inline auto) and clears the corner chrome. */
    padding-top: 16px;
    box-sizing: border-box;
    background: var(--bg);
  }

  .scroll {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    /* Horizontal breathing room so the grid doesn't run to the window edge, matching
       the feed's 16px 20px. clientWidth is measured on the inner .spacer instead, so
       this padding shrinks the usable width the column math sees. */
    padding-inline: 20px;
    box-sizing: border-box;
    background: var(--bg);
  }

  /* Reserves the full scroll height of all loaded rows; the window is absolutely
     positioned inside it and slid down via translateY. */
  .spacer {
    position: relative;
    width: 100%;
  }

  /* flex-wrap (not grid) so the partial last row is centered too, not left-packed.
     Width is pinned (inline) to exactly `cols` cells and the box is centered via
     auto margins; that makes it wrap at exactly `cols`/row (no sub-pixel drift)
     while justify-content:center centers the partial last row. */
  .window {
    position: absolute;
    top: 0;
    left: 0;
    right: 0;
    margin-inline: auto;
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    justify-content: center;
  }

  .cell {
    flex: none;
    width: 128px;
    height: 128px;
    display: block;
    position: relative; /* so .cell-text and .thumb can stack via position:absolute */
    background: var(--bg);
    border-radius: 3px;
    overflow: hidden;
  }

  /* The text-fallback label sits behind the image; image cover when loaded. */
  .cell-text {
    position: absolute;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 4px;
    color: var(--text-muted, #9c9c9c);
    font-family: system-ui, sans-serif;
    font-size: 11px;
    text-align: center;
    word-break: break-all;
    overflow: hidden;
  }

  .thumb {
    position: absolute;
    inset: 0;
    width: 128px;
    height: 128px;
    object-fit: contain;
    border-radius: 3px;
  }

  .status {
    display: flex;
    align-items: center;
    justify-content: center;
    height: 100%;
    color: var(--text-muted);
    font-family: system-ui, sans-serif;
  }
</style>
