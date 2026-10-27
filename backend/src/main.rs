mod config;
mod convert;
mod db;
mod keys;
mod secrets;
mod server;
mod solana;
mod wallet;

use anyhow::{bail, Result};
use bdk_wallet::bitcoin::{Address, Amount, FeeRate};
use bdk_wallet::keys::bip39::Mnemonic;
use clap::{Parser, Subcommand};
use solana_sdk::signature::{Keypair, Signer};

use config::ChainConfig;
use solana::SolanaWallet;
use wallet::ChainWallet;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the next ECX receive address.
    Address {
        /// Always reveal a brand-new address, even if the current one is unused.
        #[arg(long)]
        fresh: bool,
    },
    /// Report whether an ECX address belongs to this hot wallet (any revealed address, or one of
    /// the first thousand of either keychain). The installer uses it to refuse an owner address
    /// that would sweep fees back onto the server. Prints `true` or `false`; no network access.
    OwnsAddress {
        #[arg(long)]
        address: String,
    },
    /// Print the ECX wallet balance, refreshing confirmation status for UTXOs already known (not
    /// a full address-history resync - see `ChainWallet::verify_unspent`). Run `sync` or
    /// `scan-address` first if a deposit hasn't been discovered at all yet.
    Balance,
    /// Sync the ECX wallet state against its Esplora/Electrum backend.
    Sync {
        /// Do a full scan instead of an incremental sync (needed the first time).
        #[arg(long)]
        full: bool,
    },
    /// List the UTXOs the ECX wallet currently thinks it holds (from the local store - run `sync`
    /// first to refresh against the chain backend).
    Utxos,
    /// Check a single ECX address for new transactions, without resyncing the wallet's whole
    /// address history - for picking up a manual top-up (e.g. after `address`/`address --fresh`)
    /// fast. Only useful for an address this wallet has already revealed (via `address`); an
    /// address it's never seen won't be attributed to its balance even if funds are found there.
    ScanAddress {
        #[arg(long)]
        address: String,
    },
    /// Send an ECX amount to an address.
    Send {
        #[arg(long)]
        to: String,
        /// Exclusive with --all.
        #[arg(long)]
        amount_sat: Option<u64>,
        /// Send the entire spendable balance instead of a fixed amount. Exclusive with --amount-sat.
        #[arg(long)]
        all: bool,
        #[arg(long, default_value_t = 2)]
        fee_rate: u64,
        /// Required to actually broadcast: betanet ECX is real money (see CLAUDE.md), not a test
        /// network.
        #[arg(long)]
        confirm_live: bool,
    },
    /// Salvage the ECX wallet store if its tail got corrupted (e.g. by two processes writing to
    /// it at once - see `ChainWallet::recover`). Never modifies the live file: writes a
    /// `.corrupt-backup-<ts>` copy and a `.recovered` file alongside it and leaves swapping the
    /// recovered file in for you to do manually once you've checked the reported balance.
    RecoverStore,
    /// Generate a new 12-word BIP39 mnemonic for `WALLET_MNEMONIC`. Prints it once and stores
    /// nothing. This is the ECX hot wallet, BIP84 native segwit, and it must not be a wallet that
    /// already holds other funds.
    EcxKeygen,
    /// Generate a brand-new Solana keypair and print it (base58 64-byte secret key, in the format
    /// `SOLANA_KEYPAIR` expects, plus its public key). Touches no config or files and needs no
    /// env vars - copy the secret somewhere safe yourself; it is not stored anywhere.
    SolanaKeygen,
    /// Print the Solana treasury's public key, after connecting to the RPC and checking the mint.
    SolanaAddress,
    /// Print the Solana treasury's public key from SOLANA_KEYPAIR alone: no network, no mint
    /// lookup. The installer uses this so the deposit addresses exist before the machine is online.
    SolanaPubkey,
    /// Print the Solana treasury's SOL balance (for tx fees/rent) and wECX balance.
    SolanaBalance,
    /// List wECX deposits that reached the treasury without any order's reference key: the ones
    /// held for a person, owed back, and (with --all) already refunded or kept. A held deposit is
    /// refunded by hand with `solana-send`; the fee, if any, is yours to decide.
    UnmatchedDeposits {
        #[arg(long)]
        all: bool,
    },
    /// Manually send wECX from the treasury to a Solana address.
    SolanaSend {
        #[arg(long)]
        to: String,
        /// In wECX base units (already scaled by the mint's decimals) - not whole wECX. Exclusive
        /// with --all.
        #[arg(long)]
        amount_base_units: Option<u64>,
        /// Send the entire treasury wECX balance instead of a fixed amount. Exclusive with
        /// --amount-base-units.
        #[arg(long)]
        all: bool,
        /// Required to actually broadcast: Solana mainnet-beta is real money (see CLAUDE.md).
        #[arg(long)]
        confirm_live: bool,
    },
    /// Manually send SOL (native lamports) from the treasury to a Solana address.
    SolanaSendSol {
        #[arg(long)]
        to: String,
        /// Amount in SOL (e.g. `0.003`) as a decimal string. Exclusive with --amount-lamports.
        #[arg(long, conflicts_with = "amount_lamports")]
        amount_sol: Option<String>,
        /// Amount in lamports. Exclusive with --amount-sol.
        #[arg(long, conflicts_with = "amount_sol")]
        amount_lamports: Option<u64>,
        /// Required to actually broadcast: Solana mainnet-beta is real money (see CLAUDE.md).
        #[arg(long)]
        confirm_live: bool,
    },
    /// Print the current bridge conversion for a given ECX amount (fetches the wECX mint's
    /// decimals over RPC; no ECX wallet sync involved).
    Quote {
        #[arg(long)]
        amount_sat: u64,
    },
    /// Print the current bridge conversion for a given wECX amount (the peg-out direction's
    /// mirror of `Quote`).
    PegoutQuote {
        #[arg(long)]
        amount_base_units: u64,
    },
    /// Serve the bridge's quote/order API over HTTP.
    Serve {
        #[arg(long, default_value = "127.0.0.1:3000")]
        addr: String,
    },
}

