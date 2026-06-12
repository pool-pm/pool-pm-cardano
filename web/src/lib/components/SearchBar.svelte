<script lang="ts">
  import { tick } from 'svelte';
  import { searchTarget, searchSuggestions } from '../search';
  import { poolColor, formatTicker } from '../layout';
  import type { SearchResult } from '../types';

  // `visible` is the shared idle-fade state (from App). The closed icon follows
  // it; once the bar is open it stays visible regardless.
  let { visible = true, open = $bindable(false) }: { visible?: boolean; open?: boolean } = $props();

  let query = $state('');
  let results = $state<SearchResult[]>([]);
  let inputEl = $state<HTMLInputElement>();
  let containerEl = $state<HTMLElement>();
  let debounceTimer: ReturnType<typeof setTimeout> | undefined;
  let searchGen = 0; // bumped per fetch; a late response with a stale gen is dropped

  function go(id: string) {
    location.href = `/${id}`;
  }

  async function onIconClick(e: MouseEvent) {
    // Don't let the button keep focus after its action — otherwise an orientation
    // reflow can re-show its focus ring.
    const btn = e.currentTarget as HTMLButtonElement;
    if (!open) {
      open = true;
      await tick();
      inputEl?.focus();
    } else if (query.trim() === '') {
      open = false;
      btn.blur();
    } else {
      btn.blur();
    }
  }

  // A complete address navigates instantly (see searchTarget); otherwise debounce a
  // fuzzy pool/DRep lookup into the dropdown.
  function onInput(e: Event) {
    query = (e.currentTarget as HTMLInputElement).value;
    const target = searchTarget(query);
    if (target) {
      location.href = target;
      return;
    }
    clearTimeout(debounceTimer);
    const q = query;
    if (q.trim().length < 2) {
      results = [];
      return;
    }
    debounceTimer = setTimeout(async () => {
      const gen = ++searchGen;
      const r = await searchSuggestions(q);
      if (gen === searchGen) results = r;
    }, 150);
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.key === 'Enter' && results.length > 0) {
      go(results[0].id);
    } else if (e.key === 'Escape') {
      open = false;
    }
  }

  // While open, a pointer down outside the bar closes it.
  $effect(() => {
    if (!open) {
      results = [];
      return;
    }
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
  <div class="pill">
    <input
      bind:this={inputEl}
      value={query}
      oninput={onInput}
      onkeydown={onKeydown}
      class="search-input"
      type="text"
      placeholder="Search…"
      tabindex={open ? 0 : -1}
    />
    <button class="search-icon" type="button" onclick={onIconClick} aria-label="Search">
      <img src="/search.svg" alt="" />
    </button>
  </div>
  {#if open && results.length > 0}
    <div class="results">
      {#each results as r (r.id)}
        <a class="result" href={`/${r.id}`} style:color={poolColor(r.id)}>
          {r.kind === 'pool' ? formatTicker(r.label) : r.label}
        </a>
      {/each}
    </div>
  {/if}
</div>

<style>
  .search {
    position: fixed;
    top: 12px;
    /* Sit left of the feed's vertical scrollbar (width measured in App). */
    right: calc(12px + var(--scrollbar-width, 0px));
    z-index: 100;
    opacity: 1;
    transition: opacity 0.15s ease;
  }
  /* Idle fade — only when closed (the open bar never hides). */
  .search.hidden {
    opacity: 0;
    pointer-events: none;
    transition: opacity 1.5s ease;
  }

  /* The rounded pill clips the input's width animation; the results dropdown lives
     outside it so it isn't clipped. */
  .pill {
    display: flex;
    align-items: center;
    height: 48px;
    border-radius: 24px;
    overflow: hidden;
    background: transparent;
    box-shadow: 0 2px 12px rgb(0 0 0 / 0.6); /* float above the feed */
    transition: background 0.2s ease;
  }
  .search.open .pill {
    background: #2a2a2a;
  }

  .results {
    position: absolute;
    top: 52px;
    left: 0;
    right: 0;
    background: #2a2a2a;
    border-radius: 12px;
    box-shadow: 0 4px 16px rgb(0 0 0 / 0.6);
    overflow: hidden auto;
    max-height: 60vh;
    display: flex;
    flex-direction: column;
  }
  .result {
    padding: 10px 16px;
    font-size: 15px;
    font-weight: 600;
    text-decoration: none;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .result:hover {
    background: rgb(255 255 255 / 0.08);
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
    /* Extend to the top-left margin: viewport minus left margin (12) + this bar's
       icon (48) + right margin (12) + scrollbar. */
    width: calc(100vw - 72px - var(--scrollbar-width, 0px));
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
    -webkit-tap-highlight-color: transparent;
  }
  /* No focus ring for pointer/programmatic focus (e.g. after a click or an
     orientation reflow); keep it for keyboard navigation. */
  .search-icon:focus:not(:focus-visible) {
    outline: none;
  }
  .search-icon img {
    width: 32px;
    height: 32px;
    display: block;
  }
</style>
