<script lang="ts">
  import { onMount } from 'svelte';
  import { SvelteSet } from 'svelte/reactivity';
  import type { PolicyAsset, AssetGroup, AssetsResponse, GroupsResponse, AssetDelta } from '../types';
  import { stake, address } from '../stores';
  import { onAssetLive } from '../sse';
  import { commonNamePrefix } from '../assetName';
  import { formatQuantity } from '../layout';
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
  const TILE_TARGET = 168; // desired tile *width* (px); actual CELL flexes around it
  const GAP = 16;
  // Each tile is one container holding, top to bottom: a quantity band, the square
  // media, and a name band — so the text never sits on top of the artwork. The media
  // is CELL tall, giving a container CELL + QTY_H + NAME_H tall.
  // Bands sized to hold EDGE (the symmetric gap to the container edge, in CSS) plus the
  // text: one line for the quantity, up to two for the name.
  const QTY_H = 24; // top quantity band
  const NAME_H = 40; // bottom name band (fits up to two lines)
  // Mat inset: the margin the stacked-card thumbnail keeps from the media-area edges.
  const MAT = 10;
  // Fixed offset between stacked cards: each card behind peeks by exactly this much
  // regardless of how many are stacked (the card *size* shrinks to fit instead). Front
  // card fills the inner art box, so card size = (CELL - 2*MAT) - (n-1)*STACK_STEP.
  const STACK_STEP = 12;
  const GROUP_SAMPLES = 5; // max sample cards in a stack — must match the server
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
  // CELL is the (square) media width/height; the container adds the two text bands.
  const CELL = $derived(containerW > 0 ? Math.floor((containerW - (cols - 1) * GAP) / cols) : TILE_TARGET);
  const TILE_H = $derived(CELL + QTY_H + NAME_H);
  const ROW = $derived(TILE_H + GAP);
  // The square the stacked-card thumbnail fills inside the media area.
  const artBox = $derived(CELL - 2 * MAT);
  // Exact width of one full row of `cols` cells. Pinning the flex container to
  // this (rather than the full container width) makes it wrap at exactly `cols`
  // per row — matching the slice math deterministically, instead of letting
  // sub-pixel rounding drift to cols-1 and unmount on-screen rows (black gaps).
  const rowWidth = $derived(cols * CELL + (cols - 1) * GAP);
  const loadedRows = $derived(Math.ceil(items.length / cols));
  // Content height: rows are ROW apart, the last row adds only its tile height;
  // VPAD is reserved above the first row and below the last.
  const totalHeight = $derived(loadedRows > 0 ? (loadedRows - 1) * ROW + TILE_H : 0);
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
    // Lazy-load the display font (Outfit) used for the asset name — the same face the
    // single-asset page uses; shared #gallery-font id so it's injected at most once.
    if (!document.getElementById('gallery-font')) {
      const link = document.createElement('link');
      link.id = 'gallery-font';
      link.rel = 'stylesheet';
      link.href = 'https://fonts.googleapis.com/css2?family=Outfit:wght@300&display=swap';
      document.head.appendChild(link);
    }
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

