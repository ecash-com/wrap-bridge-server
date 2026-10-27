# Install

Ubuntu Server 24.04, and nothing else on the machine. A minimal image is fine. Prepare first: [prepare.md](prepare.md).

The installer downloads nothing. Everything it installs comes from a `bundle/` directory made beforehand by `make-bundle.sh`, so the server can be installed and fully configured with no network at all, and the first packet it ever sends to the internet is the VPN handshake. The house address is never seen by a package mirror, a registry, or a model host.

## 1. Make the bundle

On a machine with internet, running **Ubuntu 24.04 of the same architecture as the server** (the package files and the built programs are tied to both). A throwaway Multipass or Docker 24.04 on a laptop is fine, as is the server itself before it is wiped, or any 24.04 VM. In a copy of this repository:

```sh
sudo ./make-bundle.sh
```

That installs build tools on that machine, builds the backend and the frontend, downloads the runtime packages, Node.js, cloudflared, and the assistant's model, and writes `bundle/` with a `MANIFEST` of SHA-256 hashes. About 2.5 GB, 2 GB of it the model; `--no-assistant` leaves the model and llama-server out. Ten minutes or so on four cores.

Copy the repository together with `bundle/` to the server, on a USB stick or over the local network. The installer refuses a bundle whose files do not match `MANIFEST`.

## 2. Install

Either online or with the cable unplugged:

```sh
sudo ./install.sh /path/to/bridge-params.conf
```

What that does:

1. Checks the params file. Sample values left over from `params.conf.example`, a leaked key, a hostname where an IP address is required, and a half-filled hop 1 are refused. Then checks every bundle file against `MANIFEST`.
2. Installs the runtime packages from the bundle, skipping any the machine already has at the same or a newer version, and Node.js from the bundled tarball, checked against the pinned hash.
3. Copies this tree to `/opt/ecash-bridge/src`, without `params.conf`, `backend/.env`, or the bundle.
4. Writes `/etc/wireguard/wg0.conf` from the hop 2 fields. If hop 1 is set, it also writes `hop1.conf`. The hop 2 handshake is routed through hop 1, and hop 1's own handshake is kept on the physical route. Neither tunnel is allowed to use the other tunnel's path.
5. Installs the firewall before either tunnel starts. New inbound is dropped, including on the VPN. Outbound is limited to DHCP, the outer handshake, SSH replies, and traffic that leaves through `wg0`. IPv6 is disabled, so the generated WireGuard file keeps only `AllowedIPs = 0.0.0.0/0` even though the params file must still say `0.0.0.0/0, ::/0`. An existing IPv4 SSH session is given a route back to that client; with no network yet, SSH is accepted from private addresses. A new SSH connection from the public internet is never accepted. Reconnect over IPv4 before installing. Do not use mosh for the install session.
6. Starts the tunnel. With a network, it waits for the handshake and checks that the machine's public address is `VPN_PUBLIC_IPV4`; if that fails, the firewall stays on and the SSH session that started the install still works. Without a network, it goes on; the tunnel connects by itself later.
7. Installs the prebuilt backend and frontend from the bundle, cloudflared, and the assistant.
8. Derives the deposit addresses, with no network needed, and refuses to go on if `OWNER_ECX_ADDRESS` belongs to the hot wallet or `OWNER_SOL_ADDRESS` is the treasury. A sweep to the hot wallet would record fees as moved while they stayed on the machine.
9. With a network, starts the backend and the frontend on localhost and waits until both answer; a service that dies on startup is reported with the `journalctl` command to read. Without a network, they start on their own when the tunnel does. Payouts stay off. The public tunnel stays off.
10. Prints the ECX deposit address and the Solana treasury address.

If the server was installed offline, connect it now. Then:

```sh
sudo ./install.sh status
```

The status report shows the tunnel handshake, the public address the traffic leaves from, the services, balances, orders, recent errors, and pending updates. It contains no keys and no customer addresses, so it can be pasted to anyone helping you. The assistant reads the same report; see [assistant.md](assistant.md).

Deposit both sides. ECX goes to the `bc1q` address. SOL, for fees, token-account rent, and the optional wrap stipend, and wECX, for customer unwraps, go to the Solana treasury. Send the wECX from `OWNER_SOL_ADDRESS`: a wrapped-token transfer into the treasury from any other wallet, with no order behind it, is treated as a customer's mistake once payouts are on, and sent back minus the fee. Before payouts are on it is only recorded for review, so a first deposit from another wallet is not refunded, but `swap-backend unmatched-deposits` will list it until you leave it be. Then:

```sh
sudo ./install.sh enable --i-deposited
```

That checks that the machine's traffic really leaves from `VPN_PUBLIC_IPV4`, then sets `ALLOW_LIVE_PAYOUTS=1` and starts `cloudflared`. Customers can then use `CLOUDFLARE_HOSTNAME`. If the address does not match, payouts stay off and the message says what was seen.

