/*
 * Copyright (c)2013-2021 ZeroTier, Inc.
 *
 * Use of this software is governed by the Business Source License included
 * in the LICENSE.TXT file in the project's root directory.
 *
 * Change Date: 2026-01-01
 *
 * On the date above, in accordance with the Business Source License, use
 * of this software will be governed by version 2.0 of the Apache License.
 */
/****/

#include "VL1.hpp"

#include "AES.hpp"
#include "Context.hpp"
#include "Expect.hpp"
#include "Identity.hpp"
#include "LZ4.hpp"
#include "Path.hpp"
#include "Peer.hpp"
#include "Poly1305.hpp"
#include "SHA512.hpp"
#include "Salsa20.hpp"
#include "SelfAwareness.hpp"
#include "Topology.hpp"
#include "VL2.hpp"

namespace ZeroTier {

namespace {

ZT_INLINE const Identity &identityFromPeerPtr(const SharedPtr<Peer> &p) { return (p) ? p->identity() : Identity::NIL; }

struct p_SalsaPolyCopyFunction {
    Salsa20 s20;
    Poly1305 poly1305;
    unsigned int hdrRemaining;

    ZT_INLINE p_SalsaPolyCopyFunction(const void *salsaKey, const void *salsaIv) : s20(salsaKey, salsaIv), poly1305(), hdrRemaining(ZT_PROTO_PACKET_ENCRYPTED_SECTION_START)
    {
        uint8_t macKey[ZT_POLY1305_KEY_SIZE];
        s20.crypt12(Utils::ZERO256, macKey, ZT_POLY1305_KEY_SIZE);
        poly1305.init(macKey);
    }

    ZT_INLINE void operator()(void *dest, const void *src, unsigned int len) noexcept
    {
        if (hdrRemaining != 0) {
            unsigned int hdrBytes = (len > hdrRemaining) ? hdrRemaining : len;
            Utils::copy(dest, src, hdrBytes);
            hdrRemaining -= hdrBytes;
            dest = reinterpret_cast<uint8_t *>(dest) + hdrBytes;
            src  = reinterpret_cast<const uint8_t *>(src) + hdrBytes;
            len -= hdrBytes;
        }
        poly1305.update(src, len);
        s20.crypt12(src, dest, len);
    }
};

struct p_PolyCopyFunction {
    Poly1305 poly1305;
    unsigned int hdrRemaining;

    ZT_INLINE p_PolyCopyFunction(const void *salsaKey, const void *salsaIv) : poly1305(), hdrRemaining(ZT_PROTO_PACKET_ENCRYPTED_SECTION_START)
    {
        uint8_t macKey[ZT_POLY1305_KEY_SIZE];
        Salsa20(salsaKey, salsaIv).crypt12(Utils::ZERO256, macKey, ZT_POLY1305_KEY_SIZE);
        poly1305.init(macKey);
    }

