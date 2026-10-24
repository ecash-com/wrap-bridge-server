import { json, error } from '@sveltejs/kit';
import { backendFetch, BackendError } from '$lib/server/backend';
import type { RequestHandler } from './$types';

interface PegoutOrderResponse {
	id: string;
	amount_in_base_units: number;
	amount_out_sat: number;
	ecx_recipient: string;
	reference_pubkey: string;
	status: string;
	deposit_signature: string | null;
	depositor_owner: string | null;
	payout_txid: string | null;
	refund_signature: string | null;
	created_at: number;
	updated_at: number;
}

export const GET: RequestHandler = async ({ params }) => {
	try {
		const order = await backendFetch<PegoutOrderResponse>(
			`/pegout/orders/${encodeURIComponent(params.id)}`
		);

		return json({
			id: order.id,
			amountInBaseUnits: order.amount_in_base_units,
			amountOutSat: order.amount_out_sat,
			ecxRecipient: order.ecx_recipient,
			referencePubkey: order.reference_pubkey,
			status: order.status,
			depositSignature: order.deposit_signature,
			depositorOwner: order.depositor_owner,
			payoutTxid: order.payout_txid,
			refundSignature: order.refund_signature
		});
	} catch (e) {
		if (e instanceof BackendError) throw error(e.status, e.message);
		throw error(502, 'bridge backend is unreachable');
	}
};
