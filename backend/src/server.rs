use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use bdk_wallet::bitcoin::{Address, Amount, OutPoint, ScriptBuf};
use serde::{Deserialize, Serialize};
use solana_sdk::signature::{Keypair, Signer};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::time::{interval, MissedTickBehavior};

use crate::config::{self, ChainConfig};
use crate::db::{OrderStatus, PegoutOrderStatus, UnmatchedStatus};
use crate::solana::SolanaWallet;
use crate::wallet::ChainWallet;
use crate::{convert, db, solana};

macro_rules! eprintln_redacted {
    ($($arg:tt)*) => {{
        eprintln!("{}", config::redact_urls(&format!($($arg)*)));
    }};
}

struct AppState {
    ecx: Mutex<ChainWallet>,
    solana: SolanaWallet,
    db: db::Connection,
    limits: Limits,
    stats_cache: Mutex<Option<(Instant, StatsResponse)>>,
    earnings: Option<config::EarningsConfig>,
    money_operations: Mutex<()>,
}

/// In-memory abuse limits for the unauthenticated endpoints (see `config::max_orders_per_minute` /
/// `config::get_sync_min_interval`). std mutexes, never held across an `.await`.
#[derive(Default)]
struct Limits {
    order_creations: std::sync::Mutex<VecDeque<Instant>>,
    last_get_sync: std::sync::Mutex<Option<Instant>>,
}

impl Limits {
    /// Sliding one-minute window over successful admissions. Returns `false` when the window is
    /// full. A configured limit of `0` means no limit (see `config::max_orders_per_minute`); it
    /// must not be read as "admit nothing", which would refuse every customer.
    fn allow_order_creation(&self) -> bool {
        let limit = config::max_orders_per_minute();
        if limit == 0 {
            return true;
        }
        let mut window = self.order_creations.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        while window
            .front()
            .is_some_and(|t| now.duration_since(*t) > std::time::Duration::from_secs(60))
        {
            window.pop_front();
        }
        if window.len() as u64 >= limit {
            return false;
        }
        window.push_back(now);
        true
    }

    /// Whether a `GET /orders/{id}` or `GET /pegout/orders/{id}` may trigger its own upstream
    /// check right now (an ECX explorer sync, or a Solana reference-key lookup) - at most one per
    /// `config::get_sync_min_interval`, across all callers and both directions.
    fn allow_get_sync(&self) -> bool {
        let mut last = self.last_get_sync.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|t| t.elapsed() < config::get_sync_min_interval()) {
            return false;
        }
        *last = Some(Instant::now());
        true
    }
}

fn admit_order_budget(state: &AppState) -> Result<(), (StatusCode, String)> {
    if !state.limits.allow_order_creation() {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            "too many orders are being created right now - please retry shortly".to_string(),
        ));
    }
    Ok(())
}

/// First 8 characters of an order id, for log lines. The full id is effectively the order's only
/// credential (see `db.rs`), so it doesn't belong in logs.
fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

/// Maps an error to a response, used via `.map_err(internal_error)` for anything that isn't a
/// caller mistake (sync/DB/RPC failures) - as opposed to validation errors, which build their own
/// `(StatusCode::BAD_REQUEST, ...)` directly so the client gets a specific message. The detail goes
/// to the log, not the response: this endpoint is public, and RPC/DB error text describes internals
/// (hosts, account state) that a caller has no use for.
fn internal_error(err: anyhow::Error) -> (StatusCode, String) {
    eprintln_redacted!("[error] {err:#}");
    (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string())
}

#[derive(Deserialize)]
struct QuoteQuery {
    /// Omit for just the bridge's static parameters (fee, decimals, minimum) - e.g. to populate a
    /// frontend form before the customer has typed an amount.
    amount_in_sat: Option<u64>,
}

#[derive(Serialize)]
struct QuoteResponse {
    amount_in_sat: Option<u64>,
    /// What `POST /orders` would quote for `amount_in_sat` right now, in wECX base units. Unlike
    /// the old BTC/ECX swap's AMM quote, this involves no network call and can't go stale between
    /// this response and order creation - the bridge is a flat 1:1 peg (see
    /// `convert::ecx_sat_to_wecx_base_units`), not a price that moves with trading activity. It
    /// can still fail at order-creation time if the treasury's actual wECX balance can't cover it
    /// (see `create_order`) - this endpoint doesn't check that, since doing so would cost this
    /// fast, no-network endpoint an RPC round-trip for a number that's only a promise anyway.
    amount_out_base_units: Option<u64>,
    /// Percentage fee taken on every ECX->wECX peg-in order, in basis points (see
    /// `config::pegin_fee_bps`). Already baked into `amount_out_base_units` above.
    pegin_fee_bps: u64,
    /// Fixed ECX amount, in sat, kept on every wrap before the percentage (see
    /// `config::pegin_flat_fee_sat`). Already baked into `amount_out_base_units` above.
    pegin_flat_fee_sat: u64,
    /// Flat fee on the wECX->ECX direction, in basis points (see `config::pegout_fee_bps`).
    /// Reported here so the form can show both fees. The wrap amount above uses `pegin_fee_bps`.
    pegout_fee_bps: u64,
    /// Decimals of the wECX SPL mint, fetched once at startup (see `SolanaWallet::decimals`).
    wecx_decimals: u8,
    /// Smallest `amount_in_sat` that `POST /orders` will accept. This is the configured minimum
    /// raised, when needed, so the deposit still pays out after the flat fee.
    min_amount_in_sat: u64,
    /// Optional wrap-only SOL stipend. Both zero means the checkbox is hidden. The price is extra
    /// ECX on top of the wrap amount; it is not included in `amount_out_base_units`.
    sol_stipend_lamports: u64,
    sol_stipend_price_sat: u64,
    /// Display-only estimate, such as "0.50". Empty when the operator did not set a usable label.
    sol_stipend_usd_label: String,
    /// Minutes a new order waits for its deposit before it expires (see `config::order_max_age`).
    /// A deposit that lands later is still credited; this is the window to show the customer.
    order_lifetime_minutes: u64,
    /// Blocks an ECX deposit needs before it is paid out (see `config::deposit_min_confs`), so the
    /// order page can show "n of m confirmations".
    required_confirmations: u32,
    /// Block explorer the frontend links transaction ids to, without a trailing slash. Empty when
    /// the operator turned the links off.
    ecx_explorer_tx_url: String,
    solana_explorer_tx_url: String,
}

#[derive(Clone, Serialize)]
struct StatsResponse {
    ecx_confirmed_sat: String,
    ecx_unconfirmed_sat: String,
    wecx_balance_base_units: String,
    sol_balance_lamports: String,
    ecx_treasury_address: String,
    solana_treasury_address: String,
    wecx_decimals: u8,
    wraps: u64,
    unwraps: u64,
    wrap_fees_ecx_sat: String,
    unwrap_fees_wecx_base_units: String,
    fees_paid_ecx_sat: String,
    fees_paid_wecx_base_units: String,
    /// Reference-less wECX deposits: waiting for a person, owed back, and already returned.
    unmatched_deposits_held: u64,
    unmatched_refunds_pending: u64,
    unmatched_refunds_completed: u64,
    recent_orders: Vec<RecentOrderResponse>,
    updated_at_unix: u64,
}

#[derive(Clone, Serialize)]
struct RecentOrderResponse {
    direction: String,
    amount_in: String,
    amount_out: String,
    status: String,
    created_at_unix: i64,
}

async fn get_stats(
    State(state): State<Arc<AppState>>,
) -> Result<Json<StatsResponse>, (StatusCode, String)> {
    // Don't hold the cache lock across database and RPC work. A slow upstream used to stall
    // every other /stats caller, including ones that only needed the cached copy.
    {
        let cache = state.stats_cache.lock().await;
        if let Some((refreshed_at, stats)) = cache.as_ref() {
            if refreshed_at.elapsed() < Duration::from_secs(30) {
                return Ok(Json(stats.clone()));
            }
        }
    }

    let decimals = state.solana.decimals();
    let wraps = db::completed_wrap_amounts(&state.db)
        .await
        .map_err(internal_error)?;
    let unwraps = db::completed_unwrap_amounts(&state.db)
        .await
        .map_err(internal_error)?;
    let recent_orders = db::recent_orders(&state.db, 50)
        .await
        .map_err(internal_error)?;
    let (fees_paid_ecx_sat, fees_paid_wecx_base_units) = db::paid_earnings(&state.db)
        .await
        .map_err(internal_error)?;
    let (unmatched_held, unmatched_pending, unmatched_refunded) = db::unmatched_deposit_counts(&state.db)
        .await
        .map_err(internal_error)?;
    let unmatched_fees = db::unmatched_deposit_fees(&state.db).await.map_err(internal_error)?;
    let wrap_restocking = db::wrap_restocking_fees(&state.db).await.map_err(internal_error)?;
    let pegout_restocking = db::pegout_restocking_fees(&state.db).await.map_err(internal_error)?;

    let wrap_fees_sat: u128 = wraps
        .iter()
        .map(|(amount_in_sat, amount_out_base_units)| {
            let amount_out_sat =
                convert::wecx_base_units_to_ecx_sat(*amount_out_base_units, decimals, 0);
            amount_in_sat.saturating_sub(amount_out_sat) as u128
        })
        .chain(wrap_restocking.iter().map(|(fee, _)| *fee as u128))
        .sum();
    let unwrap_fees_base_units: u128 = unwraps
        .iter()
        .map(|(amount_in_base_units, amount_out_sat)| {
            let amount_out_base_units =
                convert::ecx_sat_to_wecx_base_units(*amount_out_sat, decimals, 0);
            amount_in_base_units.saturating_sub(amount_out_base_units) as u128
        })
        .chain(unmatched_fees.iter().map(|(fee, _)| *fee as u128))
        .chain(pegout_restocking.iter().map(|(fee, _)| *fee as u128))
        .sum();

    let mut ecx = state.ecx.lock().await;
    let ecx_treasury_address = ecx.treasury_address();
    if let Err(e) = ecx.sync_addresses([ecx_treasury_address.script_pubkey()]).await {
        eprintln_redacted!("[stats] ecx treasury address sync failed: {e:#}");
    }
    ecx.verify_unspent().await.map_err(internal_error)?;
    let balance = ecx.balance();
    let ecx_unconfirmed = balance.trusted_pending + balance.untrusted_pending;
    let ecx_confirmed_sat = balance.confirmed.to_sat();
    let ecx_unconfirmed_sat = ecx_unconfirmed.to_sat();
    drop(ecx);

    let sol_balance_lamports = state
        .solana
        .sol_balance_lamports()
        .await
        .map_err(internal_error)?;
    let wecx_balance_base_units = state
        .solana
        .treasury_token_balance()
        .await
        .map_err(internal_error)?;

    let stats = StatsResponse {
        ecx_confirmed_sat: ecx_confirmed_sat.to_string(),
        ecx_unconfirmed_sat: ecx_unconfirmed_sat.to_string(),
        wecx_balance_base_units: wecx_balance_base_units.to_string(),
        sol_balance_lamports: sol_balance_lamports.to_string(),
        ecx_treasury_address: ecx_treasury_address.to_string(),
        solana_treasury_address: state.solana.pubkey().to_string(),
        wecx_decimals: decimals,
        wraps: wraps.len() as u64,
        unwraps: unwraps.len() as u64,
        wrap_fees_ecx_sat: wrap_fees_sat.to_string(),
        unwrap_fees_wecx_base_units: unwrap_fees_base_units.to_string(),
        fees_paid_ecx_sat: fees_paid_ecx_sat.to_string(),
        fees_paid_wecx_base_units: fees_paid_wecx_base_units.to_string(),
        unmatched_deposits_held: unmatched_held,
        unmatched_refunds_pending: unmatched_pending,
        unmatched_refunds_completed: unmatched_refunded,
        recent_orders: recent_orders
            .into_iter()
            .map(|order| RecentOrderResponse {
                direction: order.direction,
                amount_in: order.amount_in.to_string(),
                amount_out: order.amount_out.to_string(),
                status: order.status,
                created_at_unix: order.created_at,
            })
            .collect(),
        updated_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    };
    *state.stats_cache.lock().await = Some((Instant::now(), stats.clone()));
    Ok(Json(stats))
}

async fn get_quote(State(state): State<Arc<AppState>>, Query(query): Query<QuoteQuery>) -> Json<QuoteResponse> {
    let pegin_fee_bps = config::pegin_fee_bps();
    let flat_fee_sat = config::pegin_flat_fee_sat();
    let decimals = state.solana.decimals();
    let amount_out_base_units = query.amount_in_sat.map(|sat| {
        convert::pegin_payout_base_units(sat, decimals, pegin_fee_bps, flat_fee_sat)
    });
    let stipend = config::sol_stipend_offer();

    Json(QuoteResponse {
        amount_in_sat: query.amount_in_sat,
        amount_out_base_units,
        pegin_fee_bps,
        pegin_flat_fee_sat: flat_fee_sat,
        pegout_fee_bps: config::pegout_fee_bps(),
        wecx_decimals: decimals,
        min_amount_in_sat: convert::minimum_pegin_deposit_sat(
            config::min_deposit_amount().to_sat(),
            decimals,
            pegin_fee_bps,
            flat_fee_sat,
        ),
        sol_stipend_lamports: stipend.lamports,
        sol_stipend_price_sat: stipend.price_sat,
        sol_stipend_usd_label: stipend.usd_label,
        order_lifetime_minutes: config::order_lifetime_minutes(),
        required_confirmations: config::deposit_min_confs(),
        ecx_explorer_tx_url: config::ecx_explorer_tx_url(),
        solana_explorer_tx_url: config::solana_explorer_tx_url(),
    })
}

#[derive(Deserialize)]
struct CreateOrderRequest {
    amount_in_sat: u64,
    /// Solana address the treasury-custodied wECX gets sent to.
    solana_recipient: String,
    /// Wrap only. The customer pays `config::sol_stipend_offer` extra ECX and receives that SOL
    /// with the wrapped tokens. The amount is not chosen by the client.
    #[serde(default)]
    include_sol_stipend: bool,
}

