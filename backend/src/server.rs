use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use bdk_wallet::bitcoin::{Address, Amount, OutPoint};
use serde::{Deserialize, Serialize};
use solana_sdk::signature::{Keypair, Signer};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::time::{interval, MissedTickBehavior};

use crate::config::{self, ChainConfig};
use crate::db::{OrderStatus, PegoutOrderStatus};
use crate::solana::SolanaWallet;
use crate::wallet::ChainWallet;
use crate::{convert, db, solana};

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
    /// Sliding one-minute window over successful admissions. Returns `false` when the window is full.
    fn allow_order_creation(&self) -> bool {
        let mut window = self.order_creations.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        while window
            .front()
            .is_some_and(|t| now.duration_since(*t) > std::time::Duration::from_secs(60))
        {
            window.pop_front();
        }
        if window.len() as u64 >= config::max_orders_per_minute() {
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
    eprintln!("[error] {err:#}");
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
    /// Flat fee that will apply to the wECX->ECX peg-out direction, in basis points (see
    /// `config::pegout_fee_bps`) - informational only for now, since peg-out itself isn't
    /// implemented yet (PLAN.md phase 2). Not used in `amount_out_base_units` above.
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
}

#[derive(Clone, Serialize)]
struct StatsResponse {
    ecx_confirmed_sat: String,
    ecx_unconfirmed_sat: String,
    wecx_balance_base_units: String,
    sol_balance_lamports: String,
    solana_treasury_address: String,
    wecx_decimals: u8,
    wraps: u64,
    unwraps: u64,
    wrap_fees_ecx_sat: String,
    unwrap_fees_wecx_base_units: String,
    fees_paid_ecx_sat: String,
    fees_paid_wecx_base_units: String,
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

    let wrap_fees_sat: u128 = wraps
        .iter()
        .map(|(amount_in_sat, amount_out_base_units)| {
            let amount_out_sat =
                convert::wecx_base_units_to_ecx_sat(*amount_out_base_units, decimals, 0);
            amount_in_sat.saturating_sub(amount_out_sat) as u128
        })
        .sum();
    let unwrap_fees_base_units: u128 = unwraps
        .iter()
        .map(|(amount_in_base_units, amount_out_sat)| {
            let amount_out_base_units =
                convert::ecx_sat_to_wecx_base_units(*amount_out_sat, decimals, 0);
            amount_in_base_units.saturating_sub(amount_out_base_units) as u128
        })
        .sum();

    let mut ecx = state.ecx.lock().await;
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
        solana_treasury_address: state.solana.pubkey().to_string(),
        wecx_decimals: decimals,
        wraps: wraps.len() as u64,
        unwraps: unwraps.len() as u64,
        wrap_fees_ecx_sat: wrap_fees_sat.to_string(),
        unwrap_fees_wecx_base_units: unwrap_fees_base_units.to_string(),
        fees_paid_ecx_sat: fees_paid_ecx_sat.to_string(),
        fees_paid_wecx_base_units: fees_paid_wecx_base_units.to_string(),
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
    })
}

#[derive(Deserialize)]
struct CreateOrderRequest {
    amount_in_sat: u64,
    /// Solana address the treasury-custodied wECX gets sent to.
    solana_recipient: String,
    /// ECX address the deposit gets sent back to if it can't be bridged (see
    /// `db::OrderStatus::RefundRequested`). Optional - an order created without one simply can't
    /// be refunded: it just sits recorded for manual review instead.
    #[serde(default)]
    refund_address: Option<String>,
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

