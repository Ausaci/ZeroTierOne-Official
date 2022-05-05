/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 *
 * (c)2021 ZeroTier, Inc.
 * https://www.zerotier.com/
 */

use std::convert::TryInto;
use std::mem::MaybeUninit;
use std::num::NonZeroI64;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

use zerotier_core_crypto::aes_gmac_siv::AesCtr;
use zerotier_core_crypto::hash::*;
use zerotier_core_crypto::poly1305::Poly1305;
use zerotier_core_crypto::random::{get_bytes_secure, next_u64_secure};
use zerotier_core_crypto::salsa::Salsa;
use zerotier_core_crypto::secret::Secret;

use crate::util::buffer::Buffer;
use crate::util::byte_array_range;
use crate::util::marshalable::Marshalable;
use crate::vl1::identity::{IDENTITY_ALGORITHM_ALL, IDENTITY_ALGORITHM_X25519};
use crate::vl1::node::*;
use crate::vl1::protocol::*;
use crate::vl1::symmetricsecret::{EphemeralSymmetricSecret, SymmetricSecret};
use crate::vl1::{Dictionary, Endpoint, Identity, Path};
use crate::{PacketBuffer, VERSION_MAJOR, VERSION_MINOR, VERSION_PROTO, VERSION_REVISION};

/// A remote peer known to this node.
/// Sending-related and receiving-related fields are locked separately since concurrent
/// send/receive is not uncommon.
pub struct Peer {
    // This peer's identity.
    pub(crate) identity: Identity,

    // Static shared secret computed from agreement with identity.
    identity_symmetric_key: SymmetricSecret,

    // Latest ephemeral secret or None if not yet negotiated.
    ephemeral_symmetric_key: Mutex<Option<Arc<EphemeralSymmetricSecret>>>,

    // Paths sorted in descending order of quality / preference.
    paths: Mutex<Vec<Arc<Path>>>,

    // Statistics and times of events.
    create_time_ticks: i64,
    pub(crate) last_send_time_ticks: AtomicI64,
    pub(crate) last_receive_time_ticks: AtomicI64,
    pub(crate) last_hello_reply_time_ticks: AtomicI64,
    last_forward_time_ticks: AtomicI64,
    total_bytes_sent: AtomicU64,
    total_bytes_sent_indirect: AtomicU64,
    total_bytes_received: AtomicU64,
    total_bytes_received_indirect: AtomicU64,
    total_bytes_forwarded: AtomicU64,

    // Counter for assigning sequential message IDs.
    message_id_counter: AtomicU64,

    // Remote peer version information.
    remote_version: AtomicU64,
    remote_protocol_version: AtomicU8,
}

/// Derive per-packet key for Sals20/12 encryption (and Poly1305 authentication).
///
/// This effectively adds a few additional bits of entropy to the IV from packet
/// characteristics such as its size and direction of communication. It also
/// effectively incorporates header information as AAD, since if the header info
/// is different the key will be wrong and MAC will fail.
///
/// This is only used for Salsa/Poly modes.
fn salsa_derive_per_packet_key(key: &Secret<64>, header: &PacketHeader, packet_size: usize) -> Secret<64> {
    let hb = header.as_bytes();
    let mut k = key.clone();
    for i in 0..18 {
        k.0[i] ^= hb[i];
    }
    k.0[18] ^= hb[HEADER_FLAGS_FIELD_INDEX] & HEADER_FLAGS_FIELD_MASK_HIDE_HOPS;
    k.0[19] ^= (packet_size >> 8) as u8;
    k.0[20] ^= packet_size as u8;
    k
}

/// Create initialized instances of Salsa20/12 and Poly1305 for a packet.
fn salsa_poly_create(secret: &SymmetricSecret, header: &PacketHeader, packet_size: usize) -> (Salsa<12>, Poly1305) {
    let key = salsa_derive_per_packet_key(&secret.key, header, packet_size);
    let mut salsa = Salsa::<12>::new(&key.0[0..32], &header.id);
    let mut poly1305_key = [0_u8; 32];
    salsa.crypt_in_place(&mut poly1305_key);
    (salsa, Poly1305::new(&poly1305_key).unwrap())
}

