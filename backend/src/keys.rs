use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use bdk_wallet::bitcoin::bip32::DerivationPath;
use bdk_wallet::bitcoin::secp256k1::Secp256k1;
use bdk_wallet::bitcoin::NetworkKind;
use bdk_wallet::descriptor;
use bdk_wallet::descriptor::IntoWalletDescriptor;
use bdk_wallet::keys::bip39::Mnemonic;
use bdk_wallet::miniscript;

pub struct WalletDescriptors {
    pub external: String,
    pub internal: String,
}

/// Derives one shared descriptor pair from a seed in `WALLET_MNEMONIC` (+ optional
/// `WALLET_PASSPHRASE`). The same descriptors are used for both the BTC and ECX wallets: since
/// eCash duplicates Bitcoin's ledger at the fork block, a pre-fork BTC address and its ECX
/// counterpart are the exact same address, so both chains must derive from identical descriptors
/// for shared UTXOs to be visible on both sides.
pub fn load_descriptors() -> Result<WalletDescriptors> {
    let phrase = std::env::var("WALLET_MNEMONIC")
        .context("WALLET_MNEMONIC env var not set (space-separated BIP39 mnemonic)")?;
    crate::secrets::reject_if_public_fixture("WALLET_MNEMONIC", phrase.trim())?;
    let passphrase = std::env::var("WALLET_PASSPHRASE").ok();

    let mnemonic = Mnemonic::from_str(phrase.trim()).map_err(|e| anyhow!("invalid mnemonic: {e}"))?;
    let mnemonic_with_passphrase = (mnemonic, passphrase);

    let secp = Secp256k1::new();
    // BIP84 native segwit, mainnet coin type (0h) - both chains use mainnet-style encoding.
    let external_path = DerivationPath::from_str("m/84h/0h/0h/0")?;
    let internal_path = DerivationPath::from_str("m/84h/0h/0h/1")?;

    let (external_descriptor, ext_keymap) =
        descriptor!(wpkh((mnemonic_with_passphrase.clone(), external_path)))?
            .into_wallet_descriptor(&secp, NetworkKind::Main)?;
    let (internal_descriptor, int_keymap) =
        descriptor!(wpkh((mnemonic_with_passphrase, internal_path)))?
            .into_wallet_descriptor(&secp, NetworkKind::Main)?;

    Ok(WalletDescriptors {
        external: external_descriptor.to_string_with_secret(&ext_keymap),
        internal: internal_descriptor.to_string_with_secret(&int_keymap),
    })
}
