/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 *
 * (c)2021 ZeroTier, Inc.
 * https://www.zerotier.com/
 */

use std::alloc::{alloc, dealloc, Layout};
use std::cmp::Ordering;
use std::convert::TryInto;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::mem::MaybeUninit;
use std::ptr::{slice_from_raw_parts, slice_from_raw_parts_mut};
use std::str::FromStr;

use lazy_static::lazy_static;

use zerotier_core_crypto::c25519::*;
use zerotier_core_crypto::hash::{hmac_sha512, SHA384, SHA384_HASH_SIZE, SHA512, SHA512_HASH_SIZE};
use zerotier_core_crypto::hex;
use zerotier_core_crypto::p384::*;
use zerotier_core_crypto::salsa::Salsa;
use zerotier_core_crypto::secret::Secret;

use crate::error::{InvalidFormatError, InvalidParameterError};
use crate::util::buffer::Buffer;
use crate::util::pool::{Pool, Pooled, PoolFactory};
use crate::vl1::Address;
use crate::vl1::protocol::{ADDRESS_SIZE, ADDRESS_SIZE_STRING, IDENTITY_POW_THRESHOLD};

/// Curve25519 and Ed25519
pub const IDENTITY_ALGORITHM_X25519: u8 = 0x01;

/// NIST P-384 ECDH and ECDSA
pub const IDENTITY_ALGORITHM_EC_NIST_P384: u8 = 0x02;

/// Bit mask to include all algorithms.
pub const IDENTITY_ALGORITHM_ALL: u8 = 0xff;

/// Current sanity limit for the size of a marshaled Identity (can be increased if needed).
pub const MAX_MARSHAL_SIZE: usize = ADDRESS_SIZE + C25519_PUBLIC_KEY_SIZE + ED25519_PUBLIC_KEY_SIZE + C25519_SECRET_KEY_SIZE + ED25519_SECRET_KEY_SIZE + P384_PUBLIC_KEY_SIZE + P384_PUBLIC_KEY_SIZE + P384_SECRET_KEY_SIZE + P384_SECRET_KEY_SIZE + P384_ECDSA_SIGNATURE_SIZE + ED25519_SIGNATURE_SIZE + 16;

#[derive(Clone)]
pub struct IdentityP384Secret {
    pub ecdh: P384KeyPair,
    pub ecdsa: P384KeyPair,
}

#[derive(Clone)]
pub struct IdentityP384Public {
    pub ecdh: P384PublicKey,
    pub ecdsa: P384PublicKey,
    pub ecdsa_self_signature: [u8; P384_ECDSA_SIGNATURE_SIZE],
    pub ed25519_self_signature: [u8; ED25519_SIGNATURE_SIZE],
}

#[derive(Clone)]
pub struct IdentitySecret {
    pub c25519: C25519KeyPair,
    pub ed25519: Ed25519KeyPair,
    pub p384: Option<IdentityP384Secret>,
}

#[derive(Clone)]
pub struct Identity {
    pub address: Address,
    pub c25519: [u8; C25519_PUBLIC_KEY_SIZE],
    pub ed25519: [u8; ED25519_PUBLIC_KEY_SIZE],
    pub p384: Option<IdentityP384Public>,
    pub secret: Option<IdentitySecret>,
    pub fingerprint: [u8; SHA512_HASH_SIZE]
}

#[inline(always)]
fn concat_arrays_2<const A: usize, const B: usize, const S: usize>(a: &[u8; A], b: &[u8; B]) -> [u8; S] {
    assert_eq!(A + B, S);
    let mut tmp: [u8; S] = unsafe { MaybeUninit::uninit().assume_init() };
    tmp[..A].copy_from_slice(a);
    tmp[A..].copy_from_slice(b);
    tmp
}

#[inline(always)]
fn concat_arrays_4<const A: usize, const B: usize, const C: usize, const D: usize, const S: usize>(a: &[u8; A], b: &[u8; B], c: &[u8; C], d: &[u8; D]) -> [u8; S] {
    assert_eq!(A + B + C + D, S);
    let mut tmp: [u8; S] = unsafe { MaybeUninit::uninit().assume_init() };
    tmp[..A].copy_from_slice(a);
    tmp[A..(A + B)].copy_from_slice(b);
    tmp[(A + B)..(A + B + C)].copy_from_slice(c);
    tmp[(A + B + C)..].copy_from_slice(d);
    tmp
}

impl Identity {
    /// Generate a new identity.
    pub fn generate() -> Self {
        let mut sha = SHA512::new();
        let ed25519 = Ed25519KeyPair::generate();
        let ed25519_pub = ed25519.public_bytes();
        let address;
        let mut c25519;
        let mut c25519_pub;
        let mut genmem_pool_obj = ADDRESS_DERVIATION_MEMORY_POOL.get();
        loop {
            c25519 = C25519KeyPair::generate();
            c25519_pub = c25519.public_bytes();

            sha.update(&c25519_pub);
            sha.update(&ed25519_pub);
            let mut digest = sha.finish();
            zt_address_derivation_memory_intensive_hash(&mut digest, &mut genmem_pool_obj);

            if digest[0] < IDENTITY_POW_THRESHOLD {
                let addr = Address::from_bytes(&digest[59..64]);
                if addr.is_some() {
                    address = addr.unwrap();
                    break;
                }
            }

            sha.reset();
        }
        drop(genmem_pool_obj);

        let mut id = Self {
            address,
            c25519: c25519_pub,
            ed25519: ed25519_pub,
            p384: None,
            secret: Some(IdentitySecret {
                c25519,
                ed25519,
                p384: None,
            }),
            fingerprint: [0_u8; 64] // replaced in upgrade()
        };
        assert!(id.upgrade().is_ok());
        id
    }

    /// Upgrade older x25519-only identities to hybrid identities with both x25519 and NIST P-384 curves.
    ///
    /// The identity must contain its x25519 secret key or an error occurs. If the identity is already
    /// a new form hybrid identity nothing happens and Ok is returned.
    pub fn upgrade(&mut self) -> Result<(), InvalidParameterError> {
        if self.secret.is_none() {
            return Err(InvalidParameterError("an identity can only be upgraded if it includes its private key"));
        }
        if self.p384.is_none() {
            let p384_ecdh = P384KeyPair::generate();
            let p384_ecdsa = P384KeyPair::generate();

            let mut self_sign_buf: Vec<u8> = Vec::with_capacity(ADDRESS_SIZE + C25519_PUBLIC_KEY_SIZE + ED25519_PUBLIC_KEY_SIZE + P384_PUBLIC_KEY_SIZE + P384_PUBLIC_KEY_SIZE + P384_ECDSA_SIGNATURE_SIZE + 4);
            let _ = self_sign_buf.write_all(&self.address.to_bytes());
            let _ = self_sign_buf.write_all(&self.c25519);
            let _ = self_sign_buf.write_all(&self.ed25519);
            self_sign_buf.push(IDENTITY_ALGORITHM_EC_NIST_P384);
            let _ = self_sign_buf.write_all(p384_ecdh.public_key_bytes());
            let _ = self_sign_buf.write_all(p384_ecdsa.public_key_bytes());

            // Sign all keys including the x25519 ones with the new P-384 keys.
            let ecdsa_self_signature = p384_ecdsa.sign(self_sign_buf.as_slice());

            // Sign everything with the original ed25519 key to bind the new key pairs. Include the ECDSA
            // signature because these signatures are not deterministic. We don't want the ability to
            // make a new identity with the same address but a different fingerprint by mangling the
            // ECDSA signature in some way.
            let ed25519_self_signature = self.secret.as_ref().unwrap().ed25519.sign(self_sign_buf.as_slice());

            let _ = self.p384.insert(IdentityP384Public {
                ecdh: p384_ecdh.public_key().clone(),
                ecdsa: p384_ecdsa.public_key().clone(),
                ecdsa_self_signature,
                ed25519_self_signature,
            });
            let _ = self.secret.as_mut().unwrap().p384.insert(IdentityP384Secret {
                ecdh: p384_ecdh,
                ecdsa: p384_ecdsa,
            });

            self.fingerprint = SHA512::hash(self_sign_buf.as_slice());
        }
        return Ok(());
    }

