<script lang="ts">
	let { active }: { active: 'bridge' | 'trade' } = $props();

	const tabs = [
		{ id: 'trade', label: 'Buy / Sell', href: '/trade' },
		{ id: 'bridge', label: 'Wrap / Unwrap', href: '/' }
	] as const;
</script>

<nav
	class="mb-6 grid grid-cols-2 gap-1 rounded-xl bg-zinc-200/70 p-1 dark:bg-zinc-800/60"
	aria-label="Sections"
>
	{#each tabs as tab}
		<a
			href={tab.href}
			aria-current={active === tab.id ? 'page' : undefined}
			class="rounded-lg py-2 text-center text-sm font-semibold transition-colors {active === tab.id
				? 'bg-white text-zinc-900 shadow-sm dark:bg-zinc-700 dark:text-white'
				: 'text-zinc-500 hover:text-zinc-900 dark:hover:text-white'}"
		>
			{tab.label}
		</a>
	{/each}
</nav>
