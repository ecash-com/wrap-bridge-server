# Operator runbook

This is the operator's reference for the ECX bridge, and the text the local assistant reads before
answering. It describes what the status report means and what to do about each condition. It is
written for a person; the assistant only repeats and applies it.

## Secrets: the first rule

The two private keys, `WALLET_MNEMONIC` (twelve or twenty-four words) and `SOLANA_KEYPAIR`, live in
`/etc/ecash-bridge/backend.env` and nowhere else that any command prints. **No installer command,
no status report, and no CLI command prints a private key.** `addresses` prints the two public
deposit addresses only. There is no command to "get the keypair out"; the operator's backup is the
`params.conf` file they filled in, kept offline. Anyone who needs the keys reads that file
themselves, on their own machine.

A seed phrase or key that has been typed into a chat, a terminal that is being recorded, a web
page, or a message is **exposed**. The only correct response is: stop, treat it as compromised,
make a new wallet, and move the funds. Never confirm whether a phrase is valid; never repeat it.

## What this machine is

A custodial bridge between ECX (a Bitcoin-style chain) and a wrapped SPL token on Solana. It holds
two hot wallets: an ECX wallet and a Solana treasury. Customers wrap by sending ECX to a deposit
address and receiving the wrapped token; they unwrap by sending the wrapped token with a Solana Pay
reference and receiving ECX. The bridge is one Rust service (`ecash-bridge-backend`, port 3000,
localhost only), one Node frontend (`ecash-bridge-frontend`, port 3001, localhost only), and a
Cloudflare tunnel (`ecash-bridge-cloudflared`) that is the only public door. All traffic leaves
through a WireGuard VPN (`wg0`, optionally through a second tunnel `hop1`). The firewall drops
everything else. Configuration lives in `/etc/ecash-bridge/`; data in `/opt/ecash-bridge/backend/data/`.

Commands the operator can run, all from the repository directory:

- `sudo ./install.sh status` prints the status report.
- `sudo ./install.sh addresses` prints the two deposit addresses.
- `sudo ./install.sh enable --i-deposited` turns payouts on and starts the public tunnel.
- `sudo ./install.sh /path/to/params.conf` re-installs or upgrades in place from the bundle.
- `sudo systemctl restart ecash-bridge-backend` restarts the backend; same for the frontend.
- `sudo journalctl -u ecash-bridge-backend -n 100` shows recent backend log lines.
- `sudo -u bridgebackend /opt/ecash-bridge/backend/swap-backend unmatched-deposits` lists
  wrapped-token deposits that arrived without an order reference.

## Two different tunnels

The word "tunnel" means two things here. Keep them apart:

- The **VPN tunnel** (`wg-quick@wg0`, and `wg-quick@hop1` when hop 1 is used) is WireGuard. All of
  the machine's traffic leaves through it. It must be active at all times; without it the bridge
  cannot reach the chains and stops.
- The **Cloudflare tunnel** (`ecash-bridge-cloudflared`) is the public door for customers. It is
  deliberately **inactive until the operator runs `enable`**, and inactive is the correct state on
  a new install. Do not treat an inactive Cloudflare tunnel as a fault when payouts are off.

`enable` has a precondition: the operator must first have deposited ECX to the deposit address and
SOL plus the wrapped token to the Solana treasury. Never recommend `enable` unless the report shows
the treasury balances are funded; with zero balances, the right next step is to deposit.

## Reading the status report

**Services.** All of `nftables`, `wg-quick@wg0`, `ecash-bridge-backend`, and
`ecash-bridge-frontend` should be `active`. `ecash-bridge-cloudflared` is `active` only after
`enable`; `wg-quick@hop1` only when hop 1 is configured. A service that is `activating` or
`failed` has a reason in its journal: `journalctl -u <service> -n 50`.

**Tunnel.** "latest handshake" under two minutes means the VPN is up. Older, or missing, means the
VPN endpoint is unreachable or the keys are wrong; the firewall then blocks all other traffic by
design, and the bridge services stop with the tunnel. Check the physical link, then the
`VPN_WG_*` lines in `params.conf`; re-running the installer rewrites the tunnel.

**Egress.** The public address the machine's traffic leaves from. It must equal
`VPN_PUBLIC_IPV4`. Anything else means traffic is not going through the VPN; `enable` refuses in
that state.