    #[inline(always)]
    pub fn algorithms(&self) -> u8 {
        if self.p384.is_some() {
            IDENTITY_ALGORITHM_X25519 | IDENTITY_ALGORITHM_EC_NIST_P384
        } else {
            IDENTITY_ALGORITHM_X25519
        }
    }

    /// Locally check the validity of this identity.
    ///
    /// This is somewhat time consuming due to the memory-intensive work algorithm.
    pub fn validate_identity(&self) -> bool {
        if self.p384.is_some() {
            let p384 = self.p384.as_ref().unwrap();

            let mut self_sign_buf: Vec<u8> = Vec::with_capacity(ADDRESS_SIZE + 4 + C25519_PUBLIC_KEY_SIZE + ED25519_PUBLIC_KEY_SIZE + P384_PUBLIC_KEY_SIZE + P384_PUBLIC_KEY_SIZE);
            let _ = self_sign_buf.write_all(&self.address.to_bytes());
            let _ = self_sign_buf.write_all(&self.c25519);
            let _ = self_sign_buf.write_all(&self.ed25519);
            self_sign_buf.push(IDENTITY_ALGORITHM_EC_NIST_P384);
            let _ = self_sign_buf.write_all(p384.ecdh.as_bytes());
            let _ = self_sign_buf.write_all(p384.ecdsa.as_bytes());

            if !p384.ecdsa.verify(self_sign_buf.as_slice(), &p384.ecdsa_self_signature) {
                println!("foo");
                return false;
            }

            let _ = self_sign_buf.write_all(&p384.ecdsa_self_signature);
            if !ed25519_verify(&self.ed25519, &p384.ed25519_self_signature, self_sign_buf.as_slice()) {
                println!("bar");
                return false;
            }
        }

        // NOTE: fingerprint is always computed locally, so no need to check it.

        let mut sha = SHA512::new();
        sha.update(&self.c25519);
        sha.update(&self.ed25519);
        let mut digest = sha.finish();
        let mut genmem_pool_obj = ADDRESS_DERVIATION_MEMORY_POOL.get();
        zt_address_derivation_memory_intensive_hash(&mut digest, &mut genmem_pool_obj);
        drop(genmem_pool_obj);

        return digest[0] < IDENTITY_POW_THRESHOLD && Address::from_bytes(&digest[59..64]).map_or(false, |a| a == self.address);
    }

    /// Perform ECDH key agreement, returning a shared secret or None on error.
    ///
    /// An error can occur if this identity does not hold its secret portion or if either key is invalid.
    ///
    /// If both sides have NIST P-384 keys then key agreement is performed using both Curve25519 and
    /// NIST P-384 and the result is HMAC(Curve25519 secret, NIST P-384 secret).
    ///
    /// Nothing actually uses a 512-bit secret directly, but if the base secret is 512 bits then
    /// no entropy is lost when deriving secrets with a KDF. Ciphers like AES use the first 256 bits
    /// of these keys.
    pub fn agree(&self, other: &Identity) -> Option<Secret<64>> {
        self.secret.as_ref().and_then(|secret| {
            let c25519_secret = Secret(SHA512::hash(&secret.c25519.agree(&other.c25519).0));

            // FIPS note: FIPS-compliant exchange algorithms must be the last algorithms in any HKDF chain
            // for the final result to be technically FIPS compliant. Non-FIPS algorithm secrets are considered
            // a salt in the HMAC(salt, key) HKDF construction.
            if secret.p384.is_some() && other.p384.is_some() {
                secret.p384.as_ref().unwrap().ecdh.agree(&other.p384.as_ref().unwrap().ecdh).map(|p384_secret| {
                    Secret(hmac_sha512(&c25519_secret.0, &p384_secret.0))
                })
            } else {
                Some(c25519_secret)
            }
        })
    }

    /// Sign a message with this identity.
    ///
    /// If legacy_compatibility is true this generates only an ed25519 signature. Otherwise it
    /// will generate a signature using both the ed25519 key and the P-384 key if the latter
    /// is present in the identity.
    ///
    /// A return of None happens if we don't have our secret key(s) or some other error occurs.
    pub fn sign(&self, msg: &[u8], legacy_compatibility: bool) -> Option<Vec<u8>> {
        if self.secret.is_some() {
            let secret = self.secret.as_ref().unwrap();
            if legacy_compatibility {
                Some(secret.ed25519.sign_zt(msg).to_vec())
            } else if secret.p384.is_some() {
                let mut tmp: Vec<u8> = Vec::with_capacity(1 + P384_ECDSA_SIGNATURE_SIZE + ED25519_SIGNATURE_SIZE);
                tmp.push(IDENTITY_ALGORITHM_X25519 | IDENTITY_ALGORITHM_EC_NIST_P384);
                let _ = tmp.write_all(&secret.p384.as_ref().unwrap().ecdsa.sign(msg));
                let _ = tmp.write_all(&secret.ed25519.sign(msg));
                Some(tmp)
            } else {
                let mut tmp: Vec<u8> = Vec::with_capacity(1 + ED25519_SIGNATURE_SIZE);
                tmp.push(IDENTITY_ALGORITHM_X25519);
                let _ = tmp.write_all(&secret.ed25519.sign(msg));
                Some(tmp)
            }
        } else {
            None
        }
    }

    /// Verify a signature against this identity.
    pub fn verify(&self, msg: &[u8], mut signature: &[u8]) -> bool {
        if signature.len() == 96 { // legacy ed25519-only signature with hash included
            ed25519_verify(&self.ed25519, signature, msg)
        } else if signature.len() > 1 {
            let algorithms = signature[0];
            signature = &signature[1..];
            let mut ok = true;
            let mut checked = false;
            if ok && (algorithms & IDENTITY_ALGORITHM_EC_NIST_P384) != 0 && signature.len() >= P384_ECDSA_SIGNATURE_SIZE && self.p384.is_some() {
                ok = self.p384.as_ref().unwrap().ecdsa.verify(msg, &signature[..P384_ECDSA_SIGNATURE_SIZE]);
                signature = &signature[P384_ECDSA_SIGNATURE_SIZE..];
                checked = true;
            }
            if ok && (algorithms & IDENTITY_ALGORITHM_X25519) != 0 && signature.len() >= ED25519_SIGNATURE_SIZE {
                ok = ed25519_verify(&self.ed25519, &signature[..ED25519_SIGNATURE_SIZE], msg);
                signature = &signature[ED25519_SIGNATURE_SIZE..];
                checked = true;
            }
            checked && ok
        } else {
            false
        }
    }

