<script lang="ts">
	import './layout.css';
	import ThemeToggle from '$lib/ThemeToggle.svelte';

	let { children } = $props();
</script>

<svelte:head><link rel="icon" type="image/svg+xml" href="/favicon.svg" /></svelte:head>
<ThemeToggle />
{@render children()}
