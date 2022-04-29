/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 *
 * (c)2021 ZeroTier, Inc.
 * https://www.zerotier.com/
 */

use std::io::Write;
use std::mem::{size_of, MaybeUninit};

use crate::util::pool::PoolFactory;

/// Annotates a structure as containing only primitive types.
///
/// This means the structure is safe to copy in raw form, does not need to be dropped, and otherwise
/// contains nothing complex that requires any special handling. It also implies that it is safe to
/// access without concern for alignment on platforms on which this is an issue, or at least that
/// the implementer must take care to guard any unaligned access in appropriate ways. FlatBlob
/// structures are generally repr(C, packed) as well to make them deterministic across systems.
///
/// The Buffer has special methods allowing these structs to be read and written in place, which
/// would be unsafe without these concerns being flagged as not applicable.
pub unsafe trait FlatBlob: Sized {}

/// A safe bounds checked I/O buffer with extensions for convenient appending of RawObject types.
pub struct Buffer<const L: usize>(usize, [u8; L]);

unsafe impl<const L: usize> FlatBlob for Buffer<L> {}

impl<const L: usize> Default for Buffer<L> {
    #[inline(always)]
    fn default() -> Self {
        Self::new()
    }
}

fn overflow_err() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "buffer overflow")
}

impl<const L: usize> Buffer<L> {
    pub const CAPACITY: usize = L;

    /// Create an empty zeroed buffer.
    #[inline(always)]
    pub fn new() -> Self {
        Self(0, [0_u8; L])
    }

    /// Create an empty buffer without internally zeroing its memory.
    ///
    /// This is technically unsafe because unwritten memory in the buffer will have undefined contents.
    /// Otherwise it behaves exactly like new().
    #[inline(always)]
    pub unsafe fn new_without_memzero() -> Self {
        Self(0, MaybeUninit::uninit().assume_init())
    }

    #[inline(always)]
    pub fn from_bytes(b: &[u8]) -> std::io::Result<Self> {
        let l = b.len();
        if l <= L {
            let mut tmp = Self::new();
            tmp.0 = l;
            tmp.1[0..l].copy_from_slice(b);
            Ok(tmp)
        } else {
            Err(overflow_err())
        }
    }

    #[inline(always)]
    pub fn as_bytes(&self) -> &[u8] {
        &self.1[0..self.0]
    }

