import { json, error } from '@sveltejs/kit';
import { backendFetch, BackendError } from '$lib/server/backend';
import type { RequestHandler } from './$types';

const UNIT_SCALE = 1e8;

interface QuoteResponse {
	amount_in_sat: number | null;
	amount_out_base_units: number | null;
	pegin_fee_bps: number;
	pegin_flat_fee_sat: number;
	pegout_fee_bps: number;
	wecx_decimals: number;
	min_amount_in_sat: number;
	sol_stipend_lamports: number;
	sol_stipend_price_sat: number;
	sol_stipend_usd_label: string;
}

// Omit ?amount to get just the bridge's static parameters (fee, decimals, minimum) - no ECX amount
// is required. Unlike the old AMM swap's quote, this involves no live pricing on the backend
// either way - it's a flat 1:1-minus-fee conversion, so there's nothing that can go stale between
// this response and order creation.
export const GET: RequestHandler = async ({ url }) => {
	const amountParam = url.searchParams.get('amount');

	const qs = new URLSearchParams();
	if (amountParam) {
		const amountDecimal = Number(amountParam);
		if (!Number.isFinite(amountDecimal) || amountDecimal <= 0) {
			throw error(400, 'amount must be a positive number');
		}
		qs.set('amount_in_sat', String(Math.round(amountDecimal * UNIT_SCALE)));
	}

	try {
		const data = await backendFetch<QuoteResponse>(`/quote?${qs}`);
		return json({
			amountIn: data.amount_in_sat != null ? data.amount_in_sat / UNIT_SCALE : null,
			amountOutBaseUnits: data.amount_out_base_units,
			peginFeeFraction: data.pegin_fee_bps / 10_000,
			peginFlatFeeSat: data.pegin_flat_fee_sat,
			// Peg-out itself isn't built in the frontend yet - carried through for when it is,
			// not rendered anywhere right now.
			pegoutFeeFraction: data.pegout_fee_bps / 10_000,
			wecxDecimals: data.wecx_decimals,
			minAmountIn: data.min_amount_in_sat / UNIT_SCALE,
			solStipendLamports: data.sol_stipend_lamports,
			solStipendPriceSat: data.sol_stipend_price_sat,
			solStipendUsdLabel: data.sol_stipend_usd_label
		});
	} catch (e) {
		if (e instanceof BackendError) throw error(e.status, e.message);
		throw error(502, 'bridge backend is unreachable');
	}
};
