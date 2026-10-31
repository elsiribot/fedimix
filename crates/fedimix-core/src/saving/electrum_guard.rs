//! An Electrum client that can be closed from outside.
//!
//! `BdkElectrumClient` scans in a blocking loop with no way to stop, and
//! electrum-client keeps its socket private, so nothing can shut the connection
//! under a running call. Every request goes through this type instead, so once
//! [`Guarded::close`] is called no further request leaves: the scan fails at its
//! next step. Only a request already written can still be answered.
//!
//! The methods are the required ones of `ElectrumApi`, each behind the same
//! check; its provided ones all end in these.

use std::borrow::Borrow;
use std::sync::atomic::{AtomicBool, Ordering};

use bdk_electrum::electrum_client::bitcoin::{Script, Txid};
use bdk_electrum::electrum_client::{self, Batch, Client, ElectrumApi, Error, Param};
use electrum_client::{
    BroadcastPackageRes, EstimationMode, GetBalanceRes, GetHeadersRes, GetHistoryRes, GetMerkleRes,
    ListUnspentRes, MempoolInfoRes, RawHeaderNotification, ScriptStatus, ServerFeaturesRes,
    TxidFromPosRes,
};

pub(super) struct Guarded {
    client: Client,
    closed: AtomicBool,
}

impl Guarded {
    pub(super) fn new(client: Client) -> Self {
        Self {
            client,
            closed: AtomicBool::new(false),
        }
    }

    /// No request leaves after this.
    pub(super) fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    fn open(&self) -> Result<(), Error> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(Error::IOError(std::io::Error::other(
                "the connection was closed",
            )));
        }
        Ok(())
    }
}

impl ElectrumApi for Guarded {
    fn raw_call(
        &self,
        method_name: &str,
        params: impl IntoIterator<Item = Param>,
    ) -> Result<serde_json::Value, Error> {
        self.open()?;
        self.client.raw_call(method_name, params)
    }

    fn batch_call(&self, batch: &Batch) -> Result<Vec<serde_json::Value>, Error> {
        self.open()?;
        self.client.batch_call(batch)
    }

    fn block_headers_subscribe_raw(&self) -> Result<RawHeaderNotification, Error> {
        self.open()?;
        self.client.block_headers_subscribe_raw()
    }

    fn block_headers_pop_raw(&self) -> Result<Option<RawHeaderNotification>, Error> {
        self.open()?;
        self.client.block_headers_pop_raw()
    }

    fn block_header_raw(&self, height: usize) -> Result<Vec<u8>, Error> {
        self.open()?;
        self.client.block_header_raw(height)
    }

    fn block_headers(&self, start_height: usize, count: usize) -> Result<GetHeadersRes, Error> {
        self.open()?;
        self.client.block_headers(start_height, count)
    }

    fn estimate_fee(&self, number: usize, mode: Option<EstimationMode>) -> Result<f64, Error> {
        self.open()?;
        self.client.estimate_fee(number, mode)
    }

    fn relay_fee(&self) -> Result<f64, Error> {
        self.open()?;
        self.client.relay_fee()
    }

    fn script_subscribe(&self, script: &Script) -> Result<Option<ScriptStatus>, Error> {
        self.open()?;
        self.client.script_subscribe(script)
    }

    fn batch_script_subscribe<'s, I>(&self, scripts: I) -> Result<Vec<Option<ScriptStatus>>, Error>
    where
        I: IntoIterator + Clone,
        I::Item: Borrow<&'s Script>,
    {
        self.open()?;
        self.client.batch_script_subscribe(scripts)
    }

    fn script_unsubscribe(&self, script: &Script) -> Result<bool, Error> {
        self.open()?;
        self.client.script_unsubscribe(script)
    }

    fn script_pop(&self, script: &Script) -> Result<Option<ScriptStatus>, Error> {
        self.open()?;
        self.client.script_pop(script)
    }

    fn script_get_balance(&self, script: &Script) -> Result<GetBalanceRes, Error> {
        self.open()?;
        self.client.script_get_balance(script)
    }

    fn batch_script_get_balance<'s, I>(&self, scripts: I) -> Result<Vec<GetBalanceRes>, Error>
    where
        I: IntoIterator + Clone,
        I::Item: Borrow<&'s Script>,
    {
        self.open()?;
        self.client.batch_script_get_balance(scripts)
    }

