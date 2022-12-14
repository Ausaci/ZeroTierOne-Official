// (c) 2020-2022 ZeroTier, Inc. -- currently proprietary pending actual release and licensing. See LICENSE.md.

// ZSSP: ZeroTier Secure Session Protocol
// FIPS compliant Noise_IK with Jedi powers and built-in attack-resistant large payload (fragmentation) support.

use std::io::{Read, Write};
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};

use zerotier_crypto::aes::{Aes, AesGcm};
use zerotier_crypto::hash::{hmac_sha512, HMACSHA384, SHA384};
use zerotier_crypto::p384::{P384KeyPair, P384PublicKey, P384_PUBLIC_KEY_SIZE};
use zerotier_crypto::random;
use zerotier_crypto::secret::Secret;

use zerotier_utils::gatherarray::GatherArray;
use zerotier_utils::memory;
use zerotier_utils::ringbuffermap::RingBufferMap;
use zerotier_utils::unlikely_branch;
use zerotier_utils::varint;

/// Minimum size of a valid physical ZSSP packet or packet fragment.
pub const MIN_PACKET_SIZE: usize = HEADER_SIZE + AES_GCM_TAG_SIZE;

/// Minimum physical MTU for ZSSP to function.
pub const MIN_TRANSPORT_MTU: usize = 1280;

/// Minimum recommended interval between calls to service() on each session, in milliseconds.
pub const SERVICE_INTERVAL: u64 = 10000;

/// Setting this to true enables kyber1024 post-quantum forward secrecy.
///
/// Kyber1024 is used for data forward secrecy but not authentication. Authentication would
/// require Kyber1024 in identities, which would make them huge, and isn't needed for our
/// threat model which is data warehousing today to decrypt tomorrow. Breaking authentication
/// is only relevant today, not in some mid to far future where a QC that can break 384-bit ECC
/// exists.
///
/// This is normally enabled but could be disabled at build time for e.g. very small devices.
/// It might not even be necessary there to disable it since it's not that big and is usually
/// faster than NIST P-384 ECDH.
const JEDI: bool = true;

/// Maximum number of fragments for data packets.
const MAX_FRAGMENTS: usize = 48; // hard protocol max: 63

/// Maximum number of fragments for key exchange packets (can be smaller to save memory, only a few needed)
const KEY_EXCHANGE_MAX_FRAGMENTS: usize = 2; // enough room for p384 + ZT identity + kyber1024 + tag/hmac/etc.

/// Start attempting to rekey after a key has been used to send packets this many times.
///
/// This is 1/4 the NIST recommended maximum and 1/8 the absolute limit where u32 wraps.
/// As such it should leave plenty of margin against nearing key reuse bounds w/AES-GCM.
const REKEY_AFTER_USES: u64 = 536870912;

/// Maximum random jitter to add to rekey-after usage count.
const REKEY_AFTER_USES_MAX_JITTER: u32 = 1048576;

/// Hard expiration after this many uses.
///
/// Use of the key beyond this point is prohibited. If we reach this number of key uses
/// the key will be destroyed in memory and the session will cease to function. A hard
/// error is also generated.
const EXPIRE_AFTER_USES: u64 = (u32::MAX - 1024) as u64;

/// Start attempting to rekey after a key has been in use for this many milliseconds.
const REKEY_AFTER_TIME_MS: i64 = 1000 * 60 * 60; // 1 hour

/// Maximum random jitter to add to rekey-after time.
const REKEY_AFTER_TIME_MS_MAX_JITTER: u32 = 1000 * 60 * 10; // 10 minutes

/// Version 0: AES-256-GCM + NIST P-384 + optional Kyber1024 PQ forward secrecy
const SESSION_PROTOCOL_VERSION: u8 = 0x00;

/// Secondary key type: none, use only P-384 for forward secrecy.
const E1_TYPE_NONE: u8 = 0;

/// Secondary key type: Kyber1024, PQ forward secrecy enabled.
const E1_TYPE_KYBER1024: u8 = 1;

/// Size of packet header
const HEADER_SIZE: usize = 16;

/// Size of AES-GCM keys (256 bits)
const AES_KEY_SIZE: usize = 32;

/// Size of AES-GCM MAC tags
const AES_GCM_TAG_SIZE: usize = 16;

/// Size of HMAC-SHA384 MAC tags
const HMAC_SIZE: usize = 48;

/// Size of a session ID, which behaves a bit like a TCP port number.
///
/// This is large since some ZeroTier nodes handle huge numbers of links, like roots and controllers.
const SESSION_ID_SIZE: usize = 6;

/// Number of session keys to hold at a given time (current, previous, next).
const KEY_HISTORY_SIZE: usize = 3;

// Packet types can range from 0 to 15 (4 bits) -- 0-3 are defined and 4-15 are reserved for future use
const PACKET_TYPE_DATA: u8 = 0;
const PACKET_TYPE_NOP: u8 = 1;
const PACKET_TYPE_KEY_OFFER: u8 = 2; // "alice"
const PACKET_TYPE_KEY_COUNTER_OFFER: u8 = 3; // "bob"

// Key usage labels for sub-key derivation using NIST-style KBKDF (basically just HMAC KDF).
const KBKDF_KEY_USAGE_LABEL_HMAC: u8 = b'M'; // HMAC-SHA384 authentication for key exchanges
const KBKDF_KEY_USAGE_LABEL_HEADER_CHECK: u8 = b'H'; // AES-based header check code generation
const KBKDF_KEY_USAGE_LABEL_AES_GCM_ALICE_TO_BOB: u8 = b'A'; // AES-GCM in A->B direction
const KBKDF_KEY_USAGE_LABEL_AES_GCM_BOB_TO_ALICE: u8 = b'B'; // AES-GCM in B->A direction
const KBKDF_KEY_USAGE_LABEL_RATCHETING: u8 = b'R'; // Key input for next ephemeral ratcheting

// AES key size for header check code generation
const HEADER_CHECK_AES_KEY_SIZE: usize = 16;

/// Aribitrary starting value for master key derivation.
///
/// It doesn't matter very much what this is but it's good for it to be unique. It should
/// be changed if this code is changed in any cryptographically meaningful way like changing
/// the primary algorithm from NIST P-384 or the transport cipher from AES-GCM.
const INITIAL_KEY: [u8; 64] = [
    // macOS command line to generate:
    // echo -n 'ZSSP_Noise_IKpsk2_NISTP384_?KYBER1024_AESGCM_SHA512' | shasum -a 512  | cut -d ' ' -f 1 | xxd -r -p | xxd -i
    0x35, 0x6a, 0x75, 0xc0, 0xbf, 0xbe, 0xc3, 0x59, 0x70, 0x94, 0x50, 0x69, 0x4c, 0xa2, 0x08, 0x40, 0xc7, 0xdf, 0x67, 0xa8, 0x68, 0x52,
    0x6e, 0xd5, 0xdd, 0x77, 0xec, 0x59, 0x6f, 0x8e, 0xa1, 0x99, 0xb4, 0x32, 0x85, 0xaf, 0x7f, 0x0d, 0xa9, 0x6c, 0x01, 0xfb, 0x72, 0x46,
    0xc0, 0x09, 0x58, 0xb8, 0xe0, 0xa8, 0xcf, 0xb1, 0x58, 0x04, 0x6e, 0x32, 0xba, 0xa8, 0xb8, 0xf9, 0x0a, 0xa4, 0xbf, 0x36,
];

pub enum Error {
    /// The packet was addressed to an unrecognized local session (should usually be ignored)
    UnknownLocalSessionId(SessionId),

    /// Packet was not well formed
    InvalidPacket,

    /// An invalid parameter was supplied to the function
    InvalidParameter,

    /// Packet failed one or more authentication (MAC) checks
    FailedAuthentication,

    /// New session was rejected via Host::check_new_session_attempt or Host::accept_new_session.
    NewSessionRejected,

    /// Rekeying failed and session secret has reached its hard usage count limit
    MaxKeyLifetimeExceeded,

    /// Attempt to send using session without established key
    SessionNotEstablished,

    /// Packet ignored by rate limiter.
    RateLimited,

    /// The other peer specified an unrecognized protocol version
    UnknownProtocolVersion,

    /// Caller supplied data buffer is too small to receive data
    DataBufferTooSmall,

    /// Data object is too large to send, even with fragmentation
    DataTooLarge,

    /// An unexpected I/O error such as a buffer overrun occurred (possible bug)
    UnexpectedIoError(std::io::Error),
}

impl From<std::io::Error> for Error {
    #[cold]
    #[inline(never)]
    fn from(e: std::io::Error) -> Self {
        Self::UnexpectedIoError(e)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownLocalSessionId(id) => f.write_str(format!("UnknownLocalSessionId({})", id.0).as_str()),
            Self::InvalidPacket => f.write_str("InvalidPacket"),
            Self::InvalidParameter => f.write_str("InvalidParameter"),
            Self::FailedAuthentication => f.write_str("FailedAuthentication"),
            Self::NewSessionRejected => f.write_str("NewSessionRejected"),
            Self::MaxKeyLifetimeExceeded => f.write_str("MaxKeyLifetimeExceeded"),
            Self::SessionNotEstablished => f.write_str("SessionNotEstablished"),
            Self::RateLimited => f.write_str("RateLimited"),
            Self::UnknownProtocolVersion => f.write_str("UnknownProtocolVersion"),
            Self::DataBufferTooSmall => f.write_str("DataBufferTooSmall"),
            Self::DataTooLarge => f.write_str("DataTooLarge"),
            Self::UnexpectedIoError(e) => f.write_str(format!("UnexpectedIoError({})", e.to_string()).as_str()),
        }
    }
}

impl std::error::Error for Error {}

impl std::fmt::Debug for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

/// Result generated by the packet receive function, with possible payloads.
pub enum ReceiveResult<'a, H: ApplicationLayer> {
    /// Packet is valid, no action needs to be taken.
    Ok,

    /// Packet is valid and a data payload was decoded and authenticated.
    ///
    /// The returned reference is to the filled parts of the data buffer supplied to receive.
    OkData(&'a mut [u8]),

    /// Packet is valid and a new session was created.
    ///
    /// The session will have already been gated by the accept_new_session() method in the Host trait.
    OkNewSession(Session<H>),

    /// Packet appears valid but was ignored e.g. as a duplicate.
    Ignored,
}

/// 48-bit session ID (most significant 16 bits of u64 are unused)
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct SessionId(u64);

impl SessionId {
    /// The nil session ID used in messages initiating a new session.
    ///
    /// This is all 1's so that ZeroTier can easily tell the difference between ZSSP init packets
    /// and ZeroTier V1 packets.
    pub const NIL: SessionId = SessionId(0xffffffffffff);

