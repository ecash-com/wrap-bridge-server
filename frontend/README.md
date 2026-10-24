# swap-frontend

SvelteKit UI for the ECX ↔ wECX bridge. Talks to [swap-backend](https://github.com/marcusmmmz/ecx-swap-backend)
over its HTTP API; server routes under `src/routes/` proxy requests to the
backend so the backend URL is never exposed to the browser.

## Setup

```sh
pnpm install
cp .env.example .env   # set BACKEND_URL to your running swap-backend instance
pnpm dev                # http://localhost:5173
```

## Scripts

- `pnpm dev` — dev server
- `pnpm build` / `pnpm preview` — production build / preview
- `pnpm check` — svelte-check
- `pnpm lint` / `pnpm format` — prettier

## Notes

- Both ECX betanet and Solana mainnet-beta hold real value; there's no test
  network for either side of the bridge. See swap-backend's README for the
  safety gates on the backend side.
- No secrets live in this app — it only holds `BACKEND_URL` and proxies
  everything else to the backend.
- Known issue: scanning the Solana Pay QR code directly with the Phantom or
  Solflare in-app scanner doesn't work. Scanning it with the phone's regular
  camera app, or tapping it, works fine.

## License

MIT — see [LICENSE](LICENSE).
