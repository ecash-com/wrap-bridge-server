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
  from the treasury's associated token account to the user's Solana address. A recipient
  with no token account pays that account's rent in native coin, 1:1, locked when the order
  is created.
- **Peg-out (wECX → ECX):** a user requests an order and sends wECX to the
  treasury with a Solana Pay reference key; the backend matches the deposit
  by that reference (Solana has no per-order address scheme) and sends ECX
  back from the treasury's ECX wallet.

An order waits `ORDER_LIFETIME_MINUTES` (default 15) for its deposit, then
expires and frees its slot. A deposit seen before that never expires. A deposit
that lands after expiry revives the order for `ORDER_EXPIRY_GRACE_HOURS`
(default 168): the poller re-checks recently expired orders every tick, and the
order page re-checks the order the customer is looking at.

**What the customer sees.** Creating an order sends the browser to a page
whose URL contains the order's random id, which is the order's only credential;
there are no accounts. The page polls every 20 seconds while the order can still
change, shows the steps the order goes through with the current one marked,
counts deposit confirmations toward `ECX_DEPOSIT_MIN_CONFS`, states why a deposit
is being refunded (`refund_reason`), links every transaction id to a block
explorer (`ECX_EXPLORER_TX_URL`, derived from the Esplora URL by default, and
`SOLANA_EXPLORER_TX_URL`, Solscan by default; set either empty for bare ids),
and offers the link itself as a copy button and a QR code for moving between
devices. Orders are also listed per browser at `/orders`, from local storage.

**Unmatched deposits.** A wECX transfer that reaches the treasury without any
order's reference key (the customer pasted the treasury address into a wallet
instead of opening the order's Solana Pay code) cannot be matched, and would
otherwise sit in the treasury as float. Every tick the poller walks the treasury
token account's history forward from where it last stopped and records each such
finalized deposit. The rule, in order: a deposit from the earnings owner address
or `TREASURY_FUNDING_ADDRESSES` is the operator's liquidity and is **kept**; one
whose sender cannot be determined, is a program rather than a wallet, or arrived
while `ALLOW_LIVE_PAYOUTS` or `UNMATCHED_DEPOSIT_REFUNDS` was off is **held** for
a person; one whose refund would fall below the minimum is **kept** as the fee;
anything else is **refunded** to the sending wallet minus
`UNMATCHED_DEPOSIT_FEE_BPS` (default 1%), with the same sign, persist, broadcast,
resolve sequence as every other Solana send. Owed refunds are reserved against
the treasury balance and the fees count as unwrap earnings. On its first run the
scan starts at the newest transaction; history before that is not examined.
`unmatched-deposits` lists what is held or owed; refund a held one by hand with
`solana-send`.

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
  The backend only sees its proxy's address, so per-client limits live in front
  of it: the frontend allows eight order requests per minute per client address,
  and a Cloudflare rate limiting rule (see `deploy/edge/cloudflare.md`) refuses
  floods before they reach the tunnel. `ORDERS_PER_MINUTE` (default 120, `0`
  disables) is only a backstop sized to keep a burst within the public Solana
  RPC's allowance; legitimate traffic should never reach it. Sustained creation
  is capped by how many orders may be open at once (`MAX_PENDING_ORDERS`); there
  is no shared daily quota, because exhausting one would refuse every customer.
- `earnings.toml`'s `ecx_owner_address` must not belong to the hot wallet. The
  server refuses to start if it is one of the wallet's revealed addresses or one
  of the first thousand of either keychain; `owns-address --address` runs the
  same check from the CLI. The Solana owner address must not be the treasury.
- ECX deposits are not treated as confirmed until `ECX_DEPOSIT_MIN_CONFS`
  (default 3). Do not set this to 0.
- `.env.example` has empty key fields. Generate new keys. The process also
  refuses the public example phrases and the secrets that were committed in an
  older repository. Anyone who used those values should treat that hot wallet
  as compromised.
- This is custodial software: whoever holds `WALLET_MNEMONIC` and
  `SOLANA_KEYPAIR` controls the funds in both wallets. Treat both as you
  would any hot wallet key.
- Not built yet: a cap on how much of total supply can be unwrapped per
  window (e.g. 10% per week), and splitting payout authority across
  multiple servers/keys (multisig) so no single one can drain the treasury.
  Both were flagged as needed guardrails but aren't enforced by this code.

## Earnings Sweeps

`earnings.toml` configures automatic cold-storage transfers. On an installed
bridge it is generated by `install.sh` at `/etc/ecash-bridge/earnings.toml` from
the `OWNER_ECX_ADDRESS`, `OWNER_SOL_ADDRESS`, and `EARNINGS_THRESHOLD_*` lines of
`params.conf`, and the service unit points `EARNINGS_CONFIG_PATH` at it. Change
`params.conf` and run the installer again rather than editing the generated
file. If that variable is set, a missing or invalid file fails startup. Without
the variable, the backend looks for `earnings.toml` in its working directory; a
missing file leaves new sweeps disabled. For a local development run, copy
`earnings.toml.example` (sweeps off, sample addresses) to `earnings.toml`, which
is gitignored, and fill in addresses you control.

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