    #[inline]
    pub fn new_from_u64(i: u64) -> Option<SessionId> {
        if i < Self::NIL.0 {
            Some(Self(i))
        } else {
            None
        }
    }

    #[inline]
    pub fn new_from_reader<R: Read>(r: &mut R) -> std::io::Result<Option<SessionId>> {
        let mut tmp = 0_u64.to_ne_bytes();
        r.read_exact(&mut tmp[..SESSION_ID_SIZE])?;
        Ok(Self::new_from_u64(u64::from_le_bytes(tmp)))
    }

    #[inline]
    pub fn new_random() -> Self {
        Self(random::next_u64_secure() % Self::NIL.0)
    }
}

impl From<SessionId> for u64 {
    #[inline(always)]
    fn from(sid: SessionId) -> Self {
        sid.0
    }
}

/// State information to associate with receiving contexts such as sockets or remote paths/endpoints.
///
/// This holds the data structures used to defragment incoming packets that are not associated with an
/// existing session, which would be new attempts to create sessions. Typically one of these is associated
/// with a single listen socket, local bound port, or other inbound endpoint.
pub struct ReceiveContext<H: ApplicationLayer> {
    initial_offer_defrag: Mutex<RingBufferMap<u32, GatherArray<H::IncomingPacketBuffer, KEY_EXCHANGE_MAX_FRAGMENTS>, 1024, 128>>,
    incoming_init_header_check_cipher: Aes,
}

/// Trait to implement to integrate the session into an application.
///
/// Templating the session on this trait lets the code here be almost entirely transport, OS,
/// and use case independent.
pub trait ApplicationLayer: Sized {
    /// Arbitrary opaque object associated with a session, such as a connection state object.
    type SessionUserData;

    /// Arbitrary object that dereferences to the session, such as Arc<Session<Self>>.
    type SessionRef: Deref<Target = Session<Self>>;

    /// A buffer containing data read from the network that can be cached.
    ///
    /// This can be e.g. a pooled buffer that automatically returns itself to the pool when dropped.
    /// It can also just be a Vec<u8> or Box<[u8]> or something like that.
    type IncomingPacketBuffer: AsRef<[u8]>;

    /// Remote physical address on whatever transport this session is using.
    type RemoteAddress;

    /// Rate limit for attempts to rekey existing sessions in milliseconds (default: 2000).
    const REKEY_RATE_LIMIT_MS: i64 = 2000;

    /// Get a reference to this host's static public key blob.
    ///
    /// This must contain a NIST P-384 public key but can contain other information. In ZeroTier this
    /// is a byte serialized identity. It could just be a naked NIST P-384 key if that's all you need.
    fn get_local_s_public_raw(&self) -> &[u8];

    /// Get SHA384(this host's static public key blob).
    ///
    /// This allows us to avoid computing SHA384(public key blob) over and over again.
    fn get_local_s_public_hash(&self) -> &[u8; 48];

    /// Get a reference to this hosts' static public key's NIST P-384 secret key pair.
    ///
    /// This must return the NIST P-384 public key that is contained within the static public key blob.
    fn get_local_s_keypair(&self) -> &P384KeyPair;

    /// Extract the NIST P-384 ECC public key component from a static public key blob or return None on failure.
    ///
    /// This is called to parse the static public key blob from the other end and extract its NIST P-384 public
    /// key. SECURITY NOTE: the information supplied here is from the wire so care must be taken to parse it
    /// safely and fail on any error or corruption.
    fn extract_s_public_from_raw(static_public: &[u8]) -> Option<P384PublicKey>;

    /// Look up a local session by local session ID or return None if not found.
    fn lookup_session(&self, local_session_id: SessionId) -> Option<Self::SessionRef>;

    /// Rate limit and check an attempted new session (called before accept_new_session).
    fn check_new_session(&self, rc: &ReceiveContext<Self>, remote_address: &Self::RemoteAddress) -> bool;

    /// Check whether a new session should be accepted.
    ///
    /// On success a tuple of local session ID, static secret, and associated object is returned. The
    /// static secret is whatever results from agreement between the local and remote static public
    /// keys.
    fn accept_new_session(
        &self,
        receive_context: &ReceiveContext<Self>,
        remote_address: &Self::RemoteAddress,
        remote_static_public: &[u8],
        remote_metadata: &[u8],
    ) -> Option<(SessionId, Secret<64>, Self::SessionUserData)>;
}

/// ZSSP bi-directional packet transport channel.
pub struct Session<Layer: ApplicationLayer> {
    /// This side's session ID (unique on this side)
    pub id: SessionId,

    /// An arbitrary object associated with session (type defined in Host trait)
    pub user_data: Layer::SessionUserData,

    send_counter: Counter,                            // Outgoing packet counter and nonce state
    psk: Secret<64>,                                  // Arbitrary PSK provided by external code
    noise_ss: Secret<48>,                             // Static raw shared ECDH NIST P-384 key
    header_check_cipher: Aes,                         // Cipher used for header MAC (fragmentation)
    state: RwLock<SessionMutableState>,               // Mutable parts of state (other than defrag buffers)
    remote_s_public_hash: [u8; 48],                   // SHA384(remote static public key blob)
    remote_s_public_raw: [u8; P384_PUBLIC_KEY_SIZE],  // Remote NIST P-384 static public key

    defrag: Mutex<RingBufferMap<u32, GatherArray<Layer::IncomingPacketBuffer, MAX_FRAGMENTS>, 8, 8>>,
}

struct SessionMutableState {
    remote_session_id: Option<SessionId>,         // The other side's 48-bit session ID
    session_keys: [Option<SessionKey>; KEY_HISTORY_SIZE], // Buffers to store current, next, and last active key
    cur_session_key_idx: usize,                               // Pointer used for keys[] circular buffer
    offer: Option<Box<EphemeralOffer>>,           // Most recent ephemeral offer sent to remote
    last_remote_offer: i64,                       // Time of most recent ephemeral offer (ms)
}

impl<Layer: ApplicationLayer> Session<Layer> {
    /// Create a new session and send an initial key offer message to the other end.
    ///
    /// * `host` - Interface to application using ZSSP
    /// * `local_session_id` - ID for this side (Alice) of the session, must be locally unique
    /// * `remote_s_public_raw` - Remote side's (Bob's) public key/identity
    /// * `offer_metadata` - Arbitrary meta-data to send with key offer (empty if none)
    /// * `psk` - Arbitrary pre-shared key to include as initial key material (use all zero secret if none)
    /// * `user_data` - Arbitrary object to put into session
    /// * `mtu` - Physical wire maximum transmition unit
    /// * `current_time` - Current monotonic time in milliseconds
    pub fn start_new<SendFunction: FnMut(&mut [u8])>(
        host: &Layer,
        mut send: SendFunction,
        local_session_id: SessionId,
        remote_s_public_raw: &[u8],
        offer_metadata: &[u8],
        psk: &Secret<64>,
        user_data: Layer::SessionUserData,
        mtu: usize,
        current_time: i64,
    ) -> Result<Self, Error> {
        let bob_s_public_raw = remote_s_public_raw;
        if let Some(bob_s_public) = Layer::extract_s_public_from_raw(bob_s_public_raw) {
            if let Some(noise_ss) = host.get_local_s_keypair().agree(&bob_s_public) {
                let send_counter = Counter::new();
                let bob_s_public_hash = SHA384::hash(bob_s_public_raw);
                let header_check_cipher =
                    Aes::new(kbkdf512(noise_ss.as_bytes(), KBKDF_KEY_USAGE_LABEL_HEADER_CHECK).first_n::<HEADER_CHECK_AES_KEY_SIZE>());
                if let Ok(offer) = send_ephemeral_offer(
                    &mut send,
                    send_counter.next(),
                    local_session_id,
                    None,
                    host.get_local_s_public_raw(),
                    offer_metadata,
                    &bob_s_public,
                    &bob_s_public_hash,
                    &noise_ss,
                    None,
                    None,
                    mtu,
                    current_time,
                ) {
                    return Ok(Self {
                        id: local_session_id,
                        user_data,
                        send_counter,
                        psk: psk.clone(),
                        noise_ss,
                        header_check_cipher,
                        state: RwLock::new(SessionMutableState {
                            remote_session_id: None,
                            session_keys: [None, None, None],
                            cur_session_key_idx: 0,
                            offer: Some(offer),
                            last_remote_offer: i64::MIN,
                        }),
                        remote_s_public_hash: bob_s_public_hash,
                        remote_s_public_raw: bob_s_public.as_bytes().clone(),
                        defrag: Mutex::new(RingBufferMap::new(random::xorshift64_random() as u32)),
                    });
                }
            }
        }
        return Err(Error::InvalidParameter);
    }

    /// Send data over the session.
    ///
    /// * `send` - Function to call to send physical packet(s)
    /// * `mtu_buffer` - A writable work buffer whose size also specifies the physical MTU
    /// * `data` - Data to send
    #[inline]
    pub fn send<SendFunction: FnMut(&mut [u8])>(
        &self,
        mut send: SendFunction,
        mtu_buffer: &mut [u8],
        mut data: &[u8],
    ) -> Result<(), Error> {
        debug_assert!(mtu_buffer.len() >= MIN_TRANSPORT_MTU);
        let state = self.state.read().unwrap();
        if let Some(remote_session_id) = state.remote_session_id {
            if let Some(sym_key) = state.session_keys[state.cur_session_key_idx].as_ref() {
                // Total size of the armored packet we are going to send (may end up being fragmented)
                let mut packet_len = data.len() + HEADER_SIZE + AES_GCM_TAG_SIZE;

                // This outgoing packet's nonce counter value.
                let counter = self.send_counter.next();

                // Create initial header for first fragment of packet and place in first HEADER_SIZE bytes of buffer.
                create_packet_header(
                    mtu_buffer,
                    packet_len,
                    mtu_buffer.len(),
                    PACKET_TYPE_DATA,
                    remote_session_id.into(),
                    counter,
                )?;

                // Get an initialized AES-GCM cipher and re-initialize with a 96-bit IV built from remote session ID,
                // packet type, and counter.
                let mut c = sym_key.get_send_cipher(counter)?;
                c.reset_init_gcm(CanonicalHeader::make(remote_session_id, PACKET_TYPE_DATA, counter.to_u32()).as_bytes());

                // Send first N-1 fragments of N total fragments.
                if packet_len > mtu_buffer.len() {
                    let mut header: [u8; 16] = mtu_buffer[..HEADER_SIZE].try_into().unwrap();
                    let fragment_data_mtu = mtu_buffer.len() - HEADER_SIZE;
                    let last_fragment_data_mtu = mtu_buffer.len() - (HEADER_SIZE + AES_GCM_TAG_SIZE);
                    loop {
                        let fragment_data_size = fragment_data_mtu.min(data.len());
                        let fragment_size = fragment_data_size + HEADER_SIZE;
                        c.crypt(&data[..fragment_data_size], &mut mtu_buffer[HEADER_SIZE..fragment_size]);
                        data = &data[fragment_data_size..];
                        set_header_check_code(mtu_buffer, &self.header_check_cipher);
                        send(&mut mtu_buffer[..fragment_size]);

                        debug_assert!(header[15].wrapping_shr(2) < 63);
                        header[15] += 0x04; // increment fragment number
                        mtu_buffer[..HEADER_SIZE].copy_from_slice(&header);

                        if data.len() <= last_fragment_data_mtu {
                            break;
                        }
                    }
                    packet_len = data.len() + HEADER_SIZE + AES_GCM_TAG_SIZE;
                }

                // Send final fragment (or only fragment if no fragmentation was needed)
                let gcm_tag_idx = data.len() + HEADER_SIZE;
                c.crypt(data, &mut mtu_buffer[HEADER_SIZE..gcm_tag_idx]);
                mtu_buffer[gcm_tag_idx..packet_len].copy_from_slice(&c.finish_encrypt());
                set_header_check_code(mtu_buffer, &self.header_check_cipher);
                send(&mut mtu_buffer[..packet_len]);

                // Check reusable AES-GCM instance back into pool.
                sym_key.return_send_cipher(c);

                return Ok(());
            } else {
                unlikely_branch();
            }
        } else {
            unlikely_branch();
        }
        return Err(Error::SessionNotEstablished);
    }