/// Creates an order: converts `amount_in_sat` to a wECX amount via the bridge's flat 1:1-minus-fee
/// conversion (see `convert::ecx_sat_to_wecx_base_units`), checks the treasury actually has enough
/// uncommitted wECX to cover it, reveals a fresh ECX receive address as the deposit target (fresh,
/// not just-unused, so two concurrently pending orders never share a deposit address and become
/// ambiguous), and persists the row.
async fn create_order(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateOrderRequest>,
) -> Result<(StatusCode, Json<db::Order>), (StatusCode, String)> {
    let _money_operation = state.money_operations.lock().await;
    let flat_fee_sat = config::pegin_flat_fee_sat();
    let min_in_sat = convert::minimum_pegin_deposit_sat(
        config::min_deposit_amount().to_sat(),
        state.solana.decimals(),
        config::pegin_fee_bps(),
        flat_fee_sat,
    );
    if req.amount_in_sat < min_in_sat {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "amount_in_sat is below the minimum deposit amount ({} sat)",
                min_in_sat
            ),
        ));
    }
    if req.amount_in_sat > config::max_deposit_amount().to_sat() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "amount_in_sat is above the maximum order amount ({} sat)",
                config::max_deposit_amount().to_sat()
            ),
        ));
    }

    let recipient = solana::parse_pubkey(&req.solana_recipient)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid solana_recipient: {e}")))?;
    state
        .solana
        .validate_recipient(&recipient)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid solana_recipient: {e}")))?;

    let stipend = if req.include_sol_stipend {
        let offer = config::sol_stipend_offer();
        if offer.lamports == 0 || offer.price_sat == 0 {
            return Err((StatusCode::BAD_REQUEST, "a SOL stipend is not offered".to_string()));
        }
        offer
    } else {
        config::SolStipendOffer { lamports: 0, price_sat: 0, usd_label: String::new() }
    };
    if req.amount_in_sat.saturating_add(stipend.price_sat) > config::max_deposit_amount().to_sat() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "amount_in_sat plus the SOL stipend is above the maximum order amount ({} sat)",
                config::max_deposit_amount().to_sat()
            ),
        ));
    }

    let amount_out = convert::pegin_payout_base_units(
        req.amount_in_sat,
        state.solana.decimals(),
        config::pegin_fee_bps(),
        flat_fee_sat,
    );
    if amount_out == 0 {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "amount_in_sat is too small to bridge any {} after fees/decimal rounding",
                config::wrapped_ticker()
            ),
        ));
    }

    // Cheap in-memory/DB checks first, then the ones that cost an RPC round-trip - and only after
    // the request has already been admitted by the rate limit below, so a flood can't burn RPC quota.
    if let Err(response) = admit_order_budget(&state) {
        return Err(response);
    }
    let pending = db::count_pending_orders(&state.db).await.map_err(internal_error)?;
    if pending >= config::max_pending_orders() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "the bridge has too many open orders right now - please retry later".to_string(),
        ));
    }

    // One RPC check per admitted order, not per quote keystroke. The result is locked on the row
    // and is not repriced if the account appears or disappears during the confirmation window.
    let ata_rent_lamports = match state.solana.missing_recipient_ata_rent(&recipient).await {
        Ok(lamports) => lamports,
        Err(e) => {
            let text = format!("{e:#}");
            if text.contains("not a token account") {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!(
                        "that Solana address cannot receive {}",
                        config::wrapped_ticker()
                    ),
                ));
            }
            eprintln_redacted!("[create_order] recipient token-account check failed: {e:#}");
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "could not check this address's token account - retry".to_string(),
            ));
        }
    };
    let ata_rent_price_sat = convert::sol_lamports_to_ecx_sat(ata_rent_lamports);
    let deposit_sat = req
        .amount_in_sat
        .checked_add(stipend.price_sat)
        .and_then(|total| total.checked_add(ata_rent_price_sat))
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                "amount_in_sat plus the extra charges overflows".to_string(),
            )
        })?;
    if deposit_sat > config::max_deposit_amount().to_sat() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "amount_in_sat plus the extra charges is above the maximum order amount ({} sat)",
                config::max_deposit_amount().to_sat()
            ),
        ));
    }

    let sol_lamports = state.solana.sol_balance_lamports().await.map_err(internal_error)?;
    if sol_lamports < config::treasury_min_sol_lamports() {
        eprintln!(
            "[create_order] treasury SOL balance {sol_lamports} lamports is below \
             TREASURY_MIN_SOL_LAMPORTS - refusing new orders, top the treasury up"
        );
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "the bridge is temporarily unavailable".to_string(),
        ));
    }
    let extra_sol = stipend.lamports.saturating_add(ata_rent_lamports);
    if extra_sol > 0 {
        let reserved = db::reserved_customer_sol_lamports(&state.db)
            .await
            .map_err(internal_error)?;
        let needed = config::treasury_min_sol_lamports()
            .saturating_add(reserved)
            .saturating_add(extra_sol);
        if sol_lamports < needed {
            eprintln!(
                "[create_order] not enough spare SOL: balance {sol_lamports}, reserved {reserved}, \
                 stipend {}, token-account rent {ata_rent_lamports}",
                stipend.lamports
            );
            let message = if ata_rent_lamports > 0 && stipend.lamports == 0 {
                "not enough SOL is available to open a token account for that address right now - \
                 retry later"
            } else {
                "not enough SOL is available for that option right now - leave it unchecked or \
                 retry later"
            };
            return Err((StatusCode::SERVICE_UNAVAILABLE, message.to_string()));
        }
    }

    let treasury_balance = state
        .solana
        .treasury_token_balance()
        .await
        .map_err(internal_error)?;
    let reserved = db::reserved_wecx_amount(&state.db).await.map_err(internal_error)?;
    let available = treasury_balance.saturating_sub(reserved);
    if amount_out > available {
        // Exact figures stay in the log: the treasury's liquidity is not public information.
        eprintln!(
            "[create_order] insufficient wECX liquidity: {amount_out} requested, {available} available"
        );
        return Err((
            StatusCode::CONFLICT,
            format!(
                "not enough {} liquidity available for that amount right now - try a smaller amount \
                 or retry later",
                config::wrapped_ticker()
            ),
        ));
    }

    let deposit_address = state
        .ecx
        .lock()
        .await
        .fresh_address()
        .map_err(internal_error)?;

    let order = db::insert_order(
        &state.db,
        &db::NewOrder {
            amount_in_sat: deposit_sat,
            amount_out_base_units: amount_out,
            deposit_address: deposit_address.to_string(),
            solana_recipient: recipient.to_string(),
            sol_stipend_lamports: stipend.lamports,
            sol_stipend_price_sat: stipend.price_sat,
            ata_rent_lamports,
            ata_rent_price_sat,
        },
    )
    .await
    .map_err(internal_error)?;

    Ok((StatusCode::CREATED, Json(order)))
}

/// Advances an order: detects an awaited deposit, attempts the wECX payout once confirmed, and
/// sends an ECX refund if one's been requested. Called both from `poll_orders_once` and lazily
/// from `GET /orders/{id}`. Each stage is already a no-op when the order isn't in the status it
/// cares about, so chaining them is safe regardless of which state the order is actually in.
async fn check_deposit(state: &AppState, order: db::Order, sync: bool) -> anyhow::Result<db::Order> {
    let _money_operation = state.money_operations.lock().await;
    let order = detect_deposit(state, order, sync).await?;
    let order = try_resolve_payout(state, order).await?;
    let order = try_fulfill(state, order).await?;
    try_refund(state, order).await
}

/// Syncs the order's deposit address and checks whether a deposit has landed, advancing
/// `status`/`deposit_txid` in the DB if so. No-op (no network call) once the order is past the
/// awaiting-deposit states.
///
/// The UTXO is matched by `script_pubkey` alone - a customer can send more or less than the
/// `amount_in_sat` they were quoted for. When the landed value doesn't match, both `amount_in_sat`
/// and `amount_out_base_units` are corrected to reality in the same write: unlike the old BTC/ECX
/// swap this replaces, there's no live market price to drift, so a wrong-amount deposit is simply
/// re-priced by the same deterministic formula the original quote used (see
/// `convert::ecx_sat_to_wecx_base_units`) - no customer decision needed.
///
/// If the deposit is below `config::min_deposit_amount()`, or its repriced amount rounds to zero
/// wECX, it is not paid out. Once confirmed, it is sent back to the address it came from, minus
/// the restocking fee. The customer is told the deposit was formatted wrong.
async fn detect_deposit(state: &AppState, order: db::Order, sync: bool) -> anyhow::Result<db::Order> {
    let status = OrderStatus::from_label(&order.status)?;
    if !matches!(status, OrderStatus::Pending | OrderStatus::DepositSeen) {
        return Ok(order);
    }

    let deposit_script = order
        .deposit_address
        .parse::<Address<_>>()?
        .assume_checked()
        .script_pubkey();

    let mut wallet = state.ecx.lock().await;
    if sync {
        // Scoped to this order's own deposit script, not a full wallet resync - see
        // `ChainWallet::sync_addresses`.
        wallet.sync_addresses([deposit_script.clone()]).await?;
    }

    // `outputs()`, not `utxos()`: matches by script pubkey against everything the wallet has ever
    // seen there, spent or not, so a deposit that lands and is then spent (e.g. picked up by
    // another order's refund coin selection - see `protected_deposit_outpoints`) before this poll
    // gets to it is still recorded.
    let matched: Vec<_> = wallet
        .outputs()
        .into_iter()
        .filter(|u| u.txout.script_pubkey == deposit_script)
        .collect();
    if matched.is_empty() {
        return Ok(order);
    }
    // Every payment to this fresh address belongs to this order. Taking only the first output let
    // a dust payment lock the quote, after which a later real payment was left unprotected.
    let actual_sat = matched
        .iter()
        .try_fold(0u64, |total, output| total.checked_add(output.txout.value.to_sat()))
        .ok_or_else(|| anyhow::anyhow!("deposit value overflow"))?;
    let confs = matched
        .iter()
        .map(|output| wallet.confirmations(output.outpoint.txid))
        .min()
        .unwrap_or(0);
    let deposit_txid = matched
        .iter()
        .max_by_key(|output| (output.txout.value, output.outpoint.txid.to_string()))
        .map(|output| output.outpoint.txid.to_string())
        .ok_or_else(|| anyhow::anyhow!("deposit match disappeared"))?;
    drop(wallet);

    let min_confs = config::deposit_min_confs();
    let new_status = if confs >= min_confs {
        OrderStatus::DepositConfirmed
    } else {
        OrderStatus::DepositSeen
    };
    let actual_amount_in = Amount::from_sat(actual_sat);

    let settled = settle_order_deposit(&order, actual_amount_in.to_sat(), state.solana.decimals());
    // Confirmations are recorded too, so the order page can show progress; past the required count
    // the exact number no longer matters and is not a reason to write.
    let confirmations_changed = confs < min_confs && i64::from(confs) != order.deposit_confirmations;
    if new_status.label() == order.status
        && order.deposit_txid.as_deref() == Some(deposit_txid.as_str())
        && actual_amount_in.to_sat() as i64 == order.amount_in_sat
        && settled.amount_out_base_units as i64 == order.amount_out_base_units
        && !confirmations_changed
    {
        return Ok(order);
    }

    let amount_out = settled.amount_out_base_units;
    let (sol_stipend_lamports, sol_stipend_price_sat) = if new_status == OrderStatus::DepositConfirmed {
        (settled.sol_stipend_lamports, settled.sol_stipend_price_sat)
    } else {
        (
            order.sol_stipend_lamports.max(0) as u64,
            order.sol_stipend_price_sat.max(0) as u64,
        )
    };

    // Conditional write (see `db::update_deposit`): `order` is a snapshot that another task may
    // already have moved past `pending`/`deposit_seen` - possibly all the way to a payout in
    // flight. If we didn't win the write, the snapshot is stale and nothing here is safe to act on;
    // hand back whatever the DB says now.
    if !db::update_deposit(
        &state.db,
        &order.id,
        new_status,
        &deposit_txid,
        actual_amount_in,
        amount_out,
        sol_stipend_lamports,
        sol_stipend_price_sat,
        confs.min(min_confs),
    )
    .await?
    {
        eprintln!(
            "[detect_deposit] order {}: already advanced by another task - re-reading",
            short(&order.id)
        );
        return Ok(db::get_order(&state.db, &order.id).await?.unwrap_or(order));
    }

    let mut updated = db::Order {
        status: new_status.label().to_string(),
        deposit_txid: Some(deposit_txid),
        amount_in_sat: actual_amount_in.to_sat() as i64,
        amount_out_base_units: amount_out as i64,
        sol_stipend_lamports: sol_stipend_lamports as i64,
        sol_stipend_price_sat: sol_stipend_price_sat as i64,
        deposit_confirmations: i64::from(confs.min(min_confs)),
        ..order
    };

    if new_status == OrderStatus::DepositConfirmed
        && (settled.ata_rent_uncovered || !payout_viable(settled.wrap_sat, amount_out))
    {
        let detail = wrap_refund_reason(&settled, amount_out);
        let reason = customer_refund_reason(true);
        eprintln!(
            "[detect_deposit] order {}: {detail} -> requesting refund",
            short(&updated.id)
        );
        if db::request_refund(&state.db, &updated.id, &reason).await? {
            updated.status = OrderStatus::RefundRequested.label().to_string();
            updated.refund_reason = reason;
        }
    }

    Ok(updated)
}

/// Sentence shown to the customer when a deposit cannot be bridged. The first clause is fixed.
/// The percentage is the configured restocking fee (`UNMATCHED_DEPOSIT_FEE_BPS`).
fn customer_refund_reason(sent_back: bool) -> String {
    let percent = config::format_fee_percent(config::unmatched_deposit_fee_bps());
    if sent_back {
        format!(
            "The deposit was formatted wrong. A {percent} restocking fee is kept and the rest is \
             sent back to the address it came from."
        )
    } else {
        format!(
            "The deposit was formatted wrong. After the {percent} restocking fee nothing was left \
             to send back."
        )
    }
}

/// Why a landed wrap deposit is being sent back, for the log. The customer sees
/// `customer_refund_reason` instead.
fn wrap_refund_reason(settled: &convert::SettledWrap, amount_out: u64) -> &'static str {
    if settled.ata_rent_uncovered {
        "the deposit was too small to cover creating the token account for your Solana address"
    } else if amount_out == 0 {
        "after fees the deposit would wrap to nothing"
    } else {
        "the deposit was below the minimum amount"
    }
}

/// Whether a landed deposit is worth paying out: at least the minimum deposit (checked against what
/// actually landed, not what was quoted at order creation - otherwise the minimum is trivially
/// bypassed by quoting big and sending small) and nonzero after fee/decimal rounding. Each payout
/// to a new recipient costs the treasury real SOL in token-account rent, so sub-minimum deposits
/// are a cost attack, not just dust.
fn settle_order_deposit(order: &db::Order, actual_sat: u64, wecx_decimals: u8) -> convert::SettledWrap {
    convert::settle_wrap_deposit(
        actual_sat,
        order.sol_stipend_lamports.max(0) as u64,
        order.sol_stipend_price_sat.max(0) as u64,
        wecx_decimals,
        config::pegin_fee_bps(),
        config::pegin_flat_fee_sat(),
        config::min_deposit_amount().to_sat(),
        order.ata_rent_lamports.max(0) as u64,
        order.ata_rent_price_sat.max(0) as u64,
    )
}

fn payout_viable(amount_in_sat: u64, amount_out_base_units: u64) -> bool {
    amount_out_base_units > 0 && amount_in_sat >= config::min_deposit_amount().to_sat()
}

/// Outpoints currently sitting unspent at any *other* active order's `deposit_address` - passed to
/// `ChainWallet::send`'s `unspendable` list so an automatic ECX send (a peg-in refund, or a
/// peg-out payout - see `try_refund`/`try_fulfill_pegout`) can never have its coin selection grab a
/// customer's deposit that's landed for a different order but hasn't been recorded yet.
/// `exclude_order_id` is the peg-in order this send is *for*, if any - without excluding it, a
/// peg-in refund would find its own already-recorded deposit "protected" from itself, when
/// spending exactly that deposit to fund its own refund is the intended behavior. `None` for a
/// peg-out payout, which isn't tied to any peg-in order's deposit address at all.
pub(crate) async fn protected_deposit_outpoints(
    db_conn: &db::Connection,
    wallet: &ChainWallet,
    exclude_order_id: Option<&str>,
) -> anyhow::Result<Vec<OutPoint>> {
    // Every deposit address that is not a finished wrap or refund, including expired orders.
    // Active-only protection let a late or just-expired deposit be selected as float for someone
    // else's payout. The excluded id is the wrap refund that is supposed to spend its own deposit.
    let mut addresses = db::unsettled_deposit_addresses(db_conn).await?;
    if let Some(id) = exclude_order_id {
        if let Some(order) = db::get_order(db_conn, id).await? {
            addresses.retain(|address| address != &order.deposit_address);
        }
    }
    let protected_scripts: HashSet<_> = addresses
        .iter()
        .filter_map(|address| address.parse::<Address<_>>().ok())
        .map(|a| a.assume_checked().script_pubkey())
        .collect();

    let mut protected: Vec<_> = wallet
        .utxos()
        .into_iter()
        .filter(|u| protected_scripts.contains(&u.txout.script_pubkey))
        .map(|u| u.outpoint)
        .collect();
    for sweep in db::pending_earnings_sweeps(db_conn).await? {
        if sweep.asset == "becx" {
            let transaction = decode_sweep_transaction(&sweep.transaction_hex)?;
            protected.extend(transaction.input.iter().map(|input| input.previous_output));
            protected.extend(transaction.output.iter().enumerate().map(|(index, _)| OutPoint {
                txid: transaction.compute_txid(), vout: index as u32,
            }));
        }
    }
    Ok(protected)
}