fn open_ecx_wallet() -> Result<ChainWallet> {
    let descriptors = keys::load_descriptors()?;
    let config = ChainConfig::ecx();
    ChainWallet::open(&config, &descriptors.external, &descriptors.internal)
}

async fn open_solana_wallet() -> Result<SolanaWallet> {
    let rpc_url = config::solana_rpc_url();
    let keypair = config::solana_keypair()?;
    let mint = config::wecx_mint()?;
    SolanaWallet::open(&rpc_url, &keypair, mint).await
}

/// Both betanet ECX and Solana mainnet-beta are real money (see CLAUDE.md) - there's no test
/// network for either side of this bridge, so every manual CLI send needs an explicit opt-in
/// rather than defaulting to "just broadcast it".
fn require_live_confirmation(confirmed: bool, description: &str) -> Result<()> {
    if !confirmed {
        bail!(
            "refusing to broadcast {description} without --confirm-live: both betanet ECX and \
             Solana mainnet-beta are real money (see CLAUDE.md). Re-run with --confirm-live once \
             you're sure."
        );
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let cli = Cli::parse();

    match cli.command {
        Command::Address { fresh } => {
            let mut wallet = open_ecx_wallet()?;
            let address = if fresh { wallet.fresh_address()? } else { wallet.next_address()? };
            println!("[ecx] {address}");
        }
        Command::OwnsAddress { address } => {
            let config = ChainConfig::ecx();
            let address: Address = address.parse::<Address<_>>()?.require_network(config.network)?;
            let wallet = open_ecx_wallet()?;
            println!("[ecx] owns {address}: {}", wallet.owns_address(&address));
        }
        Command::Balance => {
            let mut wallet = open_ecx_wallet()?;
            wallet.verify_unspent().await?;
            let balance = wallet.balance();
            let unconfirmed = balance.trusted_pending + balance.untrusted_pending;
            println!(
                "[ecx] confirmed: {}, unconfirmed: {}, total: {}",
                balance.confirmed,
                unconfirmed,
                balance.total()
            );
        }
        Command::Utxos => {
            let config = ChainConfig::ecx();
            let wallet = open_ecx_wallet()?;
            let mut utxos = wallet.utxos();
            utxos.sort_by_key(|u| std::cmp::Reverse(u.txout.value));

            println!("[ecx] {} UTXO(s):", utxos.len());
            for utxo in &utxos {
                let confs = wallet.confirmations(utxo.outpoint.txid);
                let address = Address::from_script(&utxo.txout.script_pubkey, config.network)
                    .map(|a| a.to_string())
                    .unwrap_or_else(|_| "<unknown script>".to_string());
                println!(
                    "  {} vout={} {} keychain={:?} confs={} address={}",
                    utxo.outpoint.txid, utxo.outpoint.vout, utxo.txout.value, utxo.keychain, confs, address,
                );
            }

            let balance = wallet.balance();
            let unconfirmed = balance.trusted_pending + balance.untrusted_pending;
            println!(
                "[ecx] confirmed: {}, unconfirmed: {}, total: {}",
                balance.confirmed,
                unconfirmed,
                balance.total()
            );
        }
        Command::RecoverStore => {
            let descriptors = keys::load_descriptors()?;
            let config = ChainConfig::ecx();
            let summary = wallet::ChainWallet::recover(&config, &descriptors.external, &descriptors.internal)?;
            let unconfirmed = summary.balance.trusted_pending + summary.balance.untrusted_pending;
            println!(
                "[ecx] original error: {}\nbackup of corrupted file: {}\nrecovered store: {}\n\
                 recovered balance - confirmed: {}, unconfirmed: {}, total: {}\n\nThe live store \
                 at data/ecx.db was NOT touched. Review the numbers above, stop anything with the \
                 file open, then swap the recovered file in yourself once you're satisfied.",
                summary.original_error,
                summary.backup_path.display(),
                summary.recovered_path.display(),
                summary.balance.confirmed,
                unconfirmed,
                summary.balance.total(),
            );
        }
        Command::Sync { full } => {
            let mut wallet = open_ecx_wallet()?;
            if full {
                wallet.full_scan().await?;
            } else {
                wallet.sync().await?;
            }
            let balance = wallet.balance();
            let unconfirmed = balance.trusted_pending + balance.untrusted_pending;
            println!(
                "[ecx] synced. confirmed: {}, unconfirmed: {}, total: {}",
                balance.confirmed,
                unconfirmed,
                balance.total()
            );
        }
        Command::ScanAddress { address } => {
            let config = ChainConfig::ecx();
            let address: Address = address.parse::<Address<_>>()?.require_network(config.network)?;

            let mut wallet = open_ecx_wallet()?;
            wallet.sync_addresses([address.script_pubkey()]).await?;

            let balance = wallet.balance();
            let unconfirmed = balance.trusted_pending + balance.untrusted_pending;
            println!(
                "[ecx] scanned {address}. wallet balance - confirmed: {}, unconfirmed: {}, total: {}",
                balance.confirmed,
                unconfirmed,
                balance.total()
            );
        }
        Command::Send { to, amount_sat, all, fee_rate, confirm_live } => {
            let amount = match (amount_sat, all) {
                (Some(_), true) => bail!("--amount-sat and --all are mutually exclusive"),
                (None, false) => bail!("one of --amount-sat or --all is required"),
                (Some(sat), false) => Some(Amount::from_sat(sat)),
                (None, true) => None,
            };
            require_live_confirmation(confirm_live, "on ECX")?;

            let config = ChainConfig::ecx();
            let mut wallet = open_ecx_wallet()?;
            wallet.sync().await?;

            let address: Address = to.parse::<Address<_>>()?.require_network(config.network)?;
            let fee_rate = FeeRate::from_sat_per_vb(fee_rate).ok_or_else(|| anyhow::anyhow!("invalid fee rate"))?;

            // A manual send must not spend a customer's still-unsettled deposit. The server poller
            // already excludes these; this CLI used to pass an empty list and could sweep them.
            let db_conn = db::open().await?;
            let protected = server::protected_deposit_outpoints(&db_conn, &wallet, None).await?;
            if !protected.is_empty() {
                println!(
                    "[ecx] excluding {} unsettled deposit outpoint(s) from coin selection",
                    protected.len()
                );
            }

            let txid = wallet.send(&address, amount, fee_rate, &protected).await?;
            println!("[ecx] sent. txid: {txid}");
        }
        Command::EcxKeygen => {
            // bip39's generate() needs the rand feature, which this crate does not enable.
            let mut entropy = [0u8; 16];
            std::fs::File::open("/dev/urandom")
                .and_then(|mut file| std::io::Read::read_exact(&mut file, &mut entropy))
                .map_err(|e| anyhow::anyhow!("urandom: {e}"))?;
            let mnemonic =
                Mnemonic::from_entropy(&entropy).map_err(|e| anyhow::anyhow!("mnemonic: {e}"))?;
            println!("{mnemonic}");
        }
        Command::SolanaKeygen => {
            let keypair = Keypair::new();
            println!("[solana] public key: {}", keypair.pubkey());
            println!("[solana] secret key (SOLANA_KEYPAIR): {}", keypair.to_base58_string());
        }
        Command::SolanaAddress => {
            let wallet = open_solana_wallet().await?;
            println!("[solana] {}", wallet.pubkey());
        }
        Command::SolanaPubkey => {
            let keypair = solana::keypair_from_env(&config::solana_keypair()?)?;
            println!("[solana] {}", keypair.pubkey());
        }
        Command::UnmatchedDeposits { all } => {
            let conn = db::open().await?;
            let statuses: &[db::UnmatchedStatus] = if all {
                &[
                    db::UnmatchedStatus::Held,
                    db::UnmatchedStatus::Pending,
                    db::UnmatchedStatus::Refunding,
                    db::UnmatchedStatus::Refunded,
                    db::UnmatchedStatus::Kept,
                ]
            } else {
                &[db::UnmatchedStatus::Held, db::UnmatchedStatus::Pending, db::UnmatchedStatus::Refunding]
            };
            let deposits = db::list_unmatched_deposits(&conn, statuses, 1_000).await?;
            if deposits.is_empty() {
                println!("[unmatched] no deposits{}", if all { "" } else { " waiting; use --all for settled ones" });
            }
            for d in deposits {
                println!(
                    "{}  {:<9}  {}  amount={} fee={} refund={}  from={}  {}{}",
                    d.signature,
                    d.status,
                    d.created_at,
                    d.amount_base_units,
                    d.fee_base_units,
                    d.refund_base_units,
                    d.depositor_owner.as_deref().unwrap_or("unknown"),
                    d.note,
                    d.refund_signature.as_deref().map(|s| format!("  refund_tx={s}")).unwrap_or_default(),
                );
            }
        }
        Command::SolanaBalance => {
            let wallet = open_solana_wallet().await?;
            let lamports = wallet.sol_balance_lamports().await?;
            let wecx = wallet.treasury_token_balance().await?;
            println!(
                "[solana] address: {}\nwECX mint: {}\nSOL balance: {} lamports ({:.9} SOL)\n\
                 wECX balance: {} base units ({} decimals)",
                wallet.pubkey(),
                wallet.mint(),
                lamports,
                lamports as f64 / 1_000_000_000.0,
                wecx,
                wallet.decimals(),
            );
        }
        Command::SolanaSend { to, amount_base_units, all, confirm_live } => {
            let wallet = open_solana_wallet().await?;
            let amount = match (amount_base_units, all) {
                (Some(_), true) => bail!("--amount-base-units and --all are mutually exclusive"),
                (None, false) => bail!("one of --amount-base-units or --all is required"),
                (Some(amount), false) => amount,
                (None, true) => wallet.treasury_token_balance().await?,
            };
            require_live_confirmation(confirm_live, "wECX on Solana")?;

            let recipient = solana::parse_pubkey(&to)?;
            wallet.validate_recipient(&recipient)?;
            let signature = wallet.send_wecx(recipient, amount).await?;
            println!("[solana] sent {amount} base units of wECX. signature: {signature}");
        }
        Command::SolanaSendSol { to, amount_sol, amount_lamports, confirm_live } => {
            let wallet = open_solana_wallet().await?;
            let lamports = match (amount_sol.as_deref(), amount_lamports) {
                (Some(_), Some(_)) => bail!("--amount-sol and --amount-lamports are mutually exclusive"),
                (None, None) => bail!("one of --amount-sol or --amount-lamports is required"),
                (Some(sol), None) => solana::parse_lamports_from_sol(sol)?,
                (None, Some(lamports)) => lamports,
            };
            require_live_confirmation(confirm_live, "SOL on Solana")?;

            let recipient = solana::parse_pubkey(&to)?;
            wallet.validate_recipient(&recipient)?;
            let signature = wallet.send_sol(recipient, lamports).await?;
            println!("[solana] sent {lamports} lamports of SOL. signature: {signature}");
        }
        Command::Quote { amount_sat } => {
            let wallet = open_solana_wallet().await?;
            let pegin_fee_bps = config::pegin_fee_bps();
            let flat_fee_sat = config::pegin_flat_fee_sat();
            let amount_out = convert::pegin_payout_base_units(
                amount_sat,
                wallet.decimals(),
                pegin_fee_bps,
                flat_fee_sat,
            );
            println!(
                "{amount_sat} ECX sat -> {amount_out} wECX base units ({} decimals, {pegin_fee_bps} bps plus {flat_fee_sat} sat peg-in fee)",
                wallet.decimals()
            );
        }
        Command::PegoutQuote { amount_base_units } => {
            let wallet = open_solana_wallet().await?;
            let pegout_fee_bps = config::pegout_fee_bps();
            let amount_out = convert::wecx_base_units_to_ecx_sat(amount_base_units, wallet.decimals(), pegout_fee_bps);
            println!(
                "{amount_base_units} wECX base units ({} decimals) -> {amount_out} ECX sat ({pegout_fee_bps} bps peg-out fee)",
                wallet.decimals()
            );
        }
        Command::Serve { addr } => {
            let ecx_wallet = open_ecx_wallet()?;
            let solana_wallet = open_solana_wallet().await?;
            let db_conn = db::open().await?;
            server::serve(ecx_wallet, solana_wallet, db_conn, &addr).await?;
        }
    }

    Ok(())
}
