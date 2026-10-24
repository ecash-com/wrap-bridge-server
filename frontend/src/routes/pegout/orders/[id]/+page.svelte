<script lang="ts">
	import { page } from '$app/state';
	import QRCode from 'qrcode';
	import { saveStoredPegoutOrder } from '$lib/orderHistory';
	import { ecxTicker, presentSatAmount, formatBaseUnits } from '$lib/units.svelte';
	import { buildSolanaPayUri } from '$lib/solanaPay';

	interface Order {
		id: string;
		amountInBaseUnits: number;
		amountOutSat: number;
		ecxRecipient: string;
		referencePubkey: string;
		status: string;
		depositSignature: string | null;
		depositorOwner: string | null;
		payoutTxid: string | null;
		refundSignature: string | null;
	}

	let order = $state<Order | null>(null);
	let loadError = $state('');
	let qrDataUrl = $state('');
	let copied = $state(false);

	let wecxDecimals = $state<number | null>(null);
	let treasuryPubkey = $state<string | null>(null);
	let wecxMint = $state<string | null>(null);
	$effect(() => {
		fetch('/pegout/quote')
			.then((res) => (res.ok ? res.json() : null))
			.then((data) => {
				if (!data) return;
				wecxDecimals = data.wecxDecimals;
				treasuryPubkey = data.treasuryPubkey;
				wecxMint = data.wecxMint;
			});
	});

	let orderId = $derived(page.params.id ?? '');

	async function fetchOrder() {
		try {
			const res = await fetch(`/pegout/orders/${encodeURIComponent(orderId)}`);
			if (!res.ok) {
				const err = await res.json().catch(() => ({}));
				throw new Error((err as { message?: string }).message ?? `Server error ${res.status}`);
			}
			order = (await res.json()) as Order;
			loadError = '';
			saveStoredPegoutOrder({
				id: order.id,
				amountInBaseUnits: order.amountInBaseUnits,
				amountOutSat: order.amountOutSat
			});
		} catch (e: unknown) {
			loadError = e instanceof Error ? e.message : 'Failed to load order';
		}
	}

	// Polling is disabled for now, same as the peg-in order page - each poll triggers a live
	// upstream Solana RPC check on the backend. Just fetch once on load; the customer can refresh
	// the page to get the latest status.
	$effect(() => {
		fetchOrder();
	});

	let solanaPayUri = $derived.by(() => {
		if (!order || wecxDecimals == null || !treasuryPubkey || !wecxMint) return '';
		return buildSolanaPayUri({
			recipient: treasuryPubkey,
			mint: wecxMint,
			reference: order.referencePubkey,
			amountBaseUnits: order.amountInBaseUnits,
			decimals: wecxDecimals,
			label: 'wbECX bridge redeem',
			message: `Redeem order ${order.id}`
		});
	});

	$effect(() => {
		if (order?.status === 'pending' && solanaPayUri) {
			QRCode.toDataURL(solanaPayUri, { width: 200, margin: 2 }).then((url: string) => {
				qrDataUrl = url;
			});
		} else {
			qrDataUrl = '';
		}
	});

	async function copyUri() {
		if (!solanaPayUri) return;
		await navigator.clipboard.writeText(solanaPayUri);
		copied = true;
		setTimeout(() => (copied = false), 2000);
	}

	function amountIn(order: Order): string {
		return wecxDecimals != null ? formatBaseUnits(order.amountInBaseUnits, wecxDecimals) : '…';
	}
</script>

<div class="flex min-h-screen items-center justify-center bg-zinc-50 px-4 py-12 dark:bg-zinc-950">
	<div class="w-full max-w-sm">
		{#if loadError && !order}
			<a
				href="/?direction=pegout"
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
				Back to redeem
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
				href="/?direction=pegout"
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
				Back to redeem
			</a>

			<div class="mb-6 text-center">
				<h1 class="text-2xl font-bold text-zinc-900 dark:text-white">
					{amountIn(order)} wbECX → {presentSatAmount(order.amountOutSat)}
					{ecxTicker()}
				</h1>
				<p class="mt-1 font-mono text-xs text-zinc-600">{order.id}</p>
			</div>

			<div
				class="rounded-2xl bg-white p-6 shadow-2xl ring-1 ring-black/10 dark:bg-zinc-900 dark:ring-white/10"
			>
				{#if order.status === 'pending'}
					<p class="mb-4 text-center text-xs text-zinc-500">
						Send {amountIn(order)} wbECX with a Solana Pay-compatible wallet to start redeeming
					</p>
					<div class="mb-4 flex justify-center">
						{#if qrDataUrl}
							<a href={solanaPayUri} class="rounded-xl bg-white p-3">
								<img src={qrDataUrl} alt="Solana Pay QR code" width="180" height="180" />
							</a>
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
							>{solanaPayUri || '…'}</span
						>
						<button
							onclick={copyUri}
							class="flex shrink-0 items-center gap-1.5 rounded-lg px-2.5 py-1.5 text-xs font-medium transition-colors {copied
								? 'bg-green-500/20 text-green-600 dark:text-green-400'
								: 'bg-zinc-200 text-zinc-700 hover:bg-zinc-300 hover:text-zinc-900 dark:bg-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-600 dark:hover:text-white'}"
						>
							{copied ? 'Copied!' : 'Copy'}
						</button>
					</div>
					<div
						class="mt-4 flex items-start gap-2.5 rounded-xl border border-yellow-500/30 bg-yellow-500/10 px-3 py-2.5"
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
						<p class="text-left text-xs leading-relaxed text-yellow-800 dark:text-yellow-300">
							Tap the code above, or scan it with your phone's <span class="font-semibold"
								>camera app</span
							>
							- not your wallet's own in-app scanner, since some wallets' scanners don't fill in the amount
							or token correctly. A plain manual transfer to the treasury address won't be matched to
							this order.
						</p>
					</div>
					<p class="mt-3 text-center text-xs text-zinc-600">
						This page will update automatically once your deposit is detected.
					</p>
				{:else if order.status === 'deposit_seen'}
					<p class="text-center text-sm text-zinc-700 dark:text-zinc-300">
						Deposit detected — waiting for finalization…
					</p>
				{:else if order.status === 'deposit_confirmed' || order.status === 'paying_out'}
					<p class="text-center text-sm text-zinc-700 dark:text-zinc-300">
						Deposit confirmed — sending your {ecxTicker()} now…
					</p>
				{:else if order.status === 'paid_out'}
					<p class="text-center text-sm font-medium text-green-600 dark:text-green-400">
						{ecxTicker()} sent!
					</p>
					<p class="mt-2 text-center text-xs text-zinc-500">
						You received {presentSatAmount(order.amountOutSat)}
						{ecxTicker()} at {order.ecxRecipient}
					</p>
					{#if order.payoutTxid}
						<p class="mt-2 truncate text-center font-mono text-[10px] text-zinc-600">
							{order.payoutTxid}
						</p>
					{/if}
				{:else if order.status === 'refund_requested' || order.status === 'refunding'}
					<p class="text-center text-sm text-zinc-700 dark:text-zinc-300">
						This deposit couldn't be bridged — sending your wbECX back…
					</p>
				{:else if order.status === 'refunded'}
					<p class="text-center text-sm font-medium text-green-600 dark:text-green-400">Refunded</p>
					{#if order.depositorOwner}
						<p class="mt-2 text-center text-xs text-zinc-500">
							Your wbECX deposit was sent back to {order.depositorOwner}
						</p>
					{/if}
					{#if order.refundSignature}
						<p class="mt-2 truncate text-center font-mono text-[10px] text-zinc-600">
							{order.refundSignature}
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
