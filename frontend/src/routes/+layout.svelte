<script lang="ts">
	import './layout.css';
	import ThemeToggle from '$lib/ThemeToggle.svelte';
	import { assetTicker, wrappedTicker } from '$lib/tickers';

	let { children } = $props();
</script>

<svelte:head>
	<link rel="icon" type="image/svg+xml" href="/favicon.svg" />
	<title>{assetTicker()}/{wrappedTicker()} Bridge</title>
	<meta name="description" content="Bridge {assetTicker()} to {wrappedTicker()} on Solana." />
</svelte:head>
<ThemeToggle />
{@render children()}
