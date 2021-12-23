/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 *
 * (c)2021 ZeroTier, Inc.
 * https://www.zerotier.com/
 */

use std::mem::size_of;
use std::ptr::{slice_from_raw_parts, write_bytes, copy_nonoverlapping};

use crate::IDENTITY_HASH_SIZE;

// The number of indexing sub-hashes to use, must be <= IDENTITY_HASH_SIZE / 8
const KEY_MAPPING_ITERATIONS: usize = 3;

#[inline(always)]
fn xorshift64(mut x: u64) -> u64 {
    x ^= x.wrapping_shl(13);
    x ^= x.wrapping_shr(7);
    x ^= x.wrapping_shl(17);
    x
}

#[repr(packed)]
struct IBLTEntry {
    key_sum: [u64; IDENTITY_HASH_SIZE / 8],
    check_hash_sum: u64,
    count: i64,
}

/// An IBLT (invertible bloom lookup table) specialized for reconciling sets of identity hashes.
/// This skips some extra hashing that would be necessary in a universal implementation since identity
/// hashes are already randomly distributed strong hashes.
pub struct IBLT {
    map: Vec<IBLTEntry>,
}

impl IBLTEntry {
    #[inline(always)]
    fn is_singular(&self) -> bool {
        if self.count == 1 || self.count == -1 {
            u64::from_le(self.key_sum[0]).wrapping_add(xorshift64(u64::from_le(self.key_sum[1]))) == u64::from_le(self.check_hash_sum)
        } else {
            false
        }
    }
}

impl IBLT {
    /// Construct a new IBLT with a given capacity.
    pub fn new(buckets: usize) -> Self {
        assert!(buckets > 0);
        Self {
            map: {
                let mut tmp: Vec<IBLTEntry> = Vec::with_capacity(buckets);
                unsafe {
                    tmp.set_len(buckets);
                    write_bytes(tmp.as_mut_ptr().cast::<u8>(), 0, buckets * size_of::<IBLTEntry>());
                }
                tmp
            }
        }
    }

