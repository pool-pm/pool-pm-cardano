<script lang="ts">
  import { onMount } from 'svelte';
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

  // While flinging fast, don't mount <img> for rows we pass: each thumbnail is its
  // own nftcdn subdomain, so loading rows we never stop on floods the browser's
  // request/decode pipeline and starves the row we actually land on. Images load
  // once scrolling settles.
  let suppressImages = $state(false);
  let lastScrollTop = 0;
  let lastScrollTime = 0;
  let settleTimer: ReturnType<typeof setTimeout> | undefined;

  const cols = $derived(Math.max(1, Math.floor((containerW + GAP) / ROW)));
  const loadedRows = $derived(Math.ceil(assets.length / cols));
  // Content height: rows are ROW apart, the last row adds only its cell height;
  // VPAD is reserved above the first row and below the last.
  const totalHeight = $derived(loadedRows > 0 ? (loadedRows - 1) * ROW + CELL : 0);
  const spacerHeight = $derived(totalHeight + VPAD * 2);

  // Rows live at y = VPAD + row*ROW, so the scroll math offsets by VPAD too.
  const firstRow = $derived(Math.floor(Math.max(0, scrollTop - VPAD) / ROW));
  const renderFrom = $derived(Math.max(0, firstRow - BUFFER_ROWS));
  const renderTo = $derived(firstRow + Math.ceil(viewportH / ROW) + BUFFER_ROWS);
  const startIndex = $derived(renderFrom * cols);
  const endIndex = $derived(Math.min(assets.length, renderTo * cols));
  const slice = $derived(assets.slice(startIndex, endIndex));
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
      <div class="window" style="transform:translateY({offsetY}px); --cols:{cols}">
        {#each slice as a (a.fingerprint)}
          <a
            class="cell"
            href={'/' + a.fingerprint}
            target="_blank"
            rel="noopener noreferrer"
            title={a.name ?? a.fingerprint}
          >
            <!-- No loading="lazy": windowing already keeps only near-viewport
                 rows mounted, so lazy just delays cached images from repainting
                 when a row is scrolled back into view. Skipped entirely while
                 flinging (suppressImages) so we don't queue loads we fly past. -->
            {#if !suppressImages}
              <img
                class="thumb"
                src={a.src}
                srcset={a.srcset}
                decoding="async"
                alt={a.name ?? a.fingerprint}
                onerror={(e: Event) => {
                  (e.currentTarget as HTMLElement).style.visibility = 'hidden';
                }}
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

  .window {
    position: absolute;
    top: 0;
    left: 0;
    right: 0;
    display: grid;
    grid-template-columns: repeat(var(--cols), 128px);
    gap: 8px;
    justify-content: center;
  }

  .cell {
    width: 128px;
    height: 128px;
    display: block;
    /* Faint placeholder so a tile whose image hasn't decoded yet reads as
       "loading" rather than as the black page background. */
    background: rgb(255 255 255 / 0.04);
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
