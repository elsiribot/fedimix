//! Chain access for Saving: Esplora or Electrum behind [`ChainClient`], so
//! the wallet logic above never knows which one it talks to.
//!
//! Both backends go through the SOCKS5 proxy when one is set, and fail closed:
//! a proxy that is down is an error, never a direct connection.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bdk_electrum::BdkElectrumClient;
use bdk_electrum::electrum_client::{self, ElectrumApi};
use bdk_esplora::EsploraAsyncExt;
use bdk_esplora::esplora_client;
use bdk_wallet::KeychainKind;
use bdk_wallet::bitcoin::{Transaction, Txid};
use bdk_wallet::chain::spk_client::{FullScanRequest, FullScanResponse, SyncRequest, SyncResponse};

use crate::api::ChainBackend;
use crate::error::{FedimixError, Result};

/// How long one request to the chain server may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Esplora: how many requests a scan has in flight.
const ESPLORA_PARALLEL: usize = 4;
/// Electrum: how many scripts a scan asks for in one batch.
const ELECTRUM_BATCH: usize = 10;

/// What Saving needs from the chain. Errors are sentences; a server that
/// cannot be reached is [`FedimixError::Offline`].
#[async_trait]
pub trait ChainClient: Send + Sync {
    async fn full_scan(
        &self,
        request: FullScanRequest<KeychainKind>,
        stop_gap: usize,
    ) -> Result<FullScanResponse<KeychainKind>>;

    async fn sync(&self, request: SyncRequest<(KeychainKind, u32)>) -> Result<SyncResponse>;

    /// A transaction the server already knows, in its pool or in a block, is
    /// `Ok`: a rebroadcast after a restart cannot pay twice.
    async fn broadcast(&self, tx: &Transaction) -> Result<()>;

    /// Sat/vB for confirmation within `target_blocks`, or `None` when the
    /// server has no estimate (regtest has none).
    async fn fee_rate_sat_vb(&self, target_blocks: u16) -> Result<Option<f64>>;

    /// Whether the server knows the transaction, confirmed or not.
    async fn tx_seen(&self, txid: Txid) -> Result<bool>;
}

/// The client for `backend`, through `proxy` when set. A proxy value may be
/// `host:port`, `socks5://host:port` or `socks5h://host:port`. Nothing is
/// requested here: an unreachable server shows on the first call.
pub fn connect_chain(backend: &ChainBackend, proxy: Option<&str>) -> Result<Arc<dyn ChainClient>> {
    let proxy = proxy.map(proxy_host_port).transpose()?;
    match backend {
        ChainBackend::Esplora { url } => {
            let url = url.trim();
            if url.is_empty() {
                return Err(no_server());
            }
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err(FedimixError::NotSetUp(
                    "The Esplora address must start with http:// or https://.".into(),
                ));
            }
            let mut builder = esplora_client::Builder::new(url).timeout(REQUEST_TIMEOUT.as_secs());
            if let Some(proxy) = &proxy {
                // socks5h: the proxy resolves host names, so DNS does not leak.
                builder = builder.proxy(&format!("socks5h://{proxy}"));
            }
            let client = builder.build_async().map_err(|e| esplora_error(&e))?;
            Ok(Arc::new(EsploraChain { client }))
        }
        ChainBackend::Electrum { url } => {
            let url = url.trim();
            if url.is_empty() {
                return Err(no_server());
            }
            if !(url.starts_with("ssl://") || url.starts_with("tcp://")) {
                return Err(FedimixError::NotSetUp(
                    "The Electrum address must start with ssl:// or tcp://.".into(),
                ));
            }
            Ok(Arc::new(ElectrumHandle(Arc::new(ElectrumChain {
                url: url.to_string(),
                proxy,
                client: Mutex::new(None),
            }))))
        }
    }
}

fn no_server() -> FedimixError {
    FedimixError::NotSetUp("Choose a chain server in Settings.".into())
}

/// `host:port` of a proxy setting, with or without a `socks5://` or
/// `socks5h://` prefix.
fn proxy_host_port(proxy: &str) -> Result<String> {
    let proxy = proxy.trim();
    let bare = proxy
        .strip_prefix("socks5h://")
        .or_else(|| proxy.strip_prefix("socks5://"))
        .unwrap_or(proxy)
        .trim_end_matches('/');
    match bare.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && port.parse::<u16>().is_ok() => {
            Ok(bare.to_string())
        }
        _ => Err(FedimixError::NotSetUp(
            "The proxy must look like 127.0.0.1:9050.".into(),
        )),
    }
}

