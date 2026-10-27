use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use bdk_wallet::bitcoin::{Amount, FeeRate, Network};

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EarningsConfig {
    pub enabled: bool,
    pub earnings_threshold_wbecx: String,
    pub earnings_threshold_becx: String,
    pub solana_owner_address: String,
    pub ecx_owner_address: String,
}

impl EarningsConfig {
    pub fn load() -> Result<Option<Self>> {
        let explicit = std::env::var("EARNINGS_CONFIG_PATH").ok();
        let path = explicit.as_deref().unwrap_or("earnings.toml");
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if explicit.is_none() && error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        Ok(Some(Self::parse(&text)?))
    }

    fn parse(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text)?;
        config.thresholds(8)?;
        crate::solana::parse_pubkey(&config.solana_owner_address)?;
        config.ecx_owner_address.parse::<bdk_wallet::bitcoin::Address<_>>()?
            .require_network(Network::Bitcoin)?;
        Ok(config)
    }

    pub fn thresholds(&self, wecx_decimals: u8) -> Result<(u64, u64)> {
        Ok((
            coin_amount(&self.earnings_threshold_wbecx, wecx_decimals)?,
            coin_amount(&self.earnings_threshold_becx, 8)?,
        ))
    }
}

const DEFAULT_ASSET_TICKER: &str = "bECX";

fn valid_ticker(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphabetic())
        && chars.all(|ch| ch.is_ascii_alphanumeric())
        && value.len() <= 16
}

/// Display name of the one asset this bridge wraps. The wrapped name is always "w" plus this.
pub fn asset_ticker() -> String {
    std::env::var("ASSET_TICKER")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| valid_ticker(value))
        .unwrap_or_else(|| DEFAULT_ASSET_TICKER.to_string())
}

pub fn wrapped_ticker() -> String {
    format!("w{}", asset_ticker())
}

fn coin_amount(text: &str, decimals: u8) -> Result<u64> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    anyhow::ensure!(
        !whole.is_empty() && whole.bytes().all(|digit| digit.is_ascii_digit())
            && fraction.bytes().all(|digit| digit.is_ascii_digit())
            && fraction.len() <= usize::from(decimals),
        "invalid earnings threshold: expected a positive coin amount"
    );
    let scale = 10u64.checked_pow(u32::from(decimals))
        .ok_or_else(|| anyhow::anyhow!("unsupported token decimals"))?;
    let units = whole.parse::<u64>()?.checked_mul(scale)
        .and_then(|units| {
            let fractional = if fraction.is_empty() { 0 } else { fraction.parse::<u64>().ok()? };
            units.checked_add(fractional.checked_mul(10u64.checked_pow(u32::from(decimals) - fraction.len() as u32)?)?)
        })
        .ok_or_else(|| anyhow::anyhow!("earnings threshold overflow"))?;
    anyhow::ensure!(units > 0, "earnings threshold must be positive");
    Ok(units)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Esplora,
    Electrum,
}

impl Backend {
    fn from_env(var: &str, default: Backend) -> Backend {
        match std::env::var(var).ok().as_deref() {
            Some("esplora") => Backend::Esplora,
            Some("electrum") => Backend::Electrum,
            Some(other) => panic!("{var}: unknown backend {other:?}, expected esplora or electrum"),
            None => default,
        }
    }
}

pub struct ChainConfig {
    pub network: Network,
    pub backend: Backend,
    pub esplora_url: String,
    pub electrum_url: String,
    pub db_path: PathBuf,
}

