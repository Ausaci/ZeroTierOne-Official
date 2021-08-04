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

#[cfg(all(target_pointer_width = "32"))]
use std::sync::Mutex;

#[cfg(all(target_pointer_width = "64"))]
use std::sync::atomic::{AtomicI64, Ordering};

// This implements a basic atomic i64 that uses a mutex on 32-bit systems,
// since you can't atomically access something larger than word size in most
// cases.

#[cfg(all(target_pointer_width = "32"))]
pub struct PortableAtomicI64 {
    i: Mutex<i64>
}

#[cfg(all(target_pointer_width = "32"))]
impl PortableAtomicI64 {
    #[inline(always)]
    pub fn new(v: i64) -> PortableAtomicI64 {
        PortableAtomicI64{
            i: Mutex::new(v)
        }
    }

    #[inline(always)]
    pub fn get(&self) -> i64 {
        *self.i.lock().unwrap()
    }

    #[inline(always)]
    pub fn set(&self, v: i64) {
        *self.i.lock().unwrap() = v;
    }

    #[inline(always)]
    pub fn fetch_add(&self, v: i64) -> i64 {
        let i = self.i.lock().unwrap();
        let j = *i;
        *i += v;
        j
    }
}

#[cfg(all(target_pointer_width = "64"))]
pub struct PortableAtomicI64 {
    i: AtomicI64
}

#[cfg(all(target_pointer_width = "64"))]
impl PortableAtomicI64 {
    #[inline(always)]
    pub fn new(v: i64) -> PortableAtomicI64 {
        PortableAtomicI64{
            i: AtomicI64::new(v)
        }
    }

    #[inline(always)]
    pub fn get(&self) -> i64 {
        self.i.load(Ordering::Relaxed)
    }

    #[inline(always)]
    pub fn set(&self, v: i64) {
        self.i.store(v, Ordering::Relaxed)
    }

    #[inline(always)]
    pub fn fetch_add(&self, v: i64) -> i64 {
        self.i.fetch_add(v, Ordering::Relaxed)
    }
}
