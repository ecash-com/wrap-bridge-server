import { error } from '@sveltejs/kit';
import { BackendError, backendFetch } from '$lib/server/backend';
import type { PageServerLoad } from './$types';

interface StatsResponse {
	ecx_confirmed_sat: string;
	ecx_unconfirmed_sat: string;
	wecx_balance_base_units: string;
	sol_balance_lamports: string;
	ecx_treasury_address: string;
	solana_treasury_address: string;
	wecx_decimals: number;
	wraps: number;
	unwraps: number;
	wrap_fees_ecx_sat: string;
	unwrap_fees_wecx_base_units: string;
	fees_paid_ecx_sat: string;
	fees_paid_wecx_base_units: string;
	recent_orders: RecentOrder[];
	updated_at_unix: number;
}

interface RecentOrder {
	direction: 'wrap' | 'unwrap';
	amount_in: string;
	amount_out: string;
	status: string;
	created_at_unix: number;
}

export const load: PageServerLoad = async () => {
	try {
		return { stats: await backendFetch<StatsResponse>('/stats') };
	} catch (e) {
		if (e instanceof BackendError) throw error(e.status, e.message);
		throw error(502, 'bridge backend is unreachable');
	}
};
