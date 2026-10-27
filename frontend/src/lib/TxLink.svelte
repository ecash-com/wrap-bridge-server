<script lang="ts">
	// A transaction id the customer can verify on a block explorer, or plain text when the
	// operator turned explorer links off (empty base URL). Opens in a new tab with no referrer:
	// the order URL in this page's address bar is the order's credential.
	let {
		id,
		baseUrl,
		label = 'View on explorer'
	}: {
		id: string;
		baseUrl: string | null;
		label?: string;
	} = $props();
</script>

<div class="mt-2 text-center">
	<p class="truncate font-mono text-[10px] text-zinc-600" title={id}>{id}</p>
	{#if baseUrl}
		<a
			href="{baseUrl}/{id}"
			target="_blank"
			rel="noopener noreferrer"
			class="mt-1 inline-flex items-center gap-1 text-xs font-medium text-zinc-700 underline decoration-zinc-400 underline-offset-2 hover:text-zinc-900 dark:text-zinc-300 dark:hover:text-white"
		>
			{label}
			<svg class="h-3 w-3" fill="none" viewBox="0 0 24 24" stroke="currentColor" stroke-width="2">
				<path
					stroke-linecap="round"
					stroke-linejoin="round"
					d="M10 6H6a2 2 0 00-2 2v10a2 2 0 002 2h10a2 2 0 002-2v-4M14 4h6m0 0v6m0-6L10 14"
				/>
			</svg>
		</a>
	{/if}
</div>