    /// Check whether this session is established.
    pub fn established(&self) -> bool {
        let state = self.state.read().unwrap();
        state.remote_session_id.is_some() && state.session_keys[state.cur_session_key_idx].is_some()
    }

    /// Get information about this session's security state.
    ///
    /// This returns a tuple of: the key fingerprint, the time it was established, the length of its ratchet chain,
    /// and whether Kyber1024 was used. None is returned if the session isn't established.
    pub fn status(&self) -> Option<([u8; 16], i64, u64, bool)> {
        let state = self.state.read().unwrap();
        if let Some(key) = state.session_keys[state.cur_session_key_idx].as_ref() {
            Some((key.secret_fingerprint, key.establish_time, key.ratchet_count, key.jedi))
        } else {
            None
        }
    }

    /// This function needs to be called on each session at least every SERVICE_INTERVAL milliseconds.
    ///
    /// * `host` - Interface to application using ZSSP
    /// * `send` - Function to call to send physical packet(s)
    /// * `offer_metadata' - Any meta-data to include with initial key offers sent.
    /// * `mtu` - Physical MTU for sent packets
    /// * `current_time` - Current monotonic time in milliseconds
    /// * `force_rekey` - Re-key the session now regardless of key aging (still subject to rate limiting)
    pub fn service<SendFunction: FnMut(&mut [u8])>(
        &self,
        host: &Layer,
        mut send: SendFunction,
        offer_metadata: &[u8],
        mtu: usize,
        current_time: i64,
        force_rekey: bool,
    ) {
        let state = self.state.read().unwrap();
        if (force_rekey
            || state.session_keys[state.cur_session_key_idx]
                .as_ref()
                .map_or(true, |key| key.lifetime.should_rekey(self.send_counter.previous(), current_time)))
            && state
                .offer
                .as_ref()
                .map_or(true, |o| (current_time - o.creation_time) > Layer::REKEY_RATE_LIMIT_MS)
        {
            if let Some(remote_s_public) = P384PublicKey::from_bytes(&self.remote_s_public_raw) {
                if let Ok(offer) = send_ephemeral_offer(
                    &mut send,
                    self.send_counter.next(),
                    self.id,
                    state.remote_session_id,
                    host.get_local_s_public_raw(),
                    offer_metadata,
                    &remote_s_public,
                    &self.remote_s_public_hash,
                    &self.noise_ss,
                    state.session_keys[state.cur_session_key_idx].as_ref(),
                    if state.remote_session_id.is_some() {
                        Some(&self.header_check_cipher)
                    } else {
                        None
                    },
                    mtu,
                    current_time,
                ) {
                    drop(state);
                    let _ = self.state.write().unwrap().offer.replace(offer);
                }
            }
        }
    }
}

impl<Layer: ApplicationLayer> ReceiveContext<Layer> {
    pub fn new(host: &Layer) -> Self {
        Self {
            initial_offer_defrag: Mutex::new(RingBufferMap::new(random::xorshift64_random() as u32)),
            incoming_init_header_check_cipher: Aes::new(
                kbkdf512(host.get_local_s_public_hash(), KBKDF_KEY_USAGE_LABEL_HEADER_CHECK).first_n::<HEADER_CHECK_AES_KEY_SIZE>(),
            ),
        }
    }