/// How long an order must sit in `paying_out` before `try_resolve_payout` may judge its payout
/// attempt lost or stuck. Must comfortably exceed how long a live `SolanaWallet::submit` can run
/// (bounded by the transaction's blockhash lifetime, roughly 1-2 minutes), so the resolver never
/// races an attempt that is still legitimately in flight.
const PAYOUT_RESOLVE_AFTER_SECS: i64 = 180;

/// Pays out a `deposit_confirmed` order's `amount_out_base_units` in wECX to its
/// `solana_recipient`. No-op for orders not currently `deposit_confirmed`.
///
/// Claims the order via `db::claim_for_payout` (a `deposit_confirmed -> paying_out` conditional
/// UPDATE) before sending anything, so two concurrent `GET /orders/{id}` polls can't both pay out
/// the same order - only the poll that wins the claim proceeds.
///
/// The send itself is sign -> persist signature -> broadcast (see `db::record_payout_signature`),
/// and only an outcome that is *known* to mean "not paid" (never broadcast, failed on-chain,
/// blockhash expired unconfirmed) reverts the claim for a retry. An ambiguous outcome leaves the
/// order `paying_out` with its signature on file for `try_resolve_payout` to settle - retrying
/// blindly with a fresh blockhash is how a payout that actually landed gets paid twice.
async fn try_fulfill(state: &AppState, order: db::Order) -> anyhow::Result<db::Order> {
    if OrderStatus::from_label(&order.status)? != OrderStatus::DepositConfirmed {
        return Ok(order);
    }

    if !config::live_payouts_allowed() {
        // Both betanet ECX and Solana mainnet-beta are real money - the operator
        // hasn't opted in, so leave this claimable for the next poll once ALLOW_LIVE_PAYOUTS is
        // set (or the operator pays out manually and marks it done out-of-band).
        eprintln!(
            "[try_fulfill] order {}: payout blocked (ALLOW_LIVE_PAYOUTS not set)",
            short(&order.id)
        );
        return Ok(order);
    }

    let settled = settle_order_deposit(&order, order.amount_in_sat.max(0) as u64, state.solana.decimals());
    if settled.ata_rent_uncovered {
        eprintln!(
            "[try_fulfill] order {}: deposit cannot cover the locked token-account rent - not \
             paying out",
            short(&order.id)
        );
        let detail = wrap_refund_reason(&settled, settled.amount_out_base_units);
        eprintln!("[try_fulfill] order {}: {detail}", short(&order.id));
        let reason = customer_refund_reason(true);
        if db::request_refund(&state.db, &order.id, &reason).await? {
            return Ok(db::Order {
                status: OrderStatus::RefundRequested.label().to_string(),
                refund_reason: reason,
                ..order
            });
        }
        return Ok(db::get_order(&state.db, &order.id).await?.unwrap_or(order));
    }
    if settled.amount_out_base_units != order.amount_out_base_units.max(0) as u64 {
        eprintln!(
            "[try_fulfill] order {}: stored payout does not match the settled deposit - holding",
            short(&order.id)
        );
        return Ok(order);
    }
    if !payout_viable(settled.wrap_sat, settled.amount_out_base_units) {
        eprintln!(
            "[try_fulfill] order {}: deposit is below the minimum or reprices to zero wECX - \
             sending it back",
            short(&order.id)
        );
        let reason = customer_refund_reason(true);
        if db::request_refund(&state.db, &order.id, &reason).await? {
            return Ok(db::Order {
                status: OrderStatus::RefundRequested.label().to_string(),
                refund_reason: reason,
                ..order
            });
        }
        return Ok(db::get_order(&state.db, &order.id).await?.unwrap_or(order));
    }

    let sol_lamports = state.solana.sol_balance_lamports().await?;
    let sol_needed = config::treasury_min_sol_lamports()
        .saturating_add(settled.sol_stipend_lamports)
        .saturating_add(settled.ata_rent_lamports);
    if sol_lamports < sol_needed {
        eprintln!(
            "[try_fulfill] order {}: treasury SOL balance {sol_lamports} lamports is below \
             {sol_needed} (reserve plus stipend and token-account rent) - payout held, top the treasury up",
            short(&order.id)
        );
        return Ok(order);
    }

    let recipient = match solana::parse_pubkey(&order.solana_recipient) {
        Ok(pk) => pk,
        Err(e) => {
            anyhow::bail!(
                "stored solana_recipient {:?} is invalid: {e}",
                order.solana_recipient
            );
        }
    };
    if let Err(e) = state.solana.validate_recipient(&recipient) {
        anyhow::bail!("stored solana_recipient {:?} is not payable: {e}", order.solana_recipient);
    }

    if !db::claim_for_payout(&state.db, &order.id).await? {
        eprintln!("[try_fulfill] order {}: lost payout claim race", short(&order.id));
        return Ok(db::get_order(&state.db, &order.id).await?.unwrap_or(order));
    }

    if settled.sol_stipend_lamports > 0 || settled.ata_rent_lamports > 0 {
        eprintln!(
            "[try_fulfill] order {}: sending wECX payout, {} lamports of token-account rent, \
             {} lamports of SOL stipend",
            short(&order.id),
            settled.ata_rent_lamports,
            settled.sol_stipend_lamports
        );
    } else {
        eprintln!("[try_fulfill] order {}: sending wECX payout", short(&order.id));
    }

    let prepared = match state
        .solana
        .prepare_wecx_transfer(
            recipient,
            order.amount_out_base_units as u64,
            settled.sol_stipend_lamports,
        )
        .await
    {
        Ok(p) => p,
        Err(e) => {
            // Nothing signed, let alone sent.
            eprintln_redacted!("payout could not be prepared for order {}: {e:#}", short(&order.id));
            db::revert_claim(&state.db, &order.id).await?;
            return Ok(db::Order {
                status: OrderStatus::DepositConfirmed.label().to_string(),
                ..order
            });
        }
    };
    let payout_signature = prepared.signature.to_string();

    // Durable *before* broadcast. If this write fails we have not sent anything, so unwinding is safe.
    if let Err(e) = db::record_payout_signature(&state.db, &order.id, &payout_signature).await {
        eprintln_redacted!("could not record payout signature for order {}: {e:#}", short(&order.id));
        db::revert_claim(&state.db, &order.id).await?;
        return Ok(db::Order {
            status: OrderStatus::DepositConfirmed.label().to_string(),
            ..order
        });
    }

    match state.solana.submit(&prepared).await {
        Ok(solana::PayoutOutcome::Confirmed) => {
            if !db::update_payout(&state.db, &order.id, &payout_signature).await? {
                eprintln!(
                    "[try_fulfill] order {}: payout {payout_signature} confirmed but the order \
                     was no longer paying_out - check it manually",
                    short(&order.id)
                );
            }
            Ok(db::Order {
                status: OrderStatus::PaidOut.label().to_string(),
                payout_signature: Some(payout_signature),
                ..order
            })
        }
        Ok(solana::PayoutOutcome::Failed(reason)) => {
            eprintln!(
                "payout for order {} failed on-chain ({payout_signature}): {reason} - will retry",
                short(&order.id)
            );
            db::revert_claim(&state.db, &order.id).await?;
            Ok(db::Order {
                status: OrderStatus::DepositConfirmed.label().to_string(),
                payout_signature: None,
                ..order
            })
        }
        Ok(solana::PayoutOutcome::Expired) => {
            eprintln!(
                "payout for order {} expired unconfirmed ({payout_signature}) - will retry",
                short(&order.id)
            );
            db::revert_claim(&state.db, &order.id).await?;
            Ok(db::Order {
                status: OrderStatus::DepositConfirmed.label().to_string(),
                payout_signature: None,
                ..order
            })
        }
        Err(e) => {
            // Unknown outcome: the transaction may yet land. Leave it paying_out with the signature
            // recorded; `try_resolve_payout` settles it once it can be checked conclusively.
            eprintln_redacted!(
                "payout for order {} has an UNKNOWN outcome ({payout_signature}): {e:#} - left \
                 paying_out for resolution",
                short(&order.id)
            );
            Ok(db::Order {
                payout_signature: Some(payout_signature),
                ..order
            })
        }
    }
}

/// Settles a `paying_out` order whose payout attempt didn't finish cleanly - the process died
/// mid-send, or `try_fulfill` got an ambiguous outcome from the RPC. No-op for other statuses, and
/// for attempts younger than `PAYOUT_RESOLVE_AFTER_SECS` (which may still be running).
///
/// - No signature on file: the signature is always recorded before broadcast, so nothing was ever
///   sent - safe to revert for a retry.
/// - Signature on file: look it up in ledger history. Confirmed -> `paid_out`; failed on-chain ->
///   revert for a retry; still pending -> leave it; not found -> its blockhash lifetime is long past
///   (see `PAYOUT_RESOLVE_AFTER_SECS`) so it can never land - revert for a retry.
async fn try_resolve_payout(state: &AppState, order: db::Order) -> anyhow::Result<db::Order> {
    if OrderStatus::from_label(&order.status)? != OrderStatus::PayingOut {
        return Ok(order);
    }

    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    if now - order.updated_at < PAYOUT_RESOLVE_AFTER_SECS {
        return Ok(order);
    }

    let reverted = |order: db::Order| db::Order {
        status: OrderStatus::DepositConfirmed.label().to_string(),
        payout_signature: None,
        ..order
    };

    let Some(signature_str) = order.payout_signature.clone() else {
        eprintln!(
            "[resolve_payout] order {}: paying_out with no signature on file - nothing was sent, reverting",
            short(&order.id)
        );
        db::revert_claim(&state.db, &order.id).await?;
        return Ok(reverted(order));
    };
    let signature: solana_sdk::signature::Signature = signature_str
        .parse()
        .map_err(|e| anyhow::anyhow!("stored payout_signature {signature_str:?} is invalid: {e}"))?;

    match state.solana.signature_outcome(&signature).await? {
        solana::SignatureOutcome::Confirmed => {
            eprintln!(
                "[resolve_payout] order {}: payout {signature_str} did land - marking paid_out",
                short(&order.id)
            );
            db::update_payout(&state.db, &order.id, &signature_str).await?;
            Ok(db::Order {
                status: OrderStatus::PaidOut.label().to_string(),
                ..order
            })
        }
        solana::SignatureOutcome::Failed(reason) => {
            eprintln!(
                "[resolve_payout] order {}: payout {signature_str} failed on-chain ({reason}) - reverting for retry",
                short(&order.id)
            );
            db::revert_claim(&state.db, &order.id).await?;
            Ok(reverted(order))
        }
        solana::SignatureOutcome::Pending => Ok(order),
        solana::SignatureOutcome::NotFound => {
            eprintln!(
                "[resolve_payout] order {}: payout {signature_str} never landed and can no longer - reverting for retry",
                short(&order.id)
            );
            db::revert_claim(&state.db, &order.id).await?;
            Ok(reverted(order))
        }
    }
}

const WRAP_REFUND_TX: &str = "wrap_refund";
const UNWRAP_PAYOUT_TX: &str = "unwrap_payout";

/// Broadcasts an already-recorded ECX transaction. A failure here does not authorize a replacement
/// transaction: the saved hex is retried until the explorer has this exact transaction.
async fn resubmit_saved_ecx_tx(state: &AppState, tx_hex: &str) -> anyhow::Result<String> {
    let tx = decode_sweep_transaction(tx_hex)?;
    let txid = state.ecx.lock().await.submit_signed_transfer(tx).await?;
    Ok(txid.to_string())
}

/// Sends a `refund_requested` order back to the address the deposit came from, minus the
/// restocking fee. The destination is read from the deposit's inputs; the customer does not
/// choose it. Also finishes a `refunding` order whose signed transaction was saved before
/// broadcast. A lost broadcast response retries that same transaction. It does not build a second
/// one. The refund spends only the deposit outputs. It is not requested until that deposit has
/// the required confirmations, so a later payment to the same address can still be included.
/// A replaced deposit takes the refund with it instead of spending the float.
async fn try_refund(state: &AppState, order: db::Order) -> anyhow::Result<db::Order> {
    let status = OrderStatus::from_label(&order.status)?;
    if !matches!(status, OrderStatus::RefundRequested | OrderStatus::Refunding) {
        return Ok(order);
    }
    if !config::live_payouts_allowed() {
        return Ok(order);
    }

    if let Some(tx_hex) = db::outbound_tx_hex(&state.db, &order.id, WRAP_REFUND_TX).await? {
        let tx = decode_sweep_transaction(&tx_hex)?;
        let refund_txid = resubmit_saved_ecx_tx(state, &tx_hex).await?;
        let fee_sat = state.ecx.lock().await.change_value(&tx);
        db::update_refund(&state.db, &order.id, &refund_txid, fee_sat).await?;
        return Ok(db::get_order(&state.db, &order.id).await?.unwrap_or(db::Order {
            status: OrderStatus::Refunded.label().to_string(),
            refund_txid: Some(refund_txid),
            ..order
        }));
    }
    if status == OrderStatus::Refunding {
        eprintln!(
            "[try_refund] order {}: refunding with no saved transaction - not sending a replacement; needs manual review",
            short(&order.id)
        );
        return Ok(order);
    }

    let destination = match return_address(state, &order).await {
        Ok(address) => address,
        Err(e) => {
            eprintln_redacted!(
                "[try_refund] order {}: cannot yet read the address this deposit came from: {e:#}",
                short(&order.id)
            );
            return Ok(order);
        }
    };
    if order.refund_address.as_deref() != Some(destination.to_string().as_str())
        && !db::set_refund_address(&state.db, &order.id, &destination.to_string()).await?
    {
        return Ok(db::get_order(&state.db, &order.id).await?.unwrap_or(order));
    }

    let amount_in = order.amount_in_sat.max(0) as u64;
    let (_fee, refund_sat) = convert::unmatched_refund_split(amount_in, config::unmatched_deposit_fee_bps());
    if refund_sat == 0 || refund_sat < config::min_deposit_amount().to_sat() {
        let reason = customer_refund_reason(false);
        eprintln!(
            "[try_refund] order {}: refund of {refund_sat} sat is below the minimum - keeping the deposit",
            short(&order.id)
        );
        if db::finish_refund_kept(&state.db, &order.id, amount_in, &reason).await? {
            return Ok(db::Order {
                status: OrderStatus::Refunded.label().to_string(),
                refund_reason: reason,
                refund_address: Some(destination.to_string()),
                ..order
            });
        }
        return Ok(db::get_order(&state.db, &order.id).await?.unwrap_or(order));
    }

    if !db::claim_for_refund(&state.db, &order.id).await? {
        return Ok(db::get_order(&state.db, &order.id).await?.unwrap_or(order));
    }

    let prepared = {
        let mut wallet = state.ecx.lock().await;
        let script = order.deposit_address.parse::<Address<_>>()?.assume_checked().script_pubkey();
        let outpoints: Vec<OutPoint> = wallet
            .utxos()
            .into_iter()
            .filter(|utxo| utxo.txout.script_pubkey == script)
            .map(|utxo| utxo.outpoint)
            .collect();
        wallet.prepare_return_from_deposits(
            &destination,
            &outpoints,
            refund_sat,
            config::ecx_payout_fee_rate(),
        )
    };
    let (tx, _sent) = match prepared {
        Ok(prepared) => prepared,
        Err(e) => {
            eprintln_redacted!("refund could not be prepared for order {}: {e:#}", short(&order.id));
            db::revert_refund_claim(&state.db, &order.id).await?;
            return Ok(db::Order {
                status: OrderStatus::RefundRequested.label().to_string(),
                refund_address: Some(destination.to_string()),
                ..order
            });
        }
    };
    let refund_txid = tx.compute_txid().to_string();
    let tx_hex = bdk_wallet::bitcoin::consensus::encode::serialize_hex(&tx);
    if let Err(e) = db::record_outbound_tx(&state.db, &order.id, WRAP_REFUND_TX, &refund_txid, &tx_hex).await {
        eprintln_redacted!("could not record refund transaction for order {}: {e:#}", short(&order.id));
        db::revert_refund_claim(&state.db, &order.id).await?;
        return Ok(db::Order {
            status: OrderStatus::RefundRequested.label().to_string(),
            ..order
        });
    }
    // The hex is durable. A broadcast error leaves the order refunding so the next poll retries
    // this transaction rather than signing a different one.
    let fee_sat = state.ecx.lock().await.change_value(&tx);
    if let Err(e) = state.ecx.lock().await.submit_signed_transfer(tx).await {
        eprintln_redacted!(
            "refund broadcast for order {} is unresolved ({refund_txid}): {e:#} - will retry the saved transaction",
            short(&order.id)
        );
        return Ok(db::Order {
            status: OrderStatus::Refunding.label().to_string(),
            ..order
        });
    }
    db::update_refund(&state.db, &order.id, &refund_txid, fee_sat).await?;
    Ok(db::Order {
        status: OrderStatus::Refunded.label().to_string(),
        refund_txid: Some(refund_txid),
        refund_address: Some(destination.to_string()),
        ..order
    })
}

