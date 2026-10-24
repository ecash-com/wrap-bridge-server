# ECX bridge

Custodial bridge between ECX and wECX. This repository is only the bridge. It does not create the wECX mint and it does not create the AMM pool. The published mint is `EVHqNdzjCupKi4rQkbuYw52sa1m8A7jeUAMP23S9AVVq`. Put a different mint in `params.conf` if you run one.

Install on a new Ubuntu Server only after the tokens and wallets exist. The order is [docs/prepare.md](docs/prepare.md), then [params.conf.example](params.conf.example), then [docs/install.md](docs/install.md).

```sh
cp params.conf.example "$HOME/bridge-params.conf"
./install.sh check "$HOME/bridge-params.conf"
sudo ./install.sh "$HOME/bridge-params.conf"
```

Payouts stay off until you deposit both treasuries and run `sudo ./install.sh enable --i-deposited`.

The service takes ECX and pays wECX, or takes wECX and pays ECX. It holds both treasuries. It does not have mint authority.

## Parts

- **[backend](backend/)** — the Rust/axum service: HTTP API plus CLI, holds both the ECX (BDK)
  wallet and the Solana treasury, and runs the poller that matches deposits to orders and sends
  the corresponding payout. See its README for how peg-in and peg-out each work.
- **[frontend](frontend/)** — the SvelteKit UI, talks to the backend's HTTP API and never sees
  its secrets.

Run the backend first — the frontend needs a live `BACKEND_URL` to talk to. Each has its own
`.env.example` and setup instructions in its README.
