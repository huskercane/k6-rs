use anyhow::Result;
use rquickjs::Ctx;

/// Register k6/experimental/streams module.
///
/// Provides: ReadableStream, WritableStream, TransformStream (Web Streams API)
pub fn register(ctx: &Ctx<'_>) -> Result<()> {
    ctx.eval::<(), _>(include_str!("streams_shim.js"))?;
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
    fn readable_stream_basic() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    var rs = new ReadableStream({
                        start: function(controller) {
                            controller.enqueue('a');
                            controller.enqueue('b');
                            controller.enqueue('c');
                            controller.close();
                        }
                    });
                    var reader = rs.getReader();
                    var chunks = [];
                    var r;
                    while (true) {
                        r = reader.read();
                        if (r.done) break;
                        chunks.push(r.value);
                    }
                    chunks.join(',');
                "#,
                )
                .unwrap();
            assert_eq!(result, "a,b,c");
        });
    }

    #[test]
    fn readable_stream_locked() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    var rs = new ReadableStream({ start: function(c) { c.close(); } });
                    var before = rs.locked;
                    rs.getReader();
                    var after = rs.locked;
                    before + ':' + after;
                "#,
                )
                .unwrap();
            assert_eq!(result, "false:true");
        });
    }

    #[test]
    fn writable_stream_basic() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    var received = [];
                    var ws = new WritableStream({
                        write: function(chunk) { received.push(chunk); },
                        close: function() { received.push('END'); }
                    });
                    var writer = ws.getWriter();
                    writer.write('x');
                    writer.write('y');
                    writer.close();
                    received.join(',');
                "#,
                )
                .unwrap();
            assert_eq!(result, "x,y,END");
        });
    }

    #[test]
    fn writable_stream_locked() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    var ws = new WritableStream({});
                    var before = ws.locked;
                    ws.getWriter();
                    var after = ws.locked;
                    before + ':' + after;
                "#,
                )
                .unwrap();
            assert_eq!(result, "false:true");
        });
    }

    #[test]
    fn transform_stream_basic() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    var ts = new TransformStream({
                        transform: function(chunk, controller) {
                            controller.enqueue(chunk.toUpperCase());
                        }
                    });
                    var writer = ts.writable.getWriter();
                    writer.write('hello');
                    writer.write('world');
                    writer.close();

                    var reader = ts.readable.getReader();
                    var chunks = [];
                    var r;
                    while (true) {
                        r = reader.read();
                        if (r.done) break;
                        chunks.push(r.value);
                    }
                    chunks.join(',');
                "#,
                )
                .unwrap();
            assert_eq!(result, "HELLO,WORLD");
        });
    }

    #[test]
    fn transform_stream_identity() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    var ts = new TransformStream();
                    var writer = ts.writable.getWriter();
                    writer.write('pass');
                    writer.write('through');
                    writer.close();

                    var reader = ts.readable.getReader();
                    var chunks = [];
                    var r;
                    while (true) {
                        r = reader.read();
                        if (r.done) break;
                        chunks.push(r.value);
                    }
                    chunks.join(',');
                "#,
                )
                .unwrap();
            assert_eq!(result, "pass,through");
        });
    }

    #[test]
    fn pipe_through() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    var rs = new ReadableStream({
                        start: function(c) {
                            c.enqueue('a');
                            c.enqueue('b');
                            c.close();
                        }
                    });
                    var ts = new TransformStream({
                        transform: function(chunk, controller) {
                            controller.enqueue(chunk + '!');
                        }
                    });
                    var out = rs.pipeThrough(ts);
                    var reader = out.getReader();
                    var chunks = [];
                    var r;
                    while (true) {
                        r = reader.read();
                        if (r.done) break;
                        chunks.push(r.value);
                    }
                    chunks.join(',');
                "#,
                )
                .unwrap();
            assert_eq!(result, "a!,b!");
        });
    }

    #[test]
    fn pipe_to() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    var received = [];
                    var rs = new ReadableStream({
                        start: function(c) {
                            c.enqueue('1');
                            c.enqueue('2');
                            c.close();
                        }
                    });
                    var ws = new WritableStream({
                        write: function(chunk) { received.push(chunk); }
                    });
                    rs.pipeTo(ws);
                    received.join(',');
                "#,
                )
                .unwrap();
            assert_eq!(result, "1,2");
        });
    }

    #[test]
    fn readable_stream_cancel() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    var cancelled = false;
                    var rs = new ReadableStream({
                        start: function(c) { c.enqueue('a'); },
                        cancel: function() { cancelled = true; }
                    });
                    var reader = rs.getReader();
                    reader.cancel();
                    String(cancelled);
                "#,
                )
                .unwrap();
            assert_eq!(result, "true");
        });
    }

    #[test]
    fn transform_stream_flush() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    var ts = new TransformStream({
                        transform: function(chunk, controller) {
                            controller.enqueue(chunk);
                        },
                        flush: function(controller) {
                            controller.enqueue('flushed');
                        }
                    });
                    var writer = ts.writable.getWriter();
                    writer.write('data');
                    writer.close();

                    var reader = ts.readable.getReader();
                    var chunks = [];
                    var r;
                    while (true) {
                        r = reader.read();
                        if (r.done) break;
                        chunks.push(r.value);
                    }
                    chunks.join(',');
                "#,
                )
                .unwrap();
            assert_eq!(result, "data,flushed");
        });
    }
}
