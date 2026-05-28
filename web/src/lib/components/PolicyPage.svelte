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

  let assets = $state<PolicyAsset[]>([]);
  let cursor = $state<number | undefined>(undefined);
  let hasMore = $state(true);
  let loading = $state(false);
  let error = $state<string | null>(null);

  // Measured from the scroll container; drive layout + windowing reactively.
  let containerW = $state(0);
  let viewportH = $state(0);
  let scrollTop = $state(0);

  const cols = $derived(Math.max(1, Math.floor((containerW + GAP) / ROW)));
  const loadedRows = $derived(Math.ceil(assets.length / cols));
  // Content height: rows are ROW apart, the last row adds only its cell height.
  const totalHeight = $derived(loadedRows > 0 ? (loadedRows - 1) * ROW + CELL : 0);

  const firstRow = $derived(Math.floor(scrollTop / ROW));
  const renderFrom = $derived(Math.max(0, firstRow - BUFFER_ROWS));
  const renderTo = $derived(firstRow + Math.ceil(viewportH / ROW) + BUFFER_ROWS);
  const startIndex = $derived(renderFrom * cols);
  const endIndex = $derived(Math.min(assets.length, renderTo * cols));
  const slice = $derived(assets.slice(startIndex, endIndex));
  const offsetY = $derived(renderFrom * ROW);

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
  });

  function onScroll(e: Event) {
    scrollTop = (e.currentTarget as HTMLElement).scrollTop;
  }
</script>

<div class="scroll" bind:clientWidth={containerW} bind:clientHeight={viewportH} onscroll={onScroll}>
  {#if error && assets.length === 0}
    <div class="status">Could not load policy: {error}</div>
  {:else if !loading && assets.length === 0}
    <div class="status">No assets for this policy.</div>
  {:else}
    <div class="spacer" style="height:{totalHeight}px">
      <div class="window" style="transform:translateY({offsetY}px); --cols:{cols}">
        {#each slice as a (a.fingerprint)}
          <a class="cell" href={'/' + a.fingerprint} title={a.name ?? a.fingerprint}>
            <img
              class="thumb"
              src={a.src}
              srcset={a.srcset}
              loading="lazy"
              alt={a.name ?? a.fingerprint}
              onerror={(e: Event) => {
                (e.currentTarget as HTMLElement).style.visibility = 'hidden';
              }}
            />
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
