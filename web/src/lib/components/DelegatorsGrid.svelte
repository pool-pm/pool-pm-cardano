<script lang="ts">
  import { onMount } from 'svelte';
  import type { Delegator, DelegatorDelta, DelegatorsResponse } from '../types';
  import { pool, drep } from '../stores';
  import { onDelegatorLive } from '../sse';
  import { formatAda, formatTicker, poolColor } from '../layout';
  import { shortStake, matchesFilter } from '../delegators';
  import { nextSort, sortFromParams, sortIndex, sortParams, sortTitle, type SortState } from '../sortCycle';

  // `endpoint` is the paginated API URL (`?cursor=&sort=&order=&q=` appended); `title` sets
  // document.title. The grid mirrors AssetsGrid's geometry + windowing, minus everything
  // image-related — a delegator tile is three text bands.
  let {
    endpoint,
    title,
    uiVisible = true,
  }: {
    endpoint: string;
    title: string;
    /** Shared idle-fade signal (App): the toolbar hides with the corner chrome when idle,
     * but only while the filter is empty so an active filter is never hidden away. */
    uiVisible?: boolean;
  } = $props();

  // Uniform-grid geometry (px). A fixed cell size per render is what makes windowing
  // trivial: a row's top is exactly its index * ROW. TILE_TARGET is the *desired* tile
  // width; the actual CELL is derived per render so a whole number of columns fills the
  // container. Unlike the assets grid there's no square media area, so the tile height is
  // a constant — three text bands.
  const TILE_TARGET = 200; // desired tile width (px); text is wider than it is tall
  const GAP = 16;
  const EPOCH_H = 22; // top band: the epoch the delegation started
  const STAKE_H = 44; // middle band: live stake, the tile's headline
  const NAME_H = 30; // bottom band: handle or shortened stake address
  const TILE_H = EPOCH_H + STAKE_H + NAME_H;
  const ROW = TILE_H + GAP;
  const BUFFER_ROWS = 4; // extra rows rendered above/below the viewport
  const PREFETCH_ROWS = 6; // fetch the next page once the buffer gets this close to the end
  const VPAD = 16; // breathing room above the first row / below the last (= GAP)
  const SIDE_PAD = 20; // grid inline padding; the header aligns its edges to this

  let delegators = $state<Delegator[]>([]);
  let cursor = $state<number | undefined>(undefined);
  let hasMore = $state(true);
  let loading = $state(false);
  let error = $state<string | null>(null);
  let total = $state(0);
  // Sort state and filter both live server-side and are mirrored in the URL, so leaving and
  // coming back (open a delegator, then Back) restores them.
  let sort = $state<SortState>(sortFromParams(new URLSearchParams(window.location.search)));
  let q = $state(new URLSearchParams(window.location.search).get('q') ?? '');
  let filterTimer: ReturnType<typeof setTimeout> | undefined;
  // Quarter-turns of the sort arrow. Monotonic (never reset to the state's index) so the
  // arrow keeps turning the same way past the wrap instead of unwinding three quarters.
  let turns = $state(sortIndex(sortFromParams(new URLSearchParams(window.location.search))));

  let scrollEl = $state<HTMLElement | undefined>();
  let hasLoaded = $state(false);
  // Mirrors the stake addresses in `delegators`, so a fetched page can't duplicate a live add.
  const present = new Set<string>();

  let containerW = $state(0);
  let viewportH = $state(0);
  let scrollTop = $state(0);

  const empty = $derived(delegators.length === 0);
  const subject = $derived($pool ?? $drep ?? null);
  const subjectColor = $derived($pool ? poolColor($pool.pool_id) : $drep ? poolColor($drep.drep_id) : '#fff');

  function persistUrl() {
    const url = new URL(window.location.href);
    const params = sortParams(sort);
    for (const key of ['sort', 'order'] as const) {
      const value = params[key];
      if (value) url.searchParams.set(key, value);
      else url.searchParams.delete(key);
    }
    if (q.trim()) url.searchParams.set('q', q.trim());
    else url.searchParams.delete('q');
    history.replaceState(history.state, '', url);
  }

  // Same column/row math as the assets grid — see AssetsGrid.svelte for the why.
  const cols = $derived(Math.max(1, Math.round((containerW + GAP) / (TILE_TARGET + GAP))));
  const CELL = $derived(containerW > 0 ? Math.floor((containerW - (cols - 1) * GAP) / cols) : TILE_TARGET);
  const rowWidth = $derived(cols * CELL + (cols - 1) * GAP);
  const loadedRows = $derived(Math.ceil(delegators.length / cols));
  const totalHeight = $derived(loadedRows > 0 ? (loadedRows - 1) * ROW + TILE_H : 0);
  const spacerHeight = $derived(totalHeight + VPAD * 2);

  let scrollbarW = $state(0);
  $effect(() => {
    void containerW;
    void spacerHeight;
    if (scrollEl) scrollbarW = scrollEl.offsetWidth - scrollEl.clientWidth;
  });
  const gridSlack = $derived(Math.max(0, (containerW - rowWidth) / 2));
  const headPadLeft = $derived(SIDE_PAD + gridSlack);
  const headPadRight = $derived(SIDE_PAD + gridSlack + scrollbarW);

  const firstRow = $derived(Math.floor(Math.max(0, scrollTop - VPAD) / ROW));
  const renderFrom = $derived(Math.max(0, firstRow - BUFFER_ROWS));
  const renderTo = $derived(firstRow + Math.ceil(viewportH / ROW) + BUFFER_ROWS);
  const slice = $derived(delegators.slice(renderFrom * cols, Math.min(delegators.length, renderTo * cols)));
  const offsetY = $derived(VPAD + renderFrom * ROW);

  // Bumped on every sort/filter change so an in-flight page started under the old query
  // discards its response instead of appending it to the freshly-reset list.
  let generation = 0;

  async function loadMore() {
    if (loading || !hasMore) return;
    loading = true;
    const gen = generation;
    try {
      const params = new URLSearchParams();
      if (cursor !== undefined) params.set('cursor', String(cursor));
      const sp = sortParams(sort);
      if (sp.sort) params.set('sort', sp.sort);
      if (sp.order) params.set('order', sp.order);
      if (q.trim()) params.set('q', q.trim());
      const qs = params.toString();
      const res = await fetch(qs ? `${endpoint}?${qs}` : endpoint);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const data: DelegatorsResponse = await res.json();
      if (gen !== generation) return; // a sort/filter change superseded this page
      const fresh = data.delegators.filter((d) => !present.has(d.stake_address));
      for (const d of fresh) present.add(d.stake_address);
      delegators = [...delegators, ...fresh];
      cursor = data.cursor;
      hasMore = data.has_more;
      total = data.total;
      hasLoaded = true;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
      hasMore = false;
    } finally {
      loading = false;
    }
  }

  // Reload from page 1: sort and filter are server-side, so the whole result set changes.
  function resetAndReload() {
    generation++;
    delegators = [];
    present.clear();
    cursor = undefined;
    hasMore = true;
    error = null;
    if (scrollEl) scrollEl.scrollTop = 0;
    scrollTop = 0;
    loadMore();
  }

  function cycleSort() {
    sort = nextSort(sort);
    turns++;
    persistUrl();
    resetAndReload();
  }

  function onFilterInput(e: Event) {
    q = (e.currentTarget as HTMLInputElement).value;
    clearTimeout(filterTimer);
    filterTimer = setTimeout(() => {
      persistUrl();
      resetAndReload();
    }, 250);
  }

  // Keep the buffer ahead: refetch whenever the rendered window comes within PREFETCH_ROWS
  // of the loaded data. Also fires the first load on mount.
  $effect(() => {
    if (hasMore && !loading && renderTo + PREFETCH_ROWS >= loadedRows) {
      loadMore();
    }
  });

  // Apply a live delta. Stake changes are patched **in place**: re-sorting would make tiles
  // jump under the cursor, so the page keeps the server's order until the next reload.
  // `resync` (a rollback, or an epoch boundary crediting rewards to everyone) reloads.
  function handleLive(e: DelegatorDelta) {
    if (e.resync) {
      resetAndReload();
      return;
    }
    if (e.removed.length) {
      const drop = new Set(e.removed);
      for (const addr of drop) present.delete(addr);
      delegators = delegators.filter((d) => !drop.has(d.stake_address));
      total = Math.max(0, total - drop.size);
    }
    if (e.updated.length) {
      const byAddr = new Map(e.updated.map((u) => [u.stake_address, u.live_stake]));
      delegators = delegators.map((d) => {
        const live = byAddr.get(d.stake_address);
        return live !== undefined ? { ...d, live_stake: live } : d;
      });
    }
    // A new delegator only joins the visible list if it passes the active filter.
    const adds = e.added.filter((d) => !present.has(d.stake_address) && matchesFilter(d, q));
    if (adds.length) {
      for (const d of adds) present.add(d.stake_address);
      delegators = [...adds, ...delegators];
      total += adds.length;
    }
  }

  onMount(() => {
    document.title = title;
    return onDelegatorLive(handleLive);
  });

  function onScroll(e: Event) {
    scrollTop = (e.currentTarget as HTMLElement).scrollTop;
  }

  function tileTitle(d: Delegator): string {
    const who = d.handle ? `$${d.handle} — ${d.stake_address}` : d.stake_address;
    return d.since ? `${who}\nDelegating since ${new Date(d.since * 1000).toLocaleDateString()}` : who;
  }