    #[inline(always)]
    pub fn to_bytes(&self, include_algorithms: u8, include_private: bool) -> Buffer<MAX_MARSHAL_SIZE> {
        let mut b: Buffer<MAX_MARSHAL_SIZE> = Buffer::new();
        self.marshal(&mut b, include_algorithms, include_private).expect("internal error marshaling Identity");
        b
    }

    const P384_PUBLIC_AND_PRIVATE_BUNDLE_SIZE: u16 = (P384_PUBLIC_KEY_SIZE + P384_PUBLIC_KEY_SIZE + P384_ECDSA_SIGNATURE_SIZE + ED25519_SIGNATURE_SIZE + P384_SECRET_KEY_SIZE + P384_SECRET_KEY_SIZE) as u16;
    const P384_PUBLIC_ONLY_BUNDLE_SIZE: u16 = (P384_PUBLIC_KEY_SIZE + P384_PUBLIC_KEY_SIZE + P384_ECDSA_SIGNATURE_SIZE + ED25519_SIGNATURE_SIZE) as u16;

    pub fn marshal<const BL: usize>(&self, buf: &mut Buffer<BL>, include_algorithms: u8, include_private: bool) -> std::io::Result<()> {
        let algorithms = self.algorithms() & include_algorithms;
        let secret = self.secret.as_ref();

        buf.append_bytes_fixed(&self.address.to_bytes())?;
        buf.append_u8(0x00)?; // LEGACY: 0x00 here for backward compatibility
        buf.append_bytes_fixed(&self.c25519)?;
        buf.append_bytes_fixed(&self.ed25519)?;
        if include_private && secret.is_some() {
            let secret = secret.unwrap();
            buf.append_u8((C25519_SECRET_KEY_SIZE + ED25519_SECRET_KEY_SIZE) as u8)?;
            buf.append_bytes_fixed(&secret.c25519.secret_bytes().0)?;
            buf.append_bytes_fixed(&secret.ed25519.secret_bytes().0)?;
        } else {
            buf.append_u8(0)?;
        }

        if (algorithms & IDENTITY_ALGORITHM_EC_NIST_P384) != 0 && self.p384.is_some() {
            let p384 = self.p384.as_ref().unwrap();

            /*
             * For legacy backward compatibility, any key pairs and other material after the x25519
             * keys are prefixed by 0x03 followed by the total size of this section. This lets us parsimoniously
             * maintain backward compatibility with old versions' parsing of HELLO.
             *
             * In old HELLO the identity was followed by an InetAddress. The InetAddress encoding does support
             * a variable length encoding for unknown "future use" address types. This consists of 0x03 followed
             * by a 16-bit size.
             *
             * By mimicking this we can create a HELLO containing a new format identity and cleverly skip the
             * InetAddress after it and old nodes will parse this as an old x25519 only identity followed by
             * an unrecognized type InetAddress that will be ignored.
             *
             * Key agreement can then proceed using only x25519 keys.
             */
            buf.append_u8(0x03)?;
            let p384_has_private = if include_private && secret.map_or(false, |s| s.p384.is_some()) {
                buf.append_u16(Self::P384_PUBLIC_AND_PRIVATE_BUNDLE_SIZE + 1 + 2)?;
                true
            } else {
                buf.append_u16(Self::P384_PUBLIC_ONLY_BUNDLE_SIZE + 1 + 2)?;
                false
            };

            buf.append_u8(IDENTITY_ALGORITHM_EC_NIST_P384)?;
            if p384_has_private {
                buf.append_u16(Self::P384_PUBLIC_AND_PRIVATE_BUNDLE_SIZE)?;
            } else {
                buf.append_u16(Self::P384_PUBLIC_ONLY_BUNDLE_SIZE)?;
            }
            buf.append_bytes_fixed(p384.ecdh.as_bytes())?;
            buf.append_bytes_fixed(p384.ecdsa.as_bytes())?;
            buf.append_bytes_fixed(&p384.ecdsa_self_signature)?;
            buf.append_bytes_fixed(&p384.ed25519_self_signature)?;
            if p384_has_private {
                let p384s = secret.unwrap().p384.as_ref().unwrap();
                buf.append_bytes_fixed(&p384s.ecdh.secret_key_bytes().0)?;
                buf.append_bytes_fixed(&p384s.ecdsa.secret_key_bytes().0)?;
            }
        }

        Ok(())
    }

