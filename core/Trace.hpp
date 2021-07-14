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

#ifndef ZT_TRACE_HPP
#define ZT_TRACE_HPP

#include "Address.hpp"
#include "CallContext.hpp"
#include "Constants.hpp"
#include "Containers.hpp"
#include "InetAddress.hpp"
#include "MAC.hpp"
#include "Mutex.hpp"
#include "SharedPtr.hpp"
#include "Utils.hpp"

#define ZT_TRACE_F_VL1           0x01U
#define ZT_TRACE_F_VL2           0x02U
#define ZT_TRACE_F_VL2_FILTER    0x04U
#define ZT_TRACE_F_VL2_MULTICAST 0x08U

namespace ZeroTier {

class Context;
class Identity;
class Peer;
class Path;
class Network;
class MembershipCredential;
class OwnershipCredential;
class RevocationCredential;
class TagCredential;
class CapabilityCredential;
struct NetworkConfig;

/**
 * Remote tracing and trace logging handler
 *
 * These methods are called when things happen that may be of interested to
 * someone debugging ZeroTier or its virtual networks. The codeLocation parameter
 * is an arbitrary pseudo-random identifier of the form 0xNNNNNNNN that could be
 * easily found by searching the code base. This makes it easy to locate the
 * specific line where a trace originated without relying on brittle non-portable
 * things like source file and line number. The same identifier should be used
 * for the same 'place' in the code across versions. These could eventually be
 * turned into constants that are semi-official and stored in a database to
 * provide extra debug context.
 */
class Trace {
  public:
    struct RuleResultLog : public TriviallyCopyable {
        uint8_t l[ZT_MAX_NETWORK_RULES / 2];   // ZT_MAX_NETWORK_RULES 4-bit fields

        ZT_INLINE void log(const unsigned int rn, const uint8_t thisRuleMatches, const uint8_t thisSetMatches) noexcept { l[rn >> 1U] |= (((thisRuleMatches + 1U) << 2U) | (thisSetMatches + 1U)) << ((rn & 1U) << 2U); }

        ZT_INLINE void logSkipped(const unsigned int rn, const uint8_t thisSetMatches) noexcept { l[rn >> 1U] |= (thisSetMatches + 1U) << ((rn & 1U) << 2U); }

        ZT_INLINE void clear() noexcept { memoryZero(this); }
    };

    explicit Trace(const Context &ctx);

    void unexpectedError(const CallContext &cc, uint32_t codeLocation, const char *message, ...);

    ZT_INLINE void resettingPathsInScope(const CallContext &cc, const uint32_t codeLocation, const Identity &reporter, const InetAddress &from, const InetAddress &oldExternal, const InetAddress &newExternal, const InetAddress::IpScope scope)
    {
        if (unlikely((m_traceFlags & ZT_TRACE_F_VL1) != 0))
            m_resettingPathsInScope(cc.tPtr, codeLocation, reporter, from, oldExternal, newExternal, scope);
    }

    ZT_INLINE void tryingNewPath(const CallContext &cc, const uint32_t codeLocation, const Identity &trying, const InetAddress &physicalAddress, const InetAddress &triggerAddress, uint64_t triggeringPacketId, uint8_t triggeringPacketVerb, const Identity &triggeringPeer)
    {
        if (unlikely((m_traceFlags & ZT_TRACE_F_VL1) != 0))
            m_tryingNewPath(cc.tPtr, codeLocation, trying, physicalAddress, triggerAddress, triggeringPacketId, triggeringPacketVerb, triggeringPeer);
    }

    ZT_INLINE void learnedNewPath(const CallContext &cc, const uint32_t codeLocation, uint64_t packetId, const Identity &peerIdentity, const InetAddress &physicalAddress, const InetAddress &replaced)
    {
        if (unlikely((m_traceFlags & ZT_TRACE_F_VL1) != 0))
            m_learnedNewPath(cc.tPtr, codeLocation, packetId, peerIdentity, physicalAddress, replaced);
    }

    ZT_INLINE void incomingPacketDropped(const CallContext &cc, const uint32_t codeLocation, uint64_t packetId, uint64_t networkId, const Identity &peerIdentity, const InetAddress &physicalAddress, uint8_t hops, uint8_t verb, const ZT_TracePacketDropReason reason)
    {
        if (unlikely((m_traceFlags & ZT_TRACE_F_VL1) != 0))
            m_incomingPacketDropped(cc.tPtr, codeLocation, packetId, networkId, peerIdentity, physicalAddress, hops, verb, reason);
    }

    ZT_INLINE void outgoingNetworkFrameDropped(const CallContext &cc, const uint32_t codeLocation, uint64_t networkId, const MAC &sourceMac, const MAC &destMac, uint16_t etherType, uint16_t frameLength, const uint8_t *frameData, ZT_TraceFrameDropReason reason)
    {
        if (unlikely((m_traceFlags & ZT_TRACE_F_VL2) != 0))
            m_outgoingNetworkFrameDropped(cc.tPtr, codeLocation, networkId, sourceMac, destMac, etherType, frameLength, frameData, reason);
    }

