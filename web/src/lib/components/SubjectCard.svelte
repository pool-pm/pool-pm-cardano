<script lang="ts">
  import type { PoolInfo, DRepInfo, StakeInfo, AddressInfo, CardanoInfo } from '../types';
  import { poolColor, formatTicker, formatAda, formatVotes } from '../layout';
  import { config } from '../stores';

  // Network magic numbers (Pallas GenesisValues) → homepage card name. Mainnet keeps
  // "CARDANO"; the testnets show their name so it's obvious which chain is loaded.
  const PREPROD_MAGIC = 1;
  const PREVIEW_MAGIC = 2;
  const networkName = $derived(
    $config?.magic === PREPROD_MAGIC ? 'PREPROD' : $config?.magic === PREVIEW_MAGIC ? 'PREVIEW' : 'CARDANO',
  );

  // The subject header card, shared by the feed pages and the assets page. Exactly
  // one of pool/drep/stake/address/cardano is set (the page's subject); the card
  // renders nothing when none is. `landscape` switches to the center-right column
  // layout used by the feed in landscape orientation.
  let {
    pool = null,
    drep = null,
    stake = null,
    address = null,
    cardano = null,
    landscape = false,
  }: {
    pool?: PoolInfo | null;
    drep?: DRepInfo | null;
    stake?: StakeInfo | null;
    address?: AddressInfo | null;
    cardano?: CardanoInfo | null;
    landscape?: boolean;
  } = $props();

  function formatMargin(m: number): string {
    return (m * 100).toFixed(2).replace(/\.?0+$/, '') + '%';
  }
</script>

