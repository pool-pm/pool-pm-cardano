<script lang="ts">
  import { onMount } from 'svelte';
  import NotFound from './NotFound.svelte';

  // Resolve an ADA Handle (from a `/$handle` URL) to its holder's address and redirect
  // there. `name` is the bare handle (no `$`); `rest` is any sub-path to carry over
  // (e.g. `/assets`), '' otherwise. On an unknown handle (404) we show Not Found.
  let { name, rest = '' }: { name: string; rest?: string } = $props();

  let notFound = $state(false);

  onMount(async () => {
    try {
      const res = await fetch(`/api/handle/${encodeURIComponent(name)}`);
      if (res.ok) {
        const { address } = (await res.json()) as { address: string };
        // replace() so the `$handle` URL isn't a back-button trap; the address becomes
        // the canonical entry, matching the "redirect to its address" behavior.
        location.replace(`/${address}${rest}`);
        return;
      }
    } catch {
      /* fall through to Not Found */
    }
    notFound = true;
  });
</script>

{#if notFound}
  <NotFound title="Handle not found" detail={`$${name}`} />
{:else}
  <div class="resolving">Resolving <span class="handle">${name}</span>…</div>
{/if}

<style>
  .resolving {
    position: fixed;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    color: #999;
    font-size: 15px;
  }
  .handle {
    color: #e3e3e3;
    font-weight: 600;
  }
</style>
