<script lang="ts">
	import {
		getStoredOrders,
		getStoredPegoutOrders,
		type StoredOrder,
		type StoredPegoutOrder
	} from '$lib/orderHistory';
	import { ecxTicker, presentSatAmount, formatBaseUnits } from '$lib/units.svelte';

	function formatDate(ms: number): string {
		return new Date(ms).toLocaleString(undefined, {
			month: 'short',
			day: 'numeric',
			hour: 'numeric',
			minute: '2-digit'
		});
	}

	type Row =
		| { direction: 'pegin'; order: StoredOrder }
		| { direction: 'pegout'; order: StoredPegoutOrder };

	let rows = $state<Row[]>([]);

	$effect(() => {
		const pegin: Row[] = getStoredOrders().map((order) => ({ direction: 'pegin', order }));
		const pegout: Row[] = getStoredPegoutOrders().map((order) => ({ direction: 'pegout', order }));
		rows = [...pegin, ...pegout].sort((a, b) => b.order.createdAt - a.order.createdAt);
	});

	// wECX decimals are the same for both directions (one mint), so either quote endpoint works -
	// /bridge/quote is fetched here since peg-in orders are the more common case.
	let wecxDecimals = $state<number | null>(null);
	$effect(() => {
		fetch('/bridge/quote')
			.then((res) => (res.ok ? res.json() : null))
			.then((data) => {
				if (data) wecxDecimals = data.wecxDecimals;
			});
	});

	function amountOut(order: StoredOrder): string {
		return wecxDecimals != null ? formatBaseUnits(order.amountOutBaseUnits, wecxDecimals) : '…';
	}

	function amountIn(order: StoredPegoutOrder): string {
		return wecxDecimals != null ? formatBaseUnits(order.amountInBaseUnits, wecxDecimals) : '…';
	}
</script>

<div class="flex min-h-screen justify-center bg-zinc-50 px-4 py-12 dark:bg-zinc-950">
	<div class="w-full max-w-sm">
		<a
			href="/"
			class="mb-4 inline-flex items-center gap-1.5 text-xs font-medium text-zinc-500 transition-colors hover:text-zinc-900 dark:hover:text-white"
		>
			<svg
				class="h-3.5 w-3.5"
				fill="none"
				viewBox="0 0 24 24"
				stroke="currentColor"
				stroke-width="2.5"
			>
				<path stroke-linecap="round" stroke-linejoin="round" d="M15 19l-7-7 7-7" />
			</svg>
			Back to bridge
		</a>

		<div class="mb-6 text-center">
			<h1 class="text-2xl font-bold text-zinc-900 dark:text-white">Your orders</h1>
			<p class="mt-1 text-sm text-zinc-500">Saved on this device's browser only.</p>
		</div>

		{#if rows.length === 0}
			<div
				class="rounded-2xl bg-white p-6 text-center ring-1 ring-black/5 dark:bg-zinc-900 dark:ring-white/5"
			>
				<p class="text-sm text-zinc-500">No orders yet on this device.</p>
			</div>
		{:else}
			<div class="flex flex-col gap-2">
				{#each rows as row (row.order.id)}
					<a
						href={row.direction === 'pegin'
							? `/orders/${row.order.id}`
							: `/pegout/orders/${row.order.id}`}
						class="rounded-2xl bg-white p-4 shadow-2xl ring-1 ring-black/10 transition-colors hover:ring-black/20 dark:bg-zinc-900 dark:ring-white/10 dark:hover:ring-white/20"
					>
						<div class="flex items-center justify-between">
							<span class="text-sm font-semibold text-zinc-900 dark:text-white">
								{#if row.direction === 'pegin'}
									{presentSatAmount(row.order.amountInSat)}
									{ecxTicker()} → {amountOut(row.order)} wbECX
								{:else}
									{amountIn(row.order)} wbECX → {presentSatAmount(row.order.amountOutSat)}
									{ecxTicker()}
								{/if}
							</span>
							<span class="text-xs text-zinc-600">{formatDate(row.order.createdAt)}</span>
						</div>
						<p class="mt-1 truncate font-mono text-[10px] text-zinc-600">{row.order.id}</p>
					</a>
				{/each}
			</div>
		{/if}
	</div>
</div>
