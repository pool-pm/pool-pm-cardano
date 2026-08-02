<script lang="ts">
  import type { AssetInfo, DelegationInfo, FeedTx } from '../types';
  import type { Amount, Party, PartyKind } from '../intent';
  import { describeTx } from '../intent';
  import { metadataLines } from '../metadata';
  import { config, pool, drep, stake, address } from '../stores';
  import { poolColor, formatTicker, TX_WIDTH } from '../layout';
  import { fitFontSize, fontsAreReady } from '../fit.svelte';
  import { nonChangeOutputs as computeNonChangeOutputs } from '../change';
  import { stakeCredential, rewardCredential } from '../bech32';
  import { dappForAddress } from '../dapps';
  import { assetLabel } from '../assetName';

  // On a stake or address feed, highlight inputs/outputs belonging to the feed's
  // subject (stake feed: any address sharing the credential, incl. handles;
  // address feed: the exact address) with the info-circle color.
  const feedStakeCred = $derived($stake ? rewardCredential($stake.stake_address) : null);
  const ownedColor = $derived($stake ? poolColor($stake.stake_address) : $address ? poolColor($address.address) : null);
  function ownedAddressColor(addr: string | null | undefined): string | null {
    if (!addr) return null;
    // Reward (stake1…) addresses — e.g. a withdrawal's pseudo-input — carry their
    // credential directly, so go through ownedStakeColor.
    if (addr.startsWith('stake')) return ownedStakeColor(addr);
    if (feedStakeCred) return stakeCredential(addr) === feedStakeCred ? ownedColor : null;
    if ($address) return addr === $address.address ? ownedColor : null;
    return null;
  }
  // Same idea for a reward (stake1…) address — e.g. the delegator in a delegation
  // change. Reward addresses need rewardCredential() (bytes 1-28), not
  // stakeCredential() (bytes 29-56, payment addresses); colors it as the feed
  // subject when it shares the feed's stake credential, else null (grey).
  function ownedStakeColor(stakeAddr: string | null | undefined): string | null {
    if (!stakeAddr) return null;
    const cred = rewardCredential(stakeAddr);
    if (!cred) return null;
    const feedCred = feedStakeCred ?? ($address ? stakeCredential($address.address) : null);
    return cred === feedCred ? ownedColor : null;
  }

  // Link an address to its feed: addr1…/stake1… have one; Byron and unresolved
  // addresses don't, so they render as plain text.
  function addrHref(addr: string | null | undefined): string | undefined {
    if (!addr) return undefined;
    // A `$handle` (folded stake-address summary) links to its handle page.
    if (addr.startsWith('$')) return '/' + addr;
    return /^(addr1|addr_test1|stake1|stake_test1)/.test(addr) ? '/' + addr : undefined;
  }

  function addressLabel(address: string, handle?: string): string | null {
    if (handle) return '$' + handle;
    return dappForAddress(address)?.name ?? null;
  }

  // `folded` = the decluttered rendering used on a folded stake-change block (pool/DRep
  // feed): a delegation tx shows only its delegation change (no live_stake, no fee I/O);
  // any other stake-affecting tx shows only its net stake change + the account(s) it moved.
  let { tx, compact = false, folded = false }: { tx: FeedTx; compact?: boolean; folded?: boolean } = $props();
  /** Assets whose picture failed to load — named instead of hidden. Keyed by
   *  fingerprint, since the same asset can appear in more than one group. */
  let brokenThumbs = $state<Record<string, boolean>>({});

  // Above this rendered thumbnail size the art is already legible, so the hover
  // preview only kicks in for small (densely packed) thumbnails.
  const PREVIEW_MAX_THUMB = 64;

  function showPreview(e: MouseEvent) {
    if (thumbSize > PREVIEW_MAX_THUMB) return;
    const thumb = e.target as HTMLImageElement;
    let preview = document.getElementById('asset-preview') as HTMLImageElement;
    if (!preview) {
      preview = document.createElement('img');
      preview.id = 'asset-preview';
      preview.style.cssText =
        'position:fixed;width:128px;height:128px;object-fit:contain;border-radius:6px;z-index:1000;pointer-events:none;display:none';
      document.body.appendChild(preview);
    }
    preview.src = thumb.src;
    const rect = thumb.getBoundingClientRect();
    preview.style.left = `${rect.left + rect.width / 2 - 64}px`;
    preview.style.top = `${rect.top - 132}px`;
    preview.style.display = 'block';
  }

  function hidePreview() {
    const preview = document.getElementById('asset-preview');
    if (preview) preview.style.display = 'none';
  }

  function poolLabel(ticker?: string, poolId?: string): string {
    return formatTicker(ticker ?? poolId?.slice(5, 10) ?? '');
  }

  let visibleDelegations: DelegationInfo[] = $derived(
    (tx.delegations ?? []).filter((d) => d.from_pool_id || d.to_pool_id || d.from_drep_id || d.to_drep_id),
  );
  const hasDeleg = $derived(visibleDelegations.length > 0);
  const shownAnnotations = $derived(folded ? [] : (tx.annotations ?? []));

  // The subject's *own* governance vote survives folding: on a DRep feed a vote is the
  // headline content, exactly like a pool's own minted block on a pool feed (and an SPO
  // vote on a pool feed). Such a tx then renders the vote *only* — its ADA movement is
  // just the fee, so the stake change and the moved account would be noise.
  const feedSubjectId = $derived($drep?.drep_id ?? $pool?.pool_id ?? null);
  const ownVotes = $derived(feedSubjectId ? (tx.votes ?? []).filter((v) => v.voter_id === feedSubjectId) : []);
  const voteOnly = $derived(folded && ownVotes.length > 0);
  const shownVotes = $derived(folded ? ownVotes : (tx.votes ?? []));

  // Folded non-delegation stake-affecting tx: show just the account(s) it moved. The server
  // sends the *relevant* stake addresses in `tx.stake_addresses` (only the feed's delegators —
  // not every account in a multi-party tx), so we don't derive from all I/O here.
  const foldedStakeAddrs = $derived(folded && !hasDeleg && !voteOnly ? (tx.stake_addresses ?? []) : []);

  function formatAda(lovelace: string, sign?: string): string {
    const padded = lovelace.padStart(7, '0');
    const whole = padded.slice(0, -6) || '0';
    const frac = padded.slice(-6);
    const wholeNum = Number(whole);
    const s = sign ?? '';
    // U+202F (narrow no-break space): keeps the symbol tight against the amount and
    // stops a wrap from stranding the ₳ on its own line.
    const sym = '<span class="ada-sym">\u202f₳</span>';
    const dec = (d: string) => `<span class="ada-dec">.${d}</span>`;
    if (wholeNum >= 1000) return s + wholeNum.toLocaleString() + sym;
    if (wholeNum >= 1) {
      const trimmed = frac.slice(0, 2).replace(/0+$/, '');
      return trimmed ? s + whole + dec(trimmed) + sym : s + whole + sym;
    }
    const trimmed = frac.replace(/0+$/, '');
    return trimmed ? s + '0' + dec(trimmed) + sym : s + '0' + sym;
  }

  function compactNumber(n: number): string {
    if (n >= 1e15) return (n / 1e15).toFixed(1).replace(/\.0$/, '') + 'Q';
    if (n >= 1e12) return (n / 1e12).toFixed(1).replace(/\.0$/, '') + 'T';
    if (n >= 1e9) return (n / 1e9).toFixed(1).replace(/\.0$/, '') + 'B';
    if (n >= 1e6) return (n / 1e6).toFixed(1).replace(/\.0$/, '') + 'M';
    if (n >= 1e4) return (n / 1e3).toFixed(1).replace(/\.0$/, '') + 'K';
    return n.toLocaleString();
  }

  function formatAssetQuantity(quantity: string): string {
    const dot = quantity.indexOf('.');
    if (dot === -1) {
      return compactNumber(Number(quantity));
    }
    const whole = quantity.slice(0, dot);
    const frac = quantity.slice(dot + 1);
    const wholeNum = Number(whole);
    if (wholeNum >= 10000) return compactNumber(wholeNum);
    if (wholeNum >= 1000) return wholeNum.toLocaleString();
    if (wholeNum >= 1) {
      const trimmed = frac.slice(0, 2).replace(/0+$/, '');
      return trimmed ? wholeNum.toLocaleString() + '.' + trimmed : wholeNum.toLocaleString();
    }
    return '0.' + frac;
  }

  function nftcdnUrl(asset: AssetInfo): string {
    const base = `https://${asset.fingerprint}.${$config!.nftcdn}/preview`;
    return asset.tk ? `${base}?tk=${asset.tk}&size=${asset.size}` : `${base}?size=${asset.size}`;
  }

  // The plain-language reading of this tx, when it has one. `folded` blocks (pool/DRep
  // feeds) keep their own stake-centric rendering, and `describeTx` returns null for
  // anything it can't state confidently — both fall through to the raw I/O view below.
  const intent = $derived(folded ? null : describeTx(tx));
  const shownMetadata = $derived(metadataLines(tx.metadata));
  const shownTargets = $derived(intent ? intent.targets.slice(0, compact ? 2 : intent.targets.length) : []);
  const extraTargets = $derived(intent ? intent.hiddenTargets + (intent.targets.length - shownTargets.length) : 0);

  /**
   * An amount as it renders. A quantity with no unit is ADA and gets `formatAda`'s
   * markup; a unit with no quantity is an asset named without one — a swap's wanted
   * side, where the only figure on offer is a slippage floor rather than what will
   * actually arrive.
   */
  function amountText(amount: Amount): string {
    if (!amount.unit) return formatAda(amount.quantity ?? '0');
    if (amount.quantity === undefined) return amount.unit;
    return formatAssetQuantity(amount.quantity) + ' ' + amount.unit;
  }

  // The headline — the one line a reader should land on first — is set as large as the
  // tile allows. `.sentence`'s horizontal padding, both sides; keep in step with the
  // style block below.
  const SENTENCE_PADDING = 20;
  const HEADLINE_FAMILY = 'InterVariable, Inter, sans-serif';
  const HEADLINE_WEIGHT = 700;
  /** Past this it stops reading as an amount and starts reading as a banner. */
  const HEADLINE_MAX_PX = 20;
  /** Below this it's no longer the loudest thing on the tile, which defeats the point. */
  const HEADLINE_MIN_PX = 10;

  /**
   * The headline amount, and the size it fits at. `formatAda` emits markup for the
   * decimals and the ₳, so the size is fitted to the text those tags render as. The
   * decimals are set smaller (`.ada-dec`) than what's measured here, so a value with a
   * fractional part lands a shade under the true maximum rather than over it.
   */
  /** The size at which `text` fits the tile, as a headline. */
  function fitHeadline(text: string): number {
    return fitFontSize(text, HEADLINE_FAMILY, HEADLINE_WEIGHT, TX_WIDTH - SENTENCE_PADDING, {
      min: HEADLINE_MIN_PX,
      max: HEADLINE_MAX_PX,
    });
  }

  function rendered(amount: Amount) {
    const html = amountText(amount);
    return { html, plain: !!amount.unit, text: html.replace(/<[^>]*>/g, '') };
  }

  /**
   * The loud line, or the two loud lines: a swap has an amount on each side and neither
   * is the subordinate one — "40K NIGHT for ADA" is a single fact with two halves. They
   * take one shared size, the smaller of what each would fit at, because two amounts of
   * a pair rendered at different sizes read as a headline and a footnote.
   *
   * A target carrying an amount but no party is that counterpart; a target with a party
   * is a recipient, and its amount belongs beside the name at the ordinary size.
   */
  const headline = $derived.by(() => {
    const give = intent?.amount;
    if (!give) return null;
    const counterpart = intent?.targets.length === 1 && !intent.targets[0].party ? intent.targets[0].amount : undefined;
    const shown = rendered(give);
    const other = counterpart ? rendered(counterpart) : null;
    const size = Math.min(fitHeadline(shown.text), other ? fitHeadline(other.text) : HEADLINE_MAX_PX);
    return { give: shown, counterpart: other, size };
  });

  // Every fitted size on the tile changes at once when the webfont lands, and the grid
  // packs to measured heights — so tell it to measure again when that happens.
  let card: HTMLElement | undefined = $state();
  $effect(() => {
    void fontsAreReady();
    card?.dispatchEvent(new Event('remeasure', { bubbles: true }));
  });

  // A name is worth more whole and small than truncated at full size: `$long.handle.name`
  // beats `$long.hand…`, and a pool ticker or dApp name says nothing once it's clipped.
  // So a label shrinks to fit rather than being ellipsised — down to a floor, past which
  // it stops being readable and the ellipsis is the better answer after all.
  const LABEL_MIN_PX = 7;
  const LABEL_SIZES: Record<PartyKind, number> = {
    handle: 10,
    app: 10,
    pool: 11,
    drep: 10,
    address: 10,
  };
  const MONO_FAMILY = "'SF Mono', 'Cascadia Code', 'Fira Code', Consolas, monospace";

  /** Up to this many tokens in a group get their name spelled out under the thumbnail.
   *  Art identifies an NFT on its own; a fungible token's ticker is the only thing that
   *  does, and a burn has nothing else to go on. */
  const NAMED_ASSETS_MAX = 2;

  function labelSize(party: Party): number {
    const max = LABEL_SIZES[party.kind];
    const family = party.kind === 'address' ? MONO_FAMILY : HEADLINE_FAMILY;
    const weight = party.kind === 'pool' ? 700 : party.kind === 'address' ? 400 : 600;
    return fitFontSize(party.label, family, weight, TX_WIDTH - SENTENCE_PADDING, { min: LABEL_MIN_PX, max });
  }

  // A party takes the feed subject's colour when it *is* the subject (so you can see
  // your own side of a tx at a glance); pools and DReps always carry their own.
  function partyColor(party: Party): string | null {
    if (party.kind === 'pool' || party.kind === 'drep') return poolColor(party.id);
    if (!party.id) return null;
    return party.id.startsWith('stake') ? ownedStakeColor(party.id) : ownedAddressColor(party.id);
  }

  let maxOutputs = $derived(compact ? 2 : 8);
  let maxInputs = $derived(compact ? 2 : 8);
  let maxAssets = $derived(compact ? 10 : 50);
  let maxAssetsPerOutput = $derived(compact ? 5 : 25);

  let nonChangeOutputs = $derived(computeNonChangeOutputs(tx.inputs, tx.outputs));

  // Total asset count → scale thumbnails, from whichever rendering is in use.
  let totalAssets = $derived(
    intent
      ? (intent.assets?.length ?? 0) + shownTargets.reduce((sum, t) => sum + (t.assets?.length ?? 0), 0)
      : nonChangeOutputs.reduce((sum, o) => sum + o.assets.length, 0),
  );
  let thumbSize = $derived(totalAssets <= 1 ? 96 : Math.max(16, Math.floor(96 / Math.sqrt(totalAssets))));
  let sortedOutputs = $derived([...nonChangeOutputs].sort((a, b) => Number(BigInt(b.lovelace) - BigInt(a.lovelace))));
  let visibleOutputs = $derived.by(() => {
    let assets = 0;
    let count = 0;
    for (const o of sortedOutputs) {
      if (count >= maxOutputs) break;
      if (assets >= maxAssets) break;
      assets += o.assets.length;
      count++;
    }
    return sortedOutputs.slice(0, count);
  });
  let hiddenOutputCount = $derived(nonChangeOutputs.length - visibleOutputs.length);

  // Deduplicate inputs by address
  let uniqueInputs = $derived([...new Map(tx.inputs.map((i) => [i.address, i])).values()]);
  let visibleInputs = $derived(uniqueInputs.slice(0, maxInputs));
  let hiddenInputCount = $derived(uniqueInputs.length - visibleInputs.length);
