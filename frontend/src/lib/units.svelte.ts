const STORAGE_KEY = 'displayUnit';

export type Unit = 'main' | 'smallest';

const MAIN_TICKER = 'bECX';
const SMALLEST_TICKER = 'szats';

function readInitialUnit(): Unit {
	try {
		return localStorage.getItem(STORAGE_KEY) === 'smallest' ? 'smallest' : 'main';
	} catch {
		return 'main';
	}
}

let unit = $state<Unit>(readInitialUnit());

export function currentUnit(): Unit {
	return unit;
}

export function toggleUnit() {
	unit = unit === 'smallest' ? 'main' : 'smallest';
	try {
		localStorage.setItem(STORAGE_KEY, unit);
	} catch {
		// localStorage unavailable (e.g. private browsing) - preference just won't persist
	}
}

// ECX ticker for the current unit preference - the only asset this toggle applies to (the bridge's
// deposit side). wECX is always shown in whole-token terms via formatBaseUnits below, since its
// decimals vary by mint and "smallest unit" isn't a meaningful toggle for it the way sats/szats is.
export function ecxTicker(): string {
	return unit === 'smallest' ? SMALLEST_TICKER : MAIN_TICKER;
}

// Asset name for the token pill: same as the ticker, but spells out that szats are just a
// different view of bECX rather than a separate asset.
export function ecxAssetLabel(): string {
	return unit === 'smallest' ? `${MAIN_TICKER} (in ${SMALLEST_TICKER})` : MAIN_TICKER;
}

// Formats an ECX amount given in main units (ECX) for display, honoring the current unit
// preference - converts to whole szats when the smallest-unit view is active. Accepts a number or
// a numeric string (e.g. straight from a bound input) and returns '' for empty/invalid input so
// callers can drop it straight into an input's `value` without an extra guard. Deliberately
// comma-free (unlike presentSatAmount) since this feeds a `type="number"` input, which won't
// render a comma-formatted value.
export function presentAmount(mainUnitValue: number | string): string {
	const n = typeof mainUnitValue === 'string' ? parseFloat(mainUnitValue) : mainUnitValue;
	if (!Number.isFinite(n)) return '';
	if (n === 0) return '';
	if (unit === 'smallest') {
		return Math.round(n * 1e8).toString();
	}
	return n % 1 === 0 ? n.toString() : n.toFixed(8).replace(/0+$/, '');
}

// Inverse of presentAmount for editable fields: takes whatever the user typed in the currently
// active unit and converts it back to a main-unit numeric string, which is what the rest of the
// app's bridge math (amountInEcx state) is expressed in.
export function parseDisplayAmount(displayValue: string): string {
	if (unit !== 'smallest') return displayValue;
	const cleaned = displayValue.replace(/,/g, '');
	const n = parseFloat(cleaned);
	if (!Number.isFinite(n)) return displayValue;
	return (n / 1e8).toString();
}

// Formats an ECX amount already given in satoshis (e.g. straight from the backend/order history)
// for display, honoring the current unit preference.
export function presentSatAmount(sat: number): string {
	if (unit === 'smallest') {
		return Math.round(sat).toLocaleString('en-US');
	}
	const n = sat / 1e8;
	if (n === 0) return '0';
	return n % 1 === 0 ? n.toString() : n.toFixed(8).replace(/0+$/, '');
}

// Formats a wECX amount given in base units (as the backend stores/returns it) into whole-token
// terms, using `decimals` fetched live from the mint (see /bridge/quote) rather than an assumed
// constant - wECX's decimals aren't guaranteed to match ECX's 8.
export function formatBaseUnits(baseUnits: number, decimals: number): string {
	const n = baseUnits / 10 ** decimals;
	if (n === 0) return '0';
	return n % 1 === 0 ? n.toString() : n.toFixed(decimals).replace(/0+$/, '').replace(/\.$/, '');
}
