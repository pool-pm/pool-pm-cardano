<script lang="ts">
  import { onMount } from 'svelte';
  import { SvelteSet } from 'svelte/reactivity';
  import type { PolicyAsset, PolicyResponse } from '../types';

  let { policyId }: { policyId: string } = $props();

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
  // Fingerprints whose thumbnail 404'd (no preview on nftcdn): hide the tile and
  // remember it so a recycled cell doesn't re-request the same 404.
  const broken = new SvelteSet<string>();
  let lastScrollTop = 0;
  let lastScrollTime = 0;
  let settleTimer: ReturnType<typeof setTimeout> | undefined;

  // Broken (404) assets are dropped from layout so the grid reflows and later
  // assets fill the gap, rather than leaving holes. All windowing math is over
  // `visible`, not the raw fetched `assets`.
  const visible = $derived(assets.filter((a) => !broken.has(a.fingerprint)));

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
      const url = cursor === undefined ? `/api/policy/${policyId}` : `/api/policy/${policyId}?cursor=${cursor}`;
      const res = await fetch(url);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const data: PolicyResponse = await res.json();
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
    document.title = `${policyId.slice(0, 12)}…`;
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

<div class="scroll" bind:clientWidth={containerW} bind:clientHeight={viewportH} onscroll={onScroll}>
  {#if error && assets.length === 0}
    <div class="status">Could not load policy: {error}</div>
  {:else if !loading && assets.length === 0}
    <div class="status">No assets for this policy.</div>
  {:else}
    <div class="spacer" style="height:{spacerHeight}px">
      <div class="window" style="transform:translateY({offsetY}px); width:{rowWidth}px">
        {#each slice as a (a.fingerprint)}
          {@const label = a.name ?? a.fingerprint}
          <a class="cell" href={'/' + a.fingerprint} aria-label={label} title={label}>
            <!-- No loading="lazy": windowing already keeps only near-viewport
                 rows mounted, so lazy just delays cached images from repainting
                 when a row is scrolled back into view. New rows are deferred while
                 flinging (suppressImages); rows already loaded keep showing so the
                 grid never blanks out what was on screen. alt="" keeps Firefox from
                 painting the name over a still-loading tile (the link is labelled
                 via aria-label instead). -->
            {#if !suppressImages || loaded.has(a.fingerprint)}
              <!-- onerror records a 404; the asset then drops out of `visible`,
                   collapsing its cell instead of leaving a hole. -->
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

<style>
  .scroll {
    height: 100dvh;
    overflow-y: auto;
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
    background: var(--bg);
    border-radius: 3px;
  }

  .thumb {
    width: 128px;
    height: 128px;
    object-fit: contain;
    border-radius: 3px;
  }

  .status {
    display: flex;
    align-items: center;
    justify-content: center;
    height: 100dvh;
    color: var(--text-muted);
    font-family: system-ui, sans-serif;
  }
</style>
