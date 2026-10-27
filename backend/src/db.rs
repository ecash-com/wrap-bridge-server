use anyhow::{Context, Result};
use bdk_wallet::bitcoin::Amount;
use rusqlite::types::{FromSql, Value, ValueRef};
use rusqlite::Params;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// `bridge_orders` tracks each ECX->wECX bridge order from creation through payout: the ECX
/// amount deposited, the wECX amount owed, the Solana recipient, and the deposit/payout
/// signatures once they exist. No AMM/price columns - unlike the old BTC/ECX swap this replaces,
/// the bridge is a flat 1:1 peg (see `convert::ecx_sat_to_wecx_base_units`), so `amount_out_*` is
/// always a deterministic function of `amount_in_sat`, never a live-market quote that can drift.
///
/// `id` is a random (v4) UUID, generated in application code with the `uuid` crate rather than a
/// SQL `DEFAULT` expression, since there's no auth on order lookups - a sequential id would let
/// anyone enumerate other users' orders just by incrementing it.
const SCHEMA: &str = "
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS bridge_order_statuses (
    code TEXT PRIMARY KEY
);
INSERT OR IGNORE INTO bridge_order_statuses (code) VALUES
    ('pending'),
    ('deposit_seen'),
    ('deposit_confirmed'),
    ('paying_out'),
    ('paid_out'),
    ('expired'),
    ('failed'),
    ('refund_requested'),
    ('refunding'),
    ('refunded');

CREATE TABLE IF NOT EXISTS bridge_orders (
    id TEXT PRIMARY KEY NOT NULL,
    amount_in_sat INTEGER NOT NULL CHECK (amount_in_sat > 0),
    amount_out_base_units INTEGER NOT NULL CHECK (amount_out_base_units >= 0),
    deposit_address TEXT NOT NULL,
    solana_recipient TEXT NOT NULL,
    refund_address TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'pending' REFERENCES bridge_order_statuses (code),
    deposit_txid TEXT,
    payout_signature TEXT,
    refund_txid TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    -- Wrap-only SOL stipend locked at order creation. 0 means the customer did not buy one.
    -- The price is extra ECX they must deposit; it is not converted into wrapped tokens.
    sol_stipend_lamports INTEGER NOT NULL DEFAULT 0 CHECK (sol_stipend_lamports >= 0),
    sol_stipend_price_sat INTEGER NOT NULL DEFAULT 0 CHECK (sol_stipend_price_sat >= 0),
    -- SOL rent for a recipient token account that did not exist when the order was created.
    -- Locked then; not repriced if the account appears or disappears before payout. 0 if it existed.
    ata_rent_lamports INTEGER NOT NULL DEFAULT 0 CHECK (ata_rent_lamports >= 0),
    -- Native-coin price of that rent, 1:1 and rounded up. Included in amount_in_sat. Not a wrap fee.
    ata_rent_price_sat INTEGER NOT NULL DEFAULT 0 CHECK (ata_rent_price_sat >= 0),
    -- Confirmations the deposit had at the last check, so the order page can show progress toward
    -- the required count. Not authoritative: the status transition is decided from the wallet.
    deposit_confirmations INTEGER NOT NULL DEFAULT 0 CHECK (deposit_confirmations >= 0),
    -- Why a refund was requested, for the customer. Empty until then.
    refund_reason TEXT NOT NULL DEFAULT '',
    -- Restocking fee kept when a formatted-wrong wrap is sent back, in sat. 0 until then.
    refund_fee_sat INTEGER NOT NULL DEFAULT 0 CHECK (refund_fee_sat >= 0)
);

CREATE INDEX IF NOT EXISTS bridge_orders_status_idx ON bridge_orders (status);

-- Each fresh ECX deposit address (see wallet::ChainWallet::fresh_address) is handed out to
-- exactly one order, so two concurrently pending orders never share a deposit target and become
-- ambiguous about which order a given deposit belongs to.
CREATE UNIQUE INDEX IF NOT EXISTS bridge_orders_deposit_address_uidx
    ON bridge_orders (deposit_address);

-- `pegout_orders` is the wECX->ECX mirror of `bridge_orders`. Solana has no analogue of a fresh
-- per-order receive address (every peg-out deposit lands in the same treasury ATA), so orders are
-- told apart by a `reference_pubkey` - a bare pubkey generated per order with no keypair kept,
-- included as an extra account in the customer's transfer per the Solana Pay convention (see
-- solana::SolanaWallet::find_reference_deposit). `depositor_owner` is extracted from that same
-- transaction (its wECX balance decreased) rather than asked of the customer. A formatted-wrong
-- deposit is sent back to that wallet. Wrap refunds do the same from the ECX deposit's inputs.
CREATE TABLE IF NOT EXISTS pegout_order_statuses (
    code TEXT PRIMARY KEY
);
INSERT OR IGNORE INTO pegout_order_statuses (code) VALUES
    ('pending'),
    ('deposit_seen'),
    ('deposit_confirmed'),
    ('paying_out'),
    ('paid_out'),
    ('expired'),
    ('failed'),
    ('refund_requested'),
    ('refunding'),
    ('refunded');

CREATE TABLE IF NOT EXISTS pegout_orders (
    id TEXT PRIMARY KEY NOT NULL,
    amount_in_base_units INTEGER NOT NULL CHECK (amount_in_base_units > 0),
    amount_out_sat INTEGER NOT NULL CHECK (amount_out_sat >= 0),
    ecx_recipient TEXT NOT NULL,
    reference_pubkey TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending' REFERENCES pegout_order_statuses (code),
    deposit_signature TEXT,
    depositor_owner TEXT,
    payout_txid TEXT,
    refund_signature TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    -- Restocking fee kept when a formatted-wrong unwrap is sent back. 0 until then.
    refund_fee_base_units INTEGER NOT NULL DEFAULT 0 CHECK (refund_fee_base_units >= 0)
);

CREATE INDEX IF NOT EXISTS pegout_orders_status_idx ON pegout_orders (status);

-- Mirrors `bridge_orders_deposit_address_uidx`: each reference key is generated fresh per order
-- (see server::create_pegout_order) and handed out to exactly one, so a deposit transaction can
-- never be ambiguous about which order it belongs to.
CREATE UNIQUE INDEX IF NOT EXISTS pegout_orders_reference_uidx
    ON pegout_orders (reference_pubkey);

CREATE TABLE IF NOT EXISTS earnings_sweeps (
    id TEXT PRIMARY KEY NOT NULL,
    asset TEXT NOT NULL CHECK (asset IN ('becx', 'wbecx')),
    amount INTEGER NOT NULL CHECK (amount > 0),
    destination TEXT NOT NULL,
    transaction_id TEXT NOT NULL,
    transaction_hex TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'completed', 'failed')),
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE UNIQUE INDEX IF NOT EXISTS earnings_sweeps_pending_asset
    ON earnings_sweeps (asset) WHERE status = 'pending';

-- Signed ECX transactions recorded before broadcast, so a crash or a lost RPC response retries
-- the same transaction instead of building a second one. Not part of the public order row.
CREATE TABLE IF NOT EXISTS outbound_txs (
    order_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('wrap_refund', 'unwrap_payout')),
    txid TEXT NOT NULL,
    tx_hex TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (order_id, kind)
);

-- wECX that reached the treasury token account in a transaction carrying no order's reference
-- key, so no order can claim it (see server::scan_unmatched_deposits_once). One row per deposit
-- transaction. `pending` and `refunding` rows are owed back and are reserved against the
-- treasury balance; `kept` rows are the operator's (treasury funding, or dust kept as the fee);
-- `held` rows wait for a person. The fee is earnings once the row is `refunded` or `kept`.
CREATE TABLE IF NOT EXISTS unmatched_deposit_statuses (
    code TEXT PRIMARY KEY
);
INSERT OR IGNORE INTO unmatched_deposit_statuses (code) VALUES
    ('pending'),
    ('refunding'),
    ('refunded'),
    ('held'),
    ('kept');

CREATE TABLE IF NOT EXISTS unmatched_deposits (
    signature TEXT PRIMARY KEY NOT NULL,
    amount_base_units INTEGER NOT NULL CHECK (amount_base_units > 0),
    depositor_owner TEXT,
    fee_base_units INTEGER NOT NULL CHECK (fee_base_units >= 0),
    refund_base_units INTEGER NOT NULL CHECK (refund_base_units >= 0),
    status TEXT NOT NULL REFERENCES unmatched_deposit_statuses (code),
    note TEXT NOT NULL DEFAULT '',
    refund_signature TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE INDEX IF NOT EXISTS unmatched_deposits_status_idx ON unmatched_deposits (status);

-- Where a background walk over an external history last stopped, by name.
CREATE TABLE IF NOT EXISTS scan_cursors (
    name TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL,
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);
";

/// SQLite integers are signed 64-bit; a bare `as i64` on a `u64` above `i64::MAX` silently goes
/// negative instead of failing, so every u64 -> column write goes through this.
fn to_i64(value: u64) -> Result<i64> {
    i64::try_from(value).context("amount does not fit in a signed 64-bit database column")
}

/// Local SQLite database file, shared by every task through one mutex-guarded connection. The
/// methods are `async` only so call sites stay uniform; each one takes the lock, runs the
/// statement synchronously and releases it before returning, so the lock is never held across an
/// `.await`. Queries are fully buffered into `Rows` for the same reason.
#[derive(Clone)]
pub struct Connection(Arc<Mutex<rusqlite::Connection>>);

pub struct Row(Vec<Value>);

impl Row {
    pub fn get<T: FromSql>(&self, idx: usize) -> Result<T> {
        let value = self.0.get(idx).context("column index out of range")?;
        Ok(T::column_result(ValueRef::from(value))?)
    }
}

pub struct Rows(std::vec::IntoIter<Row>);

impl Rows {
    pub async fn next(&mut self) -> Result<Option<Row>> {
        Ok(self.0.next())
    }
}

impl Connection {
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, rusqlite::Connection>> {
        self.0
            .lock()
            .map_err(|_| anyhow::anyhow!("database mutex poisoned"))
    }

    pub async fn execute<P: Params>(&self, sql: &str, params: P) -> Result<usize> {
        Ok(self.lock()?.execute(sql, params)?)
    }

    pub async fn execute_batch(&self, sql: &str) -> Result<()> {
        Ok(self.lock()?.execute_batch(sql)?)
    }

    pub async fn query<P: Params>(&self, sql: &str, params: P) -> Result<Rows> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(sql)?;
        let columns = stmt.column_count();
        let rows = stmt
            .query_map(params, |row| {
                (0..columns)
                    .map(|i| row.get::<_, Value>(i))
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map(Row)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Rows(rows.into_iter()))
    }
}

/// Opens (creating if needed) the SQLite file at `DATABASE_PATH` (default `data/bridge.db`) and
/// applies the schema. WAL + `synchronous = FULL` keeps every committed write durable, since
/// order data involves money.
pub async fn open() -> Result<Connection> {
    let path = std::env::var("DATABASE_PATH").unwrap_or_else(|_| "data/bridge.db".to_string());
    if let Some(parent) = std::path::Path::new(&path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create database directory {parent:?}"))?;
        }
    }

    let conn = rusqlite::Connection::open(&path)
        .with_context(|| format!("failed to open SQLite database at {path}"))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL;")
        .context("failed to configure SQLite")?;

    let conn = Connection(Arc::new(Mutex::new(conn)));
    conn.execute_batch(SCHEMA)
        .await
        .context("failed to apply bridge_orders schema")?;
    conn.migrate()
        .await
        .context("failed to migrate bridge_orders schema")?;

    Ok(conn)
}

