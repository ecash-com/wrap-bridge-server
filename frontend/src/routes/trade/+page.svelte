<script lang="ts">
	import PageTabs from '$lib/PageTabs.svelte';
	import { wrappedTicker } from '$lib/tickers';
	import { env } from '$env/dynamic/public';

	const WECX_MINT = env.PUBLIC_WECX_MINT || 'EVHqNdzjCupKi4rQkbuYw52sa1m8A7jeUAMP23S9AVVq';
	const USDC_MINT = 'EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v';

	const venues = [
		{
			name: 'Jupiter',
			description:
				'The leading Solana swap aggregator. Routes across all major DEXs for the best price.',
			buyUrl: `https://jup.ag/?sell=${USDC_MINT}&buy=${WECX_MINT}`,
			sellUrl: `https://jup.ag/?sell=${WECX_MINT}&buy=${USDC_MINT}`
		},
		{
			name: 'Orca',
			description: 'Leading Solana DEX with concentrated liquidity pools.',
			buyUrl: `https://www.orca.so/trade?tokenIn=${USDC_MINT}&tokenOut=${WECX_MINT}`,
			sellUrl: `https://www.orca.so/trade?tokenIn=${WECX_MINT}&tokenOut=${USDC_MINT}`
		},
		{
			name: 'Phantom',
			description:
				`Popular Solana wallet with a built-in swapper. Add ${wrappedTicker()} to your wallet, then use Swap.`,
			url: 'https://phantom.com'
		},
		{
			name: 'Solflare',
			description:
				`Long-standing Solana wallet with built-in swaps. Add ${wrappedTicker()} to your wallet, then use Swap.`,
			url: 'https://solflare.com'
		}
	];

	const linkClass = 'rounded-lg px-3 py-2 text-center text-sm font-semibold transition-colors';
</script>

<svelte:head><title>Buy / Sell {wrappedTicker()}</title></svelte:head>

<div class="flex min-h-screen items-center justify-center bg-zinc-50 px-4 py-12 dark:bg-zinc-950">
	<div class="w-full max-w-md">
		<PageTabs active="trade" />
		<div class="mb-6 text-center">
			<h1 class="text-2xl font-bold text-zinc-900 dark:text-white">Buy / Sell {wrappedTicker()}</h1>
			<p class="mt-1 text-sm text-zinc-500">Trusted places to trade {wrappedTicker()} on Solana</p>
		</div>

		<div class="flex flex-col gap-3">
			{#each venues as venue}
				<div
					class="rounded-2xl bg-white p-4 shadow-2xl ring-1 ring-black/5 dark:bg-zinc-900 dark:ring-white/5"
				>
					<h2 class="text-base font-bold text-zinc-900 dark:text-white">{venue.name}</h2>
					<p class="mt-1 text-xs leading-relaxed text-zinc-500">{venue.description}</p>
					<div class="mt-3 grid gap-2 {venue.buyUrl ? 'grid-cols-2' : 'grid-cols-1'}">
						{#if venue.buyUrl}
							<a
								href={venue.buyUrl}
								target="_blank"
								rel="noopener noreferrer"
								class="{linkClass} bg-violet-500 text-white hover:bg-violet-600">Buy {wrappedTicker()} ↗</a
							>
							<a
								href={venue.sellUrl}
								target="_blank"
								rel="noopener noreferrer"
								class="{linkClass} bg-zinc-100 text-zinc-900 hover:bg-zinc-200 dark:bg-zinc-800 dark:text-white dark:hover:bg-zinc-700"
								>Sell {wrappedTicker()} ↗</a
							>
						{:else}
							<a
								href={venue.url}
								target="_blank"
								rel="noopener noreferrer"
								class="{linkClass} bg-zinc-100 text-zinc-900 hover:bg-zinc-200 dark:bg-zinc-800 dark:text-white dark:hover:bg-zinc-700"
								>Get {venue.name} ↗</a
							>
						{/if}
					</div>
				</div>
			{/each}
		</div>

		<div class="mt-4 rounded-xl bg-zinc-100 px-4 py-3 dark:bg-zinc-800/60">
			<p class="text-xs font-medium text-zinc-500">{wrappedTicker()} token address</p>
			<p class="mt-1 font-mono text-xs break-all text-zinc-900 dark:text-white">{WECX_MINT}</p>
			<a
				href="https://solscan.io/token/{WECX_MINT}"
				target="_blank"
				rel="noopener noreferrer"
				class="mt-1 inline-block text-[10px] text-zinc-500 underline-offset-2 hover:text-zinc-900 hover:underline dark:hover:text-white"
				>View on Solscan ↗</a
			>
			<p class="mt-2 text-[10px] leading-relaxed text-zinc-500">
				Always verify the address matches before swapping.
			</p>
		</div>
	</div>
</div>
