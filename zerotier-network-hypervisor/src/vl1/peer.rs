/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 *
 * (c)2021 ZeroTier, Inc.
 * https://www.zerotier.com/
 */

use std::convert::TryInto;
use std::intrinsics::try;
use std::mem::MaybeUninit;
use std::num::NonZeroI64;
use std::ptr::copy_nonoverlapping;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicU8, Ordering};

use parking_lot::Mutex;

use zerotier_core_crypto::aes_gmac_siv::{AesCtr, AesGmacSiv};
use zerotier_core_crypto::c25519::C25519KeyPair;
use zerotier_core_crypto::hash::{SHA384, SHA384_HASH_SIZE};
use zerotier_core_crypto::kbkdf::zt_kbkdf_hmac_sha384;
use zerotier_core_crypto::p521::P521KeyPair;
use zerotier_core_crypto::poly1305::Poly1305;
use zerotier_core_crypto::random::next_u64_secure;
use zerotier_core_crypto::salsa::Salsa;
use zerotier_core_crypto::secret::Secret;

use crate::{VERSION_MAJOR, VERSION_MINOR, VERSION_PROTO, VERSION_REVISION, PacketBuffer};
use crate::defaults::UDP_DEFAULT_MTU;
use crate::util::pool::{Pool, PoolFactory};
use crate::util::buffer::Buffer;
use crate::util::{array_range, u64_as_bytes};
use crate::vl1::{Dictionary, Endpoint, Identity, InetAddress, Path};
use crate::vl1::ephemeral::EphemeralSymmetricSecret;
use crate::vl1::node::*;
use crate::vl1::protocol::*;
use crate::vl1::symmetricsecret::SymmetricSecret;

/// Interval for servicing and background operations on peers.
pub(crate) const PEER_SERVICE_INTERVAL: i64 = 30000;

struct AesGmacSivPoolFactory(Secret<48>, Secret<48>);

impl PoolFactory<AesGmacSiv> for AesGmacSivPoolFactory {
    #[inline(always)]
    fn create(&self) -> AesGmacSiv { AesGmacSiv::new(&self.0.0[0..32], &self.1.0[0..32]) }

    #[inline(always)]
    fn reset(&self, obj: &mut AesGmacSiv) { obj.reset(); }
}

/// A secret key with all its derived forms and initialized ciphers.
struct PeerSecret {
    // Time secret was created in ticks for ephemeral secrets, or -1 for static secrets.
    create_time_ticks: i64,

    // Number of times secret has been used to encrypt something during this session.
    encrypt_count: AtomicU64,

    // Raw secret itself.
    secret: Secret<48>,

    // Reusable AES-GMAC-SIV ciphers initialized with secret.
    // These can't be used concurrently so they're pooled to allow low-contention concurrency.
    aes: Pool<AesGmacSiv, AesGmacSivPoolFactory>,
}

/// A remote peer known to this node.
/// Sending-related and receiving-related fields are locked separately since concurrent
/// send/receive is not uncommon.
pub struct Peer {
    // This peer's identity.
    identity: Identity,

    // Static shared secret computed from agreement with identity.
    static_secret: SymmetricSecret,

    // Derived static secret (in initialized cipher) used to encrypt the dictionary part of HELLO.
    static_secret_hello_dictionary: Mutex<AesCtr>,

    // Derived static secret used to add full HMAC-SHA384 to packets, currently just HELLO.
    static_secret_packet_hmac: Secret<48>,

    // Latest ephemeral secret acknowledged with OK(HELLO).
    ephemeral_secret: Mutex<Option<Arc<EphemeralSymmetricSecret>>>,

    // Paths sorted in descending order of quality / preference.
    paths: Mutex<Vec<Arc<Path>>>,

    // Local external address most recently reported by this peer (IP transport only).
    reported_local_ip: Mutex<Option<InetAddress>>,

