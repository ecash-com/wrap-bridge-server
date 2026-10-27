<script lang="ts">
	// The order's journey as a short list of steps, with the current one highlighted. Driven by
	// the backend status string, so it cannot disagree with what the backend will do next.
	export interface Step {
		label: string;
		/// Shown under the label while this step is current. Expectations, not promises.
		hint?: string;
	}

	let {
		steps,
		current,
		failed = false
	}: {
		steps: Step[];
		/// Index of the step in progress. Steps before it are done. Equal to steps.length means
		/// everything is done.
		current: number;
		/// The current step ended badly (refund, expiry, failure).
		failed?: boolean;
	} = $props();
</script>

<ol class="mb-5 flex flex-col gap-2" aria-label="Order progress">
	{#each steps as step, index (step.label)}
		{@const done = index < current}
		{@const active = index === current}
		<li class="flex items-start gap-3">
			<span
				class="mt-0.5 flex h-5 w-5 shrink-0 items-center justify-center rounded-full text-[11px] font-bold {done
					? 'bg-green-500 text-white'
					: active
						? failed
							? 'bg-red-500 text-white'
							: 'bg-zinc-900 text-white ring-4 ring-zinc-900/15 dark:bg-white dark:text-zinc-900 dark:ring-white/20'
						: 'bg-zinc-200 text-zinc-500 dark:bg-zinc-700 dark:text-zinc-400'}"
				aria-hidden="true"
			>
				{#if done}
					<svg class="h-3 w-3" fill="none" viewBox="0 0 24 24" stroke="currentColor" stroke-width="3">
						<path stroke-linecap="round" stroke-linejoin="round" d="M5 13l4 4L19 7" />
					</svg>
				{:else if active && failed}
					!
				{:else}
					{index + 1}
				{/if}
			</span>
			<div class="min-w-0">
				<p
					class="text-sm {active
						? 'font-semibold text-zinc-900 dark:text-white'
						: done
							? 'text-zinc-700 dark:text-zinc-300'
							: 'text-zinc-400 dark:text-zinc-500'}"
				>
					{step.label}
				</p>
				{#if active && step.hint}
					<p class="mt-0.5 text-xs text-zinc-500 dark:text-zinc-400">{step.hint}</p>
				{/if}
			</div>
		</li>
	{/each}
</ol>
