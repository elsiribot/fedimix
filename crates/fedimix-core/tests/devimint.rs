//! Smoke test against a devimint federation. Run through
//! `just test-integration`, which starts devimint for both module generations.
use std::str::FromStr;
use std::sync::Arc;

use fedimint_bip39::Bip39RootSecretStrategy;
use fedimint_client::secret::RootSecretStrategy;
use fedimint_client::{Client, RootSecret};
use fedimint_connectors::ConnectorRegistry;
use fedimint_core::invite_code::InviteCode;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs devimint"]
async fn smoke_join_and_read_modules() {
    let invite = std::env::var("FM_INVITE_CODE").expect("FM_INVITE_CODE is set by devimint");
    let dir = tempfile::tempdir().unwrap();

    let mnemonic = Bip39RootSecretStrategy::<12>::random(&mut rand::thread_rng());
    let connectors = ConnectorRegistry::build_from_client_defaults().bind().await;
    let mut builder = Client::builder().await;
    builder.with_module(fedimint_lnv2_client::LightningClientInit::default());
    builder.with_module(fedimint_mint_client::MintClientInit);
    builder.with_module(fedimint_mintv2_client::MintClientInit);
    builder.with_module(fedimint_wallet_client::WalletClientInit(None));
    builder.with_module(fedimint_walletv2_client::WalletClientInit);

    let preview = builder
        .preview(connectors, &InviteCode::from_str(&invite).unwrap())
        .await
        .unwrap();
    let kinds: Vec<String> = preview
        .config()
        .modules
        .values()
        .map(|m| m.kind().to_string())
        .collect();
    eprintln!("module kinds: {kinds:?}");
    assert!(kinds.iter().any(|k| k == "lnv2"));

    let db = fedimint_rocksdb::RocksDb::build(dir.path().join("client.rocksdb"))
        .open()
        .await
        .unwrap()
        .into();
    let client = Arc::new(
        preview
            .join(
                db,
                RootSecret::StandardDoubleDerive(Bip39RootSecretStrategy::<12>::to_root_secret(
                    &mnemonic,
                )),
            )
            .await
            .unwrap(),
    );
    assert_eq!(client.get_balance_for_btc().await.unwrap().msats, 0);
}
