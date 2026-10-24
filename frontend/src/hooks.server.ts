import { timingSafeEqual } from 'node:crypto';
import type { Handle } from '@sveltejs/kit';

// Baseline response hardening. The theme bootstrap is a same-origin script (`/theme-init.js`), so
// `script-src` can stay `'self'`. This also blocks framing (clickjacking a deposit address), MIME
// sniffing, and referrer leakage (order ids in URLs are effectively credentials).
// Add `Strict-Transport-Security` at the TLS-terminating proxy, where the scheme is known.
//
// Per-IP admission only works because Caddy is the only process that should reach this app
// (HOST=127.0.0.1). The last X-Forwarded-For hop is the address Caddy saw. Do not trust this
// header if the Node port is ever exposed directly.
const orderPosts = new Map<string, number[]>();

function edgeToken(): string {
	return process.env.EDGE_SHARED_TOKEN ?? '';
}

// The token is checked before any customer header is trusted. A request that did not come through
// the relay cannot spoof CF-Connecting-IP into the per-IP limiter.
function edgeAuthorized(request: Request): boolean {
	const expected = edgeToken();
	if (!expected) return true;
	const presented = Buffer.from(request.headers.get('x-edge-token') ?? '');
	const required = Buffer.from(expected);
	return presented.length === required.length && timingSafeEqual(presented, required);
}

function clientIp(request: Request): string {
	if (edgeToken()) {
		const cloudflareIp = request.headers.get('cf-connecting-ip')?.trim() ?? '';
		return cloudflareIp.length > 0 && cloudflareIp.length <= 64 ? cloudflareIp : 'unknown';
	}
	const forwarded = request.headers.get('x-forwarded-for');
	if (!forwarded) return 'unknown';
	const hops = forwarded
		.split(',')
		.map((part) => part.trim())
		.filter(Boolean);
	return hops.at(-1) ?? 'unknown';
}

function allowOrderPost(ip: string): boolean {
	const now = Date.now();
	const recent = (orderPosts.get(ip) ?? []).filter((at) => now - at < 60_000);
	if (recent.length >= 8) {
		orderPosts.set(ip, recent);
		return false;
	}
	recent.push(now);
	orderPosts.set(ip, recent);
	if (orderPosts.size > 10_000) {
		for (const [key, times] of orderPosts) {
			if (times.every((at) => now - at >= 60_000)) orderPosts.delete(key);
		}
	}
	return true;
}

export const handle: Handle = async ({ event, resolve }) => {
	if (!edgeAuthorized(event.request)) {
		return new Response('not found', { status: 404 });
	}
	if (event.request.method === 'POST' && /\/(bridge\/orders|pegout\/orders|refund-address)$/.test(event.url.pathname)) {
		if (!allowOrderPost(clientIp(event.request))) {
			return new Response('too many orders from this address - please retry shortly', { status: 429 });
		}
	}
	const response = await resolve(event);
	response.headers.set(
		'Content-Security-Policy',
		"default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'self'; object-src 'none'; form-action 'self'"
	);
	response.headers.set('X-Frame-Options', 'DENY');
	response.headers.set('X-Content-Type-Options', 'nosniff');
	response.headers.set('Referrer-Policy', 'no-referrer');
	response.headers.set('Permissions-Policy', 'camera=(), microphone=(), geolocation=()');
	return response;
};
