<script lang="ts">
  import { onMount } from 'svelte';
  import '@nftcdn/media-player/nftcdn-media-player.js';
  import type { AssetMedia, AssetMediaResponse } from '../types';

  let { fingerprint }: { fingerprint: string } = $props();

  let media = $state<AssetMedia[]>([]);
  let loading = $state(true);
  let error = $state<string | null>(null);

  onMount(async () => {
    try {
      const res = await fetch(`/api/asset/${fingerprint}`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const data: AssetMediaResponse = await res.json();
      media = data.media;
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
</div>

<style>
  .asset-page {
    width: 100%;
    height: 100%;
    overflow-y: auto;
    background: var(--bg);
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
    font-family: system-ui, sans-serif;
  }
</style>