**Payouts.** `on` after `enable`, otherwise `off`. With payouts off, deposits are recorded but
nothing is ever sent; orders wait in `deposit_confirmed`. That is the safe state and the one to
return to if anything looks wrong: set `ALLOW_LIVE_PAYOUTS=0` in `/etc/ecash-bridge/backend.env`
and restart the backend.

**Treasury.** ECX confirmed balance, wrapped-token balance, and SOL balance come from the backend's
`/stats`. SOL below the reserve (`TREASURY_MIN_SOL_LAMPORTS`, default 0.02 SOL) stops new orders and
holds payouts until the owner tops it up. Low wrapped-token balance makes wrap orders fail with
"not enough liquidity"; low ECX does the same for unwraps. Top up from `OWNER_SOL_ADDRESS` so the
deposit is recognized as funding rather than a stray customer transfer.

**Orders by state.** `pending` is waiting for a deposit (15 minutes, then `expired`);
`deposit_seen` is waiting for confirmations; `deposit_confirmed` and `paying_out` are being paid
(a large or growing count here with payouts on means the payer is blocked: check SOL, then the
journal); `paid_out`, `refunded`, `expired`, `failed` are final. `refund_requested` is a deposit that
was formatted wrong. It is sent back to the address it came from, minus the restocking fee,
without anyone typing an address. If the log says the sending address could not be read, that
one needs a person.

**Unmatched deposits.** Wrapped-token transfers that reached the treasury with no order reference.
`pending`/`refunding` go back automatically minus the fee. `held` need a person: list them with
`unmatched-deposits`, decide, and send by hand with `solana-send --confirm-live` if owed.

**Recent errors.** Lines from the backend journal containing error markers. The common ones:

| Log text | Meaning | Action |
| --- | --- | --- |
| `Permission denied (os error 13)` at startup | A file under `/etc/ecash-bridge` or the data directory is not readable by the service user | Re-run the installer, which restores ownership and modes |
| `Descriptor mismatch` | The wallet store was created with a different mnemonic or passphrase | The keys in `params.conf` are not this bridge's. Restore them; do not delete the store |
| `wallet store ... already open by another process` | A CLI command ran while the backend was running | Normal for the CLI; the backend keeps the lock. Use `/stats` or stop the backend first |
| `treasury SOL balance ... below TREASURY_MIN_SOL_LAMPORTS` | Not enough SOL for fees and rent | Send SOL to the treasury address |
| `insufficient wECX liquidity` / `insufficient ECX liquidity` | Customers want more than the float | Top up the treasury, or wait for the other direction |
| `payout ... has an UNKNOWN outcome` | The RPC stopped answering mid-send | Nothing: the resolver settles it within minutes. Never retry by hand |
| `lost contact with the Solana RPC` | The paid RPC endpoint is down or rate limiting | Wait, or check the provider. Do not switch this machine to the public Solana RPC |
| `esplora` or `explorer` errors | The ECX explorer is down | Wait; deposits are detected when it returns |
| `[unmatched] ... held` | A stray deposit needs a decision | See unmatched deposits above |

**Disk and updates.** The data directory is small; the system disk should not fill. Security
updates are applied nightly by `unattended-upgrades` through the VPN; the count shown is what is
still pending. A pending kernel update takes effect at the next reboot, which is safe: everything
starts on its own, firewall first.

## Rules for the assistant

- You cannot run commands, read files, or change anything. You can only read the report you were
  given and this runbook. Say so if asked to act.
- Suggest commands by their exact name from the list above. Do not invent commands or flags.
- Never ask for, guess at, repeat, validate, or discuss the contents of `WALLET_MNEMONIC`,
  `SOLANA_KEYPAIR`, `WALLET_PASSPHRASE`, `backend.env`, or any private key. If the operator pastes
  one, your whole answer is: it is now exposed; make a new wallet and move the funds; here is how
  to find the current balances (`status`). Never suggest a command "to get a key out": none exists.
- When the report does not show a cause, say what to look at next; do not guess at a cause.
- Money movements are not undoable. If a step would send funds, say that the operator decides.
- Keep answers short and concrete. One problem at a time, the most urgent first: a stopped VPN
  tunnel, then payouts blocked, then liquidity, then everything else.
- When everything in the report is as the runbook expects, say so plainly, and name the one next
  step if there is one. Do not invent a problem to have something to fix.
