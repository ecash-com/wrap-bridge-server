<script lang="ts">
	import { page } from '$app/state';
	import QRCode from 'qrcode';
	import { saveStoredOrder } from '$lib/orderHistory';
	import { ecxTicker, presentSatAmount, formatBaseUnits } from '$lib/units.svelte';

	interface Order {
		id: string;
		amountInSat: number;
		amountOutBaseUnits: number;
		depositAddress: string;
		solanaRecipient: string;
		refundAddress: string | null;
		status: string;
		depositTxid: string | null;
		payoutSignature: string | null;
		refundTxid: string | null;
	}

	const UNIT_SCALE = 1e8;

	let order = $state<Order | null>(null);
	let loadError = $state('');
	let qrDataUrl = $state('');
	let copied = $state(false);

	let wecxDecimals = $state<number | null>(null);
	$effect(() => {
		fetch('/bridge/quote')
			.then((res) => (res.ok ? res.json() : null))
			.then((data) => {
				if (data) wecxDecimals = data.wecxDecimals;
			});
	});

	let refundAddressInput = $state('');
	let refundAddressSubmitting = $state(false);
	let refundAddressError = $state('');

	// Statuses where a deposit could still plausibly end up refunded, so adding an address is
	// still useful - matches the backend's `set_refund_address` acceptance check.
	const REFUND_ADDRESS_SETTABLE_STATUSES = new Set([
		'pending',
		'deposit_seen',
		'deposit_confirmed',
		'paying_out'
	]);

	let orderId = $derived(page.params.id ?? '');
	let canSetRefundAddress = $derived(
		order != null && !order.refundAddress && REFUND_ADDRESS_SETTABLE_STATUSES.has(order.status)
	);

	async function fetchOrder() {
		try {
			const res = await fetch(`/orders/${encodeURIComponent(orderId)}`);
			if (!res.ok) {
				const err = await res.json().catch(() => ({}));
				throw new Error((err as { message?: string }).message ?? `Server error ${res.status}`);
			}
			order = (await res.json()) as Order;
			loadError = '';
			saveStoredOrder({
				id: order.id,
				amountInSat: order.amountInSat,
				amountOutBaseUnits: order.amountOutBaseUnits
			});
		} catch (e: unknown) {
			loadError = e instanceof Error ? e.message : 'Failed to load order';
		}
	}

	// Polling is disabled for now - each poll triggers a live upstream chain-explorer sync on the
	// backend. Just fetch once on load; the customer can refresh the page to get the latest status.
	$effect(() => {
		fetchOrder();
	});

	$effect(() => {
		const address = order?.depositAddress;
		if (order?.status === 'pending' && address) {
			// ECX uses plain BIP21-style Bitcoin-format addresses (see backend/src/keys.rs), so a
			// "bitcoin:" URI with the amount pre-filled is understood by wallets scanning it.
			const amount = order.amountInSat / UNIT_SCALE;
			const uri = `bitcoin:${address}?amount=${amount}`;
			QRCode.toDataURL(uri, { width: 200, margin: 2 }).then((url: string) => {
				qrDataUrl = url;
			});
		} else {
			qrDataUrl = '';
		}
	});

	async function copyAddress() {
		if (!order) return;
		await navigator.clipboard.writeText(order.depositAddress);
		copied = true;
		setTimeout(() => (copied = false), 2000);
	}

	async function submitRefundAddress() {
		if (!refundAddressInput.trim()) return;
		refundAddressSubmitting = true;
		refundAddressError = '';
		try {
			const res = await fetch(`/orders/${encodeURIComponent(orderId)}/refund-address`, {
				method: 'POST',
				headers: { 'Content-Type': 'application/json' },
				body: JSON.stringify({ refundAddress: refundAddressInput.trim() })
			});
			if (!res.ok) {
				const err = await res.json().catch(() => ({}));
				throw new Error((err as { message?: string }).message ?? `Server error ${res.status}`);
			}
			refundAddressInput = '';
			await fetchOrder();
		} catch (e: unknown) {
			refundAddressError = e instanceof Error ? e.message : 'Failed to set refund address';
		} finally {
			refundAddressSubmitting = false;
		}
	}

	function amountOut(order: Order): string {
		return wecxDecimals != null ? formatBaseUnits(order.amountOutBaseUnits, wecxDecimals) : '…';
	}
</script>

