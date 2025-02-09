// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use crate::constants::*;
use crate::err::{Error, Res};
use crate::p11::{PK11SymKey, SymKey};
use crate::ssl;
use crate::ssl::{PRUint16, PRUint64, PRUint8, SSLAeadContext};

use std::convert::{TryFrom, TryInto};
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::os::raw::{c_char, c_uint};
use std::ptr::{null_mut, NonNull};

experimental_api!(SSL_MakeAead(
    version: PRUint16,
    cipher: PRUint16,
    secret: *mut PK11SymKey,
    label_prefix: *const c_char,
    label_prefix_len: c_uint,
    ctx: *mut *mut SSLAeadContext,
));
experimental_api!(SSL_AeadEncrypt(
    ctx: *const SSLAeadContext,
    counter: PRUint64,
    aad: *const PRUint8,
    aad_len: c_uint,
    input: *const PRUint8,
    input_len: c_uint,
    output: *const PRUint8,
    output_len: *mut c_uint,
    max_output: c_uint
));
experimental_api!(SSL_AeadDecrypt(
    ctx: *const SSLAeadContext,
    counter: PRUint64,
    aad: *const PRUint8,
    aad_len: c_uint,
    input: *const PRUint8,
    input_len: c_uint,
    output: *const PRUint8,
    output_len: *mut c_uint,
    max_output: c_uint
));
experimental_api!(SSL_DestroyAead(ctx: *mut SSLAeadContext));
scoped_ptr!(AeadContext, SSLAeadContext, SSL_DestroyAead);

pub struct Aead {
    ctx: AeadContext,
}

impl Aead {
    pub fn new<S: Into<String>>(
        version: Version,
        cipher: Cipher,
        secret: &SymKey,
        prefix: S,
    ) -> Res<Self> {
        let s: *mut PK11SymKey = **secret;
        unsafe { Self::from_raw(version, cipher, s, prefix) }
    }

    unsafe fn from_raw<S: Into<String>>(
        version: Version,
        cipher: Cipher,
        secret: *mut PK11SymKey,
        prefix: S,
    ) -> Res<Self> {
        let prefix_str = prefix.into();
        let p = prefix_str.as_bytes();
        let mut ctx: *mut ssl::SSLAeadContext = null_mut();
        SSL_MakeAead(
            version,
            cipher,
            secret,
            p.as_ptr() as *const c_char,
            c_uint::try_from(p.len())?,
            &mut ctx,
        )?;
        match NonNull::new(ctx) {
            Some(ctx_ptr) => Ok(Self {
                ctx: AeadContext::new(ctx_ptr),
            }),
            None => Err(Error::InternalError),
        }
    }

    pub fn encrypt<'a>(
        &self,
        count: u64,
        aad: &[u8],
        input: &[u8],
        output: &'a mut [u8],
    ) -> Res<&'a [u8]> {
        let mut l: c_uint = 0;
        unsafe {
            SSL_AeadEncrypt(
                *self.ctx.deref(),
                count,
                aad.as_ptr(),
                c_uint::try_from(aad.len())?,
                input.as_ptr(),
                c_uint::try_from(input.len())?,
                output.as_mut_ptr(),
                &mut l,
                c_uint::try_from(output.len())?,
            )
        }?;
        Ok(&output[0..(l.try_into().unwrap())])
    }

    pub fn decrypt<'a>(
        &self,
        count: u64,
        aad: &[u8],
        input: &[u8],
        output: &'a mut [u8],
    ) -> Res<&'a [u8]> {
        let mut l: c_uint = 0;
        unsafe {
            SSL_AeadDecrypt(
                *self.ctx.deref(),
                count,
                aad.as_ptr(),
                c_uint::try_from(aad.len())?,
                input.as_ptr(),
                c_uint::try_from(input.len())?,
                output.as_mut_ptr(),
                &mut l,
                c_uint::try_from(output.len())?,
            )
        }?;
        Ok(&output[0..(l.try_into().unwrap())])
    }
}

impl fmt::Debug for Aead {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "[AEAD Context]")
    }
}