    pub fn unmarshal<const BL: usize>(buf: &Buffer<BL>, cursor: &mut usize) -> std::io::Result<Identity> {
        let address = Address::from_bytes(buf.read_bytes_fixed::<ADDRESS_SIZE>(cursor)?);
        if !address.is_some() {
            return std::io::Result::Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid address"));
        }
        let address = address.unwrap();

        let mut x25519_public: Option<([u8; C25519_PUBLIC_KEY_SIZE], [u8; ED25519_PUBLIC_KEY_SIZE])> = None;
        let mut x25519_secret: Option<([u8; C25519_SECRET_KEY_SIZE], [u8; ED25519_SECRET_KEY_SIZE])> = None;
        let mut p384_ecdh_ecdsa_public: Option<(P384PublicKey, P384PublicKey, [u8; P384_ECDSA_SIGNATURE_SIZE], [u8; ED25519_SIGNATURE_SIZE])> = None;
        let mut p384_ecdh_ecdsa_secret: Option<([u8; P384_SECRET_KEY_SIZE], [u8; P384_SECRET_KEY_SIZE])> = None;

        loop {
            let algorithm = buf.read_u8(cursor);
            if algorithm.is_err() {
                break;
            }
            match algorithm.unwrap() {
                0x00 | IDENTITY_ALGORITHM_X25519 => {
                    let a = buf.read_bytes_fixed::<C25519_PUBLIC_KEY_SIZE>(cursor)?;
                    let b = buf.read_bytes_fixed::<ED25519_PUBLIC_KEY_SIZE>(cursor)?;
                    x25519_public = Some((a.clone(), b.clone()));
                    let sec_size = buf.read_u8(cursor)?;
                    if sec_size == (C25519_SECRET_KEY_SIZE + ED25519_SECRET_KEY_SIZE) as u8 {
                        let a = buf.read_bytes_fixed::<C25519_SECRET_KEY_SIZE>(cursor)?;
                        let b = buf.read_bytes_fixed::<ED25519_SECRET_KEY_SIZE>(cursor)?;
                        x25519_secret = Some((a.clone(), b.clone()));
                    } else if sec_size != 0 {
                        return std::io::Result::Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid x25519 secret"));
                    }
                }
                0x03 => {
                    // This isn't an algorithm; each algorithm is identified by just one bit. This
                    // indicates the total size of the section after the x25519 keys for backward
                    // compatibility. See comments in marshal(). New versions can ignore this field.
                    *cursor += 2;
                }
                IDENTITY_ALGORITHM_EC_NIST_P384 => {
                    let size = buf.read_u16(cursor)?;
                    if size < Self::P384_PUBLIC_ONLY_BUNDLE_SIZE {
                        return std::io::Result::Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid p384 public key"));
                    }
                    let a = buf.read_bytes_fixed::<P384_PUBLIC_KEY_SIZE>(cursor)?;
                    let b = buf.read_bytes_fixed::<P384_PUBLIC_KEY_SIZE>(cursor)?;
                    let c = buf.read_bytes_fixed::<P384_ECDSA_SIGNATURE_SIZE>(cursor)?;
                    let d = buf.read_bytes_fixed::<ED25519_SIGNATURE_SIZE>(cursor)?;
                    let a = P384PublicKey::from_bytes(a);
                    let b = P384PublicKey::from_bytes(b);
                    if a.is_none() || b.is_none() {
                        return std::io::Result::Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid p384 public key"));
                    }
                    p384_ecdh_ecdsa_public = Some((a.unwrap(), b.unwrap(), c.clone(), d.clone()));
                    if size > Self::P384_PUBLIC_ONLY_BUNDLE_SIZE {
                        if size != Self::P384_PUBLIC_AND_PRIVATE_BUNDLE_SIZE {
                            return std::io::Result::Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid p384 secret key"));
                        }
                        let a = buf.read_bytes_fixed::<P384_SECRET_KEY_SIZE>(cursor)?;
                        let b = buf.read_bytes_fixed::<P384_SECRET_KEY_SIZE>(cursor)?;
                        p384_ecdh_ecdsa_secret = Some((a.clone(), b.clone()));
                    }
                }
                _ => {
                    // Skip any unrecognized cipher suites, all of which will be prefixed by a size.
                    *cursor += buf.read_u16(cursor)? as usize;
                    if *cursor > buf.len() {
                        return std::io::Result::Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid field length"));
                    }
                }
            }
        }

        if x25519_public.is_none() {
            return std::io::Result::Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "x25519 key missing"));
        }
        let x25519_public = x25519_public.unwrap();

        let mut sha = SHA512::new();
        sha.update(&address.to_bytes());
        sha.update(&x25519_public.0);
        sha.update(&x25519_public.1);
        if p384_ecdh_ecdsa_public.is_some() {
            let p384 = p384_ecdh_ecdsa_public.as_ref().unwrap();
            sha.update(&[IDENTITY_ALGORITHM_EC_NIST_P384]);
            sha.update(p384.0.as_bytes());
            sha.update(p384.1.as_bytes());
        }

        Ok(Identity {
            address,
            c25519: x25519_public.0.clone(),
            ed25519: x25519_public.1.clone(),
            p384: if p384_ecdh_ecdsa_public.is_some() {
                let p384_ecdh_ecdsa_public = p384_ecdh_ecdsa_public.as_ref().unwrap();
                Some(IdentityP384Public {
                    ecdh: p384_ecdh_ecdsa_public.0.clone(),
                    ecdsa: p384_ecdh_ecdsa_public.1.clone(),
                    ecdsa_self_signature: p384_ecdh_ecdsa_public.2.clone(),
                    ed25519_self_signature: p384_ecdh_ecdsa_public.3.clone(),
                })
            } else {
                None
            },
            secret: if x25519_secret.is_some() {
                let x25519_secret = x25519_secret.unwrap();
                let c25519_secret = C25519KeyPair::from_bytes(&x25519_public.0, &x25519_secret.0);
                let ed25519_secret = Ed25519KeyPair::from_bytes(&x25519_public.1, &x25519_secret.1);
                if c25519_secret.is_none() || ed25519_secret.is_none() {
                    return std::io::Result::Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "x25519 public key invalid"));
                }
                Some(IdentitySecret {
                    c25519: c25519_secret.unwrap(),
                    ed25519: ed25519_secret.unwrap(),
                    p384: if p384_ecdh_ecdsa_secret.is_some() && p384_ecdh_ecdsa_public.is_some() {
                        let p384_ecdh_ecdsa_public = p384_ecdh_ecdsa_public.as_ref().unwrap();
                        let p384_ecdh_ecdsa_secret = p384_ecdh_ecdsa_secret.as_ref().unwrap();
                        let p384_ecdh_secret = P384KeyPair::from_bytes(p384_ecdh_ecdsa_public.0.as_bytes(), &p384_ecdh_ecdsa_secret.0);
                        let p384_ecdsa_secret = P384KeyPair::from_bytes(p384_ecdh_ecdsa_public.1.as_bytes(), &p384_ecdh_ecdsa_secret.1);
                        if p384_ecdh_secret.is_none() || p384_ecdsa_secret.is_none() {
                            return std::io::Result::Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "p384 secret key invalid"));
                        }
                        Some(IdentityP384Secret {
                            ecdh: p384_ecdh_secret.unwrap(),
                            ecdsa: p384_ecdsa_secret.unwrap(),
                        })
                    } else {
                        None
                    },
                })
            } else {
                None
            },
            fingerprint: sha.finish()
        })
    }

    /// Marshal this identity as a string with options to control which ciphers are included and whether private keys are included.
    pub fn to_string_with_options(&self, include_algorithms: u8, include_private: bool) -> String {
        if include_private && self.secret.is_some() {
            let secret = self.secret.as_ref().unwrap();
            if (include_algorithms & IDENTITY_ALGORITHM_EC_NIST_P384) == IDENTITY_ALGORITHM_EC_NIST_P384 && secret.p384.is_some() && self.p384.is_some() {
                let p384_secret = secret.p384.as_ref().unwrap();
                let p384 = self.p384.as_ref().unwrap();
                let p384_secret_joined: [u8; P384_SECRET_KEY_SIZE + P384_SECRET_KEY_SIZE] = concat_arrays_2(p384_secret.ecdh.secret_key_bytes().as_bytes(), p384_secret.ecdsa.secret_key_bytes().as_bytes());
                let p384_joined: [u8; P384_PUBLIC_KEY_SIZE + P384_PUBLIC_KEY_SIZE + P384_ECDSA_SIGNATURE_SIZE + ED25519_SIGNATURE_SIZE] = concat_arrays_4(p384.ecdh.as_bytes(), p384.ecdsa.as_bytes(), &p384.ecdsa_self_signature, &p384.ed25519_self_signature);
                format!("{}:0:{}{}:{}{}:2:{}:{}",
                    self.address.to_string(),
                    hex::to_string(&self.c25519),
                    hex::to_string(&self.ed25519),
                    hex::to_string(&secret.c25519.secret_bytes().0),
                    hex::to_string(&secret.ed25519.secret_bytes().0),
                    base64::encode_config(p384_joined, base64::URL_SAFE_NO_PAD),
                    base64::encode_config(p384_secret_joined, base64::URL_SAFE_NO_PAD))
            } else {
                format!("{}:0:{}{}:{}{}",
                    self.address.to_string(),
                    hex::to_string(&self.c25519),
                    hex::to_string(&self.ed25519),
                    hex::to_string(&secret.c25519.secret_bytes().0),
                    hex::to_string(&secret.ed25519.secret_bytes().0))
            }
        } else {
            self.p384.as_ref().map_or_else(|| {
                format!("{}:0:{}{}",
                    self.address.to_string(),
                    hex::to_string(&self.c25519),hex::to_string(&self.ed25519))
            }, |p384| {
                let p384_joined: [u8; P384_PUBLIC_KEY_SIZE + P384_PUBLIC_KEY_SIZE + P384_ECDSA_SIGNATURE_SIZE + ED25519_SIGNATURE_SIZE] = concat_arrays_4(p384.ecdh.as_bytes(), p384.ecdsa.as_bytes(), &p384.ecdsa_self_signature, &p384.ed25519_self_signature);
                format!("{}:0:{}{}::2:{}",
                    self.address.to_string(),
                    hex::to_string(&self.c25519),
                    hex::to_string(&self.ed25519),
                    base64::encode_config(p384_joined, base64::URL_SAFE_NO_PAD))
            })
        }
    }

    /// Get this identity in string form with all ciphers and with secrets (if present)
    pub fn to_secret_string(&self) -> String { self.to_string_with_options(IDENTITY_ALGORITHM_ALL, true) }
}

