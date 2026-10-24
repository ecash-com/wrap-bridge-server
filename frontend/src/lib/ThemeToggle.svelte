<script lang="ts">
	import { currentTheme, toggleTheme } from '$lib/theme.svelte';
</script>

<button
	onclick={toggleTheme}
	aria-label={currentTheme() === 'dark' ? 'Switch to light mode' : 'Switch to dark mode'}
	class="fixed top-4 left-4 z-50 flex h-9 w-9 items-center justify-center rounded-full border border-zinc-300 bg-white text-zinc-500 shadow-sm transition-colors hover:text-zinc-900 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-400 dark:hover:text-white"
>
	{#if currentTheme() === 'dark'}
		<svg class="h-4 w-4" fill="none" viewBox="0 0 24 24" stroke="currentColor" stroke-width="2">
			<circle cx="12" cy="12" r="4" />
			<path
				stroke-linecap="round"
				d="M12 2v2m0 16v2M4.93 4.93l1.41 1.41m11.32 11.32l1.41 1.41M2 12h2m16 0h2M4.93 19.07l1.41-1.41m11.32-11.32l1.41-1.41"
			/>
		</svg>
	{:else}
		<svg class="h-4 w-4" fill="none" viewBox="0 0 24 24" stroke="currentColor" stroke-width="2">
			<path
				stroke-linecap="round"
				stroke-linejoin="round"
				d="M21 12.79A9 9 0 1111.21 3 7 7 0 0021 12.79z"
			/>
		</svg>
	{/if}
</button>