impl ChainConfig {
    /// ECX's betanet network is a shadow fork of real Bitcoin mainnet (confirmed: its genesis
    /// hash matches mainnet's exactly), so it uses the same `Network::Bitcoin` address/key
    /// encoding BTC mainnet does - see src/keys.rs.
    ///
    /// betanet is the network this repo targets (see CLAUDE.md - drynet4 is an older, different
    /// network this repo does not use anymore). Both betanet and Bitcoin mainnet are real money
    /// (see CLAUDE.md) - there is no test network to develop against, so every send here is real.
    pub fn ecx() -> Self {
        let data_dir = data_dir();
        // Public betanet infra has been flaky in the past (address indexer 500s, esplora.* host
        // down, electrum port not responding - all observed and since recovered at various
        // points); if it acts up again, override with ECX_BACKEND=electrum (see src/wallet.rs for
        // both code paths).
        let backend = Backend::from_env("ECX_BACKEND", Backend::Esplora);
        // No known-good betanet electrum default to fall back to (unlike the esplora URL below)
        // - fail loudly instead of guessing a host for a real-money network. Only required (and
        // only read, see wallet.rs) when the electrum backend is actually selected - computed
        // from `backend` here rather than as an unconditional struct-field initializer, so
        // ECX_BACKEND=esplora (the default) never touches ECX_ELECTRUM_URL at all.
        let electrum_url = match backend {
            Backend::Electrum => std::env::var("ECX_ELECTRUM_URL").unwrap_or_else(|_| {
                panic!(
                    "ECX_BACKEND=electrum but ECX_ELECTRUM_URL is not set - no default betanet \
                     electrum endpoint is known, set it explicitly"
                )
            }),
            Backend::Esplora => String::new(),
        };
        ChainConfig {
            network: Network::Bitcoin,
            backend,
            esplora_url: std::env::var("ECX_ESPLORA_URL")
                .unwrap_or_else(|_| "https://explorer.beta.ecash.ninja/api".to_string()),
            electrum_url,
            db_path: data_dir.join("ecx.db"),
        }
    }
}

fn data_dir() -> PathBuf {
    let dir = PathBuf::from("data");
    // Holds the wallet store (which reveals every deposit address and UTXO) - owner-only, not the
    // default umask's world-readable 0755. Only applies when this call creates the directory; an
    // existing one keeps whatever mode it already has.
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .expect("failed to create data directory");
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(&dir).expect("failed to create data directory");
    dir
}

/// 1 sat/vB in the sat/kwu units `FeeRate` actually stores (1000 weight units = 250 vbytes).
/// sat/kwu is finer-grained than whole sat/vB, so a fractional sat/vB rate below 1 (e.g. 0.1,
/// useful for ECX's betanet where a bare 1 sat/vB can be overkill) still lands on an exact
/// sat/kwu value instead of getting rounded away by an integer-only parse.
const SAT_PER_KWU_PER_SAT_PER_VB: f64 = 250.0;

/// Fee rate used for automatic ECX sends (payouts, refunds), from `ECX_PAYOUT_FEE_RATE_SAT_VB`
/// (default 2 sat/vB, matching the CLI `Send` command's default). Accepts fractional sat/vB (e.g.
/// `ECX_PAYOUT_FEE_RATE_SAT_VB=0.1`) - missing, unparsable, negative, or non-finite falls back to
/// the 2 sat/vB default rather than producing a nonsensical rate.
pub fn ecx_payout_fee_rate() -> FeeRate {
    let sat_per_vb = std::env::var("ECX_PAYOUT_FEE_RATE_SAT_VB")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(2.0);
    FeeRate::from_sat_per_kwu((sat_per_vb * SAT_PER_KWU_PER_SAT_PER_VB).round() as u64)
}

