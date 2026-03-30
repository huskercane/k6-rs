use anyhow::Result;
use base64::{Engine as _, engine::general_purpose};
use rquickjs::{Ctx, Function};

/// Register k6/encoding functions: b64encode, b64decode.
///
/// Encoding variants: "std" (default), "rawstd", "url", "rawurl".
pub fn register(ctx: &Ctx<'_>) -> Result<()> {
    let globals = ctx.globals();

    globals.set(
        "__b64encode",
        Function::new(ctx.clone(), |input: String, encoding: Option<String>| -> String {
            let enc = encoding.unwrap_or_else(|| "std".to_string());
            match enc.as_str() {
                "rawstd" => general_purpose::STANDARD_NO_PAD.encode(input.as_bytes()),
                "url" => general_purpose::URL_SAFE.encode(input.as_bytes()),
                "rawurl" => general_purpose::URL_SAFE_NO_PAD.encode(input.as_bytes()),
                _ => general_purpose::STANDARD.encode(input.as_bytes()),
            }
        })?,
    )?;

    globals.set(
        "__b64decode",
        Function::new(
            ctx.clone(),
            |input: String, encoding: Option<String>, format: Option<String>| -> String {
                let enc = encoding.unwrap_or_else(|| "std".to_string());
                let bytes = match enc.as_str() {
                    "rawstd" => general_purpose::STANDARD_NO_PAD.decode(input.as_bytes()),
                    "url" => general_purpose::URL_SAFE.decode(input.as_bytes()),
                    "rawurl" => general_purpose::URL_SAFE_NO_PAD.decode(input.as_bytes()),
                    _ => general_purpose::STANDARD.decode(input.as_bytes()),
                };
                match bytes {
                    Ok(b) => {
                        let fmt = format.unwrap_or_else(|| "s".to_string());
                        if fmt == "b" {
                            // Return as array-like string representation for binary
                            format!("{b:?}")
                        } else {
                            String::from_utf8_lossy(&b).to_string()
                        }
                    }
                    Err(_) => String::new(),
                }
            },
        )?,
    )?;

    // JS wrappers that match k6 API
    ctx.eval::<(), _>(
        r#"
        globalThis.b64encode = function(input, encoding) {
            return __b64encode(String(input), encoding || "std");
        };
        globalThis.b64decode = function(input, encoding, format) {
            return __b64decode(String(input), encoding || "std", format || "s");
        };
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
    fn b64encode_default() {
        with_ctx(|ctx| {
            let result: String = ctx.eval("b64encode('hello world')").unwrap();
            assert_eq!(result, "aGVsbG8gd29ybGQ=");
        });
    }

    #[test]
    fn b64decode_default() {
        with_ctx(|ctx| {
            let result: String = ctx.eval("b64decode('aGVsbG8gd29ybGQ=')").unwrap();
            assert_eq!(result, "hello world");
        });
    }

    #[test]
    fn b64encode_rawstd() {
        with_ctx(|ctx| {
            let result: String = ctx.eval("b64encode('hello world', 'rawstd')").unwrap();
            assert_eq!(result, "aGVsbG8gd29ybGQ"); // no padding
        });
    }

    #[test]
    fn b64encode_url() {
        with_ctx(|ctx| {
            let result: String = ctx.eval("b64encode('subjects?_d', 'url')").unwrap();
            let decoded: String = ctx
                .eval(format!("b64decode('{}', 'url')", result))
                .unwrap();
            assert_eq!(decoded, "subjects?_d");
        });
    }

    #[test]
    fn b64encode_rawurl() {
        with_ctx(|ctx| {
            let result: String = ctx.eval("b64encode('hello', 'rawurl')").unwrap();
            assert_eq!(result, "aGVsbG8"); // no padding, URL-safe
        });
    }

    #[test]
    fn b64_roundtrip() {
        with_ctx(|ctx| {
            let result: String =
                ctx.eval("b64decode(b64encode('test data 123!@#'))").unwrap();
            assert_eq!(result, "test data 123!@#");
        });
    }

    #[test]
    fn b64decode_invalid_returns_empty() {
        with_ctx(|ctx| {
            let result: String = ctx.eval("b64decode('!!!invalid!!!')").unwrap();
            assert_eq!(result, "");
        });
    }
}