/// Attempt AEAD packet encryption and MAC validation. Returns message ID on success.
fn try_aead_decrypt(secret: &SymmetricSecret, packet_frag0_payload_bytes: &[u8], header: &PacketHeader, fragments: &[Option<PacketBuffer>], payload: &mut Buffer<PACKET_SIZE_MAX>) -> Option<u64> {
    packet_frag0_payload_bytes.get(0).map_or(None, |verb| {
        match header.cipher() {
            CIPHER_NOCRYPT_POLY1305 => {
                if (verb & VERB_MASK) == VERB_VL1_HELLO {
                    let mut total_packet_len = packet_frag0_payload_bytes.len() + PACKET_HEADER_SIZE;
                    for f in fragments.iter() {
                        total_packet_len += f.as_ref().map_or(0, |f| f.len());
                    }
                    let _ = payload.append_bytes(packet_frag0_payload_bytes);
                    for f in fragments.iter() {
                        let _ = f.as_ref().map(|f| f.as_bytes_starting_at(FRAGMENT_HEADER_SIZE).map(|f| payload.append_bytes(f)));
                    }
                    let (_, mut poly) = salsa_poly_create(secret, header, total_packet_len);
                    poly.update(payload.as_bytes());
                    if poly.finish()[0..8].eq(&header.mac) {
                        Some(u64::from_ne_bytes(header.id))
                    } else {
                        None
                    }
                } else {
                    // Only HELLO is permitted without payload encryption. Drop other packet types if sent this way.
                    None
                }
            }

            CIPHER_SALSA2012_POLY1305 => {
                let mut total_packet_len = packet_frag0_payload_bytes.len() + PACKET_HEADER_SIZE;
                for f in fragments.iter() {
                    total_packet_len += f.as_ref().map_or(0, |f| f.len());
                }
                let (mut salsa, mut poly) = salsa_poly_create(secret, header, total_packet_len);
                poly.update(packet_frag0_payload_bytes);
                let _ = payload.append_bytes_get_mut(packet_frag0_payload_bytes.len()).map(|b| salsa.crypt(packet_frag0_payload_bytes, b));
                for f in fragments.iter() {
                    let _ = f.as_ref().map(|f| {
                        f.as_bytes_starting_at(FRAGMENT_HEADER_SIZE).map(|f| {
                            poly.update(f);
                            let _ = payload.append_bytes_get_mut(f.len()).map(|b| salsa.crypt(f, b));
                        })
                    });
                }
                if poly.finish()[0..8].eq(&header.mac) {
                    Some(u64::from_ne_bytes(header.id))
                } else {
                    None
                }
            }

            CIPHER_AES_GMAC_SIV => {
                let mut aes = secret.aes_gmac_siv.get();
                aes.decrypt_init(&header.aes_gmac_siv_tag());
                aes.decrypt_set_aad(&header.aad_bytes());
                // NOTE: if there are somehow missing fragments this part will silently fail,
                // but the packet will fail MAC check in decrypt_finish() so meh.
                let _ = payload.append_bytes_get_mut(packet_frag0_payload_bytes.len()).map(|b| aes.decrypt(packet_frag0_payload_bytes, b));
                for f in fragments.iter() {
                    f.as_ref().map(|f| {
                        f.as_bytes_starting_at(FRAGMENT_HEADER_SIZE).map(|f| {
                            let _ = payload.append_bytes_get_mut(f.len()).map(|b| aes.decrypt(f, b));
                        })
                    });
                }
                aes.decrypt_finish().map_or(None, |tag| {
                    // AES-GMAC-SIV encrypts the packet ID too as part of its computation of a single
                    // opaque 128-bit tag, so to get the original packet ID we have to grab it from the
                    // decrypted tag.
                    Some(u64::from_ne_bytes(*byte_array_range::<16, 0, 8>(tag)))
                })
            }

            _ => None,
        }
    })
}

impl Peer {
    /// Create a new peer.
    ///
    /// This only returns None if this_node_identity does not have its secrets or if some
    /// fatal error occurs performing key agreement between the two identities.
    pub(crate) fn new(this_node_identity: &Identity, id: Identity, time_ticks: i64) -> Option<Peer> {
        this_node_identity.agree(&id).map(|static_secret| -> Peer {
            Peer {
                identity: id,
                identity_symmetric_key: SymmetricSecret::new(static_secret),
                ephemeral_symmetric_key: Mutex::new(None),
                paths: Mutex::new(Vec::new()),
                create_time_ticks: time_ticks,
                last_send_time_ticks: AtomicI64::new(0),
                last_receive_time_ticks: AtomicI64::new(0),
                last_hello_reply_time_ticks: AtomicI64::new(0),
                last_forward_time_ticks: AtomicI64::new(0),
                total_bytes_sent: AtomicU64::new(0),
                total_bytes_sent_indirect: AtomicU64::new(0),
                total_bytes_received: AtomicU64::new(0),
                total_bytes_received_indirect: AtomicU64::new(0),
                total_bytes_forwarded: AtomicU64::new(0),
                message_id_counter: AtomicU64::new(next_u64_secure()),
                remote_version: AtomicU64::new(0),
                remote_protocol_version: AtomicU8::new(0),
            }
        })
    }

