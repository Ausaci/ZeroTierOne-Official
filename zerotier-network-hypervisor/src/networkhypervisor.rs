/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 *
 * (c)2021 ZeroTier, Inc.
 * https://www.zerotier.com/
 */

use std::time::Duration;

use crate::error::InvalidParameterError;
use crate::vl1::protocol::PooledPacketBuffer;
use crate::vl1::{Address, Endpoint, Identity, Node, RootSet, SystemInterface};
use crate::vl2::{Switch, SwitchInterface};

pub trait Interface: SystemInterface + SwitchInterface {}

pub struct NetworkHypervisor<I: Interface> {
    vl1: Node<I>,
    vl2: Switch,
}

impl<I: Interface> NetworkHypervisor<I> {
    pub fn new(ii: &I, auto_generate_identity: bool) -> Result<Self, InvalidParameterError> {
        Ok(NetworkHypervisor {
            vl1: Node::new(ii, auto_generate_identity)?,
            vl2: Switch::new(),
        })
    }

    #[inline(always)]
    pub fn get_packet_buffer(&self) -> PooledPacketBuffer {
        self.vl1.get_packet_buffer()
    }

    #[inline(always)]
    pub fn address(&self) -> Address {
        self.vl1.identity.address
    }

    #[inline(always)]
    pub fn identity(&self) -> &Identity {
        &self.vl1.identity
    }

    #[inline(always)]
    pub fn do_background_tasks(&self, ii: &I) -> Duration {
        self.vl1.do_background_tasks(ii)
    }

    #[inline(always)]
    pub fn wire_receive(&self, ii: &I, source_endpoint: &Endpoint, source_local_socket: &I::LocalSocket, source_local_interface: &I::LocalInterface, data: PooledPacketBuffer) {
        self.vl1.wire_receive(ii, &self.vl2, source_endpoint, source_local_socket, source_local_interface, data)
    }

    #[inline(always)]
    pub fn add_update_root_set(&self, rs: RootSet) -> bool {
        self.vl1.add_update_root_set(rs)
    }
}
