<script lang="ts">
  import { connectSSE, disconnectSSE } from './lib/sse';
  import Feed from './lib/components/Feed.svelte';
  import AssetPage from './lib/components/AssetPage.svelte';
  import PolicyPage from './lib/components/PolicyPage.svelte';
  import './app.css';

  const SSE_BASE = import.meta.env.VITE_SSE_URL || `${window.location.origin}/events`;

  const path = window.location.pathname.replace(/^\/+/, '');
  // A bare CIP-14 fingerprint path renders the standalone asset page;
  // `/policy/<28-byte hex>` renders the policy asset grid; everything else is a
  // feed (root, pool id, or drep id).
  const assetFingerprint = /^asset1[a-z0-9]+$/.test(path) ? path : null;
  const policyId = /^policy\/([0-9a-f]{56})$/.exec(path)?.[1] ?? null;

  function sseUrl(): string {
    const base = path ? `${SSE_BASE}/${path}` : SSE_BASE;
    // Negotiate thumbnail resolution: the server picks the power-of-2 nftcdn
    // size rung matching this device's pixel ratio.
    const sep = base.includes('?') ? '&' : '?';
    return `${base}${sep}dpr=${window.devicePixelRatio}`;
  }

  $effect(() => {
    // The asset and policy pages are stateless HTTP views — no SSE connection.
    if (assetFingerprint || policyId) return;

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

<a class="home-logo" href="/" aria-label="pool.pm home">
  <img src="/pool.pm.svg" alt="pool.pm" />
</a>

<main>
  {#if assetFingerprint}
    <AssetPage fingerprint={assetFingerprint} />
  {:else if policyId}
    <PolicyPage {policyId} />
  {:else}
    <Feed />
  {/if}
</main>

<style>
  .home-logo {
    position: fixed;
    top: 12px;
    left: 12px;
    z-index: 100;
    display: block;
  }
  .home-logo img {
    height: 64px;
    width: auto;
    display: block;
  }
</style>