impl ToString for Identity {
    /// Get only the public portion of this identity as a string, including all cipher suites.
    #[inline(always)]
    fn to_string(&self) -> String { self.to_string_with_options(IDENTITY_ALGORITHM_ALL, false) }
}

impl FromStr for Identity {
    type Err = InvalidFormatError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let fields_v: Vec<&str> = s.split(':').collect();
        let fields = fields_v.as_slice();

        if fields.len() < 3 || fields[0].len() != ADDRESS_SIZE_STRING {
            return Err(InvalidFormatError);
        }
        let address = Address::from_str(fields[0]).map_err(|_| InvalidFormatError)?;

        // x25519 public, x25519 secret, p384 public, p384 secret
        let mut keys: [Option<&str>; 4] = [None, None, None, None];

        let mut ptr = 1;
        let mut state = 0;
        let mut key_ptr = 0;
        while ptr < fields.len() {
            match state {
                0 => {
                    if fields[ptr] == "0" || fields[ptr] == "1" {
                        key_ptr = 0;
                    } else if fields[ptr] == "2" {
                        key_ptr = 2;
                    } else {
                        return Err(InvalidFormatError);
                    }
                    state = 1;
                }
                1 | 2 => {
                    let _ = keys[key_ptr].replace(fields[ptr]);
                    key_ptr += 1;
                    state = (state + 1) % 3;
                }
                _ => {
                    return Err(InvalidFormatError);
                }
            }
            ptr += 1;
        }

        let keys = [hex::from_string(keys[0].unwrap_or("")), hex::from_string(keys[1].unwrap_or("")), base64::decode_config(keys[2].unwrap_or(""), base64::URL_SAFE_NO_PAD).unwrap_or_else(|_| Vec::new()), base64::decode_config(keys[3].unwrap_or(""), base64::URL_SAFE_NO_PAD).unwrap_or_else(|_| Vec::new())];
        if keys[0].len() != C25519_PUBLIC_KEY_SIZE + ED25519_PUBLIC_KEY_SIZE {
            return Err(InvalidFormatError);
        }
        if !keys[2].is_empty() && keys[2].len() != P384_PUBLIC_KEY_SIZE + P384_PUBLIC_KEY_SIZE + P384_ECDSA_SIGNATURE_SIZE + ED25519_SIGNATURE_SIZE {
            return Err(InvalidFormatError);
        }
        if !keys[3].is_empty() && keys[3].len() != P384_SECRET_KEY_SIZE + P384_SECRET_KEY_SIZE {
            return Err(InvalidFormatError);
        }

        let mut sha = SHA512::new();
        sha.update(&address.to_bytes());
        sha.update(&keys[0].as_slice()[0..64]);
        if !keys[2].is_empty() {
            sha.update(&[IDENTITY_ALGORITHM_EC_NIST_P384]);
            sha.update(&keys[2].as_slice()[0..(P384_PUBLIC_KEY_SIZE * 2)]);
        }

        Ok(Identity {
            address,
            c25519: keys[0].as_slice()[0..32].try_into().unwrap(),
            ed25519: keys[0].as_slice()[32..64].try_into().unwrap(),
            p384: if keys[2].is_empty() {
                None
            } else {
                let ecdh = P384PublicKey::from_bytes(&keys[2].as_slice()[..P384_PUBLIC_KEY_SIZE]);
                let ecdsa = P384PublicKey::from_bytes(&keys[2].as_slice()[P384_PUBLIC_KEY_SIZE..(P384_PUBLIC_KEY_SIZE * 2)]);
                if ecdh.is_none() || ecdsa.is_none() {
                    return Err(InvalidFormatError);
                }
                Some(IdentityP384Public {
                    ecdh: ecdh.unwrap(),
                    ecdsa: ecdsa.unwrap(),
                    ecdsa_self_signature: keys[2].as_slice()[(P384_PUBLIC_KEY_SIZE * 2)..((P384_PUBLIC_KEY_SIZE * 2) + P384_ECDSA_SIGNATURE_SIZE)].try_into().unwrap(),
                    ed25519_self_signature: keys[2].as_slice()[((P384_PUBLIC_KEY_SIZE * 2) + P384_ECDSA_SIGNATURE_SIZE)..].try_into().unwrap(),
                })
            },
            secret: if keys[1].is_empty() {
                None
            } else {
                if keys[1].len() != C25519_SECRET_KEY_SIZE + ED25519_SECRET_KEY_SIZE {
                    return Err(InvalidFormatError);
                }
                Some(IdentitySecret {
                    c25519: {
                        let tmp = C25519KeyPair::from_bytes(&keys[0].as_slice()[0..32], &keys[1].as_slice()[0..32]);
                        if tmp.is_none() {
                            return Err(InvalidFormatError);
                        }
                        tmp.unwrap()
                    },
                    ed25519: {
                        let tmp = Ed25519KeyPair::from_bytes(&keys[0].as_slice()[32..64], &keys[1].as_slice()[32..64]);
                        if tmp.is_none() {
                            return Err(InvalidFormatError);
                        }
                        tmp.unwrap()
                    },
                    p384: if keys[3].is_empty() {
                        None
                    } else {
                        Some(IdentityP384Secret {
                            ecdh: {
                                let tmp = P384KeyPair::from_bytes(&keys[2].as_slice()[..P384_PUBLIC_KEY_SIZE], &keys[3].as_slice()[..P384_SECRET_KEY_SIZE]);
                                if tmp.is_none() {
                                    return Err(InvalidFormatError);
                                }
                                tmp.unwrap()
                            },
                            ecdsa: {
                                let tmp = P384KeyPair::from_bytes(&keys[2].as_slice()[P384_PUBLIC_KEY_SIZE..(P384_PUBLIC_KEY_SIZE * 2)], &keys[3].as_slice()[P384_SECRET_KEY_SIZE..]);
                                if tmp.is_none() {
                                    return Err(InvalidFormatError);
                                }
                                tmp.unwrap()
                            },
                        })
                    },
                })
            },
            fingerprint: sha.finish()
        })
    }
}

impl PartialEq for Identity {
    #[inline(always)]
    fn eq(&self, other: &Self) -> bool { self.fingerprint == other.fingerprint }
}

impl Eq for Identity {}

impl Ord for Identity {
    fn cmp(&self, other: &Self) -> Ordering { self.address.cmp(&other.address).then_with(|| self.fingerprint.cmp(&other.fingerprint)) }
}

