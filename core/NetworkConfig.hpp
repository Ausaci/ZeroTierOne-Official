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

#ifndef ZT_NETWORKCONFIG_HPP
#define ZT_NETWORKCONFIG_HPP

#include "Address.hpp"
#include "CapabilityCredential.hpp"
#include "Constants.hpp"
#include "Containers.hpp"
#include "Dictionary.hpp"
#include "Identity.hpp"
#include "InetAddress.hpp"
#include "MembershipCredential.hpp"
#include "MulticastGroup.hpp"
#include "OwnershipCredential.hpp"
#include "TagCredential.hpp"
#include "Trace.hpp"
#include "TriviallyCopyable.hpp"
#include "Utils.hpp"

#include <algorithm>
#include <stdexcept>

namespace ZeroTier {

/**
 * Default maximum time delta for COMs, tags, and capabilities
 *
 * The current value is two hours, providing ample time for a controller to
 * experience fail-over, etc.
 */
#define ZT_NETWORKCONFIG_DEFAULT_CREDENTIAL_TIME_MAX_MAX_DELTA 7200000ULL

/**
 * Default minimum credential TTL and maxDelta for COM timestamps
 *
 * This is just slightly over three minutes and provides three retries for
 * all currently online members to refresh.
 */
#define ZT_NETWORKCONFIG_DEFAULT_CREDENTIAL_TIME_MIN_MAX_DELTA 185000ULL

/**
 * Flag: enable broadcast
 */
#define ZT_NETWORKCONFIG_FLAG_ENABLE_BROADCAST 0x0000000000000002ULL

/**
 * Flag: enable IPv6 NDP emulation for certain V6 address patterns
 */
#define ZT_NETWORKCONFIG_FLAG_ENABLE_IPV6_NDP_EMULATION 0x0000000000000004ULL

/**
 * Flag: result of unrecognized MATCH entries in a rules table: match if set, no-match if clear
 */
#define ZT_NETWORKCONFIG_FLAG_RULES_RESULT_OF_UNSUPPORTED_MATCH 0x0000000000000008ULL

/**
 * Device can bridge to other Ethernet networks and gets unknown recipient multicasts
 */
#define ZT_NETWORKCONFIG_SPECIALIST_TYPE_ACTIVE_BRIDGE 0x0000020000000000ULL

// Fields for meta-data sent with network config requests

// Protocol version (see Packet.hpp)
#define ZT_NETWORKCONFIG_REQUEST_METADATA_KEY_PROTOCOL_VERSION "pv"
// Software vendor
#define ZT_NETWORKCONFIG_REQUEST_METADATA_KEY_NODE_VENDOR "vend"
// Software major version
#define ZT_NETWORKCONFIG_REQUEST_METADATA_KEY_NODE_MAJOR_VERSION "majv"
// Software minor version
#define ZT_NETWORKCONFIG_REQUEST_METADATA_KEY_NODE_MINOR_VERSION "minv"
// Software revision
#define ZT_NETWORKCONFIG_REQUEST_METADATA_KEY_NODE_REVISION "revv"
// Rules engine revision
#define ZT_NETWORKCONFIG_REQUEST_METADATA_KEY_RULES_ENGINE_REV "revr"
// Maximum number of rules per network this node can accept
#define ZT_NETWORKCONFIG_REQUEST_METADATA_KEY_MAX_NETWORK_RULES "mr"
// Maximum number of capabilities this node can accept
#define ZT_NETWORKCONFIG_REQUEST_METADATA_KEY_MAX_NETWORK_CAPABILITIES "mc"
// Maximum number of rules per capability this node can accept
#define ZT_NETWORKCONFIG_REQUEST_METADATA_KEY_MAX_CAPABILITY_RULES "mcr"
// Maximum number of tags this node can accept
#define ZT_NETWORKCONFIG_REQUEST_METADATA_KEY_MAX_NETWORK_TAGS "mt"
// Network join authorization token (if any)
#define ZT_NETWORKCONFIG_REQUEST_METADATA_KEY_AUTH "a"
// Network configuration meta-data flags
#define ZT_NETWORKCONFIG_REQUEST_METADATA_KEY_FLAGS "f"

// These dictionary keys are short so they don't take up much room.
// By convention we use upper case for binary blobs, but it doesn't really matter.

// network config version
#define ZT_NETWORKCONFIG_DICT_KEY_VERSION "v"
// network ID
#define ZT_NETWORKCONFIG_DICT_KEY_NETWORK_ID "nwid"
// integer(hex)
#define ZT_NETWORKCONFIG_DICT_KEY_TIMESTAMP "ts"
// integer(hex)
#define ZT_NETWORKCONFIG_DICT_KEY_REVISION "r"
// address of member
#define ZT_NETWORKCONFIG_DICT_KEY_ISSUED_TO "id"
// full identity hash of member
#define ZT_NETWORKCONFIG_DICT_KEY_ISSUED_TO_IDENTITY_HASH "IDH"
// flags(hex)
#define ZT_NETWORKCONFIG_DICT_KEY_FLAGS "f"
// integer(hex)
#define ZT_NETWORKCONFIG_DICT_KEY_MULTICAST_LIMIT "ml"
// network type (hex)
#define ZT_NETWORKCONFIG_DICT_KEY_TYPE "t"
// text
#define ZT_NETWORKCONFIG_DICT_KEY_NAME "n"
// network MTU
#define ZT_NETWORKCONFIG_DICT_KEY_MTU "mtu"
// credential time max delta in ms
#define ZT_NETWORKCONFIG_DICT_KEY_CREDENTIAL_TIME_MAX_DELTA "ctmd"
// binary serialized certificate of membership
#define ZT_NETWORKCONFIG_DICT_KEY_COM "C"
// specialists (binary array of uint64_t)
#define ZT_NETWORKCONFIG_DICT_KEY_SPECIALISTS "S"
// routes (binary blob)
#define ZT_NETWORKCONFIG_DICT_KEY_ROUTES "RT"
// static IPs (binary blob)
#define ZT_NETWORKCONFIG_DICT_KEY_STATIC_IPS "I"
// rules (binary blob)
#define ZT_NETWORKCONFIG_DICT_KEY_RULES "R"
// capabilities (binary blobs)
#define ZT_NETWORKCONFIG_DICT_KEY_CAPABILITIES "CAP"
// tags (binary blobs)
#define ZT_NETWORKCONFIG_DICT_KEY_TAGS "TAG"
// tags (binary blobs)
#define ZT_NETWORKCONFIG_DICT_KEY_CERTIFICATES_OF_OWNERSHIP "COO"

/**
 * Network configuration received from network controller nodes
 */
struct NetworkConfig : TriviallyCopyable {
    ZT_INLINE NetworkConfig() noexcept
    {
        memoryZero(this);
    }   // NOLINT(cppcoreguidelines-pro-type-member-init,hicpp-member-init)

