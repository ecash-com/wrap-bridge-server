# ECX bridge

Custodial bridge for one asset at a time. The default display name is `bECX`, and the wrapped name is always `w` plus that name (`wbECX`). Set `ASSET_TICKER` to `ECX` or `aECX` if that is the name you want shown. This repository does not create the mint and it does not create the AMM pool. The published mint is `EVHqNdzjCupKi4rQkbuYw52sa1m8A7jeUAMP23S9AVVq`. Put a different mint in `params.conf` if you run one.

Install on a new Ubuntu Server only after the tokens and wallets exist. The order is [docs/prepare.md](docs/prepare.md), then [params.conf.example](params.conf.example), then [docs/install.md](docs/install.md).

```sh
cp params.conf.example "$HOME/bridge-params.conf"
./install.sh check "$HOME/bridge-params.conf"
sudo ./make-bundle.sh          # on any Ubuntu 24.04 machine with internet, same architecture
sudo ./install.sh "$HOME/bridge-params.conf"   # on the server, online or not; installs from ./bundle
```

The installer downloads nothing: the server can be installed with the cable unplugged, and its first packet to the internet is then the VPN handshake. Payouts stay off until you deposit both treasuries and run `sudo ./install.sh enable --i-deposited`. `sudo ./install.sh status` shows the state of everything, and `sudo bridge-assistant` explains it with a small language model that runs only on the machine ([docs/assistant.md](docs/assistant.md)).

The service takes ECX and pays wECX, or takes wECX and pays ECX. It holds both treasuries. It does not have mint authority. On a wrap, the customer can pay a fixed extra amount of the native asset to receive a small SOL stipend in the same transaction. If the recipient has no token account, the order also adds that account's rent in native coin, 1:1, and only then. A wallet with no SOL can still receive the wrapped token.

## Parts

- **[backend](backend/)** — the Rust/axum service: HTTP API plus CLI, holds both the ECX (BDK)
  wallet and the Solana treasury, and runs the poller that matches deposits to orders and sends
  the corresponding payout. See its README for how peg-in and peg-out each work.
- **[frontend](frontend/)** — the SvelteKit UI, talks to the backend's HTTP API and never sees
  its secrets.

Run the backend first — the frontend needs a live `BACKEND_URL` to talk to. Each has its own
`.env.example` and setup instructions in its README.
