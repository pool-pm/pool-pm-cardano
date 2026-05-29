<script lang="ts">
  import { connectSSE, disconnectSSE } from './lib/sse';
  import Feed from './lib/components/Feed.svelte';
  import AssetPage from './lib/components/AssetPage.svelte';
  import PolicyPage from './lib/components/PolicyPage.svelte';
  import SearchBar from './lib/components/SearchBar.svelte';
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

  // Auto-hide the corner chrome (logo + search icon) when idle, reveal on any
  // interaction. The open search bar opts out of hiding (handled in SearchBar).
  let uiVisible = $state(true);
  let hideTimer: ReturnType<typeof setTimeout>;

  function showUiTransiently() {
    uiVisible = true;
    clearTimeout(hideTimer);
    hideTimer = setTimeout(() => (uiVisible = false), 1000);
  }

  // Expose the scrollbar width so right-anchored chrome (search) clears it.
  $effect(() => {
    const probe = document.createElement('div');
    probe.style.cssText = 'position:absolute;top:-9999px;width:100px;height:100px;overflow:scroll';
    document.body.appendChild(probe);
    const sw = probe.offsetWidth - probe.clientWidth;
    probe.remove();
    document.documentElement.style.setProperty('--scrollbar-width', `${sw}px`);
  });

  $effect(() => {
    // Pointer events cover mouse moves (desktop) and touch (mobile); scroll is
    // captured (it doesn't bubble) to catch feed scrolling; keydown the keyboard.
    const events = ['pointermove', 'pointerdown', 'scroll', 'keydown'];
    for (const e of events) {
      window.addEventListener(e, showUiTransiently, { passive: true, capture: true });
    }
    showUiTransiently(); // start the idle countdown
    return () => {
      for (const e of events) {
        window.removeEventListener(e, showUiTransiently, { capture: true });
      }
      clearTimeout(hideTimer);
    };
  });
</script>

<a class="home-logo" class:hidden={!uiVisible} href="/" aria-label="pool.pm home">
  <img src="/pool.pm.svg" alt="pool.pm" />
</a>

<SearchBar visible={uiVisible} />

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
    opacity: 1;
    transition: opacity 0.15s ease; /* fast fade-in on interaction */
  }
  .home-logo.hidden {
    opacity: 0;
    pointer-events: none;
    transition: opacity 1.5s ease; /* slow fade-out when idle */
  }
  .home-logo img {
    width: 48px;
    height: auto;
    display: block;
  }
</style>