    fn script_get_history(&self, script: &Script) -> Result<Vec<GetHistoryRes>, Error> {
        self.open()?;
        self.client.script_get_history(script)
    }

    fn batch_script_get_history<'s, I>(&self, scripts: I) -> Result<Vec<Vec<GetHistoryRes>>, Error>
    where
        I: IntoIterator + Clone,
        I::Item: Borrow<&'s Script>,
    {
        self.open()?;
        self.client.batch_script_get_history(scripts)
    }

    fn script_list_unspent(&self, script: &Script) -> Result<Vec<ListUnspentRes>, Error> {
        self.open()?;
        self.client.script_list_unspent(script)
    }

    fn batch_script_list_unspent<'s, I>(
        &self,
        scripts: I,
    ) -> Result<Vec<Vec<ListUnspentRes>>, Error>
    where
        I: IntoIterator + Clone,
        I::Item: Borrow<&'s Script>,
    {
        self.open()?;
        self.client.batch_script_list_unspent(scripts)
    }

    fn transaction_get_raw(&self, txid: &Txid) -> Result<Vec<u8>, Error> {
        self.open()?;
        self.client.transaction_get_raw(txid)
    }

    fn batch_transaction_get_raw<'t, I>(&self, txids: I) -> Result<Vec<Vec<u8>>, Error>
    where
        I: IntoIterator + Clone,
        I::Item: Borrow<&'t Txid>,
    {
        self.open()?;
        self.client.batch_transaction_get_raw(txids)
    }

    fn batch_block_header_raw<I>(&self, heights: I) -> Result<Vec<Vec<u8>>, Error>
    where
        I: IntoIterator + Clone,
        I::Item: Borrow<u32>,
    {
        self.open()?;
        self.client.batch_block_header_raw(heights)
    }

    fn batch_estimate_fee<I>(&self, numbers: I) -> Result<Vec<f64>, Error>
    where
        I: IntoIterator + Clone,
        I::Item: Borrow<usize>,
    {
        self.open()?;
        self.client.batch_estimate_fee(numbers)
    }

    fn transaction_broadcast_raw(&self, raw_tx: &[u8]) -> Result<Txid, Error> {
        self.open()?;
        self.client.transaction_broadcast_raw(raw_tx)
    }

    fn transaction_broadcast_package_raw<T: AsRef<[u8]>>(
        &self,
        raw_txs: &[T],
    ) -> Result<BroadcastPackageRes, Error> {
        self.open()?;
        self.client.transaction_broadcast_package_raw(raw_txs)
    }

    fn transaction_get_merkle(&self, txid: &Txid, height: usize) -> Result<GetMerkleRes, Error> {
        self.open()?;
        self.client.transaction_get_merkle(txid, height)
    }

    fn batch_transaction_get_merkle<I>(
        &self,
        txids_and_heights: I,
    ) -> Result<Vec<GetMerkleRes>, Error>
    where
        I: IntoIterator + Clone,
        I::Item: Borrow<(Txid, usize)>,
    {
        self.open()?;
        self.client.batch_transaction_get_merkle(txids_and_heights)
    }

    fn txid_from_pos(&self, height: usize, tx_pos: usize) -> Result<Txid, Error> {
        self.open()?;
        self.client.txid_from_pos(height, tx_pos)
    }

    fn txid_from_pos_with_merkle(
        &self,
        height: usize,
        tx_pos: usize,
    ) -> Result<TxidFromPosRes, Error> {
        self.open()?;
        self.client.txid_from_pos_with_merkle(height, tx_pos)
    }

    fn server_features(&self) -> Result<ServerFeaturesRes, Error> {
        self.open()?;
        self.client.server_features()
    }

    fn protocol_version(&self) -> Result<String, Error> {
        self.open()?;
        self.client.protocol_version()
    }

    fn mempool_get_info(&self) -> Result<MempoolInfoRes, Error> {
        self.open()?;
        self.client.mempool_get_info()
    }

    fn ping(&self) -> Result<(), Error> {
        self.open()?;
        self.client.ping()
    }

    fn calls_made(&self) -> Result<usize, Error> {
        self.open()?;
        self.client.calls_made()
    }
}
