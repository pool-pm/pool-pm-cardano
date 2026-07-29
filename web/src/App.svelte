<script lang="ts">
  import { connectSSE, disconnectSSE } from './lib/sse';
  import { isFeedPath } from './lib/search';
  import Feed from './lib/components/Feed.svelte';
  import AssetPage from './lib/components/AssetPage.svelte';
  import AssetsGrid from './lib/components/AssetsGrid.svelte';
  import DelegatorsGrid from './lib/components/DelegatorsGrid.svelte';
  import SearchBar from './lib/components/SearchBar.svelte';
  import HandleRedirect from './lib/components/HandleRedirect.svelte';
  import NotFound from './lib/components/NotFound.svelte';
  import './app.css';

  const SSE_BASE = import.meta.env.VITE_SSE_URL || `${window.location.origin}/events`;

  // Strip leading AND trailing slashes: a trailing slash would otherwise flow into the SSE
  // URL (`/events/<feed>/`), miss the axum `/events/{feed}` route, and fall through to the
  // crawler `og_page` — so the EventSource gets HTML instead of a stream and the feed dies.
  const path = window.location.pathname.replace(/^\/+/, '').replace(/\/+$/, '');
  // A bare CIP-14 fingerprint path renders the standalone asset page;
  // `/policy/<28-byte hex>` renders the policy asset grid; `/<bech32>/assets`
  // renders the owned-assets grid for a payment address or stake credential;
  // everything else is a feed (root, pool id, drep id, addr, stake, …).
  // A bare CIP-14 fingerprint, optionally `/files/N` to deep-link a specific media.
  const assetMatch = /^(asset1[a-z0-9]+)(?:\/files\/(\d+))?$/.exec(path);
  const assetFingerprint = assetMatch?.[1] ?? null;
  const assetFileIndex = Number(assetMatch?.[2] ?? 0);
  const policyId = /^policy\/([0-9a-f]{56})$/.exec(path)?.[1] ?? null;
  const ownedAssetsSubject = /^((addr|stake)(_test)?1[a-z0-9]+)\/assets$/.exec(path)?.[1] ?? null;
  // `/<bech32>/assets/<policy>` drills into one policy of an owned-assets page.
  const ownedPolicyMatch = /^((addr|stake)(_test)?1[a-z0-9]+)\/assets\/([0-9a-f]{56})$/.exec(path);
  const ownedPolicySubject = ownedPolicyMatch?.[1] ?? null;
  const ownedPolicy = ownedPolicyMatch?.[4] ?? null;

  // `/<pool|drep bech32>/delegators` renders that subject's delegators grid.
  const delegatorsSubject = /^((pool|drep|drep_script)1[a-z0-9]+)\/delegators$/.exec(path)?.[1] ?? null;

  // `/$handle` resolves an ADA Handle to its holder's address and redirects there. Only a
  // `$`-prefixed path is a handle — `pool.pm/handle` (no `$`) is not, and falls to Not Found.
  const handleSeg = path.startsWith('$') ? path.slice(1) : null;
  const slash = handleSeg?.indexOf('/') ?? -1;
  const handleName = handleSeg ? (slash >= 0 ? handleSeg.slice(0, slash) : handleSeg) : null;
  const handleRest = handleSeg && slash >= 0 ? handleSeg.slice(slash) : '';

  // Anything that isn't one of the known routes above and isn't a valid feed subject
  // (root or a checksum-valid bech32 of a known prefix) is a dead URL → Not Found. This
  // also stops the SSE reconnect loop that a bogus `/events/{garbage}` (400) would spin.
  const routeNotFound =
    !handleName &&
    !assetFingerprint &&
    !policyId &&
    !ownedAssetsSubject &&
    !(ownedPolicySubject && ownedPolicy) &&
    !delegatorsSubject &&
    !isFeedPath(path);

  // The owned-assets drill-down shares the subject's SSE feed (drop the /policy suffix).
  const ssePath = ownedPolicySubject ? `${ownedPolicySubject}/assets` : path;

  function sseUrl(): string {
    const base = ssePath ? `${SSE_BASE}/${ssePath}` : SSE_BASE;
    // Negotiate thumbnail resolution: the server picks the power-of-2 nftcdn
    // size rung matching this device's pixel ratio.
    const sep = base.includes('?') ? '&' : '?';
    return `${base}${sep}dpr=${window.devicePixelRatio}`;
  }

  // Whether the page we'd go back to is pool.pm itself. History entries can't be inspected
  // (privacy), but this app navigates via full page loads, so `document.referrer` is the
  // previous page — a same-origin referrer means Backspace-back stays on the site.
  let backStaysOnSite = false;
  try {
    backStaysOnSite = !!document.referrer && new URL(document.referrer).origin === window.location.origin;
  } catch {
    backStaysOnSite = false;
  }

  // Backspace-family navigation, only when nothing editable is focused (so it never eats a
  // keystroke mid-edit — in a field Backspace deletes a char and Ctrl+Backspace a word) and
  // with no Alt/Meta/Shift held:
  //   Backspace       — back one step, but scoped to in-app history: it walks back through
  //                     pool.pm and stops rather than ejecting to an external referrer on a
  //                     stray press (the back button still does the standard cross-site thing).
  //   Ctrl+Backspace  — jump straight to the pool.pm homepage (not a browser binding outside
  //                     a text field).
  $effect(() => {
    function onKey(e: KeyboardEvent) {
      if (e.key !== 'Backspace' || e.metaKey || e.altKey || e.shiftKey) return;
      const t = e.target as HTMLElement | null;
      if (t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.tagName === 'SELECT' || t.isContentEditable))
        return;
      if (e.ctrlKey) {
        if (window.location.pathname !== '/') {
          e.preventDefault();
          window.location.href = '/';
        }
      } else if (backStaysOnSite) {
        e.preventDefault();
        window.history.back();
      }
    }
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  });

  $effect(() => {
    // The standalone asset and policy pages are stateless HTTP views — no SSE. Handle
    // redirects and Not Found pages don't connect either (a `/events/$handle` or
    // `/events/{garbage}` would 400 and reconnect-loop). The owned-assets page *does*
    // connect (to `/events/<bech32>/assets`): its SSE feed sends the address/stake header
    // and keeps the connection open for live asset updates.
    if (assetFingerprint || policyId || handleName || routeNotFound) return;

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
  let searchOpen = $state(false); // bound from SearchBar; hides the logo while open
  let hideTimer: ReturnType<typeof setTimeout>;
  const IDLE_HIDE_MS = 3000; // hide the corner chrome this long after the last interaction

  function showUiTransiently() {
    uiVisible = true;
    clearTimeout(hideTimer);
    hideTimer = setTimeout(() => (uiVisible = false), IDLE_HIDE_MS);
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

<a class="home-logo" class:idle-hidden={!uiVisible && !searchOpen} href="/" aria-label="pool.pm home">
  <img src="/pool.pm.svg" alt="pool.pm" />
</a>

<SearchBar visible={uiVisible} bind:open={searchOpen} />

<main>
  {#if handleName}
    <HandleRedirect name={handleName} rest={handleRest} />
  {:else if routeNotFound}
    <NotFound detail={`/${path}`} />
  {:else if assetFingerprint}
    <AssetPage fingerprint={assetFingerprint} initialIndex={assetFileIndex} />
  {:else if policyId}
    <AssetsGrid
      endpoint={`/api/policy/${policyId}`}
      title={`${policyId.slice(0, 12)}…`}
      mode="hide-broken"
      {uiVisible}
    />
  {:else if ownedAssetsSubject}
    <AssetsGrid
      endpoint={`/api/assets/${ownedAssetsSubject}`}
      title={`${ownedAssetsSubject.slice(0, 12)}… assets`}
      mode="text-fallback"
      grouped
      subject={ownedAssetsSubject}
      {uiVisible}
    />
  {:else if delegatorsSubject}
    <DelegatorsGrid
      endpoint={`/api/delegators/${delegatorsSubject}`}
      title={`${delegatorsSubject.slice(0, 12)}… delegators`}
      {uiVisible}
    />
  {:else if ownedPolicySubject && ownedPolicy}
    <AssetsGrid
      endpoint={`/api/assets/${ownedPolicySubject}/${ownedPolicy}`}
      title={`${ownedPolicy.slice(0, 12)}… assets`}
      mode="text-fallback"
      policyFilter={ownedPolicy}
      {uiVisible}
    />
  {:else}
    <Feed />
  {/if}
</main>

<style>
  .home-logo {
    position: fixed;
    top: 12px;
    /* Top-right corner; clears the feed's scrollbar, same as the search bar. */
    right: calc(12px + var(--scrollbar-width, 0px));
    z-index: 101;
    display: block;
    opacity: 1;
    transition: opacity 0.15s ease; /* fast fade-in on interaction */
  }
  /* Slow fade-out when idle. */
  .home-logo.idle-hidden {
    opacity: 0;
    pointer-events: none;
    transition: opacity 1.5s ease;
  }
  .home-logo img {
    width: 48px;
    height: auto;
    display: block;
  }
</style>