impl Connection {
    /// Adds columns introduced after the first release. `CREATE TABLE IF NOT EXISTS` does not
    /// alter an existing file.
    async fn migrate(&self) -> Result<()> {
        self.ensure_column(
            "bridge_orders",
            "sol_stipend_lamports",
            "INTEGER NOT NULL DEFAULT 0",
        )
        .await?;
        self.ensure_column(
            "bridge_orders",
            "sol_stipend_price_sat",
            "INTEGER NOT NULL DEFAULT 0",
        )
        .await?;
        self.ensure_column(
            "bridge_orders",
            "ata_rent_lamports",
            "INTEGER NOT NULL DEFAULT 0",
        )
        .await?;
        self.ensure_column(
            "bridge_orders",
            "ata_rent_price_sat",
            "INTEGER NOT NULL DEFAULT 0",
        )
        .await?;
        self.ensure_column("bridge_orders", "deposit_confirmations", "INTEGER NOT NULL DEFAULT 0").await?;
        self.ensure_column("bridge_orders", "refund_reason", "TEXT NOT NULL DEFAULT ''").await?;
        self.ensure_column("bridge_orders", "refund_fee_sat", "INTEGER NOT NULL DEFAULT 0").await?;
        self.ensure_column(
            "pegout_orders",
            "refund_fee_base_units",
            "INTEGER NOT NULL DEFAULT 0",
        )
        .await?;
        Ok(())
    }

    async fn ensure_column(&self, table: &str, column: &str, decl: &str) -> Result<()> {
        // Older databases (and the migration fixture) may predate a later table. Schema creation
        // adds that table with the column already present; there is nothing to alter until then.
        let mut existing = self
            .query(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                (table,),
            )
            .await
            .with_context(|| format!("failed to look up {table}"))?;
        if existing.next().await?.is_none() {
            return Ok(());
        }
        let mut rows = self
            .query(&format!("PRAGMA table_info({table})"), ())
            .await
            .with_context(|| format!("failed to inspect {table}"))?;
        while let Some(row) = rows.next().await.context("failed to read table info")? {
            let name: String = row.get(1)?;
            if name == column {
                return Ok(());
            }
        }
        self.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"))
            .await
            .with_context(|| format!("failed to add {table}.{column}"))?;
        Ok(())
    }
}

/// Mirrors the `bridge_order_statuses` lookup table - kept as a Rust enum (rather than comparing
/// `Order.status` strings directly) so status-transition logic can't typo its way into a value the
/// DB's `FOREIGN KEY` would otherwise reject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderStatus {
    Pending,
    DepositSeen,
    DepositConfirmed,
    /// Transitional claim state: one poller has committed to sending the wECX payout but hasn't
    /// confirmed success yet. Exists so concurrent `GET /orders/{id}` polls can't both send - see
    /// `claim_for_payout`.
    PayingOut,
    PaidOut,
    Expired,
    Failed,
    /// Refund chosen (currently only by the poller, for a deposit whose repriced payout rounds to
    /// zero wECX - see `server::detect_deposit`), not yet sent. Mirrors what `DepositConfirmed`
    /// is to a payout.
    RefundRequested,
    /// Transitional claim state while a refund send is in flight - mirrors `PayingOut`. See
    /// `claim_for_refund`.
    Refunding,
    Refunded,
}

impl OrderStatus {
    pub fn label(self) -> &'static str {
        match self {
            OrderStatus::Pending => "pending",
            OrderStatus::DepositSeen => "deposit_seen",
            OrderStatus::DepositConfirmed => "deposit_confirmed",
            OrderStatus::PayingOut => "paying_out",
            OrderStatus::PaidOut => "paid_out",
            OrderStatus::Expired => "expired",
            OrderStatus::Failed => "failed",
            OrderStatus::RefundRequested => "refund_requested",
            OrderStatus::Refunding => "refunding",
            OrderStatus::Refunded => "refunded",
        }
    }

    pub fn from_label(label: &str) -> Result<OrderStatus> {
        Ok(match label {
            "pending" => OrderStatus::Pending,
            "deposit_seen" => OrderStatus::DepositSeen,
            "deposit_confirmed" => OrderStatus::DepositConfirmed,
            "paying_out" => OrderStatus::PayingOut,
            "paid_out" => OrderStatus::PaidOut,
            "expired" => OrderStatus::Expired,
            "failed" => OrderStatus::Failed,
            "refund_requested" => OrderStatus::RefundRequested,
            "refunding" => OrderStatus::Refunding,
            "refunded" => OrderStatus::Refunded,
            other => anyhow::bail!("unknown order status {other:?}"),
        })
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Order {
    pub id: String,
    pub amount_in_sat: i64,
    pub amount_out_base_units: i64,
    pub deposit_address: String,
    pub solana_recipient: String,
    /// Address the deposit came from, filled by the server when a formatted-wrong deposit is sent
    /// back. Never chosen by the customer. `None` until that address has been read from the
    /// deposit. Stored as `TEXT NOT NULL DEFAULT ''` (empty string means "not yet known").
    pub refund_address: Option<String>,
    pub status: String,
    pub deposit_txid: Option<String>,
    pub payout_signature: Option<String>,
    pub refund_txid: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    /// SOL the customer bought with this wrap, in lamports. 0 if they did not. Locked at creation;
    /// a short deposit is not sent this SOL (see `convert::settle_wrap_deposit`).
    pub sol_stipend_lamports: i64,
    /// Extra ECX, in sat, included in `amount_in_sat` to pay for the stipend. Not converted into
    /// wrapped tokens. A refund still returns the full `amount_in_sat`.
    pub sol_stipend_price_sat: i64,
    /// Lamports the payout will spend to create this recipient's token account. 0 if it already
    /// existed at order creation. Not repriced later.
    pub ata_rent_lamports: i64,
    /// Native-coin price of `ata_rent_lamports`, included in `amount_in_sat`. Not a wrap fee.
    pub ata_rent_price_sat: i64,
    /// Confirmations the deposit had when last checked. 0 until a deposit is seen.
    pub deposit_confirmations: i64,
    /// Why a refund was requested, in words meant for the customer. Empty otherwise.
    pub refund_reason: String,
}

pub struct NewOrder {
    pub amount_in_sat: u64,
    pub amount_out_base_units: u64,
    pub deposit_address: String,
    pub solana_recipient: String,
    pub sol_stipend_lamports: u64,
    pub sol_stipend_price_sat: u64,
    pub ata_rent_lamports: u64,
    pub ata_rent_price_sat: u64,
}

const ORDER_COLUMNS: &str = "id, amount_in_sat, amount_out_base_units, deposit_address, \
    solana_recipient, refund_address, status, deposit_txid, payout_signature, refund_txid, \
    created_at, updated_at, sol_stipend_lamports, sol_stipend_price_sat, \
    ata_rent_lamports, ata_rent_price_sat, deposit_confirmations, refund_reason";

fn row_to_order(row: &Row) -> Result<Order> {
    let refund_address: String = row.get(5)?;
    let refund_address = if refund_address.is_empty() {
        None
    } else {
        Some(refund_address)
    };

    Ok(Order {
        id: row.get(0)?,
        amount_in_sat: row.get(1)?,
        amount_out_base_units: row.get(2)?,
        deposit_address: row.get(3)?,
        solana_recipient: row.get(4)?,
        refund_address,
        status: row.get(6)?,
        deposit_txid: row.get(7)?,
        payout_signature: row.get(8)?,
        refund_txid: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
        sol_stipend_lamports: row.get(12)?,
        sol_stipend_price_sat: row.get(13)?,
        ata_rent_lamports: row.get(14)?,
        ata_rent_price_sat: row.get(15)?,
        deposit_confirmations: row.get(16)?,
        refund_reason: row.get(17)?,
    })
}

/// Inserts a new order with a random (v4) UUID id and returns the row as stored (including the
/// defaulted `status`/`created_at`/`updated_at` columns).
pub async fn insert_order(conn: &Connection, order: &NewOrder) -> Result<Order> {
    let id = Uuid::new_v4().to_string();
    let amount_in_sat = to_i64(order.amount_in_sat)?;
    let amount_out_base_units = to_i64(order.amount_out_base_units)?;
    let sol_stipend_lamports = to_i64(order.sol_stipend_lamports)?;
    let sol_stipend_price_sat = to_i64(order.sol_stipend_price_sat)?;
    let ata_rent_lamports = to_i64(order.ata_rent_lamports)?;
    let ata_rent_price_sat = to_i64(order.ata_rent_price_sat)?;

    conn.execute(
        "INSERT INTO bridge_orders \
            (id, amount_in_sat, amount_out_base_units, deposit_address, solana_recipient, \
             refund_address, sol_stipend_lamports, sol_stipend_price_sat, \
             ata_rent_lamports, ata_rent_price_sat) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (
            id.as_str(),
            amount_in_sat,
            amount_out_base_units,
            order.deposit_address.as_str(),
            order.solana_recipient.as_str(),
            "",
            sol_stipend_lamports,
            sol_stipend_price_sat,
            ata_rent_lamports,
            ata_rent_price_sat,
        ),
    )
    .await
    .context("failed to insert bridge order")?;

    get_order(conn, &id)
        .await?
        .context("just-inserted bridge order not found")
}

pub async fn get_order(conn: &Connection, id: &str) -> Result<Option<Order>> {
    let mut rows = conn
        .query(
            &format!("SELECT {ORDER_COLUMNS} FROM bridge_orders WHERE id = ?"),
            (id,),
        )
        .await
        .context("failed to query bridge order")?;

    let Some(row) = rows.next().await.context("failed to read bridge order row")? else {
        return Ok(None);
    };

    Ok(Some(row_to_order(&row)?))
}

