// (c) 2020-2022 ZeroTier, Inc. -- currently propritery pending actual release and licensing. See LICENSE.md.

use std::hash::{Hash, Hasher};
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Weak};

use parking_lot::{Mutex, RwLock};

use zerotier_core_crypto::aes_gmac_siv::AesCtr;
use zerotier_core_crypto::hash::*;
use zerotier_core_crypto::random::{get_bytes_secure, next_u64_secure};
use zerotier_core_crypto::salsa::Salsa;
use zerotier_core_crypto::secret::Secret;

use crate::util::byte_array_range;
use crate::util::debug_event;
use crate::util::marshalable::Marshalable;
use crate::vl1::node::*;
use crate::vl1::protocol::*;
use crate::vl1::symmetricsecret::{EphemeralSymmetricSecret, SymmetricSecret};
use crate::vl1::{Dictionary, Endpoint, Identity, Path};
use crate::{VERSION_MAJOR, VERSION_MINOR, VERSION_REVISION};

pub(crate) const SERVICE_INTERVAL_MS: i64 = security_constants::EPHEMERAL_SECRET_REKEY_AFTER_TIME / 10;

struct PeerPath<SI: SystemInterface> {
    path: Weak<Path<SI>>,
    path_internal_instance_id: usize,
    last_receive_time_ticks: i64,
}

/// A remote peer known to this node.
///
/// Equality and hashing is implemented in terms of the identity.
pub struct Peer<SI: SystemInterface> {
    // This peer's identity.
    pub(crate) identity: Identity,

    // Static shared secret computed from agreement with identity.
    identity_symmetric_key: SymmetricSecret,

    // Latest ephemeral secret or None if not yet negotiated.
    ephemeral_symmetric_key: RwLock<Option<EphemeralSymmetricSecret>>,

    // Paths sorted in descending order of quality / preference.
    paths: Mutex<Vec<PeerPath<SI>>>,

    // Statistics and times of events.
    last_send_time_ticks: AtomicI64,
    last_receive_time_ticks: AtomicI64,
    pub(crate) last_hello_reply_time_ticks: AtomicI64,
    last_forward_time_ticks: AtomicI64,
    create_time_ticks: i64,

    // Counter for assigning sequential message IDs.
    message_id_counter: AtomicU64,

    // Remote peer version information.
    remote_version: AtomicU64,
    remote_protocol_version: AtomicU8,
}

/// Create initialized instances of Salsa20/12 and Poly1305 for a packet (LEGACY).
fn salsa_poly_create(secret: &SymmetricSecret, header: &PacketHeader, packet_size: usize) -> (Salsa<12>, [u8; 32]) {
    // Create a per-packet key from the IV, source, destination, and packet size.
    let mut key: Secret<32> = secret.key.first_n();
    let hb = header.as_bytes();
    for i in 0..18 {
        key.0[i] ^= hb[i];
    }
    key.0[18] ^= header.flags_cipher_hops & packet_constants::FLAGS_FIELD_MASK_HIDE_HOPS;
    key.0[19] ^= packet_size as u8;
    key.0[20] ^= packet_size.wrapping_shr(8) as u8;

    let mut salsa = Salsa::<12>::new(&key.0, &header.id);
    let mut poly1305_key = [0_u8; 32];
    salsa.crypt_in_place(&mut poly1305_key);
    (salsa, poly1305_key)
}

