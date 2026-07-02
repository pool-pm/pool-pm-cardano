<script lang="ts">
  import { onMount } from 'svelte';
  import { SvelteSet } from 'svelte/reactivity';
  import type { PolicyAsset, AssetGroup, AssetsResponse, GroupsResponse, AssetDelta } from '../types';
  import { stake, address } from '../stores';
  import { onAssetLive } from '../sse';
  import SubjectCard from './SubjectCard.svelte';

  // `endpoint` is the paginated API URL (cursor is appended as `?cursor=`); `title`
  // sets document.title. `mode` controls cell rendering: 'hide-broken' (policy
  // pages: tiles whose image 404s drop out of the grid) or 'text-fallback' (owned-
  // assets: the cell keeps showing, with the decoded asset name or fingerprint as
  // a backdrop the image covers when it loads — so non-image tokens stay visible).
  //
  // `grouped` switches the owned-assets grid to one tile per policy: a stacked-card
  // thumbnail + asset count, drilling into `/{subject}/assets/{policy}`. `policyFilter`
  // (on the flat drill-down) restricts live deltas to that one policy.
  let {
    endpoint,
    title,
    mode = 'hide-broken',
    grouped = false,
    subject = '',
    policyFilter,
  }: {
    endpoint: string;
    title: string;
    mode?: 'hide-broken' | 'text-fallback';
    grouped?: boolean;
    subject?: string;
    policyFilter?: string;
  } = $props();

  // Uniform-grid geometry (px). A fixed cell size per render is what makes windowing
  // trivial: a row's top is exactly its index * ROW, so no measurement is needed.
  // TILE_TARGET is the *desired* tile width; the actual CELL is derived per render so
  // a whole number of columns fills the container width (a gallery wall, not a
  // left-packed sheet). CELL is still constant within a render, so the row math holds.
  const TILE_TARGET = 168; // desired tile edge (px); actual CELL flexes around it
  const GAP = 16;
  // Mat inset: the framed border of empty space around each artwork. The art box is
  // CELL - 2*MAT, and stacked cards size within that inner box.
  const MAT = 10;
  // Fixed offset between stacked cards: each card behind peeks by exactly this much
  // regardless of how many are stacked (the card *size* shrinks to fit instead). Front
  // card fills the inner art box, so card size = (CELL - 2*MAT) - (n-1)*STACK_STEP.
  const STACK_STEP = 20;
  const GROUP_SAMPLES = 4; // max sample cards in a stack — must match the server
  const BUFFER_ROWS = 4; // extra rows rendered above/below the viewport
  const PREFETCH_ROWS = 6; // fetch the next page once the buffer gets this close to the end
  const VPAD = 16; // breathing room above the first row / below the last
  const FAST_SCROLL_PX_PER_MS = 3; // above this fling speed, defer image loads
  const SCROLL_SETTLE_MS = 120; // load the settled view this long after scrolling stops

  let assets = $state<PolicyAsset[]>([]); // flat mode
  let groups = $state<AssetGroup[]>([]); // grouped mode
  let cursor = $state<number | undefined>(undefined);
  let hasMore = $state(true);
  let loading = $state(false);
  let error = $state<string | null>(null);

  // Live de-dup: `present` mirrors the fingerprints in `assets`; `presentPolicies` the
  // policies in `groups` — so a fetched page can't duplicate a live add.
  const present = new Set<string>();
  const presentPolicies = new Set<string>();

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

  // The list the windowing operates on: policy groups when grouped, else asset tiles.
  const items: PolicyAsset[] | AssetGroup[] = $derived(grouped ? groups : visible);
  const empty = $derived((grouped ? groups.length : assets.length) === 0);

  // Pick the column count that lands each tile nearest TILE_TARGET, then size the
  // square CELL to fill the row (floored so the row never overflows the container).
  // CELL/ROW are constant within a render, so the windowing math below stays exact.
  const cols = $derived(Math.max(1, Math.round((containerW + GAP) / (TILE_TARGET + GAP))));
  const CELL = $derived(containerW > 0 ? Math.floor((containerW - (cols - 1) * GAP) / cols) : TILE_TARGET);
  const ROW = $derived(CELL + GAP);
  // Exact width of one full row of `cols` cells. Pinning the flex container to
  // this (rather than the full container width) makes it wrap at exactly `cols`
  // per row — matching the slice math deterministically, instead of letting
  // sub-pixel rounding drift to cols-1 and unmount on-screen rows (black gaps).
  const rowWidth = $derived(cols * CELL + (cols - 1) * GAP);
  const loadedRows = $derived(Math.ceil(items.length / cols));
  // Content height: rows are ROW apart, the last row adds only its cell height;
  // VPAD is reserved above the first row and below the last.
  const totalHeight = $derived(loadedRows > 0 ? (loadedRows - 1) * ROW + CELL : 0);
  const spacerHeight = $derived(totalHeight + VPAD * 2);

  // Rows live at y = VPAD + row*ROW, so the scroll math offsets by VPAD too.
  const firstRow = $derived(Math.floor(Math.max(0, scrollTop - VPAD) / ROW));
  const renderFrom = $derived(Math.max(0, firstRow - BUFFER_ROWS));
  const renderTo = $derived(firstRow + Math.ceil(viewportH / ROW) + BUFFER_ROWS);
  const startIndex = $derived(renderFrom * cols);
  const endIndex = $derived(Math.min(items.length, renderTo * cols));
  const slice = $derived(items.slice(startIndex, endIndex));
  const offsetY = $derived(VPAD + renderFrom * ROW);

  async function loadMore() {
    if (loading || !hasMore) return;
    loading = true;
    try {
      const url = cursor === undefined ? endpoint : `${endpoint}?cursor=${cursor}`;
      const res = await fetch(url);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      if (grouped) {
        const data: GroupsResponse = await res.json();
        const fresh = data.groups.filter((g) => !presentPolicies.has(g.policy));
        for (const g of fresh) presentPolicies.add(g.policy);
        groups = [...groups, ...fresh];
        cursor = data.cursor;
        hasMore = data.has_more;
      } else {
        const data: AssetsResponse = await res.json();
        // Skip anything already shown via a live add, so a page can't duplicate it.
        const fresh = data.assets.filter((a) => !present.has(a.fingerprint));
        for (const a of fresh) present.add(a.fingerprint);
        assets = [...assets, ...fresh];
        cursor = data.cursor;
        hasMore = data.has_more;
      }
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

  // Apply a live asset delta. A rollback arrives as an ordinary corrective delta — the
  // server diffs the reverted snapshot against the previous one — so there's nothing
  // special to undo. Owned-assets pages only (policy pages never connect SSE).
  function handleLive(e: AssetDelta) {
    if (grouped) return handleLiveGrouped(e);
    let { added, removed } = e;
    // The flat drill-down only cares about its one policy.
    if (policyFilter) {
      added = added.filter((a) => a.policy === policyFilter);
      removed = removed.filter((r) => r.policy === policyFilter);
    }
    const drop = new Set(removed.map((r) => r.fingerprint).filter((fp) => present.has(fp)));
    if (drop.size) {
      for (const fp of drop) present.delete(fp);
      assets = assets.filter((a) => !drop.has(a.fingerprint));
    }
    const addTiles = added.filter((a) => !present.has(a.fingerprint));
    for (const a of addTiles) present.add(a.fingerprint);
    if (addTiles.length) assets = [...addTiles, ...assets];
  }

  // Grouped: route each delta to its policy group — decrement/increment counts, drop a
  // group at zero, create one (prepended, newest-first) on a policy's first held asset.
  function handleLiveGrouped(e: AssetDelta) {
    let next = groups;
    for (const r of e.removed) {
      next = next
        .map((g) =>
          g.policy === r.policy
            ? { ...g, count: g.count - 1, samples: g.samples.filter((s) => s.fingerprint !== r.fingerprint) }
            : g,
        )
        .filter((g) => {
          if (g.count > 0) return true;
          presentPolicies.delete(g.policy);
          return false;
        });
    }
    for (const a of e.added) {
      if (next.some((g) => g.policy === a.policy)) {
        next = next.map((g) =>
          g.policy === a.policy
            ? { ...g, count: g.count + 1, samples: g.samples.length < GROUP_SAMPLES ? [...g.samples, a] : g.samples }
            : g,
        );
      } else {
        presentPolicies.add(a.policy);
        next = [{ policy: a.policy, count: 1, samples: [a] }, ...next];
      }
    }
    groups = next;
  }

  onMount(() => {
    document.title = title;
    const offLive = onAssetLive(handleLive);
    return () => {
      offLive();
      clearTimeout(settleTimer);
    };
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

<!-- One framed asset tile (also used for a single-asset policy group): a matted
     frame around the artwork, with the name on a caption revealed on hover/focus. -->
{#snippet assetCell(a: PolicyAsset)}
  {@const label = a.name ?? a.fingerprint}
  {@const isText = mode === 'text-fallback' && broken.has(a.fingerprint)}
  <a class="frame" class:text={isText} href={'/' + a.fingerprint} aria-label={label} title={label}>
    <span class="art">
      {#if isText}
        <!-- Image 404'd: the name/fingerprint stands in for the missing art as a placard. -->
        <span class="cell-text">{label}</span>
      {:else if !suppressImages || loaded.has(a.fingerprint)}
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
    </span>
    {#if a.quantity}
      <!-- Owned amount (decimals-applied); the server omits it when it's 1. -->
      <span class="qty">{a.quantity}</span>
    {/if}
    <span class="cap">{label}</span>
  </a>
{/snippet}

<div class="page">
  <!-- Populated only on the owned-assets page (address/stake), where App connects
       the SSE feed; on policy pages both stores stay null so nothing renders. -->
  <SubjectCard stake={$stake} address={$address} />
  <div class="scroll" bind:clientHeight={viewportH} onscroll={onScroll}>
    {#if error && empty}
      <div class="status">Could not load: {error}</div>
    {:else if !loading && empty}
      <div class="status">No assets.</div>
    {:else}
      <!-- clientWidth is bound here (not on .scroll): .scroll carries the horizontal
           padding, so the spacer's content-box width is what the column math needs. -->
      <div class="spacer" bind:clientWidth={containerW} style="height:{spacerHeight}px">
        <div
          class="window"
          style="transform:translateY({offsetY}px); width:{rowWidth}px; --cell:{CELL}px; --mat:{MAT}px; --gap:{GAP}px"
        >
          {#each slice as item (grouped ? (item as AssetGroup).policy : (item as PolicyAsset).fingerprint)}
            {#if grouped}
              {@const g = item as AssetGroup}
              {#if g.count <= 1}
                {@render assetCell(g.samples[0])}
              {:else}
                {@const n = g.samples.length}
                {@const inner = CELL - 2 * MAT}
                {@const card = inner - (n - 1) * STACK_STEP}
                <!-- Stacked cards inside the mat: back card top-left, front card
                     bottom-right; the chip shows the true asset count. Drills into the policy. -->
                <a
                  class="frame stack"
                  href={`/${subject}/assets/${g.policy}`}
                  aria-label={`${g.count} assets`}
                  title={`${g.count} assets`}
                >
                  <span class="art">
                    {#each g.samples as s, i (s.fingerprint)}
                      <span
                        class="stack-card"
                        style="left:{i * STACK_STEP}px; top:{i * STACK_STEP}px; width:{card}px; height:{card}px"
                      >
                        {#if (!suppressImages || loaded.has(s.fingerprint)) && !broken.has(s.fingerprint)}
                          <img
                            class="card-img"
                            src={s.src}
                            srcset={s.srcset}
                            decoding="async"
                            alt=""
                            onload={() => loaded.add(s.fingerprint)}
                            onerror={() => broken.add(s.fingerprint)}
                          />
                        {/if}
                      </span>
                    {/each}
                  </span>
                  <span class="qty">{g.count}</span>
                  <span class="cap">{g.count} assets</span>
                </a>
              {/if}
            {:else}
              {@render assetCell(item as PolicyAsset)}
            {/if}
          {/each}
        </div>
      </div>
    {/if}
  </div>
</div>

<style>
  .page {
    /* Mat colour: a shade lifted off the black wall, so a framed tile reads as a
       matted print and `contain` letterboxing blends into the frame (no black bars). */
    --mat-bg: #0e0e11;
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
     while justify-content:center centers the partial last row. --cell/--gap/--mat
     are set inline from the JS geometry so those consts stay the single source. */
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

  /* A matted, framed tile: the artwork sits inset by the mat inside a panel lifted
     off the black wall, with a hairline border and soft shadow — a print on a wall. */
  .frame {
    flex: none;
    width: var(--cell);
    height: var(--cell);
    box-sizing: border-box;
    display: block;
    position: relative;
    border-radius: 10px;
    background: var(--mat-bg);
    border: 1px solid rgb(255 255 255 / 0.07);
    box-shadow:
      0 1px 2px rgb(0 0 0 / 0.6),
      0 6px 18px -8px rgb(0 0 0 / 0.8);
    overflow: hidden;
    transition:
      transform 0.18s ease,
      border-color 0.18s ease,
      box-shadow 0.18s ease;
    -webkit-tap-highlight-color: transparent;
  }

  /* The artwork window inside the mat. Its background is the mat colour so a
     `contain`-letterboxed image blends into the frame instead of showing black bars. */
  .art {
    position: absolute;
    inset: var(--mat);
    border-radius: 4px;
    overflow: hidden;
    background: var(--mat-bg);
    display: block;
  }

  .thumb {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
    object-fit: contain;
    display: block;
    transition: transform 0.25s ease;
  }

  /* Non-image token (image 404'd): the name/fingerprint as a centered placard. */
  .frame.text .art {
    display: flex;
    align-items: center;
    justify-content: center;
    background: radial-gradient(120% 120% at 50% 0%, #17171b, #0b0b0d);
  }
  .cell-text {
    padding: 8px;
    color: var(--text-muted, #9c9c9c);
    font-family: system-ui, sans-serif;
    font-size: 11px;
    text-align: center;
    word-break: break-all;
    overflow: hidden;
  }

  /* Quantity / asset-count chip, tucked in the top-right out of the caption's way. */
  .qty {
    position: absolute;
    top: 8px;
    right: 8px;
    z-index: 2;
    padding: 2px 7px;
    border-radius: 7px;
    background: rgb(0 0 0 / 0.66);
    box-shadow: 0 0 0 1px rgb(255 255 255 / 0.08);
    color: #fff;
    font-family: system-ui, sans-serif;
    font-size: 11px;
    font-variant-numeric: tabular-nums;
    line-height: 1.4;
    max-width: calc(100% - 16px);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  /* The asset name on a caption strip, revealed on hover/focus (a gallery placard). */
  .cap {
    position: absolute;
    left: 0;
    right: 0;
    bottom: 0;
    z-index: 2;
    padding: 18px 10px 8px;
    background: linear-gradient(to top, rgb(0 0 0 / 0.85), rgb(0 0 0 / 0));
    color: #fff;
    font-family: system-ui, sans-serif;
    font-size: 11px;
    letter-spacing: 0.02em;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    pointer-events: none;
    opacity: 0;
    transform: translateY(6px);
    transition:
      opacity 0.18s ease,
      transform 0.18s ease;
  }

  /* Hover/focus: the frame lifts and brightens with a neutral light ring (no subject
     colour), the art zooms slightly, and the name caption slides up. */
  .frame:hover,
  .frame:focus-visible {
    transform: translateY(-3px);
    border-color: rgb(255 255 255 / 0.42);
    box-shadow:
      0 0 0 1px rgb(255 255 255 / 0.22),
      0 12px 28px -10px rgb(0 0 0 / 0.9);
    outline: none;
  }
  .frame:hover .thumb,
  .frame:focus-visible .thumb {
    transform: scale(1.04);
  }
  .frame:hover .cap,
  .frame:focus-visible .cap {
    opacity: 1;
    transform: none;
  }

  /* A multi-asset policy: overlapping cards stepping from top-left (back) to
     bottom-right (front), inside the mat (inherits .art's overflow: hidden). Size is
     set inline: it shrinks with the number of stacked cards so the fixed STACK_STEP
     offset leaves a constant peek for each card behind. */
  .frame.stack .art {
    background: transparent;
  }
  .stack-card {
    position: absolute;
    border-radius: 3px;
    background: #161616;
    /* A mat-coloured matte separates overlapping cards into a visible stack. */
    box-shadow:
      0 0 0 2px var(--mat-bg),
      0 1px 4px rgb(0 0 0 / 0.55);
    overflow: hidden;
  }

  .card-img {
    width: 100%;
    height: 100%;
    object-fit: contain;
    display: block;
  }

  @media (prefers-reduced-motion: reduce) {
    .frame,
    .thumb,
    .cap {
      transition: none;
    }
    .frame:hover,
    .frame:focus-visible {
      transform: none;
    }
    .frame:hover .thumb,
    .frame:focus-visible .thumb {
      transform: none;
    }
    .frame:hover .cap,
    .frame:focus-visible .cap {
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