    #[inline(always)]
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        &mut self.1[0..self.0]
    }

    #[inline(always)]
    pub fn as_ptr(&self) -> *const u8 {
        self.1.as_ptr()
    }

    #[inline(always)]
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.1.as_mut_ptr()
    }

    /// Get all bytes after a given position.
    #[inline(always)]
    pub fn as_bytes_starting_at(&self, start: usize) -> std::io::Result<&[u8]> {
        if start <= self.0 {
            Ok(&self.1[start..])
        } else {
            Err(overflow_err())
        }
    }

    #[inline(always)]
    pub fn clear(&mut self) {
        self.1[0..self.0].fill(0);
        self.0 = 0;
    }

    /// Load array into buffer.
    /// This will panic if the array is larger than L.
    pub fn set_to(&mut self, b: &[u8]) {
        let len = b.len();
        self.0 = len;
        self.1[0..len].copy_from_slice(b);
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.0
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.0 == 0
    }

    /// Set the size of this buffer's data.
    ///
    /// This will panic if the specified size is larger than L. If the size is larger
    /// than the current size uninitialized space will be zeroed.
    #[inline(always)]
    pub fn set_size(&mut self, s: usize) {
        let prev_len = self.0;
        self.0 = s;
        if s > prev_len {
            self.1[prev_len..s].fill(0);
        }
    }

    /// Set the size of the data in this buffer without checking bounds or zeroing new space.
    #[inline(always)]
    pub unsafe fn set_size_unchecked(&mut self, s: usize) {
        self.0 = s;
    }

    /// Get a byte from this buffer without checking bounds.
    #[inline(always)]
    pub unsafe fn get_unchecked(&self, i: usize) -> u8 {
        *self.1.get_unchecked(i)
    }

    /// Append a structure and return a mutable reference to its memory.
    #[inline(always)]
    pub fn append_struct_get_mut<T: FlatBlob>(&mut self) -> std::io::Result<&mut T> {
        let ptr = self.0;
        let end = ptr + size_of::<T>();
        if end <= L {
            self.0 = end;
            Ok(unsafe { &mut *self.1.as_mut_ptr().add(ptr).cast() })
        } else {
            Err(overflow_err())
        }
    }

    /// Append a fixed size array and return a mutable reference to its memory.
    #[inline(always)]
    pub fn append_bytes_fixed_get_mut<const S: usize>(&mut self) -> std::io::Result<&mut [u8; S]> {
        let ptr = self.0;
        let end = ptr + S;
        if end <= L {
            self.0 = end;
            Ok(unsafe { &mut *self.1.as_mut_ptr().add(ptr).cast() })
        } else {
            Err(overflow_err())
        }
    }

    /// Append a runtime sized array and return a mutable reference to its memory.
    #[inline(always)]
    pub fn append_bytes_get_mut(&mut self, s: usize) -> std::io::Result<&mut [u8]> {
        let ptr = self.0;
        let end = ptr + s;
        if end <= L {
            self.0 = end;
            Ok(&mut self.1[ptr..end])
        } else {
            Err(overflow_err())
        }
    }

    pub fn append_padding(&mut self, b: u8, count: usize) -> std::io::Result<()> {
        let ptr = self.0;
        let end = ptr + count;
        if end <= L {
            self.0 = end;
            self.1[ptr..end].fill(b);
            Ok(())
        } else {
            Err(overflow_err())
        }
    }

    pub fn append_bytes(&mut self, buf: &[u8]) -> std::io::Result<()> {
        let ptr = self.0;
        let end = ptr + buf.len();
        if end <= L {
            self.0 = end;
            self.1[ptr..end].copy_from_slice(buf);
            Ok(())
        } else {
            Err(overflow_err())
        }
    }

    pub fn append_bytes_fixed<const S: usize>(&mut self, buf: &[u8; S]) -> std::io::Result<()> {
        let ptr = self.0;
        let end = ptr + S;
        if end <= L {
            self.0 = end;
            self.1[ptr..end].copy_from_slice(buf);
            Ok(())
        } else {
            Err(overflow_err())
        }
    }

    #[inline(always)]
    pub fn append_varint(&mut self, i: u64) -> std::io::Result<()> {
        crate::util::varint::write(self, i)
    }

    #[inline(always)]
    pub fn append_u8(&mut self, i: u8) -> std::io::Result<()> {
        let ptr = self.0;
        if ptr < L {
            self.0 = ptr + 1;
            self.1[ptr] = i;
            Ok(())
        } else {
            Err(overflow_err())
        }
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64"))]
    #[inline(always)]
    pub fn append_u16(&mut self, i: u16) -> std::io::Result<()> {
        let ptr = self.0;
        let end = ptr + 2;
        if end <= L {
            self.0 = end;
            unsafe { *self.1.as_mut_ptr().add(ptr).cast::<u16>() = i.to_be() };
            Ok(())
        } else {
            Err(overflow_err())
        }
    }

    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64", target_arch = "powerpc64")))]
    #[inline(always)]
    pub fn append_u16(&mut self, i: u16) -> std::io::Result<()> {
        let ptr = self.0;
        let end = ptr + 2;
        if end <= L {
            self.0 = end;
            self.1[ptr..end].copy_from_slice(&i.to_be_bytes());
            Ok(())
        } else {
            Err(overflow_err())
        }
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64", target_arch = "powerpc64"))]
    #[inline(always)]
    pub fn append_u32(&mut self, i: u32) -> std::io::Result<()> {
        let ptr = self.0;
        let end = ptr + 4;
        if end <= L {
            self.0 = end;
            unsafe { *self.1.as_mut_ptr().add(ptr).cast::<u32>() = i.to_be() };
            Ok(())
        } else {
            Err(overflow_err())
        }
    }

    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64", target_arch = "powerpc64")))]
    #[inline(always)]
    pub fn append_u32(&mut self, i: u32) -> std::io::Result<()> {
        let ptr = self.0;
        let end = ptr + 4;
        if end <= L {
            self.0 = end;
            self.1[ptr..end].copy_from_slice(&i.to_be_bytes());
            Ok(())
        } else {
            Err(overflow_err())
        }
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64", target_arch = "powerpc64"))]
    #[inline(always)]
    pub fn append_u64(&mut self, i: u64) -> std::io::Result<()> {
        let ptr = self.0;
        let end = ptr + 8;
        if end <= L {
            self.0 = end;
            unsafe { *self.1.as_mut_ptr().add(ptr).cast::<u64>() = i.to_be() };
            Ok(())
        } else {
            Err(overflow_err())
        }
    }

    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64", target_arch = "powerpc64")))]
    #[inline(always)]
    pub fn append_u64(&mut self, i: u64) -> std::io::Result<()> {
        let ptr = self.0;
        let end = ptr + 8;
        if end <= L {
            self.0 = end;
            self.1[ptr..end].copy_from_slice(&i.to_be_bytes());
            Ok(())
        } else {
            Err(overflow_err())
        }
    }

    /// Get a structure at a given position in the buffer.
    #[inline(always)]
    pub fn struct_at<T: FlatBlob>(&self, ptr: usize) -> std::io::Result<&T> {
        if (ptr + size_of::<T>()) <= self.0 {
            unsafe { Ok(&*self.1.as_ptr().cast::<u8>().offset(ptr as isize).cast::<T>()) }
        } else {
            Err(overflow_err())
        }
    }

    /// Get a structure at a given position in the buffer.
    #[inline(always)]
    pub fn struct_mut_at<T: FlatBlob>(&mut self, ptr: usize) -> std::io::Result<&mut T> {
        if (ptr + size_of::<T>()) <= self.0 {
            unsafe { Ok(&mut *self.1.as_mut_ptr().cast::<u8>().offset(ptr as isize).cast::<T>()) }
        } else {
            Err(overflow_err())
        }
    }

    #[inline(always)]
    pub fn u8_at(&self, ptr: usize) -> std::io::Result<u8> {
        if ptr < self.0 {
            Ok(self.1[ptr])
        } else {
            Err(overflow_err())
        }
    }

    /// Get a structure at a given position in the buffer and advance the cursor.
    #[inline(always)]
    pub fn read_struct<T: FlatBlob>(&self, cursor: &mut usize) -> std::io::Result<&T> {
        let ptr = *cursor;
        let end = ptr + size_of::<T>();
        debug_assert!(end <= L);
        if end <= self.0 {
            *cursor = end;
            unsafe { Ok(&*self.1.as_ptr().cast::<u8>().offset(ptr as isize).cast::<T>()) }
        } else {
            Err(overflow_err())
        }
    }

    #[inline(always)]
    pub fn read_bytes_fixed<const S: usize>(&self, cursor: &mut usize) -> std::io::Result<&[u8; S]> {
        let ptr = *cursor;
        let end = ptr + S;
        debug_assert!(end <= L);
        if end <= self.0 {
            *cursor = end;
            unsafe { Ok(&*self.1.as_ptr().cast::<u8>().offset(ptr as isize).cast::<[u8; S]>()) }
        } else {
            Err(overflow_err())
        }
    }

    #[inline(always)]
    pub fn read_bytes(&self, l: usize, cursor: &mut usize) -> std::io::Result<&[u8]> {
        let ptr = *cursor;
        let end = ptr + l;
        debug_assert!(end <= L);
        if end <= self.0 {
            *cursor = end;
            Ok(&self.1[ptr..end])
        } else {
            Err(overflow_err())
        }
    }

    #[inline(always)]
    pub fn read_varint(&self, cursor: &mut usize) -> std::io::Result<u64> {
        let c = *cursor;
        if c < self.0 {
            let mut a = &self.1[c..];
            crate::util::varint::read(&mut a).map(|r| {
                *cursor = c + r.1;
                debug_assert!(*cursor < self.0);
                r.0
            })
        } else {
            Err(overflow_err())
        }
    }

    #[inline(always)]
    pub fn read_u8(&self, cursor: &mut usize) -> std::io::Result<u8> {
        let ptr = *cursor;
        debug_assert!(ptr < L);
        if ptr < self.0 {
            *cursor = ptr + 1;
            Ok(self.1[ptr])
        } else {
            Err(overflow_err())
        }
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64"))]
    #[inline(always)]
    pub fn read_u16(&self, cursor: &mut usize) -> std::io::Result<u16> {
        let ptr = *cursor;
        let end = ptr + 2;
        debug_assert!(end <= L);
        if end <= self.0 {
            *cursor = end;
            Ok(u16::from_be(unsafe { *self.1.as_ptr().add(ptr).cast::<u16>() }))
        } else {
            Err(overflow_err())
        }
    }

    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64", target_arch = "powerpc64")))]
    #[inline(always)]
    pub fn read_u16(&self, cursor: &mut usize) -> std::io::Result<u16> {
        let ptr = *cursor;
        let end = ptr + 2;
        debug_assert!(end <= L);
        if end <= self.0 {
            *cursor = end;
            Ok(u16::from_be_bytes(*self.1[ptr..end]))
        } else {
            Err(overflow_err())
        }
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64", target_arch = "powerpc64"))]
    #[inline(always)]
    pub fn read_u32(&self, cursor: &mut usize) -> std::io::Result<u32> {
        let ptr = *cursor;
        let end = ptr + 4;
        debug_assert!(end <= L);
        if end <= self.0 {
            *cursor = end;
            Ok(u32::from_be(unsafe { *self.1.as_ptr().add(ptr).cast::<u32>() }))
        } else {
            Err(overflow_err())
        }
    }

    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64", target_arch = "powerpc64")))]
    #[inline(always)]
    pub fn read_u32(&self, cursor: &mut usize) -> std::io::Result<u16> {
        let ptr = *cursor;
        let end = ptr + 4;
        debug_assert!(end <= L);
        if end <= self.0 {
            *cursor = end;
            Ok(u32::from_be_bytes(*self.1[ptr..end]))
        } else {
            Err(overflow_err())
        }
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64", target_arch = "powerpc64"))]
    #[inline(always)]
    pub fn read_u64(&self, cursor: &mut usize) -> std::io::Result<u64> {
        let ptr = *cursor;
        let end = ptr + 8;
        debug_assert!(end <= L);
        if end <= self.0 {
            *cursor = end;
            Ok(u64::from_be(unsafe { *self.1.as_ptr().add(ptr).cast::<u64>() }))
        } else {
            Err(overflow_err())
        }
    }

    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64", target_arch = "powerpc64")))]
    #[inline(always)]
    pub fn read_u64(&self, cursor: &mut usize) -> std::io::Result<u16> {
        let ptr = *cursor;
        let end = ptr + 8;
        debug_assert!(end <= L);
        if end <= self.0 {
            *cursor = end;
            Ok(u64::from_be_bytes(*self.1[ptr..end]))
        } else {
            Err(overflow_err())
        }
    }
}

impl<const L: usize> PartialEq for Buffer<L> {
    #[inline(always)]
    fn eq(&self, other: &Self) -> bool {
        self.1[0..self.0].eq(&other.1[0..other.0])
    }
}

impl<const L: usize> Eq for Buffer<L> {}

impl<const L: usize> Write for Buffer<L> {
    #[inline(always)]
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let ptr = self.0;
        let end = ptr + buf.len();
        if end <= L {
            self.0 = end;
            self.1[ptr..end].copy_from_slice(buf);
            Ok(buf.len())
        } else {
            Err(overflow_err())
        }
    }

    #[inline(always)]
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<const L: usize> AsRef<[u8]> for Buffer<L> {
    #[inline(always)]
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl<const L: usize> AsMut<[u8]> for Buffer<L> {
    #[inline(always)]
    fn as_mut(&mut self) -> &mut [u8] {
        self.as_bytes_mut()
    }
}

pub struct PooledBufferFactory<const L: usize>;

impl<const L: usize> PooledBufferFactory<L> {
    #[inline(always)]
    pub fn new() -> Self {
        Self {}
    }
}

impl<const L: usize> PoolFactory<Buffer<L>> for PooledBufferFactory<L> {
    #[inline(always)]
    fn create(&self) -> Buffer<L> {
        Buffer::new()
    }

    #[inline(always)]
    fn reset(&self, obj: &mut Buffer<L>) {
        obj.clear();
    }
}