/// Rows for the background poller: orders still awaiting a deposit, payout, or refund - i.e.
/// everything `check_deposit` (deposit detection + `try_fulfill` + `try_refund`) or the
/// max-age expiry check in `poll_orders_once` can still act on. Includes `paying_out` so an order
/// whose payout outcome was ambiguous (see `server::try_resolve_payout`) gets resolved instead of
/// sitting there forever.
pub async fn list_active_orders(conn: &Connection) -> Result<Vec<Order>> {
    let mut rows = conn
        .query(
            &format!(
                "SELECT {ORDER_COLUMNS} FROM bridge_orders \
                 WHERE status IN ('pending', 'deposit_seen', 'deposit_confirmed', \
                                   'paying_out', 'refund_requested', 'refunding')"
            ),
            (),
        )
        .await
        .context("failed to query active bridge orders")?;

    let mut orders = Vec::new();
    while let Some(row) = rows.next().await.context("failed to read bridge order row")? {
        orders.push(row_to_order(&row)?);
    }
    Ok(orders)
}

/// Number of orders still awaiting a deposit - used to cap how many deposit addresses (and
/// therefore poller sync work) unauthenticated `POST /orders` calls can pile up.
pub async fn count_pending_orders(conn: &Connection) -> Result<u64> {
    let mut rows = conn
        .query(
            "SELECT COUNT(*) FROM bridge_orders WHERE status = ?",
            (OrderStatus::Pending.label(),),
        )
        .await
        .context("failed to count pending bridge orders")?;
    let count: i64 = match rows.next().await.context("failed to read pending order count")? {
        Some(row) => row.get(0)?,
        None => 0,
    };
    Ok(count.max(0) as u64)
}

async fn completed_order_amounts(
    conn: &Connection,
    sql: &str,
    status: &str,
    context: &'static str,
) -> Result<Vec<(u64, u64)>> {
    let mut rows = conn.query(sql, (status,)).await.context(context)?;
    let mut amounts = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .context("failed to read completed order amounts")?
    {
        let amount_in: i64 = row.get(0)?;
        let amount_out: i64 = row.get(1)?;
        amounts.push((
            u64::try_from(amount_in)
                .context("completed order has a negative input amount")?,
            u64::try_from(amount_out)
                .context("completed order has a negative output amount")?,
        ));
    }
    Ok(amounts)
}

pub struct EarningsSweep {
    pub id: String,
    pub asset: String,
    pub amount: u64,
    pub destination: String,
    pub transaction_id: String,
    pub transaction_hex: String,
}

pub async fn pending_earnings_sweeps(conn: &Connection) -> Result<Vec<EarningsSweep>> {
    let mut rows = conn.query(
        "SELECT id, asset, amount, destination, transaction_id, transaction_hex \
         FROM earnings_sweeps WHERE status = 'pending' ORDER BY created_at, id", (),
    ).await?;
    let mut sweeps = Vec::new();
    while let Some(row) = rows.next().await? {
        let amount: i64 = row.get(2)?;
        sweeps.push(EarningsSweep {
            id: row.get(0)?, asset: row.get(1)?,
            amount: u64::try_from(amount)?, destination: row.get(3)?,
            transaction_id: row.get(4)?, transaction_hex: row.get(5)?,
        });
    }
    Ok(sweeps)
}

pub async fn allocated_earnings(conn: &Connection, asset: &str) -> Result<u64> {
    let mut rows = conn.query(
        "SELECT COALESCE(SUM(amount), 0) FROM earnings_sweeps \
         WHERE asset = ? AND status IN ('pending', 'completed')", (asset,),
    ).await?;
    let amount: i64 = rows.next().await?.context("missing earnings total")?.get(0)?;
    Ok(u64::try_from(amount)?)
}

pub async fn paid_earnings(conn: &Connection) -> Result<(u64, u64)> {
    let mut rows = conn
        .query("SELECT asset, amount FROM earnings_sweeps WHERE status = 'completed'", ())
        .await?;
    let mut becx = 0u64;
    let mut wbecx = 0u64;
    while let Some(row) = rows.next().await? {
        let asset: String = row.get(0)?;
        let amount: i64 = row.get(1)?;
        let total = match asset.as_str() {
            "becx" => &mut becx,
            "wbecx" => &mut wbecx,
            _ => anyhow::bail!("unknown earnings asset"),
        };
        *total = total.checked_add(u64::try_from(amount)?)
            .context("paid earnings total overflow")?;
    }
    Ok((becx, wbecx))
}

pub async fn customer_earnings_reserve(conn: &Connection, asset: &str) -> Result<u64> {
    anyhow::ensure!(matches!(asset, "becx" | "wbecx"), "unknown earnings asset");
    let (wrap_amount, unwrap_amount) = if asset == "becx" {
        ("amount_in_sat", "amount_out_sat")
    } else {
        ("amount_out_base_units", "amount_in_base_units")
    };
    // Refunds owed for reference-less deposits are wECX obligations too.
    let unmatched = if asset == "wbecx" {
        "UNION ALL SELECT refund_base_units AS amount FROM unmatched_deposits WHERE status IN ('pending', 'refunding')"
    } else {
        ""
    };
    let mut rows = conn.query(
        &format!("SELECT COALESCE(SUM(amount), 0) FROM (
            SELECT {wrap_amount} AS amount FROM bridge_orders
            WHERE status NOT IN ('paid_out', 'refunded') AND
                (deposit_txid IS NOT NULL OR status IN ('deposit_seen', 'deposit_confirmed', 'paying_out', 'refund_requested', 'refunding'))
            UNION ALL
            SELECT {unwrap_amount} AS amount FROM pegout_orders
            WHERE status NOT IN ('paid_out', 'refunded') AND
                (deposit_signature IS NOT NULL OR status IN ('deposit_seen', 'deposit_confirmed', 'paying_out', 'refund_requested', 'refunding'))
            {unmatched}
         )"), (),
    ).await?;
    let amount: i64 = rows.next().await?.context("missing customer reserve result")?.get(0)?;
    Ok(u64::try_from(amount)?)
}

pub async fn unsettled_deposit_addresses(conn: &Connection) -> Result<Vec<String>> {
    let mut rows = conn.query(
        "SELECT deposit_address FROM bridge_orders WHERE status NOT IN ('paid_out', 'refunded')", (),
    ).await?;
    let mut addresses = Vec::new();
    while let Some(row) = rows.next().await? {
        addresses.push(row.get(0)?);
    }
    Ok(addresses)
}

pub async fn record_earnings_sweep(conn: &Connection, sweep: &EarningsSweep) -> Result<()> {
    conn.execute(
        "INSERT INTO earnings_sweeps \
         (id, asset, amount, destination, transaction_id, transaction_hex) VALUES (?, ?, ?, ?, ?, ?)",
        (sweep.id.as_str(), sweep.asset.as_str(), to_i64(sweep.amount)?,
         sweep.destination.as_str(), sweep.transaction_id.as_str(), sweep.transaction_hex.as_str()),
    ).await?;
    Ok(())
}

pub async fn finish_earnings_sweep(conn: &Connection, id: &str, completed: bool) -> Result<()> {
    conn.execute(
        "UPDATE earnings_sweeps SET status = ?, updated_at = unixepoch() WHERE id = ? AND status = 'pending'",
        (if completed { "completed" } else { "failed" }, id),
    ).await?;
    Ok(())
}

#[derive(Debug)]
pub struct RecentOrder {
    pub direction: String,
    pub amount_in: u64,
    pub amount_out: u64,
    pub status: String,
    pub created_at: i64,
}

/// Most recent orders from both bridge directions, without customer or credential fields.
pub async fn recent_orders(conn: &Connection, limit: u64) -> Result<Vec<RecentOrder>> {
    let limit = i64::try_from(limit).context("recent order limit does not fit in SQLite integer")?;
    let mut rows = conn
        .query(
            "SELECT direction, amount_in, amount_out, status, created_at FROM (
                SELECT 'wrap' AS direction, amount_in_sat AS amount_in,
                       amount_out_base_units AS amount_out, status, created_at
                FROM bridge_orders
                UNION ALL
                SELECT 'unwrap' AS direction, amount_in_base_units AS amount_in,
                       amount_out_sat AS amount_out, status, created_at
                FROM pegout_orders
             ) ORDER BY created_at DESC LIMIT ?",
            (limit,),
        )
        .await
        .context("failed to query recent bridge orders")?;

    let mut orders = Vec::new();
    while let Some(row) = rows.next().await.context("failed to read recent bridge orders")? {
        let amount_in: i64 = row.get(1)?;
        let amount_out: i64 = row.get(2)?;
        orders.push(RecentOrder {
            direction: row.get(0)?,
            amount_in: u64::try_from(amount_in).context("recent order has a negative input amount")?,
            amount_out: u64::try_from(amount_out).context("recent order has a negative output amount")?,
            status: row.get(3)?,
            created_at: row.get(4)?,
        });
    }
    Ok(orders)
}

/// Input/output amounts for completed peg-ins only; incomplete and refunded orders are not trades.
pub async fn completed_wrap_amounts(conn: &Connection) -> Result<Vec<(u64, u64)>> {
    completed_order_amounts(
        conn,
        "SELECT MAX(amount_in_sat - sol_stipend_price_sat - ata_rent_price_sat, 0), \
                amount_out_base_units \
         FROM bridge_orders WHERE status = ?",
        OrderStatus::PaidOut.label(),
        "failed to query completed wraps",
    )
    .await
}

/// Input/output amounts for completed peg-outs only; incomplete and refunded orders are not trades.
pub async fn completed_unwrap_amounts(conn: &Connection) -> Result<Vec<(u64, u64)>> {
    completed_order_amounts(
        conn,
        "SELECT amount_in_base_units, amount_out_sat FROM pegout_orders WHERE status = ?",
        PegoutOrderStatus::PaidOut.label(),
        "failed to query completed unwraps",
    )
    .await
}

/// Marks an order `expired`, but only while it is still `pending`. A `deposit_seen` row already
/// has customer funds in view and must not be dropped. Conditional so a poll that raced a deposit
/// detection cannot clobber that newer state. Returns `true` iff this call actually expired the row.
pub async fn expire_order(conn: &Connection, id: &str) -> Result<bool> {
    let rows_affected = conn
        .execute(
            "UPDATE bridge_orders SET status = ?, updated_at = unixepoch() \
             WHERE id = ? AND status = ?",
            (OrderStatus::Expired.label(), id, OrderStatus::Pending.label()),
        )
        .await
        .context("failed to expire bridge order")?;
    Ok(rows_affected > 0)
}