    let ecx_network = ChainConfig::ecx().network;
    let refund_address: Option<Address> = match req.refund_address.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(addr) => Some(
            addr.parse::<Address<_>>()
                .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid refund_address: {e}")))?
                .require_network(ecx_network)
                .map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        format!("refund_address is not a valid {} address: {e}", config::asset_ticker()),
                    )
                })?,
        ),
    };

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
            eprintln!("[create_order] recipient token-account check failed: {e:#}");
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
            refund_address: refund_address.map(|a| a.to_string()),
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
/// wECX (a dust deposit, or one small enough that `pegin_fee_bps`/decimal rounding eats it
/// entirely), this isn't worth paying out or bridging at all (see `payout_viable`): routed
/// straight to a refund when one's on file, otherwise left recorded for manual review - `try_fulfill`
/// refuses to pay it out, and `try_refund` takes the same stance on a missing `refund_address`.
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
    if new_status.label() == order.status
        && order.deposit_txid.as_deref() == Some(deposit_txid.as_str())
        && actual_amount_in.to_sat() as i64 == order.amount_in_sat
        && settled.amount_out_base_units as i64 == order.amount_out_base_units
    {
        return Ok(order);
    }

    let amount_out = settled.amount_out_base_units;

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
        ..order
    };

    if new_status == OrderStatus::DepositConfirmed
        && (settled.ata_rent_uncovered || !payout_viable(settled.wrap_sat, amount_out))
        && updated.refund_address.is_some()
    {
        eprintln!(
            "[detect_deposit] order {}: deposit cannot cover a new token account, is below the \
             minimum, or reprices to zero wECX -> requesting refund",
            short(&updated.id)
        );
        if db::request_refund(&state.db, &updated.id).await? {
            updated.status = OrderStatus::RefundRequested.label().to_string();
        }
    }

    Ok(updated)
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
        // Both betanet ECX and Solana mainnet-beta are real money (see CLAUDE.md) - the operator
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
        if order.refund_address.is_some() && db::request_refund(&state.db, &order.id).await? {
            return Ok(db::Order {
                status: OrderStatus::RefundRequested.label().to_string(),
                ..order
            });
        }
        return Ok(order);
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
            "[try_fulfill] order {}: deposit is below the minimum or reprices to zero wECX - not \
             paying out, needs manual review/refund",
            short(&order.id)
        );
        return Ok(order);
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
            eprintln!("payout could not be prepared for order {}: {e:#}", short(&order.id));
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
        eprintln!("could not record payout signature for order {}: {e:#}", short(&order.id));
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
            eprintln!(
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

/// Sends a `refund_requested` order's `amount_in_sat` back to its `refund_address` on ECX. Also
/// finishes a `refunding` order whose signed transaction was saved before broadcast. A lost
/// broadcast response retries that same transaction. It does not build a second one.
async fn try_refund(state: &AppState, order: db::Order) -> anyhow::Result<db::Order> {
    let status = OrderStatus::from_label(&order.status)?;
    if !matches!(status, OrderStatus::RefundRequested | OrderStatus::Refunding) {
        return Ok(order);
    }
    if !config::live_payouts_allowed() {
        return Ok(order);
    }

    if let Some(tx_hex) = db::outbound_tx_hex(&state.db, &order.id, WRAP_REFUND_TX).await? {
        let refund_txid = resubmit_saved_ecx_tx(state, &tx_hex).await?;
        db::update_refund(&state.db, &order.id, &refund_txid).await?;
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

    if !db::claim_for_refund(&state.db, &order.id).await? {
        return Ok(db::get_order(&state.db, &order.id).await?.unwrap_or(order));
    }

    let refund_address = match order.refund_address.as_deref().map(|a| a.parse::<Address<_>>()) {
        Some(Ok(addr)) => addr.assume_checked(),
        Some(Err(e)) => {
            db::revert_refund_claim(&state.db, &order.id).await?;
            anyhow::bail!("stored refund_address {:?} is invalid: {e}", order.refund_address);
        }
        None => {
            db::revert_refund_claim(&state.db, &order.id).await?;
            anyhow::bail!(
                "order {} reached refund_requested with no refund_address on file",
                order.id
            );
        }
    };

    let prepared = async {
        let mut wallet = state.ecx.lock().await;
        let already_spent = wallet.verify_unspent().await?;
        let mut protected = protected_deposit_outpoints(&state.db, &wallet, Some(&order.id)).await?;
        protected.extend(already_spent);
        wallet.prepare_signed_transfer(
            &refund_address,
            Amount::from_sat(order.amount_in_sat as u64),
            config::ecx_payout_fee_rate(),
            &protected,
        )
    }
    .await;
    let tx = match prepared {
        Ok(tx) => tx,
        Err(e) => {
            eprintln!("refund could not be prepared for order {}: {e:#}", short(&order.id));
            db::revert_refund_claim(&state.db, &order.id).await?;
            return Ok(db::Order {
                status: OrderStatus::RefundRequested.label().to_string(),
                ..order
            });
        }
    };
    let refund_txid = tx.compute_txid().to_string();
    let tx_hex = bdk_wallet::bitcoin::consensus::encode::serialize_hex(&tx);
    if let Err(e) = db::record_outbound_tx(&state.db, &order.id, WRAP_REFUND_TX, &refund_txid, &tx_hex).await {
        eprintln!("could not record refund transaction for order {}: {e:#}", short(&order.id));
        db::revert_refund_claim(&state.db, &order.id).await?;
        return Ok(db::Order {
            status: OrderStatus::RefundRequested.label().to_string(),
            ..order
        });
    }
    // The hex is durable. A broadcast error leaves the order refunding so the next poll retries
    // this transaction rather than signing a different one.
    if let Err(e) = state.ecx.lock().await.submit_signed_transfer(tx).await {
        eprintln!(
            "refund broadcast for order {} is unresolved ({refund_txid}): {e:#} - will retry the saved transaction",
            short(&order.id)
        );
        return Ok(db::Order {
            status: OrderStatus::Refunding.label().to_string(),
            ..order
        });
    }
    db::update_refund(&state.db, &order.id, &refund_txid).await?;
    Ok(db::Order {
        status: OrderStatus::Refunded.label().to_string(),
        refund_txid: Some(refund_txid),
        ..order
    })
}

// ---------------------------------------------------------------------------------------------
// Peg-out (wECX -> ECX): the mirror of everything above with the two chains swapped. See
// PLAN.md's phase 2 note and `solana::SolanaWallet::find_reference_deposit`'s doc comment for why
// this direction can't just hand out a fresh per-order address the way the ECX side does, and
// `db.rs`'s `pegout_orders` section for the state machine this drives.
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
/// otherwise left recorded for manual review, matching the peg-in stance on a missing
/// `refund_address`.
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
            eprintln!("payout could not be prepared for pegout order {}: {e:#}", short(&order.id));
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
        eprintln!("could not record pegout payout transaction for order {}: {e:#}", short(&order.id));
        db::revert_pegout_claim(&state.db, &order.id).await?;
        return Ok(db::PegoutOrder {
            status: PegoutOrderStatus::DepositConfirmed.label().to_string(),
            ..order
        });
    }
    if let Err(e) = state.ecx.lock().await.submit_signed_transfer(tx).await {
        eprintln!(
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

/// Sends a `refund_requested` peg-out order's `amount_in_base_units` back to its
/// `depositor_owner` on Solana. No-op for orders not currently `refund_requested`, or with no
/// `depositor_owner` on file (left for manual review). Structurally mirrors `try_fulfill`: this is
/// a Solana send, so it needs the same two-phase sign -> persist signature -> broadcast handling
/// and the same ambiguous-outcome deferral to `try_resolve_pegout_refund`.
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
        .prepare_wecx_transfer(recipient, order.amount_in_base_units as u64, 0)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("pegout refund could not be prepared for order {}: {e:#}", short(&order.id));
            db::revert_pegout_refund_claim(&state.db, &order.id).await?;
            return Ok(db::PegoutOrder {
                status: PegoutOrderStatus::RefundRequested.label().to_string(),
                ..order
            });
        }
    };
    let refund_signature = prepared.signature.to_string();

    if let Err(e) = db::record_pegout_refund_signature(&state.db, &order.id, &refund_signature).await {
        eprintln!("could not record pegout refund signature for order {}: {e:#}", short(&order.id));
        db::revert_pegout_refund_claim(&state.db, &order.id).await?;
        return Ok(db::PegoutOrder {
            status: PegoutOrderStatus::RefundRequested.label().to_string(),
            ..order
        });
    }

    match state.solana.submit(&prepared).await {
        Ok(solana::PayoutOutcome::Confirmed) => {
            if !db::update_pegout_refund(&state.db, &order.id, &refund_signature).await? {
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
            eprintln!(
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
            db::update_pegout_refund(&state.db, &order.id, &signature_str).await?;
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
    let order = check_deposit(&state, order, sync).await.unwrap_or(fallback);

    Ok(Json(order))
}

#[derive(Deserialize)]
struct SetRefundAddressRequest {
    refund_address: String,
}

/// Lets a customer who skipped `refund_address` at order creation add one later, so an order that
/// later needs a refund isn't stuck with no destination for it. Only fills a blank: an order that
/// already has one (from creation, or a previous call here) refuses rather than silently replacing
/// it, and only while the order hasn't reached a terminal status.
async fn set_refund_address(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<SetRefundAddressRequest>,
) -> Result<Json<db::Order>, (StatusCode, String)> {
    let Some(order) = db::get_order(&state.db, &id)
        .await
        .map_err(internal_error)?
    else {
        return Err((StatusCode::NOT_FOUND, "order not found".to_string()));
    };

    if order.refund_address.is_some() {
        return Err((
            StatusCode::BAD_REQUEST,
            "a refund address is already set for this order".to_string(),
        ));
    }

    let status = OrderStatus::from_label(&order.status).map_err(internal_error)?;
    if !matches!(
        status,
        OrderStatus::Pending
            | OrderStatus::DepositSeen
            | OrderStatus::DepositConfirmed
            | OrderStatus::PayingOut
    ) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "order is already settled (status: {}) - a refund address can no longer be set",
                order.status
            ),
        ));
    }

    let ecx_network = ChainConfig::ecx().network;
    let refund_address: Address = req
        .refund_address
        .trim()
        .parse::<Address<_>>()
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid refund_address: {e}")))?
        .require_network(ecx_network)
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!(
                    "refund_address is not a valid {} address: {e}",
                    config::asset_ticker()
                ),
            )
        })?;

    if !db::set_refund_address(&state.db, &id, &refund_address.to_string())
        .await
        .map_err(internal_error)?
    {
        return Err((
            StatusCode::CONFLICT,
            "a refund address was already set for this order - please refresh".to_string(),
        ));
    }

    let order = db::get_order(&state.db, &id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "order not found".to_string()))?;
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
    // `Limits::allow_get_sync`.
    let fallback = order.clone();
    let sync = state.limits.allow_get_sync();
    let order = check_pegout_deposit(&state, order, sync).await.unwrap_or(fallback);

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
            eprintln!("[earnings] sweep check failed: {error:#}");
        }
        if let Err(e) = poll_orders_once(&state).await {
            eprintln!("order poll failed: {e:#}");
        }
        if let Err(e) = poll_pegout_orders_once(&state).await {
            eprintln!("pegout order poll failed: {e:#}");
        }
    }
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
    for (asset, threshold, amounts) in [
        ("becx", ecx_threshold, db::completed_wrap_amounts(&state.db).await?),
        ("wbecx", wecx_threshold, db::completed_unwrap_amounts(&state.db).await?),
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
                        eprintln!("failed to expire pegout order {}: {e:#}", short(&id));
                    }
                }
                Ok(_) => {}
                Err(e) => eprintln!("pegout order poll error for {}: {e:#} - not expiring", short(&id)),
            }
            continue;
        }

        if let Err(e) = check_pegout_deposit(state, order, true).await {
            eprintln!("pegout order poll error for {}: {e:#}", short(&id));
        }
    }
    if let Err(e) = revive_late_pegouts(state).await {
        eprintln!("[poll] late pegout revival failed: {e:#}");
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

    // Batch-sync once per tick, scoped to the deposit addresses of orders `detect_deposit` will
    // actually check (`pending`/`deposit_seen`) rather than a full `ChainWallet::sync`, which
    // re-checks every script pubkey the wallet has ever revealed - see `ChainWallet::sync_addresses`.
    let mut scripts = Vec::new();
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
    if let Err(e) = state.ecx.lock().await.sync_addresses(scripts).await {
        eprintln!("[poll] ecx sync failed: {e:#}");
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
                        eprintln!("failed to expire order {}: {e:#}", short(&id));
                    }
                }
                Ok(_) => {}
                Err(e) => eprintln!("order poll error for {}: {e:#} - not expiring", short(&id)),
            }
            continue;
        }

        if let Err(e) = check_deposit(state, order, false).await {
            eprintln!("order poll error for {}: {e:#}", short(&id));
        }
    }
    if let Err(e) = revive_late_wraps(state).await {
        eprintln!("[poll] late wrap revival failed: {e:#}");
    }
    Ok(())
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
    if let Err(e) = state.ecx.lock().await.sync_addresses(scripts).await {
        eprintln!("[poll] late-deposit sync failed: {e:#}");
        return Ok(());
    }
    for order in expired {
        let id = order.id.clone();
        let script = match order.deposit_address.parse::<Address<_>>() {
            Ok(address) => address.assume_checked().script_pubkey(),
            Err(e) => {
                eprintln!("[poll] expired order {} has an invalid deposit address: {e}", short(&id));
                continue;
            }
        };
        let has_output = state.ecx.lock().await.outputs().into_iter().any(|output| output.txout.script_pubkey == script);
        if !has_output {
            continue;
        }
        if !db::revive_expired_order(&state.db, &id).await? {
            continue;
        }
        let Some(revived) = db::get_order(&state.db, &id).await? else {
            continue;
        };
        if let Err(e) = check_deposit(state, revived, false).await {
            eprintln!("[poll] revived order {} could not be checked: {e:#}", short(&id));
        }
    }
    Ok(())
}

