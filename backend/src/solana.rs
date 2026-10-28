use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
use solana_client::rpc_config::{RpcSendTransactionConfig, RpcTransactionConfig};
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature, Signer};
use solana_sdk::transaction::Transaction;
use solana_transaction_status_client_types::{
    EncodedConfirmedTransactionWithStatusMeta, EncodedTransaction, TransactionConfirmationStatus,
    UiLoadedAddresses, UiMessage, UiTransactionEncoding,
};
use spl_associated_token_account::get_associated_token_address;
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;
use spl_token::instruction::transfer_checked;

/// Custodial treasury wallet for the wECX side of the bridge: holds a balance of already-minted
/// wECX (an existing SPL token - this backend has no mint/freeze authority over it, only over its
/// own token account) and sends from it on ECX deposits. Mirrors `wallet::ChainWallet`'s role on
/// the ECX side, but there's no local persisted state here - Solana account state is always read
/// live from `rpc`.
pub struct SolanaWallet {
    rpc: RpcClient,
    payer: Keypair,
    mint: Pubkey,
    decimals: u8,
}

impl SolanaWallet {
    /// Connects to `rpc_url`, loads the treasury keypair from `keypair_base58` (a
    /// `solana-keygen`-style base58-encoded 64-byte secret key - see `SOLANA_KEYPAIR` in
    /// config.rs), and fetches `mint`'s decimals once so every amount conversion after this uses
    /// a value read from the mint itself rather than a hardcoded/guessed constant.
    pub async fn open(rpc_url: &str, keypair_base58: &str, mint: Pubkey) -> Result<Self> {
        let payer = keypair_from_env(keypair_base58)?;

        let rpc = RpcClient::new_with_commitment(rpc_url.to_string(), CommitmentConfig::confirmed());

        let supply = rpc
            .get_token_supply(&mint)
            .await
            .context("failed to fetch wECX mint info - check WECX_MINT_ADDRESS/SOLANA_RPC_URL")?;

        // `convert::ecx_sat_to_wecx_base_units` computes 10^decimals in u128; a mint reporting
        // something absurd would overflow (panic) there. No real SPL token comes close.
        if supply.decimals > 18 {
            bail!("wECX mint reports {} decimals - refusing to run (expected <= 18)", supply.decimals);
        }

        Ok(Self { rpc, payer, mint, decimals: supply.decimals })
    }

    #[cfg(test)]
    pub(crate) fn for_test(payer: Keypair, mint: Pubkey, decimals: u8) -> Self {
        let rpc = RpcClient::new_with_commitment("http://127.0.0.1:9".to_string(), CommitmentConfig::confirmed());
        Self { rpc, payer, mint, decimals }
    }

    pub fn pubkey(&self) -> Pubkey {
        self.payer.pubkey()
    }

    pub fn mint(&self) -> Pubkey {
        self.mint
    }

    /// The treasury's own associated token account for the wECX mint - where peg-in payouts are
    /// sent from and where peg-out (wECX->ECX) deposits are expected to land.
    pub fn treasury_ata(&self) -> Pubkey {
        get_associated_token_address(&self.payer.pubkey(), &self.mint)
    }

    /// Decimals of the wECX mint, fetched once at `open()` time.
    pub fn decimals(&self) -> u8 {
        self.decimals
    }

    /// SOL balance of the treasury keypair itself, in lamports - needed to pay tx fees and (for a
    /// recipient's first payout) associated-token-account rent. Not the wECX balance; see
    /// `treasury_token_balance` for that.
    pub async fn sol_balance_lamports(&self) -> Result<u64> {
        self.rpc
            .get_balance(&self.payer.pubkey())
            .await
            .context("failed to fetch treasury SOL balance")
    }

    /// wECX balance of the treasury's own associated token account, in base units (i.e. already
    /// scaled by `decimals`). `Ok(0)` if the account has never been created yet, rather than an
    /// error - a treasury that's never received any wECX is a valid (if useless) state.
    pub async fn treasury_token_balance(&self) -> Result<u64> {
        let ata = get_associated_token_address(&self.payer.pubkey(), &self.mint);
        match self.rpc.get_token_account_balance(&ata).await {
            Ok(balance) => balance
                .amount
                .parse()
                .context("unexpected token account balance format from RPC"),
            Err(e) if e.to_string().to_lowercase().contains("could not find account") => Ok(0),
            Err(e) => Err(e).context("failed to fetch treasury wECX balance"),
        }
    }