/// A sentence for a refusal the server explained.
fn refused(reason: &str) -> FedimixError {
    let reason = reason.trim().trim_end_matches('.');
    FedimixError::Internal(format!("The chain server refused the request: {reason}."))
}

fn unreadable() -> FedimixError {
    FedimixError::Internal("The chain server sent an answer Fedimix could not read.".into())
}

/// The shared tail of both `broadcast`s: a failure is fine when the server
/// already knows the transaction, as then it is the same transaction again.
async fn already_known(
    chain: &dyn ChainClient,
    tx: &Transaction,
    error: FedimixError,
) -> Result<()> {
    match chain.tx_seen(tx.compute_txid()).await {
        Ok(true) => Ok(()),
        _ => Err(error),
    }
}

/// The estimate for `target` from a map of blocks to sat/vB: the entry for
/// the nearest target at or above it, else the highest below. An estimate
/// that is not a positive number is no estimate.
fn pick_estimate(estimates: &std::collections::HashMap<u16, f64>, target: u16) -> Option<f64> {
    let key = estimates
        .keys()
        .filter(|k| **k >= target)
        .min()
        .or_else(|| estimates.keys().max())?;
    estimates
        .get(key)
        .copied()
        .filter(|rate| rate.is_finite() && *rate > 0.0)
}

// Esplora

struct EsploraChain {
    client: esplora_client::AsyncClient,
}

fn esplora_error(error: &esplora_client::Error) -> FedimixError {
    use esplora_client::Error;
    match error {
        Error::Reqwest(e) if !e.is_decode() => FedimixError::Offline,
        Error::HttpResponse { status, .. } if matches!(*status, 502..=504) => FedimixError::Offline,
        Error::HttpResponse { message, .. } => refused(message),
        _ => unreadable(),
    }
}

#[async_trait]
impl ChainClient for EsploraChain {
    async fn full_scan(
        &self,
        request: FullScanRequest<KeychainKind>,
        stop_gap: usize,
    ) -> Result<FullScanResponse<KeychainKind>> {
        self.client
            .full_scan(request, stop_gap, ESPLORA_PARALLEL)
            .await
            .map_err(|e| esplora_error(&e))
    }

    async fn sync(&self, request: SyncRequest<(KeychainKind, u32)>) -> Result<SyncResponse> {
        self.client
            .sync(request, ESPLORA_PARALLEL)
            .await
            .map_err(|e| esplora_error(&e))
    }

    async fn broadcast(&self, tx: &Transaction) -> Result<()> {
        match self.client.broadcast(tx).await {
            Ok(()) => Ok(()),
            Err(e) => already_known(self, tx, esplora_error(&e)).await,
        }
    }

    async fn fee_rate_sat_vb(&self, target_blocks: u16) -> Result<Option<f64>> {
        let estimates = self
            .client
            .get_fee_estimates()
            .await
            .map_err(|e| esplora_error(&e))?;
        Ok(pick_estimate(&estimates, target_blocks))
    }

    async fn tx_seen(&self, txid: Txid) -> Result<bool> {
        let tx = self
            .client
            .get_tx(&txid)
            .await
            .map_err(|e| esplora_error(&e))?;
        Ok(tx.is_some())
    }
}

// Electrum

/// electrum-client blocks, so every call runs in `spawn_blocking`. The
/// connection is made on first use, and again after a failure, so an
/// unreachable server is an error of a call, not of `connect_chain`.
struct ElectrumChain {
    url: String,
    /// `host:port`.
    proxy: Option<String>,
    client: Mutex<Option<Arc<BdkElectrumClient<electrum_client::Client>>>>,
}

fn electrum_error(error: &electrum_client::Error) -> FedimixError {
    use electrum_client::Error;
    match error {
        Error::Protocol(value) => refused(
            value
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("it gave no reason"),
        ),
        Error::InvalidResponse(_) | Error::JSON(_) | Error::Hex(_) | Error::Bitcoin(_) => {
            unreadable()
        }
        // Sockets, proxies, TLS and timeouts: the server cannot be reached.
        _ => FedimixError::Offline,
    }
}