<!-- One rectangular asset tile (also used for a single-asset policy group): a matted
     square media frame with the asset name on a plate below it. -->
{#snippet assetCell(a: PolicyAsset)}
  {@const label = a.name ?? a.fingerprint}
  {@const isText = mode === 'text-fallback' && broken.has(a.fingerprint)}
  <a class="tile" href={'/' + a.fingerprint} aria-label={label} title={label}>
    <span class="frame" class:text={isText}>
      <!-- Owned amount (decimals-applied); the server omits it when it's 1. The band is
           always present (blank for single NFTs) so the media lines up across tiles. -->
      <span class="qty">{a.quantity ? formatQuantity(a.quantity) : ''}</span>
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
      <span class="name"><span class="name-text">{label}</span></span>
    </span>
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
          style="transform:translateY({offsetY}px); width:{rowWidth}px; --cell:{CELL}px; --tile-h:{TILE_H}px; --qty-h:{QTY_H}px; --name-h:{NAME_H}px; --gap:{GAP}px"
        >
          {#each slice as item (grouped ? (item as AssetGroup).policy : (item as PolicyAsset).fingerprint)}
            {#if grouped}
              {@const g = item as AssetGroup}
              {#if g.count <= 1}
                {@render assetCell(g.samples[0])}
              {:else}
                {@const n = g.samples.length}
                {@const card = artBox - (n - 1) * STACK_STEP}
                <!-- A multi-asset policy: stacked sample cards centered in the media area,
                     the held count on top (like a single asset's quantity) and a collection
                     label — the samples' common name prefix — below. Drills into the policy. -->
                {@const stackLabel = commonNamePrefix(g.samples.map((s) => s.name))}
                {@const stackTitle = stackLabel ? `${g.count} × ${stackLabel}` : `${g.count} assets`}
                <a class="tile" href={`/${subject}/assets/${g.policy}`} aria-label={stackTitle} title={stackTitle}>
                  <span class="frame stack">
                    <span class="qty">{g.count.toLocaleString()}</span>
                    <span class="art">
                      <span class="stackbox" style="width:{artBox}px; height:{artBox}px">
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
                    </span>
                    <span class="name"><span class="name-text">{stackLabel}</span></span>
                  </span>
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
    /* Matte behind the stacked-card thumbnails. */
    --mat-bg: #0e0e11;
    /* The area behind the cards is lifted a hair above pure black so each card's soft
       bottom shadow is actually visible (a black shadow on #000 shows nothing). */
    --surface: #09090b;
    display: flex;
    flex-direction: column;
    height: 100dvh;
    /* Top breathing room for the header card, matching the feed's 16px top padding.
       The card centers itself (margin-inline auto) and clears the corner chrome. */
    padding-top: 16px;
    box-sizing: border-box;
    background: var(--surface);
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
    background: var(--surface);
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

  /* A rectangular tile: a square media frame with a name plate below it. */
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

  /* The container: a panel lifted off the black wall (hairline border + soft shadow)
     stacking the quantity band, the media, and the name band top-to-bottom. */
  .frame {
    display: flex;
    flex-direction: column;
    width: 100%;
    height: var(--tile-h);
    box-sizing: border-box;
    position: relative;
    /* A dark *stretched* canvas (gallery wrap) lit from above — no frame border: the
       depth is carried by a light catch on the top edge, the underside in shadow on the
       bottom, a slight lighter-top → darker-bottom gradient, and a soft drop shadow so
       the panel sits proud of the wall. Kept subtle. */
    border-radius: 0;
    background: linear-gradient(180deg, #17171c 0%, #0c0c0f 100%);
    border: none;
    box-shadow:
      0 6px 14px -5px rgb(0 0 0 / 0.6),
      inset 0 1px 0 rgb(255 255 255 / 0.08),
      inset 0 -2px 3px -1px rgb(0 0 0 / 0.45);
    overflow: hidden;
    transition:
      transform 0.18s ease,
      border-color 0.18s ease,
      box-shadow 0.18s ease;
  }

  /* Owned amount above the media — no background, half opacity. The band is always
     present (blank for single NFTs) so the media lines up across tiles. EDGE (8px) is
     the gap to the top edge; the name uses the same gap to the bottom edge. */
  .qty {
    height: var(--qty-h);
    box-sizing: border-box;
    padding: 8px 8px 0;
    width: 100%;
    text-align: center;
    color: rgb(255 255 255 / 0.5);
    /* Inter (the app font) with tabular, slashed-zero figures — even-width digits for
       the numeric quantity. */
    font-family: Inter, sans-serif;
    font-size: 11px;
    line-height: 1.3;
    font-variant-numeric: tabular-nums slashed-zero;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  /* The media area between the two bands; the artwork is contained within it. */
  .art {
    flex: 1;
    min-height: 0;
    width: 100%;
    position: relative;
    display: flex;
    align-items: center;
    justify-content: center;
    overflow: hidden;
  }

  .thumb {
    max-width: 100%;
    max-height: 100%;
    object-fit: contain;
    display: block;
    transition: transform 0.25s ease;
  }

  /* Non-image token (image 404'd): the name/fingerprint stands in for the missing art. */
  .frame.text .art {
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

  /* Asset name below the media, inside the container; bottom-aligned with the same 8px
     edge gap as the quantity. Up to two lines, then ellipsis. */
  .name {
    height: var(--name-h);
    box-sizing: border-box;
    width: 100%;
    padding: 0 8px 8px;
    display: flex;
    align-items: flex-end;
    justify-content: center;
  }
  .name-text {
    text-align: center;
    /* Same grey as the quantity, in the Outfit display font. */
    color: rgb(255 255 255 / 0.5);
    font-family: 'Outfit', Inter, sans-serif;
    font-weight: 300;
    font-size: 13px;
    line-height: 1.25;
    display: -webkit-box;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    -webkit-box-orient: vertical;
    overflow: hidden;
  }

  /* Hover/focus: the container lifts and brightens with a neutral light ring (no subject
     colour) and the art zooms slightly. */
  .tile:hover .frame,
  .tile:focus-visible .frame {
    transform: translateY(-3px);
    box-shadow:
      0 12px 26px -7px rgb(0 0 0 / 0.7),
      inset 0 1px 0 rgb(255 255 255 / 0.11),
      inset 0 -2px 3px -1px rgb(0 0 0 / 0.5);
  }
  .tile:hover .thumb,
  .tile:focus-visible .thumb {
    transform: scale(1.04);
  }

  /* A multi-asset policy: a centered square holding overlapping sample cards stepping
     top-left → bottom-right; size set inline so a constant peek shows per card. */
  .stackbox {
    position: relative;
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
    .thumb {
      transition: none;
    }
    .tile:hover .frame,
    .tile:focus-visible .frame {
      transform: none;
    }
    .tile:hover .thumb,
    .tile:focus-visible .thumb {
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