/// Expired wrap orders still inside the late-deposit grace window, newest first.
pub async fn list_expired_orders_since(conn: &Connection, since_unix: i64, limit: u64) -> Result<Vec<Order>> {
    let mut rows = conn
        .query(
            &format!(
                "SELECT {ORDER_COLUMNS} FROM bridge_orders \
                 WHERE status = 'expired' AND updated_at >= ? \
                 ORDER BY updated_at DESC LIMIT ?"
            ),
            (since_unix, i64::try_from(limit).unwrap_or(i64::MAX)),
        )
        .await
        .context("failed to query expired bridge orders")?;
    let mut orders = Vec::new();
    while let Some(row) = rows.next().await.context("failed to read expired bridge order")? {
        orders.push(row_to_order(&row)?);
    }
    Ok(orders)
}

/// Moves an expired wrap order back to `pending` so a late deposit can be recorded. No-op if the
/// row is no longer expired.
pub async fn revive_expired_order(conn: &Connection, id: &str) -> Result<bool> {
    let rows_affected = conn
        .execute(
            "UPDATE bridge_orders SET status = ?, updated_at = unixepoch() \
             WHERE id = ? AND status = ?",
            (OrderStatus::Pending.label(), id, OrderStatus::Expired.label()),
        )
        .await
        .context("failed to revive expired bridge order")?;
    Ok(rows_affected > 0)
}

pub async fn record_outbound_tx(conn: &Connection, order_id: &str, kind: &str, txid: &str, tx_hex: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO outbound_txs (order_id, kind, txid, tx_hex) VALUES (?, ?, ?, ?)",
        (order_id, kind, txid, tx_hex),
    )
    .await
    .context("failed to record outbound transaction")?;
    Ok(())
}

pub async fn outbound_tx_hex(conn: &Connection, order_id: &str, kind: &str) -> Result<Option<String>> {
    let mut rows = conn
        .query(
            "SELECT tx_hex FROM outbound_txs WHERE order_id = ? AND kind = ?",
            (order_id, kind),
        )
        .await
        .context("failed to query outbound transaction")?;
    let Some(row) = rows.next().await.context("failed to read outbound transaction")? else {
        return Ok(None);
    };
    Ok(Some(row.get(0)?))
}

/// `amount_in_sat` is written here too, not just `status`/`deposit_txid` - it's set to whatever
/// actually landed at the deposit address, which may differ from the amount quoted at order
/// creation. Keeping it in sync is what makes every downstream consumer of `amount_in_sat`
/// (refunds, `reserved_wecx_amount`) act on the real deposit rather than a stale quote.
///
/// `amount_out_base_units` is corrected in the same write, always paired with `amount_in_sat` so a
/// wrong-amount deposit's `status` transition and its repriced payout land atomically.
///
/// Conditional on the order still being `pending`/`deposit_seen`. `server::detect_deposit` decides
/// what to write from an in-memory snapshot that can be arbitrarily stale (a concurrent
/// `GET /orders/{id}`, or the poller working through a batch, may have already advanced the order);
/// an unconditional write here used to be able to drag a `paying_out`/`paid_out` order back to
/// `deposit_confirmed`, which `claim_for_payout` would then happily claim a second time. Returns
/// `true` iff this call won the write - on `false`, the caller must re-read the order rather than
/// act on its snapshot.
pub async fn update_deposit(
    conn: &Connection,
    id: &str,
    status: OrderStatus,
    deposit_txid: &str,
    amount_in_sat: Amount,
    amount_out_base_units: u64,
    confirmations: u32,
) -> Result<bool> {
    let rows_affected = conn
        .execute(
            "UPDATE bridge_orders SET status = ?, deposit_txid = ?, amount_in_sat = ?, \
             amount_out_base_units = ?, deposit_confirmations = ?, updated_at = unixepoch() \
             WHERE id = ? AND status IN (?, ?)",
            (
                status.label(),
                deposit_txid,
                to_i64(amount_in_sat.to_sat())?,
                to_i64(amount_out_base_units)?,
                i64::from(confirmations),
                id,
                OrderStatus::Pending.label(),
                OrderStatus::DepositSeen.label(),
            ),
        )
        .await
        .context("failed to update bridge order deposit status")?;
    Ok(rows_affected > 0)
}

/// Sum of `amount_out_base_units` for orders not yet paid out but already a real claim on the
/// treasury's wECX balance: `deposit_seen` (a deposit is visible on-chain, even if unconfirmed)
/// through `paying_out` (claimed but not yet confirmed sent). Excludes `pending` (no deposit yet,
/// nothing real to protect against) and any terminal status. Used to keep `create_order` and
/// `detect_deposit`'s repricing from promising wECX that's already committed to other in-flight
/// orders.
/// SOL promised to wrap customers for a stipend or a new token account, not yet paid or expired.
/// Includes `pending`, so the bridge cannot promise more SOL than the treasury can spare.
pub async fn reserved_customer_sol_lamports(conn: &Connection) -> Result<u64> {
    let mut rows = conn
        .query(
            "SELECT COALESCE(SUM(sol_stipend_lamports + ata_rent_lamports), 0) FROM bridge_orders \
             WHERE status IN (?, ?, ?, ?) AND (sol_stipend_lamports > 0 OR ata_rent_lamports > 0)",
            (
                OrderStatus::Pending.label(),
                OrderStatus::DepositSeen.label(),
                OrderStatus::DepositConfirmed.label(),
                OrderStatus::PayingOut.label(),
            ),
        )
        .await
        .context("failed to query reserved customer SOL")?;
    let lamports: i64 = match rows.next().await.context("failed to read reserved SOL stipend")? {
        Some(row) => row.get(0)?,
        None => 0,
    };
    u64::try_from(lamports).context("reserved SOL stipend is negative")
}

pub async fn reserved_wecx_amount(conn: &Connection) -> Result<u64> {
    let mut rows = conn
        .query(
            "SELECT COALESCE(SUM(amount_out_base_units), 0) FROM bridge_orders \
             WHERE status IN (?, ?, ?)",
            (
                OrderStatus::DepositSeen.label(),
                OrderStatus::DepositConfirmed.label(),
                OrderStatus::PayingOut.label(),
            ),
        )
        .await
        .context("failed to query reserved wECX amount")?;
    let sat: i64 = match rows.next().await.context("failed to read reserved wECX amount row")? {
        Some(row) => row.get(0)?,
        None => 0,
    };
    let pending_sweeps = pending_earnings_sweeps(conn).await?;
    let reserved = pending_sweeps.iter().filter(|sweep| sweep.asset == "wbecx")
        .try_fold(u64::try_from(sat)?, |reserved, sweep| {
            reserved.checked_add(sweep.amount).context("reserved wbECX amount overflow")
        })?;
    reserved
        .checked_add(reserved_unmatched_refunds(conn).await?)
        .context("reserved wbECX amount overflow")
}

/// Atomically claims an order for payout by transitioning `deposit_confirmed -> paying_out`.
/// Returns `true` iff this call won the claim (i.e. the row was still `deposit_confirmed`) - the
/// caller should only proceed to send funds when this is `true`. Concurrent `GET /orders/{id}`
/// polls calling this racily is exactly what this guards against.
pub async fn claim_for_payout(conn: &Connection, id: &str) -> Result<bool> {
    let rows_affected = conn
        .execute(
            "UPDATE bridge_orders SET status = ?, updated_at = unixepoch() \
             WHERE id = ? AND status = ?",
            (
                OrderStatus::PayingOut.label(),
                id,
                OrderStatus::DepositConfirmed.label(),
            ),
        )
        .await
        .context("failed to claim bridge order for payout")?;
    Ok(rows_affected > 0)
}

/// Records the signature of the payout transaction *before* it is broadcast, while the order is
/// still `paying_out`. The signature is deterministic once the transaction is signed, so writing it
/// first means an ambiguous send outcome (RPC timeout mid-confirmation, process crash) leaves a
/// durable handle to look the transaction up by, instead of a bare `paying_out` row nobody can
/// distinguish from "never sent" - see `server::try_resolve_payout`. Also bumps `updated_at`, which
/// that resolver uses as the "attempt started" time. Returns `true` iff the order was still
/// `paying_out`.
pub async fn record_payout_signature(conn: &Connection, id: &str, payout_signature: &str) -> Result<bool> {
    let rows_affected = conn
        .execute(
            "UPDATE bridge_orders SET payout_signature = ?, updated_at = unixepoch() \
             WHERE id = ? AND status = ?",
            (payout_signature, id, OrderStatus::PayingOut.label()),
        )
        .await
        .context("failed to record bridge order payout signature")?;
    Ok(rows_affected > 0)
}

/// Marks a claimed order as paid out with its Solana payout transaction signature. Conditional on
/// the order still being `paying_out`; returns `true` iff this call made the transition.
pub async fn update_payout(conn: &Connection, id: &str, payout_signature: &str) -> Result<bool> {
    let rows_affected = conn
        .execute(
            "UPDATE bridge_orders SET status = ?, payout_signature = ?, updated_at = unixepoch() \
             WHERE id = ? AND status = ?",
            (
                OrderStatus::PaidOut.label(),
                payout_signature,
                id,
                OrderStatus::PayingOut.label(),
            ),
        )
        .await
        .context("failed to update bridge order payout")?;
    Ok(rows_affected > 0)
}

/// Reverts a `paying_out` claim back to `deposit_confirmed` so the next poll retries, and clears
/// any recorded payout signature. Only for outcomes where the payout is *known* not to have
/// happened (never broadcast, landed and failed on-chain, or its blockhash expired unconfirmed) -
/// never for an ambiguous send error, where the transaction may still land.
pub async fn revert_claim(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE bridge_orders SET status = ?, payout_signature = NULL, updated_at = unixepoch() \
         WHERE id = ? AND status = ?",
        (
            OrderStatus::DepositConfirmed.label(),
            id,
            OrderStatus::PayingOut.label(),
        ),
    )
    .await
    .context("failed to revert bridge order payout claim")?;
    Ok(())
}

/// Routes a confirmed deposit to a refund instead of a payout. The reason is the sentence shown
/// to the customer. Returns `true` iff this call won the transition.
pub async fn request_refund(conn: &Connection, id: &str, reason: &str) -> Result<bool> {
    let rows_affected = conn
        .execute(
            "UPDATE bridge_orders SET status = ?, refund_reason = ?, updated_at = unixepoch() \
             WHERE id = ? AND status = ?",
            (
                OrderStatus::RefundRequested.label(),
                reason,
                id,
                OrderStatus::DepositConfirmed.label(),
            ),
        )
        .await
        .context("failed to request bridge order refund")?;
    Ok(rows_affected > 0)
}

