<script lang="ts">
	import { page } from '$app/state';
	import { goto } from '$app/navigation';
	import {
		getStoredOrders,
		getStoredPegoutOrders,
		saveStoredOrder,
		saveStoredPegoutOrder
	} from '$lib/orderHistory';
	import {
		ecxTicker,
		ecxAssetLabel,
		presentAmount,
		presentSatAmount,
		parseDisplayAmount,
		formatBaseUnits
	} from '$lib/units.svelte';
	import UnitToggle from '$lib/UnitToggle.svelte';
	import PageTabs from '$lib/PageTabs.svelte';
	import { env } from '$env/dynamic/public';

	const wecxMint = env.PUBLIC_WECX_MINT || 'EVHqNdzjCupKi4rQkbuYw52sa1m8A7jeUAMP23S9AVVq';
	const solscanUrl = `https://solscan.io/token/${wecxMint}`;

	type Direction = 'pegin' | 'pegout';

	// Which way the bridge is running - ECX->wECX (pegin) or wECX->ECX (pegout). Both directions
	// live on this one page now (mirrors the old BTC/ECX swap's single-page-with-a-flip-arrow UI,
	// see main branch) rather than separate /bridge and /pegout form pages, since there are only
	// ever these two directions - no asset picker needed, just a binary flip.
	let direction = $state<Direction>(
		page.url.searchParams.get('direction') === 'pegout' ? 'pegout' : 'pegin'
	);

	// `amountIn` always holds the *currently-sent* asset's amount as a plain main-unit decimal
	// string, regardless of direction - what differs between directions is only which HTML input
	// it's bound to and how that input formats it (ECX honors the sats/ECX toggle via
	// presentAmount/parseDisplayAmount; wECX has no such toggle and binds directly).
	let amountIn = $state('');
	let recipientAddress = $state('');
	// Where the ECX deposit gets sent back if it can't be bridged (see /orders/[id]) - only
	// meaningful for pegin; peg-out has no equivalent field, since its refund destination is
	// recovered automatically from the deposit transaction itself.
	let refundAddress = $state('');

	let hasStoredOrders = $state(false);
	$effect(() => {
		hasStoredOrders = getStoredOrders().length > 0 || getStoredPegoutOrders().length > 0;
	});

	// Static parameters for both directions - fetched once each, not amount-dependent, so flipping
	// direction never needs a fresh round trip just to know the fee/decimals/minimum. Unlike the old
	// AMM swap, none of this can go stale between quoting and order creation either way: the bridge
	// is a flat 1:1-minus-fee peg in both directions, not a price that moves with trading activity.
	let peginFeeFraction = $state<number | null>(null);
	let peginFlatFeeSat = $state<number | null>(null);
	let pegoutFeeFraction = $state<number | null>(null);
	let wecxDecimals = $state<number | null>(null);
	let minAmountIn = $state<number | null>(null);
	let minAmountOutSat = $state<number | null>(null);

	$effect(() => {
		let cancelled = false;
		fetch('/bridge/quote')
			.then((res) => (res.ok ? res.json() : null))
			.then((data) => {
				if (!data || cancelled) return;
				peginFeeFraction = data.peginFeeFraction;
				peginFlatFeeSat = data.peginFlatFeeSat;
				wecxDecimals = data.wecxDecimals;
				minAmountIn = data.minAmountIn;
			});
		return () => {
			cancelled = true;
		};
	});

	$effect(() => {
		let cancelled = false;
		fetch('/pegout/quote')
			.then((res) => (res.ok ? res.json() : null))
			.then((data) => {
				if (!data || cancelled) return;
				pegoutFeeFraction = data.pegoutFeeFraction;
				minAmountOutSat = data.minAmountOutSat;
			});
		return () => {
			cancelled = true;
		};
	});

	let amountOutBaseUnits = $state<number | null>(null); // pegin output (wECX)
	let amountOutSat = $state<number | null>(null); // pegout output (ECX)
	let quoteToken = 0;
	let quoting = $state(false);
	let debounceTimer: ReturnType<typeof setTimeout> | undefined;

	function toBaseUnits(amountDecimal: string, decimals: number): number | null {
		const v = parseFloat(amountDecimal);
		if (!Number.isFinite(v) || v <= 0) return null;
		return Math.round(v * 10 ** decimals);
	}

	function onAmountInput(value: string) {
		amountIn = value;
		const v = parseFloat(value);
		if (!v || isNaN(v)) {
			amountOutBaseUnits = null;
			amountOutSat = null;
			quoting = false;
			clearTimeout(debounceTimer);
			return;
		}

		quoting = true;
		clearTimeout(debounceTimer);
		debounceTimer = setTimeout(async () => {
			const token = ++quoteToken;
			if (direction === 'pegin') {
				const res = await fetch(`/bridge/quote?amount=${encodeURIComponent(value)}`);
				if (token !== quoteToken) return;
				quoting = false;
				if (res.ok) {
					const data = await res.json();
					amountOutBaseUnits = data.amountOutBaseUnits;
				}
			} else {
				if (wecxDecimals == null) return;
				const baseUnits = toBaseUnits(value, wecxDecimals);
				if (baseUnits == null) return;
				const res = await fetch(`/pegout/quote?amountBaseUnits=${baseUnits}`);
				if (token !== quoteToken) return;
				quoting = false;
				if (res.ok) {
					const data = await res.json();
					amountOutSat = data.amountOutSat;
				}
			}
		}, 300);
	}

	// Swaps which direction is active. The last computed *output* becomes the new input estimate
	// (mirrors the old swap's flip()), but the recipient/refund addresses are always cleared rather
	// than carried across - they're addresses on two different chains, and reusing one after a flip
	// would silently point real money at the wrong chain's address format.
	function flip() {
		const nextAmountIn =
			direction === 'pegin'
				? amountOutBaseUnits != null && wecxDecimals != null
					? (amountOutBaseUnits / 10 ** wecxDecimals).toString()
					: ''
				: amountOutSat != null
					? (amountOutSat / 1e8).toString()
					: '';

		direction = direction === 'pegin' ? 'pegout' : 'pegin';
		amountOutBaseUnits = null;
		amountOutSat = null;
		recipientAddress = '';
		refundAddress = '';
		submitError = '';
		clearTimeout(debounceTimer);
		quoting = false;
		amountIn = '';
		if (nextAmountIn) onAmountInput(nextAmountIn);
	}

	let belowMinimum = $derived.by(() => {
		if (direction === 'pegin') {
			const v = parseFloat(amountIn);
			return minAmountIn != null && Number.isFinite(v) && v > 0 && v < minAmountIn;
		}
		return (
			amountOutSat != null &&
			minAmountOutSat != null &&
			amountOutSat > 0 &&
			amountOutSat < minAmountOutSat
		);
	});

	let amountOutDisplay = $derived.by(() => {
		if (direction === 'pegin') {
			return amountOutBaseUnits != null && wecxDecimals != null
				? formatBaseUnits(amountOutBaseUnits, wecxDecimals)
				: '';
		}
		return amountOutSat != null ? presentSatAmount(amountOutSat) : '';
	});

	function formatPercent(fraction: number): string {
		return `${(fraction * 100).toFixed(fraction < 0.01 ? 2 : 1)}%`;
	}

	let submitting = $state(false);
	let submitError = $state('');

	async function submitOrder() {
		submitError = '';
		submitting = true;

		try {
			if (direction === 'pegin') {
				const res = await fetch('/bridge/orders', {
					method: 'POST',
					headers: { 'Content-Type': 'application/json' },
					body: JSON.stringify({
						amountInSat: Math.round(parseFloat(amountIn) * 1e8),
						solanaRecipient: recipientAddress,
						refundAddress
					})
				});
				if (!res.ok) {
					const err = await res.json().catch(() => ({}));
					throw new Error((err as { message?: string }).message ?? `Server error ${res.status}`);
				}
				const data = (await res.json()) as {
					orderId: string;
					amountInSat: number;
					amountOutBaseUnits: number;
				};
				// Use the amounts the server actually locked in for this order, not the client's last
				// quoted estimate.
				saveStoredOrder({
					id: data.orderId,
					amountInSat: data.amountInSat,
					amountOutBaseUnits: data.amountOutBaseUnits
				});
				await goto(`/orders/${data.orderId}`);
			} else {
				if (wecxDecimals == null) return;
				const amountInBaseUnits = toBaseUnits(amountIn, wecxDecimals);
				if (amountInBaseUnits == null) return;

				const res = await fetch('/pegout/orders', {
					method: 'POST',
					headers: { 'Content-Type': 'application/json' },
					body: JSON.stringify({ amountInBaseUnits, ecxRecipient: recipientAddress })
				});
				if (!res.ok) {
					const err = await res.json().catch(() => ({}));
					throw new Error((err as { message?: string }).message ?? `Server error ${res.status}`);
				}
				const data = (await res.json()) as {
					orderId: string;
					amountInBaseUnits: number;
					amountOutSat: number;
				};
				saveStoredPegoutOrder({
					id: data.orderId,
					amountInBaseUnits: data.amountInBaseUnits,
					amountOutSat: data.amountOutSat
				});
				await goto(`/pegout/orders/${data.orderId}`);
			}
		} catch (e: unknown) {
			submitError = e instanceof Error ? e.message : 'Failed to create order';
			submitting = false;
		}
	}