/// Address the wrap deposit came from. Uses the stored one once a poll has recorded it, otherwise
/// reads the deposit transaction's inputs and picks the address that paid the most.
async fn return_address(state: &AppState, order: &db::Order) -> anyhow::Result<Address> {
    if let Some(stored) = order.refund_address.as_deref() {
        return Ok(stored.parse::<Address<_>>()?.assume_checked());
    }
    let script = order
        .deposit_address
        .parse::<Address<_>>()?
        .assume_checked()
        .script_pubkey();
    let wallet = state.ecx.lock().await;
    let txids: Vec<_> = wallet
        .outputs()
        .into_iter()
        .filter(|output| output.txout.script_pubkey == script)
        .map(|output| output.outpoint.txid)
        .collect();
    wallet.funding_address(&txids).await
}

// ---------------------------------------------------------------------------------------------
// Peg-out (wECX -> ECX): the mirror of everything above with the two chains swapped. Solana has
// no fresh per-order receive address, so orders are told apart by a reference key (see
// `solana::SolanaWallet::find_reference_deposit`). `db.rs`'s `pegout_orders` section is the state
// machine this drives.
// ---------------------------------------------------------------------------------------------

/// Whether a landed wECX deposit is worth paying out in ECX - mirrors `payout_viable`, but there's
/// only one amount to check here (unlike the ECX side, a peg-out deposit has no separate "at least
/// the minimum" input threshold distinct from its repriced output, since both amounts are the same
/// ECX-denominated question: is this worth an ECX network send).
fn pegout_payout_viable(amount_out_sat: u64) -> bool {
    amount_out_sat >= config::min_deposit_amount().to_sat()
}

/// Advances a peg-out order: detects the awaited wECX deposit, resolves an ambiguous prior refund
/// attempt, attempts the ECX payout once confirmed, and sends a wECX refund if one's been
/// requested. Mirrors `check_deposit`; see that function's doc comment.
async fn check_pegout_deposit(state: &AppState, order: db::PegoutOrder, sync: bool) -> anyhow::Result<db::PegoutOrder> {
    let _money_operation = state.money_operations.lock().await;
    let order = detect_pegout_deposit(state, order, sync).await?;
    let order = try_resolve_pegout_refund(state, order).await?;
    let order = try_fulfill_pegout(state, order).await?;
    try_refund_pegout(state, order).await
}

/// Syncs the order's Solana reference key and checks whether a wECX deposit has landed, advancing
/// `status`/`deposit_signature`/`depositor_owner` in the DB if so. No-op (no network call) once
/// the order is past the awaiting-deposit states, or if `sync` is `false`.
///
/// Mirrors `detect_deposit`'s reprice-on-landed-amount behavior (see
/// `convert::wecx_base_units_to_ecx_sat`), and its dust/below-minimum handling (see
/// `pegout_payout_viable`) - routed to a wECX refund when `depositor_owner` could be determined,
/// otherwise left recorded for manual review. The customer is told the deposit was formatted wrong.
async fn detect_pegout_deposit(state: &AppState, order: db::PegoutOrder, sync: bool) -> anyhow::Result<db::PegoutOrder> {
    let status = PegoutOrderStatus::from_label(&order.status)?;
    if !matches!(status, PegoutOrderStatus::Pending | PegoutOrderStatus::DepositSeen) || !sync {
        return Ok(order);
    }

    if status == PegoutOrderStatus::Pending {
        let reference = solana::parse_pubkey(&order.reference_pubkey)?;
        // Below the quoted amount is not this order's deposit. Binding the first dust transfer
        // would refund that dust, expire the quote, and leave the customer's later payment sitting
        // in the treasury with no order attached.
        let Some(deposit) = state
            .solana
            .find_reference_deposit(&reference, order.amount_in_base_units as u64)
            .await?
        else {
            return Ok(order);
        };

        let amount_out = convert::wecx_base_units_to_ecx_sat(
            deposit.amount_base_units,
            state.solana.decimals(),
            config::pegout_fee_bps(),
        );
        let new_status = if deposit.finalized {
            PegoutOrderStatus::DepositConfirmed
        } else {
            PegoutOrderStatus::DepositSeen
        };
        let deposit_signature = deposit.signature.to_string();
        let depositor_owner = deposit.depositor_owner.map(|p| p.to_string());

        // Conditional write, same reasoning as `update_deposit`: `order` may already have been
        // moved past `pending` by another task.
        if !db::record_pegout_deposit(
            &state.db,
            &order.id,
            new_status,
            &deposit_signature,
            depositor_owner.as_deref(),
            deposit.amount_base_units,
            amount_out,
        )
        .await?
        {
            eprintln!(
                "[detect_pegout_deposit] order {}: already advanced by another task - re-reading",
                short(&order.id)
            );
            return Ok(db::get_pegout_order(&state.db, &order.id).await?.unwrap_or(order));
        }

        let mut updated = db::PegoutOrder {
            status: new_status.label().to_string(),
            deposit_signature: Some(deposit_signature),
            depositor_owner,
            amount_in_base_units: deposit.amount_base_units as i64,
            amount_out_sat: amount_out as i64,
            ..order
        };

        if new_status == PegoutOrderStatus::DepositConfirmed && !pegout_payout_viable(amount_out) {
            eprintln!(
                "[detect_pegout_deposit] order {}: deposit reprices below the minimum ECX payout -> requesting refund",
                short(&updated.id)
            );
            if updated.depositor_owner.is_some() && db::request_pegout_refund(&state.db, &updated.id).await? {
                updated.status = PegoutOrderStatus::RefundRequested.label().to_string();
            }
        }

        return Ok(updated);
    }

    // DepositSeen: the deposit is already on file, just check whether it has since finalized.
    let Some(signature_str) = order.deposit_signature.clone() else {
        anyhow::bail!("pegout order {} is deposit_seen with no deposit_signature on file", order.id);
    };
    let signature: solana_sdk::signature::Signature = signature_str
        .parse()
        .map_err(|e| anyhow::anyhow!("stored deposit_signature {signature_str:?} is invalid: {e}"))?;
    if !state.solana.signature_finalized(&signature).await? {
        return Ok(order);
    }
    if !db::confirm_pegout_deposit(&state.db, &order.id).await? {
        return Ok(db::get_pegout_order(&state.db, &order.id).await?.unwrap_or(order));
    }

    let mut updated = db::PegoutOrder {
        status: PegoutOrderStatus::DepositConfirmed.label().to_string(),
        ..order
    };
    if !pegout_payout_viable(updated.amount_out_sat as u64) {
        eprintln!(
            "[detect_pegout_deposit] order {}: deposit reprices below the minimum ECX payout -> requesting refund",
            short(&updated.id)
        );
        if updated.depositor_owner.is_some() && db::request_pegout_refund(&state.db, &updated.id).await? {
            updated.status = PegoutOrderStatus::RefundRequested.label().to_string();
        }
    }
    Ok(updated)
}

/// Pays out a `deposit_confirmed` peg-out order's `amount_out_sat` in ECX to its `ecx_recipient`.
/// No-op for orders not currently `deposit_confirmed`. Mirrors `try_refund`'s structure (an ECX
/// send is single-phase - sign, broadcast, done - unlike the Solana payout in `try_fulfill`, so
/// there's no ambiguous-outcome case to leave for a resolver here), claiming via
/// `db::claim_for_pegout_payout` first so two concurrent polls can't both send.
async fn try_fulfill_pegout(state: &AppState, order: db::PegoutOrder) -> anyhow::Result<db::PegoutOrder> {
    let status = PegoutOrderStatus::from_label(&order.status)?;
    if !matches!(status, PegoutOrderStatus::DepositConfirmed | PegoutOrderStatus::PayingOut) {
        return Ok(order);
    }

    if let Some(tx_hex) = db::outbound_tx_hex(&state.db, &order.id, UNWRAP_PAYOUT_TX).await? {
        if !config::live_payouts_allowed() {
            return Ok(order);
        }
        let payout_txid = resubmit_saved_ecx_tx(state, &tx_hex).await?;
        if db::update_pegout_payout(&state.db, &order.id, &payout_txid).await? {
            return Ok(db::PegoutOrder {
                status: PegoutOrderStatus::PaidOut.label().to_string(),
                payout_txid: Some(payout_txid),
                ..order
            });
        }
        return Ok(db::get_pegout_order(&state.db, &order.id).await?.unwrap_or(order));
    }
    if status == PegoutOrderStatus::PayingOut {
        eprintln!(
            "[try_fulfill_pegout] order {}: paying_out with no saved transaction - not sending a replacement; needs manual review",
            short(&order.id)
        );
        return Ok(order);
    }

    if !config::live_payouts_allowed() {
        eprintln!(
            "[try_fulfill_pegout] order {}: payout blocked (ALLOW_LIVE_PAYOUTS not set)",
            short(&order.id)
        );
        return Ok(order);
    }

    if !pegout_payout_viable(order.amount_out_sat as u64) {
        eprintln!(
            "[try_fulfill_pegout] order {}: deposit reprices below the minimum ECX payout - not \
             paying out, needs manual review/refund",
            short(&order.id)
        );
        return Ok(order);
    }

    let ecx_recipient = match order.ecx_recipient.parse::<Address<_>>() {
        Ok(addr) => addr.assume_checked(),
        Err(e) => anyhow::bail!("stored ecx_recipient {:?} is invalid: {e}", order.ecx_recipient),
    };

    if !db::claim_for_pegout_payout(&state.db, &order.id).await? {
        eprintln!("[try_fulfill_pegout] order {}: lost payout claim race", short(&order.id));
        return Ok(db::get_pegout_order(&state.db, &order.id).await?.unwrap_or(order));
    }

    eprintln!("[try_fulfill_pegout] order {}: sending ECX payout", short(&order.id));

    let prepared = async {
        let mut wallet = state.ecx.lock().await;
        let already_spent = wallet.verify_unspent().await?;
        let mut protected = protected_deposit_outpoints(&state.db, &wallet, None).await?;
        protected.extend(already_spent);
        wallet.prepare_signed_transfer(
            &ecx_recipient,
            Amount::from_sat(order.amount_out_sat as u64),
            config::ecx_payout_fee_rate(),
            &protected,
        )
    }
    .await;
    let tx = match prepared {
        Ok(tx) => tx,
        Err(e) => {
            eprintln_redacted!("payout could not be prepared for pegout order {}: {e:#}", short(&order.id));
            db::revert_pegout_claim(&state.db, &order.id).await?;
            return Ok(db::PegoutOrder {
                status: PegoutOrderStatus::DepositConfirmed.label().to_string(),
                ..order
            });
        }
    };
    let payout_txid = tx.compute_txid().to_string();
    let tx_hex = bdk_wallet::bitcoin::consensus::encode::serialize_hex(&tx);
    if let Err(e) = db::record_outbound_tx(&state.db, &order.id, UNWRAP_PAYOUT_TX, &payout_txid, &tx_hex).await {
        eprintln_redacted!("could not record pegout payout transaction for order {}: {e:#}", short(&order.id));
        db::revert_pegout_claim(&state.db, &order.id).await?;
        return Ok(db::PegoutOrder {
            status: PegoutOrderStatus::DepositConfirmed.label().to_string(),
            ..order
        });
    }
    if let Err(e) = state.ecx.lock().await.submit_signed_transfer(tx).await {
        eprintln_redacted!(
            "pegout payout broadcast for order {} is unresolved ({payout_txid}): {e:#} - will retry the saved transaction",
            short(&order.id)
        );
        return Ok(db::PegoutOrder {
            status: PegoutOrderStatus::PayingOut.label().to_string(),
            payout_txid: Some(payout_txid),
            ..order
        });
    }
    if !db::update_pegout_payout(&state.db, &order.id, &payout_txid).await? {
        eprintln!(
            "[try_fulfill_pegout] order {}: payout {payout_txid} sent but the order was \
             no longer paying_out - check it manually",
            short(&order.id)
        );
    }
    Ok(db::PegoutOrder {
        status: PegoutOrderStatus::PaidOut.label().to_string(),
        payout_txid: Some(payout_txid),
        ..order
    })
}

/// Sends a `refund_requested` peg-out order back to the wallet the deposit came from, minus the
/// restocking fee. No-op with no `depositor_owner` on file (left for manual review). Structurally
/// mirrors `try_fulfill`: this is a Solana send, so it needs the same two-phase sign -> persist
/// signature -> broadcast handling and the same ambiguous-outcome deferral to
/// `try_resolve_pegout_refund`.
async fn try_refund_pegout(state: &AppState, order: db::PegoutOrder) -> anyhow::Result<db::PegoutOrder> {
    if PegoutOrderStatus::from_label(&order.status)? != PegoutOrderStatus::RefundRequested {
        return Ok(order);
    }

    if !config::live_payouts_allowed() {
        return Ok(order);
    }

    let Some(depositor_owner) = order.depositor_owner.clone() else {
        return Ok(order);
    };
    let recipient = match solana::parse_pubkey(&depositor_owner) {
        Ok(pk) => pk,
        Err(e) => anyhow::bail!("stored depositor_owner {depositor_owner:?} is invalid: {e}"),
    };
    if let Err(e) = state.solana.validate_recipient(&recipient) {
        anyhow::bail!("stored depositor_owner {depositor_owner:?} is not payable: {e}");
    }

    let amount_in = order.amount_in_base_units.max(0) as u64;
    let (fee, refund) = convert::unmatched_refund_split(amount_in, config::unmatched_deposit_fee_bps());
    let min_refund = convert::ecx_sat_to_wecx_base_units(
        config::min_deposit_amount().to_sat(),
        state.solana.decimals(),
        0,
    );
    if refund == 0 || refund < min_refund {
        eprintln!(
            "[try_refund_pegout] order {}: refund of {refund} base units is below the minimum - keeping the deposit",
            short(&order.id)
        );
        if db::finish_pegout_refund_kept(&state.db, &order.id, amount_in).await? {
            return Ok(db::PegoutOrder {
                status: PegoutOrderStatus::Refunded.label().to_string(),
                ..order
            });
        }
        return Ok(db::get_pegout_order(&state.db, &order.id).await?.unwrap_or(order));
    }

    let sol_lamports = state.solana.sol_balance_lamports().await?;
    if sol_lamports < config::treasury_min_sol_lamports() {
        eprintln!(
            "[try_refund_pegout] order {}: treasury SOL balance {sol_lamports} lamports is below \
             TREASURY_MIN_SOL_LAMPORTS - refund held, top the treasury up",
            short(&order.id)
        );
        return Ok(order);
    }

    if !db::claim_for_pegout_refund(&state.db, &order.id).await? {
        return Ok(db::get_pegout_order(&state.db, &order.id).await?.unwrap_or(order));
    }

    eprintln!("[try_refund_pegout] order {}: sending wECX refund", short(&order.id));

    let prepared = match state
        .solana
        .prepare_wecx_transfer(recipient, refund, 0)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            eprintln_redacted!("pegout refund could not be prepared for order {}: {e:#}", short(&order.id));
            db::revert_pegout_refund_claim(&state.db, &order.id).await?;
            return Ok(db::PegoutOrder {
                status: PegoutOrderStatus::RefundRequested.label().to_string(),
                ..order
            });
        }
    };
    let refund_signature = prepared.signature.to_string();

    if let Err(e) = db::record_pegout_refund_signature(&state.db, &order.id, &refund_signature).await {
        eprintln_redacted!("could not record pegout refund signature for order {}: {e:#}", short(&order.id));
        db::revert_pegout_refund_claim(&state.db, &order.id).await?;
        return Ok(db::PegoutOrder {
            status: PegoutOrderStatus::RefundRequested.label().to_string(),
            ..order
        });
    }

    match state.solana.submit(&prepared).await {
        Ok(solana::PayoutOutcome::Confirmed) => {
            if !db::update_pegout_refund(&state.db, &order.id, &refund_signature, fee).await? {
                eprintln!(
                    "[try_refund_pegout] order {}: refund {refund_signature} confirmed but the \
                     order was no longer refunding - check it manually",
                    short(&order.id)
                );
            }
            Ok(db::PegoutOrder {
                status: PegoutOrderStatus::Refunded.label().to_string(),
                refund_signature: Some(refund_signature),
                ..order
            })
        }
        Ok(solana::PayoutOutcome::Failed(reason)) => {
            eprintln!(
                "pegout refund for order {} failed on-chain ({refund_signature}): {reason} - will retry",
                short(&order.id)
            );
            db::revert_pegout_refund_claim(&state.db, &order.id).await?;
            Ok(db::PegoutOrder {
                status: PegoutOrderStatus::RefundRequested.label().to_string(),
                refund_signature: None,
                ..order
            })
        }
        Ok(solana::PayoutOutcome::Expired) => {
            eprintln!(
                "pegout refund for order {} expired unconfirmed ({refund_signature}) - will retry",
                short(&order.id)
            );
            db::revert_pegout_refund_claim(&state.db, &order.id).await?;
            Ok(db::PegoutOrder {
                status: PegoutOrderStatus::RefundRequested.label().to_string(),
                refund_signature: None,
                ..order
            })
        }
        Err(e) => {
            eprintln_redacted!(
                "pegout refund for order {} has an UNKNOWN outcome ({refund_signature}): {e:#} - \
                 left refunding for resolution",
                short(&order.id)
            );
            Ok(db::PegoutOrder {
                refund_signature: Some(refund_signature),
                ..order
            })
        }
    }
}

