// (c) 2020-2022 ZeroTier, Inc. -- currently propritery pending actual release and licensing. See LICENSE.md.

use poly1305::universal_hash::{NewUniversalHash, UniversalHash};

/// The poly1305 message authentication function.
#[repr(transparent)]
pub struct Poly1305(poly1305::Poly1305);

pub const POLY1305_ONE_TIME_KEY_SIZE: usize = 32;
pub const POLY1305_MAC_SIZE: usize = 16;

impl Poly1305 {
    #[inline(always)]
    pub fn new(key: &[u8]) -> Poly1305 {
        assert_eq!(key.len(), 32);
        Self(poly1305::Poly1305::new(poly1305::Key::from_slice(key)))
    }

    #[inline(always)]
    pub fn update(&mut self, data: &[u8]) {
        self.0.update_padded(data);
    }

    #[inline(always)]
    pub fn finish(self) -> [u8; POLY1305_MAC_SIZE] {
        self.0.finalize().into_bytes().into()
    }
}

unsafe impl Send for Poly1305 {}