{#if pool}
  {@const color = poolColor(pool.pool_id)}
  <div class="subject-card" class:landscape style:--subject-color={color}>
    <span class="pool-name" style:color>{formatTicker(pool.ticker ?? pool.pool_id.slice(5, 10))}</span>
    <span class="pool-stake">{formatAda(pool.live_stake)}</span>
    <!-- The delegator count opens the delegators grid (nothing to show at zero). -->
    <span class="pool-delegators">
      {#if pool.delegators > 0}
        <a class="delegators-link" href="/{pool.pool_id}/delegators">{pool.delegators.toLocaleString()} delegators</a>
      {:else}
        {pool.delegators.toLocaleString()} delegators
      {/if}
      · {pool.blocks.toLocaleString()} blocks
    </span>
    <div class="pool-params pool-stats">
      <div class="pool-param">
        <span class="pool-param-label">margin</span>
        <span class="pool-param-value">{formatMargin(pool.margin)}</span>
      </div>
      <div class="pool-param">
        <span class="pool-param-label">pledge</span>
        <span class="pool-param-value">{formatAda(pool.pledge)}</span>
      </div>
      <div class="pool-param">
        <span class="pool-param-label">cost</span>
        <span class="pool-param-value">{formatAda(pool.fixed_cost)}</span>
      </div>
    </div>
  </div>
{:else if drep}
  {@const color = poolColor(drep.drep_id)}
  <div class="subject-card" class:landscape style:--subject-color={color}>
    <span class="drep-name" style:color>{drep.given_name ?? drep.drep_id.slice(5, 13)}</span>
    <span class="pool-stake">{formatAda(drep.live_stake)}</span>
    <!-- Votes are to a DRep what minted blocks are to a pool, but with the participation %
         appended the pair no longer fits the card's width — so they stack, one fact per line,
         instead of wrapping mid-figure ("148 votes" / "(98%)"). -->
    <span class="pool-delegators stacked">
      <span class="fact">
        {#if drep.delegators > 0}
          <a class="delegators-link" href="/{drep.drep_id}/delegators">{drep.delegators.toLocaleString()} delegators</a>
        {:else}
          {drep.delegators.toLocaleString()} delegators
        {/if}
      </span>
      <span class="fact">{formatVotes(drep.votes ?? 0, drep.eligible)}</span>
    </span>
  </div>
{:else if stake}
  {@const color = poolColor(stake.stake_address)}
  {@const total = (BigInt(stake.balance ?? '0') + BigInt(stake.rewards ?? '0')).toString()}
  <div class="subject-card" class:landscape style:--subject-color={color}>
    <span class="stake-address" style:color title={stake.stake_address}>{stake.stake_address}</span>
    <a class="pool-stake" href="/{stake.stake_address}">{formatAda(total)}</a>
    {#if stake.rewards && stake.rewards !== '0'}
      <span class="pool-delegators">incl. {formatAda(stake.rewards)} rewards</span>
    {/if}
    <div class="pool-params">
      {#if stake.pool_id}
        <div class="pool-param">
          <span class="pool-param-label">pool</span>
          <a
            class="pool-param-value stake-link"
            style:color={poolColor(stake.pool_id)}
            href="/{stake.pool_id}"
            title={stake.pool_ticker ?? stake.pool_id}
            >{formatTicker(stake.pool_ticker ?? stake.pool_id.slice(5, 10))}</a
          >
        </div>
      {/if}
      {#if stake.drep_id}
        <div class="pool-param">
          <span class="pool-param-label">drep</span>
          <a
            class="pool-param-value stake-link"
            style:color={poolColor(stake.drep_id)}
            href="/{stake.drep_id}"
            title={stake.drep_name ?? stake.drep_id}>{stake.drep_name ?? stake.drep_id}</a
          >
        </div>
      {/if}
      <div class="pool-param">
        <span class="pool-param-label">assets</span>
        {#if stake.assets_count > 0}
          <a class="pool-param-value stake-link" style:color href="/{stake.stake_address}/assets"
            >{stake.assets_count}</a
          >
        {:else}
          <span class="pool-param-value">{stake.assets_count}</span>
        {/if}
      </div>
    </div>
  </div>
{:else if address}
  {@const color = poolColor(address.address)}
  <div class="subject-card" class:landscape style:--subject-color={color}>
    <span class="stake-address" style:color title={address.address}>{address.address}</span>
    {#if address.handle}
      <span class="handle"><span class="handle-dollar">$</span>{address.handle}</span>
    {/if}
    {#if address.balance}
      <a class="pool-stake" href="/{address.address}">{formatAda(address.balance)}</a>
    {/if}
    <div class="pool-params">
      {#if address.assets_count > 0}
        <a class="pool-param pool-param-link" href="/{address.address}/assets">
          <span class="pool-param-label">assets</span>
          <span class="pool-param-value">{address.assets_count}</span>
        </a>
      {:else}
        <div class="pool-param">
          <span class="pool-param-label">assets</span>
          <span class="pool-param-value">{address.assets_count}</span>
        </div>
      {/if}
      {#if address.stake_address && address.stake_assets_count && address.stake_assets_count !== address.assets_count}
        <a class="pool-param pool-param-link" href="/{address.stake_address}/assets">
          <span class="pool-param-label">stake assets</span>
          <span class="pool-param-value">{address.stake_assets_count}</span>
        </a>
      {/if}
      {#if address.stake_address}
        <a class="pool-param pool-param-link" href="/{address.stake_address}" title={address.stake_address}>
          <span class="pool-param-label">stake</span>
          <span class="pool-param-value">{formatAda(address.stake_value ?? '0')}</span>
        </a>
      {/if}
    </div>
  </div>
{:else if cardano}
  <div class="subject-card" class:landscape style:--subject-color={'white'}>
    <span class="pool-name" style:color="white">{networkName}</span>
    <span class="pool-stake">{formatAda(cardano.circulation)}</span>
    <div class="pool-params pool-stats">
      <div class="pool-param">
        <span class="pool-param-label">pools</span>
        <span class="pool-param-value">{cardano.pool_count.toLocaleString()}</span>
      </div>
      <div class="pool-param">
        <span class="pool-param-label">staked</span>
        <span class="pool-param-value">{cardano.staked_percent}%</span>
      </div>
      <div class="pool-param">
        <span class="pool-param-label">dreps</span>
        <span class="pool-param-value">{cardano.drep_count.toLocaleString()}</span>
      </div>
    </div>
  </div>
{/if}

<style>
  /* Subject header: compact glass card with a subject-color ridge on top and a
     soft radial glow behind. Centered at the top in portrait (narrow enough to
     clear the corner logo/search buttons); a center-right column in landscape. */
  .subject-card {
    width: 290px;
    max-width: calc(100vw - 32px);
    border-radius: var(--panel-radius-lg);
    /* Pure flat: the darker surface (the mempool tx-chip tone), solid — no gradient or glow.
       The one accent is a flat subject-colour ridge (top + bottom) identifying the feed. */
    background: var(--surface-2);
    border: none;
    border-top: 3px solid var(--subject-color);
    border-bottom: 3px solid var(--subject-color);
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 6px;
    text-align: center;
    padding: 16px 20px 12px;
    box-sizing: border-box;
    margin: 0 auto 16px;
    flex-shrink: 0;
  }

  .subject-card.landscape {
    width: 250px;
    margin: 0 16px;
    direction: ltr;
  }

  /* Stake / address cards lead with the small address line; pull the content up so
     it sits nearer the top ridge rather than floating low. (pool/drep/cardano cards
     have no `.stake-address`, so they keep the default padding.) */
  .subject-card:has(.stake-address) {
    padding-top: 10px;
  }

  .pool-name {
    font-weight: 700;
    font-size: 24px;
    line-height: 1;
  }

  .drep-name {
    font-weight: 600;
    font-size: 16px;
    line-height: 1.2;
    text-align: center;
    max-width: 100%;
    overflow: hidden;
    text-overflow: ellipsis;
    display: -webkit-box;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    -webkit-box-orient: vertical;
  }

  /* Full address stays in the DOM (copyable / select-all), clipped to one line. */
  .stake-address {
    font-weight: 600;
    font-size: 13px;
    line-height: 1.2;
    max-width: 100%;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    user-select: all;
  }

  /* Clickable stake address in the payment-address header (accent color set
     inline; ellipsis from .pool-param-value); no underline. */
  .stake-link {
    text-decoration: none;
  }

  /* ADA Handle under the address balance: white name with a dimmer `$` sigil (not
     the address accent color). One line, copyable. */
  .handle {
    font-weight: 600;
    font-size: 13px;
    line-height: 1.2;
    color: white;
    max-width: 100%;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    user-select: all;
  }

  .handle-dollar {
    color: rgb(255 255 255 / 0.55);
  }

  .pool-stake {
    font-weight: 600;
    font-size: 24px;
    color: var(--text);
    line-height: 1;
    text-decoration: none;
  }

  /* The ADA value links back to the subject's feed (a:href set only on the
     address/stake cards); dim slightly on hover to hint it's clickable. */
  a.pool-stake:hover {
    opacity: 0.8;
  }

  /* The delegator count is a link to the delegators grid; it wears the muted text colour
     until hover, like the other stat links on this card. */
  .delegators-link {
    color: inherit;
    text-decoration: none;
  }
  .delegators-link:hover {
    color: #fff;
    text-decoration: underline;
  }

  .pool-delegators {
    font-size: 13px;
    color: var(--text-muted);
  }

  /* One fact per line, tighter than the card's 6px flex gap. `nowrap` keeps each line whole,
     so a count and its percentage can never be split across lines. */
  .pool-delegators.stacked {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  .pool-delegators .fact {
    white-space: nowrap;
  }

  .pool-params {
    display: flex;
    gap: 0;
    width: 100%;
    border-top: 1px solid rgb(255 255 255 / 0.15);
    padding-top: 8px;
    margin-top: 4px;
  }

  .pool-params .pool-param {
    flex: 1;
  }

  .pool-param {
    display: flex;
    flex-direction: column;
    align-items: center;
    padding: 0 6px;
    min-width: 0; /* allow the value to shrink + ellipsize in a flex row */
    max-width: 100%; /* constrain a standalone param so its value ellipsizes */
  }

  .pool-params .pool-param + .pool-param {
    border-left: 1px solid rgb(255 255 255 / 0.15);
  }

  /* A whole param that is itself the link (address feed): the entire container is
     clickable, not just the value. Neutral colors (the label/value set their own);
     a faint hover tint signals it's interactive without an accent color. */
  .pool-param-link {
    text-decoration: none;
    cursor: pointer;
  }

  .pool-param-link:hover .pool-param-value {
    color: white;
  }

  .pool-param-link:hover .pool-param-label {
    color: var(--text);
  }

  .pool-param-label {
    font-size: 8px;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    color: var(--text-muted);
    white-space: nowrap;
  }

  .pool-param-value {
    font-size: 12px;
    font-weight: 600;
    color: var(--text);
    white-space: nowrap;
    max-width: 100%;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  /* Pool stats (margin/cost/pledge): keep three even, symmetric columns, but use a
     smaller value font so a large pledge fits the card without truncation. */
  .pool-stats .pool-param-value {
    font-size: 11px;
    max-width: none;
    overflow: visible;
    text-overflow: clip;
  }
</style>