    /// Checks that `recipient` is an address wECX can sensibly be paid to. Sends are irreversible,
    /// and `parse_pubkey` alone accepts any 32 bytes - including a mangled address, a program id, the
    /// mint, or the treasury itself. Off-curve addresses (PDAs) are rejected unless
    /// `ALLOW_OFF_CURVE_RECIPIENTS` is set - see `config::allow_off_curve_recipients`.
    /// Lamports this payout must spend to create `recipient`'s token account. `Ok(0)` when that
    /// account already exists. The figure is the chain's rent-exempt minimum, read once so the
    /// order can lock a 1:1 native-coin charge. A missing account is not an error; an RPC failure is.
    pub async fn missing_recipient_ata_rent(&self, recipient: &Pubkey) -> Result<u64> {
        let ata = get_associated_token_address(recipient, &self.mint);
        match self.rpc.get_account(&ata).await {
            Ok(account) => {
                if account.owner != spl_token::id() {
                    bail!(
                        "that address already has an account where the token account must go, and it is not a token account"
                    );
                }
                Ok(0)
            }
            Err(e) if account_missing(&e) => self
                .rpc
                .get_minimum_balance_for_rent_exemption(TOKEN_ACCOUNT_LEN)
                .await
                .context("failed to read token-account rent"),
            Err(e) => Err(e).context("failed to check recipient token account"),
        }
    }

    pub fn validate_recipient(&self, recipient: &Pubkey) -> Result<()> {
        let reserved = [
            solana_sdk::system_program::id(),
            spl_token::id(),
            spl_associated_token_account::id(),
            self.mint,
            self.payer.pubkey(),
        ];
        match recipient_problem(recipient, &reserved, crate::config::allow_off_curve_recipients()) {
            Some("reserved") => bail!(
                "that address can't receive {} (it is a program, the mint, or the bridge itself)",
                crate::config::wrapped_ticker()
            ),
            Some(problem) => bail!("{problem}"),
            None => Ok(()),
        }
    }

    /// Builds and signs (but does not send) the transfer of `amount` (base units, already scaled by
    /// `decimals`) of wECX from the treasury to `recipient`'s associated token account, creating
    /// that account first if it doesn't exist yet (idempotently). `sol_lamports`, when non-zero,
    /// is a native SOL transfer to the same wallet in the same transaction, so the token and the
    /// gas stipend land together or not at all. The treasury pays the transaction fee and any
    /// token-account rent. The returned `signature` is final: callers persist it *before*
    /// `submit`ting, so an ambiguous outcome can always be looked up afterwards instead of guessed at.
    pub async fn prepare_wecx_transfer(
        &self,
        recipient: Pubkey,
        amount: u64,
        sol_lamports: u64,
    ) -> Result<PreparedPayout> {
        let source_ata = get_associated_token_address(&self.payer.pubkey(), &self.mint);
        let dest_ata = get_associated_token_address(&recipient, &self.mint);

        let mut instructions = vec![
            create_associated_token_account_idempotent(
                &self.payer.pubkey(),
                &recipient,
                &self.mint,
                &spl_token::id(),
            ),
            transfer_checked(
                &spl_token::id(),
                &source_ata,
                &self.mint,
                &dest_ata,
                &self.payer.pubkey(),
                &[],
                amount,
                self.decimals,
            )
            .context("failed to build wECX transfer instruction")?,
        ];
        if sol_lamports > 0 {
            instructions.push(solana_sdk::system_instruction::transfer(
                &self.payer.pubkey(),
                &recipient,
                sol_lamports,
            ));
        }

        let (blockhash, last_valid_block_height) = self
            .rpc
            .get_latest_blockhash_with_commitment(CommitmentConfig::confirmed())
            .await
            .context("failed to fetch latest Solana blockhash")?;
        let tx = Transaction::new_signed_with_payer(
            &instructions,
            Some(&self.payer.pubkey()),
            &[&self.payer],
            blockhash,
        );
        let signature = tx.signatures[0];

        Ok(PreparedPayout { tx, signature, last_valid_block_height })
    }