impl PartialOrd for Identity {
    #[inline(always)]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}

impl Hash for Identity {
    #[inline(always)]
    fn hash<H: Hasher>(&self, state: &mut H) { state.write_u64(self.address.to_u64()) }
}

const ADDRESS_DERIVATION_HASH_MEMORY_SIZE: usize = 2097152;

/// This is a compound hasher used for the work function that derives an address.
///
/// FIPS note: addresses are just unique identifiers based on a hash. The actual key is
/// what truly determines node identity. For FIPS purposes this can be considered a
/// non-cryptographic hash. Its memory hardness and use in a work function is a defense
/// in depth feature rather than a primary security feature.
fn zt_address_derivation_memory_intensive_hash(digest: &mut [u8; 64], genmem_pool_obj: &mut Pooled<AddressDerivationMemory, AddressDerivationMemoryFactory>) {
    let genmem_ptr: *mut u8 = genmem_pool_obj.get_memory();
    let (genmem, genmem_alias_hack) = unsafe { (&mut *slice_from_raw_parts_mut(genmem_ptr, ADDRESS_DERIVATION_HASH_MEMORY_SIZE), &*slice_from_raw_parts(genmem_ptr, ADDRESS_DERIVATION_HASH_MEMORY_SIZE)) };
    let genmem_u64_ptr = genmem_ptr.cast::<u64>();

    let mut s20 = Salsa::<20>::new(&digest[0..32], &digest[32..40]);

    s20.crypt(&crate::util::ZEROES[0..64], &mut genmem[0..64]);
    let mut i: usize = 64;
    while i < ADDRESS_DERIVATION_HASH_MEMORY_SIZE {
        let ii = i + 64;
        s20.crypt(&genmem_alias_hack[(i - 64)..i], &mut genmem[i..ii]);
        i = ii;
    }

    i = 0;
    while i < (ADDRESS_DERIVATION_HASH_MEMORY_SIZE / 8) {
        unsafe {
            let idx1 = (((*genmem_u64_ptr.add(i)).to_be() & 7) * 8) as usize;
            let idx2 = ((*genmem_u64_ptr.add(i + 1)).to_be() % (ADDRESS_DERIVATION_HASH_MEMORY_SIZE as u64 / 8)) as usize;
            let genmem_u64_at_idx2_ptr = genmem_u64_ptr.add(idx2);
            let tmp = *genmem_u64_at_idx2_ptr;
            let digest_u64_ptr = digest.as_mut_ptr().add(idx1).cast::<u64>();
            *genmem_u64_at_idx2_ptr = *digest_u64_ptr;
            *digest_u64_ptr = tmp;
        }
        s20.crypt_in_place(digest);
        i += 2;
    }
}

#[repr(transparent)]
struct AddressDerivationMemory(*mut u8);

impl AddressDerivationMemory {
    #[inline(always)]
    fn get_memory(&mut self) -> *mut u8 { self.0 }
}

impl Drop for AddressDerivationMemory {
    #[inline(always)]
    fn drop(&mut self) { unsafe { dealloc(self.0, Layout::from_size_align(ADDRESS_DERIVATION_HASH_MEMORY_SIZE, 8).unwrap()) }; }
}

struct AddressDerivationMemoryFactory;

impl PoolFactory<AddressDerivationMemory> for AddressDerivationMemoryFactory {
    #[inline(always)]
    fn create(&self) -> AddressDerivationMemory { AddressDerivationMemory(unsafe { alloc(Layout::from_size_align(ADDRESS_DERIVATION_HASH_MEMORY_SIZE, 8).unwrap()) }) }

