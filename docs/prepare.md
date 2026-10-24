# Prepare, then install

Do this whole page before `install.sh`. The installer refuses example keys, and it will not invent a wallet for you.

Use a new domain. `swap.beta.ecash.com` already published `172.237.148.99`.

## 1. Buy the path

1. Follow [../deploy/edge/purchase.md](../deploy/edge/purchase.md) for Mullvad and OVPN. Hop 1 is optional. Hop 2, OVPN Public IPv4, is required.
2. Follow [../deploy/edge/cloudflare.md](../deploy/edge/cloudflare.md) for the domain, the nameservers, and the tunnel. Add no A or AAAA record.
3. Pay Mullvad, OVPN, and Cloudflare with different instruments. Do not use the same email for OVPN and Cloudflare.

## 2. Make new wallets

These keys will sit on the server while it is running. Generate them on a machine you trust, not in a browser. Do not reuse a phrase or a Solana key that already holds other funds.

Install Rust on that machine, then from this directory:

```sh
cargo run --manifest-path backend/Cargo.toml -- ecx-keygen
cargo run --manifest-path backend/Cargo.toml -- solana-keygen
```

`ecx-keygen` prints a 12-word BIP39 mnemonic. That becomes `WALLET_MNEMONIC`. The bridge derives native segwit addresses at `m/84'/0'/0'`, the same encoding as Bitcoin. The first address is a `bc1q` address. `solana-keygen` prints a base58 64-byte secret and its public key. The secret becomes `SOLANA_KEYPAIR`. The public key is the Solana treasury. You will send SOL and wECX there after install.

Write both secrets into the copy of `params.conf` and clear the terminal scrollback. Neither command stores the key.

`WECX_MINT_ADDRESS` is the SPL mint this bridge holds and pays out. It is not a mint authority. The published mint is `EVHqNdzjCupKi4rQkbuYw52sa1m8A7jeUAMP23S9AVVq`. If you run a different mint, put that address here. The installer passes it to the backend and the frontend.

## 3. Owner addresses

`OWNER_ECX_ADDRESS` is a `bc1q` address you control, on a different seed from the hot wallet. `OWNER_SOL_ADDRESS` is a Solana address you control, also not the hot wallet. Sweeps go there after the earnings threshold. Customer payouts do not wait for that threshold.

## 4. Fill params.conf

```sh
cp params.conf.example "$HOME/bridge-params.conf"
chmod 600 "$HOME/bridge-params.conf"
```

Fill `$HOME/bridge-params.conf`. The field names match the WireGuard lines and the Cloudflare token described in the purchase and Cloudflare guides. Leave every `HOP1_` line empty if you are not using Mullvad.

Check the file before copying it to the server:

```sh
./install.sh check "$HOME/bridge-params.conf"
```

`check` does not install anything and does not print the secrets. A filled file inside this repository is gitignored, but do not rely on that. Keep the filled file outside the tree you commit.

## 5. Then the server

Copy the repository and the filled file to the Ubuntu 24.04 server. Do not copy `backend/.env` from an older machine. On the server:

```sh
sudo ./install.sh "$HOME/bridge-params.conf"
```

The installer brings up the VPN, builds the service, and prints the two deposit addresses. It does not open the site, and it does not send money. Deposit ECX to the printed `bc1q` address and deposit SOL plus wECX to the printed Solana treasury. Then:

```sh
sudo ./install.sh enable --i-deposited
```

That is the step that lets customers in. Details are in [install.md](install.md).