`sudo ./install.sh addresses` prints the saved deposit addresses again. It does not print keys.

Running the installer again, with the same or an edited params file, upgrades in place. It installs the bundle's programs (make a new bundle for a new version), swaps the services with an interruption of a few seconds, and keeps the saved deposit addresses. The order database and the wallet store in `/opt/ecash-bridge/backend/data` are never touched, and the treasuries are on-chain, so they follow the keys. If `enable` had already been run, payouts and the public tunnel stay on; a first install is the only run that leaves them off. If a step fails, the message names it; fix the cause and run the installer again. Adding or removing hop 1 later is the same re-run with the `HOP1_` lines filled or emptied; the old tunnels are taken down with their own configuration first, so no routing rules are left behind.

A re-run whose `WALLET_MNEMONIC`, `WALLET_PASSPHRASE`, `SOLANA_KEYPAIR`, or `WECX_MINT_ADDRESS` differs from the installed bridge is refused: the database and the funds belong to the old keys and the old mint. The refusal happens before anything is committed, and the running services are started again as they were. To run a different bridge on the same machine, settle every order and move the funds out first, then move `/opt/ecash-bridge/backend/data` and `/var/lib/ecash-bridge/deposit-addresses.txt` aside.

Stopping `wg-quick@wg0` stops the bridge services with it, and starting it again brings them back. The firewall stays loaded either way, so with the tunnel down the machine cannot reach the internet at all, apart from the VPN handshake itself.

The filled params file is copied to `/etc/ecash-bridge/params.conf`, mode `0600`. The services read generated files in that directory, not the git tree. Do not point public DNS at this machine. The installer does not add a DNS record.

## How the hot keys can be lost

`WALLET_MNEMONIC` and `SOLANA_KEYPAIR` sit in plaintext in `/etc/ecash-bridge/backend.env` and in the memory of the backend process. Whoever reads either controls both treasuries: every coin in the ECX hot wallet, and every wrapped token and all the SOL in the Solana treasury, in one transaction each. One key and three confirmations are the trade for a bridge that one person can run. These are the ways that key leaves your hands.

- **Root on the server.** A guessed or leaked SSH password or key, a second service installed on the same machine, or a hole in sshd or the kernel that is not patched. Root reads the env file. Allow SSH keys only, keep the box single-purpose, and leave Ubuntu's unattended security updates on.
- **The build.** `make-bundle.sh` downloads Rust, Node, cloudflared, llama.cpp, the model, and every crate and npm package the code depends on, and builds on the machine it runs on. A malicious package runs as root there during `cargo build` or `pnpm install`. The lockfiles pin exact versions; treat a lockfile change as code you have to review. The server itself only verifies hashes and copies files.
- **The backend itself.** It has no login and trusts every request the frontend hands it. A bug in axum, Node, SvelteKit, or this code that lets a request read memory or a file exposes the keys. The firewall and the tunnel limit who can reach the bridge, not what a reachable bug can do.
- **The machine you filled `params.conf` on.** That file is a plaintext copy of both keys. So is the terminal scrollback from `ecx-keygen` and `solana-keygen`, a clipboard manager, a screenshot, a cloud-synced home folder, and any backup of that laptop. Keep one offline copy of the two secrets for recovery, and delete every other copy.
- **The disk.** The installer does not encrypt the server's disk. Anyone who takes the machine, or a drive you later throw away, has the keys. Full-disk encryption works but means typing a passphrase at every reboot; decide that before installing Ubuntu.
- **Backups of the server.** A backup that includes `/etc/ecash-bridge/backend.env` is a copy of the keys. The wallet store and the order database in `/opt/ecash-bridge/backend/data` hold no private keys, but they list every deposit address and every order.
- **The chain endpoints.** `ECX_ESPLORA_URL` and `SOLANA_RPC_URL` never see the keys, but the bridge believes what they report. An endpoint that lies about a deposit makes the bridge pay out for a deposit that never happened. The ECX default is a public explorer. `SOLANA_RPC_URL` is a paid account you buy; the installer and a live bridge both refuse the public Solana endpoint. See [../deploy/edge/solana-rpc.md](../deploy/edge/solana-rpc.md).
- **You.** Nobody who runs the bridge, the mint, a wallet, or a support channel needs the words or the key. A request to "verify" or "restore" them is a theft in progress.

What limits the loss is how much sits hot. `MAX_DEPOSIT_AMOUNT_SAT` caps one order. The earnings sweep moves fees to `OWNER_ECX_ADDRESS` and `OWNER_SOL_ADDRESS`; it does not move the float. Keep the two treasuries at the size you are prepared to lose, and top them up from cold storage instead of parking the reserve on the server.