async fn revive_late_pegouts(state: &AppState) -> anyhow::Result<()> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    let since = now.saturating_sub(config::expiry_grace().as_secs() as i64);
    for order in db::list_expired_pegout_orders_since(&state.db, since, 50).await? {
        let reference = match solana::parse_pubkey(&order.reference_pubkey) {
            Ok(reference) => reference,
            Err(e) => {
                eprintln!("[poll] expired pegout {} has an invalid reference: {e}", short(&order.id));
                continue;
            }
        };
        let found = state
            .solana
            .find_reference_deposit(&reference, order.amount_in_base_units as u64)
            .await?;
        if found.is_none() {
            continue;
        }
        if !db::revive_expired_pegout_order(&state.db, &order.id).await? {
            continue;
        }
        let Some(revived) = db::get_pegout_order(&state.db, &order.id).await? else {
            continue;
        };
        if let Err(e) = check_pegout_deposit(state, revived, true).await {
            eprintln!("[poll] revived pegout {} could not be checked: {e:#}", short(&order.id));
        }
    }
    Ok(())
}

/// Serves `GET /quote`, `POST /orders`, `GET /orders/{id}`, and `POST /orders/{id}/refund-address`
/// on `addr`, and spawns the background order poller (see `run_order_poller`).
pub async fn serve(
    ecx: ChainWallet,
    solana: SolanaWallet,
    db: db::Connection,
    addr: &str,
) -> anyhow::Result<()> {
    let earnings = config::EarningsConfig::load()?;
    if let Some(settings) = earnings.as_ref().filter(|settings| settings.enabled) {
        anyhow::ensure!(solana.decimals() == 8, "earnings sweeps currently require an 8-decimal wbECX mint");
        settings.thresholds(solana.decimals())?;
        solana.validate_recipient(&solana::parse_pubkey(&settings.solana_owner_address)?)?;
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
        .route("/orders/{id}/refund-address", post(set_refund_address))
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
        unsafe {
            std::env::set_var("ORDERS_PER_MINUTE", "3");
        }
        let limits = Limits::default();
        assert!(limits.allow_order_creation());
        assert!(limits.allow_order_creation());
        assert!(limits.allow_order_creation());
        assert!(!limits.allow_order_creation());
        unsafe {
            std::env::remove_var("ORDERS_PER_MINUTE");
        }
    }

    #[test]
    fn get_sync_is_throttled_to_one_per_interval() {
        let limits = Limits::default();
        assert!(limits.allow_get_sync());
        assert!(!limits.allow_get_sync());
    }

    #[test]
    fn payout_viable_requires_the_minimum_and_a_nonzero_output() {
        let min = config::min_deposit_amount().to_sat();
        assert!(payout_viable(min, 1));
        assert!(!payout_viable(min - 1, 1_000), "below-minimum deposits must not be paid out");
        assert!(!payout_viable(min, 0), "zero-output deposits must not be paid out");
    }

    #[test]
    fn pegout_payout_viable_requires_the_minimum() {
        let min = config::min_deposit_amount().to_sat();
        assert!(pegout_payout_viable(min));
        assert!(!pegout_payout_viable(min - 1));
    }

    #[test]
    fn short_ids_never_panic_on_short_input() {
        assert_eq!(short("abc"), "abc");
        assert_eq!(short("0123456789abcdef"), "01234567");
    }
}