    /// Get the next message ID for sending a message to this peer.
    #[inline(always)]
    pub(crate) fn next_message_id(&self) -> u64 {
        self.message_id_counter.fetch_add(1, Ordering::Relaxed)
    }

    /// Receive, decrypt, authenticate, and process an incoming packet from this peer.
    ///
    /// If the packet comes in multiple fragments, the fragments slice should contain all
    /// those fragments after the main packet header and first chunk.
    pub(crate) fn receive<SI: SystemInterface, VI: InnerProtocolInterface>(&self, node: &Node, si: &SI, vi: &VI, time_ticks: i64, source_path: &Arc<Path>, header: &PacketHeader, frag0: &Buffer<{ PACKET_SIZE_MAX }>, fragments: &[Option<PacketBuffer>]) {
        let _ = frag0.as_bytes_starting_at(PACKET_VERB_INDEX).map(|packet_frag0_payload_bytes| {
            let mut payload: Buffer<PACKET_SIZE_MAX> = unsafe { Buffer::new_without_memzero() };

            let (forward_secrecy, mut message_id) = if let Some(ephemeral_secret) = self.ephemeral_symmetric_key.lock().clone() {
                if let Some(message_id) = try_aead_decrypt(&ephemeral_secret.secret, packet_frag0_payload_bytes, header, fragments, &mut payload) {
                    // Decryption successful with ephemeral secret
                    ephemeral_secret.decrypt_uses.fetch_add(1, Ordering::Relaxed);
                    (true, message_id)
                } else {
                    // Decryption failed with ephemeral secret, which may indicate that it's obsolete.
                    (false, 0)
                }
            } else {
                // There is no ephemeral secret negotiated (yet?).
                (false, 0)
            };
            if !forward_secrecy {
                if let Some(message_id2) = try_aead_decrypt(&self.identity_symmetric_key, packet_frag0_payload_bytes, header, fragments, &mut payload) {
                    // Decryption successful with static secret.
                    message_id = message_id2;
                } else {
                    // Packet failed to decrypt using either ephemeral or permament key, reject.
                    return;
                }
            }
            debug_assert!(!payload.is_empty());

            // ---------------------------------------------------------------
            // If we made it here it decrypted and passed authentication.
            // ---------------------------------------------------------------

            self.last_receive_time_ticks.store(time_ticks, Ordering::Relaxed);
            self.total_bytes_received.fetch_add((payload.len() + PACKET_HEADER_SIZE) as u64, Ordering::Relaxed);

            let mut verb = payload.as_bytes()[0];

            // If this flag is set, the end of the payload is a full HMAC-SHA384 authentication
            // tag for much stronger authentication than is offered by the packet MAC.
            let extended_authentication = (verb & VERB_FLAG_EXTENDED_AUTHENTICATION) != 0;
            if extended_authentication {
                if payload.len() >= (1 + SHA384_HASH_SIZE) {
                    let actual_end_of_payload = payload.len() - SHA384_HASH_SIZE;
                    //let hmac = hmac_sha384(self.static_secret.packet_hmac_key.as_ref(), &[u64_as_bytes(&message_id), payload.as_bytes()]);
                    //if !hmac.eq(&(payload.as_bytes()[actual_end_of_payload..])) {
                    //    return;
                    //}
                    payload.set_size(actual_end_of_payload);
                } else {
                    return;
                }
            }

            if (verb & VERB_FLAG_COMPRESSED) != 0 {
                let mut decompressed_payload: [u8; PACKET_SIZE_MAX] = unsafe { MaybeUninit::uninit().assume_init() };
                decompressed_payload[0] = verb;
                let dlen = lz4_flex::block::decompress_into(&payload.as_bytes()[1..], &mut decompressed_payload[1..]);
                if dlen.is_ok() {
                    payload.set_to(&decompressed_payload[0..(dlen.unwrap() + 1)]);
                } else {
                    return;
                }
            }

            // For performance reasons we let VL2 handle packets first. It returns false
            // if it didn't handle the packet, in which case it's handled at VL1. This is
            // because the most performance critical path is the handling of the ???_FRAME
            // verbs, which are in VL2.
            verb &= VERB_MASK; // mask off flags
            if !vi.handle_packet(self, source_path, forward_secrecy, extended_authentication, verb, &payload) {
                match verb {
                    //VERB_VL1_NOP => {}
                    VERB_VL1_HELLO => self.receive_hello(si, node, time_ticks, source_path, &payload),
                    VERB_VL1_ERROR => self.receive_error(si, vi, node, time_ticks, source_path, forward_secrecy, extended_authentication, &payload),
                    VERB_VL1_OK => self.receive_ok(si, vi, node, time_ticks, source_path, forward_secrecy, extended_authentication, &payload),
                    VERB_VL1_WHOIS => self.receive_whois(si, node, time_ticks, source_path, &payload),
                    VERB_VL1_RENDEZVOUS => self.receive_rendezvous(si, node, time_ticks, source_path, &payload),
                    VERB_VL1_ECHO => self.receive_echo(si, node, time_ticks, source_path, &payload),
                    VERB_VL1_PUSH_DIRECT_PATHS => self.receive_push_direct_paths(si, node, time_ticks, source_path, &payload),
                    VERB_VL1_USER_MESSAGE => self.receive_user_message(si, node, time_ticks, source_path, &payload),
                    _ => {}
                }
            }
        });
    }