    /**
     * Write this network config to a dictionary for transport
     *
     * @param d Dictionary
     * @return True if dictionary was successfully created, false if e.g. overflow
     */
    bool toDictionary(Dictionary &d) const;

    /**
     * Read this network config from a dictionary
     *
     * @param d Dictionary (non-const since it might be modified during parse, should not be used after call)
     * @return True if dictionary was valid and network config successfully initialized
     */
    bool fromDictionary(const Dictionary &d);

    /**
     * @return True if broadcast (ff:ff:ff:ff:ff:ff) address should work on this network
     */
    ZT_INLINE bool enableBroadcast() const noexcept
    {
        return ((this->flags & ZT_NETWORKCONFIG_FLAG_ENABLE_BROADCAST) != 0);
    }

    /**
     * @return True if IPv6 NDP emulation should be allowed for certain "magic" IPv6 address patterns
     */
    ZT_INLINE bool ndpEmulation() const noexcept
    {
        return ((this->flags & ZT_NETWORKCONFIG_FLAG_ENABLE_IPV6_NDP_EMULATION) != 0);
    }

    /**
     * @return Network type is public (no access control)
     */
    ZT_INLINE bool isPublic() const noexcept { return (this->type == ZT_NETWORK_TYPE_PUBLIC); }

    /**
     * @return Network type is private (certificate access control)
     */
    ZT_INLINE bool isPrivate() const noexcept { return (this->type == ZT_NETWORK_TYPE_PRIVATE); }

    /**
     * @param fromPeer Peer attempting to bridge other Ethernet peers onto network
     * @return True if this network allows bridging
     */
    ZT_INLINE bool permitsBridging(const Address &fromPeer) const noexcept
    {
        for (unsigned int i = 0; i < specialistCount; ++i) {
            if ((fromPeer.toInt() == (specialists[i] & ZT_ADDRESS_MASK))
                && ((specialists[i] & ZT_NETWORKCONFIG_SPECIALIST_TYPE_ACTIVE_BRIDGE) != 0))
                return true;
        }
        return false;
    }

