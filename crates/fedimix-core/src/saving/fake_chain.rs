//! A scripted [`ChainClient`], for tests of the wallet above it.

use std::collections::{HashSet, VecDeque};
use std::sync::Mutex;

use async_trait::async_trait;
use bdk_wallet::KeychainKind;
use bdk_wallet::bitcoin::{Transaction, Txid};
use bdk_wallet::chain::spk_client::{FullScanRequest, FullScanResponse, SyncRequest, SyncResponse};
use tokio::sync::watch;

use super::ChainClient;
use super::chain::refused;
use crate::error::{FedimixError, Result};

/// How the next `broadcast` goes, when it is not plainly accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BroadcastGoes {
    /// Never reached the server.
    Offline,
    /// Reached the server, and the connection dropped before the answer.
    AnswerLost,
    /// The server judged the transaction invalid and said no.
    Refused,
    /// The server's policy said no: its fee floor.
    LowFee,
    /// The server answered with something that judges nothing.
    Unclear,
}

#[derive(Default)]
struct State {
    offline: bool,
    panics: bool,
    fee_rate: Option<f64>,
    full_scans: VecDeque<FullScanResponse<KeychainKind>>,
    syncs: VecDeque<SyncResponse>,
    known: HashSet<Txid>,
    broadcasts: Vec<Transaction>,
    broadcast_script: VecDeque<BroadcastGoes>,
    /// Every transaction offered to `broadcast`, whatever came of it.
    offered: Vec<Txid>,
    /// Transactions the server no longer has: a sync that was told to expect
    /// one reports it evicted, as bdk_esplora and bdk_electrum do.
    missing: HashSet<Txid>,
    /// What the last sync was told to expect.
    expected: Vec<Txid>,
    scans_made: usize,
    full_scans_made: usize,
}

/// Answers from a queue: an empty response when the queue is empty. `offline`
/// makes every call fail as an unreachable server does.
#[derive(Default)]
pub struct FakeChain {
    state: Mutex<State>,
    stalled: watch::Sender<bool>,
    broadcasts_held: watch::Sender<bool>,
}

impl FakeChain {
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Waits out a stall, then answers as the server is now.
    async fn check_online(&self) -> Result<()> {
        let _ = self.stalled.subscribe().wait_for(|stalled| !stalled).await;
        assert!(!self.state().panics, "the fake chain was told to panic");
        if self.state().offline {
            Err(FedimixError::Offline)
        } else {
            Ok(())
        }
    }

    pub fn set_offline(&self, offline: bool) {
        self.state().offline = offline;
    }

    /// While stalled, a scan or sync hangs without an answer, as a server that
    /// accepted the connection and went quiet does.
    pub fn set_stalled(&self, stalled: bool) {
        self.stalled.send_replace(stalled);
    }

    /// While set, `broadcast` alone hangs.
    pub fn hold_broadcasts(&self, held: bool) {
        self.broadcasts_held.send_replace(held);
    }

    /// While set, every call panics, as a bug in a client library would.
    pub fn set_panics(&self, panics: bool) {
        self.state().panics = panics;
    }

    pub fn set_fee_rate(&self, rate: Option<f64>) {
        self.state().fee_rate = rate;
    }

    /// The next `full_scan` returns `response`.
    pub fn push_full_scan(&self, response: FullScanResponse<KeychainKind>) {
        self.state().full_scans.push_back(response);
    }

    /// The next `sync` returns `response`.
    pub fn push_sync(&self, response: SyncResponse) {
        self.state().syncs.push_back(response);
    }

    /// The server knows `txid`, as if it had been mined by someone else.
    pub fn know(&self, txid: Txid) {
        self.state().known.insert(txid);
    }

    /// The next `broadcast`s go like this, in order.
    pub fn script_broadcasts(&self, script: impl IntoIterator<Item = BroadcastGoes>) {
        self.state().broadcast_script.extend(script);
    }

    /// Forgets what `script_broadcasts` left: from now on the server accepts.
    pub fn clear_broadcast_script(&self) {
        self.state().broadcast_script.clear();
    }

    /// The server lost `txid`: it forgets it, and stops listing it.
    pub fn lose(&self, txid: Txid) {
        let mut state = self.state();
        state.known.remove(&txid);
        state.missing.insert(txid);
    }

