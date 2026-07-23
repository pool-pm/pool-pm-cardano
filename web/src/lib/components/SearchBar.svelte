<script lang="ts">
  import { tick } from 'svelte';
  import { searchTarget, isHash, resolveHexTarget, searchSuggestions } from '../search';
  import { poolColor, formatTicker, formatCount, formatAdaCompact } from '../layout';
  import type { SearchResult } from '../types';

  // `visible` is the shared idle-fade state (from App). The closed icon follows
  // it; once the bar is open it stays visible regardless.
  let { visible = true, open = $bindable(false) }: { visible?: boolean; open?: boolean } = $props();

  let query = $state('');
  let results = $state<SearchResult[]>([]);
  let selected = $state(0); // highlighted row; Enter navigates to it
  let inputEl = $state<HTMLInputElement>();
  let containerEl = $state<HTMLElement>();
  let debounceTimer: ReturnType<typeof setTimeout> | undefined;
  let searchGen = 0; // bumped per fetch; a late response with a stale gen is dropped

  // A fresh result set always highlights the first row, so Enter goes there by default
  // without arrowing. (Reads only `results`, so arrow-key moves don't reset it.)
  $effect(() => {
    void results;
    selected = 0;
  });

  function go(id: string) {
    location.href = `/${id}`;
  }

  // True while focus is in a text field, so the global "/" shortcut doesn't steal a
  // slash the user is actually typing (including into our own search input).
  function isEditable(el: EventTarget | null): boolean {
    const n = el as HTMLElement | null;
    return !!n && (n.tagName === 'INPUT' || n.tagName === 'TEXTAREA' || n.tagName === 'SELECT' || n.isContentEditable);
  }

  // Open (and focus) the bar; `seed` pre-fills the first typed character so a query can
  // begin without focusing first. Caret goes to the end so the next keystrokes append.
  function openSearch(seed = ''): void {
    open = true;
    if (seed) query = seed;
    tick().then(() => {
      inputEl?.focus();
      if (seed) inputEl?.setSelectionRange(query.length, query.length);
    });
  }

  // Keyboard entry points, active only when the bar is closed, no modifier is held, and
  // focus isn't already in a text field (so browser/OS shortcuts and in-field typing are
  // untouched):
  //   "/"  — open an empty bar (the conventional site-search shortcut; unlike Ctrl/⌘-K it
  //          isn't bound by the browser).
  //   a supported starting character (letter, digit, or "$" for a handle) — open and seed
  //          it, so you just start typing an address / ticker / handle.
  // Registered once: reads of `open`/`inputEl` happen inside the handler, not as effect deps.
  $effect(() => {
    function onKey(e: KeyboardEvent) {
      if (open || e.ctrlKey || e.metaKey || e.altKey || isEditable(e.target)) return;
      if (e.key === '/') {
        e.preventDefault();
        openSearch();
      } else if (e.key.length === 1 && /[a-z0-9$_]/i.test(e.key)) {
        e.preventDefault();
        openSearch(e.key);
      }
    }
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  });

  // Compact a long bech32 address for the handle-result row (e.g. addr1q8e533…u6aldq).
  function shortAddr(a: string): string {
    return a.length > 22 ? `${a.slice(0, 12)}…${a.slice(-6)}` : a;
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
    // A complete 56-hex hash is a pool hash or a policy id; resolve server-side
    // (pool feed if registered, else the policy page) and navigate.
    if (isHash(query)) {
      const gen = ++searchGen;
      resolveHexTarget(query).then((url) => {
        if (gen === searchGen) location.href = url;
      });
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
      go(results[Math.min(selected, results.length - 1)].id);
    } else if (e.key === 'ArrowDown' && results.length > 0) {
      e.preventDefault();
      selected = Math.min(selected + 1, results.length - 1);
    } else if (e.key === 'ArrowUp' && results.length > 0) {
      e.preventDefault();
      selected = Math.max(selected - 1, 0);
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
      {#each results as r, i (`${r.kind}:${r.id}:${r.label}`)}
        <a class="result" class:selected={i === selected} href={`/${r.id}`} onmouseenter={() => (selected = i)}>
          <span class="kind">{r.kind.toUpperCase()}</span>
          <span class="name" style:color={poolColor(r.id)}>
            {r.kind === 'pool' ? formatTicker(r.label) : r.kind === 'handle' ? `$${r.label}` : r.label}
          </span>
          {#if r.kind === 'handle'}
            <span class="col addr">{shortAddr(r.id)}</span>
          {:else}
            <span class="col deleg">{formatCount(r.delegators ?? 0)}&nbsp;deleg</span>
            <span class="col stake">{formatAdaCompact(r.live_stake ?? '0')}</span>
          {/if}
        </a>
      {/each}
    </div>
  {/if}
</div>

<style>
  .search {
    position: fixed;
    top: 12px;
    /* Sit just left of the pool.pm logo (48px) in the top-right, clearing the feed's
       vertical scrollbar (measured in App): 12 margin + 48 logo + 8 gap = 68. */
    right: calc(68px + var(--scrollbar-width, 0px));
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
    display: flex;
    align-items: baseline;
    gap: 8px;
    padding: 10px 14px;
    text-decoration: none;
  }
  .result:hover,
  .result.selected {
    background: rgb(255 255 255 / 0.08);
  }
  /* Kind label first; fixed width so names line up. */
  .kind {
    flex-shrink: 0;
    align-self: center;
    font-size: 10px;
    font-weight: 700;
    letter-spacing: 0.5px;
    color: #bbb;
    background: rgb(255 255 255 / 0.1);
    padding: 2px 5px;
    border-radius: 4px;
  }
  /* Ticker/name takes the slack and truncates so the row never overflows on mobile. */
  .name {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-size: 15px;
    font-weight: 600;
  }
  /* Fixed-width, right-aligned columns so delegators and stake line up across rows. */
  .col {
    flex-shrink: 0;
    font-size: 12px;
    color: #999;
    white-space: nowrap;
    text-align: right;
  }
  .deleg {
    width: 70px;
  }
  .stake {
    width: 64px;
  }
  /* Handle row: show the destination address (where the row links) in monospace. */
  .addr {
    width: 134px;
    font-size: 11px;
    font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
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
    /* Extend to the top-left margin: viewport minus the left margin (12) + this bar's
       icon (48) + the right side now taken by the logo (12 + 48 + 8 gap = 68) + scrollbar. */
    width: calc(100vw - 128px - var(--scrollbar-width, 0px));
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
