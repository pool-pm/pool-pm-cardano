<script lang="ts">
  import { connectSSE, disconnectSSE } from './lib/sse';
  import Feed from './lib/components/Feed.svelte';
  import './app.css';

  const SSE_URL = import.meta.env.VITE_SSE_URL || 'http://localhost:3000/events';

  $effect(() => {
    connectSSE(SSE_URL);
    return () => disconnectSSE();
  });
</script>

<main>
  <Feed />
</main>
