use std::collections::HashMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::PathBuf;

use anyhow::{anyhow, bail, ensure, Context, Result};
use bdk_electrum::{electrum_client, BdkElectrumClient};
use bdk_esplora::{esplora_client, EsploraAsyncExt};
use bdk_file_store::Store;
use bdk_wallet::bitcoin::absolute::LockTime;
use bdk_wallet::bitcoin::{Address, Amount, FeeRate, Network, OutPoint, ScriptBuf, Transaction, Txid};
use bdk_wallet::error::CreateTxError;
use bdk_wallet::chain::spk_client::SyncRequest;
use bdk_wallet::{Balance, ChangeSet, KeychainKind, LocalOutput, PersistedWallet, SignOptions, Wallet};

use crate::config::{Backend, ChainConfig};

const DB_MAGIC: &[u8] = b"swap-backend-wallet-1";
const STOP_GAP: usize = 25;
const PARALLEL_REQUESTS: usize = 5;

enum Client {
    // `base_url` is kept alongside the esplora-client instance so `broadcast()` can POST the raw
    // tx hex directly to `{base_url}/tx` instead of going through esplora-client's own broadcast
    // call, which fails against this backend (see `ChainWallet::broadcast`).
    Esplora {
        async_client: esplora_client::AsyncClient,
        base_url: String,
    },
    Electrum(BdkElectrumClient<electrum_client::Client>),
}

pub struct ChainWallet {
    inner: PersistedWallet<Store<ChangeSet>>,
    store: Store<ChangeSet>,
    client: Client,
    // Held for the wallet's lifetime and released automatically on drop (even on a crash, since
    // it's an OS-level advisory lock) - see the `lock_path` acquisition in `open()`.
    _lock: File,
}

/// HTTP client for the direct esplora calls this file makes outside esplora-client (`broadcast`,
/// `esplora_spent_outpoints`), with the same bounded timeout the esplora-client instance gets. A bare
/// `reqwest::Client::new()` has no timeout at all, and both callers run while holding the global
/// wallet lock - one connection that stops responding would otherwise freeze the whole service.
fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(crate::config::esplora_timeout())
        .build()
        .context("failed to build HTTP client")
}

/// Exclusively lock a `<db_path>.lock` sidecar file so only one `ChainWallet` can have `db_path`
/// open at a time. `bdk_file_store::Store` itself has no cross-process locking - two processes
/// (e.g. `serve` plus a one-off CLI command) appending to the same store concurrently can
/// interleave writes and corrupt it (see `ChainWallet::recover`, which exists because that already
/// happened once). Returns a clear error instead of silently risking that.
fn lock_store(db_path: &std::path::Path) -> Result<File> {
    let mut lock_path = db_path.as_os_str().to_owned();
    lock_path.push(".lock");
    let lock_path = PathBuf::from(lock_path);

    let lock_file = OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("failed to open wallet lock file at {}", lock_path.display()))?;

    match lock_file.try_lock() {
        Ok(()) => Ok(lock_file),
        Err(TryLockError::WouldBlock) => bail!(
            "wallet store at {} is already open by another process (is `serve` running, or \
             another CLI command?) - refusing to open it a second time, since a concurrent writer \
             can corrupt the store",
            db_path.display()
        ),
        Err(TryLockError::Error(e)) => Err(e).context("failed to lock wallet store"),
    }
}

#[cfg(test)]
mod lock_tests {
    use super::lock_store;

