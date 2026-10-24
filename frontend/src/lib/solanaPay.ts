// Builds a Solana Pay transfer-request URI (https://docs.solanapay.com/spec) for a peg-out
// (wECX->ECX) deposit. `recipient` is the treasury's wallet address, not its associated token
// account - a Solana Pay-aware wallet resolves the right ATA itself from `spl-token`. `reference`
// is the order's Solana Pay reference key (see backend/src/solana.rs's
// SolanaWallet::find_reference_deposit) - the only way this bridge can tell which order a deposit
// belongs to, since every peg-out deposit lands in the same treasury ATA. A wallet that doesn't
// understand Solana Pay (i.e. one a customer pastes the recipient address into manually rather
// than scanning/opening this URI) won't include the reference key, and the deposit will never be
// matched to this order - see the warning rendered on the order page.
export function buildSolanaPayUri(params: {
	recipient: string;
	mint: string;
	reference: string;
	amountBaseUnits: number;
	decimals: number;
	label?: string;
	message?: string;
}): string {
	const amount = (params.amountBaseUnits / 10 ** params.decimals).toFixed(params.decimals);
	const qs = new URLSearchParams();
	qs.set('amount', amount);
	qs.set('spl-token', params.mint);
	qs.set('reference', params.reference);
	if (params.label) qs.set('label', params.label);
	if (params.message) qs.set('message', params.message);
	return `solana:${params.recipient}?${qs.toString()}`;
}
