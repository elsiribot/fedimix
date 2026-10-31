//! What signs for Saving: the [`Signer`] and [`DevicePort`] seams, and the
//! adapter that puts an async-hwi device behind [`Signer`].
//!
//! A signer verifies nothing. It adds signatures to the PSBT it is handed; the
//! caller (Task 6) checks that the PSBT pays what the preview showed.
//!
//! async-hwi talks to its devices with blocking calls inside `async fn`s (HID
//! reads, a busy loop on the Coldcard). Run on the app's runtime they would
//! stall a worker for as long as the user takes on the device, and could not be
//! cancelled. So every device call runs on its own thread, driven by
//! `Handle::block_on`, and the caller awaits a channel. Dropping the caller's
//! future cancels the wait at once; the call itself finishes on its own (see
//! [`HwiSigner`] for what that means).

// Used by the Saving operations from Task 5 on.
#![allow(dead_code)]

use std::str::FromStr;
use std::sync::Arc;

use async_hwi::utils::Bip32DerivationFilter;
use async_hwi::{Error as HwiError, HWI};
use async_trait::async_trait;
use bitcoin::bip32::DerivationPath;
use bitcoin::psbt::Psbt;
use tokio::runtime::Handle;

use crate::api::{DeviceInfo, DeviceKind};
use crate::error::{FedimixError, Result};

/// A key holder that can sign a PSBT: a hardware wallet, or under the `testing`
/// feature a software stand-in.
#[async_trait]
pub trait Signer: Send + Sync {
    fn kind(&self) -> DeviceKind;
    /// The master fingerprint, 8 lowercase hex digits.
    async fn fingerprint(&self) -> Result<String>;
    /// The extended public key at `path` (such as `m/84'/0'/0'`), as a string.
    async fn xpub(&self, path: &str) -> Result<String>;
    /// Adds the device's signatures to `psbt`. The user declining on the device
    /// is [`FedimixError::DeviceRejected`]. There is no timeout: the user may
    /// take minutes. Dropping the future stops the wait.
    async fn sign(&self, psbt: &mut Psbt) -> Result<()>;
}

/// Finds devices and opens them.
#[async_trait]
pub trait DevicePort: Send + Sync {
    /// The devices that can be reached now. A failure to look for one kind
    /// never hides the others.
    async fn list(&self) -> Result<Vec<DeviceInfo>>;
    /// Opens the device `id` from a recent `list`. May need a confirmation on
    /// the device (BitBox02 pairing, Jade PIN).
    async fn connect(&self, id: &str) -> Result<Arc<dyn Signer>>;
}

/// The name a person knows the device by, for sentences.
pub(super) fn kind_name(kind: DeviceKind) -> &'static str {
    match kind {
        DeviceKind::Ledger => "Ledger",
        DeviceKind::Trezor => "Trezor",
        DeviceKind::Coldcard => "Coldcard",
        DeviceKind::Jade => "Jade",
        DeviceKind::BitBox => "BitBox02",
        DeviceKind::Software => "software signer",
    }
}

fn internal(sentence: impl Into<String>) -> FedimixError {
    FedimixError::Internal(sentence.into())
}

/// One sentence per async-hwi error; never its Debug text. The text matching on
/// `Error::Device` is from reading the crates, not from a device: no hardware
/// was available.
pub(super) fn map_hwi_error(kind: DeviceKind, error: &HwiError) -> FedimixError {
    let name = kind_name(kind);
    match error {
        HwiError::UserRefused => FedimixError::DeviceRejected,
        HwiError::DeviceDisconnected | HwiError::DeviceNotFound => {
            FedimixError::DeviceNotConnected(name.to_string())
        }
        HwiError::Device(text) => map_device_text(kind, text),
        HwiError::UnimplementedMethod | HwiError::MissingPolicy => internal(format!(
            "Your {name} cannot do that with this version of Fedimix."
        )),
        HwiError::UnsupportedVersion => internal(format!(
            "The software on your {name} is too old or too new for Fedimix. Update it and try again."
        )),
        HwiError::NetworkMismatch => internal(format!(
            "Your {name} is set to a different Bitcoin network than Fedimix. Switch one of them and try again."
        )),
        HwiError::DeviceDidNotSign => internal(format!(
            "Your {name} did not sign. Check its screen and try again."
        )),
        HwiError::UnsupportedInput
        | HwiError::InvalidParameter(..)
        | HwiError::ParsingPolicy(_)
        | HwiError::Unexpected(_)
        | HwiError::Bip86ChangeIndex => internal(format!(
            "Your {name} could not use this request. Unplug it, plug it back in and try again."
        )),
    }
}

