# Private edge

The supported install is [install.sh](../../install.sh), after [prepare.md](../../docs/prepare.md). This folder explains the path. Do not apply these templates by hand when using the installer.



Tor is not the recommended hop. A paid VPN with one dedicated address is enough, and it does not depend on a volunteer network.

```text
customer → Cloudflare → tunnel from the bridge → OVPN dedicated address → house
```

The bridge's default route is the VPN. Cloudflare, Esplora, and Solana all see the dedicated address. The VPN provider still sees the house address. That is the trade for dropping Tor.

The dedicated address is an exit identity, not a public door. OVPN's Public IPv4 add-on can open ports. This host must not listen on them. Customers enter only through the Cloudflare tunnel.

## What each party can see

| Party | Sees the house address? |
| --- | --- |
| Customer | No, if DNS stays proxied and no old record leaked |
| Cloudflare | No, while every tunnel packet exits through the VPN |
| OVPN | Yes. They terminate the tunnel from the house |
| Esplora or Solana | No, if those calls also use the VPN route |
| House ISP | A WireGuard session to OVPN, not the domain |

`swap.beta.ecash.com` already published `172.237.148.99`. Use a new name. An old unproxied record, or a certificate for the public name on the bridge, undoes the rest.

## 1. VPN

Use OVPN and add Public IPv4. Shared OVPN addresses are NAT exits, and Proton's manual WireGuard files are shared servers that can be retired. OVPN's dedicated address is a normal WireGuard config, which a headless Ubuntu server can run without their app.

Buy the accounts and copy the tokens by following [purchase.md](purchase.md). The installer fields are listed in [params.conf.example](../../params.conf.example). The private key and the Cloudflare token are secrets. The account password does not belong on the bridge.

Install it as `/etc/wireguard/wg0.conf`, mode `0600`, with `AllowedIPs = 0.0.0.0/0, ::/0`. Enable `wg-quick@wg0`. Disable IPv6 on the physical NIC, or route it through the tunnel too. A v6 leak publishes the house.

`wg-quick` is not a kill switch. If the tunnel drops, new connections will use the house address. Install [nftables-killswitch.conf.example](nftables-killswitch.conf.example) after putting the endpoint from `VPN_WG_ENDPOINT` into it. It allows only DHCP, that endpoint, and traffic out `wg0`. [nftables-bridge.conf](nftables-bridge.conf) already drops new inbound packets, including packets that arrive through the VPN. Do not add an accept rule for the web ports.

## Optional second VPN

A second tunnel is practical, and it is the way to stop OVPN from seeing the house. It does not remove every witness. It splits the witness in two.

```text
house → Mullvad WireGuard → OVPN dedicated address → Cloudflare
```

Mullvad is hop 1 because it publishes a headless WireGuard config and does not need a dedicated address. OVPN stays hop 2, because that is the address Cloudflare sees. OVPN's own Multihop product is not this. Both of those servers belong to OVPN, so OVPN still sees the house.

| Party | Learns |
| --- | --- |
| Mullvad | The house address, and that this box talks to an OVPN endpoint. Not the customer domain |
| OVPN | A Mullvad exit address, then Cloudflare. Not the house, if the inner tunnel never starts on the physical NIC |
| Cloudflare | The OVPN dedicated address |

Linking the house to the domain then takes both companies, or a mistake. The mistake is the inner tunnel reconnecting from the house when the outer tunnel drops. The firewall has to be three layers: the physical NIC may talk only to the Mullvad endpoint, Mullvad's interface may talk only to the OVPN endpoint, and only the OVPN interface may talk to the internet. Bring Mullvad up first, confirm its handshake, then bring OVPN up, then start `cloudflared`. Set the inner MTU to 1280. Two WireGuard headers will otherwise drop large packets, including the Cloudflare tunnel.

Pay the two accounts separately. The same card links them even when the addresses do not.

A third VPN is not worth it. Two companies must already cooperate to link the house to the domain. A third company adds another legal step, and also another tunnel that can reconnect from the wrong interface and publish the house. It does not help against someone who can already see both the house link and the dedicated address, and it does not help against the burned DNS record. Stop at two.

Hop 1 adds the same WireGuard fields again, prefixed `HOP1_`, in [params.conf.example](../../params.conf.example). It does not need `VPN_PUBLIC_IPV4`.

## 2. Cloudflare

Buy the domain plan and copy the two tokens by following [cloudflare.md](cloudflare.md). The Free plan is enough. Run `cloudflared` on the bridge, not on a second VPS, and only after the VPN is up. Its packets leave through `wg0`, so Cloudflare learns `VPN_PUBLIC_IPV4`.

Store the token only in `/etc/ecash-bridge/cloudflared.env` as `TUNNEL_TOKEN=...`, mode `0600`, and use [cloudflared.service](cloudflared.service). The frontend and backend stay on localhost. The public-name [Caddyfile](../Caddyfile) stays off this machine.

## 3. Check

With the tunnel up, a remote check of the house address must not show the customer domain. A check from the bridge must show `VPN_PUBLIC_IPV4`. Stop `wg0` and confirm the bridge cannot reach Cloudflare or the chain RPC. Then start it again.

## What this does not fix

OVPN can connect the dedicated address to the account, and the account to the house address. Pay and register it in a way that matches that risk. A compelled or breached VPN provider is the remaining witness.

A person at the machine can still demand the keys while the service is unlocked. Keep the hot float small, and keep the spending keys off the public host when that split is built.

The Tor and self-hosted relay files in this directory are optional. They hide the house from the VPN provider as well, at the cost of a volunteer network or a second company that can see the house. They are not required for the layout above.
