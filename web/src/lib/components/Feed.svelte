<script lang="ts">
  import { onMount, tick, untrack } from 'svelte';
  import { slide } from 'svelte/transition';
  import { SvelteSet } from 'svelte/reactivity';
  import { sections, config, pool, drep, stake, address, cardano, blockCount } from '../stores';
  import type { GenesisConfig, Section } from '../types';
  import { TX_WIDTH, FLIP_DURATION, poolColor, formatTicker, formatAda, layoutGrid } from '../layout';
  import { loadOlder, resetOlder } from '../sse';
  import Transaction from './Transaction.svelte';
  import SubjectCard from './SubjectCard.svelte';

  const MAX_BLOCKS = 30;
  /** Prune blocks older than 1h whose net stake change is below this fraction of live stake. */
  const STAKE_CHANGE_PRUNE_DIVISOR = 1_000n; // 0.1%
  const PX_PER_SECOND = 2;
  const BLOCK_PADDING = 10;
  const BLOCK_BORDER = 0; // no border — elevation is the shadow + inset top light-catch
  const BLOCK_INSET = (BLOCK_PADDING + BLOCK_BORDER) * 2;

  let feedEl: HTMLDivElement;
  let feedWidth = $state(0);
  let feedHeight = $state(0);
  const LANDSCAPE_MARGIN = 16; // vertical breathing room in landscape
  let landscape = $state(typeof window !== 'undefined' && window.innerWidth > window.innerHeight);
  let actualGridWidths = $state<Record<string, number>>({});

  // A subject feed (pool/drep/stake) vs the global homepage feed. Drives block
  // spacing, coloring, and whether the per-block minting-pool ticker is shown.
  const isSubjectFeed = $derived(!!($pool || $drep || $stake || $address));

  // --- Block folding (pool/DRep feeds) ---
  // On pool/DRep feeds, blocks fold by default to declutter: a compact summary replaces
  // the tx grid; clicking the block header toggles it. `unfoldedIds` holds the sections the
  // user has expanded (default = folded). Only block sections on a pool/DRep feed fold.
  const foldable = $derived(!!($pool || $drep));
  const unfoldedIds = new SvelteSet<string>();
  function sectionFolded(section: Section): boolean {
    return foldable && !!section.block && !unfoldedIds.has(section.id);
  }
  function toggleFold(id: string) {
    if (unfoldedIds.has(id)) unfoldedIds.delete(id);
    else unfoldedIds.add(id);
    // The section's size changes on fold/unfold; re-measure so the pack layout repositions
    // (the measure effect keys on $sections, which a fold toggle doesn't change).
    tick().then(scheduleMeasure);
  }
  // A block this feed's pool minted itself (vs a stake-change block from another pool, or —
  // on a DRep feed — any block). Own blocks fold to "N txs · size"; others to a stake summary.
  function isOwnBlock(section: Section): boolean {
    return !!$pool && !!section.block && section.block.pool_id === $pool.pool_id;
  }

  // Folded-block size scales with the block's KB, capped so a full block is ~2× a small one.
  // Folded own-block = a SQUARE whose side ∝ block KB, so a full (~90 KB) block is ~4× the
  // smallest in both width and height. The side starts at the header/footer natural width (the
  // block can't be narrower than the date/ticker/hash) and grows to 4×. Clamped to the smaller
  // window dimension (minus margins) so a folded block never overflows the viewport.
  const FOLD_MIN_PX = 130; // ≈ header/footer natural width (the smallest square side)
  const FOLD_MAX_PX = 520; // 4× the min
  const MAX_BLOCK_KB = 90; // ~mainnet max block body size
  function foldSizePx(section: Section): number {
    const kb = (section.block?.size ?? 0) / 1024;
    const raw = FOLD_MIN_PX + (FOLD_MAX_PX - FOLD_MIN_PX) * (Math.min(kb, MAX_BLOCK_KB) / MAX_BLOCK_KB);
    const vw = feedWidth > 0 ? feedWidth : Infinity;
    const vh = feedHeight > 0 ? feedHeight : Infinity;
    return Math.round(Math.min(raw, Math.min(vw, vh) - 2 * LANDSCAPE_MARGIN));
  }

  // One consistent way to fold/unfold any block: click anywhere on it. Real links (the
  // minting-pool ticker on a stake-change block, addresses) and an unfolded block's tx tiles
  // keep their own behaviour, so those clicks don't toggle.
  function onSectionClick(e: MouseEvent, section: Section) {
    if (!foldable || !section.block) return;
    const t = e.target as HTMLElement | null;
    // Links/buttons act; the footer (block hash + number) stays selectable/copyable; a click
    // on an unfolded block's actual tx tile keeps its own interactions — but clicking the empty
    // space around/between the tiles (still inside the tx grid) folds the block.
    if (t?.closest('a, button, .block-footer')) return;
    if (!sectionFolded(section) && t?.closest('.tx-grid-item')) return;
    // Don't toggle when the click ends a text selection.
    if (window.getSelection()?.toString()) return;
    toggleFold(section.id);
  }

  // Fold or unfold *every* block at once — triggered by a click on the empty feed background.
  // Fold all if any block is currently open, else unfold all.
  function toggleAllFold() {
    if (unfoldedIds.size > 0) unfoldedIds.clear();
    else for (const s of $sections) if (s.block) unfoldedIds.add(s.id);
    tick().then(scheduleMeasure);
  }
  function onBackgroundClick(e: MouseEvent) {
    if (!foldable) return;
    const t = e.target as HTMLElement | null;
    // Only a click on the empty background — not a block/mempool section or the header card.
    if (t?.closest('.section') || t?.closest('.subject-card')) return;
    if (window.getSelection()?.toString()) return;
    toggleAllFold();
  }

  // Hide/show all the pool's own minted blocks — toggled by clicking the epoch block-count on
  // the mempool. `displaySections` (used for both rendering and layout) drops them when hidden;
  // sections[0] (the mempool) and stake-change blocks always stay.
  let hideOwnBlocks = $state(false);
  // Hide the pool's own minted blocks when toggled (keep sections[0] = the mempool).
  const displaySections = $derived(
    hideOwnBlocks && $pool
      ? $sections.filter((s, i) => i === 0 || !(s.block && s.block.pool_id === $pool!.pool_id))
      : $sections,
  );

  // On a pool feed, number each block the pool *minted* by its lifetime index: the newest
  // minted block is #blocks (from the Pool header, which the server re-emits on every mint and
  // rollback), each older one one less. Only the pool's own blocks count — a pool feed also
  // shows blocks minted by *other* pools that changed this pool's stake (pool_id !== the feed
  // pool); those are skipped and don't consume a number. Infinite scroll loads the pool's
  // minted blocks contiguously from the tip, so the minted sections stay the newest
  // contiguous run and the decrement is exact.
  // Empty (inert) on non-pool feeds, where `$pool` is null. Recomputes on section/blocks change.
  const poolBlockNumbers = $derived.by(() => {
    const map = new Map<string, number>();
    const total = $pool?.blocks;
    const poolId = $pool?.pool_id;
    if (total == null || poolId == null) return map;
    let k = 0;
    for (const s of $sections) {
      if (s.block?.pool_id === poolId) {
        map.set(s.id, total - k);
        k++;
      }
    }
    return map;
  });

  // Section positioning: absolute layout with smooth CSS transitions
  let sectionRefs = new Map<string, HTMLElement>();
  let sectionPositions = $state<Map<string, { pos: number; spacing: number }>>(new Map());
  let canvasSize = $state(0);
  let animated = $state(false);
  let sectionObserver: ResizeObserver | undefined;
  let measurePending = false;
  let scrolledAway = false;
  let ignoreScroll = false;

  // Subject feeds: space blocks by elapsed time, compressing the enormous range
  // (seconds → years). A cube-root-ish power curve keeps same-day blocks tight
  // while still visibly separating day-, week- and month-scale gaps — a log
  // flattened the high end too much, so months looked barely farther than hours.
  // Roughly: 1h≈15px, 6h≈26, 1d≈41, 1wk≈77, 1mo≈124, 6mo≈219 (then clamped).
  const GAP_TIME_EXP = 0.33;
  function timeGap(seconds: number): number {
    return Math.pow(seconds, GAP_TIME_EXP);
  }

  // Available height for tx columns in landscape mode.
  // Overhead = section border (4) + padding (20) + 3 flex gaps (30) + header (10) + ticker (13) + footer (10)
  const SECTION_OVERHEAD = BLOCK_BORDER * 2 + BLOCK_PADDING * 2 + BLOCK_PADDING * 3 + 33; // = 87
  let txAreaHeight = $derived(feedHeight - SECTION_OVERHEAD - LANDSCAPE_MARGIN);

  function trackSection(node: HTMLElement, id: string) {
    sectionRefs.set(id, node);
    sectionObserver?.observe(node);
    scheduleMeasure();
    return {
      destroy() {
        sectionObserver?.unobserve(node);
        sectionRefs.delete(id);
      },
    };
  }

  function scheduleMeasure() {
    if (!measurePending) {
      measurePending = true;
      tick().then(() => {
        measurePending = false;
        measureSections();
      });
    }
  }

  function handleScroll() {
    if (ignoreScroll) return;
    if (!feedEl) return;
    // row-reverse: scrollLeft ≈ 0 at right edge (can be slightly negative
    // due to padding/scrollbar gutter), goes more negative when scrolled left
    const wasAway = scrolledAway;
    scrolledAway = landscape ? feedEl.scrollLeft < -30 : feedEl.scrollTop > 10;

    // Back at the newest edge: re-seed pagination so a later scroll into history refills
    // contiguously (the next block trims the accumulated older blocks — see the cap above).
    if (isSubjectFeed && wasAway && !scrolledAway) resetOlder();

    // Near the oldest edge (left in landscape, bottom in portrait) → prefetch the
    // next page of older blocks (loadOlder self-guards against re-entry / end).
    if (isSubjectFeed) {
      const threshold = landscape ? feedEl.clientWidth : feedEl.clientHeight;
      const fromOldest = landscape
        ? -feedEl.scrollLeft + feedEl.clientWidth // distance scrolled toward oldest
        : feedEl.scrollTop + feedEl.clientHeight;
      if (fromOldest >= canvasSize - threshold) loadOlder();
    }
  }

  function measureSections() {
    const sects = displaySections;

    // Before remeasuring, record an anchor section's viewport position.
    // After DOM update we'll measure the actual shift and compensate scroll.
    let anchorEl: HTMLElement | undefined;
    let anchorBefore: number | undefined;
    if (animated && scrolledAway && canvasSize > 0) {
      for (let i = 1; i < sects.length; i++) {
        const el = sectionRefs.get(sects[i].id);
        if (el) {
          anchorEl = el;
          const rect = el.getBoundingClientRect();
          anchorBefore = landscape ? rect.left : rect.top;
          break;
        }
      }
    }

    const positions = new Map<string, { pos: number; spacing: number }>();
    let pos = 0;
    for (let i = 0; i < sects.length; i++) {
      const section = sects[i];
      let spacing = 0;
      if (i > 0) {
        const prev = sects[i - 1].block?.timestamp ?? sects[i - 1].reward?.timestamp ?? now / 1000;
        const curTime = section.block?.timestamp ?? section.reward?.timestamp;
        const timeDelta = curTime != null ? Math.max(0, prev - curTime) : 0;
        const maxSpacing = Math.round((landscape ? feedWidth : feedHeight) / 2);
        spacing = Math.min(
          maxSpacing,
          Math.max(2, Math.round(isSubjectFeed ? timeGap(timeDelta) : PX_PER_SECOND * timeDelta)),
        );
        pos += spacing;
      }
      positions.set(section.id, { pos, spacing });
      const el = sectionRefs.get(section.id);
      pos += landscape ? (el?.offsetWidth ?? 0) : (el?.offsetHeight ?? 0);
    }

    if (anchorEl) animated = false;

    sectionPositions = positions;
    canvasSize = pos;

    if (anchorEl && anchorBefore !== undefined) {
      const anchor = anchorEl;
      const before = anchorBefore;
      tick().then(() => {
        const rect = anchor.getBoundingClientRect();
        const shift = (landscape ? rect.left : rect.top) - before;
        if (shift !== 0) {
          ignoreScroll = true;
          if (landscape) feedEl.scrollLeft += shift;
          else feedEl.scrollTop += shift;
          requestAnimationFrame(() => {
            ignoreScroll = false;
          });
        }
        requestAnimationFrame(() => {
          animated = true;
        });
      });
    } else if (!animated) {
      tick().then(() => {
        animated = true;
      });
    }
  }

  function updateLandscape() {
    const was = landscape;
    landscape = window.innerWidth > window.innerHeight;
    if (was !== landscape) {
      animated = false;
      scrolledAway = false;
    }
  }

  onMount(() => {
    updateLandscape();
    window.addEventListener('resize', updateLandscape);

    // Use content box (matching ResizeObserver's contentRect) — exclude padding
    const cs = getComputedStyle(feedEl);
    feedWidth = feedEl.clientWidth - parseFloat(cs.paddingLeft) - parseFloat(cs.paddingRight);
    feedHeight = feedEl.clientHeight - parseFloat(cs.paddingTop) - parseFloat(cs.paddingBottom);
    const feedObserver = new ResizeObserver((entries) => {
      feedWidth = entries[0]?.contentRect.width ?? 0;
      feedHeight = entries[0]?.contentRect.height ?? 0;
    });
    feedObserver.observe(feedEl);

    feedEl.addEventListener('scroll', handleScroll, { passive: true });

    function handleWheel(e: WheelEvent) {
      if (!landscape) return;
      if (e.deltaX !== 0) return; // native horizontal scroll, don't remap
      if (e.ctrlKey || e.metaKey || e.shiftKey || e.altKey) return;
      e.preventDefault();
      feedEl.scrollLeft -= e.deltaY;
    }

    function handleKeydown(e: KeyboardEvent) {
      if (!landscape || e.ctrlKey || e.metaKey || e.altKey) return;
      const t = e.target as HTMLElement | null;
      if (t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.tagName === 'SELECT' || t.isContentEditable))
        return;
      // Landscape scrolls horizontally (row-reverse): scrollLeft is 0 at the newest (right)
      // edge and goes negative toward older. Map the vertical scroll keys onto it so they
      // behave like portrait — Home = newest, PageDown = older (leftward), PageUp = newer.
      const page = feedEl.clientWidth * 0.9;
      if (e.key === 'Home') {
        e.preventDefault();
        feedEl.scrollLeft = 0;
      } else if (e.key === 'PageDown') {
        e.preventDefault();
        feedEl.scrollLeft -= page;
      } else if (e.key === 'PageUp') {
        e.preventDefault();
        feedEl.scrollLeft += page;
      }
    }

    feedEl.addEventListener('wheel', handleWheel, { passive: false });
    window.addEventListener('keydown', handleKeydown);

    sectionObserver = new ResizeObserver(scheduleMeasure);
    for (const el of sectionRefs.values()) sectionObserver.observe(el);

    return () => {
      feedEl.removeEventListener('scroll', handleScroll);
      feedEl.removeEventListener('wheel', handleWheel);
      window.removeEventListener('keydown', handleKeydown);
      window.removeEventListener('resize', updateLandscape);
      feedObserver.disconnect();
      sectionObserver?.disconnect();
    };
  });

  function sectionMaxWidth(section: Section): string {
    // Folded own blocks are sized to a square via CSS (--fold-size), not this max-width.
    if (landscape) return 'none';
    const gw = actualGridWidths[section.id];
    if (gw) return `${gw + BLOCK_INSET}px`;
    if (section.txs.length === 0) return `${TX_WIDTH + BLOCK_INSET}px`;
    return 'none';
  }

  let now = $state(Date.now());

  const PREVIEW_MAGIC = 2;
  const PREPROD_MAGIC = 1;

  function networkName(magic: number): string | null {
    if (magic === PREVIEW_MAGIC) return 'preview';
    if (magic === PREPROD_MAGIC) return 'preprod';
    return null;
  }

  // Set page title from pool ticker, DRep name, or stake address
  $effect(() => {
    const p = $pool;
    const d = $drep;
    const s = $stake;
    const a = $address;
    const net = $config ? networkName($config.magic) : null;
    const site = net ? `${net}.pool.pm` : 'pool.pm';
    if (d) {
      document.title = `${d.given_name ?? d.drep_id.slice(5, 13)} - ${site}`;
    } else if (p) {
      document.title = `${formatTicker(p.ticker ?? p.pool_id.slice(5, 10))} - ${site}`;
    } else if (s) {
      document.title = `${s.stake_address.slice(0, 12)}… - ${site}`;
    } else if (a) {
      document.title = `${a.address.slice(0, 12)}… - ${site}`;
    } else {
      document.title = site;
    }
  });

  $effect(() => {
    const interval = setInterval(() => {
      now = Date.now();
    }, 1000);
    return () => clearInterval(interval);
  });

  $effect(() => {
    $blockCount; // trigger on each new block
    const p = $pool;
    const d = $drep;
    const nowSec = Math.floor(Date.now() / 1000);
    sections.update((s) => {
      s[0].txs = s[0].txs.filter((tx) => !tx.expiry || tx.expiry > nowSec);
      // Keep paginated (older) blocks only while the user is scrolled into history; at
      // the newest edge (top / top-right in landscape) — or on the home feed — trim back
      // to the live window so idle pages don't accumulate. `resetOlder` on return-to-top
      // (in handleScroll) re-seeds pagination so re-scrolling refills contiguously.
      let result = isSubjectFeed && scrolledAway ? s : s.slice(0, MAX_BLOCKS + 1);
      // In pool/drep feeds, prune old small stake/delegation changes
      const liveStake = p?.live_stake ?? d?.live_stake;
      if (liveStake) {
        const threshold = BigInt(liveStake) / STAKE_CHANGE_PRUNE_DIVISOR;
        const oneHourAgo = nowSec - 3600;
        const feedPoolId = p?.pool_id;
        const feedDrepId = d?.drep_id;
        result = result.filter((section, i) => {
          if (i === 0 || !section.block) return true;
          if (feedPoolId && section.block.pool_id === feedPoolId) return true;
          if (section.block.timestamp > oneHourAgo) return true;
          if (
            section.txs.some((tx) =>
              tx.delegations?.some(
                (dl) =>
                  (feedPoolId && (dl.from_pool_id === feedPoolId || dl.to_pool_id === feedPoolId)) ||
                  (feedDrepId && (dl.from_drep_id === feedDrepId || dl.to_drep_id === feedDrepId)),
              ),
            )
          )
            return true;
          // Keep this subject's own governance votes (SPO on a pool feed, DRep vote on
          // a DRep feed) — they carry no stake change or delegation.
          if (section.txs.some((tx) => tx.votes?.some((v) => v.voter_id === feedPoolId || v.voter_id === feedDrepId)))
            return true;
          let net = 0n;
          for (const tx of section.txs) {
            if (tx.stake_change) net += BigInt(tx.stake_change);
          }
          if (net < 0n) net = -net;
          return net >= threshold;
        });
      }
      return result;
    });
  });

  // Re-measure positions when sections change, time advances, or orientation changes
  $effect(() => {
    displaySections;
    now;
    landscape;
    untrack(scheduleMeasure);
  });

  // Fill the viewport: if the displayed content doesn't reach the screen edge there's no
  // scrollbar, so the scroll-driven `loadOlder` never fires (notably after hiding the pool's
  // own blocks, which can leave only a few stake-change blocks). Keep loading older history
  // until it fills or history runs out. Re-runs as $sections/canvasSize change, so it chains
  // one page at a time; `autoFillTries` caps runaway on a feed that never fills (reset when
  // the fill context changes — orientation or hiding own blocks).
  let autoFillTries = $state(0);
  const AUTO_FILL_MAX = 12;
  $effect(() => {
    $sections.length;
    canvasSize;
    landscape;
    feedHeight;
    feedWidth;
    hideOwnBlocks;
    untrack(() => {
      if (!isSubjectFeed || !feedEl || canvasSize === 0) return;
      const viewport = landscape ? feedWidth : feedHeight;
      if (canvasSize <= viewport + 20 && autoFillTries < AUTO_FILL_MAX) {
        autoFillTries++;
        loadOlder();
      }
    });
  });

  // After feed resizes (e.g. orientation change), layoutGrid rewraps and changes section
  // widths. Schedule a deferred re-measure so positions reflect the new widths.
  $effect(() => {
    feedHeight;
    feedWidth;
    untrack(() => {
      requestAnimationFrame(() => scheduleMeasure());
    });
  });

  // A block carrying this DRep's own governance vote — the DRep-feed counterpart of a
  // pool's own minted block, so it keeps the minting pool's color while folded instead of
  // going grey like the surrounding stake-change blocks.
  function hasSubjectVote(section: Section): boolean {
    const id = $drep?.drep_id;
    if (!id || !section.block) return false;
    return section.txs.some((tx) => tx.votes?.some((v) => v.voter_id === id));
  }

  function sectionColors(section: Section, folded: boolean): { bg: string; border: string; accent: string } {
    // Reward capsule: neutral (not pool-colored) with a visible gray border.
    // Neutral panels (reward capsule, mempool) sit at the shared elevated surface tone
    // (--surface-1 = #26262c), a touch lighter than the grey page so the shadow lifts them.
    if (section.reward) return { bg: '#26262c', border: '#555', accent: 'rgb(255 255 255 / 0.4)' };
    if (!section.block) return { bg: '#26262c', border: '#26262c', accent: 'rgb(255 255 255 / 0.4)' };
    // Folded stake-change block (pool/DRep feed): the minting pool isn't its meaning — the
    // delegator activity is — so it wears the neutral mempool grey and only regains the
    // pool color when unfolded (where the minter is shown). A pool's own minted block and a
    // DRep's own vote block are that feed's headline events: they keep their color.
    if (folded && !isOwnBlock(section) && !hasSubjectVote(section))
      return { bg: '#26262c', border: '#26262c', accent: 'rgb(255 255 255 / 0.4)' };
    const c = poolColor(section.block.pool_id);
    return { bg: c, border: c, accent: c };
  }

  function introScale(node: HTMLElement) {
    node.style.animation = `section-intro ${FLIP_DURATION}ms ease`;
    node.style.transition = 'none';
    const timer = setTimeout(() => {
      node.style.animation = '';
      node.style.transition = '';
    }, FLIP_DURATION);
    return {
      destroy() {
        clearTimeout(timer);
      },
    };
  }

  function timeAgo(timestamp: number): string {
    const sec = Math.floor((now - timestamp * 1000) / 1000);
    if (sec < 60) return `${sec}s ago`;
    if (sec < 3600) return `${Math.floor(sec / 60)}m ago`;
    if (sec < 86400) return `${Math.floor(sec / 3600)}h ago`;
    return `${Math.floor(sec / 86400)}d ago`;
  }

  function formatDate(timestamp: number): string {
    const date = new Date(timestamp * 1000);
    const today = new Date(now);
    if (date.toDateString() === today.toDateString()) return 'Today';
    return date.toLocaleDateString(undefined, {
      month: 'short',
      day: 'numeric',
      ...(date.getFullYear() !== today.getFullYear() ? { year: 'numeric' } : {}),
    });
  }

  function formatTime(timestamp: number): string {
    return new Date(timestamp * 1000).toLocaleTimeString();
  }

  function epochInfo(genesis: GenesisConfig): { epoch: number; epochEnd: number } {
    const nowSec = Math.floor(now / 1000);
    const slot =
      genesis.shelley_known_slot + Math.floor((nowSec - genesis.shelley_known_time) / genesis.shelley_slot_length);
    const shelleyStartEpoch = Math.floor(
      (genesis.shelley_known_slot * genesis.byron_slot_length) / genesis.byron_epoch_length,
    );
    const epochsSince = Math.floor((slot - genesis.shelley_known_slot) / genesis.shelley_epoch_length);
    const epoch = shelleyStartEpoch + epochsSince;
    const epochEndSlot = genesis.shelley_known_slot + (epochsSince + 1) * genesis.shelley_epoch_length;
    const epochEnd =
      genesis.shelley_known_time + (epochEndSlot - genesis.shelley_known_slot) * genesis.shelley_slot_length;
    return { epoch, epochEnd };
  }

  function formatTimeLeft(epochEnd: number): string {
    const sec = Math.max(0, Math.floor(epochEnd - now / 1000));
    if (sec >= 172800) return `${Math.floor(sec / 86400)} days left`;
    if (sec >= 7200) return `${Math.floor(sec / 3600)} hours left`;
    if (sec >= 120) return `${Math.floor(sec / 60)} mins left`;
    return `${sec} secs left`;
  }
