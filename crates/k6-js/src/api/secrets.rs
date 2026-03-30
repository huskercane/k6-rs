use anyhow::Result;
use rquickjs::{Ctx, Function};

/// Register k6/secrets support.
///
/// Provides:
/// - `__secrets_read_file(path)` — reads a secret from a file path
/// - `SecretVault` class (via JS shim) — reads secrets from env vars or files
pub fn register(ctx: &Ctx<'_>) -> Result<()> {
    let globals = ctx.globals();

    // __secrets_read_file: reads a file's contents, trimming trailing newlines
    globals.set(
        "__secrets_read_file",
        Function::new(ctx.clone(), |path: String| -> rquickjs::Result<String> {
            std::fs::read_to_string(&path)
                .map(|s| s.trim_end_matches('\n').trim_end_matches('\r').to_string())
                .map_err(|e| {
                    rquickjs::Error::new_from_js_message(
                        "string",
                        "string",
                        &format!("secrets: cannot read file '{path}': {e}"),
                    )
                })
        })?,
    )?;

    // Register the JS shim
    ctx.eval::<(), _>(include_str!("secrets_shim.js"))?;

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
            // Set up __ENV
            let globals = ctx.globals();
            let env_obj = rquickjs::Object::new(ctx.clone()).unwrap();
            env_obj.set("MY_SECRET", "s3cret_value").unwrap();
            env_obj.set("DB_HOST", "localhost:5432").unwrap();
            globals.set("__ENV", env_obj).unwrap();

            register(&ctx).unwrap();
            f(&ctx);
        });
    }

    #[test]
    fn vault_get_env_secret() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    var v = new SecretVault({ env: { db: 'MY_SECRET' } });
                    v.get('db');
                "#,
                )
                .unwrap();
            assert_eq!(result, "s3cret_value");
        });
    }

    #[test]
    fn vault_get_env_missing() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    var v = new SecretVault({ env: { x: 'NONEXISTENT_VAR' } });
                    try { v.get('x'); } catch(e) { e.message; }
                "#,
                )
                .unwrap();
            assert!(result.contains("not found"));
        });
    }

    #[test]
    fn vault_get_file_secret() {
        // Create a temp file with a secret
        let dir = std::env::temp_dir().join("k6rs_test_secrets");
        std::fs::create_dir_all(&dir).unwrap();
        let secret_file = dir.join("my_secret.txt");
        std::fs::write(&secret_file, "file_secret_value\n").unwrap();

        with_ctx(|ctx| {
            let path_str = secret_file.to_str().unwrap().replace('\\', "/");
            let result: String = ctx
                .eval(format!(
                    r#"
                    var v = new SecretVault({{ file: {{ token: '{path_str}' }} }});
                    v.get('token');
                "#
                ))
                .unwrap();
            assert_eq!(result, "file_secret_value");
        });

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn vault_get_unknown_key() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    var v = new SecretVault({ env: { x: 'MY_SECRET' } });
                    try { v.get('nonexistent'); } catch(e) { e.message; }
                "#,
                )
                .unwrap();
            assert!(result.contains("unknown secret"));
        });
    }

    #[test]
    fn vault_mixed_env_and_file() {
        let dir = std::env::temp_dir().join("k6rs_test_secrets_mixed");
        std::fs::create_dir_all(&dir).unwrap();
        let secret_file = dir.join("api_key.txt");
        std::fs::write(&secret_file, "key123").unwrap();

        with_ctx(|ctx| {
            let path_str = secret_file.to_str().unwrap().replace('\\', "/");
            let result: String = ctx
                .eval(format!(
                    r#"
                    var v = new SecretVault({{
                        env: {{ db: 'MY_SECRET' }},
                        file: {{ api: '{path_str}' }}
                    }});
                    v.get('db') + ':' + v.get('api');
                "#
                ))
                .unwrap();
            assert_eq!(result, "s3cret_value:key123");
        });

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn vault_empty_config() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    var v = new SecretVault({});
                    try { v.get('x'); } catch(e) { e.message; }
                "#,
                )
                .unwrap();
            assert!(result.contains("unknown secret"));
        });
    }
}
