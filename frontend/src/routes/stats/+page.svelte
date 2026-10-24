<script lang="ts">
	import { invalidateAll } from '$app/navigation';
	import type { PageData } from './$types';

	let { data }: { data: PageData } = $props();
	let refreshing = $state(false);

	const formatUnits = (amount: string, decimals: number) => {
		const value = BigInt(amount);
		const scale = 10n ** BigInt(decimals);
		const whole = (value / scale).toLocaleString('en-US');
		const fraction = (value % scale).toString().padStart(decimals, '0').replace(/0+$/, '');
		return fraction ? `${whole}.${fraction}` : whole;
	};

	const refreshedAt = $derived(
		new Intl.DateTimeFormat('en-US', {
			dateStyle: 'medium',
			timeStyle: 'short',
			timeZone: 'UTC'
		}).format(new Date(data.stats.updated_at_unix * 1000))
	);

	const ecxTotal = $derived(
		formatUnits(
			String(BigInt(data.stats.ecx_confirmed_sat) + BigInt(data.stats.ecx_unconfirmed_sat)),
			8
		)
	);

	const statusLabels: Record<string, string> = {
		pending: 'Awaiting deposit',
		deposit_seen: 'Deposit seen',
		deposit_confirmed: 'Deposit confirmed',
		paying_out: 'Payout in progress',
		paid_out: 'Completed',
		expired: 'Expired',
		failed: 'Failed',
		refund_requested: 'Refund requested',
		refunding: 'Refund in progress',
		refunded: 'Refunded'
	};

	function formatOrderAmount(order: (typeof data.stats.recent_orders)[number], input: boolean) {
		const amount = input ? order.amount_in : order.amount_out;
		if (order.direction === 'wrap') {
			return `${formatUnits(amount, input ? 8 : data.stats.wecx_decimals)} ${input ? 'bECX' : 'wbECX'}`;
		}
		return `${formatUnits(amount, input ? data.stats.wecx_decimals : 8)} ${input ? 'wbECX' : 'bECX'}`;
	}

	function formatOrderDate(timestamp: number) {
		return new Intl.DateTimeFormat('en-US', {
			dateStyle: 'medium',
			timeStyle: 'short',
			timeZone: 'UTC'
		}).format(new Date(timestamp * 1000));
	}

	async function refreshStats() {
		refreshing = true;
		try {
			await invalidateAll();
		} finally {
			refreshing = false;
		}
	}
</script>

<svelte:head>
	<title>Bridge stats</title>
	<meta name="description" content="Live bridge balances, completed wraps and unwraps, and fees." />
</svelte:head>