    fn send_to_endpoint<SI: SystemInterface>(&self, si: &SI, endpoint: &Endpoint, local_socket: Option<NonZeroI64>, local_interface: Option<NonZeroI64>, packet: &Buffer<{ PACKET_SIZE_MAX }>) -> bool {
        debug_assert!(packet.len() <= PACKET_SIZE_MAX);
        debug_assert!(packet.len() >= PACKET_SIZE_MIN);
        match endpoint {
            Endpoint::Ip(_) | Endpoint::IpUdp(_) | Endpoint::Ethernet(_) | Endpoint::Bluetooth(_) | Endpoint::WifiDirect(_) => {
                let packet_size = packet.len();
                if packet_size > UDP_DEFAULT_MTU {
                    let bytes = packet.as_bytes();
                    if !si.wire_send(endpoint, local_socket, local_interface, &[&bytes[0..UDP_DEFAULT_MTU]], 0) {
                        return false;
                    }

                    let mut pos = UDP_DEFAULT_MTU;

                    let overrun_size = (packet_size - UDP_DEFAULT_MTU) as u32;
                    let fragment_count = (overrun_size / (UDP_DEFAULT_MTU - FRAGMENT_HEADER_SIZE) as u32) + (((overrun_size % (UDP_DEFAULT_MTU - FRAGMENT_HEADER_SIZE) as u32) != 0) as u32);
                    debug_assert!(fragment_count <= PACKET_FRAGMENT_COUNT_MAX as u32);

                    let mut header = FragmentHeader {
                        id: unsafe { *packet.as_bytes().as_ptr().cast::<[u8; 8]>() },
                        dest: bytes[PACKET_DESTINATION_INDEX..PACKET_DESTINATION_INDEX + ADDRESS_SIZE].try_into().unwrap(),
                        fragment_indicator: PACKET_FRAGMENT_INDICATOR,
                        total_and_fragment_no: ((fragment_count + 1) << 4) as u8,
                        reserved_hops: 0,
                    };

                    let mut chunk_size = (packet_size - pos).min(UDP_DEFAULT_MTU - FRAGMENT_HEADER_SIZE);
                    loop {
                        header.total_and_fragment_no += 1;
                        let next_pos = pos + chunk_size;
                        if !si.wire_send(endpoint, local_socket, local_interface, &[header.as_bytes(), &bytes[pos..next_pos]], 0) {
                            return false;
                        }
                        pos = next_pos;
                        if pos < packet_size {
                            chunk_size = (packet_size - pos).min(UDP_DEFAULT_MTU - FRAGMENT_HEADER_SIZE);
                        } else {
                            return true;
                        }
                    }
                } else {
                    return si.wire_send(endpoint, local_socket, local_interface, &[packet.as_bytes()], 0);
                }
            }
            _ => {
                return si.wire_send(endpoint, local_socket, local_interface, &[packet.as_bytes()], 0);
            }
        }
    }