/// How often the background poller re-checks not-yet-settled orders for deposit confirmation /
/// payout, from `ORDER_POLL_INTERVAL_MINUTES` (default 5).
pub fn order_poll_interval() -> Duration {
    let minutes = std::env::var("ORDER_POLL_INTERVAL_MINUTES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(5);
    Duration::from_secs(minutes.max(1) * 60)
}

/// How long an order can sit without a deposit ever showing up before the poller gives up on it
/// and marks it `expired`, from `ORDER_LIFETIME_MINUTES` (default 15). The older
/// `ORDER_MAX_AGE_HOURS` is still honoured when the new name is absent. Only applies while an order
/// is still awaiting a deposit (`pending`) - once a deposit is seen, the funds are customer money
/// and the order never expires. Expiry is decided on poller ticks, so the effective lifetime is
/// this plus up to one `order_poll_interval`. An order whose deposit arrives after expiry is revived
/// for `expiry_grace`, both by the poller and when the customer opens the order page.
pub fn order_max_age() -> Duration {
    let minutes = std::env::var("ORDER_LIFETIME_MINUTES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .or_else(|| {
            std::env::var("ORDER_MAX_AGE_HOURS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map(|hours| hours.saturating_mul(60))
        })
        .unwrap_or(15);
    Duration::from_secs(minutes.max(1) * 60)
}

/// `order_max_age` in whole minutes, for telling customers how long they have to pay.
pub fn order_lifetime_minutes() -> u64 {
    order_max_age().as_secs() / 60
}

/// Per-request socket timeout for the Esplora HTTP client, from `ESPLORA_TIMEOUT_SECS` (default
/// 20). With no timeout set, esplora-client leaves reqwest's request timeout unbounded, so a
/// backend that stops responding mid-connection (rather than cleanly erroring) hangs until the
/// OS-level TCP timeout gives up. Bounding it here makes that fail fast instead, so
/// esplora-client's own built-in exponential-backoff retry gets a chance to recover instead of one
/// hung request stalling the whole sync.
pub fn esplora_timeout() -> Duration {
    let secs = std::env::var("ESPLORA_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(20);
    Duration::from_secs(secs.max(1))
}

/// Smallest ECX deposit an order is allowed to move, from `MIN_DEPOSIT_AMOUNT_SAT` (default 500
/// sat). Below this, either a payout or a refund of it is liable to cost more in tx fees than the
/// amount itself.
pub fn min_deposit_amount() -> Amount {
    let sat = std::env::var("MIN_DEPOSIT_AMOUNT_SAT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(500);
    Amount::from_sat(sat)
}

/// Confirmations required on ECX before a detected deposit counts as `deposit_confirmed` rather
/// than just `deposit_seen`, from `ECX_DEPOSIT_MIN_CONFS` (default 3). Missing or unparsable falls
/// back to the default rather than failing closed/open in a way that's easy to miss. Clamped to at
/// least 1: `0` would count an unconfirmed (and freely double-spendable) transaction as a confirmed
/// deposit and pay out real wECX against it.
pub fn deposit_min_confs() -> u32 {
    std::env::var("ECX_DEPOSIT_MIN_CONFS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(3)
        .max(1)
}

fn env_u64(var: &str, default: u64) -> u64 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
}

/// Largest `amount_in_sat` `POST /orders` accepts, from `MAX_DEPOSIT_AMOUNT_SAT` (default 21M ECX,
/// i.e. the whole supply - a bound against nonsense/overflowing amounts, not a business limit; set
/// it much lower to cap per-order exposure to treasury liquidity).
pub fn max_deposit_amount() -> Amount {
    Amount::from_sat(env_u64("MAX_DEPOSIT_AMOUNT_SAT", 21_000_000 * 100_000_000))
}

/// Global backstop on orders created per rolling minute, both directions combined, from
/// `ORDERS_PER_MINUTE` (default 120, about two per second; `0` disables it). Every `POST /orders`
/// costs a few Solana RPC calls, a persisted wallet address, and a DB row that the poller then
/// re-syncs every tick, and the backend has no auth. This is global rather than per-client because
/// the backend only ever sees the frontend proxy's address, so it cannot tell clients apart. The
/// per-client limits are in front of it: the frontend's per-address limiter and the Cloudflare rate
/// limiting rule (deploy/edge/cloudflare.md), which refuses floods before they reach the tunnel.
///
/// The default is sized so that a burst at the full rate (roughly three RPC calls per order) stays
/// within the public mainnet-beta RPC's per-address allowance; legitimate traffic should never reach
/// it. A tighter value trades protection of the RPC quota for refusing real customers during a spike.
///
/// There is no separate daily creation quota. A shared daily budget is itself a denial of service:
/// filling it locks out every customer, including ones who would have paid. Sustained flooding is
/// bounded by `max_pending_orders` instead: with the short order lifetime, the open set turns over
/// every few poll ticks, so that cap bounds the work of one tick rather than a day's address reveals.
pub fn max_orders_per_minute() -> u64 {
    env_u64("ORDERS_PER_MINUTE", 120)
}

/// How long after an order expires the poller keeps looking for a late deposit, from
/// `ORDER_EXPIRY_GRACE_HOURS` (default 168). Past this window a late deposit is still refused as
/// coin-selection input, but it is no longer revived automatically.
pub fn expiry_grace() -> Duration {
    Duration::from_secs(env_u64("ORDER_EXPIRY_GRACE_HOURS", 168).max(1) * 3600)
}

/// Cap on simultaneously `pending` (deposit-awaited) orders per direction, from
/// `MAX_PENDING_ORDERS` (default 500). This is what bounds the work of one poll tick: every pending
/// wrap costs one Esplora address lookup (batched, in chunks that release the wallet lock between
/// them - see `server::sync_scripts_in_chunks`), and every pending unwrap costs one Solana RPC
/// signature lookup. At the default, a full set is a few hundred small requests per tick, which
/// the public endpoints absorb; with the 15-minute lifetime, legitimate traffic rarely holds more
/// than a few dozen open at once. Raise it when running your own Esplora and RPC; past a few
/// thousand, a tick's unwrap lookups stop fitting inside the poll interval.
pub fn max_pending_orders() -> u64 {
    env_u64("MAX_PENDING_ORDERS", 500)
}

/// Minimum spacing between explorer syncs triggered by `GET /orders/{id}`, from
/// `GET_SYNC_MIN_INTERVAL_SECS` (default 10). A sync holds the global ECX wallet lock for a network
/// round-trip, so without this anyone with an order id could stall the poller and every
/// `POST /orders` just by polling their own order. Requests inside the window read what the last
/// sync (or the background poller) already recorded.
pub fn get_sync_min_interval() -> Duration {
    Duration::from_secs(env_u64("GET_SYNC_MIN_INTERVAL_SECS", 10))
}

/// Lamports the treasury must keep in reserve, from `TREASURY_MIN_SOL_LAMPORTS` (default 20_000_000
/// = 0.02 SOL, roughly ten new recipient token accounts' rent). Below it, new orders are refused
/// and payouts are held rather than attempted: paying ATA rent and fees for the last few payouts
/// only to strand the rest is worse than pausing loudly.
pub fn treasury_min_sol_lamports() -> u64 {
    env_u64("TREASURY_MIN_SOL_LAMPORTS", 20_000_000)
}

/// Whether payouts to off-curve Solana addresses (PDAs - e.g. some multisig vaults) are allowed,
/// from `ALLOW_OFF_CURVE_RECIPIENTS` (default off). Off by default because a mistyped/mangled
/// address that still decodes to 32 bytes is overwhelmingly likely to be off-curve, and wECX sent
/// there is unrecoverable.
pub fn allow_off_curve_recipients() -> bool {
    matches!(std::env::var("ALLOW_OFF_CURVE_RECIPIENTS").as_deref(), Ok("1") | Ok("true"))
}

/// Flat fee taken on every ECX->wECX peg-in conversion, in basis points (1/100 of a percent), from
/// `PEGIN_FEE_BPS` (default 100 = 1%). Unlike the old BTC/ECX swap's AMM curve, this is a flat
/// deduction: there's no pool to feed, since wECX is paid out from a treasury balance rather than
/// priced against one.
pub fn pegin_fee_bps() -> u64 {
    fee_bps("PEGIN_FEE_BPS", 100)
}

/// Fixed ECX amount kept on every wrap, from `PEGIN_FLAT_FEE_SAT` (default 1_000 sat, which is
/// 0.00001000 ECX). This is in addition to `pegin_fee_bps` and does not replace the minimum
/// deposit. It pays for spending the deposit output later, including a dust output a wallet may
/// attach to the same address.
pub fn pegin_flat_fee_sat() -> u64 {
    env_u64("PEGIN_FLAT_FEE_SAT", 1_000)
}

/// Optional wrap-only SOL stipend. A customer who checks the box pays `price_sat` extra ECX and
/// receives `lamports` of SOL in the same transaction as the wrapped tokens. Both values come from
/// the operator. A zero, a one-sided pair, or a stipend below 0.001 SOL disables the offer rather
/// than giving SOL away or sending an amount a brand-new wallet cannot hold.
pub struct SolStipendOffer {
    pub lamports: u64,
    pub price_sat: u64,
    pub usd_label: String,
}

pub fn sol_stipend_offer() -> SolStipendOffer {
    const MIN_LAMPORTS: u64 = 1_000_000;
    let lamports = env_u64("SOL_STIPEND_LAMPORTS", 5_000_000);
    let price_sat = env_u64("SOL_STIPEND_PRICE_SAT", 500_000);
    let usd_label = std::env::var("SOL_STIPEND_USD_LABEL").unwrap_or_else(|_| "0.50".to_string());
    let usd_label = usd_label.trim();
    let usd_label = if usd_label.is_empty()
        || usd_label.len() > 8
        || !usd_label.chars().all(|c| c.is_ascii_digit() || c == '.')
        || usd_label.matches('.').count() > 1
        || usd_label.starts_with('.')
        || usd_label.ends_with('.')
    {
        String::new()
    } else {
        usd_label.to_string()
    };
    if lamports == 0 || price_sat == 0 || lamports < MIN_LAMPORTS {
        return SolStipendOffer { lamports: 0, price_sat: 0, usd_label };
    }
    SolStipendOffer { lamports, price_sat, usd_label }
}

/// Flat fee for the wECX->ECX peg-out direction, in basis points, from `PEGOUT_FEE_BPS` (default
/// 100 = 1%). Values above 10000 are rejected rather than saturating the payout to zero.
pub fn pegout_fee_bps() -> u64 {
    fee_bps("PEGOUT_FEE_BPS", 100)
}

/// Fee kept from a wECX deposit that reached the treasury without any order's reference key, in
/// basis points, from `UNMATCHED_DEPOSIT_FEE_BPS` (default 100 = 1%). The rest is sent back to the
/// wallet it came from - see `server::scan_unmatched_deposits_once`. The fee covers the refund's
/// network cost and discourages using the treasury as a free round trip.
pub fn unmatched_deposit_fee_bps() -> u64 {
    fee_bps("UNMATCHED_DEPOSIT_FEE_BPS", 100)
}

/// Whether reference-less wECX deposits are refunded automatically, from
/// `UNMATCHED_DEPOSIT_REFUNDS` (default on). Off records them for manual review instead; nothing
/// is ever sent in that mode. Detection itself always runs.
pub fn unmatched_deposit_refunds_enabled() -> bool {
    !matches!(std::env::var("UNMATCHED_DEPOSIT_REFUNDS").as_deref(), Ok("0") | Ok("false"))
}

/// Wallets the operator tops the treasury up from, from `TREASURY_FUNDING_ADDRESSES` (comma
/// separated, optional). A reference-less deposit from one of these is liquidity, not a customer
/// mistake, and is kept. The earnings owner address is always treated as one of these.
pub fn treasury_funding_addresses() -> Vec<String> {
    std::env::var("TREASURY_FUNDING_ADDRESSES")
        .map(|value| value.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect())
        .unwrap_or_default()
}

fn fee_bps(var: &str, default: u64) -> u64 {
    let value = std::env::var(var)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default);
    if value > 10_000 {
        panic!("{var}={value} is above 10000 bps; refusing to start with a fee that saturates the payout to zero");
    }
    value
}

/// Whether the server is allowed to automatically broadcast live payouts/refunds: the ECX refund
/// send and the wECX treasury payout on Solana. Both betanet ECX and Solana mainnet-beta are real
/// money (see CLAUDE.md), so - unlike the old swap's BTC-only mainnet gate - this covers every
/// automatic outbound send this backend can make, from `ALLOW_LIVE_PAYOUTS` (default off). This is
/// the unattended-server equivalent of the CLI's `--confirm-live` flag.
pub fn live_payouts_allowed() -> bool {
    matches!(std::env::var("ALLOW_LIVE_PAYOUTS").as_deref(), Ok("1") | Ok("true"))
}

/// Solana RPC endpoint, from `SOLANA_RPC_URL` (default the public mainnet-beta RPC, which rate-
/// limits under real usage - see PLAN.md's open decisions on a production RPC provider).
pub fn solana_rpc_url() -> String {
    std::env::var("SOLANA_RPC_URL")
        .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string())
}

/// Block-explorer page for an ECX transaction, without the trailing `/<txid>`, from
/// `ECX_EXPLORER_TX_URL`. The default is derived from `ECX_ESPLORA_URL`: an Esplora API lives at
/// `<site>/api`, and the matching site shows a transaction at `<site>/tx/<txid>`. An explicit
/// empty value turns the links off; the frontend then shows bare transaction ids.
pub fn ecx_explorer_tx_url() -> String {
    if let Ok(value) = std::env::var("ECX_EXPLORER_TX_URL") {
        return value.trim().trim_end_matches('/').to_string();
    }
    let esplora = std::env::var("ECX_ESPLORA_URL")
        .unwrap_or_else(|_| "https://explorer.beta.ecash.ninja/api".to_string());
    let site = esplora.trim().trim_end_matches('/');
    let site = site.strip_suffix("/api").unwrap_or(site);
    format!("{site}/tx")
}

/// Same for Solana, from `SOLANA_EXPLORER_TX_URL` (default Solscan). The signature is appended.
pub fn solana_explorer_tx_url() -> String {
    std::env::var("SOLANA_EXPLORER_TX_URL")
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .unwrap_or_else(|_| "https://solscan.io/tx".to_string())
}

/// Base58-encoded 64-byte secret key (the standard `solana-keygen` format) for the treasury
/// keypair that custodies wECX and pays its own Solana tx fees/rent. Deliberately separate from
/// `WALLET_MNEMONIC` (see keys.rs): Solana uses ed25519, ECX/BTC use secp256k1, and keeping them
/// separate limits blast radius if one key leaks.
pub fn solana_keypair() -> Result<String> {
    std::env::var("SOLANA_KEYPAIR")
        .map_err(|_| anyhow::anyhow!("SOLANA_KEYPAIR env var not set (base58 64-byte secret key)"))
}

/// The wECX SPL token mint address, from `WECX_MINT_ADDRESS`. This backend never needs mint
/// authority over it (see PLAN.md) - only a treasury token account holding a balance of it.
pub fn wecx_mint() -> Result<solana_sdk::pubkey::Pubkey> {
    let raw = std::env::var("WECX_MINT_ADDRESS")
        .map_err(|_| anyhow::anyhow!("WECX_MINT_ADDRESS env var not set"))?;
    raw.trim()
        .parse()
        .map_err(|e| anyhow::anyhow!("WECX_MINT_ADDRESS is not a valid Solana address: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn earnings_thresholds_are_exact_and_positive() {
        assert_eq!(coin_amount("100", 8).unwrap(), 10_000_000_000);
        assert_eq!(coin_amount("100.00000001", 8).unwrap(), 10_000_000_001);
        for invalid in ["0", "-100", "NaN", "1.000000001", "18446744073709551615"] {
            assert!(coin_amount(invalid, 8).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn earnings_config_accepts_the_example_and_rejects_unknown_keys_and_bad_addresses() {
        // The shipped example is what a developer copies; it must parse, and its sample addresses
        // must be well-formed so a copied file fails only on the operator's own mistakes.
        let text = include_str!("../earnings.toml.example");
        let settings = EarningsConfig::parse(text).unwrap();
        assert!(!settings.enabled, "the example must not enable sweeps");
        assert_eq!(settings.thresholds(8).unwrap(), (10_000_000_000, 10_000_000_000));
        assert!(EarningsConfig::parse(&format!("{text}\nmisspelled_threshold = 1\n")).is_err());
        assert!(EarningsConfig::parse(&text.replace("bc1q4jkcq5jllzmjypjx8vwy0n42vxzl0w4l5zn4tu", "bc1qinvalid")).is_err());
        assert!(EarningsConfig::parse(&text.replace("BgPMuJ6FqLNbtNgHAk6hQBW7Nj9Q1U8udJ7ZzL3iVM7j", "not-a-pubkey")).is_err());
    }

    #[test]
    fn deposit_min_confs_is_clamped_to_at_least_one() {
        unsafe {
            std::env::set_var("ECX_DEPOSIT_MIN_CONFS", "0");
        }
        assert_eq!(deposit_min_confs(), 1);
        unsafe {
            std::env::remove_var("ECX_DEPOSIT_MIN_CONFS");
        }
    }

    #[test]
    fn asset_ticker_defaults_and_rejects_malformed_names() {
        unsafe {
            std::env::remove_var("ASSET_TICKER");
        }
        assert_eq!(asset_ticker(), "bECX");
        assert_eq!(wrapped_ticker(), "wbECX");
        unsafe {
            std::env::set_var("ASSET_TICKER", "ECX");
        }
        assert_eq!(asset_ticker(), "ECX");
        assert_eq!(wrapped_ticker(), "wECX");
        unsafe {
            std::env::set_var("ASSET_TICKER", "aECX");
        }
        assert_eq!(asset_ticker(), "aECX");
        assert_eq!(wrapped_ticker(), "waECX");
        unsafe {
            std::env::set_var("ASSET_TICKER", "1bad");
        }
        assert_eq!(asset_ticker(), "bECX");
        unsafe {
            std::env::remove_var("ASSET_TICKER");
        }
    }

    #[test]
    fn order_lifetime_defaults_to_fifteen_minutes_and_honours_the_old_hours_name() {
        unsafe {
            std::env::remove_var("ORDER_LIFETIME_MINUTES");
            std::env::remove_var("ORDER_MAX_AGE_HOURS");
        }
        assert_eq!(order_lifetime_minutes(), 15);
        unsafe {
            std::env::set_var("ORDER_MAX_AGE_HOURS", "2");
        }
        assert_eq!(order_lifetime_minutes(), 120);
        unsafe {
            std::env::set_var("ORDER_LIFETIME_MINUTES", "30");
        }
        assert_eq!(order_lifetime_minutes(), 30, "the minutes name wins when both are set");
        unsafe {
            std::env::set_var("ORDER_LIFETIME_MINUTES", "0");
        }
        assert_eq!(order_lifetime_minutes(), 1, "clamped to at least one minute");
        unsafe {
            std::env::remove_var("ORDER_LIFETIME_MINUTES");
            std::env::remove_var("ORDER_MAX_AGE_HOURS");
        }
    }

    #[test]
    fn explorer_tx_url_is_derived_from_the_esplora_api_url() {
        unsafe {
            std::env::remove_var("ECX_EXPLORER_TX_URL");
            std::env::set_var("ECX_ESPLORA_URL", "https://explorer.beta.ecash.ninja/api/");
        }
        assert_eq!(ecx_explorer_tx_url(), "https://explorer.beta.ecash.ninja/tx");
        unsafe {
            std::env::set_var("ECX_ESPLORA_URL", "https://esplora.example.org/api");
        }
        assert_eq!(ecx_explorer_tx_url(), "https://esplora.example.org/tx");
        unsafe {
            std::env::set_var("ECX_EXPLORER_TX_URL", "");
        }
        assert_eq!(ecx_explorer_tx_url(), "", "an explicit empty value turns links off");
        unsafe {
            std::env::set_var("ECX_EXPLORER_TX_URL", "https://other.example/tx/");
        }
        assert_eq!(ecx_explorer_tx_url(), "https://other.example/tx");
        unsafe {
            std::env::remove_var("ECX_EXPLORER_TX_URL");
            std::env::remove_var("ECX_ESPLORA_URL");
        }
        assert_eq!(solana_explorer_tx_url(), "https://solscan.io/tx");
    }

    #[test]
    fn live_payouts_allowed_defaults_false() {
        // SAFETY-relevant default: an unset/misconfigured env must never silently enable
        // automatic real-money sends.
        unsafe {
            std::env::remove_var("ALLOW_LIVE_PAYOUTS");
        }
        assert!(!live_payouts_allowed());
    }
}