<div class="flex min-h-screen justify-center bg-zinc-50 px-4 py-10 sm:py-12 dark:bg-zinc-950">
	<main class="w-full max-w-3xl">
		<header class="mb-7 flex items-end justify-between gap-4">
			<div>
				<h1 class="text-2xl font-bold text-zinc-900 dark:text-white">Bridge stats</h1>
				<p class="mt-1 text-sm text-zinc-500">Treasury balances and completed bridge activity</p>
			</div>
			<button
				type="button"
				onclick={refreshStats}
				disabled={refreshing}
				class="shrink-0 rounded-lg border border-zinc-300 bg-white px-3 py-2 text-sm font-semibold text-zinc-700 transition-colors hover:bg-zinc-100 disabled:opacity-50 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200 dark:hover:bg-zinc-800"
			>
				{refreshing ? 'Refreshing…' : 'Refresh'}
			</button>
		</header>

		<section aria-labelledby="balances-heading">
			<h2
				id="balances-heading"
				class="mb-3 text-xs font-bold tracking-wide text-zinc-500 uppercase"
			>
				Treasury balances
			</h2>
			<div class="overflow-x-auto rounded-lg border border-zinc-200 dark:border-zinc-800">
				<table
					aria-labelledby="balances-heading"
					class="w-full min-w-[680px] border-collapse text-left text-sm"
				>
					<thead class="bg-zinc-100 text-xs text-zinc-500 dark:bg-zinc-900">
						<tr>
							<th scope="col" class="px-3 py-2.5 font-semibold">Asset</th>
							<th scope="col" class="px-3 py-2.5 font-semibold">Total</th>
							<th scope="col" class="px-3 py-2.5 font-semibold">Details</th>
						</tr>
					</thead>
					<tbody class="divide-y divide-zinc-100 bg-white dark:divide-zinc-800 dark:bg-zinc-950">
						<tr>
							<th scope="row" class="px-3 py-2.5 font-medium text-zinc-800 dark:text-zinc-200"
								>bECX</th
							>
							<td
								class="px-3 py-2.5 whitespace-nowrap text-zinc-700 tabular-nums dark:text-zinc-300"
								>{ecxTotal}</td
							>
							<td class="px-3 py-2.5 text-zinc-500">
								{formatUnits(data.stats.ecx_confirmed_sat, 8)} confirmed ·
								{formatUnits(data.stats.ecx_unconfirmed_sat, 8)} unconfirmed
							</td>
						</tr>
						<tr>
							<th scope="row" class="px-3 py-2.5 font-medium text-zinc-800 dark:text-zinc-200"
								>wbECX</th
							>
							<td
								class="px-3 py-2.5 whitespace-nowrap text-zinc-700 tabular-nums dark:text-zinc-300"
								>{formatUnits(data.stats.wecx_balance_base_units, data.stats.wecx_decimals)}</td
							>
							<td class="px-3 py-2.5 text-zinc-500">Solana treasury token account</td>
						</tr>
						<tr>
							<th scope="row" class="px-3 py-2.5 font-medium text-zinc-800 dark:text-zinc-200"
								>SOL</th
							>
							<td
								class="px-3 py-2.5 whitespace-nowrap text-zinc-700 tabular-nums dark:text-zinc-300"
								>{formatUnits(data.stats.sol_balance_lamports, 9)}</td
							>
							<td class="px-3 py-2.5 text-zinc-500">Treasury transaction balance</td>
						</tr>
						<tr>
							<th scope="row" class="px-3 py-2.5 font-medium text-zinc-800 dark:text-zinc-200"
								>Account</th
							>
							<td colspan="2" class="px-3 py-2.5 break-all text-zinc-700 dark:text-zinc-300">
								{data.stats.solana_treasury_address}
							</td>
						</tr>
					</tbody>
				</table>
			</div>
		</section>

		<section aria-labelledby="activity-heading" class="mt-8">
			<h2
				id="activity-heading"
				class="mb-3 text-xs font-bold tracking-wide text-zinc-500 uppercase"
			>
				Completed Activity
			</h2>
			<div class="overflow-x-auto rounded-lg border border-zinc-200 dark:border-zinc-800">
				<table
					aria-labelledby="activity-heading"
					class="w-full min-w-[680px] border-collapse text-left text-sm"
				>
					<thead class="bg-zinc-100 text-xs text-zinc-500 dark:bg-zinc-900">
						<tr>
							<th scope="col" class="px-3 py-2.5 font-semibold">-</th>
							<th scope="col" class="px-3 py-2.5 font-semibold">Wraps</th>
							<th scope="col" class="px-3 py-2.5 font-semibold">UnWraps</th>
						</tr>
					</thead>
					<tbody class="divide-y divide-zinc-100 bg-white dark:divide-zinc-800 dark:bg-zinc-950">
						<tr>
							<th
								scope="row"
								class="px-3 py-2.5 font-medium whitespace-nowrap text-zinc-800 dark:text-zinc-200"
								>Lifetime Successful</th
							>
							<td class="px-3 py-2.5 text-zinc-700 tabular-nums dark:text-zinc-300"
								>{data.stats.wraps.toLocaleString('en-US')}</td
							>
							<td class="px-3 py-2.5 text-zinc-700 tabular-nums dark:text-zinc-300"
								>{data.stats.unwraps.toLocaleString('en-US')}</td
							>
						</tr>
						<tr>
							<th
								scope="row"
								class="px-3 py-2.5 font-medium whitespace-nowrap text-zinc-800 dark:text-zinc-200"
								>Lifetime Fees Earned</th
							>
							<td
								class="px-3 py-2.5 whitespace-nowrap text-zinc-700 tabular-nums dark:text-zinc-300"
								>{formatUnits(data.stats.wrap_fees_ecx_sat, 8)} bECX</td
							>
							<td
								class="px-3 py-2.5 whitespace-nowrap text-zinc-700 tabular-nums dark:text-zinc-300"
								>{formatUnits(data.stats.unwrap_fees_wecx_base_units, data.stats.wecx_decimals)} wbECX</td
							>
						</tr>
						<tr>
							<th
								scope="row"
								class="px-3 py-2.5 font-medium whitespace-nowrap text-zinc-800 dark:text-zinc-200"
								>Lifetime Fees Paid Out</th
							>
							<td
								class="px-3 py-2.5 whitespace-nowrap text-zinc-700 tabular-nums dark:text-zinc-300"
								>{formatUnits(data.stats.fees_paid_ecx_sat, 8)} bECX</td
							>
							<td
								class="px-3 py-2.5 whitespace-nowrap text-zinc-700 tabular-nums dark:text-zinc-300"
								>{formatUnits(data.stats.fees_paid_wecx_base_units, data.stats.wecx_decimals)} wbECX</td
							>
						</tr>
					</tbody>
				</table>
			</div>
		</section>

		<section aria-labelledby="orders-heading" class="mt-8">
			<div class="mb-3 flex items-baseline justify-between gap-3">
				<h2 id="orders-heading" class="text-xs font-bold tracking-wide text-zinc-500 uppercase">
					Recent orders
				</h2>
				<p class="text-xs text-zinc-500">Latest 50 · UTC</p>
			</div>
			<div class="overflow-x-auto rounded-lg border border-zinc-200 dark:border-zinc-800">
				<table
					aria-labelledby="orders-heading"
					class="w-full min-w-[680px] border-collapse text-left text-sm"
				>
					<thead class="bg-zinc-100 text-xs text-zinc-500 dark:bg-zinc-900">
						<tr>
							<th scope="col" class="px-3 py-2.5 font-semibold">Type</th>
							<th scope="col" class="px-3 py-2.5 font-semibold">Input</th>
							<th scope="col" class="px-3 py-2.5 font-semibold">Output</th>
							<th scope="col" class="px-3 py-2.5 font-semibold">Status</th>
							<th scope="col" class="px-3 py-2.5 font-semibold">Created</th>
						</tr>
					</thead>
					<tbody class="divide-y divide-zinc-100 bg-white dark:divide-zinc-800 dark:bg-zinc-950">
						{#each data.stats.recent_orders as order}
							<tr>
								<td class="px-3 py-2.5 font-medium text-zinc-800 dark:text-zinc-200">
									{order.direction === 'wrap' ? 'Wrap' : 'Unwrap'}
								</td>
								<td
									class="px-3 py-2.5 whitespace-nowrap text-zinc-700 tabular-nums dark:text-zinc-300"
								>
									{formatOrderAmount(order, true)}
								</td>
								<td
									class="px-3 py-2.5 whitespace-nowrap text-zinc-700 tabular-nums dark:text-zinc-300"
								>
									{formatOrderAmount(order, false)}
								</td>
								<td class="px-3 py-2.5 whitespace-nowrap text-zinc-700 dark:text-zinc-300">
									{statusLabels[order.status] ?? order.status}
								</td>
								<td class="px-3 py-2.5 whitespace-nowrap text-zinc-500">
									{formatOrderDate(order.created_at_unix)}
								</td>
							</tr>
						{:else}
							<tr>
								<td colspan="5" class="px-3 py-8 text-center text-sm text-zinc-500">
									No orders yet
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		</section>

		<p class="mt-6 text-xs text-zinc-500">
			Updated {refreshedAt} UTC. Trade counts include orders with completed payouts only.
		</p>
	</main>
</div>