    // Statistics and times of events.
    last_send_time_ticks: AtomicI64,
    last_receive_time_ticks: AtomicI64,
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
#[inline(always)]
fn salsa_derive_per_packet_key(key: &Secret<48>, header: &PacketHeader, packet_size: usize) -> Secret<48> {
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
#[inline(always)]
fn salsa_poly_create(secret: &SymmetricSecret, header: &PacketHeader, packet_size: usize) -> (Salsa, Poly1305) {
    let key = salsa_derive_per_packet_key(&secret.key, header, packet_size);
    let mut salsa = Salsa::new(&key.0[0..32], header.id_bytes(), true).unwrap();
    let mut poly1305_key = [0_u8; 32];
    salsa.crypt_in_place(&mut poly1305_key);
    (salsa, Poly1305::new(&poly1305_key).unwrap())
}

/// Attempt AEAD packet encryption and MAC validation.
fn try_aead_decrypt(secret: &SymmetricSecret, packet_frag0_payload_bytes: &[u8], header: &PacketHeader, fragments: &[Option<PacketBuffer>], payload: &mut Buffer<PACKET_SIZE_MAX>, message_id: &mut u64) -> bool {
    packet_frag0_payload_bytes.get(0).map_or(false, |verb| {
        match header.cipher() {
            CIPHER_NOCRYPT_POLY1305 => {
                if (verb & VERB_MASK) == VERB_VL1_HELLO {
                    let _ = payload.append_bytes(packet_frag0_payload_bytes);
                    for f in fragments.iter() {
                        let _ = f.as_ref().map(|f| f.as_bytes_starting_at(FRAGMENT_HEADER_SIZE).map(|f| payload.append_bytes(f)));
                    }
                    let (_, mut poly) = salsa_poly_create(secret, header, packet.len());
                    poly.update(payload.as_bytes());
                    if poly.finish()[0..8].eq(&header.mac) {
                        *message_id = u64::from_ne_bytes(header.id);
                        true
                    } else {
                        false
                    }
                } else {
                    // Only HELLO is permitted without payload encryption. Drop other packet types if sent this way.
                    false
                }
            }

            CIPHER_SALSA2012_POLY1305 => {
                let (mut salsa, mut poly) = salsa_poly_create(secret, header, packet.len());
                poly.update(packet_frag0_payload_bytes);
                let _ = payload.append_bytes_get_mut(packet_frag0_payload_bytes.len()).map(|b| salsa.crypt(packet_frag0_payload_bytes, b));
                for f in fragments.iter() {
                    let _ = f.as_ref().map(|f| f.as_bytes_starting_at(FRAGMENT_HEADER_SIZE).map(|f| {
                        poly.update(f);
                        let _ = payload.append_bytes_get_mut(f.len()).map(|b| salsa.crypt(f, b));
                    }));
                }
                if poly.finish()[0..8].eq(&header.mac) {
                    *message_id = u64::from_ne_bytes(header.id);
                    true
                } else {
                    false
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
                aes.decrypt_finish().map_or_else(false, |tag| {
                    // AES-GMAC-SIV encrypts the packet ID too as part of its computation of a single
                    // opaque 128-bit tag, so to get the original packet ID we have to grab it from the
                    // decrypted tag.
                    *mesasge_id = u64::from_ne_bytes(*array_range::<u8, 16, 0, 8>(tag));
                    true
                })
            }

            _ => false,
        }
    })
}

impl Peer {
    pub(crate) const INTERVAL: i64 = PEER_SERVICE_INTERVAL;

    /// Create a new peer.
    /// This only returns None if this_node_identity does not have its secrets or if some
    /// fatal error occurs performing key agreement between the two identities.
    pub(crate) fn new(this_node_identity: &Identity, id: Identity) -> Option<Peer> {
        this_node_identity.agree(&id).map(|static_secret| {
            Peer {
                identity: id,
                static_secret: SymmetricSecret::new(static_secret),
                static_secret_hello_dictionary: Mutex::new(AesCtr::new(&static_secret_hello_dictionary.0[0..32])),
                static_secret_packet_hmac,
                ephemeral_secret: Mutex::new(None),
                paths: Mutex::new(Vec::new()),
                reported_local_ip: Mutex::new(None),
                last_send_time_ticks: AtomicI64::new(0),
                last_receive_time_ticks: AtomicI64::new(0),
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

    /// Get the next message ID.
    #[inline(always)]
    pub(crate) fn next_message_id(&self) -> u64 { self.message_id_counter.fetch_add(1, Ordering::Relaxed) }

    /// Receive, decrypt, authenticate, and process an incoming packet from this peer.
    /// If the packet comes in multiple fragments, the fragments slice should contain all
    /// those fragments after the main packet header and first chunk.
    pub(crate) fn receive<CI: NodeInterface, PH: VL1PacketHandler>(&self, node: &Node, ci: &CI, ph: &PH, time_ticks: i64, source_path: &Arc<Path>, header: &PacketHeader, packet: &Buffer<{ PACKET_SIZE_MAX }>, fragments: &[Option<PacketBuffer>]) {
        let _ = packet.as_bytes_starting_at(PACKET_VERB_INDEX).map(|packet_frag0_payload_bytes| {
            let mut payload: Buffer<PACKET_SIZE_MAX> = unsafe { Buffer::new_nozero() };
            let mut message_id = 0_u64;
            let mut forward_secrecy = true;
            let ephemeral_secret: Option<Arc<EphemeralSymmetricSecret>> = self.ephemeral_secret.lock().clone();
            if !ephemeral_secret.map_or(false, |ephemeral_secret| try_aead_decrypt(&ephemeral_secret.secret, packet_frag0_payload_bytes, header, fragments, &mut payload, &mut message_id)) {
                unsafe { payload.set_size_unchecked(0); }
                if !try_aead_decrypt(&self.static_secret, packet_frag0_payload_bytes, header, fragments, &mut payload, &mut message_id) {
                    return;
                }
                forward_secrecy = false;
            }

            self.last_receive_time_ticks.store(time_ticks, Ordering::Relaxed);
            self.total_bytes_received.fetch_add((payload.len() + PACKET_HEADER_SIZE) as u64, Ordering::Relaxed);

            debug_assert!(!payload.is_empty()); // should be impossible since this fails in try_aead_decrypt()
            let mut verb = payload.as_bytes()[0];

            // If this flag is set, the end of the payload is a full HMAC-SHA384 authentication
            // tag for much stronger authentication.
            let extended_authentication = (verb & VERB_FLAG_EXTENDED_AUTHENTICATION) != 0;
            if extended_authentication {
                if payload.len() >= (1 + SHA384_HASH_SIZE) {
                    let actual_end_of_payload = payload.len() - SHA384_HASH_SIZE;
                    let hmac = SHA384::hmac_multipart(self.static_secret_packet_hmac.as_ref(), &[u64_as_bytes(&message_id), payload.as_bytes()]);
                    if !hmac.eq(&(payload.as_bytes()[actual_end_of_payload..])) {
                        return;
                    }
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
            verb &= VERB_MASK;
            if !ph.handle_packet(self, source_path, forward_secrecy, extended_authentication, verb, &payload) {
                match verb {
                    //VERB_VL1_NOP => {}
                    VERB_VL1_HELLO => self.receive_hello(ci, node, time_ticks, source_path, &payload),
                    VERB_VL1_ERROR => self.receive_error(ci, ph, node, time_ticks, source_path, forward_secrecy, extended_authentication, &payload),
                    VERB_VL1_OK => self.receive_ok(ci, ph, node, time_ticks, source_path, forward_secrecy, extended_authentication, &payload),
                    VERB_VL1_WHOIS => self.receive_whois(ci, node, time_ticks, source_path, &payload),
                    VERB_VL1_RENDEZVOUS => self.receive_rendezvous(ci, node, time_ticks, source_path, &payload),
                    VERB_VL1_ECHO => self.receive_echo(ci, node, time_ticks, source_path, &payload),
                    VERB_VL1_PUSH_DIRECT_PATHS => self.receive_push_direct_paths(ci, node, time_ticks, source_path, &payload),
                    VERB_VL1_USER_MESSAGE => self.receive_user_message(ci, node, time_ticks, source_path, &payload),
                    _ => {}
                }
            }
        });
    }

    fn send_to_endpoint<CI: NodeInterface>(&self, ci: &CI, endpoint: &Endpoint, local_socket: Option<NonZeroI64>, local_interface: Option<NonZeroI64>, packet: &Buffer<{ PACKET_SIZE_MAX }>) -> bool {
        debug_assert!(packet.len() <= PACKET_SIZE_MAX);
        debug_assert!(packet.len() >= PACKET_SIZE_MIN);
        match endpoint {
            Endpoint::Ip(_) | Endpoint::IpUdp(_) | Endpoint::Ethernet(_) | Endpoint::Bluetooth(_) | Endpoint::WifiDirect(_) => {
                let packet_size = packet.len();
                if packet_size > UDP_DEFAULT_MTU {
                    let bytes = packet.as_bytes();
                    if !ci.wire_send(endpoint, local_socket, local_interface, &[&bytes[0..UDP_DEFAULT_MTU]], 0) {
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
                        if !ci.wire_send(endpoint, local_socket, local_interface, &[header.as_bytes(), &bytes[pos..next_pos]], 0) {
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
                    return ci.wire_send(endpoint, local_socket, local_interface, &[packet.as_bytes()], 0);
                }
            }
            _ => {
                return ci.wire_send(endpoint, local_socket, local_interface, &[packet.as_bytes()], 0);
            }
        }
    }

    /// Send a packet to this peer.
    ///
    /// This will go directly if there is an active path, or otherwise indirectly
    /// via a root or some other route.
    pub(crate) fn send<CI: NodeInterface>(&self, ci: &CI, node: &Node, time_ticks: i64, packet: &Buffer<{ PACKET_SIZE_MAX }>) -> bool {
        self.path(node).map_or(false, |path| {
            if self.send_to_endpoint(ci, path.endpoint(), path.local_socket(), path.local_interface(), packet) {
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
    pub(crate) fn forward<CI: NodeInterface>(&self, ci: &CI, time_ticks: i64, packet: &Buffer<{ PACKET_SIZE_MAX }>) -> bool {
        self.direct_path().map_or(false, |path| {
            if ci.wire_send(path.endpoint(), path.local_socket(), path.local_interface(), &[packet.as_bytes()], 0) {
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
    /// If try_new_endpoint is not None the packet will be sent directly to this endpoint.
    /// Otherwise it will be sent via the best direct or indirect path.
    ///
    /// This has its own send logic so it can handle either an explicit endpoint or a
    /// known one.
    pub(crate) fn send_hello<CI: NodeInterface>(&self, ci: &CI, node: &Node, explicit_endpoint: Option<Endpoint>) -> bool {
        let path = if explicit_endpoint.is_none() { self.path(node) } else { None };
        explicit_endpoint.as_ref().map_or_else(|| Some(path.as_ref().unwrap().endpoint()), |ep| Some(ep)).map_or(false, |endpoint| {
            let mut packet: Buffer<{ PACKET_SIZE_MAX }> = Buffer::new();
            let time_ticks = ci.time_ticks();

            let message_id = self.next_message_id();
            let packet_header: &mut PacketHeader = packet.append_struct_get_mut().unwrap();
            let hello_fixed_headers: &mut message_component_structs::HelloFixedHeaderFields = packet.append_struct_get_mut().unwrap();
            packet_header.id = message_id.to_ne_bytes(); // packet ID and message ID are the same when Poly1305 MAC is used
            packet_header.dest = self.identity.address().to_bytes();
            packet_header.src = node.address().to_bytes();
            packet_header.flags_cipher_hops = CIPHER_NOCRYPT_POLY1305;
            hello_fixed_headers.verb = VERB_VL1_HELLO | VERB_FLAG_EXTENDED_AUTHENTICATION;
            hello_fixed_headers.version_proto = VERSION_PROTO;
            hello_fixed_headers.version_major = VERSION_MAJOR;
            hello_fixed_headers.version_minor = VERSION_MINOR;
            hello_fixed_headers.version_revision = (VERSION_REVISION as u16).to_be();
            hello_fixed_headers.timestamp = (time_ticks as u64).to_be();

            debug_assert!(self.identity.marshal(&mut packet, false).is_ok());
            debug_assert!(endpoint.marshal(&mut packet).is_ok());

            // Write an IV for AES-CTR encryption of the dictionary and allocate two more
            // bytes for reserved legacy use below.
            let aes_ctr_iv_position = packet.len();
            let aes_ctr_iv: &mut [u8; 18] = packet.append_bytes_fixed_get_mut().unwrap();
            zerotier_core_crypto::random::fill_bytes_secure(&mut aes_ctr_iv[0..16]);
            aes_ctr_iv[12] &= 0x7f; // mask off MSB of counter in iv to play nice with some AES-CTR implementations

            // LEGACY: create a 16-bit encrypted field that specifies zero "moons." This is ignored now
            // but causes old nodes to be able to parse this packet properly. This is not significant in
            // terms of encryption or authentication and can disappear once old versions are dead. Newer
            // versions ignore these bytes.
            let mut salsa_iv = message_id.to_ne_bytes();
            salsa_iv[7] &= 0xf8;
            Salsa::new(&self.static_secret.secret.0[0..32], &salsa_iv, true).unwrap().crypt(&[0_u8, 0_u8], &mut aes_ctr_iv[16..18]);

            // Create dictionary that contains extended HELLO fields.
            let dict_start_position = packet.len();
            let mut dict = Dictionary::new();
            dict.set_u64(HELLO_DICT_KEY_INSTANCE_ID, node.instance_id);
            dict.set_u64(HELLO_DICT_KEY_CLOCK, ci.time_clock() as u64);
            debug_assert!(dict.write_to(&mut packet).is_ok());

            // Encrypt extended fields with AES-CTR.
            let mut dict_aes = self.static_secret_hello_dictionary.lock();
            dict_aes.init(&packet.as_bytes()[aes_ctr_iv_position..aes_ctr_iv_position + 16]);
            dict_aes.crypt_in_place(&mut packet.as_bytes_mut()[dict_start_position..]);
            drop(dict_aes);

            // Append extended authentication HMAC.
            debug_assert!(packet.append_bytes_fixed(&SHA384::hmac_multipart(self.static_secret_packet_hmac.as_ref(), &[u64_as_bytes(&message_id), &packet.as_bytes()[PACKET_HEADER_SIZE..]])).is_ok());

            // Set outer packet MAC. We use legacy poly1305 for HELLO for backward
            // compatibility, but note that newer nodes and roots will check the full
            // HMAC-SHA384 above.
            let (_, mut poly) = salsa_poly_create(&self.static_secret, packet.struct_at::<PacketHeader>(0).unwrap(), packet.len());
            poly.update(packet.as_bytes_starting_at(PACKET_HEADER_SIZE).unwrap());
            packet_header.mac.copy_from_slice(&poly.finish()[0..8]);

            self.static_secret.encrypt_count.fetch_add(1, Ordering::Relaxed);
            self.last_send_time_ticks.store(time_ticks, Ordering::Relaxed);
            self.total_bytes_sent.fetch_add(packet.len() as u64, Ordering::Relaxed);

            path.as_ref().map_or_else(|| {
                self.send_to_endpoint(ci, endpoint, None, None, &packet)
            }, |path| {
                path.log_send(time_ticks);
                self.send_to_endpoint(ci, endpoint, path.local_socket(), path.local_interface(), &packet)
            })
        })
    }

    /// Called every INTERVAL during background tasks.
    #[inline(always)]
    pub(crate) fn call_every_interval<CI: NodeInterface>(&self, ct: &CI, time_ticks: i64) {}

    #[inline(always)]
    fn receive_hello<CI: NodeInterface>(&self, ci: &CI, node: &Node, time_ticks: i64, source_path: &Arc<Path>, payload: &Buffer<{ PACKET_SIZE_MAX }>) {}

    #[inline(always)]
    fn receive_error<CI: NodeInterface, PH: VL1PacketHandler>(&self, ci: &CI, ph: &PH, node: &Node, time_ticks: i64, source_path: &Arc<Path>, forward_secrecy: bool, extended_authentication: bool, payload: &Buffer<{ PACKET_SIZE_MAX }>) {
        let mut cursor: usize = 0;
        let _ = payload.read_struct::<message_component_structs::ErrorHeader>(&mut cursor).map(|error_header| {
            let in_re_packet_id = error_header.in_re_packet_id;
            let current_packet_id_counter = self.message_id_counter.load(Ordering::Relaxed);
            if current_packet_id_counter.checked_sub(in_re_packet_id).map_or_else(|| {
                (!in_re_packet_id).wrapping_add(current_packet_id_counter) < PACKET_RESPONSE_COUNTER_DELTA_MAX
            }, |packets_ago| {
                packets_ago <= PACKET_RESPONSE_COUNTER_DELTA_MAX
            }) {
                match error_header.in_re_verb {
                    _ => {
                        ph.handle_error(self, source_path, forward_secrecy, extended_authentication, error_header.in_re_verb, in_re_packet_id, error_header.error_code, payload, &mut cursor);
                    }
                }
            }
        });
    }

    #[inline(always)]
    fn receive_ok<CI: NodeInterface, PH: VL1PacketHandler>(&self, ci: &CI, ph: &PH, node: &Node, time_ticks: i64, source_path: &Arc<Path>, forward_secrecy: bool, extended_authentication: bool, payload: &Buffer<{ PACKET_SIZE_MAX }>) {
        let mut cursor: usize = 0;
        let _ = payload.read_struct::<message_component_structs::OkHeader>(&mut cursor).map(|ok_header| {
            let in_re_packet_id = ok_header.in_re_packet_id;
            let current_packet_id_counter = self.message_id_counter.load(Ordering::Relaxed);
            if current_packet_id_counter.checked_sub(in_re_packet_id).map_or_else(|| {
                (!in_re_packet_id).wrapping_add(current_packet_id_counter) < PACKET_RESPONSE_COUNTER_DELTA_MAX
            }, |packets_ago| {
                packets_ago <= PACKET_RESPONSE_COUNTER_DELTA_MAX
            }) {
                match ok_header.in_re_verb {
                    VERB_VL1_HELLO => {
                    }
                    VERB_VL1_WHOIS => {
                    }
                    _ => {
                        ph.handle_ok(self, source_path, forward_secrecy, extended_authentication, ok_header.in_re_verb, in_re_packet_id, payload, &mut cursor);
                    }
                }
            }
        });
    }

    #[inline(always)]
    fn receive_whois<CI: NodeInterface>(&self, ci: &CI, node: &Node, time_ticks: i64, source_path: &Arc<Path>, payload: &Buffer<{ PACKET_SIZE_MAX }>) {}

    #[inline(always)]
    fn receive_rendezvous<CI: NodeInterface>(&self, ci: &CI, node: &Node, time_ticks: i64, source_path: &Arc<Path>, payload: &Buffer<{ PACKET_SIZE_MAX }>) {}

    #[inline(always)]
    fn receive_echo<CI: NodeInterface>(&self, ci: &CI, node: &Node, time_ticks: i64, source_path: &Arc<Path>, payload: &Buffer<{ PACKET_SIZE_MAX }>) {}

    #[inline(always)]
    fn receive_push_direct_paths<CI: NodeInterface>(&self, ci: &CI, node: &Node, time_ticks: i64, source_path: &Arc<Path>, payload: &Buffer<{ PACKET_SIZE_MAX }>) {}

    #[inline(always)]
    fn receive_user_message<CI: NodeInterface>(&self, ci: &CI, node: &Node, time_ticks: i64, source_path: &Arc<Path>, payload: &Buffer<{ PACKET_SIZE_MAX }>) {}

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