{#snippet refundAddressForm()}
	<div class="mt-4 rounded-xl bg-zinc-100 p-4 dark:bg-zinc-800/60">
		<label
			for="refund-address"
			class="mb-2 block text-xs font-medium text-zinc-600 dark:text-zinc-400"
		>
			Add a refund address on {ecxTicker()} (betanet)
		</label>
		<div class="flex gap-2">
			<input
				id="refund-address"
				type="text"
				bind:value={refundAddressInput}
				placeholder="Address to refund to if needed"
				disabled={refundAddressSubmitting}
				class="w-0 flex-1 rounded-lg border border-zinc-300 bg-white px-2.5 py-2 font-mono text-xs text-zinc-900 placeholder:text-zinc-400 focus:border-zinc-400 focus:outline-none disabled:opacity-50 dark:border-zinc-700 dark:bg-zinc-900 dark:text-white"
			/>
			<button
				onclick={submitRefundAddress}
				disabled={refundAddressSubmitting || !refundAddressInput.trim()}
				class="shrink-0 rounded-lg bg-zinc-200 px-3 py-2 text-xs font-medium text-zinc-700 transition-colors hover:bg-zinc-300 disabled:cursor-not-allowed disabled:opacity-50 dark:bg-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-600"
			>
				Save
			</button>
		</div>
		{#if refundAddressError}
			<p class="mt-2 text-xs font-medium text-red-600 dark:text-red-400">{refundAddressError}</p>
		{/if}
	</div>
{/snippet}

<div class="flex min-h-screen items-center justify-center bg-zinc-50 px-4 py-12 dark:bg-zinc-950">
	<div class="w-full max-w-sm">
		{#if loadError && !order}
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
			<div
				class="rounded-2xl bg-white p-6 text-center ring-1 ring-black/5 dark:bg-zinc-900 dark:ring-white/5"
			>
				<p class="text-sm text-red-600 dark:text-red-400">{loadError}</p>
			</div>
		{:else if !order}
			<div
				class="rounded-2xl bg-white p-6 text-center ring-1 ring-black/5 dark:bg-zinc-900 dark:ring-white/5"
			>
				<p class="text-sm text-zinc-500">Loading order…</p>
			</div>
		{:else}
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
				<h1 class="text-2xl font-bold text-zinc-900 dark:text-white">
					{presentSatAmount(order.amountInSat)}
					{ecxTicker()} → {amountOut(order)} wbECX
				</h1>
				<p class="mt-1 font-mono text-xs text-zinc-600">{order.id}</p>
			</div>

			<div
				class="rounded-2xl bg-white p-6 shadow-2xl ring-1 ring-black/10 dark:bg-zinc-900 dark:ring-white/10"
			>
				{#if order.status === 'pending'}
					<p class="mb-4 text-center text-xs text-zinc-500">
						Send {presentSatAmount(order.amountInSat)}
						{ecxTicker()} to this address to start bridging
					</p>
					<div class="mb-4 flex justify-center">
						{#if qrDataUrl}
							<div class="rounded-xl bg-white p-3">
								<img src={qrDataUrl} alt="Deposit QR code" width="180" height="180" />
							</div>
						{:else}
							<div
								class="flex h-[206px] w-[206px] items-center justify-center rounded-xl bg-zinc-100 dark:bg-zinc-800"
							>
								<span class="text-xs text-zinc-600">—</span>
							</div>
						{/if}
					</div>
					<div class="flex items-center gap-2 rounded-xl bg-zinc-100 px-3 py-2.5 dark:bg-zinc-800">
						<span class="w-0 flex-1 truncate font-mono text-xs text-zinc-700 dark:text-zinc-300"
							>{order.depositAddress}</span
						>
						<button
							onclick={copyAddress}
							class="flex shrink-0 items-center gap-1.5 rounded-lg px-2.5 py-1.5 text-xs font-medium transition-colors {copied
								? 'bg-green-500/20 text-green-600 dark:text-green-400'
								: 'bg-zinc-200 text-zinc-700 hover:bg-zinc-300 hover:text-zinc-900 dark:bg-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-600 dark:hover:text-white'}"
						>
							{copied ? 'Copied!' : 'Copy'}
						</button>
					</div>
					<p class="mt-4 text-center text-xs text-zinc-600">
						This page will update automatically once your deposit is detected.
					</p>
					{#if canSetRefundAddress}
						{@render refundAddressForm()}
					{/if}
				{:else if order.status === 'deposit_seen'}
					<p class="text-center text-sm text-zinc-700 dark:text-zinc-300">
						Deposit detected — waiting for confirmation…
					</p>
					{#if canSetRefundAddress}
						{@render refundAddressForm()}
					{/if}
				{:else if order.status === 'deposit_confirmed' || order.status === 'paying_out'}
					<p class="text-center text-sm text-zinc-700 dark:text-zinc-300">
						Deposit confirmed — sending your wbECX now…
					</p>
					{#if canSetRefundAddress}
						{@render refundAddressForm()}
					{/if}
				{:else if order.status === 'paid_out'}
					<p class="text-center text-sm font-medium text-green-600 dark:text-green-400">
						wbECX sent!
					</p>
					<p class="mt-2 text-center text-xs text-zinc-500">
						You received {amountOut(order)} wbECX at {order.solanaRecipient}
					</p>
					{#if order.payoutSignature}
						<p class="mt-2 truncate text-center font-mono text-[10px] text-zinc-600">
							{order.payoutSignature}
						</p>
					{/if}
				{:else if order.status === 'refund_requested' || order.status === 'refunding'}
					<p class="text-center text-sm text-zinc-700 dark:text-zinc-300">
						This deposit couldn't be bridged — sending your refund…
					</p>
				{:else if order.status === 'refunded'}
					<p class="text-center text-sm font-medium text-green-600 dark:text-green-400">Refunded</p>
					{#if order.refundAddress}
						<p class="mt-2 text-center text-xs text-zinc-500">
							Your {ecxTicker()} deposit was sent back to {order.refundAddress}
						</p>
					{/if}
					{#if order.refundTxid}
						<p class="mt-2 truncate text-center font-mono text-[10px] text-zinc-600">
							{order.refundTxid}
						</p>
					{/if}
				{:else if order.status === 'expired'}
					<p class="text-center text-sm font-medium text-red-600 dark:text-red-400">
						This order expired without a deposit.
					</p>
				{:else if order.status === 'failed'}
					<p class="text-center text-sm font-medium text-red-600 dark:text-red-400">
						This order failed.
					</p>
				{/if}
			</div>

			<p class="mt-4 text-center text-xs text-zinc-600">
				Bookmark this page — it's the only way back to this order.
			</p>
		{/if}
	</div>
</div>
