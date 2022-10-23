// (c) 2020-2022 ZeroTier, Inc. -- currently propritery pending actual release and licensing. See LICENSE.md.

use std::sync::Arc;

use crate::protocol::PacketBuffer;
use crate::vl1::node::{HostSystem, InnerProtocol, Node, PacketHandlerResult};
use crate::vl1::{Identity, Path, Peer};

pub trait SwitchInterface: Sync + Send {}

pub struct Switch {}

impl InnerProtocol for Switch {
    fn handle_packet<HostSystemImpl: HostSystem + ?Sized>(
        &self,
        host_system: &HostSystemImpl,
        node: &Node,
        source: &Arc<Peer>,
        source_path: &Arc<Path>,
        source_hops: u8,
        message_id: u64,
        verb: u8,
        payload: &PacketBuffer,
    ) -> PacketHandlerResult {
        PacketHandlerResult::NotHandled
    }

    fn handle_error<HostSystemImpl: HostSystem + ?Sized>(
        &self,
        host_system: &HostSystemImpl,
        node: &Node,
        source: &Arc<Peer>,
        source_path: &Arc<Path>,
        source_hops: u8,
        message_id: u64,
        in_re_verb: u8,
        in_re_message_id: u64,
        error_code: u8,
        payload: &PacketBuffer,
        cursor: &mut usize,
    ) -> PacketHandlerResult {
        PacketHandlerResult::NotHandled
    }

    fn handle_ok<HostSystemImpl: HostSystem + ?Sized>(
        &self,
        host_system: &HostSystemImpl,
        node: &Node,
        source: &Arc<Peer>,
        source_path: &Arc<Path>,
        source_hops: u8,
        message_id: u64,
        in_re_verb: u8,
        in_re_message_id: u64,
        payload: &PacketBuffer,
        cursor: &mut usize,
    ) -> PacketHandlerResult {
        PacketHandlerResult::NotHandled
    }

    fn should_respond_to(&self, id: &Identity) -> bool {
        true
    }
}

impl Switch {}
