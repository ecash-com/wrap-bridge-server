<script lang="ts">
	import { currentUnit, toggleUnit } from '$lib/units.svelte';
</script>

<button
	onclick={toggleUnit}
	aria-label={currentUnit() === 'smallest' ? 'Switch to bECX display' : 'Switch to szats display'}
	class="flex items-center gap-1 rounded-full border border-zinc-300 bg-white px-2 py-0.5 text-[11px] font-semibold text-zinc-500 shadow-sm transition-colors hover:text-zinc-900 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-400 dark:hover:text-white"
>
	<svg class="h-3 w-3" fill="none" viewBox="0 0 24 24" stroke="currentColor" stroke-width="2.5">
		<path
			stroke-linecap="round"
			stroke-linejoin="round"
			d="M8 7h12m0 0l-4-4m4 4l-4 4M16 17H4m0 0l4 4m-4-4l4-4"
		/>
	</svg>
	{currentUnit() === 'smallest' ? 'szats' : 'bECX'}
</button>