/// Records the address a refund will be sent to. Only fills a blank, so two polls cannot replace
/// a destination already chosen from the deposit. Returns `true` iff this call won the write.
pub async fn set_refund_address(conn: &Connection, id: &str, refund_address: &str) -> Result<bool> {
    let rows_affected = conn
        .execute(
            "UPDATE bridge_orders SET refund_address = ?, updated_at = unixepoch() \
             WHERE id = ? AND refund_address = ''",
            (refund_address, id),
        )
        .await
        .context("failed to set bridge order refund address")?;
    Ok(rows_affected > 0)
}

/// Atomically claims an order for refund by transitioning `refund_requested -> refunding`.
/// Mirrors `claim_for_payout` - returns `true` iff this call won the claim, guarding against two
/// concurrent polls both sending the refund.
pub async fn claim_for_refund(conn: &Connection, id: &str) -> Result<bool> {
    let rows_affected = conn
        .execute(
            "UPDATE bridge_orders SET status = ?, updated_at = unixepoch() \
             WHERE id = ? AND status = ?",
            (
                OrderStatus::Refunding.label(),
                id,
                OrderStatus::RefundRequested.label(),
            ),
        )
        .await
        .context("failed to claim bridge order for refund")?;
    Ok(rows_affected > 0)
}

/// Marks a claimed order as refunded with its ECX refund txid and the fee kept in this wallet.
pub async fn update_refund(conn: &Connection, id: &str, refund_txid: &str, fee_sat: u64) -> Result<()> {
    conn.execute(
        "UPDATE bridge_orders SET status = ?, refund_txid = ?, refund_fee_sat = ?, \
         updated_at = unixepoch() WHERE id = ? AND status = ?",
        (
            OrderStatus::Refunded.label(),
            refund_txid,
            to_i64(fee_sat)?,
            id,
            OrderStatus::Refunding.label(),
        ),
    )
    .await
    .context("failed to update bridge order refund")?;
    Ok(())
}

/// The deposit was too small to send back after the restocking fee. Keeps it and closes the order.
/// Returns `true` iff the order was still `refund_requested`.
pub async fn finish_refund_kept(conn: &Connection, id: &str, fee_sat: u64, reason: &str) -> Result<bool> {
    let rows = conn
        .execute(
            "UPDATE bridge_orders SET status = ?, refund_fee_sat = ?, refund_reason = ?, \
             updated_at = unixepoch() WHERE id = ? AND status = ?",
            (
                OrderStatus::Refunded.label(),
                to_i64(fee_sat)?,
                reason,
                id,
                OrderStatus::RefundRequested.label(),
            ),
        )
        .await
        .context("failed to keep a wrap refund as the fee")?;
    Ok(rows > 0)
}

/// Restocking fees kept on completed wrap refunds, as `(fee, 0)` so they add into wrap-side
/// earnings the same way completed orders do.
pub async fn wrap_restocking_fees(conn: &Connection) -> Result<Vec<(u64, u64)>> {
    let mut rows = conn
        .query(
            "SELECT refund_fee_sat FROM bridge_orders WHERE status = ? AND refund_fee_sat > 0",
            (OrderStatus::Refunded.label(),),
        )
        .await
        .context("failed to query wrap restocking fees")?;
    let mut fees = Vec::new();
    while let Some(row) = rows.next().await? {
        fees.push((u64::try_from(row.get::<i64>(0)?)?, 0));
    }
    Ok(fees)
}

/// Reverts a `refunding` claim back to `refund_requested` so the next poll retries - used when the
/// refund send fails, or the live-payout safety gate blocks it (a refund is just as real an
/// automatic outbound send as a payout).
pub async fn revert_refund_claim(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE bridge_orders SET status = ?, updated_at = unixepoch() WHERE id = ? AND status = ?",
        (
            OrderStatus::RefundRequested.label(),
            id,
            OrderStatus::Refunding.label(),
        ),
    )
    .await
    .context("failed to revert bridge order refund claim")?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// pegout_orders: the wECX->ECX mirror of everything above. Same state-machine shape as
// `bridge_orders` (see `PegoutOrderStatus`), with the roles of the two chains swapped: the
// deposit lands on Solana (matched via `reference_pubkey`, see
// `solana::SolanaWallet::find_reference_deposit`, rather than a fresh per-order address) and the
// payout is an ECX send; a refund (if the deposit can't be bridged) sends wECX back on Solana
// instead of ECX. See `server::check_pegout_deposit` for the orchestration this supports.
// ---------------------------------------------------------------------------------------------

/// Mirrors `OrderStatus` for the peg-out direction - see that type's doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PegoutOrderStatus {
    Pending,
    DepositSeen,
    DepositConfirmed,
    /// Transitional claim state for an in-flight ECX payout - mirrors `OrderStatus::PayingOut`.
    /// Unlike the Solana-side payout in `bridge_orders`, an ECX send in this codebase is a single
    /// synchronous sign-and-broadcast call (see `wallet::ChainWallet::send`) with no ambiguous
    /// "unknown outcome" case to resolve later, so there's no `paying_out`-specific resolver
    /// mirroring `server::try_resolve_payout` here.
    PayingOut,
    PaidOut,
    Expired,
    Failed,
    /// Deposit confirmed but not worth paying out (see `server::pegout_payout_viable`) - routed to
    /// a wECX refund using the `depositor_owner` extracted from the deposit transaction itself.
    /// The customer is not asked for a destination.
    RefundRequested,
    /// Transitional claim state while a wECX refund send is in flight - mirrors
    /// `OrderStatus::Refunding`. This one *is* two-phase like the peg-in payout (see
    /// `record_pegout_refund_signature`), because it's a Solana send with the same ambiguous-RPC-
    /// outcome problem `server::try_resolve_payout` exists for.
    Refunding,
    Refunded,
}

