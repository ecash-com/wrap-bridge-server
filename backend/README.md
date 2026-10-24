# swap-backend

Custodial bridge service between ECX (an eCash-derived chain, currently on
betanet) and wECX, an SPL token on Solana. Rust/axum HTTP API plus a CLI for
manual wallet operations.

Both ECX betanet and Solana mainnet-beta hold real value. There is no test
network for either side of this bridge — every automatic payout and every
manual CLI send requires an explicit opt-in (see **Safety** below).

## How it works

- **Peg-in (ECX → wECX):** a user requests an order, the backend hands back a
  fresh ECX deposit address, a background poller waits for the deposit to
  confirm, then sends wECX for that deposit minus 0.00001000 ECX and a 1% fee
  from the treasury's associated token account to the user's Solana address.
- **Peg-out (wECX → ECX):** a user requests an order and sends wECX to the
  treasury with a Solana Pay reference key; the backend matches the deposit
  by that reference (Solana has no per-order address scheme) and sends ECX
  back from the treasury's ECX wallet.

The ECX wallet is a [BDK](https://bitcoindevkit.org/) wallet derived from a
BIP39 mnemonic. The Solana treasury is a normal keypair holding an
already-minted SPL token — this service has no mint or burn authority over
wECX, only control of its own treasury account. Order state is persisted in
a local SQLite database file.

## Setup

1. Copy `.env.example` to `.env` and fill it in. Every variable is
   documented inline; at minimum you need:
   - `DATABASE_PATH` (optional, defaults to `data/bridge.db`)
   - `WALLET_MNEMONIC` — a fresh BIP39 mnemonic dedicated to this service.
     Do not reuse a mnemonic that controls funds elsewhere.
   - `SOLANA_KEYPAIR` — base58-encoded 64-byte secret key. Generate one with
     `cargo run -- solana-keygen` (prints the key and address, stores
     nothing).
   - `WECX_MINT_ADDRESS`
2. `cargo build`
3. `cargo run -- serve` to start the HTTP API (default `127.0.0.1:3000`).

Run `cargo run -- --help` for the full CLI, which also covers wallet
inspection (`address`, `balance`, `utxos`, `sync`), manual sends
(`send`, `solana-send`), and quoting (`quote`, `pegout-quote`).

## Safety

- `ALLOW_LIVE_PAYOUTS` gates every automatic outbound send (wECX payouts,
  ECX refunds, ECX payouts, wECX refunds) the background poller makes.
  It defaults off. Only set it to `1` once you've verified the bridge
  end-to-end with small amounts.
- Manual CLI sends (`send`, `solana-send`) each require their own explicit
  `--confirm-live` flag on top of that.
- The abuse-limit and treasury-floor variables documented in `.env.example`
  (`MAX_DEPOSIT_AMOUNT_SAT`, `TREASURY_MIN_SOL_LAMPORTS`,
  `ORDERS_PER_MINUTE`, `MAX_PENDING_ORDERS`, ...) are global backend limits.
  Put per-IP rate limiting at your reverse proxy — the backend only sees
  requests from its proxy, not client IPs. Burst creation is capped per minute.
  Sustained creation is capped by how many orders may be open at once; there is
  no shared daily quota, because exhausting one would refuse every customer.
- ECX deposits are not treated as confirmed until `ECX_DEPOSIT_MIN_CONFS`
  (default 3). Do not set this to 0.
- The example mnemonic and Solana key in `.env.example` are public fixtures.
  The process refuses to open a wallet if it is given those values, or the
  secrets that were previously committed in that file. Generate new keys.
  Anyone who cloned the repo before that replacement should treat the old hot
  wallet as compromised.
- This is custodial software: whoever holds `WALLET_MNEMONIC` and
  `SOLANA_KEYPAIR` controls the funds in both wallets. Treat both as you
  would any hot wallet key.
- Not built yet: a cap on how much of total supply can be unwrapped per
  window (e.g. 10% per week), and splitting payout authority across
  multiple servers/keys (multisig) so no single one can drain the treasury.
  Both were flagged as needed guardrails but aren't enforced by this code.

## Earnings Sweeps

`earnings.toml` configures automatic cold-storage transfers. Set
`EARNINGS_CONFIG_PATH` to an absolute path in production, such as
`/etc/ecash-bridge/earnings.toml`. If that variable is set, a missing or invalid
file fails startup. Without the variable, the backend looks for `earnings.toml`
in its working directory; a missing file leaves new sweeps disabled.

The file contains `enabled`, `earnings_threshold_wbecx`,
`earnings_threshold_becx`, `solana_owner_address`, and `ecx_owner_address`.
Thresholds are decimal strings in whole-coin units, not base units. The Solana
destination is the cold wallet's owner address; the transfer resolves its token
account. The configured mint must have eight decimals for this feature.

On the first poll and each subsequent order-poll interval, all historical fees
from completed orders count toward earnings. Wrap fees accrue in bECX and
unwrap fees in wbECX. When unswept fees strictly exceed the corresponding
threshold, the backend sends all unswept fees, not just the threshold amount.
Network costs come from the treasury in addition to the transfer amount; SOL
itself is never swept. Lifetime fee totals on `/stats` do not decrease.

Sweeps require both `enabled = true` and `ALLOW_LIVE_PAYOUTS=1`. Customer
obligations, including unresolved deposits and refunds, are reserved first.
ECX sweeps only select confirmed, unprotected outputs. If reserves or network
fees cannot be covered, the whole sweep is deferred rather than partially sent.

The SQLite `earnings_sweeps` ledger stores the amount, destination, transaction
identifier, and state before broadcast. Pending and completed transfers are
deducted from future sweepable earnings. ECX retries broadcast the same signed
transaction; an ambiguous Solana transaction remains reserved until a confirmed
success or definitive on-chain failure can be established. A missing Solana
signature after restart is held for manual review, not automatically replaced.
Errors are logged with the `[earnings]` prefix.

Back up the order database together with the sweep ledger. Restoring a database
from before a sweep without reconciling its on-chain transfer can pay the same
historical fees again. Do not delete or manually reset pending ledger entries
without verifying the transaction on-chain. Disabling `enabled` stops new sweeps;
already-recorded transfers still recover while live payouts are allowed.
Changing addresses or thresholds requires a backend restart. Keep this config
root-owned and outside the frontend directory. It contains no private keys.

## Development

- `cargo test`, `cargo clippy`, `cargo fmt`
- `cargo audit` is recommended before any production deployment; it isn't
  run in CI here yet.

## License

MIT — see [LICENSE](LICENSE).