    ZT_INLINE void operator()(void *dest, const void *src, unsigned int len) noexcept
    {
        if (hdrRemaining != 0) {
            unsigned int hdrBytes = (len > hdrRemaining) ? hdrRemaining : len;
            Utils::copy(dest, src, hdrBytes);
            hdrRemaining -= hdrBytes;
            dest = reinterpret_cast<uint8_t *>(dest) + hdrBytes;
            src  = reinterpret_cast<const uint8_t *>(src) + hdrBytes;
            len -= hdrBytes;
        }
        poly1305.update(src, len);
        Utils::copy(dest, src, len);
    }
};

}   // anonymous namespace

VL1::VL1(const Context &ctx) : m_ctx(ctx) {}

void VL1::onRemotePacket(CallContext &cc, const int64_t localSocket, const InetAddress &fromAddr, SharedPtr<Buf> &data, const unsigned int len) noexcept
{
    const SharedPtr<Path> path(m_ctx.topology->path(localSocket, fromAddr));

    ZT_SPEW("%u bytes from %s (local socket %lld)", len, fromAddr.toString().c_str(), localSocket);
    path->received(cc, len);

    // NOTE: likely/unlikely are used here to highlight the most common code path
    // for valid data packets. This may allow the compiler to generate very slightly
    // faster code for that path.

    try {
        if (unlikely(len < ZT_PROTO_MIN_FRAGMENT_LENGTH))
            return;

        static_assert((ZT_PROTO_PACKET_ID_INDEX + sizeof(uint64_t)) < ZT_PROTO_MIN_FRAGMENT_LENGTH, "overflow");
        const uint64_t packetId = Utils::loadMachineEndian<uint64_t>(data->unsafeData + ZT_PROTO_PACKET_ID_INDEX);

        static_assert((ZT_PROTO_PACKET_DESTINATION_INDEX + ZT_ADDRESS_LENGTH) < ZT_PROTO_MIN_FRAGMENT_LENGTH, "overflow");
        const Address destination(data->unsafeData + ZT_PROTO_PACKET_DESTINATION_INDEX);
        if (destination != m_ctx.identity.address()) {
            m_relay(cc, path, destination, data, len);
            return;
        }

        // ----------------------------------------------------------------------------------------------------------------
        // If we made it this far, the packet is at least MIN_FRAGMENT_LENGTH and is addressed to this node's ZT address
        // ----------------------------------------------------------------------------------------------------------------

        Buf::PacketVector pktv;

        static_assert(ZT_PROTO_PACKET_FRAGMENT_INDICATOR_INDEX <= ZT_PROTO_MIN_FRAGMENT_LENGTH, "overflow");
        if (data->unsafeData[ZT_PROTO_PACKET_FRAGMENT_INDICATOR_INDEX] == ZT_PROTO_PACKET_FRAGMENT_INDICATOR) {
            // This looks like a fragment (excluding the head) of a larger packet.
            static_assert(ZT_PROTO_PACKET_FRAGMENT_COUNTS < ZT_PROTO_MIN_FRAGMENT_LENGTH, "overflow");
            const unsigned int totalFragments = (data->unsafeData[ZT_PROTO_PACKET_FRAGMENT_COUNTS] >> 4U) & 0x0fU;
            const unsigned int fragmentNo     = data->unsafeData[ZT_PROTO_PACKET_FRAGMENT_COUNTS] & 0x0fU;
            switch (m_inputPacketAssembler.assemble(packetId, pktv, data, ZT_PROTO_PACKET_FRAGMENT_PAYLOAD_START_AT, len - ZT_PROTO_PACKET_FRAGMENT_PAYLOAD_START_AT, fragmentNo, totalFragments, cc.ticks, path)) {
                case Defragmenter<ZT_MAX_PACKET_FRAGMENTS>::COMPLETE: break;
                default:
                    // case Defragmenter<ZT_MAX_PACKET_FRAGMENTS>::OK:
                    // case Defragmenter<ZT_MAX_PACKET_FRAGMENTS>::ERR_DUPLICATE_FRAGMENT:
                    // case Defragmenter<ZT_MAX_PACKET_FRAGMENTS>::ERR_INVALID_FRAGMENT:
                    // case Defragmenter<ZT_MAX_PACKET_FRAGMENTS>::ERR_TOO_MANY_FRAGMENTS_FOR_PATH:
                    // case Defragmenter<ZT_MAX_PACKET_FRAGMENTS>::ERR_OUT_OF_MEMORY:
                    return;
            }
        }
        else {
            if (unlikely(len < ZT_PROTO_MIN_PACKET_LENGTH))
                return;
            static_assert(ZT_PROTO_PACKET_FLAGS_INDEX < ZT_PROTO_MIN_PACKET_LENGTH, "overflow");
            if ((data->unsafeData[ZT_PROTO_PACKET_FLAGS_INDEX] & ZT_PROTO_FLAG_FRAGMENTED) != 0) {
                // This is the head of a series of fragments that we may or may not already have.
                switch (m_inputPacketAssembler.assemble(
                    packetId, pktv, data,
                    0,   // fragment index is 0 since this is the head
                    len,
                    0,   // always the zero'eth fragment
                    0,   // this is specified in fragments, not in the head
                    cc.ticks, path)) {
                    case Defragmenter<ZT_MAX_PACKET_FRAGMENTS>::COMPLETE: break;
                    default:
                        // case Defragmenter<ZT_MAX_PACKET_FRAGMENTS>::OK:
                        // case Defragmenter<ZT_MAX_PACKET_FRAGMENTS>::ERR_DUPLICATE_FRAGMENT:
                        // case Defragmenter<ZT_MAX_PACKET_FRAGMENTS>::ERR_INVALID_FRAGMENT:
                        // case Defragmenter<ZT_MAX_PACKET_FRAGMENTS>::ERR_TOO_MANY_FRAGMENTS_FOR_PATH:
                        // case Defragmenter<ZT_MAX_PACKET_FRAGMENTS>::ERR_OUT_OF_MEMORY:
                        return;
                }
            }
            else {
                // This is a single whole packet with no fragments.
                Buf::Slice s = pktv.push();
                s.b.swap(data);
                s.s = 0;
                s.e = len;
            }
        }

        // ----------------------------------------------------------------------------------------------------------------
        // If we made it this far without returning, a packet is fully assembled and ready to process.
        // ----------------------------------------------------------------------------------------------------------------

        const uint8_t *const hdr = pktv[0].b->unsafeData + pktv[0].s;
        static_assert((ZT_PROTO_PACKET_SOURCE_INDEX + ZT_ADDRESS_LENGTH) < ZT_PROTO_MIN_PACKET_LENGTH, "overflow");
        const Address source(hdr + ZT_PROTO_PACKET_SOURCE_INDEX);
        static_assert(ZT_PROTO_PACKET_FLAGS_INDEX < ZT_PROTO_MIN_PACKET_LENGTH, "overflow");
        const uint8_t hops   = hdr[ZT_PROTO_PACKET_FLAGS_INDEX] & ZT_PROTO_FLAG_FIELD_HOPS_MASK;
        const uint8_t cipher = (hdr[ZT_PROTO_PACKET_FLAGS_INDEX] >> 3U) & 3U;

        SharedPtr<Buf> pkt(new Buf());
        int pktSize = 0;

        static_assert(ZT_PROTO_PACKET_VERB_INDEX < ZT_PROTO_MIN_PACKET_LENGTH, "overflow");
        if (unlikely(((cipher == ZT_PROTO_CIPHER_POLY1305_NONE) || (cipher == ZT_PROTO_CIPHER_NONE)) && ((hdr[ZT_PROTO_PACKET_VERB_INDEX] & ZT_PROTO_VERB_MASK) == Protocol::VERB_HELLO))) {
            // Handle unencrypted HELLO packets.
            pktSize = pktv.mergeCopy(*pkt);
            if (unlikely(pktSize < ZT_PROTO_MIN_PACKET_LENGTH)) {
                ZT_SPEW("discarding packet %.16llx from %s(%s): assembled packet size: %d", packetId, source.toString().c_str(), fromAddr.toString().c_str(), pktSize);
                return;
            }
            const SharedPtr<Peer> peer(m_HELLO(cc, path, *pkt, pktSize));
            if (likely(peer))
                peer->received(m_ctx, cc, path, hops, packetId, pktSize - ZT_PROTO_PACKET_PAYLOAD_START, Protocol::VERB_HELLO, Protocol::VERB_NOP);
            return;
        }

        // This remains zero if authentication fails. Otherwise it gets set to a bit mask
        // indicating authentication and other security flags like encryption and forward
        // secrecy status.
        unsigned int auth = 0;

        SharedPtr<Peer> peer(m_ctx.topology->peer(cc, source));
        if (likely(peer)) {
            switch (cipher) {
                case ZT_PROTO_CIPHER_POLY1305_NONE: {
                    uint8_t perPacketKey[ZT_SALSA20_KEY_SIZE];
                    Protocol::salsa2012DeriveKey(peer->rawIdentityKey(), perPacketKey, *pktv[0].b, pktv.totalSize());
                    p_PolyCopyFunction s20cf(perPacketKey, &packetId);

                    pktSize = pktv.mergeMap<p_PolyCopyFunction &>(*pkt, ZT_PROTO_PACKET_ENCRYPTED_SECTION_START, s20cf);
                    if (unlikely(pktSize < ZT_PROTO_MIN_PACKET_LENGTH)) {
                        ZT_SPEW("discarding packet %.16llx from %s(%s): assembled packet size: %d", packetId, source.toString().c_str(), fromAddr.toString().c_str(), pktSize);
                        return;
                    }

                    uint64_t mac[2];
                    s20cf.poly1305.finish(mac);
                    static_assert((ZT_PROTO_PACKET_MAC_INDEX + 8) < ZT_PROTO_MIN_PACKET_LENGTH, "overflow");
                    if (unlikely(Utils::loadMachineEndian<uint64_t>(hdr + ZT_PROTO_PACKET_MAC_INDEX) != mac[0])) {
                        ZT_SPEW("discarding packet %.16llx from %s(%s): packet MAC failed (none/poly1305)", packetId, source.toString().c_str(), fromAddr.toString().c_str());
                        m_ctx.t->incomingPacketDropped(cc, 0xcc89c812, packetId, 0, peer->identity(), path->address(), hops, Protocol::VERB_NOP, ZT_TRACE_PACKET_DROP_REASON_MAC_FAILED);
                        return;
                    }

                    auth = ZT_VL1_AUTH_RESULT_FLAG_AUTHENTICATED;
                } break;

                case ZT_PROTO_CIPHER_POLY1305_SALSA2012: {
                    uint8_t perPacketKey[ZT_SALSA20_KEY_SIZE];
                    Protocol::salsa2012DeriveKey(peer->rawIdentityKey(), perPacketKey, *pktv[0].b, pktv.totalSize());
                    p_SalsaPolyCopyFunction s20cf(perPacketKey, &packetId);

                    pktSize = pktv.mergeMap<p_SalsaPolyCopyFunction &>(*pkt, ZT_PROTO_PACKET_ENCRYPTED_SECTION_START, s20cf);
                    if (unlikely(pktSize < ZT_PROTO_MIN_PACKET_LENGTH)) {
                        ZT_SPEW("discarding packet %.16llx from %s(%s): assembled packet size: %d", packetId, source.toString().c_str(), fromAddr.toString().c_str(), pktSize);
                        return;
                    }

                    uint64_t mac[2];
                    s20cf.poly1305.finish(mac);
                    static_assert((ZT_PROTO_PACKET_MAC_INDEX + 8) < ZT_PROTO_MIN_PACKET_LENGTH, "overflow");
                    if (unlikely(Utils::loadMachineEndian<uint64_t>(hdr + ZT_PROTO_PACKET_MAC_INDEX) != mac[0])) {
                        ZT_SPEW("discarding packet %.16llx from %s(%s): packet MAC failed (salsa/poly1305)", packetId, source.toString().c_str(), fromAddr.toString().c_str());
                        m_ctx.t->incomingPacketDropped(cc, 0xcc89c812, packetId, 0, peer->identity(), path->address(), hops, Protocol::VERB_NOP, ZT_TRACE_PACKET_DROP_REASON_MAC_FAILED);
                        return;
                    }

                    auth = ZT_VL1_AUTH_RESULT_FLAG_AUTHENTICATED | ZT_VL1_AUTH_RESULT_FLAG_ENCRYPTED;
                } break;

                case ZT_PROTO_CIPHER_NONE: {
                    // TODO
                } break;

                case ZT_PROTO_CIPHER_AES_GMAC_SIV: {
                    // TODO
                } break;

                default: m_ctx.t->incomingPacketDropped(cc, 0x5b001099, packetId, 0, identityFromPeerPtr(peer), path->address(), hops, Protocol::VERB_NOP, ZT_TRACE_PACKET_DROP_REASON_INVALID_OBJECT); return;
            }
        }

        if (likely(auth != 0)) {
            // If authentication was successful go on and process the packet.

            if (unlikely(pktSize < ZT_PROTO_MIN_PACKET_LENGTH)) {
                ZT_SPEW(
                    "discarding packet %.16llx from %s(%s): assembled packet size %d is smaller than minimum packet "
                    "length",
                    packetId, source.toString().c_str(), fromAddr.toString().c_str(), pktSize);
                return;
            }

            // TODO: should take instance ID into account here once that is fully implemented.
            if (unlikely(peer->deduplicateIncomingPacket(packetId))) {
                ZT_SPEW("discarding packet %.16llx from %s(%s): duplicate!", packetId, source.toString().c_str(), fromAddr.toString().c_str());
                return;
            }

            static_assert(ZT_PROTO_PACKET_VERB_INDEX < ZT_PROTO_MIN_PACKET_LENGTH, "overflow");
            const uint8_t verbFlags   = pkt->unsafeData[ZT_PROTO_PACKET_VERB_INDEX];
            const Protocol::Verb verb = (Protocol::Verb)(verbFlags & ZT_PROTO_VERB_MASK);

            // Decompress packet payload if compressed. For additional safety decompression is
            // only performed on packets whose MACs have already been validated. (Only HELLO is
            // sent without this, and HELLO doesn't benefit from compression.)
            if (((verbFlags & ZT_PROTO_VERB_FLAG_COMPRESSED) != 0) && (pktSize > ZT_PROTO_PACKET_PAYLOAD_START)) {
                SharedPtr<Buf> dec(new Buf());
                Utils::copy<ZT_PROTO_PACKET_PAYLOAD_START>(dec->unsafeData, pkt->unsafeData);
                const int uncompressedLen = LZ4_decompress_safe(reinterpret_cast<const char *>(pkt->unsafeData + ZT_PROTO_PACKET_PAYLOAD_START), reinterpret_cast<char *>(dec->unsafeData + ZT_PROTO_PACKET_PAYLOAD_START), pktSize - ZT_PROTO_PACKET_PAYLOAD_START, ZT_BUF_MEM_SIZE - ZT_PROTO_PACKET_PAYLOAD_START);
                if (likely((uncompressedLen >= 0) && (uncompressedLen <= (ZT_BUF_MEM_SIZE - ZT_PROTO_PACKET_PAYLOAD_START)))) {
                    pkt.swap(dec);
                    ZT_SPEW("decompressed packet: %d -> %d", pktSize, ZT_PROTO_PACKET_PAYLOAD_START + uncompressedLen);
                    pktSize = ZT_PROTO_PACKET_PAYLOAD_START + uncompressedLen;
                }
                else {
                    m_ctx.t->incomingPacketDropped(cc, 0xee9e4392, packetId, 0, identityFromPeerPtr(peer), path->address(), hops, verb, ZT_TRACE_PACKET_DROP_REASON_INVALID_COMPRESSED_DATA);
                    return;
                }
            }

            ZT_SPEW("%s from %s(%s) (%d bytes)", Protocol::verbName(verb), source.toString().c_str(), fromAddr.toString().c_str(), pktSize);

            // NOTE: HELLO is normally sent in the clear (in terms of our usual AEAD modes) and is handled
            // above. We will try to process it here, but if so it'll still get re-authenticated via HELLO's
            // own internal authentication logic as usual. It would be abnormal to make it here with HELLO
            // but not invalid.

            Protocol::Verb inReVerb = Protocol::VERB_NOP;
            bool ok                 = true;
            switch (verb) {
                case Protocol::VERB_NOP: break;
                case Protocol::VERB_HELLO: ok = (bool)(m_HELLO(cc, path, *pkt, pktSize)); break;
                case Protocol::VERB_ERROR: ok = m_ERROR(cc, packetId, auth, path, peer, *pkt, pktSize, inReVerb); break;
                case Protocol::VERB_OK: ok = m_OK(cc, packetId, auth, path, peer, *pkt, pktSize, inReVerb); break;
                case Protocol::VERB_WHOIS: ok = m_WHOIS(cc, packetId, auth, path, peer, *pkt, pktSize); break;
                case Protocol::VERB_RENDEZVOUS: ok = m_RENDEZVOUS(cc, packetId, auth, path, peer, *pkt, pktSize); break;
                case Protocol::VERB_FRAME: ok = m_ctx.vl2->m_FRAME(cc, packetId, auth, path, peer, *pkt, pktSize); break;
                case Protocol::VERB_EXT_FRAME: ok = m_ctx.vl2->m_EXT_FRAME(cc, packetId, auth, path, peer, *pkt, pktSize); break;
                case Protocol::VERB_ECHO: ok = m_ECHO(cc, packetId, auth, path, peer, *pkt, pktSize); break;
                case Protocol::VERB_MULTICAST_LIKE: ok = m_ctx.vl2->m_MULTICAST_LIKE(cc, packetId, auth, path, peer, *pkt, pktSize); break;
                case Protocol::VERB_NETWORK_CREDENTIALS: ok = m_ctx.vl2->m_NETWORK_CREDENTIALS(cc, packetId, auth, path, peer, *pkt, pktSize); break;
                case Protocol::VERB_NETWORK_CONFIG_REQUEST: ok = m_ctx.vl2->m_NETWORK_CONFIG_REQUEST(cc, packetId, auth, path, peer, *pkt, pktSize); break;
                case Protocol::VERB_NETWORK_CONFIG: ok = m_ctx.vl2->m_NETWORK_CONFIG(cc, packetId, auth, path, peer, *pkt, pktSize); break;
                case Protocol::VERB_MULTICAST_GATHER: ok = m_ctx.vl2->m_MULTICAST_GATHER(cc, packetId, auth, path, peer, *pkt, pktSize); break;
                case Protocol::VERB_MULTICAST_FRAME_deprecated: ok = m_ctx.vl2->m_MULTICAST_FRAME_deprecated(cc, packetId, auth, path, peer, *pkt, pktSize); break;
                case Protocol::VERB_PUSH_DIRECT_PATHS: ok = m_PUSH_DIRECT_PATHS(cc, packetId, auth, path, peer, *pkt, pktSize); break;
                case Protocol::VERB_USER_MESSAGE: ok = m_USER_MESSAGE(cc, packetId, auth, path, peer, *pkt, pktSize); break;
                case Protocol::VERB_MULTICAST: ok = m_ctx.vl2->m_MULTICAST(cc, packetId, auth, path, peer, *pkt, pktSize); break;
                case Protocol::VERB_ENCAP: ok = m_ENCAP(cc, packetId, auth, path, peer, *pkt, pktSize); break;

                default: m_ctx.t->incomingPacketDropped(cc, 0xeeeeeff0, packetId, 0, identityFromPeerPtr(peer), path->address(), hops, verb, ZT_TRACE_PACKET_DROP_REASON_UNRECOGNIZED_VERB); break;
            }
            if (likely(ok))
                peer->received(m_ctx, cc, path, hops, packetId, pktSize - ZT_PROTO_PACKET_PAYLOAD_START, verb, inReVerb);
        }
        else {
            // If decryption and authentication were not successful, try to look up identities.
            // This is rate limited by virtue of the retry rate limit timer.
            if (pktSize <= 0)
                pktSize = pktv.mergeCopy(*pkt);
            if (likely(pktSize >= ZT_PROTO_MIN_PACKET_LENGTH)) {
                ZT_SPEW("authentication failed or no peers match, queueing WHOIS for %s", source.toString().c_str());
                bool sendPending;
                {
                    Mutex::Lock wl(m_whoisQueue_l);
                    p_WhoisQueueItem &wq        = m_whoisQueue[source];
                    const unsigned int wpidx    = wq.waitingPacketCount++ % ZT_VL1_MAX_WHOIS_WAITING_PACKETS;
                    wq.waitingPacketSize[wpidx] = (unsigned int)pktSize;
                    wq.waitingPacket[wpidx]     = pkt;
                    sendPending                 = (cc.ticks - wq.lastRetry) >= ZT_WHOIS_RETRY_DELAY;
                }
                if (sendPending)
                    m_sendPendingWhois(cc);
            }
        }
    }
    catch (...) {
        m_ctx.t->unexpectedError(cc, 0xea1b6dea, "unexpected exception in onRemotePacket() parsing packet from %s", path->address().toString().c_str());
    }
}

void VL1::m_relay(CallContext &cc, const SharedPtr<Path> &path, Address destination, SharedPtr<Buf> &pkt, int pktSize) {}

void VL1::m_sendPendingWhois(CallContext &cc)
{
    const SharedPtr<Peer> root(m_ctx.topology->root());
    if (unlikely(!root))
        return;
    const SharedPtr<Path> rootPath(root->path(cc));
    if (unlikely(!rootPath))
        return;

    Vector<Address> toSend;
    {
        Mutex::Lock wl(m_whoisQueue_l);
        for (Map<Address, p_WhoisQueueItem>::iterator wi(m_whoisQueue.begin()); wi != m_whoisQueue.end(); ++wi) {
            if ((cc.ticks - wi->second.lastRetry) >= ZT_WHOIS_RETRY_DELAY) {
                wi->second.lastRetry = cc.ticks;
                ++wi->second.retries;
                toSend.push_back(wi->first);
            }
        }
    }

    if (!toSend.empty()) {
        SymmetricKey &key = root->key();
        uint8_t outp[ZT_DEFAULT_UDP_MTU - ZT_PROTO_MIN_PACKET_LENGTH];
        Vector<Address>::iterator a(toSend.begin());
        while (a != toSend.end()) {
            const uint64_t packetId = key.nextMessage(m_ctx.identity.address(), root->address());
            int p                   = Protocol::newPacket(outp, packetId, root->address(), m_ctx.identity.address(), Protocol::VERB_WHOIS);
            while ((a != toSend.end()) && (p < (sizeof(outp) - ZT_ADDRESS_LENGTH))) {
                a->copyTo(outp + p);
                ++a;
                p += ZT_ADDRESS_LENGTH;
            }
            m_ctx.expect->sending(Protocol::armor(outp, p, key, root->cipher()), cc.ticks);
            root->send(m_ctx, cc, outp, p, rootPath);
        }
    }
}

SharedPtr<Peer> VL1::m_HELLO(CallContext &cc, const SharedPtr<Path> &path, Buf &pkt, int packetSize)
{
    const uint64_t packetId = Utils::loadMachineEndian<uint64_t>(pkt.unsafeData + ZT_PROTO_PACKET_ID_INDEX);
    const uint64_t mac      = Utils::loadMachineEndian<uint64_t>(pkt.unsafeData + ZT_PROTO_PACKET_MAC_INDEX);
    const uint8_t hops      = pkt.unsafeData[ZT_PROTO_PACKET_FLAGS_INDEX] & ZT_PROTO_FLAG_FIELD_HOPS_MASK;

    const uint8_t protoVersion = pkt.lI8<ZT_PROTO_PACKET_PAYLOAD_START>();
    if (unlikely(protoVersion < ZT_PROTO_VERSION_MIN)) {
        m_ctx.t->incomingPacketDropped(cc, 0x907a9891, packetId, 0, Identity::NIL, path->address(), hops, Protocol::VERB_HELLO, ZT_TRACE_PACKET_DROP_REASON_PEER_TOO_OLD);
        return SharedPtr<Peer>();
    }
    const unsigned int versionMajor = pkt.lI8<ZT_PROTO_PACKET_PAYLOAD_START + 1>();
    const unsigned int versionMinor = pkt.lI8<ZT_PROTO_PACKET_PAYLOAD_START + 2>();
    const unsigned int versionRev   = pkt.lI16<ZT_PROTO_PACKET_PAYLOAD_START + 3>();
    const uint64_t timestamp        = pkt.lI64<ZT_PROTO_PACKET_PAYLOAD_START + 5>();

    int ii = ZT_PROTO_PACKET_PAYLOAD_START + 13;

    // Get identity and verify that it matches the sending address in the packet.
    Identity id;
    if (unlikely(pkt.rO(ii, id) < 0)) {
        m_ctx.t->incomingPacketDropped(cc, 0x707a9810, packetId, 0, Identity::NIL, path->address(), hops, Protocol::VERB_HELLO, ZT_TRACE_PACKET_DROP_REASON_INVALID_OBJECT);
        return SharedPtr<Peer>();
    }
    if (unlikely(id.address() != Address(pkt.unsafeData + ZT_PROTO_PACKET_SOURCE_INDEX))) {
        m_ctx.t->incomingPacketDropped(cc, 0x707a9010, packetId, 0, Identity::NIL, path->address(), hops, Protocol::VERB_HELLO, ZT_TRACE_PACKET_DROP_REASON_MAC_FAILED);
        return SharedPtr<Peer>();
    }

    // Get the peer that matches this identity, or learn a new one if we don't know it.
    SharedPtr<Peer> peer(m_ctx.topology->peer(cc, id.address(), true));
    if (peer) {
        if (unlikely(peer->identity() != id)) {
            m_ctx.t->incomingPacketDropped(cc, 0x707a9891, packetId, 0, identityFromPeerPtr(peer), path->address(), hops, Protocol::VERB_HELLO, ZT_TRACE_PACKET_DROP_REASON_MAC_FAILED);
            return SharedPtr<Peer>();
        }
        if (unlikely(peer->deduplicateIncomingPacket(packetId))) {
            ZT_SPEW("discarding packet %.16llx from %s(%s): duplicate!", packetId, id.address().toString().c_str(), path->address().toString().c_str());
            return SharedPtr<Peer>();
        }
    }
    else {
        if (unlikely(!id.locallyValidate())) {
            m_ctx.t->incomingPacketDropped(cc, 0x707a9892, packetId, 0, identityFromPeerPtr(peer), path->address(), hops, Protocol::VERB_HELLO, ZT_TRACE_PACKET_DROP_REASON_INVALID_OBJECT);
            return SharedPtr<Peer>();
        }
        peer.set(new Peer());
        if (unlikely(!peer->init(m_ctx, cc, id))) {
            m_ctx.t->incomingPacketDropped(cc, 0x707a9893, packetId, 0, identityFromPeerPtr(peer), path->address(), hops, Protocol::VERB_HELLO, ZT_TRACE_PACKET_DROP_REASON_UNSPECIFIED);
            return SharedPtr<Peer>();
        }
        peer = m_ctx.topology->add(cc, peer);
    }

    // ------------------------------------------------------------------------------------------------------------------
    // If we made it this far, peer is non-NULL and the identity is valid and matches it.
    // ------------------------------------------------------------------------------------------------------------------

    if (protoVersion >= 11) {
        // V2.x and newer use HMAC-SHA384 for HELLO, which offers a larger security margin
        // to guard key exchange and connection setup than typical AEAD. The packet MAC
        // field is ignored, and eventually it'll be undefined.
        uint8_t hmac[ZT_HMACSHA384_LEN];
        if (unlikely(packetSize < ZT_HMACSHA384_LEN)) {
            m_ctx.t->incomingPacketDropped(cc, 0xab9c9891, packetId, 0, identityFromPeerPtr(peer), path->address(), hops, Protocol::VERB_HELLO, ZT_TRACE_PACKET_DROP_REASON_MAC_FAILED);
            return SharedPtr<Peer>();
        }
        packetSize -= ZT_HMACSHA384_LEN;
        pkt.unsafeData[ZT_PROTO_PACKET_FLAGS_INDEX] &= ~ZT_PROTO_FLAG_FIELD_HOPS_MASK;        // mask hops to 0
        Utils::storeMachineEndian<uint64_t>(pkt.unsafeData + ZT_PROTO_PACKET_MAC_INDEX, 0);   // set MAC field to 0
        HMACSHA384(peer->identityHelloHmacKey(), pkt.unsafeData, packetSize, hmac);
        if (unlikely(!Utils::secureEq(hmac, pkt.unsafeData + packetSize, ZT_HMACSHA384_LEN))) {
            m_ctx.t->incomingPacketDropped(cc, 0x707a9891, packetId, 0, identityFromPeerPtr(peer), path->address(), hops, Protocol::VERB_HELLO, ZT_TRACE_PACKET_DROP_REASON_MAC_FAILED);
            return SharedPtr<Peer>();
        }
    }
    else {
        // Older versions use Poly1305 MAC (but no whole packet encryption) for HELLO.
        if (likely(packetSize > ZT_PROTO_PACKET_ENCRYPTED_SECTION_START)) {
            uint8_t perPacketKey[ZT_SALSA20_KEY_SIZE];
            Protocol::salsa2012DeriveKey(peer->rawIdentityKey(), perPacketKey, pkt, packetSize);
            uint8_t macKey[ZT_POLY1305_KEY_SIZE];
            Salsa20(perPacketKey, &packetId).crypt12(Utils::ZERO256, macKey, ZT_POLY1305_KEY_SIZE);
            Poly1305 poly1305(macKey);
            poly1305.update(pkt.unsafeData + ZT_PROTO_PACKET_ENCRYPTED_SECTION_START, packetSize - ZT_PROTO_PACKET_ENCRYPTED_SECTION_START);
            uint64_t polyMac[2];
            poly1305.finish(polyMac);
            if (unlikely(mac != polyMac[0])) {
                m_ctx.t->incomingPacketDropped(cc, 0x11bfff82, packetId, 0, id, path->address(), hops, Protocol::VERB_NOP, ZT_TRACE_PACKET_DROP_REASON_MAC_FAILED);
                return SharedPtr<Peer>();
            }
        }
        else {
            m_ctx.t->incomingPacketDropped(cc, 0x11bfff81, packetId, 0, id, path->address(), hops, Protocol::VERB_NOP, ZT_TRACE_PACKET_DROP_REASON_MAC_FAILED);
            return SharedPtr<Peer>();
        }
    }

    // ------------------------------------------------------------------------------------------------------------------
    // This far means we passed MAC (Poly1305 or HMAC-SHA384 for newer peers)
    // ------------------------------------------------------------------------------------------------------------------

    InetAddress sentTo;
    if (unlikely(pkt.rO(ii, sentTo) < 0)) {
        m_ctx.t->incomingPacketDropped(cc, 0x707a9811, packetId, 0, identityFromPeerPtr(peer), path->address(), hops, Protocol::VERB_HELLO, ZT_TRACE_PACKET_DROP_REASON_INVALID_OBJECT);
        return SharedPtr<Peer>();
    }

    SymmetricKey &key = peer->key();

    if (protoVersion >= 11) {
        // V2.x and newer supports an encrypted section and has a new OK format.
        ii += 4;   // skip reserved field
        if (likely((ii + 12) < packetSize)) {
            AES::CTR ctr(peer->identityHelloDictionaryEncryptionCipher());
            const uint8_t *const ctrNonce = pkt.unsafeData + ii;
            ii += 12;
            ctr.init(ctrNonce, 0, pkt.unsafeData + ii);
            ctr.crypt(pkt.unsafeData + ii, packetSize - ii);
            ctr.finish();

            ii += 2;   // skip reserved field
            const unsigned int dictSize = pkt.rI16(ii);
            if (unlikely((ii + dictSize) > packetSize)) {
                m_ctx.t->incomingPacketDropped(cc, 0x707a9815, packetId, 0, identityFromPeerPtr(peer), path->address(), hops, Protocol::VERB_HELLO, ZT_TRACE_PACKET_DROP_REASON_INVALID_OBJECT);
                return peer;
            }
            Dictionary md;
            if (!md.decode(pkt.unsafeData + ii, dictSize)) {
                m_ctx.t->incomingPacketDropped(cc, 0x707a9816, packetId, 0, identityFromPeerPtr(peer), path->address(), hops, Protocol::VERB_HELLO, ZT_TRACE_PACKET_DROP_REASON_INVALID_OBJECT);
                return peer;
            }

            if (!md.empty()) {
                // TODO
            }
        }
    }

    Protocol::newPacket(pkt, key.nextMessage(m_ctx.identity.address(), peer->address()), peer->address(), m_ctx.identity.address(), Protocol::VERB_OK);
    ii = ZT_PROTO_PACKET_PAYLOAD_START;
    pkt.wI8(ii, Protocol::VERB_HELLO);
    pkt.wI64(ii, packetId);
    pkt.wI64(ii, timestamp);
    pkt.wI8(ii, ZT_PROTO_VERSION);
    pkt.wI8(ii, ZEROTIER_VERSION_MAJOR);
    pkt.wI8(ii, ZEROTIER_VERSION_MINOR);
    pkt.wI16(ii, ZEROTIER_VERSION_REVISION);
    pkt.wO(ii, path->address());
    pkt.wI16(ii, 0);   // reserved, specifies no "moons" for older versions

    if (protoVersion >= 11) {
        FCV<uint8_t, 1024> okmd;
        pkt.wI16(ii, (uint16_t)okmd.size());
        pkt.wB(ii, okmd.data(), okmd.size());

        if (unlikely((ii + ZT_HMACSHA384_LEN) > ZT_BUF_MEM_SIZE))   // sanity check, should be impossible
            return SharedPtr<Peer>();

        HMACSHA384(peer->identityHelloHmacKey(), pkt.unsafeData, ii, pkt.unsafeData + ii);
        ii += ZT_HMACSHA384_LEN;
    }

    peer->setRemoteVersion(protoVersion, versionMajor, versionMinor, versionRev);
    peer->send(m_ctx, cc, pkt.unsafeData, ii, path);
    return peer;
}

bool VL1::m_ERROR(CallContext &cc, const uint64_t packetId, const unsigned int auth, const SharedPtr<Path> &path, const SharedPtr<Peer> &peer, Buf &pkt, int packetSize, Protocol::Verb &inReVerb)
{
#if 0
	if (packetSize < (int)sizeof(Protocol::ERROR::Header)) {
		RR->t->incomingPacketDropped(tPtr,0x3beb1947,0,0,identityFromPeerPtr(peer),path->address(),0,Protocol::VERB_ERROR,ZT_TRACE_PACKET_DROP_REASON_MALFORMED_PACKET);
		return false;
	}
	Protocol::ERROR::Header &eh = pkt.as<Protocol::ERROR::Header>();
	inReVerb = (Protocol::Verb)eh.inReVerb;

	const int64_t now = RR->node->now();
	if (!RR->expect->expecting(eh.inRePacketId,now)) {
		RR->t->incomingPacketDropped(tPtr,0x4c1f1ff7,0,0,identityFromPeerPtr(peer),path->address(),0,Protocol::VERB_OK,ZT_TRACE_PACKET_DROP_REASON_REPLY_NOT_EXPECTED);
		return false;
	}

	switch(eh.error) {

		//case Protocol::ERROR_INVALID_REQUEST:
		//case Protocol::ERROR_BAD_PROTOCOL_VERSION:
		//case Protocol::ERROR_CANNOT_DELIVER:
		default:
			break;

		case Protocol::ERROR_OBJ_NOT_FOUND:
			if (eh.inReVerb == Protocol::VERB_NETWORK_CONFIG_REQUEST) {
			}
			break;

		case Protocol::ERROR_UNSUPPORTED_OPERATION:
			if (eh.inReVerb == Protocol::VERB_NETWORK_CONFIG_REQUEST) {
			}
			break;

		case Protocol::ERROR_NEED_MEMBERSHIP_CERTIFICATE:
			break;

		case Protocol::ERROR_NETWORK_ACCESS_DENIED_:
			if (eh.inReVerb == Protocol::VERB_NETWORK_CONFIG_REQUEST) {
			}
			break;

	}
	return true;
#endif
}

bool VL1::m_OK(CallContext &cc, const uint64_t packetId, const unsigned int auth, const SharedPtr<Path> &path, const SharedPtr<Peer> &peer, Buf &pkt, int packetSize, Protocol::Verb &inReVerb)
{
    int ii = ZT_PROTO_PACKET_PAYLOAD_START + 13;

    inReVerb                    = (Protocol::Verb)pkt.rI8(ii);
    const uint64_t inRePacketId = pkt.rI64(ii);
    if (unlikely(Buf::readOverflow(ii, packetSize))) {
        m_ctx.t->incomingPacketDropped(cc, 0x4c1f1ff7, packetId, 0, identityFromPeerPtr(peer), path->address(), 0, Protocol::VERB_OK, ZT_TRACE_PACKET_DROP_REASON_MALFORMED_PACKET);
        return false;
    }

    if (unlikely(!m_ctx.expect->expecting(inRePacketId, cc.ticks))) {
        m_ctx.t->incomingPacketDropped(cc, 0x4c1f1ff8, packetId, 0, identityFromPeerPtr(peer), path->address(), 0, Protocol::VERB_OK, ZT_TRACE_PACKET_DROP_REASON_REPLY_NOT_EXPECTED);
        return false;
    }

    ZT_SPEW("got OK in-re %s (packet ID %.16llx) from %s(%s)", Protocol::verbName(inReVerb), inRePacketId, peer->address().toString().c_str(), path->address().toString().c_str());

    switch (inReVerb) {
        case Protocol::VERB_HELLO: break;

        case Protocol::VERB_WHOIS: break;

        case Protocol::VERB_NETWORK_CONFIG_REQUEST: break;

        case Protocol::VERB_MULTICAST_GATHER: break;
    }

    return true;
}

bool VL1::m_WHOIS(CallContext &cc, const uint64_t packetId, const unsigned int auth, const SharedPtr<Path> &path, const SharedPtr<Peer> &peer, Buf &pkt, int packetSize)
{
#if 0
	if (packetSize < (int)sizeof(Protocol::OK::Header)) {
		RR->t->incomingPacketDropped(tPtr,0x4c1f1ff7,0,0,identityFromPeerPtr(peer),path->address(),0,Protocol::VERB_OK,ZT_TRACE_PACKET_DROP_REASON_MALFORMED_PACKET);
		return false;
	}
	Protocol::Header &ph = pkt.as<Protocol::Header>();

	if (!peer->rateGateInboundWhoisRequest(RR->node->now())) {
		RR->t->incomingPacketDropped(tPtr,0x19f7194a,ph.packetId,0,peer->identity(),path->address(),Protocol::packetHops(ph),Protocol::VERB_WHOIS,ZT_TRACE_PACKET_DROP_REASON_RATE_LIMIT_EXCEEDED);
		return true;
	}

	Buf outp;
	Protocol::OK::WHOIS &outh = outp.as<Protocol::OK::WHOIS>();
	int ptr = sizeof(Protocol::Header);
	while ((ptr + ZT_ADDRESS_LENGTH) <= packetSize) {
		outh.h.h.packetId = Protocol::getPacketId();
		peer->address().copyTo(outh.h.h.destination);
		RR->identity.address().copyTo(outh.h.h.source);
		outh.h.h.flags = 0;
		outh.h.h.verb = Protocol::VERB_OK;

		outh.h.inReVerb = Protocol::VERB_WHOIS;
		outh.h.inRePacketId = ph.packetId;

		int outl = sizeof(Protocol::OK::WHOIS);
		while ( ((ptr + ZT_ADDRESS_LENGTH) <= packetSize) && ((outl + ZT_IDENTITY_MARSHAL_SIZE_MAX + ZT_LOCATOR_MARSHAL_SIZE_MAX) < ZT_PROTO_MAX_PACKET_LENGTH) ) {
			const SharedPtr<Peer> &wp(RR->topology->peer(tPtr,Address(pkt.unsafeData + ptr)));
			if (wp) {
				outp.wO(outl,wp->identity());
				if (peer->remoteVersionProtocol() >= 11) { // older versions don't know what a locator is
					const Locator loc(wp->locator());
					outp.wO(outl,loc);
				}
				if (Buf::writeOverflow(outl)) { // sanity check, shouldn't be possible
					RR->t->unexpectedError(tPtr,0xabc0f183,"Buf write overflow building OK(WHOIS) to reply to %s",Trace::str(peer->address(),path).s);
					return false;
				}
			}
			ptr += ZT_ADDRESS_LENGTH;
		}

		if (outl > (int)sizeof(Protocol::OK::WHOIS)) {
			Protocol::armor(outp,outl,peer->key(),peer->cipher());
			path->send(RR,tPtr,outp.unsafeData,outl,RR->node->now());
		}
	}

	return true;
#endif
}

bool VL1::m_RENDEZVOUS(CallContext &cc, const uint64_t packetId, const unsigned int auth, const SharedPtr<Path> &path, const SharedPtr<Peer> &peer, Buf &pkt, int packetSize)
{
#if 0
	if (RR->topology->isRoot(peer->identity())) {
		if (packetSize < (int)sizeof(Protocol::RENDEZVOUS)) {
			RR->t->incomingPacketDropped(tPtr,0x43e90ab3,Protocol::packetId(pkt,packetSize),0,peer->identity(),path->address(),Protocol::packetHops(pkt,packetSize),Protocol::VERB_RENDEZVOUS,ZT_TRACE_PACKET_DROP_REASON_MALFORMED_PACKET);
			return false;
		}
		Protocol::RENDEZVOUS &rdv = pkt.as<Protocol::RENDEZVOUS>();

		const SharedPtr<Peer> with(RR->topology->peer(tPtr,Address(rdv.peerAddress)));
		if (with) {
			const int64_t now = RR->node->now();
			const unsigned int port = Utils::ntoh(rdv.port);
			if (port != 0) {
				switch(rdv.addressLength) {
					case 4:
					case 16:
						if ((int)(sizeof(Protocol::RENDEZVOUS) + rdv.addressLength) <= packetSize) {
							const InetAddress atAddr(pkt.unsafeData + sizeof(Protocol::RENDEZVOUS),rdv.addressLength,port);
							peer->tryToContactAt(tPtr,Endpoint(atAddr),now,false);
							RR->t->tryingNewPath(tPtr,0x55a19aaa,with->identity(),atAddr,path->address(),Protocol::packetId(pkt,packetSize),Protocol::VERB_RENDEZVOUS,peer->identity(),ZT_TRACE_TRYING_NEW_PATH_REASON_RENDEZVOUS);
						}
						break;
					case 255: {
						Endpoint ep;
						int p = sizeof(Protocol::RENDEZVOUS);
						int epl = pkt.rO(p,ep);
						if ((epl > 0) && (ep) && (!Buf::readOverflow(p,packetSize))) {
							switch (ep.type()) {
								case Endpoint::TYPE_INETADDR_V4:
								case Endpoint::TYPE_INETADDR_V6:
									peer->tryToContactAt(tPtr,ep,now,false);
									RR->t->tryingNewPath(tPtr,0x55a19aab,with->identity(),ep.inetAddr(),path->address(),Protocol::packetId(pkt,packetSize),Protocol::VERB_RENDEZVOUS,peer->identity(),ZT_TRACE_TRYING_NEW_PATH_REASON_RENDEZVOUS);
									break;
								default:
									break;
							}
						}
					} break;
				}
			}
		}
	}
	return true;
#endif
}

bool VL1::m_ECHO(CallContext &cc, const uint64_t packetId, const unsigned int auth, const SharedPtr<Path> &path, const SharedPtr<Peer> &peer, Buf &pkt, int packetSize)
{
#if 0
	const uint64_t packetId = Protocol::packetId(pkt,packetSize);
	const uint64_t now = RR->node->now();
	if (packetSize < (int)sizeof(Protocol::Header)) {
		RR->t->incomingPacketDropped(tPtr,0x14d70bb0,packetId,0,peer->identity(),path->address(),Protocol::packetHops(pkt,packetSize),Protocol::VERB_ECHO,ZT_TRACE_PACKET_DROP_REASON_MALFORMED_PACKET);
		return false;
	}

	if (peer->rateGateEchoRequest(now)) {
		Buf outp;
		Protocol::OK::ECHO &outh = outp.as<Protocol::OK::ECHO>();
		outh.h.h.packetId = Protocol::getPacketId();
		peer->address().copyTo(outh.h.h.destination);
		RR->identity.address().copyTo(outh.h.h.source);
		outh.h.h.flags = 0;
		outh.h.h.verb = Protocol::VERB_OK;
		outh.h.inReVerb = Protocol::VERB_ECHO;
		outh.h.inRePacketId = packetId;
		int outl = sizeof(Protocol::OK::ECHO);
		outp.wB(outl,pkt.unsafeData + sizeof(Protocol::Header),packetSize - sizeof(Protocol::Header));

		if (Buf::writeOverflow(outl)) {
			RR->t->incomingPacketDropped(tPtr,0x14d70bb0,packetId,0,peer->identity(),path->address(),Protocol::packetHops(pkt,packetSize),Protocol::VERB_ECHO,ZT_TRACE_PACKET_DROP_REASON_MALFORMED_PACKET);
			return false;
		}

		Protocol::armor(outp,outl,peer->key(),peer->cipher());
		path->send(RR,tPtr,outp.unsafeData,outl,now);
	} else {
		RR->t->incomingPacketDropped(tPtr,0x27878bc1,packetId,0,peer->identity(),path->address(),Protocol::packetHops(pkt,packetSize),Protocol::VERB_ECHO,ZT_TRACE_PACKET_DROP_REASON_RATE_LIMIT_EXCEEDED);
	}

	return true;
#endif
}

bool VL1::m_PUSH_DIRECT_PATHS(CallContext &cc, const uint64_t packetId, const unsigned int auth, const SharedPtr<Path> &path, const SharedPtr<Peer> &peer, Buf &pkt, int packetSize)
{
#if 0
	if (packetSize < (int)sizeof(Protocol::PUSH_DIRECT_PATHS)) {
		RR->t->incomingPacketDropped(tPtr,0x1bb1bbb1,Protocol::packetId(pkt,packetSize),0,peer->identity(),path->address(),Protocol::packetHops(pkt,packetSize),Protocol::VERB_PUSH_DIRECT_PATHS,ZT_TRACE_PACKET_DROP_REASON_MALFORMED_PACKET);
		return false;
	}
	Protocol::PUSH_DIRECT_PATHS &pdp = pkt.as<Protocol::PUSH_DIRECT_PATHS>();

	int ptr = sizeof(Protocol::PUSH_DIRECT_PATHS);
	const unsigned int numPaths = Utils::ntoh(pdp.numPaths);
	InetAddress a;
	Endpoint ep;
	for(unsigned int pi=0;pi<numPaths;++pi) {
		/*const uint8_t flags = pkt.rI8(ptr);*/ ++ptr; // flags are not presently used

		const int xas = (int)pkt.rI16(ptr);
		//const uint8_t *const extendedAttrs = pkt.rBnc(ptr,xas);
		ptr += xas;

		const unsigned int addrType = pkt.rI8(ptr);
		const unsigned int addrRecordLen = pkt.rI8(ptr);
		if (addrRecordLen == 0) {
			RR->t->incomingPacketDropped(tPtr,0xaed00118,pdp.h.packetId,0,peer->identity(),path->address(),Protocol::packetHops(pdp.h),Protocol::VERB_PUSH_DIRECT_PATHS,ZT_TRACE_PACKET_DROP_REASON_MALFORMED_PACKET);
			return false;
		}
		if (Buf::readOverflow(ptr,packetSize)) {
			RR->t->incomingPacketDropped(tPtr,0xb450e10f,pdp.h.packetId,0,peer->identity(),path->address(),Protocol::packetHops(pdp.h),Protocol::VERB_PUSH_DIRECT_PATHS,ZT_TRACE_PACKET_DROP_REASON_MALFORMED_PACKET);
			return false;
		}

		const void *addrBytes = nullptr;
		unsigned int addrLen = 0;
		unsigned int addrPort = 0;
		switch(addrType) {
			case 0:
				addrBytes = pkt.rBnc(ptr,addrRecordLen);
				addrLen = addrRecordLen;
				break;
			case 4:
				addrBytes = pkt.rBnc(ptr,4);
				addrLen = 4;
				addrPort = pkt.rI16(ptr);
				break;
			case 6:
				addrBytes = pkt.rBnc(ptr,16);
				addrLen = 16;
				addrPort = pkt.rI16(ptr);
				break;
			//case 200:
				// TODO: this would be a WebRTC SDP offer contained in the extended attrs field
				//break;
			default: break;
		}

		if (Buf::readOverflow(ptr,packetSize)) {
			RR->t->incomingPacketDropped(tPtr,0xb4d0f10f,pdp.h.packetId,0,peer->identity(),path->address(),Protocol::packetHops(pdp.h),Protocol::VERB_PUSH_DIRECT_PATHS,ZT_TRACE_PACKET_DROP_REASON_MALFORMED_PACKET);
			return false;
		}

		if (addrPort) {
			a.set(addrBytes,addrLen,addrPort);
		} else if (addrLen) {
			if (ep.unmarshal(reinterpret_cast<const uint8_t *>(addrBytes),(int)addrLen) <= 0) {
				RR->t->incomingPacketDropped(tPtr,0x00e0f00d,pdp.h.packetId,0,peer->identity(),path->address(),Protocol::packetHops(pdp.h),Protocol::VERB_PUSH_DIRECT_PATHS,ZT_TRACE_PACKET_DROP_REASON_MALFORMED_PACKET);
				return false;
			}

			switch(ep.type()) {
				case Endpoint::TYPE_INETADDR_V4:
				case Endpoint::TYPE_INETADDR_V6:
					a = ep.inetAddr();
					break;
				default: // other types are not supported yet
					break;
			}
		}

		if (a) {
			RR->t->tryingNewPath(tPtr,0xa5ab1a43,peer->identity(),a,path->address(),Protocol::packetId(pkt,packetSize),Protocol::VERB_RENDEZVOUS,peer->identity(),ZT_TRACE_TRYING_NEW_PATH_REASON_RECEIVED_PUSH_DIRECT_PATHS);
		}

		ptr += (int)addrRecordLen;
	}

	// TODO: add to a peer try-queue

	return true;
#endif
}

bool VL1::m_USER_MESSAGE(CallContext &cc, const uint64_t packetId, const unsigned int auth, const SharedPtr<Path> &path, const SharedPtr<Peer> &peer, Buf &pkt, int packetSize)
{
    // TODO
    return true;
}

bool VL1::m_ENCAP(CallContext &cc, const uint64_t packetId, const unsigned int auth, const SharedPtr<Path> &path, const SharedPtr<Peer> &peer, Buf &pkt, int packetSize)
{
    // TODO: not implemented yet
    return true;
}

}   // namespace ZeroTier
