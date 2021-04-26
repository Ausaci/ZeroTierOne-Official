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

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/********************************************************************************************************************/

#ifdef __APPLE__
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <ifaddrs.h>
#include <net/bpf.h>
#include <net/if.h>
#include <net/if_arp.h>
#include <net/if_dl.h>
#include <net/if_media.h>
#include <net/ndrv.h>
#include <net/route.h>
#include <netinet/icmp6.h>
#include <netinet/in.h>
#include <netinet/in_var.h>
#include <netinet/ip.h>
#include <netinet/ip6.h>
#include <netinet6/in6_var.h>
#include <netinet6/nd6.h>
#include <sys/cdefs.h>
#include <sys/ioctl.h>
#include <sys/param.h>
#include <sys/select.h>
#include <sys/signal.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/sysctl.h>
#include <sys/types.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>
#ifdef __cplusplus
extern "C" {
#endif
/* These complex macros don't translate well with Rust bindgen, so compute
 * them with the C compiler and export them. */
extern const unsigned long c_BIOCSBLEN;
extern const unsigned long c_BIOCIMMEDIATE;
extern const unsigned long c_BIOCSSEESENT;
extern const unsigned long c_BIOCSETIF;
extern const unsigned long c_BIOCSHDRCMPLT;
extern const unsigned long c_BIOCPROMISC;
extern const unsigned long c_SIOCGIFINFO_IN6;
extern const unsigned long c_SIOCSIFINFO_FLAGS;
extern const unsigned long c_SIOCAUTOCONF_START;
extern const unsigned long c_SIOCAUTOCONF_STOP;
#ifdef __cplusplus
}
#endif
#ifndef IPV6_DONTFRAG
#define IPV6_DONTFRAG 62
#endif
#endif /* __APPLE__ */

/********************************************************************************************************************/

#if defined(__linux__) || defined(linux) || defined(__LINUX__) || defined(__linux)
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <ifaddrs.h>
#include <linux/if.h>
#include <linux/if_addr.h>
#include <linux/if_ether.h>
#include <linux/if_tun.h>
#include <net/if_arp.h>
#include <netinet/in.h>
#include <signal.h>
#include <sys/ioctl.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>
#endif /* __linux__ */

/********************************************************************************************************************/

#ifdef __cplusplus
extern "C" {
#endif

// Get the default home path for this platform.
extern const char* platformDefaultHomePath();

// This ms-since-epoch function may be faster than the one in Rust's stdlib.
extern int64_t msSinceEpoch();

// This is the number of milliseconds since some time in the past, unaffected by the clock (or msSinceEpoch() if not
// supported by host).
extern int64_t msMonotonic();

// Rust glue to C code to lock down a file, which is simple on Unix-like OSes
// and horrible on Windows.
extern void lockDownFile(const char* path, int isDir);

// Rust glue to ZeroTier's secure random PRNG.
extern void getSecureRandom(void* buf, unsigned int len);

// These AES encrypt and decrypt a single block using a key that is randomly
// generated at process init and never exported. It's used to generate HTTP
// digest authentication tokens that can just be decrypted to get and check
// a timestamp to prevent replay attacks.
extern void encryptHttpAuthNonce(void* block);
extern void decryptHttpAuthNonce(void* block);

#ifdef __cplusplus
}
#endif

/********************************************************************************************************************/
