use anyhow::{bail, Result};
use bdk_wallet::bitcoin::hashes::{sha256, Hash};

/// SHA-256 of secret strings that must never operate this bridge.
///
/// The first two are the hot-wallet secrets that were committed in `backend/.env.example` and
/// pushed to `origin/main`. The others are the public fixtures that file now contains. Matching is
/// by hash so this source file does not repeat the committed secrets.
const REJECTED_SHA256: &[&str] = &[
    "2bf92ebff61e4d459a1b6cc2c77de7c5f321bfb01d83215f198425cbd5567fb7",
    "556d56597da1132a1e562e981d9d77f9b49e2ff22d53daf9b20946ce1cfa7498",
    "c557eec878dfd852ba3f88087c4f350f09c55537ab5e549c3cd14320ec3cef38",
    "68c6ab3eba220da70c0fc012ba5f1b6107a8b5a24a4a7b7354984d6e7aa2137b",
    "d7e087650d5a7c895e4c57daab23770777a5f18ae013590a72a646ced11194dd",
];

pub fn reject_if_public_fixture(label: &str, value: &str) -> Result<()> {
    let digest = sha256::Hash::hash(value.trim().as_bytes()).to_string();
    if REJECTED_SHA256.contains(&digest.as_str()) {
        bail!(
            "{label} is a public example or a secret that was committed to git. Generate a new \
             value and keep it only in the untracked env file. Assume the old value is compromised."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wordlist_order_example_is_rejected() {
        let err = reject_if_public_fixture(
            "WALLET_MNEMONIC",
            "abandon ability able about above absent absorb abstract absurd abuse access accident",
        )
        .unwrap_err();
        assert!(err.to_string().contains("committed to git"));
    }

    #[test]
    fn public_abandon_mnemonic_is_rejected() {
        let err = reject_if_public_fixture(
            "WALLET_MNEMONIC",
            "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about",
        )
        .unwrap_err();
        assert!(err.to_string().contains("committed to git"));
    }

    #[test]
    fn an_unlisted_value_is_allowed() {
        reject_if_public_fixture("WALLET_MNEMONIC", "legal winner thank year wave sausage worth useful legal winner thank yellow").unwrap();
    }
}
