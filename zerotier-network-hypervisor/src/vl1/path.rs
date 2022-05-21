// (c) 2020-2022 ZeroTier, Inc. -- currently propritery pending actual release and licensing. See LICENSE.md.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};

use parking_lot::Mutex;

use crate::util::*;
use crate::vl1::endpoint::Endpoint;
use crate::vl1::fragmentedpacket::FragmentedPacket;
use crate::vl1::node::*;
use crate::vl1::protocol::*;

pub(crate) const SERVICE_INTERVAL_MS: i64 = PATH_KEEPALIVE_INTERVAL;

/// A remote endpoint paired with a local socket and a local interface.
/// These are maintained in Node and canonicalized so that all unique paths have
/// one and only one unique path object. That enables statistics to be tracked
/// for them and uniform application of things like keepalives.
pub struct Path<SI: SystemInterface> {
    pub endpoint: Endpoint,
    pub local_socket: SI::LocalSocket,
    pub local_interface: SI::LocalInterface,
    pub(crate) last_send_time_ticks: AtomicI64,
    pub(crate) last_receive_time_ticks: AtomicI64,
    pub(crate) create_time_ticks: i64,
    fragmented_packets: Mutex<HashMap<u64, FragmentedPacket, U64NoOpHasher>>,
}

impl<SI: SystemInterface> Path<SI> {
    pub fn new(endpoint: Endpoint, local_socket: SI::LocalSocket, local_interface: SI::LocalInterface, time_ticks: i64) -> Self {
        Self {
            endpoint,
            local_socket,
            local_interface,
            last_send_time_ticks: AtomicI64::new(0),
            last_receive_time_ticks: AtomicI64::new(0),
            create_time_ticks: time_ticks,
            fragmented_packets: Mutex::new(HashMap::with_capacity_and_hasher(4, U64NoOpHasher::new())),
        }
    }

    /// Receive a fragment and return a FragmentedPacket if the entire packet was assembled.
    /// This returns None if more fragments are needed to assemble the packet.
    pub(crate) fn receive_fragment(&self, packet_id: u64, fragment_no: u8, fragment_expecting_count: u8, packet: PooledPacketBuffer, time_ticks: i64) -> Option<FragmentedPacket> {
        let mut fp = self.fragmented_packets.lock();

        // Discard some old waiting packets if the total incoming fragments for a path exceeds a
        // sanity limit. This is to prevent memory exhaustion DOS attacks.
        let fps = fp.len();
        if fps > packet_constants::FRAGMENT_MAX_INBOUND_PACKETS_PER_PATH {
            let mut entries: Vec<(i64, u64)> = Vec::new();
            entries.reserve(fps);
            for f in fp.iter() {
                entries.push((f.1.ts_ticks, *f.0));
            }
            entries.sort_unstable_by(|a, b| (*a).0.cmp(&(*b).0));
            for i in 0..(fps / 3) {
                let _ = fp.remove(&(*entries.get(i).unwrap()).1);
            }
        }

        if fp.entry(packet_id).or_insert_with(|| FragmentedPacket::new(time_ticks)).add_fragment(packet, fragment_no, fragment_expecting_count) {
            fp.remove(&packet_id)
        } else {
            None
        }
    }

    #[inline(always)]
    pub(crate) fn log_receive_anything(&self, time_ticks: i64) {
        self.last_receive_time_ticks.store(time_ticks, Ordering::Relaxed);
    }

    #[inline(always)]
    pub(crate) fn log_send_anything(&self, time_ticks: i64) {
        self.last_send_time_ticks.store(time_ticks, Ordering::Relaxed);
    }

    pub(crate) fn service(&self, si: &SI, _: &Node<SI>, time_ticks: i64) -> bool {
        self.fragmented_packets.lock().retain(|_, frag| (time_ticks - frag.ts_ticks) < packet_constants::FRAGMENT_EXPIRATION);
        if (time_ticks - self.last_receive_time_ticks.load(Ordering::Relaxed)) < PATH_EXPIRATION_TIME {
            if (time_ticks - self.last_send_time_ticks.load(Ordering::Relaxed)) >= PATH_KEEPALIVE_INTERVAL {
                self.last_send_time_ticks.store(time_ticks, Ordering::Relaxed);
                si.wire_send(&self.endpoint, Some(&self.local_socket), Some(&self.local_interface), &[&ZEROES[..1]], 0);
            }
            true
        } else if (time_ticks - self.create_time_ticks) < PATH_EXPIRATION_TIME {
            true
        } else {
            false
        }
    }
}