    /// Receive, authenticate, decrypt, and process a physical wire packet.
    ///
    /// * `host` - Interface to application using ZSSP
    /// * `remote_address` - Remote physical address of source endpoint
    /// * `data_buf` - Buffer to receive decrypted and authenticated object data (an error is returned if too small)
    /// * `incoming_packet_buf` - Buffer containing incoming wire packet (receive() takes ownership)
    /// * `mtu` - Physical wire MTU for sending packets
    /// * `current_time` - Current monotonic time in milliseconds
    #[inline]
    pub fn receive<'a, SendFunction: FnMut(&mut [u8])>(
        &self,
        host: &Layer,
        remote_address: &Layer::RemoteAddress,
        mut send: SendFunction,
        data_buf: &'a mut [u8],
        incoming_packet_buf: Layer::IncomingPacketBuffer,
        mtu: usize,
        current_time: i64,
    ) -> Result<ReceiveResult<'a, Layer>, Error> {
        let incoming_packet = incoming_packet_buf.as_ref();
        if incoming_packet.len() < MIN_PACKET_SIZE {
            unlikely_branch();
            return Err(Error::InvalidPacket);
        }

        let counter = u32::from_le(memory::load_raw(incoming_packet));
        let packet_type_fragment_info = u16::from_le(memory::load_raw(&incoming_packet[14..16]));
        let packet_type = (packet_type_fragment_info & 0x0f) as u8;
        let fragment_count = ((packet_type_fragment_info.wrapping_shr(4) + 1) as u8) & 63;
        let fragment_no = packet_type_fragment_info.wrapping_shr(10) as u8; // & 63 not needed

        if let Some(local_session_id) = SessionId::new_from_u64(u64::from_le(memory::load_raw(&incoming_packet[8..16])) & 0xffffffffffffu64)
        {
            if let Some(session) = host.lookup_session(local_session_id) {
                if verify_header_check_code(incoming_packet, &session.header_check_cipher) {
                    let canonical_header = CanonicalHeader::make(local_session_id, packet_type, counter);
                    if fragment_count > 1 {
                        if fragment_count <= (MAX_FRAGMENTS as u8) && fragment_no < fragment_count {
                            let mut defrag = session.defrag.lock().unwrap();
                            let fragment_gather_array = defrag.get_or_create_mut(&counter, || GatherArray::new(fragment_count));
                            if let Some(assembled_packet) = fragment_gather_array.add(fragment_no, incoming_packet_buf) {
                                drop(defrag); // release lock
                                return self.receive_complete(
                                    host,
                                    remote_address,
                                    &mut send,
                                    data_buf,
                                    canonical_header.as_bytes(),
                                    assembled_packet.as_ref(),
                                    packet_type,
                                    Some(session),
                                    mtu,
                                    current_time,
                                );
                            }
                        } else {
                            unlikely_branch();
                            return Err(Error::InvalidPacket);
                        }
                    } else {
                        return self.receive_complete(
                            host,
                            remote_address,
                            &mut send,
                            data_buf,
                            canonical_header.as_bytes(),
                            &[incoming_packet_buf],
                            packet_type,
                            Some(session),
                            mtu,
                            current_time,
                        );
                    }
                } else {
                    unlikely_branch();
                    return Err(Error::FailedAuthentication);
                }
            } else {
                unlikely_branch();
                return Err(Error::UnknownLocalSessionId(local_session_id));
            }
        } else {
            unlikely_branch(); // we want data receive to be the priority branch, this is only occasionally used

            if verify_header_check_code(incoming_packet, &self.incoming_init_header_check_cipher) {
                let canonical_header = CanonicalHeader::make(SessionId::NIL, packet_type, counter);
                if fragment_count > 1 {
                    let mut defrag = self.initial_offer_defrag.lock().unwrap();
                    let fragment_gather_array = defrag.get_or_create_mut(&counter, || GatherArray::new(fragment_count));
                    if let Some(assembled_packet) = fragment_gather_array.add(fragment_no, incoming_packet_buf) {
                        drop(defrag); // release lock
                        return self.receive_complete(
                            host,
                            remote_address,
                            &mut send,
                            data_buf,
                            canonical_header.as_bytes(),
                            assembled_packet.as_ref(),
                            packet_type,
                            None,
                            mtu,
                            current_time,
                        );
                    }
                } else {
                    return self.receive_complete(
                        host,
                        remote_address,
                        &mut send,
                        data_buf,
                        canonical_header.as_bytes(),
                        &[incoming_packet_buf],
                        packet_type,
                        None,
                        mtu,
                        current_time,
                    );
                }
            } else {
                unlikely_branch();
                return Err(Error::FailedAuthentication);
            }
        };

        return Ok(ReceiveResult::Ok);
    }

    /// Called internally when all fragments of a packet are received.
    ///
    /// NOTE: header check codes will already have been validated on receipt of each fragment. AEAD authentication
    /// and decryption has NOT yet been performed, and is done here.
    #[inline]
    fn receive_complete<'a, SendFunction: FnMut(&mut [u8])>(
        &self,
        host: &Layer,
        remote_address: &Layer::RemoteAddress,
        send: &mut SendFunction,
        data_buf: &'a mut [u8],
        canonical_header_bytes: &[u8; 12],
        fragments: &[Layer::IncomingPacketBuffer],
        packet_type: u8,
        session: Option<Layer::SessionRef>,
        mtu: usize,
        current_time: i64,
    ) -> Result<ReceiveResult<'a, Layer>, Error> {
        debug_assert!(fragments.len() >= 1);

        // The first 'if' below should capture both DATA and NOP but not other types. Sanity check this.
        debug_assert_eq!(PACKET_TYPE_DATA, 0);
        debug_assert_eq!(PACKET_TYPE_NOP, 1);

        if packet_type <= PACKET_TYPE_NOP {
            if let Some(session) = session {
                let state = session.state.read().unwrap();
                for p in 0..KEY_HISTORY_SIZE {
                    let key_idx = (state.cur_session_key_idx + p) % KEY_HISTORY_SIZE;
                    if let Some(session_key) = state.session_keys[key_idx].as_ref() {
                        let mut c = session_key.get_receive_cipher();
                        c.reset_init_gcm(canonical_header_bytes);

                        let mut data_len = 0;

                        // Decrypt fragments 0..N-1 where N is the number of fragments.
                        for f in fragments[..(fragments.len() - 1)].iter() {
                            let f = f.as_ref();
                            debug_assert!(f.len() >= HEADER_SIZE);
                            let current_frag_data_start = data_len;
                            data_len += f.len() - HEADER_SIZE;
                            if data_len > data_buf.len() {
                                unlikely_branch();
                                session_key.return_receive_cipher(c);
                                return Err(Error::DataBufferTooSmall);
                            }
                            c.crypt(&f[HEADER_SIZE..], &mut data_buf[current_frag_data_start..data_len]);
                        }

                        // Decrypt final fragment (or only fragment if not fragmented)
                        let current_frag_data_start = data_len;
                        let last_fragment = fragments.last().unwrap().as_ref();
                        if last_fragment.len() < (HEADER_SIZE + AES_GCM_TAG_SIZE) {
                            unlikely_branch();
                            return Err(Error::InvalidPacket);
                        }
                        data_len += last_fragment.len() - (HEADER_SIZE + AES_GCM_TAG_SIZE);
                        if data_len > data_buf.len() {
                            unlikely_branch();
                            session_key.return_receive_cipher(c);
                            return Err(Error::DataBufferTooSmall);
                        }
                        c.crypt(
                            &last_fragment[HEADER_SIZE..(last_fragment.len() - AES_GCM_TAG_SIZE)],
                            &mut data_buf[current_frag_data_start..data_len],
                        );

                        let aead_authentication_ok = c.finish_decrypt(&last_fragment[(last_fragment.len() - AES_GCM_TAG_SIZE)..]);
                        session_key.return_receive_cipher(c);

                        if aead_authentication_ok {
                            // Select this key as the new default if it's newer than the current key.
                            if p > 0
                                && state.session_keys[state.cur_session_key_idx]
                                    .as_ref()
                                    .map_or(true, |old| old.establish_counter < session_key.establish_counter)
                            {
                                drop(state);
                                let mut state = session.state.write().unwrap();
                                state.cur_session_key_idx = key_idx;
                                for i in 0..KEY_HISTORY_SIZE {
                                    if i != key_idx {
                                        if let Some(old_key) = state.session_keys[key_idx].as_ref() {
                                            // Release pooled cipher memory from old keys.
                                            old_key.receive_cipher_pool.lock().unwrap().clear();
                                            old_key.send_cipher_pool.lock().unwrap().clear();
                                        }
                                    }
                                }
                            }

                            if packet_type == PACKET_TYPE_DATA {
                                return Ok(ReceiveResult::OkData(&mut data_buf[..data_len]));
                            } else {
                                unlikely_branch();
                                return Ok(ReceiveResult::Ok);
                            }
                        }
                    }
                }

                // If no known key authenticated the packet, decryption has failed.
                return Err(Error::FailedAuthentication);
            } else {
                unlikely_branch();
                return Err(Error::SessionNotEstablished);
            }
        } else {
            unlikely_branch();

            // To greatly simplify logic handling key exchange packets, assemble these first.
            // Handling KEX packets isn't the fast path so the extra copying isn't significant.
            const KEX_BUF_LEN: usize = MIN_TRANSPORT_MTU * KEY_EXCHANGE_MAX_FRAGMENTS;
            let mut kex_packet = [0_u8; KEX_BUF_LEN];
            let mut kex_packet_len = 0;
            for i in 0..fragments.len() {
                let mut ff = fragments[i].as_ref();
                debug_assert!(ff.len() >= MIN_PACKET_SIZE);
                if i > 0 {
                    ff = &ff[HEADER_SIZE..];
                }
                let j = kex_packet_len + ff.len();
                if j > KEX_BUF_LEN {
                    return Err(Error::InvalidPacket);
                }
                kex_packet[kex_packet_len..j].copy_from_slice(ff);
                kex_packet_len = j;
            }
            let kex_packet_saved_ciphertext = kex_packet.clone(); // save for HMAC check later

            // Key exchange packets begin (after header) with the session protocol version.
            if kex_packet[HEADER_SIZE] != SESSION_PROTOCOL_VERSION {
                return Err(Error::UnknownProtocolVersion);
            }

            match packet_type {
                PACKET_TYPE_KEY_OFFER => {
                    // alice (remote) -> bob (local)

                    if kex_packet_len < (HEADER_SIZE + 1 + P384_PUBLIC_KEY_SIZE + AES_GCM_TAG_SIZE + HMAC_SIZE + HMAC_SIZE) {
                        return Err(Error::InvalidPacket);
                    }
                    let payload_end = kex_packet_len - (AES_GCM_TAG_SIZE + HMAC_SIZE + HMAC_SIZE);
                    let aes_gcm_tag_end = kex_packet_len - (HMAC_SIZE + HMAC_SIZE);
                    let hmac1_end = kex_packet_len - HMAC_SIZE;

                    // Check the second HMAC first, which proves that the sender knows the recipient's full static identity.
                    if !hmac_sha384_2(
                        host.get_local_s_public_hash(),
                        canonical_header_bytes,
                        &kex_packet[HEADER_SIZE..hmac1_end],
                    )
                    .eq(&kex_packet[hmac1_end..kex_packet_len])
                    {
                        return Err(Error::FailedAuthentication);
                    }

                    // Check rate limits.
                    if let Some(session) = session.as_ref() {
                        if (current_time - session.state.read().unwrap().last_remote_offer) < Layer::REKEY_RATE_LIMIT_MS {
                            return Err(Error::RateLimited);
                        }
                    } else {
                        if !host.check_new_session(self, remote_address) {
                            return Err(Error::RateLimited);
                        }
                    }

                    // Key agreement: alice (remote) ephemeral NIST P-384 <> local static NIST P-384
                    let (alice_e_public, noise_es) =
                        P384PublicKey::from_bytes(&kex_packet[(HEADER_SIZE + 1)..(HEADER_SIZE + 1 + P384_PUBLIC_KEY_SIZE)])
                            .and_then(|pk| host.get_local_s_keypair().agree(&pk).map(move |s| (pk, s)))
                            .ok_or(Error::FailedAuthentication)?;

                    // Initial key derivation from starting point, mixing in alice's ephemeral public and the es.
                    let es_key = Secret(hmac_sha512(&hmac_sha512(&INITIAL_KEY, alice_e_public.as_bytes()), noise_es.as_bytes()));

                    // Decrypt the encrypted part of the packet payload and authenticate the above key exchange via AES-GCM auth.
                    let mut c = AesGcm::new(
                        kbkdf512(es_key.as_bytes(), KBKDF_KEY_USAGE_LABEL_AES_GCM_ALICE_TO_BOB).first_n::<AES_KEY_SIZE>(),
                        false,
                    );
                    c.reset_init_gcm(canonical_header_bytes);
                    c.crypt_in_place(&mut kex_packet[(HEADER_SIZE + 1 + P384_PUBLIC_KEY_SIZE)..payload_end]);
                    if !c.finish_decrypt(&kex_packet[payload_end..aes_gcm_tag_end]) {
                        return Err(Error::FailedAuthentication);
                    }

                    // Parse payload and get alice's session ID, alice's public blob, metadata, and (if present) Alice's Kyber1024 public.
                    let (offer_id, alice_session_id, alice_s_public_raw, alice_metadata, alice_e1_public_raw, alice_ratchet_key_fingerprint) =
                        parse_key_offer_after_header(&kex_packet[(HEADER_SIZE + 1 + P384_PUBLIC_KEY_SIZE)..kex_packet_len], packet_type)?;

                    // We either have a session, in which case they should have supplied a ratchet key fingerprint, or
                    // we don't and they should not have supplied one.
                    if session.is_some() != alice_ratchet_key_fingerprint.is_some() {
                        return Err(Error::FailedAuthentication);
                    }

                    // Extract alice's static NIST P-384 public key from her public blob.
                    let alice_s_public = Layer::extract_s_public_from_raw(alice_s_public_raw).ok_or(Error::InvalidPacket)?;

                    // Key agreement: both sides' static P-384 keys.
                    let noise_ss = host
                        .get_local_s_keypair()
                        .agree(&alice_s_public)
                        .ok_or(Error::FailedAuthentication)?;

                    // Mix result of 'ss' agreement into master key.
                    let ss_key = Secret(hmac_sha512(es_key.as_bytes(), noise_ss.as_bytes()));
                    drop(es_key);

                    // Authenticate entire packet with HMAC-SHA384, verifying alice's identity via 'ss' secret that was
                    // just mixed into the key.
                    if !hmac_sha384_2(
                        kbkdf512(ss_key.as_bytes(), KBKDF_KEY_USAGE_LABEL_HMAC).first_n::<48>(),
                        canonical_header_bytes,
                        &kex_packet_saved_ciphertext[HEADER_SIZE..aes_gcm_tag_end],
                    )
                    .eq(&kex_packet[aes_gcm_tag_end..hmac1_end])
                    {
                        return Err(Error::FailedAuthentication);
                    }

                    // Alice's offer has been verified and her current key state reconstructed.

                    // Perform checks and match ratchet key if there's an existing session, or gate (via host) and
                    // then create new sessions.
                    let (new_session, ratchet_key, ratchet_count) = if let Some(session) = session.as_ref() {
                        // Existing session identity must match the one in this offer.
                        if !session.remote_s_public_hash.eq(&SHA384::hash(&alice_s_public_raw)) {
                            return Err(Error::FailedAuthentication);
                        }

                        // Match ratchet key fingerprint and fail if no match, which likely indicates an old offer packet.
                        let alice_ratchet_key_fingerprint = alice_ratchet_key_fingerprint.as_ref().unwrap();
                        let mut ratchet_key = None;
                        let mut ratchet_count = 0;
                        let state = session.state.read().unwrap();
                        for k in state.session_keys.iter() {
                            if let Some(k) = k.as_ref() {
                                if secret_fingerprint(k.ratchet_key.as_bytes())[..16].eq(alice_ratchet_key_fingerprint) {
                                    ratchet_key = Some(k.ratchet_key.clone());
                                    ratchet_count = k.ratchet_count;
                                    break;
                                }
                            }
                        }
                        if ratchet_key.is_none() {
                            return Ok(ReceiveResult::Ignored); // old packet?
                        }

                        (None, ratchet_key, ratchet_count)
                    } else {
                        if let Some((new_session_id, psk, associated_object)) =
                            host.accept_new_session(self, remote_address, alice_s_public_raw, alice_metadata)
                        {
                            let header_check_cipher = Aes::new(
                                kbkdf512(noise_ss.as_bytes(), KBKDF_KEY_USAGE_LABEL_HEADER_CHECK).first_n::<HEADER_CHECK_AES_KEY_SIZE>(),
                            );
                            (
                                Some(Session::<Layer> {
                                    id: new_session_id,
                                    user_data: associated_object,
                                    send_counter: Counter::new(),
                                    psk,
                                    noise_ss,
                                    header_check_cipher,
                                    state: RwLock::new(SessionMutableState {
                                        remote_session_id: Some(alice_session_id),
                                        session_keys: [None, None, None],
                                        cur_session_key_idx: 0,
                                        offer: None,
                                        last_remote_offer: current_time,
                                    }),
                                    remote_s_public_hash: SHA384::hash(&alice_s_public_raw),
                                    remote_s_public_raw: alice_s_public.as_bytes().clone(),
                                    defrag: Mutex::new(RingBufferMap::new(random::xorshift64_random() as u32)),
                                }),
                                None,
                                0,
                            )
                        } else {
                            return Err(Error::NewSessionRejected);
                        }
                    };

                    // Set 'session' to a reference to either the existing or the new session.
                    let existing_session = session;
                    let session = existing_session.as_ref().map_or_else(|| new_session.as_ref().unwrap(), |s| &*s);

                    // Generate our ephemeral NIST P-384 key pair.
                    let bob_e_keypair = P384KeyPair::generate();

                    // Key agreement: both sides' ephemeral P-384 public keys.
                    let noise_ee = bob_e_keypair.agree(&alice_e_public).ok_or(Error::FailedAuthentication)?;

                    // Key agreement: bob (local) static NIST P-384, alice (remote) ephemeral P-384.
                    let noise_se = bob_e_keypair.agree(&alice_s_public).ok_or(Error::FailedAuthentication)?;

                    // Mix in the psk, the key to this point, our ephemeral public, ee, and se, completing Noise_IK.
                    //
                    // FIPS note: the order of HMAC parameters are flipped here from the usual Noise HMAC(key, X). That's because
                    // NIST/FIPS allows HKDF with HMAC(salt, key) and salt is allowed to be anything. This way if the PSK is not
                    // FIPS compliant the compliance of the entire key derivation is not invalidated. Both inputs are secrets of
                    // fixed size so this shouldn't matter cryptographically.
                    let noise_ik_key = Secret(hmac_sha512(
                        session.psk.as_bytes(),
                        &hmac_sha512(
                            &hmac_sha512(&hmac_sha512(ss_key.as_bytes(), bob_e_keypair.public_key_bytes()), noise_ee.as_bytes()),
                            noise_se.as_bytes(),
                        ),
                    ));
                    drop(ss_key);

                    // At this point we've completed Noise_IK key derivation with NIST P-384 ECDH, but now for hybrid and ratcheting...

                    // Generate a Kyber encapsulated ciphertext if Kyber is enabled and the other side sent us a public key.
                    let (bob_e1_public, e1e1) = if JEDI && alice_e1_public_raw.len() > 0 {
                        if let Ok((bob_e1_public, e1e1)) = pqc_kyber::encapsulate(alice_e1_public_raw, &mut random::SecureRandom::default()) {
                            (Some(bob_e1_public), Some(Secret(e1e1)))
                        } else {
                            return Err(Error::FailedAuthentication);
                        }
                    } else {
                        (None, None)
                    };

                    // Create reply packet.
                    let mut reply_buf = [0_u8; KEX_BUF_LEN];
                    let reply_counter = session.send_counter.next();
                    let mut reply_len = {
                        let mut rp = &mut reply_buf[HEADER_SIZE..];

                        rp.write_all(&[SESSION_PROTOCOL_VERSION])?;
                        rp.write_all(bob_e_keypair.public_key_bytes())?;

                        rp.write_all(&offer_id)?;
                        rp.write_all(&session.id.0.to_le_bytes()[..SESSION_ID_SIZE])?;
                        varint::write(&mut rp, 0)?; // they don't need our static public; they have it
                        varint::write(&mut rp, 0)?; // no meta-data in counter-offers (could be used in the future)
                        if let Some(bob_e1_public) = bob_e1_public.as_ref() {
                            rp.write_all(&[E1_TYPE_KYBER1024])?;
                            rp.write_all(bob_e1_public)?;
                        } else {
                            rp.write_all(&[E1_TYPE_NONE])?;
                        }
                        if ratchet_key.is_some() {
                            rp.write_all(&[0x01])?;
                            rp.write_all(alice_ratchet_key_fingerprint.as_ref().unwrap())?;
                        } else {
                            rp.write_all(&[0x00])?;
                        }

                        KEX_BUF_LEN - rp.len()
                    };
                    create_packet_header(
                        &mut reply_buf,
                        reply_len,
                        mtu,
                        PACKET_TYPE_KEY_COUNTER_OFFER,
                        alice_session_id.into(),
                        reply_counter,
                    )?;
                    let reply_canonical_header =
                        CanonicalHeader::make(alice_session_id.into(), PACKET_TYPE_KEY_COUNTER_OFFER, reply_counter.to_u32());

                    // Encrypt reply packet using final Noise_IK key BEFORE mixing hybrid or ratcheting, since the other side
                    // must decrypt before doing these things.
                    let mut c = AesGcm::new(
                        kbkdf512(noise_ik_key.as_bytes(), KBKDF_KEY_USAGE_LABEL_AES_GCM_BOB_TO_ALICE).first_n::<AES_KEY_SIZE>(),
                        true,
                    );
                    c.reset_init_gcm(reply_canonical_header.as_bytes());
                    c.crypt_in_place(&mut reply_buf[(HEADER_SIZE + 1 + P384_PUBLIC_KEY_SIZE)..reply_len]);
                    let c = c.finish_encrypt();
                    reply_buf[reply_len..(reply_len + AES_GCM_TAG_SIZE)].copy_from_slice(&c);
                    reply_len += AES_GCM_TAG_SIZE;

                    // Mix ratchet key from previous session key (if any) and Kyber1024 hybrid shared key (if any).
                    let mut session_key = noise_ik_key;
                    if let Some(ratchet_key) = ratchet_key {
                        session_key = Secret(hmac_sha512(ratchet_key.as_bytes(), session_key.as_bytes()));
                    }
                    if let Some(e1e1) = e1e1.as_ref() {
                        session_key = Secret(hmac_sha512(e1e1.as_bytes(), session_key.as_bytes()));
                    }

                    // Authenticate packet using HMAC-SHA384 with final key. Note that while the final key now has the Kyber secret
                    // mixed in, this doesn't constitute session authentication with Kyber because there's no static Kyber key
                    // associated with the remote identity. An attacker who can break NIST P-384 (and has the psk) could MITM the
                    // Kyber exchange, but you'd need a not-yet-existing quantum computer for that.
                    let hmac = hmac_sha384_2(
                        kbkdf512(session_key.as_bytes(), KBKDF_KEY_USAGE_LABEL_HMAC).first_n::<48>(),
                        reply_canonical_header.as_bytes(),
                        &reply_buf[HEADER_SIZE..reply_len],
                    );
                    reply_buf[reply_len..(reply_len + HMAC_SIZE)].copy_from_slice(&hmac);
                    reply_len += HMAC_SIZE;

                    let session_key = SessionKey::new(session_key, Role::Bob, current_time, reply_counter, ratchet_count + 1, e1e1.is_some());

                    let mut state = session.state.write().unwrap();
                    let _ = state.remote_session_id.replace(alice_session_id);
                    let next_key_ptr = (state.cur_session_key_idx + 1) % KEY_HISTORY_SIZE;
                    let _ = state.session_keys[next_key_ptr].replace(session_key);
                    drop(state);

                    // Bob now has final key state for this exchange. Yay! Now reply to Alice so she can construct it.

                    send_with_fragmentation(send, &mut reply_buf[..reply_len], mtu, &session.header_check_cipher);

                    if new_session.is_some() {
                        return Ok(ReceiveResult::OkNewSession(new_session.unwrap()));
                    } else {
                        return Ok(ReceiveResult::Ok);
                    }
                }

                PACKET_TYPE_KEY_COUNTER_OFFER => {
                    // bob (remote) -> alice (local)

                    if kex_packet_len < (HEADER_SIZE + 1 + P384_PUBLIC_KEY_SIZE + AES_GCM_TAG_SIZE + HMAC_SIZE) {
                        return Err(Error::InvalidPacket);
                    }
                    let payload_end = kex_packet_len - (AES_GCM_TAG_SIZE + HMAC_SIZE);
                    let aes_gcm_tag_end = kex_packet_len - HMAC_SIZE;

                    if let Some(session) = session {
                        let state = session.state.read().unwrap();
                        if let Some(offer) = state.offer.as_ref() {
                            let (bob_e_public, noise_ee) =
                                P384PublicKey::from_bytes(&kex_packet[(HEADER_SIZE + 1)..(HEADER_SIZE + 1 + P384_PUBLIC_KEY_SIZE)])
                                    .and_then(|pk| offer.alice_e_keypair.agree(&pk).map(move |s| (pk, s)))
                                    .ok_or(Error::FailedAuthentication)?;
                            let noise_se = host
                                .get_local_s_keypair()
                                .agree(&bob_e_public)
                                .ok_or(Error::FailedAuthentication)?;

                            let noise_ik_key = Secret(hmac_sha512(
                                session.psk.as_bytes(),
                                &hmac_sha512(
                                    &hmac_sha512(&hmac_sha512(offer.ss_key.as_bytes(), bob_e_public.as_bytes()), noise_ee.as_bytes()),
                                    noise_se.as_bytes(),
                                ),
                            ));

                            let mut c = AesGcm::new(
                                kbkdf512(noise_ik_key.as_bytes(), KBKDF_KEY_USAGE_LABEL_AES_GCM_BOB_TO_ALICE).first_n::<AES_KEY_SIZE>(),
                                false,
                            );
                            c.reset_init_gcm(canonical_header_bytes);
                            c.crypt_in_place(&mut kex_packet[(HEADER_SIZE + 1 + P384_PUBLIC_KEY_SIZE)..payload_end]);
                            if !c.finish_decrypt(&kex_packet[payload_end..aes_gcm_tag_end]) {
                                return Err(Error::FailedAuthentication);
                            }

                            // Alice has now completed Noise_IK with NIST P-384 and verified with GCM auth, but now for hybrid...

                            let (offer_id, bob_session_id, _, _, bob_e1_public_raw, bob_ratchet_key_id) = parse_key_offer_after_header(
                                &kex_packet[(HEADER_SIZE + 1 + P384_PUBLIC_KEY_SIZE)..kex_packet_len],
                                packet_type,
                            )?;

                            if !offer.id.eq(&offer_id) {
                                return Ok(ReceiveResult::Ignored);
                            }

                            let e1e1 = if JEDI && bob_e1_public_raw.len() > 0 && offer.alice_e1_keypair.is_some() {
                                if let Ok(e1e1) = pqc_kyber::decapsulate(bob_e1_public_raw, &offer.alice_e1_keypair.as_ref().unwrap().secret) {
                                    Some(Secret(e1e1))
                                } else {
                                    return Err(Error::FailedAuthentication);
                                }
                            } else {
                                None
                            };

                            let mut ratchet_count = 0;
                            let mut session_key = noise_ik_key;
                            if bob_ratchet_key_id.is_some() && offer.ratchet_key.is_some() {
                                session_key = Secret(hmac_sha512(offer.ratchet_key.as_ref().unwrap().as_bytes(), session_key.as_bytes()));
                                ratchet_count = offer.ratchet_count;
                            }
                            if let Some(e1e1) = e1e1.as_ref() {
                                session_key = Secret(hmac_sha512(e1e1.as_bytes(), session_key.as_bytes()));
                            }

                            if !hmac_sha384_2(
                                kbkdf512(session_key.as_bytes(), KBKDF_KEY_USAGE_LABEL_HMAC).first_n::<48>(),
                                canonical_header_bytes,
                                &kex_packet_saved_ciphertext[HEADER_SIZE..aes_gcm_tag_end],
                            )
                            .eq(&kex_packet[aes_gcm_tag_end..kex_packet_len])
                            {
                                return Err(Error::FailedAuthentication);
                            }

                            // Alice has now completed and validated the full hybrid exchange.

                            let counter = session.send_counter.next();
                            let session_key = SessionKey::new(session_key, Role::Alice, current_time, counter, ratchet_count + 1, e1e1.is_some());

                            let mut reply_buf = [0_u8; HEADER_SIZE + AES_GCM_TAG_SIZE];
                            create_packet_header(
                                &mut reply_buf,
                                HEADER_SIZE + AES_GCM_TAG_SIZE,
                                mtu,
                                PACKET_TYPE_NOP,
                                bob_session_id.into(),
                                counter,
                            )?;

                            let mut c = session_key.get_send_cipher(counter)?;
                            c.reset_init_gcm(CanonicalHeader::make(bob_session_id.into(), PACKET_TYPE_NOP, counter.to_u32()).as_bytes());
                            reply_buf[HEADER_SIZE..].copy_from_slice(&c.finish_encrypt());
                            session_key.return_send_cipher(c);

                            set_header_check_code(&mut reply_buf, &session.header_check_cipher);
                            send(&mut reply_buf);

                            drop(state);
                            let mut state = session.state.write().unwrap();
                            let _ = state.remote_session_id.replace(bob_session_id);
                            let next_key_idx = (state.cur_session_key_idx + 1) % KEY_HISTORY_SIZE;
                            let _ = state.session_keys[next_key_idx].replace(session_key);
                            let _ = state.offer.take();

                            return Ok(ReceiveResult::Ok);
                        }
                    }

                    // Just ignore counter-offers that are out of place. They probably indicate that this side
                    // restarted and needs to establish a new session.
                    return Ok(ReceiveResult::Ignored);
                }

                _ => return Err(Error::InvalidPacket),
            }
        }
    }
}