    /// Send a packet to this peer.
    ///
    /// This will go directly if there is an active path, or otherwise indirectly
    /// via a root or some other route.
    pub(crate) fn send<SI: SystemInterface>(&self, si: &SI, node: &Node, time_ticks: i64, packet: &Buffer<{ PACKET_SIZE_MAX }>) -> bool {
        self.path(node).map_or(false, |path| {
            if self.send_to_endpoint(si, &path.endpoint, path.local_socket, path.local_interface, packet) {
                self.last_send_time_ticks.store(time_ticks, Ordering::Relaxed);
                self.total_bytes_sent.fetch_add(packet.len() as u64, Ordering::Relaxed);
                true
            } else {
                false
            }
        })
    }

    /// Forward a packet to this peer.
    ///
    /// This is called when we receive a packet not addressed to this node and
    /// want to pass it along.
    ///
    /// This doesn't fragment large packets since fragments are forwarded individually.
    /// Intermediates don't need to adjust fragmentation.
    pub(crate) fn forward<SI: SystemInterface>(&self, si: &SI, time_ticks: i64, packet: &Buffer<{ PACKET_SIZE_MAX }>) -> bool {
        self.direct_path().map_or(false, |path| {
            if si.wire_send(&path.endpoint, path.local_socket, path.local_interface, &[packet.as_bytes()], 0) {
                self.last_forward_time_ticks.store(time_ticks, Ordering::Relaxed);
                self.total_bytes_forwarded.fetch_add(packet.len() as u64, Ordering::Relaxed);
                true
            } else {
                false
            }
        })
    }

