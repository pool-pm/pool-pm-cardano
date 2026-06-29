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
  let loading = $state(true);
  let error = $state<string | null>(null);

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
      link.href = 'https://fonts.googleapis.com/css2?family=Fraunces:opsz,wght@9..144,300..500&display=swap';
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
      <div class="media-item">
        <nftcdn-media-player src={m.src} type={m.type} name={m.name}></nftcdn-media-player>
      </div>
    {/each}
  {/if}

  {#if !loading && !error && (name || policyShort || quantityLabel || mintLabel)}
    <div class="placard" transition:fade={{ duration: 400 }}>
      {#if name}<div class="name">{name}</div>{/if}
      <dl class="meta">
        {#if policyShort}
          <div class="row">
            <dt>policy</dt>
            <dd><a href={`/policy/${policy}`} title={policy}>{policyShort}</a></dd>
          </div>
        {/if}
        {#if mintLabel}
          <div class="row">
            <dt>minted</dt>
            <dd>{mintLabel}</dd>
          </div>
        {/if}
        {#if quantityLabel}
          <div class="row">
            <dt>quantity</dt>
            <dd>{quantityLabel}</dd>
          </div>
        {/if}
      </dl>
    </div>
  {/if}
</div>

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
    padding: var(--asset-margin, 16px);
    display: flex;
    align-items: center;
    justify-content: center;
  }

  nftcdn-media-player {
    width: 100%;
    height: 100%;
  }

  /* Keep aspect ratio: fit media inside its full-window container. */
  nftcdn-media-player::part(img),
  nftcdn-media-player::part(video),
  nftcdn-media-player::part(iframe),
  nftcdn-media-player::part(object),
  nftcdn-media-player::part(model-viewer) {
    width: 100%;
    height: 100%;
    object-fit: contain;
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
    right: clamp(16px, 3.5vw, 48px);
    bottom: clamp(16px, 3.5vw, 48px);
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
    font-family: 'Fraunces', Inter, serif;
    font-optical-sizing: auto;
    font-weight: 400;
    font-size: clamp(22px, 3vw, 38px);
    line-height: 1.1;
    letter-spacing: -0.01em;
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
    color: rgba(255, 255, 255, 0.85);
    font-family: ui-monospace, 'SF Mono', monospace;
    text-decoration: none;
  }

  .meta a:hover {
    color: #fff;
    text-decoration: underline;
  }
</style>
