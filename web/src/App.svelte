<script lang="ts">
  import { connectSSE, disconnectSSE } from './lib/sse';
  import Feed from './lib/components/Feed.svelte';
  import './app.css';

  const SSE_BASE = import.meta.env.VITE_SSE_URL || `${window.location.origin}/events`;

  function sseUrl(): string {
    const path = window.location.pathname.replace(/^\/+/, '');
    return path ? `${SSE_BASE}/${path}` : SSE_BASE;
  }

  $effect(() => {
    const url = sseUrl();
    connectSSE(url);

    // Reload page when returning from background to get a clean state.
    function onVisibilityChange() {
      if (document.visibilityState === 'visible') {
        location.reload();
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
