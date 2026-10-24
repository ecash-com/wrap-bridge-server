import { env } from '$env/dynamic/private';
import { error } from '@sveltejs/kit';

const BACKEND_URL = env.BACKEND_URL ?? 'http://127.0.0.1:3000';

export class BackendError extends Error {
	constructor(
		public status: number,
		message: string
	) {
		super(message);
	}
}

export async function backendFetch<T>(path: string, init?: RequestInit): Promise<T> {
	const res = await fetch(`${BACKEND_URL}${path}`, init);
	if (!res.ok) {
		// 4xx text is written for the caller (validation, rate limit, liquidity). 5xx text describes
		// backend internals - keep it out of the response.
		if (res.status >= 500) {
			console.error(`backend ${path} -> ${res.status}: ${await res.text().catch(() => '')}`);
			throw new BackendError(res.status, 'the bridge backend hit an error, please retry shortly');
		}
		const message = (await res.text().catch(() => '')) || `backend error ${res.status}`;
		throw new BackendError(res.status, message);
	}
	return res.json() as Promise<T>;
}

/** Parses a JSON request body, answering 400 (not an unhandled 500) for malformed input. */
export async function readJsonBody(request: Request): Promise<Record<string, unknown>> {
	let body: unknown;
	try {
		body = await request.json();
	} catch {
		throw error(400, 'request body must be valid JSON');
	}
	if (typeof body !== 'object' || body === null || Array.isArray(body)) {
		throw error(400, 'request body must be a JSON object');
	}
	return body as Record<string, unknown>;
}