    /// Broadcasts a prepared transfer and waits for it to resolve.
    ///
    /// `Ok` means the outcome is *known*: `Confirmed`, `Failed` (landed and errored on-chain, so the
    /// transfer did not happen), or `Expired` (its blockhash aged out without it ever landing, so
    /// it never can). `Err` means the outcome is *unknown* - the RPC stopped answering while the
    /// transaction may still have landed - and the caller must NOT treat the payout as unsent.
    pub async fn submit(&self, prepared: &PreparedPayout) -> Result<PayoutOutcome> {
        let commitment = CommitmentConfig::confirmed();
        let send_config = RpcSendTransactionConfig {
            preflight_commitment: Some(commitment.commitment),
            ..Default::default()
        };

        // A failed initial send is not conclusive either way (a network error may still have
        // delivered it; a preflight rejection means it never can land) - the status/expiry polling
        // below is what decides, so just log and carry on.
        if let Err(e) = self.rpc.send_transaction_with_config(&prepared.tx, send_config).await {
            eprintln!(
                "[solana] initial send of {} reported: {}",
                prepared.signature,
                crate::config::redact_urls(&e.to_string())
            );
        }

        let mut last_send = Instant::now();
        let mut consecutive_rpc_errors = 0u32;
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;

            match self.rpc.get_signature_statuses(&[prepared.signature]).await {
                Ok(response) => {
                    consecutive_rpc_errors = 0;
                    if let Some(Some(status)) = response.value.first() {
                        if let Some(err) = &status.err {
                            return Ok(PayoutOutcome::Failed(err.to_string()));
                        }
                        if status.satisfies_commitment(commitment) {
                            return Ok(PayoutOutcome::Confirmed);
                        }
                        continue;
                    }
                }
                Err(e) => {
                    consecutive_rpc_errors += 1;
                    if consecutive_rpc_errors >= 10 {
                        return Err(e).context("lost contact with the Solana RPC while awaiting payout confirmation");
                    }
                    continue;
                }
            }

            // Not seen yet. Once the chain is past the last block this transaction's blockhash is
            // valid for, it can never land - but check history one last time before declaring that,
            // in case it landed in the final blocks.
            match self.rpc.get_block_height_with_commitment(commitment).await {
                Ok(height) if height > prepared.last_valid_block_height => {
                    match self.signature_outcome(&prepared.signature).await? {
                        SignatureOutcome::Confirmed => return Ok(PayoutOutcome::Confirmed),
                        SignatureOutcome::Failed(e) => return Ok(PayoutOutcome::Failed(e)),
                        SignatureOutcome::NotFound => return Ok(PayoutOutcome::Expired),
                        // Landed but not confirmed yet - keep waiting rather than call it expired.
                        SignatureOutcome::Pending => continue,
                    }
                }
                _ => {}
            }

            if last_send.elapsed() >= Duration::from_secs(10) {
                let _ = self.rpc.send_transaction_with_config(&prepared.tx, send_config).await;
                last_send = Instant::now();
            }
        }
    }

    /// Looks a transaction up by signature, searching full ledger history (not just the RPC's
    /// short recent-status cache, which a transaction can age out of). Used to resolve payouts left
    /// `paying_out` after an ambiguous outcome - see `server::try_resolve_payout`. Relies on the
    /// RPC node serving history; `NotFound` from a node that doesn't would be wrong, so use a
    /// provider that does (the public mainnet-beta endpoint does).
    pub async fn signature_outcome(&self, signature: &Signature) -> Result<SignatureOutcome> {
        let response = self
            .rpc
            .get_signature_statuses_with_history(&[*signature])
            .await
            .context("failed to look up payout transaction status")?;
        Ok(match response.value.into_iter().next().flatten() {
            Some(status) => match status.err {
                Some(err) => SignatureOutcome::Failed(err.to_string()),
                None if status.satisfies_commitment(CommitmentConfig::confirmed()) => SignatureOutcome::Confirmed,
                // Seen but not yet confirmed - still in flight, not lost.
                None => SignatureOutcome::Pending,
            },
            None => SignatureOutcome::NotFound,
        })
    }

    /// Whether `signature` has reached the `finalized` commitment level - stronger than the
    /// `confirmed` level `signature_outcome`/`submit` use elsewhere, because a peg-out deposit
    /// being "seen" is what triggers an automatic outbound ECX send (see
    /// `server::try_fulfill_pegout`): unlike a peg-in ECX deposit (protected by
    /// `config::deposit_min_confs`), Solana has no block-count analogue, so `finalized` is the
    /// bar for treating an incoming SPL transfer as irreversible. Returns `false` (not an error)
    /// if the signature isn't found at all - callers already have the signature on file from a
    /// prior `find_reference_deposit`, so "not found yet" just means try again next poll.
    pub async fn signature_finalized(&self, signature: &Signature) -> Result<bool> {
        let response = self
            .rpc
            .get_signature_statuses_with_history(&[*signature])
            .await
            .context("failed to look up peg-out deposit transaction status")?;
        Ok(match response.value.into_iter().next().flatten() {
            Some(status) if status.err.is_none() => {
                status.satisfies_commitment(CommitmentConfig::finalized())
            }
            _ => false,
        })
    }

    /// Looks for a wECX deposit into the treasury's ATA whose transaction includes `reference` as
    /// one of its account keys - the Solana Pay "reference key" convention this bridge's peg-out
    /// (wECX->ECX) direction uses to tell orders apart, since (unlike ECX) there's no way to hand
    /// out a fresh receive *address* per order on Solana. `reference` is a
    /// bare pubkey generated per order (see `server::create_pegout_order`) with no keypair kept -
    /// it's never a signer, only an extra readonly account included in the customer's transfer
    /// instruction for this lookup to find.
    ///
    /// Matches by token-balance delta (`postTokenBalances` minus `preTokenBalances` for the
    /// treasury ATA), not by decoding the transfer instruction itself - this works identically for
    /// `Transfer` and `TransferChecked`, multisig or single-owner sources, and needs no
    /// instruction-format assumptions. `depositor_owner` (for a wECX refund if this deposit can't
    /// be bridged - see `server::try_refund_pegout`) is extracted the same way: the other account
    /// of the same mint whose balance went down, and its recorded `owner`. `None` if that can't be
    /// determined (e.g. an exotic multisig source) - the caller falls back to manual review rather
    /// than guessing.
    ///
    /// Only ever returns a transaction that has at least reached `confirmed` (never `processed`,
    /// which can still be dropped) - see `GetConfirmedSignaturesForAddress2Config`'s default
    /// commitment. `finalized` on the return value additionally reports whether it already meets
    /// the bar `try_fulfill_pegout` requires before paying out.
    pub async fn find_reference_deposit(
        &self,
        reference: &Pubkey,
        minimum_base_units: u64,
    ) -> Result<Option<ReferenceDeposit>> {
        // Walk newest-page backward, then consider candidates oldest-first. A dust transfer that
        // includes the reference must not become the order's deposit: that would close the order
        // and leave the customer's later payment unmatched. Cap the walk so one reference cannot
        // turn a poll tick into an unbounded RPC loop.
        let mut before: Option<Signature> = None;
        let mut newest_first = Vec::new();
        for _page in 0..8 {
            let page = self
                .rpc
                .get_signatures_for_address_with_config(
                    reference,
                    GetConfirmedSignaturesForAddress2Config {
                        before,
                        until: None,
                        limit: Some(25),
                        commitment: Some(CommitmentConfig::confirmed()),
                    },
                )
                .await
                .context("failed to look up peg-out reference key signatures")?;
            if page.is_empty() {
                break;
            }
            let oldest = page
                .last()
                .and_then(|entry| entry.signature.parse::<Signature>().ok());
            let page_len = page.len();
            newest_first.extend(page);
            if page_len < 25 {
                break;
            }
            before = oldest;
            if before.is_none() {
                break;
            }
        }

        // Oldest first among the pages we fetched. The earliest transfer that meets
        // `minimum_base_units` is authoritative.
        for entry in newest_first.into_iter().rev() {
            if entry.err.is_some() {
                continue;
            }
            let signature: Signature = entry
                .signature
                .parse()
                .context("solana RPC returned an unparsable signature")?;
            let finalized = matches!(entry.confirmation_status, Some(TransactionConfirmationStatus::Finalized));

            if let Some(deposit) = self.fetch_treasury_deposit(&signature, finalized).await? {
                if deposit.amount_base_units < minimum_base_units {
                    continue;
                }
                return Ok(Some(deposit));
            }
        }
        Ok(None)
    }

    /// Fetches one transaction and reads it as a wECX deposit into the treasury - `Ok(None)` when
    /// it moved no wECX into the treasury token account. Shared by the per-order reference lookup
    /// and the treasury-wide scan for deposits that carry no reference at all.
    pub async fn fetch_treasury_deposit(
        &self,
        signature: &Signature,
        finalized: bool,
    ) -> Result<Option<ReferenceDeposit>> {
        let tx = self
            .rpc
            .get_transaction_with_config(
                signature,
                RpcTransactionConfig {
                    encoding: Some(UiTransactionEncoding::Json),
                    commitment: Some(CommitmentConfig::confirmed()),
                    max_supported_transaction_version: Some(0),
                },
            )
            .await
            .context("failed to fetch treasury deposit transaction")?;
        self.parse_reference_deposit(&tx, *signature, finalized)
    }

    /// Lists the treasury token account's transaction signatures newer than `until`, oldest
    /// first, so a caller can walk them in order and record how far it got. `until` is the last
    /// signature a previous walk processed; `None` lists the most recent page only, for a first
    /// run that wants to start from "now" rather than examine the whole history.
    ///
    /// The RPC pages newest-first, so reaching `until` can take several pages after a busy period.
    /// Paging is capped; `complete` is `false` when the cap was hit before `until` was found, in
    /// which case the entries older than the oldest returned one were skipped and the caller should
    /// say so, since advancing its cursor past them loses them.
    pub async fn list_treasury_signatures(&self, until: Option<&Signature>) -> Result<TreasurySignatures> {
        const PAGE: usize = 1_000;
        const MAX_PAGES: usize = 10;
        let treasury_ata = self.treasury_ata();
        let mut before: Option<Signature> = None;
        let mut newest_first = Vec::new();
        let mut complete = false;
        for _page in 0..MAX_PAGES {
            let page = self
                .rpc
                .get_signatures_for_address_with_config(
                    &treasury_ata,
                    GetConfirmedSignaturesForAddress2Config {
                        before,
                        until: until.copied(),
                        limit: Some(PAGE),
                        commitment: Some(CommitmentConfig::confirmed()),
                    },
                )
                .await
                .context("failed to list treasury token account signatures")?;
            let page_len = page.len();
            let oldest = page.last().and_then(|entry| entry.signature.parse::<Signature>().ok());
            newest_first.extend(page);
            if page_len < PAGE || until.is_none() {
                complete = true;
                break;
            }
            before = oldest;
            if before.is_none() {
                complete = true;
                break;
            }
        }
        let mut entries = Vec::with_capacity(newest_first.len());
        for entry in newest_first.into_iter().rev() {
            entries.push(TreasurySignature {
                signature: entry.signature.parse().context("solana RPC returned an unparsable signature")?,
                finalized: matches!(entry.confirmation_status, Some(TransactionConfirmationStatus::Finalized)),
                failed: entry.err.is_some(),
            });
        }
        Ok(TreasurySignatures { entries, complete })
    }

    /// Pure parsing core of `find_reference_deposit`: extracts the treasury's wECX balance delta
    /// and (best-effort) the depositor's owner pubkey from an already-fetched transaction. `Ok(None)`
    /// means the transaction didn't actually move any wECX into the treasury ATA (e.g. the
    /// reference key was included in an unrelated instruction) - not an error, just "keep looking".
    fn parse_reference_deposit(
        &self,
        tx: &EncodedConfirmedTransactionWithStatusMeta,
        signature: Signature,
        finalized: bool,
    ) -> Result<Option<ReferenceDeposit>> {
        let Some(meta) = &tx.transaction.meta else {
            return Ok(None);
        };
        if meta.err.is_some() {
            return Ok(None);
        }
        let EncodedTransaction::Json(ui_tx) = &tx.transaction.transaction else {
            bail!("unexpected transaction encoding from Solana RPC (expected json)");
        };
        let UiMessage::Raw(message) = &ui_tx.message else {
            bail!("unexpected transaction message encoding from Solana RPC (expected raw)");
        };

        // Loaded-address-table accounts are appended after the transaction's own static keys, in
        // writable-then-readonly order - required to correctly resolve `account_index` on a
        // versioned transaction that uses one (see `UiTransactionStatusMeta::loaded_addresses`).
        let mut account_keys = message.account_keys.clone();
        let loaded: Option<UiLoadedAddresses> = meta.loaded_addresses.clone().into();
        if let Some(loaded) = loaded {
            account_keys.extend(loaded.writable);
            account_keys.extend(loaded.readonly);
        }

        let post_balances: Vec<_> = Option::from(meta.post_token_balances.clone()).unwrap_or_default();
        let pre_balances: Vec<_> = Option::from(meta.pre_token_balances.clone()).unwrap_or_default();

        let treasury_ata = self.treasury_ata().to_string();
        let mint = self.mint.to_string();

        let Some(treasury_post) = post_balances.iter().find(|b| {
            b.mint == mint && account_keys.get(b.account_index as usize).map(String::as_str) == Some(treasury_ata.as_str())
        }) else {
            return Ok(None);
        };
        let post_amount: u64 = treasury_post
            .ui_token_amount
            .amount
            .parse()
            .context("unexpected post token balance amount from Solana RPC")?;
        let pre_amount: u64 = match pre_balances.iter().find(|b| b.account_index == treasury_post.account_index) {
            Some(b) => b
                .ui_token_amount
                .amount
                .parse()
                .context("unexpected pre token balance amount from Solana RPC")?,
            None => 0,
        };
        let deposited = post_amount.saturating_sub(pre_amount);
        if deposited == 0 {
            return Ok(None);
        }

        // Best-effort: the other account of the same mint whose balance went down, and its owner -
        // where a wECX refund would go if this deposit turns out not to be bridgeable.
        let depositor_owner = pre_balances
            .iter()
            .filter(|pre| pre.mint == mint && pre.account_index != treasury_post.account_index)
            .find_map(|pre| {
                let pre_amt: u64 = pre.ui_token_amount.amount.parse().ok()?;
                let post_amt: u64 = post_balances
                    .iter()
                    .find(|p| p.account_index == pre.account_index)
                    .and_then(|p| p.ui_token_amount.amount.parse().ok())
                    .unwrap_or(0);
                if post_amt >= pre_amt {
                    return None;
                }
                let owner: Option<String> = pre.owner.clone().into();
                owner.and_then(|o| o.parse::<Pubkey>().ok())
            });

        Ok(Some(ReferenceDeposit {
            signature,
            amount_base_units: deposited,
            depositor_owner,
            finalized,
            account_keys,
        }))
    }

    /// Convenience for the manual CLI path: prepare, submit, and turn anything but a confirmed
    /// transfer into an error. Blocks until the network reports the transaction confirmed.
    pub async fn send_wecx(&self, recipient: Pubkey, amount: u64) -> Result<Signature> {
        let prepared = self.prepare_wecx_transfer(recipient, amount, 0).await?;
        match self.submit(&prepared).await? {
            PayoutOutcome::Confirmed => Ok(prepared.signature),
            PayoutOutcome::Failed(e) => bail!("wECX transfer {} failed on-chain: {e}", prepared.signature),
            PayoutOutcome::Expired => bail!(
                "wECX transfer {} expired before landing (nothing was sent) - safe to retry",
                prepared.signature
            ),
        }
    }

    /// Convenience for the manual CLI path: prepare, submit, and turn anything but a confirmed
    /// native SOL transfer into an error.
    pub async fn send_sol(&self, recipient: Pubkey, lamports: u64) -> Result<Signature> {
        let prepared = self.prepare_sol_transfer(recipient, lamports).await?;
        match self.submit(&prepared).await? {
            PayoutOutcome::Confirmed => Ok(prepared.signature),
            PayoutOutcome::Failed(e) => bail!("SOL transfer {} failed on-chain: {e}", prepared.signature),
            PayoutOutcome::Expired => bail!(
                "SOL transfer {} expired before landing (nothing was sent) - safe to retry",
                prepared.signature
            ),
        }
    }

    async fn prepare_sol_transfer(&self, recipient: Pubkey, lamports: u64) -> Result<PreparedPayout> {
        let instruction = solana_sdk::system_instruction::transfer(&self.payer.pubkey(), &recipient, lamports);
        let (blockhash, last_valid_block_height) = self
            .rpc
            .get_latest_blockhash_with_commitment(CommitmentConfig::confirmed())
            .await
            .context("failed to fetch latest Solana blockhash")?;
        let tx = Transaction::new_signed_with_payer(
            &[instruction],
            Some(&self.payer.pubkey()),
            &[&self.payer],
            blockhash,
        );
        let signature = tx.signatures[0];

        Ok(PreparedPayout { tx, signature, last_valid_block_height })
    }
}

