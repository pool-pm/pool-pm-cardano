<script lang="ts">
  import { tick } from 'svelte';

  // `visible` is the shared idle-fade state (from App). The closed icon follows
  // it; once the bar is open it stays visible regardless.
  let { visible = true }: { visible?: boolean } = $props();

  let open = $state(false);
  let query = $state('');
  let inputEl = $state<HTMLInputElement>();
  let containerEl = $state<HTMLElement>();

  async function onIconClick() {
    if (!open) {
      open = true;
      await tick();
      inputEl?.focus();
    } else if (query.trim() === '') {
      open = false;
      inputEl?.blur();
    } else {
      // Non-empty: confirm the search — null action for now.
    }
  }

  // While open, a pointer down outside the bar closes it.
  $effect(() => {
    if (!open) return;
    function onDocPointerDown(e: PointerEvent) {
      if (containerEl && !containerEl.contains(e.target as Node)) {
        open = false;
      }
    }
    document.addEventListener('pointerdown', onDocPointerDown, true);
    return () => document.removeEventListener('pointerdown', onDocPointerDown, true);
  });
</script>

<div class="search" class:open class:hidden={!visible && !open} bind:this={containerEl}>
  <input
    bind:this={inputEl}
    bind:value={query}
    class="search-input"
    type="text"
    placeholder="Search…"
    tabindex={open ? 0 : -1}
  />
  <button class="search-icon" type="button" onclick={onIconClick} aria-label="Search">
    <img src="/search.svg" alt="" />
  </button>
</div>

<style>
  .search {
    position: fixed;
    top: 12px;
    /* Sit left of the feed's vertical scrollbar (width measured in App). */
    right: calc(12px + var(--scrollbar-width, 0px));
    z-index: 100;
    display: flex;
    align-items: center;
    height: 48px;
    border-radius: 24px;
    overflow: hidden;
    background: transparent;
    opacity: 1;
    transition:
      opacity 0.15s ease,
      background 0.2s ease;
  }
  .search.open {
    background: #2a2a2a;
  }
  /* Idle fade — only when closed (the open bar never hides). */
  .search.hidden {
    opacity: 0;
    pointer-events: none;
    transition: opacity 1.5s ease;
  }

  .search-input {
    width: 0;
    border: none;
    outline: none;
    background: transparent;
    color: #e3e3e3;
    font: inherit;
    font-size: 15px;
    padding: 0;
    transition:
      width 0.25s ease,
      padding 0.25s ease;
  }
  .search.open .search-input {
    /* Extend left to just past the logo: viewport minus logo (12 + 48) + gap (12)
       + this bar's icon (48) + right margin (12) + scrollbar. */
    width: calc(100vw - 132px - var(--scrollbar-width, 0px));
    padding: 0 8px 0 16px;
  }

  .search-icon {
    flex-shrink: 0;
    width: 48px;
    height: 48px;
    border: none;
    border-radius: 50%;
    background: #9c27b0;
    cursor: pointer;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 0;
  }
  .search-icon img {
    width: 24px;
    height: 24px;
    display: block;
  }
</style>
