<script lang="ts">
  import { onMount } from 'svelte';
  import { fade } from 'svelte/transition';
  import '@nftcdn/media-player/nftcdn-media-player.js';
  import type { AssetMedia, AssetMediaResponse } from '../types';

  let { fingerprint }: { fingerprint: string } = $props();

  let media = $state<AssetMedia[]>([]);
  let name = $state<string | null>(null);
  let policy = $state<string | null>(null);
  let quantity = $state<string | null>(null);
  let firstMint = $state<number | null>(null);
  let lastMint = $state<number | null>(null);
  let metadata = $state<Record<string, unknown> | null>(null);
  let loading = $state(true);
  let error = $state<string | null>(null);
  let metaOpen = $state(true); // bottom-left metadata panel; collapses to an (i) button
  let placardHeight = $state(0); // measured, so the media reserves room above it

  // The on-chain metadata to display, minus the media-technical keys (the artwork
  // itself stands in for those).
  const META_SKIP = new Set(['name', 'image', 'logo', 'mediatype', 'files']);
  const metaShown = $derived.by(() => {
    const m = metadata;
    if (!m || typeof m !== 'object' || Array.isArray(m)) return null;
    const out: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(m)) if (!META_SKIP.has(k.toLowerCase())) out[k] = v;
    return Object.keys(out).length ? out : null;
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
    {#each media as m (m.src)}
      <div class="media-item" style:padding-bottom={placardHeight ? `${placardHeight + 24}px` : undefined}>
        <nftcdn-media-player src={m.src} type={m.type} name={m.name}></nftcdn-media-player>
      </div>
    {/each}
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

  {#if !loading && !error && metaShown}
    {#if metaOpen}
      <div class="meta-panel" transition:fade={{ duration: 200 }}>
        <button class="meta-close" type="button" onclick={() => (metaOpen = false)} aria-label="Hide metadata">✕</button
        >
        {@render jsonNode(metaShown)}
      </div>
    {:else}
      <button
        class="meta-info"
        type="button"
        onclick={() => (metaOpen = true)}
        aria-label="Show metadata"
        transition:fade={{ duration: 200 }}>i</button
      >
    {/if}
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
    overflow-y: auto;
    background: var(--bg);
    position: relative;
  }

  /* Each media gets the full window, minus a uniform margin. */
  .media-item {
    width: 100%;
    height: 100dvh;
    box-sizing: border-box;
    /* The media zone sits between the top corner icons (48px @ 12px) and the
       bottom-right placard (its measured height is reserved inline), with a small
       side margin. The bottom-left metadata may overlay this. */
    padding: 72px var(--asset-margin, 16px) var(--asset-margin, 16px);
    display: flex;
    align-items: center;
    justify-content: center;
  }

  nftcdn-media-player {
    width: 100%;
    height: 100%;
    outline: none;
  }

  /* Keep aspect ratio: fit media inside its full-window container. The `outline:
     none` suppresses the browser's native focus ring around the media (it appears
     after the element is fullscreened/focused). */
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
    right: 12px;
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

  /* Bottom-left: the on-chain metadata, formatted — same type/treatment as the
     right placard but left-aligned and scrollable. */
  .meta-panel {
    position: fixed;
    left: 12px;
    bottom: 12px;
    z-index: 2;
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 5px;
    max-width: min(46vw, 400px);
    max-height: 62dvh;
    overflow-y: auto;
    font-family: Inter, sans-serif;
    font-size: 12px;
    color: rgba(255, 255, 255, 0.62);
    scrollbar-width: thin;
    text-shadow:
      0 1px 6px rgba(0, 0, 0, 0.6),
      0 0 2px rgba(0, 0, 0, 0.45);
  }

  /* Close (×) in the panel's top-right; (i) button when the panel is hidden. */
  .meta-close,
  .meta-info {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    pointer-events: auto;
    cursor: pointer;
    background: rgba(0, 0, 0, 0.3);
    color: rgba(255, 255, 255, 0.7);
    font-family: Inter, sans-serif;
    line-height: 1;
    padding: 0;
    transition: color 0.15s ease;
  }

  .meta-close {
    align-self: flex-end;
    width: 20px;
    height: 20px;
    border: none;
    border-radius: 4px;
    font-size: 13px;
  }

  .meta-info {
    position: fixed;
    left: 12px;
    bottom: 12px;
    z-index: 2;
    width: 26px;
    height: 26px;
    border: 1px solid rgba(255, 255, 255, 0.35);
    border-radius: 50%;
    font-size: 14px;
    font-style: italic;
    text-shadow: 0 1px 4px rgba(0, 0, 0, 0.6);
  }

  .meta-close:hover,
  .meta-info:hover {
    color: #fff;
    border-color: rgba(255, 255, 255, 0.7);
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
