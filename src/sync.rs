// Copyright 2016 Mozilla
//
// Licensed under the Apache License, Version 2.0 (the "License"); you may not use
// this file except in compliance with the License. You may obtain a copy of the
// License at http://www.apache.org/licenses/LICENSE-2.0
// Unless required by applicable law or agreed to in writing, software distributed
// under the License is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR
// CONDITIONS OF ANY KIND, either express or implied. See the License for the
// specific language governing permissions and limitations under the License.

use uuid::Uuid;

use store::Store;
use errors::{
    Result,
};

use conn::{
    InProgress,
    Syncable as SyncableInProgress
};
use entity_builder::TermBuilder;
use mentat_core::{
    Entid,
    KnownEntid,
};
use mentat_db as db;

use entity_builder::BuildTerms;

use mentat_tolstoy::{
    Syncer,
    SyncMetadataClient,
    TxMapper,
    TolstoyError,
};
use mentat_tolstoy::syncer::{
    SyncResult,
};
use mentat_tolstoy::types::Tx;
use mentat_tolstoy::metadata::HeadTrackable;

pub trait Syncable {
    fn sync(&mut self, server_uri: &String, user_uuid: &String) -> Result<()>;
}

fn within_user_partition(entid: Entid) -> bool {
    entid >= db::USER0 && entid < db::TX0
}

impl<'a, 'c> SyncableInProgress for InProgress<'a, 'c> {
    fn flow(&mut self, server_uri: &String, user_uuid: &Uuid) -> Result<SyncResult> {
        Syncer::flow(&mut self.transaction, server_uri, user_uuid)
    }

    fn fast_forward_local(&mut self, txs: Vec<Tx>) -> Result<Entid> {
        let mut last_tx_entid = None;
        let mut last_tx_uuid = None;

        // During fast-forwarding, we will insert datoms with known entids
        // which, by definition, fall outside of our user partition.
        // Once we've done with insertion, we need to ensure that user
        // partition's next allocation will not overlap with just-inserted datoms.
        // To allow for "holes" in the user partition (due to data excision),
        // we track the highest incoming entid we saw, and expand our
        // local partition to match.
        // In absence of excision and implementation bugs, this should work
        // just as if we counted number of incoming entids and expanded by
        // that number instead.
        let mut largest_endid_encountered = db::USER0;

        for tx in txs {
            let mut builder = TermBuilder::new();
            for part in tx.parts {
                if part.added {
                    builder.add(KnownEntid(part.e), KnownEntid(part.a), part.v.clone())?;
                } else {
                    builder.retract(KnownEntid(part.e), KnownEntid(part.a), part.v.clone())?;
                }
                // Ignore datoms that fall outside of the user partition:
                if within_user_partition(part.e) && part.e > largest_endid_encountered {
                    largest_endid_encountered = part.e;
                }
            }
            let report = self.transact_builder(builder)?;
            last_tx_entid = Some(report.tx_id);
            last_tx_uuid = Some(tx.tx.clone());
        }

        // We've just transacted a new tx, and generated a new tx entid.
        // Map it to the corresponding incoming tx uuid, advance our
        // "locally known remote head".
        if let Some(uuid) = last_tx_uuid {
            if let Some(entid) = last_tx_entid {
                SyncMetadataClient::set_remote_head(&mut self.transaction, &uuid)?;
                TxMapper::set_tx_uuid(&mut self.transaction, entid, &uuid)?;
            }
        }

        Ok(largest_endid_encountered)
    }
}

impl Syncable for Store {
    fn sync(&mut self, server_uri: &String, user_uuid: &String) -> Result<()> {
        let uuid = Uuid::parse_str(&user_uuid)?;

        let mut largest_endid_encountered = None;
        {
            let mut in_progress = self.begin_transaction()?;
            let sync_result = in_progress.flow(server_uri, &uuid)?;

            match sync_result {
                SyncResult::Merge => bail!(TolstoyError::NotYetImplemented(
                    format!("Can't sync against diverged local.")
                )),
                SyncResult::LocalFastForward(txs) => {
                    largest_endid_encountered = Some(in_progress.fast_forward_local(txs)?);
                },
                SyncResult::BadServerState => bail!(TolstoyError::NotYetImplemented(
                    format!("Bad server state.")
                )),
                SyncResult::IncompatibleBootstrapSchema => bail!(TolstoyError::NotYetImplemented(
                    format!("IncompatibleBootstrapSchema.")
                )),
                _ => ()
            }

            // All of the work we've done while syncing is committed at this point.
            in_progress.commit()?;
        }

        // See https://github.com/mozilla/mentat/pull/494 for "renumbering" work which is a generalized take on this.

        // Only need to advance the user partition, since we're using KnownEntid and partition won't
        // get auto-updated; shouldn't be a problem for tx partition, since we're relying on the builder
        // to create a tx and advance the partition for us.
        match largest_endid_encountered {
            Some(v) => self.fast_forward_user_partition(v)?,
            None => ()
        }

        Ok(())
    }
}
