use anyhow::Result;
use rquickjs::{Ctx, Function};

/// Register k6/experimental/fs module.
///
/// Provides:
/// - `__fs_read(path)` — reads file contents as string
/// - `__fs_stat(path)` — returns JSON string with name, size, isDir
/// - JS shim: `fs.open(path)` returning File object, `fs.stat(path)`
pub fn register(ctx: &Ctx<'_>) -> Result<()> {
    let globals = ctx.globals();

    globals.set(
        "__fs_read",
        Function::new(ctx.clone(), |path: String| -> rquickjs::Result<String> {
            std::fs::read_to_string(&path).map_err(|e| {
                rquickjs::Error::new_from_js_message(
                    "string",
                    "string",
                    &format!("fs.open: cannot read '{path}': {e}"),
                )
            })
        })?,
    )?;

    globals.set(
        "__fs_stat",
        Function::new(ctx.clone(), |path: String| -> rquickjs::Result<String> {
            let metadata = std::fs::metadata(&path).map_err(|e| {
                rquickjs::Error::new_from_js_message(
                    "string",
                    "string",
                    &format!("fs.stat: cannot stat '{path}': {e}"),
                )
            })?;
            let name = std::path::Path::new(&path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            Ok(format!(
                r#"{{"name":"{}","size":{},"isDir":{}}}"#,
                name,
                metadata.len(),
                metadata.is_dir()
            ))
        })?,
    )?;

    ctx.eval::<(), _>(include_str!("fs_shim.js"))?;

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
    fn fs_open_and_read() {
        let dir = std::env::temp_dir().join("k6rs_test_fs");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("test.txt");
        std::fs::write(&file, "hello world").unwrap();

        with_ctx(|ctx| {
            let path = file.to_str().unwrap().replace('\\', "/");
            let result: String = ctx
                .eval(format!(
                    r#"
                    var f = fs.open('{path}');
                    f.read();
                "#
                ))
                .unwrap();
            assert_eq!(result, "hello world");
        });

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fs_open_stat() {
        let dir = std::env::temp_dir().join("k6rs_test_fs_stat");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("data.txt");
        std::fs::write(&file, "12345").unwrap();

        with_ctx(|ctx| {
            let path = file.to_str().unwrap().replace('\\', "/");
            let result: String = ctx
                .eval(format!(
                    r#"
                    var f = fs.open('{path}');
                    var s = f.stat();
                    s.name + ':' + s.size + ':' + s.isDir;
                "#
                ))
                .unwrap();
            assert_eq!(result, "data.txt:5:false");
        });

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fs_stat_direct() {
        let dir = std::env::temp_dir().join("k6rs_test_fs_stat_direct");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("info.txt");
        std::fs::write(&file, "abc").unwrap();

        with_ctx(|ctx| {
            let path = file.to_str().unwrap().replace('\\', "/");
            let result: String = ctx
                .eval(format!(
                    r#"
                    var s = fs.stat('{path}');
                    s.name + ':' + s.size;
                "#
                ))
                .unwrap();
            assert_eq!(result, "info.txt:3");
        });

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fs_stat_directory() {
        let dir = std::env::temp_dir().join("k6rs_test_fs_stat_dir");
        std::fs::create_dir_all(&dir).unwrap();

        with_ctx(|ctx| {
            let path = dir.to_str().unwrap().replace('\\', "/");
            let result: String = ctx
                .eval(format!(
                    r#"
                    var s = fs.stat('{path}');
                    String(s.isDir);
                "#
                ))
                .unwrap();
            assert_eq!(result, "true");
        });

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fs_open_nonexistent() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    try { fs.open('/nonexistent/path/file.txt'); 'ok'; } catch(e) { 'error'; }
                "#,
                )
                .unwrap();
            assert_eq!(result, "error");
        });
    }

    #[test]
    fn fs_read_multiple_times() {
        let dir = std::env::temp_dir().join("k6rs_test_fs_multi");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("multi.txt");
        std::fs::write(&file, "content").unwrap();

        with_ctx(|ctx| {
            let path = file.to_str().unwrap().replace('\\', "/");
            let result: String = ctx
                .eval(format!(
                    r#"
                    var f = fs.open('{path}');
                    var a = f.read();
                    var b = f.read();
                    a === b ? 'same' : 'different';
                "#
                ))
                .unwrap();
            assert_eq!(result, "same");
        });

        let _ = std::fs::remove_dir_all(&dir);
    }
}
