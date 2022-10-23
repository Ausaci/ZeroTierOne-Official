use async_trait::async_trait;

use zerotier_network_hypervisor::vl1::{Address, InetAddress, NodeStorage};
use zerotier_network_hypervisor::vl2::NetworkId;

use zerotier_utils::tokio::sync::broadcast::Receiver;

use crate::model::*;

/// Database change relevant to the controller and that was NOT initiated by the controller.
#[derive(Clone)]
pub enum Change {
    NetworkDeleted(NetworkId),
    MemberAuthorized(NetworkId, Address),
    MemberDeauthorized(NetworkId, Address),
}

#[async_trait]
pub trait Database: Sync + Send + NodeStorage + 'static {
    type Error: std::error::Error + Send + 'static;

    async fn get_network(&self, id: NetworkId) -> Result<Option<Network>, Self::Error>;
    async fn save_network(&self, obj: Network) -> Result<(), Self::Error>;

    async fn list_members(&self, network_id: NetworkId) -> Result<Vec<Address>, Self::Error>;
    async fn get_member(&self, network_id: NetworkId, node_id: Address) -> Result<Option<Member>, Self::Error>;
    async fn save_member(&self, obj: Member) -> Result<(), Self::Error>;

    /// Get a receiver that can be used to receive changes made to networks and members, if supported.
    ///
    /// The receiver returned is a broadcast receiver. This can be called more than once if there are
    /// multiple parts of the controller that listen.
    ///
    /// Changes should NOT be broadcast on call to save_network() or save_member(). They should only
    /// be broadcast when externally generated changes occur.
    ///
    /// The default implementation returns None indicating that change following is not supported.
    /// Change following is required for instant deauthorization with revocations and other instant
    /// changes in response to modifications to network and member configuration.
    async fn changes(&self) -> Option<Receiver<Change>> {
        None
    }

    /// List members deauthorized after a given time (milliseconds since epoch).
    ///
    /// The default trait implementation uses a brute force method. This should be reimplemented if a
    /// more efficient way is available.
    async fn list_members_deauthorized_after(&self, network_id: NetworkId, cutoff: i64) -> Result<Vec<Address>, Self::Error> {
        let mut v = Vec::new();
        let members = self.list_members(network_id).await?;
        for a in members.iter() {
            if let Some(m) = self.get_member(network_id, *a).await? {
                if m.last_deauthorized_time.unwrap_or(i64::MIN) >= cutoff {
                    v.push(m.node_id);
                }
            }
        }
        Ok(v)
    }

    /// Check if any member of a network has a given static IP assignment.
    ///
    /// The default trait implementation uses a brute force method. This should be reimplemented if a
    /// more efficient way is available.
    async fn is_ip_assigned(&self, network_id: NetworkId, ip: &InetAddress) -> Result<bool, Self::Error> {
        let members = self.list_members(network_id).await?;
        for a in members.iter() {
            if let Some(m) = self.get_member(network_id, *a).await? {
                if m.ip_assignments.iter().any(|ip2| ip2.ip_bytes().eq(ip.ip_bytes())) {
                    return Ok(true);
                }
            }
        }
        return Ok(false);
    }

    async fn log_request(&self, obj: RequestLogItem) -> Result<(), Self::Error>;
}