    ZT_INLINE void incomingNetworkFrameDropped(
        const CallContext &cc, const uint32_t codeLocation, uint64_t networkId, const MAC &sourceMac, const MAC &destMac, const uint16_t etherType, const Identity &peerIdentity, const InetAddress &physicalAddress, uint8_t hops, uint16_t frameLength, const uint8_t *frameData, uint8_t verb, bool credentialRequestSent, ZT_TraceFrameDropReason reason)
    {
        if (unlikely((m_traceFlags & ZT_TRACE_F_VL2) != 0))
            m_incomingNetworkFrameDropped(cc.tPtr, codeLocation, networkId, sourceMac, destMac, etherType, peerIdentity, physicalAddress, hops, frameLength, frameData, verb, credentialRequestSent, reason);
    }

    ZT_INLINE void networkConfigRequestSent(const CallContext &cc, const uint32_t codeLocation, uint64_t networkId)
    {
        if (unlikely((m_traceFlags & ZT_TRACE_F_VL2) != 0))
            m_networkConfigRequestSent(cc.tPtr, codeLocation, networkId);
    }

    ZT_INLINE void networkFilter(
        const CallContext &cc, const uint32_t codeLocation, uint64_t networkId, const uint8_t primaryRuleSetLog[512], const uint8_t matchingCapabilityRuleSetLog[512], uint32_t matchingCapabilityId, int64_t matchingCapabilityTimestamp, const Address &source, const Address &dest, const MAC &sourceMac, const MAC &destMac, uint16_t frameLength,
        const uint8_t *frameData, uint16_t etherType, uint16_t vlanId, bool noTee, bool inbound, int accept)
    {
        if (unlikely((m_traceFlags & ZT_TRACE_F_VL2_FILTER) != 0)) {
            m_networkFilter(cc.tPtr, codeLocation, networkId, primaryRuleSetLog, matchingCapabilityRuleSetLog, matchingCapabilityId, matchingCapabilityTimestamp, source, dest, sourceMac, destMac, frameLength, frameData, etherType, vlanId, noTee, inbound, accept);
        }
    }

    ZT_INLINE void credentialRejected(const CallContext &cc, const uint32_t codeLocation, uint64_t networkId, const Identity &identity, uint32_t credentialId, int64_t credentialTimestamp, uint8_t credentialType, ZT_TraceCredentialRejectionReason reason)
    {
        if (unlikely((m_traceFlags & ZT_TRACE_F_VL2) != 0))
            m_credentialRejected(cc.tPtr, codeLocation, networkId, identity, credentialId, credentialTimestamp, credentialType, reason);
    }

  private:
    void m_resettingPathsInScope(void *tPtr, uint32_t codeLocation, const Identity &reporter, const InetAddress &from, const InetAddress &oldExternal, const InetAddress &newExternal, InetAddress::IpScope scope);

    void m_tryingNewPath(void *tPtr, uint32_t codeLocation, const Identity &trying, const InetAddress &physicalAddress, const InetAddress &triggerAddress, uint64_t triggeringPacketId, uint8_t triggeringPacketVerb, const Identity &triggeringPeer);

    void m_learnedNewPath(void *tPtr, uint32_t codeLocation, uint64_t packetId, const Identity &peerIdentity, const InetAddress &physicalAddress, const InetAddress &replaced);

    void m_incomingPacketDropped(void *tPtr, uint32_t codeLocation, uint64_t packetId, uint64_t networkId, const Identity &peerIdentity, const InetAddress &physicalAddress, uint8_t hops, uint8_t verb, ZT_TracePacketDropReason reason);

    void m_outgoingNetworkFrameDropped(void *tPtr, uint32_t codeLocation, uint64_t networkId, const MAC &sourceMac, const MAC &destMac, uint16_t etherType, uint16_t frameLength, const uint8_t *frameData, ZT_TraceFrameDropReason reason);

    void
    m_incomingNetworkFrameDropped(void *tPtr, uint32_t codeLocation, uint64_t networkId, const MAC &sourceMac, const MAC &destMac, const uint16_t etherType, const Identity &peerIdentity, const InetAddress &physicalAddress, uint8_t hops, uint16_t frameLength, const uint8_t *frameData, uint8_t verb, bool credentialRequestSent, ZT_TraceFrameDropReason reason);

    void m_networkConfigRequestSent(void *tPtr, uint32_t codeLocation, uint64_t networkId);

    void m_networkFilter(
        void *tPtr, uint32_t codeLocation, uint64_t networkId, const uint8_t *primaryRuleSetLog, const uint8_t *matchingCapabilityRuleSetLog, uint32_t matchingCapabilityId, int64_t matchingCapabilityTimestamp, const Address &source, const Address &dest, const MAC &sourceMac, const MAC &destMac, uint16_t frameLength, const uint8_t *frameData,
        uint16_t etherType, uint16_t vlanId, bool noTee, bool inbound, int accept);

    void m_credentialRejected(void *tPtr, uint32_t codeLocation, uint64_t networkId, const Identity &identity, uint32_t credentialId, int64_t credentialTimestamp, uint8_t credentialType, ZT_TraceCredentialRejectionReason reason);

    const Context &m_ctx;
    volatile unsigned int m_traceFlags;   // faster than atomic, but may not "instantly" change... should be okay
};

}   // namespace ZeroTier

#endif
