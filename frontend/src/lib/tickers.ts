import { env } from '$env/dynamic/public';

const TICKER = /^[A-Za-z][A-Za-z0-9]{0,15}$/;

// Display name of the one asset this bridge wraps. The wrapped token is always "w" plus this
// name, so bECX is shown as wbECX, ECX as wECX, and aECX as waECX. A missing or malformed value
// falls back to bECX rather than rendering whatever was injected into the environment.
export function assetTicker(): string {
	const value = env.PUBLIC_ASSET_TICKER?.trim() ?? '';
	return TICKER.test(value) ? value : 'bECX';
}

export function wrappedTicker(): string {
	return `w${assetTicker()}`;
}
