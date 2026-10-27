<script lang="ts">
	import QRCode from 'qrcode';

	// The order page's own URL, which is the only way back to the order: the order id, a copy
	// button, and a QR code for moving from a desktop to a phone (or back) without retyping a UUID.
	let { url, orderId }: { url: string; orderId: string } = $props();

	let copied = $state(false);
	let showQr = $state(false);
	let qrDataUrl = $state('');

	async function copyLink() {
		await navigator.clipboard.writeText(url);
		copied = true;
		setTimeout(() => (copied = false), 2000);
	}

	$effect(() => {
		if (showQr && url) {
			QRCode.toDataURL(url, { width: 160, margin: 1 }).then((data: string) => {
				qrDataUrl = data;
			});
		}
	});
</script>

<div class="mb-5 rounded-xl bg-zinc-100 px-3 py-2.5 dark:bg-zinc-800/60">
	<p class="mb-2 truncate text-xs text-zinc-600 dark:text-zinc-400" title={orderId}>
		Order ID: <span class="font-mono text-zinc-800 dark:text-zinc-200">{orderId}</span>
	</p>
	<div class="flex items-center gap-2">
		<p class="w-0 flex-1 text-xs text-zinc-600 dark:text-zinc-400">
			<span class="font-semibold text-zinc-800 dark:text-zinc-200">Keep this link.</span> It is the only
			way back to this order.
		</p>
		<button
			onclick={copyLink}
			class="shrink-0 rounded-lg px-2.5 py-1.5 text-xs font-medium transition-colors {copied
				? 'bg-green-500/20 text-green-600 dark:text-green-400'
				: 'bg-zinc-200 text-zinc-700 hover:bg-zinc-300 dark:bg-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-600'}"
		>
			{copied ? 'Copied!' : 'Copy link'}
		</button>
		<button
			onclick={() => (showQr = !showQr)}
			aria-expanded={showQr}
			class="shrink-0 rounded-lg bg-zinc-200 px-2.5 py-1.5 text-xs font-medium text-zinc-700 transition-colors hover:bg-zinc-300 dark:bg-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-600"
		>
			{showQr ? 'Hide QR' : 'QR'}
		</button>
	</div>
	{#if showQr && qrDataUrl}
		<div class="mt-3 flex flex-col items-center gap-1">
			<div class="rounded-lg bg-white p-2">
				<img src={qrDataUrl} alt="QR code of this order's link" width="160" height="160" />
			</div>
			<p class="text-[11px] text-zinc-500">Scan with another device to open this order there.</p>
		</div>
	{/if}
</div>
