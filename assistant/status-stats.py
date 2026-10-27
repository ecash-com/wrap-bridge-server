#!/usr/bin/env python3
"""Render the treasury and order lines of `install.sh status` from the backend's /stats JSON."""
import json
import sys
from collections import Counter

s = json.loads(sys.argv[1])
d = int(s.get("wecx_decimals", 8))


def coins(v, dec=8):
    return f"{int(v) / 10**dec:,.8f}".rstrip("0").rstrip(".") or "0"


print(f"  ECX confirmed      {coins(s['ecx_confirmed_sat'])}  (unconfirmed {coins(s['ecx_unconfirmed_sat'])})")
print(f"  wrapped token      {coins(s['wecx_balance_base_units'], d)}")
print(f"  SOL                {int(s['sol_balance_lamports']) / 1e9:.4f}")
print(f"  completed          {s['wraps']} wraps, {s['unwraps']} unwraps")
print(f"  fees collected     {coins(s['wrap_fees_ecx_sat'])} ECX, {coins(s['unwrap_fees_wecx_base_units'], d)} wrapped")
print(
    f"  unmatched deposits {s.get('unmatched_deposits_held', 0)} held, "
    f"{s.get('unmatched_refunds_pending', 0)} owed back, {s.get('unmatched_refunds_completed', 0)} refunded"
)
c = Counter((o["direction"], o["status"]) for o in s.get("recent_orders", []))
if c:
    print("  recent orders      " + ", ".join(f"{n} {direction} {status}" for (direction, status), n in sorted(c.items())))
