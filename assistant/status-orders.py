#!/usr/bin/env python3
"""Render the open-orders line of `install.sh status` straight from the database, read-only, so it
works even while the backend is down."""
import sqlite3
import sys

FINAL = ("paid_out", "refunded", "expired", "failed")

conn = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True, timeout=5)
rows = conn.execute(
    "SELECT 'wrap', status, COUNT(*) FROM bridge_orders GROUP BY status "
    "UNION ALL SELECT 'unwrap', status, COUNT(*) FROM pegout_orders GROUP BY status"
).fetchall()
open_orders = [(d, s, n) for d, s, n in rows if s not in FINAL]
if open_orders:
    print("  open orders        " + ", ".join(f"{n} {d} {s}" for d, s, n in open_orders))
else:
    print("  open orders        none")