fn map_device_text(kind: DeviceKind, text: &str) -> FedimixError {
    let name = kind_name(kind);
    let not_connected = || FedimixError::DeviceNotConnected(name.to_string());
    match kind {
        // The Bitcoin client prints its status word. 0x6E00 and 0x6D00 are what
        // the dashboard or another app answers; an unknown status is what a
        // locked device answers (0x5515); a transport error is an unplugged one.
        DeviceKind::Ledger
            if text.contains("ClaNotSupported") || text.contains("InsNotSupported") =>
        {
            internal("Open the Bitcoin app on your Ledger, then try again.")
        }
        DeviceKind::Ledger if text.contains("Unknown") || text.contains("Transport") => {
            not_connected()
        }
        DeviceKind::Coldcard if text.contains("Hid(") => not_connected(),
        DeviceKind::Jade if text.contains("Transport") || text.contains("Io(") => not_connected(),
        DeviceKind::BitBox if text.contains("Hid") || text.contains("NotFound") => not_connected(),
        _ => internal(format!(
            "Your {name} reported a problem. Unplug it, plug it back in and try again."
        )),
    }
}

/// Runs `f` on a thread of its own and waits for it. If the caller's future is
/// dropped, the wait ends and the thread runs on to the end of `f`.
pub(super) async fn on_thread<T, F>(f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(Handle) -> T + Send + 'static,
{
    let handle = Handle::current();
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("fedimix-device".into())
        .spawn(move || {
            let _ = tx.send(f(handle));
        })
        .map_err(|_| internal("Fedimix could not start talking to the device. Try again."))?;
    rx.await
        .map_err(|_| internal("The device task stopped unexpectedly. Unplug the device, plug it back in and try again."))
}

/// An async-hwi device as a [`Signer`].
///
/// Dropping a call's future ends the wait, not the call: the device keeps
/// showing its prompt until the user answers or unplugs it, and the answer is
/// discarded. The next call on this signer waits for that one to end (two
/// commands must not interleave on one device), so after cancelling, drop the
/// signer and `connect` again.
pub(super) struct HwiSigner<T: HWI + Send + Sync + 'static> {
    hwi: Arc<T>,
    kind: DeviceKind,
    busy: Arc<tokio::sync::Mutex<()>>,
}