/// A wECX deposit found by `SolanaWallet::find_reference_deposit` - see that method's doc comment.
pub struct ReferenceDeposit {
    pub signature: Signature,
    pub amount_base_units: u64,
    pub depositor_owner: Option<Pubkey>,
    pub finalized: bool,
    /// Every account the transaction touched, static and address-table loaded. The treasury-wide
    /// scan checks these against known order reference keys to tell an order's deposit from one
    /// that carries no reference.
    pub account_keys: Vec<String>,
}

/// One entry of the treasury token account's history - see `SolanaWallet::list_treasury_signatures`.
pub struct TreasurySignature {
    pub signature: Signature,
    pub finalized: bool,
    pub failed: bool,
}

pub struct TreasurySignatures {
    /// Oldest first.
    pub entries: Vec<TreasurySignature>,
    /// `false` when paging stopped before reaching the caller's cursor.
    pub complete: bool,
}

/// A signed, not-yet-sent wECX transfer - see `SolanaWallet::prepare_wecx_transfer`.
pub struct PreparedPayout {
    tx: Transaction,
    pub signature: Signature,
    last_valid_block_height: u64,
}

/// Definitive result of `SolanaWallet::submit`.
pub enum PayoutOutcome {
    Confirmed,
    /// Landed on-chain but errored, so the transfer did not happen (a fee was still charged).
    Failed(String),
    /// Blockhash expired without the transaction ever landing - it never can now.
    Expired,
}