/// Settles a `refunding` peg-out order whose refund attempt didn't finish cleanly. Mirrors
/// `try_resolve_payout` exactly (same ambiguous-Solana-outcome problem, same
/// `PAYOUT_RESOLVE_AFTER_SECS` grace period), with the refund destination swapped in for the
/// payout recipient.
async fn try_resolve_pegout_refund(state: &AppState, order: db::PegoutOrder) -> anyhow::Result<db::PegoutOrder> {
    if PegoutOrderStatus::from_label(&order.status)? != PegoutOrderStatus::Refunding {
        return Ok(order);
    }

    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    if now - order.updated_at < PAYOUT_RESOLVE_AFTER_SECS {
        return Ok(order);
    }

    let reverted = |order: db::PegoutOrder| db::PegoutOrder {
        status: PegoutOrderStatus::RefundRequested.label().to_string(),
        refund_signature: None,
        ..order
    };

    let Some(signature_str) = order.refund_signature.clone() else {
        eprintln!(
            "[resolve_pegout_refund] order {}: refunding with no signature on file - nothing was \
             sent, reverting",
            short(&order.id)
        );
        db::revert_pegout_refund_claim(&state.db, &order.id).await?;
        return Ok(reverted(order));
    };
    let signature: solana_sdk::signature::Signature = signature_str
        .parse()
        .map_err(|e| anyhow::anyhow!("stored refund_signature {signature_str:?} is invalid: {e}"))?;

    match state.solana.signature_outcome(&signature).await? {
        solana::SignatureOutcome::Confirmed => {
            eprintln!(
                "[resolve_pegout_refund] order {}: refund {signature_str} did land - marking refunded",
                short(&order.id)
            );
            let (fee, _) = convert::unmatched_refund_split(
                order.amount_in_base_units.max(0) as u64,
                config::unmatched_deposit_fee_bps(),
            );
            db::update_pegout_refund(&state.db, &order.id, &signature_str, fee).await?;
            Ok(db::PegoutOrder {
                status: PegoutOrderStatus::Refunded.label().to_string(),
                ..order
            })
        }
        solana::SignatureOutcome::Failed(reason) => {
            eprintln!(
                "[resolve_pegout_refund] order {}: refund {signature_str} failed on-chain ({reason}) - reverting for retry",
                short(&order.id)
            );
            db::revert_pegout_refund_claim(&state.db, &order.id).await?;
            Ok(reverted(order))
        }
        solana::SignatureOutcome::Pending => Ok(order),
        solana::SignatureOutcome::NotFound => {
            eprintln!(
                "[resolve_pegout_refund] order {}: refund {signature_str} never landed and can no longer - reverting for retry",
                short(&order.id)
            );
            db::revert_pegout_refund_claim(&state.db, &order.id).await?;
            Ok(reverted(order))
        }
    }
}

async fn get_order(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<db::Order>, (StatusCode, String)> {
    let Some(order) = db::get_order(&state.db, &id)
        .await
        .map_err(internal_error)?
    else {
        return Err((StatusCode::NOT_FOUND, "order not found".to_string()));
    };

    // Best-effort: a wallet-sync hiccup shouldn't fail the whole lookup, just leave the order as
    // last recorded. `sync` is rate-limited globally (see `Limits::allow_get_sync`): a sync holds
    // the ECX wallet lock over a network call, and this endpoint is unauthenticated.
    let fallback = order.clone();
    let sync = state.limits.allow_get_sync();
    // An expired order the customer is still looking at is the likeliest place for a late deposit
    // to be noticed: `detect_deposit` ignores expired orders, and the poller's revival pass only
    // covers the most recently expired ones.
    let order = if order.status == OrderStatus::Expired.label() && within_expiry_grace(order.updated_at) {
        revive_wrap_if_funded(&state, order, sync).await.unwrap_or(fallback)
    } else {
        check_deposit(&state, order, sync).await.unwrap_or(fallback)
    };

    Ok(Json(order))
}

#[derive(Deserialize)]
struct PegoutQuoteQuery {
    /// Omit for just the bridge's static peg-out parameters - mirrors `QuoteQuery::amount_in_sat`.
    amount_in_base_units: Option<u64>,
}

#[derive(Serialize)]
struct PegoutQuoteResponse {
    amount_in_base_units: Option<u64>,
    amount_out_sat: Option<u64>,
    pegout_fee_bps: u64,
    wecx_decimals: u8,
    /// Smallest ECX payout `POST /pegout/orders` will accept, from `config::min_deposit_amount()`.
    /// The same dust/fee-economics floor the peg-in side uses, since both directions ultimately
    /// move ECX and pay its network fee.
    min_amount_out_sat: u64,
    /// The treasury's Solana address - where the frontend builds a Solana Pay transfer-request URI
    /// to (see `db.rs`'s `pegout_orders` doc comment and `solana::SolanaWallet::treasury_ata`).
    /// Static, so it's served from the quote endpoint rather than repeated on every order.
    treasury_pubkey: String,
    wecx_mint: String,
    /// Same meaning as `QuoteResponse::order_lifetime_minutes`.
    order_lifetime_minutes: u64,
    /// What happens to a transfer that reaches the treasury without this order's reference key:
    /// sent back to the wallet it came from minus this fee, when refunds are on and the sender is
    /// an ordinary wallet. Shown to the customer before they pay.
    unmatched_deposit_fee_bps: u64,
    unmatched_deposit_refunds: bool,
    /// Same meaning as the fields on `QuoteResponse`.
    ecx_explorer_tx_url: String,
    solana_explorer_tx_url: String,
}

async fn get_pegout_quote(
    State(state): State<Arc<AppState>>,
    Query(query): Query<PegoutQuoteQuery>,
) -> Json<PegoutQuoteResponse> {
    let pegout_fee_bps = config::pegout_fee_bps();
    let decimals = state.solana.decimals();
    let amount_out_sat = query
        .amount_in_base_units
        .map(|amt| convert::wecx_base_units_to_ecx_sat(amt, decimals, pegout_fee_bps));

    Json(PegoutQuoteResponse {
        amount_in_base_units: query.amount_in_base_units,
        amount_out_sat,
        pegout_fee_bps,
        wecx_decimals: decimals,
        min_amount_out_sat: config::min_deposit_amount().to_sat(),
        treasury_pubkey: state.solana.pubkey().to_string(),
        wecx_mint: state.solana.mint().to_string(),
        order_lifetime_minutes: config::order_lifetime_minutes(),
        unmatched_deposit_fee_bps: config::unmatched_deposit_fee_bps(),
        unmatched_deposit_refunds: config::unmatched_deposit_refunds_enabled(),
        ecx_explorer_tx_url: config::ecx_explorer_tx_url(),
        solana_explorer_tx_url: config::solana_explorer_tx_url(),
    })
}

#[derive(Deserialize)]
struct CreatePegoutOrderRequest {
    /// wECX the customer intends to send, in base units (already scaled by the mint's decimals).
    amount_in_base_units: u64,
    /// ECX address the payout goes to.
    ecx_recipient: String,
}

/// Creates a peg-out order: converts `amount_in_base_units` to an ECX payout via the bridge's
/// flat 1:1-minus-fee conversion (see `convert::wecx_base_units_to_ecx_sat`), checks the treasury
/// actually has enough uncommitted ECX to cover it, and generates a fresh Solana Pay reference key
/// (see `solana::SolanaWallet::find_reference_deposit`) as the deposit-matching target - fresh, not
/// reused, so two concurrently pending orders are never ambiguous about which deposit is theirs.
/// No keypair is kept for the reference key; it's discarded immediately after taking its pubkey,
/// since it's only ever used as an extra non-signer account, never to sign anything.
async fn create_pegout_order(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreatePegoutOrderRequest>,
) -> Result<(StatusCode, Json<db::PegoutOrder>), (StatusCode, String)> {
    let _money_operation = state.money_operations.lock().await;
    if req.amount_in_base_units == 0 {
        return Err((StatusCode::BAD_REQUEST, "amount_in_base_units must be positive".to_string()));
    }

    let ecx_network = ChainConfig::ecx().network;
    let ecx_recipient: Address = req
        .ecx_recipient
        .trim()
        .parse::<Address<_>>()
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid ecx_recipient: {e}")))?
        .require_network(ecx_network)
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!(
                    "ecx_recipient is not a valid {} address: {e}",
                    config::asset_ticker()
                ),
            )
        })?;

    let amount_out_sat = convert::wecx_base_units_to_ecx_sat(
        req.amount_in_base_units,
        state.solana.decimals(),
        config::pegout_fee_bps(),
    );
    let min_out = config::min_deposit_amount().to_sat();
    if amount_out_sat < min_out {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "amount_in_base_units is too small to bridge at least the minimum {} payout \
                 ({min_out} sat) after fees/decimal rounding",
                config::asset_ticker()
            ),
        ));
    }
    if amount_out_sat > config::max_deposit_amount().to_sat() {
        return Err((
            StatusCode::BAD_REQUEST,
            "amount_in_base_units is above the maximum order amount".to_string(),
        ));
    }

    // Cheap in-memory/DB checks first, then the one that costs touching the ECX wallet - and only
    // after the request has already been admitted by the rate limit below, so a flood can't churn
    // through it. Reuses the same global limiter as peg-in order creation (see
    // `Limits::allow_order_creation`): both directions cost a DB row the poller re-checks every
    // tick, so they share one abuse budget rather than each getting their own.
    if let Err(response) = admit_order_budget(&state) {
        return Err(response);
    }
    let pending = db::count_pending_pegout_orders(&state.db).await.map_err(internal_error)?;
    if pending >= config::max_pending_orders() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "the bridge has too many open orders right now - please retry later".to_string(),
        ));
    }

    let ecx_confirmed_balance = state.ecx.lock().await.balance().confirmed;
    let reserved = db::pegout_reserved_ecx_amount(&state.db).await.map_err(internal_error)?;
    let available = ecx_confirmed_balance.to_sat().saturating_sub(reserved);
    if amount_out_sat > available {
        // Exact figures stay in the log: the treasury's liquidity is not public information.
        eprintln!(
            "[create_pegout_order] insufficient ECX liquidity: {amount_out_sat} requested, {available} available"
        );
        return Err((
            StatusCode::CONFLICT,
            format!(
                "not enough {} liquidity available for that amount right now - try a smaller amount \
                 or retry later",
                config::asset_ticker()
            ),
        ));
    }

    let reference_pubkey = Keypair::new().pubkey();

    let order = db::insert_pegout_order(
        &state.db,
        &db::NewPegoutOrder {
            amount_in_base_units: req.amount_in_base_units,
            amount_out_sat,
            ecx_recipient: ecx_recipient.to_string(),
            reference_pubkey: reference_pubkey.to_string(),
        },
    )
    .await
    .map_err(internal_error)?;

    Ok((StatusCode::CREATED, Json(order)))
}

async fn get_pegout_order(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<db::PegoutOrder>, (StatusCode, String)> {
    let Some(order) = db::get_pegout_order(&state.db, &id)
        .await
        .map_err(internal_error)?
    else {
        return Err((StatusCode::NOT_FOUND, "order not found".to_string()));
    };

    // Best-effort, rate-limited the same way `get_order` rate-limits its ECX sync - see
    // `Limits::allow_get_sync`. Expired orders take the revival path for the same reason as there.
    let fallback = order.clone();
    let sync = state.limits.allow_get_sync();
    let order = if order.status == PegoutOrderStatus::Expired.label() && within_expiry_grace(order.updated_at) {
        revive_pegout_if_funded(&state, order, sync).await.unwrap_or(fallback)
    } else {
        check_pegout_deposit(&state, order, sync).await.unwrap_or(fallback)
    };

    Ok(Json(order))
}

/// Background loop: every `config::order_poll_interval()`, re-runs `check_deposit` (deposit
/// detection + payout + refund) on every not-yet-settled order, so orders advance even if nobody
/// ever polls `GET /orders/{id}` again. Orders still awaiting a deposit past `config::order_max_age()`
/// are expired instead of checked - see `poll_orders_once`.
async fn run_order_poller(state: Arc<AppState>) {
    let mut ticker = interval(config::order_poll_interval());
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        if let Err(error) = sweep_earnings_once(&state).await {
            eprintln_redacted!("[earnings] sweep check failed: {error:#}");
        }
        if let Err(e) = poll_orders_once(&state).await {
            eprintln_redacted!("order poll failed: {e:#}");
        }
        if let Err(e) = poll_pegout_orders_once(&state).await {
            eprintln_redacted!("pegout order poll failed: {e:#}");
        }
        // After the peg-out pass, so a deposit that does carry a reference has been attributed to
        // its order before the treasury-wide scan looks at the same transaction.
        if let Err(e) = scan_unmatched_deposits_once(&state).await {
            eprintln_redacted!("[unmatched] scan failed: {e:#}");
        }
        if let Err(e) = refund_unmatched_deposits_once(&state).await {
            eprintln_redacted!("[unmatched] refund pass failed: {e:#}");
        }
    }
}

const UNMATCHED_SCAN_CURSOR: &str = "treasury_deposits";
/// Transactions examined per tick. Each costs one RPC fetch; the rest wait for the next tick.
const UNMATCHED_SCAN_PER_TICK: usize = 25;
const UNMATCHED_REFUNDS_PER_TICK: u64 = 10;

/// What to do with a wECX deposit that no order can claim. Pure, so the rules are testable.
#[derive(Debug, PartialEq, Eq)]
struct UnmatchedDecision {
    status: UnmatchedStatus,
    fee_base_units: u64,
    refund_base_units: u64,
    note: &'static str,
}

