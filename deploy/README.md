# Production Deployment

The location-hiding layout is [edge/README.md](edge/README.md). It uses an OVPN dedicated address, not Tor. Buy the VPN tokens by following [edge/purchase.md](edge/purchase.md). Buy the Cloudflare tokens by following [edge/cloudflare.md](edge/cloudflare.md). Do not point public DNS at the bridge machine.



## Laptop / Server Parity Check Before Earnings Rollout

Verified on 2026-10-23 against SSH host alias `ecash-bridge-vps`:

- The live backend binary matches the laptop's `backend/target/release/swap-backend` byte-for-byte.
- Every file in the live frontend `build/` matches the laptop's `frontend/build/` (paths and SHA-256 checksums).
- Production has compiled deployment artifacts, not backend or frontend source checkouts, under `/opt/ecash-bridge/`.
- The local `backend/src/db.rs` and `backend/src/server.rs` match the isolated source tree used to build the deployed stats and recent-orders features.
- Local changes in `backend/src/main.rs` and `backend/src/solana.rs` add manual SOL-send CLI functionality that is not in the deployed binary. Keep those changes separate when preparing a production-parity commit.

Verified artifact fingerprints:

```text
Backend binary SHA-256:
c50086f03d628ed626e0dfe3a33389a405d30964cbe40bbc78e21ba7a6ddcef5

Frontend sorted file/checksum manifest SHA-256:
20bf2499921b802593b09e97ef615eb4f90a793e3d7c21289a2a10dbbc289661
```

The frontend fingerprint is computed from its package directory:

```sh
find build -type f -exec sha256sum {} + | LC_ALL=C sort | sha256sum
```

## Configuration

- `ecash-bridge-frontend.service` started from the production unit. It now also sets `ADDRESS_HEADER` and extra sandboxing. Do not copy it over the live unit until that header and the sandbox are intentional.
- `Caddyfile` still proxies `swap.beta.ecash.com` to `127.0.0.1:3001`. It now also sets a request-body cap and security headers, which are not yet on the live proxy.
- `ecash-bridge-backend.service` no longer hardcodes `ALLOW_LIVE_PAYOUTS`. The installer sets that in `/etc/ecash-bridge/backend.env`, and leaves it at 0 until `install.sh enable`. The live server has `ALLOW_LIVE_PAYOUTS=1`. Do not copy this unit over the live unit without deliberately choosing the payout setting.
- Backend secrets remain in `/etc/ecash-bridge/backend.env` on the server. They were not copied into this repository.
- Wallet stores and the order database remain in `/opt/ecash-bridge/backend/data/`. They are runtime data, not source code, and need separate private backups.
- Do not publish environment files, private keys, wallet stores, databases, or deployment backups. Build outputs are ignored and should be rebuilt from source rather than committed.

## Git Preparation

The Git root is the parent `wrapped-ecx/` directory, not this bridge subdirectory. The configured origin is `https://github.com/ecash-com/wrapped-ecx.git`.

At the time of this check, the stats routes and deployment directory were untracked, while the backend stats implementation and other local work were modified but uncommitted. Include the desired source files explicitly, review the staged diff for secrets and unrelated changes, then choose a branch or new remote before publishing. No commit, branch, or push was made during this check.

## Earnings Sweep Release (2026-10-23)

The subsequent backend-only release adds historical earnings sweeps, configured by
`backend/earnings.toml`. Production uses a root-owned copy at
`/etc/ecash-bridge/earnings.toml` and the `earnings.conf` drop-in installed under
`/etc/systemd/system/ecash-bridge-backend.service.d/`. The frontend was unchanged.
The current backend binary SHA-256 is:

```text
a041e4ca17ebc7fefb76fec8162b719388d5f3eeca0396e07d16c2478da37f0a
```

The tested release snapshot was staged at `/tmp/ecash-earnings-release-20261023/`
on the laptop. Its sweep implementation matches the working source; the unrelated
manual SOL-send changes remain excluded. The working tree passed 30 tests and
the isolated production source passed 29 tests.

Initial historical bECX sweep:

```text
Amount:      181.93592659 bECX
Destination: bc1qress9jkv0x7z3mc5tev9cnx9wk235kvsq0yur5
Transaction: 29f8bb1d5e369fbc0466b768f55f1fb551e6734ac68508dd65743f25f284116b
Network fee: 0.00002449 bECX
```

The explorer verified the output, with block confirmation still pending at the
time of the deployment check. The ledger marks the ECX transfer completed after
accepted broadcast, matching the existing ECX payout convention. Unswept wbECX
earnings were 31.43811, below the configured 100-coin threshold, so no wbECX
sweep was sent.

Private pre-release database, wallet, and binary backups are on the VPS at
`/opt/ecash-bridge/backups/earnings-20261023-a/`. Do not restore the pre-sweep
database over the live sweep ledger without reconciling the transfer on-chain.
See the backend README for pending-transfer recovery and reservation rules.

Separately, 14 funded wrap orders were waiting because the treasury had
0.019697241 SOL, below its configured transaction-fee reserve. That safety floor
was not changed. Replenish SOL to the treasury owner wallet to resume payouts.