/// Outgoing packet counter with strictly ordered atomic semantics.
#[repr(transparent)]
struct Counter(AtomicU64);

impl Counter {
    #[inline(always)]
    fn new() -> Self {
        // Using a random value has no security implication. Zero would be fine. This just
        // helps randomize packet contents a bit.
        Self(AtomicU64::new(random::next_u32_secure() as u64))
    }

    /// Get the value most recently used to send a packet.
    #[inline(always)]
    fn previous(&self) -> CounterValue {
        CounterValue(self.0.load(Ordering::SeqCst))
    }

    /// Get a counter value for the next packet being sent.
    #[inline(always)]
    fn next(&self) -> CounterValue {
        CounterValue(self.0.fetch_add(1, Ordering::SeqCst))
    }
}

/// A value of the outgoing packet counter.
///
/// The used portion of the packet counter is the least significant 32 bits, but the internal
/// counter state is kept as a 64-bit integer. This makes it easier to correctly handle
/// key expiration after usage limits are reached without complicated logic to handle 32-bit
/// wrapping. Usage limits are below 2^32 so the actual 32-bit counter will not wrap for a
/// given shared secret key.
#[repr(transparent)]
#[derive(Copy, Clone)]
struct CounterValue(u64);

impl CounterValue {
    #[inline(always)]
    pub fn to_u32(&self) -> u32 {
        self.0 as u32
    }
}