    /// Obtain IBLT from a byte array.
    /// This returns None if the supplied bytes are not a valid IBLT.
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() >= size_of::<IBLTEntry>() && (b.len() % size_of::<IBLTEntry>()) == 0 {
            let buckets = b.len() / size_of::<IBLTEntry>();
            Some(Self {
                map: {
                    let mut tmp: Vec<IBLTEntry> = Vec::with_capacity(buckets);
                    unsafe {
                        tmp.set_len(buckets);
                        copy_nonoverlapping(b.as_ptr(), tmp.as_mut_ptr().cast::<u8>(), buckets * size_of::<IBLTEntry>());
                    }
                    tmp
                }
            })
        } else {
            None
        }
    }

    /// Get this IBLT as a byte array that is ready to be sent over the wire.
    pub fn as_bytes(&self) -> &[u8] {
        unsafe { &*slice_from_raw_parts(self.map.as_ptr().cast(), size_of::<IBLTEntry>() * self.map.len()) }
    }

    fn ins_rem(&mut self, key: &[u64; IDENTITY_HASH_SIZE / 8], delta: i64) {
        let check_hash = u64::from_le(key[0]).wrapping_add(xorshift64(u64::from_le(key[1]))).to_le();
        let buckets = self.map.len();
        for mapping_sub_hash in 0..KEY_MAPPING_ITERATIONS {
            let b = unsafe { self.map.get_unchecked_mut((u64::from_le(key[mapping_sub_hash]) as usize) % buckets) };
            for j in 0..(IDENTITY_HASH_SIZE / 8) {
                b.key_sum[j] ^= key[j];
            }
            b.check_hash_sum ^= check_hash;
            b.count = i64::from_le(b.count).wrapping_add(delta).to_le();
        }
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "x86", target_arch = "aarch64", target_arch = "powerpc64"))]
    #[inline(always)]
    pub fn insert(&mut self, key: &[u8; IDENTITY_HASH_SIZE]) {
        self.ins_rem(unsafe { &*key.as_ptr().cast::<[u64; IDENTITY_HASH_SIZE / 8]>() }, 1);
    }

    #[cfg(not(any(target_arch = "x86_64", target_arch = "x86", target_arch = "aarch64", target_arch = "powerpc64")))]
    #[inline(always)]
    pub fn insert(&mut self, key: &[u8; IDENTITY_HASH_SIZE]) {
        let mut tmp = [0_u64; IDENTITY_HASH_SIZE / 8];
        unsafe { copy_nonoverlapping(key.as_ptr(), tmp.as_mut_ptr().cast(), IDENTITY_HASH_SIZE) };
        self.ins_rem(&tmp, 1);
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "x86", target_arch = "aarch64", target_arch = "powerpc64"))]
    #[inline(always)]
    pub fn remove(&mut self, key: &[u8; IDENTITY_HASH_SIZE]) {
        self.ins_rem(unsafe { &*key.as_ptr().cast::<[u64; IDENTITY_HASH_SIZE / 8]>() }, -1);
    }

    #[cfg(not(any(target_arch = "x86_64", target_arch = "x86", target_arch = "aarch64", target_arch = "powerpc64")))]
    #[inline(always)]
    pub fn remove(&mut self, key: &[u8; IDENTITY_HASH_SIZE]) {
        let mut tmp = [0_u64; IDENTITY_HASH_SIZE / 8];
        unsafe { copy_nonoverlapping(key.as_ptr(), tmp.as_mut_ptr().cast(), IDENTITY_HASH_SIZE) };
        self.ins_rem(&tmp, -1);
    }

    /// Subtract another IBLT from this one to compute set difference.
    pub fn subtract(&mut self, other: &IBLT) {
        if other.map.len() == self.map.len() {
            for i in 0..self.map.len() {
                let self_b = unsafe { self.map.get_unchecked_mut(i) };
                let other_b = unsafe { other.map.get_unchecked(i) };
                for j in 0..(IDENTITY_HASH_SIZE / 8) {
                    self_b.key_sum[j] ^= other_b.key_sum[j];
                }
                self_b.check_hash_sum ^= other_b.check_hash_sum;
                self_b.count = i64::from_le(self_b.count).wrapping_sub(i64::from_le(other_b.count)).to_le();
            }
        }
    }

    /// Call a function for every value that can be extracted from this IBLT.
    ///
    /// The function is called with the key and a boolean. The boolean is meaningful
    /// if this IBLT is the result of subtract(). In that case the boolean is true
    /// if the "local" IBLT contained the item and false if the "remote" side contained
    /// the item.
    ///
    /// The starting_singular_bucket parameter must be the internal index of a
    /// bucket with only one entry (1 or -1). It can be obtained from the return
    /// values of either subtract() or singular_bucket().
    pub fn list<F: FnMut(&[u8; IDENTITY_HASH_SIZE], bool) -> bool>(&mut self, mut f: F) {
        let buckets = self.map.len();
        let mut singular_buckets: Vec<u32> = Vec::with_capacity(buckets);

        for i in 0..buckets {
            if unsafe { self.map.get_unchecked(i) }.is_singular() {
                singular_buckets.push(i as u32);
            };
        }

        let mut key = [0_u64; IDENTITY_HASH_SIZE / 8];
        while !singular_buckets.is_empty() {
            let b = unsafe { self.map.get_unchecked_mut(singular_buckets.pop().unwrap() as usize) };
            if b.is_singular() {
                for j in 0..(IDENTITY_HASH_SIZE / 8) {
                    key[j] = b.key_sum[j];
                }
                if f(unsafe { &*key.as_ptr().cast::<[u8; IDENTITY_HASH_SIZE]>() }, b.count == 1) {
                    let check_hash = u64::from_le(key[0]).wrapping_add(xorshift64(u64::from_le(key[1]))).to_le();
                    for mapping_sub_hash in 0..KEY_MAPPING_ITERATIONS {
                        let bi = (u64::from_le(unsafe { *key.get_unchecked(mapping_sub_hash) }) as usize) % buckets;
                        let b = unsafe { self.map.get_unchecked_mut(bi) };
                        for j in 0..(IDENTITY_HASH_SIZE / 8) {
                            b.key_sum[j] ^= key[j];
                        }
                        b.check_hash_sum ^= check_hash;
                        b.count = i64::from_le(b.count).wrapping_sub(1).to_le();
                        if b.is_singular() {
                            singular_buckets.push(bi as u32);
                        }
                    }
                } else {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use zerotier_core_crypto::hash::SHA384;
    use crate::iblt::*;

    #[allow(unused_variables)]
    #[test]
    fn insert_and_list() {
        assert_eq!(size_of::<IBLTEntry>(), IDENTITY_HASH_SIZE + 8 + 8);
        assert!(KEY_MAPPING_ITERATIONS <= (IDENTITY_HASH_SIZE / 8) && (IDENTITY_HASH_SIZE % 8) == 0);

        for expected_cnt in 0..800 {
            let mut t = IBLT::new(1000);
            for i in 0..expected_cnt {
                let k = SHA384::hash(&((i + expected_cnt) as u32).to_le_bytes());
                t.insert(&k);
            }
            let mut cnt = 0;
            t.list(|k, d| {
                cnt += 1;
                //println!("{} {}", zerotier_core_crypto::hex::to_string(k), d);
                true
            });
            //println!("retrieved {} keys", cnt);
            assert_eq!(cnt, expected_cnt);
        }
    }

    #[test]
    fn benchmark() {
    }
}
