/*
 * ZeroTier One - Network Virtualization Everywhere
 * Copyright (C) 2011-2015  ZeroTier, Inc.
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 *
 * --
 *
 * ZeroTier may be used and distributed under the terms of the GPLv3, which
 * are available at: http://www.gnu.org/licenses/gpl-3.0.html
 *
 * If you would like to embed ZeroTier into a commercial application or
 * redistribute it in a modified binary form, please contact ZeroTier Networks
 * LLC. Start here: http://www.zerotier.com/
 */

package com.zerotier.sdk;

import java.util.Arrays;

/**
 * Peer status result buffer
 *
 * Defined in ZeroTierOne.h as ZT_Peer
 */
public class Peer {

    private final long address;

    private final int versionMajor;

    private final int versionMinor;

    private final int versionRev;

    private final int latency;

    private final PeerRole role;

    private final PeerPhysicalPath[] paths;

    public Peer(long address, int versionMajor, int versionMinor, int versionRev, int latency, PeerRole role, PeerPhysicalPath[] paths) {
        this.address = address;
        this.versionMajor = versionMajor;
        this.versionMinor = versionMinor;
        this.versionRev = versionRev;
        this.latency = latency;
        this.role = role;
        this.paths = paths;
    }

    @Override
    public String toString() {
        return "Peer(" + address + ", " + versionMajor + ", " + versionMinor + ", " + versionRev + ", " + latency + ", " + role + ", " + Arrays.toString(paths) + ")";
    }

    /**
     * ZeroTier address (40 bits)
     *
     * @return XXX
     */
    public long getAddress() {
        return address;
    }

    /**
     * Remote major version or -1 if not known
     *
     * @return XXX
     */
    public int getVersionMajor() {
        return versionMajor;
    }

    /**
     * Remote minor version or -1 if not known
     *
     * @return XXX
     */
    public int getVersionMinor() {
        return versionMinor;
    }

    /**
     * Remote revision or -1 if not known
     *
     * @return XXX
     */
    public int getVersionRev() {
        return versionRev;
    }

    /**
     * Last measured latency in milliseconds or zero if unknown
     *
     * @return XXX
     */
    public int getLatency() {
        return latency;
    }

    /**
     * What trust hierarchy role does this device have?
     *
     * @return XXX
     */
    public PeerRole getRole() {
        return role;
    }

    /**
     * Known network paths to peer
     *
     * @return XXX
     */
    public PeerPhysicalPath[] getPaths() {
        return paths;
    }
}
