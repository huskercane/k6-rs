//! Shared console bindings for lifecycle contexts and coroutine VUs.

use anyhow::Result;
use k6_core::console::{ConsoleLevel, ConsoleSink, MAX_MESSAGE_BYTES};
use rquickjs::{Ctx, Function};

pub fn register(ctx: &Ctx<'_>, sink: ConsoleSink) -> Result<()> {
    ctx.globals().set(
        "__console_emit",
        Function::new(ctx.clone(), move |level: u8, message: String| {
            let level = match level {
                1 => ConsoleLevel::Warn,
                2 => ConsoleLevel::Error,
                _ => ConsoleLevel::Log,
            };
            sink.emit(level, message);
        })?,
    )?;
    ctx.globals()
        .set("__console_message_limit", MAX_MESSAGE_BYTES + 1)?;
    ctx.eval::<(), _>(
        r#"
        globalThis.console = (() => {
            const emit = __console_emit;
            const limit = __console_message_limit;
            function write(level, args) {
                let message = '';
                for (let i = 0; i < args.length; i++) {
                    // Preserve conversion side effects even after truncation.
                    const part = String(args[i]);
                    if (message.length < limit) {
                        if (i > 0) message += ' ';
                        let end = Math.min(part.length, Math.max(0, limit - message.length));
                        // Do not introduce a lone UTF-16 surrogate when
                        // bounding a string before its UTF-8 conversion.
                        if (end > 0 && end < part.length &&
                            (part.charCodeAt(end - 1) & 0xfc00) === 0xd800 &&
                            (part.charCodeAt(end) & 0xfc00) === 0xdc00) end++;
                        message += part.slice(0, end);
                    }
                }
                emit(level, message);
            }
            return {
                log: function () { write(0, arguments); },
                warn: function () { write(1, arguments); },
                error: function () { write(2, arguments); },
            };
        })();
    "#,
    )?;
    Ok(())
}
