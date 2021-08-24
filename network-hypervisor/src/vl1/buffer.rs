use std::io::Write;
use std::mem::size_of;

use crate::util::pool::PoolFactory;

/// Annotates a structure as containing only primitive types.
///
/// This indicates structs that are safe to abuse like raw memory by casting from
/// byte arrays of the same size, etc. It also generally implies packed representation
/// and alignment should not be assumed since these can be fetched using struct
/// extracting methods of Buffer that do not check alignment.
pub unsafe trait RawObject: Sized {}

/// A safe bounds checked I/O buffer with extensions for convenient appending of RawObject types.
pub struct Buffer<const L: usize>(usize, [u8; L]);

unsafe impl<const L: usize> RawObject for Buffer<L> {}

impl<const L: usize> Default for Buffer<L> {
    #[inline(always)]
    fn default() -> Self { Self(0, [0_u8; L]) }
}

const OVERFLOW_ERR_MSG: &'static str = "overflow";

impl<const L: usize> Buffer<L> {
    #[inline(always)]
    pub fn new() -> Self { Self(0, [0_u8; L]) }

    /// Get a Buffer initialized with a copy of a byte slice.
    #[inline(always)]
    pub fn from_bytes(b: &[u8]) -> std::io::Result<Self> {
        let l = b.len();
        if l <= L {
            let mut tmp = Self::new();
            tmp.0 = l;
            tmp.1[0..l].copy_from_slice(b);
            Ok(tmp)
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, OVERFLOW_ERR_MSG))
        }
    }

    #[inline(always)]
    pub fn as_bytes(&self) -> &[u8] { &self.1[0..self.0] }

    #[inline(always)]
    pub fn as_bytes_mut(&mut self) -> &mut [u8] { &mut self.1[0..self.0] }

    /// Get all bytes after a given position.
    #[inline(always)]
    pub fn as_bytes_starting_at(&self, start: usize) -> std::io::Result<&[u8]> {
        if start <= self.0 {
            Ok(&self.1[start..])
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, OVERFLOW_ERR_MSG))
        }
    }

    #[inline(always)]
    pub fn clear(&mut self) {
        let prev_len = self.0;
        self.0 = 0;
        self.1[0..prev_len].fill(0);
    }

    #[inline(always)]
    pub fn len(&self) -> usize { self.0 }

    #[inline(always)]
    pub fn is_empty(&self) -> bool { self.0 == 0 }

    /// Set the size of this buffer's data.
    ///
    /// If the new size is larger than L (the capacity) it will be limited to L. Any new
    /// space will be filled with zeroes.
    #[inline(always)]
    pub fn set_size(&mut self, s: usize) {
        let prev_len = self.0;
        if s < prev_len {
            self.0 = s;
            self.1[s..prev_len].fill(0);
        } else {
            self.0 = s.min(L);
        }
    }

    #[inline(always)]
    pub unsafe fn set_size_unchecked(&mut self, s: usize) { self.0 = s; }

    /// Append a packed structure and call a function to initialize it in place.
    /// Anything not initialized will be zero.
    #[inline(always)]
    pub fn append_and_init_struct<T: RawObject, R, F: FnOnce(&mut T) -> R>(&mut self, initializer: F) -> std::io::Result<R> {
        let ptr = self.0;
        let end = ptr + size_of::<T>();
        if end <= L {
            self.0 = end;
            unsafe {
                Ok(initializer(&mut *self.1.as_mut_ptr().cast::<u8>().offset(ptr as isize).cast::<T>()))
            }
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, OVERFLOW_ERR_MSG))
        }
    }

    /// Append and initialize a byte array with a fixed size set at compile time.
    /// This is more efficient than setting a size at runtime as it may allow the compiler to
    /// skip some bounds checking. Any bytes not initialized will be zero.
    #[inline(always)]
    pub fn append_and_init_bytes_fixed<R, F: FnOnce(&mut [u8; N]) -> R, const N: usize>(&mut self, initializer: F) -> std::io::Result<R> {
        let ptr = self.0;
        let end = ptr + N;
        if end <= L {
            self.0 = end;
            unsafe {
                Ok(initializer(&mut *self.1.as_mut_ptr().cast::<u8>().offset(ptr as isize).cast::<[u8; N]>()))
            }
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, OVERFLOW_ERR_MSG))
        }
    }

    /// Append and initialize a slice with a size that is set at runtime.
    /// Any bytes not initialized will be zero.
    #[inline(always)]
    pub fn append_and_init_bytes<R, F: FnOnce(&mut [u8]) -> R>(&mut self, l: usize, initializer: F) -> std::io::Result<R> {
        let ptr = self.0;
        let end = ptr + l;
        if end <= L {
            self.0 = end;
            Ok(initializer(&mut self.1[ptr..end]))
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, OVERFLOW_ERR_MSG))
        }
    }

    /// Append a dynamic byte slice (copy into buffer).
    #[inline(always)]
    pub fn append_bytes(&mut self, buf: &[u8]) -> std::io::Result<()> {
        let ptr = self.0;
        let end = ptr + buf.len();
        if end <= L {
            self.0 = end;
            self.1[ptr..end].copy_from_slice(buf);
            Ok(())
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, OVERFLOW_ERR_MSG))
        }
    }

    /// Append a fixed length byte array (copy into buffer).
    /// Use append_and_init_ functions if possible as these avoid extra copies.
    #[inline(always)]
    pub fn append_bytes_fixed<const S: usize>(&mut self, buf: &[u8; S]) -> std::io::Result<()> {
        let ptr = self.0;
        let end = ptr + S;
        if end <= L {
            self.0 = end;
            self.1[ptr..end].copy_from_slice(buf);
            Ok(())
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, OVERFLOW_ERR_MSG))
        }
    }

    /// Append a variable length integer to this buffer.
    ///
    /// Varints are encoded as a series of 7-bit bytes terminated by a final 7-bit byte whose
    /// most significant bit is set. Unlike fixed size integers varints are written in little
    /// endian order (in 7-bit chunks).
    ///
    /// They are slower than fixed size values so they should not be used in formats that are
    /// created or parsed in very speed-critical paths.
    pub fn append_varint(&mut self, mut i: u64) -> std::io::Result<()> {
        while i >= 0x80 {
            self.append_u8((i as u8) & 0x7f)?;
            i >>= 7;
        }
        self.append_u8((i as u8) | 0x80)
    }

    /// Append a byte
    #[inline(always)]
    pub fn append_u8(&mut self, i: u8) -> std::io::Result<()> {
        let ptr = self.0;
        if ptr < L {
            self.0 = ptr + 1;
            self.1[ptr] = i;
            Ok(())
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, OVERFLOW_ERR_MSG))
        }
    }

    /// Append a 16-bit integer (in big-endian form)
    #[inline(always)]
    pub fn append_u16(&mut self, i: u16) -> std::io::Result<()> {
        let ptr = self.0;
        let end = ptr + 2;
        if end <= L {
            self.0 = end;
            crate::util::store_u16_be(i, &mut self.1[ptr..end]);
            Ok(())
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, OVERFLOW_ERR_MSG))
        }
    }

    /// Append a 32-bit integer (in big-endian form)
    #[inline(always)]
    pub fn append_u32(&mut self, i: u32) -> std::io::Result<()> {
        let ptr = self.0;
        let end = ptr + 4;
        if end <= L {
            self.0 = end;
            crate::util::store_u32_be(i, &mut self.1[ptr..end]);
            Ok(())
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, OVERFLOW_ERR_MSG))
        }
    }

    /// Append a 64-bit integer (in big-endian form)
    #[inline(always)]
    pub fn append_u64(&mut self, i: u64) -> std::io::Result<()> {
        let ptr = self.0;
        let end = ptr + 8;
        if end <= L {
            self.0 = end;
            crate::util::store_u64_be(i, &mut self.1[ptr..end]);
            Ok(())
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, OVERFLOW_ERR_MSG))
        }
    }

    /// Get a structure at a given position in the buffer.
    #[inline(always)]
    pub fn struct_at<T: RawObject>(&self, ptr: usize) -> std::io::Result<&T> {
        if (ptr + size_of::<T>()) <= self.0 {
            unsafe {
                Ok(&*self.1.as_ptr().cast::<u8>().offset(ptr as isize).cast::<T>())
            }
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, OVERFLOW_ERR_MSG))
        }
    }

    /// Get a structure at a given position in the buffer.
    #[inline(always)]
    pub fn struct_mut_at<T: RawObject>(&mut self, ptr: usize) -> std::io::Result<&mut T> {
        if (ptr + size_of::<T>()) <= self.0 {
            unsafe {
                Ok(&mut *self.1.as_mut_ptr().cast::<u8>().offset(ptr as isize).cast::<T>())
            }
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, OVERFLOW_ERR_MSG))
        }
    }

    /// Get a byte at a fixed position.
    #[inline(always)]
    pub fn u8_at(&self, ptr: usize) -> std::io::Result<u8> {
        if ptr < self.0 {
            Ok(self.1[ptr])
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, OVERFLOW_ERR_MSG))
        }
    }

    /// Get a structure at a given position in the buffer and advance the cursor.
    #[inline(always)]
    pub fn read_struct<T: RawObject>(&self, cursor: &mut usize) -> std::io::Result<&T> {
        let ptr = *cursor;
        let end = ptr + size_of::<T>();
        debug_assert!(end <= L);
        if end <= self.0 {
            *cursor = end;
            unsafe {
                Ok(&*self.1.as_ptr().cast::<u8>().offset(ptr as isize).cast::<T>())
            }
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, OVERFLOW_ERR_MSG))
        }
    }

    /// Get a fixed length byte array and advance the cursor.
    /// This is slightly more efficient than reading a runtime sized byte slice.
    #[inline(always)]
    pub fn read_bytes_fixed<const S: usize>(&self, cursor: &mut usize) -> std::io::Result<&[u8; S]> {
        let ptr = *cursor;
        let end = ptr + S;
        debug_assert!(end <= L);
        if end <= self.0 {
            *cursor = end;
            unsafe {
                Ok(&*self.1.as_ptr().cast::<u8>().offset(ptr as isize).cast::<[u8; S]>())
            }
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, OVERFLOW_ERR_MSG))
        }
    }

    /// Get a runtime specified length byte slice and advance the cursor.
    #[inline(always)]
    pub fn read_bytes(&self, l: usize, cursor: &mut usize) -> std::io::Result<&[u8]> {
        let ptr = *cursor;
        let end = ptr + l;
        debug_assert!(end <= L);
        if end <= self.0 {
            *cursor = end;
            Ok(&self.1[ptr..end])
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, OVERFLOW_ERR_MSG))
        }
    }

    /// Get the next variable length integer and advance the cursor by its length in bytes.
    pub fn read_varint(&self, cursor: &mut usize) -> std::io::Result<u64> {
        let mut i = 0_u64;
        let mut p = 0;
        loop {
            let b = self.read_u8(cursor)?;
            if (b & 0x80) == 0 {
                i |= (b as u64).wrapping_shl(p);
                p += 7;
            } else {
                i |= ((b & 0x7f) as u64) << p;
                break;
            }
        }
        Ok(i)
    }

    /// Get the next u8 and advance the cursor.
    #[inline(always)]
    pub fn read_u8(&self, cursor: &mut usize) -> std::io::Result<u8> {
        let ptr = *cursor;
        debug_assert!(ptr < L);
        if ptr < self.0 {
            *cursor = ptr + 1;
            Ok(self.1[ptr])
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, OVERFLOW_ERR_MSG))
        }
    }

    /// Get the next u16 and advance the cursor.
    #[inline(always)]
    pub fn read_u16(&self, cursor: &mut usize) -> std::io::Result<u16> {
        let ptr = *cursor;
        let end = ptr + 2;
        debug_assert!(end <= L);
        if end <= self.0 {
            *cursor = end;
            Ok(crate::util::load_u16_be(&self.1[ptr..end]))
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, OVERFLOW_ERR_MSG))
        }
    }

    /// Get the next u32 and advance the cursor.
    #[inline(always)]
    pub fn read_u32(&self, cursor: &mut usize) -> std::io::Result<u32> {
        let ptr = *cursor;
        let end = ptr + 4;
        debug_assert!(end <= L);
        if end <= self.0 {
            *cursor = end;
            Ok(crate::util::load_u32_be(&self.1[ptr..end]))
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, OVERFLOW_ERR_MSG))
        }
    }

    /// Get the next u64 and advance the cursor.
    #[inline(always)]
    pub fn read_u64(&self, cursor: &mut usize) -> std::io::Result<u64> {
        let ptr = *cursor;
        let end = ptr + 8;
        debug_assert!(end <= L);
        if end <= self.0 {
            *cursor = end;
            Ok(crate::util::load_u64_be(&self.1[ptr..end]))
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, OVERFLOW_ERR_MSG))
        }
    }
}

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
            Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, OVERFLOW_ERR_MSG))
        }
    }

    #[inline(always)]
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

impl<const L: usize> AsRef<[u8]> for Buffer<L> {
    #[inline(always)]
    fn as_ref(&self) -> &[u8] { self.as_bytes() }
}

impl<const L: usize> AsMut<[u8]> for Buffer<L> {
    #[inline(always)]
    fn as_mut(&mut self) -> &mut [u8] { self.as_bytes_mut() }
}

pub struct PooledBufferFactory<const L: usize>;

impl<const L: usize> PoolFactory<Buffer<L>> for PooledBufferFactory<L> {
    #[inline(always)]
    fn create(&self) -> Buffer<L> { Buffer::new() }

    #[inline(always)]
    fn reset(&self, obj: &mut Buffer<L>) { obj.clear(); }
}