    /// Send a HELLO to this peer.
    ///
    /// If explicit_endpoint is not None the packet will be sent directly to this endpoint.
    /// Otherwise it will be sent via the best direct or indirect path known.
    ///
    /// Unlike other messages HELLO is sent partially in the clear and always with the long-lived
    /// static identity key.
    pub(crate) fn send_hello<SI: SystemInterface>(&self, si: &SI, node: &Node, explicit_endpoint: Option<&Endpoint>) -> bool {
        let mut path = None;
        let destination = explicit_endpoint.map_or_else(
            || {
                self.path(node).map_or(None, |p| {
                    let _ = path.insert(p.clone());
                    Some(p.endpoint.clone())
                })
            },
            |endpoint| Some(endpoint.clone()),
        );
        if destination.is_none() {
            return false;
        }
        let destination = destination.unwrap();

        let mut packet: Buffer<PACKET_SIZE_MAX> = unsafe { Buffer::new_without_memzero() };
        let time_ticks = si.time_ticks();
        let message_id = self.next_message_id();

        {
            let packet_header: &mut PacketHeader = packet.append_struct_get_mut().unwrap();
            packet_header.id = message_id.to_ne_bytes(); // packet ID and message ID are the same when Poly1305 MAC is used
            packet_header.dest = self.identity.address.to_bytes();
            packet_header.src = node.identity.address.to_bytes();
            packet_header.flags_cipher_hops = CIPHER_NOCRYPT_POLY1305;
        }

        {
            let hello_fixed_headers: &mut message_component_structs::HelloFixedHeaderFields = packet.append_struct_get_mut().unwrap();
            hello_fixed_headers.verb = VERB_VL1_HELLO | VERB_FLAG_EXTENDED_AUTHENTICATION;
            hello_fixed_headers.version_proto = VERSION_PROTO;
            hello_fixed_headers.version_major = VERSION_MAJOR;
            hello_fixed_headers.version_minor = VERSION_MINOR;
            hello_fixed_headers.version_revision = (VERSION_REVISION as u16).to_be_bytes();
            hello_fixed_headers.timestamp = (time_ticks as u64).to_be_bytes();
        }

        assert!(self.identity.marshal_with_options(&mut packet, IDENTITY_ALGORITHM_ALL, false).is_ok());
        if self.identity.algorithms() == IDENTITY_ALGORITHM_X25519 {
            // LEGACY: append an extra zero when marshaling identities containing only x25519 keys.
            // See comments in Identity::marshal(). This can go away eventually.
            assert!(packet.append_u8(0).is_ok());
        }

        // 8 reserved bytes, must be zero for legacy compatibility.
        assert!(packet.append_padding(0, 8).is_ok());

        // Generate a 12-byte nonce for the private section of HELLO.
        let mut nonce = get_bytes_secure::<12>();

        // LEGACY: create a 16-bit encrypted field that specifies zero "moons." This is ignored now
        // but causes old nodes to be able to parse this packet properly. Newer nodes will treat this
        // as part of a 12-byte nonce and otherwise ignore it. These bytes will be random.
        let mut salsa_iv = message_id.to_ne_bytes();
        salsa_iv[7] &= 0xf8;
        Salsa::<12>::new(&self.identity_symmetric_key.key.0[0..32], &salsa_iv).crypt(&[0_u8, 0_u8], &mut nonce[8..10]);

        // Append 12-byte AES-CTR nonce.
        assert!(packet.append_bytes_fixed(&nonce).is_ok());

        // Add encrypted private field map. Plain AES-CTR is used with no MAC or SIV because
        // the whole packet is authenticated with HMAC-SHA512.
        let mut fields = Dictionary::new();
        fields.set_u64(SESSION_METADATA_INSTANCE_ID, node.instance_id);
        fields.set_u64(SESSION_METADATA_CLOCK, si.time_clock() as u64);
        fields.set_bytes(SESSION_METADATA_SENT_TO, destination.to_buffer::<{ Endpoint::MAX_MARSHAL_SIZE }>().unwrap().as_bytes().to_vec());
        let fields = fields.to_bytes();
        assert!(fields.len() <= 0xffff); // sanity check, should be impossible
        assert!(packet.append_u16(fields.len() as u16).is_ok()); // prefix with unencrypted size
        let private_section_start = packet.len();
        assert!(packet.append_bytes(fields.as_slice()).is_ok());
        let mut aes = AesCtr::new(&self.identity_symmetric_key.hello_private_section_key.as_bytes()[0..32]);
        aes.init(&nonce);
        aes.crypt_in_place(&mut packet.as_mut()[private_section_start..]);
        drop(aes);
        drop(fields);

        // Add extended authentication at end of packet.
        let mut hmac = HMACSHA512::new(self.identity_symmetric_key.packet_hmac_key.as_bytes());
        hmac.update(&message_id.to_ne_bytes());
        hmac.update(&packet.as_bytes()[PACKET_HEADER_SIZE..]);
        assert!(packet.append_bytes_fixed(&hmac.finish()).is_ok());

        // Set legacy poly1305 MAC in packet header. Newer nodes also check HMAC-SHA512 but older ones only use this.
        let (_, mut poly) = salsa_poly_create(&self.identity_symmetric_key, packet.struct_at::<PacketHeader>(0).unwrap(), packet.len());
        poly.update(packet.as_bytes_starting_at(PACKET_HEADER_SIZE).unwrap());
        packet.as_mut()[HEADER_MAC_FIELD_INDEX..HEADER_MAC_FIELD_INDEX + 8].copy_from_slice(&poly.finish()[0..8]);

        self.last_send_time_ticks.store(time_ticks, Ordering::Relaxed);
        self.total_bytes_sent.fetch_add(packet.len() as u64, Ordering::Relaxed);

        path.map_or_else(
            || self.send_to_endpoint(si, &destination, None, None, &packet),
            |p| {
                if self.send_to_endpoint(si, &destination, p.local_socket, p.local_interface, &packet) {
                    p.log_send_anything(time_ticks);
                    true
                } else {
                    false
                }
            },
        )
    }

    #[inline(always)]
    fn receive_hello<SI: SystemInterface>(&self, si: &SI, node: &Node, time_ticks: i64, source_path: &Arc<Path>, payload: &Buffer<PACKET_SIZE_MAX>) {}

    #[inline(always)]
    fn receive_error<SI: SystemInterface, PH: InnerProtocolInterface>(&self, si: &SI, ph: &PH, node: &Node, time_ticks: i64, source_path: &Arc<Path>, forward_secrecy: bool, extended_authentication: bool, payload: &Buffer<PACKET_SIZE_MAX>) {
        let mut cursor: usize = 0;
        let _ = payload.read_struct::<message_component_structs::ErrorHeader>(&mut cursor).map(|error_header| {
            let in_re_message_id = u64::from_ne_bytes(error_header.in_re_message_id);
            let current_packet_id_counter = self.message_id_counter.load(Ordering::Relaxed);
            if current_packet_id_counter.wrapping_sub(in_re_message_id) <= PACKET_RESPONSE_COUNTER_DELTA_MAX {
                match error_header.in_re_verb {
                    _ => {
                        ph.handle_error(self, source_path, forward_secrecy, extended_authentication, error_header.in_re_verb, in_re_message_id, error_header.error_code, payload, &mut cursor);
                    }
                }
            }
        });
    }