    ZT_INLINE operator bool() const noexcept
    {
        return (networkId != 0);
    }   // NOLINT(google-explicit-constructor,hicpp-explicit-conversions)
    ZT_INLINE bool operator==(const NetworkConfig &nc) const noexcept
    {
        return (memcmp(this, &nc, sizeof(NetworkConfig)) == 0);
    }

    ZT_INLINE bool operator!=(const NetworkConfig &nc) const noexcept { return (!(*this == nc)); }

    /**
     * Add a specialist or mask flags if already present
     *
     * This masks the existing flags if the specialist is already here or adds
     * it otherwise.
     *
     * @param a Address of specialist
     * @param f Flags (OR of specialist role/type flags)
     * @return True if successfully masked or added
     */
    bool addSpecialist(const Address &a, uint64_t f) noexcept;

    ZT_INLINE const CapabilityCredential *capability(const uint32_t id) const
    {
        for (unsigned int i = 0; i < capabilityCount; ++i) {
            if (capabilities[i].id() == id)
                return &(capabilities[i]);
        }
        return nullptr;
    }

    ZT_INLINE const TagCredential *tag(const uint32_t id) const
    {
        for (unsigned int i = 0; i < tagCount; ++i) {
            if (tags[i].id() == id)
                return &(tags[i]);
        }
        return nullptr;
    }

    /**
     * Network ID that this configuration applies to
     */
    uint64_t networkId;

    /**
     * Controller-side time of config generation/issue
     */
    int64_t timestamp;

    /**
     * Max difference between timestamp and tag/capability timestamp
     */
    int64_t credentialTimeMaxDelta;

    /**
     * Controller-side revision counter for this configuration
     */
    uint64_t revision;

    /**
     * Address of device to which this config is issued
     */
    Address issuedTo;

    /**
     * Hash of identity public key(s) of node to whom this is issued
     *
     * If this field is all zero it is treated as undefined since old controllers
     * do not set it.
     */
    uint8_t issuedToFingerprintHash[ZT_FINGERPRINT_HASH_SIZE];

    /**
     * Flags (64-bit)
     */
    uint64_t flags;

    /**
     * Network MTU
     */
    unsigned int mtu;

    /**
     * Maximum number of recipients per multicast (not including active bridges)
     */
    unsigned int multicastLimit;

    /**
     * Number of specialists
     */
    unsigned int specialistCount;

    /**
     * Number of routes
     */
    unsigned int routeCount;

    /**
     * Number of ZT-managed static IP assignments
     */
    unsigned int staticIpCount;

    /**
     * Number of rule table entries
     */
    unsigned int ruleCount;

    /**
     * Number of capabilities
     */
    unsigned int capabilityCount;

    /**
     * Number of tags
     */
    unsigned int tagCount;

    /**
     * Number of certificates of ownership
     */
    unsigned int certificateOfOwnershipCount;

    /**
     * Specialist devices
     *
     * For each entry the least significant 40 bits are the device's ZeroTier
     * address and the most significant 24 bits are flags indicating its role.
     */
    uint64_t specialists[ZT_MAX_NETWORK_SPECIALISTS];

    /**
     * Statically defined "pushed" routes (including default gateways)
     */
    ZT_VirtualNetworkRoute routes[ZT_MAX_NETWORK_ROUTES];

    /**
     * Static IP assignments
     */
    InetAddress staticIps[ZT_MAX_ZT_ASSIGNED_ADDRESSES];

    /**
     * Base network rules
     */
    ZT_VirtualNetworkRule rules[ZT_MAX_NETWORK_RULES];

    /**
     * Capabilities for this node on this network, in ascending order of capability ID
     */
    CapabilityCredential capabilities[ZT_MAX_NETWORK_CAPABILITIES];

    /**
     * Tags for this node on this network, in ascending order of tag ID
     */
    TagCredential tags[ZT_MAX_NETWORK_TAGS];

    /**
     * Certificates of ownership for this network member
     */
    OwnershipCredential certificatesOfOwnership[ZT_MAX_CERTIFICATES_OF_OWNERSHIP];

    /**
     * Network type (currently just public or private)
     */
    ZT_VirtualNetworkType type;

    /**
     * Network short name or empty string if not defined
     */
    char name[ZT_MAX_NETWORK_SHORT_NAME_LENGTH + 1];

    /**
     * Certificate of membership (for private networks)
     */
    MembershipCredential com;
};

}   // namespace ZeroTier

#endif
