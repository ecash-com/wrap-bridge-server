import { json, error } from '@sveltejs/kit';
import { backendFetch, BackendError, readJsonBody } from '$lib/server/backend';
import { assetTicker } from '$lib/tickers';
import type { RequestHandler } from './$types';

interface Order {
	id: string;
	deposit_address: string;
	amount_in_sat: number;
	amount_out_base_units: number;
}

export const POST: RequestHandler = async ({ request }) => {
	const body = await readJsonBody(request);
	const { amountInSat, solanaRecipient, includeSolStipend } = body;

	if (typeof amountInSat !== 'number' || !Number.isInteger(amountInSat) || amountInSat <= 0) {
		throw error(400, `amountInSat must be a positive integer (${assetTicker()} satoshis)`);
	}
	if (typeof solanaRecipient !== 'string' || solanaRecipient.trim() === '') {
		throw error(400, 'solanaRecipient is required');
	}
	if (includeSolStipend != null && typeof includeSolStipend !== 'boolean') {
		throw error(400, 'includeSolStipend must be a boolean');
	}

	try {
		const order = await backendFetch<Order>('/orders', {
			method: 'POST',
			headers: { 'Content-Type': 'application/json' },
			body: JSON.stringify({
				amount_in_sat: amountInSat,
				solana_recipient: solanaRecipient,
				include_sol_stipend: includeSolStipend === true
			})
		});

		return json({
			orderId: order.id,
			depositAddress: order.deposit_address,
			amountInSat: order.amount_in_sat,
			amountOutBaseUnits: order.amount_out_base_units
		});
	} catch (e) {
		if (e instanceof BackendError) throw error(e.status, e.message);
		throw error(502, 'bridge backend is unreachable');
	}
};