    #[test]
    fn second_lock_on_same_path_is_rejected() {
        let dir = std::env::temp_dir().join(format!("wallet-lock-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("test.db");

        let _first = lock_store(&db_path).expect("first lock should succeed");
        let second = lock_store(&db_path);
        assert!(second.is_err(), "second lock on the same path should be rejected");

        drop(_first);
        let _third = lock_store(&db_path).expect("lock should be free again after drop");

        std::fs::remove_dir_all(&dir).ok();
    }
}

pub struct RecoverySummary {
    pub backup_path: PathBuf,
    pub recovered_path: PathBuf,
    pub original_error: String,
    pub balance: Balance,
}

impl ChainWallet {
    pub fn open(config: &ChainConfig, external_desc: &str, internal_desc: &str) -> Result<Self> {
        let external_desc = external_desc.to_string();
        let internal_desc = internal_desc.to_string();

        let lock = lock_store(&config.db_path)?;

        let mut store = Store::<ChangeSet>::load_or_create(DB_MAGIC, &config.db_path)
            .map(|(store, _)| store)
            .context("failed to open wallet store")?;

        let loaded = Wallet::load()
            .descriptor(KeychainKind::External, Some(external_desc.clone()))
            .descriptor(KeychainKind::Internal, Some(internal_desc.clone()))
            .extract_keys()
            .check_network(config.network)
            .load_wallet(&mut store)
            .context("failed to load wallet from store")?;

        let inner = match loaded {
            Some(w) => w,
            None => Wallet::create(external_desc, internal_desc)
                .network(config.network)
                .create_wallet(&mut store)
                .context("failed to create wallet")?,
        };

        let client = match config.backend {
            Backend::Esplora => Client::Esplora {
                async_client: esplora_client::Builder::new(&config.esplora_url)
                    .timeout(crate::config::esplora_timeout().as_secs())
                    .build_async()
                    .context("failed to build esplora client")?,
                base_url: config.esplora_url.clone(),
            },
            Backend::Electrum => Client::Electrum(BdkElectrumClient::new(
                electrum_client::Client::new(&config.electrum_url)
                    .context("failed to connect electrum client")?,
            )),
        };

        Ok(Self {
            inner,
            store,
            client,
            _lock: lock,
        })
    }

    /// Salvage a wallet store whose tail entry got corrupted (e.g. by two processes appending to
    /// the file at once - `bdk_file_store` has no cross-process locking, see its own doc warning
    /// that it's "a development/testing database" only). Never touches the live file: copies it to
    /// a `.corrupt-backup-<unix-ts>` sibling first and only reads from that copy, then writes
    /// whatever fully-valid changesets it could recover (everything up to the corrupted entry) to a
    /// new `.recovered` sibling. Swapping that in for the live file is left to the caller.
    pub fn recover(
        config: &ChainConfig,
        external_desc: &str,
        internal_desc: &str,
    ) -> Result<RecoverySummary> {
        ensure!(
            config.db_path.exists(),
            "no wallet store at {}",
            config.db_path.display()
        );

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        let mut backup_path = config.db_path.clone().into_os_string();
        backup_path.push(format!(".corrupt-backup-{timestamp}"));
        let backup_path = PathBuf::from(backup_path);
        std::fs::copy(&config.db_path, &backup_path)
            .context("failed to back up wallet store before recovery")?;

        let dump_err = match Store::<ChangeSet>::load(DB_MAGIC, &backup_path) {
            Ok(_) => {
                return Err(anyhow!(
                    "store at {} loaded cleanly - nothing to recover (the original error may \
                     have been transient, e.g. another process holding the file at the time)",
                    config.db_path.display()
                ))
            }
            Err(e) => e,
        };

        let recovered = *dump_err.changeset.context(format!(
            "no changesets could be recovered before the corruption: {}",
            dump_err.error
        ))?;

        // Sanity-check the recovered data actually loads as a valid wallet before we write it out.
        let loaded_wallet = Wallet::load()
            .descriptor(KeychainKind::External, Some(external_desc.to_string()))
            .descriptor(KeychainKind::Internal, Some(internal_desc.to_string()))
            .extract_keys()
            .check_network(config.network)
            .load_wallet_no_persist(recovered.clone())
            .context("recovered changeset failed to load as a wallet")?
            .context("recovered changeset has no wallet data")?;

        let mut recovered_path = config.db_path.clone().into_os_string();
        recovered_path.push(".recovered");
        let recovered_path = PathBuf::from(recovered_path);
        let mut new_store = Store::<ChangeSet>::create(DB_MAGIC, &recovered_path).context(
            "failed to create recovered store (remove any stale .recovered file first)",
        )?;
        new_store
            .append(&recovered)
            .context("failed to write recovered changesets")?;

        Ok(RecoverySummary {
            backup_path,
            recovered_path,
            original_error: dump_err.error.to_string(),
            balance: loaded_wallet.balance(),
        })
    }

    pub fn next_address(&mut self) -> Result<Address> {
        let info = self.inner.next_unused_address(KeychainKind::External);
        self.inner.persist(&mut self.store)?;
        Ok(info.address)
    }

    /// Always advances to a brand-new address, regardless of whether the current one was used.
    pub fn fresh_address(&mut self) -> Result<Address> {
        let info = self.inner.reveal_next_address(KeychainKind::External);
        self.inner.persist(&mut self.store)?;
        Ok(info.address)
    }

    pub fn balance(&self) -> Balance {
        self.inner.balance()
    }

    /// Whether `address` is one of this wallet's own: any address it has revealed, plus the first
    /// `PEEK_DEPTH` of each keychain whether revealed yet or not. Used to stop the cold-storage
    /// sweep destination from being an address of the hot wallet itself, which would "sweep" fees
    /// back onto the server while the ledger records them as moved. Derivation only, no network.
    pub fn owns_address(&self, address: &Address) -> bool {
        const PEEK_DEPTH: u32 = 1_000;
        let script = address.script_pubkey();
        if self.inner.is_mine(script.clone()) {
            return true;
        }
        [KeychainKind::External, KeychainKind::Internal].into_iter().any(|keychain| {
            (0..PEEK_DEPTH).any(|index| self.inner.peek_address(keychain, index).address.script_pubkey() == script)
        })
    }

    pub fn utxos(&self) -> Vec<LocalOutput> {
        self.inner.list_unspent().collect()
    }

    /// All outputs the wallet has ever seen at a tracked script pubkey - spent or unspent,
    /// confirmed or not. Unlike `utxos()`, an output found here stays found even after something
    /// else spends it, so deposit detection (which matches by script pubkey, see `detect_deposit`)
    /// can't lose track of a deposit that lands and is then swept up as an input to an unrelated
    /// payout before its own order gets a chance to record it.
    pub fn outputs(&self) -> Vec<LocalOutput> {
        self.inner.list_output().collect()
    }

    pub async fn sync(&mut self) -> Result<()> {
        let request = self.inner.start_sync_with_revealed_spks();
        match &self.client {
            Client::Esplora { async_client, .. } => {
                let update = async_client
                    .sync(request, PARALLEL_REQUESTS)
                    .await
                    .context("esplora sync failed")?;
                self.inner.apply_update(update)?;
            }
            Client::Electrum(client) => {
                client.populate_tx_cache(self.inner.tx_graph().full_txs().map(|n| n.tx));
                let update = client
                    .sync(request, PARALLEL_REQUESTS, false)
                    .context("electrum sync failed")?;
                self.inner.apply_update(update)?;
            }
        }
        self.inner.persist(&mut self.store)?;
        Ok(())
    }

    /// Like `sync()`, but scoped to a caller-chosen set of script pubkeys instead of every spk the
    /// wallet has ever revealed - for checking specific orders' deposit addresses without paying
    /// for a sync over the wallet's whole address history (which only grows as more orders are
    /// created). A single network round-trip covers every script passed in, so callers checking
    /// several addresses at once (see `poll_orders_once`) should batch them into one call rather
    /// than calling this once per address. Only updates this wallet's view of the given scripts; it
    /// doesn't refresh balance or pick up activity at any other address, so it's not a substitute
    /// for `sync()` where a fresh overall balance is actually needed. A no-op (no network call) if
    /// `scripts` is empty.
    pub async fn sync_addresses(&mut self, scripts: impl IntoIterator<Item = ScriptBuf>) -> Result<()> {
        let request = SyncRequest::builder()
            .chain_tip(self.inner.latest_checkpoint())
            .spks(scripts)
            .build();
        if request.progress().total_spks() == 0 {
            return Ok(());
        }
        match &self.client {
            Client::Esplora { async_client, .. } => {
                let update = async_client
                    .sync(request, PARALLEL_REQUESTS)
                    .await
                    .context("esplora address sync failed")?;
                self.inner.apply_update(update)?;
            }
            Client::Electrum(client) => {
                client.populate_tx_cache(self.inner.tx_graph().full_txs().map(|n| n.tx));
                let update = client
                    .sync(request, PARALLEL_REQUESTS, false)
                    .context("electrum address sync failed")?;
                self.inner.apply_update(update)?;
            }
        }
        self.inner.persist(&mut self.store)?;
        Ok(())
    }

    /// Re-checks the spend status of every outpoint this wallet currently believes is unspent,
    /// without resyncing every historical address - for verifying coin-selection candidates are
    /// still actually spendable right before building a real transaction. This wallet's own sends
    /// update local state as soon as they broadcast (see `sign_and_broadcast`), so the only gap
    /// this closes is something spending a UTXO *outside* this process's knowledge - e.g. an
    /// operator's manual CLI payout/refund against the same wallet store (see `main.rs`), or a
    /// wallet-store recovery/swap-in while this process wasn't running - between whenever that UTXO
    /// was last discovered and now. Scoped to exactly the wallet's current UTXO set rather than
    /// every spk it's ever revealed, so cost is bounded by how many outputs it actually holds, not
    /// by its address history. Returns the outpoints found already spent, so callers can exclude
    /// them from coin selection via `send`'s `unspendable`. A no-op (no network call, empty result)
    /// if there are no UTXOs.
    pub async fn verify_unspent(&mut self) -> Result<Vec<OutPoint>> {
        let outpoints: Vec<OutPoint> = self.utxos().into_iter().map(|u| u.outpoint).collect();
        if outpoints.is_empty() {
            return Ok(Vec::new());
        }
        match &self.client {
            Client::Esplora { async_client, base_url } => {
                // `EsploraAsyncExt::sync`'s outpoint checking calls esplora-client's
                // `get_output_status`, which hits the single-outpoint `GET
                // /tx/:txid/outspend/:vout` endpoint - this backend doesn't implement that route
                // (confirmed: it 404s), and esplora-client treats a 404 there as "no status"
                // rather than an error, so BDK's sync silently learns nothing and this check was a
                // permanent no-op. `GET /tx/:txid/outspends` (plural, all vouts of one tx in one
                // response) does work against this backend, so query that directly instead - one
                // request per distinct *source* tx among the current UTXOs rather than per
                // outpoint, which is usually fewer requests than the one-per-outpoint approach it
                // replaces.
                let spent = esplora_spent_outpoints(base_url, &outpoints).await?;
                if spent.is_empty() {
                    return Ok(Vec::new());
                }

                // Found spends need to be applied to local state, not just reported for this one
                // send's coin selection to exclude - otherwise `balance()` (and, downstream,
                // `db::reserve_cache` - see `refresh_reserve_cache` in server.rs) keeps counting
                // these outpoints as spendable forever, re-discovering the same spend on every
                // future call instead of ever actually correcting itself. The outspends response
                // only says *that* an outpoint is spent and by which txid, not the spending tx
                // itself, so fetch each distinct one and apply it the same way `sign_and_broadcast`
                // applies its own sends. Best-effort per tx: a fetch failure just leaves that
                // outpoint to be re-discovered (and still excluded from coin selection) next time,
                // rather than failing this whole check.
                let spending_txids: std::collections::HashSet<Txid> =
                    spent.iter().filter_map(|s| s.spending_txid).collect();
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let mut fetched = Vec::new();
                for txid in spending_txids {
                    match async_client.get_tx(&txid).await {
                        Ok(Some(tx)) => fetched.push((tx, now)),
                        Ok(None) => eprintln!(
                            "[verify_unspent] spending tx {txid} not found on this backend yet - \
                             will re-check next call"
                        ),
                        Err(e) => eprintln!(
                            "[verify_unspent] failed to fetch spending tx {txid}, will re-check \
                             next call: {e}"
                        ),
                    }
                }
                if !fetched.is_empty() {
                    self.inner.apply_unconfirmed_txs(fetched);
                    self.inner.persist(&mut self.store)?;
                }

                Ok(spent.into_iter().map(|s| s.outpoint).collect())
            }
            Client::Electrum(client) => {
                let request = SyncRequest::<()>::builder()
                    .chain_tip(self.inner.latest_checkpoint())
                    .outpoints(outpoints.clone())
                    .build();
                client.populate_tx_cache(self.inner.tx_graph().full_txs().map(|n| n.tx));
                let update = client
                    .sync(request, PARALLEL_REQUESTS, false)
                    .context("electrum outpoint verification failed")?;
                self.inner.apply_update(update)?;
                self.inner.persist(&mut self.store)?;
                let still_unspent: std::collections::HashSet<OutPoint> =
                    self.utxos().into_iter().map(|u| u.outpoint).collect();
                Ok(outpoints
                    .into_iter()
                    .filter(|op| !still_unspent.contains(op))
                    .collect())
            }
        }
    }

    pub async fn full_scan(&mut self) -> Result<()> {
        let request = self.inner.start_full_scan();
        match &self.client {
            Client::Esplora { async_client, .. } => {
                let update = async_client
                    .full_scan(request, STOP_GAP, PARALLEL_REQUESTS)
                    .await
                    .context("esplora full scan failed")?;
                self.inner.apply_update(update)?;
            }
            Client::Electrum(client) => {
                client.populate_tx_cache(self.inner.tx_graph().full_txs().map(|n| n.tx));
                let update = client
                    .full_scan(request, STOP_GAP, PARALLEL_REQUESTS, false)
                    .context("electrum full scan failed")?;
                self.inner.apply_update(update)?;
            }
        }
        self.inner.persist(&mut self.store)?;
        Ok(())
    }

    /// Builds, signs, and broadcasts a transaction paying `amount` to `to` (auto coin selection,
    /// change returned to the wallet's own change address). If `amount` is `None`, sends the
    /// wallet's entire spendable balance instead (no change output).
    ///
    /// `unspendable` is excluded from automatic coin selection entirely - callers use this to keep
    /// one order's payout from grabbing a UTXO that just landed at *another* order's deposit
    /// address before that order's own poll has recorded it (see `protected_deposit_outpoints` in
    /// server.rs). Not a hard guarantee against every race, just against automatic selection ever
    /// reaching for these specific outpoints.
    pub async fn send(
        &mut self,
        to: &Address,
        amount: Option<Amount>,
        fee_rate: FeeRate,
        unspendable: &[OutPoint],
    ) -> Result<Txid> {
        let mut builder = self.inner.build_tx();
        match amount {
            Some(amount) => {
                builder.add_recipient(to.script_pubkey(), amount);
            }
            None => {
                builder.drain_to(to.script_pubkey());
                builder.drain_wallet();
            }
        }
        builder.fee_rate(fee_rate);
        if !unspendable.is_empty() {
            builder.unspendable(unspendable.to_vec());
        }
        let psbt = builder.finish()?;
        self.sign_and_broadcast(psbt).await
    }

    /// Signs a payment and returns the transaction without broadcasting it. Callers that move
    /// customer funds must persist the hex before `submit_signed_transfer`: ECX has no blockhash
    /// expiry that makes a lost response safe to rebuild.
    pub fn prepare_signed_transfer(
        &mut self,
        to: &Address,
        amount: Amount,
        fee_rate: FeeRate,
        unspendable: &[OutPoint],
    ) -> Result<Transaction> {
        self.prepare_earnings_transfer(to, amount, fee_rate, unspendable)
    }

    /// Broadcasts a transaction that was already signed and persisted by the caller. A lost HTTP
    /// response is resolved by asking the explorer whether this exact transaction is known; it is
    /// never treated as "safe to build a different one".
    pub async fn submit_signed_transfer(&mut self, tx: Transaction) -> Result<Txid> {
        self.submit_earnings_transfer(tx).await
    }

    pub fn prepare_earnings_transfer(
        &mut self,
        to: &Address,
        amount: Amount,
        fee_rate: FeeRate,
        unspendable: &[OutPoint],
    ) -> Result<Transaction> {
        let mut builder = self.inner.build_tx();
        builder.add_recipient(to.script_pubkey(), amount);
        builder.fee_rate(fee_rate);
        builder.unspendable(unspendable.to_vec());
        let mut psbt = builder.finish()?;
        ensure!(self.inner.sign(&mut psbt, SignOptions::default())?, "failed to finalize earnings transfer");
        self.inner.persist(&mut self.store)?;
        Ok(psbt.extract_tx()?)
    }

    pub async fn submit_earnings_transfer(&mut self, tx: Transaction) -> Result<Txid> {
        let txid = tx.compute_txid();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
        self.inner.apply_unconfirmed_txs([(tx.clone(), now)]);
        self.inner.persist(&mut self.store)?;
        if let Err(error) = self.broadcast(&tx).await {
            let known = match &self.client {
                Client::Esplora { base_url, .. } => {
                    http_client()?.get(format!("{}/tx/{txid}/hex", base_url.trim_end_matches('/')))
                        .send().await?.error_for_status()?.text().await?.trim()
                        == bdk_wallet::bitcoin::consensus::encode::serialize_hex(&tx)
                }
                Client::Electrum(client) => {
                    use bdk_electrum::electrum_client::ElectrumApi;
                    client.inner.transaction_get(&txid).is_ok_and(|known| known == tx)
                }
            };
            ensure!(known, "earnings broadcast unresolved: {error:#}");
        }
        Ok(txid)
    }

    async fn sign_and_broadcast(&mut self, mut psbt: bdk_wallet::bitcoin::psbt::Psbt) -> Result<Txid> {
        let finalized = self.inner.sign(&mut psbt, SignOptions::default())?;
        ensure!(finalized, "failed to finalize psbt");
        self.inner.persist(&mut self.store)?;

        let tx = psbt.extract_tx()?;
        print_tx_details(&tx);
        self.broadcast(&tx).await.context("broadcast failed")?;
        let txid = tx.compute_txid();

        // Neither `sign` nor `persist` above actually record this tx as spending its inputs -
        // nothing does, until something applies it. Without this, the wallet's own `balance()`
        // (and therefore `db::reserve_cache`, refreshed straight from it - see
        // `server::refresh_reserve_cache`) keeps counting these just-spent UTXOs as spendable
        // until a full `sync()`, which the running server never calls again after its one-time
        // cold-start seed (see `get_quote`/`create_order`) - a real `sync()` on every send would
        // fix it too, but at the cost of a network round-trip per payout, which is exactly what
        // `sync_addresses`/`verify_unspent` elsewhere in this file exist to avoid. Applying the
        // tx we just broadcast ourselves needs no network call at all: we already know it's real,
        // so there's nothing to ask the chain to confirm. `now` as `last_seen` is correct here
        // specifically because we just broadcast it this instant, unlike a tx learned about from
        // a sync (where the server's clock and "when it entered the mempool" can diverge).
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.inner.apply_unconfirmed_txs([(tx, now)]);
        self.inner.persist(&mut self.store)?;

        Ok(txid)
    }

    async fn broadcast(&self, tx: &Transaction) -> Result<()> {
        match &self.client {
            // esplora-client's own `.broadcast()` fails against this backend; POSTing the raw tx
            // hex straight to the standard esplora `POST /tx` endpoint works, so do that instead.
            Client::Esplora { base_url, .. } => {
                let hex = bdk_wallet::bitcoin::consensus::encode::serialize_hex(tx);
                let url = format!("{}/tx", base_url.trim_end_matches('/'));
                let response = http_client()?
                    .post(&url)
                    // Without an explicit content-type, this server's body parser never picks up
                    // the raw hex body at all (it silently no-ops), so `sendrawtransaction` gets
                    // called with nothing and always returns a generic `{"code":-1}` regardless
                    // of the tx - confirmed by testing against the endpoint directly.
                    .header(reqwest::header::CONTENT_TYPE, "text/plain")
                    .body(hex)
                    .send()
                    .await
                    .context("esplora broadcast request failed")?;
                let status = response.status();
                if !status.is_success() {
                    let body = response.text().await.unwrap_or_default();
                    anyhow::bail!("esplora broadcast returned {status}: {body}");
                }
            }
            Client::Electrum(client) => client.transaction_broadcast(tx).map(|_| ())?,
        }
        Ok(())
    }

    /// Address that contributed the most value to `txids`, skipping this wallet's own addresses.
    /// A refund goes there. Address reuse is intentional: the deposit's inputs are the only
    /// place the coins can be sent back to without asking the customer for a destination.
    pub async fn funding_address(&self, txids: &[Txid]) -> Result<Address> {
        let mut seen = std::collections::HashSet::new();
        let mut contributions = Vec::new();
        for txid in txids {
            if !seen.insert(*txid) {
                continue;
            }
            contributions.extend(self.input_contributions(*txid).await?);
        }
        let best = dominant_return_address(&contributions, |address| {
            address
                .parse::<Address<_>>()
                .ok()
                .and_then(|parsed| parsed.require_network(Network::Bitcoin).ok())
                .is_some_and(|parsed| self.owns_address(&parsed))
        })
        .ok_or_else(|| anyhow!("could not determine the address this deposit came from"))?;
        Ok(best.parse::<Address<_>>()?.require_network(Network::Bitcoin)?)
    }

    /// Signs a refund that spends only `outpoints` (the deposit) and pays `send_sat` to `to`.
    /// The rest, after the network fee, is change and stays in this wallet: that is the
    /// restocking fee. The caller waits until the deposit is confirmed before asking for this.
    /// Other treasury coins are not added: if the deposit is replaced, this transaction dies
    /// with it instead of spending the float.
    ///
    /// Returns the transaction and the amount actually paid to `to`. If `send_sat` does not
    /// leave enough for the network fee, the payment is reduced until the deposit can cover it.
    pub fn prepare_return_from_deposits(
        &mut self,
        to: &Address,
        outpoints: &[OutPoint],
        send_sat: u64,
        fee_rate: FeeRate,
    ) -> Result<(Transaction, u64)> {
        ensure!(!outpoints.is_empty(), "deposit outputs are not spendable yet");
        let dust = 546u64;
        let mut send = send_sat;
        let mut last_err = None;
        for _ in 0..12 {
            if send < dust {
                break;
            }
            let mut builder = self.inner.build_tx();
            builder
                .add_utxos(outpoints)
                .map_err(|e| anyhow!("deposit output is not spendable yet: {e}"))?;
            builder.manually_selected_only();
            builder.add_recipient(to.script_pubkey(), Amount::from_sat(send));
            builder.fee_rate(fee_rate);
            match builder.finish() {
                Ok(mut psbt) => {
                    ensure!(self.inner.sign(&mut psbt, SignOptions::default())?, "failed to sign refund");
                    self.inner.persist(&mut self.store)?;
                    let tx = psbt.extract_tx()?;
                    let sent = tx
                        .output
                        .iter()
                        .filter(|output| output.script_pubkey == to.script_pubkey())
                        .map(|output| output.value.to_sat())
                        .sum();
                    return Ok((tx, sent));
                }
                Err(error @ CreateTxError::CoinSelection(_))
                | Err(error @ CreateTxError::FeeRateTooLow { .. })
                | Err(error @ CreateTxError::FeeTooLow { .. })
                | Err(error @ CreateTxError::OutputBelowDustLimit(_)) => {
                    last_err = Some(error.to_string());
                    send = send.saturating_sub(200);
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(anyhow!(
            "{}",
            last_err.unwrap_or_else(|| "refund could not be built from the deposit".to_string())
        ))
    }

    /// Satoshis of `tx` that pay this wallet. That is the restocking fee left after a return,
    /// not the network fee.
    pub fn change_value(&self, tx: &Transaction) -> u64 {
        tx.output
            .iter()
            .filter_map(|output| {
                let address = Address::from_script(&output.script_pubkey, Network::Bitcoin).ok()?;
                self.owns_address(&address).then_some(output.value.to_sat())
            })
            .sum()
    }

    async fn input_contributions(&self, txid: Txid) -> Result<Vec<(String, u64)>> {
        match &self.client {
            Client::Esplora { base_url, .. } => esplora_input_contributions(base_url, txid).await,
            Client::Electrum(client) => electrum_input_contributions(client, txid),
        }
    }

    /// Confirmation depth of `txid` from the wallet's own synced state (0 if unconfirmed or
    /// unknown). Call `sync()` first to refresh - this doesn't hit the network itself, so it
    /// works identically regardless of which backend is configured.
    pub fn confirmations(&self, txid: Txid) -> u32 {
        let Some(tx) = self.inner.get_tx(txid) else {
            return 0;
        };
        match tx.chain_position {
            bdk_wallet::chain::ChainPosition::Confirmed { anchor, .. } => {
                let tip = self.inner.latest_checkpoint().height();
                tip.saturating_sub(anchor.block_id.height) + 1
            }
            bdk_wallet::chain::ChainPosition::Unconfirmed { .. } => 0,
        }
    }
}

#[derive(serde::Deserialize)]
struct EsploraOutspend {
    spent: bool,
    /// The spending transaction's txid - present whenever `spent` is `true`. Deserialized as a
    /// plain string rather than `bitcoin::Txid` directly, so this doesn't need `bitcoin`'s `serde`
    /// feature enabled just for this one field; parsed on use instead (see `verify_unspent`).
    txid: Option<String>,
}

struct SpentOutpoint {
    outpoint: OutPoint,
    /// `None` if the backend reported `spent: true` without a usable txid (unexpected, but not
    /// treated as fatal - see `verify_unspent`, which still excludes `outpoint` from coin
    /// selection even without one, just can't apply the spend to local state).
    spending_txid: Option<Txid>,
}

/// Checks which of `outpoints` are already spent, per this backend's `GET /tx/:txid/outspends`
/// (see the comment on `verify_unspent` for why that's used instead of BDK's built-in outpoint
/// sync). Batches by source txid - one request per distinct txid among `outpoints`, covering every
/// vout of that tx in one response, rather than one request per outpoint.
async fn esplora_spent_outpoints(base_url: &str, outpoints: &[OutPoint]) -> Result<Vec<SpentOutpoint>> {
    let mut by_txid: HashMap<Txid, Vec<u32>> = HashMap::new();
    for op in outpoints {
        by_txid.entry(op.txid).or_default().push(op.vout);
    }

    let http = http_client()?;
    let mut spent = Vec::new();
    for (txid, vouts) in by_txid {
        let url = format!("{}/tx/{txid}/outspends", base_url.trim_end_matches('/'));
        let response = http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("esplora outspends request failed for {txid}"))?;
        if !response.status().is_success() {
            // A source tx this backend hasn't indexed (yet, or ever) can't be confirmed spent -
            // leave it as a coin-selection candidate rather than failing the whole check over it.
            continue;
        }
        let statuses: Vec<EsploraOutspend> = response
            .json()
            .await
            .with_context(|| format!("failed to parse esplora outspends response for {txid}"))?;
        for vout in vouts {
            if let Some(status) = statuses.get(vout as usize)
                && status.spent
            {
                let spending_txid = status.txid.as_deref().and_then(|t| t.parse::<Txid>().ok());
                spent.push(SpentOutpoint { outpoint: OutPoint { txid, vout }, spending_txid });
            }
        }
    }
    Ok(spent)
}

/// Prints the finalized tx (locktime, per-input sequence, outputs, raw hex) before it's handed to
/// `broadcast()`, so a rejected broadcast can be debugged without having to re-derive the tx from
/// node logs.
fn print_tx_details(tx: &Transaction) {
    println!("--- tx details (pre-broadcast) ---");
    println!("txid: {}", tx.compute_txid());
    println!("version: {}", tx.version);
    let locktime = tx.lock_time;
    println!(
        "locktime: {} ({})",
        locktime,
        match locktime {
            LockTime::Blocks(h) => format!("block height {h}"),
            LockTime::Seconds(t) => format!("unix timestamp {t}"),
        }
    );
    println!("inputs:");
    for input in &tx.input {
        println!(
            "  {} sequence={:#010x} ({}final{})",
            input.previous_output,
            input.sequence.0,
            if input.sequence.is_final() { "" } else { "non-" },
            if input.sequence.is_rbf() { ", rbf-signaling" } else { "" },
        );
    }
    println!("outputs:");
    for out in &tx.output {
        println!("  {} sats -> {}", out.value.to_sat(), out.script_pubkey);
    }
    println!(
        "weight: {} vsize: {}",
        tx.weight(),
        tx.vsize()
    );
    println!(
        "raw hex: {}",
        bdk_wallet::bitcoin::consensus::encode::serialize_hex(tx)
    );
    println!("--- end tx details ---");
}

/// The address that paid the most across `contributions` of `(address, satoshis)`, skipping
/// addresses `is_own` accepts. Sums repeated addresses. `None` when every input was ours, missing,
/// or empty.
fn dominant_return_address(
    contributions: &[(String, u64)],
    is_own: impl Fn(&str) -> bool,
) -> Option<String> {
    let mut totals: HashMap<&str, u64> = HashMap::new();
    for (address, value) in contributions {
        if *value == 0 || is_own(address) {
            continue;
        }
        let total = totals.entry(address.as_str()).or_insert(0);
        *total = total.saturating_add(*value);
    }
    totals
        .into_iter()
        .max_by_key(|(_, value)| *value)
        .map(|(address, _)| address.to_string())
}

#[derive(serde::Deserialize)]
struct EsploraPrevout {
    scriptpubkey_address: Option<String>,
    value: u64,
}

#[derive(serde::Deserialize)]
struct EsploraVin {
    prevout: Option<EsploraPrevout>,
}

#[derive(serde::Deserialize)]
struct EsploraTx {
    vin: Vec<EsploraVin>,
}

async fn esplora_input_contributions(base_url: &str, txid: Txid) -> Result<Vec<(String, u64)>> {
    let url = format!("{}/tx/{txid}", base_url.trim_end_matches('/'));
    let tx: EsploraTx = http_client()?
        .get(&url)
        .send()
        .await
        .with_context(|| format!("esplora transaction request failed for {txid}"))?
        .error_for_status()
        .with_context(|| format!("esplora has no transaction {txid}"))?
        .json()
        .await
        .with_context(|| format!("failed to parse esplora transaction {txid}"))?;
    Ok(tx
        .vin
        .into_iter()
        .filter_map(|input| {
            let prevout = input.prevout?;
            let address = prevout.scriptpubkey_address?;
            Some((address, prevout.value))
        })
        .collect())
}

fn electrum_input_contributions(
    client: &BdkElectrumClient<electrum_client::Client>,
    txid: Txid,
) -> Result<Vec<(String, u64)>> {
    use bdk_electrum::electrum_client::ElectrumApi;
    let tx = client
        .inner
        .transaction_get(&txid)
        .with_context(|| format!("electrum transaction request failed for {txid}"))?;
    let mut contributions = Vec::new();
    for input in &tx.input {
        let prev = match client.inner.transaction_get(&input.previous_output.txid) {
            Ok(prev) => prev,
            Err(_) => continue,
        };
        let Some(output) = prev.output.get(input.previous_output.vout as usize) else {
            continue;
        };
        let Ok(address) = Address::from_script(&output.script_pubkey, Network::Bitcoin) else {
            continue;
        };
        contributions.push((address.to_string(), output.value.to_sat()));
    }
    Ok(contributions)
}

#[cfg(test)]
mod ownership_tests {
    use super::*;
    use bdk_wallet::bitcoin::Network;

    // BIP32 test vector 1 master key: a public throwaway that has never held funds.
    const XPRV: &str = "xprv9s21ZrQH143K3QTDL4LXw2F7HEK3wJUD2nW2nRk4stbPy6cq3jPPqjiChkVvvNKmPGJxWUtg6LnF5kejMRNNU3TGtRBeJgk33yuGBxrMPHi";

    #[test]
    fn owns_address_covers_revealed_unrevealed_and_rejects_foreign_addresses() {
        let dir = std::env::temp_dir().join(format!("wallet-owns-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = ChainConfig {
            network: Network::Bitcoin,
            backend: Backend::Esplora,
            esplora_url: "http://127.0.0.1:9".to_string(),
            electrum_url: String::new(),
            db_path: dir.join("ecx.db"),
        };
        let mut wallet = ChainWallet::open(
            &config,
            &format!("wpkh({XPRV}/84h/0h/0h/0/*)"),
            &format!("wpkh({XPRV}/84h/0h/0h/1/*)"),
        )
        .unwrap();

        let revealed = wallet.fresh_address().unwrap();
        assert!(wallet.owns_address(&revealed), "a revealed deposit address is ours");

        let deep_change = wallet.inner.peek_address(KeychainKind::Internal, 999).address;
        assert!(wallet.owns_address(&deep_change), "an unrevealed address within the peek depth is ours");

        let foreign: Address = "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4"
            .parse::<Address<_>>()
            .unwrap()
            .require_network(Network::Bitcoin)
            .unwrap();
        assert!(!wallet.owns_address(&foreign), "another wallet's address is not ours");

        drop(wallet);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn return_address_is_the_largest_input_that_is_not_ours() {
        let contributions = vec![
            ("bc1qours".to_string(), 50_000),
            ("bc1qsmall".to_string(), 1_000),
            ("bc1qsmall".to_string(), 2_000),
            ("bc1qlarge".to_string(), 9_000),
        ];
        assert_eq!(
            dominant_return_address(&contributions, |address| address == "bc1qours").as_deref(),
            Some("bc1qlarge")
        );
        assert_eq!(
            dominant_return_address(&contributions, |_| true),
            None,
            "a deposit funded only by this wallet has nowhere to return it"
        );
        assert_eq!(dominant_return_address(&[], |_| false), None);
    }
}
