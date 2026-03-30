use anyhow::Result;
use hmac::{Hmac, Mac};
use rquickjs::{Ctx, Function};
use sha2::Digest;

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap_or(0))
        .collect()
}

/// Register k6/webcrypto module.
pub fn register(ctx: &Ctx<'_>) -> Result<()> {
    let globals = ctx.globals();

    // __wc_digest(algorithm, data_hex) -> hex string
    globals.set(
        "__wc_digest",
        Function::new(
            ctx.clone(),
            |algorithm: String, data_hex: String| -> rquickjs::Result<String> {
                let data = hex_decode(&data_hex);
                let result = match algorithm.to_uppercase().replace("-", "").as_str() {
                    "SHA1" => {
                        let mut h = sha1::Sha1::new();
                        h.update(&data);
                        hex_encode(&h.finalize())
                    }
                    "SHA256" => hex_encode(&sha2::Sha256::digest(&data)),
                    "SHA384" => hex_encode(&sha2::Sha384::digest(&data)),
                    "SHA512" => hex_encode(&sha2::Sha512::digest(&data)),
                    "MD5" => hex_encode(&md5::Md5::digest(&data)),
                    _ => {
                        return Err(rquickjs::Error::new_from_js_message(
                            "string",
                            "string",
                            &format!("unsupported digest algorithm: {algorithm}"),
                        ))
                    }
                };
                Ok(result)
            },
        )?,
    )?;

    // __wc_random_bytes(count) -> hex string
    globals.set(
        "__wc_random_bytes",
        Function::new(ctx.clone(), |count: usize| -> String {
            use rand::Rng;
            let mut rng = rand::rng();
            let bytes: Vec<u8> = (0..count).map(|_| rng.random()).collect();
            hex_encode(&bytes)
        })?,
    )?;

    // __wc_hmac_sign(hash_algo, key_hex, data_hex) -> hex string
    globals.set(
        "__wc_hmac_sign",
        Function::new(
            ctx.clone(),
            |hash_algo: String, key_hex: String, data_hex: String| -> rquickjs::Result<String> {
                let key = hex_decode(&key_hex);
                let data = hex_decode(&data_hex);
                let result = match hash_algo.to_uppercase().replace("-", "").as_str() {
                    "SHA1" => {
                        let mut mac = <Hmac<sha1::Sha1>>::new_from_slice(&key)
                            .expect("HMAC accepts any key length");
                        mac.update(&data);
                        hex_encode(&mac.finalize().into_bytes())
                    }
                    "SHA256" => {
                        let mut mac = <Hmac<sha2::Sha256>>::new_from_slice(&key)
                            .expect("HMAC accepts any key length");
                        mac.update(&data);
                        hex_encode(&mac.finalize().into_bytes())
                    }
                    "SHA384" => {
                        let mut mac = <Hmac<sha2::Sha384>>::new_from_slice(&key)
                            .expect("HMAC accepts any key length");
                        mac.update(&data);
                        hex_encode(&mac.finalize().into_bytes())
                    }
                    "SHA512" => {
                        let mut mac = <Hmac<sha2::Sha512>>::new_from_slice(&key)
                            .expect("HMAC accepts any key length");
                        mac.update(&data);
                        hex_encode(&mac.finalize().into_bytes())
                    }
                    _ => {
                        return Err(rquickjs::Error::new_from_js_message(
                            "string",
                            "string",
                            &format!("unsupported HMAC hash: {hash_algo}"),
                        ))
                    }
                };
                Ok(result)
            },
        )?,
    )?;

    // __wc_hmac_verify(hash_algo, key_hex, sig_hex, data_hex) -> bool
    globals.set(
        "__wc_hmac_verify",
        Function::new(
            ctx.clone(),
            |hash_algo: String,
             key_hex: String,
             sig_hex: String,
             data_hex: String|
             -> rquickjs::Result<bool> {
                let key = hex_decode(&key_hex);
                let sig = hex_decode(&sig_hex);
                let data = hex_decode(&data_hex);
                let result = match hash_algo.to_uppercase().replace("-", "").as_str() {
                    "SHA256" => {
                        let mut mac = <Hmac<sha2::Sha256>>::new_from_slice(&key)
                            .expect("HMAC accepts any key length");
                        mac.update(&data);
                        mac.verify_slice(&sig).is_ok()
                    }
                    "SHA1" => {
                        let mut mac = <Hmac<sha1::Sha1>>::new_from_slice(&key)
                            .expect("HMAC accepts any key length");
                        mac.update(&data);
                        mac.verify_slice(&sig).is_ok()
                    }
                    "SHA384" => {
                        let mut mac = <Hmac<sha2::Sha384>>::new_from_slice(&key)
                            .expect("HMAC accepts any key length");
                        mac.update(&data);
                        mac.verify_slice(&sig).is_ok()
                    }
                    "SHA512" => {
                        let mut mac = <Hmac<sha2::Sha512>>::new_from_slice(&key)
                            .expect("HMAC accepts any key length");
                        mac.update(&data);
                        mac.verify_slice(&sig).is_ok()
                    }
                    _ => {
                        return Err(rquickjs::Error::new_from_js_message(
                            "string",
                            "string",
                            &format!("unsupported HMAC hash: {hash_algo}"),
                        ))
                    }
                };
                Ok(result)
            },
        )?,
    )?;

    // __wc_pbkdf2(password_hex, salt_hex, iterations, hash_algo, bits) -> hex string
    globals.set(
        "__wc_pbkdf2",
        Function::new(
            ctx.clone(),
            |password_hex: String,
             salt_hex: String,
             iterations: u32,
             hash_algo: String,
             bits: u32|
             -> rquickjs::Result<String> {
                let password = hex_decode(&password_hex);
                let salt = hex_decode(&salt_hex);
                let key_len = (bits / 8) as usize;
                let mut dk = vec![0u8; key_len];

                match hash_algo.to_uppercase().replace("-", "").as_str() {
                    "SHA256" => {
                        pbkdf2::pbkdf2_hmac::<sha2::Sha256>(&password, &salt, iterations, &mut dk);
                    }
                    "SHA1" => {
                        pbkdf2::pbkdf2_hmac::<sha1::Sha1>(&password, &salt, iterations, &mut dk);
                    }
                    "SHA384" => {
                        pbkdf2::pbkdf2_hmac::<sha2::Sha384>(&password, &salt, iterations, &mut dk);
                    }
                    "SHA512" => {
                        pbkdf2::pbkdf2_hmac::<sha2::Sha512>(&password, &salt, iterations, &mut dk);
                    }
                    _ => {
                        return Err(rquickjs::Error::new_from_js_message(
                            "string",
                            "string",
                            &format!("unsupported PBKDF2 hash: {hash_algo}"),
                        ))
                    }
                }
                Ok(hex_encode(&dk))
            },
        )?,
    )?;

    // __wc_aes_gcm_encrypt(key_hex, iv_hex, data_hex) -> hex string (ciphertext+tag)
    globals.set(
        "__wc_aes_gcm_encrypt",
        Function::new(
            ctx.clone(),
            |key_hex: String, iv_hex: String, data_hex: String| -> rquickjs::Result<String> {
                use aes_gcm::{aead::Aead, Aes256Gcm, KeyInit, Nonce};
                let key_bytes = hex_decode(&key_hex);
                let iv_bytes = hex_decode(&iv_hex);
                let data = hex_decode(&data_hex);

                let cipher = Aes256Gcm::new_from_slice(&key_bytes).map_err(|e| {
                    rquickjs::Error::new_from_js_message(
                        "string",
                        "string",
                        &format!("AES-GCM key error: {e}"),
                    )
                })?;
                let nonce = Nonce::from_slice(&iv_bytes);
                let ciphertext = cipher.encrypt(nonce, data.as_ref()).map_err(|e| {
                    rquickjs::Error::new_from_js_message(
                        "string",
                        "string",
                        &format!("AES-GCM encrypt error: {e}"),
                    )
                })?;
                Ok(hex_encode(&ciphertext))
            },
        )?,
    )?;

    // __wc_aes_gcm_decrypt(key_hex, iv_hex, data_hex) -> hex string
    globals.set(
        "__wc_aes_gcm_decrypt",
        Function::new(
            ctx.clone(),
            |key_hex: String, iv_hex: String, data_hex: String| -> rquickjs::Result<String> {
                use aes_gcm::{aead::Aead, Aes256Gcm, KeyInit, Nonce};
                let key_bytes = hex_decode(&key_hex);
                let iv_bytes = hex_decode(&iv_hex);
                let data = hex_decode(&data_hex);

                let cipher = Aes256Gcm::new_from_slice(&key_bytes).map_err(|e| {
                    rquickjs::Error::new_from_js_message(
                        "string",
                        "string",
                        &format!("AES-GCM key error: {e}"),
                    )
                })?;
                let nonce = Nonce::from_slice(&iv_bytes);
                let plaintext = cipher.decrypt(nonce, data.as_ref()).map_err(|e| {
                    rquickjs::Error::new_from_js_message(
                        "string",
                        "string",
                        &format!("AES-GCM decrypt error: {e}"),
                    )
                })?;
                Ok(hex_encode(&plaintext))
            },
        )?,
    )?;

    // __wc_aes_cbc_encrypt(key_hex, iv_hex, data_hex) -> hex string
    globals.set(
        "__wc_aes_cbc_encrypt",
        Function::new(
            ctx.clone(),
            |key_hex: String, iv_hex: String, data_hex: String| -> rquickjs::Result<String> {
                use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
                type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;
                let key_bytes = hex_decode(&key_hex);
                let iv_bytes = hex_decode(&iv_hex);
                let data = hex_decode(&data_hex);

                let encryptor =
                    Aes256CbcEnc::new_from_slices(&key_bytes, &iv_bytes).map_err(|e| {
                        rquickjs::Error::new_from_js_message(
                            "string",
                            "string",
                            &format!("AES-CBC key error: {e}"),
                        )
                    })?;
                let ciphertext = encryptor.encrypt_padded_vec_mut::<Pkcs7>(&data);
                Ok(hex_encode(&ciphertext))
            },
        )?,
    )?;

    // __wc_aes_cbc_decrypt(key_hex, iv_hex, data_hex) -> hex string
    globals.set(
        "__wc_aes_cbc_decrypt",
        Function::new(
            ctx.clone(),
            |key_hex: String, iv_hex: String, data_hex: String| -> rquickjs::Result<String> {
                use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
                type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;
                let key_bytes = hex_decode(&key_hex);
                let iv_bytes = hex_decode(&iv_hex);
                let data = hex_decode(&data_hex);

                let decryptor =
                    Aes256CbcDec::new_from_slices(&key_bytes, &iv_bytes).map_err(|e| {
                        rquickjs::Error::new_from_js_message(
                            "string",
                            "string",
                            &format!("AES-CBC key error: {e}"),
                        )
                    })?;
                let plaintext =
                    decryptor
                        .decrypt_padded_vec_mut::<Pkcs7>(&mut data.clone())
                        .map_err(|e| {
                            rquickjs::Error::new_from_js_message(
                                "string",
                                "string",
                                &format!("AES-CBC decrypt error: {e}"),
                            )
                        })?;
                Ok(hex_encode(&plaintext))
            },
        )?,
    )?;

    // __wc_aes_ctr_encrypt(key_hex, counter_hex, data_hex) -> hex string
    globals.set(
        "__wc_aes_ctr_encrypt",
        Function::new(
            ctx.clone(),
            |key_hex: String,
             counter_hex: String,
             data_hex: String|
             -> rquickjs::Result<String> {
                use aes::cipher::{KeyIvInit, StreamCipher};
                type Aes256Ctr = ctr::Ctr128BE<aes::Aes256>;
                let key_bytes = hex_decode(&key_hex);
                let counter_bytes = hex_decode(&counter_hex);
                let mut data = hex_decode(&data_hex);

                let mut cipher =
                    Aes256Ctr::new_from_slices(&key_bytes, &counter_bytes).map_err(|e| {
                        rquickjs::Error::new_from_js_message(
                            "string",
                            "string",
                            &format!("AES-CTR key error: {e}"),
                        )
                    })?;
                cipher.apply_keystream(&mut data);
                Ok(hex_encode(&data))
            },
        )?,
    )?;

    // __wc_aes_ctr_decrypt is the same as encrypt for CTR mode
    globals.set(
        "__wc_aes_ctr_decrypt",
        Function::new(
            ctx.clone(),
            |key_hex: String,
             counter_hex: String,
             data_hex: String|
             -> rquickjs::Result<String> {
                use aes::cipher::{KeyIvInit, StreamCipher};
                type Aes256Ctr = ctr::Ctr128BE<aes::Aes256>;
                let key_bytes = hex_decode(&key_hex);
                let counter_bytes = hex_decode(&counter_hex);
                let mut data = hex_decode(&data_hex);

                let mut cipher =
                    Aes256Ctr::new_from_slices(&key_bytes, &counter_bytes).map_err(|e| {
                        rquickjs::Error::new_from_js_message(
                            "string",
                            "string",
                            &format!("AES-CTR key error: {e}"),
                        )
                    })?;
                cipher.apply_keystream(&mut data);
                Ok(hex_encode(&data))
            },
        )?,
    )?;

    ctx.eval::<(), _>(include_str!("webcrypto_shim.js"))?;

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
    fn digest_sha256() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval("crypto.subtle.digest('SHA-256', 'hello')")
                .unwrap();
            assert_eq!(
                result,
                "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
            );
        });
    }

    #[test]
    fn digest_sha1() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval("crypto.subtle.digest('SHA-1', 'hello')")
                .unwrap();
            assert_eq!(result, "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d");
        });
    }

    #[test]
    fn digest_sha512() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval("crypto.subtle.digest('SHA-512', 'hello')")
                .unwrap();
            assert!(result.len() == 128); // 512 bits = 128 hex chars
        });
    }

    #[test]
    fn get_random_values() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    var arr = new Uint8Array(16);
                    crypto.getRandomValues(arr);
                    arr.length.toString();
                "#,
                )
                .unwrap();
            assert_eq!(result, "16");
        });
    }

    #[test]
    fn get_random_values_nonzero() {
        with_ctx(|ctx| {
            let result: bool = ctx
                .eval(
                    r#"
                    var arr = new Uint8Array(32);
                    crypto.getRandomValues(arr);
                    var allZero = true;
                    for (var i = 0; i < arr.length; i++) {
                        if (arr[i] !== 0) { allZero = false; break; }
                    }
                    !allZero;
                "#,
                )
                .unwrap();
            assert!(result);
        });
    }

    #[test]
    fn generate_key_hmac() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    var key = crypto.subtle.generateKey(
                        { name: 'HMAC', hash: 'SHA-256', length: 256 },
                        true, ['sign', 'verify']
                    );
                    key.type + ':' + key.algorithm.name;
                "#,
                )
                .unwrap();
            assert_eq!(result, "secret:HMAC");
        });
    }

    #[test]
    fn generate_key_aes_gcm() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    var key = crypto.subtle.generateKey(
                        { name: 'AES-GCM', length: 256 },
                        true, ['encrypt', 'decrypt']
                    );
                    key.type + ':' + key.algorithm.name + ':' + key.algorithm.length;
                "#,
                )
                .unwrap();
            assert_eq!(result, "secret:AES-GCM:256");
        });
    }

    #[test]
    fn hmac_sign_verify() {
        with_ctx(|ctx| {
            let result: bool = ctx
                .eval(
                    r#"
                    var key = crypto.subtle.generateKey(
                        { name: 'HMAC', hash: 'SHA-256' },
                        true, ['sign', 'verify']
                    );
                    var sig = crypto.subtle.sign('HMAC', key, 'test data');
                    crypto.subtle.verify('HMAC', key, sig, 'test data');
                "#,
                )
                .unwrap();
            assert!(result);
        });
    }

    #[test]
    fn hmac_verify_wrong_data() {
        with_ctx(|ctx| {
            let result: bool = ctx
                .eval(
                    r#"
                    var key = crypto.subtle.generateKey(
                        { name: 'HMAC', hash: 'SHA-256' },
                        true, ['sign', 'verify']
                    );
                    var sig = crypto.subtle.sign('HMAC', key, 'test data');
                    crypto.subtle.verify('HMAC', key, sig, 'wrong data');
                "#,
                )
                .unwrap();
            assert!(!result);
        });
    }

    #[test]
    fn aes_gcm_encrypt_decrypt() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    var key = crypto.subtle.generateKey(
                        { name: 'AES-GCM', length: 256 },
                        true, ['encrypt', 'decrypt']
                    );
                    var iv = new Uint8Array(12);
                    crypto.getRandomValues(iv);
                    var encrypted = crypto.subtle.encrypt(
                        { name: 'AES-GCM', iv: iv }, key, 'secret message'
                    );
                    crypto.subtle.decrypt(
                        { name: 'AES-GCM', iv: iv }, key, encrypted
                    );
                "#,
                )
                .unwrap();
            assert_eq!(result, "secret message");
        });
    }

    #[test]
    fn aes_cbc_encrypt_decrypt() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    var key = crypto.subtle.generateKey(
                        { name: 'AES-CBC', length: 256 },
                        true, ['encrypt', 'decrypt']
                    );
                    var iv = new Uint8Array(16);
                    crypto.getRandomValues(iv);
                    var encrypted = crypto.subtle.encrypt(
                        { name: 'AES-CBC', iv: iv }, key, 'hello world'
                    );
                    crypto.subtle.decrypt(
                        { name: 'AES-CBC', iv: iv }, key, encrypted
                    );
                "#,
                )
                .unwrap();
            assert_eq!(result, "hello world");
        });
    }

    #[test]
    fn aes_ctr_encrypt_decrypt() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    var key = crypto.subtle.generateKey(
                        { name: 'AES-CTR', length: 256 },
                        true, ['encrypt', 'decrypt']
                    );
                    var counter = new Uint8Array(16);
                    crypto.getRandomValues(counter);
                    var encrypted = crypto.subtle.encrypt(
                        { name: 'AES-CTR', counter: counter, length: 64 }, key, 'test data'
                    );
                    crypto.subtle.decrypt(
                        { name: 'AES-CTR', counter: counter, length: 64 }, key, encrypted
                    );
                "#,
                )
                .unwrap();
            assert_eq!(result, "test data");
        });
    }

    #[test]
    fn import_export_key() {
        with_ctx(|ctx| {
            let result: bool = ctx
                .eval(
                    r#"
                    var key = crypto.subtle.generateKey(
                        { name: 'HMAC', hash: 'SHA-256' },
                        true, ['sign', 'verify']
                    );
                    var raw = crypto.subtle.exportKey('raw', key);
                    var imported = crypto.subtle.importKey(
                        'raw', raw,
                        { name: 'HMAC', hash: 'SHA-256' },
                        true, ['sign', 'verify']
                    );
                    // Sign with original, verify with imported
                    var sig = crypto.subtle.sign('HMAC', key, 'data');
                    crypto.subtle.verify('HMAC', imported, sig, 'data');
                "#,
                )
                .unwrap();
            assert!(result);
        });
    }

    #[test]
    fn pbkdf2_derive_bits() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    var keyMaterial = crypto.subtle.importKey(
                        'raw', 'password',
                        { name: 'PBKDF2' },
                        false, ['deriveBits']
                    );
                    var bits = crypto.subtle.deriveBits(
                        { name: 'PBKDF2', salt: 'salt', iterations: 1000, hash: 'SHA-256' },
                        keyMaterial, 256
                    );
                    typeof bits;
                "#,
                )
                .unwrap();
            assert_eq!(result, "string"); // hex string
        });
    }

    #[test]
    fn pbkdf2_derive_bits_deterministic() {
        with_ctx(|ctx| {
            let result: bool = ctx
                .eval(
                    r#"
                    var km = crypto.subtle.importKey('raw', 'pass', { name: 'PBKDF2' }, false, ['deriveBits']);
                    var a = crypto.subtle.deriveBits({ name: 'PBKDF2', salt: 'salt', iterations: 100, hash: 'SHA-256' }, km, 128);
                    var b = crypto.subtle.deriveBits({ name: 'PBKDF2', salt: 'salt', iterations: 100, hash: 'SHA-256' }, km, 128);
                    a === b;
                "#,
                )
                .unwrap();
            assert!(result);
        });
    }

    #[test]
    fn pbkdf2_derive_key() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    var km = crypto.subtle.importKey('raw', 'password', { name: 'PBKDF2' }, false, ['deriveKey']);
                    var derived = crypto.subtle.deriveKey(
                        { name: 'PBKDF2', salt: 'salt', iterations: 1000, hash: 'SHA-256' },
                        km,
                        { name: 'AES-GCM', length: 256 },
                        true, ['encrypt', 'decrypt']
                    );
                    derived.type + ':' + derived.algorithm.name;
                "#,
                )
                .unwrap();
            assert_eq!(result, "secret:AES-GCM");
        });
    }
}
