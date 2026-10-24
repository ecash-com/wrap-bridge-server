import { json, error } from '@sveltejs/kit';
import { backendFetch, BackendError } from '$lib/server/backend';
import type { RequestHandler } from './$types';

interface PegoutQuoteResponse {
	amount_in_base_units: number | null;
	amount_out_sat: number | null;
	pegout_fee_bps: number;
	wecx_decimals: number;
	min_amount_out_sat: number;
	treasury_pubkey: string;
	wecx_mint: string;
}

// Unlike /bridge/quote, this takes the wECX amount already in base units (?amountBaseUnits=) -
// wECX's decimals vary by mint (returned by this same endpoint), so scaling from a human amount
// has to happen client-side once the decimals are known, rather than assumed here the way the
// peg-in side hardcodes ECX's fixed 8.
export const GET: RequestHandler = async ({ url }) => {
	const amountParam = url.searchParams.get('amountBaseUnits');

	const qs = new URLSearchParams();
	if (amountParam) {
		if (!/^\d+$/.test(amountParam)) {
			throw error(400, 'amountBaseUnits must be a non-negative integer');
		}
		qs.set('amount_in_base_units', amountParam);
	}

	try {
		const data = await backendFetch<PegoutQuoteResponse>(`/pegout/quote?${qs}`);
		return json({
			amountInBaseUnits: data.amount_in_base_units,
			amountOutSat: data.amount_out_sat,
			pegoutFeeFraction: data.pegout_fee_bps / 10_000,
			wecxDecimals: data.wecx_decimals,
			minAmountOutSat: data.min_amount_out_sat,
			treasuryPubkey: data.treasury_pubkey,
			wecxMint: data.wecx_mint
		});
	} catch (e) {
		if (e instanceof BackendError) throw error(e.status, e.message);
		throw error(502, 'bridge backend is unreachable');
	}
};