/// Result of looking a signature up after the fact.
pub enum SignatureOutcome {
    Confirmed,
    Failed(String),
    /// Seen by the cluster but not yet confirmed.
    Pending,
    NotFound,
}

/// Decodes `SOLANA_KEYPAIR` (base58, 64 bytes) into a keypair, refusing the public fixtures. Pure:
/// no network, so the treasury address can be derived on a machine that is not online yet.
pub fn keypair_from_env(keypair_base58: &str) -> Result<Keypair> {
    crate::secrets::reject_if_public_fixture("SOLANA_KEYPAIR", keypair_base58)?;
    let secret = bs58::decode(keypair_base58.trim())
        .into_vec()
        .context("SOLANA_KEYPAIR is not valid base58")?;
    Keypair::try_from(secret.as_slice())
        .map_err(|e| anyhow::anyhow!("SOLANA_KEYPAIR is not a valid ed25519 keypair: {e}"))
}

pub fn parse_pubkey(s: &str) -> Result<Pubkey> {
    Pubkey::from_str(s.trim()).map_err(|e| anyhow::anyhow!("invalid Solana address: {e}"))
}

/// Parses a decimal SOL amount (e.g. `0.003`) into lamports, using the Solana convention of
/// 9 decimal places: `1 SOL = 1_000_000_000 lamports`.
pub fn parse_lamports_from_sol(input: &str) -> Result<u64> {
    let trimmed = input.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed.starts_with('-') {
        bail!("invalid SOL amount: {input:?}");
    }

    let (whole, frac) = match trimmed.split_once('.') {
        Some((whole, frac)) => (whole, frac),
        None => (trimmed, ""),
    };

    if whole.is_empty() {
        bail!("invalid SOL amount: {input:?}");
    }

    let whole_lamports = whole
        .parse::<u64>()
        .map_err(|_| anyhow::anyhow!("invalid SOL amount: {input:?}"))?
        .checked_mul(1_000_000_000)
        .context("SOL amount is too large to fit in lamports")?;

    if frac.is_empty() {
        return Ok(whole_lamports);
    }
    if frac.len() > 9 || !frac.chars().all(|c| c.is_ascii_digit()) {
        bail!("invalid SOL amount: {input:?}");
    }

    let frac_digits = frac.trim_end_matches('0');
    let frac_lamports = if frac_digits.is_empty() {
        0
    } else {
        let mut digits = frac_digits.to_string();
        while digits.len() < 9 {
            digits.push('0');
        }
        digits
            .parse::<u64>()
            .context("SOL amount is too large to fit in lamports")?
    };

    whole_lamports
        .checked_add(frac_lamports)
        .context("SOL amount is too large to fit in lamports")
}

