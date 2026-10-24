(function () {
	try {
		var stored = localStorage.getItem('theme');
		var isDark = stored ? stored === 'dark' : window.matchMedia('(prefers-color-scheme: dark)').matches;
		document.documentElement.classList.toggle('dark', isDark);
	} catch (e) {}
})();