impl<T: HWI + Send + Sync + 'static> HwiSigner<T> {
    pub(super) fn new(hwi: T, kind: DeviceKind) -> Self {
        HwiSigner {
            hwi: Arc::new(hwi),
            kind,
            busy: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    async fn run<R, F>(&self, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(Arc<T>, Handle) -> std::result::Result<R, HwiError> + Send + 'static,
    {
        // Owned, and moved into the thread: held until the device call ends,
        // whether or not anyone still waits for it.
        let guard = self.busy.clone().lock_owned().await;
        let hwi = self.hwi.clone();
        let out = on_thread(move |handle| {
            let _guard = guard;
            f(hwi, handle)
        })
        .await?;
        out.map_err(|e| map_hwi_error(self.kind, &e))
    }
}

#[async_trait]
impl<T: HWI + Send + Sync + 'static> Signer for HwiSigner<T> {
    fn kind(&self) -> DeviceKind {
        self.kind
    }

    async fn fingerprint(&self) -> Result<String> {
        self.run(|hwi, handle| handle.block_on(hwi.get_master_fingerprint()))
            .await
            .map(|fp| fp.to_string())
    }

    async fn xpub(&self, path: &str) -> Result<String> {
        let path = DerivationPath::from_str(path)
            .map_err(|_| internal(format!("{path} is not a derivation path.")))?;
        self.run(move |hwi, handle| handle.block_on(hwi.get_extended_pubkey(&path)))
            .await
            .map(|xpub| xpub.to_string())
    }

    async fn sign(&self, psbt: &mut Psbt) -> Result<()> {
        // Sign a copy: the thread may outlive this call.
        let work = psbt.clone();
        let signed = self
            .run(move |hwi, handle| {
                let mut work = work;
                handle.block_on(sign_for_device(&*hwi, &mut work))?;
                Ok(work)
            })
            .await?;
        *psbt = signed;
        Ok(())
    }
}

/// `sign_tx`, after the preparation async-hwi expects of the device kind. A
/// BitBox02 (`requires_psbt_pruning`) signs with the first derivation that
/// matches its fingerprint, so derivations of the same fingerprint that are not
/// the wanted one are taken out for the signing and put back after, through
/// async-hwi's own `Bip32DerivationFilter`.
async fn sign_for_device<T: HWI>(hwi: &T, psbt: &mut Psbt) -> std::result::Result<(), HwiError> {
    if hwi.device_kind().requires_psbt_pruning() {
        Bip32DerivationFilter::new(psbt)
            .ignore_same_fg_bip32_derivations()
            .sign_psbt(hwi)
            .await
    } else {
        hwi.sign_tx(psbt).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::saving::soft_signer::tests::{SEED, built_psbt};
    use bitcoin::bip32::{Fingerprint, Xpriv};
    use bitcoin::secp256k1::Secp256k1;
    use std::sync::Mutex;
    use std::sync::mpsc;
    use std::time::Duration;

    /// An async-hwi device that answers from a script.
    #[derive(Debug)]
    struct FakeHwi {
        kind: async_hwi::DeviceKind,
        sign_result: Mutex<Option<std::result::Result<(), HwiError>>>,
        /// Derivation counts of input 0 as the device saw them.
        seen: Mutex<Vec<usize>>,
        /// When set, `sign_tx` first waits (blocking, like a real device) for a message.
        gate: Option<Mutex<mpsc::Receiver<()>>>,
        calls: Mutex<Vec<&'static str>>,
    }

    impl FakeHwi {
        fn new(kind: async_hwi::DeviceKind) -> Self {
            FakeHwi {
                kind,
                sign_result: Mutex::new(None),
                seen: Mutex::new(Vec::new()),
                gate: None,
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl HWI for FakeHwi {
        fn device_kind(&self) -> async_hwi::DeviceKind {
            self.kind
        }
        async fn get_version(&self) -> std::result::Result<async_hwi::Version, HwiError> {
            Err(HwiError::UnimplementedMethod)
        }
        async fn get_master_fingerprint(&self) -> std::result::Result<Fingerprint, HwiError> {
            self.calls.lock().unwrap().push("fingerprint");
            Ok(Fingerprint::from([0xde, 0xad, 0xbe, 0xef]))
        }
        async fn get_extended_pubkey(
            &self,
            path: &DerivationPath,
        ) -> std::result::Result<bitcoin::bip32::Xpub, HwiError> {
            let key = Xpriv::new_master(bitcoin::Network::Regtest, &SEED).unwrap();
            let secp = Secp256k1::new();
            Ok(bitcoin::bip32::Xpub::from_priv(
                &secp,
                &key.derive_priv(&secp, path).unwrap(),
            ))
        }
        async fn register_wallet(
            &self,
            _: &str,
            _: &str,
        ) -> std::result::Result<Option<[u8; 32]>, HwiError> {
            Err(HwiError::UnimplementedMethod)
        }
        async fn is_wallet_registered(
            &self,
            _: &str,
            _: &str,
        ) -> std::result::Result<bool, HwiError> {
            Err(HwiError::UnimplementedMethod)
        }
        async fn display_address(
            &self,
            _: &async_hwi::AddressScript,
        ) -> std::result::Result<(), HwiError> {
            Err(HwiError::UnimplementedMethod)
        }
        async fn sign_tx(&self, psbt: &mut Psbt) -> std::result::Result<(), HwiError> {
            self.calls.lock().unwrap().push("sign");
            if let Some(gate) = &self.gate {
                let _ = gate.lock().unwrap().recv();
            }
            self.seen
                .lock()
                .unwrap()
                .push(psbt.inputs[0].bip32_derivation.len());
            match self.sign_result.lock().unwrap().take() {
                Some(Err(e)) => Err(e),
                _ => {
                    let key = Xpriv::new_master(bitcoin::Network::Regtest, &SEED).unwrap();
                    let mut signed = psbt.clone();
                    signed
                        .sign(&key, &Secp256k1::new())
                        .map_err(|_| HwiError::DeviceDidNotSign)?;
                    psbt.inputs[0].partial_sigs = signed.inputs[0].partial_sigs.clone();
                    Ok(())
                }
            }
        }
    }

    fn psbt() -> Psbt {
        built_psbt(&Xpriv::new_master(bitcoin::Network::Regtest, &SEED).unwrap())
    }

    #[tokio::test]
    async fn rejection_maps_to_device_rejected() {
        let hwi = FakeHwi::new(async_hwi::DeviceKind::Ledger);
        *hwi.sign_result.lock().unwrap() = Some(Err(HwiError::UserRefused));
        let signer = HwiSigner::new(hwi, DeviceKind::Ledger);
        let mut psbt = psbt();
        assert_eq!(
            signer.sign(&mut psbt).await,
            Err(FedimixError::DeviceRejected)
        );
        assert!(psbt.inputs[0].partial_sigs.is_empty());
    }

    #[tokio::test]
    async fn a_device_signs_and_the_signatures_reach_the_callers_psbt() {
        let signer = HwiSigner::new(
            FakeHwi::new(async_hwi::DeviceKind::Coldcard),
            DeviceKind::Coldcard,
        );
        let mut psbt = psbt();
        signer.sign(&mut psbt).await.unwrap();
        assert_eq!(psbt.inputs[0].partial_sigs.len(), 1);
        assert_eq!(signer.fingerprint().await.unwrap(), "deadbeef");
        assert!(
            signer
                .xpub("m/84'/1'/0'")
                .await
                .unwrap()
                .starts_with("tpub")
        );
        assert!(signer.xpub("nonsense").await.is_err());
    }

    #[test]
    fn errors_are_sentences_never_debug_text() {
        let name = |k| kind_name(k).to_string();
        assert_eq!(
            map_hwi_error(DeviceKind::Jade, &HwiError::DeviceDisconnected),
            FedimixError::DeviceNotConnected(name(DeviceKind::Jade))
        );
        assert_eq!(
            map_hwi_error(DeviceKind::Coldcard, &HwiError::DeviceNotFound),
            FedimixError::DeviceNotConnected("Coldcard".into())
        );
        assert_eq!(
            map_hwi_error(DeviceKind::BitBox, &HwiError::UserRefused),
            FedimixError::DeviceRejected
        );
        // A Ledger answering from the wrong app.
        let raw = "Device {\n    status: ClaNotSupported,\n    command: 0x1,\n}";
        assert_eq!(
            map_hwi_error(DeviceKind::Ledger, &HwiError::Device(raw.into())),
            FedimixError::Internal("Open the Bitcoin app on your Ledger, then try again.".into())
        );
        // A locked Ledger answers with a status the client does not know.
        assert_eq!(
            map_hwi_error(
                DeviceKind::Ledger,
                &HwiError::Device("Device { status: Unknown }".into())
            ),
            FedimixError::DeviceNotConnected("Ledger".into())
        );
        let others = [
            HwiError::UnimplementedMethod,
            HwiError::MissingPolicy,
            HwiError::UnsupportedVersion,
            HwiError::UnsupportedInput,
            HwiError::InvalidParameter("path", "x".into()),
            HwiError::DeviceDidNotSign,
            HwiError::Device("RawDebug { weird: 1 }".into()),
            HwiError::Unexpected("raw text"),
            HwiError::NetworkMismatch,
            HwiError::Bip86ChangeIndex,
        ];
        for e in others {
            for kind in [
                DeviceKind::Ledger,
                DeviceKind::Coldcard,
                DeviceKind::Jade,
                DeviceKind::BitBox,
            ] {
                match map_hwi_error(kind, &e) {
                    FedimixError::Internal(s) => {
                        assert!(s.ends_with('.'), "{s}");
                        assert!(!s.contains(['{', '}', '\n']), "{s}");
                        assert!(!s.contains("raw") && !s.contains("Raw"), "{s}");
                    }
                    other => panic!("{e:?} gave {other:?}"),
                }
            }
        }
    }

    #[tokio::test]
    async fn bitbox_psbt_is_pruned_for_the_device_and_restored() {
        let hwi = FakeHwi::new(async_hwi::DeviceKind::BitBox02);
        let signer = HwiSigner::new(hwi, DeviceKind::BitBox);
        let mut psbt = psbt();
        // A second key of the same fingerprint, another path.
        let (pk, (fp, _)) = psbt.inputs[0]
            .bip32_derivation
            .iter()
            .next()
            .map(|(k, v)| (*k, v.clone()))
            .unwrap();
        let other = bitcoin::secp256k1::PublicKey::from_slice(&[
            2, 0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce, 0x87,
            0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81, 0x5b,
            0x16, 0xf8, 0x17, 0x98,
        ])
        .unwrap();
        assert_ne!(pk, other);
        psbt.inputs[0].bip32_derivation.insert(
            other,
            (fp, DerivationPath::from_str("m/84'/1'/0'/1/5").unwrap()),
        );
        signer.sign(&mut psbt).await.unwrap();
        assert_eq!(
            *signer.hwi.seen.lock().unwrap(),
            vec![1],
            "the device saw one derivation"
        );
        assert_eq!(psbt.inputs[0].bip32_derivation.len(), 2, "both are back");
    }

    #[tokio::test]
    async fn other_kinds_see_the_psbt_as_it_is() {
        let hwi = FakeHwi::new(async_hwi::DeviceKind::Ledger);
        let signer = HwiSigner::new(hwi, DeviceKind::Ledger);
        let mut psbt = psbt();
        signer.sign(&mut psbt).await.unwrap();
        assert_eq!(*signer.hwi.seen.lock().unwrap(), vec![1]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dropping_a_sign_stops_the_wait_and_the_next_call_follows_the_first() {
        let (release, gate) = mpsc::channel();
        let mut hwi = FakeHwi::new(async_hwi::DeviceKind::Jade);
        hwi.gate = Some(Mutex::new(gate));
        let signer = Arc::new(HwiSigner::new(hwi, DeviceKind::Jade));

        // The user is slow: the sign is dropped after 100 ms.
        let mut psbt = psbt();
        let waited = tokio::time::timeout(Duration::from_millis(100), signer.sign(&mut psbt)).await;
        assert!(waited.is_err(), "still waiting, and now dropped");
        assert!(psbt.inputs[0].partial_sigs.is_empty());

        // Another command does not interleave: it waits for the first to end.
        let again = {
            let signer = signer.clone();
            tokio::spawn(async move { signer.fingerprint().await })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!again.is_finished());
        assert_eq!(*signer.hwi.calls.lock().unwrap(), vec!["sign"]);

        release.send(()).unwrap();
        assert_eq!(again.await.unwrap().unwrap(), "deadbeef");
        assert_eq!(
            *signer.hwi.calls.lock().unwrap(),
            vec!["sign", "fingerprint"]
        );
    }
}