/// "Canonical header" for generating 96-bit AES-GCM nonce and for inclusion in HMACs.
///
/// This is basically the actual header but with fragment count and fragment total set to zero.
/// Fragmentation is not considered when authenticating the entire packet. A separate header
/// check code is used to make fragmentation itself more robust, but that's outside the scope
/// of AEAD authentication.
#[derive(Clone, Copy)]
#[repr(C, packed)]
struct CanonicalHeader(u64, u32);

impl CanonicalHeader {
    #[inline(always)]
    pub fn make(session_id: SessionId, packet_type: u8, counter: u32) -> Self {
        CanonicalHeader(
            (u64::from(session_id) | (packet_type as u64).wrapping_shl(48)).to_le(),
            counter.to_le(),
        )
    }

    #[inline(always)]
    pub fn as_bytes(&self) -> &[u8; 12] {
        memory::as_byte_array(self)
    }
}

/// Alice's KEY_OFFER, remembered so Noise agreement process can resume on KEY_COUNTER_OFFER.
struct EphemeralOffer {
    id: [u8; 16],                                 // Arbitrary random offer ID
    creation_time: i64,                           // Local time when offer was created
    ratchet_count: u64,                           // Ratchet count starting at zero for initial offer
    ratchet_key: Option<Secret<64>>,              // Ratchet key from previous offer
    ss_key: Secret<64>,                           // Shared secret in-progress, at state after offer sent
    alice_e_keypair: P384KeyPair,                 // NIST P-384 key pair (Noise ephemeral key for Alice)
    alice_e1_keypair: Option<pqc_kyber::Keypair>, // Kyber1024 key pair (agreement result mixed post-Noise)
}

/// Create and send an ephemeral offer, returning the EphemeralOffer part that must be saved.
fn send_ephemeral_offer<SendFunction: FnMut(&mut [u8])>(
    send: &mut SendFunction,
    counter: CounterValue,
    alice_session_id: SessionId,
    bob_session_id: Option<SessionId>,
    alice_s_public: &[u8],
    alice_metadata: &[u8],
    bob_s_public_p384: &P384PublicKey,
    bob_s_public_hash: &[u8],
    ss: &Secret<48>,
    current_key: Option<&SessionKey>,
    header_check_cipher: Option<&Aes>, // None to use one based on the recipient's public key for initial contact
    mtu: usize,
    current_time: i64,
) -> Result<Box<EphemeralOffer>, Error> {
    // Generate a NIST P-384 pair.
    let alice_e_keypair = P384KeyPair::generate();

    // Perform key agreement with the other side's static P-384 public key.
    let noise_es = alice_e_keypair.agree(bob_s_public_p384).ok_or(Error::InvalidPacket)?;

    // Generate a Kyber1024 pair if enabled.
    let alice_e1_keypair = if JEDI {
        Some(pqc_kyber::keypair(&mut random::SecureRandom::get()))
    } else {
        None
    };

    // Get ratchet key for current key if one exists.
    let (ratchet_key, ratchet_count) = if let Some(current_key) = current_key {
        (Some(current_key.ratchet_key.clone()), current_key.ratchet_count)
    } else {
        (None, 0)
    };

    // Random ephemeral offer ID
    let id: [u8; 16] = random::get_bytes_secure();

    // Create ephemeral offer packet (not fragmented yet).
    const PACKET_BUF_SIZE: usize = MIN_TRANSPORT_MTU * KEY_EXCHANGE_MAX_FRAGMENTS;
    let mut packet_buf = [0_u8; PACKET_BUF_SIZE];
    let mut packet_len = {
        let mut p = &mut packet_buf[HEADER_SIZE..];

        p.write_all(&[SESSION_PROTOCOL_VERSION])?;
        p.write_all(alice_e_keypair.public_key_bytes())?;

        p.write_all(&id)?;
        p.write_all(&alice_session_id.0.to_le_bytes()[..SESSION_ID_SIZE])?;
        varint::write(&mut p, alice_s_public.len() as u64)?;
        p.write_all(alice_s_public)?;
        varint::write(&mut p, alice_metadata.len() as u64)?;
        p.write_all(alice_metadata)?;
        if let Some(e1kp) = alice_e1_keypair {
            p.write_all(&[E1_TYPE_KYBER1024])?;
            p.write_all(&e1kp.public)?;
        } else {
            p.write_all(&[E1_TYPE_NONE])?;
        }
        if let Some(ratchet_key) = ratchet_key.as_ref() {
            p.write_all(&[0x01])?;
            p.write_all(&secret_fingerprint(ratchet_key.as_bytes())[..16])?;
        } else {
            p.write_all(&[0x00])?;
        }

        PACKET_BUF_SIZE - p.len()
    };

    // Create ephemeral agreement secret.
    let es_key = Secret(hmac_sha512(
        &hmac_sha512(&INITIAL_KEY, alice_e_keypair.public_key_bytes()),
        noise_es.as_bytes(),
    ));

    let bob_session_id = bob_session_id.unwrap_or(SessionId::NIL);
    create_packet_header(&mut packet_buf, packet_len, mtu, PACKET_TYPE_KEY_OFFER, bob_session_id, counter)?;

    let canonical_header = CanonicalHeader::make(bob_session_id, PACKET_TYPE_KEY_OFFER, counter.to_u32());

    // Encrypt packet and attach AES-GCM tag.
    let gcm_tag = {
        let mut c = AesGcm::new(
            kbkdf512(es_key.as_bytes(), KBKDF_KEY_USAGE_LABEL_AES_GCM_ALICE_TO_BOB).first_n::<AES_KEY_SIZE>(),
            true,
        );
        c.reset_init_gcm(canonical_header.as_bytes());
        c.crypt_in_place(&mut packet_buf[(HEADER_SIZE + 1 + P384_PUBLIC_KEY_SIZE)..packet_len]);
        c.finish_encrypt()
    };
    packet_buf[packet_len..(packet_len + AES_GCM_TAG_SIZE)].copy_from_slice(&gcm_tag);
    packet_len += AES_GCM_TAG_SIZE;

    // Mix in static secret.
    let ss_key = Secret(hmac_sha512(es_key.as_bytes(), ss.as_bytes()));
    drop(es_key);

    // HMAC packet using static + ephemeral key.
    let hmac = hmac_sha384_2(
        kbkdf512(ss_key.as_bytes(), KBKDF_KEY_USAGE_LABEL_HMAC).first_n::<48>(),
        canonical_header.as_bytes(),
        &packet_buf[HEADER_SIZE..packet_len],
    );
    packet_buf[packet_len..(packet_len + HMAC_SIZE)].copy_from_slice(&hmac);
    packet_len += HMAC_SIZE;

    // Add secondary HMAC to verify that the caller knows the recipient's full static public identity.
    let hmac = hmac_sha384_2(bob_s_public_hash, canonical_header.as_bytes(), &packet_buf[HEADER_SIZE..packet_len]);
    packet_buf[packet_len..(packet_len + HMAC_SIZE)].copy_from_slice(&hmac);
    packet_len += HMAC_SIZE;

    if let Some(header_check_cipher) = header_check_cipher {
        send_with_fragmentation(send, &mut packet_buf[..packet_len], mtu, header_check_cipher);
    } else {
        send_with_fragmentation(
            send,
            &mut packet_buf[..packet_len],
            mtu,
            &Aes::new(kbkdf512(&bob_s_public_hash, KBKDF_KEY_USAGE_LABEL_HEADER_CHECK).first_n::<HEADER_CHECK_AES_KEY_SIZE>()),
        );
    }

    Ok(Box::new(EphemeralOffer {
        id,
        creation_time: current_time,
        ratchet_count,
        ratchet_key,
        ss_key,
        alice_e_keypair,
        alice_e1_keypair,
    }))
}