/// Pure core of `SolanaWallet::validate_recipient`: why `recipient` can't be paid, if it can't.
/// `spl_token::state::Account` is 165 bytes. Rent is read from the chain for that size.
const TOKEN_ACCOUNT_LEN: usize = 165;

fn account_missing(err: &solana_client::client_error::ClientError) -> bool {
    let text = err.to_string().to_lowercase();
    text.contains("could not find account")
        || text.contains("accountnotfound")
        || text.contains("account not found")
}

fn recipient_problem(recipient: &Pubkey, reserved: &[Pubkey], allow_off_curve: bool) -> Option<&'static str> {
    if reserved.contains(recipient) {
        return Some("reserved");
    }
    if !recipient.is_on_curve() && !allow_off_curve {
        return Some(
            "that address is not a wallet address (it is off the ed25519 curve, e.g. a program-derived \
             address) - check it was copied correctly",
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_transaction_status_client_types::option_serializer::OptionSerializer;
    use solana_transaction_status_client_types::{
        EncodedTransactionWithStatusMeta, UiRawMessage, UiTransaction,
        UiTransactionStatusMeta, UiTransactionTokenBalance,
    };

    fn token_balance(account_index: u8, mint: &Pubkey, owner: Option<&Pubkey>, amount: u64) -> UiTransactionTokenBalance {
        let owner_line = match owner {
            Some(pk) => format!("owner = \"{pk}\"\n"),
            None => String::new(),
        };
        let doc = format!(
            "accountIndex = {account_index}\n\
             mint = \"{mint}\"\n\
             {owner_line}\
             programId = \"{}\"\n\
             [uiTokenAmount]\n\
             amount = \"{amount}\"\n\
             decimals = 8\n\
             uiAmountString = \"{amount}\"\n",
            spl_token::id()
        );
        toml::from_str(&doc).unwrap()
    }

    fn build_encoded_tx(
        static_keys: Vec<String>,
        loaded: Option<UiLoadedAddresses>,
        pre_balances: Vec<UiTransactionTokenBalance>,
        post_balances: Vec<UiTransactionTokenBalance>,
        failed: bool,
    ) -> EncodedConfirmedTransactionWithStatusMeta {
        let mut meta = UiTransactionStatusMeta {
            err: if failed {
                Some(solana_sdk::transaction::TransactionError::AccountNotFound)
            } else {
                None
            },
            status: if failed {
                Err(solana_sdk::transaction::TransactionError::AccountNotFound)
            } else {
                Ok(())
            },
            fee: 5_000,
            pre_balances: vec![],
            post_balances: vec![],
            inner_instructions: OptionSerializer::Skip,
            log_messages: OptionSerializer::Skip,
            pre_token_balances: OptionSerializer::Some(pre_balances),
            post_token_balances: OptionSerializer::Some(post_balances),
            rewards: OptionSerializer::Skip,
            loaded_addresses: match loaded {
                Some(l) => OptionSerializer::Some(l),
                None => OptionSerializer::Skip,
            },
            return_data: OptionSerializer::Skip,
            compute_units_consumed: OptionSerializer::Skip,
            cost_units: OptionSerializer::Skip,
        };
        if failed {
            meta.err = Some(solana_sdk::transaction::TransactionError::AccountNotFound);
        }
        EncodedConfirmedTransactionWithStatusMeta {
            slot: 1,
            block_time: Some(1_700_000_000),
            transaction: EncodedTransactionWithStatusMeta {
                transaction: EncodedTransaction::Json(UiTransaction {
                    signatures: vec![Signature::default().to_string()],
                    message: UiMessage::Raw(UiRawMessage {
                        header: solana_sdk::message::MessageHeader::default(),
                        account_keys: static_keys,
                        recent_blockhash: String::new(),
                        instructions: vec![],
                        address_table_lookups: None,
                    }),
                }),
                meta: Some(meta),
                version: None,
            },
        }
    }

    #[test]
    fn ordinary_wallet_address_is_accepted() {
        let wallet = Keypair::new().pubkey();
        assert!(recipient_problem(&wallet, &[], false).is_none());
    }

    #[test]
    fn sol_amount_to_lamports_round_trips_and_rejects_malformed_inputs() {
        assert_eq!(parse_lamports_from_sol("0").unwrap(), 0);
        assert_eq!(parse_lamports_from_sol("0.003").unwrap(), 3_000_000);
        assert_eq!(parse_lamports_from_sol("  1.000000000 ").unwrap(), 1_000_000_000);
        assert_eq!(parse_lamports_from_sol("0.000000001").unwrap(), 1);
        for invalid in ["", ".", ".5", "-0.1", "0.0000000001", "1.2.3", "abc", "18446744073709551615"] {
            assert!(parse_lamports_from_sol(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn reserved_addresses_are_rejected() {
        let mint = Keypair::new().pubkey();
        let payer = Keypair::new();
        let treasury = payer.pubkey();
        let wallet = SolanaWallet::for_test(payer, mint, 8);
        for forbidden in [
            solana_sdk::system_program::id(),
            spl_token::id(),
            spl_associated_token_account::id(),
            mint,
            treasury,
        ] {
            assert!(wallet.validate_recipient(&forbidden).is_err(), "accepted {forbidden}");
        }
        let user = Keypair::new().pubkey();
        assert!(wallet.validate_recipient(&user).is_ok());
    }

    #[test]
    fn off_curve_addresses_are_rejected_unless_allowed() {
        let (pda, _) = Pubkey::find_program_address(&[b"vault"], &spl_token::id());
        assert!(!pda.is_on_curve());
        assert!(recipient_problem(&pda, &[], false).is_some());
        assert!(recipient_problem(&pda, &[], true).is_none());
    }

    #[test]
    fn keypair_from_env_decodes_valid_secret_and_rejects_bad_inputs() {
        let generated = Keypair::new();
        let base58 = generated.to_base58_string();
        let decoded = keypair_from_env(&format!("  {base58}\n")).unwrap();
        assert_eq!(decoded.pubkey(), generated.pubkey());

        assert!(keypair_from_env("not-valid-base58-!!!").is_err());
        assert!(keypair_from_env(&bs58::encode([1u8; 32]).into_string()).is_err());
    }

    #[test]
    fn parse_reference_deposit_extracts_delta_owner_and_loaded_addresses() {
        let payer = Keypair::new();
        let mint = Keypair::new().pubkey();
        let wallet = SolanaWallet::for_test(payer, mint, 8);
        let treasury_ata = wallet.treasury_ata();
        let customer = Keypair::new().pubkey();
        let customer_ata = get_associated_token_address(&customer, &mint);
        let reference = Keypair::new().pubkey();
        let sig = Signature::new_unique();

        // Standard deposit where treasury ATA already had 1_000 units and receives 25_000 more,
        // and the reference key arrives via a v0 address lookup table.
        let tx = build_encoded_tx(
            vec![customer.to_string(), customer_ata.to_string(), treasury_ata.to_string()],
            Some(UiLoadedAddresses {
                writable: vec![],
                readonly: vec![reference.to_string()],
            }),
            vec![
                token_balance(1, &mint, Some(&customer), 100_000),
                token_balance(2, &mint, Some(&wallet.pubkey()), 1_000),
            ],
            vec![
                token_balance(1, &mint, Some(&customer), 75_000),
                token_balance(2, &mint, Some(&wallet.pubkey()), 26_000),
            ],
            false,
        );
        let parsed = wallet.parse_reference_deposit(&tx, sig, true).unwrap().unwrap();
        assert_eq!(parsed.signature, sig);
        assert_eq!(parsed.amount_base_units, 25_000);
        assert_eq!(parsed.depositor_owner, Some(customer));
        assert!(parsed.finalized);
        assert!(parsed.account_keys.contains(&reference.to_string()));

        // First-ever deposit to treasury ATA (no pre_token_balances entry for treasury) and source
        // token account closed in the same transaction (absent from post_token_balances).
        let first_tx = build_encoded_tx(
            vec![customer_ata.to_string(), treasury_ata.to_string()],
            None,
            vec![token_balance(0, &mint, Some(&customer), 50_000)],
            vec![token_balance(1, &mint, Some(&wallet.pubkey()), 50_000)],
            false,
        );
        let first = wallet.parse_reference_deposit(&first_tx, sig, false).unwrap().unwrap();
        assert_eq!(first.amount_base_units, 50_000);
        assert_eq!(first.depositor_owner, Some(customer));
        assert!(!first.finalized);

        // Failed transaction on-chain must never be treated as a deposit.
        let failed_tx = build_encoded_tx(
            vec![customer_ata.to_string(), treasury_ata.to_string()],
            None,
            vec![token_balance(0, &mint, Some(&customer), 50_000)],
            vec![token_balance(1, &mint, Some(&wallet.pubkey()), 50_000)],
            true,
        );
        assert!(wallet.parse_reference_deposit(&failed_tx, sig, true).unwrap().is_none());

        // Outbound treasury transfer (balance decreased, did not increase) returns None.
        let outbound_tx = build_encoded_tx(
            vec![treasury_ata.to_string()],
            None,
            vec![token_balance(0, &mint, Some(&wallet.pubkey()), 50_000)],
            vec![token_balance(0, &mint, Some(&wallet.pubkey()), 40_000)],
            false,
        );
        assert!(wallet.parse_reference_deposit(&outbound_tx, sig, true).unwrap().is_none());
    }
}
