import { timingSafeEqual } from 'node:crypto';
import type { Handle } from '@sveltejs/kit';

// Baseline response hardening: framing (clickjacking a deposit address), MIME sniffing, and
// referrer leakage (order ids in URLs are effectively credentials). The Content-Security-Policy
// itself is configured in svelte.config.js so SvelteKit can nonce its inline hydration script.
// Add `Strict-Transport-Security` at the TLS-terminating proxy, where the scheme is known.
//
// Per-IP admission for order creation. Which header names the client depends on what sits in
// front of this app, and both layouts rely on Node listening on 127.0.0.1 only:
//
// - Cloudflare tunnel (the installed layout): cloudflared is the only process that can reach this
//   port. Cloudflare appends the address that connected to it to the end of X-Forwarded-For on
//   every request, after anything the client sent, so the last hop is the client as Cloudflare saw
//   it and cannot be displaced from the client side.
// - Caddy relay (optional, EDGE_SHARED_TOKEN set): the token proves the request crossed the relay
//   before CF-Connecting-IP is trusted, so a request that did not come through it cannot spoof an
//   address into the limiter.
//
// Neither header is trustworthy if the Node port is ever exposed directly. This limiter is the
// second line of defense; the first is the Cloudflare rate limiting rule described in
// deploy/edge/cloudflare.md, which refuses floods before they enter the tunnel. The backend's own
// ORDERS_PER_MINUTE is a global backstop behind both.
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
	// Content-Security-Policy is set by SvelteKit from svelte.config.js, with a per-response nonce
	// for its own hydration script. Setting a fixed one here would override it and block that script.
	response.headers.set('X-Frame-Options', 'DENY');
	response.headers.set('X-Content-Type-Options', 'nosniff');
	response.headers.set('Referrer-Policy', 'no-referrer');
	response.headers.set('Permissions-Policy', 'camera=(), microphone=(), geolocation=()');
	return response;
};
