//! What Fedimix knows about Saving coins that the chain does not say: the
//! privacy tag, the federation a coin came from, whether the user froze it,
//! and which outgoing transactions sent change to Spending.
//!
//! A [`Labels`] is a snapshot read from the store; its setters write through
//! first, so a crash never leaves the snapshot ahead of the database.

use std::collections::HashMap;

use crate::api::{PrivacyTag, SentChange};
use crate::error::Result;
use crate::store::Store;

/// The privacy standing of one coin. Anything without a label is `Unmixed`:
/// the claim that is safe to make about a coin nobody has looked at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tag {
    #[default]
    Unmixed,
    SmallChange,
    Anonymity {
        estimate: u32,
    },
    EarlyExit {
        estimate: u32,
    },
}

impl Tag {
    pub(crate) fn to_public(self) -> PrivacyTag {
        match self {
            Tag::Unmixed => PrivacyTag::Unmixed,
            Tag::SmallChange => PrivacyTag::SmallChange,
            Tag::Anonymity { estimate } => PrivacyTag::Anonymity { estimate },
            Tag::EarlyExit { estimate } => PrivacyTag::EarlyExit { estimate },
        }
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "RealBackend uses it from Task 5")
    )]
    /// The stored name and estimate.
    pub(crate) fn to_parts(self) -> (&'static str, Option<u32>) {
        match self {
            Tag::Unmixed => ("unmixed", None),
            Tag::SmallChange => ("small_change", None),
            Tag::Anonymity { estimate } => ("anonymity", Some(estimate)),
            Tag::EarlyExit { estimate } => ("early_exit", Some(estimate)),
        }
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "RealBackend uses it from Task 5")
    )]
    /// `None` for a name this version does not know; the coin then reads as
    /// unlabelled rather than failing the whole wallet.
    pub(crate) fn from_parts(name: &str, estimate: Option<u32>) -> Option<Tag> {
        let estimate = estimate.unwrap_or(0);
        Some(match name {
            "unmixed" => Tag::Unmixed,
            "small_change" => Tag::SmallChange,
            "anonymity" => Tag::Anonymity { estimate },
            "early_exit" => Tag::EarlyExit { estimate },
            _ => return None,
        })
    }
}

/// One coin's label. `tag: None` is a coin that is only frozen or only has a
/// source, and reads as `Unmixed`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Label {
    pub tag: Option<Tag>,
    pub source_federation: Option<String>,
    pub frozen: bool,
}

/// An outgoing transaction Fedimix built: where its change went. `change` is
/// `None` when the send had no change at all.
#[derive(Debug, Clone, PartialEq)]
pub struct SentRecord {
    pub change: Option<SentChange>,
}

#[derive(Debug, Clone, Default)]
pub struct Labels {
    coins: HashMap<String, Label>,
    sent: HashMap<String, SentRecord>,
}

impl Labels {
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "RealBackend uses it from Task 5")
    )]
    pub(crate) fn from_rows(
        coins: HashMap<String, Label>,
        sent: HashMap<String, SentRecord>,
    ) -> Self {
        Labels { coins, sent }
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "RealBackend uses it from Task 5")
    )]
    pub(crate) fn load(store: &Store) -> Result<Labels> {
        store.saving_labels()
    }

    /// The label of `outpoint` (`txid:vout`), if any was ever written.
    pub fn get(&self, outpoint: &str) -> Option<&Label> {
        self.coins.get(outpoint)
    }

    pub fn tag(&self, outpoint: &str) -> Tag {
        self.get(outpoint).and_then(|l| l.tag).unwrap_or_default()
    }

    pub fn is_frozen(&self, outpoint: &str) -> bool {
        self.get(outpoint).is_some_and(|l| l.frozen)
    }

    pub fn sent(&self, txid: &str) -> Option<&SentRecord> {
        self.sent.get(txid)
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "RealBackend uses it from Task 5")
    )]
    pub(crate) fn set_tag(
        &mut self,
        store: &Store,
        outpoint: &str,
        tag: Tag,
        source_federation: Option<String>,
    ) -> Result<()> {
        let mut label = self.coins.get(outpoint).cloned().unwrap_or_default();
        label.tag = Some(tag);
        label.source_federation = source_federation;
        store.put_saving_label(outpoint, &label)?;
        self.coins.insert(outpoint.to_string(), label);
        Ok(())
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "RealBackend uses it from Task 5")
    )]
    pub(crate) fn set_frozen(&mut self, store: &Store, outpoint: &str, frozen: bool) -> Result<()> {
        let mut label = self.coins.get(outpoint).cloned().unwrap_or_default();
        label.frozen = frozen;
        store.put_saving_label(outpoint, &label)?;
        self.coins.insert(outpoint.to_string(), label);
        Ok(())
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "RealBackend uses it from Task 5")
    )]
    /// Notes that the transaction `txid` is a send Fedimix built, with where
    /// its change went. Task 6 calls this before it broadcasts.
    pub(crate) fn record_sent(
        &mut self,
        store: &Store,
        txid: &str,
        change: Option<SentChange>,
    ) -> Result<()> {
        let record = SentRecord { change };
        store.put_saving_sent(txid, &record)?;
        self.sent.insert(txid.to_string(), record);
        Ok(())
    }
}
