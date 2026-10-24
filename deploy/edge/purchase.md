# Buying the two VPNs

Checked against the providers' own pages on 2026-10-24. Prices change. The field names below do not: they are the lines inside a WireGuard file, plus one address that is not in that file.

Buy these from a machine other than the bridge. Do not install either provider's app on the bridge. The bridge only gets the tokens in the last section.

Pay Mullvad and OVPN with different instruments. The same card, PayPal account, or email links the two hops even when the tunnels do not. Mullvad can be paid with cash or Monero and does not require an email. Use that for hop 1 if you can.

## Hop 1: Mullvad

This hop does not need a dedicated address. One shared Mullvad server is enough.

1. Open https://mullvad.net/en/account/create and generate an account. Mullvad shows a numeric account number. That number is the login. Write it down somewhere that is not the bridge.
2. Pay at the account page. The price on https://mullvad.net/en/pricing is €5 per month, including VAT, whether you buy one month or longer. They accept cash, Bitcoin, Bitcoin Cash, Monero, bank wire, credit card, PayPal, Swish, Eps, Bancontact, iDEAL | Wero, and Przelewy24. Cryptocurrency is discounted 10% on that page. Cash uses a payment token from their site, mailed with the cash. A 14-day refund exists, except for cash.
3. On the same machine, generate a keypair. Do not let Mullvad generate the private key.

```sh
wg genkey | tee mullvad.private | wg pubkey > mullvad.public
chmod 600 mullvad.private
```

4. Log in with the account number and add one device by pasting `mullvad.public` only. Then open https://mullvad.net/en/account/wireguard-config?platform=linux and download one server, for example a Sweden server. Do not enable Mullvad Multihop. Do not enable Mullvad's kill switch. Both fight the layout in [README.md](README.md).
5. Open the downloaded file, named like `se-mma-wg-001.conf`. If `PrivateKey` is already filled, Mullvad generated it. Delete that download and repeat from step 3. `PrivateKey` must be empty, or absent, so you can fill it from `mullvad.private`.

One Mullvad account allows 5 devices. This bridge is one device. Do not reuse this key on another machine.

## Hop 2: OVPN Public IPv4

This is the address Cloudflare will see. A normal OVPN subscription is not enough. Shared OVPN addresses are NAT exits.

1. OVPN's own pages price the subscription at $12 for one month, $4.99 per month for 12 months, or $4.22 per month for 36 months: https://www.ovpn.com/en/pricing. Account creation starts on that page. Login is a username or email plus a password: https://www.ovpn.com/en/account/login.
2. Before paying for the add-on, write to OVPN support and say this is a commercial server with a Cloudflare tunnel, not a home browsing account. Their Public IPv4 page says commercial use with higher bandwidth can be terminated without a refund unless they approve it. A surprise termination takes the bridge offline.
3. After they approve it, buy Public IPv4 from https://www.ovpn.com/en/features/public-ipv4. The page prices that add-on at $4 per month and asks you to pick a city. Pick a city in a different country from the house. Stockholm, Malmö, and Gothenburg had supply when this was checked. Several cities were sold out.
4. Generate the hop 2 key on the same non-bridge machine:

```sh
wg genkey | tee ovpn.private | wg pubkey > ovpn.public
chmod 600 ovpn.private
```

5. Log in. Their Ubuntu guide says the WireGuard files are available only after login, and their configuration generator accepts a public key you paste in. Paste `ovpn.public`. Download the WireGuard config for the Public IPv4 you just bought, not a shared server, and not an OpenVPN file from https://www.ovpn.com/en/configurations.
6. Do not enable OVPN Multihop. That stays inside OVPN and does not hide the house from them.
7. In the downloaded file, `PrivateKey` must be empty. If OVPN filled it, delete the file and use the public-key generator instead.
8. Copy the dedicated address from the Public IPv4 page in the account. It is not one of the lines in the WireGuard file. That address is `VPN_PUBLIC_IPV4`.

## What the installer accepts

Copy [params.conf.example](../../params.conf.example) to a path outside this repository. Fill only these tokens. The installer does not get the Mullvad account number, the OVPN username, the OVPN password, or either private-key file left in Downloads.

| Installer token | Where it comes from |
| --- | --- |
| `HOP1_PROVIDER` | The literal `mullvad` |
| `HOP1_WG_PRIVATE_KEY` | `mullvad.private`, also the `PrivateKey` line under `[Interface]` |
| `HOP1_WG_ADDRESS` | `Address` under `[Interface]` in the Mullvad file |
| `HOP1_WG_DNS` | `DNS` under `[Interface]` in the Mullvad file |
| `HOP1_WG_PEER_PUBLIC_KEY` | `PublicKey` under `[Peer]` in the Mullvad file |
| `HOP1_WG_ENDPOINT` | `Endpoint` under `[Peer]` in the Mullvad file |
| `HOP1_WG_ALLOWED_IPS` | `0.0.0.0/0, ::/0` |
| `VPN_PROVIDER` | The literal `ovpn` |
| `VPN_WG_PRIVATE_KEY` | `ovpn.private`, also the `PrivateKey` line under `[Interface]` |
| `VPN_WG_ADDRESS` | `Address` under `[Interface]` in the OVPN file |
| `VPN_WG_DNS` | `DNS` under `[Interface]` in the OVPN file |
| `VPN_WG_PEER_PUBLIC_KEY` | `PublicKey` under `[Peer]` in the OVPN file |
| `VPN_WG_ENDPOINT` | `Endpoint` under `[Peer]` in the OVPN file |
| `VPN_WG_ALLOWED_IPS` | `0.0.0.0/0, ::/0` |
| `VPN_PUBLIC_IPV4` | The dedicated address shown for the Public IPv4 add-on. Not a line in the WireGuard file |
| `CLOUDFLARE_TUNNEL_TOKEN` | The `eyJ...` string from [cloudflare.md](cloudflare.md). Not the install command |
| `CLOUDFLARE_HOSTNAME` | The published name from [cloudflare.md](cloudflare.md). Not `swap.beta.ecash.com` |

Leave every `HOP1_` line empty if you are using OVPN alone. One VPN is enough to hide the house from Cloudflare. It is not enough to hide the house from OVPN.

After the tokens are in the outside file, delete `mullvad.private`, `ovpn.private`, and both downloaded `.conf` files from the machine you used to buy the accounts. Keep the account number and the OVPN login somewhere you can reach without the bridge. You need them to renew, and to replace a config if a server is retired. You do not need them to run the bridge.
