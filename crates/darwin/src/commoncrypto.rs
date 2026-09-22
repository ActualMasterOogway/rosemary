use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};

use md5::Md5;
use sha1::Sha1;
use sha2::digest::Digest;
use sha2::{Sha224, Sha256, Sha384, Sha512};

enum Context {
    Md5(Md5),
    Sha1(Sha1),
    Sha224(Sha224),
    Sha256(Sha256),
    Sha384(Sha384),
    Sha512(Sha512),
}

impl Context {
    fn update(&mut self, bytes: &[u8]) {
        match self {
            Self::Md5(h) => h.update(bytes),
            Self::Sha1(h) => h.update(bytes),
            Self::Sha224(h) => h.update(bytes),
            Self::Sha256(h) => h.update(bytes),
            Self::Sha384(h) => h.update(bytes),
            Self::Sha512(h) => h.update(bytes),
        }
    }
    fn finish(self) -> Vec<u8> {
        match self {
            Self::Md5(h) => h.finalize().to_vec(),
            Self::Sha1(h) => h.finalize().to_vec(),
            Self::Sha224(h) => h.finalize().to_vec(),
            Self::Sha256(h) => h.finalize().to_vec(),
            Self::Sha384(h) => h.finalize().to_vec(),
            Self::Sha512(h) => h.finalize().to_vec(),
        }
    }
}

fn contexts() -> &'static Mutex<HashMap<usize, Context>> {
    static CONTEXTS: OnceLock<Mutex<HashMap<usize, Context>>> = OnceLock::new();
    CONTEXTS.get_or_init(|| Mutex::new(HashMap::new()))
}

unsafe fn init(context: *mut c_void, hash: Context) -> i32 {
    if context.is_null() {
        return 0;
    }
    contexts()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(context as usize, hash);
    1
}

unsafe fn update(context: *mut c_void, input: *const c_void, len: u32) -> i32 {
    if context.is_null() || (input.is_null() && len != 0) {
        return 0;
    }
    let input = if len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(input.cast(), len as usize) }
    };
    let mut all = contexts().lock().unwrap_or_else(|e| e.into_inner());
    let Some(hash) = all.get_mut(&(context as usize)) else {
        return 0;
    };
    hash.update(input);
    1
}

unsafe fn final_(output: *mut u8, context: *mut c_void) -> i32 {
    if output.is_null() || context.is_null() {
        return 0;
    }
    let Some(hash) = contexts()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(context as usize))
    else {
        return 0;
    };
    let result = hash.finish();
    unsafe { std::ptr::copy_nonoverlapping(result.as_ptr(), output, result.len()) };
    1
}

macro_rules! digest_api {
    ($i:ident,$u:ident,$f:ident,$one:ident,$variant:ident,$ty:ty) => {
        unsafe extern "C" fn $i(context: *mut c_void) -> i32 {
            unsafe { init(context, Context::$variant(<$ty>::new())) }
        }
        unsafe extern "C" fn $u(context: *mut c_void, input: *const c_void, len: u32) -> i32 {
            unsafe { update(context, input, len) }
        }
        unsafe extern "C" fn $f(output: *mut u8, context: *mut c_void) -> i32 {
            unsafe { final_(output, context) }
        }
        unsafe extern "C" fn $one(input: *const c_void, len: u32, output: *mut u8) -> *mut u8 {
            if output.is_null() || (input.is_null() && len != 0) {
                return std::ptr::null_mut();
            }
            let input = if len == 0 {
                &[]
            } else {
                unsafe { std::slice::from_raw_parts(input.cast(), len as usize) }
            };
            let result = <$ty>::digest(input);
            unsafe { std::ptr::copy_nonoverlapping(result.as_ptr(), output, result.len()) };
            output
        }
    };
}
digest_api!(md5_init, md5_update, md5_final, md5, Md5, Md5);
digest_api!(sha1_init, sha1_update, sha1_final, sha1, Sha1, Sha1);
digest_api!(
    sha224_init,
    sha224_update,
    sha224_final,
    sha224,
    Sha224,
    Sha224
);
digest_api!(
    sha256_init,
    sha256_update,
    sha256_final,
    sha256,
    Sha256,
    Sha256
);
digest_api!(
    sha384_init,
    sha384_update,
    sha384_final,
    sha384,
    Sha384,
    Sha384
);
digest_api!(
    sha512_init,
    sha512_update,
    sha512_final,
    sha512,
    Sha512,
    Sha512
);
exports::exports! {
    functions {
        "CC_MD5_Init"=>md5_init,
        "CC_MD5_Update"=>md5_update,
        "CC_MD5_Final"=>md5_final,
        "CC_MD5"=>md5,
        "CC_SHA1_Init"=>sha1_init,
        "CC_SHA1_Update"=>sha1_update,
        "CC_SHA1_Final"=>sha1_final,
        "CC_SHA1"=>sha1,
        "CC_SHA224_Init"=>sha224_init,
        "CC_SHA224_Update"=>sha224_update,
        "CC_SHA224_Final"=>sha224_final,
        "CC_SHA224"=>sha224,
        "CC_SHA256_Init"=>sha256_init,
        "CC_SHA256_Update"=>sha256_update,
        "CC_SHA256_Final"=>sha256_final,
        "CC_SHA256"=>sha256,
        "CC_SHA384_Init"=>sha384_init,
        "CC_SHA384_Update"=>sha384_update,
        "CC_SHA384_Final"=>sha384_final,
        "CC_SHA384"=>sha384,
        "CC_SHA512_Init"=>sha512_init,
        "CC_SHA512_Update"=>sha512_update,
        "CC_SHA512_Final"=>sha512_final,
        "CC_SHA512"=>sha512,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sha256_streaming_matches_known_digest() {
        let mut c = [0u8; 104];
        let mut out = [0u8; 32];
        unsafe {
            assert_eq!(sha256_init(c.as_mut_ptr().cast()), 1);
            assert_eq!(
                sha256_update(c.as_mut_ptr().cast(), c"abc".as_ptr().cast(), 3),
                1
            );
            assert_eq!(sha256_final(out.as_mut_ptr(), c.as_mut_ptr().cast()), 1);
        }
        assert_eq!(
            out,
            [
                0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
                0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
                0xf2, 0x00, 0x15, 0xad
            ]
        );
    }
    #[test]
    fn one_shot_returns_output_argument() {
        let mut out = [0u8; 20];
        assert_eq!(
            unsafe { sha1(c"abc".as_ptr().cast(), 3, out.as_mut_ptr()) },
            out.as_mut_ptr()
        );
    }
}
