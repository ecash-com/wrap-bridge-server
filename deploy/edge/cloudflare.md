# Buying and configuring Cloudflare

Checked against Cloudflare's own pages on 2026-10-24. The Free plan is enough. Do this from a machine other than the bridge, and do it after the VPN tokens in [purchase.md](purchase.md) exist. Do not run any command Cloudflare shows you on the bridge.

The bridge gets two tokens from this page: `CLOUDFLARE_TUNNEL_TOKEN` and `CLOUDFLARE_HOSTNAME`. It does not get the Cloudflare password, the global API key, or an API token that can edit DNS.

Use a new domain. `swap.beta.ecash.com` already published `172.237.148.99`. Adding that name to Cloudflare does not erase the old record.

## 1. Account and domain

1. Create an account at https://dash.cloudflare.com/sign-up. Use an email that is not the OVPN account email.
2. In the dashboard, go to **Domains** and select **Onboard a domain**. Enter the apex domain, such as `example.com`, not a hostname under it. Cloudflare's own onboarding page is https://developers.cloudflare.com/fundamentals/manage-domains/add-site/.
3. Select the Free plan. On https://www.cloudflare.com/plans/network-cdn.md that plan is $0 and includes DNS, the CDN, a universal certificate, unmetered DDoS protection, and the basic WAF. Pro is $20 per month billed annually, or $25 billed monthly. Buy Pro only if you want ticket support. The tunnel does not require it.
4. On the DNS review screen, delete every A and AAAA record. A scanned record can point at an old host. Do not add the house address. Do not add `VPN_PUBLIC_IPV4`.
5. At the registrar, replace the nameservers with the two Cloudflare assigns. Wait until the dashboard says the domain is active. Until that happens, the registrar's old DNS can still publish an address.

## 2. Tunnel token

The tunnel is free on the Free plan. Create a remotely managed tunnel. Do not run `cloudflared tunnel login`. That writes a `cert.pem` which can create more tunnels. The bridge must not have that file.

1. Go to **Networking** > **Tunnels**: https://dash.cloudflare.com/?to=/:account/tunnels
2. Select **Create a tunnel**. Name it for this bridge, then select **Create Tunnel**.
3. Cloudflare shows an install command. Copy that command into a text editor. Do not run it. Do not select **Continue** yet. The connector will not connect until the bridge is started later, through the VPN.
4. The token is the `eyJ...` string in that command. That string, and nothing around it, is `CLOUDFLARE_TUNNEL_TOKEN`.
5. If the command is no longer on screen, open the tunnel, go to the **Overview** tab, and select **Add a replica**. Cloudflare's token page says that reveals the command again: https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/configure-tunnels/remote-tunnel-permissions/

Anyone with this token can answer for the tunnel. Treat it like a key. **Refresh token** invalidates the old one. Do not refresh it until the new string is ready to replace the old one.

## 3. Published hostname

The domain must be active before this step. Cloudflare's guide is https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/get-started/create-remote-tunnel/.

1. Open the tunnel. On the **Routes** tab, select **Add route**, then **Published application**.
2. Use one label under the apex. `swap.example.com` is right. `a.b.example.com` is not. Cloudflare requires a paid Advanced Certificate for a name with more than one label, and the free universal certificate will not cover it.
3. Set the service URL to `http://127.0.0.1:3001`. Not `https://`. Not port 3000. The frontend listens on 3001 and is the only public door. The backend stays on 3000 and is not in this form.
4. Do not add a private network route. That would advertise a LAN into Cloudflare.
5. Save the route. Cloudflare creates the DNS record. Open **DNS** for the zone and confirm that record is **Proxied**. A grey-cloud, DNS-only record publishes the target address. If the only record for that name is a proxied tunnel CNAME, leave it. Do not add an A or AAAA beside it.
6. `CLOUDFLARE_HOSTNAME` is that full name, such as `swap.example.com`.

## DNS records to add

None of type A or AAAA.

The published application creates the only record this bridge needs:

| Name | Type | Target | Proxy |
| --- | --- | --- | --- |
| `swap` | CNAME | `<tunnel-id>.cfargotunnel.com` | Proxied |

Do not type that target yourself. The route in step 3 creates it. A public lookup of `swap.example.com` will still return A and AAAA answers. Those addresses belong to Cloudflare's edge, not to this zone. That is what proxied means.

Do not add an A or AAAA for the apex, for `www`, or for the mail host. Any of those can publish the house address or `VPN_PUBLIC_IPV4`. If the apex should send visitors to the bridge, add a second published application for `@` with the same service URL, or a proxied redirect. Still no address record.

Do not add MX records pointing at this bridge. Mail is not part of this service, and an MX host with an address record is another way to publish the house.

A CAA record is optional and is not an address. If you add one, allow only the issuers Cloudflare uses for proxied names. It does not hide the host. Skipping it is fine.

Do not put a Cloudflare Access policy in front of this hostname. Access would demand a Cloudflare login from every customer. This site is public.

## 4. What the installer accepts

| Installer token | Where it comes from |
| --- | --- |
| `CLOUDFLARE_TUNNEL_TOKEN` | The `eyJ...` string in the install command. Not the whole command |
| `CLOUDFLARE_HOSTNAME` | The published application name, one label under the apex |

These two lines are already in [params.conf.example](../../params.conf.example). Leave the Cloudflare password and any API token out of that file.

`cloudflared` has to reach Cloudflare on port 7844, but only through the VPN. The kill switch in [README.md](README.md) allows that as long as it leaves through `wg0`. Do not open 7844 on the house router.

After the VPN is up, the bridge runs `cloudflared` from [cloudflared.service](cloudflared.service). The token goes in `/etc/ecash-bridge/cloudflared.env` as `TUNNEL_TOKEN=eyJ...`, mode `0600`. Starting it earlier lets Cloudflare learn the house address.
