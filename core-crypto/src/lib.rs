// (c) 2020-2022 ZeroTier, Inc. -- currently propritery pending actual release and licensing. See LICENSE.md.

pub mod aes;
pub mod aes_gmac_siv;
pub mod hash;
pub mod kbkdf;
pub mod p384;
pub mod poly1305;
pub mod random;
pub mod salsa;
pub mod secret;
pub mod x25519;
pub mod zssp;

pub const ZEROES: [u8; 64] = [0_u8; 64];