struct UnmatchedFacts<'a> {
    amount_base_units: u64,
    depositor_owner: Option<&'a str>,
    /// `depositor_owner` is a wallet that can be paid (on-curve, not a program, mint, or the
    /// treasury) - the result of `SolanaWallet::validate_recipient`.
    depositor_payable: bool,
    depositor_is_funding_wallet: bool,
    live_payouts: bool,
    refunds_enabled: bool,
    fee_bps: u64,
    /// Smallest refund worth sending, in wECX base units.
    min_refund_base_units: u64,
}

fn decide_unmatched(facts: &UnmatchedFacts<'_>) -> UnmatchedDecision {
    let held = |note| UnmatchedDecision {
        status: UnmatchedStatus::Held,
        fee_base_units: 0,
        refund_base_units: 0,
        note,
    };
    if facts.depositor_is_funding_wallet {
        // The operator adding liquidity. Not a fee, not a refund.
        return UnmatchedDecision {
            status: UnmatchedStatus::Kept,
            fee_base_units: 0,
            refund_base_units: 0,
            note: "treasury funding from an operator wallet",
        };
    }
    if facts.depositor_owner.is_none() {
        return held("the sending wallet could not be determined from the transaction");
    }
    if !facts.depositor_payable {
        return held("the sending account is a program or the bridge itself, not a wallet a refund can go to");
    }
    if !facts.live_payouts {
        // Before `enable`, nothing public exists, so this is almost certainly the operator's own
        // funding from a wallet other than the owner address. During a later pause it may be a
        // customer. Either way a person decides; nothing is sent while payouts are off.
        return held("received while payouts were off; review, then refund by hand if it is a customer's");
    }
    if !facts.refunds_enabled {
        return held("automatic refunds are disabled (UNMATCHED_DEPOSIT_REFUNDS)");
    }
    let (fee, refund) = convert::unmatched_refund_split(facts.amount_base_units, facts.fee_bps);
    if refund == 0 || refund < facts.min_refund_base_units {
        return UnmatchedDecision {
            status: UnmatchedStatus::Kept,
            fee_base_units: facts.amount_base_units,
            refund_base_units: 0,
            note: "below the minimum refund; kept as the fee",
        };
    }
    UnmatchedDecision {
        status: UnmatchedStatus::Pending,
        fee_base_units: fee,
        refund_base_units: refund,
        note: "no order reference in the transaction",
    }
}

/// Walks the treasury token account's history forward from where the last tick stopped and
/// records every finalized wECX deposit that no order can claim - a transfer made by pasting the
/// treasury address into a wallet instead of opening the order's Solana Pay link, so it carries no
/// reference key. Left alone, such a deposit quietly becomes float and is paid out to wrap
/// customers while its sender waits for an order that will never advance.
///
/// Only finalized transactions are examined, and the cursor only ever points at one, so a dropped
/// fork can never leave it pointing at a signature the RPC no longer knows. On the first run the
/// cursor is set to the newest finalized signature and nothing older is examined: the history
/// before this feature existed may contain deposits the operator already dealt with by hand.
async fn scan_unmatched_deposits_once(state: &AppState) -> anyhow::Result<()> {
    let _money_operation = state.money_operations.lock().await;
    let cursor = db::scan_cursor(&state.db, UNMATCHED_SCAN_CURSOR).await?;
    let cursor_signature = cursor
        .as_deref()
        .map(|value| value.parse::<solana_sdk::signature::Signature>())
        .transpose()
        .map_err(|e| anyhow::anyhow!("stored treasury scan cursor is invalid: {e}"))?;

    let listing = state.solana.list_treasury_signatures(cursor_signature.as_ref()).await?;
    if cursor.is_none() {
        if let Some(newest) = listing.entries.iter().rev().find(|entry| entry.finalized) {
            db::set_scan_cursor(&state.db, UNMATCHED_SCAN_CURSOR, &newest.signature.to_string()).await?;
            eprintln!(
                "[unmatched] treasury deposit scanning starts after {}; earlier deposits are not examined",
                newest.signature
            );
        }
        return Ok(());
    }
    if listing.entries.is_empty() {
        return Ok(());
    }
    if !listing.complete {
        eprintln!(
            "[unmatched] the treasury had more transactions since the last scan than one pass reads; \
             transactions older than {} were skipped and need a manual look",
            listing.entries[0].signature
        );
    }

    let references = db::pegout_reference_pubkeys(&state.db).await?;
    let mut funding: HashSet<String> = config::treasury_funding_addresses().into_iter().collect();
    if let Some(settings) = state.earnings.as_ref() {
        funding.insert(settings.solana_owner_address.clone());
    }
    let live_payouts = config::live_payouts_allowed();
    let refunds_enabled = config::unmatched_deposit_refunds_enabled();
    let fee_bps = config::unmatched_deposit_fee_bps();
    let min_refund_base_units = convert::ecx_sat_to_wecx_base_units(
        config::min_deposit_amount().to_sat(),
        state.solana.decimals(),
        0,
    );

    for (examined, entry) in listing.entries.iter().enumerate() {
        if !entry.finalized || examined >= UNMATCHED_SCAN_PER_TICK {
            // Everything from here on is newer. It waits for the next tick, and the cursor stays
            // on the last finalized transaction that was examined.
            break;
        }
        let signature = entry.signature.to_string();
        // A failed transaction moved nothing, so there is nothing to fetch.
        let deposit = if entry.failed { None } else { state.solana.fetch_treasury_deposit(&entry.signature, true).await? };
        if let Some(deposit) = deposit {
            let belongs_to_an_order = db::is_pegout_deposit_signature(&state.db, &signature).await?
                || deposit.account_keys.iter().any(|key| references.contains(key));
            if !belongs_to_an_order {
                let owner = deposit.depositor_owner.map(|pk| pk.to_string());
                let facts = UnmatchedFacts {
                    amount_base_units: deposit.amount_base_units,
                    depositor_owner: owner.as_deref(),
                    depositor_payable: deposit
                        .depositor_owner
                        .is_some_and(|pk| state.solana.validate_recipient(&pk).is_ok()),
                    depositor_is_funding_wallet: owner.as_deref().is_some_and(|o| funding.contains(o)),
                    live_payouts,
                    refunds_enabled,
                    fee_bps,
                    min_refund_base_units,
                };
                let decision = decide_unmatched(&facts);
                let inserted = db::insert_unmatched_deposit(
                    &state.db,
                    &db::NewUnmatchedDeposit {
                        signature: &signature,
                        amount_base_units: deposit.amount_base_units,
                        depositor_owner: owner.as_deref(),
                        fee_base_units: decision.fee_base_units,
                        refund_base_units: decision.refund_base_units,
                        status: decision.status,
                        note: decision.note,
                    },
                )
                .await?;
                if inserted {
                    eprintln!(
                        "[unmatched] deposit {signature}: {} base units from {} -> {} ({})",
                        deposit.amount_base_units,
                        owner.as_deref().unwrap_or("unknown"),
                        decision.status.label(),
                        decision.note
                    );
                }
            }
        }
        db::set_scan_cursor(&state.db, UNMATCHED_SCAN_CURSOR, &signature).await?;
    }
    Ok(())
}

/// Sends the refunds recorded by `scan_unmatched_deposits_once`, and settles attempts that did not
/// finish cleanly. Same two-phase pattern as `try_refund_pegout`: claim, sign, persist the
/// signature, broadcast, and leave an ambiguous outcome `refunding` for the next pass to look up
/// rather than retrying blind.
async fn refund_unmatched_deposits_once(state: &AppState) -> anyhow::Result<()> {
    let _money_operation = state.money_operations.lock().await;

    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    for deposit in db::list_unmatched_deposits(&state.db, &[UnmatchedStatus::Refunding], 50).await? {
        if now - deposit.updated_at < PAYOUT_RESOLVE_AFTER_SECS {
            continue;
        }
        let Some(refund_signature) = deposit.refund_signature.as_deref() else {
            eprintln!("[unmatched] deposit {}: refunding with no signature on file - nothing was sent, reverting", deposit.signature);
            db::revert_unmatched_refund_claim(&state.db, &deposit.signature).await?;
            continue;
        };
        let parsed: solana_sdk::signature::Signature = refund_signature
            .parse()
            .map_err(|e| anyhow::anyhow!("stored refund signature {refund_signature:?} is invalid: {e}"))?;
        match state.solana.signature_outcome(&parsed).await? {
            solana::SignatureOutcome::Confirmed => {
                eprintln!("[unmatched] deposit {}: refund {refund_signature} did land - marking refunded", deposit.signature);
                db::finish_unmatched_refund(&state.db, &deposit.signature, refund_signature).await?;
            }
            solana::SignatureOutcome::Failed(reason) => {
                eprintln!("[unmatched] deposit {}: refund {refund_signature} failed on-chain ({reason}) - reverting for retry", deposit.signature);
                db::revert_unmatched_refund_claim(&state.db, &deposit.signature).await?;
            }
            solana::SignatureOutcome::NotFound => {
                eprintln!("[unmatched] deposit {}: refund {refund_signature} never landed and no longer can - reverting for retry", deposit.signature);
                db::revert_unmatched_refund_claim(&state.db, &deposit.signature).await?;
            }
            solana::SignatureOutcome::Pending => {}
        }
    }

    if !config::live_payouts_allowed() || !config::unmatched_deposit_refunds_enabled() {
        return Ok(());
    }
    let pending = db::list_unmatched_deposits(&state.db, &[UnmatchedStatus::Pending], UNMATCHED_REFUNDS_PER_TICK).await?;
    if pending.is_empty() {
        return Ok(());
    }
    let sol_lamports = state.solana.sol_balance_lamports().await?;
    if sol_lamports < config::treasury_min_sol_lamports() {
        eprintln!(
            "[unmatched] treasury SOL balance {sol_lamports} lamports is below TREASURY_MIN_SOL_LAMPORTS - \
             {} refund(s) held, top the treasury up",
            pending.len()
        );
        return Ok(());
    }

    for deposit in pending {
        let signature = deposit.signature.clone();
        let Some(owner) = deposit.depositor_owner.as_deref() else {
            db::hold_unmatched_deposit(&state.db, &signature, "the sending wallet is not on file").await?;
            continue;
        };
        let recipient = match solana::parse_pubkey(owner).and_then(|pk| state.solana.validate_recipient(&pk).map(|()| pk)) {
            Ok(pk) => pk,
            Err(e) => {
                eprintln_redacted!("[unmatched] deposit {signature}: sender {owner} cannot be refunded ({e}) - holding");
                db::hold_unmatched_deposit(&state.db, &signature, "the sending wallet cannot receive a refund").await?;
                continue;
            }
        };
        if !db::claim_unmatched_refund(&state.db, &signature).await? {
            continue;
        }
        eprintln!(
            "[unmatched] deposit {signature}: refunding {} base units to {owner} (fee {} kept)",
            deposit.refund_base_units, deposit.fee_base_units
        );
        let prepared = match state.solana.prepare_wecx_transfer(recipient, deposit.refund_base_units, 0).await {
            Ok(p) => p,
            Err(e) => {
                eprintln_redacted!("[unmatched] deposit {signature}: refund could not be prepared: {e:#}");
                db::revert_unmatched_refund_claim(&state.db, &signature).await?;
                continue;
            }
        };
        let refund_signature = prepared.signature.to_string();
        if let Err(e) = db::record_unmatched_refund_signature(&state.db, &signature, &refund_signature).await {
            eprintln_redacted!("[unmatched] deposit {signature}: could not record refund signature: {e:#}");
            db::revert_unmatched_refund_claim(&state.db, &signature).await?;
            continue;
        }
        match state.solana.submit(&prepared).await {
            Ok(solana::PayoutOutcome::Confirmed) => {
                db::finish_unmatched_refund(&state.db, &signature, &refund_signature).await?;
                eprintln!("[unmatched] deposit {signature}: refunded, {refund_signature}");
            }
            Ok(solana::PayoutOutcome::Failed(reason)) => {
                eprintln!("[unmatched] deposit {signature}: refund {refund_signature} failed on-chain ({reason}) - will retry");
                db::revert_unmatched_refund_claim(&state.db, &signature).await?;
            }
            Ok(solana::PayoutOutcome::Expired) => {
                eprintln!("[unmatched] deposit {signature}: refund {refund_signature} expired unconfirmed - will retry");
                db::revert_unmatched_refund_claim(&state.db, &signature).await?;
            }
            Err(e) => {
                eprintln_redacted!("[unmatched] deposit {signature}: refund {refund_signature} has an UNKNOWN outcome: {e:#} - left refunding for resolution");
            }
        }
    }
    Ok(())
}

fn decode_sweep_transaction(hex: &str) -> anyhow::Result<bdk_wallet::bitcoin::Transaction> {
    use bdk_wallet::bitcoin::hex::FromHex;
    let bytes = Vec::<u8>::from_hex(hex)?;
    Ok(bdk_wallet::bitcoin::consensus::deserialize(&bytes)?)
}

fn unswept_fees(amounts: &[(u64, u64)], allocated: u64) -> anyhow::Result<u64> {
    let mut total = 0u64;
    for (amount_in, amount_out) in amounts {
        let fee = amount_in.checked_sub(*amount_out)
            .ok_or_else(|| anyhow::anyhow!("stored payout exceeds deposit"))?;
        total = total.checked_add(fee).ok_or_else(|| anyhow::anyhow!("earnings overflow"))?;
    }
    total.checked_sub(allocated).ok_or_else(|| anyhow::anyhow!("allocated earnings exceed recorded fees"))
}

