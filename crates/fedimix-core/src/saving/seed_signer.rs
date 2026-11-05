//! The software Saving wallet: a [`Signer`] keyed from the recovery words, for
//! people without a hardware wallet. Unlike the `testing` `SoftSigner` it is in
//! every build, and it has no hooks.
//!
//! It keeps the words (shared with Spending), never a private key: each call
//! makes the BIP32 master key from them and drops it at the end. Nothing here
//! logs, returns or puts in an error any secret. It talks to no network, so a
//! proxy changes nothing for it.

use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use bitcoin::bip32::{DerivationPath, Xpriv, Xpub};
use bitcoin::psbt::Psbt;
use bitcoin::secp256k1::Secp256k1;

use super::signer::Signer;
use crate::api::{DeviceKind, Network};
use crate::error::{FedimixError, Result};
use crate::seed::Seed;

/// Signs with the BIP84 account `m/84'/<coin>'/0'` of the recovery words.
pub struct SeedSigner {
    seed: Arc<Seed>,
    network: Network,
}

impl SeedSigner {
    pub(crate) fn new(seed: Arc<Seed>, network: Network) -> Self {
        SeedSigner { seed, network }
    }

    /// The master key, for the length of one call.
    fn master(&self) -> Result<Xpriv> {
        let network = match self.network {
            Network::Bitcoin => bitcoin::Network::Bitcoin,
            Network::Signet => bitcoin::Network::Signet,
            Network::Regtest => bitcoin::Network::Regtest,
        };
        self.seed.saving_master_key(network)
    }
}

#[async_trait]
impl Signer for SeedSigner {
    fn kind(&self) -> DeviceKind {
        DeviceKind::Software
    }

    async fn fingerprint(&self) -> Result<String> {
        Ok(self.master()?.fingerprint(&Secp256k1::new()).to_string())
    }

    async fn xpub(&self, path: &str) -> Result<String> {
        let path = DerivationPath::from_str(path)
            .map_err(|_| FedimixError::Internal(format!("{path} is not a derivation path.")))?;
        let secp = Secp256k1::new();
        let child = self.master()?.derive_priv(&secp, &path).map_err(|_| {
            FedimixError::Internal("The software wallet could not derive that key.".into())
        })?;
        Ok(Xpub::from_priv(&secp, &child).to_string())
    }

    /// Signs every input whose key origin is this wallet's, as a hardware
    /// wallet would; the caller has checked what the PSBT pays.
    async fn sign(&self, psbt: &mut Psbt) -> Result<()> {
        let master = self.master()?;
        psbt.sign(&master, &Secp256k1::new()).map_err(|_| {
            FedimixError::Internal("The software wallet could not sign this transaction.".into())
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{Address, CompressedPublicKey};

    const VECTOR: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    fn signer(network: Network) -> SeedSigner {
        SeedSigner::new(Arc::new(Seed::from_phrase(VECTOR)), network)
    }

    /// The first receive address, `<account>/0/0`, of an account xpub.
    fn first_receive(xpub: &str, network: bitcoin::Network) -> String {
        let secp = Secp256k1::new();
        let xpub = Xpub::from_str(xpub).unwrap();
        let child = xpub
            .derive_pub(&secp, &DerivationPath::from_str("m/0/0").unwrap())
            .unwrap();
        let key = CompressedPublicKey(child.public_key);
        Address::p2wpkh(&key, network).to_string()
    }

    /// BIP84's own test vector.
    #[tokio::test]
    async fn seed_signer_matches_bip84_vector() {
        let signer = signer(Network::Bitcoin);
        assert_eq!(signer.kind(), DeviceKind::Software);
        assert_eq!(signer.fingerprint().await.unwrap(), "73c5da0a");
        let account = signer.xpub("m/84'/0'/0'").await.unwrap();
        assert_eq!(
            first_receive(&account, bitcoin::Network::Bitcoin),
            "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu"
        );
    }

    #[tokio::test]
    async fn the_test_networks_get_test_keys() {
        let signer = signer(Network::Regtest);
        assert_eq!(signer.fingerprint().await.unwrap(), "73c5da0a");
        assert!(
            signer
                .xpub("m/84'/1'/0'")
                .await
                .unwrap()
                .starts_with("tpub")
        );
        assert!(signer.xpub("not a path").await.is_err());
    }

    #[tokio::test]
    async fn seed_signer_signs_a_built_psbt() {
        let signer = signer(Network::Regtest);
        let key = signer
            .seed
            .saving_master_key(bitcoin::Network::Regtest)
            .unwrap();
        let mut psbt = crate::saving::soft_signer::tests::built_psbt(&key);
        signer.sign(&mut psbt).await.unwrap();
        assert_eq!(psbt.inputs[0].partial_sigs.len(), 1);
    }

    #[tokio::test]
    async fn no_error_holds_a_secret() {
        let signer = signer(Network::Regtest);
        let Err(FedimixError::Internal(sentence)) = signer.xpub("m/x").await else {
            panic!("a bad path is refused");
        };
        assert!(!sentence.contains("prv"), "{sentence}");
        assert!(!sentence.contains("abandon"), "{sentence}");
    }
}
