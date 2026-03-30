use anyhow::Result;
use rquickjs::Ctx;

/// Register k6/experimental/csv module.
///
/// Provides: `csv.parse(csvString, options?)` — parses CSV string into array of objects
pub fn register(ctx: &Ctx<'_>) -> Result<()> {
    ctx.eval::<(), _>(include_str!("csv_shim.js"))?;
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
    fn parse_simple_csv() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    var data = csv.parse("name,age\nAlice,30\nBob,25");
                    JSON.stringify(data);
                "#,
                )
                .unwrap();
            assert_eq!(
                result,
                r#"[{"name":"Alice","age":"30"},{"name":"Bob","age":"25"}]"#
            );
        });
    }

    #[test]
    fn parse_csv_with_quotes() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    var data = csv.parse('name,desc\nAlice,"has, comma"\nBob,"says ""hi"""');
                    JSON.stringify(data);
                "#,
                )
                .unwrap();
            assert_eq!(
                result,
                r#"[{"name":"Alice","desc":"has, comma"},{"name":"Bob","desc":"says \"hi\""}]"#
            );
        });
    }

    #[test]
    fn parse_csv_empty_string() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    var data = csv.parse("");
                    JSON.stringify(data);
                "#,
                )
                .unwrap();
            assert_eq!(result, "[]");
        });
    }

    #[test]
    fn parse_csv_header_only() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    var data = csv.parse("name,age\n");
                    JSON.stringify(data);
                "#,
                )
                .unwrap();
            assert_eq!(result, "[]");
        });
    }

    #[test]
    fn parse_csv_with_delimiter() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    var data = csv.parse("name;age\nAlice;30", { delimiter: ';' });
                    JSON.stringify(data);
                "#,
                )
                .unwrap();
            assert_eq!(result, r#"[{"name":"Alice","age":"30"}]"#);
        });
    }

    #[test]
    fn parse_csv_missing_values() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    var data = csv.parse("a,b,c\n1,2\n4,5,6");
                    JSON.stringify(data);
                "#,
                )
                .unwrap();
            assert_eq!(
                result,
                r#"[{"a":"1","b":"2","c":""},{"a":"4","b":"5","c":"6"}]"#
            );
        });
    }

    #[test]
    fn parse_csv_trailing_newlines() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    var data = csv.parse("x,y\n1,2\n\n");
                    JSON.stringify(data);
                "#,
                )
                .unwrap();
            assert_eq!(result, r#"[{"x":"1","y":"2"}]"#);
        });
    }

    #[test]
    fn parse_csv_foreach() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    var names = [];
                    csv.parse("name,age\nAlice,30\nBob,25").forEach(function(row) {
                        names.push(row.name);
                    });
                    names.join(',');
                "#,
                )
                .unwrap();
            assert_eq!(result, "Alice,Bob");
        });
    }
}