    #[inline(always)]
    fn receive_ok<SI: SystemInterface, PH: InnerProtocolInterface>(&self, si: &SI, ph: &PH, node: &Node, time_ticks: i64, source_path: &Arc<Path>, forward_secrecy: bool, extended_authentication: bool, payload: &Buffer<PACKET_SIZE_MAX>) {
        let mut cursor: usize = 0;
        let _ = payload.read_struct::<message_component_structs::OkHeader>(&mut cursor).map(|ok_header| {
            let in_re_message_id = u64::from_ne_bytes(ok_header.in_re_message_id);
            let current_packet_id_counter = self.message_id_counter.load(Ordering::Relaxed);
            if current_packet_id_counter.wrapping_sub(in_re_message_id) <= PACKET_RESPONSE_COUNTER_DELTA_MAX {
                match ok_header.in_re_verb {
                    VERB_VL1_HELLO => {
                        // TODO
                        self.last_hello_reply_time_ticks.store(time_ticks, Ordering::Relaxed);
                    }
                    VERB_VL1_WHOIS => {}
                    _ => {
                        ph.handle_ok(self, source_path, forward_secrecy, extended_authentication, ok_header.in_re_verb, in_re_message_id, payload, &mut cursor);
                    }
                }
            }
        });
    }

    #[inline(always)]
    fn receive_whois<SI: SystemInterface>(&self, si: &SI, node: &Node, time_ticks: i64, source_path: &Arc<Path>, payload: &Buffer<{ PACKET_SIZE_MAX }>) {}

    #[inline(always)]
    fn receive_rendezvous<SI: SystemInterface>(&self, si: &SI, node: &Node, time_ticks: i64, source_path: &Arc<Path>, payload: &Buffer<{ PACKET_SIZE_MAX }>) {}

    #[inline(always)]
    fn receive_echo<SI: SystemInterface>(&self, si: &SI, node: &Node, time_ticks: i64, source_path: &Arc<Path>, payload: &Buffer<{ PACKET_SIZE_MAX }>) {}

    #[inline(always)]
    fn receive_push_direct_paths<SI: SystemInterface>(&self, si: &SI, node: &Node, time_ticks: i64, source_path: &Arc<Path>, payload: &Buffer<{ PACKET_SIZE_MAX }>) {}

    #[inline(always)]
    fn receive_user_message<SI: SystemInterface>(&self, si: &SI, node: &Node, time_ticks: i64, source_path: &Arc<Path>, payload: &Buffer<{ PACKET_SIZE_MAX }>) {}

    /// Get current best path or None if there are no direct paths to this peer.
    #[inline(always)]
    pub fn direct_path(&self) -> Option<Arc<Path>> {
        self.paths.lock().first().map(|p| p.clone())
    }

    /// Get either the current best direct path or an indirect path.
    pub fn path(&self, node: &Node) -> Option<Arc<Path>> {
        self.direct_path().map_or_else(|| node.root().map_or(None, |root| root.direct_path().map_or(None, |bp| Some(bp))), |bp| Some(bp))
    }

    /// Get the remote version of this peer: major, minor, revision, and build.
    /// Returns None if it's not yet known.
    pub fn version(&self) -> Option<[u16; 4]> {
        let rv = self.remote_version.load(Ordering::Relaxed);
        if rv != 0 {
            Some([(rv >> 48) as u16, (rv >> 32) as u16, (rv >> 16) as u16, rv as u16])
        } else {
            None
        }
    }

    /// Get the remote protocol version of this peer or None if not yet known.
    pub fn protocol_version(&self) -> Option<u8> {
        let pv = self.remote_protocol_version.load(Ordering::Relaxed);
        if pv != 0 {
            Some(pv)
        } else {
            None
        }
    }
}

impl BackgroundServicable for Peer {
    const SERVICE_INTERVAL_MS: i64 = EPHEMERAL_SECRET_REKEY_AFTER_TIME / 10;

    #[inline(always)]
    fn service<SI: SystemInterface>(&self, si: &SI, node: &Node, time_ticks: i64) -> bool {
        true
    }
}
