/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 *
 * (c)2021 ZeroTier, Inc.
 * https://www.zerotier.com/
 */

pub const VERSION_MAJOR: u8 = 1;
pub const VERSION_MINOR: u8 = 99;
pub const VERSION_REVISION: u8 = 1;

pub mod error;
pub mod util;
pub mod vl1;
pub mod vl2;

mod networkhypervisor;
pub use networkhypervisor::{Interface, NetworkHypervisor};

/// Standard packet buffer type including pool container.
pub type PacketBuffer = crate::util::pool::Pooled<crate::util::buffer::Buffer<{ crate::vl1::protocol::PACKET_SIZE_MAX }>, crate::PacketBufferFactory>;

/// Factory type to supply to a new PacketBufferPool.
pub type PacketBufferFactory = crate::util::buffer::PooledBufferFactory<{ crate::vl1::protocol::PACKET_SIZE_MAX }>;

/// Source for instances of PacketBuffer
pub type PacketBufferPool = crate::util::pool::Pool<crate::util::buffer::Buffer<{ crate::vl1::protocol::PACKET_SIZE_MAX }>, crate::PacketBufferFactory>;

/*
 * Protocol versions
 *
 * 1  - 0.2.0 ... 0.2.5
 * 2  - 0.3.0 ... 0.4.5
 *    + Added signature and originating peer to multicast frame
 *    + Double size of multicast frame bloom filter
 * 3  - 0.5.0 ... 0.6.0
 *    + Yet another multicast redesign
 *    + New crypto completely changes key agreement cipher
 * 4  - 0.6.0 ... 1.0.6
 *    + BREAKING CHANGE: New identity format based on hashcash design
 * 5  - 1.1.0 ... 1.1.5
 *    + Supports echo
 *    + Supports in-band world (root server definition) updates
 *    + Clustering! (Though this will work with protocol v4 clients.)
 *    + Otherwise backward compatible with protocol v4
 * 6  - 1.1.5 ... 1.1.10
 *    + Network configuration format revisions including binary values
 * 7  - 1.1.10 ... 1.1.17
 *    + Introduce trusted paths for local SDN use
 * 8  - 1.1.17 ... 1.2.0
 *    + Multipart network configurations for large network configs
 *    + Tags and Capabilities
 *    + inline push of CertificateOfMembership deprecated
 * 9  - 1.2.0 ... 1.2.14
 * 10 - 1.4.0 ... 1.4.6
 *    + Contained early pre-alpha versions of multipath, which are deprecated
 * 11 - 1.6.0 ... 2.0.0
 *    + Supports and prefers AES-GMAC-SIV symmetric crypto, backported.
 *
 * 20 - 2.0.0 ... CURRENT
 *    + Forward secrecy with cryptographic ratchet! Finally!!!
 *    + New identity format including both x25519 and NIST P-521 keys.
 *    + AES-GMAC-SIV, a FIPS-compliant SIV construction using AES.
 *    + HELLO and OK(HELLO) include an extra HMAC to harden authentication
 *    + HELLO and OK(HELLO) use a dictionary for better extensibilit.
 */
pub const VERSION_PROTO: u8 = 20;
