<script lang="ts">
  import type { AssetInfo, DelegationInfo, FeedTx, TxOutputInfo } from '../types';
  import { bech32Decode, paymentCredential, stakeCredential } from '../bech32';
  import { config } from '../stores';
  import { poolColor, formatTicker } from '../layout';

  let { tx }: { tx: FeedTx } = $props();

  function truncateHash(h: string): string {
    return h.slice(0, 4) + '\u2026' + h.slice(-4);
  }

  function poolLabel(ticker?: string, poolId?: string): string {
    return formatTicker(ticker ?? poolId?.slice(5, 10) ?? '');
  }

  let visibleDelegations: DelegationInfo[] = $derived(
    (tx.delegations ?? []).filter((d) => d.from_pool_id || d.to_pool_id),
  );

  function formatAda(lovelace: string): string {
    const padded = lovelace.padStart(7, '0');
    const whole = padded.slice(0, -6) || '0';
    const frac = padded.slice(-6);
    const wholeNum = Number(whole);
    const dec = (d: string) => `<span class="ada-dec">.${d}</span>`;
    if (wholeNum >= 1000) return wholeNum.toLocaleString() + ' ADA';
    if (wholeNum >= 1) return whole + dec(frac.slice(0, 2)) + ' ADA';
    return '0' + dec(frac) + ' ADA';
  }

  function nftcdnUrl(asset: AssetInfo): string {
    const base = `https://${asset.fingerprint}.${$config!.nftcdn}/preview`;
    return asset.tk ? `${base}?tk=${asset.tk}&size=128` : `${base}?size=128`;
  }

  // Filter outputs: exclude those going back to a source address (change)
  let filteredOutputs: TxOutputInfo[] = $derived.by(() => {
    const inputPayments = new Set(
      tx.inputs.map((i) => (i.address ? paymentCredential(i.address) : null)).filter((x): x is string => x !== null),
    );
    const inputStakes = new Set(
      tx.inputs.map((i) => (i.address ? stakeCredential(i.address) : null)).filter((x): x is string => x !== null),
    );
    // First pass: filter by exact payment credential match
    const afterPayment = tx.outputs.filter((o) => {
      const cred = paymentCredential(o.address);
      return cred === null || !inputPayments.has(cred);
    });
    // Second pass: heuristic change detection for non-script addresses
    return afterPayment.filter((o) => {
      if (o.assets.length <= 1) return true;
      // Byron address with many assets, only if non-change outputs remain
      if (!o.address.startsWith('addr')) return afterPayment.length <= 1;
      // Same stake credential with many assets, but not a script address
      const bytes = bech32Decode(o.address);
      if (bytes && (bytes[0] & 0x10) !== 0) return true;
      const stake = stakeCredential(o.address);
      return stake === null || !inputStakes.has(stake);
    });
  });

  // Total asset count across visible outputs → scale thumbnails
  let totalAssets = $derived(filteredOutputs.reduce((sum, o) => sum + o.assets.length, 0));
  let thumbSize = $derived(totalAssets <= 1 ? 64 : Math.max(16, Math.floor(64 / Math.sqrt(totalAssets))));

  const MAX_OUTPUTS = 8;
  let sortedOutputs = $derived([...filteredOutputs].sort((a, b) => {
    const aHas = a.assets.length > 0 ? 0 : 1;
    const bHas = b.assets.length > 0 ? 0 : 1;
    if (aHas !== bHas) return aHas - bHas;
    return Number(BigInt(a.lovelace) - BigInt(b.lovelace));
  }));
  let visibleOutputs = $derived(sortedOutputs.slice(-MAX_OUTPUTS));
  let hiddenOutputCount = $derived(filteredOutputs.length - visibleOutputs.length);

  // Deduplicate inputs by address
  let uniqueInputs = $derived([...new Map(tx.inputs.map((i) => [i.address, i])).values()]);
  let visibleInputs = $derived(uniqueInputs.slice(0, MAX_OUTPUTS));
  let hiddenInputCount = $derived(uniqueInputs.length - visibleInputs.length);
</script>

