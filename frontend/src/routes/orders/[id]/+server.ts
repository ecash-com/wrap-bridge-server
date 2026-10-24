import { json, error } from '@sveltejs/kit';
import { backendFetch, BackendError } from '$lib/server/backend';
import type { RequestHandler } from './$types';

interface OrderResponse {
	id: string;
	amount_in_sat: number;
	amount_out_base_units: number;
	deposit_address: string;
	solana_recipient: string;
	refund_address: string | null;
	status: string;
	deposit_txid: string | null;
	payout_signature: string | null;
	refund_txid: string | null;
	created_at: number;
	updated_at: number;
}

export const GET: RequestHandler = async ({ params }) => {
	try {
		const order = await backendFetch<OrderResponse>(`/orders/${encodeURIComponent(params.id)}`);

		return json({
			id: order.id,
			amountInSat: order.amount_in_sat,
			amountOutBaseUnits: order.amount_out_base_units,
			depositAddress: order.deposit_address,
			solanaRecipient: order.solana_recipient,
			refundAddress: order.refund_address,
			status: order.status,
			depositTxid: order.deposit_txid,
			payoutSignature: order.payout_signature,
			refundTxid: order.refund_txid
		});
	} catch (e) {
		if (e instanceof BackendError) throw error(e.status, e.message);
		throw error(502, 'bridge backend is unreachable');
	}
};