    #[inline(always)]
    fn reset(&self, _: &mut AddressDerivationMemory) {}
}

lazy_static! {
    static ref ADDRESS_DERVIATION_MEMORY_POOL: Pool<AddressDerivationMemory, AddressDerivationMemoryFactory> = Pool::new(0, AddressDerivationMemoryFactory);
}

/// Purge the memory pool used to verify identities. This can be called periodically
/// from the maintenance function to prevent memory buildup from bursts of identity
/// verification.
#[inline(always)]
pub(crate) fn purge_verification_memory_pool() {
    ADDRESS_DERVIATION_MEMORY_POOL.purge();
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::time::{Duration, SystemTime};
    use zerotier_core_crypto::hex;
    use crate::vl1::identity::{Identity, IDENTITY_ALGORITHM_ALL};

    const GOOD_V0_IDENTITIES: [&'static str; 10] = [
        "51ef313c3a:0:79fee239cf79833be3a9068565661dc33e04759fa0f7e2218d10f1a51d441f1bf71332eba26dfc3755ce60e14650fe68dede66cf145e429972a7f51e026374de:6d12b1c5e0eae3983a5ee5872fa9061963d9e2f8cdd85adab54bdec4bd67f538cafc91b8b5b93fca658a630aab030ec10d66235f2443ccf362c55c41ae01b46e",
        "9532db97eb:0:86a2c3a7d08be09f794188ef86014f54b699577536db1ded58537c9159020b48c962ff7f25501ada8ef20b604dd29fb1a915966aaffe1ef6a589527525599f10:06ab13d2704583451bb326feb5c3d9bfe7879aa327669ff33150a42c04464aa5435cec79d952e0af970142e9d8c8a0dd26deadf9b9ba2f1cb454bf2ac22e53e6",
        "361d9b8016:0:5935e0d38e690a992d22fdbb587b873e9b6de4de4a45d161c47f249a2dbeed44e917da80736c8c3b61cdcc5a3f0a2c77fc8fa41c1302fa7bb871fe5833f9995f:7cfb67189c36e2588682a065db769a3827f423d099a84c61f30b5ad41c2e51a4c750235820441a524a011facad4555869042750684b01d6eca4b86223e816569",
        "77f925e5e3:0:161678a69aa19d1de096cd9cd7801745f038f74c3680f28da0890c995ecf56408c1f6022a02ab20c68e21b1afc587a0038f1405cbd3167877a69926788e92620:2e1a73ffb750f201451f5c35693179cfa0de14404c8d55e6bb5749787e7e220b292f9193f454b2e97404c5d136cff665874373e9a6d5139efa1b904f19efc7d3",
        "e32e883ac6:0:29e41f935cf419d41103a748938ab0dcc978b6fde9fbb82d6f34ef124538f93dc680c8b26ba03f0c66d15be1a3895ef73dc6879843f3720095fa144d33369195:3654e04cac0beb98a94b97bca2b9a0aea4c7001e13c3ebe813fe8096395ecb69b824d3b6ee2d5b149077abd73cff61dd9ee04811c30b0c7f964b59c67eefa799",
        "6f66865615:0:11443c5c0a6a096245f9790240e15d3b8ea228397447f118bd8b44030b24191f97e11bf704807561cd6d54f627d57d599ca7983547c6d4db52597dbd1c86114b:1d1cb5bbced28b11f2f61ddcbc9693d0233485fc8fe0825c090a7309fe94fd26e8e89d137071ef7567b80cce60672a31da4c1677fa1c37237b0713456788dc81",
        "c5bdb4a6a6:0:e0a8575bc0277ecf59aaa4a2724acc55554151fff510c8211b0b863398a04224ed918c16405552336ad4c4da3b98eb6224574f1cacaa69e19cdfde184fd9292d:0d45f17d73337cc1898f7be6aae54a050b39ed0259b608b80619c3f898caf8a3a48ae56e51c3d7d8426ef295c0628d81b1a99616a3ed28da49bf8f81e1bec863",
        "c622dedbe4:0:0cfec354be26b4b2fa9ea29166b4acaf9476d169d51fd741d7e4cd9de93f321c6b80628c50da566d0a6b07d58d651eba8af63e0edc36202c05c3f97c828788ad:31a75d2b46c1b0f33228d3869bc807b42b371bbcef4c96f7232a27c62f56397568558f115d9cff3d6f7b8efb726a1ea49a591662d9aacd1049e295cbb0cf3197",
        "e28829ab3c:0:8e36c4f6cb524cae6bbea5f26dadb601a76f2a3793961779317365effb17ac6cde4ff4149a1b3480fbdbdbabfe62e1f264e764f95540b63158d1ea8b1eb0df5b:957508a7546df18784cd285da2e6216e4265906c6c7fba9a895f29a724d63a2e0268128c0c9c2cc304c8c3304863cdfe437a7b93b12dc778c0372a116088e9cd",
        "aec623e59d:0:d7b1a715d95490611b8d467bbee442e3c88949f677371d3692da92f5b23d9e01bb916596cc1ddd2d5e0e5ecd6c750bb71ad2ba594b614b771c6f07b39dbe4126:ae4e4759d67158dcc54ede8c8ddb08acac49baf8b816883fc0ac5b6e328d17ced5f05ee0b4cd20b03bc5005471795c29206b835081b873fef26d3941416bd626"
    ];
    const GOOD_V1_IDENTITIES: [&'static str; 10] = [
        "c35be0ca60:0:bd7b7d9f59fa7bd7700d7291f394dde3bda0c7a0d0da6b1992fdb4b74d4bba7ff831999362d996b0032e94f9454e636363a9ec125185edfa9451f2cb5a47e8aa::2:AiUlMlsVUa2wLr6hSL6PWb8v4n7H-6SjBz_rMNjQVlSgoHTKdrc2pTaFlvgXXVDsxgKYj6XOCAy-kPcB6gbaAEr4HC0BaRDphLi7Q3Od2NNx1Fm3A8NDrTX6agcVCxRybxRVkJdLFE3EBkLWMAzPQ1_Qr0nvSVGZ0inAbQEQFbd0j7aDgEsik2A2pYqhvPRINiIzPBqr7kwPL7OSXF-v6oNlwFJ5NhVmleioIGekapFJkjTYF0xxMR9eOwjHArGbHtWP_yiFgXcVwV4ta-ttjnGImjCuq9AaEqhhhYsYeGhya8Pd9e9obIwbTYIS1DaCd9fXPcK6vhnLi-wEtYZ3qg0A",
        "f7193a4d70:0:a796837e99841ac933c85dee615172485c87b7d9de5493c9f2fc7c5428d88833869bcdfbbf8ab0966ba3e284e131cf89b308364ed8a2af69a6986583b490cc69::2:A44ZHHXaN9bk4vssB-HuGodAbBIHmGmRmwqyD4ocUWwQWlPaQDt_y6HbNX8GmWLv5QPoMhFzuHmusOx6sAP5-2xYnUf5mpBRGTtKvW2VxG5yHNlR6olL9p2xX2okSppuYgab7X3QzTA6AiOQYCqWs-39GDA0L68q5_Vdnebfn36u3AIh0WnUlkPzkCsqF87lrgaqq_fdtE4Dz96YhvChWevDdCHhqdzjlgddk6KREpx2IPb8dQMGtwdOQZ7Kl12Ezza69PwaFsDfp47AmfsomYIB-e4kzo3G7_2me-0Zi5By9WnPtKRYHh_5HDFpjwlg9ElIjs7cyQlgz6Duc7nQHQ4E",
        "780a66fd18:0:ab09057d5ffed767bf0b7a75c8d810c878758aee85e29f80ebbb80b3aefc274b22fe7fb5757dbddd0f81621a0386a9b586b50722222b8a15fc4349f0ca102dbd::2:A7CL-xo4PA8geCRMGkDa347wtlt8vsPb5ShHQs63fDzREjqPx_ghu6lDKBZSUbfRqgPkvD-dsHbx-j-Neyvp7xF-O3TsiD_OuVmYF9MCZ8e7UeCyNN9Ao6blsHiXNlE6dL5BGcOy7kmcIGEqiLnQM_SM1iggRJI3YkpYRaGuJdu7Jpc8A0XTWVzE3RccPlbu25jvagmY4u3phR9DjA3vcaY21NLDNuyBzCXQmh66WIqjbOEcIH1Tr7JjciBdhE-cmzYqFOyGxNBDCm2oXdPHHaTMljySc526LkfiMcJilNiVrTf0-6CjccLxAXGlUU6VZz_DxPoVGRzPSj8kUbfvujwO",
        "3961fb1d69:0:360b65017c4b690f370e9ac84a7cb1b2866978dbded70e2a8ca7c72c73b7bc3d74052cc8f51307a79fd570fad104447f524b4bdac9dadd8ebf7da3672a49739d::2:A-RabnEvl-55tTWseqP3IphwMM6dCi2PMAYnabt2yjWALVYqRV27VD4_T6P0H513SwOY27dfClgv3a-vVnw7uag8p4CBPax1q-nOuvrl62w8KzUcDS5eWyZxnvkR5iFBp3Iq4oSEDBBV1pzTZlc3aoz5Muqzt4c6X3Qh8Wcahnu2f86FL9Yl5xL9_al9Jh8PfnupO6uyvXv4jSQJaRZbQbJo0gl8WRsSZwHUTWgKbZdb3RcOxyOhJhj3dFMrTr3OrwjMAoAYuREsHkj72dOBuoZs5aMMsyjS4cjjFBDijJ4RhuzAd8n0hp1a_DV5roVOj_-sdMf0dPbDDibAV9nA_QUG",
        "16c21adb85:0:8fe2f38bf5749861fabbedf7fcaee0dd79a6e6127f1e092c67ff2198b0475c771f26428438dadc3aefca1bef2e23a8cdbd1dd97f559e799e79617faa97c4da80::2:A48nTihf1f4MKVpCzyigjFNGcVeCynZTwMevsi07gfzpwTuRb1fjNmgfuyYh00X0mAMWdupKP_2rTpDd0vbcPb9aqxdUO9fqgmGHsTtudBWpjQVCueC_aITwmZhtF3ey--PV2iItyINxXeGGkXHr2EYoM5mfoZWuLhaEV0lmAk_FKZ-Mb09zfXfgNKXKCA2eQBWWBth-oHxH0vzlX86dd7D2qxsG2LY2hpn4ma2AaLHtZTKa_EsouTOxA43DlXeQxkDL3HSRjlX-ET_xae_JkPMuaMCmo0wKIOM3_6tYJkj5sKVK28gY_ziRC_27bznow9z3sJIvxVcx9MUu5nRgaakH",
        "d2586ea351:0:9c7d5f25533d042564a8b1bd76f37c27e3bca494a6693d456703ba3eadc2d94d3f131fc7914e66347f9624d6f303964714e8d9f0b03bd79eb1dac137e6b8153b::2:AwwJDn3zibov2pahZy4Do714Zq2j8w3kmHpcAqBQkvEg4cVoHjkDUCOUW8HbivJdsQK5cZRTvcitNb65Gch8OfAGVfkDHYrO5eE0Ev7EXYlRqtqXaTzdwijVd3R1hxZh8FTln75Be0JMNDVQgjNH4F_WH2KLzNf8Uy44AIXYRVvC0GF0eAx3AqxpajqA9VsyT2mF0dwoeuwyre54SXu2w7cap8s4OlhW52Fv99NhE2W2inlI7gxBC_KdinIlTYepWcnHwFJnj6ZVeLYRMZuNGDMSUPXk00wqbqfEYdWSLGvz9g9NvSAq_NGI-L7WQCB368K1teXsdL1WLzYaA2vQYYMG",
        "6ae70ee955:0:858884d8b8d863ee0fd79cd55d96034a7e1a4828f1661d362d6b2ac1e942c459a38a17907fdd268e19e82ade375a9a654e2a477a28ecaf8ce5f14c9b141d59f6::2:A0aVghbPjh5okma0NrfrArePpgX9RYj7ULLiib_DB6yh0pKYemgYY5sHmHeRrf33ngO8dYzDTSPMd_pWdGWkpiwgrCxyY8TNYxjn_b-odGxdjP9Id-QUJ9bHEAA0W1C7I3LkrWGsGajw18lUJroE4_QiwQm6Csh7l4hUma7mgyBtOumMfEdQL5sxvvv5e1E5skveuEEyCzei7tu9Yl1oFh-kqj4OAIA9fw8yc7F19a102HoGA3on_mYyEglTmoRAL7GyN8RAEGm7dzLNrwI3Q4acHFcoyA3D_pbF0EyGFN2YDTZYs7fNGP8HFXj_c_5zAmIe-99UgqlOoVAoQvQ4w8QL",
        "875b7f95a6:0:0a81b6da6cc1d7924a18c3b719b1621bd9e09e0638ab86a99e530f67267c7a0fb3bcdebf09c242ff1b29f19b1442e907b06a2a81028add0090f8061c8b841636::2:AkAXRtMOLsm2LoxZ314SC9fgCOpkLwor6Mr9newu1wFJbnBNA2RVH639hRo26VDDmwJLrEH2Dxo4ssRGM4McOcRCzWi2VameTu_PqUTfZ8_B60xOfNqlES6ZS12ujho28l8kP7TFbfwl9CbB1y8Lrs5x-LbEZG0sx0PgiadcNJzjlecwcLCq7hZ58mnrKNX3clMnb59X1AVMHYsY4XlflYzkICxB_QnpNmPTyeruIg-Hl4gmnuOyYeSnEOkPTYYByxlLmc4Eyz3AqhzA634WaAfqkF1DiR4XJ3B5W1-D3Z5znIowVgNoBlIJ2uVDbjm02eRh-Glxvgt0OimuptT_GgcN",
        "e4f8e758c6:0:cf3504c7392b1e15dc11a73ca76f1144578459ee32f3f12dc35bc28be93ada2cc3f8dd2ee066a9b9358a4e7653222b399b4ff2f53ecf7d264d528f8bfb3434c8::2:Aw170fJF8sqymG1z5w4K1ZSU2M2epkkXtbnnzVd9zcLEcum7CQ4_1QTZH5ze1OXzBAIOxZSy4eQBgBaNKPM2x2fGIc5eIcHgnYkXvR41hUVAhdAe4zAi52GQvyCyZ2H5qSZYZrgcHQec4ctpqZEodbfJaznO4VjOTxh6dh41SQLqjWIQmruFDSZ1KG_yQD4mRnPsMzFT4pNmIuQq-mw52_64A7RQ0wEIXRpTkfGav1if1qnfU-TQVj6I8607XugdGE3CoILRBdNMld8J3W7Cq6xHtyw6DeCPCwGN7xt4giyMsIKgbOd0x8HNHX0QRYmvmUZpPQT_wp2LA4NNieNZ9VsO",
        "a6b56f96ad:0:d1d5b9cc259804516edb11903784ee3c3e69fe1b4334129a2db3859406298a3379a2fe894ce24f565fd7e2c065cdf295a7488a5197e62a9aae7d48c311d03ef7::2:AnwaTJ3eJdMKY17HlwVNKpMb_H_kezrgKYCdz_h62-eFW8DGPuqND_QRt4XDSmPO9ANZHkINJ35q4g-MfKITocXzBE3uFTgVgCJhqrsKdf2CspjQT2ZGT5xZHbKGUU9eUShpSdQEVEfuAURTOEyWzifjp9ZEqXbigWeNXaAwiUBihRvh0vMPDumvdrovxX2rAm5N1f2nKKYUYhx5YsSyBhoIjjsKei00iPoBj5gNeINylnxf6PqVyUp9HVApsupHm5xblPWhw0lwa56q5R8rKVwygtHb74qNNmFPKjS6VziNI0XxcKN9lapfj1dmmj6cXCkvdD8YpznaERclOZotdbcA"
    ];

    #[test]
    fn marshal_unmarshal_sign_verify_agree() {
        let gen = Identity::generate();
        assert!(gen.agree(&gen).is_some());
        assert!(gen.validate_identity());
        let bytes = gen.to_bytes(IDENTITY_ALGORITHM_ALL, true);
        let string = gen.to_string_with_options(IDENTITY_ALGORITHM_ALL, true);
        assert!(Identity::from_str(string.as_str()).unwrap().eq(&gen));
        let mut cursor = 0_usize;
        assert!(Identity::unmarshal(&bytes, &mut cursor).unwrap().eq(&gen));
        cursor = 0;
        assert!(Identity::unmarshal(&bytes, &mut cursor).unwrap().secret.is_some());
        assert!(Identity::from_str(string.as_str()).unwrap().secret.is_some());

        let gen2 = Identity::generate();
        assert!(gen2.validate_identity());
        assert!(gen2.agree(&gen).unwrap().eq(&gen.agree(&gen2).unwrap()));

        for id_str in GOOD_V0_IDENTITIES {
            let mut id = Identity::from_str(id_str).unwrap();

            assert!(id.validate_identity());
            assert!(id.p384.is_none());

            let idb = id.to_bytes(IDENTITY_ALGORITHM_ALL, true);
            let mut cursor = 0;
            let id_unmarshal = Identity::unmarshal(&idb, &mut cursor).unwrap();
            assert!(id == id_unmarshal);
            assert!(id_unmarshal.secret.is_some());

            let idb2 = id_unmarshal.to_bytes(IDENTITY_ALGORITHM_ALL, false);
            cursor = 0;
            let id_unmarshal2 = Identity::unmarshal(&idb2, &mut cursor).unwrap();
            assert!(id_unmarshal2 == id_unmarshal);
            assert!(id_unmarshal2 == id);
            assert!(id_unmarshal2.secret.is_none());

            let ids = id.to_string();
            assert!(Identity::from_str(ids.as_str()).unwrap() == id);

            assert!(id.upgrade().is_ok());
            assert!(id.validate_identity());
            assert!(id.p384.is_some());
            assert!(id.secret.as_ref().unwrap().p384.is_some());

            let ids = id.to_string();
            assert!(Identity::from_str(ids.as_str()).unwrap() == id);
        }
        for id_str in GOOD_V1_IDENTITIES {
            let id = Identity::from_str(id_str).unwrap();

            assert!(id.validate_identity());
            assert!(id.p384.is_some());

            let idb = id.to_bytes(IDENTITY_ALGORITHM_ALL, true);
            let mut cursor = 0;
            let id_unmarshal = Identity::unmarshal(&idb, &mut cursor).unwrap();
            assert!(id == id_unmarshal);

            cursor = 0;
            let idb2 = id_unmarshal.to_bytes(IDENTITY_ALGORITHM_ALL, false);
            let id_unmarshal2 = Identity::unmarshal(&idb2, &mut cursor).unwrap();
            assert!(id_unmarshal2 == id_unmarshal);
            assert!(id_unmarshal2 == id);

            let ids = id.to_string();
            assert!(Identity::from_str(ids.as_str()).unwrap() == id);
        }
    }

    #[test]
    fn benchmark_generate() {
        let mut count = 0;
        let run_time = Duration::from_secs(5);
        let start = SystemTime::now();
        let mut end;
        let mut duration;
        loop {
            let _id = Identity::generate();
            //println!("{}", _id.to_string());
            end = SystemTime::now();
            duration = end.duration_since(start).unwrap();
            count += 1;
            if duration >= run_time {
                break;
            }
        }
        println!("benchmark: V1 identity generation: {} ms / identity (average)", (duration.as_millis() as f64) / (count as f64));
    }
}
