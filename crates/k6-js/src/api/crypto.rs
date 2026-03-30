use anyhow::Result;
use digest::Digest;
use hmac::{Hmac, Mac};
use rand::Rng;
use rquickjs::{Ctx, Function};

fn hex_hash<D: Digest>(input: &[u8]) -> String {
    let result = D::digest(input);
    hex_encode(&result)
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

macro_rules! hmac_hex {
    ($t:ty, $key:expr, $input:expr) => {{
        let mut mac =
            <Hmac<$t>>::new_from_slice($key).expect("HMAC accepts any key length");
        mac.update($input);
        hex_encode(&mac.finalize().into_bytes())
    }};
}

/// Register k6/crypto functions.
///
/// Provides: md4, md5, sha1, sha256, sha384, sha512, ripemd160,
///           hmac(algorithm, key, data, outputEncoding),
///           randomBytes(size)
pub fn register(ctx: &Ctx<'_>) -> Result<()> {
    let globals = ctx.globals();

    // Hash functions: __crypto_hash(algorithm, input) -> hex string
    globals.set(
        "__crypto_hash",
        Function::new(ctx.clone(), |algorithm: String, input: String| -> String {
            let data = input.as_bytes();
            match algorithm.as_str() {
                "md4" => hex_hash::<md4::Md4>(data),
                "md5" => hex_hash::<md5::Md5>(data),
                "sha1" => hex_hash::<sha1::Sha1>(data),
                "sha256" => hex_hash::<sha2::Sha256>(data),
                "sha384" => hex_hash::<sha2::Sha384>(data),
                "sha512" => hex_hash::<sha2::Sha512>(data),
                "sha512_224" => hex_hash::<sha2::Sha512_224>(data),
                "sha512_256" => hex_hash::<sha2::Sha512_256>(data),
                "ripemd160" => hex_hash::<ripemd::Ripemd160>(data),
                _ => format!("unsupported algorithm: {algorithm}"),
            }
        })?,
    )?;

    // HMAC: __crypto_hmac(algorithm, key, data) -> hex string
    globals.set(
        "__crypto_hmac",
        Function::new(
            ctx.clone(),
            |algorithm: String, key: String, input: String| -> String {
                let key_bytes = key.as_bytes();
                let data = input.as_bytes();
                match algorithm.as_str() {
                    "md5" => hmac_hex!(md5::Md5, key_bytes, data),
                    "sha1" => hmac_hex!(sha1::Sha1, key_bytes, data),
                    "sha256" => hmac_hex!(sha2::Sha256, key_bytes, data),
                    "sha384" => hmac_hex!(sha2::Sha384, key_bytes, data),
                    "sha512" => hmac_hex!(sha2::Sha512, key_bytes, data),
                    "ripemd160" => hmac_hex!(ripemd::Ripemd160, key_bytes, data),
                    _ => format!("unsupported algorithm: {algorithm}"),
                }
            },
        )?,
    )?;

    // randomBytes: __crypto_random_bytes(size) -> hex string
    globals.set(
        "__crypto_random_bytes",
        Function::new(ctx.clone(), |size: usize| -> String {
            let mut bytes = vec![0u8; size];
            rand::rng().fill(&mut bytes[..]);
            hex_encode(&bytes)
        })?,
    )?;

    // JS API wrappers matching k6/crypto
    ctx.eval::<(), _>(
        r#"
        var crypto = {
            md4: function(input, outputEncoding) {
                return __crypto_hash("md4", String(input));
            },
            md5: function(input, outputEncoding) {
                return __crypto_hash("md5", String(input));
            },
            sha1: function(input, outputEncoding) {
                return __crypto_hash("sha1", String(input));
            },
            sha256: function(input, outputEncoding) {
                return __crypto_hash("sha256", String(input));
            },
            sha384: function(input, outputEncoding) {
                return __crypto_hash("sha384", String(input));
            },
            sha512: function(input, outputEncoding) {
                return __crypto_hash("sha512", String(input));
            },
            sha512_224: function(input, outputEncoding) {
                return __crypto_hash("sha512_224", String(input));
            },
            sha512_256: function(input, outputEncoding) {
                return __crypto_hash("sha512_256", String(input));
            },
            ripemd160: function(input, outputEncoding) {
                return __crypto_hash("ripemd160", String(input));
            },
            hmac: function(algorithm, key, data, outputEncoding) {
                return __crypto_hmac(algorithm, String(key), String(data));
            },
            randomBytes: function(size) {
                var hex = __crypto_random_bytes(size);
                // Convert hex string to ArrayBuffer-like array
                var bytes = [];
                for (var i = 0; i < hex.length; i += 2) {
                    bytes.push(parseInt(hex.substr(i, 2), 16));
                }
                return bytes;
            },
            createHash: function(algorithm) {
                var _data = "";
                return {
                    update: function(input) {
                        _data += String(input);
                        return this;
                    },
                    digest: function(outputEncoding) {
                        return __crypto_hash(algorithm, _data);
                    }
                };
            },
            createHMAC: function(algorithm, key) {
                var _data = "";
                var _key = String(key);
                return {
                    update: function(input) {
                        _data += String(input);
                        return this;
                    },
                    digest: function(outputEncoding) {
                        return __crypto_hmac(algorithm, _key, _data);
                    }
                };
            }
        };
        globalThis.crypto = crypto;
    "#,
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime;

    fn with_ctx(f: impl FnOnce(&Ctx<'_>)) {
        let rt = runtime::create_runtime().unwrap();
        let ctx = runtime::create_context(&rt).unwrap();
        ctx.with(|ctx| {
            register(&ctx).unwrap();
            f(&ctx);
        });
    }

    #[test]
    fn md5_hash() {
        with_ctx(|ctx| {
            let result: String = ctx.eval("crypto.md5('hello')").unwrap();
            assert_eq!(result, "5d41402abc4b2a76b9719d911017c592");
        });
    }

    #[test]
    fn sha1_hash() {
        with_ctx(|ctx| {
            let result: String = ctx.eval("crypto.sha1('hello')").unwrap();
            assert_eq!(result, "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d");
        });
    }

    #[test]
    fn sha256_hash() {
        with_ctx(|ctx| {
            let result: String = ctx.eval("crypto.sha256('hello')").unwrap();
            assert_eq!(
                result,
                "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
            );
        });
    }

    #[test]
    fn sha384_hash() {
        with_ctx(|ctx| {
            let result: String = ctx.eval("crypto.sha384('hello')").unwrap();
            assert_eq!(
                result,
                "59e1748777448c69de6b800d7a33bbfb9ff1b463e44354c3553bcdb9c666fa90125a3c79f90397bdf5f6a13de828684f"
            );
        });
    }

    #[test]
    fn sha512_hash() {
        with_ctx(|ctx| {
            let result: String = ctx.eval("crypto.sha512('hello')").unwrap();
            assert_eq!(
                result,
                "9b71d224bd62f3785d96d46ad3ea3d73319bfbc2890caadae2dff72519673ca72323c3d99ba5c11d7c7acc6e14b8c5da0c4663475c2e5c3adef46f73bcdec043"
            );
        });
    }

    #[test]
    fn ripemd160_hash() {
        with_ctx(|ctx| {
            let result: String = ctx.eval("crypto.ripemd160('hello')").unwrap();
            assert_eq!(result, "108f07b8382412612c048d07d13f814118445acd");
        });
    }

    #[test]
    fn md4_hash() {
        with_ctx(|ctx| {
            let result: String = ctx.eval("crypto.md4('hello')").unwrap();
            assert_eq!(result, "866437cb7a794bce2b727acc0362ee27");
        });
    }

    #[test]
    fn hmac_sha256() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval("crypto.hmac('sha256', 'secret', 'hello')")
                .unwrap();
            assert_eq!(
                result,
                "88aab3ede8d3adf94d26ab90d3bafd4a2083070c3bcce9c014ee04a443847c0b"
            );
        });
    }

    #[test]
    fn hmac_md5() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval("crypto.hmac('md5', 'key', 'message')")
                .unwrap();
            // HMAC-MD5("key", "message")
            assert_eq!(result, "4e4748e62b463521f6775fbf921234b5");
        });
    }

    #[test]
    fn random_bytes_length() {
        with_ctx(|ctx| {
            let len: i32 = ctx.eval("crypto.randomBytes(16).length").unwrap();
            assert_eq!(len, 16);
        });
    }

    #[test]
    fn random_bytes_values_are_valid() {
        with_ctx(|ctx| {
            let valid: bool = ctx
                .eval("crypto.randomBytes(32).every(function(b) { return b >= 0 && b <= 255; })")
                .unwrap();
            assert!(valid);
        });
    }

    #[test]
    fn create_hash_streaming() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval("crypto.createHash('sha256').update('hel').update('lo').digest('hex')")
                .unwrap();
            assert_eq!(
                result,
                "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
            );
        });
    }

    #[test]
    fn create_hmac_streaming() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    "crypto.createHMAC('sha256', 'secret').update('hel').update('lo').digest('hex')",
                )
                .unwrap();
            assert_eq!(
                result,
                "88aab3ede8d3adf94d26ab90d3bafd4a2083070c3bcce9c014ee04a443847c0b"
            );
        });
    }
}
