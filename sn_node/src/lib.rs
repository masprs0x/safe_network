// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

//! Implementation of the Node in SAFE Network.

// For quick_error
#![recursion_limit = "256"]
#![doc(
    html_logo_url = "https://github.com/maidsafe/QA/raw/master/Images/maidsafe_logo.png",
    html_favicon_url = "https://maidsafe.net/img/favicon.ico",
    test(attr(deny(warnings)))
)]
// Forbid some very bad patterns. Forbid is stronger than `deny`, preventing us from suppressing the
// lint with `#[allow(...)]` et-all.
#![forbid(
    arithmetic_overflow,
    mutable_transmutes,
    no_mangle_const_items,
    unknown_crate_types,
    unsafe_code
)]
// Turn on some additional warnings to encourage good style.
#![warn(
    missing_docs,
    trivial_casts,
    trivial_numeric_casts,
    unreachable_pub,
    unused_extern_crates,
    unused_import_braces,
    unused_qualifications,
    unused_results,
    clippy::unicode_not_nfc,
    clippy::unwrap_used,
    clippy::unused_async
)]

#[macro_use]
extern crate tracing;

mod error;
mod event;
mod log_markers;
#[cfg(feature = "open-metrics")]
mod metrics;
mod node;
mod put_validation;
mod quote;
mod replication;
mod spends;

pub use self::{
    event::{NodeEvent, NodeEventsChannel, NodeEventsReceiver},
    log_markers::Marker,
    node::{NodeBuilder, NodeCmd, ROYALTY_TRANSFER_NOTIF_TOPIC},
};

use crate::error::{Error, Result};
use bls::PublicKey;
use bytes::Bytes;
use libp2p::PeerId;
use sn_networking::{Network, SwarmLocalState};
use sn_protocol::NetworkAddress;
use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
};
use tokio::sync::broadcast;

/// Once a node is started and running, the user obtains
/// a `NodeRunning` object which can be used to interact with it.
#[derive(Clone)]
pub struct RunningNode {
    network: Network,
    node_events_channel: NodeEventsChannel,
    node_cmds: broadcast::Sender<NodeCmd>,
}

impl RunningNode {
    /// Returns this node's `PeerId`
    pub fn peer_id(&self) -> PeerId {
        self.network.peer_id
    }

    /// Returns the root directory path for the node.
    ///
    /// This will either be a value defined by the user, or a default location, plus the peer ID
    /// appended. The default location is platform specific:
    ///  - Linux: $HOME/.local/share/safe/node/<peer-id>
    ///  - macOS: $HOME/Library/Application Support/safe/node/<peer-id>
    ///  - Windows: C:\Users\<username>\AppData\Roaming\safe\node\<peer-id>
    #[allow(rustdoc::invalid_html_tags)]
    pub fn root_dir_path(&self) -> PathBuf {
        self.network.root_dir_path.clone()
    }

    /// Returns a `SwarmLocalState` with some information obtained from swarm's local state.
    pub async fn get_swarm_local_state(&self) -> Result<SwarmLocalState> {
        let state = self.network.get_swarm_local_state().await?;
        Ok(state)
    }

    /// Returns the node events channel where to subscribe to receive `NodeEvent`s
    pub fn node_events_channel(&self) -> &NodeEventsChannel {
        &self.node_events_channel
    }

    /// Returns the list of all the RecordKeys held by the node
    pub async fn get_all_record_addresses(&self) -> Result<HashSet<NetworkAddress>> {
        #[allow(clippy::mutable_key_type)] // for Bytes in NetworkAddress
        let addresses: HashSet<_> = self
            .network
            .get_all_local_record_addresses()
            .await?
            .keys()
            .cloned()
            .collect();
        Ok(addresses)
    }

    /// Returns a map where each key is the ilog2 distance of that Kbucket and each value is a vector of peers in that
    /// bucket.
    pub async fn get_kbuckets(&self) -> Result<BTreeMap<u32, Vec<PeerId>>> {
        let kbuckets = self.network.get_kbuckets().await?;
        Ok(kbuckets)
    }

    /// Subscribe to given gossipsub topic
    pub fn subscribe_to_topic(&self, topic_id: String) -> Result<()> {
        self.network.subscribe_to_topic(topic_id)?;
        Ok(())
    }

    /// Starts handling gossipsub topics
    pub fn start_handle_gossip(&self) -> Result<()> {
        self.network.start_handle_gossip()?;
        Ok(())
    }

    /// Unsubscribe from given gossipsub topic
    pub fn unsubscribe_from_topic(&self, topic_id: String) -> Result<()> {
        self.network.unsubscribe_from_topic(topic_id)?;
        Ok(())
    }

    /// Publish a message on a given gossipsub topic
    pub fn publish_on_topic(&self, topic_id: String, msg: Bytes) -> Result<()> {
        self.network.publish_on_topic(topic_id, msg)?;
        Ok(())
    }

    /// Set a PublicKey to start decoding and accepting Transfer notifications received over gossipsub.
    /// All Transfer notifications are dropped/discarded if no public key is set.
    /// All Transfer notifications received for a key which don't match the set public key is also discarded.
    pub fn transfer_notifs_filter(&self, filter: Option<PublicKey>) -> Result<()> {
        let _ = self
            .node_cmds
            .send(NodeCmd::TransferNotifsFilter(filter))
            .map_err(|err| Error::NodeCmdFailed(err.to_string()))?;
        Ok(())
    }
}