async fn sweep_earnings_once(state: &AppState) -> anyhow::Result<()> {
    let _money_operation = state.money_operations.lock().await;
    if !config::live_payouts_allowed() {
        return Ok(());
    }
    for sweep in db::pending_earnings_sweeps(&state.db).await? {
        if sweep.asset == "becx" {
            let transaction = decode_sweep_transaction(&sweep.transaction_hex)?;
            anyhow::ensure!(transaction.compute_txid().to_string() == sweep.transaction_id, "sweep transaction ID mismatch");
            state.ecx.lock().await.submit_earnings_transfer(transaction).await?;
            db::finish_earnings_sweep(&state.db, &sweep.id, true).await?;
            eprintln!("[earnings] recovered bECX sweep {}", sweep.transaction_id);
        } else {
            let signature = sweep.transaction_id.parse()?;
            match state.solana.signature_outcome(&signature).await? {
                solana::SignatureOutcome::Confirmed => {
                    db::finish_earnings_sweep(&state.db, &sweep.id, true).await?;
                }
                solana::SignatureOutcome::Failed(_) => {
                    db::finish_earnings_sweep(&state.db, &sweep.id, false).await?;
                }
                _ => eprintln!("[earnings] unresolved wbECX sweep {}; retained for review, no replacement sent", sweep.transaction_id),
            }
        }
    }
    let Some(settings) = state.earnings.as_ref().filter(|settings| settings.enabled) else {
        return Ok(());
    };
    let (wecx_threshold, ecx_threshold) = settings.thresholds(state.solana.decimals())?;
    // Fees kept from reference-less deposits are unwrap-side earnings, recorded as (fee, 0).
    let mut wrap_amounts = db::completed_wrap_amounts(&state.db).await?;
    wrap_amounts.extend(db::wrap_restocking_fees(&state.db).await?);
    let mut unwrap_amounts = db::completed_unwrap_amounts(&state.db).await?;
    unwrap_amounts.extend(db::unmatched_deposit_fees(&state.db).await?);
    unwrap_amounts.extend(db::pegout_restocking_fees(&state.db).await?);
    for (asset, threshold, amounts) in [
        ("becx", ecx_threshold, wrap_amounts),
        ("wbecx", wecx_threshold, unwrap_amounts),
    ] {
        if db::pending_earnings_sweeps(&state.db).await?.iter().any(|sweep| sweep.asset == asset) {
            continue;
        }
        let amount = unswept_fees(&amounts, db::allocated_earnings(&state.db, asset).await?)?;
        if amount <= threshold {
            continue;
        }
        let mut sweep = db::EarningsSweep {
            id: uuid::Uuid::new_v4().to_string(), asset: asset.into(), amount,
            destination: String::new(), transaction_id: String::new(), transaction_hex: String::new(),
        };
        if asset == "becx" {
            let mut wallet = state.ecx.lock().await;
            let mut protected = wallet.verify_unspent().await?;
            protected.extend(protected_deposit_outpoints(&state.db, &wallet, None).await?);
            let unsettled_scripts: HashSet<_> = db::unsettled_deposit_addresses(&state.db).await?
                .iter().map(|address| address.parse::<Address<_>>())
                .collect::<Result<Vec<_>, _>>()?.into_iter()
                .map(|address| address.assume_checked().script_pubkey()).collect();
            protected.extend(wallet.utxos().iter()
                .filter(|output| unsettled_scripts.contains(&output.txout.script_pubkey))
                .map(|output| output.outpoint));
            protected.extend(wallet.utxos().iter()
                .filter(|output| wallet.confirmations(output.outpoint.txid) == 0)
                .map(|output| output.outpoint));
            let reserved = db::customer_earnings_reserve(&state.db, "becx").await?;
            let available = wallet.balance().confirmed.to_sat().saturating_sub(reserved);
            if amount >= available {
                eprintln!("[earnings] insufficient confirmed bECX for {amount} base units plus network fee");
                continue;
            }
            let destination = settings.ecx_owner_address.parse::<Address<_>>()?
                .require_network(ChainConfig::ecx().network)?;
            let utxos = wallet.utxos();
            let transaction = wallet.prepare_earnings_transfer(
                &destination, Amount::from_sat(amount), config::ecx_payout_fee_rate(), &protected,
            )?;
            let input_total: u64 = transaction.input.iter().map(|input| {
                utxos.iter().find(|output| output.outpoint == input.previous_output)
                    .map(|output| output.txout.value.to_sat()).unwrap_or(0)
            }).sum();
            let output_total: u64 = transaction.output.iter().map(|output| output.value.to_sat()).sum();
            let network_fee = input_total.checked_sub(output_total)
                .ok_or_else(|| anyhow::anyhow!("invalid sweep network fee"))?;
            anyhow::ensure!(amount.checked_add(network_fee).is_some_and(|cost| cost <= available), "sweep would consume customer reserves");
            sweep.destination = destination.to_string();
            sweep.transaction_id = transaction.compute_txid().to_string();
            sweep.transaction_hex = bdk_wallet::bitcoin::consensus::encode::serialize_hex(&transaction);
            db::record_earnings_sweep(&state.db, &sweep).await?;
            wallet.submit_earnings_transfer(transaction).await?;
            db::finish_earnings_sweep(&state.db, &sweep.id, true).await?;
        } else {
            let available = state.solana.treasury_token_balance().await?
                .saturating_sub(db::customer_earnings_reserve(&state.db, "wbecx").await?);
            if amount > available || state.solana.sol_balance_lamports().await? < config::treasury_min_sol_lamports() {
                eprintln!("[earnings] insufficient wbECX or SOL for earnings sweep");
                continue;
            }
            let destination = solana::parse_pubkey(&settings.solana_owner_address)?;
            state.solana.validate_recipient(&destination)?;
            let prepared = state.solana.prepare_wecx_transfer(destination, amount, 0).await?;
            sweep.destination = destination.to_string();
            sweep.transaction_id = prepared.signature.to_string();
            db::record_earnings_sweep(&state.db, &sweep).await?;
            match state.solana.submit(&prepared).await? {
                solana::PayoutOutcome::Confirmed => db::finish_earnings_sweep(&state.db, &sweep.id, true).await?,
                solana::PayoutOutcome::Failed(_) | solana::PayoutOutcome::Expired => {
                    db::finish_earnings_sweep(&state.db, &sweep.id, false).await?;
                    continue;
                }
            }
        }
        eprintln!("[earnings] swept {} {} base units to {}: {}", amount, asset, sweep.destination, sweep.transaction_id);
    }
    Ok(())
}

/// Peg-out mirror of `poll_orders_once`. No ECX-style batched address pre-sync here: each active
/// order's Solana reference key is its own independent RPC lookup (see
/// `solana::SolanaWallet::find_reference_deposit`), so `check_pegout_deposit` is just called
/// directly with `sync: true` for each one.
async fn poll_pegout_orders_once(state: &AppState) -> anyhow::Result<()> {
    let max_age_secs = config::order_max_age().as_secs();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let active = db::list_active_pegout_orders(&state.db).await?;
    eprintln!("[poll] tick: {} active pegout order(s)", active.len());

    for order in active {
        let id = order.id.clone();
        let status = PegoutOrderStatus::from_label(&order.status)?;
        let age_secs = (now - order.created_at).max(0) as u64;
        eprintln!("[poll] pegout order {}: status={status:?} age={age_secs}s", short(&id));

        if status == PegoutOrderStatus::Pending && age_secs > max_age_secs {
            match check_pegout_deposit(state, order, true).await {
                Ok(updated) if PegoutOrderStatus::from_label(&updated.status).ok() == Some(PegoutOrderStatus::Pending) => {
                    if let Err(e) = db::expire_pegout_order(&state.db, &id).await {
                        eprintln_redacted!("failed to expire pegout order {}: {e:#}", short(&id));
                    }
                }
                Ok(_) => {}
                Err(e) => eprintln_redacted!("pegout order poll error for {}: {e:#} - not expiring", short(&id)),
            }
            continue;
        }

        if let Err(e) = check_pegout_deposit(state, order, true).await {
            eprintln_redacted!("pegout order poll error for {}: {e:#}", short(&id));
        }
    }
    if let Err(e) = revive_late_pegouts(state).await {
        eprintln_redacted!("[poll] late pegout revival failed: {e:#}");
    }
    Ok(())
}

async fn poll_orders_once(state: &AppState) -> anyhow::Result<()> {
    let max_age_secs = config::order_max_age().as_secs();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let active = db::list_active_orders(&state.db).await?;
    eprintln!("[poll] tick: {} active order(s)", active.len());

    // Batch-sync once per tick, scoped to the treasury deposit address and the deposit addresses
    // of orders `detect_deposit` will actually check (`pending`/`deposit_seen`) rather than a full
    // `ChainWallet::sync`, which re-checks every script pubkey the wallet has ever revealed - see
    // `ChainWallet::sync_addresses`.
    let mut scripts = vec![state.ecx.lock().await.treasury_address().script_pubkey()];
    for o in &active {
        if !matches!(
            OrderStatus::from_label(&o.status)?,
            OrderStatus::Pending | OrderStatus::DepositSeen
        ) {
            continue;
        }
        if let Ok(addr) = o.deposit_address.parse::<Address<_>>() {
            scripts.push(addr.assume_checked().script_pubkey());
        }
    }
    if let Err(e) = sync_scripts_in_chunks(state, scripts).await {
        eprintln_redacted!("[poll] ecx sync failed: {e:#}");
    }

    for order in active {
        let id = order.id.clone();
        let status = OrderStatus::from_label(&order.status)?;
        let age_secs = (now - order.created_at).max(0) as u64;
        eprintln!("[poll] order {}: status={status:?} age={age_secs}s", short(&id));

        // A seen deposit is customer funds. Never expire it. A still-pending order is expired only
        // after this tick's sync has been checked, so a deposit that landed in the same sync is
        // recorded instead of being abandoned and later spent as float.
        if status == OrderStatus::Pending && age_secs > max_age_secs {
            match check_deposit(state, order, false).await {
                Ok(updated) if OrderStatus::from_label(&updated.status).ok() == Some(OrderStatus::Pending) => {
                    if let Err(e) = db::expire_order(&state.db, &id).await {
                        eprintln_redacted!("failed to expire order {}: {e:#}", short(&id));
                    }
                }
                Ok(_) => {}
                Err(e) => eprintln_redacted!("order poll error for {}: {e:#} - not expiring", short(&id)),
            }
            continue;
        }

        if let Err(e) = check_deposit(state, order, false).await {
            eprintln_redacted!("order poll error for {}: {e:#}", short(&id));
        }
    }
    if let Err(e) = revive_late_wraps(state).await {
        eprintln_redacted!("[poll] late wrap revival failed: {e:#}");
    }
    Ok(())
}

/// Syncs `scripts` against the explorer in chunks, taking the wallet lock per chunk instead of for
/// the whole batch. A chunk is a couple of seconds of Esplora requests. Holding the lock across a
/// full set of `config::max_pending_orders` addresses would stall every `create_order`, which
/// needs the lock for a fresh address and holds `money_operations` while it waits, and with it
/// every other order request, for the length of the tick's sync. Stops at the first failed chunk:
/// an explorer that is down would otherwise time out once per chunk.
async fn sync_scripts_in_chunks(state: &AppState, scripts: Vec<ScriptBuf>) -> anyhow::Result<()> {
    const CHUNK: usize = 50;
    for chunk in scripts.chunks(CHUNK) {
        state.ecx.lock().await.sync_addresses(chunk.to_vec()).await?;
    }
    Ok(())
}

/// Whether an order that expired at `updated_at` is still inside `config::expiry_grace`, the
/// window in which a late deposit revives it.
fn within_expiry_grace(updated_at: i64) -> bool {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    updated_at >= now.saturating_sub(config::expiry_grace().as_secs() as i64)
}

/// Puts an expired wrap order back in play if its deposit address has since been paid. With
/// `sync`, the address is re-checked against the explorer first; without it, only what earlier
/// syncs already recorded counts. Returns the order as it stands afterwards: revived and advanced
/// through `check_deposit`, or unchanged. Used by the poller's revival pass and by `get_order`, so
/// a customer who paid after the 15-minute window and is looking at the "expired" page is served
/// as soon as the deposit shows up, not only if their order is among the ones the poller re-checks.
async fn revive_wrap_if_funded(state: &AppState, order: db::Order, sync: bool) -> anyhow::Result<db::Order> {
    let script = order.deposit_address.parse::<Address<_>>()?.assume_checked().script_pubkey();
    let mut wallet = state.ecx.lock().await;
    if sync {
        wallet.sync_addresses([script.clone()]).await?;
    }
    let funded = wallet.outputs().into_iter().any(|output| output.txout.script_pubkey == script);
    drop(wallet);
    if !funded || !db::revive_expired_order(&state.db, &order.id).await? {
        return Ok(order);
    }
    let Some(revived) = db::get_order(&state.db, &order.id).await? else {
        return Ok(order);
    };
    check_deposit(state, revived, false).await
}

/// Peg-out mirror of `revive_wrap_if_funded`. The reference-key lookup is the sync itself, so
/// without `sync` there is nothing to consult and the order is returned unchanged.
async fn revive_pegout_if_funded(
    state: &AppState,
    order: db::PegoutOrder,
    sync: bool,
) -> anyhow::Result<db::PegoutOrder> {
    if !sync {
        return Ok(order);
    }
    let reference = solana::parse_pubkey(&order.reference_pubkey)?;
    let found = state
        .solana
        .find_reference_deposit(&reference, order.amount_in_base_units as u64)
        .await?;
    if found.is_none() || !db::revive_expired_pegout_order(&state.db, &order.id).await? {
        return Ok(order);
    }
    let Some(revived) = db::get_pegout_order(&state.db, &order.id).await? else {
        return Ok(order);
    };
    check_pegout_deposit(state, revived, true).await
}

async fn revive_late_wraps(state: &AppState) -> anyhow::Result<()> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    let since = now.saturating_sub(config::expiry_grace().as_secs() as i64);
    let expired = db::list_expired_orders_since(&state.db, since, 100).await?;
    if expired.is_empty() {
        return Ok(());
    }
    let scripts: Vec<_> = expired
        .iter()
        .filter_map(|order| order.deposit_address.parse::<Address<_>>().ok())
        .map(|address| address.assume_checked().script_pubkey())
        .collect();
    if let Err(e) = sync_scripts_in_chunks(state, scripts).await {
        eprintln_redacted!("[poll] late-deposit sync failed: {e:#}");
        return Ok(());
    }
    for order in expired {
        let id = order.id.clone();
        if let Err(e) = revive_wrap_if_funded(state, order, false).await {
            eprintln_redacted!("[poll] expired order {} could not be revived: {e:#}", short(&id));
        }
    }
    Ok(())
}

async fn revive_late_pegouts(state: &AppState) -> anyhow::Result<()> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    let since = now.saturating_sub(config::expiry_grace().as_secs() as i64);
    for order in db::list_expired_pegout_orders_since(&state.db, since, 50).await? {
        let id = order.id.clone();
        if let Err(e) = revive_pegout_if_funded(state, order, true).await {
            eprintln_redacted!("[poll] expired pegout {} could not be revived: {e:#}", short(&id));
        }
    }
    Ok(())
}