/// Attempt AEAD packet encryption and MAC validation. Returns message ID on success.
fn try_aead_decrypt(secret: &SymmetricSecret, packet_frag0_payload_bytes: &[u8], header: &PacketHeader, fragments: &[Option<PooledPacketBuffer>], payload: &mut PacketBuffer) -> Option<MessageId> {
    packet_frag0_payload_bytes.get(0).map_or(None, |verb| {
        let cipher = header.cipher();
        match cipher {
            security_constants::CIPHER_NOCRYPT_POLY1305 | security_constants::CIPHER_SALSA2012_POLY1305 => {
                if (verb & packet_constants::VERB_MASK) == verbs::VL1_HELLO {
                    let mut total_packet_len = packet_frag0_payload_bytes.len() + packet_constants::HEADER_SIZE;
                    for f in fragments.iter() {
                        total_packet_len += f.as_ref().map_or(0, |f| f.len());
                    }
                    let _ = payload.append_bytes(packet_frag0_payload_bytes);
                    for f in fragments.iter() {
                        let _ = f.as_ref().map(|f| f.as_bytes_starting_at(packet_constants::HEADER_SIZE).map(|f| payload.append_bytes(f)));
                    }
                    let (mut salsa, poly1305_key) = salsa_poly_create(secret, header, total_packet_len);
                    let mac = zerotier_core_crypto::poly1305::compute(&poly1305_key, &payload.as_bytes());
                    if mac[0..8].eq(&header.mac) {
                        if cipher == security_constants::CIPHER_SALSA2012_POLY1305 {
                            salsa.crypt_in_place(payload.as_bytes_mut());
                        }
                        Some(u64::from_ne_bytes(header.id))
                    } else {
                        None
                    }
                } else {
                    // Only HELLO is permitted without payload encryption. Drop other packet types if sent this way.
                    None
                }
            }

            security_constants::CIPHER_AES_GMAC_SIV => {
                let mut aes = secret.aes_gmac_siv.get();
                aes.decrypt_init(&header.aes_gmac_siv_tag());
                aes.decrypt_set_aad(&header.aad_bytes());
                // NOTE: if there are somehow missing fragments this part will silently fail,
                // but the packet will fail MAC check in decrypt_finish() so meh.
                let _ = payload.append_bytes_get_mut(packet_frag0_payload_bytes.len()).map(|b| aes.decrypt(packet_frag0_payload_bytes, b));
                for f in fragments.iter() {
                    f.as_ref().map(|f| {
                        f.as_bytes_starting_at(packet_constants::FRAGMENT_HEADER_SIZE).map(|f| {
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

impl<SI: SystemInterface> Peer<SI> {
    /// Create a new peer.
    ///
    /// This only returns None if this_node_identity does not have its secrets or if some
    /// fatal error occurs performing key agreement between the two identities.
    pub(crate) fn new(this_node_identity: &Identity, id: Identity, time_clock: i64, time_ticks: i64) -> Option<Peer<SI>> {
        this_node_identity.agree(&id).map(|static_secret| -> Self {
            Self {
                identity: id,
                identity_symmetric_key: SymmetricSecret::new(static_secret),
                ephemeral_symmetric_key: RwLock::new(None),
                paths: Mutex::new(Vec::with_capacity(4)),
                last_send_time_ticks: AtomicI64::new(0),
                last_receive_time_ticks: AtomicI64::new(0),
                last_forward_time_ticks: AtomicI64::new(0),
                last_hello_reply_time_ticks: AtomicI64::new(0),
                create_time_ticks: time_ticks,
                message_id_counter: AtomicU64::new(((time_clock as u64) / 100).wrapping_shl(28) ^ next_u64_secure().wrapping_shr(36)),
                remote_version: AtomicU64::new(0),
                remote_protocol_version: AtomicU8::new(0),
            }
        })
    }

    /// Get the next message ID for sending a message to this peer.
    #[inline(always)]
    pub(crate) fn next_message_id(&self) -> MessageId {
        self.message_id_counter.fetch_add(1, Ordering::SeqCst)
    }

    /// Get current best path or None if there are no direct paths to this peer.
    pub fn direct_path(&self) -> Option<Arc<Path<SI>>> {
        for p in self.paths.lock().iter() {
            let pp = p.path.upgrade();
            if pp.is_some() {
                return pp;
            }
        }
        return None;
    }

    /// Get either the current best direct path or an indirect path.
    pub fn path(&self, node: &Node<SI>) -> Option<Arc<Path<SI>>> {
        let direct_path = self.direct_path();
        if direct_path.is_some() {
            return direct_path;
        }
        if let Some(root) = node.best_root() {
            return root.direct_path();
        }
        return None;
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

    /// Sort a list of paths by quality or priority, with best paths first.
    fn prioritize_paths(paths: &mut Vec<PeerPath<SI>>) {
        paths.sort_unstable_by(|a, b| a.last_receive_time_ticks.cmp(&b.last_receive_time_ticks).reverse());
    }

    /// Called every SERVICE_INTERVAL_MS by the background service loop in Node.
    pub(crate) fn service(&self, _: &SI, _: &Node<SI>, time_ticks: i64) -> bool {
        {
            let mut paths = self.paths.lock();
            paths.retain(|p| ((time_ticks - p.last_receive_time_ticks) < PEER_EXPIRATION_TIME) && (p.path.strong_count() > 0));
            Self::prioritize_paths(&mut paths);
        }
        (time_ticks - self.last_receive_time_ticks.load(Ordering::Relaxed).max(self.create_time_ticks)) < PEER_EXPIRATION_TIME
    }

    /// Receive, decrypt, authenticate, and process an incoming packet from this peer.
    ///
    /// If the packet comes in multiple fragments, the fragments slice should contain all
    /// those fragments after the main packet header and first chunk.
    ///
    /// This returns true if the packet decrypted and passed authentication.
    pub(crate) async fn receive<PH: InnerProtocolInterface>(&self, node: &Node<SI>, si: &SI, ph: &PH, time_ticks: i64, source_path: &Arc<Path<SI>>, packet_header: &PacketHeader, frag0: &PacketBuffer, fragments: &[Option<PooledPacketBuffer>]) {
        if let Ok(packet_frag0_payload_bytes) = frag0.as_bytes_starting_at(packet_constants::VERB_INDEX) {
            //let mut payload = unsafe { PacketBuffer::new_without_memzero() };
            let mut payload = PacketBuffer::new();

            // First try decrypting and authenticating with an ephemeral secret if one is negotiated.
            let (forward_secrecy, mut message_id) = if let Some(ephemeral_secret) = self.ephemeral_symmetric_key.read().as_ref() {
                if let Some(message_id) = try_aead_decrypt(&ephemeral_secret.secret, packet_frag0_payload_bytes, packet_header, fragments, &mut payload) {
                    // Decryption successful with ephemeral secret
                    (true, message_id)
                } else {
                    // Decryption failed with ephemeral secret, which may indicate that it's obsolete.
                    (false, 0)
                }
            } else {
                // There is no ephemeral secret negotiated (yet?).
                (false, 0)
            };

            // If forward_secrecy is false it means the ephemeral key failed. Try decrypting with the permanent key.
            if !forward_secrecy {
                payload.clear();
                if let Some(message_id2) = try_aead_decrypt(&self.identity_symmetric_key, packet_frag0_payload_bytes, packet_header, fragments, &mut payload) {
                    // Decryption successful with static secret.
                    message_id = message_id2;
                } else {
                    // Packet failed to decrypt using either ephemeral or permament key, reject.
                    debug_event!(si, "[vl1] #{:0>16x} failed authentication", u64::from_be_bytes(packet_header.id));
                    return;
                }
            }

            if let Ok(mut verb) = payload.u8_at(0) {
                let extended_authentication = (verb & packet_constants::VERB_FLAG_EXTENDED_AUTHENTICATION) != 0;
                if extended_authentication {
                    if payload.len() >= SHA512_HASH_SIZE {
                        let actual_end_of_payload = payload.len() - SHA512_HASH_SIZE;
                        let mut hmac = HMACSHA512::new(self.identity_symmetric_key.packet_hmac_key.as_bytes());
                        hmac.update(&node.identity.fingerprint);
                        hmac.update(&self.identity.fingerprint);
                        hmac.update(&message_id.to_ne_bytes());
                        hmac.update(&payload.as_bytes()[..actual_end_of_payload]);
                        if !hmac.finish().eq(&payload.as_bytes()[actual_end_of_payload..]) {
                            return;
                        }
                        payload.set_size(actual_end_of_payload);
                    } else {
                        return;
                    }
                }

                // ---------------------------------------------------------------
                // If we made it here it decrypted and passed authentication.
                // ---------------------------------------------------------------

                debug_event!(si, "[vl1] #{:0>16x} decrypted and authenticated, verb: {:0>2x}", u64::from_be_bytes(packet_header.id), (verb & packet_constants::VERB_MASK) as u32);

                if (verb & packet_constants::VERB_FLAG_COMPRESSED) != 0 {
                    let mut decompressed_payload: [u8; packet_constants::SIZE_MAX] = unsafe { MaybeUninit::uninit().assume_init() };
                    decompressed_payload[0] = verb;
                    if let Ok(dlen) = lz4_flex::block::decompress_into(&payload.as_bytes()[1..], &mut decompressed_payload[1..]) {
                        payload.set_to(&decompressed_payload[..(dlen + 1)]);
                    } else {
                        return;
                    }
                }

                self.last_receive_time_ticks.store(time_ticks, Ordering::Relaxed);

                let mut path_is_known = false;
                for p in self.paths.lock().iter_mut() {
                    if p.path_internal_instance_id == source_path.internal_instance_id {
                        p.last_receive_time_ticks = time_ticks;
                        path_is_known = true;
                        break;
                    }
                }

                // For performance reasons we let VL2 handle packets first. It returns false
                // if it didn't handle the packet, in which case it's handled at VL1. This is
                // because the most performance critical path is the handling of the ???_FRAME
                // verbs, which are in VL2.
                verb &= packet_constants::VERB_MASK; // mask off flags
                if !ph.handle_packet(self, &source_path, forward_secrecy, extended_authentication, verb, &payload).await {
                    match verb {
                        //VERB_VL1_NOP => {}
                        verbs::VL1_HELLO => self.handle_incoming_hello(si, node, time_ticks, source_path, &payload).await,
                        verbs::VL1_ERROR => self.handle_incoming_error(si, ph, node, time_ticks, source_path, forward_secrecy, extended_authentication, &payload).await,
                        verbs::VL1_OK => self.handle_incoming_ok(si, ph, node, time_ticks, source_path, path_is_known, forward_secrecy, extended_authentication, &payload).await,
                        verbs::VL1_WHOIS => self.handle_incoming_whois(si, node, time_ticks, source_path, &payload).await,
                        verbs::VL1_RENDEZVOUS => self.handle_incoming_rendezvous(si, node, time_ticks, source_path, &payload).await,
                        verbs::VL1_ECHO => self.handle_incoming_echo(si, node, time_ticks, source_path, &payload).await,
                        verbs::VL1_PUSH_DIRECT_PATHS => self.handle_incoming_push_direct_paths(si, node, time_ticks, source_path, &payload).await,
                        verbs::VL1_USER_MESSAGE => self.handle_incoming_user_message(si, node, time_ticks, source_path, &payload).await,
                        _ => {}
                    }
                }
            }
        }
    }

    /// Send to an endpoint, fragmenting if needed.
    ///
    /// This does not set the fragmentation field in the packet header, MAC, or encrypt the packet. The sender
    /// must do that while building the packet. The fragmentation flag must be set if fragmentation will be needed.
    async fn internal_send(&self, si: &SI, endpoint: &Endpoint, local_socket: Option<&SI::LocalSocket>, local_interface: Option<&SI::LocalInterface>, max_fragment_size: usize, packet: &PacketBuffer) -> bool {
        let packet_size = packet.len();
        if packet_size > max_fragment_size {
            let bytes = packet.as_bytes();
            if !si.wire_send(endpoint, local_socket, local_interface, &[&bytes[0..UDP_DEFAULT_MTU]], 0).await {
                return false;
            }
            let mut pos = UDP_DEFAULT_MTU;

            let overrun_size = (packet_size - UDP_DEFAULT_MTU) as u32;
            let fragment_count = (overrun_size / (UDP_DEFAULT_MTU - packet_constants::FRAGMENT_HEADER_SIZE) as u32) + (((overrun_size % (UDP_DEFAULT_MTU - packet_constants::FRAGMENT_HEADER_SIZE) as u32) != 0) as u32);
            debug_assert!(fragment_count <= packet_constants::FRAGMENT_COUNT_MAX as u32);

            let mut header = FragmentHeader {
                id: *packet.bytes_fixed_at(0).unwrap(),
                dest: *packet.bytes_fixed_at(packet_constants::DESTINATION_INDEX).unwrap(),
                fragment_indicator: packet_constants::FRAGMENT_INDICATOR,
                total_and_fragment_no: ((fragment_count + 1) << 4) as u8,
                reserved_hops: 0,
            };

            let mut chunk_size = (packet_size - pos).min(UDP_DEFAULT_MTU - packet_constants::HEADER_SIZE);
            loop {
                header.total_and_fragment_no += 1;
                let next_pos = pos + chunk_size;
                if !si.wire_send(endpoint, local_socket, local_interface, &[header.as_bytes(), &bytes[pos..next_pos]], 0).await {
                    return false;
                }
                pos = next_pos;
                if pos < packet_size {
                    chunk_size = (packet_size - pos).min(UDP_DEFAULT_MTU - packet_constants::HEADER_SIZE);
                } else {
                    return true;
                }
            }
        } else {
            return si.wire_send(endpoint, local_socket, local_interface, &[packet.as_bytes()], 0).await;
        }
    }

    /// Send a packet to this peer.
    ///
    /// This will go directly if there is an active path, or otherwise indirectly
    /// via a root or some other route.
    pub(crate) async fn send(&self, si: &SI, node: &Node<SI>, time_ticks: i64, packet: &PacketBuffer) -> bool {
        if let Some(path) = self.path(node) {
            if self.internal_send(si, &path.endpoint, Some(&path.local_socket), Some(&path.local_interface), if path.endpoint.requires_fragmentation() { UDP_DEFAULT_MTU } else { usize::MAX }, packet).await {
                self.last_send_time_ticks.store(time_ticks, Ordering::Relaxed);
                return true;
            }
        }
        return false;
    }

    /// Forward a packet to this peer.
    ///
    /// This is called when we receive a packet not addressed to this node and
    /// want to pass it along.
    ///
    /// This doesn't fragment large packets since fragments are forwarded individually.
    /// Intermediates don't need to adjust fragmentation.
    pub(crate) async fn forward(&self, si: &SI, time_ticks: i64, packet: &PacketBuffer) -> bool {
        if let Some(path) = self.direct_path() {
            if si.wire_send(&path.endpoint, Some(&path.local_socket), Some(&path.local_interface), &[packet.as_bytes()], 0).await {
                self.last_forward_time_ticks.store(time_ticks, Ordering::Relaxed);
                return true;
            }
        }
        return false;
    }

    /// Send a HELLO to this peer.
    ///
    /// If explicit_endpoint is not None the packet will be sent directly to this endpoint.
    /// Otherwise it will be sent via the best direct or indirect path known.
    ///
    /// Unlike other messages HELLO is sent partially in the clear and always with the long-lived
    /// static identity key. Authentication in old versions is via Poly1305 and in new versions
    /// via HMAC-SHA512.
    pub(crate) async fn send_hello(&self, si: &SI, node: &Node<SI>, explicit_endpoint: Option<&Endpoint>) -> bool {
        let mut path = None;
        let destination = if let Some(explicit_endpoint) = explicit_endpoint {
            explicit_endpoint
        } else {
            if let Some(p) = self.path(node) {
                let _ = path.insert(p);
                &path.as_ref().unwrap().endpoint
            } else {
                return false;
            }
        };

        let max_fragment_size = if destination.requires_fragmentation() { UDP_DEFAULT_MTU } else { usize::MAX };
        let time_ticks = si.time_ticks();

        let mut packet = PacketBuffer::new();
        {
            let message_id = self.next_message_id();

            {
                let packet_header: &mut PacketHeader = packet.append_struct_get_mut().unwrap();
                packet_header.id = message_id.to_ne_bytes(); // packet ID and message ID are the same when Poly1305 MAC is used
                packet_header.dest = self.identity.address.to_bytes();
                packet_header.src = node.identity.address.to_bytes();
                packet_header.flags_cipher_hops = security_constants::CIPHER_NOCRYPT_POLY1305;
            }

            {
                let hello_fixed_headers: &mut message_component_structs::HelloFixedHeaderFields = packet.append_struct_get_mut().unwrap();
                hello_fixed_headers.verb = verbs::VL1_HELLO | packet_constants::VERB_FLAG_EXTENDED_AUTHENTICATION;
                hello_fixed_headers.version_proto = PROTOCOL_VERSION;
                hello_fixed_headers.version_major = VERSION_MAJOR;
                hello_fixed_headers.version_minor = VERSION_MINOR;
                hello_fixed_headers.version_revision = (VERSION_REVISION as u16).to_be_bytes();
                hello_fixed_headers.timestamp = si.time_clock().to_be_bytes();
            }

            assert_eq!(packet.len(), 41);

            // Full identity of this node.
            assert!(node.identity.marshal_with_options(&mut packet, Identity::ALGORITHM_ALL, false).is_ok());

            // Append two reserved bytes, currently always zero.
            assert!(packet.append_padding(0, 2).is_ok());

            // Append a 16-byte AES-CTR nonce. LEGACY: for compatibility the last two bytes of this nonce
            // are in fact an encryption of two zeroes with Salsa20/12, which old nodes will interpret as
            // zero "moons." New nodes will just use these as part of the nonce.
            let mut nonce = get_bytes_secure::<16>();
            let mut salsa_iv = message_id.to_ne_bytes();
            salsa_iv[7] &= 0xf8;
            Salsa::<12>::new(&self.identity_symmetric_key.key.0[0..32], &salsa_iv).crypt(&crate::util::ZEROES[..2], &mut nonce[14..]);
            assert!(packet.append_bytes_fixed(&nonce).is_ok());

            // Add session meta-data, which is encrypted using plain AES-CTR. No authentication (AEAD) is needed
            // because the whole packet is authenticated. While the data in this section is not necessarily critical
            // to protect, encrypting it is a defense in depth measure.
            let mut session_metadata = Dictionary::new();
            session_metadata.set_bytes(session_metadata::INSTANCE_ID, node.instance_id.to_vec());
            session_metadata.set_u64(session_metadata::TIME_TICKS, time_ticks as u64);
            session_metadata.set_bytes(session_metadata::SENT_TO, destination.to_buffer::<{ Endpoint::MAX_MARSHAL_SIZE }>().unwrap().as_bytes().to_vec());
            let session_metadata = session_metadata.to_bytes();

            // Prefix encrypted session metadata with its size (in cleartext).
            assert!(session_metadata.len() <= 0xffff); // sanity check, should be impossible
            assert!(packet.append_u16(session_metadata.len() as u16).is_ok());

            // Write session meta-data in encrypted form. A derived key is made using the message ID as a salt
            // because this only ever uses the permanent identity key. In V2 we don't want to get near any
            // key usage boundaries unless forward secrecy is off for some reason.
            nonce[12] &= 0x7f; // mask off the MSB of the 32-bit counter part of the CTR nonce for compatibility with AES libraries that don't wrap
            let salted_key = Secret(hmac_sha384(&message_id.to_ne_bytes(), self.identity_symmetric_key.hello_private_section_key.as_bytes()));
            let mut aes = AesCtr::new(&salted_key.as_bytes()[0..32]);
            aes.init(&nonce);
            aes.crypt(session_metadata.as_slice(), packet.append_bytes_get_mut(session_metadata.len()).unwrap());

            // Set fragment flag if the packet will need to be fragmented.
            if (packet.len() + SHA512_HASH_SIZE) > max_fragment_size {
                set_packet_fragment_flag(&mut packet);
            }

            // Seal packet with HMAC-SHA512 extended authentication.
            let mut hmac = HMACSHA512::new(self.identity_symmetric_key.packet_hmac_key.as_bytes());
            hmac.update(&self.identity.fingerprint);
            hmac.update(&node.identity.fingerprint);
            hmac.update(&message_id.to_ne_bytes());
            hmac.update(&packet.as_bytes()[packet_constants::HEADER_SIZE..]);
            assert!(packet.append_bytes_fixed(&hmac.finish()).is_ok());
            drop(hmac);

            // Set poly1305 in header, which is the only authentication for old nodes.
            let (_, poly1305_key) = salsa_poly_create(&self.identity_symmetric_key, packet.struct_at::<PacketHeader>(0).unwrap(), packet.len());
            let mac = zerotier_core_crypto::poly1305::compute(&poly1305_key, packet.as_bytes_starting_at(packet_constants::HEADER_SIZE).unwrap());
            packet.as_mut()[packet_constants::MAC_FIELD_INDEX..packet_constants::MAC_FIELD_INDEX + 8].copy_from_slice(&mac[0..8]);

            self.last_send_time_ticks.store(time_ticks, Ordering::Relaxed);

            debug_event!(si, "HELLO -> {} @ {} ({} bytes)", self.identity.address.to_string(), destination.to_string(), packet.len());
        }

        if let Some(p) = path.as_ref() {
            if self.internal_send(si, destination, Some(&p.local_socket), Some(&p.local_interface), max_fragment_size, &packet).await {
                p.log_send_anything(time_ticks);
                true
            } else {
                false
            }
        } else {
            self.internal_send(si, destination, None, None, max_fragment_size, &packet).await
        }
    }

    async fn handle_incoming_hello(&self, si: &SI, node: &Node<SI>, time_ticks: i64, source_path: &Arc<Path<SI>>, payload: &PacketBuffer) {}

    async fn handle_incoming_error<PH: InnerProtocolInterface>(&self, si: &SI, ph: &PH, node: &Node<SI>, time_ticks: i64, source_path: &Arc<Path<SI>>, forward_secrecy: bool, extended_authentication: bool, payload: &PacketBuffer) {
        let mut cursor: usize = 1;
        if let Ok(error_header) = payload.read_struct::<message_component_structs::ErrorHeader>(&mut cursor) {
            let in_re_message_id = u64::from_ne_bytes(error_header.in_re_message_id);
            let current_packet_id_counter = self.message_id_counter.load(Ordering::Relaxed);
            if current_packet_id_counter.wrapping_sub(in_re_message_id) <= PACKET_RESPONSE_COUNTER_DELTA_MAX {
                match error_header.in_re_verb {
                    _ => {
                        ph.handle_error(self, &source_path, forward_secrecy, extended_authentication, error_header.in_re_verb, in_re_message_id, error_header.error_code, payload, &mut cursor).await;
                    }
                }
            }
        }
    }

    async fn handle_incoming_ok<PH: InnerProtocolInterface>(&self, si: &SI, ph: &PH, node: &Node<SI>, time_ticks: i64, source_path: &Arc<Path<SI>>, mut path_is_known: bool, forward_secrecy: bool, extended_authentication: bool, payload: &PacketBuffer) {
        let mut cursor: usize = 1;
        if let Ok(ok_header) = payload.read_struct::<message_component_structs::OkHeader>(&mut cursor) {
            let in_re_message_id = u64::from_ne_bytes(ok_header.in_re_message_id);
            let current_packet_id_counter = self.message_id_counter.load(Ordering::Relaxed);
            if current_packet_id_counter.wrapping_sub(in_re_message_id) <= PACKET_RESPONSE_COUNTER_DELTA_MAX {
                // TODO: learn new paths
                /*
                if !path_is_known {
                    let mut paths = self.paths.lock();
                    paths.retain_mut(|p| {
                        if let Some(path) = p.path.upgrade() {
                            match (&path.endpoint, &source_path.endpoint) {
                                (Endpoint::IpUdp(current_address), Endpoint::IpUdp(source_address)) => {
                                    if current_address.ip_bytes().eq(source_address.ip_bytes()) {
                                        // In UDP mode, replace paths that come from the same IP but a different port. This helps avoid
                                        // creating endless duplicate paths in NAT traversal scenarios if a NAT changes its port.
                                        p.path = Arc::downgrade(source_path);
                                        p.path_internal_instance_id = source_path.internal_instance_id;
                                        p.last_receive_time_ticks = time_ticks;
                                        path_is_known = true;
                                        true
                                    } else {
                                        true
                                    }
                                }
                                _ => true,
                            }
                        } else {
                            false
                        }
                    });
                    if !path_is_known {
                        paths.push(PeerPath::<SI> {
                            path: Arc::downgrade(source_path),
                            path_internal_instance_id: source_path.internal_instance_id,
                            last_receive_time_ticks: time_ticks,
                        });
                    }
                    Self::prioritize_paths(&mut paths);
                }
                */

                match ok_header.in_re_verb {
                    verbs::VL1_HELLO => {
                        // TODO
                        self.last_hello_reply_time_ticks.store(time_ticks, Ordering::Relaxed);
                    }
                    verbs::VL1_WHOIS => {}
                    _ => {
                        ph.handle_ok(self, &source_path, forward_secrecy, extended_authentication, ok_header.in_re_verb, in_re_message_id, payload, &mut cursor).await;
                    }
                }
            }
        }
    }

    async fn handle_incoming_whois(&self, si: &SI, node: &Node<SI>, time_ticks: i64, source_path: &Arc<Path<SI>>, payload: &PacketBuffer) {}

    async fn handle_incoming_rendezvous(&self, si: &SI, node: &Node<SI>, time_ticks: i64, source_path: &Arc<Path<SI>>, payload: &PacketBuffer) {}

    async fn handle_incoming_echo(&self, si: &SI, node: &Node<SI>, time_ticks: i64, source_path: &Arc<Path<SI>>, payload: &PacketBuffer) {}

    async fn handle_incoming_push_direct_paths(&self, si: &SI, node: &Node<SI>, time_ticks: i64, source_path: &Arc<Path<SI>>, payload: &PacketBuffer) {}

    async fn handle_incoming_user_message(&self, si: &SI, node: &Node<SI>, time_ticks: i64, source_path: &Arc<Path<SI>>, payload: &PacketBuffer) {}
}

impl<SI: SystemInterface> PartialEq for Peer<SI> {
    #[inline(always)]
    fn eq(&self, other: &Self) -> bool {
        self.identity.eq(&other.identity)
    }
}

impl<SI: SystemInterface> Eq for Peer<SI> {}

impl<SI: SystemInterface> Hash for Peer<SI> {
    #[inline(always)]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.identity.hash(state);
    }
}
