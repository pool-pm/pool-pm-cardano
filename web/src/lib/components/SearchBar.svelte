<script lang="ts">
  import { tick } from 'svelte';
  import { sanitizeQuery, searchTarget } from '../search';

  // `visible` is the shared idle-fade state (from App). The closed icon follows
  // it; once the bar is open it stays visible regardless.
  let { visible = true, open = $bindable(false) }: { visible?: boolean; open?: boolean } = $props();

  let query = $state('');
  let inputEl = $state<HTMLInputElement>();
  let containerEl = $state<HTMLElement>();

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
      // Non-empty: confirm the search — null action for now.
      btn.blur();
    }
  }

  // Filter to the allowed charset; navigate as soon as the field holds a complete
  // address (see searchTarget).
  function onInput(e: Event) {
    const el = e.currentTarget as HTMLInputElement;
    const clean = sanitizeQuery(el.value);
    query = clean;
    if (el.value !== clean) el.value = clean;
    const target = searchTarget(clean);
    if (target) location.href = target;
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
    value={query}
    oninput={onInput}
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
    box-shadow: 0 2px 12px rgb(0 0 0 / 0.6); /* float above the feed */
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