</script>

<!-- One delegator tile: when the run started (grey), the live stake (the headline), and
     who — the ADA Handle if there is one, else the shortened stake address. -->
{#snippet delegatorCell(d: Delegator)}
  <a class="tile" href={'/' + d.stake_address} title={tileTitle(d)}>
    <span class="frame">
      <span class="epoch">{d.epoch != null ? `epoch ${d.epoch}` : ''}</span>
      <span class="stake">{formatAda(d.live_stake)}</span>
      <span class="name">
        {#if d.handle}
          <span class="name-text"><span class="dollar">$</span>{d.handle}</span>
        {:else}
          <span class="name-text mono">{shortStake(d.stake_address)}</span>
        {/if}
      </span>
    </span>
  </a>
{/snippet}

<div class="page">
  <!-- Top row: the subject (ticker/name, live stake, counts) on the left, aligned to the
       leftmost tile; the filter + sort toolbar on the right, aligned to the rightmost one. -->
  <div class="assets-head" style="padding-left:{headPadLeft}px; padding-right:{headPadRight}px">
    {#if subject}
      <div class="subject">
        <a class="subject-balance" href="/{$pool ? $pool.pool_id : $drep!.drep_id}">{formatAda(subject.live_stake)}</a>
        <div class="subject-id" style:color={subjectColor}>
          {#if $pool}
            {formatTicker($pool.ticker ?? $pool.pool_id.slice(5, 10))}
          {:else if $drep}
            {$drep.given_name ?? $drep.drep_id.slice(5, 13)}
          {/if}
        </div>
        <div class="subject-counts">
          <span class="lbl">delegators</span><span class="val">{subject.delegators.toLocaleString()}</span>
          {#if $pool}
            <span class="slash">·</span><span class="lbl">blocks</span><span class="val"
              >{$pool.blocks.toLocaleString()}</span
            >
          {:else if $drep}
            <span class="slash">·</span><span class="lbl">votes</span><span class="val"
              >{($drep.votes ?? 0).toLocaleString()}</span
            >
          {/if}
        </div>
      </div>
    {/if}
    {#if hasLoaded}
      <div class="toolbar" class:idle-hidden={!uiVisible && q.trim() === ''} style="--toolbar-w: {CELL}px">
        <div class="filter">
          <input
            class="filter-input"
            type="text"
            placeholder="Filter by handle or address"
            value={q}
            oninput={onFilterInput}
            aria-label="Filter delegators by handle or address"
          />
          <!-- Four sort states, one quarter turn per click: stake ↓, delegation time ←,
               stake ↑, delegation time →. -->
          <button class="sort-btn" onclick={cycleSort} title={sortTitle(sort)} aria-label={sortTitle(sort)}>
            <svg
              class="sort-arrow"
              style="transform: rotate({turns * 90}deg)"
              viewBox="0 0 12 12"
              width="12"
              height="12"
              aria-hidden="true"
            >
              <path d="M6 1.5 V10.5 M2.5 7 L6 10.5 L9.5 7" fill="none" stroke="currentColor" stroke-width="1.4" />
            </svg>
          </button>
        </div>
      </div>
    {/if}
  </div>
  <div class="scroll" bind:this={scrollEl} bind:clientHeight={viewportH} onscroll={onScroll}>
    {#if error && empty}
      <div class="status">Could not load: {error}</div>
    {:else if !loading && empty}
      <!-- A pool/DRep nobody delegates to (yet) is not an error and not "not found". -->
      <div class="status">{q.trim() ? 'No matching delegators.' : 'No delegators.'}</div>
    {:else}
      <div class="spacer" bind:clientWidth={containerW} style="height:{spacerHeight}px">
        <div
          class="window"
          style="transform:translateY({offsetY}px); width:{rowWidth}px; --cell:{CELL}px; --tile-h:{TILE_H}px; --epoch-h:{EPOCH_H}px; --stake-h:{STAKE_H}px; --name-h:{NAME_H}px; --gap:{GAP}px"
        >
          {#each slice as d (d.stake_address)}
            {@render delegatorCell(d)}
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
    box-sizing: border-box;
    background: var(--bg);
  }

  /* Head row: subject bottom-left, toolbar bottom-right, both one GAP above the grid.
     Mirrors the assets grid so the two pages read as one family. */
  .assets-head {
    display: flex;
    align-items: flex-end;
    justify-content: space-between;
    gap: 16px;
    min-height: 116px;
    padding: 12px 20px 0;
    box-sizing: border-box;
    background: var(--bg);
  }
  .subject {
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .subject-balance {
    display: block;
    width: fit-content;
    font-size: 42px;
    font-weight: 650;
    line-height: 1.1;
    color: rgb(255 255 255 / 0.92);
    font-variant-numeric: tabular-nums;
    margin-bottom: 1px;
    text-decoration: none;
  }
  .subject-balance:hover {
    text-decoration: underline;
  }
  /* The subject's identity, in its own colour — the one splash on an otherwise grey page. */
  .subject-id {
    max-width: 58vw;
    font-size: 15px;
    font-weight: 600;
    line-height: 1.2;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .subject-counts {
    display: flex;
    align-items: baseline;
    gap: 5px;
    font-size: 12px;
    color: rgb(255 255 255 / 0.75);
  }
  .lbl {
    text-transform: uppercase;
    letter-spacing: 0.08em;
    font-size: 9px;
    color: rgb(255 255 255 / 0.4);
  }
  .val {
    font-variant-numeric: tabular-nums;
  }
  .slash {
    color: rgb(255 255 255 / 0.3);
  }

  .scroll {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding-inline: 20px;
    box-sizing: border-box;
    background: var(--bg);
  }

  .toolbar {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-left: auto;
    align-self: flex-end;
    flex-shrink: 0;
    opacity: 1;
    transition: opacity 0.15s ease;
  }
  .toolbar.idle-hidden {
    opacity: 0;
    pointer-events: none;
    transition: opacity 1.5s ease;
  }

  /* Flat: the panel is the tile grey, the input inside it fully black. */
  .filter {
    display: flex;
    align-items: center;
    gap: 3px;
    height: 28px;
    box-sizing: border-box;
    width: var(--toolbar-w, 240px);
    border-radius: var(--panel-radius);
    padding: 3px;
    background: var(--surface-2);
    border: none;
  }
  .filter-input {
    flex: 1;
    min-width: 0;
    height: 100%;
    box-sizing: border-box;
    padding: 0 9px;
    border: none;
    border-radius: 7px;
    color: rgb(255 255 255 / 0.85);
    font-family: Inter, sans-serif;
    font-size: 12px;
    outline: none;
    background: var(--bg);
    transition: background 0.18s ease;
  }
  .filter-input::placeholder {
    color: rgb(255 255 255 / 0.4);
  }
  .filter-input:focus {
    background: #0b0b0e;
  }
  .sort-btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    height: 100%;
    aspect-ratio: 1;
    padding: 0;
    border: none;
    border-radius: 7px;
    color: rgb(255 255 255 / 0.55);
    cursor: pointer;
    background: transparent;
    transition:
      color 0.18s ease,
      background 0.12s ease;
  }
  .sort-btn:hover,
  .sort-btn:focus-visible {
    outline: none;
    color: rgb(255 255 255 / 0.85);
  }
  .sort-btn:active {
    color: rgb(255 255 255 / 0.85);
    background: var(--bg);
  }
  .sort-arrow {
    transition: transform 0.2s ease;
  }

  .spacer {
    position: relative;
    width: 100%;
  }

  /* flex-wrap (not grid) so the partial last row is centered too; the width is pinned to
     exactly `cols` cells so it can't drift to cols-1. */
  .window {
    position: absolute;
    top: 0;
    left: 0;
    right: 0;
    margin-inline: auto;
    display: flex;
    flex-wrap: wrap;
    gap: var(--gap);
    justify-content: center;
  }

  .tile {
    flex: none;
    width: var(--cell);
    display: flex;
    flex-direction: column;
    text-decoration: none;
    -webkit-tap-highlight-color: transparent;
  }
  .tile:focus-visible {
    outline: none;
  }

  .frame {
    display: flex;
    flex-direction: column;
    width: 100%;
    height: var(--tile-h);
    box-sizing: border-box;
    border-radius: var(--panel-radius);
    background: var(--surface-2);
    border: none;
    overflow: hidden;
    transition: transform 0.18s ease;
  }

  /* When this delegator's current run started. Same tone as an asset tile's quantity band. */
  .epoch {
    height: var(--epoch-h);
    box-sizing: border-box;
    padding: 6px 8px 0;
    width: 100%;
    text-align: center;
    color: rgb(255 255 255 / 0.5);
    font-family: Inter, sans-serif;
    font-size: 11px;
    line-height: 1.3;
    font-variant-numeric: tabular-nums slashed-zero;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  /* The headline: this delegator's live stake. */
  .stake {
    height: var(--stake-h);
    box-sizing: border-box;
    width: 100%;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 0 8px;
    color: rgb(255 255 255 / 0.92);
    font-size: 20px;
    font-weight: 600;
    font-variant-numeric: tabular-nums;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .name {
    height: var(--name-h);
    box-sizing: border-box;
    width: 100%;
    padding: 0 8px 8px;
    display: flex;
    align-items: flex-end;
    justify-content: center;
    overflow: hidden;
  }
  .name-text {
    color: rgb(255 255 255 / 0.5);
    font-size: 12px;
    line-height: 1.25;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .name-text.mono {
    font-family: 'SF Mono', Menlo, Consolas, monospace;
    font-size: 11px;
  }
  .dollar {
    color: rgb(255 255 255 / 0.3);
  }

  .tile:hover .frame,
  .tile:focus-visible .frame {
    transform: translateY(-3px);
  }

  @media (prefers-reduced-motion: reduce) {
    .frame,
    .sort-arrow {
      transition: none;
    }
    .tile:hover .frame,
    .tile:focus-visible .frame {
      transform: none;
    }
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
