<script lang="ts">
  import type { AssetInfo, DelegationInfo, FeedTx, TxOutputInfo } from '../types';
  import { config } from '../stores';
  import { poolColor, formatTicker } from '../layout';

  let { tx, compact = false }: { tx: FeedTx; compact?: boolean } = $props();
  let failedAssets = $state<Record<number, number>>({});

  function showPreview(e: MouseEvent) {
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
    (tx.delegations ?? []).filter((d) => d.from_pool_id || d.to_pool_id),
  );

  function formatAda(lovelace: string): string {
    const padded = lovelace.padStart(7, '0');
    const whole = padded.slice(0, -6) || '0';
    const frac = padded.slice(-6);
    const wholeNum = Number(whole);
    const sym = '<span class="ada-sym">₳\u2009</span>';
    const dec = (d: string) => `<span class="ada-dec">.${d}</span>`;
    if (wholeNum >= 1000) return sym + wholeNum.toLocaleString();
    if (wholeNum >= 1) {
      const trimmed = frac.slice(0, 2).replace(/0+$/, '');
      return trimmed ? sym + whole + dec(trimmed) : sym + whole;
    }
    const trimmed = frac.replace(/0+$/, '');
    return trimmed ? sym + '0' + dec(trimmed) : sym + '0';
  }

  function nftcdnUrl(asset: AssetInfo): string {
    const base = `https://${asset.fingerprint}.${$config!.nftcdn}/preview`;
    return asset.tk ? `${base}?tk=${asset.tk}&size=128` : `${base}?size=128`;
  }

  // Total asset count across outputs → scale thumbnails
  let totalAssets = $derived(tx.outputs.reduce((sum, o) => sum + o.assets.length, 0));
  let thumbSize = $derived(totalAssets <= 1 ? 64 : Math.max(16, Math.floor(64 / Math.sqrt(totalAssets))));

  let maxOutputs = $derived(compact ? 2 : 8);
  let maxInputs = $derived(compact ? 2 : 8);
  let maxAssets = $derived(compact ? 10 : 50);
  let maxAssetsPerOutput = $derived(compact ? 5 : 25);

  let sortedOutputs = $derived([...tx.outputs].sort((a, b) => Number(BigInt(b.lovelace) - BigInt(a.lovelace))));
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
  let hiddenOutputCount = $derived(tx.outputs.length - visibleOutputs.length);

  // Deduplicate inputs by address
  let uniqueInputs = $derived([...new Map(tx.inputs.map((i) => [i.address, i])).values()]);
  let visibleInputs = $derived(uniqueInputs.slice(0, maxInputs));
  let hiddenInputCount = $derived(uniqueInputs.length - visibleInputs.length);
</script>

<div class="tx-card" style:--thumb-size="{thumbSize}px">
  {#if tx.stake_change}
    {@const negative = tx.stake_change.startsWith('-')}
    <div class="stake-change" style:color={negative ? 'oklch(0.7 0.25 25)' : 'oklch(0.7 0.25 145)'}>
      {negative ? '−' : '+'}{@html formatAda(negative ? tx.stake_change.slice(1) : tx.stake_change)}
    </div>
  {/if}
  {#if visibleDelegations.length > 0}
    <div class="deleg-section">
      <div class="addr-list">
        {#each visibleDelegations as deleg}
          {@const isDeregistration = !deleg.to_pool_id && !!deleg.from_pool_id}
          <div class="addr-item">
            {#if deleg.to_pool_id}
              <a class="deleg-pool" href="/{deleg.to_pool_id}">{poolLabel(deleg.to_ticker, deleg.to_pool_id)}</a>
            {/if}
            {#if deleg.to_pool_id}
              <span class="deleg-arrow">{@html '&#x2191;'}</span>
            {/if}
            {#if deleg.from_pool_id}
              <a class="deleg-pool" class:deregistered={isDeregistration} href="/{deleg.from_pool_id}"
                >{poolLabel(deleg.from_ticker, deleg.from_pool_id)}</a
              >
            {/if}
            <span class="ada">{@html formatAda(deleg.live_stake)}</span>
            <span class="addr mono">{deleg.stake_address}</span>
          </div>
        {/each}
      </div>
    </div>
  {/if}

  {#if tx.inputs.length > 0 || tx.outputs.length > 0}
    <div class="tx-body">
      <div class="addr-list">
        {#each visibleOutputs as output, oi}
          <div class="addr-item">
            <span class="ada">{@html formatAda(output.lovelace)}</span>
            {#if output.assets.length > 0 && $config}
              {@const visibleAssetCount = Math.min(output.assets.length, maxAssetsPerOutput)}
              {@const hiddenAssets = output.assets.length - visibleAssetCount}
              <div class="assets">
                {#each output.assets.slice(0, visibleAssetCount) as asset}
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
                        failedAssets = { ...failedAssets, [oi]: (failedAssets[oi] ?? 0) + 1 };
                      }}
                      onmouseenter={showPreview}
                      onmouseleave={hidePreview}
                    />
                    {#if thumbSize >= 32 && asset.quantity !== '1'}
                      <span class="asset-label">{BigInt(asset.quantity).toLocaleString()}</span>
                    {/if}
                  </div>
                {/each}
              </div>
              {@const totalHidden = hiddenAssets + (failedAssets[oi] ?? 0)}
              {#if totalHidden > 0}
                <span class="more-outputs">+{totalHidden} asset{totalHidden > 1 ? 's' : ''}</span>
              {/if}
            {/if}
            <span class="addr mono">{output.address}</span>
          </div>
        {/each}
        {#if tx.outputs.length === 0}
          <span class="ada">{@html formatAda(tx.outputs.reduce((s, o) => s + BigInt(o.lovelace), 0n).toString())}</span>
        {/if}
        {#if hiddenOutputCount > 0}
          <span class="more-outputs">+{hiddenOutputCount} output{hiddenOutputCount > 1 ? 's' : ''}</span>
        {/if}
        <div class="addr-item">
          <span class="ada">{@html formatAda(tx.fee)}</span>
          <span class="addr">fee</span>
        </div>
      </div>
      <div class="arrow" class:flip={tx.outputs.length === 0}>{tx.outputs.length === 0 ? '↻' : '↑'}</div>

      <div class="addr-list">
        {#each visibleInputs as input}
          <div class="addr-item">
            <span class="ada">{@html formatAda(input.lovelace)}</span>
            <span class="addr mono">{input.address ?? '???'}</span>
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
</div>

<style>
  .tx-card {
    width: var(--item-width);
    font-size: 11px;
    text-align: center;
    transition: filter var(--flip-duration) ease;
  }

  .stake-change {
    font-size: 13px;
    font-weight: 700;
    margin-bottom: 8px;
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
    color: rgb(255 255 255 / 0.4);
    font-size: 10px;
  }
</style>
