<script lang="ts" generics="T">
	import { onMount, tick, untrack } from 'svelte';
	import { flip } from 'svelte/animate';

	type Props = {
		items: T[];
		key: (item: T) => string;
		itemWidth: number;
		gap: number;
		children: import('svelte').Snippet<[T]>;
	};

	let { items, key, itemWidth, gap, children }: Props = $props();

	let container: HTMLDivElement;
	let containerWidth = $state(0);
	let containerHeight = $state(0);
	let itemPositions = $state<Map<string, { x: number; y: number }>>(new Map());
	let itemRefs = new Map<string, HTMLElement>();

	const colCount = $derived(Math.max(1, Math.floor((containerWidth + gap) / (itemWidth + gap))));

	// Center the grid within container
	const gridWidth = $derived(colCount * itemWidth + (colCount - 1) * gap);
	const offsetX = $derived(Math.max(0, (containerWidth - gridWidth) / 2));

	function measure() {
		if (!container || items.length === 0) {
			containerHeight = 0;
			return;
		}

		const newPositions = new Map<string, { x: number; y: number }>();
		const colHeights = new Array(colCount).fill(0);

		for (const item of items) {
			const k = key(item);
			const el = itemRefs.get(k);
			if (!el) continue;

			const height = el.offsetHeight;

			// Find column with minimum height
			let minCol = 0;
			for (let c = 1; c < colCount; c++) {
				if (colHeights[c] < colHeights[minCol]) {
					minCol = c;
				}
			}

			const x = offsetX + minCol * (itemWidth + gap);
			const y = colHeights[minCol];

			newPositions.set(k, { x, y });
			colHeights[minCol] = y + height + gap;
		}

		itemPositions = newPositions;
		containerHeight = Math.max(0, Math.max(...colHeights) - gap);
	}

	function registerItem(k: string, el: HTMLElement) {
		itemRefs.set(k, el);
	}

	function unregisterItem(k: string) {
		itemRefs.delete(k);
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

		return () => resizeObserver.disconnect();
	});

	// Re-measure when dependencies change
	$effect(() => {
		// Track these dependencies
		items;
		colCount;
		offsetX;
		untrack(() => {
			tick().then(measure);
		});
	});
</script>

<div
	class="bin-pack-container"
	bind:this={container}
	style="height: {containerHeight}px; --item-width: {itemWidth}px"
>
	{#each items as item (key(item))}
		{@const k = key(item)}
		{@const pos = itemPositions.get(k)}
		<div
			class="bin-pack-item"
			style="transform: translate({pos?.x ?? 0}px, {pos?.y ?? 0}px)"
			use:registerRef={{ k, register: registerItem, unregister: unregisterItem }}
			animate:flip={{ duration: 300 }}
		>
			{@render children(item)}
		</div>
	{/each}
</div>

<script lang="ts" module>
	function registerRef(
		node: HTMLElement,
		params: { k: string; register: (k: string, el: HTMLElement) => void; unregister: (k: string) => void }
	) {
		params.register(params.k, node);
		return {
			destroy() {
				params.unregister(params.k);
			}
		};
	}
</script>

<style>
	.bin-pack-container {
		position: relative;
		width: 100%;
	}

	.bin-pack-item {
		position: absolute;
		width: var(--item-width);
		transition: transform 0.3s ease;
	}
</style>