    /// How often `txid` was offered to `broadcast`, accepted or not.
    pub fn offers_of(&self, txid: Txid) -> usize {
        self.state().offered.iter().filter(|t| **t == txid).count()
    }

    /// The transactions the last sync's request expected the server to list.
    pub fn expected(&self) -> Vec<Txid> {
        self.state().expected.clone()
    }

    /// Every transaction handed to `broadcast`, in order, repeats included.
    pub fn broadcasts(&self) -> Vec<Transaction> {
        self.state().broadcasts.clone()
    }

    /// How many of those were full scans.
    pub fn full_scans_made(&self) -> usize {
        self.state().full_scans_made
    }

    /// How many scans and syncs were asked for.
    pub fn scans_made(&self) -> usize {
        self.state().scans_made
    }
}

#[async_trait]
impl ChainClient for FakeChain {
    async fn full_scan(
        &self,
        _request: FullScanRequest<KeychainKind>,
        _stop_gap: usize,
    ) -> Result<FullScanResponse<KeychainKind>> {
        self.check_online().await?;
        let mut state = self.state();
        state.scans_made += 1;
        state.full_scans_made += 1;
        Ok(state.full_scans.pop_front().unwrap_or_default())
    }

    async fn sync(&self, mut request: SyncRequest<(KeychainKind, u32)>) -> Result<SyncResponse> {
        self.check_online().await?;
        let started = request.start_time();
        let expected: Vec<Txid> = request
            .iter_spks_with_expected_txids()
            .flat_map(|spk| spk.expected_txids)
            .collect();
        let mut state = self.state();
        state.scans_made += 1;
        let mut response = state.syncs.pop_front().unwrap_or_default();
        for txid in &expected {
            if state.missing.contains(txid) {
                response.tx_update.evicted_ats.insert((*txid, started));
            }
        }
        state.expected = expected;
        Ok(response)
    }

    async fn broadcast(&self, tx: &Transaction) -> Result<()> {
        self.check_online().await?;
        let _ = self
            .broadcasts_held
            .subscribe()
            .wait_for(|held| !held)
            .await;
        let mut state = self.state();
        let txid = tx.compute_txid();
        state.offered.push(txid);
        let goes = state.broadcast_script.pop_front();
        if goes == Some(BroadcastGoes::Offline) {
            return Err(FedimixError::Offline);
        }
        if goes == Some(BroadcastGoes::Refused) {
            return Err(FedimixError::TxRejected(
                "bad-txns-inputs-missingorspent".into(),
            ));
        }
        if goes == Some(BroadcastGoes::LowFee) {
            return Err(FedimixError::TxRejected("mempool min fee not met".into()));
        }
        if goes == Some(BroadcastGoes::Unclear) {
            return Err(refused("server busy"));
        }
        state.broadcasts.push(tx.clone());
        state.known.insert(txid);
        state.missing.remove(&txid);
        if goes == Some(BroadcastGoes::AnswerLost) {
            return Err(FedimixError::Offline);
        }
        Ok(())
    }

    async fn fee_rate_sat_vb(&self, _target_blocks: u16) -> Result<Option<f64>> {
        self.check_online().await?;
        Ok(self.state().fee_rate)
    }

    async fn tx_seen(&self, txid: Txid) -> Result<bool> {
        self.check_online().await?;
        Ok(self.state().known.contains(&txid))
    }
}

#[cfg(test)]
mod tests {
    use bdk_wallet::bitcoin::{absolute::LockTime, transaction::Version};

    use super::*;

    fn tx() -> Transaction {
        Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![],
            output: vec![],
        }
    }

    #[tokio::test]
    async fn the_fake_remembers_broadcasts_and_goes_offline() {
        let chain = FakeChain::new();
        let tx = tx();
        assert!(!chain.tx_seen(tx.compute_txid()).await.unwrap());
        chain.broadcast(&tx).await.unwrap();
        chain.broadcast(&tx).await.unwrap();
        assert!(chain.tx_seen(tx.compute_txid()).await.unwrap());
        assert_eq!(chain.broadcasts().len(), 2);
        chain.set_fee_rate(Some(3.0));
        assert_eq!(chain.fee_rate_sat_vb(6).await.unwrap(), Some(3.0));
        chain.set_offline(true);
        assert_eq!(chain.fee_rate_sat_vb(6).await, Err(FedimixError::Offline));
        assert_eq!(chain.broadcast(&tx).await, Err(FedimixError::Offline));
    }
}
