import { json, error } from '@sveltejs/kit';
import { backendFetch, BackendError, readJsonBody } from '$lib/server/backend';
import type { RequestHandler } from './$types';

export const POST: RequestHandler = async ({ params, request }) => {
	const body = await readJsonBody(request);
	const { refundAddress } = body;

	if (typeof refundAddress !== 'string' || refundAddress.trim() === '') {
		throw error(400, 'refundAddress is required');
	}

	try {
		await backendFetch(`/orders/${encodeURIComponent(params.id)}/refund-address`, {
			method: 'POST',
			headers: { 'Content-Type': 'application/json' },
			body: JSON.stringify({ refund_address: refundAddress })
		});
		return json({ ok: true });
	} catch (e) {
		if (e instanceof BackendError) throw error(e.status, e.message);
		throw error(502, 'bridge backend is unreachable');
	}
};