</script>

<!-- One named side of a sentence: the label, coloured by what it stands for, linked to
     its own feed when it has one. -->
{#snippet partyLine(party: Party)}
  <svelte:element
    this={party.href ? 'a' : 'span'}
    href={party.href}
    class="party"
    class:mono={party.kind === 'address'}
    class:handle={party.kind === 'handle'}
    class:app={party.kind === 'app'}
    class:pool={party.kind === 'pool'}
    class:drep={party.kind === 'drep'}
    class:former={party.former}
    style:color={partyColor(party)}
    style:font-size="{labelSize(party)}px">{party.label}</svelte:element
  >
{/snippet}

<!-- `headline` is the loud line under the verb; the small form sits next to a target. -->
{#snippet amountArt(asset: AssetInfo)}
  {#if $config && !brokenThumbs[asset.fingerprint]}
    <a class="asset-link" href="/{asset.fingerprint}">
      <img
        class="swap-art"
        src={nftcdnUrl(asset)}
        alt={asset.fingerprint}
        loading="lazy"
        onload={(e: Event) => {
          (e.target as HTMLElement).dispatchEvent(new Event('remeasure', { bubbles: true }));
        }}
        onerror={(e: Event) => {
          brokenThumbs = { ...brokenThumbs, [asset.fingerprint]: true };
          (e.target as HTMLElement).dispatchEvent(new Event('remeasure', { bubbles: true }));
        }}
        onmouseenter={showPreview}
        onmouseleave={hidePreview}
      />
    </a>
  {/if}
{/snippet}

{#snippet amountLine(amount: Amount)}
  {#if amount.image}{@render amountArt(amount.image)}{/if}
  {#if amount.unit}
    <span class="amount">{amountText(amount)}</span>
  {:else}
    <span class="amount">{@html amountText(amount)}</span>
  {/if}
{/snippet}

<!-- Asset thumbnails, which fall back to naming the asset when it has no picture.
     Not every asset is art: a protocol's state token has no image at all, and hiding it
     on a failed load left a mint reading "+1 asset" — a tally where the thing itself
     should be. The fingerprint always identifies it even when the on-chain name is bytes
     that don't render, so something identifying is always shown. -->
{#snippet assetThumbs(assets: AssetInfo[])}
  {@const visibleCount = Math.min(assets.length, maxAssetsPerOutput)}
  <div class="assets">
    {#each assets.slice(0, visibleCount) as asset}
      {@const broken = brokenThumbs[asset.fingerprint]}
      <div class="asset">
        {#if !broken}
          <a class="asset-link" href="/{asset.fingerprint}">
            <img
              class="asset-thumb"
              src={nftcdnUrl(asset)}
              alt={asset.fingerprint}
              loading="lazy"
              onload={(e: Event) => {
                (e.target as HTMLElement).dispatchEvent(new Event('remeasure', { bubbles: true }));
              }}
              onerror={(e: Event) => {
                brokenThumbs = { ...brokenThumbs, [asset.fingerprint]: true };
                (e.target as HTMLElement).dispatchEvent(new Event('remeasure', { bubbles: true }));
              }}
              onmouseenter={showPreview}
              onmouseleave={hidePreview}
            />
          </a>
        {/if}
        {#if broken || (thumbSize >= 32 && asset.quantity !== '1')}
          <span class="asset-label">{formatAssetQuantity(asset.quantity)}</span>
        {/if}
        {#if broken || (asset.name && assets.length <= NAMED_ASSETS_MAX)}
          <a class="asset-name" href="/{asset.fingerprint}">{assetLabel(asset)}</a>
        {/if}
      </div>
    {/each}
  </div>
  {@const hiddenAssets = assets.length - visibleCount}
  {#if hiddenAssets > 0}
    <span class="more-outputs">+{hiddenAssets} asset{hiddenAssets > 1 ? 's' : ''}</span>
  {/if}
{/snippet}

<div class="tx-card" bind:this={card} style:--thumb-size="{thumbSize}px">
  {#if tx.stake_change && !voteOnly}
    {@const negative = tx.stake_change.startsWith('-')}
    <div class="stake-change" style:color={negative ? 'oklch(0.7 0.25 25)' : 'oklch(0.7 0.25 145)'}>
      {@html formatAda(negative ? tx.stake_change.slice(1) : tx.stake_change, negative ? '−' : '+')}
    </div>
  {/if}
  {#if folded && foldedStakeAddrs.length > 0}
    <!-- Folded non-delegation tx: just the account(s) it moved. -->
    <div class="deleg-section">
      <div class="addr-list">
        {#each foldedStakeAddrs as a (a)}
          <div class="addr-item">
            <svelte:element
              this={addrHref(a) ? 'a' : 'span'}
              href={addrHref(a)}
              style:color={ownedStakeColor(a)}
              class="addr mono">{a}</svelte:element
            >
          </div>
        {/each}
      </div>
    </div>
  {/if}
  {#if !folded && shownMetadata.length > 0 && !intent?.messageRead}
    <div class="msg-section">
      {#each shownMetadata as line}
        <span class="msg-line">{line}</span>
      {/each}
    </div>
  {/if}
  {#if shownVotes.length > 0}
    <div class="vote-section" class:vote-only={voteOnly}>
      {#each shownVotes as vote}
        <div class="vote-item">
          <!-- On the voter's own folded feed the name is the page itself — drop it. -->
          {#if !voteOnly}
            <span class="vote-voter">{vote.voter_name ?? vote.voter_id.slice(0, 12)}</span>
          {/if}
          voted
          <span
            class="vote-badge"
            class:yes={vote.vote === 'Yes'}
            class:no={vote.vote === 'No'}
            class:abstain={vote.vote === 'Abstain'}>{vote.vote}</span
          >
          {#if vote.action_title}
            to <span class="vote-action">{vote.action_title}</span>
          {/if}
        </div>
      {/each}
    </div>
  {/if}
  {#if intent}
    <div class="sentence">
      {#if intent.subject}{@render partyLine(intent.subject)}{/if}
      <span class="verb">{intent.verb}</span>
      {#if headline}
        {#if intent.amount?.image}{@render amountArt(intent.amount.image)}{/if}
        <span class="amount headline" style:font-size="{headline.size}px">
          {#if headline.give.plain}{headline.give.html}{:else}{@html headline.give.html}{/if}
        </span>
      {/if}
      {#if intent.assets && $config}{@render assetThumbs(intent.assets)}{/if}
      {#if intent.note}
        {#each intent.note as line}
          <span class="note">{line}</span>
        {/each}
      {/if}
      {#if intent.preposition}<span class="prep">{intent.preposition}</span>{/if}
      {#each shownTargets as target}
        <div class="target">
          {#if headline?.counterpart && !target.party}
            {#if target.amount?.image}{@render amountArt(target.amount.image)}{/if}
            <span class="amount headline" style:font-size="{headline.size}px">
              {#if headline.counterpart.plain}{headline.counterpart.html}{:else}{@html headline.counterpart.html}{/if}
            </span>
          {:else if target.amount}{@render amountLine(target.amount)}{/if}
          {#if target.assets && $config}{@render assetThumbs(target.assets)}{/if}
          {#if target.party}{@render partyLine(target.party)}{/if}
        </div>
      {/each}
      {#if extraTargets > 0}
        <span class="more-outputs">+{extraTargets} more</span>
      {/if}
      {#if intent.via}
        <span class="prep">ON</span>
        {@render partyLine(intent.via)}
      {/if}
      {#if tx.hash}
        <div class="tx-hash mono">{tx.hash}</div>
      {/if}
    </div>
  {:else}
    {#if visibleDelegations.length > 0}
      <div class="deleg-section">
        <div class="addr-list">
          {#each visibleDelegations as deleg}
            {@const isDeregistration =
              !deleg.to_pool_id && !deleg.to_drep_id && (!!deleg.from_pool_id || !!deleg.from_drep_id)}
            {@const hasFrom = !!(deleg.from_pool_id || deleg.from_drep_id)}
            <div class="addr-item">
              {#if deleg.to_pool_id}
                <span class="deleg-kind">POOL</span>
                <a class="deleg-pool" style:color={poolColor(deleg.to_pool_id)} href="/{deleg.to_pool_id}"
                  >{poolLabel(deleg.to_ticker, deleg.to_pool_id)}</a
                >
              {/if}
              {#if deleg.to_drep_id}
                <span class="deleg-kind">DREP</span>
                <a class="deleg-drep" style:color={poolColor(deleg.to_drep_id)} href="/{deleg.to_drep_id}"
                  >{deleg.to_drep_name ?? deleg.to_drep_id.slice(5, 13)}</a
                >
              {/if}
              {#if deleg.to_pool_id || deleg.to_drep_id}
                <span class="deleg-arrow" style:color={ownedStakeColor(deleg.stake_address)}>{@html '&#x2191;'}</span>
              {/if}
              {#if deleg.from_pool_id}
                <span class="deleg-kind">POOL</span>
                <a
                  class="deleg-pool"
                  class:deregistered={isDeregistration}
                  style:color={poolColor(deleg.from_pool_id)}
                  href="/{deleg.from_pool_id}">{poolLabel(deleg.from_ticker, deleg.from_pool_id)}</a
                >
              {/if}
              {#if deleg.from_drep_id}
                <span class="deleg-kind">DREP</span>
                <a
                  class="deleg-drep"
                  class:deregistered={isDeregistration}
                  style:color={poolColor(deleg.from_drep_id)}
                  href="/{deleg.from_drep_id}">{deleg.from_drep_name ?? deleg.from_drep_id.slice(5, 13)}</a
                >
              {/if}
              <div class="stake-group" class:spaced={hasFrom}>
                {#if !folded}
                  <span class="ada">{@html formatAda(deleg.live_stake)}</span>
                {/if}
                <svelte:element
                  this={addrHref(deleg.stake_address) ? 'a' : 'span'}
                  href={addrHref(deleg.stake_address)}
                  style:color={ownedStakeColor(deleg.stake_address)}
                  class="addr mono">{deleg.stake_address}</svelte:element
                >
              </div>
            </div>
          {/each}
        </div>
      </div>
    {/if}
    {#if !folded && tx.catalyst}
      <div class="deleg-section">
        <div class="addr-list">
          <div class="addr-item">
            <span class="catalyst-label">Catalyst voting registration</span>
            <div class="stake-group spaced">
              {#if tx.catalyst.live_stake}
                <span class="ada">{@html formatAda(tx.catalyst.live_stake)}</span>
              {/if}
              <svelte:element
                this={addrHref(tx.catalyst.stake_address) ? 'a' : 'span'}
                href={addrHref(tx.catalyst.stake_address)}
                style:color={ownedStakeColor(tx.catalyst.stake_address)}
                class="addr mono">{tx.catalyst.stake_address}</svelte:element
              >
            </div>
          </div>
        </div>
      </div>
    {/if}
    {#each shownAnnotations as ann}
      {#if ann.kind === 'oracle'}
        <div class="annotation">
          <span class="annotation-label">{ann.source} price feed</span>
          {#if ann.value}
            <span class="oracle-value">
              {ann.feed ? `1 ${ann.feed.split('/')[0]} = ${ann.value}` : ann.value}
            </span>
          {/if}
        </div>
      {/if}
    {/each}

    {#if !folded && (tx.inputs.length > 0 || tx.outputs.length > 0)}
      <div class="tx-body">
        <div class="addr-list">
          {#each visibleOutputs as output}
            <div class="addr-item">
              <span class="ada">{@html formatAda(output.lovelace)}</span>
              {#if output.assets.length > 0 && $config}
                {@render assetThumbs(output.assets)}
              {/if}
              {#if addressLabel(output.address, output.handle)}
                <svelte:element
                  this={addrHref(output.address) ? 'a' : 'span'}
                  href={addrHref(output.address)}
                  class="addr mono label"
                  style:color={ownedAddressColor(output.address)}
                  >{addressLabel(output.address, output.handle)}</svelte:element
                >
              {:else}
                <svelte:element
                  this={addrHref(output.address) ? 'a' : 'span'}
                  href={addrHref(output.address)}
                  class="addr mono"
                  style:color={ownedAddressColor(output.address)}>{output.address}</svelte:element
                >
              {/if}
            </div>
          {/each}
          {#if visibleOutputs.length === 0}
            <span class="ada"
              >{@html formatAda(tx.outputs.reduce((s, o) => s + BigInt(o.lovelace), 0n).toString())}</span
            >
          {/if}
          {#if hiddenOutputCount > 0}
            <span class="more-outputs">+{hiddenOutputCount} output{hiddenOutputCount > 1 ? 's' : ''}</span>
          {/if}
        </div>
        <div class="arrow" class:flip={visibleOutputs.length === 0}>{visibleOutputs.length === 0 ? '↻' : '↑'}</div>

        <div class="addr-list">
          {#each visibleInputs as input}
            <div class="addr-item">
              {#if addressLabel(input.address ?? '', input.handle)}
                <svelte:element
                  this={addrHref(input.address) ? 'a' : 'span'}
                  href={addrHref(input.address)}
                  class="addr mono label"
                  style:color={ownedAddressColor(input.address)}
                  >{addressLabel(input.address ?? '', input.handle)}</svelte:element
                >
              {:else}
                <svelte:element
                  this={addrHref(input.address) ? 'a' : 'span'}
                  href={addrHref(input.address)}
                  class="addr mono"
                  style:color={ownedAddressColor(input.address)}>{input.address ?? '???'}</svelte:element
                >
              {/if}
            </div>
          {/each}
          {#if hiddenInputCount > 0}
            <span class="more-outputs">+{hiddenInputCount} input{hiddenInputCount > 1 ? 's' : ''}</span>
          {/if}
        </div>
        {#if tx.hash}
          <div class="tx-hash mono">{tx.hash}</div>
        {/if}
      </div>
    {/if}
  {/if}
</div>

<style>
  .tx-card {
    width: var(--item-width);
    font-size: 11px;
    text-align: center;
    transition: filter var(--flip-duration) ease;
  }

  .stake-change {
    background: rgb(0 0 0 / 0.6);
    border-radius: 6px;
    padding: 8px 10px;
    font-size: 13px;
    font-weight: 700;
  }

  .msg-section {
    background: rgb(0 0 0 / 0.6);
    border-radius: 6px;
    padding: 8px 10px;
  }

  .msg-line {
    display: block;
    font-size: 10px;
    color: rgb(255 255 255 / 0.8);
    word-break: break-word;
  }

  .vote-section {
    background: rgb(0 0 0 / 0.6);
    border-radius: 6px;
    padding: 8px 10px;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  /* The subject's own vote on a folded block: it *is* the block's meaning (the DRep-feed
     counterpart of a pool's own minted block), so it reads louder than the vote line
     shown inline inside an unfolded tx. */
  .vote-section.vote-only .vote-item {
    font-size: 12px;
  }

  .vote-section.vote-only .vote-badge {
    font-size: 11px;
  }

  .vote-section.vote-only .vote-action {
    color: rgb(255 255 255 / 0.6);
  }

  .vote-item {
    display: flex;
    align-items: baseline;
    justify-content: center;
    gap: 6px;
    font-size: 10px;
    flex-wrap: wrap;
  }

  .vote-badge {
    font-weight: 700;
    font-size: 9px;
    padding: 1px 4px;
    border-radius: 3px;
    text-transform: uppercase;
    flex-shrink: 0;
  }

  .vote-badge.yes,
  .vote-badge.no,
  .vote-badge.abstain {
    color: #111;
    background: rgb(255 255 255 / 0.5);
  }

  .vote-voter {
    color: rgb(255 255 255 / 0.7);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .vote-action {
    color: rgb(255 255 255 / 0.4);
    overflow-wrap: anywhere;
    text-align: center;
  }

  .deleg-section {
    background: rgb(0 0 0 / 0.6);
    border-radius: 6px;
    padding: 8px 10px;
  }

  /* The plain-language reading of a tx, stacked one clause per line: subject, verb,
     amount, preposition, object. The tile is 108px wide, so every line has to stand on
     its own — which is why the verb is a word and not an arrow. */
  .sentence {
    background: rgb(0 0 0 / 0.6);
    border-radius: 6px;
    padding: 8px 10px;
    display: flex;
    flex-direction: column;
    align-items: center;
    min-width: 0;
  }

  /* Connective tissue. Small and dim on purpose: they carry the grammar, not the
     information, and must never compete with the amount or the parties. */
  .verb,
  .prep {
    font-size: 8px;
    font-weight: 600;
    letter-spacing: 0.5px;
    line-height: 1.5;
    color: rgb(255 255 255 / 0.35);
  }

  .verb {
    color: rgb(255 255 255 / 0.55);
  }

  .amount {
    color: white;
    font-weight: 600;
    font-size: 10px;
    line-height: 1.2;
  }

  /* The one line a casual reader should land on first. Its size is measured to fit the
     tile (see `headline`), so the family and weight have to match what was measured;
     nowrap keeps a long number on one line rather than breaking it at a separator. */
  .amount.headline {
    font-family: InterVariable, Inter, sans-serif;
    font-weight: 700;
    white-space: nowrap;
  }

  .amount :global(.ada-dec) {
    font-weight: 400;
    font-size: 0.72em;
  }

  /* Size comes from `labelSize`, which fits the label to the tile; the ellipsis is the
     floor's safety net, for a name too long to fit even at the minimum. */
  .party {
    line-height: 1.3;
    color: rgb(255 255 255 / 0.4);
    max-width: 100%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    text-decoration: none;
    /* Same underscore-clipping fix as `.addr`. */
    padding-bottom: 2px;
    margin-bottom: -2px;
  }

  a.party {
    cursor: pointer;
  }
  a.party:hover {
    text-decoration: underline;
  }

  /* A name someone chose — a handle or a dApp — reads at full strength; a bare address
     stays dim, since it identifies without meaning anything. */
  .party.handle,
  .party.app {
    color: white;
  }

  .party.app {
    font-weight: 600;
    letter-spacing: 0.3px;
  }

  .party.pool {
    font-family: Inter, sans-serif;
    font-weight: 700;
    color: white;
  }

  .party.drep {
    font-family: Inter, sans-serif;
    font-weight: 600;
    color: white;
  }

  .party.former {
    text-decoration: line-through;
  }

  /* The tx's own words, when they are the point rather than a caption on a payment. */
  .note {
    font-size: 11px;
    color: white;
    text-align: center;
    overflow-wrap: anywhere;
    line-height: 1.3;
  }

  .target {
    display: flex;
    flex-direction: column;
    align-items: center;
    min-width: 0;
    max-width: 100%;
  }

  .tx-body {
    background: rgb(0 0 0 / 0.6);
    border-radius: 6px;
    padding: 8px 10px;
  }

  .tx-hash {
    color: var(--section-color, var(--accent));
    font-size: 10px;
    margin-top: 4px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 8ch;
    margin-inline: auto;
  }

  .addr-list {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 4px;
    min-width: 0;
    width: 100%;
  }

  .addr-item {
    display: flex;
    flex-direction: column;
    align-items: center;
    min-width: 0;
    max-width: 100%;
  }

  .ada {
    color: white;
    font-weight: 600;
    font-size: 10px;
  }

  .ada :global(.ada-dec) {
    font-weight: 400;
    font-size: 9px;
  }

  .addr {
    color: rgb(255 255 255 / 0.4);
    font-size: 10px;
    max-width: 100%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    /* `overflow: hidden` is here for the ellipsis, but it clips vertically too, and at 10px the
       mono underscore is painted on the very bottom edge of the line box — `$lynx_kpo` read as
       "$lynx kpo". The clip region is the padding box, so padding pushes it down past the glyph
       and the negative margin gives the space back: same outer height, same tile layout. */
    padding-bottom: 2px;
    margin-bottom: -2px;
  }

  .addr.label {
    color: white;
  }

  a.addr {
    text-decoration: none;
    cursor: pointer;
  }
  a.addr:hover {
    text-decoration: underline;
  }

  .arrow {
    color: var(--section-color, var(--accent));
    text-align: center;
    font-size: 12px;
    line-height: 1;
    margin: 2px 0;
  }

  .arrow.flip {
    transform: rotate(120deg);
  }

  .deleg-pool {
    font-family: Inter, sans-serif;
    font-size: 11px;
    font-weight: 700;
    text-decoration: none;
    color: white;
  }

  .deleg-pool.deregistered,
  .deleg-drep.deregistered {
    text-decoration: line-through;
  }

  .deleg-drep {
    font-family: Inter, sans-serif;
    font-size: 10px;
    font-weight: 600;
    text-decoration: none;
    color: white;
  }

  .deleg-arrow {
    color: rgb(255 255 255 / 0.4);
    font-size: 12px;
    line-height: 1;
  }

  /* Stake value + address; centered column like the rest of the delegation item. */
  .stake-group {
    display: flex;
    flex-direction: column;
    align-items: center;
    min-width: 0;
    max-width: 100%;
  }
  /* Separate the stake value/address from the change/registration above it (only
     when there's a previous target, i.e. a real from→to change, or a registration). */
  .stake-group.spaced {
    margin-top: 6px;
  }

  /* Match the CIP-20 message text style (not bold). */
  .catalyst-label {
    font-size: 10px;
    color: rgb(255 255 255 / 0.8);
    word-break: break-word;
  }

  /* Generic panel for a recognized protocol annotation (oracle, …). Same dark card as
     .deleg-section, but a centered column owning its own caption + value. */
  .annotation {
    background: rgb(0 0 0 / 0.6);
    border-radius: 6px;
    padding: 8px 10px;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 2px;
    min-width: 0;
  }

  /* Grey caption above an annotation's value, like the CIP-20 message text. */
  .annotation-label {
    font-size: 10px;
    color: rgb(255 255 255 / 0.8);
    word-break: break-word;
  }

  .oracle-value {
    font-size: 11px;
    color: white;
    white-space: nowrap;
  }

  /* Grey caption above each pool/DRep target, like the stake-address grey. */
  .deleg-kind {
    color: rgb(255 255 255 / 0.4);
    font-size: 8px;
    font-weight: 600;
    letter-spacing: 0.5px;
    line-height: 1.2;
    margin-top: 3px;
  }

  .assets {
    display: flex;
    flex-wrap: wrap;
    gap: 2px;
    justify-content: center;
  }

  .asset {
    display: flex;
    flex-direction: column;
    align-items: center;
  }

  /* Transparent wrapper: links the thumbnail to its asset page without
     affecting the .asset flex layout. */
  .asset-link {
    display: contents;
  }

  /* The token's on-chain name, under its art. */
  .asset-name {
    font-size: 9px;
    color: rgb(255 255 255 / 0.75);
    text-align: center;
    max-width: 100%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .asset-label {
    font-size: 9px;
    color: white;
    text-align: center;
    white-space: nowrap;
  }

  .asset-thumb {
    max-width: var(--thumb-size, 96px);
    max-height: var(--thumb-size, 96px);
    align-self: center;
    border-radius: 3px;
    background: transparent;
  }

  /* A swap side's art. Fixed and small: the sentence already carries the amount and the
     ticker, so this is there to be recognised at a glance, not read. */
  .swap-art {
    width: 36px;
    height: 36px;
    object-fit: contain;
    align-self: center;
    border-radius: 3px;
    background: transparent;
  }

  .more-outputs {
    color: rgb(255 255 255 / 0.4);
    font-size: 10px;
  }
</style>
