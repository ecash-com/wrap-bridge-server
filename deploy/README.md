# Deployment files

The supported install is [../install.sh](../install.sh), from the bundle made by
[../make-bundle.sh](../make-bundle.sh), after [../docs/prepare.md](../docs/prepare.md). This
directory holds the files it installs and the guides for the services around the bridge. Nothing
here is applied by hand.

- [ecash-bridge-backend.service](ecash-bridge-backend.service) and
  [ecash-bridge-frontend.service](ecash-bridge-frontend.service): the two bridge services, installed
  by `install.sh` with drop-ins that tie them to the VPN tunnel. `ALLOW_LIVE_PAYOUTS` is not in the
  unit; the installer writes it to `/etc/ecash-bridge/backend.env`, off until `install.sh enable`.
- [earnings.conf](earnings.conf): the drop-in that points the backend at the generated
  `/etc/ecash-bridge/earnings.toml`.
- [edge/](edge/): the location-hiding layout (two VPN hops and a Cloudflare tunnel; a dedicated VPN
  address, not Tor), with [edge/purchase.md](edge/purchase.md) for the VPN tokens and
  [edge/cloudflare.md](edge/cloudflare.md) for the domain and tunnel. Do not point public DNS at the
  bridge machine.
- [Caddyfile](Caddyfile) and [edge/relay.Caddyfile](edge/relay.Caddyfile): reverse-proxy
  configurations for a separate relay host. Not used by the installer, which publishes the site
  only through the Cloudflare tunnel.

Runtime data lives outside this repository and outside the installed source tree:

- Secrets in `/etc/ecash-bridge/backend.env`, generated from `params.conf`, mode `0600`.
- The wallet store and the order database in `/opt/ecash-bridge/backend/data/`. They are not
  source code and need their own private backups; the sweep ledger is part of the database, so
  back the directory up as a whole (see the backend README on restoring).

Do not publish environment files, private keys, wallet stores, databases, or deployment backups.
Build outputs are ignored and are rebuilt by `make-bundle.sh`.