</script>

<!-- svelte-ignore a11y_no_static_element_interactions a11y_click_events_have_key_events -->
<div
  class="feed"
  class:landscape
  bind:this={feedEl}
  style:--block-padding="{BLOCK_PADDING}px"
  style:--block-border="{BLOCK_BORDER}px"
  style:--section-min-width="{TX_WIDTH + BLOCK_INSET}px"
  style:--flip-duration="{FLIP_DURATION}ms"
  onclick={onBackgroundClick}
>
  <SubjectCard pool={$pool} drep={$drep} stake={$stake} address={$address} cardano={$cardano} {landscape} />
  <div class="canvas" style={landscape ? `width: ${canvasSize}px` : `height: ${canvasSize}px`}>
    {#each displaySections as section, i (section.id)}
      {@const isMempool = !section.block && !section.reward}
      {@const layout = sectionPositions.get(section.id)}
      {@const secFolded = sectionFolded(section)}
      {@const colors = sectionColors(section, secFolded)}
      {@const secOwn = section.block ? isOwnBlock(section) : false}
      {@const foldOwn = !!section.block && secFolded && secOwn}
      <!-- svelte-ignore a11y_no_static_element_interactions a11y_click_events_have_key_events -->
      <div
        class="section"
        class:mempool={isMempool}
        class:reward-capsule={!!section.reward}
        class:animated
        class:measured={canvasSize > 0}
        class:foldable={foldable && !!section.block}
        class:fold-own={foldOwn}
        style:--fold-size={foldOwn ? `${foldSizePx(section)}px` : undefined}
        class:has-line={i > 0 && (layout?.spacing ?? 0) > 0}
        style:--block-bg={colors.bg}
        style:--section-color={colors.accent}
        style:--meta-color={colors.bg.startsWith('#') ? 'rgb(255 255 255 / 0.4)' : ''}
        style:--section-width={sectionMaxWidth(section)}
        style:--spacing="{layout?.spacing ?? 0}px"
        style:transform={landscape
          ? `translate(${-(layout?.pos ?? 0)}px, ${Math.round((feedHeight - (sectionRefs.get(section.id)?.offsetHeight ?? 0)) / 2)}px)`
          : `translateY(${layout?.pos ?? 0}px)`}
        ongridwidth={(e: CustomEvent<number>) => {
          actualGridWidths[section.id] = e.detail;
        }}
        onclick={(e) => onSectionClick(e, section)}
        use:trackSection={section.id}
        use:introScale
        out:slide|local={{ duration: isMempool ? 0 : FLIP_DURATION, axis: landscape ? 'x' : 'y' }}
      >
        {#if section.reward}
          <div class="block-header">
            <span class="block-meta block-when">{formatDate(section.reward.timestamp)}</span>
            <span class="block-meta block-when">
              {#if i === 1}{timeAgo(section.reward.timestamp)}{:else}{formatTime(section.reward.timestamp)}{/if}
            </span>
          </div>
          <span class="block-ticker">REWARDS</span>
          <div class="reward-rows">
            {#each section.reward.rows as row, ri (row.label + (row.pool_id ?? '') + ri)}
              <div class="reward-row">
                <div class="reward-source">
                  {#if row.pool_id}
                    <a class="reward-pool" style:color={poolColor(row.pool_id)} href="/{row.pool_id}"
                      >{formatTicker(row.pool_ticker ?? row.pool_id.slice(5, 10))}</a
                    >
                  {/if}
                  <span class="reward-label">{row.label}</span>
                </div>
                <span class="reward-amount">+{formatAda(row.amount)}</span>
              </div>
            {/each}
          </div>
        {:else}
          {@const folded = secFolded}
          {@const own = secOwn}
          <div class="block-header">
            {#if isMempool && $config?.genesis}
              {@const ei = epochInfo($config.genesis)}
              <span class="block-meta">Epoch {ei.epoch}</span>
              <span class="block-meta">{formatTimeLeft(ei.epochEnd)}</span>
            {:else if section.block}
              <span class="block-meta block-when">{formatDate(section.block.timestamp)}</span>
              <span class="block-meta block-when">
                {#if i === 1}{timeAgo(section.block.timestamp)}{:else}{formatTime(section.block.timestamp)}{/if}
              </span>
            {/if}
          </div>
          {#if isMempool && $pool && $config?.genesis}
            <!-- Blocks this pool minted in the current epoch (server-counted, exact). On its
                 own centered line so the empty-mempool width stays ~1 tx (adding it to the
                 nowrap header row would widen it). The count is for `$pool.epoch`; once the
                 displayed epoch rolls past it, show 0 until the pool's next mint re-emits. -->
            {@const cur = epochInfo($config.genesis).epoch}
            {@const n = cur === $pool.epoch ? $pool.epoch_blocks : 0}
            <!-- Click to hide/show all the pool's own minted blocks; dimmed while hidden. -->
            <!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
            <div
              class="epoch-blocks"
              class:dimmed={hideOwnBlocks}
              style:color={poolColor($pool.pool_id)}
              onclick={(e) => {
                e.stopPropagation();
                hideOwnBlocks = !hideOwnBlocks;
                autoFillTries = 0; // let the viewport-fill run again for the new visible set
                tick().then(scheduleMeasure);
              }}
            >
              {n} block{n > 1 ? 's' : ''}
            </div>
          {/if}
          <!-- Ticker. Own blocks: a plain label (a link to your own page would be redundant, and
               clicking it must fold like the rest of the block, so it's not a link). Stake-change
               blocks: the minting-pool link, shown only when unfolded (hidden on the folded view,
               whose meaning is the delegators, not the minter). -->
          {#if section.block && own}
            {@const pn = poolBlockNumbers.get(section.id)}
            <span class="block-ticker"
              >{formatTicker(
                section.block.pool_ticker ?? section.block.pool_id?.slice(5, 10) ?? '',
              )}{#if pn != null && pn > 0}<span class="pool-block-no">&nbsp;#{pn}</span>{/if}</span
            >
          {:else if section.block && !folded}
            <a class="block-ticker" href="/{section.block.pool_id ?? ''}"
              >{formatTicker(section.block.pool_ticker ?? section.block.pool_id?.slice(5, 10) ?? '')}</a
            >
          {:else if !section.block && section.txs.length > 0}
            <!-- Hide the MEMPOOL label while the mempool is empty (no pending txs). -->
            <span class="block-ticker">MEMPOOL</span>
          {/if}

          {#if section.block && folded && own}
            <!-- Own minted block: tx count + block size, box sized by KB (--fold-size, which also
                 caps the header/footer width so the whole block scales). Section click folds. -->
            <div class="fold-summary own">
              <span class="fold-count">{section.txs.length} tx{section.txs.length === 1 ? '' : 's'}</span>
              <span class="fold-kb">{(section.block.size / 1024).toFixed(1)} KB</span>
            </div>
          {:else if section.txs.length > 0}
            <!-- Unfolded: the full tx grid. Folded stake-change: the same tiles, decluttered
                 (Transaction `folded` hides everything but each tx's stake meaning). -->
            <div
              class="tx-grid"
              use:layoutGrid={{
                landscape,
                availableWidth: feedWidth - BLOCK_INSET,
                availableHeight: txAreaHeight,
                foldRev: folded,
              }}
            >
              {#each section.txs as tx (tx.hash)}
                <div class="tx-grid-item">
                  <Transaction {tx} compact={landscape && feedHeight < 500} folded={folded && !own} />
                </div>
              {/each}
            </div>
          {/if}

          {#if section.block}
            <div class="block-footer">
              <span class="block-meta block-hash mono">{section.block.hash}</span>
              <span class="block-meta mono">#{section.block.number}</span>
            </div>
          {/if}
        {/if}
      </div>
    {/each}
  </div>
</div>

<style>
  .feed {
    flex: 1;
    overflow-y: auto;
    scrollbar-gutter: stable;
    padding: 16px 20px;
  }

  .feed.landscape {
    display: flex;
    flex-direction: row-reverse;
    align-items: center;
    overflow-y: hidden;
    overflow-x: auto;
  }

  .canvas {
    position: relative;
    flex-shrink: 0;
  }

  .landscape .canvas {
    height: 100%;
    direction: ltr;
  }

  /* Every block container is one flat, rounded panel: its own solid colour (--block-bg) on the
     black page — no gradient, no glow, no border. */
  .section {
    position: absolute;
    left: 0;
    right: 0;
    margin: 0 auto;
    max-width: var(--section-width);
    /* Exactly one tx column + the block inset, so a single-tx block hugs its tile with equal
       margins (no right-side gap). Driven by the same TX_WIDTH/BLOCK_INSET as the width math. */
    min-width: var(--section-min-width);
    border: none;
    border-radius: var(--panel-radius);
    padding: var(--block-padding);
    display: flex;
    flex-direction: column;
    gap: var(--block-padding);
    background: var(--block-bg);
  }

  .landscape .section {
    left: auto;
    right: 0;
    top: 0;
    margin: 0;
  }

  .section:not(.measured) {
    visibility: hidden;
  }

  .section.animated {
    transition: transform var(--flip-duration) ease;
    will-change: transform;
  }

  @keyframes section-intro {
    from {
      scale: 0;
    }
    to {
      scale: 1;
    }
  }

  /* Portrait: vertical connecting line above the block */
  .section.has-line::before {
    content: '';
    position: absolute;
    bottom: calc(100% + var(--block-border));
    left: 50%;
    width: 1px;
    height: var(--spacing);
    background: #444;
  }

  /* Landscape: horizontal connecting line to the right of the block */
  .landscape .section.has-line::before {
    bottom: auto;
    left: calc(100% + var(--block-border));
    top: 50%;
    width: var(--spacing);
    height: 1px;
  }

  .landscape .section.mempool {
    max-height: calc(100vh - 72px);
    overflow: hidden;
  }

  /* Desaturate the mempool, but per-child so the pool-colored epoch-block count is spared
     (a `filter` on the section itself would grey its whole subtree — children can't escape
     a parent filter). Everything else (header, MEMPOOL label, pending txs) stays greyed. */
  .section.mempool > :not(.epoch-blocks) {
    filter: grayscale(1);
  }

  .block-header {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    line-height: 1;
    white-space: nowrap;
    gap: 8px;
  }
  /* A foldable block: clicking anywhere on it toggles (except links / unfolded tx tiles). */
  .section.foldable {
    cursor: pointer;
  }

  /* Folded own-block body (replaces the tx grid): a centered tx-count/size tile whose side is
     --fold-size (∝ block KB), so the block's height (and width, above the header/footer's
     natural size) grows with the block's KB. */
  /* Folded own block is a square whose side (--fold-size) scales with the block's KB. */
  .section.fold-own {
    width: var(--fold-size);
    min-width: var(--fold-size);
    max-width: var(--fold-size);
    height: var(--fold-size);
    box-sizing: border-box;
  }
  .fold-summary.own {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 4px;
    flex: 1; /* fill the square's middle between the header/ticker and the footer */
    color: rgb(255 255 255 / 0.85);
  }
  .fold-count {
    font-size: 12px;
    font-weight: 700;
  }
  .fold-kb {
    font-size: 10px;
    color: var(--meta-color, rgb(255 255 255 / 0.5));
  }

  .block-footer {
    display: flex;
    justify-content: space-between;
    white-space: nowrap;
    gap: 8px;
    /* The hash + number stay selectable/copyable (the section-click fold handler skips them). */
    user-select: text;
    cursor: text;
  }

  .block-meta {
    color: var(--meta-color, rgb(0 0 0 / 0.5));
    font-size: 10px;
  }

  /* Pool's current-epoch block count: its own centered line below the header (and above
     MEMPOOL), so the empty-mempool width stays ~1 tx rather than widening the header row. */
  .epoch-blocks {
    text-align: center;
    white-space: nowrap;
    color: var(--meta-color, rgb(0 0 0 / 0.5));
    font-size: 10px;
    line-height: 1;
    cursor: pointer;
  }
  /* Dimmed while the pool's own blocks are hidden. */
  .epoch-blocks.dimmed {
    opacity: 0.4;
    filter: grayscale(1);
  }

  /* The block date and time read a touch heavier than the rest of the meta line. */
  .block-when {
    font-weight: 500;
  }

  .block-hash {
    overflow: hidden;
    text-overflow: ellipsis;
    max-width: 8ch;
  }

  .block-ticker {
    display: block;
    text-align: center;
    color: white;
    font-size: 13px;
    font-weight: 700;
    line-height: 1;
    text-decoration: none;
  }
  /* The pool-relative block index, secondary to the ticker it follows. */
  .pool-block-no {
    font-weight: 500;
    opacity: 0.6;
  }

  .tx-grid {
    position: relative;
    overflow: hidden;
  }

  .feed:not(.landscape) .tx-grid {
    /* Width is set to the exact packed grid width by layoutGrid (JS); margin-inline:auto then
       centres the grid in the section, so tiles stay centred even when a wide header (e.g. the
       mempool's "Epoch … / MEMPOOL") widens the section past one tile column. 100% is only the
       pre-layout fallback. */
    width: 100%;
    margin-inline: auto;
  }

  .tx-grid-item {
    position: absolute;
    width: 108px;
    transition: transform var(--flip-duration) ease;
    will-change: transform;
  }

  /* Per-epoch REWARDS capsule: same elevated surface, but a dashed edge and rounder radius
     set it apart from the solid blocks (blocks are borderless). */
  .reward-capsule {
    border-radius: var(--panel-radius-lg);
    border: 1px dashed rgb(255 255 255 / 0.18);
  }

  .reward-rows {
    display: flex;
    flex-direction: column;
  }

  .reward-row {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 2px;
    padding: 4px 0;
    text-align: center;
  }

  .reward-row + .reward-row {
    border-top: 1px solid rgb(255 255 255 / 0.12);
  }

  .reward-source {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 2px;
  }

  .reward-label {
    font-size: 10px;
    color: rgb(255 255 255 / 0.5);
  }

  .reward-pool {
    font-size: 13px;
    font-weight: 700;
    line-height: 1;
    text-decoration: none;
  }

  /* Same green as a positive stake change (Transaction.svelte). */
  .reward-amount {
    font-size: 12px;
    color: oklch(0.7 0.25 145);
    font-variant-numeric: tabular-nums;
  }
</style>