impl PegoutOrderStatus {
    pub fn label(self) -> &'static str {
        match self {
            PegoutOrderStatus::Pending => "pending",
            PegoutOrderStatus::DepositSeen => "deposit_seen",
            PegoutOrderStatus::DepositConfirmed => "deposit_confirmed",
            PegoutOrderStatus::PayingOut => "paying_out",
            PegoutOrderStatus::PaidOut => "paid_out",
            PegoutOrderStatus::Expired => "expired",
            PegoutOrderStatus::Failed => "failed",
            PegoutOrderStatus::RefundRequested => "refund_requested",
            PegoutOrderStatus::Refunding => "refunding",
            PegoutOrderStatus::Refunded => "refunded",
        }
    }

    pub fn from_label(label: &str) -> Result<PegoutOrderStatus> {
        Ok(match label {
            "pending" => PegoutOrderStatus::Pending,
            "deposit_seen" => PegoutOrderStatus::DepositSeen,
            "deposit_confirmed" => PegoutOrderStatus::DepositConfirmed,
            "paying_out" => PegoutOrderStatus::PayingOut,
            "paid_out" => PegoutOrderStatus::PaidOut,
            "expired" => PegoutOrderStatus::Expired,
            "failed" => PegoutOrderStatus::Failed,
            "refund_requested" => PegoutOrderStatus::RefundRequested,
            "refunding" => PegoutOrderStatus::Refunding,
            "refunded" => PegoutOrderStatus::Refunded,
            other => anyhow::bail!("unknown pegout order status {other:?}"),
        })
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PegoutOrder {
    pub id: String,
    pub amount_in_base_units: i64,
    pub amount_out_sat: i64,
    pub ecx_recipient: String,
    pub reference_pubkey: String,
    pub status: String,
    pub deposit_signature: Option<String>,
    /// Solana wallet owner pubkey the deposit's wECX balance decrease was attributed to (see
    /// `solana::SolanaWallet::find_reference_deposit`) - where a refund goes if this deposit can't
    /// be bridged. `None` if it couldn't be determined from the transaction (e.g. an exotic
    /// multisig source) - such an order can't be auto-refunded, only reviewed manually.
    pub depositor_owner: Option<String>,
    pub payout_txid: Option<String>,
    pub refund_signature: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

pub struct NewPegoutOrder {
    pub amount_in_base_units: u64,
    pub amount_out_sat: u64,
    pub ecx_recipient: String,
    pub reference_pubkey: String,
}

const PEGOUT_ORDER_COLUMNS: &str = "id, amount_in_base_units, amount_out_sat, ecx_recipient, \
    reference_pubkey, status, deposit_signature, depositor_owner, payout_txid, refund_signature, \
    created_at, updated_at";

fn row_to_pegout_order(row: &Row) -> Result<PegoutOrder> {
    Ok(PegoutOrder {
        id: row.get(0)?,
        amount_in_base_units: row.get(1)?,
        amount_out_sat: row.get(2)?,
        ecx_recipient: row.get(3)?,
        reference_pubkey: row.get(4)?,
        status: row.get(5)?,
        deposit_signature: row.get(6)?,
        depositor_owner: row.get(7)?,
        payout_txid: row.get(8)?,
        refund_signature: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
    })
}

pub async fn insert_pegout_order(conn: &Connection, order: &NewPegoutOrder) -> Result<PegoutOrder> {
    let id = Uuid::new_v4().to_string();
    let amount_in_base_units = to_i64(order.amount_in_base_units)?;
    let amount_out_sat = to_i64(order.amount_out_sat)?;

    conn.execute(
        "INSERT INTO pegout_orders \
            (id, amount_in_base_units, amount_out_sat, ecx_recipient, reference_pubkey) \
         VALUES (?, ?, ?, ?, ?)",
        (
            id.as_str(),
            amount_in_base_units,
            amount_out_sat,
            order.ecx_recipient.as_str(),
            order.reference_pubkey.as_str(),
        ),
    )
    .await
    .context("failed to insert pegout order")?;

    get_pegout_order(conn, &id)
        .await?
        .context("just-inserted pegout order not found")
}

pub async fn get_pegout_order(conn: &Connection, id: &str) -> Result<Option<PegoutOrder>> {
    let mut rows = conn
        .query(
            &format!("SELECT {PEGOUT_ORDER_COLUMNS} FROM pegout_orders WHERE id = ?"),
            (id,),
        )
        .await
        .context("failed to query pegout order")?;

    let Some(row) = rows.next().await.context("failed to read pegout order row")? else {
        return Ok(None);
    };

    Ok(Some(row_to_pegout_order(&row)?))
}

/// Rows for the background poller - mirrors `list_active_orders`.
pub async fn list_active_pegout_orders(conn: &Connection) -> Result<Vec<PegoutOrder>> {
    let mut rows = conn
        .query(
            &format!(
                "SELECT {PEGOUT_ORDER_COLUMNS} FROM pegout_orders \
                 WHERE status IN ('pending', 'deposit_seen', 'deposit_confirmed', \
                                   'paying_out', 'refund_requested', 'refunding')"
            ),
            (),
        )
        .await
        .context("failed to query active pegout orders")?;

    let mut orders = Vec::new();
    while let Some(row) = rows.next().await.context("failed to read pegout order row")? {
        orders.push(row_to_pegout_order(&row)?);
    }
    Ok(orders)
}

/// Mirrors `count_pending_orders` - caps how many reference keys (and therefore poller RPC calls)
/// unauthenticated `POST /pegout/orders` calls can pile up.
pub async fn count_pending_pegout_orders(conn: &Connection) -> Result<u64> {
    let mut rows = conn
        .query(
            "SELECT COUNT(*) FROM pegout_orders WHERE status = ?",
            (PegoutOrderStatus::Pending.label(),),
        )
        .await
        .context("failed to count pending pegout orders")?;
    let count: i64 = match rows.next().await.context("failed to read pending pegout order count")? {
        Some(row) => row.get(0)?,
        None => 0,
    };
    Ok(count.max(0) as u64)
}

/// Mirrors `expire_order`.
pub async fn expire_pegout_order(conn: &Connection, id: &str) -> Result<bool> {
    let rows_affected = conn
        .execute(
            "UPDATE pegout_orders SET status = ?, updated_at = unixepoch() \
             WHERE id = ? AND status = ?",
            (
                PegoutOrderStatus::Expired.label(),
                id,
                PegoutOrderStatus::Pending.label(),
            ),
        )
        .await
        .context("failed to expire pegout order")?;
    Ok(rows_affected > 0)
}

pub async fn list_expired_pegout_orders_since(
    conn: &Connection,
    since_unix: i64,
    limit: u64,
) -> Result<Vec<PegoutOrder>> {
    let mut rows = conn
        .query(
            &format!(
                "SELECT {PEGOUT_ORDER_COLUMNS} FROM pegout_orders \
                 WHERE status = 'expired' AND updated_at >= ? \
                 ORDER BY updated_at DESC LIMIT ?"
            ),
            (since_unix, i64::try_from(limit).unwrap_or(i64::MAX)),
        )
        .await
        .context("failed to query expired pegout orders")?;
    let mut orders = Vec::new();
    while let Some(row) = rows.next().await.context("failed to read expired pegout order")? {
        orders.push(row_to_pegout_order(&row)?);
    }
    Ok(orders)
}

pub async fn revive_expired_pegout_order(conn: &Connection, id: &str) -> Result<bool> {
    let rows_affected = conn
        .execute(
            "UPDATE pegout_orders SET status = ?, updated_at = unixepoch() \
             WHERE id = ? AND status = ?",
            (PegoutOrderStatus::Pending.label(), id, PegoutOrderStatus::Expired.label()),
        )
        .await
        .context("failed to revive expired pegout order")?;
    Ok(rows_affected > 0)
}

/// Mirrors `reserved_wecx_amount`, but for the ECX side: `amount_out_sat` already committed to
/// peg-out orders whose wECX deposit has landed but hasn't been paid out in ECX yet. Used to keep
/// `create_pegout_order` from promising more ECX than the treasury wallet can actually cover -
/// unlike a peg-in refund (which just returns ECX that was itself the deposit, net-zero on the
/// wallet's balance), a peg-out payout draws down the treasury's ECX balance for real.
pub async fn pegout_reserved_ecx_amount(conn: &Connection) -> Result<u64> {
    let mut rows = conn
        .query(
            "SELECT COALESCE(SUM(amount_out_sat), 0) FROM pegout_orders WHERE status IN (?, ?, ?)",
            (
                PegoutOrderStatus::DepositSeen.label(),
                PegoutOrderStatus::DepositConfirmed.label(),
                PegoutOrderStatus::PayingOut.label(),
            ),
        )
        .await
        .context("failed to query reserved ECX amount")?;
    let sat: i64 = match rows.next().await.context("failed to read reserved ECX amount row")? {
        Some(row) => row.get(0)?,
        None => 0,
    };
    Ok(sat as u64)
}

/// Records a newly-found deposit (see `solana::SolanaWallet::find_reference_deposit`) and reprices
/// `amount_out_sat` to what actually landed, exactly like `update_deposit` does for the ECX side -
/// the customer's wallet may not have sent precisely the quoted `amount_in_base_units`. Conditional
/// on the order still being `pending`, for the same staleness reason `update_deposit` guards
/// against. Returns `true` iff this call won the write.
pub async fn record_pegout_deposit(
    conn: &Connection,
    id: &str,
    status: PegoutOrderStatus,
    deposit_signature: &str,
    depositor_owner: Option<&str>,
    amount_in_base_units: u64,
    amount_out_sat: u64,
) -> Result<bool> {
    let rows_affected = conn
        .execute(
            "UPDATE pegout_orders SET status = ?, deposit_signature = ?, depositor_owner = ?, \
             amount_in_base_units = ?, amount_out_sat = ?, updated_at = unixepoch() \
             WHERE id = ? AND status = ?",
            (
                status.label(),
                deposit_signature,
                depositor_owner,
                to_i64(amount_in_base_units)?,
                to_i64(amount_out_sat)?,
                id,
                PegoutOrderStatus::Pending.label(),
            ),
        )
        .await
        .context("failed to record pegout order deposit")?;
    Ok(rows_affected > 0)
}

/// Flips a previously-seen deposit to confirmed once its transaction reaches `finalized` (see
/// `solana::SolanaWallet::signature_finalized`) - the peg-out mirror of the confirmation-depth
/// check `detect_deposit` does on the ECX side. Conditional on the order still being
/// `deposit_seen`. Returns `true` iff this call won the write.
pub async fn confirm_pegout_deposit(conn: &Connection, id: &str) -> Result<bool> {
    let rows_affected = conn
        .execute(
            "UPDATE pegout_orders SET status = ?, updated_at = unixepoch() WHERE id = ? AND status = ?",
            (
                PegoutOrderStatus::DepositConfirmed.label(),
                id,
                PegoutOrderStatus::DepositSeen.label(),
            ),
        )
        .await
        .context("failed to confirm pegout order deposit")?;
    Ok(rows_affected > 0)
}

/// Mirrors `claim_for_payout`.
pub async fn claim_for_pegout_payout(conn: &Connection, id: &str) -> Result<bool> {
    let rows_affected = conn
        .execute(
            "UPDATE pegout_orders SET status = ?, updated_at = unixepoch() WHERE id = ? AND status = ?",
            (
                PegoutOrderStatus::PayingOut.label(),
                id,
                PegoutOrderStatus::DepositConfirmed.label(),
            ),
        )
        .await
        .context("failed to claim pegout order for payout")?;
    Ok(rows_affected > 0)
}

/// Marks a claimed order paid out with its ECX payout txid. The signed transaction is stored in
/// `outbound_txs` before broadcast; this only moves `paying_out` to `paid_out`.
pub async fn update_pegout_payout(conn: &Connection, id: &str, payout_txid: &str) -> Result<bool> {
    let rows_affected = conn
        .execute(
            "UPDATE pegout_orders SET status = ?, payout_txid = ?, updated_at = unixepoch() \
             WHERE id = ? AND status = ?",
            (
                PegoutOrderStatus::PaidOut.label(),
                payout_txid,
                id,
                PegoutOrderStatus::PayingOut.label(),
            ),
        )
        .await
        .context("failed to update pegout order payout")?;
    Ok(rows_affected > 0)
}

/// Mirrors `revert_claim`.
pub async fn revert_pegout_claim(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE pegout_orders SET status = ?, updated_at = unixepoch() WHERE id = ? AND status = ?",
        (
            PegoutOrderStatus::DepositConfirmed.label(),
            id,
            PegoutOrderStatus::PayingOut.label(),
        ),
    )
    .await
    .context("failed to revert pegout order payout claim")?;
    Ok(())
}

/// Mirrors `request_refund`.
pub async fn request_pegout_refund(conn: &Connection, id: &str) -> Result<bool> {
    let rows_affected = conn
        .execute(
            "UPDATE pegout_orders SET status = ?, updated_at = unixepoch() WHERE id = ? AND status = ?",
            (
                PegoutOrderStatus::RefundRequested.label(),
                id,
                PegoutOrderStatus::DepositConfirmed.label(),
            ),
        )
        .await
        .context("failed to request pegout order refund")?;
    Ok(rows_affected > 0)
}

/// Mirrors `claim_for_refund`.
pub async fn claim_for_pegout_refund(conn: &Connection, id: &str) -> Result<bool> {
    let rows_affected = conn
        .execute(
            "UPDATE pegout_orders SET status = ?, updated_at = unixepoch() WHERE id = ? AND status = ?",
            (
                PegoutOrderStatus::Refunding.label(),
                id,
                PegoutOrderStatus::RefundRequested.label(),
            ),
        )
        .await
        .context("failed to claim pegout order for refund")?;
    Ok(rows_affected > 0)
}

/// Mirrors `record_payout_signature`: durable *before* broadcast, since this refund is a Solana
/// send with the same ambiguous-RPC-outcome problem the peg-in payout has (see
/// `server::try_resolve_pegout_refund`).
pub async fn record_pegout_refund_signature(conn: &Connection, id: &str, refund_signature: &str) -> Result<bool> {
    let rows_affected = conn
        .execute(
            "UPDATE pegout_orders SET refund_signature = ?, updated_at = unixepoch() \
             WHERE id = ? AND status = ?",
            (refund_signature, id, PegoutOrderStatus::Refunding.label()),
        )
        .await
        .context("failed to record pegout order refund signature")?;
    Ok(rows_affected > 0)
}

/// Marks a claimed refund as sent and records the restocking fee kept. Conditional on the order
/// still being `refunding`.
pub async fn update_pegout_refund(
    conn: &Connection,
    id: &str,
    refund_signature: &str,
    fee_base_units: u64,
) -> Result<bool> {
    let rows_affected = conn
        .execute(
            "UPDATE pegout_orders SET status = ?, refund_signature = ?, refund_fee_base_units = ?, \
             updated_at = unixepoch() WHERE id = ? AND status = ?",
            (
                PegoutOrderStatus::Refunded.label(),
                refund_signature,
                to_i64(fee_base_units)?,
                id,
                PegoutOrderStatus::Refunding.label(),
            ),
        )
        .await
        .context("failed to update pegout order refund")?;
    Ok(rows_affected > 0)
}

/// The unwrap deposit was too small to send back after the restocking fee. Keeps it.
pub async fn finish_pegout_refund_kept(conn: &Connection, id: &str, fee_base_units: u64) -> Result<bool> {
    let rows = conn
        .execute(
            "UPDATE pegout_orders SET status = ?, refund_fee_base_units = ?, updated_at = unixepoch() \
             WHERE id = ? AND status = ?",
            (
                PegoutOrderStatus::Refunded.label(),
                to_i64(fee_base_units)?,
                id,
                PegoutOrderStatus::RefundRequested.label(),
            ),
        )
        .await
        .context("failed to keep a pegout refund as the fee")?;
    Ok(rows > 0)
}

/// Restocking fees kept on completed unwrap refunds, as `(fee, 0)` for unwrap-side earnings.
pub async fn pegout_restocking_fees(conn: &Connection) -> Result<Vec<(u64, u64)>> {
    let mut rows = conn
        .query(
            "SELECT refund_fee_base_units FROM pegout_orders WHERE status = ? AND refund_fee_base_units > 0",
            (PegoutOrderStatus::Refunded.label(),),
        )
        .await
        .context("failed to query pegout restocking fees")?;
    let mut fees = Vec::new();
    while let Some(row) = rows.next().await? {
        fees.push((u64::try_from(row.get::<i64>(0)?)?, 0));
    }
    Ok(fees)
}

/// Mirrors `revert_refund_claim`.
pub async fn revert_pegout_refund_claim(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE pegout_orders SET status = ?, refund_signature = NULL, updated_at = unixepoch() \
         WHERE id = ? AND status = ?",
        (
            PegoutOrderStatus::RefundRequested.label(),
            id,
            PegoutOrderStatus::Refunding.label(),
        ),
    )
    .await
    .context("failed to revert pegout order refund claim")?;
    Ok(())
}

