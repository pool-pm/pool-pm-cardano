<script lang="ts">
  import { onMount } from 'svelte';
  import { fade } from 'svelte/transition';
  import '@nftcdn/media-player/nftcdn-media-player.js';
  import type { AssetMedia, AssetMediaResponse } from '../types';

  // `initialIndex` comes from the URL (`/asset1…/files/N`); 0 is the bare asset page.
  let { fingerprint, initialIndex = 0 }: { fingerprint: string; initialIndex?: number } = $props();

  let media = $state<AssetMedia[]>([]);
  let name = $state<string | null>(null);
  let policy = $state<string | null>(null);
  let quantity = $state<string | null>(null);
  let firstMint = $state<number | null>(null);
  let lastMint = $state<number | null>(null);
  let metadata = $state<Record<string, unknown> | null>(null);
  let loading = $state(true);
  let error = $state<string | null>(null);
  let placardHeight = $state(0); // measured, so the media reserves room above the placard
  let metaHeight = $state(0); // measured, so the media reserves room below the metadata
  let current = $state(0); // which media is on screen (one at a time); onMount applies initialIndex
  // The top-left metadata panel; a tap toggles it (the media reclaims the space). Hidden by
  // default in a small window — narrow (the panel wraps tall) or short (it crowds the media) —
  // and shown otherwise. Keyed on window size, not device type, so a small desktop window
  // behaves like a phone and a roomy tablet keeps it open. The default re-evaluates live on
  // resize / orientation change until the user toggles it, after which their choice sticks.
  const metaSmall = window.matchMedia('(max-width: 720px), (max-height: 600px)');
  let metaOpen = $state(!metaSmall.matches);
  let metaUserSet = false; // set once the user taps — then stop tracking the window size
  $effect(() => {
    function onChange() {
      if (!metaUserSet) metaOpen = !metaSmall.matches;
    }
    metaSmall.addEventListener('change', onChange);
    return () => metaSmall.removeEventListener('change', onChange);
  });

  // Reserved gaps so the media never touches the chrome around it.
  const MEDIA_TOP_GAP = 14; // below the top band (corner icons / metadata)
  const MEDIA_BOTTOM_GAP = 24; // above the bottom-right placard
  const ICONS_BOTTOM = 76; // the pool.pm logo ends ~76px down (top:12 + ~64px tall)
  const META_INSET = 12; // the metadata panel's top/left offset
  const NAV_COOLDOWN_MS = 350; // min time between wheel/swipe steps

  // Drop the skipped keys (case-insensitive); null unless something remains to show.
  function filterMeta(obj: unknown, skip: Set<string>): Record<string, unknown> | null {
    if (!obj || typeof obj !== 'object' || Array.isArray(obj)) return null;
    const out: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(obj)) if (!skip.has(k.toLowerCase())) out[k] = v;
    return Object.keys(out).length ? out : null;
  }

  // Global on-chain metadata (shown with the first media), minus the media-technical
  // keys — the artwork itself stands in for those.
  const META_SKIP = new Set([
    'name',
    'ticker',
    'image',
    'logo',
    'mediatype',
    'files',
    'decimals',
    'src',
    'imagesha256hash',
    'srcsha256hash',
  ]);
  const globalMeta = $derived.by(() => filterMeta(metadata, META_SKIP));

  // The metadata shown with each media: the global metadata for the first, and for the rest
  // each one's `files[]` entry (the server builds media[i] 1:1 from files[i]) minus its
  // media-technical src/mediaType. Null entries show no metadata.
  const FILE_SKIP = new Set(['src', 'mediatype']);
  const mediaMeta = $derived.by(() => {
    const filesVal = metadata?.files;
    const files: unknown[] = Array.isArray(filesVal) ? filesVal : [];
    return media.map((_, i) => (i === 0 ? globalMeta : filterMeta(files[i], FILE_SKIP)));
  });

  function fmtDate(epoch: number): string {
    return new Date(epoch * 1000).toLocaleDateString(undefined, {
      day: 'numeric',
      month: 'short',
      year: 'numeric',
    });
  }

  // A single date, or "first – last" when the asset was minted across several txs.
  const mintLabel = $derived.by(() => {
    if (!firstMint) return null;
    const a = fmtDate(firstMint);
    if (!lastMint || lastMint === firstMint) return a;
    const b = fmtDate(lastMint);
    return a === b ? a : `${a} – ${b}`;
  });

  const quantityLabel = $derived(quantity ? quantity.replace(/\B(?=(\d{3})+(?!\d))/g, ',') : null);
  const policyShort = $derived(policy ? `${policy.slice(0, 6)}…${policy.slice(-4)}` : null);

  // Step between media (clamped); each media is effectively its own page.
  function go(delta: number) {
    const n = media.length;
    if (n) current = Math.max(0, Math.min(n - 1, current + delta));
  }

  // Reflect the current media in the URL (asset1…/files/N; N=0 → the bare asset), without
  // adding history entries.
  $effect(() => {
    if (!media.length) return;
    const url = current === 0 ? `/${fingerprint}` : `/${fingerprint}/files/${current}`;
    history.replaceState(history.state, '', url);
  });

  // Navigate with the wheel, arrow keys, or a vertical swipe. The wheel defers to a
  // scrollable metadata panel until it reaches its edge.
  $effect(() => {
    let lastNav = 0;
    function step(delta: number) {
      const now = performance.now();
      if (now - lastNav < NAV_COOLDOWN_MS) return;
      lastNav = now;
      go(delta);
    }
    function onKey(e: KeyboardEvent) {
      const ae = document.activeElement;
      if (ae && (ae.tagName === 'INPUT' || ae.tagName === 'TEXTAREA' || ae.closest?.('.search'))) return;
      if (e.key === 'ArrowDown' || e.key === 'ArrowRight') step(1);
      else if (e.key === 'ArrowUp' || e.key === 'ArrowLeft') step(-1);
      else return;
      e.preventDefault();
    }
    function onWheel(e: WheelEvent) {
      if (Math.abs(e.deltaY) < 8) return;
      const panel = (e.target as Element | null)?.closest?.('.meta-panel') as HTMLElement | null;
      if (panel && panel.scrollHeight > panel.clientHeight) {
        const atTop = panel.scrollTop <= 0;
        const atBottom = panel.scrollTop + panel.clientHeight >= panel.scrollHeight - 1;
        if ((e.deltaY < 0 && !atTop) || (e.deltaY > 0 && !atBottom)) return; // let the panel scroll
      }
      step(e.deltaY > 0 ? 1 : -1);
    }
    let touchY = 0;
    function onTouchStart(e: TouchEvent) {
      touchY = e.touches[0]?.clientY ?? 0;
    }
    function onTouchEnd(e: TouchEvent) {
      const dy = touchY - (e.changedTouches[0]?.clientY ?? touchY);
      if (Math.abs(dy) > 40) step(dy > 0 ? 1 : -1); // swipe up → next
    }
    window.addEventListener('keydown', onKey);
    window.addEventListener('wheel', onWheel, { passive: true });
    window.addEventListener('touchstart', onTouchStart, { passive: true });
    window.addEventListener('touchend', onTouchEnd, { passive: true });
    return () => {
      window.removeEventListener('keydown', onKey);
      window.removeEventListener('wheel', onWheel);
      window.removeEventListener('touchstart', onTouchStart);
      window.removeEventListener('touchend', onTouchEnd);
    };
  });

  // A click toggles this media's metadata (hidden → the media reclaims the space). Clicks
  // inside the metadata are left alone so its text stays selectable; the logo/search keep
  // their own clicks. Capture phase so the media player can't swallow it.
  $effect(() => {
    function onClick(e: MouseEvent) {
      const t = e.target as Element | null;
      if (t?.closest?.('.home-logo') || t?.closest?.('.search') || t?.closest?.('.meta-panel')) return;
      metaOpen = !metaOpen;
      metaUserSet = true; // manual choice wins from here — stop auto-tracking window size
    }
    document.addEventListener('click', onClick, true);
    return () => document.removeEventListener('click', onClick, true);
  });

  onMount(async () => {
    // Lazy-load the gallery display font — only when an asset page is viewed.
    if (!document.getElementById('gallery-font')) {
      const link = document.createElement('link');
      link.id = 'gallery-font';
      link.rel = 'stylesheet';
      link.href = 'https://fonts.googleapis.com/css2?family=Outfit:wght@300&display=swap';
      document.head.appendChild(link);
    }
    try {
      const res = await fetch(`/api/asset/${fingerprint}`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const data: AssetMediaResponse = await res.json();
      media = data.media;
      name = data.name ?? null;
      policy = data.policy ?? null;
      quantity = data.quantity ?? null;
      firstMint = data.first_mint ?? null;
      lastMint = data.last_mint ?? null;
      metadata = data.metadata ?? null;
      current = Math.max(0, Math.min(initialIndex, media.length - 1)); // clamp a deep-linked index
      document.title = data.name ?? fingerprint;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      loading = false;
    }
  });
</script>

<div class="asset-page">
  {#if loading}
    <div class="status">Loading…</div>
  {:else if error}
    <div class="status">Could not load asset: {error}</div>
  {:else if media.length === 0}
    <div class="status">No media for this asset.</div>
  {:else}
    <!-- One media at a time. It reserves the top band for the icons / metadata and the
         bottom band for the placard, so neither overlaps it. -->
    <div
      class="media-item"
      style:padding-top={`${Math.max(ICONS_BOTTOM, metaOpen && mediaMeta[current] ? META_INSET + metaHeight : 0) + MEDIA_TOP_GAP}px`}
      style:padding-bottom={`${placardHeight + MEDIA_BOTTOM_GAP}px`}
    >
      {#key current}
        <nftcdn-media-player src={media[current].src} type={media[current].type} name={media[current].name}
        ></nftcdn-media-player>
      {/key}
    </div>

    {#if mediaMeta[current] && metaOpen}
      <!-- This media's metadata, top-left. A click outside it hides it (and the media
           expands); a click inside selects text without hiding. -->
      <div class="meta-panel" bind:clientHeight={metaHeight} transition:fade={{ duration: 150 }}>
        {@render jsonNode(mediaMeta[current])}
      </div>
    {/if}

    {#if media.length > 1}
      <div class="pager">{current + 1} / {media.length}</div>
    {/if}
  {/if}

  {#if !loading && !error && (name || policyShort || quantityLabel || mintLabel)}
    <div class="placard" bind:clientHeight={placardHeight} transition:fade={{ duration: 400 }}>
      <dl class="meta">
        {#if quantityLabel}
          <div class="row">
            <dt>quantity</dt>
            <dd>{quantityLabel}</dd>
          </div>
        {/if}
        {#if mintLabel}
          <div class="row">
            <dt>minted</dt>
            <dd>{mintLabel}</dd>
          </div>
        {/if}
        {#if policyShort}
          <div class="row">
            <dt>policy</dt>
            <dd><a href={`/policy/${policy}`} title={policy}>{policyShort}</a></dd>
          </div>
        {/if}
      </dl>
      {#if name}<div class="name">{name}</div>{/if}
    </div>
  {/if}
</div>

<!-- Recursively format a JSON value as label/value rows (objects), comma-joined
     primitives (arrays), or plain text. -->
{#snippet jsonNode(value: unknown)}
  {#if value !== null && typeof value === 'object' && !Array.isArray(value)}
    <dl class="kv">
      {#each Object.entries(value as Record<string, unknown>) as [k, v]}
        <div class="row">
          <dt>{k}</dt>
          <dd>{@render jsonNode(v)}</dd>
        </div>
      {/each}
    </dl>
  {:else if Array.isArray(value)}
    {#if value.every((x) => x === null || typeof x !== 'object')}
      {value.join(', ')}
    {:else}
      <div class="arr">
        {#each value as item}{@render jsonNode(item)}{/each}
      </div>
    {/if}
  {:else}
    {String(value)}
  {/if}
{/snippet}

<style>
  .asset-page {
    width: 100%;
    height: 100%;
    overflow: hidden; /* one media per screen — navigate, don't scroll */
    background: var(--bg);
    position: relative;
  }

  /* The current media fills the window minus the reserved bands (inline padding). */
  .media-item {
    width: 100%;
    height: 100dvh;
    box-sizing: border-box;
    padding-left: var(--asset-margin, 16px);
    padding-right: var(--asset-margin, 16px);
    display: flex;
    align-items: center;
    justify-content: center;
    transition: padding 0.3s ease; /* smooth as the reserved bands change between media */
  }

  nftcdn-media-player {
    width: 100%;
    height: 100%;
    outline: none;
  }

  /* Keep aspect ratio: fit media inside its container. The `outline: none` suppresses the
     browser's native focus ring around the media (it appears after fullscreen/focus). */
  nftcdn-media-player::part(img),
  nftcdn-media-player::part(video),
  nftcdn-media-player::part(iframe),
  nftcdn-media-player::part(object),
  nftcdn-media-player::part(model-viewer) {
    width: 100%;
    height: 100%;
    object-fit: contain;
    outline: none;
  }

  .status {
    display: flex;
    align-items: center;
    justify-content: center;
    height: 100dvh;
    color: var(--text-muted);
    font-family: Inter, sans-serif;
  }

  /* Gallery placard: clean text in the bottom-right corner over the piece. */
  .placard {
    position: fixed;
    /* Clear the feed's vertical scrollbar when present (width measured in App). */
    right: calc(12px + var(--scrollbar-width, 0px));
    bottom: 12px;
    margin: 0;
    z-index: 2;
    display: flex;
    flex-direction: column;
    align-items: flex-end;
    gap: 8px;
    max-width: min(82vw, 460px);
    text-align: right;
    /* Don't intercept media interaction; the link re-enables pointer events. */
    pointer-events: none;
    text-shadow:
      0 1px 6px rgba(0, 0, 0, 0.6),
      0 0 2px rgba(0, 0, 0, 0.45);
  }

  .name {
    font-family: 'Outfit', Inter, sans-serif;
    font-weight: 300;
    font-size: clamp(22px, 3vw, 38px);
    line-height: 1.1;
    letter-spacing: -0.005em;
    color: #fff;
  }

  .meta {
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: 3px;
    font-family: Inter, sans-serif;
    font-size: 12px;
    color: rgba(255, 255, 255, 0.62);
  }

  .meta .row {
    display: flex;
    justify-content: flex-end;
    align-items: baseline;
    gap: 10px;
  }

  .meta dt {
    text-transform: uppercase;
    letter-spacing: 0.09em;
    font-size: 9px;
    color: rgba(255, 255, 255, 0.4);
  }

  .meta dd {
    margin: 0;
    font-variant-numeric: tabular-nums;
  }

  .meta a {
    pointer-events: auto;
    color: inherit;
    text-decoration: none;
  }

  .meta a:hover,
  .meta a:focus-visible {
    text-decoration: underline;
  }

  /* This media's metadata, top-left — full width up to the top-right corner icons. */
  .meta-panel {
    position: fixed;
    left: 12px;
    top: 12px;
    z-index: 2;
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    max-width: calc(100vw - 160px - var(--scrollbar-width, 0px));
    max-height: 62dvh;
    overflow-y: auto;
    font-family: Inter, sans-serif;
    font-size: 12px;
    color: rgba(255, 255, 255, 0.62);
    scrollbar-width: thin;
    /* No background; a text-shadow keeps it legible over the artwork's letterbox. */
    text-shadow:
      0 1px 6px rgba(0, 0, 0, 0.6),
      0 0 2px rgba(0, 0, 0, 0.45);
  }

  /* Page indicator (only with several media), bottom-centre. */
  .pager {
    position: fixed;
    left: 50%;
    bottom: 14px;
    transform: translateX(-50%);
    z-index: 2;
    font-family: Inter, sans-serif;
    font-size: 11px;
    letter-spacing: 0.08em;
    color: rgba(255, 255, 255, 0.5);
    text-shadow: 0 1px 4px rgba(0, 0, 0, 0.6);
    pointer-events: none;
  }

  .kv {
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: 3px;
  }

  .kv .row {
    display: flex;
    gap: 10px;
    align-items: baseline;
  }

  .kv dt {
    flex-shrink: 0;
    text-transform: uppercase;
    letter-spacing: 0.09em;
    font-size: 9px;
    color: rgba(255, 255, 255, 0.4);
  }

  .kv dd {
    margin: 0;
    overflow-wrap: anywhere;
  }

  .kv .arr {
    display: flex;
    flex-direction: column;
    gap: 3px;
  }

  /* Nested objects: a subtle indent + rule to show the hierarchy. */
  .kv dd > .kv {
    margin-top: 2px;
    padding-left: 8px;
    border-left: 1px solid rgba(255, 255, 255, 0.12);
  }
</style>