<div class="tx-card" style:--thumb-size="{thumbSize}px">
  {#if visibleDelegations.length > 0}
    <div class="deleg-section">
      <div class="addr-list">
        {#each visibleDelegations as deleg}
          {@const isDeregistration = !deleg.to_pool_id && !!deleg.from_pool_id}
          <div class="addr-item">
            {#if deleg.to_pool_id}
              <a class="deleg-pool" href="/{deleg.to_pool_id}"
                >{poolLabel(deleg.to_ticker, deleg.to_pool_id)}</a
              >
            {/if}
            {#if deleg.to_pool_id}
              <span class="deleg-arrow">{@html '&#x2191;'}</span>
            {/if}
            {#if deleg.from_pool_id}
              <a
                class="deleg-pool"
                class:deregistered={isDeregistration}
                href="/{deleg.from_pool_id}"
                >{poolLabel(deleg.from_ticker, deleg.from_pool_id)}</a
              >
            {/if}
            <span class="ada" style:color={poolColor(deleg.to_pool_id ?? deleg.from_pool_id)}>{@html formatAda(deleg.live_stake)}</span>
            <span class="addr mono">{deleg.stake_address}</span>
          </div>
        {/each}
      </div>
    </div>
  {/if}

  <div class="tx-body">
    <div class="addr-list">
      {#if hiddenOutputCount > 0}
        <span class="more-outputs">+{hiddenOutputCount} more</span>
      {/if}
      {#each visibleOutputs as output}
        <div class="addr-item">
          <span class="ada">{@html formatAda(output.lovelace)}</span>
          {#if output.assets.length > 0 && $config}
            <div class="assets">
              {#each output.assets as asset}
                <div class="asset">
                  <img
                    class="asset-thumb"
                    src={nftcdnUrl(asset)}
                    alt={asset.fingerprint}
                    loading="lazy"
                    onload={(e: Event) => {
                      (e.target as HTMLElement).dispatchEvent(new Event('remeasure', { bubbles: true }));
                    }}
                    onerror={(e: Event) => {
                      const el = (e.target as HTMLElement).parentElement!;
                      el.style.display = 'none';
                      el.dispatchEvent(new Event('remeasure', { bubbles: true }));
                    }}
                  />
                  {#if thumbSize >= 32 && asset.quantity !== '1'}
                    <span class="asset-label">{BigInt(asset.quantity).toLocaleString()}</span>
                  {/if}
                </div>
              {/each}
            </div>
          {/if}
          <span class="addr mono">{output.address}</span>
        </div>
      {/each}
    </div>

    {#if filteredOutputs.length === 0}
      <span class="ada">{@html formatAda(tx.outputs.reduce((s, o) => s + BigInt(o.lovelace), 0n).toString())}</span>
    {/if}
    <div class="arrow" class:flip={filteredOutputs.length === 0}>{filteredOutputs.length === 0 ? '↻' : '↑'}</div>

    <div class="addr-list">
      {#each visibleInputs as input}
        <div class="addr-item">
          <span class="addr mono">{input.address ?? '???'}</span>
        </div>
      {/each}
      {#if hiddenInputCount > 0}
        <span class="more-outputs">+{hiddenInputCount} more</span>
      {/if}
    </div>

    <div class="tx-hash mono">{truncateHash(tx.hash)}</div>
  </div>
</div>

<style>
  .tx-card {
    width: var(--item-width);
    font-size: 11px;
    text-align: center;
    transition: filter var(--flip-duration) ease;
    overflow: hidden;
  }

  .deleg-section {
    background: rgb(0 0 0 / 0.5);
    border-radius: 6px;
    padding: 8px 10px;
  }

  .tx-body {
    background: rgb(0 0 0 / 0.5);
    border-radius: 6px;
    padding: 8px 10px;
  }

  .tx-hash {
    color: var(--section-color, var(--accent));
    font-size: 10px;
    margin-top: 6px;
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
  }

  .arrow {
    color: rgb(255 255 255 / 0.4);
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

.deleg-pool.deregistered {
    text-decoration: line-through;
  }

  .deleg-arrow {
    color: rgb(255 255 255 / 0.4);
    font-size: 12px;
    line-height: 1;
  }

  .assets {
    display: flex;
    flex-wrap: wrap;
    gap: 2px;
    margin-top: 2px;
    justify-content: center;
  }

  .asset {
    display: flex;
    flex-direction: column;
    align-items: center;
  }

  .asset-label {
    font-size: 9px;
    color: white;
    text-align: center;
    white-space: nowrap;
  }

  .asset-thumb {
    max-width: var(--thumb-size, 64px);
    max-height: var(--thumb-size, 64px);
    align-self: center;
    border-radius: 3px;
    background: transparent;
  }

  .more-outputs {
    color: var(--text-muted);
    font-size: 10px;
  }
</style>