// --- Reference-less treasury deposits -------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnmatchedStatus {
    /// Owed back; the refund has not been attempted yet.
    Pending,
    /// A refund is signed or in flight - see `claim_unmatched_refund`.
    Refunding,
    Refunded,
    /// Needs a person: depositor unknown, not an ordinary wallet, or refunds are off.
    Held,
    /// Stays in the treasury on purpose: operator funding, or dust kept as the fee.
    Kept,
}

impl UnmatchedStatus {
    pub fn label(self) -> &'static str {
        match self {
            UnmatchedStatus::Pending => "pending",
            UnmatchedStatus::Refunding => "refunding",
            UnmatchedStatus::Refunded => "refunded",
            UnmatchedStatus::Held => "held",
            UnmatchedStatus::Kept => "kept",
        }
    }
}

#[derive(Debug, Clone)]
pub struct UnmatchedDeposit {
    pub signature: String,
    pub amount_base_units: u64,
    pub depositor_owner: Option<String>,
    pub fee_base_units: u64,
    pub refund_base_units: u64,
    pub status: String,
    pub note: String,
    pub refund_signature: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

pub struct NewUnmatchedDeposit<'a> {
    pub signature: &'a str,
    pub amount_base_units: u64,
    pub depositor_owner: Option<&'a str>,
    pub fee_base_units: u64,
    pub refund_base_units: u64,
    pub status: UnmatchedStatus,
    pub note: &'a str,
}

const UNMATCHED_COLUMNS: &str = "signature, amount_base_units, depositor_owner, fee_base_units, \
    refund_base_units, status, note, refund_signature, created_at, updated_at";