impl ElectrumChain {
    /// Runs `call` on a blocking thread with the connection.
    async fn call<T: Send + 'static>(
        self: &Arc<Self>,
        call: impl FnOnce(
            &BdkElectrumClient<electrum_client::Client>,
        ) -> std::result::Result<T, electrum_client::Error>
        + Send
        + 'static,
    ) -> Result<T> {
        let this = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let client = this.connection()?;
            call(&client).map_err(|e| {
                // A failed connection is not reused; a refusal says nothing about it.
                if !matches!(e, electrum_client::Error::Protocol(_)) {
                    *this.client.lock().unwrap_or_else(|p| p.into_inner()) = None;
                }
                electrum_error(&e)
            })
        })
        .await
        .map_err(|e| {
            FedimixError::Internal(format!("The chain request stopped unexpectedly: {e}."))
        })?
    }

    fn connection(&self) -> Result<Arc<BdkElectrumClient<electrum_client::Client>>> {
        let mut slot = self.client.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(client) = &*slot {
            return Ok(Arc::clone(client));
        }
        let config = electrum_client::ConfigBuilder::new()
            .timeout(Some(REQUEST_TIMEOUT))
            .socks5(
                self.proxy
                    .as_deref()
                    .map(electrum_client::Socks5Config::new),
            )
            .build();
        let client = electrum_client::Client::from_config(&self.url, config)
            .map_err(|e| electrum_error(&e))?;
        let client = Arc::new(BdkElectrumClient::new(client));
        *slot = Some(Arc::clone(&client));
        Ok(client)
    }
}

/// `ChainClient` for the shared `ElectrumChain`, which `call` needs as an `Arc`.
struct ElectrumHandle(Arc<ElectrumChain>);

#[async_trait]
impl ChainClient for ElectrumHandle {
    async fn full_scan(
        &self,
        request: FullScanRequest<KeychainKind>,
        stop_gap: usize,
    ) -> Result<FullScanResponse<KeychainKind>> {
        self.0
            .call(move |c| c.full_scan(request, stop_gap, ELECTRUM_BATCH, true))
            .await
    }

    async fn sync(&self, request: SyncRequest<(KeychainKind, u32)>) -> Result<SyncResponse> {
        self.0
            .call(move |c| c.sync(request, ELECTRUM_BATCH, true))
            .await
    }

    async fn broadcast(&self, tx: &Transaction) -> Result<()> {
        let owned = tx.clone();
        match self
            .0
            .call(move |c| c.transaction_broadcast(&owned).map(|_| ()))
            .await
        {
            Ok(()) => Ok(()),
            Err(e) => already_known(self, tx, e).await,
        }
    }

    async fn fee_rate_sat_vb(&self, target_blocks: u16) -> Result<Option<f64>> {
        let btc_per_kvb = self
            .0
            .call(move |c| {
                c.inner
                    .estimate_fee(usize::from(target_blocks.max(1)), None)
            })
            .await?;
        // The server says -1 when it has no estimate.
        Ok(Some(btc_per_kvb * 100_000.0).filter(|rate| rate.is_finite() && *rate > 0.0))
    }

    async fn tx_seen(&self, txid: Txid) -> Result<bool> {
        match self.0.call(move |c| c.inner.transaction_get(&txid)).await {
            Ok(_) => Ok(true),
            // "No such mempool or blockchain transaction" is a refusal.
            Err(FedimixError::Internal(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn proxy_forms_normalise_to_host_port() {
        for form in [
            "127.0.0.1:9050",
            "socks5://127.0.0.1:9050",
            "socks5h://127.0.0.1:9050",
            " socks5h://127.0.0.1:9050/ ",
        ] {
            assert_eq!(proxy_host_port(form).unwrap(), "127.0.0.1:9050", "{form}");
        }
        for bad in ["", "9050", "socks5h://", "host:", "host:http"] {
            assert!(proxy_host_port(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn server_addresses_are_checked() {
        let esplora = |url: &str| connect_chain(&ChainBackend::Esplora { url: url.into() }, None);
        let electrum = |url: &str| connect_chain(&ChainBackend::Electrum { url: url.into() }, None);
        assert!(esplora("https://blockstream.info/api").is_ok());
        assert!(esplora("").is_err());
        assert!(esplora("ssl://host:50002").is_err());
        assert!(electrum("ssl://host:50002").is_ok());
        assert!(electrum("tcp://127.0.0.1:50001").is_ok());
        assert!(electrum("http://host").is_err());
        assert!(electrum("").is_err());
    }

    #[test]
    fn estimate_picks_the_nearest_target_at_or_above() {
        let map = HashMap::from([(2, 9.0), (6, 4.0), (144, 1.0)]);
        assert_eq!(pick_estimate(&map, 6), Some(4.0));
        assert_eq!(pick_estimate(&map, 3), Some(4.0));
        assert_eq!(pick_estimate(&map, 500), Some(1.0));
        assert_eq!(pick_estimate(&HashMap::new(), 6), None);
        assert_eq!(pick_estimate(&HashMap::from([(6, 0.0)]), 6), None);
    }
}