/// Populate all but the header check code in the first 16 bytes of a packet or fragment.
#[inline(always)]
fn create_packet_header(
    header: &mut [u8],
    packet_len: usize,
    mtu: usize,
    packet_type: u8,
    recipient_session_id: SessionId,
    counter: CounterValue,
) -> Result<(), Error> {
    let fragment_count = ((packet_len as f32) / (mtu - HEADER_SIZE) as f32).ceil() as usize;

    debug_assert!(header.len() >= HEADER_SIZE);
    debug_assert!(mtu >= MIN_TRANSPORT_MTU);
    debug_assert!(packet_len >= MIN_PACKET_SIZE);
    debug_assert!(fragment_count > 0);
    debug_assert!(fragment_count <= MAX_FRAGMENTS);
    debug_assert!(packet_type <= 0x0f); // packet type is 4 bits

    if fragment_count <= MAX_FRAGMENTS {
        // Header indexed by bit:
        //   [0-31]    counter
        //   [32-63]   header check code (computed later)
        //   [64-111]  recipient's session ID (unique on their side)
        //   [112-115] packet type (0-15)
        //   [116-121] number of fragments (0..63 for 1..64 fragments total)
        //   [122-127] fragment number (0, 1, 2, ...)
        memory::store_raw((counter.to_u32() as u64).to_le(), header);
        memory::store_raw(
            (u64::from(recipient_session_id) | (packet_type as u64).wrapping_shl(48) | ((fragment_count - 1) as u64).wrapping_shl(52))
                .to_le(),
            &mut header[8..],
        );
        Ok(())
    } else {
        unlikely_branch();
        Err(Error::DataTooLarge)
    }
}

/// Break a packet into fragments and send them all.
fn send_with_fragmentation<SendFunction: FnMut(&mut [u8])>(
    send: &mut SendFunction,
    packet: &mut [u8],
    mtu: usize,
    header_check_cipher: &Aes,
) {
    let packet_len = packet.len();
    let mut fragment_start = 0;
    let mut fragment_end = packet_len.min(mtu);
    let mut header: [u8; 16] = packet[..HEADER_SIZE].try_into().unwrap();
    loop {
        let fragment = &mut packet[fragment_start..fragment_end];
        set_header_check_code(fragment, header_check_cipher);
        send(fragment);
        if fragment_end < packet_len {
            debug_assert!(header[15].wrapping_shr(2) < 63);
            header[15] += 0x04; // increment fragment number
            fragment_start = fragment_end - HEADER_SIZE;
            fragment_end = (fragment_start + mtu).min(packet_len);
            packet[fragment_start..(fragment_start + HEADER_SIZE)].copy_from_slice(&header);
        } else {
            debug_assert_eq!(fragment_end, packet_len);
            break;
        }
    }
}

/// Set 32-bit header check code, used to make fragmentation mechanism robust.
#[inline]
fn set_header_check_code(packet: &mut [u8], header_check_cipher: &Aes) {
    debug_assert!(packet.len() >= MIN_PACKET_SIZE);
    let mut check_code = 0u128.to_ne_bytes();
    header_check_cipher.encrypt_block(&packet[8..24], &mut check_code);
    packet[4..8].copy_from_slice(&check_code[..4]);
}

/// Verify 32-bit header check code.
#[inline]
fn verify_header_check_code(packet: &[u8], header_check_cipher: &Aes) -> bool {
    debug_assert!(packet.len() >= MIN_PACKET_SIZE);
    let mut header_mac = 0u128.to_ne_bytes();
    header_check_cipher.encrypt_block(&packet[8..24], &mut header_mac);
    memory::load_raw::<u32>(&packet[4..8]) == memory::load_raw::<u32>(&header_mac)
}

/// Parse KEY_OFFER and KEY_COUNTER_OFFER starting after the unencrypted public key part.
fn parse_key_offer_after_header(
    incoming_packet: &[u8],
    packet_type: u8,
) -> Result<([u8; 16], SessionId, &[u8], &[u8], &[u8], Option<[u8; 16]>), Error> {
    let mut p = &incoming_packet[..];
    let mut offer_id = [0_u8; 16];
    p.read_exact(&mut offer_id)?;
    let alice_session_id = SessionId::new_from_reader(&mut p)?;
    if alice_session_id.is_none() {
        return Err(Error::InvalidPacket);
    }
    let alice_session_id = alice_session_id.unwrap();
    let alice_s_public_len = varint::read(&mut p)?.0;
    if (p.len() as u64) < alice_s_public_len {
        return Err(Error::InvalidPacket);
    }
    let alice_s_public = &p[..(alice_s_public_len as usize)];
    p = &p[(alice_s_public_len as usize)..];
    let alice_metadata_len = varint::read(&mut p)?.0;
    if (p.len() as u64) < alice_metadata_len {
        return Err(Error::InvalidPacket);
    }
    let alice_metadata = &p[..(alice_metadata_len as usize)];
    p = &p[(alice_metadata_len as usize)..];
    if p.is_empty() {
        return Err(Error::InvalidPacket);
    }
    let alice_e1_public = match p[0] {
        E1_TYPE_KYBER1024 => {
            if packet_type == PACKET_TYPE_KEY_OFFER {
                if p.len() < (pqc_kyber::KYBER_PUBLICKEYBYTES + 1) {
                    return Err(Error::InvalidPacket);
                }
                let e1p = &p[1..(pqc_kyber::KYBER_PUBLICKEYBYTES + 1)];
                p = &p[(pqc_kyber::KYBER_PUBLICKEYBYTES + 1)..];
                e1p
            } else {
                if p.len() < (pqc_kyber::KYBER_CIPHERTEXTBYTES + 1) {
                    return Err(Error::InvalidPacket);
                }
                let e1p = &p[1..(pqc_kyber::KYBER_CIPHERTEXTBYTES + 1)];
                p = &p[(pqc_kyber::KYBER_CIPHERTEXTBYTES + 1)..];
                e1p
            }
        }
        _ => &[],
    };
    if p.is_empty() {
        return Err(Error::InvalidPacket);
    }
    let alice_ratchet_key_fingerprint = if p[0] == 0x01 {
        if p.len() < 16 {
            return Err(Error::InvalidPacket);
        }
        Some(p[1..17].try_into().unwrap())
    } else {
        None
    };
    Ok((
        offer_id,
        alice_session_id,
        alice_s_public,
        alice_metadata,
        alice_e1_public,
        alice_ratchet_key_fingerprint,
    ))
}

/// Was this side the one who sent the first offer (Alice) or countered (Bob).
/// Note that role is not fixed. Either side can take either role. It's just who
/// initiated first.
enum Role {
    Alice,
    Bob,
}

/// Key lifetime manager state and logic (separate to spotlight and keep clean)
struct KeyLifetime {
    rekey_at_or_after_counter: u64,
    hard_expire_at_counter: u64,
    rekey_at_or_after_timestamp: i64,
}

impl KeyLifetime {
    fn new(current_counter: CounterValue, current_time: i64) -> Self {
        Self {
            rekey_at_or_after_counter: current_counter.0
                + REKEY_AFTER_USES
                + (random::next_u32_secure() % REKEY_AFTER_USES_MAX_JITTER) as u64,
            hard_expire_at_counter: current_counter.0 + EXPIRE_AFTER_USES,
            rekey_at_or_after_timestamp: current_time
                + REKEY_AFTER_TIME_MS
                + (random::next_u32_secure() % REKEY_AFTER_TIME_MS_MAX_JITTER) as i64,
        }
    }

    #[inline(always)]
    fn should_rekey(&self, counter: CounterValue, current_time: i64) -> bool {
        counter.0 >= self.rekey_at_or_after_counter || current_time >= self.rekey_at_or_after_timestamp
    }

    #[inline(always)]
    fn expired(&self, counter: CounterValue) -> bool {
        counter.0 >= self.hard_expire_at_counter
    }
}

/// A shared symmetric session key.
struct SessionKey {
    secret_fingerprint: [u8; 16],                 // First 128 bits of a SHA384 computed from the secret
    establish_time: i64,                          // Time session key was established
    establish_counter: u64,                       // Counter value at which session was established
    lifetime: KeyLifetime,                        // Key expiration time and counter
    ratchet_key: Secret<64>,                      // Ratchet key for deriving the next session key
    receive_key: Secret<AES_KEY_SIZE>,            // Receive side AES-GCM key
    send_key: Secret<AES_KEY_SIZE>,               // Send side AES-GCM key
    receive_cipher_pool: Mutex<Vec<Box<AesGcm>>>, // Pool of initialized sending ciphers
    send_cipher_pool: Mutex<Vec<Box<AesGcm>>>,    // Pool of initialized receiving ciphers
    ratchet_count: u64,                           // Number of new keys negotiated in this session
    jedi: bool,                                   // True if Kyber1024 was used (both sides enabled)
}

