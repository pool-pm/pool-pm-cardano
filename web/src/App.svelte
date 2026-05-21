<script lang="ts">
  import { connectSSE, disconnectSSE } from './lib/sse';
  import Feed from './lib/components/Feed.svelte';
  import './app.css';

  const SSE_BASE = import.meta.env.VITE_SSE_URL || `${window.location.origin}/events`;

  function sseUrl(): string {
    const path = window.location.pathname.replace(/^\/+/, '');
    const base = path ? `${SSE_BASE}/${path}` : SSE_BASE;
    // Negotiate thumbnail resolution: the server picks the power-of-2 nftcdn
    // size rung matching this device's pixel ratio.
    const sep = base.includes('?') ? '&' : '?';
    return `${base}${sep}dpr=${window.devicePixelRatio}`;
  }

  $effect(() => {
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
  <Feed />
</main>