fn row_to_unmatched(row: &Row) -> Result<UnmatchedDeposit> {
    Ok(UnmatchedDeposit {
        signature: row.get(0)?,
        amount_base_units: u64::try_from(row.get::<i64>(1)?)?,
        depositor_owner: row.get(2)?,
        fee_base_units: u64::try_from(row.get::<i64>(3)?)?,
        refund_base_units: u64::try_from(row.get::<i64>(4)?)?,
        status: row.get(5)?,
        note: row.get(6)?,
        refund_signature: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

/// Records a deposit once. Returns `false` if that signature was already on file, so a rescan
/// of the same history never duplicates a row or re-opens a settled one.
pub async fn insert_unmatched_deposit(conn: &Connection, deposit: &NewUnmatchedDeposit<'_>) -> Result<bool> {
    let rows = conn
        .execute(
            "INSERT OR IGNORE INTO unmatched_deposits \
             (signature, amount_base_units, depositor_owner, fee_base_units, refund_base_units, status, note) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            (
                deposit.signature,
                to_i64(deposit.amount_base_units)?,
                deposit.depositor_owner,
                to_i64(deposit.fee_base_units)?,
                to_i64(deposit.refund_base_units)?,
                deposit.status.label(),
                deposit.note,
            ),
        )
        .await
        .context("failed to record unmatched deposit")?;
    Ok(rows > 0)
}

pub async fn list_unmatched_deposits(
    conn: &Connection,
    statuses: &[UnmatchedStatus],
    limit: u64,
) -> Result<Vec<UnmatchedDeposit>> {
    let placeholders = statuses.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
    let sql = format!(
        "SELECT {UNMATCHED_COLUMNS} FROM unmatched_deposits WHERE status IN ({placeholders}) \
         ORDER BY created_at ASC LIMIT {}",
        i64::try_from(limit).unwrap_or(i64::MAX)
    );
    let params = rusqlite::params_from_iter(statuses.iter().map(|status| status.label()));
    let mut rows = conn.query(&sql, params).await.context("failed to query unmatched deposits")?;
    let mut deposits = Vec::new();
    while let Some(row) = rows.next().await? {
        deposits.push(row_to_unmatched(&row)?);
    }
    Ok(deposits)
}

/// `pending -> refunding`. `true` iff this call won the claim.
pub async fn claim_unmatched_refund(conn: &Connection, signature: &str) -> Result<bool> {
    let rows = conn
        .execute(
            "UPDATE unmatched_deposits SET status = 'refunding', updated_at = unixepoch() \
             WHERE signature = ? AND status = 'pending'",
            (signature,),
        )
        .await
        .context("failed to claim unmatched deposit for refund")?;
    Ok(rows == 1)
}

/// Durable before broadcast, like `record_pegout_refund_signature`.
pub async fn record_unmatched_refund_signature(conn: &Connection, signature: &str, refund_signature: &str) -> Result<bool> {
    let rows = conn
        .execute(
            "UPDATE unmatched_deposits SET refund_signature = ?, updated_at = unixepoch() \
             WHERE signature = ? AND status = 'refunding'",
            (refund_signature, signature),
        )
        .await
        .context("failed to record unmatched refund signature")?;
    Ok(rows == 1)
}

pub async fn finish_unmatched_refund(conn: &Connection, signature: &str, refund_signature: &str) -> Result<bool> {
    let rows = conn
        .execute(
            "UPDATE unmatched_deposits SET status = 'refunded', refund_signature = ?, updated_at = unixepoch() \
             WHERE signature = ? AND status = 'refunding'",
            (refund_signature, signature),
        )
        .await
        .context("failed to finish unmatched refund")?;
    Ok(rows == 1)
}

pub async fn revert_unmatched_refund_claim(conn: &Connection, signature: &str) -> Result<()> {
    conn.execute(
        "UPDATE unmatched_deposits SET status = 'pending', refund_signature = NULL, updated_at = unixepoch() \
         WHERE signature = ? AND status = 'refunding'",
        (signature,),
    )
    .await
    .context("failed to revert unmatched refund claim")?;
    Ok(())
}

/// Parks a `pending` or `refunding` row for a person, keeping any signature on file.
pub async fn hold_unmatched_deposit(conn: &Connection, signature: &str, note: &str) -> Result<()> {
    conn.execute(
        "UPDATE unmatched_deposits SET status = 'held', note = ?, updated_at = unixepoch() \
         WHERE signature = ? AND status IN ('pending', 'refunding')",
        (note, signature),
    )
    .await
    .context("failed to hold unmatched deposit")?;
    Ok(())
}

/// wECX owed back for reference-less deposits that are not yet refunded.
pub async fn reserved_unmatched_refunds(conn: &Connection) -> Result<u64> {
    let mut rows = conn
        .query(
            "SELECT COALESCE(SUM(refund_base_units), 0) FROM unmatched_deposits \
             WHERE status IN ('pending', 'refunding')",
            (),
        )
        .await
        .context("failed to query reserved unmatched refunds")?;
    let amount: i64 = rows.next().await?.context("missing reserved unmatched refunds row")?.get(0)?;
    Ok(u64::try_from(amount)?)
}

/// Fees kept from settled reference-less deposits, as `(fee, 0)` pairs so they add into the
/// unwrap-side earnings the same way completed orders do (see `server::sweep_earnings_once`).
pub async fn unmatched_deposit_fees(conn: &Connection) -> Result<Vec<(u64, u64)>> {
    let mut rows = conn
        .query(
            "SELECT fee_base_units FROM unmatched_deposits \
             WHERE status IN ('refunded', 'kept') AND fee_base_units > 0",
            (),
        )
        .await
        .context("failed to query unmatched deposit fees")?;
    let mut fees = Vec::new();
    while let Some(row) = rows.next().await? {
        fees.push((u64::try_from(row.get::<i64>(0)?)?, 0));
    }
    Ok(fees)
}

/// Counts by status: (held, pending or refunding, refunded).
pub async fn unmatched_deposit_counts(conn: &Connection) -> Result<(u64, u64, u64)> {
    let mut rows = conn
        .query(
            "SELECT \
                COALESCE(SUM(status = 'held'), 0), \
                COALESCE(SUM(status IN ('pending', 'refunding')), 0), \
                COALESCE(SUM(status = 'refunded'), 0) \
             FROM unmatched_deposits",
            (),
        )
        .await
        .context("failed to count unmatched deposits")?;
    let row = rows.next().await?.context("missing unmatched deposit counts")?;
    Ok((
        u64::try_from(row.get::<i64>(0)?)?,
        u64::try_from(row.get::<i64>(1)?)?,
        u64::try_from(row.get::<i64>(2)?)?,
    ))
}

/// Every reference key ever handed to a peg-out order. A treasury deposit whose transaction
/// touches one of these belongs to that order's flow, whatever its state.
pub async fn pegout_reference_pubkeys(conn: &Connection) -> Result<std::collections::HashSet<String>> {
    let mut rows = conn
        .query("SELECT reference_pubkey FROM pegout_orders", ())
        .await
        .context("failed to query pegout reference keys")?;
    let mut keys = std::collections::HashSet::new();
    while let Some(row) = rows.next().await? {
        keys.insert(row.get::<String>(0)?);
    }
    Ok(keys)
}

pub async fn is_pegout_deposit_signature(conn: &Connection, signature: &str) -> Result<bool> {
    let mut rows = conn
        .query("SELECT 1 FROM pegout_orders WHERE deposit_signature = ? LIMIT 1", (signature,))
        .await
        .context("failed to check pegout deposit signature")?;
    Ok(rows.next().await?.is_some())
}

pub async fn scan_cursor(conn: &Connection, name: &str) -> Result<Option<String>> {
    let mut rows = conn
        .query("SELECT value FROM scan_cursors WHERE name = ?", (name,))
        .await
        .context("failed to read scan cursor")?;
    Ok(match rows.next().await? {
        Some(row) => Some(row.get(0)?),
        None => None,
    })
}

pub async fn set_scan_cursor(conn: &Connection, name: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO scan_cursors (name, value) VALUES (?, ?) \
         ON CONFLICT(name) DO UPDATE SET value = excluded.value, updated_at = unixepoch()",
        (name, value),
    )
    .await
    .context("failed to write scan cursor")?;
    Ok(())
}

#[cfg(test)]
mod recent_order_tests {
    use super::*;

    #[tokio::test]
    async fn unmatched_deposits_reserve_until_refunded_and_fees_count_once_settled() {
        let conn = Connection(Arc::new(Mutex::new(rusqlite::Connection::open_in_memory().unwrap())));
        conn.execute_batch(SCHEMA).await.unwrap();
        assert_eq!(scan_cursor(&conn, "treasury_deposits").await.unwrap(), None);
        set_scan_cursor(&conn, "treasury_deposits", "sig-a").await.unwrap();
        set_scan_cursor(&conn, "treasury_deposits", "sig-b").await.unwrap();
        assert_eq!(scan_cursor(&conn, "treasury_deposits").await.unwrap().as_deref(), Some("sig-b"));

        let pending = NewUnmatchedDeposit {
            signature: "dep-1", amount_base_units: 10_000, depositor_owner: Some("wallet"),
            fee_base_units: 100, refund_base_units: 9_900, status: UnmatchedStatus::Pending, note: "",
        };
        assert!(insert_unmatched_deposit(&conn, &pending).await.unwrap());
        assert!(!insert_unmatched_deposit(&conn, &pending).await.unwrap(), "a rescan must not duplicate");
        assert!(insert_unmatched_deposit(&conn, &NewUnmatchedDeposit {
            signature: "dep-2", amount_base_units: 5, depositor_owner: Some("wallet"),
            fee_base_units: 5, refund_base_units: 0, status: UnmatchedStatus::Kept, note: "dust",
        }).await.unwrap());
        assert!(insert_unmatched_deposit(&conn, &NewUnmatchedDeposit {
            signature: "dep-3", amount_base_units: 777, depositor_owner: None,
            fee_base_units: 0, refund_base_units: 0, status: UnmatchedStatus::Held, note: "unknown depositor",
        }).await.unwrap());

        assert_eq!(reserved_unmatched_refunds(&conn).await.unwrap(), 9_900);
        assert_eq!(reserved_wecx_amount(&conn).await.unwrap(), 9_900, "owed refunds are not wrap float");
        assert_eq!(customer_earnings_reserve(&conn, "wbecx").await.unwrap(), 9_900);
        assert_eq!(customer_earnings_reserve(&conn, "becx").await.unwrap(), 0);
        assert_eq!(unmatched_deposit_fees(&conn).await.unwrap(), vec![(5, 0)], "only settled rows earn");
        assert_eq!(unmatched_deposit_counts(&conn).await.unwrap(), (1, 1, 0));

        assert!(claim_unmatched_refund(&conn, "dep-1").await.unwrap());
        assert!(!claim_unmatched_refund(&conn, "dep-1").await.unwrap(), "a second claim must lose");
        assert_eq!(reserved_unmatched_refunds(&conn).await.unwrap(), 9_900, "still owed while refunding");
        assert!(record_unmatched_refund_signature(&conn, "dep-1", "refund-sig").await.unwrap());
        revert_unmatched_refund_claim(&conn, "dep-1").await.unwrap();
        let row = &list_unmatched_deposits(&conn, &[UnmatchedStatus::Pending], 10).await.unwrap()[0];
        assert_eq!((row.signature.as_str(), row.refund_signature.as_deref()), ("dep-1", None));
        assert!(claim_unmatched_refund(&conn, "dep-1").await.unwrap());
        assert!(finish_unmatched_refund(&conn, "dep-1", "refund-sig-2").await.unwrap());
        assert_eq!(reserved_unmatched_refunds(&conn).await.unwrap(), 0);
        let mut fees = unmatched_deposit_fees(&conn).await.unwrap();
        fees.sort();
        assert_eq!(fees, vec![(5, 0), (100, 0)]);
        assert_eq!(unmatched_deposit_counts(&conn).await.unwrap(), (1, 0, 1));

        hold_unmatched_deposit(&conn, "dep-1", "x").await.unwrap();
        assert_eq!(unmatched_deposit_counts(&conn).await.unwrap(), (1, 0, 1), "a refunded row cannot be held");
        assert!(!is_pegout_deposit_signature(&conn, "dep-1").await.unwrap());
        assert!(pegout_reference_pubkeys(&conn).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn sweep_ledger_reserves_pending_and_completed_once() {
        let conn = Connection(Arc::new(Mutex::new(rusqlite::Connection::open_in_memory().unwrap())));
        conn.execute_batch(SCHEMA).await.unwrap();
        assert_eq!(paid_earnings(&conn).await.unwrap(), (0, 0));
        let mut sweep = EarningsSweep {
            id: "first".into(), asset: "becx".into(), amount: 10_000,
            destination: "owner".into(), transaction_id: "tx".into(), transaction_hex: "hex".into(),
        };
        record_earnings_sweep(&conn, &sweep).await.unwrap();
        assert_eq!(allocated_earnings(&conn, "becx").await.unwrap(), 10_000);
        assert_eq!(paid_earnings(&conn).await.unwrap(), (0, 0));
        sweep.id = "second".into();
        assert!(record_earnings_sweep(&conn, &sweep).await.is_err());
        finish_earnings_sweep(&conn, "first", true).await.unwrap();
        finish_earnings_sweep(&conn, "first", false).await.unwrap();
        assert_eq!(allocated_earnings(&conn, "becx").await.unwrap(), 10_000);
        record_earnings_sweep(&conn, &sweep).await.unwrap();
        finish_earnings_sweep(&conn, "second", false).await.unwrap();
        assert_eq!(allocated_earnings(&conn, "becx").await.unwrap(), 10_000);
        assert!(pending_earnings_sweeps(&conn).await.unwrap().is_empty());
        assert_eq!(paid_earnings(&conn).await.unwrap(), (10_000, 0));
        sweep.id = "token-sweep".into();
        sweep.asset = "wbecx".into();
        record_earnings_sweep(&conn, &sweep).await.unwrap();
        assert_eq!(reserved_wecx_amount(&conn).await.unwrap(), 10_000);
        assert_eq!(paid_earnings(&conn).await.unwrap(), (10_000, 0));
        finish_earnings_sweep(&conn, "token-sweep", true).await.unwrap();
        assert_eq!(reserved_wecx_amount(&conn).await.unwrap(), 0);
        assert_eq!(paid_earnings(&conn).await.unwrap(), (10_000, 10_000));
    }

    #[tokio::test]
    async fn stipend_columns_are_added_to_an_older_database() {
        let conn = Connection(Arc::new(Mutex::new(rusqlite::Connection::open_in_memory().unwrap())));
        conn.execute_batch(
            "CREATE TABLE bridge_orders (
                id TEXT PRIMARY KEY NOT NULL,
                amount_in_sat INTEGER NOT NULL,
                amount_out_base_units INTEGER NOT NULL,
                deposit_address TEXT NOT NULL,
                solana_recipient TEXT NOT NULL,
                refund_address TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL DEFAULT 'pending',
                deposit_txid TEXT,
                payout_signature TEXT,
                refund_txid TEXT,
                created_at INTEGER NOT NULL DEFAULT 0,
                updated_at INTEGER NOT NULL DEFAULT 0
            );",
        )
        .await
        .unwrap();
        conn.migrate().await.unwrap();
        conn.migrate().await.unwrap();
        conn.execute(
            "INSERT INTO bridge_orders \
                (id, amount_in_sat, amount_out_base_units, deposit_address, solana_recipient) \
             VALUES ('old', 1, 1, 'dest', 'recip')",
            (),
        )
        .await
        .unwrap();
        let order = get_order(&conn, "old").await.unwrap().unwrap();
        assert_eq!(order.sol_stipend_lamports, 0);
        assert_eq!(order.sol_stipend_price_sat, 0);
        assert_eq!(order.ata_rent_lamports, 0);
        assert_eq!(order.ata_rent_price_sat, 0);
    }

    #[tokio::test]
    async fn recent_orders_merge_directions_statuses_and_sort_newest_first() {
        let sqlite = rusqlite::Connection::open_in_memory().unwrap();
        let conn = Connection(Arc::new(Mutex::new(sqlite)));
        conn.execute_batch(SCHEMA).await.unwrap();
        conn.execute_batch(
            "INSERT INTO bridge_orders \
                (id, amount_in_sat, amount_out_base_units, deposit_address, solana_recipient, status, created_at) \
             VALUES ('wrap-complete', 100, 99, 'deposit-1', 'recipient-1', 'paid_out', 100), \
                    ('wrap-failed', 200, 198, 'deposit-2', 'recipient-2', 'failed', 300); \
             INSERT INTO pegout_orders \
                (id, amount_in_base_units, amount_out_sat, ecx_recipient, reference_pubkey, status, created_at) \
             VALUES ('unwrap-refunded', 300, 297, 'recipient-3', 'reference-3', 'refunded', 200);",
        )
        .await
        .unwrap();

        let orders = recent_orders(&conn, 3).await.unwrap();

        assert_eq!(orders.len(), 3);
        assert_eq!(orders[0].direction, "wrap");
        assert_eq!(orders[0].status, "failed");
        assert_eq!(orders[1].direction, "unwrap");
        assert_eq!(orders[1].status, "refunded");
        assert_eq!(orders[2].direction, "wrap");
        assert_eq!(orders[2].status, "paid_out");
        assert_eq!(customer_earnings_reserve(&conn, "becx").await.unwrap(), 0);
        conn.execute("UPDATE bridge_orders SET deposit_txid = 'funded' WHERE id = 'wrap-failed'", ()).await.unwrap();
        assert_eq!(customer_earnings_reserve(&conn, "becx").await.unwrap(), 200);
        assert_eq!(customer_earnings_reserve(&conn, "wbecx").await.unwrap(), 198);
        conn.execute("UPDATE pegout_orders SET status = 'refunding' WHERE id = 'unwrap-refunded'", ()).await.unwrap();
        assert_eq!(customer_earnings_reserve(&conn, "becx").await.unwrap(), 497);
        assert_eq!(customer_earnings_reserve(&conn, "wbecx").await.unwrap(), 498);
        assert_eq!(unsettled_deposit_addresses(&conn).await.unwrap(), vec!["deposit-2"]);
    }
}
