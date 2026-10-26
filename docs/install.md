# Install

Ubuntu Server 24.04, and nothing else on the machine. Prepare first: [prepare.md](prepare.md).

```sh
sudo ./install.sh /path/to/bridge-params.conf
```

What that does:

1. Checks the file. Example keys, a leaked key, and a half-filled hop 1 are refused.
2. Installs build tools, WireGuard, and Node.js 22. The Node tarball is checked against a pinned SHA-256.
3. Copies this tree to `/opt/ecash-bridge/src`, without `params.conf` or `backend/.env`.
4. Writes `/etc/wireguard/wg0.conf` from the OVPN fields. If hop 1 is set, it also writes `hop1.conf`. The OVPN handshake is routed through Mullvad, and Mullvad's own handshake is kept on the physical route. Neither tunnel is allowed to use the other tunnel's path.
5. Installs the firewall before either tunnel starts. New inbound is dropped, including on the VPN. Outbound is limited to DHCP, the outer handshake, SSH replies, and traffic that leaves through `wg0`. IPv6 is disabled, so the generated WireGuard file keeps only `AllowedIPs = 0.0.0.0/0` even though the params file must still say `0.0.0.0/0, ::/0`. An existing IPv4 SSH session is given a route back to that client. A new SSH connection from the public internet is not accepted. Reconnect over IPv4 before installing. Do not use mosh for the install session.
6. Starts the tunnel and checks that the machine's public address is `VPN_PUBLIC_IPV4`. If that check fails, the firewall stays on and the SSH session that started the install still works.
7. Builds the backend and the frontend through the tunnel, then starts them on localhost. Payouts stay off. The public tunnel stays off.
8. Prints the ECX deposit address and the Solana treasury address.

Deposit both sides. ECX goes to the `bc1q` address. SOL, for fees, token-account rent, and the optional wrap stipend, and wECX, for customer unwraps, go to the Solana treasury. Then:

```sh
sudo ./install.sh enable --i-deposited
```

That sets `ALLOW_LIVE_PAYOUTS=1` and starts `cloudflared`. Customers can then use `CLOUDFLARE_HOSTNAME`.

`sudo ./install.sh addresses` prints the saved deposit addresses again. It does not print keys.

The filled params file is copied to `/etc/ecash-bridge/params.conf`, mode `0600`. The services read generated files in that directory, not the git tree. Do not point public DNS at this machine. The installer does not add a DNS record.
