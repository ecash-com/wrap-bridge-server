import { json, error } from '@sveltejs/kit';
import { backendFetch, BackendError, readJsonBody } from '$lib/server/backend';
import type { RequestHandler } from './$types';

interface PegoutOrder {
	id: string;
	amount_in_base_units: number;
	amount_out_sat: number;
	ecx_recipient: string;
	reference_pubkey: string;
}

export const POST: RequestHandler = async ({ request }) => {
	const body = await readJsonBody(request);
	const { amountInBaseUnits, ecxRecipient } = body;

	if (
		typeof amountInBaseUnits !== 'number' ||
		!Number.isInteger(amountInBaseUnits) ||
		amountInBaseUnits <= 0
	) {
		throw error(400, 'amountInBaseUnits must be a positive integer (wbECX base units)');
	}
	if (typeof ecxRecipient !== 'string' || ecxRecipient.trim() === '') {
		throw error(400, 'ecxRecipient is required');
	}

	try {
		const order = await backendFetch<PegoutOrder>('/pegout/orders', {
			method: 'POST',
			headers: { 'Content-Type': 'application/json' },
			body: JSON.stringify({
				amount_in_base_units: amountInBaseUnits,
				ecx_recipient: ecxRecipient
			})
		});

		return json({
			orderId: order.id,
			amountInBaseUnits: order.amount_in_base_units,
			amountOutSat: order.amount_out_sat,
			ecxRecipient: order.ecx_recipient,
			referencePubkey: order.reference_pubkey
		});
	} catch (e) {
		if (e instanceof BackendError) throw error(e.status, e.message);
		throw error(502, 'bridge backend is unreachable');
	}
};
