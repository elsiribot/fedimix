//! A software signer for tests. It exists only under the `testing` feature, so
//! no release binary has it: `scripts/check-no-soft-signer.sh` proves that.

use async_trait::async_trait;
use bitcoin::bip32::{DerivationPath, Xpriv, Xpub};
use bitcoin::psbt::Psbt;
use bitcoin::secp256k1::Secp256k1;
use std::str::FromStr;

use super::signer::Signer;
use crate::api::DeviceKind;
use crate::error::{FedimixError, Result};

/// Holds an extended private key. Lives apart from the watch-only wallet.
pub struct SoftSigner {
    key: Xpriv,
    rejects: bool,
}

impl SoftSigner {
    pub fn from_seed(seed: &[u8], network: bitcoin::Network) -> Self {
        SoftSigner {
            key: Xpriv::new_master(network, seed).expect("a seed of 16 to 64 bytes"),
            rejects: false,
        }
    }

    /// From an extended private key as text, such as `tprv…`.
    pub fn from_xprv(text: &str) -> Result<Self> {
        let key = Xpriv::from_str(text.trim()).map_err(|_| {
            FedimixError::Internal("The test signer's key is not an extended private key.".into())
        })?;
        Ok(SoftSigner {
            key,
            rejects: false,
        })
    }

    /// The same key, but `sign` answers as if the user declined on the device.
    pub fn rejecting(mut self) -> Self {
        self.rejects = true;
        self
    }
}

#[async_trait]
impl Signer for SoftSigner {
    fn kind(&self) -> DeviceKind {
        DeviceKind::Software
    }

    async fn fingerprint(&self) -> Result<String> {
        Ok(self.key.fingerprint(&Secp256k1::new()).to_string())
    }

    async fn xpub(&self, path: &str) -> Result<String> {
        let secp = Secp256k1::new();
        let path = DerivationPath::from_str(path)
            .map_err(|_| FedimixError::Internal(format!("{path} is not a derivation path.")))?;
        let child = self.key.derive_priv(&secp, &path).map_err(|_| {
            FedimixError::Internal("The software signer could not derive that key.".into())
        })?;
        Ok(Xpub::from_priv(&secp, &child).to_string())
    }

    async fn sign(&self, psbt: &mut Psbt) -> Result<()> {
        if self.rejects {
            return Err(FedimixError::DeviceRejected);
        }
        psbt.sign(&self.key, &Secp256k1::new()).map_err(|_| {
            FedimixError::Internal("The software signer could not sign this transaction.".into())
        })?;
        Ok(())
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use bitcoin::absolute::LockTime;
    use bitcoin::bip32::Fingerprint;
    use bitcoin::transaction::Version;
    use bitcoin::{
        Amount, CompressedPublicKey, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut,
        Witness,
    };

    pub(in crate::saving) const SEED: [u8; 32] = [7; 32];

    /// A one-input PSBT spending a P2WPKH output of `key` at m/84'/1'/0'/0/0,
    /// with the key origin filled in as BDK does.
    pub(in crate::saving) fn built_psbt(key: &Xpriv) -> Psbt {
        let secp = Secp256k1::new();
        let path = DerivationPath::from_str("m/84'/1'/0'/0/0").unwrap();
        let child = key.derive_priv(&secp, &path).unwrap();
        let public = CompressedPublicKey::from_private_key(&secp, &child.to_priv()).unwrap();
        let spk = ScriptBuf::new_p2wpkh(&public.wpubkey_hash());
        let tx = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(40_000),
                script_pubkey: spk.clone(),
            }],
        };
        let mut psbt = Psbt::from_unsigned_tx(tx).unwrap();
        psbt.inputs[0].witness_utxo = Some(TxOut {
            value: Amount::from_sat(50_000),
            script_pubkey: spk,
        });
        let origin: (Fingerprint, DerivationPath) = (key.fingerprint(&secp), path);
        psbt.inputs[0].bip32_derivation.insert(public.0, origin);
        psbt
    }

    fn signer() -> SoftSigner {
        SoftSigner::from_seed(&SEED, bitcoin::Network::Regtest)
    }

    #[tokio::test]
    async fn soft_signer_signs_a_built_psbt() {
        let signer = signer();
        let mut psbt = built_psbt(&signer.key);
        assert!(psbt.inputs[0].partial_sigs.is_empty());
        signer.sign(&mut psbt).await.unwrap();
        assert_eq!(psbt.inputs[0].partial_sigs.len(), 1);
    }

    #[tokio::test]
    async fn a_rejecting_soft_signer_adds_nothing() {
        let signer = signer().rejecting();
        let mut psbt = built_psbt(&signer.key);
        assert_eq!(
            signer.sign(&mut psbt).await,
            Err(FedimixError::DeviceRejected)
        );
        assert!(psbt.inputs[0].partial_sigs.is_empty());
    }

    #[tokio::test]
    async fn a_key_read_as_text_is_the_same_key() {
        let key = signer().key.to_string();
        let read = SoftSigner::from_xprv(&format!("{key}\n")).unwrap();
        assert_eq!(
            read.fingerprint().await.unwrap(),
            signer().fingerprint().await.unwrap()
        );
        assert!(SoftSigner::from_xprv("not a key").is_err());
    }

    #[tokio::test]
    async fn fingerprint_and_xpub_agree_with_the_key() {
        let signer = signer();
        let fp = signer.fingerprint().await.unwrap();
        assert_eq!(fp.len(), 8);
        let account = signer.xpub("m/84'/1'/0'").await.unwrap();
        assert!(account.starts_with("tpub"));
        let root = signer.xpub("m").await.unwrap();
        assert_eq!(
            Xpub::from_str(&root)
                .unwrap()
                .fingerprint()
                .to_string()
                .len(),
            8
        );
        assert!(signer.xpub("not a path").await.is_err());
    }
}
