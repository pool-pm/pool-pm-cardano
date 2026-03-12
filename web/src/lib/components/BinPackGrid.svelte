<script lang="ts" module>
  function registerRef(
    node: HTMLElement,
    params: { k: string; register: (k: string, el: HTMLElement) => void; unregister: (k: string) => void },
  ) {
    params.register(params.k, node);
    return {
      destroy() {
        params.unregister(params.k);
      },
    };
  }
</script>

<script lang="ts" generics="T">
  import { onMount, tick, untrack } from 'svelte';

  type Props = {
    items: T[];
    key: (item: T) => string;
    itemWidth: number;
    gap: number;
    children: import('svelte').Snippet<[T]>;
    availableWidth?: number;
  };

  let { items, key, itemWidth, gap, children, availableWidth }: Props = $props();

  let container: HTMLDivElement;
  let containerWidth = $state(0);
  let containerHeight = $state(0);
  let itemPositions = $state<Map<string, { x: number; y: number }>>(new Map());
  let itemRefs = new Map<string, HTMLElement>();

  const layoutWidth = $derived(availableWidth ?? containerWidth);
  const colCount = $derived(Math.max(1, Math.floor((layoutWidth + gap) / (itemWidth + gap))));

  function measure() {
    if (!container || items.length === 0 || containerWidth === 0) {
      containerHeight = 0;
      return;
    }

    // Bottom-up bin-packing: oldest at bottom-right, newest at top-left
    // Process oldest first, fill right-to-left, stack on shortest column
    const colHeights = new Array(colCount).fill(0); // heights grow upward from bottom
    const itemData: { k: string; col: number; y: number; height: number }[] = [];
    let maxColUsed = -1;
    // Process from oldest (end of array) to newest (start)
    for (let i = items.length - 1; i >= 0; i--) {
      const item = items[i];
      const k = key(item);
      const el = itemRefs.get(k);
      if (!el) continue;

      const height = el.offsetHeight;

      // Among used columns, find shortest (rightmost if tie)
      let shortestUsed = 0;
      for (let c = 1; c <= maxColUsed; c++) {
        if (colHeights[c] <= colHeights[shortestUsed]) {
          shortestUsed = c;
        }
      }

      const currentMax = maxColUsed >= 0 ? Math.max(...colHeights.slice(0, maxColUsed + 1)) : 0;
      const canStack = maxColUsed >= 0 && colHeights[shortestUsed] + height + gap <= currentMax;

      let targetCol: number;
      if (canStack) {
        targetCol = shortestUsed;
      } else if (maxColUsed < colCount - 1) {
        targetCol = maxColUsed + 1;
      } else {
        targetCol = shortestUsed;
      }

      maxColUsed = Math.max(maxColUsed, targetCol);

      const y = colHeights[targetCol];
      itemData.push({ k, col: targetCol, y, height });
      colHeights[targetCol] = y + height + gap;
    }

    // Calculate total height and convert to top-down coordinates
    const totalHeight = Math.max(0, Math.max(...colHeights) - gap);

    // Calculate actual grid width and center offset
    const actualCols = maxColUsed + 1;
    const actualGridWidth = actualCols * itemWidth + (actualCols - 1) * gap;
    const actualOffsetX = Math.max(0, (containerWidth - actualGridWidth) / 2);

    // Convert positions: flip y-axis and apply x offset
    const newPositions = new Map<string, { x: number; y: number }>();
    for (const { k, col, y, height } of itemData) {
      const displayY = totalHeight - y - height;
      const displayX = actualOffsetX + col * (itemWidth + gap);
      newPositions.set(k, { x: displayX, y: displayY });
    }

    itemPositions = newPositions;
    containerHeight = totalHeight;
    container.dispatchEvent(new CustomEvent('gridwidth', { detail: actualGridWidth, bubbles: true }));
  }

  function registerItem(k: string, el: HTMLElement) {
    itemRefs.set(k, el);
  }

  function unregisterItem(k: string) {
    itemRefs.delete(k);
  }

  function scheduleMeasure() {
    if (!measurePending) {
      measurePending = true;
      tick().then(() => {
        measurePending = false;
        measure();
      });
    }
  }

  onMount(() => {
    containerWidth = container.offsetWidth;

    const resizeObserver = new ResizeObserver((entries) => {
      const newWidth = entries[0]?.contentRect.width ?? 0;
      if (newWidth !== containerWidth) {
        containerWidth = newWidth;
      }
    });
    resizeObserver.observe(container);

    container.addEventListener('remeasure', scheduleMeasure);

    return () => {
      resizeObserver.disconnect();
      container.removeEventListener('remeasure', scheduleMeasure);
    };
  });

  // Re-measure when dependencies change
  let measurePending = false;
  $effect(() => {
    items;
    colCount;
    containerWidth;
    untrack(scheduleMeasure);
  });
</script>

<div class="bin-pack-container" bind:this={container} style="height: {containerHeight}px; --item-width: {itemWidth}px">
  {#each items as item (key(item))}
    {@const k = key(item)}
    {@const pos = itemPositions.get(k)}
    {@const defaultX = Math.max(0, (containerWidth - itemWidth) / 2)}
    <div
      class="bin-pack-item"
      style="transform: translate({pos?.x ?? defaultX}px, {pos?.y ?? -100}px)"
      use:registerRef={{ k, register: registerItem, unregister: unregisterItem }}
    >
      {@render children(item)}
    </div>
  {/each}
</div>

<style>
  .bin-pack-container {
    position: relative;
    width: 100%;
    overflow: hidden;
  }

  .bin-pack-item {
    position: absolute;
    width: var(--item-width);
    transition: transform var(--flip-duration) ease;
  }
</style>
