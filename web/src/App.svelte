<script lang="ts">
  import { connectSSE, disconnectSSE } from './lib/sse';
  import Feed from './lib/components/Feed.svelte';
  import AssetPage from './lib/components/AssetPage.svelte';
  import './app.css';

  const SSE_BASE = import.meta.env.VITE_SSE_URL || `${window.location.origin}/events`;

  const path = window.location.pathname.replace(/^\/+/, '');
  // A bare CIP-14 fingerprint path renders the standalone asset page; everything
  // else is a feed (root, pool id, or drep id).
  const assetFingerprint = /^asset1[a-z0-9]+$/.test(path) ? path : null;

  function sseUrl(): string {
    const base = path ? `${SSE_BASE}/${path}` : SSE_BASE;
    // Negotiate thumbnail resolution: the server picks the power-of-2 nftcdn
    // size rung matching this device's pixel ratio.
    const sep = base.includes('?') ? '&' : '?';
    return `${base}${sep}dpr=${window.devicePixelRatio}`;
  }

  $effect(() => {
    if (assetFingerprint) return;

    const url = sseUrl();
    connectSSE(url);

    // Disconnect SSE when backgrounded to prevent event accumulation,
    // reload on return for a clean state.
    function onVisibilityChange() {
      if (document.visibilityState === 'visible') {
        location.reload();
      } else {
        disconnectSSE();
      }
    }
    document.addEventListener('visibilitychange', onVisibilityChange);

    return () => {
      document.removeEventListener('visibilitychange', onVisibilityChange);
      disconnectSSE();
    };
  });
</script>

<main>
  {#if assetFingerprint}
    <AssetPage fingerprint={assetFingerprint} />
  {:else}
    <Feed />
  {/if}
</main>
