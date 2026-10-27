<script lang="ts">
	import { page } from '$app/state';
	import QRCode from 'qrcode';
	import { saveStoredOrder } from '$lib/orderHistory';
	import { wrappedTicker } from '$lib/tickers';
	import { ecxTicker, presentSatAmount, formatBaseUnits } from '$lib/units.svelte';
	import OrderProgress, { type Step } from '$lib/OrderProgress.svelte';
	import OrderLink from '$lib/OrderLink.svelte';
	import TxLink from '$lib/TxLink.svelte';

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
		solStipendLamports: number;
		solStipendPriceSat: number;
		ataRentLamports: number;
		ataRentPriceSat: number;
		depositConfirmations: number;
		refundReason: string;
	}

	const UNIT_SCALE = 1e8;

	function formatExact(units: number, decimals: number): string {
		const abs = Math.abs(Math.trunc(units));
		const scale = 10 ** decimals;
		const whole = Math.floor(abs / scale);
		const frac = String(abs % scale)
			.padStart(decimals, '0')
			.replace(/0+$/, '');
		return frac ? `${whole}.${frac}` : String(whole);
	}

	let order = $state<Order | null>(null);
	let loadError = $state('');
	let qrDataUrl = $state('');
	let copied = $state(false);

	let wecxDecimals = $state<number | null>(null);
	let orderLifetimeMinutes = $state<number | null>(null);
	let requiredConfirmations = $state<number | null>(null);
	let ecxExplorerTxUrl = $state<string | null>(null);
	let solanaExplorerTxUrl = $state<string | null>(null);
	$effect(() => {
		fetch('/bridge/quote')
			.then((res) => (res.ok ? res.json() : null))
			.then((data) => {
				if (!data) return;
				wecxDecimals = data.wecxDecimals;
				orderLifetimeMinutes = data.orderLifetimeMinutes;
				requiredConfirmations = data.requiredConfirmations;
				ecxExplorerTxUrl = data.ecxExplorerTxUrl;
				solanaExplorerTxUrl = data.solanaExplorerTxUrl;
			});
	});

	// ECX blocks arrive about every ten minutes, so the wait for confirmations is the long part
	// of a wrap; the wording keeps it approximate.
	const MINUTES_PER_BLOCK = 10;

	let confirmationText = $derived.by(() => {
		if (!order || requiredConfirmations == null) return 'Waiting for confirmations';
		const have = Math.min(order.depositConfirmations, requiredConfirmations);
		return `${have} of ${requiredConfirmations} confirmations`;
	});

	let progress = $derived.by((): { steps: Step[]; current: number; failed: boolean } | null => {
		if (!order) return null;
		const waitMinutes =
			requiredConfirmations != null
				? `about ${requiredConfirmations * MINUTES_PER_BLOCK} minutes on ${ecxTicker()}`
				: 'a few blocks';
		const steps: Step[] = [
			{
				label: `Send ${ecxTicker()} to the deposit address`,
				hint: orderLifetimeMinutes != null ? `Within ${orderLifetimeMinutes} minutes of creating the order.` : undefined
			},
			{ label: 'Deposit seen on the network', hint: 'Usually within a minute of sending.' },
			{ label: confirmationText, hint: `Takes ${waitMinutes}. Nothing to do; keep this page open or come back.` },
			{ label: `${wrappedTicker()} sent to your Solana address`, hint: 'A Solana transaction, usually under a minute.' }
		];
		switch (order.status) {
			case 'pending':
				return { steps, current: 0, failed: false };
			case 'deposit_seen':
				return { steps, current: 2, failed: false };
			case 'deposit_confirmed':
			case 'paying_out':
				return { steps, current: 3, failed: false };
			case 'paid_out':
				return { steps, current: 4, failed: false };
			case 'expired':
				return { steps, current: 0, failed: true };
			case 'refund_requested':
			case 'refunding':
			case 'refunded': {
				const refundSteps: Step[] = [
					steps[0],
					steps[1],
					{ label: 'Deposit was formatted wrong', hint: 'A restocking fee is kept. The rest goes back to the address it came from.' },
					{ label: `${ecxTicker()} sent back to the address it came from`, hint: 'An ECX transaction; a few minutes.' }
				];
				// The step that went wrong is marked, and the refund that follows is an ordinary step;
				// once refunded everything is done.
				return { steps: refundSteps, current: order.status === 'refunded' ? 4 : 2, failed: order.status !== 'refunded' };
			}
			case 'failed':
				return { steps, current: 3, failed: true };
			default:
				return { steps, current: 0, failed: false };
		}
	});

	// The page's own address, which is the order's only credential; see OrderLink.
	let orderUrl = $derived(page.url.href);

	let orderId = $derived(page.params.id ?? '');

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

	// Re-fetch while the order can still change. The backend throttles the chain-explorer sync a
	// GET can trigger to one per 10 s across all callers, so polling costs it a DB read, not a
	// network call. Expired orders are included: a deposit that arrives after the window revives
	// the order, and the GET is what makes the backend look for it.
	const TERMINAL_STATUSES = new Set(['paid_out', 'refunded', 'failed']);
	const POLL_INTERVAL_MS = 20_000;

	$effect(() => {
		fetchOrder();
	});

	$effect(() => {
		if (!order || TERMINAL_STATUSES.has(order.status)) return;
		const timer = setInterval(fetchOrder, POLL_INTERVAL_MS);
		return () => clearInterval(timer);
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

	function amountOut(order: Order): string {
		return wecxDecimals != null ? formatBaseUnits(order.amountOutBaseUnits, wecxDecimals) : '…';
	}
</script>

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
					{ecxTicker()} → {amountOut(order)} {wrappedTicker()}
				</h1>
				{#if order.ataRentPriceSat > 0}
					<p class="mx-auto mt-3 max-w-md text-sm text-zinc-600 dark:text-zinc-400">
						This address had no {wrappedTicker()} account, so {presentSatAmount(order.ataRentPriceSat)}
						{ecxTicker()} in the amount above pays for creating it. That cost matches the SOL rent 1:1
						and is not charged the wrap fee.
					</p>
				{/if}
				{#if order.solStipendLamports > 0}
					<p class="mx-auto mt-3 max-w-md text-sm text-zinc-600 dark:text-zinc-400">
						Includes {formatExact(order.solStipendLamports, 9)} SOL,
						paid for with {presentSatAmount(order.solStipendPriceSat)} {ecxTicker()} in the amount
						above. That SOL arrives in the same transaction as the {wrappedTicker()}.
					</p>
				{/if}
			</div>

			<div
				class="rounded-2xl bg-white p-6 shadow-2xl ring-1 ring-black/10 dark:bg-zinc-900 dark:ring-white/10"
			>
				{#if orderUrl}
					<OrderLink url={orderUrl} orderId={order.id} />
				{/if}
				{#if progress}
					<OrderProgress steps={progress.steps} current={progress.current} failed={progress.failed} />
				{/if}
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
						This page checks for your deposit every 20 seconds and updates once it is detected.
					</p>
				{:else if order.status === 'deposit_seen'}
					<p class="text-center text-sm text-zinc-700 dark:text-zinc-300">
						Deposit seen — {confirmationText.toLowerCase()}
					</p>
					{#if order.depositTxid}
						<TxLink id={order.depositTxid} baseUrl={ecxExplorerTxUrl} label="View deposit on explorer" />
					{/if}
				{:else if order.status === 'deposit_confirmed' || order.status === 'paying_out'}
					<p class="text-center text-sm text-zinc-700 dark:text-zinc-300">
						Deposit confirmed — sending your {wrappedTicker()} now…
					</p>
					{#if order.depositTxid}
						<TxLink id={order.depositTxid} baseUrl={ecxExplorerTxUrl} label="View deposit on explorer" />
					{/if}
				{:else if order.status === 'paid_out'}
					<p class="text-center text-sm font-medium text-green-600 dark:text-green-400">
						{wrappedTicker()} sent!
					</p>
					<p class="mt-2 text-center text-xs text-zinc-500">
						You received {amountOut(order)} {wrappedTicker()} at {order.solanaRecipient}
					</p>
					{#if order.payoutSignature}
						<TxLink id={order.payoutSignature} baseUrl={solanaExplorerTxUrl} label="View payout on Solscan" />
					{/if}
				{:else if order.status === 'refund_requested' || order.status === 'refunding'}
					<p class="text-center text-sm text-zinc-700 dark:text-zinc-300">
						{order.refundReason || 'The deposit was formatted wrong.'}
					</p>
					<p class="mt-2 text-center text-xs text-zinc-500">
						{#if order.refundAddress}
							Sending the rest back to {order.refundAddress}…
						{:else}
							Sending the rest back to the address it came from…
						{/if}
					</p>
					{#if order.depositTxid}
						<TxLink id={order.depositTxid} baseUrl={ecxExplorerTxUrl} label="View deposit on explorer" />
					{/if}
				{:else if order.status === 'refunded'}
					<p class="text-center text-sm font-medium text-green-600 dark:text-green-400">Sent back</p>
					<p class="mt-2 text-center text-xs text-zinc-500">
						{order.refundReason || 'The deposit was formatted wrong.'}
						{#if order.refundAddress}
							Returned to {order.refundAddress}.
						{/if}
					</p>
					{#if order.refundTxid}
						<TxLink id={order.refundTxid} baseUrl={ecxExplorerTxUrl} label="View refund on explorer" />
					{/if}
				{:else if order.status === 'expired'}
					<p class="text-center text-sm font-medium text-red-600 dark:text-red-400">
						This order expired before a deposit was seen.
					</p>
					<p class="mt-2 text-center text-xs text-zinc-600">
						Already sent {ecxTicker()} to {order.depositAddress}? Keep this page open: the deposit is
						still credited once it appears, and this order continues from there. Do not send again.
					</p>
				{:else if order.status === 'failed'}
					<p class="text-center text-sm font-medium text-red-600 dark:text-red-400">
						This order failed.
					</p>
				{/if}
			</div>

		{/if}
	</div>
</div>