impl SessionKey {
    /// Create a new symmetric shared session key and set its key expiration times, etc.
    fn new(key: Secret<64>, role: Role, current_time: i64, current_counter: CounterValue, ratchet_count: u64, jedi: bool) -> Self {
        let a2b: Secret<AES_KEY_SIZE> = kbkdf512(key.as_bytes(), KBKDF_KEY_USAGE_LABEL_AES_GCM_ALICE_TO_BOB).first_n_clone();
        let b2a: Secret<AES_KEY_SIZE> = kbkdf512(key.as_bytes(), KBKDF_KEY_USAGE_LABEL_AES_GCM_BOB_TO_ALICE).first_n_clone();
        let (receive_key, send_key) = match role {
            Role::Alice => (b2a, a2b),
            Role::Bob => (a2b, b2a),
        };
        Self {
            secret_fingerprint: secret_fingerprint(key.as_bytes())[..16].try_into().unwrap(),
            establish_time: current_time,
            establish_counter: current_counter.0,
            lifetime: KeyLifetime::new(current_counter, current_time),
            ratchet_key: kbkdf512(key.as_bytes(), KBKDF_KEY_USAGE_LABEL_RATCHETING),
            receive_key,
            send_key,
            receive_cipher_pool: Mutex::new(Vec::with_capacity(2)),
            send_cipher_pool: Mutex::new(Vec::with_capacity(2)),
            ratchet_count,
            jedi,
        }
    }

    #[inline]
    fn get_send_cipher(&self, counter: CounterValue) -> Result<Box<AesGcm>, Error> {
        if !self.lifetime.expired(counter) {
            Ok(self
                .send_cipher_pool
                .lock()
                .unwrap()
                .pop()
                .unwrap_or_else(|| Box::new(AesGcm::new(self.send_key.as_bytes(), true))))
        } else {
            // Not only do we return an error, but we also destroy the key.
            let mut scp = self.send_cipher_pool.lock().unwrap();
            scp.clear();
            self.send_key.nuke();

            Err(Error::MaxKeyLifetimeExceeded)
        }
    }

    #[inline]
    fn return_send_cipher(&self, c: Box<AesGcm>) {
        self.send_cipher_pool.lock().unwrap().push(c);
    }

    #[inline]
    fn get_receive_cipher(&self) -> Box<AesGcm> {
        self.receive_cipher_pool
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| Box::new(AesGcm::new(self.receive_key.as_bytes(), false)))
    }

    #[inline]
    fn return_receive_cipher(&self, c: Box<AesGcm>) {
        self.receive_cipher_pool.lock().unwrap().push(c);
    }
}

/// Shortcut to HMAC data split into two slices.
fn hmac_sha384_2(key: &[u8], a: &[u8], b: &[u8]) -> [u8; 48] {
    let mut hmac = HMACSHA384::new(key);
    hmac.update(a);
    hmac.update(b);
    hmac.finish()
}

/// HMAC-SHA512 key derivation function modeled on: https://csrc.nist.gov/publications/detail/sp/800-108/final (page 12)
/// Cryptographically this isn't really different from HMAC(key, [label]) with just one byte.
fn kbkdf512(key: &[u8], label: u8) -> Secret<64> {
    Secret(hmac_sha512(key, &[0, 0, 0, 0, b'Z', b'T', label, 0, 0, 0, 0, 0x02, 0x00]))
}

/// Get a hash of a secret key that can be used as a public fingerprint.
fn secret_fingerprint(key: &[u8]) -> [u8; 48] {
    let mut tmp = SHA384::new();
    tmp.update("fp".as_bytes());
    tmp.update(key);
    tmp.finish()
}

#[cfg(test)]
mod tests {
    use std::collections::LinkedList;
    use std::sync::{Arc, Mutex};
    use zerotier_utils::hex;

    #[allow(unused_imports)]
    use super::*;

    struct TestHost {
        local_s: P384KeyPair,
        local_s_hash: [u8; 48],
        psk: Secret<64>,
        session: Mutex<Option<Arc<Session<Box<TestHost>>>>>,
        session_id_counter: Mutex<u64>,
        queue: Mutex<LinkedList<Vec<u8>>>,
        key_id: Mutex<[u8; 16]>,
        this_name: &'static str,
        other_name: &'static str,
    }

    impl TestHost {
        fn new(psk: Secret<64>, this_name: &'static str, other_name: &'static str) -> Self {
            let local_s = P384KeyPair::generate();
            let local_s_hash = SHA384::hash(local_s.public_key_bytes());
            Self {
                local_s,
                local_s_hash,
                psk,
                session: Mutex::new(None),
                session_id_counter: Mutex::new(1),
                queue: Mutex::new(LinkedList::new()),
                key_id: Mutex::new([0; 16]),
                this_name,
                other_name,
            }
        }
    }

    impl ApplicationLayer for Box<TestHost> {
        type SessionUserData = u32;
        type SessionRef = Arc<Session<Box<TestHost>>>;
        type IncomingPacketBuffer = Vec<u8>;
        type RemoteAddress = u32;

        const REKEY_RATE_LIMIT_MS: i64 = 0;

        fn get_local_s_public_raw(&self) -> &[u8] {
            self.local_s.public_key_bytes()
        }

        fn get_local_s_public_hash(&self) -> &[u8; 48] {
            &self.local_s_hash
        }

        fn get_local_s_keypair(&self) -> &P384KeyPair {
            &self.local_s
        }

        fn extract_s_public_from_raw(static_public: &[u8]) -> Option<P384PublicKey> {
            P384PublicKey::from_bytes(static_public)
        }

        fn lookup_session(&self, local_session_id: SessionId) -> Option<Self::SessionRef> {
            self.session.lock().unwrap().as_ref().and_then(|s| {
                if s.id == local_session_id {
                    Some(s.clone())
                } else {
                    None
                }
            })
        }

        fn check_new_session(&self, _: &ReceiveContext<Self>, _: &Self::RemoteAddress) -> bool {
            true
        }

        fn accept_new_session(
            &self,
            _: &ReceiveContext<Self>,
            _: &u32,
            _: &[u8],
            _: &[u8],
        ) -> Option<(SessionId, Secret<64>, Self::SessionUserData)> {
            loop {
                let mut new_id = self.session_id_counter.lock().unwrap();
                *new_id += 1;
                return Some((SessionId::new_from_u64(*new_id).unwrap(), self.psk.clone(), 0));
            }
        }
    }

    #[allow(unused_variables)]
    #[test]
    fn establish_session() {
        let mut data_buf = [0_u8; (1280 - 32) * MAX_FRAGMENTS];
        let mut mtu_buffer = [0_u8; 1280];
        let mut psk: Secret<64> = Secret::default();
        random::fill_bytes_secure(&mut psk.0);

        let alice_host = Box::new(TestHost::new(psk.clone(), "alice", "bob"));
        let bob_host = Box::new(TestHost::new(psk.clone(), "bob", "alice"));
        let alice_rc: Box<ReceiveContext<Box<TestHost>>> = Box::new(ReceiveContext::new(&alice_host));
        let bob_rc: Box<ReceiveContext<Box<TestHost>>> = Box::new(ReceiveContext::new(&bob_host));

        //println!("zssp: size of session (bytes): {}", std::mem::size_of::<Session<Box<TestHost>>>());

        let _ = alice_host.session.lock().unwrap().insert(Arc::new(
            Session::start_new(
                &alice_host,
                |data| bob_host.queue.lock().unwrap().push_front(data.to_vec()),
                SessionId::new_random(),
                bob_host.local_s.public_key_bytes(),
                &[],
                &psk,
                1,
                mtu_buffer.len(),
                1,
            )
            .unwrap(),
        ));

        let mut ts = 0;
        for test_loop in 0..256 {
            for host in [&alice_host, &bob_host] {
                let send_to_other = |data: &mut [u8]| {
                    if std::ptr::eq(host, &alice_host) {
                        bob_host.queue.lock().unwrap().push_front(data.to_vec());
                    } else {
                        alice_host.queue.lock().unwrap().push_front(data.to_vec());
                    }
                };

                let rc = if std::ptr::eq(host, &alice_host) {
                    &alice_rc
                } else {
                    &bob_rc
                };

                loop {
                    if let Some(qi) = host.queue.lock().unwrap().pop_back() {
                        let qi_len = qi.len();
                        ts += 1;
                        let r = rc.receive(host, &0, send_to_other, &mut data_buf, qi, mtu_buffer.len(), ts);
                        if r.is_ok() {
                            let r = r.unwrap();
                            match r {
                                ReceiveResult::Ok => {
                                    //println!("zssp: {} => {} ({}): Ok", host.other_name, host.this_name, qi_len);
                                }
                                ReceiveResult::OkData(data) => {
                                    //println!("zssp: {} => {} ({}): OkData length=={}", host.other_name, host.this_name, qi_len, data.len());
                                    assert!(!data.iter().any(|x| *x != 0x12));
                                }
                                ReceiveResult::OkNewSession(new_session) => {
                                    println!(
                                        "zssp: {} => {} ({}): OkNewSession ({})",
                                        host.other_name,
                                        host.this_name,
                                        qi_len,
                                        u64::from(new_session.id)
                                    );
                                    let mut hs = host.session.lock().unwrap();
                                    assert!(hs.is_none());
                                    let _ = hs.insert(Arc::new(new_session));
                                }
                                ReceiveResult::Ignored => {
                                    println!("zssp: {} => {} ({}): Ignored", host.other_name, host.this_name, qi_len);
                                }
                            }
                        } else {
                            println!(
                                "zssp: {} => {} ({}): error: {}",
                                host.other_name,
                                host.this_name,
                                qi_len,
                                r.err().unwrap().to_string()
                            );
                            panic!();
                        }
                    } else {
                        break;
                    }
                }

                data_buf.fill(0x12);
                if let Some(session) = host.session.lock().unwrap().as_ref().cloned() {
                    if session.established() {
                        {
                            let mut key_id = host.key_id.lock().unwrap();
                            let security_info = session.status().unwrap();
                            if !security_info.0.eq(key_id.as_ref()) {
                                *key_id = security_info.0;
                                println!(
                                    "zssp: new key at {}: fingerprint {} ratchet {} kyber {}",
                                    host.this_name,
                                    hex::to_string(key_id.as_ref()),
                                    security_info.2,
                                    security_info.3
                                );
                            }
                        }
                        for _ in 0..4 {
                            assert!(session
                                .send(
                                    send_to_other,
                                    &mut mtu_buffer,
                                    &data_buf[..((random::xorshift64_random() as usize) % data_buf.len())]
                                )
                                .is_ok());
                        }
                        if (test_loop % 8) == 0 && test_loop >= 8 && host.this_name.eq("alice") {
                            session.service(host, send_to_other, &[], mtu_buffer.len(), test_loop as i64, true);
                        }
                    }
                }
            }
        }
    }
}
