# Buying a paid Solana RPC

Checked against Helius's own pages on 2026-10-27. Prices change. The installer does not know provider names. Any HTTPS Solana mainnet URL that can read recent history and send a transaction works. Helius is the one written out below because its paid Developer plan is enough and its dashboard shows the URL.

Buy this from a machine other than the bridge. Do not install a provider app on the bridge. The bridge gets one value from this page: `SOLANA_RPC_URL`. It does not get the provider password.

Pay with an instrument that is not the one used for the exit VPN or for Cloudflare. Do not use the same email for this account, the exit VPN, and Cloudflare.

The public endpoints `api.mainnet-beta.solana.com`, `api.devnet.solana.com`, and `api.testnet.solana.com` are refused. A live bridge will not start on them, and `install.sh` will not write them into the service. They are shared, they rate-limit, and a keyless URL is not an account you can call when a payout is stuck.

## What the bridge needs

The poller reads token accounts, signatures, and transaction history, and it sends the wrapped-token payouts and refunds. A free shared plan is not the setup this guide is for. Helius's free plan is 10 requests per second and 1 `sendTransaction` per second. Use a paid plan.

The URL is a secret. Anyone with it can spend the provider's quota and, on some plans, send transactions through the account. It goes only in the params file, which the installer copies to `/etc/ecash-bridge/backend.env`.

## Helius, Developer plan

1. Create an account at https://dashboard.helius.dev/signup. Use an email that is not the exit VPN's account email and not the Cloudflare account email.
2. Open https://www.helius.dev/pricing. The Developer plan is $49 per month. It includes 10 million credits, 50 requests per second, and 5 `sendTransaction` per second. Archival history is included on that plan. The Free plan is $0 and is not enough. Business is $499 per month; buy it only if the Developer quota is exhausted. They accept card or crypto. Annual billing is discounted on that page.
3. In the dashboard, create a mainnet endpoint. Copy the HTTPS URL the dashboard shows. It looks like `https://mainnet.helius-rpc.com/?api-key=` followed by the key. That whole string is `SOLANA_RPC_URL`.
4. Do not copy a devnet or testnet URL. This bridge pays real Solana mainnet.

Helius's pricing page: https://www.helius.dev/pricing. Signup: https://dashboard.helius.dev/signup.

A free Helius key and a paid one look the same in the URL. The installer cannot tell them apart. Confirm in the dashboard that the project is on Developer or higher before you enable payouts.

## Any other provider

Fill the same line. The URL must start with `https://`. It must not be one of the three public hosts above. It must be mainnet, and it must still answer `getTransaction` for a signature that is no longer in the newest slots: the bridge matches unwrap deposits by walking history. A trimmed node that only keeps recent slots will miss a deposit and can pay the wrong person, or pay nobody, when the history is gone.

After you paste it, the key is inside the URL. Do not put the URL in a ticket, a screenshot, or the terminal scrollback you keep.

## What the installer accepts

Copy [params.conf.example](../../params.conf.example) outside this repository, as [prepare.md](../../docs/prepare.md) says. `SOLANA_RPC_URL` is one of the lines the installer refuses while it still holds the sample.

| Installer token | Where it comes from |
| --- | --- |
| `SOLANA_RPC_URL` | The HTTPS mainnet URL from the provider dashboard, including the API key |
