## ECX bridge

This is a custodial bridge for converting **ECX** into **wrapped ECX** (aka wECX) -- and vice versa.

It is designed to be (relatively) easy to start up, and operate:

1. Sign up for [Cloudflare](https://www.cloudflare.com/), [SurfShark](https://surfshark.com/), [Mullvad](https://mullvad.net/en), and [Helius](https://www.helius.dev/).
2. Create new wallets (keep the private keys safe and private!):
   - 1 hot ECX wallet
   - 1 cold storage ECX wallet
   - 1 hot solana wallet
   - 1 cold storage solana wallet
3. Get a server somewhere, with the [Ubuntu Server](https://ubuntu.com/download/server) operating system.
4. Copy everything in this repo to that server.
5. Edit **params.conf** to replace all the example values with your own values (see below). Be sure to replace "bECX" with "ECX", for example, if you are using real ECX.
6. Run the **install.sh** below.

It should just work! And it should be: secure, safe, ddos-resistant, and user-friendly for swapping. It even comes with a tiny LLM (optional), so that you have someone to talk to.

Install on a new Ubuntu Server only after the tokens and wallets exist.

The order is: [docs/prepare.md](docs/prepare.md), then [params.conf.example](params.conf.example), then [docs/install.md](docs/install.md).

### Install

```sh
cp params.conf.example "$HOME/bridge-params.conf"
./install.sh check "$HOME/bridge-params.conf"
sudo ./make-bundle.sh          # on any Ubuntu 24.04 machine with internet, same architecture
sudo ./install.sh "$HOME/bridge-params.conf"   # on the server, online or not; installs from ./bundle
```

The installer downloads nothing: the server can be installed with the cable unplugged, and its first packet to the internet is then the VPN handshake. Payouts stay off until you deposit both treasuries and run `sudo ./install.sh enable --i-deposited`. `sudo ./install.sh status` shows the state of everything, and `sudo bridge-assistant` explains it with a small language model that runs only on the machine ([docs/assistant.md](docs/assistant.md)).

The service takes ECX and pays wECX, or takes wECX and pays ECX. It holds both treasuries. It does not have mint authority. On a wrap, the customer can pay a fixed extra amount of the native asset to receive a small SOL stipend in the same transaction. If the recipient has no token account, the order also adds that account's rent in native coin, 1:1, and only then. A wallet with no SOL can still receive the wrapped token.

### Parts

- **[backend](backend/)** — the Rust/axum service: HTTP API plus CLI, holds both the ECX (BDK)
  wallet and the Solana treasury, and runs the poller that matches deposits to orders and sends
  the corresponding payout. See its README for how peg-in and peg-out each work.
- **[frontend](frontend/)** — the SvelteKit UI, talks to the backend's HTTP API and never sees
  its secrets.

Run the backend first — the frontend needs a live `BACKEND_URL` to talk to. Each has its own
`.env.example` and setup instructions in its README.

### Feedback

If you have ideas for improving this project, please let us know!
