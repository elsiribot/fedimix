//! What Fedimix knows about Saving coins that the chain does not say: the
//! privacy tag, the federation a coin came from, whether the user froze it,
//! and which outgoing transactions sent change to Spending.
//!
//! A [`Labels`] is a snapshot read from the store; its setters write through
//! first, so a crash never leaves the snapshot ahead of the database. Each
//! write names only the columns it changes, so an older snapshot cannot undo
//! what another one wrote.

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
        expect(dead_code, reason = "the Saving send of Task 6 uses it")
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
    pub(crate) fn from_rows(
        coins: HashMap<String, Label>,
        sent: HashMap<String, SentRecord>,
    ) -> Self {
        Labels { coins, sent }
    }

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
        expect(dead_code, reason = "the Saving send of Task 6 uses it")
    )]
    pub(crate) fn set_tag(
        &mut self,
        store: &Store,
        outpoint: &str,
        tag: Tag,
        source_federation: Option<String>,
    ) -> Result<()> {
        store.set_saving_tag(outpoint, tag, source_federation.as_deref())?;
        let label = self.coins.entry(outpoint.to_string()).or_default();
        label.tag = Some(tag);
        label.source_federation = source_federation;
        Ok(())
    }

    pub(crate) fn set_frozen(&mut self, store: &Store, outpoint: &str, frozen: bool) -> Result<()> {
        store.set_saving_frozen(outpoint, frozen)?;
        self.coins.entry(outpoint.to_string()).or_default().frozen = frozen;
        Ok(())
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the Saving send of Task 6 uses it")
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes change only what they name, so an older snapshot undoes nothing.
    #[test]
    fn a_stale_snapshot_keeps_what_others_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("fedimix.db")).unwrap();
        let outpoint = format!("{:064x}:0", 1);
        let mut stale = Labels::load(&store).unwrap();
        let mut other = Labels::load(&store).unwrap();
        other.set_frozen(&store, &outpoint, true).unwrap();
        stale
            .set_tag(&store, &outpoint, Tag::SmallChange, Some("fed".into()))
            .unwrap();
        let read = Labels::load(&store).unwrap();
        assert!(read.is_frozen(&outpoint), "the tag undid the freeze");
        assert_eq!(read.tag(&outpoint), Tag::SmallChange);

        // And the other way round.
        let mut stale = Labels::load(&store).unwrap();
        other
            .set_tag(&store, &outpoint, Tag::Anonymity { estimate: 9 }, None)
            .unwrap();
        stale.set_frozen(&store, &outpoint, false).unwrap();
        let read = Labels::load(&store).unwrap();
        assert_eq!(read.tag(&outpoint), Tag::Anonymity { estimate: 9 });
        assert!(!read.is_frozen(&outpoint));
    }
}