/// Serves the quote and order API on `addr`, and spawns the background order poller (see
/// `run_order_poller`).
pub async fn serve(
    ecx: ChainWallet,
    solana: SolanaWallet,
    db: db::Connection,
    addr: &str,
) -> anyhow::Result<()> {
    if config::live_payouts_allowed() {
        config::reject_public_solana_rpc(&config::solana_rpc_url())?;
    }
    let earnings = config::EarningsConfig::load()?;
    if let Some(settings) = earnings.as_ref().filter(|settings| settings.enabled) {
        anyhow::ensure!(solana.decimals() == 8, "earnings sweeps currently require an 8-decimal wbECX mint");
        settings.thresholds(solana.decimals())?;
        solana.validate_recipient(&solana::parse_pubkey(&settings.solana_owner_address)?)?;
        // The Solana side is covered by `validate_recipient` (the treasury is a reserved
        // recipient). The ECX side needs a derivation check: an owner address taken from the hot
        // wallet would record fees as swept while they never left this machine.
        let ecx_owner = settings
            .ecx_owner_address
            .parse::<Address<_>>()?
            .require_network(ChainConfig::ecx().network)?;
        anyhow::ensure!(
            !ecx.owns_address(&ecx_owner),
            "ecx_owner_address {ecx_owner} belongs to this hot wallet; use a cold wallet on a different seed"
        );
    }
    let state = Arc::new(AppState {
        ecx: Mutex::new(ecx),
        solana,
        db,
        limits: Limits::default(),
        stats_cache: Mutex::new(None),
        earnings,
        money_operations: Mutex::new(()),
    });

    tokio::spawn(run_order_poller(state.clone()));

    let app = Router::new()
        .route("/quote", get(get_quote))
        .route("/stats", get(get_stats))
        .route("/orders", post(create_order))
        .route("/orders/{id}", get(get_order))
        .route("/pegout/quote", get(get_pegout_quote))
        .route("/pegout/orders", post(create_pegout_order))
        .route("/pegout/orders/{id}", get(get_pegout_order))
        .with_state(state);

    let listener = TcpListener::bind(addr).await?;
    println!("listening on {addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    const TEST_XPRV: &str = "xprv9s21ZrQH143K3QTDL4LXw2F7HEK3wJUD2nW2nRk4stbPy6cq3jPPqjiChkVvvNKmPGJxWUtg6LnF5kejMRNNU3TGtRBeJgk33yuGBxrMPHi";

    struct TestEnv {
        dir: std::path::PathBuf,
        state: Arc<AppState>,
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    async fn test_env() -> TestEnv {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("server-test-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = ChainConfig {
            network: bdk_wallet::bitcoin::Network::Bitcoin,
            backend: config::Backend::Esplora,
            esplora_url: "http://127.0.0.1:9".to_string(),
            electrum_url: String::new(),
            db_path: dir.join("ecx.db"),
        };
        let ecx = ChainWallet::open(
            &config,
            &format!("wpkh({TEST_XPRV}/84h/0h/0h/0/*)"),
            &format!("wpkh({TEST_XPRV}/84h/0h/0h/1/*)"),
        )
        .unwrap();
        let solana = SolanaWallet::for_test(Keypair::new(), Keypair::new().pubkey(), 8);
        let db = db::Connection::in_memory_for_test().await;
        let state = Arc::new(AppState {
            ecx: Mutex::new(ecx),
            solana,
            db,
            limits: Limits::default(),
            stats_cache: Mutex::new(None),
            earnings: None,
            money_operations: Mutex::new(()),
        });
        TestEnv { dir, state }
    }

    fn funding_tx(outputs: Vec<(ScriptBuf, u64)>, seed: u8) -> bdk_wallet::bitcoin::Transaction {
        use bdk_wallet::bitcoin::absolute::LockTime;
        use bdk_wallet::bitcoin::hashes::Hash;
        use bdk_wallet::bitcoin::transaction::Version;
        use bdk_wallet::bitcoin::{Sequence, TxIn, TxOut, Txid, Witness};

        bdk_wallet::bitcoin::Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: Txid::from_slice(&[seed; 32]).unwrap(),
                    vout: 0,
                },
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: outputs
                .into_iter()
                .map(|(script_pubkey, sats)| TxOut {
                    value: Amount::from_sat(sats),
                    script_pubkey,
                })
                .collect(),
        }
    }

    #[test]
    fn earnings_include_history_and_subtract_reserved_sweeps() {
        let amounts = [(10_000, 9_900), (20_000, 19_800)];
        assert_eq!(unswept_fees(&amounts, 0).unwrap(), 300);
        assert_eq!(unswept_fees(&amounts, 100).unwrap(), 200);
        assert_eq!(unswept_fees(&amounts, 300).unwrap(), 0);
        assert!(unswept_fees(&amounts, 301).is_err());
        assert!(unswept_fees(&[(100, 101)], 0).is_err());
    }

    #[test]
    fn order_creation_limit_admits_up_to_the_cap_then_refuses() {
        let _guard = config::env_lock();
        unsafe {
            std::env::set_var("ORDERS_PER_MINUTE", "3");
        }
        let limits = Limits::default();
        assert!(limits.allow_order_creation());
        assert!(limits.allow_order_creation());
        assert!(limits.allow_order_creation());
        assert!(!limits.allow_order_creation());
        unsafe {
            std::env::set_var("ORDERS_PER_MINUTE", "0");
        }
        assert!(limits.allow_order_creation(), "0 must disable the limit, not refuse every order");
        unsafe {
            std::env::remove_var("ORDERS_PER_MINUTE");
        }
    }

    #[test]
    fn get_sync_is_throttled_to_one_per_interval() {
        let _guard = config::env_lock();
        let limits = Limits::default();
        assert!(limits.allow_get_sync());
        assert!(!limits.allow_get_sync());
    }

    #[test]
    fn payout_viable_requires_the_minimum_and_a_nonzero_output() {
        let _guard = config::env_lock();
        let min = config::min_deposit_amount().to_sat();
        assert!(payout_viable(min, 1));
        assert!(!payout_viable(min - 1, 1_000), "below-minimum deposits must not be paid out");
        assert!(!payout_viable(min, 0), "zero-output deposits must not be paid out");
    }

    #[test]
    fn pegout_payout_viable_requires_the_minimum() {
        let _guard = config::env_lock();
        let min = config::min_deposit_amount().to_sat();
        assert!(pegout_payout_viable(min));
        assert!(!pegout_payout_viable(min - 1));
    }

    #[test]
    fn short_ids_never_panic_on_short_input() {
        assert_eq!(short("abc"), "abc");
        assert_eq!(short("0123456789abcdef"), "01234567");
    }

    #[test]
    fn unmatched_deposit_rules_never_send_to_the_wrong_place() {
        let base = UnmatchedFacts {
            amount_base_units: 10_000,
            depositor_owner: Some("wallet"),
            depositor_payable: true,
            depositor_is_funding_wallet: false,
            live_payouts: true,
            refunds_enabled: true,
            fee_bps: 100,
            min_refund_base_units: 50,
        };
        let refund = decide_unmatched(&base);
        assert_eq!(
            (refund.status, refund.fee_base_units, refund.refund_base_units),
            (UnmatchedStatus::Pending, 100, 9_900)
        );

        let funding = decide_unmatched(&UnmatchedFacts { depositor_is_funding_wallet: true, ..base });
        assert_eq!((funding.status, funding.fee_base_units, funding.refund_base_units), (UnmatchedStatus::Kept, 0, 0),
            "the operator's own liquidity is neither refunded nor charged");

        for facts in [
            UnmatchedFacts { depositor_owner: None, ..base },
            UnmatchedFacts { depositor_payable: false, ..base },
            UnmatchedFacts { live_payouts: false, ..base },
            UnmatchedFacts { refunds_enabled: false, ..base },
        ] {
            let held = decide_unmatched(&facts);
            assert_eq!((held.status, held.refund_base_units), (UnmatchedStatus::Held, 0), "{}", held.note);
        }

        let dust = decide_unmatched(&UnmatchedFacts { amount_base_units: 40, ..base });
        assert_eq!((dust.status, dust.fee_base_units, dust.refund_base_units), (UnmatchedStatus::Kept, 40, 0),
            "a refund below the floor is kept whole as the fee");
        let one_unit = decide_unmatched(&UnmatchedFacts { amount_base_units: 1, ..base });
        assert_eq!(one_unit.status, UnmatchedStatus::Kept);
    }

    #[tokio::test]
    async fn detect_deposit_sums_multiple_outputs_and_drops_unpaid_stipend_on_confirmation() {
        let env = test_env().await;
        let deposit_addr = env.state.ecx.lock().await.fresh_address().unwrap();
        let recipient = Keypair::new().pubkey().to_string();

        // Order created with a 500_000-sat SOL stipend, but the customer only deposits 400_000 sat
        // across two outputs (100_000 + 300_000).
        let order = db::insert_order(
            &env.state.db,
            &db::NewOrder {
                amount_in_sat: 100_500_000,
                amount_out_base_units: 98_999_010,
                deposit_address: deposit_addr.to_string(),
                solana_recipient: recipient,
                sol_stipend_lamports: 5_000_000,
                sol_stipend_price_sat: 500_000,
                ata_rent_lamports: 0,
                ata_rent_price_sat: 0,
            },
        )
        .await
        .unwrap();

        let tx1 = funding_tx(vec![(deposit_addr.script_pubkey(), 100_000)], 1);
        let tx2 = funding_tx(vec![(deposit_addr.script_pubkey(), 300_000)], 2);
        let tx2_id = tx2.compute_txid().to_string();
        {
            let mut wallet = env.state.ecx.lock().await;
            wallet.apply_unconfirmed_for_test(tx1.clone(), 1_700_000_000);
            wallet.apply_unconfirmed_for_test(tx2.clone(), 1_700_000_000);
        }

        // Unconfirmed: transitions to deposit_seen, sums both outputs, and preserves the stipend
        // offer on the row in case another payment lands before confirmation.
        let seen = detect_deposit(&env.state, order, false).await.unwrap();
        assert_eq!(seen.status, "deposit_seen");
        assert_eq!(seen.amount_in_sat, 400_000);
        assert_eq!(seen.deposit_txid.as_deref(), Some(tx2_id.as_str()));
        assert_eq!(seen.sol_stipend_lamports, 5_000_000);
        assert_eq!(seen.sol_stipend_price_sat, 500_000);

        // Confirmed at depth 3: drops the unpaid stipend so completed_wrap_amounts and unswept_fees
        // do not subtract an unpaid 500_000-sat stipend from a 400_000-sat deposit.
        {
            let mut wallet = env.state.ecx.lock().await;
            wallet.confirm_tx_for_test(tx1, 100, 102);
            wallet.confirm_tx_for_test(tx2, 100, 102);
        }
        let confirmed = detect_deposit(&env.state, seen, false).await.unwrap();
        assert_eq!(confirmed.status, "deposit_confirmed");
        assert_eq!(confirmed.sol_stipend_lamports, 0);
        assert_eq!(confirmed.sol_stipend_price_sat, 0);
        let expected_out = convert::pegin_payout_base_units(
            400_000,
            8,
            config::pegin_fee_bps(),
            config::pegin_flat_fee_sat(),
        );
        assert_eq!(confirmed.amount_out_base_units, expected_out as i64);

        // Simulate payout completion and verify earnings calculation succeeds without underflow.
        assert!(db::claim_for_payout(&env.state.db, &confirmed.id).await.unwrap());
        assert!(db::update_payout(&env.state.db, &confirmed.id, "sol-sig").await.unwrap());
        let completed = db::completed_wrap_amounts(&env.state.db).await.unwrap();
        assert_eq!(completed, vec![(400_000, expected_out)]);
        assert_eq!(unswept_fees(&completed, 0).unwrap(), 400_000 - expected_out);
    }

    #[tokio::test]
    async fn detect_deposit_routes_uncovered_rent_and_dust_to_refund() {
        let env = test_env().await;

        // 1. Deposit cannot cover locked ATA rent price -> refund_requested.
        let rent_addr = env.state.ecx.lock().await.fresh_address().unwrap();
        let rent_order = db::insert_order(
            &env.state.db,
            &db::NewOrder {
                amount_in_sat: 1_203_928,
                amount_out_base_units: 989_010,
                deposit_address: rent_addr.to_string(),
                solana_recipient: Keypair::new().pubkey().to_string(),
                sol_stipend_lamports: 0,
                sol_stipend_price_sat: 0,
                ata_rent_lamports: 2_039_280,
                ata_rent_price_sat: 203_928,
            },
        )
        .await
        .unwrap();
        let short_rent_tx = funding_tx(vec![(rent_addr.script_pubkey(), 200_000)], 10);
        env.state.ecx.lock().await.confirm_tx_for_test(short_rent_tx, 50, 55);
        let after_rent = detect_deposit(&env.state, rent_order, false).await.unwrap();
        assert_eq!(after_rent.status, "refund_requested");
        assert!(after_rent.refund_reason.contains("formatted wrong"));

        // 2. Deposit below minimum -> refund_requested.
        let dust_addr = env.state.ecx.lock().await.fresh_address().unwrap();
        let dust_order = db::insert_order(
            &env.state.db,
            &db::NewOrder {
                amount_in_sat: 100_000,
                amount_out_base_units: 98_010,
                deposit_address: dust_addr.to_string(),
                solana_recipient: Keypair::new().pubkey().to_string(),
                sol_stipend_lamports: 0,
                sol_stipend_price_sat: 0,
                ata_rent_lamports: 0,
                ata_rent_price_sat: 0,
            },
        )
        .await
        .unwrap();
        let dust_tx = funding_tx(vec![(dust_addr.script_pubkey(), 300)], 11);
        env.state.ecx.lock().await.confirm_tx_for_test(dust_tx, 50, 55);
        let after_dust = detect_deposit(&env.state, dust_order, false).await.unwrap();
        assert_eq!(after_dust.status, "refund_requested");
    }

    #[tokio::test]
    async fn protected_outpoints_cover_unsettled_and_expired_orders_but_exclude_refund_target() {
        let env = test_env().await;
        let (addr_pending, addr_expired, addr_paid) = {
            let mut wallet = env.state.ecx.lock().await;
            (
                wallet.fresh_address().unwrap(),
                wallet.fresh_address().unwrap(),
                wallet.fresh_address().unwrap(),
            )
        };

        let pending_order = db::insert_order(
            &env.state.db,
            &db::NewOrder {
                amount_in_sat: 50_000,
                amount_out_base_units: 48_510,
                deposit_address: addr_pending.to_string(),
                solana_recipient: Keypair::new().pubkey().to_string(),
                sol_stipend_lamports: 0,
                sol_stipend_price_sat: 0,
                ata_rent_lamports: 0,
                ata_rent_price_sat: 0,
            },
        )
        .await
        .unwrap();
        let expired_order = db::insert_order(
            &env.state.db,
            &db::NewOrder {
                amount_in_sat: 60_000,
                amount_out_base_units: 58_410,
                deposit_address: addr_expired.to_string(),
                solana_recipient: Keypair::new().pubkey().to_string(),
                sol_stipend_lamports: 0,
                sol_stipend_price_sat: 0,
                ata_rent_lamports: 0,
                ata_rent_price_sat: 0,
            },
        )
        .await
        .unwrap();
        db::expire_order(&env.state.db, &expired_order.id).await.unwrap();

        let paid_order = db::insert_order(
            &env.state.db,
            &db::NewOrder {
                amount_in_sat: 70_000,
                amount_out_base_units: 68_310,
                deposit_address: addr_paid.to_string(),
                solana_recipient: Keypair::new().pubkey().to_string(),
                sol_stipend_lamports: 0,
                sol_stipend_price_sat: 0,
                ata_rent_lamports: 0,
                ata_rent_price_sat: 0,
            },
        )
        .await
        .unwrap();
        db::update_deposit(
            &env.state.db,
            &paid_order.id,
            OrderStatus::DepositConfirmed,
            "tx-paid",
            Amount::from_sat(70_000),
            68_310,
            0,
            0,
            3,
        )
        .await
        .unwrap();
        db::claim_for_payout(&env.state.db, &paid_order.id).await.unwrap();
        db::update_payout(&env.state.db, &paid_order.id, "sig-paid").await.unwrap();

        let tx_pending = funding_tx(vec![(addr_pending.script_pubkey(), 50_000)], 21);
        let tx_expired = funding_tx(vec![(addr_expired.script_pubkey(), 60_000)], 22);
        let tx_paid = funding_tx(vec![(addr_paid.script_pubkey(), 70_000)], 23);
        let op_pending = OutPoint { txid: tx_pending.compute_txid(), vout: 0 };
        let op_expired = OutPoint { txid: tx_expired.compute_txid(), vout: 0 };
        let op_paid = OutPoint { txid: tx_paid.compute_txid(), vout: 0 };
        {
            let mut wallet = env.state.ecx.lock().await;
            wallet.confirm_tx_for_test(tx_pending, 10, 15);
            wallet.confirm_tx_for_test(tx_expired, 10, 15);
            wallet.confirm_tx_for_test(tx_paid, 10, 15);
        }

        let wallet = env.state.ecx.lock().await;
        let all_protected = protected_deposit_outpoints(&env.state.db, &wallet, None).await.unwrap();
        assert!(all_protected.contains(&op_pending));
        assert!(all_protected.contains(&op_expired), "expired order deposits must stay protected from coin selection");
        assert!(!all_protected.contains(&op_paid), "paid_out order deposits are treasury float");

        let excluding_pending = protected_deposit_outpoints(&env.state.db, &wallet, Some(&pending_order.id))
            .await
            .unwrap();
        assert!(!excluding_pending.contains(&op_pending), "excluded order must be spendable for its own refund");
        assert!(excluding_pending.contains(&op_expired));
    }

    #[tokio::test]
    async fn expired_wrap_order_revives_when_late_deposit_arrives() {
        let env = test_env().await;
        let deposit_addr = env.state.ecx.lock().await.fresh_address().unwrap();
        let order = db::insert_order(
            &env.state.db,
            &db::NewOrder {
                amount_in_sat: 50_000,
                amount_out_base_units: 48_510,
                deposit_address: deposit_addr.to_string(),
                solana_recipient: Keypair::new().pubkey().to_string(),
                sol_stipend_lamports: 0,
                sol_stipend_price_sat: 0,
                ata_rent_lamports: 0,
                ata_rent_price_sat: 0,
            },
        )
        .await
        .unwrap();
        db::expire_order(&env.state.db, &order.id).await.unwrap();
        let expired = db::get_order(&env.state.db, &order.id).await.unwrap().unwrap();
        assert_eq!(expired.status, "expired");

        // Unfunded expired order stays expired.
        let still_expired = revive_wrap_if_funded(&env.state, expired.clone(), false).await.unwrap();
        assert_eq!(still_expired.status, "expired");

        // Late deposit lands -> revives and advances straight to deposit_confirmed.
        let tx = funding_tx(vec![(deposit_addr.script_pubkey(), 50_000)], 31);
        env.state.ecx.lock().await.confirm_tx_for_test(tx, 20, 25);
        let revived = revive_wrap_if_funded(&env.state, expired, false).await.unwrap();
        assert_eq!(revived.status, "deposit_confirmed");
        assert_eq!(revived.amount_in_sat, 50_000);
    }
}
