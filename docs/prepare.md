# Prepare, then install

Do this whole page before `install.sh`. The installer refuses example keys, and it will not invent a wallet for you.

Use a new domain. `swap.beta.ecash.com` already published `172.237.148.99`.

## 1. Buy the path

1. Follow [../deploy/edge/purchase.md](../deploy/edge/purchase.md) for the two VPNs. Hop 2, Surfshark with a Dedicated IP, is required. Hop 1, Mullvad, is optional and keeps the house address away from Surfshark.
2. Follow [../deploy/edge/cloudflare.md](../deploy/edge/cloudflare.md) for the domain, the nameservers, and the tunnel. Add no A or AAAA record.
3. Pay Mullvad, the exit VPN, and Cloudflare with different instruments. Do not use the same email for the exit VPN and Cloudflare.

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

`ASSET_TICKER` is the display name of the one asset this bridge wraps. Leave it as `bECX`, or set it to `ECX` or `aECX`. The wrapped name is always `w` plus that name, so those three are shown as `wbECX`, `wECX`, and `waECX`. It does not add a second asset.

Wrap customers can check a box to buy a little SOL with extra native coin. The defaults are 0.005 SOL for 0.005 of the native asset (`SOL_STIPEND_LAMPORTS = "5000000"`, `SOL_STIPEND_PRICE_SAT = "500000"`). `SOL_STIPEND_USD_LABEL` is only the "about $0.50" text and will go stale. Set both amounts to `0` to hide the checkbox. The treasury pays that SOL. It also creates the recipient token account the first time that wallet receives the token. That rent is about 0.002 SOL. The order adds the same amount of native coin, 1:1 and rounded up, and only when the account is missing. The treasury still fronts the SOL, then the deposit pays it back. Keep the Solana treasury topped up beyond the fee reserve.

## 3. Owner addresses

`OWNER_ECX_ADDRESS` is a `bc1q` address you control, on a different seed from the hot wallet. `OWNER_SOL_ADDRESS` is a Solana address you control, also not the hot wallet. Sweeps go there after the earnings threshold. Customer payouts do not wait for that threshold. The installer and the backend both refuse an owner address that belongs to the hot wallet: a sweep to the hot wallet would record fees as moved to cold storage while they stayed on the server.

## 4. Fill params.conf

```sh
cp params.conf.example "$HOME/bridge-params.conf"
chmod 600 "$HOME/bridge-params.conf"
```

Fill `$HOME/bridge-params.conf`. Every value in the example is a sample that shows the shape of the real one; replace each line whose comment says `Replace`. The installer refuses a sample left in place, including the ones cut short with `...`. The field names match the WireGuard lines and the Cloudflare token described in the purchase and Cloudflare guides. Leave the six `HOP1_WG_` lines empty if you are not using Mullvad; `HOP1_PROVIDER` is only a label.

Check the file before copying it to the server:

```sh
./install.sh check "$HOME/bridge-params.conf"
```

`check` does not install anything and does not print the secrets. A filled file inside this repository is gitignored, but do not rely on that. Keep the filled file outside the tree you commit.

## 5. Make the bundle

The installer downloads nothing. On a machine with internet that runs Ubuntu 24.04 of the same architecture as the server (a throwaway Multipass or Docker 24.04 on your laptop is fine), in a copy of this repository:

```sh
sudo ./make-bundle.sh
```

That builds the programs and collects everything the server needs into `bundle/`, about 2.5 GB.

## 6. Then the server

Copy the repository with its `bundle/` directory and the filled params file to the Ubuntu 24.04 server, on a USB stick if you like; it can stay disconnected until the install is done. Do not copy `backend/.env` from an older machine. On the server:

```sh
sudo ./install.sh "$HOME/bridge-params.conf"
```

The installer configures the VPN and the firewall, installs the service, and prints the two deposit addresses. It does not open the site, and it does not send money. If the server was offline, connect it now; the first thing it sends is the VPN handshake, and `sudo ./install.sh status` shows when the tunnel is up. Deposit the native asset to the printed `bc1q` address and deposit SOL plus the wrapped token to the printed Solana treasury. Then:

```sh
sudo ./install.sh enable --i-deposited
```

That is the step that lets customers in. Details are in [install.md](install.md).
