// Local, per-browser record of bridge orders the user has created — orders are only reachable by
// their bookmarked URL (see /orders/[id]), so this is what lets a "your orders" view exist at all.
const STORAGE_KEY = 'bridge:orders';

export interface StoredOrder {
	id: string;
	amountInSat: number;
	amountOutBaseUnits: number;
	createdAt: number;
}

export function getStoredOrders(): StoredOrder[] {
	if (typeof localStorage === 'undefined') return [];
	try {
		const raw = localStorage.getItem(STORAGE_KEY);
		if (!raw) return [];
		const parsed = JSON.parse(raw);
		return Array.isArray(parsed) ? parsed : [];
	} catch {
		return [];
	}
}

export function saveStoredOrder(order: Omit<StoredOrder, 'createdAt'>): void {
	if (typeof localStorage === 'undefined') return;
	try {
		const existing = getStoredOrders().filter((o) => o.id !== order.id);
		existing.unshift({ ...order, createdAt: Date.now() });
		localStorage.setItem(STORAGE_KEY, JSON.stringify(existing));
	} catch {
		// Storage may be full or unavailable (private browsing); losing order history isn't fatal.
	}
}

// Peg-out (wECX->ECX) mirror of the above, kept as a separate key/list since its orders carry
// different fields (wECX base units in, ECX sat out - the reverse of a peg-in order) and are
// looked up against a different backend endpoint (/pegout/orders/[id]).
const PEGOUT_STORAGE_KEY = 'bridge:pegout-orders';

export interface StoredPegoutOrder {
	id: string;
	amountInBaseUnits: number;
	amountOutSat: number;
	createdAt: number;
}

export function getStoredPegoutOrders(): StoredPegoutOrder[] {
	if (typeof localStorage === 'undefined') return [];
	try {
		const raw = localStorage.getItem(PEGOUT_STORAGE_KEY);
		if (!raw) return [];
		const parsed = JSON.parse(raw);
		return Array.isArray(parsed) ? parsed : [];
	} catch {
		return [];
	}
}

export function saveStoredPegoutOrder(order: Omit<StoredPegoutOrder, 'createdAt'>): void {
	if (typeof localStorage === 'undefined') return;
	try {
		const existing = getStoredPegoutOrders().filter((o) => o.id !== order.id);
		existing.unshift({ ...order, createdAt: Date.now() });
		localStorage.setItem(PEGOUT_STORAGE_KEY, JSON.stringify(existing));
	} catch {
		// Storage may be full or unavailable (private browsing); losing order history isn't fatal.
	}
}