</script>

<div class="flex min-h-screen items-center justify-center bg-zinc-50 px-4 py-12 dark:bg-zinc-950">
	<div class="w-full max-w-md">
		<PageTabs active="bridge" />
		<!-- Header -->
		<div class="relative mb-6 text-center">
			<div class="absolute top-0 right-0">
				{#if hasStoredOrders}
					<a
						href="/orders"
						class="inline-flex items-center gap-1.5 text-xs font-medium text-zinc-500 transition-colors hover:text-zinc-900 dark:hover:text-white"
					>
						Your orders
						<svg
							class="h-3.5 w-3.5"
							fill="none"
							viewBox="0 0 24 24"
							stroke="currentColor"
							stroke-width="2.5"
						>
							<path stroke-linecap="round" stroke-linejoin="round" d="M9 5l7 7-7 7" />
						</svg>
					</a>
				{/if}
			</div>
			<h1 class="text-2xl font-bold text-zinc-900 dark:text-white">Bridge</h1>
			<p class="mt-1 text-sm text-zinc-500">
				{direction === 'pegin'
					? 'Bridge ECX to wbECX on Solana'
					: 'Redeem wbECX on Solana back to ECX'}
			</p>
		</div>

		<!-- Warning banner -->
		<div
			class="mb-4 flex items-start gap-3 rounded-xl border border-yellow-500/30 bg-yellow-500/10 px-4 py-3"
		>
			<svg
				class="mt-0.5 h-4 w-4 shrink-0 text-yellow-600 dark:text-yellow-400"
				fill="none"
				viewBox="0 0 24 24"
				stroke="currentColor"
				stroke-width="2"
			>
				<path
					stroke-linecap="round"
					stroke-linejoin="round"
					d="M12 9v2m0 4h.01M10.29 3.86L1.82 18a2 2 0 001.71 3h16.94a2 2 0 001.71-3L13.71 3.86a2 2 0 00-3.42 0z"
				/>
			</svg>
			<p class="text-xs leading-relaxed text-yellow-800 dark:text-yellow-300">
				<span class="font-semibold">Beta warning:</span> This bridge sends real
				<span class="font-semibold">Betanet eCash</span> and real
				<span class="font-semibold">wbECX on Solana mainnet-beta</span>. Both are real money.
				{#if direction === 'pegout'}
					Your deposit must be sent from a wallet that supports
					<span class="font-semibold">Solana Pay</span> (e.g. Phantom, Solflare) via the link/QR on the
					next page - a plain manual transfer to the treasury address won't be matched to your order.
				{/if}
			</p>
		</div>

		<!-- Card -->
		<div
			class="rounded-2xl bg-white p-1 shadow-2xl ring-1 ring-black/5 dark:bg-zinc-900 dark:ring-white/5"
		>
			<!-- Wrap / Unwrap toggle -->
			<div class="mb-1 grid grid-cols-2 gap-1 rounded-xl bg-zinc-100 p-1 dark:bg-zinc-800/60">
				{#each [{ value: 'pegin', label: 'Wrap' }, { value: 'pegout', label: 'Unwrap' }] as const as option}
					<button
						onclick={() => direction !== option.value && flip()}
						aria-pressed={direction === option.value}
						class="rounded-lg py-2 text-sm font-semibold transition-colors {direction ===
						option.value
							? 'bg-white text-zinc-900 shadow-sm dark:bg-zinc-700 dark:text-white'
							: 'text-zinc-500 hover:text-zinc-900 dark:hover:text-white'}"
					>
						{option.label}
					</button>
				{/each}
			</div>

			<!-- From panel -->
			<div class="rounded-xl bg-zinc-100 p-4 dark:bg-zinc-800/60">
				{#if direction === 'pegin'}
					<div class="mb-2 flex items-center justify-between">
						<div class="flex items-center gap-2">
							<span class="text-xs font-medium text-zinc-500">You send</span>
							<UnitToggle />
						</div>
						<span class="text-xs text-zinc-600">
							{#if minAmountIn != null}Min: {presentAmount(minAmountIn)} {ecxTicker()}{/if}
						</span>
					</div>
					<div class="flex items-center gap-3">
						<input
							type="number"
							inputmode="decimal"
							placeholder="0.00"
							min="0"
							value={presentAmount(amountIn)}
							oninput={(e) => onAmountInput(parseDisplayAmount(e.currentTarget.value))}
							class="w-0 flex-1 [appearance:textfield] bg-transparent text-3xl font-semibold transition-opacity outline-none placeholder:text-zinc-600 [&::-webkit-inner-spin-button]:appearance-none [&::-webkit-outer-spin-button]:appearance-none {belowMinimum
								? 'text-red-600 dark:text-red-400'
								: 'text-zinc-900 dark:text-white'} {quoting ? 'opacity-60' : ''}"
						/>
						<div class="flex shrink-0 flex-col items-end gap-1">
							<span
								class="flex items-center gap-2 rounded-full bg-amber-500/15 py-1.5 pr-4 pl-2 font-bold text-amber-600 dark:text-amber-400"
							>
								<img src="/beta_logo_vector.svg" alt="" class="h-9 w-9" />
								<span class="text-sm font-semibold text-zinc-900 dark:text-white"
									>{ecxAssetLabel()}</span
								>
							</span>
							<a
								href="https://explorer.ecash.com"
								target="_blank"
								rel="noopener noreferrer"
								class="text-[10px] text-zinc-500 underline-offset-2 hover:text-zinc-900 hover:underline dark:hover:text-white"
								>View on Explorer ↗</a
							>
						</div>
					</div>
					{#if belowMinimum}
						<div class="mt-2 text-xs font-medium text-red-600 dark:text-red-400">
							Minimum bridge amount is {presentAmount(minAmountIn ?? 0)}
							{ecxTicker()}
						</div>
					{/if}
				{:else}
					<div class="mb-2 flex items-center justify-between">
						<span class="text-xs font-medium text-zinc-500">You send</span>
					</div>
					<div class="flex items-center gap-3">
						<input
							type="number"
							inputmode="decimal"
							placeholder="0.00"
							min="0"
							bind:value={amountIn}
							oninput={(e) => onAmountInput(e.currentTarget.value)}
							class="w-0 flex-1 [appearance:textfield] bg-transparent text-3xl font-semibold text-zinc-900 transition-opacity outline-none placeholder:text-zinc-600 dark:text-white [&::-webkit-inner-spin-button]:appearance-none [&::-webkit-outer-spin-button]:appearance-none {quoting
								? 'opacity-60'
								: ''}"
						/>
						<div class="flex shrink-0 flex-col items-end gap-1">
							<span
								class="flex items-center gap-2 rounded-full bg-violet-500/15 py-1.5 pr-4 pl-2 font-bold text-violet-600 dark:text-violet-400"
							>
								<img src="/wbecx-logo.svg" alt="" class="h-9 w-9" />
								<span class="text-sm font-semibold text-zinc-900 dark:text-white">wbECX (Sol)</span>
							</span>
							<a
								href={solscanUrl}
								target="_blank"
								rel="noopener noreferrer"
								class="text-[10px] text-zinc-500 underline-offset-2 hover:text-zinc-900 hover:underline dark:hover:text-white"
								>View on Solscan ↗</a
							>
						</div>
					</div>
				{/if}
			</div>

			<!-- Flip -->
			<div class="relative z-10 flex items-center justify-center py-1">
				<div
					class="absolute inset-x-0 top-1/2 -translate-y-1/2 border-t border-zinc-200 dark:border-zinc-800"
				></div>
				<button
					onclick={flip}
					aria-label="Switch bridge direction"
					class="relative flex h-9 w-9 items-center justify-center rounded-full border border-zinc-300 bg-white text-zinc-500 transition-all hover:border-zinc-400 hover:text-zinc-900 active:scale-95 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-400 dark:hover:border-zinc-500 dark:hover:text-white"
				>
					<svg
						class="h-4 w-4"
						fill="none"
						viewBox="0 0 24 24"
						stroke="currentColor"
						stroke-width="2.5"
					>
						<path
							stroke-linecap="round"
							stroke-linejoin="round"
							d="M7 16V4m0 0L3 8m4-4l4 4M17 8v12m0 0l4-4m-4 4l-4-4"
						/>
					</svg>
				</button>
			</div>

			<!-- To panel -->
			<div class="rounded-xl bg-zinc-50 p-4 dark:bg-zinc-800/30">
				{#if direction === 'pegin'}
					<div class="mb-2 flex items-center justify-between">
						<span class="text-xs font-medium text-zinc-500">You receive</span>
					</div>
					<div class="flex items-center gap-3">
						<div
							class="w-0 flex-1 truncate text-3xl font-semibold text-zinc-900 transition-opacity dark:text-white {quoting
								? 'opacity-60'
								: ''}"
						>
							{amountOutDisplay || '0.00'}
						</div>
						<div class="flex shrink-0 flex-col items-end gap-1">
							<span
								class="flex items-center gap-2 rounded-full bg-violet-500/15 py-1.5 pr-4 pl-2 font-bold text-violet-600 dark:text-violet-400"
							>
								<img src="/wbecx-logo.svg" alt="" class="h-9 w-9" />
								<span class="text-sm font-semibold text-zinc-900 dark:text-white">wbECX (Sol)</span>
							</span>
							<a
								href={solscanUrl}
								target="_blank"
								rel="noopener noreferrer"
								class="text-[10px] text-zinc-500 underline-offset-2 hover:text-zinc-900 hover:underline dark:hover:text-white"
								>View on Solscan ↗</a
							>
						</div>
					</div>
				{:else}
					<div class="mb-2 flex items-center justify-between">
						<div class="flex items-center gap-2">
							<span class="text-xs font-medium text-zinc-500">You receive</span>
							<UnitToggle />
						</div>
						<span class="text-xs text-zinc-600">
							{#if minAmountOutSat != null}Min: {presentSatAmount(minAmountOutSat)}
								{ecxTicker()}{/if}
						</span>
					</div>
					<div class="flex items-center gap-3">
						<div
							class="w-0 flex-1 truncate text-3xl font-semibold transition-opacity {belowMinimum
								? 'text-red-600 dark:text-red-400'
								: 'text-zinc-900 dark:text-white'} {quoting ? 'opacity-60' : ''}"
						>
							{amountOutDisplay || '0.00'}
						</div>
						<div class="flex shrink-0 flex-col items-end gap-1">
							<span
								class="flex items-center gap-2 rounded-full bg-amber-500/15 py-1.5 pr-4 pl-2 font-bold text-amber-600 dark:text-amber-400"
							>
								<img src="/beta_logo_vector.svg" alt="" class="h-9 w-9" />
								<span class="text-sm font-semibold text-zinc-900 dark:text-white"
									>{ecxAssetLabel()}</span
								>
							</span>
							<a
								href="https://explorer.ecash.com"
								target="_blank"
								rel="noopener noreferrer"
								class="text-[10px] text-zinc-500 underline-offset-2 hover:text-zinc-900 hover:underline dark:hover:text-white"
								>View on Explorer ↗</a
							>
						</div>
					</div>
					{#if belowMinimum}
						<div class="mt-2 text-xs font-medium text-red-600 dark:text-red-400">
							Minimum redeem amount is {presentSatAmount(minAmountOutSat ?? 0)}
							{ecxTicker()} out
						</div>
					{/if}
				{/if}
			</div>

			<!-- Rate + details -->
			<div class="mt-3 px-1 pb-1">
				<div class="flex items-center justify-between rounded-lg px-3 py-2 text-xs text-zinc-500">
					<span>Rate</span>
					<!-- Always in main units, independent of the szats/ECX display toggle: this states the
					     fixed peg itself, not a value scaled to the current unit (a "1 szat = 1 wECX"
					     reading would be off by 8 orders of magnitude). -->
					{#if direction === 'pegin'}
						<span
							>1 ECX = 1 wbECX{peginFeeFraction
								? ` (minus ${formatPercent(peginFeeFraction)} fee${
										peginFlatFeeSat
											? ` and ${presentSatAmount(peginFlatFeeSat)} ${ecxTicker()}`
											: ''
									})`
								: ''}</span
						>
					{:else}
						<span
							>1 wbECX = 1 {ecxTicker()}{pegoutFeeFraction
								? ` (minus ${formatPercent(pegoutFeeFraction)} fee)`
								: ''}</span
						>
					{/if}
				</div>
				<div class="flex items-center justify-between rounded-lg px-3 py-1.5 text-xs text-zinc-500">
					<span>Estimated time</span>
					<span>{direction === 'pegin' ? '~10 min' : '~1 min'}</span>
				</div>
			</div>
		</div>

		<!-- Recipient address -->
		<div
			class="mt-3 rounded-xl bg-white p-4 ring-1 ring-black/5 dark:bg-zinc-900 dark:ring-white/5"
		>
			<label for="recipient" class="mb-2 block text-xs font-medium text-zinc-500">
				{direction === 'pegin' ? 'Receive wbECX (Sol) at' : `Receive ${ecxTicker()} (betanet) at`}
			</label>
			<input
				id="recipient"
				type="text"
				bind:value={recipientAddress}
				placeholder={direction === 'pegin' ? 'Solana address' : 'ecash:…'}
				class="w-full bg-transparent font-mono text-sm text-zinc-900 outline-none placeholder:text-zinc-500 dark:text-white dark:placeholder:text-zinc-600"
			/>
		</div>

		<!-- Refund address (pegin only - peg-out recovers its refund destination automatically from
		     the deposit transaction itself, see backend/src/solana.rs's find_reference_deposit) -->
		{#if direction === 'pegin'}
			<div
				class="mt-3 rounded-xl bg-white p-4 ring-1 ring-black/5 dark:bg-zinc-900 dark:ring-white/5"
			>
				<label for="refund-address" class="mb-2 block text-xs font-medium text-zinc-500">
					Refund {ecxTicker()} (betanet) to (optional)
				</label>
				<input
					id="refund-address"
					type="text"
					bind:value={refundAddress}
					placeholder="ecash:…"
					class="w-full bg-transparent font-mono text-sm text-zinc-900 outline-none placeholder:text-zinc-500 dark:text-white dark:placeholder:text-zinc-600"
				/>
				<p class="mt-2 text-xs text-zinc-600">
					Used only if your deposit can't be bridged (e.g. it's too small after fees). Without one,
					a deposit that can't be bridged just sits for manual review.
				</p>
			</div>
		{/if}

		{#if submitError}
			<p class="mt-3 text-center text-xs font-medium text-red-600 dark:text-red-400">
				{submitError}
			</p>
		{/if}

		<!-- Submit CTA -->
		<button
			onclick={submitOrder}
			class="mt-3 w-full rounded-xl py-4 text-lg font-semibold text-white shadow-lg transition-all active:scale-[0.98] disabled:cursor-not-allowed disabled:opacity-40 {direction ===
			'pegin'
				? 'bg-orange-500 shadow-orange-500/20 hover:bg-orange-400'
				: 'bg-violet-500 shadow-violet-500/20 hover:bg-violet-400'}"
			disabled={!amountIn || !recipientAddress.trim() || belowMinimum || quoting || submitting}
		>
			{#if submitting}
				Creating order…
			{:else if !amountIn}
				Enter an amount
			{:else if quoting}
				Getting quote…
			{:else if belowMinimum}
				{direction === 'pegin' ? 'Below minimum bridge amount' : 'Below minimum redeem amount'}
			{:else if !recipientAddress.trim()}
				{direction === 'pegin' ? 'Enter a Solana address' : `Enter an ${ecxTicker()} address`}
			{:else}
				{direction === 'pegin' ? 'Bridge' : 'Redeem'}
			{/if}
		</button>

		<!-- Footnote -->
		<p class="mt-4 text-center text-xs text-zinc-600">
			Rates are indicative. Final amount confirmed at execution.
		</p>
	</div>
</div>
