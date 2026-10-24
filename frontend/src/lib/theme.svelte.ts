const STORAGE_KEY = 'theme';

function readInitialTheme(): 'light' | 'dark' {
	if (typeof document === 'undefined') return 'dark';
	return document.documentElement.classList.contains('dark') ? 'dark' : 'light';
}

let theme = $state<'light' | 'dark'>(readInitialTheme());

export function currentTheme(): 'light' | 'dark' {
	return theme;
}

export function toggleTheme() {
	theme = theme === 'dark' ? 'light' : 'dark';
	document.documentElement.classList.toggle('dark', theme === 'dark');
	try {
		localStorage.setItem(STORAGE_KEY, theme);
	} catch {
		// localStorage unavailable (e.g. private browsing) - theme just won't persist
	}
}
